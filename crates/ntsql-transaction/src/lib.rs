//! I/O-free transaction lifecycle invariants.

use std::{error::Error, fmt};

use ntsql_wal::{CommitError, CommitLog, LogSequenceNumber, commit_durability};

/// Opaque ntsql-internal transaction identity assigned by a future coordinator.
///
/// This value defines no SQL Server, wire, session, or persistent representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransactionId(u64);

impl TransactionId {
    /// Preserves an identity assigned by the transaction coordinator.
    ///
    /// The future coordinator remains responsible for issuing each logical
    /// identity once; this constructor does not establish uniqueness.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the opaque numeric identity for internal adapter bookkeeping.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Caller-owned WAL record for one internal transaction commit attempt.
///
/// Only [`ActiveTransaction::commit`] constructs this value. Persistence
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

/// Transaction state that may begin one commit attempt.
#[derive(Debug, Eq, PartialEq)]
pub struct ActiveTransaction {
    transaction_id: TransactionId,
}

impl ActiveTransaction {
    /// Starts the internal lifecycle for a coordinator-assigned identity.
    ///
    /// The coordinator must not reconstruct active state for an identity whose
    /// commit was already attempted.
    #[must_use]
    pub const fn new(transaction_id: TransactionId) -> Self {
        Self { transaction_id }
    }

    /// Returns the transaction identity without changing state.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Consumes active state and attempts to establish durable commit state.
    ///
    /// ```compile_fail
    /// use ntsql_transaction::{ActiveTransaction, TransactionCommitRecord};
    /// use ntsql_wal::CommitLog;
    ///
    /// fn cannot_commit_twice<L>(transaction: ActiveTransaction, log: &mut L)
    /// where
    ///     L: CommitLog<TransactionCommitRecord>,
    /// {
    ///     let _first = transaction.commit(log);
    ///     let _second = transaction.commit(log);
    /// }
    /// ```
    pub fn commit<L>(
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
/// use ntsql_transaction::{IndeterminateTransaction, TransactionCommitRecord};
/// use ntsql_wal::CommitLog;
///
/// fn cannot_retry<L>(transaction: IndeterminateTransaction, log: &mut L)
/// where
///     L: CommitLog<TransactionCommitRecord>,
/// {
///     transaction.commit(log);
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
            self.transaction.transaction_id().get(),
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
