use std::{error::Error, io};

use ntsql_page::{
    PageAddress, PageImage, PageNumber, PageVersion, flush_dirty_page, stage_page_write,
};
use ntsql_storage_memory::{
    FaultPoint, InMemoryCommitLog, InMemoryCommittedPageRecoveryStoreError, InMemoryPageStore,
    PageStoreFaultPoint,
};
use ntsql_transaction::{
    CommittedTransactionPageRecoveryError, CommittedTransactionPageRecoveryOutcome,
    CommittedTransactionPageRecoverySourceState, CommittedTransactionPageRecoveryTarget,
    CoordinatedCommitError, TransactionCoordinator, TransactionId, flush_committed_page,
    recover_committed_transaction_page,
};
use ntsql_wal::{LogDurability, LogLineage, PersistentLogId};

struct RecoveryScenario {
    log: InMemoryCommitLog<2>,
    behind_store: InMemoryPageStore<2>,
    exact_store: InMemoryPageStore<2>,
    missing_store: InMemoryPageStore<2>,
    raw_store: InMemoryPageStore<2>,
    page_number: PageNumber,
    latest_owner: TransactionId,
}

#[test]
fn recovery_source_excludes_volatile_suffix_and_no_write_paths_preserve_faults()
-> Result<(), Box<dyn Error>> {
    let mut scenario = recovery_scenario()?;
    let page_number = scenario.page_number;

    let (physical, owned, commits) =
        ntsql_transaction::DurableTransactionPageRecoverySource::with_durable_page_evidence(
            &mut scenario.log,
            page_number,
            |physical, owned, commits| {
                (
                    physical
                        .iter()
                        .map(|observation| observation.position().get())
                        .collect::<Vec<_>>(),
                    owned
                        .iter()
                        .map(|observation| observation.position().get())
                        .collect::<Vec<_>>(),
                    commits
                        .iter()
                        .map(|observation| observation.position().get())
                        .collect::<Vec<_>>(),
                )
            },
        )?;
    assert_eq!(physical, [1, 3, 5, 6, 8]);
    assert_eq!(owned, [1, 3, 5, 8]);
    assert_eq!(commits, [2, 4]);
    assert_eq!(scenario.log.records().len(), 8);
    assert_eq!(scenario.log.durable_records().len(), 7);
    assert!(scenario.log.records()[7].transaction_id().is_some());
    assert_eq!(scenario.log.records()[7].position().get(), 9);

    scenario
        .exact_store
        .arm_fault(PageStoreFaultPoint::BeforeWrite)?;
    let exact = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.exact_store,
        page_number,
    )?;
    let CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { target } = exact else {
        return Err(io::Error::other("exact memory store attempted recovery").into());
    };
    assert_latest_target(&scenario, &target);
    assert_eq!(
        scenario.exact_store.armed_fault(),
        Some(PageStoreFaultPoint::BeforeWrite)
    );

    scenario
        .raw_store
        .arm_fault(PageStoreFaultPoint::BeforeWrite)?;
    let raw =
        recover_committed_transaction_page(&mut scenario.log, &mut scenario.raw_store, page_number);
    assert!(matches!(
        raw,
        Err(CommittedTransactionPageRecoveryError::Planning { .. })
    ));
    assert_eq!(
        scenario.raw_store.armed_fault(),
        Some(PageStoreFaultPoint::BeforeWrite)
    );
    let raw_page = scenario
        .raw_store
        .page(page_number)
        .ok_or_else(|| io::Error::other("raw-backed page disappeared"))?;
    assert_eq!(raw_page.page_version(), PageVersion::new(30));
    assert_eq!(raw_page.bytes(), &[7, 8]);
    assert_eq!(raw_page.required_position().get(), 6);
    Ok(())
}

#[test]
fn memory_gate_recovers_missing_and_behind_to_lower_version_committed_target()
-> Result<(), Box<dyn Error>> {
    let mut scenario = recovery_scenario()?;
    let page_number = scenario.page_number;

    let behind = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.behind_store,
        page_number,
    )?;
    let CommittedTransactionPageRecoveryOutcome::Recovered { target } = behind else {
        return Err(io::Error::other("behind memory store was not recovered").into());
    };
    assert_latest_target(&scenario, &target);
    assert_recovered_page(&scenario, &scenario.behind_store)?;

    let missing = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.missing_store,
        page_number,
    )?;
    let CommittedTransactionPageRecoveryOutcome::Recovered { target } = missing else {
        return Err(io::Error::other("missing memory store was not recovered").into());
    };
    assert_latest_target(&scenario, &target);
    assert_recovered_page(&scenario, &scenario.missing_store)?;
    Ok(())
}

#[test]
fn memory_recovery_faults_require_fresh_authoritative_reruns() -> Result<(), Box<dyn Error>> {
    let mut scenario = recovery_scenario()?;
    let page_number = scenario.page_number;
    let mut before_store = InMemoryPageStore::new(&scenario.log);
    before_store.arm_fault(PageStoreFaultPoint::BeforeWrite)?;

    let before =
        recover_committed_transaction_page(&mut scenario.log, &mut before_store, page_number);
    let Err(CommittedTransactionPageRecoveryError::StoreWrite { state }) = before else {
        return Err(io::Error::other("before-write recovery fault was not terminal").into());
    };
    assert_eq!(
        state.as_ref().cause(),
        &InMemoryCommittedPageRecoveryStoreError::InjectedFault(PageStoreFaultPoint::BeforeWrite)
    );
    assert!(matches!(
        state.source_state(),
        CommittedTransactionPageRecoverySourceState::StoreMissing {
            page_number: source_page,
            target_page_position,
        } if *source_page == page_number && target_page_position.get() == 3
    ));
    assert_latest_target(&scenario, state.target());
    assert!(before_store.page(page_number).is_none());

    let retry =
        recover_committed_transaction_page(&mut scenario.log, &mut before_store, page_number)?;
    assert!(matches!(
        retry,
        CommittedTransactionPageRecoveryOutcome::Recovered { .. }
    ));
    assert_recovered_page(&scenario, &before_store)?;

    scenario
        .behind_store
        .arm_fault(PageStoreFaultPoint::AfterWrite)?;
    let after = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.behind_store,
        page_number,
    );
    let Err(CommittedTransactionPageRecoveryError::StoreWrite { state }) = after else {
        return Err(io::Error::other("after-write recovery fault was not terminal").into());
    };
    assert_eq!(
        state.as_ref().cause(),
        &InMemoryCommittedPageRecoveryStoreError::InjectedFault(PageStoreFaultPoint::AfterWrite)
    );
    let CommittedTransactionPageRecoverySourceState::ExactSnapshot {
        page_number: source_page,
        page_version,
        bytes,
        page_position,
        commit_position,
    } = state.source_state()
    else {
        return Err(io::Error::other("after-write fault lost the exact source snapshot").into());
    };
    assert_eq!(*source_page, page_number);
    assert_eq!(*page_version, PageVersion::new(10));
    assert_eq!(bytes, &[1, 2]);
    assert_eq!(page_position.get(), 1);
    assert_eq!(commit_position.get(), 2);
    assert_latest_target(&scenario, state.target());
    assert_recovered_page(&scenario, &scenario.behind_store)?;

    scenario
        .behind_store
        .arm_fault(PageStoreFaultPoint::BeforeWrite)?;
    let resolved = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.behind_store,
        page_number,
    )?;
    assert!(matches!(
        resolved,
        CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
    ));
    assert_eq!(
        scenario.behind_store.armed_fault(),
        Some(PageStoreFaultPoint::BeforeWrite)
    );
    Ok(())
}

fn recovery_scenario() -> Result<RecoveryScenario, Box<dyn Error>> {
    let mut log = InMemoryCommitLog::<2>::with_persistent_lineage_id(persistent_log_id(23)?);
    let page_number = page_number(80)?;
    let mut behind_store = InMemoryPageStore::new(&log);
    let mut exact_store = InMemoryPageStore::new(&log);
    let missing_store = InMemoryPageStore::new(&log);
    let mut raw_store = InMemoryPageStore::new(&log);
    let mut coordinator = TransactionCoordinator::open(&mut log)?;

    let stored_active = coordinator.begin()?;
    let stored_page = unlogged_page(log.lineage(), 80, 10, [1, 2])?;
    let (stored_active, stored_dirty) =
        coordinator.stage_page_write(stored_active, stored_page, &mut log)?;
    let stored_commit = coordinator.commit(stored_active, &mut log)?;
    flush_committed_page(&stored_commit, &mut log, &mut behind_store, stored_dirty)?;

    let latest_active = coordinator.begin()?;
    let latest_page = unlogged_page(log.lineage(), 80, 1, [3, 4])?;
    let (latest_active, latest_dirty) =
        coordinator.stage_page_write(latest_active, latest_page, &mut log)?;
    let latest_owner = latest_active.transaction_id();
    let latest_commit = coordinator.commit(latest_active, &mut log)?;
    flush_committed_page(&latest_commit, &mut log, &mut exact_store, latest_dirty)?;

    let uncommitted_active = coordinator.begin()?;
    let uncommitted_page = unlogged_page(log.lineage(), 80, 20, [5, 6])?;
    let (uncommitted_active, uncommitted_dirty) =
        coordinator.stage_page_write(uncommitted_active, uncommitted_page, &mut log)?;
    log.flush_through(uncommitted_dirty.required_position())?;

    let raw_page = unlogged_page(log.lineage(), 80, 30, [7, 8])?;
    let raw_dirty = stage_page_write(&mut log, raw_page)?;
    flush_dirty_page(&mut log, &mut raw_store, raw_dirty)?;

    log.arm_fault(FaultPoint::BeforeFlush)?;
    let volatile_commit = coordinator
        .commit(uncommitted_active, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("first volatile commit unexpectedly became durable"))?;
    assert!(matches!(
        volatile_commit,
        CoordinatedCommitError::Indeterminate(_)
    ));
    assert_eq!(log.records().len(), 7);
    assert_eq!(log.durable_records().len(), 6);
    drop(uncommitted_dirty);
    drop(coordinator);

    let mut log = log.restart();
    log.reopen()?;
    assert_eq!(log.records().len(), 6);
    assert_eq!(log.durable_records().len(), 6);

    let mut reopened_coordinator = TransactionCoordinator::open(&mut log)?;
    let volatile_active = reopened_coordinator.begin()?;
    let volatile_page = unlogged_page(log.lineage(), 80, 40, [9, 10])?;
    let (volatile_active, volatile_dirty) =
        reopened_coordinator.stage_page_write(volatile_active, volatile_page, &mut log)?;
    assert_eq!(volatile_dirty.required_position().get(), 8);
    log.flush_through(volatile_dirty.required_position())?;
    log.arm_fault(FaultPoint::BeforeFlush)?;
    let volatile_commit = reopened_coordinator
        .commit(volatile_active, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("reopened volatile commit unexpectedly became durable"))?;
    assert!(matches!(
        volatile_commit,
        CoordinatedCommitError::Indeterminate(_)
    ));
    assert_eq!(log.records().len(), 8);
    assert_eq!(log.durable_records().len(), 7);
    assert_eq!(log.records()[7].position().get(), 9);
    drop(volatile_dirty);

    Ok(RecoveryScenario {
        log,
        behind_store,
        exact_store,
        missing_store,
        raw_store,
        page_number,
        latest_owner,
    })
}

fn assert_latest_target(
    scenario: &RecoveryScenario,
    target: &CommittedTransactionPageRecoveryTarget<2>,
) {
    assert!(
        target
            .transaction()
            .matches_transaction_id(scenario.latest_owner)
    );
    assert_eq!(target.page_number(), scenario.page_number);
    assert_eq!(target.page_version(), PageVersion::new(1));
    assert_eq!(target.bytes(), &[3, 4]);
    assert_eq!(target.page_position().get(), 3);
    assert_eq!(target.commit_position().get(), 4);
    assert!(
        target
            .page_position()
            .lineage()
            .same_lineage(scenario.log.lineage())
    );
    assert!(
        target
            .commit_position()
            .lineage()
            .same_lineage(scenario.log.lineage())
    );
}

fn assert_recovered_page(
    scenario: &RecoveryScenario,
    store: &InMemoryPageStore<2>,
) -> Result<(), Box<dyn Error>> {
    let stored = store
        .page(scenario.page_number)
        .ok_or_else(|| io::Error::other("recovered memory page is missing"))?;
    assert_eq!(stored.page_number(), scenario.page_number);
    assert_eq!(stored.page_version(), PageVersion::new(1));
    assert_eq!(stored.bytes(), &[3, 4]);
    assert_eq!(stored.required_position().get(), 3);
    assert!(
        stored
            .required_position()
            .lineage()
            .same_lineage(scenario.log.lineage())
    );
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
