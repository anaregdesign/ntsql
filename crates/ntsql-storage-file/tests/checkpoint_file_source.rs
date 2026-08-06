use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_page::{PageAddress, PageImage, PageNumber, PageVersion, UnloggedPage};
use ntsql_storage_file::{
    FileCommitLog, FilePageStore, FileRestartCheckpointBaselineSource,
    FileRestartCheckpointBaselineSourceError, FileRestartCheckpointSlotCreateError,
    FileRestartCheckpointSlotFormatErrorReason, FileRestartCheckpointSlotIoStage,
    FileRestartCheckpointSlotOpenError, FileTransactionPageStorageCheckpointOpenError,
    PageStoreIoStage, PageStoreOpenError, RestartCheckpointBaselineDecodeError,
    encode_restart_checkpoint_baseline, open_transaction_page_storage,
    open_transaction_page_storage_with_checkpoint,
};
use ntsql_transaction::{
    DurableTransactionRestartCheckpointBaselineSource, TransactionCoordinator,
    UnrecoveredTransactionPageStorage, flush_committed_page,
};
use ntsql_wal::{LogDurability, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[test]
fn create_open_empty_slot_has_exact_control_bytes_and_lifetime_lock() -> Result<(), Box<dyn Error>>
{
    let directory = TestDirectory::new("create-open-empty")?;
    let slot_path = directory.path().join("checkpoint");
    let persistent_log_id = persistent_log_id(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)?;
    let mut source =
        FileRestartCheckpointBaselineSource::create_new(&slot_path, persistent_log_id)?;

    assert_eq!(source.persistent_log_id(), persistent_log_id);
    assert_eq!(source.slot_directory(), slot_path);
    assert_eq!(source.load_restart_checkpoint_baseline()?, None);
    assert!(!slot_path.join("current").exists());
    assert_eq!(
        fs::read(slot_path.join("control"))?,
        [
            0x4e, 0x54, 0x53, 0x51, 0x43, 0x4b, 0x53, 0x31, 0x00, 0x01, 0x00, 0x40, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xee, 0xd7, 0x4c, 0xab, 0xff, 0x69, 0xc4, 0xff,
        ]
    );

    assert_checkpoint_slot_locked(
        FileRestartCheckpointBaselineSource::open(&slot_path)
            .err()
            .ok_or_else(|| io::Error::other("second checkpoint source acquired the lock"))?,
    )?;
    let create_error =
        FileRestartCheckpointBaselineSource::create_new(&slot_path, persistent_log_id)
            .err()
            .ok_or_else(|| io::Error::other("existing checkpoint slot was created twice"))?;
    let FileRestartCheckpointSlotCreateError::Io(create_source) = create_error else {
        return Err(io::Error::other("duplicate create did not retain an I/O error").into());
    };
    assert_eq!(
        create_source.stage(),
        FileRestartCheckpointSlotIoStage::CreateSlotDirectory
    );
    assert_eq!(
        create_source.io_source().kind(),
        io::ErrorKind::AlreadyExists
    );

    drop(source);
    let mut reopened = FileRestartCheckpointBaselineSource::open(&slot_path)?;
    assert_eq!(reopened.persistent_log_id(), persistent_log_id);
    assert_eq!(reopened.load_restart_checkpoint_baseline()?, None);
    Ok(())
}

#[test]
fn current_blob_loads_as_untrusted_then_validates_against_real_current_wal()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("current-validates")?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("checkpoint");
    let persistent_log_id = persistent_log_id(142)?;
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_log_id)?;
    let mut store = FilePageStore::<2>::create_new(&store_path, persistent_log_id)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;

    let active = coordinator.begin()?;
    let (active, dirty) = coordinator.stage_page_write(
        active,
        unlogged_page(LogDurability::lineage(&log), 142, 1, [0x14, 0x02])?,
        &mut log,
    )?;
    let committed = coordinator.commit(active, &mut log)?;
    flush_committed_page(&committed, &mut log, &mut store, dirty)?;
    drop(coordinator);

    let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?;
    let baseline = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
    let encoded = encode_restart_checkpoint_baseline(&baseline)?;

    drop(FileRestartCheckpointBaselineSource::create_new(
        &slot_path,
        persistent_log_id,
    )?);
    write_synced_new(&slot_path.join("current"), &encoded)?;
    let mut source = FileRestartCheckpointBaselineSource::open(&slot_path)?;
    let loaded = source
        .load_restart_checkpoint_baseline()?
        .ok_or_else(|| io::Error::other("current checkpoint was reported absent"))?;
    assert_eq!(loaded.persistent_log_id(), persistent_log_id.get());
    assert_eq!(loaded.durable_frontier(), baseline.durable_frontier());
    assert_eq!(loaded.transactions().len(), baseline.transactions().len());
    assert_eq!(
        owner.validate_restart_checkpoint_baseline_against_current_prefix(
            &loaded.as_observation()
        )?,
        baseline
    );

    write_synced_new(&slot_path.join("candidate"), &encoded)?;
    fs::rename(slot_path.join("candidate"), slot_path.join("current"))?;
    assert_checkpoint_slot_locked(
        FileRestartCheckpointBaselineSource::open(&slot_path)
            .err()
            .ok_or_else(|| {
                io::Error::other("current-file replacement released the control lock")
            })?,
    )?;
    assert!(source.load_restart_checkpoint_baseline()?.is_some());
    drop(source);
    assert!(
        FileRestartCheckpointBaselineSource::open(&slot_path)?
            .load_restart_checkpoint_baseline()?
            .is_some()
    );
    Ok(())
}

#[test]
fn malformed_current_and_current_io_failures_are_distinct() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("current-errors")?;
    let slot_path = directory.path().join("checkpoint");
    let persistent_log_id = persistent_log_id(143)?;
    drop(FileRestartCheckpointBaselineSource::create_new(
        &slot_path,
        persistent_log_id,
    )?);

    write_synced_new(&slot_path.join("current"), &[0x4e])?;
    let mut source = FileRestartCheckpointBaselineSource::open(&slot_path)?;
    assert_eq!(
        source.load_restart_checkpoint_baseline(),
        Err(FileRestartCheckpointBaselineSourceError::Decode(
            RestartCheckpointBaselineDecodeError::Truncated {
                expected_length: 64,
                actual_length: 1,
            }
        ))
    );
    drop(source);
    fs::remove_file(slot_path.join("current"))?;
    fs::create_dir(slot_path.join("current"))?;

    let mut source = FileRestartCheckpointBaselineSource::open(&slot_path)?;
    let error = source
        .load_restart_checkpoint_baseline()
        .err()
        .ok_or_else(|| io::Error::other("current directory loaded as a checkpoint blob"))?;
    let FileRestartCheckpointBaselineSourceError::Io(io_source) = error else {
        return Err(io::Error::other("current-file I/O failure changed error category").into());
    };
    assert!(
        matches!(
            io_source.stage(),
            FileRestartCheckpointSlotIoStage::OpenCurrentFile
                | FileRestartCheckpointSlotIoStage::ReadCurrentBytes
        ),
        "unexpected directory-read failure stage: {:?}",
        io_source.stage()
    );
    assert!(Error::source(&io_source).is_some());

    #[cfg(unix)]
    {
        fs::remove_dir(slot_path.join("current"))?;
        std::os::unix::fs::symlink("missing-target", slot_path.join("current"))?;
        let dangling = source
            .load_restart_checkpoint_baseline()
            .err()
            .ok_or_else(|| io::Error::other("dangling current symlink appeared absent"))?;
        let FileRestartCheckpointBaselineSourceError::Io(dangling) = dangling else {
            return Err(io::Error::other("dangling current symlink changed error category").into());
        };
        assert_eq!(
            dangling.stage(),
            FileRestartCheckpointSlotIoStage::OpenCurrentFile
        );

        fs::remove_file(slot_path.join("current"))?;
        fs::remove_file(slot_path.join("control"))?;
        fs::remove_dir(&slot_path)?;
        let removed_slot = source
            .load_restart_checkpoint_baseline()
            .err()
            .ok_or_else(|| io::Error::other("removed slot directory appeared empty"))?;
        let FileRestartCheckpointBaselineSourceError::Io(removed_slot) = removed_slot else {
            return Err(io::Error::other("removed slot changed error category").into());
        };
        assert_eq!(
            removed_slot.stage(),
            FileRestartCheckpointSlotIoStage::VerifyCurrentAbsence
        );
    }

    let capacity =
        FileRestartCheckpointBaselineSourceError::CurrentCapacityExhausted { length: usize::MAX };
    assert!(Error::source(&capacity).is_none());
    Ok(())
}

#[test]
fn control_lock_precedes_format_parsing_and_malformed_control_fails_after_release()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("control-order")?;
    let slot_path = directory.path().join("checkpoint");
    let persistent_log_id = persistent_log_id(144)?;
    let source = FileRestartCheckpointBaselineSource::create_new(&slot_path, persistent_log_id)?;

    let mut noncooperating_writer = OpenOptions::new()
        .write(true)
        .open(slot_path.join("control"))?;
    noncooperating_writer.write_all(b"X")?;
    noncooperating_writer.sync_all()?;
    drop(noncooperating_writer);

    assert_checkpoint_slot_locked(
        FileRestartCheckpointBaselineSource::open(&slot_path)
            .err()
            .ok_or_else(|| io::Error::other("contending open parsed locked control bytes"))?,
    )?;
    drop(source);

    let malformed = FileRestartCheckpointBaselineSource::open(&slot_path)
        .err()
        .ok_or_else(|| io::Error::other("malformed control header opened"))?;
    let FileRestartCheckpointSlotOpenError::Format(malformed) = malformed else {
        return Err(io::Error::other("malformed control header was not a format error").into());
    };
    assert_eq!(malformed.offset(), 0);
    assert!(matches!(
        malformed.reason(),
        FileRestartCheckpointSlotFormatErrorReason::HeaderMagic { .. }
    ));

    let short_slot = directory.path().join("short-checkpoint");
    drop(FileRestartCheckpointBaselineSource::create_new(
        &short_slot,
        persistent_log_id,
    )?);
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(short_slot.join("control"))?
        .sync_all()?;
    let short = FileRestartCheckpointBaselineSource::open(&short_slot)
        .err()
        .ok_or_else(|| io::Error::other("short control file opened"))?;
    assert!(matches!(
        short,
        FileRestartCheckpointSlotOpenError::Format(ref source)
            if source.offset() == 0
                && source.reason()
                    == &FileRestartCheckpointSlotFormatErrorReason::FileLength { actual: 0 }
    ));
    Ok(())
}

#[test]
fn composition_opens_wal_page_store_checkpoint_in_fixed_order_and_releases_prefixes()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("composition-order")?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("checkpoint");
    let missing_log = directory.path().join("missing-wal.bin");
    let missing_store = directory.path().join("missing-pages.bin");
    let missing_slot = directory.path().join("missing-checkpoint");
    let persistent_log_id = persistent_log_id(145)?;
    drop(FileCommitLog::<1>::create_new_transaction_page_capable(
        &log_path,
        persistent_log_id,
    )?);
    drop(FilePageStore::<1>::create_new(
        &store_path,
        persistent_log_id,
    )?);
    let held_checkpoint =
        FileRestartCheckpointBaselineSource::create_new(&slot_path, persistent_log_id)?;

    assert!(matches!(
        open_transaction_page_storage_with_checkpoint::<1, _, _, _>(
            &missing_log,
            &store_path,
            &slot_path,
        ),
        Err(FileTransactionPageStorageCheckpointOpenError::CommitLog(_))
    ));
    assert!(matches!(
        open_transaction_page_storage_with_checkpoint::<1, _, _, _>(
            &log_path,
            &missing_store,
            &slot_path,
        ),
        Err(FileTransactionPageStorageCheckpointOpenError::PageStore(_))
    ));
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);

    let checkpoint_error = open_transaction_page_storage_with_checkpoint::<1, _, _, _>(
        &log_path,
        &store_path,
        &slot_path,
    )
    .err()
    .ok_or_else(|| io::Error::other("composition bypassed held checkpoint lock"))?;
    let FileTransactionPageStorageCheckpointOpenError::Checkpoint(
        FileRestartCheckpointSlotOpenError::Io(checkpoint_source),
    ) = checkpoint_error
    else {
        return Err(io::Error::other("checkpoint contention changed open stage").into());
    };
    assert_eq!(
        checkpoint_source.stage(),
        FileRestartCheckpointSlotIoStage::AcquireExclusiveControlLock
    );
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);
    drop(FilePageStore::<1>::open(&store_path)?);

    drop(held_checkpoint);
    let missing_checkpoint = open_transaction_page_storage_with_checkpoint::<1, _, _, _>(
        &log_path,
        &store_path,
        &missing_slot,
    )
    .err()
    .ok_or_else(|| io::Error::other("missing checkpoint slot opened"))?;
    assert!(matches!(
        missing_checkpoint,
        FileTransactionPageStorageCheckpointOpenError::Checkpoint(_)
    ));
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);
    drop(FilePageStore::<1>::open(&store_path)?);

    let opened = open_transaction_page_storage_with_checkpoint::<1, _, _, _>(
        &log_path,
        &store_path,
        &slot_path,
    )?;
    assert!(open_transaction_page_storage::<1, _, _>(&log_path, &store_path).is_err());
    assert_page_store_locked(
        FilePageStore::<1>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("composition released its page-store lock"))?,
    )?;
    assert_checkpoint_slot_locked(
        FileRestartCheckpointBaselineSource::open(&slot_path)
            .err()
            .ok_or_else(|| io::Error::other("composition released its checkpoint lock"))?,
    )?;

    let (unrecovered, mut checkpoint) = opened.into_parts();
    let mut analyzed = unrecovered.recover()?.analyze_restart()?;
    assert_eq!(
        analyzed.validate_restart_checkpoint_baseline_from_source(&mut checkpoint)?,
        None
    );
    Ok(())
}

#[test]
fn composition_rejects_storage_and_checkpoint_lineage_mismatch_before_returning()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("composition-lineage")?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let foreign_store_path = directory.path().join("foreign-pages.bin");
    let slot_path = directory.path().join("checkpoint");
    let foreign_slot_path = directory.path().join("foreign-checkpoint");
    let foreign_log_id = persistent_log_id(147)?;
    let persistent_log_id = persistent_log_id(146)?;
    drop(FileCommitLog::<1>::create_new_transaction_page_capable(
        &log_path,
        persistent_log_id,
    )?);
    drop(FilePageStore::<1>::create_new(
        &store_path,
        persistent_log_id,
    )?);
    drop(FilePageStore::<1>::create_new(
        &foreign_store_path,
        foreign_log_id,
    )?);
    drop(FileRestartCheckpointBaselineSource::create_new(
        &slot_path,
        persistent_log_id,
    )?);
    drop(FileRestartCheckpointBaselineSource::create_new(
        &foreign_slot_path,
        foreign_log_id,
    )?);

    assert_eq!(
        open_transaction_page_storage_with_checkpoint::<1, _, _, _>(
            &log_path,
            &foreign_store_path,
            directory.path().join("not-consulted"),
        )
        .err(),
        Some(
            FileTransactionPageStorageCheckpointOpenError::StoragePersistentLogIdMismatch {
                commit_log: persistent_log_id,
                page_store: foreign_log_id,
            }
        )
    );
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);
    drop(FilePageStore::<1>::open(&foreign_store_path)?);

    assert_eq!(
        open_transaction_page_storage_with_checkpoint::<1, _, _, _>(
            &log_path,
            &store_path,
            &foreign_slot_path,
        )
        .err(),
        Some(
            FileTransactionPageStorageCheckpointOpenError::CheckpointPersistentLogIdMismatch {
                storage: persistent_log_id,
                checkpoint: foreign_log_id,
            }
        )
    );
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);
    drop(FilePageStore::<1>::open(&store_path)?);
    drop(FileRestartCheckpointBaselineSource::open(
        &foreign_slot_path,
    )?);
    Ok(())
}

#[cfg(unix)]
#[test]
fn hard_link_control_alias_cannot_bypass_lifetime_lock() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("control-hard-link")?;
    let first_slot = directory.path().join("first");
    let alias_slot = directory.path().join("alias");
    let persistent_log_id = persistent_log_id(148)?;
    let source = FileRestartCheckpointBaselineSource::create_new(&first_slot, persistent_log_id)?;
    fs::create_dir(&alias_slot)?;
    fs::hard_link(first_slot.join("control"), alias_slot.join("control"))?;

    assert_checkpoint_slot_locked(
        FileRestartCheckpointBaselineSource::open(&alias_slot)
            .err()
            .ok_or_else(|| io::Error::other("hard-linked control bypassed the lock"))?,
    )?;
    drop(source);
    let alias = FileRestartCheckpointBaselineSource::open(&alias_slot)?;
    assert_eq!(alias.persistent_log_id(), persistent_log_id);
    Ok(())
}

fn assert_checkpoint_slot_locked(
    error: FileRestartCheckpointSlotOpenError,
) -> Result<(), io::Error> {
    let FileRestartCheckpointSlotOpenError::Io(source) = error else {
        return Err(io::Error::other(
            "checkpoint lock contention was not an I/O failure",
        ));
    };
    if source.stage() != FileRestartCheckpointSlotIoStage::AcquireExclusiveControlLock
        || source.io_source().kind() != io::ErrorKind::WouldBlock
    {
        return Err(io::Error::other(
            "checkpoint lock contention had the wrong stage or cause",
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
            "page-store lock contention had the wrong stage or cause",
        ));
    }
    Ok(())
}

fn write_synced_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
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

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> io::Result<Self> {
        let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ntsql-checkpoint-file-source-{}-{name}-{unique}",
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
