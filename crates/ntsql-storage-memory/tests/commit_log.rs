use std::{error::Error, io};

use ntsql_storage_memory::{FaultPoint, InMemoryCommitLog, InMemoryCommitLogError};
use ntsql_transaction::{
    CoordinatedCommitError, TransactionCommitRejectionReason, TransactionCoordinator,
    TransactionLifecycleStatus,
};
use ntsql_wal::{CommitError, CommitLog, LogSequenceNumber};

#[test]
fn successful_commit_appends_and_flushes_exact_record() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::new();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let transaction_id = active.transaction_id();

    let committed = coordinator.commit(active, &mut log)?;

    assert_eq!(committed.transaction_id(), transaction_id);
    assert_eq!(committed.log_position(), LogSequenceNumber::new(1));
    assert_eq!(log.records().len(), 1);
    assert_eq!(
        log.records()
            .first()
            .ok_or_else(|| io::Error::other("appended record is missing"))?
            .transaction_id(),
        transaction_id
    );
    assert_eq!(
        log.durable_records().copied().collect::<Vec<_>>(),
        log.records()
    );
    assert_eq!(log.durable_position(), Some(LogSequenceNumber::new(1)));
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

    assert_eq!(committed.log_position(), LogSequenceNumber::new(2));
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
    assert_eq!(first_commit.log_position(), LogSequenceNumber::new(1));

    log.arm_fault(FaultPoint::BeforeFlush)?;
    let second = coordinator.begin()?;
    let second_id = second.transaction_id();
    let second_cause = commit_cause(coordinator.commit(second, &mut log))?;

    assert_eq!(
        second_cause,
        CommitError::Flush {
            position: LogSequenceNumber::new(2),
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
    assert_eq!(
        restarted.durable_position(),
        Some(LogSequenceNumber::new(1))
    );

    let third = coordinator.begin()?;
    let third_commit = coordinator.commit(third, &mut restarted)?;
    assert_eq!(third_commit.log_position(), LogSequenceNumber::new(3));
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
            position: LogSequenceNumber::new(1),
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

    let error = log
        .flush_through(LogSequenceNumber::new(0))
        .err()
        .ok_or_else(|| io::Error::other("unknown position was accepted"))?;
    assert_eq!(
        error,
        InMemoryCommitLogError::UnknownFlushPosition(LogSequenceNumber::new(0))
    );
    assert_eq!(log.armed_fault(), Some(FaultPoint::BeforeFlush));

    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let cause = commit_cause(coordinator.commit(active, &mut log))?;
    assert_eq!(
        cause,
        CommitError::Flush {
            position: LogSequenceNumber::new(1),
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeFlush),
        }
    );

    log.flush_through(LogSequenceNumber::new(1))?;
    log.arm_fault(FaultPoint::BeforeFlush)?;
    log.flush_through(LogSequenceNumber::new(1))?;
    assert_eq!(log.armed_fault(), Some(FaultPoint::BeforeFlush));

    let second = coordinator.begin()?;
    let second_cause = commit_cause(coordinator.commit(second, &mut log))?;
    assert_eq!(
        second_cause,
        CommitError::Flush {
            position: LogSequenceNumber::new(2),
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeFlush),
        }
    );
    log.flush_through(LogSequenceNumber::new(2))?;

    log.arm_fault(FaultPoint::AfterFlush)?;
    log.flush_through(LogSequenceNumber::new(2))?;
    assert_eq!(log.armed_fault(), Some(FaultPoint::AfterFlush));

    let third = coordinator.begin()?;
    let third_cause = commit_cause(coordinator.commit(third, &mut log))?;
    assert_eq!(
        third_cause,
        CommitError::Flush {
            position: LogSequenceNumber::new(3),
            source: InMemoryCommitLogError::InjectedFault(FaultPoint::AfterFlush),
        }
    );
    assert_eq!(log.durable_records().len(), 3);
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
    let error = result
        .err()
        .ok_or_else(|| io::Error::other("faulted commit unexpectedly succeeded"))?;
    match error {
        CoordinatedCommitError::Indeterminate(error) => Ok(error.into_parts().1),
        CoordinatedCommitError::Rejected(_) => {
            Err(io::Error::other("faulted commit was rejected before WAL").into())
        }
    }
}
