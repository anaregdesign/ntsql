//! I/O-free transaction lifecycle invariants.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    error::Error,
    fmt,
    num::NonZeroU64,
    sync::Arc,
};

use ntsql_wal::{CommitError, CommitLog, LogLineage, LogSequenceNumber, commit_durability};

/// Persistence-owned source of nonzero coordinator epochs.
///
/// Implementations are responsible for never reissuing an epoch within one
/// persistence lineage. The transaction domain validates live token ownership
/// independently and does not infer source correctness from safe Rust alone.
pub trait TransactionEpochSource {
    /// Source-specific failure to allocate a fresh epoch.
    type Error;

    /// Allocates one epoch paired with the lineage in which it is unique.
    fn allocate_transaction_epoch(&mut self) -> Result<(NonZeroU64, LogLineage), Self::Error>;
}

/// Opaque persistence-lineage epoch assigned to one coordinator.
///
/// ```compile_fail
/// use ntsql_transaction::TransactionEpoch;
///
/// let forged = TransactionEpoch::new(1);
/// ```
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TransactionEpoch(NonZeroU64);

impl TransactionEpoch {
    /// Returns the source-assigned numeric epoch for adapter bookkeeping.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Opaque ntsql-internal transaction identity assigned by its coordinator.
///
/// This value defines no SQL Server, wire, session, or persistent representation.
///
/// ```compile_fail
/// use ntsql_transaction::TransactionId;
///
/// let reconstructed = TransactionId::new(1, 1);
/// ```
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TransactionId {
    epoch: TransactionEpoch,
    sequence: NonZeroU64,
}

impl TransactionId {
    /// Returns the persistence-lineage coordinator epoch.
    #[must_use]
    pub const fn epoch(self) -> TransactionEpoch {
        self.epoch
    }

    /// Returns the coordinator-local sequence for adapter bookkeeping.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence.get()
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.epoch.get(), self.sequence.get())
    }
}

/// Read-only in-process lifecycle phase recorded by the coordinator.
///
/// This phase is not persistent recovery evidence and deliberately carries no
/// log position or client-visible transaction state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionLifecycleStatus {
    /// The coordinator issued an active transaction token.
    Active,
    /// The token was consumed and WAL work may have begun.
    CommitAttempted,
    /// The exact WAL durability fence completed successfully.
    Committed,
    /// The WAL port failed after the commit attempt began.
    Indeterminate,
}

/// Failure to issue a fresh in-process transaction identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionIssueError {
    /// Every nonzero numeric identity has already been issued.
    IdentitySpaceExhausted,
    /// The lifecycle registry already contains the next identity.
    IdentityAlreadyIssued(TransactionId),
}

impl fmt::Display for TransactionIssueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IdentitySpaceExhausted => {
                formatter.write_str("transaction identity space is exhausted")
            }
            Self::IdentityAlreadyIssued(transaction_id) => write!(
                formatter,
                "transaction identity {transaction_id} was already issued"
            ),
        }
    }
}

impl Error for TransactionIssueError {}

/// Coordinator-owned transaction issuance and commit-attempt registry.
///
/// The private runtime identity binds active tokens to one coordinator without
/// global state. Registry entries are retained for the coordinator lifetime so
/// issued identities cannot be reused by this in-process model.
///
/// ```compile_fail
/// use ntsql_transaction::TransactionCoordinator;
///
/// fn cannot_clone(coordinator: TransactionCoordinator) {
/// let duplicate = coordinator.clone();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::TransactionCoordinator;
///
/// let bypass = TransactionCoordinator::new();
/// ```
#[derive(Debug)]
pub struct TransactionCoordinator {
    epoch: TransactionEpoch,
    identity: Arc<()>,
    log_lineage: LogLineage,
    next_transaction_id: Option<NonZeroU64>,
    lifecycles: BTreeMap<TransactionId, TransactionLifecycleStatus>,
}

impl TransactionCoordinator {
    /// Opens an empty coordinator with one source-assigned lineage epoch.
    pub fn open<Source>(source: &mut Source) -> Result<Self, Source::Error>
    where
        Source: TransactionEpochSource + ?Sized,
    {
        let (epoch, log_lineage) = source.allocate_transaction_epoch()?;
        Ok(Self {
            epoch: TransactionEpoch(epoch),
            identity: Arc::new(()),
            log_lineage,
            next_transaction_id: Some(NonZeroU64::MIN),
            lifecycles: BTreeMap::new(),
        })
    }

    /// Returns this coordinator's persistence-lineage epoch.
    #[must_use]
    pub const fn epoch(&self) -> TransactionEpoch {
        self.epoch
    }

    /// Issues one fresh active transaction token.
    pub fn begin(&mut self) -> Result<ActiveTransaction, TransactionIssueError> {
        let Some(next_transaction_id) = self.next_transaction_id else {
            return Err(TransactionIssueError::IdentitySpaceExhausted);
        };
        let transaction_id = TransactionId {
            epoch: self.epoch,
            sequence: next_transaction_id,
        };
        match self.lifecycles.entry(transaction_id) {
            Entry::Vacant(entry) => {
                entry.insert(TransactionLifecycleStatus::Active);
            }
            Entry::Occupied(_) => {
                return Err(TransactionIssueError::IdentityAlreadyIssued(transaction_id));
            }
        }
        self.next_transaction_id = next_transaction_id
            .get()
            .checked_add(1)
            .and_then(NonZeroU64::new);

        Ok(ActiveTransaction {
            transaction_id,
            coordinator_identity: Arc::clone(&self.identity),
        })
    }

    /// Returns whether this coordinator issued the active token.
    #[must_use]
    pub fn owns(&self, transaction: &ActiveTransaction) -> bool {
        Arc::ptr_eq(&self.identity, &transaction.coordinator_identity)
    }

    /// Returns the in-process lifecycle phase for an issued identity.
    #[must_use]
    pub fn status(&self, transaction_id: TransactionId) -> Option<TransactionLifecycleStatus> {
        self.lifecycles.get(&transaction_id).copied()
    }

    /// Starts one commit attempt for an active token issued by this coordinator.
    ///
    /// Rejections occur before the WAL port is called and retain the active
    /// token. Once WAL work starts, any port failure is terminally
    /// indeterminate for this in-process coordinator.
    pub fn commit<L>(
        &mut self,
        transaction: ActiveTransaction,
        log: &mut L,
    ) -> Result<CommittedTransaction, CoordinatedCommitError<L::Error>>
    where
        L: CommitLog<TransactionCommitRecord>,
    {
        if !self.owns(&transaction) {
            return Err(CoordinatedCommitError::Rejected(
                TransactionCommitRejection {
                    transaction,
                    reason: TransactionCommitRejectionReason::ForeignCoordinator,
                },
            ));
        }
        if !self.log_lineage.same_lineage(log.lineage()) {
            return Err(CoordinatedCommitError::Rejected(
                TransactionCommitRejection {
                    transaction,
                    reason: TransactionCommitRejectionReason::ForeignLogLineage,
                },
            ));
        }

        let transaction_id = transaction.transaction_id();
        let status = match self.lifecycles.get_mut(&transaction_id) {
            Some(status) if *status == TransactionLifecycleStatus::Active => status,
            Some(status) => {
                return Err(CoordinatedCommitError::Rejected(
                    TransactionCommitRejection {
                        transaction,
                        reason: TransactionCommitRejectionReason::LifecycleMismatch {
                            actual: Some(*status),
                        },
                    },
                ));
            }
            None => {
                return Err(CoordinatedCommitError::Rejected(
                    TransactionCommitRejection {
                        transaction,
                        reason: TransactionCommitRejectionReason::LifecycleMismatch {
                            actual: None,
                        },
                    },
                ));
            }
        };

        *status = TransactionLifecycleStatus::CommitAttempted;
        match transaction.commit(log) {
            Ok(committed) => {
                *status = TransactionLifecycleStatus::Committed;
                Ok(committed)
            }
            Err(error) => {
                *status = TransactionLifecycleStatus::Indeterminate;
                Err(CoordinatedCommitError::Indeterminate(error))
            }
        }
    }
}

/// Caller-owned WAL record for one internal transaction commit attempt.
///
/// Only [`TransactionCoordinator::commit`] constructs this value. Persistence
/// adapters may inspect the identity but do not own transaction state.
#[derive(Debug, Eq, PartialEq)]
pub struct TransactionCommitRecord {
    transaction_id: TransactionId,
}

impl TransactionCommitRecord {
    /// Returns the transaction whose commit attempt this record identifies.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }
}

/// Transaction state that may begin one coordinator-owned commit attempt.
///
/// ```compile_fail
/// use ntsql_transaction::ActiveTransaction;
///
/// fn cannot_clone(transaction: ActiveTransaction) {
///     let duplicate = transaction.clone();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{ActiveTransaction, TransactionCommitRecord, TransactionCoordinator};
/// use ntsql_wal::CommitLog;
///
/// fn cannot_commit_twice<L>(
///     coordinator: &mut TransactionCoordinator,
///     transaction: ActiveTransaction,
///     log: &mut L,
/// )
/// where
///     L: CommitLog<TransactionCommitRecord>,
/// {
///     let _first = coordinator.commit(transaction, log);
///     let _second = coordinator.commit(transaction, log);
/// }
/// ```
#[derive(Debug)]
pub struct ActiveTransaction {
    transaction_id: TransactionId,
    coordinator_identity: Arc<()>,
}

impl ActiveTransaction {
    /// Returns the transaction identity without changing state.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    fn commit<L>(
        self,
        log: &mut L,
    ) -> Result<CommittedTransaction, TransactionCommitError<L::Error>>
    where
        L: CommitLog<TransactionCommitRecord>,
    {
        let transaction_id = self.transaction_id;
        let record = TransactionCommitRecord { transaction_id };

        commit_durability(log, &record, |acknowledgement| CommittedTransaction {
            transaction_id,
            log_position: acknowledgement.position(),
        })
        .map_err(|source| TransactionCommitError {
            transaction: IndeterminateTransaction { transaction_id },
            source,
        })
    }
}

/// Reason an active token was rejected before the WAL port was called.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionCommitRejectionReason {
    /// The active token belongs to another coordinator runtime identity.
    ForeignCoordinator,
    /// The commit-log port does not match the epoch source lineage.
    ForeignLogLineage,
    /// The issuing coordinator did not retain the expected active phase.
    LifecycleMismatch {
        /// Recorded phase, or `None` when the registry entry is absent.
        actual: Option<TransactionLifecycleStatus>,
    },
}

/// Pre-WAL commit rejection that retains the still-active token.
#[derive(Debug)]
pub struct TransactionCommitRejection {
    transaction: ActiveTransaction,
    reason: TransactionCommitRejectionReason,
}

impl TransactionCommitRejection {
    /// Returns why the coordinator rejected the token.
    #[must_use]
    pub const fn reason(&self) -> TransactionCommitRejectionReason {
        self.reason
    }

    /// Returns the rejected token's transaction identity.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction.transaction_id()
    }

    /// Returns the active token for routing back to its issuing coordinator.
    #[must_use]
    pub fn into_transaction(self) -> ActiveTransaction {
        self.transaction
    }
}

impl fmt::Display for TransactionCommitRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            TransactionCommitRejectionReason::ForeignCoordinator => write!(
                formatter,
                "transaction {} belongs to another coordinator",
                self.transaction_id()
            ),
            TransactionCommitRejectionReason::ForeignLogLineage => write!(
                formatter,
                "transaction {} belongs to another commit-log lineage",
                self.transaction_id()
            ),
            TransactionCommitRejectionReason::LifecycleMismatch { actual } => write!(
                formatter,
                "transaction {} is not active in its coordinator registry: {actual:?}",
                self.transaction_id()
            ),
        }
    }
}

impl Error for TransactionCommitRejection {}

/// Coordinator commit failure before or after the WAL attempt boundary.
#[derive(Debug)]
pub enum CoordinatedCommitError<E> {
    /// The WAL port was not called and the active token is retained.
    Rejected(TransactionCommitRejection),
    /// WAL work began and the transaction outcome is indeterminate.
    Indeterminate(TransactionCommitError<E>),
}

impl<E: fmt::Display> fmt::Display for CoordinatedCommitError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(rejection) => rejection.fmt(formatter),
            Self::Indeterminate(error) => error.fmt(formatter),
        }
    }
}

impl<E> Error for CoordinatedCommitError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(rejection) => Some(rejection),
            Self::Indeterminate(error) => Some(error),
        }
    }
}

/// Transaction whose commit record is confirmed durable by the WAL port.
#[derive(Debug, Eq, PartialEq)]
pub struct CommittedTransaction {
    transaction_id: TransactionId,
    log_position: LogSequenceNumber,
}

impl CommittedTransaction {
    /// Returns the committed transaction identity.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the exact internal log position confirmed durable.
    #[must_use]
    pub const fn log_position(&self) -> LogSequenceNumber {
        self.log_position
    }
}

/// Transaction whose durable commit outcome cannot be established safely.
///
/// This state deliberately offers no commit or rollback operation.
///
/// ```compile_fail
/// use ntsql_transaction::{
///     IndeterminateTransaction, TransactionCommitRecord, TransactionCoordinator,
/// };
/// use ntsql_wal::CommitLog;
///
/// fn cannot_retry<L>(
///     coordinator: &mut TransactionCoordinator,
///     transaction: IndeterminateTransaction,
///     log: &mut L,
/// )
/// where
///     L: CommitLog<TransactionCommitRecord>,
/// {
///     coordinator.commit(transaction, log);
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct IndeterminateTransaction {
    transaction_id: TransactionId,
}

impl IndeterminateTransaction {
    /// Returns the transaction identity requiring later outcome resolution.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }
}

/// WAL failure paired with the consumed transaction's fail-closed state.
#[derive(Debug, Eq, PartialEq)]
pub struct TransactionCommitError<E> {
    transaction: IndeterminateTransaction,
    source: CommitError<E>,
}

impl<E> TransactionCommitError<E> {
    /// Borrows the transaction that cannot be retried as active.
    #[must_use]
    pub const fn transaction(&self) -> &IndeterminateTransaction {
        &self.transaction
    }

    /// Borrows the original append- or flush-specific WAL failure.
    #[must_use]
    pub const fn cause(&self) -> &CommitError<E> {
        &self.source
    }

    /// Returns the fail-closed transaction state and original WAL failure.
    #[must_use]
    pub fn into_parts(self) -> (IndeterminateTransaction, CommitError<E>) {
        (self.transaction, self.source)
    }
}

impl<E: fmt::Display> fmt::Display for TransactionCommitError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {} commit outcome is indeterminate: {}",
            self.transaction.transaction_id(),
            self.source
        )
    }
}

impl<E> Error for TransactionCommitError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEpochSource {
        lineage: LogLineage,
        next_epoch: Option<NonZeroU64>,
    }

    impl TransactionEpochSource for TestEpochSource {
        type Error = TransactionIssueError;

        fn allocate_transaction_epoch(&mut self) -> Result<(NonZeroU64, LogLineage), Self::Error> {
            let epoch = self
                .next_epoch
                .ok_or(TransactionIssueError::IdentitySpaceExhausted)?;
            self.next_epoch = epoch.get().checked_add(1).and_then(NonZeroU64::new);
            Ok((epoch, self.lineage.clone()))
        }
    }

    #[test]
    fn identity_exhaustion_is_terminal_without_wrapping() -> Result<(), TransactionIssueError> {
        let mut source = TestEpochSource {
            lineage: LogLineage::new(),
            next_epoch: Some(NonZeroU64::MIN),
        };
        let mut coordinator = TransactionCoordinator::open(&mut source)?;
        coordinator.next_transaction_id = Some(NonZeroU64::MAX);

        let last = coordinator.begin()?;
        assert_eq!(last.transaction_id().epoch().get(), 1);
        assert_eq!(last.transaction_id().sequence(), u64::MAX);
        assert_eq!(
            coordinator.begin().err(),
            Some(TransactionIssueError::IdentitySpaceExhausted)
        );
        assert_eq!(
            coordinator.begin().err(),
            Some(TransactionIssueError::IdentitySpaceExhausted)
        );
        Ok(())
    }
}
