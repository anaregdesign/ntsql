//! Pure-memory codec for the ADR 0048 restart checkpoint completeness baseline.
//!
//! This module is an independent version-1 namespace. It does not extend,
//! reinterpret, or wrap the ADR 0044 `NTSQCKP1` transaction-only blob defined
//! by [`crate::restart_checkpoint_codec`]; the two formats share no magic,
//! footer, or version dispatch and evolve independently.

use std::{error::Error, fmt};

use ntsql_transaction::{
    DurableTransactionRestartCheckpointBaselineEntry,
    DurableTransactionRestartCheckpointBaselineEntryObservation,
    DurableTransactionRestartCheckpointBaselineState,
    DurableTransactionRestartCheckpointBaselineStateObservation,
    DurableTransactionRestartCheckpointCompletenessBaseline,
    DurableTransactionRestartCheckpointCompletenessBaselinePageObservation,
    DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation,
    DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation,
    DurableTransactionRestartPageEntry, DurableTransactionRestartPageState,
    DurableTransactionRestartReplayStart, DurableTransactionRestartReplayStartCause,
    DurableTransactionRestartRequiredPageImage,
    OwnedDurableTransactionRestartCheckpointBaselineObservation,
    OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
};

const HEADER_MAGIC: [u8; 8] = *b"NTSQCMP1";
const FOOTER_MAGIC: [u8; 8] = *b"NTSQCME1";
const FORMAT_VERSION: u16 = 1;
const HEADER_LENGTH: usize = 128;
const HEADER_LENGTH_U16: u16 = 128;
const HEADER_LENGTH_U64: u64 = 128;
const TRANSACTION_ENTRY_LENGTH: usize = 64;
const TRANSACTION_ENTRY_LENGTH_U16: u16 = 64;
const TRANSACTION_ENTRY_LENGTH_U64: u64 = 64;
const PAGE_ENTRY_LENGTH: usize = 64;
const PAGE_ENTRY_LENGTH_U16: u16 = 64;
const PAGE_ENTRY_LENGTH_U64: u64 = 64;
const FOOTER_LENGTH: usize = 16;
const FOOTER_LENGTH_U16: u16 = 16;
const FOOTER_LENGTH_U64: u64 = 16;

const PERSISTENT_LOG_ID_OFFSET: usize = 24;
const DURABLE_FRONTIER_OFFSET: usize = 40;
const DURABLE_FRONTIER_PRESENCE_OFFSET: usize = 48;
const HEADER_RESERVED_A_START: usize = 18;
const HEADER_RESERVED_A_END: usize = 24;
const HEADER_RESERVED_B_START: usize = 49;
const HEADER_RESERVED_B_END: usize = 56;
const HEADER_RESERVED_C_START: usize = 84;
const HEADER_RESERVED_C_END: usize = 88;
const TRANSACTION_COUNT_OFFSET: usize = 56;
const PAGE_COUNT_OFFSET: usize = 64;
const TOTAL_LENGTH_OFFSET: usize = 72;
const REPLAY_KIND_OFFSET: usize = 80;
const REPLAY_FRONTIER_PRESENCE_OFFSET: usize = 81;
const REPLAY_POSITION_PRESENCE_OFFSET: usize = 82;
const REPLAY_CAUSE_DISCRIMINANT_OFFSET: usize = 83;
const REPLAY_FRONTIER_OFFSET: usize = 88;
const REPLAY_POSITION_OFFSET: usize = 96;
const REPLAY_CAUSE_PAGE_NUMBER_OFFSET: usize = 104;
const REPLAY_CAUSE_EPOCH_OFFSET: usize = 112;
const REPLAY_CAUSE_SEQUENCE_OFFSET: usize = 120;

const TRANSACTION_ENTRY_EPOCH_OFFSET: usize = 0;
const TRANSACTION_ENTRY_SEQUENCE_OFFSET: usize = 8;
const TRANSACTION_ENTRY_FIRST_OWNED_PAGE_POSITION_OFFSET: usize = 16;
const TRANSACTION_ENTRY_LAST_OWNED_PAGE_POSITION_OFFSET: usize = 24;
const TRANSACTION_ENTRY_OWNED_PAGE_RECORD_COUNT_OFFSET: usize = 32;
const TRANSACTION_ENTRY_COMMIT_POSITION_OFFSET: usize = 40;
const TRANSACTION_ENTRY_STATE_OFFSET: usize = 48;
const TRANSACTION_ENTRY_FIRST_POSITION_PRESENCE_OFFSET: usize = 49;
const TRANSACTION_ENTRY_LAST_POSITION_PRESENCE_OFFSET: usize = 50;
const TRANSACTION_ENTRY_RESERVED_START: usize = 51;
const TRANSACTION_ENTRY_RESERVED_END: usize = 64;

const PAGE_ENTRY_NUMBER_OFFSET: usize = 0;
const PAGE_ENTRY_STATE_OFFSET: usize = 8;
const PAGE_ENTRY_REQUIRED_IMAGE_PRESENCE_OFFSET: usize = 9;
const PAGE_ENTRY_REQUIRED_IMAGE_KIND_OFFSET: usize = 10;
const PAGE_ENTRY_STORED_POSITION_PRESENCE_OFFSET: usize = 11;
const PAGE_ENTRY_RESERVED_A_START: usize = 12;
const PAGE_ENTRY_RESERVED_A_END: usize = 16;
const PAGE_ENTRY_REQUIRED_IMAGE_PAGE_POSITION_OFFSET: usize = 16;
const PAGE_ENTRY_REQUIRED_IMAGE_EPOCH_OFFSET: usize = 24;
const PAGE_ENTRY_REQUIRED_IMAGE_SEQUENCE_OFFSET: usize = 32;
const PAGE_ENTRY_REQUIRED_IMAGE_COMMIT_POSITION_OFFSET: usize = 40;
const PAGE_ENTRY_STORED_POSITION_OFFSET: usize = 48;
const PAGE_ENTRY_RESERVED_B_START: usize = 56;
const PAGE_ENTRY_RESERVED_B_END: usize = 64;

const ABSENT: u8 = 0;
const PRESENT: u8 = 1;
const STATE_UNCOMMITTED: u8 = 0;
const STATE_COMMITTED: u8 = 1;

const PAGE_STATE_NO_REQUIRED_IMAGE: u8 = 0;
const PAGE_STATE_STORE_MISSING: u8 = 1;
const PAGE_STATE_STORE_CURRENT: u8 = 2;
const PAGE_STATE_STORE_BEHIND: u8 = 3;

const REQUIRED_IMAGE_KIND_RAW: u8 = 0;
const REQUIRED_IMAGE_KIND_COMMITTED_TRANSACTION: u8 = 1;

const REPLAY_KIND_AFTER_FRONTIER: u8 = 0;
const REPLAY_KIND_AT_POSITION: u8 = 1;

const REPLAY_CAUSE_ABSENT: u8 = 0;
const REPLAY_CAUSE_STORE_MISSING_PAGE: u8 = 1;
const REPLAY_CAUSE_STORE_BEHIND_PAGE: u8 = 2;
const REPLAY_CAUSE_UNCOMMITTED_TRANSACTION: u8 = 3;

/// Optional numeric field inside one encoded completeness transaction entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointCompletenessBaselineEntryOptionalField {
    /// First transaction-owned page-record position.
    FirstOwnedPagePosition,
    /// Last transaction-owned page-record position.
    LastOwnedPagePosition,
}

impl fmt::Display for RestartCheckpointCompletenessBaselineEntryOptionalField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FirstOwnedPagePosition => formatter.write_str("first owned-page position"),
            Self::LastOwnedPagePosition => formatter.write_str("last owned-page position"),
        }
    }
}

/// Required-image numeric field inside one encoded completeness page entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointCompletenessBaselineRequiredImageField {
    /// Numeric position of the required full image.
    PagePosition,
    /// Coordinator epoch of a committed-transaction required image.
    Epoch,
    /// Coordinator-local sequence of a committed-transaction required image.
    Sequence,
    /// Numeric commit position of a committed-transaction required image.
    CommitPosition,
}

impl fmt::Display for RestartCheckpointCompletenessBaselineRequiredImageField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PagePosition => formatter.write_str("required-image page position"),
            Self::Epoch => formatter.write_str("required-image epoch"),
            Self::Sequence => formatter.write_str("required-image sequence"),
            Self::CommitPosition => formatter.write_str("required-image commit position"),
        }
    }
}

/// Replay-cause numeric field inside the encoded completeness header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointCompletenessBaselineReplayCauseField {
    /// Numeric page number of a page-relative replay cause.
    PageNumber,
    /// Coordinator epoch of an uncommitted-transaction replay cause.
    Epoch,
    /// Coordinator-local sequence of an uncommitted-transaction replay cause.
    Sequence,
}

impl fmt::Display for RestartCheckpointCompletenessBaselineReplayCauseField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PageNumber => formatter.write_str("replay-cause page number"),
            Self::Epoch => formatter.write_str("replay-cause epoch"),
            Self::Sequence => formatter.write_str("replay-cause sequence"),
        }
    }
}

/// Failure to encode one authoritative completeness checkpoint baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointCompletenessBaselineEncodeError {
    /// The transaction count cannot be represented by the format's `u64` field.
    TransactionCountOutOfRange {
        /// Exact host-sized count that was rejected.
        transaction_count: usize,
    },
    /// The page count cannot be represented by the format's `u64` field.
    PageCountOutOfRange {
        /// Exact host-sized count that was rejected.
        page_count: usize,
    },
    /// The fixed-width blob length overflowed host-sized arithmetic.
    EncodedLengthOverflow {
        /// Exact transaction count used in the failed calculation.
        transaction_count: usize,
        /// Exact page count used in the failed calculation.
        page_count: usize,
    },
    /// The host-sized encoded length cannot be represented by the format.
    EncodedLengthOutOfRange {
        /// Exact host-sized encoded length that was rejected.
        encoded_length: usize,
    },
    /// The complete output buffer could not reserve its exact required length.
    CapacityExhausted {
        /// Exact byte length that required reservation.
        encoded_length: usize,
    },
}

impl fmt::Display for RestartCheckpointCompletenessBaselineEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransactionCountOutOfRange { transaction_count } => write!(
                formatter,
                "completeness checkpoint transaction count {transaction_count} is not representable as u64"
            ),
            Self::PageCountOutOfRange { page_count } => write!(
                formatter,
                "completeness checkpoint page count {page_count} is not representable as u64"
            ),
            Self::EncodedLengthOverflow {
                transaction_count,
                page_count,
            } => write!(
                formatter,
                "completeness checkpoint encoded length overflowed for {transaction_count} transaction entries and {page_count} page entries"
            ),
            Self::EncodedLengthOutOfRange { encoded_length } => write!(
                formatter,
                "completeness checkpoint encoded length {encoded_length} is not representable as u64"
            ),
            Self::CapacityExhausted { encoded_length } => write!(
                formatter,
                "completeness checkpoint output capacity is exhausted for {encoded_length} bytes"
            ),
        }
    }
}

impl Error for RestartCheckpointCompletenessBaselineEncodeError {}

/// Structural failure to decode one versioned completeness checkpoint blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointCompletenessBaselineDecodeError {
    /// The input ended before the complete declared structural boundary.
    Truncated {
        /// Minimum or declared byte length required at this stage.
        expected_length: usize,
        /// Exact supplied byte length.
        actual_length: usize,
    },
    /// The independent completeness header magic did not match.
    HeaderMagicMismatch {
        /// Exact eight bytes found at the header magic offset.
        actual: [u8; 8],
    },
    /// The completeness format version is not supported.
    UnsupportedVersion {
        /// Exact decoded version.
        actual: u16,
    },
    /// The encoded fixed header length is not version 1's exact width.
    HeaderLengthMismatch {
        /// Exact decoded header length.
        actual: u16,
    },
    /// The encoded fixed transaction-entry length is not version 1's exact width.
    TransactionEntryLengthMismatch {
        /// Exact decoded transaction-entry length.
        actual: u16,
    },
    /// The encoded fixed page-entry length is not version 1's exact width.
    PageEntryLengthMismatch {
        /// Exact decoded page-entry length.
        actual: u16,
    },
    /// The encoded fixed footer length is not version 1's exact width.
    FooterLengthMismatch {
        /// Exact decoded footer length.
        actual: u16,
    },
    /// The transaction count cannot be represented on this host.
    TransactionCountOutOfRange {
        /// Exact decoded count.
        transaction_count: u64,
    },
    /// The page count cannot be represented on this host.
    PageCountOutOfRange {
        /// Exact decoded count.
        page_count: u64,
    },
    /// The fixed-width expected length overflowed the format's `u64` arithmetic.
    EncodedLengthOverflow {
        /// Exact decoded transaction count used in the failed calculation.
        transaction_count: u64,
        /// Exact decoded page count used in the failed calculation.
        page_count: u64,
    },
    /// The declared total length cannot be represented on this host.
    TotalLengthOutOfRange {
        /// Exact decoded total length.
        total_length: u64,
    },
    /// The declared total length disagreed with the fixed geometry and counts.
    DeclaredLengthMismatch {
        /// Exact decoded total length.
        declared: u64,
        /// Exact length implied by version 1 geometry and counts.
        expected: u64,
    },
    /// Bytes followed the one complete declared blob.
    TrailingBytes {
        /// Exact declared and structurally expected byte length.
        expected_length: usize,
        /// Exact supplied byte length.
        actual_length: usize,
    },
    /// The independent completeness footer magic did not match.
    FooterMagicMismatch {
        /// Exact eight bytes found at the footer magic offset.
        actual: [u8; 8],
    },
    /// The protected complete blob checksum did not match.
    ChecksumMismatch {
        /// Checksum computed from every byte before the checksum field.
        expected: u64,
        /// Checksum decoded from the final field.
        actual: u64,
    },
    /// The header frontier presence discriminant was not zero or one.
    FrontierPresenceInvalid {
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// An absent frontier retained a nonzero noncanonical payload.
    AbsentFrontierValueNonZero {
        /// Exact decoded payload.
        actual: u64,
    },
    /// A reserved byte was nonzero.
    ReservedByteNonZero {
        /// Absolute byte offset in the supplied blob.
        offset: usize,
        /// Exact nonzero byte.
        actual: u8,
    },
    /// One transaction-entry state discriminant was not uncommitted or committed.
    EntryStateInvalid {
        /// Zero-based transaction entry index.
        transaction_index: usize,
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// One optional entry-position presence discriminant was not zero or one.
    EntryPositionPresenceInvalid {
        /// Zero-based transaction entry index.
        transaction_index: usize,
        /// Optional field whose discriminant was invalid.
        field: RestartCheckpointCompletenessBaselineEntryOptionalField,
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// An absent optional entry position retained a nonzero payload.
    AbsentEntryPositionValueNonZero {
        /// Zero-based transaction entry index.
        transaction_index: usize,
        /// Optional field whose absent payload was nonzero.
        field: RestartCheckpointCompletenessBaselineEntryOptionalField,
        /// Exact decoded payload.
        actual: u64,
    },
    /// An uncommitted entry retained a nonzero commit-position payload.
    UncommittedPositionValueNonZero {
        /// Zero-based transaction entry index.
        transaction_index: usize,
        /// Exact decoded payload.
        actual: u64,
    },
    /// The complete decoded transaction vector could not reserve its exact count.
    TransactionCapacityExhausted {
        /// Exact host-sized count that required reservation.
        transaction_count: usize,
    },
    /// One page-entry state discriminant was outside the four defined values.
    PageStateInvalid {
        /// Zero-based page entry index.
        page_index: usize,
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// One page entry's required-image presence discriminant was not zero or one.
    RequiredImagePresenceInvalid {
        /// Zero-based page entry index.
        page_index: usize,
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// One page entry's required-image kind discriminant was not raw or committed.
    RequiredImageKindInvalid {
        /// Zero-based page entry index.
        page_index: usize,
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// An absent required image retained a nonzero kind discriminant.
    RequiredImageKindNonZeroWhenAbsent {
        /// Zero-based page entry index.
        page_index: usize,
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// An absent required image retained a nonzero payload field.
    AbsentRequiredImagePayloadNonZero {
        /// Zero-based page entry index.
        page_index: usize,
        /// Required-image field whose absent payload was nonzero.
        field: RestartCheckpointCompletenessBaselineRequiredImageField,
        /// Exact decoded payload.
        actual: u64,
    },
    /// A raw required image retained a nonzero committed-only payload field.
    RawRequiredImagePayloadNonZero {
        /// Zero-based page entry index.
        page_index: usize,
        /// Required-image field whose raw-kind payload was nonzero.
        field: RestartCheckpointCompletenessBaselineRequiredImageField,
        /// Exact decoded payload.
        actual: u64,
    },
    /// One page entry's stored-position presence discriminant was not zero or one.
    StoredPositionPresenceInvalid {
        /// Zero-based page entry index.
        page_index: usize,
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// An absent stored position retained a nonzero payload.
    AbsentStoredPositionValueNonZero {
        /// Zero-based page entry index.
        page_index: usize,
        /// Exact decoded payload.
        actual: u64,
    },
    /// The complete decoded page vector could not reserve its exact count.
    PageCapacityExhausted {
        /// Exact host-sized count that required reservation.
        page_count: usize,
    },
    /// The header replay-kind discriminant was not after-frontier or at-position.
    ReplayKindInvalid {
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// The header replay-frontier presence discriminant was not zero or one.
    ReplayFrontierPresenceInvalid {
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// An absent replay frontier retained a nonzero payload.
    AbsentReplayFrontierValueNonZero {
        /// Exact decoded payload.
        actual: u64,
    },
    /// The header replay-position presence discriminant was not zero or one.
    ReplayPositionPresenceInvalid {
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// An absent replay position retained a nonzero payload.
    AbsentReplayPositionValueNonZero {
        /// Exact decoded payload.
        actual: u64,
    },
    /// The header replay-cause discriminant was outside the four defined values.
    ReplayCauseDiscriminantInvalid {
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// A replay-cause field retained a nonzero payload unused by its discriminant.
    ReplayCauseFieldNonZero {
        /// Replay-cause field whose unused payload was nonzero.
        field: RestartCheckpointCompletenessBaselineReplayCauseField,
        /// Exact decoded payload.
        actual: u64,
    },
}

impl fmt::Display for RestartCheckpointCompletenessBaselineDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                expected_length,
                actual_length,
            } => write!(
                formatter,
                "completeness checkpoint is truncated: expected at least {expected_length} bytes, found {actual_length}"
            ),
            Self::HeaderMagicMismatch { actual } => write!(
                formatter,
                "completeness checkpoint header magic is invalid: {actual:?}"
            ),
            Self::UnsupportedVersion { actual } => write!(
                formatter,
                "completeness checkpoint version {actual} is unsupported"
            ),
            Self::HeaderLengthMismatch { actual } => write!(
                formatter,
                "completeness checkpoint header length {actual} is invalid"
            ),
            Self::TransactionEntryLengthMismatch { actual } => write!(
                formatter,
                "completeness checkpoint transaction-entry length {actual} is invalid"
            ),
            Self::PageEntryLengthMismatch { actual } => write!(
                formatter,
                "completeness checkpoint page-entry length {actual} is invalid"
            ),
            Self::FooterLengthMismatch { actual } => write!(
                formatter,
                "completeness checkpoint footer length {actual} is invalid"
            ),
            Self::TransactionCountOutOfRange { transaction_count } => write!(
                formatter,
                "completeness checkpoint transaction count {transaction_count} is not representable on this host"
            ),
            Self::PageCountOutOfRange { page_count } => write!(
                formatter,
                "completeness checkpoint page count {page_count} is not representable on this host"
            ),
            Self::EncodedLengthOverflow {
                transaction_count,
                page_count,
            } => write!(
                formatter,
                "completeness checkpoint encoded length overflowed for {transaction_count} transaction entries and {page_count} page entries"
            ),
            Self::TotalLengthOutOfRange { total_length } => write!(
                formatter,
                "completeness checkpoint total length {total_length} is not representable on this host"
            ),
            Self::DeclaredLengthMismatch { declared, expected } => write!(
                formatter,
                "completeness checkpoint declared length {declared} does not match expected length {expected}"
            ),
            Self::TrailingBytes {
                expected_length,
                actual_length,
            } => write!(
                formatter,
                "completeness checkpoint has trailing bytes: expected {expected_length} bytes, found {actual_length}"
            ),
            Self::FooterMagicMismatch { actual } => write!(
                formatter,
                "completeness checkpoint footer magic is invalid: {actual:?}"
            ),
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "completeness checkpoint checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
            Self::FrontierPresenceInvalid { actual } => write!(
                formatter,
                "completeness checkpoint frontier presence value {actual} is invalid"
            ),
            Self::AbsentFrontierValueNonZero { actual } => write!(
                formatter,
                "completeness checkpoint absent frontier has nonzero payload {actual}"
            ),
            Self::ReservedByteNonZero { offset, actual } => write!(
                formatter,
                "completeness checkpoint reserved byte at offset {offset} is nonzero: {actual}"
            ),
            Self::EntryStateInvalid {
                transaction_index,
                actual,
            } => write!(
                formatter,
                "completeness checkpoint transaction entry {transaction_index} state value {actual} is invalid"
            ),
            Self::EntryPositionPresenceInvalid {
                transaction_index,
                field,
                actual,
            } => write!(
                formatter,
                "completeness checkpoint transaction entry {transaction_index} {field} presence value {actual} is invalid"
            ),
            Self::AbsentEntryPositionValueNonZero {
                transaction_index,
                field,
                actual,
            } => write!(
                formatter,
                "completeness checkpoint transaction entry {transaction_index} absent {field} has nonzero payload {actual}"
            ),
            Self::UncommittedPositionValueNonZero {
                transaction_index,
                actual,
            } => write!(
                formatter,
                "completeness checkpoint transaction entry {transaction_index} is uncommitted with nonzero commit position {actual}"
            ),
            Self::TransactionCapacityExhausted { transaction_count } => write!(
                formatter,
                "completeness checkpoint decode capacity is exhausted for {transaction_count} transaction entries"
            ),
            Self::PageStateInvalid { page_index, actual } => write!(
                formatter,
                "completeness checkpoint page entry {page_index} state value {actual} is invalid"
            ),
            Self::RequiredImagePresenceInvalid { page_index, actual } => write!(
                formatter,
                "completeness checkpoint page entry {page_index} required-image presence value {actual} is invalid"
            ),
            Self::RequiredImageKindInvalid { page_index, actual } => write!(
                formatter,
                "completeness checkpoint page entry {page_index} required-image kind value {actual} is invalid"
            ),
            Self::RequiredImageKindNonZeroWhenAbsent { page_index, actual } => write!(
                formatter,
                "completeness checkpoint page entry {page_index} absent required image has nonzero kind {actual}"
            ),
            Self::AbsentRequiredImagePayloadNonZero {
                page_index,
                field,
                actual,
            } => write!(
                formatter,
                "completeness checkpoint page entry {page_index} absent required image has nonzero {field} {actual}"
            ),
            Self::RawRequiredImagePayloadNonZero {
                page_index,
                field,
                actual,
            } => write!(
                formatter,
                "completeness checkpoint page entry {page_index} raw required image has nonzero {field} {actual}"
            ),
            Self::StoredPositionPresenceInvalid { page_index, actual } => write!(
                formatter,
                "completeness checkpoint page entry {page_index} stored-position presence value {actual} is invalid"
            ),
            Self::AbsentStoredPositionValueNonZero { page_index, actual } => write!(
                formatter,
                "completeness checkpoint page entry {page_index} absent stored position has nonzero payload {actual}"
            ),
            Self::PageCapacityExhausted { page_count } => write!(
                formatter,
                "completeness checkpoint decode capacity is exhausted for {page_count} page entries"
            ),
            Self::ReplayKindInvalid { actual } => write!(
                formatter,
                "completeness checkpoint replay kind value {actual} is invalid"
            ),
            Self::ReplayFrontierPresenceInvalid { actual } => write!(
                formatter,
                "completeness checkpoint replay frontier presence value {actual} is invalid"
            ),
            Self::AbsentReplayFrontierValueNonZero { actual } => write!(
                formatter,
                "completeness checkpoint absent replay frontier has nonzero payload {actual}"
            ),
            Self::ReplayPositionPresenceInvalid { actual } => write!(
                formatter,
                "completeness checkpoint replay position presence value {actual} is invalid"
            ),
            Self::AbsentReplayPositionValueNonZero { actual } => write!(
                formatter,
                "completeness checkpoint absent replay position has nonzero payload {actual}"
            ),
            Self::ReplayCauseDiscriminantInvalid { actual } => write!(
                formatter,
                "completeness checkpoint replay cause value {actual} is invalid"
            ),
            Self::ReplayCauseFieldNonZero { field, actual } => write!(
                formatter,
                "completeness checkpoint replay cause has unused nonzero {field} {actual}"
            ),
        }
    }
}

impl Error for RestartCheckpointCompletenessBaselineDecodeError {}

#[derive(Clone, Copy)]
struct EncodedTransactionEntry {
    epoch: u64,
    sequence: u64,
    first_owned_page_position: Option<u64>,
    last_owned_page_position: Option<u64>,
    owned_page_record_count: u64,
    state: DurableTransactionRestartCheckpointBaselineStateObservation,
}

impl EncodedTransactionEntry {
    fn from_baseline(entry: &DurableTransactionRestartCheckpointBaselineEntry) -> Self {
        let transaction = entry.transaction();
        let state = match entry.state() {
            DurableTransactionRestartCheckpointBaselineState::Uncommitted => {
                DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted
            }
            DurableTransactionRestartCheckpointBaselineState::Committed { commit_position } => {
                DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                    commit_position,
                }
            }
        };
        Self {
            epoch: transaction.epoch(),
            sequence: transaction.sequence(),
            first_owned_page_position: entry.first_owned_page_position(),
            last_owned_page_position: entry.last_owned_page_position(),
            owned_page_record_count: entry.owned_page_record_count(),
            state,
        }
    }
}

#[derive(Clone, Copy)]
enum EncodedRequiredImage {
    Raw {
        page_position: u64,
    },
    CommittedTransaction {
        epoch: u64,
        sequence: u64,
        page_position: u64,
        commit_position: u64,
    },
}

impl EncodedRequiredImage {
    fn from_required_image(required: &DurableTransactionRestartRequiredPageImage) -> Self {
        match required {
            DurableTransactionRestartRequiredPageImage::Raw { page_position } => Self::Raw {
                page_position: *page_position,
            },
            DurableTransactionRestartRequiredPageImage::CommittedTransaction {
                transaction,
                page_position,
                commit_position,
            } => Self::CommittedTransaction {
                epoch: transaction.epoch(),
                sequence: transaction.sequence(),
                page_position: *page_position,
                commit_position: *commit_position,
            },
        }
    }
}

#[derive(Clone, Copy)]
struct EncodedPageEntry {
    page_number: u64,
    state: u8,
    required_image: Option<EncodedRequiredImage>,
    stored_position: Option<u64>,
}

impl EncodedPageEntry {
    fn from_baseline(entry: &DurableTransactionRestartPageEntry) -> Self {
        let (state, required_image, stored_position) = match entry.state() {
            DurableTransactionRestartPageState::NoRequiredImage => {
                (PAGE_STATE_NO_REQUIRED_IMAGE, None, None)
            }
            DurableTransactionRestartPageState::StoreMissing { required } => (
                PAGE_STATE_STORE_MISSING,
                Some(EncodedRequiredImage::from_required_image(required)),
                None,
            ),
            DurableTransactionRestartPageState::StoreCurrent {
                required,
                stored_position,
            } => (
                PAGE_STATE_STORE_CURRENT,
                Some(EncodedRequiredImage::from_required_image(required)),
                Some(*stored_position),
            ),
            DurableTransactionRestartPageState::StoreBehind {
                stored_position,
                required,
            } => (
                PAGE_STATE_STORE_BEHIND,
                Some(EncodedRequiredImage::from_required_image(required)),
                Some(*stored_position),
            ),
        };
        Self {
            page_number: entry.page_number().get(),
            state,
            required_image,
            stored_position,
        }
    }
}

#[derive(Clone, Copy)]
enum EncodedReplayCause {
    StoreMissingPage { page_number: u64 },
    StoreBehindPage { page_number: u64 },
    UncommittedTransaction { epoch: u64, sequence: u64 },
}

#[derive(Clone, Copy)]
struct EncodedReplay {
    kind: u8,
    frontier: Option<u64>,
    position: Option<u64>,
    cause: Option<EncodedReplayCause>,
}

impl EncodedReplay {
    fn from_replay_start(replay_start: &DurableTransactionRestartReplayStart) -> Self {
        match replay_start {
            DurableTransactionRestartReplayStart::AfterFrontier { frontier } => Self {
                kind: REPLAY_KIND_AFTER_FRONTIER,
                frontier: *frontier,
                position: None,
                cause: None,
            },
            DurableTransactionRestartReplayStart::AtPosition { position, cause } => Self {
                kind: REPLAY_KIND_AT_POSITION,
                frontier: None,
                position: Some(*position),
                cause: Some(match cause {
                    DurableTransactionRestartReplayStartCause::StoreMissing { page_number } => {
                        EncodedReplayCause::StoreMissingPage {
                            page_number: page_number.get(),
                        }
                    }
                    DurableTransactionRestartReplayStartCause::StoreBehind { page_number } => {
                        EncodedReplayCause::StoreBehindPage {
                            page_number: page_number.get(),
                        }
                    }
                    DurableTransactionRestartReplayStartCause::UncommittedTransaction {
                        transaction,
                    } => EncodedReplayCause::UncommittedTransaction {
                        epoch: transaction.epoch(),
                        sequence: transaction.sequence(),
                    },
                }),
            },
        }
    }
}

/// Encodes one authoritative restart checkpoint completeness baseline.
///
/// The returned bytes contain inert transaction, page, and replay metadata
/// only. They are not a filesystem publication, startup selection, replay
/// plan, or retention proof, and they use an independent version-1 namespace
/// that does not extend or reinterpret ADR 0044's `NTSQCKP1` bytes.
///
/// An untrusted decoded observation cannot substitute for the authoritative
/// encoder input:
///
/// ```compile_fail
/// use ntsql_storage_file::encode_restart_checkpoint_completeness_baseline;
/// use ntsql_transaction::OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation;
///
/// fn cannot_encode_untrusted(
///     observation: &OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
/// ) {
///     let _ = encode_restart_checkpoint_completeness_baseline(observation);
/// }
/// ```
pub fn encode_restart_checkpoint_completeness_baseline(
    baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
) -> Result<Vec<u8>, RestartCheckpointCompletenessBaselineEncodeError> {
    let transaction_baseline = baseline.transaction_baseline();
    encode_fields(
        transaction_baseline.persistent_log_id().get(),
        transaction_baseline.durable_frontier(),
        transaction_baseline
            .transactions()
            .iter()
            .map(EncodedTransactionEntry::from_baseline),
        baseline.pages().iter().map(EncodedPageEntry::from_baseline),
        EncodedReplay::from_replay_start(baseline.replay_start()),
    )
}

fn encode_fields<Transactions, Pages>(
    persistent_log_id: u128,
    durable_frontier: Option<u64>,
    transactions: Transactions,
    pages: Pages,
    replay: EncodedReplay,
) -> Result<Vec<u8>, RestartCheckpointCompletenessBaselineEncodeError>
where
    Transactions: ExactSizeIterator<Item = EncodedTransactionEntry>,
    Pages: ExactSizeIterator<Item = EncodedPageEntry>,
{
    let transaction_count = transactions.len();
    let page_count = pages.len();
    let transaction_count_u64 = u64::try_from(transaction_count).map_err(|_| {
        RestartCheckpointCompletenessBaselineEncodeError::TransactionCountOutOfRange {
            transaction_count,
        }
    })?;
    let page_count_u64 = u64::try_from(page_count).map_err(|_| {
        RestartCheckpointCompletenessBaselineEncodeError::PageCountOutOfRange { page_count }
    })?;
    let overflow_error =
        || RestartCheckpointCompletenessBaselineEncodeError::EncodedLengthOverflow {
            transaction_count,
            page_count,
        };
    let transaction_entries_length = transaction_count
        .checked_mul(TRANSACTION_ENTRY_LENGTH)
        .ok_or_else(overflow_error)?;
    let page_entries_length = page_count
        .checked_mul(PAGE_ENTRY_LENGTH)
        .ok_or_else(overflow_error)?;
    let encoded_length = HEADER_LENGTH
        .checked_add(transaction_entries_length)
        .and_then(|length| length.checked_add(page_entries_length))
        .and_then(|length| length.checked_add(FOOTER_LENGTH))
        .ok_or_else(overflow_error)?;
    let encoded_length_u64 = u64::try_from(encoded_length).map_err(|_| {
        RestartCheckpointCompletenessBaselineEncodeError::EncodedLengthOutOfRange { encoded_length }
    })?;
    let mut encoded = Vec::new();
    encoded.try_reserve_exact(encoded_length).map_err(|_| {
        RestartCheckpointCompletenessBaselineEncodeError::CapacityExhausted { encoded_length }
    })?;

    let mut header = [0_u8; HEADER_LENGTH];
    header[..8].copy_from_slice(&HEADER_MAGIC);
    super::write_u16(&mut header, 8, FORMAT_VERSION);
    super::write_u16(&mut header, 10, HEADER_LENGTH_U16);
    super::write_u16(&mut header, 12, TRANSACTION_ENTRY_LENGTH_U16);
    super::write_u16(&mut header, 14, PAGE_ENTRY_LENGTH_U16);
    super::write_u16(&mut header, 16, FOOTER_LENGTH_U16);
    super::write_u128(&mut header, PERSISTENT_LOG_ID_OFFSET, persistent_log_id);
    match durable_frontier {
        Some(frontier) => {
            super::write_u64(&mut header, DURABLE_FRONTIER_OFFSET, frontier);
            header[DURABLE_FRONTIER_PRESENCE_OFFSET] = PRESENT;
        }
        None => {
            header[DURABLE_FRONTIER_PRESENCE_OFFSET] = ABSENT;
        }
    }
    super::write_u64(&mut header, TRANSACTION_COUNT_OFFSET, transaction_count_u64);
    super::write_u64(&mut header, PAGE_COUNT_OFFSET, page_count_u64);
    super::write_u64(&mut header, TOTAL_LENGTH_OFFSET, encoded_length_u64);
    header[REPLAY_KIND_OFFSET] = replay.kind;
    match replay.frontier {
        Some(frontier) => {
            super::write_u64(&mut header, REPLAY_FRONTIER_OFFSET, frontier);
            header[REPLAY_FRONTIER_PRESENCE_OFFSET] = PRESENT;
        }
        None => {
            header[REPLAY_FRONTIER_PRESENCE_OFFSET] = ABSENT;
        }
    }
    match replay.position {
        Some(position) => {
            super::write_u64(&mut header, REPLAY_POSITION_OFFSET, position);
            header[REPLAY_POSITION_PRESENCE_OFFSET] = PRESENT;
        }
        None => {
            header[REPLAY_POSITION_PRESENCE_OFFSET] = ABSENT;
        }
    }
    match replay.cause {
        Some(EncodedReplayCause::StoreMissingPage { page_number }) => {
            header[REPLAY_CAUSE_DISCRIMINANT_OFFSET] = REPLAY_CAUSE_STORE_MISSING_PAGE;
            super::write_u64(&mut header, REPLAY_CAUSE_PAGE_NUMBER_OFFSET, page_number);
        }
        Some(EncodedReplayCause::StoreBehindPage { page_number }) => {
            header[REPLAY_CAUSE_DISCRIMINANT_OFFSET] = REPLAY_CAUSE_STORE_BEHIND_PAGE;
            super::write_u64(&mut header, REPLAY_CAUSE_PAGE_NUMBER_OFFSET, page_number);
        }
        Some(EncodedReplayCause::UncommittedTransaction { epoch, sequence }) => {
            header[REPLAY_CAUSE_DISCRIMINANT_OFFSET] = REPLAY_CAUSE_UNCOMMITTED_TRANSACTION;
            super::write_u64(&mut header, REPLAY_CAUSE_EPOCH_OFFSET, epoch);
            super::write_u64(&mut header, REPLAY_CAUSE_SEQUENCE_OFFSET, sequence);
        }
        None => {
            header[REPLAY_CAUSE_DISCRIMINANT_OFFSET] = REPLAY_CAUSE_ABSENT;
        }
    }
    encoded.extend_from_slice(&header);

    for entry in transactions {
        let mut bytes = [0_u8; TRANSACTION_ENTRY_LENGTH];
        super::write_u64(&mut bytes, TRANSACTION_ENTRY_EPOCH_OFFSET, entry.epoch);
        super::write_u64(
            &mut bytes,
            TRANSACTION_ENTRY_SEQUENCE_OFFSET,
            entry.sequence,
        );
        write_optional_u64(
            &mut bytes,
            TRANSACTION_ENTRY_FIRST_OWNED_PAGE_POSITION_OFFSET,
            TRANSACTION_ENTRY_FIRST_POSITION_PRESENCE_OFFSET,
            entry.first_owned_page_position,
        );
        write_optional_u64(
            &mut bytes,
            TRANSACTION_ENTRY_LAST_OWNED_PAGE_POSITION_OFFSET,
            TRANSACTION_ENTRY_LAST_POSITION_PRESENCE_OFFSET,
            entry.last_owned_page_position,
        );
        super::write_u64(
            &mut bytes,
            TRANSACTION_ENTRY_OWNED_PAGE_RECORD_COUNT_OFFSET,
            entry.owned_page_record_count,
        );
        match entry.state {
            DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted => {
                bytes[TRANSACTION_ENTRY_STATE_OFFSET] = STATE_UNCOMMITTED;
            }
            DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                commit_position,
            } => {
                super::write_u64(
                    &mut bytes,
                    TRANSACTION_ENTRY_COMMIT_POSITION_OFFSET,
                    commit_position,
                );
                bytes[TRANSACTION_ENTRY_STATE_OFFSET] = STATE_COMMITTED;
            }
        }
        encoded.extend_from_slice(&bytes);
    }

    for entry in pages {
        let mut bytes = [0_u8; PAGE_ENTRY_LENGTH];
        super::write_u64(&mut bytes, PAGE_ENTRY_NUMBER_OFFSET, entry.page_number);
        bytes[PAGE_ENTRY_STATE_OFFSET] = entry.state;
        match entry.required_image {
            Some(EncodedRequiredImage::Raw { page_position }) => {
                bytes[PAGE_ENTRY_REQUIRED_IMAGE_PRESENCE_OFFSET] = PRESENT;
                bytes[PAGE_ENTRY_REQUIRED_IMAGE_KIND_OFFSET] = REQUIRED_IMAGE_KIND_RAW;
                super::write_u64(
                    &mut bytes,
                    PAGE_ENTRY_REQUIRED_IMAGE_PAGE_POSITION_OFFSET,
                    page_position,
                );
            }
            Some(EncodedRequiredImage::CommittedTransaction {
                epoch,
                sequence,
                page_position,
                commit_position,
            }) => {
                bytes[PAGE_ENTRY_REQUIRED_IMAGE_PRESENCE_OFFSET] = PRESENT;
                bytes[PAGE_ENTRY_REQUIRED_IMAGE_KIND_OFFSET] =
                    REQUIRED_IMAGE_KIND_COMMITTED_TRANSACTION;
                super::write_u64(
                    &mut bytes,
                    PAGE_ENTRY_REQUIRED_IMAGE_PAGE_POSITION_OFFSET,
                    page_position,
                );
                super::write_u64(&mut bytes, PAGE_ENTRY_REQUIRED_IMAGE_EPOCH_OFFSET, epoch);
                super::write_u64(
                    &mut bytes,
                    PAGE_ENTRY_REQUIRED_IMAGE_SEQUENCE_OFFSET,
                    sequence,
                );
                super::write_u64(
                    &mut bytes,
                    PAGE_ENTRY_REQUIRED_IMAGE_COMMIT_POSITION_OFFSET,
                    commit_position,
                );
            }
            None => {
                bytes[PAGE_ENTRY_REQUIRED_IMAGE_PRESENCE_OFFSET] = ABSENT;
            }
        }
        write_optional_u64(
            &mut bytes,
            PAGE_ENTRY_STORED_POSITION_OFFSET,
            PAGE_ENTRY_STORED_POSITION_PRESENCE_OFFSET,
            entry.stored_position,
        );
        encoded.extend_from_slice(&bytes);
    }

    encoded.extend_from_slice(&FOOTER_MAGIC);
    let checksum = super::checksum_v1(&encoded);
    encoded.extend_from_slice(&checksum.to_be_bytes());
    Ok(encoded)
}

fn write_optional_u64(
    bytes: &mut [u8],
    value_offset: usize,
    presence_offset: usize,
    value: Option<u64>,
) {
    match value {
        Some(value) => {
            super::write_u64(bytes, value_offset, value);
            bytes[presence_offset] = PRESENT;
        }
        None => {
            bytes[presence_offset] = ABSENT;
        }
    }
}

/// Decodes one structurally valid versioned completeness checkpoint blob.
///
/// Zero and contradictory domain fields are preserved for a later
/// source-relative validator. Successful decoding does not create an
/// authoritative completeness baseline:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointCompletenessBaselineDecodeError,
///     decode_restart_checkpoint_completeness_baseline,
/// };
/// use ntsql_transaction::DurableTransactionRestartCheckpointCompletenessBaseline;
///
/// fn cannot_authorize(
///     bytes: &[u8],
/// ) -> Result<
///     DurableTransactionRestartCheckpointCompletenessBaseline,
///     RestartCheckpointCompletenessBaselineDecodeError,
/// > {
///     decode_restart_checkpoint_completeness_baseline(bytes).map(Into::into)
/// }
/// ```
///
/// It also cannot create transaction lifecycle state:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointCompletenessBaselineDecodeError,
///     decode_restart_checkpoint_completeness_baseline,
/// };
/// use ntsql_transaction::ActiveTransaction;
///
/// fn cannot_activate(
///     bytes: &[u8],
/// ) -> Result<ActiveTransaction, RestartCheckpointCompletenessBaselineDecodeError> {
///     decode_restart_checkpoint_completeness_baseline(bytes).map(Into::into)
/// }
/// ```
///
/// Nor can it create page-write authority:
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_storage_file::{
///     RestartCheckpointCompletenessBaselineDecodeError,
///     decode_restart_checkpoint_completeness_baseline,
/// };
///
/// fn cannot_write_page<'attempt>(
///     bytes: &[u8],
/// ) -> Result<PageWritePermit<'attempt>, RestartCheckpointCompletenessBaselineDecodeError> {
///     decode_restart_checkpoint_completeness_baseline(bytes).map(Into::into)
/// }
/// ```
///
/// Decoded fields cannot satisfy WAL durability:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointCompletenessBaselineDecodeError,
///     decode_restart_checkpoint_completeness_baseline,
/// };
/// use ntsql_wal::LogDurability;
///
/// fn require_log<Log: LogDurability>(_log: &mut Log) {}
///
/// fn cannot_flush(bytes: &[u8]) -> Result<(), RestartCheckpointCompletenessBaselineDecodeError> {
///     let mut decoded = decode_restart_checkpoint_completeness_baseline(bytes)?;
///     require_log(&mut decoded);
///     Ok(())
/// }
/// ```
///
/// They cannot satisfy committed-page recovery storage:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointCompletenessBaselineDecodeError,
///     decode_restart_checkpoint_completeness_baseline,
/// };
/// use ntsql_transaction::CommittedTransactionPageRecoveryStore;
///
/// fn require_store<Store: CommittedTransactionPageRecoveryStore<1>>(_store: &mut Store) {}
///
/// fn cannot_recover(bytes: &[u8]) -> Result<(), RestartCheckpointCompletenessBaselineDecodeError> {
///     let mut decoded = decode_restart_checkpoint_completeness_baseline(bytes)?;
///     require_store(&mut decoded);
///     Ok(())
/// }
/// ```
///
/// They cannot become restart-analyzed storage ownership:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointCompletenessBaselineDecodeError,
///     decode_restart_checkpoint_completeness_baseline,
/// };
/// use ntsql_transaction::RestartAnalyzedTransactionPageStorage;
///
/// fn cannot_release<Source, Store>(
///     bytes: &[u8],
/// ) -> Result<
///     RestartAnalyzedTransactionPageStorage<Source, Store, 1>,
///     RestartCheckpointCompletenessBaselineDecodeError,
/// > {
///     decode_restart_checkpoint_completeness_baseline(bytes).map(Into::into)
/// }
/// ```
pub fn decode_restart_checkpoint_completeness_baseline(
    bytes: &[u8],
) -> Result<
    OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
    RestartCheckpointCompletenessBaselineDecodeError,
> {
    if bytes.len() < HEADER_LENGTH {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::Truncated {
                expected_length: HEADER_LENGTH,
                actual_length: bytes.len(),
            },
        );
    }

    let header_magic = copy_magic(bytes, 0);
    if header_magic != HEADER_MAGIC {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::HeaderMagicMismatch {
                actual: header_magic,
            },
        );
    }
    let version = super::read_u16(bytes, 8);
    if version != FORMAT_VERSION {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::UnsupportedVersion {
                actual: version,
            },
        );
    }
    let header_length = super::read_u16(bytes, 10);
    if header_length != HEADER_LENGTH_U16 {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::HeaderLengthMismatch {
                actual: header_length,
            },
        );
    }
    let transaction_entry_length = super::read_u16(bytes, 12);
    if transaction_entry_length != TRANSACTION_ENTRY_LENGTH_U16 {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::TransactionEntryLengthMismatch {
                actual: transaction_entry_length,
            },
        );
    }
    let page_entry_length = super::read_u16(bytes, 14);
    if page_entry_length != PAGE_ENTRY_LENGTH_U16 {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::PageEntryLengthMismatch {
                actual: page_entry_length,
            },
        );
    }
    let footer_length = super::read_u16(bytes, 16);
    if footer_length != FOOTER_LENGTH_U16 {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::FooterLengthMismatch {
                actual: footer_length,
            },
        );
    }

    let transaction_count_u64 = super::read_u64(bytes, TRANSACTION_COUNT_OFFSET);
    let transaction_count = usize::try_from(transaction_count_u64).map_err(|_| {
        RestartCheckpointCompletenessBaselineDecodeError::TransactionCountOutOfRange {
            transaction_count: transaction_count_u64,
        }
    })?;
    let page_count_u64 = super::read_u64(bytes, PAGE_COUNT_OFFSET);
    let page_count = usize::try_from(page_count_u64).map_err(|_| {
        RestartCheckpointCompletenessBaselineDecodeError::PageCountOutOfRange {
            page_count: page_count_u64,
        }
    })?;
    let overflow_error =
        || RestartCheckpointCompletenessBaselineDecodeError::EncodedLengthOverflow {
            transaction_count: transaction_count_u64,
            page_count: page_count_u64,
        };
    let expected_length_u64 = transaction_count_u64
        .checked_mul(TRANSACTION_ENTRY_LENGTH_U64)
        .and_then(|length| {
            page_count_u64
                .checked_mul(PAGE_ENTRY_LENGTH_U64)
                .and_then(|page_length| length.checked_add(page_length))
        })
        .and_then(|length| length.checked_add(HEADER_LENGTH_U64))
        .and_then(|length| length.checked_add(FOOTER_LENGTH_U64))
        .ok_or_else(overflow_error)?;
    let declared_length_u64 = super::read_u64(bytes, TOTAL_LENGTH_OFFSET);
    if declared_length_u64 != expected_length_u64 {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::DeclaredLengthMismatch {
                declared: declared_length_u64,
                expected: expected_length_u64,
            },
        );
    }
    let expected_length = usize::try_from(expected_length_u64).map_err(|_| {
        RestartCheckpointCompletenessBaselineDecodeError::TotalLengthOutOfRange {
            total_length: expected_length_u64,
        }
    })?;
    if bytes.len() < expected_length {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::Truncated {
                expected_length,
                actual_length: bytes.len(),
            },
        );
    }
    if bytes.len() > expected_length {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::TrailingBytes {
                expected_length,
                actual_length: bytes.len(),
            },
        );
    }

    let footer_offset = expected_length - FOOTER_LENGTH;
    let footer_magic = copy_magic(bytes, footer_offset);
    if footer_magic != FOOTER_MAGIC {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::FooterMagicMismatch {
                actual: footer_magic,
            },
        );
    }
    let checksum_offset = expected_length - 8;
    let actual_checksum = super::read_u64(bytes, checksum_offset);
    let expected_checksum = super::checksum_v1(&bytes[..checksum_offset]);
    if actual_checksum != expected_checksum {
        return Err(
            RestartCheckpointCompletenessBaselineDecodeError::ChecksumMismatch {
                expected: expected_checksum,
                actual: actual_checksum,
            },
        );
    }

    for (relative_offset, actual) in bytes[HEADER_RESERVED_A_START..HEADER_RESERVED_A_END]
        .iter()
        .copied()
        .enumerate()
    {
        if actual != 0 {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                    offset: HEADER_RESERVED_A_START + relative_offset,
                    actual,
                },
            );
        }
    }
    for (relative_offset, actual) in bytes[HEADER_RESERVED_B_START..HEADER_RESERVED_B_END]
        .iter()
        .copied()
        .enumerate()
    {
        if actual != 0 {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                    offset: HEADER_RESERVED_B_START + relative_offset,
                    actual,
                },
            );
        }
    }
    for (relative_offset, actual) in bytes[HEADER_RESERVED_C_START..HEADER_RESERVED_C_END]
        .iter()
        .copied()
        .enumerate()
    {
        if actual != 0 {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                    offset: HEADER_RESERVED_C_START + relative_offset,
                    actual,
                },
            );
        }
    }

    let frontier_value = super::read_u64(bytes, DURABLE_FRONTIER_OFFSET);
    let durable_frontier =
        decode_header_frontier(bytes[DURABLE_FRONTIER_PRESENCE_OFFSET], frontier_value)?;
    let replay = decode_replay(bytes)?;

    let mut transactions = Vec::new();
    transactions
        .try_reserve_exact(transaction_count)
        .map_err(|_| {
            RestartCheckpointCompletenessBaselineDecodeError::TransactionCapacityExhausted {
                transaction_count,
            }
        })?;
    for transaction_index in 0..transaction_count {
        let entry_offset = HEADER_LENGTH + transaction_index * TRANSACTION_ENTRY_LENGTH;
        transactions.push(decode_transaction_entry(
            bytes,
            entry_offset,
            transaction_index,
        )?);
    }

    let pages_start = HEADER_LENGTH + transaction_count * TRANSACTION_ENTRY_LENGTH;
    let mut pages = Vec::new();
    pages.try_reserve_exact(page_count).map_err(|_| {
        RestartCheckpointCompletenessBaselineDecodeError::PageCapacityExhausted { page_count }
    })?;
    for page_index in 0..page_count {
        let entry_offset = pages_start + page_index * PAGE_ENTRY_LENGTH;
        pages.push(decode_page_entry(bytes, entry_offset, page_index)?);
    }

    Ok(
        OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation::new(
            OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
                super::read_u128(bytes, PERSISTENT_LOG_ID_OFFSET),
                durable_frontier,
                transactions,
            ),
            pages,
            replay,
        ),
    )
}

fn decode_transaction_entry(
    bytes: &[u8],
    entry_offset: usize,
    transaction_index: usize,
) -> Result<
    DurableTransactionRestartCheckpointBaselineEntryObservation,
    RestartCheckpointCompletenessBaselineDecodeError,
> {
    let entry = &bytes[entry_offset..entry_offset + TRANSACTION_ENTRY_LENGTH];
    for (relative_offset, actual) in entry
        [TRANSACTION_ENTRY_RESERVED_START..TRANSACTION_ENTRY_RESERVED_END]
        .iter()
        .copied()
        .enumerate()
    {
        if actual != 0 {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                    offset: entry_offset + TRANSACTION_ENTRY_RESERVED_START + relative_offset,
                    actual,
                },
            );
        }
    }

    let first_owned_page_position = decode_entry_optional_position(
        transaction_index,
        RestartCheckpointCompletenessBaselineEntryOptionalField::FirstOwnedPagePosition,
        entry[TRANSACTION_ENTRY_FIRST_POSITION_PRESENCE_OFFSET],
        super::read_u64(entry, TRANSACTION_ENTRY_FIRST_OWNED_PAGE_POSITION_OFFSET),
    )?;
    let last_owned_page_position = decode_entry_optional_position(
        transaction_index,
        RestartCheckpointCompletenessBaselineEntryOptionalField::LastOwnedPagePosition,
        entry[TRANSACTION_ENTRY_LAST_POSITION_PRESENCE_OFFSET],
        super::read_u64(entry, TRANSACTION_ENTRY_LAST_OWNED_PAGE_POSITION_OFFSET),
    )?;
    let commit_position = super::read_u64(entry, TRANSACTION_ENTRY_COMMIT_POSITION_OFFSET);
    let state = match entry[TRANSACTION_ENTRY_STATE_OFFSET] {
        STATE_UNCOMMITTED => {
            if commit_position != 0 {
                return Err(
                    RestartCheckpointCompletenessBaselineDecodeError::UncommittedPositionValueNonZero {
                        transaction_index,
                        actual: commit_position,
                    },
                );
            }
            DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted
        }
        STATE_COMMITTED => DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
            commit_position,
        },
        actual => {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::EntryStateInvalid {
                    transaction_index,
                    actual,
                },
            );
        }
    };
    Ok(
        DurableTransactionRestartCheckpointBaselineEntryObservation::new(
            super::read_u64(entry, TRANSACTION_ENTRY_EPOCH_OFFSET),
            super::read_u64(entry, TRANSACTION_ENTRY_SEQUENCE_OFFSET),
            first_owned_page_position,
            last_owned_page_position,
            super::read_u64(entry, TRANSACTION_ENTRY_OWNED_PAGE_RECORD_COUNT_OFFSET),
            state,
        ),
    )
}

fn decode_page_entry(
    bytes: &[u8],
    entry_offset: usize,
    page_index: usize,
) -> Result<
    DurableTransactionRestartCheckpointCompletenessBaselinePageObservation,
    RestartCheckpointCompletenessBaselineDecodeError,
> {
    let entry = &bytes[entry_offset..entry_offset + PAGE_ENTRY_LENGTH];
    for (relative_offset, actual) in entry[PAGE_ENTRY_RESERVED_A_START..PAGE_ENTRY_RESERVED_A_END]
        .iter()
        .copied()
        .enumerate()
    {
        if actual != 0 {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                    offset: entry_offset + PAGE_ENTRY_RESERVED_A_START + relative_offset,
                    actual,
                },
            );
        }
    }
    for (relative_offset, actual) in entry[PAGE_ENTRY_RESERVED_B_START..PAGE_ENTRY_RESERVED_B_END]
        .iter()
        .copied()
        .enumerate()
    {
        if actual != 0 {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                    offset: entry_offset + PAGE_ENTRY_RESERVED_B_START + relative_offset,
                    actual,
                },
            );
        }
    }

    let state = match entry[PAGE_ENTRY_STATE_OFFSET] {
        PAGE_STATE_NO_REQUIRED_IMAGE => {
            DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::NoRequiredImage
        }
        PAGE_STATE_STORE_MISSING => {
            DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreMissing
        }
        PAGE_STATE_STORE_CURRENT => {
            DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent
        }
        PAGE_STATE_STORE_BEHIND => {
            DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreBehind
        }
        actual => {
            return Err(RestartCheckpointCompletenessBaselineDecodeError::PageStateInvalid {
                page_index,
                actual,
            });
        }
    };

    let required_image = decode_required_image(entry, page_index)?;
    let stored_position = decode_page_optional_position(
        page_index,
        entry[PAGE_ENTRY_STORED_POSITION_PRESENCE_OFFSET],
        super::read_u64(entry, PAGE_ENTRY_STORED_POSITION_OFFSET),
    )?;

    Ok(
        DurableTransactionRestartCheckpointCompletenessBaselinePageObservation::new(
            super::read_u64(entry, PAGE_ENTRY_NUMBER_OFFSET),
            state,
            required_image,
            stored_position,
        ),
    )
}

fn decode_required_image(
    entry: &[u8],
    page_index: usize,
) -> Result<
    Option<DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation>,
    RestartCheckpointCompletenessBaselineDecodeError,
> {
    use RestartCheckpointCompletenessBaselineRequiredImageField as Field;

    let page_position = super::read_u64(entry, PAGE_ENTRY_REQUIRED_IMAGE_PAGE_POSITION_OFFSET);
    let epoch = super::read_u64(entry, PAGE_ENTRY_REQUIRED_IMAGE_EPOCH_OFFSET);
    let sequence = super::read_u64(entry, PAGE_ENTRY_REQUIRED_IMAGE_SEQUENCE_OFFSET);
    let commit_position = super::read_u64(entry, PAGE_ENTRY_REQUIRED_IMAGE_COMMIT_POSITION_OFFSET);
    let kind = entry[PAGE_ENTRY_REQUIRED_IMAGE_KIND_OFFSET];

    match entry[PAGE_ENTRY_REQUIRED_IMAGE_PRESENCE_OFFSET] {
        ABSENT => {
            if kind != 0 {
                return Err(
                    RestartCheckpointCompletenessBaselineDecodeError::RequiredImageKindNonZeroWhenAbsent {
                        page_index,
                        actual: kind,
                    },
                );
            }
            for (field, actual) in [
                (Field::PagePosition, page_position),
                (Field::Epoch, epoch),
                (Field::Sequence, sequence),
                (Field::CommitPosition, commit_position),
            ] {
                if actual != 0 {
                    return Err(
                        RestartCheckpointCompletenessBaselineDecodeError::AbsentRequiredImagePayloadNonZero {
                            page_index,
                            field,
                            actual,
                        },
                    );
                }
            }
            Ok(None)
        }
        PRESENT => match kind {
            REQUIRED_IMAGE_KIND_RAW => {
                for (field, actual) in [
                    (Field::Epoch, epoch),
                    (Field::Sequence, sequence),
                    (Field::CommitPosition, commit_position),
                ] {
                    if actual != 0 {
                        return Err(
                            RestartCheckpointCompletenessBaselineDecodeError::RawRequiredImagePayloadNonZero {
                                page_index,
                                field,
                                actual,
                            },
                        );
                    }
                }
                Ok(Some(
                    DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::Raw {
                        page_position,
                    },
                ))
            }
            REQUIRED_IMAGE_KIND_COMMITTED_TRANSACTION => Ok(Some(
                DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::CommittedTransaction {
                    epoch,
                    sequence,
                    page_position,
                    commit_position,
                },
            )),
            actual => Err(
                RestartCheckpointCompletenessBaselineDecodeError::RequiredImageKindInvalid {
                    page_index,
                    actual,
                },
            ),
        },
        actual => Err(
            RestartCheckpointCompletenessBaselineDecodeError::RequiredImagePresenceInvalid {
                page_index,
                actual,
            },
        ),
    }
}

fn decode_replay(
    bytes: &[u8],
) -> Result<
    DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation,
    RestartCheckpointCompletenessBaselineDecodeError,
> {
    use RestartCheckpointCompletenessBaselineReplayCauseField as Field;

    let kind = match bytes[REPLAY_KIND_OFFSET] {
        REPLAY_KIND_AFTER_FRONTIER => {
            DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AfterFrontier
        }
        REPLAY_KIND_AT_POSITION => {
            DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition
        }
        actual => {
            return Err(RestartCheckpointCompletenessBaselineDecodeError::ReplayKindInvalid {
                actual,
            });
        }
    };

    let frontier_value = super::read_u64(bytes, REPLAY_FRONTIER_OFFSET);
    let frontier = match bytes[REPLAY_FRONTIER_PRESENCE_OFFSET] {
        ABSENT => {
            if frontier_value != 0 {
                return Err(
                    RestartCheckpointCompletenessBaselineDecodeError::AbsentReplayFrontierValueNonZero {
                        actual: frontier_value,
                    },
                );
            }
            None
        }
        PRESENT => Some(frontier_value),
        actual => {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayFrontierPresenceInvalid {
                    actual,
                },
            );
        }
    };

    let position_value = super::read_u64(bytes, REPLAY_POSITION_OFFSET);
    let position = match bytes[REPLAY_POSITION_PRESENCE_OFFSET] {
        ABSENT => {
            if position_value != 0 {
                return Err(
                    RestartCheckpointCompletenessBaselineDecodeError::AbsentReplayPositionValueNonZero {
                        actual: position_value,
                    },
                );
            }
            None
        }
        PRESENT => Some(position_value),
        actual => {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayPositionPresenceInvalid {
                    actual,
                },
            );
        }
    };

    let cause_page_number = super::read_u64(bytes, REPLAY_CAUSE_PAGE_NUMBER_OFFSET);
    let cause_epoch = super::read_u64(bytes, REPLAY_CAUSE_EPOCH_OFFSET);
    let cause_sequence = super::read_u64(bytes, REPLAY_CAUSE_SEQUENCE_OFFSET);
    let cause = match bytes[REPLAY_CAUSE_DISCRIMINANT_OFFSET] {
        REPLAY_CAUSE_ABSENT => {
            for (field, actual) in [
                (Field::PageNumber, cause_page_number),
                (Field::Epoch, cause_epoch),
                (Field::Sequence, cause_sequence),
            ] {
                if actual != 0 {
                    return Err(
                        RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseFieldNonZero {
                            field,
                            actual,
                        },
                    );
                }
            }
            None
        }
        REPLAY_CAUSE_STORE_MISSING_PAGE => {
            for (field, actual) in [
                (Field::Epoch, cause_epoch),
                (Field::Sequence, cause_sequence),
            ] {
                if actual != 0 {
                    return Err(
                        RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseFieldNonZero {
                            field,
                            actual,
                        },
                    );
                }
            }
            Some(
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreMissingPage {
                    page_number: cause_page_number,
                },
            )
        }
        REPLAY_CAUSE_STORE_BEHIND_PAGE => {
            for (field, actual) in [
                (Field::Epoch, cause_epoch),
                (Field::Sequence, cause_sequence),
            ] {
                if actual != 0 {
                    return Err(
                        RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseFieldNonZero {
                            field,
                            actual,
                        },
                    );
                }
            }
            Some(
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreBehindPage {
                    page_number: cause_page_number,
                },
            )
        }
        REPLAY_CAUSE_UNCOMMITTED_TRANSACTION => {
            if cause_page_number != 0 {
                return Err(
                    RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseFieldNonZero {
                        field: Field::PageNumber,
                        actual: cause_page_number,
                    },
                );
            }
            Some(
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::UncommittedTransaction {
                    epoch: cause_epoch,
                    sequence: cause_sequence,
                },
            )
        }
        actual => {
            return Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseDiscriminantInvalid {
                    actual,
                },
            );
        }
    };

    Ok(
        DurableTransactionRestartCheckpointCompletenessBaselineReplayObservation::new(
            kind, frontier, position, cause,
        ),
    )
}

fn decode_header_frontier(
    presence: u8,
    value: u64,
) -> Result<Option<u64>, RestartCheckpointCompletenessBaselineDecodeError> {
    match presence {
        ABSENT => {
            if value != 0 {
                Err(
                    RestartCheckpointCompletenessBaselineDecodeError::AbsentFrontierValueNonZero {
                        actual: value,
                    },
                )
            } else {
                Ok(None)
            }
        }
        PRESENT => Ok(Some(value)),
        actual => Err(
            RestartCheckpointCompletenessBaselineDecodeError::FrontierPresenceInvalid { actual },
        ),
    }
}

fn decode_entry_optional_position(
    transaction_index: usize,
    field: RestartCheckpointCompletenessBaselineEntryOptionalField,
    presence: u8,
    value: u64,
) -> Result<Option<u64>, RestartCheckpointCompletenessBaselineDecodeError> {
    match presence {
        ABSENT => {
            if value != 0 {
                Err(
                    RestartCheckpointCompletenessBaselineDecodeError::AbsentEntryPositionValueNonZero {
                        transaction_index,
                        field,
                        actual: value,
                    },
                )
            } else {
                Ok(None)
            }
        }
        PRESENT => Ok(Some(value)),
        actual => Err(
            RestartCheckpointCompletenessBaselineDecodeError::EntryPositionPresenceInvalid {
                transaction_index,
                field,
                actual,
            },
        ),
    }
}

fn decode_page_optional_position(
    page_index: usize,
    presence: u8,
    value: u64,
) -> Result<Option<u64>, RestartCheckpointCompletenessBaselineDecodeError> {
    match presence {
        ABSENT => {
            if value != 0 {
                Err(
                    RestartCheckpointCompletenessBaselineDecodeError::AbsentStoredPositionValueNonZero {
                        page_index,
                        actual: value,
                    },
                )
            } else {
                Ok(None)
            }
        }
        PRESENT => Ok(Some(value)),
        actual => Err(
            RestartCheckpointCompletenessBaselineDecodeError::StoredPositionPresenceInvalid {
                page_index,
                actual,
            },
        ),
    }
}

fn copy_magic(bytes: &[u8], offset: usize) -> [u8; 8] {
    let mut magic = [0_u8; 8];
    magic.copy_from_slice(&bytes[offset..offset + 8]);
    magic
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    fn sample_blob() -> Result<Vec<u8>, RestartCheckpointCompletenessBaselineEncodeError> {
        encode_fields(
            0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            Some(42),
            [
                EncodedTransactionEntry {
                    epoch: 7,
                    sequence: 3,
                    first_owned_page_position: Some(1),
                    last_owned_page_position: Some(2),
                    owned_page_record_count: 2,
                    state: DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted,
                },
                EncodedTransactionEntry {
                    epoch: 9,
                    sequence: 1,
                    first_owned_page_position: None,
                    last_owned_page_position: None,
                    owned_page_record_count: 0,
                    state: DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                        commit_position: 5,
                    },
                },
            ]
            .into_iter(),
            [
                EncodedPageEntry {
                    page_number: 11,
                    state: PAGE_STATE_STORE_MISSING,
                    required_image: Some(EncodedRequiredImage::Raw { page_position: 4 }),
                    stored_position: None,
                },
                EncodedPageEntry {
                    page_number: 12,
                    state: PAGE_STATE_STORE_CURRENT,
                    required_image: Some(EncodedRequiredImage::CommittedTransaction {
                        epoch: 9,
                        sequence: 1,
                        page_position: 6,
                        commit_position: 5,
                    }),
                    stored_position: Some(6),
                },
            ]
            .into_iter(),
            EncodedReplay {
                kind: REPLAY_KIND_AT_POSITION,
                frontier: None,
                position: Some(4),
                cause: Some(EncodedReplayCause::StoreMissingPage { page_number: 11 }),
            },
        )
    }

    const TX0_OFFSET: usize = HEADER_LENGTH;
    const TX1_OFFSET: usize = HEADER_LENGTH + TRANSACTION_ENTRY_LENGTH;
    const PAGES_START: usize = HEADER_LENGTH + 2 * TRANSACTION_ENTRY_LENGTH;
    const PAGE0_OFFSET: usize = PAGES_START;
    const PAGE1_OFFSET: usize = PAGES_START + PAGE_ENTRY_LENGTH;

    fn replace_checksum(bytes: &mut [u8]) {
        let checksum_offset = bytes.len() - 8;
        let checksum = crate::checksum_v1(&bytes[..checksum_offset]);
        crate::write_u64(bytes, checksum_offset, checksum);
    }

    fn empty_replay() -> EncodedReplay {
        EncodedReplay {
            kind: REPLAY_KIND_AFTER_FRONTIER,
            frontier: None,
            position: None,
            cause: None,
        }
    }

    #[test]
    fn empty_baseline_has_exact_golden_bytes_and_is_deterministic() -> Result<(), Box<dyn Error>> {
        let encoded = encode_fields(0, None, [].into_iter(), [].into_iter(), empty_replay())?;
        let expected = [
            0x4e, 0x54, 0x53, 0x51, 0x43, 0x4d, 0x50, 0x31, 0x00, 0x01, 0x00, 0x80, 0x00, 0x40,
            0x00, 0x40, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x4e, 0x54, 0x53, 0x51, 0x43, 0x4d, 0x45, 0x31, 0x5b, 0xb0, 0x28, 0xc5,
            0x2e, 0x45, 0x8e, 0xdc,
        ];
        assert_eq!(encoded, expected);
        assert_eq!(
            encode_fields(0, None, [].into_iter(), [].into_iter(), empty_replay())?,
            expected
        );

        let decoded = decode_restart_checkpoint_completeness_baseline(&encoded)?;
        assert_eq!(decoded.transactions().persistent_log_id(), 0);
        assert_eq!(decoded.transactions().durable_frontier(), None);
        assert!(decoded.transactions().transactions().is_empty());
        assert!(decoded.pages().is_empty());
        assert_eq!(
            decoded.replay().kind(),
            DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AfterFrontier
        );
        assert_eq!(decoded.replay().frontier(), None);
        assert_eq!(decoded.replay().position(), None);
        assert_eq!(decoded.replay().cause(), None);

        let borrowed = decoded.as_observation();
        assert_eq!(borrowed.transactions().persistent_log_id(), 0);
        assert!(borrowed.pages().is_empty());
        Ok(())
    }

    #[test]
    fn full_authoritative_shape_round_trips_every_independent_field() -> Result<(), Box<dyn Error>>
    {
        let encoded = sample_blob()?;
        let decoded = decode_restart_checkpoint_completeness_baseline(&encoded)?;

        assert_eq!(
            decoded.transactions().persistent_log_id(),
            0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10
        );
        assert_eq!(decoded.transactions().durable_frontier(), Some(42));
        assert_eq!(decoded.transactions().transactions().len(), 2);
        let first_tx = decoded.transactions().transactions()[0];
        assert_eq!(first_tx.epoch(), 7);
        assert_eq!(first_tx.sequence(), 3);
        assert_eq!(first_tx.first_owned_page_position(), Some(1));
        assert_eq!(first_tx.last_owned_page_position(), Some(2));
        assert_eq!(first_tx.owned_page_record_count(), 2);
        assert_eq!(
            first_tx.state(),
            DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted
        );
        let second_tx = decoded.transactions().transactions()[1];
        assert_eq!(second_tx.epoch(), 9);
        assert_eq!(second_tx.sequence(), 1);
        assert_eq!(second_tx.first_owned_page_position(), None);
        assert_eq!(second_tx.last_owned_page_position(), None);
        assert_eq!(
            second_tx.state(),
            DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                commit_position: 5
            }
        );

        assert_eq!(decoded.pages().len(), 2);
        let first_page = decoded.pages()[0];
        assert_eq!(first_page.page_number(), 11);
        assert_eq!(
            first_page.state(),
            DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreMissing
        );
        assert_eq!(
            first_page.required_image(),
            Some(
                DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::Raw {
                    page_position: 4
                }
            )
        );
        assert_eq!(first_page.stored_position(), None);

        let second_page = decoded.pages()[1];
        assert_eq!(second_page.page_number(), 12);
        assert_eq!(
            second_page.state(),
            DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent
        );
        assert_eq!(
            second_page.required_image(),
            Some(
                DurableTransactionRestartCheckpointCompletenessBaselineRequiredImageObservation::CommittedTransaction {
                    epoch: 9,
                    sequence: 1,
                    page_position: 6,
                    commit_position: 5,
                }
            )
        );
        assert_eq!(second_page.stored_position(), Some(6));

        assert_eq!(
            decoded.replay().kind(),
            DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition
        );
        assert_eq!(decoded.replay().frontier(), None);
        assert_eq!(decoded.replay().position(), Some(4));
        assert_eq!(
            decoded.replay().cause(),
            Some(
                DurableTransactionRestartCheckpointCompletenessBaselineReplayCauseObservation::StoreMissingPage {
                    page_number: 11
                }
            )
        );

        assert_eq!(encode_fields(
            0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            Some(42),
            [
                EncodedTransactionEntry {
                    epoch: 7,
                    sequence: 3,
                    first_owned_page_position: Some(1),
                    last_owned_page_position: Some(2),
                    owned_page_record_count: 2,
                    state: DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted,
                },
                EncodedTransactionEntry {
                    epoch: 9,
                    sequence: 1,
                    first_owned_page_position: None,
                    last_owned_page_position: None,
                    owned_page_record_count: 0,
                    state: DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                        commit_position: 5,
                    },
                },
            ]
            .into_iter(),
            [
                EncodedPageEntry {
                    page_number: 11,
                    state: PAGE_STATE_STORE_MISSING,
                    required_image: Some(EncodedRequiredImage::Raw { page_position: 4 }),
                    stored_position: None,
                },
                EncodedPageEntry {
                    page_number: 12,
                    state: PAGE_STATE_STORE_CURRENT,
                    required_image: Some(EncodedRequiredImage::CommittedTransaction {
                        epoch: 9,
                        sequence: 1,
                        page_position: 6,
                        commit_position: 5,
                    }),
                    stored_position: Some(6),
                },
            ]
            .into_iter(),
            EncodedReplay {
                kind: REPLAY_KIND_AT_POSITION,
                frontier: None,
                position: Some(4),
                cause: Some(EncodedReplayCause::StoreMissingPage { page_number: 11 }),
            },
        )?, encoded);
        Ok(())
    }

    #[test]
    fn every_short_prefix_is_truncated_and_one_trailing_byte_is_rejected()
    -> Result<(), Box<dyn Error>> {
        let encoded = sample_blob()?;
        for truncated_length in 0..encoded.len() {
            let error =
                decode_restart_checkpoint_completeness_baseline(&encoded[..truncated_length])
                    .err()
                    .ok_or_else(|| io::Error::other("truncated completeness checkpoint decoded"))?;
            assert!(
                matches!(
                    error,
                    RestartCheckpointCompletenessBaselineDecodeError::Truncated { .. }
                ),
                "length {truncated_length} returned {error:?}"
            );
        }

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&trailing),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::TrailingBytes {
                    expected_length: encoded.len(),
                    actual_length: encoded.len() + 1,
                }
            )
        );
        Ok(())
    }

    #[test]
    fn outer_framing_and_checksum_corruption_fail_distinctly() -> Result<(), Box<dyn Error>> {
        let encoded = sample_blob()?;

        let mut header_magic = encoded.clone();
        header_magic[0] ^= 1;
        let mut actual_header_magic = HEADER_MAGIC;
        actual_header_magic[0] ^= 1;
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&header_magic),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::HeaderMagicMismatch {
                    actual: actual_header_magic
                }
            )
        );

        let mut version = encoded.clone();
        crate::write_u16(&mut version, 8, FORMAT_VERSION + 1);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&version),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::UnsupportedVersion {
                    actual: FORMAT_VERSION + 1
                }
            )
        );

        let mut header_length = encoded.clone();
        crate::write_u16(&mut header_length, 10, HEADER_LENGTH_U16 + 1);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&header_length),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::HeaderLengthMismatch {
                    actual: HEADER_LENGTH_U16 + 1
                }
            )
        );

        let mut transaction_entry_length = encoded.clone();
        crate::write_u16(
            &mut transaction_entry_length,
            12,
            TRANSACTION_ENTRY_LENGTH_U16 + 1,
        );
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&transaction_entry_length),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::TransactionEntryLengthMismatch {
                    actual: TRANSACTION_ENTRY_LENGTH_U16 + 1
                }
            )
        );

        let mut page_entry_length = encoded.clone();
        crate::write_u16(&mut page_entry_length, 14, PAGE_ENTRY_LENGTH_U16 + 1);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&page_entry_length),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::PageEntryLengthMismatch {
                    actual: PAGE_ENTRY_LENGTH_U16 + 1
                }
            )
        );

        let mut footer_length = encoded.clone();
        crate::write_u16(&mut footer_length, 16, FOOTER_LENGTH_U16 + 1);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&footer_length),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::FooterLengthMismatch {
                    actual: FOOTER_LENGTH_U16 + 1
                }
            )
        );

        let mut declared_length = encoded.clone();
        let encoded_length_u64 = u64::try_from(encoded.len())?;
        crate::write_u64(
            &mut declared_length,
            TOTAL_LENGTH_OFFSET,
            encoded_length_u64 + 1,
        );
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&declared_length),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::DeclaredLengthMismatch {
                    declared: encoded_length_u64 + 1,
                    expected: encoded_length_u64,
                }
            )
        );

        let mut footer_magic = encoded.clone();
        let footer_offset = footer_magic.len() - FOOTER_LENGTH;
        footer_magic[footer_offset] ^= 1;
        let mut actual_footer_magic = FOOTER_MAGIC;
        actual_footer_magic[0] ^= 1;
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&footer_magic),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::FooterMagicMismatch {
                    actual: actual_footer_magic
                }
            )
        );

        let mut checksum = encoded;
        checksum[TX0_OFFSET + TRANSACTION_ENTRY_EPOCH_OFFSET] ^= 1;
        let actual_checksum = crate::read_u64(&checksum, checksum.len() - 8);
        let expected_checksum = crate::checksum_v1(&checksum[..checksum.len() - 8]);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&checksum),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::ChecksumMismatch {
                    expected: expected_checksum,
                    actual: actual_checksum,
                }
            )
        );
        Ok(())
    }

    #[test]
    fn header_discriminants_and_reserved_bytes_fail_distinctly() -> Result<(), Box<dyn Error>> {
        let encoded = sample_blob()?;

        let mut frontier_presence = encoded.clone();
        frontier_presence[DURABLE_FRONTIER_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut frontier_presence);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&frontier_presence),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::FrontierPresenceInvalid {
                    actual: 2
                }
            )
        );

        let mut absent_frontier = encoded.clone();
        absent_frontier[DURABLE_FRONTIER_PRESENCE_OFFSET] = ABSENT;
        crate::write_u64(&mut absent_frontier, DURABLE_FRONTIER_OFFSET, 1);
        replace_checksum(&mut absent_frontier);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&absent_frontier),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::AbsentFrontierValueNonZero {
                    actual: 1
                }
            )
        );

        for (start, end) in [
            (HEADER_RESERVED_A_START, HEADER_RESERVED_A_END),
            (HEADER_RESERVED_B_START, HEADER_RESERVED_B_END),
            (HEADER_RESERVED_C_START, HEADER_RESERVED_C_END),
        ] {
            let mut reserved = encoded.clone();
            reserved[start] = 1;
            replace_checksum(&mut reserved);
            assert_eq!(
                decode_restart_checkpoint_completeness_baseline(&reserved),
                Err(
                    RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                        offset: start,
                        actual: 1,
                    }
                ),
                "reserved range {start}..{end}"
            );
        }

        let mut replay_kind = encoded.clone();
        replay_kind[REPLAY_KIND_OFFSET] = 2;
        replace_checksum(&mut replay_kind);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&replay_kind),
            Err(RestartCheckpointCompletenessBaselineDecodeError::ReplayKindInvalid { actual: 2 })
        );

        let mut replay_frontier_presence = encoded.clone();
        replay_frontier_presence[REPLAY_FRONTIER_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut replay_frontier_presence);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&replay_frontier_presence),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayFrontierPresenceInvalid {
                    actual: 2
                }
            )
        );

        let mut absent_replay_frontier = encoded.clone();
        crate::write_u64(&mut absent_replay_frontier, REPLAY_FRONTIER_OFFSET, 1);
        replace_checksum(&mut absent_replay_frontier);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&absent_replay_frontier),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::AbsentReplayFrontierValueNonZero {
                    actual: 1
                }
            )
        );

        let mut replay_position_presence = encoded.clone();
        replay_position_presence[REPLAY_POSITION_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut replay_position_presence);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&replay_position_presence),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayPositionPresenceInvalid {
                    actual: 2
                }
            )
        );

        let mut absent_replay_position = encoded.clone();
        absent_replay_position[REPLAY_POSITION_PRESENCE_OFFSET] = ABSENT;
        replace_checksum(&mut absent_replay_position);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&absent_replay_position),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::AbsentReplayPositionValueNonZero {
                    actual: 4
                }
            )
        );

        let mut replay_cause_discriminant = encoded.clone();
        replay_cause_discriminant[REPLAY_CAUSE_DISCRIMINANT_OFFSET] = 4;
        replace_checksum(&mut replay_cause_discriminant);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&replay_cause_discriminant),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseDiscriminantInvalid {
                    actual: 4
                }
            )
        );

        let mut absent_cause_epoch = encoded.clone();
        absent_cause_epoch[REPLAY_CAUSE_DISCRIMINANT_OFFSET] = REPLAY_CAUSE_ABSENT;
        crate::write_u64(&mut absent_cause_epoch, REPLAY_CAUSE_PAGE_NUMBER_OFFSET, 0);
        crate::write_u64(&mut absent_cause_epoch, REPLAY_CAUSE_EPOCH_OFFSET, 9);
        replace_checksum(&mut absent_cause_epoch);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&absent_cause_epoch),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseFieldNonZero {
                    field: RestartCheckpointCompletenessBaselineReplayCauseField::Epoch,
                    actual: 9,
                }
            )
        );

        let mut page_cause_with_epoch = encoded.clone();
        crate::write_u64(&mut page_cause_with_epoch, REPLAY_CAUSE_EPOCH_OFFSET, 9);
        replace_checksum(&mut page_cause_with_epoch);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&page_cause_with_epoch),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseFieldNonZero {
                    field: RestartCheckpointCompletenessBaselineReplayCauseField::Epoch,
                    actual: 9,
                }
            )
        );

        let mut uncommitted_cause_with_page = encoded;
        uncommitted_cause_with_page[REPLAY_CAUSE_DISCRIMINANT_OFFSET] =
            REPLAY_CAUSE_UNCOMMITTED_TRANSACTION;
        replace_checksum(&mut uncommitted_cause_with_page);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&uncommitted_cause_with_page),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReplayCauseFieldNonZero {
                    field: RestartCheckpointCompletenessBaselineReplayCauseField::PageNumber,
                    actual: 11,
                }
            )
        );
        Ok(())
    }

    #[test]
    fn transaction_entry_discriminants_and_reserved_bytes_fail_distinctly()
    -> Result<(), Box<dyn Error>> {
        let encoded = sample_blob()?;

        let mut state = encoded.clone();
        state[TX0_OFFSET + TRANSACTION_ENTRY_STATE_OFFSET] = 2;
        replace_checksum(&mut state);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&state),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::EntryStateInvalid {
                    transaction_index: 0,
                    actual: 2,
                }
            )
        );

        let mut first_presence = encoded.clone();
        first_presence[TX0_OFFSET + TRANSACTION_ENTRY_FIRST_POSITION_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut first_presence);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&first_presence),
            Err(RestartCheckpointCompletenessBaselineDecodeError::EntryPositionPresenceInvalid {
                transaction_index: 0,
                field: RestartCheckpointCompletenessBaselineEntryOptionalField::FirstOwnedPagePosition,
                actual: 2,
            })
        );

        let mut last_presence = encoded.clone();
        last_presence[TX0_OFFSET + TRANSACTION_ENTRY_LAST_POSITION_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut last_presence);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&last_presence),
            Err(RestartCheckpointCompletenessBaselineDecodeError::EntryPositionPresenceInvalid {
                transaction_index: 0,
                field: RestartCheckpointCompletenessBaselineEntryOptionalField::LastOwnedPagePosition,
                actual: 2,
            })
        );

        let mut absent_first = encoded.clone();
        absent_first[TX1_OFFSET + TRANSACTION_ENTRY_FIRST_POSITION_PRESENCE_OFFSET] = ABSENT;
        crate::write_u64(
            &mut absent_first[TX1_OFFSET..TX1_OFFSET + TRANSACTION_ENTRY_LENGTH],
            TRANSACTION_ENTRY_FIRST_OWNED_PAGE_POSITION_OFFSET,
            1,
        );
        replace_checksum(&mut absent_first);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&absent_first),
            Err(RestartCheckpointCompletenessBaselineDecodeError::AbsentEntryPositionValueNonZero {
                transaction_index: 1,
                field: RestartCheckpointCompletenessBaselineEntryOptionalField::FirstOwnedPagePosition,
                actual: 1,
            })
        );

        let mut absent_last = encoded.clone();
        crate::write_u64(
            &mut absent_last[TX1_OFFSET..TX1_OFFSET + TRANSACTION_ENTRY_LENGTH],
            TRANSACTION_ENTRY_LAST_OWNED_PAGE_POSITION_OFFSET,
            1,
        );
        replace_checksum(&mut absent_last);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&absent_last),
            Err(RestartCheckpointCompletenessBaselineDecodeError::AbsentEntryPositionValueNonZero {
                transaction_index: 1,
                field: RestartCheckpointCompletenessBaselineEntryOptionalField::LastOwnedPagePosition,
                actual: 1,
            })
        );

        let mut uncommitted_position = encoded.clone();
        crate::write_u64(
            &mut uncommitted_position[TX0_OFFSET..TX0_OFFSET + TRANSACTION_ENTRY_LENGTH],
            TRANSACTION_ENTRY_COMMIT_POSITION_OFFSET,
            1,
        );
        replace_checksum(&mut uncommitted_position);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&uncommitted_position),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::UncommittedPositionValueNonZero {
                    transaction_index: 0,
                    actual: 1,
                }
            )
        );

        let mut entry_reserved = encoded;
        entry_reserved[TX1_OFFSET + TRANSACTION_ENTRY_RESERVED_START] = 1;
        replace_checksum(&mut entry_reserved);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&entry_reserved),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                    offset: TX1_OFFSET + TRANSACTION_ENTRY_RESERVED_START,
                    actual: 1,
                }
            )
        );
        Ok(())
    }

    #[test]
    fn page_entry_discriminants_presence_and_reserved_bytes_fail_distinctly()
    -> Result<(), Box<dyn Error>> {
        let encoded = sample_blob()?;

        let mut page_state = encoded.clone();
        page_state[PAGE0_OFFSET + PAGE_ENTRY_STATE_OFFSET] = 4;
        replace_checksum(&mut page_state);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&page_state),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::PageStateInvalid {
                    page_index: 0,
                    actual: 4,
                }
            )
        );

        let mut required_presence = encoded.clone();
        required_presence[PAGE0_OFFSET + PAGE_ENTRY_REQUIRED_IMAGE_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut required_presence);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&required_presence),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::RequiredImagePresenceInvalid {
                    page_index: 0,
                    actual: 2,
                }
            )
        );

        let mut required_kind_invalid = encoded.clone();
        required_kind_invalid[PAGE0_OFFSET + PAGE_ENTRY_REQUIRED_IMAGE_KIND_OFFSET] = 2;
        replace_checksum(&mut required_kind_invalid);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&required_kind_invalid),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::RequiredImageKindInvalid {
                    page_index: 0,
                    actual: 2,
                }
            )
        );

        // Page 1's required image is `CommittedTransaction` (kind byte 1), so
        // marking it absent while leaving the kind byte untouched exercises the
        // absent-but-nonzero-kind canonical check independently of payloads.
        let mut kind_when_absent = encoded.clone();
        kind_when_absent[PAGE1_OFFSET + PAGE_ENTRY_REQUIRED_IMAGE_PRESENCE_OFFSET] = ABSENT;
        replace_checksum(&mut kind_when_absent);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&kind_when_absent),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::RequiredImageKindNonZeroWhenAbsent {
                    page_index: 1,
                    actual: REQUIRED_IMAGE_KIND_COMMITTED_TRANSACTION,
                }
            )
        );

        for (field, offset) in [
            (
                RestartCheckpointCompletenessBaselineRequiredImageField::PagePosition,
                PAGE_ENTRY_REQUIRED_IMAGE_PAGE_POSITION_OFFSET,
            ),
            (
                RestartCheckpointCompletenessBaselineRequiredImageField::Epoch,
                PAGE_ENTRY_REQUIRED_IMAGE_EPOCH_OFFSET,
            ),
            (
                RestartCheckpointCompletenessBaselineRequiredImageField::Sequence,
                PAGE_ENTRY_REQUIRED_IMAGE_SEQUENCE_OFFSET,
            ),
            (
                RestartCheckpointCompletenessBaselineRequiredImageField::CommitPosition,
                PAGE_ENTRY_REQUIRED_IMAGE_COMMIT_POSITION_OFFSET,
            ),
        ] {
            let mut absent_payload = encoded.clone();
            absent_payload[PAGE0_OFFSET + PAGE_ENTRY_REQUIRED_IMAGE_PRESENCE_OFFSET] = ABSENT;
            absent_payload[PAGE0_OFFSET + PAGE_ENTRY_REQUIRED_IMAGE_KIND_OFFSET] = 0;
            // Zero every required-image payload field first so only the field
            // under test is nonzero; page 0's original Raw image otherwise
            // leaves its own page-position payload nonzero.
            for zeroed_offset in [
                PAGE_ENTRY_REQUIRED_IMAGE_PAGE_POSITION_OFFSET,
                PAGE_ENTRY_REQUIRED_IMAGE_EPOCH_OFFSET,
                PAGE_ENTRY_REQUIRED_IMAGE_SEQUENCE_OFFSET,
                PAGE_ENTRY_REQUIRED_IMAGE_COMMIT_POSITION_OFFSET,
            ] {
                crate::write_u64(
                    &mut absent_payload[PAGE0_OFFSET..PAGE0_OFFSET + PAGE_ENTRY_LENGTH],
                    zeroed_offset,
                    0,
                );
            }
            crate::write_u64(
                &mut absent_payload[PAGE0_OFFSET..PAGE0_OFFSET + PAGE_ENTRY_LENGTH],
                offset,
                7,
            );
            replace_checksum(&mut absent_payload);
            assert_eq!(
                decode_restart_checkpoint_completeness_baseline(&absent_payload),
                Err(
                    RestartCheckpointCompletenessBaselineDecodeError::AbsentRequiredImagePayloadNonZero {
                        page_index: 0,
                        field,
                        actual: 7,
                    }
                ),
                "field {field}"
            );
        }

        for (field, offset) in [
            (
                RestartCheckpointCompletenessBaselineRequiredImageField::Epoch,
                PAGE_ENTRY_REQUIRED_IMAGE_EPOCH_OFFSET,
            ),
            (
                RestartCheckpointCompletenessBaselineRequiredImageField::Sequence,
                PAGE_ENTRY_REQUIRED_IMAGE_SEQUENCE_OFFSET,
            ),
            (
                RestartCheckpointCompletenessBaselineRequiredImageField::CommitPosition,
                PAGE_ENTRY_REQUIRED_IMAGE_COMMIT_POSITION_OFFSET,
            ),
        ] {
            let mut raw_payload = encoded.clone();
            crate::write_u64(
                &mut raw_payload[PAGE0_OFFSET..PAGE0_OFFSET + PAGE_ENTRY_LENGTH],
                offset,
                9,
            );
            replace_checksum(&mut raw_payload);
            assert_eq!(
                decode_restart_checkpoint_completeness_baseline(&raw_payload),
                Err(
                    RestartCheckpointCompletenessBaselineDecodeError::RawRequiredImagePayloadNonZero {
                        page_index: 0,
                        field,
                        actual: 9,
                    }
                ),
                "field {field}"
            );
        }

        let mut stored_presence = encoded.clone();
        stored_presence[PAGE1_OFFSET + PAGE_ENTRY_STORED_POSITION_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut stored_presence);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&stored_presence),
            Err(
                RestartCheckpointCompletenessBaselineDecodeError::StoredPositionPresenceInvalid {
                    page_index: 1,
                    actual: 2,
                }
            )
        );

        let mut absent_stored = encoded.clone();
        crate::write_u64(
            &mut absent_stored[PAGE0_OFFSET..PAGE0_OFFSET + PAGE_ENTRY_LENGTH],
            PAGE_ENTRY_STORED_POSITION_OFFSET,
            3,
        );
        replace_checksum(&mut absent_stored);
        assert_eq!(
            decode_restart_checkpoint_completeness_baseline(&absent_stored),
            Err(RestartCheckpointCompletenessBaselineDecodeError::AbsentStoredPositionValueNonZero {
                page_index: 0,
                actual: 3,
            })
        );

        for (start, page_offset) in [
            (PAGE_ENTRY_RESERVED_A_START, PAGE0_OFFSET),
            (PAGE_ENTRY_RESERVED_B_START, PAGE1_OFFSET),
        ] {
            let mut reserved = encoded.clone();
            reserved[page_offset + start] = 1;
            replace_checksum(&mut reserved);
            assert_eq!(
                decode_restart_checkpoint_completeness_baseline(&reserved),
                Err(
                    RestartCheckpointCompletenessBaselineDecodeError::ReservedByteNonZero {
                        offset: page_offset + start,
                        actual: 1,
                    }
                )
            );
        }
        Ok(())
    }

    #[test]
    fn semantically_invalid_raw_fields_survive_decode_unchanged() -> Result<(), Box<dyn Error>> {
        let encoded = encode_fields(
            0,
            Some(0),
            [EncodedTransactionEntry {
                epoch: 0,
                sequence: 0,
                first_owned_page_position: Some(0),
                last_owned_page_position: None,
                owned_page_record_count: 0,
                state: DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                    commit_position: 0,
                },
            }]
            .into_iter(),
            [EncodedPageEntry {
                page_number: 0,
                state: PAGE_STATE_STORE_CURRENT,
                required_image: None,
                stored_position: None,
            }]
            .into_iter(),
            EncodedReplay {
                kind: REPLAY_KIND_AT_POSITION,
                frontier: Some(0),
                position: None,
                cause: None,
            },
        )?;
        let decoded = decode_restart_checkpoint_completeness_baseline(&encoded)?;
        assert_eq!(decoded.transactions().persistent_log_id(), 0);
        assert_eq!(decoded.transactions().durable_frontier(), Some(0));
        assert_eq!(decoded.transactions().transactions()[0].epoch(), 0);
        assert_eq!(decoded.pages()[0].page_number(), 0);
        // The state discriminant (StoreCurrent) structurally contradicts the
        // independently absent required image and stored position, and the
        // codec preserves that contradiction rather than rejecting it.
        assert_eq!(
            decoded.pages()[0].state(),
            DurableTransactionRestartCheckpointCompletenessBaselinePageStateObservation::StoreCurrent
        );
        assert_eq!(decoded.pages()[0].required_image(), None);
        assert_eq!(decoded.pages()[0].stored_position(), None);
        // The replay kind (AtPosition) structurally contradicts the
        // independently present frontier and absent position/cause.
        assert_eq!(
            decoded.replay().kind(),
            DurableTransactionRestartCheckpointCompletenessBaselineReplayKindObservation::AtPosition
        );
        assert_eq!(decoded.replay().frontier(), Some(0));
        assert_eq!(decoded.replay().position(), None);
        assert_eq!(decoded.replay().cause(), None);
        Ok(())
    }

    #[test]
    fn impossible_counts_and_capacity_errors_remain_typed() -> Result<(), Box<dyn Error>> {
        let mut encoded = sample_blob()?;
        crate::write_u64(&mut encoded, TRANSACTION_COUNT_OFFSET, u64::MAX);
        let count_error = decode_restart_checkpoint_completeness_baseline(&encoded)
            .err()
            .ok_or_else(|| io::Error::other("impossible transaction count decoded"))?;
        assert!(matches!(
            count_error,
            RestartCheckpointCompletenessBaselineDecodeError::TransactionCountOutOfRange { .. }
                | RestartCheckpointCompletenessBaselineDecodeError::EncodedLengthOverflow { .. }
        ));

        let mut page_encoded = sample_blob()?;
        crate::write_u64(&mut page_encoded, PAGE_COUNT_OFFSET, u64::MAX);
        let page_count_error = decode_restart_checkpoint_completeness_baseline(&page_encoded)
            .err()
            .ok_or_else(|| io::Error::other("impossible page count decoded"))?;
        assert!(matches!(
            page_count_error,
            RestartCheckpointCompletenessBaselineDecodeError::PageCountOutOfRange { .. }
                | RestartCheckpointCompletenessBaselineDecodeError::EncodedLengthOverflow { .. }
        ));

        let encode_transaction_capacity =
            RestartCheckpointCompletenessBaselineEncodeError::CapacityExhausted {
                encoded_length: 400,
            };
        assert!(Error::source(&encode_transaction_capacity).is_none());
        assert!(
            encode_transaction_capacity
                .to_string()
                .contains("400 bytes")
        );

        let encode_overflow =
            RestartCheckpointCompletenessBaselineEncodeError::EncodedLengthOverflow {
                transaction_count: 3,
                page_count: 5,
            };
        assert!(
            encode_overflow
                .to_string()
                .contains("3 transaction entries and 5 page entries")
        );

        let decode_transaction_capacity =
            RestartCheckpointCompletenessBaselineDecodeError::TransactionCapacityExhausted {
                transaction_count: 1,
            };
        assert!(Error::source(&decode_transaction_capacity).is_none());
        assert!(
            decode_transaction_capacity
                .to_string()
                .contains("1 transaction entries")
        );

        let decode_page_capacity =
            RestartCheckpointCompletenessBaselineDecodeError::PageCapacityExhausted {
                page_count: 2,
            };
        assert!(Error::source(&decode_page_capacity).is_none());
        assert!(decode_page_capacity.to_string().contains("2 page entries"));
        Ok(())
    }
}
