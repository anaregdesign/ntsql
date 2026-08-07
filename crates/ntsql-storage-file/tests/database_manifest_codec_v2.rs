use std::{error::Error, io, ops::Range};

use ntsql_database::{
    DatabaseCleanCloseCertificate, DatabaseCleanCloseCertificateError, DatabaseCompositionIdentity,
    DatabaseCompositionIdentityError, DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole,
    DatabaseId, DatabaseLifecycleGeneration, DatabaseLifecycleGenerationTransitionError,
    DatabaseManifest, DatabaseManifestLifecycleState, DatabaseRequiredFeatures,
    DatabaseRequiredFeaturesError, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_storage_file::{
    DATABASE_MANIFEST_V2_LENGTH, DatabaseManifestV2DecodeError, decode_database_manifest_v2,
    encode_database_manifest_v2,
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
const SOURCE_GENERATION_OFFSET: usize = 128;
const FRONTIER_PRESENCE_OFFSET: usize = 136;
const FRONTIER_OFFSET: usize = 144;
const ALLOCATED_EPOCH_OFFSET: usize = 152;
const CHECKPOINT_ANCHOR_VERSION_OFFSET: usize = 160;
const CHECKPOINT_ANCHOR_VALUE_OFFSET: usize = 168;
const TRANSACTION_COUNT_OFFSET: usize = 184;
const PAGE_COUNT_OFFSET: usize = 192;
const FOOTER_MAGIC_OFFSET: usize = 240;
const CHECKSUM_OFFSET: usize = 248;

const CHECKSUM_SEED: u64 = 0x4e54_5351_4c43_4b31;
const CHECKSUM_MULTIPLIER: u64 = 0x4e54_5351_4c57_414d;
const CHECKSUM_MIX: u64 = 0x4348_4543_4b53_554d;

#[test]
fn recovery_required_manifest_has_exact_version_two_golden_bytes() -> Result<(), Box<dyn Error>> {
    let manifest = golden_recovery_required_manifest()?;
    let encoded = encode_database_manifest_v2(&manifest);
    let expected = [
        0x4e, 0x54, 0x53, 0x51, 0x44, 0x42, 0x4d, 0x32, 0x00, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c,
        0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b,
        0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a,
        0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
        0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x00, 0x04, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x4e, 0x54, 0x53, 0x51, 0x44, 0x42, 0x45, 0x32, 0x40, 0xe4, 0x21, 0x7a, 0x78, 0x59, 0xa1,
        0x91,
    ];

    assert_eq!(encoded, expected);
    assert_eq!(decode_database_manifest_v2(&expected)?, manifest);
    Ok(())
}

fn golden_recovery_required_manifest() -> Result<DatabaseManifest, Box<dyn Error>> {
    manifest(GoldenFields::recovery_required())
}

fn golden_clean_manifest() -> Result<DatabaseManifest, Box<dyn Error>> {
    manifest_clean(GoldenFields::clean(), golden_certificate()?)
}

fn golden_certificate() -> Result<DatabaseCleanCloseCertificate, Box<dyn Error>> {
    DatabaseCleanCloseCertificate::new(
        generation(0x1112_1314_1516_1718)?,
        Some(0x6162_6364_6566_6768),
        0x7172_7374_7576_7778,
        0x8182,
        0x9192_9394_9596_9798_999a_9b9c_9d9e_9fa0,
        0xa1a2_a3a4_a5a6_a7a8,
        0xb1b2_b3b4_b5b6_b7b8,
    )
    .map_err(|source| io::Error::other(source.to_string()).into())
}

#[derive(Clone, Copy)]
struct GoldenFields;

impl GoldenFields {
    fn recovery_required() -> TestManifestFields {
        TestManifestFields {
            database_id: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
            lifecycle_generation: 0x1112_1314_1516_1718,
            wal_file_id: 0x3132_3334_3536_3738_393a_3b3c_3d3e_3f40,
            page_store_file_id: 0x4142_4344_4546_4748_494a_4b4c_4d4e_4f50,
            restart_checkpoint_file_id: 0x5152_5354_5556_5758_595a_5b5c_5d5e_5f60,
            persistent_log_id: 0x2122_2324_2526_2728_292a_2b2c_2d2e_2f30,
            wal_format_version: 4,
            page_store_format_version: 1,
            restart_checkpoint_format_version: 1,
        }
    }

    fn clean() -> TestManifestFields {
        TestManifestFields {
            lifecycle_generation: 0x1112_1314_1516_1719,
            ..Self::recovery_required()
        }
    }
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

fn composition_identity(
    fields: TestManifestFields,
) -> Result<DatabaseCompositionIdentity, Box<dyn Error>> {
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
    Ok(DatabaseCompositionIdentity::new(
        database_id(fields.database_id)?,
        generation(fields.lifecycle_generation)?,
        persistent_log_id(fields.persistent_log_id)?,
        &files,
    )?)
}

fn storage_formats(
    fields: TestManifestFields,
) -> Result<DatabaseStorageFormatRequirements, Box<dyn Error>> {
    Ok(DatabaseStorageFormatRequirements::new(
        format_version(fields.wal_format_version)?,
        format_version(fields.page_store_format_version)?,
        format_version(fields.restart_checkpoint_format_version)?,
    ))
}

fn manifest(fields: TestManifestFields) -> Result<DatabaseManifest, Box<dyn Error>> {
    Ok(DatabaseManifest::recovery_required(
        composition_identity(fields)?,
        storage_formats(fields)?,
        DatabaseRequiredFeatures::NONE,
    ))
}

fn manifest_clean(
    fields: TestManifestFields,
    certificate: DatabaseCleanCloseCertificate,
) -> Result<DatabaseManifest, Box<dyn Error>> {
    Ok(DatabaseManifest::clean(
        composition_identity(fields)?,
        storage_formats(fields)?,
        DatabaseRequiredFeatures::NONE,
        certificate,
    )?)
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

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    let mut buffer = [0_u8; 2];
    buffer.copy_from_slice(&bytes[offset..offset + 2]);
    u16::from_be_bytes(buffer)
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

fn replace_checksum(encoded: &mut [u8; DATABASE_MANIFEST_V2_LENGTH]) {
    let checksum = checksum_v1(&encoded[..CHECKSUM_OFFSET]);
    write_u64(encoded, CHECKSUM_OFFSET, checksum);
}

fn assert_reserved_range(
    encoded: &[u8; DATABASE_MANIFEST_V2_LENGTH],
    range: Range<usize>,
) -> Result<(), Box<dyn Error>> {
    for offset in range {
        let mut noncanonical = *encoded;
        noncanonical[offset] = 1;
        replace_checksum(&mut noncanonical);
        assert_eq!(
            decode_database_manifest_v2(&noncanonical),
            Err(DatabaseManifestV2DecodeError::ReservedByteNonZero { offset, actual: 1 })
        );
    }
    Ok(())
}

#[test]
fn clean_manifest_has_exact_version_two_golden_bytes() -> Result<(), Box<dyn Error>> {
    let manifest = golden_clean_manifest()?;
    let encoded = encode_database_manifest_v2(&manifest);
    let expected = [
        0x4e, 0x54, 0x53, 0x51, 0x44, 0x42, 0x4d, 0x32, 0x00, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x19, 0x02, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c,
        0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b,
        0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a,
        0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
        0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x00, 0x04, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
        0x18, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66,
        0x67, 0x68, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x81, 0x82, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c,
        0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xb1, 0xb2, 0xb3,
        0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x4e, 0x54, 0x53, 0x51, 0x44, 0x42, 0x45, 0x32, 0x6e, 0x0e, 0x92, 0x65, 0x3c, 0x7b, 0x54,
        0x6f,
    ];

    assert_eq!(encoded, expected);
    assert_eq!(decode_database_manifest_v2(&expected)?, manifest);
    Ok(())
}

#[test]
fn every_prefix_is_truncated_and_one_extra_byte_is_trailing() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_recovery_required_manifest()?);
    for actual_length in 0..DATABASE_MANIFEST_V2_LENGTH {
        assert_eq!(
            decode_database_manifest_v2(&encoded[..actual_length]),
            Err(DatabaseManifestV2DecodeError::Truncated {
                expected_length: DATABASE_MANIFEST_V2_LENGTH,
                actual_length,
            })
        );
    }

    let mut trailing = encoded.to_vec();
    trailing.push(0);
    assert_eq!(
        decode_database_manifest_v2(&trailing),
        Err(DatabaseManifestV2DecodeError::TrailingBytes {
            expected_length: DATABASE_MANIFEST_V2_LENGTH,
            actual_length: DATABASE_MANIFEST_V2_LENGTH + 1,
        })
    );
    Ok(())
}

#[test]
fn envelope_fields_and_checksum_fail_distinctly() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_recovery_required_manifest()?);

    let mut wrong_magic = encoded;
    wrong_magic[0] = 0;
    assert_eq!(
        decode_database_manifest_v2(&wrong_magic),
        Err(DatabaseManifestV2DecodeError::HeaderMagicMismatch {
            actual: [0, 0x54, 0x53, 0x51, 0x44, 0x42, 0x4d, 0x32],
        })
    );

    let mut wrong_version = encoded;
    write_u16(&mut wrong_version, 8, 1);
    assert_eq!(
        decode_database_manifest_v2(&wrong_version),
        Err(DatabaseManifestV2DecodeError::UnsupportedVersion { actual: 1 })
    );

    let mut wrong_length = encoded;
    write_u16(&mut wrong_length, 10, 255);
    assert_eq!(
        decode_database_manifest_v2(&wrong_length),
        Err(DatabaseManifestV2DecodeError::FrameLengthMismatch { actual: 255 })
    );

    for actual in [1, 0x8000_0000] {
        let mut unknown_header_flags = encoded;
        write_u32(&mut unknown_header_flags, 12, actual);
        assert_eq!(
            decode_database_manifest_v2(&unknown_header_flags),
            Err(DatabaseManifestV2DecodeError::HeaderFlagsUnsupported { actual })
        );
    }

    let mut wrong_footer = encoded;
    wrong_footer[FOOTER_MAGIC_OFFSET] = 0;
    assert_eq!(
        decode_database_manifest_v2(&wrong_footer),
        Err(DatabaseManifestV2DecodeError::FooterMagicMismatch {
            actual: [0, 0x54, 0x53, 0x51, 0x44, 0x42, 0x45, 0x32],
        })
    );

    let mut wrong_checksum = encoded;
    wrong_checksum[CHECKSUM_OFFSET] ^= 0xff;
    assert_eq!(
        decode_database_manifest_v2(&wrong_checksum),
        Err(DatabaseManifestV2DecodeError::ChecksumMismatch {
            expected: checksum_v1(&wrong_checksum[..CHECKSUM_OFFSET]),
            actual: read_u64(&wrong_checksum, CHECKSUM_OFFSET),
        })
    );
    Ok(())
}

#[test]
fn every_common_reserved_byte_is_rejected_after_checksum_validation() -> Result<(), Box<dyn Error>>
{
    let encoded = encode_database_manifest_v2(&golden_recovery_required_manifest()?);
    for range in [41..48, 118..120] {
        assert_reserved_range(&encoded, range)?;
    }
    Ok(())
}

#[test]
fn recovery_required_certificate_area_must_be_entirely_zero() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_recovery_required_manifest()?);
    for offset in 128..240 {
        let mut noncanonical = encoded;
        noncanonical[offset] = 1;
        replace_checksum(&mut noncanonical);
        assert_eq!(
            decode_database_manifest_v2(&noncanonical),
            Err(DatabaseManifestV2DecodeError::CertificateAreaNonZero { offset, actual: 1 })
        );
    }
    Ok(())
}

#[test]
fn clean_certificate_reserved_bytes_are_rejected_independently() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_clean_manifest()?);
    for range in [137..144, 162..168, 200..240] {
        assert_reserved_range(&encoded, range)?;
    }
    Ok(())
}

#[test]
fn zero_and_unsupported_common_scalar_fields_fail_closed() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_recovery_required_manifest()?);

    let mut zero_database = encoded;
    zero_database[DATABASE_ID_OFFSET..DATABASE_ID_OFFSET + 16].fill(0);
    replace_checksum(&mut zero_database);
    assert_eq!(
        decode_database_manifest_v2(&zero_database),
        Err(DatabaseManifestV2DecodeError::DatabaseIdZero)
    );

    let mut zero_generation = encoded;
    zero_generation[LIFECYCLE_GENERATION_OFFSET..LIFECYCLE_GENERATION_OFFSET + 8].fill(0);
    replace_checksum(&mut zero_generation);
    assert_eq!(
        decode_database_manifest_v2(&zero_generation),
        Err(DatabaseManifestV2DecodeError::LifecycleGenerationZero)
    );

    for actual in [0, 3, u8::MAX] {
        let mut unsupported_state = encoded;
        unsupported_state[LIFECYCLE_STATE_OFFSET] = actual;
        replace_checksum(&mut unsupported_state);
        assert_eq!(
            decode_database_manifest_v2(&unsupported_state),
            Err(DatabaseManifestV2DecodeError::LifecycleStateUnsupported { actual })
        );
    }

    let mut zero_log = encoded;
    zero_log[PERSISTENT_LOG_ID_OFFSET..PERSISTENT_LOG_ID_OFFSET + 16].fill(0);
    replace_checksum(&mut zero_log);
    assert_eq!(
        decode_database_manifest_v2(&zero_log),
        Err(DatabaseManifestV2DecodeError::PersistentLogIdZero)
    );
    Ok(())
}

#[test]
fn each_file_role_rejects_zero_and_cross_role_identity_reuse() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_recovery_required_manifest()?);
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
            decode_database_manifest_v2(&zero_file),
            Err(DatabaseManifestV2DecodeError::FileIdZero { role })
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
            decode_database_manifest_v2(&duplicate),
            Err(DatabaseManifestV2DecodeError::CompositionIdentity(
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
    let encoded = encode_database_manifest_v2(&golden_recovery_required_manifest()?);
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
            decode_database_manifest_v2(&zero_version),
            Err(DatabaseManifestV2DecodeError::StorageFormatVersionZero { role })
        );
    }

    for actual in [1_u64, 0x8000_0000_0000_0000] {
        let mut unknown_features = encoded;
        write_u64(&mut unknown_features, REQUIRED_FEATURES_OFFSET, actual);
        replace_checksum(&mut unknown_features);
        assert_eq!(
            decode_database_manifest_v2(&unknown_features),
            Err(DatabaseManifestV2DecodeError::RequiredFeatures(
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
fn frontier_presence_byte_is_validated_and_absence_must_be_canonical() -> Result<(), Box<dyn Error>>
{
    let encoded = encode_database_manifest_v2(&golden_clean_manifest()?);

    for actual in [2_u8, u8::MAX] {
        let mut unsupported_presence = encoded;
        unsupported_presence[FRONTIER_PRESENCE_OFFSET] = actual;
        replace_checksum(&mut unsupported_presence);
        assert_eq!(
            decode_database_manifest_v2(&unsupported_presence),
            Err(DatabaseManifestV2DecodeError::FrontierPresenceUnsupported { actual })
        );
    }

    let mut noncanonical_absence = encoded;
    noncanonical_absence[FRONTIER_PRESENCE_OFFSET] = 0;
    replace_checksum(&mut noncanonical_absence);
    let actual = read_u64(&noncanonical_absence, FRONTIER_OFFSET);
    assert_eq!(
        decode_database_manifest_v2(&noncanonical_absence),
        Err(DatabaseManifestV2DecodeError::FrontierNotCanonicallyZero { actual })
    );

    let mut zero_frontier_present = encoded;
    zero_frontier_present[FRONTIER_OFFSET..FRONTIER_OFFSET + 8].fill(0);
    replace_checksum(&mut zero_frontier_present);
    assert_eq!(
        decode_database_manifest_v2(&zero_frontier_present),
        Err(DatabaseManifestV2DecodeError::CleanCertificate(
            DatabaseCleanCloseCertificateError::DurableWalFrontierZero
        ))
    );
    Ok(())
}

#[test]
fn absent_frontier_round_trips_with_canonical_zero_bytes() -> Result<(), Box<dyn Error>> {
    let certificate = DatabaseCleanCloseCertificate::new(generation(1)?, None, 2, 3, 4, 5, 6)
        .map_err(|source| Box::<dyn Error>::from(source.to_string()))?;
    let manifest = manifest_clean(
        TestManifestFields {
            lifecycle_generation: 2,
            ..GoldenFields::recovery_required()
        },
        certificate,
    )?;
    let encoded = encode_database_manifest_v2(&manifest);
    assert_eq!(encoded[FRONTIER_PRESENCE_OFFSET], 0);
    assert_eq!(read_u64(&encoded, FRONTIER_OFFSET), 0);
    assert_eq!(decode_database_manifest_v2(&encoded)?, manifest);
    Ok(())
}

#[test]
fn clean_certificate_scalar_fields_fail_closed() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_clean_manifest()?);

    let mut zero_source_generation = encoded;
    zero_source_generation[SOURCE_GENERATION_OFFSET..SOURCE_GENERATION_OFFSET + 8].fill(0);
    replace_checksum(&mut zero_source_generation);
    assert_eq!(
        decode_database_manifest_v2(&zero_source_generation),
        Err(DatabaseManifestV2DecodeError::CertificateSourceGenerationZero)
    );

    let mut zero_epoch = encoded;
    zero_epoch[ALLOCATED_EPOCH_OFFSET..ALLOCATED_EPOCH_OFFSET + 8].fill(0);
    replace_checksum(&mut zero_epoch);
    assert_eq!(
        decode_database_manifest_v2(&zero_epoch),
        Err(DatabaseManifestV2DecodeError::CleanCertificate(
            DatabaseCleanCloseCertificateError::AllocatedTransactionEpochHighWaterZero
        ))
    );

    let mut zero_anchor_version = encoded;
    write_u16(
        &mut zero_anchor_version,
        CHECKPOINT_ANCHOR_VERSION_OFFSET,
        0,
    );
    replace_checksum(&mut zero_anchor_version);
    assert_eq!(
        decode_database_manifest_v2(&zero_anchor_version),
        Err(DatabaseManifestV2DecodeError::CleanCertificate(
            DatabaseCleanCloseCertificateError::CheckpointAnchorVersionZero
        ))
    );
    Ok(())
}

#[test]
fn clean_certificate_source_generation_must_be_exact_predecessor() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_clean_manifest()?);

    let mut skipped = encoded;
    write_u64(
        &mut skipped,
        SOURCE_GENERATION_OFFSET,
        0x1112_1314_1516_1717,
    );
    replace_checksum(&mut skipped);
    assert_eq!(
        decode_database_manifest_v2(&skipped),
        Err(DatabaseManifestV2DecodeError::CleanManifest(
            DatabaseLifecycleGenerationTransitionError::Skipped {
                expected: generation(0x1112_1314_1516_1718)?,
                proposed: generation(0x1112_1314_1516_1719)?,
            }
        ))
    );

    let mut regressed = encoded;
    write_u64(
        &mut regressed,
        SOURCE_GENERATION_OFFSET,
        0x1112_1314_1516_1719,
    );
    replace_checksum(&mut regressed);
    assert_eq!(
        decode_database_manifest_v2(&regressed),
        Err(DatabaseManifestV2DecodeError::CleanManifest(
            DatabaseLifecycleGenerationTransitionError::NotStrictlyIncreasing {
                current: generation(0x1112_1314_1516_1719)?,
                proposed: generation(0x1112_1314_1516_1719)?,
            }
        ))
    );
    Ok(())
}

#[test]
fn maximum_recovery_required_fields_round_trip_without_host_width_dependence()
-> Result<(), Box<dyn Error>> {
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
    let encoded = encode_database_manifest_v2(&manifest);
    assert_eq!(decode_database_manifest_v2(&encoded)?, manifest);
    Ok(())
}

#[test]
fn maximum_clean_certificate_fields_round_trip_without_host_width_dependence()
-> Result<(), Box<dyn Error>> {
    let fields = TestManifestFields {
        database_id: u128::MAX,
        lifecycle_generation: u64::MAX,
        wal_file_id: u128::MAX - 2,
        page_store_file_id: u128::MAX - 3,
        restart_checkpoint_file_id: u128::MAX - 4,
        persistent_log_id: u128::MAX - 1,
        wal_format_version: u16::MAX,
        page_store_format_version: u16::MAX,
        restart_checkpoint_format_version: u16::MAX,
    };
    let certificate = DatabaseCleanCloseCertificate::new(
        generation(u64::MAX - 1)?,
        Some(u64::MAX),
        u64::MAX,
        u16::MAX,
        u128::MAX,
        u64::MAX,
        u64::MAX,
    )
    .map_err(|source| Box::<dyn Error>::from(source.to_string()))?;
    let manifest = manifest_clean(fields, certificate)?;
    let encoded = encode_database_manifest_v2(&manifest);
    assert_eq!(decode_database_manifest_v2(&encoded)?, manifest);
    Ok(())
}

#[test]
fn decoded_v2_generation_regression_is_rejected_against_exact_previous_manifest()
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
    let decoded = decode_database_manifest_v2(&encode_database_manifest_v2(&regressed))?;
    assert_eq!(
        decoded.require_successor_of(previous),
        Err(
            ntsql_database::DatabaseManifestSuccessorError::LifecycleGeneration(
                DatabaseLifecycleGenerationTransitionError::NotStrictlyIncreasing {
                    current: generation(2)?,
                    proposed: generation(1)?,
                }
            )
        )
    );
    Ok(())
}

#[test]
fn recovery_required_and_clean_encode_and_decode_are_never_confused_by_lifecycle_state()
-> Result<(), Box<dyn Error>> {
    let recovery_required = golden_recovery_required_manifest()?;
    let clean = golden_clean_manifest()?;
    assert!(matches!(
        recovery_required.lifecycle_state(),
        DatabaseManifestLifecycleState::RecoveryRequired
    ));
    assert!(matches!(
        clean.lifecycle_state(),
        DatabaseManifestLifecycleState::Clean(_)
    ));
    assert_eq!(
        decode_database_manifest_v2(&encode_database_manifest_v2(&recovery_required))?,
        recovery_required
    );
    assert_eq!(
        decode_database_manifest_v2(&encode_database_manifest_v2(&clean))?,
        clean
    );
    Ok(())
}

#[test]
fn certificate_countable_fields_round_trip_exactly() -> Result<(), Box<dyn Error>> {
    let encoded = encode_database_manifest_v2(&golden_clean_manifest()?);
    assert_eq!(read_u16(&encoded, CHECKPOINT_ANCHOR_VERSION_OFFSET), 0x8182);
    assert_eq!(
        read_u128(&encoded, CHECKPOINT_ANCHOR_VALUE_OFFSET),
        0x9192_9394_9596_9798_999a_9b9c_9d9e_9fa0
    );
    assert_eq!(
        read_u64(&encoded, TRANSACTION_COUNT_OFFSET),
        0xa1a2_a3a4_a5a6_a7a8
    );
    assert_eq!(read_u64(&encoded, PAGE_COUNT_OFFSET), 0xb1b2_b3b4_b5b6_b7b8);
    Ok(())
}
