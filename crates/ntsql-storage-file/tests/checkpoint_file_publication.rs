use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_page::{PageAddress, PageImage, PageNumber, PageVersion, UnloggedPage};
use ntsql_storage_file::{
    FileCommitLog, FilePageStore, FileRestartCheckpointBaselinePublicationError,
    FileRestartCheckpointBaselinePublicationFaultPoint, FileRestartCheckpointBaselineSource,
    FileRestartCheckpointSlotIoStage, encode_restart_checkpoint_baseline,
};
use ntsql_transaction::{
    DurableTransactionRestartCheckpointBaselineCurrentPublicationError,
    RestartAnalyzedTransactionPageStorage, TransactionCoordinator,
    UnrecoveredTransactionPageStorage, flush_committed_page,
};
use ntsql_wal::{LogDurability, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

type FileOwner = RestartAnalyzedTransactionPageStorage<FileCommitLog<2>, FilePageStore<2>, 2>;

#[test]
fn publication_reconciles_stale_candidate_replaces_current_and_loads_untrusted()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("publish-replace")?;
    let persistent_log_id = persistent_log_id(1441)?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let slot_path = directory.path().join("checkpoint");
    let mut checkpoint =
        FileRestartCheckpointBaselineSource::create_new(&slot_path, persistent_log_id)?;
    write_synced_new(&slot_path.join("candidate"), b"stale candidate")?;

    let first_expected = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
    let first_bytes = encode_restart_checkpoint_baseline(&first_expected)?;
    let first_receipt =
        owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(
        first_receipt.persistent_log_id(),
        first_expected.persistent_log_id()
    );
    assert_eq!(
        first_receipt.durable_frontier(),
        first_expected.durable_frontier()
    );
    assert_eq!(
        first_receipt.transaction_count(),
        first_expected.transactions().len()
    );
    assert_eq!(fs::read(slot_path.join("current"))?, first_bytes);
    assert!(!slot_path.join("candidate").exists());

    append_committed_page(&mut owner, 144, 1, [0x14, 0x41])?;
    let replacement = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
    let replacement_bytes = encode_restart_checkpoint_baseline(&replacement)?;
    assert_ne!(replacement_bytes, first_bytes);

    let replacement_receipt =
        owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(
        replacement_receipt.persistent_log_id(),
        replacement.persistent_log_id()
    );
    assert_eq!(
        replacement_receipt.durable_frontier(),
        replacement.durable_frontier()
    );
    assert_eq!(
        replacement_receipt.transaction_count(),
        replacement.transactions().len()
    );
    assert_eq!(fs::read(slot_path.join("current"))?, replacement_bytes);
    assert!(!slot_path.join("candidate").exists());
    assert_eq!(
        owner.validate_restart_checkpoint_baseline_from_source(&mut checkpoint)?,
        Some(replacement)
    );
    assert_eq!(checkpoint.armed_publication_fault(), None);
    Ok(())
}

#[test]
fn every_publication_fault_has_exact_candidate_and_current_effect_then_fresh_success()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("publication-faults")?;
    let points = [
        FileRestartCheckpointBaselinePublicationFaultPoint::BeforeCandidateCleanup,
        FileRestartCheckpointBaselinePublicationFaultPoint::AfterCandidateCleanup,
        FileRestartCheckpointBaselinePublicationFaultPoint::AfterCandidateCreate,
        FileRestartCheckpointBaselinePublicationFaultPoint::AfterCandidateWrite,
        FileRestartCheckpointBaselinePublicationFaultPoint::AfterCandidateSync,
        FileRestartCheckpointBaselinePublicationFaultPoint::AfterCurrentReplace,
        FileRestartCheckpointBaselinePublicationFaultPoint::AfterDirectorySync,
    ];

    for (index, point) in points.into_iter().enumerate() {
        let case_path = directory.path().join(format!("case-{index}"));
        fs::create_dir(&case_path)?;
        let persistent_log_id = persistent_log_id(1500 + index as u128)?;
        let mut owner = analyzed_owner(&case_path, persistent_log_id)?;
        let slot_path = case_path.join("checkpoint");
        let mut checkpoint =
            FileRestartCheckpointBaselineSource::create_new(&slot_path, persistent_log_id)?;

        let old = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
        let old_bytes = encode_restart_checkpoint_baseline(&old)?;
        let _final_receipt =
            owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
        append_committed_page(&mut owner, 200 + index as u64, 1, [0x15, index as u8])?;
        let expected = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
        let expected_bytes = encode_restart_checkpoint_baseline(&expected)?;
        write_synced_new(&slot_path.join("candidate"), b"stale")?;

        checkpoint.arm_publication_fault(point)?;
        let error = owner
            .publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)
            .err()
            .ok_or_else(|| io::Error::other(format!("fault {point} reported success")))?;
        let DurableTransactionRestartCheckpointBaselineCurrentPublicationError::Publication(
            failure,
        ) = &error
        else {
            return Err(io::Error::other(format!("fault {point} became preparation")).into());
        };
        assert_eq!(
            failure.cause(),
            &FileRestartCheckpointBaselinePublicationError::InjectedFault(point)
        );
        assert_eq!(
            failure.publication().persistent_log_id(),
            expected.persistent_log_id()
        );
        assert_eq!(
            failure.publication().durable_frontier(),
            expected.durable_frontier()
        );
        assert_eq!(
            failure.publication().transaction_count(),
            expected.transactions().len()
        );
        assert!(Error::source(&error).is_some());
        assert!(Error::source(failure).is_some());
        assert!(Error::source(failure.cause()).is_none());
        assert_eq!(checkpoint.armed_publication_fault(), None);

        let candidate_path = slot_path.join("candidate");
        let current_path = slot_path.join("current");
        match point {
            FileRestartCheckpointBaselinePublicationFaultPoint::BeforeCandidateCleanup => {
                assert_eq!(fs::read(&candidate_path)?, b"stale");
                assert_eq!(fs::read(&current_path)?, old_bytes);
            }
            FileRestartCheckpointBaselinePublicationFaultPoint::AfterCandidateCleanup => {
                assert!(!candidate_path.exists());
                assert_eq!(fs::read(&current_path)?, old_bytes);
            }
            FileRestartCheckpointBaselinePublicationFaultPoint::AfterCandidateCreate => {
                assert_eq!(fs::read(&candidate_path)?, []);
                assert_eq!(fs::read(&current_path)?, old_bytes);
            }
            FileRestartCheckpointBaselinePublicationFaultPoint::AfterCandidateWrite
            | FileRestartCheckpointBaselinePublicationFaultPoint::AfterCandidateSync => {
                assert_eq!(fs::read(&candidate_path)?, expected_bytes);
                assert_eq!(fs::read(&current_path)?, old_bytes);
            }
            FileRestartCheckpointBaselinePublicationFaultPoint::AfterCurrentReplace
            | FileRestartCheckpointBaselinePublicationFaultPoint::AfterDirectorySync => {
                assert!(!candidate_path.exists());
                assert_eq!(fs::read(&current_path)?, expected_bytes);
            }
        }

        let receipt =
            owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
        assert_eq!(receipt.persistent_log_id(), expected.persistent_log_id());
        assert_eq!(receipt.durable_frontier(), expected.durable_frontier());
        assert_eq!(receipt.transaction_count(), expected.transactions().len());
        assert_eq!(fs::read(current_path)?, expected_bytes);
        assert!(!candidate_path.exists());
        assert_eq!(
            owner.validate_restart_checkpoint_baseline_from_source(&mut checkpoint)?,
            Some(expected)
        );
    }
    Ok(())
}

#[test]
fn wrong_slot_rejects_before_candidate_effect_or_fault_consumption() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("wrong-slot")?;
    let owner_log_id = persistent_log_id(1601)?;
    let slot_log_id = persistent_log_id(1602)?;
    let mut owner = analyzed_owner(directory.path(), owner_log_id)?;
    let expected = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
    let slot_path = directory.path().join("checkpoint");
    let mut checkpoint = FileRestartCheckpointBaselineSource::create_new(&slot_path, slot_log_id)?;
    write_synced_new(&slot_path.join("candidate"), b"retained")?;
    checkpoint.arm_publication_fault(
        FileRestartCheckpointBaselinePublicationFaultPoint::BeforeCandidateCleanup,
    )?;
    let already_armed = checkpoint
        .arm_publication_fault(
            FileRestartCheckpointBaselinePublicationFaultPoint::AfterDirectorySync,
        )
        .err()
        .ok_or_else(|| io::Error::other("armed publication fault was replaced"))?;
    assert_eq!(
        already_armed.armed(),
        FileRestartCheckpointBaselinePublicationFaultPoint::BeforeCandidateCleanup
    );
    assert_eq!(
        already_armed.requested(),
        FileRestartCheckpointBaselinePublicationFaultPoint::AfterDirectorySync
    );

    let error = owner
        .publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)
        .err()
        .ok_or_else(|| io::Error::other("wrong checkpoint slot accepted publication"))?;
    let DurableTransactionRestartCheckpointBaselineCurrentPublicationError::Publication(failure) =
        &error
    else {
        return Err(io::Error::other("wrong slot became preparation failure").into());
    };
    assert_eq!(
        failure.cause(),
        &FileRestartCheckpointBaselinePublicationError::SlotPersistentLogIdMismatch {
            slot: slot_log_id,
            baseline: owner_log_id,
        }
    );
    assert_eq!(
        failure.publication().persistent_log_id(),
        expected.persistent_log_id()
    );
    assert_eq!(fs::read(slot_path.join("candidate"))?, b"retained");
    assert!(!slot_path.join("current").exists());
    assert_eq!(
        checkpoint.armed_publication_fault(),
        Some(FileRestartCheckpointBaselinePublicationFaultPoint::BeforeCandidateCleanup)
    );
    assert!(Error::source(failure.cause()).is_none());
    Ok(())
}

#[test]
fn candidate_and_replace_io_failures_preserve_exact_stage_without_delete_fallback()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("publication-io")?;
    let persistent_log_id = persistent_log_id(1701)?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let slot_path = directory.path().join("checkpoint");
    let mut checkpoint =
        FileRestartCheckpointBaselineSource::create_new(&slot_path, persistent_log_id)?;
    let old = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
    let old_bytes = encode_restart_checkpoint_baseline(&old)?;
    let _old_receipt =
        owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
    append_committed_page(&mut owner, 170, 1, [0x17, 0x01])?;
    let expected = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
    let expected_bytes = encode_restart_checkpoint_baseline(&expected)?;

    fs::create_dir(slot_path.join("candidate"))?;
    let cleanup_error = owner
        .publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)
        .err()
        .ok_or_else(|| io::Error::other("candidate directory was removed as a file"))?;
    assert_publication_io_stage(
        &cleanup_error,
        FileRestartCheckpointSlotIoStage::RemoveCandidateFile,
    )?;
    assert_eq!(fs::read(slot_path.join("current"))?, old_bytes);
    assert!(slot_path.join("candidate").is_dir());
    fs::remove_dir(slot_path.join("candidate"))?;

    fs::remove_file(slot_path.join("current"))?;
    fs::create_dir(slot_path.join("current"))?;
    let replace_error = owner
        .publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)
        .err()
        .ok_or_else(|| io::Error::other("publisher replaced a directory as current"))?;
    assert_publication_io_stage(
        &replace_error,
        FileRestartCheckpointSlotIoStage::ReplaceCurrentFile,
    )?;
    assert!(slot_path.join("current").is_dir());
    assert_eq!(fs::read(slot_path.join("candidate"))?, expected_bytes);

    fs::remove_dir(slot_path.join("current"))?;
    let _final_receipt =
        owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(fs::read(slot_path.join("current"))?, expected_bytes);
    assert!(!slot_path.join("candidate").exists());
    assert_eq!(
        owner.validate_restart_checkpoint_baseline_from_source(&mut checkpoint)?,
        Some(expected)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn stale_candidate_symlink_is_unlinked_without_following_target() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("candidate-symlink")?;
    let persistent_log_id = persistent_log_id(1801)?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let slot_path = directory.path().join("checkpoint");
    let mut checkpoint =
        FileRestartCheckpointBaselineSource::create_new(&slot_path, persistent_log_id)?;
    let sentinel_path = directory.path().join("sentinel");
    write_synced_new(&sentinel_path, b"sentinel")?;
    std::os::unix::fs::symlink(&sentinel_path, slot_path.join("candidate"))?;

    let _receipt =
        owner.publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(fs::read(sentinel_path)?, b"sentinel");
    assert!(!slot_path.join("candidate").exists());
    assert!(slot_path.join("current").is_file());
    Ok(())
}

fn assert_publication_io_stage(
    error: &DurableTransactionRestartCheckpointBaselineCurrentPublicationError<
        ntsql_storage_file::FileTransactionRestartAnalysisSourceError<2>,
        FileRestartCheckpointBaselinePublicationError,
    >,
    expected: FileRestartCheckpointSlotIoStage,
) -> Result<(), io::Error> {
    let DurableTransactionRestartCheckpointBaselineCurrentPublicationError::Publication(failure) =
        error
    else {
        return Err(io::Error::other(
            "filesystem publication I/O error became preparation",
        ));
    };
    let FileRestartCheckpointBaselinePublicationError::Io(source) = failure.cause() else {
        return Err(io::Error::other(
            "filesystem publication I/O error changed category",
        ));
    };
    if source.stage() != expected {
        return Err(io::Error::other(format!(
            "filesystem publication I/O stage {:?} did not match {expected:?}",
            source.stage()
        )));
    }
    if Error::source(error).is_none()
        || Error::source(failure).is_none()
        || Error::source(failure.cause()).is_none()
        || Error::source(source).is_none()
    {
        return Err(io::Error::other(
            "filesystem publication I/O cause chain is incomplete",
        ));
    }
    Ok(())
}

fn analyzed_owner(
    directory: &Path,
    persistent_log_id: PersistentLogId,
) -> Result<FileOwner, Box<dyn Error>> {
    let log = FileCommitLog::<2>::create_new_transaction_page_capable(
        directory.join("wal.bin"),
        persistent_log_id,
    )?;
    let store = FilePageStore::<2>::create_new(directory.join("pages.bin"), persistent_log_id)?;
    Ok(UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?)
}

fn append_committed_page(
    owner: &mut FileOwner,
    page_number: u64,
    page_version: u64,
    bytes: [u8; 2],
) -> Result<(), Box<dyn Error>> {
    let (log, store) = owner.parts_mut();
    let lineage = LogDurability::lineage(log).clone();
    let mut coordinator = TransactionCoordinator::open(log)?;
    let active = coordinator.begin()?;
    let (active, dirty) = coordinator.stage_page_write(
        active,
        unlogged_page(&lineage, page_number, page_version, bytes)?,
        log,
    )?;
    let committed = coordinator.commit(active, log)?;
    flush_committed_page(&committed, log, store, dirty)?;
    Ok(())
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

fn write_synced_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> io::Result<Self> {
        let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ntsql-checkpoint-file-publication-{}-{name}-{unique}",
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
