use std::{error::Error, io};

use ntsql_page::{PageAddress, PageImage, PageNumber, PageVersion, stage_page_write};
use ntsql_storage_memory::{
    FaultPoint, InMemoryCommitLog, InMemoryCommitLogError, InMemoryPageStore,
    InMemoryPageStoreError, InMemoryTransactionRecoveryError, PageStoreFaultPoint,
};
use ntsql_transaction::{
    CoordinatedCommitError, DurableCommitLookup, TransactionCommitResolution,
    TransactionCommittedFlushError, TransactionCoordinator, TransactionLifecycleStatus,
    TransactionPageStageError, TransactionPageStageRejectionReason, TransactionRecoverySource,
    flush_committed_page,
};
use ntsql_wal::{LogDurability, LogLineage, PersistentLogId};

#[test]
fn owned_page_record_snapshots_exact_owner_number_version_bytes_and_position()
-> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<4>::default();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 7, 3, [1, 2, 3, 4])?;

    let (_active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;

    assert_eq!(dirty.required_position().get(), 1);
    assert_eq!(log.records().len(), 1);
    let record = &log.records()[0];
    assert_eq!(record.position(), &log.lineage().position(1));
    assert_eq!(record.transaction_id(), None);
    assert_eq!(record.page_owner_transaction_id(), Some(owner));
    let owned = record
        .transaction_page_write()
        .ok_or_else(|| io::Error::other("owned page record is missing"))?;
    assert_eq!(owned.transaction_id(), owner);
    assert_eq!(owned.page_write().page_number().get(), 7);
    assert_eq!(owned.page_write().page_version().get(), 3);
    assert_eq!(owned.page_write().bytes(), &[1, 2, 3, 4]);
    // page_write() returns the same payload through the shared accessor.
    let payload = record
        .page_write()
        .ok_or_else(|| io::Error::other("shared page payload is missing"))?;
    assert_eq!(payload.bytes(), &[1, 2, 3, 4]);
    Ok(())
}

#[test]
fn durable_owned_page_without_commit_is_never_a_commit_record() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<3>::default();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 9, 4, [9, 8, 7])?;

    let (_active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
    log.flush_through(dirty.required_position())?;

    assert_eq!(log.durable_records().len(), 1);
    let record = &log.records()[0];
    // Commit-only: an owned page never looks committed.
    assert_eq!(record.transaction_id(), None);
    assert_eq!(record.page_owner_transaction_id(), Some(owner));
    let observation = record
        .page_recovery_observation()?
        .ok_or_else(|| io::Error::other("durable owned page projected as transaction"))?;
    assert_eq!(observation.page_number().get(), 9);
    assert_eq!(observation.page_version().get(), 4);
    assert_eq!(observation.image().bytes(), &[9, 8, 7]);
    assert_eq!(observation.position(), &log.lineage().position(1));

    // Recovery scan finds no durable commit for the page owner.
    let (lineage, lookup) = TransactionRecoverySource::lookup_durable_commit(&mut log, owner)?;
    assert!(lineage.same_lineage(log.lineage()));
    assert_eq!(lookup, DurableCommitLookup::Absent);
    Ok(())
}

#[test]
fn owned_page_plus_real_commit_looks_up_found_not_duplicate() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<2>::default();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 12, 6, [5, 6])?;

    let (active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
    let committed = coordinator.commit(active, &mut log)?;

    assert_eq!(dirty.required_position().get(), 1);
    assert_eq!(committed.log_position().get(), 2);
    assert!(dirty.required_position().get() < committed.log_position().get());
    // Commit flush covers both records on the shared frontier.
    assert_eq!(log.durable_records().len(), 2);
    assert_eq!(log.durable_position(), Some(log.lineage().position(2)));

    let (_lineage, lookup) = TransactionRecoverySource::lookup_durable_commit(&mut log, owner)?;
    // Found at the commit position exactly, never Duplicate from the page record.
    assert_eq!(
        lookup,
        DurableCommitLookup::Found {
            position: log.lineage().position(2),
        }
    );
    Ok(())
}

#[test]
fn staged_committed_flush_stores_owned_clean_page() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<4>::default();
    let mut store = InMemoryPageStore::new(&log);
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 15, 5, [7, 8, 9, 10])?;

    let (active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
    let committed = coordinator.commit(active, &mut log)?;
    let clean = flush_committed_page(&committed, &mut log, &mut store, dirty)?;

    assert_eq!(clean.transaction_id(), owner);
    assert_eq!(
        clean.address(),
        &PageAddress::new(log.lineage(), page_number(15)?)
    );
    assert_eq!(clean.version().get(), 5);
    assert_eq!(clean.image().bytes(), &[7, 8, 9, 10]);
    assert_eq!(clean.required_position().get(), 1);
    let stored = store
        .page(page_number(15)?)
        .ok_or_else(|| io::Error::other("stored owned page is missing"))?;
    assert_eq!(stored.page_number().get(), 15);
    assert_eq!(stored.page_version().get(), 5);
    assert_eq!(stored.bytes(), &[7, 8, 9, 10]);
    assert_eq!(stored.required_position(), clean.required_position());
    Ok(())
}

#[test]
fn manual_flush_before_commit_is_durable_but_grants_no_authorization() -> Result<(), Box<dyn Error>>
{
    let mut log = InMemoryCommitLog::<2>::default();
    let store = InMemoryPageStore::new(&log);
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 18, 2, [4, 5])?;

    let (_active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
    log.flush_through(dirty.required_position())?;

    // The page record is durable, but durability is not authorization.
    assert_eq!(log.durable_records().len(), 1);
    assert!(store.pages().is_empty());
    let (_lineage, lookup) = TransactionRecoverySource::lookup_durable_commit(&mut log, owner)?;
    assert_eq!(lookup, DurableCommitLookup::Absent);
    Ok(())
}

#[test]
fn owned_page_before_and_after_append_faults_are_terminal() -> Result<(), Box<dyn Error>> {
    let mut before_log = InMemoryCommitLog::<2>::default();
    let mut before_coordinator = TransactionCoordinator::open(&mut before_log)?;
    before_log.arm_fault(FaultPoint::BeforeAppend)?;
    let before_active = before_coordinator.begin()?;
    let before_owner = before_active.transaction_id();
    let before_page = unlogged_page(before_log.lineage(), 20, 1, [1, 2])?;

    let before_error = before_coordinator
        .stage_page_write(before_active, before_page, &mut before_log)
        .err()
        .ok_or_else(|| io::Error::other("before-append fault unexpectedly succeeded"))?;
    let TransactionPageStageError::Append(before_error) = before_error else {
        return Err(io::Error::other("before-append fault returned wrong error shape").into());
    };
    assert_eq!(
        before_error.cause(),
        &InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeAppend)
    );
    assert!(before_log.records().is_empty());
    assert_eq!(before_log.armed_fault(), None);
    assert_eq!(
        before_coordinator.status(before_owner),
        Some(TransactionLifecycleStatus::PageAppendIndeterminate)
    );
    let next_before_page = unlogged_page(before_log.lineage(), 22, 3, [5, 6])?;
    let next_before = stage_page_write(&mut before_log, next_before_page)?;
    assert_eq!(next_before.required_position().get(), 1);

    let mut after_log = InMemoryCommitLog::<2>::default();
    let mut after_coordinator = TransactionCoordinator::open(&mut after_log)?;
    after_log.arm_fault(FaultPoint::AfterAppend)?;
    let after_active = after_coordinator.begin()?;
    let after_owner = after_active.transaction_id();
    let after_page = unlogged_page(after_log.lineage(), 21, 2, [3, 4])?;

    let after_error = after_coordinator
        .stage_page_write(after_active, after_page, &mut after_log)
        .err()
        .ok_or_else(|| io::Error::other("after-append fault unexpectedly succeeded"))?;
    let TransactionPageStageError::Append(after_error) = after_error else {
        return Err(io::Error::other("after-append fault returned wrong error shape").into());
    };
    assert_eq!(
        after_error.cause(),
        &InMemoryCommitLogError::InjectedFault(FaultPoint::AfterAppend)
    );
    assert_eq!(after_log.records().len(), 1);
    let owned = after_log.records()[0]
        .transaction_page_write()
        .ok_or_else(|| io::Error::other("after-append owned record is missing"))?;
    assert_eq!(owned.transaction_id(), after_owner);
    assert_eq!(owned.page_write().page_number().get(), 21);
    assert_eq!(owned.page_write().bytes(), &[3, 4]);
    assert_eq!(after_log.records()[0].position().get(), 1);
    assert_eq!(after_log.durable_records().len(), 0);
    assert_eq!(
        after_coordinator.status(after_owner),
        Some(TransactionLifecycleStatus::PageAppendIndeterminate)
    );
    let next_after_page = unlogged_page(after_log.lineage(), 23, 4, [6, 7])?;
    let next_after = stage_page_write(&mut after_log, next_after_page)?;
    assert_eq!(next_after.required_position().get(), 2);
    Ok(())
}

#[test]
fn commit_before_flush_after_staged_page_leaves_both_records_volatile() -> Result<(), Box<dyn Error>>
{
    let mut log = InMemoryCommitLog::<2>::default();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 24, 1, [8, 9])?;
    let (active, _dirty) = coordinator.stage_page_write(active, page, &mut log)?;

    log.arm_fault(FaultPoint::BeforeFlush)?;
    let error = coordinator
        .commit(active, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("before-flush commit unexpectedly succeeded"))?;
    let CoordinatedCommitError::Indeterminate(_) = error else {
        return Err(io::Error::other("before-flush commit was rejected before WAL").into());
    };

    // Both the page and commit records exist but neither is durable.
    assert_eq!(log.records().len(), 2);
    assert_eq!(log.durable_records().len(), 0);
    assert_eq!(
        coordinator.status(owner),
        Some(TransactionLifecycleStatus::Indeterminate)
    );
    let error = TransactionRecoverySource::lookup_durable_commit(&mut log, owner)
        .err()
        .ok_or_else(|| io::Error::other("volatile commit lookup unexpectedly resolved"))?;
    assert_eq!(
        error,
        InMemoryTransactionRecoveryError::VolatileCommitRecord(owner)
    );
    Ok(())
}

#[test]
fn commit_after_flush_resolves_committed_then_flushes_page() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<2>::default();
    let mut store = InMemoryPageStore::new(&log);
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 27, 3, [2, 4])?;
    let (active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;

    log.arm_fault(FaultPoint::AfterFlush)?;
    let error = coordinator
        .commit(active, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("after-flush commit unexpectedly succeeded"))?;
    let CoordinatedCommitError::Indeterminate(error) = error else {
        return Err(io::Error::other("after-flush commit was rejected before WAL").into());
    };
    let (indeterminate, _) = error.into_parts();
    // The after-flush fault made both the page and commit durable.
    assert_eq!(log.durable_records().len(), 2);
    assert_eq!(
        coordinator.status(owner),
        Some(TransactionLifecycleStatus::Indeterminate)
    );

    let resolution = coordinator.resolve(indeterminate, &mut log)?;
    let TransactionCommitResolution::Committed(committed) = resolution else {
        return Err(io::Error::other("durable commit resolved as absent").into());
    };
    assert_eq!(committed.transaction_id(), owner);
    assert_eq!(committed.log_position().get(), 2);

    let clean = flush_committed_page(&committed, &mut log, &mut store, dirty)?;
    assert_eq!(clean.transaction_id(), owner);
    let stored = store
        .page(page_number(27)?)
        .ok_or_else(|| io::Error::other("resolved commit did not store page"))?;
    assert_eq!(stored.page_version().get(), 3);
    assert_eq!(stored.bytes(), &[2, 4]);
    Ok(())
}

#[test]
fn committed_page_store_faults_preserve_owner_and_terminal_indeterminate_write()
-> Result<(), Box<dyn Error>> {
    // BeforeWrite: durable WAL, store untouched, terminal indeterminate write.
    let mut before_log = InMemoryCommitLog::<2>::default();
    let mut before_store = InMemoryPageStore::new(&before_log);
    let mut before_coordinator = TransactionCoordinator::open(&mut before_log)?;
    let before_active = before_coordinator.begin()?;
    let before_owner = before_active.transaction_id();
    let before_page = unlogged_page(before_log.lineage(), 30, 6, [1, 3])?;
    let (before_active, before_dirty) =
        before_coordinator.stage_page_write(before_active, before_page, &mut before_log)?;
    let before_committed = before_coordinator.commit(before_active, &mut before_log)?;
    before_store.arm_fault(PageStoreFaultPoint::BeforeWrite)?;

    let before_error = flush_committed_page(
        &before_committed,
        &mut before_log,
        &mut before_store,
        before_dirty,
    )
    .err()
    .ok_or_else(|| io::Error::other("before-write fault unexpectedly succeeded"))?;
    let TransactionCommittedFlushError::StoreWrite(before_error) = before_error else {
        return Err(io::Error::other("before-write fault returned wrong error shape").into());
    };
    assert_eq!(before_error.transaction_id(), before_owner);
    assert_eq!(before_error.page().address().number().get(), 30);
    assert_eq!(before_error.page().image().bytes(), &[1, 3]);
    assert_eq!(
        before_error.cause(),
        &InMemoryPageStoreError::InjectedFault(PageStoreFaultPoint::BeforeWrite)
    );
    assert!(before_store.pages().is_empty());
    assert_eq!(
        before_log.durable_position(),
        Some(before_log.lineage().position(2))
    );

    // AfterWrite: durable WAL, store mutated, terminal indeterminate write.
    let mut after_log = InMemoryCommitLog::<2>::default();
    let mut after_store = InMemoryPageStore::new(&after_log);
    let mut after_coordinator = TransactionCoordinator::open(&mut after_log)?;
    let after_active = after_coordinator.begin()?;
    let after_owner = after_active.transaction_id();
    let after_page = unlogged_page(after_log.lineage(), 31, 7, [2, 8])?;
    let (after_active, after_dirty) =
        after_coordinator.stage_page_write(after_active, after_page, &mut after_log)?;
    let after_committed = after_coordinator.commit(after_active, &mut after_log)?;
    after_store.arm_fault(PageStoreFaultPoint::AfterWrite)?;

    let after_error = flush_committed_page(
        &after_committed,
        &mut after_log,
        &mut after_store,
        after_dirty,
    )
    .err()
    .ok_or_else(|| io::Error::other("after-write fault unexpectedly succeeded"))?;
    let TransactionCommittedFlushError::StoreWrite(after_error) = after_error else {
        return Err(io::Error::other("after-write fault returned wrong error shape").into());
    };
    assert_eq!(after_error.transaction_id(), after_owner);
    assert_eq!(
        after_error.cause(),
        &InMemoryPageStoreError::InjectedFault(PageStoreFaultPoint::AfterWrite)
    );
    let stored = after_store
        .page(page_number(31)?)
        .ok_or_else(|| io::Error::other("after-write store lost the page"))?;
    assert_eq!(stored.page_version().get(), 7);
    assert_eq!(stored.bytes(), &[2, 8]);
    Ok(())
}

#[test]
fn restart_stale_wrapper_reflush_fails_on_unknown_position_without_store()
-> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<2>::default();
    let mut store = InMemoryPageStore::new(&log);
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 33, 4, [6, 7])?;
    let (active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
    assert_eq!(dirty.required_position().get(), 1);
    assert_eq!(log.records().len(), 1);
    assert_eq!(log.durable_records().len(), 0);
    let stale_page_position = dirty.required_position().clone();

    // Lose the volatile page record while retaining the active token and wrapper.
    let mut restarted = log.restart();
    assert!(restarted.records().is_empty());

    // The retained active token commits on the restarted log at a later high-water
    // position, so the committed position is strictly after the stale page position.
    let committed = coordinator.commit(active, &mut restarted)?;
    assert_eq!(committed.transaction_id(), owner);
    assert_eq!(committed.log_position().get(), 2);

    let error = flush_committed_page(&committed, &mut restarted, &mut store, dirty)
        .err()
        .ok_or_else(|| io::Error::other("stale reflush unexpectedly succeeded"))?;
    let TransactionCommittedFlushError::LogFlush(error) = error else {
        return Err(io::Error::other("stale reflush returned wrong error shape").into());
    };
    assert_eq!(
        error.cause(),
        &InMemoryCommitLogError::UnknownFlushPosition(stale_page_position.clone())
    );
    // The retryable wrapper is retained and the store was never called.
    let (retained, source) = error.into_parts();
    assert_eq!(retained.transaction_id(), owner);
    assert_eq!(retained.required_position(), &stale_page_position);
    assert_eq!(
        source,
        InMemoryCommitLogError::UnknownFlushPosition(stale_page_position)
    );
    assert!(store.pages().is_empty());
    Ok(())
}

#[test]
fn restart_and_reopen_retain_durable_owned_page_and_commit() -> Result<(), Box<dyn Error>> {
    let persistent_id = persistent_log_id(9)?;
    let mut log = InMemoryCommitLog::<3>::with_persistent_lineage_id(persistent_id);
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 36, 8, [1, 2, 3])?;
    let (active, _dirty) = coordinator.stage_page_write(active, page, &mut log)?;
    let committed = coordinator.commit(active, &mut log)?;
    assert_eq!(committed.log_position().get(), 2);
    assert_eq!(log.durable_records().len(), 2);

    // Restart retains the durable owned page and commit records and positions.
    let mut restarted = log.restart();
    assert_eq!(restarted.records().len(), 2);
    assert_eq!(restarted.durable_records().len(), 2);
    let owned = restarted.records()[0]
        .transaction_page_write()
        .ok_or_else(|| io::Error::other("restart lost the owned page record"))?;
    assert_eq!(owned.transaction_id(), owner);
    assert_eq!(owned.page_write().bytes(), &[1, 2, 3]);
    assert_eq!(restarted.records()[0].position().get(), 1);
    assert_eq!(restarted.records()[1].transaction_id(), Some(owner));
    assert_eq!(restarted.records()[1].position().get(), 2);

    // Persistent reopen reconstructs both positions under the persistent lineage.
    restarted.reopen()?;
    assert_eq!(restarted.records().len(), 2);
    let reopened_owned = restarted.records()[0]
        .transaction_page_write()
        .ok_or_else(|| io::Error::other("reopen lost the owned page record"))?;
    assert_eq!(reopened_owned.transaction_id(), owner);
    assert_eq!(reopened_owned.page_write().page_number().get(), 36);
    assert_eq!(reopened_owned.page_write().page_version().get(), 8);
    assert_eq!(reopened_owned.page_write().bytes(), &[1, 2, 3]);
    assert_eq!(
        restarted.records()[0].position(),
        &restarted.lineage().position(1)
    );
    assert_eq!(
        restarted.records()[1].position(),
        &restarted.lineage().position(2)
    );

    // The allocator high-water mark is preserved after reopen.
    let later_page = unlogged_page(restarted.lineage(), 37, 9, [4, 5, 6])?;
    let later = stage_page_write(&mut restarted, later_page)?;
    assert_eq!(later.required_position().get(), 3);
    Ok(())
}

#[test]
fn coordinator_rejects_foreign_page_before_any_log_effect() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<2>::default();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    log.arm_fault(FaultPoint::AfterAppend)?;
    let active = coordinator.begin()?;
    let owner = active.transaction_id();
    let foreign_lineage = LogLineage::new();
    let foreign_page = unlogged_page(&foreign_lineage, 39, 5, [7, 8])?;

    let error = coordinator
        .stage_page_write(active, foreign_page, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("foreign page unexpectedly staged"))?;
    let TransactionPageStageError::Rejected(rejection) = error else {
        return Err(io::Error::other("foreign page reached the append port").into());
    };
    assert_eq!(
        rejection.reason(),
        TransactionPageStageRejectionReason::ForeignPageLineage
    );
    // No fault, position, or record effect occurred on the shared frontier.
    assert!(log.records().is_empty());
    assert_eq!(log.durable_position(), None);
    assert_eq!(log.armed_fault(), Some(FaultPoint::AfterAppend));

    // A local page reaches the still-armed append fault at position one, proving
    // the coordinator rejection consumed neither the fault nor a position.
    let (retained, _page) = rejection.into_parts();
    assert_eq!(retained.transaction_id(), owner);
    let local_page = unlogged_page(log.lineage(), 40, 6, [8, 9])?;
    let local_error = coordinator
        .stage_page_write(retained, local_page, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("armed append fault unexpectedly disappeared"))?;
    let TransactionPageStageError::Append(local_error) = local_error else {
        return Err(io::Error::other("local page did not reach the append port").into());
    };
    assert_eq!(
        local_error.cause(),
        &InMemoryCommitLogError::InjectedFault(FaultPoint::AfterAppend)
    );
    assert_eq!(log.records().len(), 1);
    assert_eq!(log.records()[0].position().get(), 1);
    assert_eq!(log.records()[0].page_owner_transaction_id(), Some(owner));
    Ok(())
}

fn page_number(value: u64) -> Result<PageNumber, io::Error> {
    PageNumber::new(value).ok_or_else(|| io::Error::other("nonzero page number was rejected"))
}

fn page_image<const N: usize>(bytes: [u8; N]) -> Result<PageImage<N>, io::Error> {
    PageImage::new(bytes).map_err(io::Error::other)
}

fn persistent_log_id(value: u128) -> Result<PersistentLogId, io::Error> {
    PersistentLogId::new(value)
        .ok_or_else(|| io::Error::other("nonzero persistent log ID was rejected"))
}

fn unlogged_page<const N: usize>(
    lineage: &LogLineage,
    number: u64,
    version: u64,
    bytes: [u8; N],
) -> Result<ntsql_page::UnloggedPage<N>, io::Error> {
    Ok(ntsql_page::UnloggedPage::new(
        PageAddress::new(lineage, page_number(number)?),
        PageVersion::new(version),
        page_image(bytes)?,
    ))
}
