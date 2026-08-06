use std::{error::Error, fmt};

use ntsql_transaction::{
    DurableTransactionRestartCheckpointBaseline, DurableTransactionRestartCheckpointBaselineEntry,
    DurableTransactionRestartCheckpointBaselineEntryObservation,
    DurableTransactionRestartCheckpointBaselineState,
    DurableTransactionRestartCheckpointBaselineStateObservation,
    OwnedDurableTransactionRestartCheckpointBaselineObservation,
};

const HEADER_MAGIC: [u8; 8] = *b"NTSQCKP1";
const FOOTER_MAGIC: [u8; 8] = *b"NTSQCKE1";
const FORMAT_VERSION: u16 = 1;
const HEADER_LENGTH: usize = 64;
const HEADER_LENGTH_U16: u16 = 64;
const HEADER_LENGTH_U64: u64 = 64;
const ENTRY_LENGTH: usize = 64;
const ENTRY_LENGTH_U16: u16 = 64;
const ENTRY_LENGTH_U64: u64 = 64;
const FOOTER_LENGTH: usize = 16;
const FOOTER_LENGTH_U16: u16 = 16;
const FOOTER_LENGTH_U64: u64 = 16;

const PERSISTENT_LOG_ID_OFFSET: usize = 16;
const DURABLE_FRONTIER_OFFSET: usize = 32;
const DURABLE_FRONTIER_PRESENCE_OFFSET: usize = 40;
const HEADER_RESERVED_START: usize = 41;
const HEADER_RESERVED_END: usize = 48;
const TRANSACTION_COUNT_OFFSET: usize = 48;
const TOTAL_LENGTH_OFFSET: usize = 56;

const ENTRY_EPOCH_OFFSET: usize = 0;
const ENTRY_SEQUENCE_OFFSET: usize = 8;
const ENTRY_FIRST_OWNED_PAGE_POSITION_OFFSET: usize = 16;
const ENTRY_LAST_OWNED_PAGE_POSITION_OFFSET: usize = 24;
const ENTRY_OWNED_PAGE_RECORD_COUNT_OFFSET: usize = 32;
const ENTRY_COMMIT_POSITION_OFFSET: usize = 40;
const ENTRY_STATE_OFFSET: usize = 48;
const ENTRY_FIRST_POSITION_PRESENCE_OFFSET: usize = 49;
const ENTRY_LAST_POSITION_PRESENCE_OFFSET: usize = 50;
const ENTRY_RESERVED_START: usize = 51;
const ENTRY_RESERVED_END: usize = 64;

const ABSENT: u8 = 0;
const PRESENT: u8 = 1;
const STATE_UNCOMMITTED: u8 = 0;
const STATE_COMMITTED: u8 = 1;

/// Failure to encode one authoritative transaction restart checkpoint baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointBaselineEncodeError {
    /// The transaction count cannot be represented by the format's `u64` field.
    TransactionCountOutOfRange {
        /// Exact host-sized count that was rejected.
        transaction_count: usize,
    },
    /// The fixed-width blob length overflowed host-sized arithmetic.
    EncodedLengthOverflow {
        /// Exact transaction count used in the failed calculation.
        transaction_count: usize,
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

impl fmt::Display for RestartCheckpointBaselineEncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransactionCountOutOfRange { transaction_count } => write!(
                formatter,
                "restart checkpoint transaction count {transaction_count} is not representable as u64"
            ),
            Self::EncodedLengthOverflow { transaction_count } => write!(
                formatter,
                "restart checkpoint encoded length overflowed for {transaction_count} transaction entries"
            ),
            Self::EncodedLengthOutOfRange { encoded_length } => write!(
                formatter,
                "restart checkpoint encoded length {encoded_length} is not representable as u64"
            ),
            Self::CapacityExhausted { encoded_length } => write!(
                formatter,
                "restart checkpoint output capacity is exhausted for {encoded_length} bytes"
            ),
        }
    }
}

impl Error for RestartCheckpointBaselineEncodeError {}

/// Optional numeric field inside one encoded checkpoint transaction entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointBaselineEntryOptionalField {
    /// First transaction-owned page-record position.
    FirstOwnedPagePosition,
    /// Last transaction-owned page-record position.
    LastOwnedPagePosition,
}

impl fmt::Display for RestartCheckpointBaselineEntryOptionalField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FirstOwnedPagePosition => formatter.write_str("first owned-page position"),
            Self::LastOwnedPagePosition => formatter.write_str("last owned-page position"),
        }
    }
}

/// Structural failure to decode one versioned restart checkpoint baseline blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartCheckpointBaselineDecodeError {
    /// The input ended before the complete declared structural boundary.
    Truncated {
        /// Minimum or declared byte length required at this stage.
        expected_length: usize,
        /// Exact supplied byte length.
        actual_length: usize,
    },
    /// The independent checkpoint header magic did not match.
    HeaderMagicMismatch {
        /// Exact eight bytes found at the header magic offset.
        actual: [u8; 8],
    },
    /// The checkpoint format version is not supported.
    UnsupportedVersion {
        /// Exact decoded version.
        actual: u16,
    },
    /// The encoded fixed header length is not version 1's exact width.
    HeaderLengthMismatch {
        /// Exact decoded header length.
        actual: u16,
    },
    /// The encoded fixed entry length is not version 1's exact width.
    EntryLengthMismatch {
        /// Exact decoded entry length.
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
    /// The fixed-width expected length overflowed the format's `u64` arithmetic.
    EncodedLengthOverflow {
        /// Exact decoded count used in the failed calculation.
        transaction_count: u64,
    },
    /// The declared total length cannot be represented on this host.
    TotalLengthOutOfRange {
        /// Exact decoded total length.
        total_length: u64,
    },
    /// The declared total length disagreed with the fixed geometry and count.
    DeclaredLengthMismatch {
        /// Exact decoded total length.
        declared: u64,
        /// Exact length implied by version 1 geometry and count.
        expected: u64,
    },
    /// Bytes followed the one complete declared blob.
    TrailingBytes {
        /// Exact declared and structurally expected byte length.
        expected_length: usize,
        /// Exact supplied byte length.
        actual_length: usize,
    },
    /// The independent checkpoint footer magic did not match.
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
    /// One entry state discriminant was not uncommitted or committed.
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
        field: RestartCheckpointBaselineEntryOptionalField,
        /// Exact decoded discriminant.
        actual: u8,
    },
    /// An absent optional entry position retained a nonzero payload.
    AbsentEntryPositionValueNonZero {
        /// Zero-based transaction entry index.
        transaction_index: usize,
        /// Optional field whose absent payload was nonzero.
        field: RestartCheckpointBaselineEntryOptionalField,
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
    CapacityExhausted {
        /// Exact host-sized count that required reservation.
        transaction_count: usize,
    },
}

impl fmt::Display for RestartCheckpointBaselineDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                expected_length,
                actual_length,
            } => write!(
                formatter,
                "restart checkpoint is truncated: expected at least {expected_length} bytes, found {actual_length}"
            ),
            Self::HeaderMagicMismatch { actual } => {
                write!(
                    formatter,
                    "restart checkpoint header magic is invalid: {actual:?}"
                )
            }
            Self::UnsupportedVersion { actual } => {
                write!(
                    formatter,
                    "restart checkpoint version {actual} is unsupported"
                )
            }
            Self::HeaderLengthMismatch { actual } => write!(
                formatter,
                "restart checkpoint header length {actual} is invalid"
            ),
            Self::EntryLengthMismatch { actual } => write!(
                formatter,
                "restart checkpoint entry length {actual} is invalid"
            ),
            Self::FooterLengthMismatch { actual } => write!(
                formatter,
                "restart checkpoint footer length {actual} is invalid"
            ),
            Self::TransactionCountOutOfRange { transaction_count } => write!(
                formatter,
                "restart checkpoint transaction count {transaction_count} is not representable on this host"
            ),
            Self::EncodedLengthOverflow { transaction_count } => write!(
                formatter,
                "restart checkpoint encoded length overflowed for {transaction_count} transaction entries"
            ),
            Self::TotalLengthOutOfRange { total_length } => write!(
                formatter,
                "restart checkpoint total length {total_length} is not representable on this host"
            ),
            Self::DeclaredLengthMismatch { declared, expected } => write!(
                formatter,
                "restart checkpoint declared length {declared} does not match expected length {expected}"
            ),
            Self::TrailingBytes {
                expected_length,
                actual_length,
            } => write!(
                formatter,
                "restart checkpoint has trailing bytes: expected {expected_length} bytes, found {actual_length}"
            ),
            Self::FooterMagicMismatch { actual } => {
                write!(
                    formatter,
                    "restart checkpoint footer magic is invalid: {actual:?}"
                )
            }
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "restart checkpoint checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
            Self::FrontierPresenceInvalid { actual } => write!(
                formatter,
                "restart checkpoint frontier presence value {actual} is invalid"
            ),
            Self::AbsentFrontierValueNonZero { actual } => write!(
                formatter,
                "restart checkpoint absent frontier has nonzero payload {actual}"
            ),
            Self::ReservedByteNonZero { offset, actual } => write!(
                formatter,
                "restart checkpoint reserved byte at offset {offset} is nonzero: {actual}"
            ),
            Self::EntryStateInvalid {
                transaction_index,
                actual,
            } => write!(
                formatter,
                "restart checkpoint transaction entry {transaction_index} state value {actual} is invalid"
            ),
            Self::EntryPositionPresenceInvalid {
                transaction_index,
                field,
                actual,
            } => write!(
                formatter,
                "restart checkpoint transaction entry {transaction_index} {field} presence value {actual} is invalid"
            ),
            Self::AbsentEntryPositionValueNonZero {
                transaction_index,
                field,
                actual,
            } => write!(
                formatter,
                "restart checkpoint transaction entry {transaction_index} absent {field} has nonzero payload {actual}"
            ),
            Self::UncommittedPositionValueNonZero {
                transaction_index,
                actual,
            } => write!(
                formatter,
                "restart checkpoint transaction entry {transaction_index} is uncommitted with nonzero commit position {actual}"
            ),
            Self::CapacityExhausted { transaction_count } => write!(
                formatter,
                "restart checkpoint decode capacity is exhausted for {transaction_count} transaction entries"
            ),
        }
    }
}

impl Error for RestartCheckpointBaselineDecodeError {}

#[derive(Clone, Copy)]
struct EncodedEntry {
    epoch: u64,
    sequence: u64,
    first_owned_page_position: Option<u64>,
    last_owned_page_position: Option<u64>,
    owned_page_record_count: u64,
    state: DurableTransactionRestartCheckpointBaselineStateObservation,
}

impl EncodedEntry {
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

/// Encodes one authoritative transaction restart checkpoint baseline.
///
/// The returned bytes contain inert transaction metadata only. They are not a
/// filesystem publication, startup selection, replay plan, or retention proof.
///
/// An untrusted decoded observation cannot substitute for the authoritative
/// encoder input:
///
/// ```compile_fail
/// use ntsql_storage_file::encode_restart_checkpoint_baseline;
/// use ntsql_transaction::OwnedDurableTransactionRestartCheckpointBaselineObservation;
///
/// fn cannot_encode_untrusted(
///     observation: &OwnedDurableTransactionRestartCheckpointBaselineObservation,
/// ) {
///     let _ = encode_restart_checkpoint_baseline(observation);
/// }
/// ```
pub fn encode_restart_checkpoint_baseline(
    baseline: &DurableTransactionRestartCheckpointBaseline,
) -> Result<Vec<u8>, RestartCheckpointBaselineEncodeError> {
    encode_fields(
        baseline.persistent_log_id().get(),
        baseline.durable_frontier(),
        baseline
            .transactions()
            .iter()
            .map(EncodedEntry::from_baseline),
    )
}

fn encode_fields<Entries>(
    persistent_log_id: u128,
    durable_frontier: Option<u64>,
    entries: Entries,
) -> Result<Vec<u8>, RestartCheckpointBaselineEncodeError>
where
    Entries: ExactSizeIterator<Item = EncodedEntry>,
{
    let transaction_count = entries.len();
    let transaction_count_u64 = u64::try_from(transaction_count).map_err(|_| {
        RestartCheckpointBaselineEncodeError::TransactionCountOutOfRange { transaction_count }
    })?;
    let entries_length = transaction_count
        .checked_mul(ENTRY_LENGTH)
        .ok_or(RestartCheckpointBaselineEncodeError::EncodedLengthOverflow { transaction_count })?;
    let encoded_length = HEADER_LENGTH
        .checked_add(entries_length)
        .and_then(|length| length.checked_add(FOOTER_LENGTH))
        .ok_or(RestartCheckpointBaselineEncodeError::EncodedLengthOverflow { transaction_count })?;
    let encoded_length_u64 = u64::try_from(encoded_length).map_err(|_| {
        RestartCheckpointBaselineEncodeError::EncodedLengthOutOfRange { encoded_length }
    })?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(encoded_length)
        .map_err(|_| RestartCheckpointBaselineEncodeError::CapacityExhausted { encoded_length })?;

    let mut header = [0_u8; HEADER_LENGTH];
    header[..8].copy_from_slice(&HEADER_MAGIC);
    super::write_u16(&mut header, 8, FORMAT_VERSION);
    super::write_u16(&mut header, 10, HEADER_LENGTH_U16);
    super::write_u16(&mut header, 12, ENTRY_LENGTH_U16);
    super::write_u16(&mut header, 14, FOOTER_LENGTH_U16);
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
    super::write_u64(&mut header, TOTAL_LENGTH_OFFSET, encoded_length_u64);
    encoded.extend_from_slice(&header);

    for entry in entries {
        let mut bytes = [0_u8; ENTRY_LENGTH];
        super::write_u64(&mut bytes, ENTRY_EPOCH_OFFSET, entry.epoch);
        super::write_u64(&mut bytes, ENTRY_SEQUENCE_OFFSET, entry.sequence);
        write_optional_u64(
            &mut bytes,
            ENTRY_FIRST_OWNED_PAGE_POSITION_OFFSET,
            ENTRY_FIRST_POSITION_PRESENCE_OFFSET,
            entry.first_owned_page_position,
        );
        write_optional_u64(
            &mut bytes,
            ENTRY_LAST_OWNED_PAGE_POSITION_OFFSET,
            ENTRY_LAST_POSITION_PRESENCE_OFFSET,
            entry.last_owned_page_position,
        );
        super::write_u64(
            &mut bytes,
            ENTRY_OWNED_PAGE_RECORD_COUNT_OFFSET,
            entry.owned_page_record_count,
        );
        match entry.state {
            DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted => {
                bytes[ENTRY_STATE_OFFSET] = STATE_UNCOMMITTED;
            }
            DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                commit_position,
            } => {
                super::write_u64(&mut bytes, ENTRY_COMMIT_POSITION_OFFSET, commit_position);
                bytes[ENTRY_STATE_OFFSET] = STATE_COMMITTED;
            }
        }
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

/// Decodes one structurally valid versioned checkpoint blob into untrusted fields.
///
/// Zero and contradictory domain fields are preserved for source-relative ADR
/// 0039 validation. Successful decoding does not create an authoritative
/// baseline:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointBaselineDecodeError, decode_restart_checkpoint_baseline,
/// };
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaseline;
///
/// fn cannot_authorize(
///     bytes: &[u8],
/// ) -> Result<
///     DurableTransactionRestartCheckpointBaseline,
///     RestartCheckpointBaselineDecodeError,
/// > {
///     decode_restart_checkpoint_baseline(bytes).map(Into::into)
/// }
/// ```
///
/// It also cannot create transaction lifecycle state:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointBaselineDecodeError, decode_restart_checkpoint_baseline,
/// };
/// use ntsql_transaction::ActiveTransaction;
///
/// fn cannot_activate(
///     bytes: &[u8],
/// ) -> Result<ActiveTransaction, RestartCheckpointBaselineDecodeError> {
///     decode_restart_checkpoint_baseline(bytes).map(Into::into)
/// }
/// ```
///
/// Nor can it create page-write authority:
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_storage_file::{
///     RestartCheckpointBaselineDecodeError, decode_restart_checkpoint_baseline,
/// };
///
/// fn cannot_write_page<'attempt>(
///     bytes: &[u8],
/// ) -> Result<PageWritePermit<'attempt>, RestartCheckpointBaselineDecodeError> {
///     decode_restart_checkpoint_baseline(bytes).map(Into::into)
/// }
/// ```
///
/// Decoded fields cannot satisfy WAL durability:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointBaselineDecodeError, decode_restart_checkpoint_baseline,
/// };
/// use ntsql_wal::LogDurability;
///
/// fn require_log<Log: LogDurability>(_log: &mut Log) {}
///
/// fn cannot_flush(bytes: &[u8]) -> Result<(), RestartCheckpointBaselineDecodeError> {
///     let mut decoded = decode_restart_checkpoint_baseline(bytes)?;
///     require_log(&mut decoded);
///     Ok(())
/// }
/// ```
///
/// They cannot satisfy committed-page recovery storage:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointBaselineDecodeError, decode_restart_checkpoint_baseline,
/// };
/// use ntsql_transaction::CommittedTransactionPageRecoveryStore;
///
/// fn require_store<Store: CommittedTransactionPageRecoveryStore<1>>(
///     _store: &mut Store,
/// ) {}
///
/// fn cannot_recover(bytes: &[u8]) -> Result<(), RestartCheckpointBaselineDecodeError> {
///     let mut decoded = decode_restart_checkpoint_baseline(bytes)?;
///     require_store(&mut decoded);
///     Ok(())
/// }
/// ```
///
/// They cannot become restart-analyzed storage ownership:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     RestartCheckpointBaselineDecodeError, decode_restart_checkpoint_baseline,
/// };
/// use ntsql_transaction::RestartAnalyzedTransactionPageStorage;
///
/// fn cannot_release<Source, Store>(
///     bytes: &[u8],
/// ) -> Result<
///     RestartAnalyzedTransactionPageStorage<Source, Store, 1>,
///     RestartCheckpointBaselineDecodeError,
/// > {
///     decode_restart_checkpoint_baseline(bytes).map(Into::into)
/// }
/// ```
pub fn decode_restart_checkpoint_baseline(
    bytes: &[u8],
) -> Result<
    OwnedDurableTransactionRestartCheckpointBaselineObservation,
    RestartCheckpointBaselineDecodeError,
> {
    if bytes.len() < HEADER_LENGTH {
        return Err(RestartCheckpointBaselineDecodeError::Truncated {
            expected_length: HEADER_LENGTH,
            actual_length: bytes.len(),
        });
    }

    let header_magic = copy_magic(bytes, 0);
    if header_magic != HEADER_MAGIC {
        return Err(RestartCheckpointBaselineDecodeError::HeaderMagicMismatch {
            actual: header_magic,
        });
    }
    let version = super::read_u16(bytes, 8);
    if version != FORMAT_VERSION {
        return Err(RestartCheckpointBaselineDecodeError::UnsupportedVersion { actual: version });
    }
    let header_length = super::read_u16(bytes, 10);
    if header_length != HEADER_LENGTH_U16 {
        return Err(RestartCheckpointBaselineDecodeError::HeaderLengthMismatch {
            actual: header_length,
        });
    }
    let entry_length = super::read_u16(bytes, 12);
    if entry_length != ENTRY_LENGTH_U16 {
        return Err(RestartCheckpointBaselineDecodeError::EntryLengthMismatch {
            actual: entry_length,
        });
    }
    let footer_length = super::read_u16(bytes, 14);
    if footer_length != FOOTER_LENGTH_U16 {
        return Err(RestartCheckpointBaselineDecodeError::FooterLengthMismatch {
            actual: footer_length,
        });
    }

    let transaction_count_u64 = super::read_u64(bytes, TRANSACTION_COUNT_OFFSET);
    let transaction_count = usize::try_from(transaction_count_u64).map_err(|_| {
        RestartCheckpointBaselineDecodeError::TransactionCountOutOfRange {
            transaction_count: transaction_count_u64,
        }
    })?;
    let expected_length_u64 = transaction_count_u64
        .checked_mul(ENTRY_LENGTH_U64)
        .and_then(|length| length.checked_add(HEADER_LENGTH_U64))
        .and_then(|length| length.checked_add(FOOTER_LENGTH_U64))
        .ok_or(
            RestartCheckpointBaselineDecodeError::EncodedLengthOverflow {
                transaction_count: transaction_count_u64,
            },
        )?;
    let declared_length_u64 = super::read_u64(bytes, TOTAL_LENGTH_OFFSET);
    if declared_length_u64 != expected_length_u64 {
        return Err(
            RestartCheckpointBaselineDecodeError::DeclaredLengthMismatch {
                declared: declared_length_u64,
                expected: expected_length_u64,
            },
        );
    }
    let expected_length = usize::try_from(expected_length_u64).map_err(|_| {
        RestartCheckpointBaselineDecodeError::TotalLengthOutOfRange {
            total_length: expected_length_u64,
        }
    })?;
    if bytes.len() < expected_length {
        return Err(RestartCheckpointBaselineDecodeError::Truncated {
            expected_length,
            actual_length: bytes.len(),
        });
    }
    if bytes.len() > expected_length {
        return Err(RestartCheckpointBaselineDecodeError::TrailingBytes {
            expected_length,
            actual_length: bytes.len(),
        });
    }

    let footer_offset = expected_length - FOOTER_LENGTH;
    let footer_magic = copy_magic(bytes, footer_offset);
    if footer_magic != FOOTER_MAGIC {
        return Err(RestartCheckpointBaselineDecodeError::FooterMagicMismatch {
            actual: footer_magic,
        });
    }
    let checksum_offset = expected_length - 8;
    let actual_checksum = super::read_u64(bytes, checksum_offset);
    let expected_checksum = super::checksum_v1(&bytes[..checksum_offset]);
    if actual_checksum != expected_checksum {
        return Err(RestartCheckpointBaselineDecodeError::ChecksumMismatch {
            expected: expected_checksum,
            actual: actual_checksum,
        });
    }

    for (relative_offset, actual) in bytes[HEADER_RESERVED_START..HEADER_RESERVED_END]
        .iter()
        .copied()
        .enumerate()
    {
        if actual != 0 {
            return Err(RestartCheckpointBaselineDecodeError::ReservedByteNonZero {
                offset: HEADER_RESERVED_START + relative_offset,
                actual,
            });
        }
    }
    let frontier_value = super::read_u64(bytes, DURABLE_FRONTIER_OFFSET);
    let durable_frontier =
        decode_header_frontier(bytes[DURABLE_FRONTIER_PRESENCE_OFFSET], frontier_value)?;

    let mut transactions = Vec::new();
    transactions
        .try_reserve_exact(transaction_count)
        .map_err(
            |_| RestartCheckpointBaselineDecodeError::CapacityExhausted { transaction_count },
        )?;
    for transaction_index in 0..transaction_count {
        let entry_offset = HEADER_LENGTH + transaction_index * ENTRY_LENGTH;
        let entry = &bytes[entry_offset..entry_offset + ENTRY_LENGTH];
        for (relative_offset, actual) in entry[ENTRY_RESERVED_START..ENTRY_RESERVED_END]
            .iter()
            .copied()
            .enumerate()
        {
            if actual != 0 {
                return Err(RestartCheckpointBaselineDecodeError::ReservedByteNonZero {
                    offset: entry_offset + ENTRY_RESERVED_START + relative_offset,
                    actual,
                });
            }
        }

        let first_owned_page_position = decode_entry_optional_position(
            transaction_index,
            RestartCheckpointBaselineEntryOptionalField::FirstOwnedPagePosition,
            entry[ENTRY_FIRST_POSITION_PRESENCE_OFFSET],
            super::read_u64(entry, ENTRY_FIRST_OWNED_PAGE_POSITION_OFFSET),
        )?;
        let last_owned_page_position = decode_entry_optional_position(
            transaction_index,
            RestartCheckpointBaselineEntryOptionalField::LastOwnedPagePosition,
            entry[ENTRY_LAST_POSITION_PRESENCE_OFFSET],
            super::read_u64(entry, ENTRY_LAST_OWNED_PAGE_POSITION_OFFSET),
        )?;
        let commit_position = super::read_u64(entry, ENTRY_COMMIT_POSITION_OFFSET);
        let state = match entry[ENTRY_STATE_OFFSET] {
            STATE_UNCOMMITTED => {
                if commit_position != 0 {
                    return Err(
                        RestartCheckpointBaselineDecodeError::UncommittedPositionValueNonZero {
                            transaction_index,
                            actual: commit_position,
                        },
                    );
                }
                DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted
            }
            STATE_COMMITTED => {
                DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                    commit_position,
                }
            }
            actual => {
                return Err(RestartCheckpointBaselineDecodeError::EntryStateInvalid {
                    transaction_index,
                    actual,
                });
            }
        };
        transactions.push(
            DurableTransactionRestartCheckpointBaselineEntryObservation::new(
                super::read_u64(entry, ENTRY_EPOCH_OFFSET),
                super::read_u64(entry, ENTRY_SEQUENCE_OFFSET),
                first_owned_page_position,
                last_owned_page_position,
                super::read_u64(entry, ENTRY_OWNED_PAGE_RECORD_COUNT_OFFSET),
                state,
            ),
        );
    }

    Ok(
        OwnedDurableTransactionRestartCheckpointBaselineObservation::new(
            super::read_u128(bytes, PERSISTENT_LOG_ID_OFFSET),
            durable_frontier,
            transactions,
        ),
    )
}

fn decode_header_frontier(
    presence: u8,
    value: u64,
) -> Result<Option<u64>, RestartCheckpointBaselineDecodeError> {
    match presence {
        ABSENT => {
            if value != 0 {
                Err(
                    RestartCheckpointBaselineDecodeError::AbsentFrontierValueNonZero {
                        actual: value,
                    },
                )
            } else {
                Ok(None)
            }
        }
        PRESENT => Ok(Some(value)),
        actual => Err(RestartCheckpointBaselineDecodeError::FrontierPresenceInvalid { actual }),
    }
}

fn decode_entry_optional_position(
    transaction_index: usize,
    field: RestartCheckpointBaselineEntryOptionalField,
    presence: u8,
    value: u64,
) -> Result<Option<u64>, RestartCheckpointBaselineDecodeError> {
    match presence {
        ABSENT => {
            if value != 0 {
                Err(
                    RestartCheckpointBaselineDecodeError::AbsentEntryPositionValueNonZero {
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
            RestartCheckpointBaselineDecodeError::EntryPositionPresenceInvalid {
                transaction_index,
                field,
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

    fn one_entry_blob() -> Result<Vec<u8>, RestartCheckpointBaselineEncodeError> {
        encode_fields(
            0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            Some(0),
            [EncodedEntry {
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
        )
    }

    fn replace_checksum(bytes: &mut [u8]) {
        let checksum_offset = bytes.len() - 8;
        let checksum = crate::checksum_v1(&bytes[..checksum_offset]);
        crate::write_u64(bytes, checksum_offset, checksum);
    }

    #[test]
    fn structurally_valid_semantically_invalid_fields_remain_untrusted()
    -> Result<(), Box<dyn Error>> {
        let encoded = encode_fields(
            0,
            Some(0),
            [
                EncodedEntry {
                    epoch: 0,
                    sequence: 0,
                    first_owned_page_position: Some(0),
                    last_owned_page_position: None,
                    owned_page_record_count: 0,
                    state: DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                        commit_position: 0,
                    },
                },
                EncodedEntry {
                    epoch: 7,
                    sequence: 3,
                    first_owned_page_position: None,
                    last_owned_page_position: Some(0),
                    owned_page_record_count: u64::MAX,
                    state: DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted,
                },
            ]
            .into_iter(),
        )?;
        let decoded = decode_restart_checkpoint_baseline(&encoded)?;
        assert_eq!(decoded.persistent_log_id(), 0);
        assert_eq!(decoded.durable_frontier(), Some(0));
        assert_eq!(decoded.transactions().len(), 2);
        assert_eq!(decoded.transactions()[0].epoch(), 0);
        assert_eq!(decoded.transactions()[0].sequence(), 0);
        assert_eq!(
            decoded.transactions()[0].first_owned_page_position(),
            Some(0)
        );
        assert_eq!(decoded.transactions()[0].last_owned_page_position(), None);
        assert_eq!(decoded.transactions()[0].owned_page_record_count(), 0);
        assert_eq!(
            decoded.transactions()[0].state(),
            DurableTransactionRestartCheckpointBaselineStateObservation::Committed {
                commit_position: 0
            }
        );
        assert_eq!(decoded.transactions()[1].epoch(), 7);
        assert_eq!(decoded.transactions()[1].sequence(), 3);
        assert_eq!(decoded.transactions()[1].first_owned_page_position(), None);
        assert_eq!(
            decoded.transactions()[1].last_owned_page_position(),
            Some(0)
        );
        assert_eq!(
            decoded.transactions()[1].owned_page_record_count(),
            u64::MAX
        );
        assert_eq!(
            decoded.transactions()[1].state(),
            DurableTransactionRestartCheckpointBaselineStateObservation::Uncommitted
        );

        let encoded_none = encode_fields(0, None, [].into_iter())?;
        let decoded_none = decode_restart_checkpoint_baseline(&encoded_none)?;
        assert_eq!(decoded_none.durable_frontier(), None);
        assert_ne!(decoded_none.durable_frontier(), decoded.durable_frontier());
        Ok(())
    }

    #[test]
    fn every_short_prefix_is_truncated_and_one_trailing_byte_is_rejected()
    -> Result<(), Box<dyn Error>> {
        let encoded = one_entry_blob()?;
        for truncated_length in 0..encoded.len() {
            let error = decode_restart_checkpoint_baseline(&encoded[..truncated_length])
                .err()
                .ok_or_else(|| io::Error::other("truncated checkpoint decoded"))?;
            assert!(
                matches!(
                    error,
                    RestartCheckpointBaselineDecodeError::Truncated { .. }
                ),
                "length {truncated_length} returned {error:?}"
            );
        }

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            decode_restart_checkpoint_baseline(&trailing),
            Err(RestartCheckpointBaselineDecodeError::TrailingBytes {
                expected_length: encoded.len(),
                actual_length: encoded.len() + 1,
            })
        );
        Ok(())
    }

    #[test]
    fn outer_framing_and_checksum_corruption_fail_distinctly() -> Result<(), Box<dyn Error>> {
        let encoded = one_entry_blob()?;

        let mut header_magic = encoded.clone();
        header_magic[0] ^= 1;
        let mut actual_header_magic = HEADER_MAGIC;
        actual_header_magic[0] ^= 1;
        assert_eq!(
            decode_restart_checkpoint_baseline(&header_magic),
            Err(RestartCheckpointBaselineDecodeError::HeaderMagicMismatch {
                actual: actual_header_magic
            })
        );

        let mut version = encoded.clone();
        crate::write_u16(&mut version, 8, FORMAT_VERSION + 1);
        assert_eq!(
            decode_restart_checkpoint_baseline(&version),
            Err(RestartCheckpointBaselineDecodeError::UnsupportedVersion {
                actual: FORMAT_VERSION + 1
            })
        );

        let mut header_length = encoded.clone();
        crate::write_u16(&mut header_length, 10, HEADER_LENGTH_U16 + 1);
        assert_eq!(
            decode_restart_checkpoint_baseline(&header_length),
            Err(RestartCheckpointBaselineDecodeError::HeaderLengthMismatch {
                actual: HEADER_LENGTH_U16 + 1
            })
        );

        let mut entry_length = encoded.clone();
        crate::write_u16(&mut entry_length, 12, ENTRY_LENGTH_U16 + 1);
        assert_eq!(
            decode_restart_checkpoint_baseline(&entry_length),
            Err(RestartCheckpointBaselineDecodeError::EntryLengthMismatch {
                actual: ENTRY_LENGTH_U16 + 1
            })
        );

        let mut footer_length = encoded.clone();
        crate::write_u16(&mut footer_length, 14, FOOTER_LENGTH_U16 + 1);
        assert_eq!(
            decode_restart_checkpoint_baseline(&footer_length),
            Err(RestartCheckpointBaselineDecodeError::FooterLengthMismatch {
                actual: FOOTER_LENGTH_U16 + 1
            })
        );

        let mut declared_length = encoded.clone();
        let encoded_length_u64 = u64::try_from(encoded.len())?;
        crate::write_u64(
            &mut declared_length,
            TOTAL_LENGTH_OFFSET,
            encoded_length_u64 + 1,
        );
        assert_eq!(
            decode_restart_checkpoint_baseline(&declared_length),
            Err(
                RestartCheckpointBaselineDecodeError::DeclaredLengthMismatch {
                    declared: encoded_length_u64 + 1,
                    expected: encoded_length_u64
                }
            )
        );

        let mut footer_magic = encoded.clone();
        let footer_offset = footer_magic.len() - FOOTER_LENGTH;
        footer_magic[footer_offset] ^= 1;
        let mut actual_footer_magic = FOOTER_MAGIC;
        actual_footer_magic[0] ^= 1;
        assert_eq!(
            decode_restart_checkpoint_baseline(&footer_magic),
            Err(RestartCheckpointBaselineDecodeError::FooterMagicMismatch {
                actual: actual_footer_magic
            })
        );

        let mut checksum = encoded;
        checksum[HEADER_LENGTH + ENTRY_EPOCH_OFFSET] ^= 1;
        let actual_checksum = crate::read_u64(&checksum, checksum.len() - 8);
        let expected_checksum = crate::checksum_v1(&checksum[..checksum.len() - 8]);
        assert_eq!(
            decode_restart_checkpoint_baseline(&checksum),
            Err(RestartCheckpointBaselineDecodeError::ChecksumMismatch {
                expected: expected_checksum,
                actual: actual_checksum,
            })
        );
        Ok(())
    }

    #[test]
    fn noncanonical_header_and_entry_fields_fail_after_valid_checksum() -> Result<(), Box<dyn Error>>
    {
        let encoded = one_entry_blob()?;

        let mut frontier_presence = encoded.clone();
        frontier_presence[DURABLE_FRONTIER_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut frontier_presence);
        assert_eq!(
            decode_restart_checkpoint_baseline(&frontier_presence),
            Err(RestartCheckpointBaselineDecodeError::FrontierPresenceInvalid { actual: 2 })
        );

        let mut absent_frontier = encoded.clone();
        absent_frontier[DURABLE_FRONTIER_PRESENCE_OFFSET] = ABSENT;
        crate::write_u64(&mut absent_frontier, DURABLE_FRONTIER_OFFSET, 1);
        replace_checksum(&mut absent_frontier);
        assert_eq!(
            decode_restart_checkpoint_baseline(&absent_frontier),
            Err(RestartCheckpointBaselineDecodeError::AbsentFrontierValueNonZero { actual: 1 })
        );

        let mut header_reserved = encoded.clone();
        header_reserved[HEADER_RESERVED_START] = 1;
        replace_checksum(&mut header_reserved);
        assert_eq!(
            decode_restart_checkpoint_baseline(&header_reserved),
            Err(RestartCheckpointBaselineDecodeError::ReservedByteNonZero {
                offset: HEADER_RESERVED_START,
                actual: 1,
            })
        );

        let entry_offset = HEADER_LENGTH;
        let mut state = encoded.clone();
        state[entry_offset + ENTRY_STATE_OFFSET] = 2;
        replace_checksum(&mut state);
        assert_eq!(
            decode_restart_checkpoint_baseline(&state),
            Err(RestartCheckpointBaselineDecodeError::EntryStateInvalid {
                transaction_index: 0,
                actual: 2,
            })
        );

        let mut first_presence = encoded.clone();
        first_presence[entry_offset + ENTRY_FIRST_POSITION_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut first_presence);
        assert_eq!(
            decode_restart_checkpoint_baseline(&first_presence),
            Err(
                RestartCheckpointBaselineDecodeError::EntryPositionPresenceInvalid {
                    transaction_index: 0,
                    field: RestartCheckpointBaselineEntryOptionalField::FirstOwnedPagePosition,
                    actual: 2,
                }
            )
        );

        let mut last_presence = encoded.clone();
        last_presence[entry_offset + ENTRY_LAST_POSITION_PRESENCE_OFFSET] = 2;
        replace_checksum(&mut last_presence);
        assert_eq!(
            decode_restart_checkpoint_baseline(&last_presence),
            Err(
                RestartCheckpointBaselineDecodeError::EntryPositionPresenceInvalid {
                    transaction_index: 0,
                    field: RestartCheckpointBaselineEntryOptionalField::LastOwnedPagePosition,
                    actual: 2,
                }
            )
        );

        let mut absent_first = encoded.clone();
        absent_first[entry_offset + ENTRY_FIRST_POSITION_PRESENCE_OFFSET] = ABSENT;
        crate::write_u64(
            &mut absent_first[entry_offset..entry_offset + ENTRY_LENGTH],
            ENTRY_FIRST_OWNED_PAGE_POSITION_OFFSET,
            1,
        );
        replace_checksum(&mut absent_first);
        assert_eq!(
            decode_restart_checkpoint_baseline(&absent_first),
            Err(
                RestartCheckpointBaselineDecodeError::AbsentEntryPositionValueNonZero {
                    transaction_index: 0,
                    field: RestartCheckpointBaselineEntryOptionalField::FirstOwnedPagePosition,
                    actual: 1,
                }
            )
        );

        let mut absent_last = encoded.clone();
        crate::write_u64(
            &mut absent_last[entry_offset..entry_offset + ENTRY_LENGTH],
            ENTRY_LAST_OWNED_PAGE_POSITION_OFFSET,
            1,
        );
        replace_checksum(&mut absent_last);
        assert_eq!(
            decode_restart_checkpoint_baseline(&absent_last),
            Err(
                RestartCheckpointBaselineDecodeError::AbsentEntryPositionValueNonZero {
                    transaction_index: 0,
                    field: RestartCheckpointBaselineEntryOptionalField::LastOwnedPagePosition,
                    actual: 1,
                }
            )
        );

        let mut uncommitted_position = encoded.clone();
        uncommitted_position[entry_offset + ENTRY_STATE_OFFSET] = STATE_UNCOMMITTED;
        crate::write_u64(
            &mut uncommitted_position[entry_offset..entry_offset + ENTRY_LENGTH],
            ENTRY_COMMIT_POSITION_OFFSET,
            1,
        );
        replace_checksum(&mut uncommitted_position);
        assert_eq!(
            decode_restart_checkpoint_baseline(&uncommitted_position),
            Err(
                RestartCheckpointBaselineDecodeError::UncommittedPositionValueNonZero {
                    transaction_index: 0,
                    actual: 1,
                }
            )
        );

        let mut entry_reserved = encoded;
        entry_reserved[entry_offset + ENTRY_RESERVED_START] = 1;
        replace_checksum(&mut entry_reserved);
        assert_eq!(
            decode_restart_checkpoint_baseline(&entry_reserved),
            Err(RestartCheckpointBaselineDecodeError::ReservedByteNonZero {
                offset: entry_offset + ENTRY_RESERVED_START,
                actual: 1,
            })
        );
        Ok(())
    }

    #[test]
    fn impossible_counts_and_capacity_errors_remain_typed() -> Result<(), Box<dyn Error>> {
        let mut encoded = one_entry_blob()?;
        crate::write_u64(&mut encoded, TRANSACTION_COUNT_OFFSET, u64::MAX);
        let count_error = decode_restart_checkpoint_baseline(&encoded)
            .err()
            .ok_or_else(|| io::Error::other("impossible transaction count decoded"))?;
        assert!(matches!(
            count_error,
            RestartCheckpointBaselineDecodeError::TransactionCountOutOfRange { .. }
                | RestartCheckpointBaselineDecodeError::EncodedLengthOverflow { .. }
        ));

        let encode_capacity = RestartCheckpointBaselineEncodeError::CapacityExhausted {
            encoded_length: 144,
        };
        assert!(Error::source(&encode_capacity).is_none());
        assert!(encode_capacity.to_string().contains("144 bytes"));
        let decode_capacity = RestartCheckpointBaselineDecodeError::CapacityExhausted {
            transaction_count: 1,
        };
        assert!(Error::source(&decode_capacity).is_none());
        assert!(
            decode_capacity
                .to_string()
                .contains("1 transaction entries")
        );
        Ok(())
    }
}
