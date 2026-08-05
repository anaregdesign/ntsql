use std::{error::Error, io};

use ntsql_storage_memory::{
    FaultPoint, InMemoryCommitLog, InMemoryCommitLogError, InMemoryLogReopenError,
    InMemoryTransactionRecoveryError,
};
use ntsql_transaction::{
    CoordinatedCommitError, IndeterminateTransaction, TransactionCommitRejectionReason,
    TransactionCommitResolution, TransactionCoordinator, TransactionLifecycleStatus,
    TransactionResolutionFailure,
};
use ntsql_wal::{CommitError, CommitLog, LogSequenceNumber, PersistentLogId};

#[test]
fn successful_commit_appends_and_flushes_exact_record() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();

    let committed = coordinator.commit(active, &mut log)?;

    assert_eq!(committed.transaction_id(), transaction_id);
    assert_eq!(committed.log_position(), &position(&log, 1));
    assert_eq!(log.records().len(), 1);
    assert_eq!(
        log.records()
            .first()
            .ok_or_else(|| io::Error::other("appended record is missing"))?
            .transaction_id(),
        transaction_id
    );
    assert_eq!(
        log.durable_records().cloned().collect::<Vec<_>>(),
        log.records()
    );
    assert_eq!(log.durable_position(), Some(position(&log, 1)));
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Committed)
    );
    Ok(())
}

#[test]
fn before_append_fault_has_no_physical_effect() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::BeforeAppend)?;
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();

    let cause = commit_cause(coordinator.commit(active, &mut log))?;

    assert_eq!(
        cause,
        CommitError::Append {
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeAppend),
        }
    );
    assert!(log.records().is_empty());
    assert_eq!(log.durable_records().len(), 0);
    assert_eq!(log.armed_fault(), None);
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );
    Ok(())
}

#[test]
fn after_append_fault_leaves_volatile_record_and_later_flush_covers_it()
-> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::AfterAppend)?;
    let first = coordinator.begin()?;
    let first_id = first.transaction_id();

    let first_cause = commit_cause(coordinator.commit(first, &mut log))?;

    assert_eq!(
        first_cause,
        CommitError::Append {
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::AfterAppend),
        }
    );
    assert_eq!(log.records().len(), 1);
    assert_eq!(
        log.records()
            .first()
            .ok_or_else(|| io::Error::other("volatile record is missing"))?
            .transaction_id(),
        first_id
    );
    assert_eq!(log.durable_records().len(), 0);

    let second = coordinator.begin()?;
    let second_id = second.transaction_id();
    let committed = coordinator.commit(second, &mut log)?;

    assert_eq!(committed.log_position(), &position(&log, 2));
    assert_eq!(log.durable_records().len(), 2);
    assert_eq!(
        coordinator.status(first_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );
    assert_eq!(
        coordinator.status(second_id),
        Some(TransactionLifecycleStatus::Committed)
    );
    Ok(())
}

#[test]
fn before_flush_fault_stays_armed_through_append_and_restart_drops_suffix()
-> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let first = coordinator.begin()?;
    let first_commit = coordinator.commit(first, &mut log)?;
    assert_eq!(first_commit.log_position(), &position(&log, 1));

    log.arm_fault(FaultPoint::BeforeFlush)?;
    let second = coordinator.begin()?;
    let second_id = second.transaction_id();
    let second_cause = commit_cause(coordinator.commit(second, &mut log))?;

    assert_eq!(
        second_cause,
        CommitError::Flush {
            position: position(&log, 2),
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeFlush),
        }
    );
    assert_eq!(log.records().len(), 2);
    assert_eq!(log.durable_records().len(), 1);
    assert_eq!(log.armed_fault(), None);
    assert_eq!(
        coordinator.status(second_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );

    let mut restarted = log.restart();
    assert_eq!(restarted.records().len(), 1);
    assert_eq!(restarted.durable_position(), Some(position(&restarted, 1)));

    let third = coordinator.begin()?;
    let third_commit = coordinator.commit(third, &mut restarted)?;
    assert_eq!(third_commit.log_position(), &position(&restarted, 3));
    assert_eq!(restarted.records().len(), 2);
    assert_eq!(restarted.durable_records().len(), 2);
    Ok(())
}

#[test]
fn after_flush_fault_is_durable_but_transaction_remains_indeterminate() -> Result<(), Box<dyn Error>>
{
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::AfterFlush)?;
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();

    let cause = commit_cause(coordinator.commit(active, &mut log))?;

    assert_eq!(
        cause,
        CommitError::Flush {
            position: position(&log, 1),
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::AfterFlush),
        }
    );
    assert_eq!(log.records().len(), 1);
    assert_eq!(log.durable_records().len(), 1);
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );

    let restarted = log.restart();
    assert_eq!(restarted.records().len(), 1);
    assert_eq!(restarted.durable_records().len(), 1);
    Ok(())
}

#[test]
fn invalid_and_idempotent_flushes_preserve_unreached_faults() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    log.arm_fault(FaultPoint::BeforeFlush)?;

    let error = flush_numeric(&mut log, 0)
        .err()
        .ok_or_else(|| io::Error::other("unknown position was accepted"))?;
    assert_eq!(
        error,
        InMemoryCommitLogError::UnknownFlushPosition(position(&log, 0))
    );
    assert_eq!(log.armed_fault(), Some(FaultPoint::BeforeFlush));

    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let cause = commit_cause(coordinator.commit(active, &mut log))?;
    assert_eq!(
        cause,
        CommitError::Flush {
            position: position(&log, 1),
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeFlush),
        }
    );

    flush_numeric(&mut log, 1)?;
    log.arm_fault(FaultPoint::BeforeFlush)?;
    flush_numeric(&mut log, 1)?;
    assert_eq!(log.armed_fault(), Some(FaultPoint::BeforeFlush));

    let second = coordinator.begin()?;
    let second_cause = commit_cause(coordinator.commit(second, &mut log))?;
    assert_eq!(
        second_cause,
        CommitError::Flush {
            position: position(&log, 2),
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeFlush),
        }
    );
    flush_numeric(&mut log, 2)?;

    log.arm_fault(FaultPoint::AfterFlush)?;
    flush_numeric(&mut log, 2)?;
    assert_eq!(log.armed_fault(), Some(FaultPoint::AfterFlush));

    let third = coordinator.begin()?;
    let third_cause = commit_cause(coordinator.commit(third, &mut log))?;
    assert_eq!(
        third_cause,
        CommitError::Flush {
            position: position(&log, 3),
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::AfterFlush),
        }
    );
    assert_eq!(log.durable_records().len(), 3);
    Ok(())
}

#[test]
fn foreign_numeric_alias_is_rejected_before_fault_or_durability() -> Result<(), Box<dyn Error>> {
    let mut owner_log = InMemoryCommitLog::new();
    let mut owner = TransactionCoordinator::open(&mut owner_log)?;
    let owner_active = owner.begin()?;
    let owner_position = owner
        .commit(owner_active, &mut owner_log)?
        .log_position()
        .clone();

    let mut target_log = InMemoryCommitLog::new();
    let mut target = TransactionCoordinator::open(&mut target_log)?;
    target_log.arm_fault(FaultPoint::AfterAppend)?;
    let target_active = target.begin()?;
    let _ = indeterminate_parts(target.commit(target_active, &mut target_log))?;
    let target_position = target_log
        .records()
        .first()
        .ok_or_else(|| io::Error::other("target record is missing"))?
        .position()
        .clone();
    assert_eq!(owner_position.get(), target_position.get());
    assert_ne!(owner_position, target_position);

    target_log.arm_fault(FaultPoint::BeforeFlush)?;
    let error = target_log
        .flush_through(&owner_position)
        .err()
        .ok_or_else(|| io::Error::other("foreign numeric alias was accepted"))?;

    assert_eq!(
        error,
        InMemoryCommitLogError::ForeignFlushPosition(owner_position)
    );
    assert_eq!(target_log.armed_fault(), Some(FaultPoint::BeforeFlush));
    assert_eq!(target_log.durable_position(), None);

    let local_error = target_log
        .flush_through(&target_position)
        .err()
        .ok_or_else(|| io::Error::other("preserved local fault did not fire"))?;
    assert_eq!(
        local_error,
        InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeFlush)
    );
    Ok(())
}

#[test]
fn one_log_lineage_distinguishes_same_sequence_from_two_coordinators() -> Result<(), Box<dyn Error>>
{
    let mut log = InMemoryCommitLog::new();
    let mut first_coordinator = TransactionCoordinator::open(&mut log)?;
    let mut second_coordinator = TransactionCoordinator::open(&mut log)?;
    let first = first_coordinator.begin()?;
    let second = second_coordinator.begin()?;
    let first_id = first.transaction_id();
    let second_id = second.transaction_id();

    first_coordinator.commit(first, &mut log)?;
    second_coordinator.commit(second, &mut log)?;

    assert_eq!(first_id.sequence(), 1);
    assert_eq!(second_id.sequence(), 1);
    assert_ne!(first_id.epoch(), second_id.epoch());
    assert_ne!(first_id, second_id);
    assert_eq!(
        log.records()
            .iter()
            .map(|record| record.transaction_id())
            .collect::<Vec<_>>(),
        [first_id, second_id]
    );
    Ok(())
}

#[test]
fn independent_log_lineage_rejects_before_append_and_returns_token() -> Result<(), Box<dyn Error>> {
    let mut owner_log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut owner_log)?;
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();
    let mut foreign_log = InMemoryCommitLog::new();

    let error = coordinator
        .commit(active, &mut foreign_log)
        .err()
        .ok_or_else(|| io::Error::other("foreign log unexpectedly committed"))?;
    let CoordinatedCommitError::Rejected(rejection) = error else {
        return Err(io::Error::other("foreign log reached append").into());
    };

    assert_eq!(
        rejection.reason(),
        TransactionCommitRejectionReason::ForeignLogLineage
    );
    assert!(foreign_log.records().is_empty());

    let committed = coordinator.commit(rejection.into_transaction(), &mut owner_log)?;
    assert_eq!(committed.transaction_id(), transaction_id);
    Ok(())
}

#[test]
fn restart_preserves_log_lineage_and_advances_coordinator_epoch() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut first_coordinator = TransactionCoordinator::open(&mut log)?;
    let first = first_coordinator.begin()?;
    let first_id = first.transaction_id();

    let mut restarted = log.restart();
    let mut second_coordinator = TransactionCoordinator::open(&mut restarted)?;
    let second = second_coordinator.begin()?;
    let second_id = second.transaction_id();

    assert_eq!(first_id.epoch().get(), 1);
    assert_eq!(second_id.epoch().get(), 2);
    assert_ne!(first_id, second_id);
    first_coordinator.commit(first, &mut restarted)?;
    second_coordinator.commit(second, &mut restarted)?;
    assert_eq!(restarted.durable_records().len(), 2);
    Ok(())
}

#[test]
fn persistent_reopen_reconstructs_lineage_and_preserves_high_water_marks()
-> Result<(), Box<dyn Error>> {
    let persistent_id = PersistentLogId::new(1)
        .ok_or_else(|| io::Error::other("nonzero persistent ID was rejected"))?;
    let mut log = InMemoryCommitLog::with_persistent_lineage(persistent_id);
    let mut first_coordinator = TransactionCoordinator::open(&mut log)?;

    let first = first_coordinator.begin()?;
    let first_position = first_coordinator
        .commit(first, &mut log)?
        .log_position()
        .clone();

    log.arm_fault(FaultPoint::AfterAppend)?;
    let second = first_coordinator.begin()?;
    let _ = indeterminate_parts(first_coordinator.commit(second, &mut log))?;
    let third = first_coordinator.begin()?;
    log.arm_fault(FaultPoint::BeforeFlush)?;

    log.reopen()?;

    assert_eq!(log.records().len(), 1);
    assert_eq!(log.durable_records().len(), 1);
    assert_eq!(log.armed_fault(), None);
    assert_eq!(first_position, log.lineage().position(1));

    let third_commit = first_coordinator.commit(third, &mut log)?;
    assert_eq!(third_commit.log_position().get(), 3);
    let second_coordinator = TransactionCoordinator::open(&mut log)?;
    assert_eq!(second_coordinator.epoch().get(), 2);
    Ok(())
}

#[test]
fn ephemeral_reopen_rejects_before_discarding_state() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::AfterAppend)?;
    let active = coordinator.begin()?;
    let _ = indeterminate_parts(coordinator.commit(active, &mut log))?;
    log.arm_fault(FaultPoint::BeforeFlush)?;

    let error = log
        .reopen()
        .err()
        .ok_or_else(|| io::Error::other("ephemeral lineage unexpectedly reopened"))?;

    assert_eq!(error, InMemoryLogReopenError::EphemeralLineage);
    assert_eq!(log.records().len(), 1);
    assert_eq!(log.durable_records().len(), 0);
    assert_eq!(log.armed_fault(), Some(FaultPoint::BeforeFlush));
    Ok(())
}

#[test]
fn before_append_failure_resolves_as_no_durable_record() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::BeforeAppend)?;
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();
    let (indeterminate, _) = indeterminate_parts(coordinator.commit(active, &mut log))?;

    let resolution = coordinator.resolve(indeterminate, &mut log)?;

    let TransactionCommitResolution::NoDurableCommitRecord(without_record) = resolution else {
        return Err(io::Error::other("absent record resolved as committed").into());
    };
    assert_eq!(without_record.transaction_id(), transaction_id);
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::NoDurableCommitRecord)
    );
    Ok(())
}

#[test]
fn after_flush_failure_resolves_as_committed_at_exact_position() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::AfterFlush)?;
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();
    let (indeterminate, _) = indeterminate_parts(coordinator.commit(active, &mut log))?;

    let resolution = coordinator.resolve(indeterminate, &mut log)?;

    let TransactionCommitResolution::Committed(committed) = resolution else {
        return Err(io::Error::other("durable record resolved as absent").into());
    };
    assert_eq!(committed.transaction_id(), transaction_id);
    assert_eq!(committed.log_position(), &position(&log, 1));
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Committed)
    );
    Ok(())
}

#[test]
fn volatile_record_retains_token_until_restart_discards_it() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::AfterAppend)?;
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();
    let (indeterminate, _) = indeterminate_parts(coordinator.commit(active, &mut log))?;

    let error = coordinator
        .resolve(indeterminate, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("volatile record produced a terminal resolution"))?;

    assert_eq!(
        error.failure(),
        &TransactionResolutionFailure::Source(
            InMemoryTransactionRecoveryError::VolatileCommitRecord(transaction_id)
        )
    );
    assert_eq!(
        coordinator.status(transaction_id),
        Some(TransactionLifecycleStatus::Indeterminate)
    );

    let mut restarted = log.restart();
    let resolution = coordinator.resolve(error.into_transaction(), &mut restarted)?;
    assert!(matches!(
        resolution,
        TransactionCommitResolution::NoDurableCommitRecord(_)
    ));
    Ok(())
}

#[test]
fn later_flush_resolves_an_earlier_volatile_record_as_committed() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::AfterAppend)?;
    let first = coordinator.begin()?;
    let first_id = first.transaction_id();
    let (indeterminate, _) = indeterminate_parts(coordinator.commit(first, &mut log))?;
    let error = coordinator
        .resolve(indeterminate, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("volatile record produced a terminal resolution"))?;

    let second = coordinator.begin()?;
    coordinator.commit(second, &mut log)?;

    let resolution = coordinator.resolve(error.into_transaction(), &mut log)?;
    let TransactionCommitResolution::Committed(committed) = resolution else {
        return Err(io::Error::other("flushed record resolved as absent").into());
    };
    assert_eq!(committed.transaction_id(), first_id);
    assert_eq!(committed.log_position(), &position(&log, 1));
    Ok(())
}

#[test]
fn complete_identity_prevents_equal_sequences_from_aliasing() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut first_coordinator = TransactionCoordinator::open(&mut log)?;
    let mut second_coordinator = TransactionCoordinator::open(&mut log)?;

    log.arm_fault(FaultPoint::BeforeAppend)?;
    let first = first_coordinator.begin()?;
    let first_id = first.transaction_id();
    let (indeterminate, _) = indeterminate_parts(first_coordinator.commit(first, &mut log))?;

    let second = second_coordinator.begin()?;
    let second_id = second.transaction_id();
    second_coordinator.commit(second, &mut log)?;

    assert_eq!(first_id.sequence(), second_id.sequence());
    assert_ne!(first_id.epoch(), second_id.epoch());
    let resolution = first_coordinator.resolve(indeterminate, &mut log)?;
    assert!(matches!(
        resolution,
        TransactionCommitResolution::NoDurableCommitRecord(_)
    ));
    Ok(())
}

#[test]
fn arming_a_fault_never_silently_replaces_one() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    log.arm_fault(FaultPoint::AfterAppend)?;

    let error = log
        .arm_fault(FaultPoint::BeforeFlush)
        .err()
        .ok_or_else(|| io::Error::other("armed fault was silently replaced"))?;

    assert_eq!(error.armed(), FaultPoint::AfterAppend);
    assert_eq!(error.requested(), FaultPoint::BeforeFlush);
    assert_eq!(log.armed_fault(), Some(FaultPoint::AfterAppend));
    Ok(())
}

fn commit_cause(
    result: Result<
        ntsql_transaction::CommittedTransaction,
        CoordinatedCommitError<InMemoryCommitLogError>,
    >,
) -> Result<CommitError<InMemoryCommitLogError>, Box<dyn Error>> {
    Ok(indeterminate_parts(result)?.1)
}

fn indeterminate_parts(
    result: Result<
        ntsql_transaction::CommittedTransaction,
        CoordinatedCommitError<InMemoryCommitLogError>,
    >,
) -> Result<
    (
        IndeterminateTransaction,
        CommitError<InMemoryCommitLogError>,
    ),
    Box<dyn Error>,
> {
    let error = result
        .err()
        .ok_or_else(|| io::Error::other("faulted commit unexpectedly succeeded"))?;
    match error {
        CoordinatedCommitError::Indeterminate(error) => Ok(error.into_parts()),
        CoordinatedCommitError::Rejected(_) => {
            Err(io::Error::other("faulted commit was rejected before WAL").into())
        }
    }
}

fn position(log: &InMemoryCommitLog, value: u64) -> LogSequenceNumber {
    log.lineage().position(value)
}

fn flush_numeric(log: &mut InMemoryCommitLog, value: u64) -> Result<(), InMemoryCommitLogError> {
    let position = log.lineage().position(value);
    log.flush_through(&position)
}
