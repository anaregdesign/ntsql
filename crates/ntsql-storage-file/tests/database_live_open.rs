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
    DatabaseCompositionIdentity, DatabaseFileIdentity, DatabaseFileRole, DatabaseId,
    DatabaseLifecycleGeneration, DatabaseLifecycleStage, DatabaseManifest,
    DatabaseRequiredFeatures, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_storage_file::{
    FileDatabaseCreateOutcome, FileDatabaseLayout, FileDatabaseLiveOpenError,
    FileDatabaseOpenPhase, FileDatabaseOwnershipOpenError, create_file_database,
    open_live_file_database, open_live_file_database_with_observer,
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
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let Err(error) = file.try_lock() else {
            return Err(io::Error::other(format!("{} was not locked", path.display())).into());
        };
        assert!(
            matches!(error, std::fs::TryLockError::WouldBlock),
            "{} returned unexpected lock error: {error}",
            path.display()
        );
    }
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
