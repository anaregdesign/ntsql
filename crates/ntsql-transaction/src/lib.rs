//! I/O-free transaction lifecycle invariants.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    error::Error,
    fmt,
    num::NonZeroU64,
    sync::Arc,
};

use ntsql_page::{
    CleanPage, DirtyPage, DurablePageWalObservation, FlushDirtyPageError,
    IndeterminatePageLogAppend, IndeterminatePageWrite, PageLog, PageNumber,
    PageRecoveryObservationBytesErrorReason, PageStore, PageVersion, StagePageWriteError,
    StagePageWriteEvidenceErrorReason, UnloggedPage, flush_dirty_page, stage_page_write,
};
use ntsql_wal::{
    CommitError, CommitLog, LogDurability, LogLineage, LogSequenceNumber, commit_durability,
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

    use std::{cell::RefCell, rc::Rc};

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

    fn durable_page_observation(
        lineage: &LogLineage,
        owner: DurableTransactionIdentityObservation,
        number: u64,
        version: u64,
        byte: u8,
        position: u64,
    ) -> Result<DurableTransactionPageObservation<1>, TestError> {
        let number = PageNumber::new(number).ok_or(TestError("durable page number"))?;
        let page = DurablePageWalObservation::from_bytes(
            number,
            PageVersion::new(version),
            [byte],
            lineage.position(position),
        )
        .map_err(|_| TestError("durable page observation"))?;
        Ok(DurableTransactionPageObservation::new(owner, page))
    }

    fn durable_commit_observation(
        lineage: &LogLineage,
        transaction: DurableTransactionIdentityObservation,
        position: u64,
    ) -> Result<DurableTransactionCommitObservation, TestError> {
        DurableTransactionCommitObservation::new(transaction, lineage.position(position))
            .map_err(|_| TestError("durable commit observation"))
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
}
