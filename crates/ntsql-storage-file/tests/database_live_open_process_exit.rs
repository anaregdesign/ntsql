// This binary holds only `process_exit_after_every_open_phase_converges_on_fresh_reopen`
// and its inert child entry point `file_live_open_process_crash_child`.
//
// The parent test spawns `Command::new(env::current_exe()?)` to re-exec this
// test binary as a child process. On fork-based hosts (Linux), the child
// temporarily inherits the parent's open file descriptions across
// fork -> exec, including any `std::fs::File` flock locks held by libtest
// sibling threads running concurrently in the same binary. A sibling that
// drops and reopens a lock during that pre-exec window can observe
// `WouldBlock` even though `O_CLOEXEC` is set, because the descriptor stays
// open until the exec syscall actually completes. Keeping this test (and its
// crash child) as the only tests in this binary guarantees no sibling thread
// is ever holding a database lock when the child is spawned.

use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_compatibility::{CompatibilityContext, CompatibilityProfile};
use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseFileIdentity, DatabaseFileRole, DatabaseId,
    DatabaseLifecycleGeneration, DatabaseLifecycleStage, DatabaseManifest,
    DatabaseRequiredFeatures, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_storage_file::{
    FileDatabaseCreateOutcome, FileDatabaseLayout, FileDatabaseOpenPhase, create_file_database,
    open_live_file_database, open_live_file_database_with_observer,
};
use ntsql_transaction::TransactionPageStorageRecoveryHandoffPhase;
use ntsql_wal::PersistentLogId;

#[test]
fn process_exit_after_every_open_phase_converges_on_fresh_reopen() -> Result<(), Box<dyn Error>> {
    for (index, phase) in absent_checkpoint_phases().into_iter().enumerate() {
        let value = 10_000 + index as u128;
        let database = TestDatabase::create("process-exit", value)?;
        let status = Command::new(env::current_exe()?)
            .arg("--exact")
            .arg("file_live_open_process_crash_child")
            .arg("--nocapture")
            .env("NTSQL_LIVE_OPEN_CRASH_ROOT", database.directory.path())
            .env("NTSQL_LIVE_OPEN_CRASH_VALUE", value.to_string())
            .env("NTSQL_LIVE_OPEN_CRASH_PHASE", index.to_string())
            .status()?;
        assert_eq!(
            status.code(),
            Some(89),
            "child did not exit after {phase:?}"
        );

        let live = open_live_file_database::<1>(
            database.database_id,
            database.layout.clone(),
            compatibility_context("fresh-reopen")?,
        )?;
        assert_eq!(live.stage(), DatabaseLifecycleStage::Live);
        assert_eq!(live.identity(), database.manifest.composition_identity());
        assert_eq!(live.transaction_parts().1.generation(), 0);
    }
    Ok(())
}

#[test]
fn file_live_open_process_crash_child() -> Result<(), Box<dyn Error>> {
    let Ok(root) = env::var("NTSQL_LIVE_OPEN_CRASH_ROOT") else {
        return Ok(());
    };
    let value = env::var("NTSQL_LIVE_OPEN_CRASH_VALUE")?.parse::<u128>()?;
    let phase_index = env::var("NTSQL_LIVE_OPEN_CRASH_PHASE")?.parse::<usize>()?;
    let exit_phase = absent_checkpoint_phases()
        .get(phase_index)
        .copied()
        .ok_or_else(|| io::Error::other("live-open crash phase index is invalid"))?;
    let database_id = nonzero_database_id(value)?;
    let layout = layout(Path::new(&root));

    let _live = open_live_file_database_with_observer::<1, _>(
        database_id,
        layout,
        compatibility_context("crash-child")?,
        |phase| {
            if phase == exit_phase {
                std::process::exit(89);
            }
        },
    )?;
    Err(io::Error::other("live-open crash child did not reach requested phase").into())
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

const fn recovery_phase(
    phase: TransactionPageStorageRecoveryHandoffPhase,
) -> FileDatabaseOpenPhase {
    FileDatabaseOpenPhase::Recovery(phase)
}

struct TestDatabase {
    directory: TestDirectory,
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
            directory,
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
            "ntsql-database-live-open-process-exit-{tag}-{}-{}",
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
