use std::{error::Error, io, ops::Range};

use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseCompositionIdentityError, DatabaseFileId,
    DatabaseFileIdentity, DatabaseFileRole, DatabaseId, DatabaseLifecycleGeneration,
    DatabaseLifecycleGenerationTransitionError, DatabaseManifest, DatabaseManifestSuccessorError,
    DatabaseRequiredFeatures, DatabaseRequiredFeaturesError, DatabaseStorageFormatRequirements,
    DatabaseStorageFormatVersion,
};
use ntsql_storage_file::{
    DATABASE_MANIFEST_V1_LENGTH, DatabaseManifestDecodeError, decode_database_manifest,
    encode_database_manifest,
};
use ntsql_wal::PersistentLogId;

const DATABASE_ID_OFFSET: usize = 16;
const LIFECYCLE_GENERATION_OFFSET: usize = 32;
const LIFECYCLE_STATE_OFFSET: usize = 40;
const PERSISTENT_LOG_ID_OFFSET: usize = 48;
const WAL_FILE_ID_OFFSET: usize = 64;
const PAGE_STORE_FILE_ID_OFFSET: usize = 80;
const RESTART_CHECKPOINT_FILE_ID_OFFSET: usize = 96;
const WAL_FORMAT_VERSION_OFFSET: usize = 112;
const PAGE_STORE_FORMAT_VERSION_OFFSET: usize = 114;
const RESTART_CHECKPOINT_FORMAT_VERSION_OFFSET: usize = 116;
const REQUIRED_FEATURES_OFFSET: usize = 120;
const FOOTER_MAGIC_OFFSET: usize = 144;
const CHECKSUM_OFFSET: usize = 152;

const CHECKSUM_SEED: u64 = 0x4e54_5351_4c43_4b31;
const CHECKSUM_MULTIPLIER: u64 = 0x4e54_5351_4c57_414d;
const CHECKSUM_MIX: u64 = 0x4348_4543_4b53_554d;

#[test]
fn recovery_required_manifest_has_exact_version_one_golden_bytes() -> Result<(), Box<dyn Error>> {
    let manifest = golden_manifest()?;
    let encoded = encode_database_manifest(&manifest);
    let expected = [
        0x4e, 0x54, 0x53, 0x51, 0x44, 0x42, 0x4d, 0x31, 0x00, 0x01, 0x00, 0xa0, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c,
        0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b,
        0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a,
        0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
        0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x00, 0x04, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4e, 0x54, 0x53, 0x51, 0x44, 0x42,
        0x45, 0x31, 0x47, 0x88, 0xff, 0x3d, 0x69, 0x33, 0x1a, 0x6b,
    ];

    assert_eq!(encoded, expected);
    assert_eq!(encode_database_manifest(&manifest), expected);
    assert_eq!(decode_database_manifest(&expected)?, manifest);
    Ok(())
}

#[test]
fn every_prefix_is_truncated_and_one_extra_byte_is_trailing() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest(&golden_manifest()?);
    for actual_length in 0..DATABASE_MANIFEST_V1_LENGTH {
        assert_eq!(
            decode_database_manifest(&encoded[..actual_length]),
            Err(DatabaseManifestDecodeError::Truncated {
                expected_length: DATABASE_MANIFEST_V1_LENGTH,
                actual_length,
            })
        );
    }

    let mut trailing = encoded.to_vec();
    trailing.push(0);
    assert_eq!(
        decode_database_manifest(&trailing),
        Err(DatabaseManifestDecodeError::TrailingBytes {
            expected_length: DATABASE_MANIFEST_V1_LENGTH,
            actual_length: DATABASE_MANIFEST_V1_LENGTH + 1,
        })
    );
    Ok(())
}

#[test]
fn envelope_fields_and_checksum_fail_distinctly() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest(&golden_manifest()?);

    let mut wrong_magic = encoded;
    wrong_magic[0] = 0;
    assert_eq!(
        decode_database_manifest(&wrong_magic),
        Err(DatabaseManifestDecodeError::HeaderMagicMismatch {
            actual: [0, 0x54, 0x53, 0x51, 0x44, 0x42, 0x4d, 0x31],
        })
    );

    let mut wrong_version = encoded;
    write_u16(&mut wrong_version, 8, 2);
    assert_eq!(
        decode_database_manifest(&wrong_version),
        Err(DatabaseManifestDecodeError::UnsupportedVersion { actual: 2 })
    );

    let mut wrong_length = encoded;
    write_u16(&mut wrong_length, 10, 159);
    assert_eq!(
        decode_database_manifest(&wrong_length),
        Err(DatabaseManifestDecodeError::FrameLengthMismatch { actual: 159 })
    );

    for actual in [1, 0x8000_0000] {
        let mut unknown_header_flags = encoded;
        write_u32(&mut unknown_header_flags, 12, actual);
        assert_eq!(
            decode_database_manifest(&unknown_header_flags),
            Err(DatabaseManifestDecodeError::HeaderFlagsUnsupported { actual })
        );
    }

    let mut wrong_footer = encoded;
    wrong_footer[FOOTER_MAGIC_OFFSET] = 0;
    assert_eq!(
        decode_database_manifest(&wrong_footer),
        Err(DatabaseManifestDecodeError::FooterMagicMismatch {
            actual: [0, 0x54, 0x53, 0x51, 0x44, 0x42, 0x45, 0x31],
        })
    );

    let mut wrong_checksum = encoded;
    wrong_checksum[CHECKSUM_OFFSET] ^= 0xff;
    assert_eq!(
        decode_database_manifest(&wrong_checksum),
        Err(DatabaseManifestDecodeError::ChecksumMismatch {
            expected: checksum_v1(&wrong_checksum[..CHECKSUM_OFFSET]),
            actual: read_u64(&wrong_checksum, CHECKSUM_OFFSET),
        })
    );
    Ok(())
}

#[test]
fn every_reserved_byte_is_rejected_after_checksum_validation() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest(&golden_manifest()?);
    for range in [41..48, 118..120, 128..144] {
        assert_reserved_range(&encoded, range)?;
    }
    Ok(())
}

#[test]
fn zero_and_unsupported_scalar_fields_fail_closed() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest(&golden_manifest()?);

    let mut zero_database = encoded;
    zero_database[DATABASE_ID_OFFSET..DATABASE_ID_OFFSET + 16].fill(0);
    replace_checksum(&mut zero_database);
    assert_eq!(
        decode_database_manifest(&zero_database),
        Err(DatabaseManifestDecodeError::DatabaseIdZero)
    );

    let mut zero_generation = encoded;
    zero_generation[LIFECYCLE_GENERATION_OFFSET..LIFECYCLE_GENERATION_OFFSET + 8].fill(0);
    replace_checksum(&mut zero_generation);
    assert_eq!(
        decode_database_manifest(&zero_generation),
        Err(DatabaseManifestDecodeError::LifecycleGenerationZero)
    );

    for actual in [0, 2, u8::MAX] {
        let mut unsupported_state = encoded;
        unsupported_state[LIFECYCLE_STATE_OFFSET] = actual;
        replace_checksum(&mut unsupported_state);
        assert_eq!(
            decode_database_manifest(&unsupported_state),
            Err(DatabaseManifestDecodeError::LifecycleStateUnsupported { actual })
        );
    }

    let mut zero_log = encoded;
    zero_log[PERSISTENT_LOG_ID_OFFSET..PERSISTENT_LOG_ID_OFFSET + 16].fill(0);
    replace_checksum(&mut zero_log);
    assert_eq!(
        decode_database_manifest(&zero_log),
        Err(DatabaseManifestDecodeError::PersistentLogIdZero)
    );
    Ok(())
}

#[test]
fn each_file_role_rejects_zero_and_cross_role_identity_reuse() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest(&golden_manifest()?);
    for (role, offset) in [
        (DatabaseFileRole::Wal, WAL_FILE_ID_OFFSET),
        (DatabaseFileRole::PageStore, PAGE_STORE_FILE_ID_OFFSET),
        (
            DatabaseFileRole::RestartCheckpoint,
            RESTART_CHECKPOINT_FILE_ID_OFFSET,
        ),
    ] {
        let mut zero_file = encoded;
        zero_file[offset..offset + 16].fill(0);
        replace_checksum(&mut zero_file);
        assert_eq!(
            decode_database_manifest(&zero_file),
            Err(DatabaseManifestDecodeError::FileIdZero { role })
        );
    }

    for (first_role, first_offset, second_role, second_offset) in [
        (
            DatabaseFileRole::Wal,
            WAL_FILE_ID_OFFSET,
            DatabaseFileRole::PageStore,
            PAGE_STORE_FILE_ID_OFFSET,
        ),
        (
            DatabaseFileRole::Wal,
            WAL_FILE_ID_OFFSET,
            DatabaseFileRole::RestartCheckpoint,
            RESTART_CHECKPOINT_FILE_ID_OFFSET,
        ),
        (
            DatabaseFileRole::PageStore,
            PAGE_STORE_FILE_ID_OFFSET,
            DatabaseFileRole::RestartCheckpoint,
            RESTART_CHECKPOINT_FILE_ID_OFFSET,
        ),
    ] {
        let mut duplicate = encoded;
        duplicate.copy_within(first_offset..first_offset + 16, second_offset);
        replace_checksum(&mut duplicate);
        assert_eq!(
            decode_database_manifest(&duplicate),
            Err(DatabaseManifestDecodeError::CompositionIdentity(
                DatabaseCompositionIdentityError::DuplicateFileIdentity {
                    file_id: file_id(read_u128(&duplicate, first_offset))?,
                    first_role,
                    second_role,
                }
            ))
        );
    }
    Ok(())
}

#[test]
fn format_versions_and_required_features_are_validated() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest(&golden_manifest()?);
    for (role, offset) in [
        (DatabaseFileRole::Wal, WAL_FORMAT_VERSION_OFFSET),
        (
            DatabaseFileRole::PageStore,
            PAGE_STORE_FORMAT_VERSION_OFFSET,
        ),
        (
            DatabaseFileRole::RestartCheckpoint,
            RESTART_CHECKPOINT_FORMAT_VERSION_OFFSET,
        ),
    ] {
        let mut zero_version = encoded;
        write_u16(&mut zero_version, offset, 0);
        replace_checksum(&mut zero_version);
        assert_eq!(
            decode_database_manifest(&zero_version),
            Err(DatabaseManifestDecodeError::StorageFormatVersionZero { role })
        );
    }

    for actual in [1_u64, 0x8000_0000_0000_0000] {
        let mut unknown_features = encoded;
        write_u64(&mut unknown_features, REQUIRED_FEATURES_OFFSET, actual);
        replace_checksum(&mut unknown_features);
        assert_eq!(
            decode_database_manifest(&unknown_features),
            Err(DatabaseManifestDecodeError::RequiredFeatures(
                DatabaseRequiredFeaturesError {
                    actual,
                    unknown: actual,
                }
            ))
        );
    }
    Ok(())
}

#[test]
fn maximum_nonzero_fields_round_trip_without_host_width_dependence() -> Result<(), Box<dyn Error>> {
    let manifest = manifest(TestManifestFields {
        database_id: u128::MAX,
        lifecycle_generation: u64::MAX,
        wal_file_id: u128::MAX - 2,
        page_store_file_id: u128::MAX - 3,
        restart_checkpoint_file_id: u128::MAX - 4,
        persistent_log_id: u128::MAX - 1,
        wal_format_version: u16::MAX,
        page_store_format_version: u16::MAX,
        restart_checkpoint_format_version: u16::MAX,
    })?;
    let encoded = encode_database_manifest(&manifest);
    assert_eq!(decode_database_manifest(&encoded)?, manifest);
    Ok(())
}

#[test]
fn decoded_generation_regression_is_rejected_against_exact_previous_manifest()
-> Result<(), Box<dyn Error>> {
    let previous = manifest(TestManifestFields {
        database_id: 1,
        lifecycle_generation: 2,
        wal_file_id: 3,
        page_store_file_id: 4,
        restart_checkpoint_file_id: 5,
        persistent_log_id: 6,
        wal_format_version: 4,
        page_store_format_version: 1,
        restart_checkpoint_format_version: 1,
    })?;
    let regressed = manifest(TestManifestFields {
        lifecycle_generation: 1,
        ..TestManifestFields::from_manifest(previous)
    })?;
    let decoded = decode_database_manifest(&encode_database_manifest(&regressed))?;
    assert_eq!(
        decoded.require_successor_of(previous),
        Err(DatabaseManifestSuccessorError::LifecycleGeneration(
            DatabaseLifecycleGenerationTransitionError::NotStrictlyIncreasing {
                current: generation(2)?,
                proposed: generation(1)?,
            }
        ))
    );
    Ok(())
}

fn golden_manifest() -> Result<DatabaseManifest, Box<dyn Error>> {
    manifest(TestManifestFields {
        database_id: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
        lifecycle_generation: 0x1112_1314_1516_1718,
        wal_file_id: 0x3132_3334_3536_3738_393a_3b3c_3d3e_3f40,
        page_store_file_id: 0x4142_4344_4546_4748_494a_4b4c_4d4e_4f50,
        restart_checkpoint_file_id: 0x5152_5354_5556_5758_595a_5b5c_5d5e_5f60,
        persistent_log_id: 0x2122_2324_2526_2728_292a_2b2c_2d2e_2f30,
        wal_format_version: 4,
        page_store_format_version: 1,
        restart_checkpoint_format_version: 1,
    })
}

#[derive(Clone, Copy)]
struct TestManifestFields {
    database_id: u128,
    lifecycle_generation: u64,
    wal_file_id: u128,
    page_store_file_id: u128,
    restart_checkpoint_file_id: u128,
    persistent_log_id: u128,
    wal_format_version: u16,
    page_store_format_version: u16,
    restart_checkpoint_format_version: u16,
}

impl TestManifestFields {
    fn from_manifest(manifest: DatabaseManifest) -> Self {
        let composition = manifest.composition_identity();
        let formats = manifest.storage_formats();
        Self {
            database_id: composition.database_id().get(),
            lifecycle_generation: composition.lifecycle_generation().get(),
            wal_file_id: composition.file_id(DatabaseFileRole::Wal).get(),
            page_store_file_id: composition.file_id(DatabaseFileRole::PageStore).get(),
            restart_checkpoint_file_id: composition
                .file_id(DatabaseFileRole::RestartCheckpoint)
                .get(),
            persistent_log_id: composition.persistent_log_id().get(),
            wal_format_version: formats.version(DatabaseFileRole::Wal).get(),
            page_store_format_version: formats.version(DatabaseFileRole::PageStore).get(),
            restart_checkpoint_format_version: formats
                .version(DatabaseFileRole::RestartCheckpoint)
                .get(),
        }
    }
}

fn manifest(fields: TestManifestFields) -> Result<DatabaseManifest, Box<dyn Error>> {
    let files = [
        DatabaseFileIdentity::new(DatabaseFileRole::Wal, file_id(fields.wal_file_id)?),
        DatabaseFileIdentity::new(
            DatabaseFileRole::PageStore,
            file_id(fields.page_store_file_id)?,
        ),
        DatabaseFileIdentity::new(
            DatabaseFileRole::RestartCheckpoint,
            file_id(fields.restart_checkpoint_file_id)?,
        ),
    ];
    let composition = DatabaseCompositionIdentity::new(
        database_id(fields.database_id)?,
        generation(fields.lifecycle_generation)?,
        persistent_log_id(fields.persistent_log_id)?,
        &files,
    )?;
    let formats = DatabaseStorageFormatRequirements::new(
        format_version(fields.wal_format_version)?,
        format_version(fields.page_store_format_version)?,
        format_version(fields.restart_checkpoint_format_version)?,
    );
    Ok(DatabaseManifest::recovery_required(
        composition,
        formats,
        DatabaseRequiredFeatures::NONE,
    ))
}

fn database_id(value: u128) -> Result<DatabaseId, io::Error> {
    DatabaseId::new(value).ok_or_else(|| io::Error::other("test database ID is zero"))
}

fn file_id(value: u128) -> Result<DatabaseFileId, io::Error> {
    DatabaseFileId::new(value).ok_or_else(|| io::Error::other("test file ID is zero"))
}

fn generation(value: u64) -> Result<DatabaseLifecycleGeneration, io::Error> {
    DatabaseLifecycleGeneration::new(value)
        .ok_or_else(|| io::Error::other("test lifecycle generation is zero"))
}

fn persistent_log_id(value: u128) -> Result<PersistentLogId, io::Error> {
    PersistentLogId::new(value).ok_or_else(|| io::Error::other("test persistent log ID is zero"))
}

fn format_version(value: u16) -> Result<DatabaseStorageFormatVersion, io::Error> {
    DatabaseStorageFormatVersion::new(value)
        .ok_or_else(|| io::Error::other("test storage format version is zero"))
}

fn assert_reserved_range(
    encoded: &[u8; DATABASE_MANIFEST_V1_LENGTH],
    range: Range<usize>,
) -> Result<(), Box<dyn Error>> {
    for offset in range {
        let mut noncanonical = *encoded;
        noncanonical[offset] = 1;
        replace_checksum(&mut noncanonical);
        assert_eq!(
            decode_database_manifest(&noncanonical),
            Err(DatabaseManifestDecodeError::ReservedByteNonZero { offset, actual: 1 })
        );
    }
    Ok(())
}

fn replace_checksum(encoded: &mut [u8; DATABASE_MANIFEST_V1_LENGTH]) {
    let checksum = checksum_v1(&encoded[..CHECKSUM_OFFSET]);
    write_u64(encoded, CHECKSUM_OFFSET, checksum);
}

fn checksum_v1(bytes: &[u8]) -> u64 {
    let mut state = CHECKSUM_SEED;
    let mut protected_len = 0_u64;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(CHECKSUM_MULTIPLIER);
        state = state.rotate_left(7) ^ CHECKSUM_MIX;
        protected_len = protected_len.wrapping_add(1);
    }
    state ^ protected_len
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
