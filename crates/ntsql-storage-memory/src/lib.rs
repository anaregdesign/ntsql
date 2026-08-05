//! Deterministic in-memory persistence adapter for transaction/WAL tests.

use std::{error::Error, fmt, num::NonZeroU64};

use ntsql_transaction::{
    DurableCommitLookup, TransactionCommitRecord, TransactionEpochSource, TransactionId,
    TransactionRecoverySource,
};
use ntsql_wal::{CommitLog, LogLineage, LogSequenceNumber, PersistentLogId};

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

/// Immutable snapshot of one physically appended transaction commit record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InMemoryLogRecord {
    position: LogSequenceNumber,
    transaction_id: TransactionId,
}

impl InMemoryLogRecord {
    /// Returns the adapter-assigned log position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.position
    }

    /// Returns the transaction identity copied from the caller-owned record.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }
}

/// Failure while executing an in-memory commit-log operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InMemoryCommitLogError {
    /// The armed fault fired at its exact physical-effect boundary.
    InjectedFault(FaultPoint),
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

/// Refusal to silently replace an already armed one-shot fault.
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

/// Inspectable in-memory implementation of the transaction commit-log port.
///
/// This adapter models only repository-authored physical effects. Its durable
/// prefix is not an operating-system flush guarantee or a SQL Server recovery
/// outcome.
#[derive(Debug)]
pub struct InMemoryCommitLog {
    lineage: LogLineage,
    records: Vec<InMemoryLogRecord>,
    durable_len: usize,
    next_epoch: Option<NonZeroU64>,
    next_position: Option<u64>,
    armed_fault: Option<FaultPoint>,
}

impl InMemoryCommitLog {
    /// Creates an empty log whose first assigned position is one.
    #[must_use]
    pub fn new() -> Self {
        Self::with_lineage(LogLineage::new())
    }

    /// Creates an empty log with one adapter-supplied persistent identity.
    #[must_use]
    pub fn with_persistent_lineage(id: PersistentLogId) -> Self {
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
    pub fn records(&self) -> &[InMemoryLogRecord] {
        &self.records
    }

    /// Iterates over exactly the prefix that this model marked durable.
    pub fn durable_records(
        &self,
    ) -> impl DoubleEndedIterator<Item = &InMemoryLogRecord> + ExactSizeIterator {
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

    fn consume_fault(&mut self, point: FaultPoint) -> bool {
        if self.armed_fault == Some(point) {
            self.armed_fault = None;
            true
        } else {
            false
        }
    }
}

impl Default for InMemoryCommitLog {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionEpochSource for InMemoryCommitLog {
    type Error = InMemoryTransactionEpochError;

    fn allocate_transaction_epoch(&mut self) -> Result<(NonZeroU64, LogLineage), Self::Error> {
        let epoch = self
            .next_epoch
            .ok_or(InMemoryTransactionEpochError::EpochSpaceExhausted)?;
        self.next_epoch = epoch.get().checked_add(1).and_then(NonZeroU64::new);
        Ok((epoch, self.lineage.clone()))
    }
}

impl TransactionRecoverySource for InMemoryCommitLog {
    type Error = InMemoryTransactionRecoveryError;

    fn lookup_durable_commit(
        &mut self,
        transaction_id: TransactionId,
    ) -> Result<(LogLineage, DurableCommitLookup), Self::Error> {
        let mut matching_record = None;

        for (index, record) in self.records.iter().enumerate() {
            if record.transaction_id() != transaction_id {
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

impl CommitLog<TransactionCommitRecord> for InMemoryCommitLog {
    type Error = InMemoryCommitLogError;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn append_commit(
        &mut self,
        record: &TransactionCommitRecord,
    ) -> Result<LogSequenceNumber, Self::Error> {
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
            transaction_id: record.transaction_id(),
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
