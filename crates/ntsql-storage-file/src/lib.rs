//! Versioned filesystem-backed commit-log adapter for ntsql transaction and page records.
//!
//! The caller supplies one existing parent directory, one trusted file path, and
//! one stable [`PersistentLogId`]. The adapter holds a nonblocking advisory
//! exclusive file lock for its lifetime; excluding non-cooperating writers
//! remains an outer trust boundary.
//!
//! ## Format v1
//!
//! The file begins with one immutable 64-byte header followed by zero or more
//! fixed-size 56-byte frames. Every multibyte field is big-endian.
//!
//! Header bytes:
//!
//! - `0..8`   : 8-byte magic
//! - `8..10`  : `u16` version (`1`)
//! - `10..12` : `u16` header length (`64`)
//! - `12..16` : `u32` flags (`0`)
//! - `16..32` : nonzero `u128` persistent lineage ID
//! - `32..56` : reserved zeros
//! - `56..64` : checksum of bytes `0..56`
//!
//! Frame bytes:
//!
//! - `0..4`   : 4-byte magic
//! - `4..6`   : `u16` kind
//! - `6..8`   : `u16` version (`1`)
//! - `8..12`  : `u32` flags (`0`)
//! - `12..14` : `u16` frame length (`56`)
//! - `14..16` : reserved zero `u16`
//! - `16..24` : payload 0
//! - `24..32` : payload 1
//! - `32..40` : payload 2
//! - `40..48` : reserved zeros
//! - `48..56` : checksum of bytes `0..48`
//!
//! Frame kind payloads:
//!
//! - epoch allocation    : `(epoch, 0, 0)`
//! - commit record       : `(position, epoch, sequence)`
//! - durable-through     : `(position, 0, 0)`
//!
//! ## Format v2
//!
//! Version 2 keeps the same 64-byte header size, 56-byte frame size, and
//! checksum algorithm. The header stores one nonzero exact page width at bytes
//! `32..40`. Frame kinds `1..3` retain their v1 payloads and add page-capable
//! kinds `4` (page header) and `5` (page data).
//!
//! A page header frame payload is `(position, page_number, page_version)`. It is
//! followed immediately by exactly `ceil(page_width / 8)` page-data frames. Each
//! page-data frame payload is `(position, chunk_index, raw_8_bytes)`. The final
//! chunk must zero-pad every unused byte.
//!
//! The separate page-store file reuses the same checksum and fixed-size framing
//! discipline with its own header magic (`NTSQPGS1`) and frame magic (`NTSP`).
//! Each durable snapshot is one contiguous group: snapshot-header,
//! required-position, then exactly `ceil(page_width / 8)` page-data frames.
//! Open recovery truncates only a final incomplete physical frame or final
//! incomplete snapshot group; complete malformed groups are rejected without
//! truncation.
//!
//! ## Format v3
//!
//! Version 3 preserves the v2 header body and kinds `1..5`, then adds
//! transaction-owned full-image pages. Kind `6` is an owned-page header with
//! payload `(position, page_number, page_version)`. It is followed immediately
//! by exactly one kind `7` owner frame with payload
//! `(position, transaction_epoch, transaction_sequence)`, then by the existing
//! kind `5` page-data frames.
//!
//! The distinct owned-page header prevents a missing owner frame from silently
//! turning transaction-owned data into a valid nontransactional page record.
//! Open recovery truncates an incomplete final owned-page group to its kind `6`
//! offset; a complete malformed or interrupted group is corruption.
//!
//! ## Database child formats
//!
//! Database-aware creation uses one common 48-byte child identity extension:
//! `NTSQCFI1`, version `1`, length `48`, exact role at byte `12`, reserved
//! `13..16`, nonzero database ID at `16..32`, and nonzero file ID at `32..48`.
//! Lifecycle generation is intentionally absent because it belongs to manifest
//! publication rather than immutable child identity. Each enclosing header
//! checksum protects the extension.
//!
//! WAL V5 has a 192-byte header. It preserves V4 recovery-coordinate geometry
//! through byte `119`, uses byte `76` to distinguish initial zeroed coordinates
//! from present reclamation metadata, reserves `120..128`, stores the child
//! extension at `128..176`, reserves `176..184`, and checksums `0..184` into
//! `184..192`. Reclamation remains V5, advances its independent WAL generation,
//! and preserves the child extension. Frames retain V3 encoding.
//!
//! Page-store V2 has a 128-byte header. It preserves V1 lineage and page-width
//! fields, reserves `40..64`, stores the child extension at `64..112`, reserves
//! `112..120`, and checksums `0..120` into `120..128`. Snapshot frames retain V1
//! encoding. Completeness-control V2 uses the same 128-byte extension/reserved/
//! checksum geometry and preserves its V1 slot-publication protocol.
//!
//! ## Checksum
//!
//! The v1/v2/v3 checksum is an ntsql-owned, deterministic, non-cryptographic
//! function. It starts from `0x4e5453514c434b31` and, for each protected byte in
//! order, applies:
//!
//! 1. `state ^= u64::from(byte)`
//! 2. `state = state.wrapping_mul(0x4e5453514c57414d)`
//! 3. `state = state.rotate_left(7) ^ 0x434845434b53554d`
//!
//! The seed is `0x4e5453514c434b31`. The final stored checksum is
//! `state ^ protected_len`, where `protected_len` is counted as a wrapping `u64`
//! while the bytes are folded.
//!
//! ## Restart checkpoint baseline codec v1
//!
//! The pure checkpoint codec uses its own `NTSQCKP1` header and `NTSQCKE1`
//! footer namespace. A 64-byte header is followed by fixed 64-byte transaction
//! entries and one 16-byte footer. Explicit presence bytes preserve absent
//! versus present-zero fields, while the final checksum protects the complete
//! preceding blob. Decoding returns owned untrusted fields and performs no file
//! I/O, publication, startup selection, or checkpoint validation.
//!
//! ## Restart checkpoint completeness baseline codec v1
//!
//! [`restart_checkpoint_completeness_codec`] is a second, completely
//! independent pure-memory codec for ADR 0048's
//! `DurableTransactionRestartCheckpointCompletenessBaseline`. It uses its own
//! `NTSQCMP1` header and `NTSQCME1` footer namespace; it does not extend,
//! wrap, or reinterpret the ADR 0044 `NTSQCKP1` bytes above, and encoding or
//! decoding one format never touches the other's bytes or module.
//!
//! A fixed 128-byte header is followed by fixed 64-byte transaction entries
//! (retaining ADR 0044's exact field layout as an independent copy), then
//! fixed 64-byte page entries, then one 16-byte footer. The header carries the
//! exact geometry, raw persistent ID, baseline frontier payload and presence,
//! transaction/page counts, total length, and the replay kind plus
//! independent optional frontier/position/cause fields and cause payloads.
//! Each page entry independently retains its page number, optional
//! required-image payload/kind, optional stored position, and state
//! discriminator; unused absent or raw-kind fields are structurally canonical
//! zero, as are unused replay payloads. Decoding validates every discriminant,
//! presence bit, canonical-zero field, and reserved byte, plus the complete
//! blob checksum, before returning only an owned untrusted completeness
//! observation. It never sorts, deduplicates, infers, compares, or authorizes
//! semantic relationships.
//!
//! The independent database-manifest codec uses one fixed 160-byte
//! `NTSQDBM1`/`NTSQDBE1` frame. It binds repository-owned database, lifecycle,
//! child-file, persistent-WAL, required-format, and required-feature identities.
//! Decoding validates the complete frame and returns only an inert
//! `ntsql_database::DatabaseManifest`; it performs no file I/O, publication,
//! locking, recovery, or live-authority transition.
//!
//! ## Database-wide ownership
//!
//! [`open_file_database_ownership`] extends the existing fixed
//! WAL/page-store/completeness lock order with an immutable database-owner
//! control file and the selected manifest. The owner control is the stable
//! cooperative lock across later manifest-inode replacement. Opened child
//! adapters validate exact manifest format and lineage requirements while
//! retaining their complete unrecovered state. The legacy-compatible opener
//! returns a private manifest-selected wrapper. The successor-only exact opener
//! additionally validates each physically parsed child identity and returns
//! recovery-required authority while retaining every lock.
//!
//! ## Filesystem restart checkpoint baseline source
//!
//! One caller-selected checkpoint slot directory contains an immutable
//! lineaged `control` file and an optional `current` ADR 0044 blob. The adapter
//! holds a nonblocking exclusive lock on `control`, not `current`, so later
//! replacement of the selected blob cannot move the lock to an obsolete inode.
//! Creating a slot synchronizes the control file, slot directory, and parent
//! directory. Loading structurally decodes one complete current blob but returns
//! only owned untrusted checkpoint fields. Publication writes and synchronizes
//! one fresh unselected `candidate`, closes it, renames it over `current`, and
//! synchronizes the retained directory before reporting success.
//!
//! ## Filesystem restart checkpoint completeness baseline source
//!
//! [`FileRestartCheckpointCompletenessBaselineSource`] owns a second,
//! completely separate caller-selected slot directory for ADR 0051's
//! completeness ports. It never opens, empties, or overwrites the
//! transaction-only slot above, and the transaction-only adapter never reads
//! its entries.
//!
//! The two namespaces are distinguished by independent control magic:
//! the transaction-only slot's `control` starts with `NTSQCKS1`, while the
//! completeness slot's `control` starts with `NTSQCMS1`. Legacy creation uses
//! version `1`, header length `64`, zero flags, a nonzero persistent log ID,
//! reserved zero bytes, and the final checksum. Database-aware completeness
//! creation uses the V2 geometry described above. Opening one slot type as the
//! other fails at the control header magic even when the optional `current`
//! entry is absent.
//!
//! Inside the completeness slot the selected `current` entry holds only
//! `NTSQCMP1` bytes and the fixed unselected `candidate` entry holds only
//! in-progress publication bytes. Creation, lock-before-parse open, exact
//! optional reading, and the candidate/rename/directory-synchronization
//! publication sequence match the transaction-only slot's reviewed discipline,
//! but each format decodes strictly through its own codec.

use std::{
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    num::NonZeroU64,
    path::{Path, PathBuf},
};

use ntsql_database::{DatabaseFileHeaderIdentity, DatabaseStorageIdentity};
use ntsql_page::{
    DurablePageWalObservation, PageLog, PageNumber, PageRecoveryObservationBytesError, PageVersion,
    StoredPageSnapshotObservation, UnloggedPage,
};
use ntsql_transaction::{
    CommittedTransactionPageRecoveryWritePermit, DurableCommitLookup,
    DurableCommittedTransactionPageRecoveryCandidate,
    DurableCommittedTransactionPageRecoveryComparison,
    DurableCommittedTransactionPageRecoveryComparisonError, DurablePageStoreInventorySource,
    DurableTransactionCommitObservation, DurableTransactionCommitObservationFieldsError,
    DurableTransactionPageObservation, DurableTransactionPageObservationBytesError,
    DurableTransactionRestartCheckpointPageRepairCandidate,
    DurableTransactionRestartCheckpointPageRepairComparison,
    DurableTransactionRestartCheckpointPageRepairComparisonError,
    DurableTransactionRestartCheckpointPageRepairTargetKind, DurableTransactionRestartObservation,
    DurableTransactionRestartPrunedGenerationSource,
    DurableTransactionRestartRetentionMetadataObservation,
    DurableTransactionRestartRetentionMetadataSource,
    DurableTransactionRestartWalReclamationEffectObservation,
    DurableTransactionRestartWalReclamationPermit,
    DurableTransactionRestartWalReclamationReplacementObservation,
    DurableTransactionRestartWalReclamationSource,
    DurableTransactionRestartWalReclamationSourceObservation, TransactionCommitRecord,
    TransactionEpochSource, TransactionId, TransactionPageLog, TransactionPageWriteRecord,
    TransactionRecoverySource, TransactionRestartCheckpointPageRepairStore,
    TransactionRestartCheckpointPageRepairWritePermit,
    TransactionRestartCoordinatorEpochAllocationError, TransactionRestartCoordinatorEpochSource,
    UnrecoveredTransactionPageStorage, compare_committed_transaction_page_recovery_candidate,
    compare_transaction_restart_checkpoint_page_repair_candidate,
};
use ntsql_wal::{CommitLog, LogDurability, LogLineage, LogSequenceNumber, PersistentLogId};

mod database_child_identity_codec;
mod database_manifest_codec;
mod database_ownership;
mod restart_checkpoint_codec;
mod restart_checkpoint_completeness_codec;
mod restart_checkpoint_completeness_file;
mod restart_checkpoint_file;

pub use database_child_identity_codec::{
    DatabaseChildIdentityDecodeError, DatabaseChildIdentityDecodeErrorReason,
};
pub use database_manifest_codec::{
    DATABASE_MANIFEST_V1_LENGTH, DatabaseManifestDecodeError, decode_database_manifest,
    encode_database_manifest,
};
pub use database_ownership::{
    DATABASE_OWNER_CONTROL_V1_LENGTH, DatabaseOwnerControlDecodeError, FileDatabaseCreateBoundary,
    FileDatabaseCreateEntry, FileDatabaseCreateError, FileDatabaseCreateFault,
    FileDatabaseCreateFaultTiming, FileDatabaseCreateIoError, FileDatabaseCreateIoStage,
    FileDatabaseCreateLocation, FileDatabaseCreateManifestError,
    FileDatabaseCreateManifestMismatch, FileDatabaseCreateNamespaceEvidence,
    FileDatabaseCreateOutcome, FileDatabaseCreatePhase, FileDatabaseLayout,
    FileDatabaseLiveOpenError, FileDatabaseLockRole, FileDatabaseOpenPhase, FileDatabaseOwnership,
    FileDatabaseOwnershipIoError, FileDatabaseOwnershipIoStage, FileDatabaseOwnershipOpenError,
    FileDatabaseOwnershipSelection, LiveFileDatabase, RecoveredFileDatabaseOuterOwnership,
    RecoveryRequiredFileDatabase, create_file_database, decode_database_owner_control,
    encode_database_owner_control, open_file_database_ownership, open_live_file_database,
    open_live_file_database_with_observer, open_recovery_required_file_database,
};
pub use restart_checkpoint_codec::{
    RestartCheckpointBaselineDecodeError, RestartCheckpointBaselineEncodeError,
    RestartCheckpointBaselineEntryOptionalField, decode_restart_checkpoint_baseline,
    encode_restart_checkpoint_baseline,
};
pub use restart_checkpoint_completeness_codec::{
    RestartCheckpointCompletenessBaselineDecodeError,
    RestartCheckpointCompletenessBaselineEncodeError,
    RestartCheckpointCompletenessBaselineEntryOptionalField,
    RestartCheckpointCompletenessBaselineReplayCauseField,
    RestartCheckpointCompletenessBaselineRequiredImageField,
    decode_restart_checkpoint_completeness_baseline,
    encode_restart_checkpoint_completeness_baseline,
};
pub use restart_checkpoint_completeness_file::{
    FileRestartCheckpointCompletenessBaselinePublicationError,
    FileRestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed,
    FileRestartCheckpointCompletenessBaselinePublicationFaultPoint,
    FileRestartCheckpointCompletenessBaselineSource,
    FileRestartCheckpointCompletenessBaselineSourceError,
    FileTransactionPageStorageCompletenessCheckpointOpenError,
    FileTransactionPageStorageRestartCheckpointCompletenessSelection,
    UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint,
    open_transaction_page_storage_with_completeness_checkpoint,
};
pub use restart_checkpoint_file::{
    FileRestartCheckpointBaselinePublicationError,
    FileRestartCheckpointBaselinePublicationFaultAlreadyArmed,
    FileRestartCheckpointBaselinePublicationFaultPoint, FileRestartCheckpointBaselineSource,
    FileRestartCheckpointBaselineSourceError, FileRestartCheckpointSlotCreateError,
    FileRestartCheckpointSlotFormatError, FileRestartCheckpointSlotFormatErrorReason,
    FileRestartCheckpointSlotIoError, FileRestartCheckpointSlotIoStage,
    FileRestartCheckpointSlotOpenError, FileTransactionPageStorageCheckpointOpenError,
    UnrecoveredFileTransactionPageStorageWithCheckpoint,
    open_transaction_page_storage_with_checkpoint,
};

const HEADER_MAGIC: [u8; 8] = *b"NTSQLOG1";
const FRAME_MAGIC: [u8; 4] = *b"NTSQ";
const FORMAT_VERSION_V1: u16 = 1;
const FORMAT_VERSION_V2: u16 = 2;
const FORMAT_VERSION_V3: u16 = 3;
const FORMAT_VERSION_V4: u16 = 4;
const FORMAT_VERSION_V5: u16 = 5;
const HEADER_LENGTH: usize = 64;
const HEADER_LENGTH_U16: u16 = 64;
const HEADER_LENGTH_U64: u64 = 64;
const FRAME_LENGTH: usize = 56;
const FRAME_LENGTH_U16: u16 = 56;
const FRAME_LENGTH_U64: u64 = 56;
const HEADER_CHECKSUM_OFFSET: usize = 56;
const HEADER_V2_PAGE_WIDTH_OFFSET: usize = 32;
const HEADER_V2_RESERVED_OFFSET: usize = 40;
const HEADER_V4_LENGTH: usize = 128;
const HEADER_V4_LENGTH_U16: u16 = 128;
const HEADER_V4_LENGTH_U64: u64 = 128;
const HEADER_V4_GENERATION_OFFSET: usize = 40;
const HEADER_V4_RETAINED_FIRST_OFFSET: usize = 48;
const HEADER_V4_LOGICAL_HIGH_WATER_OFFSET: usize = 56;
const HEADER_V4_ALLOCATED_EPOCH_HIGH_WATER_OFFSET: usize = 64;
const HEADER_V4_ANCHOR_VERSION_OFFSET: usize = 72;
const HEADER_V4_RETAINED_FIRST_PRESENCE_OFFSET: usize = 74;
const HEADER_V4_LOGICAL_HIGH_WATER_PRESENCE_OFFSET: usize = 75;
const HEADER_V4_ANCHOR_VALUE_OFFSET: usize = 80;
const HEADER_V4_RESERVED_START: usize = 76;
const HEADER_V4_RESERVED_MIDDLE: usize = 96;
const HEADER_V4_CHECKSUM_OFFSET: usize = 120;
const HEADER_V5_LENGTH: usize = 192;
const HEADER_V5_LENGTH_U16: u16 = 192;
const HEADER_V5_LENGTH_U64: u64 = 192;
const HEADER_V5_RECLAMATION_PRESENCE_OFFSET: usize = 76;
const HEADER_V5_IDENTITY_OFFSET: usize = 128;
const HEADER_V5_RESERVED_END: usize = 184;
const HEADER_V5_CHECKSUM_OFFSET: usize = 184;
const FRAME_CHECKSUM_OFFSET: usize = 48;
#[cfg(test)]
const FRAME_CHECKSUM_OFFSET_U64: u64 = 48;
const CHECKSUM_SEED: u64 = 0x4e54_5351_4c43_4b31;
const CHECKSUM_MULTIPLIER: u64 = 0x4e54_5351_4c57_414d;
const CHECKSUM_XOR: u64 = 0x4348_4543_4b53_554d;
const PAGE_CHUNK_WIDTH: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogFormat {
    V1,
    V2,
    V3,
    V4,
    V5,
}

impl LogFormat {
    const fn version(self) -> u16 {
        match self {
            Self::V1 => FORMAT_VERSION_V1,
            Self::V2 => FORMAT_VERSION_V2,
            Self::V3 => FORMAT_VERSION_V3,
            Self::V4 => FORMAT_VERSION_V4,
            Self::V5 => FORMAT_VERSION_V5,
        }
    }

    const fn frame_version(self) -> u16 {
        match self {
            Self::V4 | Self::V5 => FORMAT_VERSION_V3,
            Self::V1 | Self::V2 | Self::V3 => self.version(),
        }
    }

    const fn supports_pages(self) -> bool {
        match self {
            Self::V1 => false,
            Self::V2 | Self::V3 | Self::V4 | Self::V5 => true,
        }
    }

    const fn supports_transaction_pages(self) -> bool {
        matches!(self, Self::V3 | Self::V4 | Self::V5)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PageLayout {
    width_u64: u64,
    chunk_count: usize,
    final_chunk_len: usize,
}

impl PageLayout {
    fn for_const<const N: usize>() -> Result<Self, FilePageWidthError> {
        if N == 0 {
            return Err(FilePageWidthError::Zero);
        }
        let width_u64 =
            u64::try_from(N).map_err(|_| FilePageWidthError::NotRepresentable { actual: N })?;
        let chunk_count = match N.checked_add(PAGE_CHUNK_WIDTH - 1) {
            Some(sum) => sum / PAGE_CHUNK_WIDTH,
            None => return Err(FilePageWidthError::NotRepresentable { actual: N }),
        };
        let final_chunk_len = match N % PAGE_CHUNK_WIDTH {
            0 => PAGE_CHUNK_WIDTH,
            remainder => remainder,
        };
        Ok(Self {
            width_u64,
            chunk_count,
            final_chunk_len,
        })
    }

    fn logical_bytes_for_chunk(self, chunk_index: usize) -> usize {
        if chunk_index + 1 == self.chunk_count {
            self.final_chunk_len
        } else {
            PAGE_CHUNK_WIDTH
        }
    }
}

/// One-shot physical-effect boundary for the next matching log operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultPoint {
    /// Fail before an append changes the file.
    BeforeAppend,
    /// Append one complete unmarked record, then report append failure.
    AfterAppend,
    /// Fail before the next durability marker work begins.
    BeforeFlush,
    /// Persist the durability marker and advance state, then report failure.
    AfterFlush,
    /// Fail before reconciling the fixed reclamation candidate.
    BeforeReclamationCandidateCleanup,
    /// Fail after locking the replacement candidate but before writing it.
    BeforeReclamationWrite,
    /// Fail after beginning to write the replacement candidate.
    DuringReclamationCopy,
    /// Fail after writing the candidate but before synchronizing it.
    BeforeReclamationCandidateSync,
    /// Synchronize the candidate, then report failure.
    AfterReclamationCandidateSync,
    /// Fail after synchronizing the candidate but before rename.
    BeforeReclamationRename,
    /// Rename the candidate over the selected path, then report failure.
    AfterReclamationRename,
    /// Rename the candidate but fail before parent-directory synchronization.
    DuringReclamationDirectorySync,
}

impl fmt::Display for FaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeAppend => formatter.write_str("before append"),
            Self::AfterAppend => formatter.write_str("after append"),
            Self::BeforeFlush => formatter.write_str("before flush"),
            Self::AfterFlush => formatter.write_str("after flush"),
            Self::BeforeReclamationCandidateCleanup => {
                formatter.write_str("before reclamation candidate cleanup")
            }
            Self::BeforeReclamationWrite => {
                formatter.write_str("before reclamation candidate write")
            }
            Self::DuringReclamationCopy => formatter.write_str("during reclamation copy"),
            Self::BeforeReclamationCandidateSync => {
                formatter.write_str("before reclamation candidate sync")
            }
            Self::AfterReclamationCandidateSync => {
                formatter.write_str("after reclamation candidate sync")
            }
            Self::BeforeReclamationRename => {
                formatter.write_str("before reclamation candidate rename")
            }
            Self::AfterReclamationRename => {
                formatter.write_str("after reclamation candidate rename")
            }
            Self::DuringReclamationDirectorySync => {
                formatter.write_str("during reclamation directory sync")
            }
        }
    }
}

/// Refusal to silently replace an already armed one-shot fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultAlreadyArmed {
    armed: FaultPoint,
    requested: FaultPoint,
}

impl FaultAlreadyArmed {
    /// Returns the fault that remains armed.
    #[must_use]
    pub const fn armed(&self) -> FaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> FaultPoint {
        self.requested
    }
}

impl fmt::Display for FaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for FaultAlreadyArmed {}

/// Page-width validation failure for page-capable v2 operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilePageWidthError {
    Zero,
    NotRepresentable { actual: usize },
}

impl fmt::Display for FilePageWidthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => formatter.write_str("page width must be nonzero for the v2 file format"),
            Self::NotRepresentable { actual } => write!(
                formatter,
                "page width {actual} does not fit the persistent v2 file format"
            ),
        }
    }
}

impl Error for FilePageWidthError {}

/// Exact I/O boundary that reported failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileIoStage {
    CreateFile,
    OpenFile,
    AcquireExclusiveLock,
    OpenParentDirectory,
    ReadMetadata,
    ReadHeader,
    ReadFrame,
    WriteHeader,
    SyncCreatedFile,
    SyncOpenedFile,
    SyncParentDirectory,
    TruncateIncompleteTail,
    SyncTruncatedTail,
    SeekEnd,
    WriteEpochFrame,
    SyncEpochFrame,
    WriteCommitFrame,
    WritePageHeaderFrame,
    WriteTransactionPageHeaderFrame,
    WriteTransactionPageOwnerFrame,
    WritePageDataFrame,
    SyncCommitPrefix,
    WriteDurableMarker,
    SyncDurableMarker,
    ReadReclamationCandidateMetadata,
    RemoveReclamationCandidate,
    CreateReclamationCandidate,
    AcquireReclamationCandidateLock,
    WriteReclamationHeader,
    WriteReclamationFrame,
    SyncReclamationCandidate,
    RenameReclamationCandidate,
    SyncReclamationDirectory,
}

impl fmt::Display for FileIoStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateFile => formatter.write_str("creating the commit-log file"),
            Self::OpenFile => formatter.write_str("opening the commit-log file"),
            Self::AcquireExclusiveLock => {
                formatter.write_str("acquiring the exclusive commit-log file lock")
            }
            Self::OpenParentDirectory => formatter.write_str("opening the parent directory"),
            Self::ReadMetadata => formatter.write_str("reading commit-log metadata"),
            Self::ReadHeader => formatter.write_str("reading the commit-log header"),
            Self::ReadFrame => formatter.write_str("reading a commit-log frame"),
            Self::WriteHeader => formatter.write_str("writing the commit-log header"),
            Self::SyncCreatedFile => formatter.write_str("synchronizing the created file"),
            Self::SyncOpenedFile => formatter.write_str("synchronizing the opened file"),
            Self::SyncParentDirectory => formatter.write_str("synchronizing the parent directory"),
            Self::TruncateIncompleteTail => {
                formatter.write_str("truncating an incomplete tail frame or record")
            }
            Self::SyncTruncatedTail => formatter.write_str("synchronizing a repaired file tail"),
            Self::SeekEnd => formatter.write_str("seeking to the end of the commit-log file"),
            Self::WriteEpochFrame => formatter.write_str("writing an epoch-allocation frame"),
            Self::SyncEpochFrame => formatter.write_str("synchronizing an epoch-allocation frame"),
            Self::WriteCommitFrame => formatter.write_str("writing a commit frame"),
            Self::WritePageHeaderFrame => formatter.write_str("writing a page-header frame"),
            Self::WriteTransactionPageHeaderFrame => {
                formatter.write_str("writing a transaction-page-header frame")
            }
            Self::WriteTransactionPageOwnerFrame => {
                formatter.write_str("writing a transaction-page-owner frame")
            }
            Self::WritePageDataFrame => formatter.write_str("writing a page-data frame"),
            Self::SyncCommitPrefix => {
                formatter.write_str("synchronizing the requested durable prefix")
            }
            Self::WriteDurableMarker => formatter.write_str("writing a durable-through marker"),
            Self::SyncDurableMarker => {
                formatter.write_str("synchronizing a durable-through marker")
            }
            Self::ReadReclamationCandidateMetadata => {
                formatter.write_str("reading reclamation candidate metadata")
            }
            Self::RemoveReclamationCandidate => {
                formatter.write_str("removing the fixed reclamation candidate")
            }
            Self::CreateReclamationCandidate => {
                formatter.write_str("creating the fixed reclamation candidate")
            }
            Self::AcquireReclamationCandidateLock => {
                formatter.write_str("acquiring the reclamation candidate lock")
            }
            Self::WriteReclamationHeader => formatter.write_str("writing the reclaimed WAL header"),
            Self::WriteReclamationFrame => {
                formatter.write_str("writing a retained reclamation frame")
            }
            Self::SyncReclamationCandidate => {
                formatter.write_str("synchronizing the reclamation candidate")
            }
            Self::RenameReclamationCandidate => {
                formatter.write_str("renaming the reclamation candidate over the selected WAL")
            }
            Self::SyncReclamationDirectory => {
                formatter.write_str("synchronizing the reclamation parent directory")
            }
        }
    }
}

/// I/O failure paired with the exact adapter stage that reported it.
#[derive(Debug)]
pub struct FileIoError {
    stage: FileIoStage,
    source: io::Error,
}

impl FileIoError {
    fn new(stage: FileIoStage, source: io::Error) -> Self {
        Self { stage, source }
    }

    /// Returns the adapter stage that reported the I/O error.
    #[must_use]
    pub const fn stage(&self) -> FileIoStage {
        self.stage
    }

    /// Returns the original `std::io::Error`.
    #[must_use]
    pub const fn io_source(&self) -> &io::Error {
        &self.source
    }
}

impl PartialEq for FileIoError {
    fn eq(&self, other: &Self) -> bool {
        self.stage == other.stage
            && self.source.kind() == other.source.kind()
            && self.source.raw_os_error() == other.source.raw_os_error()
    }
}

impl Eq for FileIoError {}

impl fmt::Display for FileIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} failed: {}", self.stage, self.source)
    }
}

impl Error for FileIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Exact malformed-format reason reported by [`FileCommitLog::open`] or
/// [`FileCommitLog::open_page_capable`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileFormatErrorReason {
    HeaderTooShort {
        actual: u64,
    },
    HeaderMagic,
    HeaderVersion {
        actual: u16,
    },
    HeaderLength {
        actual: u16,
    },
    HeaderV4Length {
        actual: u16,
    },
    HeaderV5Length {
        actual: u16,
    },
    HeaderFlags {
        actual: u32,
    },
    HeaderPageWidthZero,
    HeaderPageWidthMismatch {
        expected: u64,
        actual: u64,
    },
    HeaderReserved,
    HeaderV4GenerationZero,
    HeaderV4RetainedFirstPresence {
        actual: u8,
    },
    HeaderV4LogicalHighWaterPresence {
        actual: u8,
    },
    HeaderV4RetainedFirstZero,
    HeaderV4LogicalHighWaterZero,
    HeaderV4RetainedFirstWithoutHighWater,
    HeaderV4RetainedFirstBeyondHighWater {
        retained_first: u64,
        high_water: u64,
    },
    HeaderV4AllocatedEpochHighWaterZero,
    HeaderV4AnchorVersionZero,
    HeaderV4Reserved,
    HeaderV5ReclamationPresence {
        actual: u8,
    },
    HeaderDatabaseChildIdentity(DatabaseChildIdentityDecodeErrorReason),
    HeaderV4RetainedFirstMismatch {
        expected: Option<u64>,
        actual: Option<u64>,
    },
    HeaderV4LogicalHighWaterMismatch {
        expected: Option<u64>,
        actual: Option<u64>,
    },
    HeaderChecksum {
        expected: u64,
        actual: u64,
    },
    LineageIdZero,
    FrameMagic,
    FrameKind {
        actual: u16,
    },
    FrameVersion {
        actual: u16,
    },
    FrameLength {
        actual: u16,
    },
    FrameFlags {
        actual: u32,
    },
    FrameReserved,
    FrameChecksum {
        expected: u64,
        actual: u64,
    },
    UnexpectedNonzeroPayload {
        field: &'static str,
        actual: u64,
    },
    EpochValueZero,
    EpochOutOfSequence {
        expected: u64,
        actual: u64,
    },
    EpochSpaceExhausted,
    CommitPositionZero,
    CommitPositionOutOfSequence {
        expected: u64,
        actual: u64,
    },
    CommitPositionSpaceExhausted,
    CommitEpochZero,
    CommitEpochUnallocated {
        actual: u64,
        highest_allocated: u64,
    },
    CommitSequenceZero,
    DuplicateTransactionIdentity {
        epoch: u64,
        sequence: u64,
    },
    MarkerPositionZero,
    MarkerDoesNotAdvance {
        previous: u64,
        actual: u64,
    },
    MarkerReferencesUnknownCommit {
        actual: u64,
        highest_committed: u64,
    },
    PageHeaderPositionZero,
    PageHeaderPositionOutOfSequence {
        expected: u64,
        actual: u64,
    },
    PageHeaderPositionSpaceExhausted,
    PageNumberZero,
    TransactionPageOwnerWithoutHeader,
    TransactionPageOwnerInterruptedByFrameKind {
        actual: u16,
    },
    TransactionPageOwnerDuplicate,
    TransactionPageOwnerParentPositionZero,
    TransactionPageOwnerParentMismatch {
        expected: u64,
        actual: u64,
    },
    TransactionPageOwnerEpochZero,
    TransactionPageOwnerEpochUnallocated {
        actual: u64,
        highest_allocated: u64,
    },
    TransactionPageOwnerSequenceZero,
    TransactionPageOwnerMissing,
    PageDataWithoutHeader,
    PageDataParentPositionZero,
    PageDataParentMismatch {
        expected: u64,
        actual: u64,
    },
    PageDataChunkIndexOutOfSequence {
        expected: u64,
        actual: u64,
    },
    PageDataInterruptedByFrameKind {
        actual: u16,
    },
    PageDataFinalPaddingNonzero,
}

impl fmt::Display for FileFormatErrorReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTooShort { actual } => {
                write!(formatter, "header is shorter than 64 bytes: found {actual}")
            }
            Self::HeaderMagic => formatter.write_str("header magic does not match ntsql format"),
            Self::HeaderVersion { actual } => {
                write!(formatter, "unsupported header version {actual}")
            }
            Self::HeaderLength { actual } => {
                write!(formatter, "header length {actual} does not equal 64")
            }
            Self::HeaderV4Length { actual } => {
                write!(formatter, "v4 header length {actual} does not equal 128")
            }
            Self::HeaderV5Length { actual } => {
                write!(formatter, "v5 header length {actual} does not equal 192")
            }
            Self::HeaderFlags { actual } => {
                write!(formatter, "header flags are nonzero: {actual}")
            }
            Self::HeaderPageWidthZero => formatter.write_str("v2 header page width is zero"),
            Self::HeaderPageWidthMismatch { expected, actual } => write!(
                formatter,
                "v2 header page width {actual} does not equal required width {expected}"
            ),
            Self::HeaderReserved => formatter.write_str("header reserved bytes are nonzero"),
            Self::HeaderV4GenerationZero => formatter.write_str("v4 WAL generation is zero"),
            Self::HeaderV4RetainedFirstPresence { actual } => write!(
                formatter,
                "v4 retained-first presence byte is not canonical: {actual}"
            ),
            Self::HeaderV4LogicalHighWaterPresence { actual } => write!(
                formatter,
                "v4 logical-high-water presence byte is not canonical: {actual}"
            ),
            Self::HeaderV4RetainedFirstZero => {
                formatter.write_str("v4 retained-first position is zero")
            }
            Self::HeaderV4LogicalHighWaterZero => {
                formatter.write_str("v4 logical high-water is zero")
            }
            Self::HeaderV4RetainedFirstWithoutHighWater => {
                formatter.write_str("v4 retained-first exists without a logical high-water")
            }
            Self::HeaderV4RetainedFirstBeyondHighWater {
                retained_first,
                high_water,
            } => write!(
                formatter,
                "v4 retained-first position {retained_first} exceeds logical high-water {high_water}"
            ),
            Self::HeaderV4AllocatedEpochHighWaterZero => {
                formatter.write_str("v4 allocated epoch high-water is zero")
            }
            Self::HeaderV4AnchorVersionZero => {
                formatter.write_str("v4 selected-checkpoint anchor version is zero")
            }
            Self::HeaderV4Reserved => formatter.write_str("v4 header reserved bytes are nonzero"),
            Self::HeaderV5ReclamationPresence { actual } => write!(
                formatter,
                "v5 reclamation-metadata presence byte is not canonical: {actual}"
            ),
            Self::HeaderDatabaseChildIdentity(source) => source.fmt(formatter),
            Self::HeaderV4RetainedFirstMismatch { expected, actual } => write!(
                formatter,
                "v4 retained-first metadata {expected:?} does not match retained records {actual:?}"
            ),
            Self::HeaderV4LogicalHighWaterMismatch { expected, actual } => write!(
                formatter,
                "v4 logical high-water metadata {expected:?} does not match durable records {actual:?}"
            ),
            Self::HeaderChecksum { expected, actual } => write!(
                formatter,
                "header checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
            Self::LineageIdZero => formatter.write_str("persistent lineage ID is zero"),
            Self::FrameMagic => formatter.write_str("frame magic does not match ntsql format"),
            Self::FrameKind { actual } => write!(formatter, "unknown frame kind {actual}"),
            Self::FrameVersion { actual } => {
                write!(formatter, "unsupported frame version {actual}")
            }
            Self::FrameLength { actual } => {
                write!(formatter, "frame length {actual} does not equal 56")
            }
            Self::FrameFlags { actual } => {
                write!(formatter, "frame flags are nonzero: {actual}")
            }
            Self::FrameReserved => formatter.write_str("frame reserved bytes are nonzero"),
            Self::FrameChecksum { expected, actual } => write!(
                formatter,
                "frame checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
            Self::UnexpectedNonzeroPayload { field, actual } => write!(
                formatter,
                "payload field {field} is nonzero when the frame kind requires zero: {actual}"
            ),
            Self::EpochValueZero => formatter.write_str("epoch allocation is zero"),
            Self::EpochOutOfSequence { expected, actual } => write!(
                formatter,
                "epoch allocation {actual} does not equal the next contiguous epoch {expected}"
            ),
            Self::EpochSpaceExhausted => formatter.write_str(
                "found another epoch allocation after the maximum persisted epoch was already used",
            ),
            Self::CommitPositionZero => formatter.write_str("commit position is zero"),
            Self::CommitPositionOutOfSequence { expected, actual } => write!(
                formatter,
                "commit position {actual} does not equal the next contiguous position {expected}"
            ),
            Self::CommitPositionSpaceExhausted => formatter.write_str(
                "found another commit record after the maximum persisted position was already used",
            ),
            Self::CommitEpochZero => formatter.write_str("commit epoch is zero"),
            Self::CommitEpochUnallocated {
                actual,
                highest_allocated,
            } => write!(
                formatter,
                "commit epoch {actual} was never allocated; highest allocated epoch is {highest_allocated}"
            ),
            Self::CommitSequenceZero => formatter.write_str("commit sequence is zero"),
            Self::DuplicateTransactionIdentity { epoch, sequence } => {
                write!(
                    formatter,
                    "duplicate transaction identity {epoch}:{sequence}"
                )
            }
            Self::MarkerPositionZero => {
                formatter.write_str("durable-through marker position is zero")
            }
            Self::MarkerDoesNotAdvance { previous, actual } => write!(
                formatter,
                "durable-through marker {actual} does not advance past {previous}"
            ),
            Self::MarkerReferencesUnknownCommit {
                actual,
                highest_committed,
            } => write!(
                formatter,
                "durable-through marker {actual} does not reference an earlier completed record; highest completed position is {highest_committed}"
            ),
            Self::PageHeaderPositionZero => formatter.write_str("page header position is zero"),
            Self::PageHeaderPositionOutOfSequence { expected, actual } => write!(
                formatter,
                "page header position {actual} does not equal the next contiguous position {expected}"
            ),
            Self::PageHeaderPositionSpaceExhausted => formatter.write_str(
                "found another page record after the maximum persisted position was already used",
            ),
            Self::PageNumberZero => formatter.write_str("page header page number is zero"),
            Self::TransactionPageOwnerWithoutHeader => formatter
                .write_str("transaction-page owner frame has no pending transaction-page header"),
            Self::TransactionPageOwnerInterruptedByFrameKind { actual } => write!(
                formatter,
                "transaction-page record expected an owner frame but found frame kind {actual}"
            ),
            Self::TransactionPageOwnerDuplicate => {
                formatter.write_str("transaction-page record has more than one owner frame")
            }
            Self::TransactionPageOwnerParentPositionZero => {
                formatter.write_str("transaction-page owner parent position is zero")
            }
            Self::TransactionPageOwnerParentMismatch { expected, actual } => write!(
                formatter,
                "transaction-page owner parent position {actual} does not match pending page position {expected}"
            ),
            Self::TransactionPageOwnerEpochZero => {
                formatter.write_str("transaction-page owner epoch is zero")
            }
            Self::TransactionPageOwnerEpochUnallocated {
                actual,
                highest_allocated,
            } => write!(
                formatter,
                "transaction-page owner epoch {actual} was never allocated; highest allocated epoch is {highest_allocated}"
            ),
            Self::TransactionPageOwnerSequenceZero => {
                formatter.write_str("transaction-page owner sequence is zero")
            }
            Self::TransactionPageOwnerMissing => {
                formatter.write_str("transaction-page record is missing its owner")
            }
            Self::PageDataWithoutHeader => {
                formatter.write_str("page data frame has no pending page header")
            }
            Self::PageDataParentPositionZero => {
                formatter.write_str("page data frame parent position is zero")
            }
            Self::PageDataParentMismatch { expected, actual } => write!(
                formatter,
                "page data frame parent position {actual} does not match pending page position {expected}"
            ),
            Self::PageDataChunkIndexOutOfSequence { expected, actual } => write!(
                formatter,
                "page data chunk index {actual} does not equal required contiguous chunk {expected}"
            ),
            Self::PageDataInterruptedByFrameKind { actual } => write!(
                formatter,
                "page record expected a page-data frame but found frame kind {actual}"
            ),
            Self::PageDataFinalPaddingNonzero => {
                formatter.write_str("page data frame final-chunk padding bytes are nonzero")
            }
        }
    }
}

/// Malformed-format error paired with the byte offset that reported it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileFormatError {
    offset: u64,
    reason: FileFormatErrorReason,
}

impl FileFormatError {
    fn new(offset: u64, reason: FileFormatErrorReason) -> Self {
        Self { offset, reason }
    }

    /// Returns the byte offset that reported the format error.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the exact malformed-format reason.
    #[must_use]
    pub const fn reason(&self) -> &FileFormatErrorReason {
        &self.reason
    }
}

impl fmt::Display for FileFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "format error at byte {}: {}",
            self.offset, self.reason
        )
    }
}

impl Error for FileFormatError {}

/// Failure while creating a new file commit log.
#[derive(Debug, Eq, PartialEq)]
pub enum FileCreateError {
    MissingParentDirectory,
    PageWidth(FilePageWidthError),
    Io(FileIoError),
}

impl fmt::Display for FileCreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParentDirectory => {
                formatter.write_str("commit-log path does not have an existing parent directory")
            }
            Self::PageWidth(source) => source.fmt(formatter),
            Self::Io(source) => source.fmt(formatter),
        }
    }
}

impl Error for FileCreateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingParentDirectory => None,
            Self::PageWidth(source) => Some(source),
            Self::Io(source) => Some(source),
        }
    }
}

/// Failure while opening an existing file commit log.
#[derive(Debug, Eq, PartialEq)]
pub enum FileOpenError {
    PageWidth(FilePageWidthError),
    Io(FileIoError),
    Format(FileFormatError),
    RecordCapacityExhausted,
}

impl fmt::Display for FileOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PageWidth(source) => source.fmt(formatter),
            Self::Io(source) => source.fmt(formatter),
            Self::Format(source) => source.fmt(formatter),
            Self::RecordCapacityExhausted => {
                formatter.write_str("commit-log record snapshot capacity is exhausted")
            }
        }
    }
}

impl Error for FileOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PageWidth(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::Format(source) => Some(source),
            Self::RecordCapacityExhausted => None,
        }
    }
}

/// Failure while executing a file-backed commit-log operation.
#[derive(Debug, Eq, PartialEq)]
pub enum FileCommitLogError {
    InjectedFault(FaultPoint),
    PageWidth(FilePageWidthError),
    PageSupportUnavailable,
    TransactionPageSupportUnavailable,
    PageWidthMismatch { expected: usize, actual: usize },
    ForeignPageLineage(PageNumber),
    Io(FileIoError),
    PoisonedWriter,
    UnknownFlushPosition(LogSequenceNumber),
    ForeignFlushPosition(LogSequenceNumber),
    RecordCapacityExhausted,
    PositionSpaceExhausted,
}

impl fmt::Display for FileCommitLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InjectedFault(point) => write!(formatter, "injected commit-log failure {point}"),
            Self::PageWidth(source) => source.fmt(formatter),
            Self::PageSupportUnavailable => {
                formatter.write_str("this file commit-log format does not support page records")
            }
            Self::TransactionPageSupportUnavailable => formatter.write_str(
                "this file commit-log format does not support transaction-owned page records",
            ),
            Self::PageWidthMismatch { expected, actual } => write!(
                formatter,
                "page width {actual} does not match log page width {expected}"
            ),
            Self::ForeignPageLineage(page_number) => write!(
                formatter,
                "commit-log page {} belongs to another lineage",
                page_number.get()
            ),
            Self::Io(source) => source.fmt(formatter),
            Self::PoisonedWriter => formatter
                .write_str("commit-log writer is poisoned; reopen the file before retrying"),
            Self::UnknownFlushPosition(position) => write!(
                formatter,
                "commit-log position {} was not appended",
                position.get()
            ),
            Self::ForeignFlushPosition(position) => write!(
                formatter,
                "commit-log position {} belongs to another lineage",
                position.get()
            ),
            Self::RecordCapacityExhausted => {
                formatter.write_str("commit-log record snapshot capacity is exhausted")
            }
            Self::PositionSpaceExhausted => {
                formatter.write_str("commit-log position space is exhausted")
            }
        }
    }
}

impl Error for FileCommitLogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PageWidth(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::InjectedFault(_)
            | Self::PageSupportUnavailable
            | Self::TransactionPageSupportUnavailable
            | Self::PageWidthMismatch { .. }
            | Self::ForeignPageLineage(_)
            | Self::PoisonedWriter
            | Self::UnknownFlushPosition(_)
            | Self::ForeignFlushPosition(_)
            | Self::RecordCapacityExhausted
            | Self::PositionSpaceExhausted => None,
        }
    }
}

/// Failure to allocate a fresh transaction epoch from the file log.
#[derive(Debug, Eq, PartialEq)]
pub enum FileTransactionEpochError {
    Io(FileIoError),
    PoisonedWriter,
    EpochSpaceExhausted,
}

impl fmt::Display for FileTransactionEpochError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::PoisonedWriter => formatter
                .write_str("commit-log writer is poisoned; reopen the file before retrying"),
            Self::EpochSpaceExhausted => {
                formatter.write_str("transaction epoch space is exhausted")
            }
        }
    }
}

impl Error for FileTransactionEpochError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::PoisonedWriter | Self::EpochSpaceExhausted => None,
        }
    }
}

/// Failure to establish an authoritative recovery result from the file log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTransactionRecoveryError {
    PoisonedWriter,
    VolatileCommitRecord(TransactionId),
    DuplicateCommitRecord(TransactionId),
}

impl fmt::Display for FileTransactionRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoisonedWriter => formatter
                .write_str("commit-log writer is poisoned; reopen the file before retrying"),
            Self::VolatileCommitRecord(transaction_id) => write!(
                formatter,
                "transaction {transaction_id} has a complete unmarked commit record"
            ),
            Self::DuplicateCommitRecord(transaction_id) => write!(
                formatter,
                "transaction {transaction_id} has duplicate commit records"
            ),
        }
    }
}

impl Error for FileTransactionRecoveryError {}

/// Failure to inventory durable transaction-owned pages in a filesystem WAL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileCommittedPageRecoveryInventoryError {
    /// The opened WAL format cannot contain transaction-owned page evidence.
    TransactionPageSupportUnavailable {
        /// Exact opened WAL format version.
        version: u16,
    },
    /// An uncertain prior WAL write requires reopen before authoritative recovery.
    PoisonedWriter,
    /// The owned-page inventory could not reserve its durable-prefix upper bound.
    PageCapacityExhausted,
}

impl fmt::Display for FileCommittedPageRecoveryInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransactionPageSupportUnavailable { version } => write!(
                formatter,
                "file WAL format version {version} does not support committed-page recovery inventory"
            ),
            Self::PoisonedWriter => formatter.write_str(
                "commit-log writer is poisoned; reopen the file before committed-page recovery inventory",
            ),
            Self::PageCapacityExhausted => {
                formatter.write_str("filesystem recovery page inventory capacity is exhausted")
            }
        }
    }
}

impl Error for FileCommittedPageRecoveryInventoryError {}

/// Projection whose filesystem recovery evidence could not reserve memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilePageRecoveryProjection {
    /// Commit-agnostic physical page observations.
    PhysicalPages,
    /// Transaction-owner-aware page observations.
    TransactionPages,
    /// Complete durable commit observations.
    Commits,
}

impl fmt::Display for FilePageRecoveryProjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PhysicalPages => formatter.write_str("physical page"),
            Self::TransactionPages => formatter.write_str("transaction page"),
            Self::Commits => formatter.write_str("commit"),
        }
    }
}

/// Failure to project one stable filesystem durable prefix for page recovery.
#[derive(Debug, Eq, PartialEq)]
pub enum FileCommittedPageRecoverySourceError<const N: usize> {
    /// The opened WAL format cannot contain transaction-owned page evidence.
    TransactionPageSupportUnavailable {
        /// Exact opened WAL format version.
        version: u16,
    },
    /// An uncertain prior WAL write requires reopen before authoritative recovery.
    PoisonedWriter,
    /// One projection could not reserve enough memory before scanning.
    EvidenceCapacityExhausted {
        /// Projection whose allocation failed.
        projection: FilePageRecoveryProjection,
    },
    /// A matching physical page record could not become domain evidence.
    PhysicalPageProjection(Box<PageRecoveryObservationBytesError<N>>),
    /// A matching transaction-owned page record could not become domain evidence.
    TransactionPageProjection(Box<DurableTransactionPageObservationBytesError<N>>),
    /// A durable commit record could not become domain evidence.
    CommitProjection(Box<DurableTransactionCommitObservationFieldsError>),
}

impl<const N: usize> fmt::Display for FileCommittedPageRecoverySourceError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransactionPageSupportUnavailable { version } => write!(
                formatter,
                "file WAL format version {version} does not support committed-page recovery"
            ),
            Self::PoisonedWriter => formatter.write_str(
                "commit-log writer is poisoned; reopen the file before committed-page recovery",
            ),
            Self::EvidenceCapacityExhausted { projection } => write!(
                formatter,
                "filesystem {projection} recovery evidence capacity is exhausted"
            ),
            Self::PhysicalPageProjection(source) => {
                write!(
                    formatter,
                    "physical page recovery projection failed: {source}"
                )
            }
            Self::TransactionPageProjection(source) => {
                write!(
                    formatter,
                    "transaction page recovery projection failed: {source}"
                )
            }
            Self::CommitProjection(source) => {
                write!(formatter, "commit recovery projection failed: {source}")
            }
        }
    }
}

impl<const N: usize> Error for FileCommittedPageRecoverySourceError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PhysicalPageProjection(source) => Some(source.as_ref()),
            Self::TransactionPageProjection(source) => Some(source.as_ref()),
            Self::CommitProjection(source) => Some(source.as_ref()),
            Self::TransactionPageSupportUnavailable { .. }
            | Self::PoisonedWriter
            | Self::EvidenceCapacityExhausted { .. } => None,
        }
    }
}

/// Failure to project one complete filesystem durable prefix for restart analysis.
#[derive(Debug, Eq, PartialEq)]
pub enum FileTransactionRestartAnalysisSourceError<const N: usize> {
    /// An uncertain prior WAL write requires reopen before authoritative analysis.
    PoisonedWriter,
    /// Complete-prefix projection is invalid after an anchored reclamation.
    PrunedGenerationRequiresCheckpoint {
        /// Exact nonzero source generation.
        generation: u64,
    },
    /// Physical generation metadata requires a previously allocated epoch.
    NoAllocatedEpoch,
    /// The unified observation stream could not reserve its durable-prefix bound.
    ObservationCapacityExhausted {
        /// Exact number of durable logical records that required reservation.
        record_count: usize,
    },
    /// One raw page record could not become adapter-neutral restart evidence.
    PageProjection(Box<PageRecoveryObservationBytesError<N>>),
    /// One transaction-owned page could not become restart evidence.
    TransactionPageProjection(Box<DurableTransactionPageObservationBytesError<N>>),
    /// One transaction commit could not become restart evidence.
    CommitProjection(Box<DurableTransactionCommitObservationFieldsError>),
}

impl<const N: usize> fmt::Display for FileTransactionRestartAnalysisSourceError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoisonedWriter => formatter.write_str(
                "commit-log writer is poisoned; reopen the file before restart analysis",
            ),
            Self::PrunedGenerationRequiresCheckpoint { generation } => write!(
                formatter,
                "filesystem WAL generation {generation} is pruned and requires its anchored checkpoint"
            ),
            Self::NoAllocatedEpoch => {
                formatter.write_str("no filesystem transaction epoch has been allocated")
            }
            Self::ObservationCapacityExhausted { record_count } => write!(
                formatter,
                "filesystem restart observation capacity is exhausted for {record_count} durable records"
            ),
            Self::PageProjection(source) => {
                write!(formatter, "raw page restart projection failed: {source}")
            }
            Self::TransactionPageProjection(source) => {
                write!(
                    formatter,
                    "transaction page restart projection failed: {source}"
                )
            }
            Self::CommitProjection(source) => {
                write!(formatter, "commit restart projection failed: {source}")
            }
        }
    }
}

impl<const N: usize> Error for FileTransactionRestartAnalysisSourceError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PageProjection(source) => Some(source.as_ref()),
            Self::TransactionPageProjection(source) => Some(source.as_ref()),
            Self::CommitProjection(source) => Some(source.as_ref()),
            Self::PoisonedWriter
            | Self::PrunedGenerationRequiresCheckpoint { .. }
            | Self::NoAllocatedEpoch
            | Self::ObservationCapacityExhausted { .. } => None,
        }
    }
}

/// Failure to observe filesystem physical restart-allocation metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTransactionRestartRetentionMetadataSourceError {
    /// An uncertain prior WAL write requires reopen before metadata observation.
    PoisonedWriter,
    /// No restart or transaction epoch has yet been durably allocated.
    NoAllocatedEpoch,
}

impl fmt::Display for FileTransactionRestartRetentionMetadataSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoisonedWriter => formatter.write_str(
                "commit-log writer is poisoned; reopen the file before retention metadata observation",
            ),
            Self::NoAllocatedEpoch => {
                formatter.write_str("no filesystem transaction epoch has been allocated")
            }
        }
    }
}

impl Error for FileTransactionRestartRetentionMetadataSourceError {}

/// Failure while observing or replacing one filesystem WAL generation.
#[derive(Debug, Eq, PartialEq)]
pub enum FileTransactionRestartWalReclamationError {
    /// An uncertain prior WAL effect requires a complete reopen.
    PoisonedWriter,
    /// This physical source format has no reviewed reclamation encoding.
    UnsupportedPhysicalFormat {
        /// Exact unsupported source version.
        version: u16,
    },
    /// The selected WAL path has no final file-name component.
    MissingFileName,
    /// No durable transaction epoch exists to preserve in replacement metadata.
    NoAllocatedEpoch,
    /// A V5 source lost the stable database-file identity required by replacement.
    MissingDatabaseFileIdentity,
    /// The opaque domain permit does not describe the currently owned source.
    PermitMismatch,
    /// The current source generation cannot advance.
    GenerationExhausted,
    /// Complete logical records exist beyond the durable frontier.
    VolatileLogicalSuffix {
        /// Exact durable logical record count.
        durable_record_count: usize,
        /// Exact complete logical record count.
        total_record_count: usize,
    },
    /// The inclusive retained floor is not a current durable record boundary.
    RetainedBoundaryMissing {
        /// Exact requested numeric floor.
        position: u64,
    },
    /// The retained frame plan could not reserve memory.
    FrameCapacityExhausted {
        /// Exact retained logical record bound.
        record_count: usize,
    },
    /// The fixed candidate resolves to the selected inode.
    CandidateAliasesSelected,
    /// One deterministic replacement fault was consumed.
    InjectedFault(FaultPoint),
    /// One exact filesystem stage failed.
    Io(FileIoError),
}

impl fmt::Display for FileTransactionRestartWalReclamationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoisonedWriter => {
                formatter.write_str("commit-log writer is poisoned; reopen before WAL reclamation")
            }
            Self::UnsupportedPhysicalFormat { version } => {
                write!(
                    formatter,
                    "filesystem WAL format {version} cannot be reclaimed"
                )
            }
            Self::MissingFileName => {
                formatter.write_str("selected WAL path has no file-name component")
            }
            Self::NoAllocatedEpoch => {
                formatter.write_str("no filesystem transaction epoch has been allocated")
            }
            Self::MissingDatabaseFileIdentity => {
                formatter.write_str("V5 WAL has no stable database-file identity")
            }
            Self::PermitMismatch => {
                formatter.write_str("WAL reclamation permit does not match the selected source")
            }
            Self::GenerationExhausted => {
                formatter.write_str("filesystem WAL generation space is exhausted")
            }
            Self::VolatileLogicalSuffix {
                durable_record_count,
                total_record_count,
            } => write!(
                formatter,
                "filesystem WAL has {total_record_count} complete records but only {durable_record_count} are durable"
            ),
            Self::RetainedBoundaryMissing { position } => write!(
                formatter,
                "retained WAL floor {position} is not a durable logical record boundary"
            ),
            Self::FrameCapacityExhausted { record_count } => write!(
                formatter,
                "reclamation frame capacity is exhausted for {record_count} retained logical records"
            ),
            Self::CandidateAliasesSelected => {
                formatter.write_str("reclamation candidate aliases the selected WAL inode")
            }
            Self::InjectedFault(point) => {
                write!(formatter, "injected WAL reclamation failure {point}")
            }
            Self::Io(source) => source.fmt(formatter),
        }
    }
}

impl Error for FileTransactionRestartWalReclamationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::PoisonedWriter
            | Self::UnsupportedPhysicalFormat { .. }
            | Self::MissingFileName
            | Self::NoAllocatedEpoch
            | Self::MissingDatabaseFileIdentity
            | Self::PermitMismatch
            | Self::GenerationExhausted
            | Self::VolatileLogicalSuffix { .. }
            | Self::RetainedBoundaryMissing { .. }
            | Self::FrameCapacityExhausted { .. }
            | Self::CandidateAliasesSelected
            | Self::InjectedFault(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct StoredTransactionIdentity {
    epoch: u64,
    sequence: u64,
}

impl StoredTransactionIdentity {
    fn from_transaction_id(transaction_id: TransactionId) -> Self {
        Self {
            epoch: transaction_id.epoch().get(),
            sequence: transaction_id.sequence(),
        }
    }

    const fn from_epoch_sequence(epoch: u64, sequence: u64) -> Self {
        Self { epoch, sequence }
    }

    fn matches(self, transaction_id: TransactionId) -> bool {
        self.epoch == transaction_id.epoch().get() && self.sequence == transaction_id.sequence()
    }
}

/// Immutable snapshot of one physically appended full page image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePageWriteRecord<const N: usize = 0> {
    page_number: PageNumber,
    page_version: PageVersion,
    bytes: [u8; N],
}

impl<const N: usize> FilePageWriteRecord<N> {
    /// Returns the copied page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.page_number
    }

    /// Returns the copied page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.page_version
    }

    /// Returns the borrowed page bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Returns the owned page bytes.
    #[must_use]
    pub fn into_bytes(self) -> [u8; N] {
        self.bytes
    }
}

/// Immutable snapshot of one physically appended transaction-owned page image.
///
/// The persisted owner remains an inspectable epoch/sequence pair rather than a
/// reconstructible domain token. This type has no public constructor.
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     FilePageWriteRecord, FileTransactionPageWriteRecord,
/// };
///
/// fn cannot_construct(page: FilePageWriteRecord<1>) {
///     let _forged = FileTransactionPageWriteRecord {
///         transaction_epoch: 1,
///         transaction_sequence: 1,
///         page,
///     };
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileTransactionPageWriteRecord<const N: usize = 0> {
    transaction_epoch: u64,
    transaction_sequence: u64,
    page: FilePageWriteRecord<N>,
}

impl<const N: usize> FileTransactionPageWriteRecord<N> {
    /// Returns the persisted owner epoch.
    #[must_use]
    pub const fn transaction_epoch(&self) -> u64 {
        self.transaction_epoch
    }

    /// Returns the persisted owner sequence.
    #[must_use]
    pub const fn transaction_sequence(&self) -> u64 {
        self.transaction_sequence
    }

    /// Returns whether the persisted owner matches the complete domain identity.
    #[must_use]
    pub fn matches_transaction_id(&self, transaction_id: TransactionId) -> bool {
        self.transaction_epoch == transaction_id.epoch().get()
            && self.transaction_sequence == transaction_id.sequence()
    }

    /// Returns the copied full page-image payload.
    #[must_use]
    pub const fn page_write(&self) -> &FilePageWriteRecord<N> {
        &self.page
    }
}

/// Safely inspectable payload of one physically appended file-log record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileLogRecordKind<const N: usize = 0> {
    /// One transaction commit record.
    TransactionCommit {
        transaction_epoch: u64,
        transaction_sequence: u64,
    },
    /// One complete page-image write.
    PageWrite(FilePageWriteRecord<N>),
    /// One complete transaction-owned page-image write.
    TransactionPageWrite(FileTransactionPageWriteRecord<N>),
}

impl<const N: usize> FileLogRecordKind<N> {
    /// Returns the transaction epoch when this record is a commit.
    #[must_use]
    pub const fn transaction_epoch(&self) -> Option<u64> {
        match self {
            Self::TransactionCommit {
                transaction_epoch, ..
            } => Some(*transaction_epoch),
            Self::PageWrite(_) | Self::TransactionPageWrite(_) => None,
        }
    }

    /// Returns the transaction sequence when this record is a commit.
    #[must_use]
    pub const fn transaction_sequence(&self) -> Option<u64> {
        match self {
            Self::TransactionCommit {
                transaction_sequence,
                ..
            } => Some(*transaction_sequence),
            Self::PageWrite(_) | Self::TransactionPageWrite(_) => None,
        }
    }

    /// Returns the full page-image payload when this record is a page write.
    #[must_use]
    pub const fn page_write(&self) -> Option<&FilePageWriteRecord<N>> {
        match self {
            Self::TransactionCommit { .. } => None,
            Self::PageWrite(record) => Some(record),
            Self::TransactionPageWrite(record) => Some(record.page_write()),
        }
    }

    /// Returns the typed transaction-owned page record when present.
    #[must_use]
    pub const fn transaction_page_write(&self) -> Option<&FileTransactionPageWriteRecord<N>> {
        match self {
            Self::TransactionCommit { .. } | Self::PageWrite(_) => None,
            Self::TransactionPageWrite(record) => Some(record),
        }
    }

    /// Returns the persisted page-owner epoch only for an owned page record.
    #[must_use]
    pub const fn page_owner_transaction_epoch(&self) -> Option<u64> {
        match self {
            Self::TransactionCommit { .. } | Self::PageWrite(_) => None,
            Self::TransactionPageWrite(record) => Some(record.transaction_epoch()),
        }
    }

    /// Returns the persisted page-owner sequence only for an owned page record.
    #[must_use]
    pub const fn page_owner_transaction_sequence(&self) -> Option<u64> {
        match self {
            Self::TransactionCommit { .. } | Self::PageWrite(_) => None,
            Self::TransactionPageWrite(record) => Some(record.transaction_sequence()),
        }
    }
}

/// Immutable snapshot of one physically appended commit or page record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileLogRecord<const N: usize = 0> {
    position: LogSequenceNumber,
    kind: FileLogRecordKind<N>,
}

impl<const N: usize> FileLogRecord<N> {
    fn transaction_identity(&self) -> Option<StoredTransactionIdentity> {
        match self.kind {
            FileLogRecordKind::TransactionCommit {
                transaction_epoch,
                transaction_sequence,
            } => Some(StoredTransactionIdentity::from_epoch_sequence(
                transaction_epoch,
                transaction_sequence,
            )),
            FileLogRecordKind::PageWrite(_) | FileLogRecordKind::TransactionPageWrite(_) => None,
        }
    }

    /// Returns the adapter-assigned log position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.position
    }

    /// Returns the safely inspectable record payload.
    #[must_use]
    pub const fn kind(&self) -> &FileLogRecordKind<N> {
        &self.kind
    }

    /// Returns the recorded transaction epoch when this record is a commit.
    #[must_use]
    pub const fn transaction_epoch(&self) -> Option<u64> {
        self.kind.transaction_epoch()
    }

    /// Returns the recorded transaction sequence when this record is a commit.
    #[must_use]
    pub const fn transaction_sequence(&self) -> Option<u64> {
        self.kind.transaction_sequence()
    }

    /// Returns whether this record matches the complete transaction identity.
    #[must_use]
    pub fn matches_transaction_id(&self, transaction_id: TransactionId) -> bool {
        match self.transaction_identity() {
            Some(transaction) => transaction.matches(transaction_id),
            None => false,
        }
    }

    /// Returns the full page-image payload when this record is a page write.
    #[must_use]
    pub const fn page_write(&self) -> Option<&FilePageWriteRecord<N>> {
        self.kind.page_write()
    }

    /// Returns the typed transaction-owned page record when present.
    #[must_use]
    pub const fn transaction_page_write(&self) -> Option<&FileTransactionPageWriteRecord<N>> {
        self.kind.transaction_page_write()
    }

    /// Returns the persisted page-owner epoch only for an owned page record.
    #[must_use]
    pub const fn page_owner_transaction_epoch(&self) -> Option<u64> {
        self.kind.page_owner_transaction_epoch()
    }

    /// Returns the persisted page-owner sequence only for an owned page record.
    #[must_use]
    pub const fn page_owner_transaction_sequence(&self) -> Option<u64> {
        self.kind.page_owner_transaction_sequence()
    }

    /// Returns whether this record is an owned page for the complete identity.
    #[must_use]
    pub fn page_owner_matches_transaction_id(&self, transaction_id: TransactionId) -> bool {
        match self.transaction_page_write() {
            Some(record) => record.matches_transaction_id(transaction_id),
            None => false,
        }
    }

    /// Projects a page record into adapter-neutral recovery evidence.
    ///
    /// Callers must select records from a commit log's durable prefix before
    /// treating the result as durable. Transaction records return `Ok(None)`.
    pub fn page_recovery_observation(
        &self,
    ) -> Result<Option<DurablePageWalObservation<N>>, PageRecoveryObservationBytesError<N>> {
        match self.page_write() {
            Some(record) => self.project_page_recovery_observation(record).map(Some),
            None => Ok(None),
        }
    }

    /// Projects an owned-page record into transaction-aware recovery evidence.
    ///
    /// Callers must select this record from the complete durable prefix before
    /// treating the result as durable. Commit and raw page records return
    /// `Ok(None)`. An owned page intentionally also projects through
    /// [`Self::page_recovery_observation`] for commit-agnostic physical
    /// reconciliation; callers must not double-count those two views.
    pub fn transaction_page_recovery_observation(
        &self,
    ) -> Result<
        Option<DurableTransactionPageObservation<N>>,
        DurableTransactionPageObservationBytesError<N>,
    > {
        match self.transaction_page_write() {
            Some(record) => self
                .project_transaction_page_recovery_observation(record)
                .map(Some),
            None => Ok(None),
        }
    }

    /// Projects a commit record into transaction-aware recovery evidence.
    ///
    /// Callers must select this record from the complete durable prefix before
    /// treating the result as durable. Both page record kinds return
    /// `Ok(None)`, so persisted page ownership remains separate from commitment.
    pub fn transaction_commit_recovery_observation(
        &self,
    ) -> Result<
        Option<DurableTransactionCommitObservation>,
        DurableTransactionCommitObservationFieldsError,
    > {
        match &self.kind {
            FileLogRecordKind::TransactionCommit {
                transaction_epoch,
                transaction_sequence,
            } => self
                .project_transaction_commit_recovery_observation(
                    *transaction_epoch,
                    *transaction_sequence,
                )
                .map(Some),
            FileLogRecordKind::PageWrite(_) | FileLogRecordKind::TransactionPageWrite(_) => {
                Ok(None)
            }
        }
    }

    fn project_page_recovery_observation(
        &self,
        record: &FilePageWriteRecord<N>,
    ) -> Result<DurablePageWalObservation<N>, PageRecoveryObservationBytesError<N>> {
        DurablePageWalObservation::from_bytes(
            record.page_number(),
            record.page_version(),
            *record.bytes(),
            self.position.clone(),
        )
    }

    fn project_transaction_page_recovery_observation(
        &self,
        record: &FileTransactionPageWriteRecord<N>,
    ) -> Result<DurableTransactionPageObservation<N>, DurableTransactionPageObservationBytesError<N>>
    {
        let page = record.page_write();
        DurableTransactionPageObservation::from_bytes(
            record.transaction_epoch(),
            record.transaction_sequence(),
            page.page_number(),
            page.page_version(),
            *page.bytes(),
            self.position.clone(),
        )
    }

    fn project_transaction_commit_recovery_observation(
        &self,
        transaction_epoch: u64,
        transaction_sequence: u64,
    ) -> Result<DurableTransactionCommitObservation, DurableTransactionCommitObservationFieldsError>
    {
        DurableTransactionCommitObservation::from_fields(
            transaction_epoch,
            transaction_sequence,
            self.position.clone(),
        )
    }
}

/// Inspectable filesystem-backed implementation of the transaction commit-log
/// port, the v2/v3 page WAL port, and the v3 transaction-page WAL port.
#[derive(Debug)]
pub struct FileCommitLog<const N: usize = 0> {
    file: File,
    reclamation_old_file: Option<File>,
    path: PathBuf,
    parent_directory: File,
    format: LogFormat,
    lineage: LogLineage,
    persistent_id: PersistentLogId,
    database_file_identity: Option<DatabaseFileHeaderIdentity>,
    generation: u64,
    reclaimed_retained_first: Option<LogSequenceNumber>,
    reclaimed_logical_high_water: Option<LogSequenceNumber>,
    reclaimed_allocated_epoch_high_water: Option<NonZeroU64>,
    selected_checkpoint_anchor: Option<(u16, u128)>,
    records: Vec<FileLogRecord<N>>,
    durable_len: usize,
    next_epoch: Option<NonZeroU64>,
    next_position: Option<u64>,
    armed_fault: Option<FaultPoint>,
    poisoned: bool,
}

pub(crate) struct LockedFileCommitLogOpen<const N: usize> {
    log: FileCommitLog<N>,
    repaired_len: Option<u64>,
}

impl<const N: usize> LockedFileCommitLogOpen<N> {
    pub(crate) fn metadata(&self) -> io::Result<fs::Metadata> {
        self.log.file.metadata()
    }

    pub(crate) const fn persistent_id(&self) -> PersistentLogId {
        self.log.persistent_id
    }

    pub(crate) const fn physical_format_version(&self) -> u16 {
        self.log.format.version()
    }

    pub(crate) const fn database_file_identity(&self) -> Option<DatabaseFileHeaderIdentity> {
        self.log.database_file_identity
    }

    pub(crate) fn is_exact_initial_database_file(&self) -> bool {
        self.repaired_len.is_none()
            && self.log.format == LogFormat::V5
            && self.log.generation == 0
            && self.log.reclaimed_retained_first.is_none()
            && self.log.reclaimed_logical_high_water.is_none()
            && self.log.reclaimed_allocated_epoch_high_water.is_none()
            && self.log.selected_checkpoint_anchor.is_none()
            && self.log.records.is_empty()
            && self.log.durable_len == 0
            && self.log.next_epoch == Some(NonZeroU64::MIN)
            && self.log.next_position == Some(1)
    }

    pub(crate) fn finish(mut self) -> Result<FileCommitLog<N>, FileOpenError> {
        if let Some(repaired_len) = self.repaired_len {
            self.log.file.set_len(repaired_len).map_err(|source| {
                FileOpenError::Io(FileIoError::new(
                    FileIoStage::TruncateIncompleteTail,
                    source,
                ))
            })?;
            self.log.file.sync_all().map_err(|source| {
                FileOpenError::Io(FileIoError::new(FileIoStage::SyncTruncatedTail, source))
            })?;
        }
        self.log
            .file
            .seek(SeekFrom::End(0))
            .map_err(|source| FileOpenError::Io(FileIoError::new(FileIoStage::SeekEnd, source)))?;
        cleanup_reclamation_candidate(&self.log.path, &self.log.file, &self.log.parent_directory)?;
        Ok(self.log)
    }

    pub(crate) fn finish_for_database_create(mut self) -> Result<FileCommitLog<N>, FileOpenError> {
        self.log
            .file
            .seek(SeekFrom::End(0))
            .map_err(|source| FileOpenError::Io(FileIoError::new(FileIoStage::SeekEnd, source)))?;
        Ok(self.log)
    }
}

impl FileCommitLog<0> {
    /// Creates a new empty v1 file with one caller-supplied persistent lineage ID.
    pub fn create_new<P>(path: P, persistent_id: PersistentLogId) -> Result<Self, FileCreateError>
    where
        P: AsRef<Path>,
    {
        Self::create_new_internal(
            path.as_ref(),
            persistent_id,
            LogFormat::V1,
            &build_header_v1(persistent_id),
            None,
        )
    }

    /// Opens an existing v1 file, synchronizes it, validates the complete prefix,
    /// repairs only a trailing incomplete frame, and reconstructs in-memory state.
    pub fn open<P>(path: P) -> Result<Self, FileOpenError>
    where
        P: AsRef<Path>,
    {
        Self::open_internal(path.as_ref(), HeaderExpectation::V1)
    }
}

impl<const N: usize> FileCommitLog<N> {
    pub(crate) fn database_create_metadata(&self) -> io::Result<fs::Metadata> {
        self.file.metadata()
    }

    pub(crate) fn rebind_database_selected_path(&mut self, path: &Path) {
        self.path = path.to_path_buf();
    }

    /// Creates a new empty v2 page-capable file with one caller-supplied persistent lineage ID.
    pub fn create_new_page_capable<P>(
        path: P,
        persistent_id: PersistentLogId,
    ) -> Result<Self, FileCreateError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(FileCreateError::PageWidth)?;
        Self::create_new_internal(
            path.as_ref(),
            persistent_id,
            LogFormat::V2,
            &build_header_v2(persistent_id, layout.width_u64),
            None,
        )
    }

    /// Opens an existing v2 page-capable file and reconstructs mixed commit/page state.
    pub fn open_page_capable<P>(path: P) -> Result<Self, FileOpenError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(FileOpenError::PageWidth)?;
        Self::open_internal(path.as_ref(), HeaderExpectation::V2(layout))
    }

    /// Creates a new empty v3 transaction-page-capable file.
    pub fn create_new_transaction_page_capable<P>(
        path: P,
        persistent_id: PersistentLogId,
    ) -> Result<Self, FileCreateError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(FileCreateError::PageWidth)?;
        Self::create_new_internal(
            path.as_ref(),
            persistent_id,
            LogFormat::V3,
            &build_header_v3(persistent_id, layout.width_u64),
            None,
        )
    }

    /// Creates a new empty V5 WAL carrying one exact stable database-file identity.
    pub fn create_new_database_transaction_page_capable<P>(
        path: P,
        storage_identity: DatabaseStorageIdentity,
    ) -> Result<Self, FileCreateError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(FileCreateError::PageWidth)?;
        let database_file_identity =
            storage_identity.file_header_identity(ntsql_database::DatabaseFileRole::Wal);
        let persistent_id = storage_identity.persistent_log_id();
        Self::create_new_internal(
            path.as_ref(),
            persistent_id,
            LogFormat::V5,
            &build_header_v5_initial(persistent_id, layout.width_u64, database_file_identity),
            Some(database_file_identity),
        )
    }

    /// Opens an existing v3 transaction-page-capable file.
    pub fn open_transaction_page_capable<P>(path: P) -> Result<Self, FileOpenError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(FileOpenError::PageWidth)?;
        Self::open_internal(path.as_ref(), HeaderExpectation::V3OrLater(layout))
    }

    pub(crate) fn inspect_transaction_page_capable<P>(
        path: P,
    ) -> Result<LockedFileCommitLogOpen<N>, FileOpenError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(FileOpenError::PageWidth)?;
        Self::inspect_internal(path.as_ref(), HeaderExpectation::V3OrLater(layout))
    }

    fn create_new_internal(
        path: &Path,
        persistent_id: PersistentLogId,
        format: LogFormat,
        header: &[u8],
        database_file_identity: Option<DatabaseFileHeaderIdentity>,
    ) -> Result<Self, FileCreateError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| {
                FileCreateError::Io(FileIoError::new(FileIoStage::CreateFile, source))
            })?;
        file.try_lock().map_err(|source| {
            FileCreateError::Io(FileIoError::new(
                FileIoStage::AcquireExclusiveLock,
                source.into(),
            ))
        })?;

        file.write_all(header).map_err(|source| {
            FileCreateError::Io(FileIoError::new(FileIoStage::WriteHeader, source))
        })?;
        file.sync_all().map_err(|source| {
            FileCreateError::Io(FileIoError::new(FileIoStage::SyncCreatedFile, source))
        })?;
        let parent_directory = sync_parent_directory(path)?;
        file.seek(SeekFrom::End(0)).map_err(|source| {
            FileCreateError::Io(FileIoError::new(FileIoStage::SeekEnd, source))
        })?;

        Ok(Self {
            file,
            reclamation_old_file: None,
            path: path.to_path_buf(),
            parent_directory,
            format,
            lineage: LogLineage::persistent(persistent_id),
            persistent_id,
            database_file_identity,
            generation: 0,
            reclaimed_retained_first: None,
            reclaimed_logical_high_water: None,
            reclaimed_allocated_epoch_high_water: None,
            selected_checkpoint_anchor: None,
            records: Vec::new(),
            durable_len: 0,
            next_epoch: Some(NonZeroU64::MIN),
            next_position: Some(1),
            armed_fault: None,
            poisoned: false,
        })
    }

    fn open_internal(path: &Path, expectation: HeaderExpectation) -> Result<Self, FileOpenError> {
        Self::inspect_internal(path, expectation)?.finish()
    }

    fn inspect_internal(
        path: &Path,
        expectation: HeaderExpectation,
    ) -> Result<LockedFileCommitLogOpen<N>, FileOpenError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| FileOpenError::Io(FileIoError::new(FileIoStage::OpenFile, source)))?;
        file.try_lock().map_err(|source| {
            FileOpenError::Io(FileIoError::new(
                FileIoStage::AcquireExclusiveLock,
                source.into(),
            ))
        })?;
        file.sync_all().map_err(|source| {
            FileOpenError::Io(FileIoError::new(FileIoStage::SyncOpenedFile, source))
        })?;

        let file_len = file
            .metadata()
            .map_err(|source| {
                FileOpenError::Io(FileIoError::new(FileIoStage::ReadMetadata, source))
            })?
            .len();
        if file_len < HEADER_LENGTH_U64 {
            return Err(FileOpenError::Format(FileFormatError::new(
                0,
                FileFormatErrorReason::HeaderTooShort { actual: file_len },
            )));
        }

        let mut header = [0_u8; HEADER_LENGTH];
        file.read_exact(&mut header).map_err(|source| {
            FileOpenError::Io(FileIoError::new(FileIoStage::ReadHeader, source))
        })?;
        let header_version = read_u16(&header, 8);
        let (
            format,
            persistent_id,
            generation,
            reclaimed_retained_first,
            reclaimed_logical_high_water,
            allocated_epoch_high_water,
            selected_checkpoint_anchor,
            database_file_identity,
            header_length,
        ) = if header_version == FORMAT_VERSION_V5 {
            if !expectation.accepts(LogFormat::V5) {
                return Err(FileOpenError::Format(FileFormatError::new(
                    8,
                    FileFormatErrorReason::HeaderVersion {
                        actual: header_version,
                    },
                )));
            }
            if file_len < HEADER_V5_LENGTH_U64 {
                return Err(FileOpenError::Format(FileFormatError::new(
                    0,
                    FileFormatErrorReason::HeaderTooShort { actual: file_len },
                )));
            }
            let mut header_v5 = [0_u8; HEADER_V5_LENGTH];
            header_v5[..HEADER_LENGTH].copy_from_slice(&header);
            file.read_exact(&mut header_v5[HEADER_LENGTH..])
                .map_err(|source| {
                    FileOpenError::Io(FileIoError::new(FileIoStage::ReadHeader, source))
                })?;
            let metadata = parse_header_v5(
                &header_v5,
                expectation.page_layout().ok_or_else(|| {
                    FileOpenError::Format(FileFormatError::new(
                        HEADER_V2_PAGE_WIDTH_OFFSET as u64,
                        FileFormatErrorReason::HeaderPageWidthZero,
                    ))
                })?,
            )
            .map_err(FileOpenError::Format)?;
            (
                LogFormat::V5,
                metadata.persistent_id,
                metadata.generation,
                metadata.retained_first,
                metadata.logical_high_water,
                metadata.allocated_epoch_high_water,
                metadata.selected_checkpoint_anchor,
                Some(metadata.database_file_identity),
                HEADER_V5_LENGTH_U64,
            )
        } else if header_version == FORMAT_VERSION_V4 {
            if !expectation.accepts(LogFormat::V4) {
                return Err(FileOpenError::Format(FileFormatError::new(
                    8,
                    FileFormatErrorReason::HeaderVersion {
                        actual: header_version,
                    },
                )));
            }
            if file_len < HEADER_V4_LENGTH_U64 {
                return Err(FileOpenError::Format(FileFormatError::new(
                    0,
                    FileFormatErrorReason::HeaderTooShort { actual: file_len },
                )));
            }
            let mut header_v4 = [0_u8; HEADER_V4_LENGTH];
            header_v4[..HEADER_LENGTH].copy_from_slice(&header);
            file.read_exact(&mut header_v4[HEADER_LENGTH..])
                .map_err(|source| {
                    FileOpenError::Io(FileIoError::new(FileIoStage::ReadHeader, source))
                })?;
            let metadata = parse_header_v4(
                &header_v4,
                expectation.page_layout().ok_or_else(|| {
                    FileOpenError::Format(FileFormatError::new(
                        HEADER_V2_PAGE_WIDTH_OFFSET as u64,
                        FileFormatErrorReason::HeaderPageWidthZero,
                    ))
                })?,
            )
            .map_err(FileOpenError::Format)?;
            (
                LogFormat::V4,
                metadata.persistent_id,
                metadata.generation,
                metadata.retained_first,
                metadata.logical_high_water,
                Some(metadata.allocated_epoch_high_water),
                Some(metadata.selected_checkpoint_anchor),
                None,
                HEADER_V4_LENGTH_U64,
            )
        } else {
            let persistent_id =
                parse_header(&header, expectation).map_err(FileOpenError::Format)?;
            let format = match header_version {
                FORMAT_VERSION_V1 => LogFormat::V1,
                FORMAT_VERSION_V2 => LogFormat::V2,
                FORMAT_VERSION_V3 => LogFormat::V3,
                _ => {
                    return Err(FileOpenError::Format(FileFormatError::new(
                        8,
                        FileFormatErrorReason::HeaderVersion {
                            actual: header_version,
                        },
                    )));
                }
            };
            (
                format,
                persistent_id,
                0,
                None,
                None,
                None,
                None,
                None,
                HEADER_LENGTH_U64,
            )
        };
        let lineage = LogLineage::persistent(persistent_id);
        let initial_epoch_high_water = allocated_epoch_high_water.map(NonZeroU64::get).unwrap_or(0);
        let initial_next_position = match reclaimed_retained_first {
            Some(position) => Some(position),
            None => match reclaimed_logical_high_water {
                Some(position) => position.checked_add(1),
                None => Some(1),
            },
        };
        let initial_completed_position = reclaimed_retained_first
            .and_then(|position| position.checked_sub(1))
            .or(reclaimed_logical_high_water)
            .unwrap_or(0);
        let initial_durable_position = if reclaimed_retained_first.is_none() {
            reclaimed_logical_high_water
        } else {
            None
        };
        let mut open_state = OpenState::new(
            lineage.clone(),
            expectation.page_layout(),
            initial_epoch_high_water,
            initial_next_position,
            initial_completed_position,
            initial_durable_position,
        );

        let frame_region_len = file_len - header_length;
        let complete_frame_count = frame_region_len / FRAME_LENGTH_U64;
        let incomplete_tail_len = frame_region_len % FRAME_LENGTH_U64;

        for frame_index in 0..complete_frame_count {
            let mut frame = [0_u8; FRAME_LENGTH];
            file.read_exact(&mut frame).map_err(|source| {
                FileOpenError::Io(FileIoError::new(FileIoStage::ReadFrame, source))
            })?;
            let offset = header_length + frame_index * FRAME_LENGTH_U64;
            let decoded = parse_frame(&frame, offset, format).map_err(FileOpenError::Format)?;
            open_state.apply_frame(decoded, offset)?;
        }

        if matches!(format, LogFormat::V4 | LogFormat::V5)
            && open_state.last_completed_position < reclaimed_logical_high_water.unwrap_or(0)
        {
            return Err(FileOpenError::Format(FileFormatError::new(
                header_length + complete_frame_count * FRAME_LENGTH_U64,
                FileFormatErrorReason::HeaderV4LogicalHighWaterMismatch {
                    expected: reclaimed_logical_high_water,
                    actual: NonZeroU64::new(open_state.last_completed_position)
                        .map(NonZeroU64::get),
                },
            )));
        }
        let repaired_len = match open_state.pending_page_header_offset() {
            Some(offset) => Some(offset),
            None if incomplete_tail_len > 0 => {
                Some(header_length + complete_frame_count * FRAME_LENGTH_U64)
            }
            None => None,
        };
        if matches!(format, LogFormat::V4 | LogFormat::V5) {
            let actual_retained_first = open_state
                .records
                .iter()
                .take(open_state.durable_len)
                .next()
                .map(|record| record.position().get());
            if reclaimed_retained_first.is_some()
                && actual_retained_first != reclaimed_retained_first
            {
                return Err(FileOpenError::Format(FileFormatError::new(
                    HEADER_V4_RETAINED_FIRST_OFFSET as u64,
                    FileFormatErrorReason::HeaderV4RetainedFirstMismatch {
                        expected: reclaimed_retained_first,
                        actual: actual_retained_first,
                    },
                )));
            }
            if reclaimed_logical_high_water.is_some_and(|expected| {
                open_state
                    .last_durable_position
                    .is_none_or(|actual| actual < expected)
            }) {
                return Err(FileOpenError::Format(FileFormatError::new(
                    HEADER_V4_LOGICAL_HIGH_WATER_OFFSET as u64,
                    FileFormatErrorReason::HeaderV4LogicalHighWaterMismatch {
                        expected: reclaimed_logical_high_water,
                        actual: open_state.last_durable_position,
                    },
                )));
            }
        }
        let parent_directory = open_parent_directory_for_open(path)?;

        Ok(LockedFileCommitLogOpen {
            log: Self {
                file,
                reclamation_old_file: None,
                path: path.to_path_buf(),
                parent_directory,
                format,
                lineage,
                persistent_id,
                database_file_identity,
                generation,
                reclaimed_retained_first: reclaimed_retained_first
                    .map(|position| LogLineage::persistent(persistent_id).position(position)),
                reclaimed_logical_high_water: reclaimed_logical_high_water
                    .map(|position| LogLineage::persistent(persistent_id).position(position)),
                reclaimed_allocated_epoch_high_water: allocated_epoch_high_water,
                selected_checkpoint_anchor,
                records: open_state.records,
                durable_len: open_state.durable_len,
                next_epoch: open_state.next_epoch,
                next_position: open_state.next_position,
                armed_fault: None,
                poisoned: false,
            },
            repaired_len,
        })
    }

    /// Returns the stable persistent lineage ID reconstructed from the header.
    #[must_use]
    pub const fn persistent_id(&self) -> PersistentLogId {
        self.persistent_id
    }

    /// Returns the stable database-file identity physically carried by V5.
    #[must_use]
    pub const fn database_file_identity(&self) -> Option<DatabaseFileHeaderIdentity> {
        self.database_file_identity
    }

    /// Returns the selected WAL file's repository-owned physical format version.
    #[must_use]
    pub const fn physical_format_version(&self) -> u16 {
        self.format.version()
    }

    /// Returns the selected WAL file's physical generation.
    ///
    /// Generation zero is an unreclaimed V1-V3 file. Every successful V4
    /// replacement advances this value exactly once.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the next logical position without consuming it.
    #[must_use]
    pub const fn next_logical_position(&self) -> Option<u64> {
        self.next_position
    }

    /// Returns the retained-first value encoded in the selected V4 replacement header.
    #[must_use]
    pub const fn replacement_header_retained_first(&self) -> Option<&LogSequenceNumber> {
        self.reclaimed_retained_first.as_ref()
    }

    /// Returns the logical high-water encoded in the selected V4 replacement header.
    #[must_use]
    pub const fn replacement_header_logical_high_water(&self) -> Option<&LogSequenceNumber> {
        self.reclaimed_logical_high_water.as_ref()
    }

    /// Returns the epoch high-water encoded in the selected V4 replacement header.
    #[must_use]
    pub const fn replacement_header_allocated_epoch_high_water(&self) -> Option<NonZeroU64> {
        self.reclaimed_allocated_epoch_high_water
    }

    /// Arms one fault without replacing an existing plan.
    pub fn arm_fault(&mut self, fault: FaultPoint) -> Result<(), FaultAlreadyArmed> {
        if let Some(armed) = self.armed_fault {
            return Err(FaultAlreadyArmed {
                armed,
                requested: fault,
            });
        }
        self.armed_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot fault that has not yet reached its matching stage.
    #[must_use]
    pub const fn armed_fault(&self) -> Option<FaultPoint> {
        self.armed_fault
    }

    /// Returns whether this writer must be reopened before more mutations or
    /// authoritative recovery are allowed.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Returns every physically appended record snapshot, including the complete
    /// unmarked suffix.
    #[must_use]
    pub fn records(&self) -> &[FileLogRecord<N>] {
        &self.records
    }

    /// Iterates over exactly the prefix covered by the latest durable-through marker.
    pub fn durable_records(
        &self,
    ) -> impl DoubleEndedIterator<Item = &FileLogRecord<N>> + ExactSizeIterator {
        self.records.iter().take(self.durable_len)
    }

    /// Returns the durable frontier position reconstructed from marker frames.
    #[must_use]
    pub fn durable_position(&self) -> Option<LogSequenceNumber> {
        self.durable_len
            .checked_sub(1)
            .and_then(|index| self.records.get(index))
            .map(FileLogRecord::position)
            .cloned()
            .or_else(|| self.reclaimed_logical_high_water.clone())
    }

    fn consume_fault(&mut self, point: FaultPoint) -> bool {
        if self.armed_fault == Some(point) {
            self.armed_fault = None;
            true
        } else {
            false
        }
    }

    fn write_frame(
        &mut self,
        frame: &[u8; FRAME_LENGTH],
        stage: FileIoStage,
        poison_on_error: bool,
    ) -> Result<(), FileIoError> {
        self.file.write_all(frame).map_err(|source| {
            if poison_on_error {
                self.poisoned = true;
            }
            FileIoError::new(stage, source)
        })
    }

    fn sync_file(&mut self, stage: FileIoStage, poison_on_error: bool) -> Result<(), FileIoError> {
        self.file.sync_all().map_err(|source| {
            if poison_on_error {
                self.poisoned = true;
            }
            FileIoError::new(stage, source)
        })
    }

    fn append_page_internal<const PAGE_N: usize>(
        &mut self,
        page: &UnloggedPage<PAGE_N>,
    ) -> Result<LogSequenceNumber, FileCommitLogError> {
        self.append_page_group(page, None)
    }

    fn append_transaction_page_internal<const PAGE_N: usize>(
        &mut self,
        record: &TransactionPageWriteRecord<'_, PAGE_N>,
    ) -> Result<LogSequenceNumber, FileCommitLogError> {
        if !self.format.supports_transaction_pages() {
            return Err(FileCommitLogError::TransactionPageSupportUnavailable);
        }
        self.append_page_group(
            record.page(),
            Some(StoredTransactionIdentity::from_transaction_id(
                record.transaction_id(),
            )),
        )
    }

    fn append_page_group<const PAGE_N: usize>(
        &mut self,
        page: &UnloggedPage<PAGE_N>,
        owner: Option<StoredTransactionIdentity>,
    ) -> Result<LogSequenceNumber, FileCommitLogError> {
        if self.poisoned {
            return Err(FileCommitLogError::PoisonedWriter);
        }
        if !self.format.supports_pages() {
            return Err(FileCommitLogError::PageSupportUnavailable);
        }
        if N != PAGE_N {
            return Err(FileCommitLogError::PageWidthMismatch {
                expected: N,
                actual: PAGE_N,
            });
        }
        if !self.lineage.same_lineage(page.address().lineage()) {
            return Err(FileCommitLogError::ForeignPageLineage(
                page.address().number(),
            ));
        }
        let layout = PageLayout::for_const::<N>().map_err(FileCommitLogError::PageWidth)?;
        let position_value = self
            .next_position
            .ok_or(FileCommitLogError::PositionSpaceExhausted)?;
        if self.consume_fault(FaultPoint::BeforeAppend) {
            return Err(FileCommitLogError::InjectedFault(FaultPoint::BeforeAppend));
        }
        self.records
            .try_reserve(1)
            .map_err(|_| FileCommitLogError::RecordCapacityExhausted)?;

        let (header_kind, header_stage) = match owner {
            Some(_) => (
                FrameKind::TransactionPageHeader,
                FileIoStage::WriteTransactionPageHeaderFrame,
            ),
            None => (FrameKind::PageHeader, FileIoStage::WritePageHeaderFrame),
        };
        let header = build_frame(
            self.format,
            header_kind,
            position_value,
            page.address().number().get(),
            page.version().get(),
        );
        self.write_frame(&header, header_stage, true)
            .map_err(FileCommitLogError::Io)?;

        if let Some(owner) = owner {
            let owner_frame = build_frame(
                self.format,
                FrameKind::TransactionPageOwner,
                position_value,
                owner.epoch,
                owner.sequence,
            );
            self.write_frame(
                &owner_frame,
                FileIoStage::WriteTransactionPageOwnerFrame,
                true,
            )
            .map_err(FileCommitLogError::Io)?;
        }

        let page_bytes = page.image().bytes();
        for chunk_index in 0..layout.chunk_count {
            let mut chunk = [0_u8; PAGE_CHUNK_WIDTH];
            let start = chunk_index * PAGE_CHUNK_WIDTH;
            let logical_len = layout.logical_bytes_for_chunk(chunk_index);
            let end = start + logical_len;
            for (destination, source) in chunk[..logical_len]
                .iter_mut()
                .zip(page_bytes[start..end].iter())
            {
                *destination = *source;
            }
            let data = build_frame_with_payload2_bytes(
                self.format,
                FrameKind::PageData,
                position_value,
                u64::try_from(chunk_index)
                    .map_err(|_| FileCommitLogError::PositionSpaceExhausted)?,
                chunk,
            );
            self.write_frame(&data, FileIoStage::WritePageDataFrame, true)
                .map_err(FileCommitLogError::Io)?;
        }

        let mut stored_bytes = [0_u8; N];
        for (destination, source) in stored_bytes.iter_mut().zip(page_bytes.iter()) {
            *destination = *source;
        }
        let position = self.lineage.position(position_value);
        let page_record = FilePageWriteRecord {
            page_number: page.address().number(),
            page_version: page.version(),
            bytes: stored_bytes,
        };
        let kind = match owner {
            Some(owner) => {
                FileLogRecordKind::TransactionPageWrite(FileTransactionPageWriteRecord {
                    transaction_epoch: owner.epoch,
                    transaction_sequence: owner.sequence,
                    page: page_record,
                })
            }
            None => FileLogRecordKind::PageWrite(page_record),
        };
        self.records.push(FileLogRecord {
            position: position.clone(),
            kind,
        });
        self.next_position = position_value.checked_add(1);

        if self.consume_fault(FaultPoint::AfterAppend) {
            Err(FileCommitLogError::InjectedFault(FaultPoint::AfterAppend))
        } else {
            Ok(position)
        }
    }

    fn allocate_transaction_epoch_frame(
        &mut self,
    ) -> Result<(NonZeroU64, LogLineage), FileTransactionEpochError> {
        if self.poisoned {
            return Err(FileTransactionEpochError::PoisonedWriter);
        }
        let epoch = self
            .next_epoch
            .ok_or(FileTransactionEpochError::EpochSpaceExhausted)?;
        let frame = build_frame(self.format, FrameKind::EpochAllocation, epoch.get(), 0, 0);
        self.write_frame(&frame, FileIoStage::WriteEpochFrame, true)
            .map_err(FileTransactionEpochError::Io)?;
        self.sync_file(FileIoStage::SyncEpochFrame, true)
            .map_err(FileTransactionEpochError::Io)?;
        self.next_epoch = epoch.get().checked_add(1).and_then(NonZeroU64::new);
        Ok((epoch, self.lineage.clone()))
    }

    fn allocated_epoch_high_water(&self) -> Option<NonZeroU64> {
        match self.next_epoch {
            Some(next_epoch) => NonZeroU64::new(next_epoch.get().saturating_sub(1)),
            None => Some(NonZeroU64::MAX),
        }
    }

    fn project_durable_restart_observations(
        &self,
    ) -> Result<
        Vec<DurableTransactionRestartObservation<N>>,
        FileTransactionRestartAnalysisSourceError<N>,
    > {
        let durable_len = self.durable_len;
        let mut observations = Vec::new();
        observations.try_reserve(durable_len).map_err(|_| {
            FileTransactionRestartAnalysisSourceError::ObservationCapacityExhausted {
                record_count: durable_len,
            }
        })?;
        for record in self.durable_records() {
            let observation = match record.kind() {
                FileLogRecordKind::TransactionCommit {
                    transaction_epoch,
                    transaction_sequence,
                } => record
                    .project_transaction_commit_recovery_observation(
                        *transaction_epoch,
                        *transaction_sequence,
                    )
                    .map(DurableTransactionRestartObservation::Commit)
                    .map_err(|source| {
                        FileTransactionRestartAnalysisSourceError::CommitProjection(Box::new(
                            source,
                        ))
                    })?,
                FileLogRecordKind::PageWrite(page) => record
                    .project_page_recovery_observation(page)
                    .map(DurableTransactionRestartObservation::Page)
                    .map_err(|source| {
                        FileTransactionRestartAnalysisSourceError::PageProjection(Box::new(source))
                    })?,
                FileLogRecordKind::TransactionPageWrite(transaction_page) => record
                    .project_transaction_page_recovery_observation(transaction_page)
                    .map(DurableTransactionRestartObservation::TransactionPage)
                    .map_err(|source| {
                        FileTransactionRestartAnalysisSourceError::TransactionPageProjection(
                            Box::new(source),
                        )
                    })?,
            };
            observations.push(observation);
        }
        Ok(observations)
    }
}

impl<const N: usize> TransactionEpochSource for FileCommitLog<N> {
    type Error = FileTransactionEpochError;

    fn allocate_transaction_epoch(&mut self) -> Result<(NonZeroU64, LogLineage), Self::Error> {
        self.allocate_transaction_epoch_frame()
    }
}

impl<const N: usize> TransactionRestartCoordinatorEpochSource for FileCommitLog<N> {
    type Error = FileTransactionEpochError;

    fn allocate_restart_transaction_epoch(
        &mut self,
        persisted_epoch_high_water: Option<NonZeroU64>,
    ) -> Result<
        (NonZeroU64, LogLineage),
        TransactionRestartCoordinatorEpochAllocationError<Self::Error>,
    > {
        let Some(next_epoch) = self.next_epoch else {
            return Err(
                TransactionRestartCoordinatorEpochAllocationError::IdentitySpaceExhausted {
                    persisted_epoch_high_water: persisted_epoch_high_water.map(NonZeroU64::get),
                },
            );
        };
        if let Some(high_water) = persisted_epoch_high_water
            && next_epoch <= high_water
        {
            return Err(
                TransactionRestartCoordinatorEpochAllocationError::PersistedEpochHighWaterNotAdvanced {
                    persisted_epoch_high_water: high_water.get(),
                    next_epoch: next_epoch.get(),
                },
            );
        }
        self.allocate_transaction_epoch_frame()
            .map_err(|source| match source {
                FileTransactionEpochError::EpochSpaceExhausted => {
                    TransactionRestartCoordinatorEpochAllocationError::IdentitySpaceExhausted {
                        persisted_epoch_high_water: persisted_epoch_high_water.map(NonZeroU64::get),
                    }
                }
                FileTransactionEpochError::Io(_) | FileTransactionEpochError::PoisonedWriter => {
                    TransactionRestartCoordinatorEpochAllocationError::Source(source)
                }
            })
    }
}

impl<const N: usize> TransactionRecoverySource for FileCommitLog<N> {
    type Error = FileTransactionRecoveryError;

    fn lookup_durable_commit(
        &mut self,
        transaction_id: TransactionId,
    ) -> Result<(LogLineage, DurableCommitLookup), Self::Error> {
        if self.poisoned {
            return Err(FileTransactionRecoveryError::PoisonedWriter);
        }

        let mut matching_record = None;
        for (index, record) in self.records.iter().enumerate() {
            if !record.matches_transaction_id(transaction_id) {
                continue;
            }
            if matching_record.is_some() {
                return Err(FileTransactionRecoveryError::DuplicateCommitRecord(
                    transaction_id,
                ));
            }
            matching_record = Some((record.position().clone(), index < self.durable_len));
        }

        let lookup = match matching_record {
            Some((position, true)) => DurableCommitLookup::Found { position },
            Some((_, false)) => {
                return Err(FileTransactionRecoveryError::VolatileCommitRecord(
                    transaction_id,
                ));
            }
            None => DurableCommitLookup::Absent,
        };
        Ok((self.lineage.clone(), lookup))
    }
}

impl<const N: usize> ntsql_transaction::DurableTransactionPageRecoveryInventory<N>
    for FileCommitLog<N>
{
    type Error = FileCommittedPageRecoveryInventoryError;

    fn durable_transaction_page_numbers(&mut self) -> Result<Vec<PageNumber>, Self::Error> {
        if self.poisoned {
            return Err(FileCommittedPageRecoveryInventoryError::PoisonedWriter);
        }
        if !self.format.supports_transaction_pages() {
            return Err(
                FileCommittedPageRecoveryInventoryError::TransactionPageSupportUnavailable {
                    version: self.format.version(),
                },
            );
        }

        let mut page_numbers = Vec::new();
        page_numbers
            .try_reserve(self.durable_len)
            .map_err(|_| FileCommittedPageRecoveryInventoryError::PageCapacityExhausted)?;
        for record in self.durable_records() {
            if let Some(page) = record.transaction_page_write() {
                page_numbers.push(page.page_write().page_number());
            }
        }
        page_numbers.sort_unstable();
        page_numbers.dedup();
        Ok(page_numbers)
    }
}

impl<const N: usize> ntsql_transaction::DurableTransactionPageRecoverySource<N>
    for FileCommitLog<N>
{
    type Error = FileCommittedPageRecoverySourceError<N>;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn with_durable_page_evidence<Output, Operation>(
        &mut self,
        page_number: PageNumber,
        operation: Operation,
    ) -> Result<Output, Self::Error>
    where
        Operation: for<'evidence> FnOnce(
            &'evidence [DurablePageWalObservation<N>],
            &'evidence [DurableTransactionPageObservation<N>],
            &'evidence [DurableTransactionCommitObservation],
        ) -> Output,
    {
        if self.poisoned {
            return Err(FileCommittedPageRecoverySourceError::PoisonedWriter);
        }
        if !self.format.supports_transaction_pages() {
            return Err(
                FileCommittedPageRecoverySourceError::TransactionPageSupportUnavailable {
                    version: self.format.version(),
                },
            );
        }

        let durable_len = self.durable_len;
        let mut physical_pages = Vec::new();
        physical_pages.try_reserve(durable_len).map_err(|_| {
            FileCommittedPageRecoverySourceError::EvidenceCapacityExhausted {
                projection: FilePageRecoveryProjection::PhysicalPages,
            }
        })?;
        let mut transaction_pages = Vec::new();
        transaction_pages.try_reserve(durable_len).map_err(|_| {
            FileCommittedPageRecoverySourceError::EvidenceCapacityExhausted {
                projection: FilePageRecoveryProjection::TransactionPages,
            }
        })?;
        let mut commits = Vec::new();
        commits.try_reserve(durable_len).map_err(|_| {
            FileCommittedPageRecoverySourceError::EvidenceCapacityExhausted {
                projection: FilePageRecoveryProjection::Commits,
            }
        })?;

        for record in self.durable_records() {
            if record
                .page_write()
                .is_some_and(|page| page.page_number() == page_number)
                && let Some(observation) = record.page_recovery_observation().map_err(|source| {
                    FileCommittedPageRecoverySourceError::PhysicalPageProjection(Box::new(source))
                })?
            {
                physical_pages.push(observation);
            }
            if record
                .transaction_page_write()
                .is_some_and(|page| page.page_write().page_number() == page_number)
                && let Some(observation) =
                    record
                        .transaction_page_recovery_observation()
                        .map_err(|source| {
                            FileCommittedPageRecoverySourceError::TransactionPageProjection(
                                Box::new(source),
                            )
                        })?
            {
                transaction_pages.push(observation);
            }
            if let Some(observation) =
                record
                    .transaction_commit_recovery_observation()
                    .map_err(|source| {
                        FileCommittedPageRecoverySourceError::CommitProjection(Box::new(source))
                    })?
            {
                commits.push(observation);
            }
        }

        Ok(operation(&physical_pages, &transaction_pages, &commits))
    }
}

impl<const N: usize> ntsql_transaction::DurableTransactionRestartAnalysisSource<N>
    for FileCommitLog<N>
{
    type Error = FileTransactionRestartAnalysisSourceError<N>;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn with_durable_transaction_restart_observations<Output, Operation>(
        &mut self,
        operation: Operation,
    ) -> Result<Output, Self::Error>
    where
        Operation: for<'evidence> FnOnce(
            Option<&'evidence LogSequenceNumber>,
            &'evidence [DurableTransactionRestartObservation<N>],
        ) -> Output,
    {
        if self.poisoned {
            return Err(FileTransactionRestartAnalysisSourceError::PoisonedWriter);
        }
        if self.generation != 0 {
            return Err(
                FileTransactionRestartAnalysisSourceError::PrunedGenerationRequiresCheckpoint {
                    generation: self.generation,
                },
            );
        }
        let durable_frontier = self.durable_position();
        let observations = self.project_durable_restart_observations()?;
        Ok(operation(durable_frontier.as_ref(), &observations))
    }
}

impl<const N: usize> DurableTransactionRestartPrunedGenerationSource<N> for FileCommitLog<N> {
    fn observe_restart_source_generation(
        &mut self,
    ) -> Result<u64, <Self as ntsql_transaction::DurableTransactionRestartAnalysisSource<N>>::Error>
    {
        if self.poisoned {
            return Err(FileTransactionRestartAnalysisSourceError::PoisonedWriter);
        }
        Ok(self.generation)
    }

    fn observe_restart_pruned_generation(
        &mut self,
    ) -> Result<
        DurableTransactionRestartWalReclamationSourceObservation,
        <Self as ntsql_transaction::DurableTransactionRestartAnalysisSource<N>>::Error,
    > {
        if self.poisoned {
            return Err(FileTransactionRestartAnalysisSourceError::PoisonedWriter);
        }
        let allocated_epoch_high_water = self
            .allocated_epoch_high_water()
            .ok_or(FileTransactionRestartAnalysisSourceError::NoAllocatedEpoch)?;
        Ok(
            DurableTransactionRestartWalReclamationSourceObservation::new(
                self.lineage.clone(),
                self.format.version(),
                self.generation,
                self.durable_records()
                    .next()
                    .map(FileLogRecord::position)
                    .cloned(),
                self.durable_position(),
                allocated_epoch_high_water,
                self.selected_checkpoint_anchor,
            ),
        )
    }

    fn with_durable_transaction_restart_retained_observations<Output, Operation>(
        &mut self,
        operation: Operation,
    ) -> Result<
        Output,
        <Self as ntsql_transaction::DurableTransactionRestartAnalysisSource<N>>::Error,
    >
    where
        Operation:
            for<'evidence> FnOnce(&'evidence [DurableTransactionRestartObservation<N>]) -> Output,
    {
        if self.poisoned {
            return Err(FileTransactionRestartAnalysisSourceError::PoisonedWriter);
        }
        let observations = self.project_durable_restart_observations()?;
        Ok(operation(&observations))
    }
}

impl<const N: usize> DurableTransactionRestartWalReclamationSource<N> for FileCommitLog<N> {
    type Error = FileTransactionRestartWalReclamationError;

    fn observe_restart_wal_reclamation_source(
        &mut self,
    ) -> Result<DurableTransactionRestartWalReclamationSourceObservation, Self::Error> {
        if self.poisoned {
            return Err(FileTransactionRestartWalReclamationError::PoisonedWriter);
        }
        let allocated_epoch_high_water = self
            .allocated_epoch_high_water()
            .ok_or(FileTransactionRestartWalReclamationError::NoAllocatedEpoch)?;
        Ok(
            DurableTransactionRestartWalReclamationSourceObservation::new(
                self.lineage.clone(),
                self.format.version(),
                self.generation,
                self.durable_records()
                    .next()
                    .map(FileLogRecord::position)
                    .cloned(),
                self.durable_position(),
                allocated_epoch_high_water,
                self.selected_checkpoint_anchor,
            ),
        )
    }

    fn reclaim_restart_wal_prefix(
        &mut self,
        permit: DurableTransactionRestartWalReclamationPermit<'_>,
    ) -> Result<DurableTransactionRestartWalReclamationEffectObservation, Self::Error> {
        if self.poisoned {
            return Err(FileTransactionRestartWalReclamationError::PoisonedWriter);
        }
        if !matches!(self.format, LogFormat::V3 | LogFormat::V4 | LogFormat::V5) {
            return Err(
                FileTransactionRestartWalReclamationError::UnsupportedPhysicalFormat {
                    version: self.format.version(),
                },
            );
        }
        let allocated_epoch_high_water = self
            .allocated_epoch_high_water()
            .ok_or(FileTransactionRestartWalReclamationError::NoAllocatedEpoch)?;
        let current_high_water = self.durable_position();
        let current_anchor_matches = match self.selected_checkpoint_anchor {
            Some((version, value)) => {
                let permit_anchor = permit.selected_checkpoint_anchor();
                version == permit_anchor.version() && value == permit_anchor.value()
            }
            None => self.generation == 0,
        };
        if permit.persistent_log_id() != self.persistent_id
            || !permit.lineage().same_lineage(&self.lineage)
            || permit.physical_format_version().get() != self.format.version()
            || permit.source_generation() != self.generation
            || permit.durable_frontier() != current_high_water.as_ref()
            || permit.allocated_epoch_high_water() != allocated_epoch_high_water
            || !current_anchor_matches
        {
            return Err(FileTransactionRestartWalReclamationError::PermitMismatch);
        }
        if self.records.len() != self.durable_len {
            return Err(
                FileTransactionRestartWalReclamationError::VolatileLogicalSuffix {
                    durable_record_count: self.durable_len,
                    total_record_count: self.records.len(),
                },
            );
        }
        let retained_start = match permit.retained_first_logical_record() {
            Some(floor) => self
                .records
                .iter()
                .take(self.durable_len)
                .position(|record| record.position() == floor)
                .ok_or(
                    FileTransactionRestartWalReclamationError::RetainedBoundaryMissing {
                        position: floor.get(),
                    },
                )?,
            None => self.durable_len,
        };
        let retained_count = self.durable_len - retained_start;
        let mut retained_records = Vec::new();
        retained_records
            .try_reserve_exact(retained_count)
            .map_err(
                |_| FileTransactionRestartWalReclamationError::FrameCapacityExhausted {
                    record_count: retained_count,
                },
            )?;
        retained_records.extend(
            self.records[retained_start..self.durable_len]
                .iter()
                .cloned(),
        );
        let new_generation = self
            .generation
            .checked_add(1)
            .ok_or(FileTransactionRestartWalReclamationError::GenerationExhausted)?;
        let layout = PageLayout::for_const::<N>().map_err(|_| {
            FileTransactionRestartWalReclamationError::UnsupportedPhysicalFormat {
                version: self.format.version(),
            }
        })?;
        let logical_high_water = current_high_water.as_ref().map(LogSequenceNumber::get);
        let replacement_format = if self.format == LogFormat::V5 {
            LogFormat::V5
        } else {
            LogFormat::V4
        };
        let frames = build_reclamation_frame_plan(
            &retained_records,
            logical_high_water,
            layout,
            replacement_format,
        )?;
        let retained_logical_record_count =
            u64::try_from(retained_records.len()).map_err(|_| {
                FileTransactionRestartWalReclamationError::FrameCapacityExhausted {
                    record_count: retained_records.len(),
                }
            })?;
        let retained_physical_unit_count = u64::try_from(frames.len()).map_err(|_| {
            FileTransactionRestartWalReclamationError::FrameCapacityExhausted {
                record_count: retained_records.len(),
            }
        })?;
        let permit_anchor = permit.selected_checkpoint_anchor();
        let anchor = (permit_anchor.version(), permit_anchor.value());
        let retained_first = permit
            .retained_first_logical_record()
            .map(LogSequenceNumber::get);
        let header = if replacement_format == LogFormat::V5 {
            let database_file_identity = self
                .database_file_identity
                .ok_or(FileTransactionRestartWalReclamationError::MissingDatabaseFileIdentity)?;
            WalReclamationHeader::V5(build_header_v5_reclaimed(
                V4HeaderMetadata {
                    persistent_id: self.persistent_id,
                    generation: new_generation,
                    retained_first,
                    logical_high_water,
                    allocated_epoch_high_water,
                    selected_checkpoint_anchor: anchor,
                },
                layout.width_u64,
                database_file_identity,
            ))
        } else {
            WalReclamationHeader::V4(build_header_v4(
                self.persistent_id,
                layout.width_u64,
                new_generation,
                retained_first,
                logical_high_water,
                allocated_epoch_high_water,
                anchor,
            ))
        };
        let candidate_path = reclamation_candidate_path(&self.path)
            .ok_or(FileTransactionRestartWalReclamationError::MissingFileName)?;

        if self.consume_fault(FaultPoint::BeforeReclamationCandidateCleanup) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::InjectedFault(
                FaultPoint::BeforeReclamationCandidateCleanup,
            ));
        }
        if let Err(source) = remove_reclamation_candidate_for_effect(
            &candidate_path,
            &self.file,
            &self.parent_directory,
        ) {
            self.poisoned = true;
            return Err(source);
        }
        let mut candidate = match OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&candidate_path)
        {
            Ok(candidate) => candidate,
            Err(source) => {
                self.poisoned = true;
                return Err(FileTransactionRestartWalReclamationError::Io(
                    FileIoError::new(FileIoStage::CreateReclamationCandidate, source),
                ));
            }
        };
        if let Err(source) = candidate.try_lock() {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::Io(
                FileIoError::new(FileIoStage::AcquireReclamationCandidateLock, source.into()),
            ));
        }
        if self.consume_fault(FaultPoint::BeforeReclamationWrite) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::InjectedFault(
                FaultPoint::BeforeReclamationWrite,
            ));
        }
        if let Err(source) = candidate.write_all(header.as_bytes()) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::Io(
                FileIoError::new(FileIoStage::WriteReclamationHeader, source),
            ));
        }
        if self.consume_fault(FaultPoint::DuringReclamationCopy) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::InjectedFault(
                FaultPoint::DuringReclamationCopy,
            ));
        }
        for frame in &frames {
            if let Err(source) = candidate.write_all(frame) {
                self.poisoned = true;
                return Err(FileTransactionRestartWalReclamationError::Io(
                    FileIoError::new(FileIoStage::WriteReclamationFrame, source),
                ));
            }
        }
        if self.consume_fault(FaultPoint::BeforeReclamationCandidateSync) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::InjectedFault(
                FaultPoint::BeforeReclamationCandidateSync,
            ));
        }
        if let Err(source) = candidate.sync_all() {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::Io(
                FileIoError::new(FileIoStage::SyncReclamationCandidate, source),
            ));
        }
        if self.consume_fault(FaultPoint::AfterReclamationCandidateSync) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::InjectedFault(
                FaultPoint::AfterReclamationCandidateSync,
            ));
        }
        if self.consume_fault(FaultPoint::BeforeReclamationRename) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::InjectedFault(
                FaultPoint::BeforeReclamationRename,
            ));
        }
        if let Err(source) = fs::rename(&candidate_path, &self.path) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::Io(
                FileIoError::new(FileIoStage::RenameReclamationCandidate, source),
            ));
        }
        let old_file = std::mem::replace(&mut self.file, candidate);
        self.reclamation_old_file = Some(old_file);
        if self.consume_fault(FaultPoint::AfterReclamationRename) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::InjectedFault(
                FaultPoint::AfterReclamationRename,
            ));
        }
        if self.consume_fault(FaultPoint::DuringReclamationDirectorySync) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::InjectedFault(
                FaultPoint::DuringReclamationDirectorySync,
            ));
        }
        if let Err(source) = self.parent_directory.sync_all() {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::Io(
                FileIoError::new(FileIoStage::SyncReclamationDirectory, source),
            ));
        }
        self.reclamation_old_file = None;
        if let Err(source) = self.file.seek(SeekFrom::End(0)) {
            self.poisoned = true;
            return Err(FileTransactionRestartWalReclamationError::Io(
                FileIoError::new(FileIoStage::SeekEnd, source),
            ));
        }
        self.format = replacement_format;
        self.generation = new_generation;
        self.reclaimed_retained_first = permit.retained_first_logical_record().cloned();
        self.reclaimed_logical_high_water = current_high_water.clone();
        self.reclaimed_allocated_epoch_high_water = Some(allocated_epoch_high_water);
        self.selected_checkpoint_anchor = Some(anchor);
        self.records = retained_records;
        self.durable_len = self.records.len();
        self.next_position = match current_high_water {
            Some(position) => position.get().checked_add(1),
            None => Some(1),
        };
        self.poisoned = false;

        Ok(
            DurableTransactionRestartWalReclamationEffectObservation::new(
                DurableTransactionRestartWalReclamationReplacementObservation::new(
                    permit.source_generation(),
                    new_generation,
                    replacement_format.version(),
                ),
                permit.retained_first_logical_record().cloned(),
                permit.durable_frontier().cloned(),
                retained_logical_record_count,
                retained_physical_unit_count,
                allocated_epoch_high_water,
            ),
        )
    }
}

impl<const N: usize> DurableTransactionRestartRetentionMetadataSource for FileCommitLog<N> {
    type Error = FileTransactionRestartRetentionMetadataSourceError;

    fn observe_restart_retention_metadata(
        &mut self,
    ) -> Result<DurableTransactionRestartRetentionMetadataObservation, Self::Error> {
        if self.poisoned {
            return Err(FileTransactionRestartRetentionMetadataSourceError::PoisonedWriter);
        }
        let allocated_epoch_high_water = self
            .allocated_epoch_high_water()
            .ok_or(FileTransactionRestartRetentionMetadataSourceError::NoAllocatedEpoch)?;
        Ok(DurableTransactionRestartRetentionMetadataObservation::new(
            self.lineage.clone(),
            allocated_epoch_high_water,
            None,
        ))
    }
}

impl<const N: usize> LogDurability for FileCommitLog<N> {
    type Error = FileCommitLogError;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn flush_through(&mut self, position: &LogSequenceNumber) -> Result<(), Self::Error> {
        if self.poisoned {
            return Err(FileCommitLogError::PoisonedWriter);
        }
        if !self.lineage.same_lineage(position.lineage()) {
            return Err(FileCommitLogError::ForeignFlushPosition(position.clone()));
        }
        let record_index = self
            .records
            .iter()
            .position(|record| record.position() == position)
            .ok_or_else(|| FileCommitLogError::UnknownFlushPosition(position.clone()))?;
        let requested_durable_len = record_index + 1;
        if requested_durable_len <= self.durable_len {
            return Ok(());
        }
        if self.consume_fault(FaultPoint::BeforeFlush) {
            return Err(FileCommitLogError::InjectedFault(FaultPoint::BeforeFlush));
        }

        self.sync_file(FileIoStage::SyncCommitPrefix, false)
            .map_err(FileCommitLogError::Io)?;
        let marker = build_frame(self.format, FrameKind::DurableThrough, position.get(), 0, 0);
        self.write_frame(&marker, FileIoStage::WriteDurableMarker, true)
            .map_err(FileCommitLogError::Io)?;
        self.sync_file(FileIoStage::SyncDurableMarker, true)
            .map_err(FileCommitLogError::Io)?;
        self.durable_len = requested_durable_len;

        if self.consume_fault(FaultPoint::AfterFlush) {
            Err(FileCommitLogError::InjectedFault(FaultPoint::AfterFlush))
        } else {
            Ok(())
        }
    }
}

impl<const N: usize> CommitLog<TransactionCommitRecord> for FileCommitLog<N> {
    fn append_commit(
        &mut self,
        record: &TransactionCommitRecord,
    ) -> Result<LogSequenceNumber, Self::Error> {
        if self.poisoned {
            return Err(FileCommitLogError::PoisonedWriter);
        }
        let position_value = self
            .next_position
            .ok_or(FileCommitLogError::PositionSpaceExhausted)?;
        if self.consume_fault(FaultPoint::BeforeAppend) {
            return Err(FileCommitLogError::InjectedFault(FaultPoint::BeforeAppend));
        }
        self.records
            .try_reserve(1)
            .map_err(|_| FileCommitLogError::RecordCapacityExhausted)?;

        let transaction = StoredTransactionIdentity::from_transaction_id(record.transaction_id());
        let frame = build_frame(
            self.format,
            FrameKind::CommitRecord,
            position_value,
            transaction.epoch,
            transaction.sequence,
        );
        self.write_frame(&frame, FileIoStage::WriteCommitFrame, true)
            .map_err(FileCommitLogError::Io)?;

        let position = self.lineage.position(position_value);
        self.records.push(FileLogRecord {
            position: position.clone(),
            kind: FileLogRecordKind::TransactionCommit {
                transaction_epoch: transaction.epoch,
                transaction_sequence: transaction.sequence,
            },
        });
        self.next_position = position_value.checked_add(1);

        if self.consume_fault(FaultPoint::AfterAppend) {
            Err(FileCommitLogError::InjectedFault(FaultPoint::AfterAppend))
        } else {
            Ok(position)
        }
    }
}

impl<const LOG_N: usize, const PAGE_N: usize> PageLog<PAGE_N> for FileCommitLog<LOG_N> {
    fn append_page(
        &mut self,
        page: &UnloggedPage<PAGE_N>,
    ) -> Result<LogSequenceNumber, Self::Error> {
        self.append_page_internal(page)
    }
}

impl<const LOG_N: usize, const PAGE_N: usize> TransactionPageLog<PAGE_N> for FileCommitLog<LOG_N> {
    fn append_transaction_page(
        &mut self,
        record: &TransactionPageWriteRecord<'_, PAGE_N>,
    ) -> Result<LogSequenceNumber, Self::Error> {
        self.append_transaction_page_internal(record)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
enum FrameKind {
    EpochAllocation = 1,
    CommitRecord = 2,
    DurableThrough = 3,
    PageHeader = 4,
    PageData = 5,
    TransactionPageHeader = 6,
    TransactionPageOwner = 7,
}

impl FrameKind {
    fn from_u16(value: u16, format: LogFormat) -> Option<Self> {
        match value {
            1 => Some(Self::EpochAllocation),
            2 => Some(Self::CommitRecord),
            3 => Some(Self::DurableThrough),
            4 if format.supports_pages() => Some(Self::PageHeader),
            5 if format.supports_pages() => Some(Self::PageData),
            6 if format.supports_transaction_pages() => Some(Self::TransactionPageHeader),
            7 if format.supports_transaction_pages() => Some(Self::TransactionPageOwner),
            _ => None,
        }
    }

    const fn code(self) -> u16 {
        match self {
            Self::EpochAllocation => 1,
            Self::CommitRecord => 2,
            Self::DurableThrough => 3,
            Self::PageHeader => 4,
            Self::PageData => 5,
            Self::TransactionPageHeader => 6,
            Self::TransactionPageOwner => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DecodedFrame {
    kind: FrameKind,
    payload0: u64,
    payload1: u64,
    payload2: u64,
    payload2_bytes: [u8; PAGE_CHUNK_WIDTH],
}

struct OpenState<const N: usize> {
    lineage: LogLineage,
    page_layout: Option<PageLayout>,
    records: Vec<FileLogRecord<N>>,
    durable_len: usize,
    last_durable_position: Option<u64>,
    last_completed_position: u64,
    highest_allocated_epoch: u64,
    next_epoch: Option<NonZeroU64>,
    next_position: Option<u64>,
    pending_page: Option<PendingPageRecord<N>>,
}

impl<const N: usize> OpenState<N> {
    fn new(
        lineage: LogLineage,
        page_layout: Option<PageLayout>,
        highest_allocated_epoch: u64,
        next_position: Option<u64>,
        last_completed_position: u64,
        last_durable_position: Option<u64>,
    ) -> Self {
        Self {
            lineage,
            page_layout,
            records: Vec::new(),
            durable_len: 0,
            last_durable_position,
            last_completed_position,
            highest_allocated_epoch,
            next_epoch: highest_allocated_epoch
                .checked_add(1)
                .and_then(NonZeroU64::new),
            next_position,
            pending_page: None,
        }
    }

    fn pending_page_header_offset(&self) -> Option<u64> {
        self.pending_page
            .as_ref()
            .map(PendingPageRecord::header_offset)
    }

    fn apply_frame(&mut self, frame: DecodedFrame, offset: u64) -> Result<(), FileOpenError> {
        if self.pending_page.is_some() {
            return self.apply_pending_page_frame(frame, offset);
        }
        match frame.kind {
            FrameKind::EpochAllocation => self.apply_epoch_frame(frame, offset),
            FrameKind::CommitRecord => self.apply_commit_frame(frame, offset),
            FrameKind::DurableThrough => self.apply_marker_frame(frame, offset),
            FrameKind::PageHeader => {
                self.apply_page_header_frame(frame, offset, PendingPageOwnership::Raw)
            }
            FrameKind::TransactionPageHeader => {
                self.apply_page_header_frame(frame, offset, PendingPageOwnership::AwaitingOwner)
            }
            FrameKind::TransactionPageOwner => Err(FileOpenError::Format(FileFormatError::new(
                offset + 4,
                FileFormatErrorReason::TransactionPageOwnerWithoutHeader,
            ))),
            FrameKind::PageData => Err(FileOpenError::Format(FileFormatError::new(
                offset + 4,
                FileFormatErrorReason::PageDataWithoutHeader,
            ))),
        }
    }

    fn apply_pending_page_frame(
        &mut self,
        frame: DecodedFrame,
        offset: u64,
    ) -> Result<(), FileOpenError> {
        let ownership = match self.pending_page.as_ref() {
            Some(pending) => pending.ownership(),
            None => {
                return Err(FileOpenError::Format(FileFormatError::new(
                    offset + 4,
                    FileFormatErrorReason::PageDataWithoutHeader,
                )));
            }
        };
        match ownership {
            PendingPageOwnership::AwaitingOwner => {
                self.apply_pending_transaction_page_owner(frame, offset)
            }
            PendingPageOwnership::Raw if frame.kind == FrameKind::TransactionPageOwner => {
                Err(FileOpenError::Format(FileFormatError::new(
                    offset + 4,
                    FileFormatErrorReason::TransactionPageOwnerWithoutHeader,
                )))
            }
            PendingPageOwnership::Owned(_) if frame.kind == FrameKind::TransactionPageOwner => {
                Err(FileOpenError::Format(FileFormatError::new(
                    offset + 4,
                    FileFormatErrorReason::TransactionPageOwnerDuplicate,
                )))
            }
            PendingPageOwnership::Raw | PendingPageOwnership::Owned(_) => {
                self.apply_pending_page_data_frame(frame, offset)
            }
        }
    }

    fn apply_pending_transaction_page_owner(
        &mut self,
        frame: DecodedFrame,
        offset: u64,
    ) -> Result<(), FileOpenError> {
        if frame.kind != FrameKind::TransactionPageOwner {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 4,
                FileFormatErrorReason::TransactionPageOwnerInterruptedByFrameKind {
                    actual: frame.kind.code(),
                },
            )));
        }
        let pending_position = match self.pending_page.as_ref() {
            Some(pending) => pending.position(),
            None => {
                return Err(FileOpenError::Format(FileFormatError::new(
                    offset + 4,
                    FileFormatErrorReason::TransactionPageOwnerWithoutHeader,
                )));
            }
        };
        if frame.payload0 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::TransactionPageOwnerParentPositionZero,
            )));
        }
        if frame.payload0 != pending_position {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::TransactionPageOwnerParentMismatch {
                    expected: pending_position,
                    actual: frame.payload0,
                },
            )));
        }
        if frame.payload1 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::TransactionPageOwnerEpochZero,
            )));
        }
        if frame.payload1 > self.highest_allocated_epoch {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::TransactionPageOwnerEpochUnallocated {
                    actual: frame.payload1,
                    highest_allocated: self.highest_allocated_epoch,
                },
            )));
        }
        if frame.payload2 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 32,
                FileFormatErrorReason::TransactionPageOwnerSequenceZero,
            )));
        }
        let owner = StoredTransactionIdentity::from_epoch_sequence(frame.payload1, frame.payload2);
        match self.pending_page.as_mut() {
            Some(pending) => pending.set_owner(owner),
            None => {
                return Err(FileOpenError::Format(FileFormatError::new(
                    offset + 4,
                    FileFormatErrorReason::TransactionPageOwnerWithoutHeader,
                )));
            }
        }
        Ok(())
    }

    fn apply_pending_page_data_frame(
        &mut self,
        frame: DecodedFrame,
        offset: u64,
    ) -> Result<(), FileOpenError> {
        if frame.kind != FrameKind::PageData {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 4,
                FileFormatErrorReason::PageDataInterruptedByFrameKind {
                    actual: frame.kind.code(),
                },
            )));
        }
        let pending = match self.pending_page.as_mut() {
            Some(pending) => pending,
            None => {
                return Err(FileOpenError::Format(FileFormatError::new(
                    offset + 4,
                    FileFormatErrorReason::PageDataWithoutHeader,
                )));
            }
        };
        if frame.payload0 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::PageDataParentPositionZero,
            )));
        }
        if frame.payload0 != pending.position() {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::PageDataParentMismatch {
                    expected: pending.position(),
                    actual: frame.payload0,
                },
            )));
        }
        let expected_chunk_index = u64::try_from(pending.next_chunk_index())
            .map_err(|_| FileOpenError::RecordCapacityExhausted)?;
        if frame.payload1 != expected_chunk_index {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::PageDataChunkIndexOutOfSequence {
                    expected: expected_chunk_index,
                    actual: frame.payload1,
                },
            )));
        }
        pending.copy_chunk(
            frame.payload2_bytes,
            self.page_layout.ok_or_else(|| {
                FileOpenError::Format(FileFormatError::new(
                    offset + 32,
                    FileFormatErrorReason::PageDataWithoutHeader,
                ))
            })?,
        );
        if pending.requires_zero_padding(self.page_layout.ok_or_else(|| {
            FileOpenError::Format(FileFormatError::new(
                offset + 32,
                FileFormatErrorReason::PageDataWithoutHeader,
            ))
        })?) {
            let logical_len = self
                .page_layout
                .ok_or_else(|| {
                    FileOpenError::Format(FileFormatError::new(
                        offset + 32,
                        FileFormatErrorReason::PageDataWithoutHeader,
                    ))
                })?
                .logical_bytes_for_chunk(pending.next_chunk_index() - 1);
            if frame.payload2_bytes[logical_len..]
                .iter()
                .any(|byte| *byte != 0)
            {
                return Err(FileOpenError::Format(FileFormatError::new(
                    offset + 32,
                    FileFormatErrorReason::PageDataFinalPaddingNonzero,
                )));
            }
        }
        if pending.is_complete(self.page_layout.ok_or_else(|| {
            FileOpenError::Format(FileFormatError::new(
                offset + 32,
                FileFormatErrorReason::PageDataWithoutHeader,
            ))
        })?) {
            let pending = match self.pending_page.take() {
                Some(pending) => pending,
                None => {
                    return Err(FileOpenError::Format(FileFormatError::new(
                        offset + 32,
                        FileFormatErrorReason::PageDataWithoutHeader,
                    )));
                }
            };
            self.records
                .try_reserve(1)
                .map_err(|_| FileOpenError::RecordCapacityExhausted)?;
            let record = pending.into_record(&self.lineage)?;
            self.last_completed_position = record.position().get();
            self.next_position = self.last_completed_position.checked_add(1);
            self.records.push(record);
        }
        Ok(())
    }

    fn apply_epoch_frame(&mut self, frame: DecodedFrame, offset: u64) -> Result<(), FileOpenError> {
        if frame.payload0 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::EpochValueZero,
            )));
        }
        if frame.payload1 != 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::UnexpectedNonzeroPayload {
                    field: "payload1",
                    actual: frame.payload1,
                },
            )));
        }
        if frame.payload2 != 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 32,
                FileFormatErrorReason::UnexpectedNonzeroPayload {
                    field: "payload2",
                    actual: frame.payload2,
                },
            )));
        }
        let expected_epoch = self.next_epoch.ok_or_else(|| {
            FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::EpochSpaceExhausted,
            ))
        })?;
        if frame.payload0 != expected_epoch.get() {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::EpochOutOfSequence {
                    expected: expected_epoch.get(),
                    actual: frame.payload0,
                },
            )));
        }

        self.highest_allocated_epoch = frame.payload0;
        self.next_epoch = frame.payload0.checked_add(1).and_then(NonZeroU64::new);
        Ok(())
    }

    fn apply_commit_frame(
        &mut self,
        frame: DecodedFrame,
        offset: u64,
    ) -> Result<(), FileOpenError> {
        if frame.payload0 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::CommitPositionZero,
            )));
        }
        if frame.payload1 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::CommitEpochZero,
            )));
        }
        if frame.payload2 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 32,
                FileFormatErrorReason::CommitSequenceZero,
            )));
        }

        let expected_position = self.next_position.ok_or_else(|| {
            FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::CommitPositionSpaceExhausted,
            ))
        })?;
        if frame.payload0 != expected_position {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::CommitPositionOutOfSequence {
                    expected: expected_position,
                    actual: frame.payload0,
                },
            )));
        }
        if frame.payload1 > self.highest_allocated_epoch {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::CommitEpochUnallocated {
                    actual: frame.payload1,
                    highest_allocated: self.highest_allocated_epoch,
                },
            )));
        }

        let transaction =
            StoredTransactionIdentity::from_epoch_sequence(frame.payload1, frame.payload2);
        if self
            .records
            .iter()
            .any(|record| record.transaction_identity() == Some(transaction))
        {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::DuplicateTransactionIdentity {
                    epoch: frame.payload1,
                    sequence: frame.payload2,
                },
            )));
        }
        self.records
            .try_reserve(1)
            .map_err(|_| FileOpenError::RecordCapacityExhausted)?;
        self.records.push(FileLogRecord {
            position: self.lineage.position(frame.payload0),
            kind: FileLogRecordKind::TransactionCommit {
                transaction_epoch: frame.payload1,
                transaction_sequence: frame.payload2,
            },
        });
        self.last_completed_position = frame.payload0;
        self.next_position = frame.payload0.checked_add(1);
        Ok(())
    }

    fn apply_marker_frame(
        &mut self,
        frame: DecodedFrame,
        offset: u64,
    ) -> Result<(), FileOpenError> {
        if frame.payload0 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::MarkerPositionZero,
            )));
        }
        if frame.payload1 != 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::UnexpectedNonzeroPayload {
                    field: "payload1",
                    actual: frame.payload1,
                },
            )));
        }
        if frame.payload2 != 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 32,
                FileFormatErrorReason::UnexpectedNonzeroPayload {
                    field: "payload2",
                    actual: frame.payload2,
                },
            )));
        }

        let previous = self.last_durable_position.unwrap_or(0);
        if frame.payload0 <= previous {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::MarkerDoesNotAdvance {
                    previous,
                    actual: frame.payload0,
                },
            )));
        }
        if frame.payload0 > self.last_completed_position {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::MarkerReferencesUnknownCommit {
                    actual: frame.payload0,
                    highest_committed: self.last_completed_position,
                },
            )));
        }

        self.durable_len = self
            .records
            .iter()
            .position(|record| record.position().get() == frame.payload0)
            .map(|index| index + 1)
            .ok_or_else(|| {
                FileOpenError::Format(FileFormatError::new(
                    offset + 16,
                    FileFormatErrorReason::MarkerReferencesUnknownCommit {
                        actual: frame.payload0,
                        highest_committed: self.last_completed_position,
                    },
                ))
            })?;
        self.last_durable_position = Some(frame.payload0);
        Ok(())
    }

    fn apply_page_header_frame(
        &mut self,
        frame: DecodedFrame,
        offset: u64,
        ownership: PendingPageOwnership,
    ) -> Result<(), FileOpenError> {
        if frame.payload0 == 0 {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::PageHeaderPositionZero,
            )));
        }
        let expected_position = self.next_position.ok_or_else(|| {
            FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::PageHeaderPositionSpaceExhausted,
            ))
        })?;
        if frame.payload0 != expected_position {
            return Err(FileOpenError::Format(FileFormatError::new(
                offset + 16,
                FileFormatErrorReason::PageHeaderPositionOutOfSequence {
                    expected: expected_position,
                    actual: frame.payload0,
                },
            )));
        }
        let page_number = PageNumber::new(frame.payload1).ok_or_else(|| {
            FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::PageNumberZero,
            ))
        })?;
        let layout = self.page_layout.ok_or_else(|| {
            FileOpenError::Format(FileFormatError::new(
                offset + 24,
                FileFormatErrorReason::PageDataWithoutHeader,
            ))
        })?;
        self.pending_page = Some(PendingPageRecord::new(
            offset,
            frame.payload0,
            page_number,
            PageVersion::new(frame.payload2),
            ownership,
            layout,
        ));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingPageOwnership {
    Raw,
    AwaitingOwner,
    Owned(StoredTransactionIdentity),
}

#[derive(Debug)]
struct PendingPageRecord<const N: usize> {
    header_offset: u64,
    position: u64,
    page_number: PageNumber,
    page_version: PageVersion,
    ownership: PendingPageOwnership,
    bytes: [u8; N],
    next_chunk_index: usize,
    expected_chunk_count: usize,
}

impl<const N: usize> PendingPageRecord<N> {
    fn new(
        header_offset: u64,
        position: u64,
        page_number: PageNumber,
        page_version: PageVersion,
        ownership: PendingPageOwnership,
        layout: PageLayout,
    ) -> Self {
        Self {
            header_offset,
            position,
            page_number,
            page_version,
            ownership,
            bytes: [0_u8; N],
            next_chunk_index: 0,
            expected_chunk_count: layout.chunk_count,
        }
    }

    const fn header_offset(&self) -> u64 {
        self.header_offset
    }

    const fn position(&self) -> u64 {
        self.position
    }

    const fn ownership(&self) -> PendingPageOwnership {
        self.ownership
    }

    fn set_owner(&mut self, owner: StoredTransactionIdentity) {
        self.ownership = PendingPageOwnership::Owned(owner);
    }

    const fn next_chunk_index(&self) -> usize {
        self.next_chunk_index
    }

    fn copy_chunk(&mut self, chunk: [u8; PAGE_CHUNK_WIDTH], layout: PageLayout) {
        let chunk_index = self.next_chunk_index;
        let start = chunk_index * PAGE_CHUNK_WIDTH;
        let logical_len = layout.logical_bytes_for_chunk(chunk_index);
        let end = start + logical_len;
        for (destination, source) in self.bytes[start..end]
            .iter_mut()
            .zip(chunk[..logical_len].iter())
        {
            *destination = *source;
        }
        self.next_chunk_index += 1;
    }

    fn requires_zero_padding(&self, layout: PageLayout) -> bool {
        self.next_chunk_index > 0
            && self.next_chunk_index == self.expected_chunk_count
            && layout.final_chunk_len < PAGE_CHUNK_WIDTH
    }

    fn is_complete(&self, layout: PageLayout) -> bool {
        self.next_chunk_index == layout.chunk_count
    }

    fn into_record(self, lineage: &LogLineage) -> Result<FileLogRecord<N>, FileOpenError> {
        let page = FilePageWriteRecord {
            page_number: self.page_number,
            page_version: self.page_version,
            bytes: self.bytes,
        };
        let kind = match self.ownership {
            PendingPageOwnership::Raw => FileLogRecordKind::PageWrite(page),
            PendingPageOwnership::Owned(owner) => {
                FileLogRecordKind::TransactionPageWrite(FileTransactionPageWriteRecord {
                    transaction_epoch: owner.epoch,
                    transaction_sequence: owner.sequence,
                    page,
                })
            }
            PendingPageOwnership::AwaitingOwner => {
                return Err(FileOpenError::Format(FileFormatError::new(
                    self.header_offset + 4,
                    FileFormatErrorReason::TransactionPageOwnerMissing,
                )));
            }
        };
        Ok(FileLogRecord {
            position: lineage.position(self.position),
            kind,
        })
    }
}

fn commit_log_parent_path(path: &Path) -> Result<&Path, FileCreateError> {
    let parent = match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => return Err(FileCreateError::MissingParentDirectory),
    };
    Ok(parent)
}

fn sync_parent_directory(path: &Path) -> Result<File, FileCreateError> {
    let parent = commit_log_parent_path(path)?;
    let directory = File::open(parent).map_err(|source| {
        FileCreateError::Io(FileIoError::new(FileIoStage::OpenParentDirectory, source))
    })?;
    directory.sync_all().map_err(|source| {
        FileCreateError::Io(FileIoError::new(FileIoStage::SyncParentDirectory, source))
    })?;
    Ok(directory)
}

fn open_parent_directory_for_open(path: &Path) -> Result<File, FileOpenError> {
    let parent = match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => {
            return Err(FileOpenError::Io(FileIoError::new(
                FileIoStage::OpenParentDirectory,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "commit-log path has no parent directory",
                ),
            )));
        }
    };
    File::open(parent).map_err(|source| {
        FileOpenError::Io(FileIoError::new(FileIoStage::OpenParentDirectory, source))
    })
}

fn reclamation_candidate_path(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?;
    let mut candidate_name = OsString::from(file_name);
    candidate_name.push(".reclaim-candidate");
    Some(path.with_file_name(candidate_name))
}

#[cfg(unix)]
fn metadata_identifies_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn metadata_identifies_same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    false
}

fn cleanup_reclamation_candidate(
    selected_path: &Path,
    selected_file: &File,
    parent_directory: &File,
) -> Result<(), FileOpenError> {
    let Some(candidate_path) = reclamation_candidate_path(selected_path) else {
        return Ok(());
    };
    match fs::symlink_metadata(&candidate_path) {
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(FileOpenError::Io(FileIoError::new(
                FileIoStage::ReadReclamationCandidateMetadata,
                source,
            )));
        }
    };
    match fs::metadata(&candidate_path) {
        Ok(candidate_metadata) => {
            let selected_metadata = selected_file.metadata().map_err(|source| {
                FileOpenError::Io(FileIoError::new(FileIoStage::ReadMetadata, source))
            })?;
            if metadata_identifies_same_file(&selected_metadata, &candidate_metadata) {
                return Err(FileOpenError::Io(FileIoError::new(
                    FileIoStage::ReadReclamationCandidateMetadata,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "reclamation candidate aliases selected WAL",
                    ),
                )));
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(FileOpenError::Io(FileIoError::new(
                FileIoStage::ReadReclamationCandidateMetadata,
                source,
            )));
        }
    }
    fs::remove_file(&candidate_path).map_err(|source| {
        FileOpenError::Io(FileIoError::new(
            FileIoStage::RemoveReclamationCandidate,
            source,
        ))
    })?;
    parent_directory.sync_all().map_err(|source| {
        FileOpenError::Io(FileIoError::new(
            FileIoStage::SyncReclamationDirectory,
            source,
        ))
    })
}

fn remove_reclamation_candidate_for_effect(
    candidate_path: &Path,
    selected_file: &File,
    parent_directory: &File,
) -> Result<(), FileTransactionRestartWalReclamationError> {
    match fs::symlink_metadata(candidate_path) {
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(FileTransactionRestartWalReclamationError::Io(
                FileIoError::new(FileIoStage::ReadReclamationCandidateMetadata, source),
            ));
        }
    };
    match fs::metadata(candidate_path) {
        Ok(candidate_metadata) => {
            let selected_metadata = selected_file.metadata().map_err(|source| {
                FileTransactionRestartWalReclamationError::Io(FileIoError::new(
                    FileIoStage::ReadMetadata,
                    source,
                ))
            })?;
            if metadata_identifies_same_file(&selected_metadata, &candidate_metadata) {
                return Err(FileTransactionRestartWalReclamationError::CandidateAliasesSelected);
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(FileTransactionRestartWalReclamationError::Io(
                FileIoError::new(FileIoStage::ReadReclamationCandidateMetadata, source),
            ));
        }
    }
    fs::remove_file(candidate_path).map_err(|source| {
        FileTransactionRestartWalReclamationError::Io(FileIoError::new(
            FileIoStage::RemoveReclamationCandidate,
            source,
        ))
    })?;
    parent_directory.sync_all().map_err(|source| {
        FileTransactionRestartWalReclamationError::Io(FileIoError::new(
            FileIoStage::SyncReclamationDirectory,
            source,
        ))
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderExpectation {
    V1,
    V2(PageLayout),
    V3OrLater(PageLayout),
}

impl HeaderExpectation {
    const fn accepts(self, format: LogFormat) -> bool {
        match self {
            Self::V1 => matches!(format, LogFormat::V1),
            Self::V2(_) => matches!(format, LogFormat::V2),
            Self::V3OrLater(_) => {
                matches!(format, LogFormat::V3 | LogFormat::V4 | LogFormat::V5)
            }
        }
    }

    const fn page_layout(self) -> Option<PageLayout> {
        match self {
            Self::V1 => None,
            Self::V2(layout) | Self::V3OrLater(layout) => Some(layout),
        }
    }
}

fn parse_header(
    header: &[u8; HEADER_LENGTH],
    expectation: HeaderExpectation,
) -> Result<PersistentLogId, FileFormatError> {
    if header[..8] != HEADER_MAGIC {
        return Err(FileFormatError::new(0, FileFormatErrorReason::HeaderMagic));
    }
    let version = read_u16(header, 8);
    let format = match version {
        FORMAT_VERSION_V1 => LogFormat::V1,
        FORMAT_VERSION_V2 => LogFormat::V2,
        FORMAT_VERSION_V3 => LogFormat::V3,
        _ => {
            return Err(FileFormatError::new(
                8,
                FileFormatErrorReason::HeaderVersion { actual: version },
            ));
        }
    };
    if !expectation.accepts(format) {
        return Err(FileFormatError::new(
            8,
            FileFormatErrorReason::HeaderVersion { actual: version },
        ));
    }
    let length = read_u16(header, 10);
    if usize::from(length) != HEADER_LENGTH {
        return Err(FileFormatError::new(
            10,
            FileFormatErrorReason::HeaderLength { actual: length },
        ));
    }
    let flags = read_u32(header, 12);
    if flags != 0 {
        return Err(FileFormatError::new(
            12,
            FileFormatErrorReason::HeaderFlags { actual: flags },
        ));
    }
    let lineage_raw = read_u128(header, 16);
    let persistent_id = match PersistentLogId::new(lineage_raw) {
        Some(id) => id,
        None => {
            return Err(FileFormatError::new(
                16,
                FileFormatErrorReason::LineageIdZero,
            ));
        }
    };

    match expectation {
        HeaderExpectation::V1 => {
            if header[32..56].iter().any(|byte| *byte != 0) {
                return Err(FileFormatError::new(
                    32,
                    FileFormatErrorReason::HeaderReserved,
                ));
            }
        }
        HeaderExpectation::V2(layout) | HeaderExpectation::V3OrLater(layout) => {
            let page_width = read_u64(header, HEADER_V2_PAGE_WIDTH_OFFSET);
            if page_width == 0 {
                return Err(FileFormatError::new(
                    HEADER_V2_PAGE_WIDTH_OFFSET as u64,
                    FileFormatErrorReason::HeaderPageWidthZero,
                ));
            }
            if page_width != layout.width_u64 {
                return Err(FileFormatError::new(
                    HEADER_V2_PAGE_WIDTH_OFFSET as u64,
                    FileFormatErrorReason::HeaderPageWidthMismatch {
                        expected: layout.width_u64,
                        actual: page_width,
                    },
                ));
            }
            if header[HEADER_V2_RESERVED_OFFSET..HEADER_CHECKSUM_OFFSET]
                .iter()
                .any(|byte| *byte != 0)
            {
                return Err(FileFormatError::new(
                    HEADER_V2_RESERVED_OFFSET as u64,
                    FileFormatErrorReason::HeaderReserved,
                ));
            }
        }
    }
    let actual_checksum = read_u64(header, HEADER_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&header[..HEADER_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(FileFormatError::new(
            56,
            FileFormatErrorReason::HeaderChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        ));
    }
    Ok(persistent_id)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct V4HeaderMetadata {
    persistent_id: PersistentLogId,
    generation: u64,
    retained_first: Option<u64>,
    logical_high_water: Option<u64>,
    allocated_epoch_high_water: NonZeroU64,
    selected_checkpoint_anchor: (u16, u128),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct V5HeaderMetadata {
    persistent_id: PersistentLogId,
    generation: u64,
    retained_first: Option<u64>,
    logical_high_water: Option<u64>,
    allocated_epoch_high_water: Option<NonZeroU64>,
    selected_checkpoint_anchor: Option<(u16, u128)>,
    database_file_identity: DatabaseFileHeaderIdentity,
}

fn parse_header_v4(
    header: &[u8; HEADER_V4_LENGTH],
    layout: PageLayout,
) -> Result<V4HeaderMetadata, FileFormatError> {
    if header[..8] != HEADER_MAGIC {
        return Err(FileFormatError::new(0, FileFormatErrorReason::HeaderMagic));
    }
    let version = read_u16(header, 8);
    if version != FORMAT_VERSION_V4 {
        return Err(FileFormatError::new(
            8,
            FileFormatErrorReason::HeaderVersion { actual: version },
        ));
    }
    let length = read_u16(header, 10);
    if usize::from(length) != HEADER_V4_LENGTH {
        return Err(FileFormatError::new(
            10,
            FileFormatErrorReason::HeaderV4Length { actual: length },
        ));
    }
    let flags = read_u32(header, 12);
    if flags != 0 {
        return Err(FileFormatError::new(
            12,
            FileFormatErrorReason::HeaderFlags { actual: flags },
        ));
    }
    let persistent_id = PersistentLogId::new(read_u128(header, 16))
        .ok_or_else(|| FileFormatError::new(16, FileFormatErrorReason::LineageIdZero))?;
    let page_width = read_u64(header, HEADER_V2_PAGE_WIDTH_OFFSET);
    if page_width == 0 {
        return Err(FileFormatError::new(
            HEADER_V2_PAGE_WIDTH_OFFSET as u64,
            FileFormatErrorReason::HeaderPageWidthZero,
        ));
    }
    if page_width != layout.width_u64 {
        return Err(FileFormatError::new(
            HEADER_V2_PAGE_WIDTH_OFFSET as u64,
            FileFormatErrorReason::HeaderPageWidthMismatch {
                expected: layout.width_u64,
                actual: page_width,
            },
        ));
    }
    let generation = read_u64(header, HEADER_V4_GENERATION_OFFSET);
    if generation == 0 {
        return Err(FileFormatError::new(
            HEADER_V4_GENERATION_OFFSET as u64,
            FileFormatErrorReason::HeaderV4GenerationZero,
        ));
    }
    let retained_first_raw = read_u64(header, HEADER_V4_RETAINED_FIRST_OFFSET);
    let retained_first = match header[HEADER_V4_RETAINED_FIRST_PRESENCE_OFFSET] {
        0 if retained_first_raw == 0 => None,
        0 => {
            return Err(FileFormatError::new(
                HEADER_V4_RETAINED_FIRST_OFFSET as u64,
                FileFormatErrorReason::HeaderV4Reserved,
            ));
        }
        1 => Some(retained_first_raw),
        actual => {
            return Err(FileFormatError::new(
                HEADER_V4_RETAINED_FIRST_PRESENCE_OFFSET as u64,
                FileFormatErrorReason::HeaderV4RetainedFirstPresence { actual },
            ));
        }
    };
    if retained_first == Some(0) {
        return Err(FileFormatError::new(
            HEADER_V4_RETAINED_FIRST_OFFSET as u64,
            FileFormatErrorReason::HeaderV4RetainedFirstZero,
        ));
    }
    let high_water_raw = read_u64(header, HEADER_V4_LOGICAL_HIGH_WATER_OFFSET);
    let logical_high_water = match header[HEADER_V4_LOGICAL_HIGH_WATER_PRESENCE_OFFSET] {
        0 if high_water_raw == 0 => None,
        0 => {
            return Err(FileFormatError::new(
                HEADER_V4_LOGICAL_HIGH_WATER_OFFSET as u64,
                FileFormatErrorReason::HeaderV4Reserved,
            ));
        }
        1 => Some(high_water_raw),
        actual => {
            return Err(FileFormatError::new(
                HEADER_V4_LOGICAL_HIGH_WATER_PRESENCE_OFFSET as u64,
                FileFormatErrorReason::HeaderV4LogicalHighWaterPresence { actual },
            ));
        }
    };
    if logical_high_water == Some(0) {
        return Err(FileFormatError::new(
            HEADER_V4_LOGICAL_HIGH_WATER_OFFSET as u64,
            FileFormatErrorReason::HeaderV4LogicalHighWaterZero,
        ));
    }
    match (retained_first, logical_high_water) {
        (Some(_), None) => {
            return Err(FileFormatError::new(
                HEADER_V4_RETAINED_FIRST_OFFSET as u64,
                FileFormatErrorReason::HeaderV4RetainedFirstWithoutHighWater,
            ));
        }
        (Some(retained_first), Some(high_water)) if retained_first > high_water => {
            return Err(FileFormatError::new(
                HEADER_V4_RETAINED_FIRST_OFFSET as u64,
                FileFormatErrorReason::HeaderV4RetainedFirstBeyondHighWater {
                    retained_first,
                    high_water,
                },
            ));
        }
        (None, None | Some(_)) | (Some(_), Some(_)) => {}
    }
    let allocated_epoch_high_water = NonZeroU64::new(read_u64(
        header,
        HEADER_V4_ALLOCATED_EPOCH_HIGH_WATER_OFFSET,
    ))
    .ok_or_else(|| {
        FileFormatError::new(
            HEADER_V4_ALLOCATED_EPOCH_HIGH_WATER_OFFSET as u64,
            FileFormatErrorReason::HeaderV4AllocatedEpochHighWaterZero,
        )
    })?;
    let anchor_version = read_u16(header, HEADER_V4_ANCHOR_VERSION_OFFSET);
    if anchor_version == 0 {
        return Err(FileFormatError::new(
            HEADER_V4_ANCHOR_VERSION_OFFSET as u64,
            FileFormatErrorReason::HeaderV4AnchorVersionZero,
        ));
    }
    if header[HEADER_V4_RESERVED_START..HEADER_V4_ANCHOR_VALUE_OFFSET]
        .iter()
        .chain(header[HEADER_V4_RESERVED_MIDDLE..HEADER_V4_CHECKSUM_OFFSET].iter())
        .any(|byte| *byte != 0)
    {
        return Err(FileFormatError::new(
            HEADER_V4_RESERVED_START as u64,
            FileFormatErrorReason::HeaderV4Reserved,
        ));
    }
    let actual_checksum = read_u64(header, HEADER_V4_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&header[..HEADER_V4_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(FileFormatError::new(
            HEADER_V4_CHECKSUM_OFFSET as u64,
            FileFormatErrorReason::HeaderChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        ));
    }
    Ok(V4HeaderMetadata {
        persistent_id,
        generation,
        retained_first,
        logical_high_water,
        allocated_epoch_high_water,
        selected_checkpoint_anchor: (
            anchor_version,
            read_u128(header, HEADER_V4_ANCHOR_VALUE_OFFSET),
        ),
    })
}

fn parse_header_v5(
    header: &[u8; HEADER_V5_LENGTH],
    layout: PageLayout,
) -> Result<V5HeaderMetadata, FileFormatError> {
    if header[..8] != HEADER_MAGIC {
        return Err(FileFormatError::new(0, FileFormatErrorReason::HeaderMagic));
    }
    let version = read_u16(header, 8);
    if version != FORMAT_VERSION_V5 {
        return Err(FileFormatError::new(
            8,
            FileFormatErrorReason::HeaderVersion { actual: version },
        ));
    }
    let length = read_u16(header, 10);
    if usize::from(length) != HEADER_V5_LENGTH {
        return Err(FileFormatError::new(
            10,
            FileFormatErrorReason::HeaderV5Length { actual: length },
        ));
    }
    let flags = read_u32(header, 12);
    if flags != 0 {
        return Err(FileFormatError::new(
            12,
            FileFormatErrorReason::HeaderFlags { actual: flags },
        ));
    }

    let persistent_id = PersistentLogId::new(read_u128(header, 16))
        .ok_or_else(|| FileFormatError::new(16, FileFormatErrorReason::LineageIdZero))?;
    let page_width = read_u64(header, HEADER_V2_PAGE_WIDTH_OFFSET);
    if page_width == 0 {
        return Err(FileFormatError::new(
            HEADER_V2_PAGE_WIDTH_OFFSET as u64,
            FileFormatErrorReason::HeaderPageWidthZero,
        ));
    }
    if page_width != layout.width_u64 {
        return Err(FileFormatError::new(
            HEADER_V2_PAGE_WIDTH_OFFSET as u64,
            FileFormatErrorReason::HeaderPageWidthMismatch {
                expected: layout.width_u64,
                actual: page_width,
            },
        ));
    }

    let (
        generation,
        retained_first,
        logical_high_water,
        allocated_epoch_high_water,
        selected_checkpoint_anchor,
    ) = match header[HEADER_V5_RECLAMATION_PRESENCE_OFFSET] {
        0 => {
            if header[HEADER_V4_GENERATION_OFFSET..HEADER_V5_IDENTITY_OFFSET]
                .iter()
                .any(|byte| *byte != 0)
            {
                return Err(FileFormatError::new(
                    HEADER_V4_GENERATION_OFFSET as u64,
                    FileFormatErrorReason::HeaderV4Reserved,
                ));
            }
            (0, None, None, None, None)
        }
        1 => {
            let mut v4_header = [0_u8; HEADER_V4_LENGTH];
            v4_header[..HEADER_V4_CHECKSUM_OFFSET]
                .copy_from_slice(&header[..HEADER_V4_CHECKSUM_OFFSET]);
            write_u16(&mut v4_header, 8, FORMAT_VERSION_V4);
            write_u16(&mut v4_header, 10, HEADER_V4_LENGTH_U16);
            v4_header[HEADER_V5_RECLAMATION_PRESENCE_OFFSET] = 0;
            let checksum = checksum_v1(&v4_header[..HEADER_V4_CHECKSUM_OFFSET]);
            write_u64(&mut v4_header, HEADER_V4_CHECKSUM_OFFSET, checksum);
            let metadata = parse_header_v4(&v4_header, layout)?;
            (
                metadata.generation,
                metadata.retained_first,
                metadata.logical_high_water,
                Some(metadata.allocated_epoch_high_water),
                Some(metadata.selected_checkpoint_anchor),
            )
        }
        actual => {
            return Err(FileFormatError::new(
                HEADER_V5_RECLAMATION_PRESENCE_OFFSET as u64,
                FileFormatErrorReason::HeaderV5ReclamationPresence { actual },
            ));
        }
    };
    if header[HEADER_V4_CHECKSUM_OFFSET..HEADER_V5_IDENTITY_OFFSET]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(FileFormatError::new(
            HEADER_V4_CHECKSUM_OFFSET as u64,
            FileFormatErrorReason::HeaderV4Reserved,
        ));
    }

    let mut identity_bytes =
        [0_u8; database_child_identity_codec::DATABASE_CHILD_IDENTITY_V1_LENGTH];
    identity_bytes.copy_from_slice(
        &header[HEADER_V5_IDENTITY_OFFSET
            ..HEADER_V5_IDENTITY_OFFSET
                + database_child_identity_codec::DATABASE_CHILD_IDENTITY_V1_LENGTH],
    );
    let database_file_identity = database_child_identity_codec::decode_database_child_identity(
        &identity_bytes,
    )
    .map_err(|source| {
        FileFormatError::new(
            (HEADER_V5_IDENTITY_OFFSET + source.offset()) as u64,
            FileFormatErrorReason::HeaderDatabaseChildIdentity(source.reason()),
        )
    })?;
    if header[HEADER_V5_IDENTITY_OFFSET
        + database_child_identity_codec::DATABASE_CHILD_IDENTITY_V1_LENGTH
        ..HEADER_V5_RESERVED_END]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(FileFormatError::new(
            (HEADER_V5_IDENTITY_OFFSET
                + database_child_identity_codec::DATABASE_CHILD_IDENTITY_V1_LENGTH)
                as u64,
            FileFormatErrorReason::HeaderV4Reserved,
        ));
    }
    let actual_checksum = read_u64(header, HEADER_V5_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&header[..HEADER_V5_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(FileFormatError::new(
            HEADER_V5_CHECKSUM_OFFSET as u64,
            FileFormatErrorReason::HeaderChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        ));
    }

    Ok(V5HeaderMetadata {
        persistent_id,
        generation,
        retained_first,
        logical_high_water,
        allocated_epoch_high_water,
        selected_checkpoint_anchor,
        database_file_identity,
    })
}

fn parse_frame(
    frame: &[u8; FRAME_LENGTH],
    offset: u64,
    format: LogFormat,
) -> Result<DecodedFrame, FileFormatError> {
    if frame[..4] != FRAME_MAGIC {
        return Err(FileFormatError::new(
            offset,
            FileFormatErrorReason::FrameMagic,
        ));
    }
    let kind_raw = read_u16(frame, 4);
    let kind = FrameKind::from_u16(kind_raw, format).ok_or_else(|| {
        FileFormatError::new(
            offset + 4,
            FileFormatErrorReason::FrameKind { actual: kind_raw },
        )
    })?;
    let version = read_u16(frame, 6);
    if version != format.frame_version() {
        return Err(FileFormatError::new(
            offset + 6,
            FileFormatErrorReason::FrameVersion { actual: version },
        ));
    }
    let flags = read_u32(frame, 8);
    if flags != 0 {
        return Err(FileFormatError::new(
            offset + 8,
            FileFormatErrorReason::FrameFlags { actual: flags },
        ));
    }
    let length = read_u16(frame, 12);
    if usize::from(length) != FRAME_LENGTH {
        return Err(FileFormatError::new(
            offset + 12,
            FileFormatErrorReason::FrameLength { actual: length },
        ));
    }
    if read_u16(frame, 14) != 0 || frame[40..48].iter().any(|byte| *byte != 0) {
        return Err(FileFormatError::new(
            offset + 14,
            FileFormatErrorReason::FrameReserved,
        ));
    }
    let actual_checksum = read_u64(frame, FRAME_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&frame[..FRAME_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(FileFormatError::new(
            offset + 48,
            FileFormatErrorReason::FrameChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        ));
    }

    let mut payload2_bytes = [0_u8; PAGE_CHUNK_WIDTH];
    payload2_bytes.copy_from_slice(&frame[32..40]);
    Ok(DecodedFrame {
        kind,
        payload0: read_u64(frame, 16),
        payload1: read_u64(frame, 24),
        payload2: read_u64(frame, 32),
        payload2_bytes,
    })
}

fn build_header_v1(persistent_id: PersistentLogId) -> [u8; HEADER_LENGTH] {
    build_header(LogFormat::V1, persistent_id, 0)
}

fn build_header_v2(persistent_id: PersistentLogId, page_width: u64) -> [u8; HEADER_LENGTH] {
    build_header(LogFormat::V2, persistent_id, page_width)
}

fn build_header_v3(persistent_id: PersistentLogId, page_width: u64) -> [u8; HEADER_LENGTH] {
    build_header(LogFormat::V3, persistent_id, page_width)
}

fn build_header_v4(
    persistent_id: PersistentLogId,
    page_width: u64,
    generation: u64,
    retained_first: Option<u64>,
    logical_high_water: Option<u64>,
    allocated_epoch_high_water: NonZeroU64,
    selected_checkpoint_anchor: (u16, u128),
) -> [u8; HEADER_V4_LENGTH] {
    let mut header = [0_u8; HEADER_V4_LENGTH];
    header[..8].copy_from_slice(&HEADER_MAGIC);
    write_u16(&mut header, 8, FORMAT_VERSION_V4);
    write_u16(&mut header, 10, HEADER_V4_LENGTH_U16);
    write_u32(&mut header, 12, 0);
    write_u128(&mut header, 16, persistent_id.get());
    write_u64(&mut header, HEADER_V2_PAGE_WIDTH_OFFSET, page_width);
    write_u64(&mut header, HEADER_V4_GENERATION_OFFSET, generation);
    if let Some(position) = retained_first {
        write_u64(&mut header, HEADER_V4_RETAINED_FIRST_OFFSET, position);
        header[HEADER_V4_RETAINED_FIRST_PRESENCE_OFFSET] = 1;
    }
    if let Some(position) = logical_high_water {
        write_u64(&mut header, HEADER_V4_LOGICAL_HIGH_WATER_OFFSET, position);
        header[HEADER_V4_LOGICAL_HIGH_WATER_PRESENCE_OFFSET] = 1;
    }
    write_u64(
        &mut header,
        HEADER_V4_ALLOCATED_EPOCH_HIGH_WATER_OFFSET,
        allocated_epoch_high_water.get(),
    );
    write_u16(
        &mut header,
        HEADER_V4_ANCHOR_VERSION_OFFSET,
        selected_checkpoint_anchor.0,
    );
    write_u128(
        &mut header,
        HEADER_V4_ANCHOR_VALUE_OFFSET,
        selected_checkpoint_anchor.1,
    );
    let checksum = checksum_v1(&header[..HEADER_V4_CHECKSUM_OFFSET]);
    write_u64(&mut header, HEADER_V4_CHECKSUM_OFFSET, checksum);
    header
}

fn build_header_v5_initial(
    persistent_id: PersistentLogId,
    page_width: u64,
    database_file_identity: DatabaseFileHeaderIdentity,
) -> [u8; HEADER_V5_LENGTH] {
    let mut header = [0_u8; HEADER_V5_LENGTH];
    header[..8].copy_from_slice(&HEADER_MAGIC);
    write_u16(&mut header, 8, FORMAT_VERSION_V5);
    write_u16(&mut header, 10, HEADER_V5_LENGTH_U16);
    write_u128(&mut header, 16, persistent_id.get());
    write_u64(&mut header, HEADER_V2_PAGE_WIDTH_OFFSET, page_width);
    let identity =
        database_child_identity_codec::encode_database_child_identity(database_file_identity);
    header[HEADER_V5_IDENTITY_OFFSET..HEADER_V5_IDENTITY_OFFSET + identity.len()]
        .copy_from_slice(&identity);
    let checksum = checksum_v1(&header[..HEADER_V5_CHECKSUM_OFFSET]);
    write_u64(&mut header, HEADER_V5_CHECKSUM_OFFSET, checksum);
    header
}

fn build_header_v5_reclaimed(
    metadata: V4HeaderMetadata,
    page_width: u64,
    database_file_identity: DatabaseFileHeaderIdentity,
) -> [u8; HEADER_V5_LENGTH] {
    let v4 = build_header_v4(
        metadata.persistent_id,
        page_width,
        metadata.generation,
        metadata.retained_first,
        metadata.logical_high_water,
        metadata.allocated_epoch_high_water,
        metadata.selected_checkpoint_anchor,
    );
    let mut header = [0_u8; HEADER_V5_LENGTH];
    header[..HEADER_V4_CHECKSUM_OFFSET].copy_from_slice(&v4[..HEADER_V4_CHECKSUM_OFFSET]);
    write_u16(&mut header, 8, FORMAT_VERSION_V5);
    write_u16(&mut header, 10, HEADER_V5_LENGTH_U16);
    header[HEADER_V5_RECLAMATION_PRESENCE_OFFSET] = 1;
    let identity =
        database_child_identity_codec::encode_database_child_identity(database_file_identity);
    header[HEADER_V5_IDENTITY_OFFSET..HEADER_V5_IDENTITY_OFFSET + identity.len()]
        .copy_from_slice(&identity);
    let checksum = checksum_v1(&header[..HEADER_V5_CHECKSUM_OFFSET]);
    write_u64(&mut header, HEADER_V5_CHECKSUM_OFFSET, checksum);
    header
}

enum WalReclamationHeader {
    V4([u8; HEADER_V4_LENGTH]),
    V5([u8; HEADER_V5_LENGTH]),
}

impl WalReclamationHeader {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::V4(header) => header,
            Self::V5(header) => header,
        }
    }
}

fn build_header(
    format: LogFormat,
    persistent_id: PersistentLogId,
    page_width: u64,
) -> [u8; HEADER_LENGTH] {
    let mut header = [0_u8; HEADER_LENGTH];
    header[..8].copy_from_slice(&HEADER_MAGIC);
    write_u16(&mut header, 8, format.version());
    write_u16(&mut header, 10, HEADER_LENGTH_U16);
    write_u32(&mut header, 12, 0);
    write_u128(&mut header, 16, persistent_id.get());
    if format.supports_pages() {
        write_u64(&mut header, HEADER_V2_PAGE_WIDTH_OFFSET, page_width);
    }
    let checksum = checksum_v1(&header[..HEADER_CHECKSUM_OFFSET]);
    write_u64(&mut header, HEADER_CHECKSUM_OFFSET, checksum);
    header
}

fn build_frame(
    format: LogFormat,
    kind: FrameKind,
    payload0: u64,
    payload1: u64,
    payload2: u64,
) -> [u8; FRAME_LENGTH] {
    build_frame_with_payload2_bytes(format, kind, payload0, payload1, payload2.to_be_bytes())
}

fn build_frame_with_payload2_bytes(
    format: LogFormat,
    kind: FrameKind,
    payload0: u64,
    payload1: u64,
    payload2_bytes: [u8; PAGE_CHUNK_WIDTH],
) -> [u8; FRAME_LENGTH] {
    let mut frame = [0_u8; FRAME_LENGTH];
    frame[..4].copy_from_slice(&FRAME_MAGIC);
    write_u16(&mut frame, 4, kind.code());
    write_u16(&mut frame, 6, format.frame_version());
    write_u32(&mut frame, 8, 0);
    write_u16(&mut frame, 12, FRAME_LENGTH_U16);
    write_u16(&mut frame, 14, 0);
    write_u64(&mut frame, 16, payload0);
    write_u64(&mut frame, 24, payload1);
    frame[32..40].copy_from_slice(&payload2_bytes);
    let checksum = checksum_v1(&frame[..FRAME_CHECKSUM_OFFSET]);
    write_u64(&mut frame, FRAME_CHECKSUM_OFFSET, checksum);
    frame
}

fn build_reclamation_frame_plan<const N: usize>(
    records: &[FileLogRecord<N>],
    logical_high_water: Option<u64>,
    layout: PageLayout,
    format: LogFormat,
) -> Result<Vec<[u8; FRAME_LENGTH]>, FileTransactionRestartWalReclamationError> {
    let mut frame_count = usize::from(!records.is_empty());
    for record in records {
        let record_frame_count = match record.kind() {
            FileLogRecordKind::TransactionCommit { .. } => Some(1),
            FileLogRecordKind::PageWrite(_) => 1_usize.checked_add(layout.chunk_count),
            FileLogRecordKind::TransactionPageWrite(_) => 2_usize.checked_add(layout.chunk_count),
        }
        .ok_or(
            FileTransactionRestartWalReclamationError::FrameCapacityExhausted {
                record_count: records.len(),
            },
        )?;
        frame_count = frame_count.checked_add(record_frame_count).ok_or(
            FileTransactionRestartWalReclamationError::FrameCapacityExhausted {
                record_count: records.len(),
            },
        )?;
    }
    let mut frames = Vec::new();
    frames.try_reserve_exact(frame_count).map_err(|_| {
        FileTransactionRestartWalReclamationError::FrameCapacityExhausted {
            record_count: records.len(),
        }
    })?;
    for record in records {
        let position = record.position().get();
        match record.kind() {
            FileLogRecordKind::TransactionCommit {
                transaction_epoch,
                transaction_sequence,
            } => frames.push(build_frame(
                format,
                FrameKind::CommitRecord,
                position,
                *transaction_epoch,
                *transaction_sequence,
            )),
            FileLogRecordKind::PageWrite(page) => {
                append_reclamation_page_frames(&mut frames, position, page, None, layout, format)?;
            }
            FileLogRecordKind::TransactionPageWrite(transaction_page) => {
                append_reclamation_page_frames(
                    &mut frames,
                    position,
                    transaction_page.page_write(),
                    Some(StoredTransactionIdentity::from_epoch_sequence(
                        transaction_page.transaction_epoch(),
                        transaction_page.transaction_sequence(),
                    )),
                    layout,
                    format,
                )?;
            }
        }
    }
    if !records.is_empty() {
        let high_water =
            logical_high_water.ok_or(FileTransactionRestartWalReclamationError::PermitMismatch)?;
        frames.push(build_frame(
            format,
            FrameKind::DurableThrough,
            high_water,
            0,
            0,
        ));
    }
    Ok(frames)
}

fn append_reclamation_page_frames<const N: usize>(
    frames: &mut Vec<[u8; FRAME_LENGTH]>,
    position: u64,
    page: &FilePageWriteRecord<N>,
    owner: Option<StoredTransactionIdentity>,
    layout: PageLayout,
    format: LogFormat,
) -> Result<(), FileTransactionRestartWalReclamationError> {
    let header_kind = if owner.is_some() {
        FrameKind::TransactionPageHeader
    } else {
        FrameKind::PageHeader
    };
    frames.push(build_frame(
        format,
        header_kind,
        position,
        page.page_number().get(),
        page.page_version().get(),
    ));
    if let Some(owner) = owner {
        frames.push(build_frame(
            format,
            FrameKind::TransactionPageOwner,
            position,
            owner.epoch,
            owner.sequence,
        ));
    }
    for chunk_index in 0..layout.chunk_count {
        let mut chunk = [0_u8; PAGE_CHUNK_WIDTH];
        let start = chunk_index * PAGE_CHUNK_WIDTH;
        let logical_len = layout.logical_bytes_for_chunk(chunk_index);
        let end = start + logical_len;
        chunk[..logical_len].copy_from_slice(&page.bytes()[start..end]);
        frames.push(build_frame_with_payload2_bytes(
            format,
            FrameKind::PageData,
            position,
            u64::try_from(chunk_index).map_err(|_| {
                FileTransactionRestartWalReclamationError::FrameCapacityExhausted {
                    record_count: frames.len(),
                }
            })?,
            chunk,
        ));
    }
    Ok(())
}

fn checksum_v1(bytes: &[u8]) -> u64 {
    let mut state = CHECKSUM_SEED;
    let mut protected_len = 0_u64;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(CHECKSUM_MULTIPLIER);
        state = state.rotate_left(7) ^ CHECKSUM_XOR;
        protected_len = protected_len.wrapping_add(1);
    }
    state ^ protected_len
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    let mut buffer = [0_u8; 2];
    buffer.copy_from_slice(&bytes[offset..offset + 2]);
    u16::from_be_bytes(buffer)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut buffer = [0_u8; 4];
    buffer.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_be_bytes(buffer)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut buffer = [0_u8; 8];
    buffer.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_be_bytes(buffer)
}

fn read_u128(bytes: &[u8], offset: usize) -> u128 {
    let mut buffer = [0_u8; 16];
    buffer.copy_from_slice(&bytes[offset..offset + 16]);
    u128::from_be_bytes(buffer)
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn write_u128(bytes: &mut [u8], offset: usize, value: u128) {
    bytes[offset..offset + 16].copy_from_slice(&value.to_be_bytes());
}

// ---------------------------------------------------------------------------
// FilePageStore – append-only page-image store backed by a separate file.
//
// ## Page-store format
//
// The file begins with one immutable 64-byte header followed by zero or more
// groups of fixed-size 56-byte frames. Every multibyte field is big-endian.
// The checksum algorithm and frame geometry are shared with the WAL format.
//
// Header bytes:
//   0..8   – magic `NTSQPGS1`
//   8..10  – u16 version (1)
//   10..12 – u16 header length (64)
//   12..16 – u32 flags (0)
//   16..32 – nonzero u128 persistent lineage ID
//   32..40 – nonzero page width N (big-endian u64)
//   40..56 – reserved zeros
//   56..64 – checksum of bytes 0..56
//
// Each group is: snapshot-header frame, required-position frame, then
// exactly ceil(N/8) page-data frames. The store sequence is contiguous
// from 1 and each rewrite appends a new group; the latest complete group
// per PageNumber is the inspectable current value.
//
// Open recovery repairs only a final incomplete physical frame or final
// incomplete logical group (truncating to the snapshot-header offset).
// Any complete malformed group fails without truncation.
// ---------------------------------------------------------------------------

const PAGE_STORE_HEADER_MAGIC: [u8; 8] = *b"NTSQPGS1";
const PAGE_STORE_FRAME_MAGIC: [u8; 4] = *b"NTSP";
const PAGE_STORE_FORMAT_VERSION: u16 = 1;
const PAGE_STORE_FORMAT_VERSION_V2: u16 = 2;
const PAGE_STORE_HEADER_V2_LENGTH: usize = 128;
const PAGE_STORE_HEADER_V2_LENGTH_U16: u16 = 128;
const PAGE_STORE_HEADER_V2_LENGTH_U64: u64 = 128;
const PAGE_STORE_HEADER_V2_IDENTITY_OFFSET: usize = 64;
const PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET: usize = 120;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageStoreFormat {
    V1,
    V2,
}

impl PageStoreFormat {
    const fn version(self) -> u16 {
        match self {
            Self::V1 => PAGE_STORE_FORMAT_VERSION,
            Self::V2 => PAGE_STORE_FORMAT_VERSION_V2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
enum PageStoreFrameKind {
    SnapshotHeader = 1,
    RequiredPosition = 2,
    PageData = 3,
}

impl PageStoreFrameKind {
    fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::SnapshotHeader),
            2 => Some(Self::RequiredPosition),
            3 => Some(Self::PageData),
            _ => None,
        }
    }

    const fn code(self) -> u16 {
        match self {
            Self::SnapshotHeader => 1,
            Self::RequiredPosition => 2,
            Self::PageData => 3,
        }
    }
}

/// One-shot physical-effect boundary for the next page-store write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageStoreFaultPoint {
    /// Fail before file/state mutation.
    BeforeWrite,
    /// Fire only after the entire group is written, sync succeeds, and state
    /// is updated.
    AfterWrite,
}

impl fmt::Display for PageStoreFaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeWrite => formatter.write_str("before write"),
            Self::AfterWrite => formatter.write_str("after write"),
        }
    }
}

/// Refusal to silently replace an already armed page-store fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageStoreFaultAlreadyArmed {
    armed: PageStoreFaultPoint,
    requested: PageStoreFaultPoint,
}

impl PageStoreFaultAlreadyArmed {
    /// Returns the fault that remains armed.
    #[must_use]
    pub const fn armed(&self) -> PageStoreFaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> PageStoreFaultPoint {
        self.requested
    }
}

impl fmt::Display for PageStoreFaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "page-store fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for PageStoreFaultAlreadyArmed {}

/// I/O stage for page-store operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageStoreIoStage {
    CreateFile,
    OpenFile,
    AcquireExclusiveLock,
    OpenParentDirectory,
    ReadMetadata,
    ReadHeader,
    ReadFrame,
    WriteHeader,
    SyncCreatedFile,
    SyncOpenedFile,
    SyncParentDirectory,
    TruncateIncompleteTail,
    SyncTruncatedTail,
    SeekEnd,
    WriteSnapshotHeaderFrame,
    WriteRequiredPositionFrame,
    WritePageDataFrame,
    SyncPageGroup,
}

impl fmt::Display for PageStoreIoStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateFile => formatter.write_str("creating the page-store file"),
            Self::OpenFile => formatter.write_str("opening the page-store file"),
            Self::AcquireExclusiveLock => {
                formatter.write_str("acquiring the exclusive page-store file lock")
            }
            Self::OpenParentDirectory => formatter.write_str("opening the parent directory"),
            Self::ReadMetadata => formatter.write_str("reading page-store metadata"),
            Self::ReadHeader => formatter.write_str("reading the page-store header"),
            Self::ReadFrame => formatter.write_str("reading a page-store frame"),
            Self::WriteHeader => formatter.write_str("writing the page-store header"),
            Self::SyncCreatedFile => {
                formatter.write_str("synchronizing the created page-store file")
            }
            Self::SyncOpenedFile => formatter.write_str("synchronizing the opened page-store file"),
            Self::SyncParentDirectory => formatter.write_str("synchronizing the parent directory"),
            Self::TruncateIncompleteTail => {
                formatter.write_str("truncating an incomplete page-store tail")
            }
            Self::SyncTruncatedTail => {
                formatter.write_str("synchronizing a repaired page-store tail")
            }
            Self::SeekEnd => formatter.write_str("seeking to the end of the page-store file"),
            Self::WriteSnapshotHeaderFrame => {
                formatter.write_str("writing a page-store snapshot-header frame")
            }
            Self::WriteRequiredPositionFrame => {
                formatter.write_str("writing a page-store required-position frame")
            }
            Self::WritePageDataFrame => formatter.write_str("writing a page-store page-data frame"),
            Self::SyncPageGroup => formatter.write_str("synchronizing a page-store group"),
        }
    }
}

/// I/O failure paired with the exact page-store stage.
#[derive(Debug)]
pub struct PageStoreIoError {
    stage: PageStoreIoStage,
    source: io::Error,
}

impl PageStoreIoError {
    fn new(stage: PageStoreIoStage, source: io::Error) -> Self {
        Self { stage, source }
    }

    /// Returns the adapter stage that reported the I/O error.
    #[must_use]
    pub const fn stage(&self) -> PageStoreIoStage {
        self.stage
    }

    /// Returns the original `std::io::Error`.
    #[must_use]
    pub const fn io_source(&self) -> &io::Error {
        &self.source
    }
}

impl PartialEq for PageStoreIoError {
    fn eq(&self, other: &Self) -> bool {
        self.stage == other.stage
            && self.source.kind() == other.source.kind()
            && self.source.raw_os_error() == other.source.raw_os_error()
    }
}

impl Eq for PageStoreIoError {}

impl fmt::Display for PageStoreIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} failed: {}", self.stage, self.source)
    }
}

impl Error for PageStoreIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Exact malformed-format reason for the page store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageStoreFormatErrorReason {
    HeaderTooShort { actual: u64 },
    HeaderMagic,
    HeaderVersion { actual: u16 },
    HeaderLength { actual: u16 },
    HeaderV2Length { actual: u16 },
    HeaderFlags { actual: u32 },
    HeaderPageWidthZero,
    HeaderPageWidthMismatch { expected: u64, actual: u64 },
    HeaderReserved,
    HeaderDatabaseChildIdentity(DatabaseChildIdentityDecodeErrorReason),
    HeaderChecksum { expected: u64, actual: u64 },
    LineageIdZero,
    FrameMagic,
    FrameKind { actual: u16 },
    FrameVersion { actual: u16 },
    FrameLength { actual: u16 },
    FrameFlags { actual: u32 },
    FrameReserved,
    FrameChecksum { expected: u64, actual: u64 },
    SnapshotSequenceZero,
    SnapshotSequenceOutOfOrder { expected: u64, actual: u64 },
    SnapshotSequenceSpaceExhausted,
    SnapshotPageNumberZero,
    RequiredPositionZero,
    RequiredPositionSequenceMismatch { expected: u64, actual: u64 },
    RequiredPositionPayloadCNonzero { actual: u64 },
    PageDataSequenceMismatch { expected: u64, actual: u64 },
    PageDataChunkIndexOutOfSequence { expected: u64, actual: u64 },
    PageDataFinalPaddingNonzero,
    PageDataWithoutHeader,
    UnexpectedKindAfterHeader { actual: u16 },
    UnexpectedKindAfterRequiredPosition { actual: u16 },
}

impl fmt::Display for PageStoreFormatErrorReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTooShort { actual } => {
                write!(
                    formatter,
                    "page-store header is shorter than 64 bytes: found {actual}"
                )
            }
            Self::HeaderMagic => formatter.write_str("page-store header magic does not match"),
            Self::HeaderVersion { actual } => {
                write!(formatter, "unsupported page-store header version {actual}")
            }
            Self::HeaderLength { actual } => {
                write!(
                    formatter,
                    "page-store header length {actual} does not equal 64"
                )
            }
            Self::HeaderV2Length { actual } => {
                write!(
                    formatter,
                    "page-store V2 header length {actual} does not equal {PAGE_STORE_HEADER_V2_LENGTH}"
                )
            }
            Self::HeaderFlags { actual } => {
                write!(formatter, "page-store header flags are nonzero: {actual}")
            }
            Self::HeaderPageWidthZero => {
                formatter.write_str("page-store header page width is zero")
            }
            Self::HeaderPageWidthMismatch { expected, actual } => write!(
                formatter,
                "page-store header page width {actual} does not equal required width {expected}"
            ),
            Self::HeaderReserved => {
                formatter.write_str("page-store header reserved bytes are nonzero")
            }
            Self::HeaderDatabaseChildIdentity(source) => source.fmt(formatter),
            Self::HeaderChecksum { expected, actual } => write!(
                formatter,
                "page-store header checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
            Self::LineageIdZero => formatter.write_str("page-store persistent lineage ID is zero"),
            Self::FrameMagic => formatter.write_str("page-store frame magic does not match"),
            Self::FrameKind { actual } => {
                write!(formatter, "unknown page-store frame kind {actual}")
            }
            Self::FrameVersion { actual } => {
                write!(formatter, "unsupported page-store frame version {actual}")
            }
            Self::FrameLength { actual } => {
                write!(
                    formatter,
                    "page-store frame length {actual} does not equal 56"
                )
            }
            Self::FrameFlags { actual } => {
                write!(formatter, "page-store frame flags are nonzero: {actual}")
            }
            Self::FrameReserved => {
                formatter.write_str("page-store frame reserved bytes are nonzero")
            }
            Self::FrameChecksum { expected, actual } => write!(
                formatter,
                "page-store frame checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
            Self::SnapshotSequenceZero => {
                formatter.write_str("page-store snapshot sequence is zero")
            }
            Self::SnapshotSequenceOutOfOrder { expected, actual } => write!(
                formatter,
                "page-store snapshot sequence {actual} does not equal the next contiguous sequence {expected}"
            ),
            Self::SnapshotSequenceSpaceExhausted => {
                formatter.write_str("page-store snapshot sequence space is exhausted")
            }
            Self::SnapshotPageNumberZero => {
                formatter.write_str("page-store snapshot page number is zero")
            }
            Self::RequiredPositionZero => {
                formatter.write_str("page-store required WAL position is zero")
            }
            Self::RequiredPositionSequenceMismatch { expected, actual } => write!(
                formatter,
                "page-store required-position sequence {actual} does not match pending snapshot sequence {expected}"
            ),
            Self::RequiredPositionPayloadCNonzero { actual } => write!(
                formatter,
                "page-store required-position payload C is nonzero: {actual}"
            ),
            Self::PageDataSequenceMismatch { expected, actual } => write!(
                formatter,
                "page-store page-data sequence {actual} does not match pending snapshot sequence {expected}"
            ),
            Self::PageDataChunkIndexOutOfSequence { expected, actual } => write!(
                formatter,
                "page-store page-data chunk index {actual} does not equal required contiguous chunk {expected}"
            ),
            Self::PageDataFinalPaddingNonzero => {
                formatter.write_str("page-store page-data final-chunk padding bytes are nonzero")
            }
            Self::PageDataWithoutHeader => {
                formatter.write_str("page-store page-data frame has no pending snapshot header")
            }
            Self::UnexpectedKindAfterHeader { actual } => write!(
                formatter,
                "page-store expected a required-position frame after snapshot header but found kind {actual}"
            ),
            Self::UnexpectedKindAfterRequiredPosition { actual } => write!(
                formatter,
                "page-store expected a page-data frame after required position but found kind {actual}"
            ),
        }
    }
}

/// Malformed-format error for the page store, paired with byte offset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageStoreFormatError {
    offset: u64,
    reason: PageStoreFormatErrorReason,
}

impl PageStoreFormatError {
    fn new(offset: u64, reason: PageStoreFormatErrorReason) -> Self {
        Self { offset, reason }
    }

    /// Returns the byte offset that reported the format error.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the exact malformed-format reason.
    #[must_use]
    pub const fn reason(&self) -> &PageStoreFormatErrorReason {
        &self.reason
    }
}

impl fmt::Display for PageStoreFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "page-store format error at byte {}: {}",
            self.offset, self.reason
        )
    }
}

impl Error for PageStoreFormatError {}

/// Failure while creating a new page store file.
#[derive(Debug, Eq, PartialEq)]
pub enum PageStoreCreateError {
    MissingParentDirectory,
    PageWidth(FilePageWidthError),
    Io(PageStoreIoError),
}

impl fmt::Display for PageStoreCreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParentDirectory => {
                formatter.write_str("page-store path does not have an existing parent directory")
            }
            Self::PageWidth(source) => source.fmt(formatter),
            Self::Io(source) => source.fmt(formatter),
        }
    }
}

impl Error for PageStoreCreateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingParentDirectory => None,
            Self::PageWidth(source) => Some(source),
            Self::Io(source) => Some(source),
        }
    }
}

/// Failure while opening an existing page store file.
#[derive(Debug, Eq, PartialEq)]
pub enum PageStoreOpenError {
    PageWidth(FilePageWidthError),
    Io(PageStoreIoError),
    Format(PageStoreFormatError),
    SnapshotCapacityExhausted,
}

impl fmt::Display for PageStoreOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PageWidth(source) => source.fmt(formatter),
            Self::Io(source) => source.fmt(formatter),
            Self::Format(source) => source.fmt(formatter),
            Self::SnapshotCapacityExhausted => {
                formatter.write_str("page-store snapshot capacity is exhausted")
            }
        }
    }
}

impl Error for PageStoreOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PageWidth(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::Format(source) => Some(source),
            Self::SnapshotCapacityExhausted => None,
        }
    }
}

/// Stage-specific failure while opening an owned WAL/page-store recovery pair.
#[derive(Debug, Eq, PartialEq)]
pub enum FileTransactionPageStorageOpenError {
    /// The transaction-page-capable WAL could not be opened first.
    CommitLog(FileOpenError),
    /// The page store could not be opened after the WAL lock was acquired.
    PageStore(PageStoreOpenError),
}

impl fmt::Display for FileTransactionPageStorageOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommitLog(source) => {
                write!(formatter, "transaction-page WAL open failed: {source}")
            }
            Self::PageStore(source) => write!(formatter, "page-store open failed: {source}"),
        }
    }
}

impl Error for FileTransactionPageStorageOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CommitLog(source) => Some(source),
            Self::PageStore(source) => Some(source),
        }
    }
}

/// Failure while writing to the page store.
#[derive(Debug)]
pub enum FilePageStoreError {
    InjectedFault(PageStoreFaultPoint),
    PageWidth(FilePageWidthError),
    PoisonedWriter,
    ForeignPageLineage(PageNumber),
    ForeignPermitLineage(LogSequenceNumber),
    PermitPositionMismatch {
        expected: LogSequenceNumber,
        actual: LogSequenceNumber,
    },
    RequiredPositionZero(PageNumber),
    SnapshotCapacityExhausted,
    StoreSequenceSpaceExhausted,
    Io(PageStoreIoError),
}

impl fmt::Display for FilePageStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InjectedFault(point) => {
                write!(formatter, "injected page-store failure {point}")
            }
            Self::PageWidth(source) => source.fmt(formatter),
            Self::PoisonedWriter => formatter
                .write_str("page-store writer is poisoned; reopen the file before retrying"),
            Self::ForeignPageLineage(page_number) => write!(
                formatter,
                "page-store page {} belongs to another lineage",
                page_number.get()
            ),
            Self::ForeignPermitLineage(position) => write!(
                formatter,
                "page-store permit position {} belongs to another lineage",
                position.get()
            ),
            Self::PermitPositionMismatch { expected, actual } => write!(
                formatter,
                "page-store permit position {} does not equal required position {}",
                actual.get(),
                expected.get()
            ),
            Self::RequiredPositionZero(page_number) => write!(
                formatter,
                "page-store page {} has zero as its required WAL position",
                page_number.get()
            ),
            Self::SnapshotCapacityExhausted => {
                formatter.write_str("page-store snapshot capacity is exhausted")
            }
            Self::StoreSequenceSpaceExhausted => {
                formatter.write_str("page-store sequence space is exhausted")
            }
            Self::Io(source) => source.fmt(formatter),
        }
    }
}

impl PartialEq for FilePageStoreError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InjectedFault(a), Self::InjectedFault(b)) => a == b,
            (Self::PageWidth(a), Self::PageWidth(b)) => a == b,
            (Self::PoisonedWriter, Self::PoisonedWriter) => true,
            (Self::ForeignPageLineage(a), Self::ForeignPageLineage(b)) => a == b,
            (Self::ForeignPermitLineage(a), Self::ForeignPermitLineage(b)) => a == b,
            (
                Self::PermitPositionMismatch {
                    expected: ea,
                    actual: aa,
                },
                Self::PermitPositionMismatch {
                    expected: eb,
                    actual: ab,
                },
            ) => ea == eb && aa == ab,
            (Self::RequiredPositionZero(a), Self::RequiredPositionZero(b)) => a == b,
            (Self::SnapshotCapacityExhausted, Self::SnapshotCapacityExhausted) => true,
            (Self::StoreSequenceSpaceExhausted, Self::StoreSequenceSpaceExhausted) => true,
            (Self::Io(a), Self::Io(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for FilePageStoreError {}

impl Error for FilePageStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PageWidth(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::InjectedFault(_)
            | Self::PoisonedWriter
            | Self::ForeignPageLineage(_)
            | Self::ForeignPermitLineage(_)
            | Self::PermitPositionMismatch { .. }
            | Self::RequiredPositionZero(_)
            | Self::SnapshotCapacityExhausted
            | Self::StoreSequenceSpaceExhausted => None,
        }
    }
}

/// Failure to observe an authoritative filesystem page-store snapshot.
#[derive(Debug, Eq, PartialEq)]
pub enum FileCommittedPageRecoveryObservationError<const N: usize> {
    /// An uncertain prior page-store write requires reopen before observation.
    PoisonedWriter,
    /// The current stored bytes could not become adapter-neutral evidence.
    Projection(Box<PageRecoveryObservationBytesError<N>>),
}

impl<const N: usize> fmt::Display for FileCommittedPageRecoveryObservationError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoisonedWriter => formatter.write_str(
                "page-store writer is poisoned; reopen the file before committed-page recovery",
            ),
            Self::Projection(source) => {
                write!(formatter, "filesystem page observation failed: {source}")
            }
        }
    }
}

impl<const N: usize> Error for FileCommittedPageRecoveryObservationError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection(source) => Some(source.as_ref()),
            Self::PoisonedWriter => None,
        }
    }
}

/// Failure to project every current filesystem page-store snapshot.
#[derive(Debug, Eq, PartialEq)]
pub enum FilePageStoreInventoryError<const N: usize> {
    /// An uncertain prior write requires reopen before complete inventory.
    PoisonedWriter,
    /// The complete owned inventory could not reserve its exact page bound.
    CapacityExhausted {
        /// Number of current snapshots requiring projection.
        page_count: usize,
    },
    /// One current snapshot could not become adapter-neutral evidence.
    Projection {
        /// Page whose projection failed.
        page_number: PageNumber,
        /// Exact projection cause.
        source: Box<PageRecoveryObservationBytesError<N>>,
    },
}

impl<const N: usize> fmt::Display for FilePageStoreInventoryError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoisonedWriter => formatter.write_str(
                "page-store writer is poisoned; reopen the file before complete inventory",
            ),
            Self::CapacityExhausted { page_count } => write!(
                formatter,
                "filesystem page inventory capacity is exhausted for {page_count} pages"
            ),
            Self::Projection {
                page_number,
                source,
            } => write!(
                formatter,
                "filesystem page {} inventory projection failed: {source}",
                page_number.get()
            ),
        }
    }
}

impl<const N: usize> Error for FilePageStoreInventoryError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Projection { source, .. } => Some(source.as_ref()),
            Self::PoisonedWriter | Self::CapacityExhausted { .. } => None,
        }
    }
}

/// Failure during the filesystem page-store's atomic recovery replacement.
#[derive(Debug, Eq, PartialEq)]
pub enum FileCommittedPageRecoveryStoreError<const N: usize> {
    /// The candidate target page position belongs to another lineage.
    ForeignTargetPagePosition(LogSequenceNumber),
    /// The candidate target commit position belongs to another lineage.
    ForeignTargetCommitPosition(LogSequenceNumber),
    /// The recovery permit page position belongs to another lineage.
    ForeignPermitPagePosition(LogSequenceNumber),
    /// The recovery permit commit position belongs to another lineage.
    ForeignPermitCommitPosition(LogSequenceNumber),
    /// The permit page position differs from the candidate target.
    PermitPagePositionMismatch {
        /// Candidate target page position.
        expected: LogSequenceNumber,
        /// Supplied permit page position.
        actual: LogSequenceNumber,
    },
    /// The permit commit position differs from the candidate target.
    PermitCommitPositionMismatch {
        /// Candidate target commit position.
        expected: LogSequenceNumber,
        /// Supplied permit commit position.
        actual: LogSequenceNumber,
    },
    /// Current store state could not be projected during the locked recheck.
    CurrentObservation(Box<PageRecoveryObservationBytesError<N>>),
    /// Current store state contradicted the candidate.
    SourceComparison(Box<DurableCommittedTransactionPageRecoveryComparisonError>),
    /// Current store state was valid but no longer matched the candidate source.
    SourceNotMatched {
        /// Non-source comparison observed under the store's lifetime lock.
        actual: DurableCommittedTransactionPageRecoveryComparison,
    },
    /// Shared physical page-store writing failed at an exact typed boundary.
    PageStore(FilePageStoreError),
}

impl<const N: usize> fmt::Display for FileCommittedPageRecoveryStoreError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignTargetPagePosition(position) => write!(
                formatter,
                "recovery target page position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignTargetCommitPosition(position) => write!(
                formatter,
                "recovery target commit position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignPermitPagePosition(position) => write!(
                formatter,
                "recovery permit page position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignPermitCommitPosition(position) => write!(
                formatter,
                "recovery permit commit position {} belongs to another lineage",
                position.get()
            ),
            Self::PermitPagePositionMismatch { expected, actual } => write!(
                formatter,
                "recovery permit page position {} does not match target position {}",
                actual.get(),
                expected.get()
            ),
            Self::PermitCommitPositionMismatch { expected, actual } => write!(
                formatter,
                "recovery permit commit position {} does not match target position {}",
                actual.get(),
                expected.get()
            ),
            Self::CurrentObservation(source) => {
                write!(
                    formatter,
                    "recovery current-page observation failed: {source}"
                )
            }
            Self::SourceComparison(source) => {
                write!(formatter, "recovery source comparison failed: {source}")
            }
            Self::SourceNotMatched { actual } => write!(
                formatter,
                "recovery source no longer matches the candidate: {actual:?}"
            ),
            Self::PageStore(source) => {
                write!(formatter, "recovery page-store write failed: {source}")
            }
        }
    }
}

impl<const N: usize> Error for FileCommittedPageRecoveryStoreError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentObservation(source) => Some(source.as_ref()),
            Self::SourceComparison(source) => Some(source.as_ref()),
            Self::PageStore(source) => Some(source),
            Self::ForeignTargetPagePosition(_)
            | Self::ForeignTargetCommitPosition(_)
            | Self::ForeignPermitPagePosition(_)
            | Self::ForeignPermitCommitPosition(_)
            | Self::PermitPagePositionMismatch { .. }
            | Self::PermitCommitPositionMismatch { .. }
            | Self::SourceNotMatched { .. } => None,
        }
    }
}

/// Failure during one filesystem atomic replay page replacement.
#[derive(Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointPageRepairStoreError<const N: usize> {
    /// The candidate target page position belongs to another lineage.
    ForeignTargetPagePosition(LogSequenceNumber),
    /// A committed candidate target position belongs to another lineage.
    ForeignTargetCommitPosition(LogSequenceNumber),
    /// The repair permit page position belongs to another lineage.
    ForeignPermitPagePosition(LogSequenceNumber),
    /// A repair permit commit position belongs to another lineage.
    ForeignPermitCommitPosition(LogSequenceNumber),
    /// The permit page position differs from the candidate target.
    PermitPagePositionMismatch {
        /// Candidate target page position.
        expected: LogSequenceNumber,
        /// Supplied permit page position.
        actual: LogSequenceNumber,
    },
    /// The permit commit shape or position differs from the candidate target.
    PermitCommitPositionMismatch {
        /// Candidate commit position, absent for a raw target.
        expected: Option<LogSequenceNumber>,
        /// Supplied permit commit position.
        actual: Option<LogSequenceNumber>,
    },
    /// Current store state could not be projected during the locked recheck.
    CurrentObservation(Box<PageRecoveryObservationBytesError<N>>),
    /// Current store state contradicted the candidate.
    SourceComparison(Box<DurableTransactionRestartCheckpointPageRepairComparisonError>),
    /// Current store state was valid but no longer matched the candidate source.
    SourceNotMatched {
        /// Non-source comparison observed under the lifetime store lock.
        actual: DurableTransactionRestartCheckpointPageRepairComparison,
    },
    /// Shared physical page-store writing failed at an exact typed boundary.
    PageStore(FilePageStoreError),
}

impl<const N: usize> fmt::Display for FileRestartCheckpointPageRepairStoreError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignTargetPagePosition(position) => write!(
                formatter,
                "replay-repair target page position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignTargetCommitPosition(position) => write!(
                formatter,
                "replay-repair target commit position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignPermitPagePosition(position) => write!(
                formatter,
                "replay-repair permit page position {} belongs to another lineage",
                position.get()
            ),
            Self::ForeignPermitCommitPosition(position) => write!(
                formatter,
                "replay-repair permit commit position {} belongs to another lineage",
                position.get()
            ),
            Self::PermitPagePositionMismatch { expected, actual } => write!(
                formatter,
                "replay-repair permit page position {} does not match target position {}",
                actual.get(),
                expected.get()
            ),
            Self::PermitCommitPositionMismatch { expected, actual } => write!(
                formatter,
                "replay-repair permit commit position {:?} does not match target position {:?}",
                actual.as_ref().map(LogSequenceNumber::get),
                expected.as_ref().map(LogSequenceNumber::get)
            ),
            Self::CurrentObservation(source) => {
                write!(
                    formatter,
                    "replay-repair current-page observation failed: {source}"
                )
            }
            Self::SourceComparison(source) => {
                write!(
                    formatter,
                    "replay-repair source comparison failed: {source}"
                )
            }
            Self::SourceNotMatched { actual } => write!(
                formatter,
                "replay-repair source no longer matches the candidate: {actual:?}"
            ),
            Self::PageStore(source) => {
                write!(formatter, "replay-repair page-store write failed: {source}")
            }
        }
    }
}

impl<const N: usize> Error for FileRestartCheckpointPageRepairStoreError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentObservation(source) => Some(source.as_ref()),
            Self::SourceComparison(source) => Some(source.as_ref()),
            Self::PageStore(source) => Some(source),
            Self::ForeignTargetPagePosition(_)
            | Self::ForeignTargetCommitPosition(_)
            | Self::ForeignPermitPagePosition(_)
            | Self::ForeignPermitCommitPosition(_)
            | Self::PermitPagePositionMismatch { .. }
            | Self::PermitCommitPositionMismatch { .. }
            | Self::SourceNotMatched { .. } => None,
        }
    }
}

/// Safely inspectable latest snapshot of one stored page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileStoredPage<const N: usize> {
    page_number: PageNumber,
    page_version: PageVersion,
    bytes: [u8; N],
    required_position: LogSequenceNumber,
    store_sequence: u64,
}

impl<const N: usize> FileStoredPage<N> {
    /// Returns the page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.page_number
    }

    /// Returns the page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.page_version
    }

    /// Returns the exact stored page bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Returns the exact required WAL position.
    #[must_use]
    pub const fn required_position(&self) -> &LogSequenceNumber {
        &self.required_position
    }

    /// Projects this durable snapshot into adapter-neutral recovery evidence.
    pub fn page_recovery_observation(
        &self,
    ) -> Result<StoredPageSnapshotObservation<N>, PageRecoveryObservationBytesError<N>> {
        StoredPageSnapshotObservation::from_bytes(
            self.page_number,
            self.page_version,
            self.bytes,
            self.required_position.clone(),
        )
    }

    /// Returns the store-internal sequence number.
    #[must_use]
    pub const fn store_sequence(&self) -> u64 {
        self.store_sequence
    }
}

/// Append-only page image store backed by a separate file.
#[derive(Debug)]
pub struct FilePageStore<const N: usize> {
    file: File,
    format: PageStoreFormat,
    lineage: LogLineage,
    persistent_id: PersistentLogId,
    database_file_identity: Option<DatabaseFileHeaderIdentity>,
    pages: Vec<FileStoredPage<N>>,
    next_sequence: Option<u64>,
    armed_fault: Option<PageStoreFaultPoint>,
    poisoned: bool,
}

pub(crate) struct LockedFilePageStoreOpen<const N: usize> {
    store: FilePageStore<N>,
    repaired_len: Option<u64>,
}

impl<const N: usize> LockedFilePageStoreOpen<N> {
    pub(crate) fn metadata(&self) -> io::Result<fs::Metadata> {
        self.store.file.metadata()
    }

    pub(crate) const fn persistent_id(&self) -> PersistentLogId {
        self.store.persistent_id
    }

    pub(crate) const fn physical_format_version(&self) -> u16 {
        self.store.format.version()
    }

    pub(crate) const fn database_file_identity(&self) -> Option<DatabaseFileHeaderIdentity> {
        self.store.database_file_identity
    }

    pub(crate) fn is_exact_initial_database_file(&self) -> bool {
        self.repaired_len.is_none()
            && self.store.format == PageStoreFormat::V2
            && self.store.pages.is_empty()
            && self.store.next_sequence == Some(1)
    }

    pub(crate) fn finish(mut self) -> Result<FilePageStore<N>, PageStoreOpenError> {
        if let Some(repaired_len) = self.repaired_len {
            self.store.file.set_len(repaired_len).map_err(|source| {
                PageStoreOpenError::Io(PageStoreIoError::new(
                    PageStoreIoStage::TruncateIncompleteTail,
                    source,
                ))
            })?;
            self.store.file.sync_all().map_err(|source| {
                PageStoreOpenError::Io(PageStoreIoError::new(
                    PageStoreIoStage::SyncTruncatedTail,
                    source,
                ))
            })?;
        }
        self.store.file.seek(SeekFrom::End(0)).map_err(|source| {
            PageStoreOpenError::Io(PageStoreIoError::new(PageStoreIoStage::SeekEnd, source))
        })?;
        Ok(self.store)
    }
}

impl<const N: usize> FilePageStore<N> {
    pub(crate) fn database_create_metadata(&self) -> io::Result<fs::Metadata> {
        self.file.metadata()
    }

    /// Creates a new empty page store file.
    pub fn create_new<P>(
        path: P,
        persistent_id: PersistentLogId,
    ) -> Result<Self, PageStoreCreateError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(PageStoreCreateError::PageWidth)?;
        Self::create_new_internal(
            path.as_ref(),
            persistent_id,
            PageStoreFormat::V1,
            &build_page_store_header(persistent_id, layout.width_u64),
            None,
        )
    }

    /// Creates a new empty V2 page store carrying stable database-file identity.
    pub fn create_new_database<P>(
        path: P,
        storage_identity: DatabaseStorageIdentity,
    ) -> Result<Self, PageStoreCreateError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(PageStoreCreateError::PageWidth)?;
        let database_file_identity =
            storage_identity.file_header_identity(ntsql_database::DatabaseFileRole::PageStore);
        let persistent_id = storage_identity.persistent_log_id();
        Self::create_new_internal(
            path.as_ref(),
            persistent_id,
            PageStoreFormat::V2,
            &build_page_store_header_v2(persistent_id, layout.width_u64, database_file_identity),
            Some(database_file_identity),
        )
    }

    fn create_new_internal(
        path: &Path,
        persistent_id: PersistentLogId,
        format: PageStoreFormat,
        header: &[u8],
        database_file_identity: Option<DatabaseFileHeaderIdentity>,
    ) -> Result<Self, PageStoreCreateError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| {
                PageStoreCreateError::Io(PageStoreIoError::new(
                    PageStoreIoStage::CreateFile,
                    source,
                ))
            })?;
        file.try_lock().map_err(|source| {
            PageStoreCreateError::Io(PageStoreIoError::new(
                PageStoreIoStage::AcquireExclusiveLock,
                source.into(),
            ))
        })?;

        file.write_all(header).map_err(|source| {
            PageStoreCreateError::Io(PageStoreIoError::new(PageStoreIoStage::WriteHeader, source))
        })?;
        file.sync_all().map_err(|source| {
            PageStoreCreateError::Io(PageStoreIoError::new(
                PageStoreIoStage::SyncCreatedFile,
                source,
            ))
        })?;
        sync_page_store_parent_directory(path)?;
        file.seek(SeekFrom::End(0)).map_err(|source| {
            PageStoreCreateError::Io(PageStoreIoError::new(PageStoreIoStage::SeekEnd, source))
        })?;

        Ok(Self {
            file,
            format,
            lineage: LogLineage::persistent(persistent_id),
            persistent_id,
            database_file_identity,
            pages: Vec::new(),
            next_sequence: Some(1),
            armed_fault: None,
            poisoned: false,
        })
    }

    /// Opens an existing page store file, validates, scans, and repairs tail.
    pub fn open<P>(path: P) -> Result<Self, PageStoreOpenError>
    where
        P: AsRef<Path>,
    {
        Self::inspect(path)?.finish()
    }

    pub(crate) fn inspect<P>(path: P) -> Result<LockedFilePageStoreOpen<N>, PageStoreOpenError>
    where
        P: AsRef<Path>,
    {
        let layout = PageLayout::for_const::<N>().map_err(PageStoreOpenError::PageWidth)?;
        let path = path.as_ref();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| {
                PageStoreOpenError::Io(PageStoreIoError::new(PageStoreIoStage::OpenFile, source))
            })?;
        file.try_lock().map_err(|source| {
            PageStoreOpenError::Io(PageStoreIoError::new(
                PageStoreIoStage::AcquireExclusiveLock,
                source.into(),
            ))
        })?;
        file.sync_all().map_err(|source| {
            PageStoreOpenError::Io(PageStoreIoError::new(
                PageStoreIoStage::SyncOpenedFile,
                source,
            ))
        })?;

        let file_len = file
            .metadata()
            .map_err(|source| {
                PageStoreOpenError::Io(PageStoreIoError::new(
                    PageStoreIoStage::ReadMetadata,
                    source,
                ))
            })?
            .len();
        if file_len < HEADER_LENGTH_U64 {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                0,
                PageStoreFormatErrorReason::HeaderTooShort { actual: file_len },
            )));
        }

        let mut header = [0_u8; HEADER_LENGTH];
        file.read_exact(&mut header).map_err(|source| {
            PageStoreOpenError::Io(PageStoreIoError::new(PageStoreIoStage::ReadHeader, source))
        })?;
        let header_version = read_u16(&header, 8);
        let (format, persistent_id, database_file_identity, header_length) =
            if header_version == PAGE_STORE_FORMAT_VERSION_V2 {
                if file_len < PAGE_STORE_HEADER_V2_LENGTH_U64 {
                    return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                        0,
                        PageStoreFormatErrorReason::HeaderTooShort { actual: file_len },
                    )));
                }
                let mut header_v2 = [0_u8; PAGE_STORE_HEADER_V2_LENGTH];
                header_v2[..HEADER_LENGTH].copy_from_slice(&header);
                file.read_exact(&mut header_v2[HEADER_LENGTH..])
                    .map_err(|source| {
                        PageStoreOpenError::Io(PageStoreIoError::new(
                            PageStoreIoStage::ReadHeader,
                            source,
                        ))
                    })?;
                let metadata = parse_page_store_header_v2(&header_v2, layout)
                    .map_err(PageStoreOpenError::Format)?;
                (
                    PageStoreFormat::V2,
                    metadata.persistent_id,
                    Some(metadata.database_file_identity),
                    PAGE_STORE_HEADER_V2_LENGTH_U64,
                )
            } else {
                let persistent_id =
                    parse_page_store_header(&header, layout).map_err(PageStoreOpenError::Format)?;
                (PageStoreFormat::V1, persistent_id, None, HEADER_LENGTH_U64)
            };
        let lineage = LogLineage::persistent(persistent_id);

        let mut open_state = PageStoreOpenState::<N>::new(layout, lineage.clone());

        let frame_region_len = file_len - header_length;
        let complete_frame_count = frame_region_len / FRAME_LENGTH_U64;
        let incomplete_tail_len = frame_region_len % FRAME_LENGTH_U64;

        for frame_index in 0..complete_frame_count {
            let mut frame = [0_u8; FRAME_LENGTH];
            file.read_exact(&mut frame).map_err(|source| {
                PageStoreOpenError::Io(PageStoreIoError::new(PageStoreIoStage::ReadFrame, source))
            })?;
            let offset = header_length + frame_index * FRAME_LENGTH_U64;
            let decoded =
                parse_page_store_frame(&frame, offset).map_err(PageStoreOpenError::Format)?;
            open_state.apply_frame(decoded, offset)?;
        }

        let repaired_len = match open_state.pending_group_header_offset() {
            Some(offset) => Some(offset),
            None if incomplete_tail_len > 0 => {
                Some(header_length + complete_frame_count * FRAME_LENGTH_U64)
            }
            None => None,
        };

        Ok(LockedFilePageStoreOpen {
            store: Self {
                file,
                format,
                lineage,
                persistent_id,
                database_file_identity,
                pages: open_state.pages,
                next_sequence: open_state.next_sequence,
                armed_fault: None,
                poisoned: false,
            },
            repaired_len,
        })
    }

    /// Returns the stable persistent lineage ID.
    #[must_use]
    pub const fn persistent_id(&self) -> PersistentLogId {
        self.persistent_id
    }

    /// Returns the physically parsed page-store header format version.
    #[must_use]
    pub const fn physical_format_version(&self) -> u16 {
        self.format.version()
    }

    /// Returns the stable database-file identity physically carried by V2.
    #[must_use]
    pub const fn database_file_identity(&self) -> Option<DatabaseFileHeaderIdentity> {
        self.database_file_identity
    }

    /// Arms one fault without replacing an existing plan.
    pub fn arm_fault(
        &mut self,
        fault: PageStoreFaultPoint,
    ) -> Result<(), PageStoreFaultAlreadyArmed> {
        if let Some(armed) = self.armed_fault {
            return Err(PageStoreFaultAlreadyArmed {
                armed,
                requested: fault,
            });
        }
        self.armed_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot fault that has not yet reached its matching stage.
    #[must_use]
    pub const fn armed_fault(&self) -> Option<PageStoreFaultPoint> {
        self.armed_fault
    }

    /// Returns whether this writer is poisoned.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Returns the latest snapshots of all stored pages.
    #[must_use]
    pub fn pages(&self) -> &[FileStoredPage<N>] {
        &self.pages
    }

    /// Looks up the latest snapshot for a specific page number.
    #[must_use]
    pub fn page(&self, page_number: PageNumber) -> Option<&FileStoredPage<N>> {
        self.pages.iter().find(|p| p.page_number == page_number)
    }

    fn consume_fault(&mut self, point: PageStoreFaultPoint) -> bool {
        if self.armed_fault == Some(point) {
            self.armed_fault = None;
            true
        } else {
            false
        }
    }

    fn page_index(&self, page_number: PageNumber) -> Option<usize> {
        self.pages
            .iter()
            .position(|stored| stored.page_number == page_number)
    }

    fn write_snapshot_group(
        &mut self,
        layout: PageLayout,
        stored: FileStoredPage<N>,
        existing_index: Option<usize>,
        sequence: u64,
    ) -> Result<(), FilePageStoreError> {
        if self.poisoned {
            return Err(FilePageStoreError::PoisonedWriter);
        }
        if self.consume_fault(PageStoreFaultPoint::BeforeWrite) {
            return Err(FilePageStoreError::InjectedFault(
                PageStoreFaultPoint::BeforeWrite,
            ));
        }

        let header_frame = build_page_store_frame(
            PageStoreFrameKind::SnapshotHeader,
            sequence,
            stored.page_number().get(),
            stored.page_version().get(),
        );
        self.file.write_all(&header_frame).map_err(|source| {
            self.poisoned = true;
            FilePageStoreError::Io(PageStoreIoError::new(
                PageStoreIoStage::WriteSnapshotHeaderFrame,
                source,
            ))
        })?;

        let required_position_frame = build_page_store_frame(
            PageStoreFrameKind::RequiredPosition,
            sequence,
            stored.required_position().get(),
            0,
        );
        self.file
            .write_all(&required_position_frame)
            .map_err(|source| {
                self.poisoned = true;
                FilePageStoreError::Io(PageStoreIoError::new(
                    PageStoreIoStage::WriteRequiredPositionFrame,
                    source,
                ))
            })?;

        let page_bytes = stored.bytes();
        for chunk_index in 0..layout.chunk_count {
            let mut chunk = [0_u8; PAGE_CHUNK_WIDTH];
            let start = chunk_index * PAGE_CHUNK_WIDTH;
            let logical_len = layout.logical_bytes_for_chunk(chunk_index);
            let end = start + logical_len;
            chunk[..logical_len].copy_from_slice(&page_bytes[start..end]);
            let chunk_index_u64 = u64::try_from(chunk_index).map_err(|_| {
                FilePageStoreError::Io(PageStoreIoError::new(
                    PageStoreIoStage::WritePageDataFrame,
                    io::Error::other("chunk index overflow"),
                ))
            })?;
            let data_frame = build_page_store_frame_with_payload2_bytes(
                PageStoreFrameKind::PageData,
                sequence,
                chunk_index_u64,
                chunk,
            );
            self.file.write_all(&data_frame).map_err(|source| {
                self.poisoned = true;
                FilePageStoreError::Io(PageStoreIoError::new(
                    PageStoreIoStage::WritePageDataFrame,
                    source,
                ))
            })?;
        }

        self.file.sync_all().map_err(|source| {
            self.poisoned = true;
            FilePageStoreError::Io(PageStoreIoError::new(
                PageStoreIoStage::SyncPageGroup,
                source,
            ))
        })?;

        if let Some(index) = existing_index {
            self.pages[index] = stored;
        } else {
            self.pages.push(stored);
        }
        self.next_sequence = sequence.checked_add(1);

        if self.consume_fault(PageStoreFaultPoint::AfterWrite) {
            Err(FilePageStoreError::InjectedFault(
                PageStoreFaultPoint::AfterWrite,
            ))
        } else {
            Ok(())
        }
    }
}

/// Unrecovered owner returned by [`open_transaction_page_storage`].
pub type UnrecoveredFileTransactionPageStorage<const N: usize> =
    UnrecoveredTransactionPageStorage<FileCommitLog<N>, FilePageStore<N>, N>;

/// Opens and locks one WAL/page-store pair without exposing either before recovery.
///
/// The WAL is opened first and the page store second. A second-stage failure
/// drops the already-opened WAL before returning.
pub fn open_transaction_page_storage<const N: usize, LogPath, StorePath>(
    log_path: LogPath,
    store_path: StorePath,
) -> Result<UnrecoveredFileTransactionPageStorage<N>, FileTransactionPageStorageOpenError>
where
    LogPath: AsRef<Path>,
    StorePath: AsRef<Path>,
{
    let log = FileCommitLog::<N>::open_transaction_page_capable(log_path)
        .map_err(FileTransactionPageStorageOpenError::CommitLog)?;
    let store = FilePageStore::<N>::open(store_path)
        .map_err(FileTransactionPageStorageOpenError::PageStore)?;
    Ok(UnrecoveredTransactionPageStorage::new(log, store))
}

impl<const N: usize> ntsql_page::PageStore<N> for FilePageStore<N> {
    type Error = FilePageStoreError;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn write_page(
        &mut self,
        page: &ntsql_page::DirtyPage<N>,
        permit: ntsql_page::PageWritePermit<'_>,
    ) -> Result<(), Self::Error> {
        if self.poisoned {
            return Err(FilePageStoreError::PoisonedWriter);
        }
        let layout = PageLayout::for_const::<N>().map_err(FilePageStoreError::PageWidth)?;
        if !self.lineage.same_lineage(page.address().lineage()) {
            return Err(FilePageStoreError::ForeignPageLineage(
                page.address().number(),
            ));
        }
        if !self
            .lineage
            .same_lineage(permit.durable_position().lineage())
        {
            return Err(FilePageStoreError::ForeignPermitLineage(
                permit.durable_position().clone(),
            ));
        }
        if permit.durable_position() != page.required_position() {
            return Err(FilePageStoreError::PermitPositionMismatch {
                expected: page.required_position().clone(),
                actual: permit.durable_position().clone(),
            });
        }
        if page.required_position().get() == 0 {
            return Err(FilePageStoreError::RequiredPositionZero(
                page.address().number(),
            ));
        }

        let sequence = self
            .next_sequence
            .ok_or(FilePageStoreError::StoreSequenceSpaceExhausted)?;
        let existing_index = self.page_index(page.address().number());
        if existing_index.is_none() && self.pages.try_reserve(1).is_err() {
            return Err(FilePageStoreError::SnapshotCapacityExhausted);
        }

        let stored = FileStoredPage {
            page_number: page.address().number(),
            page_version: page.version(),
            bytes: *page.image().bytes(),
            required_position: page.required_position().clone(),
            store_sequence: sequence,
        };
        self.write_snapshot_group(layout, stored, existing_index, sequence)
    }
}

fn require_file_recovery_source_match<const N: usize>(
    candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
    current: Option<&StoredPageSnapshotObservation<N>>,
) -> Result<(), FileCommittedPageRecoveryStoreError<N>> {
    match compare_committed_transaction_page_recovery_candidate(candidate, current) {
        Ok(DurableCommittedTransactionPageRecoveryComparison::SourceMatches) => Ok(()),
        Ok(actual) => Err(FileCommittedPageRecoveryStoreError::SourceNotMatched { actual }),
        Err(source) => Err(FileCommittedPageRecoveryStoreError::SourceComparison(
            Box::new(source),
        )),
    }
}

impl<const N: usize> ntsql_transaction::DurablePageStoreSnapshotSource<N> for FilePageStore<N> {
    type ObservationError = FileCommittedPageRecoveryObservationError<N>;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn observe_page(
        &self,
        page_number: PageNumber,
    ) -> Result<Option<StoredPageSnapshotObservation<N>>, Self::ObservationError> {
        if self.poisoned {
            return Err(FileCommittedPageRecoveryObservationError::PoisonedWriter);
        }
        self.page(page_number)
            .map(FileStoredPage::page_recovery_observation)
            .transpose()
            .map_err(|source| {
                FileCommittedPageRecoveryObservationError::Projection(Box::new(source))
            })
    }
}

impl<const N: usize> DurablePageStoreInventorySource<N> for FilePageStore<N> {
    type InventoryError = FilePageStoreInventoryError<N>;

    fn durable_page_store_inventory(
        &mut self,
    ) -> Result<Vec<StoredPageSnapshotObservation<N>>, Self::InventoryError> {
        if self.poisoned {
            return Err(FilePageStoreInventoryError::PoisonedWriter);
        }
        let page_count = self.pages.len();
        let mut inventory = Vec::new();
        inventory
            .try_reserve_exact(page_count)
            .map_err(|_| FilePageStoreInventoryError::CapacityExhausted { page_count })?;
        for page in &self.pages {
            inventory.push(page.page_recovery_observation().map_err(|source| {
                FilePageStoreInventoryError::Projection {
                    page_number: page.page_number(),
                    source: Box::new(source),
                }
            })?);
        }
        inventory.sort_unstable_by_key(StoredPageSnapshotObservation::page_number);
        Ok(inventory)
    }
}

impl<const N: usize> ntsql_transaction::CommittedTransactionPageRecoveryStore<N>
    for FilePageStore<N>
{
    type WriteError = FileCommittedPageRecoveryStoreError<N>;

    fn compare_and_replace(
        &mut self,
        candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
        permit: CommittedTransactionPageRecoveryWritePermit<'_>,
    ) -> Result<(), Self::WriteError> {
        if self.poisoned {
            return Err(FileCommittedPageRecoveryStoreError::PageStore(
                FilePageStoreError::PoisonedWriter,
            ));
        }

        let latest = candidate.latest_committed();
        let target = latest.observation();
        if !self.lineage.same_lineage(target.position().lineage()) {
            return Err(
                FileCommittedPageRecoveryStoreError::ForeignTargetPagePosition(
                    target.position().clone(),
                ),
            );
        }
        if !self
            .lineage
            .same_lineage(latest.commit_position().lineage())
        {
            return Err(
                FileCommittedPageRecoveryStoreError::ForeignTargetCommitPosition(
                    latest.commit_position().clone(),
                ),
            );
        }
        if !self.lineage.same_lineage(permit.page_position().lineage()) {
            return Err(
                FileCommittedPageRecoveryStoreError::ForeignPermitPagePosition(
                    permit.page_position().clone(),
                ),
            );
        }
        if !self
            .lineage
            .same_lineage(permit.commit_position().lineage())
        {
            return Err(
                FileCommittedPageRecoveryStoreError::ForeignPermitCommitPosition(
                    permit.commit_position().clone(),
                ),
            );
        }
        if permit.page_position() != target.position() {
            return Err(
                FileCommittedPageRecoveryStoreError::PermitPagePositionMismatch {
                    expected: target.position().clone(),
                    actual: permit.page_position().clone(),
                },
            );
        }
        if permit.commit_position() != latest.commit_position() {
            return Err(
                FileCommittedPageRecoveryStoreError::PermitCommitPositionMismatch {
                    expected: latest.commit_position().clone(),
                    actual: permit.commit_position().clone(),
                },
            );
        }

        let page_number = target.page().page_number();
        let current = self
            .page(page_number)
            .map(FileStoredPage::page_recovery_observation)
            .transpose()
            .map_err(|source| {
                FileCommittedPageRecoveryStoreError::CurrentObservation(Box::new(source))
            })?;
        require_file_recovery_source_match(candidate, current.as_ref())?;

        let layout = PageLayout::for_const::<N>()
            .map_err(FilePageStoreError::PageWidth)
            .map_err(FileCommittedPageRecoveryStoreError::PageStore)?;
        let sequence = self
            .next_sequence
            .ok_or(FilePageStoreError::StoreSequenceSpaceExhausted)
            .map_err(FileCommittedPageRecoveryStoreError::PageStore)?;
        let page_index = self.page_index(page_number);
        if page_index.is_none() {
            self.pages
                .try_reserve(1)
                .map_err(|_| FilePageStoreError::SnapshotCapacityExhausted)
                .map_err(FileCommittedPageRecoveryStoreError::PageStore)?;
        }

        let stored = FileStoredPage {
            page_number,
            page_version: target.page().page_version(),
            bytes: *target.page().image().bytes(),
            required_position: target.position().clone(),
            store_sequence: sequence,
        };
        self.write_snapshot_group(layout, stored, page_index, sequence)
            .map_err(FileCommittedPageRecoveryStoreError::PageStore)
    }
}

fn require_file_restart_checkpoint_repair_source_match<const N: usize>(
    candidate: &DurableTransactionRestartCheckpointPageRepairCandidate<'_, '_, N>,
    current: Option<&StoredPageSnapshotObservation<N>>,
) -> Result<(), FileRestartCheckpointPageRepairStoreError<N>> {
    match compare_transaction_restart_checkpoint_page_repair_candidate(candidate, current) {
        Ok(DurableTransactionRestartCheckpointPageRepairComparison::SourceMatches) => Ok(()),
        Ok(actual) => Err(FileRestartCheckpointPageRepairStoreError::SourceNotMatched { actual }),
        Err(source) => Err(FileRestartCheckpointPageRepairStoreError::SourceComparison(
            Box::new(source),
        )),
    }
}

impl<const N: usize> TransactionRestartCheckpointPageRepairStore<N> for FilePageStore<N> {
    type WriteError = FileRestartCheckpointPageRepairStoreError<N>;

    fn compare_and_replace_replay_page(
        &mut self,
        candidate: &DurableTransactionRestartCheckpointPageRepairCandidate<'_, '_, N>,
        permit: TransactionRestartCheckpointPageRepairWritePermit<'_>,
    ) -> Result<(), Self::WriteError> {
        let target = candidate.target();
        if !self.lineage.same_lineage(target.page_position().lineage()) {
            return Err(
                FileRestartCheckpointPageRepairStoreError::ForeignTargetPagePosition(
                    target.page_position().clone(),
                ),
            );
        }
        let expected_commit_position = match target.kind() {
            DurableTransactionRestartCheckpointPageRepairTargetKind::Raw => None,
            DurableTransactionRestartCheckpointPageRepairTargetKind::CommittedTransaction {
                commit_position,
                ..
            } => {
                if !self.lineage.same_lineage(commit_position.lineage()) {
                    return Err(
                        FileRestartCheckpointPageRepairStoreError::ForeignTargetCommitPosition(
                            commit_position.clone(),
                        ),
                    );
                }
                Some(commit_position)
            }
        };
        if !self.lineage.same_lineage(permit.page_position().lineage()) {
            return Err(
                FileRestartCheckpointPageRepairStoreError::ForeignPermitPagePosition(
                    permit.page_position().clone(),
                ),
            );
        }
        if let Some(commit_position) = permit.commit_position()
            && !self.lineage.same_lineage(commit_position.lineage())
        {
            return Err(
                FileRestartCheckpointPageRepairStoreError::ForeignPermitCommitPosition(
                    commit_position.clone(),
                ),
            );
        }
        if permit.page_position() != target.page_position() {
            return Err(
                FileRestartCheckpointPageRepairStoreError::PermitPagePositionMismatch {
                    expected: target.page_position().clone(),
                    actual: permit.page_position().clone(),
                },
            );
        }
        if permit.commit_position() != expected_commit_position {
            return Err(
                FileRestartCheckpointPageRepairStoreError::PermitCommitPositionMismatch {
                    expected: expected_commit_position.cloned(),
                    actual: permit.commit_position().cloned(),
                },
            );
        }

        let page_number = target.page_number();
        let current = self
            .page(page_number)
            .map(FileStoredPage::page_recovery_observation)
            .transpose()
            .map_err(|source| {
                FileRestartCheckpointPageRepairStoreError::CurrentObservation(Box::new(source))
            })?;
        require_file_restart_checkpoint_repair_source_match(candidate, current.as_ref())?;

        let layout = PageLayout::for_const::<N>()
            .map_err(FilePageStoreError::PageWidth)
            .map_err(FileRestartCheckpointPageRepairStoreError::PageStore)?;
        let sequence = self
            .next_sequence
            .ok_or(FilePageStoreError::StoreSequenceSpaceExhausted)
            .map_err(FileRestartCheckpointPageRepairStoreError::PageStore)?;
        let page_index = self.page_index(page_number);
        if page_index.is_none() {
            self.pages
                .try_reserve(1)
                .map_err(|_| FilePageStoreError::SnapshotCapacityExhausted)
                .map_err(FileRestartCheckpointPageRepairStoreError::PageStore)?;
        }

        let stored = FileStoredPage {
            page_number,
            page_version: target.page_version(),
            bytes: *target.bytes(),
            required_position: target.page_position().clone(),
            store_sequence: sequence,
        };
        self.write_snapshot_group(layout, stored, page_index, sequence)
            .map_err(FileRestartCheckpointPageRepairStoreError::PageStore)
    }
}

fn build_page_store_header(persistent_id: PersistentLogId, page_width: u64) -> [u8; HEADER_LENGTH] {
    let mut header = [0_u8; HEADER_LENGTH];
    header[..8].copy_from_slice(&PAGE_STORE_HEADER_MAGIC);
    write_u16(&mut header, 8, PAGE_STORE_FORMAT_VERSION);
    write_u16(&mut header, 10, HEADER_LENGTH_U16);
    write_u32(&mut header, 12, 0);
    write_u128(&mut header, 16, persistent_id.get());
    write_u64(&mut header, 32, page_width);
    // 40..56 reserved zero
    let checksum = checksum_v1(&header[..HEADER_CHECKSUM_OFFSET]);
    write_u64(&mut header, HEADER_CHECKSUM_OFFSET, checksum);
    header
}

fn build_page_store_header_v2(
    persistent_id: PersistentLogId,
    page_width: u64,
    database_file_identity: DatabaseFileHeaderIdentity,
) -> [u8; PAGE_STORE_HEADER_V2_LENGTH] {
    let mut header = [0_u8; PAGE_STORE_HEADER_V2_LENGTH];
    header[..8].copy_from_slice(&PAGE_STORE_HEADER_MAGIC);
    write_u16(&mut header, 8, PAGE_STORE_FORMAT_VERSION_V2);
    write_u16(&mut header, 10, PAGE_STORE_HEADER_V2_LENGTH_U16);
    write_u128(&mut header, 16, persistent_id.get());
    write_u64(&mut header, 32, page_width);
    let identity =
        database_child_identity_codec::encode_database_child_identity(database_file_identity);
    header[PAGE_STORE_HEADER_V2_IDENTITY_OFFSET
        ..PAGE_STORE_HEADER_V2_IDENTITY_OFFSET + identity.len()]
        .copy_from_slice(&identity);
    let checksum = checksum_v1(&header[..PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET]);
    write_u64(&mut header, PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET, checksum);
    header
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PageStoreHeaderV2Metadata {
    persistent_id: PersistentLogId,
    database_file_identity: DatabaseFileHeaderIdentity,
}

fn parse_page_store_header(
    header: &[u8; HEADER_LENGTH],
    layout: PageLayout,
) -> Result<PersistentLogId, PageStoreFormatError> {
    if header[..8] != PAGE_STORE_HEADER_MAGIC {
        return Err(PageStoreFormatError::new(
            0,
            PageStoreFormatErrorReason::HeaderMagic,
        ));
    }
    let version = read_u16(header, 8);
    if version != PAGE_STORE_FORMAT_VERSION {
        return Err(PageStoreFormatError::new(
            8,
            PageStoreFormatErrorReason::HeaderVersion { actual: version },
        ));
    }
    let length = read_u16(header, 10);
    if usize::from(length) != HEADER_LENGTH {
        return Err(PageStoreFormatError::new(
            10,
            PageStoreFormatErrorReason::HeaderLength { actual: length },
        ));
    }
    let flags = read_u32(header, 12);
    if flags != 0 {
        return Err(PageStoreFormatError::new(
            12,
            PageStoreFormatErrorReason::HeaderFlags { actual: flags },
        ));
    }
    let lineage_raw = read_u128(header, 16);
    let persistent_id = match PersistentLogId::new(lineage_raw) {
        Some(id) => id,
        None => {
            return Err(PageStoreFormatError::new(
                16,
                PageStoreFormatErrorReason::LineageIdZero,
            ));
        }
    };
    let page_width = read_u64(header, 32);
    if page_width == 0 {
        return Err(PageStoreFormatError::new(
            32,
            PageStoreFormatErrorReason::HeaderPageWidthZero,
        ));
    }
    if page_width != layout.width_u64 {
        return Err(PageStoreFormatError::new(
            32,
            PageStoreFormatErrorReason::HeaderPageWidthMismatch {
                expected: layout.width_u64,
                actual: page_width,
            },
        ));
    }
    // 40..56 reserved
    if header[40..56].iter().any(|byte| *byte != 0) {
        return Err(PageStoreFormatError::new(
            40,
            PageStoreFormatErrorReason::HeaderReserved,
        ));
    }
    let actual_checksum = read_u64(header, HEADER_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&header[..HEADER_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(PageStoreFormatError::new(
            56,
            PageStoreFormatErrorReason::HeaderChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        ));
    }
    Ok(persistent_id)
}

fn parse_page_store_header_v2(
    header: &[u8; PAGE_STORE_HEADER_V2_LENGTH],
    layout: PageLayout,
) -> Result<PageStoreHeaderV2Metadata, PageStoreFormatError> {
    if header[..8] != PAGE_STORE_HEADER_MAGIC {
        return Err(PageStoreFormatError::new(
            0,
            PageStoreFormatErrorReason::HeaderMagic,
        ));
    }
    let version = read_u16(header, 8);
    if version != PAGE_STORE_FORMAT_VERSION_V2 {
        return Err(PageStoreFormatError::new(
            8,
            PageStoreFormatErrorReason::HeaderVersion { actual: version },
        ));
    }
    let length = read_u16(header, 10);
    if usize::from(length) != PAGE_STORE_HEADER_V2_LENGTH {
        return Err(PageStoreFormatError::new(
            10,
            PageStoreFormatErrorReason::HeaderV2Length { actual: length },
        ));
    }
    let flags = read_u32(header, 12);
    if flags != 0 {
        return Err(PageStoreFormatError::new(
            12,
            PageStoreFormatErrorReason::HeaderFlags { actual: flags },
        ));
    }
    let persistent_id = PersistentLogId::new(read_u128(header, 16))
        .ok_or_else(|| PageStoreFormatError::new(16, PageStoreFormatErrorReason::LineageIdZero))?;
    let page_width = read_u64(header, 32);
    if page_width == 0 {
        return Err(PageStoreFormatError::new(
            32,
            PageStoreFormatErrorReason::HeaderPageWidthZero,
        ));
    }
    if page_width != layout.width_u64 {
        return Err(PageStoreFormatError::new(
            32,
            PageStoreFormatErrorReason::HeaderPageWidthMismatch {
                expected: layout.width_u64,
                actual: page_width,
            },
        ));
    }
    if header[40..PAGE_STORE_HEADER_V2_IDENTITY_OFFSET]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(PageStoreFormatError::new(
            40,
            PageStoreFormatErrorReason::HeaderReserved,
        ));
    }
    let mut identity_bytes =
        [0_u8; database_child_identity_codec::DATABASE_CHILD_IDENTITY_V1_LENGTH];
    identity_bytes.copy_from_slice(
        &header[PAGE_STORE_HEADER_V2_IDENTITY_OFFSET
            ..PAGE_STORE_HEADER_V2_IDENTITY_OFFSET
                + database_child_identity_codec::DATABASE_CHILD_IDENTITY_V1_LENGTH],
    );
    let database_file_identity = database_child_identity_codec::decode_database_child_identity(
        &identity_bytes,
    )
    .map_err(|source| {
        PageStoreFormatError::new(
            (PAGE_STORE_HEADER_V2_IDENTITY_OFFSET + source.offset()) as u64,
            PageStoreFormatErrorReason::HeaderDatabaseChildIdentity(source.reason()),
        )
    })?;
    if header[PAGE_STORE_HEADER_V2_IDENTITY_OFFSET
        + database_child_identity_codec::DATABASE_CHILD_IDENTITY_V1_LENGTH
        ..PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(PageStoreFormatError::new(
            (PAGE_STORE_HEADER_V2_IDENTITY_OFFSET
                + database_child_identity_codec::DATABASE_CHILD_IDENTITY_V1_LENGTH)
                as u64,
            PageStoreFormatErrorReason::HeaderReserved,
        ));
    }
    let actual_checksum = read_u64(header, PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&header[..PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(PageStoreFormatError::new(
            PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET as u64,
            PageStoreFormatErrorReason::HeaderChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        ));
    }
    Ok(PageStoreHeaderV2Metadata {
        persistent_id,
        database_file_identity,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DecodedPageStoreFrame {
    kind: PageStoreFrameKind,
    payload_a: u64,
    payload_b: u64,
    payload_c: u64,
    payload_c_bytes: [u8; PAGE_CHUNK_WIDTH],
}

fn parse_page_store_frame(
    frame: &[u8; FRAME_LENGTH],
    offset: u64,
) -> Result<DecodedPageStoreFrame, PageStoreFormatError> {
    if frame[..4] != PAGE_STORE_FRAME_MAGIC {
        return Err(PageStoreFormatError::new(
            offset,
            PageStoreFormatErrorReason::FrameMagic,
        ));
    }
    let kind_raw = read_u16(frame, 4);
    let kind = PageStoreFrameKind::from_u16(kind_raw).ok_or_else(|| {
        PageStoreFormatError::new(
            offset + 4,
            PageStoreFormatErrorReason::FrameKind { actual: kind_raw },
        )
    })?;
    let version = read_u16(frame, 6);
    if version != PAGE_STORE_FORMAT_VERSION {
        return Err(PageStoreFormatError::new(
            offset + 6,
            PageStoreFormatErrorReason::FrameVersion { actual: version },
        ));
    }
    let flags = read_u32(frame, 8);
    if flags != 0 {
        return Err(PageStoreFormatError::new(
            offset + 8,
            PageStoreFormatErrorReason::FrameFlags { actual: flags },
        ));
    }
    let length = read_u16(frame, 12);
    if usize::from(length) != FRAME_LENGTH {
        return Err(PageStoreFormatError::new(
            offset + 12,
            PageStoreFormatErrorReason::FrameLength { actual: length },
        ));
    }
    if read_u16(frame, 14) != 0 || frame[40..48].iter().any(|byte| *byte != 0) {
        return Err(PageStoreFormatError::new(
            offset + 14,
            PageStoreFormatErrorReason::FrameReserved,
        ));
    }
    let actual_checksum = read_u64(frame, FRAME_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&frame[..FRAME_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(PageStoreFormatError::new(
            offset + 48,
            PageStoreFormatErrorReason::FrameChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        ));
    }

    let mut payload_c_bytes = [0_u8; PAGE_CHUNK_WIDTH];
    payload_c_bytes.copy_from_slice(&frame[32..40]);
    Ok(DecodedPageStoreFrame {
        kind,
        payload_a: read_u64(frame, 16),
        payload_b: read_u64(frame, 24),
        payload_c: read_u64(frame, 32),
        payload_c_bytes,
    })
}

fn build_page_store_frame(
    kind: PageStoreFrameKind,
    payload_a: u64,
    payload_b: u64,
    payload_c: u64,
) -> [u8; FRAME_LENGTH] {
    build_page_store_frame_with_payload2_bytes(kind, payload_a, payload_b, payload_c.to_be_bytes())
}

fn build_page_store_frame_with_payload2_bytes(
    kind: PageStoreFrameKind,
    payload_a: u64,
    payload_b: u64,
    payload_c_bytes: [u8; PAGE_CHUNK_WIDTH],
) -> [u8; FRAME_LENGTH] {
    let mut frame = [0_u8; FRAME_LENGTH];
    frame[..4].copy_from_slice(&PAGE_STORE_FRAME_MAGIC);
    write_u16(&mut frame, 4, kind.code());
    write_u16(&mut frame, 6, PAGE_STORE_FORMAT_VERSION);
    write_u32(&mut frame, 8, 0);
    write_u16(&mut frame, 12, FRAME_LENGTH_U16);
    write_u16(&mut frame, 14, 0);
    write_u64(&mut frame, 16, payload_a);
    write_u64(&mut frame, 24, payload_b);
    frame[32..40].copy_from_slice(&payload_c_bytes);
    let checksum = checksum_v1(&frame[..FRAME_CHECKSUM_OFFSET]);
    write_u64(&mut frame, FRAME_CHECKSUM_OFFSET, checksum);
    frame
}

fn sync_page_store_parent_directory(path: &Path) -> Result<(), PageStoreCreateError> {
    let parent = match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => return Err(PageStoreCreateError::MissingParentDirectory),
    };
    let directory = File::open(parent).map_err(|source| {
        PageStoreCreateError::Io(PageStoreIoError::new(
            PageStoreIoStage::OpenParentDirectory,
            source,
        ))
    })?;
    directory.sync_all().map_err(|source| {
        PageStoreCreateError::Io(PageStoreIoError::new(
            PageStoreIoStage::SyncParentDirectory,
            source,
        ))
    })
}

// --- Open/scan state machine for page store ---

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageStoreGroupState {
    /// Expecting snapshot header or end of file.
    Idle,
    /// Received snapshot header, expecting required-position frame.
    AwaitingRequiredPosition {
        header_offset: u64,
        sequence: u64,
        page_number: PageNumber,
        page_version: PageVersion,
    },
    /// Received required-position, expecting page-data frames.
    AwaitingPageData {
        header_offset: u64,
        sequence: u64,
        page_number: PageNumber,
        page_version: PageVersion,
        required_position: u64,
        next_chunk_index: usize,
        chunk_count: usize,
    },
}

struct PageStoreOpenState<const N: usize> {
    layout: PageLayout,
    lineage: LogLineage,
    pages: Vec<FileStoredPage<N>>,
    next_sequence: Option<u64>,
    group_state: PageStoreGroupState,
    pending_bytes: [u8; N],
}

impl<const N: usize> PageStoreOpenState<N> {
    fn new(layout: PageLayout, lineage: LogLineage) -> Self {
        Self {
            layout,
            lineage,
            pages: Vec::new(),
            next_sequence: Some(1),
            group_state: PageStoreGroupState::Idle,
            pending_bytes: [0_u8; N],
        }
    }

    fn pending_group_header_offset(&self) -> Option<u64> {
        match self.group_state {
            PageStoreGroupState::Idle => None,
            PageStoreGroupState::AwaitingRequiredPosition { header_offset, .. }
            | PageStoreGroupState::AwaitingPageData { header_offset, .. } => Some(header_offset),
        }
    }

    fn apply_frame(
        &mut self,
        frame: DecodedPageStoreFrame,
        offset: u64,
    ) -> Result<(), PageStoreOpenError> {
        match self.group_state {
            PageStoreGroupState::Idle => self.apply_idle_frame(frame, offset),
            PageStoreGroupState::AwaitingRequiredPosition { .. } => {
                self.apply_awaiting_required_position_frame(frame, offset)
            }
            PageStoreGroupState::AwaitingPageData { .. } => {
                self.apply_awaiting_page_data_frame(frame, offset)
            }
        }
    }

    fn apply_idle_frame(
        &mut self,
        frame: DecodedPageStoreFrame,
        offset: u64,
    ) -> Result<(), PageStoreOpenError> {
        if frame.kind != PageStoreFrameKind::SnapshotHeader {
            match frame.kind {
                PageStoreFrameKind::RequiredPosition | PageStoreFrameKind::PageData => {
                    return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                        offset + 4,
                        PageStoreFormatErrorReason::PageDataWithoutHeader,
                    )));
                }
                PageStoreFrameKind::SnapshotHeader => {}
            }
        }
        // Validate sequence
        if frame.payload_a == 0 {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 16,
                PageStoreFormatErrorReason::SnapshotSequenceZero,
            )));
        }
        let expected_sequence =
            self.next_sequence
                .ok_or(PageStoreOpenError::Format(PageStoreFormatError::new(
                    offset + 16,
                    PageStoreFormatErrorReason::SnapshotSequenceSpaceExhausted,
                )))?;
        if frame.payload_a != expected_sequence {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 16,
                PageStoreFormatErrorReason::SnapshotSequenceOutOfOrder {
                    expected: expected_sequence,
                    actual: frame.payload_a,
                },
            )));
        }
        // Validate page number nonzero
        let page_number = PageNumber::new(frame.payload_b).ok_or_else(|| {
            PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 24,
                PageStoreFormatErrorReason::SnapshotPageNumberZero,
            ))
        })?;
        let page_version = PageVersion::new(frame.payload_c);

        self.pending_bytes = [0_u8; N];
        self.group_state = PageStoreGroupState::AwaitingRequiredPosition {
            header_offset: offset,
            sequence: frame.payload_a,
            page_number,
            page_version,
        };
        Ok(())
    }

    fn apply_awaiting_required_position_frame(
        &mut self,
        frame: DecodedPageStoreFrame,
        offset: u64,
    ) -> Result<(), PageStoreOpenError> {
        let (header_offset, sequence, page_number, page_version) = match self.group_state {
            PageStoreGroupState::AwaitingRequiredPosition {
                header_offset,
                sequence,
                page_number,
                page_version,
            } => (header_offset, sequence, page_number, page_version),
            _ => {
                return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                    offset + 4,
                    PageStoreFormatErrorReason::PageDataWithoutHeader,
                )));
            }
        };
        if frame.kind != PageStoreFrameKind::RequiredPosition {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 4,
                PageStoreFormatErrorReason::UnexpectedKindAfterHeader {
                    actual: frame.kind.code(),
                },
            )));
        }
        if frame.payload_a != sequence {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 16,
                PageStoreFormatErrorReason::RequiredPositionSequenceMismatch {
                    expected: sequence,
                    actual: frame.payload_a,
                },
            )));
        }
        if frame.payload_b == 0 {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 24,
                PageStoreFormatErrorReason::RequiredPositionZero,
            )));
        }
        if frame.payload_c != 0 {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 32,
                PageStoreFormatErrorReason::RequiredPositionPayloadCNonzero {
                    actual: frame.payload_c,
                },
            )));
        }
        self.group_state = PageStoreGroupState::AwaitingPageData {
            header_offset,
            sequence,
            page_number,
            page_version,
            required_position: frame.payload_b,
            next_chunk_index: 0,
            chunk_count: self.layout.chunk_count,
        };
        Ok(())
    }

    fn apply_awaiting_page_data_frame(
        &mut self,
        frame: DecodedPageStoreFrame,
        offset: u64,
    ) -> Result<(), PageStoreOpenError> {
        let (
            header_offset,
            sequence,
            page_number,
            page_version,
            required_position,
            next_chunk_index,
            chunk_count,
        ) = match self.group_state {
            PageStoreGroupState::AwaitingPageData {
                header_offset,
                sequence,
                page_number,
                page_version,
                required_position,
                next_chunk_index,
                chunk_count,
            } => (
                header_offset,
                sequence,
                page_number,
                page_version,
                required_position,
                next_chunk_index,
                chunk_count,
            ),
            _ => {
                return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                    offset + 4,
                    PageStoreFormatErrorReason::PageDataWithoutHeader,
                )));
            }
        };
        if frame.kind != PageStoreFrameKind::PageData {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 4,
                PageStoreFormatErrorReason::UnexpectedKindAfterRequiredPosition {
                    actual: frame.kind.code(),
                },
            )));
        }
        if frame.payload_a != sequence {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 16,
                PageStoreFormatErrorReason::PageDataSequenceMismatch {
                    expected: sequence,
                    actual: frame.payload_a,
                },
            )));
        }
        let expected_chunk_u64 = u64::try_from(next_chunk_index).map_err(|_| {
            PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 24,
                PageStoreFormatErrorReason::PageDataChunkIndexOutOfSequence {
                    expected: u64::MAX,
                    actual: frame.payload_b,
                },
            ))
        })?;
        if frame.payload_b != expected_chunk_u64 {
            return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                offset + 24,
                PageStoreFormatErrorReason::PageDataChunkIndexOutOfSequence {
                    expected: expected_chunk_u64,
                    actual: frame.payload_b,
                },
            )));
        }

        // Copy chunk data
        let start = next_chunk_index * PAGE_CHUNK_WIDTH;
        let logical_len = self.layout.logical_bytes_for_chunk(next_chunk_index);
        let end = start + logical_len;
        for (destination, source_byte) in self.pending_bytes[start..end]
            .iter_mut()
            .zip(frame.payload_c_bytes[..logical_len].iter())
        {
            *destination = *source_byte;
        }

        let new_next = next_chunk_index + 1;

        // Check final chunk padding
        if new_next == chunk_count && self.layout.final_chunk_len < PAGE_CHUNK_WIDTH {
            let pad_start = self.layout.final_chunk_len;
            if frame.payload_c_bytes[pad_start..]
                .iter()
                .any(|byte| *byte != 0)
            {
                return Err(PageStoreOpenError::Format(PageStoreFormatError::new(
                    offset + 32,
                    PageStoreFormatErrorReason::PageDataFinalPaddingNonzero,
                )));
            }
        }

        if new_next == chunk_count {
            let stored = FileStoredPage {
                page_number,
                page_version,
                bytes: self.pending_bytes,
                required_position: self.lineage.position(required_position),
                store_sequence: sequence,
            };
            if let Some(index) = self
                .pages
                .iter()
                .position(|page| page.page_number == page_number)
            {
                self.pages[index] = stored;
            } else {
                self.pages
                    .try_reserve(1)
                    .map_err(|_| PageStoreOpenError::SnapshotCapacityExhausted)?;
                self.pages.push(stored);
            }
            self.next_sequence = sequence.checked_add(1);
            self.group_state = PageStoreGroupState::Idle;
        } else {
            self.group_state = PageStoreGroupState::AwaitingPageData {
                header_offset,
                sequence,
                page_number,
                page_version,
                required_position,
                next_chunk_index: new_next,
                chunk_count,
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use ntsql_database::{DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole, DatabaseId};
    use ntsql_page::{
        DurablePageReconciliation, PageAddress, PageImage, PageLog, PageNumber, PageVersion,
        StagePageWriteError, reconcile_durable_page, stage_page_write,
    };
    use ntsql_transaction::{
        CommittedTransactionPageRecoveryError, CommittedTransactionPageRecoveryOutcome,
        CoordinatedCommitError, DurableCommittedTransactionPageReconciliation,
        DurableCommittedTransactionPageReconciliationError,
        DurableTransactionPageCommitClassification, IndeterminateTransaction,
        TransactionCommitResolution, TransactionCommittedFlushError, TransactionCoordinator,
        TransactionLifecycleStatus, TransactionPageStageError, TransactionResolutionFailure,
        classify_durable_transaction_page, flush_committed_page,
        reconcile_committed_transaction_page, recover_committed_transaction_page,
    };
    use ntsql_wal::{CommitError, LogDurability, LogLineage, PersistentLogId};

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn database_file_header_identity(
        role: DatabaseFileRole,
    ) -> Result<DatabaseFileHeaderIdentity, io::Error> {
        let database_id = DatabaseId::new(0x1112_1314_1516_1718_191a_1b1c_1d1e_1f20)
            .ok_or_else(|| io::Error::other("test database ID is zero"))?;
        let file_id = DatabaseFileId::new(0x2122_2324_2526_2728_292a_2b2c_2d2e_2f30)
            .ok_or_else(|| io::Error::other("test file ID is zero"))?;
        Ok(DatabaseFileHeaderIdentity::new(
            database_id,
            DatabaseFileIdentity::new(role, file_id),
        ))
    }

    struct PoisonBeforeRecoveryCompare(FilePageStore<2>);

    impl ntsql_transaction::DurablePageStoreSnapshotSource<2> for PoisonBeforeRecoveryCompare {
        type ObservationError = FileCommittedPageRecoveryObservationError<2>;

        fn lineage(&self) -> &LogLineage {
            &self.0.lineage
        }

        fn observe_page(
            &self,
            page_number: PageNumber,
        ) -> Result<Option<StoredPageSnapshotObservation<2>>, Self::ObservationError> {
            <FilePageStore<2> as ntsql_transaction::DurablePageStoreSnapshotSource<2>>::observe_page(
                &self.0,
                page_number,
            )
        }
    }

    impl ntsql_transaction::CommittedTransactionPageRecoveryStore<2> for PoisonBeforeRecoveryCompare {
        type WriteError = FileCommittedPageRecoveryStoreError<2>;

        fn compare_and_replace(
            &mut self,
            candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, 2>,
            permit: CommittedTransactionPageRecoveryWritePermit<'_>,
        ) -> Result<(), Self::WriteError> {
            self.0.poisoned = true;
            <FilePageStore<2> as ntsql_transaction::CommittedTransactionPageRecoveryStore<
                2,
            >>::compare_and_replace(&mut self.0, candidate, permit)
        }
    }

    struct TargetAppearsBeforeRecoveryCompare(FilePageStore<2>);

    impl ntsql_transaction::DurablePageStoreSnapshotSource<2> for TargetAppearsBeforeRecoveryCompare {
        type ObservationError = FileCommittedPageRecoveryObservationError<2>;

        fn lineage(&self) -> &LogLineage {
            &self.0.lineage
        }

        fn observe_page(
            &self,
            page_number: PageNumber,
        ) -> Result<Option<StoredPageSnapshotObservation<2>>, Self::ObservationError> {
            <FilePageStore<2> as ntsql_transaction::DurablePageStoreSnapshotSource<2>>::observe_page(
                &self.0,
                page_number,
            )
        }
    }

    impl ntsql_transaction::CommittedTransactionPageRecoveryStore<2>
        for TargetAppearsBeforeRecoveryCompare
    {
        type WriteError = FileCommittedPageRecoveryStoreError<2>;

        fn compare_and_replace(
            &mut self,
            candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, 2>,
            permit: CommittedTransactionPageRecoveryWritePermit<'_>,
        ) -> Result<(), Self::WriteError> {
            let target = candidate.latest_committed().observation();
            let sequence = self
                .0
                .next_sequence
                .ok_or(FilePageStoreError::StoreSequenceSpaceExhausted)
                .map_err(FileCommittedPageRecoveryStoreError::PageStore)?;
            self.0
                .pages
                .try_reserve(1)
                .map_err(|_| FilePageStoreError::SnapshotCapacityExhausted)
                .map_err(FileCommittedPageRecoveryStoreError::PageStore)?;
            let stored = FileStoredPage {
                page_number: target.page().page_number(),
                page_version: target.page().page_version(),
                bytes: *target.page().image().bytes(),
                required_position: target.position().clone(),
                store_sequence: sequence,
            };
            let layout = PageLayout::for_const::<2>()
                .map_err(FilePageStoreError::PageWidth)
                .map_err(FileCommittedPageRecoveryStoreError::PageStore)?;
            self.0
                .write_snapshot_group(layout, stored, None, sequence)
                .map_err(FileCommittedPageRecoveryStoreError::PageStore)?;
            self.0.armed_fault = Some(PageStoreFaultPoint::BeforeWrite);

            <FilePageStore<2> as ntsql_transaction::CommittedTransactionPageRecoveryStore<
                2,
            >>::compare_and_replace(&mut self.0, candidate, permit)
        }
    }

    #[test]
    fn create_new_writes_v1_header_and_rejects_existing_path() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("create-new-v1")?;
        let path = directory.path().join("commit-log.bin");
        let persistent_id = persistent_id(7)?;

        let log = FileCommitLog::create_new(&path, persistent_id)?;
        assert_eq!(log.persistent_id(), persistent_id);
        assert!(log.records().is_empty());
        drop(log);

        let bytes = fs::read(&path)?;
        assert_eq!(bytes.len(), HEADER_LENGTH);
        let mut header = [0_u8; HEADER_LENGTH];
        header.copy_from_slice(&bytes);
        assert_eq!(parse_header(&header, HeaderExpectation::V1)?, persistent_id);
        assert_eq!(&header[..8], &HEADER_MAGIC);
        assert_eq!(read_u16(&header, 8), FORMAT_VERSION_V1);
        assert_eq!(read_u16(&header, 10), HEADER_LENGTH_U16);
        assert_eq!(read_u32(&header, 12), 0);
        assert_eq!(read_u128(&header, 16), persistent_id.get());
        assert_eq!(
            read_u64(&header, HEADER_CHECKSUM_OFFSET),
            0x4d9a_c185_1e54_3c92
        );

        let create_error = FileCommitLog::create_new(&path, persistent_id)
            .err()
            .ok_or_else(|| io::Error::other("existing path unexpectedly accepted"))?;
        match create_error {
            FileCreateError::Io(source) => {
                assert_eq!(source.stage(), FileIoStage::CreateFile);
                assert_eq!(source.io_source().kind(), io::ErrorKind::AlreadyExists);
            }
            FileCreateError::MissingParentDirectory | FileCreateError::PageWidth(_) => {
                return Err(
                    io::Error::other("existing file returned the wrong create error").into(),
                );
            }
        }

        let reopened = FileCommitLog::open(&path)?;
        assert_eq!(reopened.persistent_id(), persistent_id);
        assert!(reopened.records().is_empty());
        Ok(())
    }

    #[test]
    fn exclusive_lock_blocks_reopen_before_tail_repair_and_releases_on_drop()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("exclusive-lock")?;
        let path = directory.path().join("commit-log.bin");
        let log = FileCommitLog::create_new(&path, persistent_id(9)?)?;
        append_bytes(&path, &[1, 2, 3])?;
        let bytes_before_open = fs::read(&path)?;

        let error = FileCommitLog::open(&path)
            .err()
            .ok_or_else(|| io::Error::other("second writer acquired the file lock"))?;

        let FileOpenError::Io(source) = error else {
            return Err(io::Error::other("lock contention was not reported as I/O").into());
        };
        assert_eq!(source.stage(), FileIoStage::AcquireExclusiveLock);
        assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read(&path)?, bytes_before_open);

        drop(log);
        let reopened = FileCommitLog::open(&path)?;
        assert_eq!(fs::metadata(&path)?.len(), HEADER_LENGTH_U64);
        drop(reopened);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn hard_link_alias_cannot_bypass_exclusive_lock() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("hard-link-lock")?;
        let path = directory.path().join("commit-log.bin");
        let alias = directory.path().join("commit-log-alias.bin");
        let log = FileCommitLog::create_new(&path, persistent_id(10)?)?;
        fs::hard_link(&path, &alias)?;

        let error = FileCommitLog::open(&alias)
            .err()
            .ok_or_else(|| io::Error::other("hard-link alias bypassed the file lock"))?;

        let FileOpenError::Io(source) = error else {
            return Err(io::Error::other("alias lock contention was not reported as I/O").into());
        };
        assert_eq!(source.stage(), FileIoStage::AcquireExclusiveLock);
        assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);

        drop(log);
        let reopened = FileCommitLog::open(&alias)?;
        assert_eq!(reopened.persistent_id(), persistent_id(10)?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn wal_open_removes_dangling_candidate_but_rejects_selected_inode_alias()
    -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("wal-candidate-cleanup")?;
        let path = directory.path().join("commit-log.bin");
        let candidate = directory.path().join("commit-log.bin.reclaim-candidate");
        drop(FileCommitLog::<2>::create_new_transaction_page_capable(
            &path,
            persistent_id(170)?,
        )?);

        symlink(
            directory.path().join("missing-candidate-target"),
            &candidate,
        )?;
        drop(FileCommitLog::<2>::open_transaction_page_capable(&path)?);
        assert!(!candidate.exists());
        assert!(fs::symlink_metadata(&candidate).is_err());

        fs::hard_link(&path, &candidate)?;
        let error = FileCommitLog::<2>::open_transaction_page_capable(&path)
            .err()
            .ok_or_else(|| io::Error::other("selected/candidate inode alias was accepted"))?;
        let FileOpenError::Io(source) = error else {
            return Err(io::Error::other("candidate alias changed error category").into());
        };
        assert_eq!(
            source.stage(),
            FileIoStage::ReadReclamationCandidateMetadata
        );
        assert_eq!(source.io_source().kind(), io::ErrorKind::InvalidData);
        assert!(path.exists());
        assert!(candidate.exists());

        fs::remove_file(&candidate)?;
        drop(FileCommitLog::<2>::open_transaction_page_capable(&path)?);
        Ok(())
    }

    #[test]
    fn v1_format_bytes_cover_header_epoch_commit_and_marker() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("format-bytes-v1")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(11)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        let committed = coordinator.commit(active, &mut log)?;
        let bytes = fs::read(&path)?;

        assert_eq!(bytes.len(), HEADER_LENGTH + FRAME_LENGTH * 3);
        let mut header = [0_u8; HEADER_LENGTH];
        header.copy_from_slice(&bytes[..HEADER_LENGTH]);
        assert_eq!(
            parse_header(&header, HeaderExpectation::V1)?,
            persistent_id(11)?
        );

        let mut epoch_frame = [0_u8; FRAME_LENGTH];
        epoch_frame.copy_from_slice(&bytes[HEADER_LENGTH..HEADER_LENGTH + FRAME_LENGTH]);
        let epoch = parse_frame(&epoch_frame, HEADER_LENGTH_U64, LogFormat::V1)?;
        assert_eq!(
            read_u64(&epoch_frame, FRAME_CHECKSUM_OFFSET),
            0x0ae5_7a86_12ea_63a2
        );
        assert_eq!(
            epoch,
            DecodedFrame {
                kind: FrameKind::EpochAllocation,
                payload0: 1,
                payload1: 0,
                payload2: 0,
                payload2_bytes: 0_u64.to_be_bytes(),
            }
        );

        let commit_start = HEADER_LENGTH + FRAME_LENGTH;
        let mut commit_frame = [0_u8; FRAME_LENGTH];
        commit_frame.copy_from_slice(&bytes[commit_start..commit_start + FRAME_LENGTH]);
        let commit = parse_frame(
            &commit_frame,
            HEADER_LENGTH_U64 + FRAME_LENGTH_U64,
            LogFormat::V1,
        )?;
        assert_eq!(
            read_u64(&commit_frame, FRAME_CHECKSUM_OFFSET),
            0x6c26_89cf_2a11_e4c3
        );
        assert_eq!(
            commit,
            DecodedFrame {
                kind: FrameKind::CommitRecord,
                payload0: 1,
                payload1: transaction_id.epoch().get(),
                payload2: transaction_id.sequence(),
                payload2_bytes: transaction_id.sequence().to_be_bytes(),
            }
        );
        assert_eq!(committed.log_position().get(), 1);

        let marker_start = HEADER_LENGTH + FRAME_LENGTH * 2;
        let mut marker_frame = [0_u8; FRAME_LENGTH];
        marker_frame.copy_from_slice(&bytes[marker_start..marker_start + FRAME_LENGTH]);
        let marker = parse_frame(
            &marker_frame,
            HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 2,
            LogFormat::V1,
        )?;
        assert_eq!(
            read_u64(&marker_frame, FRAME_CHECKSUM_OFFSET),
            0xc85b_852c_c8ab_66b5
        );
        assert_eq!(
            marker,
            DecodedFrame {
                kind: FrameKind::DurableThrough,
                payload0: 1,
                payload1: 0,
                payload2: 0,
                payload2_bytes: 0_u64.to_be_bytes(),
            }
        );
        Ok(())
    }

    #[test]
    fn reopen_reconstructs_epoch_and_position_high_water() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("high-water")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(17)?)?;

        let mut first_coordinator = TransactionCoordinator::open(&mut log)?;
        let first = first_coordinator.begin()?;
        let first_id = first.transaction_id();
        let first_commit = first_coordinator.commit(first, &mut log)?;
        assert_eq!(first_commit.log_position().get(), 1);

        let mut second_coordinator = TransactionCoordinator::open(&mut log)?;
        log.arm_fault(FaultPoint::AfterAppend)?;
        let second = second_coordinator.begin()?;
        let second_id = second.transaction_id();
        let (_, second_cause) = indeterminate_parts(second_coordinator.commit(second, &mut log))?;
        assert_eq!(
            second_cause,
            CommitError::Append {
                source: FileCommitLogError::InjectedFault(FaultPoint::AfterAppend),
            }
        );
        assert_eq!(log.records().len(), 2);
        assert_eq!(log.durable_records().len(), 1);
        drop(log);

        let mut reopened = FileCommitLog::open(&path)?;
        assert_eq!(reopened.records().len(), 2);
        assert_eq!(reopened.durable_records().len(), 1);
        assert!(reopened.records()[0].matches_transaction_id(first_id));
        assert!(reopened.records()[1].matches_transaction_id(second_id));

        let mut third_coordinator = TransactionCoordinator::open(&mut reopened)?;
        assert_eq!(third_coordinator.epoch().get(), 3);
        let third = third_coordinator.begin()?;
        let third_commit = third_coordinator.commit(third, &mut reopened)?;
        assert_eq!(third_commit.log_position().get(), 3);
        Ok(())
    }

    #[test]
    fn old_coordinator_and_position_survive_close_and_open_same_lineage()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("old-coordinator")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(23)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;

        let first = coordinator.begin()?;
        let committed = coordinator.commit(first, &mut log)?;
        let position = committed.log_position().clone();
        drop(log);

        let mut reopened = FileCommitLog::open(&path)?;
        reopened.flush_through(&position)?;
        assert_eq!(position, reopened.lineage().position(1));

        let second = coordinator.begin()?;
        let second_commit = coordinator.commit(second, &mut reopened)?;
        assert_eq!(second_commit.log_position().get(), 2);
        Ok(())
    }

    #[test]
    fn before_append_fault_leaves_no_record_and_resolves_absent() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("before-append")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(29)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        log.arm_fault(FaultPoint::BeforeAppend)?;
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();

        let (indeterminate, cause) = indeterminate_parts(coordinator.commit(active, &mut log))?;
        assert_eq!(
            cause,
            CommitError::Append {
                source: FileCommitLogError::InjectedFault(FaultPoint::BeforeAppend),
            }
        );
        assert!(log.records().is_empty());
        assert_eq!(log.durable_records().len(), 0);

        let resolution = coordinator.resolve(indeterminate, &mut log)?;
        let TransactionCommitResolution::NoDurableCommitRecord(without_record) = resolution else {
            return Err(io::Error::other("missing record resolved as committed").into());
        };
        assert_eq!(without_record.transaction_id(), transaction_id);
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::NoDurableCommitRecord)
        );
        Ok(())
    }

    #[test]
    fn after_append_fault_keeps_unmarked_record_until_later_marker_then_resolves_committed()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("after-append")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(31)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        log.arm_fault(FaultPoint::AfterAppend)?;
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();

        let (indeterminate, cause) = indeterminate_parts(coordinator.commit(active, &mut log))?;
        assert_eq!(
            cause,
            CommitError::Append {
                source: FileCommitLogError::InjectedFault(FaultPoint::AfterAppend),
            }
        );
        let position = log.records()[0].position().clone();
        assert_eq!(log.durable_records().len(), 0);

        let error = coordinator
            .resolve(indeterminate, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("volatile record produced a terminal resolution"))?;
        assert_eq!(
            error.failure(),
            &TransactionResolutionFailure::Source(
                FileTransactionRecoveryError::VolatileCommitRecord(transaction_id)
            )
        );
        drop(log);

        let mut reopened = FileCommitLog::open(&path)?;
        assert_eq!(reopened.records().len(), 1);
        assert_eq!(reopened.durable_records().len(), 0);
        let error = coordinator
            .resolve(error.into_transaction(), &mut reopened)
            .err()
            .ok_or_else(|| {
                io::Error::other("reopened volatile record produced a terminal resolution")
            })?;
        assert_eq!(
            error.failure(),
            &TransactionResolutionFailure::Source(
                FileTransactionRecoveryError::VolatileCommitRecord(transaction_id)
            )
        );

        reopened.flush_through(&position)?;
        let resolution = coordinator.resolve(error.into_transaction(), &mut reopened)?;
        let TransactionCommitResolution::Committed(committed) = resolution else {
            return Err(io::Error::other("flushed record did not resolve as committed").into());
        };
        assert_eq!(committed.transaction_id(), transaction_id);
        assert_eq!(committed.log_position(), &position);
        drop(reopened);

        let reopened_again = FileCommitLog::open(&path)?;
        assert_eq!(reopened_again.durable_position(), Some(position));
        Ok(())
    }

    #[test]
    fn before_flush_fault_keeps_complete_unmarked_record_until_later_marker()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("before-flush")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(37)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        log.arm_fault(FaultPoint::BeforeFlush)?;
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();

        let (indeterminate, cause) = indeterminate_parts(coordinator.commit(active, &mut log))?;
        let position = match &cause {
            CommitError::Flush { position, .. } => position.clone(),
            _ => {
                return Err(
                    io::Error::other("before-flush fault did not report a flush error").into(),
                );
            }
        };
        assert_eq!(
            cause,
            CommitError::Flush {
                position: position.clone(),
                source: FileCommitLogError::InjectedFault(FaultPoint::BeforeFlush),
            }
        );
        assert_eq!(log.records().len(), 1);
        assert_eq!(log.durable_records().len(), 0);

        let error = coordinator
            .resolve(indeterminate, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("unmarked record produced a terminal resolution"))?;
        assert_eq!(
            error.failure(),
            &TransactionResolutionFailure::Source(
                FileTransactionRecoveryError::VolatileCommitRecord(transaction_id)
            )
        );
        drop(log);

        let mut reopened = FileCommitLog::open(&path)?;
        assert_eq!(reopened.durable_records().len(), 0);
        let error = coordinator
            .resolve(error.into_transaction(), &mut reopened)
            .err()
            .ok_or_else(|| {
                io::Error::other("reopened unmarked record produced a terminal resolution")
            })?;
        assert_eq!(
            error.failure(),
            &TransactionResolutionFailure::Source(
                FileTransactionRecoveryError::VolatileCommitRecord(transaction_id)
            )
        );

        reopened.flush_through(&position)?;
        let resolution = coordinator.resolve(error.into_transaction(), &mut reopened)?;
        let TransactionCommitResolution::Committed(committed) = resolution else {
            return Err(
                io::Error::other("marker-covered record did not resolve as committed").into(),
            );
        };
        assert_eq!(committed.transaction_id(), transaction_id);
        assert_eq!(committed.log_position(), &position);
        Ok(())
    }

    #[test]
    fn after_flush_fault_is_durable_and_resolves_committed_after_reopen()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("after-flush")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(41)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        log.arm_fault(FaultPoint::AfterFlush)?;
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();

        let (indeterminate, cause) = indeterminate_parts(coordinator.commit(active, &mut log))?;
        let position = match &cause {
            CommitError::Flush { position, .. } => position.clone(),
            _ => {
                return Err(
                    io::Error::other("after-flush fault did not report a flush error").into(),
                );
            }
        };
        assert_eq!(
            cause,
            CommitError::Flush {
                position: position.clone(),
                source: FileCommitLogError::InjectedFault(FaultPoint::AfterFlush),
            }
        );
        assert_eq!(log.durable_position(), Some(position.clone()));
        drop(log);

        let mut reopened = FileCommitLog::open(&path)?;
        let resolution = coordinator.resolve(indeterminate, &mut reopened)?;
        let TransactionCommitResolution::Committed(committed) = resolution else {
            return Err(io::Error::other("durable record did not resolve as committed").into());
        };
        assert_eq!(committed.transaction_id(), transaction_id);
        assert_eq!(committed.log_position(), &position);
        Ok(())
    }

    #[test]
    fn incomplete_tail_is_truncated_and_open_seeks_end_for_further_appends()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("tail-truncation")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(43)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let committed = coordinator.commit(active, &mut log)?;
        assert_eq!(committed.log_position().get(), 1);
        drop(log);

        let intact_len = fs::metadata(&path)?.len();
        append_bytes(&path, &[1, 2, 3, 4, 5])?;
        assert_eq!(fs::metadata(&path)?.len(), intact_len + 5);

        let mut reopened = FileCommitLog::open(&path)?;
        assert_eq!(fs::metadata(&path)?.len(), intact_len);
        assert_eq!(reopened.records().len(), 1);
        assert_eq!(reopened.durable_records().len(), 1);

        let mut second_coordinator = TransactionCoordinator::open(&mut reopened)?;
        let second = second_coordinator.begin()?;
        let second_commit = second_coordinator.commit(second, &mut reopened)?;
        assert_eq!(second_commit.log_position().get(), 2);
        Ok(())
    }

    #[test]
    fn checksum_corruption_fails_closed_without_truncating_tail() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("checksum-corruption")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(47)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        coordinator.commit(active, &mut log)?;
        drop(log);

        let intact_len = fs::metadata(&path)?.len();
        append_bytes(&path, &[9, 8, 7])?;
        let corrupt_offset = HEADER_LENGTH + FRAME_LENGTH + FRAME_CHECKSUM_OFFSET;
        flip_byte(&path, corrupt_offset)?;

        let bytes = fs::read(&path)?;
        let error = FileCommitLog::open(&path)
            .err()
            .ok_or_else(|| io::Error::other("corrupted checksum unexpectedly opened"))?;
        assert_eq!(
            error,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 + FRAME_CHECKSUM_OFFSET_U64,
                FileFormatErrorReason::FrameChecksum {
                    expected: checksum_v1(
                        &bytes[HEADER_LENGTH + FRAME_LENGTH
                            ..HEADER_LENGTH + FRAME_LENGTH + FRAME_CHECKSUM_OFFSET],
                    ),
                    actual: read_u64(&bytes, HEADER_LENGTH + FRAME_LENGTH + FRAME_CHECKSUM_OFFSET),
                },
            ))
        );
        assert_eq!(fs::metadata(&path)?.len(), intact_len + 3);
        Ok(())
    }

    #[test]
    fn unsupported_header_version_is_rejected() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("header-version")?;
        let path = directory.path().join("commit-log.bin");
        let log = FileCommitLog::create_new(&path, persistent_id(53)?)?;
        drop(log);

        let mut bytes = fs::read(&path)?;
        write_u16(&mut bytes, 8, 2);
        let checksum = checksum_v1(&bytes[..HEADER_CHECKSUM_OFFSET]);
        write_u64(&mut bytes, HEADER_CHECKSUM_OFFSET, checksum);
        write_exact_bytes(&path, 0, &bytes)?;

        let error = FileCommitLog::open(&path)
            .err()
            .ok_or_else(|| io::Error::other("unsupported header version unexpectedly opened"))?;
        assert_eq!(
            error,
            FileOpenError::Format(FileFormatError::new(
                8,
                FileFormatErrorReason::HeaderVersion { actual: 2 },
            ))
        );
        Ok(())
    }

    #[test]
    fn duplicate_transaction_identity_is_rejected_on_open() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("duplicate-open")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(59)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let transaction_id = active.transaction_id();
        coordinator.commit(active, &mut log)?;
        drop(log);

        let duplicate = build_frame(
            LogFormat::V1,
            FrameKind::CommitRecord,
            2,
            transaction_id.epoch().get(),
            transaction_id.sequence(),
        );
        append_bytes(&path, &duplicate)?;

        let error = FileCommitLog::open(&path)
            .err()
            .ok_or_else(|| io::Error::other("duplicate identity unexpectedly opened"))?;
        assert_eq!(
            error,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 3 + 24,
                FileFormatErrorReason::DuplicateTransactionIdentity {
                    epoch: transaction_id.epoch().get(),
                    sequence: transaction_id.sequence(),
                },
            ))
        );
        Ok(())
    }

    #[test]
    fn out_of_sequence_commit_position_is_rejected_on_open() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("position-open")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(61)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        coordinator.commit(active, &mut log)?;
        drop(log);

        let frame = build_frame(LogFormat::V1, FrameKind::CommitRecord, 3, 1, 2);
        append_bytes(&path, &frame)?;

        let error = FileCommitLog::open(&path)
            .err()
            .ok_or_else(|| io::Error::other("out-of-sequence position unexpectedly opened"))?;
        assert_eq!(
            error,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 3 + 16,
                FileFormatErrorReason::CommitPositionOutOfSequence {
                    expected: 2,
                    actual: 3,
                },
            ))
        );
        Ok(())
    }

    #[test]
    fn non_advancing_marker_is_rejected_on_open() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("marker-open")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(67)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        coordinator.commit(active, &mut log)?;
        drop(log);

        let frame = build_frame(LogFormat::V1, FrameKind::DurableThrough, 1, 0, 0);
        append_bytes(&path, &frame)?;

        let error = FileCommitLog::open(&path)
            .err()
            .ok_or_else(|| io::Error::other("non-advancing marker unexpectedly opened"))?;
        assert_eq!(
            error,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 3 + 16,
                FileFormatErrorReason::MarkerDoesNotAdvance {
                    previous: 1,
                    actual: 1,
                },
            ))
        );
        Ok(())
    }

    #[test]
    fn foreign_unknown_and_idempotent_flushes_preserve_faults() -> Result<(), Box<dyn Error>> {
        let owner_directory = TestDirectory::new("flush-owner")?;
        let owner_path = owner_directory.path().join("commit-log.bin");
        let mut owner_log = FileCommitLog::create_new(&owner_path, persistent_id(71)?)?;
        let mut owner = TransactionCoordinator::open(&mut owner_log)?;
        let owner_active = owner.begin()?;
        let owner_position = owner
            .commit(owner_active, &mut owner_log)?
            .log_position()
            .clone();

        let target_directory = TestDirectory::new("flush-target")?;
        let target_path = target_directory.path().join("commit-log.bin");
        let mut target_log = FileCommitLog::create_new(&target_path, persistent_id(73)?)?;
        let mut target = TransactionCoordinator::open(&mut target_log)?;
        let target_active = target.begin()?;
        let target_position = target
            .commit(target_active, &mut target_log)?
            .log_position()
            .clone();
        target_log.arm_fault(FaultPoint::BeforeFlush)?;

        let unknown_position = target_log.lineage().position(9);
        let unknown_error = target_log
            .flush_through(&unknown_position)
            .err()
            .ok_or_else(|| io::Error::other("unknown flush position unexpectedly accepted"))?;
        assert_eq!(
            unknown_error,
            FileCommitLogError::UnknownFlushPosition(unknown_position)
        );
        assert_eq!(target_log.armed_fault(), Some(FaultPoint::BeforeFlush));

        let foreign_error = target_log
            .flush_through(&owner_position)
            .err()
            .ok_or_else(|| io::Error::other("foreign flush position unexpectedly accepted"))?;
        assert_eq!(
            foreign_error,
            FileCommitLogError::ForeignFlushPosition(owner_position)
        );
        assert_eq!(target_log.armed_fault(), Some(FaultPoint::BeforeFlush));

        target_log.flush_through(&target_position)?;
        assert_eq!(target_log.armed_fault(), Some(FaultPoint::BeforeFlush));

        let second = target.begin()?;
        let cause = commit_cause(target.commit(second, &mut target_log))?;
        assert_eq!(
            cause,
            CommitError::Flush {
                position: target_log.records()[1].position().clone(),
                source: FileCommitLogError::InjectedFault(FaultPoint::BeforeFlush),
            }
        );
        Ok(())
    }

    #[test]
    fn arming_a_fault_never_silently_replaces_one() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("armed-fault")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(79)?)?;
        log.arm_fault(FaultPoint::AfterFlush)?;

        let error = log
            .arm_fault(FaultPoint::BeforeAppend)
            .err()
            .ok_or_else(|| io::Error::other("armed fault was silently replaced"))?;
        assert_eq!(error.armed(), FaultPoint::AfterFlush);
        assert_eq!(error.requested(), FaultPoint::BeforeAppend);
        assert_eq!(log.armed_fault(), Some(FaultPoint::AfterFlush));
        Ok(())
    }

    #[test]
    fn poisoned_writer_requires_reopen_for_epoch_commit_flush_and_recovery()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("poisoned")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(83)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let first = coordinator.begin()?;
        let committed = coordinator.commit(first, &mut log)?;
        let durable_position = committed.log_position().clone();

        let second = coordinator.begin()?;
        let transaction_id = second.transaction_id();
        log.poisoned = true;

        let (indeterminate, cause) = indeterminate_parts(coordinator.commit(second, &mut log))?;
        assert_eq!(
            cause,
            CommitError::Append {
                source: FileCommitLogError::PoisonedWriter,
            }
        );
        assert_eq!(
            log.flush_through(&durable_position).err(),
            Some(FileCommitLogError::PoisonedWriter)
        );
        assert_eq!(
            TransactionCoordinator::open(&mut log).err(),
            Some(FileTransactionEpochError::PoisonedWriter)
        );

        let error = coordinator
            .resolve(indeterminate, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("poisoned recovery unexpectedly resolved"))?;
        assert_eq!(
            error.failure(),
            &TransactionResolutionFailure::Source(FileTransactionRecoveryError::PoisonedWriter)
        );
        assert_eq!(
            coordinator.status(transaction_id),
            Some(TransactionLifecycleStatus::Indeterminate)
        );
        Ok(())
    }

    #[test]
    fn committed_page_recovery_rejects_poisoned_source_observation_and_write()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("committed-recovery-poison")?;
        let log_path = directory.path().join("commit-log.bin");
        let observed_store_path = directory.path().join("observed-pages.bin");
        let compared_store_path = directory.path().join("compared-pages.bin");
        let persistent_id = persistent_id(501)?;
        let page_number = page_number(41)?;
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let page = unlogged_page(log.lineage(), 41, 1, [1, 2])?;
        let (active, _dirty) = coordinator.stage_page_write(active, page, &mut log)?;
        coordinator.commit(active, &mut log)?;

        log.poisoned = true;
        assert_eq!(
            <FileCommitLog<2> as ntsql_transaction::DurableTransactionPageRecoveryInventory<
                2,
            >>::durable_transaction_page_numbers(&mut log),
            Err(FileCommittedPageRecoveryInventoryError::PoisonedWriter)
        );
        let mut callback_called = false;
        let source =
            <FileCommitLog<2> as ntsql_transaction::DurableTransactionPageRecoverySource<
                2,
            >>::with_durable_page_evidence(&mut log, page_number, |_, _, _| {
                callback_called = true;
            });
        assert_eq!(
            source,
            Err(FileCommittedPageRecoverySourceError::PoisonedWriter)
        );
        assert!(!callback_called);
        log.poisoned = false;

        let mut observed_store =
            FilePageStore::<2>::create_new(&observed_store_path, persistent_id)?;
        observed_store.arm_fault(PageStoreFaultPoint::BeforeWrite)?;
        observed_store.poisoned = true;
        let observation =
            recover_committed_transaction_page(&mut log, &mut observed_store, page_number);
        assert!(matches!(
            observation,
            Err(CommittedTransactionPageRecoveryError::StoreObservation(
                FileCommittedPageRecoveryObservationError::PoisonedWriter
            ))
        ));
        assert_eq!(
            observed_store.armed_fault(),
            Some(PageStoreFaultPoint::BeforeWrite)
        );
        assert!(observed_store.pages().is_empty());

        let mut compared_store = PoisonBeforeRecoveryCompare(FilePageStore::<2>::create_new(
            &compared_store_path,
            persistent_id,
        )?);
        compared_store
            .0
            .arm_fault(PageStoreFaultPoint::BeforeWrite)?;
        let comparison =
            recover_committed_transaction_page(&mut log, &mut compared_store, page_number);
        let Err(CommittedTransactionPageRecoveryError::StoreWrite { state }) = comparison else {
            return Err(io::Error::other("poisoned compare was not terminal").into());
        };
        assert_eq!(
            state.as_ref().cause(),
            &FileCommittedPageRecoveryStoreError::PageStore(FilePageStoreError::PoisonedWriter)
        );
        assert_eq!(
            compared_store.0.armed_fault(),
            Some(PageStoreFaultPoint::BeforeWrite)
        );
        assert!(compared_store.0.pages().is_empty());
        assert!(compared_store.0.is_poisoned());
        Ok(())
    }

    #[test]
    fn committed_page_recovery_errors_retain_projection_causes() -> Result<(), Box<dyn Error>> {
        let lineage = LogLineage::new();
        let page_number = page_number(43)?;

        let physical = DurablePageWalObservation::<0>::from_bytes(
            page_number,
            PageVersion::new(1),
            [],
            lineage.position(1),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width physical page projected"))?;
        let expected_physical = DurablePageWalObservation::<0>::from_bytes(
            page_number,
            PageVersion::new(1),
            [],
            lineage.position(1),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width physical page projected twice"))?;
        let error =
            FileCommittedPageRecoverySourceError::PhysicalPageProjection(Box::new(physical));
        assert!(Error::source(&error).is_some());
        let FileCommittedPageRecoverySourceError::PhysicalPageProjection(source) = error else {
            return Err(io::Error::other("physical projection cause changed variant").into());
        };
        assert_eq!(*source, expected_physical);

        let owned = DurableTransactionPageObservation::<0>::from_bytes(
            1,
            1,
            page_number,
            PageVersion::new(2),
            [],
            lineage.position(2),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width owned page projected"))?;
        let expected_owned = DurableTransactionPageObservation::<0>::from_bytes(
            1,
            1,
            page_number,
            PageVersion::new(2),
            [],
            lineage.position(2),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width owned page projected twice"))?;
        let error =
            FileCommittedPageRecoverySourceError::TransactionPageProjection(Box::new(owned));
        assert!(Error::source(&error).is_some());
        let FileCommittedPageRecoverySourceError::TransactionPageProjection(source) = error else {
            return Err(io::Error::other("owned projection cause changed variant").into());
        };
        assert_eq!(*source, expected_owned);

        let commit = DurableTransactionCommitObservation::from_fields(0, 1, lineage.position(3))
            .err()
            .ok_or_else(|| io::Error::other("zero-epoch commit projected"))?;
        let expected_commit = commit.clone();
        let error = FileCommittedPageRecoverySourceError::<1>::CommitProjection(Box::new(commit));
        assert!(Error::source(&error).is_some());
        let FileCommittedPageRecoverySourceError::CommitProjection(source) = error else {
            return Err(io::Error::other("commit projection cause changed variant").into());
        };
        assert_eq!(*source, expected_commit);

        let observation_source = DurablePageWalObservation::<0>::from_bytes(
            page_number,
            PageVersion::new(1),
            [],
            lineage.position(1),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width snapshot projected"))?;
        let observation =
            FileCommittedPageRecoveryObservationError::Projection(Box::new(observation_source));
        assert!(Error::source(&observation).is_some());
        let capacity = FileCommittedPageRecoverySourceError::<1>::EvidenceCapacityExhausted {
            projection: FilePageRecoveryProjection::Commits,
        };
        assert!(Error::source(&capacity).is_none());
        Ok(())
    }

    #[test]
    fn restart_analysis_source_errors_are_typed_and_callback_free() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("restart-analysis-source-errors")?;
        let capacity_path = directory.path().join("capacity.bin");
        let malformed_path = directory.path().join("malformed.bin");

        let mut capacity_log = FileCommitLog::<1>::create_new_transaction_page_capable(
            &capacity_path,
            persistent_id(503)?,
        )?;
        capacity_log.poisoned = true;
        let mut poison_callback = false;
        let poison =
            <FileCommitLog<1> as ntsql_transaction::DurableTransactionRestartAnalysisSource<1>>::with_durable_transaction_restart_observations(
                &mut capacity_log,
                |_frontier, _observations| poison_callback = true,
            );
        assert_eq!(
            poison,
            Err(FileTransactionRestartAnalysisSourceError::PoisonedWriter)
        );
        assert!(!poison_callback);

        capacity_log.poisoned = false;
        capacity_log.durable_len = usize::MAX;
        let mut capacity_callback = false;
        let capacity =
            <FileCommitLog<1> as ntsql_transaction::DurableTransactionRestartAnalysisSource<1>>::with_durable_transaction_restart_observations(
                &mut capacity_log,
                |_frontier, _observations| capacity_callback = true,
            )
            .err()
            .ok_or_else(|| io::Error::other("impossible restart capacity was reserved"))?;
        assert!(!capacity_callback);
        assert!(Error::source(&capacity).is_none());
        assert_eq!(
            capacity,
            FileTransactionRestartAnalysisSourceError::ObservationCapacityExhausted {
                record_count: usize::MAX,
            }
        );

        let mut malformed = FileCommitLog::create_new(&malformed_path, persistent_id(504)?)?;
        let lineage = malformed.lineage.clone();
        let page_number = page_number(44)?;

        let raw_position = lineage.position(1);
        let expected_raw = DurablePageWalObservation::<0>::from_bytes(
            page_number,
            PageVersion::new(1),
            [],
            raw_position.clone(),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width raw page projected"))?;
        malformed.records.push(FileLogRecord {
            position: raw_position,
            kind: FileLogRecordKind::PageWrite(FilePageWriteRecord {
                page_number,
                page_version: PageVersion::new(1),
                bytes: [],
            }),
        });
        malformed.durable_len = 1;
        let mut raw_callback = false;
        let raw_error =
            <FileCommitLog<0> as ntsql_transaction::DurableTransactionRestartAnalysisSource<0>>::with_durable_transaction_restart_observations(
                &mut malformed,
                |_frontier, _observations| raw_callback = true,
            )
            .err()
            .ok_or_else(|| io::Error::other("malformed raw page entered restart analysis"))?;
        assert!(!raw_callback);
        assert!(Error::source(&raw_error).is_some());
        let FileTransactionRestartAnalysisSourceError::PageProjection(source) = raw_error else {
            return Err(io::Error::other("raw restart cause changed variant").into());
        };
        assert_eq!(*source, expected_raw);

        let transaction_position = lineage.position(2);
        let expected_transaction = DurableTransactionPageObservation::<0>::from_bytes(
            1,
            1,
            page_number,
            PageVersion::new(2),
            [],
            transaction_position.clone(),
        )
        .err()
        .ok_or_else(|| io::Error::other("zero-width transaction page projected"))?;
        malformed.records.clear();
        malformed.records.push(FileLogRecord {
            position: transaction_position,
            kind: FileLogRecordKind::TransactionPageWrite(FileTransactionPageWriteRecord {
                transaction_epoch: 1,
                transaction_sequence: 1,
                page: FilePageWriteRecord {
                    page_number,
                    page_version: PageVersion::new(2),
                    bytes: [],
                },
            }),
        });
        let mut transaction_callback = false;
        let transaction_error =
            <FileCommitLog<0> as ntsql_transaction::DurableTransactionRestartAnalysisSource<0>>::with_durable_transaction_restart_observations(
                &mut malformed,
                |_frontier, _observations| transaction_callback = true,
            )
            .err()
            .ok_or_else(|| {
                io::Error::other("malformed transaction page entered restart analysis")
            })?;
        assert!(!transaction_callback);
        assert!(Error::source(&transaction_error).is_some());
        let FileTransactionRestartAnalysisSourceError::TransactionPageProjection(source) =
            transaction_error
        else {
            return Err(io::Error::other("transaction restart cause changed variant").into());
        };
        assert_eq!(*source, expected_transaction);

        let zero_position = lineage.position(0);
        let expected_commit =
            DurableTransactionCommitObservation::from_fields(1, 1, zero_position.clone())
                .err()
                .ok_or_else(|| io::Error::other("zero-position commit projected"))?;
        malformed.records.clear();
        malformed.records.push(FileLogRecord {
            position: zero_position,
            kind: FileLogRecordKind::TransactionCommit {
                transaction_epoch: 1,
                transaction_sequence: 1,
            },
        });
        let mut commit_callback = false;
        let commit_error =
            <FileCommitLog<0> as ntsql_transaction::DurableTransactionRestartAnalysisSource<0>>::with_durable_transaction_restart_observations(
                &mut malformed,
                |_frontier, _observations| commit_callback = true,
            )
            .err()
            .ok_or_else(|| io::Error::other("malformed commit entered restart analysis"))?;
        assert!(!commit_callback);
        assert!(Error::source(&commit_error).is_some());
        let FileTransactionRestartAnalysisSourceError::CommitProjection(source) = commit_error
        else {
            return Err(io::Error::other("commit restart cause changed variant").into());
        };
        assert_eq!(*source, expected_commit);
        Ok(())
    }

    #[test]
    fn committed_page_recovery_recheck_rejects_target_that_appeared() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new("committed-recovery-recheck")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let persistent_id = persistent_id(502)?;
        let page_number = page_number(42)?;
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let page = unlogged_page(log.lineage(), 42, 3, [3, 4])?;
        let (active, _dirty) = coordinator.stage_page_write(active, page, &mut log)?;
        coordinator.commit(active, &mut log)?;
        let mut store = TargetAppearsBeforeRecoveryCompare(FilePageStore::<2>::create_new(
            &store_path,
            persistent_id,
        )?);

        let result = recover_committed_transaction_page(&mut log, &mut store, page_number);
        let Err(CommittedTransactionPageRecoveryError::StoreWrite { state }) = result else {
            return Err(io::Error::other("changed recovery source was not terminal").into());
        };
        assert_eq!(
            state.as_ref().cause(),
            &FileCommittedPageRecoveryStoreError::SourceNotMatched {
                actual: DurableCommittedTransactionPageRecoveryComparison::TargetAlreadyPresent,
            }
        );
        let stored = store
            .0
            .page(page_number)
            .ok_or_else(|| io::Error::other("intervening durable target is missing"))?;
        assert_eq!(stored.page_version(), PageVersion::new(3));
        assert_eq!(stored.bytes(), &[3, 4]);
        assert_eq!(stored.required_position().get(), 1);
        assert_eq!(stored.store_sequence(), 1);
        assert_eq!(
            store.0.armed_fault(),
            Some(PageStoreFaultPoint::BeforeWrite)
        );

        let rerun = recover_committed_transaction_page(&mut log, &mut store, page_number)?;
        assert!(matches!(
            rerun,
            CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
        ));
        assert_eq!(
            store.0.armed_fault(),
            Some(PageStoreFaultPoint::BeforeWrite)
        );
        Ok(())
    }

    #[test]
    fn epoch_and_position_exhaustion_are_typed() -> Result<(), Box<dyn Error>> {
        let epoch_directory = TestDirectory::new("epoch-exhaustion")?;
        let epoch_path = epoch_directory.path().join("commit-log.bin");
        let mut epoch_log = FileCommitLog::create_new(&epoch_path, persistent_id(89)?)?;
        epoch_log.next_epoch = None;
        assert_eq!(
            TransactionCoordinator::open(&mut epoch_log).err(),
            Some(FileTransactionEpochError::EpochSpaceExhausted)
        );

        let position_directory = TestDirectory::new("position-exhaustion")?;
        let position_path = position_directory.path().join("commit-log.bin");
        let mut position_log = FileCommitLog::create_new(&position_path, persistent_id(97)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut position_log)?;
        position_log.next_position = None;
        let active = coordinator.begin()?;
        let cause = commit_cause(coordinator.commit(active, &mut position_log))?;
        assert_eq!(
            cause,
            CommitError::Append {
                source: FileCommitLogError::PositionSpaceExhausted,
            }
        );
        Ok(())
    }

    #[test]
    fn restart_epoch_high_water_is_checked_and_persisted_across_file_reopen()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("restart-epoch-high-water")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::create_new(&path, persistent_id(167)?)?;
        for expected in 1_u64..=4 {
            let coordinator = TransactionCoordinator::open(&mut log)?;
            assert_eq!(coordinator.epoch().get(), expected);
        }
        let high_water = NonZeroU64::new(4)
            .ok_or_else(|| io::Error::other("restart epoch high-water is zero"))?;
        let (first, _) = log.allocate_restart_transaction_epoch(Some(high_water))?;
        assert_eq!(first.get(), 5);
        drop(log);

        let mut reopened = FileCommitLog::open(&path)?;
        let (second, _) = reopened.allocate_restart_transaction_epoch(Some(first))?;
        assert_eq!(second.get(), 6);
        let bytes_before_rejection = fs::read(&path)?;
        reopened.next_epoch = Some(second);
        assert!(matches!(
            reopened.allocate_restart_transaction_epoch(Some(second)),
            Err(
                TransactionRestartCoordinatorEpochAllocationError::PersistedEpochHighWaterNotAdvanced {
                    persisted_epoch_high_water: 6,
                    next_epoch: 6,
                }
            )
        ));
        assert_eq!(fs::read(&path)?, bytes_before_rejection);

        reopened.next_epoch = None;
        assert!(matches!(
            reopened.allocate_restart_transaction_epoch(Some(NonZeroU64::MAX)),
            Err(
                TransactionRestartCoordinatorEpochAllocationError::IdentitySpaceExhausted {
                    persisted_epoch_high_water: Some(u64::MAX),
                }
            )
        ));
        assert_eq!(fs::read(&path)?, bytes_before_rejection);
        Ok(())
    }

    #[test]
    fn v1_page_append_is_rejected_before_fault_consumption_or_position_change()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v1-page-rejection")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::<0>::create_new(&path, persistent_id(101)?)?;
        log.arm_fault(FaultPoint::AfterAppend)?;
        let page = unlogged_page(log.lineage(), 5, 7, [1_u8])?;

        let error = PageLog::<1>::append_page(&mut log, &page)
            .err()
            .ok_or_else(|| io::Error::other("v1 log unexpectedly accepted a page append"))?;
        assert_eq!(error, FileCommitLogError::PageSupportUnavailable);
        assert!(log.records().is_empty());
        assert_eq!(log.durable_position(), None);
        assert_eq!(log.armed_fault(), Some(FaultPoint::AfterAppend));
        Ok(())
    }

    #[test]
    fn v2_create_open_and_bytes_cover_header_page_frames_and_marker() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new("v2-format-bytes")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::<10>::create_new_page_capable(&path, persistent_id(103)?)?;
        let page = unlogged_page(log.lineage(), 7, 9, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10])?;
        let dirty = stage_page_write(&mut log, page)?;
        log.flush_through(dirty.required_position())?;
        drop(log);

        let bytes = fs::read(&path)?;
        assert_eq!(bytes.len(), HEADER_LENGTH + FRAME_LENGTH * 4);
        let mut header = [0_u8; HEADER_LENGTH];
        header.copy_from_slice(&bytes[..HEADER_LENGTH]);
        assert_eq!(
            parse_header(
                &header,
                HeaderExpectation::V2(PageLayout::for_const::<10>()?)
            )?,
            persistent_id(103)?
        );
        assert_eq!(read_u16(&header, 8), FORMAT_VERSION_V2);
        assert_eq!(read_u64(&header, HEADER_V2_PAGE_WIDTH_OFFSET), 10);
        assert_eq!(
            read_u64(&header, HEADER_CHECKSUM_OFFSET),
            0xfc7c_1b9c_d9ce_d65c
        );

        let mut page_header_frame = [0_u8; FRAME_LENGTH];
        page_header_frame.copy_from_slice(&bytes[HEADER_LENGTH..HEADER_LENGTH + FRAME_LENGTH]);
        let page_header = parse_frame(&page_header_frame, HEADER_LENGTH_U64, LogFormat::V2)?;
        assert_eq!(
            page_header,
            DecodedFrame {
                kind: FrameKind::PageHeader,
                payload0: 1,
                payload1: 7,
                payload2: 9,
                payload2_bytes: 9_u64.to_be_bytes(),
            }
        );
        assert_eq!(
            read_u64(&page_header_frame, FRAME_CHECKSUM_OFFSET),
            0x5b37_7e96_b0b2_e7d5
        );

        let mut first_data_frame = [0_u8; FRAME_LENGTH];
        let first_offset = HEADER_LENGTH + FRAME_LENGTH;
        first_data_frame.copy_from_slice(&bytes[first_offset..first_offset + FRAME_LENGTH]);
        let first_data = parse_frame(
            &first_data_frame,
            HEADER_LENGTH_U64 + FRAME_LENGTH_U64,
            LogFormat::V2,
        )?;
        assert_eq!(first_data.kind, FrameKind::PageData);
        assert_eq!(first_data.payload0, 1);
        assert_eq!(first_data.payload1, 0);
        assert_eq!(first_data.payload2_bytes, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            read_u64(&first_data_frame, FRAME_CHECKSUM_OFFSET),
            0x0b03_c0c4_3035_cc7e
        );

        let mut final_data_frame = [0_u8; FRAME_LENGTH];
        let final_offset = HEADER_LENGTH + FRAME_LENGTH * 2;
        final_data_frame.copy_from_slice(&bytes[final_offset..final_offset + FRAME_LENGTH]);
        let final_data = parse_frame(
            &final_data_frame,
            HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 2,
            LogFormat::V2,
        )?;
        assert_eq!(final_data.kind, FrameKind::PageData);
        assert_eq!(final_data.payload0, 1);
        assert_eq!(final_data.payload1, 1);
        assert_eq!(final_data.payload2_bytes, [9, 10, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            read_u64(&final_data_frame, FRAME_CHECKSUM_OFFSET),
            0x10e7_f1c2_8c70_6051
        );

        let mut marker_frame = [0_u8; FRAME_LENGTH];
        let marker_offset = HEADER_LENGTH + FRAME_LENGTH * 3;
        marker_frame.copy_from_slice(&bytes[marker_offset..marker_offset + FRAME_LENGTH]);
        let marker = parse_frame(
            &marker_frame,
            HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 3,
            LogFormat::V2,
        )?;
        assert_eq!(marker.kind, FrameKind::DurableThrough);
        assert_eq!(marker.payload0, 1);
        assert_eq!(
            read_u64(&marker_frame, FRAME_CHECKSUM_OFFSET),
            0x94e8_67e0_753c_4fa4
        );

        let reopened = FileCommitLog::<10>::open_page_capable(&path)?;
        assert_eq!(reopened.records().len(), 1);
        let page_record = reopened.records()[0]
            .page_write()
            .ok_or_else(|| io::Error::other("reopened v2 page record missing"))?;
        assert_eq!(page_record.page_number().get(), 7);
        assert_eq!(page_record.page_version().get(), 9);
        assert_eq!(page_record.bytes(), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        Ok(())
    }

    #[test]
    fn mixed_commit_page_commit_share_ordering_durable_prefix_and_reopen()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("mixed-records")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::<4>::create_new_page_capable(&path, persistent_id(107)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let first = coordinator.begin()?;
        let first_id = first.transaction_id();
        let first_commit = coordinator.commit(first, &mut log)?;
        let page = unlogged_page(log.lineage(), 3, 4, [7, 8, 9, 10])?;
        let dirty = stage_page_write(&mut log, page)?;
        let second = coordinator.begin()?;
        let second_id = second.transaction_id();
        let second_commit = coordinator.commit(second, &mut log)?;

        assert_eq!(first_commit.log_position().get(), 1);
        assert_eq!(dirty.required_position().get(), 2);
        assert_eq!(second_commit.log_position().get(), 3);
        assert_eq!(log.records().len(), 3);
        assert_eq!(log.durable_records().len(), 3);
        assert_eq!(log.durable_position(), Some(log.lineage().position(3)));
        assert!(log.records()[0].matches_transaction_id(first_id));
        assert!(log.records()[2].matches_transaction_id(second_id));
        let page_record = log.records()[1]
            .page_write()
            .ok_or_else(|| io::Error::other("page record missing from mixed log"))?;
        assert_eq!(page_record.page_number().get(), 3);
        assert_eq!(page_record.page_version().get(), 4);
        assert_eq!(page_record.bytes(), &[7, 8, 9, 10]);
        drop(log);

        let mut reopened = FileCommitLog::<4>::open_page_capable(&path)?;
        assert_eq!(reopened.records().len(), 3);
        assert_eq!(reopened.durable_records().len(), 3);
        assert_eq!(
            reopened.durable_position(),
            Some(reopened.lineage().position(3))
        );
        reopened.flush_through(&reopened.lineage().position(3))?;
        assert_eq!(reopened.durable_records().len(), 3);
        let third = coordinator.begin()?;
        let third_commit = coordinator.commit(third, &mut reopened)?;
        assert_eq!(third_commit.log_position().get(), 4);
        assert_eq!(reopened.records().len(), 4);
        assert_eq!(reopened.durable_records().len(), 4);
        Ok(())
    }

    #[test]
    fn transaction_recovery_ignores_page_records() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("recovery-ignores-pages")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::<2>::create_new_page_capable(&path, persistent_id(109)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let first = coordinator.begin()?;
        let durable_id = first.transaction_id();
        let durable_commit = coordinator.commit(first, &mut log)?;
        let page = unlogged_page(log.lineage(), 1, 1, [4, 5])?;
        let _dirty = stage_page_write(&mut log, page)?;
        let second = coordinator.begin()?;
        let volatile_id = second.transaction_id();
        log.arm_fault(FaultPoint::AfterAppend)?;
        let (indeterminate, _) = indeterminate_parts(coordinator.commit(second, &mut log))?;
        drop(log);

        let mut reopened = FileCommitLog::<2>::open_page_capable(&path)?;
        let (_, durable_lookup) = reopened.lookup_durable_commit(durable_id)?;
        assert_eq!(
            durable_lookup,
            DurableCommitLookup::Found {
                position: reopened
                    .lineage()
                    .position(durable_commit.log_position().get())
            }
        );
        let error = reopened
            .lookup_durable_commit(volatile_id)
            .err()
            .ok_or_else(|| {
                io::Error::other("volatile commit unexpectedly resolved after page interleaving")
            })?;
        assert_eq!(
            error,
            FileTransactionRecoveryError::VolatileCommitRecord(volatile_id)
        );
        let resolution_error = coordinator
            .resolve(indeterminate, &mut reopened)
            .err()
            .ok_or_else(|| io::Error::other("volatile record unexpectedly resolved"))?;
        assert_eq!(
            resolution_error.failure(),
            &TransactionResolutionFailure::Source(
                FileTransactionRecoveryError::VolatileCommitRecord(volatile_id)
            )
        );
        Ok(())
    }

    #[test]
    fn page_append_and_flush_faults_match_commit_semantics() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("page-faults")?;
        let path = directory.path().join("commit-log.bin");
        let mut before_log =
            FileCommitLog::<2>::create_new_page_capable(&path, persistent_id(113)?)?;
        before_log.arm_fault(FaultPoint::BeforeAppend)?;
        let before_page = unlogged_page(before_log.lineage(), 21, 2, [1, 2])?;
        let before_error = stage_page_write(&mut before_log, before_page)
            .err()
            .ok_or_else(|| io::Error::other("before-append page fault unexpectedly succeeded"))?;
        let StagePageWriteError::Append(before_error) = before_error else {
            return Err(io::Error::other("before-append fault returned wrong error shape").into());
        };
        assert_eq!(
            before_error.cause(),
            &FileCommitLogError::InjectedFault(FaultPoint::BeforeAppend)
        );
        assert!(before_error.page().observed_position().is_none());
        assert!(before_log.records().is_empty());

        let path = directory.path().join("commit-log-after.bin");
        let mut after_log =
            FileCommitLog::<2>::create_new_page_capable(&path, persistent_id(127)?)?;
        after_log.arm_fault(FaultPoint::AfterAppend)?;
        let after_page = unlogged_page(after_log.lineage(), 22, 3, [3, 4])?;
        let after_error = stage_page_write(&mut after_log, after_page)
            .err()
            .ok_or_else(|| io::Error::other("after-append page fault unexpectedly succeeded"))?;
        let StagePageWriteError::Append(after_error) = after_error else {
            return Err(io::Error::other("after-append fault returned wrong error shape").into());
        };
        assert_eq!(
            after_error.cause(),
            &FileCommitLogError::InjectedFault(FaultPoint::AfterAppend)
        );
        assert!(after_error.page().observed_position().is_none());
        assert_eq!(after_log.records().len(), 1);
        assert_eq!(after_log.records()[0].position().get(), 1);

        let path = directory.path().join("commit-log-flush.bin");
        let mut flush_log =
            FileCommitLog::<2>::create_new_page_capable(&path, persistent_id(131)?)?;
        let flush_page = unlogged_page(flush_log.lineage(), 23, 4, [5, 6])?;
        let dirty = stage_page_write(&mut flush_log, flush_page)?;
        flush_log.arm_fault(FaultPoint::BeforeFlush)?;
        let flush_error = flush_log
            .flush_through(dirty.required_position())
            .err()
            .ok_or_else(|| io::Error::other("before-flush page error unexpectedly succeeded"))?;
        assert_eq!(
            flush_error,
            FileCommitLogError::InjectedFault(FaultPoint::BeforeFlush)
        );
        assert_eq!(flush_log.durable_records().len(), 0);

        flush_log.flush_through(dirty.required_position())?;
        flush_log.arm_fault(FaultPoint::AfterFlush)?;
        let second_page = unlogged_page(flush_log.lineage(), 24, 5, [7, 8])?;
        let second_dirty = stage_page_write(&mut flush_log, second_page)?;
        let after_flush_error = flush_log
            .flush_through(second_dirty.required_position())
            .err()
            .ok_or_else(|| io::Error::other("after-flush page error unexpectedly succeeded"))?;
        assert_eq!(
            after_flush_error,
            FileCommitLogError::InjectedFault(FaultPoint::AfterFlush)
        );
        assert_eq!(
            flush_log.durable_position(),
            Some(flush_log.lineage().position(2))
        );
        Ok(())
    }

    #[test]
    fn page_log_persists_reopen_and_discards_partial_group() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("page-reopen")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::<9>::create_new_page_capable(&path, persistent_id(137)?)?;
        let durable_page = unlogged_page(log.lineage(), 1, 1, [1, 2, 3, 4, 5, 6, 7, 8, 9])?;
        let durable_dirty = stage_page_write(&mut log, durable_page)?;
        log.flush_through(durable_dirty.required_position())?;
        log.arm_fault(FaultPoint::AfterAppend)?;
        let volatile_page = unlogged_page(log.lineage(), 2, 2, [9, 8, 7, 6, 5, 4, 3, 2, 1])?;
        let volatile_error = stage_page_write(&mut log, volatile_page)
            .err()
            .ok_or_else(|| io::Error::other("after-append page fault unexpectedly succeeded"))?;
        let StagePageWriteError::Append(error) = volatile_error else {
            return Err(
                io::Error::other("after-append page fault returned wrong error shape").into(),
            );
        };
        assert_eq!(
            error.cause(),
            &FileCommitLogError::InjectedFault(FaultPoint::AfterAppend)
        );
        assert_eq!(log.records().len(), 2);
        drop(log);

        let reopened = FileCommitLog::<9>::open_page_capable(&path)?;
        assert_eq!(reopened.records().len(), 2);
        assert_eq!(reopened.durable_records().len(), 1);
        let record = reopened
            .records()
            .first()
            .ok_or_else(|| io::Error::other("durable page record disappeared after reopen"))?;
        assert_eq!(record.position(), &reopened.lineage().position(1));
        let page_record = record
            .page_write()
            .ok_or_else(|| io::Error::other("reopened record lost page payload"))?;
        assert_eq!(page_record.page_number().get(), 1);
        assert_eq!(page_record.page_version().get(), 1);
        assert_eq!(page_record.bytes(), &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            reopened.records()[1].position(),
            &reopened.lineage().position(2)
        );
        Ok(())
    }

    #[test]
    fn page_capable_open_rejects_zero_and_width_mismatch() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("page-width-errors")?;
        let path = directory.path().join("commit-log.bin");
        let log = FileCommitLog::<4>::create_new_page_capable(&path, persistent_id(139)?)?;
        drop(log);
        append_bytes(&path, &[1, 2, 3])?;
        let mismatched_len = fs::metadata(&path)?.len();

        assert_eq!(
            FileCommitLog::<0>::create_new_page_capable(
                directory.path().join("zero.bin"),
                persistent_id(141)?
            )
            .err(),
            Some(FileCreateError::PageWidth(FilePageWidthError::Zero))
        );
        assert_eq!(
            FileCommitLog::<0>::open_page_capable(&path).err(),
            Some(FileOpenError::PageWidth(FilePageWidthError::Zero))
        );

        let mismatch = FileCommitLog::<8>::open_page_capable(&path)
            .err()
            .ok_or_else(|| io::Error::other("mismatched page width unexpectedly opened"))?;
        assert_eq!(
            mismatch,
            FileOpenError::Format(FileFormatError::new(
                HEADER_V2_PAGE_WIDTH_OFFSET as u64,
                FileFormatErrorReason::HeaderPageWidthMismatch {
                    expected: 8,
                    actual: 4,
                },
            ))
        );
        assert_eq!(fs::metadata(&path)?.len(), mismatched_len);

        let mut zero_width_bytes = fs::read(&path)?;
        write_u64(&mut zero_width_bytes, HEADER_V2_PAGE_WIDTH_OFFSET, 0);
        let checksum = checksum_v1(&zero_width_bytes[..HEADER_CHECKSUM_OFFSET]);
        write_u64(&mut zero_width_bytes, HEADER_CHECKSUM_OFFSET, checksum);
        let zero_path = directory.path().join("zero-width.bin");
        fs::write(&zero_path, zero_width_bytes)?;
        let zero_open = FileCommitLog::<4>::open_page_capable(&zero_path)
            .err()
            .ok_or_else(|| io::Error::other("zero-width v2 header unexpectedly opened"))?;
        assert_eq!(
            zero_open,
            FileOpenError::Format(FileFormatError::new(
                HEADER_V2_PAGE_WIDTH_OFFSET as u64,
                FileFormatErrorReason::HeaderPageWidthZero,
            ))
        );
        Ok(())
    }

    #[test]
    fn incomplete_physical_frame_and_partial_logical_groups_are_repaired()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("page-tail-repair")?;
        let layout = PageLayout::for_const::<10>()?;

        for complete_chunks in [0_usize, 1_usize] {
            let path = directory
                .path()
                .join(format!("partial-{complete_chunks}.bin"));
            let mut log = FileCommitLog::<10>::create_new_page_capable(
                &path,
                persistent_id(151 + u128::from(complete_chunks as u64))?,
            )?;
            let prefix_page = unlogged_page(log.lineage(), 1, 1, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9])?;
            let prefix_dirty = stage_page_write(&mut log, prefix_page)?;
            log.flush_through(prefix_dirty.required_position())?;
            drop(log);
            let intact_prefix = fs::metadata(&path)?.len();
            append_bytes(
                &path,
                &build_frame(LogFormat::V2, FrameKind::PageHeader, 2, 4, 6),
            )?;
            for chunk_index in 0..complete_chunks {
                append_bytes(
                    &path,
                    &build_frame_with_payload2_bytes(
                        LogFormat::V2,
                        FrameKind::PageData,
                        2,
                        u64::try_from(chunk_index).map_err(io::Error::other)?,
                        if chunk_index == 0 {
                            [1, 2, 3, 4, 5, 6, 7, 8]
                        } else {
                            [9, 10, 0, 0, 0, 0, 0, 0]
                        },
                    ),
                )?;
            }
            let mut repaired = FileCommitLog::<10>::open_page_capable(&path)?;
            assert_eq!(repaired.records().len(), 1);
            assert_eq!(repaired.durable_records().len(), 1);
            assert_eq!(fs::metadata(&path)?.len(), intact_prefix);
            let replacement_page =
                unlogged_page(repaired.lineage(), 2, 2, [9, 8, 7, 6, 5, 4, 3, 2, 1, 0])?;
            let replacement_dirty = stage_page_write(&mut repaired, replacement_page)?;
            assert_eq!(replacement_dirty.required_position().get(), 2);
            drop(repaired);
        }

        let path = directory.path().join("partial-physical.bin");
        let mut log = FileCommitLog::<10>::create_new_page_capable(&path, persistent_id(161)?)?;
        let prefix_page = unlogged_page(log.lineage(), 1, 1, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9])?;
        let prefix_dirty = stage_page_write(&mut log, prefix_page)?;
        log.flush_through(prefix_dirty.required_position())?;
        drop(log);
        let intact_prefix = fs::metadata(&path)?.len();
        append_bytes(
            &path,
            &build_frame(LogFormat::V2, FrameKind::PageHeader, 2, 4, 6),
        )?;
        append_bytes(
            &path,
            &build_frame_with_payload2_bytes(
                LogFormat::V2,
                FrameKind::PageData,
                2,
                0,
                [1, 2, 3, 4, 5, 6, 7, 8],
            ),
        )?;
        append_bytes(
            &path,
            &build_frame_with_payload2_bytes(
                LogFormat::V2,
                FrameKind::PageData,
                2,
                1,
                [9, 10, 0, 0, 0, 0, 0, 0],
            )[..FRAME_LENGTH - 3],
        )?;
        let mut repaired = FileCommitLog::<10>::open_page_capable(&path)?;
        assert_eq!(repaired.records().len(), 1);
        assert_eq!(repaired.durable_records().len(), 1);
        assert_eq!(fs::metadata(&path)?.len(), intact_prefix);
        let replacement_page =
            unlogged_page(repaired.lineage(), 2, 2, [9, 8, 7, 6, 5, 4, 3, 2, 1, 0])?;
        let replacement_dirty = stage_page_write(&mut repaired, replacement_page)?;
        assert_eq!(replacement_dirty.required_position().get(), 2);
        assert_eq!(layout.chunk_count, 2);
        Ok(())
    }

    #[test]
    fn complete_page_corruption_fails_without_truncation() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("page-corruption")?;

        let path = directory.path().join("wrong-kind.bin");
        let log = FileCommitLog::<10>::create_new_page_capable(&path, persistent_id(167)?)?;
        drop(log);
        append_bytes(
            &path,
            &build_frame(LogFormat::V2, FrameKind::PageHeader, 1, 4, 6),
        )?;
        append_bytes(
            &path,
            &build_frame(LogFormat::V2, FrameKind::DurableThrough, 1, 0, 0),
        )?;
        let len_before = fs::metadata(&path)?.len();
        let wrong_kind = FileCommitLog::<10>::open_page_capable(&path)
            .err()
            .ok_or_else(|| io::Error::other("wrong frame kind inside page unexpectedly opened"))?;
        assert_eq!(
            wrong_kind,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 + 4,
                FileFormatErrorReason::PageDataInterruptedByFrameKind { actual: 3 },
            ))
        );
        assert_eq!(fs::metadata(&path)?.len(), len_before);

        let path = directory.path().join("wrong-parent.bin");
        let log = FileCommitLog::<10>::create_new_page_capable(&path, persistent_id(173)?)?;
        drop(log);
        append_bytes(
            &path,
            &build_frame(LogFormat::V2, FrameKind::PageHeader, 1, 4, 6),
        )?;
        append_bytes(
            &path,
            &build_frame_with_payload2_bytes(
                LogFormat::V2,
                FrameKind::PageData,
                2,
                0,
                [1, 2, 3, 4, 5, 6, 7, 8],
            ),
        )?;
        let len_before = fs::metadata(&path)?.len();
        let wrong_parent = FileCommitLog::<10>::open_page_capable(&path)
            .err()
            .ok_or_else(|| io::Error::other("wrong page parent unexpectedly opened"))?;
        assert_eq!(
            wrong_parent,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 + 16,
                FileFormatErrorReason::PageDataParentMismatch {
                    expected: 1,
                    actual: 2,
                },
            ))
        );
        assert_eq!(fs::metadata(&path)?.len(), len_before);

        let path = directory.path().join("wrong-index.bin");
        let log = FileCommitLog::<10>::create_new_page_capable(&path, persistent_id(179)?)?;
        drop(log);
        append_bytes(
            &path,
            &build_frame(LogFormat::V2, FrameKind::PageHeader, 1, 4, 6),
        )?;
        append_bytes(
            &path,
            &build_frame_with_payload2_bytes(
                LogFormat::V2,
                FrameKind::PageData,
                1,
                1,
                [1, 2, 3, 4, 5, 6, 7, 8],
            ),
        )?;
        let len_before = fs::metadata(&path)?.len();
        let wrong_index = FileCommitLog::<10>::open_page_capable(&path)
            .err()
            .ok_or_else(|| io::Error::other("wrong chunk index unexpectedly opened"))?;
        assert_eq!(
            wrong_index,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 + 24,
                FileFormatErrorReason::PageDataChunkIndexOutOfSequence {
                    expected: 0,
                    actual: 1,
                },
            ))
        );
        assert_eq!(fs::metadata(&path)?.len(), len_before);

        let path = directory.path().join("replayed-position.bin");
        let log = FileCommitLog::<10>::create_new_page_capable(&path, persistent_id(181)?)?;
        drop(log);
        append_bytes(
            &path,
            &build_frame(LogFormat::V2, FrameKind::PageHeader, 1, 4, 6),
        )?;
        append_bytes(
            &path,
            &build_frame_with_payload2_bytes(
                LogFormat::V2,
                FrameKind::PageData,
                1,
                0,
                [1, 2, 3, 4, 5, 6, 7, 8],
            ),
        )?;
        append_bytes(
            &path,
            &build_frame_with_payload2_bytes(
                LogFormat::V2,
                FrameKind::PageData,
                1,
                1,
                [9, 10, 0, 0, 0, 0, 0, 0],
            ),
        )?;
        append_bytes(
            &path,
            &build_frame(LogFormat::V2, FrameKind::PageHeader, 1, 5, 7),
        )?;
        let len_before = fs::metadata(&path)?.len();
        let replayed_position = FileCommitLog::<10>::open_page_capable(&path)
            .err()
            .ok_or_else(|| io::Error::other("replayed page position unexpectedly opened"))?;
        assert_eq!(
            replayed_position,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 3 + 16,
                FileFormatErrorReason::PageHeaderPositionOutOfSequence {
                    expected: 2,
                    actual: 1,
                },
            ))
        );
        assert_eq!(fs::metadata(&path)?.len(), len_before);

        let path = directory.path().join("wrong-padding.bin");
        let log = FileCommitLog::<10>::create_new_page_capable(&path, persistent_id(191)?)?;
        drop(log);
        append_bytes(
            &path,
            &build_frame(LogFormat::V2, FrameKind::PageHeader, 1, 4, 6),
        )?;
        append_bytes(
            &path,
            &build_frame_with_payload2_bytes(
                LogFormat::V2,
                FrameKind::PageData,
                1,
                0,
                [1, 2, 3, 4, 5, 6, 7, 8],
            ),
        )?;
        append_bytes(
            &path,
            &build_frame_with_payload2_bytes(
                LogFormat::V2,
                FrameKind::PageData,
                1,
                1,
                [9, 10, 1, 0, 0, 0, 0, 0],
            ),
        )?;
        let len_before = fs::metadata(&path)?.len();
        let wrong_padding = FileCommitLog::<10>::open_page_capable(&path)
            .err()
            .ok_or_else(|| io::Error::other("nonzero page padding unexpectedly opened"))?;
        assert_eq!(
            wrong_padding,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 2 + 32,
                FileFormatErrorReason::PageDataFinalPaddingNonzero,
            ))
        );
        assert_eq!(fs::metadata(&path)?.len(), len_before);

        let path = directory.path().join("wrong-checksum.bin");
        let mut log = FileCommitLog::<10>::create_new_page_capable(&path, persistent_id(193)?)?;
        let page = unlogged_page(log.lineage(), 8, 2, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10])?;
        let _dirty = stage_page_write(&mut log, page)?;
        drop(log);
        let len_before = fs::metadata(&path)?.len();
        flip_byte(&path, HEADER_LENGTH + FRAME_LENGTH + FRAME_CHECKSUM_OFFSET)?;
        let wrong_checksum = FileCommitLog::<10>::open_page_capable(&path)
            .err()
            .ok_or_else(|| io::Error::other("corrupted page checksum unexpectedly opened"))?;
        let bytes = fs::read(&path)?;
        assert_eq!(
            wrong_checksum,
            FileOpenError::Format(FileFormatError::new(
                HEADER_LENGTH_U64 + FRAME_LENGTH_U64 + FRAME_CHECKSUM_OFFSET_U64,
                FileFormatErrorReason::FrameChecksum {
                    expected: checksum_v1(
                        &bytes[HEADER_LENGTH + FRAME_LENGTH
                            ..HEADER_LENGTH + FRAME_LENGTH + FRAME_CHECKSUM_OFFSET],
                    ),
                    actual: read_u64(&bytes, HEADER_LENGTH + FRAME_LENGTH + FRAME_CHECKSUM_OFFSET),
                },
            ))
        );
        assert_eq!(fs::metadata(&path)?.len(), len_before);
        Ok(())
    }

    #[test]
    fn page_foreign_lineage_and_fault_preservation_match_in_memory_contract()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("page-foreign")?;
        let path = directory.path().join("commit-log.bin");
        let mut log = FileCommitLog::<2>::create_new_page_capable(&path, persistent_id(193)?)?;
        log.arm_fault(FaultPoint::AfterAppend)?;
        let foreign_lineage = LogLineage::new();
        let foreign_page = unlogged_page(&foreign_lineage, 9, 7, [5, 6])?;

        let error = PageLog::<2>::append_page(&mut log, &foreign_page)
            .err()
            .ok_or_else(|| io::Error::other("foreign page unexpectedly appended"))?;
        assert_eq!(
            error,
            FileCommitLogError::ForeignPageLineage(page_number(9)?)
        );
        assert!(log.records().is_empty());
        assert_eq!(log.durable_position(), None);
        assert_eq!(log.armed_fault(), Some(FaultPoint::AfterAppend));

        let local_page = unlogged_page(log.lineage(), 10, 8, [7, 8])?;
        let local_error = PageLog::<2>::append_page(&mut log, &local_page)
            .err()
            .ok_or_else(|| io::Error::other("armed append fault unexpectedly disappeared"))?;
        assert_eq!(
            local_error,
            FileCommitLogError::InjectedFault(FaultPoint::AfterAppend)
        );
        assert_eq!(log.records().len(), 1);
        assert_eq!(log.records()[0].position().get(), 1);
        Ok(())
    }

    #[test]
    fn v3_golden_bytes_cover_owned_page_owner_data_commit_and_marker() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new("v3-golden-bytes")?;
        let path = directory.path().join("commit-log.bin");
        let mut log =
            FileCommitLog::<10>::create_new_transaction_page_capable(&path, persistent_id(271)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let owner = active.transaction_id();
        let page = unlogged_page(log.lineage(), 7, 9, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10])?;
        let (active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
        let committed = coordinator.commit(active, &mut log)?;
        assert_eq!(dirty.required_position().get(), 1);
        assert_eq!(committed.log_position().get(), 2);
        drop(log);

        let bytes = fs::read(&path)?;
        assert_eq!(bytes.len(), HEADER_LENGTH + FRAME_LENGTH * 7);
        let mut header = [0_u8; HEADER_LENGTH];
        header.copy_from_slice(&bytes[..HEADER_LENGTH]);
        assert_eq!(
            parse_header(
                &header,
                HeaderExpectation::V3OrLater(PageLayout::for_const::<10>()?)
            )?,
            persistent_id(271)?
        );
        assert_eq!(read_u16(&header, 8), FORMAT_VERSION_V3);
        assert_eq!(read_u64(&header, HEADER_V2_PAGE_WIDTH_OFFSET), 10);
        assert_eq!(
            read_u64(&header, HEADER_CHECKSUM_OFFSET),
            0xb458_6dc8_06be_b448
        );

        let epoch_frame = wal_frame(&bytes, 0)?;
        assert_eq!(
            parse_frame(&epoch_frame, wal_frame_offset(0)?, LogFormat::V3)?,
            DecodedFrame {
                kind: FrameKind::EpochAllocation,
                payload0: 1,
                payload1: 0,
                payload2: 0,
                payload2_bytes: 0_u64.to_be_bytes(),
            }
        );
        assert_eq!(
            read_u64(&epoch_frame, FRAME_CHECKSUM_OFFSET),
            0x73f5_86d9_0b90_6091
        );

        let owned_header_frame = wal_frame(&bytes, 1)?;
        assert_eq!(
            parse_frame(&owned_header_frame, wal_frame_offset(1)?, LogFormat::V3)?,
            DecodedFrame {
                kind: FrameKind::TransactionPageHeader,
                payload0: 1,
                payload1: 7,
                payload2: 9,
                payload2_bytes: 9_u64.to_be_bytes(),
            }
        );
        assert_eq!(
            read_u64(&owned_header_frame, FRAME_CHECKSUM_OFFSET),
            0x55e6_ed5c_e0e4_afb3
        );

        let owner_frame = wal_frame(&bytes, 2)?;
        assert_eq!(
            parse_frame(&owner_frame, wal_frame_offset(2)?, LogFormat::V3)?,
            DecodedFrame {
                kind: FrameKind::TransactionPageOwner,
                payload0: 1,
                payload1: owner.epoch().get(),
                payload2: owner.sequence(),
                payload2_bytes: owner.sequence().to_be_bytes(),
            }
        );
        assert_eq!(
            read_u64(&owner_frame, FRAME_CHECKSUM_OFFSET),
            0xcf49_d22b_674e_bff7
        );

        let first_data_frame = wal_frame(&bytes, 3)?;
        let first_data = parse_frame(&first_data_frame, wal_frame_offset(3)?, LogFormat::V3)?;
        assert_eq!(first_data.kind, FrameKind::PageData);
        assert_eq!(first_data.payload0, 1);
        assert_eq!(first_data.payload1, 0);
        assert_eq!(first_data.payload2_bytes, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            read_u64(&first_data_frame, FRAME_CHECKSUM_OFFSET),
            0x7a1e_fe7d_8c1b_1e37
        );

        let final_data_frame = wal_frame(&bytes, 4)?;
        let final_data = parse_frame(&final_data_frame, wal_frame_offset(4)?, LogFormat::V3)?;
        assert_eq!(final_data.kind, FrameKind::PageData);
        assert_eq!(final_data.payload0, 1);
        assert_eq!(final_data.payload1, 1);
        assert_eq!(final_data.payload2_bytes, [9, 10, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            read_u64(&final_data_frame, FRAME_CHECKSUM_OFFSET),
            0x9f91_16f8_ede9_80cf
        );

        let commit_frame = wal_frame(&bytes, 5)?;
        assert_eq!(
            parse_frame(&commit_frame, wal_frame_offset(5)?, LogFormat::V3)?,
            DecodedFrame {
                kind: FrameKind::CommitRecord,
                payload0: 2,
                payload1: owner.epoch().get(),
                payload2: owner.sequence(),
                payload2_bytes: owner.sequence().to_be_bytes(),
            }
        );
        assert_eq!(
            read_u64(&commit_frame, FRAME_CHECKSUM_OFFSET),
            0x1f43_b64d_8069_8d46
        );

        let marker_frame = wal_frame(&bytes, 6)?;
        assert_eq!(
            parse_frame(&marker_frame, wal_frame_offset(6)?, LogFormat::V3)?,
            DecodedFrame {
                kind: FrameKind::DurableThrough,
                payload0: 2,
                payload1: 0,
                payload2: 0,
                payload2_bytes: 0_u64.to_be_bytes(),
            }
        );
        assert_eq!(
            read_u64(&marker_frame, FRAME_CHECKSUM_OFFSET),
            0xb28e_1b5b_447e_a1a4
        );
        Ok(())
    }

    #[test]
    fn v3_owned_page_and_commit_keep_owner_lookup_and_store_authority_separate()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-owned-lifecycle")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let persistent_id = persistent_id(277)?;
        let mut log =
            FileCommitLog::<4>::create_new_transaction_page_capable(&log_path, persistent_id)?;
        let mut store = FilePageStore::<4>::create_new(&store_path, persistent_id)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let owner = active.transaction_id();
        let page = unlogged_page(log.lineage(), 15, 5, [7, 8, 9, 10])?;

        let (active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
        log.flush_through(dirty.required_position())?;
        let (_, lookup_before_commit) = log.lookup_durable_commit(owner)?;
        assert_eq!(lookup_before_commit, DurableCommitLookup::Absent);
        assert!(store.pages().is_empty());

        let owned_record = &log.records()[0];
        assert_eq!(owned_record.transaction_epoch(), None);
        assert_eq!(owned_record.transaction_sequence(), None);
        assert_eq!(
            owned_record.page_owner_transaction_epoch(),
            Some(owner.epoch().get())
        );
        assert_eq!(
            owned_record.page_owner_transaction_sequence(),
            Some(owner.sequence())
        );
        assert!(owned_record.page_owner_matches_transaction_id(owner));
        let owned = owned_record
            .transaction_page_write()
            .ok_or_else(|| io::Error::other("owned v3 record is missing"))?;
        assert!(owned.matches_transaction_id(owner));
        assert_eq!(owned.page_write().page_number().get(), 15);
        assert_eq!(owned.page_write().page_version().get(), 5);
        assert_eq!(owned.page_write().bytes(), &[7, 8, 9, 10]);
        let observation = owned_record
            .page_recovery_observation()?
            .ok_or_else(|| io::Error::other("owned page lost its recovery projection"))?;
        assert_eq!(observation.position(), dirty.required_position());
        assert_eq!(observation.image().bytes(), &[7, 8, 9, 10]);

        drop(log);
        let mut log = FileCommitLog::<4>::open_transaction_page_capable(&log_path)?;
        assert_eq!(log.records().len(), 1);
        assert_eq!(log.durable_records().len(), 1);
        assert!(log.records()[0].page_owner_matches_transaction_id(owner));
        let (_, reopened_lookup_before_commit) = log.lookup_durable_commit(owner)?;
        assert_eq!(reopened_lookup_before_commit, DurableCommitLookup::Absent);

        let committed = coordinator.commit(active, &mut log)?;
        assert_eq!(dirty.required_position().get(), 1);
        assert_eq!(committed.log_position().get(), 2);
        assert_eq!(log.durable_records().len(), 2);
        let (_, lookup_after_commit) = log.lookup_durable_commit(owner)?;
        assert_eq!(
            lookup_after_commit,
            DurableCommitLookup::Found {
                position: log.lineage().position(2),
            }
        );

        let clean = flush_committed_page(&committed, &mut log, &mut store, dirty)?;
        assert_eq!(clean.transaction_id(), owner);
        assert_eq!(clean.required_position().get(), 1);
        let stored = store
            .page(page_number(15)?)
            .ok_or_else(|| io::Error::other("committed v3 page was not stored"))?;
        assert_eq!(stored.page_version().get(), 5);
        assert_eq!(stored.bytes(), &[7, 8, 9, 10]);
        drop(store);
        drop(log);

        let mut reopened = FileCommitLog::<4>::open_transaction_page_capable(&log_path)?;
        let reopened_store = FilePageStore::<4>::open(&store_path)?;
        assert_eq!(reopened.records().len(), 2);
        assert_eq!(reopened.durable_records().len(), 2);
        assert!(reopened.records()[0].page_owner_matches_transaction_id(owner));
        assert!(reopened.records()[1].matches_transaction_id(owner));
        let (_, reopened_lookup) = reopened.lookup_durable_commit(owner)?;
        assert_eq!(
            reopened_lookup,
            DurableCommitLookup::Found {
                position: reopened.lineage().position(2),
            }
        );
        assert_eq!(
            reopened_store
                .page(page_number(15)?)
                .ok_or_else(|| io::Error::other("reopened store lost committed page"))?
                .bytes(),
            &[7, 8, 9, 10]
        );
        Ok(())
    }

    #[test]
    fn v1_and_v2_reject_transaction_pages_before_fault_or_position_effect()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("transaction-page-version-rejection")?;

        let v1_path = directory.path().join("v1.bin");
        let mut v1 = FileCommitLog::<0>::create_new(&v1_path, persistent_id(281)?)?;
        let mut v1_coordinator = TransactionCoordinator::open(&mut v1)?;
        v1.arm_fault(FaultPoint::AfterAppend)?;
        let v1_active = v1_coordinator.begin()?;
        let v1_page = unlogged_page(v1.lineage(), 1, 1, [1_u8])?;
        let v1_error = v1_coordinator
            .stage_page_write(v1_active, v1_page, &mut v1)
            .err()
            .ok_or_else(|| io::Error::other("v1 accepted a transaction page"))?;
        let TransactionPageStageError::Append(v1_error) = v1_error else {
            return Err(io::Error::other("v1 rejection returned the wrong error shape").into());
        };
        assert_eq!(
            v1_error.cause(),
            &FileCommitLogError::TransactionPageSupportUnavailable
        );
        assert!(v1.records().is_empty());
        assert_eq!(v1.armed_fault(), Some(FaultPoint::AfterAppend));

        let v2_path = directory.path().join("v2.bin");
        let mut v2 = FileCommitLog::<2>::create_new_page_capable(&v2_path, persistent_id(283)?)?;
        let mut v2_coordinator = TransactionCoordinator::open(&mut v2)?;
        v2.arm_fault(FaultPoint::AfterAppend)?;
        let v2_active = v2_coordinator.begin()?;
        let v2_page = unlogged_page(v2.lineage(), 2, 2, [2_u8, 3])?;
        let v2_error = v2_coordinator
            .stage_page_write(v2_active, v2_page, &mut v2)
            .err()
            .ok_or_else(|| io::Error::other("v2 accepted a transaction page"))?;
        let TransactionPageStageError::Append(v2_error) = v2_error else {
            return Err(io::Error::other("v2 rejection returned the wrong error shape").into());
        };
        assert_eq!(
            v2_error.cause(),
            &FileCommitLogError::TransactionPageSupportUnavailable
        );
        assert!(v2.records().is_empty());
        assert_eq!(v2.armed_fault(), Some(FaultPoint::AfterAppend));

        let raw_page = unlogged_page(v2.lineage(), 3, 3, [4_u8, 5])?;
        let raw_error = stage_page_write(&mut v2, raw_page)
            .err()
            .ok_or_else(|| io::Error::other("preserved v2 fault unexpectedly disappeared"))?;
        let StagePageWriteError::Append(raw_error) = raw_error else {
            return Err(io::Error::other("raw v2 fault returned the wrong error shape").into());
        };
        assert_eq!(
            raw_error.cause(),
            &FileCommitLogError::InjectedFault(FaultPoint::AfterAppend)
        );
        assert_eq!(v2.records().len(), 1);
        assert_eq!(v2.records()[0].position().get(), 1);
        Ok(())
    }

    #[test]
    fn v3_transaction_page_append_faults_have_exact_file_and_position_effects()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-append-faults")?;

        let before_path = directory.path().join("before.bin");
        let mut before = FileCommitLog::<2>::create_new_transaction_page_capable(
            &before_path,
            persistent_id(287)?,
        )?;
        let mut before_coordinator = TransactionCoordinator::open(&mut before)?;
        before.arm_fault(FaultPoint::BeforeAppend)?;
        let before_active = before_coordinator.begin()?;
        let before_page = unlogged_page(before.lineage(), 4, 1, [1_u8, 2])?;
        let before_error = before_coordinator
            .stage_page_write(before_active, before_page, &mut before)
            .err()
            .ok_or_else(|| io::Error::other("v3 before-append fault succeeded"))?;
        let TransactionPageStageError::Append(before_error) = before_error else {
            return Err(io::Error::other("before fault returned wrong error shape").into());
        };
        assert_eq!(
            before_error.cause(),
            &FileCommitLogError::InjectedFault(FaultPoint::BeforeAppend)
        );
        assert!(before.records().is_empty());
        assert_eq!(
            fs::metadata(&before_path)?.len(),
            HEADER_LENGTH_U64 + FRAME_LENGTH_U64
        );
        let raw = unlogged_page(before.lineage(), 5, 2, [3_u8, 4])?;
        let raw_dirty = stage_page_write(&mut before, raw)?;
        assert_eq!(raw_dirty.required_position().get(), 1);
        drop(before);
        let reopened_before = FileCommitLog::<2>::open_transaction_page_capable(&before_path)?;
        assert_eq!(reopened_before.records().len(), 1);
        assert_eq!(reopened_before.records()[0].position().get(), 1);

        let after_path = directory.path().join("after.bin");
        let mut after = FileCommitLog::<2>::create_new_transaction_page_capable(
            &after_path,
            persistent_id(289)?,
        )?;
        let mut after_coordinator = TransactionCoordinator::open(&mut after)?;
        after.arm_fault(FaultPoint::AfterAppend)?;
        let after_active = after_coordinator.begin()?;
        let owner = after_active.transaction_id();
        let after_page = unlogged_page(after.lineage(), 6, 3, [5_u8, 6])?;
        let after_error = after_coordinator
            .stage_page_write(after_active, after_page, &mut after)
            .err()
            .ok_or_else(|| io::Error::other("v3 after-append fault succeeded"))?;
        let TransactionPageStageError::Append(after_error) = after_error else {
            return Err(io::Error::other("after fault returned wrong error shape").into());
        };
        assert_eq!(
            after_error.cause(),
            &FileCommitLogError::InjectedFault(FaultPoint::AfterAppend)
        );
        assert_eq!(after.records().len(), 1);
        assert_eq!(after.records()[0].position().get(), 1);
        assert!(after.records()[0].page_owner_matches_transaction_id(owner));
        assert_eq!(after.durable_records().len(), 0);
        assert_eq!(
            fs::metadata(&after_path)?.len(),
            HEADER_LENGTH_U64 + FRAME_LENGTH_U64 * 4
        );
        drop(after);

        let mut reopened_after = FileCommitLog::<2>::open_transaction_page_capable(&after_path)?;
        assert_eq!(reopened_after.records().len(), 1);
        assert_eq!(reopened_after.durable_records().len(), 0);
        assert!(reopened_after.records()[0].page_owner_matches_transaction_id(owner));
        let next_page = unlogged_page(reopened_after.lineage(), 7, 4, [7_u8, 8])?;
        let next_dirty = stage_page_write(&mut reopened_after, next_page)?;
        assert_eq!(next_dirty.required_position().get(), 2);
        Ok(())
    }

    #[test]
    fn v4_generation_header_has_exact_big_endian_golden_bytes() -> Result<(), Box<dyn Error>> {
        let persistent_id = persistent_id(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)?;
        let allocated_epoch_high_water = NonZeroU64::new(0x5152_5354_5556_5758)
            .ok_or_else(|| io::Error::other("golden epoch is zero"))?;
        let actual = build_header_v4(
            persistent_id,
            10,
            0x2122_2324_2526_2728,
            Some(0x3132_3334_3536_3738),
            Some(0x4142_4344_4546_4748),
            allocated_epoch_high_water,
            (0x6162, 0x7172_7374_7576_7778_797a_7b7c_7d7e_7f80),
        );
        let expected = [
            0x4e, 0x54, 0x53, 0x51, 0x4c, 0x4f, 0x47, 0x31, 0x00, 0x04, 0x00, 0x80, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x21, 0x22,
            0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38,
            0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56,
            0x57, 0x58, 0x61, 0x62, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x71, 0x72, 0x73, 0x74,
            0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2b, 0x01, 0xc3, 0x42, 0xeb, 0xdc,
            0x97, 0x73,
        ];
        assert_eq!(actual, expected);
        assert_eq!(
            parse_header_v4(&actual, PageLayout::for_const::<10>()?)?,
            V4HeaderMetadata {
                persistent_id,
                generation: 0x2122_2324_2526_2728,
                retained_first: Some(0x3132_3334_3536_3738),
                logical_high_water: Some(0x4142_4344_4546_4748),
                allocated_epoch_high_water,
                selected_checkpoint_anchor: (0x6162, 0x7172_7374_7576_7778_797a_7b7c_7d7e_7f80,),
            }
        );
        Ok(())
    }

    #[test]
    fn v5_database_header_has_exact_golden_bytes_and_survives_reclamation()
    -> Result<(), Box<dyn Error>> {
        let persistent_id = persistent_id(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)?;
        let identity = database_file_header_identity(DatabaseFileRole::Wal)?;
        let initial = build_header_v5_initial(persistent_id, 10, identity);
        let mut expected = [0_u8; HEADER_V5_LENGTH];
        expected[..8].copy_from_slice(b"NTSQLOG1");
        expected[8..12].copy_from_slice(&[0, 5, 0, 192]);
        expected[16..32].copy_from_slice(&persistent_id.get().to_be_bytes());
        expected[32..40].copy_from_slice(&10_u64.to_be_bytes());
        expected[128..176].copy_from_slice(&[
            0x4e, 0x54, 0x53, 0x51, 0x43, 0x46, 0x49, 0x31, 0x00, 0x01, 0x00, 0x30, 0x01, 0x00,
            0x00, 0x00, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a,
            0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30,
        ]);
        expected[184..192].copy_from_slice(&0xceda_f738_4da6_08cf_u64.to_be_bytes());
        assert_eq!(initial, expected);
        assert_eq!(
            parse_header_v5(&initial, PageLayout::for_const::<10>()?)?,
            V5HeaderMetadata {
                persistent_id,
                generation: 0,
                retained_first: None,
                logical_high_water: None,
                allocated_epoch_high_water: None,
                selected_checkpoint_anchor: None,
                database_file_identity: identity,
            }
        );

        let allocated_epoch_high_water = NonZeroU64::new(0x5152_5354_5556_5758)
            .ok_or_else(|| io::Error::other("test epoch is zero"))?;
        let reclaimed = build_header_v5_reclaimed(
            V4HeaderMetadata {
                persistent_id,
                generation: 0x2122_2324_2526_2728,
                retained_first: Some(0x3132_3334_3536_3738),
                logical_high_water: Some(0x4142_4344_4546_4748),
                allocated_epoch_high_water,
                selected_checkpoint_anchor: (0x6162, 0x7172_7374_7576_7778_797a_7b7c_7d7e_7f80),
            },
            10,
            identity,
        );
        assert_eq!(&reclaimed[128..176], &initial[128..176]);
        assert_eq!(
            read_u64(&reclaimed, HEADER_V5_CHECKSUM_OFFSET),
            0xb749_8c55_7b2f_625f
        );
        let metadata = parse_header_v5(&reclaimed, PageLayout::for_const::<10>()?)?;
        assert_eq!(metadata.database_file_identity, identity);
        assert_eq!(metadata.generation, 0x2122_2324_2526_2728);

        let mut reserved = initial;
        reserved[176] = 1;
        let checksum = checksum_v1(&reserved[..HEADER_V5_CHECKSUM_OFFSET]);
        write_u64(&mut reserved, HEADER_V5_CHECKSUM_OFFSET, checksum);
        assert!(parse_header_v5(&reserved, PageLayout::for_const::<10>()?).is_err());
        let mut bad_checksum = initial;
        bad_checksum[HEADER_V5_CHECKSUM_OFFSET] ^= 1;
        assert!(parse_header_v5(&bad_checksum, PageLayout::for_const::<10>()?).is_err());
        Ok(())
    }

    #[test]
    fn v3_raw_owned_and_commit_records_share_order_and_reopen() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-mixed-order")?;
        let path = directory.path().join("commit-log.bin");
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&path, persistent_id(293)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;

        let raw_page = unlogged_page(log.lineage(), 8, 1, [1_u8, 2])?;
        let raw_dirty = stage_page_write(&mut log, raw_page)?;
        let active = coordinator.begin()?;
        let owner = active.transaction_id();
        let owned_page = unlogged_page(log.lineage(), 9, 2, [3_u8, 4])?;
        let (active, owned_dirty) = coordinator.stage_page_write(active, owned_page, &mut log)?;
        let committed = coordinator.commit(active, &mut log)?;

        assert_eq!(raw_dirty.required_position().get(), 1);
        assert_eq!(owned_dirty.required_position().get(), 2);
        assert_eq!(committed.log_position().get(), 3);
        assert_eq!(log.records().len(), 3);
        assert_eq!(log.durable_records().len(), 3);
        assert!(log.records()[0].page_write().is_some());
        assert!(log.records()[0].transaction_page_write().is_none());
        assert!(log.records()[1].page_owner_matches_transaction_id(owner));
        assert!(log.records()[2].matches_transaction_id(owner));
        drop(log);

        let reopened = FileCommitLog::<2>::open_transaction_page_capable(&path)?;
        assert_eq!(reopened.records().len(), 3);
        assert_eq!(reopened.durable_records().len(), 3);
        assert_eq!(
            reopened.durable_position(),
            Some(reopened.lineage().position(3))
        );
        assert!(reopened.records()[0].transaction_page_write().is_none());
        assert!(reopened.records()[1].page_owner_matches_transaction_id(owner));
        assert!(reopened.records()[2].matches_transaction_id(owner));
        Ok(())
    }

    #[test]
    fn v3_durable_prefix_projection_classifies_committed_and_uncommitted_pages_after_reopen()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-transaction-recovery-projection")?;
        let path = directory.path().join("commit-log.bin");
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&path, persistent_id(419)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;

        let committed_active = coordinator.begin()?;
        let committed_owner = committed_active.transaction_id();
        let committed_page = unlogged_page(log.lineage(), 20, 1, [1_u8, 2])?;
        let (committed_active, committed_dirty) =
            coordinator.stage_page_write(committed_active, committed_page, &mut log)?;
        let committed = coordinator.commit(committed_active, &mut log)?;
        assert_eq!(committed_dirty.required_position().get(), 1);
        assert_eq!(committed.log_position().get(), 2);

        let uncommitted_active = coordinator.begin()?;
        let uncommitted_owner = uncommitted_active.transaction_id();
        let uncommitted_page = unlogged_page(log.lineage(), 21, 2, [3_u8, 4])?;
        let (uncommitted_active, uncommitted_dirty) =
            coordinator.stage_page_write(uncommitted_active, uncommitted_page, &mut log)?;
        assert_eq!(uncommitted_dirty.required_position().get(), 3);
        log.flush_through(uncommitted_dirty.required_position())?;

        let raw_page = unlogged_page(log.lineage(), 22, 3, [5_u8, 6])?;
        let raw_dirty = stage_page_write(&mut log, raw_page)?;
        assert_eq!(raw_dirty.required_position().get(), 4);
        log.flush_through(raw_dirty.required_position())?;

        log.arm_fault(FaultPoint::BeforeFlush)?;
        let volatile_commit = coordinator
            .commit(uncommitted_active, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("volatile v3 commit unexpectedly became durable"))?;
        assert!(matches!(
            volatile_commit,
            CoordinatedCommitError::Indeterminate(_)
        ));
        assert_eq!(log.records().len(), 5);
        assert_eq!(log.durable_records().len(), 4);

        drop(committed_dirty);
        drop(uncommitted_dirty);
        drop(raw_dirty);
        drop(log);

        let reopened = FileCommitLog::<2>::open_transaction_page_capable(&path)?;
        assert_eq!(reopened.records().len(), 5);
        assert_eq!(reopened.durable_records().len(), 4);

        let committed_page_observation = reopened.records()[0]
            .transaction_page_recovery_observation()?
            .ok_or_else(|| io::Error::other("reopened committed page did not project"))?;
        assert!(
            committed_page_observation
                .owner()
                .matches_transaction_id(committed_owner)
        );
        assert!(
            reopened.records()[0]
                .transaction_commit_recovery_observation()?
                .is_none()
        );
        assert!(reopened.records()[0].page_recovery_observation()?.is_some());

        assert!(
            reopened.records()[1]
                .transaction_page_recovery_observation()?
                .is_none()
        );
        assert!(
            reopened.records()[1]
                .transaction_commit_recovery_observation()?
                .is_some()
        );

        let uncommitted_page_observation = reopened.records()[2]
            .transaction_page_recovery_observation()?
            .ok_or_else(|| io::Error::other("reopened uncommitted page did not project"))?;
        assert!(
            uncommitted_page_observation
                .owner()
                .matches_transaction_id(uncommitted_owner)
        );

        assert!(
            reopened.records()[3]
                .transaction_page_recovery_observation()?
                .is_none()
        );
        assert!(
            reopened.records()[3]
                .transaction_commit_recovery_observation()?
                .is_none()
        );
        assert!(reopened.records()[3].page_recovery_observation()?.is_some());

        let durable_commits = reopened
            .durable_records()
            .map(|record| record.transaction_commit_recovery_observation())
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(durable_commits.len(), 1);
        assert!(
            durable_commits[0]
                .transaction()
                .matches_transaction_id(committed_owner)
        );
        assert_eq!(
            classify_durable_transaction_page(
                reopened.lineage(),
                &committed_page_observation,
                durable_commits.iter(),
            )?,
            DurableTransactionPageCommitClassification::Committed {
                page_position: reopened.lineage().position(1),
                commit_position: reopened.lineage().position(2),
            }
        );
        assert_eq!(
            classify_durable_transaction_page(
                reopened.lineage(),
                &uncommitted_page_observation,
                durable_commits.iter(),
            )?,
            DurableTransactionPageCommitClassification::Uncommitted {
                page_position: reopened.lineage().position(3),
            }
        );

        // The complete but unmarked commit would change the result if recovery
        // projected records() instead of the marker-covered durable prefix.
        let all_commits = reopened
            .records()
            .iter()
            .map(|record| record.transaction_commit_recovery_observation())
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(all_commits.len(), 2);
        assert_eq!(
            classify_durable_transaction_page(
                reopened.lineage(),
                &uncommitted_page_observation,
                all_commits.iter(),
            )?,
            DurableTransactionPageCommitClassification::Committed {
                page_position: reopened.lineage().position(3),
                commit_position: reopened.lineage().position(5),
            }
        );
        Ok(())
    }

    #[test]
    fn v3_committed_reconciliation_uses_one_durable_prefix_after_reopen()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-committed-reconciliation")?;
        let log_path = directory.path().join("commit-log.bin");
        let behind_store_path = directory.path().join("behind-pages.bin");
        let exact_store_path = directory.path().join("exact-pages.bin");
        let missing_store_path = directory.path().join("missing-pages.bin");
        let raw_store_path = directory.path().join("raw-pages.bin");
        let persistent_id = persistent_id(431)?;
        let number = page_number(80)?;
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
        let lineage = log.lineage().clone();
        let mut behind_store = FilePageStore::<2>::create_new(&behind_store_path, persistent_id)?;
        let mut exact_store = FilePageStore::<2>::create_new(&exact_store_path, persistent_id)?;
        let missing_store = FilePageStore::<2>::create_new(&missing_store_path, persistent_id)?;
        let mut raw_store = FilePageStore::<2>::create_new(&raw_store_path, persistent_id)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;

        let stored_active = coordinator.begin()?;
        let stored_page = unlogged_page(log.lineage(), 80, 10, [1_u8, 2])?;
        let (stored_active, stored_dirty) =
            coordinator.stage_page_write(stored_active, stored_page, &mut log)?;
        let stored_commit = coordinator.commit(stored_active, &mut log)?;
        flush_committed_page(&stored_commit, &mut log, &mut behind_store, stored_dirty)?;
        assert_eq!(stored_commit.log_position().get(), 2);

        let latest_active = coordinator.begin()?;
        let latest_page = unlogged_page(log.lineage(), 80, 1, [3_u8, 4])?;
        let (latest_active, latest_dirty) =
            coordinator.stage_page_write(latest_active, latest_page, &mut log)?;
        let latest_commit = coordinator.commit(latest_active, &mut log)?;
        flush_committed_page(&latest_commit, &mut log, &mut exact_store, latest_dirty)?;
        assert_eq!(latest_commit.log_position().get(), 4);

        let uncommitted_active = coordinator.begin()?;
        let uncommitted_page = unlogged_page(log.lineage(), 80, 20, [5_u8, 6])?;
        let (uncommitted_active, uncommitted_dirty) =
            coordinator.stage_page_write(uncommitted_active, uncommitted_page, &mut log)?;
        assert_eq!(uncommitted_dirty.required_position().get(), 5);
        log.flush_through(uncommitted_dirty.required_position())?;

        let raw_page = unlogged_page(log.lineage(), 80, 30, [7_u8, 8])?;
        let raw_dirty = stage_page_write(&mut log, raw_page)
            .map_err(|error| io::Error::other(format!("{error}")))?;
        assert_eq!(raw_dirty.required_position().get(), 6);
        write_page_through_flush(&mut log, &mut raw_store, raw_dirty)?;

        log.arm_fault(FaultPoint::BeforeFlush)?;
        let volatile_commit = coordinator
            .commit(uncommitted_active, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("volatile v3 commit unexpectedly became durable"))?;
        assert!(matches!(
            volatile_commit,
            CoordinatedCommitError::Indeterminate(_)
        ));
        assert_eq!(log.records().len(), 7);
        assert_eq!(log.durable_records().len(), 6);

        drop(uncommitted_dirty);
        drop(behind_store);
        drop(exact_store);
        drop(missing_store);
        drop(raw_store);
        drop(log);

        let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&log_path)?;
        let behind_store = FilePageStore::<2>::open(&behind_store_path)?;
        let exact_store = FilePageStore::<2>::open(&exact_store_path)?;
        let missing_store = FilePageStore::<2>::open(&missing_store_path)?;
        let raw_store = FilePageStore::<2>::open(&raw_store_path)?;
        assert!(reopened.lineage().same_lineage(&lineage));
        assert_eq!(reopened.records().len(), 7);
        assert_eq!(reopened.durable_records().len(), 6);

        let exact_snapshot = exact_store
            .page(number)
            .ok_or_else(|| io::Error::other("reopened exact snapshot is missing"))?
            .page_recovery_observation()?;
        let mut all_physical = Vec::new();
        let mut all_owned = Vec::new();
        let mut all_commits = Vec::new();
        for record in reopened.records() {
            if let Some(observation) = record.page_recovery_observation()? {
                all_physical.push(observation);
            }
            if let Some(observation) = record.transaction_page_recovery_observation()? {
                all_owned.push(observation);
            }
            if let Some(observation) = record.transaction_commit_recovery_observation()? {
                all_commits.push(observation);
            }
        }
        let all_result = reconcile_committed_transaction_page(
            reopened.lineage(),
            number,
            Some(&exact_snapshot),
            &all_physical,
            &all_owned,
            &all_commits,
        )?;
        let DurableCommittedTransactionPageReconciliation::StoreBehind {
            stored_page_position,
            stored_commit_position,
            latest_committed,
        } = all_result
        else {
            return Err(io::Error::other("all v3 records did not expose volatile commit").into());
        };
        assert_eq!(stored_page_position, reopened.lineage().position(3));
        assert_eq!(stored_commit_position, reopened.lineage().position(4));
        assert_eq!(latest_committed.observation().position().get(), 5);
        assert_eq!(latest_committed.commit_position().get(), 7);

        let mut physical = Vec::new();
        let mut owned = Vec::new();
        let mut commits = Vec::new();
        for record in reopened.durable_records() {
            if let Some(observation) = record.page_recovery_observation()? {
                physical.push(observation);
            }
            if let Some(observation) = record.transaction_page_recovery_observation()? {
                owned.push(observation);
            }
            if let Some(observation) = record.transaction_commit_recovery_observation()? {
                commits.push(observation);
            }
        }
        assert_eq!(
            physical
                .iter()
                .map(|observation| observation.position().get())
                .collect::<Vec<_>>(),
            [1, 3, 5, 6]
        );
        assert_eq!(
            owned
                .iter()
                .map(|observation| observation.position().get())
                .collect::<Vec<_>>(),
            [1, 3, 5]
        );
        assert_eq!(
            commits
                .iter()
                .map(|observation| observation.position().get())
                .collect::<Vec<_>>(),
            [2, 4]
        );
        assert!(physical.iter().all(|observation| {
            observation
                .position()
                .lineage()
                .same_lineage(reopened.lineage())
        }));
        assert!(owned.iter().all(|observation| {
            observation
                .position()
                .lineage()
                .same_lineage(reopened.lineage())
        }));
        assert!(commits.iter().all(|observation| {
            observation
                .position()
                .lineage()
                .same_lineage(reopened.lineage())
        }));

        let behind_snapshot = behind_store
            .page(number)
            .ok_or_else(|| io::Error::other("reopened behind snapshot is missing"))?
            .page_recovery_observation()?;
        let behind = reconcile_committed_transaction_page(
            reopened.lineage(),
            number,
            Some(&behind_snapshot),
            &physical,
            &owned,
            &commits,
        )?;
        let DurableCommittedTransactionPageReconciliation::StoreBehind {
            stored_page_position,
            stored_commit_position,
            latest_committed,
        } = behind
        else {
            return Err(io::Error::other("reopened v3 store was not behind").into());
        };
        assert_eq!(stored_page_position, reopened.lineage().position(1));
        assert_eq!(stored_commit_position, reopened.lineage().position(2));
        assert!(std::ptr::eq(latest_committed.observation(), &owned[1]));
        assert_eq!(
            latest_committed.observation().page().page_version().get(),
            1
        );
        assert_eq!(latest_committed.commit_position().get(), 4);

        let exact = reconcile_committed_transaction_page(
            reopened.lineage(),
            number,
            Some(&exact_snapshot),
            &physical,
            &owned,
            &commits,
        )?;
        let DurableCommittedTransactionPageReconciliation::ExactCurrent { latest_committed } =
            exact
        else {
            return Err(io::Error::other("reopened v3 store was not exact").into());
        };
        assert!(std::ptr::eq(latest_committed.observation(), &owned[1]));

        let missing_snapshot = missing_store
            .page(number)
            .map(|page| page.page_recovery_observation())
            .transpose()?;
        let missing = reconcile_committed_transaction_page(
            reopened.lineage(),
            number,
            missing_snapshot.as_ref(),
            &physical,
            &owned,
            &commits,
        )?;
        let DurableCommittedTransactionPageReconciliation::StoreMissing { latest_committed } =
            missing
        else {
            return Err(io::Error::other("reopened v3 store was not missing").into());
        };
        assert!(std::ptr::eq(latest_committed.observation(), &owned[1]));

        let raw_snapshot = raw_store
            .page(number)
            .ok_or_else(|| io::Error::other("reopened raw snapshot is missing"))?
            .page_recovery_observation()?;
        assert_eq!(
            reconcile_committed_transaction_page(
                reopened.lineage(),
                number,
                Some(&raw_snapshot),
                &physical,
                &owned,
                &commits,
            ),
            Err(
                DurableCommittedTransactionPageReconciliationError::SnapshotBackedByRawPage {
                    page_number: number,
                    position: reopened.lineage().position(6),
                }
            )
        );

        let later_page = unlogged_page(reopened.lineage(), 81, 1, [9_u8, 10])?;
        let later_dirty = stage_page_write(&mut reopened, later_page)
            .map_err(|error| io::Error::other(format!("{error}")))?;
        assert_eq!(later_dirty.required_position().get(), 8);
        Ok(())
    }

    #[test]
    fn v3_before_flush_keeps_owned_page_and_commit_volatile_across_reopen()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-before-flush")?;
        let path = directory.path().join("commit-log.bin");
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&path, persistent_id(299)?)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let owner = active.transaction_id();
        let page = unlogged_page(log.lineage(), 10, 3, [8_u8, 9])?;
        let (active, _dirty) = coordinator.stage_page_write(active, page, &mut log)?;
        log.arm_fault(FaultPoint::BeforeFlush)?;

        let (_indeterminate, cause) = indeterminate_parts(coordinator.commit(active, &mut log))?;
        let CommitError::Flush { position, source } = cause else {
            return Err(io::Error::other("v3 before-flush returned a non-flush error").into());
        };
        assert_eq!(position.get(), 2);
        assert_eq!(
            source,
            FileCommitLogError::InjectedFault(FaultPoint::BeforeFlush)
        );
        assert_eq!(log.records().len(), 2);
        assert_eq!(log.durable_records().len(), 0);
        drop(log);

        let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&path)?;
        assert_eq!(reopened.records().len(), 2);
        assert_eq!(reopened.durable_records().len(), 0);
        assert!(reopened.records()[0].page_owner_matches_transaction_id(owner));
        assert!(reopened.records()[1].matches_transaction_id(owner));
        assert_eq!(
            reopened.lookup_durable_commit(owner).err(),
            Some(FileTransactionRecoveryError::VolatileCommitRecord(owner))
        );
        Ok(())
    }

    #[test]
    fn v3_scanner_allows_repeated_owned_images_as_format_validity() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-owned-repeat")?;
        let path = directory.path().join("commit-log.bin");
        create_v3_with_allocated_epoch::<2>(&path, persistent_id(303)?)?;

        for (position, bytes) in [(1_u64, [1_u8, 2]), (2_u64, [3_u8, 4])] {
            append_bytes(
                &path,
                &build_frame(
                    LogFormat::V3,
                    FrameKind::TransactionPageHeader,
                    position,
                    12,
                    position,
                ),
            )?;
            append_bytes(
                &path,
                &build_frame(
                    LogFormat::V3,
                    FrameKind::TransactionPageOwner,
                    position,
                    1,
                    1,
                ),
            )?;
            append_bytes(
                &path,
                &build_frame_with_payload2_bytes(
                    LogFormat::V3,
                    FrameKind::PageData,
                    position,
                    0,
                    [bytes[0], bytes[1], 0, 0, 0, 0, 0, 0],
                ),
            )?;
        }

        let reopened = FileCommitLog::<2>::open_transaction_page_capable(&path)?;
        assert_eq!(reopened.records().len(), 2);
        assert_eq!(reopened.records()[0].position().get(), 1);
        assert_eq!(reopened.records()[1].position().get(), 2);
        assert_eq!(
            reopened.records()[0]
                .page_write()
                .ok_or_else(|| io::Error::other("first repeated page is missing"))?
                .bytes(),
            &[1, 2]
        );
        assert_eq!(
            reopened.records()[1]
                .page_write()
                .ok_or_else(|| io::Error::other("second repeated page is missing"))?
                .bytes(),
            &[3, 4]
        );
        Ok(())
    }

    #[test]
    fn v3_incomplete_owned_groups_repair_to_the_owned_header() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-owned-tail-repair")?;
        let owned_header = build_frame(LogFormat::V3, FrameKind::TransactionPageHeader, 1, 4, 6);
        let owner = build_frame(LogFormat::V3, FrameKind::TransactionPageOwner, 1, 1, 1);
        let first_data = build_frame_with_payload2_bytes(
            LogFormat::V3,
            FrameKind::PageData,
            1,
            0,
            [1, 2, 3, 4, 5, 6, 7, 8],
        );
        let final_data = build_frame_with_payload2_bytes(
            LogFormat::V3,
            FrameKind::PageData,
            1,
            1,
            [9, 10, 0, 0, 0, 0, 0, 0],
        );
        let cases = [
            ("header-only", vec![owned_header], Vec::new()),
            ("owner-no-data", vec![owned_header, owner], Vec::new()),
            (
                "one-data-chunk",
                vec![owned_header, owner, first_data],
                Vec::new(),
            ),
            (
                "partial-owner",
                vec![owned_header],
                owner[..FRAME_LENGTH - 3].to_vec(),
            ),
            (
                "partial-final-data",
                vec![owned_header, owner, first_data],
                final_data[..FRAME_LENGTH - 5].to_vec(),
            ),
        ];

        for (index, (name, frames, partial)) in cases.into_iter().enumerate() {
            let path = directory.path().join(format!("{name}.bin"));
            create_v3_with_allocated_epoch::<10>(
                &path,
                persistent_id(307 + u128::try_from(index)?)?,
            )?;
            let intact_prefix = fs::metadata(&path)?.len();
            for frame in frames {
                append_bytes(&path, &frame)?;
            }
            if !partial.is_empty() {
                append_bytes(&path, &partial)?;
            }
            assert!(fs::metadata(&path)?.len() > intact_prefix);

            let mut repaired = FileCommitLog::<10>::open_transaction_page_capable(&path)?;
            assert!(repaired.records().is_empty());
            assert_eq!(repaired.durable_records().len(), 0);
            assert_eq!(fs::metadata(&path)?.len(), intact_prefix);
            let raw_page = unlogged_page(repaired.lineage(), 5, 7, [9, 8, 7, 6, 5, 4, 3, 2, 1, 0])?;
            let raw_dirty = stage_page_write(&mut repaired, raw_page)?;
            assert_eq!(raw_dirty.required_position().get(), 1);
        }
        Ok(())
    }

    #[test]
    fn v3_owner_validation_corruption_never_truncates() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-owner-corruption")?;
        let owned_header = build_frame(LogFormat::V3, FrameKind::TransactionPageHeader, 1, 4, 6);
        let valid_owner = build_frame(LogFormat::V3, FrameKind::TransactionPageOwner, 1, 1, 1);

        let orphan_path = directory.path().join("orphan-owner.bin");
        create_v3_with_allocated_epoch::<10>(&orphan_path, persistent_id(313)?)?;
        append_bytes(&orphan_path, &valid_owner)?;
        assert_v3_open_error_without_truncation::<10>(
            &orphan_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(1)? + 4,
                FileFormatErrorReason::TransactionPageOwnerWithoutHeader,
            )),
        )?;

        let missing_path = directory.path().join("missing-owner.bin");
        create_v3_with_allocated_epoch::<10>(&missing_path, persistent_id(317)?)?;
        append_bytes(&missing_path, &owned_header)?;
        append_bytes(
            &missing_path,
            &build_frame_with_payload2_bytes(
                LogFormat::V3,
                FrameKind::PageData,
                1,
                0,
                [1, 2, 3, 4, 5, 6, 7, 8],
            ),
        )?;
        assert_v3_open_error_without_truncation::<10>(
            &missing_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(2)? + 4,
                FileFormatErrorReason::TransactionPageOwnerInterruptedByFrameKind { actual: 5 },
            )),
        )?;

        let wrong_parent_path = directory.path().join("wrong-parent.bin");
        create_v3_with_allocated_epoch::<10>(&wrong_parent_path, persistent_id(319)?)?;
        append_bytes(&wrong_parent_path, &owned_header)?;
        append_bytes(
            &wrong_parent_path,
            &build_frame(LogFormat::V3, FrameKind::TransactionPageOwner, 2, 1, 1),
        )?;
        assert_v3_open_error_without_truncation::<10>(
            &wrong_parent_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(2)? + 16,
                FileFormatErrorReason::TransactionPageOwnerParentMismatch {
                    expected: 1,
                    actual: 2,
                },
            )),
        )?;

        let zero_parent_path = directory.path().join("zero-parent.bin");
        create_v3_with_allocated_epoch::<10>(&zero_parent_path, persistent_id(327)?)?;
        append_bytes(&zero_parent_path, &owned_header)?;
        append_bytes(
            &zero_parent_path,
            &build_frame(LogFormat::V3, FrameKind::TransactionPageOwner, 0, 1, 1),
        )?;
        assert_v3_open_error_without_truncation::<10>(
            &zero_parent_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(2)? + 16,
                FileFormatErrorReason::TransactionPageOwnerParentPositionZero,
            )),
        )?;

        let zero_epoch_path = directory.path().join("zero-epoch.bin");
        create_v3_with_allocated_epoch::<10>(&zero_epoch_path, persistent_id(331)?)?;
        append_bytes(&zero_epoch_path, &owned_header)?;
        append_bytes(
            &zero_epoch_path,
            &build_frame(LogFormat::V3, FrameKind::TransactionPageOwner, 1, 0, 1),
        )?;
        assert_v3_open_error_without_truncation::<10>(
            &zero_epoch_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(2)? + 24,
                FileFormatErrorReason::TransactionPageOwnerEpochZero,
            )),
        )?;

        let unallocated_epoch_path = directory.path().join("unallocated-epoch.bin");
        create_v3_with_allocated_epoch::<10>(&unallocated_epoch_path, persistent_id(337)?)?;
        append_bytes(&unallocated_epoch_path, &owned_header)?;
        append_bytes(
            &unallocated_epoch_path,
            &build_frame(LogFormat::V3, FrameKind::TransactionPageOwner, 1, 2, 1),
        )?;
        assert_v3_open_error_without_truncation::<10>(
            &unallocated_epoch_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(2)? + 24,
                FileFormatErrorReason::TransactionPageOwnerEpochUnallocated {
                    actual: 2,
                    highest_allocated: 1,
                },
            )),
        )?;

        let zero_sequence_path = directory.path().join("zero-sequence.bin");
        create_v3_with_allocated_epoch::<10>(&zero_sequence_path, persistent_id(347)?)?;
        append_bytes(&zero_sequence_path, &owned_header)?;
        append_bytes(
            &zero_sequence_path,
            &build_frame(LogFormat::V3, FrameKind::TransactionPageOwner, 1, 1, 0),
        )?;
        assert_v3_open_error_without_truncation::<10>(
            &zero_sequence_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(2)? + 32,
                FileFormatErrorReason::TransactionPageOwnerSequenceZero,
            )),
        )?;

        let duplicate_path = directory.path().join("duplicate-owner.bin");
        create_v3_with_allocated_epoch::<10>(&duplicate_path, persistent_id(349)?)?;
        append_bytes(&duplicate_path, &owned_header)?;
        append_bytes(&duplicate_path, &valid_owner)?;
        append_bytes(&duplicate_path, &valid_owner)?;
        assert_v3_open_error_without_truncation::<10>(
            &duplicate_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(3)? + 4,
                FileFormatErrorReason::TransactionPageOwnerDuplicate,
            )),
        )?;

        let raw_owner_path = directory.path().join("raw-owner.bin");
        create_v3_with_allocated_epoch::<10>(&raw_owner_path, persistent_id(353)?)?;
        append_bytes(
            &raw_owner_path,
            &build_frame(LogFormat::V3, FrameKind::PageHeader, 1, 4, 6),
        )?;
        append_bytes(&raw_owner_path, &valid_owner)?;
        assert_v3_open_error_without_truncation::<10>(
            &raw_owner_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(2)? + 4,
                FileFormatErrorReason::TransactionPageOwnerWithoutHeader,
            )),
        )?;
        Ok(())
    }

    #[test]
    fn v3_owned_data_corruption_never_truncates() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-owned-data-corruption")?;
        let owned_header = build_frame(LogFormat::V3, FrameKind::TransactionPageHeader, 1, 4, 6);
        let owner = build_frame(LogFormat::V3, FrameKind::TransactionPageOwner, 1, 1, 1);
        let first_data = build_frame_with_payload2_bytes(
            LogFormat::V3,
            FrameKind::PageData,
            1,
            0,
            [1, 2, 3, 4, 5, 6, 7, 8],
        );

        let interrupted_path = directory.path().join("interrupted.bin");
        create_v3_with_allocated_epoch::<10>(&interrupted_path, persistent_id(359)?)?;
        append_bytes(&interrupted_path, &owned_header)?;
        append_bytes(&interrupted_path, &owner)?;
        append_bytes(
            &interrupted_path,
            &build_frame(LogFormat::V3, FrameKind::TransactionPageHeader, 2, 5, 7),
        )?;
        assert_v3_open_error_without_truncation::<10>(
            &interrupted_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(3)? + 4,
                FileFormatErrorReason::PageDataInterruptedByFrameKind { actual: 6 },
            )),
        )?;

        let padding_path = directory.path().join("padding.bin");
        create_v3_with_allocated_epoch::<10>(&padding_path, persistent_id(367)?)?;
        append_bytes(&padding_path, &owned_header)?;
        append_bytes(&padding_path, &owner)?;
        append_bytes(&padding_path, &first_data)?;
        append_bytes(
            &padding_path,
            &build_frame_with_payload2_bytes(
                LogFormat::V3,
                FrameKind::PageData,
                1,
                1,
                [9, 10, 1, 0, 0, 0, 0, 0],
            ),
        )?;
        assert_v3_open_error_without_truncation::<10>(
            &padding_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(4)? + 32,
                FileFormatErrorReason::PageDataFinalPaddingNonzero,
            )),
        )?;

        let checksum_path = directory.path().join("checksum.bin");
        create_v3_with_allocated_epoch::<10>(&checksum_path, persistent_id(373)?)?;
        append_bytes(&checksum_path, &owned_header)?;
        append_bytes(&checksum_path, &owner)?;
        let mut corrupted_data = first_data;
        corrupted_data[FRAME_CHECKSUM_OFFSET] ^= 0xff;
        let expected_checksum = checksum_v1(&corrupted_data[..FRAME_CHECKSUM_OFFSET]);
        let actual_checksum = read_u64(&corrupted_data, FRAME_CHECKSUM_OFFSET);
        append_bytes(&checksum_path, &corrupted_data)?;
        assert_v3_open_error_without_truncation::<10>(
            &checksum_path,
            FileOpenError::Format(FileFormatError::new(
                wal_frame_offset(3)? + FRAME_CHECKSUM_OFFSET_U64,
                FileFormatErrorReason::FrameChecksum {
                    expected: expected_checksum,
                    actual: actual_checksum,
                },
            )),
        )?;
        Ok(())
    }

    #[test]
    fn v3_version_and_lock_checks_precede_scan_or_repair() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-version-boundary")?;
        let v3_path = directory.path().join("v3.bin");
        let v3 =
            FileCommitLog::<2>::create_new_transaction_page_capable(&v3_path, persistent_id(379)?)?;
        append_bytes(&v3_path, &[1, 2, 3])?;
        let v3_len = fs::metadata(&v3_path)?.len();

        let locked = FileCommitLog::<2>::open_transaction_page_capable(&v3_path)
            .err()
            .ok_or_else(|| io::Error::other("second v3 writer acquired the lock"))?;
        let FileOpenError::Io(locked) = locked else {
            return Err(io::Error::other("v3 lock contention was not I/O").into());
        };
        assert_eq!(locked.stage(), FileIoStage::AcquireExclusiveLock);
        assert_eq!(locked.io_source().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::metadata(&v3_path)?.len(), v3_len);
        drop(v3);

        assert_eq!(
            FileCommitLog::<2>::open_page_capable(&v3_path).err(),
            Some(FileOpenError::Format(FileFormatError::new(
                8,
                FileFormatErrorReason::HeaderVersion { actual: 3 },
            )))
        );
        assert_eq!(
            FileCommitLog::<0>::open(&v3_path).err(),
            Some(FileOpenError::Format(FileFormatError::new(
                8,
                FileFormatErrorReason::HeaderVersion { actual: 3 },
            )))
        );
        assert_eq!(
            FileCommitLog::<3>::open_transaction_page_capable(&v3_path).err(),
            Some(FileOpenError::Format(FileFormatError::new(
                HEADER_V2_PAGE_WIDTH_OFFSET as u64,
                FileFormatErrorReason::HeaderPageWidthMismatch {
                    expected: 3,
                    actual: 2,
                },
            )))
        );
        assert_eq!(fs::metadata(&v3_path)?.len(), v3_len);
        let repaired = FileCommitLog::<2>::open_transaction_page_capable(&v3_path)?;
        assert_eq!(fs::metadata(&v3_path)?.len(), HEADER_LENGTH_U64);
        drop(repaired);

        let v2_path = directory.path().join("v2.bin");
        let v2 = FileCommitLog::<2>::create_new_page_capable(&v2_path, persistent_id(383)?)?;
        drop(v2);
        append_bytes(&v2_path, &[4, 5, 6])?;
        let v2_len = fs::metadata(&v2_path)?.len();
        assert_eq!(
            FileCommitLog::<2>::open_transaction_page_capable(&v2_path).err(),
            Some(FileOpenError::Format(FileFormatError::new(
                8,
                FileFormatErrorReason::HeaderVersion { actual: 2 },
            )))
        );
        assert_eq!(fs::metadata(&v2_path)?.len(), v2_len);
        let repaired_v2 = FileCommitLog::<2>::open_page_capable(&v2_path)?;
        assert_eq!(fs::metadata(&v2_path)?.len(), HEADER_LENGTH_U64);
        drop(repaired_v2);
        assert_eq!(
            FileCommitLog::<0>::create_new_transaction_page_capable(
                directory.path().join("zero-v3.bin"),
                persistent_id(387)?,
            )
            .err(),
            Some(FileCreateError::PageWidth(FilePageWidthError::Zero))
        );
        Ok(())
    }

    #[test]
    fn v3_after_flush_commit_resolution_can_store_the_owned_page() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-after-flush-resolution")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let persistent_id = persistent_id(389)?;
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, persistent_id)?;
        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let active = coordinator.begin()?;
        let owner = active.transaction_id();
        let page = unlogged_page(log.lineage(), 11, 3, [2_u8, 4])?;
        let (active, dirty) = coordinator.stage_page_write(active, page, &mut log)?;
        log.arm_fault(FaultPoint::AfterFlush)?;

        let error = coordinator
            .commit(active, &mut log)
            .err()
            .ok_or_else(|| io::Error::other("v3 after-flush commit succeeded"))?;
        let CoordinatedCommitError::Indeterminate(error) = error else {
            return Err(io::Error::other("v3 after-flush commit was rejected").into());
        };
        let (indeterminate, cause) = error.into_parts();
        let CommitError::Flush { position, source } = cause else {
            return Err(io::Error::other("v3 after-flush returned a non-flush error").into());
        };
        assert_eq!(position.get(), 2);
        assert_eq!(
            source,
            FileCommitLogError::InjectedFault(FaultPoint::AfterFlush)
        );
        assert_eq!(log.durable_records().len(), 2);

        let resolution = coordinator.resolve(indeterminate, &mut log)?;
        let TransactionCommitResolution::Committed(committed) = resolution else {
            return Err(io::Error::other("durable v3 commit resolved as absent").into());
        };
        assert_eq!(committed.transaction_id(), owner);
        let clean = flush_committed_page(&committed, &mut log, &mut store, dirty)?;
        assert_eq!(clean.transaction_id(), owner);
        assert_eq!(
            store
                .page(page_number(11)?)
                .ok_or_else(|| io::Error::other("resolved v3 page was not stored"))?
                .bytes(),
            &[2, 4]
        );
        Ok(())
    }

    #[test]
    fn v3_committed_page_store_faults_preserve_terminal_physical_effects()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("v3-store-faults")?;

        let before_log_path = directory.path().join("before-log.bin");
        let before_store_path = directory.path().join("before-pages.bin");
        let before_id = persistent_id(397)?;
        let mut before_log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&before_log_path, before_id)?;
        let mut before_store = FilePageStore::<2>::create_new(&before_store_path, before_id)?;
        let mut before_coordinator = TransactionCoordinator::open(&mut before_log)?;
        let before_active = before_coordinator.begin()?;
        let before_owner = before_active.transaction_id();
        let before_page = unlogged_page(before_log.lineage(), 12, 4, [1_u8, 3])?;
        let (before_active, before_dirty) =
            before_coordinator.stage_page_write(before_active, before_page, &mut before_log)?;
        let before_committed = before_coordinator.commit(before_active, &mut before_log)?;
        before_store.arm_fault(PageStoreFaultPoint::BeforeWrite)?;
        let before_error = flush_committed_page(
            &before_committed,
            &mut before_log,
            &mut before_store,
            before_dirty,
        )
        .err()
        .ok_or_else(|| io::Error::other("v3 before-write store fault succeeded"))?;
        let TransactionCommittedFlushError::StoreWrite(before_error) = before_error else {
            return Err(io::Error::other("v3 before-write returned wrong error shape").into());
        };
        assert_eq!(before_error.transaction_id(), before_owner);
        assert_eq!(
            before_error.cause(),
            &FilePageStoreError::InjectedFault(PageStoreFaultPoint::BeforeWrite)
        );
        assert!(before_store.pages().is_empty());

        let after_log_path = directory.path().join("after-log.bin");
        let after_store_path = directory.path().join("after-pages.bin");
        let after_id = persistent_id(401)?;
        let mut after_log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&after_log_path, after_id)?;
        let mut after_store = FilePageStore::<2>::create_new(&after_store_path, after_id)?;
        let mut after_coordinator = TransactionCoordinator::open(&mut after_log)?;
        let after_active = after_coordinator.begin()?;
        let after_owner = after_active.transaction_id();
        let after_page = unlogged_page(after_log.lineage(), 13, 5, [2_u8, 8])?;
        let (after_active, after_dirty) =
            after_coordinator.stage_page_write(after_active, after_page, &mut after_log)?;
        let after_committed = after_coordinator.commit(after_active, &mut after_log)?;
        after_store.arm_fault(PageStoreFaultPoint::AfterWrite)?;
        let after_error = flush_committed_page(
            &after_committed,
            &mut after_log,
            &mut after_store,
            after_dirty,
        )
        .err()
        .ok_or_else(|| io::Error::other("v3 after-write store fault succeeded"))?;
        let TransactionCommittedFlushError::StoreWrite(after_error) = after_error else {
            return Err(io::Error::other("v3 after-write returned wrong error shape").into());
        };
        assert_eq!(after_error.transaction_id(), after_owner);
        assert_eq!(
            after_error.cause(),
            &FilePageStoreError::InjectedFault(PageStoreFaultPoint::AfterWrite)
        );
        assert_eq!(
            after_store
                .page(page_number(13)?)
                .ok_or_else(|| io::Error::other("v3 after-write lost the page"))?
                .bytes(),
            &[2, 8]
        );
        Ok(())
    }

    fn create_v3_with_allocated_epoch<const N: usize>(
        path: &Path,
        persistent_id: PersistentLogId,
    ) -> Result<(), Box<dyn Error>> {
        let mut log = FileCommitLog::<N>::create_new_transaction_page_capable(path, persistent_id)?;
        let coordinator = TransactionCoordinator::open(&mut log)?;
        drop(coordinator);
        drop(log);
        Ok(())
    }

    #[test]
    fn retention_ports_report_physical_high_water_sort_inventory_and_reject_poison()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("retention-ports")?;
        let persistent_id = persistent_id(409)?;
        let log_path = directory.path().join("wal.bin");
        let store_path = directory.path().join("pages.bin");
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&log_path, persistent_id)?;
        assert!(matches!(
            log.observe_restart_retention_metadata(),
            Err(FileTransactionRestartRetentionMetadataSourceError::NoAllocatedEpoch)
        ));
        let coordinator = TransactionCoordinator::open(&mut log)?;
        assert_eq!(coordinator.epoch().get(), 1);
        let metadata = log.observe_restart_retention_metadata()?;
        assert_eq!(metadata.allocated_epoch_high_water(), NonZeroU64::MIN);
        assert!(metadata.lineage().same_lineage(log.lineage()));
        assert_eq!(metadata.oldest_required_log_position(), None);
        log.next_epoch = Some(NonZeroU64::MAX);
        let (allocated, _) = log.allocate_transaction_epoch()?;
        assert_eq!(allocated, NonZeroU64::MAX);
        assert_eq!(
            log.observe_restart_retention_metadata()?
                .allocated_epoch_high_water(),
            NonZeroU64::MAX
        );

        let first = page_number(1)?;
        let second = page_number(2)?;
        let lineage = log.lineage().clone();
        let mut store = FilePageStore::<2>::create_new(&store_path, persistent_id)?;
        store.pages = vec![
            FileStoredPage {
                page_number: second,
                page_version: PageVersion::new(2),
                bytes: [2, 2],
                required_position: lineage.position(2),
                store_sequence: 2,
            },
            FileStoredPage {
                page_number: first,
                page_version: PageVersion::new(1),
                bytes: [1, 1],
                required_position: lineage.position(1),
                store_sequence: 1,
            },
        ];
        let inventory = store.durable_page_store_inventory()?;
        assert_eq!(
            inventory
                .iter()
                .map(StoredPageSnapshotObservation::page_number)
                .collect::<Vec<_>>(),
            [first, second]
        );
        assert_eq!(store.pages()[0].page_number(), second);
        assert_eq!(store.pages()[1].page_number(), first);

        store.poisoned = true;
        assert!(matches!(
            store.durable_page_store_inventory(),
            Err(FilePageStoreInventoryError::PoisonedWriter)
        ));
        log.poisoned = true;
        assert!(matches!(
            log.observe_restart_retention_metadata(),
            Err(FileTransactionRestartRetentionMetadataSourceError::PoisonedWriter)
        ));
        Ok(())
    }

    fn assert_v3_open_error_without_truncation<const N: usize>(
        path: &Path,
        expected: FileOpenError,
    ) -> Result<(), Box<dyn Error>> {
        let len_before = fs::metadata(path)?.len();
        let actual = FileCommitLog::<N>::open_transaction_page_capable(path)
            .err()
            .ok_or_else(|| io::Error::other("corrupted v3 file unexpectedly opened"))?;
        assert_eq!(actual, expected);
        assert_eq!(fs::metadata(path)?.len(), len_before);
        Ok(())
    }

    fn wal_frame(bytes: &[u8], index: usize) -> Result<[u8; FRAME_LENGTH], io::Error> {
        let start = HEADER_LENGTH
            .checked_add(
                index
                    .checked_mul(FRAME_LENGTH)
                    .ok_or_else(|| io::Error::other("frame index overflow"))?,
            )
            .ok_or_else(|| io::Error::other("frame offset overflow"))?;
        let end = start
            .checked_add(FRAME_LENGTH)
            .ok_or_else(|| io::Error::other("frame end overflow"))?;
        let source = bytes
            .get(start..end)
            .ok_or_else(|| io::Error::other("frame is outside the file bytes"))?;
        let mut frame = [0_u8; FRAME_LENGTH];
        frame.copy_from_slice(source);
        Ok(frame)
    }

    fn wal_frame_offset(index: usize) -> Result<u64, io::Error> {
        let index =
            u64::try_from(index).map_err(|_| io::Error::other("frame index does not fit u64"))?;
        HEADER_LENGTH_U64
            .checked_add(
                index
                    .checked_mul(FRAME_LENGTH_U64)
                    .ok_or_else(|| io::Error::other("frame offset overflow"))?,
            )
            .ok_or_else(|| io::Error::other("frame offset overflow"))
    }

    fn page_number(value: u64) -> Result<PageNumber, io::Error> {
        PageNumber::new(value).ok_or_else(|| io::Error::other("nonzero page number was rejected"))
    }

    fn page_image<const N: usize>(bytes: [u8; N]) -> Result<PageImage<N>, io::Error> {
        PageImage::new(bytes).map_err(io::Error::other)
    }

    fn persistent_id(value: u128) -> Result<PersistentLogId, io::Error> {
        PersistentLogId::new(value)
            .ok_or_else(|| io::Error::other("nonzero persistent ID rejected"))
    }

    fn unlogged_page<const N: usize>(
        lineage: &LogLineage,
        number: u64,
        version: u64,
        bytes: [u8; N],
    ) -> Result<ntsql_page::UnloggedPage<N>, io::Error> {
        Ok(ntsql_page::UnloggedPage::new(
            PageAddress::new(lineage, page_number(number)?),
            PageVersion::new(version),
            page_image(bytes)?,
        ))
    }

    fn append_bytes(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
        let mut file = OpenOptions::new().append(true).open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    fn write_exact_bytes(path: &Path, offset: usize, bytes: &[u8]) -> Result<(), io::Error> {
        let mut file = OpenOptions::new().write(true).open(path)?;
        let offset = u64::try_from(offset)
            .map_err(|_| io::Error::other("file offset does not fit in u64"))?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    fn flip_byte(path: &Path, offset: usize) -> Result<(), io::Error> {
        let mut bytes = fs::read(path)?;
        let byte = bytes
            .get_mut(offset)
            .ok_or_else(|| io::Error::other("flip offset is out of range"))?;
        *byte ^= 0xff;
        write_exact_bytes(path, 0, &bytes)
    }

    fn commit_cause(
        result: Result<
            ntsql_transaction::CommittedTransaction,
            CoordinatedCommitError<FileCommitLogError>,
        >,
    ) -> Result<CommitError<FileCommitLogError>, Box<dyn Error>> {
        Ok(indeterminate_parts(result)?.1)
    }

    fn indeterminate_parts(
        result: Result<
            ntsql_transaction::CommittedTransaction,
            CoordinatedCommitError<FileCommitLogError>,
        >,
    ) -> Result<(IndeterminateTransaction, CommitError<FileCommitLogError>), Box<dyn Error>> {
        let error = result
            .err()
            .ok_or_else(|| io::Error::other("faulted commit unexpectedly succeeded"))?;
        match error {
            CoordinatedCommitError::Indeterminate(error) => Ok(error.into_parts()),
            CoordinatedCommitError::Rejected(_) => {
                Err(io::Error::other("commit was rejected before WAL work").into())
            }
        }
    }

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(prefix: &str) -> Result<Self, io::Error> {
            let root = match std::env::var_os("CARGO_TARGET_TMPDIR") {
                Some(path) => PathBuf::from(path),
                None => std::env::current_dir()?
                    .join("target")
                    .join("ntsql-storage-file-tests"),
            };
            fs::create_dir_all(&root)?;

            let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("{prefix}-{}-{unique}", std::process::id()));
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

    // -----------------------------------------------------------------------
    // FilePageStore tests
    // -----------------------------------------------------------------------

    use ntsql_page::{
        CleanPage, DirtyPage, FlushDirtyPageError, FlushDirtyPageRejectionReason, flush_dirty_page,
    };

    fn write_page_through_flush<const N: usize>(
        log: &mut FileCommitLog<N>,
        store: &mut FilePageStore<N>,
        dirty: DirtyPage<N>,
    ) -> Result<CleanPage<N>, Box<dyn Error>> {
        let clean = flush_dirty_page(log, store, dirty)
            .map_err(|e| io::Error::other(format!("flush_dirty_page failed: {e}")))?;
        Ok(clean)
    }

    struct ZeroPositionPageLog {
        lineage: LogLineage,
    }

    impl LogDurability for ZeroPositionPageLog {
        type Error = std::convert::Infallible;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn flush_through(&mut self, _position: &LogSequenceNumber) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    impl<const N: usize> PageLog<N> for ZeroPositionPageLog {
        fn append_page(
            &mut self,
            _page: &UnloggedPage<N>,
        ) -> Result<LogSequenceNumber, Self::Error> {
            Ok(self.lineage.position(0))
        }
    }

    #[test]
    fn page_store_header_exact_bytes_and_checksum() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-header")?;
        let path = directory.path().join("pages.bin");
        let pid = persistent_id(200)?;

        let store = FilePageStore::<10>::create_new(&path, pid)?;
        assert_eq!(store.persistent_id(), pid);
        assert!(store.pages().is_empty());
        drop(store);

        let bytes = fs::read(&path)?;
        assert_eq!(bytes.len(), HEADER_LENGTH);
        assert_eq!(&bytes[..8], b"NTSQPGS1");
        assert_eq!(read_u16(&bytes, 8), 1); // version
        assert_eq!(read_u16(&bytes, 10), 64); // header length
        assert_eq!(read_u32(&bytes, 12), 0); // flags
        assert_eq!(read_u128(&bytes, 16), pid.get()); // persistent ID
        assert_eq!(read_u64(&bytes, 32), 10); // page width N=10
        // 40..56 reserved zero
        for byte in &bytes[40..56] {
            assert_eq!(*byte, 0, "reserved byte should be zero");
        }
        assert_eq!(read_u64(&bytes, 56), 0xec25_2b5e_1c3d_f64d);

        Ok(())
    }

    #[test]
    fn page_store_integration_with_commit_log_v2() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-integration")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(201)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;

        // stage_page_write -> flush_dirty_page -> FilePageStore
        let unlogged = unlogged_page(log.lineage(), 1, 5, [0xAA, 0xBB])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        let required_pos = dirty.required_position().get();
        assert_eq!(required_pos, 1);

        let clean = write_page_through_flush(&mut log, &mut store, dirty)?;
        assert_eq!(clean.address().number().get(), 1);
        assert_eq!(clean.version().get(), 5);
        assert_eq!(*clean.image().bytes(), [0xAA, 0xBB]);
        assert_eq!(clean.required_position().get(), 1);

        // Inspect store
        assert_eq!(store.pages().len(), 1);
        let snapshot = store.page(page_number(1)?);
        assert!(snapshot.is_some());
        let snapshot = snapshot.ok_or_else(|| io::Error::other("missing page"))?;
        assert_eq!(snapshot.page_number().get(), 1);
        assert_eq!(snapshot.page_version().get(), 5);
        assert_eq!(*snapshot.bytes(), [0xAA, 0xBB]);
        assert_eq!(
            snapshot.required_position(),
            &log.lineage().position(required_pos)
        );
        assert_eq!(snapshot.store_sequence(), 1);

        // Drop and reopen both
        drop(store);
        drop(log);

        let mut log = FileCommitLog::<2>::open_page_capable(&log_path)?;
        let mut store = FilePageStore::<2>::open(&store_path)?;
        assert_eq!(store.pages().len(), 1);
        let snapshot = store
            .page(page_number(1)?)
            .ok_or_else(|| io::Error::other("missing page after reopen"))?;
        assert_eq!(*snapshot.bytes(), [0xAA, 0xBB]);
        assert_eq!(
            snapshot.required_position(),
            &log.lineage().position(required_pos)
        );
        assert_eq!(snapshot.store_sequence(), 1);

        // Append after reopen proving sequence high-water
        let unlogged2 = unlogged_page(log.lineage(), 2, 10, [0xCC, 0xDD])?;
        let dirty2 =
            stage_page_write(&mut log, unlogged2).map_err(|e| io::Error::other(format!("{e}")))?;
        let clean2 = write_page_through_flush(&mut log, &mut store, dirty2)?;
        assert_eq!(clean2.required_position().get(), 2);
        assert_eq!(store.pages().len(), 2);
        let snapshot2 = store
            .page(page_number(2)?)
            .ok_or_else(|| io::Error::other("missing page 2"))?;
        assert_eq!(snapshot2.store_sequence(), 2);

        Ok(())
    }

    #[test]
    fn projected_file_evidence_reconciles_current_behind_and_missing() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new("ps-reconciliation")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let empty_store_path = directory.path().join("empty-pages.bin");
        let pid = persistent_id(252)?;
        let number = page_number(7)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;
        let first = unlogged_page(log.lineage(), 7, 11, [1_u8, 2])?;
        let first_dirty = stage_page_write(&mut log, first)
            .map_err(|error| io::Error::other(format!("{error}")))?;
        write_page_through_flush(&mut log, &mut store, first_dirty)?;
        drop(store);
        drop(log);

        let mut log = FileCommitLog::<2>::open_page_capable(&log_path)?;
        let store = FilePageStore::<2>::open(&store_path)?;
        let first_record = log
            .durable_records()
            .next()
            .ok_or_else(|| io::Error::other("reopened first page record is missing"))?
            .page_recovery_observation()?
            .ok_or_else(|| io::Error::other("first page record projected as transaction"))?;
        let first_snapshot = store
            .page(number)
            .ok_or_else(|| io::Error::other("reopened first snapshot is missing"))?
            .page_recovery_observation()?;
        assert_eq!(
            reconcile_durable_page(
                log.lineage(),
                number,
                Some(&first_snapshot),
                std::iter::once(&first_record),
            )?,
            DurablePageReconciliation::ExactCurrent {
                durable_position: log.lineage().position(1),
            }
        );

        let mut coordinator = TransactionCoordinator::open(&mut log)?;
        let transaction = coordinator.begin()?;
        let commit = coordinator.commit(transaction, &mut log)?;
        assert_eq!(commit.log_position().get(), 2);
        let second = unlogged_page(log.lineage(), 7, 4, [3_u8, 4])?;
        let second_dirty = stage_page_write(&mut log, second)
            .map_err(|error| io::Error::other(format!("{error}")))?;
        log.flush_through(second_dirty.required_position())?;
        assert_eq!(second_dirty.required_position().get(), 3);
        drop(second_dirty);
        drop(store);
        drop(log);

        let log = FileCommitLog::<2>::open_page_capable(&log_path)?;
        let store = FilePageStore::<2>::open(&store_path)?;
        assert_eq!(log.records().len(), 3);
        let mut durable_records = log.durable_records();
        let first_record = durable_records
            .next()
            .ok_or_else(|| io::Error::other("reopened durable prefix lost its first record"))?
            .page_recovery_observation()?
            .ok_or_else(|| io::Error::other("first reopened record lost page projection"))?;
        let transaction_record = durable_records.next().ok_or_else(|| {
            io::Error::other("reopened durable prefix lost its transaction record")
        })?;
        assert!(transaction_record.page_recovery_observation()?.is_none());
        let second_record = durable_records
            .next()
            .ok_or_else(|| io::Error::other("reopened durable prefix lost its second page record"))?
            .page_recovery_observation()?
            .ok_or_else(|| io::Error::other("second reopened record lost page projection"))?;
        assert!(durable_records.next().is_none());
        let observations = [first_record, second_record];
        let snapshot = store
            .page(number)
            .ok_or_else(|| io::Error::other("stored snapshot disappeared"))?
            .page_recovery_observation()?;
        assert_eq!(
            reconcile_durable_page(log.lineage(), number, Some(&snapshot), observations.iter(),)?,
            DurablePageReconciliation::StoreBehind {
                stored_position: log.lineage().position(1),
                latest_durable_position: log.lineage().position(3),
            }
        );

        let empty_store = FilePageStore::<2>::create_new(&empty_store_path, pid)?;
        let empty_snapshot = empty_store
            .page(number)
            .map(|page| page.page_recovery_observation())
            .transpose()?;
        assert!(empty_snapshot.is_none());
        assert_eq!(
            reconcile_durable_page(
                log.lineage(),
                number,
                empty_snapshot.as_ref(),
                observations.iter(),
            )?,
            DurablePageReconciliation::StoreMissing {
                latest_durable_position: log.lineage().position(3),
            }
        );
        Ok(())
    }

    #[test]
    fn page_store_rewrite_same_page_appends_and_latest_wins() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-rewrite")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(202)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;

        // Write page 1 version 1
        let unlogged1 = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty1 =
            stage_page_write(&mut log, unlogged1).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty1)?;

        // Write page 1 version 2 (rewrite)
        let unlogged2 = unlogged_page(log.lineage(), 1, 2, [0x03, 0x04])?;
        let dirty2 =
            stage_page_write(&mut log, unlogged2).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty2)?;

        // Latest wins
        assert_eq!(store.pages().len(), 1);
        let snapshot = store
            .page(page_number(1)?)
            .ok_or_else(|| io::Error::other("missing page"))?;
        assert_eq!(snapshot.page_version().get(), 2);
        assert_eq!(*snapshot.bytes(), [0x03, 0x04]);
        assert_eq!(snapshot.store_sequence(), 2);

        // Reopen
        drop(store);
        let store = FilePageStore::<2>::open(&store_path)?;
        assert_eq!(store.pages().len(), 1);
        let snapshot = store
            .page(page_number(1)?)
            .ok_or_else(|| io::Error::other("missing page after reopen"))?;
        assert_eq!(snapshot.page_version().get(), 2);
        assert_eq!(*snapshot.bytes(), [0x03, 0x04]);
        assert_eq!(snapshot.store_sequence(), 2);

        Ok(())
    }

    #[test]
    fn page_store_width_zero_rejects_before_mutation() -> Result<(), Box<dyn Error>> {
        let pid = persistent_id(250)?;
        let result = FilePageStore::<0>::create_new("should-not-exist.bin", pid);
        assert!(
            matches!(
                result,
                Err(PageStoreCreateError::PageWidth(FilePageWidthError::Zero))
            ),
            "expected PageWidth(Zero) error"
        );
        Ok(())
    }

    #[test]
    fn page_store_width_mismatch_rejects_before_mutation() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-width-mismatch")?;
        let path = directory.path().join("pages.bin");
        let pid = persistent_id(203)?;

        let store = FilePageStore::<10>::create_new(&path, pid)?;
        drop(store);
        append_bytes(&path, &[1, 2, 3])?;
        let len_before = fs::metadata(&path)?.len();

        // Try to open with width 5 instead of 10
        let error = FilePageStore::<5>::open(&path)
            .err()
            .ok_or_else(|| io::Error::other("width mismatch accepted"))?;
        match error {
            PageStoreOpenError::Format(ref e) => {
                assert!(matches!(
                    e.reason(),
                    PageStoreFormatErrorReason::HeaderPageWidthMismatch {
                        expected: 5,
                        actual: 10
                    }
                ));
            }
            _ => return Err(io::Error::other("wrong error type for width mismatch").into()),
        }
        assert_eq!(fs::metadata(&path)?.len(), len_before);
        Ok(())
    }

    #[test]
    fn page_store_foreign_page_lineage_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-foreign-page")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid1 = persistent_id(204)?;
        let pid2 = persistent_id(205)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid1)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid2)?;
        store.arm_fault(PageStoreFaultPoint::AfterWrite)?;
        let len_before = fs::metadata(&store_path)?.len();

        let unlogged = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;

        let error = flush_dirty_page(&mut log, &mut store, dirty)
            .err()
            .ok_or_else(|| io::Error::other("foreign store unexpectedly accepted the page"))?;
        let FlushDirtyPageError::Rejected(rejection) = error else {
            return Err(io::Error::other("foreign store reached the write port").into());
        };
        assert_eq!(
            rejection.reason(),
            FlushDirtyPageRejectionReason::ForeignStore
        );
        assert_eq!(store.armed_fault(), Some(PageStoreFaultPoint::AfterWrite));
        assert!(store.pages().is_empty());
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_zero_required_position_rejects_before_mutation() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-zero-required-position")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(251)?;
        let lineage = LogLineage::persistent(pid);
        let mut log = ZeroPositionPageLog {
            lineage: lineage.clone(),
        };
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;
        store.arm_fault(PageStoreFaultPoint::AfterWrite)?;
        let len_before = fs::metadata(&store_path)?.len();
        let unlogged = unlogged_page(&lineage, 1, 1, [0x01, 0x02])?;
        let dirty = stage_page_write(&mut log, unlogged)?;

        let error = flush_dirty_page(&mut log, &mut store, dirty)
            .err()
            .ok_or_else(|| io::Error::other("zero required position unexpectedly persisted"))?;
        let FlushDirtyPageError::StoreWrite(error) = error else {
            return Err(io::Error::other("zero required position returned the wrong error").into());
        };
        assert_eq!(
            error.cause(),
            &FilePageStoreError::RequiredPositionZero(page_number(1)?)
        );
        assert_eq!(store.armed_fault(), Some(PageStoreFaultPoint::AfterWrite));
        assert!(store.pages().is_empty());
        assert!(!store.is_poisoned());
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        drop(store);
        let reopened = FilePageStore::<2>::open(&store_path)?;
        assert!(reopened.pages().is_empty());
        Ok(())
    }

    #[test]
    fn page_store_before_write_fault_returns_before_mutation() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-before-write")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(206)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;

        store.arm_fault(PageStoreFaultPoint::BeforeWrite)?;

        let unlogged = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;

        let error = flush_dirty_page(&mut log, &mut store, dirty)
            .err()
            .ok_or_else(|| io::Error::other("before-write fault unexpectedly succeeded"))?;
        let FlushDirtyPageError::StoreWrite(error) = error else {
            return Err(io::Error::other("before-write fault returned the wrong error").into());
        };
        assert_eq!(
            error.cause(),
            &FilePageStoreError::InjectedFault(PageStoreFaultPoint::BeforeWrite)
        );

        assert!(store.pages().is_empty());
        assert!(!store.is_poisoned());
        assert_eq!(fs::metadata(&store_path)?.len(), HEADER_LENGTH_U64);

        Ok(())
    }

    #[test]
    fn page_store_after_write_fault_writes_and_updates_state_then_reports_error()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-after-write")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(207)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;

        store.arm_fault(PageStoreFaultPoint::AfterWrite)?;

        let unlogged = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;

        let error = flush_dirty_page(&mut log, &mut store, dirty)
            .err()
            .ok_or_else(|| io::Error::other("after-write fault unexpectedly succeeded"))?;
        let FlushDirtyPageError::StoreWrite(error) = error else {
            return Err(io::Error::other("after-write fault returned the wrong error").into());
        };
        assert_eq!(
            error.cause(),
            &FilePageStoreError::InjectedFault(PageStoreFaultPoint::AfterWrite)
        );

        assert_eq!(store.pages().len(), 1);
        let snapshot = store
            .page(page_number(1)?)
            .ok_or_else(|| io::Error::other("missing page after AfterWrite"))?;
        assert_eq!(*snapshot.bytes(), [0x01, 0x02]);
        assert!(!store.is_poisoned());

        // Reopen should work
        drop(store);
        let store = FilePageStore::<2>::open(&store_path)?;
        assert_eq!(store.pages().len(), 1);

        Ok(())
    }

    #[test]
    fn page_store_incomplete_physical_frame_repaired_on_open() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-incomplete-physical")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(208)?;

        let store = FilePageStore::<2>::create_new(&store_path, pid)?;
        drop(store);

        // Append partial frame bytes (less than 56)
        append_bytes(&store_path, &[1, 2, 3])?;
        let len_before = fs::metadata(&store_path)?.len();
        assert_eq!(len_before, HEADER_LENGTH_U64 + 3);

        // Reopen should repair back to header
        let store = FilePageStore::<2>::open(&store_path)?;
        assert!(store.pages().is_empty());
        assert_eq!(fs::metadata(&store_path)?.len(), HEADER_LENGTH_U64);
        drop(store);
        Ok(())
    }

    #[test]
    fn page_store_incomplete_logical_group_at_header_repaired() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-incomplete-group-header")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(209)?;

        // Write one complete page, then simulate interrupted second write
        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;

        let unlogged = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty)?;

        let len_after_first = fs::metadata(&store_path)?.len();
        drop(store);

        // Append just a snapshot-header frame for page 2 (incomplete group)
        let header_frame = build_page_store_frame(
            PageStoreFrameKind::SnapshotHeader,
            2, // sequence
            2, // page number
            0, // version
        );
        append_bytes(&store_path, &header_frame)?;

        // Reopen should truncate the incomplete group
        let store = FilePageStore::<2>::open(&store_path)?;
        assert_eq!(store.pages().len(), 1);
        assert_eq!(fs::metadata(&store_path)?.len(), len_after_first);
        let snapshot = store
            .page(page_number(1)?)
            .ok_or_else(|| io::Error::other("missing page after repair"))?;
        assert_eq!(*snapshot.bytes(), [0x01, 0x02]);

        Ok(())
    }

    #[test]
    fn page_store_incomplete_logical_group_after_required_position_repaired()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-incomplete-group-reqpos")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(210)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;

        // Write one complete page
        let unlogged = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty)?;

        let len_after_first = fs::metadata(&store_path)?.len();
        drop(store);

        // Append snapshot-header + required-position (but no data frames)
        let header_frame = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 2, 2, 0);
        let req_frame = build_page_store_frame(PageStoreFrameKind::RequiredPosition, 2, 42, 0);
        append_bytes(&store_path, &header_frame)?;
        append_bytes(&store_path, &req_frame)?;

        // Reopen should truncate back to end of first group
        let store = FilePageStore::<2>::open(&store_path)?;
        assert_eq!(store.pages().len(), 1);
        assert_eq!(fs::metadata(&store_path)?.len(), len_after_first);

        Ok(())
    }

    #[test]
    fn page_store_wrong_kind_rejects_without_truncation() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-wrong-kind")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(211)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;

        let unlogged = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty)?;
        drop(store);

        // Write a complete valid snapshot-header, but follow with another snapshot-header
        // instead of required-position (wrong kind)
        let header_frame = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 2, 2, 0);
        let wrong_frame = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 2, 3, 0);
        append_bytes(&store_path, &header_frame)?;
        append_bytes(&store_path, &wrong_frame)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<2>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("wrong kind accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_sequence_replay_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-replay")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(212)?;

        let store = FilePageStore::<2>::create_new(&store_path, pid)?;
        drop(store);

        // Write sequence 1 group completely
        let hdr = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 1, 1, 0);
        let req = build_page_store_frame(PageStoreFrameKind::RequiredPosition, 1, 1, 0);
        let data = build_page_store_frame(PageStoreFrameKind::PageData, 1, 0, 0);
        append_bytes(&store_path, &hdr)?;
        append_bytes(&store_path, &req)?;
        append_bytes(&store_path, &data)?;

        // Then replay sequence 1 again
        append_bytes(&store_path, &hdr)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<2>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("sequence replay accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_sequence_exhaustion_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-sequence-exhausted")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(224)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;
        store.next_sequence = None;

        let unlogged = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        let error = flush_dirty_page(&mut log, &mut store, dirty)
            .err()
            .ok_or_else(|| io::Error::other("sequence exhaustion unexpectedly wrote"))?;
        let ntsql_page::FlushDirtyPageError::StoreWrite(store_error) = error else {
            return Err(io::Error::other("expected store write failure").into());
        };
        assert_eq!(
            store_error.cause(),
            &FilePageStoreError::StoreSequenceSpaceExhausted
        );
        assert!(store.pages().is_empty());
        assert_eq!(fs::metadata(&store_path)?.len(), HEADER_LENGTH_U64);
        assert!(!store.is_poisoned());

        Ok(())
    }

    #[test]
    fn page_store_sequence_exhaustion_on_open() -> Result<(), Box<dyn Error>> {
        let pid = persistent_id(225)?;
        let lineage = LogLineage::persistent(pid);
        let layout = PageLayout::for_const::<2>().map_err(io::Error::other)?;
        let mut state = PageStoreOpenState::<2>::new(layout, lineage.clone());
        state.next_sequence = Some(u64::MAX);

        let header_offset = HEADER_LENGTH_U64;
        let header = parse_page_store_frame(
            &build_page_store_frame(PageStoreFrameKind::SnapshotHeader, u64::MAX, 1, 7),
            header_offset,
        )
        .map_err(io::Error::other)?;
        state.apply_frame(header, header_offset)?;

        let required_position_offset = header_offset + FRAME_LENGTH_U64;
        let required_position = parse_page_store_frame(
            &build_page_store_frame(PageStoreFrameKind::RequiredPosition, u64::MAX, 11, 0),
            required_position_offset,
        )
        .map_err(io::Error::other)?;
        state.apply_frame(required_position, required_position_offset)?;

        let data_offset = required_position_offset + FRAME_LENGTH_U64;
        let data = parse_page_store_frame(
            &build_page_store_frame(
                PageStoreFrameKind::PageData,
                u64::MAX,
                0,
                0x0102_0000_0000_0000,
            ),
            data_offset,
        )
        .map_err(io::Error::other)?;
        state.apply_frame(data, data_offset)?;

        assert!(state.next_sequence.is_none());
        assert_eq!(state.pages.len(), 1);
        assert_eq!(state.pages[0].store_sequence(), u64::MAX);
        assert_eq!(state.pages[0].required_position().get(), 11);
        assert!(
            state
                .lineage
                .same_lineage(state.pages[0].required_position().lineage())
        );
        let extra_offset = data_offset + FRAME_LENGTH_U64;
        let extra_header = parse_page_store_frame(
            &build_page_store_frame(PageStoreFrameKind::SnapshotHeader, u64::MAX, 2, 8),
            extra_offset,
        )
        .map_err(io::Error::other)?;
        let extra_error = state
            .apply_frame(extra_header, extra_offset)
            .err()
            .ok_or_else(|| io::Error::other("sequence after u64::MAX unexpectedly opened"))?;
        assert_eq!(
            extra_error,
            PageStoreOpenError::Format(PageStoreFormatError::new(
                extra_offset + 16,
                PageStoreFormatErrorReason::SnapshotSequenceSpaceExhausted,
            ))
        );

        let directory = TestDirectory::new("ps-sequence-exhausted-open")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;
        store.next_sequence = state.next_sequence;

        let unlogged = unlogged_page(log.lineage(), 2, 1, [0x03, 0x04])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        let error = flush_dirty_page(&mut log, &mut store, dirty)
            .err()
            .ok_or_else(|| io::Error::other("sequence exhaustion unexpectedly wrote"))?;
        let ntsql_page::FlushDirtyPageError::StoreWrite(store_error) = error else {
            return Err(io::Error::other("expected store write failure").into());
        };
        assert_eq!(
            store_error.cause(),
            &FilePageStoreError::StoreSequenceSpaceExhausted
        );

        Ok(())
    }

    #[test]
    fn page_store_parent_sequence_mismatch_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-parent-mismatch")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(213)?;

        let store = FilePageStore::<2>::create_new(&store_path, pid)?;
        drop(store);

        // Snapshot header with sequence 1, required-position with wrong sequence
        let hdr = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 1, 1, 0);
        let req = build_page_store_frame(PageStoreFrameKind::RequiredPosition, 99, 1, 0);
        append_bytes(&store_path, &hdr)?;
        append_bytes(&store_path, &req)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<2>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("parent mismatch accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_data_chunk_index_mismatch_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-chunk-index")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(214)?;

        let store = FilePageStore::<2>::create_new(&store_path, pid)?;
        drop(store);

        let hdr = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 1, 1, 0);
        let req = build_page_store_frame(PageStoreFrameKind::RequiredPosition, 1, 1, 0);
        // Data with chunk_index=1 instead of 0
        let data = build_page_store_frame(PageStoreFrameKind::PageData, 1, 1, 0);
        append_bytes(&store_path, &hdr)?;
        append_bytes(&store_path, &req)?;
        append_bytes(&store_path, &data)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<2>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("chunk index mismatch accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_nonzero_padding_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-nonzero-padding")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(215)?;

        // N=3 => final chunk has 3 bytes, padding 5 bytes
        let store = FilePageStore::<3>::create_new(&store_path, pid)?;
        drop(store);

        let hdr = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 1, 1, 0);
        let req = build_page_store_frame(PageStoreFrameKind::RequiredPosition, 1, 1, 0);
        // Data with nonzero padding: byte[3..8] should be zero but we put nonzero
        let bad_chunk: [u8; 8] = [0x01, 0x02, 0x03, 0xFF, 0x00, 0x00, 0x00, 0x00];
        let data = build_page_store_frame_with_payload2_bytes(
            PageStoreFrameKind::PageData,
            1,
            0,
            bad_chunk,
        );
        append_bytes(&store_path, &hdr)?;
        append_bytes(&store_path, &req)?;
        append_bytes(&store_path, &data)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<3>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("nonzero padding accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_checksum_corruption_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-checksum")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(216)?;

        let mut log = FileCommitLog::<2>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<2>::create_new(&store_path, pid)?;

        let unlogged = unlogged_page(log.lineage(), 1, 1, [0x01, 0x02])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty)?;
        drop(store);

        // Corrupt a byte in the first frame (snapshot header)
        let corrupt_offset = HEADER_LENGTH + 20; // inside payload
        flip_byte(&store_path, corrupt_offset)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<2>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("checksum corruption accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_required_position_zero_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-reqpos-zero")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(217)?;

        let store = FilePageStore::<2>::create_new(&store_path, pid)?;
        drop(store);

        let hdr = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 1, 1, 0);
        let req = build_page_store_frame(PageStoreFrameKind::RequiredPosition, 1, 0, 0);
        append_bytes(&store_path, &hdr)?;
        append_bytes(&store_path, &req)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<2>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("required position zero accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_nonzero_reserved_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-nonzero-reserved")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(218)?;

        let store = FilePageStore::<2>::create_new(&store_path, pid)?;
        drop(store);

        // Build a frame manually with nonzero reserved bytes (40..48)
        let mut frame = [0_u8; FRAME_LENGTH];
        frame[..4].copy_from_slice(&PAGE_STORE_FRAME_MAGIC);
        write_u16(&mut frame, 4, PageStoreFrameKind::SnapshotHeader.code());
        write_u16(&mut frame, 6, PAGE_STORE_FORMAT_VERSION);
        write_u32(&mut frame, 8, 0);
        write_u16(&mut frame, 12, FRAME_LENGTH_U16);
        write_u16(&mut frame, 14, 0);
        write_u64(&mut frame, 16, 1); // sequence
        write_u64(&mut frame, 24, 1); // page number
        write_u64(&mut frame, 32, 0); // version
        frame[40] = 0xFF; // nonzero reserved
        let checksum = checksum_v1(&frame[..FRAME_CHECKSUM_OFFSET]);
        write_u64(&mut frame, FRAME_CHECKSUM_OFFSET, checksum);

        append_bytes(&store_path, &frame)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<2>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("nonzero reserved accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }

    #[test]
    fn page_store_exclusive_lock_blocks_reopen_and_releases_on_drop() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new("ps-lock")?;
        let path = directory.path().join("pages.bin");
        let pid = persistent_id(219)?;

        let store = FilePageStore::<2>::create_new(&path, pid)?;
        append_bytes(&path, &[1, 2, 3])?;
        let bytes_before_open = fs::read(&path)?;

        let error = FilePageStore::<2>::open(&path)
            .err()
            .ok_or_else(|| io::Error::other("second writer acquired the file lock"))?;
        let PageStoreOpenError::Io(source) = error else {
            return Err(io::Error::other("lock contention not I/O").into());
        };
        assert_eq!(source.stage(), PageStoreIoStage::AcquireExclusiveLock);
        assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read(&path)?, bytes_before_open);

        drop(store);
        let reopened = FilePageStore::<2>::open(&path)?;
        assert_eq!(reopened.persistent_id(), pid);
        assert_eq!(fs::metadata(&path)?.len(), HEADER_LENGTH_U64);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn page_store_hard_link_lock_exclusion() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-hard-link")?;
        let path = directory.path().join("pages.bin");
        let alias = directory.path().join("pages-alias.bin");
        let pid = persistent_id(220)?;

        let store = FilePageStore::<2>::create_new(&path, pid)?;
        fs::hard_link(&path, &alias)?;

        let error = FilePageStore::<2>::open(&alias)
            .err()
            .ok_or_else(|| io::Error::other("hard-link alias bypassed file lock"))?;
        let PageStoreOpenError::Io(source) = error else {
            return Err(io::Error::other("alias lock not I/O").into());
        };
        assert_eq!(source.stage(), PageStoreIoStage::AcquireExclusiveLock);
        assert_eq!(source.io_source().kind(), io::ErrorKind::WouldBlock);

        drop(store);
        let reopened = FilePageStore::<2>::open(&alias)?;
        assert_eq!(reopened.persistent_id(), pid);
        Ok(())
    }

    #[test]
    fn page_store_snapshot_header_and_data_frame_golden_bytes() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-golden-bytes")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(221)?;

        let mut log = FileCommitLog::<10>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<10>::create_new(&store_path, pid)?;

        let unlogged = unlogged_page(
            log.lineage(),
            1,
            5,
            [0xAA, 0xBB, 1, 2, 3, 4, 5, 6, 0xCC, 0xDD],
        )?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty)?;
        drop(store);

        let bytes = fs::read(&store_path)?;
        assert_eq!(bytes.len(), HEADER_LENGTH + FRAME_LENGTH * 4);

        let mut frame = [0_u8; FRAME_LENGTH];

        let header_start = HEADER_LENGTH;
        frame.copy_from_slice(&bytes[header_start..header_start + FRAME_LENGTH]);
        assert_eq!(&frame[..4], b"NTSP");
        assert_eq!(read_u16(&frame, 4), 1);
        assert_eq!(read_u16(&frame, 6), 1);
        assert_eq!(read_u64(&frame, 16), 1);
        assert_eq!(read_u64(&frame, 24), 1);
        assert_eq!(read_u64(&frame, 32), 5);
        assert_eq!(
            read_u64(&frame, FRAME_CHECKSUM_OFFSET),
            0x410f_8838_f683_3c71
        );

        let required_position_start = HEADER_LENGTH + FRAME_LENGTH;
        frame.copy_from_slice(
            &bytes[required_position_start..required_position_start + FRAME_LENGTH],
        );
        assert_eq!(&frame[..4], b"NTSP");
        assert_eq!(read_u16(&frame, 4), 2);
        assert_eq!(read_u64(&frame, 16), 1);
        assert_eq!(read_u64(&frame, 24), 1);
        assert_eq!(read_u64(&frame, 32), 0);
        assert_eq!(
            read_u64(&frame, FRAME_CHECKSUM_OFFSET),
            0x666a_2ca3_47af_4883
        );

        let first_data_start = HEADER_LENGTH + FRAME_LENGTH * 2;
        let mut first_data_frame = [0_u8; FRAME_LENGTH];
        first_data_frame.copy_from_slice(&bytes[first_data_start..first_data_start + FRAME_LENGTH]);
        assert_eq!(&first_data_frame[..4], b"NTSP");
        assert_eq!(read_u16(&first_data_frame, 4), 3);
        assert_eq!(read_u64(&first_data_frame, 16), 1);
        assert_eq!(read_u64(&first_data_frame, 24), 0);
        assert_eq!(&first_data_frame[32..40], &[0xAA, 0xBB, 1, 2, 3, 4, 5, 6]);
        assert_eq!(
            read_u64(&first_data_frame, FRAME_CHECKSUM_OFFSET),
            0xe9ea_8e28_a88d_17d7
        );

        let final_data_start = HEADER_LENGTH + FRAME_LENGTH * 3;
        let mut final_data_frame = [0_u8; FRAME_LENGTH];
        final_data_frame.copy_from_slice(&bytes[final_data_start..final_data_start + FRAME_LENGTH]);
        assert_eq!(&final_data_frame[..4], b"NTSP");
        assert_eq!(read_u16(&final_data_frame, 4), 3);
        assert_eq!(read_u64(&final_data_frame, 16), 1);
        assert_eq!(read_u64(&final_data_frame, 24), 1);
        assert_eq!(&final_data_frame[32..40], &[0xCC, 0xDD, 0, 0, 0, 0, 0, 0]);
        assert_ne!(first_data_frame, final_data_frame);
        assert_eq!(
            read_u64(&final_data_frame, FRAME_CHECKSUM_OFFSET),
            0x2305_fa72_23d5_15db
        );

        Ok(())
    }

    #[test]
    fn page_store_v2_database_header_has_exact_golden_bytes_and_mutation_rejection()
    -> Result<(), Box<dyn Error>> {
        let persistent_id = persistent_id(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)?;
        let identity = database_file_header_identity(DatabaseFileRole::PageStore)?;
        let header = build_page_store_header_v2(persistent_id, 10, identity);
        let mut expected = [0_u8; PAGE_STORE_HEADER_V2_LENGTH];
        expected[..8].copy_from_slice(b"NTSQPGS1");
        expected[8..12].copy_from_slice(&[0, 2, 0, 128]);
        expected[16..32].copy_from_slice(&persistent_id.get().to_be_bytes());
        expected[32..40].copy_from_slice(&10_u64.to_be_bytes());
        expected[64..112].copy_from_slice(&[
            0x4e, 0x54, 0x53, 0x51, 0x43, 0x46, 0x49, 0x31, 0x00, 0x01, 0x00, 0x30, 0x02, 0x00,
            0x00, 0x00, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a,
            0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30,
        ]);
        expected[120..128].copy_from_slice(&0xb2af_61e3_42f5_ce02_u64.to_be_bytes());
        assert_eq!(header, expected);
        assert_eq!(
            parse_page_store_header_v2(&header, PageLayout::for_const::<10>()?)?,
            PageStoreHeaderV2Metadata {
                persistent_id,
                database_file_identity: identity,
            }
        );

        let mut reserved = header;
        reserved[112] = 1;
        let checksum = checksum_v1(&reserved[..PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET]);
        write_u64(
            &mut reserved,
            PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET,
            checksum,
        );
        assert!(parse_page_store_header_v2(&reserved, PageLayout::for_const::<10>()?).is_err());
        let mut bad_checksum = header;
        bad_checksum[PAGE_STORE_HEADER_V2_CHECKSUM_OFFSET] ^= 1;
        assert!(parse_page_store_header_v2(&bad_checksum, PageLayout::for_const::<10>()?).is_err());
        Ok(())
    }

    #[test]
    fn page_store_incomplete_group_at_partial_data_repaired() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-incomplete-data")?;
        let log_path = directory.path().join("commit-log.bin");
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(222)?;

        // Use N=10 which needs ceil(10/8) = 2 data frames
        let mut log = FileCommitLog::<10>::create_new_page_capable(&log_path, pid)?;
        let mut store = FilePageStore::<10>::create_new(&store_path, pid)?;

        let unlogged = unlogged_page(log.lineage(), 1, 1, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10])?;
        let dirty =
            stage_page_write(&mut log, unlogged).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty)?;

        let len_after_first = fs::metadata(&store_path)?.len();
        drop(store);

        // Write incomplete second group: header + required-position + 1 data frame (need 2)
        let hdr = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 2, 2, 0);
        let req = build_page_store_frame(PageStoreFrameKind::RequiredPosition, 2, 42, 0);
        let data = build_page_store_frame(PageStoreFrameKind::PageData, 2, 0, 0);
        append_bytes(&store_path, &hdr)?;
        append_bytes(&store_path, &req)?;
        append_bytes(&store_path, &data)?;

        // Reopen should truncate back to first group
        let store = FilePageStore::<10>::open(&store_path)?;
        assert_eq!(store.pages().len(), 1);
        assert_eq!(fs::metadata(&store_path)?.len(), len_after_first);
        let snapshot = store
            .page(page_number(1)?)
            .ok_or_else(|| io::Error::other("missing page after repair"))?;
        assert_eq!(*snapshot.bytes(), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(snapshot.store_sequence(), 1);

        // Verify high-water: next write should get sequence 2
        drop(store);
        drop(log);
        let mut log = FileCommitLog::<10>::open_page_capable(&log_path)?;
        let mut store = FilePageStore::<10>::open(&store_path)?;
        let unlogged2 = unlogged_page(
            log.lineage(),
            3,
            1,
            [11, 12, 13, 14, 15, 16, 17, 18, 19, 20],
        )?;
        let dirty2 =
            stage_page_write(&mut log, unlogged2).map_err(|e| io::Error::other(format!("{e}")))?;
        write_page_through_flush(&mut log, &mut store, dirty2)?;
        let snapshot2 = store
            .page(page_number(3)?)
            .ok_or_else(|| io::Error::other("missing page 3"))?;
        assert_eq!(snapshot2.store_sequence(), 2);

        Ok(())
    }

    #[test]
    fn page_store_required_position_payload_c_nonzero_rejects() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("ps-reqpos-c-nonzero")?;
        let store_path = directory.path().join("pages.bin");
        let pid = persistent_id(223)?;

        let store = FilePageStore::<2>::create_new(&store_path, pid)?;
        drop(store);

        let hdr = build_page_store_frame(PageStoreFrameKind::SnapshotHeader, 1, 1, 0);
        // Required-position with nonzero payload C
        let req = build_page_store_frame(PageStoreFrameKind::RequiredPosition, 1, 1, 42);
        append_bytes(&store_path, &hdr)?;
        append_bytes(&store_path, &req)?;
        let len_before = fs::metadata(&store_path)?.len();

        let error = FilePageStore::<2>::open(&store_path)
            .err()
            .ok_or_else(|| io::Error::other("payload C nonzero accepted"))?;
        assert!(matches!(error, PageStoreOpenError::Format(_)));
        assert_eq!(fs::metadata(&store_path)?.len(), len_before);

        Ok(())
    }
}
