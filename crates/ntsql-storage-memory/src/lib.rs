//! Deterministic in-memory persistence adapter for transaction/page WAL tests.

use std::{error::Error, fmt, num::NonZeroU64};

use ntsql_page::{
    DurablePageWalObservation, PageLog, PageNumber, PageRecoveryObservationBytesError, PageStore,
    PageVersion, PageWritePermit, StoredPageSnapshotObservation, UnloggedPage,
};
use ntsql_transaction::{
    DurableCommitLookup, TransactionCommitRecord, TransactionEpochSource, TransactionId,
    TransactionRecoverySource,
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

/// Safely inspectable payload of one physically appended in-memory log record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InMemoryLogRecordKind<const N: usize = 1> {
    /// One transaction commit record.
    TransactionCommit {
        /// The caller-owned transaction identity.
        transaction_id: TransactionId,
    },
    /// One complete page-image write.
    PageWrite(InMemoryPageWriteRecord<N>),
}

impl<const N: usize> InMemoryLogRecordKind<N> {
    /// Returns the transaction identity when this record is a commit.
    #[must_use]
    pub const fn transaction_id(&self) -> Option<TransactionId> {
        match self {
            Self::TransactionCommit { transaction_id } => Some(*transaction_id),
            Self::PageWrite(_) => None,
        }
    }

    /// Returns the full page-image payload when this record is a page write.
    #[must_use]
    pub const fn page_write(&self) -> Option<&InMemoryPageWriteRecord<N>> {
        match self {
            Self::TransactionCommit { .. } => None,
            Self::PageWrite(record) => Some(record),
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
    #[must_use]
    pub const fn transaction_id(&self) -> Option<TransactionId> {
        self.kind.transaction_id()
    }

    /// Returns the full page-image payload when this record is a page write.
    #[must_use]
    pub const fn page_write(&self) -> Option<&InMemoryPageWriteRecord<N>> {
        self.kind.page_write()
    }

    /// Projects a page record into adapter-neutral recovery evidence.
    ///
    /// Callers must select records from a commit log's durable prefix before
    /// treating the result as durable. Transaction records return `Ok(None)`.
    pub fn page_recovery_observation(
        &self,
    ) -> Result<Option<DurablePageWalObservation<N>>, PageRecoveryObservationBytesError<N>> {
        match self.page_write() {
            Some(record) => DurablePageWalObservation::from_bytes(
                record.page_number(),
                record.page_version(),
                *record.bytes(),
                self.position.clone(),
            )
            .map(Some),
            None => Ok(None),
        }
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
        Self::with_lineage(log.lineage().clone())
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

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use ntsql_transaction::{
        CoordinatedCommitError, TransactionCoordinator, TransactionLifecycleStatus,
        TransactionResolutionFailure,
    };
    use ntsql_wal::CommitError;

    use super::*;

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
}
