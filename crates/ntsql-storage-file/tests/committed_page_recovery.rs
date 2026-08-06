use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_page::{
    DurablePageWalObservation, PageAddress, PageImage, PageNumber, PageVersion, flush_dirty_page,
    stage_page_write,
};
use ntsql_storage_file::{
    FaultPoint, FileCommitLog, FileCommittedPageRecoveryInventoryError,
    FileCommittedPageRecoverySourceError, FileCommittedPageRecoveryStoreError, FileIoStage,
    FileOpenError, FilePageStore, FilePageStoreError, FileTransactionPageStorageOpenError,
    PageStoreFaultPoint, PageStoreIoStage, PageStoreOpenError, open_transaction_page_storage,
};
use ntsql_transaction::{
    CommittedTransactionPageRecoveryError, CommittedTransactionPageRecoveryOutcome,
    CommittedTransactionPageRecoverySourceState, CommittedTransactionPageRecoveryTarget,
    CommittedTransactionPagesRecoveryError, CoordinatedCommitError,
    DurableTransactionCommitObservation, DurableTransactionPageObservation,
    DurableTransactionPageRecoveryInventory, DurableTransactionRestartAnalysisError,
    DurableTransactionRestartObservation, DurableTransactionRestartState, TransactionCoordinator,
    TransactionId, UnrecoveredTransactionPageStorage, flush_committed_page,
    recover_committed_transaction_page, recover_committed_transaction_pages,
};
use ntsql_wal::{LogDurability, LogLineage, LogSequenceNumber, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct RecoveryScenario {
    log: FileCommitLog<2>,
    behind_store: FilePageStore<2>,
    exact_store: FilePageStore<2>,
    missing_store: FilePageStore<2>,
    raw_store: FilePageStore<2>,
    page_number: PageNumber,
    latest_owner: TransactionId,
    behind_store_path: PathBuf,
    missing_store_path: PathBuf,
    _directory: TestDirectory,
}

struct FailingRestartFileSource(FileCommitLog<1>);

impl DurableTransactionPageRecoveryInventory<1> for FailingRestartFileSource {
    type Error = FileCommittedPageRecoveryInventoryError;

    fn durable_transaction_page_numbers(&mut self) -> Result<Vec<PageNumber>, Self::Error> {
        self.0.durable_transaction_page_numbers()
    }
}

impl ntsql_transaction::DurableTransactionPageRecoverySource<1> for FailingRestartFileSource {
    type Error = FileCommittedPageRecoverySourceError<1>;

    fn lineage(&self) -> &LogLineage {
        <FileCommitLog<1> as ntsql_transaction::DurableTransactionPageRecoverySource<1>>::lineage(
            &self.0,
        )
    }

    fn with_durable_page_evidence<Output, Operation>(
        &mut self,
        page_number: PageNumber,
        operation: Operation,
    ) -> Result<Output, Self::Error>
    where
        Operation: for<'evidence> FnOnce(
            &'evidence [DurablePageWalObservation<1>],
            &'evidence [DurableTransactionPageObservation<1>],
            &'evidence [DurableTransactionCommitObservation],
        ) -> Output,
    {
        ntsql_transaction::DurableTransactionPageRecoverySource::with_durable_page_evidence(
            &mut self.0,
            page_number,
            operation,
        )
    }
}

impl ntsql_transaction::DurableTransactionRestartAnalysisSource<1> for FailingRestartFileSource {
    type Error = io::Error;

    fn lineage(&self) -> &LogLineage {
        <FileCommitLog<1> as ntsql_transaction::DurableTransactionRestartAnalysisSource<1>>::lineage(
            &self.0,
        )
    }

    fn with_durable_transaction_restart_observations<Output, Operation>(
        &mut self,
        _operation: Operation,
    ) -> Result<Output, Self::Error>
    where
        Operation: for<'evidence> FnOnce(
            Option<&'evidence LogSequenceNumber>,
            &'evidence [DurableTransactionRestartObservation<1>],
        ) -> Output,
    {
        Err(io::Error::other("injected restart-analysis failure"))
    }
}

#[test]
fn recovery_source_excludes_volatile_suffix_and_no_write_paths_preserve_faults()
-> Result<(), Box<dyn Error>> {
    let mut scenario = recovery_scenario()?;
    let page_number = scenario.page_number;

    let (physical, owned, commits) =
        <FileCommitLog<2> as ntsql_transaction::DurableTransactionPageRecoverySource<2>>::with_durable_page_evidence(
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
    assert_eq!(physical, [1, 3, 5, 6, 7]);
    assert_eq!(owned, [1, 3, 5, 7]);
    assert_eq!(commits, [2, 4]);
    assert_eq!(scenario.log.records().len(), 8);
    assert_eq!(scenario.log.durable_records().len(), 7);
    assert!(scenario.log.records()[7].transaction_epoch().is_some());
    assert_eq!(scenario.log.records()[7].position().get(), 8);

    scenario
        .exact_store
        .arm_fault(PageStoreFaultPoint::BeforeWrite)?;
    let exact = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.exact_store,
        page_number,
    )?;
    let CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { target } = exact else {
        return Err(io::Error::other("exact filesystem store attempted recovery").into());
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
        .ok_or_else(|| io::Error::other("raw-backed filesystem page disappeared"))?;
    assert_eq!(raw_page.page_version(), PageVersion::new(30));
    assert_eq!(raw_page.bytes(), &[7, 8]);
    assert_eq!(raw_page.required_position().get(), 6);
    Ok(())
}

#[test]
fn filesystem_gate_recovers_missing_and_behind_and_persists_lower_version_target()
-> Result<(), Box<dyn Error>> {
    let mut scenario = recovery_scenario()?;
    let page_number = scenario.page_number;

    let behind = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.behind_store,
        page_number,
    )?;
    let CommittedTransactionPageRecoveryOutcome::Recovered { target } = behind else {
        return Err(io::Error::other("behind filesystem store was not recovered").into());
    };
    assert_latest_target(&scenario, &target);
    assert_recovered_page(&scenario.log, &scenario.behind_store, page_number, 2)?;

    let missing = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.missing_store,
        page_number,
    )?;
    let CommittedTransactionPageRecoveryOutcome::Recovered { target } = missing else {
        return Err(io::Error::other("missing filesystem store was not recovered").into());
    };
    assert_latest_target(&scenario, &target);
    assert_recovered_page(&scenario.log, &scenario.missing_store, page_number, 1)?;

    let behind_store_path = scenario.behind_store_path.clone();
    let missing_store_path = scenario.missing_store_path.clone();
    drop(scenario.behind_store);
    drop(scenario.missing_store);

    let behind_store = FilePageStore::<2>::open(&behind_store_path)?;
    let missing_store = FilePageStore::<2>::open(&missing_store_path)?;
    assert_recovered_page(&scenario.log, &behind_store, page_number, 2)?;
    assert_recovered_page(&scenario.log, &missing_store, page_number, 1)?;
    Ok(())
}

#[test]
fn filesystem_recovery_faults_require_fresh_authoritative_reruns() -> Result<(), Box<dyn Error>> {
    let mut scenario = recovery_scenario()?;
    let page_number = scenario.page_number;
    scenario
        .missing_store
        .arm_fault(PageStoreFaultPoint::BeforeWrite)?;

    let before = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.missing_store,
        page_number,
    );
    let Err(CommittedTransactionPageRecoveryError::StoreWrite { state }) = before else {
        return Err(io::Error::other("before-write recovery fault was not terminal").into());
    };
    assert_eq!(
        state.as_ref().cause(),
        &FileCommittedPageRecoveryStoreError::PageStore(FilePageStoreError::InjectedFault(
            PageStoreFaultPoint::BeforeWrite
        ))
    );
    assert!(matches!(
        state.source_state(),
        CommittedTransactionPageRecoverySourceState::StoreMissing {
            page_number: source_page,
            target_page_position,
        } if *source_page == page_number && target_page_position.get() == 3
    ));
    assert_latest_target(&scenario, state.target());
    assert!(scenario.missing_store.page(page_number).is_none());

    let retry = recover_committed_transaction_page(
        &mut scenario.log,
        &mut scenario.missing_store,
        page_number,
    )?;
    assert!(matches!(
        retry,
        CommittedTransactionPageRecoveryOutcome::Recovered { .. }
    ));
    assert_recovered_page(&scenario.log, &scenario.missing_store, page_number, 1)?;

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
        &FileCommittedPageRecoveryStoreError::PageStore(FilePageStoreError::InjectedFault(
            PageStoreFaultPoint::AfterWrite
        ))
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
    assert_recovered_page(&scenario.log, &scenario.behind_store, page_number, 2)?;

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

    let behind_store_path = scenario.behind_store_path.clone();
    drop(scenario.behind_store);
    let behind_store = FilePageStore::<2>::open(&behind_store_path)?;
    assert_recovered_page(&scenario.log, &behind_store, page_number, 2)?;
    Ok(())
}

#[test]
fn filesystem_batch_recovery_is_sorted_fail_fast_idempotent_and_persistent()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("committed-pages-recovery")?;
    let log_path = directory.path().join("commit-log.bin");
    let store_path = directory.path().join("pages.bin");
    let persistent_id = persistent_log_id(520)?;
    let mut log =
        FileCommitLog::<1>::create_new_transaction_page_capable(&log_path, persistent_id)?;
    let mut store = FilePageStore::<1>::create_new(&store_path, persistent_id)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;

    let last_page = page_number(83)?;
    let behind_active = coordinator.begin()?;
    let behind_owner = behind_active.transaction_id();
    let behind_page = unlogged_page(log.lineage(), 83, 100, [0xA3])?;
    let (behind_active, behind_dirty) =
        coordinator.stage_page_write(behind_active, behind_page, &mut log)?;
    let behind_commit = coordinator.commit(behind_active, &mut log)?;
    flush_committed_page(&behind_commit, &mut log, &mut store, behind_dirty)?;

    let first_page = page_number(81)?;
    let exact_active = coordinator.begin()?;
    let exact_owner = exact_active.transaction_id();
    let exact_page = unlogged_page(log.lineage(), 81, 81, [0x81])?;
    let (exact_active, exact_dirty) =
        coordinator.stage_page_write(exact_active, exact_page, &mut log)?;
    let exact_commit = coordinator.commit(exact_active, &mut log)?;
    flush_committed_page(&exact_commit, &mut log, &mut store, exact_dirty)?;

    let failed_page = page_number(82)?;
    let missing_active = coordinator.begin()?;
    let missing_owner = missing_active.transaction_id();
    let missing_page = unlogged_page(log.lineage(), 82, 82, [0x82])?;
    let (missing_active, missing_dirty) =
        coordinator.stage_page_write(missing_active, missing_page, &mut log)?;
    coordinator.commit(missing_active, &mut log)?;
    drop(missing_dirty);

    let latest_active = coordinator.begin()?;
    let latest_owner = latest_active.transaction_id();
    let latest_page = unlogged_page(log.lineage(), 83, 1, [0x03])?;
    let (latest_active, latest_dirty) =
        coordinator.stage_page_write(latest_active, latest_page, &mut log)?;
    coordinator.commit(latest_active, &mut log)?;
    drop(latest_dirty);

    let uncommitted_page_number = page_number(84)?;
    let uncommitted_active = coordinator.begin()?;
    let uncommitted_owner = uncommitted_active.transaction_id();
    let uncommitted_page = unlogged_page(log.lineage(), 84, 84, [0x84])?;
    let (uncommitted_active, uncommitted_dirty) =
        coordinator.stage_page_write(uncommitted_active, uncommitted_page, &mut log)?;
    log.flush_through(uncommitted_dirty.required_position())?;
    drop(uncommitted_active);
    drop(uncommitted_dirty);

    let raw_page_number = page_number(85)?;
    let raw_page = unlogged_page(log.lineage(), 85, 85, [0x85])?;
    let raw_dirty = stage_page_write(&mut log, raw_page)?;
    log.flush_through(raw_dirty.required_position())?;
    drop(raw_dirty);

    let volatile_page_number = page_number(86)?;
    let volatile_active = coordinator.begin()?;
    let volatile_page = unlogged_page(log.lineage(), 86, 86, [0x86])?;
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
    let expected_restart_lineage = log.lineage().clone();
    let expected_restart_frontier = log
        .durable_position()
        .ok_or_else(|| io::Error::other("filesystem restart frontier is missing"))?;
    assert_eq!(
        log.records()[10]
            .transaction_page_write()
            .map(|record| record.page_write().page_number()),
        Some(volatile_page_number)
    );
    assert!(log.records()[11].transaction_epoch().is_some());
    assert!(log.records()[11].transaction_page_write().is_none());
    assert_eq!(
        log.durable_transaction_page_numbers()?,
        [first_page, failed_page, last_page, uncommitted_page_number]
    );

    let behind = store
        .page(last_page)
        .ok_or_else(|| io::Error::other("behind filesystem page disappeared"))?;
    assert_eq!(behind.page_version(), PageVersion::new(100));
    assert_eq!(behind.bytes(), &[0xA3]);
    store.arm_fault(PageStoreFaultPoint::BeforeWrite)?;

    let failure = UnrecoveredTransactionPageStorage::new(log, store)
        .recover()
        .err()
        .ok_or_else(|| io::Error::other("owning filesystem batch unexpectedly succeeded"))?;
    let CommittedTransactionPagesRecoveryError::Page {
        completed,
        page_number,
        source: CommittedTransactionPageRecoveryError::StoreWrite { state: write_state },
    } = failure.error()
    else {
        return Err(io::Error::other("filesystem batch did not stop at the fault").into());
    };
    assert_eq!(*page_number, failed_page);
    assert_eq!(completed.pages().len(), 1);
    assert_eq!(completed.pages()[0].page_number(), first_page);
    assert_eq!(
        write_state.as_ref().cause(),
        &FileCommittedPageRecoveryStoreError::PageStore(FilePageStoreError::InjectedFault(
            PageStoreFaultPoint::BeforeWrite
        ))
    );

    let page_recovered = failure.retry()?;
    assert_transaction_page_storage_wal_locked(
        open_transaction_page_storage::<1, _, _>(&log_path, &store_path)
            .err()
            .ok_or_else(|| io::Error::other("page-recovered owner released the WAL lock"))?,
    )?;
    let mut recovered = page_recovered.analyze_restart()?;
    assert_transaction_page_storage_wal_locked(
        open_transaction_page_storage::<1, _, _>(&log_path, &store_path)
            .err()
            .ok_or_else(|| io::Error::other("analyzed owner released the WAL lock"))?,
    )?;
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
    let (log, store) = recovered.parts_mut();
    let recovered_behind = store
        .page(last_page)
        .ok_or_else(|| io::Error::other("behind filesystem page was not recovered"))?;
    assert_eq!(recovered_behind.page_version(), PageVersion::new(1));
    assert_eq!(recovered_behind.bytes(), &[0x03]);
    let sequences = [
        store
            .page(first_page)
            .ok_or_else(|| io::Error::other("exact page disappeared"))?
            .store_sequence(),
        store
            .page(failed_page)
            .ok_or_else(|| io::Error::other("missing page was not recovered"))?
            .store_sequence(),
        recovered_behind.store_sequence(),
    ];

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
    assert_eq!(
        [
            store
                .page(first_page)
                .ok_or_else(|| io::Error::other("exact page disappeared after idempotent run"))?
                .store_sequence(),
            store
                .page(failed_page)
                .ok_or_else(|| io::Error::other("missing page disappeared after idempotent run"))?
                .store_sequence(),
            store
                .page(last_page)
                .ok_or_else(|| io::Error::other("behind page disappeared after idempotent run"))?
                .store_sequence(),
        ],
        sequences
    );
    assert!(store.page(uncommitted_page_number).is_none());
    assert!(store.page(raw_page_number).is_none());
    assert!(store.page(volatile_page_number).is_none());

    let (_log, store, report, analysis) = recovered.into_parts();
    assert_eq!(report.pages().len(), 4);
    assert_eq!(
        analysis.durable_frontier(),
        Some(&expected_restart_frontier)
    );
    drop(store);

    let reopened = FilePageStore::<1>::open(&store_path)?;
    for (number, version, bytes) in [
        (first_page, 81, [0x81]),
        (failed_page, 82, [0x82]),
        (last_page, 1, [0x03]),
    ] {
        let page = reopened
            .page(number)
            .ok_or_else(|| io::Error::other("recovered filesystem page is missing"))?;
        assert_eq!(page.page_version(), PageVersion::new(version));
        assert_eq!(page.bytes(), &bytes);
    }
    assert!(reopened.page(uncommitted_page_number).is_none());
    assert!(reopened.page(raw_page_number).is_none());
    assert!(reopened.page(volatile_page_number).is_none());
    Ok(())
}

#[test]
fn filesystem_pair_open_reports_stage_and_releases_first_lock_on_second_failure()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("transaction-page-storage-open")?;
    let log_path = directory.path().join("commit-log.bin");
    let store_path = directory.path().join("pages.bin");
    let missing_log_path = directory.path().join("missing-log.bin");
    let missing_store_path = directory.path().join("missing-pages.bin");
    let persistent_id = persistent_log_id(521)?;
    drop(FileCommitLog::<1>::create_new_transaction_page_capable(
        &log_path,
        persistent_id,
    )?);
    drop(FilePageStore::<1>::create_new(&store_path, persistent_id)?);

    let held_store = FilePageStore::<1>::open(&store_path)?;
    assert!(matches!(
        open_transaction_page_storage::<1, _, _>(&missing_log_path, &store_path),
        Err(FileTransactionPageStorageOpenError::CommitLog(_))
    ));
    drop(held_store);

    assert!(matches!(
        open_transaction_page_storage::<1, _, _>(&log_path, &missing_store_path),
        Err(FileTransactionPageStorageOpenError::PageStore(_))
    ));
    let reopened_log = FileCommitLog::<1>::open_transaction_page_capable(&log_path)?;
    drop(reopened_log);
    Ok(())
}

#[test]
fn filesystem_pair_recovery_releases_live_parts_and_reopens_exact() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("transaction-page-storage-owner")?;
    let log_path = directory.path().join("commit-log.bin");
    let store_path = directory.path().join("pages.bin");
    let persistent_id = persistent_log_id(522)?;
    let first_page = page_number(90)?;
    let second_page = page_number(91)?;
    let mut log =
        FileCommitLog::<1>::create_new_transaction_page_capable(&log_path, persistent_id)?;
    let store = FilePageStore::<1>::create_new(&store_path, persistent_id)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let active = coordinator.begin()?;
    let first_owner = active.transaction_id();
    let page = unlogged_page(log.lineage(), 90, 9, [0x90])?;
    let (active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
    coordinator.commit(active, &mut log)?;
    drop(dirty);
    drop(coordinator);
    drop(log);
    drop(store);

    let page_recovered =
        open_transaction_page_storage::<1, _, _>(&log_path, &store_path)?.recover()?;
    assert_transaction_page_storage_wal_locked(
        open_transaction_page_storage::<1, _, _>(&log_path, &store_path)
            .err()
            .ok_or_else(|| io::Error::other("page-recovered pair released its WAL lock"))?,
    )?;
    assert_page_store_locked(
        FilePageStore::<1>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("page-recovered pair released its page-store lock"))?,
    )?;
    let mut recovered = page_recovered.analyze_restart()?;
    assert_transaction_page_storage_wal_locked(
        open_transaction_page_storage::<1, _, _>(&log_path, &store_path)
            .err()
            .ok_or_else(|| io::Error::other("analyzed pair released its WAL lock"))?,
    )?;
    assert_page_store_locked(
        FilePageStore::<1>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("analyzed pair released its page-store lock"))?,
    )?;
    assert_eq!(recovered.recovery_report().pages().len(), 1);
    assert!(matches!(
        recovered.recovery_report().pages()[0],
        CommittedTransactionPageRecoveryOutcome::Recovered { .. }
    ));
    assert_eq!(
        recovered
            .restart_analysis()
            .durable_frontier()
            .map(|position| position.get()),
        Some(2)
    );
    assert_eq!(recovered.restart_analysis().transactions().len(), 1);
    let first_entry = &recovered.restart_analysis().transactions()[0];
    assert!(
        first_entry
            .transaction()
            .matches_transaction_id(first_owner)
    );
    assert_eq!(
        first_entry
            .first_owned_page_position()
            .map(LogSequenceNumber::get),
        Some(1)
    );
    assert_eq!(
        first_entry
            .last_owned_page_position()
            .map(LogSequenceNumber::get),
        Some(1)
    );
    assert_eq!(first_entry.owned_page_record_count(), 1);
    assert_eq!(
        first_entry
            .state()
            .commit_position()
            .map(LogSequenceNumber::get),
        Some(2)
    );
    let second_owner;
    {
        let (log, store) = recovered.parts_mut();
        let stored = store
            .page(first_page)
            .ok_or_else(|| io::Error::other("startup recovery did not release the first page"))?;
        assert_eq!(stored.page_version(), PageVersion::new(9));
        assert_eq!(stored.bytes(), &[0x90]);

        let mut coordinator = TransactionCoordinator::open(log)?;
        let active = coordinator.begin()?;
        second_owner = active.transaction_id();
        let page = unlogged_page(log.lineage(), 91, 10, [0x91])?;
        let (active, dirty) = coordinator.stage_page_write(active, page, log)?;
        let committed = coordinator.commit(active, log)?;
        flush_committed_page(&committed, log, store, dirty)?;
    }
    let (log, store, report, analysis) = recovered.into_parts();
    assert_eq!(report.pages().len(), 1);
    assert_eq!(
        analysis.durable_frontier().map(|position| position.get()),
        Some(2)
    );
    drop(log);
    drop(store);

    let reopened = open_transaction_page_storage::<1, _, _>(&log_path, &store_path)?
        .recover()?
        .analyze_restart()?;
    assert_eq!(
        reopened
            .recovery_report()
            .pages()
            .iter()
            .map(CommittedTransactionPageRecoveryOutcome::page_number)
            .collect::<Vec<_>>(),
        [first_page, second_page]
    );
    assert!(
        reopened
            .recovery_report()
            .pages()
            .iter()
            .all(|outcome| matches!(
                outcome,
                CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
            ))
    );
    assert_eq!(
        reopened
            .restart_analysis()
            .durable_frontier()
            .map(|position| position.get()),
        Some(4)
    );
    assert_eq!(reopened.restart_analysis().transactions().len(), 2);
    for (entry, (owner, page_position, commit_position)) in reopened
        .restart_analysis()
        .transactions()
        .iter()
        .zip([(first_owner, 1, 2), (second_owner, 3, 4)])
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
            Some(commit_position)
        );
    }
    let (_, store) = reopened.parts();
    let first = store
        .page(first_page)
        .ok_or_else(|| io::Error::other("first page disappeared after pair reopen"))?;
    let second = store
        .page(second_page)
        .ok_or_else(|| io::Error::other("live page disappeared after pair reopen"))?;
    assert_eq!(first.bytes(), &[0x90]);
    assert_eq!(second.bytes(), &[0x91]);
    Ok(())
}

#[test]
fn filesystem_failed_restart_analysis_retains_both_adapter_locks() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("failed-restart-owner-locks")?;
    let log_path = directory.path().join("commit-log.bin");
    let store_path = directory.path().join("pages.bin");
    let persistent_id = persistent_log_id(523)?;
    let log = FileCommitLog::<1>::create_new_transaction_page_capable(&log_path, persistent_id)?;
    let store = FilePageStore::<1>::create_new(&store_path, persistent_id)?;

    let page_recovered =
        UnrecoveredTransactionPageStorage::new(FailingRestartFileSource(log), store).recover()?;
    let failure = page_recovered
        .analyze_restart()
        .err()
        .ok_or_else(|| io::Error::other("injected restart failure reached live storage"))?;
    assert!(failure.recovery_report().pages().is_empty());
    let DurableTransactionRestartAnalysisError::Source(source) = failure.error() else {
        return Err(io::Error::other("injected restart failure lost its source error").into());
    };
    assert_eq!(source.kind(), io::ErrorKind::Other);

    assert_commit_log_locked(
        FileCommitLog::<1>::open_transaction_page_capable(&log_path)
            .err()
            .ok_or_else(|| io::Error::other("failed analysis released the WAL lock"))?,
    )?;
    assert_page_store_locked(
        FilePageStore::<1>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("failed analysis released the page-store lock"))?,
    )?;

    drop(failure);
    let reopened_log = FileCommitLog::<1>::open_transaction_page_capable(&log_path)?;
    let reopened_store = FilePageStore::<1>::open(&store_path)?;
    drop(reopened_log);
    drop(reopened_store);
    Ok(())
}

#[test]
fn transaction_page_recovery_rejects_older_file_wal_formats_before_callback()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("unsupported-recovery")?;
    let page_number = page_number(90)?;
    let v1_path = directory.path().join("v1.bin");
    let v2_path = directory.path().join("v2.bin");
    let mut v1 = FileCommitLog::create_new(&v1_path, persistent_log_id(510)?)?;
    let mut v2 = FileCommitLog::<2>::create_new_page_capable(&v2_path, persistent_log_id(511)?)?;
    let mut v1_called = false;
    let mut v2_called = false;

    assert_eq!(
        v1.durable_transaction_page_numbers(),
        Err(
            FileCommittedPageRecoveryInventoryError::TransactionPageSupportUnavailable {
                version: 1
            }
        )
    );
    assert_eq!(
        v2.durable_transaction_page_numbers(),
        Err(
            FileCommittedPageRecoveryInventoryError::TransactionPageSupportUnavailable {
                version: 2
            }
        )
    );
    let v1_result = <FileCommitLog<0> as ntsql_transaction::DurableTransactionPageRecoverySource<
        0,
    >>::with_durable_page_evidence(&mut v1, page_number, |_, _, _| {
        v1_called = true;
    });
    let v2_result = <FileCommitLog<2> as ntsql_transaction::DurableTransactionPageRecoverySource<
        2,
    >>::with_durable_page_evidence(&mut v2, page_number, |_, _, _| {
        v2_called = true;
    });

    assert_eq!(
        v1_result,
        Err(FileCommittedPageRecoverySourceError::TransactionPageSupportUnavailable { version: 1 })
    );
    assert_eq!(
        v2_result,
        Err(FileCommittedPageRecoverySourceError::TransactionPageSupportUnavailable { version: 2 })
    );
    assert!(!v1_called);
    assert!(!v2_called);
    Ok(())
}

fn assert_transaction_page_storage_wal_locked(
    error: FileTransactionPageStorageOpenError,
) -> Result<(), io::Error> {
    let FileTransactionPageStorageOpenError::CommitLog(source) = error else {
        return Err(io::Error::other(
            "owned transaction-page WAL lock contention was not commit-log I/O",
        ));
    };
    assert_commit_log_locked(source)
}

fn assert_commit_log_locked(error: FileOpenError) -> Result<(), io::Error> {
    let FileOpenError::Io(source) = error else {
        return Err(io::Error::other("WAL lock contention was not I/O"));
    };
    if source.stage() != FileIoStage::AcquireExclusiveLock
        || source.io_source().kind() != io::ErrorKind::WouldBlock
    {
        return Err(io::Error::other(
            "owned transaction-page WAL lock contention had the wrong cause",
        ));
    }
    Ok(())
}

fn assert_page_store_locked(error: PageStoreOpenError) -> Result<(), io::Error> {
    let PageStoreOpenError::Io(source) = error else {
        return Err(io::Error::other("page-store lock contention was not I/O"));
    };
    if source.stage() != PageStoreIoStage::AcquireExclusiveLock
        || source.io_source().kind() != io::ErrorKind::WouldBlock
    {
        return Err(io::Error::other(
            "owned transaction-page page-store lock contention had the wrong cause",
        ));
    }
    Ok(())
}

fn recovery_scenario() -> Result<RecoveryScenario, Box<dyn Error>> {
    let directory = TestDirectory::new("committed-page-recovery")?;
    let log_path = directory.path().join("commit-log.bin");
    let behind_store_path = directory.path().join("behind-pages.bin");
    let exact_store_path = directory.path().join("exact-pages.bin");
    let missing_store_path = directory.path().join("missing-pages.bin");
    let raw_store_path = directory.path().join("raw-pages.bin");
    let persistent_id = persistent_log_id(501)?;
    let page_number = page_number(80)?;
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
    let mut behind_store = FilePageStore::<2>::create_new(&behind_store_path, persistent_id)?;
    let mut exact_store = FilePageStore::<2>::create_new(&exact_store_path, persistent_id)?;
    let missing_store = FilePageStore::<2>::create_new(&missing_store_path, persistent_id)?;
    let mut raw_store = FilePageStore::<2>::create_new(&raw_store_path, persistent_id)?;
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
    drop(uncommitted_active);
    drop(uncommitted_dirty);

    let raw_page = unlogged_page(log.lineage(), 80, 30, [7, 8])?;
    let raw_dirty = stage_page_write(&mut log, raw_page)?;
    flush_dirty_page(&mut log, &mut raw_store, raw_dirty)?;
    drop(coordinator);
    drop(behind_store);
    drop(exact_store);
    drop(missing_store);
    drop(raw_store);
    drop(log);

    let mut log = FileCommitLog::<2>::open_transaction_page_capable(&log_path)?;
    let behind_store = FilePageStore::<2>::open(&behind_store_path)?;
    let exact_store = FilePageStore::<2>::open(&exact_store_path)?;
    let missing_store = FilePageStore::<2>::open(&missing_store_path)?;
    let raw_store = FilePageStore::<2>::open(&raw_store_path)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let volatile_active = coordinator.begin()?;
    let volatile_page = unlogged_page(log.lineage(), 80, 40, [9, 10])?;
    let (volatile_active, volatile_dirty) =
        coordinator.stage_page_write(volatile_active, volatile_page, &mut log)?;
    assert_eq!(volatile_dirty.required_position().get(), 7);
    log.flush_through(volatile_dirty.required_position())?;
    log.arm_fault(FaultPoint::BeforeFlush)?;
    let volatile_commit = coordinator
        .commit(volatile_active, &mut log)
        .err()
        .ok_or_else(|| {
            io::Error::other("volatile filesystem commit unexpectedly became durable")
        })?;
    assert!(matches!(
        volatile_commit,
        CoordinatedCommitError::Indeterminate(_)
    ));
    assert_eq!(log.records().len(), 8);
    assert_eq!(log.durable_records().len(), 7);
    drop(volatile_dirty);

    Ok(RecoveryScenario {
        log,
        behind_store,
        exact_store,
        missing_store,
        raw_store,
        page_number,
        latest_owner,
        behind_store_path,
        missing_store_path,
        _directory: directory,
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
    log: &FileCommitLog<2>,
    store: &FilePageStore<2>,
    page_number: PageNumber,
    expected_store_sequence: u64,
) -> Result<(), Box<dyn Error>> {
    let stored = store
        .page(page_number)
        .ok_or_else(|| io::Error::other("recovered filesystem page is missing"))?;
    assert_eq!(stored.page_number(), page_number);
    assert_eq!(stored.page_version(), PageVersion::new(1));
    assert_eq!(stored.bytes(), &[3, 4]);
    assert_eq!(stored.required_position().get(), 3);
    assert_eq!(stored.store_sequence(), expected_store_sequence);
    assert!(
        stored
            .required_position()
            .lineage()
            .same_lineage(log.lineage())
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

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(prefix: &str) -> Result<Self, io::Error> {
        let root = match std::env::var_os("CARGO_TARGET_TMPDIR") {
            Some(path) => PathBuf::from(path),
            None => std::env::current_dir()?
                .join("target")
                .join("ntsql-storage-file-integration-tests"),
        };
        fs::create_dir_all(&root)?;
        let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("{prefix}-{}-{unique}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
