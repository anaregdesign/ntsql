use std::{
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use ntsql_compatibility::CompatibilityContext;
use ntsql_database::{
    AbandonedDatabase, AbandonedDatabaseClosePublication, ClosePendingDatabase, ClosedDatabase,
    DatabaseCleanManifestPublicationPermit, DatabaseCleanManifestPublicationReceipt,
    DatabaseCleanManifestPublicationState, DatabaseCleanManifestPublisher,
    DatabaseCleanManifestPublisherFailure, DatabaseCloseSourceManifestOwner,
    DatabaseCompositionIdentity, DatabaseCompositionIdentityError,
    DatabaseCompositionIdentityMismatch, DatabaseFileHeaderIdentity, DatabaseFileRole, DatabaseId,
    DatabaseLifecycleStage, DatabaseManifest, DatabaseManifestSelectionRejection,
    DatabaseManifestSuccessorError, DatabaseRecoveryFailureCause, DatabaseRecoveryOwner,
    DatabaseStorageFormatVersion, DatabaseStorageIdentity, FailedDatabaseClosePreparation,
    FailedDatabaseClosePublication, FailedDatabaseRecovery, LiveDatabase, ManifestSelectedDatabase,
    PreparedDatabaseCloseOwnership, PublishedDatabaseCloseOwnership, RecoveredDatabaseOwnership,
    RecoveryRequiredDatabase, UnboundDatabase,
};
use ntsql_transaction::{
    FailedTransactionPageStorageRecoveryHandoff, TransactionCoordinator,
    TransactionPageStorageRecoveryHandoffPhase,
    WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay,
    complete_transaction_page_storage_recovery_handoff_with_observer,
};
use ntsql_wal::PersistentLogId;

use super::{
    FileCleanCloseCheckpointFaultAlreadyArmed, FileCleanCloseCheckpointFaultPoint, FileCommitLog,
    FileCreateError, FileOpenError, FilePageStore, FilePageWidthError,
    FileRestartCheckpointCompletenessBaselineSource, PageLayout, PageStoreCreateError,
    PageStoreOpenError, UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint,
    checksum_v1, clean_close_checkpoint_slot_directory, decode_database_manifest,
    decode_database_manifest_v2, encode_database_manifest_v2, metadata_identifies_same_file,
    read_u16, read_u32, read_u64, read_u128,
    restart_checkpoint_file::{
        CONTROL_FILE_NAME, FileRestartCheckpointSlotCreateError, FileRestartCheckpointSlotOpenError,
    },
    write_u16, write_u32, write_u64, write_u128,
};

const DATABASE_OWNER_CONTROL_MAGIC: [u8; 8] = *b"NTSQDBO1";
const DATABASE_OWNER_CONTROL_FORMAT_VERSION: u16 = 1;
const DATABASE_OWNER_CONTROL_LENGTH_U16: u16 = 64;
const DATABASE_OWNER_CONTROL_FLAGS_OFFSET: usize = 12;
const DATABASE_OWNER_CONTROL_DATABASE_ID_OFFSET: usize = 16;
const DATABASE_OWNER_CONTROL_RESERVED_START: usize = 32;
const DATABASE_OWNER_CONTROL_CHECKSUM_OFFSET: usize = 56;

/// Exact byte length of the stable database-owner control format version 1.
pub const DATABASE_OWNER_CONTROL_V1_LENGTH: usize = 64;

/// Failure to decode one complete stable database-owner control frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DatabaseOwnerControlDecodeError {
    /// The supplied byte slice ends before the fixed frame boundary.
    Truncated {
        /// Exact supplied byte count.
        actual: usize,
    },
    /// The supplied byte slice extends beyond the fixed frame boundary.
    TrailingBytes {
        /// Exact supplied byte count.
        actual: usize,
    },
    /// The independent owner-control magic does not match.
    MagicMismatch {
        /// Exact decoded magic bytes.
        actual: [u8; 8],
    },
    /// The format version is not supported.
    UnsupportedVersion {
        /// Exact decoded version.
        actual: u16,
    },
    /// The encoded fixed frame length is not canonical.
    FrameLengthMismatch {
        /// Exact decoded frame length.
        actual: u16,
    },
    /// Version 1 defines no header flags.
    HeaderFlagsUnsupported {
        /// Exact decoded flags.
        actual: u32,
    },
    /// The complete protected prefix does not match the stored checksum.
    ChecksumMismatch {
        /// Checksum computed from the supplied protected prefix.
        expected: u64,
        /// Exact decoded checksum.
        actual: u64,
    },
    /// One version-1 reserved byte is nonzero.
    ReservedByteNonZero {
        /// Exact frame offset.
        offset: usize,
        /// Exact rejected byte.
        actual: u8,
    },
    /// The database identity is zero.
    DatabaseIdZero,
}

impl fmt::Display for DatabaseOwnerControlDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { actual } => write!(
                formatter,
                "database owner control is truncated: expected {DATABASE_OWNER_CONTROL_V1_LENGTH} bytes, got {actual}"
            ),
            Self::TrailingBytes { actual } => write!(
                formatter,
                "database owner control has trailing bytes: expected {DATABASE_OWNER_CONTROL_V1_LENGTH} bytes, got {actual}"
            ),
            Self::MagicMismatch { actual } => {
                write!(
                    formatter,
                    "database owner control magic mismatch: {actual:02x?}"
                )
            }
            Self::UnsupportedVersion { actual } => {
                write!(
                    formatter,
                    "unsupported database owner control version {actual}"
                )
            }
            Self::FrameLengthMismatch { actual } => write!(
                formatter,
                "database owner control frame length {actual} is not {DATABASE_OWNER_CONTROL_V1_LENGTH}"
            ),
            Self::HeaderFlagsUnsupported { actual } => write!(
                formatter,
                "database owner control header flags {actual:#010x} are unsupported"
            ),
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "database owner control checksum mismatch: expected {expected:#018x}, got {actual:#018x}"
            ),
            Self::ReservedByteNonZero { offset, actual } => write!(
                formatter,
                "database owner control reserved byte at offset {offset} is nonzero: {actual:#04x}"
            ),
            Self::DatabaseIdZero => formatter.write_str("database owner control identity is zero"),
        }
    }
}

impl Error for DatabaseOwnerControlDecodeError {}

/// Encodes one inert database identity as a stable owner-control frame.
///
/// The returned bytes do not create, lock, publish, select, or authorize a
/// database. Filesystem creation belongs to the atomic create protocol.
#[must_use]
pub fn encode_database_owner_control(
    database_id: DatabaseId,
) -> [u8; DATABASE_OWNER_CONTROL_V1_LENGTH] {
    let mut frame = [0_u8; DATABASE_OWNER_CONTROL_V1_LENGTH];
    frame[..8].copy_from_slice(&DATABASE_OWNER_CONTROL_MAGIC);
    write_u16(&mut frame, 8, DATABASE_OWNER_CONTROL_FORMAT_VERSION);
    write_u16(&mut frame, 10, DATABASE_OWNER_CONTROL_LENGTH_U16);
    write_u32(&mut frame, DATABASE_OWNER_CONTROL_FLAGS_OFFSET, 0);
    write_u128(
        &mut frame,
        DATABASE_OWNER_CONTROL_DATABASE_ID_OFFSET,
        database_id.get(),
    );
    let checksum = checksum_v1(&frame[..DATABASE_OWNER_CONTROL_CHECKSUM_OFFSET]);
    write_u64(&mut frame, DATABASE_OWNER_CONTROL_CHECKSUM_OFFSET, checksum);
    frame
}

/// Decodes one complete stable owner-control frame into an inert identity.
pub fn decode_database_owner_control(
    bytes: &[u8],
) -> Result<DatabaseId, DatabaseOwnerControlDecodeError> {
    match bytes.len().cmp(&DATABASE_OWNER_CONTROL_V1_LENGTH) {
        std::cmp::Ordering::Less => {
            return Err(DatabaseOwnerControlDecodeError::Truncated {
                actual: bytes.len(),
            });
        }
        std::cmp::Ordering::Greater => {
            return Err(DatabaseOwnerControlDecodeError::TrailingBytes {
                actual: bytes.len(),
            });
        }
        std::cmp::Ordering::Equal => {}
    }

    if bytes[..8] != DATABASE_OWNER_CONTROL_MAGIC {
        let mut actual = [0_u8; 8];
        actual.copy_from_slice(&bytes[..8]);
        return Err(DatabaseOwnerControlDecodeError::MagicMismatch { actual });
    }
    let version = read_u16(bytes, 8);
    if version != DATABASE_OWNER_CONTROL_FORMAT_VERSION {
        return Err(DatabaseOwnerControlDecodeError::UnsupportedVersion { actual: version });
    }
    let frame_length = read_u16(bytes, 10);
    if frame_length != DATABASE_OWNER_CONTROL_LENGTH_U16 {
        return Err(DatabaseOwnerControlDecodeError::FrameLengthMismatch {
            actual: frame_length,
        });
    }
    let flags = read_u32(bytes, DATABASE_OWNER_CONTROL_FLAGS_OFFSET);
    if flags != 0 {
        return Err(DatabaseOwnerControlDecodeError::HeaderFlagsUnsupported { actual: flags });
    }
    let actual_checksum = read_u64(bytes, DATABASE_OWNER_CONTROL_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&bytes[..DATABASE_OWNER_CONTROL_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(DatabaseOwnerControlDecodeError::ChecksumMismatch {
            expected: expected_checksum,
            actual: actual_checksum,
        });
    }
    if let Some((relative_offset, actual)) = bytes
        [DATABASE_OWNER_CONTROL_RESERVED_START..DATABASE_OWNER_CONTROL_CHECKSUM_OFFSET]
        .iter()
        .copied()
        .enumerate()
        .find(|(_, byte)| *byte != 0)
    {
        return Err(DatabaseOwnerControlDecodeError::ReservedByteNonZero {
            offset: DATABASE_OWNER_CONTROL_RESERVED_START + relative_offset,
            actual,
        });
    }
    DatabaseId::new(read_u128(bytes, DATABASE_OWNER_CONTROL_DATABASE_ID_OFFSET))
        .ok_or(DatabaseOwnerControlDecodeError::DatabaseIdZero)
}

/// Inert trusted-path selection for one database composition.
///
/// Paths select objects to inspect. They are neither persistent identities nor
/// lock authority, and cloning this value does not clone any owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDatabaseLayout {
    database_owner: PathBuf,
    manifest: PathBuf,
    wal: PathBuf,
    page_store: PathBuf,
    restart_checkpoint: PathBuf,
}

impl FileDatabaseLayout {
    /// Selects the five trusted paths used by database ownership acquisition.
    #[must_use]
    pub fn new<OwnerPath, ManifestPath, WalPath, PageStorePath, RestartCheckpointPath>(
        database_owner: OwnerPath,
        manifest: ManifestPath,
        wal: WalPath,
        page_store: PageStorePath,
        restart_checkpoint: RestartCheckpointPath,
    ) -> Self
    where
        OwnerPath: Into<PathBuf>,
        ManifestPath: Into<PathBuf>,
        WalPath: Into<PathBuf>,
        PageStorePath: Into<PathBuf>,
        RestartCheckpointPath: Into<PathBuf>,
    {
        Self {
            database_owner: database_owner.into(),
            manifest: manifest.into(),
            wal: wal.into(),
            page_store: page_store.into(),
            restart_checkpoint: restart_checkpoint.into(),
        }
    }

    /// Returns the stable database-owner control path.
    #[must_use]
    pub fn database_owner(&self) -> &Path {
        &self.database_owner
    }

    /// Returns the currently selected manifest path.
    #[must_use]
    pub fn manifest(&self) -> &Path {
        &self.manifest
    }

    /// Returns the selected WAL path.
    #[must_use]
    pub fn wal(&self) -> &Path {
        &self.wal
    }

    /// Returns the selected page-store path.
    #[must_use]
    pub fn page_store(&self) -> &Path {
        &self.page_store
    }

    /// Returns the selected restart-checkpoint completeness slot directory.
    #[must_use]
    pub fn restart_checkpoint(&self) -> &Path {
        &self.restart_checkpoint
    }
}

/// One selected, candidate, or auxiliary entry in atomic database creation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FileDatabaseCreateEntry {
    /// Stable database-owner control.
    DatabaseOwner,
    /// Selected database manifest.
    Manifest,
    /// Unselected create manifest.
    ManifestCandidate,
    /// Unselected clean-close manifest.
    ManifestCloseCandidate,
    /// Selected WAL.
    Wal,
    /// Unselected create WAL.
    WalCandidate,
    /// Selected page store.
    PageStore,
    /// Unselected create page store.
    PageStoreCandidate,
    /// Selected restart-checkpoint slot directory.
    RestartCheckpoint,
    /// Unselected create restart-checkpoint slot directory.
    RestartCheckpointCandidate,
    /// Selected restart-checkpoint control file.
    RestartCheckpointControl,
    /// Unselected restart-checkpoint control file.
    RestartCheckpointCandidateControl,
    /// Disjoint clean-close restart-checkpoint slot.
    RestartCheckpointCleanCloseCandidate,
    /// Control file inside the clean-close restart-checkpoint slot.
    RestartCheckpointCleanCloseControl,
    /// WAL reclamation candidate derived from the selected WAL.
    WalReclamationCandidate,
    /// WAL reclamation candidate derived from the create WAL.
    WalCandidateReclamationCandidate,
}

impl fmt::Display for FileDatabaseCreateEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseOwner => formatter.write_str("database owner"),
            Self::Manifest => formatter.write_str("manifest"),
            Self::ManifestCandidate => formatter.write_str("manifest create candidate"),
            Self::ManifestCloseCandidate => formatter.write_str("manifest close candidate"),
            Self::Wal => formatter.write_str("WAL"),
            Self::WalCandidate => formatter.write_str("WAL create candidate"),
            Self::PageStore => formatter.write_str("page store"),
            Self::PageStoreCandidate => formatter.write_str("page-store create candidate"),
            Self::RestartCheckpoint => formatter.write_str("restart checkpoint"),
            Self::RestartCheckpointCandidate => {
                formatter.write_str("restart-checkpoint create candidate")
            }
            Self::RestartCheckpointControl => formatter.write_str("restart-checkpoint control"),
            Self::RestartCheckpointCandidateControl => {
                formatter.write_str("restart-checkpoint create-candidate control")
            }
            Self::RestartCheckpointCleanCloseCandidate => {
                formatter.write_str("restart-checkpoint clean-close candidate")
            }
            Self::RestartCheckpointCleanCloseControl => {
                formatter.write_str("restart-checkpoint clean-close candidate control")
            }
            Self::WalReclamationCandidate => formatter.write_str("WAL reclamation candidate"),
            Self::WalCandidateReclamationCandidate => {
                formatter.write_str("create-WAL reclamation candidate")
            }
        }
    }
}

/// One complete durable state in the atomic create prefix.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FileDatabaseCreatePhase {
    /// No selected or create entry exists.
    Absent,
    /// Only the stable owner exists.
    Owner,
    /// The owner and manifest candidate exist.
    ManifestCandidate,
    /// The manifest and WAL candidates exist.
    WalCandidate,
    /// The manifest, WAL, and page candidates exist.
    PageStoreCandidate,
    /// All four create candidates exist.
    RestartCheckpointCandidate,
    /// The WAL is selected and the other candidates remain.
    WalPublished,
    /// The WAL and page store are selected.
    PageStorePublished,
    /// All children are selected while the manifest remains a candidate.
    ChildrenPublished,
    /// The manifest and every child are selected.
    Published,
}

impl fmt::Display for FileDatabaseCreatePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => formatter.write_str("absent"),
            Self::Owner => formatter.write_str("owner"),
            Self::ManifestCandidate => formatter.write_str("manifest candidate"),
            Self::WalCandidate => formatter.write_str("WAL candidate"),
            Self::PageStoreCandidate => formatter.write_str("page-store candidate"),
            Self::RestartCheckpointCandidate => formatter.write_str("restart-checkpoint candidate"),
            Self::WalPublished => formatter.write_str("WAL published"),
            Self::PageStorePublished => formatter.write_str("page store published"),
            Self::ChildrenPublished => formatter.write_str("children published"),
            Self::Published => formatter.write_str("published"),
        }
    }
}

/// Exact durable effect boundary used by deterministic create fault tests.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FileDatabaseCreateBoundary {
    /// Stable owner creation and parent synchronization.
    OwnerPublication,
    /// Manifest-candidate creation and parent synchronization.
    ManifestCandidatePublication,
    /// WAL-candidate creation and parent synchronization.
    WalCandidatePublication,
    /// Page-candidate creation and parent synchronization.
    PageStoreCandidatePublication,
    /// Checkpoint-candidate creation and parent synchronization.
    RestartCheckpointCandidatePublication,
    /// WAL rename and selected-parent synchronization.
    WalPublication,
    /// Page-store rename and selected-parent synchronization.
    PageStorePublication,
    /// Checkpoint-directory rename and selected-parent synchronization.
    RestartCheckpointPublication,
    /// Manifest rename and selected-parent synchronization.
    ManifestPublication,
}

impl fmt::Display for FileDatabaseCreateBoundary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerPublication => formatter.write_str("owner publication"),
            Self::ManifestCandidatePublication => {
                formatter.write_str("manifest-candidate publication")
            }
            Self::WalCandidatePublication => formatter.write_str("WAL-candidate publication"),
            Self::PageStoreCandidatePublication => {
                formatter.write_str("page-store-candidate publication")
            }
            Self::RestartCheckpointCandidatePublication => {
                formatter.write_str("restart-checkpoint-candidate publication")
            }
            Self::WalPublication => formatter.write_str("WAL publication"),
            Self::PageStorePublication => formatter.write_str("page-store publication"),
            Self::RestartCheckpointPublication => {
                formatter.write_str("restart-checkpoint publication")
            }
            Self::ManifestPublication => formatter.write_str("manifest publication"),
        }
    }
}

/// Physical state and caller-certainty timing of one injected create fault.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FileDatabaseCreateFaultTiming {
    /// Fail definitely before installing the complete effect.
    BeforeEffect,
    /// Install the complete effect, then report a definite injected failure.
    AfterEffect,
    /// Leave the prior phase while making the caller treat the outcome as indeterminate.
    OutcomeIndeterminateBeforeEffect,
    /// Install the complete effect while making the caller treat the outcome as indeterminate.
    OutcomeIndeterminateAfterEffect,
}

impl FileDatabaseCreateFaultTiming {
    /// Returns whether the injected report is outcome-indeterminate.
    #[must_use]
    pub const fn is_outcome_indeterminate(self) -> bool {
        matches!(
            self,
            Self::OutcomeIndeterminateBeforeEffect | Self::OutcomeIndeterminateAfterEffect
        )
    }
}

impl fmt::Display for FileDatabaseCreateFaultTiming {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeEffect => formatter.write_str("before effect"),
            Self::AfterEffect => formatter.write_str("after effect"),
            Self::OutcomeIndeterminateBeforeEffect => {
                formatter.write_str("outcome-indeterminate before effect")
            }
            Self::OutcomeIndeterminateAfterEffect => {
                formatter.write_str("outcome-indeterminate after effect")
            }
        }
    }
}

/// One deterministic one-shot fault for atomic database creation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileDatabaseCreateFault {
    boundary: FileDatabaseCreateBoundary,
    timing: FileDatabaseCreateFaultTiming,
}

impl FileDatabaseCreateFault {
    /// Selects one exact boundary and timing.
    #[must_use]
    pub const fn new(
        boundary: FileDatabaseCreateBoundary,
        timing: FileDatabaseCreateFaultTiming,
    ) -> Self {
        Self { boundary, timing }
    }

    /// Returns the selected durable effect boundary.
    #[must_use]
    pub const fn boundary(self) -> FileDatabaseCreateBoundary {
        self.boundary
    }

    /// Returns the selected physical/certainty timing.
    #[must_use]
    pub const fn timing(self) -> FileDatabaseCreateFaultTiming {
        self.timing
    }
}

impl fmt::Display for FileDatabaseCreateFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.boundary, self.timing)
    }
}

/// Create-manifest prerequisite rejected before any filesystem mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDatabaseCreateManifestError {
    /// Initial creation requires lifecycle generation one.
    LifecycleGeneration {
        /// Exact rejected generation.
        actual: u64,
    },
    /// Initial creation requires one exact successor child format.
    StorageFormatVersion {
        /// Rejected child role.
        role: DatabaseFileRole,
        /// Required initial format.
        expected: u16,
        /// Exact manifest requirement.
        actual: u16,
    },
    /// Initial creation supports no required feature bit.
    RequiredFeatures {
        /// Exact rejected feature bits.
        actual: u64,
    },
}

impl fmt::Display for FileDatabaseCreateManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LifecycleGeneration { actual } => write!(
                formatter,
                "initial database lifecycle generation must be 1, not {actual}"
            ),
            Self::StorageFormatVersion {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "initial {role} format must be {expected}, not {actual}"
            ),
            Self::RequiredFeatures { actual } => write!(
                formatter,
                "initial database required feature bits must be zero, not {actual:#018x}"
            ),
        }
    }
}

impl Error for FileDatabaseCreateManifestError {}

/// Candidate or selected location of one child.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FileDatabaseCreateLocation {
    /// Fixed unselected `.create-candidate` location.
    Candidate,
    /// Selected final location.
    Final,
}

impl fmt::Display for FileDatabaseCreateLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Candidate => formatter.write_str("candidate"),
            Self::Final => formatter.write_str("final"),
        }
    }
}

/// Exact presence bitmap that did not match one legal create phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileDatabaseCreateNamespaceEvidence {
    present: u16,
}

impl FileDatabaseCreateNamespaceEvidence {
    /// Returns whether `entry` was observed as a directory entry.
    #[must_use]
    pub const fn contains(self, entry: FileDatabaseCreateEntry) -> bool {
        let bit = match entry {
            FileDatabaseCreateEntry::DatabaseOwner => 0,
            FileDatabaseCreateEntry::Manifest => 1,
            FileDatabaseCreateEntry::ManifestCandidate => 2,
            FileDatabaseCreateEntry::Wal => 3,
            FileDatabaseCreateEntry::WalCandidate => 4,
            FileDatabaseCreateEntry::PageStore => 5,
            FileDatabaseCreateEntry::PageStoreCandidate => 6,
            FileDatabaseCreateEntry::RestartCheckpoint => 7,
            FileDatabaseCreateEntry::RestartCheckpointCandidate => 8,
            FileDatabaseCreateEntry::RestartCheckpointControl
            | FileDatabaseCreateEntry::RestartCheckpointCandidateControl
            | FileDatabaseCreateEntry::ManifestCloseCandidate
            | FileDatabaseCreateEntry::RestartCheckpointCleanCloseCandidate
            | FileDatabaseCreateEntry::RestartCheckpointCleanCloseControl
            | FileDatabaseCreateEntry::WalReclamationCandidate
            | FileDatabaseCreateEntry::WalCandidateReclamationCandidate => return false,
        };
        self.present & (1 << bit) != 0
    }
}

/// Exact filesystem operation performed directly by the create coordinator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDatabaseCreateIoStage {
    /// Observing whether one directory entry exists.
    ObserveEntry {
        /// Entry being observed.
        entry: FileDatabaseCreateEntry,
    },
    /// Creating one exact file.
    CreateFile {
        /// Entry being created.
        entry: FileDatabaseCreateEntry,
    },
    /// Opening one existing file.
    OpenFile {
        /// Entry being opened.
        entry: FileDatabaseCreateEntry,
    },
    /// Acquiring one nonblocking exclusive lock.
    AcquireExclusiveLock {
        /// Entry being locked.
        entry: FileDatabaseCreateEntry,
    },
    /// Reading metadata from one opened object.
    ReadMetadata {
        /// Entry being inspected.
        entry: FileDatabaseCreateEntry,
    },
    /// Reading one complete fixed header.
    ReadHeader {
        /// Entry being decoded.
        entry: FileDatabaseCreateEntry,
    },
    /// Writing one complete fixed header.
    WriteHeader {
        /// Entry being initialized.
        entry: FileDatabaseCreateEntry,
    },
    /// Synchronizing one initialized or reopened object.
    SyncObject {
        /// Entry being synchronized.
        entry: FileDatabaseCreateEntry,
    },
    /// Reading a checkpoint candidate directory.
    ReadCheckpointDirectory {
        /// Candidate or final checkpoint location.
        location: FileDatabaseCreateLocation,
    },
    /// Opening a containing parent directory.
    OpenParentDirectory {
        /// Entry whose parent is required.
        entry: FileDatabaseCreateEntry,
    },
    /// Synchronizing a containing parent directory.
    SyncParentDirectory {
        /// Entry whose parent is synchronized.
        entry: FileDatabaseCreateEntry,
    },
    /// Renaming one candidate to its selected path.
    RenameCandidate {
        /// Child or manifest being published.
        entry: FileDatabaseCreateEntry,
    },
}

impl fmt::Display for FileDatabaseCreateIoStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObserveEntry { entry } => write!(formatter, "observing {entry}"),
            Self::CreateFile { entry } => write!(formatter, "creating {entry}"),
            Self::OpenFile { entry } => write!(formatter, "opening {entry}"),
            Self::AcquireExclusiveLock { entry } => {
                write!(formatter, "acquiring the {entry} exclusive lock")
            }
            Self::ReadMetadata { entry } => write!(formatter, "reading {entry} metadata"),
            Self::ReadHeader { entry } => write!(formatter, "reading the {entry} header"),
            Self::WriteHeader { entry } => write!(formatter, "writing the {entry} header"),
            Self::SyncObject { entry } => write!(formatter, "synchronizing {entry}"),
            Self::ReadCheckpointDirectory { location } => {
                write!(formatter, "reading the {location} checkpoint directory")
            }
            Self::OpenParentDirectory { entry } => {
                write!(formatter, "opening the {entry} parent directory")
            }
            Self::SyncParentDirectory { entry } => {
                write!(formatter, "synchronizing the {entry} parent directory")
            }
            Self::RenameCandidate { entry } => {
                write!(formatter, "renaming the {entry} create candidate")
            }
        }
    }
}

/// Stage-preserving coordinator I/O failure.
#[derive(Debug)]
pub struct FileDatabaseCreateIoError {
    stage: FileDatabaseCreateIoStage,
    source: io::Error,
}

impl FileDatabaseCreateIoError {
    fn new(stage: FileDatabaseCreateIoStage, source: io::Error) -> Self {
        Self { stage, source }
    }

    /// Returns the exact failed coordinator stage.
    #[must_use]
    pub const fn stage(&self) -> FileDatabaseCreateIoStage {
        self.stage
    }

    /// Returns the retained operating-system cause.
    #[must_use]
    pub const fn io_source(&self) -> &io::Error {
        &self.source
    }
}

impl fmt::Display for FileDatabaseCreateIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.stage, self.source)
    }
}

impl Error for FileDatabaseCreateIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Failure to create or resume one exact database composition.
#[derive(Debug)]
pub enum FileDatabaseCreateError {
    /// The requested page geometry is not representable.
    PageWidth(FilePageWidthError),
    /// The manifest is not an exact initial create manifest.
    ManifestRequirement(FileDatabaseCreateManifestError),
    /// A selected path cannot derive its fixed adjacent candidate.
    CandidatePathUnavailable {
        /// Selected entry without a terminal file name.
        entry: FileDatabaseCreateEntry,
    },
    /// Two trusted selected, candidate, or auxiliary names are identical.
    PathCollision {
        /// Earlier name in protocol order.
        first: FileDatabaseCreateEntry,
        /// Later colliding name.
        second: FileDatabaseCreateEntry,
    },
    /// One exact coordinator filesystem operation failed.
    Io(FileDatabaseCreateIoError),
    /// Another owner retains the stable database lock.
    OwnershipContended {
        /// Database whose stable owner was contended.
        database_id: DatabaseId,
        /// Original operating-system lock error.
        source: io::Error,
    },
    /// The observed top-level namespace is not one legal prefix.
    NamespaceConflict(FileDatabaseCreateNamespaceEvidence),
    /// A protocol entry has an unsupported filesystem object type.
    UnexpectedObjectType {
        /// Entry with the unsupported type.
        entry: FileDatabaseCreateEntry,
    },
    /// An unselected WAL reclamation entry exists during initial create.
    UnexpectedAuxiliaryEntry {
        /// Exact auxiliary entry that exists.
        entry: FileDatabaseCreateEntry,
    },
    /// Two locked protocol entries resolve to the same filesystem object.
    OpenedObjectAlias {
        /// Earlier entry in lock order.
        first: FileDatabaseCreateEntry,
        /// Later aliased entry.
        second: FileDatabaseCreateEntry,
    },
    /// A selected path resolved to another object between preflight and locked open.
    OpenedObjectChanged {
        /// Entry whose opened object changed.
        entry: FileDatabaseCreateEntry,
    },
    /// The stable owner-control frame is not exact.
    DatabaseOwnerControl(DatabaseOwnerControlDecodeError),
    /// The stable owner belongs to another database.
    DatabaseOwnerIdMismatch {
        /// Manifest-requested database identity.
        expected: DatabaseId,
        /// Owner-control database identity.
        actual: DatabaseId,
    },
    /// A candidate manifest file has a noncanonical length.
    ManifestFileLength {
        /// Candidate or final location.
        location: FileDatabaseCreateLocation,
        /// Exact observed byte length.
        actual: u64,
    },
    /// The requested manifest cannot be encoded by frame version 1.
    ManifestEncode(super::DatabaseManifestV1UnsupportedLifecycleState),
    /// A candidate manifest is structurally invalid.
    Manifest(super::DatabaseManifestDecodeError),
    /// A structurally valid candidate manifest differs from the request.
    ManifestMismatch(Box<FileDatabaseCreateManifestMismatch>),
    /// Creating a new WAL candidate failed.
    WalCreate(FileCreateError),
    /// Opening an existing candidate or final WAL failed.
    WalOpen(FileOpenError),
    /// Creating a new page-store candidate failed.
    PageStoreCreate(PageStoreCreateError),
    /// Opening an existing candidate or final page store failed.
    PageStoreOpen(PageStoreOpenError),
    /// Creating a new checkpoint candidate failed.
    RestartCheckpointCreate(FileRestartCheckpointSlotCreateError),
    /// Opening an existing candidate or final checkpoint failed.
    RestartCheckpointOpen(FileRestartCheckpointSlotOpenError),
    /// One parsed child contradicts the requested manifest.
    ChildValidation(FileDatabaseOwnershipOpenError),
    /// An existing child contains more than its exact initial header.
    NonInitialChild {
        /// Child role.
        role: DatabaseFileRole,
        /// Candidate or selected location.
        location: FileDatabaseCreateLocation,
    },
    /// A checkpoint slot contains an entry other than its exact control.
    UnexpectedCheckpointEntry {
        /// Candidate or selected location.
        location: FileDatabaseCreateLocation,
        /// Exact unexpected entry name.
        actual: OsString,
    },
    /// An exact published namespace failed ordinary successor-only open.
    PublishedOpen(FileDatabaseOwnershipOpenError),
    /// A deterministic fault fired at the requested boundary.
    InjectedFault(FileDatabaseCreateFault),
    /// The physical child set could not form one storage identity.
    ObservedStorageIdentity(DatabaseCompositionIdentityError),
    /// Domain manifest selection rejected the retained owner.
    ManifestSelection(DatabaseManifestSelectionRejection),
    /// Domain stable-storage binding rejected the retained observations.
    StorageBinding(DatabaseCompositionIdentityMismatch),
}

/// Exact requested and observed manifests retained by a create mismatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileDatabaseCreateManifestMismatch {
    expected: DatabaseManifest,
    actual: DatabaseManifest,
}

impl FileDatabaseCreateManifestMismatch {
    /// Returns the exact manifest requested by create.
    #[must_use]
    pub const fn expected(&self) -> DatabaseManifest {
        self.expected
    }

    /// Returns the exact structurally valid manifest found in storage.
    #[must_use]
    pub const fn actual(&self) -> DatabaseManifest {
        self.actual
    }
}

impl FileDatabaseCreateError {
    /// Returns whether the caller must use a fresh resolver attempt to decide the effect.
    #[must_use]
    pub const fn is_outcome_indeterminate(&self) -> bool {
        match self {
            Self::InjectedFault(fault) => fault.timing().is_outcome_indeterminate(),
            Self::Io(_) | Self::OwnershipContended { .. } => true,
            Self::WalCreate(source) => matches!(source, FileCreateError::Io(_)),
            Self::WalOpen(source) => matches!(source, FileOpenError::Io(_)),
            Self::PageStoreCreate(source) => matches!(source, PageStoreCreateError::Io(_)),
            Self::PageStoreOpen(source) => matches!(source, PageStoreOpenError::Io(_)),
            Self::RestartCheckpointCreate(source) => {
                matches!(source, FileRestartCheckpointSlotCreateError::Io(_))
            }
            Self::RestartCheckpointOpen(source) => {
                matches!(source, FileRestartCheckpointSlotOpenError::Io(_))
            }
            Self::ChildValidation(source) | Self::PublishedOpen(source) => {
                ownership_open_is_outcome_indeterminate(source)
            }
            Self::PageWidth(_)
            | Self::ManifestRequirement(_)
            | Self::CandidatePathUnavailable { .. }
            | Self::PathCollision { .. }
            | Self::NamespaceConflict(_)
            | Self::UnexpectedObjectType { .. }
            | Self::UnexpectedAuxiliaryEntry { .. }
            | Self::OpenedObjectAlias { .. }
            | Self::OpenedObjectChanged { .. }
            | Self::DatabaseOwnerControl(_)
            | Self::DatabaseOwnerIdMismatch { .. }
            | Self::ManifestFileLength { .. }
            | Self::Manifest(_)
            | Self::ManifestEncode(_)
            | Self::ManifestMismatch { .. }
            | Self::NonInitialChild { .. }
            | Self::UnexpectedCheckpointEntry { .. }
            | Self::ObservedStorageIdentity(_)
            | Self::ManifestSelection(_)
            | Self::StorageBinding(_) => false,
        }
    }
}

const fn ownership_open_is_outcome_indeterminate(source: &FileDatabaseOwnershipOpenError) -> bool {
    match source {
        FileDatabaseOwnershipOpenError::Io(_) => true,
        FileDatabaseOwnershipOpenError::WalOpen(source) => {
            matches!(source, FileOpenError::Io(_))
        }
        FileDatabaseOwnershipOpenError::PageStoreOpen(source) => {
            matches!(source, PageStoreOpenError::Io(_))
        }
        FileDatabaseOwnershipOpenError::RestartCheckpointOpen(source) => {
            matches!(source, FileRestartCheckpointSlotOpenError::Io(_))
        }
        FileDatabaseOwnershipOpenError::CleanCloseCheckpointFault(_) => false,
        FileDatabaseOwnershipOpenError::PageWidth(_)
        | FileDatabaseOwnershipOpenError::DatabaseOwnerControlFileLength { .. }
        | FileDatabaseOwnershipOpenError::DatabaseOwnerControl(_)
        | FileDatabaseOwnershipOpenError::DatabaseOwnerIdMismatch { .. }
        | FileDatabaseOwnershipOpenError::OpenedObjectAlias { .. }
        | FileDatabaseOwnershipOpenError::OpenedObjectChanged { .. }
        | FileDatabaseOwnershipOpenError::WalReclamationCandidateCollision { .. }
        | FileDatabaseOwnershipOpenError::CloseCandidatePathUnavailable { .. }
        | FileDatabaseOwnershipOpenError::CloseCandidatePathCollision { .. }
        | FileDatabaseOwnershipOpenError::ManifestFileLength { .. }
        | FileDatabaseOwnershipOpenError::Manifest(_)
        | FileDatabaseOwnershipOpenError::ManifestV2(_)
        | FileDatabaseOwnershipOpenError::ManifestDatabaseIdMismatch { .. }
        | FileDatabaseOwnershipOpenError::ManifestLifecycle { .. }
        | FileDatabaseOwnershipOpenError::StorageFormatVersionMismatch { .. }
        | FileDatabaseOwnershipOpenError::PersistentLogIdMismatch { .. }
        | FileDatabaseOwnershipOpenError::ChildDatabaseIdMismatch { .. }
        | FileDatabaseOwnershipOpenError::ChildFileRoleMismatch { .. }
        | FileDatabaseOwnershipOpenError::ChildFileIdMismatch { .. }
        | FileDatabaseOwnershipOpenError::StableStorageIdentityUnavailable { .. }
        | FileDatabaseOwnershipOpenError::ObservedStorageIdentity(_)
        | FileDatabaseOwnershipOpenError::StorageBinding(_)
        | FileDatabaseOwnershipOpenError::ManifestSelection(_) => false,
    }
}

impl fmt::Display for FileDatabaseCreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PageWidth(source) => {
                write!(formatter, "database page width is invalid: {source}")
            }
            Self::ManifestRequirement(source) => {
                write!(formatter, "database create manifest is invalid: {source}")
            }
            Self::CandidatePathUnavailable { entry } => {
                write!(formatter, "{entry} has no file name for a create candidate")
            }
            Self::PathCollision { first, second } => {
                write!(
                    formatter,
                    "database create {second} path collides with {first}"
                )
            }
            Self::Io(source) => source.fmt(formatter),
            Self::OwnershipContended {
                database_id,
                source,
            } => write!(
                formatter,
                "database {} is already owned: {source}",
                database_id.get()
            ),
            Self::NamespaceConflict(evidence) => write!(
                formatter,
                "database create namespace does not match a legal prefix: {:#05x}",
                evidence.present
            ),
            Self::UnexpectedObjectType { entry } => {
                write!(
                    formatter,
                    "database create {entry} has an unsupported object type"
                )
            }
            Self::UnexpectedAuxiliaryEntry { entry } => {
                write!(formatter, "database create found unexpected {entry}")
            }
            Self::OpenedObjectAlias { first, second } => {
                write!(formatter, "database create {second} aliases {first}")
            }
            Self::OpenedObjectChanged { entry } => {
                write!(
                    formatter,
                    "database create {entry} changed before locked open"
                )
            }
            Self::DatabaseOwnerControl(source) => {
                write!(formatter, "database owner control is invalid: {source}")
            }
            Self::DatabaseOwnerIdMismatch { expected, actual } => write!(
                formatter,
                "database owner identity {} does not match create identity {}",
                actual.get(),
                expected.get()
            ),
            Self::ManifestFileLength { location, actual } => write!(
                formatter,
                "{location} manifest length {actual} is not {}",
                super::DATABASE_MANIFEST_V1_LENGTH
            ),
            Self::ManifestEncode(source) => {
                write!(formatter, "create manifest cannot be encoded: {source}")
            }
            Self::Manifest(source) => write!(formatter, "manifest candidate is invalid: {source}"),
            Self::ManifestMismatch(_) => {
                formatter.write_str("manifest candidate does not match the requested manifest")
            }
            Self::WalCreate(source) => write!(formatter, "creating WAL candidate failed: {source}"),
            Self::WalOpen(source) => write!(formatter, "opening create WAL failed: {source}"),
            Self::PageStoreCreate(source) => {
                write!(formatter, "creating page-store candidate failed: {source}")
            }
            Self::PageStoreOpen(source) => {
                write!(formatter, "opening create page store failed: {source}")
            }
            Self::RestartCheckpointCreate(source) => {
                write!(formatter, "creating checkpoint candidate failed: {source}")
            }
            Self::RestartCheckpointOpen(source) => {
                write!(formatter, "opening create checkpoint failed: {source}")
            }
            Self::ChildValidation(source) => {
                write!(formatter, "create child validation failed: {source}")
            }
            Self::NonInitialChild { role, location } => {
                write!(formatter, "{location} {role} is not an exact initial child")
            }
            Self::UnexpectedCheckpointEntry { location, actual } => write!(
                formatter,
                "{location} checkpoint contains unexpected entry {actual:?}"
            ),
            Self::PublishedOpen(source) => {
                write!(formatter, "opening the published database failed: {source}")
            }
            Self::InjectedFault(fault) => {
                write!(formatter, "injected database create fault {fault}")
            }
            Self::ObservedStorageIdentity(source) => {
                write!(formatter, "created storage identity is invalid: {source}")
            }
            Self::ManifestSelection(source) => {
                write!(formatter, "created manifest selection failed: {source}")
            }
            Self::StorageBinding(source) => {
                write!(formatter, "created storage binding failed: {source}")
            }
        }
    }
}

impl Error for FileDatabaseCreateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PageWidth(source) => Some(source),
            Self::ManifestRequirement(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::OwnershipContended { source, .. } => Some(source),
            Self::DatabaseOwnerControl(source) => Some(source),
            Self::Manifest(source) => Some(source),
            Self::ManifestEncode(source) => Some(source),
            Self::WalCreate(source) => Some(source),
            Self::WalOpen(source) => Some(source),
            Self::PageStoreCreate(source) => Some(source),
            Self::PageStoreOpen(source) => Some(source),
            Self::RestartCheckpointCreate(source) => Some(source),
            Self::RestartCheckpointOpen(source) => Some(source),
            Self::ChildValidation(source) => Some(source),
            Self::PublishedOpen(source) => Some(source),
            Self::ObservedStorageIdentity(source) => Some(source),
            Self::ManifestSelection(source) => Some(source),
            Self::StorageBinding(source) => Some(source),
            Self::CandidatePathUnavailable { .. }
            | Self::PathCollision { .. }
            | Self::NamespaceConflict(_)
            | Self::UnexpectedObjectType { .. }
            | Self::UnexpectedAuxiliaryEntry { .. }
            | Self::OpenedObjectAlias { .. }
            | Self::OpenedObjectChanged { .. }
            | Self::DatabaseOwnerIdMismatch { .. }
            | Self::ManifestFileLength { .. }
            | Self::ManifestMismatch(_)
            | Self::NonInitialChild { .. }
            | Self::UnexpectedCheckpointEntry { .. }
            | Self::InjectedFault(_) => None,
        }
    }
}

/// Successful initial publication or exact already-published retry.
#[must_use = "created database ownership must remain inside its lifecycle typestate"]
pub enum FileDatabaseCreateOutcome<const N: usize> {
    /// This invocation completed manifest-last publication.
    Created(RecoveryRequiredFileDatabase<N>),
    /// The exact composition was already manifest-selected before this invocation.
    AlreadyPublished(RecoveryRequiredFileDatabase<N>),
}

impl<const N: usize> fmt::Debug for FileDatabaseCreateOutcome<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created(database) => formatter
                .debug_tuple("Created")
                .field(&database.identity())
                .finish(),
            Self::AlreadyPublished(database) => formatter
                .debug_tuple("AlreadyPublished")
                .field(&database.identity())
                .finish(),
        }
    }
}

/// Stable acquisition order labels for database-wide cooperative ownership.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FileDatabaseLockRole {
    /// Immutable database-owner control file.
    DatabaseOwner,
    /// Current database manifest inode.
    Manifest,
    /// Transaction/page WAL.
    Wal,
    /// Page store.
    PageStore,
    /// Restart-checkpoint completeness control file.
    RestartCheckpoint,
}

impl fmt::Display for FileDatabaseLockRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseOwner => formatter.write_str("database owner"),
            Self::Manifest => formatter.write_str("database manifest"),
            Self::Wal => formatter.write_str("WAL"),
            Self::PageStore => formatter.write_str("page store"),
            Self::RestartCheckpoint => formatter.write_str("restart checkpoint"),
        }
    }
}

/// Exact filesystem operation that failed during database ownership acquisition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDatabaseOwnershipIoStage {
    /// Opening one selected file for read/write access.
    OpenFile {
        /// Role of the selected file.
        role: FileDatabaseLockRole,
    },
    /// Reading opened-object metadata from one exact file handle.
    ReadMetadata {
        /// Role of the opened file.
        role: FileDatabaseLockRole,
    },
    /// Reading metadata for the derived WAL reclamation-candidate path.
    ReadWalReclamationCandidateMetadata,
    /// Acquiring one nonblocking exclusive advisory lock.
    AcquireExclusiveLock {
        /// Role of the lock target.
        role: FileDatabaseLockRole,
    },
    /// Reading one fixed owner-control or manifest frame.
    ReadHeader {
        /// Role of the opened file.
        role: FileDatabaseLockRole,
    },
    /// Synchronizing one selected file before exposing ownership.
    SyncFile {
        /// Role of the selected file.
        role: FileDatabaseLockRole,
    },
    /// Opening the selected file's parent directory.
    OpenParentDirectory {
        /// Role of the selected file.
        role: FileDatabaseLockRole,
    },
    /// Synchronizing the selected file's parent directory.
    SyncParentDirectory {
        /// Role of the selected file.
        role: FileDatabaseLockRole,
    },
}

impl fmt::Display for FileDatabaseOwnershipIoStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenFile { role } => write!(formatter, "opening database {role} file"),
            Self::ReadMetadata { role } => {
                write!(formatter, "reading opened database {role} metadata")
            }
            Self::ReadWalReclamationCandidateMetadata => {
                formatter.write_str("reading WAL reclamation-candidate metadata")
            }
            Self::AcquireExclusiveLock { role } => {
                write!(formatter, "acquiring database {role} exclusive lock")
            }
            Self::ReadHeader { role } => {
                write!(formatter, "reading database {role} header")
            }
            Self::SyncFile { role } => {
                write!(formatter, "synchronizing database {role} file")
            }
            Self::OpenParentDirectory { role } => {
                write!(formatter, "opening database {role} parent directory")
            }
            Self::SyncParentDirectory { role } => {
                write!(formatter, "synchronizing database {role} parent directory")
            }
        }
    }
}

/// Stage-specific filesystem failure during database ownership acquisition.
#[derive(Debug)]
pub struct FileDatabaseOwnershipIoError {
    stage: FileDatabaseOwnershipIoStage,
    source: io::Error,
}

impl FileDatabaseOwnershipIoError {
    fn new(stage: FileDatabaseOwnershipIoStage, source: io::Error) -> Self {
        Self { stage, source }
    }

    /// Returns the exact failed filesystem stage.
    #[must_use]
    pub const fn stage(&self) -> FileDatabaseOwnershipIoStage {
        self.stage
    }

    /// Returns the retained operating-system cause.
    #[must_use]
    pub const fn io_source(&self) -> &io::Error {
        &self.source
    }
}

impl fmt::Display for FileDatabaseOwnershipIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.stage, self.source)
    }
}

impl Error for FileDatabaseOwnershipIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Failure to acquire and validate one exact filesystem database composition.
#[derive(Debug)]
pub enum FileDatabaseOwnershipOpenError {
    /// The requested page geometry cannot be represented by child adapters.
    PageWidth(FilePageWidthError),
    /// One exact filesystem operation failed.
    Io(FileDatabaseOwnershipIoError),
    /// The stable owner-control file does not have its exact fixed length.
    DatabaseOwnerControlFileLength {
        /// Exact opened file length.
        actual: u64,
    },
    /// The stable owner-control bytes are structurally invalid.
    DatabaseOwnerControl(DatabaseOwnerControlDecodeError),
    /// The stable owner belongs to a different requested database.
    DatabaseOwnerIdMismatch {
        /// Caller-requested database identity.
        expected: DatabaseId,
        /// Identity decoded from the stable owner control.
        actual: DatabaseId,
    },
    /// Two selected roles resolve to the same opened filesystem object.
    OpenedObjectAlias {
        /// Earlier role in acquisition order.
        first: FileDatabaseLockRole,
        /// Later aliased role.
        second: FileDatabaseLockRole,
    },
    /// A path resolved to another opened object between preflight and locked open.
    OpenedObjectChanged {
        /// Role whose selected opened object changed.
        role: FileDatabaseLockRole,
    },
    /// The derived WAL reclamation candidate selects a database lock target.
    WalReclamationCandidateCollision {
        /// Protected role selected by the candidate path or opened object.
        role: FileDatabaseLockRole,
    },
    /// A reserved close candidate cannot be derived from its selected path.
    CloseCandidatePathUnavailable {
        /// Exact reserved entry whose sibling path could not be derived.
        entry: FileDatabaseCreateEntry,
    },
    /// Two selected or reserved candidate paths are lexically equal.
    CloseCandidatePathCollision {
        /// Earlier role in stable validation order.
        first: FileDatabaseCreateEntry,
        /// Later colliding role.
        second: FileDatabaseCreateEntry,
    },
    /// The selected manifest file does not have its exact fixed length.
    ManifestFileLength {
        /// Exact opened file length.
        actual: u64,
    },
    /// The selected manifest bytes are structurally invalid.
    Manifest(super::DatabaseManifestDecodeError),
    /// The selected Manifest V2 bytes are structurally invalid.
    ManifestV2(super::DatabaseManifestV2DecodeError),
    /// The manifest belongs to a database other than the locked owner.
    ManifestDatabaseIdMismatch {
        /// Identity decoded from the stable owner control.
        owner: DatabaseId,
        /// Identity decoded from the manifest.
        manifest: DatabaseId,
    },
    /// Recovery-required open received another manifest lifecycle.
    ManifestLifecycle {
        /// Exact rejected lifecycle state.
        actual: ntsql_database::DatabaseManifestLifecycleState,
    },
    /// The selected WAL could not be locked and reconstructed.
    WalOpen(FileOpenError),
    /// The selected page store could not be locked and reconstructed.
    PageStoreOpen(PageStoreOpenError),
    /// The selected restart-checkpoint completeness slot could not be locked and
    /// reconstructed.
    RestartCheckpointOpen(FileRestartCheckpointSlotOpenError),
    /// A clean-close checkpoint fault was already armed on a freshly opened source.
    CleanCloseCheckpointFault(FileCleanCloseCheckpointFaultAlreadyArmed),
    /// One child physical format differs from the selected manifest requirement.
    StorageFormatVersionMismatch {
        /// Child role.
        role: DatabaseFileRole,
        /// Exact manifest-required version.
        required: DatabaseStorageFormatVersion,
        /// Exact observed version.
        actual: u16,
    },
    /// One child persistent WAL identity differs from the manifest.
    PersistentLogIdMismatch {
        /// Child role.
        role: DatabaseFileRole,
        /// Exact manifest-required identity.
        expected: PersistentLogId,
        /// Exact observed child identity.
        actual: PersistentLogId,
    },
    /// A successor child header belongs to another logical database.
    ChildDatabaseIdMismatch {
        /// Child role being opened.
        role: DatabaseFileRole,
        /// Manifest-selected database identity.
        expected: DatabaseId,
        /// Physically decoded database identity.
        actual: DatabaseId,
    },
    /// A successor child header identifies a different role.
    ChildFileRoleMismatch {
        /// Role selected by the database layout.
        expected: DatabaseFileRole,
        /// Role physically decoded from the child header.
        actual: DatabaseFileRole,
    },
    /// A successor child header identifies another file.
    ChildFileIdMismatch {
        /// Child role being opened.
        role: DatabaseFileRole,
        /// Manifest-selected file identity.
        expected: ntsql_database::DatabaseFileId,
        /// Physically decoded file identity.
        actual: ntsql_database::DatabaseFileId,
    },
    /// A legacy child lacks the stable physical identity needed for binding.
    StableStorageIdentityUnavailable {
        /// First role without successor identity in stable order.
        role: DatabaseFileRole,
    },
    /// The physically observed role set is internally invalid.
    ObservedStorageIdentity(DatabaseCompositionIdentityError),
    /// The domain storage-binding gate rejected physical evidence.
    StorageBinding(DatabaseCompositionIdentityMismatch),
    /// The domain manifest-selection gate rejected the locked owner.
    ManifestSelection(DatabaseManifestSelectionRejection),
}

impl fmt::Display for FileDatabaseOwnershipOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PageWidth(source) => {
                write!(formatter, "database page width is invalid: {source}")
            }
            Self::Io(source) => source.fmt(formatter),
            Self::DatabaseOwnerControlFileLength { actual } => write!(
                formatter,
                "database owner control file length {actual} is not {DATABASE_OWNER_CONTROL_V1_LENGTH}"
            ),
            Self::DatabaseOwnerControl(source) => {
                write!(formatter, "database owner control decode failed: {source}")
            }
            Self::DatabaseOwnerIdMismatch { expected, actual } => write!(
                formatter,
                "database owner identity {} does not match requested database {}",
                actual.get(),
                expected.get()
            ),
            Self::OpenedObjectAlias { first, second } => {
                write!(formatter, "database {second} aliases opened {first}")
            }
            Self::OpenedObjectChanged { role } => write!(
                formatter,
                "database {role} selected object changed before locked open"
            ),
            Self::WalReclamationCandidateCollision { role } => write!(
                formatter,
                "WAL reclamation candidate collides with database {role}"
            ),
            Self::CloseCandidatePathUnavailable { entry } => {
                write!(formatter, "cannot derive reserved {entry} path")
            }
            Self::CloseCandidatePathCollision { first, second } => {
                write!(
                    formatter,
                    "reserved database {second} path collides with {first}"
                )
            }
            Self::ManifestFileLength { actual } => write!(
                formatter,
                "database manifest file length {actual} is neither {} nor {}",
                super::DATABASE_MANIFEST_V1_LENGTH,
                super::DATABASE_MANIFEST_V2_LENGTH
            ),
            Self::Manifest(source) => {
                write!(formatter, "database manifest decode failed: {source}")
            }
            Self::ManifestV2(source) => {
                write!(formatter, "database Manifest V2 decode failed: {source}")
            }
            Self::ManifestDatabaseIdMismatch { owner, manifest } => write!(
                formatter,
                "database manifest identity {} does not match locked owner {}",
                manifest.get(),
                owner.get()
            ),
            Self::ManifestLifecycle { actual } => write!(
                formatter,
                "recovery-required filesystem open cannot select {actual} manifest"
            ),
            Self::WalOpen(source) => {
                write!(formatter, "database WAL open failed: {source}")
            }
            Self::PageStoreOpen(source) => {
                write!(formatter, "database page-store open failed: {source}")
            }
            Self::RestartCheckpointOpen(source) => write!(
                formatter,
                "database restart-checkpoint completeness open failed: {source}"
            ),
            Self::CleanCloseCheckpointFault(source) => {
                write!(
                    formatter,
                    "database clean-close checkpoint fault setup failed: {source}"
                )
            }
            Self::StorageFormatVersionMismatch {
                role,
                required,
                actual,
            } => write!(
                formatter,
                "database {role} format version {actual} does not match manifest requirement {}",
                required.get()
            ),
            Self::PersistentLogIdMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "database {role} persistent WAL identity {} does not match manifest identity {}",
                actual.get(),
                expected.get()
            ),
            Self::ChildDatabaseIdMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "database {role} header database identity {} does not match manifest identity {}",
                actual.get(),
                expected.get()
            ),
            Self::ChildFileRoleMismatch { expected, actual } => write!(
                formatter,
                "database child header role {actual} does not match selected {expected}"
            ),
            Self::ChildFileIdMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "database {role} header file identity {} does not match manifest identity {}",
                actual.get(),
                expected.get()
            ),
            Self::StableStorageIdentityUnavailable { role } => write!(
                formatter,
                "database {role} uses a legacy header without stable storage identity"
            ),
            Self::ObservedStorageIdentity(source) => {
                write!(
                    formatter,
                    "observed database storage identity is invalid: {source}"
                )
            }
            Self::StorageBinding(source) => {
                write!(formatter, "database storage binding failed: {source}")
            }
            Self::ManifestSelection(source) => {
                write!(
                    formatter,
                    "locked database manifest selection failed: {source}"
                )
            }
        }
    }
}

impl Error for FileDatabaseOwnershipOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PageWidth(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::DatabaseOwnerControl(source) => Some(source),
            Self::Manifest(source) => Some(source),
            Self::ManifestV2(source) => Some(source),
            Self::WalOpen(source) => Some(source),
            Self::PageStoreOpen(source) => Some(source),
            Self::RestartCheckpointOpen(source) => Some(source),
            Self::CleanCloseCheckpointFault(source) => Some(source),
            Self::ManifestSelection(source) => Some(source),
            Self::ObservedStorageIdentity(source) => Some(source),
            Self::StorageBinding(source) => Some(source),
            Self::DatabaseOwnerControlFileLength { .. }
            | Self::DatabaseOwnerIdMismatch { .. }
            | Self::OpenedObjectAlias { .. }
            | Self::OpenedObjectChanged { .. }
            | Self::WalReclamationCandidateCollision { .. }
            | Self::CloseCandidatePathUnavailable { .. }
            | Self::CloseCandidatePathCollision { .. }
            | Self::ManifestFileLength { .. }
            | Self::ManifestDatabaseIdMismatch { .. }
            | Self::ManifestLifecycle { .. }
            | Self::StorageFormatVersionMismatch { .. }
            | Self::PersistentLogIdMismatch { .. }
            | Self::ChildDatabaseIdMismatch { .. }
            | Self::ChildFileRoleMismatch { .. }
            | Self::ChildFileIdMismatch { .. }
            | Self::StableStorageIdentityUnavailable { .. } => None,
        }
    }
}

impl From<FileDatabaseOwnershipIoError> for FileDatabaseOwnershipOpenError {
    fn from(source: FileDatabaseOwnershipIoError) -> Self {
        Self::Io(source)
    }
}

/// Complete filesystem lock set and unrecovered child composition.
///
/// Construction is private to [`open_file_database_ownership`]. The files are
/// retained so ordinary destruction releases the complete child, manifest, and
/// stable owner lock set without an explicit unlock path.
///
/// ```compile_fail
/// use ntsql_storage_file::FileDatabaseOwnership;
///
/// fn cannot_clone<const N: usize>(ownership: FileDatabaseOwnership<N>) {
///     let first = ownership;
///     let second = ownership;
/// }
/// ```
#[must_use = "database ownership must remain inside its lifecycle typestate"]
pub struct FileDatabaseOwnership<const N: usize> {
    composition: UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint<N>,
    _manifest_file: File,
    _database_owner_file: File,
    manifest: DatabaseManifest,
    layout: FileDatabaseLayout,
}

impl<const N: usize> FileDatabaseOwnership<N> {
    /// Returns the validated inert manifest retained by this lock set.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.manifest
    }

    /// Returns the inert trusted-path selection retained for later handoffs.
    #[must_use]
    pub const fn layout(&self) -> &FileDatabaseLayout {
        &self.layout
    }
}

impl<const N: usize> fmt::Debug for FileDatabaseOwnership<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileDatabaseOwnership")
            .field("manifest", &self.manifest)
            .field("composition", &self.composition)
            .finish_non_exhaustive()
    }
}

/// Ordered physical boundary in filesystem clean-manifest publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDatabaseCloseBoundary {
    /// Remove one stale manifest close candidate.
    CandidateCleanup,
    /// Create and lock the new candidate descriptor.
    CandidateCreate,
    /// Write the exact Manifest V2 frame.
    CandidateWrite,
    /// Synchronize the candidate descriptor.
    CandidateSynchronization,
    /// Atomically replace the selected manifest path.
    ManifestReplacement,
    /// Verify selected inode and exact decoded target.
    SelectedManifestVerification,
    /// Synchronize the selected manifest's containing directory.
    ParentDirectorySynchronization,
}

/// Deterministic timing for one filesystem close-publication fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDatabaseCloseFaultTiming {
    /// Report a definite failure before the boundary effect.
    BeforeEffect,
    /// Apply the boundary effect and report a definite failure.
    AfterEffect,
    /// Report an indeterminate outcome without applying the effect.
    OutcomeIndeterminateBeforeEffect,
    /// Apply the effect and report an indeterminate outcome.
    OutcomeIndeterminateAfterEffect,
}

impl FileDatabaseCloseFaultTiming {
    const fn is_before(self) -> bool {
        matches!(
            self,
            Self::BeforeEffect | Self::OutcomeIndeterminateBeforeEffect
        )
    }

    const fn is_after(self) -> bool {
        matches!(
            self,
            Self::AfterEffect | Self::OutcomeIndeterminateAfterEffect
        )
    }

    /// Returns whether the injected report hides its physical outcome.
    #[must_use]
    pub const fn is_outcome_indeterminate(self) -> bool {
        matches!(
            self,
            Self::OutcomeIndeterminateBeforeEffect | Self::OutcomeIndeterminateAfterEffect
        )
    }
}

/// One exact filesystem clean-manifest publication fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileDatabaseCloseFault {
    boundary: FileDatabaseCloseBoundary,
    timing: FileDatabaseCloseFaultTiming,
}

impl FileDatabaseCloseFault {
    /// Arms one exact physical boundary and timing.
    #[must_use]
    pub const fn new(
        boundary: FileDatabaseCloseBoundary,
        timing: FileDatabaseCloseFaultTiming,
    ) -> Self {
        Self { boundary, timing }
    }

    /// Returns the armed physical boundary.
    #[must_use]
    pub const fn boundary(self) -> FileDatabaseCloseBoundary {
        self.boundary
    }

    /// Returns the armed timing.
    #[must_use]
    pub const fn timing(self) -> FileDatabaseCloseFaultTiming {
        self.timing
    }

    const fn publication_state(self) -> DatabaseCleanManifestPublicationState {
        match (self.boundary, self.timing) {
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
}

impl fmt::Display for FileDatabaseCloseFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?} at {:?}", self.boundary, self.timing)
    }
}

/// Exact filesystem operation that failed during clean-manifest publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDatabaseClosePublicationIoStage {
    /// Inspecting a stale close-candidate entry.
    InspectCandidate,
    /// Removing a stale close-candidate entry.
    RemoveCandidate,
    /// Creating a fresh close-candidate file.
    CreateCandidate,
    /// Locking the fresh candidate descriptor.
    LockCandidate,
    /// Writing the exact Manifest V2 frame.
    WriteCandidate,
    /// Synchronizing the candidate descriptor.
    SyncCandidate,
    /// Replacing the selected manifest path.
    ReplaceManifest,
    /// Reading selected-path metadata after replacement.
    ReadSelectedMetadata,
    /// Reading retained candidate-descriptor metadata.
    ReadCandidateMetadata,
    /// Seeking the retained candidate descriptor.
    SeekCandidate,
    /// Reading the retained candidate descriptor.
    ReadCandidate,
    /// Opening the selected manifest's parent directory.
    OpenParentDirectory,
    /// Synchronizing the selected manifest's parent directory.
    SyncParentDirectory,
}

impl fmt::Display for FileDatabaseClosePublicationIoStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InspectCandidate => formatter.write_str("inspecting manifest close candidate"),
            Self::RemoveCandidate => formatter.write_str("removing manifest close candidate"),
            Self::CreateCandidate => formatter.write_str("creating manifest close candidate"),
            Self::LockCandidate => formatter.write_str("locking manifest close candidate"),
            Self::WriteCandidate => formatter.write_str("writing Manifest V2 close candidate"),
            Self::SyncCandidate => formatter.write_str("synchronizing Manifest V2 close candidate"),
            Self::ReplaceManifest => formatter.write_str("replacing selected database manifest"),
            Self::ReadSelectedMetadata => formatter.write_str("reading selected manifest metadata"),
            Self::ReadCandidateMetadata => {
                formatter.write_str("reading retained manifest candidate metadata")
            }
            Self::SeekCandidate => formatter.write_str("seeking retained manifest candidate"),
            Self::ReadCandidate => formatter.write_str("reading retained manifest candidate"),
            Self::OpenParentDirectory => {
                formatter.write_str("opening selected manifest parent directory")
            }
            Self::SyncParentDirectory => {
                formatter.write_str("synchronizing selected manifest parent directory")
            }
        }
    }
}

/// Stage-specific filesystem cause retained by failed close publication.
#[derive(Debug)]
pub struct FileDatabaseClosePublicationIoError {
    stage: FileDatabaseClosePublicationIoStage,
    source: io::Error,
}

impl FileDatabaseClosePublicationIoError {
    fn new(stage: FileDatabaseClosePublicationIoStage, source: io::Error) -> Self {
        Self { stage, source }
    }

    /// Returns the exact failed filesystem stage.
    #[must_use]
    pub const fn stage(&self) -> FileDatabaseClosePublicationIoStage {
        self.stage
    }

    /// Returns the retained operating-system cause.
    #[must_use]
    pub const fn io_source(&self) -> &io::Error {
        &self.source
    }
}

impl fmt::Display for FileDatabaseClosePublicationIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.stage, self.source)
    }
}

impl Error for FileDatabaseClosePublicationIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Filesystem-adapter cause for a clean-manifest publication failure.
#[derive(Debug)]
pub enum FileDatabaseClosePublicationError {
    /// The permit target is not an exact lifecycle successor.
    TargetManifest(DatabaseManifestSuccessorError),
    /// The selected manifest path has no sibling candidate path.
    CandidatePathUnavailable {
        /// Exact selected manifest path.
        selected: PathBuf,
    },
    /// A stale candidate is a directory and cannot be reconciled as a file.
    CandidateIsDirectory {
        /// Exact rejected candidate path.
        path: PathBuf,
    },
    /// The retained candidate descriptor disappeared from private owner state.
    CandidateDescriptorMissing,
    /// The selected manifest's containing directory cannot be derived.
    ParentDirectoryUnavailable {
        /// Exact selected manifest path.
        selected: PathBuf,
    },
    /// Selected path and retained candidate descriptor identify different files.
    SelectedObjectChanged,
    /// The selected Manifest V2 descriptor has a noncanonical length.
    SelectedManifestFileLength {
        /// Exact observed byte length.
        actual: u64,
    },
    /// The selected descriptor changed length during exact verification.
    SelectedManifestLengthChanged {
        /// Length before reading.
        before: u64,
        /// Length after reading.
        after: u64,
    },
    /// The selected Manifest V2 frame is structurally invalid.
    SelectedManifestDecode(super::DatabaseManifestV2DecodeError),
    /// The freshly decoded selected manifest differs from the permit target.
    SelectedManifestMismatch,
    /// One deterministic fault fired.
    InjectedFault(FileDatabaseCloseFault),
    /// One exact filesystem operation failed.
    Io(FileDatabaseClosePublicationIoError),
}

impl fmt::Display for FileDatabaseClosePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetManifest(source) => {
                write!(
                    formatter,
                    "filesystem clean-manifest target is invalid: {source}"
                )
            }
            Self::CandidatePathUnavailable { selected } => write!(
                formatter,
                "selected manifest path {} has no close-candidate sibling",
                selected.display()
            ),
            Self::CandidateIsDirectory { path } => write!(
                formatter,
                "manifest close candidate {} is a directory",
                path.display()
            ),
            Self::CandidateDescriptorMissing => {
                formatter.write_str("retained manifest close-candidate descriptor is missing")
            }
            Self::ParentDirectoryUnavailable { selected } => write!(
                formatter,
                "selected manifest path {} has no parent directory",
                selected.display()
            ),
            Self::SelectedObjectChanged => formatter.write_str(
                "selected manifest path does not identify the retained candidate descriptor",
            ),
            Self::SelectedManifestFileLength { actual } => write!(
                formatter,
                "selected clean manifest length {actual} is not {}",
                super::DATABASE_MANIFEST_V2_LENGTH
            ),
            Self::SelectedManifestLengthChanged { before, after } => write!(
                formatter,
                "selected clean manifest length changed while reading: {before} to {after}"
            ),
            Self::SelectedManifestDecode(source) => {
                write!(formatter, "selected clean manifest decode failed: {source}")
            }
            Self::SelectedManifestMismatch => {
                formatter.write_str("selected clean manifest differs from the exact permit target")
            }
            Self::InjectedFault(fault) => {
                write!(
                    formatter,
                    "injected filesystem database close fault {fault}"
                )
            }
            Self::Io(source) => source.fmt(formatter),
        }
    }
}

impl Error for FileDatabaseClosePublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TargetManifest(source) => Some(source),
            Self::SelectedManifestDecode(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::CandidatePathUnavailable { .. }
            | Self::CandidateIsDirectory { .. }
            | Self::CandidateDescriptorMissing
            | Self::ParentDirectoryUnavailable { .. }
            | Self::SelectedObjectChanged
            | Self::SelectedManifestFileLength { .. }
            | Self::SelectedManifestLengthChanged { .. }
            | Self::SelectedManifestMismatch
            | Self::InjectedFault(_) => None,
        }
    }
}

struct FileDatabaseCloseFaultController {
    armed: Option<FileDatabaseCloseFault>,
}

impl FileDatabaseCloseFaultController {
    const fn new(armed: Option<FileDatabaseCloseFault>) -> Self {
        Self { armed }
    }

    fn before(
        &mut self,
        boundary: FileDatabaseCloseBoundary,
    ) -> Result<(), DatabaseCleanManifestPublisherFailure<FileDatabaseClosePublicationError>> {
        self.fire(boundary, FileDatabaseCloseFaultTiming::is_before)
    }

    fn after(
        &mut self,
        boundary: FileDatabaseCloseBoundary,
    ) -> Result<(), DatabaseCleanManifestPublisherFailure<FileDatabaseClosePublicationError>> {
        self.fire(boundary, FileDatabaseCloseFaultTiming::is_after)
    }

    fn fire(
        &mut self,
        boundary: FileDatabaseCloseBoundary,
        matches_timing: fn(FileDatabaseCloseFaultTiming) -> bool,
    ) -> Result<(), DatabaseCleanManifestPublisherFailure<FileDatabaseClosePublicationError>> {
        if let Some(fault) = self.armed
            && fault.boundary() == boundary
            && matches_timing(fault.timing())
        {
            self.armed = None;
            return Err(DatabaseCleanManifestPublisherFailure::new(
                fault.publication_state(),
                FileDatabaseClosePublicationError::InjectedFault(fault),
            ));
        }
        Ok(())
    }
}

/// Filesystem database/manifest locks retained after transaction recovery.
#[must_use = "live filesystem database ownership must remain inside its database typestate"]
pub struct RecoveredFileDatabaseOuterOwnership {
    _manifest_file: File,
    _close_manifest_file: Option<File>,
    _database_owner_file: File,
    manifest: DatabaseManifest,
    layout: FileDatabaseLayout,
    compatibility_context: CompatibilityContext,
}

impl RecoveredFileDatabaseOuterOwnership {
    /// Returns the exact validated manifest retained under lock.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.manifest
    }

    /// Returns the trusted selected layout retained by this owner.
    #[must_use]
    pub const fn layout(&self) -> &FileDatabaseLayout {
        &self.layout
    }

    /// Returns the one immutable exact-target context moved through open.
    #[must_use]
    pub const fn compatibility_context(&self) -> &CompatibilityContext {
        &self.compatibility_context
    }
}

impl DatabaseCloseSourceManifestOwner for RecoveredFileDatabaseOuterOwnership {
    fn close_source_manifest(&self) -> DatabaseManifest {
        self.manifest()
    }
}

fn file_database_close_io_failure(
    state: DatabaseCleanManifestPublicationState,
    stage: FileDatabaseClosePublicationIoStage,
    source: io::Error,
) -> DatabaseCleanManifestPublisherFailure<FileDatabaseClosePublicationError> {
    DatabaseCleanManifestPublisherFailure::new(
        state,
        FileDatabaseClosePublicationError::Io(FileDatabaseClosePublicationIoError::new(
            stage, source,
        )),
    )
}

impl DatabaseCleanManifestPublisher for RecoveredFileDatabaseOuterOwnership {
    type Input = Option<FileDatabaseCloseFault>;
    type Error = FileDatabaseClosePublicationError;

    fn publish_clean_manifest(
        &mut self,
        input: Self::Input,
        permit: DatabaseCleanManifestPublicationPermit<'_>,
    ) -> Result<
        DatabaseCleanManifestPublicationReceipt,
        DatabaseCleanManifestPublisherFailure<Self::Error>,
    > {
        let source_manifest = self.manifest;
        let target_manifest = permit.target_manifest();
        target_manifest
            .require_successor_of(source_manifest)
            .map_err(|source| {
                DatabaseCleanManifestPublisherFailure::new(
                    DatabaseCleanManifestPublicationState::SourceSelected,
                    FileDatabaseClosePublicationError::TargetManifest(source),
                )
            })?;
        let candidate_path = database_manifest_close_candidate_path(self.layout.manifest())
            .ok_or_else(|| {
                DatabaseCleanManifestPublisherFailure::new(
                    DatabaseCleanManifestPublicationState::SourceSelected,
                    FileDatabaseClosePublicationError::CandidatePathUnavailable {
                        selected: self.layout.manifest().to_path_buf(),
                    },
                )
            })?;
        let encoded = encode_database_manifest_v2(&target_manifest);
        let mut fault = FileDatabaseCloseFaultController::new(input);

        fault.before(FileDatabaseCloseBoundary::CandidateCleanup)?;
        match fs::symlink_metadata(&candidate_path) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                return Err(DatabaseCleanManifestPublisherFailure::new(
                    DatabaseCleanManifestPublicationState::SourceSelected,
                    FileDatabaseClosePublicationError::CandidateIsDirectory {
                        path: candidate_path,
                    },
                ));
            }
            Ok(_) => fs::remove_file(&candidate_path).map_err(|source| {
                file_database_close_io_failure(
                    DatabaseCleanManifestPublicationState::SourceSelected,
                    FileDatabaseClosePublicationIoStage::RemoveCandidate,
                    source,
                )
            })?,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(file_database_close_io_failure(
                    DatabaseCleanManifestPublicationState::SourceSelected,
                    FileDatabaseClosePublicationIoStage::InspectCandidate,
                    source,
                ));
            }
        }
        fault.after(FileDatabaseCloseBoundary::CandidateCleanup)?;

        fault.before(FileDatabaseCloseBoundary::CandidateCreate)?;
        let candidate_file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&candidate_path)
            .map_err(|source| {
                file_database_close_io_failure(
                    DatabaseCleanManifestPublicationState::SourceSelected,
                    FileDatabaseClosePublicationIoStage::CreateCandidate,
                    source,
                )
            })?;
        candidate_file.try_lock().map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::SourceSelected,
                FileDatabaseClosePublicationIoStage::LockCandidate,
                source.into(),
            )
        })?;
        self._close_manifest_file = Some(candidate_file);
        fault.after(FileDatabaseCloseBoundary::CandidateCreate)?;

        fault.before(FileDatabaseCloseBoundary::CandidateWrite)?;
        let candidate_file = self._close_manifest_file.as_mut().ok_or_else(|| {
            DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SourceSelected,
                FileDatabaseClosePublicationError::CandidateDescriptorMissing,
            )
        })?;
        candidate_file.write_all(&encoded).map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::SourceSelected,
                FileDatabaseClosePublicationIoStage::WriteCandidate,
                source,
            )
        })?;
        fault.after(FileDatabaseCloseBoundary::CandidateWrite)?;

        fault.before(FileDatabaseCloseBoundary::CandidateSynchronization)?;
        candidate_file.sync_all().map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::SourceSelected,
                FileDatabaseClosePublicationIoStage::SyncCandidate,
                source,
            )
        })?;
        fault.after(FileDatabaseCloseBoundary::CandidateSynchronization)?;

        fault.before(FileDatabaseCloseBoundary::ManifestReplacement)?;
        fs::rename(&candidate_path, self.layout.manifest()).map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                FileDatabaseClosePublicationIoStage::ReplaceManifest,
                source,
            )
        })?;
        fault.after(FileDatabaseCloseBoundary::ManifestReplacement)?;

        fault.before(FileDatabaseCloseBoundary::SelectedManifestVerification)?;
        let selected_metadata = fs::metadata(self.layout.manifest()).map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationIoStage::ReadSelectedMetadata,
                source,
            )
        })?;
        let candidate_metadata = candidate_file.metadata().map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationIoStage::ReadCandidateMetadata,
                source,
            )
        })?;
        if !metadata_identifies_same_file(&selected_metadata, &candidate_metadata) {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                FileDatabaseClosePublicationError::SelectedObjectChanged,
            ));
        }
        let before = candidate_metadata.len();
        if before != super::DATABASE_MANIFEST_V2_LENGTH as u64 {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationError::SelectedManifestFileLength { actual: before },
            ));
        }
        candidate_file.seek(SeekFrom::Start(0)).map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationIoStage::SeekCandidate,
                source,
            )
        })?;
        let mut selected_bytes = [0_u8; super::DATABASE_MANIFEST_V2_LENGTH];
        candidate_file
            .read_exact(&mut selected_bytes)
            .map_err(|source| {
                file_database_close_io_failure(
                    DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                    FileDatabaseClosePublicationIoStage::ReadCandidate,
                    source,
                )
            })?;
        let after = candidate_file
            .metadata()
            .map_err(|source| {
                file_database_close_io_failure(
                    DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                    FileDatabaseClosePublicationIoStage::ReadCandidateMetadata,
                    source,
                )
            })?
            .len();
        if before != after {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationError::SelectedManifestLengthChanged { before, after },
            ));
        }
        let selected_manifest = decode_database_manifest_v2(&selected_bytes).map_err(|source| {
            DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationError::SelectedManifestDecode(source),
            )
        })?;
        if selected_manifest != target_manifest {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationError::SelectedManifestMismatch,
            ));
        }
        fault.after(FileDatabaseCloseBoundary::SelectedManifestVerification)?;

        let parent = match self.layout.manifest().parent() {
            Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
            Some(parent) => parent,
            None => {
                return Err(DatabaseCleanManifestPublisherFailure::new(
                    DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                    FileDatabaseClosePublicationError::ParentDirectoryUnavailable {
                        selected: self.layout.manifest().to_path_buf(),
                    },
                ));
            }
        };
        fault.before(FileDatabaseCloseBoundary::ParentDirectorySynchronization)?;
        let parent_directory = File::open(parent).map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationIoStage::OpenParentDirectory,
                source,
            )
        })?;
        parent_directory.sync_all().map_err(|source| {
            file_database_close_io_failure(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                FileDatabaseClosePublicationIoStage::SyncParentDirectory,
                source,
            )
        })?;
        fault.after(FileDatabaseCloseBoundary::ParentDirectorySynchronization)?;

        let selected_file = self._close_manifest_file.take().ok_or_else(|| {
            DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::TargetDurable,
                FileDatabaseClosePublicationError::CandidateDescriptorMissing,
            )
        })?;
        drop(std::mem::replace(&mut self._manifest_file, selected_file));
        self.manifest = target_manifest;
        Ok(permit.complete(selected_manifest, target_manifest))
    }
}

impl fmt::Debug for RecoveredFileDatabaseOuterOwnership {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveredFileDatabaseOuterOwnership")
            .field("manifest", &self.manifest)
            .field("layout", &self.layout)
            .field(
                "compatibility_target",
                self.compatibility_context.target_id(),
            )
            .finish_non_exhaustive()
    }
}

/// Terminal filesystem recovery attempt retaining every acquired lock.
#[must_use = "failed filesystem recovery retains all database and child locks"]
pub struct FailedFileDatabaseRecoveryAttempt<const N: usize> {
    _outer_owner: RecoveredFileDatabaseOuterOwnership,
    failure: FailedTransactionPageStorageRecoveryHandoff<
        FileCommitLog<N>,
        FilePageStore<N>,
        FileRestartCheckpointCompletenessBaselineSource,
        N,
    >,
}

impl<const N: usize> FailedFileDatabaseRecoveryAttempt<N> {
    /// Returns the first transaction recovery phase that did not complete.
    #[must_use]
    pub const fn phase(&self) -> TransactionPageStorageRecoveryHandoffPhase {
        self.failure.phase()
    }
}

impl<const N: usize> fmt::Debug for FailedFileDatabaseRecoveryAttempt<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FailedFileDatabaseRecoveryAttempt")
            .field("phase", &self.phase())
            .finish_non_exhaustive()
    }
}

/// Filesystem open boundary crossed after one complete owning phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDatabaseOpenPhase {
    /// Database-wide, manifest, WAL, page, and checkpoint validation completed.
    CompositionValidated,
    /// One transaction recovery handoff phase completed.
    Recovery(TransactionPageStorageRecoveryHandoffPhase),
    /// The database domain accepted exact completion evidence and released Live.
    LiveReleased,
}

/// Observer input accepted only by the filesystem recovery-owner implementation.
pub struct FileDatabaseRecoveryInput<'observer, Observer> {
    compatibility_context: CompatibilityContext,
    observer: &'observer mut Observer,
}

impl<const N: usize, Observer> DatabaseRecoveryOwner<FileDatabaseRecoveryInput<'_, Observer>, N>
    for FileDatabaseOwnership<N>
where
    Observer: FnMut(FileDatabaseOpenPhase),
{
    type Source = FileCommitLog<N>;
    type Store = FilePageStore<N>;
    type CheckpointSource = FileRestartCheckpointCompletenessBaselineSource;
    type RetainedOwner = RecoveredFileDatabaseOuterOwnership;
    type Failure = FailedFileDatabaseRecoveryAttempt<N>;

    fn complete_database_recovery(
        self,
        input: FileDatabaseRecoveryInput<'_, Observer>,
    ) -> Result<
        (
            Self::RetainedOwner,
            WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
                Self::Source,
                Self::Store,
                Self::CheckpointSource,
                N,
            >,
        ),
        Self::Failure,
    > {
        let Self {
            composition,
            _manifest_file,
            _database_owner_file,
            manifest,
            layout,
        } = self;
        let FileDatabaseRecoveryInput {
            compatibility_context,
            observer,
        } = input;
        let outer_owner = RecoveredFileDatabaseOuterOwnership {
            _manifest_file,
            _close_manifest_file: None,
            _database_owner_file,
            manifest,
            layout,
            compatibility_context,
        };
        let selection = composition.select_restart_checkpoint_completeness();
        match complete_transaction_page_storage_recovery_handoff_with_observer(selection, |phase| {
            observer(FileDatabaseOpenPhase::Recovery(phase))
        }) {
            Ok(transaction) => Ok((outer_owner, transaction)),
            Err(failure) => Err(FailedFileDatabaseRecoveryAttempt {
                _outer_owner: outer_owner,
                failure,
            }),
        }
    }
}

type LiveFileDatabaseDomainOwner<const N: usize> = RecoveredDatabaseOwnership<
    RecoveredFileDatabaseOuterOwnership,
    FileCommitLog<N>,
    FilePageStore<N>,
    FileRestartCheckpointCompletenessBaselineSource,
    N,
>;

type FailedFileDatabaseDomainRecovery<const N: usize> = FailedDatabaseRecovery<
    FailedFileDatabaseRecoveryAttempt<N>,
    RecoveredFileDatabaseOuterOwnership,
    FileCommitLog<N>,
    FilePageStore<N>,
    FileRestartCheckpointCompletenessBaselineSource,
    N,
>;

/// Recovery-complete filesystem database owner with one exact target context.
#[must_use = "live filesystem database must be closed, abandoned, or dropped"]
pub struct LiveFileDatabase<const N: usize> {
    database: LiveDatabase<LiveFileDatabaseDomainOwner<N>>,
}

impl<const N: usize> LiveFileDatabase<N> {
    /// Returns the exact selected database composition identity.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.database.identity()
    }

    /// Returns the database lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        self.database.stage()
    }

    /// Returns the one immutable exact-target context moved through open.
    #[must_use]
    pub const fn compatibility_context(&self) -> &CompatibilityContext {
        self.database.owner().outer_owner().compatibility_context()
    }

    /// Returns the exact manifest retained under the database-wide locks.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.database.owner().outer_owner().manifest()
    }

    /// Borrows the recovered coordinator, WAL, and page store.
    #[must_use]
    pub const fn transaction_parts(
        &self,
    ) -> (
        &TransactionCoordinator,
        &FileCommitLog<N>,
        &FilePageStore<N>,
    ) {
        self.database.owner().transaction().parts()
    }

    /// Borrows the recovered coordinator, WAL, and page store for live work.
    pub const fn transaction_parts_mut(
        &mut self,
    ) -> (
        &mut TransactionCoordinator,
        &mut FileCommitLog<N>,
        &mut FilePageStore<N>,
    ) {
        self.database.owner_mut().transaction_mut().parts_mut()
    }

    /// Borrows the exact completion and WAL-retention handoff owner.
    pub const fn recovery_handoff(
        &self,
    ) -> &WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
        FileCommitLog<N>,
        FilePageStore<N>,
        FileRestartCheckpointCompletenessBaselineSource,
        N,
    > {
        self.database.owner().transaction()
    }

    /// Consumes Live and binds fresh transaction close evidence to this database.
    pub fn prepare_close(
        self,
    ) -> Result<ClosePendingFileDatabase<N>, FailedFileDatabaseClosePreparation<N>> {
        self.database
            .prepare_close()
            .map(|database| ClosePendingFileDatabase { database })
    }

    /// Relinquishes live ownership without publishing any clean state.
    pub fn abandon(self) -> AbandonedDatabase {
        self.database.abandon()
    }
}

impl<const N: usize> fmt::Debug for LiveFileDatabase<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveFileDatabase")
            .field("identity", &self.identity())
            .field(
                "compatibility_target",
                self.compatibility_context().target_id(),
            )
            .finish_non_exhaustive()
    }
}

type PreparedFileDatabaseCloseOwnership<const N: usize> = PreparedDatabaseCloseOwnership<
    RecoveredFileDatabaseOuterOwnership,
    FileCommitLog<N>,
    FilePageStore<N>,
    FileRestartCheckpointCompletenessBaselineSource,
    N,
>;

type PublishedFileDatabaseCloseOwnership<const N: usize> = PublishedDatabaseCloseOwnership<
    RecoveredFileDatabaseOuterOwnership,
    FileCommitLog<N>,
    FilePageStore<N>,
    FileRestartCheckpointCompletenessBaselineSource,
    N,
>;

/// Terminal filesystem owner retained when close preparation fails.
pub type FailedFileDatabaseClosePreparation<const N: usize> = FailedDatabaseClosePreparation<
    RecoveredFileDatabaseOuterOwnership,
    FileCommitLog<N>,
    FilePageStore<N>,
    FileRestartCheckpointCompletenessBaselineSource,
    N,
>;

/// Terminal filesystem owner retained when manifest publication fails.
pub type FailedFileDatabaseClosePublication<const N: usize> = FailedDatabaseClosePublication<
    RecoveredFileDatabaseOuterOwnership,
    FileCommitLog<N>,
    FilePageStore<N>,
    FileRestartCheckpointCompletenessBaselineSource,
    FileDatabaseClosePublicationError,
    N,
>;

/// Filesystem database whose exact clean manifest awaits publication.
#[must_use = "close-pending filesystem database must publish or be explicitly abandoned"]
pub struct ClosePendingFileDatabase<const N: usize> {
    database: ClosePendingDatabase<PreparedFileDatabaseCloseOwnership<N>>,
}

impl<const N: usize> ClosePendingFileDatabase<N> {
    /// Returns the recovery-required source composition.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.database.identity()
    }

    /// Returns the adjacent clean composition targeted by publication.
    #[must_use]
    pub const fn target_identity(&self) -> DatabaseCompositionIdentity {
        self.database.prepared().target_identity()
    }

    /// Returns the exact adjacent clean target manifest.
    #[must_use]
    pub const fn target_manifest(&self) -> DatabaseManifest {
        self.database.prepared().target_manifest()
    }

    /// Returns the database lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        self.database.stage()
    }

    /// Publishes and synchronizes the exact clean manifest.
    pub fn close(self) -> Result<ClosedFileDatabase<N>, FailedFileDatabaseClosePublication<N>> {
        self.database
            .close(None)
            .map(|database| ClosedFileDatabase { database })
    }

    /// Publishes with one deterministic filesystem fault.
    pub fn close_with_fault(
        self,
        fault: FileDatabaseCloseFault,
    ) -> Result<ClosedFileDatabase<N>, FailedFileDatabaseClosePublication<N>> {
        self.database
            .close(Some(fault))
            .map(|database| ClosedFileDatabase { database })
    }

    /// Relinquishes close-pending ownership without manifest publication.
    pub fn abandon(self) -> AbandonedDatabase {
        self.database.abandon()
    }
}

impl<const N: usize> fmt::Debug for ClosePendingFileDatabase<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClosePendingFileDatabase")
            .field("identity", &self.identity())
            .field("target_identity", &self.target_identity())
            .finish_non_exhaustive()
    }
}

/// Filesystem database retained after exact clean-manifest durability.
#[must_use = "closed filesystem database ownership must remain retained or be dropped"]
pub struct ClosedFileDatabase<const N: usize> {
    database: ClosedDatabase<PublishedFileDatabaseCloseOwnership<N>>,
}

impl<const N: usize> ClosedFileDatabase<N> {
    /// Returns the exact selected clean composition.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.database.identity()
    }

    /// Returns the exact selected and synchronized clean manifest.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.database.published().manifest()
    }

    /// Returns the retained selected layout.
    #[must_use]
    pub const fn layout(&self) -> &FileDatabaseLayout {
        self.database.published().prepared().outer_owner().layout()
    }

    /// Returns the immutable exact-target context.
    #[must_use]
    pub const fn compatibility_context(&self) -> &CompatibilityContext {
        self.database
            .published()
            .prepared()
            .outer_owner()
            .compatibility_context()
    }

    /// Returns the database lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        self.database.stage()
    }
}

impl<const N: usize> fmt::Debug for ClosedFileDatabase<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClosedFileDatabase")
            .field("identity", &self.identity())
            .field("manifest", &self.manifest())
            .finish_non_exhaustive()
    }
}

/// Terminal inert result after relinquishing a failed filesystem publication.
pub type AbandonedFileDatabaseClosePublication = AbandonedDatabaseClosePublication;

/// Failure before or during fail-closed filesystem database open.
#[must_use = "failed filesystem database open may retain every database owner"]
pub enum FileDatabaseLiveOpenError<const N: usize> {
    /// Database-wide ownership or structural composition validation failed.
    Ownership(FileDatabaseOwnershipOpenError),
    /// Transaction recovery or exact completion-evidence binding failed.
    Recovery(FailedFileDatabaseDomainRecovery<N>),
}

impl<const N: usize> FileDatabaseLiveOpenError<N> {
    /// Returns the transaction recovery phase for an adapter-operation failure.
    #[must_use]
    pub const fn recovery_phase(&self) -> Option<TransactionPageStorageRecoveryHandoffPhase> {
        match self {
            Self::Ownership(_) => None,
            Self::Recovery(failure) => match failure.cause() {
                DatabaseRecoveryFailureCause::Operation(failure) => Some(failure.phase()),
                DatabaseRecoveryFailureCause::Evidence(_) => None,
            },
        }
    }
}

impl<const N: usize> fmt::Debug for FileDatabaseLiveOpenError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ownership(error) => formatter
                .debug_tuple("FileDatabaseLiveOpenError::Ownership")
                .field(error)
                .finish(),
            Self::Recovery(error) => formatter
                .debug_tuple("FileDatabaseLiveOpenError::Recovery")
                .field(error)
                .finish(),
        }
    }
}

impl<const N: usize> fmt::Display for FileDatabaseLiveOpenError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ownership(error) => write!(formatter, "filesystem database open failed: {error}"),
            Self::Recovery(error) => match error.cause() {
                DatabaseRecoveryFailureCause::Operation(failure) => write!(
                    formatter,
                    "filesystem database recovery failed before completing {:?}",
                    failure.phase()
                ),
                DatabaseRecoveryFailureCause::Evidence(error) => {
                    write!(
                        formatter,
                        "filesystem database recovery evidence failed: {error}"
                    )
                }
            },
        }
    }
}

impl<const N: usize> Error for FileDatabaseLiveOpenError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ownership(error) => Some(error),
            Self::Recovery(_) => None,
        }
    }
}

/// Manifest-selected filesystem ownership retaining every acquired lock.
///
/// This legacy-compatible wrapper deliberately withholds stable-storage binding.
/// The successor-only opener performs that gate before returning a distinct
/// recovery-required type.
///
/// ```compile_fail
/// use ntsql_storage_file::FileDatabaseOwnershipSelection;
///
/// fn cannot_claim_exact<const N: usize>(selected: FileDatabaseOwnershipSelection<N>) {
///     let observed = selected.identity().storage_identity();
///     selected.bind_observed_storage(observed);
/// }
/// ```
#[must_use = "selected filesystem ownership must remain retained or be dropped"]
pub struct FileDatabaseOwnershipSelection<const N: usize> {
    selected: ManifestSelectedDatabase<FileDatabaseOwnership<N>>,
}

impl<const N: usize> FileDatabaseOwnershipSelection<N> {
    /// Returns the selected inert manifest identity.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.selected.identity()
    }

    /// Returns the strongest lifecycle stage established by current headers.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        DatabaseLifecycleStage::ManifestSelected
    }
}

impl<const N: usize> fmt::Debug for FileDatabaseOwnershipSelection<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileDatabaseOwnershipSelection")
            .field("identity", &self.selected.identity())
            .finish_non_exhaustive()
    }
}

/// Recovery-required filesystem ownership proven by stable physical child identity.
pub type RecoveryRequiredFileDatabase<const N: usize> =
    RecoveryRequiredDatabase<FileDatabaseOwnership<N>>;

struct AcquiredFileDatabaseOwnership<const N: usize> {
    selected: ManifestSelectedDatabase<FileDatabaseOwnership<N>>,
    stable_storage_observation: StableStorageObservation,
}

#[derive(Clone, Copy)]
enum StableStorageObservation {
    Complete(DatabaseStorageIdentity),
    Missing(DatabaseFileRole),
}

/// Acquires one database lock set and validates all currently physical evidence.
///
/// Acquisition is nonblocking and ordered:
///
/// 1. stable database owner;
/// 2. selected manifest;
/// 3. WAL;
/// 4. page store; and
/// 5. restart-checkpoint completeness control.
///
/// Successful return retains every lock inside a manifest-selected adapter
/// owner. Only after all child evidence passes may finalization apply each
/// existing adapter's reviewed incomplete-tail repair. This operation performs
/// no database creation, manifest publication, exact-composition promotion,
/// database recovery, live release, close, or drop.
pub fn open_file_database_ownership<const N: usize>(
    expected_database_id: DatabaseId,
    layout: FileDatabaseLayout,
) -> Result<FileDatabaseOwnershipSelection<N>, FileDatabaseOwnershipOpenError> {
    let acquired = acquire_file_database_ownership(expected_database_id, layout, false, None)?;
    Ok(FileDatabaseOwnershipSelection {
        selected: acquired.selected,
    })
}

/// Opens only successor children that physically prove the selected storage identity.
///
/// Legacy child formats remain openable through [`open_file_database_ownership`]
/// but cannot cross this recovery-required authority boundary.
pub fn open_recovery_required_file_database<const N: usize>(
    expected_database_id: DatabaseId,
    layout: FileDatabaseLayout,
) -> Result<RecoveryRequiredFileDatabase<N>, FileDatabaseOwnershipOpenError> {
    open_recovery_required_file_database_with_checkpoint_fault(expected_database_id, layout, None)
}

fn open_recovery_required_file_database_with_checkpoint_fault<const N: usize>(
    expected_database_id: DatabaseId,
    layout: FileDatabaseLayout,
    checkpoint_fault: Option<FileCleanCloseCheckpointFaultPoint>,
) -> Result<RecoveryRequiredFileDatabase<N>, FileDatabaseOwnershipOpenError> {
    let acquired =
        acquire_file_database_ownership(expected_database_id, layout, true, checkpoint_fault)?;
    let observed_storage_identity = match acquired.stable_storage_observation {
        StableStorageObservation::Complete(identity) => identity,
        StableStorageObservation::Missing(role) => {
            return Err(FileDatabaseOwnershipOpenError::StableStorageIdentityUnavailable { role });
        }
    };
    acquired
        .selected
        .bind_observed_storage(observed_storage_identity)
        .map_err(|failure| FileDatabaseOwnershipOpenError::StorageBinding(*failure.reason()))
}

/// Opens one exact filesystem composition through recovery into Live.
pub fn open_live_file_database<const N: usize>(
    expected_database_id: DatabaseId,
    layout: FileDatabaseLayout,
    compatibility_context: CompatibilityContext,
) -> Result<LiveFileDatabase<N>, FileDatabaseLiveOpenError<N>> {
    open_live_file_database_with_observer(
        expected_database_id,
        layout,
        compatibility_context,
        |_| {},
    )
}

/// Opens through recovery with one deterministic clean-close checkpoint fault.
pub fn open_live_file_database_with_close_checkpoint_fault<const N: usize>(
    expected_database_id: DatabaseId,
    layout: FileDatabaseLayout,
    compatibility_context: CompatibilityContext,
    checkpoint_fault: FileCleanCloseCheckpointFaultPoint,
) -> Result<LiveFileDatabase<N>, FileDatabaseLiveOpenError<N>> {
    let recovery_required = open_recovery_required_file_database_with_checkpoint_fault(
        expected_database_id,
        layout,
        Some(checkpoint_fault),
    )
    .map_err(FileDatabaseLiveOpenError::Ownership)?;
    let mut observer = |_| {};
    let database = recovery_required
        .complete_recovery::<_, N>(FileDatabaseRecoveryInput {
            compatibility_context,
            observer: &mut observer,
        })
        .map_err(FileDatabaseLiveOpenError::Recovery)?;
    Ok(LiveFileDatabase { database })
}

/// Opens through recovery while reporting each completed owning phase.
///
/// The observer receives inert phase values only. A process-exit test may
/// terminate after any callback without obtaining an adapter or live owner.
pub fn open_live_file_database_with_observer<const N: usize, Observer>(
    expected_database_id: DatabaseId,
    layout: FileDatabaseLayout,
    compatibility_context: CompatibilityContext,
    mut observer: Observer,
) -> Result<LiveFileDatabase<N>, FileDatabaseLiveOpenError<N>>
where
    Observer: FnMut(FileDatabaseOpenPhase),
{
    let recovery_required = open_recovery_required_file_database(expected_database_id, layout)
        .map_err(FileDatabaseLiveOpenError::Ownership)?;
    observer(FileDatabaseOpenPhase::CompositionValidated);
    let database = recovery_required
        .complete_recovery::<_, N>(FileDatabaseRecoveryInput {
            compatibility_context,
            observer: &mut observer,
        })
        .map_err(FileDatabaseLiveOpenError::Recovery)?;
    observer(FileDatabaseOpenPhase::LiveReleased);
    Ok(LiveFileDatabase { database })
}

/// Derives the sibling file reserved for one clean-manifest publication.
#[must_use]
pub fn database_manifest_close_candidate_path(selected_manifest: &Path) -> Option<PathBuf> {
    suffixed_sibling_path(selected_manifest, ".close-candidate")
}

fn suffixed_sibling_path(path: &Path, suffix: &str) -> Option<PathBuf> {
    let file_name = path.file_name()?;
    let mut candidate_name = OsString::from(file_name);
    candidate_name.push(suffix);
    Some(path.with_file_name(candidate_name))
}

fn validate_close_candidate_paths(
    layout: &FileDatabaseLayout,
) -> Result<(), FileDatabaseOwnershipOpenError> {
    let derive = |path: &Path, suffix: &str, entry| {
        suffixed_sibling_path(path, suffix)
            .ok_or(FileDatabaseOwnershipOpenError::CloseCandidatePathUnavailable { entry })
    };
    let manifest_create = derive(
        layout.manifest(),
        ".create-candidate",
        FileDatabaseCreateEntry::ManifestCandidate,
    )?;
    let wal_create = derive(
        layout.wal(),
        ".create-candidate",
        FileDatabaseCreateEntry::WalCandidate,
    )?;
    let page_store_create = derive(
        layout.page_store(),
        ".create-candidate",
        FileDatabaseCreateEntry::PageStoreCandidate,
    )?;
    let restart_checkpoint_create = derive(
        layout.restart_checkpoint(),
        ".create-candidate",
        FileDatabaseCreateEntry::RestartCheckpointCandidate,
    )?;
    let manifest_close = database_manifest_close_candidate_path(layout.manifest()).ok_or(
        FileDatabaseOwnershipOpenError::CloseCandidatePathUnavailable {
            entry: FileDatabaseCreateEntry::ManifestCloseCandidate,
        },
    )?;
    let restart_checkpoint_clean =
        clean_close_checkpoint_slot_directory(layout.restart_checkpoint()).ok_or(
            FileDatabaseOwnershipOpenError::CloseCandidatePathUnavailable {
                entry: FileDatabaseCreateEntry::RestartCheckpointCleanCloseCandidate,
            },
        )?;
    let wal_reclamation = super::reclamation_candidate_path(layout.wal()).ok_or(
        FileDatabaseOwnershipOpenError::CloseCandidatePathUnavailable {
            entry: FileDatabaseCreateEntry::WalReclamationCandidate,
        },
    )?;
    let wal_create_reclamation = super::reclamation_candidate_path(&wal_create).ok_or(
        FileDatabaseOwnershipOpenError::CloseCandidatePathUnavailable {
            entry: FileDatabaseCreateEntry::WalCandidateReclamationCandidate,
        },
    )?;
    let paths = [
        (
            FileDatabaseCreateEntry::DatabaseOwner,
            layout.database_owner().to_path_buf(),
        ),
        (
            FileDatabaseCreateEntry::Manifest,
            layout.manifest().to_path_buf(),
        ),
        (FileDatabaseCreateEntry::ManifestCandidate, manifest_create),
        (
            FileDatabaseCreateEntry::ManifestCloseCandidate,
            manifest_close,
        ),
        (FileDatabaseCreateEntry::Wal, layout.wal().to_path_buf()),
        (FileDatabaseCreateEntry::WalCandidate, wal_create),
        (
            FileDatabaseCreateEntry::WalReclamationCandidate,
            wal_reclamation,
        ),
        (
            FileDatabaseCreateEntry::WalCandidateReclamationCandidate,
            wal_create_reclamation,
        ),
        (
            FileDatabaseCreateEntry::PageStore,
            layout.page_store().to_path_buf(),
        ),
        (
            FileDatabaseCreateEntry::PageStoreCandidate,
            page_store_create,
        ),
        (
            FileDatabaseCreateEntry::RestartCheckpoint,
            layout.restart_checkpoint().to_path_buf(),
        ),
        (
            FileDatabaseCreateEntry::RestartCheckpointControl,
            layout.restart_checkpoint().join(CONTROL_FILE_NAME),
        ),
        (
            FileDatabaseCreateEntry::RestartCheckpointCandidate,
            restart_checkpoint_create.clone(),
        ),
        (
            FileDatabaseCreateEntry::RestartCheckpointCandidateControl,
            restart_checkpoint_create.join(CONTROL_FILE_NAME),
        ),
        (
            FileDatabaseCreateEntry::RestartCheckpointCleanCloseCandidate,
            restart_checkpoint_clean.clone(),
        ),
        (
            FileDatabaseCreateEntry::RestartCheckpointCleanCloseControl,
            restart_checkpoint_clean.join(CONTROL_FILE_NAME),
        ),
    ];
    for (index, (second_entry, second_path)) in paths.iter().enumerate() {
        for (first_entry, first_path) in &paths[..index] {
            let is_close_entry = |entry| {
                matches!(
                    entry,
                    FileDatabaseCreateEntry::ManifestCloseCandidate
                        | FileDatabaseCreateEntry::RestartCheckpointCleanCloseCandidate
                        | FileDatabaseCreateEntry::RestartCheckpointCleanCloseControl
                )
            };
            if !is_close_entry(*first_entry) && !is_close_entry(*second_entry) {
                continue;
            }
            if first_path == second_path {
                return Err(
                    FileDatabaseOwnershipOpenError::CloseCandidatePathCollision {
                        first: *first_entry,
                        second: *second_entry,
                    },
                );
            }
        }
    }
    Ok(())
}

fn acquire_file_database_ownership<const N: usize>(
    expected_database_id: DatabaseId,
    layout: FileDatabaseLayout,
    require_stable_storage: bool,
    clean_close_checkpoint_fault: Option<FileCleanCloseCheckpointFaultPoint>,
) -> Result<AcquiredFileDatabaseOwnership<N>, FileDatabaseOwnershipOpenError> {
    PageLayout::for_const::<N>().map_err(FileDatabaseOwnershipOpenError::PageWidth)?;
    validate_close_candidate_paths(&layout)?;

    let (mut database_owner_file, database_owner_metadata) =
        open_file(layout.database_owner(), FileDatabaseLockRole::DatabaseOwner)?;
    lock_file(&database_owner_file, FileDatabaseLockRole::DatabaseOwner)?;
    let owner_database_id = read_database_owner_control(&mut database_owner_file)?;
    if owner_database_id != expected_database_id {
        return Err(FileDatabaseOwnershipOpenError::DatabaseOwnerIdMismatch {
            expected: expected_database_id,
            actual: owner_database_id,
        });
    }

    let (mut manifest_file, manifest_metadata) =
        open_file(layout.manifest(), FileDatabaseLockRole::Manifest)?;
    reject_alias(
        FileDatabaseLockRole::DatabaseOwner,
        &database_owner_metadata,
        FileDatabaseLockRole::Manifest,
        &manifest_metadata,
    )?;
    lock_file(&manifest_file, FileDatabaseLockRole::Manifest)?;
    let manifest = read_manifest(&mut manifest_file)?;
    let manifest_database_id = manifest.composition_identity().database_id();
    if manifest_database_id != owner_database_id {
        return Err(FileDatabaseOwnershipOpenError::ManifestDatabaseIdMismatch {
            owner: owner_database_id,
            manifest: manifest_database_id,
        });
    }
    if require_stable_storage
        && !matches!(
            manifest.lifecycle_state(),
            ntsql_database::DatabaseManifestLifecycleState::RecoveryRequired
        )
    {
        return Err(FileDatabaseOwnershipOpenError::ManifestLifecycle {
            actual: manifest.lifecycle_state(),
        });
    }

    let wal_probe_metadata = {
        let (_wal_probe, wal_metadata) = open_file(layout.wal(), FileDatabaseLockRole::Wal)?;
        reject_alias(
            FileDatabaseLockRole::DatabaseOwner,
            &database_owner_metadata,
            FileDatabaseLockRole::Wal,
            &wal_metadata,
        )?;
        reject_alias(
            FileDatabaseLockRole::Manifest,
            &manifest_metadata,
            FileDatabaseLockRole::Wal,
            &wal_metadata,
        )?;
        wal_metadata
    };

    let page_store_probe_metadata = {
        let (_page_store_probe, page_store_metadata) =
            open_file(layout.page_store(), FileDatabaseLockRole::PageStore)?;
        reject_alias(
            FileDatabaseLockRole::DatabaseOwner,
            &database_owner_metadata,
            FileDatabaseLockRole::PageStore,
            &page_store_metadata,
        )?;
        reject_alias(
            FileDatabaseLockRole::Manifest,
            &manifest_metadata,
            FileDatabaseLockRole::PageStore,
            &page_store_metadata,
        )?;
        reject_alias(
            FileDatabaseLockRole::Wal,
            &wal_probe_metadata,
            FileDatabaseLockRole::PageStore,
            &page_store_metadata,
        )?;
        page_store_metadata
    };

    let checkpoint_probe_metadata = {
        let checkpoint_control_path = layout.restart_checkpoint().join(CONTROL_FILE_NAME);
        let (_checkpoint_probe, checkpoint_metadata) = open_file(
            &checkpoint_control_path,
            FileDatabaseLockRole::RestartCheckpoint,
        )?;
        for (first, metadata) in [
            (
                FileDatabaseLockRole::DatabaseOwner,
                &database_owner_metadata,
            ),
            (FileDatabaseLockRole::Manifest, &manifest_metadata),
            (FileDatabaseLockRole::Wal, &wal_probe_metadata),
            (FileDatabaseLockRole::PageStore, &page_store_probe_metadata),
        ] {
            reject_alias(
                first,
                metadata,
                FileDatabaseLockRole::RestartCheckpoint,
                &checkpoint_metadata,
            )?;
        }
        checkpoint_metadata
    };

    reject_wal_reclamation_candidate_collision(
        &layout,
        &[
            (
                FileDatabaseLockRole::DatabaseOwner,
                &database_owner_metadata,
            ),
            (FileDatabaseLockRole::Manifest, &manifest_metadata),
            (FileDatabaseLockRole::Wal, &wal_probe_metadata),
            (FileDatabaseLockRole::PageStore, &page_store_probe_metadata),
            (
                FileDatabaseLockRole::RestartCheckpoint,
                &checkpoint_probe_metadata,
            ),
        ],
    )?;

    let pending_log = FileCommitLog::<N>::inspect_transaction_page_capable(layout.wal())
        .map_err(FileDatabaseOwnershipOpenError::WalOpen)?;
    require_same_opened_object(
        FileDatabaseLockRole::Wal,
        &wal_probe_metadata,
        &pending_log.metadata().map_err(|source| {
            FileDatabaseOwnershipIoError::new(
                FileDatabaseOwnershipIoStage::ReadMetadata {
                    role: FileDatabaseLockRole::Wal,
                },
                source,
            )
        })?,
    )?;
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::Wal,
            format_version: pending_log.physical_format_version(),
            persistent_log_id: pending_log.persistent_id(),
            database_file_identity: pending_log.database_file_identity(),
        },
    )?;
    let wal_database_file_identity = pending_log.database_file_identity();
    let observed_persistent_log_id = pending_log.persistent_id();

    let pending_store = FilePageStore::<N>::inspect(layout.page_store())
        .map_err(FileDatabaseOwnershipOpenError::PageStoreOpen)?;
    require_same_opened_object(
        FileDatabaseLockRole::PageStore,
        &page_store_probe_metadata,
        &pending_store.metadata().map_err(|source| {
            FileDatabaseOwnershipIoError::new(
                FileDatabaseOwnershipIoStage::ReadMetadata {
                    role: FileDatabaseLockRole::PageStore,
                },
                source,
            )
        })?,
    )?;
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::PageStore,
            format_version: pending_store.physical_format_version(),
            persistent_log_id: pending_store.persistent_id(),
            database_file_identity: pending_store.database_file_identity(),
        },
    )?;
    let page_store_database_file_identity = pending_store.database_file_identity();

    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::open(layout.restart_checkpoint())
            .map_err(FileDatabaseOwnershipOpenError::RestartCheckpointOpen)?;
    if let Some(fault) = clean_close_checkpoint_fault {
        checkpoint
            .arm_clean_close_fault(fault)
            .map_err(FileDatabaseOwnershipOpenError::CleanCloseCheckpointFault)?;
    }
    require_same_opened_object(
        FileDatabaseLockRole::RestartCheckpoint,
        &checkpoint_probe_metadata,
        &checkpoint.control_metadata().map_err(|source| {
            FileDatabaseOwnershipIoError::new(
                FileDatabaseOwnershipIoStage::ReadMetadata {
                    role: FileDatabaseLockRole::RestartCheckpoint,
                },
                source,
            )
        })?,
    )?;
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::RestartCheckpoint,
            format_version: checkpoint.control_format_version(),
            persistent_log_id: checkpoint.persistent_log_id(),
            database_file_identity: checkpoint.database_file_identity(),
        },
    )?;
    let restart_checkpoint_database_file_identity = checkpoint.database_file_identity();
    let stable_storage_observation = match (
        wal_database_file_identity,
        page_store_database_file_identity,
        restart_checkpoint_database_file_identity,
    ) {
        (Some(wal), Some(page_store), Some(restart_checkpoint)) => {
            let files = [wal.file(), page_store.file(), restart_checkpoint.file()];
            let observed =
                DatabaseStorageIdentity::new(wal.database_id(), observed_persistent_log_id, &files)
                    .map_err(FileDatabaseOwnershipOpenError::ObservedStorageIdentity)?;
            StableStorageObservation::Complete(observed)
        }
        (None, _, _) => StableStorageObservation::Missing(DatabaseFileRole::Wal),
        (Some(_), None, _) => StableStorageObservation::Missing(DatabaseFileRole::PageStore),
        (Some(_), Some(_), None) => {
            StableStorageObservation::Missing(DatabaseFileRole::RestartCheckpoint)
        }
    };
    if require_stable_storage
        && let StableStorageObservation::Missing(role) = stable_storage_observation
    {
        return Err(FileDatabaseOwnershipOpenError::StableStorageIdentityUnavailable { role });
    }
    sync_selected_manifest_publication(&manifest_file, layout.manifest())?;

    let log = pending_log
        .finish()
        .map_err(FileDatabaseOwnershipOpenError::WalOpen)?;
    let store = pending_store
        .finish()
        .map_err(FileDatabaseOwnershipOpenError::PageStoreOpen)?;
    let composition =
        UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint::from_locked_parts(
            log, store, checkpoint,
        );

    let selected_identity = manifest.composition_identity();
    let owner = FileDatabaseOwnership {
        composition,
        _manifest_file: manifest_file,
        _database_owner_file: database_owner_file,
        manifest,
        layout,
    };
    let selected = match UnboundDatabase::new(owner, expected_database_id)
        .select_manifest(selected_identity)
    {
        Ok(selected) => selected,
        Err(failure) => {
            let reason = *failure.reason();
            drop(failure);
            return Err(FileDatabaseOwnershipOpenError::ManifestSelection(reason));
        }
    };
    Ok(AcquiredFileDatabaseOwnership {
        selected,
        stable_storage_observation,
    })
}

#[derive(Clone, Copy)]
struct ChildObservation {
    role: DatabaseFileRole,
    format_version: u16,
    persistent_log_id: PersistentLogId,
    database_file_identity: Option<DatabaseFileHeaderIdentity>,
}

fn open_file(
    path: &Path,
    role: FileDatabaseLockRole,
) -> Result<(File, Metadata), FileDatabaseOwnershipIoError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| {
            FileDatabaseOwnershipIoError::new(
                FileDatabaseOwnershipIoStage::OpenFile { role },
                source,
            )
        })?;
    let metadata = file.metadata().map_err(|source| {
        FileDatabaseOwnershipIoError::new(
            FileDatabaseOwnershipIoStage::ReadMetadata { role },
            source,
        )
    })?;
    Ok((file, metadata))
}

fn sync_selected_manifest_publication(
    manifest_file: &File,
    manifest_path: &Path,
) -> Result<(), FileDatabaseOwnershipIoError> {
    let role = FileDatabaseLockRole::Manifest;
    manifest_file.sync_all().map_err(|source| {
        FileDatabaseOwnershipIoError::new(FileDatabaseOwnershipIoStage::SyncFile { role }, source)
    })?;
    let parent = match manifest_path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => {
            return Err(FileDatabaseOwnershipIoError::new(
                FileDatabaseOwnershipIoStage::OpenParentDirectory { role },
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "database manifest path has no parent directory",
                ),
            ));
        }
    };
    let directory = File::open(parent).map_err(|source| {
        FileDatabaseOwnershipIoError::new(
            FileDatabaseOwnershipIoStage::OpenParentDirectory { role },
            source,
        )
    })?;
    directory.sync_all().map_err(|source| {
        FileDatabaseOwnershipIoError::new(
            FileDatabaseOwnershipIoStage::SyncParentDirectory { role },
            source,
        )
    })
}

fn lock_file(file: &File, role: FileDatabaseLockRole) -> Result<(), FileDatabaseOwnershipIoError> {
    file.try_lock().map_err(|source| {
        FileDatabaseOwnershipIoError::new(
            FileDatabaseOwnershipIoStage::AcquireExclusiveLock { role },
            source.into(),
        )
    })
}

fn read_exact(
    file: &mut File,
    role: FileDatabaseLockRole,
    bytes: &mut [u8],
) -> Result<(), FileDatabaseOwnershipIoError> {
    file.read_exact(bytes).map_err(|source| {
        FileDatabaseOwnershipIoError::new(FileDatabaseOwnershipIoStage::ReadHeader { role }, source)
    })
}

fn read_database_owner_control(
    file: &mut File,
) -> Result<DatabaseId, FileDatabaseOwnershipOpenError> {
    let length = file.metadata().map_err(|source| {
        FileDatabaseOwnershipIoError::new(
            FileDatabaseOwnershipIoStage::ReadMetadata {
                role: FileDatabaseLockRole::DatabaseOwner,
            },
            source,
        )
    })?;
    if length.len() != DATABASE_OWNER_CONTROL_V1_LENGTH as u64 {
        return Err(
            FileDatabaseOwnershipOpenError::DatabaseOwnerControlFileLength {
                actual: length.len(),
            },
        );
    }
    let mut bytes = [0_u8; DATABASE_OWNER_CONTROL_V1_LENGTH];
    read_exact(file, FileDatabaseLockRole::DatabaseOwner, &mut bytes)?;
    decode_database_owner_control(&bytes)
        .map_err(FileDatabaseOwnershipOpenError::DatabaseOwnerControl)
}

fn read_manifest(file: &mut File) -> Result<DatabaseManifest, FileDatabaseOwnershipOpenError> {
    let length = file.metadata().map_err(|source| {
        FileDatabaseOwnershipIoError::new(
            FileDatabaseOwnershipIoStage::ReadMetadata {
                role: FileDatabaseLockRole::Manifest,
            },
            source,
        )
    })?;
    match usize::try_from(length.len()) {
        Ok(super::DATABASE_MANIFEST_V1_LENGTH) => {
            let mut bytes = [0_u8; super::DATABASE_MANIFEST_V1_LENGTH];
            read_exact(file, FileDatabaseLockRole::Manifest, &mut bytes)?;
            decode_database_manifest(&bytes).map_err(FileDatabaseOwnershipOpenError::Manifest)
        }
        Ok(super::DATABASE_MANIFEST_V2_LENGTH) => {
            let mut bytes = [0_u8; super::DATABASE_MANIFEST_V2_LENGTH];
            read_exact(file, FileDatabaseLockRole::Manifest, &mut bytes)?;
            decode_database_manifest_v2(&bytes).map_err(FileDatabaseOwnershipOpenError::ManifestV2)
        }
        Ok(_) | Err(_) => Err(FileDatabaseOwnershipOpenError::ManifestFileLength {
            actual: length.len(),
        }),
    }
}

fn validate_child_observation(
    manifest: DatabaseManifest,
    observation: ChildObservation,
) -> Result<(), FileDatabaseOwnershipOpenError> {
    let required = manifest.storage_formats().version(observation.role);
    if required.get() != observation.format_version {
        return Err(
            FileDatabaseOwnershipOpenError::StorageFormatVersionMismatch {
                role: observation.role,
                required,
                actual: observation.format_version,
            },
        );
    }
    let expected = manifest.composition_identity().persistent_log_id();
    if expected != observation.persistent_log_id {
        return Err(FileDatabaseOwnershipOpenError::PersistentLogIdMismatch {
            role: observation.role,
            expected,
            actual: observation.persistent_log_id,
        });
    }
    if let Some(actual) = observation.database_file_identity {
        let expected = manifest
            .composition_identity()
            .file_header_identity(observation.role);
        if actual.database_id() != expected.database_id() {
            return Err(FileDatabaseOwnershipOpenError::ChildDatabaseIdMismatch {
                role: observation.role,
                expected: expected.database_id(),
                actual: actual.database_id(),
            });
        }
        if actual.file().role() != expected.file().role() {
            return Err(FileDatabaseOwnershipOpenError::ChildFileRoleMismatch {
                expected: expected.file().role(),
                actual: actual.file().role(),
            });
        }
        if actual.file().file_id() != expected.file().file_id() {
            return Err(FileDatabaseOwnershipOpenError::ChildFileIdMismatch {
                role: observation.role,
                expected: expected.file().file_id(),
                actual: actual.file().file_id(),
            });
        }
    }
    Ok(())
}

fn reject_wal_reclamation_candidate_collision(
    layout: &FileDatabaseLayout,
    protected: &[(FileDatabaseLockRole, &Metadata)],
) -> Result<(), FileDatabaseOwnershipOpenError> {
    let Some(candidate_path) = super::reclamation_candidate_path(layout.wal()) else {
        return Ok(());
    };
    let checkpoint_control = layout.restart_checkpoint().join(CONTROL_FILE_NAME);
    for (role, path) in [
        (FileDatabaseLockRole::DatabaseOwner, layout.database_owner()),
        (FileDatabaseLockRole::Manifest, layout.manifest()),
        (FileDatabaseLockRole::Wal, layout.wal()),
        (FileDatabaseLockRole::PageStore, layout.page_store()),
        (
            FileDatabaseLockRole::RestartCheckpoint,
            checkpoint_control.as_path(),
        ),
    ] {
        if candidate_path == path {
            return Err(FileDatabaseOwnershipOpenError::WalReclamationCandidateCollision { role });
        }
    }

    let candidate_metadata = match fs::metadata(&candidate_path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(FileDatabaseOwnershipIoError::new(
                FileDatabaseOwnershipIoStage::ReadWalReclamationCandidateMetadata,
                source,
            )
            .into());
        }
    };
    for (role, metadata) in protected {
        if metadata_identifies_same_file(metadata, &candidate_metadata) {
            return Err(
                FileDatabaseOwnershipOpenError::WalReclamationCandidateCollision { role: *role },
            );
        }
    }
    Ok(())
}

fn reject_alias(
    first: FileDatabaseLockRole,
    first_metadata: &Metadata,
    second: FileDatabaseLockRole,
    second_metadata: &Metadata,
) -> Result<(), FileDatabaseOwnershipOpenError> {
    if metadata_identifies_same_file(first_metadata, second_metadata) {
        return Err(FileDatabaseOwnershipOpenError::OpenedObjectAlias { first, second });
    }
    Ok(())
}

fn require_same_opened_object(
    role: FileDatabaseLockRole,
    probed: &Metadata,
    locked: &Metadata,
) -> Result<(), FileDatabaseOwnershipOpenError> {
    #[cfg(unix)]
    if !metadata_identifies_same_file(probed, locked) {
        return Err(FileDatabaseOwnershipOpenError::OpenedObjectChanged { role });
    }

    #[cfg(not(unix))]
    let _ = (role, probed, locked);

    Ok(())
}

#[derive(Debug)]
struct FileDatabaseCreatePaths {
    manifest_candidate: PathBuf,
    wal_candidate: PathBuf,
    page_store_candidate: PathBuf,
    restart_checkpoint_candidate: PathBuf,
    restart_checkpoint_control: PathBuf,
    restart_checkpoint_candidate_control: PathBuf,
    wal_reclamation_candidate: PathBuf,
    wal_candidate_reclamation_candidate: PathBuf,
}

impl FileDatabaseCreatePaths {
    fn derive(layout: &FileDatabaseLayout) -> Result<Self, FileDatabaseCreateError> {
        let manifest_candidate =
            create_candidate_path(layout.manifest(), FileDatabaseCreateEntry::Manifest)?;
        let wal_candidate = create_candidate_path(layout.wal(), FileDatabaseCreateEntry::Wal)?;
        let page_store_candidate =
            create_candidate_path(layout.page_store(), FileDatabaseCreateEntry::PageStore)?;
        let restart_checkpoint_candidate = create_candidate_path(
            layout.restart_checkpoint(),
            FileDatabaseCreateEntry::RestartCheckpoint,
        )?;
        let wal_reclamation_candidate = super::reclamation_candidate_path(layout.wal()).ok_or(
            FileDatabaseCreateError::CandidatePathUnavailable {
                entry: FileDatabaseCreateEntry::WalReclamationCandidate,
            },
        )?;
        let wal_candidate_reclamation_candidate = super::reclamation_candidate_path(&wal_candidate)
            .ok_or(FileDatabaseCreateError::CandidatePathUnavailable {
                entry: FileDatabaseCreateEntry::WalCandidateReclamationCandidate,
            })?;
        let paths = Self {
            restart_checkpoint_control: layout.restart_checkpoint().join(CONTROL_FILE_NAME),
            restart_checkpoint_candidate_control: restart_checkpoint_candidate
                .join(CONTROL_FILE_NAME),
            manifest_candidate,
            wal_candidate,
            page_store_candidate,
            restart_checkpoint_candidate,
            wal_reclamation_candidate,
            wal_candidate_reclamation_candidate,
        };
        paths.reject_lexical_collisions(layout)?;
        Ok(paths)
    }

    fn reject_lexical_collisions(
        &self,
        layout: &FileDatabaseLayout,
    ) -> Result<(), FileDatabaseCreateError> {
        let paths = [
            (
                FileDatabaseCreateEntry::DatabaseOwner,
                layout.database_owner(),
            ),
            (FileDatabaseCreateEntry::Manifest, layout.manifest()),
            (
                FileDatabaseCreateEntry::ManifestCandidate,
                self.manifest_candidate.as_path(),
            ),
            (FileDatabaseCreateEntry::Wal, layout.wal()),
            (
                FileDatabaseCreateEntry::WalCandidate,
                self.wal_candidate.as_path(),
            ),
            (FileDatabaseCreateEntry::PageStore, layout.page_store()),
            (
                FileDatabaseCreateEntry::PageStoreCandidate,
                self.page_store_candidate.as_path(),
            ),
            (
                FileDatabaseCreateEntry::RestartCheckpoint,
                layout.restart_checkpoint(),
            ),
            (
                FileDatabaseCreateEntry::RestartCheckpointCandidate,
                self.restart_checkpoint_candidate.as_path(),
            ),
            (
                FileDatabaseCreateEntry::RestartCheckpointControl,
                self.restart_checkpoint_control.as_path(),
            ),
            (
                FileDatabaseCreateEntry::RestartCheckpointCandidateControl,
                self.restart_checkpoint_candidate_control.as_path(),
            ),
            (
                FileDatabaseCreateEntry::WalReclamationCandidate,
                self.wal_reclamation_candidate.as_path(),
            ),
            (
                FileDatabaseCreateEntry::WalCandidateReclamationCandidate,
                self.wal_candidate_reclamation_candidate.as_path(),
            ),
        ];
        for (index, (second_entry, second_path)) in paths.iter().enumerate() {
            for (first_entry, first_path) in &paths[..index] {
                if first_path == second_path {
                    return Err(FileDatabaseCreateError::PathCollision {
                        first: *first_entry,
                        second: *second_entry,
                    });
                }
            }
        }
        Ok(())
    }
}

fn create_candidate_path(
    path: &Path,
    entry: FileDatabaseCreateEntry,
) -> Result<PathBuf, FileDatabaseCreateError> {
    let file_name = path
        .file_name()
        .ok_or(FileDatabaseCreateError::CandidatePathUnavailable { entry })?;
    let mut candidate_name = OsString::from(file_name);
    candidate_name.push(".create-candidate");
    Ok(path.with_file_name(candidate_name))
}

#[derive(Clone, Copy)]
struct CreateNamespaceObservation {
    evidence: FileDatabaseCreateNamespaceEvidence,
}

impl CreateNamespaceObservation {
    fn observe(
        layout: &FileDatabaseLayout,
        paths: &FileDatabaseCreatePaths,
    ) -> Result<Self, FileDatabaseCreateError> {
        let entries = [
            (
                FileDatabaseCreateEntry::DatabaseOwner,
                layout.database_owner(),
            ),
            (FileDatabaseCreateEntry::Manifest, layout.manifest()),
            (
                FileDatabaseCreateEntry::ManifestCandidate,
                paths.manifest_candidate.as_path(),
            ),
            (FileDatabaseCreateEntry::Wal, layout.wal()),
            (
                FileDatabaseCreateEntry::WalCandidate,
                paths.wal_candidate.as_path(),
            ),
            (FileDatabaseCreateEntry::PageStore, layout.page_store()),
            (
                FileDatabaseCreateEntry::PageStoreCandidate,
                paths.page_store_candidate.as_path(),
            ),
            (
                FileDatabaseCreateEntry::RestartCheckpoint,
                layout.restart_checkpoint(),
            ),
            (
                FileDatabaseCreateEntry::RestartCheckpointCandidate,
                paths.restart_checkpoint_candidate.as_path(),
            ),
        ];
        let mut present = 0_u16;
        for (bit, (entry, path)) in entries.into_iter().enumerate() {
            if create_entry_exists(path, entry)? {
                present |= 1 << bit;
            }
        }
        Ok(Self {
            evidence: FileDatabaseCreateNamespaceEvidence { present },
        })
    }

    const fn phase(self) -> Option<FileDatabaseCreatePhase> {
        match self.evidence.present {
            0b000_000_000 => Some(FileDatabaseCreatePhase::Absent),
            0b000_000_001 => Some(FileDatabaseCreatePhase::Owner),
            0b000_000_101 => Some(FileDatabaseCreatePhase::ManifestCandidate),
            0b000_010_101 => Some(FileDatabaseCreatePhase::WalCandidate),
            0b001_010_101 => Some(FileDatabaseCreatePhase::PageStoreCandidate),
            0b101_010_101 => Some(FileDatabaseCreatePhase::RestartCheckpointCandidate),
            0b101_001_101 => Some(FileDatabaseCreatePhase::WalPublished),
            0b100_101_101 => Some(FileDatabaseCreatePhase::PageStorePublished),
            0b010_101_101 => Some(FileDatabaseCreatePhase::ChildrenPublished),
            0b010_101_011 => Some(FileDatabaseCreatePhase::Published),
            _ => None,
        }
    }
}

fn create_entry_exists(
    path: &Path,
    entry: FileDatabaseCreateEntry,
) -> Result<bool, FileDatabaseCreateError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ObserveEntry { entry },
            source,
        ))),
    }
}

fn require_create_object_type(
    path: &Path,
    entry: FileDatabaseCreateEntry,
    directory: bool,
) -> Result<(), FileDatabaseCreateError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata { entry },
            source,
        ))
    })?;
    let expected = if directory {
        metadata.file_type().is_dir()
    } else {
        metadata.file_type().is_file()
    };
    if !expected {
        return Err(FileDatabaseCreateError::UnexpectedObjectType { entry });
    }
    Ok(())
}

fn validate_create_namespace_types(
    observation: CreateNamespaceObservation,
    layout: &FileDatabaseLayout,
    paths: &FileDatabaseCreatePaths,
) -> Result<(), FileDatabaseCreateError> {
    for (entry, path, directory) in [
        (
            FileDatabaseCreateEntry::DatabaseOwner,
            layout.database_owner(),
            false,
        ),
        (FileDatabaseCreateEntry::Manifest, layout.manifest(), false),
        (
            FileDatabaseCreateEntry::ManifestCandidate,
            paths.manifest_candidate.as_path(),
            false,
        ),
        (FileDatabaseCreateEntry::Wal, layout.wal(), false),
        (
            FileDatabaseCreateEntry::WalCandidate,
            paths.wal_candidate.as_path(),
            false,
        ),
        (
            FileDatabaseCreateEntry::PageStore,
            layout.page_store(),
            false,
        ),
        (
            FileDatabaseCreateEntry::PageStoreCandidate,
            paths.page_store_candidate.as_path(),
            false,
        ),
        (
            FileDatabaseCreateEntry::RestartCheckpoint,
            layout.restart_checkpoint(),
            true,
        ),
        (
            FileDatabaseCreateEntry::RestartCheckpointCandidate,
            paths.restart_checkpoint_candidate.as_path(),
            true,
        ),
    ] {
        if observation.evidence.contains(entry) {
            require_create_object_type(path, entry, directory)?;
        }
    }
    Ok(())
}

struct CreateFaultController {
    armed: Option<FileDatabaseCreateFault>,
}

impl CreateFaultController {
    const fn new(armed: Option<FileDatabaseCreateFault>) -> Self {
        Self { armed }
    }

    fn before(
        &mut self,
        boundary: FileDatabaseCreateBoundary,
    ) -> Result<(), FileDatabaseCreateError> {
        if let Some(fault) = self.armed
            && fault.boundary() == boundary
            && matches!(
                fault.timing(),
                FileDatabaseCreateFaultTiming::BeforeEffect
                    | FileDatabaseCreateFaultTiming::OutcomeIndeterminateBeforeEffect
            )
        {
            self.armed = None;
            return Err(FileDatabaseCreateError::InjectedFault(fault));
        }
        Ok(())
    }

    fn after(
        &mut self,
        boundary: FileDatabaseCreateBoundary,
    ) -> Result<(), FileDatabaseCreateError> {
        if let Some(fault) = self.armed
            && fault.boundary() == boundary
            && matches!(
                fault.timing(),
                FileDatabaseCreateFaultTiming::AfterEffect
                    | FileDatabaseCreateFaultTiming::OutcomeIndeterminateAfterEffect
            )
        {
            self.armed = None;
            return Err(FileDatabaseCreateError::InjectedFault(fault));
        }
        Ok(())
    }
}

/// Creates or resumes one exact successor-format database composition.
///
/// Candidate objects are written and synchronized before child-first,
/// manifest-last publication. The returned owner retains all five protocol
/// locks and has crossed only the recovery-required domain boundary.
pub fn create_file_database<const N: usize>(
    manifest: DatabaseManifest,
    layout: FileDatabaseLayout,
    fault: Option<FileDatabaseCreateFault>,
) -> Result<FileDatabaseCreateOutcome<N>, FileDatabaseCreateError> {
    validate_create_manifest(manifest)?;
    PageLayout::for_const::<N>().map_err(FileDatabaseCreateError::PageWidth)?;
    let paths = FileDatabaseCreatePaths::derive(&layout)?;
    let mut fault = CreateFaultController::new(fault);

    let initial = CreateNamespaceObservation::observe(&layout, &paths)?;
    let initial_phase = initial
        .phase()
        .ok_or(FileDatabaseCreateError::NamespaceConflict(initial.evidence))?;
    validate_create_namespace_types(initial, &layout, &paths)?;
    require_absent_auxiliary_entries(&paths)?;

    let (mut owner_file, owner_metadata) = match initial_phase {
        FileDatabaseCreatePhase::Absent => {
            fault.before(FileDatabaseCreateBoundary::OwnerPublication)?;
            let created = create_locked_header(
                layout.database_owner(),
                FileDatabaseCreateEntry::DatabaseOwner,
                &encode_database_owner_control(manifest.composition_identity().database_id()),
                Some(manifest.composition_identity().database_id()),
            )?;
            fault.after(FileDatabaseCreateBoundary::OwnerPublication)?;
            created
        }
        _ => open_locked_create_file(
            layout.database_owner(),
            FileDatabaseCreateEntry::DatabaseOwner,
            Some(manifest.composition_identity().database_id()),
        )?,
    };
    let owner_id = read_create_owner(&mut owner_file)?;
    let expected_database_id = manifest.composition_identity().database_id();
    if owner_id != expected_database_id {
        return Err(FileDatabaseCreateError::DatabaseOwnerIdMismatch {
            expected: expected_database_id,
            actual: owner_id,
        });
    }
    sync_create_file_and_parent(
        &owner_file,
        layout.database_owner(),
        FileDatabaseCreateEntry::DatabaseOwner,
    )?;

    let locked_observation = CreateNamespaceObservation::observe(&layout, &paths)?;
    let phase = locked_observation
        .phase()
        .ok_or(FileDatabaseCreateError::NamespaceConflict(
            locked_observation.evidence,
        ))?;
    validate_create_namespace_types(locked_observation, &layout, &paths)?;
    require_absent_auxiliary_entries(&paths)?;
    if phase == FileDatabaseCreatePhase::Published {
        return open_already_published(manifest, layout, owner_file, owner_metadata);
    }
    if phase == FileDatabaseCreatePhase::Absent {
        return Err(FileDatabaseCreateError::NamespaceConflict(
            locked_observation.evidence,
        ));
    }

    let manifest_probe_metadata = if phase == FileDatabaseCreatePhase::Owner {
        None
    } else {
        let metadata = preflight_create_metadata(
            &paths.manifest_candidate,
            FileDatabaseCreateEntry::ManifestCandidate,
        )?;
        reject_create_alias(
            FileDatabaseCreateEntry::DatabaseOwner,
            &owner_metadata,
            FileDatabaseCreateEntry::ManifestCandidate,
            &metadata,
        )?;
        Some(metadata)
    };
    let (mut manifest_file, manifest_metadata) = if phase == FileDatabaseCreatePhase::Owner {
        fault.before(FileDatabaseCreateBoundary::ManifestCandidatePublication)?;
        let encoded_manifest = super::encode_database_manifest(&manifest)
            .map_err(FileDatabaseCreateError::ManifestEncode)?;
        let created = create_locked_header(
            &paths.manifest_candidate,
            FileDatabaseCreateEntry::ManifestCandidate,
            &encoded_manifest,
            None,
        )?;
        fault.after(FileDatabaseCreateBoundary::ManifestCandidatePublication)?;
        created
    } else {
        open_locked_create_file(
            &paths.manifest_candidate,
            FileDatabaseCreateEntry::ManifestCandidate,
            None,
        )?
    };
    if let Some(probed) = &manifest_probe_metadata {
        require_same_create_object(
            FileDatabaseCreateEntry::ManifestCandidate,
            probed,
            &manifest_metadata,
        )?;
    }
    reject_create_alias(
        FileDatabaseCreateEntry::DatabaseOwner,
        &owner_metadata,
        FileDatabaseCreateEntry::ManifestCandidate,
        &manifest_metadata,
    )?;
    let observed_manifest =
        read_create_manifest(&mut manifest_file, FileDatabaseCreateLocation::Candidate)?;
    if observed_manifest != manifest {
        return Err(FileDatabaseCreateError::ManifestMismatch(Box::new(
            FileDatabaseCreateManifestMismatch {
                expected: manifest,
                actual: observed_manifest,
            },
        )));
    }
    sync_create_file_and_parent(
        &manifest_file,
        &paths.manifest_candidate,
        FileDatabaseCreateEntry::ManifestCandidate,
    )?;

    let wal_location = if phase < FileDatabaseCreatePhase::WalPublished {
        FileDatabaseCreateLocation::Candidate
    } else {
        FileDatabaseCreateLocation::Final
    };
    let wal_path = if wal_location == FileDatabaseCreateLocation::Candidate {
        paths.wal_candidate.as_path()
    } else {
        layout.wal()
    };
    let wal_probe_metadata = if phase < FileDatabaseCreatePhase::WalCandidate {
        None
    } else {
        let metadata = preflight_create_metadata(
            wal_path,
            create_child_entry(DatabaseFileRole::Wal, wal_location),
        )?;
        reject_create_alias_prefix(
            create_child_entry(DatabaseFileRole::Wal, wal_location),
            &metadata,
            &[
                (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
                (
                    FileDatabaseCreateEntry::ManifestCandidate,
                    &manifest_metadata,
                ),
            ],
        )?;
        Some(metadata)
    };
    let mut wal = if phase < FileDatabaseCreatePhase::WalCandidate {
        fault.before(FileDatabaseCreateBoundary::WalCandidatePublication)?;
        let created = FileCommitLog::<N>::create_new_database_transaction_page_capable(
            &paths.wal_candidate,
            manifest.composition_identity().storage_identity(),
        )
        .map_err(FileDatabaseCreateError::WalCreate)?;
        fault.after(FileDatabaseCreateBoundary::WalCandidatePublication)?;
        created
    } else if let Some(probed) = &wal_probe_metadata {
        open_exact_initial_create_wal(wal_path, manifest, wal_location, probed)?
    } else {
        return Err(FileDatabaseCreateError::NamespaceConflict(
            locked_observation.evidence,
        ));
    };
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::Wal,
            format_version: 5,
            persistent_log_id: wal.persistent_id(),
            database_file_identity: wal.database_file_identity(),
        },
    )
    .map_err(FileDatabaseCreateError::ChildValidation)?;
    let wal_metadata = wal.database_create_metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata {
                entry: create_child_entry(DatabaseFileRole::Wal, wal_location),
            },
            source,
        ))
    })?;
    if let Some(probed) = &wal_probe_metadata {
        require_same_create_object(
            create_child_entry(DatabaseFileRole::Wal, wal_location),
            probed,
            &wal_metadata,
        )?;
    }
    reject_create_alias_prefix(
        create_child_entry(DatabaseFileRole::Wal, wal_location),
        &wal_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (
                FileDatabaseCreateEntry::ManifestCandidate,
                &manifest_metadata,
            ),
        ],
    )?;

    let page_location = if phase < FileDatabaseCreatePhase::PageStorePublished {
        FileDatabaseCreateLocation::Candidate
    } else {
        FileDatabaseCreateLocation::Final
    };
    let page_store_path = if page_location == FileDatabaseCreateLocation::Candidate {
        paths.page_store_candidate.as_path()
    } else {
        layout.page_store()
    };
    let page_probe_metadata = if phase < FileDatabaseCreatePhase::PageStoreCandidate {
        None
    } else {
        let metadata = preflight_create_metadata(
            page_store_path,
            create_child_entry(DatabaseFileRole::PageStore, page_location),
        )?;
        reject_create_alias_prefix(
            create_child_entry(DatabaseFileRole::PageStore, page_location),
            &metadata,
            &[
                (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
                (
                    FileDatabaseCreateEntry::ManifestCandidate,
                    &manifest_metadata,
                ),
                (
                    create_child_entry(DatabaseFileRole::Wal, wal_location),
                    &wal_metadata,
                ),
            ],
        )?;
        Some(metadata)
    };
    let page_store = if phase < FileDatabaseCreatePhase::PageStoreCandidate {
        fault.before(FileDatabaseCreateBoundary::PageStoreCandidatePublication)?;
        let created = FilePageStore::<N>::create_new_database(
            &paths.page_store_candidate,
            manifest.composition_identity().storage_identity(),
        )
        .map_err(FileDatabaseCreateError::PageStoreCreate)?;
        fault.after(FileDatabaseCreateBoundary::PageStoreCandidatePublication)?;
        created
    } else {
        open_exact_initial_create_page_store(page_store_path, manifest, page_location)?
    };
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::PageStore,
            format_version: 2,
            persistent_log_id: page_store.persistent_id(),
            database_file_identity: page_store.database_file_identity(),
        },
    )
    .map_err(FileDatabaseCreateError::ChildValidation)?;
    let page_metadata = page_store.database_create_metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata {
                entry: create_child_entry(DatabaseFileRole::PageStore, page_location),
            },
            source,
        ))
    })?;
    if let Some(probed) = &page_probe_metadata {
        require_same_create_object(
            create_child_entry(DatabaseFileRole::PageStore, page_location),
            probed,
            &page_metadata,
        )?;
    }
    reject_create_alias_prefix(
        create_child_entry(DatabaseFileRole::PageStore, page_location),
        &page_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (
                FileDatabaseCreateEntry::ManifestCandidate,
                &manifest_metadata,
            ),
            (
                create_child_entry(DatabaseFileRole::Wal, wal_location),
                &wal_metadata,
            ),
        ],
    )?;

    let checkpoint_location = if phase < FileDatabaseCreatePhase::ChildrenPublished {
        FileDatabaseCreateLocation::Candidate
    } else {
        FileDatabaseCreateLocation::Final
    };
    let checkpoint_path = if checkpoint_location == FileDatabaseCreateLocation::Candidate {
        paths.restart_checkpoint_candidate.as_path()
    } else {
        layout.restart_checkpoint()
    };
    let checkpoint_control_path = checkpoint_path.join(CONTROL_FILE_NAME);
    let checkpoint_probe_metadata = if phase < FileDatabaseCreatePhase::RestartCheckpointCandidate {
        None
    } else {
        let metadata = preflight_create_metadata(
            &checkpoint_control_path,
            create_child_entry(DatabaseFileRole::RestartCheckpoint, checkpoint_location),
        )?;
        reject_create_alias_prefix(
            create_child_entry(DatabaseFileRole::RestartCheckpoint, checkpoint_location),
            &metadata,
            &[
                (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
                (
                    FileDatabaseCreateEntry::ManifestCandidate,
                    &manifest_metadata,
                ),
                (
                    create_child_entry(DatabaseFileRole::Wal, wal_location),
                    &wal_metadata,
                ),
                (
                    create_child_entry(DatabaseFileRole::PageStore, page_location),
                    &page_metadata,
                ),
            ],
        )?;
        Some(metadata)
    };
    let mut checkpoint = if phase < FileDatabaseCreatePhase::RestartCheckpointCandidate {
        fault.before(FileDatabaseCreateBoundary::RestartCheckpointCandidatePublication)?;
        let created = FileRestartCheckpointCompletenessBaselineSource::create_new_database(
            &paths.restart_checkpoint_candidate,
            manifest.composition_identity().storage_identity(),
        )
        .map_err(FileDatabaseCreateError::RestartCheckpointCreate)?;
        fault.after(FileDatabaseCreateBoundary::RestartCheckpointCandidatePublication)?;
        created
    } else {
        FileRestartCheckpointCompletenessBaselineSource::open(checkpoint_path)
            .map_err(FileDatabaseCreateError::RestartCheckpointOpen)?
    };
    validate_exact_initial_checkpoint(&checkpoint, checkpoint_location)?;
    checkpoint.sync_for_database_create().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::SyncObject {
                entry: create_child_entry(DatabaseFileRole::RestartCheckpoint, checkpoint_location),
            },
            source,
        ))
    })?;
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::RestartCheckpoint,
            format_version: checkpoint.control_format_version(),
            persistent_log_id: checkpoint.persistent_log_id(),
            database_file_identity: checkpoint.database_file_identity(),
        },
    )
    .map_err(FileDatabaseCreateError::ChildValidation)?;
    let checkpoint_metadata = checkpoint.control_metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata {
                entry: create_child_entry(DatabaseFileRole::RestartCheckpoint, checkpoint_location),
            },
            source,
        ))
    })?;
    if let Some(probed) = &checkpoint_probe_metadata {
        require_same_create_object(
            create_child_entry(DatabaseFileRole::RestartCheckpoint, checkpoint_location),
            probed,
            &checkpoint_metadata,
        )?;
    }
    reject_create_alias_prefix(
        create_child_entry(DatabaseFileRole::RestartCheckpoint, checkpoint_location),
        &checkpoint_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (
                FileDatabaseCreateEntry::ManifestCandidate,
                &manifest_metadata,
            ),
            (
                create_child_entry(DatabaseFileRole::Wal, wal_location),
                &wal_metadata,
            ),
            (
                create_child_entry(DatabaseFileRole::PageStore, page_location),
                &page_metadata,
            ),
        ],
    )?;

    if phase < FileDatabaseCreatePhase::WalPublished {
        fault.before(FileDatabaseCreateBoundary::WalPublication)?;
        publish_create_candidate(
            &paths.wal_candidate,
            layout.wal(),
            FileDatabaseCreateEntry::Wal,
        )?;
        wal.rebind_database_selected_path(layout.wal());
        fault.after(FileDatabaseCreateBoundary::WalPublication)?;
    }
    if phase < FileDatabaseCreatePhase::PageStorePublished {
        fault.before(FileDatabaseCreateBoundary::PageStorePublication)?;
        publish_create_candidate(
            &paths.page_store_candidate,
            layout.page_store(),
            FileDatabaseCreateEntry::PageStore,
        )?;
        fault.after(FileDatabaseCreateBoundary::PageStorePublication)?;
    }
    if phase < FileDatabaseCreatePhase::ChildrenPublished {
        fault.before(FileDatabaseCreateBoundary::RestartCheckpointPublication)?;
        publish_create_candidate(
            &paths.restart_checkpoint_candidate,
            layout.restart_checkpoint(),
            FileDatabaseCreateEntry::RestartCheckpoint,
        )?;
        checkpoint.rebind_database_selected_slot(layout.restart_checkpoint());
        fault.after(FileDatabaseCreateBoundary::RestartCheckpointPublication)?;
    }
    fault.before(FileDatabaseCreateBoundary::ManifestPublication)?;
    publish_create_candidate(
        &paths.manifest_candidate,
        layout.manifest(),
        FileDatabaseCreateEntry::Manifest,
    )?;
    fault.after(FileDatabaseCreateBoundary::ManifestPublication)?;

    let database = bind_created_file_database(
        manifest,
        layout,
        owner_file,
        manifest_file,
        wal,
        page_store,
        checkpoint,
    )?;
    Ok(FileDatabaseCreateOutcome::Created(database))
}

fn validate_create_manifest(manifest: DatabaseManifest) -> Result<(), FileDatabaseCreateError> {
    let generation = manifest.composition_identity().lifecycle_generation().get();
    if generation != 1 {
        return Err(FileDatabaseCreateError::ManifestRequirement(
            FileDatabaseCreateManifestError::LifecycleGeneration { actual: generation },
        ));
    }
    for (role, expected) in [
        (DatabaseFileRole::Wal, 5_u16),
        (DatabaseFileRole::PageStore, 2),
        (DatabaseFileRole::RestartCheckpoint, 2),
    ] {
        let actual = manifest.storage_formats().version(role).get();
        if actual != expected {
            return Err(FileDatabaseCreateError::ManifestRequirement(
                FileDatabaseCreateManifestError::StorageFormatVersion {
                    role,
                    expected,
                    actual,
                },
            ));
        }
    }
    let required_features = manifest.required_features().bits();
    if required_features != 0 {
        return Err(FileDatabaseCreateError::ManifestRequirement(
            FileDatabaseCreateManifestError::RequiredFeatures {
                actual: required_features,
            },
        ));
    }
    Ok(())
}

fn open_already_published<const N: usize>(
    manifest: DatabaseManifest,
    layout: FileDatabaseLayout,
    owner_file: File,
    owner_metadata: Metadata,
) -> Result<FileDatabaseCreateOutcome<N>, FileDatabaseCreateError> {
    let manifest_probe_metadata =
        preflight_create_metadata(layout.manifest(), FileDatabaseCreateEntry::Manifest)?;
    reject_create_alias(
        FileDatabaseCreateEntry::DatabaseOwner,
        &owner_metadata,
        FileDatabaseCreateEntry::Manifest,
        &manifest_probe_metadata,
    )?;
    let (mut manifest_file, manifest_metadata) =
        open_locked_create_file(layout.manifest(), FileDatabaseCreateEntry::Manifest, None)?;
    require_same_create_object(
        FileDatabaseCreateEntry::Manifest,
        &manifest_probe_metadata,
        &manifest_metadata,
    )?;
    reject_create_alias(
        FileDatabaseCreateEntry::DatabaseOwner,
        &owner_metadata,
        FileDatabaseCreateEntry::Manifest,
        &manifest_metadata,
    )?;
    let observed_manifest =
        read_create_manifest(&mut manifest_file, FileDatabaseCreateLocation::Final)?;
    if observed_manifest != manifest {
        return Err(FileDatabaseCreateError::ManifestMismatch(Box::new(
            FileDatabaseCreateManifestMismatch {
                expected: manifest,
                actual: observed_manifest,
            },
        )));
    }
    let wal_probe_metadata = preflight_create_metadata(layout.wal(), FileDatabaseCreateEntry::Wal)?;
    reject_create_alias_prefix(
        FileDatabaseCreateEntry::Wal,
        &wal_probe_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (FileDatabaseCreateEntry::Manifest, &manifest_metadata),
        ],
    )?;
    let wal = open_exact_initial_create_wal(
        layout.wal(),
        manifest,
        FileDatabaseCreateLocation::Final,
        &wal_probe_metadata,
    )?;
    let wal_metadata = wal.database_create_metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata {
                entry: FileDatabaseCreateEntry::Wal,
            },
            source,
        ))
    })?;
    require_same_create_object(
        FileDatabaseCreateEntry::Wal,
        &wal_probe_metadata,
        &wal_metadata,
    )?;
    reject_create_alias_prefix(
        FileDatabaseCreateEntry::Wal,
        &wal_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (FileDatabaseCreateEntry::Manifest, &manifest_metadata),
        ],
    )?;

    let page_probe_metadata =
        preflight_create_metadata(layout.page_store(), FileDatabaseCreateEntry::PageStore)?;
    reject_create_alias_prefix(
        FileDatabaseCreateEntry::PageStore,
        &page_probe_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (FileDatabaseCreateEntry::Manifest, &manifest_metadata),
            (FileDatabaseCreateEntry::Wal, &wal_metadata),
        ],
    )?;
    let page_store = open_exact_initial_create_page_store(
        layout.page_store(),
        manifest,
        FileDatabaseCreateLocation::Final,
    )?;
    let page_metadata = page_store.database_create_metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata {
                entry: FileDatabaseCreateEntry::PageStore,
            },
            source,
        ))
    })?;
    require_same_create_object(
        FileDatabaseCreateEntry::PageStore,
        &page_probe_metadata,
        &page_metadata,
    )?;
    reject_create_alias_prefix(
        FileDatabaseCreateEntry::PageStore,
        &page_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (FileDatabaseCreateEntry::Manifest, &manifest_metadata),
            (FileDatabaseCreateEntry::Wal, &wal_metadata),
        ],
    )?;

    let checkpoint_control_path = layout.restart_checkpoint().join(CONTROL_FILE_NAME);
    let checkpoint_probe_metadata = preflight_create_metadata(
        &checkpoint_control_path,
        FileDatabaseCreateEntry::RestartCheckpoint,
    )?;
    reject_create_alias_prefix(
        FileDatabaseCreateEntry::RestartCheckpoint,
        &checkpoint_probe_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (FileDatabaseCreateEntry::Manifest, &manifest_metadata),
            (FileDatabaseCreateEntry::Wal, &wal_metadata),
            (FileDatabaseCreateEntry::PageStore, &page_metadata),
        ],
    )?;
    let checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::open(layout.restart_checkpoint())
            .map_err(FileDatabaseCreateError::RestartCheckpointOpen)?;
    validate_exact_initial_checkpoint(&checkpoint, FileDatabaseCreateLocation::Final)?;
    checkpoint.sync_for_database_create().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::SyncObject {
                entry: FileDatabaseCreateEntry::RestartCheckpoint,
            },
            source,
        ))
    })?;
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::RestartCheckpoint,
            format_version: checkpoint.control_format_version(),
            persistent_log_id: checkpoint.persistent_log_id(),
            database_file_identity: checkpoint.database_file_identity(),
        },
    )
    .map_err(FileDatabaseCreateError::ChildValidation)?;
    let checkpoint_metadata = checkpoint.control_metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata {
                entry: FileDatabaseCreateEntry::RestartCheckpoint,
            },
            source,
        ))
    })?;
    require_same_create_object(
        FileDatabaseCreateEntry::RestartCheckpoint,
        &checkpoint_probe_metadata,
        &checkpoint_metadata,
    )?;
    reject_create_alias_prefix(
        FileDatabaseCreateEntry::RestartCheckpoint,
        &checkpoint_metadata,
        &[
            (FileDatabaseCreateEntry::DatabaseOwner, &owner_metadata),
            (FileDatabaseCreateEntry::Manifest, &manifest_metadata),
            (FileDatabaseCreateEntry::Wal, &wal_metadata),
            (FileDatabaseCreateEntry::PageStore, &page_metadata),
        ],
    )?;
    sync_create_file_and_parent(
        &manifest_file,
        layout.manifest(),
        FileDatabaseCreateEntry::Manifest,
    )?;

    let database = bind_created_file_database(
        manifest,
        layout,
        owner_file,
        manifest_file,
        wal,
        page_store,
        checkpoint,
    )?;
    Ok(FileDatabaseCreateOutcome::AlreadyPublished(database))
}

fn create_locked_header(
    path: &Path,
    entry: FileDatabaseCreateEntry,
    bytes: &[u8],
    contended_database_id: Option<DatabaseId>,
) -> Result<(File, Metadata), FileDatabaseCreateError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| {
            FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
                FileDatabaseCreateIoStage::CreateFile { entry },
                source,
            ))
        })?;
    lock_create_file(&file, entry, contended_database_id)?;
    file.write_all(bytes).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::WriteHeader { entry },
            source,
        ))
    })?;
    file.sync_all().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::SyncObject { entry },
            source,
        ))
    })?;
    let metadata = file.metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata { entry },
            source,
        ))
    })?;
    sync_create_parent(path, entry)?;
    Ok((file, metadata))
}

fn open_locked_create_file(
    path: &Path,
    entry: FileDatabaseCreateEntry,
    contended_database_id: Option<DatabaseId>,
) -> Result<(File, Metadata), FileDatabaseCreateError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| {
            FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
                FileDatabaseCreateIoStage::OpenFile { entry },
                source,
            ))
        })?;
    lock_create_file(&file, entry, contended_database_id)?;
    let metadata = file.metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata { entry },
            source,
        ))
    })?;
    Ok((file, metadata))
}

fn lock_create_file(
    file: &File,
    entry: FileDatabaseCreateEntry,
    contended_database_id: Option<DatabaseId>,
) -> Result<(), FileDatabaseCreateError> {
    if let Err(source) = file.try_lock() {
        let source: io::Error = source.into();
        if let Some(database_id) = contended_database_id {
            return Err(FileDatabaseCreateError::OwnershipContended {
                database_id,
                source,
            });
        }
        return Err(FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::AcquireExclusiveLock { entry },
            source,
        )));
    }
    Ok(())
}

fn read_create_owner(file: &mut File) -> Result<DatabaseId, FileDatabaseCreateError> {
    let actual = file.metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata {
                entry: FileDatabaseCreateEntry::DatabaseOwner,
            },
            source,
        ))
    })?;
    if actual.len() != DATABASE_OWNER_CONTROL_V1_LENGTH as u64 {
        return Err(FileDatabaseCreateError::DatabaseOwnerControl(
            if actual.len() < DATABASE_OWNER_CONTROL_V1_LENGTH as u64 {
                DatabaseOwnerControlDecodeError::Truncated {
                    actual: usize::try_from(actual.len()).map_or(usize::MAX, |length| length),
                }
            } else {
                DatabaseOwnerControlDecodeError::TrailingBytes {
                    actual: usize::try_from(actual.len()).map_or(usize::MAX, |length| length),
                }
            },
        ));
    }
    let mut bytes = [0_u8; DATABASE_OWNER_CONTROL_V1_LENGTH];
    file.seek(SeekFrom::Start(0)).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadHeader {
                entry: FileDatabaseCreateEntry::DatabaseOwner,
            },
            source,
        ))
    })?;
    file.read_exact(&mut bytes).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadHeader {
                entry: FileDatabaseCreateEntry::DatabaseOwner,
            },
            source,
        ))
    })?;
    decode_database_owner_control(&bytes).map_err(FileDatabaseCreateError::DatabaseOwnerControl)
}

fn read_create_manifest(
    file: &mut File,
    location: FileDatabaseCreateLocation,
) -> Result<DatabaseManifest, FileDatabaseCreateError> {
    let entry = match location {
        FileDatabaseCreateLocation::Candidate => FileDatabaseCreateEntry::ManifestCandidate,
        FileDatabaseCreateLocation::Final => FileDatabaseCreateEntry::Manifest,
    };
    let actual = file.metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata { entry },
            source,
        ))
    })?;
    if actual.len() != super::DATABASE_MANIFEST_V1_LENGTH as u64 {
        return Err(FileDatabaseCreateError::ManifestFileLength {
            location,
            actual: actual.len(),
        });
    }
    let mut bytes = [0_u8; super::DATABASE_MANIFEST_V1_LENGTH];
    file.seek(SeekFrom::Start(0)).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadHeader { entry },
            source,
        ))
    })?;
    file.read_exact(&mut bytes).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadHeader { entry },
            source,
        ))
    })?;
    decode_database_manifest(&bytes).map_err(FileDatabaseCreateError::Manifest)
}

fn sync_create_file_and_parent(
    file: &File,
    path: &Path,
    entry: FileDatabaseCreateEntry,
) -> Result<(), FileDatabaseCreateError> {
    file.sync_all().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::SyncObject { entry },
            source,
        ))
    })?;
    sync_create_parent(path, entry)
}

fn sync_create_parent(
    path: &Path,
    entry: FileDatabaseCreateEntry,
) -> Result<(), FileDatabaseCreateError> {
    let parent = match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => {
            return Err(FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
                FileDatabaseCreateIoStage::OpenParentDirectory { entry },
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "database create path has no parent directory",
                ),
            )));
        }
    };
    let directory = File::open(parent).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::OpenParentDirectory { entry },
            source,
        ))
    })?;
    directory.sync_all().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::SyncParentDirectory { entry },
            source,
        ))
    })
}

fn require_absent_auxiliary_entries(
    paths: &FileDatabaseCreatePaths,
) -> Result<(), FileDatabaseCreateError> {
    for (entry, path) in [
        (
            FileDatabaseCreateEntry::WalReclamationCandidate,
            paths.wal_reclamation_candidate.as_path(),
        ),
        (
            FileDatabaseCreateEntry::WalCandidateReclamationCandidate,
            paths.wal_candidate_reclamation_candidate.as_path(),
        ),
    ] {
        if create_entry_exists(path, entry)? {
            return Err(FileDatabaseCreateError::UnexpectedAuxiliaryEntry { entry });
        }
    }
    Ok(())
}

fn open_exact_initial_create_wal<const N: usize>(
    path: &Path,
    manifest: DatabaseManifest,
    location: FileDatabaseCreateLocation,
    probed_metadata: &Metadata,
) -> Result<FileCommitLog<N>, FileDatabaseCreateError> {
    let pending = FileCommitLog::<N>::inspect_transaction_page_capable(path)
        .map_err(FileDatabaseCreateError::WalOpen)?;
    let entry = create_child_entry(DatabaseFileRole::Wal, location);
    let locked_metadata = pending.metadata().map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata { entry },
            source,
        ))
    })?;
    require_same_create_object(entry, probed_metadata, &locked_metadata)?;
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::Wal,
            format_version: pending.physical_format_version(),
            persistent_log_id: pending.persistent_id(),
            database_file_identity: pending.database_file_identity(),
        },
    )
    .map_err(FileDatabaseCreateError::ChildValidation)?;
    if !pending.is_exact_initial_database_file() {
        return Err(FileDatabaseCreateError::NonInitialChild {
            role: DatabaseFileRole::Wal,
            location,
        });
    }
    let wal = pending
        .finish_for_database_create()
        .map_err(FileDatabaseCreateError::WalOpen)?;
    sync_create_parent(path, entry)?;
    Ok(wal)
}

fn open_exact_initial_create_page_store<const N: usize>(
    path: &Path,
    manifest: DatabaseManifest,
    location: FileDatabaseCreateLocation,
) -> Result<FilePageStore<N>, FileDatabaseCreateError> {
    let pending =
        FilePageStore::<N>::inspect(path).map_err(FileDatabaseCreateError::PageStoreOpen)?;
    validate_child_observation(
        manifest,
        ChildObservation {
            role: DatabaseFileRole::PageStore,
            format_version: pending.physical_format_version(),
            persistent_log_id: pending.persistent_id(),
            database_file_identity: pending.database_file_identity(),
        },
    )
    .map_err(FileDatabaseCreateError::ChildValidation)?;
    if !pending.is_exact_initial_database_file() {
        return Err(FileDatabaseCreateError::NonInitialChild {
            role: DatabaseFileRole::PageStore,
            location,
        });
    }
    let store = pending
        .finish()
        .map_err(FileDatabaseCreateError::PageStoreOpen)?;
    sync_create_parent(
        path,
        create_child_entry(DatabaseFileRole::PageStore, location),
    )?;
    Ok(store)
}

fn validate_exact_initial_checkpoint(
    checkpoint: &FileRestartCheckpointCompletenessBaselineSource,
    location: FileDatabaseCreateLocation,
) -> Result<(), FileDatabaseCreateError> {
    let slot = checkpoint.slot_directory();
    let control_path = slot.join(CONTROL_FILE_NAME);
    require_create_object_type(
        slot,
        create_child_entry(DatabaseFileRole::RestartCheckpoint, location),
        true,
    )?;
    require_create_object_type(
        &control_path,
        match location {
            FileDatabaseCreateLocation::Candidate => {
                FileDatabaseCreateEntry::RestartCheckpointCandidateControl
            }
            FileDatabaseCreateLocation::Final => FileDatabaseCreateEntry::RestartCheckpointControl,
        },
        false,
    )?;
    let entries = fs::read_dir(slot).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadCheckpointDirectory { location },
            source,
        ))
    })?;
    let mut control_seen = false;
    for entry in entries {
        let entry = entry.map_err(|source| {
            FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
                FileDatabaseCreateIoStage::ReadCheckpointDirectory { location },
                source,
            ))
        })?;
        let name = entry.file_name();
        if name == CONTROL_FILE_NAME {
            if control_seen {
                return Err(FileDatabaseCreateError::UnexpectedCheckpointEntry {
                    location,
                    actual: name,
                });
            }
            control_seen = true;
        } else {
            return Err(FileDatabaseCreateError::UnexpectedCheckpointEntry {
                location,
                actual: name,
            });
        }
    }
    if !control_seen {
        return Err(FileDatabaseCreateError::UnexpectedCheckpointEntry {
            location,
            actual: OsString::from("<missing control>"),
        });
    }
    Ok(())
}

const fn create_child_entry(
    role: DatabaseFileRole,
    location: FileDatabaseCreateLocation,
) -> FileDatabaseCreateEntry {
    match (role, location) {
        (DatabaseFileRole::Wal, FileDatabaseCreateLocation::Candidate) => {
            FileDatabaseCreateEntry::WalCandidate
        }
        (DatabaseFileRole::Wal, FileDatabaseCreateLocation::Final) => FileDatabaseCreateEntry::Wal,
        (DatabaseFileRole::PageStore, FileDatabaseCreateLocation::Candidate) => {
            FileDatabaseCreateEntry::PageStoreCandidate
        }
        (DatabaseFileRole::PageStore, FileDatabaseCreateLocation::Final) => {
            FileDatabaseCreateEntry::PageStore
        }
        (DatabaseFileRole::RestartCheckpoint, FileDatabaseCreateLocation::Candidate) => {
            FileDatabaseCreateEntry::RestartCheckpointCandidate
        }
        (DatabaseFileRole::RestartCheckpoint, FileDatabaseCreateLocation::Final) => {
            FileDatabaseCreateEntry::RestartCheckpoint
        }
    }
}

fn reject_create_alias(
    first: FileDatabaseCreateEntry,
    first_metadata: &Metadata,
    second: FileDatabaseCreateEntry,
    second_metadata: &Metadata,
) -> Result<(), FileDatabaseCreateError> {
    if metadata_identifies_same_file(first_metadata, second_metadata) {
        return Err(FileDatabaseCreateError::OpenedObjectAlias { first, second });
    }
    Ok(())
}

fn reject_create_alias_prefix(
    second: FileDatabaseCreateEntry,
    second_metadata: &Metadata,
    prefix: &[(FileDatabaseCreateEntry, &Metadata)],
) -> Result<(), FileDatabaseCreateError> {
    for (first, first_metadata) in prefix {
        reject_create_alias(*first, first_metadata, second, second_metadata)?;
    }
    Ok(())
}

fn preflight_create_metadata(
    path: &Path,
    entry: FileDatabaseCreateEntry,
) -> Result<Metadata, FileDatabaseCreateError> {
    fs::metadata(path).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::ReadMetadata { entry },
            source,
        ))
    })
}

fn require_same_create_object(
    entry: FileDatabaseCreateEntry,
    probed: &Metadata,
    locked: &Metadata,
) -> Result<(), FileDatabaseCreateError> {
    #[cfg(unix)]
    if !metadata_identifies_same_file(probed, locked) {
        return Err(FileDatabaseCreateError::OpenedObjectChanged { entry });
    }

    #[cfg(not(unix))]
    let _ = (entry, probed, locked);

    Ok(())
}

fn publish_create_candidate(
    candidate: &Path,
    selected: &Path,
    entry: FileDatabaseCreateEntry,
) -> Result<(), FileDatabaseCreateError> {
    if create_entry_exists(selected, entry)? {
        return Err(FileDatabaseCreateError::NamespaceConflict(
            FileDatabaseCreateNamespaceEvidence { present: 0 },
        ));
    }
    fs::rename(candidate, selected).map_err(|source| {
        FileDatabaseCreateError::Io(FileDatabaseCreateIoError::new(
            FileDatabaseCreateIoStage::RenameCandidate { entry },
            source,
        ))
    })?;
    sync_create_parent(selected, entry)
}

fn bind_created_file_database<const N: usize>(
    manifest: DatabaseManifest,
    layout: FileDatabaseLayout,
    database_owner_file: File,
    manifest_file: File,
    wal: FileCommitLog<N>,
    page_store: FilePageStore<N>,
    checkpoint: FileRestartCheckpointCompletenessBaselineSource,
) -> Result<RecoveryRequiredFileDatabase<N>, FileDatabaseCreateError> {
    let wal_identity =
        wal.database_file_identity()
            .ok_or(FileDatabaseCreateError::NonInitialChild {
                role: DatabaseFileRole::Wal,
                location: FileDatabaseCreateLocation::Final,
            })?;
    let page_identity =
        page_store
            .database_file_identity()
            .ok_or(FileDatabaseCreateError::NonInitialChild {
                role: DatabaseFileRole::PageStore,
                location: FileDatabaseCreateLocation::Final,
            })?;
    let checkpoint_identity =
        checkpoint
            .database_file_identity()
            .ok_or(FileDatabaseCreateError::NonInitialChild {
                role: DatabaseFileRole::RestartCheckpoint,
                location: FileDatabaseCreateLocation::Final,
            })?;
    let observed_files = [
        wal_identity.file(),
        page_identity.file(),
        checkpoint_identity.file(),
    ];
    let observed_storage_identity = DatabaseStorageIdentity::new(
        wal_identity.database_id(),
        wal.persistent_id(),
        &observed_files,
    )
    .map_err(FileDatabaseCreateError::ObservedStorageIdentity)?;
    let composition =
        UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint::from_locked_parts(
            wal, page_store, checkpoint,
        );
    let selected_identity = manifest.composition_identity();
    let owner = FileDatabaseOwnership {
        composition,
        _manifest_file: manifest_file,
        _database_owner_file: database_owner_file,
        manifest,
        layout,
    };
    let selected = match UnboundDatabase::new(owner, selected_identity.database_id())
        .select_manifest(selected_identity)
    {
        Ok(selected) => selected,
        Err(failure) => {
            let reason = *failure.reason();
            drop(failure);
            return Err(FileDatabaseCreateError::ManifestSelection(reason));
        }
    };
    selected
        .bind_observed_storage(observed_storage_identity)
        .map_err(|failure| FileDatabaseCreateError::StorageBinding(*failure.reason()))
}
