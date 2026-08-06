use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_page::{PageAddress, PageImage, PageNumber, PageVersion, UnloggedPage};
use ntsql_storage_file::{
    FileCommitLog, FilePageStore, decode_restart_checkpoint_baseline,
    encode_restart_checkpoint_baseline,
};
use ntsql_transaction::{
    DurableTransactionRestartCheckpointBaseline, TransactionCoordinator,
    UnrecoveredTransactionPageStorage, flush_committed_page,
};
use ntsql_wal::{LogDurability, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[test]
fn empty_authoritative_baseline_has_exact_version_one_golden_bytes() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("empty-checkpoint-codec")?;
    let persistent_log_id = persistent_log_id(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_log_id)?;
    let store = FilePageStore::<2>::create_new(&store_path, persistent_log_id)?;
    let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?;
    let baseline = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;

    let encoded = encode_restart_checkpoint_baseline(&baseline)?;
    let expected = [
        0x4e, 0x54, 0x53, 0x51, 0x43, 0x4b, 0x50, 0x31, 0x00, 0x01, 0x00, 0x40, 0x00, 0x40, 0x00,
        0x10, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x50, 0x4e, 0x54, 0x53, 0x51, 0x43, 0x4b, 0x45, 0x31, 0x04, 0x02, 0xf4,
        0x8f, 0x02, 0x5c, 0x7a, 0xf1,
    ];
    assert_eq!(encoded, expected);
    assert_eq!(encode_restart_checkpoint_baseline(&baseline)?, expected);

    let decoded = decode_restart_checkpoint_baseline(&encoded)?;
    assert_checkpoint_observation_matches_baseline(&decoded, &baseline);
    assert_eq!(
        owner.validate_restart_checkpoint_baseline_against_current_prefix(
            &decoded.as_observation()
        )?,
        baseline
    );
    Ok(())
}

#[test]
fn nonempty_authoritative_round_trip_preserves_order_and_requires_validation()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("nonempty-checkpoint-codec")?;
    let persistent_log_id = persistent_log_id(0x1402)?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_log_id)?;
    let mut store = FilePageStore::<2>::create_new(&store_path, persistent_log_id)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;

    let committed = coordinator.begin()?;
    let (committed, committed_dirty) = coordinator.stage_page_write(
        committed,
        unlogged_page(LogDurability::lineage(&log), 140, 1, [0x14, 0x01])?,
        &mut log,
    )?;
    let committed = coordinator.commit(committed, &mut log)?;
    flush_committed_page(&committed, &mut log, &mut store, committed_dirty)?;

    let uncommitted = coordinator.begin()?;
    let (uncommitted, uncommitted_dirty) = coordinator.stage_page_write(
        uncommitted,
        unlogged_page(LogDurability::lineage(&log), 141, 2, [0x14, 0x02])?,
        &mut log,
    )?;
    log.flush_through(uncommitted_dirty.required_position())?;
    drop((uncommitted, uncommitted_dirty, coordinator));

    let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?;
    let baseline = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
    assert_eq!(baseline.persistent_log_id(), persistent_log_id);
    assert_eq!(baseline.durable_frontier(), Some(3));
    assert_eq!(baseline.transactions().len(), 2);
    assert_eq!(
        baseline.transactions()[0].state().commit_position(),
        Some(2)
    );
    assert_eq!(baseline.transactions()[1].state().commit_position(), None);

    let encoded = encode_restart_checkpoint_baseline(&baseline)?;
    let decoded = decode_restart_checkpoint_baseline(&encoded)?;
    assert_checkpoint_observation_matches_baseline(&decoded, &baseline);
    assert_eq!(
        owner.validate_restart_checkpoint_baseline_against_current_prefix(
            &decoded.as_observation()
        )?,
        baseline
    );

    let mut semantically_invalid = encoded;
    semantically_invalid[16..32].fill(0);
    replace_checksum(&mut semantically_invalid);
    let decoded_invalid = decode_restart_checkpoint_baseline(&semantically_invalid)?;
    assert_eq!(decoded_invalid.persistent_log_id(), 0);
    assert!(
        owner
            .validate_restart_checkpoint_baseline_against_current_prefix(
                &decoded_invalid.as_observation()
            )
            .is_err()
    );
    Ok(())
}

fn assert_checkpoint_observation_matches_baseline(
    observation: &ntsql_transaction::OwnedDurableTransactionRestartCheckpointBaselineObservation,
    baseline: &DurableTransactionRestartCheckpointBaseline,
) {
    assert_eq!(
        observation.persistent_log_id(),
        baseline.persistent_log_id().get()
    );
    assert_eq!(observation.durable_frontier(), baseline.durable_frontier());
    assert_eq!(
        observation.transactions().len(),
        baseline.transactions().len()
    );
    for (actual, expected) in observation
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
            "ntsql-checkpoint-codec-{}-{name}-{unique}",
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
