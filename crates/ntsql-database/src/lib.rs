//! I/O-free database identity and lifecycle ownership invariants.

use std::{
    error::Error,
    fmt,
    num::{NonZeroU16, NonZeroU64, NonZeroU128},
};

use ntsql_transaction::{
    DurablePageStoreSnapshotSource, DurableTransactionRestartAnalysisSource,
    WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay,
};
use ntsql_wal::PersistentLogId;

/// Repository-owned nonzero identity for one logical database.
///
/// Allocation, durable storage, uniqueness, and byte encoding belong to an
/// outer adapter. This value defines no Microsoft database identifier.
///
/// ```compile_fail
/// use std::num::NonZeroU128;
/// use ntsql_database::DatabaseId;
///
/// let forged = DatabaseId(NonZeroU128::MIN);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatabaseId(NonZeroU128);

impl DatabaseId {
    /// Wraps one nonzero adapter-owned database identity.
    #[must_use]
    pub const fn new(value: u128) -> Option<Self> {
        match NonZeroU128::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric identity for adapter bookkeeping.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0.get()
    }
}

/// Repository-owned nonzero identity for one database file role.
///
/// This identity is independent from [`PersistentLogId`]. Equal numeric values
/// in those separate namespaces do not imply equal authority.
///
/// ```compile_fail
/// use std::num::NonZeroU128;
/// use ntsql_database::DatabaseFileId;
///
/// let forged = DatabaseFileId(NonZeroU128::MIN);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatabaseFileId(NonZeroU128);

impl DatabaseFileId {
    /// Wraps one nonzero adapter-owned file identity.
    #[must_use]
    pub const fn new(value: u128) -> Option<Self> {
        match NonZeroU128::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric identity for adapter bookkeeping.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0.get()
    }
}

/// Strictly positive generation of one database lifecycle record.
///
/// Generation one is the first publishable lifecycle generation. Zero is
/// rejected rather than interpreted as an implicit or legacy database.
///
/// ```compile_fail
/// use std::num::NonZeroU64;
/// use ntsql_database::DatabaseLifecycleGeneration;
///
/// let forged = DatabaseLifecycleGeneration(NonZeroU64::MIN);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatabaseLifecycleGeneration(NonZeroU64);

impl DatabaseLifecycleGeneration {
    /// Wraps one nonzero lifecycle generation.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric generation for adapter bookkeeping.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the exact next generation or explicit exhaustion.
    pub const fn checked_next(self) -> Result<Self, DatabaseLifecycleGenerationExhausted> {
        let Some(next) = self.0.get().checked_add(1) else {
            return Err(DatabaseLifecycleGenerationExhausted { current: self });
        };
        let Some(next) = NonZeroU64::new(next) else {
            return Err(DatabaseLifecycleGenerationExhausted { current: self });
        };
        Ok(Self(next))
    }

    /// Requires `proposed` to be the exact successor of this generation.
    pub const fn require_successor(
        self,
        proposed: Self,
    ) -> Result<(), DatabaseLifecycleGenerationTransitionError> {
        let expected = match self.checked_next() {
            Ok(expected) => expected,
            Err(_) => {
                return Err(DatabaseLifecycleGenerationTransitionError::Exhausted {
                    current: self,
                });
            }
        };
        if proposed.get() <= self.get() {
            return Err(
                DatabaseLifecycleGenerationTransitionError::NotStrictlyIncreasing {
                    current: self,
                    proposed,
                },
            );
        }
        if proposed.get() != expected.get() {
            return Err(DatabaseLifecycleGenerationTransitionError::Skipped { expected, proposed });
        }
        Ok(())
    }
}

/// Failure to allocate a lifecycle generation above the current high-water.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseLifecycleGenerationExhausted {
    /// Highest already-owned lifecycle generation.
    pub current: DatabaseLifecycleGeneration,
}

impl fmt::Display for DatabaseLifecycleGenerationExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "database lifecycle generation {} is exhausted",
            self.current.get()
        )
    }
}

impl Error for DatabaseLifecycleGenerationExhausted {}

/// Rejection of a proposed lifecycle-generation transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseLifecycleGenerationTransitionError {
    /// No generation exists above the retained current generation.
    Exhausted {
        /// Highest already-owned lifecycle generation.
        current: DatabaseLifecycleGeneration,
    },
    /// The proposed generation is equal to or below the retained generation.
    NotStrictlyIncreasing {
        /// Retained lifecycle generation.
        current: DatabaseLifecycleGeneration,
        /// Rejected proposed lifecycle generation.
        proposed: DatabaseLifecycleGeneration,
    },
    /// The proposed generation skipped the exact required successor.
    Skipped {
        /// Exact required successor.
        expected: DatabaseLifecycleGeneration,
        /// Rejected proposed lifecycle generation.
        proposed: DatabaseLifecycleGeneration,
    },
}

impl fmt::Display for DatabaseLifecycleGenerationTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted { current } => write!(
                formatter,
                "database lifecycle generation {} is exhausted",
                current.get()
            ),
            Self::NotStrictlyIncreasing { current, proposed } => write!(
                formatter,
                "database lifecycle generation {} does not advance current generation {}",
                proposed.get(),
                current.get()
            ),
            Self::Skipped { expected, proposed } => write!(
                formatter,
                "database lifecycle generation {} skips required successor {}",
                proposed.get(),
                expected.get()
            ),
        }
    }
}

impl Error for DatabaseLifecycleGenerationTransitionError {}

/// Stable role order for the three files in one database composition.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DatabaseFileRole {
    /// Transaction/page write-ahead log.
    Wal,
    /// Durable page store.
    PageStore,
    /// Restart-checkpoint completeness source.
    RestartCheckpoint,
}

impl fmt::Display for DatabaseFileRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wal => formatter.write_str("WAL"),
            Self::PageStore => formatter.write_str("page store"),
            Self::RestartCheckpoint => formatter.write_str("restart checkpoint"),
        }
    }
}

/// Inert association between one required role and one file identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseFileIdentity {
    role: DatabaseFileRole,
    file_id: DatabaseFileId,
}

impl DatabaseFileIdentity {
    /// Associates `file_id` with exactly one database file role.
    #[must_use]
    pub const fn new(role: DatabaseFileRole, file_id: DatabaseFileId) -> Self {
        Self { role, file_id }
    }

    /// Returns the required role.
    #[must_use]
    pub const fn role(self) -> DatabaseFileRole {
        self.role
    }

    /// Returns the role-bound file identity.
    #[must_use]
    pub const fn file_id(self) -> DatabaseFileId {
        self.file_id
    }
}

/// Stable identity persisted by one repository-owned database child header.
///
/// Lifecycle generation is deliberately absent. It is a manifest publication
/// coordinate and may advance while this child identity remains unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseFileHeaderIdentity {
    database_id: DatabaseId,
    file: DatabaseFileIdentity,
}

impl DatabaseFileHeaderIdentity {
    /// Binds one stable database identity to one exact child role and file ID.
    #[must_use]
    pub const fn new(database_id: DatabaseId, file: DatabaseFileIdentity) -> Self {
        Self { database_id, file }
    }

    /// Returns the logical database identity persisted by the child header.
    #[must_use]
    pub const fn database_id(self) -> DatabaseId {
        self.database_id
    }

    /// Returns the exact role-bound file identity persisted by the child header.
    #[must_use]
    pub const fn file(self) -> DatabaseFileIdentity {
        self.file
    }
}

/// Invalid set of role-bound identities for one database composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseCompositionIdentityError {
    /// More than one entry claimed the same required role.
    DuplicateRole {
        /// Duplicated role.
        role: DatabaseFileRole,
        /// File identity in the first entry.
        first_file_id: DatabaseFileId,
        /// File identity in the duplicate entry.
        duplicate_file_id: DatabaseFileId,
    },
    /// No entry supplied one required role.
    MissingRole {
        /// Missing role.
        role: DatabaseFileRole,
    },
    /// Two distinct roles reused one globally unique file identity.
    DuplicateFileIdentity {
        /// Reused file identity.
        file_id: DatabaseFileId,
        /// First role in stable role order.
        first_role: DatabaseFileRole,
        /// Second role in stable role order.
        second_role: DatabaseFileRole,
    },
}

impl fmt::Display for DatabaseCompositionIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateRole {
                role,
                first_file_id,
                duplicate_file_id,
            } => write!(
                formatter,
                "database composition has duplicate {role} roles with file identities {} and {}",
                first_file_id.get(),
                duplicate_file_id.get()
            ),
            Self::MissingRole { role } => {
                write!(formatter, "database composition is missing the {role} role")
            }
            Self::DuplicateFileIdentity {
                file_id,
                first_role,
                second_role,
            } => write!(
                formatter,
                "database composition reuses file identity {} for {first_role} and {second_role}",
                file_id.get()
            ),
        }
    }
}

impl Error for DatabaseCompositionIdentityError {}

/// Validated stable identity of one database storage composition.
///
/// This value is the exact cross-file observation an adapter can reconstruct
/// from successor child headers and their existing persistent WAL identities.
/// It intentionally excludes the manifest-only lifecycle generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseStorageIdentity {
    database_id: DatabaseId,
    persistent_log_id: PersistentLogId,
    wal_file_id: DatabaseFileId,
    page_store_file_id: DatabaseFileId,
    restart_checkpoint_file_id: DatabaseFileId,
}

impl DatabaseStorageIdentity {
    /// Validates exactly one globally distinct identity for every required role.
    pub fn new(
        database_id: DatabaseId,
        persistent_log_id: PersistentLogId,
        files: &[DatabaseFileIdentity],
    ) -> Result<Self, DatabaseCompositionIdentityError> {
        let (wal_file_id, page_store_file_id, restart_checkpoint_file_id) =
            validate_database_files(files)?;
        Ok(Self {
            database_id,
            persistent_log_id,
            wal_file_id,
            page_store_file_id,
            restart_checkpoint_file_id,
        })
    }

    /// Returns the logical database identity.
    #[must_use]
    pub const fn database_id(self) -> DatabaseId {
        self.database_id
    }

    /// Returns the persistent WAL lineage identity.
    #[must_use]
    pub const fn persistent_log_id(self) -> PersistentLogId {
        self.persistent_log_id
    }

    /// Returns the file identity assigned to `role`.
    #[must_use]
    pub const fn file_id(self, role: DatabaseFileRole) -> DatabaseFileId {
        match role {
            DatabaseFileRole::Wal => self.wal_file_id,
            DatabaseFileRole::PageStore => self.page_store_file_id,
            DatabaseFileRole::RestartCheckpoint => self.restart_checkpoint_file_id,
        }
    }

    /// Returns all role identities in stable role order.
    #[must_use]
    pub const fn ordered_files(self) -> [DatabaseFileIdentity; 3] {
        [
            DatabaseFileIdentity::new(DatabaseFileRole::Wal, self.wal_file_id),
            DatabaseFileIdentity::new(DatabaseFileRole::PageStore, self.page_store_file_id),
            DatabaseFileIdentity::new(
                DatabaseFileRole::RestartCheckpoint,
                self.restart_checkpoint_file_id,
            ),
        ]
    }

    /// Returns the exact stable identity one child header must persist.
    #[must_use]
    pub const fn file_header_identity(self, role: DatabaseFileRole) -> DatabaseFileHeaderIdentity {
        DatabaseFileHeaderIdentity::new(
            self.database_id,
            DatabaseFileIdentity::new(role, self.file_id(role)),
        )
    }

    /// Compares every physically observable storage identity in stable order.
    pub fn require_exact_match(
        self,
        actual: Self,
    ) -> Result<(), DatabaseCompositionIdentityMismatch> {
        if self.database_id != actual.database_id {
            return Err(DatabaseCompositionIdentityMismatch::DatabaseId {
                expected: self.database_id,
                actual: actual.database_id,
            });
        }
        for role in [
            DatabaseFileRole::Wal,
            DatabaseFileRole::PageStore,
            DatabaseFileRole::RestartCheckpoint,
        ] {
            let expected = self.file_id(role);
            let actual = actual.file_id(role);
            if expected != actual {
                return Err(DatabaseCompositionIdentityMismatch::FileId {
                    role,
                    expected,
                    actual,
                });
            }
        }
        if self.persistent_log_id != actual.persistent_log_id {
            return Err(DatabaseCompositionIdentityMismatch::PersistentLogId {
                expected: self.persistent_log_id,
                actual: actual.persistent_log_id,
            });
        }
        Ok(())
    }
}

/// Validated inert identity of one exact database storage composition.
///
/// This value contains no paths, handles, locks, decoded bytes, or lifecycle
/// authority. An outer adapter must independently validate persisted bytes and
/// opened objects before a later gate may create live authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseCompositionIdentity {
    database_id: DatabaseId,
    lifecycle_generation: DatabaseLifecycleGeneration,
    persistent_log_id: PersistentLogId,
    wal_file_id: DatabaseFileId,
    page_store_file_id: DatabaseFileId,
    restart_checkpoint_file_id: DatabaseFileId,
}

impl DatabaseCompositionIdentity {
    /// Validates exactly one globally distinct identity for every required role.
    pub fn new(
        database_id: DatabaseId,
        lifecycle_generation: DatabaseLifecycleGeneration,
        persistent_log_id: PersistentLogId,
        files: &[DatabaseFileIdentity],
    ) -> Result<Self, DatabaseCompositionIdentityError> {
        let storage_identity = DatabaseStorageIdentity::new(database_id, persistent_log_id, files)?;

        Ok(Self {
            database_id,
            lifecycle_generation,
            persistent_log_id,
            wal_file_id: storage_identity.wal_file_id,
            page_store_file_id: storage_identity.page_store_file_id,
            restart_checkpoint_file_id: storage_identity.restart_checkpoint_file_id,
        })
    }

    /// Returns the logical database identity.
    #[must_use]
    pub const fn database_id(self) -> DatabaseId {
        self.database_id
    }

    /// Returns the lifecycle generation selecting this composition.
    #[must_use]
    pub const fn lifecycle_generation(self) -> DatabaseLifecycleGeneration {
        self.lifecycle_generation
    }

    /// Returns the persistent WAL lineage identity.
    #[must_use]
    pub const fn persistent_log_id(self) -> PersistentLogId {
        self.persistent_log_id
    }

    /// Returns the file identity assigned to `role`.
    #[must_use]
    pub const fn file_id(self, role: DatabaseFileRole) -> DatabaseFileId {
        match role {
            DatabaseFileRole::Wal => self.wal_file_id,
            DatabaseFileRole::PageStore => self.page_store_file_id,
            DatabaseFileRole::RestartCheckpoint => self.restart_checkpoint_file_id,
        }
    }

    /// Returns all role identities in stable role order.
    #[must_use]
    pub const fn ordered_files(self) -> [DatabaseFileIdentity; 3] {
        [
            DatabaseFileIdentity::new(DatabaseFileRole::Wal, self.wal_file_id),
            DatabaseFileIdentity::new(DatabaseFileRole::PageStore, self.page_store_file_id),
            DatabaseFileIdentity::new(
                DatabaseFileRole::RestartCheckpoint,
                self.restart_checkpoint_file_id,
            ),
        ]
    }

    /// Returns the stable storage identity independently observable from children.
    #[must_use]
    pub const fn storage_identity(self) -> DatabaseStorageIdentity {
        DatabaseStorageIdentity {
            database_id: self.database_id,
            persistent_log_id: self.persistent_log_id,
            wal_file_id: self.wal_file_id,
            page_store_file_id: self.page_store_file_id,
            restart_checkpoint_file_id: self.restart_checkpoint_file_id,
        }
    }

    /// Returns the stable identity one child header must persist.
    #[must_use]
    pub const fn file_header_identity(self, role: DatabaseFileRole) -> DatabaseFileHeaderIdentity {
        self.storage_identity().file_header_identity(role)
    }

    /// Produces the same composition at the exact next lifecycle generation.
    pub fn next_generation(self) -> Result<Self, DatabaseLifecycleGenerationExhausted> {
        Ok(Self {
            lifecycle_generation: self.lifecycle_generation.checked_next()?,
            ..self
        })
    }

    /// Requires and installs one caller-observed exact successor generation.
    pub fn with_successor_generation(
        self,
        proposed: DatabaseLifecycleGeneration,
    ) -> Result<Self, DatabaseLifecycleGenerationTransitionError> {
        self.lifecycle_generation.require_successor(proposed)?;
        Ok(Self {
            lifecycle_generation: proposed,
            ..self
        })
    }

    /// Compares database, file-role, and WAL identities while ignoring generation.
    ///
    /// This is the stable-storage comparison used before a lifecycle successor
    /// checks its generation separately.
    pub fn require_same_storage_identity(
        self,
        actual: Self,
    ) -> Result<(), DatabaseCompositionIdentityMismatch> {
        self.storage_identity()
            .require_exact_match(actual.storage_identity())
    }

    /// Compares every identity in stable field order.
    pub fn require_exact_match(
        self,
        actual: Self,
    ) -> Result<(), DatabaseCompositionIdentityMismatch> {
        if self.database_id != actual.database_id {
            return Err(DatabaseCompositionIdentityMismatch::DatabaseId {
                expected: self.database_id,
                actual: actual.database_id,
            });
        }
        if self.lifecycle_generation != actual.lifecycle_generation {
            return Err(DatabaseCompositionIdentityMismatch::LifecycleGeneration {
                expected: self.lifecycle_generation,
                actual: actual.lifecycle_generation,
            });
        }
        for role in [
            DatabaseFileRole::Wal,
            DatabaseFileRole::PageStore,
            DatabaseFileRole::RestartCheckpoint,
        ] {
            let expected = self.file_id(role);
            let actual = actual.file_id(role);
            if expected != actual {
                return Err(DatabaseCompositionIdentityMismatch::FileId {
                    role,
                    expected,
                    actual,
                });
            }
        }
        if self.persistent_log_id != actual.persistent_log_id {
            return Err(DatabaseCompositionIdentityMismatch::PersistentLogId {
                expected: self.persistent_log_id,
                actual: actual.persistent_log_id,
            });
        }
        Ok(())
    }
}

fn validate_database_files(
    files: &[DatabaseFileIdentity],
) -> Result<(DatabaseFileId, DatabaseFileId, DatabaseFileId), DatabaseCompositionIdentityError> {
    let mut wal_file_id = None;
    let mut page_store_file_id = None;
    let mut restart_checkpoint_file_id = None;

    for file in files {
        let slot = match file.role {
            DatabaseFileRole::Wal => &mut wal_file_id,
            DatabaseFileRole::PageStore => &mut page_store_file_id,
            DatabaseFileRole::RestartCheckpoint => &mut restart_checkpoint_file_id,
        };
        if let Some(first_file_id) = *slot {
            return Err(DatabaseCompositionIdentityError::DuplicateRole {
                role: file.role,
                first_file_id,
                duplicate_file_id: file.file_id,
            });
        }
        *slot = Some(file.file_id);
    }

    let Some(wal_file_id) = wal_file_id else {
        return Err(DatabaseCompositionIdentityError::MissingRole {
            role: DatabaseFileRole::Wal,
        });
    };
    let Some(page_store_file_id) = page_store_file_id else {
        return Err(DatabaseCompositionIdentityError::MissingRole {
            role: DatabaseFileRole::PageStore,
        });
    };
    let Some(restart_checkpoint_file_id) = restart_checkpoint_file_id else {
        return Err(DatabaseCompositionIdentityError::MissingRole {
            role: DatabaseFileRole::RestartCheckpoint,
        });
    };

    if wal_file_id == page_store_file_id {
        return Err(DatabaseCompositionIdentityError::DuplicateFileIdentity {
            file_id: wal_file_id,
            first_role: DatabaseFileRole::Wal,
            second_role: DatabaseFileRole::PageStore,
        });
    }
    if wal_file_id == restart_checkpoint_file_id {
        return Err(DatabaseCompositionIdentityError::DuplicateFileIdentity {
            file_id: wal_file_id,
            first_role: DatabaseFileRole::Wal,
            second_role: DatabaseFileRole::RestartCheckpoint,
        });
    }
    if page_store_file_id == restart_checkpoint_file_id {
        return Err(DatabaseCompositionIdentityError::DuplicateFileIdentity {
            file_id: page_store_file_id,
            first_role: DatabaseFileRole::PageStore,
            second_role: DatabaseFileRole::RestartCheckpoint,
        });
    }

    Ok((wal_file_id, page_store_file_id, restart_checkpoint_file_id))
}

/// First exact contradiction between selected and observed composition identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseCompositionIdentityMismatch {
    /// The observed database is foreign.
    DatabaseId {
        /// Selected database identity.
        expected: DatabaseId,
        /// Observed database identity.
        actual: DatabaseId,
    },
    /// The observed lifecycle generation is stale or foreign.
    LifecycleGeneration {
        /// Selected lifecycle generation.
        expected: DatabaseLifecycleGeneration,
        /// Observed lifecycle generation.
        actual: DatabaseLifecycleGeneration,
    },
    /// One required file role identifies a different file.
    FileId {
        /// Contradictory role.
        role: DatabaseFileRole,
        /// Selected file identity.
        expected: DatabaseFileId,
        /// Observed file identity.
        actual: DatabaseFileId,
    },
    /// The observed WAL lineage is foreign.
    PersistentLogId {
        /// Selected persistent WAL identity.
        expected: PersistentLogId,
        /// Observed persistent WAL identity.
        actual: PersistentLogId,
    },
}

impl fmt::Display for DatabaseCompositionIdentityMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseId { expected, actual } => write!(
                formatter,
                "database identity mismatch: expected {}, observed {}",
                expected.get(),
                actual.get()
            ),
            Self::LifecycleGeneration { expected, actual } => write!(
                formatter,
                "database lifecycle generation mismatch: expected {}, observed {}",
                expected.get(),
                actual.get()
            ),
            Self::FileId {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "{role} identity mismatch: expected {}, observed {}",
                expected.get(),
                actual.get()
            ),
            Self::PersistentLogId { expected, actual } => write!(
                formatter,
                "persistent WAL identity mismatch: expected {}, observed {}",
                expected.get(),
                actual.get()
            ),
        }
    }
}

impl Error for DatabaseCompositionIdentityMismatch {}

/// Nonzero required persistent-format version for one database file role.
///
/// The numeric value is inert. Each outer adapter decides whether it supports
/// the selected requirement and compares it with the opened child file.
///
/// ```compile_fail
/// use std::num::NonZeroU16;
/// use ntsql_database::DatabaseStorageFormatVersion;
///
/// let forged = DatabaseStorageFormatVersion(NonZeroU16::MIN);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatabaseStorageFormatVersion(NonZeroU16);

impl DatabaseStorageFormatVersion {
    /// Wraps one nonzero repository-owned format version.
    #[must_use]
    pub const fn new(value: u16) -> Option<Self> {
        match NonZeroU16::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric format version for adapter comparison.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// Exact required child-format versions for one database composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseStorageFormatRequirements {
    wal: DatabaseStorageFormatVersion,
    page_store: DatabaseStorageFormatVersion,
    restart_checkpoint: DatabaseStorageFormatVersion,
}

impl DatabaseStorageFormatRequirements {
    /// Binds one nonzero required version to every fixed file role.
    #[must_use]
    pub const fn new(
        wal: DatabaseStorageFormatVersion,
        page_store: DatabaseStorageFormatVersion,
        restart_checkpoint: DatabaseStorageFormatVersion,
    ) -> Self {
        Self {
            wal,
            page_store,
            restart_checkpoint,
        }
    }

    /// Returns the required persistent-format version for `role`.
    #[must_use]
    pub const fn version(self, role: DatabaseFileRole) -> DatabaseStorageFormatVersion {
        match role {
            DatabaseFileRole::Wal => self.wal,
            DatabaseFileRole::PageStore => self.page_store,
            DatabaseFileRole::RestartCheckpoint => self.restart_checkpoint,
        }
    }
}

/// Required database feature bits not understood by this repository version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseRequiredFeaturesError {
    /// Exact complete decoded bit set.
    pub actual: u64,
    /// Exact subset that this repository version does not understand.
    pub unknown: u64,
}

impl fmt::Display for DatabaseRequiredFeaturesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "database required feature bits {:#018x} contain unknown bits {:#018x}",
            self.actual, self.unknown
        )
    }
}

impl Error for DatabaseRequiredFeaturesError {}

/// Validated required feature set for one database manifest.
///
/// Version 1 defines no required features. Keeping this checked type separate
/// prevents a future reader from silently ignoring a bit it does not implement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DatabaseRequiredFeatures(u64);

impl DatabaseRequiredFeatures {
    const KNOWN_BITS: u64 = 0;

    /// No required database features.
    pub const NONE: Self = Self(0);

    /// Validates that every required bit is understood by this repository version.
    pub const fn from_bits(bits: u64) -> Result<Self, DatabaseRequiredFeaturesError> {
        let unknown = bits & !Self::KNOWN_BITS;
        if unknown != 0 {
            return Err(DatabaseRequiredFeaturesError {
                actual: bits,
                unknown,
            });
        }
        Ok(Self(bits))
    }

    /// Returns the canonical required-feature bit set.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }
}

/// Persisted lifecycle state understood by manifest format version 1.
///
/// Later clean-close and tombstone issues must add their states together with
/// the evidence fields and version policy that make those states meaningful.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DatabaseManifestLifecycleState {
    /// Startup must complete the approved recovery path before live release.
    RecoveryRequired,
}

impl fmt::Display for DatabaseManifestLifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecoveryRequired => formatter.write_str("recovery required"),
        }
    }
}

/// Validated inert content of one repository-owned database manifest.
///
/// A manifest contains identity and compatibility requirements only. It owns no
/// decoded bytes, path, lock, opened adapter, recovery evidence, or live
/// authority.
///
/// ```compile_fail
/// use ntsql_database::{DatabaseManifest, LiveDatabase};
///
/// fn cannot_promote_manifest<Owner>(manifest: DatabaseManifest) -> LiveDatabase<Owner> {
///     manifest.into()
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseManifest {
    composition_identity: DatabaseCompositionIdentity,
    lifecycle_state: DatabaseManifestLifecycleState,
    storage_formats: DatabaseStorageFormatRequirements,
    required_features: DatabaseRequiredFeatures,
}

impl DatabaseManifest {
    /// Constructs the only lifecycle state supported by manifest format version 1.
    #[must_use]
    pub const fn recovery_required(
        composition_identity: DatabaseCompositionIdentity,
        storage_formats: DatabaseStorageFormatRequirements,
        required_features: DatabaseRequiredFeatures,
    ) -> Self {
        Self {
            composition_identity,
            lifecycle_state: DatabaseManifestLifecycleState::RecoveryRequired,
            storage_formats,
            required_features,
        }
    }

    /// Returns the exact database and child storage identities.
    #[must_use]
    pub const fn composition_identity(self) -> DatabaseCompositionIdentity {
        self.composition_identity
    }

    /// Returns the inert persisted lifecycle state.
    #[must_use]
    pub const fn lifecycle_state(self) -> DatabaseManifestLifecycleState {
        self.lifecycle_state
    }

    /// Returns the exact required child-format versions.
    #[must_use]
    pub const fn storage_formats(self) -> DatabaseStorageFormatRequirements {
        self.storage_formats
    }

    /// Returns the validated required feature set.
    #[must_use]
    pub const fn required_features(self) -> DatabaseRequiredFeatures {
        self.required_features
    }

    /// Produces the same recovery-required manifest at the exact next generation.
    pub fn next_recovery_required(self) -> Result<Self, DatabaseLifecycleGenerationExhausted> {
        Ok(Self::recovery_required(
            self.composition_identity.next_generation()?,
            self.storage_formats,
            self.required_features,
        ))
    }

    /// Validates this manifest as the exact next generation after `previous`.
    ///
    /// This comparison is explicit because decoding one isolated frame has no
    /// prior generation against which it could detect regression.
    pub fn require_successor_of(
        self,
        previous: Self,
    ) -> Result<(), DatabaseManifestSuccessorError> {
        previous
            .composition_identity
            .require_same_storage_identity(self.composition_identity)
            .map_err(DatabaseManifestSuccessorError::CompositionIdentity)?;
        previous
            .composition_identity
            .lifecycle_generation()
            .require_successor(self.composition_identity.lifecycle_generation())
            .map_err(DatabaseManifestSuccessorError::LifecycleGeneration)?;
        for role in [
            DatabaseFileRole::Wal,
            DatabaseFileRole::PageStore,
            DatabaseFileRole::RestartCheckpoint,
        ] {
            let expected = previous.storage_formats.version(role);
            let actual = self.storage_formats.version(role);
            if expected != actual {
                return Err(DatabaseManifestSuccessorError::StorageFormatVersion {
                    role,
                    expected,
                    actual,
                });
            }
        }
        if previous.required_features != self.required_features {
            return Err(DatabaseManifestSuccessorError::RequiredFeatures {
                expected: previous.required_features,
                actual: self.required_features,
            });
        }
        Ok(())
    }
}

/// Rejection of a manifest claimed as one exact lifecycle successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseManifestSuccessorError {
    /// Database, child-file, or persistent-WAL identity changed.
    CompositionIdentity(DatabaseCompositionIdentityMismatch),
    /// The lifecycle generation regressed, skipped, or exhausted.
    LifecycleGeneration(DatabaseLifecycleGenerationTransitionError),
    /// One child persistent-format requirement changed without migration.
    StorageFormatVersion {
        /// Changed file role.
        role: DatabaseFileRole,
        /// Previously selected required version.
        expected: DatabaseStorageFormatVersion,
        /// Proposed required version.
        actual: DatabaseStorageFormatVersion,
    },
    /// Required feature bits changed without migration.
    RequiredFeatures {
        /// Previously selected required feature set.
        expected: DatabaseRequiredFeatures,
        /// Proposed required feature set.
        actual: DatabaseRequiredFeatures,
    },
}

impl fmt::Display for DatabaseManifestSuccessorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompositionIdentity(source) => {
                write!(formatter, "database manifest identity changed: {source}")
            }
            Self::LifecycleGeneration(source) => {
                write!(
                    formatter,
                    "database manifest generation is invalid: {source}"
                )
            }
            Self::StorageFormatVersion {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "database manifest {role} format changed from {} to {}",
                expected.get(),
                actual.get()
            ),
            Self::RequiredFeatures { expected, actual } => write!(
                formatter,
                "database manifest required features changed from {:#018x} to {:#018x}",
                expected.bits(),
                actual.bits()
            ),
        }
    }
}

impl Error for DatabaseManifestSuccessorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CompositionIdentity(source) => Some(source),
            Self::LifecycleGeneration(source) => Some(source),
            Self::StorageFormatVersion { .. } | Self::RequiredFeatures { .. } => None,
        }
    }
}

/// Observable label for a typed database lifecycle owner.
///
/// This enum is descriptive only. It cannot construct or advance an owner.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DatabaseLifecycleStage {
    /// No manifest identity has been selected.
    Unbound,
    /// One inert manifest composition has been selected.
    ManifestSelected,
    /// The exact opened composition is bound but recovery is incomplete.
    RecoveryRequired,
    /// Recovery is complete and ordinary live work is permitted.
    Live,
    /// A live owner has been consumed into an orderly close attempt.
    ClosePending,
    /// The exact composition reached an orderly terminal close.
    Closed,
    /// A closed owner has been consumed into a drop attempt.
    DropPending,
    /// The database reached the terminal dropped state.
    Dropped,
}

/// Owning state before one manifest identity has been selected.
///
/// The generic owner may later be a database-wide lock owner. It remains
/// private and cannot be extracted from any successful staged state.
#[must_use = "unbound database ownership must be selected or dropped"]
pub struct UnboundDatabase<Owner> {
    owner: Owner,
    expected_database_id: DatabaseId,
}

impl<Owner> UnboundDatabase<Owner> {
    /// Takes one outer owner while retaining the expected logical database.
    pub const fn new(owner: Owner, expected_database_id: DatabaseId) -> Self {
        Self {
            owner,
            expected_database_id,
        }
    }

    /// Returns the expected logical database identity.
    #[must_use]
    pub const fn expected_database_id(&self) -> DatabaseId {
        self.expected_database_id
    }

    /// Returns this owner's lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        DatabaseLifecycleStage::Unbound
    }

    /// Selects one validated inert manifest identity for the expected database.
    pub fn select_manifest(
        self,
        selected_identity: DatabaseCompositionIdentity,
    ) -> Result<ManifestSelectedDatabase<Owner>, Box<DatabaseManifestSelectionError<Owner>>> {
        if self.expected_database_id != selected_identity.database_id {
            let expected = self.expected_database_id;
            return Err(Box::new(DatabaseManifestSelectionError {
                database: self,
                selected_identity,
                reason: DatabaseManifestSelectionRejection::ForeignDatabaseId {
                    expected,
                    actual: selected_identity.database_id,
                },
            }));
        }
        Ok(ManifestSelectedDatabase {
            owner: self.owner,
            identity: selected_identity,
        })
    }
}

impl<Owner> fmt::Debug for UnboundDatabase<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnboundDatabase")
            .field("expected_database_id", &self.expected_database_id)
            .finish_non_exhaustive()
    }
}

/// Reason one inert manifest cannot be selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseManifestSelectionRejection {
    /// The manifest names a database other than the requested database.
    ForeignDatabaseId {
        /// Requested database identity.
        expected: DatabaseId,
        /// Manifest database identity.
        actual: DatabaseId,
    },
}

impl fmt::Display for DatabaseManifestSelectionRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignDatabaseId { expected, actual } => write!(
                formatter,
                "manifest database identity {} does not match requested database {}",
                actual.get(),
                expected.get()
            ),
        }
    }
}

impl Error for DatabaseManifestSelectionRejection {}

/// Failed manifest selection retaining the exact unbound owner and evidence.
#[must_use = "failed manifest selection retains the unbound database owner"]
pub struct DatabaseManifestSelectionError<Owner> {
    database: UnboundDatabase<Owner>,
    selected_identity: DatabaseCompositionIdentity,
    reason: DatabaseManifestSelectionRejection,
}

impl<Owner> DatabaseManifestSelectionError<Owner> {
    /// Returns the exact rejection.
    #[must_use]
    pub const fn reason(&self) -> &DatabaseManifestSelectionRejection {
        &self.reason
    }

    /// Returns the rejected inert manifest identity.
    #[must_use]
    pub const fn selected_identity(&self) -> DatabaseCompositionIdentity {
        self.selected_identity
    }

    /// Releases the retained unbound owner and rejected inert identity together.
    pub fn into_parts(self) -> (UnboundDatabase<Owner>, DatabaseCompositionIdentity) {
        (self.database, self.selected_identity)
    }
}

impl<Owner> fmt::Debug for DatabaseManifestSelectionError<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseManifestSelectionError")
            .field("selected_identity", &self.selected_identity)
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl<Owner> fmt::Display for DatabaseManifestSelectionError<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "database manifest selection failed: {}",
            self.reason
        )
    }
}

impl<Owner> Error for DatabaseManifestSelectionError<Owner> {}

/// Owner paired with one selected inert manifest composition.
///
/// ```compile_fail
/// use ntsql_database::ManifestSelectedDatabase;
///
/// fn cannot_clone<Owner>(selected: ManifestSelectedDatabase<Owner>) {
///     let first = selected;
///     let second = selected;
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_database::ManifestSelectedDatabase;
///
/// fn cannot_extract_owner<Owner>(selected: ManifestSelectedDatabase<Owner>) -> Owner {
///     selected.into_owner()
/// }
/// ```
#[must_use = "selected manifest ownership must bind exact storage or be dropped"]
pub struct ManifestSelectedDatabase<Owner> {
    owner: Owner,
    identity: DatabaseCompositionIdentity,
}

impl<Owner> ManifestSelectedDatabase<Owner> {
    /// Returns the selected inert composition identity.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.identity
    }

    /// Returns this owner's lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        DatabaseLifecycleStage::ManifestSelected
    }

    /// Binds this owner only when child storage reports its exact stable identity.
    ///
    /// Lifecycle generation remains the selected manifest's publication
    /// coordinate and is not synthesized into the child observation.
    pub fn bind_observed_storage(
        self,
        observed_identity: DatabaseStorageIdentity,
    ) -> Result<RecoveryRequiredDatabase<Owner>, Box<DatabaseStorageBindingError<Owner>>> {
        if let Err(reason) = self
            .identity
            .storage_identity()
            .require_exact_match(observed_identity)
        {
            return Err(Box::new(DatabaseStorageBindingError {
                database: self,
                observed_identity,
                reason,
            }));
        }
        Ok(RecoveryRequiredDatabase {
            _owner: self.owner,
            identity: self.identity,
        })
    }
}

impl<Owner> fmt::Debug for ManifestSelectedDatabase<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManifestSelectedDatabase")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// Failed storage-identity binding retaining selected ownership and evidence.
#[must_use = "failed storage binding retains the selected database owner"]
pub struct DatabaseStorageBindingError<Owner> {
    database: ManifestSelectedDatabase<Owner>,
    observed_identity: DatabaseStorageIdentity,
    reason: DatabaseCompositionIdentityMismatch,
}

impl<Owner> DatabaseStorageBindingError<Owner> {
    /// Returns the first stable storage contradiction.
    #[must_use]
    pub const fn reason(&self) -> &DatabaseCompositionIdentityMismatch {
        &self.reason
    }

    /// Returns the rejected observed composition identity.
    #[must_use]
    pub const fn observed_identity(&self) -> DatabaseStorageIdentity {
        self.observed_identity
    }

    /// Releases the retained selected owner and observed inert identity together.
    pub fn into_parts(self) -> (ManifestSelectedDatabase<Owner>, DatabaseStorageIdentity) {
        (self.database, self.observed_identity)
    }
}

impl<Owner> fmt::Debug for DatabaseStorageBindingError<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseStorageBindingError")
            .field("observed_identity", &self.observed_identity)
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl<Owner> fmt::Display for DatabaseStorageBindingError<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "database storage binding failed: {}",
            self.reason
        )
    }
}

impl<Owner> Error for DatabaseStorageBindingError<Owner> {}

macro_rules! define_owned_database_state {
    ($(#[$attribute:meta])* $name:ident, $stage:expr) => {
        $(#[$attribute])*
        pub struct $name<Owner> {
            _owner: Owner,
            identity: DatabaseCompositionIdentity,
        }

        impl<Owner> $name<Owner> {
            /// Returns the exact retained composition identity.
            #[must_use]
            pub const fn identity(&self) -> DatabaseCompositionIdentity {
                self.identity
            }

            /// Returns this owner's lifecycle stage.
            #[must_use]
            pub const fn stage(&self) -> DatabaseLifecycleStage {
                $stage
            }
        }

        impl<Owner> fmt::Debug for $name<Owner> {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($name))
                    .field("identity", &self.identity)
                    .finish_non_exhaustive()
            }
        }
    };
}

define_owned_database_state!(
    /// Exact database composition that cannot release live authority before recovery.
    ///
    /// ```compile_fail
    /// use ntsql_database::{LiveDatabase, RecoveryRequiredDatabase};
    ///
    /// fn cannot_skip_recovery<Owner>(
    ///     recovery: RecoveryRequiredDatabase<Owner>,
    /// ) -> LiveDatabase<Owner> {
    ///     recovery.into()
    /// }
    /// ```
    #[must_use = "recovery-required database ownership must complete recovery or be dropped"]
    RecoveryRequiredDatabase,
    DatabaseLifecycleStage::RecoveryRequired
);

define_owned_database_state!(
    /// Recovery-complete owner permitted to perform ordinary live work.
    ///
    /// Construction is intentionally private until the recovery handoff owns the
    /// exact effectful completion proof.
    ///
    /// ```compile_fail
    /// use ntsql_database::{DatabaseCompositionIdentity, LiveDatabase};
    ///
    /// fn cannot_construct_live<Owner>(
    ///     owner: Owner,
    ///     identity: DatabaseCompositionIdentity,
    /// ) -> LiveDatabase<Owner> {
    ///     LiveDatabase {
    ///         _owner: owner,
    ///         identity,
    ///     }
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// use ntsql_database::LiveDatabase;
    ///
    /// fn cannot_extract_recovered_owner<Owner>(live: LiveDatabase<Owner>) -> Owner {
    ///     let LiveDatabase { _owner, .. } = live;
    ///     _owner
    /// }
    /// ```
    #[must_use = "live database ownership must be closed, abandoned, or dropped"]
    LiveDatabase,
    DatabaseLifecycleStage::Live
);

/// Adapter-owned recovery operation accepted by the database live gate.
///
/// Implementations consume one exact database owner and must return the
/// private-constructible transaction completion owner produced by the approved
/// selected-restart path. For a concrete repository adapter, Rust coherence
/// prevents downstream crates from replacing this implementation.
pub trait DatabaseRecoveryOwner<Input, const N: usize>: Sized {
    /// WAL adapter retained after successful restart completion.
    type Source: DurableTransactionRestartAnalysisSource<N>;
    /// Page-store adapter retained after successful restart completion.
    type Store: DurablePageStoreSnapshotSource<N>;
    /// Completeness-checkpoint adapter retained by the completed restart.
    type CheckpointSource;
    /// Database-wide ownership retained beside the transaction composition.
    type RetainedOwner;
    /// Exact adapter recovery failure retaining every acquired owner.
    type Failure;

    /// Consumes this recovery-required owner through the approved restart path.
    fn complete_database_recovery(
        self,
        input: Input,
    ) -> DatabaseRecoveryOperationResult<Self, Input, N>;
}

/// Result of one concrete adapter's approved transaction-recovery operation.
pub type DatabaseRecoveryOperationResult<Owner, Input, const N: usize> = Result<
    (
        <Owner as DatabaseRecoveryOwner<Input, N>>::RetainedOwner,
        WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
            <Owner as DatabaseRecoveryOwner<Input, N>>::Source,
            <Owner as DatabaseRecoveryOwner<Input, N>>::Store,
            <Owner as DatabaseRecoveryOwner<Input, N>>::CheckpointSource,
            N,
        >,
    ),
    <Owner as DatabaseRecoveryOwner<Input, N>>::Failure,
>;

/// Exact database-wide owner paired with completed transaction recovery.
///
/// Construction is private to [`RecoveryRequiredDatabase::complete_recovery`].
/// The retained transaction owner still owns its checkpoint source and
/// non-authorizing WAL retention analysis.
#[must_use = "recovered ownership must remain inside the live database typestate"]
pub struct RecoveredDatabaseOwnership<OuterOwner, Source, Store, CheckpointSource, const N: usize>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    outer_owner: OuterOwner,
    transaction: WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
        Source,
        Store,
        CheckpointSource,
        N,
    >,
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize>
    RecoveredDatabaseOwnership<OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    /// Borrows the retained database-wide owner.
    #[must_use]
    pub const fn outer_owner(&self) -> &OuterOwner {
        &self.outer_owner
    }

    /// Borrows the exact completed transaction recovery owner.
    pub const fn transaction(
        &self,
    ) -> &WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
        Source,
        Store,
        CheckpointSource,
        N,
    > {
        &self.transaction
    }

    /// Borrows the completed transaction owner for ordinary live work.
    pub const fn transaction_mut(
        &mut self,
    ) -> &mut WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
        Source,
        Store,
        CheckpointSource,
        N,
    > {
        &mut self.transaction
    }
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize> fmt::Debug
    for RecoveredDatabaseOwnership<OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveredDatabaseOwnership")
            .field(
                "persistent_log_id",
                &self.transaction.completion_evidence().persistent_log_id(),
            )
            .field("outer_owner", &format_args!("<retained>"))
            .field("transaction", &format_args!("<retained>"))
            .finish()
    }
}

/// Transaction completion identified a different persistent WAL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseRecoveryEvidenceMismatch {
    expected: PersistentLogId,
    actual: PersistentLogId,
}

impl DatabaseRecoveryEvidenceMismatch {
    /// Returns the manifest-selected persistent WAL identity.
    #[must_use]
    pub const fn expected(&self) -> PersistentLogId {
        self.expected
    }

    /// Returns the identity reported by exact transaction completion evidence.
    #[must_use]
    pub const fn actual(&self) -> PersistentLogId {
        self.actual
    }
}

impl fmt::Display for DatabaseRecoveryEvidenceMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "completed recovery persistent log ID {} does not match selected database persistent log ID {}",
            self.actual.get(),
            self.expected.get()
        )
    }
}

impl Error for DatabaseRecoveryEvidenceMismatch {}

enum FailedDatabaseRecoveryState<Failure, RecoveredOwner> {
    Operation(Failure),
    Evidence {
        _owner: RecoveredOwner,
        error: DatabaseRecoveryEvidenceMismatch,
    },
}

struct FailedDatabaseRecoveryInner<Failure, RecoveredOwner> {
    identity: DatabaseCompositionIdentity,
    state: FailedDatabaseRecoveryState<Failure, RecoveredOwner>,
}

/// Borrowed cause of one terminal database recovery failure.
#[derive(Debug)]
pub enum DatabaseRecoveryFailureCause<'failure, Failure> {
    /// The concrete adapter recovery operation failed.
    Operation(&'failure Failure),
    /// Exact transaction completion evidence identified another WAL.
    Evidence(&'failure DatabaseRecoveryEvidenceMismatch),
}

/// Terminal database recovery failure retaining every operation owner.
///
/// There is no retry or owner extraction. Resolution requires dropping this
/// value, correcting any external cause, and reopening the complete database.
#[must_use = "failed database recovery retains all ownership until dropped"]
pub struct FailedDatabaseRecovery<
    Failure,
    OuterOwner,
    Source,
    Store,
    CheckpointSource,
    const N: usize,
> where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    inner: Box<
        FailedDatabaseRecoveryInner<
            Failure,
            RecoveredDatabaseOwnership<OuterOwner, Source, Store, CheckpointSource, N>,
        >,
    >,
}

impl<Failure, OuterOwner, Source, Store, CheckpointSource, const N: usize>
    FailedDatabaseRecovery<Failure, OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    /// Returns the selected database composition retained by the failed owner.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.inner.identity
    }

    /// Returns the exact operation or evidence cause without releasing ownership.
    #[must_use]
    pub const fn cause(&self) -> DatabaseRecoveryFailureCause<'_, Failure> {
        match &self.inner.state {
            FailedDatabaseRecoveryState::Operation(failure) => {
                DatabaseRecoveryFailureCause::Operation(failure)
            }
            FailedDatabaseRecoveryState::Evidence { error, .. } => {
                DatabaseRecoveryFailureCause::Evidence(error)
            }
        }
    }
}

impl<Failure, OuterOwner, Source, Store, CheckpointSource, const N: usize> fmt::Debug
    for FailedDatabaseRecovery<Failure, OuterOwner, Source, Store, CheckpointSource, N>
where
    Failure: fmt::Debug,
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FailedDatabaseRecovery")
            .field("identity", &self.identity())
            .field("cause", &self.cause())
            .finish_non_exhaustive()
    }
}

/// Result of the only database RecoveryRequired-to-Live transition.
pub type DatabaseRecoveryResult<Owner, Input, const N: usize> = Result<
    LiveDatabase<
        RecoveredDatabaseOwnership<
            <Owner as DatabaseRecoveryOwner<Input, N>>::RetainedOwner,
            <Owner as DatabaseRecoveryOwner<Input, N>>::Source,
            <Owner as DatabaseRecoveryOwner<Input, N>>::Store,
            <Owner as DatabaseRecoveryOwner<Input, N>>::CheckpointSource,
            N,
        >,
    >,
    FailedDatabaseRecovery<
        <Owner as DatabaseRecoveryOwner<Input, N>>::Failure,
        <Owner as DatabaseRecoveryOwner<Input, N>>::RetainedOwner,
        <Owner as DatabaseRecoveryOwner<Input, N>>::Source,
        <Owner as DatabaseRecoveryOwner<Input, N>>::Store,
        <Owner as DatabaseRecoveryOwner<Input, N>>::CheckpointSource,
        N,
    >,
>;

impl<Owner> RecoveryRequiredDatabase<Owner> {
    /// Completes the exact adapter recovery operation before releasing Live.
    ///
    /// The adapter operation is selected by the concrete owner type rather than
    /// a caller-supplied success closure. Its completed transaction owner is
    /// retained and its persistent identity is cross-checked before the private
    /// Live constructor is reached.
    pub fn complete_recovery<Input, const N: usize>(
        self,
        input: Input,
    ) -> DatabaseRecoveryResult<Owner, Input, N>
    where
        Owner: DatabaseRecoveryOwner<Input, N>,
    {
        let Self {
            _owner: owner,
            identity,
        } = self;
        let (outer_owner, transaction) = match owner.complete_database_recovery(input) {
            Ok(recovered) => recovered,
            Err(failure) => {
                return Err(FailedDatabaseRecovery {
                    inner: Box::new(FailedDatabaseRecoveryInner {
                        identity,
                        state: FailedDatabaseRecoveryState::Operation(failure),
                    }),
                });
            }
        };
        let actual = transaction.completion_evidence().persistent_log_id();
        let recovered = RecoveredDatabaseOwnership {
            outer_owner,
            transaction,
        };
        let expected = identity.persistent_log_id();
        if actual != expected {
            return Err(FailedDatabaseRecovery {
                inner: Box::new(FailedDatabaseRecoveryInner {
                    identity,
                    state: FailedDatabaseRecoveryState::Evidence {
                        _owner: recovered,
                        error: DatabaseRecoveryEvidenceMismatch { expected, actual },
                    },
                }),
            });
        }
        Ok(LiveDatabase {
            _owner: recovered,
            identity,
        })
    }
}

impl<Owner> LiveDatabase<Owner> {
    /// Borrows the exact recovery-complete owner for inspection.
    #[must_use]
    pub const fn owner(&self) -> &Owner {
        &self._owner
    }

    /// Borrows the exact recovery-complete owner for ordinary live work.
    pub const fn owner_mut(&mut self) -> &mut Owner {
        &mut self._owner
    }
}

define_owned_database_state!(
    /// Live owner consumed into an orderly close attempt.
    ///
    /// Construction remains private until the close protocol owns an effectful
    /// transition and outcome-indeterminate failure state.
    #[must_use = "close-pending database ownership must resolve or be dropped"]
    ClosePendingDatabase,
    DatabaseLifecycleStage::ClosePending
);

define_owned_database_state!(
    /// Exact composition that completed an orderly terminal close.
    ///
    /// Construction remains private until a synchronized clean-close certificate
    /// exists.
    #[must_use = "closed database ownership must be reopened, dropped, or released"]
    ClosedDatabase,
    DatabaseLifecycleStage::Closed
);

define_owned_database_state!(
    /// Closed owner consumed into an exact drop attempt.
    ///
    /// Construction remains private until the tombstone protocol owns an
    /// effectful transition and outcome-indeterminate failure state.
    #[must_use = "drop-pending database ownership must resolve or be dropped"]
    DropPendingDatabase,
    DatabaseLifecycleStage::DropPending
);

/// Terminal inert identity of one completed drop.
///
/// Construction remains private until exact tombstone removal is synchronized.
#[must_use]
pub struct DroppedDatabase {
    database_id: DatabaseId,
    final_generation: DatabaseLifecycleGeneration,
}

impl DroppedDatabase {
    /// Returns the identity of the dropped database.
    #[must_use]
    pub const fn database_id(&self) -> DatabaseId {
        self.database_id
    }

    /// Returns the final lifecycle generation consumed by drop.
    #[must_use]
    pub const fn final_generation(&self) -> DatabaseLifecycleGeneration {
        self.final_generation
    }

    /// Returns this terminal lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        DatabaseLifecycleStage::Dropped
    }
}

impl fmt::Debug for DroppedDatabase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DroppedDatabase")
            .field("database_id", &self.database_id)
            .field("final_generation", &self.final_generation)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestValueError(&'static str);

    impl fmt::Display for TestValueError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for TestValueError {}

    fn database_id(value: u128) -> Result<DatabaseId, TestValueError> {
        DatabaseId::new(value).ok_or(TestValueError("test database ID must be nonzero"))
    }

    fn file_id(value: u128) -> Result<DatabaseFileId, TestValueError> {
        DatabaseFileId::new(value).ok_or(TestValueError("test file ID must be nonzero"))
    }

    fn generation(value: u64) -> Result<DatabaseLifecycleGeneration, TestValueError> {
        DatabaseLifecycleGeneration::new(value)
            .ok_or(TestValueError("test generation must be nonzero"))
    }

    fn log_id(value: u128) -> Result<PersistentLogId, TestValueError> {
        PersistentLogId::new(value).ok_or(TestValueError("test log ID must be nonzero"))
    }

    fn format_version(value: u16) -> Result<DatabaseStorageFormatVersion, TestValueError> {
        DatabaseStorageFormatVersion::new(value)
            .ok_or(TestValueError("test format version must be nonzero"))
    }

    fn composition(
        database: u128,
        lifecycle_generation: u64,
        wal_file: u128,
        page_store_file: u128,
        checkpoint_file: u128,
        persistent_log: u128,
    ) -> Result<DatabaseCompositionIdentity, TestValueError> {
        let files = [
            DatabaseFileIdentity::new(
                DatabaseFileRole::RestartCheckpoint,
                file_id(checkpoint_file)?,
            ),
            DatabaseFileIdentity::new(DatabaseFileRole::Wal, file_id(wal_file)?),
            DatabaseFileIdentity::new(DatabaseFileRole::PageStore, file_id(page_store_file)?),
        ];
        DatabaseCompositionIdentity::new(
            database_id(database)?,
            generation(lifecycle_generation)?,
            log_id(persistent_log)?,
            &files,
        )
        .map_err(|_| TestValueError("test composition must be valid"))
    }

    fn manifest(
        database: u128,
        lifecycle_generation: u64,
        wal_file: u128,
        wal_format: u16,
    ) -> Result<DatabaseManifest, TestValueError> {
        Ok(DatabaseManifest::recovery_required(
            composition(database, lifecycle_generation, wal_file, 4, 5, 6)?,
            DatabaseStorageFormatRequirements::new(
                format_version(wal_format)?,
                format_version(1)?,
                format_version(1)?,
            ),
            DatabaseRequiredFeatures::NONE,
        ))
    }

    #[test]
    fn scalar_identities_reject_zero_and_preserve_order() -> Result<(), TestValueError> {
        assert_eq!(DatabaseId::new(0), None);
        assert_eq!(DatabaseFileId::new(0), None);
        assert_eq!(DatabaseLifecycleGeneration::new(0), None);

        assert!(database_id(1)? < database_id(2)?);
        assert!(file_id(1)? < file_id(2)?);
        assert!(generation(1)? < generation(2)?);
        assert_eq!(database_id(7)?.get(), 7);
        assert_eq!(file_id(8)?.get(), 8);
        assert_eq!(generation(9)?.get(), 9);
        Ok(())
    }

    #[test]
    fn lifecycle_generation_requires_one_exact_successor() -> Result<(), TestValueError> {
        let current = generation(10)?;
        assert_eq!(current.checked_next(), Ok(generation(11)?));
        assert_eq!(current.require_successor(generation(11)?), Ok(()));
        assert_eq!(
            current.require_successor(generation(10)?),
            Err(
                DatabaseLifecycleGenerationTransitionError::NotStrictlyIncreasing {
                    current,
                    proposed: generation(10)?,
                }
            )
        );
        assert_eq!(
            current.require_successor(generation(12)?),
            Err(DatabaseLifecycleGenerationTransitionError::Skipped {
                expected: generation(11)?,
                proposed: generation(12)?,
            })
        );

        let exhausted = generation(u64::MAX)?;
        assert_eq!(
            exhausted.checked_next(),
            Err(DatabaseLifecycleGenerationExhausted { current: exhausted })
        );
        assert_eq!(
            exhausted.require_successor(exhausted),
            Err(DatabaseLifecycleGenerationTransitionError::Exhausted { current: exhausted })
        );
        Ok(())
    }

    #[test]
    fn composition_accepts_each_role_once_in_any_input_order() -> Result<(), TestValueError> {
        let identity = composition(1, 2, 3, 4, 5, 6)?;
        assert_eq!(identity.database_id(), database_id(1)?);
        assert_eq!(identity.lifecycle_generation(), generation(2)?);
        assert_eq!(identity.persistent_log_id(), log_id(6)?);
        assert_eq!(identity.file_id(DatabaseFileRole::Wal), file_id(3)?);
        assert_eq!(identity.file_id(DatabaseFileRole::PageStore), file_id(4)?);
        assert_eq!(
            identity.file_id(DatabaseFileRole::RestartCheckpoint),
            file_id(5)?
        );
        assert_eq!(
            identity.ordered_files(),
            [
                DatabaseFileIdentity::new(DatabaseFileRole::Wal, file_id(3)?),
                DatabaseFileIdentity::new(DatabaseFileRole::PageStore, file_id(4)?),
                DatabaseFileIdentity::new(DatabaseFileRole::RestartCheckpoint, file_id(5)?,),
            ]
        );
        Ok(())
    }

    #[test]
    fn composition_rejects_missing_duplicate_roles_and_reused_files() -> Result<(), TestValueError>
    {
        let missing = [
            DatabaseFileIdentity::new(DatabaseFileRole::Wal, file_id(10)?),
            DatabaseFileIdentity::new(DatabaseFileRole::PageStore, file_id(11)?),
        ];
        assert_eq!(
            DatabaseCompositionIdentity::new(database_id(1)?, generation(1)?, log_id(1)?, &missing,),
            Err(DatabaseCompositionIdentityError::MissingRole {
                role: DatabaseFileRole::RestartCheckpoint,
            })
        );

        let duplicate_role = [
            DatabaseFileIdentity::new(DatabaseFileRole::Wal, file_id(10)?),
            DatabaseFileIdentity::new(DatabaseFileRole::Wal, file_id(12)?),
            DatabaseFileIdentity::new(DatabaseFileRole::PageStore, file_id(11)?),
        ];
        assert_eq!(
            DatabaseCompositionIdentity::new(
                database_id(1)?,
                generation(1)?,
                log_id(1)?,
                &duplicate_role,
            ),
            Err(DatabaseCompositionIdentityError::DuplicateRole {
                role: DatabaseFileRole::Wal,
                first_file_id: file_id(10)?,
                duplicate_file_id: file_id(12)?,
            })
        );

        let duplicate_file = [
            DatabaseFileIdentity::new(DatabaseFileRole::Wal, file_id(10)?),
            DatabaseFileIdentity::new(DatabaseFileRole::PageStore, file_id(10)?),
            DatabaseFileIdentity::new(DatabaseFileRole::RestartCheckpoint, file_id(12)?),
        ];
        assert_eq!(
            DatabaseCompositionIdentity::new(
                database_id(1)?,
                generation(1)?,
                log_id(1)?,
                &duplicate_file,
            ),
            Err(DatabaseCompositionIdentityError::DuplicateFileIdentity {
                file_id: file_id(10)?,
                first_role: DatabaseFileRole::Wal,
                second_role: DatabaseFileRole::PageStore,
            })
        );
        Ok(())
    }

    #[test]
    fn exact_composition_comparison_reports_first_stable_mismatch() -> Result<(), TestValueError> {
        let expected = composition(1, 2, 3, 4, 5, 6)?;

        assert_eq!(
            expected.require_exact_match(composition(9, 2, 3, 4, 5, 6)?),
            Err(DatabaseCompositionIdentityMismatch::DatabaseId {
                expected: database_id(1)?,
                actual: database_id(9)?,
            })
        );
        assert_eq!(
            expected.require_exact_match(composition(1, 9, 3, 4, 5, 6)?),
            Err(DatabaseCompositionIdentityMismatch::LifecycleGeneration {
                expected: generation(2)?,
                actual: generation(9)?,
            })
        );
        assert_eq!(
            expected.require_exact_match(composition(1, 2, 9, 4, 5, 6)?),
            Err(DatabaseCompositionIdentityMismatch::FileId {
                role: DatabaseFileRole::Wal,
                expected: file_id(3)?,
                actual: file_id(9)?,
            })
        );
        assert_eq!(
            expected.require_exact_match(composition(1, 2, 3, 9, 5, 6)?),
            Err(DatabaseCompositionIdentityMismatch::FileId {
                role: DatabaseFileRole::PageStore,
                expected: file_id(4)?,
                actual: file_id(9)?,
            })
        );
        assert_eq!(
            expected.require_exact_match(composition(1, 2, 3, 4, 9, 6)?),
            Err(DatabaseCompositionIdentityMismatch::FileId {
                role: DatabaseFileRole::RestartCheckpoint,
                expected: file_id(5)?,
                actual: file_id(9)?,
            })
        );
        assert_eq!(
            expected.require_exact_match(composition(1, 2, 3, 4, 5, 9)?),
            Err(DatabaseCompositionIdentityMismatch::PersistentLogId {
                expected: log_id(6)?,
                actual: log_id(9)?,
            })
        );
        assert_eq!(expected.require_exact_match(expected), Ok(()));
        Ok(())
    }

    #[test]
    fn composition_generation_advances_without_changing_child_identity()
    -> Result<(), TestValueError> {
        let current = composition(1, 2, 3, 4, 5, 6)?;
        let next = current
            .next_generation()
            .map_err(|_| TestValueError("test generation must advance"))?;
        assert_eq!(next.lifecycle_generation(), generation(3)?);
        assert_eq!(next.database_id(), current.database_id());
        assert_eq!(next.persistent_log_id(), current.persistent_log_id());
        assert_eq!(next.ordered_files(), current.ordered_files());
        assert_eq!(
            current.with_successor_generation(generation(4)?),
            Err(DatabaseLifecycleGenerationTransitionError::Skipped {
                expected: generation(3)?,
                proposed: generation(4)?,
            })
        );
        Ok(())
    }

    #[test]
    fn required_features_and_storage_versions_are_checked() -> Result<(), TestValueError> {
        assert_eq!(
            DatabaseRequiredFeatures::from_bits(0),
            Ok(DatabaseRequiredFeatures::NONE)
        );
        for actual in [1, 0x8000_0000_0000_0000] {
            assert_eq!(
                DatabaseRequiredFeatures::from_bits(actual),
                Err(DatabaseRequiredFeaturesError {
                    actual,
                    unknown: actual,
                })
            );
        }
        assert_eq!(DatabaseStorageFormatVersion::new(0), None);

        let formats = DatabaseStorageFormatRequirements::new(
            format_version(4)?,
            format_version(2)?,
            format_version(3)?,
        );
        assert_eq!(formats.version(DatabaseFileRole::Wal), format_version(4)?);
        assert_eq!(
            formats.version(DatabaseFileRole::PageStore),
            format_version(2)?
        );
        assert_eq!(
            formats.version(DatabaseFileRole::RestartCheckpoint),
            format_version(3)?
        );
        Ok(())
    }

    #[test]
    fn manifest_successor_preserves_storage_and_advances_exactly_once() -> Result<(), TestValueError>
    {
        let previous = manifest(1, 2, 3, 4)?;
        assert_eq!(
            previous.lifecycle_state(),
            DatabaseManifestLifecycleState::RecoveryRequired
        );
        assert_eq!(previous.required_features(), DatabaseRequiredFeatures::NONE);

        let next = previous
            .next_recovery_required()
            .map_err(|_| TestValueError("test manifest generation must advance"))?;
        assert_eq!(next.require_successor_of(previous), Ok(()));
        assert_eq!(
            previous.require_successor_of(previous),
            Err(DatabaseManifestSuccessorError::LifecycleGeneration(
                DatabaseLifecycleGenerationTransitionError::NotStrictlyIncreasing {
                    current: generation(2)?,
                    proposed: generation(2)?,
                }
            ))
        );

        let skipped = manifest(1, 4, 3, 4)?;
        assert_eq!(
            skipped.require_successor_of(previous),
            Err(DatabaseManifestSuccessorError::LifecycleGeneration(
                DatabaseLifecycleGenerationTransitionError::Skipped {
                    expected: generation(3)?,
                    proposed: generation(4)?,
                }
            ))
        );

        let foreign_wal = manifest(1, 3, 9, 4)?;
        assert_eq!(
            foreign_wal.require_successor_of(previous),
            Err(DatabaseManifestSuccessorError::CompositionIdentity(
                DatabaseCompositionIdentityMismatch::FileId {
                    role: DatabaseFileRole::Wal,
                    expected: file_id(3)?,
                    actual: file_id(9)?,
                }
            ))
        );

        let changed_format = manifest(1, 3, 3, 5)?;
        assert_eq!(
            changed_format.require_successor_of(previous),
            Err(DatabaseManifestSuccessorError::StorageFormatVersion {
                role: DatabaseFileRole::Wal,
                expected: format_version(4)?,
                actual: format_version(5)?,
            })
        );

        let changed_features = DatabaseManifest::recovery_required(
            next.composition_identity(),
            next.storage_formats(),
            DatabaseRequiredFeatures(1),
        );
        assert_eq!(
            changed_features.require_successor_of(previous),
            Err(DatabaseManifestSuccessorError::RequiredFeatures {
                expected: DatabaseRequiredFeatures::NONE,
                actual: DatabaseRequiredFeatures(1),
            })
        );
        Ok(())
    }

    #[test]
    fn maximum_manifest_generation_is_explicitly_exhausted() -> Result<(), TestValueError> {
        let exhausted = manifest(1, u64::MAX, 3, 4)?;
        assert_eq!(
            exhausted.next_recovery_required(),
            Err(DatabaseLifecycleGenerationExhausted {
                current: generation(u64::MAX)?,
            })
        );
        Ok(())
    }

    #[test]
    fn staged_owner_rejects_foreign_manifest_and_composition() -> Result<(), TestValueError> {
        let expected = composition(1, 2, 3, 4, 5, 6)?;
        let foreign_database = composition(9, 2, 3, 4, 5, 6)?;
        let unbound = UnboundDatabase::new("database-owner", database_id(1)?);

        let selection_error = match unbound.select_manifest(foreign_database) {
            Ok(_) => return Err(TestValueError("foreign manifest must be rejected")),
            Err(error) => error,
        };
        assert_eq!(
            selection_error.reason(),
            &DatabaseManifestSelectionRejection::ForeignDatabaseId {
                expected: database_id(1)?,
                actual: database_id(9)?,
            }
        );
        let (unbound, rejected) = (*selection_error).into_parts();
        assert_eq!(rejected, foreign_database);

        let selected = unbound
            .select_manifest(expected)
            .map_err(|_| TestValueError("exact manifest must select"))?;
        assert_eq!(selected.stage(), DatabaseLifecycleStage::ManifestSelected);

        let foreign_wal = composition(1, 9, 9, 4, 5, 6)?.storage_identity();
        let binding_error = match selected.bind_observed_storage(foreign_wal) {
            Ok(_) => return Err(TestValueError("foreign composition must be rejected")),
            Err(error) => error,
        };
        assert_eq!(
            binding_error.reason(),
            &DatabaseCompositionIdentityMismatch::FileId {
                role: DatabaseFileRole::Wal,
                expected: file_id(3)?,
                actual: file_id(9)?,
            }
        );
        let (selected, rejected) = (*binding_error).into_parts();
        assert_eq!(rejected, foreign_wal);

        let recovery_required = selected
            .bind_observed_storage(expected.storage_identity())
            .map_err(|_| TestValueError("exact storage must bind"))?;
        assert_eq!(
            recovery_required.stage(),
            DatabaseLifecycleStage::RecoveryRequired
        );
        assert_eq!(recovery_required.identity(), expected);
        Ok(())
    }
}
