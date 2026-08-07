use std::{
    error::Error,
    fmt, fs,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use ntsql_transaction::{
    DurableTransactionRestartCheckpointBaseline,
    DurableTransactionRestartCheckpointBaselinePublicationPermit,
    DurableTransactionRestartCheckpointBaselinePublisher,
    DurableTransactionRestartCheckpointBaselineSource,
    OwnedDurableTransactionRestartCheckpointBaselineObservation, UnrecoveredTransactionPageStorage,
};
use ntsql_wal::PersistentLogId;

use super::{
    FileCommitLog, FileOpenError, FilePageStore, PageStoreOpenError,
    RestartCheckpointBaselineDecodeError, RestartCheckpointBaselineEncodeError, checksum_v1,
    decode_restart_checkpoint_baseline, encode_restart_checkpoint_baseline, read_u16, read_u32,
    read_u64, read_u128, write_u16, write_u32, write_u64, write_u128,
};

pub(crate) const CONTROL_FILE_NAME: &str = "control";
pub(crate) const CURRENT_FILE_NAME: &str = "current";
pub(crate) const CANDIDATE_FILE_NAME: &str = "candidate";
const CONTROL_MAGIC: [u8; 8] = *b"NTSQCKS1";
pub(crate) const CONTROL_FORMAT_VERSION: u16 = 1;
pub(crate) const CONTROL_HEADER_LENGTH: usize = 64;
const CONTROL_HEADER_LENGTH_U16: u16 = 64;
const CONTROL_RESERVED_START: usize = 32;
const CONTROL_CHECKSUM_OFFSET: usize = 56;

/// Exact filesystem operation that reported a checkpoint-slot failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointSlotIoStage {
    /// Creating the caller-selected slot directory.
    CreateSlotDirectory,
    /// Creating the immutable control file inside a new slot directory.
    CreateControlFile,
    /// Opening the immutable control file inside an existing slot directory.
    OpenControlFile,
    /// Acquiring the control file's lifetime exclusive lock.
    AcquireExclusiveControlLock,
    /// Reading the control file's metadata after acquiring its lock.
    ReadControlMetadata,
    /// Reading the complete fixed control header.
    ReadControlHeader,
    /// Writing the complete fixed control header.
    WriteControlHeader,
    /// Synchronizing a newly written control file.
    SyncControlFile,
    /// Opening the checkpoint slot directory itself.
    OpenSlotDirectory,
    /// Synchronizing the newly created checkpoint slot directory.
    SyncSlotDirectory,
    /// Opening the slot directory's parent after creation.
    OpenParentDirectory,
    /// Synchronizing the slot directory's parent after creation.
    SyncParentDirectory,
    /// Opening the optional current checkpoint blob.
    OpenCurrentFile,
    /// Verifying that only the optional current entry is absent.
    VerifyCurrentAbsence,
    /// Reading current-blob metadata before reading its bytes.
    ReadCurrentMetadataBeforeRead,
    /// Reading the complete current checkpoint blob.
    ReadCurrentBytes,
    /// Reading current-blob metadata after reading its bytes.
    ReadCurrentMetadataAfterRead,
    /// Removing one stale unselected publication candidate.
    RemoveCandidateFile,
    /// Creating one fresh unselected publication candidate.
    CreateCandidateFile,
    /// Writing the complete encoded baseline to the candidate.
    WriteCandidateFile,
    /// Synchronizing the complete candidate file.
    SyncCandidateFile,
    /// Atomically replacing the selected current entry with the candidate.
    ReplaceCurrentFile,
    /// Synchronizing the slot directory after selected-entry replacement.
    SyncPublishedSlotDirectory,
}

impl fmt::Display for FileRestartCheckpointSlotIoStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateSlotDirectory => {
                formatter.write_str("creating the restart checkpoint slot directory")
            }
            Self::CreateControlFile => {
                formatter.write_str("creating the restart checkpoint control file")
            }
            Self::OpenControlFile => {
                formatter.write_str("opening the restart checkpoint control file")
            }
            Self::AcquireExclusiveControlLock => {
                formatter.write_str("acquiring the exclusive restart checkpoint control lock")
            }
            Self::ReadControlMetadata => {
                formatter.write_str("reading restart checkpoint control metadata")
            }
            Self::ReadControlHeader => {
                formatter.write_str("reading the restart checkpoint control header")
            }
            Self::WriteControlHeader => {
                formatter.write_str("writing the restart checkpoint control header")
            }
            Self::SyncControlFile => {
                formatter.write_str("synchronizing the restart checkpoint control file")
            }
            Self::OpenSlotDirectory => {
                formatter.write_str("opening the restart checkpoint slot directory")
            }
            Self::SyncSlotDirectory => {
                formatter.write_str("synchronizing the restart checkpoint slot directory")
            }
            Self::OpenParentDirectory => {
                formatter.write_str("opening the restart checkpoint parent directory")
            }
            Self::SyncParentDirectory => {
                formatter.write_str("synchronizing the restart checkpoint parent directory")
            }
            Self::OpenCurrentFile => {
                formatter.write_str("opening the current restart checkpoint blob")
            }
            Self::VerifyCurrentAbsence => {
                formatter.write_str("verifying current restart checkpoint absence")
            }
            Self::ReadCurrentMetadataBeforeRead => {
                formatter.write_str("reading current restart checkpoint metadata before its bytes")
            }
            Self::ReadCurrentBytes => {
                formatter.write_str("reading the current restart checkpoint bytes")
            }
            Self::ReadCurrentMetadataAfterRead => {
                formatter.write_str("reading current restart checkpoint metadata after its bytes")
            }
            Self::RemoveCandidateFile => {
                formatter.write_str("removing a stale restart checkpoint candidate")
            }
            Self::CreateCandidateFile => {
                formatter.write_str("creating a fresh restart checkpoint candidate")
            }
            Self::WriteCandidateFile => {
                formatter.write_str("writing the restart checkpoint candidate")
            }
            Self::SyncCandidateFile => {
                formatter.write_str("synchronizing the restart checkpoint candidate")
            }
            Self::ReplaceCurrentFile => {
                formatter.write_str("replacing the current restart checkpoint")
            }
            Self::SyncPublishedSlotDirectory => {
                formatter.write_str("synchronizing the published restart checkpoint directory")
            }
        }
    }
}

/// I/O failure paired with the exact checkpoint-slot operation that reported it.
#[derive(Debug)]
pub struct FileRestartCheckpointSlotIoError {
    stage: FileRestartCheckpointSlotIoStage,
    source: io::Error,
}

impl FileRestartCheckpointSlotIoError {
    fn new(stage: FileRestartCheckpointSlotIoStage, source: io::Error) -> Self {
        Self { stage, source }
    }

    /// Returns the exact operation that reported the failure.
    #[must_use]
    pub const fn stage(&self) -> FileRestartCheckpointSlotIoStage {
        self.stage
    }

    /// Returns the original standard-library I/O cause.
    #[must_use]
    pub const fn io_source(&self) -> &io::Error {
        &self.source
    }
}

impl PartialEq for FileRestartCheckpointSlotIoError {
    fn eq(&self, other: &Self) -> bool {
        self.stage == other.stage
            && self.source.kind() == other.source.kind()
            && self.source.raw_os_error() == other.source.raw_os_error()
    }
}

impl Eq for FileRestartCheckpointSlotIoError {}

impl fmt::Display for FileRestartCheckpointSlotIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} failed: {}", self.stage, self.source)
    }
}

impl Error for FileRestartCheckpointSlotIoError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Exact malformed-field reason for a checkpoint slot's immutable control file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointSlotFormatErrorReason {
    /// The control file was not exactly one complete fixed header.
    FileLength {
        /// Exact observed file length.
        actual: u64,
    },
    /// The independent control-file magic did not match.
    HeaderMagic {
        /// Exact eight bytes found at the magic offset.
        actual: [u8; 8],
    },
    /// The control-file format version is unsupported.
    HeaderVersion {
        /// Exact decoded version.
        actual: u16,
    },
    /// The encoded header length was not the exact version 1 width.
    HeaderLength {
        /// Exact decoded header length.
        actual: u16,
    },
    /// Reserved header flags were nonzero.
    HeaderFlags {
        /// Exact decoded flags.
        actual: u32,
    },
    /// The immutable persistent log identity was zero.
    PersistentLogIdZero,
    /// One reserved header byte was nonzero.
    ReservedByteNonZero {
        /// Exact absolute byte offset.
        offset: usize,
        /// Exact nonzero byte.
        actual: u8,
    },
    /// The complete protected control header checksum did not match.
    HeaderChecksum {
        /// Checksum calculated from bytes before the checksum field.
        expected: u64,
        /// Checksum stored in the control file.
        actual: u64,
    },
}

impl fmt::Display for FileRestartCheckpointSlotFormatErrorReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileLength { actual } => write!(
                formatter,
                "control file length {actual} is not the required {CONTROL_HEADER_LENGTH} bytes"
            ),
            Self::HeaderMagic { actual } => {
                write!(formatter, "control header magic is invalid: {actual:?}")
            }
            Self::HeaderVersion { actual } => {
                write!(formatter, "control header version {actual} is unsupported")
            }
            Self::HeaderLength { actual } => {
                write!(formatter, "control header length {actual} is invalid")
            }
            Self::HeaderFlags { actual } => {
                write!(formatter, "control header flags {actual:#010x} are invalid")
            }
            Self::PersistentLogIdZero => {
                formatter.write_str("control header persistent log ID is zero")
            }
            Self::ReservedByteNonZero { offset, actual } => write!(
                formatter,
                "control header reserved byte at offset {offset} is nonzero: {actual:#04x}"
            ),
            Self::HeaderChecksum { expected, actual } => write!(
                formatter,
                "control header checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
        }
    }
}

/// Malformed checkpoint control file paired with the exact byte offset.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRestartCheckpointSlotFormatError {
    offset: u64,
    reason: FileRestartCheckpointSlotFormatErrorReason,
}

impl FileRestartCheckpointSlotFormatError {
    fn new(offset: u64, reason: FileRestartCheckpointSlotFormatErrorReason) -> Self {
        Self { offset, reason }
    }

    /// Returns the exact byte offset that reported the defect.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the exact malformed-field reason.
    #[must_use]
    pub const fn reason(&self) -> &FileRestartCheckpointSlotFormatErrorReason {
        &self.reason
    }
}

impl fmt::Display for FileRestartCheckpointSlotFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "restart checkpoint control format error at byte {}: {}",
            self.offset, self.reason
        )
    }
}

impl Error for FileRestartCheckpointSlotFormatError {}

/// Failure while creating one empty, lineaged filesystem checkpoint slot.
#[derive(Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointSlotCreateError {
    /// The supplied slot path has no usable parent directory.
    MissingParentDirectory,
    /// A stage-specific filesystem operation failed.
    Io(FileRestartCheckpointSlotIoError),
}

impl fmt::Display for FileRestartCheckpointSlotCreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParentDirectory => {
                formatter.write_str("restart checkpoint slot path has no existing parent directory")
            }
            Self::Io(source) => source.fmt(formatter),
        }
    }
}

impl Error for FileRestartCheckpointSlotCreateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingParentDirectory => None,
            Self::Io(source) => Some(source),
        }
    }
}

/// Failure while opening one existing filesystem checkpoint slot.
#[derive(Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointSlotOpenError {
    /// A stage-specific filesystem operation failed.
    Io(FileRestartCheckpointSlotIoError),
    /// The immutable control file was malformed.
    Format(FileRestartCheckpointSlotFormatError),
}

impl fmt::Display for FileRestartCheckpointSlotOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::Format(source) => source.fmt(formatter),
        }
    }
}

impl Error for FileRestartCheckpointSlotOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Format(source) => Some(source),
        }
    }
}

/// Failure to load one optional current filesystem checkpoint baseline.
#[derive(Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointBaselineSourceError {
    /// A stage-specific filesystem operation failed.
    Io(FileRestartCheckpointSlotIoError),
    /// The current file length cannot be represented on this host.
    CurrentLengthOutOfRange {
        /// Exact filesystem length that was rejected.
        actual: u64,
    },
    /// The complete current byte buffer could not reserve its exact length.
    CurrentCapacityExhausted {
        /// Exact host-sized byte length that required reservation.
        length: usize,
    },
    /// The current file changed length while its already-open handle was read.
    CurrentLengthChanged {
        /// File length observed before the read.
        before: u64,
        /// File length observed after the read.
        after: u64,
    },
    /// The complete bytes failed ADR 0044 structural decoding.
    Decode(RestartCheckpointBaselineDecodeError),
}

impl fmt::Display for FileRestartCheckpointBaselineSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::CurrentLengthOutOfRange { actual } => write!(
                formatter,
                "current restart checkpoint length {actual} is not representable on this host"
            ),
            Self::CurrentCapacityExhausted { length } => write!(
                formatter,
                "current restart checkpoint byte capacity is exhausted for {length} bytes"
            ),
            Self::CurrentLengthChanged { before, after } => write!(
                formatter,
                "current restart checkpoint length changed while reading: {before} to {after}"
            ),
            Self::Decode(source) => write!(
                formatter,
                "current restart checkpoint structural decoding failed: {source}"
            ),
        }
    }
}

impl Error for FileRestartCheckpointBaselineSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Decode(source) => Some(source),
            Self::CurrentLengthOutOfRange { .. }
            | Self::CurrentCapacityExhausted { .. }
            | Self::CurrentLengthChanged { .. } => None,
        }
    }
}

impl From<SlotCurrentReadError> for FileRestartCheckpointBaselineSourceError {
    fn from(source: SlotCurrentReadError) -> Self {
        match source {
            SlotCurrentReadError::Io(source) => Self::Io(source),
            SlotCurrentReadError::CurrentLengthOutOfRange { actual } => {
                Self::CurrentLengthOutOfRange { actual }
            }
            SlotCurrentReadError::CurrentCapacityExhausted { length } => {
                Self::CurrentCapacityExhausted { length }
            }
            SlotCurrentReadError::CurrentLengthChanged { before, after } => {
                Self::CurrentLengthChanged { before, after }
            }
        }
    }
}

/// Exact deterministic failure point in filesystem checkpoint publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointBaselinePublicationFaultPoint {
    /// Fail after encoding but before stale-candidate cleanup.
    BeforeCandidateCleanup,
    /// Fail after stale-candidate cleanup and before candidate creation.
    AfterCandidateCleanup,
    /// Fail after creating an empty candidate and before writing bytes.
    AfterCandidateCreate,
    /// Fail after writing all bytes and before candidate synchronization.
    AfterCandidateWrite,
    /// Fail after synchronizing and closing the candidate, before replacement.
    AfterCandidateSync,
    /// Fail after replacing `current` and before directory synchronization.
    AfterCurrentReplace,
    /// Fail after directory synchronization instead of reporting success.
    AfterDirectorySync,
}

impl FileRestartCheckpointBaselinePublicationFaultPoint {
    const fn from_step(step: SlotPublicationStep) -> Self {
        match step {
            SlotPublicationStep::BeforeCandidateCleanup => Self::BeforeCandidateCleanup,
            SlotPublicationStep::AfterCandidateCleanup => Self::AfterCandidateCleanup,
            SlotPublicationStep::AfterCandidateCreate => Self::AfterCandidateCreate,
            SlotPublicationStep::AfterCandidateWrite => Self::AfterCandidateWrite,
            SlotPublicationStep::AfterCandidateSync => Self::AfterCandidateSync,
            SlotPublicationStep::AfterCurrentReplace => Self::AfterCurrentReplace,
            SlotPublicationStep::AfterDirectorySync => Self::AfterDirectorySync,
        }
    }
}

impl fmt::Display for FileRestartCheckpointBaselinePublicationFaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeCandidateCleanup => formatter.write_str("before stale-candidate cleanup"),
            Self::AfterCandidateCleanup => formatter.write_str("after stale-candidate cleanup"),
            Self::AfterCandidateCreate => formatter.write_str("after candidate creation"),
            Self::AfterCandidateWrite => formatter.write_str("after candidate write"),
            Self::AfterCandidateSync => formatter.write_str("after candidate synchronization"),
            Self::AfterCurrentReplace => formatter.write_str("after current-entry replacement"),
            Self::AfterDirectorySync => formatter.write_str("after slot-directory synchronization"),
        }
    }
}

/// Rejected attempt to replace an already armed publication fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileRestartCheckpointBaselinePublicationFaultAlreadyArmed {
    armed: FileRestartCheckpointBaselinePublicationFaultPoint,
    requested: FileRestartCheckpointBaselinePublicationFaultPoint,
}

impl FileRestartCheckpointBaselinePublicationFaultAlreadyArmed {
    /// Returns the retained existing fault.
    #[must_use]
    pub const fn armed(&self) -> FileRestartCheckpointBaselinePublicationFaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(&self) -> FileRestartCheckpointBaselinePublicationFaultPoint {
        self.requested
    }
}

impl fmt::Display for FileRestartCheckpointBaselinePublicationFaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "filesystem checkpoint publication fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for FileRestartCheckpointBaselinePublicationFaultAlreadyArmed {}

/// Outcome-indeterminate filesystem checkpoint publication failure.
#[derive(Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointBaselinePublicationError {
    /// The owner permit did not identify the supplied baseline exactly.
    PublicationPermitMismatch {
        /// Persistent log ID carried by the authoritative baseline.
        baseline_persistent_log_id: u128,
        /// Persistent log ID carried by the owner permit.
        permit_persistent_log_id: u128,
        /// Optional frontier carried by the authoritative baseline.
        baseline_durable_frontier: Option<u64>,
        /// Optional frontier carried by the owner permit.
        permit_durable_frontier: Option<u64>,
        /// Transaction count carried by the authoritative baseline.
        baseline_transaction_count: usize,
        /// Transaction count carried by the owner permit.
        permit_transaction_count: usize,
    },
    /// The authoritative baseline belongs to another lineaged slot.
    SlotPersistentLogIdMismatch {
        /// Persistent log ID bound to the immutable slot control header.
        slot: PersistentLogId,
        /// Persistent log ID carried by the authoritative baseline.
        baseline: PersistentLogId,
    },
    /// ADR 0044 encoding failed before filesystem mutation.
    Encode(RestartCheckpointBaselineEncodeError),
    /// A deterministic test fault fired at one exact physical boundary.
    InjectedFault(FileRestartCheckpointBaselinePublicationFaultPoint),
    /// One exact filesystem operation failed.
    Io(FileRestartCheckpointSlotIoError),
}

impl fmt::Display for FileRestartCheckpointBaselinePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicationPermitMismatch {
                baseline_persistent_log_id,
                permit_persistent_log_id,
                baseline_durable_frontier,
                permit_durable_frontier,
                baseline_transaction_count,
                permit_transaction_count,
            } => write!(
                formatter,
                "filesystem checkpoint publication permit mismatch: baseline id {baseline_persistent_log_id:#034x}, frontier {baseline_durable_frontier:?}, count {baseline_transaction_count}; permit id {permit_persistent_log_id:#034x}, frontier {permit_durable_frontier:?}, count {permit_transaction_count}"
            ),
            Self::SlotPersistentLogIdMismatch { slot, baseline } => write!(
                formatter,
                "filesystem checkpoint slot persistent log ID {} does not match baseline persistent log ID {}",
                slot.get(),
                baseline.get()
            ),
            Self::Encode(source) => {
                write!(formatter, "filesystem checkpoint encoding failed: {source}")
            }
            Self::InjectedFault(point) => {
                write!(
                    formatter,
                    "injected filesystem checkpoint publication failure {point}"
                )
            }
            Self::Io(source) => source.fmt(formatter),
        }
    }
}

impl Error for FileRestartCheckpointBaselinePublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::PublicationPermitMismatch { .. }
            | Self::SlotPersistentLogIdMismatch { .. }
            | Self::InjectedFault(_) => None,
        }
    }
}

/// Locked filesystem source and publisher for one current restart checkpoint baseline.
///
/// The stable lineaged `control` file, not the replaceable `current` blob, owns
/// the lifetime advisory lock. Loaded bytes remain untrusted and cannot be used
/// as an authoritative encoder input:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     FileRestartCheckpointBaselineSource, encode_restart_checkpoint_baseline,
/// };
///
/// fn cannot_encode_source(source: &FileRestartCheckpointBaselineSource) {
///     let _ = encode_restart_checkpoint_baseline(source);
/// }
/// ```
///
/// The sibling publication port still cannot be invoked without its owner permit:
///
/// ```compile_fail
/// use ntsql_storage_file::FileRestartCheckpointBaselineSource;
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointBaseline,
///     DurableTransactionRestartCheckpointBaselinePublisher,
/// };
///
/// fn cannot_publish(
///     source: &mut FileRestartCheckpointBaselineSource,
///     baseline: &DurableTransactionRestartCheckpointBaseline,
/// ) {
///     let _ = source.publish_restart_checkpoint_baseline(baseline);
/// }
/// ```
#[derive(Debug)]
pub struct FileRestartCheckpointBaselineSource {
    slot_directory: PathBuf,
    _control_file: File,
    directory: File,
    persistent_log_id: PersistentLogId,
    armed_publication_fault: Option<FileRestartCheckpointBaselinePublicationFaultPoint>,
}

impl FileRestartCheckpointBaselineSource {
    /// Creates and locks one new empty checkpoint slot.
    ///
    /// Any error after directory creation may leave a partial slot requiring
    /// explicit caller reconciliation. It is never reported as successful.
    pub fn create_new<P>(
        slot_directory: P,
        persistent_log_id: PersistentLogId,
    ) -> Result<Self, FileRestartCheckpointSlotCreateError>
    where
        P: AsRef<Path>,
    {
        let slot_directory = slot_directory.as_ref().to_path_buf();
        let (control_file, directory) =
            create_locked_control_slot(&slot_directory, CONTROL_MAGIC, persistent_log_id)?;

        Ok(Self {
            slot_directory,
            _control_file: control_file,
            directory,
            persistent_log_id,
            armed_publication_fault: None,
        })
    }

    /// Opens, locks, and validates one existing checkpoint slot.
    pub fn open<P>(slot_directory: P) -> Result<Self, FileRestartCheckpointSlotOpenError>
    where
        P: AsRef<Path>,
    {
        let slot_directory = slot_directory.as_ref().to_path_buf();
        let (control_file, directory, persistent_log_id) =
            open_locked_control_slot(&slot_directory, CONTROL_MAGIC)?;

        Ok(Self {
            slot_directory,
            _control_file: control_file,
            directory,
            persistent_log_id,
            armed_publication_fault: None,
        })
    }

    /// Returns the immutable persistent log identity bound to this slot.
    #[must_use]
    pub const fn persistent_log_id(&self) -> PersistentLogId {
        self.persistent_log_id
    }

    /// Returns the caller-selected slot directory.
    #[must_use]
    pub fn slot_directory(&self) -> &Path {
        &self.slot_directory
    }

    /// Arms one publication fault without replacing an existing plan.
    pub fn arm_publication_fault(
        &mut self,
        fault: FileRestartCheckpointBaselinePublicationFaultPoint,
    ) -> Result<(), FileRestartCheckpointBaselinePublicationFaultAlreadyArmed> {
        if let Some(armed) = self.armed_publication_fault {
            return Err(FileRestartCheckpointBaselinePublicationFaultAlreadyArmed {
                armed,
                requested: fault,
            });
        }
        self.armed_publication_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot publication fault that has not reached its stage.
    #[must_use]
    pub const fn armed_publication_fault(
        &self,
    ) -> Option<FileRestartCheckpointBaselinePublicationFaultPoint> {
        self.armed_publication_fault
    }
}

impl DurableTransactionRestartCheckpointBaselineSource for FileRestartCheckpointBaselineSource {
    type Error = FileRestartCheckpointBaselineSourceError;

    fn load_restart_checkpoint_baseline(
        &mut self,
    ) -> Result<Option<OwnedDurableTransactionRestartCheckpointBaselineObservation>, Self::Error>
    {
        let Some(bytes) = read_current_slot_bytes(&self.slot_directory)? else {
            return Ok(None);
        };
        decode_restart_checkpoint_baseline(&bytes)
            .map(Some)
            .map_err(FileRestartCheckpointBaselineSourceError::Decode)
    }
}

impl DurableTransactionRestartCheckpointBaselinePublisher for FileRestartCheckpointBaselineSource {
    type Error = FileRestartCheckpointBaselinePublicationError;

    fn publish_restart_checkpoint_baseline(
        &mut self,
        baseline: &DurableTransactionRestartCheckpointBaseline,
        permit: DurableTransactionRestartCheckpointBaselinePublicationPermit<'_>,
    ) -> Result<(), Self::Error> {
        let baseline_persistent_log_id = baseline.persistent_log_id().get();
        let permit_persistent_log_id = permit.persistent_log_id().get();
        let baseline_durable_frontier = baseline.durable_frontier();
        let permit_durable_frontier = permit.durable_frontier();
        let baseline_transaction_count = baseline.transactions().len();
        let permit_transaction_count = permit.transaction_count();
        if baseline_persistent_log_id != permit_persistent_log_id
            || baseline_durable_frontier != permit_durable_frontier
            || baseline_transaction_count != permit_transaction_count
        {
            return Err(
                FileRestartCheckpointBaselinePublicationError::PublicationPermitMismatch {
                    baseline_persistent_log_id,
                    permit_persistent_log_id,
                    baseline_durable_frontier,
                    permit_durable_frontier,
                    baseline_transaction_count,
                    permit_transaction_count,
                },
            );
        }
        if self.persistent_log_id != baseline.persistent_log_id() {
            return Err(
                FileRestartCheckpointBaselinePublicationError::SlotPersistentLogIdMismatch {
                    slot: self.persistent_log_id,
                    baseline: baseline.persistent_log_id(),
                },
            );
        }

        let encoded = encode_restart_checkpoint_baseline(baseline)
            .map_err(FileRestartCheckpointBaselinePublicationError::Encode)?;

        let Self {
            slot_directory,
            directory,
            armed_publication_fault,
            ..
        } = self;
        publish_slot_current_bytes(slot_directory, directory, &encoded, |step| {
            let point = FileRestartCheckpointBaselinePublicationFaultPoint::from_step(step);
            if *armed_publication_fault == Some(point) {
                *armed_publication_fault = None;
                true
            } else {
                false
            }
        })
        .map_err(|source| match source {
            SlotPublicationError::InjectedFault(step) => {
                FileRestartCheckpointBaselinePublicationError::InjectedFault(
                    FileRestartCheckpointBaselinePublicationFaultPoint::from_step(step),
                )
            }
            SlotPublicationError::Io(source) => {
                FileRestartCheckpointBaselinePublicationError::Io(source)
            }
        })
    }
}

/// Locked unrecovered WAL/page-store owner paired with its checkpoint source.
///
/// Construction is available only through
/// [`open_transaction_page_storage_with_checkpoint`], which acquires all three
/// lifetime locks in one fixed order.
pub struct UnrecoveredFileTransactionPageStorageWithCheckpoint<const N: usize> {
    storage: UnrecoveredTransactionPageStorage<FileCommitLog<N>, FilePageStore<N>, N>,
    checkpoint: FileRestartCheckpointBaselineSource,
}

impl<const N: usize> fmt::Debug for UnrecoveredFileTransactionPageStorageWithCheckpoint<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnrecoveredFileTransactionPageStorageWithCheckpoint")
            .field(
                "checkpoint_persistent_log_id",
                &self.checkpoint.persistent_log_id(),
            )
            .finish_non_exhaustive()
    }
}

impl<const N: usize> UnrecoveredFileTransactionPageStorageWithCheckpoint<N> {
    /// Separates the already-locked unrecovered owner and untrusted checkpoint source.
    pub fn into_parts(
        self,
    ) -> (
        UnrecoveredTransactionPageStorage<FileCommitLog<N>, FilePageStore<N>, N>,
        FileRestartCheckpointBaselineSource,
    ) {
        (self.storage, self.checkpoint)
    }
}

/// Failure while opening the fixed-order WAL/page-store/checkpoint composition.
#[derive(Debug, Eq, PartialEq)]
pub enum FileTransactionPageStorageCheckpointOpenError {
    /// The transaction-page-capable WAL could not be opened first.
    CommitLog(FileOpenError),
    /// The page store could not be opened second.
    PageStore(PageStoreOpenError),
    /// WAL and page-store control headers identify different persistent logs.
    StoragePersistentLogIdMismatch {
        /// Persistent log ID read from the WAL.
        commit_log: PersistentLogId,
        /// Persistent log ID read from the page store.
        page_store: PersistentLogId,
    },
    /// The checkpoint slot could not be opened third.
    Checkpoint(FileRestartCheckpointSlotOpenError),
    /// The checkpoint control file identifies a different persistent log.
    CheckpointPersistentLogIdMismatch {
        /// Persistent log ID shared by the WAL and page store.
        storage: PersistentLogId,
        /// Persistent log ID read from the checkpoint control file.
        checkpoint: PersistentLogId,
    },
}

impl fmt::Display for FileTransactionPageStorageCheckpointOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommitLog(source) => {
                write!(formatter, "transaction-page WAL open failed: {source}")
            }
            Self::PageStore(source) => write!(formatter, "page-store open failed: {source}"),
            Self::StoragePersistentLogIdMismatch {
                commit_log,
                page_store,
            } => write!(
                formatter,
                "WAL persistent log ID {} does not match page-store persistent log ID {}",
                commit_log.get(),
                page_store.get()
            ),
            Self::Checkpoint(source) => {
                write!(formatter, "restart checkpoint slot open failed: {source}")
            }
            Self::CheckpointPersistentLogIdMismatch {
                storage,
                checkpoint,
            } => write!(
                formatter,
                "storage persistent log ID {} does not match checkpoint persistent log ID {}",
                storage.get(),
                checkpoint.get()
            ),
        }
    }
}

impl Error for FileTransactionPageStorageCheckpointOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CommitLog(source) => Some(source),
            Self::PageStore(source) => Some(source),
            Self::Checkpoint(source) => Some(source),
            Self::StoragePersistentLogIdMismatch { .. }
            | Self::CheckpointPersistentLogIdMismatch { .. } => None,
        }
    }
}

/// Opens and locks a WAL, page store, and lineaged checkpoint slot in that order.
///
/// A later-stage failure drops every earlier adapter before returning. The
/// operation is nonblocking and does not derive its lock order from checkpoint
/// validation or publication touch order.
pub fn open_transaction_page_storage_with_checkpoint<
    const N: usize,
    LogPath,
    StorePath,
    CheckpointPath,
>(
    log_path: LogPath,
    store_path: StorePath,
    checkpoint_path: CheckpointPath,
) -> Result<
    UnrecoveredFileTransactionPageStorageWithCheckpoint<N>,
    FileTransactionPageStorageCheckpointOpenError,
>
where
    LogPath: AsRef<Path>,
    StorePath: AsRef<Path>,
    CheckpointPath: AsRef<Path>,
{
    let log = FileCommitLog::<N>::open_transaction_page_capable(log_path)
        .map_err(FileTransactionPageStorageCheckpointOpenError::CommitLog)?;
    let store = FilePageStore::<N>::open(store_path)
        .map_err(FileTransactionPageStorageCheckpointOpenError::PageStore)?;
    if log.persistent_id() != store.persistent_id() {
        return Err(
            FileTransactionPageStorageCheckpointOpenError::StoragePersistentLogIdMismatch {
                commit_log: log.persistent_id(),
                page_store: store.persistent_id(),
            },
        );
    }
    let checkpoint = FileRestartCheckpointBaselineSource::open(checkpoint_path)
        .map_err(FileTransactionPageStorageCheckpointOpenError::Checkpoint)?;
    if checkpoint.persistent_log_id() != log.persistent_id() {
        return Err(
            FileTransactionPageStorageCheckpointOpenError::CheckpointPersistentLogIdMismatch {
                storage: log.persistent_id(),
                checkpoint: checkpoint.persistent_log_id(),
            },
        );
    }

    Ok(UnrecoveredFileTransactionPageStorageWithCheckpoint {
        storage: UnrecoveredTransactionPageStorage::new(log, store),
        checkpoint,
    })
}

/// Exact untyped failure while reading one optional selected slot value.
///
/// Each concrete adapter maps these variants onto its own public source error
/// so the two slot namespaces never share a public error type.
pub(crate) enum SlotCurrentReadError {
    Io(FileRestartCheckpointSlotIoError),
    CurrentLengthOutOfRange { actual: u64 },
    CurrentCapacityExhausted { length: usize },
    CurrentLengthChanged { before: u64, after: u64 },
}

/// Exact physical boundary inside one atomic slot publication attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlotPublicationStep {
    BeforeCandidateCleanup,
    AfterCandidateCleanup,
    AfterCandidateCreate,
    AfterCandidateWrite,
    AfterCandidateSync,
    AfterCurrentReplace,
    AfterDirectorySync,
}

/// Untyped publication failure before an adapter-specific error is built.
pub(crate) enum SlotPublicationError {
    InjectedFault(SlotPublicationStep),
    Io(FileRestartCheckpointSlotIoError),
}

pub(crate) fn build_slot_control_header(
    magic: [u8; 8],
    persistent_log_id: PersistentLogId,
) -> [u8; CONTROL_HEADER_LENGTH] {
    let mut header = [0_u8; CONTROL_HEADER_LENGTH];
    header[..8].copy_from_slice(&magic);
    write_u16(&mut header, 8, CONTROL_FORMAT_VERSION);
    write_u16(&mut header, 10, CONTROL_HEADER_LENGTH_U16);
    write_u32(&mut header, 12, 0);
    write_u128(&mut header, 16, persistent_log_id.get());
    let checksum = checksum_v1(&header[..CONTROL_CHECKSUM_OFFSET]);
    write_u64(&mut header, CONTROL_CHECKSUM_OFFSET, checksum);
    header
}

pub(crate) fn parse_slot_control_header(
    magic: [u8; 8],
    header: &[u8; CONTROL_HEADER_LENGTH],
) -> Result<PersistentLogId, FileRestartCheckpointSlotFormatError> {
    if header[..8] != magic {
        let mut actual = [0_u8; 8];
        actual.copy_from_slice(&header[..8]);
        return Err(FileRestartCheckpointSlotFormatError::new(
            0,
            FileRestartCheckpointSlotFormatErrorReason::HeaderMagic { actual },
        ));
    }
    let version = read_u16(header, 8);
    if version != CONTROL_FORMAT_VERSION {
        return Err(FileRestartCheckpointSlotFormatError::new(
            8,
            FileRestartCheckpointSlotFormatErrorReason::HeaderVersion { actual: version },
        ));
    }
    let header_length = read_u16(header, 10);
    if header_length != CONTROL_HEADER_LENGTH_U16 {
        return Err(FileRestartCheckpointSlotFormatError::new(
            10,
            FileRestartCheckpointSlotFormatErrorReason::HeaderLength {
                actual: header_length,
            },
        ));
    }
    let flags = read_u32(header, 12);
    if flags != 0 {
        return Err(FileRestartCheckpointSlotFormatError::new(
            12,
            FileRestartCheckpointSlotFormatErrorReason::HeaderFlags { actual: flags },
        ));
    }
    let persistent_log_id = PersistentLogId::new(read_u128(header, 16)).ok_or_else(|| {
        FileRestartCheckpointSlotFormatError::new(
            16,
            FileRestartCheckpointSlotFormatErrorReason::PersistentLogIdZero,
        )
    })?;
    if let Some((relative_offset, actual)) = header[CONTROL_RESERVED_START..CONTROL_CHECKSUM_OFFSET]
        .iter()
        .copied()
        .enumerate()
        .find(|(_, byte)| *byte != 0)
    {
        let offset = CONTROL_RESERVED_START + relative_offset;
        return Err(FileRestartCheckpointSlotFormatError::new(
            offset as u64,
            FileRestartCheckpointSlotFormatErrorReason::ReservedByteNonZero { offset, actual },
        ));
    }
    let actual_checksum = read_u64(header, CONTROL_CHECKSUM_OFFSET);
    let expected_checksum = checksum_v1(&header[..CONTROL_CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(FileRestartCheckpointSlotFormatError::new(
            CONTROL_CHECKSUM_OFFSET as u64,
            FileRestartCheckpointSlotFormatErrorReason::HeaderChecksum {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        ));
    }
    Ok(persistent_log_id)
}

/// Creates, locks, writes, and synchronizes one new lineaged control slot.
///
/// The exclusive control lock is acquired before the header is written or
/// synchronized, so a cooperating adapter cannot observe an in-progress
/// creation through a successfully opened slot. No rollback or best-effort
/// cleanup is attempted after a partial failure.
pub(crate) fn create_locked_control_slot(
    slot_directory: &Path,
    magic: [u8; 8],
    persistent_log_id: PersistentLogId,
) -> Result<(File, File), FileRestartCheckpointSlotCreateError> {
    let control_path = slot_directory.join(CONTROL_FILE_NAME);

    fs::create_dir(slot_directory).map_err(|source| {
        FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::CreateSlotDirectory,
            source,
        ))
    })?;
    let mut control_file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&control_path)
        .map_err(|source| {
            FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
                FileRestartCheckpointSlotIoStage::CreateControlFile,
                source,
            ))
        })?;
    control_file.try_lock().map_err(|source| {
        FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::AcquireExclusiveControlLock,
            source.into(),
        ))
    })?;

    let header = build_slot_control_header(magic, persistent_log_id);
    control_file.write_all(&header).map_err(|source| {
        FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::WriteControlHeader,
            source,
        ))
    })?;
    control_file.sync_all().map_err(|source| {
        FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::SyncControlFile,
            source,
        ))
    })?;

    let directory = File::open(slot_directory).map_err(|source| {
        FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::OpenSlotDirectory,
            source,
        ))
    })?;
    directory.sync_all().map_err(|source| {
        FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::SyncSlotDirectory,
            source,
        ))
    })?;
    sync_parent_directory(slot_directory)?;
    Ok((control_file, directory))
}

/// Opens and exclusively locks one control file before parsing its bytes.
///
/// The returned control file and slot-directory handle are the adapter's
/// lifetime lock and later directory-synchronization ownership.
pub(crate) fn open_locked_control_slot(
    slot_directory: &Path,
    magic: [u8; 8],
) -> Result<(File, File, PersistentLogId), FileRestartCheckpointSlotOpenError> {
    let control_path = slot_directory.join(CONTROL_FILE_NAME);
    let mut control_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(control_path)
        .map_err(|source| {
            FileRestartCheckpointSlotOpenError::Io(FileRestartCheckpointSlotIoError::new(
                FileRestartCheckpointSlotIoStage::OpenControlFile,
                source,
            ))
        })?;
    control_file.try_lock().map_err(|source| {
        FileRestartCheckpointSlotOpenError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::AcquireExclusiveControlLock,
            source.into(),
        ))
    })?;

    let control_length = control_file
        .metadata()
        .map_err(|source| {
            FileRestartCheckpointSlotOpenError::Io(FileRestartCheckpointSlotIoError::new(
                FileRestartCheckpointSlotIoStage::ReadControlMetadata,
                source,
            ))
        })?
        .len();
    if control_length != CONTROL_HEADER_LENGTH as u64 {
        return Err(FileRestartCheckpointSlotOpenError::Format(
            FileRestartCheckpointSlotFormatError::new(
                0,
                FileRestartCheckpointSlotFormatErrorReason::FileLength {
                    actual: control_length,
                },
            ),
        ));
    }

    let mut header = [0_u8; CONTROL_HEADER_LENGTH];
    control_file.read_exact(&mut header).map_err(|source| {
        FileRestartCheckpointSlotOpenError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::ReadControlHeader,
            source,
        ))
    })?;
    let persistent_log_id = parse_slot_control_header(magic, &header)
        .map_err(FileRestartCheckpointSlotOpenError::Format)?;
    let directory = File::open(slot_directory).map_err(|source| {
        FileRestartCheckpointSlotOpenError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::OpenSlotDirectory,
            source,
        ))
    })?;
    Ok((control_file, directory, persistent_log_id))
}

/// Reads the complete optional selected `current` value of one slot.
///
/// `Ok(None)` means exactly one existing slot directory with no `current`
/// entry. A dangling link, directory, removed slot, access failure, short
/// read, metadata failure, host-length overflow, capacity exhaustion, or
/// length race is reported instead of absence.
pub(crate) fn read_current_slot_bytes(
    slot_directory: &Path,
) -> Result<Option<Vec<u8>>, SlotCurrentReadError> {
    let current_path = slot_directory.join(CURRENT_FILE_NAME);
    let mut current_file = match File::open(&current_path) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            match fs::symlink_metadata(&current_path) {
                Ok(_) => {
                    return Err(SlotCurrentReadError::Io(
                        FileRestartCheckpointSlotIoError::new(
                            FileRestartCheckpointSlotIoStage::OpenCurrentFile,
                            source,
                        ),
                    ));
                }
                Err(absence_source) if absence_source.kind() == io::ErrorKind::NotFound => {
                    match fs::metadata(slot_directory) {
                        Ok(metadata) if metadata.is_dir() => return Ok(None),
                        Ok(_) => {
                            return Err(SlotCurrentReadError::Io(
                                FileRestartCheckpointSlotIoError::new(
                                    FileRestartCheckpointSlotIoStage::OpenCurrentFile,
                                    source,
                                ),
                            ));
                        }
                        Err(parent_source) => {
                            return Err(SlotCurrentReadError::Io(
                                FileRestartCheckpointSlotIoError::new(
                                    FileRestartCheckpointSlotIoStage::VerifyCurrentAbsence,
                                    parent_source,
                                ),
                            ));
                        }
                    }
                }
                Err(absence_source) => {
                    return Err(SlotCurrentReadError::Io(
                        FileRestartCheckpointSlotIoError::new(
                            FileRestartCheckpointSlotIoStage::VerifyCurrentAbsence,
                            absence_source,
                        ),
                    ));
                }
            }
        }
        Err(source) => {
            return Err(SlotCurrentReadError::Io(
                FileRestartCheckpointSlotIoError::new(
                    FileRestartCheckpointSlotIoStage::OpenCurrentFile,
                    source,
                ),
            ));
        }
    };
    let before = current_file
        .metadata()
        .map_err(|source| {
            SlotCurrentReadError::Io(FileRestartCheckpointSlotIoError::new(
                FileRestartCheckpointSlotIoStage::ReadCurrentMetadataBeforeRead,
                source,
            ))
        })?
        .len();
    let length = usize::try_from(before)
        .map_err(|_| SlotCurrentReadError::CurrentLengthOutOfRange { actual: before })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| SlotCurrentReadError::CurrentCapacityExhausted { length })?;
    bytes.resize(length, 0);
    current_file.read_exact(&mut bytes).map_err(|source| {
        SlotCurrentReadError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::ReadCurrentBytes,
            source,
        ))
    })?;
    let after = current_file
        .metadata()
        .map_err(|source| {
            SlotCurrentReadError::Io(FileRestartCheckpointSlotIoError::new(
                FileRestartCheckpointSlotIoStage::ReadCurrentMetadataAfterRead,
                source,
            ))
        })?
        .len();
    if before != after {
        return Err(SlotCurrentReadError::CurrentLengthChanged { before, after });
    }
    Ok(Some(bytes))
}

/// Performs one complete atomic replacement of a slot's selected `current`.
///
/// The caller supplies already encoded bytes; this operation performs no
/// encoding, permit check, or slot-identity check. `fault` reports whether a
/// deterministic test fault is armed at each exact physical boundary and
/// consumes it when it fires.
pub(crate) fn publish_slot_current_bytes<Fault>(
    slot_directory: &Path,
    directory: &File,
    encoded: &[u8],
    mut fault: Fault,
) -> Result<(), SlotPublicationError>
where
    Fault: FnMut(SlotPublicationStep) -> bool,
{
    if fault(SlotPublicationStep::BeforeCandidateCleanup) {
        return Err(SlotPublicationError::InjectedFault(
            SlotPublicationStep::BeforeCandidateCleanup,
        ));
    }

    let candidate_path = slot_directory.join(CANDIDATE_FILE_NAME);
    match fs::remove_file(&candidate_path) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(SlotPublicationError::Io(
                FileRestartCheckpointSlotIoError::new(
                    FileRestartCheckpointSlotIoStage::RemoveCandidateFile,
                    source,
                ),
            ));
        }
    }
    if fault(SlotPublicationStep::AfterCandidateCleanup) {
        return Err(SlotPublicationError::InjectedFault(
            SlotPublicationStep::AfterCandidateCleanup,
        ));
    }

    let mut candidate_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&candidate_path)
        .map_err(|source| {
            SlotPublicationError::Io(FileRestartCheckpointSlotIoError::new(
                FileRestartCheckpointSlotIoStage::CreateCandidateFile,
                source,
            ))
        })?;
    if fault(SlotPublicationStep::AfterCandidateCreate) {
        return Err(SlotPublicationError::InjectedFault(
            SlotPublicationStep::AfterCandidateCreate,
        ));
    }

    candidate_file.write_all(encoded).map_err(|source| {
        SlotPublicationError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::WriteCandidateFile,
            source,
        ))
    })?;
    if fault(SlotPublicationStep::AfterCandidateWrite) {
        return Err(SlotPublicationError::InjectedFault(
            SlotPublicationStep::AfterCandidateWrite,
        ));
    }

    candidate_file.sync_all().map_err(|source| {
        SlotPublicationError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::SyncCandidateFile,
            source,
        ))
    })?;
    drop(candidate_file);
    if fault(SlotPublicationStep::AfterCandidateSync) {
        return Err(SlotPublicationError::InjectedFault(
            SlotPublicationStep::AfterCandidateSync,
        ));
    }

    let current_path = slot_directory.join(CURRENT_FILE_NAME);
    fs::rename(&candidate_path, &current_path).map_err(|source| {
        SlotPublicationError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::ReplaceCurrentFile,
            source,
        ))
    })?;
    if fault(SlotPublicationStep::AfterCurrentReplace) {
        return Err(SlotPublicationError::InjectedFault(
            SlotPublicationStep::AfterCurrentReplace,
        ));
    }

    directory.sync_all().map_err(|source| {
        SlotPublicationError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::SyncPublishedSlotDirectory,
            source,
        ))
    })?;
    if fault(SlotPublicationStep::AfterDirectorySync) {
        Err(SlotPublicationError::InjectedFault(
            SlotPublicationStep::AfterDirectorySync,
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn build_control_header(persistent_log_id: PersistentLogId) -> [u8; CONTROL_HEADER_LENGTH] {
    build_slot_control_header(CONTROL_MAGIC, persistent_log_id)
}

#[cfg(test)]
fn parse_control_header(
    header: &[u8; CONTROL_HEADER_LENGTH],
) -> Result<PersistentLogId, FileRestartCheckpointSlotFormatError> {
    parse_slot_control_header(CONTROL_MAGIC, header)
}

fn sync_parent_directory(
    slot_directory: &Path,
) -> Result<(), FileRestartCheckpointSlotCreateError> {
    let parent = match slot_directory.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => return Err(FileRestartCheckpointSlotCreateError::MissingParentDirectory),
    };
    let parent = File::open(parent).map_err(|source| {
        FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::OpenParentDirectory,
            source,
        ))
    })?;
    parent.sync_all().map_err(|source| {
        FileRestartCheckpointSlotCreateError::Io(FileRestartCheckpointSlotIoError::new(
            FileRestartCheckpointSlotIoStage::SyncParentDirectory,
            source,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_header_round_trips_exact_persistent_id() -> Result<(), io::Error> {
        let persistent_log_id = PersistentLogId::new(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)
            .ok_or_else(|| io::Error::other("test persistent log ID is zero"))?;
        let header = build_control_header(persistent_log_id);
        let expected = [
            0x4e, 0x54, 0x53, 0x51, 0x43, 0x4b, 0x53, 0x31, 0x00, 0x01, 0x00, 0x40, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xee, 0xd7, 0x4c, 0xab, 0xff, 0x69, 0xc4, 0xff,
        ];

        assert_eq!(header, expected);
        assert_eq!(&header[..8], b"NTSQCKS1");
        assert_eq!(read_u16(&header, 8), 1);
        assert_eq!(read_u16(&header, 10), 64);
        assert_eq!(read_u32(&header, 12), 0);
        assert_eq!(read_u128(&header, 16), persistent_log_id.get());
        assert!(header[32..56].iter().all(|byte| *byte == 0));
        assert_eq!(
            read_u64(&header, 56),
            checksum_v1(&header[..CONTROL_CHECKSUM_OFFSET])
        );
        assert_eq!(parse_control_header(&header), Ok(persistent_log_id));
        Ok(())
    }

    #[test]
    fn control_header_rejects_each_canonical_field() -> Result<(), io::Error> {
        let persistent_log_id = PersistentLogId::new(1)
            .ok_or_else(|| io::Error::other("test persistent log ID is zero"))?;
        let valid = build_control_header(persistent_log_id);

        let mut magic = valid;
        magic[0] ^= 1;
        assert!(matches!(
            parse_control_header(&magic),
            Err(FileRestartCheckpointSlotFormatError {
                offset: 0,
                reason: FileRestartCheckpointSlotFormatErrorReason::HeaderMagic { .. },
            })
        ));

        let mut version = valid;
        write_u16(&mut version, 8, 2);
        assert_eq!(
            parse_control_header(&version),
            Err(FileRestartCheckpointSlotFormatError::new(
                8,
                FileRestartCheckpointSlotFormatErrorReason::HeaderVersion { actual: 2 },
            ))
        );

        let mut length = valid;
        write_u16(&mut length, 10, 63);
        assert_eq!(
            parse_control_header(&length),
            Err(FileRestartCheckpointSlotFormatError::new(
                10,
                FileRestartCheckpointSlotFormatErrorReason::HeaderLength { actual: 63 },
            ))
        );

        let mut flags = valid;
        write_u32(&mut flags, 12, 1);
        assert_eq!(
            parse_control_header(&flags),
            Err(FileRestartCheckpointSlotFormatError::new(
                12,
                FileRestartCheckpointSlotFormatErrorReason::HeaderFlags { actual: 1 },
            ))
        );

        let mut zero_id = valid;
        zero_id[16..32].fill(0);
        assert_eq!(
            parse_control_header(&zero_id),
            Err(FileRestartCheckpointSlotFormatError::new(
                16,
                FileRestartCheckpointSlotFormatErrorReason::PersistentLogIdZero,
            ))
        );

        let mut reserved = valid;
        reserved[41] = 7;
        assert_eq!(
            parse_control_header(&reserved),
            Err(FileRestartCheckpointSlotFormatError::new(
                41,
                FileRestartCheckpointSlotFormatErrorReason::ReservedByteNonZero {
                    offset: 41,
                    actual: 7,
                },
            ))
        );

        let mut checksum = valid;
        checksum[63] ^= 1;
        assert!(matches!(
            parse_control_header(&checksum),
            Err(FileRestartCheckpointSlotFormatError {
                offset: 56,
                reason: FileRestartCheckpointSlotFormatErrorReason::HeaderChecksum { .. },
            })
        ));
        Ok(())
    }
}
