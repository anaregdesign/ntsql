use std::{error::Error, io};

use ntsql_page::{
    FlushDirtyPageError, FlushDirtyPageRejectionReason, PageAddress, PageImage, PageLog,
    PageNumber, PageVersion, StagePageWriteError, StagePageWriteRejectionReason, flush_dirty_page,
    stage_page_write,
};
use ntsql_storage_memory::{
    FaultPoint, InMemoryCommitLog, InMemoryCommitLogError, InMemoryPageStore,
    InMemoryPageStoreError, PageStoreFaultPoint,
};
use ntsql_transaction::TransactionCoordinator;
use ntsql_wal::{LogDurability, LogLineage, PersistentLogId};

#[test]
fn transaction_and_page_records_share_ordering_and_durable_prefix() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<4>::default();
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let first = coordinator.begin()?;
    let first_id = first.transaction_id();
    let first_commit = coordinator.commit(first, &mut log)?;
    let page = unlogged_page(log.lineage(), 7, 3, [1, 2, 3, 4])?;
    let dirty = stage_page_write(&mut log, page)?;
    let second = coordinator.begin()?;
    let second_id = second.transaction_id();
    let second_commit = coordinator.commit(second, &mut log)?;

    assert_eq!(first_commit.log_position().get(), 1);
    assert_eq!(dirty.required_position().get(), 2);
    assert_eq!(second_commit.log_position().get(), 3);
    assert_eq!(log.records().len(), 3);
    assert_eq!(log.durable_records().len(), 3);
    assert_eq!(log.durable_position(), Some(log.lineage().position(3)));
    assert_eq!(log.records()[0].transaction_id(), Some(first_id));
    assert_eq!(log.records()[1].transaction_id(), None);
    assert_eq!(log.records()[2].transaction_id(), Some(second_id));
    let page_record = log.records()[1]
        .page_write()
        .ok_or_else(|| io::Error::other("page record is missing"))?;
    assert_eq!(page_record.page_number().get(), 7);
    assert_eq!(page_record.page_version().get(), 3);
    assert_eq!(page_record.bytes(), &[1, 2, 3, 4]);
    Ok(())
}

#[test]
fn page_record_retains_exact_bytes_and_enters_durable_prefix_on_flush() -> Result<(), Box<dyn Error>>
{
    let mut log = InMemoryCommitLog::<3>::default();
    let page = unlogged_page(log.lineage(), 9, 4, [9, 8, 7])?;
    let dirty = stage_page_write(&mut log, page)?;

    assert_eq!(log.records().len(), 1);
    assert_eq!(log.durable_records().len(), 0);
    let page_record = log.records()[0]
        .page_write()
        .ok_or_else(|| io::Error::other("page record is missing"))?;
    assert_eq!(page_record.page_number().get(), 9);
    assert_eq!(page_record.page_version().get(), 4);
    assert_eq!(page_record.bytes(), &[9, 8, 7]);

    log.flush_through(dirty.required_position())?;

    assert_eq!(log.durable_records().len(), 1);
    assert_eq!(log.durable_position(), Some(log.lineage().position(1)));
    let durable_page_record = log
        .durable_records()
        .next()
        .and_then(|record| record.page_write())
        .ok_or_else(|| io::Error::other("durable page record is missing"))?;
    assert_eq!(durable_page_record.bytes(), &[9, 8, 7]);
    Ok(())
}

#[test]
fn restart_discards_volatile_page_records_but_preserves_position_high_water()
-> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<2>::default();
    let page = unlogged_page(log.lineage(), 3, 1, [4, 5])?;
    let dirty = stage_page_write(&mut log, page)?;

    assert_eq!(dirty.required_position().get(), 1);
    assert_eq!(log.records().len(), 1);
    assert_eq!(log.durable_records().len(), 0);

    let mut restarted = log.restart();
    assert!(restarted.records().is_empty());
    assert_eq!(restarted.durable_records().len(), 0);

    let replacement_page = unlogged_page(restarted.lineage(), 4, 2, [6, 7])?;
    let replacement = stage_page_write(&mut restarted, replacement_page)?;
    assert_eq!(replacement.required_position().get(), 2);
    Ok(())
}

#[test]
fn persistent_reopen_reconstructs_durable_page_positions() -> Result<(), Box<dyn Error>> {
    let persistent_id = persistent_log_id(7)?;
    let mut log = InMemoryCommitLog::<4>::with_persistent_lineage_id(persistent_id);
    let durable_page = unlogged_page(log.lineage(), 1, 1, [1, 1, 1, 1])?;
    let durable_dirty = stage_page_write(&mut log, durable_page)?;
    log.flush_through(durable_dirty.required_position())?;
    log.arm_fault(FaultPoint::AfterAppend)?;
    let volatile_page = unlogged_page(log.lineage(), 2, 2, [2, 2, 2, 2])?;
    let error = stage_page_write(&mut log, volatile_page)
        .err()
        .ok_or_else(|| io::Error::other("after-append fault unexpectedly succeeded"))?;
    let StagePageWriteError::Append(error) = error else {
        return Err(io::Error::other("after-append fault returned wrong error shape").into());
    };
    assert_eq!(
        error.cause(),
        &InMemoryCommitLogError::InjectedFault(FaultPoint::AfterAppend)
    );
    assert_eq!(log.records().len(), 2);

    log.reopen()?;

    assert_eq!(log.records().len(), 1);
    assert_eq!(log.durable_records().len(), 1);
    let record = log
        .records()
        .first()
        .ok_or_else(|| io::Error::other("durable record disappeared after reopen"))?;
    assert_eq!(record.position(), &log.lineage().position(1));
    let page_record = record
        .page_write()
        .ok_or_else(|| io::Error::other("reopened record lost page payload"))?;
    assert_eq!(page_record.page_number().get(), 1);
    assert_eq!(page_record.page_version().get(), 1);
    assert_eq!(page_record.bytes(), &[1, 1, 1, 1]);

    let later_page = unlogged_page(log.lineage(), 3, 3, [3, 3, 3, 3])?;
    let later = stage_page_write(&mut log, later_page)?;
    assert_eq!(later.required_position().get(), 3);
    Ok(())
}

#[test]
fn end_to_end_page_write_produces_clean_page_and_store_snapshot() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<4>::default();
    let mut store = InMemoryPageStore::new(&log);
    let page = unlogged_page(log.lineage(), 11, 5, [7, 8, 9, 10])?;
    let dirty = stage_page_write(&mut log, page)?;

    let clean = flush_dirty_page(&mut log, &mut store, dirty)?;

    assert_eq!(
        clean.address(),
        &PageAddress::new(log.lineage(), page_number(11)?)
    );
    assert_eq!(clean.version().get(), 5);
    assert_eq!(clean.image().bytes(), &[7, 8, 9, 10]);
    assert_eq!(clean.required_position().get(), 1);
    let stored = store
        .page(page_number(11)?)
        .ok_or_else(|| io::Error::other("stored page is missing"))?;
    assert_eq!(stored.page_number().get(), 11);
    assert_eq!(stored.page_version().get(), 5);
    assert_eq!(stored.bytes(), &[7, 8, 9, 10]);
    assert_eq!(stored.required_position(), clean.required_position());
    Ok(())
}

#[test]
fn wal_before_and_after_append_faults_leave_ambiguous_terminal_stage() -> Result<(), Box<dyn Error>>
{
    let mut before_log = InMemoryCommitLog::<2>::default();
    before_log.arm_fault(FaultPoint::BeforeAppend)?;
    let before_page = unlogged_page(before_log.lineage(), 20, 1, [1, 2])?;
    let before_error = stage_page_write(&mut before_log, before_page)
        .err()
        .ok_or_else(|| io::Error::other("before-append fault unexpectedly succeeded"))?;
    let StagePageWriteError::Append(before_error) = before_error else {
        return Err(io::Error::other("before-append fault returned wrong error shape").into());
    };
    assert_eq!(
        before_error.cause(),
        &InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeAppend)
    );
    assert!(before_error.page().observed_position().is_none());
    assert!(before_log.records().is_empty());

    let mut after_log = InMemoryCommitLog::<2>::default();
    after_log.arm_fault(FaultPoint::AfterAppend)?;
    let after_page = unlogged_page(after_log.lineage(), 21, 2, [3, 4])?;
    let after_error = stage_page_write(&mut after_log, after_page)
        .err()
        .ok_or_else(|| io::Error::other("after-append fault unexpectedly succeeded"))?;
    let StagePageWriteError::Append(after_error) = after_error else {
        return Err(io::Error::other("after-append fault returned wrong error shape").into());
    };
    assert_eq!(
        after_error.cause(),
        &InMemoryCommitLogError::InjectedFault(FaultPoint::AfterAppend)
    );
    assert!(after_error.page().observed_position().is_none());
    assert_eq!(after_log.records().len(), 1);
    let stored = after_log.records()[0]
        .page_write()
        .ok_or_else(|| io::Error::other("after-append page record is missing"))?;
    assert_eq!(stored.page_number().get(), 21);
    assert_eq!(stored.bytes(), &[3, 4]);
    Ok(())
}

#[test]
fn direct_page_log_rejects_foreign_page_before_fault_or_position_effect()
-> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<2>::default();
    log.arm_fault(FaultPoint::AfterAppend)?;
    let foreign_lineage = LogLineage::new();
    let foreign_page = unlogged_page(&foreign_lineage, 22, 7, [5, 6])?;

    let error = PageLog::append_page(&mut log, &foreign_page)
        .err()
        .ok_or_else(|| io::Error::other("foreign page unexpectedly appended"))?;

    assert_eq!(
        error,
        InMemoryCommitLogError::ForeignPageLineage(page_number(22)?)
    );
    assert!(log.records().is_empty());
    assert_eq!(log.durable_position(), None);
    assert_eq!(log.armed_fault(), Some(FaultPoint::AfterAppend));

    let local_page = unlogged_page(log.lineage(), 23, 8, [7, 8])?;
    let local_error = PageLog::append_page(&mut log, &local_page)
        .err()
        .ok_or_else(|| io::Error::other("armed append fault unexpectedly disappeared"))?;
    assert_eq!(
        local_error,
        InMemoryCommitLogError::InjectedFault(FaultPoint::AfterAppend)
    );
    assert_eq!(log.records().len(), 1);
    assert_eq!(log.records()[0].position().get(), 1);
    Ok(())
}

#[test]
fn wal_flush_failure_retains_dirty_page_for_safe_retry() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<4>::default();
    let mut store = InMemoryPageStore::new(&log);
    let page = unlogged_page(log.lineage(), 30, 6, [1, 3, 5, 7])?;
    let dirty = stage_page_write(&mut log, page)?;
    log.arm_fault(FaultPoint::BeforeFlush)?;

    let error = flush_dirty_page(&mut log, &mut store, dirty)
        .err()
        .ok_or_else(|| io::Error::other("before-flush fault unexpectedly succeeded"))?;
    let dirty = match error {
        FlushDirtyPageError::LogFlush(error) => {
            assert_eq!(
                error.cause(),
                &InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeFlush)
            );
            let (dirty, source) = error.into_parts();
            assert_eq!(
                source,
                InMemoryCommitLogError::InjectedFault(FaultPoint::BeforeFlush)
            );
            dirty
        }
        FlushDirtyPageError::Rejected(_) | FlushDirtyPageError::StoreWrite(_) => {
            return Err(io::Error::other("before-flush fault returned wrong error shape").into());
        }
    };
    assert!(store.pages().is_empty());
    assert_eq!(log.durable_records().len(), 0);

    let clean = flush_dirty_page(&mut log, &mut store, dirty)?;
    assert_eq!(clean.required_position().get(), 1);
    assert_eq!(store.pages().len(), 1);
    Ok(())
}

#[test]
fn wal_after_flush_failure_retains_dirty_page_after_durable_prefix_then_retry_writes_page()
-> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<4>::default();
    let mut store = InMemoryPageStore::new(&log);
    let page = unlogged_page(log.lineage(), 31, 7, [2, 4, 6, 8])?;
    let dirty = stage_page_write(&mut log, page)?;
    log.arm_fault(FaultPoint::AfterFlush)?;

    let error = flush_dirty_page(&mut log, &mut store, dirty)
        .err()
        .ok_or_else(|| io::Error::other("after-flush fault unexpectedly succeeded"))?;
    let dirty = match error {
        FlushDirtyPageError::LogFlush(error) => {
            assert_eq!(
                error.cause(),
                &InMemoryCommitLogError::InjectedFault(FaultPoint::AfterFlush)
            );
            let (dirty, source) = error.into_parts();
            assert_eq!(
                source,
                InMemoryCommitLogError::InjectedFault(FaultPoint::AfterFlush)
            );
            dirty
        }
        FlushDirtyPageError::Rejected(_) | FlushDirtyPageError::StoreWrite(_) => {
            return Err(io::Error::other("after-flush fault returned wrong error shape").into());
        }
    };
    assert!(store.pages().is_empty());
    assert_eq!(log.durable_position(), Some(log.lineage().position(1)));

    let clean = flush_dirty_page(&mut log, &mut store, dirty)?;
    assert_eq!(clean.required_position().get(), 1);
    let stored = store
        .page(page_number(31)?)
        .ok_or_else(|| io::Error::other("retry did not store page"))?;
    assert_eq!(stored.page_version().get(), 7);
    assert_eq!(stored.bytes(), &[2, 4, 6, 8]);
    Ok(())
}

#[test]
fn page_store_before_and_after_write_faults_have_different_physical_effects()
-> Result<(), Box<dyn Error>> {
    let mut before_log = InMemoryCommitLog::<2>::default();
    let mut before_store = InMemoryPageStore::new(&before_log);
    before_store.arm_fault(PageStoreFaultPoint::BeforeWrite)?;
    let before_page = unlogged_page(before_log.lineage(), 40, 1, [8, 9])?;
    let before_dirty = stage_page_write(&mut before_log, before_page)?;

    let before_error = flush_dirty_page(&mut before_log, &mut before_store, before_dirty)
        .err()
        .ok_or_else(|| io::Error::other("before-write fault unexpectedly succeeded"))?;
    let FlushDirtyPageError::StoreWrite(before_error) = before_error else {
        return Err(io::Error::other("before-write fault returned wrong error shape").into());
    };
    assert_eq!(
        before_error.cause(),
        &InMemoryPageStoreError::InjectedFault(PageStoreFaultPoint::BeforeWrite)
    );
    assert!(before_store.pages().is_empty());
    assert_eq!(
        before_log.durable_position(),
        Some(before_log.lineage().position(1))
    );

    let mut after_log = InMemoryCommitLog::<2>::default();
    let mut after_store = InMemoryPageStore::new(&after_log);
    after_store.arm_fault(PageStoreFaultPoint::AfterWrite)?;
    let after_page = unlogged_page(after_log.lineage(), 41, 2, [10, 11])?;
    let after_dirty = stage_page_write(&mut after_log, after_page)?;

    let after_error = flush_dirty_page(&mut after_log, &mut after_store, after_dirty)
        .err()
        .ok_or_else(|| io::Error::other("after-write fault unexpectedly succeeded"))?;
    let FlushDirtyPageError::StoreWrite(after_error) = after_error else {
        return Err(io::Error::other("after-write fault returned wrong error shape").into());
    };
    assert_eq!(
        after_error.cause(),
        &InMemoryPageStoreError::InjectedFault(PageStoreFaultPoint::AfterWrite)
    );
    let stored = after_store
        .page(page_number(41)?)
        .ok_or_else(|| io::Error::other("after-write store lost updated page"))?;
    assert_eq!(stored.page_version().get(), 2);
    assert_eq!(stored.bytes(), &[10, 11]);
    Ok(())
}

#[test]
fn later_page_write_replaces_existing_snapshot() -> Result<(), Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<3>::default();
    let mut store = InMemoryPageStore::new(&log);
    let first_page = unlogged_page(log.lineage(), 50, 1, [1, 1, 1])?;
    let first = stage_page_write(&mut log, first_page)?;
    let first_clean = flush_dirty_page(&mut log, &mut store, first)?;
    let second_page = unlogged_page(log.lineage(), 50, 2, [2, 2, 2])?;
    let second = stage_page_write(&mut log, second_page)?;
    let second_clean = flush_dirty_page(&mut log, &mut store, second)?;

    assert_eq!(first_clean.required_position().get(), 1);
    assert_eq!(second_clean.required_position().get(), 2);
    assert_eq!(store.pages().len(), 1);
    let stored = store
        .page(page_number(50)?)
        .ok_or_else(|| io::Error::other("replacement page is missing"))?;
    assert_eq!(stored.page_version().get(), 2);
    assert_eq!(stored.bytes(), &[2, 2, 2]);
    assert_eq!(stored.required_position().get(), 2);
    Ok(())
}

#[test]
fn foreign_log_and_store_lineages_are_rejected_before_mutation() -> Result<(), Box<dyn Error>> {
    let owner_log = InMemoryCommitLog::<2>::default();
    let mut foreign_log = InMemoryCommitLog::<2>::default();
    let foreign_page = unlogged_page(owner_log.lineage(), 60, 1, [1, 9])?;
    let stage_error = stage_page_write(&mut foreign_log, foreign_page)
        .err()
        .ok_or_else(|| io::Error::other("foreign WAL lineage unexpectedly accepted page"))?;
    let StagePageWriteError::Rejected(stage_rejection) = stage_error else {
        return Err(io::Error::other("foreign WAL lineage reached append").into());
    };
    assert_eq!(
        stage_rejection.reason(),
        StagePageWriteRejectionReason::ForeignLog
    );
    assert!(foreign_log.records().is_empty());

    let mut log = InMemoryCommitLog::<2>::default();
    let mut foreign_store = InMemoryPageStore::with_lineage(LogLineage::new());
    let page = unlogged_page(log.lineage(), 61, 2, [2, 8])?;
    let dirty = stage_page_write(&mut log, page)?;
    let flush_error = flush_dirty_page(&mut log, &mut foreign_store, dirty)
        .err()
        .ok_or_else(|| io::Error::other("foreign store lineage unexpectedly accepted page"))?;
    let FlushDirtyPageError::Rejected(rejection) = flush_error else {
        return Err(io::Error::other("foreign store lineage reached write").into());
    };
    assert_eq!(
        rejection.reason(),
        FlushDirtyPageRejectionReason::ForeignStore
    );
    assert!(foreign_store.pages().is_empty());
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
