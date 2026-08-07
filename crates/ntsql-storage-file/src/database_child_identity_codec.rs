use std::{error::Error, fmt};

use ntsql_database::{
    DatabaseFileHeaderIdentity, DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole, DatabaseId,
};

use super::{read_u16, read_u128, write_u16, write_u128};

const MAGIC: [u8; 8] = *b"NTSQCFI1";
const FORMAT_VERSION: u16 = 1;
const LENGTH_U16: u16 = 48;
const ROLE_OFFSET: usize = 12;
const RESERVED_START: usize = 13;
const RESERVED_END: usize = 16;
const DATABASE_ID_OFFSET: usize = 16;
const FILE_ID_OFFSET: usize = 32;

pub(crate) const DATABASE_CHILD_IDENTITY_V1_LENGTH: usize = 48;

const ROLE_WAL: u8 = 1;
const ROLE_PAGE_STORE: u8 = 2;
const ROLE_RESTART_CHECKPOINT: u8 = 3;

/// Structural reason one database child-identity extension is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseChildIdentityDecodeErrorReason {
    /// The independent extension magic did not match.
    Magic,
    /// The extension version is unsupported.
    Version {
        /// Exact decoded version.
        actual: u16,
    },
    /// The declared extension length is not canonical.
    Length {
        /// Exact decoded length.
        actual: u16,
    },
    /// The role discriminant is unsupported.
    Role {
        /// Exact decoded role byte.
        actual: u8,
    },
    /// One reserved extension byte was nonzero.
    Reserved {
        /// Exact nonzero byte.
        actual: u8,
    },
    /// The repository-owned database identity was zero.
    DatabaseIdZero,
    /// The repository-owned child file identity was zero.
    FileIdZero,
}

impl fmt::Display for DatabaseChildIdentityDecodeErrorReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Magic => formatter.write_str("database child identity magic does not match"),
            Self::Version { actual } => {
                write!(
                    formatter,
                    "database child identity version {actual} is unsupported"
                )
            }
            Self::Length { actual } => {
                write!(
                    formatter,
                    "database child identity length {actual} is not {DATABASE_CHILD_IDENTITY_V1_LENGTH}"
                )
            }
            Self::Role { actual } => {
                write!(
                    formatter,
                    "database child identity role {actual} is unsupported"
                )
            }
            Self::Reserved { actual } => {
                write!(
                    formatter,
                    "database child identity reserved byte is nonzero: {actual:#04x}"
                )
            }
            Self::DatabaseIdZero => {
                formatter.write_str("database child identity database ID is zero")
            }
            Self::FileIdZero => formatter.write_str("database child identity file ID is zero"),
        }
    }
}

impl Error for DatabaseChildIdentityDecodeErrorReason {}

/// Malformed child-identity extension paired with its relative byte offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseChildIdentityDecodeError {
    offset: usize,
    reason: DatabaseChildIdentityDecodeErrorReason,
}

impl DatabaseChildIdentityDecodeError {
    const fn new(offset: usize, reason: DatabaseChildIdentityDecodeErrorReason) -> Self {
        Self { offset, reason }
    }

    /// Returns the byte offset relative to the extension start.
    #[must_use]
    pub const fn offset(self) -> usize {
        self.offset
    }

    /// Returns the exact malformed-field reason.
    #[must_use]
    pub const fn reason(self) -> DatabaseChildIdentityDecodeErrorReason {
        self.reason
    }
}

impl fmt::Display for DatabaseChildIdentityDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "database child identity format error at byte {}: {}",
            self.offset, self.reason
        )
    }
}

impl Error for DatabaseChildIdentityDecodeError {}

pub(crate) fn encode_database_child_identity(
    identity: DatabaseFileHeaderIdentity,
) -> [u8; DATABASE_CHILD_IDENTITY_V1_LENGTH] {
    let mut encoded = [0_u8; DATABASE_CHILD_IDENTITY_V1_LENGTH];
    encoded[..8].copy_from_slice(&MAGIC);
    write_u16(&mut encoded, 8, FORMAT_VERSION);
    write_u16(&mut encoded, 10, LENGTH_U16);
    encoded[ROLE_OFFSET] = encode_role(identity.file().role());
    write_u128(
        &mut encoded,
        DATABASE_ID_OFFSET,
        identity.database_id().get(),
    );
    write_u128(
        &mut encoded,
        FILE_ID_OFFSET,
        identity.file().file_id().get(),
    );
    encoded
}

pub(crate) fn decode_database_child_identity(
    encoded: &[u8; DATABASE_CHILD_IDENTITY_V1_LENGTH],
) -> Result<DatabaseFileHeaderIdentity, DatabaseChildIdentityDecodeError> {
    if encoded[..8] != MAGIC {
        return Err(DatabaseChildIdentityDecodeError::new(
            0,
            DatabaseChildIdentityDecodeErrorReason::Magic,
        ));
    }
    let version = read_u16(encoded, 8);
    if version != FORMAT_VERSION {
        return Err(DatabaseChildIdentityDecodeError::new(
            8,
            DatabaseChildIdentityDecodeErrorReason::Version { actual: version },
        ));
    }
    let length = read_u16(encoded, 10);
    if length != LENGTH_U16 {
        return Err(DatabaseChildIdentityDecodeError::new(
            10,
            DatabaseChildIdentityDecodeErrorReason::Length { actual: length },
        ));
    }
    let role = decode_role(encoded[ROLE_OFFSET])?;
    for (offset, actual) in encoded[RESERVED_START..RESERVED_END]
        .iter()
        .copied()
        .enumerate()
    {
        if actual != 0 {
            return Err(DatabaseChildIdentityDecodeError::new(
                RESERVED_START + offset,
                DatabaseChildIdentityDecodeErrorReason::Reserved { actual },
            ));
        }
    }
    let database_id = DatabaseId::new(read_u128(encoded, DATABASE_ID_OFFSET)).ok_or(
        DatabaseChildIdentityDecodeError::new(
            DATABASE_ID_OFFSET,
            DatabaseChildIdentityDecodeErrorReason::DatabaseIdZero,
        ),
    )?;
    let file_id = DatabaseFileId::new(read_u128(encoded, FILE_ID_OFFSET)).ok_or(
        DatabaseChildIdentityDecodeError::new(
            FILE_ID_OFFSET,
            DatabaseChildIdentityDecodeErrorReason::FileIdZero,
        ),
    )?;
    Ok(DatabaseFileHeaderIdentity::new(
        database_id,
        DatabaseFileIdentity::new(role, file_id),
    ))
}

const fn encode_role(role: DatabaseFileRole) -> u8 {
    match role {
        DatabaseFileRole::Wal => ROLE_WAL,
        DatabaseFileRole::PageStore => ROLE_PAGE_STORE,
        DatabaseFileRole::RestartCheckpoint => ROLE_RESTART_CHECKPOINT,
    }
}

fn decode_role(role: u8) -> Result<DatabaseFileRole, DatabaseChildIdentityDecodeError> {
    match role {
        ROLE_WAL => Ok(DatabaseFileRole::Wal),
        ROLE_PAGE_STORE => Ok(DatabaseFileRole::PageStore),
        ROLE_RESTART_CHECKPOINT => Ok(DatabaseFileRole::RestartCheckpoint),
        actual => Err(DatabaseChildIdentityDecodeError::new(
            ROLE_OFFSET,
            DatabaseChildIdentityDecodeErrorReason::Role { actual },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Result<DatabaseFileHeaderIdentity, &'static str> {
        let database_id = DatabaseId::new(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)
            .ok_or("database ID must be nonzero")?;
        let file_id = DatabaseFileId::new(0x1112_1314_1516_1718_191a_1b1c_1d1e_1f20)
            .ok_or("file ID must be nonzero")?;
        Ok(DatabaseFileHeaderIdentity::new(
            database_id,
            DatabaseFileIdentity::new(DatabaseFileRole::Wal, file_id),
        ))
    }

    #[test]
    fn child_identity_has_exact_golden_bytes() -> Result<(), &'static str> {
        let expected = [
            0x4e, 0x54, 0x53, 0x51, 0x43, 0x46, 0x49, 0x31, 0x00, 0x01, 0x00, 0x30, 0x01, 0x00,
            0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a,
            0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
        ];
        let identity = identity()?;
        assert_eq!(encode_database_child_identity(identity), expected);
        assert_eq!(decode_database_child_identity(&expected), Ok(identity));
        Ok(())
    }

    #[test]
    fn child_identity_rejects_every_structural_field() -> Result<(), &'static str> {
        let original = encode_database_child_identity(identity()?);
        for (offset, replacement, reason) in [
            (0, 0, DatabaseChildIdentityDecodeErrorReason::Magic),
            (
                9,
                2,
                DatabaseChildIdentityDecodeErrorReason::Version { actual: 2 },
            ),
            (
                11,
                47,
                DatabaseChildIdentityDecodeErrorReason::Length { actual: 47 },
            ),
            (
                12,
                9,
                DatabaseChildIdentityDecodeErrorReason::Role { actual: 9 },
            ),
            (
                13,
                1,
                DatabaseChildIdentityDecodeErrorReason::Reserved { actual: 1 },
            ),
        ] {
            let mut mutated = original;
            mutated[offset] = replacement;
            let error = decode_database_child_identity(&mutated)
                .err()
                .ok_or("mutated identity must fail")?;
            assert_eq!(error.reason(), reason);
        }

        let mut zero_database = original;
        zero_database[DATABASE_ID_OFFSET..FILE_ID_OFFSET].fill(0);
        assert_eq!(
            decode_database_child_identity(&zero_database)
                .err()
                .ok_or("zero database ID must fail")?
                .reason(),
            DatabaseChildIdentityDecodeErrorReason::DatabaseIdZero
        );

        let mut zero_file = original;
        zero_file[FILE_ID_OFFSET..].fill(0);
        assert_eq!(
            decode_database_child_identity(&zero_file)
                .err()
                .ok_or("zero file ID must fail")?
                .reason(),
            DatabaseChildIdentityDecodeErrorReason::FileIdZero
        );
        Ok(())
    }
}
