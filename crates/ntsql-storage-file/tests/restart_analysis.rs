use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_page::{PageAddress, PageImage, PageNumber, PageVersion, UnloggedPage, stage_page_write};
use ntsql_storage_file::{
    FaultPoint, FileCommitLog, FileIoStage, FileOpenError, FilePageStore, PageStoreIoStage,
    PageStoreOpenError, open_transaction_page_storage,
};
use ntsql_transaction::{
    CoordinatedCommitError, DurableTransactionRestartAnalysis, DurableTransactionRestartEntry,
    DurableTransactionRestartObservation, DurableTransactionRestartObservationKind,
    DurableTransactionRestartPageState, DurableTransactionRestartReplayStart,
    DurableTransactionRestartReplayStartCause, DurableTransactionRestartRequiredPageImage,
    DurableTransactionRestartState, TransactionCoordinator, TransactionId,
    analyze_durable_transaction_restart, flush_committed_page,
};
use ntsql_wal::{LogDurability, LogLineage, LogSequenceNumber, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[test]
fn empty_v3_prefix_has_no_frontier_or_transactions() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("empty-restart-analysis")?;
    let path = directory.path().join("commit-log.bin");
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&path, persistent_log_id(1240)?)?;
    let lineage = log.lineage().clone();
    let mut callbacks = 0;

    <FileCommitLog<2> as ntsql_transaction::DurableTransactionRestartAnalysisSource<
        2,
    >>::with_durable_transaction_restart_observations(&mut log, |frontier, observations| {
        callbacks += 1;
        assert_eq!(frontier, None);
        assert!(observations.is_empty());
    })?;

    assert_eq!(callbacks, 1);
    let analysis = analyze_durable_transaction_restart(&mut log)?;
    assert!(analysis.lineage().same_lineage(&lineage));
    assert_eq!(analysis.durable_frontier(), None);
    assert!(analysis.transactions().is_empty());
    Ok(())
}

#[test]
fn reopened_v1_and_v2_prefixes_remain_format_relative() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("older-format-restart-analysis")?;
    let v1_path = directory.path().join("v1.bin");
    let v2_path = directory.path().join("v2.bin");

    let mut v1 = FileCommitLog::create_new(&v1_path, persistent_log_id(1241)?)?;
    let mut v1_coordinator = TransactionCoordinator::open(&mut v1)?;
    let v1_active = v1_coordinator.begin()?;
    let v1_transaction = v1_active.transaction_id();
    let v1_commit = v1_coordinator.commit(v1_active, &mut v1)?;
    assert_eq!(v1_commit.log_position().get(), 1);
    drop(v1_coordinator);
    drop(v1);

    let mut v1 = FileCommitLog::open(&v1_path)?;
    <FileCommitLog<0> as ntsql_transaction::DurableTransactionRestartAnalysisSource<
        0,
    >>::with_durable_transaction_restart_observations(&mut v1, |frontier, observations| {
        assert_eq!(frontier.map(|position| position.get()), Some(1));
        assert_eq!(observations.len(), 1);
        assert_commit(&observations[0], v1_transaction)?;
        Ok::<(), io::Error>(())
    })??;
    let v1_analysis = analyze_durable_transaction_restart(&mut v1)?;
    assert_commit_only_analysis(&v1_analysis, v1.lineage(), v1_transaction, 1);

    let mut v2 = FileCommitLog::<2>::create_new_page_capable(&v2_path, persistent_log_id(1242)?)?;
    let mut v2_coordinator = TransactionCoordinator::open(&mut v2)?;
    let v2_active = v2_coordinator.begin()?;
    let v2_transaction = v2_active.transaction_id();
    let raw_page = unlogged_page(v2.lineage(), 31, 8, [8, 9])?;
    let raw_dirty = stage_page_write(&mut v2, raw_page)?;
    assert_eq!(raw_dirty.required_position().get(), 1);
    let v2_commit = v2_coordinator.commit(v2_active, &mut v2)?;
    assert_eq!(v2_commit.log_position().get(), 2);
    drop((raw_dirty, v2_commit, v2_coordinator));
    drop(v2);

    let mut v2 = FileCommitLog::<2>::open_page_capable(&v2_path)?;
    <FileCommitLog<2> as ntsql_transaction::DurableTransactionRestartAnalysisSource<
        2,
    >>::with_durable_transaction_restart_observations(&mut v2, |frontier, observations| {
        assert_eq!(frontier.map(|position| position.get()), Some(2));
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.kind())
                .collect::<Vec<_>>(),
            [
                DurableTransactionRestartObservationKind::Page,
                DurableTransactionRestartObservationKind::Commit,
            ]
        );
        assert_raw_page(&observations[0], 31, 8, [8, 9])?;
        assert_commit(&observations[1], v2_transaction)?;
        Ok::<(), io::Error>(())
    })??;
    let v2_analysis = analyze_durable_transaction_restart(&mut v2)?;
    assert_commit_only_analysis(&v2_analysis, v2.lineage(), v2_transaction, 2);
    Ok(())
}

#[test]
fn reopened_v3_prefix_stays_locked_and_excludes_unmarked_commit() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("v3-restart-analysis")?;
    let path = directory.path().join("commit-log.bin");
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&path, persistent_log_id(1243)?)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;

    let committed_with_page = coordinator.begin()?;
    let committed_with_page_id = committed_with_page.transaction_id();
    let (committed_with_page, first_owned) = coordinator.stage_page_write(
        committed_with_page,
        unlogged_page(log.lineage(), 21, 3, [1, 1])?,
        &mut log,
    )?;
    assert_eq!(first_owned.required_position().get(), 1);

    let middle_raw_page = unlogged_page(log.lineage(), 22, 4, [2, 2])?;
    let middle_raw = stage_page_write(&mut log, middle_raw_page)?;
    assert_eq!(middle_raw.required_position().get(), 2);

    let committed = coordinator.commit(committed_with_page, &mut log)?;
    assert_eq!(committed.log_position().get(), 3);

    let uncommitted = coordinator.begin()?;
    let uncommitted_id = uncommitted.transaction_id();
    let (uncommitted, second_owned) = coordinator.stage_page_write(
        uncommitted,
        unlogged_page(log.lineage(), 23, 5, [3, 3])?,
        &mut log,
    )?;
    assert_eq!(second_owned.required_position().get(), 4);
    log.flush_through(second_owned.required_position())?;
    drop(uncommitted);

    let commit_only = coordinator.begin()?;
    let commit_only_id = commit_only.transaction_id();
    let commit_only = coordinator.commit(commit_only, &mut log)?;
    assert_eq!(commit_only.log_position().get(), 5);

    let tail_raw_page = unlogged_page(log.lineage(), 24, 6, [4, 4])?;
    let tail_raw = stage_page_write(&mut log, tail_raw_page)?;
    assert_eq!(tail_raw.required_position().get(), 6);
    log.flush_through(tail_raw.required_position())?;

    drop((
        first_owned,
        middle_raw,
        committed,
        second_owned,
        commit_only,
        tail_raw,
        coordinator,
    ));
    drop(log);

    let mut log = FileCommitLog::<2>::open_transaction_page_capable(&path)?;
    assert_eq!(log.records().len(), 6);
    assert_eq!(log.durable_records().len(), 6);
    let mut reopened_coordinator = TransactionCoordinator::open(&mut log)?;
    let volatile = reopened_coordinator.begin()?;
    let volatile_id = volatile.transaction_id();
    let (volatile, volatile_dirty) = reopened_coordinator.stage_page_write(
        volatile,
        unlogged_page(log.lineage(), 25, 7, [5, 5])?,
        &mut log,
    )?;
    assert_eq!(volatile_dirty.required_position().get(), 7);
    log.flush_through(volatile_dirty.required_position())?;
    log.arm_fault(FaultPoint::BeforeFlush)?;
    let volatile_commit = reopened_coordinator
        .commit(volatile, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("unmarked filesystem commit became durable"))?;
    assert!(matches!(
        volatile_commit,
        CoordinatedCommitError::Indeterminate(_)
    ));
    assert_eq!(log.records().len(), 8);
    assert_eq!(log.durable_records().len(), 7);
    assert_eq!(log.records()[7].position().get(), 8);
    assert_eq!(
        log.records()[7].transaction_epoch(),
        Some(volatile_id.epoch().get())
    );

    let lineage = log.lineage().clone();
    let mut callbacks = 0;
    <FileCommitLog<2> as ntsql_transaction::DurableTransactionRestartAnalysisSource<
        2,
    >>::with_durable_transaction_restart_observations(
        &mut log,
        |frontier, observations| -> Result<(), io::Error> {
            callbacks += 1;
            assert_second_opener_is_locked(&path)?;
            assert_eq!(frontier.map(|position| position.get()), Some(7));
            assert_eq!(
                observations
                    .iter()
                    .map(|observation| observation.kind())
                    .collect::<Vec<_>>(),
                [
                    DurableTransactionRestartObservationKind::TransactionPage,
                    DurableTransactionRestartObservationKind::Page,
                    DurableTransactionRestartObservationKind::Commit,
                    DurableTransactionRestartObservationKind::TransactionPage,
                    DurableTransactionRestartObservationKind::Commit,
                    DurableTransactionRestartObservationKind::Page,
                    DurableTransactionRestartObservationKind::TransactionPage,
                ]
            );
            assert_eq!(
                observations
                    .iter()
                    .map(|observation| observation.position().get())
                    .collect::<Vec<_>>(),
                [1, 2, 3, 4, 5, 6, 7]
            );
            assert_transaction_page(
                &observations[0],
                committed_with_page_id,
                21,
                3,
                [1, 1],
            )?;
            assert_raw_page(&observations[1], 22, 4, [2, 2])?;
            assert_commit(&observations[2], committed_with_page_id)?;
            assert_transaction_page(&observations[3], uncommitted_id, 23, 5, [3, 3])?;
            assert_commit(&observations[4], commit_only_id)?;
            assert_raw_page(&observations[5], 24, 6, [4, 4])?;
            assert_transaction_page(&observations[6], volatile_id, 25, 7, [5, 5])?;
            assert!(observations.iter().all(|observation| {
                observation
                    .position()
                    .lineage()
                    .same_lineage(&lineage)
            }));
            Ok(())
        },
    )??;
    assert_eq!(callbacks, 1);

    let analysis = analyze_durable_transaction_restart(&mut log)?;
    assert_v3_analysis(
        &analysis,
        &lineage,
        committed_with_page_id,
        uncommitted_id,
        commit_only_id,
        volatile_id,
    );

    drop((volatile_dirty, reopened_coordinator));
    drop(log);
    let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&path)?;
    assert_eq!(reopened.records().len(), 8);
    assert_eq!(reopened.durable_records().len(), 7);
    let reopened_analysis = analyze_durable_transaction_restart(&mut reopened)?;
    assert_v3_analysis(
        &reopened_analysis,
        reopened.lineage(),
        committed_with_page_id,
        uncommitted_id,
        commit_only_id,
        volatile_id,
    );
    Ok(())
}

#[test]
fn reopened_filesystem_owner_derives_page_completeness_without_store_mutation()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("restart-completeness")?;
    let log_path = directory.path().join("commit-log.bin");
    let store_path = directory.path().join("page-store.bin");
    let persistent_id = persistent_log_id(1244)?;
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
    let mut store = FilePageStore::<2>::create_new(&store_path, persistent_id)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let lineage = log.lineage().clone();

    let raw_page_number =
        PageNumber::new(10).ok_or_else(|| io::Error::other("raw page number is zero"))?;
    let raw_dirty = stage_page_write(
        &mut log,
        unlogged_page(&lineage, raw_page_number.get(), 1, [1, 0])?,
    )?;
    assert_eq!(raw_dirty.required_position().get(), 1);

    let uncommitted_page_number =
        PageNumber::new(20).ok_or_else(|| io::Error::other("uncommitted page number is zero"))?;
    let uncommitted = coordinator.begin()?;
    let uncommitted_id = uncommitted.transaction_id();
    let (uncommitted, uncommitted_dirty) = coordinator.stage_page_write(
        uncommitted,
        unlogged_page(&lineage, uncommitted_page_number.get(), 2, [2, 0])?,
        &mut log,
    )?;
    assert_eq!(uncommitted_dirty.required_position().get(), 2);

    let current_page_number =
        PageNumber::new(30).ok_or_else(|| io::Error::other("current page number is zero"))?;
    let current = coordinator.begin()?;
    let current_id = current.transaction_id();
    let (current, current_dirty) = coordinator.stage_page_write(
        current,
        unlogged_page(&lineage, current_page_number.get(), 3, [3, 0])?,
        &mut log,
    )?;
    assert_eq!(current_dirty.required_position().get(), 3);
    let current = coordinator.commit(current, &mut log)?;
    assert_eq!(current.log_position().get(), 4);
    flush_committed_page(&current, &mut log, &mut store, current_dirty)?;

    drop((
        raw_dirty,
        uncommitted,
        uncommitted_dirty,
        current,
        coordinator,
        log,
        store,
    ));

    let page_recovered = open_transaction_page_storage::<2, _, _>(&log_path, &store_path)?
        .recover()
        .map_err(|_| io::Error::other("filesystem page recovery failed"))?;
    let mut owner = page_recovered
        .analyze_restart()
        .map_err(|_| io::Error::other("filesystem restart analysis failed"))?;
    let page_count = owner.parts().1.pages().len();
    let store_sequence = owner
        .parts()
        .1
        .page(current_page_number)
        .ok_or_else(|| io::Error::other("current page disappeared"))?
        .store_sequence();

    let completeness = owner.analyze_current_restart_completeness()?;
    assert_eq!(
        completeness
            .transaction_analysis()
            .durable_frontier()
            .map(LogSequenceNumber::get),
        Some(4)
    );
    assert_eq!(
        completeness
            .pages()
            .iter()
            .map(|entry| entry.page_number())
            .collect::<Vec<_>>(),
        [
            raw_page_number,
            uncommitted_page_number,
            current_page_number
        ]
    );
    assert_eq!(
        completeness.pages()[0].state(),
        &DurableTransactionRestartPageState::StoreMissing {
            required: DurableTransactionRestartRequiredPageImage::Raw { page_position: 1 },
        }
    );
    assert_eq!(
        completeness.pages()[1].state(),
        &DurableTransactionRestartPageState::NoRequiredImage
    );
    assert!(matches!(
        completeness.pages()[2].state(),
        DurableTransactionRestartPageState::StoreCurrent {
            required: DurableTransactionRestartRequiredPageImage::CommittedTransaction {
                transaction,
                page_position: 3,
                commit_position: 4,
            },
            stored_position: 3,
        } if transaction.matches_transaction_id(current_id)
    ));
    assert_eq!(
        completeness.replay_start(),
        &DurableTransactionRestartReplayStart::AtPosition {
            position: 1,
            cause: DurableTransactionRestartReplayStartCause::StoreMissing {
                page_number: raw_page_number,
            },
        }
    );
    assert!(
        completeness
            .transaction_analysis()
            .transactions()
            .iter()
            .any(|entry| {
                entry.transaction().matches_transaction_id(uncommitted_id)
                    && matches!(entry.state(), DurableTransactionRestartState::Uncommitted)
            })
    );
    assert_second_opener_is_locked(&log_path)?;
    assert_page_store_is_locked(
        FilePageStore::<2>::open(&store_path).err().ok_or_else(|| {
            io::Error::other("completeness analysis released the page-store lock")
        })?,
    )?;
    assert_eq!(owner.parts().1.pages().len(), page_count);
    assert_eq!(
        owner
            .parts()
            .1
            .page(current_page_number)
            .ok_or_else(|| io::Error::other("current page changed during analysis"))?
            .store_sequence(),
        store_sequence
    );
    Ok(())
}

fn assert_commit_only_analysis(
    analysis: &DurableTransactionRestartAnalysis,
    lineage: &LogLineage,
    transaction: TransactionId,
    commit_position: u64,
) {
    assert!(analysis.lineage().same_lineage(lineage));
    assert_eq!(
        analysis.durable_frontier().map(|position| position.get()),
        Some(commit_position)
    );
    assert_eq!(analysis.transactions().len(), 1);
    assert_entry(
        &analysis.transactions()[0],
        transaction,
        None,
        None,
        0,
        Some(commit_position),
    );
}

fn assert_v3_analysis(
    analysis: &DurableTransactionRestartAnalysis,
    lineage: &LogLineage,
    committed_with_page: TransactionId,
    uncommitted: TransactionId,
    commit_only: TransactionId,
    volatile: TransactionId,
) {
    assert!(analysis.lineage().same_lineage(lineage));
    assert_eq!(
        analysis.durable_frontier().map(|position| position.get()),
        Some(7)
    );
    let transactions = analysis.transactions();
    assert_eq!(transactions.len(), 4);
    assert_entry(
        &transactions[0],
        committed_with_page,
        Some(1),
        Some(1),
        1,
        Some(3),
    );
    assert_entry(&transactions[1], uncommitted, Some(4), Some(4), 1, None);
    assert_entry(&transactions[2], commit_only, None, None, 0, Some(5));
    assert_entry(&transactions[3], volatile, Some(7), Some(7), 1, None);
}

fn assert_entry(
    entry: &DurableTransactionRestartEntry,
    transaction: TransactionId,
    first_page_position: Option<u64>,
    last_page_position: Option<u64>,
    page_count: usize,
    commit_position: Option<u64>,
) {
    assert_eq!(entry.transaction().epoch(), transaction.epoch().get());
    assert_eq!(entry.transaction().sequence(), transaction.sequence());
    assert_eq!(
        entry
            .first_owned_page_position()
            .map(|position| position.get()),
        first_page_position
    );
    assert_eq!(
        entry
            .last_owned_page_position()
            .map(|position| position.get()),
        last_page_position
    );
    assert_eq!(entry.owned_page_record_count(), page_count);
    assert_eq!(
        entry
            .state()
            .commit_position()
            .map(|position| position.get()),
        commit_position
    );
    if commit_position.is_none() {
        assert!(matches!(
            entry.state(),
            DurableTransactionRestartState::Uncommitted
        ));
    }
}

fn assert_transaction_page<const N: usize>(
    observation: &DurableTransactionRestartObservation<N>,
    owner: TransactionId,
    page_number: u64,
    page_version: u64,
    bytes: [u8; N],
) -> Result<(), io::Error> {
    match observation {
        DurableTransactionRestartObservation::TransactionPage(observation) => {
            assert_eq!(observation.owner().epoch(), owner.epoch().get());
            assert_eq!(observation.owner().sequence(), owner.sequence());
            assert_eq!(observation.page().page_number().get(), page_number);
            assert_eq!(observation.page().page_version().get(), page_version);
            assert_eq!(observation.page().image().bytes(), &bytes);
            Ok(())
        }
        DurableTransactionRestartObservation::Page(_)
        | DurableTransactionRestartObservation::Commit(_) => Err(io::Error::other(
            "expected one transaction-owned page observation",
        )),
    }
}

fn assert_raw_page<const N: usize>(
    observation: &DurableTransactionRestartObservation<N>,
    page_number: u64,
    page_version: u64,
    bytes: [u8; N],
) -> Result<(), io::Error> {
    match observation {
        DurableTransactionRestartObservation::Page(observation) => {
            assert_eq!(observation.page_number().get(), page_number);
            assert_eq!(observation.page_version().get(), page_version);
            assert_eq!(observation.image().bytes(), &bytes);
            Ok(())
        }
        DurableTransactionRestartObservation::TransactionPage(_)
        | DurableTransactionRestartObservation::Commit(_) => {
            Err(io::Error::other("expected one raw page observation"))
        }
    }
}

fn assert_commit<const N: usize>(
    observation: &DurableTransactionRestartObservation<N>,
    transaction: TransactionId,
) -> Result<(), io::Error> {
    match observation {
        DurableTransactionRestartObservation::Commit(observation) => {
            assert_eq!(observation.transaction().epoch(), transaction.epoch().get());
            assert_eq!(observation.transaction().sequence(), transaction.sequence());
            Ok(())
        }
        DurableTransactionRestartObservation::Page(_)
        | DurableTransactionRestartObservation::TransactionPage(_) => {
            Err(io::Error::other("expected one commit observation"))
        }
    }
}

fn assert_second_opener_is_locked(path: &Path) -> Result<(), io::Error> {
    let error = FileCommitLog::<2>::open_transaction_page_capable(path)
        .err()
        .ok_or_else(|| io::Error::other("second filesystem WAL opener acquired the lock"))?;
    let FileOpenError::Io(source) = error else {
        return Err(io::Error::other("second opener failure was not I/O"));
    };
    if source.stage() != FileIoStage::AcquireExclusiveLock {
        return Err(io::Error::other(
            "second opener failed outside the lock boundary",
        ));
    }
    if source.io_source().kind() != io::ErrorKind::WouldBlock {
        return Err(io::Error::other(
            "second opener lock failure was not nonblocking",
        ));
    }
    Ok(())
}

fn assert_page_store_is_locked(error: PageStoreOpenError) -> Result<(), io::Error> {
    let PageStoreOpenError::Io(source) = error else {
        return Err(io::Error::other(
            "second page-store opener failure was not I/O",
        ));
    };
    if source.stage() != PageStoreIoStage::AcquireExclusiveLock
        || source.io_source().kind() != io::ErrorKind::WouldBlock
    {
        return Err(io::Error::other(
            "second page-store opener failed outside the lock boundary",
        ));
    }
    Ok(())
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
) -> Result<UnloggedPage<N>, io::Error> {
    let page_number = PageNumber::new(number)
        .ok_or_else(|| io::Error::other("nonzero page number was rejected"))?;
    let image = PageImage::new(bytes).map_err(io::Error::other)?;
    Ok(UnloggedPage::new(
        PageAddress::new(lineage, page_number),
        PageVersion::new(version),
        image,
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
