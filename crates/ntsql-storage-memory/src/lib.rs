//! Deterministic in-memory persistence adapter for transaction/page WAL tests.

use std::{error::Error, fmt, num::NonZeroU64};

use ntsql_page::{
    DurablePageWalObservation, PageLog, PageNumber, PageRecoveryObservationBytesError, PageStore,
    PageVersion, PageWritePermit, StoredPageSnapshotObservation, UnloggedPage,
};
use ntsql_transaction::{
    CommittedTransactionPageRecoveryStore, CommittedTransactionPageRecoveryWritePermit,
    DurableCommitLookup, DurableCommittedTransactionPageRecoveryCandidate,
    DurableCommittedTransactionPageRecoveryComparison,
    DurableCommittedTransactionPageRecoveryComparisonError, DurableTransactionCommitObservation,
    DurableTransactionCommitObservationFieldsError, DurableTransactionPageObservation,
    DurableTransactionPageObservationBytesError, DurableTransactionPageRecoveryInventory,
    DurableTransactionPageRecoverySource, DurableTransactionRestartAnalysisSource,
    DurableTransactionRestartCheckpointBaseline,
    DurableTransactionRestartCheckpointBaselineEntryObservation,
    DurableTransactionRestartCheckpointBaselinePublicationPermit,
    DurableTransactionRestartCheckpointBaselinePublisher,
    DurableTransactionRestartCheckpointBaselineSource,
    DurableTransactionRestartCheckpointBaselineState,
    DurableTransactionRestartCheckpointBaselineStateObservation,
    DurableTransactionRestartCheckpointCompletenessBaseline,
    DurableTransactionRestartCheckpointCompletenessBaselinePageObservation,
    DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation,
    DurableTransactionRestartCheckpointCompletenessBaselinePublicationPermit,
    DurableTransactionRestartCheckpointCompletenessBaselinePublisher,
    DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineSource,
    DurableTransactionRestartObservation, DurableTransactionRestartPageEntry,
    DurableTransactionRestartPageState, DurableTransactionRestartReplayStart,
    DurableTransactionRestartReplayStartCause, DurableTransactionRestartRequiredPageImage,
    OwnedDurableTransactionRestartCheckpointBaselineObservation,
    OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
    TransactionCommitRecord, TransactionEpochSource, TransactionId, TransactionPageLog,
    TransactionPageWriteRecord, TransactionRecoverySource,
    compare_committed_transaction_page_recovery_candidate,
};
use ntsql_wal::{CommitLog, LogDurability, LogLineage, LogSequenceNumber, PersistentLogId};

/// One-shot physical-effect boundary for the next matching log operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultPoint {
    /// Fail before an append changes the in-memory log.
    BeforeAppend,
    /// Append a volatile record, then report append failure.
    AfterAppend,
    /// Fail before a flush advances the durable prefix.
    BeforeFlush,
    /// Advance the durable prefix, then report flush failure.
    AfterFlush,
}

impl fmt::Display for FaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeAppend => formatter.write_str("before append"),
            Self::AfterAppend => formatter.write_str("after append"),
            Self::BeforeFlush => formatter.write_str("before flush"),
            Self::AfterFlush => formatter.write_str("after flush"),
        }
    }
}

/// Immutable snapshot of one physically appended full page image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InMemoryPageWriteRecord<const N: usize = 1> {
    page_number: PageNumber,
    page_version: PageVersion,
    bytes: [u8; N],
}

impl<const N: usize> InMemoryPageWriteRecord<N> {
    fn from_unlogged(page: &UnloggedPage<N>) -> Self {
        Self {
            page_number: page.address().number(),
            page_version: page.version(),
            bytes: *page.image().bytes(),
        }
    }

    /// Returns the copied page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.page_number
    }

    /// Returns the copied page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.page_version
    }

    /// Returns the borrowed page bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Returns the owned page bytes.
    #[must_use]
    pub fn into_bytes(self) -> [u8; N] {
        self.bytes
    }
}

/// Immutable snapshot of one physically appended transaction-owned page image.
///
/// This owns the exact owning [`TransactionId`] alongside the same
/// [`InMemoryPageWriteRecord`] payload that a nontransactional page record
/// snapshots. It has no public constructor: safe downstream code cannot forge a
/// transaction-owned page record, and only the adapter's private append path
/// builds one from a domain [`TransactionPageWriteRecord`].
///
/// ```compile_fail
/// use ntsql_storage_memory::{InMemoryPageWriteRecord, InMemoryTransactionPageWriteRecord};
/// use ntsql_transaction::TransactionId;
///
/// fn cannot_construct(transaction_id: TransactionId, page: InMemoryPageWriteRecord<1>) {
///     let _forged = InMemoryTransactionPageWriteRecord {
///         transaction_id,
///         page,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InMemoryTransactionPageWriteRecord<const N: usize = 1> {
    transaction_id: TransactionId,
    page: InMemoryPageWriteRecord<N>,
}

impl<const N: usize> InMemoryTransactionPageWriteRecord<N> {
    fn from_record(record: &TransactionPageWriteRecord<'_, N>) -> Self {
        Self {
            transaction_id: record.transaction_id(),
            page: InMemoryPageWriteRecord::from_unlogged(record.page()),
        }
    }

    /// Returns the transaction identity that owns this page write.
    ///
    /// This is a page-ownership tag, not a durable commit. It never implies the
    /// owning transaction committed.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// Returns the copied full page-image payload.
    #[must_use]
    pub const fn page_write(&self) -> &InMemoryPageWriteRecord<N> {
        &self.page
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InMemoryLogRecordKind<const N: usize = 1> {
    /// One transaction commit record.
    TransactionCommit {
        /// The caller-owned transaction identity.
        transaction_id: TransactionId,
    },
    /// One complete nontransactional page-image write.
    PageWrite(InMemoryPageWriteRecord<N>),
    /// One complete transaction-owned page-image write.
    TransactionPageWrite(InMemoryTransactionPageWriteRecord<N>),
}

impl<const N: usize> InMemoryLogRecordKind<N> {
    /// Returns the transaction identity when this record is a commit.
    ///
    /// This is commit-only on purpose: a transaction-owned page record returns
    /// `None` so [`TransactionRecoverySource`] scans of this accessor never
    /// mistake page ownership for a durable commit. Use
    /// [`Self::page_owner_transaction_id`] to inspect page ownership.
    #[must_use]
    pub const fn transaction_id(&self) -> Option<TransactionId> {
        match self {
            Self::TransactionCommit { transaction_id } => Some(*transaction_id),
            Self::PageWrite(_) | Self::TransactionPageWrite(_) => None,
        }
    }

    /// Returns the full page-image payload for both page-write record kinds.
    ///
    /// A commit record returns `None`. Both the nontransactional and the
    /// transaction-owned page records return their identical page payload.
    #[must_use]
    pub const fn page_write(&self) -> Option<&InMemoryPageWriteRecord<N>> {
        match self {
            Self::TransactionCommit { .. } => None,
            Self::PageWrite(record) => Some(record),
            Self::TransactionPageWrite(record) => Some(record.page_write()),
        }
    }

    /// Returns the owning transaction identity only for a transaction-owned page
    /// record.
    ///
    /// A commit record and a nontransactional page record both return `None`.
    /// This owner tag is never a durable commit signal.
    #[must_use]
    pub const fn page_owner_transaction_id(&self) -> Option<TransactionId> {
        match self {
            Self::TransactionCommit { .. } | Self::PageWrite(_) => None,
            Self::TransactionPageWrite(record) => Some(record.transaction_id()),
        }
    }

    /// Returns the typed transaction-owned page record when present.
    #[must_use]
    pub const fn transaction_page_write(&self) -> Option<&InMemoryTransactionPageWriteRecord<N>> {
        match self {
            Self::TransactionCommit { .. } | Self::PageWrite(_) => None,
            Self::TransactionPageWrite(record) => Some(record),
        }
    }
}

/// Immutable snapshot of one physically appended transaction or page record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InMemoryLogRecord<const N: usize = 1> {
    position: LogSequenceNumber,
    kind: InMemoryLogRecordKind<N>,
}

impl<const N: usize> InMemoryLogRecord<N> {
    /// Returns the adapter-assigned log position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.position
    }

    /// Returns the safely inspectable record payload.
    #[must_use]
    pub const fn kind(&self) -> &InMemoryLogRecordKind<N> {
        &self.kind
    }

    /// Returns the transaction identity when this record is a commit.
    ///
    /// This stays commit-only: a transaction-owned page record returns `None`
    /// here. Recovery commit scans must never observe page ownership through
    /// this accessor.
    #[must_use]
    pub const fn transaction_id(&self) -> Option<TransactionId> {
        self.kind.transaction_id()
    }

    /// Returns the full page-image payload for either page-write record kind.
    #[must_use]
    pub const fn page_write(&self) -> Option<&InMemoryPageWriteRecord<N>> {
        self.kind.page_write()
    }

    /// Returns the owning transaction identity only for a transaction-owned page
    /// record.
    ///
    /// A commit record and a nontransactional page record both return `None`.
    #[must_use]
    pub const fn page_owner_transaction_id(&self) -> Option<TransactionId> {
        self.kind.page_owner_transaction_id()
    }

    /// Returns the typed transaction-owned page record when present.
    #[must_use]
    pub const fn transaction_page_write(&self) -> Option<&InMemoryTransactionPageWriteRecord<N>> {
        self.kind.transaction_page_write()
    }

    /// Projects a page record into adapter-neutral recovery evidence.
    ///
    /// Callers must select records from a commit log's durable prefix before
    /// treating the result as durable. Both page-write record kinds project
    /// their exact page payload; commit records return `Ok(None)`. The owning
    /// transaction identity is intentionally not carried into the observation:
    /// ADR 0019 reconciliation stays commit-agnostic and non-authorizing.
    pub fn page_recovery_observation(
        &self,
    ) -> Result<Option<DurablePageWalObservation<N>>, PageRecoveryObservationBytesError<N>> {
        match self.page_write() {
            Some(record) => self.project_page_recovery_observation(record).map(Some),
            None => Ok(None),
        }
    }

    /// Projects an owned-page record into transaction-aware recovery evidence.
    ///
    /// Callers must select this record from the complete durable prefix before
    /// treating the result as durable. Commit and raw page records return
    /// `Ok(None)`. An owned page intentionally also projects through
    /// [`Self::page_recovery_observation`] for commit-agnostic physical
    /// reconciliation; callers must not double-count those two views.
    pub fn transaction_page_recovery_observation(
        &self,
    ) -> Result<
        Option<DurableTransactionPageObservation<N>>,
        DurableTransactionPageObservationBytesError<N>,
    > {
        match self.transaction_page_write() {
            Some(record) => self
                .project_transaction_page_recovery_observation(record)
                .map(Some),
            None => Ok(None),
        }
    }

    /// Projects a commit record into transaction-aware recovery evidence.
    ///
    /// Callers must select this record from the complete durable prefix before
    /// treating the result as durable. Both page record kinds return
    /// `Ok(None)`, so page ownership remains separate from commitment.
    pub fn transaction_commit_recovery_observation(
        &self,
    ) -> Result<
        Option<DurableTransactionCommitObservation>,
        DurableTransactionCommitObservationFieldsError,
    > {
        match self.transaction_id() {
            Some(transaction_id) => self
                .project_transaction_commit_recovery_observation(transaction_id)
                .map(Some),
            None => Ok(None),
        }
    }

    fn project_page_recovery_observation(
        &self,
        record: &InMemoryPageWriteRecord<N>,
    ) -> Result<DurablePageWalObservation<N>, PageRecoveryObservationBytesError<N>> {
        DurablePageWalObservation::from_bytes(
            record.page_number(),
            record.page_version(),
            *record.bytes(),
            self.position.clone(),
        )
    }

    fn project_transaction_page_recovery_observation(
        &self,
        record: &InMemoryTransactionPageWriteRecord<N>,
    ) -> Result<DurableTransactionPageObservation<N>, DurableTransactionPageObservationBytesError<N>>
    {
        let transaction_id = record.transaction_id();
        let page = record.page_write();
        DurableTransactionPageObservation::from_bytes(
            transaction_id.epoch().get(),
            transaction_id.sequence(),
            page.page_number(),
            page.page_version(),
            *page.bytes(),
            self.position.clone(),
        )
    }

    fn project_transaction_commit_recovery_observation(
        &self,
        transaction_id: TransactionId,
    ) -> Result<DurableTransactionCommitObservation, DurableTransactionCommitObservationFieldsError>
    {
        DurableTransactionCommitObservation::from_fields(
            transaction_id.epoch().get(),
            transaction_id.sequence(),
            self.position.clone(),
        )
    }
}

/// Failure while executing an in-memory commit-log operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InMemoryCommitLogError {
    /// The armed fault fired at its exact physical-effect boundary.
    InjectedFault(FaultPoint),
    /// The supplied page belongs to another log lineage.
    ForeignPageLineage(PageNumber),
    /// No appended record owns the requested flush position.
    UnknownFlushPosition(LogSequenceNumber),
    /// The requested position belongs to another log lineage.
    ForeignFlushPosition(LogSequenceNumber),
    /// The record snapshot could not reserve additional memory.
    RecordCapacityExhausted,
    /// The adapter has already assigned every `u64` position.
    PositionSpaceExhausted,
}

impl fmt::Display for InMemoryCommitLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InjectedFault(point) => {
                write!(formatter, "injected commit-log failure {point}")
            }
            Self::ForeignPageLineage(page_number) => write!(
                formatter,
                "commit-log page {} belongs to another lineage",
                page_number.get()
            ),
            Self::UnknownFlushPosition(position) => write!(
                formatter,
                "commit-log position {} was not appended",
                position.get()
            ),
            Self::ForeignFlushPosition(position) => write!(
                formatter,
                "commit-log position {} belongs to another lineage",
                position.get()
            ),
            Self::RecordCapacityExhausted => {
                formatter.write_str("commit-log record capacity is exhausted")
            }
            Self::PositionSpaceExhausted => {
                formatter.write_str("commit-log position space is exhausted")
            }
        }
    }
}

impl Error for InMemoryCommitLogError {}

/// Refusal to silently replace an already armed one-shot commit-log fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultAlreadyArmed {
    armed: FaultPoint,
    requested: FaultPoint,
}

impl FaultAlreadyArmed {
    /// Returns the fault that remains armed.
    #[must_use]
    pub const fn armed(&self) -> FaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> FaultPoint {
        self.requested
    }
}

impl fmt::Display for FaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for FaultAlreadyArmed {}

/// One-shot physical-effect boundary for the next matching page-store operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageStoreFaultPoint {
    /// Fail before any durable page mutation occurs.
    BeforeWrite,
    /// Apply the durable page mutation, then report failure.
    AfterWrite,
}

impl fmt::Display for PageStoreFaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeWrite => formatter.write_str("before write"),
            Self::AfterWrite => formatter.write_str("after write"),
        }
    }
}

/// Refusal to silently replace an already armed one-shot page-store fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageStoreFaultAlreadyArmed {
    armed: PageStoreFaultPoint,
    requested: PageStoreFaultPoint,
}

impl PageStoreFaultAlreadyArmed {
    /// Returns the fault that remains armed.
    #[must_use]
    pub const fn armed(&self) -> PageStoreFaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> PageStoreFaultPoint {
        self.requested
    }
}

impl fmt::Display for PageStoreFaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "page-store fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for PageStoreFaultAlreadyArmed {}

/// Durable snapshot of one stored page image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InMemoryStoredPage<const N: usize = 1> {
    page_number: PageNumber,
    page_version: PageVersion,
    bytes: [u8; N],
    required_position: LogSequenceNumber,
}

impl<const N: usize> InMemoryStoredPage<N> {
    fn from_dirty(page: &ntsql_page::DirtyPage<N>) -> Self {
        Self {
            page_number: page.address().number(),
            page_version: page.version(),
            bytes: *page.image().bytes(),
            required_position: page.required_position().clone(),
        }
    }

    fn from_recovery_candidate(
        candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
    ) -> Self {
        let target = candidate.latest_committed().observation();
        Self {
            page_number: target.page().page_number(),
            page_version: target.page().page_version(),
            bytes: *target.page().image().bytes(),
            required_position: target.position().clone(),
        }
    }

    /// Returns the copied page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.page_number
    }

    /// Returns the copied page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.page_version
    }

    /// Returns the borrowed page bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Returns the exact durable WAL position paired with this snapshot.
    #[must_use]
    pub const fn required_position(&self) -> &LogSequenceNumber {
        &self.required_position
    }

    /// Projects this durable snapshot into adapter-neutral recovery evidence.
    pub fn page_recovery_observation(
        &self,
    ) -> Result<StoredPageSnapshotObservation<N>, PageRecoveryObservationBytesError<N>> {
        StoredPageSnapshotObservation::from_bytes(
            self.page_number,
            self.page_version,
            self.bytes,
            self.required_position.clone(),
        )
    }

    /// Returns the owned page bytes.
    #[must_use]
    pub fn into_bytes(self) -> [u8; N] {
        self.bytes
    }
}

/// Failure while executing an in-memory page-store write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InMemoryPageStoreError {
    /// The armed fault fired at its exact physical-effect boundary.
    InjectedFault(PageStoreFaultPoint),
    /// The dirty page belongs to another lineage.
    ForeignPageLineage(PageNumber),
    /// The durable-write permit belongs to another lineage.
    ForeignPermitPosition(LogSequenceNumber),
    /// The durable-write permit does not match the page's required position.
    PermitPositionMismatch {
        /// The page's exact required position.
        expected: LogSequenceNumber,
        /// The supplied permit position.
        actual: LogSequenceNumber,
    },
    /// The snapshot table could not reserve capacity for a new page number.
    PageCapacityExhausted,
}

impl fmt::Display for InMemoryPageStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InjectedFault(point) => {
                write!(formatter, "injected page-store failure {point}")
            }
            Self::ForeignPageLineage(page_number) => write!(
                formatter,
                "page-store page {} belongs to another lineage",
                page_number.get()
            ),
            Self::ForeignPermitPosition(position) => write!(
                formatter,
                "page-store permit position {} belongs to another lineage",
                position.get()
            ),
            Self::PermitPositionMismatch { expected, actual } => write!(
                formatter,
                "page-store permit position {} does not match required position {}",
                actual.get(),
                expected.get()
            ),
            Self::PageCapacityExhausted => {
                formatter.write_str("page-store page capacity is exhausted")
            }
        }
    }
}

impl Error for InMemoryPageStoreError {}

/// Failure while atomically comparing and replacing one in-memory recovery page.
#[derive(Debug, Eq, PartialEq)]
pub enum InMemoryCommittedPageRecoveryStoreError<const N: usize> {
    /// The armed fault fired at its exact physical-effect boundary.
    InjectedFault(PageStoreFaultPoint),
    /// The candidate target page position belongs to another lineage.
    ForeignTargetPagePosition(LogSequenceNumber),
    /// The candidate target commit position belongs to another lineage.
    ForeignTargetCommitPosition(LogSequenceNumber),
    /// The recovery permit page position belongs to another lineage.
    ForeignPermitPagePosition(LogSequenceNumber),
    /// The recovery permit commit position belongs to another lineage.
    ForeignPermitCommitPosition(LogSequenceNumber),
    /// The permit page position differs from the candidate target.
    PermitPagePositionMismatch {
        /// Candidate target page position.
        expected: LogSequenceNumber,
        /// Supplied permit page position.
        actual: LogSequenceNumber,
    },
    /// The permit commit position differs from the candidate target.
    PermitCommitPositionMismatch {
        /// Candidate target commit position.
        expected: LogSequenceNumber,
        /// Supplied permit commit position.
        actual: LogSequenceNumber,
    },
    /// Current store state could not be projected during the atomic recheck.
    CurrentObservation(Box<PageRecoveryObservationBytesError<N>>),
    /// Current store state contradicted the candidate.
    SourceComparison(Box<DurableCommittedTransactionPageRecoveryComparisonError>),
    /// Current store state was valid but no longer matched the candidate source.
    SourceNotMatched {
        /// Non-source comparison observed under the store hold.
        actual: DurableCommittedTransactionPageRecoveryComparison,
    },
    /// The snapshot table could not reserve capacity for a missing page.
    PageCapacityExhausted,
}

impl<const N: usize> fmt::Display for InMemoryCommittedPageRecoveryStoreError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InjectedFault(point) => {
                write!(formatter, "injected recovery page-store failure {point}")
            }
            Self::ForeignTargetPagePosition(position) => write!(
                formatter,
                "recovery target page position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignTargetCommitPosition(position) => write!(
                formatter,
                "recovery target commit position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignPermitPagePosition(position) => write!(
                formatter,
                "recovery permit page position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignPermitCommitPosition(position) => write!(
                formatter,
                "recovery permit commit position {} belongs to another lineage",
                position.get()
            ),
            Self::PermitPagePositionMismatch { expected, actual } => write!(
                formatter,
                "recovery permit page position {} does not match target position {}",
                actual.get(),
                expected.get()
            ),
            Self::PermitCommitPositionMismatch { expected, actual } => write!(
                formatter,
                "recovery permit commit position {} does not match target position {}",
                actual.get(),
                expected.get()
            ),
            Self::CurrentObservation(source) => {
                write!(
                    formatter,
                    "recovery current-page observation failed: {source}"
                )
            }
            Self::SourceComparison(source) => {
                write!(formatter, "recovery source comparison failed: {source}")
            }
            Self::SourceNotMatched { actual } => write!(
                formatter,
                "recovery source no longer matches the candidate: {actual:?}"
            ),
            Self::PageCapacityExhausted => {
                formatter.write_str("recovery page-store capacity is exhausted")
            }
        }
    }
}

impl<const N: usize> Error for InMemoryCommittedPageRecoveryStoreError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentObservation(source) => Some(source.as_ref()),
            Self::SourceComparison(source) => Some(source.as_ref()),
            Self::InjectedFault(_)
            | Self::ForeignTargetPagePosition(_)
            | Self::ForeignTargetCommitPosition(_)
            | Self::ForeignPermitPagePosition(_)
            | Self::ForeignPermitCommitPosition(_)
            | Self::PermitPagePositionMismatch { .. }
            | Self::PermitCommitPositionMismatch { .. }
            | Self::SourceNotMatched { .. }
            | Self::PageCapacityExhausted => None,
        }
    }
}

/// Failure to allocate a fresh coordinator epoch in this model lineage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryTransactionEpochError {
    /// Every nonzero `u64` epoch has already been issued.
    EpochSpaceExhausted,
}

impl fmt::Display for InMemoryTransactionEpochError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EpochSpaceExhausted => {
                formatter.write_str("transaction epoch space is exhausted")
            }
        }
    }
}

impl Error for InMemoryTransactionEpochError {}

/// Failure to establish an authoritative transaction outcome from this model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryTransactionRecoveryError {
    /// The matching record exists only in the volatile suffix.
    VolatileCommitRecord(TransactionId),
    /// More than one physical record carries the same complete identity.
    DuplicateCommitRecord(TransactionId),
}

impl fmt::Display for InMemoryTransactionRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VolatileCommitRecord(transaction_id) => write!(
                formatter,
                "transaction {transaction_id} has a volatile commit record"
            ),
            Self::DuplicateCommitRecord(transaction_id) => write!(
                formatter,
                "transaction {transaction_id} has duplicate commit records"
            ),
        }
    }
}

impl Error for InMemoryTransactionRecoveryError {}

/// Failure to reconstruct a memory log as a later storage runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryLogReopenError {
    /// Ephemeral pointer identity has no stable value to reconstruct.
    EphemeralLineage,
}

impl fmt::Display for InMemoryLogReopenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EphemeralLineage => {
                formatter.write_str("ephemeral commit-log lineage cannot be reopened")
            }
        }
    }
}

impl Error for InMemoryLogReopenError {}

/// Failure to inventory durable transaction-owned pages in memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryCommittedPageRecoveryInventoryError {
    /// The owned-page inventory could not reserve its durable-prefix upper bound.
    PageCapacityExhausted,
}

impl fmt::Display for InMemoryCommittedPageRecoveryInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PageCapacityExhausted => {
                formatter.write_str("in-memory recovery page inventory capacity is exhausted")
            }
        }
    }
}

impl Error for InMemoryCommittedPageRecoveryInventoryError {}

/// Projection whose recovery evidence could not reserve memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryPageRecoveryProjection {
    /// Commit-agnostic physical page observations.
    PhysicalPages,
    /// Transaction-owner-aware page observations.
    TransactionPages,
    /// Complete durable commit observations.
    Commits,
}

impl fmt::Display for InMemoryPageRecoveryProjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PhysicalPages => formatter.write_str("physical page"),
            Self::TransactionPages => formatter.write_str("transaction page"),
            Self::Commits => formatter.write_str("commit"),
        }
    }
}

/// Failure to project one stable in-memory durable prefix for page recovery.
#[derive(Debug, Eq, PartialEq)]
pub enum InMemoryCommittedPageRecoverySourceError<const N: usize> {
    /// One projection could not reserve enough memory before scanning.
    EvidenceCapacityExhausted {
        /// Projection whose allocation failed.
        projection: InMemoryPageRecoveryProjection,
    },
    /// A matching physical page record could not become domain evidence.
    PhysicalPageProjection(Box<PageRecoveryObservationBytesError<N>>),
    /// A matching transaction-owned page record could not become domain evidence.
    TransactionPageProjection(Box<DurableTransactionPageObservationBytesError<N>>),
    /// A durable commit record could not become domain evidence.
    CommitProjection(Box<DurableTransactionCommitObservationFieldsError>),
}

impl<const N: usize> fmt::Display for InMemoryCommittedPageRecoverySourceError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EvidenceCapacityExhausted { projection } => write!(
                formatter,
                "in-memory {projection} recovery evidence capacity is exhausted"
            ),
            Self::PhysicalPageProjection(source) => {
                write!(
                    formatter,
                    "physical page recovery projection failed: {source}"
                )
            }
            Self::TransactionPageProjection(source) => {
                write!(
                    formatter,
                    "transaction page recovery projection failed: {source}"
                )
            }
            Self::CommitProjection(source) => {
                write!(formatter, "commit recovery projection failed: {source}")
            }
        }
    }
}

impl<const N: usize> Error for InMemoryCommittedPageRecoverySourceError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PhysicalPageProjection(source) => Some(source.as_ref()),
            Self::TransactionPageProjection(source) => Some(source.as_ref()),
            Self::CommitProjection(source) => Some(source.as_ref()),
            Self::EvidenceCapacityExhausted { .. } => None,
        }
    }
}

/// Failure to project one complete in-memory durable prefix for restart analysis.
#[derive(Debug, Eq, PartialEq)]
pub enum InMemoryTransactionRestartAnalysisSourceError<const N: usize> {
    /// The unified observation stream could not reserve its durable-prefix bound.
    ObservationCapacityExhausted {
        /// Exact number of durable logical records that required reservation.
        record_count: usize,
    },
    /// One raw page record could not become adapter-neutral restart evidence.
    PageProjection(Box<PageRecoveryObservationBytesError<N>>),
    /// One transaction-owned page could not become restart evidence.
    TransactionPageProjection(Box<DurableTransactionPageObservationBytesError<N>>),
    /// One transaction commit could not become restart evidence.
    CommitProjection(Box<DurableTransactionCommitObservationFieldsError>),
}

impl<const N: usize> fmt::Display for InMemoryTransactionRestartAnalysisSourceError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObservationCapacityExhausted { record_count } => write!(
                formatter,
                "in-memory restart observation capacity is exhausted for {record_count} durable records"
            ),
            Self::PageProjection(source) => {
                write!(formatter, "raw page restart projection failed: {source}")
            }
            Self::TransactionPageProjection(source) => {
                write!(
                    formatter,
                    "transaction page restart projection failed: {source}"
                )
            }
            Self::CommitProjection(source) => {
                write!(formatter, "commit restart projection failed: {source}")
            }
        }
    }
}

impl<const N: usize> Error for InMemoryTransactionRestartAnalysisSourceError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PageProjection(source) => Some(source.as_ref()),
            Self::TransactionPageProjection(source) => Some(source.as_ref()),
            Self::CommitProjection(source) => Some(source.as_ref()),
            Self::ObservationCapacityExhausted { .. } => None,
        }
    }
}

/// One-shot failure boundary for the next checkpoint baseline load.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointBaselineSourceFaultPoint {
    /// Fail before inspecting or copying the optional seeded slot.
    BeforeLoad,
}

impl fmt::Display for RestartCheckpointBaselineSourceFaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeLoad => formatter.write_str("before load"),
        }
    }
}

/// Refusal to replace an already armed checkpoint-source fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartCheckpointBaselineSourceFaultAlreadyArmed {
    armed: RestartCheckpointBaselineSourceFaultPoint,
    requested: RestartCheckpointBaselineSourceFaultPoint,
}

impl RestartCheckpointBaselineSourceFaultAlreadyArmed {
    /// Returns the fault that remains armed.
    #[must_use]
    pub const fn armed(&self) -> RestartCheckpointBaselineSourceFaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> RestartCheckpointBaselineSourceFaultPoint {
        self.requested
    }
}

impl fmt::Display for RestartCheckpointBaselineSourceFaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "checkpoint-source fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for RestartCheckpointBaselineSourceFaultAlreadyArmed {}

/// Failure to load one complete owned checkpoint observation from memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryTransactionRestartCheckpointBaselineSourceError {
    /// The configured deterministic one-shot failure was reached.
    InjectedFault(RestartCheckpointBaselineSourceFaultPoint),
    /// The returned entry vector could not reserve its exact required bound.
    TransactionCapacityExhausted {
        /// Exact number of entries that required reservation.
        transaction_count: usize,
    },
}

impl fmt::Display for InMemoryTransactionRestartCheckpointBaselineSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InjectedFault(point) => {
                write!(formatter, "injected checkpoint-source failure {point}")
            }
            Self::TransactionCapacityExhausted { transaction_count } => write!(
                formatter,
                "in-memory checkpoint observation capacity is exhausted for {transaction_count} transaction entries"
            ),
        }
    }
}

impl Error for InMemoryTransactionRestartCheckpointBaselineSourceError {}

/// One-shot failure boundary for the next checkpoint baseline publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointBaselinePublicationFaultPoint {
    /// Fail before replacing the current in-memory slot.
    BeforeReplace,
    /// Replace the current in-memory slot, then report publication failure.
    AfterReplace,
}

impl fmt::Display for RestartCheckpointBaselinePublicationFaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeReplace => formatter.write_str("before replace"),
            Self::AfterReplace => formatter.write_str("after replace"),
        }
    }
}

/// Refusal to replace an already armed checkpoint-publication fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartCheckpointBaselinePublicationFaultAlreadyArmed {
    armed: RestartCheckpointBaselinePublicationFaultPoint,
    requested: RestartCheckpointBaselinePublicationFaultPoint,
}

impl RestartCheckpointBaselinePublicationFaultAlreadyArmed {
    /// Returns the fault that remains armed.
    #[must_use]
    pub const fn armed(&self) -> RestartCheckpointBaselinePublicationFaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> RestartCheckpointBaselinePublicationFaultPoint {
        self.requested
    }
}

impl fmt::Display for RestartCheckpointBaselinePublicationFaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "checkpoint-publication fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for RestartCheckpointBaselinePublicationFaultAlreadyArmed {}

/// Outcome-indeterminate failure to publish one checkpoint baseline in memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryTransactionRestartCheckpointBaselinePublicationError {
    /// The private owner permit did not identify the supplied baseline exactly.
    PublicationPermitMismatch {
        /// Persistent log ID carried by the supplied baseline.
        baseline_persistent_log_id: u128,
        /// Persistent log ID carried by the owner permit.
        permit_persistent_log_id: u128,
        /// Optional frontier carried by the supplied baseline.
        baseline_durable_frontier: Option<u64>,
        /// Optional frontier carried by the owner permit.
        permit_durable_frontier: Option<u64>,
        /// Transaction count carried by the supplied baseline.
        baseline_transaction_count: usize,
        /// Transaction count carried by the owner permit.
        permit_transaction_count: usize,
    },
    /// The configured deterministic one-shot failure was reached.
    InjectedFault(RestartCheckpointBaselinePublicationFaultPoint),
    /// A complete replacement candidate could not reserve its exact entry bound.
    TransactionCapacityExhausted {
        /// Exact number of entries that required reservation.
        transaction_count: usize,
    },
}

impl fmt::Display for InMemoryTransactionRestartCheckpointBaselinePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicationPermitMismatch {
                baseline_persistent_log_id,
                permit_persistent_log_id,
                baseline_durable_frontier,
                permit_durable_frontier,
                baseline_transaction_count,
                permit_transaction_count,
            } => write!(
                formatter,
                "checkpoint publication permit mismatch: baseline id/frontier/count {baseline_persistent_log_id}/{baseline_durable_frontier:?}/{baseline_transaction_count}, permit {permit_persistent_log_id}/{permit_durable_frontier:?}/{permit_transaction_count}"
            ),
            Self::InjectedFault(point) => {
                write!(formatter, "injected checkpoint-publication failure {point}")
            }
            Self::TransactionCapacityExhausted { transaction_count } => write!(
                formatter,
                "in-memory checkpoint publication capacity is exhausted for {transaction_count} transaction entries"
            ),
        }
    }
}

impl Error for InMemoryTransactionRestartCheckpointBaselinePublicationError {}

/// Deterministic read and publication adapter for one in-memory checkpoint slot.
///
/// The constructor seed is untrusted fixture state, not publication evidence.
/// Runtime replacement is available only through the separately permitted
/// checkpoint publisher port. The slot remains distinct from the memory WAL and
/// page store.
///
/// It cannot satisfy WAL durability:
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointBaselineSource;
/// use ntsql_wal::LogDurability;
///
/// fn require_log<Log: LogDurability>(_log: &mut Log) {}
///
/// let mut source = InMemoryTransactionRestartCheckpointBaselineSource::empty();
/// require_log(&mut source);
/// ```
///
/// It cannot satisfy page-store write authority:
///
/// ```compile_fail
/// use ntsql_page::PageStore;
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointBaselineSource;
///
/// fn require_store<Store: PageStore<1>>(_store: &mut Store) {}
///
/// let mut source = InMemoryTransactionRestartCheckpointBaselineSource::empty();
/// require_store(&mut source);
/// ```
///
/// It cannot substitute for the authoritative WAL restart-analysis source:
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointBaselineSource;
/// use ntsql_transaction::DurableTransactionRestartAnalysisSource;
///
/// fn require_wal_source<Source: DurableTransactionRestartAnalysisSource<1>>(
///     _source: &mut Source,
/// ) {}
///
/// let mut source = InMemoryTransactionRestartCheckpointBaselineSource::empty();
/// require_wal_source(&mut source);
/// ```
///
/// Its untrusted slot cannot become an authoritative baseline:
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointBaselineSource;
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaseline;
///
/// fn cannot_authorize(
///     source: InMemoryTransactionRestartCheckpointBaselineSource,
/// ) -> DurableTransactionRestartCheckpointBaseline {
///     source.into()
/// }
/// ```
#[derive(Debug)]
#[must_use]
pub struct InMemoryTransactionRestartCheckpointBaselineSource {
    slot: Option<OwnedDurableTransactionRestartCheckpointBaselineObservation>,
    armed_fault: Option<RestartCheckpointBaselineSourceFaultPoint>,
    armed_publication_fault: Option<RestartCheckpointBaselinePublicationFaultPoint>,
}

impl InMemoryTransactionRestartCheckpointBaselineSource {
    /// Creates a source with no current checkpoint slot.
    pub const fn empty() -> Self {
        Self {
            slot: None,
            armed_fault: None,
            armed_publication_fault: None,
        }
    }

    /// Creates a source with one untrusted fixture snapshot.
    pub const fn seeded(slot: OwnedDurableTransactionRestartCheckpointBaselineObservation) -> Self {
        Self {
            slot: Some(slot),
            armed_fault: None,
            armed_publication_fault: None,
        }
    }

    /// Returns the exact current untrusted slot.
    #[must_use]
    pub const fn slot(
        &self,
    ) -> Option<&OwnedDurableTransactionRestartCheckpointBaselineObservation> {
        self.slot.as_ref()
    }

    /// Arms one load fault without replacing an existing plan.
    pub fn arm_fault(
        &mut self,
        fault: RestartCheckpointBaselineSourceFaultPoint,
    ) -> Result<(), RestartCheckpointBaselineSourceFaultAlreadyArmed> {
        if let Some(armed) = self.armed_fault {
            return Err(RestartCheckpointBaselineSourceFaultAlreadyArmed {
                armed,
                requested: fault,
            });
        }
        self.armed_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot fault that has not yet reached its matching stage.
    #[must_use]
    pub const fn armed_fault(&self) -> Option<RestartCheckpointBaselineSourceFaultPoint> {
        self.armed_fault
    }

    /// Arms one publication fault without replacing an existing plan.
    pub fn arm_publication_fault(
        &mut self,
        fault: RestartCheckpointBaselinePublicationFaultPoint,
    ) -> Result<(), RestartCheckpointBaselinePublicationFaultAlreadyArmed> {
        if let Some(armed) = self.armed_publication_fault {
            return Err(RestartCheckpointBaselinePublicationFaultAlreadyArmed {
                armed,
                requested: fault,
            });
        }
        self.armed_publication_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot publication fault that has not reached its stage.
    #[must_use]
    pub const fn armed_publication_fault(
        &self,
    ) -> Option<RestartCheckpointBaselinePublicationFaultPoint> {
        self.armed_publication_fault
    }

    fn consume_fault(&mut self, point: RestartCheckpointBaselineSourceFaultPoint) -> bool {
        if self.armed_fault == Some(point) {
            self.armed_fault = None;
            true
        } else {
            false
        }
    }

    fn consume_publication_fault(
        &mut self,
        point: RestartCheckpointBaselinePublicationFaultPoint,
    ) -> bool {
        if self.armed_publication_fault == Some(point) {
            self.armed_publication_fault = None;
            true
        } else {
            false
        }
    }

    fn lower_restart_checkpoint_baseline(
        baseline: &DurableTransactionRestartCheckpointBaseline,
    ) -> Result<
        OwnedDurableTransactionRestartCheckpointBaselineObservation,
        InMemoryTransactionRestartCheckpointBaselinePublicationError,
    > {
        let transaction_count = baseline.transactions().len();
        let mut transactions = Vec::new();
        transactions.try_reserve_exact(transaction_count).map_err(|_| {
            InMemoryTransactionRestartCheckpointBaselinePublicationError::TransactionCapacityExhausted {
                transaction_count,
            }
        })?;
        transactions.extend(baseline.transactions().iter().map(|entry| {
            let transaction = entry.transaction();
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
                transaction.epoch(),
                transaction.sequence(),
                entry.first_owned_page_position(),
                entry.last_owned_page_position(),
                entry.owned_page_record_count(),
                state,
            )
        }));
        Ok(
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
                baseline.persistent_log_id().get(),
                baseline.durable_frontier(),
                transactions,
            ),
        )
    }
}

impl Default for InMemoryTransactionRestartCheckpointBaselineSource {
    fn default() -> Self {
        Self::empty()
    }
}

impl DurableTransactionRestartCheckpointBaselineSource
    for InMemoryTransactionRestartCheckpointBaselineSource
{
    type Error = InMemoryTransactionRestartCheckpointBaselineSourceError;

    fn load_restart_checkpoint_baseline(
        &mut self,
    ) -> Result<Option<OwnedDurableTransactionRestartCheckpointBaselineObservation>, Self::Error>
    {
        if self.consume_fault(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad) {
            return Err(
                InMemoryTransactionRestartCheckpointBaselineSourceError::InjectedFault(
                    RestartCheckpointBaselineSourceFaultPoint::BeforeLoad,
                ),
            );
        }

        let Some(slot) = self.slot.as_ref() else {
            return Ok(None);
        };
        let transaction_count = slot.transactions().len();
        let mut transactions = Vec::new();
        transactions.try_reserve_exact(transaction_count).map_err(|_| {
            InMemoryTransactionRestartCheckpointBaselineSourceError::TransactionCapacityExhausted {
                transaction_count,
            }
        })?;
        transactions.extend_from_slice(slot.transactions());
        Ok(Some(
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
                slot.persistent_log_id(),
                slot.durable_frontier(),
                transactions,
            ),
        ))
    }
}

impl DurableTransactionRestartCheckpointBaselinePublisher
    for InMemoryTransactionRestartCheckpointBaselineSource
{
    type Error = InMemoryTransactionRestartCheckpointBaselinePublicationError;

    fn publish_restart_checkpoint_baseline(
        &mut self,
        baseline: &DurableTransactionRestartCheckpointBaseline,
        permit: DurableTransactionRestartCheckpointBaselinePublicationPermit<'_>,
    ) -> Result<(), Self::Error> {
        let baseline_persistent_log_id = baseline.persistent_log_id().get();
        let permit_persistent_log_id = permit.persistent_log_id().get();
        let baseline_durable_frontier = baseline.durable_frontier();
        let permit_durable_frontier = permit.durable_frontier();
        let baseline_transaction_count = baseline.transactions().len();
        let permit_transaction_count = permit.transaction_count();
        if baseline_persistent_log_id != permit_persistent_log_id
            || baseline_durable_frontier != permit_durable_frontier
            || baseline_transaction_count != permit_transaction_count
        {
            return Err(
                InMemoryTransactionRestartCheckpointBaselinePublicationError::PublicationPermitMismatch {
                    baseline_persistent_log_id,
                    permit_persistent_log_id,
                    baseline_durable_frontier,
                    permit_durable_frontier,
                    baseline_transaction_count,
                    permit_transaction_count,
                },
            );
        }

        if self.consume_publication_fault(
            RestartCheckpointBaselinePublicationFaultPoint::BeforeReplace,
        ) {
            return Err(
                InMemoryTransactionRestartCheckpointBaselinePublicationError::InjectedFault(
                    RestartCheckpointBaselinePublicationFaultPoint::BeforeReplace,
                ),
            );
        }

        let replacement = Self::lower_restart_checkpoint_baseline(baseline)?;
        self.slot = Some(replacement);

        if self
            .consume_publication_fault(RestartCheckpointBaselinePublicationFaultPoint::AfterReplace)
        {
            Err(
                InMemoryTransactionRestartCheckpointBaselinePublicationError::InjectedFault(
                    RestartCheckpointBaselinePublicationFaultPoint::AfterReplace,
                ),
            )
        } else {
            Ok(())
        }
    }
}

/// One-shot failure boundary for the next completeness checkpoint load.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointCompletenessBaselineSourceFaultPoint {
    /// Fail before inspecting or copying the optional seeded completeness slot.
    BeforeLoad,
}

impl fmt::Display for RestartCheckpointCompletenessBaselineSourceFaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeLoad => formatter.write_str("before load"),
        }
    }
}

/// Refusal to replace an already armed completeness checkpoint-source fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartCheckpointCompletenessBaselineSourceFaultAlreadyArmed {
    armed: RestartCheckpointCompletenessBaselineSourceFaultPoint,
    requested: RestartCheckpointCompletenessBaselineSourceFaultPoint,
}

impl RestartCheckpointCompletenessBaselineSourceFaultAlreadyArmed {
    /// Returns the fault that remains armed.
    #[must_use]
    pub const fn armed(&self) -> RestartCheckpointCompletenessBaselineSourceFaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> RestartCheckpointCompletenessBaselineSourceFaultPoint {
        self.requested
    }
}

impl fmt::Display for RestartCheckpointCompletenessBaselineSourceFaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "completeness checkpoint-source fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for RestartCheckpointCompletenessBaselineSourceFaultAlreadyArmed {}

/// Failure to load one complete owned completeness observation from memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError {
    /// The configured deterministic one-shot failure was reached.
    InjectedFault(RestartCheckpointCompletenessBaselineSourceFaultPoint),
    /// The returned nested entry vector could not reserve its exact bound.
    TransactionCapacityExhausted {
        /// Exact number of nested transaction entries that required reservation.
        transaction_count: usize,
    },
    /// The returned page vector could not reserve its exact required bound.
    PageCapacityExhausted {
        /// Exact number of page entries that required reservation.
        page_count: usize,
    },
}

impl fmt::Display for InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InjectedFault(point) => write!(
                formatter,
                "injected completeness checkpoint-source failure {point}"
            ),
            Self::TransactionCapacityExhausted { transaction_count } => write!(
                formatter,
                "in-memory completeness checkpoint observation capacity is exhausted for {transaction_count} transaction entries"
            ),
            Self::PageCapacityExhausted { page_count } => write!(
                formatter,
                "in-memory completeness checkpoint observation capacity is exhausted for {page_count} page entries"
            ),
        }
    }
}

impl Error for InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError {}

/// One-shot failure boundary for the next completeness baseline publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointCompletenessBaselinePublicationFaultPoint {
    /// Fail before replacing the current in-memory completeness slot.
    BeforeReplace,
    /// Replace the current in-memory completeness slot, then report failure.
    AfterReplace,
}

impl fmt::Display for RestartCheckpointCompletenessBaselinePublicationFaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeReplace => formatter.write_str("before replace"),
            Self::AfterReplace => formatter.write_str("after replace"),
        }
    }
}

/// Refusal to replace an already armed completeness-publication fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {
    armed: RestartCheckpointCompletenessBaselinePublicationFaultPoint,
    requested: RestartCheckpointCompletenessBaselinePublicationFaultPoint,
}

impl RestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {
    /// Returns the fault that remains armed.
    #[must_use]
    pub const fn armed(&self) -> RestartCheckpointCompletenessBaselinePublicationFaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> RestartCheckpointCompletenessBaselinePublicationFaultPoint {
        self.requested
    }
}

impl fmt::Display for RestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "completeness checkpoint-publication fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for RestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {}

/// Outcome-indeterminate failure to publish one completeness baseline in memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError {
    /// The private owner permit did not identify the supplied baseline exactly.
    PublicationPermitMismatch {
        /// Persistent log ID carried by the supplied baseline.
        baseline_persistent_log_id: u128,
        /// Persistent log ID carried by the owner permit.
        permit_persistent_log_id: u128,
        /// Optional frontier carried by the supplied baseline.
        baseline_durable_frontier: Option<u64>,
        /// Optional frontier carried by the owner permit.
        permit_durable_frontier: Option<u64>,
        /// Transaction count carried by the supplied baseline.
        baseline_transaction_count: usize,
        /// Transaction count carried by the owner permit.
        permit_transaction_count: usize,
        /// Page count carried by the supplied baseline.
        baseline_page_count: usize,
        /// Page count carried by the owner permit.
        permit_page_count: usize,
    },
    /// The configured deterministic one-shot failure was reached.
    InjectedFault(RestartCheckpointCompletenessBaselinePublicationFaultPoint),
    /// A complete candidate could not reserve its exact nested entry bound.
    TransactionCapacityExhausted {
        /// Exact number of nested transaction entries that required reservation.
        transaction_count: usize,
    },
    /// A complete candidate could not reserve its exact page bound.
    PageCapacityExhausted {
        /// Exact number of page entries that required reservation.
        page_count: usize,
    },
}

impl fmt::Display for InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicationPermitMismatch {
                baseline_persistent_log_id,
                permit_persistent_log_id,
                baseline_durable_frontier,
                permit_durable_frontier,
                baseline_transaction_count,
                permit_transaction_count,
                baseline_page_count,
                permit_page_count,
            } => write!(
                formatter,
                "completeness checkpoint publication permit mismatch: baseline id/frontier/transactions/pages {baseline_persistent_log_id}/{baseline_durable_frontier:?}/{baseline_transaction_count}/{baseline_page_count}, permit {permit_persistent_log_id}/{permit_durable_frontier:?}/{permit_transaction_count}/{permit_page_count}"
            ),
            Self::InjectedFault(point) => write!(
                formatter,
                "injected completeness checkpoint-publication failure {point}"
            ),
            Self::TransactionCapacityExhausted { transaction_count } => write!(
                formatter,
                "in-memory completeness checkpoint publication capacity is exhausted for {transaction_count} transaction entries"
            ),
            Self::PageCapacityExhausted { page_count } => write!(
                formatter,
                "in-memory completeness checkpoint publication capacity is exhausted for {page_count} page entries"
            ),
        }
    }
}

impl Error for InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError {}

/// Deterministic read and publication adapter for one completeness slot.
///
/// This slot is distinct from
/// [`InMemoryTransactionRestartCheckpointBaselineSource`]: it retains the
/// decoded page table and replay lower bound the transaction-only slot cannot
/// represent, and it owns independent load and publication fault plans. The
/// constructor seed is untrusted fixture state, not publication evidence.
/// Runtime replacement is available only through the separately permitted
/// completeness publisher port.
///
/// It cannot satisfy WAL durability:
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointCompletenessBaselineSource;
/// use ntsql_wal::LogDurability;
///
/// fn require_log<Log: LogDurability>(_log: &mut Log) {}
///
/// let mut source = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
/// require_log(&mut source);
/// ```
///
/// It cannot satisfy page-store write authority:
///
/// ```compile_fail
/// use ntsql_page::PageStore;
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointCompletenessBaselineSource;
///
/// fn require_store<Store: PageStore<1>>(_store: &mut Store) {}
///
/// let mut source = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
/// require_store(&mut source);
/// ```
///
/// It cannot substitute for the authoritative WAL restart-analysis source:
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::DurableTransactionRestartAnalysisSource;
///
/// fn require_wal_source<Source: DurableTransactionRestartAnalysisSource<1>>(
///     _source: &mut Source,
/// ) {}
///
/// let mut source = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
/// require_wal_source(&mut source);
/// ```
///
/// It cannot substitute for the committed-page recovery store:
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::CommittedTransactionPageRecoveryStore;
///
/// fn require_recovery_store<Store: CommittedTransactionPageRecoveryStore<1>>(
///     _store: &mut Store,
/// ) {}
///
/// let mut source = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
/// require_recovery_store(&mut source);
/// ```
///
/// It cannot substitute for the transaction-only checkpoint slot's ports:
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaselineSource;
///
/// fn require_transaction_source<Source: DurableTransactionRestartCheckpointBaselineSource>(
///     _source: &mut Source,
/// ) {}
///
/// let mut source = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
/// require_transaction_source(&mut source);
/// ```
///
/// Its untrusted slot cannot become an authoritative completeness baseline:
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryTransactionRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::DurableTransactionRestartCheckpointCompletenessBaseline;
///
/// fn cannot_authorize(
///     source: InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
/// ) -> DurableTransactionRestartCheckpointCompletenessBaseline {
///     source.into()
/// }
/// ```
#[derive(Debug)]
#[must_use]
pub struct InMemoryTransactionRestartCheckpointCompletenessBaselineSource {
    slot: Option<OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation>,
    armed_fault: Option<RestartCheckpointCompletenessBaselineSourceFaultPoint>,
    armed_publication_fault: Option<RestartCheckpointCompletenessBaselinePublicationFaultPoint>,
}

impl InMemoryTransactionRestartCheckpointCompletenessBaselineSource {
    /// Creates a source with no current completeness checkpoint slot.
    pub const fn empty() -> Self {
        Self {
            slot: None,
            armed_fault: None,
            armed_publication_fault: None,
        }
    }

    /// Creates a source with one untrusted fixture completeness snapshot.
    pub const fn seeded(
        slot: OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
    ) -> Self {
        Self {
            slot: Some(slot),
            armed_fault: None,
            armed_publication_fault: None,
        }
    }

    /// Returns the exact current untrusted completeness slot.
    #[must_use]
    pub const fn slot(
        &self,
    ) -> Option<&OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation> {
        self.slot.as_ref()
    }

    /// Arms one completeness load fault without replacing an existing plan.
    pub fn arm_fault(
        &mut self,
        fault: RestartCheckpointCompletenessBaselineSourceFaultPoint,
    ) -> Result<(), RestartCheckpointCompletenessBaselineSourceFaultAlreadyArmed> {
        if let Some(armed) = self.armed_fault {
            return Err(
                RestartCheckpointCompletenessBaselineSourceFaultAlreadyArmed {
                    armed,
                    requested: fault,
                },
            );
        }
        self.armed_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot load fault that has not yet reached its stage.
    #[must_use]
    pub const fn armed_fault(
        &self,
    ) -> Option<RestartCheckpointCompletenessBaselineSourceFaultPoint> {
        self.armed_fault
    }

    /// Arms one completeness publication fault without replacing a plan.
    pub fn arm_publication_fault(
        &mut self,
        fault: RestartCheckpointCompletenessBaselinePublicationFaultPoint,
    ) -> Result<(), RestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed> {
        if let Some(armed) = self.armed_publication_fault {
            return Err(
                RestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {
                    armed,
                    requested: fault,
                },
            );
        }
        self.armed_publication_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot publication fault that has not reached its stage.
    #[must_use]
    pub const fn armed_publication_fault(
        &self,
    ) -> Option<RestartCheckpointCompletenessBaselinePublicationFaultPoint> {
        self.armed_publication_fault
    }

    fn consume_fault(
        &mut self,
        point: RestartCheckpointCompletenessBaselineSourceFaultPoint,
    ) -> bool {
        if self.armed_fault == Some(point) {
            self.armed_fault = None;
            true
        } else {
            false
        }
    }

    fn consume_publication_fault(
        &mut self,
        point: RestartCheckpointCompletenessBaselinePublicationFaultPoint,
    ) -> bool {
        if self.armed_publication_fault == Some(point) {
            self.armed_publication_fault = None;
            true
        } else {
            false
        }
    }

    fn lower_completeness_required_image(
        required: &DurableTransactionRestartRequiredPageImage,
    ) -> DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation {
        match required {
            DurableTransactionRestartRequiredPageImage::Raw { page_position } => {
                DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::Raw {
                    page_position: *page_position,
                }
            }
            DurableTransactionRestartRequiredPageImage::CommittedTransaction {
                transaction,
                page_position,
                commit_position,
            } => {
                DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::CommittedTransaction {
                    epoch: transaction.epoch(),
                    sequence: transaction.sequence(),
                    page_position: *page_position,
                    commit_position: *commit_position,
                }
            }
        }
    }

    fn lower_completeness_page_entry(
        entry: &DurableTransactionRestartPageEntry,
    ) -> DurableTransactionRestartCheckpointCompletenessBaselinePageObservation {
        let state = match entry.state() {
            DurableTransactionRestartPageState::NoRequiredImage => {
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::NoRequiredImage
            }
            DurableTransactionRestartPageState::StoreMissing { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreMissing
            }
            DurableTransactionRestartPageState::StoreCurrent { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent
            }
            DurableTransactionRestartPageState::StoreBehind { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreBehind
            }
        };
        DurableTransactionRestartCheckpointCompletenessBaselinePageObservation::new(
            entry.page_number().get(),
            state,
            entry
                .state()
                .required_image()
                .map(Self::lower_completeness_required_image),
            entry.state().stored_position(),
        )
    }

    fn lower_completeness_replay(
        replay: &DurableTransactionRestartReplayStart,
    ) -> DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation {
        let kind = match replay {
            DurableTransactionRestartReplayStart::AfterFrontier { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AfterFrontier
            }
            DurableTransactionRestartReplayStart::AtPosition { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition
            }
        };
        let cause = replay.cause().map(|cause| match cause {
            DurableTransactionRestartReplayStartCause::StoreMissing { page_number } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreMissingPage {
                    page_number: page_number.get(),
                }
            }
            DurableTransactionRestartReplayStartCause::StoreBehind { page_number } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreBehindPage {
                    page_number: page_number.get(),
                }
            }
            DurableTransactionRestartReplayStartCause::UncommittedTransaction { transaction } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::UncommittedTransaction {
                    epoch: transaction.epoch(),
                    sequence: transaction.sequence(),
                }
            }
        });
        DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation::new(
            kind,
            replay.frontier(),
            replay.position(),
            cause,
        )
    }

    fn lower_restart_checkpoint_completeness_baseline(
        baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
    ) -> Result<
        OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
        InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError,
    > {
        let transaction_count = baseline.transactions().len();
        let mut transactions = Vec::new();
        transactions.try_reserve_exact(transaction_count).map_err(|_| {
            InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::TransactionCapacityExhausted {
                transaction_count,
            }
        })?;
        transactions.extend(baseline.transactions().iter().map(|entry| {
            let transaction = entry.transaction();
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
                transaction.epoch(),
                transaction.sequence(),
                entry.first_owned_page_position(),
                entry.last_owned_page_position(),
                entry.owned_page_record_count(),
                state,
            )
        }));

        let page_count = baseline.pages().len();
        let mut pages = Vec::new();
        pages.try_reserve_exact(page_count).map_err(|_| {
            InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::PageCapacityExhausted {
                page_count,
            }
        })?;
        pages.extend(
            baseline
                .pages()
                .iter()
                .map(Self::lower_completeness_page_entry),
        );

        Ok(
            OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation::new(
                OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
                    baseline.persistent_log_id().get(),
                    baseline.durable_frontier(),
                    transactions,
                ),
                pages,
                Self::lower_completeness_replay(baseline.replay_start()),
            ),
        )
    }
}

impl Default for InMemoryTransactionRestartCheckpointCompletenessBaselineSource {
    fn default() -> Self {
        Self::empty()
    }
}

impl DurableTransactionRestartCheckpointCompletenessBaselineSource
    for InMemoryTransactionRestartCheckpointCompletenessBaselineSource
{
    type Error = InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError;

    fn load_restart_checkpoint_completeness_baseline(
        &mut self,
    ) -> Result<
        Option<OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation>,
        Self::Error,
    > {
        if self.consume_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad) {
            return Err(
                InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::InjectedFault(
                    RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad,
                ),
            );
        }

        let Some(slot) = self.slot.as_ref() else {
            return Ok(None);
        };
        let slot_transactions = slot.transactions();
        let transaction_count = slot_transactions.transactions().len();
        let mut transactions = Vec::new();
        transactions.try_reserve_exact(transaction_count).map_err(|_| {
            InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::TransactionCapacityExhausted {
                transaction_count,
            }
        })?;
        transactions.extend_from_slice(slot_transactions.transactions());

        let page_count = slot.pages().len();
        let mut pages = Vec::new();
        pages.try_reserve_exact(page_count).map_err(|_| {
            InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::PageCapacityExhausted {
                page_count,
            }
        })?;
        pages.extend_from_slice(slot.pages());

        Ok(Some(
            OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation::new(
                OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
                    slot_transactions.persistent_log_id(),
                    slot_transactions.durable_frontier(),
                    transactions,
                ),
                pages,
                slot.replay(),
            ),
        ))
    }
}

impl DurableTransactionRestartCheckpointCompletenessBaselinePublisher
    for InMemoryTransactionRestartCheckpointCompletenessBaselineSource
{
    type Error = InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError;

    fn publish_restart_checkpoint_completeness_baseline(
        &mut self,
        baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
        permit: DurableTransactionRestartCheckpointCompletenessBaselinePublicationPermit<'_>,
    ) -> Result<(), Self::Error> {
        let baseline_persistent_log_id = baseline.persistent_log_id().get();
        let permit_persistent_log_id = permit.persistent_log_id().get();
        let baseline_durable_frontier = baseline.durable_frontier();
        let permit_durable_frontier = permit.durable_frontier();
        let baseline_transaction_count = baseline.transactions().len();
        let permit_transaction_count = permit.transaction_count();
        let baseline_page_count = baseline.pages().len();
        let permit_page_count = permit.page_count();
        if baseline_persistent_log_id != permit_persistent_log_id
            || baseline_durable_frontier != permit_durable_frontier
            || baseline_transaction_count != permit_transaction_count
            || baseline_page_count != permit_page_count
        {
            return Err(
                InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::PublicationPermitMismatch {
                    baseline_persistent_log_id,
                    permit_persistent_log_id,
                    baseline_durable_frontier,
                    permit_durable_frontier,
                    baseline_transaction_count,
                    permit_transaction_count,
                    baseline_page_count,
                    permit_page_count,
                },
            );
        }

        if self.consume_publication_fault(
            RestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeReplace,
        ) {
            return Err(
                InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::InjectedFault(
                    RestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeReplace,
                ),
            );
        }

        let replacement = Self::lower_restart_checkpoint_completeness_baseline(baseline)?;
        self.slot = Some(replacement);

        if self.consume_publication_fault(
            RestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterReplace,
        ) {
            Err(
                InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::InjectedFault(
                    RestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterReplace,
                ),
            )
        } else {
            Ok(())
        }
    }
}

/// Inspectable in-memory implementation of the transaction and page WAL ports.
///
/// This adapter models only repository-authored physical effects. Its durable
/// prefix is not an operating-system flush guarantee or a SQL Server recovery
/// outcome.
#[derive(Debug)]
pub struct InMemoryCommitLog<const N: usize = 1> {
    lineage: LogLineage,
    records: Vec<InMemoryLogRecord<N>>,
    durable_len: usize,
    next_epoch: Option<NonZeroU64>,
    next_position: Option<u64>,
    armed_fault: Option<FaultPoint>,
}

impl<const N: usize> InMemoryCommitLog<N> {
    /// Creates an empty log whose first assigned position is one.
    #[must_use]
    pub fn with_ephemeral_lineage() -> Self {
        Self::with_lineage(LogLineage::new())
    }

    /// Creates an empty log with one adapter-supplied persistent identity.
    #[must_use]
    pub fn with_persistent_lineage_id(id: PersistentLogId) -> Self {
        Self::with_lineage(LogLineage::persistent(id))
    }

    fn with_lineage(lineage: LogLineage) -> Self {
        Self {
            lineage,
            records: Vec::new(),
            durable_len: 0,
            next_epoch: Some(NonZeroU64::MIN),
            next_position: Some(1),
            armed_fault: None,
        }
    }

    /// Arms one fault without replacing an existing plan.
    pub fn arm_fault(&mut self, fault: FaultPoint) -> Result<(), FaultAlreadyArmed> {
        if let Some(armed) = self.armed_fault {
            return Err(FaultAlreadyArmed {
                armed,
                requested: fault,
            });
        }
        self.armed_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot fault that has not yet reached its matching stage.
    #[must_use]
    pub const fn armed_fault(&self) -> Option<FaultPoint> {
        self.armed_fault
    }

    /// Returns every physically appended snapshot, including the volatile suffix.
    #[must_use]
    pub fn records(&self) -> &[InMemoryLogRecord<N>] {
        &self.records
    }

    /// Iterates over exactly the prefix that this model marked durable.
    pub fn durable_records(
        &self,
    ) -> impl DoubleEndedIterator<Item = &InMemoryLogRecord<N>> + ExactSizeIterator {
        self.records.iter().take(self.durable_len)
    }

    /// Returns the tail of the durable prefix.
    #[must_use]
    pub fn durable_position(&self) -> Option<LogSequenceNumber> {
        self.durable_len
            .checked_sub(1)
            .and_then(|index| self.records.get(index))
            .map(InMemoryLogRecord::position)
            .cloned()
    }

    /// Simulates loss of volatile state while preserving allocator identity.
    ///
    /// The high-water position is intentionally retained so an old copied
    /// position for a discarded record cannot alias a future record. The armed
    /// transient fault is cleared.
    #[must_use]
    pub fn restart(mut self) -> Self {
        self.records.truncate(self.durable_len);
        self.armed_fault = None;
        self
    }

    /// Simulates reopening durable state after runtime lineage identity is lost.
    ///
    /// The persistent ID is validated before any volatile state or fault is
    /// discarded. Durable positions are reconstructed from the reopened
    /// capability while allocator high-water marks remain unchanged.
    pub fn reopen(&mut self) -> Result<(), InMemoryLogReopenError> {
        let id = self
            .lineage
            .persistent_id()
            .ok_or(InMemoryLogReopenError::EphemeralLineage)?;
        let lineage = LogLineage::persistent(id);

        self.records.truncate(self.durable_len);
        for record in &mut self.records {
            let value = record.position.get();
            record.position = lineage.position(value);
        }
        self.lineage = lineage;
        self.armed_fault = None;
        Ok(())
    }

    fn append_record(
        &mut self,
        kind: InMemoryLogRecordKind<N>,
    ) -> Result<LogSequenceNumber, InMemoryCommitLogError> {
        let position_value = self
            .next_position
            .ok_or(InMemoryCommitLogError::PositionSpaceExhausted)?;
        if self.consume_fault(FaultPoint::BeforeAppend) {
            return Err(InMemoryCommitLogError::InjectedFault(
                FaultPoint::BeforeAppend,
            ));
        }
        self.records
            .try_reserve(1)
            .map_err(|_| InMemoryCommitLogError::RecordCapacityExhausted)?;

        let position = self.lineage.position(position_value);
        self.records.push(InMemoryLogRecord {
            position: position.clone(),
            kind,
        });
        self.next_position = position_value.checked_add(1);

        if self.consume_fault(FaultPoint::AfterAppend) {
            Err(InMemoryCommitLogError::InjectedFault(
                FaultPoint::AfterAppend,
            ))
        } else {
            Ok(position)
        }
    }

    fn consume_fault(&mut self, point: FaultPoint) -> bool {
        if self.armed_fault == Some(point) {
            self.armed_fault = None;
            true
        } else {
            false
        }
    }
}

impl<const N: usize> Default for InMemoryCommitLog<N> {
    fn default() -> Self {
        Self::with_ephemeral_lineage()
    }
}

impl InMemoryCommitLog<1> {
    /// Creates an empty width-1 log whose first assigned position is one.
    #[must_use]
    pub fn new() -> Self {
        Self::with_ephemeral_lineage()
    }

    /// Creates an empty width-1 log with one adapter-supplied persistent identity.
    #[must_use]
    pub fn with_persistent_lineage(id: PersistentLogId) -> Self {
        Self::with_persistent_lineage_id(id)
    }
}

impl<const N: usize> TransactionEpochSource for InMemoryCommitLog<N> {
    type Error = InMemoryTransactionEpochError;

    fn allocate_transaction_epoch(&mut self) -> Result<(NonZeroU64, LogLineage), Self::Error> {
        let epoch = self
            .next_epoch
            .ok_or(InMemoryTransactionEpochError::EpochSpaceExhausted)?;
        self.next_epoch = epoch.get().checked_add(1).and_then(NonZeroU64::new);
        Ok((epoch, self.lineage.clone()))
    }
}

impl<const N: usize> TransactionRecoverySource for InMemoryCommitLog<N> {
    type Error = InMemoryTransactionRecoveryError;

    fn lookup_durable_commit(
        &mut self,
        transaction_id: TransactionId,
    ) -> Result<(LogLineage, DurableCommitLookup), Self::Error> {
        let mut matching_record = None;

        for (index, record) in self.records.iter().enumerate() {
            if record.transaction_id() != Some(transaction_id) {
                continue;
            }
            if matching_record.is_some() {
                return Err(InMemoryTransactionRecoveryError::DuplicateCommitRecord(
                    transaction_id,
                ));
            }
            matching_record = Some((record.position().clone(), index < self.durable_len));
        }

        let lookup = match matching_record {
            Some((position, true)) => DurableCommitLookup::Found { position },
            None => DurableCommitLookup::Absent,
            Some((_, false)) => {
                return Err(InMemoryTransactionRecoveryError::VolatileCommitRecord(
                    transaction_id,
                ));
            }
        };
        Ok((self.lineage.clone(), lookup))
    }
}

impl<const N: usize> DurableTransactionPageRecoveryInventory<N> for InMemoryCommitLog<N> {
    type Error = InMemoryCommittedPageRecoveryInventoryError;

    fn durable_transaction_page_numbers(&mut self) -> Result<Vec<PageNumber>, Self::Error> {
        let mut page_numbers = Vec::new();
        page_numbers
            .try_reserve(self.durable_len)
            .map_err(|_| InMemoryCommittedPageRecoveryInventoryError::PageCapacityExhausted)?;

        for record in self.durable_records() {
            if let Some(page) = record.transaction_page_write() {
                page_numbers.push(page.page_write().page_number());
            }
        }
        page_numbers.sort_unstable();
        page_numbers.dedup();
        Ok(page_numbers)
    }
}

impl<const N: usize> DurableTransactionPageRecoverySource<N> for InMemoryCommitLog<N> {
    type Error = InMemoryCommittedPageRecoverySourceError<N>;

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
            &'evidence [DurablePageWalObservation<N>],
            &'evidence [DurableTransactionPageObservation<N>],
            &'evidence [DurableTransactionCommitObservation],
        ) -> Output,
    {
        let durable_len = self.durable_len;
        let mut physical_pages = Vec::new();
        physical_pages.try_reserve(durable_len).map_err(|_| {
            InMemoryCommittedPageRecoverySourceError::EvidenceCapacityExhausted {
                projection: InMemoryPageRecoveryProjection::PhysicalPages,
            }
        })?;
        let mut transaction_pages = Vec::new();
        transaction_pages.try_reserve(durable_len).map_err(|_| {
            InMemoryCommittedPageRecoverySourceError::EvidenceCapacityExhausted {
                projection: InMemoryPageRecoveryProjection::TransactionPages,
            }
        })?;
        let mut commits = Vec::new();
        commits.try_reserve(durable_len).map_err(|_| {
            InMemoryCommittedPageRecoverySourceError::EvidenceCapacityExhausted {
                projection: InMemoryPageRecoveryProjection::Commits,
            }
        })?;

        for record in self.durable_records() {
            if record
                .page_write()
                .is_some_and(|page| page.page_number() == page_number)
                && let Some(observation) = record.page_recovery_observation().map_err(|source| {
                    InMemoryCommittedPageRecoverySourceError::PhysicalPageProjection(Box::new(
                        source,
                    ))
                })?
            {
                physical_pages.push(observation);
            }
            if record
                .transaction_page_write()
                .is_some_and(|page| page.page_write().page_number() == page_number)
                && let Some(observation) =
                    record
                        .transaction_page_recovery_observation()
                        .map_err(|source| {
                            InMemoryCommittedPageRecoverySourceError::TransactionPageProjection(
                                Box::new(source),
                            )
                        })?
            {
                transaction_pages.push(observation);
            }
            if let Some(observation) =
                record
                    .transaction_commit_recovery_observation()
                    .map_err(|source| {
                        InMemoryCommittedPageRecoverySourceError::CommitProjection(Box::new(source))
                    })?
            {
                commits.push(observation);
            }
        }

        Ok(operation(&physical_pages, &transaction_pages, &commits))
    }
}

impl<const N: usize> DurableTransactionRestartAnalysisSource<N> for InMemoryCommitLog<N> {
    type Error = InMemoryTransactionRestartAnalysisSourceError<N>;

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
            &'evidence [DurableTransactionRestartObservation<N>],
        ) -> Output,
    {
        let durable_len = self.durable_len;
        let durable_frontier = self.durable_position();
        let mut observations = Vec::new();
        observations.try_reserve(durable_len).map_err(|_| {
            InMemoryTransactionRestartAnalysisSourceError::ObservationCapacityExhausted {
                record_count: durable_len,
            }
        })?;

        for record in self.durable_records() {
            let observation = match record.kind() {
                InMemoryLogRecordKind::TransactionCommit { transaction_id } => record
                    .project_transaction_commit_recovery_observation(*transaction_id)
                    .map(DurableTransactionRestartObservation::Commit)
                    .map_err(|source| {
                        InMemoryTransactionRestartAnalysisSourceError::CommitProjection(Box::new(
                            source,
                        ))
                    })?,
                InMemoryLogRecordKind::PageWrite(page) => record
                    .project_page_recovery_observation(page)
                    .map(DurableTransactionRestartObservation::Page)
                    .map_err(|source| {
                        InMemoryTransactionRestartAnalysisSourceError::PageProjection(Box::new(
                            source,
                        ))
                    })?,
                InMemoryLogRecordKind::TransactionPageWrite(transaction_page) => record
                    .project_transaction_page_recovery_observation(transaction_page)
                    .map(DurableTransactionRestartObservation::TransactionPage)
                    .map_err(|source| {
                        InMemoryTransactionRestartAnalysisSourceError::TransactionPageProjection(
                            Box::new(source),
                        )
                    })?,
            };
            observations.push(observation);
        }

        Ok(operation(durable_frontier.as_ref(), &observations))
    }
}

impl<const N: usize> LogDurability for InMemoryCommitLog<N> {
    type Error = InMemoryCommitLogError;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn flush_through(&mut self, position: &LogSequenceNumber) -> Result<(), Self::Error> {
        if !self.lineage.same_lineage(position.lineage()) {
            return Err(InMemoryCommitLogError::ForeignFlushPosition(
                position.clone(),
            ));
        }
        let record_index = self
            .records
            .iter()
            .position(|record| record.position() == position)
            .ok_or_else(|| InMemoryCommitLogError::UnknownFlushPosition(position.clone()))?;
        let requested_durable_len = record_index + 1;
        if requested_durable_len <= self.durable_len {
            return Ok(());
        }
        if self.consume_fault(FaultPoint::BeforeFlush) {
            return Err(InMemoryCommitLogError::InjectedFault(
                FaultPoint::BeforeFlush,
            ));
        }

        self.durable_len = requested_durable_len;

        if self.consume_fault(FaultPoint::AfterFlush) {
            Err(InMemoryCommitLogError::InjectedFault(
                FaultPoint::AfterFlush,
            ))
        } else {
            Ok(())
        }
    }
}

impl<const N: usize> CommitLog<TransactionCommitRecord> for InMemoryCommitLog<N> {
    fn append_commit(
        &mut self,
        record: &TransactionCommitRecord,
    ) -> Result<LogSequenceNumber, Self::Error> {
        self.append_record(InMemoryLogRecordKind::TransactionCommit {
            transaction_id: record.transaction_id(),
        })
    }
}

impl<const N: usize> PageLog<N> for InMemoryCommitLog<N> {
    fn append_page(&mut self, page: &UnloggedPage<N>) -> Result<LogSequenceNumber, Self::Error> {
        if !self.lineage.same_lineage(page.address().lineage()) {
            return Err(InMemoryCommitLogError::ForeignPageLineage(
                page.address().number(),
            ));
        }
        self.append_record(InMemoryLogRecordKind::PageWrite(
            InMemoryPageWriteRecord::from_unlogged(page),
        ))
    }
}

impl<const N: usize> TransactionPageLog<N> for InMemoryCommitLog<N> {
    fn append_transaction_page(
        &mut self,
        record: &TransactionPageWriteRecord<'_, N>,
    ) -> Result<LogSequenceNumber, Self::Error> {
        if !self.lineage.same_lineage(record.page().address().lineage()) {
            return Err(InMemoryCommitLogError::ForeignPageLineage(
                record.page().address().number(),
            ));
        }
        self.append_record(InMemoryLogRecordKind::TransactionPageWrite(
            InMemoryTransactionPageWriteRecord::from_record(record),
        ))
    }
}

/// Inspectable in-memory implementation of one page-store lineage.
#[derive(Debug)]
pub struct InMemoryPageStore<const N: usize = 1> {
    lineage: LogLineage,
    pages: Vec<InMemoryStoredPage<N>>,
    armed_fault: Option<PageStoreFaultPoint>,
}

impl<const N: usize> InMemoryPageStore<N> {
    /// Creates an empty page store bound to one in-memory WAL lineage.
    #[must_use]
    pub fn new(log: &InMemoryCommitLog<N>) -> Self {
        Self::with_lineage(log.lineage.clone())
    }

    /// Creates an empty page store with an explicit lineage.
    #[must_use]
    pub fn with_lineage(lineage: LogLineage) -> Self {
        Self {
            lineage,
            pages: Vec::new(),
            armed_fault: None,
        }
    }

    /// Arms one fault without replacing an existing plan.
    pub fn arm_fault(
        &mut self,
        fault: PageStoreFaultPoint,
    ) -> Result<(), PageStoreFaultAlreadyArmed> {
        if let Some(armed) = self.armed_fault {
            return Err(PageStoreFaultAlreadyArmed {
                armed,
                requested: fault,
            });
        }
        self.armed_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot fault that has not yet reached its matching stage.
    #[must_use]
    pub const fn armed_fault(&self) -> Option<PageStoreFaultPoint> {
        self.armed_fault
    }

    /// Returns every current durable page snapshot.
    #[must_use]
    pub fn pages(&self) -> &[InMemoryStoredPage<N>] {
        &self.pages
    }

    /// Returns the current durable snapshot for one page number.
    #[must_use]
    pub fn page(&self, number: PageNumber) -> Option<&InMemoryStoredPage<N>> {
        self.pages.iter().find(|page| page.page_number() == number)
    }

    fn page_index(&self, number: PageNumber) -> Option<usize> {
        self.pages
            .iter()
            .position(|page| page.page_number() == number)
    }

    fn consume_fault(&mut self, point: PageStoreFaultPoint) -> bool {
        if self.armed_fault == Some(point) {
            self.armed_fault = None;
            true
        } else {
            false
        }
    }
}

impl<const N: usize> PageStore<N> for InMemoryPageStore<N> {
    type Error = InMemoryPageStoreError;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn write_page(
        &mut self,
        page: &ntsql_page::DirtyPage<N>,
        permit: PageWritePermit<'_>,
    ) -> Result<(), Self::Error> {
        if !self.lineage.same_lineage(page.address().lineage()) {
            return Err(InMemoryPageStoreError::ForeignPageLineage(
                page.address().number(),
            ));
        }
        if !self
            .lineage
            .same_lineage(permit.durable_position().lineage())
        {
            return Err(InMemoryPageStoreError::ForeignPermitPosition(
                permit.durable_position().clone(),
            ));
        }
        if permit.durable_position() != page.required_position() {
            return Err(InMemoryPageStoreError::PermitPositionMismatch {
                expected: page.required_position().clone(),
                actual: permit.durable_position().clone(),
            });
        }
        if self.consume_fault(PageStoreFaultPoint::BeforeWrite) {
            return Err(InMemoryPageStoreError::InjectedFault(
                PageStoreFaultPoint::BeforeWrite,
            ));
        }

        let snapshot = InMemoryStoredPage::from_dirty(page);
        if let Some(index) = self.page_index(snapshot.page_number()) {
            self.pages[index] = snapshot;
        } else {
            self.pages
                .try_reserve(1)
                .map_err(|_| InMemoryPageStoreError::PageCapacityExhausted)?;
            self.pages.push(snapshot);
        }

        if self.consume_fault(PageStoreFaultPoint::AfterWrite) {
            Err(InMemoryPageStoreError::InjectedFault(
                PageStoreFaultPoint::AfterWrite,
            ))
        } else {
            Ok(())
        }
    }
}

fn require_in_memory_recovery_source_match<const N: usize>(
    candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
    current: Option<&StoredPageSnapshotObservation<N>>,
) -> Result<(), InMemoryCommittedPageRecoveryStoreError<N>> {
    match compare_committed_transaction_page_recovery_candidate(candidate, current) {
        Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches) => Ok(()),
        Ok(actual) => Err(InMemoryCommittedPageRecoveryStoreError::SourceNotMatched { actual }),
        Err(source) => Err(InMemoryCommittedPageRecoveryStoreError::SourceComparison(
            Box::new(source),
        )),
    }
}

impl<const N: usize> ntsql_transaction::DurablePageStoreSnapshotSource<N> for InMemoryPageStore<N> {
    type ObservationError = PageRecoveryObservationBytesError<N>;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn observe_page(
        &self,
        page_number: PageNumber,
    ) -> Result<Option<StoredPageSnapshotObservation<N>>, Self::ObservationError> {
        self.page(page_number)
            .map(InMemoryStoredPage::page_recovery_observation)
            .transpose()
    }
}

impl<const N: usize> CommittedTransactionPageRecoveryStore<N> for InMemoryPageStore<N> {
    type WriteError = InMemoryCommittedPageRecoveryStoreError<N>;

    fn compare_and_replace(
        &mut self,
        candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
        permit: CommittedTransactionPageRecoveryWritePermit<'_>,
    ) -> Result<(), Self::WriteError> {
        let latest = candidate.latest_committed();
        let target = latest.observation();
        if !self.lineage.same_lineage(target.position().lineage()) {
            return Err(
                InMemoryCommittedPageRecoveryStoreError::ForeignTargetPagePosition(
                    target.position().clone(),
                ),
            );
        }
        if !self
            .lineage
            .same_lineage(latest.commit_position().lineage())
        {
            return Err(
                InMemoryCommittedPageRecoveryStoreError::ForeignTargetCommitPosition(
                    latest.commit_position().clone(),
                ),
            );
        }
        if !self.lineage.same_lineage(permit.page_position().lineage()) {
            return Err(
                InMemoryCommittedPageRecoveryStoreError::ForeignPermitPagePosition(
                    permit.page_position().clone(),
                ),
            );
        }
        if !self
            .lineage
            .same_lineage(permit.commit_position().lineage())
        {
            return Err(
                InMemoryCommittedPageRecoveryStoreError::ForeignPermitCommitPosition(
                    permit.commit_position().clone(),
                ),
            );
        }
        if permit.page_position() != target.position() {
            return Err(
                InMemoryCommittedPageRecoveryStoreError::PermitPagePositionMismatch {
                    expected: target.position().clone(),
                    actual: permit.page_position().clone(),
                },
            );
        }
        if permit.commit_position() != latest.commit_position() {
            return Err(
                InMemoryCommittedPageRecoveryStoreError::PermitCommitPositionMismatch {
                    expected: latest.commit_position().clone(),
                    actual: permit.commit_position().clone(),
                },
            );
        }

        let page_number = target.page().page_number();
        let current = self
            .page(page_number)
            .map(InMemoryStoredPage::page_recovery_observation)
            .transpose()
            .map_err(|source| {
                InMemoryCommittedPageRecoveryStoreError::CurrentObservation(Box::new(source))
            })?;
        require_in_memory_recovery_source_match(candidate, current.as_ref())?;

        let page_index = self.page_index(page_number);
        if page_index.is_none() {
            self.pages
                .try_reserve(1)
                .map_err(|_| InMemoryCommittedPageRecoveryStoreError::PageCapacityExhausted)?;
        }
        if self.consume_fault(PageStoreFaultPoint::BeforeWrite) {
            return Err(InMemoryCommittedPageRecoveryStoreError::InjectedFault(
                PageStoreFaultPoint::BeforeWrite,
            ));
        }

        let snapshot = InMemoryStoredPage::from_recovery_candidate(candidate);
        match page_index {
            Some(index) => self.pages[index] = snapshot,
            None => self.pages.push(snapshot),
        }

        if self.consume_fault(PageStoreFaultPoint::AfterWrite) {
            Err(InMemoryCommittedPageRecoveryStoreError::InjectedFault(
                PageStoreFaultPoint::AfterWrite,
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use ntsql_page::{PageAddress, PageImage};
    use ntsql_transaction::{
        CommittedTransactionPageRecoveryError, CommittedTransactionPageRecoveryOutcome,
        CommittedTransactionPagesRecoveryError, CoordinatedCommitError,
        DurableTransactionIdentityObservation, DurableTransactionRestartCheckpointBaseline,
        DurableTransactionRestartCheckpointBaselineCurrentPublicationError,
        DurableTransactionRestartCheckpointBaselineEntryObservation,
        DurableTransactionRestartCheckpointBaselineSourceValidationError,
        DurableTransactionRestartCheckpointBaselineStateObservation,
        DurableTransactionRestartCheckpointCompletenessBaseline,
        DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError,
        DurableTransactionRestartCheckpointCompletenessBaselinePageObservation,
        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation,
        DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation,
        DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation,
        DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation,
        DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation,
        DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError,
        DurableTransactionRestartCheckpointCompletenessBaselineValidationError,
        DurableTransactionRestartCheckpointCompletenessBaselineValidationEvidenceError,
        DurableTransactionRestartCompletenessError,
        DurableTransactionRestartCompletenessEvidenceError, DurableTransactionRestartPageState,
        DurableTransactionRestartReplayStart, DurableTransactionRestartReplayStartCause,
        DurableTransactionRestartRequiredPageImage, DurableTransactionRestartState,
        OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
        RestartAnalyzedTransactionPageStorage, TransactionCoordinator, TransactionLifecycleStatus,
        TransactionPageStorageRestartCheckpointCompletenessSelection, TransactionResolutionFailure,
        UnrecoveredTransactionPageStorage, flush_committed_page,
        recover_committed_transaction_pages,
    };
    use ntsql_wal::CommitError;

    use super::*;

    fn checkpoint_publication_owner(
        persistent_log_id: PersistentLogId,
        first_page_number: u64,
    ) -> Result<
        RestartAnalyzedTransactionPageStorage<InMemoryCommitLog<1>, InMemoryPageStore<1>, 1>,
        Box<dyn Error>,
    > {
        let mut log = InMemoryCommitLog::with_persistent_lineage_id(persistent_log_id);
        let mut store = InMemoryPageStore::new(&log);
        let mut coordinator = TransactionCoordinator::open(&mut log)?;

        let committed_page_number = PageNumber::new(first_page_number)
            .ok_or_else(|| io::Error::other("committed page number is zero"))?;
        let committed = coordinator.begin()?;
        let committed_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), committed_page_number),
            PageVersion::new(1),
            PageImage::new([0xA1])?,
        );
        let (committed, committed_dirty) =
            coordinator.stage_page_write(committed, committed_page, &mut log)?;
        let committed = coordinator.commit(committed, &mut log)?;
        flush_committed_page(&committed, &mut log, &mut store, committed_dirty)?;

        let uncommitted_page_number = PageNumber::new(first_page_number + 1)
            .ok_or_else(|| io::Error::other("uncommitted page number is zero"))?;
        let uncommitted = coordinator.begin()?;
        let uncommitted_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), uncommitted_page_number),
            PageVersion::new(2),
            PageImage::new([0xA2])?,
        );
        let (uncommitted, uncommitted_dirty) =
            coordinator.stage_page_write(uncommitted, uncommitted_page, &mut log)?;
        log.flush_through(uncommitted_dirty.required_position())?;
        drop(uncommitted);
        drop(uncommitted_dirty);
        drop(coordinator);

        Ok(UnrecoveredTransactionPageStorage::new(log, store)
            .recover()?
            .analyze_restart()?)
    }

    fn assert_checkpoint_observation_matches_baseline(
        observation: &OwnedDurableTransactionRestartCheckpointBaselineObservation,
        baseline: &DurableTransactionRestartCheckpointBaseline,
    ) {
        assert_eq!(
            observation.persistent_log_id(),
            baseline.persistent_log_id().get()
        );
        assert_eq!(observation.durable_frontier(), baseline.durable_frontier());
        assert_eq!(
            observation.transactions().len(),
            baseline.transactions().len()
        );
        for (actual, expected) in observation
            .transactions()
            .iter()
            .zip(baseline.transactions())
        {
            let expected_transaction = expected.transaction();
            assert_eq!(actual.epoch(), expected_transaction.epoch());
            assert_eq!(actual.sequence(), expected_transaction.sequence());
            assert_eq!(
                actual.first_owned_page_position(),
                expected.first_owned_page_position()
            );
            assert_eq!(
                actual.last_owned_page_position(),
                expected.last_owned_page_position()
            );
            assert_eq!(
                actual.owned_page_record_count(),
                expected.owned_page_record_count()
            );
            assert_eq!(
                actual.state().commit_position(),
                expected.state().commit_position()
            );
        }
    }

    #[test]
    fn position_exhaustion_is_explicit_after_assigning_max() -> Result<(), Box<dyn Error>> {
        let mut log = InMemoryCommitLog::new();
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        log.next_position = Some(u64::MAX);

        let last = coordinator.begin()?;
        let committed = coordinator.commit(last, &mut log)?;
        assert_eq!(committed.log_position().get(), u64::MAX);

        let exhausted = coordinator.begin()?;
        let error = coordinator
            .commit(exhausted, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("exhausted log accepted another append"))?;
        let CoordinatedCommitError::Indeterminate(error) = error else {
            return Err(io::Error::other("exhaustion was rejected before WAL").into());
        };
        assert_eq!(
            error.cause(),
            &CommitError::Append {
                source: InMemoryCommitLogError::PositionSpaceExhausted,
            }
        );
        Ok(())
    }

    #[test]
    fn epoch_exhaustion_survives_restart_without_reissue() -> Result<(), Box<dyn Error>> {
        let mut log = InMemoryCommitLog::new();
        log.next_epoch = Some(NonZeroU64::MAX);

        let coordinator = TransactionCoordinator::open(&mut log)?;
        assert_eq!(coordinator.epoch().get(), u64::MAX);

        let mut restarted = log.restart();
        assert_eq!(
            TransactionCoordinator::open(&mut restarted).err(),
            Some(InMemoryTransactionEpochError::EpochSpaceExhausted)
        );
        assert_eq!(
            TransactionCoordinator::open(&mut restarted).err(),
            Some(InMemoryTransactionEpochError::EpochSpaceExhausted)
        );
        Ok(())
    }

    #[test]
    fn duplicate_commit_records_retain_indeterminate_resolution() -> Result<(), Box<dyn Error>> {
        let mut log = InMemoryCommitLog::new();
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        log.arm_fault(FaultPoint::AfterFlush)?;
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        let error = coordinator
            .commit(active, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("faulted commit unexpectedly succeeded"))?;
        let CoordinatedCommitError::Indeterminate(error) = error else {
            return Err(io::Error::other("faulted commit was rejected before WAL").into());
        };
        let (indeterminate, _) = error.into_parts();
        let record = log
            .records
            .first()
            .cloned()
            .ok_or_else(|| io::Error::other("durable record is missing"))?;
        log.records.push(record);
        log.durable_len = 2;

        let error = coordinator
            .resolve(indeterminate, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("duplicate records produced a resolution"))?;

        assert_eq!(
            error.failure(),
            &TransactionResolutionFailure::Source(
                InMemoryTransactionRecoveryError::DuplicateCommitRecord(transaction_id)
            )
        );
        assert_eq!(error.transaction_id(), transaction_id);
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::Indeterminate)
        );
        Ok(())
    }

    #[test]
    fn recovery_source_errors_retain_projection_causes() -> Result<(), Box<dyn Error>> {
        let lineage = LogLineage::new();
        let page_number =
            PageNumber::new(91).ok_or_else(|| io::Error::other("page number is zero"))?;

        let physical = DurablePageWalObservation::<0>::from_bytes(
            page_number,
            PageVersion::new(1),
            [],
            lineage.position(1),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width physical page projected"))?;
        let expected_physical = DurablePageWalObservation::<0>::from_bytes(
            page_number,
            PageVersion::new(1),
            [],
            lineage.position(1),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width physical page projected twice"))?;
        let error =
            InMemoryCommittedPageRecoverySourceError::PhysicalPageProjection(Box::new(physical));
        assert!(Error::source(&error).is_some());
        let InMemoryCommittedPageRecoverySourceError::PhysicalPageProjection(source) = error else {
            return Err(io::Error::other("physical projection cause changed variant").into());
        };
        assert_eq!(*source, expected_physical);

        let owned = DurableTransactionPageObservation::<0>::from_bytes(
            1,
            1,
            page_number,
            PageVersion::new(2),
            [],
            lineage.position(2),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width owned page projected"))?;
        let expected_owned = DurableTransactionPageObservation::<0>::from_bytes(
            1,
            1,
            page_number,
            PageVersion::new(2),
            [],
            lineage.position(2),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width owned page projected twice"))?;
        let error =
            InMemoryCommittedPageRecoverySourceError::TransactionPageProjection(Box::new(owned));
        assert!(Error::source(&error).is_some());
        let InMemoryCommittedPageRecoverySourceError::TransactionPageProjection(source) = error
        else {
            return Err(io::Error::other("owned projection cause changed variant").into());
        };
        assert_eq!(*source, expected_owned);

        let commit = DurableTransactionCommitObservation::from_fields(0, 1, lineage.position(3))
            .err()
            .ok_or_else(|| io::Error::other("zero-epoch commit projected"))?;
        let expected_commit = commit.clone();
        let error =
            InMemoryCommittedPageRecoverySourceError::<1>::CommitProjection(Box::new(commit));
        assert!(Error::source(&error).is_some());
        let InMemoryCommittedPageRecoverySourceError::CommitProjection(source) = error else {
            return Err(io::Error::other("commit projection cause changed variant").into());
        };
        assert_eq!(*source, expected_commit);

        let capacity = InMemoryCommittedPageRecoverySourceError::<1>::EvidenceCapacityExhausted {
            projection: InMemoryPageRecoveryProjection::Commits,
        };
        assert!(Error::source(&capacity).is_none());
        Ok(())
    }

    #[test]
    fn restart_source_errors_retain_projection_causes() -> Result<(), Box<dyn Error>> {
        let page_number =
            PageNumber::new(93).ok_or_else(|| io::Error::other("page number is zero"))?;

        let raw_lineage = LogLineage::new();
        let raw_position = raw_lineage.position(1);
        let expected_raw = DurablePageWalObservation::<0>::from_bytes(
            page_number,
            PageVersion::new(1),
            [],
            raw_position.clone(),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width raw page projected"))?;
        let mut raw_log = InMemoryCommitLog::<0>::with_lineage(raw_lineage);
        raw_log.records.push(InMemoryLogRecord {
            position: raw_position,
            kind: InMemoryLogRecordKind::PageWrite(InMemoryPageWriteRecord {
                page_number,
                page_version: PageVersion::new(1),
                bytes: [],
            }),
        });
        raw_log.durable_len = 1;
        let mut raw_callback = false;
        let raw_error =
            DurableTransactionRestartAnalysisSource::with_durable_transaction_restart_observations(
                &mut raw_log,
                |_frontier, _observations| raw_callback = true,
            )
            .err()
            .ok_or_else(|| io::Error::other("malformed raw page entered restart analysis"))?;
        assert!(!raw_callback);
        assert!(Error::source(&raw_error).is_some());
        let InMemoryTransactionRestartAnalysisSourceError::PageProjection(source) = raw_error
        else {
            return Err(io::Error::other("raw restart cause changed variant").into());
        };
        assert_eq!(*source, expected_raw);

        let mut identity_log = InMemoryCommitLog::<1>::new();
        let mut coordinator = TransactionCoordinator::open(&mut identity_log)?;
        let owner = coordinator.begin()?.transaction_id();

        let transaction_lineage = LogLineage::new();
        let transaction_position = transaction_lineage.position(2);
        let expected_transaction = DurableTransactionPageObservation::<0>::from_bytes(
            owner.epoch().get(),
            owner.sequence(),
            page_number,
            PageVersion::new(2),
            [],
            transaction_position.clone(),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width transaction page projected"))?;
        let mut transaction_log = InMemoryCommitLog::<0>::with_lineage(transaction_lineage);
        transaction_log.records.push(InMemoryLogRecord {
            position: transaction_position,
            kind: InMemoryLogRecordKind::TransactionPageWrite(InMemoryTransactionPageWriteRecord {
                transaction_id: owner,
                page: InMemoryPageWriteRecord {
                    page_number,
                    page_version: PageVersion::new(2),
                    bytes: [],
                },
            }),
        });
        transaction_log.durable_len = 1;
        let mut transaction_callback = false;
        let transaction_error =
            DurableTransactionRestartAnalysisSource::with_durable_transaction_restart_observations(
                &mut transaction_log,
                |_frontier, _observations| transaction_callback = true,
            )
            .err()
            .ok_or_else(|| {
                io::Error::other("malformed transaction page entered restart analysis")
            })?;
        assert!(!transaction_callback);
        assert!(Error::source(&transaction_error).is_some());
        let InMemoryTransactionRestartAnalysisSourceError::TransactionPageProjection(source) =
            transaction_error
        else {
            return Err(io::Error::other("transaction restart cause changed variant").into());
        };
        assert_eq!(*source, expected_transaction);

        let commit_lineage = LogLineage::new();
        let zero_position = commit_lineage.position(0);
        let expected_commit = DurableTransactionCommitObservation::from_fields(
            owner.epoch().get(),
            owner.sequence(),
            zero_position.clone(),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-position commit projected"))?;
        let mut commit_log = InMemoryCommitLog::<1>::with_lineage(commit_lineage);
        commit_log.records.push(InMemoryLogRecord {
            position: zero_position,
            kind: InMemoryLogRecordKind::TransactionCommit {
                transaction_id: owner,
            },
        });
        commit_log.durable_len = 1;
        let mut commit_callback = false;
        let commit_error =
            DurableTransactionRestartAnalysisSource::with_durable_transaction_restart_observations(
                &mut commit_log,
                |_frontier, _observations| commit_callback = true,
            )
            .err()
            .ok_or_else(|| io::Error::other("malformed commit entered restart analysis"))?;
        assert!(!commit_callback);
        assert!(Error::source(&commit_error).is_some());
        let InMemoryTransactionRestartAnalysisSourceError::CommitProjection(source) = commit_error
        else {
            return Err(io::Error::other("commit restart cause changed variant").into());
        };
        assert_eq!(*source, expected_commit);

        let mut capacity_log = InMemoryCommitLog::<1>::new();
        capacity_log.durable_len = usize::MAX;
        let mut capacity_callback = false;
        let capacity =
            DurableTransactionRestartAnalysisSource::with_durable_transaction_restart_observations(
                &mut capacity_log,
                |_frontier, _observations| capacity_callback = true,
            )
            .err()
            .ok_or_else(|| io::Error::other("impossible restart capacity was reserved"))?;
        assert!(!capacity_callback);
        assert!(Error::source(&capacity).is_none());
        assert_eq!(
            capacity,
            InMemoryTransactionRestartAnalysisSourceError::ObservationCapacityExhausted {
                record_count: usize::MAX,
            }
        );
        Ok(())
    }

    #[test]
    fn checkpoint_source_preserves_seed_and_retries_one_shot_fault() -> Result<(), Box<dyn Error>> {
        let raw_entries = vec![
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                0,
                0,
                Some(9),
                None,
                0,
                DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                    commit_position: 0,
                },
            ),
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                7,
                3,
                None,
                Some(2),
                u64::MAX,
                DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted,
            ),
        ];
        let seeded = OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
            0,
            Some(0),
            raw_entries,
        );
        let mut source = InMemoryTransactionRestartCheckpointBaselineSource::seeded(seeded.clone());
        assert_eq!(source.slot(), Some(&seeded));
        assert_eq!(source.armed_fault(), None);

        source.arm_fault(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad)?;
        let already_armed = source
            .arm_fault(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad)
            .err()
            .ok_or_else(|| io::Error::other("armed checkpoint fault was replaced"))?;
        assert_eq!(
            already_armed.armed(),
            RestartCheckpointBaselineSourceFaultPoint::BeforeLoad
        );
        assert_eq!(
            already_armed.requested(),
            RestartCheckpointBaselineSourceFaultPoint::BeforeLoad
        );
        assert_eq!(
            source.load_restart_checkpoint_baseline(),
            Err(
                InMemoryTransactionRestartCheckpointBaselineSourceError::InjectedFault(
                    RestartCheckpointBaselineSourceFaultPoint::BeforeLoad,
                )
            )
        );
        assert_eq!(source.armed_fault(), None);
        assert_eq!(source.slot(), Some(&seeded));

        assert_eq!(
            source.load_restart_checkpoint_baseline()?,
            Some(seeded.clone())
        );
        assert_eq!(
            source.load_restart_checkpoint_baseline()?,
            Some(seeded.clone())
        );
        assert_eq!(source.slot(), Some(&seeded));

        let mut empty = InMemoryTransactionRestartCheckpointBaselineSource::default();
        assert_eq!(empty.load_restart_checkpoint_baseline()?, None);
        assert_eq!(empty.slot(), None);

        let capacity =
            InMemoryTransactionRestartCheckpointBaselineSourceError::TransactionCapacityExhausted {
                transaction_count: seeded.transactions().len(),
            };
        assert!(Error::source(&capacity).is_none());
        assert!(capacity.to_string().contains("2 transaction entries"));
        Ok(())
    }

    #[test]
    fn checkpoint_publication_replaces_then_loads_untrusted_exact_current_baseline()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1381)
            .ok_or_else(|| io::Error::other("persistent log id is zero"))?;
        let mut owner = checkpoint_publication_owner(persistent_log_id, 138)?;
        let expected = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
        assert_eq!(expected.transactions().len(), 2);
        let startup_frontier = owner
            .restart_analysis()
            .durable_frontier()
            .map(LogSequenceNumber::get);
        let startup_transaction_count = owner.restart_analysis().transactions().len();
        let stored_pages = owner.parts().1.pages().to_vec();

        let seeded =
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(0, Some(0), vec![]);
        let mut checkpoint =
            InMemoryTransactionRestartCheckpointBaselineSource::seeded(seeded.clone());
        checkpoint.arm_fault(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad)?;

        let receipt =
            owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
        assert_eq!(receipt.persistent_log_id(), expected.persistent_log_id());
        assert_eq!(receipt.durable_frontier(), expected.durable_frontier());
        assert_eq!(receipt.transaction_count(), expected.transactions().len());
        let published = checkpoint
            .slot()
            .ok_or_else(|| io::Error::other("successful publication left no slot"))?;
        assert_ne!(published, &seeded);
        assert_checkpoint_observation_matches_baseline(published, &expected);
        assert_eq!(
            checkpoint.armed_fault(),
            Some(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad)
        );
        assert_eq!(checkpoint.armed_publication_fault(), None);

        assert_eq!(
            checkpoint.load_restart_checkpoint_baseline(),
            Err(
                InMemoryTransactionRestartCheckpointBaselineSourceError::InjectedFault(
                    RestartCheckpointBaselineSourceFaultPoint::BeforeLoad
                )
            )
        );
        let first_load = checkpoint
            .load_restart_checkpoint_baseline()?
            .ok_or_else(|| io::Error::other("published checkpoint did not load"))?;
        let second_load = checkpoint
            .load_restart_checkpoint_baseline()?
            .ok_or_else(|| io::Error::other("published checkpoint was consumed"))?;
        assert_eq!(first_load, second_load);
        assert_checkpoint_observation_matches_baseline(&first_load, &expected);
        assert_eq!(checkpoint.slot(), Some(&first_load));
        assert_eq!(
            owner.validate_restart_checkpoint_baseline_from_source(&mut checkpoint)?,
            Some(expected)
        );

        assert_eq!(
            owner
                .restart_analysis()
                .durable_frontier()
                .map(LogSequenceNumber::get),
            startup_frontier
        );
        assert_eq!(
            owner.restart_analysis().transactions().len(),
            startup_transaction_count
        );
        assert_eq!(owner.parts().1.pages(), stored_pages);
        Ok(())
    }

    #[test]
    fn checkpoint_publication_faults_preserve_before_and_replace_after_effect()
    -> Result<(), Box<dyn Error>> {
        let before_log_id = PersistentLogId::new(0x1382)
            .ok_or_else(|| io::Error::other("before-fault log id is zero"))?;
        let mut before_owner = checkpoint_publication_owner(before_log_id, 140)?;
        let before_expected =
            before_owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
        let old_slot =
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(7, Some(9), vec![]);
        let mut before =
            InMemoryTransactionRestartCheckpointBaselineSource::seeded(old_slot.clone());
        before.arm_fault(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad)?;
        before
            .arm_publication_fault(RestartCheckpointBaselinePublicationFaultPoint::BeforeReplace)?;
        let already_armed = before
            .arm_publication_fault(RestartCheckpointBaselinePublicationFaultPoint::AfterReplace)
            .err()
            .ok_or_else(|| io::Error::other("armed publication fault was replaced"))?;
        assert_eq!(
            already_armed.armed(),
            RestartCheckpointBaselinePublicationFaultPoint::BeforeReplace
        );
        assert_eq!(
            already_armed.requested(),
            RestartCheckpointBaselinePublicationFaultPoint::AfterReplace
        );

        let before_failure = before_owner
            .publish_restart_checkpoint_baseline_from_current_prefix(&mut before)
            .err()
            .ok_or_else(|| io::Error::other("before-replacement fault reported success"))?;
        let DurableTransactionRestartCheckpointBaselineCurrentPublicationError::Publication(
            publication_failure,
        ) = &before_failure
        else {
            return Err(io::Error::other("before-replacement fault became preparation").into());
        };
        assert_eq!(
            publication_failure.cause(),
            &InMemoryTransactionRestartCheckpointBaselinePublicationError::InjectedFault(
                RestartCheckpointBaselinePublicationFaultPoint::BeforeReplace
            )
        );
        assert_eq!(
            publication_failure.publication().persistent_log_id(),
            before_expected.persistent_log_id()
        );
        assert_eq!(
            publication_failure.publication().durable_frontier(),
            before_expected.durable_frontier()
        );
        assert_eq!(
            publication_failure.publication().transaction_count(),
            before_expected.transactions().len()
        );
        assert_eq!(before.slot(), Some(&old_slot));
        assert_eq!(before.armed_publication_fault(), None);
        assert_eq!(
            before.armed_fault(),
            Some(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad)
        );
        assert!(Error::source(&before_failure).is_some());
        assert!(Error::source(publication_failure).is_some());
        assert!(Error::source(publication_failure.cause()).is_none());

        let after_log_id = PersistentLogId::new(0x1383)
            .ok_or_else(|| io::Error::other("after-fault log id is zero"))?;
        let mut after_owner = checkpoint_publication_owner(after_log_id, 142)?;
        let after_expected =
            after_owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
        let mut after =
            InMemoryTransactionRestartCheckpointBaselineSource::seeded(old_slot.clone());
        after
            .arm_publication_fault(RestartCheckpointBaselinePublicationFaultPoint::AfterReplace)?;
        assert_eq!(
            after.load_restart_checkpoint_baseline()?,
            Some(old_slot.clone())
        );
        assert_eq!(
            after.armed_publication_fault(),
            Some(RestartCheckpointBaselinePublicationFaultPoint::AfterReplace)
        );
        let after_failure = after_owner
            .publish_restart_checkpoint_baseline_from_current_prefix(&mut after)
            .err()
            .ok_or_else(|| io::Error::other("after-replacement fault reported success"))?;
        let DurableTransactionRestartCheckpointBaselineCurrentPublicationError::Publication(
            after_failure,
        ) = &after_failure
        else {
            return Err(io::Error::other("after-replacement fault became preparation").into());
        };
        assert_eq!(
            after_failure.cause(),
            &InMemoryTransactionRestartCheckpointBaselinePublicationError::InjectedFault(
                RestartCheckpointBaselinePublicationFaultPoint::AfterReplace
            )
        );
        assert_eq!(
            after_failure.publication().persistent_log_id(),
            after_expected.persistent_log_id()
        );
        assert_eq!(
            after_failure.publication().durable_frontier(),
            after_expected.durable_frontier()
        );
        assert_eq!(
            after_failure.publication().transaction_count(),
            after_expected.transactions().len()
        );
        let installed = after
            .slot()
            .ok_or_else(|| io::Error::other("after-replacement fault left no slot"))?;
        assert_ne!(installed, &old_slot);
        assert_checkpoint_observation_matches_baseline(installed, &after_expected);
        assert_eq!(after.armed_publication_fault(), None);

        let capacity =
            InMemoryTransactionRestartCheckpointBaselinePublicationError::TransactionCapacityExhausted {
                transaction_count: after_expected.transactions().len(),
            };
        assert!(capacity.to_string().contains("2 transaction entries"));
        assert!(Error::source(&capacity).is_none());
        Ok(())
    }

    #[test]
    fn checkpoint_publication_replaces_with_exact_empty_baseline() -> Result<(), Box<dyn Error>> {
        let persistent_log_id =
            PersistentLogId::new(0x1384).ok_or_else(|| io::Error::other("empty log id is zero"))?;
        let log = InMemoryCommitLog::<1>::with_persistent_lineage_id(persistent_log_id);
        let store = InMemoryPageStore::new(&log);
        let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
            .recover()?
            .analyze_restart()?;
        let mut checkpoint = InMemoryTransactionRestartCheckpointBaselineSource::seeded(
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
                u128::MAX,
                Some(u64::MAX),
                vec![],
            ),
        );

        let receipt =
            owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
        assert_eq!(receipt.persistent_log_id(), persistent_log_id);
        assert_eq!(receipt.durable_frontier(), None);
        assert_eq!(receipt.transaction_count(), 0);
        let published = checkpoint
            .slot()
            .ok_or_else(|| io::Error::other("empty baseline was not published"))?;
        assert_eq!(published.persistent_log_id(), persistent_log_id.get());
        assert_eq!(published.durable_frontier(), None);
        assert!(published.transactions().is_empty());
        assert_eq!(
            owner.validate_restart_checkpoint_baseline_from_source(&mut checkpoint)?,
            Some(owner.prepare_restart_checkpoint_baseline_from_current_prefix()?)
        );
        Ok(())
    }

    #[test]
    fn completeness_checkpoint_source_preserves_seed_and_retries_one_shot_fault()
    -> Result<(), Box<dyn Error>> {
        let raw_transactions = OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
            0,
            Some(0),
            vec![
                DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                    0,
                    0,
                    Some(9),
                    None,
                    0,
                    DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                        commit_position: 0,
                    },
                ),
                DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                    7,
                    3,
                    None,
                    Some(2),
                    u64::MAX,
                    DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted,
                ),
            ],
        );
        let raw_pages = vec![
            DurableTransactionRestartCheckpointCompletenessBaselinePageObservation::new(
                0,
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::NoRequiredImage,
                Some(
                    DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::Raw {
                        page_position: 0,
                    },
                ),
                Some(0),
            ),
            DurableTransactionRestartCheckpointCompletenessBaselinePageObservation::new(
                u64::MAX,
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreBehind,
                None,
                None,
            ),
        ];
        let raw_replay =
            DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation::new(
                DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition,
                Some(0),
                None,
                Some(
                    DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::UncommittedTransaction {
                        epoch: 0,
                        sequence: 0,
                    },
                ),
            );
        let seeded = OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation::new(
            raw_transactions,
            raw_pages,
            raw_replay,
        );
        let mut source =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(seeded.clone());
        assert_eq!(source.slot(), Some(&seeded));
        assert_eq!(source.armed_fault(), None);
        assert_eq!(source.armed_publication_fault(), None);

        source.arm_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)?;
        let already_armed = source
            .arm_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)
            .err()
            .ok_or_else(|| io::Error::other("armed completeness fault was replaced"))?;
        assert_eq!(
            already_armed.armed(),
            RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad
        );
        assert_eq!(
            already_armed.requested(),
            RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad
        );
        assert!(Error::source(&already_armed).is_none());
        assert_eq!(
            source.load_restart_checkpoint_completeness_baseline(),
            Err(
                InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::InjectedFault(
                    RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad,
                )
            )
        );
        assert_eq!(source.armed_fault(), None);
        assert_eq!(source.slot(), Some(&seeded));

        assert_eq!(
            source.load_restart_checkpoint_completeness_baseline()?,
            Some(seeded.clone())
        );
        assert_eq!(
            source.load_restart_checkpoint_completeness_baseline()?,
            Some(seeded.clone())
        );
        assert_eq!(source.slot(), Some(&seeded));

        let mut empty = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::default();
        assert_eq!(empty.load_restart_checkpoint_completeness_baseline()?, None);
        assert_eq!(empty.slot(), None);

        let transaction_capacity =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::TransactionCapacityExhausted {
                transaction_count: seeded.transactions().transactions().len(),
            };
        assert!(Error::source(&transaction_capacity).is_none());
        assert!(
            transaction_capacity
                .to_string()
                .contains("2 transaction entries")
        );
        let page_capacity =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::PageCapacityExhausted {
                page_count: seeded.pages().len(),
            };
        assert!(Error::source(&page_capacity).is_none());
        assert!(page_capacity.to_string().contains("2 page entries"));
        Ok(())
    }

    fn assert_completeness_observation_matches_baseline(
        observation: &OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
        baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
    ) {
        assert_checkpoint_observation_matches_baseline(
            observation.transactions(),
            baseline.transaction_baseline(),
        );
        assert_eq!(observation.pages().len(), baseline.pages().len());
        for (actual, expected) in observation.pages().iter().zip(baseline.pages()) {
            assert_eq!(actual.page_number(), expected.page_number().get());
            assert_eq!(
                actual.state(),
                match expected.state() {
                    DurableTransactionRestartPageState::NoRequiredImage =>
                        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::NoRequiredImage,
                    DurableTransactionRestartPageState::StoreMissing { .. } =>
                        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreMissing,
                    DurableTransactionRestartPageState::StoreCurrent { .. } =>
                        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent,
                    DurableTransactionRestartPageState::StoreBehind { .. } =>
                        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreBehind,
                }
            );
            assert_eq!(
                actual.required_image(),
                expected
                    .state()
                    .required_image()
                    .map(decoded_completeness_required_image_observation)
            );
            assert_eq!(actual.stored_position(), expected.state().stored_position());
        }
        let replay = observation.replay();
        let expected_replay = baseline.replay_start();
        assert_eq!(
            replay.kind(),
            match expected_replay {
                DurableTransactionRestartReplayStart::AfterFrontier { .. } =>
                    DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AfterFrontier,
                DurableTransactionRestartReplayStart::AtPosition { .. } =>
                    DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition,
            }
        );
        assert_eq!(replay.frontier(), expected_replay.frontier());
        assert_eq!(replay.position(), expected_replay.position());
        assert_eq!(
            replay.cause(),
            expected_replay.cause().map(|cause| match cause {
                DurableTransactionRestartReplayStartCause::StoreMissing { page_number } =>
                    DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreMissingPage {
                        page_number: page_number.get(),
                    },
                DurableTransactionRestartReplayStartCause::StoreBehind { page_number } =>
                    DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreBehindPage {
                        page_number: page_number.get(),
                    },
                DurableTransactionRestartReplayStartCause::UncommittedTransaction {
                    transaction,
                } =>
                    DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::UncommittedTransaction {
                        epoch: transaction.epoch(),
                        sequence: transaction.sequence(),
                    },
            })
        );
    }

    fn arbitrary_completeness_slot()
    -> OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation {
        OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation::new(
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
                7,
                Some(9),
                Vec::new(),
            ),
            vec![
                DurableTransactionRestartCheckpointCompletenessBaselinePageObservation::new(
                    5,
                    DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreMissing,
                    None,
                    Some(4),
                ),
            ],
            DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation::new(
                DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AfterFrontier,
                Some(9),
                Some(3),
                None,
            ),
        )
    }

    #[test]
    fn completeness_checkpoint_publication_replaces_then_loads_untrusted_exact_current_baseline()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1580)
            .ok_or_else(|| io::Error::other("completeness publication log id is zero"))?;
        let mut owner = checkpoint_publication_owner(persistent_log_id, 150)?;
        let expected =
            owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        assert_eq!(expected.transactions().len(), 2);
        assert_eq!(expected.pages().len(), 2);
        let startup_frontier = owner
            .restart_analysis()
            .durable_frontier()
            .map(LogSequenceNumber::get);
        let startup_transaction_count = owner.restart_analysis().transactions().len();
        let stored_pages = owner.parts().1.pages().to_vec();
        let durable_records = owner.parts().0.records.len();

        let seeded = arbitrary_completeness_slot();
        let mut checkpoint =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(seeded.clone());
        checkpoint.arm_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)?;

        let receipt = owner.publish_restart_checkpoint_completeness_baseline_from_current_prefix(
            &mut checkpoint,
        )?;
        assert_eq!(receipt.persistent_log_id(), expected.persistent_log_id());
        assert_eq!(receipt.durable_frontier(), expected.durable_frontier());
        assert_eq!(receipt.transaction_count(), expected.transactions().len());
        assert_eq!(receipt.page_count(), expected.pages().len());

        let published = checkpoint
            .slot()
            .ok_or_else(|| io::Error::other("successful publication left no completeness slot"))?;
        assert_ne!(published, &seeded);
        assert_completeness_observation_matches_baseline(published, &expected);
        assert_eq!(published, &owned_completeness_checkpoint(&expected));
        assert_eq!(
            checkpoint.armed_fault(),
            Some(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)
        );
        assert_eq!(checkpoint.armed_publication_fault(), None);

        assert_eq!(
            checkpoint.load_restart_checkpoint_completeness_baseline(),
            Err(
                InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::InjectedFault(
                    RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad
                )
            )
        );
        let first_load = checkpoint
            .load_restart_checkpoint_completeness_baseline()?
            .ok_or_else(|| io::Error::other("published completeness checkpoint did not load"))?;
        let second_load = checkpoint
            .load_restart_checkpoint_completeness_baseline()?
            .ok_or_else(|| io::Error::other("published completeness checkpoint was consumed"))?;
        assert_eq!(first_load, second_load);
        assert_completeness_observation_matches_baseline(&first_load, &expected);
        assert_eq!(checkpoint.slot(), Some(&first_load));

        assert_eq!(
            owner.validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)?,
            Some(expected)
        );

        assert_eq!(
            owner
                .restart_analysis()
                .durable_frontier()
                .map(LogSequenceNumber::get),
            startup_frontier
        );
        assert_eq!(
            owner.restart_analysis().transactions().len(),
            startup_transaction_count
        );
        assert_eq!(owner.parts().1.pages(), stored_pages);
        assert_eq!(owner.parts().0.records.len(), durable_records);
        Ok(())
    }

    #[test]
    fn completeness_checkpoint_publication_faults_preserve_before_and_replace_after_effect()
    -> Result<(), Box<dyn Error>> {
        let before_log_id = PersistentLogId::new(0x1581)
            .ok_or_else(|| io::Error::other("before-fault completeness log id is zero"))?;
        let mut before_owner = checkpoint_publication_owner(before_log_id, 152)?;
        let before_expected =
            before_owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let old_slot = arbitrary_completeness_slot();
        let mut before = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            old_slot.clone(),
        );
        before.arm_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)?;
        before.arm_publication_fault(
            RestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeReplace,
        )?;
        let already_armed = before
            .arm_publication_fault(
                RestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterReplace,
            )
            .err()
            .ok_or_else(|| io::Error::other("armed completeness publication fault was replaced"))?;
        assert_eq!(
            already_armed.armed(),
            RestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeReplace
        );
        assert_eq!(
            already_armed.requested(),
            RestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterReplace
        );
        assert!(Error::source(&already_armed).is_none());

        let before_failure = before_owner
            .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut before)
            .err()
            .ok_or_else(|| {
                io::Error::other("before-replacement completeness fault reported success")
            })?;
        let DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError::Publication(
            publication_failure,
        ) = &before_failure
        else {
            return Err(io::Error::other(
                "before-replacement completeness fault became preparation",
            )
            .into());
        };
        assert_eq!(
            publication_failure.cause(),
            &InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::InjectedFault(
                RestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeReplace
            )
        );
        assert_eq!(
            publication_failure.publication().persistent_log_id(),
            before_expected.persistent_log_id()
        );
        assert_eq!(
            publication_failure.publication().durable_frontier(),
            before_expected.durable_frontier()
        );
        assert_eq!(
            publication_failure.publication().transaction_count(),
            before_expected.transactions().len()
        );
        assert_eq!(
            publication_failure.publication().page_count(),
            before_expected.pages().len()
        );
        assert_eq!(before.slot(), Some(&old_slot));
        assert_eq!(before.armed_publication_fault(), None);
        assert_eq!(
            before.armed_fault(),
            Some(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)
        );
        assert!(Error::source(&before_failure).is_some());
        assert!(Error::source(publication_failure).is_some());
        assert!(Error::source(publication_failure.cause()).is_none());

        // The one-shot fault is consumed, so a later fresh attempt succeeds and
        // exactly replaces the preserved old slot.
        let retried = before_owner
            .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut before)?;
        assert_eq!(retried.page_count(), before_expected.pages().len());
        let retried_slot = before
            .slot()
            .ok_or_else(|| io::Error::other("retried completeness publication left no slot"))?;
        assert_ne!(retried_slot, &old_slot);
        assert_completeness_observation_matches_baseline(retried_slot, &before_expected);

        let after_log_id = PersistentLogId::new(0x1582)
            .ok_or_else(|| io::Error::other("after-fault completeness log id is zero"))?;
        let mut after_owner = checkpoint_publication_owner(after_log_id, 154)?;
        let after_expected =
            after_owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let mut after = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            old_slot.clone(),
        );
        after.arm_publication_fault(
            RestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterReplace,
        )?;
        assert_eq!(
            after.load_restart_checkpoint_completeness_baseline()?,
            Some(old_slot.clone())
        );
        assert_eq!(
            after.armed_publication_fault(),
            Some(RestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterReplace)
        );
        let after_failure = after_owner
            .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut after)
            .err()
            .ok_or_else(|| {
                io::Error::other("after-replacement completeness fault reported success")
            })?;
        let DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError::Publication(
            after_failure,
        ) = &after_failure
        else {
            return Err(io::Error::other(
                "after-replacement completeness fault became preparation",
            )
            .into());
        };
        assert_eq!(
            after_failure.cause(),
            &InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::InjectedFault(
                RestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterReplace
            )
        );
        assert_eq!(
            after_failure.publication().persistent_log_id(),
            after_expected.persistent_log_id()
        );
        assert_eq!(
            after_failure.publication().page_count(),
            after_expected.pages().len()
        );
        let installed = after
            .slot()
            .ok_or_else(|| io::Error::other("after-replacement completeness left no slot"))?;
        assert_ne!(installed, &old_slot);
        assert_completeness_observation_matches_baseline(installed, &after_expected);
        assert_eq!(after.armed_publication_fault(), None);

        let transaction_capacity =
            InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::TransactionCapacityExhausted {
                transaction_count: after_expected.transactions().len(),
            };
        assert!(
            transaction_capacity
                .to_string()
                .contains("2 transaction entries")
        );
        assert!(Error::source(&transaction_capacity).is_none());
        let page_capacity =
            InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::PageCapacityExhausted {
                page_count: after_expected.pages().len(),
            };
        assert!(page_capacity.to_string().contains("2 page entries"));
        assert!(Error::source(&page_capacity).is_none());

        let mismatch =
            InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::PublicationPermitMismatch {
                baseline_persistent_log_id: after_log_id.get(),
                permit_persistent_log_id: before_log_id.get(),
                baseline_durable_frontier: after_expected.durable_frontier(),
                permit_durable_frontier: None,
                baseline_transaction_count: after_expected.transactions().len(),
                permit_transaction_count: 0,
                baseline_page_count: after_expected.pages().len(),
                permit_page_count: 0,
            };
        assert!(mismatch.to_string().contains("permit mismatch"));
        assert!(Error::source(&mismatch).is_none());
        Ok(())
    }

    #[test]
    fn completeness_checkpoint_publication_replaces_with_exact_empty_baseline()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1583)
            .ok_or_else(|| io::Error::other("empty completeness log id is zero"))?;
        let log = InMemoryCommitLog::<1>::with_persistent_lineage_id(persistent_log_id);
        let store = InMemoryPageStore::new(&log);
        let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
            .recover()?
            .analyze_restart()?;
        let mut checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            arbitrary_completeness_slot(),
        );

        let receipt = owner.publish_restart_checkpoint_completeness_baseline_from_current_prefix(
            &mut checkpoint,
        )?;
        assert_eq!(receipt.persistent_log_id(), persistent_log_id);
        assert_eq!(receipt.durable_frontier(), None);
        assert_eq!(receipt.transaction_count(), 0);
        assert_eq!(receipt.page_count(), 0);

        let published = checkpoint
            .slot()
            .ok_or_else(|| io::Error::other("empty completeness baseline was not published"))?;
        assert_eq!(
            published.transactions().persistent_log_id(),
            persistent_log_id.get()
        );
        assert_eq!(published.transactions().durable_frontier(), None);
        assert!(published.transactions().transactions().is_empty());
        assert!(published.pages().is_empty());
        assert_eq!(
            published.replay(),
            DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation::new(
                DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AfterFrontier,
                None,
                None,
                None,
            )
        );

        assert_eq!(
            owner.validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)?,
            Some(owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?)
        );
        Ok(())
    }

    #[test]
    fn completeness_checkpoint_source_failure_returns_before_any_wal_or_store_work()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1584)
            .ok_or_else(|| io::Error::other("completeness source failure log id is zero"))?;
        let mut owner = checkpoint_publication_owner(persistent_log_id, 156)?;
        let expected =
            owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let stored_pages = owner.parts().1.pages().to_vec();
        let durable_records = owner.parts().0.records.len();
        let startup_frontier = owner
            .restart_analysis()
            .durable_frontier()
            .map(LogSequenceNumber::get);

        let mut absent = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
        assert_eq!(
            owner.validate_restart_checkpoint_completeness_baseline_from_source(&mut absent)?,
            None
        );

        let mut checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            owned_completeness_checkpoint(&expected),
        );
        checkpoint.arm_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)?;
        let failure = owner
            .validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)
            .err()
            .ok_or_else(|| io::Error::other("completeness source fault validated a slot"))?;
        assert!(matches!(
            failure,
            DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError::CheckpointSource(
                InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::InjectedFault(
                    RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad
                )
            )
        ));
        assert_eq!(
            Error::source(&failure).map(ToString::to_string),
            Some(String::from(
                "injected completeness checkpoint-source failure before load"
            ))
        );

        assert_eq!(
            owner.validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)?,
            Some(expected)
        );
        assert_eq!(owner.parts().1.pages(), stored_pages);
        assert_eq!(owner.parts().0.records.len(), durable_records);
        assert_eq!(
            owner
                .restart_analysis()
                .durable_frontier()
                .map(LogSequenceNumber::get),
            startup_frontier
        );
        Ok(())
    }

    #[test]
    fn recovery_store_recheck_rejects_target_and_changed_source() -> Result<(), Box<dyn Error>> {
        let lineage = LogLineage::new();
        let page_number =
            PageNumber::new(92).ok_or_else(|| io::Error::other("page number is zero"))?;
        let physical = [DurablePageWalObservation::from_bytes(
            page_number,
            PageVersion::new(4),
            [4],
            lineage.position(1),
        )?];
        let owned = [DurableTransactionPageObservation::from_bytes(
            1,
            1,
            page_number,
            PageVersion::new(4),
            [4],
            lineage.position(1),
        )?];
        let commits = [DurableTransactionCommitObservation::from_fields(
            1,
            1,
            lineage.position(2),
        )?];
        let decision = ntsql_transaction::derive_committed_transaction_page_recovery_candidate(
            &lineage,
            page_number,
            None,
            &physical,
            &owned,
            &commits,
        )?;
        let ntsql_transaction::DurableCommittedTransactionPageRecoveryDecision::Candidate(
            candidate,
        ) = decision
        else {
            return Err(io::Error::other("missing store did not produce a candidate").into());
        };

        let target = StoredPageSnapshotObservation::from_bytes(
            page_number,
            PageVersion::new(4),
            [4],
            lineage.position(1),
        )?;
        assert_eq!(
            require_in_memory_recovery_source_match(&candidate, Some(&target)),
            Err(InMemoryCommittedPageRecoveryStoreError::SourceNotMatched {
                actual: DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent,
            })
        );

        let changed = StoredPageSnapshotObservation::from_bytes(
            page_number,
            PageVersion::new(9),
            [9],
            lineage.position(3),
        )?;
        assert_eq!(
            require_in_memory_recovery_source_match(&candidate, Some(&changed)),
            Err(InMemoryCommittedPageRecoveryStoreError::SourceComparison(
                Box::new(
                    DurableCommittedTransactionPageRecoveryComparisonError::StoreChanged {
                        page_number,
                        expected_source_position: None,
                        actual_position: Some(lineage.position(3)),
                    },
                )
            ))
        );
        Ok(())
    }

    #[test]
    fn batch_recovery_uses_sorted_owned_inventory_stops_and_reruns_fresh()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1282)
            .ok_or_else(|| io::Error::other("persistent log id is zero"))?;
        let mut log = InMemoryCommitLog::<1>::with_persistent_lineage_id(persistent_log_id);
        let mut store = InMemoryPageStore::new(&log);
        let mut coordinator = TransactionCoordinator::open(&mut log)?;

        let behind_page_number =
            PageNumber::new(83).ok_or_else(|| io::Error::other("behind page is zero"))?;
        let behind_active = coordinator.begin()?;
        let behind_owner = behind_active.transaction_id();
        let behind_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), behind_page_number),
            PageVersion::new(100),
            PageImage::new([0xA3])?,
        );
        let (behind_active, behind_dirty) =
            coordinator.stage_page_write(behind_active, behind_page, &mut log)?;
        let behind_commit = coordinator.commit(behind_active, &mut log)?;
        flush_committed_page(&behind_commit, &mut log, &mut store, behind_dirty)?;

        let first_page =
            PageNumber::new(81).ok_or_else(|| io::Error::other("first page is zero"))?;
        let exact_active = coordinator.begin()?;
        let exact_owner = exact_active.transaction_id();
        let exact_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), first_page),
            PageVersion::new(81),
            PageImage::new([0x81])?,
        );
        let (exact_active, exact_dirty) =
            coordinator.stage_page_write(exact_active, exact_page, &mut log)?;
        let exact_commit = coordinator.commit(exact_active, &mut log)?;
        flush_committed_page(&exact_commit, &mut log, &mut store, exact_dirty)?;

        let failed_page =
            PageNumber::new(82).ok_or_else(|| io::Error::other("failed page is zero"))?;
        let missing_active = coordinator.begin()?;
        let missing_owner = missing_active.transaction_id();
        let missing_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), failed_page),
            PageVersion::new(82),
            PageImage::new([0x82])?,
        );
        let (missing_active, missing_dirty) =
            coordinator.stage_page_write(missing_active, missing_page, &mut log)?;
        coordinator.commit(missing_active, &mut log)?;
        drop(missing_dirty);

        let latest_active = coordinator.begin()?;
        let latest_owner = latest_active.transaction_id();
        let latest_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), behind_page_number),
            PageVersion::new(1),
            PageImage::new([0x03])?,
        );
        let (latest_active, latest_dirty) =
            coordinator.stage_page_write(latest_active, latest_page, &mut log)?;
        coordinator.commit(latest_active, &mut log)?;
        drop(latest_dirty);

        let uncommitted_active = coordinator.begin()?;
        let uncommitted_owner = uncommitted_active.transaction_id();
        let uncommitted_page_number =
            PageNumber::new(84).ok_or_else(|| io::Error::other("uncommitted page is zero"))?;
        let uncommitted_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), uncommitted_page_number),
            PageVersion::new(84),
            PageImage::new([0x84])?,
        );
        let (uncommitted_active, uncommitted_dirty) =
            coordinator.stage_page_write(uncommitted_active, uncommitted_page, &mut log)?;
        log.flush_through(uncommitted_dirty.required_position())?;
        drop(uncommitted_active);
        drop(uncommitted_dirty);

        let raw_page_number =
            PageNumber::new(85).ok_or_else(|| io::Error::other("raw page is zero"))?;
        let raw_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), raw_page_number),
            PageVersion::new(85),
            PageImage::new([0x85])?,
        );
        let raw_dirty = ntsql_page::stage_page_write(&mut log, raw_page)?;
        log.flush_through(raw_dirty.required_position())?;
        drop(raw_dirty);

        let volatile_active = coordinator.begin()?;
        let volatile_owner = volatile_active.transaction_id();
        let volatile_page_number =
            PageNumber::new(86).ok_or_else(|| io::Error::other("volatile page is zero"))?;
        let volatile_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), volatile_page_number),
            PageVersion::new(86),
            PageImage::new([0x86])?,
        );
        let (volatile_active, volatile_dirty) =
            coordinator.stage_page_write(volatile_active, volatile_page, &mut log)?;
        log.arm_fault(FaultPoint::BeforeFlush)?;
        assert!(matches!(
            coordinator.commit(volatile_active, &mut log),
            Err(CoordinatedCommitError::Indeterminate(_))
        ));
        drop(volatile_dirty);
        drop(coordinator);

        assert_eq!(log.records().len(), 12);
        assert_eq!(log.durable_records().len(), 10);
        let expected_restart_lineage = LogDurability::lineage(&log).clone();
        let expected_restart_frontier = log
            .durable_position()
            .ok_or_else(|| io::Error::other("durable restart frontier is missing"))?;
        assert_eq!(
            log.records()[10]
                .transaction_page_write()
                .map(|record| record.page_write().page_number()),
            Some(volatile_page_number)
        );
        assert!(log.records()[11].transaction_id().is_some());
        assert!(log.records()[11].transaction_page_write().is_none());
        assert_eq!(
            log.durable_transaction_page_numbers()?,
            [81, 82, 83, 84]
                .into_iter()
                .map(|number| PageNumber::new(number).ok_or_else(|| io::Error::other("page zero")))
                .collect::<Result<Vec<_>, _>>()?
        );

        let last_page = behind_page_number;
        let behind = store
            .page(last_page)
            .ok_or_else(|| io::Error::other("behind page disappeared"))?;
        assert_eq!(behind.page_version(), PageVersion::new(100));
        assert_eq!(behind.bytes(), &[0xA3]);
        store.arm_fault(PageStoreFaultPoint::BeforeWrite)?;

        let failure = UnrecoveredTransactionPageStorage::new(log, store)
            .recover()
            .err()
            .ok_or_else(|| io::Error::other("owning batch unexpectedly succeeded"))?;
        let CommittedTransactionPagesRecoveryError::Page {
            completed,
            page_number,
            source: CommittedTransactionPageRecoveryError::StoreWrite { state: write_state },
        } = failure.error()
        else {
            return Err(io::Error::other("batch did not stop at the injected fault").into());
        };
        assert_eq!(*page_number, failed_page);
        assert_eq!(completed.pages().len(), 1);
        assert_eq!(completed.pages()[0].page_number(), first_page);
        assert_eq!(
            write_state.as_ref().cause(),
            &InMemoryCommittedPageRecoveryStoreError::InjectedFault(
                PageStoreFaultPoint::BeforeWrite
            )
        );

        let page_recovered = failure.retry()?;
        let mut recovered = page_recovered.analyze_restart()?;
        let rerun = recovered.recovery_report();
        assert_eq!(
            rerun
                .pages()
                .iter()
                .map(CommittedTransactionPageRecoveryOutcome::page_number)
                .collect::<Vec<_>>(),
            [first_page, failed_page, last_page, uncommitted_page_number]
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
        assert_eq!(
            rerun.pages()[3],
            CommittedTransactionPageRecoveryOutcome::NoCommittedPage {
                page_number: uncommitted_page_number
            }
        );
        let analysis = recovered.restart_analysis();
        assert!(analysis.lineage().same_lineage(&expected_restart_lineage));
        assert_eq!(
            analysis.durable_frontier(),
            Some(&expected_restart_frontier)
        );
        let expected_transactions = [
            (behind_owner, 1, Some(2)),
            (exact_owner, 3, Some(4)),
            (missing_owner, 5, Some(6)),
            (latest_owner, 7, Some(8)),
            (uncommitted_owner, 9, None),
        ];
        assert_eq!(analysis.transactions().len(), expected_transactions.len());
        for (entry, (owner, page_position, commit_position)) in
            analysis.transactions().iter().zip(expected_transactions)
        {
            assert!(entry.transaction().matches_transaction_id(owner));
            assert_eq!(
                entry
                    .first_owned_page_position()
                    .map(LogSequenceNumber::get),
                Some(page_position)
            );
            assert_eq!(
                entry.last_owned_page_position().map(LogSequenceNumber::get),
                Some(page_position)
            );
            assert_eq!(entry.owned_page_record_count(), 1);
            assert_eq!(
                entry.state().commit_position().map(LogSequenceNumber::get),
                commit_position
            );
        }
        assert_eq!(
            analysis.transactions()[4].state(),
            &DurableTransactionRestartState::Uncommitted
        );
        let checkpoint_baseline = recovered.prepare_restart_checkpoint_baseline()?;
        assert_eq!(checkpoint_baseline.persistent_log_id(), persistent_log_id);
        assert_eq!(
            checkpoint_baseline.durable_frontier(),
            Some(expected_restart_frontier.get())
        );
        assert_eq!(
            checkpoint_baseline.transactions().len(),
            expected_transactions.len()
        );
        for (entry, (owner, page_position, commit_position)) in checkpoint_baseline
            .transactions()
            .iter()
            .zip(expected_transactions)
        {
            assert!(entry.transaction().matches_transaction_id(owner));
            assert_eq!(entry.first_owned_page_position(), Some(page_position));
            assert_eq!(entry.last_owned_page_position(), Some(page_position));
            assert_eq!(entry.owned_page_record_count(), 1);
            assert_eq!(entry.state().commit_position(), commit_position);
        }
        let decoded_checkpoint_entries = checkpoint_baseline
            .transactions()
            .iter()
            .map(|entry| {
                let state = match entry.state().commit_position() {
                    Some(commit_position) => {
                        DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                            commit_position,
                        }
                    }
                    None => {
                        DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted
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
            })
            .collect::<Vec<_>>();
        let owned_checkpoint = OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
            checkpoint_baseline.persistent_log_id().get(),
            checkpoint_baseline.durable_frontier(),
            decoded_checkpoint_entries.clone(),
        );
        let decoded_checkpoint = owned_checkpoint.as_observation();
        let (log, store) = recovered.parts_mut();
        let recovered_behind = store
            .page(last_page)
            .ok_or_else(|| io::Error::other("behind page was not recovered"))?;
        assert_eq!(recovered_behind.page_version(), PageVersion::new(1));
        assert_eq!(recovered_behind.bytes(), &[0x03]);
        let page_count = store.pages().len();
        let idempotent = recover_committed_transaction_pages(log, store)?;
        assert!(idempotent.pages()[..3].iter().all(|outcome| matches!(
            outcome,
            CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
        )));
        assert_eq!(
            idempotent.pages()[3],
            CommittedTransactionPageRecoveryOutcome::NoCommittedPage {
                page_number: uncommitted_page_number
            }
        );
        assert_eq!(store.pages().len(), page_count);

        let live_page_number =
            PageNumber::new(87).ok_or_else(|| io::Error::other("live page is zero"))?;
        let live_page = UnloggedPage::new(
            PageAddress::new(
                LogDurability::lineage(recovered.parts().0),
                live_page_number,
            ),
            PageVersion::new(87),
            PageImage::new([0x87])?,
        );
        let (log, store) = recovered.parts_mut();
        let live_dirty = ntsql_page::stage_page_write(log, live_page)?;
        ntsql_page::flush_dirty_page(log, store, live_dirty)?;
        assert!(store.page(live_page_number).is_some());
        let live_frontier = log
            .durable_position()
            .ok_or_else(|| io::Error::other("live durable frontier is missing"))?;
        assert!(live_frontier.get() > expected_restart_frontier.get());
        assert_eq!(
            recovered.restart_analysis().durable_frontier(),
            Some(&expected_restart_frontier)
        );
        assert_eq!(
            checkpoint_baseline.durable_frontier(),
            Some(expected_restart_frontier.get())
        );
        assert_eq!(checkpoint_baseline.transactions().len(), 5);
        let page_count = recovered.parts().1.pages().len();
        let validated_checkpoint = recovered
            .validate_restart_checkpoint_baseline_against_current_prefix(&decoded_checkpoint)?;
        assert_eq!(validated_checkpoint, checkpoint_baseline);
        assert_eq!(recovered.parts().1.pages().len(), page_count);
        let mut checkpoint_source =
            InMemoryTransactionRestartCheckpointBaselineSource::seeded(owned_checkpoint.clone());
        assert_eq!(
            recovered.validate_restart_checkpoint_baseline_from_source(&mut checkpoint_source)?,
            Some(checkpoint_baseline.clone())
        );
        assert_eq!(
            recovered.validate_restart_checkpoint_baseline_from_source(&mut checkpoint_source)?,
            Some(checkpoint_baseline.clone())
        );
        assert_eq!(checkpoint_source.slot(), Some(&owned_checkpoint));
        assert_eq!(recovered.parts().1.pages().len(), page_count);
        let current_checkpoint =
            recovered.prepare_restart_checkpoint_baseline_from_current_prefix()?;
        assert_eq!(
            current_checkpoint.durable_frontier(),
            Some(live_frontier.get())
        );
        assert_eq!(
            &current_checkpoint.transactions()[..5],
            checkpoint_baseline.transactions()
        );
        assert_eq!(current_checkpoint.transactions().len(), 6);
        let newly_durable = &current_checkpoint.transactions()[5];
        assert!(
            newly_durable
                .transaction()
                .matches_transaction_id(volatile_owner)
        );
        assert_eq!(newly_durable.first_owned_page_position(), Some(11));
        assert_eq!(newly_durable.last_owned_page_position(), Some(11));
        assert_eq!(newly_durable.owned_page_record_count(), 1);
        assert_eq!(newly_durable.state().commit_position(), Some(12));
        assert_eq!(recovered.parts().1.pages().len(), page_count);

        let completeness = recovered.analyze_current_restart_completeness()?;
        let durable_volatile_owner = DurableTransactionIdentityObservation::new(
            volatile_owner.epoch().get(),
            volatile_owner.sequence(),
        )?;
        let durable_uncommitted_owner = DurableTransactionIdentityObservation::new(
            uncommitted_owner.epoch().get(),
            uncommitted_owner.sequence(),
        )?;
        assert_eq!(
            completeness
                .transaction_analysis()
                .durable_frontier()
                .map(LogSequenceNumber::get),
            Some(live_frontier.get())
        );
        assert_eq!(
            completeness
                .pages()
                .iter()
                .map(|entry| entry.page_number().get())
                .collect::<Vec<_>>(),
            [81, 82, 83, 84, 85, 86, 87]
        );
        assert!(completeness.pages()[..3].iter().all(|entry| matches!(
            entry.state(),
            DurableTransactionRestartPageState::StoreCurrent { .. }
        )));
        assert_eq!(
            completeness.pages()[3].state(),
            &DurableTransactionRestartPageState::NoRequiredImage
        );
        assert_eq!(
            completeness.pages()[4].state(),
            &DurableTransactionRestartPageState::StoreMissing {
                required: DurableTransactionRestartRequiredPageImage::Raw { page_position: 10 },
            }
        );
        assert_eq!(
            completeness.pages()[5].state(),
            &DurableTransactionRestartPageState::StoreMissing {
                required: DurableTransactionRestartRequiredPageImage::CommittedTransaction {
                    transaction: durable_volatile_owner,
                    page_position: 11,
                    commit_position: 12,
                },
            }
        );
        assert!(matches!(
            completeness.pages()[6].state(),
            DurableTransactionRestartPageState::StoreCurrent {
                required: DurableTransactionRestartRequiredPageImage::Raw { page_position: 13 },
                stored_position: 13,
            }
        ));
        assert_eq!(
            completeness.replay_start(),
            &DurableTransactionRestartReplayStart::AtPosition {
                position: 9,
                cause: DurableTransactionRestartReplayStartCause::UncommittedTransaction {
                    transaction: durable_uncommitted_owner,
                },
            }
        );
        assert_eq!(recovered.parts().1.pages().len(), page_count);

        let completeness_baseline =
            recovered.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        assert_eq!(completeness_baseline.persistent_log_id(), persistent_log_id);
        assert_eq!(
            completeness_baseline.durable_frontier(),
            Some(live_frontier.get())
        );
        assert_eq!(completeness_baseline.pages(), completeness.pages());
        assert_eq!(
            completeness_baseline.replay_start(),
            completeness.replay_start()
        );
        assert_eq!(
            completeness_baseline.transactions(),
            completeness_baseline.transaction_baseline().transactions()
        );
        assert_eq!(recovered.parts().1.pages().len(), page_count);

        let duplicate_commit = recovered
            .parts()
            .0
            .records()
            .iter()
            .find(|record| record.transaction_id().is_some())
            .cloned()
            .ok_or_else(|| io::Error::other("durable commit is missing"))?;
        let durable_len = recovered.parts().0.durable_len;
        recovered.parts_mut().0.records.push(duplicate_commit);
        recovered.parts_mut().0.durable_len = durable_len + 1;

        let mut absent_source = InMemoryTransactionRestartCheckpointBaselineSource::empty();
        assert_eq!(
            recovered.validate_restart_checkpoint_baseline_from_source(&mut absent_source)?,
            None
        );
        let mut faulted_source =
            InMemoryTransactionRestartCheckpointBaselineSource::seeded(owned_checkpoint.clone());
        faulted_source.arm_fault(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad)?;
        let source_error = recovered
            .validate_restart_checkpoint_baseline_from_source(&mut faulted_source)
            .err()
            .ok_or_else(|| io::Error::other("checkpoint-source fault entered the WAL source"))?;
        let DurableTransactionRestartCheckpointBaselineSourceValidationError::CheckpointSource(
            source_error,
        ) = source_error
        else {
            return Err(io::Error::other("checkpoint-source fault changed boundary").into());
        };
        assert_eq!(
            source_error,
            InMemoryTransactionRestartCheckpointBaselineSourceError::InjectedFault(
                RestartCheckpointBaselineSourceFaultPoint::BeforeLoad
            )
        );
        assert_eq!(faulted_source.slot(), Some(&owned_checkpoint));

        recovered.parts_mut().0.records.pop();
        recovered.parts_mut().0.durable_len = durable_len;

        let first = decoded_checkpoint_entries[0];
        let mut invalid_entries = decoded_checkpoint_entries;
        invalid_entries[0] = DurableTransactionRestartCheckpointBaselineEntryObservation::new(
            first.epoch(),
            first.sequence(),
            first.first_owned_page_position(),
            first.last_owned_page_position(),
            first.owned_page_record_count() + 1,
            first.state(),
        );
        let mut invalid_source = InMemoryTransactionRestartCheckpointBaselineSource::seeded(
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
                checkpoint_baseline.persistent_log_id().get(),
                checkpoint_baseline.durable_frontier(),
                invalid_entries,
            ),
        );
        let invalid = recovered
            .validate_restart_checkpoint_baseline_from_source(&mut invalid_source)
            .err()
            .ok_or_else(|| io::Error::other("invalid memory checkpoint was validated"))?;
        assert!(matches!(
            invalid,
            DurableTransactionRestartCheckpointBaselineSourceValidationError::BaselineValidation(_)
        ));
        assert_eq!(
            recovered.validate_restart_checkpoint_baseline_from_source(&mut faulted_source)?,
            Some(checkpoint_baseline.clone())
        );
        assert_eq!(recovered.parts().1.pages().len(), page_count);
        assert_eq!(
            recovered.restart_analysis().durable_frontier(),
            Some(&expected_restart_frontier)
        );

        let (_log, store, report, analysis) = recovered.into_parts();
        assert_eq!(report.pages().len(), 4);
        assert_eq!(
            analysis.durable_frontier(),
            Some(&expected_restart_frontier)
        );
        assert!(store.page(raw_page_number).is_none());
        assert!(store.page(volatile_page_number).is_none());
        assert!(store.page(live_page_number).is_some());
        Ok(())
    }

    fn decoded_completeness_required_image_observation(
        required: &DurableTransactionRestartRequiredPageImage,
    ) -> DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation {
        match required {
            DurableTransactionRestartRequiredPageImage::Raw { page_position } => {
                DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::Raw {
                    page_position: *page_position,
                }
            }
            DurableTransactionRestartRequiredPageImage::CommittedTransaction {
                transaction,
                page_position,
                commit_position,
            } => {
                DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::CommittedTransaction {
                    epoch: transaction.epoch(),
                    sequence: transaction.sequence(),
                    page_position: *page_position,
                    commit_position: *commit_position,
                }
            }
        }
    }

    fn decoded_completeness_pages(
        baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
    ) -> Vec<DurableTransactionRestartCheckpointCompletenessBaselinePageObservation> {
        baseline
            .pages()
            .iter()
            .map(|entry| {
                let state = match entry.state() {
                    DurableTransactionRestartPageState::NoRequiredImage => {
                        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::NoRequiredImage
                    }
                    DurableTransactionRestartPageState::StoreMissing { .. } => {
                        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreMissing
                    }
                    DurableTransactionRestartPageState::StoreCurrent { .. } => {
                        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent
                    }
                    DurableTransactionRestartPageState::StoreBehind { .. } => {
                        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreBehind
                    }
                };
                let required_image = entry
                    .state()
                    .required_image()
                    .map(decoded_completeness_required_image_observation);
                DurableTransactionRestartCheckpointCompletenessBaselinePageObservation::new(
                    entry.page_number().get(),
                    state,
                    required_image,
                    entry.state().stored_position(),
                )
            })
            .collect()
    }

    fn decoded_completeness_replay_observation(
        replay: &DurableTransactionRestartReplayStart,
    ) -> DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation {
        let kind = match replay {
            DurableTransactionRestartReplayStart::AfterFrontier { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AfterFrontier
            }
            DurableTransactionRestartReplayStart::AtPosition { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition
            }
        };
        let cause = replay.cause().map(|cause| match cause {
            DurableTransactionRestartReplayStartCause::StoreMissing { page_number } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreMissingPage {
                    page_number: page_number.get(),
                }
            }
            DurableTransactionRestartReplayStartCause::StoreBehind { page_number } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreBehindPage {
                    page_number: page_number.get(),
                }
            }
            DurableTransactionRestartReplayStartCause::UncommittedTransaction { transaction } => {
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::UncommittedTransaction {
                    epoch: transaction.epoch(),
                    sequence: transaction.sequence(),
                }
            }
        });
        DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation::new(
            kind,
            replay.frontier(),
            replay.position(),
            cause,
        )
    }

    /// Builds an owned decoded-like completeness observation from an
    /// authoritative baseline, mirroring how a real codec decoder would
    /// reconstruct untrusted fields from persisted bytes.
    fn owned_completeness_checkpoint(
        baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
    ) -> OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation {
        let transaction_entries = baseline
            .transactions()
            .iter()
            .map(|entry| {
                let state = match entry.state().commit_position() {
                    Some(commit_position) => {
                        DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                            commit_position,
                        }
                    }
                    None => {
                        DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted
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
            })
            .collect::<Vec<_>>();
        let transactions = OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
            baseline.persistent_log_id().get(),
            baseline.durable_frontier(),
            transaction_entries,
        );
        OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation::new(
            transactions,
            decoded_completeness_pages(baseline),
            decoded_completeness_replay_observation(baseline.replay_start()),
        )
    }

    #[test]
    fn completeness_checkpoint_validation_succeeds_current_and_safe_stale_but_fails_closed_after_selected_page_advances()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1500)
            .ok_or_else(|| io::Error::other("completeness persistent log id"))?;
        let mut log = InMemoryCommitLog::<1>::with_persistent_lineage_id(persistent_log_id);
        let mut store = InMemoryPageStore::new(&log);
        let mut coordinator = TransactionCoordinator::open(&mut log)?;

        let current_page_number =
            PageNumber::new(201).ok_or_else(|| io::Error::other("current page is zero"))?;
        let committed = coordinator.begin()?;
        let committed_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), current_page_number),
            PageVersion::new(1),
            PageImage::new([0x11])?,
        );
        let (committed, committed_dirty) =
            coordinator.stage_page_write(committed, committed_page, &mut log)?;
        let committed = coordinator.commit(committed, &mut log)?;
        flush_committed_page(&committed, &mut log, &mut store, committed_dirty)?;

        let uncommitted_page_number =
            PageNumber::new(202).ok_or_else(|| io::Error::other("uncommitted page is zero"))?;
        let uncommitted = coordinator.begin()?;
        let uncommitted_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(&log), uncommitted_page_number),
            PageVersion::new(1),
            PageImage::new([0x12])?,
        );
        let (uncommitted, uncommitted_dirty) =
            coordinator.stage_page_write(uncommitted, uncommitted_page, &mut log)?;
        log.flush_through(uncommitted_dirty.required_position())?;
        drop(uncommitted);
        drop(uncommitted_dirty);
        drop(coordinator);

        let mut recovered = UnrecoveredTransactionPageStorage::new(log, store)
            .recover()?
            .analyze_restart()?;

        let baseline =
            recovered.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        assert_eq!(baseline.persistent_log_id(), persistent_log_id);
        assert_eq!(baseline.pages().len(), 2);
        let page_count = recovered.parts().1.pages().len();

        let owned_checkpoint = owned_completeness_checkpoint(&baseline);
        let decoded_checkpoint = owned_checkpoint.as_observation();

        let validated = recovered
            .validate_restart_checkpoint_completeness_baseline_against_current_prefix(
                &decoded_checkpoint,
            )?;
        assert_eq!(validated, baseline);
        assert_eq!(recovered.parts().1.pages().len(), page_count);

        // Append an unrelated later WAL record; the older decoded checkpoint
        // remains exactly valid because the unrelated suffix does not change
        // any selected-prefix page classification.
        let unrelated_page_number =
            PageNumber::new(203).ok_or_else(|| io::Error::other("unrelated page is zero"))?;
        let unrelated_page = UnloggedPage::new(
            PageAddress::new(
                LogDurability::lineage(recovered.parts().0),
                unrelated_page_number,
            ),
            PageVersion::new(1),
            PageImage::new([0x13])?,
        );
        let (log, store) = recovered.parts_mut();
        let unrelated_dirty = ntsql_page::stage_page_write(log, unrelated_page)?;
        ntsql_page::flush_dirty_page(log, store, unrelated_dirty)?;

        let validated_stale = recovered
            .validate_restart_checkpoint_completeness_baseline_against_current_prefix(
                &decoded_checkpoint,
            )?;
        assert_eq!(validated_stale, baseline);
        assert_eq!(recovered.parts().1.pages().len(), page_count + 1);

        // Advance the selected current page beyond the decoded checkpoint's
        // frontier: the current store snapshot no longer yields the exact
        // selected-prefix classification, so validation must fail closed.
        let (log, store) = recovered.parts_mut();
        let mut advancing_coordinator = TransactionCoordinator::open(log)?;
        let advancing_active = advancing_coordinator.begin()?;
        let advancing_page = UnloggedPage::new(
            PageAddress::new(LogDurability::lineage(log), current_page_number),
            PageVersion::new(2),
            PageImage::new([0x21])?,
        );
        let (advancing_active, advancing_dirty) =
            advancing_coordinator.stage_page_write(advancing_active, advancing_page, log)?;
        let advancing_committed = advancing_coordinator.commit(advancing_active, log)?;
        flush_committed_page(&advancing_committed, log, store, advancing_dirty)?;

        let fail_closed = recovered
            .validate_restart_checkpoint_completeness_baseline_against_current_prefix(
                &decoded_checkpoint,
            )
            .err()
            .ok_or_else(|| io::Error::other("advanced selected page unexpectedly validated"))?;
        let DurableTransactionRestartCheckpointCompletenessBaselineValidationError::Evidence(
            evidence,
        ) = fail_closed
        else {
            return Err(io::Error::other("advanced selected page produced a source error").into());
        };
        let DurableTransactionRestartCheckpointCompletenessBaselineValidationEvidenceError::CompletenessEvidence(
            completeness,
        ) = *evidence
        else {
            return Err(io::Error::other("advanced selected page produced the wrong stage").into());
        };
        let DurableTransactionRestartCompletenessError::Evidence(snapshot_evidence) = *completeness
        else {
            return Err(
                io::Error::other("advanced selected page did not fail on store evidence").into(),
            );
        };
        assert!(matches!(
            *snapshot_evidence,
            DurableTransactionRestartCompletenessEvidenceError::SnapshotBeyondFrontier {
                page_number,
                ..
            } if page_number == current_page_number
        ));

        assert!(recovered.restart_analysis().durable_frontier().is_some());
        let (log, store, _, _) = recovered.into_parts();
        let checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            owned_checkpoint,
        );
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Rejected(rejected) =
            selection
        else {
            return Err(io::Error::other(
                "advanced selected page was not rejected before recovery",
            )
            .into());
        };
        assert!(matches!(
            rejected.error(),
            DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError::BaselineValidation(
                validation
            ) if matches!(
                validation.as_ref(),
                DurableTransactionRestartCheckpointCompletenessBaselineValidationError::Evidence(
                    evidence
                ) if matches!(
                    evidence.as_ref(),
                    DurableTransactionRestartCheckpointCompletenessBaselineValidationEvidenceError::CompletenessEvidence(
                        completeness
                    ) if matches!(
                        completeness.as_ref(),
                        DurableTransactionRestartCompletenessError::Evidence(snapshot_evidence)
                            if matches!(
                                snapshot_evidence.as_ref(),
                                DurableTransactionRestartCompletenessEvidenceError::SnapshotBeyondFrontier {
                                    page_number,
                                    ..
                                } if *page_number == current_page_number
                            )
                    )
                )
            )
        ));
        let (uncheckpointed, _) = rejected.continue_with_full_recovery();
        let recovered = uncheckpointed.recover()?.analyze_restart()?;
        assert!(
            recovered
                .recovery_report()
                .pages()
                .iter()
                .all(|page| matches!(
                    page,
                    CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
                        | CommittedTransactionPageRecoveryOutcome::NoCommittedPage { .. }
                ))
        );
        Ok(())
    }

    #[test]
    fn completeness_checkpoint_selection_retains_memory_source_through_decline_and_publication()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1593)
            .ok_or_else(|| io::Error::other("selection persistent log id"))?;
        let mut owner = checkpoint_publication_owner(persistent_log_id, 210)?;
        let baseline =
            owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            owned_completeness_checkpoint(&baseline),
        );
        let original_record_count = owner.parts().0.records.len();
        let original_pages = owner.parts().1.pages().to_vec();
        let (log, store, _, _) = owner.into_parts();

        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
            selection
        else {
            return Err(io::Error::other("current memory checkpoint was not selected").into());
        };
        assert_eq!(selected.persistent_log_id(), persistent_log_id);
        assert_eq!(selected.durable_frontier(), baseline.durable_frontier());
        assert_eq!(selected.transaction_count(), baseline.transactions().len());
        assert_eq!(selected.page_count(), baseline.pages().len());

        let mut recovered = selected.decline_checkpoint().recover()?.analyze_restart()?;
        assert!(recovered.recovery_report().pages().iter().all(|page| {
            matches!(
                page,
                CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
                    | CommittedTransactionPageRecoveryOutcome::NoCommittedPage { .. }
            )
        }));
        let receipt =
            recovered.publish_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        assert_eq!(receipt.persistent_log_id(), persistent_log_id);
        assert_eq!(receipt.page_count(), baseline.pages().len());

        let (log, store, _, _, checkpoint) = recovered.into_parts();
        assert_eq!(log.records.len(), original_record_count);
        assert_eq!(store.pages(), original_pages.as_slice());
        let published = checkpoint
            .slot()
            .ok_or_else(|| io::Error::other("retained memory source did not publish"))?;
        assert_completeness_observation_matches_baseline(published, &baseline);
        Ok(())
    }

    #[test]
    fn selected_completeness_checkpoint_owns_replay_window_through_durable_suffix()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1613)
            .ok_or_else(|| io::Error::other("replay planning persistent log id"))?;
        let mut owner = checkpoint_publication_owner(persistent_log_id, 220)?;
        let baseline =
            owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let checkpoint_frontier = baseline
            .durable_frontier()
            .ok_or_else(|| io::Error::other("replay checkpoint frontier is empty"))?;
        let replay_start = baseline
            .replay_start()
            .position()
            .ok_or_else(|| io::Error::other("replay checkpoint has no inclusive start"))?;
        let checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            owned_completeness_checkpoint(&baseline),
        );

        let suffix_page_number =
            PageNumber::new(222).ok_or_else(|| io::Error::other("suffix page number is zero"))?;
        {
            let (log, _) = owner.parts_mut();
            let lineage = LogDurability::lineage(log).clone();
            let mut coordinator = TransactionCoordinator::open(log)?;
            let active = coordinator.begin()?;
            let suffix_page = UnloggedPage::new(
                PageAddress::new(&lineage, suffix_page_number),
                PageVersion::new(3),
                PageImage::new([0xA3])?,
            );
            let (active, dirty) = coordinator.stage_page_write(active, suffix_page, log)?;
            let committed = coordinator.commit(active, log)?;
            drop((committed, dirty));
        }
        let current_frontier = owner
            .parts()
            .0
            .durable_position()
            .ok_or_else(|| io::Error::other("suffix durable frontier is empty"))?;
        assert!(current_frontier.get() > checkpoint_frontier);
        let durable_record_count = owner.parts().0.durable_records().len();
        let (log, store, _, _) = owner.into_parts();

        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
            selection
        else {
            return Err(io::Error::other("memory replay checkpoint was not selected").into());
        };
        let planned = selected
            .plan_replay_window()
            .map_err(|error| io::Error::other(error.to_string()))?;
        assert_eq!(planned.persistent_log_id(), persistent_log_id);
        assert_eq!(planned.checkpoint_frontier(), Some(checkpoint_frontier));
        assert_eq!(planned.current_frontier(), Some(current_frontier.get()));
        assert_eq!(planned.inclusive_replay_start(), Some(replay_start));
        assert_eq!(planned.replay_record_count(), 3);
        assert_eq!(planned.current_transaction_count(), 3);

        let analyzed = planned.decline_replay_plan().recover()?.analyze_restart()?;
        assert!(analyzed.recovery_report().pages().iter().any(|page| {
            matches!(
                page,
                CommittedTransactionPageRecoveryOutcome::Recovered { .. }
            ) && page.page_number() == suffix_page_number
        }));
        assert_eq!(
            analyzed
                .restart_analysis()
                .durable_frontier()
                .map(LogSequenceNumber::get),
            Some(current_frontier.get())
        );
        let (log, _, _, _, checkpoint) = analyzed.into_parts();
        assert_eq!(log.durable_records().len(), durable_record_count);
        let retained = checkpoint
            .slot()
            .ok_or_else(|| io::Error::other("planned memory checkpoint disappeared"))?;
        assert_completeness_observation_matches_baseline(retained, &baseline);
        Ok(())
    }

    #[test]
    fn completeness_checkpoint_selection_absence_and_source_fault_use_explicit_full_recovery()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x1594)
            .ok_or_else(|| io::Error::other("fallback persistent log id"))?;

        let log = InMemoryCommitLog::<1>::with_persistent_lineage_id(persistent_log_id);
        let store = InMemoryPageStore::new(&log);
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_restart_checkpoint_completeness(
                InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
            );
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Absent(absent) =
            selection
        else {
            return Err(io::Error::other("empty memory checkpoint was not absent").into());
        };
        let mut recovered = absent
            .continue_with_full_recovery()
            .recover()?
            .analyze_restart()?;
        let receipt =
            recovered.publish_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        assert_eq!(receipt.durable_frontier(), None);
        let (_, _, _, _, checkpoint) = recovered.into_parts();
        assert!(checkpoint.slot().is_some());

        let log = InMemoryCommitLog::<1>::with_persistent_lineage_id(persistent_log_id);
        let store = InMemoryPageStore::new(&log);
        let mut checkpoint =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
        checkpoint.arm_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)?;
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Rejected(rejected) =
            selection
        else {
            return Err(io::Error::other("memory source fault was not rejected").into());
        };
        assert!(matches!(
            rejected.error(),
            DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError::CheckpointSource(
                InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::InjectedFault(
                    RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad
                )
            )
        ));
        let (uncheckpointed, rejection) = rejected.continue_with_full_recovery();
        assert!(matches!(
            rejection,
            DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError::CheckpointSource(
                InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::InjectedFault(
                    RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad
                )
            )
        ));
        let mut recovered = uncheckpointed.recover()?.analyze_restart()?;
        let receipt =
            recovered.publish_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        assert_eq!(receipt.persistent_log_id(), persistent_log_id);
        let (_, _, _, _, checkpoint) = recovered.into_parts();
        assert!(checkpoint.slot().is_some());
        assert_eq!(checkpoint.armed_fault(), None);
        Ok(())
    }
}
