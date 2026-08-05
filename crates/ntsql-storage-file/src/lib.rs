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
//! ## Checksum
//!
//! The v1/v2 checksum is an ntsql-owned, deterministic, non-cryptographic
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

use std::{
    error::Error,
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    num::NonZeroU64,
    path::Path,
};

use ntsql_page::{PageLog, PageNumber, PageVersion, UnloggedPage};
use ntsql_transaction::{
    DurableCommitLookup, TransactionCommitRecord, TransactionEpochSource, TransactionId,
    TransactionRecoverySource,
};
use ntsql_wal::{CommitLog, LogDurability, LogLineage, LogSequenceNumber, PersistentLogId};

const HEADER_MAGIC: [u8; 8] = *b"NTSQLOG1";
const FRAME_MAGIC: [u8; 4] = *b"NTSQ";
const FORMAT_VERSION_V1: u16 = 1;
const FORMAT_VERSION_V2: u16 = 2;
const HEADER_LENGTH: usize = 64;
const HEADER_LENGTH_U16: u16 = 64;
const HEADER_LENGTH_U64: u64 = 64;
const FRAME_LENGTH: usize = 56;
const FRAME_LENGTH_U16: u16 = 56;
const FRAME_LENGTH_U64: u64 = 56;
const HEADER_CHECKSUM_OFFSET: usize = 56;
const HEADER_V2_PAGE_WIDTH_OFFSET: usize = 32;
const HEADER_V2_RESERVED_OFFSET: usize = 40;
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
}

impl LogFormat {
    const fn version(self) -> u16 {
        match self {
            Self::V1 => FORMAT_VERSION_V1,
            Self::V2 => FORMAT_VERSION_V2,
        }
    }

    const fn supports_pages(self) -> bool {
        match self {
            Self::V1 => false,
            Self::V2 => true,
        }
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
}

impl fmt::Display for FaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeAppend => formatter.write_str("before append"),
            Self::AfterAppend => formatter.write_str("after append"),
            Self::BeforeFlush => formatter.write_str("before flush"),
            Self::AfterFlush => formatter.write_str("after flush"),
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
    WritePageDataFrame,
    SyncCommitPrefix,
    WriteDurableMarker,
    SyncDurableMarker,
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
            Self::WritePageDataFrame => formatter.write_str("writing a page-data frame"),
            Self::SyncCommitPrefix => {
                formatter.write_str("synchronizing the requested durable prefix")
            }
            Self::WriteDurableMarker => formatter.write_str("writing a durable-through marker"),
            Self::SyncDurableMarker => {
                formatter.write_str("synchronizing a durable-through marker")
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
    HeaderTooShort { actual: u64 },
    HeaderMagic,
    HeaderVersion { actual: u16 },
    HeaderLength { actual: u16 },
    HeaderFlags { actual: u32 },
    HeaderPageWidthZero,
    HeaderPageWidthMismatch { expected: u64, actual: u64 },
    HeaderReserved,
    HeaderChecksum { expected: u64, actual: u64 },
    LineageIdZero,
    FrameMagic,
    FrameKind { actual: u16 },
    FrameVersion { actual: u16 },
    FrameLength { actual: u16 },
    FrameFlags { actual: u32 },
    FrameReserved,
    FrameChecksum { expected: u64, actual: u64 },
    UnexpectedNonzeroPayload { field: &'static str, actual: u64 },
    EpochValueZero,
    EpochOutOfSequence { expected: u64, actual: u64 },
    EpochSpaceExhausted,
    CommitPositionZero,
    CommitPositionOutOfSequence { expected: u64, actual: u64 },
    CommitPositionSpaceExhausted,
    CommitEpochZero,
    CommitEpochUnallocated { actual: u64, highest_allocated: u64 },
    CommitSequenceZero,
    DuplicateTransactionIdentity { epoch: u64, sequence: u64 },
    MarkerPositionZero,
    MarkerDoesNotAdvance { previous: u64, actual: u64 },
    MarkerReferencesUnknownCommit { actual: u64, highest_committed: u64 },
    PageHeaderPositionZero,
    PageHeaderPositionOutOfSequence { expected: u64, actual: u64 },
    PageHeaderPositionSpaceExhausted,
    PageNumberZero,
    PageDataWithoutHeader,
    PageDataParentPositionZero,
    PageDataParentMismatch { expected: u64, actual: u64 },
    PageDataChunkIndexOutOfSequence { expected: u64, actual: u64 },
    PageDataInterruptedByFrameKind { actual: u16 },
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
            Self::HeaderFlags { actual } => {
                write!(formatter, "header flags are nonzero: {actual}")
            }
            Self::HeaderPageWidthZero => formatter.write_str("v2 header page width is zero"),
            Self::HeaderPageWidthMismatch { expected, actual } => write!(
                formatter,
                "v2 header page width {actual} does not equal required width {expected}"
            ),
            Self::HeaderReserved => formatter.write_str("header reserved bytes are nonzero"),
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
}

impl<const N: usize> FileLogRecordKind<N> {
    /// Returns the transaction epoch when this record is a commit.
    #[must_use]
    pub const fn transaction_epoch(&self) -> Option<u64> {
        match self {
            Self::TransactionCommit {
                transaction_epoch, ..
            } => Some(*transaction_epoch),
            Self::PageWrite(_) => None,
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
            Self::PageWrite(_) => None,
        }
    }

    /// Returns the full page-image payload when this record is a page write.
    #[must_use]
    pub const fn page_write(&self) -> Option<&FilePageWriteRecord<N>> {
        match self {
            Self::TransactionCommit { .. } => None,
            Self::PageWrite(record) => Some(record),
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
            FileLogRecordKind::PageWrite(_) => None,
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
}

/// Inspectable filesystem-backed implementation of the transaction commit-log
/// port and, in v2, the page WAL port.
#[derive(Debug)]
pub struct FileCommitLog<const N: usize = 0> {
    file: File,
    lineage: LogLineage,
    persistent_id: PersistentLogId,
    records: Vec<FileLogRecord<N>>,
    durable_len: usize,
    next_epoch: Option<NonZeroU64>,
    next_position: Option<u64>,
    armed_fault: Option<FaultPoint>,
    poisoned: bool,
}

impl FileCommitLog<0> {
    /// Creates a new empty v1 file with one caller-supplied persistent lineage ID.
    pub fn create_new<P>(path: P, persistent_id: PersistentLogId) -> Result<Self, FileCreateError>
    where
        P: AsRef<Path>,
    {
        Self::create_new_internal(path.as_ref(), persistent_id, build_header_v1(persistent_id))
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
            build_header_v2(persistent_id, layout.width_u64),
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

    fn create_new_internal(
        path: &Path,
        persistent_id: PersistentLogId,
        header: [u8; HEADER_LENGTH],
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

        file.write_all(&header).map_err(|source| {
            FileCreateError::Io(FileIoError::new(FileIoStage::WriteHeader, source))
        })?;
        file.sync_all().map_err(|source| {
            FileCreateError::Io(FileIoError::new(FileIoStage::SyncCreatedFile, source))
        })?;
        sync_parent_directory(path)?;
        file.seek(SeekFrom::End(0)).map_err(|source| {
            FileCreateError::Io(FileIoError::new(FileIoStage::SeekEnd, source))
        })?;

        Ok(Self {
            file,
            lineage: LogLineage::persistent(persistent_id),
            persistent_id,
            records: Vec::new(),
            durable_len: 0,
            next_epoch: Some(NonZeroU64::MIN),
            next_position: Some(1),
            armed_fault: None,
            poisoned: false,
        })
    }

    fn open_internal(path: &Path, expectation: HeaderExpectation) -> Result<Self, FileOpenError> {
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
        let persistent_id = parse_header(&header, expectation).map_err(FileOpenError::Format)?;
        let lineage = LogLineage::persistent(persistent_id);
        let mut open_state = OpenState::new(lineage.clone(), expectation.page_layout());

        let frame_region_len = file_len - HEADER_LENGTH_U64;
        let complete_frame_count = frame_region_len / FRAME_LENGTH_U64;
        let incomplete_tail_len = frame_region_len % FRAME_LENGTH_U64;

        for frame_index in 0..complete_frame_count {
            let mut frame = [0_u8; FRAME_LENGTH];
            file.read_exact(&mut frame).map_err(|source| {
                FileOpenError::Io(FileIoError::new(FileIoStage::ReadFrame, source))
            })?;
            let offset = HEADER_LENGTH_U64 + frame_index * FRAME_LENGTH_U64;
            let decoded = parse_frame(&frame, offset, expectation.log_format())
                .map_err(FileOpenError::Format)?;
            open_state.apply_frame(decoded, offset)?;
        }

        let repaired_len = match open_state.pending_page_header_offset() {
            Some(offset) => Some(offset),
            None if incomplete_tail_len > 0 => {
                Some(HEADER_LENGTH_U64 + complete_frame_count * FRAME_LENGTH_U64)
            }
            None => None,
        };
        if let Some(repaired_len) = repaired_len {
            file.set_len(repaired_len).map_err(|source| {
                FileOpenError::Io(FileIoError::new(
                    FileIoStage::TruncateIncompleteTail,
                    source,
                ))
            })?;
            file.sync_all().map_err(|source| {
                FileOpenError::Io(FileIoError::new(FileIoStage::SyncTruncatedTail, source))
            })?;
        }
        file.seek(SeekFrom::End(0))
            .map_err(|source| FileOpenError::Io(FileIoError::new(FileIoStage::SeekEnd, source)))?;

        Ok(Self {
            file,
            lineage,
            persistent_id,
            records: open_state.records,
            durable_len: open_state.durable_len,
            next_epoch: open_state.next_epoch,
            next_position: open_state.next_position,
            armed_fault: None,
            poisoned: false,
        })
    }

    /// Returns the stable persistent lineage ID reconstructed from the header.
    #[must_use]
    pub const fn persistent_id(&self) -> PersistentLogId {
        self.persistent_id
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
        if self.poisoned {
            return Err(FileCommitLogError::PoisonedWriter);
        }
        if N == 0 {
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

        let header = build_frame(
            LogFormat::V2,
            FrameKind::PageHeader,
            position_value,
            page.address().number().get(),
            page.version().get(),
        );
        self.write_frame(&header, FileIoStage::WritePageHeaderFrame, true)
            .map_err(FileCommitLogError::Io)?;

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
                LogFormat::V2,
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
        self.records.push(FileLogRecord {
            position: position.clone(),
            kind: FileLogRecordKind::PageWrite(FilePageWriteRecord {
                page_number: page.address().number(),
                page_version: page.version(),
                bytes: stored_bytes,
            }),
        });
        self.next_position = position_value.checked_add(1);

        if self.consume_fault(FaultPoint::AfterAppend) {
            Err(FileCommitLogError::InjectedFault(FaultPoint::AfterAppend))
        } else {
            Ok(position)
        }
    }
}

impl<const N: usize> TransactionEpochSource for FileCommitLog<N> {
    type Error = FileTransactionEpochError;

    fn allocate_transaction_epoch(&mut self) -> Result<(NonZeroU64, LogLineage), Self::Error> {
        if self.poisoned {
            return Err(FileTransactionEpochError::PoisonedWriter);
        }
        let epoch = self
            .next_epoch
            .ok_or(FileTransactionEpochError::EpochSpaceExhausted)?;
        let frame = build_frame(
            log_format_for_width::<N>(),
            FrameKind::EpochAllocation,
            epoch.get(),
            0,
            0,
        );
        self.write_frame(&frame, FileIoStage::WriteEpochFrame, true)
            .map_err(FileTransactionEpochError::Io)?;
        self.sync_file(FileIoStage::SyncEpochFrame, true)
            .map_err(FileTransactionEpochError::Io)?;
        self.next_epoch = epoch.get().checked_add(1).and_then(NonZeroU64::new);
        Ok((epoch, self.lineage.clone()))
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
        let marker = build_frame(
            log_format_for_width::<N>(),
            FrameKind::DurableThrough,
            position.get(),
            0,
            0,
        );
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
            log_format_for_width::<N>(),
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

const fn log_format_for_width<const N: usize>() -> LogFormat {
    if N == 0 { LogFormat::V1 } else { LogFormat::V2 }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
enum FrameKind {
    EpochAllocation = 1,
    CommitRecord = 2,
    DurableThrough = 3,
    PageHeader = 4,
    PageData = 5,
}

impl FrameKind {
    fn from_u16(value: u16, format: LogFormat) -> Option<Self> {
        match value {
            1 => Some(Self::EpochAllocation),
            2 => Some(Self::CommitRecord),
            3 => Some(Self::DurableThrough),
            4 if format.supports_pages() => Some(Self::PageHeader),
            5 if format.supports_pages() => Some(Self::PageData),
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
    fn new(lineage: LogLineage, page_layout: Option<PageLayout>) -> Self {
        Self {
            lineage,
            page_layout,
            records: Vec::new(),
            durable_len: 0,
            last_durable_position: None,
            last_completed_position: 0,
            highest_allocated_epoch: 0,
            next_epoch: Some(NonZeroU64::MIN),
            next_position: Some(1),
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
            FrameKind::PageHeader => self.apply_page_header_frame(frame, offset),
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
            let record = pending.into_record(&self.lineage);
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
            layout,
        ));
        Ok(())
    }
}

#[derive(Debug)]
struct PendingPageRecord<const N: usize> {
    header_offset: u64,
    position: u64,
    page_number: PageNumber,
    page_version: PageVersion,
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
        layout: PageLayout,
    ) -> Self {
        Self {
            header_offset,
            position,
            page_number,
            page_version,
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

    fn into_record(self, lineage: &LogLineage) -> FileLogRecord<N> {
        FileLogRecord {
            position: lineage.position(self.position),
            kind: FileLogRecordKind::PageWrite(FilePageWriteRecord {
                page_number: self.page_number,
                page_version: self.page_version,
                bytes: self.bytes,
            }),
        }
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), FileCreateError> {
    let parent = match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => return Err(FileCreateError::MissingParentDirectory),
    };
    let directory = File::open(parent).map_err(|source| {
        FileCreateError::Io(FileIoError::new(FileIoStage::OpenParentDirectory, source))
    })?;
    directory.sync_all().map_err(|source| {
        FileCreateError::Io(FileIoError::new(FileIoStage::SyncParentDirectory, source))
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderExpectation {
    V1,
    V2(PageLayout),
}

impl HeaderExpectation {
    const fn log_format(self) -> LogFormat {
        match self {
            Self::V1 => LogFormat::V1,
            Self::V2(_) => LogFormat::V2,
        }
    }

    const fn page_layout(self) -> Option<PageLayout> {
        match self {
            Self::V1 => None,
            Self::V2(layout) => Some(layout),
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
    if version != expectation.log_format().version() {
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
        HeaderExpectation::V2(layout) => {
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
    if version != format.version() {
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
    if format == LogFormat::V2 {
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
    write_u16(&mut frame, 6, format.version());
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

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use ntsql_page::{
        PageAddress, PageImage, PageLog, PageNumber, PageVersion, StagePageWriteError,
        stage_page_write,
    };
    use ntsql_transaction::{
        CoordinatedCommitError, IndeterminateTransaction, TransactionCommitResolution,
        TransactionCoordinator, TransactionLifecycleStatus, TransactionResolutionFailure,
    };
    use ntsql_wal::{CommitError, LogDurability, LogLineage, PersistentLogId};

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

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
}
