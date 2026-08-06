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
    FileRestartCheckpointCompletenessBaselineSource,
    FileRestartCheckpointCompletenessBaselineSourceError, FileRestartCheckpointSlotCreateError,
    FileRestartCheckpointSlotFormatErrorReason, FileRestartCheckpointSlotIoStage,
    FileRestartCheckpointSlotOpenError, FileTransactionPageStorageCompletenessCheckpointOpenError,
    PageStoreIoStage, PageStoreOpenError, RestartCheckpointCompletenessBaselineDecodeError,
    encode_restart_checkpoint_baseline, encode_restart_checkpoint_completeness_baseline,
    open_transaction_page_storage_with_completeness_checkpoint,
};
use ntsql_transaction::{
    DurableTransactionRestartCheckpointCompletenessBaselineSource, TransactionCoordinator,
    TransactionPageStorageRestartCheckpointCompletenessSelection,
    UnrecoveredTransactionPageStorage, flush_committed_page,
};
use ntsql_wal::{LogDurability, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

const COMPLETENESS_CONTROL_BYTES: [u8; 64] = [
    0x4e, 0x54, 0x53, 0x51, 0x43, 0x4d, 0x53, 0x31, 0x00, 0x01, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xba, 0x49, 0xee, 0xcf, 0xf9, 0xb8, 0x4d, 0x5a,
];

#[test]
fn create_open_empty_completeness_slot_has_exact_control_bytes_and_lifetime_lock()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("create-open-empty")?;
    let slot_path = directory.path().join("completeness");
    let persistent_log_id = persistent_log_id(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)?;
    let mut source =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;

    assert_eq!(source.persistent_log_id(), persistent_log_id);
    assert_eq!(source.slot_directory(), slot_path);
    assert_eq!(
        source.load_restart_checkpoint_completeness_baseline()?,
        None
    );
    assert!(!slot_path.join("current").exists());
    assert!(!slot_path.join("candidate").exists());
    assert_eq!(
        fs::read(slot_path.join("control"))?,
        COMPLETENESS_CONTROL_BYTES
    );

    assert_completeness_slot_locked(
        FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)
            .err()
            .ok_or_else(|| io::Error::other("second completeness source acquired the lock"))?,
    )?;
    let create_error =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)
            .err()
            .ok_or_else(|| io::Error::other("existing completeness slot was created twice"))?;
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

    let missing_parent = directory.path().join("absent").join("completeness");
    let missing_parent_error = FileRestartCheckpointCompletenessBaselineSource::create_new(
        &missing_parent,
        persistent_log_id,
    )
    .err()
    .ok_or_else(|| io::Error::other("completeness slot created without a parent directory"))?;
    let FileRestartCheckpointSlotCreateError::Io(missing_parent_source) = missing_parent_error
    else {
        return Err(io::Error::other("missing parent did not retain an I/O error").into());
    };
    assert_eq!(
        missing_parent_source.stage(),
        FileRestartCheckpointSlotIoStage::CreateSlotDirectory
    );
    assert_eq!(
        missing_parent_source.io_source().kind(),
        io::ErrorKind::NotFound
    );
    assert!(!missing_parent.exists());

    drop(source);
    let mut reopened = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
    assert_eq!(reopened.persistent_log_id(), persistent_log_id);
    assert_eq!(
        reopened.load_restart_checkpoint_completeness_baseline()?,
        None
    );
    Ok(())
}

#[test]
fn empty_completeness_and_transaction_slots_reject_cross_namespace_open()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("cross-namespace")?;
    let transaction_path = directory.path().join("checkpoint");
    let completeness_path = directory.path().join("completeness");
    let persistent_log_id = persistent_log_id(1570)?;
    drop(FileRestartCheckpointBaselineSource::create_new(
        &transaction_path,
        persistent_log_id,
    )?);
    drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
        &completeness_path,
        persistent_log_id,
    )?);
    assert!(!transaction_path.join("current").exists());
    assert!(!completeness_path.join("current").exists());
    assert_ne!(
        fs::read(transaction_path.join("control"))?,
        fs::read(completeness_path.join("control"))?
    );

    let transaction_as_completeness =
        FileRestartCheckpointCompletenessBaselineSource::open(&transaction_path)
            .err()
            .ok_or_else(|| {
                io::Error::other("transaction-only slot opened as a completeness slot")
            })?;
    let FileRestartCheckpointSlotOpenError::Format(transaction_as_completeness) =
        transaction_as_completeness
    else {
        return Err(io::Error::other("cross-namespace open was not a format error").into());
    };
    assert_eq!(transaction_as_completeness.offset(), 0);
    assert_eq!(
        transaction_as_completeness.reason(),
        &FileRestartCheckpointSlotFormatErrorReason::HeaderMagic {
            actual: *b"NTSQCKS1"
        }
    );

    let completeness_as_transaction = FileRestartCheckpointBaselineSource::open(&completeness_path)
        .err()
        .ok_or_else(|| io::Error::other("completeness slot opened as a transaction-only slot"))?;
    let FileRestartCheckpointSlotOpenError::Format(completeness_as_transaction) =
        completeness_as_transaction
    else {
        return Err(io::Error::other("cross-namespace open was not a format error").into());
    };
    assert_eq!(completeness_as_transaction.offset(), 0);
    assert_eq!(
        completeness_as_transaction.reason(),
        &FileRestartCheckpointSlotFormatErrorReason::HeaderMagic {
            actual: *b"NTSQCMS1"
        }
    );

    drop(FileRestartCheckpointBaselineSource::open(
        &transaction_path,
    )?);
    drop(FileRestartCheckpointCompletenessBaselineSource::open(
        &completeness_path,
    )?);
    Ok(())
}

#[test]
fn completeness_current_blob_loads_as_untrusted_then_validates_against_real_current_wal()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("current-validates")?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("completeness");
    let persistent_log_id = persistent_log_id(1571)?;
    let mut log =
        FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_log_id)?;
    let mut store = FilePageStore::<2>::create_new(&store_path, persistent_log_id)?;
    let mut coordinator = TransactionCoordinator::open(&mut log)?;

    let active = coordinator.begin()?;
    let (active, dirty) = coordinator.stage_page_write(
        active,
        unlogged_page(LogDurability::lineage(&log), 157, 1, [0x15, 0x71])?,
        &mut log,
    )?;
    let committed = coordinator.commit(active, &mut log)?;
    flush_committed_page(&committed, &mut log, &mut store, dirty)?;
    drop(coordinator);

    let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?;
    let baseline = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let encoded = encode_restart_checkpoint_completeness_baseline(&baseline)?;
    assert_eq!(&encoded[..8], b"NTSQCMP1");
    assert!(!baseline.pages().is_empty());

    drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
        &slot_path,
        persistent_log_id,
    )?);
    write_synced_new(&slot_path.join("current"), &encoded)?;
    write_synced_new(&slot_path.join("candidate"), b"unselected candidate")?;
    let mut source = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
    let loaded = source
        .load_restart_checkpoint_completeness_baseline()?
        .ok_or_else(|| io::Error::other("current completeness checkpoint was reported absent"))?;
    assert_eq!(
        loaded.transactions().persistent_log_id(),
        persistent_log_id.get()
    );
    assert_eq!(
        loaded.transactions().durable_frontier(),
        baseline.durable_frontier()
    );
    assert_eq!(
        loaded.transactions().transactions().len(),
        baseline.transactions().len()
    );
    assert_eq!(loaded.pages().len(), baseline.pages().len());
    assert_eq!(
        loaded.pages()[0].page_number(),
        baseline.pages()[0].page_number().get()
    );
    assert_eq!(
        loaded.replay().frontier(),
        baseline.replay_start().frontier()
    );
    assert_eq!(
        loaded.replay().position(),
        baseline.replay_start().position()
    );
    assert_eq!(
        fs::read(slot_path.join("candidate"))?,
        b"unselected candidate"
    );

    assert_eq!(
        owner.validate_restart_checkpoint_completeness_baseline_against_current_prefix(
            &loaded.as_observation()
        )?,
        baseline
    );

    fs::rename(slot_path.join("candidate"), slot_path.join("current"))?;
    assert_completeness_slot_locked(
        FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)
            .err()
            .ok_or_else(|| {
                io::Error::other("current-file replacement released the control lock")
            })?,
    )?;
    write_synced_new(&slot_path.join("candidate"), &encoded)?;
    fs::rename(slot_path.join("candidate"), slot_path.join("current"))?;
    assert!(
        source
            .load_restart_checkpoint_completeness_baseline()?
            .is_some()
    );
    drop(source);
    assert!(
        FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?
            .load_restart_checkpoint_completeness_baseline()?
            .is_some()
    );
    Ok(())
}

#[test]
fn malformed_wrong_format_and_current_io_failures_are_distinct() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("current-errors")?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("completeness");
    let persistent_log_id = persistent_log_id(1572)?;
    drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
        &slot_path,
        persistent_log_id,
    )?);

    write_synced_new(&slot_path.join("current"), &[0x4e])?;
    let mut source = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
    assert_eq!(
        source.load_restart_checkpoint_completeness_baseline(),
        Err(
            FileRestartCheckpointCompletenessBaselineSourceError::Decode(
                RestartCheckpointCompletenessBaselineDecodeError::Truncated {
                    expected_length: 128,
                    actual_length: 1,
                }
            )
        )
    );
    drop(source);

    let transaction_only_bytes = {
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_log_id)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, persistent_log_id)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let (active, dirty) = coordinator.stage_page_write(
            active,
            unlogged_page(LogDurability::lineage(&log), 158, 1, [0x15, 0x72])?,
            &mut log,
        )?;
        let committed = coordinator.commit(active, &mut log)?;
        flush_committed_page(&committed, &mut log, &mut store, dirty)?;
        drop(coordinator);
        let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
            .recover()?
            .analyze_restart()?;
        let baseline = owner.prepare_restart_checkpoint_baseline_from_current_prefix()?;
        assert_eq!(baseline.transactions().len(), 1);
        encode_restart_checkpoint_baseline(&baseline)?
    };
    assert_eq!(&transaction_only_bytes[..8], b"NTSQCKP1");
    assert!(transaction_only_bytes.len() >= 128);
    fs::remove_file(slot_path.join("current"))?;
    write_synced_new(&slot_path.join("current"), &transaction_only_bytes)?;
    let mut source = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
    assert_eq!(
        source.load_restart_checkpoint_completeness_baseline(),
        Err(
            FileRestartCheckpointCompletenessBaselineSourceError::Decode(
                RestartCheckpointCompletenessBaselineDecodeError::HeaderMagicMismatch {
                    actual: *b"NTSQCKP1"
                }
            )
        )
    );
    drop(source);

    fs::remove_file(slot_path.join("current"))?;
    fs::create_dir(slot_path.join("current"))?;
    let mut source = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
    let error = source
        .load_restart_checkpoint_completeness_baseline()
        .err()
        .ok_or_else(|| io::Error::other("current directory loaded as a completeness blob"))?;
    let FileRestartCheckpointCompletenessBaselineSourceError::Io(io_source) = error else {
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
            .load_restart_checkpoint_completeness_baseline()
            .err()
            .ok_or_else(|| io::Error::other("dangling current symlink appeared absent"))?;
        let FileRestartCheckpointCompletenessBaselineSourceError::Io(dangling) = dangling else {
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
            .load_restart_checkpoint_completeness_baseline()
            .err()
            .ok_or_else(|| io::Error::other("removed slot directory appeared empty"))?;
        let FileRestartCheckpointCompletenessBaselineSourceError::Io(removed_slot) = removed_slot
        else {
            return Err(io::Error::other("removed slot changed error category").into());
        };
        assert_eq!(
            removed_slot.stage(),
            FileRestartCheckpointSlotIoStage::VerifyCurrentAbsence
        );
    }

    let capacity = FileRestartCheckpointCompletenessBaselineSourceError::CurrentCapacityExhausted {
        length: usize::MAX,
    };
    assert!(Error::source(&capacity).is_none());
    let out_of_range =
        FileRestartCheckpointCompletenessBaselineSourceError::CurrentLengthOutOfRange {
            actual: u64::MAX,
        };
    assert!(Error::source(&out_of_range).is_none());
    let changed = FileRestartCheckpointCompletenessBaselineSourceError::CurrentLengthChanged {
        before: 128,
        after: 144,
    };
    assert!(Error::source(&changed).is_none());
    assert_ne!(changed.to_string(), out_of_range.to_string());
    Ok(())
}

#[test]
fn completeness_control_lock_precedes_format_parsing_and_malformed_control_fails_after_release()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("control-order")?;
    let slot_path = directory.path().join("completeness");
    let persistent_log_id = persistent_log_id(1573)?;
    let source =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;

    let mut noncooperating_writer = OpenOptions::new()
        .write(true)
        .open(slot_path.join("control"))?;
    noncooperating_writer.write_all(b"X")?;
    noncooperating_writer.sync_all()?;
    drop(noncooperating_writer);

    assert_completeness_slot_locked(
        FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)
            .err()
            .ok_or_else(|| io::Error::other("contending open parsed locked control bytes"))?,
    )?;
    drop(source);

    let malformed = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)
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

    let short_slot = directory.path().join("short-completeness");
    drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
        &short_slot,
        persistent_log_id,
    )?);
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(short_slot.join("control"))?
        .sync_all()?;
    let short = FileRestartCheckpointCompletenessBaselineSource::open(&short_slot)
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
fn completeness_composition_opens_wal_page_store_slot_in_fixed_order_and_releases_prefixes()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("composition-order")?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("completeness");
    let missing_log = directory.path().join("missing-wal.bin");
    let missing_store = directory.path().join("missing-pages.bin");
    let missing_slot = directory.path().join("missing-completeness");
    let persistent_log_id = persistent_log_id(1574)?;
    drop(FileCommitLog::<1>::create_new_transaction_page_capable(
        &log_path,
        persistent_log_id,
    )?);
    drop(FilePageStore::<1>::create_new(
        &store_path,
        persistent_log_id,
    )?);
    let held_checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;

    assert!(matches!(
        open_transaction_page_storage_with_completeness_checkpoint::<1, _, _, _>(
            &missing_log,
            &store_path,
            &slot_path,
        ),
        Err(FileTransactionPageStorageCompletenessCheckpointOpenError::CommitLog(_))
    ));
    assert!(matches!(
        open_transaction_page_storage_with_completeness_checkpoint::<1, _, _, _>(
            &log_path,
            &missing_store,
            &slot_path,
        ),
        Err(FileTransactionPageStorageCompletenessCheckpointOpenError::PageStore(_))
    ));
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);

    let checkpoint_error =
        open_transaction_page_storage_with_completeness_checkpoint::<1, _, _, _>(
            &log_path,
            &store_path,
            &slot_path,
        )
        .err()
        .ok_or_else(|| io::Error::other("composition bypassed held completeness lock"))?;
    let FileTransactionPageStorageCompletenessCheckpointOpenError::Checkpoint(
        FileRestartCheckpointSlotOpenError::Io(checkpoint_source),
    ) = checkpoint_error
    else {
        return Err(io::Error::other("completeness contention changed open stage").into());
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
    let missing_checkpoint =
        open_transaction_page_storage_with_completeness_checkpoint::<1, _, _, _>(
            &log_path,
            &store_path,
            &missing_slot,
        )
        .err()
        .ok_or_else(|| io::Error::other("missing completeness slot opened"))?;
    assert!(matches!(
        missing_checkpoint,
        FileTransactionPageStorageCompletenessCheckpointOpenError::Checkpoint(_)
    ));
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);
    drop(FilePageStore::<1>::open(&store_path)?);

    let opened = open_transaction_page_storage_with_completeness_checkpoint::<1, _, _, _>(
        &log_path,
        &store_path,
        &slot_path,
    )?;
    assert_composition_locks_held(&log_path, &store_path, &slot_path)?;

    let selection = opened.select_restart_checkpoint_completeness();
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Absent(absent) = selection
    else {
        return Err(io::Error::other("new completeness slot was not absent").into());
    };
    assert_composition_locks_held(&log_path, &store_path, &slot_path)?;

    let uncheckpointed = absent.continue_with_full_recovery();
    assert_composition_locks_held(&log_path, &store_path, &slot_path)?;
    let recovered = uncheckpointed.recover()?;
    assert_composition_locks_held(&log_path, &store_path, &slot_path)?;
    let mut analyzed = recovered.analyze_restart()?;
    assert_composition_locks_held(&log_path, &store_path, &slot_path)?;
    let receipt =
        analyzed.publish_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    assert_eq!(receipt.persistent_log_id(), persistent_log_id);
    assert_eq!(receipt.durable_frontier(), None);
    assert_composition_locks_held(&log_path, &store_path, &slot_path)?;

    let (log, store, _, _, checkpoint) = analyzed.into_parts();
    assert_composition_locks_held(&log_path, &store_path, &slot_path)?;
    drop((log, store, checkpoint));
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);
    drop(FilePageStore::<1>::open(&store_path)?);
    drop(FileRestartCheckpointCompletenessBaselineSource::open(
        &slot_path,
    )?);
    Ok(())
}

#[test]
fn completeness_composition_rejects_storage_and_slot_lineage_mismatch_before_returning()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("composition-lineage")?;
    let log_path = directory.path().join("wal.bin");
    let store_path = directory.path().join("pages.bin");
    let foreign_store_path = directory.path().join("foreign-pages.bin");
    let slot_path = directory.path().join("completeness");
    let foreign_slot_path = directory.path().join("foreign-completeness");
    let foreign_log_id = persistent_log_id(1576)?;
    let persistent_log_id = persistent_log_id(1575)?;
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
    drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
        &slot_path,
        persistent_log_id,
    )?);
    drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
        &foreign_slot_path,
        foreign_log_id,
    )?);

    assert_eq!(
        open_transaction_page_storage_with_completeness_checkpoint::<1, _, _, _>(
            &log_path,
            &foreign_store_path,
            directory.path().join("not-consulted"),
        )
        .err(),
        Some(
            FileTransactionPageStorageCompletenessCheckpointOpenError::StoragePersistentLogIdMismatch {
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
        open_transaction_page_storage_with_completeness_checkpoint::<1, _, _, _>(
            &log_path,
            &store_path,
            &foreign_slot_path,
        )
        .err(),
        Some(
            FileTransactionPageStorageCompletenessCheckpointOpenError::CheckpointPersistentLogIdMismatch {
                storage: persistent_log_id,
                checkpoint: foreign_log_id,
            }
        )
    );
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        &log_path,
    )?);
    drop(FilePageStore::<1>::open(&store_path)?);
    drop(FileRestartCheckpointCompletenessBaselineSource::open(
        &foreign_slot_path,
    )?);
    Ok(())
}

fn assert_completeness_slot_locked(
    error: FileRestartCheckpointSlotOpenError,
) -> Result<(), io::Error> {
    let FileRestartCheckpointSlotOpenError::Io(source) = error else {
        return Err(io::Error::other(
            "completeness lock contention was not an I/O failure",
        ));
    };
    if source.stage() != FileRestartCheckpointSlotIoStage::AcquireExclusiveControlLock
        || source.io_source().kind() != io::ErrorKind::WouldBlock
    {
        return Err(io::Error::other(
            "completeness lock contention had the wrong stage or cause",
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

fn assert_composition_locks_held(
    log_path: &Path,
    store_path: &Path,
    slot_path: &Path,
) -> Result<(), io::Error> {
    if FileCommitLog::<1>::open_transaction_page_capable(log_path).is_ok() {
        return Err(io::Error::other(
            "composition released its transaction-page WAL lock",
        ));
    }
    assert_page_store_locked(
        FilePageStore::<1>::open(store_path)
            .err()
            .ok_or_else(|| io::Error::other("composition released its page-store lock"))?,
    )?;
    assert_completeness_slot_locked(
        FileRestartCheckpointCompletenessBaselineSource::open(slot_path)
            .err()
            .ok_or_else(|| io::Error::other("composition released its completeness lock"))?,
    )
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
            "ntsql-completeness-file-source-{}-{name}-{unique}",
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
