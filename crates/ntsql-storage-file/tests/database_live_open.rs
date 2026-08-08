use std::{
    env,
    error::Error,
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_compatibility::{CompatibilityContext, CompatibilityProfile};
use ntsql_database::{
    DatabaseCleanManifestPublicationState, DatabaseClosePreparationFailureCause,
    DatabaseCompositionIdentity, DatabaseFileIdentity, DatabaseFileRole, DatabaseId,
    DatabaseLifecycleGeneration, DatabaseLifecycleStage, DatabaseManifest,
    DatabaseManifestLifecycleState, DatabaseRequiredFeatures, DatabaseStorageFormatRequirements,
    DatabaseStorageFormatVersion,
};
use ntsql_storage_file::{
    DATABASE_MANIFEST_V1_LENGTH, DATABASE_MANIFEST_V2_LENGTH, FileCleanCloseCheckpointFaultPoint,
    FileDatabaseCloseBoundary, FileDatabaseCloseFault, FileDatabaseCloseFaultTiming,
    FileDatabaseCreateEntry, FileDatabaseCreateOutcome, FileDatabaseLayout,
    FileDatabaseLiveOpenError, FileDatabaseOpenPhase, FileDatabaseOwnershipOpenError,
    FileRestartCheckpointCompletenessBaselinePublicationFaultPoint,
    clean_close_checkpoint_slot_directory, create_file_database,
    database_manifest_close_candidate_path, decode_database_manifest, decode_database_manifest_v2,
    open_file_database_ownership, open_live_file_database,
    open_live_file_database_with_close_checkpoint_fault, open_live_file_database_with_observer,
    open_recovery_required_file_database,
};
use ntsql_transaction::TransactionPageStorageRecoveryHandoffPhase;
use ntsql_wal::PersistentLogId;

#[test]
fn fresh_open_bootstraps_checkpoint_and_retains_context_and_all_locks() -> Result<(), Box<dyn Error>>
{
    let database = TestDatabase::create("fresh", 1)?;
    let mut phases = Vec::new();
    let live = open_live_file_database_with_observer::<1, _>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("file-live")?,
        |phase| phases.push(phase),
    )?;

    assert_eq!(live.stage(), DatabaseLifecycleStage::Live);
    assert_eq!(live.identity(), database.manifest.composition_identity());
    assert_eq!(
        live.compatibility_context().target_id().as_str(),
        "file-live"
    );
    assert_eq!(live.manifest(), database.manifest);
    assert_eq!(live.transaction_parts().1.generation(), 0);
    assert_eq!(phases, absent_checkpoint_phases());
    assert_all_database_files_locked(&database.layout)?;
    assert!(matches!(
        open_recovery_required_file_database::<1>(database.database_id, database.layout.clone()),
        Err(FileDatabaseOwnershipOpenError::Io(_))
    ));

    let abandoned = live.abandon();
    assert_eq!(abandoned.stage(), DatabaseLifecycleStage::Abandoned);
    assert_eq!(
        abandoned.identity(),
        database.manifest.composition_identity()
    );
    drop(open_recovery_required_file_database::<1>(
        database.database_id,
        database.layout.clone(),
    )?);
    Ok(())
}

#[test]
fn selected_reopen_ignores_unpublished_candidate_without_reclaiming_wal()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::create("selected", 2)?;
    let mut first = open_live_file_database::<1>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("first-open")?,
    )?;
    let (coordinator, log, _) = first.transaction_parts_mut();
    let transaction = coordinator.begin()?;
    drop(coordinator.commit(transaction, log)?);
    drop(first);
    fs::write(
        database.layout.restart_checkpoint().join("candidate"),
        b"unpublished and invalid",
    )?;

    let mut phases = Vec::new();
    let live = open_live_file_database_with_observer::<1, _>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("selected-open")?,
        |phase| phases.push(phase),
    )?;

    assert_eq!(live.transaction_parts().1.generation(), 0);
    assert_eq!(phases, selected_checkpoint_phases());
    assert!(
        database
            .layout
            .restart_checkpoint()
            .join("candidate")
            .is_file()
    );
    Ok(())
}

#[test]
fn rejected_selected_checkpoint_never_falls_back_and_failure_retains_all_locks()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::create("rejected", 3)?;
    drop(open_live_file_database::<1>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("bootstrap")?,
    )?);
    let current = database.layout.restart_checkpoint().join("current");
    let mut corrupt = fs::read(&current)?;
    corrupt.push(0);
    fs::write(&current, corrupt)?;

    let mut phases = Vec::new();
    let error = open_live_file_database_with_observer::<1, _>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("rejected")?,
        |phase| phases.push(phase),
    )
    .err()
    .ok_or_else(|| io::Error::other("corrupt selected checkpoint released Live"))?;

    assert!(matches!(error, FileDatabaseLiveOpenError::Recovery(_)));
    assert_eq!(
        error.recovery_phase(),
        Some(TransactionPageStorageRecoveryHandoffPhase::CheckpointSelected)
    );
    assert_eq!(phases, [FileDatabaseOpenPhase::CompositionValidated]);
    assert_all_database_files_locked(&database.layout)?;
    drop(error);
    Ok(())
}

#[test]
fn clean_close_publishes_exact_v2_and_retains_every_lock() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::create("clean-close", 4)?;
    let live = open_live_file_database::<1>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("file-clean-close")?,
    )?;
    let selected_checkpoint = database.layout.restart_checkpoint().join("current");
    let selected_checkpoint_before = fs::read(&selected_checkpoint)?;

    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("filesystem close preparation failed"))?;
    let target = pending.target_manifest();
    let clean_checkpoint =
        clean_close_checkpoint_slot_directory(database.layout.restart_checkpoint())
            .ok_or_else(|| io::Error::other("clean-close checkpoint path is unavailable"))?;
    assert!(clean_checkpoint.join("current").is_file());
    assert_eq!(fs::read(&selected_checkpoint)?, selected_checkpoint_before);

    let closed = pending
        .close()
        .map_err(|_| io::Error::other("filesystem clean-manifest publication failed"))?;

    assert_eq!(closed.stage(), DatabaseLifecycleStage::Closed);
    assert_eq!(closed.identity(), target.composition_identity());
    assert_eq!(closed.manifest(), target);
    assert_eq!(
        closed.compatibility_context().target_id().as_str(),
        "file-clean-close"
    );
    let selected_bytes = fs::read(database.layout.manifest())?;
    assert_eq!(selected_bytes.len(), DATABASE_MANIFEST_V2_LENGTH);
    assert_eq!(decode_database_manifest_v2(&selected_bytes)?, target);
    assert!(
        !database_manifest_close_candidate_path(database.layout.manifest())
            .ok_or_else(|| io::Error::other("manifest close-candidate path is unavailable"))?
            .exists()
    );
    assert_eq!(fs::read(&selected_checkpoint)?, selected_checkpoint_before);
    assert_all_database_files_locked(&database.layout)?;
    assert_file_locked(&clean_checkpoint.join("control"))?;
    assert!(matches!(
        open_recovery_required_file_database::<1>(database.database_id, database.layout.clone()),
        Err(FileDatabaseOwnershipOpenError::Io(_))
    ));

    drop(closed);
    assert!(matches!(
        open_recovery_required_file_database::<1>(database.database_id, database.layout.clone()),
        Err(FileDatabaseOwnershipOpenError::ManifestLifecycle {
            actual: DatabaseManifestLifecycleState::Clean(_),
        })
    ));
    drop(open_file_database_ownership::<1>(
        database.database_id,
        database.layout.clone(),
    )?);
    Ok(())
}

#[test]
fn every_manifest_publication_fault_retains_state_and_allows_fresh_open()
-> Result<(), Box<dyn Error>> {
    let boundaries = [
        FileDatabaseCloseBoundary::CandidateCleanup,
        FileDatabaseCloseBoundary::CandidateCreate,
        FileDatabaseCloseBoundary::CandidateWrite,
        FileDatabaseCloseBoundary::CandidateSynchronization,
        FileDatabaseCloseBoundary::ManifestReplacement,
        FileDatabaseCloseBoundary::SelectedManifestVerification,
        FileDatabaseCloseBoundary::ParentDirectorySynchronization,
    ];
    let timings = [
        FileDatabaseCloseFaultTiming::BeforeEffect,
        FileDatabaseCloseFaultTiming::AfterEffect,
        FileDatabaseCloseFaultTiming::OutcomeIndeterminateBeforeEffect,
        FileDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect,
    ];

    for (boundary_index, boundary) in boundaries.into_iter().enumerate() {
        for (timing_index, timing) in timings.into_iter().enumerate() {
            let database = TestDatabase::create(
                "close-fault",
                1000 + (boundary_index * timings.len() + timing_index) as u128,
            )?;
            let live = open_live_file_database::<1>(
                database.database_id,
                database.layout.clone(),
                compatibility_context("file-close-fault")?,
            )?;
            let selected_checkpoint = database.layout.restart_checkpoint().join("current");
            let selected_checkpoint_before = fs::read(&selected_checkpoint)?;
            let pending = live
                .prepare_close()
                .map_err(|_| io::Error::other("filesystem close preparation failed"))?;
            let target = pending.target_manifest();
            let fault = FileDatabaseCloseFault::new(boundary, timing);

            let failure = pending
                .close_with_fault(fault)
                .err()
                .ok_or_else(|| io::Error::other("armed filesystem close fault did not fire"))?;

            let expected_state = expected_close_fault_state(boundary, timing);
            assert_eq!(failure.state(), expected_state);
            assert_eq!(
                failure.source_identity(),
                database.manifest.composition_identity()
            );
            assert_eq!(failure.target_identity(), target.composition_identity());
            assert_all_database_files_locked(&database.layout)?;
            let clean_checkpoint =
                clean_close_checkpoint_slot_directory(database.layout.restart_checkpoint())
                    .ok_or_else(|| {
                        io::Error::other("clean-close checkpoint path is unavailable")
                    })?;
            assert_file_locked(&clean_checkpoint.join("control"))?;
            assert_eq!(fs::read(&selected_checkpoint)?, selected_checkpoint_before);

            let effect_applied = matches!(
                timing,
                FileDatabaseCloseFaultTiming::AfterEffect
                    | FileDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect
            );
            let target_selected = matches!(
                boundary,
                FileDatabaseCloseBoundary::SelectedManifestVerification
                    | FileDatabaseCloseBoundary::ParentDirectorySynchronization
            ) || (boundary == FileDatabaseCloseBoundary::ManifestReplacement
                && effect_applied);
            let selected_bytes = fs::read(database.layout.manifest())?;
            if target_selected {
                assert_eq!(selected_bytes.len(), DATABASE_MANIFEST_V2_LENGTH);
                assert_eq!(decode_database_manifest_v2(&selected_bytes)?, target);
            } else {
                assert_eq!(selected_bytes.len(), DATABASE_MANIFEST_V1_LENGTH);
                assert_eq!(
                    decode_database_manifest(&selected_bytes)?,
                    database.manifest
                );
            }

            let abandoned = failure.abandon();
            assert_eq!(abandoned.state(), expected_state);
            assert_eq!(abandoned.stage(), DatabaseLifecycleStage::Abandoned);
            drop(open_file_database_ownership::<1>(
                database.database_id,
                database.layout.clone(),
            )?);
            if target_selected {
                assert!(matches!(
                    open_recovery_required_file_database::<1>(
                        database.database_id,
                        database.layout.clone()
                    ),
                    Err(FileDatabaseOwnershipOpenError::ManifestLifecycle {
                        actual: DatabaseManifestLifecycleState::Clean(_),
                    })
                ));
            } else {
                drop(open_recovery_required_file_database::<1>(
                    database.database_id,
                    database.layout.clone(),
                )?);
            }
        }
    }
    Ok(())
}

#[test]
fn stale_manifest_candidate_is_reconciled_by_a_fresh_close() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::create("close-retry", 5)?;
    let live = open_live_file_database::<1>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("file-close-retry-first")?,
    )?;
    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("first filesystem close preparation failed"))?;
    let failure = pending
        .close_with_fault(FileDatabaseCloseFault::new(
            FileDatabaseCloseBoundary::CandidateWrite,
            FileDatabaseCloseFaultTiming::AfterEffect,
        ))
        .err()
        .ok_or_else(|| io::Error::other("candidate-write fault did not fire"))?;
    let candidate = database_manifest_close_candidate_path(database.layout.manifest())
        .ok_or_else(|| io::Error::other("manifest close-candidate path is unavailable"))?;
    assert!(candidate.is_file());
    drop(failure.abandon());

    let live = open_live_file_database::<1>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("file-close-retry-second")?,
    )?;
    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("second filesystem close preparation failed"))?;
    let target = pending.target_manifest();
    let closed = pending
        .close()
        .map_err(|_| io::Error::other("fresh filesystem close publication failed"))?;
    assert_eq!(closed.manifest(), target);
    assert!(!candidate.exists());
    Ok(())
}

#[test]
fn every_clean_checkpoint_fault_preserves_restart_selection_and_requires_fresh_open()
-> Result<(), Box<dyn Error>> {
    let publication_faults = [
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeCandidateCleanup,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateCleanup,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateCreate,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateWrite,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateSync,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCurrentReplace,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterDirectorySync,
    ];
    let faults = publication_faults
        .map(FileCleanCloseCheckpointFaultPoint::Publication)
        .into_iter()
        .chain([FileCleanCloseCheckpointFaultPoint::BeforeLoad]);

    for (index, fault) in faults.enumerate() {
        let database = TestDatabase::create("checkpoint-close-fault", 2000 + index as u128)?;
        let live = open_live_file_database_with_close_checkpoint_fault::<1>(
            database.database_id,
            database.layout.clone(),
            compatibility_context("file-checkpoint-close-fault")?,
            fault,
        )?;
        let selected_checkpoint = database.layout.restart_checkpoint().join("current");
        let selected_checkpoint_before = fs::read(&selected_checkpoint)?;

        let failure = live
            .prepare_close()
            .err()
            .ok_or_else(|| io::Error::other("armed clean-close checkpoint fault did not fire"))?;

        match failure.cause() {
            DatabaseClosePreparationFailureCause::Transaction(transaction) => {
                assert!(transaction.error().outcome_is_indeterminate());
            }
            _ => {
                return Err(
                    io::Error::other("clean-close checkpoint fault returned wrong cause").into(),
                );
            }
        }
        assert_eq!(fs::read(&selected_checkpoint)?, selected_checkpoint_before);
        assert_eq!(
            decode_database_manifest(&fs::read(database.layout.manifest())?)?,
            database.manifest
        );
        assert_all_database_files_locked(&database.layout)?;
        let clean_checkpoint =
            clean_close_checkpoint_slot_directory(database.layout.restart_checkpoint())
                .ok_or_else(|| io::Error::other("clean-close checkpoint path is unavailable"))?;
        assert_file_locked(&clean_checkpoint.join("control"))?;
        drop(failure.abandon());

        let live = open_live_file_database::<1>(
            database.database_id,
            database.layout.clone(),
            compatibility_context("file-checkpoint-close-retry")?,
        )?;
        let pending = live
            .prepare_close()
            .map_err(|_| io::Error::other("fresh checkpoint close preparation failed"))?;
        let target = pending.target_manifest();
        let closed = pending
            .close()
            .map_err(|_| io::Error::other("fresh checkpoint close publication failed"))?;
        assert_eq!(closed.manifest(), target);
        assert_eq!(fs::read(&selected_checkpoint)?, selected_checkpoint_before);
    }
    Ok(())
}

#[test]
fn partial_clean_checkpoint_directory_is_reconciled_before_publication()
-> Result<(), Box<dyn Error>> {
    let database = TestDatabase::create("partial-clean-checkpoint", 6)?;
    let clean_checkpoint =
        clean_close_checkpoint_slot_directory(database.layout.restart_checkpoint())
            .ok_or_else(|| io::Error::other("clean-close checkpoint path is unavailable"))?;
    fs::create_dir(&clean_checkpoint)?;
    let live = open_live_file_database::<1>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("file-partial-clean-checkpoint")?,
    )?;

    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("partial clean checkpoint was not reconciled"))?;

    assert!(clean_checkpoint.join("control").is_file());
    assert!(clean_checkpoint.join("current").is_file());
    drop(
        pending
            .close()
            .map_err(|_| io::Error::other("clean publication failed after reconciliation"))?,
    );
    Ok(())
}

#[test]
fn manifest_close_candidate_directory_fails_before_selection() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::create("manifest-close-directory", 7)?;
    let live = open_live_file_database::<1>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("file-manifest-close-directory")?,
    )?;
    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("filesystem close preparation failed"))?;
    let candidate = database_manifest_close_candidate_path(database.layout.manifest())
        .ok_or_else(|| io::Error::other("manifest close-candidate path is unavailable"))?;
    fs::create_dir(&candidate)?;

    let failure = pending
        .close()
        .err()
        .ok_or_else(|| io::Error::other("manifest candidate directory was accepted"))?;

    assert_eq!(
        failure.state(),
        DatabaseCleanManifestPublicationState::SourceSelected
    );
    assert_eq!(
        decode_database_manifest(&fs::read(database.layout.manifest())?)?,
        database.manifest
    );
    assert_all_database_files_locked(&database.layout)?;
    drop(failure.abandon());
    fs::remove_dir(&candidate)?;

    let live = open_live_file_database::<1>(
        database.database_id,
        database.layout.clone(),
        compatibility_context("file-manifest-close-directory-retry")?,
    )?;
    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("fresh filesystem close preparation failed"))?;
    drop(
        pending
            .close()
            .map_err(|_| io::Error::other("fresh filesystem close publication failed"))?,
    );
    Ok(())
}

#[test]
fn close_candidate_path_collision_is_rejected_before_open() -> Result<(), Box<dyn Error>> {
    let database = TestDatabase::create("close-path-collision", 8)?;
    let clean_checkpoint =
        clean_close_checkpoint_slot_directory(database.layout.restart_checkpoint())
            .ok_or_else(|| io::Error::other("clean-close checkpoint path is unavailable"))?;
    let colliding_layout = FileDatabaseLayout::new(
        database.layout.database_owner(),
        clean_checkpoint,
        database.layout.wal(),
        database.layout.page_store(),
        database.layout.restart_checkpoint(),
    );

    assert!(matches!(
        open_file_database_ownership::<1>(database.database_id, colliding_layout),
        Err(
            FileDatabaseOwnershipOpenError::CloseCandidatePathCollision {
                first: FileDatabaseCreateEntry::Manifest,
                second: FileDatabaseCreateEntry::RestartCheckpointCleanCloseCandidate,
            }
        )
    ));
    Ok(())
}

const fn expected_close_fault_state(
    boundary: FileDatabaseCloseBoundary,
    timing: FileDatabaseCloseFaultTiming,
) -> DatabaseCleanManifestPublicationState {
    match (boundary, timing) {
        (
            FileDatabaseCloseBoundary::CandidateCleanup
            | FileDatabaseCloseBoundary::CandidateCreate
            | FileDatabaseCloseBoundary::CandidateWrite
            | FileDatabaseCloseBoundary::CandidateSynchronization,
            _,
        ) => DatabaseCleanManifestPublicationState::SourceSelected,
        (
            FileDatabaseCloseBoundary::ManifestReplacement,
            FileDatabaseCloseFaultTiming::BeforeEffect,
        ) => DatabaseCleanManifestPublicationState::SourceSelected,
        (
            FileDatabaseCloseBoundary::ManifestReplacement,
            FileDatabaseCloseFaultTiming::OutcomeIndeterminateBeforeEffect
            | FileDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect,
        ) => DatabaseCleanManifestPublicationState::SelectionIndeterminate,
        (
            FileDatabaseCloseBoundary::ManifestReplacement,
            FileDatabaseCloseFaultTiming::AfterEffect,
        )
        | (FileDatabaseCloseBoundary::SelectedManifestVerification, _)
        | (
            FileDatabaseCloseBoundary::ParentDirectorySynchronization,
            FileDatabaseCloseFaultTiming::BeforeEffect
            | FileDatabaseCloseFaultTiming::OutcomeIndeterminateBeforeEffect
            | FileDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect,
        ) => DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
        (
            FileDatabaseCloseBoundary::ParentDirectorySynchronization,
            FileDatabaseCloseFaultTiming::AfterEffect,
        ) => DatabaseCleanManifestPublicationState::TargetDurable,
    }
}

fn absent_checkpoint_phases() -> [FileDatabaseOpenPhase; 13] {
    [
        FileDatabaseOpenPhase::CompositionValidated,
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::CheckpointAbsent),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::FullRecoveryCompleted),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::FullRecoveryRestartAnalyzed),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::CheckpointBootstrapped),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::CheckpointSelected),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::ReplayPlanned),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::PageRepairsPrepared),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::PageRepairsCompleted),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::TransactionStateRestored),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::RestartCompleted),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::WalRetentionAnalyzed),
        FileDatabaseOpenPhase::LiveReleased,
    ]
}

fn selected_checkpoint_phases() -> [FileDatabaseOpenPhase; 9] {
    [
        FileDatabaseOpenPhase::CompositionValidated,
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::CheckpointSelected),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::ReplayPlanned),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::PageRepairsPrepared),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::PageRepairsCompleted),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::TransactionStateRestored),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::RestartCompleted),
        recovery_phase(TransactionPageStorageRecoveryHandoffPhase::WalRetentionAnalyzed),
        FileDatabaseOpenPhase::LiveReleased,
    ]
}

const fn recovery_phase(
    phase: TransactionPageStorageRecoveryHandoffPhase,
) -> FileDatabaseOpenPhase {
    FileDatabaseOpenPhase::Recovery(phase)
}

fn assert_all_database_files_locked(layout: &FileDatabaseLayout) -> Result<(), Box<dyn Error>> {
    let paths = [
        layout.database_owner().to_path_buf(),
        layout.manifest().to_path_buf(),
        layout.wal().to_path_buf(),
        layout.page_store().to_path_buf(),
        layout.restart_checkpoint().join("control"),
    ];
    for path in paths {
        assert_file_locked(&path)?;
    }
    Ok(())
}

fn assert_file_locked(path: &Path) -> Result<(), Box<dyn Error>> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let Err(error) = file.try_lock() else {
        return Err(io::Error::other(format!("{} was not locked", path.display())).into());
    };
    assert!(
        matches!(error, std::fs::TryLockError::WouldBlock),
        "{} returned unexpected lock error: {error}",
        path.display()
    );
    Ok(())
}

struct TestDatabase {
    _directory: TestDirectory,
    layout: FileDatabaseLayout,
    database_id: DatabaseId,
    manifest: DatabaseManifest,
}

impl TestDatabase {
    fn create(tag: &str, value: u128) -> Result<Self, Box<dyn Error>> {
        let directory = TestDirectory::new(tag)?;
        let database_id = nonzero_database_id(value)?;
        let manifest = manifest(database_id, value + 100_000)?;
        let layout = layout(directory.path());
        let created = create_file_database::<1>(manifest, layout.clone(), None)?;
        let FileDatabaseCreateOutcome::Created(owner) = created else {
            return Err(io::Error::other("fresh test database was already published").into());
        };
        drop(owner);
        Ok(Self {
            _directory: directory,
            layout,
            database_id,
            manifest,
        })
    }
}

fn layout(root: &Path) -> FileDatabaseLayout {
    FileDatabaseLayout::new(
        root.join("owner"),
        root.join("manifest"),
        root.join("wal"),
        root.join("pages"),
        root.join("checkpoint"),
    )
}

fn manifest(database_id: DatabaseId, base: u128) -> Result<DatabaseManifest, Box<dyn Error>> {
    let files = [
        DatabaseFileIdentity::new(DatabaseFileRole::Wal, nonzero_file_id(base + 1)?),
        DatabaseFileIdentity::new(DatabaseFileRole::PageStore, nonzero_file_id(base + 2)?),
        DatabaseFileIdentity::new(
            DatabaseFileRole::RestartCheckpoint,
            nonzero_file_id(base + 3)?,
        ),
    ];
    Ok(DatabaseManifest::recovery_required(
        DatabaseCompositionIdentity::new(
            database_id,
            DatabaseLifecycleGeneration::new(1)
                .ok_or_else(|| io::Error::other("generation is zero"))?,
            PersistentLogId::new(base + 4)
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

fn compatibility_context(target_id: &str) -> Result<CompatibilityContext, Box<dyn Error>> {
    Ok(CompatibilityContext::try_new(CompatibilityProfile {
        target_id: target_id.to_owned(),
        product_release: "test-release".to_owned(),
        servicing_update: "test-update".to_owned(),
        product_version: "1.2.3.4".to_owned(),
        edition: "test-edition".to_owned(),
        operating_system: "test-operating-system".to_owned(),
        architecture: "test-architecture".to_owned(),
        compatibility_level: 42,
        collation: "test-collation".to_owned(),
        language: "test-language".to_owned(),
        lcid: 1,
        timezone: "test-timezone".to_owned(),
        session_defaults: vec!["SET TEST_OPTION ON".to_owned()],
    })?)
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

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(tag: &str) -> Result<Self, io::Error> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = env::temp_dir().join(format!(
            "ntsql-database-live-open-{tag}-{}-{}",
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
