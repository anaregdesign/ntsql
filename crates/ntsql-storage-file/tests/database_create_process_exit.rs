// This binary holds only `process_exit_after_every_publication_boundary_resumes_exactly`
// and its inert child entry point `create_process_crash_child`.
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

use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseFileIdentity, DatabaseFileRole, DatabaseId,
    DatabaseLifecycleGeneration, DatabaseManifest, DatabaseRequiredFeatures,
    DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_storage_file::{
    FileDatabaseCreateBoundary, FileDatabaseCreateError, FileDatabaseCreateFault,
    FileDatabaseCreateFaultTiming, FileDatabaseCreateOutcome, FileDatabaseCreatePhase,
    FileDatabaseLayout, create_file_database,
};
use ntsql_wal::PersistentLogId;

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
    _database_id: DatabaseId,
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
            _database_id: database_id,
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

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(tag: &str) -> Result<Self, io::Error> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "ntsql-database-create-process-exit-{tag}-{}-{}",
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
