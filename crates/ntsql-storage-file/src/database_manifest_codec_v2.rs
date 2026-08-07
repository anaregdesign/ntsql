//! Pure-memory codec for manifest format version 2.
//!
//! Version 2 is one independent fixed frame with its own magic and version
//! namespace; it shares no byte compatibility contract with version 1 beyond
//! reusing the same relative field semantics for bytes `0..128`. It performs
//! no filesystem operation and grants no database lifecycle authority. In
//! particular this module exposes pure codec functions only: current
//! filesystem open remains version-1-only and cannot select or promote a
//! decoded `Clean` manifest.

use std::{error::Error, fmt};

use ntsql_database::{
    DatabaseCleanCloseCertificate, DatabaseCleanCloseCertificateError, DatabaseCompositionIdentity,
    DatabaseCompositionIdentityError, DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole,
    DatabaseId, DatabaseLifecycleGeneration, DatabaseLifecycleGenerationTransitionError,
    DatabaseManifest, DatabaseManifestLifecycleState, DatabaseRequiredFeatures,
    DatabaseRequiredFeaturesError, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_wal::PersistentLogId;

const HEADER_MAGIC: [u8; 8] = *b"NTSQDBM2";
const FOOTER_MAGIC: [u8; 8] = *b"NTSQDBE2";
const FORMAT_VERSION: u16 = 2;
/// Exact byte length of a version-2 database manifest.
pub const DATABASE_MANIFEST_V2_LENGTH: usize = 256;
const FRAME_LENGTH_U16: u16 = 256;

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

const SOURCE_GENERATION_OFFSET: usize = 128;
const FRONTIER_PRESENCE_OFFSET: usize = 136;
const RESERVED_D_START: usize = 137;
const RESERVED_D_END: usize = 144;
const FRONTIER_OFFSET: usize = 144;
const ALLOCATED_EPOCH_OFFSET: usize = 152;
const CHECKPOINT_ANCHOR_VERSION_OFFSET: usize = 160;
const RESERVED_E_START: usize = 162;
const RESERVED_E_END: usize = 168;
const CHECKPOINT_ANCHOR_VALUE_OFFSET: usize = 168;
const TRANSACTION_COUNT_OFFSET: usize = 184;
const PAGE_COUNT_OFFSET: usize = 192;
const RESERVED_F_START: usize = 200;
const RESERVED_F_END: usize = 240;
const CERTIFICATE_AREA_START: usize = 128;
const CERTIFICATE_AREA_END: usize = 240;
const FOOTER_MAGIC_OFFSET: usize = 240;
const CHECKSUM_OFFSET: usize = 248;

const LIFECYCLE_STATE_RECOVERY_REQUIRED: u8 = 1;
const LIFECYCLE_STATE_CLEAN: u8 = 2;

/// Structural or semantic failure to decode one complete version-2 database manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseManifestV2DecodeError {
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
    /// The declared frame length is not version 2's exact fixed width.
    FrameLengthMismatch {
        /// Exact decoded frame length.
        actual: u16,
    },
    /// Version 2 does not understand any nonzero header flag.
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
        /// Checksum computed from bytes `0..248`.
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
    /// The lifecycle-state discriminant is not supported by version 2.
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
    /// `RecoveryRequired` requires the entire certificate area `128..240` to be zero.
    CertificateAreaNonZero {
        /// Absolute byte offset in the supplied frame.
        offset: usize,
        /// Exact nonzero byte.
        actual: u8,
    },
    /// The frontier-presence byte was neither `0` nor `1`.
    FrontierPresenceUnsupported {
        /// Exact decoded presence byte.
        actual: u8,
    },
    /// The frontier field was nonzero while its presence byte declared it absent.
    FrontierNotCanonicallyZero {
        /// Exact decoded frontier field.
        actual: u64,
    },
    /// The certificate source generation was zero.
    CertificateSourceGenerationZero,
    /// The decoded certificate scalar fields were invalid.
    CleanCertificate(DatabaseCleanCloseCertificateError),
    /// The certificate source generation was not the manifest's exact predecessor.
    CleanManifest(DatabaseLifecycleGenerationTransitionError),
}

impl fmt::Display for DatabaseManifestV2DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                expected_length,
                actual_length,
            } => write!(
                formatter,
                "database manifest v2 is truncated: expected {expected_length} bytes, found {actual_length}"
            ),
            Self::TrailingBytes {
                expected_length,
                actual_length,
            } => write!(
                formatter,
                "database manifest v2 has trailing bytes: expected {expected_length} bytes, found {actual_length}"
            ),
            Self::HeaderMagicMismatch { actual } => {
                write!(
                    formatter,
                    "database manifest v2 header magic is invalid: {actual:?}"
                )
            }
            Self::UnsupportedVersion { actual } => {
                write!(
                    formatter,
                    "database manifest v2 version {actual} is unsupported"
                )
            }
            Self::FrameLengthMismatch { actual } => {
                write!(
                    formatter,
                    "database manifest v2 frame length {actual} is invalid"
                )
            }
            Self::HeaderFlagsUnsupported { actual } => write!(
                formatter,
                "database manifest v2 header flags are unsupported: {actual:#010x}"
            ),
            Self::FooterMagicMismatch { actual } => {
                write!(
                    formatter,
                    "database manifest v2 footer magic is invalid: {actual:?}"
                )
            }
            Self::ChecksumMismatch { expected, actual } => write!(
                formatter,
                "database manifest v2 checksum mismatch: expected {expected:#018x}, found {actual:#018x}"
            ),
            Self::ReservedByteNonZero { offset, actual } => write!(
                formatter,
                "database manifest v2 reserved byte at offset {offset} is nonzero: {actual}"
            ),
            Self::DatabaseIdZero => formatter.write_str("database manifest v2 database ID is zero"),
            Self::LifecycleGenerationZero => {
                formatter.write_str("database manifest v2 lifecycle generation is zero")
            }
            Self::LifecycleStateUnsupported { actual } => write!(
                formatter,
                "database manifest v2 lifecycle state {actual} is unsupported"
            ),
            Self::PersistentLogIdZero => {
                formatter.write_str("database manifest v2 persistent WAL identity is zero")
            }
            Self::FileIdZero { role } => {
                write!(
                    formatter,
                    "database manifest v2 {role} file identity is zero"
                )
            }
            Self::CompositionIdentity(source) => {
                write!(
                    formatter,
                    "database manifest v2 composition identity is invalid: {source}"
                )
            }
            Self::StorageFormatVersionZero { role } => {
                write!(
                    formatter,
                    "database manifest v2 {role} format version is zero"
                )
            }
            Self::RequiredFeatures(source) => {
                write!(
                    formatter,
                    "database manifest v2 required features are invalid: {source}"
                )
            }
            Self::CertificateAreaNonZero { offset, actual } => write!(
                formatter,
                "database manifest v2 recovery-required certificate area byte at offset {offset} is nonzero: {actual}"
            ),
            Self::FrontierPresenceUnsupported { actual } => write!(
                formatter,
                "database manifest v2 frontier presence byte {actual} is unsupported"
            ),
            Self::FrontierNotCanonicallyZero { actual } => write!(
                formatter,
                "database manifest v2 absent frontier is not canonically zero: {actual}"
            ),
            Self::CertificateSourceGenerationZero => {
                formatter.write_str("database manifest v2 certificate source generation is zero")
            }
            Self::CleanCertificate(source) => {
                write!(
                    formatter,
                    "database manifest v2 clean certificate is invalid: {source}"
                )
            }
            Self::CleanManifest(source) => {
                write!(
                    formatter,
                    "database manifest v2 clean manifest is invalid: {source}"
                )
            }
        }
    }
}

impl Error for DatabaseManifestV2DecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CompositionIdentity(source) => Some(source),
            Self::RequiredFeatures(source) => Some(source),
            Self::CleanCertificate(source) => Some(source),
            Self::CleanManifest(source) => Some(source),
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
            | Self::StorageFormatVersionZero { .. }
            | Self::CertificateAreaNonZero { .. }
            | Self::FrontierPresenceUnsupported { .. }
            | Self::FrontierNotCanonicallyZero { .. }
            | Self::CertificateSourceGenerationZero => None,
        }
    }
}

/// Encodes one validated inert database manifest into the exact version-2 frame.
///
/// Encoding is allocation-free and performs no publication or filesystem I/O.
/// Unlike version 1, version 2 supports both `RecoveryRequired` and `Clean`
/// and is therefore infallible: every valid [`DatabaseManifest`] has an exact
/// version-2 representation.
#[must_use]
pub fn encode_database_manifest_v2(
    manifest: &DatabaseManifest,
) -> [u8; DATABASE_MANIFEST_V2_LENGTH] {
    let mut encoded = [0_u8; DATABASE_MANIFEST_V2_LENGTH];
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

    match manifest.lifecycle_state() {
        DatabaseManifestLifecycleState::RecoveryRequired => {
            encoded[LIFECYCLE_STATE_OFFSET] = LIFECYCLE_STATE_RECOVERY_REQUIRED;
        }
        DatabaseManifestLifecycleState::Clean(certificate) => {
            encoded[LIFECYCLE_STATE_OFFSET] = LIFECYCLE_STATE_CLEAN;
            super::write_u64(
                &mut encoded,
                SOURCE_GENERATION_OFFSET,
                certificate.source_generation().get(),
            );
            match certificate.durable_wal_frontier() {
                Some(value) => {
                    encoded[FRONTIER_PRESENCE_OFFSET] = 1;
                    super::write_u64(&mut encoded, FRONTIER_OFFSET, value);
                }
                None => {
                    encoded[FRONTIER_PRESENCE_OFFSET] = 0;
                }
            }
            super::write_u64(
                &mut encoded,
                ALLOCATED_EPOCH_OFFSET,
                certificate.allocated_transaction_epoch_high_water(),
            );
            super::write_u16(
                &mut encoded,
                CHECKPOINT_ANCHOR_VERSION_OFFSET,
                certificate.checkpoint_anchor_version(),
            );
            super::write_u128(
                &mut encoded,
                CHECKPOINT_ANCHOR_VALUE_OFFSET,
                certificate.checkpoint_anchor_value(),
            );
            super::write_u64(
                &mut encoded,
                TRANSACTION_COUNT_OFFSET,
                certificate.transaction_entry_count(),
            );
            super::write_u64(
                &mut encoded,
                PAGE_COUNT_OFFSET,
                certificate.page_entry_count(),
            );
        }
    }

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

/// Decodes and fully validates one exact version-2 database manifest frame.
///
/// The returned [`DatabaseManifest`] remains inert identity and compatibility
/// data. It cannot create a database owner, select opened storage, grant
/// recovery completion, or release live authority. This function does not
/// perform filesystem I/O and current filesystem open remains version-1-only:
/// decoding a `Clean` manifest here confers no clean-open authority by itself.
pub fn decode_database_manifest_v2(
    encoded: &[u8],
) -> Result<DatabaseManifest, DatabaseManifestV2DecodeError> {
    if encoded.len() < DATABASE_MANIFEST_V2_LENGTH {
        return Err(DatabaseManifestV2DecodeError::Truncated {
            expected_length: DATABASE_MANIFEST_V2_LENGTH,
            actual_length: encoded.len(),
        });
    }
    if encoded.len() > DATABASE_MANIFEST_V2_LENGTH {
        return Err(DatabaseManifestV2DecodeError::TrailingBytes {
            expected_length: DATABASE_MANIFEST_V2_LENGTH,
            actual_length: encoded.len(),
        });
    }

    let actual_header_magic = read_magic(encoded, 0);
    if actual_header_magic != HEADER_MAGIC {
        return Err(DatabaseManifestV2DecodeError::HeaderMagicMismatch {
            actual: actual_header_magic,
        });
    }
    let version = super::read_u16(encoded, 8);
    if version != FORMAT_VERSION {
        return Err(DatabaseManifestV2DecodeError::UnsupportedVersion { actual: version });
    }
    let frame_length = super::read_u16(encoded, 10);
    if frame_length != FRAME_LENGTH_U16 {
        return Err(DatabaseManifestV2DecodeError::FrameLengthMismatch {
            actual: frame_length,
        });
    }
    let header_flags = super::read_u32(encoded, HEADER_FLAGS_OFFSET);
    if header_flags != 0 {
        return Err(DatabaseManifestV2DecodeError::HeaderFlagsUnsupported {
            actual: header_flags,
        });
    }

    let actual_footer_magic = read_magic(encoded, FOOTER_MAGIC_OFFSET);
    if actual_footer_magic != FOOTER_MAGIC {
        return Err(DatabaseManifestV2DecodeError::FooterMagicMismatch {
            actual: actual_footer_magic,
        });
    }
    let actual_checksum = super::read_u64(encoded, CHECKSUM_OFFSET);
    let expected_checksum = super::checksum_v1(&encoded[..CHECKSUM_OFFSET]);
    if actual_checksum != expected_checksum {
        return Err(DatabaseManifestV2DecodeError::ChecksumMismatch {
            expected: expected_checksum,
            actual: actual_checksum,
        });
    }

    for range in [
        RESERVED_A_START..RESERVED_A_END,
        RESERVED_B_START..RESERVED_B_END,
    ] {
        for offset in range {
            let actual = encoded[offset];
            if actual != 0 {
                return Err(DatabaseManifestV2DecodeError::ReservedByteNonZero { offset, actual });
            }
        }
    }

    let Some(database_id) = DatabaseId::new(super::read_u128(encoded, DATABASE_ID_OFFSET)) else {
        return Err(DatabaseManifestV2DecodeError::DatabaseIdZero);
    };
    let Some(lifecycle_generation) =
        DatabaseLifecycleGeneration::new(super::read_u64(encoded, LIFECYCLE_GENERATION_OFFSET))
    else {
        return Err(DatabaseManifestV2DecodeError::LifecycleGenerationZero);
    };
    let lifecycle_state_code = encoded[LIFECYCLE_STATE_OFFSET];
    match lifecycle_state_code {
        LIFECYCLE_STATE_RECOVERY_REQUIRED | LIFECYCLE_STATE_CLEAN => {}
        actual => {
            return Err(DatabaseManifestV2DecodeError::LifecycleStateUnsupported { actual });
        }
    }
    let Some(persistent_log_id) =
        PersistentLogId::new(super::read_u128(encoded, PERSISTENT_LOG_ID_OFFSET))
    else {
        return Err(DatabaseManifestV2DecodeError::PersistentLogIdZero);
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
    .map_err(DatabaseManifestV2DecodeError::CompositionIdentity)?;

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
            .map_err(DatabaseManifestV2DecodeError::RequiredFeatures)?;

    match lifecycle_state_code {
        LIFECYCLE_STATE_RECOVERY_REQUIRED => {
            for (index, actual) in encoded[CERTIFICATE_AREA_START..CERTIFICATE_AREA_END]
                .iter()
                .enumerate()
            {
                if *actual != 0 {
                    return Err(DatabaseManifestV2DecodeError::CertificateAreaNonZero {
                        offset: CERTIFICATE_AREA_START + index,
                        actual: *actual,
                    });
                }
            }
            Ok(DatabaseManifest::recovery_required(
                composition_identity,
                storage_formats,
                required_features,
            ))
        }
        _ => {
            for range in [
                RESERVED_D_START..RESERVED_D_END,
                RESERVED_E_START..RESERVED_E_END,
                RESERVED_F_START..RESERVED_F_END,
            ] {
                for offset in range {
                    let actual = encoded[offset];
                    if actual != 0 {
                        return Err(DatabaseManifestV2DecodeError::ReservedByteNonZero {
                            offset,
                            actual,
                        });
                    }
                }
            }
            let Some(source_generation) = DatabaseLifecycleGeneration::new(super::read_u64(
                encoded,
                SOURCE_GENERATION_OFFSET,
            )) else {
                return Err(DatabaseManifestV2DecodeError::CertificateSourceGenerationZero);
            };
            let frontier_presence = encoded[FRONTIER_PRESENCE_OFFSET];
            let frontier_raw = super::read_u64(encoded, FRONTIER_OFFSET);
            let durable_wal_frontier = match frontier_presence {
                0 => {
                    if frontier_raw != 0 {
                        return Err(DatabaseManifestV2DecodeError::FrontierNotCanonicallyZero {
                            actual: frontier_raw,
                        });
                    }
                    None
                }
                1 => Some(frontier_raw),
                actual => {
                    return Err(DatabaseManifestV2DecodeError::FrontierPresenceUnsupported {
                        actual,
                    });
                }
            };
            let allocated_transaction_epoch_high_water =
                super::read_u64(encoded, ALLOCATED_EPOCH_OFFSET);
            let checkpoint_anchor_version =
                super::read_u16(encoded, CHECKPOINT_ANCHOR_VERSION_OFFSET);
            let checkpoint_anchor_value = super::read_u128(encoded, CHECKPOINT_ANCHOR_VALUE_OFFSET);
            let transaction_entry_count = super::read_u64(encoded, TRANSACTION_COUNT_OFFSET);
            let page_entry_count = super::read_u64(encoded, PAGE_COUNT_OFFSET);
            let certificate = DatabaseCleanCloseCertificate::new(
                source_generation,
                durable_wal_frontier,
                allocated_transaction_epoch_high_water,
                checkpoint_anchor_version,
                checkpoint_anchor_value,
                transaction_entry_count,
                page_entry_count,
            )
            .map_err(DatabaseManifestV2DecodeError::CleanCertificate)?;
            DatabaseManifest::clean(
                composition_identity,
                storage_formats,
                required_features,
                certificate,
            )
            .map_err(DatabaseManifestV2DecodeError::CleanManifest)
        }
    }
}

fn decode_file_id(
    encoded: &[u8],
    role: DatabaseFileRole,
    offset: usize,
) -> Result<DatabaseFileId, DatabaseManifestV2DecodeError> {
    DatabaseFileId::new(super::read_u128(encoded, offset))
        .ok_or(DatabaseManifestV2DecodeError::FileIdZero { role })
}

fn decode_storage_format_version(
    encoded: &[u8],
    role: DatabaseFileRole,
    offset: usize,
) -> Result<DatabaseStorageFormatVersion, DatabaseManifestV2DecodeError> {
    DatabaseStorageFormatVersion::new(super::read_u16(encoded, offset))
        .ok_or(DatabaseManifestV2DecodeError::StorageFormatVersionZero { role })
}

fn read_magic(encoded: &[u8], offset: usize) -> [u8; 8] {
    let mut magic = [0_u8; 8];
    magic.copy_from_slice(&encoded[offset..offset + 8]);
    magic
}
