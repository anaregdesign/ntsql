//! I/O-free database identity and lifecycle ownership invariants.

use std::{
    error::Error,
    fmt,
    marker::PhantomData,
    num::{NonZeroU16, NonZeroU64, NonZeroU128},
};

use ntsql_transaction::{
    DurablePageStoreSnapshotSource, DurableTransactionRestartAnalysisSource,
    DurableTransactionRestartRetentionMetadataSource,
    FailedTransactionPageStorageCleanClosePreparation, PreparedTransactionPageStorageCleanClose,
    TransactionPageStorageCleanCloseCheckpointPublisher,
    TransactionPageStorageCleanCloseCheckpointSource,
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

/// Failure to validate one inert clean-close certificate field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseCleanCloseCertificateError {
    /// The optional durable WAL frontier was present but canonically zero.
    DurableWalFrontierZero,
    /// The allocated transaction-epoch high-water was zero.
    AllocatedTransactionEpochHighWaterZero,
    /// The selected completeness-checkpoint anchor version was zero.
    CheckpointAnchorVersionZero,
}

impl fmt::Display for DatabaseCleanCloseCertificateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DurableWalFrontierZero => {
                formatter.write_str("database clean-close certificate durable WAL frontier is zero")
            }
            Self::AllocatedTransactionEpochHighWaterZero => formatter.write_str(
                "database clean-close certificate allocated transaction epoch high-water is zero",
            ),
            Self::CheckpointAnchorVersionZero => formatter
                .write_str("database clean-close certificate checkpoint anchor version is zero"),
        }
    }
}

impl Error for DatabaseCleanCloseCertificateError {}

/// Inert repository-owned evidence summary for one orderly database close.
///
/// This certificate is entirely descriptive. Constructing or decoding one does
/// not perform a close, select storage, advance a manifest, or promote any
/// owner to live or closed authority. A later issue defines the effectful
/// transaction close orchestration that actually produces these values and
/// the filesystem publication that makes a clean manifest durable.
///
/// ```compile_fail
/// use ntsql_database::{DatabaseCleanCloseCertificate, LiveDatabase};
///
/// fn cannot_promote_certificate<Owner>(
///     certificate: DatabaseCleanCloseCertificate,
/// ) -> LiveDatabase<Owner> {
///     certificate.into()
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatabaseCleanCloseCertificate {
    source_generation: DatabaseLifecycleGeneration,
    durable_wal_frontier: Option<NonZeroU64>,
    allocated_transaction_epoch_high_water: NonZeroU64,
    checkpoint_anchor_version: NonZeroU16,
    checkpoint_anchor_value: u128,
    transaction_entry_count: u64,
    page_entry_count: u64,
}

impl DatabaseCleanCloseCertificate {
    /// Validates and constructs one inert clean-close certificate.
    ///
    /// `source_generation` is the exact `RecoveryRequired` generation this
    /// evidence was produced from. `durable_wal_frontier` is optional but must
    /// be nonzero when present; `None` is the canonical absent representation.
    /// `allocated_transaction_epoch_high_water` and `checkpoint_anchor_version`
    /// must be nonzero. `checkpoint_anchor_value`, `transaction_entry_count`,
    /// and `page_entry_count` are portable counters and may be zero.
    pub const fn new(
        source_generation: DatabaseLifecycleGeneration,
        durable_wal_frontier: Option<u64>,
        allocated_transaction_epoch_high_water: u64,
        checkpoint_anchor_version: u16,
        checkpoint_anchor_value: u128,
        transaction_entry_count: u64,
        page_entry_count: u64,
    ) -> Result<Self, DatabaseCleanCloseCertificateError> {
        let durable_wal_frontier = match durable_wal_frontier {
            None => None,
            Some(value) => match NonZeroU64::new(value) {
                Some(nonzero) => Some(nonzero),
                None => return Err(DatabaseCleanCloseCertificateError::DurableWalFrontierZero),
            },
        };
        let Some(allocated_transaction_epoch_high_water) =
            NonZeroU64::new(allocated_transaction_epoch_high_water)
        else {
            return Err(DatabaseCleanCloseCertificateError::AllocatedTransactionEpochHighWaterZero);
        };
        let Some(checkpoint_anchor_version) = NonZeroU16::new(checkpoint_anchor_version) else {
            return Err(DatabaseCleanCloseCertificateError::CheckpointAnchorVersionZero);
        };
        Ok(Self {
            source_generation,
            durable_wal_frontier,
            allocated_transaction_epoch_high_water,
            checkpoint_anchor_version,
            checkpoint_anchor_value,
            transaction_entry_count,
            page_entry_count,
        })
    }

    /// Returns the source `RecoveryRequired` generation this evidence extends.
    #[must_use]
    pub const fn source_generation(self) -> DatabaseLifecycleGeneration {
        self.source_generation
    }

    /// Returns the optional durable WAL frontier, canonically absent as `None`.
    #[must_use]
    pub const fn durable_wal_frontier(self) -> Option<u64> {
        match self.durable_wal_frontier {
            Some(value) => Some(value.get()),
            None => None,
        }
    }

    /// Returns the nonzero allocated transaction-epoch high-water.
    #[must_use]
    pub const fn allocated_transaction_epoch_high_water(self) -> u64 {
        self.allocated_transaction_epoch_high_water.get()
    }

    /// Returns the nonzero selected completeness-checkpoint anchor version.
    #[must_use]
    pub const fn checkpoint_anchor_version(self) -> u16 {
        self.checkpoint_anchor_version.get()
    }

    /// Returns the selected completeness-checkpoint anchor value.
    #[must_use]
    pub const fn checkpoint_anchor_value(self) -> u128 {
        self.checkpoint_anchor_value
    }

    /// Returns the portable transaction-entry count.
    #[must_use]
    pub const fn transaction_entry_count(self) -> u64 {
        self.transaction_entry_count
    }

    /// Returns the portable page-entry count.
    #[must_use]
    pub const fn page_entry_count(self) -> u64 {
        self.page_entry_count
    }
}

/// Persisted lifecycle state understood by manifest format versions 1 and 2.
///
/// Later tombstone work must add its state together with the evidence fields
/// and version policy that make that state meaningful.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DatabaseManifestLifecycleState {
    /// Startup must complete the approved recovery path before live release.
    RecoveryRequired,
    /// An orderly close published this inert certificate as durable evidence.
    ///
    /// This state carries no authority by itself; a later clean-open issue
    /// must define the effectful gate that may consume it.
    Clean(DatabaseCleanCloseCertificate),
}

impl fmt::Display for DatabaseManifestLifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecoveryRequired => formatter.write_str("recovery required"),
            Self::Clean(_) => formatter.write_str("clean"),
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

    /// Constructs a `Clean` manifest bound to one exact certificate.
    ///
    /// `certificate.source_generation()` must be the exact predecessor of
    /// `composition_identity`'s lifecycle generation; this is the same adjacency
    /// rule [`DatabaseLifecycleGeneration::require_successor`] already enforces
    /// elsewhere. This constructor selects a lifecycle state only; it performs
    /// no filesystem publication and grants no clean-open authority.
    pub fn clean(
        composition_identity: DatabaseCompositionIdentity,
        storage_formats: DatabaseStorageFormatRequirements,
        required_features: DatabaseRequiredFeatures,
        certificate: DatabaseCleanCloseCertificate,
    ) -> Result<Self, DatabaseLifecycleGenerationTransitionError> {
        certificate
            .source_generation()
            .require_successor(composition_identity.lifecycle_generation())?;
        Ok(Self {
            composition_identity,
            lifecycle_state: DatabaseManifestLifecycleState::Clean(certificate),
            storage_formats,
            required_features,
        })
    }

    /// Produces the same recovery-required manifest at the exact next generation.
    ///
    /// This works from either lifecycle state: it only reads
    /// `composition_identity`, `storage_formats`, and `required_features`, not
    /// the current lifecycle state.
    pub fn next_recovery_required(self) -> Result<Self, DatabaseLifecycleGenerationExhausted> {
        Ok(Self::recovery_required(
            self.composition_identity.next_generation()?,
            self.storage_formats,
            self.required_features,
        ))
    }

    /// Produces the `Clean` successor manifest bound to `certificate` at the
    /// exact next generation.
    ///
    /// The current manifest must be `RecoveryRequired`, and `certificate` must
    /// report its exact current generation as the source generation. A clean
    /// manifest cannot produce another clean successor without first publishing
    /// a recovery-required generation.
    pub fn next_clean(
        self,
        certificate: DatabaseCleanCloseCertificate,
    ) -> Result<Self, DatabaseManifestCleanSuccessorError> {
        if matches!(
            self.lifecycle_state,
            DatabaseManifestLifecycleState::Clean(_)
        ) {
            return Err(DatabaseManifestCleanSuccessorError::LifecycleTransition(
                DatabaseManifestLifecycleTransitionError::CleanToClean,
            ));
        }
        let next_composition_identity = self
            .composition_identity
            .next_generation()
            .map_err(DatabaseManifestCleanSuccessorError::Exhausted)?;
        Self::clean(
            next_composition_identity,
            self.storage_formats,
            self.required_features,
            certificate,
        )
        .map_err(DatabaseManifestCleanSuccessorError::SourceGeneration)
    }

    /// Validates this manifest as the exact next generation after `previous`.
    ///
    /// This comparison is explicit because decoding one isolated frame has no
    /// prior generation against which it could detect regression.
    /// `RecoveryRequired -> RecoveryRequired`, `RecoveryRequired -> Clean`, and
    /// `Clean -> RecoveryRequired` are the only valid lifecycle transitions;
    /// `Clean -> Clean` is rejected because an orderly close must always leave
    /// a fresh recovery-required generation before it may become clean again.
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
        if matches!(
            previous.lifecycle_state,
            DatabaseManifestLifecycleState::Clean(_)
        ) && matches!(
            self.lifecycle_state,
            DatabaseManifestLifecycleState::Clean(_)
        ) {
            return Err(DatabaseManifestSuccessorError::LifecycleTransition(
                DatabaseManifestLifecycleTransitionError::CleanToClean,
            ));
        }
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

/// Rejection of an invalid lifecycle-state pairing between adjacent manifests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseManifestLifecycleTransitionError {
    /// A `Clean` manifest cannot be immediately followed by another `Clean` manifest.
    CleanToClean,
}

impl fmt::Display for DatabaseManifestLifecycleTransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CleanToClean => formatter.write_str(
                "database manifest lifecycle cannot transition from clean directly to clean",
            ),
        }
    }
}

impl Error for DatabaseManifestLifecycleTransitionError {}

/// Rejection of a manifest claimed as one exact `Clean` lifecycle successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseManifestCleanSuccessorError {
    /// The retained lifecycle state cannot directly become clean.
    LifecycleTransition(DatabaseManifestLifecycleTransitionError),
    /// No generation exists above the retained current generation.
    Exhausted(DatabaseLifecycleGenerationExhausted),
    /// The certificate's source generation is not the exact predecessor of the
    /// proposed clean generation.
    SourceGeneration(DatabaseLifecycleGenerationTransitionError),
}

impl fmt::Display for DatabaseManifestCleanSuccessorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LifecycleTransition(source) => write!(
                formatter,
                "database manifest clean successor lifecycle is invalid: {source}"
            ),
            Self::Exhausted(source) => {
                write!(
                    formatter,
                    "database manifest clean successor is invalid: {source}"
                )
            }
            Self::SourceGeneration(source) => write!(
                formatter,
                "database manifest clean successor certificate is invalid: {source}"
            ),
        }
    }
}

impl Error for DatabaseManifestCleanSuccessorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LifecycleTransition(source) => Some(source),
            Self::Exhausted(source) => Some(source),
            Self::SourceGeneration(source) => Some(source),
        }
    }
}

/// Rejection of a manifest claimed as one exact lifecycle successor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseManifestSuccessorError {
    /// Database, child-file, or persistent-WAL identity changed.
    CompositionIdentity(DatabaseCompositionIdentityMismatch),
    /// The lifecycle generation regressed, skipped, or exhausted.
    LifecycleGeneration(DatabaseLifecycleGenerationTransitionError),
    /// The lifecycle-state pairing is not one of the allowed transitions.
    LifecycleTransition(DatabaseManifestLifecycleTransitionError),
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
            Self::LifecycleTransition(source) => {
                write!(
                    formatter,
                    "database manifest lifecycle is invalid: {source}"
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
            Self::LifecycleTransition(source) => Some(source),
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
    /// Live or close-pending authority was explicitly relinquished without clean publication.
    Abandoned,
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

    /// Selects one complete validated inert manifest for the expected database.
    pub fn select_manifest(
        self,
        selected_manifest: DatabaseManifest,
    ) -> Result<ManifestSelectedDatabase<Owner>, Box<DatabaseManifestSelectionError<Owner>>> {
        let selected_identity = selected_manifest.composition_identity();
        if self.expected_database_id != selected_identity.database_id {
            let expected = self.expected_database_id;
            return Err(Box::new(DatabaseManifestSelectionError {
                database: self,
                selected_manifest,
                reason: DatabaseManifestSelectionRejection::ForeignDatabaseId {
                    expected,
                    actual: selected_identity.database_id,
                },
            }));
        }
        Ok(ManifestSelectedDatabase {
            owner: self.owner,
            manifest: selected_manifest,
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
    selected_manifest: DatabaseManifest,
    reason: DatabaseManifestSelectionRejection,
}

impl<Owner> DatabaseManifestSelectionError<Owner> {
    /// Returns the exact rejection.
    #[must_use]
    pub const fn reason(&self) -> &DatabaseManifestSelectionRejection {
        &self.reason
    }

    /// Returns the rejected inert manifest's composition identity.
    #[must_use]
    pub const fn selected_identity(&self) -> DatabaseCompositionIdentity {
        self.selected_manifest.composition_identity()
    }

    /// Returns the complete rejected inert manifest.
    #[must_use]
    pub const fn selected_manifest(&self) -> DatabaseManifest {
        self.selected_manifest
    }

    /// Releases the retained unbound owner and rejected inert manifest together.
    pub fn into_parts(self) -> (UnboundDatabase<Owner>, DatabaseManifest) {
        (self.database, self.selected_manifest)
    }
}

impl<Owner> fmt::Debug for DatabaseManifestSelectionError<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseManifestSelectionError")
            .field("selected_manifest", &self.selected_manifest)
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
    manifest: DatabaseManifest,
}

impl<Owner> ManifestSelectedDatabase<Owner> {
    /// Returns the selected inert composition identity.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.manifest.composition_identity()
    }

    /// Returns the complete selected inert manifest.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.manifest
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
    ) -> Result<RecoveryRequiredDatabase<Owner>, Box<DatabaseRecoveryRequiredBindingError<Owner>>>
    {
        if !matches!(
            self.manifest.lifecycle_state(),
            DatabaseManifestLifecycleState::RecoveryRequired
        ) {
            let actual = self.manifest.lifecycle_state();
            return Err(Box::new(DatabaseRecoveryRequiredBindingError {
                database: self,
                observed_identity,
                reason: DatabaseRecoveryRequiredBindingRejection::ManifestLifecycle { actual },
            }));
        }
        if let Err(reason) = self
            .manifest
            .composition_identity()
            .storage_identity()
            .require_exact_match(observed_identity)
        {
            return Err(Box::new(DatabaseRecoveryRequiredBindingError {
                database: self,
                observed_identity,
                reason: DatabaseRecoveryRequiredBindingRejection::StorageIdentity(reason),
            }));
        }
        Ok(RecoveryRequiredDatabase {
            _owner: self.owner,
            identity: self.manifest.composition_identity(),
        })
    }
}

impl<Owner> fmt::Debug for ManifestSelectedDatabase<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManifestSelectedDatabase")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

/// Reason a selected manifest cannot cross the recovery-required binding gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseRecoveryRequiredBindingRejection {
    /// The complete selected manifest describes another lifecycle state.
    ManifestLifecycle {
        /// Exact rejected lifecycle state.
        actual: DatabaseManifestLifecycleState,
    },
    /// The physical child identity differs from the selected manifest.
    StorageIdentity(DatabaseCompositionIdentityMismatch),
}

impl fmt::Display for DatabaseRecoveryRequiredBindingRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestLifecycle { actual } => write!(
                formatter,
                "recovery-required binding cannot consume a {actual} manifest"
            ),
            Self::StorageIdentity(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for DatabaseRecoveryRequiredBindingRejection {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::StorageIdentity(source) => Some(source),
            Self::ManifestLifecycle { .. } => None,
        }
    }
}

/// Failed recovery-required binding retaining selected ownership and evidence.
#[must_use = "failed recovery-required binding retains the selected database owner"]
pub struct DatabaseRecoveryRequiredBindingError<Owner> {
    database: ManifestSelectedDatabase<Owner>,
    observed_identity: DatabaseStorageIdentity,
    reason: DatabaseRecoveryRequiredBindingRejection,
}

impl<Owner> DatabaseRecoveryRequiredBindingError<Owner> {
    /// Returns the lifecycle or stable-storage contradiction.
    #[must_use]
    pub const fn reason(&self) -> &DatabaseRecoveryRequiredBindingRejection {
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

impl<Owner> fmt::Debug for DatabaseRecoveryRequiredBindingError<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseRecoveryRequiredBindingError")
            .field("observed_identity", &self.observed_identity)
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl<Owner> fmt::Display for DatabaseRecoveryRequiredBindingError<Owner> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "database recovery-required binding failed: {}",
            self.reason
        )
    }
}

impl<Owner> Error for DatabaseRecoveryRequiredBindingError<Owner> {}

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

    /// Relinquishes live authority without attempting any durable close effect.
    ///
    /// This explicit outcome drops the retained adapter owners and leaves the
    /// selected durable manifest recovery-required.
    pub fn abandon(self) -> AbandonedDatabase {
        let Self {
            _owner: owner,
            identity,
        } = self;
        drop(owner);
        AbandonedDatabase { identity }
    }
}

define_owned_database_state!(
    /// Live owner consumed into an orderly close attempt.
    ///
    /// Construction remains private until the close protocol owns an effectful
    /// transition and outcome-indeterminate failure state.
    #[must_use = "close-pending database ownership must publish, be abandoned, or be dropped"]
    ClosePendingDatabase,
    DatabaseLifecycleStage::ClosePending
);

/// Database-level contradiction discovered while binding a transaction close proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseClosePreparationEvidenceError {
    /// The transaction proof describes another persistent WAL.
    PersistentLogIdMismatch {
        /// Manifest-selected persistent WAL identity.
        expected: PersistentLogId,
        /// Persistent WAL identity bound by the transaction proof.
        actual: PersistentLogId,
    },
    /// A proof field could not form the canonical database certificate.
    Certificate(DatabaseCleanCloseCertificateError),
    /// The retained source manifest could not construct the exact clean successor.
    TargetManifest(DatabaseManifestCleanSuccessorError),
}

impl fmt::Display for DatabaseClosePreparationEvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PersistentLogIdMismatch { expected, actual } => write!(
                formatter,
                "transaction close proof persistent log ID {} does not match selected database persistent log ID {}",
                actual.get(),
                expected.get()
            ),
            Self::Certificate(source) => source.fmt(formatter),
            Self::TargetManifest(source) => source.fmt(formatter),
        }
    }
}

impl Error for DatabaseClosePreparationEvidenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PersistentLogIdMismatch { .. } => None,
            Self::Certificate(source) => Some(source),
            Self::TargetManifest(source) => Some(source),
        }
    }
}

/// Trusted outer-owner observation required before transaction close publication.
pub trait DatabaseCloseSourceManifestOwner {
    /// Returns the exact selected manifest retained under database-wide ownership.
    fn close_source_manifest(&self) -> DatabaseManifest;
}

/// Source-manifest contradiction detected before transaction close publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseClosePreparationPreflightError {
    /// The retained manifest does not identify the Live database composition.
    SourceManifestIdentity(DatabaseCompositionIdentityMismatch),
    /// The retained manifest is not recovery-required.
    SourceManifestLifecycle {
        /// Rejected inert source lifecycle state.
        actual: DatabaseManifestLifecycleState,
    },
    /// No adjacent lifecycle generation can represent a clean successor.
    LifecycleGeneration(DatabaseLifecycleGenerationExhausted),
}

impl fmt::Display for DatabaseClosePreparationPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceManifestIdentity(source) => {
                write!(
                    formatter,
                    "database close source manifest identity mismatch: {source}"
                )
            }
            Self::SourceManifestLifecycle { actual } => write!(
                formatter,
                "database close source manifest lifecycle is {actual}, not recovery required"
            ),
            Self::LifecycleGeneration(source) => source.fmt(formatter),
        }
    }
}

impl Error for DatabaseClosePreparationPreflightError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SourceManifestIdentity(source) => Some(source),
            Self::SourceManifestLifecycle { .. } => None,
            Self::LifecycleGeneration(source) => Some(source),
        }
    }
}

/// Exact database and transaction owners retained after close preparation.
///
/// Construction is private to [`LiveDatabase::prepare_close`]. The transaction
/// owner and its proof remain inseparable until a later synchronized manifest
/// publication transition consumes this value.
///
/// ```compile_fail
/// use ntsql_database::PreparedDatabaseCloseOwnership;
///
/// fn cannot_extract_transaction<OuterOwner, Source, Store, CheckpointSource, const N: usize>(
///     prepared: PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>,
/// ) {
///     let _transaction = prepared.into_transaction();
/// }
/// ```
#[must_use = "prepared database close ownership must remain inside ClosePending"]
pub struct PreparedDatabaseCloseOwnership<
    OuterOwner,
    Source,
    Store,
    CheckpointSource,
    const N: usize,
> where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    outer_owner: OuterOwner,
    transaction: PreparedTransactionPageStorageCleanClose<Source, Store, CheckpointSource, N>,
    source_manifest: DatabaseManifest,
    certificate: DatabaseCleanCloseCertificate,
    target_manifest: DatabaseManifest,
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize>
    PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    /// Borrows the retained database-wide owner.
    #[must_use]
    pub const fn outer_owner(&self) -> &OuterOwner {
        &self.outer_owner
    }

    /// Borrows the inseparable transaction close owner and proof.
    pub const fn transaction(
        &self,
    ) -> &PreparedTransactionPageStorageCleanClose<Source, Store, CheckpointSource, N> {
        &self.transaction
    }

    /// Returns the exact recovery-required manifest retained at preparation.
    #[must_use]
    pub const fn source_manifest(&self) -> DatabaseManifest {
        self.source_manifest
    }

    /// Returns the exact database clean-close certificate.
    #[must_use]
    pub const fn certificate(&self) -> DatabaseCleanCloseCertificate {
        self.certificate
    }

    /// Returns the exact adjacent composition targeted by clean publication.
    #[must_use]
    pub const fn target_identity(&self) -> DatabaseCompositionIdentity {
        self.target_manifest.composition_identity()
    }

    /// Returns the exact adjacent clean manifest awaiting publication.
    #[must_use]
    pub const fn target_manifest(&self) -> DatabaseManifest {
        self.target_manifest
    }
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize> fmt::Debug
    for PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedDatabaseCloseOwnership")
            .field("source_manifest", &self.source_manifest)
            .field("certificate", &self.certificate)
            .field("target_manifest", &self.target_manifest)
            .field("outer_owner", &format_args!("<retained>"))
            .field("transaction", &format_args!("<retained>"))
            .finish()
    }
}

enum FailedDatabaseClosePreparationState<
    RecoveredOwner,
    OuterOwner,
    TransactionFailure,
    PreparedTransaction,
> {
    Preflight {
        _owner: Box<RecoveredOwner>,
        error: DatabaseClosePreparationPreflightError,
    },
    Transaction {
        _owners: Box<(OuterOwner, TransactionFailure)>,
    },
    Evidence {
        _owners: Box<(OuterOwner, PreparedTransaction)>,
        error: DatabaseClosePreparationEvidenceError,
    },
}

type DatabaseClosePreparationStateFor<OuterOwner, Source, Store, CheckpointSource, const N: usize> =
    FailedDatabaseClosePreparationState<
        RecoveredDatabaseOwnership<OuterOwner, Source, Store, CheckpointSource, N>,
        OuterOwner,
        FailedTransactionPageStorageCleanClosePreparation<Source, Store, CheckpointSource, N>,
        PreparedTransactionPageStorageCleanClose<Source, Store, CheckpointSource, N>,
    >;

struct FailedDatabaseClosePreparationInner<State> {
    identity: DatabaseCompositionIdentity,
    state: State,
}

/// Borrowed cause of one terminal database close-preparation failure.
#[derive(Debug)]
pub enum DatabaseClosePreparationFailureCause<'failure, TransactionFailure> {
    /// Source manifest or adjacent lifecycle preflight failed.
    Preflight(&'failure DatabaseClosePreparationPreflightError),
    /// Fresh transaction-storage close preparation failed.
    Transaction(&'failure TransactionFailure),
    /// The produced transaction proof contradicted the selected database.
    Evidence(&'failure DatabaseClosePreparationEvidenceError),
}

/// Terminal owner retained when database close preparation cannot complete.
///
/// There is no stale same-owner retry or adapter extraction. The caller may
/// inspect the cause and explicitly abandon ownership before reopening from
/// durable state.
///
/// ```compile_fail
/// use ntsql_database::FailedDatabaseClosePreparation;
///
/// fn cannot_retry<OuterOwner, Source, Store, CheckpointSource, const N: usize>(
///     failure: FailedDatabaseClosePreparation<
///         OuterOwner,
///         Source,
///         Store,
///         CheckpointSource,
///         N,
///     >,
/// ) {
///     let _retry = failure.retry();
/// }
/// ```
#[must_use = "failed database close preparation retains all ownership until abandoned or dropped"]
pub struct FailedDatabaseClosePreparation<
    OuterOwner,
    Source,
    Store,
    CheckpointSource,
    const N: usize,
> where
    Source: DurableTransactionRestartAnalysisSource<N>
        + DurableTransactionRestartRetentionMetadataSource,
    Store: DurablePageStoreSnapshotSource<N>,
    CheckpointSource: TransactionPageStorageCleanCloseCheckpointSource
        + TransactionPageStorageCleanCloseCheckpointPublisher,
{
    inner: Box<
        FailedDatabaseClosePreparationInner<
            DatabaseClosePreparationStateFor<OuterOwner, Source, Store, CheckpointSource, N>,
        >,
    >,
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize>
    FailedDatabaseClosePreparation<OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>
        + DurableTransactionRestartRetentionMetadataSource,
    Store: DurablePageStoreSnapshotSource<N>,
    CheckpointSource: TransactionPageStorageCleanCloseCheckpointSource
        + TransactionPageStorageCleanCloseCheckpointPublisher,
{
    fn new(
        identity: DatabaseCompositionIdentity,
        state: DatabaseClosePreparationStateFor<OuterOwner, Source, Store, CheckpointSource, N>,
    ) -> Self {
        Self {
            inner: Box::new(FailedDatabaseClosePreparationInner { identity, state }),
        }
    }

    /// Returns the recovery-required composition retained by the failed owner.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.inner.identity
    }

    /// Returns the exact failure cause without releasing any owner.
    #[must_use]
    pub const fn cause(
        &self,
    ) -> DatabaseClosePreparationFailureCause<
        '_,
        FailedTransactionPageStorageCleanClosePreparation<Source, Store, CheckpointSource, N>,
    > {
        match &self.inner.state {
            FailedDatabaseClosePreparationState::Preflight { error, .. } => {
                DatabaseClosePreparationFailureCause::Preflight(error)
            }
            FailedDatabaseClosePreparationState::Transaction { _owners } => {
                DatabaseClosePreparationFailureCause::Transaction(&_owners.1)
            }
            FailedDatabaseClosePreparationState::Evidence { error, .. } => {
                DatabaseClosePreparationFailureCause::Evidence(error)
            }
        }
    }

    /// Explicitly relinquishes every retained owner without publishing clean state.
    pub fn abandon(self) -> AbandonedDatabase {
        let identity = self.inner.identity;
        drop(self);
        AbandonedDatabase { identity }
    }
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize> fmt::Debug
    for FailedDatabaseClosePreparation<OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>
        + DurableTransactionRestartRetentionMetadataSource,
    Store: DurablePageStoreSnapshotSource<N>,
    CheckpointSource: TransactionPageStorageCleanCloseCheckpointSource
        + TransactionPageStorageCleanCloseCheckpointPublisher,
    FailedTransactionPageStorageCleanClosePreparation<Source, Store, CheckpointSource, N>:
        fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FailedDatabaseClosePreparation")
            .field("identity", &self.inner.identity)
            .field("cause", &self.cause())
            .finish_non_exhaustive()
    }
}

/// Result of the only database Live-to-ClosePending preparation transition.
pub type DatabaseClosePreparationResult<
    OuterOwner,
    Source,
    Store,
    CheckpointSource,
    const N: usize,
> = Result<
    ClosePendingDatabase<
        PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>,
    >,
    FailedDatabaseClosePreparation<OuterOwner, Source, Store, CheckpointSource, N>,
>;

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize>
    LiveDatabase<RecoveredDatabaseOwnership<OuterOwner, Source, Store, CheckpointSource, N>>
where
    OuterOwner: DatabaseCloseSourceManifestOwner,
    Source: DurableTransactionRestartAnalysisSource<N>
        + DurableTransactionRestartRetentionMetadataSource,
    Store: DurablePageStoreSnapshotSource<N>,
    CheckpointSource: TransactionPageStorageCleanCloseCheckpointSource
        + TransactionPageStorageCleanCloseCheckpointPublisher,
{
    /// Consumes Live before deriving and binding a fresh transaction close proof.
    ///
    /// Generation exhaustion is rejected before transaction candidate
    /// publication. Every later failure is terminal and retains the exact
    /// database and transaction owners for explicit unclean abandonment.
    pub fn prepare_close(
        self,
    ) -> DatabaseClosePreparationResult<OuterOwner, Source, Store, CheckpointSource, N> {
        let Self {
            _owner: recovered,
            identity,
        } = self;
        let source_manifest = recovered.outer_owner.close_source_manifest();
        if let Err(source) = identity.require_exact_match(source_manifest.composition_identity()) {
            return Err(FailedDatabaseClosePreparation::new(
                identity,
                FailedDatabaseClosePreparationState::Preflight {
                    _owner: Box::new(recovered),
                    error: DatabaseClosePreparationPreflightError::SourceManifestIdentity(source),
                },
            ));
        }
        if let actual @ DatabaseManifestLifecycleState::Clean(_) = source_manifest.lifecycle_state()
        {
            return Err(FailedDatabaseClosePreparation::new(
                identity,
                FailedDatabaseClosePreparationState::Preflight {
                    _owner: Box::new(recovered),
                    error: DatabaseClosePreparationPreflightError::SourceManifestLifecycle {
                        actual,
                    },
                },
            ));
        }
        if let Err(source) = identity.lifecycle_generation().checked_next() {
            return Err(FailedDatabaseClosePreparation::new(
                identity,
                FailedDatabaseClosePreparationState::Preflight {
                    _owner: Box::new(recovered),
                    error: DatabaseClosePreparationPreflightError::LifecycleGeneration(source),
                },
            ));
        }

        let RecoveredDatabaseOwnership {
            outer_owner,
            transaction,
        } = recovered;
        let transaction = match transaction.prepare_clean_close() {
            Ok(transaction) => transaction,
            Err(failure) => {
                return Err(FailedDatabaseClosePreparation::new(
                    identity,
                    FailedDatabaseClosePreparationState::Transaction {
                        _owners: Box::new((outer_owner, failure)),
                    },
                ));
            }
        };

        let (
            actual_persistent_log_id,
            durable_frontier,
            allocated_epoch_high_water,
            checkpoint_anchor,
            transaction_entry_count,
            page_entry_count,
        ) = {
            let proof = transaction.proof();
            (
                proof.persistent_log_id(),
                proof.durable_frontier(),
                proof.allocated_epoch_high_water(),
                proof.checkpoint_anchor(),
                proof.transaction_entry_count(),
                proof.page_entry_count(),
            )
        };
        let expected_persistent_log_id = identity.persistent_log_id();
        if actual_persistent_log_id != expected_persistent_log_id {
            return Err(FailedDatabaseClosePreparation::new(
                identity,
                FailedDatabaseClosePreparationState::Evidence {
                    _owners: Box::new((outer_owner, transaction)),
                    error: DatabaseClosePreparationEvidenceError::PersistentLogIdMismatch {
                        expected: expected_persistent_log_id,
                        actual: actual_persistent_log_id,
                    },
                },
            ));
        }

        let certificate = match DatabaseCleanCloseCertificate::new(
            identity.lifecycle_generation(),
            durable_frontier,
            allocated_epoch_high_water,
            checkpoint_anchor.version(),
            checkpoint_anchor.value(),
            transaction_entry_count,
            page_entry_count,
        ) {
            Ok(certificate) => certificate,
            Err(source) => {
                return Err(FailedDatabaseClosePreparation::new(
                    identity,
                    FailedDatabaseClosePreparationState::Evidence {
                        _owners: Box::new((outer_owner, transaction)),
                        error: DatabaseClosePreparationEvidenceError::Certificate(source),
                    },
                ));
            }
        };
        let target_manifest = match source_manifest.next_clean(certificate) {
            Ok(target_manifest) => target_manifest,
            Err(error) => {
                return Err(FailedDatabaseClosePreparation::new(
                    identity,
                    FailedDatabaseClosePreparationState::Evidence {
                        _owners: Box::new((outer_owner, transaction)),
                        error: DatabaseClosePreparationEvidenceError::TargetManifest(error),
                    },
                ));
            }
        };

        Ok(ClosePendingDatabase {
            _owner: PreparedDatabaseCloseOwnership {
                outer_owner,
                transaction,
                source_manifest,
                certificate,
                target_manifest,
            },
            identity,
        })
    }
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize>
    ClosePendingDatabase<
        PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>,
    >
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    /// Borrows the exact owners and evidence awaiting manifest publication.
    pub const fn prepared(
        &self,
    ) -> &PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N> {
        &self._owner
    }
}

/// Durable selection knowledge after a clean-manifest publication attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseCleanManifestPublicationState {
    /// The recovery-required source manifest is still selected.
    SourceSelected,
    /// The caller cannot determine whether the source or target is selected.
    SelectionIndeterminate,
    /// The clean target is selected, but its durability barrier is unresolved.
    TargetSelectedDurabilityIndeterminate,
    /// The exact clean target and its publication barrier are durable.
    TargetDurable,
}

/// One-use authority to publish one exact clean manifest.
///
/// Construction is private and the attempt brand prevents a publisher from
/// retaining authority for a later close attempt.
///
/// ```compile_fail
/// use ntsql_database::{
///     DatabaseCleanManifestPublicationPermit, DatabaseManifest,
/// };
///
/// fn cannot_forge(
///     target_manifest: DatabaseManifest,
/// ) -> DatabaseCleanManifestPublicationPermit<'static> {
///     DatabaseCleanManifestPublicationPermit {
///         target_manifest,
///         attempt_brand: core::marker::PhantomData,
///     }
/// }
/// ```
pub struct DatabaseCleanManifestPublicationPermit<'attempt> {
    target_manifest: DatabaseManifest,
    attempt_brand: PhantomData<&'attempt mut &'attempt ()>,
}

impl DatabaseCleanManifestPublicationPermit<'_> {
    /// Returns the only manifest this attempt may publish.
    #[must_use]
    pub const fn target_manifest(&self) -> DatabaseManifest {
        self.target_manifest
    }

    /// Completes this one-use authority with fresh publisher observations.
    #[must_use]
    pub fn complete(
        self,
        selected_manifest: DatabaseManifest,
        synchronized_manifest: DatabaseManifest,
    ) -> DatabaseCleanManifestPublicationReceipt {
        DatabaseCleanManifestPublicationReceipt {
            selected_manifest,
            synchronized_manifest,
        }
    }
}

fn with_database_clean_manifest_publication_permit<Output, Operation>(
    target_manifest: DatabaseManifest,
    operation: Operation,
) -> Output
where
    Operation: for<'attempt> FnOnce(DatabaseCleanManifestPublicationPermit<'attempt>) -> Output,
{
    operation(DatabaseCleanManifestPublicationPermit {
        target_manifest,
        attempt_brand: PhantomData,
    })
}

/// Private-field evidence returned by a clean-manifest publisher.
///
/// ```compile_fail
/// use ntsql_database::{
///     DatabaseCleanManifestPublicationReceipt, DatabaseManifest,
/// };
///
/// fn cannot_forge(
///     manifest: DatabaseManifest,
/// ) -> DatabaseCleanManifestPublicationReceipt {
///     DatabaseCleanManifestPublicationReceipt {
///         selected_manifest: manifest,
///         synchronized_manifest: manifest,
///     }
/// }
/// ```
pub struct DatabaseCleanManifestPublicationReceipt {
    selected_manifest: DatabaseManifest,
    synchronized_manifest: DatabaseManifest,
}

impl DatabaseCleanManifestPublicationReceipt {
    /// Returns the manifest freshly observed at the selected location.
    #[must_use]
    pub const fn selected_manifest(&self) -> DatabaseManifest {
        self.selected_manifest
    }

    /// Returns the manifest covered by the publisher's durability barrier.
    #[must_use]
    pub const fn synchronized_manifest(&self) -> DatabaseManifest {
        self.synchronized_manifest
    }
}

impl fmt::Debug for DatabaseCleanManifestPublicationReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseCleanManifestPublicationReceipt")
            .field("selected_manifest", &self.selected_manifest)
            .field("synchronized_manifest", &self.synchronized_manifest)
            .finish()
    }
}

/// Adapter-reported clean-manifest publication failure and durable-state class.
pub struct DatabaseCleanManifestPublisherFailure<PublisherError> {
    state: DatabaseCleanManifestPublicationState,
    error: PublisherError,
}

impl<PublisherError> DatabaseCleanManifestPublisherFailure<PublisherError> {
    /// Constructs a typed publisher failure with exact durable-state knowledge.
    #[must_use]
    pub const fn new(state: DatabaseCleanManifestPublicationState, error: PublisherError) -> Self {
        Self { state, error }
    }

    /// Returns the durable selection knowledge at the failure boundary.
    #[must_use]
    pub const fn state(&self) -> DatabaseCleanManifestPublicationState {
        self.state
    }

    /// Borrows the adapter-specific publication cause.
    #[must_use]
    pub const fn error(&self) -> &PublisherError {
        &self.error
    }

    fn into_parts(self) -> (DatabaseCleanManifestPublicationState, PublisherError) {
        (self.state, self.error)
    }
}

impl<PublisherError: fmt::Debug> fmt::Debug
    for DatabaseCleanManifestPublisherFailure<PublisherError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseCleanManifestPublisherFailure")
            .field("state", &self.state)
            .field("error", &self.error)
            .finish()
    }
}

impl<PublisherError: fmt::Display> fmt::Display
    for DatabaseCleanManifestPublisherFailure<PublisherError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "database clean-manifest publication failed with {:?} state: {}",
            self.state, self.error
        )
    }
}

impl<PublisherError> Error for DatabaseCleanManifestPublisherFailure<PublisherError>
where
    PublisherError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

/// Trusted adapter port for publishing one exact clean manifest.
pub trait DatabaseCleanManifestPublisher: DatabaseCloseSourceManifestOwner {
    /// Adapter-specific input, such as one deterministic fault plan.
    type Input;
    /// Adapter-specific publication cause.
    type Error;

    /// Publishes and synchronizes the permit's exact target manifest.
    fn publish_clean_manifest(
        &mut self,
        input: Self::Input,
        permit: DatabaseCleanManifestPublicationPermit<'_>,
    ) -> Result<
        DatabaseCleanManifestPublicationReceipt,
        DatabaseCleanManifestPublisherFailure<Self::Error>,
    >;
}

/// Contradiction found immediately before clean-manifest publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseClosePublicationPreflightError {
    /// The selected manifest changed after close preparation.
    SourceManifestChanged {
        /// Exact recovery-required manifest retained during preparation.
        expected: DatabaseManifest,
        /// Fresh manifest observation made immediately before publication.
        actual: DatabaseManifest,
    },
    /// The retained source no longer identifies the source composition.
    SourceManifestIdentity(DatabaseCompositionIdentityMismatch),
    /// The retained source is not recovery-required.
    SourceManifestLifecycle {
        /// Rejected source lifecycle state.
        actual: DatabaseManifestLifecycleState,
    },
    /// The retained certificate cannot produce an adjacent clean successor.
    TargetManifest(DatabaseManifestCleanSuccessorError),
    /// Recalculation did not reproduce the retained exact target manifest.
    TargetManifestChanged {
        /// Target retained during close preparation.
        expected: DatabaseManifest,
        /// Target freshly recalculated from source and certificate.
        actual: DatabaseManifest,
    },
}

impl fmt::Display for DatabaseClosePublicationPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceManifestChanged { .. } => formatter
                .write_str("selected database close source manifest changed after preparation"),
            Self::SourceManifestIdentity(source) => write!(
                formatter,
                "database close publication source identity mismatch: {source}"
            ),
            Self::SourceManifestLifecycle { actual } => write!(
                formatter,
                "database close publication source lifecycle is {actual}, not recovery required"
            ),
            Self::TargetManifest(source) => write!(
                formatter,
                "database close publication target manifest is invalid: {source}"
            ),
            Self::TargetManifestChanged { .. } => {
                formatter.write_str("database close publication target changed after preparation")
            }
        }
    }
}

impl Error for DatabaseClosePublicationPreflightError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SourceManifestIdentity(source) => Some(source),
            Self::TargetManifest(source) => Some(source),
            Self::SourceManifestChanged { .. }
            | Self::SourceManifestLifecycle { .. }
            | Self::TargetManifestChanged { .. } => None,
        }
    }
}

/// Invalid exact receipt returned after an effectful publication attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseCleanManifestPublicationReceiptError {
    /// The freshly observed selected manifest is not the retained target.
    SelectedManifest {
        /// Exact clean target retained before publication.
        expected: DatabaseManifest,
        /// Publisher-reported selected manifest.
        actual: DatabaseManifest,
    },
    /// The synchronized manifest is not the retained target.
    SynchronizedManifest {
        /// Exact clean target retained before publication.
        expected: DatabaseManifest,
        /// Publisher-reported synchronized manifest.
        actual: DatabaseManifest,
    },
}

impl fmt::Display for DatabaseCleanManifestPublicationReceiptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelectedManifest { .. } => formatter.write_str(
                "database clean-manifest receipt selected manifest does not equal the target",
            ),
            Self::SynchronizedManifest { .. } => formatter.write_str(
                "database clean-manifest receipt synchronized manifest does not equal the target",
            ),
        }
    }
}

impl Error for DatabaseCleanManifestPublicationReceiptError {}

/// Typed cause retained after close publication cannot produce Closed.
#[derive(Debug)]
pub enum DatabaseClosePublicationError<PublisherError> {
    /// Final source/target revalidation failed before the permit was issued.
    Preflight(DatabaseClosePublicationPreflightError),
    /// The adapter returned an effectful publication failure.
    Publisher(PublisherError),
    /// The adapter returned a receipt that did not prove the exact target.
    Receipt(DatabaseCleanManifestPublicationReceiptError),
}

impl<PublisherError: fmt::Display> fmt::Display for DatabaseClosePublicationError<PublisherError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preflight(source) => source.fmt(formatter),
            Self::Publisher(source) => source.fmt(formatter),
            Self::Receipt(source) => source.fmt(formatter),
        }
    }
}

impl<PublisherError> Error for DatabaseClosePublicationError<PublisherError>
where
    PublisherError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Preflight(source) => Some(source),
            Self::Publisher(source) => Some(source),
            Self::Receipt(source) => Some(source),
        }
    }
}

/// Exact owners and receipt retained after synchronized clean publication.
#[must_use = "published database close ownership must remain inside Closed"]
pub struct PublishedDatabaseCloseOwnership<
    OuterOwner,
    Source,
    Store,
    CheckpointSource,
    const N: usize,
> where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    prepared: PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>,
    receipt: DatabaseCleanManifestPublicationReceipt,
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize>
    PublishedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    /// Borrows the complete prepared owner set retained through Closed.
    pub const fn prepared(
        &self,
    ) -> &PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N> {
        &self.prepared
    }

    /// Returns the exact selected and synchronized clean manifest.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.receipt.synchronized_manifest
    }
}

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize> fmt::Debug
    for PublishedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublishedDatabaseCloseOwnership")
            .field("prepared", &self.prepared)
            .field("receipt", &self.receipt)
            .finish()
    }
}

/// Terminal owner-retaining failure of clean-manifest publication.
#[must_use = "failed database close publication retains every owner until abandoned or dropped"]
pub struct FailedDatabaseClosePublication<
    OuterOwner,
    Source,
    Store,
    CheckpointSource,
    PublisherError,
    const N: usize,
> where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    state: DatabaseCleanManifestPublicationState,
    error: Box<DatabaseClosePublicationError<PublisherError>>,
    ownership: Box<PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>>,
}

impl<OuterOwner, Source, Store, CheckpointSource, PublisherError, const N: usize>
    FailedDatabaseClosePublication<OuterOwner, Source, Store, CheckpointSource, PublisherError, N>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    fn new(
        state: DatabaseCleanManifestPublicationState,
        error: DatabaseClosePublicationError<PublisherError>,
        ownership: PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>,
    ) -> Self {
        Self {
            state,
            error: Box::new(error),
            ownership: Box::new(ownership),
        }
    }

    /// Returns the recovery-required source composition.
    #[must_use]
    pub const fn source_identity(&self) -> DatabaseCompositionIdentity {
        self.ownership.source_manifest().composition_identity()
    }

    /// Returns the proposed adjacent clean composition.
    #[must_use]
    pub const fn target_identity(&self) -> DatabaseCompositionIdentity {
        self.ownership.target_identity()
    }

    /// Returns the durable selection knowledge at the failure boundary.
    #[must_use]
    pub const fn state(&self) -> DatabaseCleanManifestPublicationState {
        self.state
    }

    /// Borrows the exact typed failure cause.
    #[must_use]
    pub const fn error(&self) -> &DatabaseClosePublicationError<PublisherError> {
        &self.error
    }

    /// Returns the exact recovery-required source manifest.
    #[must_use]
    pub const fn source_manifest(&self) -> DatabaseManifest {
        self.ownership.source_manifest()
    }

    /// Returns the exact clean target manifest.
    #[must_use]
    pub const fn target_manifest(&self) -> DatabaseManifest {
        self.ownership.target_manifest()
    }

    /// Relinquishes every retained owner without making a selection claim.
    pub fn abandon(self) -> AbandonedDatabaseClosePublication {
        let source_identity = self.source_identity();
        let target_identity = self.ownership.target_identity();
        let state = self.state;
        drop(self);
        AbandonedDatabaseClosePublication {
            source_identity,
            target_identity,
            state,
        }
    }
}

impl<OuterOwner, Source, Store, CheckpointSource, PublisherError, const N: usize> fmt::Debug
    for FailedDatabaseClosePublication<
        OuterOwner,
        Source,
        Store,
        CheckpointSource,
        PublisherError,
        N,
    >
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
    PublisherError: fmt::Debug,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FailedDatabaseClosePublication")
            .field("source_identity", &self.source_identity())
            .field("target_identity", &self.target_identity())
            .field("state", &self.state)
            .field("error", &self.error)
            .field("ownership", &format_args!("<retained>"))
            .finish()
    }
}

/// Closed owner or terminal owner-retaining publication failure.
pub type DatabaseClosePublicationResult<
    OuterOwner,
    Source,
    Store,
    CheckpointSource,
    const N: usize,
> = Result<
    ClosedDatabase<PublishedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>>,
    FailedDatabaseClosePublication<
        OuterOwner,
        Source,
        Store,
        CheckpointSource,
        <OuterOwner as DatabaseCleanManifestPublisher>::Error,
        N,
    >,
>;

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize>
    ClosePendingDatabase<
        PreparedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>,
    >
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
    OuterOwner: DatabaseCleanManifestPublisher,
{
    /// Consumes ClosePending and publishes the exact clean manifest.
    pub fn close(
        self,
        input: OuterOwner::Input,
    ) -> DatabaseClosePublicationResult<OuterOwner, Source, Store, CheckpointSource, N> {
        let Self {
            _owner: mut ownership,
            identity: source_identity,
        } = self;
        let expected_source = ownership.source_manifest;
        let actual_source = ownership.outer_owner.close_source_manifest();
        if actual_source != expected_source {
            return Err(FailedDatabaseClosePublication::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                DatabaseClosePublicationError::Preflight(
                    DatabaseClosePublicationPreflightError::SourceManifestChanged {
                        expected: expected_source,
                        actual: actual_source,
                    },
                ),
                ownership,
            ));
        }
        if let Err(source) = actual_source
            .composition_identity()
            .require_exact_match(source_identity)
        {
            return Err(FailedDatabaseClosePublication::new(
                DatabaseCleanManifestPublicationState::SourceSelected,
                DatabaseClosePublicationError::Preflight(
                    DatabaseClosePublicationPreflightError::SourceManifestIdentity(source),
                ),
                ownership,
            ));
        }
        if !matches!(
            actual_source.lifecycle_state(),
            DatabaseManifestLifecycleState::RecoveryRequired
        ) {
            return Err(FailedDatabaseClosePublication::new(
                DatabaseCleanManifestPublicationState::SourceSelected,
                DatabaseClosePublicationError::Preflight(
                    DatabaseClosePublicationPreflightError::SourceManifestLifecycle {
                        actual: actual_source.lifecycle_state(),
                    },
                ),
                ownership,
            ));
        }
        let actual_target = match actual_source.next_clean(ownership.certificate) {
            Ok(target) => target,
            Err(source) => {
                return Err(FailedDatabaseClosePublication::new(
                    DatabaseCleanManifestPublicationState::SourceSelected,
                    DatabaseClosePublicationError::Preflight(
                        DatabaseClosePublicationPreflightError::TargetManifest(source),
                    ),
                    ownership,
                ));
            }
        };
        let expected_target = ownership.target_manifest;
        if actual_target != expected_target {
            return Err(FailedDatabaseClosePublication::new(
                DatabaseCleanManifestPublicationState::SourceSelected,
                DatabaseClosePublicationError::Preflight(
                    DatabaseClosePublicationPreflightError::TargetManifestChanged {
                        expected: expected_target,
                        actual: actual_target,
                    },
                ),
                ownership,
            ));
        }

        let publication =
            with_database_clean_manifest_publication_permit(expected_target, |permit| {
                ownership.outer_owner.publish_clean_manifest(input, permit)
            });
        let receipt = match publication {
            Ok(receipt) => receipt,
            Err(failure) => {
                let (state, error) = failure.into_parts();
                return Err(FailedDatabaseClosePublication::new(
                    state,
                    DatabaseClosePublicationError::Publisher(error),
                    ownership,
                ));
            }
        };
        if receipt.selected_manifest != expected_target {
            return Err(FailedDatabaseClosePublication::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                DatabaseClosePublicationError::Receipt(
                    DatabaseCleanManifestPublicationReceiptError::SelectedManifest {
                        expected: expected_target,
                        actual: receipt.selected_manifest,
                    },
                ),
                ownership,
            ));
        }
        if receipt.synchronized_manifest != expected_target {
            return Err(FailedDatabaseClosePublication::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                DatabaseClosePublicationError::Receipt(
                    DatabaseCleanManifestPublicationReceiptError::SynchronizedManifest {
                        expected: expected_target,
                        actual: receipt.synchronized_manifest,
                    },
                ),
                ownership,
            ));
        }

        Ok(ClosedDatabase {
            _owner: PublishedDatabaseCloseOwnership {
                prepared: ownership,
                receipt,
            },
            identity: expected_target.composition_identity(),
        })
    }
}

/// Terminal record of relinquished authority after an effectful close attempt.
#[must_use]
pub struct AbandonedDatabaseClosePublication {
    source_identity: DatabaseCompositionIdentity,
    target_identity: DatabaseCompositionIdentity,
    state: DatabaseCleanManifestPublicationState,
}

impl AbandonedDatabaseClosePublication {
    /// Returns the recovery-required source composition.
    #[must_use]
    pub const fn source_identity(&self) -> DatabaseCompositionIdentity {
        self.source_identity
    }

    /// Returns the clean composition targeted by the publication attempt.
    #[must_use]
    pub const fn target_identity(&self) -> DatabaseCompositionIdentity {
        self.target_identity
    }

    /// Returns the final durable selection knowledge.
    #[must_use]
    pub const fn state(&self) -> DatabaseCleanManifestPublicationState {
        self.state
    }

    /// Returns this terminal in-process lifecycle outcome.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        DatabaseLifecycleStage::Abandoned
    }
}

impl fmt::Debug for AbandonedDatabaseClosePublication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AbandonedDatabaseClosePublication")
            .field("source_identity", &self.source_identity)
            .field("target_identity", &self.target_identity)
            .field("state", &self.state)
            .finish()
    }
}

impl<Owner> ClosePendingDatabase<Owner> {
    /// Relinquishes close-pending authority without publishing clean state.
    pub fn abandon(self) -> AbandonedDatabase {
        let Self {
            _owner: owner,
            identity,
        } = self;
        drop(owner);
        AbandonedDatabase { identity }
    }
}

define_owned_database_state!(
    /// Exact composition that completed an orderly terminal close.
    ///
    /// Construction remains private until a synchronized clean-close certificate
    /// exists.
    #[must_use = "closed database ownership must be reopened, dropped, or released"]
    ClosedDatabase,
    DatabaseLifecycleStage::Closed
);

impl<OuterOwner, Source, Store, CheckpointSource, const N: usize>
    ClosedDatabase<PublishedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N>>
where
    Source: DurableTransactionRestartAnalysisSource<N>,
    Store: DurablePageStoreSnapshotSource<N>,
{
    /// Borrows the exact published owner set and receipt retained by Closed.
    pub const fn published(
        &self,
    ) -> &PublishedDatabaseCloseOwnership<OuterOwner, Source, Store, CheckpointSource, N> {
        &self._owner
    }
}

/// Terminal inert record of authority relinquished without clean publication.
///
/// The retained durable manifest remains recovery-required at `identity`.
#[must_use]
pub struct AbandonedDatabase {
    identity: DatabaseCompositionIdentity,
}

impl AbandonedDatabase {
    /// Returns the last selected recovery-required composition.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.identity
    }

    /// Returns this terminal in-process lifecycle outcome.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        DatabaseLifecycleStage::Abandoned
    }
}

impl fmt::Debug for AbandonedDatabase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AbandonedDatabase")
            .field("identity", &self.identity)
            .finish()
    }
}

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

    fn certificate(
        source_generation: DatabaseLifecycleGeneration,
        durable_wal_frontier: Option<u64>,
        allocated_transaction_epoch_high_water: u64,
        checkpoint_anchor_version: u16,
        checkpoint_anchor_value: u128,
        transaction_entry_count: u64,
        page_entry_count: u64,
    ) -> Result<DatabaseCleanCloseCertificate, DatabaseCleanCloseCertificateError> {
        DatabaseCleanCloseCertificate::new(
            source_generation,
            durable_wal_frontier,
            allocated_transaction_epoch_high_water,
            checkpoint_anchor_version,
            checkpoint_anchor_value,
            transaction_entry_count,
            page_entry_count,
        )
    }

    #[test]
    fn clean_close_certificate_rejects_zero_optional_frontier_and_zero_scalars()
    -> Result<(), TestValueError> {
        assert_eq!(
            certificate(generation(1)?, Some(0), 5, 6, 7, 8, 9),
            Err(DatabaseCleanCloseCertificateError::DurableWalFrontierZero)
        );
        assert_eq!(
            certificate(generation(1)?, None, 0, 6, 7, 8, 9),
            Err(DatabaseCleanCloseCertificateError::AllocatedTransactionEpochHighWaterZero)
        );
        assert_eq!(
            certificate(generation(1)?, None, 5, 0, 7, 8, 9),
            Err(DatabaseCleanCloseCertificateError::CheckpointAnchorVersionZero)
        );

        let absent_frontier = certificate(generation(1)?, None, 5, 6, 0, 0, 0)
            .map_err(|_| TestValueError("absent-frontier certificate must construct"))?;
        assert_eq!(absent_frontier.durable_wal_frontier(), None);
        assert_eq!(absent_frontier.checkpoint_anchor_value(), 0);
        assert_eq!(absent_frontier.transaction_entry_count(), 0);
        assert_eq!(absent_frontier.page_entry_count(), 0);

        let maximum = certificate(
            generation(u64::MAX)?,
            Some(u64::MAX),
            u64::MAX,
            u16::MAX,
            u128::MAX,
            u64::MAX,
            u64::MAX,
        )
        .map_err(|_| TestValueError("maximum-field certificate must construct"))?;
        assert_eq!(maximum.source_generation(), generation(u64::MAX)?);
        assert_eq!(maximum.durable_wal_frontier(), Some(u64::MAX));
        assert_eq!(maximum.allocated_transaction_epoch_high_water(), u64::MAX);
        assert_eq!(maximum.checkpoint_anchor_version(), u16::MAX);
        assert_eq!(maximum.checkpoint_anchor_value(), u128::MAX);
        assert_eq!(maximum.transaction_entry_count(), u64::MAX);
        assert_eq!(maximum.page_entry_count(), u64::MAX);
        Ok(())
    }

    #[test]
    fn clean_manifest_requires_exact_predecessor_source_generation() -> Result<(), TestValueError> {
        let recovery_required = manifest(1, 2, 3, 4)?;
        let next_identity = recovery_required
            .composition_identity()
            .next_generation()
            .map_err(|_| TestValueError("test composition must advance"))?;

        // Certificate source generation 2 is the exact predecessor of target generation 3.
        let matching_certificate = certificate(generation(2)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        let clean = DatabaseManifest::clean(
            next_identity,
            recovery_required.storage_formats(),
            recovery_required.required_features(),
            matching_certificate,
        )
        .map_err(|_| TestValueError("exact predecessor certificate must select clean"))?;
        assert_eq!(
            clean.lifecycle_state(),
            DatabaseManifestLifecycleState::Clean(matching_certificate)
        );
        assert_eq!(clean.composition_identity(), next_identity);

        // Certificate source generation 3 equals the target generation: regression.
        let regressed_certificate = certificate(generation(3)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        assert_eq!(
            DatabaseManifest::clean(
                next_identity,
                recovery_required.storage_formats(),
                recovery_required.required_features(),
                regressed_certificate,
            ),
            Err(
                DatabaseLifecycleGenerationTransitionError::NotStrictlyIncreasing {
                    current: generation(3)?,
                    proposed: generation(3)?,
                }
            )
        );

        // Certificate source generation 1 skips the exact predecessor (2).
        let skipped_certificate = certificate(generation(1)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        assert_eq!(
            DatabaseManifest::clean(
                next_identity,
                recovery_required.storage_formats(),
                recovery_required.required_features(),
                skipped_certificate,
            ),
            Err(DatabaseLifecycleGenerationTransitionError::Skipped {
                expected: generation(2)?,
                proposed: generation(3)?,
            })
        );

        let exhausted_certificate = certificate(generation(u64::MAX)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        assert_eq!(
            DatabaseManifest::clean(
                recovery_required.composition_identity(),
                recovery_required.storage_formats(),
                recovery_required.required_features(),
                exhausted_certificate,
            ),
            Err(DatabaseLifecycleGenerationTransitionError::Exhausted {
                current: generation(u64::MAX)?,
            })
        );
        Ok(())
    }

    #[test]
    fn manifest_lifecycle_transitions_reject_only_clean_to_clean() -> Result<(), TestValueError> {
        let generation_one = manifest(1, 1, 3, 4)?;
        let generation_two = manifest(1, 2, 3, 4)?;
        let certificate_for_two = certificate(generation(1)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        let clean_two = DatabaseManifest::clean(
            generation_two.composition_identity(),
            generation_two.storage_formats(),
            generation_two.required_features(),
            certificate_for_two,
        )
        .map_err(|_| TestValueError("clean manifest must select"))?;

        // RecoveryRequired -> RecoveryRequired is allowed.
        assert_eq!(generation_two.require_successor_of(generation_one), Ok(()));
        // RecoveryRequired -> Clean is allowed.
        assert_eq!(clean_two.require_successor_of(generation_one), Ok(()));

        let certificate_for_three = certificate(generation(2)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        let generation_three = manifest(1, 3, 3, 4)?;
        let clean_three = DatabaseManifest::clean(
            generation_three.composition_identity(),
            generation_three.storage_formats(),
            generation_three.required_features(),
            certificate_for_three,
        )
        .map_err(|_| TestValueError("clean manifest must select"))?;
        // Clean -> RecoveryRequired is allowed.
        assert_eq!(generation_three.require_successor_of(clean_two), Ok(()));
        // Clean -> Clean is rejected.
        assert_eq!(
            clean_three.require_successor_of(clean_two),
            Err(DatabaseManifestSuccessorError::LifecycleTransition(
                DatabaseManifestLifecycleTransitionError::CleanToClean
            ))
        );
        assert_eq!(
            clean_two.next_clean(certificate_for_three),
            Err(DatabaseManifestCleanSuccessorError::LifecycleTransition(
                DatabaseManifestLifecycleTransitionError::CleanToClean
            ))
        );
        Ok(())
    }

    #[test]
    fn next_clean_advances_generation_and_binds_fresh_certificate() -> Result<(), TestValueError> {
        let recovery_required = manifest(1, 2, 3, 4)?;
        let certificate_for_three = certificate(generation(2)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        let clean = recovery_required
            .next_clean(certificate_for_three)
            .map_err(|_| TestValueError("next_clean must select with exact predecessor"))?;
        assert_eq!(
            clean.composition_identity().lifecycle_generation(),
            generation(3)?
        );
        assert_eq!(clean.require_successor_of(recovery_required), Ok(()));

        let mismatched_certificate = certificate(generation(1)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        assert_eq!(
            recovery_required.next_clean(mismatched_certificate),
            Err(DatabaseManifestCleanSuccessorError::SourceGeneration(
                DatabaseLifecycleGenerationTransitionError::Skipped {
                    expected: generation(2)?,
                    proposed: generation(3)?,
                }
            ))
        );

        let exhausted = manifest(1, u64::MAX, 3, 4)?;
        let exhausted_certificate = certificate(generation(u64::MAX)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        assert_eq!(
            exhausted.next_clean(exhausted_certificate),
            Err(DatabaseManifestCleanSuccessorError::Exhausted(
                DatabaseLifecycleGenerationExhausted {
                    current: generation(u64::MAX)?,
                }
            ))
        );
        Ok(())
    }

    #[test]
    fn staged_owner_rejects_foreign_manifest_and_composition() -> Result<(), TestValueError> {
        let formats = DatabaseStorageFormatRequirements::new(
            format_version(1)?,
            format_version(1)?,
            format_version(1)?,
        );
        let expected_identity = composition(1, 2, 3, 4, 5, 6)?;
        let expected = DatabaseManifest::recovery_required(
            expected_identity,
            formats,
            DatabaseRequiredFeatures::NONE,
        );
        let foreign_database = DatabaseManifest::recovery_required(
            composition(9, 2, 3, 4, 5, 6)?,
            formats,
            DatabaseRequiredFeatures::NONE,
        );
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
            &DatabaseRecoveryRequiredBindingRejection::StorageIdentity(
                DatabaseCompositionIdentityMismatch::FileId {
                    role: DatabaseFileRole::Wal,
                    expected: file_id(3)?,
                    actual: file_id(9)?,
                }
            )
        );
        let (selected, rejected) = (*binding_error).into_parts();
        assert_eq!(rejected, foreign_wal);

        let recovery_required = selected
            .bind_observed_storage(expected_identity.storage_identity())
            .map_err(|_| TestValueError("exact storage must bind"))?;
        assert_eq!(
            recovery_required.stage(),
            DatabaseLifecycleStage::RecoveryRequired
        );
        assert_eq!(recovery_required.identity(), expected_identity);
        Ok(())
    }

    #[test]
    fn recovery_required_binding_rejects_a_selected_clean_manifest() -> Result<(), TestValueError> {
        let source = manifest(1, 2, 3, 1)?;
        let certificate = certificate(generation(2)?, None, 5, 6, 7, 8, 9)
            .map_err(|_| TestValueError("certificate must construct"))?;
        let clean = source
            .next_clean(certificate)
            .map_err(|_| TestValueError("clean manifest must construct"))?;
        let selected = UnboundDatabase::new("database-owner", database_id(1)?)
            .select_manifest(clean)
            .map_err(|_| TestValueError("clean manifest must remain observable"))?;
        assert_eq!(selected.manifest(), clean);

        let failure = selected
            .bind_observed_storage(clean.composition_identity().storage_identity())
            .err()
            .ok_or(TestValueError(
                "clean manifest must not acquire recovery-required authority",
            ))?;
        assert_eq!(
            failure.reason(),
            &DatabaseRecoveryRequiredBindingRejection::ManifestLifecycle {
                actual: DatabaseManifestLifecycleState::Clean(certificate),
            }
        );
        Ok(())
    }
}
