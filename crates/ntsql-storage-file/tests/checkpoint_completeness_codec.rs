use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_page::{
    PageAddress, PageImage, PageNumber, PageVersion, UnloggedPage, flush_dirty_page,
    stage_page_write,
};
use ntsql_storage_file::{
    FileCommitLog, FilePageStore, decode_restart_checkpoint_completeness_baseline,
    encode_restart_checkpoint_completeness_baseline, open_transaction_page_storage,
};
use ntsql_transaction::{
    DurableTransactionRestartCheckpointCompletenessBaseline,
    DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation,
    TransactionCoordinator, flush_committed_page,
};
use ntsql_wal::{LogDurability, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[test]
fn empty_authoritative_baseline_has_exact_version_one_golden_bytes() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("empty-completeness-codec")?;
    let persistent_log_id = persistent_log_id(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_log_id)?;
    let store = FilePageStore::<2>::create_new(&store_path, persistent_log_id)?;
    let mut owner = ntsql_transaction::UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?;
    let baseline = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;

    let encoded = encode_restart_checkpoint_completeness_baseline(&baseline)?;
    let expected = [
        0x4e, 0x54, 0x53, 0x51, 0x43, 0x4d, 0x50, 0x31, 0x00, 0x01, 0x00, 0x80, 0x00, 0x40, 0x00,
        0x40, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
        0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4e, 0x54, 0x53, 0x51, 0x43, 0x4d, 0x45,
        0x31, 0x0b, 0xf7, 0xaa, 0x28, 0x7c, 0xa2, 0x1d, 0x9a,
    ];
    assert_eq!(encoded, expected);
    assert_eq!(
        encode_restart_checkpoint_completeness_baseline(&baseline)?,
        expected
    );

    let decoded = decode_restart_checkpoint_completeness_baseline(&encoded)?;
    assert_completeness_observation_matches_baseline(&decoded, &baseline);
    Ok(())
}

#[test]
fn full_round_trip_covers_transactions_page_states_and_required_images()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("full-completeness-codec")?;
    let log_path = directory.path().join("commit-log.bin");
    let store_path = directory.path().join("page-store.bin");
    let persistent_id = persistent_log_id(9001)?;
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
    let mut store = FilePageStore::<2>::create_new(&store_path, persistent_id)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let lineage = log.lineage().clone();

    // Page 101: raw image only, never flushed to the store -> StoreMissing/Raw,
    // and the earliest dirty floor, so it also drives the chosen replay cause.
    let missing_page_number =
        PageNumber::new(101).ok_or_else(|| io::Error::other("missing page number is zero"))?;
    let missing_dirty = stage_page_write(
        &mut log,
        unlogged_page(&lineage, missing_page_number.get(), 1, [0x01, 0x00])?,
    )?;
    assert_eq!(missing_dirty.required_position().get(), 1);

    // Page 102: an uncommitted transaction's owned page -> NoRequiredImage.
    let uncommitted_page_number =
        PageNumber::new(102).ok_or_else(|| io::Error::other("uncommitted page number is zero"))?;
    let uncommitted = coordinator.begin()?;
    let uncommitted_id = uncommitted.transaction_id();
    let (uncommitted, uncommitted_dirty) = coordinator.stage_page_write(
        uncommitted,
        unlogged_page(&lineage, uncommitted_page_number.get(), 2, [0x02, 0x00])?,
        &mut log,
    )?;
    assert_eq!(uncommitted_dirty.required_position().get(), 2);

    // Page 103: a committed transaction's owned page, flushed -> StoreCurrent
    // with a CommittedTransaction required image.
    let committed_page_number =
        PageNumber::new(103).ok_or_else(|| io::Error::other("committed page number is zero"))?;
    let committed = coordinator.begin()?;
    let committed_id = committed.transaction_id();
    let (committed, committed_dirty) = coordinator.stage_page_write(
        committed,
        unlogged_page(&lineage, committed_page_number.get(), 3, [0x03, 0x00])?,
        &mut log,
    )?;
    assert_eq!(committed_dirty.required_position().get(), 3);
    let committed = coordinator.commit(committed, &mut log)?;
    assert_eq!(committed.log_position().get(), 4);
    flush_committed_page(&committed, &mut log, &mut store, committed_dirty)?;

    // Page 104: raw image flushed, then a later raw image left unflushed ->
    // StoreBehind with a Raw required image.
    let behind_page_number =
        PageNumber::new(104).ok_or_else(|| io::Error::other("behind page number is zero"))?;
    let behind_first = stage_page_write(
        &mut log,
        unlogged_page(&lineage, behind_page_number.get(), 4, [0x04, 0x00])?,
    )?;
    assert_eq!(behind_first.required_position().get(), 5);
    flush_dirty_page(&mut log, &mut store, behind_first)?;
    let behind_second = stage_page_write(
        &mut log,
        unlogged_page(&lineage, behind_page_number.get(), 5, [0x04, 0x01])?,
    )?;
    assert_eq!(behind_second.required_position().get(), 6);

    // Page 105: raw image immediately flushed -> StoreCurrent with a Raw
    // required image.
    let current_page_number =
        PageNumber::new(105).ok_or_else(|| io::Error::other("current page number is zero"))?;
    let current_dirty = stage_page_write(
        &mut log,
        unlogged_page(&lineage, current_page_number.get(), 6, [0x05, 0x00])?,
    )?;
    assert_eq!(current_dirty.required_position().get(), 7);
    flush_dirty_page(&mut log, &mut store, current_dirty)?;

    drop((
        missing_dirty,
        uncommitted,
        uncommitted_dirty,
        committed,
        behind_second,
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

    let baseline = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    assert_eq!(baseline.persistent_log_id(), persistent_id);
    assert_eq!(baseline.durable_frontier(), Some(7));
    assert_eq!(baseline.transactions().len(), 2);
    assert_eq!(baseline.pages().len(), 5);
    assert_eq!(owner.parts().1.pages().len(), page_count);

    let encoded = encode_restart_checkpoint_completeness_baseline(&baseline)?;
    let decoded = decode_restart_checkpoint_completeness_baseline(&encoded)?;
    assert_completeness_observation_matches_baseline(&decoded, &baseline);

    // Decoding never grants recovery, storage, or publication authority; the
    // only operations available on the owned observation are the accessors
    // exercised above. `decoded` remains a plain untrusted data value here.
    assert_eq!(
        decoded.replay().cause(),
        Some(
            DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreMissingPage {
                page_number: missing_page_number.get(),
            }
        )
    );
    assert_eq!(decoded.replay().position(), Some(1));
    assert!(
        decoded
            .transactions()
            .transactions()
            .iter()
            .any(|entry| entry.epoch() == uncommitted_id.epoch().get()
                && entry.sequence() == uncommitted_id.sequence()
                && entry.state().commit_position().is_none())
    );
    assert!(
        decoded
            .transactions()
            .transactions()
            .iter()
            .any(|entry| entry.epoch() == committed_id.epoch().get()
                && entry.sequence() == committed_id.sequence()
                && entry.state().commit_position() == Some(4))
    );

    let missing_entry = decoded
        .pages()
        .iter()
        .find(|entry| entry.page_number() == missing_page_number.get())
        .ok_or_else(|| io::Error::other("missing page entry absent"))?;
    assert_eq!(
        missing_entry.state(),
        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreMissing
    );
    assert_eq!(
        missing_entry.required_image(),
        Some(
            DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::Raw {
                page_position: 1
            }
        )
    );
    assert_eq!(missing_entry.stored_position(), None);

    let no_required_entry = decoded
        .pages()
        .iter()
        .find(|entry| entry.page_number() == uncommitted_page_number.get())
        .ok_or_else(|| io::Error::other("uncommitted page entry absent"))?;
    assert_eq!(
        no_required_entry.state(),
        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::NoRequiredImage
    );
    assert_eq!(no_required_entry.required_image(), None);
    assert_eq!(no_required_entry.stored_position(), None);

    let committed_current_entry = decoded
        .pages()
        .iter()
        .find(|entry| entry.page_number() == committed_page_number.get())
        .ok_or_else(|| io::Error::other("committed page entry absent"))?;
    assert_eq!(
        committed_current_entry.state(),
        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent
    );
    assert_eq!(
        committed_current_entry.required_image(),
        Some(
            DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::CommittedTransaction {
                epoch: committed_id.epoch().get(),
                sequence: committed_id.sequence(),
                page_position: 3,
                commit_position: 4,
            }
        )
    );
    assert_eq!(committed_current_entry.stored_position(), Some(3));

    let behind_entry = decoded
        .pages()
        .iter()
        .find(|entry| entry.page_number() == behind_page_number.get())
        .ok_or_else(|| io::Error::other("behind page entry absent"))?;
    assert_eq!(
        behind_entry.state(),
        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreBehind
    );
    assert_eq!(
        behind_entry.required_image(),
        Some(
            DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::Raw {
                page_position: 6
            }
        )
    );
    assert_eq!(behind_entry.stored_position(), Some(5));

    let raw_current_entry = decoded
        .pages()
        .iter()
        .find(|entry| entry.page_number() == current_page_number.get())
        .ok_or_else(|| io::Error::other("raw current page entry absent"))?;
    assert_eq!(
        raw_current_entry.state(),
        DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent
    );
    assert_eq!(
        raw_current_entry.required_image(),
        Some(
            DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::Raw {
                page_position: 7
            }
        )
    );
    assert_eq!(raw_current_entry.stored_position(), Some(7));

    let mut semantically_invalid = encoded;
    semantically_invalid[24..40].fill(0);
    replace_checksum(&mut semantically_invalid);
    let decoded_invalid = decode_restart_checkpoint_completeness_baseline(&semantically_invalid)?;
    assert_eq!(decoded_invalid.transactions().persistent_log_id(), 0);
    Ok(())
}

#[test]
fn store_behind_replay_cause_round_trips_alone() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("behind-cause-completeness-codec")?;
    let log_path = directory.path().join("commit-log.bin");
    let store_path = directory.path().join("page-store.bin");
    let persistent_id = persistent_log_id(9002)?;
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
    let mut store = FilePageStore::<2>::create_new(&store_path, persistent_id)?;
    let lineage = log.lineage().clone();

    let page_number =
        PageNumber::new(201).ok_or_else(|| io::Error::other("behind page number is zero"))?;
    let first = stage_page_write(
        &mut log,
        unlogged_page(&lineage, page_number.get(), 1, [0xb1, 0x00])?,
    )?;
    assert_eq!(first.required_position().get(), 1);
    flush_dirty_page(&mut log, &mut store, first)?;
    let second = stage_page_write(
        &mut log,
        unlogged_page(&lineage, page_number.get(), 2, [0xb2, 0x00])?,
    )?;
    assert_eq!(second.required_position().get(), 2);
    log.flush_through(second.required_position())?;

    drop((second, log, store));

    let page_recovered = open_transaction_page_storage::<2, _, _>(&log_path, &store_path)?
        .recover()
        .map_err(|_| io::Error::other("filesystem page recovery failed"))?;
    let mut owner = page_recovered
        .analyze_restart()
        .map_err(|_| io::Error::other("filesystem restart analysis failed"))?;
    let baseline = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    assert_eq!(
        baseline.replay_start(),
        &ntsql_transaction::DurableTransactionRestartReplayStart::AtPosition {
            position: 2,
            cause: ntsql_transaction::DurableTransactionRestartReplayStartCause::StoreBehind {
                page_number,
            },
        }
    );

    let encoded = encode_restart_checkpoint_completeness_baseline(&baseline)?;
    let decoded = decode_restart_checkpoint_completeness_baseline(&encoded)?;
    assert_eq!(
        decoded.replay().kind(),
        DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition
    );
    assert_eq!(decoded.replay().position(), Some(2));
    assert_eq!(
        decoded.replay().cause(),
        Some(
            DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreBehindPage {
                page_number: page_number.get(),
            }
        )
    );
    Ok(())
}

#[test]
fn uncommitted_transaction_replay_cause_round_trips_alone() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("uncommitted-cause-completeness-codec")?;
    let log_path = directory.path().join("commit-log.bin");
    let store_path = directory.path().join("page-store.bin");
    let persistent_id = persistent_log_id(9003)?;
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
    let store = FilePageStore::<2>::create_new(&store_path, persistent_id)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;
    let lineage = log.lineage().clone();

    let page_number =
        PageNumber::new(301).ok_or_else(|| io::Error::other("uncommitted page number is zero"))?;
    let uncommitted = coordinator.begin()?;
    let uncommitted_id = uncommitted.transaction_id();
    let (uncommitted, uncommitted_dirty) = coordinator.stage_page_write(
        uncommitted,
        unlogged_page(&lineage, page_number.get(), 1, [0xc1, 0x00])?,
        &mut log,
    )?;
    assert_eq!(uncommitted_dirty.required_position().get(), 1);
    log.flush_through(uncommitted_dirty.required_position())?;

    drop((uncommitted, uncommitted_dirty, coordinator, log, store));

    let page_recovered = open_transaction_page_storage::<2, _, _>(&log_path, &store_path)?
        .recover()
        .map_err(|_| io::Error::other("filesystem page recovery failed"))?;
    let mut owner = page_recovered
        .analyze_restart()
        .map_err(|_| io::Error::other("filesystem restart analysis failed"))?;
    let baseline = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let ntsql_transaction::DurableTransactionRestartReplayStart::AtPosition { position, cause } =
        baseline.replay_start()
    else {
        return Err(io::Error::other("expected an inclusive replay floor").into());
    };
    assert_eq!(*position, 1);
    let ntsql_transaction::DurableTransactionRestartReplayStartCause::UncommittedTransaction {
        transaction,
    } = cause
    else {
        return Err(io::Error::other("expected an uncommitted-transaction cause").into());
    };
    assert!(transaction.matches_transaction_id(uncommitted_id));

    let encoded = encode_restart_checkpoint_completeness_baseline(&baseline)?;
    let decoded = decode_restart_checkpoint_completeness_baseline(&encoded)?;
    assert_eq!(
        decoded.replay().kind(),
        DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition
    );
    assert_eq!(decoded.replay().position(), Some(1));
    assert_eq!(
        decoded.replay().cause(),
        Some(
            DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::UncommittedTransaction {
                epoch: uncommitted_id.epoch().get(),
                sequence: uncommitted_id.sequence(),
            }
        )
    );
    Ok(())
}

fn assert_completeness_observation_matches_baseline(
    observation: &ntsql_transaction::OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
    baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
) {
    assert_eq!(
        observation.transactions().persistent_log_id(),
        baseline.persistent_log_id().get()
    );
    assert_eq!(
        observation.transactions().durable_frontier(),
        baseline.durable_frontier()
    );
    assert_eq!(
        observation.transactions().transactions().len(),
        baseline.transactions().len()
    );
    for (actual, expected) in observation
        .transactions()
        .transactions()
        .iter()
        .zip(baseline.transactions())
    {
        let transaction = expected.transaction();
        assert_eq!(actual.epoch(), transaction.epoch());
        assert_eq!(actual.sequence(), transaction.sequence());
        assert_eq!(
            actual.first_owned_page_position(),
            expected.first_owned_page_position()
        );
        assert_eq!(
            actual.last_owned_page_position(),
            expected.last_owned_page_position()
        );
        assert_eq!(
            actual.owned_page_record_count(),
            expected.owned_page_record_count()
        );
        assert_eq!(
            actual.state().commit_position(),
            expected.state().commit_position()
        );
    }

    assert_eq!(observation.pages().len(), baseline.pages().len());
    for (actual, expected) in observation.pages().iter().zip(baseline.pages()) {
        assert_eq!(actual.page_number(), expected.page_number().get());
        let expected_state = match expected.state() {
            ntsql_transaction::DurableTransactionRestartPageState::NoRequiredImage => {
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::NoRequiredImage
            }
            ntsql_transaction::DurableTransactionRestartPageState::StoreMissing { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreMissing
            }
            ntsql_transaction::DurableTransactionRestartPageState::StoreCurrent { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent
            }
            ntsql_transaction::DurableTransactionRestartPageState::StoreBehind { .. } => {
                DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreBehind
            }
        };
        assert_eq!(actual.state(), expected_state);
        assert_eq!(
            actual.required_image().map(|image| image.page_position()),
            expected
                .state()
                .required_image()
                .map(|image| image.page_position())
        );
        assert_eq!(actual.stored_position(), expected.state().stored_position());
    }

    let expected_replay_kind = match baseline.replay_start() {
        ntsql_transaction::DurableTransactionRestartReplayStart::AfterFrontier { .. } => {
            DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AfterFrontier
        }
        ntsql_transaction::DurableTransactionRestartReplayStart::AtPosition { .. } => {
            DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition
        }
    };
    assert_eq!(observation.replay().kind(), expected_replay_kind);
    assert_eq!(
        observation.replay().frontier(),
        baseline.replay_start().frontier()
    );
    assert_eq!(
        observation.replay().position(),
        baseline.replay_start().position()
    );
}

fn unlogged_page(
    lineage: &ntsql_wal::LogLineage,
    page_number: u64,
    page_version: u64,
    bytes: [u8; 2],
) -> Result<UnloggedPage<2>, Box<dyn Error>> {
    let page_number =
        PageNumber::new(page_number).ok_or_else(|| io::Error::other("page number is zero"))?;
    Ok(UnloggedPage::new(
        PageAddress::new(lineage, page_number),
        PageVersion::new(page_version),
        PageImage::new(bytes)?,
    ))
}

fn persistent_log_id(value: u128) -> Result<PersistentLogId, io::Error> {
    PersistentLogId::new(value).ok_or_else(|| io::Error::other("persistent log ID is zero"))
}

fn replace_checksum(bytes: &mut [u8]) {
    let checksum_offset = bytes.len() - 8;
    let checksum = checksum_v1(&bytes[..checksum_offset]);
    bytes[checksum_offset..].copy_from_slice(&checksum.to_be_bytes());
}

fn checksum_v1(bytes: &[u8]) -> u64 {
    const SEED: u64 = 0x4e54_5351_4c43_4b31;
    const MULTIPLIER: u64 = 0x4e54_5351_4c57_414d;
    const XOR: u64 = 0x4348_4543_4b53_554d;

    let mut state = SEED;
    let mut protected_length = 0_u64;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(MULTIPLIER);
        state = state.rotate_left(7) ^ XOR;
        protected_length = protected_length.wrapping_add(1);
    }
    state ^ protected_length
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> io::Result<Self> {
        let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ntsql-completeness-codec-{}-{name}-{unique}",
            std::process::id()
        ));
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
