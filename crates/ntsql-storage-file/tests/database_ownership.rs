use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseFileIdentity, DatabaseFileRole, DatabaseId,
    DatabaseLifecycleGeneration, DatabaseLifecycleStage, DatabaseManifest,
    DatabaseRequiredFeatures, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_storage_file::{
    DATABASE_OWNER_CONTROL_V1_LENGTH, DatabaseOwnerControlDecodeError, FileCommitLog,
    FileDatabaseLayout, FileDatabaseLockRole, FileDatabaseOwnershipIoStage,
    FileDatabaseOwnershipOpenError, FileIoStage, FileOpenError, FilePageStore,
    FileRestartCheckpointCompletenessBaselineSource, FileRestartCheckpointSlotIoStage,
    PageStoreIoStage, PageStoreOpenError, decode_database_owner_control, encode_database_manifest,
    encode_database_owner_control, open_file_database_ownership,
    open_recovery_required_file_database,
};
use ntsql_wal::PersistentLogId;

const CHECKSUM_SEED: u64 = 0x4e54_5351_4c43_4b31;
const CHECKSUM_MULTIPLIER: u64 = 0x4e54_5351_4c57_414d;
const CHECKSUM_XOR: u64 = 0x4348_4543_4b53_554d;

#[test]
fn database_owner_control_has_exact_golden_bytes_and_field_validation() -> Result<(), Box<dyn Error>>
{
    let database_id = database_id(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)?;
    let encoded = encode_database_owner_control(database_id);
    let expected = [
        0x4e, 0x54, 0x53, 0x51, 0x44, 0x42, 0x4f, 0x31, 0x00, 0x01, 0x00, 0x40, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xcc, 0xc2, 0xc3, 0xc6,
        0xb6, 0xe8, 0xec, 0x40,
    ];
    assert_eq!(encoded, expected);
    assert_eq!(decode_database_owner_control(&encoded), Ok(database_id));

    for prefix in 0..DATABASE_OWNER_CONTROL_V1_LENGTH {
        assert_eq!(
            decode_database_owner_control(&encoded[..prefix]),
            Err(DatabaseOwnerControlDecodeError::Truncated { actual: prefix })
        );
    }
    let mut trailing = encoded.to_vec();
    trailing.push(0);
    assert_eq!(
        decode_database_owner_control(&trailing),
        Err(DatabaseOwnerControlDecodeError::TrailingBytes {
            actual: DATABASE_OWNER_CONTROL_V1_LENGTH + 1,
        })
    );

    let mut wrong_magic = encoded;
    wrong_magic[0] = 0;
    assert_eq!(
        decode_database_owner_control(&wrong_magic),
        Err(DatabaseOwnerControlDecodeError::MagicMismatch {
            actual: [0, 0x54, 0x53, 0x51, 0x44, 0x42, 0x4f, 0x31],
        })
    );
    let mut wrong_version = encoded;
    write_u16(&mut wrong_version, 8, 2);
    assert_eq!(
        decode_database_owner_control(&wrong_version),
        Err(DatabaseOwnerControlDecodeError::UnsupportedVersion { actual: 2 })
    );
    let mut wrong_length = encoded;
    write_u16(&mut wrong_length, 10, 63);
    assert_eq!(
        decode_database_owner_control(&wrong_length),
        Err(DatabaseOwnerControlDecodeError::FrameLengthMismatch { actual: 63 })
    );
    for actual in [1_u32, 0x8000_0000] {
        let mut flags = encoded;
        write_u32(&mut flags, 12, actual);
        assert_eq!(
            decode_database_owner_control(&flags),
            Err(DatabaseOwnerControlDecodeError::HeaderFlagsUnsupported { actual })
        );
    }
    let mut wrong_checksum = encoded;
    wrong_checksum[63] ^= 1;
    assert!(matches!(
        decode_database_owner_control(&wrong_checksum),
        Err(DatabaseOwnerControlDecodeError::ChecksumMismatch { .. })
    ));
    for offset in 32..56 {
        let mut reserved = encoded;
        reserved[offset] = 1;
        replace_control_checksum(&mut reserved);
        assert_eq!(
            decode_database_owner_control(&reserved),
            Err(DatabaseOwnerControlDecodeError::ReservedByteNonZero { offset, actual: 1 })
        );
    }
    let mut zero_id = encoded;
    zero_id[16..32].fill(0);
    replace_control_checksum(&mut zero_id);
    assert_eq!(
        decode_database_owner_control(&zero_id),
        Err(DatabaseOwnerControlDecodeError::DatabaseIdZero)
    );
    Ok(())
}

#[test]
fn successful_open_retains_every_lock_until_owner_drop() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("success", 1, 101)?;
    let opened = open_file_database_ownership::<1>(database.database_id, database.layout.clone())?;
    assert_eq!(opened.identity(), database.manifest.composition_identity());
    assert_eq!(opened.stage(), DatabaseLifecycleStage::ManifestSelected);
    assert_all_locks_held(&database.layout)?;

    drop(opened);
    drop(open_file_database_ownership::<1>(
        database.database_id,
        database.layout.clone(),
    )?);
    Ok(())
}

#[test]
fn successor_children_bind_stable_storage_across_manifest_generations() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::new_successor("successor", 500, 600)?;
    let opened =
        open_recovery_required_file_database::<1>(database.database_id, database.layout.clone())?;
    assert_eq!(opened.identity(), database.manifest.composition_identity());
    assert_eq!(opened.stage(), DatabaseLifecycleStage::RecoveryRequired);
    assert_all_locks_held(&database.layout)?;
    drop(opened);

    let successor = database.manifest.next_recovery_required()?;
    overwrite_synced(
        database.layout.manifest(),
        &encode_database_manifest(&successor),
    )?;
    let reopened =
        open_recovery_required_file_database::<1>(database.database_id, database.layout.clone())?;
    assert_eq!(reopened.identity(), successor.composition_identity());
    assert_eq!(
        reopened.identity().storage_identity(),
        database.manifest.composition_identity().storage_identity()
    );
    Ok(())
}

#[test]
fn legacy_children_cannot_claim_stable_storage_binding() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("legacy-binding", 501, 601)?;
    append_synced(database.layout.wal(), &[0xaa])?;
    append_synced(database.layout.page_store(), &[0xbb])?;
    let wal_before = fs::read(database.layout.wal())?;
    let page_store_before = fs::read(database.layout.page_store())?;
    assert!(matches!(
        open_recovery_required_file_database::<1>(database.database_id, database.layout.clone(),),
        Err(
            FileDatabaseOwnershipOpenError::StableStorageIdentityUnavailable {
                role: DatabaseFileRole::Wal,
            }
        )
    ));
    assert_eq!(fs::read(database.layout.wal())?, wal_before);
    assert_eq!(fs::read(database.layout.page_store())?, page_store_before);
    drop(open_file_database_ownership::<1>(
        database.database_id,
        database.layout.clone(),
    )?);
    Ok(())
}

#[test]
fn every_successor_child_identity_field_is_physically_checked() -> Result<(), Box<dyn Error>> {
    for (index, role) in [
        DatabaseFileRole::Wal,
        DatabaseFileRole::PageStore,
        DatabaseFileRole::RestartCheckpoint,
    ]
    .into_iter()
    .enumerate()
    {
        let database = TestDatabase::new_successor("child-database-id", 510 + index as u128, 610)?;
        mutate_child_identity(
            &database.layout,
            role,
            ChildIdentityMutation::DatabaseId(9_000 + index as u128),
        )?;
        assert!(matches!(
            open_recovery_required_file_database::<1>(
                database.database_id,
                database.layout.clone(),
            ),
            Err(FileDatabaseOwnershipOpenError::ChildDatabaseIdMismatch {
                role: actual,
                ..
            }) if actual == role
        ));

        let database = TestDatabase::new_successor("child-file-id", 520 + index as u128, 620)?;
        mutate_child_identity(
            &database.layout,
            role,
            ChildIdentityMutation::FileId(10_000 + index as u128),
        )?;
        assert!(matches!(
            open_recovery_required_file_database::<1>(
                database.database_id,
                database.layout.clone(),
            ),
            Err(FileDatabaseOwnershipOpenError::ChildFileIdMismatch {
                role: actual,
                ..
            }) if actual == role
        ));

        let database = TestDatabase::new_successor("child-role", 530 + index as u128, 630)?;
        mutate_child_identity(
            &database.layout,
            role,
            ChildIdentityMutation::Role(next_role(role)),
        )?;
        assert!(matches!(
            open_recovery_required_file_database::<1>(
                database.database_id,
                database.layout.clone(),
            ),
            Err(FileDatabaseOwnershipOpenError::ChildFileRoleMismatch {
                expected,
                actual,
            }) if expected == role && actual == next_role(role)
        ));
    }
    Ok(())
}

#[test]
fn database_owner_contention_precedes_manifest_parsing() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("owner-contention", 2, 102)?;
    let opened = open_file_database_ownership::<1>(database.database_id, database.layout.clone())?;
    overwrite_synced(database.layout.manifest(), &[0_u8; 160])?;

    assert_lock_error(
        open_file_database_ownership::<1>(database.database_id, database.layout.clone())
            .err()
            .ok_or_else(|| io::Error::other("second database owner unexpectedly opened"))?,
        FileDatabaseLockRole::DatabaseOwner,
    )?;
    drop(opened);
    assert!(matches!(
        open_file_database_ownership::<1>(database.database_id, database.layout.clone()),
        Err(FileDatabaseOwnershipOpenError::Manifest(_))
    ));
    Ok(())
}

#[test]
fn manifest_contention_releases_the_stable_owner() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("manifest-contention", 3, 103)?;
    let manifest_lock = open_and_lock(database.layout.manifest())?;

    assert_lock_error(
        open_file_database_ownership::<1>(database.database_id, database.layout.clone())
            .err()
            .ok_or_else(|| io::Error::other("locked manifest unexpectedly opened"))?,
        FileDatabaseLockRole::Manifest,
    )?;
    assert_path_lockable(database.layout.database_owner())?;
    drop(manifest_lock);
    drop(open_file_database_ownership::<1>(
        database.database_id,
        database.layout.clone(),
    )?);
    Ok(())
}

#[test]
fn partial_child_acquisition_releases_every_earlier_lock() -> Result<(), Box<dyn Error>> {
    let missing = TestDatabase::new("missing-wal", 4, 104)?;
    fs::remove_file(missing.layout.wal())?;
    let missing_error =
        open_file_database_ownership::<1>(missing.database_id, missing.layout.clone())
            .err()
            .ok_or_else(|| io::Error::other("missing WAL unexpectedly opened"))?;
    let FileDatabaseOwnershipOpenError::Io(source) = missing_error else {
        return Err(io::Error::other("missing WAL changed error category").into());
    };
    assert_eq!(
        source.stage(),
        FileDatabaseOwnershipIoStage::OpenFile {
            role: FileDatabaseLockRole::Wal,
        }
    );
    assert_path_lockable(missing.layout.database_owner())?;
    assert_path_lockable(missing.layout.manifest())?;

    let page = TestDatabase::new("held-page", 5, 105)?;
    let held_page = FilePageStore::<1>::open(page.layout.page_store())?;
    let page_error = open_file_database_ownership::<1>(page.database_id, page.layout.clone())
        .err()
        .ok_or_else(|| io::Error::other("held page store unexpectedly opened"))?;
    let FileDatabaseOwnershipOpenError::PageStoreOpen(PageStoreOpenError::Io(source)) = page_error
    else {
        return Err(io::Error::other("held page store changed acquisition stage").into());
    };
    assert_eq!(source.stage(), PageStoreIoStage::AcquireExclusiveLock);
    assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);
    assert_path_lockable(page.layout.database_owner())?;
    assert_path_lockable(page.layout.manifest())?;
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        page.layout.wal(),
    )?);
    drop(held_page);

    let checkpoint = TestDatabase::new("held-checkpoint", 6, 106)?;
    let held_checkpoint = FileRestartCheckpointCompletenessBaselineSource::open(
        checkpoint.layout.restart_checkpoint(),
    )?;
    let checkpoint_error =
        open_file_database_ownership::<1>(checkpoint.database_id, checkpoint.layout.clone())
            .err()
            .ok_or_else(|| io::Error::other("held checkpoint unexpectedly opened"))?;
    let FileDatabaseOwnershipOpenError::RestartCheckpointOpen(
        ntsql_storage_file::FileRestartCheckpointSlotOpenError::Io(source),
    ) = checkpoint_error
    else {
        return Err(io::Error::other("held checkpoint changed acquisition stage").into());
    };
    assert_eq!(
        source.stage(),
        FileRestartCheckpointSlotIoStage::AcquireExclusiveControlLock
    );
    assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);
    assert_path_lockable(checkpoint.layout.database_owner())?;
    assert_path_lockable(checkpoint.layout.manifest())?;
    drop(FileCommitLog::<1>::open_transaction_page_capable(
        checkpoint.layout.wal(),
    )?);
    drop(FilePageStore::<1>::open(checkpoint.layout.page_store())?);
    drop(held_checkpoint);
    Ok(())
}

#[test]
fn owner_manifest_and_each_child_reject_foreign_identity() -> Result<(), Box<dyn Error>> {
    let owner = TestDatabase::new("foreign-owner", 7, 107)?;
    overwrite_synced(
        owner.layout.database_owner(),
        &encode_database_owner_control(database_id(700)?),
    )?;
    assert!(matches!(
        open_file_database_ownership::<1>(owner.database_id, owner.layout.clone()),
        Err(FileDatabaseOwnershipOpenError::DatabaseOwnerIdMismatch { .. })
    ));

    let manifest = TestDatabase::new("foreign-manifest", 8, 108)?;
    let foreign_manifest =
        manifest.manifest_with(database_id(800)?, manifest.persistent_log_id, [3, 1, 1])?;
    overwrite_synced(
        manifest.layout.manifest(),
        &encode_database_manifest(&foreign_manifest),
    )?;
    assert!(matches!(
        open_file_database_ownership::<1>(manifest.database_id, manifest.layout.clone()),
        Err(FileDatabaseOwnershipOpenError::ManifestDatabaseIdMismatch { .. })
    ));

    for (index, role) in [
        DatabaseFileRole::Wal,
        DatabaseFileRole::PageStore,
        DatabaseFileRole::RestartCheckpoint,
    ]
    .into_iter()
    .enumerate()
    {
        let database = TestDatabase::new("foreign-child", 20 + index as u128, 120 + index as u128)?;
        let foreign_id = persistent_log_id(900 + index as u128)?;
        let foreign_path = database.directory.path().join(format!("foreign-{index}"));
        let layout = match role {
            DatabaseFileRole::Wal => {
                drop(FileCommitLog::<1>::create_new_transaction_page_capable(
                    &foreign_path,
                    foreign_id,
                )?);
                layout_with(&database.layout, role, &foreign_path)
            }
            DatabaseFileRole::PageStore => {
                drop(FilePageStore::<1>::create_new(&foreign_path, foreign_id)?);
                layout_with(&database.layout, role, &foreign_path)
            }
            DatabaseFileRole::RestartCheckpoint => {
                drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
                    &foreign_path,
                    foreign_id,
                )?);
                layout_with(&database.layout, role, &foreign_path)
            }
        };
        assert_eq!(
            persistent_mismatch(
                open_file_database_ownership::<1>(database.database_id, layout)
                    .err()
                    .ok_or_else(|| io::Error::other("foreign child unexpectedly opened"))?
            ),
            Some((role, database.persistent_log_id, foreign_id))
        );
    }
    Ok(())
}

#[test]
fn late_child_rejection_precedes_tail_repair_and_wal_candidate_cleanup()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("deferred-repair", 29, 129)?;
    append_synced(database.layout.wal(), &[0xaa])?;
    append_synced(database.layout.page_store(), &[0xbb])?;
    let wal_len = fs::metadata(database.layout.wal())?.len();
    let page_len = fs::metadata(database.layout.page_store())?.len();
    let candidate_path = wal_reclamation_candidate(database.layout.wal())?;
    write_synced_new(&candidate_path, b"candidate-must-remain")?;

    let foreign_checkpoint = database.directory.path().join("foreign-checkpoint");
    let foreign_id = persistent_log_id(929)?;
    drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
        &foreign_checkpoint,
        foreign_id,
    )?);
    let layout = layout_with(
        &database.layout,
        DatabaseFileRole::RestartCheckpoint,
        &foreign_checkpoint,
    );
    assert_eq!(
        persistent_mismatch(
            open_file_database_ownership::<1>(database.database_id, layout)
                .err()
                .ok_or_else(|| io::Error::other("foreign checkpoint unexpectedly opened"))?
        ),
        Some((
            DatabaseFileRole::RestartCheckpoint,
            database.persistent_log_id,
            foreign_id,
        ))
    );
    assert_eq!(fs::metadata(database.layout.wal())?.len(), wal_len);
    assert_eq!(fs::metadata(database.layout.page_store())?.len(), page_len);
    assert_eq!(fs::read(&candidate_path)?, b"candidate-must-remain");
    Ok(())
}

#[test]
fn every_child_format_requirement_is_checked_before_composition_open() -> Result<(), Box<dyn Error>>
{
    for (index, role) in [
        DatabaseFileRole::Wal,
        DatabaseFileRole::PageStore,
        DatabaseFileRole::RestartCheckpoint,
    ]
    .into_iter()
    .enumerate()
    {
        let database = TestDatabase::new("format", 30 + index as u128, 130 + index as u128)?;
        let mut formats = [3, 1, 1];
        formats[role_index(role)] += 1;
        let changed =
            database.manifest_with(database.database_id, database.persistent_log_id, formats)?;
        overwrite_synced(
            database.layout.manifest(),
            &encode_database_manifest(&changed),
        )?;
        assert!(matches!(
            open_file_database_ownership::<1>(database.database_id, database.layout.clone()),
            Err(FileDatabaseOwnershipOpenError::StorageFormatVersionMismatch {
                role: actual_role,
                ..
            }) if actual_role == role
        ));
        drop(FileCommitLog::<1>::open_transaction_page_capable(
            database.layout.wal(),
        )?);
        drop(FilePageStore::<1>::open(database.layout.page_store())?);
        drop(FileRestartCheckpointCompletenessBaselineSource::open(
            database.layout.restart_checkpoint(),
        )?);
    }
    Ok(())
}

#[test]
fn missing_and_reversed_roles_fail_closed_without_fallback() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("reversed", 40, 140)?;
    let reversed = FileDatabaseLayout::new(
        database.layout.database_owner(),
        database.layout.manifest(),
        database.layout.page_store(),
        database.layout.wal(),
        database.layout.restart_checkpoint(),
    );
    assert!(matches!(
        open_file_database_ownership::<1>(database.database_id, reversed),
        Err(FileDatabaseOwnershipOpenError::WalOpen(
            FileOpenError::Format(_)
        ))
    ));
    assert!(matches!(
        open_file_database_ownership::<1>(
            database.database_id,
            FileDatabaseLayout::new(
                database.layout.database_owner(),
                database.layout.manifest(),
                database.directory.path().join("absent-wal"),
                database.layout.page_store(),
                database.layout.restart_checkpoint(),
            ),
        ),
        Err(FileDatabaseOwnershipOpenError::Io(_))
    ));
    Ok(())
}

#[test]
fn exact_wal_candidate_path_cannot_select_a_database_role() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("candidate-path", 41, 141)?;
    let candidate_path = wal_reclamation_candidate(database.layout.wal())?;
    fs::rename(database.layout.manifest(), &candidate_path)?;
    let layout = FileDatabaseLayout::new(
        database.layout.database_owner(),
        &candidate_path,
        database.layout.wal(),
        database.layout.page_store(),
        database.layout.restart_checkpoint(),
    );
    assert!(matches!(
        open_file_database_ownership::<1>(database.database_id, layout),
        Err(
            FileDatabaseOwnershipOpenError::WalReclamationCandidateCollision {
                role: FileDatabaseLockRole::Manifest,
            }
        )
    ));
    assert!(candidate_path.exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn every_later_role_rejects_a_hard_link_to_an_earlier_opened_object() -> Result<(), Box<dyn Error>>
{
    let roles = [
        FileDatabaseLockRole::DatabaseOwner,
        FileDatabaseLockRole::Manifest,
        FileDatabaseLockRole::Wal,
        FileDatabaseLockRole::PageStore,
        FileDatabaseLockRole::RestartCheckpoint,
    ];
    let mut case = 0_u128;
    for (first_index, first) in roles.iter().copied().enumerate() {
        for second in roles.iter().copied().skip(first_index + 1) {
            case += 1;
            let database = TestDatabase::new("alias", 100 + case, 200 + case)?;
            let first_path = role_path(&database.layout, first);
            let second_path = role_path(&database.layout, second);
            fs::remove_file(&second_path)?;
            fs::hard_link(&first_path, &second_path)?;

            assert!(matches!(
                open_file_database_ownership::<1>(database.database_id, database.layout.clone()),
                Err(FileDatabaseOwnershipOpenError::OpenedObjectAlias {
                    first: actual_first,
                    second: actual_second,
                }) if actual_first == first && actual_second == second
            ));
            assert_path_lockable(database.layout.database_owner())?;
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn wal_reclamation_candidate_rejects_every_database_lock_target() -> Result<(), Box<dyn Error>> {
    for (index, role) in [
        FileDatabaseLockRole::DatabaseOwner,
        FileDatabaseLockRole::Manifest,
        FileDatabaseLockRole::Wal,
        FileDatabaseLockRole::PageStore,
        FileDatabaseLockRole::RestartCheckpoint,
    ]
    .into_iter()
    .enumerate()
    {
        let database =
            TestDatabase::new("candidate-alias", 150 + index as u128, 250 + index as u128)?;
        let candidate_path = wal_reclamation_candidate(database.layout.wal())?;
        fs::hard_link(role_path(&database.layout, role), &candidate_path)?;
        assert!(matches!(
            open_file_database_ownership::<1>(database.database_id, database.layout.clone()),
            Err(
                FileDatabaseOwnershipOpenError::WalReclamationCandidateCollision {
                    role: actual,
                }
            ) if actual == role
        ));
        assert!(candidate_path.exists());
        assert_path_lockable(database.layout.database_owner())?;
    }
    Ok(())
}

fn persistent_mismatch(
    error: FileDatabaseOwnershipOpenError,
) -> Option<(DatabaseFileRole, PersistentLogId, PersistentLogId)> {
    match error {
        FileDatabaseOwnershipOpenError::PersistentLogIdMismatch {
            role,
            expected,
            actual,
        } => Some((role, expected, actual)),
        _ => None,
    }
}

fn assert_all_locks_held(layout: &FileDatabaseLayout) -> Result<(), Box<dyn Error>> {
    assert_path_contended(layout.database_owner())?;
    assert_path_contended(layout.manifest())?;

    let wal_error = FileCommitLog::<1>::open_transaction_page_capable(layout.wal())
        .err()
        .ok_or_else(|| io::Error::other("database owner released the WAL lock"))?;
    let ntsql_storage_file::FileOpenError::Io(source) = wal_error else {
        return Err(io::Error::other("WAL contention changed error category").into());
    };
    assert_eq!(source.stage(), FileIoStage::AcquireExclusiveLock);
    assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);

    let page_error = FilePageStore::<1>::open(layout.page_store())
        .err()
        .ok_or_else(|| io::Error::other("database owner released the page-store lock"))?;
    let ntsql_storage_file::PageStoreOpenError::Io(source) = page_error else {
        return Err(io::Error::other("page contention changed error category").into());
    };
    assert_eq!(source.stage(), PageStoreIoStage::AcquireExclusiveLock);
    assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);

    let checkpoint_error =
        FileRestartCheckpointCompletenessBaselineSource::open(layout.restart_checkpoint())
            .err()
            .ok_or_else(|| io::Error::other("database owner released the checkpoint lock"))?;
    let ntsql_storage_file::FileRestartCheckpointSlotOpenError::Io(source) = checkpoint_error
    else {
        return Err(io::Error::other("checkpoint contention changed error category").into());
    };
    assert_eq!(
        source.stage(),
        FileRestartCheckpointSlotIoStage::AcquireExclusiveControlLock
    );
    assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);
    Ok(())
}

fn assert_lock_error(
    error: FileDatabaseOwnershipOpenError,
    role: FileDatabaseLockRole,
) -> Result<(), io::Error> {
    let FileDatabaseOwnershipOpenError::Io(source) = error else {
        return Err(io::Error::other("lock contention was not an I/O failure"));
    };
    if source.stage() != (FileDatabaseOwnershipIoStage::AcquireExclusiveLock { role })
        || source.io_source().kind() != io::ErrorKind::WouldBlock
    {
        return Err(io::Error::other(
            "lock contention had the wrong stage or cause",
        ));
    }
    Ok(())
}

fn assert_path_contended(path: &Path) -> Result<(), io::Error> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let error = file
        .try_lock()
        .err()
        .ok_or_else(|| io::Error::other("expected file lock contention"))?;
    let error: io::Error = error.into();
    if error.kind() != io::ErrorKind::WouldBlock {
        return Err(io::Error::other("file contention had the wrong cause"));
    }
    Ok(())
}

fn assert_path_lockable(path: &Path) -> Result<(), io::Error> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    file.try_lock()?;
    Ok(())
}

fn open_and_lock(path: &Path) -> Result<File, io::Error> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    file.try_lock()?;
    Ok(file)
}

fn layout_with(
    layout: &FileDatabaseLayout,
    role: DatabaseFileRole,
    replacement: &Path,
) -> FileDatabaseLayout {
    FileDatabaseLayout::new(
        layout.database_owner(),
        layout.manifest(),
        if role == DatabaseFileRole::Wal {
            replacement
        } else {
            layout.wal()
        },
        if role == DatabaseFileRole::PageStore {
            replacement
        } else {
            layout.page_store()
        },
        if role == DatabaseFileRole::RestartCheckpoint {
            replacement
        } else {
            layout.restart_checkpoint()
        },
    )
}

fn role_path(layout: &FileDatabaseLayout, role: FileDatabaseLockRole) -> PathBuf {
    match role {
        FileDatabaseLockRole::DatabaseOwner => layout.database_owner().to_path_buf(),
        FileDatabaseLockRole::Manifest => layout.manifest().to_path_buf(),
        FileDatabaseLockRole::Wal => layout.wal().to_path_buf(),
        FileDatabaseLockRole::PageStore => layout.page_store().to_path_buf(),
        FileDatabaseLockRole::RestartCheckpoint => layout.restart_checkpoint().join("control"),
    }
}

fn role_index(role: DatabaseFileRole) -> usize {
    match role {
        DatabaseFileRole::Wal => 0,
        DatabaseFileRole::PageStore => 1,
        DatabaseFileRole::RestartCheckpoint => 2,
    }
}

struct TestDatabase {
    directory: TestDirectory,
    layout: FileDatabaseLayout,
    database_id: DatabaseId,
    persistent_log_id: PersistentLogId,
    manifest: DatabaseManifest,
}

impl TestDatabase {
    fn new(
        tag: &str,
        database_value: u128,
        persistent_value: u128,
    ) -> Result<Self, Box<dyn Error>> {
        let directory = TestDirectory::new(tag)?;
        let database_id = database_id(database_value)?;
        let persistent_log_id = persistent_log_id(persistent_value)?;
        let owner_path = directory.path().join("owner");
        let manifest_path = directory.path().join("manifest");
        let wal_path = directory.path().join("wal");
        let page_store_path = directory.path().join("pages");
        let checkpoint_path = directory.path().join("checkpoint");
        let manifest = manifest(
            database_id,
            persistent_log_id,
            [
                1_000 + database_value,
                2_000 + database_value,
                3_000 + database_value,
            ],
            [3, 1, 1],
        )?;

        write_synced_new(&owner_path, &encode_database_owner_control(database_id))?;
        write_synced_new(&manifest_path, &encode_database_manifest(&manifest))?;
        drop(FileCommitLog::<1>::create_new_transaction_page_capable(
            &wal_path,
            persistent_log_id,
        )?);
        drop(FilePageStore::<1>::create_new(
            &page_store_path,
            persistent_log_id,
        )?);
        drop(FileRestartCheckpointCompletenessBaselineSource::create_new(
            &checkpoint_path,
            persistent_log_id,
        )?);

        Ok(Self {
            directory,
            layout: FileDatabaseLayout::new(
                owner_path,
                manifest_path,
                wal_path,
                page_store_path,
                checkpoint_path,
            ),
            database_id,
            persistent_log_id,
            manifest,
        })
    }

    fn new_successor(
        tag: &str,
        database_value: u128,
        persistent_value: u128,
    ) -> Result<Self, Box<dyn Error>> {
        let directory = TestDirectory::new(tag)?;
        let database_id = database_id(database_value)?;
        let persistent_log_id = persistent_log_id(persistent_value)?;
        let owner_path = directory.path().join("owner");
        let manifest_path = directory.path().join("manifest");
        let wal_path = directory.path().join("wal");
        let page_store_path = directory.path().join("pages");
        let checkpoint_path = directory.path().join("checkpoint");
        let manifest = manifest(
            database_id,
            persistent_log_id,
            [
                1_000 + database_value,
                2_000 + database_value,
                3_000 + database_value,
            ],
            [5, 2, 2],
        )?;
        let storage_identity = manifest.composition_identity().storage_identity();

        write_synced_new(&owner_path, &encode_database_owner_control(database_id))?;
        write_synced_new(&manifest_path, &encode_database_manifest(&manifest))?;
        drop(
            FileCommitLog::<1>::create_new_database_transaction_page_capable(
                &wal_path,
                storage_identity,
            )?,
        );
        drop(FilePageStore::<1>::create_new_database(
            &page_store_path,
            storage_identity,
        )?);
        drop(
            FileRestartCheckpointCompletenessBaselineSource::create_new_database(
                &checkpoint_path,
                storage_identity,
            )?,
        );

        Ok(Self {
            directory,
            layout: FileDatabaseLayout::new(
                owner_path,
                manifest_path,
                wal_path,
                page_store_path,
                checkpoint_path,
            ),
            database_id,
            persistent_log_id,
            manifest,
        })
    }

    fn manifest_with(
        &self,
        database_id: DatabaseId,
        persistent_log_id: PersistentLogId,
        formats: [u16; 3],
    ) -> Result<DatabaseManifest, Box<dyn Error>> {
        let identity = self.manifest.composition_identity();
        manifest(
            database_id,
            persistent_log_id,
            [
                identity.file_id(DatabaseFileRole::Wal).get(),
                identity.file_id(DatabaseFileRole::PageStore).get(),
                identity.file_id(DatabaseFileRole::RestartCheckpoint).get(),
            ],
            formats,
        )
    }
}

#[derive(Clone, Copy)]
enum ChildIdentityMutation {
    DatabaseId(u128),
    Role(DatabaseFileRole),
    FileId(u128),
}

fn mutate_child_identity(
    layout: &FileDatabaseLayout,
    role: DatabaseFileRole,
    mutation: ChildIdentityMutation,
) -> Result<(), io::Error> {
    let (path, identity_offset, checksum_offset) = match role {
        DatabaseFileRole::Wal => (layout.wal().to_path_buf(), 128, 184),
        DatabaseFileRole::PageStore => (layout.page_store().to_path_buf(), 64, 120),
        DatabaseFileRole::RestartCheckpoint => {
            (layout.restart_checkpoint().join("control"), 64, 120)
        }
    };
    let mut bytes = fs::read(&path)?;
    match mutation {
        ChildIdentityMutation::DatabaseId(value) => {
            bytes[identity_offset + 16..identity_offset + 32].copy_from_slice(&value.to_be_bytes());
        }
        ChildIdentityMutation::Role(role) => {
            bytes[identity_offset + 12] = role_code(role);
        }
        ChildIdentityMutation::FileId(value) => {
            bytes[identity_offset + 32..identity_offset + 48].copy_from_slice(&value.to_be_bytes());
        }
    }
    let checksum = checksum(&bytes[..checksum_offset]);
    bytes[checksum_offset..checksum_offset + 8].copy_from_slice(&checksum.to_be_bytes());
    overwrite_synced(&path, &bytes)
}

const fn next_role(role: DatabaseFileRole) -> DatabaseFileRole {
    match role {
        DatabaseFileRole::Wal => DatabaseFileRole::PageStore,
        DatabaseFileRole::PageStore => DatabaseFileRole::RestartCheckpoint,
        DatabaseFileRole::RestartCheckpoint => DatabaseFileRole::Wal,
    }
}

const fn role_code(role: DatabaseFileRole) -> u8 {
    match role {
        DatabaseFileRole::Wal => 1,
        DatabaseFileRole::PageStore => 2,
        DatabaseFileRole::RestartCheckpoint => 3,
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(tag: &str) -> Result<Self, io::Error> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "ntsql-database-ownership-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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

fn manifest(
    database_id: DatabaseId,
    persistent_log_id: PersistentLogId,
    file_ids: [u128; 3],
    formats: [u16; 3],
) -> Result<DatabaseManifest, Box<dyn Error>> {
    let files = [
        DatabaseFileIdentity::new(DatabaseFileRole::Wal, database_file_id(file_ids[0])?),
        DatabaseFileIdentity::new(DatabaseFileRole::PageStore, database_file_id(file_ids[1])?),
        DatabaseFileIdentity::new(
            DatabaseFileRole::RestartCheckpoint,
            database_file_id(file_ids[2])?,
        ),
    ];
    Ok(DatabaseManifest::recovery_required(
        DatabaseCompositionIdentity::new(
            database_id,
            DatabaseLifecycleGeneration::new(1)
                .ok_or_else(|| io::Error::other("test generation is zero"))?,
            persistent_log_id,
            &files,
        )?,
        DatabaseStorageFormatRequirements::new(
            format_version(formats[0])?,
            format_version(formats[1])?,
            format_version(formats[2])?,
        ),
        DatabaseRequiredFeatures::NONE,
    ))
}

fn database_id(value: u128) -> Result<DatabaseId, io::Error> {
    DatabaseId::new(value).ok_or_else(|| io::Error::other("test database ID is zero"))
}

fn database_file_id(value: u128) -> Result<ntsql_database::DatabaseFileId, io::Error> {
    ntsql_database::DatabaseFileId::new(value)
        .ok_or_else(|| io::Error::other("test database file ID is zero"))
}

fn persistent_log_id(value: u128) -> Result<PersistentLogId, io::Error> {
    PersistentLogId::new(value).ok_or_else(|| io::Error::other("test persistent log ID is zero"))
}

fn format_version(value: u16) -> Result<DatabaseStorageFormatVersion, io::Error> {
    DatabaseStorageFormatVersion::new(value)
        .ok_or_else(|| io::Error::other("test format version is zero"))
}

fn write_synced_new(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn overwrite_synced(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn append_synced(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn wal_reclamation_candidate(wal: &Path) -> Result<PathBuf, io::Error> {
    let file_name = wal
        .file_name()
        .ok_or_else(|| io::Error::other("test WAL has no file name"))?;
    let mut candidate_name = file_name.to_os_string();
    candidate_name.push(".reclaim-candidate");
    Ok(wal.with_file_name(candidate_name))
}

fn replace_control_checksum(bytes: &mut [u8; DATABASE_OWNER_CONTROL_V1_LENGTH]) {
    let checksum = checksum(&bytes[..56]);
    bytes[56..64].copy_from_slice(&checksum.to_be_bytes());
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut state = CHECKSUM_SEED;
    let mut length = 0_u64;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(CHECKSUM_MULTIPLIER);
        state = state.rotate_left(7) ^ CHECKSUM_XOR;
        length = length.wrapping_add(1);
    }
    state ^ length
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}
