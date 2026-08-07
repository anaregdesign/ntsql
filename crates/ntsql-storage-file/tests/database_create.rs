use std::{
    env,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseFileIdentity, DatabaseFileRole, DatabaseId,
    DatabaseLifecycleGeneration, DatabaseLifecycleStage, DatabaseManifest,
    DatabaseRequiredFeatures, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
    DatabaseStorageIdentity,
};
use ntsql_storage_file::{
    FileCommitLog, FileDatabaseCreateBoundary, FileDatabaseCreateEntry, FileDatabaseCreateError,
    FileDatabaseCreateFault, FileDatabaseCreateFaultTiming, FileDatabaseCreateManifestError,
    FileDatabaseCreateOutcome, FileDatabaseCreatePhase, FileDatabaseLayout, FileDatabaseLockRole,
    FileDatabaseOwnershipIoStage, FileDatabaseOwnershipOpenError, FilePageStore,
    FileRestartCheckpointCompletenessBaselineSource, create_file_database,
    open_recovery_required_file_database,
};
use ntsql_wal::PersistentLogId;

#[test]
fn create_publishes_manifest_last_and_repeated_create_is_explicit() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("success", 1)?;

    let created = create_file_database::<1>(database.manifest, database.layout.clone(), None)?;
    let FileDatabaseCreateOutcome::Created(created) = created else {
        return Err(io::Error::other("fresh create did not report Created").into());
    };
    assert_eq!(created.stage(), DatabaseLifecycleStage::RecoveryRequired);
    assert_eq!(created.identity(), database.manifest.composition_identity());
    assert_published(&database.layout)?;
    assert_create_candidates_absent(&database.layout);

    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::OwnershipContended { .. })
    ));
    drop(created);

    let repeated = create_file_database::<1>(database.manifest, database.layout.clone(), None)?;
    let FileDatabaseCreateOutcome::AlreadyPublished(repeated) = repeated else {
        return Err(io::Error::other("repeated create did not report AlreadyPublished").into());
    };
    assert_eq!(
        repeated.identity(),
        database.manifest.composition_identity()
    );
    drop(repeated);
    drop(open_recovery_required_file_database::<1>(
        database.database_id,
        database.layout.clone(),
    )?);
    Ok(())
}

#[test]
fn every_declared_fault_timing_resumes_from_fresh_evidence() -> Result<(), Box<dyn Error>> {
    let boundaries = [
        FileDatabaseCreateBoundary::OwnerPublication,
        FileDatabaseCreateBoundary::ManifestCandidatePublication,
        FileDatabaseCreateBoundary::WalCandidatePublication,
        FileDatabaseCreateBoundary::PageStoreCandidatePublication,
        FileDatabaseCreateBoundary::RestartCheckpointCandidatePublication,
        FileDatabaseCreateBoundary::WalPublication,
        FileDatabaseCreateBoundary::PageStorePublication,
        FileDatabaseCreateBoundary::RestartCheckpointPublication,
        FileDatabaseCreateBoundary::ManifestPublication,
    ];
    let timings = [
        FileDatabaseCreateFaultTiming::BeforeEffect,
        FileDatabaseCreateFaultTiming::AfterEffect,
        FileDatabaseCreateFaultTiming::OutcomeIndeterminateBeforeEffect,
        FileDatabaseCreateFaultTiming::OutcomeIndeterminateAfterEffect,
    ];

    for (boundary_index, boundary) in boundaries.into_iter().enumerate() {
        for (timing_index, timing) in timings.into_iter().enumerate() {
            let database = TestDatabase::new(
                "fault",
                100 + (boundary_index * timings.len() + timing_index) as u128,
            )?;
            let fault = FileDatabaseCreateFault::new(boundary, timing);
            let error =
                create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault))
                    .err()
                    .ok_or_else(|| io::Error::other("armed create fault did not fire"))?;
            assert!(matches!(
                &error,
                FileDatabaseCreateError::InjectedFault(actual) if *actual == fault
            ));
            assert_eq!(
                error.is_outcome_indeterminate(),
                timing.is_outcome_indeterminate()
            );
            assert_eq!(
                observed_phase(&database.layout)?,
                expected_fault_phase(boundary, timing)
            );

            let resumed =
                create_file_database::<1>(database.manifest, database.layout.clone(), None)?;
            match (boundary, timing, resumed) {
                (
                    FileDatabaseCreateBoundary::ManifestPublication,
                    FileDatabaseCreateFaultTiming::AfterEffect
                    | FileDatabaseCreateFaultTiming::OutcomeIndeterminateAfterEffect,
                    FileDatabaseCreateOutcome::AlreadyPublished(database),
                )
                | (_, _, FileDatabaseCreateOutcome::Created(database)) => drop(database),
                _ => return Err(io::Error::other("fault retry returned wrong outcome").into()),
            }
            assert_published(&database.layout)?;
            assert_create_candidates_absent(&database.layout);
        }
    }
    Ok(())
}

#[test]
fn ordinary_open_never_selects_complete_create_candidates() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("candidate-non-selection", 300)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::RestartCheckpointCandidatePublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    assert!(!database.layout.manifest().exists());
    assert!(
        open_recovery_required_file_database::<1>(database.database_id, database.layout.clone(),)
            .is_err()
    );

    let FileDatabaseCreateOutcome::Created(created) =
        create_file_database::<1>(database.manifest, database.layout.clone(), None)?
    else {
        return Err(io::Error::other("candidate retry did not report Created").into());
    };
    drop(created);
    Ok(())
}

#[cfg(unix)]
#[test]
fn ordinary_open_completes_manifest_durability_before_child_repair() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new_with_split_manifest("manifest-barrier", 350)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::RestartCheckpointPublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    fs::rename(
        candidate_path(database.layout.manifest()),
        database.layout.manifest(),
    )?;
    append_synced(database.layout.wal(), &[0xaa])?;
    let wal_before = fs::read(database.layout.wal())?;

    let manifest_parent = database
        .layout
        .manifest()
        .parent()
        .ok_or_else(|| io::Error::other("manifest has no parent"))?;
    let original_permissions = fs::metadata(manifest_parent)?.permissions();
    let mut blocked_permissions = original_permissions.clone();
    blocked_permissions.set_mode(0o100);
    fs::set_permissions(manifest_parent, blocked_permissions)?;
    let opened =
        open_recovery_required_file_database::<1>(database.database_id, database.layout.clone());
    fs::set_permissions(manifest_parent, original_permissions)?;

    let error = opened.err().ok_or_else(|| {
        io::Error::other("ordinary open bypassed the manifest durability barrier")
    })?;
    assert!(matches!(
        error,
        FileDatabaseOwnershipOpenError::Io(source)
            if source.stage()
                == (FileDatabaseOwnershipIoStage::OpenParentDirectory {
                    role: FileDatabaseLockRole::Manifest,
                })
    ));
    assert_eq!(fs::read(database.layout.wal())?, wal_before);
    Ok(())
}

#[test]
fn repairable_wal_candidate_tail_is_preserved_and_rejected() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("candidate-tail", 400)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::WalCandidatePublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    let wal_candidate = candidate_path(database.layout.wal());
    append_synced(&wal_candidate, &[0xaa])?;
    let before = fs::read(&wal_candidate)?;

    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::NonInitialChild {
            role: DatabaseFileRole::Wal,
            ..
        })
    ));
    assert_eq!(fs::read(wal_candidate)?, before);
    Ok(())
}

#[test]
fn published_retry_rejects_noninitial_children_without_repair() -> Result<(), Box<dyn Error>> {
    for (index, role) in [DatabaseFileRole::Wal, DatabaseFileRole::PageStore]
        .into_iter()
        .enumerate()
    {
        let database = TestDatabase::new("published-noninitial", 450 + index as u128)?;
        drop(create_file_database::<1>(
            database.manifest,
            database.layout.clone(),
            None,
        )?);
        let path = match role {
            DatabaseFileRole::Wal => database.layout.wal(),
            DatabaseFileRole::PageStore => database.layout.page_store(),
            DatabaseFileRole::RestartCheckpoint => {
                return Err(io::Error::other("unexpected test role").into());
            }
        };
        append_synced(path, &[0xaa])?;
        let before = fs::read(path)?;

        assert!(matches!(
            create_file_database::<1>(database.manifest, database.layout.clone(), None),
            Err(FileDatabaseCreateError::NonInitialChild {
                role: actual,
                location: ntsql_storage_file::FileDatabaseCreateLocation::Final,
            }) if actual == role
        ));
        assert_eq!(fs::read(path)?, before);
    }

    let database = TestDatabase::new("published-checkpoint-entry", 452)?;
    drop(create_file_database::<1>(
        database.manifest,
        database.layout.clone(),
        None,
    )?);
    let unknown = database.layout.restart_checkpoint().join("unknown");
    write_synced_new(&unknown, b"foreign")?;
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::UnexpectedCheckpointEntry {
            location: ntsql_storage_file::FileDatabaseCreateLocation::Final,
            actual,
        }) if actual == "unknown"
    ));
    assert_eq!(fs::read(unknown)?, b"foreign");
    Ok(())
}

#[test]
fn auxiliary_wal_entries_fail_before_owner_publication_or_cleanup() -> Result<(), Box<dyn Error>> {
    for (index, candidate_wal) in [false, true].into_iter().enumerate() {
        let database = TestDatabase::new("orphan-auxiliary", 460 + index as u128)?;
        let wal_path = if candidate_wal {
            candidate_path(database.layout.wal())
        } else {
            database.layout.wal().to_path_buf()
        };
        let auxiliary = reclamation_candidate_path(&wal_path);
        write_synced_new(&auxiliary, b"foreign")?;

        let expected = if candidate_wal {
            FileDatabaseCreateEntry::WalCandidateReclamationCandidate
        } else {
            FileDatabaseCreateEntry::WalReclamationCandidate
        };
        assert!(matches!(
            create_file_database::<1>(database.manifest, database.layout.clone(), None),
            Err(FileDatabaseCreateError::UnexpectedAuxiliaryEntry { entry })
                if entry == expected
        ));
        assert!(!database.layout.database_owner().exists());
        assert_eq!(fs::read(auxiliary)?, b"foreign");
    }

    let database = TestDatabase::new("published-auxiliary", 462)?;
    drop(create_file_database::<1>(
        database.manifest,
        database.layout.clone(),
        None,
    )?);
    let auxiliary = reclamation_candidate_path(database.layout.wal());
    write_synced_new(&auxiliary, b"foreign")?;
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::UnexpectedAuxiliaryEntry {
            entry: FileDatabaseCreateEntry::WalReclamationCandidate,
        })
    ));
    assert_eq!(fs::read(auxiliary)?, b"foreign");
    Ok(())
}

#[test]
fn deterministic_child_corruption_is_not_outcome_indeterminate() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("deterministic-corruption", 470)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::ManifestCandidatePublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    let wal_candidate = candidate_path(database.layout.wal());
    write_synced_new(&wal_candidate, b"not a WAL")?;

    let error = create_file_database::<1>(database.manifest, database.layout.clone(), None)
        .err()
        .ok_or_else(|| io::Error::other("malformed WAL candidate was accepted"))?;
    assert!(matches!(&error, FileDatabaseCreateError::WalOpen(_)));
    assert!(!error.is_outcome_indeterminate());
    assert_eq!(fs::read(wal_candidate)?, b"not a WAL");
    Ok(())
}

#[test]
fn out_of_order_final_child_is_terminal_namespace_evidence() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("out-of-order", 500)?;
    let owner_fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::OwnerPublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(
            database.manifest,
            database.layout.clone(),
            Some(owner_fault),
        ),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == owner_fault
    ));
    drop(
        FileCommitLog::<1>::create_new_database_transaction_page_capable(
            database.layout.wal(),
            database.manifest.composition_identity().storage_identity(),
        )?,
    );

    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::NamespaceConflict(_))
    ));
    assert!(!database.layout.manifest().exists());
    assert!(database.layout.wal().exists());
    Ok(())
}

#[test]
fn manifest_prerequisites_and_path_collisions_fail_before_owner_creation()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("preconditions", 600)?;
    let successor = database.manifest.next_recovery_required()?;
    assert!(matches!(
        create_file_database::<1>(successor, database.layout.clone(), None),
        Err(FileDatabaseCreateError::ManifestRequirement(
            FileDatabaseCreateManifestError::LifecycleGeneration { actual: 2 }
        ))
    ));
    assert!(!database.layout.database_owner().exists());

    let shared = database._directory.path().join("shared");
    let colliding = FileDatabaseLayout::new(
        database.layout.database_owner(),
        &shared,
        &shared,
        database.layout.page_store(),
        database.layout.restart_checkpoint(),
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, colliding, None),
        Err(FileDatabaseCreateError::PathCollision { .. })
    ));
    assert!(!database.layout.database_owner().exists());
    Ok(())
}

#[test]
fn foreign_manifest_candidate_is_preserved_and_rejected() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("foreign-manifest", 700)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::ManifestCandidatePublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    let manifest_candidate = candidate_path(database.layout.manifest());
    let foreign = manifest(database.database_id, 90_000)?;
    overwrite_synced(
        &manifest_candidate,
        &ntsql_storage_file::encode_database_manifest(&foreign),
    )?;
    let before = fs::read(&manifest_candidate)?;

    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::ManifestMismatch(_))
    ));
    assert_eq!(fs::read(manifest_candidate)?, before);
    Ok(())
}

#[test]
fn checkpoint_candidate_unknown_entry_is_preserved_and_rejected() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("checkpoint-entry", 800)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::RestartCheckpointCandidatePublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    let checkpoint_candidate = candidate_path(database.layout.restart_checkpoint());
    let unknown = checkpoint_candidate.join("unknown");
    write_synced_new(&unknown, b"foreign")?;

    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::UnexpectedCheckpointEntry { actual, .. })
            if actual == "unknown"
    ));
    assert_eq!(fs::read(unknown)?, b"foreign");
    Ok(())
}

#[cfg(unix)]
#[test]
fn hard_link_alias_is_rejected_before_later_lock_acquisition() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("alias", 900)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::ManifestCandidatePublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    fs::hard_link(
        candidate_path(database.layout.manifest()),
        candidate_path(database.layout.wal()),
    )?;

    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::OpenedObjectAlias { .. })
    ));
    Ok(())
}

#[cfg(unix)]
#[test]
fn owner_manifest_alias_is_rejected_before_manifest_lock_acquisition() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::new("owner-manifest-alias", 901)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::OwnerPublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    fs::hard_link(
        database.layout.database_owner(),
        candidate_path(database.layout.manifest()),
    )?;

    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::OpenedObjectAlias {
            first: FileDatabaseCreateEntry::DatabaseOwner,
            second: FileDatabaseCreateEntry::ManifestCandidate,
        })
    ));
    Ok(())
}

#[test]
fn candidate_and_final_coexistence_is_not_normalized() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::new("candidate-final", 1_000)?;
    let fault = FileDatabaseCreateFault::new(
        FileDatabaseCreateBoundary::WalCandidatePublication,
        FileDatabaseCreateFaultTiming::AfterEffect,
    );
    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ));
    let candidate = candidate_path(database.layout.wal());
    fs::copy(&candidate, database.layout.wal())?;
    let candidate_before = fs::read(&candidate)?;
    let final_before = fs::read(database.layout.wal())?;

    assert!(matches!(
        create_file_database::<1>(database.manifest, database.layout.clone(), None),
        Err(FileDatabaseCreateError::NamespaceConflict(_))
    ));
    assert_eq!(fs::read(candidate)?, candidate_before);
    assert_eq!(fs::read(database.layout.wal())?, final_before);
    Ok(())
}

#[test]
fn every_foreign_child_candidate_is_preserved_and_rejected() -> Result<(), Box<dyn Error>> {
    for (index, role) in [
        DatabaseFileRole::Wal,
        DatabaseFileRole::PageStore,
        DatabaseFileRole::RestartCheckpoint,
    ]
    .into_iter()
    .enumerate()
    {
        let database = TestDatabase::new("foreign-child", 1_100 + index as u128)?;
        let preceding = match role {
            DatabaseFileRole::Wal => FileDatabaseCreateBoundary::ManifestCandidatePublication,
            DatabaseFileRole::PageStore => FileDatabaseCreateBoundary::WalCandidatePublication,
            DatabaseFileRole::RestartCheckpoint => {
                FileDatabaseCreateBoundary::PageStoreCandidatePublication
            }
        };
        let fault =
            FileDatabaseCreateFault::new(preceding, FileDatabaseCreateFaultTiming::AfterEffect);
        assert!(matches!(
            create_file_database::<1>(database.manifest, database.layout.clone(), Some(fault)),
            Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
        ));

        let identity = database.manifest.composition_identity();
        let foreign_storage = DatabaseStorageIdentity::new(
            nonzero_database_id(90_000 + index as u128)?,
            identity.persistent_log_id(),
            &identity.ordered_files(),
        )?;
        let candidate = match role {
            DatabaseFileRole::Wal => {
                let path = candidate_path(database.layout.wal());
                drop(
                    FileCommitLog::<1>::create_new_database_transaction_page_capable(
                        &path,
                        foreign_storage,
                    )?,
                );
                path
            }
            DatabaseFileRole::PageStore => {
                let path = candidate_path(database.layout.page_store());
                drop(FilePageStore::<1>::create_new_database(
                    &path,
                    foreign_storage,
                )?);
                path
            }
            DatabaseFileRole::RestartCheckpoint => {
                let path = candidate_path(database.layout.restart_checkpoint());
                drop(
                    FileRestartCheckpointCompletenessBaselineSource::create_new_database(
                        &path,
                        foreign_storage,
                    )?,
                );
                path.join("control")
            }
        };
        let before = fs::read(&candidate)?;
        assert!(matches!(
            create_file_database::<1>(database.manifest, database.layout.clone(), None),
            Err(FileDatabaseCreateError::ChildValidation(
                FileDatabaseOwnershipOpenError::ChildDatabaseIdMismatch {
                    role: actual,
                    ..
                }
            )) if actual == role
        ));
        assert_eq!(fs::read(candidate)?, before);
    }
    Ok(())
}

#[test]
fn process_exit_after_every_publication_boundary_resumes_exactly() -> Result<(), Box<dyn Error>> {
    for (index, boundary) in create_boundaries().into_iter().enumerate() {
        let value = 2_000 + index as u128;
        let database = TestDatabase::new("process-crash", value)?;
        let status = Command::new(env::current_exe()?)
            .arg("--exact")
            .arg("create_process_crash_child")
            .arg("--nocapture")
            .env("NTSQL_CREATE_CRASH_ROOT", database._directory.path())
            .env("NTSQL_CREATE_CRASH_VALUE", value.to_string())
            .env("NTSQL_CREATE_CRASH_BOUNDARY", index.to_string())
            .status()?;
        assert_eq!(status.code(), Some(83), "child did not exit at {boundary}");

        let resumed = create_file_database::<1>(database.manifest, database.layout.clone(), None)?;
        match (boundary, resumed) {
            (
                FileDatabaseCreateBoundary::ManifestPublication,
                FileDatabaseCreateOutcome::AlreadyPublished(database),
            )
            | (_, FileDatabaseCreateOutcome::Created(database)) => drop(database),
            _ => return Err(io::Error::other("process-crash retry returned wrong outcome").into()),
        }
        assert_eq!(
            observed_phase(&database.layout)?,
            FileDatabaseCreatePhase::Published
        );
    }
    Ok(())
}

#[test]
fn create_process_crash_child() -> Result<(), Box<dyn Error>> {
    let Ok(root) = env::var("NTSQL_CREATE_CRASH_ROOT") else {
        return Ok(());
    };
    let value = env::var("NTSQL_CREATE_CRASH_VALUE")?.parse::<u128>()?;
    let boundary_index = env::var("NTSQL_CREATE_CRASH_BOUNDARY")?.parse::<usize>()?;
    let boundary = create_boundaries()
        .get(boundary_index)
        .copied()
        .ok_or_else(|| io::Error::other("create crash boundary index is invalid"))?;
    let root = PathBuf::from(root);
    let database_id = nonzero_database_id(value)?;
    let manifest = manifest(database_id, value + 10_000)?;
    let layout = FileDatabaseLayout::new(
        root.join("owner"),
        root.join("manifest"),
        root.join("wal"),
        root.join("pages"),
        root.join("checkpoint"),
    );
    let fault = FileDatabaseCreateFault::new(boundary, FileDatabaseCreateFaultTiming::AfterEffect);
    if !matches!(
        create_file_database::<1>(manifest, layout, Some(fault)),
        Err(FileDatabaseCreateError::InjectedFault(actual)) if actual == fault
    ) {
        return Err(io::Error::other("create crash child did not reach its boundary").into());
    }
    std::process::exit(83);
}

struct TestDatabase {
    _directory: TestDirectory,
    layout: FileDatabaseLayout,
    database_id: DatabaseId,
    manifest: DatabaseManifest,
}

impl TestDatabase {
    fn new(tag: &str, value: u128) -> Result<Self, Box<dyn Error>> {
        let directory = TestDirectory::new(tag)?;
        let database_id = nonzero_database_id(value)?;
        let manifest = manifest(database_id, value + 10_000)?;
        let layout = FileDatabaseLayout::new(
            directory.path().join("owner"),
            directory.path().join("manifest"),
            directory.path().join("wal"),
            directory.path().join("pages"),
            directory.path().join("checkpoint"),
        );
        Ok(Self {
            _directory: directory,
            layout,
            database_id,
            manifest,
        })
    }

    fn new_with_split_manifest(tag: &str, value: u128) -> Result<Self, Box<dyn Error>> {
        let directory = TestDirectory::new(tag)?;
        let manifest_parent = directory.path().join("manifest-parent");
        fs::create_dir(&manifest_parent)?;
        let database_id = nonzero_database_id(value)?;
        let manifest = manifest(database_id, value + 10_000)?;
        let layout = FileDatabaseLayout::new(
            directory.path().join("owner"),
            manifest_parent.join("manifest"),
            directory.path().join("wal"),
            directory.path().join("pages"),
            directory.path().join("checkpoint"),
        );
        Ok(Self {
            _directory: directory,
            layout,
            database_id,
            manifest,
        })
    }
}

fn manifest(database_id: DatabaseId, base: u128) -> Result<DatabaseManifest, Box<dyn Error>> {
    let files = [
        DatabaseFileIdentity::new(
            DatabaseFileRole::Wal,
            nonzero_file_id(
                base.checked_add(1)
                    .ok_or_else(|| io::Error::other("ID overflow"))?,
            )?,
        ),
        DatabaseFileIdentity::new(
            DatabaseFileRole::PageStore,
            nonzero_file_id(
                base.checked_add(2)
                    .ok_or_else(|| io::Error::other("ID overflow"))?,
            )?,
        ),
        DatabaseFileIdentity::new(
            DatabaseFileRole::RestartCheckpoint,
            nonzero_file_id(
                base.checked_add(3)
                    .ok_or_else(|| io::Error::other("ID overflow"))?,
            )?,
        ),
    ];
    Ok(DatabaseManifest::recovery_required(
        DatabaseCompositionIdentity::new(
            database_id,
            DatabaseLifecycleGeneration::new(1)
                .ok_or_else(|| io::Error::other("generation is zero"))?,
            PersistentLogId::new(
                base.checked_add(4)
                    .ok_or_else(|| io::Error::other("ID overflow"))?,
            )
            .ok_or_else(|| io::Error::other("persistent log ID is zero"))?,
            &files,
        )?,
        DatabaseStorageFormatRequirements::new(
            format_version(5)?,
            format_version(2)?,
            format_version(2)?,
        ),
        DatabaseRequiredFeatures::NONE,
    ))
}

fn nonzero_database_id(value: u128) -> Result<DatabaseId, io::Error> {
    DatabaseId::new(value).ok_or_else(|| io::Error::other("database ID is zero"))
}

fn nonzero_file_id(value: u128) -> Result<ntsql_database::DatabaseFileId, io::Error> {
    ntsql_database::DatabaseFileId::new(value)
        .ok_or_else(|| io::Error::other("database file ID is zero"))
}

fn format_version(value: u16) -> Result<DatabaseStorageFormatVersion, io::Error> {
    DatabaseStorageFormatVersion::new(value)
        .ok_or_else(|| io::Error::other("format version is zero"))
}

fn candidate_path(path: &Path) -> PathBuf {
    let mut candidate = path.as_os_str().to_os_string();
    candidate.push(".create-candidate");
    PathBuf::from(candidate)
}

fn reclamation_candidate_path(path: &Path) -> PathBuf {
    let mut candidate = path.as_os_str().to_os_string();
    candidate.push(".reclaim-candidate");
    PathBuf::from(candidate)
}

fn observed_phase(layout: &FileDatabaseLayout) -> Result<FileDatabaseCreatePhase, io::Error> {
    let present = [
        layout.database_owner().to_path_buf(),
        layout.manifest().to_path_buf(),
        candidate_path(layout.manifest()),
        layout.wal().to_path_buf(),
        candidate_path(layout.wal()),
        layout.page_store().to_path_buf(),
        candidate_path(layout.page_store()),
        layout.restart_checkpoint().to_path_buf(),
        candidate_path(layout.restart_checkpoint()),
    ]
    .map(|path| fs::symlink_metadata(path).is_ok());
    match present {
        [
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        ] => Ok(FileDatabaseCreatePhase::Absent),
        [true, false, false, false, false, false, false, false, false] => {
            Ok(FileDatabaseCreatePhase::Owner)
        }

        [true, false, true, false, false, false, false, false, false] => {
            Ok(FileDatabaseCreatePhase::ManifestCandidate)
        }
        [true, false, true, false, true, false, false, false, false] => {
            Ok(FileDatabaseCreatePhase::WalCandidate)
        }
        [true, false, true, false, true, false, true, false, false] => {
            Ok(FileDatabaseCreatePhase::PageStoreCandidate)
        }
        [true, false, true, false, true, false, true, false, true] => {
            Ok(FileDatabaseCreatePhase::RestartCheckpointCandidate)
        }
        [true, false, true, true, false, false, true, false, true] => {
            Ok(FileDatabaseCreatePhase::WalPublished)
        }
        [true, false, true, true, false, true, false, false, true] => {
            Ok(FileDatabaseCreatePhase::PageStorePublished)
        }
        [true, false, true, true, false, true, false, true, false] => {
            Ok(FileDatabaseCreatePhase::ChildrenPublished)
        }
        [true, true, false, true, false, true, false, true, false] => {
            Ok(FileDatabaseCreatePhase::Published)
        }
        _ => Err(io::Error::other(
            "test observed a noncanonical database create phase",
        )),
    }
}

const fn create_boundaries() -> [FileDatabaseCreateBoundary; 9] {
    [
        FileDatabaseCreateBoundary::OwnerPublication,
        FileDatabaseCreateBoundary::ManifestCandidatePublication,
        FileDatabaseCreateBoundary::WalCandidatePublication,
        FileDatabaseCreateBoundary::PageStoreCandidatePublication,
        FileDatabaseCreateBoundary::RestartCheckpointCandidatePublication,
        FileDatabaseCreateBoundary::WalPublication,
        FileDatabaseCreateBoundary::PageStorePublication,
        FileDatabaseCreateBoundary::RestartCheckpointPublication,
        FileDatabaseCreateBoundary::ManifestPublication,
    ]
}

const fn expected_fault_phase(
    boundary: FileDatabaseCreateBoundary,
    timing: FileDatabaseCreateFaultTiming,
) -> FileDatabaseCreatePhase {
    let after = matches!(
        timing,
        FileDatabaseCreateFaultTiming::AfterEffect
            | FileDatabaseCreateFaultTiming::OutcomeIndeterminateAfterEffect
    );
    match (boundary, after) {
        (FileDatabaseCreateBoundary::OwnerPublication, false) => FileDatabaseCreatePhase::Absent,
        (FileDatabaseCreateBoundary::OwnerPublication, true)
        | (FileDatabaseCreateBoundary::ManifestCandidatePublication, false) => {
            FileDatabaseCreatePhase::Owner
        }
        (FileDatabaseCreateBoundary::ManifestCandidatePublication, true)
        | (FileDatabaseCreateBoundary::WalCandidatePublication, false) => {
            FileDatabaseCreatePhase::ManifestCandidate
        }
        (FileDatabaseCreateBoundary::WalCandidatePublication, true)
        | (FileDatabaseCreateBoundary::PageStoreCandidatePublication, false) => {
            FileDatabaseCreatePhase::WalCandidate
        }
        (FileDatabaseCreateBoundary::PageStoreCandidatePublication, true)
        | (FileDatabaseCreateBoundary::RestartCheckpointCandidatePublication, false) => {
            FileDatabaseCreatePhase::PageStoreCandidate
        }
        (FileDatabaseCreateBoundary::RestartCheckpointCandidatePublication, true)
        | (FileDatabaseCreateBoundary::WalPublication, false) => {
            FileDatabaseCreatePhase::RestartCheckpointCandidate
        }
        (FileDatabaseCreateBoundary::WalPublication, true)
        | (FileDatabaseCreateBoundary::PageStorePublication, false) => {
            FileDatabaseCreatePhase::WalPublished
        }
        (FileDatabaseCreateBoundary::PageStorePublication, true)
        | (FileDatabaseCreateBoundary::RestartCheckpointPublication, false) => {
            FileDatabaseCreatePhase::PageStorePublished
        }
        (FileDatabaseCreateBoundary::RestartCheckpointPublication, true)
        | (FileDatabaseCreateBoundary::ManifestPublication, false) => {
            FileDatabaseCreatePhase::ChildrenPublished
        }
        (FileDatabaseCreateBoundary::ManifestPublication, true) => {
            FileDatabaseCreatePhase::Published
        }
    }
}

fn assert_published(layout: &FileDatabaseLayout) -> Result<(), io::Error> {
    for path in [
        layout.database_owner(),
        layout.manifest(),
        layout.wal(),
        layout.page_store(),
        layout.restart_checkpoint(),
    ] {
        if fs::symlink_metadata(path).is_err() {
            return Err(io::Error::other(format!(
                "published path is absent: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn assert_create_candidates_absent(layout: &FileDatabaseLayout) {
    for path in [
        layout.manifest(),
        layout.wal(),
        layout.page_store(),
        layout.restart_checkpoint(),
    ] {
        assert!(fs::symlink_metadata(candidate_path(path)).is_err());
    }
}

fn append_synced(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
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

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(tag: &str) -> Result<Self, io::Error> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "ntsql-database-create-{tag}-{}-{}",
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
