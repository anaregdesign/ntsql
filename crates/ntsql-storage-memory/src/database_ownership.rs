use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, btree_map::Entry},
    error::Error,
    fmt,
    num::NonZeroU128,
    rc::Rc,
};

use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseCompositionIdentityError,
    DatabaseCompositionIdentityMismatch, DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole,
    DatabaseId, DatabaseLifecycleStage, DatabaseManifest, DatabaseManifestSelectionRejection,
    DatabaseStorageFormatVersion, DatabaseStorageIdentity, ManifestSelectedDatabase,
    RecoveryRequiredDatabase, UnboundDatabase,
};
use ntsql_wal::PersistentLogId;

/// Nonzero deterministic identity for one modeled opened storage object.
///
/// This is memory-adapter test state, not a database/file identity, path, inode,
/// lock token, or persistent authority.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InMemoryDatabaseObjectId(NonZeroU128);

impl InMemoryDatabaseObjectId {
    /// Wraps one nonzero modeled object identity.
    #[must_use]
    pub const fn new(value: u128) -> Option<Self> {
        match NonZeroU128::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the deterministic numeric test identity.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0.get()
    }
}

/// Stable acquisition-order role for one modeled database object.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InMemoryDatabaseObjectRole {
    /// Stable database-owner object.
    DatabaseOwner,
    /// Selected manifest object.
    Manifest,
    /// WAL object.
    Wal,
    /// Page-store object.
    PageStore,
    /// Restart-checkpoint object.
    RestartCheckpoint,
}

impl fmt::Display for InMemoryDatabaseObjectRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseOwner => formatter.write_str("database owner"),
            Self::Manifest => formatter.write_str("database manifest"),
            Self::Wal => formatter.write_str("WAL"),
            Self::PageStore => formatter.write_str("page store"),
            Self::RestartCheckpoint => formatter.write_str("restart checkpoint"),
        }
    }
}

impl From<DatabaseFileRole> for InMemoryDatabaseObjectRole {
    fn from(role: DatabaseFileRole) -> Self {
        match role {
            DatabaseFileRole::Wal => Self::Wal,
            DatabaseFileRole::PageStore => Self::PageStore,
            DatabaseFileRole::RestartCheckpoint => Self::RestartCheckpoint,
        }
    }
}

/// Deterministic observed identity and format of one modeled child object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InMemoryDatabaseFileObservation {
    role: DatabaseFileRole,
    file_id: DatabaseFileId,
    object_id: InMemoryDatabaseObjectId,
    persistent_log_id: PersistentLogId,
    format_version: DatabaseStorageFormatVersion,
}

impl InMemoryDatabaseFileObservation {
    /// Records one modeled opened child object.
    #[must_use]
    pub const fn new(
        role: DatabaseFileRole,
        file_id: DatabaseFileId,
        object_id: InMemoryDatabaseObjectId,
        persistent_log_id: PersistentLogId,
        format_version: DatabaseStorageFormatVersion,
    ) -> Self {
        Self {
            role,
            file_id,
            object_id,
            persistent_log_id,
            format_version,
        }
    }

    /// Returns the modeled child role.
    #[must_use]
    pub const fn role(self) -> DatabaseFileRole {
        self.role
    }

    /// Returns the modeled logical file identity.
    #[must_use]
    pub const fn file_id(self) -> DatabaseFileId {
        self.file_id
    }

    /// Returns the independent modeled opened-object identity.
    #[must_use]
    pub const fn object_id(self) -> InMemoryDatabaseObjectId {
        self.object_id
    }

    /// Returns the observed persistent WAL identity.
    #[must_use]
    pub const fn persistent_log_id(self) -> PersistentLogId {
        self.persistent_log_id
    }

    /// Returns the observed physical format requirement.
    #[must_use]
    pub const fn format_version(self) -> DatabaseStorageFormatVersion {
        self.format_version
    }
}

#[derive(Debug)]
struct InMemoryDatabaseOwnershipState {
    binding: Cell<Option<InMemoryDatabaseObjectBinding>>,
    owned: Cell<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InMemoryDatabaseObjectBinding {
    database_id: DatabaseId,
    role: InMemoryDatabaseObjectRole,
}

/// One deterministic ownership universe for modeled database objects.
///
/// Equal object IDs resolve to the same private ownership state within a world.
/// Separate worlds represent separate model executions, not concurrent views of
/// one database.
#[derive(Debug)]
pub struct InMemoryDatabaseOwnershipWorld {
    states: Rc<RefCell<BTreeMap<InMemoryDatabaseObjectId, Rc<InMemoryDatabaseOwnershipState>>>>,
}

impl InMemoryDatabaseOwnershipWorld {
    /// Creates one empty deterministic ownership universe.
    #[must_use]
    pub fn new() -> Self {
        Self {
            states: Rc::new(RefCell::new(BTreeMap::new())),
        }
    }

    /// Resolves one stable object to its shared ownership slot.
    pub fn slot(
        &mut self,
        database_id: DatabaseId,
        object_id: InMemoryDatabaseObjectId,
    ) -> Result<InMemoryDatabaseOwnershipSlot, InMemoryDatabaseOwnershipSlotError> {
        let state = resolve_world_state(&self.states, object_id);
        let requested = InMemoryDatabaseObjectBinding {
            database_id,
            role: InMemoryDatabaseObjectRole::DatabaseOwner,
        };
        if let Some(bound) = state.binding.get() {
            if bound != requested {
                return Err(InMemoryDatabaseOwnershipSlotError::ObjectBindingMismatch {
                    object_id,
                    bound_database_id: bound.database_id,
                    bound_role: bound.role,
                    requested_database_id: requested.database_id,
                    requested_role: requested.role,
                });
            }
        } else {
            if state.owned.get() {
                return Err(InMemoryDatabaseOwnershipSlotError::ObjectCurrentlyOwned { object_id });
            }
            state.binding.set(Some(requested));
        }
        Ok(InMemoryDatabaseOwnershipSlot {
            database_id,
            object_id,
            state,
            world: Rc::clone(&self.states),
        })
    }
}

impl Default for InMemoryDatabaseOwnershipWorld {
    fn default() -> Self {
        Self::new()
    }
}

fn resolve_world_state(
    world: &Rc<RefCell<BTreeMap<InMemoryDatabaseObjectId, Rc<InMemoryDatabaseOwnershipState>>>>,
    object_id: InMemoryDatabaseObjectId,
) -> Rc<InMemoryDatabaseOwnershipState> {
    match world.borrow_mut().entry(object_id) {
        Entry::Occupied(entry) => Rc::clone(entry.get()),
        Entry::Vacant(entry) => {
            let state = Rc::new(InMemoryDatabaseOwnershipState {
                binding: Cell::new(None),
                owned: Cell::new(false),
            });
            entry.insert(Rc::clone(&state));
            state
        }
    }
}

/// Rejection while resolving one stable object inside a memory world.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryDatabaseOwnershipSlotError {
    /// One modeled object was already bound to another database role.
    ObjectBindingMismatch {
        /// Stable modeled object identity.
        object_id: InMemoryDatabaseObjectId,
        /// Database identity already bound inside this world.
        bound_database_id: DatabaseId,
        /// Role already bound inside this world.
        bound_role: InMemoryDatabaseObjectRole,
        /// Contradictory requested database identity.
        requested_database_id: DatabaseId,
        /// Contradictory requested role.
        requested_role: InMemoryDatabaseObjectRole,
    },
    /// An unbound object is currently retained by another acquisition.
    ObjectCurrentlyOwned {
        /// Stable modeled object identity.
        object_id: InMemoryDatabaseObjectId,
    },
}

impl fmt::Display for InMemoryDatabaseOwnershipSlotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObjectBindingMismatch {
                object_id,
                bound_database_id,
                bound_role,
                requested_database_id,
                requested_role,
            } => write!(
                formatter,
                "modeled object {} is bound to database {} {bound_role}, not database {} {requested_role}",
                object_id.get(),
                bound_database_id.get(),
                requested_database_id.get()
            ),
            Self::ObjectCurrentlyOwned { object_id } => write!(
                formatter,
                "unbound modeled object {} is currently owned",
                object_id.get()
            ),
        }
    }
}

impl Error for InMemoryDatabaseOwnershipSlotError {}

/// Deterministic shared slot from which one non-cloneable database owner may be acquired.
#[derive(Clone, Debug)]
pub struct InMemoryDatabaseOwnershipSlot {
    database_id: DatabaseId,
    object_id: InMemoryDatabaseObjectId,
    state: Rc<InMemoryDatabaseOwnershipState>,
    world: Rc<RefCell<BTreeMap<InMemoryDatabaseObjectId, Rc<InMemoryDatabaseOwnershipState>>>>,
}

impl InMemoryDatabaseOwnershipSlot {
    /// Returns the stable database identity bound to this slot.
    #[must_use]
    pub const fn database_id(&self) -> DatabaseId {
        self.database_id
    }

    /// Returns the modeled opened-object identity of this stable slot.
    #[must_use]
    pub const fn object_id(&self) -> InMemoryDatabaseObjectId {
        self.object_id
    }

    /// Returns whether one modeled owner currently retains the slot.
    #[must_use]
    pub fn is_owned(&self) -> bool {
        self.state.owned.get()
    }

    /// Acquires and validates one deterministic manifest-selected composition.
    ///
    /// Contention is checked before any supplied evidence. Every later rejection
    /// releases the acquisition guard before returning.
    pub fn try_acquire(
        &self,
        expected_database_id: DatabaseId,
        manifest_object_id: InMemoryDatabaseObjectId,
        manifest: DatabaseManifest,
        files: &[InMemoryDatabaseFileObservation],
    ) -> Result<InMemoryDatabaseOwnershipSelection, InMemoryDatabaseOwnershipError> {
        let acquired = self.acquire(expected_database_id, manifest_object_id, manifest, files)?;
        Ok(InMemoryDatabaseOwnershipSelection {
            selected: acquired.selected,
        })
    }

    /// Acquires exact stable storage and crosses the recovery-required boundary.
    pub fn try_acquire_recovery_required(
        &self,
        expected_database_id: DatabaseId,
        manifest_object_id: InMemoryDatabaseObjectId,
        manifest: DatabaseManifest,
        files: &[InMemoryDatabaseFileObservation],
    ) -> Result<RecoveryRequiredInMemoryDatabase, InMemoryDatabaseOwnershipError> {
        let acquired = self.acquire(expected_database_id, manifest_object_id, manifest, files)?;
        acquired
            .selected
            .bind_observed_storage(acquired.observed_storage_identity)
            .map_err(|failure| InMemoryDatabaseOwnershipError::StorageBinding(*failure.reason()))
    }

    fn acquire(
        &self,
        expected_database_id: DatabaseId,
        manifest_object_id: InMemoryDatabaseObjectId,
        manifest: DatabaseManifest,
        files: &[InMemoryDatabaseFileObservation],
    ) -> Result<AcquiredInMemoryDatabaseOwnership, InMemoryDatabaseOwnershipError> {
        let mut guard = InMemoryDatabaseOwnershipGuard::new();
        guard.acquire(
            self.object_id,
            Rc::clone(&self.state),
            InMemoryDatabaseObjectBinding {
                database_id: self.database_id,
                role: InMemoryDatabaseObjectRole::DatabaseOwner,
            },
        )?;

        if self.database_id != expected_database_id {
            return Err(InMemoryDatabaseOwnershipError::DatabaseOwnerIdMismatch {
                expected: expected_database_id,
                actual: self.database_id,
            });
        }
        reject_object_alias(
            InMemoryDatabaseObjectRole::DatabaseOwner,
            self.object_id,
            InMemoryDatabaseObjectRole::Manifest,
            manifest_object_id,
        )?;
        guard.acquire(
            manifest_object_id,
            resolve_world_state(&self.world, manifest_object_id),
            InMemoryDatabaseObjectBinding {
                database_id: self.database_id,
                role: InMemoryDatabaseObjectRole::Manifest,
            },
        )?;
        let manifest_database_id = manifest.composition_identity().database_id();
        if manifest_database_id != self.database_id {
            return Err(InMemoryDatabaseOwnershipError::ManifestDatabaseIdMismatch {
                owner: self.database_id,
                manifest: manifest_database_id,
            });
        }

        let wal = require_exact_role(files, DatabaseFileRole::Wal)?;
        let page_store = require_exact_role(files, DatabaseFileRole::PageStore)?;
        let restart_checkpoint = require_exact_role(files, DatabaseFileRole::RestartCheckpoint)?;

        validate_object_against_prefix(
            wal,
            &[
                (InMemoryDatabaseObjectRole::DatabaseOwner, self.object_id),
                (InMemoryDatabaseObjectRole::Manifest, manifest_object_id),
            ],
        )?;
        guard.acquire(
            wal.object_id,
            resolve_world_state(&self.world, wal.object_id),
            InMemoryDatabaseObjectBinding {
                database_id: self.database_id,
                role: InMemoryDatabaseObjectRole::Wal,
            },
        )?;
        validate_file(manifest, wal)?;
        validate_object_against_prefix(
            page_store,
            &[
                (InMemoryDatabaseObjectRole::DatabaseOwner, self.object_id),
                (InMemoryDatabaseObjectRole::Manifest, manifest_object_id),
                (InMemoryDatabaseObjectRole::Wal, wal.object_id),
            ],
        )?;
        guard.acquire(
            page_store.object_id,
            resolve_world_state(&self.world, page_store.object_id),
            InMemoryDatabaseObjectBinding {
                database_id: self.database_id,
                role: InMemoryDatabaseObjectRole::PageStore,
            },
        )?;
        validate_file(manifest, page_store)?;
        validate_object_against_prefix(
            restart_checkpoint,
            &[
                (InMemoryDatabaseObjectRole::DatabaseOwner, self.object_id),
                (InMemoryDatabaseObjectRole::Manifest, manifest_object_id),
                (InMemoryDatabaseObjectRole::Wal, wal.object_id),
                (InMemoryDatabaseObjectRole::PageStore, page_store.object_id),
            ],
        )?;
        guard.acquire(
            restart_checkpoint.object_id,
            resolve_world_state(&self.world, restart_checkpoint.object_id),
            InMemoryDatabaseObjectBinding {
                database_id: self.database_id,
                role: InMemoryDatabaseObjectRole::RestartCheckpoint,
            },
        )?;
        validate_file(manifest, restart_checkpoint)?;

        let ordered_files = [wal, page_store, restart_checkpoint];
        let observed_files = [
            DatabaseFileIdentity::new(wal.role, wal.file_id),
            DatabaseFileIdentity::new(page_store.role, page_store.file_id),
            DatabaseFileIdentity::new(restart_checkpoint.role, restart_checkpoint.file_id),
        ];
        let selected_identity = manifest.composition_identity();
        let observed_storage_identity =
            DatabaseStorageIdentity::new(self.database_id, wal.persistent_log_id, &observed_files)
                .map_err(InMemoryDatabaseOwnershipError::ObservedStorageIdentity)?;
        selected_identity
            .storage_identity()
            .require_exact_match(observed_storage_identity)
            .map_err(InMemoryDatabaseOwnershipError::StorageBinding)?;
        guard.commit_bindings();
        let owner = InMemoryDatabaseOwnership {
            manifest,
            manifest_object_id,
            files: ordered_files,
            _guard: guard,
        };
        let selected = match UnboundDatabase::new(owner, expected_database_id)
            .select_manifest(selected_identity)
        {
            Ok(selected) => selected,
            Err(failure) => {
                let reason = *failure.reason();
                drop(failure);
                return Err(InMemoryDatabaseOwnershipError::ManifestSelection(reason));
            }
        };
        Ok(AcquiredInMemoryDatabaseOwnership {
            selected,
            observed_storage_identity,
        })
    }
}

/// Recovery-required memory ownership proven by modeled stable child identity.
pub type RecoveryRequiredInMemoryDatabase = RecoveryRequiredDatabase<InMemoryDatabaseOwnership>;

struct AcquiredInMemoryDatabaseOwnership {
    selected: ManifestSelectedDatabase<InMemoryDatabaseOwnership>,
    observed_storage_identity: DatabaseStorageIdentity,
}

/// Complete deterministic memory ownership retained inside lifecycle typestate.
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryDatabaseOwnership;
///
/// fn cannot_clone(ownership: InMemoryDatabaseOwnership) {
///     let first = ownership;
///     let second = ownership;
/// }
/// ```
#[must_use = "database ownership must remain inside its lifecycle typestate"]
pub struct InMemoryDatabaseOwnership {
    manifest: DatabaseManifest,
    manifest_object_id: InMemoryDatabaseObjectId,
    files: [InMemoryDatabaseFileObservation; 3],
    _guard: InMemoryDatabaseOwnershipGuard,
}

impl InMemoryDatabaseOwnership {
    /// Returns the retained inert manifest.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.manifest
    }

    /// Returns the modeled manifest-object identity.
    #[must_use]
    pub const fn manifest_object_id(&self) -> InMemoryDatabaseObjectId {
        self.manifest_object_id
    }

    /// Returns child observations in stable role order.
    #[must_use]
    pub const fn files(&self) -> &[InMemoryDatabaseFileObservation; 3] {
        &self.files
    }
}

impl fmt::Debug for InMemoryDatabaseOwnership {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryDatabaseOwnership")
            .field("manifest", &self.manifest)
            .field("manifest_object_id", &self.manifest_object_id)
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

struct InMemoryDatabaseOwnershipGuard {
    held: [Option<InMemoryDatabaseHeldObject>; 5],
}

struct InMemoryDatabaseHeldObject {
    state: Rc<InMemoryDatabaseOwnershipState>,
    binding: InMemoryDatabaseObjectBinding,
}

impl InMemoryDatabaseOwnershipGuard {
    fn new() -> Self {
        Self {
            held: [None, None, None, None, None],
        }
    }

    fn acquire(
        &mut self,
        object_id: InMemoryDatabaseObjectId,
        state: Rc<InMemoryDatabaseOwnershipState>,
        requested: InMemoryDatabaseObjectBinding,
    ) -> Result<(), InMemoryDatabaseOwnershipError> {
        if let Some(bound) = state.binding.get()
            && bound != requested
        {
            return Err(InMemoryDatabaseOwnershipError::ObjectBindingMismatch {
                object_id,
                bound_database_id: bound.database_id,
                bound_role: bound.role,
                requested_database_id: requested.database_id,
                requested_role: requested.role,
            });
        }
        if state.owned.replace(true) {
            return match requested.role {
                InMemoryDatabaseObjectRole::DatabaseOwner => {
                    Err(InMemoryDatabaseOwnershipError::Contended {
                        database_id: requested.database_id,
                    })
                }
                role => Err(InMemoryDatabaseOwnershipError::ObjectContended { object_id, role }),
            };
        }
        self.held[object_role_index(requested.role)] = Some(InMemoryDatabaseHeldObject {
            state,
            binding: requested,
        });
        Ok(())
    }

    fn commit_bindings(&self) {
        for held in self.held.iter().flatten() {
            if held.state.binding.get().is_none() {
                held.state.binding.set(Some(held.binding));
            }
        }
    }
}

impl fmt::Debug for InMemoryDatabaseOwnershipGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryDatabaseOwnershipGuard")
            .finish_non_exhaustive()
    }
}

impl Drop for InMemoryDatabaseOwnershipGuard {
    fn drop(&mut self) {
        for held in self.held.iter().flatten() {
            held.state.owned.set(false);
        }
    }
}

const fn object_role_index(role: InMemoryDatabaseObjectRole) -> usize {
    match role {
        InMemoryDatabaseObjectRole::DatabaseOwner => 0,
        InMemoryDatabaseObjectRole::Manifest => 1,
        InMemoryDatabaseObjectRole::Wal => 2,
        InMemoryDatabaseObjectRole::PageStore => 3,
        InMemoryDatabaseObjectRole::RestartCheckpoint => 4,
    }
}

/// Manifest-selected memory ownership retaining one unforgeable world guard.
///
/// ```compile_fail
/// use ntsql_storage_memory::InMemoryDatabaseOwnershipSelection;
///
/// fn cannot_claim_exact(selected: InMemoryDatabaseOwnershipSelection) {
///     let observed = selected.identity().storage_identity();
///     selected.bind_observed_storage(observed);
/// }
/// ```
#[must_use = "selected memory ownership must remain retained or be dropped"]
pub struct InMemoryDatabaseOwnershipSelection {
    selected: ManifestSelectedDatabase<InMemoryDatabaseOwnership>,
}

impl InMemoryDatabaseOwnershipSelection {
    /// Returns the selected inert manifest identity.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.selected.identity()
    }

    /// Returns the strongest lifecycle stage exposed by this ownership gate.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        DatabaseLifecycleStage::ManifestSelected
    }
}

impl fmt::Debug for InMemoryDatabaseOwnershipSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryDatabaseOwnershipSelection")
            .field("identity", &self.selected.identity())
            .finish_non_exhaustive()
    }
}

/// Deterministic rejection while acquiring a modeled database composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryDatabaseOwnershipError {
    /// Another modeled owner retains the stable database object.
    Contended {
        /// Contended database identity.
        database_id: DatabaseId,
    },
    /// Another acquisition retains one later modeled object.
    ObjectContended {
        /// Contended modeled object identity.
        object_id: InMemoryDatabaseObjectId,
        /// Role requested by this acquisition.
        role: InMemoryDatabaseObjectRole,
    },
    /// One modeled object is permanently bound to another database role.
    ObjectBindingMismatch {
        /// Contradictory modeled object identity.
        object_id: InMemoryDatabaseObjectId,
        /// Previously bound database identity.
        bound_database_id: DatabaseId,
        /// Previously bound role.
        bound_role: InMemoryDatabaseObjectRole,
        /// Requested database identity.
        requested_database_id: DatabaseId,
        /// Requested role.
        requested_role: InMemoryDatabaseObjectRole,
    },
    /// The stable owner belongs to another requested database.
    DatabaseOwnerIdMismatch {
        /// Requested identity.
        expected: DatabaseId,
        /// Stable owner identity.
        actual: DatabaseId,
    },
    /// Two roles reuse one modeled opened object.
    ObjectAlias {
        /// Earlier role in acquisition order.
        first: InMemoryDatabaseObjectRole,
        /// Later aliased role.
        second: InMemoryDatabaseObjectRole,
    },
    /// The manifest belongs to another stable owner.
    ManifestDatabaseIdMismatch {
        /// Stable owner identity.
        owner: DatabaseId,
        /// Manifest identity.
        manifest: DatabaseId,
    },
    /// One required child role is absent.
    MissingRole {
        /// Missing role.
        role: DatabaseFileRole,
    },
    /// One required child role appears more than once.
    DuplicateRole {
        /// Duplicated role.
        role: DatabaseFileRole,
    },
    /// One child logical file identity contradicts the manifest.
    FileIdMismatch {
        /// Contradictory role.
        role: DatabaseFileRole,
        /// Manifest-required identity.
        expected: DatabaseFileId,
        /// Observed identity.
        actual: DatabaseFileId,
    },
    /// One child format contradicts the manifest.
    StorageFormatVersionMismatch {
        /// Contradictory role.
        role: DatabaseFileRole,
        /// Manifest-required version.
        expected: DatabaseStorageFormatVersion,
        /// Observed version.
        actual: DatabaseStorageFormatVersion,
    },
    /// One child persistent WAL identity contradicts the manifest.
    PersistentLogIdMismatch {
        /// Contradictory role.
        role: DatabaseFileRole,
        /// Manifest-required identity.
        expected: PersistentLogId,
        /// Observed identity.
        actual: PersistentLogId,
    },
    /// The complete observed identity set is internally invalid.
    ObservedStorageIdentity(DatabaseCompositionIdentityError),
    /// The domain manifest-selection gate rejected the modeled owner.
    ManifestSelection(DatabaseManifestSelectionRejection),
    /// Exact comparison rejected the modeled observations.
    StorageBinding(DatabaseCompositionIdentityMismatch),
}

impl fmt::Display for InMemoryDatabaseOwnershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contended { database_id } => {
                write!(formatter, "database {} is already owned", database_id.get())
            }
            Self::ObjectContended { object_id, role } => write!(
                formatter,
                "modeled database {role} object {} is already owned",
                object_id.get()
            ),
            Self::ObjectBindingMismatch {
                object_id,
                bound_database_id,
                bound_role,
                requested_database_id,
                requested_role,
            } => write!(
                formatter,
                "modeled object {} is bound to database {} {bound_role}, not database {} {requested_role}",
                object_id.get(),
                bound_database_id.get(),
                requested_database_id.get()
            ),
            Self::DatabaseOwnerIdMismatch { expected, actual } => write!(
                formatter,
                "database owner identity {} does not match requested database {}",
                actual.get(),
                expected.get()
            ),
            Self::ObjectAlias { first, second } => {
                write!(formatter, "modeled database {second} aliases {first}")
            }
            Self::ManifestDatabaseIdMismatch { owner, manifest } => write!(
                formatter,
                "database manifest identity {} does not match owner {}",
                manifest.get(),
                owner.get()
            ),
            Self::MissingRole { role } => write!(formatter, "database is missing the {role} role"),
            Self::DuplicateRole { role } => {
                write!(formatter, "database has a duplicate {role} role")
            }
            Self::FileIdMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "database {role} identity {} does not match manifest identity {}",
                actual.get(),
                expected.get()
            ),
            Self::StorageFormatVersionMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "database {role} format {} does not match manifest format {}",
                actual.get(),
                expected.get()
            ),
            Self::PersistentLogIdMismatch {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "database {role} persistent WAL identity {} does not match manifest identity {}",
                actual.get(),
                expected.get()
            ),
            Self::ObservedStorageIdentity(source) => {
                write!(
                    formatter,
                    "modeled database composition is invalid: {source}"
                )
            }
            Self::ManifestSelection(source) => {
                write!(formatter, "modeled manifest selection failed: {source}")
            }
            Self::StorageBinding(source) => {
                write!(formatter, "modeled storage binding failed: {source}")
            }
        }
    }
}

impl Error for InMemoryDatabaseOwnershipError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ObservedStorageIdentity(source) => Some(source),
            Self::ManifestSelection(source) => Some(source),
            Self::StorageBinding(source) => Some(source),
            Self::Contended { .. }
            | Self::ObjectContended { .. }
            | Self::ObjectBindingMismatch { .. }
            | Self::DatabaseOwnerIdMismatch { .. }
            | Self::ObjectAlias { .. }
            | Self::ManifestDatabaseIdMismatch { .. }
            | Self::MissingRole { .. }
            | Self::DuplicateRole { .. }
            | Self::FileIdMismatch { .. }
            | Self::StorageFormatVersionMismatch { .. }
            | Self::PersistentLogIdMismatch { .. } => None,
        }
    }
}

fn require_exact_role(
    files: &[InMemoryDatabaseFileObservation],
    role: DatabaseFileRole,
) -> Result<InMemoryDatabaseFileObservation, InMemoryDatabaseOwnershipError> {
    let mut found = None;
    for file in files.iter().copied().filter(|file| file.role == role) {
        if found.is_some() {
            return Err(InMemoryDatabaseOwnershipError::DuplicateRole { role });
        }
        found = Some(file);
    }
    found.ok_or(InMemoryDatabaseOwnershipError::MissingRole { role })
}

fn validate_file(
    manifest: DatabaseManifest,
    file: InMemoryDatabaseFileObservation,
) -> Result<(), InMemoryDatabaseOwnershipError> {
    let identity = manifest.composition_identity();
    let expected_file_id = identity.file_id(file.role);
    if file.file_id != expected_file_id {
        return Err(InMemoryDatabaseOwnershipError::FileIdMismatch {
            role: file.role,
            expected: expected_file_id,
            actual: file.file_id,
        });
    }
    let expected_format = manifest.storage_formats().version(file.role);
    if file.format_version != expected_format {
        return Err(
            InMemoryDatabaseOwnershipError::StorageFormatVersionMismatch {
                role: file.role,
                expected: expected_format,
                actual: file.format_version,
            },
        );
    }
    let expected_log = identity.persistent_log_id();
    if file.persistent_log_id != expected_log {
        return Err(InMemoryDatabaseOwnershipError::PersistentLogIdMismatch {
            role: file.role,
            expected: expected_log,
            actual: file.persistent_log_id,
        });
    }
    Ok(())
}

fn validate_object_against_prefix(
    file: InMemoryDatabaseFileObservation,
    prefix: &[(InMemoryDatabaseObjectRole, InMemoryDatabaseObjectId)],
) -> Result<(), InMemoryDatabaseOwnershipError> {
    let role = InMemoryDatabaseObjectRole::from(file.role);
    for (first_role, first_object_id) in prefix {
        reject_object_alias(*first_role, *first_object_id, role, file.object_id)?;
    }
    Ok(())
}

fn reject_object_alias(
    first: InMemoryDatabaseObjectRole,
    first_object_id: InMemoryDatabaseObjectId,
    second: InMemoryDatabaseObjectRole,
    second_object_id: InMemoryDatabaseObjectId,
) -> Result<(), InMemoryDatabaseOwnershipError> {
    if first_object_id == second_object_id {
        return Err(InMemoryDatabaseOwnershipError::ObjectAlias { first, second });
    }
    Ok(())
}
