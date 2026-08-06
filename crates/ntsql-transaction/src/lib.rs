//! I/O-free transaction lifecycle invariants.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    error::Error,
    fmt,
    marker::PhantomData,
    num::NonZeroU64,
    sync::Arc,
};

use ntsql_page::{
    CleanPage, DirtyPage, DurablePageReconciliationError, DurablePageWalObservation,
    FlushDirtyPageError, IndeterminatePageLogAppend, IndeterminatePageWrite, PageLog, PageNumber,
    PageRecoveryObservationBytesErrorReason, PageStore, PageVersion, StagePageWriteError,
    StagePageWriteEvidenceErrorReason, StoredPageSnapshotObservation, UnloggedPage,
    flush_dirty_page, reconcile_durable_page, stage_page_write,
};
use ntsql_wal::{
    CommitError, CommitLog, LogDurability, LogLineage, LogSequenceNumber, PersistentLogId,
    commit_durability,
};

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

/// Authoritative lookup of one transaction identity in a durable log view.
///
/// Implementations must return [`DurableCommitLookup::Absent`] only after
/// completely checking the matching lineage. Partial, corrupt, duplicate, or
/// otherwise uncertain views must return an error instead.
pub trait TransactionRecoverySource {
    /// Source-specific failure to establish an authoritative lookup.
    type Error;

    /// Looks up one complete transaction identity and atomically pairs the
    /// result with the lineage that was searched.
    fn lookup_durable_commit(
        &mut self,
        transaction_id: TransactionId,
    ) -> Result<(LogLineage, DurableCommitLookup), Self::Error>;
}

/// Result of authoritatively searching one durable log view.
///
/// This adapter-provided value is data, not proof by itself. Only
/// [`TransactionCoordinator::resolve`] can convert it into terminal transaction
/// state after validating token ownership, lifecycle, and lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableCommitLookup {
    /// Exactly one durable commit record was found at this internal position.
    Found {
        /// Adapter-assigned position of the durable record.
        position: LogSequenceNumber,
    },
    /// The complete durable view contains no record for this identity.
    Absent,
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

/// Persisted transaction identity fields observed in a durable log.
///
/// This value is data, not a coordinator lifecycle token or proof of a commit.
/// It may compare with a caller-supplied [`TransactionId`], but it cannot
/// reconstruct one:
///
/// ```compile_fail
/// use ntsql_transaction::{DurableTransactionIdentityObservation, TransactionId};
///
/// fn cannot_reconstruct(
///     observation: DurableTransactionIdentityObservation,
/// ) -> TransactionId {
///     observation.into()
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DurableTransactionIdentityObservation {
    epoch: NonZeroU64,
    sequence: NonZeroU64,
}

impl DurableTransactionIdentityObservation {
    /// Constructs one compare-only observation from exact persisted fields.
    pub fn new(
        epoch: u64,
        sequence: u64,
    ) -> Result<Self, DurableTransactionIdentityObservationError> {
        let Some(epoch_value) = NonZeroU64::new(epoch) else {
            return Err(DurableTransactionIdentityObservationError {
                epoch,
                sequence,
                reason: DurableTransactionIdentityObservationErrorReason::ZeroEpoch,
            });
        };
        let Some(sequence_value) = NonZeroU64::new(sequence) else {
            return Err(DurableTransactionIdentityObservationError {
                epoch,
                sequence,
                reason: DurableTransactionIdentityObservationErrorReason::ZeroSequence,
            });
        };
        Ok(Self {
            epoch: epoch_value,
            sequence: sequence_value,
        })
    }

    /// Returns the exact persisted epoch field.
    #[must_use]
    pub const fn epoch(self) -> u64 {
        self.epoch.get()
    }

    /// Returns the exact persisted transaction sequence field.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence.get()
    }

    /// Compares this observation with a caller-supplied lifecycle identity.
    #[must_use]
    pub fn matches_transaction_id(self, transaction_id: TransactionId) -> bool {
        self.epoch.get() == transaction_id.epoch().get()
            && self.sequence.get() == transaction_id.sequence()
    }
}

impl fmt::Display for DurableTransactionIdentityObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.epoch.get(), self.sequence.get())
    }
}

/// Reason persisted transaction identity fields could not form an observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableTransactionIdentityObservationErrorReason {
    /// The persisted coordinator epoch was zero.
    ZeroEpoch,
    /// The persisted coordinator-local sequence was zero.
    ZeroSequence,
}

/// Rejected persisted transaction identity fields retained without alteration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableTransactionIdentityObservationError {
    epoch: u64,
    sequence: u64,
    reason: DurableTransactionIdentityObservationErrorReason,
}

impl DurableTransactionIdentityObservationError {
    /// Returns the rejected epoch field.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the rejected sequence field.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the exact validation failure.
    #[must_use]
    pub const fn reason(&self) -> DurableTransactionIdentityObservationErrorReason {
        self.reason
    }

    /// Returns both rejected fields and the exact validation failure.
    #[must_use]
    pub const fn into_parts(self) -> (u64, u64, DurableTransactionIdentityObservationErrorReason) {
        (self.epoch, self.sequence, self.reason)
    }
}

impl fmt::Display for DurableTransactionIdentityObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "persisted transaction identity {}:{} is invalid: {:?}",
            self.epoch, self.sequence, self.reason
        )
    }
}

impl Error for DurableTransactionIdentityObservationError {}

/// Adapter-neutral observation of one durable transaction-owned page record.
///
/// The owner fields and page record remain observations supplied from a
/// complete durable prefix. Pairing them grants no commit, replay, or page-store
/// authority.
#[derive(Debug, Eq, PartialEq)]
pub struct DurableTransactionPageObservation<const N: usize> {
    owner: DurableTransactionIdentityObservation,
    page: DurablePageWalObservation<N>,
}

impl<const N: usize> DurableTransactionPageObservation<N> {
    /// Pairs one persisted owner with one validated durable page observation.
    #[must_use]
    pub fn new(
        owner: DurableTransactionIdentityObservation,
        page: DurablePageWalObservation<N>,
    ) -> Self {
        Self { owner, page }
    }

    /// Projects exact raw adapter fields into one owned-page observation.
    ///
    /// Owner fields are validated before page fields. A failure retains every
    /// supplied owner field, page field, byte, and lineage-bound position.
    pub fn from_bytes(
        epoch: u64,
        sequence: u64,
        page_number: PageNumber,
        page_version: PageVersion,
        bytes: [u8; N],
        position: LogSequenceNumber,
    ) -> Result<Self, DurableTransactionPageObservationBytesError<N>> {
        let owner = match DurableTransactionIdentityObservation::new(epoch, sequence) {
            Ok(owner) => owner,
            Err(error) => {
                return Err(DurableTransactionPageObservationBytesError {
                    epoch,
                    sequence,
                    page_number,
                    page_version,
                    bytes,
                    position,
                    reason: DurableTransactionPageObservationBytesErrorReason::Identity(
                        error.reason(),
                    ),
                });
            }
        };
        let page =
            match DurablePageWalObservation::from_bytes(page_number, page_version, bytes, position)
            {
                Ok(page) => page,
                Err(error) => {
                    let reason = error.reason();
                    let (page_number, page_version, bytes, position, _) = error.into_parts();
                    return Err(DurableTransactionPageObservationBytesError {
                        epoch,
                        sequence,
                        page_number,
                        page_version,
                        bytes,
                        position,
                        reason: DurableTransactionPageObservationBytesErrorReason::Page(reason),
                    });
                }
            };
        Ok(Self { owner, page })
    }

    /// Returns the observed persisted owner fields.
    #[must_use]
    pub const fn owner(&self) -> DurableTransactionIdentityObservation {
        self.owner
    }

    /// Returns the complete underlying durable page observation.
    #[must_use]
    pub const fn page(&self) -> &DurablePageWalObservation<N> {
        &self.page
    }

    /// Returns the lineage-bound position of the owned page record.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        self.page.position()
    }
}

/// Why raw adapter fields could not become an owned-page observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableTransactionPageObservationBytesErrorReason {
    /// The persisted owner fields were invalid.
    Identity(DurableTransactionIdentityObservationErrorReason),
    /// The page payload or position was invalid.
    Page(PageRecoveryObservationBytesErrorReason),
}

/// Rejected raw owned-page fields retained without alteration.
#[derive(Debug, Eq, PartialEq)]
pub struct DurableTransactionPageObservationBytesError<const N: usize> {
    epoch: u64,
    sequence: u64,
    page_number: PageNumber,
    page_version: PageVersion,
    bytes: [u8; N],
    position: LogSequenceNumber,
    reason: DurableTransactionPageObservationBytesErrorReason,
}

impl<const N: usize> DurableTransactionPageObservationBytesError<N> {
    /// Returns the rejected owner epoch.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the rejected owner sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the retained page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.page_number
    }

    /// Returns the retained page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.page_version
    }

    /// Returns the retained raw page bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Returns the retained lineage-bound position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.position
    }

    /// Returns the exact projection failure.
    #[must_use]
    pub const fn reason(&self) -> DurableTransactionPageObservationBytesErrorReason {
        self.reason
    }

    /// Returns every retained input and the exact projection failure.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        u64,
        u64,
        PageNumber,
        PageVersion,
        [u8; N],
        LogSequenceNumber,
        DurableTransactionPageObservationBytesErrorReason,
    ) {
        (
            self.epoch,
            self.sequence,
            self.page_number,
            self.page_version,
            self.bytes,
            self.position,
            self.reason,
        )
    }
}

impl<const N: usize> fmt::Display for DurableTransactionPageObservationBytesError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {}:{} page {} recovery observation is invalid: {:?}",
            self.epoch,
            self.sequence,
            self.page_number.get(),
            self.reason
        )
    }
}

impl<const N: usize> Error for DurableTransactionPageObservationBytesError<N> {}

/// Adapter-neutral observation of one complete durable commit record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTransactionCommitObservation {
    transaction: DurableTransactionIdentityObservation,
    position: LogSequenceNumber,
}

impl DurableTransactionCommitObservation {
    /// Constructs one observed commit from persisted identity and position.
    pub fn new(
        transaction: DurableTransactionIdentityObservation,
        position: LogSequenceNumber,
    ) -> Result<Self, DurableTransactionCommitObservationError> {
        if position.get() == 0 {
            return Err(DurableTransactionCommitObservationError {
                transaction,
                position,
            });
        }
        Ok(Self {
            transaction,
            position,
        })
    }

    /// Projects exact raw adapter fields into one durable commit observation.
    ///
    /// Identity fields are validated before the position. A failure retains all
    /// three raw inputs and the exact lineage capability on the position.
    pub fn from_fields(
        epoch: u64,
        sequence: u64,
        position: LogSequenceNumber,
    ) -> Result<Self, DurableTransactionCommitObservationFieldsError> {
        let transaction = match DurableTransactionIdentityObservation::new(epoch, sequence) {
            Ok(transaction) => transaction,
            Err(error) => {
                return Err(DurableTransactionCommitObservationFieldsError {
                    epoch,
                    sequence,
                    position,
                    reason: DurableTransactionCommitObservationFieldsErrorReason::Identity(
                        error.reason(),
                    ),
                });
            }
        };
        match Self::new(transaction, position) {
            Ok(observation) => Ok(observation),
            Err(error) => {
                let (_, position) = error.into_parts();
                Err(DurableTransactionCommitObservationFieldsError {
                    epoch,
                    sequence,
                    position,
                    reason: DurableTransactionCommitObservationFieldsErrorReason::ZeroPosition,
                })
            }
        }
    }

    /// Returns the persisted transaction identity fields.
    #[must_use]
    pub const fn transaction(&self) -> DurableTransactionIdentityObservation {
        self.transaction
    }

    /// Returns the exact durable commit position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.position
    }
}

/// Rejected zero-position durable commit observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTransactionCommitObservationError {
    transaction: DurableTransactionIdentityObservation,
    position: LogSequenceNumber,
}

impl DurableTransactionCommitObservationError {
    /// Returns the retained persisted transaction identity.
    #[must_use]
    pub const fn transaction(&self) -> DurableTransactionIdentityObservation {
        self.transaction
    }

    /// Returns the retained zero position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.position
    }

    /// Returns both retained inputs.
    #[must_use]
    pub fn into_parts(self) -> (DurableTransactionIdentityObservation, LogSequenceNumber) {
        (self.transaction, self.position)
    }
}

impl fmt::Display for DurableTransactionCommitObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {} durable commit position must be nonzero",
            self.transaction
        )
    }
}

impl Error for DurableTransactionCommitObservationError {}

/// Why raw commit fields could not become a durable commit observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableTransactionCommitObservationFieldsErrorReason {
    /// The persisted transaction identity fields were invalid.
    Identity(DurableTransactionIdentityObservationErrorReason),
    /// The supplied commit position was zero.
    ZeroPosition,
}

/// Rejected raw durable commit fields retained without alteration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTransactionCommitObservationFieldsError {
    epoch: u64,
    sequence: u64,
    position: LogSequenceNumber,
    reason: DurableTransactionCommitObservationFieldsErrorReason,
}

impl DurableTransactionCommitObservationFieldsError {
    /// Returns the rejected transaction epoch.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the rejected transaction sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the retained lineage-bound position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.position
    }

    /// Returns the exact projection failure.
    #[must_use]
    pub const fn reason(&self) -> DurableTransactionCommitObservationFieldsErrorReason {
        self.reason
    }

    /// Returns every retained input and the exact projection failure.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        u64,
        u64,
        LogSequenceNumber,
        DurableTransactionCommitObservationFieldsErrorReason,
    ) {
        (self.epoch, self.sequence, self.position, self.reason)
    }
}

impl fmt::Display for DurableTransactionCommitObservationFieldsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {}:{} durable commit observation is invalid: {:?}",
            self.epoch, self.sequence, self.reason
        )
    }
}

impl Error for DurableTransactionCommitObservationFieldsError {}

/// Inert per-record classification of durable transaction commit evidence.
///
/// This value deliberately cannot become a committed lifecycle token:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransaction, DurableTransactionPageCommitClassification,
/// };
///
/// fn cannot_create_committed(
///     classification: DurableTransactionPageCommitClassification,
/// ) -> CommittedTransaction {
///     classification.into()
/// }
/// ```
///
/// It also cannot create a dirty page or write permit:
///
/// ```compile_fail
/// use ntsql_page::DirtyPage;
/// use ntsql_transaction::DurableTransactionPageCommitClassification;
///
/// fn cannot_create_dirty<const N: usize>(
///     classification: DurableTransactionPageCommitClassification,
/// ) -> DirtyPage<N> {
///     classification.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::DurableTransactionPageCommitClassification;
///
/// fn cannot_authorize_write(
///     classification: DurableTransactionPageCommitClassification,
/// ) -> PageWritePermit<'static> {
///     classification.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTransactionPageCommitClassification {
    /// Exactly one later durable commit matches the page owner.
    Committed {
        /// Exact durable position of the owned page record.
        page_position: LogSequenceNumber,
        /// Exact later position of the matching durable commit.
        commit_position: LogSequenceNumber,
    },
    /// The complete supplied durable commit prefix has no matching identity.
    Uncommitted {
        /// Exact durable position of the owned page record.
        page_position: LogSequenceNumber,
    },
}

/// Contradiction that prevents durable transaction-page commit classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTransactionPageClassificationError {
    /// The owned page position belongs to another WAL lineage.
    ForeignPageLineage {
        /// Position supplied by the owned page observation.
        position: LogSequenceNumber,
    },
    /// A durable commit position belongs to another WAL lineage.
    ForeignCommitLineage {
        /// Position supplied by the commit observation.
        position: LogSequenceNumber,
    },
    /// Two adjacent observations repeat one position and identity.
    DuplicateCommitPosition {
        /// Repeated durable commit position.
        position: LogSequenceNumber,
    },
    /// Two adjacent observations reuse one position for different identities.
    ContradictoryCommitPosition {
        /// Contradictory durable commit position.
        position: LogSequenceNumber,
    },
    /// Commit observations were not supplied in strictly increasing order.
    NonAdvancingCommitPosition {
        /// Previous durable commit position.
        previous: LogSequenceNumber,
        /// Later supplied position that did not advance.
        actual: LogSequenceNumber,
    },
    /// More than one durable commit observation matches the page owner.
    DuplicateMatchingCommit {
        /// Persisted identity shared by the page and both commits.
        transaction: DurableTransactionIdentityObservation,
        /// First matching durable commit position.
        first: LogSequenceNumber,
        /// Later supplied matching durable commit position.
        duplicate: LogSequenceNumber,
    },
    /// The sole matching commit does not occur after the page record.
    CommitNotAfterPage {
        /// Persisted identity shared by the page and commit.
        transaction: DurableTransactionIdentityObservation,
        /// Durable owned-page position.
        page_position: LogSequenceNumber,
        /// Matching durable commit position.
        commit_position: LogSequenceNumber,
    },
}

impl fmt::Display for DurableTransactionPageClassificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignPageLineage { position } => write!(
                formatter,
                "transaction-owned page position {} belongs to another WAL lineage",
                position.get()
            ),
            Self::ForeignCommitLineage { position } => write!(
                formatter,
                "durable commit position {} belongs to another WAL lineage",
                position.get()
            ),
            Self::DuplicateCommitPosition { position } => write!(
                formatter,
                "durable commit position {} repeats with the same identity",
                position.get()
            ),
            Self::ContradictoryCommitPosition { position } => write!(
                formatter,
                "durable commit position {} repeats with a different identity",
                position.get()
            ),
            Self::NonAdvancingCommitPosition { previous, actual } => write!(
                formatter,
                "durable commit position {} does not advance beyond {}",
                actual.get(),
                previous.get()
            ),
            Self::DuplicateMatchingCommit {
                transaction,
                first,
                duplicate,
            } => write!(
                formatter,
                "transaction {transaction} has matching durable commits at positions {} and {}",
                first.get(),
                duplicate.get()
            ),
            Self::CommitNotAfterPage {
                transaction,
                page_position,
                commit_position,
            } => write!(
                formatter,
                "transaction {transaction} commit position {} is not after owned-page position {}",
                commit_position.get(),
                page_position.get()
            ),
        }
    }
}

impl Error for DurableTransactionPageClassificationError {}

fn scan_durable_transaction_commits<'observation, Commits>(
    expected_lineage: &LogLineage,
    matching_transaction: Option<DurableTransactionIdentityObservation>,
    commit_observations: Commits,
) -> Result<Option<&'observation LogSequenceNumber>, DurableTransactionPageClassificationError>
where
    Commits: IntoIterator<Item = &'observation DurableTransactionCommitObservation>,
{
    let mut previous: Option<&DurableTransactionCommitObservation> = None;
    let mut matching_position: Option<&LogSequenceNumber> = None;
    for observation in commit_observations {
        if !expected_lineage.same_lineage(observation.position().lineage()) {
            return Err(
                DurableTransactionPageClassificationError::ForeignCommitLineage {
                    position: observation.position().clone(),
                },
            );
        }

        if matching_transaction == Some(observation.transaction()) {
            if let Some(first) = matching_position {
                return Err(
                    DurableTransactionPageClassificationError::DuplicateMatchingCommit {
                        transaction: observation.transaction(),
                        first: first.clone(),
                        duplicate: observation.position().clone(),
                    },
                );
            }
            matching_position = Some(observation.position());
        }

        if let Some(previous) = previous {
            if observation.position().get() == previous.position().get() {
                let reason = if observation.transaction() == previous.transaction() {
                    DurableTransactionPageClassificationError::DuplicateCommitPosition {
                        position: observation.position().clone(),
                    }
                } else {
                    DurableTransactionPageClassificationError::ContradictoryCommitPosition {
                        position: observation.position().clone(),
                    }
                };
                return Err(reason);
            }
            if observation.position().get() < previous.position().get() {
                return Err(
                    DurableTransactionPageClassificationError::NonAdvancingCommitPosition {
                        previous: previous.position().clone(),
                        actual: observation.position().clone(),
                    },
                );
            }
        }
        previous = Some(observation);
    }
    Ok(matching_position)
}

/// Classifies one durable transaction-owned page from a complete commit prefix.
///
/// `commit_observations` must contain every durable commit observation in the
/// expected lineage in strictly increasing physical order. The function
/// validates the complete iterator, including unrelated identities, before
/// returning. An absent match means uncommitted only under that completeness
/// contract.
///
/// The function performs one pass with bounded state. It does not select among
/// repeated page records, return an image, authorize replay, or access a store.
pub fn classify_durable_transaction_page<'observation, const N: usize, Commits>(
    expected_lineage: &LogLineage,
    page: &DurableTransactionPageObservation<N>,
    commit_observations: Commits,
) -> Result<DurableTransactionPageCommitClassification, DurableTransactionPageClassificationError>
where
    Commits: IntoIterator<Item = &'observation DurableTransactionCommitObservation>,
{
    if !expected_lineage.same_lineage(page.position().lineage()) {
        return Err(
            DurableTransactionPageClassificationError::ForeignPageLineage {
                position: page.position().clone(),
            },
        );
    }

    let matching_position = scan_durable_transaction_commits(
        expected_lineage,
        Some(page.owner()),
        commit_observations,
    )?;

    let Some(commit_position) = matching_position else {
        return Ok(DurableTransactionPageCommitClassification::Uncommitted {
            page_position: page.position().clone(),
        });
    };
    if commit_position.get() <= page.position().get() {
        return Err(
            DurableTransactionPageClassificationError::CommitNotAfterPage {
                transaction: page.owner(),
                page_position: page.position().clone(),
                commit_position: commit_position.clone(),
            },
        );
    }
    Ok(DurableTransactionPageCommitClassification::Committed {
        page_position: page.position().clone(),
        commit_position: commit_position.clone(),
    })
}

/// Borrowed latest committed full-image page selected from durable evidence.
///
/// This value deliberately cannot create transaction lifecycle authority:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     LatestCommittedTransactionPage, TransactionId,
/// };
///
/// fn cannot_create_transaction_id<const N: usize>(
///     selection: LatestCommittedTransactionPage<'_, N>,
/// ) -> TransactionId {
///     selection.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransaction, LatestCommittedTransactionPage,
/// };
///
/// fn cannot_create_committed<const N: usize>(
///     selection: LatestCommittedTransactionPage<'_, N>,
/// ) -> CommittedTransaction {
///     selection.into()
/// }
/// ```
///
/// It also cannot create dirty or write-authorizing state:
///
/// ```compile_fail
/// use ntsql_page::DirtyPage;
/// use ntsql_transaction::LatestCommittedTransactionPage;
///
/// fn cannot_create_dirty<const N: usize>(
///     selection: LatestCommittedTransactionPage<'_, N>,
/// ) -> DirtyPage<N> {
///     selection.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     LatestCommittedTransactionPage, TransactionDirtyPage,
/// };
///
/// fn cannot_create_transaction_dirty<const N: usize>(
///     selection: LatestCommittedTransactionPage<'_, N>,
/// ) -> TransactionDirtyPage<N> {
///     selection.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::LatestCommittedTransactionPage;
///
/// fn cannot_authorize_write<const N: usize>(
///     selection: LatestCommittedTransactionPage<'_, N>,
/// ) -> PageWritePermit<'static> {
///     selection.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatestCommittedTransactionPage<'observation, const N: usize> {
    observation: &'observation DurableTransactionPageObservation<N>,
    commit_position: LogSequenceNumber,
}

impl<'observation, const N: usize> LatestCommittedTransactionPage<'observation, N> {
    /// Returns the exact borrowed owned-page observation selected by WAL order.
    #[must_use]
    pub const fn observation(&self) -> &'observation DurableTransactionPageObservation<N> {
        self.observation
    }

    /// Returns the matching durable commit position for the selected owner.
    #[must_use]
    pub const fn commit_position(&self) -> &LogSequenceNumber {
        &self.commit_position
    }
}

/// Inert result of selecting one page's latest committed durable observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTransactionPageSelection<'observation, const N: usize> {
    /// No supplied owned-page observation had matching durable commit evidence.
    NoCommittedPage {
        /// Requested page number for which no committed observation was found.
        page_number: PageNumber,
    },
    /// Greatest committed owned-page observation by physical WAL position.
    LatestCommitted(LatestCommittedTransactionPage<'observation, N>),
}

/// Contradiction that prevents latest committed transaction-page selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTransactionPageSelectionError {
    /// The complete commit slice failed validation before page selection.
    CommitPrefix {
        /// Exact ADR 0023 commit-evidence failure.
        source: Box<DurableTransactionPageClassificationError>,
    },
    /// An owned observation describes another page.
    UnexpectedOwnedPage {
        /// Page requested by the caller.
        expected: PageNumber,
        /// Page carried by the observation.
        actual: PageNumber,
        /// Position of the unexpected owned-page observation.
        position: LogSequenceNumber,
    },
    /// An owned-page position belongs to another WAL lineage.
    ForeignOwnedPageLineage {
        /// Page number carried by the observation.
        page_number: PageNumber,
        /// Position supplied by the owned-page observation.
        position: LogSequenceNumber,
    },
    /// Two adjacent observations repeat every relevant field at one position.
    DuplicateOwnedPagePosition {
        /// Requested page number.
        page_number: PageNumber,
        /// Repeated owned-page position.
        position: LogSequenceNumber,
    },
    /// Two adjacent observations reuse one position with differing evidence.
    ContradictoryOwnedPagePosition {
        /// Requested page number.
        page_number: PageNumber,
        /// Contradictory owned-page position.
        position: LogSequenceNumber,
    },
    /// Owned-page observations were not supplied in strictly increasing order.
    NonAdvancingOwnedPagePosition {
        /// Requested page number.
        page_number: PageNumber,
        /// Previous owned-page position.
        previous: LogSequenceNumber,
        /// Later supplied position that did not advance.
        actual: LogSequenceNumber,
    },
    /// One owned-page observation failed ADR 0023 classification.
    PageClassification {
        /// Page number carried by the observation.
        page_number: PageNumber,
        /// Position of the owned-page observation.
        page_position: LogSequenceNumber,
        /// Exact ADR 0023 per-record classification failure.
        source: Box<DurableTransactionPageClassificationError>,
    },
}

impl fmt::Display for DurableTransactionPageSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommitPrefix { source } => {
                write!(
                    formatter,
                    "complete durable commit prefix is invalid: {source}"
                )
            }
            Self::UnexpectedOwnedPage {
                expected,
                actual,
                position,
            } => write!(
                formatter,
                "owned-page observation at position {} describes page {}, expected page {}",
                position.get(),
                actual.get(),
                expected.get()
            ),
            Self::ForeignOwnedPageLineage {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} owned-page position {} belongs to another WAL lineage",
                page_number.get(),
                position.get()
            ),
            Self::DuplicateOwnedPagePosition {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} owned-page position {} repeats identical evidence",
                page_number.get(),
                position.get()
            ),
            Self::ContradictoryOwnedPagePosition {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} owned-page position {} repeats contradictory evidence",
                page_number.get(),
                position.get()
            ),
            Self::NonAdvancingOwnedPagePosition {
                page_number,
                previous,
                actual,
            } => write!(
                formatter,
                "page {} owned-page position {} does not advance beyond {}",
                page_number.get(),
                actual.get(),
                previous.get()
            ),
            Self::PageClassification {
                page_number,
                page_position,
                source,
            } => write!(
                formatter,
                "page {} owned-page observation at position {} cannot be classified: {}",
                page_number.get(),
                page_position.get(),
                source
            ),
        }
    }
}

impl Error for DurableTransactionPageSelectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CommitPrefix { source } | Self::PageClassification { source, .. } => Some(source),
            Self::UnexpectedOwnedPage { .. }
            | Self::ForeignOwnedPageLineage { .. }
            | Self::DuplicateOwnedPagePosition { .. }
            | Self::ContradictoryOwnedPagePosition { .. }
            | Self::NonAdvancingOwnedPagePosition { .. } => None,
        }
    }
}

fn same_owned_page_evidence<const N: usize>(
    left: &DurableTransactionPageObservation<N>,
    right: &DurableTransactionPageObservation<N>,
) -> bool {
    left.owner() == right.owner()
        && left.page().page_version() == right.page().page_version()
        && left.page().image().bytes() == right.page().image().bytes()
}

/// Selects one page's latest committed full image from complete durable evidence.
///
/// `owned_pages` must contain every durable transaction-owned observation for
/// `expected_page` in strictly increasing physical order. `commit_observations`
/// must contain every durable commit observation in the complete matching
/// prefix. The commit slice is validated even when no owned page exists.
///
/// Selection uses only owned-page WAL position. Uncommitted records are omitted
/// from the selected state but remain fully validated. The function borrows its
/// inputs, allocates no collection, authorizes no replay, and accesses no store.
pub fn select_latest_committed_transaction_page<'page, 'commit, const N: usize, Pages>(
    expected_lineage: &LogLineage,
    expected_page: PageNumber,
    owned_pages: Pages,
    commit_observations: &'commit [DurableTransactionCommitObservation],
) -> Result<DurableTransactionPageSelection<'page, N>, DurableTransactionPageSelectionError>
where
    Pages: IntoIterator<Item = &'page DurableTransactionPageObservation<N>>,
{
    let _ = scan_durable_transaction_commits(expected_lineage, None, commit_observations.iter())
        .map_err(
            |source| DurableTransactionPageSelectionError::CommitPrefix {
                source: Box::new(source),
            },
        )?;

    let mut previous: Option<&DurableTransactionPageObservation<N>> = None;
    let mut selected = None;
    for observation in owned_pages {
        let actual_page = observation.page().page_number();
        if actual_page != expected_page {
            return Err(DurableTransactionPageSelectionError::UnexpectedOwnedPage {
                expected: expected_page,
                actual: actual_page,
                position: observation.position().clone(),
            });
        }
        if !expected_lineage.same_lineage(observation.position().lineage()) {
            return Err(
                DurableTransactionPageSelectionError::ForeignOwnedPageLineage {
                    page_number: actual_page,
                    position: observation.position().clone(),
                },
            );
        }

        if let Some(previous) = previous {
            if observation.position().get() == previous.position().get() {
                let error = if same_owned_page_evidence(previous, observation) {
                    DurableTransactionPageSelectionError::DuplicateOwnedPagePosition {
                        page_number: actual_page,
                        position: observation.position().clone(),
                    }
                } else {
                    DurableTransactionPageSelectionError::ContradictoryOwnedPagePosition {
                        page_number: actual_page,
                        position: observation.position().clone(),
                    }
                };
                return Err(error);
            }
            if observation.position().get() < previous.position().get() {
                return Err(
                    DurableTransactionPageSelectionError::NonAdvancingOwnedPagePosition {
                        page_number: actual_page,
                        previous: previous.position().clone(),
                        actual: observation.position().clone(),
                    },
                );
            }
        }

        let classification = classify_durable_transaction_page(
            expected_lineage,
            observation,
            commit_observations.iter(),
        )
        .map_err(
            |source| DurableTransactionPageSelectionError::PageClassification {
                page_number: actual_page,
                page_position: observation.position().clone(),
                source: Box::new(source),
            },
        )?;
        if let DurableTransactionPageCommitClassification::Committed {
            commit_position, ..
        } = classification
        {
            selected = Some(LatestCommittedTransactionPage {
                observation,
                commit_position,
            });
        }
        previous = Some(observation);
    }

    Ok(match selected {
        Some(selected) => DurableTransactionPageSelection::LatestCommitted(selected),
        None => DurableTransactionPageSelection::NoCommittedPage {
            page_number: expected_page,
        },
    })
}

/// Committed-relative reconciliation of one durable page and stored snapshot.
///
/// This value deliberately cannot create transaction lifecycle authority:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableCommittedTransactionPageReconciliation, TransactionId,
/// };
///
/// fn cannot_create_transaction_id<const N: usize>(
///     reconciliation: DurableCommittedTransactionPageReconciliation<'_, N>,
/// ) -> TransactionId {
///     reconciliation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransaction, DurableCommittedTransactionPageReconciliation,
/// };
///
/// fn cannot_create_committed<const N: usize>(
///     reconciliation: DurableCommittedTransactionPageReconciliation<'_, N>,
/// ) -> CommittedTransaction {
///     reconciliation.into()
/// }
/// ```
///
/// It also cannot create dirty or write-authorizing state:
///
/// ```compile_fail
/// use ntsql_page::DirtyPage;
/// use ntsql_transaction::DurableCommittedTransactionPageReconciliation;
///
/// fn cannot_create_dirty<const N: usize>(
///     reconciliation: DurableCommittedTransactionPageReconciliation<'_, N>,
/// ) -> DirtyPage<N> {
///     reconciliation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableCommittedTransactionPageReconciliation, TransactionDirtyPage,
/// };
///
/// fn cannot_create_transaction_dirty<const N: usize>(
///     reconciliation: DurableCommittedTransactionPageReconciliation<'_, N>,
/// ) -> TransactionDirtyPage<N> {
///     reconciliation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::DurableCommittedTransactionPageReconciliation;
///
/// fn cannot_authorize_write<const N: usize>(
///     reconciliation: DurableCommittedTransactionPageReconciliation<'_, N>,
/// ) -> PageWritePermit<'static> {
///     reconciliation.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableCommittedTransactionPageReconciliation<'observation, const N: usize> {
    /// No committed transaction-owned page and no stored snapshot were observed.
    NoCommittedPage {
        /// Requested page number with no committed durable state.
        page_number: PageNumber,
    },
    /// A latest committed page exists but the store has no snapshot.
    StoreMissing {
        /// Exact latest committed transaction-owned observation.
        latest_committed: LatestCommittedTransactionPage<'observation, N>,
    },
    /// The store is backed by the exact latest committed page observation.
    ExactCurrent {
        /// Exact latest committed transaction-owned observation.
        latest_committed: LatestCommittedTransactionPage<'observation, N>,
    },
    /// The store is backed by an earlier committed page observation.
    StoreBehind {
        /// Durable owned-page position backing the current stored snapshot.
        stored_page_position: LogSequenceNumber,
        /// Matching durable commit position for the stored owned page.
        stored_commit_position: LogSequenceNumber,
        /// Exact later committed transaction-owned observation.
        latest_committed: LatestCommittedTransactionPage<'observation, N>,
    },
}

/// Contradiction that prevents committed-relative page reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableCommittedTransactionPageReconciliationError {
    /// Complete commit or owner-aware evidence failed ADR 0025 selection.
    Selection {
        /// Exact latest-selection failure.
        source: Box<DurableTransactionPageSelectionError>,
    },
    /// Snapshot or physical page evidence failed ADR 0019 reconciliation.
    Physical {
        /// Exact physical reconciliation failure.
        source: Box<DurablePageReconciliationError>,
    },
    /// One owner-aware page has no physical projection at the same position.
    OwnedPagePositionUnbacked {
        /// Requested page number.
        page_number: PageNumber,
        /// Unbacked owner-aware page position.
        position: LogSequenceNumber,
    },
    /// Owner-aware and physical projections disagree at one shared position.
    OwnedPagePayloadContradiction {
        /// Requested page number.
        page_number: PageNumber,
        /// Position whose page version or bytes disagree.
        position: LogSequenceNumber,
    },
    /// The stored snapshot is backed by a physical raw-page record.
    SnapshotBackedByRawPage {
        /// Requested page number.
        page_number: PageNumber,
        /// Raw physical position backing the snapshot.
        position: LogSequenceNumber,
    },
    /// The stored snapshot is backed by an uncommitted owned-page record.
    SnapshotBackedByUncommittedTransactionPage {
        /// Requested page number.
        page_number: PageNumber,
        /// Persisted owner of the uncommitted page.
        transaction: DurableTransactionIdentityObservation,
        /// Uncommitted owned-page position backing the snapshot.
        page_position: LogSequenceNumber,
    },
    /// Snapshot-backing owner evidence failed ADR 0023 classification.
    SnapshotBackingClassification {
        /// Requested page number.
        page_number: PageNumber,
        /// Owned-page position backing the snapshot.
        page_position: LogSequenceNumber,
        /// Exact per-record classification failure.
        source: Box<DurableTransactionPageClassificationError>,
    },
    /// A committed snapshot backing exists despite an empty committed selection.
    CommittedSnapshotWithoutSelection {
        /// Requested page number.
        page_number: PageNumber,
        /// Committed owned-page position backing the snapshot.
        stored_page_position: LogSequenceNumber,
        /// Matching commit position for the stored page.
        stored_commit_position: LogSequenceNumber,
    },
    /// A committed snapshot backing occurs after the selected latest record.
    CommittedSnapshotAfterSelection {
        /// Requested page number.
        page_number: PageNumber,
        /// Committed owned-page position backing the snapshot.
        stored_page_position: LogSequenceNumber,
        /// Selected latest committed owned-page position.
        selected_page_position: LogSequenceNumber,
    },
}

impl fmt::Display for DurableCommittedTransactionPageReconciliationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selection { source } => {
                write!(formatter, "committed page selection failed: {source}")
            }
            Self::Physical { source } => {
                write!(formatter, "physical page reconciliation failed: {source}")
            }
            Self::OwnedPagePositionUnbacked {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} owned observation at position {} has no physical projection",
                page_number.get(),
                position.get()
            ),
            Self::OwnedPagePayloadContradiction {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} owned and physical projections contradict at position {}",
                page_number.get(),
                position.get()
            ),
            Self::SnapshotBackedByRawPage {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} snapshot is backed by raw physical position {}",
                page_number.get(),
                position.get()
            ),
            Self::SnapshotBackedByUncommittedTransactionPage {
                page_number,
                transaction,
                page_position,
            } => write!(
                formatter,
                "page {} snapshot is backed by uncommitted transaction {} at position {}",
                page_number.get(),
                transaction,
                page_position.get()
            ),
            Self::SnapshotBackingClassification {
                page_number,
                page_position,
                source,
            } => write!(
                formatter,
                "page {} snapshot backing at position {} cannot be classified: {}",
                page_number.get(),
                page_position.get(),
                source
            ),
            Self::CommittedSnapshotWithoutSelection {
                page_number,
                stored_page_position,
                stored_commit_position,
            } => write!(
                formatter,
                "page {} snapshot has committed backing at positions {}/{} but selection found no committed page",
                page_number.get(),
                stored_page_position.get(),
                stored_commit_position.get()
            ),
            Self::CommittedSnapshotAfterSelection {
                page_number,
                stored_page_position,
                selected_page_position,
            } => write!(
                formatter,
                "page {} committed snapshot position {} is after selected committed position {}",
                page_number.get(),
                stored_page_position.get(),
                selected_page_position.get()
            ),
        }
    }
}

impl Error for DurableCommittedTransactionPageReconciliationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Selection { source } => Some(source.as_ref()),
            Self::Physical { source } => Some(source.as_ref()),
            Self::SnapshotBackingClassification { source, .. } => Some(source.as_ref()),
            Self::OwnedPagePositionUnbacked { .. }
            | Self::OwnedPagePayloadContradiction { .. }
            | Self::SnapshotBackedByRawPage { .. }
            | Self::SnapshotBackedByUncommittedTransactionPage { .. }
            | Self::CommittedSnapshotWithoutSelection { .. }
            | Self::CommittedSnapshotAfterSelection { .. } => None,
        }
    }
}

fn validate_owned_physical_page_projections<const N: usize>(
    page_number: PageNumber,
    physical_pages: &[DurablePageWalObservation<N>],
    owned_pages: &[DurableTransactionPageObservation<N>],
) -> Result<(), DurableCommittedTransactionPageReconciliationError> {
    let mut physical_index = 0;
    for owned in owned_pages {
        while physical_pages
            .get(physical_index)
            .is_some_and(|physical| physical.position().get() < owned.position().get())
        {
            physical_index += 1;
        }
        let Some(physical) = physical_pages.get(physical_index) else {
            return Err(
                DurableCommittedTransactionPageReconciliationError::OwnedPagePositionUnbacked {
                    page_number,
                    position: owned.position().clone(),
                },
            );
        };
        if physical.position().get() != owned.position().get() {
            return Err(
                DurableCommittedTransactionPageReconciliationError::OwnedPagePositionUnbacked {
                    page_number,
                    position: owned.position().clone(),
                },
            );
        }
        if physical.page_version() != owned.page().page_version()
            || physical.image().bytes() != owned.page().image().bytes()
        {
            return Err(
                DurableCommittedTransactionPageReconciliationError::OwnedPagePayloadContradiction {
                    page_number,
                    position: owned.position().clone(),
                },
            );
        }
        physical_index += 1;
    }
    Ok(())
}

fn resolve_committed_snapshot_reconciliation<'observation, const N: usize>(
    page_number: PageNumber,
    selection: DurableTransactionPageSelection<'observation, N>,
    stored_page_position: LogSequenceNumber,
    stored_commit_position: LogSequenceNumber,
) -> Result<
    DurableCommittedTransactionPageReconciliation<'observation, N>,
    DurableCommittedTransactionPageReconciliationError,
> {
    let DurableTransactionPageSelection::LatestCommitted(latest_committed) = selection else {
        return Err(
            DurableCommittedTransactionPageReconciliationError::CommittedSnapshotWithoutSelection {
                page_number,
                stored_page_position,
                stored_commit_position,
            },
        );
    };
    match stored_page_position
        .get()
        .cmp(&latest_committed.observation().position().get())
    {
        std::cmp::Ordering::Equal => {
            Ok(DurableCommittedTransactionPageReconciliation::ExactCurrent { latest_committed })
        }
        std::cmp::Ordering::Less => {
            Ok(DurableCommittedTransactionPageReconciliation::StoreBehind {
                stored_page_position,
                stored_commit_position,
                latest_committed,
            })
        }
        std::cmp::Ordering::Greater => Err(
            DurableCommittedTransactionPageReconciliationError::CommittedSnapshotAfterSelection {
                page_number,
                stored_page_position,
                selected_page_position: latest_committed.observation().position().clone(),
            },
        ),
    }
}

/// Reconciles latest committed transaction state with physical WAL and store
/// evidence without authorizing mutation.
///
/// `physical_pages` must contain the ADR 0019 physical projection of every
/// durable full-image record for `page_number`, including every transaction-owned
/// record. `owned_pages` must contain every owner-aware projection for the same
/// complete durable prefix. Unmatched physical positions are raw only under
/// those completeness contracts.
///
/// Validation deterministically proceeds through ADR 0025 selection, ADR 0019
/// physical reconciliation, cross-projection integrity, and snapshot-backing
/// classification. Success builds no collection and the result borrows only the
/// selected owned-page observation.
pub fn reconcile_committed_transaction_page<'observation, const N: usize>(
    expected_lineage: &LogLineage,
    page_number: PageNumber,
    snapshot: Option<&StoredPageSnapshotObservation<N>>,
    physical_pages: &[DurablePageWalObservation<N>],
    owned_pages: &'observation [DurableTransactionPageObservation<N>],
    commit_observations: &[DurableTransactionCommitObservation],
) -> Result<
    DurableCommittedTransactionPageReconciliation<'observation, N>,
    DurableCommittedTransactionPageReconciliationError,
> {
    let selection = select_latest_committed_transaction_page(
        expected_lineage,
        page_number,
        owned_pages.iter(),
        commit_observations,
    )
    .map_err(
        |source| DurableCommittedTransactionPageReconciliationError::Selection {
            source: Box::new(source),
        },
    )?;

    let _ = reconcile_durable_page(
        expected_lineage,
        page_number,
        snapshot,
        physical_pages.iter(),
    )
    .map_err(
        |source| DurableCommittedTransactionPageReconciliationError::Physical {
            source: Box::new(source),
        },
    )?;

    validate_owned_physical_page_projections(page_number, physical_pages, owned_pages)?;

    let Some(snapshot) = snapshot else {
        return Ok(match selection {
            DurableTransactionPageSelection::NoCommittedPage { page_number } => {
                DurableCommittedTransactionPageReconciliation::NoCommittedPage { page_number }
            }
            DurableTransactionPageSelection::LatestCommitted(latest_committed) => {
                DurableCommittedTransactionPageReconciliation::StoreMissing { latest_committed }
            }
        });
    };

    let Ok(backing_index) = owned_pages
        .binary_search_by_key(&snapshot.required_position().get(), |observation| {
            observation.position().get()
        })
    else {
        return Err(
            DurableCommittedTransactionPageReconciliationError::SnapshotBackedByRawPage {
                page_number,
                position: snapshot.required_position().clone(),
            },
        );
    };
    let backing = &owned_pages[backing_index];
    let classification =
        classify_durable_transaction_page(expected_lineage, backing, commit_observations.iter())
            .map_err(|source| {
                DurableCommittedTransactionPageReconciliationError::SnapshotBackingClassification {
                    page_number,
                    page_position: backing.position().clone(),
                    source: Box::new(source),
                }
            })?;

    match classification {
        DurableTransactionPageCommitClassification::Uncommitted { page_position } => Err(
            DurableCommittedTransactionPageReconciliationError::SnapshotBackedByUncommittedTransactionPage {
                page_number,
                transaction: backing.owner(),
                page_position,
            },
        ),
        DurableTransactionPageCommitClassification::Committed {
            page_position,
            commit_position,
        } => resolve_committed_snapshot_reconciliation(
            page_number,
            selection,
            page_position,
            commit_position,
        ),
    }
}

/// Exact store precondition retained by one inert committed-page recovery
/// candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableCommittedTransactionPageRecoveryPrecondition<'snapshot, const N: usize> {
    /// The validated page store had no snapshot.
    StoreMissing,
    /// The validated page store contained one exact earlier committed snapshot.
    ExactSnapshot {
        /// Exact snapshot supplied to committed-relative reconciliation.
        snapshot: &'snapshot StoredPageSnapshotObservation<N>,
        /// Durable commit position that classified the snapshot backing as
        /// committed.
        commit_position: LogSequenceNumber,
    },
}

/// Compare-only recovery candidate from validated committed-relative evidence.
///
/// The candidate binds an exact source-store precondition to the selected
/// committed target. It is not a replay command and cannot create transaction
/// lifecycle authority:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableCommittedTransactionPageRecoveryCandidate, TransactionId,
/// };
///
/// fn cannot_create_transaction_id<const N: usize>(
///     candidate: DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
/// ) -> TransactionId {
///     candidate.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransaction, DurableCommittedTransactionPageRecoveryCandidate,
/// };
///
/// fn cannot_create_committed<const N: usize>(
///     candidate: DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
/// ) -> CommittedTransaction {
///     candidate.into()
/// }
/// ```
///
/// It also cannot create dirty or write-authorizing state:
///
/// ```compile_fail
/// use ntsql_page::DirtyPage;
/// use ntsql_transaction::DurableCommittedTransactionPageRecoveryCandidate;
///
/// fn cannot_create_dirty<const N: usize>(
///     candidate: DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
/// ) -> DirtyPage<N> {
///     candidate.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableCommittedTransactionPageRecoveryCandidate, TransactionDirtyPage,
/// };
///
/// fn cannot_create_transaction_dirty<const N: usize>(
///     candidate: DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
/// ) -> TransactionDirtyPage<N> {
///     candidate.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::DurableCommittedTransactionPageRecoveryCandidate;
///
/// fn cannot_authorize_write<'candidate, const N: usize>(
///     candidate: DurableCommittedTransactionPageRecoveryCandidate<
///         'candidate,
///         'candidate,
///         N,
///     >,
/// ) -> PageWritePermit<'candidate> {
///     candidate.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableCommittedTransactionPageRecoveryCandidate<'observation, 'snapshot, const N: usize>
{
    precondition: DurableCommittedTransactionPageRecoveryPrecondition<'snapshot, N>,
    latest_committed: LatestCommittedTransactionPage<'observation, N>,
}

impl<'observation, 'snapshot, const N: usize>
    DurableCommittedTransactionPageRecoveryCandidate<'observation, 'snapshot, N>
{
    /// Returns the exact validated source-store precondition.
    #[must_use]
    pub const fn precondition(
        &self,
    ) -> &DurableCommittedTransactionPageRecoveryPrecondition<'snapshot, N> {
        &self.precondition
    }

    /// Returns the exact selected committed target.
    #[must_use]
    pub const fn latest_committed(&self) -> &LatestCommittedTransactionPage<'observation, N> {
        &self.latest_committed
    }
}

/// Inert decision produced while deriving a committed-page recovery candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableCommittedTransactionPageRecoveryDecision<'observation, 'snapshot, const N: usize> {
    /// No committed transaction-owned page exists and no recovery is proposed.
    NoCommittedPage {
        /// Requested page number with no committed durable state.
        page_number: PageNumber,
    },
    /// The store already contains the latest committed page.
    ExactCurrent {
        /// Exact selected committed observation already present in the store.
        latest_committed: LatestCommittedTransactionPage<'observation, N>,
    },
    /// A missing or behind store produced one compare-only candidate.
    Candidate(DurableCommittedTransactionPageRecoveryCandidate<'observation, 'snapshot, N>),
}

/// Failure to derive a candidate from committed-relative reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableCommittedTransactionPageRecoveryPlanningError {
    /// Committed-relative reconciliation failed.
    Reconciliation {
        /// Exact ADR 0026 reconciliation failure.
        source: Box<DurableCommittedTransactionPageReconciliationError>,
    },
    /// Reconciliation reported an absent store despite a supplied snapshot.
    UnexpectedSnapshotForAbsentStore {
        /// Requested page number.
        page_number: PageNumber,
        /// Supplied snapshot position.
        position: LogSequenceNumber,
    },
    /// Reconciliation reported a present store despite no supplied snapshot.
    MissingSnapshotForPresentStore {
        /// Requested page number.
        page_number: PageNumber,
        /// Position reconciliation reported as stored.
        expected_position: LogSequenceNumber,
    },
    /// The supplied snapshot position disagreed with the successful
    /// reconciliation result.
    SnapshotPositionContradiction {
        /// Requested page number.
        page_number: PageNumber,
        /// Position returned by reconciliation.
        reconciled_position: LogSequenceNumber,
        /// Position carried by the supplied snapshot.
        snapshot_position: LogSequenceNumber,
    },
}

impl fmt::Display for DurableCommittedTransactionPageRecoveryPlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reconciliation { source } => {
                write!(formatter, "committed page reconciliation failed: {source}")
            }
            Self::UnexpectedSnapshotForAbsentStore {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} reconciliation reported an absent store despite snapshot position {}",
                page_number.get(),
                position.get()
            ),
            Self::MissingSnapshotForPresentStore {
                page_number,
                expected_position,
            } => write!(
                formatter,
                "page {} reconciliation reported stored position {} without a snapshot",
                page_number.get(),
                expected_position.get()
            ),
            Self::SnapshotPositionContradiction {
                page_number,
                reconciled_position,
                snapshot_position,
            } => write!(
                formatter,
                "page {} reconciliation position {} contradicts snapshot position {}",
                page_number.get(),
                reconciled_position.get(),
                snapshot_position.get()
            ),
        }
    }
}

impl Error for DurableCommittedTransactionPageRecoveryPlanningError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Reconciliation { source } => Some(source.as_ref()),
            Self::UnexpectedSnapshotForAbsentStore { .. }
            | Self::MissingSnapshotForPresentStore { .. }
            | Self::SnapshotPositionContradiction { .. } => None,
        }
    }
}

/// Derives a non-authorizing recovery candidate from complete durable evidence.
///
/// This operation composes [`reconcile_committed_transaction_page`] over the
/// same inputs. Missing and behind stores retain exact source preconditions;
/// no-committed and exact-current states remain explicit no-candidate decisions.
/// The returned target is current only for the supplied durable prefix. A future
/// mutation gate must re-run authoritative reconciliation immediately before
/// attempting a physical write.
pub fn derive_committed_transaction_page_recovery_candidate<
    'observation,
    'snapshot,
    const N: usize,
>(
    expected_lineage: &LogLineage,
    page_number: PageNumber,
    snapshot: Option<&'snapshot StoredPageSnapshotObservation<N>>,
    physical_pages: &[DurablePageWalObservation<N>],
    owned_pages: &'observation [DurableTransactionPageObservation<N>],
    commit_observations: &[DurableTransactionCommitObservation],
) -> Result<
    DurableCommittedTransactionPageRecoveryDecision<'observation, 'snapshot, N>,
    DurableCommittedTransactionPageRecoveryPlanningError,
> {
    let reconciliation = reconcile_committed_transaction_page(
        expected_lineage,
        page_number,
        snapshot,
        physical_pages,
        owned_pages,
        commit_observations,
    )
    .map_err(|source| {
        DurableCommittedTransactionPageRecoveryPlanningError::Reconciliation {
            source: Box::new(source),
        }
    })?;

    match reconciliation {
        DurableCommittedTransactionPageReconciliation::NoCommittedPage { page_number } => {
            if let Some(snapshot) = snapshot {
                return Err(
                    DurableCommittedTransactionPageRecoveryPlanningError::UnexpectedSnapshotForAbsentStore {
                        page_number,
                        position: snapshot.required_position().clone(),
                    },
                );
            }
            Ok(DurableCommittedTransactionPageRecoveryDecision::NoCommittedPage { page_number })
        }
        DurableCommittedTransactionPageReconciliation::StoreMissing { latest_committed } => {
            if let Some(snapshot) = snapshot {
                return Err(
                    DurableCommittedTransactionPageRecoveryPlanningError::UnexpectedSnapshotForAbsentStore {
                        page_number,
                        position: snapshot.required_position().clone(),
                    },
                );
            }
            Ok(DurableCommittedTransactionPageRecoveryDecision::Candidate(
                DurableCommittedTransactionPageRecoveryCandidate {
                    precondition: DurableCommittedTransactionPageRecoveryPrecondition::StoreMissing,
                    latest_committed,
                },
            ))
        }
        DurableCommittedTransactionPageReconciliation::ExactCurrent { latest_committed } => {
            let Some(snapshot) = snapshot else {
                return Err(
                    DurableCommittedTransactionPageRecoveryPlanningError::MissingSnapshotForPresentStore {
                        page_number,
                        expected_position: latest_committed.observation().position().clone(),
                    },
                );
            };
            if snapshot.required_position() != latest_committed.observation().position() {
                return Err(
                    DurableCommittedTransactionPageRecoveryPlanningError::SnapshotPositionContradiction {
                        page_number,
                        reconciled_position: latest_committed.observation().position().clone(),
                        snapshot_position: snapshot.required_position().clone(),
                    },
                );
            }
            Ok(DurableCommittedTransactionPageRecoveryDecision::ExactCurrent { latest_committed })
        }
        DurableCommittedTransactionPageReconciliation::StoreBehind {
            stored_page_position,
            stored_commit_position,
            latest_committed,
        } => {
            let Some(snapshot) = snapshot else {
                return Err(
                    DurableCommittedTransactionPageRecoveryPlanningError::MissingSnapshotForPresentStore {
                        page_number,
                        expected_position: stored_page_position,
                    },
                );
            };
            if snapshot.required_position() != &stored_page_position {
                return Err(
                    DurableCommittedTransactionPageRecoveryPlanningError::SnapshotPositionContradiction {
                        page_number,
                        reconciled_position: stored_page_position,
                        snapshot_position: snapshot.required_position().clone(),
                    },
                );
            }
            Ok(DurableCommittedTransactionPageRecoveryDecision::Candidate(
                DurableCommittedTransactionPageRecoveryCandidate {
                    precondition:
                        DurableCommittedTransactionPageRecoveryPrecondition::ExactSnapshot {
                            snapshot,
                            commit_position: stored_commit_position,
                        },
                    latest_committed,
                },
            ))
        }
    }
}

/// Inert result of comparing a candidate with a newly observed store state.
///
/// Neither variant is write authority:
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::DurableCommittedTransactionPageRecoveryComparison;
///
/// fn cannot_authorize_write(
///     comparison: DurableCommittedTransactionPageRecoveryComparison,
/// ) -> PageWritePermit<'static> {
///     comparison.into()
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableCommittedTransactionPageRecoveryComparison {
    /// The exact source-store precondition still matches.
    SourceMatches,
    /// The exact target page image is already present in the store.
    ///
    /// This does not prove the candidate remains latest in a newer WAL prefix.
    TargetAlreadyPresent,
}

/// Contradiction between a recovery candidate and newly observed store state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableCommittedTransactionPageRecoveryComparisonError {
    /// The current snapshot describes another page.
    UnexpectedCurrentSnapshotPage {
        /// Candidate page number.
        expected: PageNumber,
        /// Current snapshot page number.
        actual: PageNumber,
        /// Current snapshot position.
        position: LogSequenceNumber,
    },
    /// The current snapshot belongs to another WAL lineage.
    ForeignCurrentSnapshotLineage {
        /// Candidate page number.
        page_number: PageNumber,
        /// Foreign current snapshot position.
        position: LogSequenceNumber,
    },
    /// The current snapshot uses the target position but contradicts its
    /// version or bytes.
    TargetSnapshotPayloadContradiction {
        /// Candidate page number.
        page_number: PageNumber,
        /// Contradictory target position.
        position: LogSequenceNumber,
    },
    /// The current snapshot uses the source position but contradicts its
    /// version or bytes.
    SourceSnapshotPayloadContradiction {
        /// Candidate page number.
        page_number: PageNumber,
        /// Contradictory source position.
        position: LogSequenceNumber,
    },
    /// The current store matches neither the candidate source nor target.
    StoreChanged {
        /// Candidate page number.
        page_number: PageNumber,
        /// Expected source position, or absence for a missing-store candidate.
        expected_source_position: Option<LogSequenceNumber>,
        /// Newly observed position, or absence when the store is now missing.
        actual_position: Option<LogSequenceNumber>,
    },
}

impl fmt::Display for DurableCommittedTransactionPageRecoveryComparisonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedCurrentSnapshotPage {
                expected,
                actual,
                position,
            } => write!(
                formatter,
                "recovery candidate for page {} received page {} at position {}",
                expected.get(),
                actual.get(),
                position.get()
            ),
            Self::ForeignCurrentSnapshotLineage {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} current snapshot position {} belongs to another log lineage",
                page_number.get(),
                position.get()
            ),
            Self::TargetSnapshotPayloadContradiction {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} current snapshot contradicts target payload at position {}",
                page_number.get(),
                position.get()
            ),
            Self::SourceSnapshotPayloadContradiction {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} current snapshot contradicts source payload at position {}",
                page_number.get(),
                position.get()
            ),
            Self::StoreChanged {
                page_number,
                expected_source_position,
                actual_position,
            } => write!(
                formatter,
                "page {} store changed from source position {:?} to position {:?}",
                page_number.get(),
                expected_source_position
                    .as_ref()
                    .map(LogSequenceNumber::get),
                actual_position.as_ref().map(LogSequenceNumber::get)
            ),
        }
    }
}

impl Error for DurableCommittedTransactionPageRecoveryComparisonError {}

fn snapshot_matches_owned_page<const N: usize>(
    snapshot: &StoredPageSnapshotObservation<N>,
    page: &DurableTransactionPageObservation<N>,
) -> bool {
    snapshot.page_number() == page.page().page_number()
        && snapshot.required_position() == page.position()
        && snapshot.page_version() == page.page().page_version()
        && snapshot.image().bytes() == page.page().image().bytes()
}

fn snapshots_match<const N: usize>(
    left: &StoredPageSnapshotObservation<N>,
    right: &StoredPageSnapshotObservation<N>,
) -> bool {
    left.page_number() == right.page_number()
        && left.required_position() == right.required_position()
        && left.page_version() == right.page_version()
        && left.image().bytes() == right.image().bytes()
}

/// Compares a candidate with the current store without authorizing mutation.
///
/// Page and lineage validation precede target/source position and payload
/// comparison. An absent store matches only a missing-store precondition. Exact
/// target presence provides idempotent retry classification, but does not prove
/// that later durable WAL evidence has not superseded the candidate.
pub fn compare_committed_transaction_page_recovery_candidate<const N: usize>(
    candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
    current_snapshot: Option<&StoredPageSnapshotObservation<N>>,
) -> Result<
    DurableCommittedTransactionPageRecoveryComparison,
    DurableCommittedTransactionPageRecoveryComparisonError,
> {
    let target = candidate.latest_committed().observation();
    let page_number = target.page().page_number();
    let Some(current_snapshot) = current_snapshot else {
        return match candidate.precondition() {
            DurableCommittedTransactionPageRecoveryPrecondition::StoreMissing => {
                Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches)
            }
            DurableCommittedTransactionPageRecoveryPrecondition::ExactSnapshot {
                snapshot, ..
            } => Err(
                DurableCommittedTransactionPageRecoveryComparisonError::StoreChanged {
                    page_number,
                    expected_source_position: Some(snapshot.required_position().clone()),
                    actual_position: None,
                },
            ),
        };
    };

    if current_snapshot.page_number() != page_number {
        return Err(
            DurableCommittedTransactionPageRecoveryComparisonError::UnexpectedCurrentSnapshotPage {
                expected: page_number,
                actual: current_snapshot.page_number(),
                position: current_snapshot.required_position().clone(),
            },
        );
    }
    if !target
        .position()
        .lineage()
        .same_lineage(current_snapshot.required_position().lineage())
    {
        return Err(
            DurableCommittedTransactionPageRecoveryComparisonError::ForeignCurrentSnapshotLineage {
                page_number,
                position: current_snapshot.required_position().clone(),
            },
        );
    }

    if current_snapshot.required_position().get() == target.position().get() {
        if snapshot_matches_owned_page(current_snapshot, target) {
            return Ok(DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent);
        }
        return Err(
            DurableCommittedTransactionPageRecoveryComparisonError::TargetSnapshotPayloadContradiction {
                page_number,
                position: current_snapshot.required_position().clone(),
            },
        );
    }

    match candidate.precondition() {
        DurableCommittedTransactionPageRecoveryPrecondition::StoreMissing => Err(
            DurableCommittedTransactionPageRecoveryComparisonError::StoreChanged {
                page_number,
                expected_source_position: None,
                actual_position: Some(current_snapshot.required_position().clone()),
            },
        ),
        DurableCommittedTransactionPageRecoveryPrecondition::ExactSnapshot { snapshot, .. } => {
            if current_snapshot.required_position().get() == snapshot.required_position().get() {
                if snapshots_match(current_snapshot, snapshot) {
                    return Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches);
                }
                return Err(
                    DurableCommittedTransactionPageRecoveryComparisonError::SourceSnapshotPayloadContradiction {
                        page_number,
                        position: current_snapshot.required_position().clone(),
                    },
                );
            }
            Err(
                DurableCommittedTransactionPageRecoveryComparisonError::StoreChanged {
                    page_number,
                    expected_source_position: Some(snapshot.required_position().clone()),
                    actual_position: Some(current_snapshot.required_position().clone()),
                },
            )
        }
    }
}

/// Trusted source of one stable authoritative durable page-recovery prefix.
///
/// Implementations must build all three slices from one durable prefix, keep
/// that prefix from advancing for the callback's full duration, and return the
/// callback output directly once invoked. The physical and owner-aware slices
/// must be complete for `page_number`; the commit slice must be complete for the
/// prefix. The domain trusts these adapter obligations.
///
/// `Output` cannot borrow callback evidence:
///
/// ```compile_fail
/// use ntsql_page::{DurablePageWalObservation, PageNumber};
/// use ntsql_transaction::DurableTransactionPageRecoverySource;
///
/// fn cannot_escape<'source, Source, const N: usize>(
///     source: &'source mut Source,
///     page_number: PageNumber,
/// ) -> Result<&'source [DurablePageWalObservation<N>], Source::Error>
/// where
///     Source: DurableTransactionPageRecoverySource<N>,
/// {
///     source.with_durable_page_evidence(
///         page_number,
///         |physical, _owned, _commits| physical,
///     )
/// }
/// ```
///
/// A safe implementation cannot manufacture an arbitrary successful `Output`
/// without invoking the callback:
///
/// ```compile_fail
/// use ntsql_page::{DurablePageWalObservation, PageNumber};
/// use ntsql_transaction::{
///     DurableTransactionCommitObservation, DurableTransactionPageObservation,
///     DurableTransactionPageRecoverySource,
/// };
/// use ntsql_wal::LogLineage;
///
/// struct InvalidSource {
///     lineage: LogLineage,
/// }
///
/// impl<const N: usize> DurableTransactionPageRecoverySource<N> for InvalidSource {
///     type Error = ();
///
///     fn lineage(&self) -> &LogLineage {
///         &self.lineage
///     }
///
///     fn with_durable_page_evidence<Output, Operation>(
///         &mut self,
///         _page_number: PageNumber,
///         _operation: Operation,
///     ) -> Result<Output, Self::Error>
///     where
///         Operation: for<'evidence> FnOnce(
///             &'evidence [DurablePageWalObservation<N>],
///             &'evidence [DurableTransactionPageObservation<N>],
///             &'evidence [DurableTransactionCommitObservation],
///         ) -> Output,
///     {
///         Ok(Default::default())
///     }
/// }
/// ```
pub trait DurableTransactionPageRecoverySource<const N: usize> {
    /// Source-specific failure before authoritative evidence is available.
    type Error;

    /// Returns the exact WAL lineage whose stable prefix will be projected.
    fn lineage(&self) -> &LogLineage;

    /// Runs one operation while the projected durable prefix remains stable.
    fn with_durable_page_evidence<Output, Operation>(
        &mut self,
        page_number: PageNumber,
        operation: Operation,
    ) -> Result<Output, Self::Error>
    where
        Operation: for<'evidence> FnOnce(
            &'evidence [DurablePageWalObservation<N>],
            &'evidence [DurableTransactionPageObservation<N>],
            &'evidence [DurableTransactionCommitObservation],
        ) -> Output;
}

/// Authoritative owned inventory for deterministic committed-page recovery.
///
/// Implementations return every distinct transaction-owned page present in one
/// durable prefix, strictly increasing by [`PageNumber`]. Raw-only and volatile
/// records must not enter the inventory. The returned values are inert
/// bookkeeping and grant no recovery write authority.
pub trait DurableTransactionPageRecoveryInventory<const N: usize> {
    /// Adapter-specific failure before a complete inventory is available.
    type Error;

    /// Returns one complete, strictly increasing durable owned-page inventory.
    fn durable_transaction_page_numbers(&mut self) -> Result<Vec<PageNumber>, Self::Error>;
}

type CommittedPageRecoveryAttemptBrand<'attempt> = (&'attempt (), fn(&'attempt ()) -> &'attempt ());

/// Single-use proof that the recovery gate authorized one exact store attempt.
///
/// Fields and construction are private. The invariant attempt brand cannot
/// escape the gate, be widened, or be cloned.
///
/// ```compile_fail
/// use ntsql_transaction::CommittedTransactionPageRecoveryWritePermit;
/// use ntsql_wal::LogLineage;
///
/// fn cannot_forge() {
///     let lineage = LogLineage::new();
///     let _permit = CommittedTransactionPageRecoveryWritePermit {
///         page_position: lineage.position(1),
///         commit_position: lineage.position(2),
///     };
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::CommittedTransactionPageRecoveryWritePermit;
///
/// fn cannot_clone(permit: CommittedTransactionPageRecoveryWritePermit<'_>) {
///     let _copy = permit.clone();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::CommittedTransactionPageRecoveryWritePermit;
///
/// fn cannot_widen<'attempt>(
///     permit: CommittedTransactionPageRecoveryWritePermit<'attempt>,
/// ) -> CommittedTransactionPageRecoveryWritePermit<'static> {
///     permit
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
#[must_use]
pub struct CommittedTransactionPageRecoveryWritePermit<'attempt> {
    page_position: LogSequenceNumber,
    commit_position: LogSequenceNumber,
    attempt_brand: PhantomData<CommittedPageRecoveryAttemptBrand<'attempt>>,
}

impl CommittedTransactionPageRecoveryWritePermit<'_> {
    /// Returns the exact committed page position authorized for this attempt.
    #[must_use]
    pub const fn page_position(&self) -> &LogSequenceNumber {
        &self.page_position
    }

    /// Returns the exact matching commit position authorized for this attempt.
    #[must_use]
    pub const fn commit_position(&self) -> &LogSequenceNumber {
        &self.commit_position
    }
}

fn with_committed_page_recovery_write_permit<Output, Operation>(
    page_position: LogSequenceNumber,
    commit_position: LogSequenceNumber,
    operation: Operation,
) -> Output
where
    Operation:
        for<'attempt> FnOnce(CommittedTransactionPageRecoveryWritePermit<'attempt>) -> Output,
{
    operation(CommittedTransactionPageRecoveryWritePermit {
        page_position,
        commit_position,
        attempt_brand: PhantomData,
    })
}

/// Recovery-only page-store port with atomic source recheck and replacement.
///
/// `compare_and_replace` must validate the permit against the candidate, recheck
/// the exact source precondition against authoritative current store state, and
/// durably write the exact target under one continuous exclusive hold. An error
/// after this method is invoked does not prove whether the target became durable.
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryStore,
///     DurableCommittedTransactionPageRecoveryCandidate,
/// };
///
/// fn cannot_call_without_permit<Store, const N: usize>(
///     store: &mut Store,
///     candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
/// )
/// where
///     Store: CommittedTransactionPageRecoveryStore<N>,
/// {
///     let _ = store.compare_and_replace(candidate);
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryStore,
///     DurableCommittedTransactionPageRecoveryCandidate,
/// };
///
/// fn cannot_substitute_live_permit<Store, const N: usize>(
///     store: &mut Store,
///     candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
///     permit: PageWritePermit<'_>,
/// )
/// where
///     Store: CommittedTransactionPageRecoveryStore<N>,
/// {
///     let _ = store.compare_and_replace(candidate, permit);
/// }
/// ```
pub trait CommittedTransactionPageRecoveryStore<const N: usize> {
    /// Adapter-specific current-snapshot observation failure.
    type ObservationError;
    /// Adapter-specific compare-and-replace failure.
    type WriteError;

    /// Returns the exact lineage this store protects.
    fn lineage(&self) -> &LogLineage;

    /// Observes the authoritative current snapshot under store ownership.
    fn observe_page(
        &self,
        page_number: PageNumber,
    ) -> Result<Option<StoredPageSnapshotObservation<N>>, Self::ObservationError>;

    /// Atomically rechecks the source and durably replaces it with the target.
    fn compare_and_replace(
        &mut self,
        candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
        permit: CommittedTransactionPageRecoveryWritePermit<'_>,
    ) -> Result<(), Self::WriteError>;
}

/// Owned exact source-store identity retained after a recovery write attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommittedTransactionPageRecoverySourceState<const N: usize> {
    /// The exact page was absent from one store lineage.
    StoreMissing {
        /// Missing page number.
        page_number: PageNumber,
        /// Exact candidate target position whose lineage scopes the absence.
        target_page_position: LogSequenceNumber,
    },
    /// The store contained one exact earlier committed snapshot.
    ExactSnapshot {
        /// Stored page number.
        page_number: PageNumber,
        /// Stored page version.
        page_version: PageVersion,
        /// Exact stored page bytes.
        bytes: [u8; N],
        /// Page WAL position backing the snapshot.
        page_position: LogSequenceNumber,
        /// Matching durable commit position.
        commit_position: LogSequenceNumber,
    },
}

impl<const N: usize> CommittedTransactionPageRecoverySourceState<N> {
    /// Returns the page number whose source state was validated.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        match self {
            Self::StoreMissing { page_number, .. } | Self::ExactSnapshot { page_number, .. } => {
                *page_number
            }
        }
    }
}

/// Owned exact committed target retained after recovery planning or an attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedTransactionPageRecoveryTarget<const N: usize> {
    transaction: DurableTransactionIdentityObservation,
    page_number: PageNumber,
    page_version: PageVersion,
    bytes: [u8; N],
    page_position: LogSequenceNumber,
    commit_position: LogSequenceNumber,
}

impl<const N: usize> CommittedTransactionPageRecoveryTarget<N> {
    /// Returns the persisted owner of the committed target.
    #[must_use]
    pub const fn transaction(&self) -> DurableTransactionIdentityObservation {
        self.transaction
    }

    /// Returns the target page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.page_number
    }

    /// Returns the target page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.page_version
    }

    /// Returns the exact target page bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Returns the exact target page WAL position.
    #[must_use]
    pub const fn page_position(&self) -> &LogSequenceNumber {
        &self.page_position
    }

    /// Returns the exact matching durable commit position.
    #[must_use]
    pub const fn commit_position(&self) -> &LogSequenceNumber {
        &self.commit_position
    }
}

fn owned_recovery_source_state<const N: usize>(
    candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
) -> CommittedTransactionPageRecoverySourceState<N> {
    match candidate.precondition() {
        DurableCommittedTransactionPageRecoveryPrecondition::StoreMissing => {
            let target = candidate.latest_committed().observation();
            CommittedTransactionPageRecoverySourceState::StoreMissing {
                page_number: target.page().page_number(),
                target_page_position: target.position().clone(),
            }
        }
        DurableCommittedTransactionPageRecoveryPrecondition::ExactSnapshot {
            snapshot,
            commit_position,
        } => CommittedTransactionPageRecoverySourceState::ExactSnapshot {
            page_number: snapshot.page_number(),
            page_version: snapshot.page_version(),
            bytes: *snapshot.image().bytes(),
            page_position: snapshot.required_position().clone(),
            commit_position: commit_position.clone(),
        },
    }
}

fn owned_recovery_target<const N: usize>(
    latest: &LatestCommittedTransactionPage<'_, N>,
) -> CommittedTransactionPageRecoveryTarget<N> {
    let observation = latest.observation();
    CommittedTransactionPageRecoveryTarget {
        transaction: observation.owner(),
        page_number: observation.page().page_number(),
        page_version: observation.page().page_version(),
        bytes: *observation.page().image().bytes(),
        page_position: observation.position().clone(),
        commit_position: latest.commit_position().clone(),
    }
}

/// Completed recovery-gate outcome that grants no further write authority.
///
/// ```compile_fail
/// use ntsql_page::DirtyPage;
/// use ntsql_transaction::CommittedTransactionPageRecoveryOutcome;
///
/// fn cannot_create_dirty<const N: usize>(
///     outcome: CommittedTransactionPageRecoveryOutcome<N>,
/// ) -> DirtyPage<N> {
///     outcome.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryOutcome,
///     CommittedTransactionPageRecoveryWritePermit,
/// };
///
/// fn cannot_reuse_outcome<'attempt, const N: usize>(
///     outcome: CommittedTransactionPageRecoveryOutcome<N>,
/// ) -> CommittedTransactionPageRecoveryWritePermit<'attempt> {
///     outcome.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub enum CommittedTransactionPageRecoveryOutcome<const N: usize> {
    /// The stable prefix contains no committed page and no write was attempted.
    NoCommittedPage {
        /// Requested page number.
        page_number: PageNumber,
    },
    /// The store already contains the exact latest committed target.
    AlreadyCurrent {
        /// Exact committed target already present.
        target: CommittedTransactionPageRecoveryTarget<N>,
    },
    /// The store reported the exact committed target durably replaced.
    Recovered {
        /// Exact committed target reported durable.
        target: CommittedTransactionPageRecoveryTarget<N>,
    },
}

impl<const N: usize> CommittedTransactionPageRecoveryOutcome<N> {
    /// Returns the page number resolved by this single-page recovery outcome.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        match self {
            Self::NoCommittedPage { page_number } => *page_number,
            Self::AlreadyCurrent { target } | Self::Recovered { target } => target.page_number(),
        }
    }
}

/// Terminal state after the recovery store method returned an error.
///
/// This value has no retry entrypoint and cannot recreate a permit:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryWritePermit,
///     IndeterminateCommittedTransactionPageRecovery,
/// };
///
/// fn cannot_retry<'attempt, Error, const N: usize>(
///     recovery: IndeterminateCommittedTransactionPageRecovery<Error, N>,
/// ) -> CommittedTransactionPageRecoveryWritePermit<'attempt> {
///     recovery.into()
/// }
/// ```
#[derive(Debug)]
pub struct IndeterminateCommittedTransactionPageRecovery<WriteError, const N: usize> {
    source_state: CommittedTransactionPageRecoverySourceState<N>,
    target: CommittedTransactionPageRecoveryTarget<N>,
    source: WriteError,
}

impl<WriteError, const N: usize> IndeterminateCommittedTransactionPageRecovery<WriteError, N> {
    /// Returns the exact source state used for the attempted replacement.
    #[must_use]
    pub const fn source_state(&self) -> &CommittedTransactionPageRecoverySourceState<N> {
        &self.source_state
    }

    /// Returns the exact committed target used for the attempted replacement.
    #[must_use]
    pub const fn target(&self) -> &CommittedTransactionPageRecoveryTarget<N> {
        &self.target
    }

    /// Returns the exact adapter failure.
    #[must_use]
    pub const fn cause(&self) -> &WriteError {
        &self.source
    }

    /// Returns the exact source state, target, and adapter failure.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        CommittedTransactionPageRecoverySourceState<N>,
        CommittedTransactionPageRecoveryTarget<N>,
        WriteError,
    ) {
        (self.source_state, self.target, self.source)
    }
}

impl<WriteError: fmt::Display, const N: usize> fmt::Display
    for IndeterminateCommittedTransactionPageRecovery<WriteError, N>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "committed recovery write for page {} at position {} failed: {}",
            self.target.page_number().get(),
            self.target.page_position().get(),
            self.source
        )
    }
}

impl<WriteError, const N: usize> Error
    for IndeterminateCommittedTransactionPageRecovery<WriteError, N>
where
    WriteError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Recovery-gate failure before or after the store indeterminacy boundary.
#[derive(Debug)]
pub enum CommittedTransactionPageRecoveryError<
    SourceError,
    ObservationError,
    WriteError,
    const N: usize,
> {
    /// Source and store do not protect one log lineage; neither effectful port
    /// operation was called.
    LineageMismatch {
        /// Authoritative source lineage.
        source_lineage: LogLineage,
        /// Page-store lineage.
        store_lineage: LogLineage,
    },
    /// The authoritative source failed before any store write was attempted.
    Source(SourceError),
    /// The store snapshot could not be observed before any write attempt.
    StoreObservation(ObservationError),
    /// Fresh complete-prefix recovery planning failed before any write attempt.
    Planning {
        /// Exact ADR 0028 planning failure.
        source: Box<DurableCommittedTransactionPageRecoveryPlanningError>,
    },
    /// Candidate self-comparison failed before any write attempt.
    CandidateComparison {
        /// Exact ADR 0028 comparison failure.
        source: Box<DurableCommittedTransactionPageRecoveryComparisonError>,
    },
    /// Candidate self-comparison produced a non-source success unexpectedly.
    UnexpectedCandidateComparison {
        /// Unexpected comparison result.
        actual: DurableCommittedTransactionPageRecoveryComparison,
    },
    /// The store method was invoked and its physical result is indeterminate.
    StoreWrite {
        /// Terminal exact source, target, and adapter failure.
        state: Box<IndeterminateCommittedTransactionPageRecovery<WriteError, N>>,
    },
    /// The source returned the callback's write marker but the gate retained no
    /// corresponding attempt result.
    AttemptResultMissing {
        /// Requested page number.
        page_number: PageNumber,
    },
}

impl<SourceError, ObservationError, WriteError, const N: usize> fmt::Display
    for CommittedTransactionPageRecoveryError<SourceError, ObservationError, WriteError, N>
where
    SourceError: fmt::Display,
    ObservationError: fmt::Display,
    WriteError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineageMismatch { .. } => {
                formatter.write_str("recovery source and page store belong to different lineages")
            }
            Self::Source(source) => write!(formatter, "recovery source failed: {source}"),
            Self::StoreObservation(source) => {
                write!(formatter, "page-store observation failed: {source}")
            }
            Self::Planning { source } => write!(formatter, "recovery planning failed: {source}"),
            Self::CandidateComparison { source } => {
                write!(formatter, "recovery candidate comparison failed: {source}")
            }
            Self::UnexpectedCandidateComparison { actual } => write!(
                formatter,
                "recovery candidate self-comparison unexpectedly returned {actual:?}"
            ),
            Self::StoreWrite { state } => state.fmt(formatter),
            Self::AttemptResultMissing { page_number } => write!(
                formatter,
                "page {} recovery source returned a write marker without an attempt result",
                page_number.get()
            ),
        }
    }
}

impl<SourceError, ObservationError, WriteError, const N: usize> Error
    for CommittedTransactionPageRecoveryError<SourceError, ObservationError, WriteError, N>
where
    SourceError: Error + 'static,
    ObservationError: Error + 'static,
    WriteError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::StoreObservation(source) => Some(source),
            Self::Planning { source } => Some(source.as_ref()),
            Self::CandidateComparison { source } => Some(source.as_ref()),
            Self::StoreWrite { state } => Some(state.as_ref()),
            Self::LineageMismatch { .. }
            | Self::UnexpectedCandidateComparison { .. }
            | Self::AttemptResultMissing { .. } => None,
        }
    }
}

/// Result of one complete committed transaction-page recovery-gate invocation.
pub type CommittedTransactionPageRecoveryResult<
    SourceError,
    ObservationError,
    WriteError,
    const N: usize,
> = Result<
    CommittedTransactionPageRecoveryOutcome<N>,
    CommittedTransactionPageRecoveryError<SourceError, ObservationError, WriteError, N>,
>;

enum RecoveryBeforeWriteError<ObservationError> {
    StoreObservation(ObservationError),
    Planning(Box<DurableCommittedTransactionPageRecoveryPlanningError>),
    CandidateComparison(Box<DurableCommittedTransactionPageRecoveryComparisonError>),
    UnexpectedCandidateComparison(DurableCommittedTransactionPageRecoveryComparison),
}

enum RecoveryCallbackOutcome<ObservationError, const N: usize> {
    Completed(
        Result<
            CommittedTransactionPageRecoveryOutcome<N>,
            RecoveryBeforeWriteError<ObservationError>,
        >,
    ),
    WriteAttempted,
}

enum RecoveryWriteAttempt<WriteError, const N: usize> {
    Succeeded(CommittedTransactionPageRecoveryTarget<N>),
    Failed(IndeterminateCommittedTransactionPageRecovery<WriteError, N>),
}

fn map_recovery_before_write_error<SourceError, ObservationError, WriteError, const N: usize>(
    error: RecoveryBeforeWriteError<ObservationError>,
) -> CommittedTransactionPageRecoveryError<SourceError, ObservationError, WriteError, N> {
    match error {
        RecoveryBeforeWriteError::StoreObservation(source) => {
            CommittedTransactionPageRecoveryError::StoreObservation(source)
        }
        RecoveryBeforeWriteError::Planning(source) => {
            CommittedTransactionPageRecoveryError::Planning { source }
        }
        RecoveryBeforeWriteError::CandidateComparison(source) => {
            CommittedTransactionPageRecoveryError::CandidateComparison { source }
        }
        RecoveryBeforeWriteError::UnexpectedCandidateComparison(actual) => {
            CommittedTransactionPageRecoveryError::UnexpectedCandidateComparison { actual }
        }
    }
}

/// Reconciles one stable durable prefix and atomically attempts exact recovery.
///
/// The source prefix remains exclusively stable for the callback duration. The
/// store observes current state, ADR 0026/0028 planning is rerun, and only an
/// exact candidate source match creates the private one-attempt permit. The
/// adapter must perform its own atomic source recheck under the store lock.
///
/// A write attempt is recorded outside the callback output. If a defective
/// source discards that output and returns an error after the store was invoked,
/// the attempted success or terminal store failure still takes priority.
///
/// Source and store must be distinct objects or disjoint split borrows:
///
/// ```compile_fail
/// use ntsql_page::PageNumber;
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryStore, DurableTransactionPageRecoverySource,
///     recover_committed_transaction_page,
/// };
///
/// fn cannot_alias_source_and_store<Both, const N: usize>(
///     both: &mut Both,
///     page_number: PageNumber,
/// )
/// where
///     Both: DurableTransactionPageRecoverySource<N>
///         + CommittedTransactionPageRecoveryStore<N>,
/// {
///     let _ = recover_committed_transaction_page(both, both, page_number);
/// }
/// ```
pub fn recover_committed_transaction_page<Source, Store, const N: usize>(
    source: &mut Source,
    store: &mut Store,
    page_number: PageNumber,
) -> CommittedTransactionPageRecoveryResult<
    Source::Error,
    Store::ObservationError,
    Store::WriteError,
    N,
>
where
    Source: DurableTransactionPageRecoverySource<N>,
    Store: CommittedTransactionPageRecoveryStore<N>,
{
    let source_lineage = source.lineage().clone();
    if !source_lineage.same_lineage(store.lineage()) {
        return Err(CommittedTransactionPageRecoveryError::LineageMismatch {
            source_lineage,
            store_lineage: store.lineage().clone(),
        });
    }

    let mut write_attempt = None;
    let callback_result = source.with_durable_page_evidence(
        page_number,
        |physical_pages, owned_pages, commit_observations| {
            let snapshot = match store.observe_page(page_number) {
                Ok(snapshot) => snapshot,
                Err(source) => {
                    return RecoveryCallbackOutcome::Completed(Err(
                        RecoveryBeforeWriteError::StoreObservation(source),
                    ));
                }
            };
            let decision = match derive_committed_transaction_page_recovery_candidate(
                &source_lineage,
                page_number,
                snapshot.as_ref(),
                physical_pages,
                owned_pages,
                commit_observations,
            ) {
                Ok(decision) => decision,
                Err(source) => {
                    return RecoveryCallbackOutcome::Completed(Err(
                        RecoveryBeforeWriteError::Planning(Box::new(source)),
                    ));
                }
            };

            match decision {
                DurableCommittedTransactionPageRecoveryDecision::NoCommittedPage {
                    page_number,
                } => RecoveryCallbackOutcome::Completed(Ok(
                    CommittedTransactionPageRecoveryOutcome::NoCommittedPage { page_number },
                )),
                DurableCommittedTransactionPageRecoveryDecision::ExactCurrent {
                    latest_committed,
                } => RecoveryCallbackOutcome::Completed(Ok(
                    CommittedTransactionPageRecoveryOutcome::AlreadyCurrent {
                        target: owned_recovery_target(&latest_committed),
                    },
                )),
                DurableCommittedTransactionPageRecoveryDecision::Candidate(candidate) => {
                    match compare_committed_transaction_page_recovery_candidate(
                        &candidate,
                        snapshot.as_ref(),
                    ) {
                        Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches) => {}
                        Ok(actual) => {
                            return RecoveryCallbackOutcome::Completed(Err(
                                RecoveryBeforeWriteError::UnexpectedCandidateComparison(actual),
                            ));
                        }
                        Err(source) => {
                            return RecoveryCallbackOutcome::Completed(Err(
                                RecoveryBeforeWriteError::CandidateComparison(Box::new(source)),
                            ));
                        }
                    }

                    let source_state = owned_recovery_source_state(&candidate);
                    let target = owned_recovery_target(candidate.latest_committed());
                    let page_position = target.page_position().clone();
                    let commit_position = target.commit_position().clone();
                    let result = with_committed_page_recovery_write_permit(
                        page_position,
                        commit_position,
                        |permit| store.compare_and_replace(&candidate, permit),
                    );
                    write_attempt = Some(match result {
                        Ok(()) => RecoveryWriteAttempt::Succeeded(target),
                        Err(source) => RecoveryWriteAttempt::Failed(
                            IndeterminateCommittedTransactionPageRecovery {
                                source_state,
                                target,
                                source,
                            },
                        ),
                    });
                    RecoveryCallbackOutcome::WriteAttempted
                }
            }
        },
    );

    if let Some(write_attempt) = write_attempt {
        return match write_attempt {
            RecoveryWriteAttempt::Succeeded(target) => {
                Ok(CommittedTransactionPageRecoveryOutcome::Recovered { target })
            }
            RecoveryWriteAttempt::Failed(state) => {
                Err(CommittedTransactionPageRecoveryError::StoreWrite {
                    state: Box::new(state),
                })
            }
        };
    }

    match callback_result {
        Err(source) => Err(CommittedTransactionPageRecoveryError::Source(source)),
        Ok(RecoveryCallbackOutcome::Completed(result)) => result.map_err(
            map_recovery_before_write_error::<
                Source::Error,
                Store::ObservationError,
                Store::WriteError,
                N,
            >,
        ),
        Ok(RecoveryCallbackOutcome::WriteAttempted) => {
            Err(CommittedTransactionPageRecoveryError::AttemptResultMissing { page_number })
        }
    }
}

/// Ordered completed prefix from one deterministic multi-page recovery run.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub struct CommittedTransactionPagesRecoveryOutcome<const N: usize> {
    pages: Vec<CommittedTransactionPageRecoveryOutcome<N>>,
}

impl<const N: usize> CommittedTransactionPagesRecoveryOutcome<N> {
    /// Returns the completed outcomes in strict inventory order.
    pub fn pages(&self) -> &[CommittedTransactionPageRecoveryOutcome<N>] {
        &self.pages
    }

    /// Consumes the batch outcome and returns its ordered page outcomes.
    pub fn into_pages(self) -> Vec<CommittedTransactionPageRecoveryOutcome<N>> {
        self.pages
    }
}

/// Failure before or during deterministic multi-page committed recovery.
#[derive(Debug)]
pub enum CommittedTransactionPagesRecoveryError<
    InventoryError,
    SourceError,
    ObservationError,
    WriteError,
    const N: usize,
> {
    /// Source and store lineages differed before inventory or observation.
    LineageMismatch {
        /// Authoritative source lineage.
        source_lineage: LogLineage,
        /// Page-store lineage.
        store_lineage: LogLineage,
    },
    /// The source could not produce one complete durable owned-page inventory.
    Inventory(InventoryError),
    /// The inventory was duplicated or out of deterministic ascending order.
    InventoryNotStrictlyIncreasing {
        /// Page immediately preceding the violation.
        previous: PageNumber,
        /// Duplicate or descending page at the violation.
        actual: PageNumber,
    },
    /// The complete outcome vector could not reserve capacity before recovery.
    OutcomeCapacityExhausted {
        /// Number of inventoried pages whose outcomes required reservation.
        page_count: usize,
    },
    /// One page failed after the retained completed prefix.
    Page {
        /// Exact outcomes completed before the failing page.
        completed: CommittedTransactionPagesRecoveryOutcome<N>,
        /// Page whose fresh single-page gate failed.
        page_number: PageNumber,
        /// Exact nested single-page gate failure.
        source: CommittedTransactionPageRecoveryError<SourceError, ObservationError, WriteError, N>,
    },
}

impl<InventoryError, SourceError, ObservationError, WriteError, const N: usize> fmt::Display
    for CommittedTransactionPagesRecoveryError<
        InventoryError,
        SourceError,
        ObservationError,
        WriteError,
        N,
    >
where
    InventoryError: fmt::Display,
    SourceError: fmt::Display,
    ObservationError: fmt::Display,
    WriteError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineageMismatch { .. } => formatter
                .write_str("recovery inventory source and page store belong to different lineages"),
            Self::Inventory(source) => write!(formatter, "recovery inventory failed: {source}"),
            Self::InventoryNotStrictlyIncreasing { previous, actual } => write!(
                formatter,
                "recovery inventory page {} is not strictly greater than page {}",
                actual.get(),
                previous.get()
            ),
            Self::OutcomeCapacityExhausted { page_count } => write!(
                formatter,
                "recovery outcome capacity is exhausted for {page_count} inventoried pages"
            ),
            Self::Page {
                page_number,
                source,
                ..
            } => write!(
                formatter,
                "committed recovery failed at inventoried page {}: {source}",
                page_number.get()
            ),
        }
    }
}

impl<InventoryError, SourceError, ObservationError, WriteError, const N: usize> Error
    for CommittedTransactionPagesRecoveryError<
        InventoryError,
        SourceError,
        ObservationError,
        WriteError,
        N,
    >
where
    InventoryError: Error + 'static,
    SourceError: Error + 'static,
    ObservationError: Error + 'static,
    WriteError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inventory(source) => Some(source),
            Self::Page { source, .. } => Some(source),
            Self::LineageMismatch { .. }
            | Self::InventoryNotStrictlyIncreasing { .. }
            | Self::OutcomeCapacityExhausted { .. } => None,
        }
    }
}

/// Result of one deterministic complete-inventory recovery run.
pub type CommittedTransactionPagesRecoveryResult<
    InventoryError,
    SourceError,
    ObservationError,
    WriteError,
    const N: usize,
> = Result<
    CommittedTransactionPagesRecoveryOutcome<N>,
    CommittedTransactionPagesRecoveryError<
        InventoryError,
        SourceError,
        ObservationError,
        WriteError,
        N,
    >,
>;

/// Recovers every transaction-owned page in one deterministic durable inventory.
///
/// The source and store are held mutably for the complete run. Implementations
/// of the inventory and evidence ports must retain the same durable prefix until
/// this function returns. Every mutation remains delegated to
/// [`recover_committed_transaction_page`].
pub fn recover_committed_transaction_pages<Source, Store, const N: usize>(
    source: &mut Source,
    store: &mut Store,
) -> CommittedTransactionPagesRecoveryResult<
    <Source as DurableTransactionPageRecoveryInventory<N>>::Error,
    <Source as DurableTransactionPageRecoverySource<N>>::Error,
    Store::ObservationError,
    Store::WriteError,
    N,
>
where
    Source: DurableTransactionPageRecoveryInventory<N> + DurableTransactionPageRecoverySource<N>,
    Store: CommittedTransactionPageRecoveryStore<N>,
{
    let source_lineage =
        <Source as DurableTransactionPageRecoverySource<N>>::lineage(source).clone();
    if !source_lineage.same_lineage(store.lineage()) {
        return Err(CommittedTransactionPagesRecoveryError::LineageMismatch {
            source_lineage,
            store_lineage: store.lineage().clone(),
        });
    }

    let page_numbers = source
        .durable_transaction_page_numbers()
        .map_err(CommittedTransactionPagesRecoveryError::Inventory)?;
    for pair in page_numbers.windows(2) {
        let previous = pair[0];
        let actual = pair[1];
        if previous >= actual {
            return Err(
                CommittedTransactionPagesRecoveryError::InventoryNotStrictlyIncreasing {
                    previous,
                    actual,
                },
            );
        }
    }

    let mut pages = Vec::new();
    pages.try_reserve(page_numbers.len()).map_err(|_| {
        CommittedTransactionPagesRecoveryError::OutcomeCapacityExhausted {
            page_count: page_numbers.len(),
        }
    })?;

    for page_number in page_numbers {
        match recover_committed_transaction_page(source, store, page_number) {
            Ok(outcome) => pages.push(outcome),
            Err(source) => {
                return Err(CommittedTransactionPagesRecoveryError::Page {
                    completed: CommittedTransactionPagesRecoveryOutcome { pages },
                    page_number,
                    source,
                });
            }
        }
    }

    Ok(CommittedTransactionPagesRecoveryOutcome { pages })
}

/// Owning startup state that has not completed committed-page recovery.
///
/// The source and store remain private until [`Self::recover`] succeeds:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryStore, DurableTransactionPageRecoveryInventory,
///     DurableTransactionPageRecoverySource, UnrecoveredTransactionPageStorage,
/// };
///
/// fn cannot_access_unrecovered<Source, Store, const N: usize>(
///     mut storage: UnrecoveredTransactionPageStorage<Source, Store, N>,
/// )
/// where
///     Source: DurableTransactionPageRecoveryInventory<N>
///         + DurableTransactionPageRecoverySource<N>,
///     Store: CommittedTransactionPageRecoveryStore<N>,
/// {
///     let _ = storage.parts_mut();
/// }
/// ```
#[must_use = "unrecovered storage must be recovered or dropped"]
pub struct UnrecoveredTransactionPageStorage<Source, Store, const N: usize> {
    source: Source,
    store: Store,
    page_width: PhantomData<[u8; N]>,
}

impl<Source, Store, const N: usize> UnrecoveredTransactionPageStorage<Source, Store, N> {
    /// Takes exclusive ownership of one recovery source and page store.
    pub fn new(source: Source, store: Store) -> Self {
        Self {
            source,
            store,
            page_width: PhantomData,
        }
    }
}

impl<Source, Store, const N: usize> UnrecoveredTransactionPageStorage<Source, Store, N>
where
    Source: DurableTransactionPageRecoveryInventory<N> + DurableTransactionPageRecoverySource<N>,
    Store: CommittedTransactionPageRecoveryStore<N>,
{
    /// Runs one complete deterministic batch and releases access only on success.
    pub fn recover(
        mut self,
    ) -> Result<
        RecoveredTransactionPageStorage<Source, Store, N>,
        FailedTransactionPageStorageRecovery<Source, Store, N>,
    > {
        match recover_committed_transaction_pages(&mut self.source, &mut self.store) {
            Ok(report) => Ok(RecoveredTransactionPageStorage {
                storage: self,
                report,
            }),
            Err(error) => Err(FailedTransactionPageStorageRecovery {
                storage: self,
                error,
            }),
        }
    }
}

/// Owning startup failure that permits only inspection, drop, or a fresh retry.
///
/// Neither adapter can escape this state:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryStore, DurableTransactionPageRecoveryInventory,
///     DurableTransactionPageRecoverySource, FailedTransactionPageStorageRecovery,
/// };
///
/// fn cannot_extract_failed<Source, Store, const N: usize>(
///     failure: FailedTransactionPageStorageRecovery<Source, Store, N>,
/// )
/// where
///     Source: DurableTransactionPageRecoveryInventory<N>
///         + DurableTransactionPageRecoverySource<N>,
///     Store: CommittedTransactionPageRecoveryStore<N>,
/// {
///     let _ = failure.into_parts();
/// }
/// ```
#[must_use = "failed recovery must be inspected, retried, or dropped"]
pub struct FailedTransactionPageStorageRecovery<Source, Store, const N: usize>
where
    Source: DurableTransactionPageRecoveryInventory<N> + DurableTransactionPageRecoverySource<N>,
    Store: CommittedTransactionPageRecoveryStore<N>,
{
    storage: UnrecoveredTransactionPageStorage<Source, Store, N>,
    error: CommittedTransactionPagesRecoveryError<
        <Source as DurableTransactionPageRecoveryInventory<N>>::Error,
        <Source as DurableTransactionPageRecoverySource<N>>::Error,
        Store::ObservationError,
        Store::WriteError,
        N,
    >,
}

impl<Source, Store, const N: usize> FailedTransactionPageStorageRecovery<Source, Store, N>
where
    Source: DurableTransactionPageRecoveryInventory<N> + DurableTransactionPageRecoverySource<N>,
    Store: CommittedTransactionPageRecoveryStore<N>,
{
    /// Returns the exact failed batch result without exposing either adapter.
    #[must_use]
    pub const fn error(
        &self,
    ) -> &CommittedTransactionPagesRecoveryError<
        <Source as DurableTransactionPageRecoveryInventory<N>>::Error,
        <Source as DurableTransactionPageRecoverySource<N>>::Error,
        Store::ObservationError,
        Store::WriteError,
        N,
    > {
        &self.error
    }

    /// Consumes the failure and starts a fresh complete batch from inventory.
    pub fn retry(
        self,
    ) -> Result<
        RecoveredTransactionPageStorage<Source, Store, N>,
        FailedTransactionPageStorageRecovery<Source, Store, N>,
    > {
        self.storage.recover()
    }
}

impl<Source, Store, const N: usize> fmt::Debug
    for FailedTransactionPageStorageRecovery<Source, Store, N>
where
    Source: DurableTransactionPageRecoveryInventory<N> + DurableTransactionPageRecoverySource<N>,
    Store: CommittedTransactionPageRecoveryStore<N>,
    CommittedTransactionPagesRecoveryError<
        <Source as DurableTransactionPageRecoveryInventory<N>>::Error,
        <Source as DurableTransactionPageRecoverySource<N>>::Error,
        Store::ObservationError,
        Store::WriteError,
        N,
    >: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FailedTransactionPageStorageRecovery")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<Source, Store, const N: usize> fmt::Display
    for FailedTransactionPageStorageRecovery<Source, Store, N>
where
    Source: DurableTransactionPageRecoveryInventory<N> + DurableTransactionPageRecoverySource<N>,
    Store: CommittedTransactionPageRecoveryStore<N>,
    CommittedTransactionPagesRecoveryError<
        <Source as DurableTransactionPageRecoveryInventory<N>>::Error,
        <Source as DurableTransactionPageRecoverySource<N>>::Error,
        Store::ObservationError,
        Store::WriteError,
        N,
    >: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction-page storage recovery failed: {}",
            self.error
        )
    }
}

impl<Source, Store, const N: usize> Error for FailedTransactionPageStorageRecovery<Source, Store, N>
where
    Source: DurableTransactionPageRecoveryInventory<N> + DurableTransactionPageRecoverySource<N>,
    Store: CommittedTransactionPageRecoveryStore<N>,
    <Source as DurableTransactionPageRecoveryInventory<N>>::Error: Error + 'static,
    <Source as DurableTransactionPageRecoverySource<N>>::Error: Error + 'static,
    Store::ObservationError: Error + 'static,
    Store::WriteError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

/// Owning startup state whose committed-page recovery has completed.
///
/// Its adapters remain private until restart analysis also succeeds:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     RecoveredTransactionPageStorage,
/// };
///
/// fn cannot_inspect_before_analysis<Source, Store, const N: usize>(
///     storage: &RecoveredTransactionPageStorage<Source, Store, N>,
/// ) {
///     let _ = storage.parts();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     RecoveredTransactionPageStorage,
/// };
///
/// fn cannot_release_before_analysis<Source, Store, const N: usize>(
///     mut storage: RecoveredTransactionPageStorage<Source, Store, N>,
/// ) {
///     let _ = storage.parts_mut();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     RecoveredTransactionPageStorage,
/// };
///
/// fn cannot_extract_before_analysis<Source, Store, const N: usize>(
///     storage: RecoveredTransactionPageStorage<Source, Store, N>,
/// ) {
///     let _ = storage.into_parts();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPagesRecoveryOutcome, RecoveredTransactionPageStorage,
///     UnrecoveredTransactionPageStorage,
/// };
///
/// fn cannot_forge_page_recovered<Source, Store, const N: usize>(
///     source: Source,
///     store: Store,
///     report: CommittedTransactionPagesRecoveryOutcome<N>,
/// ) -> RecoveredTransactionPageStorage<Source, Store, N> {
///     RecoveredTransactionPageStorage {
///         storage: UnrecoveredTransactionPageStorage::new(source, store),
///         report,
///     }
/// }
/// ```
#[must_use = "page-recovered storage must be restart-analyzed or dropped"]
pub struct RecoveredTransactionPageStorage<Source, Store, const N: usize> {
    storage: UnrecoveredTransactionPageStorage<Source, Store, N>,
    report: CommittedTransactionPagesRecoveryOutcome<N>,
}

impl<Source, Store, const N: usize> RecoveredTransactionPageStorage<Source, Store, N> {
    /// Returns the exact complete ordered startup recovery report.
    pub const fn recovery_report(&self) -> &CommittedTransactionPagesRecoveryOutcome<N> {
        &self.report
    }
}

impl<Source, Store, const N: usize> RecoveredTransactionPageStorage<Source, Store, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
{
    /// Consumes page-recovered storage and validates its complete restart prefix.
    pub fn analyze_restart(
        mut self,
    ) -> Result<
        RestartAnalyzedTransactionPageStorage<Source, Store, N>,
        FailedTransactionPageStorageRestartAnalysis<Source, Store, N>,
    > {
        match analyze_durable_transaction_restart(&mut self.storage.source) {
            Ok(analysis) => Ok(RestartAnalyzedTransactionPageStorage {
                storage: self,
                analysis,
            }),
            Err(error) => Err(FailedTransactionPageStorageRestartAnalysis {
                storage: self,
                error,
            }),
        }
    }
}

/// Startup storage released only after page recovery and restart analysis.
///
/// Private fields prevent callers from substituting inert analysis metadata:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartAnalysis, RecoveredTransactionPageStorage,
///     RestartAnalyzedTransactionPageStorage,
/// };
///
/// fn cannot_forge_analyzed<Source, Store, const N: usize>(
///     storage: RecoveredTransactionPageStorage<Source, Store, N>,
///     analysis: DurableTransactionRestartAnalysis,
/// ) -> RestartAnalyzedTransactionPageStorage<Source, Store, N> {
///     RestartAnalyzedTransactionPageStorage { storage, analysis }
/// }
/// ```
#[must_use = "restart-analyzed storage owns the live source and page store"]
pub struct RestartAnalyzedTransactionPageStorage<Source, Store, const N: usize> {
    storage: RecoveredTransactionPageStorage<Source, Store, N>,
    analysis: DurableTransactionRestartAnalysis,
}

impl<Source, Store, const N: usize> RestartAnalyzedTransactionPageStorage<Source, Store, N> {
    /// Returns the exact complete ordered committed-page recovery report.
    pub const fn recovery_report(&self) -> &CommittedTransactionPagesRecoveryOutcome<N> {
        &self.storage.report
    }

    /// Returns the exact point-in-time durable restart analysis.
    pub const fn restart_analysis(&self) -> &DurableTransactionRestartAnalysis {
        &self.analysis
    }

    /// Prepares an inert persistable baseline from the exact startup analysis.
    ///
    /// This is not a complete checkpoint: it contains no dirty-page table,
    /// replay start, publication evidence, or log-reclamation authority.
    pub fn prepare_restart_checkpoint_baseline(
        &self,
    ) -> Result<
        DurableTransactionRestartCheckpointBaseline,
        DurableTransactionRestartCheckpointBaselineError,
    > {
        prepare_restart_checkpoint_baseline(&self.analysis)
    }

    /// Borrows the startup-validated source and store for read-only inspection.
    pub const fn parts(&self) -> (&Source, &Store) {
        (&self.storage.storage.source, &self.storage.storage.store)
    }

    /// Borrows the startup-validated source and store for live operations.
    pub const fn parts_mut(&mut self) -> (&mut Source, &mut Store) {
        (
            &mut self.storage.storage.source,
            &mut self.storage.storage.store,
        )
    }

    /// Consumes the validated state and returns adapters plus immutable evidence.
    pub fn into_parts(
        self,
    ) -> (
        Source,
        Store,
        CommittedTransactionPagesRecoveryOutcome<N>,
        DurableTransactionRestartAnalysis,
    ) {
        (
            self.storage.storage.source,
            self.storage.storage.store,
            self.storage.report,
            self.analysis,
        )
    }
}

impl<Source, Store, const N: usize> RestartAnalyzedTransactionPageStorage<Source, Store, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
{
    /// Re-analyzes the current durable WAL prefix and prepares its inert baseline.
    ///
    /// Unlike [`Self::prepare_restart_checkpoint_baseline`], this operation does
    /// not use the immutable startup analysis. It leaves that evidence and the
    /// page store untouched.
    pub fn prepare_restart_checkpoint_baseline_from_current_prefix(
        &mut self,
    ) -> Result<
        DurableTransactionRestartCheckpointBaseline,
        DurableTransactionRestartCheckpointBaselineCurrentPreparationError<Source::Error>,
    > {
        let analysis = analyze_durable_transaction_restart(&mut self.storage.storage.source)
            .map_err(
                DurableTransactionRestartCheckpointBaselineCurrentPreparationError::Analysis,
            )?;
        prepare_restart_checkpoint_baseline(&analysis).map_err(
            DurableTransactionRestartCheckpointBaselineCurrentPreparationError::BaselinePreparation,
        )
    }

    /// Validates decoded baseline fields against their claimed current-WAL prefix.
    ///
    /// This operation re-reads the current durable source. It does not compare
    /// against the immutable startup analysis, inspect the page store, or grant
    /// checkpoint, replay, or log-reclamation authority.
    pub fn validate_restart_checkpoint_baseline_against_current_prefix(
        &mut self,
        observation: &DurableTransactionRestartCheckpointBaselineObservation<'_>,
    ) -> Result<
        DurableTransactionRestartCheckpointBaseline,
        DurableTransactionRestartCheckpointBaselineValidationError<Source::Error>,
    > {
        validate_restart_checkpoint_baseline_against_current_prefix(
            &mut self.storage.storage.source,
            observation,
        )
    }

    /// Loads one owned decoded slot, then validates it against the current WAL.
    ///
    /// Checkpoint retrieval completes before WAL validation begins. No
    /// checkpoint-source borrow is held while the WAL source callback runs.
    pub fn validate_restart_checkpoint_baseline_from_source<CheckpointSource>(
        &mut self,
        checkpoint_source: &mut CheckpointSource,
    ) -> Result<
        Option<DurableTransactionRestartCheckpointBaseline>,
        DurableTransactionRestartCheckpointBaselineSourceValidationError<
            CheckpointSource::Error,
            Source::Error,
        >,
    >
    where
        CheckpointSource: DurableTransactionRestartCheckpointBaselineSource,
    {
        let checkpoint = checkpoint_source
            .load_restart_checkpoint_baseline()
            .map_err(
                DurableTransactionRestartCheckpointBaselineSourceValidationError::CheckpointSource,
            )?;
        let Some(checkpoint) = checkpoint else {
            return Ok(None);
        };
        self.validate_restart_checkpoint_baseline_against_current_prefix(
            &checkpoint.as_observation(),
        )
        .map(Some)
        .map_err(|source| {
            DurableTransactionRestartCheckpointBaselineSourceValidationError::BaselineValidation(
                Box::new(source),
            )
        })
    }
}

/// Fail-closed restart-analysis state that retains the recovered storage pair.
///
/// The unchanged owned prefix provides no meaningful in-place retry, and neither
/// adapter can escape this state:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartAnalysisSource,
///     FailedTransactionPageStorageRestartAnalysis,
/// };
///
/// fn cannot_inspect_failed<Source, Store, const N: usize>(
///     failure: &FailedTransactionPageStorageRestartAnalysis<Source, Store, N>,
/// )
/// where
///     Source: DurableTransactionRestartAnalysisSource<N>,
/// {
///     let _ = failure.parts();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartAnalysisSource,
///     FailedTransactionPageStorageRestartAnalysis,
/// };
///
/// fn cannot_mutate_failed<Source, Store, const N: usize>(
///     mut failure: FailedTransactionPageStorageRestartAnalysis<Source, Store, N>,
/// )
/// where
///     Source: DurableTransactionRestartAnalysisSource<N>,
/// {
///     let _ = failure.parts_mut();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartAnalysisSource,
///     FailedTransactionPageStorageRestartAnalysis,
/// };
///
/// fn cannot_extract_failed<Source, Store, const N: usize>(
///     failure: FailedTransactionPageStorageRestartAnalysis<Source, Store, N>,
/// )
/// where
///     Source: DurableTransactionRestartAnalysisSource<N>,
/// {
///     let _ = failure.into_parts();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartAnalysisSource,
///     FailedTransactionPageStorageRestartAnalysis,
/// };
///
/// fn cannot_retry_unchanged_prefix<Source, Store, const N: usize>(
///     failure: FailedTransactionPageStorageRestartAnalysis<Source, Store, N>,
/// )
/// where
///     Source: DurableTransactionRestartAnalysisSource<N>,
/// {
///     let _ = failure.retry();
/// }
/// ```
#[must_use = "failed restart analysis retains storage until dropped"]
pub struct FailedTransactionPageStorageRestartAnalysis<Source, Store, const N: usize>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
{
    storage: RecoveredTransactionPageStorage<Source, Store, N>,
    error: DurableTransactionRestartAnalysisError<Source::Error>,
}

impl<Source, Store, const N: usize> FailedTransactionPageStorageRestartAnalysis<Source, Store, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
{
    /// Returns the successful committed-page recovery report.
    pub const fn recovery_report(&self) -> &CommittedTransactionPagesRecoveryOutcome<N> {
        &self.storage.report
    }

    /// Returns the exact source or evidence failure that blocked live access.
    pub const fn error(&self) -> &DurableTransactionRestartAnalysisError<Source::Error> {
        &self.error
    }
}

impl<Source, Store, const N: usize> fmt::Debug
    for FailedTransactionPageStorageRestartAnalysis<Source, Store, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Source::Error: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FailedTransactionPageStorageRestartAnalysis")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<Source, Store, const N: usize> fmt::Display
    for FailedTransactionPageStorageRestartAnalysis<Source, Store, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Source::Error: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction-page storage restart analysis failed: {}",
            self.error
        )
    }
}

impl<Source, Store, const N: usize> Error
    for FailedTransactionPageStorageRestartAnalysis<Source, Store, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Source::Error: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

/// Kind of one logical record in a complete durable restart-analysis stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableTransactionRestartObservationKind {
    /// A page record without transaction ownership.
    Page,
    /// A page record owned by one persisted transaction identity.
    TransactionPage,
    /// A durable transaction commit record.
    Commit,
}

/// Adapter-neutral observation of one logical record in a durable WAL prefix.
///
/// The three variants cover every logical record currently exposed by the
/// repository-authored transaction/page WAL. Values remain inert observations:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartObservation, TransactionId,
/// };
///
/// fn cannot_create_transaction<const N: usize>(
///     observation: DurableTransactionRestartObservation<N>,
/// ) -> TransactionId {
///     observation.into()
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub enum DurableTransactionRestartObservation<const N: usize> {
    /// One complete nontransactional page-image record.
    Page(DurablePageWalObservation<N>),
    /// One complete transaction-owned page-image record.
    TransactionPage(DurableTransactionPageObservation<N>),
    /// One complete transaction commit record.
    Commit(DurableTransactionCommitObservation),
}

impl<const N: usize> DurableTransactionRestartObservation<N> {
    /// Returns the logical record kind without exposing mutation authority.
    #[must_use]
    pub const fn kind(&self) -> DurableTransactionRestartObservationKind {
        match self {
            Self::Page(_) => DurableTransactionRestartObservationKind::Page,
            Self::TransactionPage(_) => DurableTransactionRestartObservationKind::TransactionPage,
            Self::Commit(_) => DurableTransactionRestartObservationKind::Commit,
        }
    }

    /// Returns the exact lineage-bound physical position of this record.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        match self {
            Self::Page(observation) => observation.position(),
            Self::TransactionPage(observation) => observation.position(),
            Self::Commit(observation) => observation.position(),
        }
    }
}

/// Stable complete-prefix source for transaction restart analysis.
///
/// Implementations must project every durable logical record exactly once in
/// strict physical order and keep that prefix stable for the callback. `None`
/// means the logical-record prefix is authoritatively empty; a nonempty prefix
/// must supply its exact final position.
///
/// The higher-ranked evidence lifetime prevents the callback input from
/// escaping:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartAnalysisSource,
///     DurableTransactionRestartObservation,
/// };
///
/// fn cannot_escape<'source, Source, const N: usize>(
///     source: &'source mut Source,
/// ) -> Result<&'source [DurableTransactionRestartObservation<N>], Source::Error>
/// where
///     Source: DurableTransactionRestartAnalysisSource<N>,
/// {
///     source.with_durable_transaction_restart_observations(
///         |_frontier, observations| observations,
///     )
/// }
/// ```
pub trait DurableTransactionRestartAnalysisSource<const N: usize> {
    /// Source-specific failure before authoritative evidence is available.
    type Error;

    /// Returns the exact WAL lineage whose stable prefix will be projected.
    fn lineage(&self) -> &LogLineage;

    /// Runs one operation while the complete durable logical prefix is stable.
    fn with_durable_transaction_restart_observations<Output, Operation>(
        &mut self,
        operation: Operation,
    ) -> Result<Output, Self::Error>
    where
        Operation: for<'evidence> FnOnce(
            Option<&'evidence LogSequenceNumber>,
            &'evidence [DurableTransactionRestartObservation<N>],
        ) -> Output;
}

/// Commit classification reconstructed for one persisted transaction identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTransactionRestartState {
    /// No commit for this identity occurred in the complete durable prefix.
    Uncommitted,
    /// Exactly one commit followed every owned-page record for this identity.
    Committed {
        /// Exact durable position of the sole commit record.
        commit_position: LogSequenceNumber,
    },
}

impl DurableTransactionRestartState {
    /// Returns the sole commit position, or `None` for an uncommitted identity.
    #[must_use]
    pub const fn commit_position(&self) -> Option<&LogSequenceNumber> {
        match self {
            Self::Uncommitted => None,
            Self::Committed { commit_position } => Some(commit_position),
        }
    }
}

/// Inert restart metadata for one persisted transaction identity.
///
/// This entry cannot become transaction lifecycle or page-write authority:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransaction, DurableTransactionRestartEntry,
/// };
///
/// fn cannot_authorize_commit(entry: DurableTransactionRestartEntry) -> CommittedTransaction {
///     entry.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryWritePermit, DurableTransactionRestartEntry,
/// };
///
/// fn cannot_authorize_recovery<'attempt>(
///     entry: DurableTransactionRestartEntry,
/// ) -> CommittedTransactionPageRecoveryWritePermit<'attempt> {
///     entry.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::DurableTransactionRestartEntry;
///
/// fn cannot_authorize_page_write<'attempt>(
///     entry: DurableTransactionRestartEntry,
/// ) -> PageWritePermit<'attempt> {
///     entry.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTransactionRestartEntry {
    transaction: DurableTransactionIdentityObservation,
    first_owned_page_position: Option<LogSequenceNumber>,
    last_owned_page_position: Option<LogSequenceNumber>,
    owned_page_record_count: usize,
    state: DurableTransactionRestartState,
}

impl DurableTransactionRestartEntry {
    /// Returns the exact persisted transaction identity.
    #[must_use]
    pub const fn transaction(&self) -> DurableTransactionIdentityObservation {
        self.transaction
    }

    /// Returns the first owned-page position, if this transaction owns a page.
    #[must_use]
    pub const fn first_owned_page_position(&self) -> Option<&LogSequenceNumber> {
        self.first_owned_page_position.as_ref()
    }

    /// Returns the last owned-page position, if this transaction owns a page.
    #[must_use]
    pub const fn last_owned_page_position(&self) -> Option<&LogSequenceNumber> {
        self.last_owned_page_position.as_ref()
    }

    /// Returns the exact number of durable owned-page records for this identity.
    #[must_use]
    pub const fn owned_page_record_count(&self) -> usize {
        self.owned_page_record_count
    }

    /// Returns the reconstructed commit classification.
    #[must_use]
    pub const fn state(&self) -> &DurableTransactionRestartState {
        &self.state
    }
}

/// Owned point-in-time restart analysis for one complete durable WAL prefix.
///
/// The value is metadata only. In particular, it is not a log position and
/// cannot be passed to a durability port as truncation or flush authority:
///
/// ```compile_fail
/// use ntsql_transaction::DurableTransactionRestartAnalysis;
/// use ntsql_wal::LogDurability;
///
/// fn cannot_use_as_log_authority<Log: LogDurability>(
///     log: &mut Log,
///     analysis: &DurableTransactionRestartAnalysis,
/// ) {
///     let _ = log.flush_through(analysis);
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartAnalysis, RecoveredTransactionPageStorage,
/// };
///
/// fn cannot_forge_recovered<Source, Store, const N: usize>(
///     analysis: DurableTransactionRestartAnalysis,
/// ) -> RecoveredTransactionPageStorage<Source, Store, N> {
///     analysis.into()
/// }
/// ```
#[derive(Clone, Debug)]
#[must_use]
pub struct DurableTransactionRestartAnalysis {
    lineage: LogLineage,
    durable_frontier: Option<LogSequenceNumber>,
    transactions: Vec<DurableTransactionRestartEntry>,
}

impl DurableTransactionRestartAnalysis {
    /// Returns the exact lineage analyzed by this point-in-time result.
    #[must_use]
    pub const fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    /// Returns the exact analyzed durable frontier, or `None` for an empty prefix.
    #[must_use]
    pub const fn durable_frontier(&self) -> Option<&LogSequenceNumber> {
        self.durable_frontier.as_ref()
    }

    /// Returns transaction entries in strict persisted-identity order.
    pub fn transactions(&self) -> &[DurableTransactionRestartEntry] {
        &self.transactions
    }

    /// Consumes the analysis and returns its inert metadata.
    pub fn into_parts(
        self,
    ) -> (
        LogLineage,
        Option<LogSequenceNumber>,
        Vec<DurableTransactionRestartEntry>,
    ) {
        (self.lineage, self.durable_frontier, self.transactions)
    }
}

/// Persistable commit classification in one restart checkpoint baseline entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableTransactionRestartCheckpointBaselineState {
    /// No commit for this identity occurred in the analyzed durable prefix.
    Uncommitted,
    /// Exactly one commit followed every owned-page record for this identity.
    Committed {
        /// Numeric durable position of the sole commit record.
        commit_position: u64,
    },
}

impl DurableTransactionRestartCheckpointBaselineState {
    /// Returns the sole numeric commit position, or `None` when uncommitted.
    #[must_use]
    pub const fn commit_position(self) -> Option<u64> {
        match self {
            Self::Uncommitted => None,
            Self::Committed { commit_position } => Some(commit_position),
        }
    }
}

/// Inert persistable transaction-table entry for one analyzed durable prefix.
///
/// An entry cannot reconstruct transaction lifecycle or page-write authority:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaselineEntry, TransactionId,
/// };
///
/// fn cannot_reconstruct_transaction(
///     entry: DurableTransactionRestartCheckpointBaselineEntry,
/// ) -> TransactionId {
///     entry.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaselineEntry;
///
/// fn cannot_authorize_page_write<'attempt>(
///     entry: DurableTransactionRestartCheckpointBaselineEntry,
/// ) -> PageWritePermit<'attempt> {
///     entry.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryWritePermit,
///     DurableTransactionRestartCheckpointBaselineEntry,
/// };
///
/// fn cannot_authorize_recovery<'attempt>(
///     entry: DurableTransactionRestartCheckpointBaselineEntry,
/// ) -> CommittedTransactionPageRecoveryWritePermit<'attempt> {
///     entry.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableTransactionRestartCheckpointBaselineEntry {
    transaction: DurableTransactionIdentityObservation,
    first_owned_page_position: Option<u64>,
    last_owned_page_position: Option<u64>,
    owned_page_record_count: u64,
    state: DurableTransactionRestartCheckpointBaselineState,
}

impl DurableTransactionRestartCheckpointBaselineEntry {
    /// Returns the exact persisted transaction identity.
    #[must_use]
    pub const fn transaction(&self) -> DurableTransactionIdentityObservation {
        self.transaction
    }

    /// Returns the first numeric owned-page position, if one was analyzed.
    #[must_use]
    pub const fn first_owned_page_position(&self) -> Option<u64> {
        self.first_owned_page_position
    }

    /// Returns the last numeric owned-page position, if one was analyzed.
    #[must_use]
    pub const fn last_owned_page_position(&self) -> Option<u64> {
        self.last_owned_page_position
    }

    /// Returns the exact portable owned-page record count.
    #[must_use]
    pub const fn owned_page_record_count(&self) -> u64 {
        self.owned_page_record_count
    }

    /// Returns the persistable commit classification.
    #[must_use]
    pub const fn state(&self) -> DurableTransactionRestartCheckpointBaselineState {
        self.state
    }
}

/// Inert persistable transaction restart baseline for one durable WAL prefix.
///
/// This value is not a complete checkpoint. It has no dirty-page table, redo or
/// undo start, encoded bytes, publication proof, replay command, or retention
/// authority.
///
/// Its private fields prevent direct construction:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaseline,
///     DurableTransactionRestartCheckpointBaselineEntry,
/// };
/// use ntsql_wal::PersistentLogId;
///
/// fn cannot_forge(
///     persistent_log_id: PersistentLogId,
///     transactions: Vec<DurableTransactionRestartCheckpointBaselineEntry>,
/// ) -> DurableTransactionRestartCheckpointBaseline {
///     DurableTransactionRestartCheckpointBaseline {
///         persistent_log_id,
///         durable_frontier: None,
///         transactions,
///     }
/// }
/// ```
///
/// The baseline exposes no runtime lineage capability:
///
/// ```compile_fail
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaseline;
/// use ntsql_wal::LogLineage;
///
/// fn cannot_extract_lineage(
///     baseline: &DurableTransactionRestartCheckpointBaseline,
/// ) -> &LogLineage {
///     baseline.lineage()
/// }
/// ```
///
/// It cannot become a lineage-bound log position:
///
/// ```compile_fail
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaseline;
/// use ntsql_wal::LogSequenceNumber;
///
/// fn cannot_reconstruct_position(
///     baseline: DurableTransactionRestartCheckpointBaseline,
/// ) -> LogSequenceNumber {
///     baseline.into()
/// }
/// ```
///
/// It cannot satisfy a log durability fence:
///
/// ```compile_fail
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaseline;
/// use ntsql_wal::LogDurability;
///
/// fn cannot_flush_checkpoint<Log: LogDurability>(
///     log: &mut Log,
///     baseline: &DurableTransactionRestartCheckpointBaseline,
/// ) {
///     let _ = log.flush_through(baseline);
/// }
/// ```
///
/// It cannot become a recovered storage owner:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaseline, RecoveredTransactionPageStorage,
/// };
///
/// fn cannot_release_storage<Source, Store, const N: usize>(
///     baseline: DurableTransactionRestartCheckpointBaseline,
/// ) -> RecoveredTransactionPageStorage<Source, Store, N> {
///     baseline.into()
/// }
/// ```
///
/// A detached analysis cannot bypass the analyzed storage-owner gate:
///
/// ```compile_fail
/// use ntsql_transaction::DurableTransactionRestartAnalysis;
///
/// fn cannot_prepare_from_detached_analysis(
///     analysis: &DurableTransactionRestartAnalysis,
/// ) {
///     let _ = analysis.prepare_restart_checkpoint_baseline();
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub struct DurableTransactionRestartCheckpointBaseline {
    persistent_log_id: PersistentLogId,
    durable_frontier: Option<u64>,
    transactions: Vec<DurableTransactionRestartCheckpointBaselineEntry>,
}

impl DurableTransactionRestartCheckpointBaseline {
    /// Returns the exact adapter-owned persistent log identity.
    #[must_use]
    pub const fn persistent_log_id(&self) -> PersistentLogId {
        self.persistent_log_id
    }

    /// Returns the numeric durable frontier, or `None` for zero durable records.
    #[must_use]
    pub const fn durable_frontier(&self) -> Option<u64> {
        self.durable_frontier
    }

    /// Returns entries in strict persisted-identity order.
    pub fn transactions(&self) -> &[DurableTransactionRestartCheckpointBaselineEntry] {
        &self.transactions
    }
}

/// Untrusted decoded commit classification for one baseline entry.
///
/// This observation remains distinct from the authoritative baseline state:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaselineState,
///     DurableTransactionRestartCheckpointBaselineStateObservation,
/// };
///
/// fn cannot_authorize_state(
///     observation: DurableTransactionRestartCheckpointBaselineStateObservation,
/// ) -> DurableTransactionRestartCheckpointBaselineState {
///     observation.into()
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableTransactionRestartCheckpointBaselineStateObservation {
    /// Decoded bytes claim no commit in the summarized prefix.
    Uncommitted,
    /// Decoded bytes claim exactly one commit at this raw numeric position.
    Committed {
        /// Unvalidated decoded commit position.
        commit_position: u64,
    },
}

impl DurableTransactionRestartCheckpointBaselineStateObservation {
    /// Returns the decoded commit position, or `None` for uncommitted bytes.
    #[must_use]
    pub const fn commit_position(self) -> Option<u64> {
        match self {
            Self::Uncommitted => None,
            Self::Committed { commit_position } => Some(commit_position),
        }
    }
}

/// Untrusted decoded fields for one restart checkpoint baseline entry.
///
/// Construction deliberately retains zero and contradictory fields so exact
/// validation can report them without granting transaction or page authority:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaselineEntry,
///     DurableTransactionRestartCheckpointBaselineEntryObservation,
/// };
///
/// fn cannot_authorize_entry(
///     observation: DurableTransactionRestartCheckpointBaselineEntryObservation,
/// ) -> DurableTransactionRestartCheckpointBaselineEntry {
///     observation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaselineEntryObservation;
///
/// fn cannot_authorize_page_write<'attempt>(
///     observation: DurableTransactionRestartCheckpointBaselineEntryObservation,
/// ) -> PageWritePermit<'attempt> {
///     observation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaselineEntryObservation, TransactionId,
/// };
///
/// fn cannot_reconstruct_transaction(
///     observation: DurableTransactionRestartCheckpointBaselineEntryObservation,
/// ) -> TransactionId {
///     observation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     CommittedTransactionPageRecoveryWritePermit,
///     DurableTransactionRestartCheckpointBaselineEntryObservation,
/// };
///
/// fn cannot_authorize_recovery<'attempt>(
///     observation: DurableTransactionRestartCheckpointBaselineEntryObservation,
/// ) -> CommittedTransactionPageRecoveryWritePermit<'attempt> {
///     observation.into()
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableTransactionRestartCheckpointBaselineEntryObservation {
    epoch: u64,
    sequence: u64,
    first_owned_page_position: Option<u64>,
    last_owned_page_position: Option<u64>,
    owned_page_record_count: u64,
    state: DurableTransactionRestartCheckpointBaselineStateObservation,
}

impl DurableTransactionRestartCheckpointBaselineEntryObservation {
    /// Retains one complete set of decoded fields without validation.
    #[must_use]
    pub const fn new(
        epoch: u64,
        sequence: u64,
        first_owned_page_position: Option<u64>,
        last_owned_page_position: Option<u64>,
        owned_page_record_count: u64,
        state: DurableTransactionRestartCheckpointBaselineStateObservation,
    ) -> Self {
        Self {
            epoch,
            sequence,
            first_owned_page_position,
            last_owned_page_position,
            owned_page_record_count,
            state,
        }
    }

    /// Returns the raw decoded coordinator epoch.
    #[must_use]
    pub const fn epoch(self) -> u64 {
        self.epoch
    }

    /// Returns the raw decoded coordinator-local sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// Returns the raw decoded first owned-page position.
    #[must_use]
    pub const fn first_owned_page_position(self) -> Option<u64> {
        self.first_owned_page_position
    }

    /// Returns the raw decoded last owned-page position.
    #[must_use]
    pub const fn last_owned_page_position(self) -> Option<u64> {
        self.last_owned_page_position
    }

    /// Returns the raw decoded portable record count.
    #[must_use]
    pub const fn owned_page_record_count(self) -> u64 {
        self.owned_page_record_count
    }

    /// Returns the raw decoded commit classification.
    #[must_use]
    pub const fn state(self) -> DurableTransactionRestartCheckpointBaselineStateObservation {
        self.state
    }
}

/// Borrowed untrusted fields decoded from checkpoint storage.
///
/// This observation cannot become an authoritative baseline or storage owner:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaseline,
///     DurableTransactionRestartCheckpointBaselineObservation,
/// };
///
/// fn cannot_authorize_baseline(
///     observation: DurableTransactionRestartCheckpointBaselineObservation<'_>,
/// ) -> DurableTransactionRestartCheckpointBaseline {
///     observation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaselineObservation,
///     RecoveredTransactionPageStorage,
/// };
///
/// fn cannot_release_storage<Source, Store, const N: usize>(
///     observation: DurableTransactionRestartCheckpointBaselineObservation<'_>,
/// ) -> RecoveredTransactionPageStorage<Source, Store, N> {
///     observation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaselineObservation;
/// use ntsql_wal::LogSequenceNumber;
///
/// fn cannot_reconstruct_position(
///     observation: DurableTransactionRestartCheckpointBaselineObservation<'_>,
/// ) -> LogSequenceNumber {
///     observation.into()
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableTransactionRestartCheckpointBaselineObservation<'evidence> {
    persistent_log_id: u128,
    durable_frontier: Option<u64>,
    transactions: &'evidence [DurableTransactionRestartCheckpointBaselineEntryObservation],
}

impl<'evidence> DurableTransactionRestartCheckpointBaselineObservation<'evidence> {
    /// Retains decoded baseline fields without validation or allocation.
    #[must_use]
    pub const fn new(
        persistent_log_id: u128,
        durable_frontier: Option<u64>,
        transactions: &'evidence [DurableTransactionRestartCheckpointBaselineEntryObservation],
    ) -> Self {
        Self {
            persistent_log_id,
            durable_frontier,
            transactions,
        }
    }

    /// Returns the raw decoded persistent log identity.
    #[must_use]
    pub const fn persistent_log_id(&self) -> u128 {
        self.persistent_log_id
    }

    /// Returns the raw decoded optional durable frontier.
    #[must_use]
    pub const fn durable_frontier(&self) -> Option<u64> {
        self.durable_frontier
    }

    /// Returns decoded entries in their persisted order.
    pub const fn transactions(
        &self,
    ) -> &'evidence [DurableTransactionRestartCheckpointBaselineEntryObservation] {
        self.transactions
    }
}

/// Owned untrusted checkpoint fields returned after one source read completes.
///
/// This value owns decoded fields only and cannot become an authoritative
/// baseline, log position, or recovered storage owner:
///
/// ```compile_fail
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaseline,
///     OwnedDurableTransactionRestartCheckpointBaselineObservation,
/// };
///
/// fn cannot_authorize_baseline(
///     observation: OwnedDurableTransactionRestartCheckpointBaselineObservation,
/// ) -> DurableTransactionRestartCheckpointBaseline {
///     observation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::{
///     OwnedDurableTransactionRestartCheckpointBaselineObservation,
///     RecoveredTransactionPageStorage,
/// };
///
/// fn cannot_release_storage<Source, Store, const N: usize>(
///     observation: OwnedDurableTransactionRestartCheckpointBaselineObservation,
/// ) -> RecoveredTransactionPageStorage<Source, Store, N> {
///     observation.into()
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::OwnedDurableTransactionRestartCheckpointBaselineObservation;
/// use ntsql_wal::LogSequenceNumber;
///
/// fn cannot_reconstruct_position(
///     observation: OwnedDurableTransactionRestartCheckpointBaselineObservation,
/// ) -> LogSequenceNumber {
///     observation.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedDurableTransactionRestartCheckpointBaselineObservation {
    persistent_log_id: u128,
    durable_frontier: Option<u64>,
    transactions: Vec<DurableTransactionRestartCheckpointBaselineEntryObservation>,
}

impl OwnedDurableTransactionRestartCheckpointBaselineObservation {
    /// Retains one owned set of decoded fields without validation.
    #[must_use]
    pub const fn new(
        persistent_log_id: u128,
        durable_frontier: Option<u64>,
        transactions: Vec<DurableTransactionRestartCheckpointBaselineEntryObservation>,
    ) -> Self {
        Self {
            persistent_log_id,
            durable_frontier,
            transactions,
        }
    }

    /// Returns the raw decoded persistent log identity.
    #[must_use]
    pub const fn persistent_log_id(&self) -> u128 {
        self.persistent_log_id
    }

    /// Returns the raw decoded optional durable frontier.
    #[must_use]
    pub const fn durable_frontier(&self) -> Option<u64> {
        self.durable_frontier
    }

    /// Returns owned decoded entries in their persisted order.
    pub fn transactions(&self) -> &[DurableTransactionRestartCheckpointBaselineEntryObservation] {
        &self.transactions
    }

    /// Borrows this owned snapshot through the ADR 0039 validation shape.
    #[must_use]
    pub fn as_observation(&self) -> DurableTransactionRestartCheckpointBaselineObservation<'_> {
        DurableTransactionRestartCheckpointBaselineObservation::new(
            self.persistent_log_id,
            self.durable_frontier,
            &self.transactions,
        )
    }
}

/// Source of one optional owned decoded restart checkpoint baseline slot.
///
/// Retrieval must complete before the returned value is validated against a
/// WAL source. The slot remains untrusted data and grants no storage authority:
///
/// ```compile_fail
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaselineSource;
/// use ntsql_wal::LogDurability;
///
/// fn cannot_use_source_as_log<
///     Source: DurableTransactionRestartCheckpointBaselineSource,
///     Log: LogDurability,
/// >(
///     source: &mut Source,
///     log: &mut Log,
/// ) {
///     let _ = log.flush_through(source);
/// }
/// ```
pub trait DurableTransactionRestartCheckpointBaselineSource {
    /// Source-specific failure to obtain an absent or complete owned slot.
    type Error;

    /// Loads the single current slot, or `None` when no checkpoint is present.
    ///
    /// An error returns no candidate. Multi-generation selection, persistence,
    /// and source locking remain adapter responsibilities outside this port.
    fn load_restart_checkpoint_baseline(
        &mut self,
    ) -> Result<Option<OwnedDurableTransactionRestartCheckpointBaselineObservation>, Self::Error>;
}

/// Failure to prepare a persistable transaction restart checkpoint baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTransactionRestartCheckpointBaselineError {
    /// The analyzed WAL lineage has no stable adapter-owned identity.
    PersistentLineageRequired,
    /// The exact transaction table could not reserve its required capacity.
    TransactionCapacityExhausted {
        /// Number of transaction entries in the analyzed table.
        transaction_count: usize,
    },
    /// A platform-width analyzed count could not fit the portable `u64` field.
    ///
    /// This is defensive on supported targets, whose `usize` is at most 64 bits.
    OwnedPageCountWidthExceeded {
        /// Identity whose exact count could not be represented.
        transaction: DurableTransactionIdentityObservation,
        /// Rejected platform-width count.
        record_count: usize,
    },
}

impl fmt::Display for DurableTransactionRestartCheckpointBaselineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PersistentLineageRequired => {
                formatter.write_str("restart checkpoint baseline requires a persistent log lineage")
            }
            Self::TransactionCapacityExhausted { transaction_count } => write!(
                formatter,
                "restart checkpoint baseline transaction capacity is exhausted for {transaction_count} entries"
            ),
            Self::OwnedPageCountWidthExceeded {
                transaction,
                record_count,
            } => write!(
                formatter,
                "transaction {transaction} owned-page count {record_count} exceeds the portable checkpoint width"
            ),
        }
    }
}

impl Error for DurableTransactionRestartCheckpointBaselineError {}

fn reserve_restart_checkpoint_baseline_transactions(
    transactions: &mut Vec<DurableTransactionRestartCheckpointBaselineEntry>,
    transaction_count: usize,
) -> Result<(), DurableTransactionRestartCheckpointBaselineError> {
    transactions
        .try_reserve_exact(transaction_count)
        .map_err(|_| {
            DurableTransactionRestartCheckpointBaselineError::TransactionCapacityExhausted {
                transaction_count,
            }
        })
}

fn prepare_restart_checkpoint_baseline(
    analysis: &DurableTransactionRestartAnalysis,
) -> Result<
    DurableTransactionRestartCheckpointBaseline,
    DurableTransactionRestartCheckpointBaselineError,
> {
    let persistent_log_id = analysis
        .lineage()
        .persistent_id()
        .ok_or(DurableTransactionRestartCheckpointBaselineError::PersistentLineageRequired)?;
    let transaction_count = analysis.transactions().len();
    let mut transactions = Vec::new();
    reserve_restart_checkpoint_baseline_transactions(&mut transactions, transaction_count)?;

    for entry in analysis.transactions() {
        let transaction = entry.transaction();
        let owned_page_record_count =
            u64::try_from(entry.owned_page_record_count()).map_err(|_| {
                DurableTransactionRestartCheckpointBaselineError::OwnedPageCountWidthExceeded {
                    transaction,
                    record_count: entry.owned_page_record_count(),
                }
            })?;
        let state = match entry.state() {
            DurableTransactionRestartState::Uncommitted => {
                DurableTransactionRestartCheckpointBaselineState::Uncommitted
            }
            DurableTransactionRestartState::Committed { commit_position } => {
                DurableTransactionRestartCheckpointBaselineState::Committed {
                    commit_position: commit_position.get(),
                }
            }
        };
        transactions.push(DurableTransactionRestartCheckpointBaselineEntry {
            transaction,
            first_owned_page_position: entry
                .first_owned_page_position()
                .map(LogSequenceNumber::get),
            last_owned_page_position: entry.last_owned_page_position().map(LogSequenceNumber::get),
            owned_page_record_count,
            state,
        });
    }

    Ok(DurableTransactionRestartCheckpointBaseline {
        persistent_log_id,
        durable_frontier: analysis.durable_frontier().map(LogSequenceNumber::get),
        transactions,
    })
}

/// Invalid decoded baseline evidence or current-prefix relationship.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTransactionRestartCheckpointBaselineValidationEvidenceError {
    /// The current WAL lineage cannot be reconstructed after process restart.
    CurrentPersistentLineageRequired,
    /// Decoded bytes supplied the reserved zero persistent log identity.
    ZeroPersistentLogId {
        /// Rejected raw decoded identity.
        persistent_log_id: u128,
    },
    /// Decoded bytes name another persistent WAL lineage.
    ForeignPersistentLogId {
        /// Exact persistent identity of the current source.
        expected: PersistentLogId,
        /// Rejected decoded persistent identity.
        actual: PersistentLogId,
    },
    /// A decoded nonempty frontier used the reserved zero numeric position.
    ZeroCheckpointFrontier {
        /// Rejected decoded frontier.
        checkpoint_frontier: u64,
    },
    /// Current source frontier or complete-stream evidence was invalid.
    CurrentPrefix(Box<DurableTransactionRestartAnalysisEvidenceError>),
    /// The checkpoint claims a position beyond the current durable frontier.
    CheckpointBeyondDurableFrontier {
        /// Rejected decoded checkpoint frontier.
        checkpoint_frontier: u64,
        /// Current numeric durable frontier, or `None` for an empty source.
        durable_frontier: Option<u64>,
    },
    /// The checkpoint position lies in a valid numeric gap between WAL records.
    CheckpointFrontierNotRecordBoundary {
        /// Rejected decoded checkpoint frontier.
        checkpoint_frontier: u64,
        /// Current numeric durable frontier.
        durable_frontier: u64,
    },
    /// The selected claimed prefix itself failed restart analysis.
    SelectedPrefix(Box<DurableTransactionRestartAnalysisEvidenceError>),
    /// The authoritative selected-prefix baseline could not be prepared.
    ExpectedBaselinePreparation(DurableTransactionRestartCheckpointBaselineError),
    /// The decoded and authoritative frontier shapes or values differ.
    DurableFrontierMismatch {
        /// Frontier recomputed from authoritative WAL evidence.
        expected: Option<u64>,
        /// Raw decoded checkpoint frontier.
        actual: Option<u64>,
    },
    /// The decoded transaction table has a different length.
    TransactionCountMismatch {
        /// Number of entries recomputed from authoritative WAL evidence.
        expected: usize,
        /// Number of decoded entries.
        actual: usize,
    },
    /// One decoded entry differs from the authoritative entry at the same index.
    TransactionEntryMismatch {
        /// Zero-based identity-sorted entry index.
        index: usize,
        /// Entry recomputed from authoritative WAL evidence.
        expected: Box<DurableTransactionRestartCheckpointBaselineEntry>,
        /// Exact untrusted decoded entry.
        actual: DurableTransactionRestartCheckpointBaselineEntryObservation,
    },
}

impl fmt::Display for DurableTransactionRestartCheckpointBaselineValidationEvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentPersistentLineageRequired => formatter.write_str(
                "current restart checkpoint validation source requires a persistent log lineage",
            ),
            Self::ZeroPersistentLogId { .. } => {
                formatter.write_str("decoded restart checkpoint persistent log identity is zero")
            }
            Self::ForeignPersistentLogId { expected, actual } => write!(
                formatter,
                "decoded restart checkpoint log identity {} does not match current identity {}",
                actual.get(),
                expected.get()
            ),
            Self::ZeroCheckpointFrontier { .. } => {
                formatter.write_str("decoded nonempty restart checkpoint frontier is zero")
            }
            Self::CurrentPrefix(source) => {
                write!(
                    formatter,
                    "current durable restart prefix is invalid: {source}"
                )
            }
            Self::CheckpointBeyondDurableFrontier {
                checkpoint_frontier,
                durable_frontier: Some(durable_frontier),
            } => write!(
                formatter,
                "decoded restart checkpoint frontier {checkpoint_frontier} exceeds current durable frontier {durable_frontier}"
            ),
            Self::CheckpointBeyondDurableFrontier {
                checkpoint_frontier,
                durable_frontier: None,
            } => write!(
                formatter,
                "decoded restart checkpoint frontier {checkpoint_frontier} exceeds the empty current durable prefix"
            ),
            Self::CheckpointFrontierNotRecordBoundary {
                checkpoint_frontier,
                durable_frontier,
            } => write!(
                formatter,
                "decoded restart checkpoint frontier {checkpoint_frontier} is not a logical record boundary through durable frontier {durable_frontier}"
            ),
            Self::SelectedPrefix(source) => {
                write!(
                    formatter,
                    "selected restart checkpoint prefix is invalid: {source}"
                )
            }
            Self::ExpectedBaselinePreparation(source) => write!(
                formatter,
                "authoritative restart checkpoint baseline preparation failed: {source}"
            ),
            Self::DurableFrontierMismatch { expected, actual } => write!(
                formatter,
                "decoded restart checkpoint frontier {actual:?} does not match authoritative frontier {expected:?}"
            ),
            Self::TransactionCountMismatch { expected, actual } => write!(
                formatter,
                "decoded restart checkpoint has {actual} transaction entries, expected {expected}"
            ),
            Self::TransactionEntryMismatch {
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "decoded restart checkpoint transaction entry {index} ({:?}:{:?}) does not match authoritative identity {}",
                actual.epoch(),
                actual.sequence(),
                expected.transaction()
            ),
        }
    }
}

impl Error for DurableTransactionRestartCheckpointBaselineValidationEvidenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentPrefix(source) | Self::SelectedPrefix(source) => Some(source.as_ref()),
            Self::ExpectedBaselinePreparation(source) => Some(source),
            Self::CurrentPersistentLineageRequired
            | Self::ZeroPersistentLogId { .. }
            | Self::ForeignPersistentLogId { .. }
            | Self::ZeroCheckpointFrontier { .. }
            | Self::CheckpointBeyondDurableFrontier { .. }
            | Self::CheckpointFrontierNotRecordBoundary { .. }
            | Self::DurableFrontierMismatch { .. }
            | Self::TransactionCountMismatch { .. }
            | Self::TransactionEntryMismatch { .. } => None,
        }
    }
}

/// Failure to read the current prefix or validate decoded checkpoint evidence.
#[derive(Debug)]
pub enum DurableTransactionRestartCheckpointBaselineValidationError<SourceError> {
    /// The current durable-prefix source failed authoritatively.
    Source(SourceError),
    /// Decoded or source-relative checkpoint evidence was invalid.
    Evidence(Box<DurableTransactionRestartCheckpointBaselineValidationEvidenceError>),
}

impl<SourceError: fmt::Display> fmt::Display
    for DurableTransactionRestartCheckpointBaselineValidationError<SourceError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => {
                write!(
                    formatter,
                    "restart checkpoint validation source failed: {source}"
                )
            }
            Self::Evidence(source) => {
                write!(formatter, "restart checkpoint validation failed: {source}")
            }
        }
    }
}

impl<SourceError> Error for DurableTransactionRestartCheckpointBaselineValidationError<SourceError>
where
    SourceError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Evidence(source) => Some(source.as_ref()),
        }
    }
}

/// Failure to analyze and prepare the current durable restart prefix.
#[derive(Debug)]
pub enum DurableTransactionRestartCheckpointBaselineCurrentPreparationError<SourceError> {
    /// Obtaining or analyzing the current durable prefix failed.
    Analysis(DurableTransactionRestartAnalysisError<SourceError>),
    /// The current analysis could not form a persistable baseline.
    BaselinePreparation(DurableTransactionRestartCheckpointBaselineError),
}

impl<SourceError: fmt::Display> fmt::Display
    for DurableTransactionRestartCheckpointBaselineCurrentPreparationError<SourceError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Analysis(source) => {
                write!(
                    formatter,
                    "current restart checkpoint analysis failed: {source}"
                )
            }
            Self::BaselinePreparation(source) => {
                write!(
                    formatter,
                    "current restart checkpoint preparation failed: {source}"
                )
            }
        }
    }
}

impl<SourceError> Error
    for DurableTransactionRestartCheckpointBaselineCurrentPreparationError<SourceError>
where
    SourceError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Analysis(source) => Some(source),
            Self::BaselinePreparation(source) => Some(source),
        }
    }
}

/// Failure to load an owned checkpoint slot or validate it against the WAL.
#[derive(Debug)]
pub enum DurableTransactionRestartCheckpointBaselineSourceValidationError<
    CheckpointSourceError,
    WalSourceError,
> {
    /// Loading the optional owned checkpoint slot failed.
    CheckpointSource(CheckpointSourceError),
    /// A present decoded slot failed current-prefix validation.
    BaselineValidation(
        Box<DurableTransactionRestartCheckpointBaselineValidationError<WalSourceError>>,
    ),
}

impl<CheckpointSourceError, WalSourceError> fmt::Display
    for DurableTransactionRestartCheckpointBaselineSourceValidationError<
        CheckpointSourceError,
        WalSourceError,
    >
where
    CheckpointSourceError: fmt::Display,
    WalSourceError: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CheckpointSource(source) => {
                write!(formatter, "restart checkpoint source failed: {source}")
            }
            Self::BaselineValidation(source) => {
                write!(
                    formatter,
                    "loaded restart checkpoint validation failed: {source}"
                )
            }
        }
    }
}

impl<CheckpointSourceError, WalSourceError> Error
    for DurableTransactionRestartCheckpointBaselineSourceValidationError<
        CheckpointSourceError,
        WalSourceError,
    >
where
    CheckpointSourceError: Error + 'static,
    WalSourceError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CheckpointSource(source) => Some(source),
            Self::BaselineValidation(source) => Some(source.as_ref()),
        }
    }
}

fn restart_checkpoint_entry_matches_observation(
    expected: &DurableTransactionRestartCheckpointBaselineEntry,
    actual: DurableTransactionRestartCheckpointBaselineEntryObservation,
) -> bool {
    let expected_state_matches = match (expected.state(), actual.state()) {
        (
            DurableTransactionRestartCheckpointBaselineState::Uncommitted,
            DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted,
        ) => true,
        (
            DurableTransactionRestartCheckpointBaselineState::Committed {
                commit_position: expected,
            },
            DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                commit_position: actual,
            },
        ) => expected == actual,
        (
            DurableTransactionRestartCheckpointBaselineState::Uncommitted,
            DurableTransactionRestartCheckpointBaselineStateObservation::Committed { .. },
        )
        | (
            DurableTransactionRestartCheckpointBaselineState::Committed { .. },
            DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted,
        ) => false,
    };
    let expected_transaction = expected.transaction();
    expected_transaction.epoch() == actual.epoch()
        && expected_transaction.sequence() == actual.sequence()
        && expected.first_owned_page_position() == actual.first_owned_page_position()
        && expected.last_owned_page_position() == actual.last_owned_page_position()
        && expected.owned_page_record_count() == actual.owned_page_record_count()
        && expected_state_matches
}

fn compare_restart_checkpoint_baseline_observation(
    expected: DurableTransactionRestartCheckpointBaseline,
    actual: &DurableTransactionRestartCheckpointBaselineObservation<'_>,
) -> Result<
    DurableTransactionRestartCheckpointBaseline,
    DurableTransactionRestartCheckpointBaselineValidationEvidenceError,
> {
    if expected.durable_frontier() != actual.durable_frontier() {
        return Err(
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::DurableFrontierMismatch {
                expected: expected.durable_frontier(),
                actual: actual.durable_frontier(),
            },
        );
    }
    if expected.transactions().len() != actual.transactions().len() {
        return Err(
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::TransactionCountMismatch {
                expected: expected.transactions().len(),
                actual: actual.transactions().len(),
            },
        );
    }
    for (index, (expected_entry, actual_entry)) in expected
        .transactions()
        .iter()
        .zip(actual.transactions())
        .enumerate()
    {
        if !restart_checkpoint_entry_matches_observation(expected_entry, *actual_entry) {
            return Err(
                DurableTransactionRestartCheckpointBaselineValidationEvidenceError::TransactionEntryMismatch {
                    index,
                    expected: Box::new(expected_entry.clone()),
                    actual: *actual_entry,
                },
            );
        }
    }
    Ok(expected)
}

fn current_restart_prefix_error(
    source: DurableTransactionRestartAnalysisEvidenceError,
) -> DurableTransactionRestartCheckpointBaselineValidationEvidenceError {
    DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CurrentPrefix(Box::new(
        source,
    ))
}

fn validate_restart_checkpoint_baseline_evidence<const N: usize>(
    lineage: &LogLineage,
    current_frontier: Option<&LogSequenceNumber>,
    observations: &[DurableTransactionRestartObservation<N>],
    checkpoint: &DurableTransactionRestartCheckpointBaselineObservation<'_>,
) -> Result<
    DurableTransactionRestartCheckpointBaseline,
    DurableTransactionRestartCheckpointBaselineValidationEvidenceError,
> {
    let Some(checkpoint_frontier) = checkpoint.durable_frontier() else {
        let empty_analysis = analyze_durable_transaction_restart_evidence::<N>(lineage, None, &[])
            .map_err(|source| {
                DurableTransactionRestartCheckpointBaselineValidationEvidenceError::SelectedPrefix(
                    Box::new(source),
                )
            })?;
        let expected = prepare_restart_checkpoint_baseline(&empty_analysis).map_err(
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ExpectedBaselinePreparation,
        )?;
        return compare_restart_checkpoint_baseline_observation(expected, checkpoint);
    };

    let current_frontier_value = match current_frontier {
        None if observations.is_empty() => {
            return Err(
                DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CheckpointBeyondDurableFrontier {
                    checkpoint_frontier,
                    durable_frontier: None,
                },
            );
        }
        None => {
            let source = analyze_durable_transaction_restart_evidence(lineage, None, observations)
                .err()
                .ok_or(
                    DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CheckpointBeyondDurableFrontier {
                        checkpoint_frontier,
                        durable_frontier: None,
                    },
                )?;
            return Err(current_restart_prefix_error(source));
        }
        Some(frontier) if !lineage.same_lineage(frontier.lineage()) => {
            return Err(current_restart_prefix_error(
                DurableTransactionRestartAnalysisEvidenceError::ForeignFrontier {
                    frontier: frontier.clone(),
                },
            ));
        }
        Some(frontier) if frontier.get() == 0 => {
            return Err(current_restart_prefix_error(
                DurableTransactionRestartAnalysisEvidenceError::ZeroFrontier {
                    frontier: frontier.clone(),
                },
            ));
        }
        Some(frontier) if observations.is_empty() => {
            return Err(current_restart_prefix_error(
                DurableTransactionRestartAnalysisEvidenceError::FrontierWithoutRecords {
                    frontier: frontier.clone(),
                },
            ));
        }
        Some(frontier) => frontier.get(),
    };

    if checkpoint_frontier > current_frontier_value {
        return Err(
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CheckpointBeyondDurableFrontier {
                checkpoint_frontier,
                durable_frontier: Some(current_frontier_value),
            },
        );
    }

    let Some(boundary_index) = observations
        .iter()
        .position(|observation| observation.position().get() == checkpoint_frontier)
    else {
        match analyze_durable_transaction_restart_evidence(lineage, current_frontier, observations)
        {
            Ok(_) => {
                return Err(
                    DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CheckpointFrontierNotRecordBoundary {
                        checkpoint_frontier,
                        durable_frontier: current_frontier_value,
                    },
                );
            }
            Err(source) => return Err(current_restart_prefix_error(source)),
        }
    };
    let selected_frontier = lineage.position(checkpoint_frontier);
    let selected = &observations[..=boundary_index];
    let analysis =
        analyze_durable_transaction_restart_evidence(lineage, Some(&selected_frontier), selected)
            .map_err(|source| {
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::SelectedPrefix(
                Box::new(source),
            )
        })?;
    let expected = prepare_restart_checkpoint_baseline(&analysis).map_err(
        DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ExpectedBaselinePreparation,
    )?;
    compare_restart_checkpoint_baseline_observation(expected, checkpoint)
}

fn validate_restart_checkpoint_baseline_against_current_prefix<Source, const N: usize>(
    source: &mut Source,
    checkpoint: &DurableTransactionRestartCheckpointBaselineObservation<'_>,
) -> Result<
    DurableTransactionRestartCheckpointBaseline,
    DurableTransactionRestartCheckpointBaselineValidationError<Source::Error>,
>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
{
    let lineage = source.lineage().clone();
    let Some(expected_id) = lineage.persistent_id() else {
        return Err(
            DurableTransactionRestartCheckpointBaselineValidationError::Evidence(Box::new(
                DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CurrentPersistentLineageRequired,
            )),
        );
    };
    let Some(actual_id) = PersistentLogId::new(checkpoint.persistent_log_id()) else {
        return Err(
            DurableTransactionRestartCheckpointBaselineValidationError::Evidence(Box::new(
                DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ZeroPersistentLogId {
                    persistent_log_id: checkpoint.persistent_log_id(),
                },
            )),
        );
    };
    if actual_id != expected_id {
        return Err(
            DurableTransactionRestartCheckpointBaselineValidationError::Evidence(Box::new(
                DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ForeignPersistentLogId {
                    expected: expected_id,
                    actual: actual_id,
                },
            )),
        );
    }
    if checkpoint.durable_frontier() == Some(0) {
        return Err(
            DurableTransactionRestartCheckpointBaselineValidationError::Evidence(Box::new(
                DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ZeroCheckpointFrontier {
                    checkpoint_frontier: 0,
                },
            )),
        );
    }

    match source.with_durable_transaction_restart_observations(|current_frontier, observations| {
        validate_restart_checkpoint_baseline_evidence(
            &lineage,
            current_frontier,
            observations,
            checkpoint,
        )
    }) {
        Ok(Ok(baseline)) => Ok(baseline),
        Ok(Err(source)) => Err(
            DurableTransactionRestartCheckpointBaselineValidationError::Evidence(Box::new(source)),
        ),
        Err(source) => {
            Err(DurableTransactionRestartCheckpointBaselineValidationError::Source(source))
        }
    }
}

/// Invalid complete-prefix evidence supplied to restart analysis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableTransactionRestartAnalysisEvidenceError {
    /// A nonempty observation stream omitted its durable frontier.
    FrontierMissing {
        /// Number of supplied logical observations.
        record_count: usize,
    },
    /// A durable frontier was supplied for an empty logical-record stream.
    FrontierWithoutRecords {
        /// Unexpected supplied frontier.
        frontier: LogSequenceNumber,
    },
    /// The durable frontier belongs to another WAL lineage.
    ForeignFrontier {
        /// Rejected foreign frontier.
        frontier: LogSequenceNumber,
    },
    /// The nonempty durable frontier used the reserved zero position.
    ZeroFrontier {
        /// Rejected zero frontier.
        frontier: LogSequenceNumber,
    },
    /// One logical observation belongs to another WAL lineage.
    ForeignRecordLineage {
        /// Zero-based observation index.
        index: usize,
        /// Kind of the rejected observation.
        kind: DurableTransactionRestartObservationKind,
        /// Rejected foreign position.
        position: LogSequenceNumber,
    },
    /// Two identical adjacent observations claim one physical position.
    DuplicateRecordPosition {
        /// Zero-based index of the first observation.
        previous_index: usize,
        /// Zero-based index of the repeated observation.
        actual_index: usize,
        /// Kind shared by the identical observations.
        kind: DurableTransactionRestartObservationKind,
        /// Duplicated physical position.
        position: LogSequenceNumber,
    },
    /// Two different adjacent observations claim one physical position.
    ContradictoryRecordPosition {
        /// Zero-based index of the prior observation.
        previous_index: usize,
        /// Kind of the prior observation.
        previous_kind: DurableTransactionRestartObservationKind,
        /// Zero-based index of the contradictory observation.
        actual_index: usize,
        /// Kind of the contradictory observation.
        actual_kind: DurableTransactionRestartObservationKind,
        /// Contradictory physical position.
        position: LogSequenceNumber,
    },
    /// A later observation has a numerically lower physical position.
    NonAdvancingRecordPosition {
        /// Zero-based index of the prior observation.
        previous_index: usize,
        /// Prior physical position.
        previous: LogSequenceNumber,
        /// Zero-based index of the decreasing observation.
        actual_index: usize,
        /// Decreasing physical position.
        actual: LogSequenceNumber,
    },
    /// The last projected record does not equal the supplied durable frontier.
    TailFrontierMismatch {
        /// Supplied durable frontier.
        frontier: LogSequenceNumber,
        /// Actual final observation position.
        tail: LogSequenceNumber,
    },
    /// The owned transaction table could not reserve its complete upper bound.
    TransactionCapacityExhausted {
        /// Durable record count used as the transaction-count upper bound.
        record_count: usize,
    },
    /// One persisted identity has more than one durable commit.
    DuplicateCommit {
        /// Identity with contradictory commit records.
        transaction: DurableTransactionIdentityObservation,
        /// First durable commit position.
        first_commit_position: LogSequenceNumber,
        /// Later duplicate commit position.
        duplicate_commit_position: LogSequenceNumber,
    },
    /// A transaction-owned page occurs after the same identity's commit.
    PageAfterCommit {
        /// Identity whose post-commit page is invalid.
        transaction: DurableTransactionIdentityObservation,
        /// Earlier durable commit position.
        commit_position: LogSequenceNumber,
        /// Invalid later page position.
        page_position: LogSequenceNumber,
    },
    /// Counting one identity's owned pages exceeded `usize`.
    OwnedPageCountExhausted {
        /// Identity whose record count could not advance.
        transaction: DurableTransactionIdentityObservation,
    },
}

impl fmt::Display for DurableTransactionRestartAnalysisEvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrontierMissing { record_count } => write!(
                formatter,
                "nonempty durable restart stream with {record_count} records has no frontier"
            ),
            Self::FrontierWithoutRecords { frontier } => write!(
                formatter,
                "durable restart frontier {} has no logical records",
                frontier.get()
            ),
            Self::ForeignFrontier { frontier } => write!(
                formatter,
                "durable restart frontier {} belongs to another lineage",
                frontier.get()
            ),
            Self::ZeroFrontier { .. } => {
                formatter.write_str("nonempty durable restart frontier must be nonzero")
            }
            Self::ForeignRecordLineage {
                index,
                kind,
                position,
            } => write!(
                formatter,
                "durable restart record {index} ({kind:?}) at position {} belongs to another lineage",
                position.get()
            ),
            Self::DuplicateRecordPosition {
                previous_index,
                actual_index,
                position,
                ..
            } => write!(
                formatter,
                "durable restart records {previous_index} and {actual_index} duplicate position {}",
                position.get()
            ),
            Self::ContradictoryRecordPosition {
                previous_index,
                actual_index,
                position,
                ..
            } => write!(
                formatter,
                "durable restart records {previous_index} and {actual_index} contradict at position {}",
                position.get()
            ),
            Self::NonAdvancingRecordPosition {
                previous_index,
                previous,
                actual_index,
                actual,
            } => write!(
                formatter,
                "durable restart record {actual_index} position {} does not advance beyond record {previous_index} position {}",
                actual.get(),
                previous.get()
            ),
            Self::TailFrontierMismatch { frontier, tail } => write!(
                formatter,
                "durable restart tail position {} does not equal frontier {}",
                tail.get(),
                frontier.get()
            ),
            Self::TransactionCapacityExhausted { record_count } => write!(
                formatter,
                "durable restart transaction capacity is exhausted for {record_count} records"
            ),
            Self::DuplicateCommit {
                transaction,
                first_commit_position,
                duplicate_commit_position,
            } => write!(
                formatter,
                "transaction {transaction} has duplicate commits at positions {} and {}",
                first_commit_position.get(),
                duplicate_commit_position.get()
            ),
            Self::PageAfterCommit {
                transaction,
                commit_position,
                page_position,
            } => write!(
                formatter,
                "transaction {transaction} page at position {} follows commit at position {}",
                page_position.get(),
                commit_position.get()
            ),
            Self::OwnedPageCountExhausted { transaction } => write!(
                formatter,
                "transaction {transaction} owned-page record count is exhausted"
            ),
        }
    }
}

impl Error for DurableTransactionRestartAnalysisEvidenceError {}

/// Failure to obtain or analyze one complete durable restart prefix.
#[derive(Debug)]
pub enum DurableTransactionRestartAnalysisError<SourceError> {
    /// The source failed before returning authoritative stable-prefix evidence.
    Source(SourceError),
    /// The projected complete-prefix evidence was invalid.
    Evidence(Box<DurableTransactionRestartAnalysisEvidenceError>),
}

impl<SourceError: fmt::Display> fmt::Display
    for DurableTransactionRestartAnalysisError<SourceError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => write!(formatter, "restart-analysis source failed: {source}"),
            Self::Evidence(source) => write!(formatter, "restart analysis failed: {source}"),
        }
    }
}

impl<SourceError> Error for DurableTransactionRestartAnalysisError<SourceError>
where
    SourceError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Evidence(source) => Some(source.as_ref()),
        }
    }
}

fn analyze_durable_transaction_restart_evidence<const N: usize>(
    lineage: &LogLineage,
    durable_frontier: Option<&LogSequenceNumber>,
    observations: &[DurableTransactionRestartObservation<N>],
) -> Result<DurableTransactionRestartAnalysis, DurableTransactionRestartAnalysisEvidenceError> {
    if let Some(frontier) = durable_frontier {
        if !lineage.same_lineage(frontier.lineage()) {
            return Err(
                DurableTransactionRestartAnalysisEvidenceError::ForeignFrontier {
                    frontier: frontier.clone(),
                },
            );
        }
        if frontier.get() == 0 {
            return Err(
                DurableTransactionRestartAnalysisEvidenceError::ZeroFrontier {
                    frontier: frontier.clone(),
                },
            );
        }
    }

    match (durable_frontier, observations.is_empty()) {
        (None, false) => {
            return Err(
                DurableTransactionRestartAnalysisEvidenceError::FrontierMissing {
                    record_count: observations.len(),
                },
            );
        }
        (Some(frontier), true) => {
            return Err(
                DurableTransactionRestartAnalysisEvidenceError::FrontierWithoutRecords {
                    frontier: frontier.clone(),
                },
            );
        }
        (None, true) => {
            return Ok(DurableTransactionRestartAnalysis {
                lineage: lineage.clone(),
                durable_frontier: None,
                transactions: Vec::new(),
            });
        }
        (Some(_), false) => {}
    }

    let mut previous: Option<(usize, &DurableTransactionRestartObservation<N>)> = None;
    for (index, observation) in observations.iter().enumerate() {
        let position = observation.position();
        if !lineage.same_lineage(position.lineage()) {
            return Err(
                DurableTransactionRestartAnalysisEvidenceError::ForeignRecordLineage {
                    index,
                    kind: observation.kind(),
                    position: position.clone(),
                },
            );
        }
        if let Some((previous_index, previous_observation)) = previous {
            let previous_position = previous_observation.position();
            if position.get() == previous_position.get() {
                if observation == previous_observation {
                    return Err(
                        DurableTransactionRestartAnalysisEvidenceError::DuplicateRecordPosition {
                            previous_index,
                            actual_index: index,
                            kind: observation.kind(),
                            position: position.clone(),
                        },
                    );
                }
                return Err(
                    DurableTransactionRestartAnalysisEvidenceError::ContradictoryRecordPosition {
                        previous_index,
                        previous_kind: previous_observation.kind(),
                        actual_index: index,
                        actual_kind: observation.kind(),
                        position: position.clone(),
                    },
                );
            }
            if position.get() < previous_position.get() {
                return Err(
                    DurableTransactionRestartAnalysisEvidenceError::NonAdvancingRecordPosition {
                        previous_index,
                        previous: previous_position.clone(),
                        actual_index: index,
                        actual: position.clone(),
                    },
                );
            }
        }
        previous = Some((index, observation));
    }

    let frontier = durable_frontier.ok_or(
        DurableTransactionRestartAnalysisEvidenceError::FrontierMissing {
            record_count: observations.len(),
        },
    )?;
    let tail = observations
        .last()
        .map(DurableTransactionRestartObservation::position);
    let Some(tail) = tail else {
        return Err(
            DurableTransactionRestartAnalysisEvidenceError::FrontierWithoutRecords {
                frontier: frontier.clone(),
            },
        );
    };
    if tail != frontier {
        return Err(
            DurableTransactionRestartAnalysisEvidenceError::TailFrontierMismatch {
                frontier: frontier.clone(),
                tail: tail.clone(),
            },
        );
    }

    let mut transactions: Vec<DurableTransactionRestartEntry> = Vec::new();
    transactions.try_reserve(observations.len()).map_err(|_| {
        DurableTransactionRestartAnalysisEvidenceError::TransactionCapacityExhausted {
            record_count: observations.len(),
        }
    })?;

    for observation in observations {
        match observation {
            DurableTransactionRestartObservation::Page(_) => {}
            DurableTransactionRestartObservation::TransactionPage(observation) => {
                let transaction = observation.owner();
                let page_position = observation.position();
                match transactions.binary_search_by_key(&transaction, |entry| entry.transaction) {
                    Ok(index) => {
                        let entry = &mut transactions[index];
                        if let DurableTransactionRestartState::Committed { commit_position } =
                            &entry.state
                        {
                            return Err(
                                DurableTransactionRestartAnalysisEvidenceError::PageAfterCommit {
                                    transaction,
                                    commit_position: commit_position.clone(),
                                    page_position: page_position.clone(),
                                },
                            );
                        }
                        entry.owned_page_record_count = entry
                            .owned_page_record_count
                            .checked_add(1)
                            .ok_or(
                                DurableTransactionRestartAnalysisEvidenceError::OwnedPageCountExhausted {
                                    transaction,
                                },
                            )?;
                        entry.last_owned_page_position = Some(page_position.clone());
                    }
                    Err(index) => transactions.insert(
                        index,
                        DurableTransactionRestartEntry {
                            transaction,
                            first_owned_page_position: Some(page_position.clone()),
                            last_owned_page_position: Some(page_position.clone()),
                            owned_page_record_count: 1,
                            state: DurableTransactionRestartState::Uncommitted,
                        },
                    ),
                }
            }
            DurableTransactionRestartObservation::Commit(observation) => {
                let transaction = observation.transaction();
                let commit_position = observation.position();
                match transactions.binary_search_by_key(&transaction, |entry| entry.transaction) {
                    Ok(index) => {
                        let entry = &mut transactions[index];
                        match &entry.state {
                            DurableTransactionRestartState::Uncommitted => {
                                entry.state = DurableTransactionRestartState::Committed {
                                    commit_position: commit_position.clone(),
                                };
                            }
                            DurableTransactionRestartState::Committed {
                                commit_position: first_commit_position,
                            } => {
                                return Err(
                                    DurableTransactionRestartAnalysisEvidenceError::DuplicateCommit {
                                        transaction,
                                        first_commit_position: first_commit_position.clone(),
                                        duplicate_commit_position: commit_position.clone(),
                                    },
                                );
                            }
                        }
                    }
                    Err(index) => transactions.insert(
                        index,
                        DurableTransactionRestartEntry {
                            transaction,
                            first_owned_page_position: None,
                            last_owned_page_position: None,
                            owned_page_record_count: 0,
                            state: DurableTransactionRestartState::Committed {
                                commit_position: commit_position.clone(),
                            },
                        },
                    ),
                }
            }
        }
    }

    Ok(DurableTransactionRestartAnalysis {
        lineage: lineage.clone(),
        durable_frontier: Some(frontier.clone()),
        transactions,
    })
}

/// Reconstructs one deterministic transaction table from a complete durable prefix.
///
/// The source callback is invoked at most once. Its complete unified stream is
/// validated before table allocation or transaction-specific classification.
pub fn analyze_durable_transaction_restart<Source, const N: usize>(
    source: &mut Source,
) -> Result<DurableTransactionRestartAnalysis, DurableTransactionRestartAnalysisError<Source::Error>>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
{
    let lineage = source.lineage().clone();
    match source.with_durable_transaction_restart_observations(|durable_frontier, observations| {
        analyze_durable_transaction_restart_evidence(&lineage, durable_frontier, observations)
    }) {
        Ok(Ok(analysis)) => Ok(analysis),
        Ok(Err(source)) => Err(DurableTransactionRestartAnalysisError::Evidence(Box::new(
            source,
        ))),
        Err(source) => Err(DurableTransactionRestartAnalysisError::Source(source)),
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
    /// A transaction-owned page WAL append was attempted but valid append
    /// evidence was not established.
    ///
    /// This phase is distinct from [`Self::Indeterminate`]. Commit and
    /// commit-outcome resolution gate strictly on [`Self::Indeterminate`] and
    /// never reinterpret this page-append phase as a commit attempt.
    PageAppendIndeterminate,
    /// Authoritative recovery found no durable commit record.
    NoDurableCommitRecord,
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
    staged_pages: BTreeSet<(TransactionId, ntsql_page::PageNumber)>,
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
            staged_pages: BTreeSet::new(),
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

    /// Returns whether this coordinator issued the indeterminate token.
    #[must_use]
    pub fn owns_indeterminate(&self, transaction: &IndeterminateTransaction) -> bool {
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

        let log_lineage = self.log_lineage.clone();
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
        match transaction.commit(log, log_lineage) {
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

    /// Resolves one indeterminate token from authoritative durable-log evidence.
    ///
    /// Ownership and lifecycle mismatches are rejected before consulting the
    /// source. Source failures and lineage mismatches retain the same token and
    /// leave the lifecycle indeterminate. Authoritative absence is terminal but
    /// does not recreate active state or define a retry or rollback rule.
    pub fn resolve<Source>(
        &mut self,
        transaction: IndeterminateTransaction,
        source: &mut Source,
    ) -> Result<TransactionCommitResolution, TransactionResolutionError<Source::Error>>
    where
        Source: TransactionRecoverySource + ?Sized,
    {
        if !self.owns_indeterminate(&transaction) {
            return Err(TransactionResolutionError {
                transaction,
                failure: TransactionResolutionFailure::ForeignCoordinator,
            });
        }

        let transaction_id = transaction.transaction_id();
        let actual = self.status(transaction_id);
        if actual != Some(TransactionLifecycleStatus::Indeterminate) {
            return Err(TransactionResolutionError {
                transaction,
                failure: TransactionResolutionFailure::LifecycleMismatch { actual },
            });
        }

        let (lineage, lookup) = match source.lookup_durable_commit(transaction_id) {
            Ok(result) => result,
            Err(source) => {
                return Err(TransactionResolutionError {
                    transaction,
                    failure: TransactionResolutionFailure::Source(source),
                });
            }
        };
        if !transaction.log_lineage.same_lineage(&lineage) {
            return Err(TransactionResolutionError {
                transaction,
                failure: TransactionResolutionFailure::ForeignLogLineage,
            });
        }
        if let DurableCommitLookup::Found { position } = &lookup
            && !lineage.same_lineage(position.lineage())
        {
            return Err(TransactionResolutionError {
                transaction,
                failure: TransactionResolutionFailure::ForeignLogLineage,
            });
        }

        let Some(status) = self.lifecycles.get_mut(&transaction_id) else {
            return Err(TransactionResolutionError {
                transaction,
                failure: TransactionResolutionFailure::LifecycleMismatch { actual: None },
            });
        };
        match lookup {
            DurableCommitLookup::Found { position } => {
                *status = TransactionLifecycleStatus::Committed;
                Ok(TransactionCommitResolution::Committed(
                    CommittedTransaction {
                        transaction_id,
                        log_position: position,
                    },
                ))
            }
            DurableCommitLookup::Absent => {
                *status = TransactionLifecycleStatus::NoDurableCommitRecord;
                Ok(TransactionCommitResolution::NoDurableCommitRecord(
                    NoDurableCommitRecord { transaction_id },
                ))
            }
        }
    }

    /// Stages one transaction-owned page write for an active token issued by
    /// this coordinator.
    ///
    /// Ownership, coordinator/log lineage, page/log lineage, the retained
    /// `Active` phase, and the one-image-per-page limit are validated before the
    /// transaction-page WAL port is called. Every pre-append rejection retains
    /// the unchanged active token and the exact unlogged page and never calls
    /// the append port.
    ///
    /// Once the append port is invoked, an adapter error or invalid lineage
    /// evidence is terminal: the coordinator records
    /// [`TransactionLifecycleStatus::PageAppendIndeterminate`], returns terminal
    /// page evidence with no path back to [`ActiveTransaction`], and blocks
    /// commit and commit-outcome resolution for that identity. Success returns
    /// the same active token plus one [`TransactionDirtyPage`] and leaves the
    /// coordinator lifecycle `Active`; the page store is never called here.
    pub fn stage_page_write<L, const N: usize>(
        &mut self,
        transaction: ActiveTransaction,
        page: UnloggedPage<N>,
        log: &mut L,
    ) -> Result<(ActiveTransaction, TransactionDirtyPage<N>), TransactionPageStageError<L::Error, N>>
    where
        L: TransactionPageLog<N>,
    {
        if !self.owns(&transaction) {
            return Err(TransactionPageStageError::Rejected(
                TransactionPageStageRejection {
                    transaction,
                    page,
                    reason: TransactionPageStageRejectionReason::ForeignCoordinator,
                },
            ));
        }
        if !self.log_lineage.same_lineage(log.lineage()) {
            return Err(TransactionPageStageError::Rejected(
                TransactionPageStageRejection {
                    transaction,
                    page,
                    reason: TransactionPageStageRejectionReason::ForeignLogLineage,
                },
            ));
        }
        if !page.address().lineage().same_lineage(log.lineage()) {
            return Err(TransactionPageStageError::Rejected(
                TransactionPageStageRejection {
                    transaction,
                    page,
                    reason: TransactionPageStageRejectionReason::ForeignPageLineage,
                },
            ));
        }
        let transaction_id = transaction.transaction_id();
        match self.lifecycles.get(&transaction_id).copied() {
            Some(TransactionLifecycleStatus::Active) => {}
            actual => {
                return Err(TransactionPageStageError::Rejected(
                    TransactionPageStageRejection {
                        transaction,
                        page,
                        reason: TransactionPageStageRejectionReason::LifecycleMismatch { actual },
                    },
                ));
            }
        }
        let page_number = page.address().number();
        if !self.staged_pages.insert((transaction_id, page_number)) {
            return Err(TransactionPageStageError::Rejected(
                TransactionPageStageRejection {
                    transaction,
                    page,
                    reason: TransactionPageStageRejectionReason::PageAlreadyStaged,
                },
            ));
        }

        let mut bridge = TransactionPageAppendBridge {
            transaction_id,
            log,
        };
        match stage_page_write(&mut bridge, page) {
            Ok(dirty) => Ok((
                transaction,
                TransactionDirtyPage {
                    transaction_id,
                    dirty,
                },
            )),
            Err(StagePageWriteError::Rejected(rejection)) => {
                // The domain page port re-rejected after this coordinator had
                // already validated the shared lineage. This never invoked the
                // append effect, so the token stays active without poisoning
                // the lifecycle.
                let _ = self.staged_pages.remove(&(transaction_id, page_number));
                Err(TransactionPageStageError::Rejected(
                    TransactionPageStageRejection {
                        transaction,
                        page: rejection.into_page(),
                        reason: TransactionPageStageRejectionReason::InternalPageLogRejection,
                    },
                ))
            }
            Err(StagePageWriteError::Append(error)) => {
                self.mark_page_append_indeterminate(transaction_id);
                let (page, source) = error.into_parts();
                Err(TransactionPageStageError::Append(
                    TransactionPageAppendError {
                        transaction_id,
                        page,
                        source,
                    },
                ))
            }
            Err(StagePageWriteError::InvalidEvidence(error)) => {
                self.mark_page_append_indeterminate(transaction_id);
                let reason = error.reason();
                Err(TransactionPageStageError::InvalidEvidence(
                    TransactionPageEvidenceError {
                        transaction_id,
                        page: error.into_page(),
                        reason,
                    },
                ))
            }
        }
    }

    fn mark_page_append_indeterminate(&mut self, transaction_id: TransactionId) {
        if let Some(status) = self.lifecycles.get_mut(&transaction_id) {
            *status = TransactionLifecycleStatus::PageAppendIndeterminate;
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
        log_lineage: LogLineage,
    ) -> Result<CommittedTransaction, TransactionCommitError<L::Error>>
    where
        L: CommitLog<TransactionCommitRecord>,
    {
        let Self {
            transaction_id,
            coordinator_identity,
        } = self;
        let record = TransactionCommitRecord { transaction_id };

        match commit_durability(log, &record, |acknowledgement| CommittedTransaction {
            transaction_id,
            log_position: acknowledgement.position().clone(),
        }) {
            Ok(committed) => Ok(committed),
            Err(source) => Err(TransactionCommitError {
                transaction: IndeterminateTransaction {
                    transaction_id,
                    coordinator_identity,
                    log_lineage,
                },
                source,
            }),
        }
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
    pub const fn log_position(&self) -> &LogSequenceNumber {
        &self.log_position
    }
}

/// Terminal transaction state for authoritative absence of a durable record.
///
/// This state offers no active, retry, rollback, or client-visible transition.
///
/// ```compile_fail
/// use ntsql_transaction::{NoDurableCommitRecord, TransactionId};
///
/// fn cannot_construct(transaction_id: TransactionId) {
///     let forged = NoDurableCommitRecord { transaction_id };
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct NoDurableCommitRecord {
    transaction_id: TransactionId,
}

impl NoDurableCommitRecord {
    /// Returns the transaction identity absent from the durable view.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }
}

/// Terminal result of resolving an indeterminate commit attempt.
#[derive(Debug, Eq, PartialEq)]
pub enum TransactionCommitResolution {
    /// The exact commit record is present in the durable view.
    Committed(CommittedTransaction),
    /// The complete durable view contains no commit record.
    NoDurableCommitRecord(NoDurableCommitRecord),
}

impl TransactionCommitResolution {
    /// Returns the resolved transaction identity.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        match self {
            Self::Committed(transaction) => transaction.transaction_id(),
            Self::NoDurableCommitRecord(transaction) => transaction.transaction_id(),
        }
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
#[derive(Debug)]
pub struct IndeterminateTransaction {
    transaction_id: TransactionId,
    coordinator_identity: Arc<()>,
    log_lineage: LogLineage,
}

impl IndeterminateTransaction {
    /// Returns the transaction identity requiring later outcome resolution.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }
}

/// Reason authoritative resolution did not produce terminal transaction state.
#[derive(Debug, Eq, PartialEq)]
pub enum TransactionResolutionFailure<E> {
    /// The token belongs to another coordinator runtime identity.
    ForeignCoordinator,
    /// The recovery source searched a different log lineage.
    ForeignLogLineage,
    /// The issuing coordinator did not retain the expected indeterminate phase.
    LifecycleMismatch {
        /// Recorded phase, or `None` when the registry entry is absent.
        actual: Option<TransactionLifecycleStatus>,
    },
    /// The recovery source could not establish an authoritative result.
    Source(E),
}

/// Failed resolution that retains the consumed indeterminate token.
#[derive(Debug)]
pub struct TransactionResolutionError<E> {
    transaction: IndeterminateTransaction,
    failure: TransactionResolutionFailure<E>,
}

impl<E> TransactionResolutionError<E> {
    /// Returns the transaction identity that remains indeterminate.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction.transaction_id()
    }

    /// Borrows the token retained after failed resolution.
    #[must_use]
    pub const fn transaction(&self) -> &IndeterminateTransaction {
        &self.transaction
    }

    /// Borrows the exact resolution failure.
    #[must_use]
    pub const fn failure(&self) -> &TransactionResolutionFailure<E> {
        &self.failure
    }

    /// Returns the retained token for another authoritative resolution attempt.
    #[must_use]
    pub fn into_transaction(self) -> IndeterminateTransaction {
        self.transaction
    }

    /// Returns the retained token and exact resolution failure.
    #[must_use]
    pub fn into_parts(self) -> (IndeterminateTransaction, TransactionResolutionFailure<E>) {
        (self.transaction, self.failure)
    }
}

impl<E: fmt::Display> fmt::Display for TransactionResolutionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.failure {
            TransactionResolutionFailure::ForeignCoordinator => write!(
                formatter,
                "transaction {} belongs to another coordinator",
                self.transaction_id()
            ),
            TransactionResolutionFailure::ForeignLogLineage => write!(
                formatter,
                "transaction {} recovery source belongs to another log lineage",
                self.transaction_id()
            ),
            TransactionResolutionFailure::LifecycleMismatch { actual } => write!(
                formatter,
                "transaction {} is not indeterminate in its coordinator registry: {actual:?}",
                self.transaction_id()
            ),
            TransactionResolutionFailure::Source(source) => write!(
                formatter,
                "transaction {} durable commit lookup failed: {source}",
                self.transaction_id()
            ),
        }
    }
}

impl<E> Error for TransactionResolutionError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.failure {
            TransactionResolutionFailure::Source(source) => Some(source),
            TransactionResolutionFailure::ForeignCoordinator
            | TransactionResolutionFailure::ForeignLogLineage
            | TransactionResolutionFailure::LifecycleMismatch { .. } => None,
        }
    }
}

/// WAL failure paired with the consumed transaction's fail-closed state.
#[derive(Debug)]
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

/// Caller-borrowed WAL record for one transaction-owned page write.
///
/// Only the coordinator's private append bridge constructs this value, so safe
/// downstream code cannot forge a transaction-owned page record. Persistence
/// adapters may inspect the identity and borrowed page but do not own the
/// transaction lifecycle.
///
/// ```compile_fail
/// use ntsql_page::UnloggedPage;
/// use ntsql_transaction::{TransactionId, TransactionPageWriteRecord};
///
/// fn cannot_construct<const N: usize>(
///     transaction_id: TransactionId,
///     page: &UnloggedPage<N>,
/// ) {
///     let _forged = TransactionPageWriteRecord {
///         transaction_id,
///         page,
///     };
/// }
/// ```
#[derive(Debug)]
pub struct TransactionPageWriteRecord<'page, const N: usize> {
    transaction_id: TransactionId,
    page: &'page UnloggedPage<N>,
}

impl<const N: usize> TransactionPageWriteRecord<'_, N> {
    /// Returns the transaction identity that owns this page write.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the borrowed unlogged page image being appended.
    #[must_use]
    pub const fn page(&self) -> &UnloggedPage<N> {
        self.page
    }
}

/// WAL append port for one transaction-owned full page image.
///
/// Hard adapter obligation: transaction-page appends and transaction commit
/// appends for one lineage must share exactly one monotonic position space and
/// one durable frontier, so a later commit flush also makes every prior
/// transaction-page record durable. The domain validates that the returned
/// position is lineage-bound; it cannot prove the adapter honors this shared
/// frontier. Extending [`CommitLog<TransactionCommitRecord>`] requires one
/// adapter surface to provide both append operations. An adapter that assigns
/// handles sharing a lineage to independent spaces or frontiers still violates
/// this port even if each record is individually well formed.
pub trait TransactionPageLog<const N: usize>: CommitLog<TransactionCommitRecord> {
    /// Appends one transaction-owned page image and returns its exact
    /// lineage-bound WAL position.
    ///
    /// Success means the record was appended, not made durable. An error does
    /// not specify whether the physical append occurred.
    fn append_transaction_page(
        &mut self,
        record: &TransactionPageWriteRecord<'_, N>,
    ) -> Result<LogSequenceNumber, Self::Error>;
}

/// Private adapter that presents a [`TransactionPageLog`] as a [`PageLog`] so
/// the domain page-staging evidence checks can run unchanged while every append
/// carries the owning [`TransactionId`].
struct TransactionPageAppendBridge<'log, L, const N: usize>
where
    L: TransactionPageLog<N>,
{
    transaction_id: TransactionId,
    log: &'log mut L,
}

impl<L, const N: usize> LogDurability for TransactionPageAppendBridge<'_, L, N>
where
    L: TransactionPageLog<N>,
{
    type Error = L::Error;

    fn lineage(&self) -> &LogLineage {
        self.log.lineage()
    }

    fn flush_through(&mut self, position: &LogSequenceNumber) -> Result<(), Self::Error> {
        self.log.flush_through(position)
    }
}

impl<L, const N: usize> PageLog<N> for TransactionPageAppendBridge<'_, L, N>
where
    L: TransactionPageLog<N>,
{
    fn append_page(&mut self, page: &UnloggedPage<N>) -> Result<LogSequenceNumber, Self::Error> {
        let record = TransactionPageWriteRecord {
            transaction_id: self.transaction_id,
            page,
        };
        self.log.append_transaction_page(&record)
    }
}

/// Transaction-owned dirty page whose exact WAL position was appended under one
/// transaction identity but which cannot reach the page store until the same
/// transaction is durably committed.
///
/// This wrapper is not cloneable, exposes no raw [`DirtyPage`],
/// `PageWritePermit`, or store capability, and provides only read-only owner and
/// page inspection. The local no-steal rule is enforced structurally: a
/// transaction-owned image cannot be flushed except through
/// [`flush_committed_page`], which requires a [`CommittedTransaction`].
///
/// ```compile_fail
/// use ntsql_transaction::{TransactionDirtyPage, TransactionId};
/// use ntsql_page::DirtyPage;
///
/// fn cannot_construct<const N: usize>(transaction_id: TransactionId, dirty: DirtyPage<N>) {
///     let _forged = TransactionDirtyPage {
///         transaction_id,
///         dirty,
///     };
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::TransactionDirtyPage;
///
/// fn cannot_clone<const N: usize>(page: TransactionDirtyPage<N>) {
///     let _duplicate = page.clone();
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_transaction::TransactionDirtyPage;
/// use ntsql_page::DirtyPage;
///
/// fn cannot_extract_raw_dirty<const N: usize>(page: TransactionDirtyPage<N>) -> DirtyPage<N> {
///     page.into_dirty_page()
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct TransactionDirtyPage<const N: usize> {
    transaction_id: TransactionId,
    dirty: DirtyPage<N>,
}

impl<const N: usize> TransactionDirtyPage<N> {
    /// Returns the owning transaction identity.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the adapter-assigned page version.
    #[must_use]
    pub const fn version(&self) -> ntsql_page::PageVersion {
        self.dirty.version()
    }

    /// Returns the internal page address.
    #[must_use]
    pub const fn address(&self) -> &ntsql_page::PageAddress {
        self.dirty.address()
    }

    /// Returns the borrowed dirty image bytes.
    #[must_use]
    pub const fn image(&self) -> &ntsql_page::PageImage<N> {
        self.dirty.image()
    }

    /// Returns the exact WAL position that must be durable before a committed
    /// flush may store this image.
    #[must_use]
    pub const fn required_position(&self) -> &LogSequenceNumber {
        self.dirty.required_position()
    }
}

/// Transaction-owned clean page whose required WAL position and durable page
/// write both reported success for the owning committed transaction.
///
/// ```compile_fail
/// use ntsql_transaction::{TransactionCleanPage, TransactionId};
/// use ntsql_page::CleanPage;
///
/// fn cannot_construct<const N: usize>(transaction_id: TransactionId, clean: CleanPage<N>) {
///     let _forged = TransactionCleanPage {
///         transaction_id,
///         clean,
///     };
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct TransactionCleanPage<const N: usize> {
    transaction_id: TransactionId,
    clean: CleanPage<N>,
}

impl<const N: usize> TransactionCleanPage<N> {
    /// Returns the owning transaction identity.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the internal page address.
    #[must_use]
    pub const fn address(&self) -> &ntsql_page::PageAddress {
        self.clean.address()
    }

    /// Returns the adapter-assigned page version.
    #[must_use]
    pub const fn version(&self) -> ntsql_page::PageVersion {
        self.clean.version()
    }

    /// Returns the borrowed clean image bytes.
    #[must_use]
    pub const fn image(&self) -> &ntsql_page::PageImage<N> {
        self.clean.image()
    }

    /// Returns the exact WAL position that was durable before durable page
    /// completion was reported.
    #[must_use]
    pub const fn required_position(&self) -> &LogSequenceNumber {
        self.clean.required_position()
    }

    /// Returns the owning identity and the terminal clean page.
    #[must_use]
    pub fn into_parts(self) -> (TransactionId, CleanPage<N>) {
        (self.transaction_id, self.clean)
    }
}

/// Reason a transaction-owned page write was rejected before the append port
/// was called.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionPageStageRejectionReason {
    /// The active token belongs to another coordinator runtime identity.
    ForeignCoordinator,
    /// The transaction-page log does not match the coordinator lineage.
    ForeignLogLineage,
    /// The page address belongs to another lineage than the log.
    ForeignPageLineage,
    /// The issuing coordinator did not retain the expected active phase.
    LifecycleMismatch {
        /// Recorded phase, or `None` when the registry entry is absent.
        actual: Option<TransactionLifecycleStatus>,
    },
    /// This transaction already staged an image for the same page.
    PageAlreadyStaged,
    /// The domain page port re-rejected the composition before invoking the
    /// append effect. This is retryable and did not poison the lifecycle.
    InternalPageLogRejection,
}

/// Pre-append page-stage rejection that retains the still-active token and the
/// exact unlogged page.
#[derive(Debug)]
pub struct TransactionPageStageRejection<const N: usize> {
    transaction: ActiveTransaction,
    page: UnloggedPage<N>,
    reason: TransactionPageStageRejectionReason,
}

impl<const N: usize> TransactionPageStageRejection<N> {
    /// Returns the rejected token's transaction identity.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction.transaction_id()
    }

    /// Returns the exact rejection reason.
    #[must_use]
    pub const fn reason(&self) -> TransactionPageStageRejectionReason {
        self.reason
    }

    /// Borrows the retained still-active token.
    #[must_use]
    pub const fn transaction(&self) -> &ActiveTransaction {
        &self.transaction
    }

    /// Borrows the retained unchanged unlogged page.
    #[must_use]
    pub const fn page(&self) -> &UnloggedPage<N> {
        &self.page
    }

    /// Returns the retained active token and unchanged unlogged page for a
    /// corrected composition.
    #[must_use]
    pub fn into_parts(self) -> (ActiveTransaction, UnloggedPage<N>) {
        (self.transaction, self.page)
    }
}

impl<const N: usize> fmt::Display for TransactionPageStageRejection<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            TransactionPageStageRejectionReason::ForeignCoordinator => write!(
                formatter,
                "transaction {} belongs to another coordinator",
                self.transaction_id()
            ),
            TransactionPageStageRejectionReason::ForeignLogLineage => write!(
                formatter,
                "transaction {} belongs to another transaction-page log lineage",
                self.transaction_id()
            ),
            TransactionPageStageRejectionReason::ForeignPageLineage => write!(
                formatter,
                "transaction {} page belongs to another log lineage",
                self.transaction_id()
            ),
            TransactionPageStageRejectionReason::LifecycleMismatch { actual } => write!(
                formatter,
                "transaction {} is not active in its coordinator registry: {actual:?}",
                self.transaction_id()
            ),
            TransactionPageStageRejectionReason::PageAlreadyStaged => write!(
                formatter,
                "transaction {} already staged page {}",
                self.transaction_id(),
                self.page.address().number().get()
            ),
            TransactionPageStageRejectionReason::InternalPageLogRejection => write!(
                formatter,
                "transaction {} page composition was rejected before append",
                self.transaction_id()
            ),
        }
    }
}

impl<const N: usize> Error for TransactionPageStageRejection<N> {}

/// Terminal transaction-owned page state after an append port error.
///
/// This value offers no path back to [`ActiveTransaction`].
#[derive(Debug)]
pub struct TransactionPageAppendError<E, const N: usize> {
    transaction_id: TransactionId,
    page: IndeterminatePageLogAppend<N>,
    source: E,
}

impl<E, const N: usize> TransactionPageAppendError<E, N> {
    /// Returns the owning transaction identity that is now page-indeterminate.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the terminal page state.
    #[must_use]
    pub const fn page(&self) -> &IndeterminatePageLogAppend<N> {
        &self.page
    }

    /// Returns the exact WAL append failure.
    #[must_use]
    pub const fn cause(&self) -> &E {
        &self.source
    }

    /// Returns the owning identity, terminal page state, and exact WAL cause.
    #[must_use]
    pub fn into_parts(self) -> (TransactionId, IndeterminatePageLogAppend<N>, E) {
        (self.transaction_id, self.page, self.source)
    }
}

impl<E: fmt::Display, const N: usize> fmt::Display for TransactionPageAppendError<E, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {} page {} WAL append failed: {}",
            self.transaction_id,
            self.page.address().number().get(),
            self.source
        )
    }
}

impl<E, const N: usize> Error for TransactionPageAppendError<E, N>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Terminal transaction-owned page state after append returned invalid lineage
/// evidence.
///
/// This value offers no path back to [`ActiveTransaction`].
#[derive(Debug, Eq, PartialEq)]
pub struct TransactionPageEvidenceError<const N: usize> {
    transaction_id: TransactionId,
    page: IndeterminatePageLogAppend<N>,
    reason: StagePageWriteEvidenceErrorReason,
}

impl<const N: usize> TransactionPageEvidenceError<N> {
    /// Returns the owning transaction identity that is now page-indeterminate.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the terminal page state.
    #[must_use]
    pub const fn page(&self) -> &IndeterminatePageLogAppend<N> {
        &self.page
    }

    /// Returns the exact evidence failure.
    #[must_use]
    pub const fn reason(&self) -> StagePageWriteEvidenceErrorReason {
        self.reason
    }

    /// Returns the owning identity and terminal page state.
    #[must_use]
    pub fn into_parts(self) -> (TransactionId, IndeterminatePageLogAppend<N>) {
        (self.transaction_id, self.page)
    }
}

impl<const N: usize> fmt::Display for TransactionPageEvidenceError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {} page {} WAL append evidence is invalid: {:?}",
            self.transaction_id,
            self.page.address().number().get(),
            self.reason
        )
    }
}

impl<const N: usize> Error for TransactionPageEvidenceError<N> {}

/// Transaction-owned page staging failure before or after the append effect
/// boundary.
#[derive(Debug)]
pub enum TransactionPageStageError<E, const N: usize> {
    /// Composition was rejected before append; the active token and unlogged
    /// page are retained.
    Rejected(TransactionPageStageRejection<N>),
    /// Append returned an adapter failure after it was invoked; terminal.
    Append(TransactionPageAppendError<E, N>),
    /// Append returned success with invalid lineage evidence; terminal.
    InvalidEvidence(TransactionPageEvidenceError<N>),
}

impl<E: fmt::Display, const N: usize> fmt::Display for TransactionPageStageError<E, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(error) => error.fmt(formatter),
            Self::Append(error) => error.fmt(formatter),
            Self::InvalidEvidence(error) => error.fmt(formatter),
        }
    }
}

impl<E, const N: usize> Error for TransactionPageStageError<E, N>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::Append(error) => Some(error),
            Self::InvalidEvidence(error) => Some(error),
        }
    }
}

/// Reason a committed-page flush was rejected before any log or store port was
/// called.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionCommittedFlushRejectionReason {
    /// The committed transaction identity does not equal the page owner.
    WrongTransaction,
    /// The committed transaction position belongs to another lineage than the
    /// page's required WAL position.
    ForeignCommitLineage,
    /// The committed position is not strictly after the page WAL position, so
    /// the commit does not cover the page record on the shared frontier.
    CommitPositionNotAfterPage,
    /// A domain flush port re-rejected the page for a foreign log or store
    /// lineage before invoking either port. Retryable.
    InternalFlushRejection,
}

/// Failed committed-page flush rejected before touching either injected port.
#[derive(Debug, Eq, PartialEq)]
pub struct TransactionCommittedFlushRejection<const N: usize> {
    page: TransactionDirtyPage<N>,
    reason: TransactionCommittedFlushRejectionReason,
}

impl<const N: usize> TransactionCommittedFlushRejection<N> {
    /// Returns the retained transaction-owned dirty page.
    #[must_use]
    pub const fn page(&self) -> &TransactionDirtyPage<N> {
        &self.page
    }

    /// Returns the exact rejection reason.
    #[must_use]
    pub const fn reason(&self) -> TransactionCommittedFlushRejectionReason {
        self.reason
    }

    /// Returns the retained transaction-owned dirty page.
    #[must_use]
    pub fn into_page(self) -> TransactionDirtyPage<N> {
        self.page
    }
}

impl<const N: usize> fmt::Display for TransactionCommittedFlushRejection<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            TransactionCommittedFlushRejectionReason::WrongTransaction => write!(
                formatter,
                "committed flush transaction does not own page {}",
                self.page.address().number().get()
            ),
            TransactionCommittedFlushRejectionReason::ForeignCommitLineage => write!(
                formatter,
                "committed flush position belongs to another lineage than page {}",
                self.page.address().number().get()
            ),
            TransactionCommittedFlushRejectionReason::CommitPositionNotAfterPage => write!(
                formatter,
                "committed flush position is not strictly after page {} WAL position {}",
                self.page.address().number().get(),
                self.page.required_position().get()
            ),
            TransactionCommittedFlushRejectionReason::InternalFlushRejection => write!(
                formatter,
                "committed flush of page {} was rejected before any port",
                self.page.address().number().get()
            ),
        }
    }
}

impl<const N: usize> Error for TransactionCommittedFlushRejection<N> {}

/// WAL flush failure that retains the retryable transaction-owned dirty page
/// because the page store was never called.
#[derive(Debug)]
pub struct TransactionCommittedFlushLogError<E, const N: usize> {
    page: TransactionDirtyPage<N>,
    source: E,
}

impl<E, const N: usize> TransactionCommittedFlushLogError<E, N> {
    /// Returns the retained retryable transaction-owned dirty page.
    #[must_use]
    pub const fn page(&self) -> &TransactionDirtyPage<N> {
        &self.page
    }

    /// Returns the exact WAL failure.
    #[must_use]
    pub const fn cause(&self) -> &E {
        &self.source
    }

    /// Returns the retryable dirty page and the exact WAL failure.
    #[must_use]
    pub fn into_parts(self) -> (TransactionDirtyPage<N>, E) {
        (self.page, self.source)
    }
}

impl<E: fmt::Display, const N: usize> fmt::Display for TransactionCommittedFlushLogError<E, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {} page {} WAL flush through {} failed: {}",
            self.page.transaction_id(),
            self.page.address().number().get(),
            self.page.required_position().get(),
            self.source
        )
    }
}

impl<E, const N: usize> Error for TransactionCommittedFlushLogError<E, N>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Terminal transaction-owned page state after WAL durability succeeded but the
/// page-store write failed.
///
/// This value retains the committed transaction identity and offers no retry
/// entrypoint and no manufactured success.
#[derive(Debug)]
pub struct TransactionCommittedFlushStoreError<E, const N: usize> {
    transaction_id: TransactionId,
    page: IndeterminatePageWrite<N>,
    source: E,
}

impl<E, const N: usize> TransactionCommittedFlushStoreError<E, N> {
    /// Returns the committed transaction identity whose page write is now
    /// terminally indeterminate.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the terminal indeterminate page state.
    #[must_use]
    pub const fn page(&self) -> &IndeterminatePageWrite<N> {
        &self.page
    }

    /// Returns the exact store failure.
    #[must_use]
    pub const fn cause(&self) -> &E {
        &self.source
    }

    /// Returns the committed identity, terminal page state, and exact store
    /// failure.
    #[must_use]
    pub fn into_parts(self) -> (TransactionId, IndeterminatePageWrite<N>, E) {
        (self.transaction_id, self.page, self.source)
    }
}

impl<E: fmt::Display, const N: usize> fmt::Display for TransactionCommittedFlushStoreError<E, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "transaction {} page {} durable write after WAL position {} failed: {}",
            self.transaction_id,
            self.page.address().number().get(),
            self.page.required_position().get(),
            self.source
        )
    }
}

impl<E, const N: usize> Error for TransactionCommittedFlushStoreError<E, N>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Failed committed-page flush before or after the store indeterminacy boundary.
#[derive(Debug)]
pub enum TransactionCommittedFlushError<LogError, StoreError, const N: usize> {
    /// Identity, lineage, or ordering validation rejected the flush before any
    /// port was called; the wrapper is retained.
    Rejected(TransactionCommittedFlushRejection<N>),
    /// The WAL flush failed before the page store was called; the retryable
    /// wrapper is retained.
    LogFlush(TransactionCommittedFlushLogError<LogError, N>),
    /// The WAL flush succeeded but the page-store result is terminally
    /// indeterminate.
    StoreWrite(TransactionCommittedFlushStoreError<StoreError, N>),
}

impl<LogError: fmt::Display, StoreError: fmt::Display, const N: usize> fmt::Display
    for TransactionCommittedFlushError<LogError, StoreError, N>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(error) => error.fmt(formatter),
            Self::LogFlush(error) => error.fmt(formatter),
            Self::StoreWrite(error) => error.fmt(formatter),
        }
    }
}

impl<LogError, StoreError, const N: usize> Error
    for TransactionCommittedFlushError<LogError, StoreError, N>
where
    LogError: Error + 'static,
    StoreError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::LogFlush(error) => Some(error),
            Self::StoreWrite(error) => Some(error),
        }
    }
}

/// Flushes one transaction-owned dirty page only after its owning transaction is
/// durably committed.
///
/// This is the committed gate for the local no-steal rule. Before touching any
/// port it validates exact transaction-identity equality, that the committed
/// position shares the page's WAL lineage, and that the committed position is
/// strictly after the page WAL position on the shared frontier. Identity
/// equality alone is insufficient because identities can repeat across
/// lineages. Each pre-port rejection retains the wrapper.
///
/// On success it delegates to the existing WAL-before-store page flush. A WAL
/// flush failure retains the retryable transaction-owned dirty page and the
/// exact cause. A store-write failure is terminal: it retains the committed
/// identity, an `IndeterminatePageWrite`, and the exact cause and never
/// manufactures success.
///
/// ```compile_fail
/// use ntsql_transaction::{ActiveTransaction, TransactionDirtyPage, flush_committed_page};
/// use ntsql_page::PageStore;
/// use ntsql_wal::LogDurability;
///
/// fn cannot_flush_active<Log, Store, const N: usize>(
///     active: &ActiveTransaction,
///     log: &mut Log,
///     store: &mut Store,
///     page: TransactionDirtyPage<N>,
/// )
/// where
///     Log: LogDurability,
///     Store: PageStore<N>,
/// {
///     let _ = flush_committed_page(active, log, store, page);
/// }
/// ```
pub fn flush_committed_page<Log, Store, const N: usize>(
    committed: &CommittedTransaction,
    log: &mut Log,
    store: &mut Store,
    page: TransactionDirtyPage<N>,
) -> Result<TransactionCleanPage<N>, TransactionCommittedFlushError<Log::Error, Store::Error, N>>
where
    Log: LogDurability,
    Store: PageStore<N>,
{
    if committed.transaction_id() != page.transaction_id {
        return Err(TransactionCommittedFlushError::Rejected(
            TransactionCommittedFlushRejection {
                page,
                reason: TransactionCommittedFlushRejectionReason::WrongTransaction,
            },
        ));
    }
    if !committed
        .log_position()
        .lineage()
        .same_lineage(page.required_position().lineage())
    {
        return Err(TransactionCommittedFlushError::Rejected(
            TransactionCommittedFlushRejection {
                page,
                reason: TransactionCommittedFlushRejectionReason::ForeignCommitLineage,
            },
        ));
    }
    if committed.log_position().get() <= page.required_position().get() {
        return Err(TransactionCommittedFlushError::Rejected(
            TransactionCommittedFlushRejection {
                page,
                reason: TransactionCommittedFlushRejectionReason::CommitPositionNotAfterPage,
            },
        ));
    }

    let TransactionDirtyPage {
        transaction_id,
        dirty,
    } = page;
    match flush_dirty_page(log, store, dirty) {
        Ok(clean) => Ok(TransactionCleanPage {
            transaction_id,
            clean,
        }),
        Err(FlushDirtyPageError::Rejected(rejection)) => Err(
            TransactionCommittedFlushError::Rejected(TransactionCommittedFlushRejection {
                page: TransactionDirtyPage {
                    transaction_id,
                    dirty: rejection.into_page(),
                },
                reason: TransactionCommittedFlushRejectionReason::InternalFlushRejection,
            }),
        ),
        Err(FlushDirtyPageError::LogFlush(error)) => {
            let (dirty, source) = error.into_parts();
            Err(TransactionCommittedFlushError::LogFlush(
                TransactionCommittedFlushLogError {
                    page: TransactionDirtyPage {
                        transaction_id,
                        dirty,
                    },
                    source,
                },
            ))
        }
        Err(FlushDirtyPageError::StoreWrite(error)) => {
            let (page, source) = error.into_parts();
            Err(TransactionCommittedFlushError::StoreWrite(
                TransactionCommittedFlushStoreError {
                    transaction_id,
                    page,
                    source,
                },
            ))
        }
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

    struct TestRecoverySource {
        lineage: LogLineage,
        calls: usize,
    }

    impl TransactionRecoverySource for TestRecoverySource {
        type Error = TransactionIssueError;

        fn lookup_durable_commit(
            &mut self,
            _transaction_id: TransactionId,
        ) -> Result<(LogLineage, DurableCommitLookup), Self::Error> {
            self.calls += 1;
            Ok((self.lineage.clone(), DurableCommitLookup::Absent))
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

    #[test]
    fn lifecycle_mismatch_rejects_before_recovery_lookup() -> Result<(), TransactionIssueError> {
        let lineage = LogLineage::new();
        let mut source = TestEpochSource {
            lineage: lineage.clone(),
            next_epoch: Some(NonZeroU64::MIN),
        };
        let mut coordinator = TransactionCoordinator::open(&mut source)?;
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        let indeterminate = IndeterminateTransaction {
            transaction_id,
            coordinator_identity: Arc::clone(&coordinator.identity),
            log_lineage: lineage.clone(),
        };
        let mut recovery = TestRecoverySource { lineage, calls: 0 };

        let error = coordinator.resolve(indeterminate, &mut recovery).err();

        let Some(error) = error else {
            return Err(TransactionIssueError::IdentityAlreadyIssued(transaction_id));
        };
        assert_eq!(
            error.failure(),
            &TransactionResolutionFailure::LifecycleMismatch {
                actual: Some(TransactionLifecycleStatus::Active)
            }
        );
        assert_eq!(recovery.calls, 0);
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::Active)
        );
        Ok(())
    }

    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use ntsql_page::{PageAddress, PageImage, PageNumber, PageVersion};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeFault(&'static str);

    impl fmt::Display for FakeFault {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeFault {}

    #[derive(Debug, Eq, PartialEq)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for TestError {}

    impl From<TransactionIssueError> for TestError {
        fn from(_: TransactionIssueError) -> Self {
            Self("transaction issue")
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FlushEvent {
        Flush(u64),
        Write(u64),
    }

    struct FakeLog {
        lineage: LogLineage,
        next_position: u64,
        append_page_fault: Option<FakeFault>,
        foreign_append_lineage: Option<LogLineage>,
        rotate_after_append: Option<LogLineage>,
        flush_fault: Option<FakeFault>,
        appended_pages: Vec<(TransactionId, u64)>,
        flushed: Vec<u64>,
        events: Option<Rc<RefCell<Vec<FlushEvent>>>>,
    }

    impl FakeLog {
        fn new(lineage: LogLineage) -> Self {
            Self {
                lineage,
                next_position: 1,
                append_page_fault: None,
                foreign_append_lineage: None,
                rotate_after_append: None,
                flush_fault: None,
                appended_pages: Vec::new(),
                flushed: Vec::new(),
                events: None,
            }
        }
    }

    impl LogDurability for FakeLog {
        type Error = FakeFault;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn flush_through(&mut self, position: &LogSequenceNumber) -> Result<(), Self::Error> {
            if let Some(events) = &self.events {
                events.borrow_mut().push(FlushEvent::Flush(position.get()));
            }
            if let Some(fault) = self.flush_fault {
                return Err(fault);
            }
            self.flushed.push(position.get());
            Ok(())
        }
    }

    impl<const N: usize> TransactionPageLog<N> for FakeLog {
        fn append_transaction_page(
            &mut self,
            record: &TransactionPageWriteRecord<'_, N>,
        ) -> Result<LogSequenceNumber, Self::Error> {
            if let Some(fault) = self.append_page_fault {
                return Err(fault);
            }
            self.appended_pages.push((
                record.transaction_id(),
                record.page().address().number().get(),
            ));
            let value = self.next_position;
            self.next_position += 1;
            let position = match &self.foreign_append_lineage {
                Some(foreign) => foreign.position(value),
                None => self.lineage.position(value),
            };
            if let Some(rotated) = &self.rotate_after_append {
                self.lineage = rotated.clone();
            }
            Ok(position)
        }
    }

    impl CommitLog<TransactionCommitRecord> for FakeLog {
        fn append_commit(
            &mut self,
            _record: &TransactionCommitRecord,
        ) -> Result<LogSequenceNumber, Self::Error> {
            let value = self.next_position;
            self.next_position += 1;
            Ok(self.lineage.position(value))
        }
    }

    struct FakeStore {
        lineage: LogLineage,
        write_fault: Option<FakeFault>,
        writes: Vec<u64>,
        events: Option<Rc<RefCell<Vec<FlushEvent>>>>,
    }

    impl FakeStore {
        fn new(lineage: LogLineage) -> Self {
            Self {
                lineage,
                write_fault: None,
                writes: Vec::new(),
                events: None,
            }
        }
    }

    impl<const N: usize> PageStore<N> for FakeStore {
        type Error = FakeFault;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn write_page(
            &mut self,
            page: &DirtyPage<N>,
            permit: ntsql_page::PageWritePermit<'_>,
        ) -> Result<(), Self::Error> {
            if let Some(events) = &self.events {
                events
                    .borrow_mut()
                    .push(FlushEvent::Write(permit.durable_position().get()));
            }
            if let Some(fault) = self.write_fault {
                return Err(fault);
            }
            self.writes.push(page.required_position().get());
            Ok(())
        }
    }

    fn open_coordinator(lineage: &LogLineage) -> Result<TransactionCoordinator, TestError> {
        let mut source = TestEpochSource {
            lineage: lineage.clone(),
            next_epoch: Some(NonZeroU64::MIN),
        };
        Ok(TransactionCoordinator::open(&mut source)?)
    }

    fn make_page(
        lineage: &LogLineage,
        number: u64,
        version: u64,
        byte: u8,
    ) -> Result<UnloggedPage<1>, TestError> {
        let number = PageNumber::new(number).ok_or(TestError("page number"))?;
        let image = PageImage::new([byte]).map_err(|_| TestError("page image"))?;
        Ok(UnloggedPage::new(
            PageAddress::new(lineage, number),
            PageVersion::new(version),
            image,
        ))
    }

    fn durable_identity(
        epoch: u64,
        sequence: u64,
    ) -> Result<DurableTransactionIdentityObservation, TestError> {
        DurableTransactionIdentityObservation::new(epoch, sequence)
            .map_err(|_| TestError("durable transaction identity"))
    }

    fn physical_page_observation(
        lineage: &LogLineage,
        number: u64,
        version: u64,
        byte: u8,
        position: u64,
    ) -> Result<DurablePageWalObservation<1>, TestError> {
        let number = PageNumber::new(number).ok_or(TestError("durable page number"))?;
        DurablePageWalObservation::from_bytes(
            number,
            PageVersion::new(version),
            [byte],
            lineage.position(position),
        )
        .map_err(|_| TestError("durable page observation"))
    }

    fn durable_page_observation(
        lineage: &LogLineage,
        owner: DurableTransactionIdentityObservation,
        number: u64,
        version: u64,
        byte: u8,
        position: u64,
    ) -> Result<DurableTransactionPageObservation<1>, TestError> {
        let page = physical_page_observation(lineage, number, version, byte, position)?;
        Ok(DurableTransactionPageObservation::new(owner, page))
    }

    fn stored_page_observation(
        lineage: &LogLineage,
        number: u64,
        version: u64,
        byte: u8,
        position: u64,
    ) -> Result<StoredPageSnapshotObservation<1>, TestError> {
        let number = PageNumber::new(number).ok_or(TestError("stored page number"))?;
        StoredPageSnapshotObservation::from_bytes(
            number,
            PageVersion::new(version),
            [byte],
            lineage.position(position),
        )
        .map_err(|_| TestError("stored page observation"))
    }

    fn durable_commit_observation(
        lineage: &LogLineage,
        transaction: DurableTransactionIdentityObservation,
        position: u64,
    ) -> Result<DurableTransactionCommitObservation, TestError> {
        DurableTransactionCommitObservation::new(transaction, lineage.position(position))
            .map_err(|_| TestError("durable commit observation"))
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeRecoverySnapshot {
        page_number: PageNumber,
        page_version: PageVersion,
        byte: u8,
        page_position: LogSequenceNumber,
    }

    impl FakeRecoverySnapshot {
        fn observation(&self) -> Result<StoredPageSnapshotObservation<1>, FakeFault> {
            StoredPageSnapshotObservation::from_bytes(
                self.page_number,
                self.page_version,
                [self.byte],
                self.page_position.clone(),
            )
            .map_err(|_| FakeFault("snapshot projection"))
        }

        fn from_target(target: &CommittedTransactionPageRecoveryTarget<1>) -> Self {
            Self {
                page_number: target.page_number(),
                page_version: target.page_version(),
                byte: target.bytes()[0],
                page_position: target.page_position().clone(),
            }
        }
    }

    struct FakeDurablePageRecoverySource {
        lineage: LogLineage,
        physical: Vec<DurablePageWalObservation<1>>,
        owned: Vec<DurableTransactionPageObservation<1>>,
        commits: Vec<DurableTransactionCommitObservation>,
        inventory: Vec<PageNumber>,
        inventory_error: Option<FakeFault>,
        inventory_calls: usize,
        before_callback_error: Option<FakeFault>,
        after_callback_error: Option<FakeFault>,
        callbacks: usize,
        restart_frontier: Option<LogSequenceNumber>,
        restart_observations: Vec<DurableTransactionRestartObservation<1>>,
        restart_before_callback_error: Option<FakeFault>,
        restart_after_callback_error: Option<FakeFault>,
        restart_callbacks: usize,
        restart_events: Option<Rc<RefCell<Vec<&'static str>>>>,
    }

    impl FakeDurablePageRecoverySource {
        fn new(
            lineage: LogLineage,
            mut physical: Vec<DurablePageWalObservation<1>>,
            mut owned: Vec<DurableTransactionPageObservation<1>>,
            commits: Vec<DurableTransactionCommitObservation>,
        ) -> Self {
            physical.sort_by_key(DurablePageWalObservation::page_number);
            owned.sort_by_key(|observation| observation.page().page_number());
            let mut inventory = owned
                .iter()
                .map(|observation| observation.page().page_number())
                .collect::<Vec<_>>();
            inventory.sort_unstable();
            inventory.dedup();
            Self {
                lineage,
                physical,
                owned,
                commits,
                inventory,
                inventory_error: None,
                inventory_calls: 0,
                before_callback_error: None,
                after_callback_error: None,
                callbacks: 0,
                restart_frontier: None,
                restart_observations: Vec::new(),
                restart_before_callback_error: None,
                restart_after_callback_error: None,
                restart_callbacks: 0,
                restart_events: None,
            }
        }
    }

    impl DurableTransactionPageRecoveryInventory<1> for FakeDurablePageRecoverySource {
        type Error = FakeFault;

        fn durable_transaction_page_numbers(&mut self) -> Result<Vec<PageNumber>, Self::Error> {
            self.inventory_calls += 1;
            match self.inventory_error.take() {
                Some(source) => Err(source),
                None => Ok(self.inventory.clone()),
            }
        }
    }

    impl DurableTransactionRestartAnalysisSource<1> for FakeDurablePageRecoverySource {
        type Error = FakeFault;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn with_durable_transaction_restart_observations<Output, Operation>(
            &mut self,
            operation: Operation,
        ) -> Result<Output, Self::Error>
        where
            Operation: for<'evidence> FnOnce(
                Option<&'evidence LogSequenceNumber>,
                &'evidence [DurableTransactionRestartObservation<1>],
            ) -> Output,
        {
            if let Some(source) = self.restart_before_callback_error.take() {
                return Err(source);
            }
            if let Some(events) = &self.restart_events {
                events.borrow_mut().push("wal");
            }
            self.restart_callbacks += 1;
            let output = operation(self.restart_frontier.as_ref(), &self.restart_observations);
            match self.restart_after_callback_error.take() {
                Some(source) => Err(source),
                None => Ok(output),
            }
        }
    }

    impl DurableTransactionPageRecoverySource<1> for FakeDurablePageRecoverySource {
        type Error = FakeFault;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn with_durable_page_evidence<Output, Operation>(
            &mut self,
            page_number: PageNumber,
            operation: Operation,
        ) -> Result<Output, Self::Error>
        where
            Operation: for<'evidence> FnOnce(
                &'evidence [DurablePageWalObservation<1>],
                &'evidence [DurableTransactionPageObservation<1>],
                &'evidence [DurableTransactionCommitObservation],
            ) -> Output,
        {
            if let Some(source) = self.before_callback_error.take() {
                return Err(source);
            }
            self.callbacks += 1;
            let physical_start = self
                .physical
                .partition_point(|observation| observation.page_number() < page_number);
            let physical_end = self
                .physical
                .partition_point(|observation| observation.page_number() <= page_number);
            let owned_start = self
                .owned
                .partition_point(|observation| observation.page().page_number() < page_number);
            let owned_end = self
                .owned
                .partition_point(|observation| observation.page().page_number() <= page_number);
            let output = operation(
                &self.physical[physical_start..physical_end],
                &self.owned[owned_start..owned_end],
                &self.commits,
            );
            match self.after_callback_error.take() {
                Some(source) => Err(source),
                None => Ok(output),
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeRecoveryWriteFault {
        Before(FakeFault),
        After(FakeFault),
    }

    struct FakeCommittedPageRecoveryStore {
        lineage: LogLineage,
        current: Option<FakeRecoverySnapshot>,
        observation_fault: Option<FakeFault>,
        write_fault: Option<FakeRecoveryWriteFault>,
        replace_before_compare: Option<FakeRecoverySnapshot>,
        observations: Cell<usize>,
        attempts: usize,
    }

    impl FakeCommittedPageRecoveryStore {
        fn new(lineage: LogLineage, current: Option<FakeRecoverySnapshot>) -> Self {
            Self {
                lineage,
                current,
                observation_fault: None,
                write_fault: None,
                replace_before_compare: None,
                observations: Cell::new(0),
                attempts: 0,
            }
        }

        fn current_observation(
            &self,
        ) -> Result<Option<StoredPageSnapshotObservation<1>>, FakeFault> {
            self.current
                .as_ref()
                .map(FakeRecoverySnapshot::observation)
                .transpose()
        }
    }

    impl CommittedTransactionPageRecoveryStore<1> for FakeCommittedPageRecoveryStore {
        type ObservationError = FakeFault;
        type WriteError = FakeFault;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn observe_page(
            &self,
            _page_number: PageNumber,
        ) -> Result<Option<StoredPageSnapshotObservation<1>>, Self::ObservationError> {
            self.observations.set(self.observations.get() + 1);
            match self.observation_fault {
                Some(source) => Err(source),
                None => self.current_observation(),
            }
        }

        fn compare_and_replace(
            &mut self,
            candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, 1>,
            permit: CommittedTransactionPageRecoveryWritePermit<'_>,
        ) -> Result<(), Self::WriteError> {
            self.attempts += 1;
            let target = candidate.latest_committed();
            if permit.page_position() != target.observation().position()
                || permit.commit_position() != target.commit_position()
            {
                return Err(FakeFault("permit mismatch"));
            }

            if let Some(replacement) = self.replace_before_compare.take() {
                self.current = Some(replacement);
            }
            let current = self.current_observation()?;
            if compare_committed_transaction_page_recovery_candidate(candidate, current.as_ref())
                != Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches)
            {
                return Err(FakeFault("precondition changed"));
            }
            if let Some(FakeRecoveryWriteFault::Before(source)) = self.write_fault {
                self.write_fault = None;
                return Err(source);
            }

            let target = owned_recovery_target(target);
            self.current = Some(FakeRecoverySnapshot::from_target(&target));
            match self.write_fault.take() {
                Some(FakeRecoveryWriteFault::After(source)) => Err(source),
                Some(FakeRecoveryWriteFault::Before(_)) | None => Ok(()),
            }
        }
    }

    struct FakeBatchCommittedPageRecoveryStore {
        lineage: LogLineage,
        current: Vec<FakeRecoverySnapshot>,
        write_fault: Option<(PageNumber, FakeRecoveryWriteFault)>,
        observations: RefCell<Vec<PageNumber>>,
        attempts: Vec<PageNumber>,
    }

    impl FakeBatchCommittedPageRecoveryStore {
        fn new(lineage: LogLineage) -> Self {
            Self {
                lineage,
                current: Vec::new(),
                write_fault: None,
                observations: RefCell::new(Vec::new()),
                attempts: Vec::new(),
            }
        }

        fn current_observation(
            &self,
            page_number: PageNumber,
        ) -> Result<Option<StoredPageSnapshotObservation<1>>, FakeFault> {
            self.current
                .iter()
                .find(|snapshot| snapshot.page_number == page_number)
                .map(FakeRecoverySnapshot::observation)
                .transpose()
        }
    }

    impl CommittedTransactionPageRecoveryStore<1> for FakeBatchCommittedPageRecoveryStore {
        type ObservationError = FakeFault;
        type WriteError = FakeFault;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn observe_page(
            &self,
            page_number: PageNumber,
        ) -> Result<Option<StoredPageSnapshotObservation<1>>, Self::ObservationError> {
            self.observations.borrow_mut().push(page_number);
            self.current_observation(page_number)
        }

        fn compare_and_replace(
            &mut self,
            candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, 1>,
            permit: CommittedTransactionPageRecoveryWritePermit<'_>,
        ) -> Result<(), Self::WriteError> {
            let target = candidate.latest_committed();
            let page_number = target.observation().page().page_number();
            self.attempts.push(page_number);
            if permit.page_position() != target.observation().position()
                || permit.commit_position() != target.commit_position()
            {
                return Err(FakeFault("permit mismatch"));
            }

            let current = self.current_observation(page_number)?;
            if compare_committed_transaction_page_recovery_candidate(candidate, current.as_ref())
                != Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches)
            {
                return Err(FakeFault("precondition changed"));
            }
            if self.write_fault
                == Some((
                    page_number,
                    FakeRecoveryWriteFault::Before(FakeFault("batch write")),
                ))
            {
                self.write_fault = None;
                return Err(FakeFault("batch write"));
            }

            let snapshot = FakeRecoverySnapshot::from_target(&owned_recovery_target(target));
            match self
                .current
                .iter()
                .position(|current| current.page_number == page_number)
            {
                Some(index) => self.current[index] = snapshot,
                None => self.current.push(snapshot),
            }
            if self.write_fault
                == Some((
                    page_number,
                    FakeRecoveryWriteFault::After(FakeFault("batch write")),
                ))
            {
                self.write_fault = None;
                Err(FakeFault("batch write"))
            } else {
                Ok(())
            }
        }
    }

    fn one_page_recovery_source(
        lineage: &LogLineage,
        owner: DurableTransactionIdentityObservation,
        page_number: u64,
        page_version: u64,
        byte: u8,
        page_position: u64,
        commit_position: u64,
    ) -> Result<FakeDurablePageRecoverySource, TestError> {
        Ok(FakeDurablePageRecoverySource::new(
            lineage.clone(),
            vec![physical_page_observation(
                lineage,
                page_number,
                page_version,
                byte,
                page_position,
            )?],
            vec![durable_page_observation(
                lineage,
                owner,
                page_number,
                page_version,
                byte,
                page_position,
            )?],
            vec![durable_commit_observation(lineage, owner, commit_position)?],
        ))
    }

    fn two_page_recovery_source(
        lineage: &LogLineage,
        source_owner: DurableTransactionIdentityObservation,
        target_owner: DurableTransactionIdentityObservation,
        page_number: u64,
    ) -> Result<FakeDurablePageRecoverySource, TestError> {
        Ok(FakeDurablePageRecoverySource::new(
            lineage.clone(),
            vec![
                physical_page_observation(lineage, page_number, 10, 0xA0, 2)?,
                physical_page_observation(lineage, page_number, 1, 0xB0, 5)?,
            ],
            vec![
                durable_page_observation(lineage, source_owner, page_number, 10, 0xA0, 2)?,
                durable_page_observation(lineage, target_owner, page_number, 1, 0xB0, 5)?,
            ],
            vec![
                durable_commit_observation(lineage, source_owner, 3)?,
                durable_commit_observation(lineage, target_owner, 7)?,
            ],
        ))
    }

    fn batch_recovery_source(
        lineage: &LogLineage,
        page_numbers: &[u64],
    ) -> Result<FakeDurablePageRecoverySource, TestError> {
        let mut physical = Vec::new();
        let mut owned = Vec::new();
        let mut commits = Vec::new();
        let mut restart_observations = Vec::new();
        let mut restart_frontier = None;
        for (index, page_number) in page_numbers.iter().copied().enumerate() {
            let sequence = u64::try_from(index + 1).map_err(|_| TestError("batch sequence"))?;
            let page_position = sequence
                .checked_mul(2)
                .and_then(|value| value.checked_sub(1))
                .ok_or(TestError("batch page position"))?;
            let commit_position = page_position
                .checked_add(1)
                .ok_or(TestError("batch commit position"))?;
            let byte = u8::try_from(page_number).map_err(|_| TestError("batch page byte"))?;
            let owner = durable_identity(40, sequence)?;
            physical.push(physical_page_observation(
                lineage,
                page_number,
                sequence,
                byte,
                page_position,
            )?);
            owned.push(durable_page_observation(
                lineage,
                owner,
                page_number,
                sequence,
                byte,
                page_position,
            )?);
            commits.push(durable_commit_observation(lineage, owner, commit_position)?);
            restart_observations.push(restart_owned_page(
                lineage,
                owner,
                page_number,
                page_position,
            )?);
            restart_observations.push(restart_commit(lineage, owner, commit_position)?);
            restart_frontier = Some(lineage.position(commit_position));
        }
        let mut source =
            FakeDurablePageRecoverySource::new(lineage.clone(), physical, owned, commits);
        source.restart_frontier = restart_frontier;
        source.restart_observations = restart_observations;
        Ok(source)
    }

    #[test]
    fn successful_staging_retains_active_and_owns_dirty() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        let page = make_page(&lineage, 7, 0, 0xAB)?;

        let (active, dirty) = coordinator
            .stage_page_write(active, page, &mut log)
            .map_err(|_| TestError("stage rejected"))?;

        assert_eq!(active.transaction_id(), transaction_id);
        assert_eq!(dirty.transaction_id(), transaction_id);
        assert_eq!(dirty.address().number().get(), 7);
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::Active)
        );
        assert_eq!(log.appended_pages, vec![(transaction_id, 7)]);
        assert!(log.flushed.is_empty());
        Ok(())
    }

    #[test]
    fn two_pages_thread_active_then_commit_succeeds() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();

        let first = make_page(&lineage, 1, 0, 0x01)?;
        let (active, first_dirty) = coordinator
            .stage_page_write(active, first, &mut log)
            .map_err(|_| TestError("first stage rejected"))?;

        let second = make_page(&lineage, 2, 0, 0x02)?;
        let (active, second_dirty) = coordinator
            .stage_page_write(active, second, &mut log)
            .map_err(|_| TestError("second stage rejected"))?;

        assert_ne!(
            first_dirty.required_position().get(),
            second_dirty.required_position().get()
        );
        assert_eq!(
            log.appended_pages,
            vec![(transaction_id, 1), (transaction_id, 2)]
        );

        let committed = coordinator
            .commit(active, &mut log)
            .map_err(|_| TestError("commit failed"))?;
        assert_eq!(committed.transaction_id(), transaction_id);
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::Committed)
        );
        Ok(())
    }

    #[test]
    fn second_image_for_same_page_is_rejected_before_append() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();

        let first = make_page(&lineage, 12, 1, 0x12)?;
        let (active, first_dirty) = coordinator
            .stage_page_write(active, first, &mut log)
            .map_err(|_| TestError("first stage rejected"))?;
        let second = make_page(&lineage, 12, 2, 0x13)?;

        let error = coordinator
            .stage_page_write(active, second, &mut log)
            .err()
            .ok_or(TestError("expected duplicate-page rejection"))?;
        let TransactionPageStageError::Rejected(rejection) = error else {
            return Err(TestError("expected pre-append rejection"));
        };
        assert_eq!(
            rejection.reason(),
            TransactionPageStageRejectionReason::PageAlreadyStaged
        );
        assert_eq!(rejection.page().version(), PageVersion::new(2));
        assert_eq!(rejection.page().image().bytes(), &[0x13]);
        assert_eq!(log.appended_pages, vec![(transaction_id, 12)]);
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::Active)
        );

        let (active, _) = rejection.into_parts();
        let committed = coordinator
            .commit(active, &mut log)
            .map_err(|_| TestError("commit after duplicate rejection failed"))?;
        assert_eq!(committed.transaction_id(), transaction_id);
        drop(first_dirty);
        Ok(())
    }

    #[test]
    fn dropping_dirty_wrapper_before_commit_writes_nothing() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        let store: FakeStore = FakeStore::new(lineage.clone());
        let active = coordinator.begin()?;
        let page = make_page(&lineage, 3, 0, 0x03)?;

        let (_active, dirty) = coordinator
            .stage_page_write(active, page, &mut log)
            .map_err(|_| TestError("stage rejected"))?;
        drop(dirty);

        assert!(PageStore::<1>::lineage(&store).same_lineage(&lineage));
        assert!(store.writes.is_empty());
        assert!(log.flushed.is_empty());
        Ok(())
    }

    #[test]
    fn foreign_coordinator_retains_active_and_page() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut owner = open_coordinator(&lineage)?;
        let mut other = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        let foreign = other.begin()?;
        let transaction_id = foreign.transaction_id();
        let page = make_page(&lineage, 4, 0, 0x04)?;

        let error = owner
            .stage_page_write(foreign, page, &mut log)
            .err()
            .ok_or(TestError("expected rejection"))?;
        let TransactionPageStageError::Rejected(rejection) = error else {
            return Err(TestError("expected pre-append rejection"));
        };
        assert_eq!(
            rejection.reason(),
            TransactionPageStageRejectionReason::ForeignCoordinator
        );
        assert_eq!(rejection.transaction_id(), transaction_id);
        assert_eq!(rejection.page().address().number().get(), 4);
        assert!(log.appended_pages.is_empty());
        Ok(())
    }

    #[test]
    fn foreign_log_lineage_retains_active_and_page() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let other_lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(other_lineage);
        let active = coordinator.begin()?;
        let page = make_page(&lineage, 5, 0, 0x05)?;

        let error = coordinator
            .stage_page_write(active, page, &mut log)
            .err()
            .ok_or(TestError("expected rejection"))?;
        let TransactionPageStageError::Rejected(rejection) = error else {
            return Err(TestError("expected pre-append rejection"));
        };
        assert_eq!(
            rejection.reason(),
            TransactionPageStageRejectionReason::ForeignLogLineage
        );
        assert!(log.appended_pages.is_empty());
        Ok(())
    }

    #[test]
    fn foreign_page_lineage_retains_active_and_page() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign_page_lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        let active = coordinator.begin()?;
        let page = make_page(&foreign_page_lineage, 6, 0, 0x06)?;

        let error = coordinator
            .stage_page_write(active, page, &mut log)
            .err()
            .ok_or(TestError("expected rejection"))?;
        let TransactionPageStageError::Rejected(rejection) = error else {
            return Err(TestError("expected pre-append rejection"));
        };
        assert_eq!(
            rejection.reason(),
            TransactionPageStageRejectionReason::ForeignPageLineage
        );
        assert!(log.appended_pages.is_empty());
        Ok(())
    }

    #[test]
    fn lifecycle_mismatch_retains_active_and_page() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        // Drive the registry out of Active without consuming the token.
        if let Some(status) = coordinator.lifecycles.get_mut(&transaction_id) {
            *status = TransactionLifecycleStatus::Committed;
        }
        let page = make_page(&lineage, 8, 0, 0x08)?;

        let error = coordinator
            .stage_page_write(active, page, &mut log)
            .err()
            .ok_or(TestError("expected rejection"))?;
        let TransactionPageStageError::Rejected(rejection) = error else {
            return Err(TestError("expected pre-append rejection"));
        };
        assert_eq!(
            rejection.reason(),
            TransactionPageStageRejectionReason::LifecycleMismatch {
                actual: Some(TransactionLifecycleStatus::Committed)
            }
        );
        assert!(log.appended_pages.is_empty());
        Ok(())
    }

    #[test]
    fn append_source_failure_is_terminal_page_indeterminate() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        log.append_page_fault = Some(FakeFault("append boom"));
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        let page = make_page(&lineage, 9, 0, 0x09)?;

        let error = coordinator
            .stage_page_write(active, page, &mut log)
            .err()
            .ok_or(TestError("expected terminal error"))?;
        let TransactionPageStageError::Append(append) = error else {
            return Err(TestError("expected append error"));
        };
        assert_eq!(append.transaction_id(), transaction_id);
        assert_eq!(append.cause(), &FakeFault("append boom"));
        assert_eq!(append.page().address().number().get(), 9);
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::PageAppendIndeterminate)
        );
        Ok(())
    }

    #[test]
    fn foreign_returned_position_is_terminal_and_blocks_commit_and_resolve() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        log.foreign_append_lineage = Some(LogLineage::new());
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        let page = make_page(&lineage, 10, 0, 0x0A)?;

        let error = coordinator
            .stage_page_write(active, page, &mut log)
            .err()
            .ok_or(TestError("expected terminal error"))?;
        let TransactionPageStageError::InvalidEvidence(evidence) = error else {
            return Err(TestError("expected invalid evidence"));
        };
        assert_eq!(evidence.transaction_id(), transaction_id);
        assert_eq!(
            evidence.reason(),
            StagePageWriteEvidenceErrorReason::ForeignPosition
        );
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::PageAppendIndeterminate)
        );

        // Commit is impossible without an active token (it was consumed on the
        // terminal path), and resolve must not reinterpret the distinct
        // page-append phase as a commit-indeterminate attempt.
        let indeterminate = IndeterminateTransaction {
            transaction_id,
            coordinator_identity: Arc::clone(&coordinator.identity),
            log_lineage: lineage.clone(),
        };
        let mut recovery = TestRecoverySource {
            lineage: lineage.clone(),
            calls: 0,
        };
        let resolution_error = coordinator
            .resolve(indeterminate, &mut recovery)
            .err()
            .ok_or(TestError("resolve should reject page-append phase"))?;
        assert_eq!(
            resolution_error.failure(),
            &TransactionResolutionFailure::LifecycleMismatch {
                actual: Some(TransactionLifecycleStatus::PageAppendIndeterminate)
            }
        );
        assert_eq!(recovery.calls, 0);
        Ok(())
    }

    #[test]
    fn append_time_lineage_rotation_is_terminal_page_indeterminate() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        log.rotate_after_append = Some(LogLineage::new());
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        let page = make_page(&lineage, 11, 0, 0x0B)?;

        let error = coordinator
            .stage_page_write(active, page, &mut log)
            .err()
            .ok_or(TestError("expected terminal error"))?;
        let TransactionPageStageError::InvalidEvidence(evidence) = error else {
            return Err(TestError("expected invalid evidence"));
        };
        assert_eq!(
            evidence.reason(),
            StagePageWriteEvidenceErrorReason::LogLineageChanged
        );
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::PageAppendIndeterminate)
        );
        Ok(())
    }

    fn stage_and_commit(
        lineage: &LogLineage,
    ) -> Result<
        (
            TransactionId,
            CommittedTransaction,
            TransactionDirtyPage<1>,
            FakeLog,
        ),
        TestError,
    > {
        let mut coordinator = open_coordinator(lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        let page = make_page(lineage, 21, 0, 0x21)?;
        let (active, dirty) = coordinator
            .stage_page_write(active, page, &mut log)
            .map_err(|_| TestError("stage rejected"))?;
        let committed = coordinator
            .commit(active, &mut log)
            .map_err(|_| TestError("commit failed"))?;
        Ok((transaction_id, committed, dirty, log))
    }

    #[test]
    fn committed_flush_preserves_order_and_returns_clean_page() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let (transaction_id, committed, dirty, mut log) = stage_and_commit(&lineage)?;
        let page_position = dirty.required_position().get();
        assert!(committed.log_position().get() > page_position);

        let events = Rc::new(RefCell::new(Vec::new()));
        log.events = Some(Rc::clone(&events));
        let mut store = FakeStore::new(lineage.clone());
        store.events = Some(Rc::clone(&events));

        let clean = flush_committed_page(&committed, &mut log, &mut store, dirty)
            .map_err(|_| TestError("committed flush failed"))?;

        assert_eq!(clean.transaction_id(), transaction_id);
        assert_eq!(clean.required_position().get(), page_position);
        assert_eq!(
            events.borrow().as_slice(),
            &[
                FlushEvent::Flush(page_position),
                FlushEvent::Write(page_position)
            ]
        );
        Ok(())
    }

    #[test]
    fn committed_flush_wrong_transaction_retains_wrapper() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());

        let page_owner = coordinator.begin()?;
        let page = make_page(&lineage, 30, 0, 0x30)?;
        let (_page_owner, dirty) = coordinator
            .stage_page_write(page_owner, page, &mut log)
            .map_err(|_| TestError("stage rejected"))?;

        let committer = coordinator.begin()?;
        let committed = coordinator
            .commit(committer, &mut log)
            .map_err(|_| TestError("commit failed"))?;
        assert_ne!(committed.transaction_id(), dirty.transaction_id());

        let mut flush_log = FakeLog::new(lineage.clone());
        let mut store = FakeStore::new(lineage.clone());
        let error = flush_committed_page(&committed, &mut flush_log, &mut store, dirty)
            .err()
            .ok_or(TestError("expected rejection"))?;
        let TransactionCommittedFlushError::Rejected(rejection) = error else {
            return Err(TestError("expected pre-port rejection"));
        };
        assert_eq!(
            rejection.reason(),
            TransactionCommittedFlushRejectionReason::WrongTransaction
        );
        assert_eq!(rejection.page().address().number().get(), 30);
        assert!(flush_log.flushed.is_empty());
        assert!(store.writes.is_empty());
        Ok(())
    }

    #[test]
    fn committed_flush_foreign_commit_lineage_retains_wrapper() -> Result<(), TestError> {
        // Two coordinators over different lineages issue the same TransactionId,
        // so identity equality alone would wrongly accept the flush.
        let page_lineage = LogLineage::new();
        let commit_lineage = LogLineage::new();

        let mut page_coordinator = open_coordinator(&page_lineage)?;
        let mut page_log = FakeLog::new(page_lineage.clone());
        let page_active = page_coordinator.begin()?;
        let page = make_page(&page_lineage, 31, 0, 0x31)?;
        let (_page_active, dirty) = page_coordinator
            .stage_page_write(page_active, page, &mut page_log)
            .map_err(|_| TestError("stage rejected"))?;

        let mut commit_coordinator = open_coordinator(&commit_lineage)?;
        let mut commit_log = FakeLog::new(commit_lineage.clone());
        let commit_active = commit_coordinator.begin()?;
        let committed = commit_coordinator
            .commit(commit_active, &mut commit_log)
            .map_err(|_| TestError("commit failed"))?;
        assert_eq!(committed.transaction_id(), dirty.transaction_id());

        let mut flush_log = FakeLog::new(page_lineage.clone());
        let mut store = FakeStore::new(page_lineage.clone());
        let error = flush_committed_page(&committed, &mut flush_log, &mut store, dirty)
            .err()
            .ok_or(TestError("expected rejection"))?;
        let TransactionCommittedFlushError::Rejected(rejection) = error else {
            return Err(TestError("expected pre-port rejection"));
        };
        assert_eq!(
            rejection.reason(),
            TransactionCommittedFlushRejectionReason::ForeignCommitLineage
        );
        assert!(flush_log.flushed.is_empty());
        assert!(store.writes.is_empty());
        Ok(())
    }

    #[test]
    fn committed_flush_position_not_after_page_retains_wrapper() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let mut log = FakeLog::new(lineage.clone());
        // Force the page append and the commit append onto the same position so
        // the strict-after check rejects an equal position.
        log.next_position = 4;
        let active = coordinator.begin()?;
        let page = make_page(&lineage, 32, 0, 0x32)?;
        let (active, dirty) = coordinator
            .stage_page_write(active, page, &mut log)
            .map_err(|_| TestError("stage rejected"))?;
        assert_eq!(dirty.required_position().get(), 4);
        log.next_position = 4;
        let committed = coordinator
            .commit(active, &mut log)
            .map_err(|_| TestError("commit failed"))?;
        assert_eq!(committed.log_position().get(), 4);

        let mut flush_log = FakeLog::new(lineage.clone());
        let mut store = FakeStore::new(lineage.clone());
        let error = flush_committed_page(&committed, &mut flush_log, &mut store, dirty)
            .err()
            .ok_or(TestError("expected rejection"))?;
        let TransactionCommittedFlushError::Rejected(rejection) = error else {
            return Err(TestError("expected pre-port rejection"));
        };
        assert_eq!(
            rejection.reason(),
            TransactionCommittedFlushRejectionReason::CommitPositionNotAfterPage
        );
        assert!(flush_log.flushed.is_empty());
        assert!(store.writes.is_empty());
        Ok(())
    }

    #[test]
    fn committed_flush_wal_failure_retains_retryable_dirty() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let (transaction_id, committed, dirty, mut flush_log) = stage_and_commit(&lineage)?;

        flush_log.flush_fault = Some(FakeFault("flush boom"));
        let mut store = FakeStore::new(lineage.clone());
        let error = flush_committed_page(&committed, &mut flush_log, &mut store, dirty)
            .err()
            .ok_or(TestError("expected log flush error"))?;
        let TransactionCommittedFlushError::LogFlush(log_error) = error else {
            return Err(TestError("expected log flush error"));
        };
        assert_eq!(log_error.page().transaction_id(), transaction_id);
        assert_eq!(log_error.cause(), &FakeFault("flush boom"));
        assert!(store.writes.is_empty());
        Ok(())
    }

    #[test]
    fn committed_flush_store_failure_is_terminal_indeterminate() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let (transaction_id, committed, dirty, mut flush_log) = stage_and_commit(&lineage)?;
        let page_position = dirty.required_position().get();

        let mut store = FakeStore::new(lineage.clone());
        store.write_fault = Some(FakeFault("store boom"));
        let error = flush_committed_page(&committed, &mut flush_log, &mut store, dirty)
            .err()
            .ok_or(TestError("expected store error"))?;
        let TransactionCommittedFlushError::StoreWrite(store_error) = error else {
            return Err(TestError("expected store write error"));
        };
        assert_eq!(store_error.transaction_id(), transaction_id);
        assert_eq!(store_error.cause(), &FakeFault("store boom"));
        assert_eq!(store_error.page().required_position().get(), page_position);
        // The commit first made the shared prefix durable, then the page path
        // repeated the lower fence before the store failed.
        assert_eq!(flush_log.flushed.last(), Some(&page_position));
        Ok(())
    }

    #[test]
    fn persisted_identity_validates_fields_and_only_compares_with_live_identity()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut coordinator = open_coordinator(&lineage)?;
        let transaction = coordinator.begin()?;
        let observed = durable_identity(1, 1)?;
        let other = durable_identity(1, 2)?;

        assert_eq!(observed.epoch(), 1);
        assert_eq!(observed.sequence(), 1);
        assert!(observed.matches_transaction_id(transaction.transaction_id()));
        assert!(!other.matches_transaction_id(transaction.transaction_id()));

        let zero_epoch = DurableTransactionIdentityObservation::new(0, 9)
            .err()
            .ok_or(TestError("zero epoch must fail"))?;
        assert_eq!(zero_epoch.epoch(), 0);
        assert_eq!(zero_epoch.sequence(), 9);
        assert_eq!(
            zero_epoch.reason(),
            DurableTransactionIdentityObservationErrorReason::ZeroEpoch
        );
        assert_eq!(
            zero_epoch.into_parts(),
            (
                0,
                9,
                DurableTransactionIdentityObservationErrorReason::ZeroEpoch
            )
        );

        let zero_sequence = DurableTransactionIdentityObservation::new(7, 0)
            .err()
            .ok_or(TestError("zero sequence must fail"))?;
        assert_eq!(zero_sequence.epoch(), 7);
        assert_eq!(zero_sequence.sequence(), 0);
        assert_eq!(
            zero_sequence.reason(),
            DurableTransactionIdentityObservationErrorReason::ZeroSequence
        );
        Ok(())
    }

    #[test]
    fn durable_commit_observation_retains_rejected_zero_position() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let transaction = durable_identity(3, 4)?;

        let error = DurableTransactionCommitObservation::new(transaction, lineage.position(0))
            .err()
            .ok_or(TestError("zero commit position must fail"))?;

        assert_eq!(error.transaction(), transaction);
        assert_eq!(error.position().get(), 0);
        let (retained_transaction, retained_position) = error.into_parts();
        assert_eq!(retained_transaction, transaction);
        assert_eq!(retained_position.get(), 0);
        assert!(retained_position.lineage().same_lineage(&lineage));
        Ok(())
    }

    #[test]
    fn raw_owned_page_constructor_retains_every_input_on_each_failure() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let number = PageNumber::new(39).ok_or(TestError("durable page number"))?;
        let version = PageVersion::new(6);

        let valid = DurableTransactionPageObservation::from_bytes(
            3,
            4,
            number,
            version,
            [7_u8, 8],
            lineage.position(9),
        )
        .map_err(|_| TestError("valid raw owned page"))?;
        assert_eq!(valid.owner().epoch(), 3);
        assert_eq!(valid.owner().sequence(), 4);
        assert_eq!(valid.page().page_number(), number);
        assert_eq!(valid.page().page_version(), version);
        assert_eq!(valid.page().image().bytes(), &[7_u8, 8]);
        assert_eq!(valid.position(), &lineage.position(9));

        let identity_error = DurableTransactionPageObservation::from_bytes(
            0,
            5,
            number,
            version,
            [9_u8, 10],
            lineage.position(11),
        )
        .err()
        .ok_or(TestError("zero owner epoch must fail"))?;
        assert_eq!(identity_error.epoch(), 0);
        assert_eq!(identity_error.sequence(), 5);
        assert_eq!(identity_error.page_number(), number);
        assert_eq!(identity_error.page_version(), version);
        assert_eq!(identity_error.bytes(), &[9_u8, 10]);
        assert_eq!(identity_error.position(), &lineage.position(11));
        assert_eq!(
            identity_error.reason(),
            DurableTransactionPageObservationBytesErrorReason::Identity(
                DurableTransactionIdentityObservationErrorReason::ZeroEpoch
            )
        );
        assert_eq!(
            identity_error.into_parts(),
            (
                0,
                5,
                number,
                version,
                [9_u8, 10],
                lineage.position(11),
                DurableTransactionPageObservationBytesErrorReason::Identity(
                    DurableTransactionIdentityObservationErrorReason::ZeroEpoch
                ),
            )
        );

        let zero_width = DurableTransactionPageObservation::<0>::from_bytes(
            6,
            7,
            number,
            version,
            [],
            lineage.position(12),
        )
        .err()
        .ok_or(TestError("zero page width must fail"))?;
        assert_eq!(zero_width.epoch(), 6);
        assert_eq!(zero_width.sequence(), 7);
        assert_eq!(zero_width.page_number(), number);
        assert_eq!(zero_width.page_version(), version);
        assert_eq!(zero_width.bytes(), &[]);
        assert_eq!(zero_width.position(), &lineage.position(12));
        assert_eq!(
            zero_width.reason(),
            DurableTransactionPageObservationBytesErrorReason::Page(
                PageRecoveryObservationBytesErrorReason::ZeroPageWidth
            )
        );

        let zero_position = DurableTransactionPageObservation::from_bytes(
            8,
            9,
            number,
            version,
            [11_u8, 12],
            lineage.position(0),
        )
        .err()
        .ok_or(TestError("zero page position must fail"))?;
        assert_eq!(zero_position.epoch(), 8);
        assert_eq!(zero_position.sequence(), 9);
        assert_eq!(zero_position.page_number(), number);
        assert_eq!(zero_position.page_version(), version);
        assert_eq!(zero_position.bytes(), &[11_u8, 12]);
        assert_eq!(zero_position.position(), &lineage.position(0));
        assert_eq!(
            zero_position.reason(),
            DurableTransactionPageObservationBytesErrorReason::Page(
                PageRecoveryObservationBytesErrorReason::ZeroPosition
            )
        );
        Ok(())
    }

    #[test]
    fn raw_commit_constructor_retains_identity_and_position_failures() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let valid = DurableTransactionCommitObservation::from_fields(10, 11, lineage.position(13))
            .map_err(|_| TestError("valid raw commit"))?;
        assert_eq!(valid.transaction().epoch(), 10);
        assert_eq!(valid.transaction().sequence(), 11);
        assert_eq!(valid.position(), &lineage.position(13));

        let zero_epoch =
            DurableTransactionCommitObservation::from_fields(0, 12, lineage.position(14))
                .err()
                .ok_or(TestError("zero commit epoch must fail"))?;
        assert_eq!(zero_epoch.epoch(), 0);
        assert_eq!(zero_epoch.sequence(), 12);
        assert_eq!(zero_epoch.position(), &lineage.position(14));
        assert_eq!(
            zero_epoch.reason(),
            DurableTransactionCommitObservationFieldsErrorReason::Identity(
                DurableTransactionIdentityObservationErrorReason::ZeroEpoch
            )
        );

        let zero_sequence =
            DurableTransactionCommitObservation::from_fields(13, 0, lineage.position(15))
                .err()
                .ok_or(TestError("zero commit sequence must fail"))?;
        assert_eq!(zero_sequence.epoch(), 13);
        assert_eq!(zero_sequence.sequence(), 0);
        assert_eq!(zero_sequence.position(), &lineage.position(15));
        assert_eq!(
            zero_sequence.reason(),
            DurableTransactionCommitObservationFieldsErrorReason::Identity(
                DurableTransactionIdentityObservationErrorReason::ZeroSequence
            )
        );

        let zero_position =
            DurableTransactionCommitObservation::from_fields(14, 15, lineage.position(0))
                .err()
                .ok_or(TestError("zero commit position must fail"))?;
        assert_eq!(zero_position.epoch(), 14);
        assert_eq!(zero_position.sequence(), 15);
        assert_eq!(zero_position.position(), &lineage.position(0));
        assert_eq!(
            zero_position.reason(),
            DurableTransactionCommitObservationFieldsErrorReason::ZeroPosition
        );
        assert_eq!(
            zero_position.into_parts(),
            (
                14,
                15,
                lineage.position(0),
                DurableTransactionCommitObservationFieldsErrorReason::ZeroPosition,
            )
        );
        Ok(())
    }

    #[test]
    fn complete_commit_prefix_classifies_committed_and_uncommitted_pages() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let owner = durable_identity(5, 8)?;
        let unrelated = durable_identity(5, 9)?;
        let page = durable_page_observation(&lineage, owner, 40, 2, 0x40, 2)?;
        let commits = [
            durable_commit_observation(&lineage, unrelated, 3)?,
            durable_commit_observation(&lineage, owner, 5)?,
        ];

        let committed = classify_durable_transaction_page(&lineage, &page, commits.iter());
        let empty = classify_durable_transaction_page(
            &lineage,
            &page,
            std::iter::empty::<&DurableTransactionCommitObservation>(),
        );
        let unrelated_only =
            classify_durable_transaction_page(&lineage, &page, commits[..1].iter());

        assert_eq!(
            committed,
            Ok(DurableTransactionPageCommitClassification::Committed {
                page_position: lineage.position(2),
                commit_position: lineage.position(5),
            })
        );
        assert_eq!(
            empty,
            Ok(DurableTransactionPageCommitClassification::Uncommitted {
                page_position: lineage.position(2),
            })
        );
        assert_eq!(
            unrelated_only,
            Ok(DurableTransactionPageCommitClassification::Uncommitted {
                page_position: lineage.position(2),
            })
        );
        assert_eq!(page.owner(), owner);
        assert_eq!(page.page().page_number().get(), 40);
        Ok(())
    }

    #[test]
    fn duplicate_matching_commit_fails_closed_before_choosing_a_position() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let owner = durable_identity(6, 1)?;
        let page = durable_page_observation(&lineage, owner, 41, 0, 0x41, 2)?;
        let commits = [
            durable_commit_observation(&lineage, owner, 4)?,
            durable_commit_observation(&lineage, owner, 7)?,
        ];
        let same_position = [
            durable_commit_observation(&lineage, owner, 5)?,
            durable_commit_observation(&lineage, owner, 5)?,
        ];
        let decreasing = [
            durable_commit_observation(&lineage, owner, 9)?,
            durable_commit_observation(&lineage, owner, 6)?,
        ];

        let result = classify_durable_transaction_page(&lineage, &page, commits.iter());
        let same_position_result =
            classify_durable_transaction_page(&lineage, &page, same_position.iter());
        let decreasing_result =
            classify_durable_transaction_page(&lineage, &page, decreasing.iter());

        assert_eq!(
            result,
            Err(
                DurableTransactionPageClassificationError::DuplicateMatchingCommit {
                    transaction: owner,
                    first: lineage.position(4),
                    duplicate: lineage.position(7),
                }
            )
        );
        assert_eq!(
            same_position_result,
            Err(
                DurableTransactionPageClassificationError::DuplicateMatchingCommit {
                    transaction: owner,
                    first: lineage.position(5),
                    duplicate: lineage.position(5),
                }
            )
        );
        assert_eq!(
            decreasing_result,
            Err(
                DurableTransactionPageClassificationError::DuplicateMatchingCommit {
                    transaction: owner,
                    first: lineage.position(9),
                    duplicate: lineage.position(6),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn matching_commit_must_be_strictly_after_the_owned_page() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let owner = durable_identity(7, 1)?;
        let page = durable_page_observation(&lineage, owner, 42, 0, 0x42, 5)?;
        let earlier = [durable_commit_observation(&lineage, owner, 3)?];
        let equal = [durable_commit_observation(&lineage, owner, 5)?];

        let earlier_result = classify_durable_transaction_page(&lineage, &page, earlier.iter());
        let equal_result = classify_durable_transaction_page(&lineage, &page, equal.iter());

        assert_eq!(
            earlier_result,
            Err(
                DurableTransactionPageClassificationError::CommitNotAfterPage {
                    transaction: owner,
                    page_position: lineage.position(5),
                    commit_position: lineage.position(3),
                }
            )
        );
        assert_eq!(
            equal_result,
            Err(
                DurableTransactionPageClassificationError::CommitNotAfterPage {
                    transaction: owner,
                    page_position: lineage.position(5),
                    commit_position: lineage.position(5),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn foreign_page_and_complete_prefix_commit_lineages_fail() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let owner = durable_identity(8, 1)?;
        let foreign_page = durable_page_observation(&foreign, owner, 43, 0, 0x43, 2)?;
        let page = durable_page_observation(&lineage, owner, 43, 0, 0x43, 2)?;
        let commits = [
            durable_commit_observation(&lineage, owner, 4)?,
            durable_commit_observation(&foreign, durable_identity(8, 2)?, 5)?,
        ];

        let page_result = classify_durable_transaction_page(
            &lineage,
            &foreign_page,
            std::iter::empty::<&DurableTransactionCommitObservation>(),
        );
        let commit_result = classify_durable_transaction_page(&lineage, &page, commits.iter());

        assert_eq!(
            page_result,
            Err(
                DurableTransactionPageClassificationError::ForeignPageLineage {
                    position: foreign.position(2),
                }
            )
        );
        assert_eq!(
            commit_result,
            Err(
                DurableTransactionPageClassificationError::ForeignCommitLineage {
                    position: foreign.position(5),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn commit_prefix_position_shape_is_validated_for_unrelated_identities() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let owner = durable_identity(9, 1)?;
        let first = durable_identity(9, 2)?;
        let second = durable_identity(9, 3)?;
        let page = durable_page_observation(&lineage, owner, 44, 0, 0x44, 1)?;
        let duplicate = [
            durable_commit_observation(&lineage, first, 4)?,
            durable_commit_observation(&lineage, first, 4)?,
        ];
        let contradictory = [
            durable_commit_observation(&lineage, first, 5)?,
            durable_commit_observation(&lineage, second, 5)?,
        ];
        let decreasing = [
            durable_commit_observation(&lineage, first, 8)?,
            durable_commit_observation(&lineage, second, 6)?,
        ];

        assert_eq!(
            classify_durable_transaction_page(&lineage, &page, duplicate.iter()),
            Err(
                DurableTransactionPageClassificationError::DuplicateCommitPosition {
                    position: lineage.position(4),
                }
            )
        );
        assert_eq!(
            classify_durable_transaction_page(&lineage, &page, contradictory.iter()),
            Err(
                DurableTransactionPageClassificationError::ContradictoryCommitPosition {
                    position: lineage.position(5),
                }
            )
        );
        assert_eq!(
            classify_durable_transaction_page(&lineage, &page, decreasing.iter()),
            Err(
                DurableTransactionPageClassificationError::NonAdvancingCommitPosition {
                    previous: lineage.position(8),
                    actual: lineage.position(6),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn same_numeric_identity_from_another_lineage_is_not_commit_evidence() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let owner = durable_identity(1, 1)?;
        let page = durable_page_observation(&lineage, owner, 45, 0, 0x45, 2)?;
        let commits = [durable_commit_observation(&foreign, owner, 4)?];

        let result = classify_durable_transaction_page(&lineage, &page, commits.iter());

        assert_eq!(
            result,
            Err(
                DurableTransactionPageClassificationError::ForeignCommitLineage {
                    position: foreign.position(4),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn repeated_owned_page_records_are_classified_independently() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let owner = durable_identity(10, 1)?;
        let first = durable_page_observation(&lineage, owner, 46, 1, 0x46, 2)?;
        let second = durable_page_observation(&lineage, owner, 46, 2, 0x47, 3)?;
        let commits = [durable_commit_observation(&lineage, owner, 6)?];

        let first_result = classify_durable_transaction_page(&lineage, &first, commits.iter());
        let second_result = classify_durable_transaction_page(&lineage, &second, commits.iter());

        assert_eq!(
            first_result,
            Ok(DurableTransactionPageCommitClassification::Committed {
                page_position: lineage.position(2),
                commit_position: lineage.position(6),
            })
        );
        assert_eq!(
            second_result,
            Ok(DurableTransactionPageCommitClassification::Committed {
                page_position: lineage.position(3),
                commit_position: lineage.position(6),
            })
        );
        Ok(())
    }

    #[test]
    fn latest_selection_validates_empty_and_all_uncommitted_inputs() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let page_number = PageNumber::new(47).ok_or(TestError("page number"))?;
        let first_owner = durable_identity(11, 1)?;
        let second_owner = durable_identity(11, 2)?;
        let unrelated = durable_identity(11, 3)?;
        let pages = [
            durable_page_observation(&lineage, first_owner, 47, 1, 0x47, 1)?,
            durable_page_observation(&lineage, second_owner, 47, 2, 0x48, 2)?,
        ];
        let commits = [durable_commit_observation(&lineage, unrelated, 3)?];
        let foreign_commits = [durable_commit_observation(&foreign, unrelated, 4)?];

        let empty = select_latest_committed_transaction_page(
            &lineage,
            page_number,
            std::iter::empty::<&DurableTransactionPageObservation<1>>(),
            &commits,
        );
        let all_uncommitted =
            select_latest_committed_transaction_page(&lineage, page_number, pages.iter(), &commits);
        let malformed_empty = select_latest_committed_transaction_page(
            &lineage,
            page_number,
            std::iter::empty::<&DurableTransactionPageObservation<1>>(),
            &foreign_commits,
        );

        let no_committed = Ok(DurableTransactionPageSelection::NoCommittedPage { page_number });
        assert_eq!(empty, no_committed);
        assert_eq!(all_uncommitted, no_committed);
        assert_eq!(
            malformed_empty,
            Err(DurableTransactionPageSelectionError::CommitPrefix {
                source: Box::new(
                    DurableTransactionPageClassificationError::ForeignCommitLineage {
                        position: foreign.position(4),
                    },
                ),
            })
        );
        Ok(())
    }

    #[test]
    fn latest_selection_uses_page_wal_order_across_owners() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(48).ok_or(TestError("page number"))?;
        let uncommitted_before = durable_identity(12, 1)?;
        let first_committed = durable_identity(12, 2)?;
        let latest_committed = durable_identity(12, 3)?;
        let uncommitted_after = durable_identity(12, 4)?;
        let pages = [
            durable_page_observation(&lineage, uncommitted_before, 48, 3, 0x48, 1)?,
            durable_page_observation(&lineage, first_committed, 48, 10, 0x49, 2)?,
            durable_page_observation(&lineage, latest_committed, 48, 1, 0x4A, 6)?,
            durable_page_observation(&lineage, uncommitted_after, 48, 20, 0x4B, 9)?,
        ];
        let commits = [
            durable_commit_observation(&lineage, first_committed, 5)?,
            durable_commit_observation(&lineage, latest_committed, 8)?,
        ];

        let result =
            select_latest_committed_transaction_page(&lineage, page_number, pages.iter(), &commits)
                .map_err(|_| TestError("latest selection failed"))?;
        let DurableTransactionPageSelection::LatestCommitted(selected) = result else {
            return Err(TestError("expected latest committed page"));
        };

        assert!(std::ptr::eq(selected.observation(), &pages[2]));
        assert_eq!(
            selected.observation().page().page_version(),
            PageVersion::new(1)
        );
        assert_eq!(selected.observation().position(), &lineage.position(6));
        assert_eq!(selected.commit_position(), &lineage.position(8));
        Ok(())
    }

    #[test]
    fn latest_selection_accepts_repeated_owner_records_before_one_commit() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(49).ok_or(TestError("page number"))?;
        let owner = durable_identity(13, 1)?;
        let pages = [
            durable_page_observation(&lineage, owner, 49, 1, 0x49, 2)?,
            durable_page_observation(&lineage, owner, 49, 2, 0x4A, 3)?,
        ];
        let commits = [durable_commit_observation(&lineage, owner, 5)?];

        let one_result = {
            let temporary_commits = [durable_commit_observation(&lineage, owner, 5)?];
            select_latest_committed_transaction_page(
                &lineage,
                page_number,
                pages[..1].iter(),
                &temporary_commits,
            )
        }
        .map_err(|_| TestError("one-page selection failed"))?;
        let DurableTransactionPageSelection::LatestCommitted(one_selected) = one_result else {
            return Err(TestError("expected one committed page"));
        };
        assert!(std::ptr::eq(one_selected.observation(), &pages[0]));
        assert_eq!(one_selected.commit_position(), &lineage.position(5));

        let result =
            select_latest_committed_transaction_page(&lineage, page_number, pages.iter(), &commits)
                .map_err(|_| TestError("repeated-owner selection failed"))?;
        let DurableTransactionPageSelection::LatestCommitted(selected) = result else {
            return Err(TestError("expected repeated-owner selection"));
        };

        assert!(std::ptr::eq(selected.observation(), &pages[1]));
        assert_eq!(selected.commit_position(), &lineage.position(5));
        Ok(())
    }

    #[test]
    fn latest_selection_rejects_page_and_lineage_before_position_order() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let page_number = PageNumber::new(50).ok_or(TestError("page number"))?;
        let owner = durable_identity(14, 1)?;
        let first = durable_page_observation(&lineage, owner, 50, 1, 0x50, 5)?;
        let wrong_page = durable_page_observation(&lineage, owner, 51, 1, 0x51, 4)?;
        let foreign_page = durable_page_observation(&foreign, owner, 50, 1, 0x50, 4)?;
        let wrong_page_records = [first, wrong_page];
        let lineage_first = durable_page_observation(&lineage, owner, 50, 1, 0x50, 5)?;
        let foreign_records = [lineage_first, foreign_page];

        let wrong_page_result = select_latest_committed_transaction_page(
            &lineage,
            page_number,
            wrong_page_records.iter(),
            &[],
        );
        let foreign_result = select_latest_committed_transaction_page(
            &lineage,
            page_number,
            foreign_records.iter(),
            &[],
        );

        assert_eq!(
            wrong_page_result,
            Err(DurableTransactionPageSelectionError::UnexpectedOwnedPage {
                expected: page_number,
                actual: PageNumber::new(51).ok_or(TestError("wrong page number"))?,
                position: lineage.position(4),
            })
        );
        assert_eq!(
            foreign_result,
            Err(
                DurableTransactionPageSelectionError::ForeignOwnedPageLineage {
                    page_number,
                    position: foreign.position(4),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn latest_selection_rejects_owned_page_position_shapes() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(52).ok_or(TestError("page number"))?;
        let owner = durable_identity(15, 1)?;
        let duplicate = [
            durable_page_observation(&lineage, owner, 52, 1, 0x52, 2)?,
            durable_page_observation(&lineage, owner, 52, 1, 0x52, 2)?,
        ];
        let contradictory = [
            durable_page_observation(&lineage, owner, 52, 1, 0x52, 3)?,
            durable_page_observation(&lineage, owner, 52, 2, 0x53, 3)?,
        ];
        let decreasing = [
            durable_page_observation(&lineage, owner, 52, 1, 0x52, 6)?,
            durable_page_observation(&lineage, owner, 52, 2, 0x53, 4)?,
        ];

        assert_eq!(
            select_latest_committed_transaction_page(&lineage, page_number, duplicate.iter(), &[],),
            Err(
                DurableTransactionPageSelectionError::DuplicateOwnedPagePosition {
                    page_number,
                    position: lineage.position(2),
                }
            )
        );
        assert_eq!(
            select_latest_committed_transaction_page(
                &lineage,
                page_number,
                contradictory.iter(),
                &[],
            ),
            Err(
                DurableTransactionPageSelectionError::ContradictoryOwnedPagePosition {
                    page_number,
                    position: lineage.position(3),
                }
            )
        );
        assert_eq!(
            select_latest_committed_transaction_page(&lineage, page_number, decreasing.iter(), &[],),
            Err(
                DurableTransactionPageSelectionError::NonAdvancingOwnedPagePosition {
                    page_number,
                    previous: lineage.position(6),
                    actual: lineage.position(4),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn latest_selection_validates_commit_position_shapes_without_pages() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(53).ok_or(TestError("page number"))?;
        let first = durable_identity(16, 1)?;
        let second = durable_identity(16, 2)?;
        let duplicate = [
            durable_commit_observation(&lineage, first, 4)?,
            durable_commit_observation(&lineage, first, 4)?,
        ];
        let contradictory = [
            durable_commit_observation(&lineage, first, 5)?,
            durable_commit_observation(&lineage, second, 5)?,
        ];
        let decreasing = [
            durable_commit_observation(&lineage, first, 8)?,
            durable_commit_observation(&lineage, second, 6)?,
        ];

        let empty_pages = || std::iter::empty::<&DurableTransactionPageObservation<1>>();
        assert_eq!(
            select_latest_committed_transaction_page(
                &lineage,
                page_number,
                empty_pages(),
                &duplicate,
            ),
            Err(DurableTransactionPageSelectionError::CommitPrefix {
                source: Box::new(
                    DurableTransactionPageClassificationError::DuplicateCommitPosition {
                        position: lineage.position(4),
                    },
                ),
            })
        );
        assert_eq!(
            select_latest_committed_transaction_page(
                &lineage,
                page_number,
                empty_pages(),
                &contradictory,
            ),
            Err(DurableTransactionPageSelectionError::CommitPrefix {
                source: Box::new(
                    DurableTransactionPageClassificationError::ContradictoryCommitPosition {
                        position: lineage.position(5),
                    },
                ),
            })
        );
        assert_eq!(
            select_latest_committed_transaction_page(
                &lineage,
                page_number,
                empty_pages(),
                &decreasing,
            ),
            Err(DurableTransactionPageSelectionError::CommitPrefix {
                source: Box::new(
                    DurableTransactionPageClassificationError::NonAdvancingCommitPosition {
                        previous: lineage.position(8),
                        actual: lineage.position(6),
                    },
                ),
            })
        );
        Ok(())
    }

    #[test]
    fn latest_selection_retains_per_record_classification_failures() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(54).ok_or(TestError("page number"))?;
        let owner = durable_identity(17, 1)?;
        let page = durable_page_observation(&lineage, owner, 54, 1, 0x54, 5)?;
        let early_commit = [durable_commit_observation(&lineage, owner, 3)?];
        let duplicate_commits = [
            durable_commit_observation(&lineage, owner, 6)?,
            durable_commit_observation(&lineage, owner, 7)?,
        ];

        assert_eq!(
            select_latest_committed_transaction_page(
                &lineage,
                page_number,
                std::iter::once(&page),
                &early_commit,
            ),
            Err(DurableTransactionPageSelectionError::PageClassification {
                page_number,
                page_position: lineage.position(5),
                source: Box::new(
                    DurableTransactionPageClassificationError::CommitNotAfterPage {
                        transaction: owner,
                        page_position: lineage.position(5),
                        commit_position: lineage.position(3),
                    },
                ),
            })
        );

        let earlier_page = durable_page_observation(&lineage, owner, 54, 1, 0x54, 2)?;
        assert_eq!(
            select_latest_committed_transaction_page(
                &lineage,
                page_number,
                std::iter::once(&earlier_page),
                &duplicate_commits,
            ),
            Err(DurableTransactionPageSelectionError::PageClassification {
                page_number,
                page_position: lineage.position(2),
                source: Box::new(
                    DurableTransactionPageClassificationError::DuplicateMatchingCommit {
                        transaction: owner,
                        first: lineage.position(6),
                        duplicate: lineage.position(7),
                    },
                ),
            })
        );

        let post_commit_pages = [
            durable_page_observation(&lineage, owner, 54, 1, 0x54, 10)?,
            durable_page_observation(&lineage, owner, 54, 2, 0x55, 30)?,
        ];
        let middle_commit = [durable_commit_observation(&lineage, owner, 20)?];
        assert_eq!(
            select_latest_committed_transaction_page(
                &lineage,
                page_number,
                post_commit_pages.iter(),
                &middle_commit,
            ),
            Err(DurableTransactionPageSelectionError::PageClassification {
                page_number,
                page_position: lineage.position(30),
                source: Box::new(
                    DurableTransactionPageClassificationError::CommitNotAfterPage {
                        transaction: owner,
                        page_position: lineage.position(30),
                        commit_position: lineage.position(20),
                    },
                ),
            })
        );
        Ok(())
    }

    #[test]
    fn committed_reconciliation_reports_no_committed_state_without_store() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(55).ok_or(TestError("page number"))?;
        let owner = durable_identity(18, 1)?;
        let physical = [
            physical_page_observation(&lineage, 55, 1, 0x55, 1)?,
            physical_page_observation(&lineage, 55, 2, 0x56, 2)?,
        ];
        let owned = [durable_page_observation(&lineage, owner, 55, 2, 0x56, 2)?];
        let empty_physical: [DurablePageWalObservation<1>; 0] = [];
        let empty_owned: [DurableTransactionPageObservation<1>; 0] = [];

        assert_eq!(
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                None,
                &empty_physical,
                &empty_owned,
                &[],
            ),
            Ok(DurableCommittedTransactionPageReconciliation::NoCommittedPage { page_number })
        );
        assert_eq!(
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                None,
                &physical,
                &owned,
                &[],
            ),
            Ok(DurableCommittedTransactionPageReconciliation::NoCommittedPage { page_number })
        );
        Ok(())
    }

    #[test]
    fn committed_reconciliation_reports_missing_store_and_borrows_only_owned_input()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(56).ok_or(TestError("page number"))?;
        let owner = durable_identity(19, 1)?;
        let owned = [durable_page_observation(&lineage, owner, 56, 1, 0x56, 2)?];

        let result = {
            let physical = [physical_page_observation(&lineage, 56, 1, 0x56, 2)?];
            let commits = [durable_commit_observation(&lineage, owner, 4)?];
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                None,
                &physical,
                &owned,
                &commits,
            )
        }
        .map_err(|_| TestError("missing-store reconciliation failed"))?;
        let DurableCommittedTransactionPageReconciliation::StoreMissing { latest_committed } =
            result
        else {
            return Err(TestError("expected missing store"));
        };

        assert!(std::ptr::eq(latest_committed.observation(), &owned[0]));
        assert_eq!(latest_committed.commit_position(), &lineage.position(4));
        Ok(())
    }

    #[test]
    fn committed_reconciliation_is_exact_despite_later_raw_and_uncommitted_records()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(57).ok_or(TestError("page number"))?;
        let committed_owner = durable_identity(20, 1)?;
        let uncommitted_owner = durable_identity(20, 2)?;
        let physical = [
            physical_page_observation(&lineage, 57, 5, 0x57, 2)?,
            physical_page_observation(&lineage, 57, 6, 0x58, 5)?,
            physical_page_observation(&lineage, 57, 7, 0x59, 6)?,
        ];
        let owned = [
            durable_page_observation(&lineage, committed_owner, 57, 5, 0x57, 2)?,
            durable_page_observation(&lineage, uncommitted_owner, 57, 7, 0x59, 6)?,
        ];
        let commits = [durable_commit_observation(&lineage, committed_owner, 4)?];
        let snapshot = stored_page_observation(&lineage, 57, 5, 0x57, 2)?;

        let result = reconcile_committed_transaction_page(
            &lineage,
            page_number,
            Some(&snapshot),
            &physical,
            &owned,
            &commits,
        )
        .map_err(|_| TestError("exact committed reconciliation failed"))?;
        let DurableCommittedTransactionPageReconciliation::ExactCurrent { latest_committed } =
            result
        else {
            return Err(TestError("expected exact committed state"));
        };

        assert!(std::ptr::eq(latest_committed.observation(), &owned[0]));
        assert_eq!(
            latest_committed.observation().position(),
            &lineage.position(2)
        );
        assert_eq!(latest_committed.commit_position(), &lineage.position(4));
        Ok(())
    }

    #[test]
    fn committed_reconciliation_reports_store_behind_later_lower_version() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(58).ok_or(TestError("page number"))?;
        let stored_owner = durable_identity(21, 1)?;
        let latest_owner = durable_identity(21, 2)?;
        let physical = [
            physical_page_observation(&lineage, 58, 10, 0x58, 2)?,
            physical_page_observation(&lineage, 58, 1, 0x59, 5)?,
        ];
        let owned = [
            durable_page_observation(&lineage, stored_owner, 58, 10, 0x58, 2)?,
            durable_page_observation(&lineage, latest_owner, 58, 1, 0x59, 5)?,
        ];
        let commits = [
            durable_commit_observation(&lineage, stored_owner, 3)?,
            durable_commit_observation(&lineage, latest_owner, 7)?,
        ];
        let snapshot = stored_page_observation(&lineage, 58, 10, 0x58, 2)?;

        let result = reconcile_committed_transaction_page(
            &lineage,
            page_number,
            Some(&snapshot),
            &physical,
            &owned,
            &commits,
        )
        .map_err(|_| TestError("behind reconciliation failed"))?;
        let DurableCommittedTransactionPageReconciliation::StoreBehind {
            stored_page_position,
            stored_commit_position,
            latest_committed,
        } = result
        else {
            return Err(TestError("expected store behind"));
        };

        assert_eq!(stored_page_position, lineage.position(2));
        assert_eq!(stored_commit_position, lineage.position(3));
        assert!(std::ptr::eq(latest_committed.observation(), &owned[1]));
        assert_eq!(
            latest_committed.observation().page().page_version(),
            PageVersion::new(1)
        );
        assert_eq!(latest_committed.commit_position(), &lineage.position(7));
        Ok(())
    }

    #[test]
    fn committed_reconciliation_rejects_raw_and_uncommitted_snapshot_backing()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(59).ok_or(TestError("page number"))?;
        let committed_owner = durable_identity(22, 1)?;
        let uncommitted_owner = durable_identity(22, 2)?;
        let commits = [durable_commit_observation(&lineage, committed_owner, 4)?];

        let raw_physical = [
            physical_page_observation(&lineage, 59, 1, 0x59, 2)?,
            physical_page_observation(&lineage, 59, 2, 0x5A, 5)?,
        ];
        let raw_owned = [durable_page_observation(
            &lineage,
            committed_owner,
            59,
            1,
            0x59,
            2,
        )?];
        let raw_snapshot = stored_page_observation(&lineage, 59, 2, 0x5A, 5)?;
        assert_eq!(
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                Some(&raw_snapshot),
                &raw_physical,
                &raw_owned,
                &commits,
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::SnapshotBackedByRawPage {
                    page_number,
                    position: lineage.position(5),
                }
            )
        );

        let uncommitted_physical = [
            physical_page_observation(&lineage, 59, 1, 0x59, 2)?,
            physical_page_observation(&lineage, 59, 3, 0x5B, 6)?,
        ];
        let uncommitted_owned = [
            durable_page_observation(&lineage, committed_owner, 59, 1, 0x59, 2)?,
            durable_page_observation(&lineage, uncommitted_owner, 59, 3, 0x5B, 6)?,
        ];
        let uncommitted_snapshot = stored_page_observation(&lineage, 59, 3, 0x5B, 6)?;
        assert_eq!(
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                Some(&uncommitted_snapshot),
                &uncommitted_physical,
                &uncommitted_owned,
                &commits,
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::SnapshotBackedByUncommittedTransactionPage {
                    page_number,
                    transaction: uncommitted_owner,
                    page_position: lineage.position(6),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn committed_reconciliation_retains_selection_and_physical_failures() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let page_number = PageNumber::new(60).ok_or(TestError("page number"))?;
        let owner = durable_identity(23, 1)?;
        let foreign_commits = [durable_commit_observation(&foreign, owner, 1)?];
        let empty_physical: [DurablePageWalObservation<1>; 0] = [];
        let empty_owned: [DurableTransactionPageObservation<1>; 0] = [];

        assert_eq!(
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                None,
                &empty_physical,
                &empty_owned,
                &foreign_commits,
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::Selection {
                    source: Box::new(DurableTransactionPageSelectionError::CommitPrefix {
                        source: Box::new(
                            DurableTransactionPageClassificationError::ForeignCommitLineage {
                                position: foreign.position(1),
                            },
                        ),
                    }),
                }
            )
        );

        let snapshot = stored_page_observation(&lineage, 60, 1, 0x60, 2)?;
        assert_eq!(
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                Some(&snapshot),
                &empty_physical,
                &empty_owned,
                &[],
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::Physical {
                    source: Box::new(DurablePageReconciliationError::SnapshotPositionUnbacked {
                        page_number,
                        position: lineage.position(2),
                    }),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn committed_reconciliation_rejects_missing_and_contradictory_physical_projections()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(61).ok_or(TestError("page number"))?;
        let owner = durable_identity(24, 1)?;
        let owned = [durable_page_observation(&lineage, owner, 61, 1, 0x61, 2)?];
        let empty_physical: [DurablePageWalObservation<1>; 0] = [];
        let contradictory_physical = [physical_page_observation(&lineage, 61, 2, 0x62, 2)?];

        assert_eq!(
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                None,
                &empty_physical,
                &owned,
                &[],
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::OwnedPagePositionUnbacked {
                    page_number,
                    position: lineage.position(2),
                }
            )
        );
        assert_eq!(
            reconcile_committed_transaction_page(
                &lineage,
                page_number,
                None,
                &contradictory_physical,
                &owned,
                &[],
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::OwnedPagePayloadContradiction {
                    page_number,
                    position: lineage.position(2),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn committed_reconciliation_defensively_rejects_impossible_committed_backing()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(62).ok_or(TestError("page number"))?;
        let owner = durable_identity(25, 1)?;

        assert_eq!(
            resolve_committed_snapshot_reconciliation::<1>(
                page_number,
                DurableTransactionPageSelection::NoCommittedPage { page_number },
                lineage.position(2),
                lineage.position(4),
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::CommittedSnapshotWithoutSelection {
                    page_number,
                    stored_page_position: lineage.position(2),
                    stored_commit_position: lineage.position(4),
                }
            )
        );

        let owned = durable_page_observation(&lineage, owner, 62, 1, 0x62, 2)?;
        let commits = [durable_commit_observation(&lineage, owner, 4)?];
        let selection = select_latest_committed_transaction_page(
            &lineage,
            page_number,
            std::iter::once(&owned),
            &commits,
        )
        .map_err(|_| TestError("selection failed"))?;
        assert_eq!(
            resolve_committed_snapshot_reconciliation(
                page_number,
                selection,
                lineage.position(5),
                lineage.position(6),
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::CommittedSnapshotAfterSelection {
                    page_number,
                    stored_page_position: lineage.position(5),
                    selected_page_position: lineage.position(2),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn recovery_candidate_from_missing_store_matches_source_and_target() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(63).ok_or(TestError("page number"))?;
        let owner = durable_identity(26, 1)?;
        let physical = [physical_page_observation(&lineage, 63, 4, 0x63, 2)?];
        let owned = [durable_page_observation(&lineage, owner, 63, 4, 0x63, 2)?];
        let commits = [durable_commit_observation(&lineage, owner, 4)?];

        let decision = derive_committed_transaction_page_recovery_candidate(
            &lineage,
            page_number,
            None,
            &physical,
            &owned,
            &commits,
        )
        .map_err(|_| TestError("missing-store candidate"))?;
        let DurableCommittedTransactionPageRecoveryDecision::Candidate(candidate) = decision else {
            return Err(TestError("expected recovery candidate"));
        };

        assert_eq!(
            candidate.precondition(),
            &DurableCommittedTransactionPageRecoveryPrecondition::StoreMissing
        );
        assert!(std::ptr::eq(
            candidate.latest_committed().observation(),
            &owned[0]
        ));
        assert_eq!(
            candidate.latest_committed().commit_position(),
            &lineage.position(4)
        );
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(&candidate, None),
            Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches)
        );

        let target = stored_page_observation(&lineage, 63, 4, 0x63, 2)?;
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(&candidate, Some(&target)),
            Ok(DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent)
        );
        Ok(())
    }

    #[test]
    fn recovery_candidate_from_behind_store_retains_exact_source_and_lower_version_target()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(64).ok_or(TestError("page number"))?;
        let source_owner = durable_identity(27, 1)?;
        let target_owner = durable_identity(27, 2)?;
        let physical = [
            physical_page_observation(&lineage, 64, 10, 0x64, 2)?,
            physical_page_observation(&lineage, 64, 1, 0x65, 5)?,
        ];
        let owned = [
            durable_page_observation(&lineage, source_owner, 64, 10, 0x64, 2)?,
            durable_page_observation(&lineage, target_owner, 64, 1, 0x65, 5)?,
        ];
        let commits = [
            durable_commit_observation(&lineage, source_owner, 3)?,
            durable_commit_observation(&lineage, target_owner, 7)?,
        ];
        let source = stored_page_observation(&lineage, 64, 10, 0x64, 2)?;

        let decision = derive_committed_transaction_page_recovery_candidate(
            &lineage,
            page_number,
            Some(&source),
            &physical,
            &owned,
            &commits,
        )
        .map_err(|_| TestError("behind-store candidate"))?;
        let DurableCommittedTransactionPageRecoveryDecision::Candidate(candidate) = decision else {
            return Err(TestError("expected recovery candidate"));
        };

        let DurableCommittedTransactionPageRecoveryPrecondition::ExactSnapshot {
            snapshot,
            commit_position,
        } = candidate.precondition()
        else {
            return Err(TestError("expected exact source snapshot"));
        };
        assert!(std::ptr::eq(*snapshot, &source));
        assert_eq!(commit_position, &lineage.position(3));
        assert!(std::ptr::eq(
            candidate.latest_committed().observation(),
            &owned[1]
        ));
        assert_eq!(
            candidate
                .latest_committed()
                .observation()
                .page()
                .page_version(),
            PageVersion::new(1)
        );
        assert_eq!(
            candidate.latest_committed().commit_position(),
            &lineage.position(7)
        );
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(&candidate, Some(&source)),
            Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches)
        );

        let target = stored_page_observation(&lineage, 64, 1, 0x65, 5)?;
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(&candidate, Some(&target)),
            Ok(DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent)
        );
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(&candidate, None),
            Err(
                DurableCommittedTransactionPageRecoveryComparisonError::StoreChanged {
                    page_number,
                    expected_source_position: Some(lineage.position(2)),
                    actual_position: None,
                }
            )
        );

        let contradictory_source = stored_page_observation(&lineage, 64, 11, 0x66, 2)?;
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(
                &candidate,
                Some(&contradictory_source)
            ),
            Err(
                DurableCommittedTransactionPageRecoveryComparisonError::SourceSnapshotPayloadContradiction {
                    page_number,
                    position: lineage.position(2),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn recovery_planning_preserves_explicit_no_candidate_decisions() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let page_number = PageNumber::new(65).ok_or(TestError("page number"))?;
        let empty_physical: [DurablePageWalObservation<1>; 0] = [];
        let empty_owned: [DurableTransactionPageObservation<1>; 0] = [];

        assert_eq!(
            derive_committed_transaction_page_recovery_candidate(
                &lineage,
                page_number,
                None,
                &empty_physical,
                &empty_owned,
                &[],
            ),
            Ok(DurableCommittedTransactionPageRecoveryDecision::NoCommittedPage { page_number })
        );

        let owner = durable_identity(28, 1)?;
        let foreign_commits = [durable_commit_observation(&foreign, owner, 1)?];
        assert_eq!(
            derive_committed_transaction_page_recovery_candidate(
                &lineage,
                page_number,
                None,
                &empty_physical,
                &empty_owned,
                &foreign_commits,
            ),
            Err(
                DurableCommittedTransactionPageRecoveryPlanningError::Reconciliation {
                    source: Box::new(
                        DurableCommittedTransactionPageReconciliationError::Selection {
                            source: Box::new(
                                DurableTransactionPageSelectionError::CommitPrefix {
                                    source: Box::new(
                                        DurableTransactionPageClassificationError::ForeignCommitLineage {
                                            position: foreign.position(1),
                                        },
                                    ),
                                },
                            ),
                        },
                    ),
                },
            )
        );

        let physical = [physical_page_observation(&lineage, 65, 3, 0x65, 2)?];
        let owned = [durable_page_observation(&lineage, owner, 65, 3, 0x65, 2)?];
        let commits = [durable_commit_observation(&lineage, owner, 4)?];
        let snapshot = stored_page_observation(&lineage, 65, 3, 0x65, 2)?;
        let decision = derive_committed_transaction_page_recovery_candidate(
            &lineage,
            page_number,
            Some(&snapshot),
            &physical,
            &owned,
            &commits,
        )
        .map_err(|_| TestError("exact-current planning"))?;
        let DurableCommittedTransactionPageRecoveryDecision::ExactCurrent { latest_committed } =
            decision
        else {
            return Err(TestError("expected exact-current decision"));
        };
        assert!(std::ptr::eq(latest_committed.observation(), &owned[0]));
        assert_eq!(latest_committed.commit_position(), &lineage.position(4));
        Ok(())
    }

    #[test]
    fn recovery_candidate_comparison_rejects_invalid_or_changed_store_state()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let page_number = PageNumber::new(66).ok_or(TestError("page number"))?;
        let other_page = PageNumber::new(67).ok_or(TestError("other page number"))?;
        let owner = durable_identity(29, 1)?;
        let physical = [physical_page_observation(&lineage, 66, 2, 0x66, 2)?];
        let owned = [durable_page_observation(&lineage, owner, 66, 2, 0x66, 2)?];
        let commits = [durable_commit_observation(&lineage, owner, 4)?];
        let decision = derive_committed_transaction_page_recovery_candidate(
            &lineage,
            page_number,
            None,
            &physical,
            &owned,
            &commits,
        )
        .map_err(|_| TestError("comparison candidate"))?;
        let DurableCommittedTransactionPageRecoveryDecision::Candidate(candidate) = decision else {
            return Err(TestError("expected recovery candidate"));
        };

        let wrong_page = StoredPageSnapshotObservation::from_bytes(
            other_page,
            PageVersion::new(2),
            [0x66],
            foreign.position(2),
        )
        .map_err(|_| TestError("wrong-page snapshot"))?;
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(
                &candidate,
                Some(&wrong_page)
            ),
            Err(
                DurableCommittedTransactionPageRecoveryComparisonError::UnexpectedCurrentSnapshotPage {
                    expected: page_number,
                    actual: other_page,
                    position: foreign.position(2),
                }
            )
        );

        let foreign_target = stored_page_observation(&foreign, 66, 2, 0x66, 2)?;
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(
                &candidate,
                Some(&foreign_target)
            ),
            Err(
                DurableCommittedTransactionPageRecoveryComparisonError::ForeignCurrentSnapshotLineage {
                    page_number,
                    position: foreign.position(2),
                }
            )
        );

        let contradictory_target = stored_page_observation(&lineage, 66, 3, 0x67, 2)?;
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(
                &candidate,
                Some(&contradictory_target)
            ),
            Err(
                DurableCommittedTransactionPageRecoveryComparisonError::TargetSnapshotPayloadContradiction {
                    page_number,
                    position: lineage.position(2),
                }
            )
        );

        let changed = stored_page_observation(&lineage, 66, 5, 0x68, 9)?;
        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(&candidate, Some(&changed)),
            Err(
                DurableCommittedTransactionPageRecoveryComparisonError::StoreChanged {
                    page_number,
                    expected_source_position: None,
                    actual_position: Some(lineage.position(9)),
                }
            )
        );
        Ok(())
    }

    #[test]
    fn recovery_candidate_comparison_does_not_prove_wal_currency() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(68).ok_or(TestError("page number"))?;
        let first_owner = durable_identity(30, 1)?;
        let later_owner = durable_identity(30, 2)?;
        let first_physical = [physical_page_observation(&lineage, 68, 4, 0x68, 2)?];
        let first_owned = [durable_page_observation(
            &lineage,
            first_owner,
            68,
            4,
            0x68,
            2,
        )?];
        let first_commits = [durable_commit_observation(&lineage, first_owner, 4)?];
        let decision = derive_committed_transaction_page_recovery_candidate(
            &lineage,
            page_number,
            None,
            &first_physical,
            &first_owned,
            &first_commits,
        )
        .map_err(|_| TestError("stale candidate"))?;
        let DurableCommittedTransactionPageRecoveryDecision::Candidate(candidate) = decision else {
            return Err(TestError("expected recovery candidate"));
        };
        let first_target = stored_page_observation(&lineage, 68, 4, 0x68, 2)?;

        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(&candidate, Some(&first_target)),
            Ok(DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent)
        );

        let current_physical = [
            physical_page_observation(&lineage, 68, 4, 0x68, 2)?,
            physical_page_observation(&lineage, 68, 5, 0x69, 5)?,
        ];
        let current_owned = [
            durable_page_observation(&lineage, first_owner, 68, 4, 0x68, 2)?,
            durable_page_observation(&lineage, later_owner, 68, 5, 0x69, 5)?,
        ];
        let current_commits = [
            durable_commit_observation(&lineage, first_owner, 4)?,
            durable_commit_observation(&lineage, later_owner, 7)?,
        ];
        let current = reconcile_committed_transaction_page(
            &lineage,
            page_number,
            Some(&first_target),
            &current_physical,
            &current_owned,
            &current_commits,
        )
        .map_err(|_| TestError("current reconciliation"))?;
        let DurableCommittedTransactionPageReconciliation::StoreBehind {
            latest_committed, ..
        } = current
        else {
            return Err(TestError("expected newer committed target"));
        };
        assert!(std::ptr::eq(
            latest_committed.observation(),
            &current_owned[1]
        ));

        assert_eq!(
            compare_committed_transaction_page_recovery_candidate(&candidate, Some(&first_target)),
            Ok(DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent)
        );
        Ok(())
    }

    #[test]
    fn recovery_gate_rejects_lineage_and_prewrite_port_failures() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let page_number = PageNumber::new(69).ok_or(TestError("page number"))?;

        let mut mismatch_source =
            FakeDurablePageRecoverySource::new(lineage.clone(), vec![], vec![], vec![]);
        let mut mismatch_store = FakeCommittedPageRecoveryStore::new(foreign.clone(), None);
        let mismatch = recover_committed_transaction_page(
            &mut mismatch_source,
            &mut mismatch_store,
            page_number,
        );
        let Err(CommittedTransactionPageRecoveryError::LineageMismatch {
            source_lineage,
            store_lineage,
        }) = mismatch
        else {
            return Err(TestError("expected lineage mismatch"));
        };
        assert!(source_lineage.same_lineage(&lineage));
        assert!(store_lineage.same_lineage(&foreign));
        assert_eq!(mismatch_source.callbacks, 0);
        assert_eq!(mismatch_store.observations.get(), 0);
        assert_eq!(mismatch_store.attempts, 0);

        let mut source_failure =
            FakeDurablePageRecoverySource::new(lineage.clone(), vec![], vec![], vec![]);
        source_failure.before_callback_error = Some(FakeFault("source before callback"));
        let mut untouched_store = FakeCommittedPageRecoveryStore::new(lineage.clone(), None);
        assert!(matches!(
            recover_committed_transaction_page(
                &mut source_failure,
                &mut untouched_store,
                page_number
            ),
            Err(CommittedTransactionPageRecoveryError::Source(FakeFault(
                "source before callback"
            )))
        ));
        assert_eq!(source_failure.callbacks, 0);
        assert_eq!(untouched_store.observations.get(), 0);
        assert_eq!(untouched_store.attempts, 0);

        let mut observation_source =
            FakeDurablePageRecoverySource::new(lineage.clone(), vec![], vec![], vec![]);
        let mut observation_store = FakeCommittedPageRecoveryStore::new(lineage.clone(), None);
        observation_store.observation_fault = Some(FakeFault("observation"));
        assert!(matches!(
            recover_committed_transaction_page(
                &mut observation_source,
                &mut observation_store,
                page_number
            ),
            Err(CommittedTransactionPageRecoveryError::StoreObservation(
                FakeFault("observation")
            ))
        ));
        assert_eq!(observation_source.callbacks, 1);
        assert_eq!(observation_store.observations.get(), 1);
        assert_eq!(observation_store.attempts, 0);
        Ok(())
    }

    #[test]
    fn recovery_gate_returns_explicit_no_write_outcomes() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(70).ok_or(TestError("page number"))?;
        let mut empty_source =
            FakeDurablePageRecoverySource::new(lineage.clone(), vec![], vec![], vec![]);
        let mut empty_store = FakeCommittedPageRecoveryStore::new(lineage.clone(), None);

        assert_eq!(
            recover_committed_transaction_page(&mut empty_source, &mut empty_store, page_number)
                .map_err(|_| TestError("empty recovery")),
            Ok(CommittedTransactionPageRecoveryOutcome::NoCommittedPage { page_number })
        );
        assert_eq!(empty_store.observations.get(), 1);
        assert_eq!(empty_store.attempts, 0);

        let owner = durable_identity(31, 1)?;
        let mut exact_source = one_page_recovery_source(&lineage, owner, 70, 4, 0x70, 2, 4)?;
        let exact_snapshot = FakeRecoverySnapshot {
            page_number,
            page_version: PageVersion::new(4),
            byte: 0x70,
            page_position: lineage.position(2),
        };
        let mut exact_store =
            FakeCommittedPageRecoveryStore::new(lineage.clone(), Some(exact_snapshot));
        let exact =
            recover_committed_transaction_page(&mut exact_source, &mut exact_store, page_number)
                .map_err(|_| TestError("exact recovery"))?;
        let CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { target } = exact else {
            return Err(TestError("expected already-current outcome"));
        };
        assert_eq!(target.transaction(), owner);
        assert_eq!(target.page_number(), page_number);
        assert_eq!(target.page_version(), PageVersion::new(4));
        assert_eq!(target.bytes(), &[0x70]);
        assert_eq!(target.page_position(), &lineage.position(2));
        assert_eq!(target.commit_position(), &lineage.position(4));
        assert_eq!(exact_store.observations.get(), 1);
        assert_eq!(exact_store.attempts, 0);
        Ok(())
    }

    #[test]
    fn recovery_gate_writes_missing_and_behind_lower_version_targets() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(71).ok_or(TestError("page number"))?;
        let source_owner = durable_identity(32, 1)?;
        let target_owner = durable_identity(32, 2)?;

        let mut missing_source =
            two_page_recovery_source(&lineage, source_owner, target_owner, 71)?;
        let mut missing_store = FakeCommittedPageRecoveryStore::new(lineage.clone(), None);
        let missing = recover_committed_transaction_page(
            &mut missing_source,
            &mut missing_store,
            page_number,
        )
        .map_err(|_| TestError("missing recovery"))?;
        let CommittedTransactionPageRecoveryOutcome::Recovered { target } = missing else {
            return Err(TestError("expected missing recovery"));
        };
        assert_eq!(target.transaction(), target_owner);
        assert_eq!(target.page_version(), PageVersion::new(1));
        assert_eq!(target.bytes(), &[0xB0]);
        assert_eq!(target.page_position(), &lineage.position(5));
        assert_eq!(target.commit_position(), &lineage.position(7));
        assert_eq!(missing_store.attempts, 1);
        assert_eq!(
            missing_store.current,
            Some(FakeRecoverySnapshot::from_target(&target))
        );

        let mut behind_source = two_page_recovery_source(&lineage, source_owner, target_owner, 71)?;
        let behind_snapshot = FakeRecoverySnapshot {
            page_number,
            page_version: PageVersion::new(10),
            byte: 0xA0,
            page_position: lineage.position(2),
        };
        let mut behind_store =
            FakeCommittedPageRecoveryStore::new(lineage.clone(), Some(behind_snapshot));
        let behind =
            recover_committed_transaction_page(&mut behind_source, &mut behind_store, page_number)
                .map_err(|_| TestError("behind recovery"))?;
        let CommittedTransactionPageRecoveryOutcome::Recovered { target } = behind else {
            return Err(TestError("expected behind recovery"));
        };
        assert_eq!(target.transaction(), target_owner);
        assert_eq!(target.page_version(), PageVersion::new(1));
        assert_eq!(target.page_position(), &lineage.position(5));
        assert_eq!(behind_store.attempts, 1);
        assert_eq!(
            behind_store.current,
            Some(FakeRecoverySnapshot::from_target(&target))
        );
        Ok(())
    }

    #[test]
    fn recovery_gate_preserves_planning_failure_before_store_attempt() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let page_number = PageNumber::new(72).ok_or(TestError("page number"))?;
        let owner = durable_identity(33, 1)?;
        let mut source = FakeDurablePageRecoverySource::new(
            lineage.clone(),
            vec![],
            vec![],
            vec![durable_commit_observation(&foreign, owner, 1)?],
        );
        let mut store = FakeCommittedPageRecoveryStore::new(lineage.clone(), None);

        let result = recover_committed_transaction_page(&mut source, &mut store, page_number);
        let Err(CommittedTransactionPageRecoveryError::Planning { source }) = result else {
            return Err(TestError("expected planning failure"));
        };
        assert_eq!(
            source.as_ref(),
            &DurableCommittedTransactionPageRecoveryPlanningError::Reconciliation {
                source: Box::new(
                    DurableCommittedTransactionPageReconciliationError::Selection {
                        source: Box::new(DurableTransactionPageSelectionError::CommitPrefix {
                            source: Box::new(
                                DurableTransactionPageClassificationError::ForeignCommitLineage {
                                    position: foreign.position(1),
                                },
                            ),
                        },),
                    },
                ),
            }
        );
        assert_eq!(store.observations.get(), 1);
        assert_eq!(store.attempts, 0);
        Ok(())
    }

    #[test]
    fn recovery_gate_preserves_defensive_comparison_failures() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(72).ok_or(TestError("page number"))?;
        let comparison = DurableCommittedTransactionPageRecoveryComparisonError::StoreChanged {
            page_number,
            expected_source_position: None,
            actual_position: Some(lineage.position(2)),
        };
        let mapped = map_recovery_before_write_error::<FakeFault, FakeFault, FakeFault, 1>(
            RecoveryBeforeWriteError::CandidateComparison(Box::new(comparison)),
        );
        let CommittedTransactionPageRecoveryError::CandidateComparison { source } = mapped else {
            return Err(TestError("expected candidate-comparison failure"));
        };
        assert_eq!(
            source.as_ref(),
            &DurableCommittedTransactionPageRecoveryComparisonError::StoreChanged {
                page_number,
                expected_source_position: None,
                actual_position: Some(lineage.position(2)),
            }
        );

        let mapped = map_recovery_before_write_error::<FakeFault, FakeFault, FakeFault, 1>(
            RecoveryBeforeWriteError::UnexpectedCandidateComparison(
                DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent,
            ),
        );
        assert!(matches!(
            mapped,
            CommittedTransactionPageRecoveryError::UnexpectedCandidateComparison {
                actual: DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent,
            }
        ));
        Ok(())
    }

    #[test]
    fn recovery_gate_store_recheck_prevents_changed_source_overwrite() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let page_number = PageNumber::new(73).ok_or(TestError("page number"))?;
        let source_owner = durable_identity(34, 1)?;
        let target_owner = durable_identity(34, 2)?;
        let mut source = two_page_recovery_source(&lineage, source_owner, target_owner, 73)?;
        let original = FakeRecoverySnapshot {
            page_number,
            page_version: PageVersion::new(10),
            byte: 0xA0,
            page_position: lineage.position(2),
        };
        let changed = FakeRecoverySnapshot {
            page_number,
            page_version: PageVersion::new(99),
            byte: 0xCC,
            page_position: lineage.position(6),
        };
        let mut store =
            FakeCommittedPageRecoveryStore::new(lineage.clone(), Some(original.clone()));
        store.replace_before_compare = Some(changed.clone());

        let result = recover_committed_transaction_page(&mut source, &mut store, page_number);
        let Err(CommittedTransactionPageRecoveryError::StoreWrite { state }) = result else {
            return Err(TestError("expected terminal changed-store state"));
        };
        assert_eq!(state.as_ref().cause(), &FakeFault("precondition changed"));
        let CommittedTransactionPageRecoverySourceState::ExactSnapshot {
            page_version,
            bytes,
            page_position,
            commit_position,
            ..
        } = state.source_state()
        else {
            return Err(TestError("expected exact source state"));
        };
        assert_eq!(*page_version, PageVersion::new(10));
        assert_eq!(bytes, &[0xA0]);
        assert_eq!(page_position, &lineage.position(2));
        assert_eq!(commit_position, &lineage.position(3));
        assert_eq!(state.target().transaction(), target_owner);
        assert_eq!(state.target().bytes(), &[0xB0]);
        assert_eq!(state.target().page_position(), &lineage.position(5));
        assert_eq!(store.current, Some(changed));
        assert_eq!(store.attempts, 1);
        Ok(())
    }

    #[test]
    fn recovery_gate_attempt_outcome_overrides_post_callback_source_error() -> Result<(), TestError>
    {
        let lineage = LogLineage::new();
        let owner = durable_identity(35, 1)?;

        let success_page = PageNumber::new(74).ok_or(TestError("success page"))?;
        let mut success_source = one_page_recovery_source(&lineage, owner, 74, 4, 0x74, 2, 4)?;
        success_source.after_callback_error = Some(FakeFault("source after success"));
        let mut success_store = FakeCommittedPageRecoveryStore::new(lineage.clone(), None);
        let success = recover_committed_transaction_page(
            &mut success_source,
            &mut success_store,
            success_page,
        )
        .map_err(|_| TestError("post-callback success lost"))?;
        assert!(matches!(
            success,
            CommittedTransactionPageRecoveryOutcome::Recovered { .. }
        ));
        assert!(success_store.current.is_some());

        let before_page = PageNumber::new(75).ok_or(TestError("before page"))?;
        let mut before_source = one_page_recovery_source(&lineage, owner, 75, 5, 0x75, 2, 4)?;
        before_source.after_callback_error = Some(FakeFault("source after before-fault"));
        let mut before_store = FakeCommittedPageRecoveryStore::new(lineage.clone(), None);
        before_store.write_fault = Some(FakeRecoveryWriteFault::Before(FakeFault("before write")));
        let before =
            recover_committed_transaction_page(&mut before_source, &mut before_store, before_page);
        let Err(CommittedTransactionPageRecoveryError::StoreWrite { state }) = before else {
            return Err(TestError("before fault became source error"));
        };
        assert_eq!(state.as_ref().cause(), &FakeFault("before write"));
        assert!(before_store.current.is_none());
        assert!(matches!(
            state.source_state(),
            CommittedTransactionPageRecoverySourceState::StoreMissing { .. }
        ));

        let retry =
            recover_committed_transaction_page(&mut before_source, &mut before_store, before_page)
                .map_err(|_| TestError("fresh before-fault retry"))?;
        assert!(matches!(
            retry,
            CommittedTransactionPageRecoveryOutcome::Recovered { .. }
        ));
        assert_eq!(before_store.attempts, 2);

        let after_page = PageNumber::new(76).ok_or(TestError("after page"))?;
        let mut after_source = one_page_recovery_source(&lineage, owner, 76, 6, 0x76, 2, 4)?;
        after_source.after_callback_error = Some(FakeFault("source after after-fault"));
        let mut after_store = FakeCommittedPageRecoveryStore::new(lineage.clone(), None);
        after_store.write_fault = Some(FakeRecoveryWriteFault::After(FakeFault("after write")));
        let after =
            recover_committed_transaction_page(&mut after_source, &mut after_store, after_page);
        let Err(CommittedTransactionPageRecoveryError::StoreWrite { state }) = after else {
            return Err(TestError("after fault became source error"));
        };
        assert_eq!(state.as_ref().cause(), &FakeFault("after write"));
        assert!(matches!(
            state.source_state(),
            CommittedTransactionPageRecoverySourceState::StoreMissing {
                page_number,
                target_page_position,
            } if *page_number == after_page && target_page_position == &lineage.position(2)
        ));
        assert_eq!(state.target().transaction(), owner);
        assert_eq!(state.target().page_number(), after_page);
        assert_eq!(state.target().page_version(), PageVersion::new(6));
        assert_eq!(state.target().bytes(), &[0x76]);
        assert_eq!(state.target().page_position(), &lineage.position(2));
        assert_eq!(state.target().commit_position(), &lineage.position(4));
        assert!(after_store.current.is_some());
        assert_eq!(after_store.attempts, 1);

        let resolved =
            recover_committed_transaction_page(&mut after_source, &mut after_store, after_page)
                .map_err(|_| TestError("after-fault resolution"))?;
        assert!(matches!(
            resolved,
            CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
        ));
        assert_eq!(after_store.attempts, 1);
        Ok(())
    }

    #[test]
    fn batch_recovery_rejects_lineage_inventory_failure_and_invalid_order_before_store_access()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let first_page = PageNumber::new(81).ok_or(TestError("first batch page"))?;
        let second_page = PageNumber::new(82).ok_or(TestError("second batch page"))?;

        let mut mismatch_source = batch_recovery_source(&lineage, &[81, 82])?;
        let mut mismatch_store = FakeBatchCommittedPageRecoveryStore::new(foreign.clone());
        let mismatch =
            recover_committed_transaction_pages(&mut mismatch_source, &mut mismatch_store);
        let Err(CommittedTransactionPagesRecoveryError::LineageMismatch {
            source_lineage,
            store_lineage,
        }) = mismatch
        else {
            return Err(TestError("expected batch lineage mismatch"));
        };
        assert!(source_lineage.same_lineage(&lineage));
        assert!(store_lineage.same_lineage(&foreign));
        assert_eq!(mismatch_source.inventory_calls, 0);
        assert_eq!(mismatch_source.callbacks, 0);
        assert!(mismatch_store.observations.borrow().is_empty());

        let mut failed_inventory = batch_recovery_source(&lineage, &[81, 82])?;
        failed_inventory.inventory_error = Some(FakeFault("inventory"));
        let mut untouched_store = FakeBatchCommittedPageRecoveryStore::new(lineage.clone());
        assert!(matches!(
            recover_committed_transaction_pages(&mut failed_inventory, &mut untouched_store),
            Err(CommittedTransactionPagesRecoveryError::Inventory(
                FakeFault("inventory")
            ))
        ));
        assert_eq!(failed_inventory.inventory_calls, 1);
        assert_eq!(failed_inventory.callbacks, 0);
        assert!(untouched_store.observations.borrow().is_empty());

        let mut descending = batch_recovery_source(&lineage, &[81, 82])?;
        descending.inventory = vec![second_page, first_page];
        let result = recover_committed_transaction_pages(&mut descending, &mut untouched_store);
        assert!(matches!(
            result,
            Err(
                CommittedTransactionPagesRecoveryError::InventoryNotStrictlyIncreasing {
                    previous,
                    actual,
                }
            ) if previous == second_page && actual == first_page
        ));
        assert_eq!(descending.callbacks, 0);
        assert!(untouched_store.observations.borrow().is_empty());

        let mut duplicate = batch_recovery_source(&lineage, &[81, 82])?;
        duplicate.inventory = vec![first_page, first_page];
        let result = recover_committed_transaction_pages(&mut duplicate, &mut untouched_store);
        assert!(matches!(
            result,
            Err(
                CommittedTransactionPagesRecoveryError::InventoryNotStrictlyIncreasing {
                    previous,
                    actual,
                }
            ) if previous == first_page && actual == first_page
        ));
        assert_eq!(duplicate.callbacks, 0);
        assert!(untouched_store.observations.borrow().is_empty());
        Ok(())
    }

    #[test]
    fn batch_recovery_completes_once_in_strict_inventory_order() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let expected = [81, 82, 83]
            .into_iter()
            .map(|number| PageNumber::new(number).ok_or(TestError("batch page")))
            .collect::<Result<Vec<_>, _>>()?;
        let mut source = batch_recovery_source(&lineage, &[83, 81, 82])?;
        let mut store = FakeBatchCommittedPageRecoveryStore::new(lineage);

        let outcome = recover_committed_transaction_pages(&mut source, &mut store)
            .map_err(|_| TestError("batch recovery"))?;

        assert_eq!(
            outcome
                .pages()
                .iter()
                .map(CommittedTransactionPageRecoveryOutcome::page_number)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(outcome.pages().iter().all(|outcome| matches!(
            outcome,
            CommittedTransactionPageRecoveryOutcome::Recovered { .. }
        )));
        assert_eq!(source.inventory_calls, 1);
        assert_eq!(source.callbacks, 3);
        assert_eq!(store.observations.into_inner(), expected);
        assert_eq!(store.attempts, expected);
        assert_eq!(store.current.len(), 3);
        Ok(())
    }

    #[test]
    fn batch_recovery_stops_with_exact_prefix_and_fresh_rerun_is_idempotent()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let first_page = PageNumber::new(81).ok_or(TestError("first batch page"))?;
        let failed_page = PageNumber::new(82).ok_or(TestError("failed batch page"))?;
        let last_page = PageNumber::new(83).ok_or(TestError("last batch page"))?;
        let mut source = batch_recovery_source(&lineage, &[81, 82, 83])?;
        let mut store = FakeBatchCommittedPageRecoveryStore::new(lineage);
        store.write_fault = Some((
            failed_page,
            FakeRecoveryWriteFault::Before(FakeFault("batch write")),
        ));

        let result = recover_committed_transaction_pages(&mut source, &mut store);
        let Err(CommittedTransactionPagesRecoveryError::Page {
            page_number,
            completed,
            source: nested,
        }) = result
        else {
            return Err(TestError("expected batch page failure"));
        };
        assert_eq!(page_number, failed_page);
        assert_eq!(completed.pages().len(), 1);
        assert_eq!(completed.pages()[0].page_number(), first_page);
        let CommittedTransactionPageRecoveryError::StoreWrite { state } = nested else {
            return Err(TestError("batch failure lost nested write state"));
        };
        assert_eq!(state.as_ref().target().page_number(), failed_page);
        assert_eq!(state.as_ref().cause(), &FakeFault("batch write"));
        assert_eq!(
            store.observations.borrow().as_slice(),
            &[first_page, failed_page]
        );
        assert_eq!(store.attempts, [first_page, failed_page]);
        assert_eq!(store.current.len(), 1);
        assert_eq!(store.current[0].page_number, first_page);

        let rerun = recover_committed_transaction_pages(&mut source, &mut store)
            .map_err(|_| TestError("fresh batch rerun"))?;
        assert_eq!(
            rerun
                .pages()
                .iter()
                .map(CommittedTransactionPageRecoveryOutcome::page_number)
                .collect::<Vec<_>>(),
            [first_page, failed_page, last_page]
        );
        assert!(matches!(
            rerun.pages()[0],
            CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
        ));
        assert!(matches!(
            rerun.pages()[1],
            CommittedTransactionPageRecoveryOutcome::Recovered { .. }
        ));
        assert!(matches!(
            rerun.pages()[2],
            CommittedTransactionPageRecoveryOutcome::Recovered { .. }
        ));
        assert_eq!(source.inventory_calls, 2);
        assert_eq!(source.callbacks, 5);
        assert_eq!(
            store.attempts,
            [first_page, failed_page, failed_page, last_page]
        );
        assert_eq!(store.current.len(), 3);
        Ok(())
    }

    #[test]
    fn owning_recovery_state_releases_parts_only_after_retry_and_restart_analysis()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let first_page = PageNumber::new(81).ok_or(TestError("first owning page"))?;
        let failed_page = PageNumber::new(82).ok_or(TestError("failed owning page"))?;
        let last_page = PageNumber::new(83).ok_or(TestError("last owning page"))?;
        let source = batch_recovery_source(&lineage, &[81, 82, 83])?;
        let mut store = FakeBatchCommittedPageRecoveryStore::new(lineage.clone());
        store.write_fault = Some((
            failed_page,
            FakeRecoveryWriteFault::Before(FakeFault("batch write")),
        ));

        let failure = UnrecoveredTransactionPageStorage::new(source, store)
            .recover()
            .err()
            .ok_or(TestError("owning recovery unexpectedly succeeded"))?;
        let CommittedTransactionPagesRecoveryError::Page {
            page_number,
            completed,
            source: CommittedTransactionPageRecoveryError::StoreWrite { state },
        } = failure.error()
        else {
            return Err(TestError("owning recovery lost exact batch failure"));
        };
        assert_eq!(*page_number, failed_page);
        assert_eq!(completed.pages().len(), 1);
        assert_eq!(completed.pages()[0].page_number(), first_page);
        assert_eq!(state.as_ref().cause(), &FakeFault("batch write"));
        assert!(Error::source(&failure).is_some());

        let page_recovered = failure
            .retry()
            .map_err(|_| TestError("fresh owning retry failed"))?;
        let mut recovered = page_recovered
            .analyze_restart()
            .map_err(|_| TestError("owning restart analysis failed"))?;
        assert_eq!(
            recovered
                .recovery_report()
                .pages()
                .iter()
                .map(CommittedTransactionPageRecoveryOutcome::page_number)
                .collect::<Vec<_>>(),
            [first_page, failed_page, last_page]
        );
        assert!(matches!(
            recovered.recovery_report().pages()[0],
            CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
        ));
        assert!(
            recovered
                .restart_analysis()
                .lineage()
                .same_lineage(&lineage)
        );
        assert_eq!(
            recovered.restart_analysis().durable_frontier(),
            Some(&lineage.position(6))
        );
        let transactions = recovered.restart_analysis().transactions();
        assert_eq!(transactions.len(), 3);
        for (index, entry) in transactions.iter().enumerate() {
            let sequence =
                u64::try_from(index + 1).map_err(|_| TestError("owning transaction sequence"))?;
            let page_position = sequence
                .checked_mul(2)
                .and_then(|position| position.checked_sub(1))
                .ok_or(TestError("owning page position"))?;
            let commit_position = page_position
                .checked_add(1)
                .ok_or(TestError("owning commit position"))?;
            assert_eq!(entry.transaction(), durable_identity(40, sequence)?);
            assert_eq!(
                entry.first_owned_page_position(),
                Some(&lineage.position(page_position))
            );
            assert_eq!(
                entry.last_owned_page_position(),
                Some(&lineage.position(page_position))
            );
            assert_eq!(entry.owned_page_record_count(), 1);
            assert_eq!(
                entry.state().commit_position(),
                Some(&lineage.position(commit_position))
            );
        }
        assert_eq!(
            recovered.prepare_restart_checkpoint_baseline().err(),
            Some(DurableTransactionRestartCheckpointBaselineError::PersistentLineageRequired)
        );
        let (source, store) = recovered.parts_mut();
        assert_eq!(source.inventory_calls, 2);
        assert_eq!(source.callbacks, 5);
        assert_eq!(source.restart_callbacks, 1);
        assert_eq!(
            store.observations.borrow().as_slice(),
            &[first_page, failed_page, first_page, failed_page, last_page]
        );
        assert_eq!(
            store.attempts,
            [first_page, failed_page, failed_page, last_page]
        );

        let (source, store, report, analysis) = recovered.into_parts();
        assert_eq!(source.inventory_calls, 2);
        assert_eq!(source.restart_callbacks, 1);
        assert_eq!(store.current.len(), 3);
        assert_eq!(report.pages().len(), 3);
        assert_eq!(analysis.transactions().len(), 3);
        Ok(())
    }

    #[test]
    fn restart_analysis_ownership_fails_closed_with_exact_evidence_and_source_errors()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let transaction = durable_identity(90, 1)?;
        let mut contradictory =
            FakeDurablePageRecoverySource::new(lineage.clone(), Vec::new(), Vec::new(), Vec::new());
        contradictory.restart_frontier = Some(lineage.position(2));
        contradictory.restart_observations = vec![
            restart_commit(&lineage, transaction, 1)?,
            restart_commit(&lineage, transaction, 2)?,
        ];
        let store = FakeBatchCommittedPageRecoveryStore::new(lineage.clone());

        let page_recovered = UnrecoveredTransactionPageStorage::new(contradictory, store)
            .recover()
            .map_err(|_| TestError("empty page recovery failed"))?;
        let failure = page_recovered
            .analyze_restart()
            .err()
            .ok_or(TestError("duplicate commit reached live storage"))?;
        assert!(failure.recovery_report().pages().is_empty());
        assert!(matches!(
            failure.error(),
            DurableTransactionRestartAnalysisError::Evidence(source)
                if matches!(
                    source.as_ref(),
                    DurableTransactionRestartAnalysisEvidenceError::DuplicateCommit {
                        transaction: actual,
                        first_commit_position,
                        duplicate_commit_position,
                    } if *actual == transaction
                        && first_commit_position == &lineage.position(1)
                        && duplicate_commit_position == &lineage.position(2)
                )
        ));
        assert!(Error::source(&failure).is_some());

        let mut unavailable =
            FakeDurablePageRecoverySource::new(lineage.clone(), Vec::new(), Vec::new(), Vec::new());
        unavailable.restart_before_callback_error = Some(FakeFault("restart source"));
        let store = FakeBatchCommittedPageRecoveryStore::new(lineage);
        let page_recovered = UnrecoveredTransactionPageStorage::new(unavailable, store)
            .recover()
            .map_err(|_| TestError("source-failure page recovery failed"))?;
        let failure = page_recovered
            .analyze_restart()
            .err()
            .ok_or(TestError("restart source failure reached live storage"))?;
        assert!(failure.recovery_report().pages().is_empty());
        assert!(matches!(
            failure.error(),
            DurableTransactionRestartAnalysisError::Source(FakeFault("restart source"))
        ));
        assert!(Error::source(&failure).is_some());
        Ok(())
    }

    struct FakeDurableTransactionRestartSource {
        lineage: LogLineage,
        durable_frontier: Option<LogSequenceNumber>,
        observations: Vec<DurableTransactionRestartObservation<1>>,
        before_callback_error: Option<FakeFault>,
        after_callback_error: Option<FakeFault>,
        callbacks: usize,
    }

    impl FakeDurableTransactionRestartSource {
        fn new(
            lineage: LogLineage,
            durable_frontier: Option<LogSequenceNumber>,
            observations: Vec<DurableTransactionRestartObservation<1>>,
        ) -> Self {
            Self {
                lineage,
                durable_frontier,
                observations,
                before_callback_error: None,
                after_callback_error: None,
                callbacks: 0,
            }
        }
    }

    impl DurableTransactionRestartAnalysisSource<1> for FakeDurableTransactionRestartSource {
        type Error = FakeFault;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn with_durable_transaction_restart_observations<Output, Operation>(
            &mut self,
            operation: Operation,
        ) -> Result<Output, Self::Error>
        where
            Operation: for<'evidence> FnOnce(
                Option<&'evidence LogSequenceNumber>,
                &'evidence [DurableTransactionRestartObservation<1>],
            ) -> Output,
        {
            if let Some(source) = self.before_callback_error.take() {
                return Err(source);
            }
            self.callbacks += 1;
            let output = operation(self.durable_frontier.as_ref(), &self.observations);
            match self.after_callback_error.take() {
                Some(source) => Err(source),
                None => Ok(output),
            }
        }
    }

    fn restart_raw_page(
        lineage: &LogLineage,
        page_number: u64,
        position: u64,
    ) -> Result<DurableTransactionRestartObservation<1>, TestError> {
        Ok(DurableTransactionRestartObservation::Page(
            physical_page_observation(
                lineage,
                page_number,
                position,
                u8::try_from(page_number).map_err(|_| TestError("raw page byte"))?,
                position,
            )?,
        ))
    }

    fn restart_owned_page(
        lineage: &LogLineage,
        transaction: DurableTransactionIdentityObservation,
        page_number: u64,
        position: u64,
    ) -> Result<DurableTransactionRestartObservation<1>, TestError> {
        Ok(DurableTransactionRestartObservation::TransactionPage(
            durable_page_observation(
                lineage,
                transaction,
                page_number,
                position,
                u8::try_from(page_number).map_err(|_| TestError("owned page byte"))?,
                position,
            )?,
        ))
    }

    fn restart_commit(
        lineage: &LogLineage,
        transaction: DurableTransactionIdentityObservation,
        position: u64,
    ) -> Result<DurableTransactionRestartObservation<1>, TestError> {
        Ok(DurableTransactionRestartObservation::Commit(
            durable_commit_observation(lineage, transaction, position)?,
        ))
    }

    fn restart_evidence_error(
        result: Result<
            DurableTransactionRestartAnalysis,
            DurableTransactionRestartAnalysisError<FakeFault>,
        >,
    ) -> Result<DurableTransactionRestartAnalysisEvidenceError, TestError> {
        match result {
            Err(DurableTransactionRestartAnalysisError::Evidence(source)) => Ok(*source),
            Err(DurableTransactionRestartAnalysisError::Source(_)) => Err(TestError(
                "expected restart evidence error, found source error",
            )),
            Ok(_) => Err(TestError("expected restart evidence error")),
        }
    }

    type FakeRestartAnalyzedStorage = RestartAnalyzedTransactionPageStorage<
        FakeDurablePageRecoverySource,
        FakeBatchCommittedPageRecoveryStore,
        1,
    >;

    fn restart_analyzed_checkpoint_owner(
        lineage: &LogLineage,
        durable_frontier: Option<LogSequenceNumber>,
        observations: Vec<DurableTransactionRestartObservation<1>>,
    ) -> Result<FakeRestartAnalyzedStorage, TestError> {
        let mut source =
            FakeDurablePageRecoverySource::new(lineage.clone(), Vec::new(), Vec::new(), Vec::new());
        source.restart_frontier = durable_frontier;
        source.restart_observations = observations;
        let store = FakeBatchCommittedPageRecoveryStore::new(lineage.clone());
        let page_recovered = UnrecoveredTransactionPageStorage::new(source, store)
            .recover()
            .map_err(|_| TestError("checkpoint owner page recovery"))?;
        page_recovered
            .analyze_restart()
            .map_err(|_| TestError("checkpoint owner restart analysis"))
    }

    fn batch_restart_analyzed_checkpoint_owner(
        lineage: &LogLineage,
        page_numbers: &[u64],
    ) -> Result<FakeRestartAnalyzedStorage, TestError> {
        let source = batch_recovery_source(lineage, page_numbers)?;
        let store = FakeBatchCommittedPageRecoveryStore::new(lineage.clone());
        let page_recovered = UnrecoveredTransactionPageStorage::new(source, store)
            .recover()
            .map_err(|_| TestError("batch checkpoint owner page recovery"))?;
        page_recovered
            .analyze_restart()
            .map_err(|_| TestError("batch checkpoint owner restart analysis"))
    }

    fn decoded_checkpoint_entry(
        entry: &DurableTransactionRestartCheckpointBaselineEntry,
    ) -> DurableTransactionRestartCheckpointBaselineEntryObservation {
        let state = match entry.state() {
            DurableTransactionRestartCheckpointBaselineState::Uncommitted => {
                DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted
            }
            DurableTransactionRestartCheckpointBaselineState::Committed { commit_position } => {
                DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                    commit_position,
                }
            }
        };
        DurableTransactionRestartCheckpointBaselineEntryObservation::new(
            entry.transaction().epoch(),
            entry.transaction().sequence(),
            entry.first_owned_page_position(),
            entry.last_owned_page_position(),
            entry.owned_page_record_count(),
            state,
        )
    }

    fn decoded_checkpoint_entries(
        baseline: &DurableTransactionRestartCheckpointBaseline,
    ) -> Vec<DurableTransactionRestartCheckpointBaselineEntryObservation> {
        baseline
            .transactions()
            .iter()
            .map(decoded_checkpoint_entry)
            .collect()
    }

    fn decoded_checkpoint_observation<'evidence>(
        baseline: &DurableTransactionRestartCheckpointBaseline,
        transactions: &'evidence [DurableTransactionRestartCheckpointBaselineEntryObservation],
    ) -> DurableTransactionRestartCheckpointBaselineObservation<'evidence> {
        DurableTransactionRestartCheckpointBaselineObservation::new(
            baseline.persistent_log_id().get(),
            baseline.durable_frontier(),
            transactions,
        )
    }

    fn owned_decoded_checkpoint(
        baseline: &DurableTransactionRestartCheckpointBaseline,
    ) -> OwnedDurableTransactionRestartCheckpointBaselineObservation {
        OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
            baseline.persistent_log_id().get(),
            baseline.durable_frontier(),
            decoded_checkpoint_entries(baseline),
        )
    }

    struct FakeCheckpointBaselineSource {
        checkpoint: Option<OwnedDurableTransactionRestartCheckpointBaselineObservation>,
        fault: Option<FakeFault>,
        calls: usize,
        events: Option<Rc<RefCell<Vec<&'static str>>>>,
    }

    impl FakeCheckpointBaselineSource {
        fn new(
            checkpoint: Option<OwnedDurableTransactionRestartCheckpointBaselineObservation>,
        ) -> Self {
            Self {
                checkpoint,
                fault: None,
                calls: 0,
                events: None,
            }
        }
    }

    impl DurableTransactionRestartCheckpointBaselineSource for FakeCheckpointBaselineSource {
        type Error = FakeFault;

        fn load_restart_checkpoint_baseline(
            &mut self,
        ) -> Result<Option<OwnedDurableTransactionRestartCheckpointBaselineObservation>, Self::Error>
        {
            self.calls += 1;
            if let Some(events) = &self.events {
                events.borrow_mut().push("checkpoint");
            }
            match self.fault.take() {
                Some(source) => Err(source),
                None => Ok(self.checkpoint.clone()),
            }
        }
    }

    fn checkpoint_validation_evidence_error(
        result: Result<
            DurableTransactionRestartCheckpointBaseline,
            DurableTransactionRestartCheckpointBaselineValidationError<FakeFault>,
        >,
    ) -> Result<DurableTransactionRestartCheckpointBaselineValidationEvidenceError, TestError> {
        match result {
            Err(DurableTransactionRestartCheckpointBaselineValidationError::Evidence(source)) => {
                Ok(*source)
            }
            Err(DurableTransactionRestartCheckpointBaselineValidationError::Source(_)) => Err(
                TestError("expected checkpoint validation evidence error, found source error"),
            ),
            Ok(_) => Err(TestError("expected checkpoint validation evidence error")),
        }
    }

    #[test]
    fn restart_analysis_accepts_authoritatively_empty_prefix() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut source =
            FakeDurableTransactionRestartSource::new(lineage.clone(), None, Vec::new());

        let analysis = analyze_durable_transaction_restart(&mut source)
            .map_err(|_| TestError("empty restart analysis"))?;

        assert!(analysis.lineage().same_lineage(&lineage));
        assert_eq!(analysis.durable_frontier(), None);
        assert!(analysis.transactions().is_empty());
        assert_eq!(source.callbacks, 1);
        Ok(())
    }

    #[test]
    fn restart_checkpoint_baseline_distinguishes_empty_and_raw_only_prefixes()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1280).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let mut empty_source =
            FakeDurableTransactionRestartSource::new(lineage.clone(), None, Vec::new());

        let empty_analysis = analyze_durable_transaction_restart(&mut empty_source)
            .map_err(|_| TestError("empty persistent restart analysis"))?;
        let empty = prepare_restart_checkpoint_baseline(&empty_analysis)
            .map_err(|_| TestError("empty restart checkpoint baseline"))?;

        assert_eq!(empty.persistent_log_id(), persistent_log_id);
        assert_eq!(empty.durable_frontier(), None);
        assert!(empty.transactions().is_empty());

        let mut raw_source = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(9)),
            vec![restart_raw_page(&lineage, 90, 9)?],
        );
        let raw_analysis = analyze_durable_transaction_restart(&mut raw_source)
            .map_err(|_| TestError("raw-only persistent restart analysis"))?;
        let raw = prepare_restart_checkpoint_baseline(&raw_analysis)
            .map_err(|_| TestError("raw-only restart checkpoint baseline"))?;

        assert_eq!(raw.persistent_log_id(), persistent_log_id);
        assert_eq!(raw.durable_frontier(), Some(9));
        assert!(raw.transactions().is_empty());
        Ok(())
    }

    #[test]
    fn restart_checkpoint_baseline_preserves_exact_sorted_transaction_table()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1281).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let uncommitted = durable_identity(40, 1)?;
        let committed = durable_identity(41, 2)?;
        let commit_only = durable_identity(42, 3)?;
        let observations = vec![
            restart_raw_page(&lineage, 90, 2)?,
            restart_owned_page(&lineage, committed, 91, 4)?,
            restart_owned_page(&lineage, uncommitted, 92, 7)?,
            restart_owned_page(&lineage, committed, 93, 8)?,
            restart_commit(&lineage, committed, 10)?,
            restart_commit(&lineage, commit_only, 12)?,
            restart_owned_page(&lineage, uncommitted, 94, 15)?,
        ];
        let mut source = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(15)),
            observations,
        );
        let analysis = analyze_durable_transaction_restart(&mut source)
            .map_err(|_| TestError("persistent interleaved restart analysis"))?;

        let baseline = prepare_restart_checkpoint_baseline(&analysis)
            .map_err(|_| TestError("interleaved restart checkpoint baseline"))?;

        assert_eq!(baseline.persistent_log_id(), persistent_log_id);
        assert_eq!(baseline.durable_frontier(), Some(15));
        assert_eq!(
            baseline
                .transactions()
                .iter()
                .map(DurableTransactionRestartCheckpointBaselineEntry::transaction)
                .collect::<Vec<_>>(),
            [uncommitted, committed, commit_only]
        );
        let transactions = baseline.transactions();
        assert_eq!(transactions[0].first_owned_page_position(), Some(7));
        assert_eq!(transactions[0].last_owned_page_position(), Some(15));
        assert_eq!(transactions[0].owned_page_record_count(), 2);
        assert_eq!(
            transactions[0].state(),
            DurableTransactionRestartCheckpointBaselineState::Uncommitted
        );

        assert_eq!(transactions[1].first_owned_page_position(), Some(4));
        assert_eq!(transactions[1].last_owned_page_position(), Some(8));
        assert_eq!(transactions[1].owned_page_record_count(), 2);
        assert_eq!(transactions[1].state().commit_position(), Some(10));

        assert_eq!(transactions[2].first_owned_page_position(), None);
        assert_eq!(transactions[2].last_owned_page_position(), None);
        assert_eq!(transactions[2].owned_page_record_count(), 0);
        assert_eq!(transactions[2].state().commit_position(), Some(12));
        Ok(())
    }

    #[test]
    fn restart_checkpoint_baseline_capacity_failure_is_typed() {
        let mut transactions = Vec::new();

        assert_eq!(
            reserve_restart_checkpoint_baseline_transactions(&mut transactions, usize::MAX),
            Err(
                DurableTransactionRestartCheckpointBaselineError::TransactionCapacityExhausted {
                    transaction_count: usize::MAX
                }
            )
        );
        assert!(transactions.is_empty());
    }

    #[test]
    fn decoded_checkpoint_validation_returns_exact_authoritative_current_baseline()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1300).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let mut owner = batch_restart_analyzed_checkpoint_owner(&lineage, &[81, 82, 83])?;
        let baseline = owner
            .prepare_restart_checkpoint_baseline()
            .map_err(|_| TestError("prepare current checkpoint baseline"))?;
        let entries = decoded_checkpoint_entries(&baseline);
        let observation = decoded_checkpoint_observation(&baseline, &entries);
        let store_observations = owner.parts().1.observations.borrow().clone();
        let store_attempts = owner.parts().1.attempts.clone();

        let validated = owner
            .validate_restart_checkpoint_baseline_against_current_prefix(&observation)
            .map_err(|_| TestError("validate current checkpoint baseline"))?;

        assert_eq!(validated, baseline);
        assert_eq!(owner.parts().0.restart_callbacks, 2);
        assert_eq!(
            owner.parts().1.observations.borrow().as_slice(),
            store_observations
        );
        assert_eq!(owner.parts().1.attempts, store_attempts);
        Ok(())
    }

    #[test]
    fn decoded_checkpoint_validation_distinguishes_empty_and_raw_only_stale_prefixes()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1301).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let transaction = durable_identity(130, 1)?;
        let observations = vec![
            restart_raw_page(&lineage, 90, 2)?,
            restart_commit(&lineage, transaction, 4)?,
        ];
        let mut owner =
            restart_analyzed_checkpoint_owner(&lineage, Some(lineage.position(4)), observations)?;
        let no_entries = [];
        let empty = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            None,
            &no_entries,
        );

        let validated_empty = owner
            .validate_restart_checkpoint_baseline_against_current_prefix(&empty)
            .map_err(|_| TestError("validate empty checkpoint baseline"))?;

        assert_eq!(validated_empty.durable_frontier(), None);
        assert!(validated_empty.transactions().is_empty());

        let raw_only = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            Some(2),
            &no_entries,
        );
        let validated_raw = owner
            .validate_restart_checkpoint_baseline_against_current_prefix(&raw_only)
            .map_err(|_| TestError("validate raw-only checkpoint baseline"))?;

        assert_eq!(validated_raw.durable_frontier(), Some(2));
        assert!(validated_raw.transactions().is_empty());
        assert_eq!(owner.parts().0.restart_callbacks, 3);
        assert!(owner.parts().1.observations.borrow().is_empty());
        assert!(owner.parts().1.attempts.is_empty());
        Ok(())
    }

    #[test]
    fn decoded_checkpoint_validation_rejects_identity_and_zero_frontier_before_callback()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1302).ok_or(TestError("persistent log id"))?;
        let foreign_log_id =
            PersistentLogId::new(0x1303).ok_or(TestError("foreign persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let mut owner = batch_restart_analyzed_checkpoint_owner(&lineage, &[81])?;
        let no_entries = [];
        let callbacks = owner.parts().0.restart_callbacks;

        let zero_id =
            DurableTransactionRestartCheckpointBaselineObservation::new(0, None, &no_entries);
        assert_eq!(
            checkpoint_validation_evidence_error(
                owner.validate_restart_checkpoint_baseline_against_current_prefix(&zero_id)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ZeroPersistentLogId {
                persistent_log_id: 0
            }
        );

        let foreign_id = DurableTransactionRestartCheckpointBaselineObservation::new(
            foreign_log_id.get(),
            None,
            &no_entries,
        );
        assert_eq!(
            checkpoint_validation_evidence_error(
                owner.validate_restart_checkpoint_baseline_against_current_prefix(&foreign_id)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ForeignPersistentLogId {
                expected: persistent_log_id,
                actual: foreign_log_id,
            }
        );

        let zero_frontier = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            Some(0),
            &no_entries,
        );
        assert_eq!(
            checkpoint_validation_evidence_error(
                owner.validate_restart_checkpoint_baseline_against_current_prefix(&zero_frontier)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ZeroCheckpointFrontier {
                checkpoint_frontier: 0
            }
        );
        assert_eq!(owner.parts().0.restart_callbacks, callbacks);

        let ephemeral = LogLineage::new();
        let mut ephemeral_owner = batch_restart_analyzed_checkpoint_owner(&ephemeral, &[82])?;
        let apparently_persistent = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            None,
            &no_entries,
        );
        let ephemeral_callbacks = ephemeral_owner.parts().0.restart_callbacks;
        assert_eq!(
            checkpoint_validation_evidence_error(
                ephemeral_owner
                    .validate_restart_checkpoint_baseline_against_current_prefix(
                        &apparently_persistent
                    )
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CurrentPersistentLineageRequired
        );
        assert_eq!(
            ephemeral_owner.parts().0.restart_callbacks,
            ephemeral_callbacks
        );
        Ok(())
    }

    #[test]
    fn decoded_checkpoint_validation_rejects_future_gap_and_invalid_current_frontiers()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1304).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let no_entries = [];
        let mut dense_owner = batch_restart_analyzed_checkpoint_owner(&lineage, &[81])?;
        let future = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            Some(3),
            &no_entries,
        );
        assert_eq!(
            checkpoint_validation_evidence_error(
                dense_owner
                    .validate_restart_checkpoint_baseline_against_current_prefix(&future)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CheckpointBeyondDurableFrontier {
                checkpoint_frontier: 3,
                durable_frontier: Some(2),
            }
        );

        let transaction = durable_identity(131, 1)?;
        let observations = vec![
            restart_raw_page(&lineage, 90, 2)?,
            restart_commit(&lineage, transaction, 4)?,
        ];
        let mut gapped_owner =
            restart_analyzed_checkpoint_owner(&lineage, Some(lineage.position(4)), observations)?;
        let gap = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            Some(3),
            &no_entries,
        );
        assert_eq!(
            checkpoint_validation_evidence_error(
                gapped_owner.validate_restart_checkpoint_baseline_against_current_prefix(&gap)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CheckpointFrontierNotRecordBoundary {
                checkpoint_frontier: 3,
                durable_frontier: 4,
            }
        );

        let checkpoint = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            Some(2),
            &no_entries,
        );
        let foreign = LogLineage::new();
        gapped_owner.parts_mut().0.restart_frontier = Some(foreign.position(4));
        assert!(matches!(
            checkpoint_validation_evidence_error(
                gapped_owner
                    .validate_restart_checkpoint_baseline_against_current_prefix(&checkpoint)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CurrentPrefix(source)
                if matches!(
                    source.as_ref(),
                    DurableTransactionRestartAnalysisEvidenceError::ForeignFrontier { frontier }
                        if frontier == &foreign.position(4)
                )
        ));

        gapped_owner.parts_mut().0.restart_frontier = Some(lineage.position(0));
        assert!(matches!(
            checkpoint_validation_evidence_error(
                gapped_owner
                    .validate_restart_checkpoint_baseline_against_current_prefix(&checkpoint)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CurrentPrefix(source)
                if matches!(
                    source.as_ref(),
                    DurableTransactionRestartAnalysisEvidenceError::ZeroFrontier { frontier }
                        if frontier == &lineage.position(0)
                )
        ));

        gapped_owner.parts_mut().0.restart_frontier = None;
        assert!(matches!(
            checkpoint_validation_evidence_error(
                gapped_owner
                    .validate_restart_checkpoint_baseline_against_current_prefix(&checkpoint)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CurrentPrefix(source)
                if matches!(
                    source.as_ref(),
                    DurableTransactionRestartAnalysisEvidenceError::FrontierMissing {
                        record_count: 2
                    }
                )
        ));
        Ok(())
    }

    #[test]
    fn decoded_checkpoint_validation_compares_every_transaction_entry_field()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1305).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let mut owner = batch_restart_analyzed_checkpoint_owner(&lineage, &[81])?;
        let baseline = owner
            .prepare_restart_checkpoint_baseline()
            .map_err(|_| TestError("prepare mismatch baseline"))?;
        let entries = decoded_checkpoint_entries(&baseline);
        let original = entries[0];
        let wrong_entries = [
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                0,
                original.sequence(),
                original.first_owned_page_position(),
                original.last_owned_page_position(),
                original.owned_page_record_count(),
                original.state(),
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                original.epoch(),
                0,
                original.first_owned_page_position(),
                original.last_owned_page_position(),
                original.owned_page_record_count(),
                original.state(),
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                original.epoch(),
                original.sequence(),
                None,
                original.last_owned_page_position(),
                original.owned_page_record_count(),
                original.state(),
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                original.epoch(),
                original.sequence(),
                Some(9),
                original.last_owned_page_position(),
                original.owned_page_record_count(),
                original.state(),
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                original.epoch(),
                original.sequence(),
                original.first_owned_page_position(),
                None,
                original.owned_page_record_count(),
                original.state(),
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                original.epoch(),
                original.sequence(),
                original.first_owned_page_position(),
                Some(9),
                original.owned_page_record_count(),
                original.state(),
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                original.epoch(),
                original.sequence(),
                original.first_owned_page_position(),
                original.last_owned_page_position(),
                0,
                original.state(),
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                original.epoch(),
                original.sequence(),
                original.first_owned_page_position(),
                original.last_owned_page_position(),
                original.owned_page_record_count(),
                DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted,
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                original.epoch(),
                original.sequence(),
                original.first_owned_page_position(),
                original.last_owned_page_position(),
                original.owned_page_record_count(),
                DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                    commit_position: 9,
                },
            ),
        ];

        for actual in wrong_entries {
            let actual_entries = [actual];
            let observation = DurableTransactionRestartCheckpointBaselineObservation::new(
                persistent_log_id.get(),
                baseline.durable_frontier(),
                &actual_entries,
            );
            assert!(matches!(
                checkpoint_validation_evidence_error(
                    owner.validate_restart_checkpoint_baseline_against_current_prefix(&observation)
                )?,
                DurableTransactionRestartCheckpointBaselineValidationEvidenceError::TransactionEntryMismatch {
                    index: 0,
                    expected,
                    actual: rejected,
                } if expected.as_ref() == &baseline.transactions()[0] && rejected == actual
            ));
        }

        let no_entries = [];
        let missing = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            baseline.durable_frontier(),
            &no_entries,
        );
        assert_eq!(
            checkpoint_validation_evidence_error(
                owner.validate_restart_checkpoint_baseline_against_current_prefix(&missing)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::TransactionCountMismatch {
                expected: 1,
                actual: 0,
            }
        );
        Ok(())
    }

    #[test]
    fn decoded_checkpoint_validation_ignores_suffix_after_stale_boundary_but_validates_selected_prefix()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1306).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let mut owner = batch_restart_analyzed_checkpoint_owner(&lineage, &[81, 82])?;
        let baseline = owner
            .prepare_restart_checkpoint_baseline()
            .map_err(|_| TestError("prepare stale checkpoint baseline"))?;
        let entries = decoded_checkpoint_entries(&baseline);
        let stale = decoded_checkpoint_observation(&baseline, &entries);
        let duplicate_owner = baseline.transactions()[0].transaction();
        let duplicate_commit = restart_commit(&lineage, duplicate_owner, 6)?;
        {
            let (source, _) = owner.parts_mut();
            source.restart_observations.push(duplicate_commit);
            source.restart_frontier = Some(lineage.position(6));
        }

        let validated = owner
            .validate_restart_checkpoint_baseline_against_current_prefix(&stale)
            .map_err(|_| TestError("stale checkpoint rejected by malformed suffix"))?;
        assert_eq!(validated, baseline);

        let selected_suffix = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            Some(6),
            &entries,
        );
        assert!(matches!(
            checkpoint_validation_evidence_error(
                owner.validate_restart_checkpoint_baseline_against_current_prefix(&selected_suffix)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::SelectedPrefix(source)
                if matches!(
                    source.as_ref(),
                    DurableTransactionRestartAnalysisEvidenceError::DuplicateCommit {
                        transaction,
                        first_commit_position,
                        duplicate_commit_position,
                    } if *transaction == duplicate_owner
                        && first_commit_position == &lineage.position(2)
                        && duplicate_commit_position == &lineage.position(6)
                )
        ));

        let missing_boundary = DurableTransactionRestartCheckpointBaselineObservation::new(
            persistent_log_id.get(),
            Some(5),
            &entries,
        );
        assert!(matches!(
            checkpoint_validation_evidence_error(
                owner.validate_restart_checkpoint_baseline_against_current_prefix(&missing_boundary)
            )?,
            DurableTransactionRestartCheckpointBaselineValidationEvidenceError::CurrentPrefix(source)
                if matches!(
                    source.as_ref(),
                    DurableTransactionRestartAnalysisEvidenceError::DuplicateCommit { .. }
                )
        ));
        Ok(())
    }

    #[test]
    fn decoded_checkpoint_validation_preserves_source_errors_before_and_after_callback()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1307).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let mut owner = batch_restart_analyzed_checkpoint_owner(&lineage, &[81])?;
        let baseline = owner
            .prepare_restart_checkpoint_baseline()
            .map_err(|_| TestError("prepare source-error baseline"))?;
        let entries = decoded_checkpoint_entries(&baseline);
        let observation = decoded_checkpoint_observation(&baseline, &entries);
        let callbacks = owner.parts().0.restart_callbacks;

        owner.parts_mut().0.restart_before_callback_error = Some(FakeFault("before validation"));
        let before = owner
            .validate_restart_checkpoint_baseline_against_current_prefix(&observation)
            .err()
            .ok_or(TestError("before-callback validation source error"))?;
        assert!(matches!(
            before,
            DurableTransactionRestartCheckpointBaselineValidationError::Source(FakeFault(
                "before validation"
            ))
        ));
        assert_eq!(owner.parts().0.restart_callbacks, callbacks);

        owner.parts_mut().0.restart_after_callback_error = Some(FakeFault("after validation"));
        let after = owner
            .validate_restart_checkpoint_baseline_against_current_prefix(&observation)
            .err()
            .ok_or(TestError("after-callback validation source error"))?;
        assert_eq!(
            Error::source(&after).map(ToString::to_string),
            Some(String::from("after validation"))
        );
        assert!(matches!(
            after,
            DurableTransactionRestartCheckpointBaselineValidationError::Source(FakeFault(
                "after validation"
            ))
        ));
        assert_eq!(owner.parts().0.restart_callbacks, callbacks + 1);

        let validated = owner
            .validate_restart_checkpoint_baseline_against_current_prefix(&observation)
            .map_err(|_| TestError("validation did not recover after source faults"))?;
        assert_eq!(validated, baseline);
        assert_eq!(owner.parts().0.restart_callbacks, callbacks + 2);
        Ok(())
    }

    #[test]
    fn owned_decoded_checkpoint_retains_raw_fields_without_authorizing_them() {
        let transaction = DurableTransactionRestartCheckpointBaselineEntryObservation::new(
            0,
            0,
            Some(0),
            None,
            u64::MAX,
            DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                commit_position: 0,
            },
        );
        let owned = OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
            0,
            Some(0),
            vec![transaction],
        );

        assert_eq!(owned.persistent_log_id(), 0);
        assert_eq!(owned.durable_frontier(), Some(0));
        assert_eq!(owned.transactions(), [transaction]);
        let borrowed = owned.as_observation();
        assert_eq!(borrowed.persistent_log_id(), 0);
        assert_eq!(borrowed.durable_frontier(), Some(0));
        assert_eq!(borrowed.transactions(), [transaction]);
    }

    #[test]
    fn current_checkpoint_preparation_reanalyzes_without_changing_startup_or_store()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1320).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let mut owner = batch_restart_analyzed_checkpoint_owner(&lineage, &[81])?;
        let startup = owner
            .prepare_restart_checkpoint_baseline()
            .map_err(|_| TestError("prepare startup checkpoint"))?;
        let startup_frontier = owner
            .restart_analysis()
            .durable_frontier()
            .ok_or(TestError("startup frontier"))?
            .clone();
        let store_observations = owner.parts().1.observations.borrow().clone();
        let store_attempts = owner.parts().1.attempts.clone();
        {
            let (source, _) = owner.parts_mut();
            source
                .restart_observations
                .push(restart_raw_page(&lineage, 90, 4)?);
            source.restart_frontier = Some(lineage.position(4));
        }

        let current = owner
            .prepare_restart_checkpoint_baseline_from_current_prefix()
            .map_err(|_| TestError("prepare current checkpoint"))?;

        assert_eq!(startup.durable_frontier(), Some(2));
        assert_eq!(current.durable_frontier(), Some(4));
        assert_eq!(current.transactions(), startup.transactions());
        assert_eq!(
            owner.restart_analysis().durable_frontier(),
            Some(&startup_frontier)
        );
        assert_eq!(owner.parts().0.restart_callbacks, 2);
        assert_eq!(
            owner.parts().1.observations.borrow().as_slice(),
            store_observations
        );
        assert_eq!(owner.parts().1.attempts, store_attempts);

        let duplicate_owner = startup.transactions()[0].transaction();
        {
            let (source, _) = owner.parts_mut();
            source
                .restart_observations
                .push(restart_commit(&lineage, duplicate_owner, 6)?);
            source.restart_frontier = Some(lineage.position(6));
        }
        let malformed = owner
            .prepare_restart_checkpoint_baseline_from_current_prefix()
            .err()
            .ok_or(TestError("malformed current checkpoint preparation"))?;
        assert!(Error::source(&malformed).is_some());
        assert!(matches!(
            malformed,
            DurableTransactionRestartCheckpointBaselineCurrentPreparationError::Analysis(
                DurableTransactionRestartAnalysisError::Evidence(source)
            ) if matches!(
                source.as_ref(),
                DurableTransactionRestartAnalysisEvidenceError::DuplicateCommit {
                    transaction,
                    first_commit_position,
                    duplicate_commit_position,
                } if *transaction == duplicate_owner
                    && first_commit_position == &lineage.position(2)
                    && duplicate_commit_position == &lineage.position(6)
            )
        ));
        {
            let (source, _) = owner.parts_mut();
            source
                .restart_observations
                .pop()
                .ok_or(TestError("malformed suffix disappeared"))?;
            source.restart_frontier = Some(lineage.position(4));
        }
        assert_eq!(
            owner
                .prepare_restart_checkpoint_baseline_from_current_prefix()
                .map_err(|_| TestError("current preparation did not recover"))?
                .durable_frontier(),
            Some(4)
        );

        let ephemeral = LogLineage::new();
        let mut ephemeral_owner = batch_restart_analyzed_checkpoint_owner(&ephemeral, &[82])?;
        assert!(matches!(
            ephemeral_owner
                .prepare_restart_checkpoint_baseline_from_current_prefix()
                .err(),
            Some(
                DurableTransactionRestartCheckpointBaselineCurrentPreparationError::BaselinePreparation(
                    DurableTransactionRestartCheckpointBaselineError::PersistentLineageRequired
                )
            )
        ));
        assert_eq!(ephemeral_owner.parts().0.restart_callbacks, 2);
        Ok(())
    }

    #[test]
    fn owned_checkpoint_source_composition_is_sequential_optional_and_non_consuming()
    -> Result<(), TestError> {
        let persistent_log_id =
            PersistentLogId::new(0x1321).ok_or(TestError("persistent log id"))?;
        let lineage = LogLineage::persistent(persistent_log_id);
        let mut owner = batch_restart_analyzed_checkpoint_owner(&lineage, &[81, 82])?;
        let baseline = owner
            .prepare_restart_checkpoint_baseline()
            .map_err(|_| TestError("prepare source checkpoint"))?;
        let events = Rc::new(RefCell::new(Vec::new()));
        owner.parts_mut().0.restart_events = Some(Rc::clone(&events));
        let callbacks = owner.parts().0.restart_callbacks;
        let store_observations = owner.parts().1.observations.borrow().clone();
        let store_attempts = owner.parts().1.attempts.clone();
        let mut exact =
            FakeCheckpointBaselineSource::new(Some(owned_decoded_checkpoint(&baseline)));
        exact.events = Some(Rc::clone(&events));

        let validated = owner
            .validate_restart_checkpoint_baseline_from_source(&mut exact)
            .map_err(|_| TestError("validate owned checkpoint source"))?
            .ok_or(TestError("owned checkpoint source disappeared"))?;

        assert_eq!(validated, baseline);
        assert_eq!(exact.calls, 1);
        assert_eq!(events.borrow().as_slice(), ["checkpoint", "wal"]);
        assert_eq!(owner.parts().0.restart_callbacks, callbacks + 1);

        let mut absent = FakeCheckpointBaselineSource::new(None);
        absent.events = Some(Rc::clone(&events));
        assert_eq!(
            owner
                .validate_restart_checkpoint_baseline_from_source(&mut absent)
                .map_err(|_| TestError("absent checkpoint source failed"))?,
            None
        );
        assert_eq!(absent.calls, 1);
        assert_eq!(
            events.borrow().as_slice(),
            ["checkpoint", "wal", "checkpoint"]
        );
        assert_eq!(owner.parts().0.restart_callbacks, callbacks + 1);

        let invalid =
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(0, None, Vec::new());
        let mut invalid_source = FakeCheckpointBaselineSource::new(Some(invalid));
        invalid_source.events = Some(Rc::clone(&events));
        let invalid_error = owner
            .validate_restart_checkpoint_baseline_from_source(&mut invalid_source)
            .err()
            .ok_or(TestError("invalid owned checkpoint source"))?;
        assert!(matches!(
            invalid_error,
            DurableTransactionRestartCheckpointBaselineSourceValidationError::BaselineValidation(
                source
            ) if matches!(
                source.as_ref(),
                DurableTransactionRestartCheckpointBaselineValidationError::Evidence(evidence)
                    if matches!(
                        evidence.as_ref(),
                        DurableTransactionRestartCheckpointBaselineValidationEvidenceError::ZeroPersistentLogId {
                            persistent_log_id: 0
                        }
                    )
            )
        ));
        assert_eq!(owner.parts().0.restart_callbacks, callbacks + 1);

        let mut unavailable =
            FakeCheckpointBaselineSource::new(Some(owned_decoded_checkpoint(&baseline)));
        unavailable.fault = Some(FakeFault("checkpoint unavailable"));
        unavailable.events = Some(Rc::clone(&events));
        let unavailable_error = owner
            .validate_restart_checkpoint_baseline_from_source(&mut unavailable)
            .err()
            .ok_or(TestError("checkpoint source failure"))?;
        assert_eq!(
            Error::source(&unavailable_error).map(ToString::to_string),
            Some(String::from("checkpoint unavailable"))
        );
        assert!(matches!(
            unavailable_error,
            DurableTransactionRestartCheckpointBaselineSourceValidationError::CheckpointSource(
                FakeFault("checkpoint unavailable")
            )
        ));
        assert_eq!(owner.parts().0.restart_callbacks, callbacks + 1);

        let validated_again = owner
            .validate_restart_checkpoint_baseline_from_source(&mut exact)
            .map_err(|_| TestError("owner unusable after checkpoint source failures"))?
            .ok_or(TestError("exact checkpoint disappeared"))?;
        assert_eq!(validated_again, baseline);
        assert_eq!(
            events.borrow().as_slice(),
            [
                "checkpoint",
                "wal",
                "checkpoint",
                "checkpoint",
                "checkpoint",
                "checkpoint",
                "wal",
            ]
        );
        assert_eq!(owner.parts().0.restart_callbacks, callbacks + 2);
        assert_eq!(
            owner.parts().1.observations.borrow().as_slice(),
            store_observations
        );
        assert_eq!(owner.parts().1.attempts, store_attempts);
        Ok(())
    }

    #[test]
    fn restart_analysis_builds_identity_sorted_table_from_interleaved_gapped_stream()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let uncommitted = durable_identity(40, 1)?;
        let committed = durable_identity(41, 2)?;
        let commit_only = durable_identity(42, 3)?;
        let observations = vec![
            restart_raw_page(&lineage, 90, 2)?,
            restart_owned_page(&lineage, committed, 91, 4)?,
            restart_owned_page(&lineage, uncommitted, 92, 7)?,
            restart_owned_page(&lineage, committed, 93, 8)?,
            restart_commit(&lineage, committed, 10)?,
            restart_commit(&lineage, commit_only, 12)?,
            restart_owned_page(&lineage, uncommitted, 94, 15)?,
        ];
        assert_eq!(
            observations
                .iter()
                .map(DurableTransactionRestartObservation::kind)
                .collect::<Vec<_>>(),
            [
                DurableTransactionRestartObservationKind::Page,
                DurableTransactionRestartObservationKind::TransactionPage,
                DurableTransactionRestartObservationKind::TransactionPage,
                DurableTransactionRestartObservationKind::TransactionPage,
                DurableTransactionRestartObservationKind::Commit,
                DurableTransactionRestartObservationKind::Commit,
                DurableTransactionRestartObservationKind::TransactionPage,
            ]
        );
        let mut source = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(15)),
            observations,
        );

        let analysis = analyze_durable_transaction_restart(&mut source)
            .map_err(|_| TestError("interleaved restart analysis"))?;
        assert_eq!(analysis.durable_frontier(), Some(&lineage.position(15)));
        let transactions = analysis.transactions();
        assert_eq!(
            transactions
                .iter()
                .map(DurableTransactionRestartEntry::transaction)
                .collect::<Vec<_>>(),
            [uncommitted, committed, commit_only]
        );

        assert_eq!(
            transactions[0].first_owned_page_position(),
            Some(&lineage.position(7))
        );
        assert_eq!(
            transactions[0].last_owned_page_position(),
            Some(&lineage.position(15))
        );
        assert_eq!(transactions[0].owned_page_record_count(), 2);
        assert_eq!(
            transactions[0].state(),
            &DurableTransactionRestartState::Uncommitted
        );

        assert_eq!(
            transactions[1].first_owned_page_position(),
            Some(&lineage.position(4))
        );
        assert_eq!(
            transactions[1].last_owned_page_position(),
            Some(&lineage.position(8))
        );
        assert_eq!(transactions[1].owned_page_record_count(), 2);
        assert_eq!(
            transactions[1].state().commit_position(),
            Some(&lineage.position(10))
        );

        assert_eq!(transactions[2].first_owned_page_position(), None);
        assert_eq!(transactions[2].last_owned_page_position(), None);
        assert_eq!(transactions[2].owned_page_record_count(), 0);
        assert_eq!(
            transactions[2].state().commit_position(),
            Some(&lineage.position(12))
        );

        let (analyzed_lineage, frontier, transactions) = analysis.into_parts();
        assert!(analyzed_lineage.same_lineage(&lineage));
        assert_eq!(frontier, Some(lineage.position(15)));
        assert_eq!(transactions.len(), 3);
        assert_eq!(source.callbacks, 1);
        Ok(())
    }

    #[test]
    fn restart_analysis_rejects_every_frontier_shape_before_table_construction()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let transaction = durable_identity(43, 1)?;

        let mut foreign_frontier = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(foreign.position(1)),
            Vec::new(),
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(
                &mut foreign_frontier
            ))?,
            DurableTransactionRestartAnalysisEvidenceError::ForeignFrontier { frontier }
                if frontier == foreign.position(1)
        ));

        let mut zero_frontier = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(0)),
            Vec::new(),
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut zero_frontier))?,
            DurableTransactionRestartAnalysisEvidenceError::ZeroFrontier { frontier }
                if frontier == lineage.position(0)
        ));

        let mut missing_frontier = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            None,
            vec![restart_commit(&lineage, transaction, 1)?],
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut missing_frontier))?,
            DurableTransactionRestartAnalysisEvidenceError::FrontierMissing { record_count: 1 }
        ));

        let mut empty_with_frontier = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(1)),
            Vec::new(),
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(
                &mut empty_with_frontier
            ))?,
            DurableTransactionRestartAnalysisEvidenceError::FrontierWithoutRecords { frontier }
                if frontier == lineage.position(1)
        ));

        let mut stale_frontier = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(3)),
            vec![restart_commit(&lineage, transaction, 2)?],
        );
        let error = analyze_durable_transaction_restart(&mut stale_frontier)
            .err()
            .ok_or(TestError("tail/frontier mismatch must fail"))?;
        assert!(Error::source(&error).is_some());
        assert!(matches!(
            restart_evidence_error(Err(error))?,
            DurableTransactionRestartAnalysisEvidenceError::TailFrontierMismatch {
                frontier,
                tail,
            } if frontier == lineage.position(3) && tail == lineage.position(2)
        ));
        Ok(())
    }

    #[test]
    fn restart_analysis_rejects_foreign_duplicate_contradictory_and_decreasing_records()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let transaction = durable_identity(44, 1)?;

        let mut foreign_record = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(2)),
            vec![restart_commit(&foreign, transaction, 2)?],
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut foreign_record))?,
            DurableTransactionRestartAnalysisEvidenceError::ForeignRecordLineage {
                index: 0,
                kind: DurableTransactionRestartObservationKind::Commit,
                position,
            } if position == foreign.position(2)
        ));

        let mut duplicate = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(2)),
            vec![
                restart_raw_page(&lineage, 95, 2)?,
                restart_raw_page(&lineage, 95, 2)?,
            ],
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut duplicate))?,
            DurableTransactionRestartAnalysisEvidenceError::DuplicateRecordPosition {
                previous_index: 0,
                actual_index: 1,
                kind: DurableTransactionRestartObservationKind::Page,
                position,
            } if position == lineage.position(2)
        ));

        let mut contradictory = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(2)),
            vec![
                restart_raw_page(&lineage, 96, 2)?,
                restart_commit(&lineage, transaction, 2)?,
            ],
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut contradictory))?,
            DurableTransactionRestartAnalysisEvidenceError::ContradictoryRecordPosition {
                previous_index: 0,
                previous_kind: DurableTransactionRestartObservationKind::Page,
                actual_index: 1,
                actual_kind: DurableTransactionRestartObservationKind::Commit,
                position,
            } if position == lineage.position(2)
        ));

        let mut decreasing = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(2)),
            vec![
                restart_raw_page(&lineage, 97, 3)?,
                restart_commit(&lineage, transaction, 2)?,
            ],
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut decreasing))?,
            DurableTransactionRestartAnalysisEvidenceError::NonAdvancingRecordPosition {
                previous_index: 0,
                previous,
                actual_index: 1,
                actual,
            } if previous == lineage.position(3) && actual == lineage.position(2)
        ));
        Ok(())
    }

    #[test]
    fn restart_analysis_validates_complete_stream_before_transaction_contradictions()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let transaction = durable_identity(45, 1)?;
        let mut source = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(2)),
            vec![
                restart_commit(&lineage, transaction, 1)?,
                restart_commit(&lineage, transaction, 3)?,
                restart_raw_page(&lineage, 98, 2)?,
            ],
        );

        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut source))?,
            DurableTransactionRestartAnalysisEvidenceError::NonAdvancingRecordPosition {
                previous_index: 1,
                previous,
                actual_index: 2,
                actual,
            } if previous == lineage.position(3) && actual == lineage.position(2)
        ));
        Ok(())
    }

    #[test]
    fn restart_analysis_rejects_duplicate_commit_and_page_after_commit() -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let duplicate_owner = durable_identity(46, 1)?;
        let mut duplicate = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(5)),
            vec![
                restart_owned_page(&lineage, duplicate_owner, 99, 1)?,
                restart_commit(&lineage, duplicate_owner, 3)?,
                restart_commit(&lineage, duplicate_owner, 5)?,
            ],
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut duplicate))?,
            DurableTransactionRestartAnalysisEvidenceError::DuplicateCommit {
                transaction,
                first_commit_position,
                duplicate_commit_position,
            } if transaction == duplicate_owner
                && first_commit_position == lineage.position(3)
                && duplicate_commit_position == lineage.position(5)
        ));

        let page_after_owner = durable_identity(46, 2)?;
        let mut page_after = FakeDurableTransactionRestartSource::new(
            lineage.clone(),
            Some(lineage.position(5)),
            vec![
                restart_commit(&lineage, page_after_owner, 2)?,
                restart_raw_page(&lineage, 100, 4)?,
                restart_owned_page(&lineage, page_after_owner, 101, 5)?,
            ],
        );
        assert!(matches!(
            restart_evidence_error(analyze_durable_transaction_restart(&mut page_after))?,
            DurableTransactionRestartAnalysisEvidenceError::PageAfterCommit {
                transaction,
                commit_position,
                page_position,
            } if transaction == page_after_owner
                && commit_position == lineage.position(2)
                && page_position == lineage.position(5)
        ));
        Ok(())
    }

    #[test]
    fn restart_analysis_preserves_source_failures_before_and_after_callback()
    -> Result<(), TestError> {
        let lineage = LogLineage::new();
        let mut before =
            FakeDurableTransactionRestartSource::new(lineage.clone(), None, Vec::new());
        before.before_callback_error = Some(FakeFault("before restart evidence"));
        let before_error = analyze_durable_transaction_restart(&mut before)
            .err()
            .ok_or(TestError("before-callback source error must fail"))?;
        assert!(matches!(
            before_error,
            DurableTransactionRestartAnalysisError::Source(FakeFault("before restart evidence"))
        ));
        assert_eq!(before.callbacks, 0);

        let mut after = FakeDurableTransactionRestartSource::new(lineage, None, Vec::new());
        after.after_callback_error = Some(FakeFault("after restart evidence"));
        let after_error = analyze_durable_transaction_restart(&mut after)
            .err()
            .ok_or(TestError("after-callback source error must fail"))?;
        assert_eq!(
            Error::source(&after_error).map(ToString::to_string),
            Some(String::from("after restart evidence"))
        );
        assert!(matches!(
            after_error,
            DurableTransactionRestartAnalysisError::Source(FakeFault("after restart evidence"))
        ));
        assert_eq!(after.callbacks, 1);
        Ok(())
    }
}
