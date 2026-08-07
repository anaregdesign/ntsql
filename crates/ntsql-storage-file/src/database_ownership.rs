use std::{
    error::Error,
    fmt,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseCompositionIdentityError,
    DatabaseCompositionIdentityMismatch, DatabaseFileHeaderIdentity, DatabaseFileRole, DatabaseId,
    DatabaseLifecycleStage, DatabaseManifest, DatabaseManifestSelectionRejection,
    DatabaseStorageFormatVersion, DatabaseStorageIdentity, ManifestSelectedDatabase,
    RecoveryRequiredDatabase, UnboundDatabase,
};
use ntsql_wal::PersistentLogId;

use super::{
    FileCommitLog, FileOpenError, FilePageStore, FilePageWidthError,
    FileRestartCheckpointCompletenessBaselineSource, PageLayout, PageStoreOpenError,
    UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint, checksum_v1,
    decode_database_manifest, metadata_identifies_same_file, read_u16, read_u32, read_u64,
    read_u128,
    restart_checkpoint_file::{CONTROL_FILE_NAME, FileRestartCheckpointSlotOpenError},
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
    /// The selected manifest file does not have its exact fixed length.
    ManifestFileLength {
        /// Exact opened file length.
        actual: u64,
    },
    /// The selected manifest bytes are structurally invalid.
    Manifest(super::DatabaseManifestDecodeError),
    /// The manifest belongs to a database other than the locked owner.
    ManifestDatabaseIdMismatch {
        /// Identity decoded from the stable owner control.
        owner: DatabaseId,
        /// Identity decoded from the manifest.
        manifest: DatabaseId,
    },
    /// The selected WAL could not be locked and reconstructed.
    WalOpen(FileOpenError),
    /// The selected page store could not be locked and reconstructed.
    PageStoreOpen(PageStoreOpenError),
    /// The selected restart-checkpoint completeness slot could not be locked and
    /// reconstructed.
    RestartCheckpointOpen(FileRestartCheckpointSlotOpenError),
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
            Self::ManifestFileLength { actual } => write!(
                formatter,
                "database manifest file length {actual} is not {}",
                super::DATABASE_MANIFEST_V1_LENGTH
            ),
            Self::Manifest(source) => {
                write!(formatter, "database manifest decode failed: {source}")
            }
            Self::ManifestDatabaseIdMismatch { owner, manifest } => write!(
                formatter,
                "database manifest identity {} does not match locked owner {}",
                manifest.get(),
                owner.get()
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
            Self::WalOpen(source) => Some(source),
            Self::PageStoreOpen(source) => Some(source),
            Self::RestartCheckpointOpen(source) => Some(source),
            Self::ManifestSelection(source) => Some(source),
            Self::ObservedStorageIdentity(source) => Some(source),
            Self::StorageBinding(source) => Some(source),
            Self::DatabaseOwnerControlFileLength { .. }
            | Self::DatabaseOwnerIdMismatch { .. }
            | Self::OpenedObjectAlias { .. }
            | Self::OpenedObjectChanged { .. }
            | Self::WalReclamationCandidateCollision { .. }
            | Self::ManifestFileLength { .. }
            | Self::ManifestDatabaseIdMismatch { .. }
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
    let acquired = acquire_file_database_ownership(expected_database_id, layout, false)?;
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
    let acquired = acquire_file_database_ownership(expected_database_id, layout, true)?;
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

fn acquire_file_database_ownership<const N: usize>(
    expected_database_id: DatabaseId,
    layout: FileDatabaseLayout,
    require_stable_storage: bool,
) -> Result<AcquiredFileDatabaseOwnership<N>, FileDatabaseOwnershipOpenError> {
    PageLayout::for_const::<N>().map_err(FileDatabaseOwnershipOpenError::PageWidth)?;

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

    let checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::open(layout.restart_checkpoint())
            .map_err(FileDatabaseOwnershipOpenError::RestartCheckpointOpen)?;
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
    if length.len() != super::DATABASE_MANIFEST_V1_LENGTH as u64 {
        return Err(FileDatabaseOwnershipOpenError::ManifestFileLength {
            actual: length.len(),
        });
    }
    let mut bytes = [0_u8; super::DATABASE_MANIFEST_V1_LENGTH];
    read_exact(file, FileDatabaseLockRole::Manifest, &mut bytes)?;
    decode_database_manifest(&bytes).map_err(FileDatabaseOwnershipOpenError::Manifest)
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
