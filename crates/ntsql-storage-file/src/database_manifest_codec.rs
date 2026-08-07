//! Pure-memory codec for the repository-owned database manifest.
//!
//! Version 1 is one fixed frame and is independent from every WAL, page-store,
//! and checkpoint format namespace. It performs no filesystem operation and
//! grants no database lifecycle authority.

use std::{error::Error, fmt};

use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseCompositionIdentityError, DatabaseFileId,
    DatabaseFileIdentity, DatabaseFileRole, DatabaseId, DatabaseLifecycleGeneration,
    DatabaseManifest, DatabaseManifestLifecycleState, DatabaseRequiredFeatures,
    DatabaseRequiredFeaturesError, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_wal::PersistentLogId;

const HEADER_MAGIC: [u8; 8] = *b"NTSQDBM1";
const FOOTER_MAGIC: [u8; 8] = *b"NTSQDBE1";
const FORMAT_VERSION: u16 = 1;
/// Exact byte length of a version-1 database manifest.
pub const DATABASE_MANIFEST_V1_LENGTH: usize = 160;
const FRAME_LENGTH_U16: u16 = 160;

const HEADER_FLAGS_OFFSET: usize = 12;
const DATABASE_ID_OFFSET: usize = 16;
const LIFECYCLE_GENERATION_OFFSET: usize = 32;
const LIFECYCLE_STATE_OFFSET: usize = 40;
const RESERVED_A_START: usize = 41;
const RESERVED_A_END: usize = 48;
const PERSISTENT_LOG_ID_OFFSET: usize = 48;
const WAL_FILE_ID_OFFSET: usize = 64;
const PAGE_STORE_FILE_ID_OFFSET: usize = 80;
const RESTART_CHECKPOINT_FILE_ID_OFFSET: usize = 96;
const WAL_FORMAT_VERSION_OFFSET: usize = 112;
const PAGE_STORE_FORMAT_VERSION_OFFSET: usize = 114;
const RESTART_CHECKPOINT_FORMAT_VERSION_OFFSET: usize = 116;
const RESERVED_B_START: usize = 118;
const RESERVED_B_END: usize = 120;
const REQUIRED_FEATURES_OFFSET: usize = 120;
const RESERVED_C_START: usize = 128;
const RESERVED_C_END: usize = 144;
const FOOTER_MAGIC_OFFSET: usize = 144;
const CHECKSUM_OFFSET: usize = 152;

const LIFECYCLE_STATE_RECOVERY_REQUIRED: u8 = 1;

/// Structural or semantic failure to decode one complete database manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseManifestDecodeError {
    /// The supplied bytes end before the complete fixed frame.
    Truncated {
        /// Exact required frame length.
        expected_length: usize,
        /// Exact supplied byte length.
        actual_length: usize,
    },
    /// Bytes follow the one complete fixed frame.
    TrailingBytes {
        /// Exact required frame length.
        expected_length: usize,
        /// Exact supplied byte length.
        actual_length: usize,
    },
    /// The independent manifest header magic did not match.
    HeaderMagicMismatch {
        /// Exact eight bytes found at the header magic offset.
        actual: [u8; 8],
    },
    /// The database manifest format version is not supported.
    UnsupportedVersion {
        /// Exact decoded version.
        actual: u16,
    },
    /// The declared frame length is not version 1's exact fixed width.
    FrameLengthMismatch {
        /// Exact decoded frame length.
        actual: u16,
    },
    /// Version 1 does not understand any nonzero header flag.
    HeaderFlagsUnsupported {
        /// Exact decoded flag set.
        actual: u32,
    },
    /// The independent manifest footer magic did not match.
    FooterMagicMismatch {
        /// Exact eight bytes found at the footer magic offset.
        actual: [u8; 8],
    },
    /// The checksum over every preceding frame byte did not match.
    ChecksumMismatch {
        /// Checksum computed from bytes `0..152`.
        expected: u64,
        /// Checksum decoded from the final field.
        actual: u64,
    },
    /// A reserved byte was nonzero.
    ReservedByteNonZero {
        /// Absolute byte offset in the supplied frame.
        offset: usize,
        /// Exact nonzero byte.
        actual: u8,
    },
    /// The repository-owned database identity was zero.
    DatabaseIdZero,
    /// The lifecycle generation was zero.
    LifecycleGenerationZero,
    /// The lifecycle-state discriminant is not supported by version 1.
    LifecycleStateUnsupported {
        /// Exact decoded state value.
        actual: u8,
    },
    /// The persistent WAL lineage identity was zero.
    PersistentLogIdZero,
    /// One required file-role identity was zero.
    FileIdZero {
        /// Role whose identity was zero.
        role: DatabaseFileRole,
    },
    /// The complete file-role identity set was invalid.
    CompositionIdentity(DatabaseCompositionIdentityError),
    /// One required child-format version was zero.
    StorageFormatVersionZero {
        /// Role whose required version was zero.
        role: DatabaseFileRole,
    },
    /// Required feature bits are not understood by this repository version.
    RequiredFeatures(DatabaseRequiredFeaturesError),
}

impl fmt::Display for DatabaseManifestDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                expected_length,
                actual_length,
            } => write!(
                formatter,
                "database manifest is truncated: expected {expected_length} bytes, found {actual_length}"
            ),
            Self::TrailingBytes {
                expected_length,
                actual_length,
            } => write!(
                formatter,
                "database manifest has trailing bytes: expected {expected_length} bytes, found {actual_length}"
            ),
            Self::HeaderMagicMismatch { actual } => {
                write!(
                    formatter,
                    "database manifest header magic is invalid: {actual:?}"
                )
            }
            Self::UnsupportedVersion { actual } => {
                write!(
                    formatter,
                    "database manifest version {actual} is unsupported"
                )
            }
            Self::FrameLengthMismatch { actual } => {
                write!(
                    formatter,
                    "database manifest frame length {actual} is invalid"
                )
            }
            Self::HeaderFlagsUnsupported { actual } => write!(
                formatter,
                "database manifest header flags are unsupported: {actual:#010x}"
            ),
            Self::FooterMagicMismatch { actual } => {
                write!(
                    formatter,
                    "database manifest footer magic is invalid: {actual:?}"
                )
            }
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "database manifest checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
            Self::ReservedByteNonZero { offset, actual } => write!(
                formatter,
                "database manifest reserved byte at offset {offset} is nonzero: {actual}"
            ),
            Self::DatabaseIdZero => formatter.write_str("database manifest database ID is zero"),
            Self::LifecycleGenerationZero => {
                formatter.write_str("database manifest lifecycle generation is zero")
            }
            Self::LifecycleStateUnsupported { actual } => write!(
                formatter,
                "database manifest lifecycle state {actual} is unsupported"
            ),
            Self::PersistentLogIdZero => {
                formatter.write_str("database manifest persistent WAL identity is zero")
            }
            Self::FileIdZero { role } => {
                write!(formatter, "database manifest {role} file identity is zero")
            }
            Self::CompositionIdentity(source) => {
                write!(
                    formatter,
                    "database manifest composition identity is invalid: {source}"
                )
            }
            Self::StorageFormatVersionZero { role } => {
                write!(formatter, "database manifest {role} format version is zero")
            }
            Self::RequiredFeatures(source) => {
                write!(
                    formatter,
                    "database manifest required features are invalid: {source}"
                )
            }
        }
    }
}

impl Error for DatabaseManifestDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CompositionIdentity(source) => Some(source),
            Self::RequiredFeatures(source) => Some(source),
            Self::Truncated { .. }
            | Self::TrailingBytes { .. }
            | Self::HeaderMagicMismatch { .. }
            | Self::UnsupportedVersion { .. }
            | Self::FrameLengthMismatch { .. }
            | Self::HeaderFlagsUnsupported { .. }
            | Self::FooterMagicMismatch { .. }
            | Self::ChecksumMismatch { .. }
            | Self::ReservedByteNonZero { .. }
            | Self::DatabaseIdZero
            | Self::LifecycleGenerationZero
            | Self::LifecycleStateUnsupported { .. }
            | Self::PersistentLogIdZero
            | Self::FileIdZero { .. }
            | Self::StorageFormatVersionZero { .. } => None,
        }
    }
}

/// Encodes one validated inert database manifest into the exact version-1 frame.
///
/// Encoding is allocation-free and performs no publication or filesystem I/O.
#[must_use]
pub fn encode_database_manifest(manifest: &DatabaseManifest) -> [u8; DATABASE_MANIFEST_V1_LENGTH] {
    let mut encoded = [0_u8; DATABASE_MANIFEST_V1_LENGTH];
    encoded[..8].copy_from_slice(&HEADER_MAGIC);
    super::write_u16(&mut encoded, 8, FORMAT_VERSION);
    super::write_u16(&mut encoded, 10, FRAME_LENGTH_U16);

    let composition = manifest.composition_identity();
    super::write_u128(
        &mut encoded,
        DATABASE_ID_OFFSET,
        composition.database_id().get(),
    );
    super::write_u64(
        &mut encoded,
        LIFECYCLE_GENERATION_OFFSET,
        composition.lifecycle_generation().get(),
    );
    encoded[LIFECYCLE_STATE_OFFSET] = match manifest.lifecycle_state() {
        DatabaseManifestLifecycleState::RecoveryRequired => LIFECYCLE_STATE_RECOVERY_REQUIRED,
    };
    super::write_u128(
        &mut encoded,
        PERSISTENT_LOG_ID_OFFSET,
        composition.persistent_log_id().get(),
    );
    super::write_u128(
        &mut encoded,
        WAL_FILE_ID_OFFSET,
        composition.file_id(DatabaseFileRole::Wal).get(),
    );
    super::write_u128(
        &mut encoded,
        PAGE_STORE_FILE_ID_OFFSET,
        composition.file_id(DatabaseFileRole::PageStore).get(),
    );
    super::write_u128(
        &mut encoded,
        RESTART_CHECKPOINT_FILE_ID_OFFSET,
        composition
            .file_id(DatabaseFileRole::RestartCheckpoint)
            .get(),
    );

    let formats = manifest.storage_formats();
    super::write_u16(
        &mut encoded,
        WAL_FORMAT_VERSION_OFFSET,
        formats.version(DatabaseFileRole::Wal).get(),
    );
    super::write_u16(
        &mut encoded,
        PAGE_STORE_FORMAT_VERSION_OFFSET,
        formats.version(DatabaseFileRole::PageStore).get(),
    );
    super::write_u16(
        &mut encoded,
        RESTART_CHECKPOINT_FORMAT_VERSION_OFFSET,
        formats.version(DatabaseFileRole::RestartCheckpoint).get(),
    );
    super::write_u64(
        &mut encoded,
        REQUIRED_FEATURES_OFFSET,
        manifest.required_features().bits(),
    );
    encoded[FOOTER_MAGIC_OFFSET..CHECKSUM_OFFSET].copy_from_slice(&FOOTER_MAGIC);
    let checksum = super::checksum_v1(&encoded[..CHECKSUM_OFFSET]);
    super::write_u64(&mut encoded, CHECKSUM_OFFSET, checksum);
    encoded
}

/// Decodes and fully validates one exact version-1 database manifest frame.
///
/// The returned [`DatabaseManifest`] remains inert identity and compatibility
/// data. It cannot create a database owner, select opened storage, grant
/// recovery completion, or release live authority.
pub fn decode_database_manifest(
    encoded: &[u8],
) -> Result<DatabaseManifest, DatabaseManifestDecodeError> {
    if encoded.len() < DATABASE_MANIFEST_V1_LENGTH {
        return Err(DatabaseManifestDecodeError::Truncated {
            expected_length: DATABASE_MANIFEST_V1_LENGTH,
            actual_length: encoded.len(),
        });
    }
    if encoded.len() > DATABASE_MANIFEST_V1_LENGTH {
        return Err(DatabaseManifestDecodeError::TrailingBytes {
            expected_length: DATABASE_MANIFEST_V1_LENGTH,
            actual_length: encoded.len(),
        });
    }

    let actual_header_magic = read_magic(encoded, 0);
    if actual_header_magic != HEADER_MAGIC {
        return Err(DatabaseManifestDecodeError::HeaderMagicMismatch {
            actual: actual_header_magic,
        });
    }
    let version = super::read_u16(encoded, 8);
    if version != FORMAT_VERSION {
        return Err(DatabaseManifestDecodeError::UnsupportedVersion { actual: version });
    }
    let frame_length = super::read_u16(encoded, 10);
    if frame_length != FRAME_LENGTH_U16 {
        return Err(DatabaseManifestDecodeError::FrameLengthMismatch {
            actual: frame_length,
        });
    }
    let header_flags = super::read_u32(encoded, HEADER_FLAGS_OFFSET);
    if header_flags != 0 {
        return Err(DatabaseManifestDecodeError::HeaderFlagsUnsupported {
            actual: header_flags,
        });
    }

    let actual_footer_magic = read_magic(encoded, FOOTER_MAGIC_OFFSET);
    if actual_footer_magic != FOOTER_MAGIC {
        return Err(DatabaseManifestDecodeError::FooterMagicMismatch {
            actual: actual_footer_magic,
        });
    }
    let actual_checksum = super::read_u64(encoded, CHECKSUM_OFFSET);
    let expected_checksum = super::checksum_v1(&encoded[..CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(DatabaseManifestDecodeError::ChecksumMismatch {
            expected: expected_checksum,
            actual: actual_checksum,
        });
    }

    for range in [
        RESERVED_A_START..RESERVED_A_END,
        RESERVED_B_START..RESERVED_B_END,
        RESERVED_C_START..RESERVED_C_END,
    ] {
        for offset in range {
            let actual = encoded[offset];
            if actual != 0 {
                return Err(DatabaseManifestDecodeError::ReservedByteNonZero { offset, actual });
            }
        }
    }

    let Some(database_id) = DatabaseId::new(super::read_u128(encoded, DATABASE_ID_OFFSET)) else {
        return Err(DatabaseManifestDecodeError::DatabaseIdZero);
    };
    let Some(lifecycle_generation) =
        DatabaseLifecycleGeneration::new(super::read_u64(encoded, LIFECYCLE_GENERATION_OFFSET))
    else {
        return Err(DatabaseManifestDecodeError::LifecycleGenerationZero);
    };
    match encoded[LIFECYCLE_STATE_OFFSET] {
        LIFECYCLE_STATE_RECOVERY_REQUIRED => {}
        actual => {
            return Err(DatabaseManifestDecodeError::LifecycleStateUnsupported { actual });
        }
    }
    let Some(persistent_log_id) =
        PersistentLogId::new(super::read_u128(encoded, PERSISTENT_LOG_ID_OFFSET))
    else {
        return Err(DatabaseManifestDecodeError::PersistentLogIdZero);
    };

    let wal_file_id = decode_file_id(encoded, DatabaseFileRole::Wal, WAL_FILE_ID_OFFSET)?;
    let page_store_file_id = decode_file_id(
        encoded,
        DatabaseFileRole::PageStore,
        PAGE_STORE_FILE_ID_OFFSET,
    )?;
    let restart_checkpoint_file_id = decode_file_id(
        encoded,
        DatabaseFileRole::RestartCheckpoint,
        RESTART_CHECKPOINT_FILE_ID_OFFSET,
    )?;
    let files = [
        DatabaseFileIdentity::new(DatabaseFileRole::Wal, wal_file_id),
        DatabaseFileIdentity::new(DatabaseFileRole::PageStore, page_store_file_id),
        DatabaseFileIdentity::new(
            DatabaseFileRole::RestartCheckpoint,
            restart_checkpoint_file_id,
        ),
    ];
    let composition_identity = DatabaseCompositionIdentity::new(
        database_id,
        lifecycle_generation,
        persistent_log_id,
        &files,
    )
    .map_err(DatabaseManifestDecodeError::CompositionIdentity)?;

    let storage_formats = DatabaseStorageFormatRequirements::new(
        decode_storage_format_version(encoded, DatabaseFileRole::Wal, WAL_FORMAT_VERSION_OFFSET)?,
        decode_storage_format_version(
            encoded,
            DatabaseFileRole::PageStore,
            PAGE_STORE_FORMAT_VERSION_OFFSET,
        )?,
        decode_storage_format_version(
            encoded,
            DatabaseFileRole::RestartCheckpoint,
            RESTART_CHECKPOINT_FORMAT_VERSION_OFFSET,
        )?,
    );
    let required_features =
        DatabaseRequiredFeatures::from_bits(super::read_u64(encoded, REQUIRED_FEATURES_OFFSET))
            .map_err(DatabaseManifestDecodeError::RequiredFeatures)?;

    Ok(DatabaseManifest::recovery_required(
        composition_identity,
        storage_formats,
        required_features,
    ))
}

fn decode_file_id(
    encoded: &[u8],
    role: DatabaseFileRole,
    offset: usize,
) -> Result<DatabaseFileId, DatabaseManifestDecodeError> {
    DatabaseFileId::new(super::read_u128(encoded, offset))
        .ok_or(DatabaseManifestDecodeError::FileIdZero { role })
}

fn decode_storage_format_version(
    encoded: &[u8],
    role: DatabaseFileRole,
    offset: usize,
) -> Result<DatabaseStorageFormatVersion, DatabaseManifestDecodeError> {
    DatabaseStorageFormatVersion::new(super::read_u16(encoded, offset))
        .ok_or(DatabaseManifestDecodeError::StorageFormatVersionZero { role })
}

fn read_magic(encoded: &[u8], offset: usize) -> [u8; 8] {
    let mut magic = [0_u8; 8];
    magic.copy_from_slice(&encoded[offset..offset + 8]);
    magic
}
