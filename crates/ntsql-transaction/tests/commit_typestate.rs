use std::{error::Error, fmt, io, num::NonZeroU64};

use ntsql_transaction::{
    CoordinatedCommitError, DurableCommitLookup, TransactionCommitRecord,
    TransactionCommitRejectionReason, TransactionCommitResolution, TransactionCoordinator,
    TransactionEpochSource, TransactionId, TransactionLifecycleStatus, TransactionRecoverySource,
    TransactionResolutionFailure,
};
use ntsql_wal::{CommitError, CommitLog, LogLineage, LogSequenceNumber};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FakeError {
    Append,
    Epoch,
    Flush,
    Recovery,
}

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Append => formatter.write_str("append failure"),
            Self::Epoch => formatter.write_str("epoch failure"),
            Self::Flush => formatter.write_str("flush failure"),
            Self::Recovery => formatter.write_str("recovery lookup failure"),
        }
    }
}

impl Error for FakeError {}

struct FakeEpochSource {
    lineage: LogLineage,
    next_epoch: Option<NonZeroU64>,
}

impl FakeEpochSource {
    fn new(lineage: LogLineage) -> Self {
        Self {
            lineage,
            next_epoch: Some(NonZeroU64::MIN),
        }
    }
}

impl TransactionEpochSource for FakeEpochSource {
    type Error = FakeError;

    fn allocate_transaction_epoch(&mut self) -> Result<(NonZeroU64, LogLineage), Self::Error> {
        let epoch = self.next_epoch.ok_or(FakeError::Epoch)?;
        self.next_epoch = epoch.get().checked_add(1).and_then(NonZeroU64::new);
        Ok((epoch, self.lineage.clone()))
    }
}

struct FailingEpochSource;

impl TransactionEpochSource for FailingEpochSource {
    type Error = FakeError;

    fn allocate_transaction_epoch(&mut self) -> Result<(NonZeroU64, LogLineage), Self::Error> {
        Err(FakeError::Epoch)
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Call {
    Append(TransactionId),
    Flush(LogSequenceNumber),
}

struct FakeCommitLog {
    lineage: LogLineage,
    position: LogSequenceNumber,
    append_fails: bool,
    flush_fails: bool,
    calls: Vec<Call>,
}

impl FakeCommitLog {
    fn succeeds_at(position: u64, lineage: LogLineage) -> Self {
        Self {
            lineage,
            position: LogSequenceNumber::new(position),
            append_fails: false,
            flush_fails: false,
            calls: Vec::new(),
        }
    }
}

impl CommitLog<TransactionCommitRecord> for FakeCommitLog {
    type Error = FakeError;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn append_commit(
        &mut self,
        record: &TransactionCommitRecord,
    ) -> Result<LogSequenceNumber, Self::Error> {
        self.calls.push(Call::Append(record.transaction_id()));
        if self.append_fails {
            Err(FakeError::Append)
        } else {
            Ok(self.position)
        }
    }

    fn flush_through(&mut self, position: LogSequenceNumber) -> Result<(), Self::Error> {
        self.calls.push(Call::Flush(position));
        if self.flush_fails {
            Err(FakeError::Flush)
        } else {
            Ok(())
        }
    }
}

struct FakeRecoverySource {
    lineage: LogLineage,
    result: Result<DurableCommitLookup, FakeError>,
    calls: Vec<TransactionId>,
}

impl FakeRecoverySource {
    fn new(lineage: LogLineage, result: Result<DurableCommitLookup, FakeError>) -> Self {
        Self {
            lineage,
            result,
            calls: Vec::new(),
        }
    }
}

impl TransactionRecoverySource for FakeRecoverySource {
    type Error = FakeError;

    fn lookup_durable_commit(
        &mut self,
        transaction_id: TransactionId,
    ) -> Result<(LogLineage, DurableCommitLookup), Self::Error> {
        self.calls.push(transaction_id);
        self.result.map(|lookup| (self.lineage.clone(), lookup))
    }
}

#[test]
fn durable_commit_consumes_active_state_and_preserves_identity() -> Result<(), Box<dyn Error>> {
    let lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(lineage.clone());
    let mut coordinator = TransactionCoordinator::open(&mut epochs)?;
    let mut log = FakeCommitLog::succeeds_at(41, lineage);
    let transaction = coordinator.begin()?;
    let transaction_id = transaction.transaction_id();

    let committed = coordinator.commit(transaction, &mut log)?;

    assert_eq!(committed.transaction_id(), transaction_id);
    assert_eq!(committed.log_position(), LogSequenceNumber::new(41));
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Committed)
    );
    assert_eq!(
        log.calls,
        [
            Call::Append(transaction_id),
            Call::Flush(LogSequenceNumber::new(41)),
        ]
    );
    Ok(())
}

#[test]
fn append_failure_consumes_active_state_into_indeterminate() -> Result<(), Box<dyn Error>> {
    let lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(lineage.clone());
    let mut coordinator = TransactionCoordinator::open(&mut epochs)?;
    let mut log = FakeCommitLog {
        append_fails: true,
        ..FakeCommitLog::succeeds_at(41, lineage)
    };
    let transaction = coordinator.begin()?;
    let transaction_id = transaction.transaction_id();

    let error = coordinator
        .commit(transaction, &mut log)
        .err()
        .ok_or_else(|| invalid_data("append failure unexpectedly committed"))?;
    let CoordinatedCommitError::Indeterminate(error) = error else {
        return Err(invalid_data("append failure was rejected before WAL").into());
    };
    let (indeterminate, cause) = error.into_parts();

    assert_eq!(indeterminate.transaction_id(), transaction_id);
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );
    assert_eq!(
        cause,
        CommitError::Append {
            source: FakeError::Append,
        }
    );
    assert_eq!(log.calls, [Call::Append(transaction_id)]);
    Ok(())
}

#[test]
fn flush_failure_consumes_active_state_into_indeterminate() -> Result<(), Box<dyn Error>> {
    let lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(lineage.clone());
    let mut coordinator = TransactionCoordinator::open(&mut epochs)?;
    let mut log = FakeCommitLog {
        flush_fails: true,
        ..FakeCommitLog::succeeds_at(83, lineage)
    };
    let transaction = coordinator.begin()?;
    let transaction_id = transaction.transaction_id();

    let error = coordinator
        .commit(transaction, &mut log)
        .err()
        .ok_or_else(|| invalid_data("flush failure unexpectedly committed"))?;
    let CoordinatedCommitError::Indeterminate(error) = error else {
        return Err(invalid_data("flush failure was rejected before WAL").into());
    };
    let (indeterminate, cause) = error.into_parts();

    assert_eq!(indeterminate.transaction_id(), transaction_id);
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );
    assert_eq!(
        cause,
        CommitError::Flush {
            position: LogSequenceNumber::new(83),
            source: FakeError::Flush,
        }
    );
    assert_eq!(
        log.calls,
        [
            Call::Append(transaction_id),
            Call::Flush(LogSequenceNumber::new(83)),
        ]
    );
    Ok(())
}

#[test]
fn coordinator_issues_distinct_active_transactions() -> Result<(), Box<dyn Error>> {
    let mut epochs = FakeEpochSource::new(LogLineage::new());
    let mut coordinator = TransactionCoordinator::open(&mut epochs)?;

    let first = coordinator.begin()?;
    let second = coordinator.begin()?;

    assert_ne!(first.transaction_id(), second.transaction_id());
    assert_eq!(first.transaction_id().epoch(), coordinator.epoch());
    assert_eq!(second.transaction_id().epoch(), coordinator.epoch());
    assert_eq!(first.transaction_id().sequence(), 1);
    assert_eq!(second.transaction_id().sequence(), 2);
    assert_eq!(
        coordinator.status(first.transaction_id()),
        Some(TransactionLifecycleStatus::Active)
    );
    assert_eq!(
        coordinator.status(second.transaction_id()),
        Some(TransactionLifecycleStatus::Active)
    );
    Ok(())
}

#[test]
fn foreign_coordinator_rejects_before_wal_and_returns_token() -> Result<(), Box<dyn Error>> {
    let lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(lineage.clone());
    let mut owner = TransactionCoordinator::open(&mut epochs)?;
    let mut foreign = TransactionCoordinator::open(&mut epochs)?;
    let transaction = owner.begin()?;
    let transaction_id = transaction.transaction_id();
    let mut log = FakeCommitLog::succeeds_at(41, lineage);

    assert!(owner.owns(&transaction));
    assert!(!foreign.owns(&transaction));
    let error = foreign
        .commit(transaction, &mut log)
        .err()
        .ok_or_else(|| invalid_data("foreign coordinator unexpectedly committed"))?;
    let CoordinatedCommitError::Rejected(rejection) = error else {
        return Err(invalid_data("foreign coordinator reached the WAL").into());
    };

    assert_eq!(
        rejection.reason(),
        TransactionCommitRejectionReason::ForeignCoordinator
    );
    assert!(log.calls.is_empty());

    let transaction = rejection.into_transaction();
    let committed = owner.commit(transaction, &mut log)?;

    assert_eq!(committed.transaction_id(), transaction_id);
    assert_eq!(
        owner.status(transaction_id),
        Some(TransactionLifecycleStatus::Committed)
    );
    Ok(())
}

#[test]
fn foreign_log_lineage_rejects_before_wal_and_returns_token() -> Result<(), Box<dyn Error>> {
    let owner_lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(owner_lineage.clone());
    let mut coordinator = TransactionCoordinator::open(&mut epochs)?;
    let transaction = coordinator.begin()?;
    let transaction_id = transaction.transaction_id();
    let mut foreign_log = FakeCommitLog::succeeds_at(41, LogLineage::new());

    let error = coordinator
        .commit(transaction, &mut foreign_log)
        .err()
        .ok_or_else(|| invalid_data("foreign log unexpectedly committed"))?;
    let CoordinatedCommitError::Rejected(rejection) = error else {
        return Err(invalid_data("foreign log reached the WAL").into());
    };

    assert_eq!(
        rejection.reason(),
        TransactionCommitRejectionReason::ForeignLogLineage
    );
    assert!(foreign_log.calls.is_empty());
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Active)
    );

    let mut owner_log = FakeCommitLog::succeeds_at(41, owner_lineage);
    let committed = coordinator.commit(rejection.into_transaction(), &mut owner_log)?;
    assert_eq!(committed.transaction_id(), transaction_id);
    Ok(())
}

#[test]
fn coordinator_open_preserves_epoch_source_failure() {
    let mut source = FailingEpochSource;

    let error = TransactionCoordinator::open(&mut source).err();

    assert_eq!(error, Some(FakeError::Epoch));
}

#[test]
fn durable_lookup_resolves_indeterminate_commit_and_updates_lifecycle() -> Result<(), Box<dyn Error>>
{
    let lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(lineage.clone());
    let mut coordinator = TransactionCoordinator::open(&mut epochs)?;
    let mut log = FakeCommitLog {
        flush_fails: true,
        ..FakeCommitLog::succeeds_at(83, lineage.clone())
    };
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();
    let error = coordinator
        .commit(active, &mut log)
        .err()
        .ok_or_else(|| invalid_data("flush failure unexpectedly committed"))?;
    let CoordinatedCommitError::Indeterminate(error) = error else {
        return Err(invalid_data("flush failure was rejected before WAL").into());
    };
    let (indeterminate, _) = error.into_parts();
    let mut recovery = FakeRecoverySource::new(
        lineage,
        Ok(DurableCommitLookup::Found {
            position: LogSequenceNumber::new(83),
        }),
    );

    let resolved = coordinator.resolve(indeterminate, &mut recovery)?;

    let TransactionCommitResolution::Committed(committed) = resolved else {
        return Err(invalid_data("durable record resolved as absent").into());
    };
    assert_eq!(committed.transaction_id(), transaction_id);
    assert_eq!(committed.log_position(), LogSequenceNumber::new(83));
    assert_eq!(recovery.calls, [transaction_id]);
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Committed)
    );
    Ok(())
}

#[test]
fn recovery_source_failure_retains_indeterminate_token() -> Result<(), Box<dyn Error>> {
    let lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(lineage.clone());
    let mut coordinator = TransactionCoordinator::open(&mut epochs)?;
    let mut log = FakeCommitLog {
        append_fails: true,
        ..FakeCommitLog::succeeds_at(41, lineage.clone())
    };
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();
    let error = coordinator
        .commit(active, &mut log)
        .err()
        .ok_or_else(|| invalid_data("append failure unexpectedly committed"))?;
    let CoordinatedCommitError::Indeterminate(error) = error else {
        return Err(invalid_data("append failure was rejected before WAL").into());
    };
    let (indeterminate, _) = error.into_parts();
    let mut recovery = FakeRecoverySource::new(lineage, Err(FakeError::Recovery));

    let error = coordinator
        .resolve(indeterminate, &mut recovery)
        .err()
        .ok_or_else(|| invalid_data("failed lookup unexpectedly resolved"))?;

    assert_eq!(
        error.failure(),
        &TransactionResolutionFailure::Source(FakeError::Recovery)
    );
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );

    recovery.result = Ok(DurableCommitLookup::Absent);
    let resolved = coordinator.resolve(error.into_transaction(), &mut recovery)?;
    let TransactionCommitResolution::NoDurableCommitRecord(without_record) = resolved else {
        return Err(invalid_data("absent record resolved as committed").into());
    };
    assert_eq!(without_record.transaction_id(), transaction_id);
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::NoDurableCommitRecord)
    );
    Ok(())
}

#[test]
fn foreign_coordinator_rejects_resolution_before_lookup_and_returns_token()
-> Result<(), Box<dyn Error>> {
    let lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(lineage.clone());
    let mut owner = TransactionCoordinator::open(&mut epochs)?;
    let mut foreign = TransactionCoordinator::open(&mut epochs)?;
    let mut log = FakeCommitLog {
        append_fails: true,
        ..FakeCommitLog::succeeds_at(41, lineage.clone())
    };
    let active = owner.begin()?;
    let transaction_id = active.transaction_id();
    let error = owner
        .commit(active, &mut log)
        .err()
        .ok_or_else(|| invalid_data("append failure unexpectedly committed"))?;
    let CoordinatedCommitError::Indeterminate(error) = error else {
        return Err(invalid_data("append failure was rejected before WAL").into());
    };
    let (indeterminate, _) = error.into_parts();
    let mut recovery = FakeRecoverySource::new(lineage, Ok(DurableCommitLookup::Absent));

    let error = foreign
        .resolve(indeterminate, &mut recovery)
        .err()
        .ok_or_else(|| invalid_data("foreign coordinator resolved transaction"))?;

    assert_eq!(
        error.failure(),
        &TransactionResolutionFailure::ForeignCoordinator
    );
    assert!(recovery.calls.is_empty());
    let resolved = owner.resolve(error.into_transaction(), &mut recovery)?;
    assert_eq!(resolved.transaction_id(), transaction_id);
    Ok(())
}

#[test]
fn foreign_recovery_lineage_retains_indeterminate_token() -> Result<(), Box<dyn Error>> {
    let lineage = LogLineage::new();
    let mut epochs = FakeEpochSource::new(lineage.clone());
    let mut coordinator = TransactionCoordinator::open(&mut epochs)?;
    let mut log = FakeCommitLog {
        append_fails: true,
        ..FakeCommitLog::succeeds_at(41, lineage.clone())
    };
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();
    let error = coordinator
        .commit(active, &mut log)
        .err()
        .ok_or_else(|| invalid_data("append failure unexpectedly committed"))?;
    let CoordinatedCommitError::Indeterminate(error) = error else {
        return Err(invalid_data("append failure was rejected before WAL").into());
    };
    let (indeterminate, _) = error.into_parts();
    let mut recovery = FakeRecoverySource::new(LogLineage::new(), Ok(DurableCommitLookup::Absent));

    let error = coordinator
        .resolve(indeterminate, &mut recovery)
        .err()
        .ok_or_else(|| invalid_data("foreign lineage resolved transaction"))?;

    assert_eq!(
        error.failure(),
        &TransactionResolutionFailure::ForeignLogLineage
    );
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );

    recovery.lineage = lineage;
    let resolved = coordinator.resolve(error.into_transaction(), &mut recovery)?;
    assert_eq!(resolved.transaction_id(), transaction_id);
    Ok(())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
