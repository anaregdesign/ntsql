use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, btree_map::Entry},
    error::Error,
    fmt,
    num::NonZeroU128,
    rc::Rc,
};

use ntsql_compatibility::CompatibilityContext;
use ntsql_database::{
    AbandonedDatabase, AbandonedDatabaseClosePublication, ClosePendingDatabase, ClosedDatabase,
    DatabaseCleanCloseCertificate, DatabaseCleanManifestPublicationPermit,
    DatabaseCleanManifestPublicationReceipt, DatabaseCleanManifestPublicationState,
    DatabaseCleanManifestPublisher, DatabaseCleanManifestPublisherFailure,
    DatabaseCloseSourceManifestOwner, DatabaseCompositionIdentity,
    DatabaseCompositionIdentityError, DatabaseCompositionIdentityMismatch, DatabaseFileId,
    DatabaseFileIdentity, DatabaseFileRole, DatabaseId, DatabaseLifecycleStage, DatabaseManifest,
    DatabaseManifestSelectionRejection, DatabaseManifestSuccessorError,
    DatabaseRecoveryFailureCause, DatabaseRecoveryOwner, DatabaseStorageFormatVersion,
    DatabaseStorageIdentity, FailedDatabaseClosePreparation, FailedDatabaseClosePublication,
    FailedDatabaseRecovery, LiveDatabase, ManifestSelectedDatabase, PreparedDatabaseCloseOwnership,
    PublishedDatabaseCloseOwnership, RecoveredDatabaseOwnership, RecoveryRequiredDatabase,
    UnboundDatabase,
};
use ntsql_transaction::{
    FailedTransactionPageStorageRecoveryHandoff, TransactionCoordinator,
    TransactionPageStorageRecoveryHandoffPhase, UnrecoveredTransactionPageStorage,
    WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay,
    complete_transaction_page_storage_recovery_handoff_with_observer,
};
use ntsql_wal::PersistentLogId;

use super::{
    InMemoryCommitLog, InMemoryPageStore,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
};

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
}

/// One durable phase in deterministic memory database creation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InMemoryDatabaseCreatePhase {
    /// No owner publication has occurred.
    Absent,
    /// Only the stable owner has been published.
    Owner,
    /// The manifest candidate has been published.
    ManifestCandidate,
    /// The WAL candidate has been published.
    WalCandidate,
    /// The page-store candidate has been published.
    PageStoreCandidate,
    /// The restart-checkpoint candidate has been published.
    RestartCheckpointCandidate,
    /// The WAL candidate has moved to the selected entry.
    WalPublished,
    /// The page-store candidate has moved to the selected entry.
    PageStorePublished,
    /// Every child is selected while the manifest remains a candidate.
    ChildrenPublished,
    /// The manifest and every child are selected.
    Published,
}

impl fmt::Display for InMemoryDatabaseCreatePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => formatter.write_str("absent"),
            Self::Owner => formatter.write_str("owner"),
            Self::ManifestCandidate => formatter.write_str("manifest candidate"),
            Self::WalCandidate => formatter.write_str("WAL candidate"),
            Self::PageStoreCandidate => formatter.write_str("page-store candidate"),
            Self::RestartCheckpointCandidate => formatter.write_str("restart-checkpoint candidate"),
            Self::WalPublished => formatter.write_str("WAL published"),
            Self::PageStorePublished => formatter.write_str("page store published"),
            Self::ChildrenPublished => formatter.write_str("children published"),
            Self::Published => formatter.write_str("published"),
        }
    }
}

/// One complete modeled effect boundary in deterministic memory creation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InMemoryDatabaseCreateBoundary {
    /// Stable owner publication.
    OwnerPublication,
    /// Manifest-candidate publication.
    ManifestCandidatePublication,
    /// WAL-candidate publication.
    WalCandidatePublication,
    /// Page-store-candidate publication.
    PageStoreCandidatePublication,
    /// Restart-checkpoint-candidate publication.
    RestartCheckpointCandidatePublication,
    /// WAL selection.
    WalPublication,
    /// Page-store selection.
    PageStorePublication,
    /// Restart-checkpoint selection.
    RestartCheckpointPublication,
    /// Manifest selection.
    ManifestPublication,
}

impl fmt::Display for InMemoryDatabaseCreateBoundary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerPublication => formatter.write_str("owner publication"),
            Self::ManifestCandidatePublication => {
                formatter.write_str("manifest-candidate publication")
            }
            Self::WalCandidatePublication => formatter.write_str("WAL-candidate publication"),
            Self::PageStoreCandidatePublication => {
                formatter.write_str("page-store-candidate publication")
            }
            Self::RestartCheckpointCandidatePublication => {
                formatter.write_str("restart-checkpoint-candidate publication")
            }
            Self::WalPublication => formatter.write_str("WAL publication"),
            Self::PageStorePublication => formatter.write_str("page-store publication"),
            Self::RestartCheckpointPublication => {
                formatter.write_str("restart-checkpoint publication")
            }
            Self::ManifestPublication => formatter.write_str("manifest publication"),
        }
    }
}

/// Physical state and caller certainty for one deterministic memory fault.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum InMemoryDatabaseCreateFaultTiming {
    /// Fail definitely before installing the complete effect.
    BeforeEffect,
    /// Install the complete effect, then report a definite failure.
    AfterEffect,
    /// Keep the prior phase but make the report outcome-indeterminate.
    OutcomeIndeterminateBeforeEffect,
    /// Install the complete effect and make the report outcome-indeterminate.
    OutcomeIndeterminateAfterEffect,
}

impl InMemoryDatabaseCreateFaultTiming {
    /// Returns whether the injected report is outcome-indeterminate.
    #[must_use]
    pub const fn is_outcome_indeterminate(self) -> bool {
        matches!(
            self,
            Self::OutcomeIndeterminateBeforeEffect | Self::OutcomeIndeterminateAfterEffect
        )
    }
}

impl fmt::Display for InMemoryDatabaseCreateFaultTiming {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeEffect => formatter.write_str("before effect"),
            Self::AfterEffect => formatter.write_str("after effect"),
            Self::OutcomeIndeterminateBeforeEffect => {
                formatter.write_str("outcome-indeterminate before effect")
            }
            Self::OutcomeIndeterminateAfterEffect => {
                formatter.write_str("outcome-indeterminate after effect")
            }
        }
    }
}

/// One deterministic one-shot memory create fault.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InMemoryDatabaseCreateFault {
    boundary: InMemoryDatabaseCreateBoundary,
    timing: InMemoryDatabaseCreateFaultTiming,
}

impl InMemoryDatabaseCreateFault {
    /// Selects one exact modeled boundary and timing.
    #[must_use]
    pub const fn new(
        boundary: InMemoryDatabaseCreateBoundary,
        timing: InMemoryDatabaseCreateFaultTiming,
    ) -> Self {
        Self { boundary, timing }
    }

    /// Returns the selected modeled boundary.
    #[must_use]
    pub const fn boundary(self) -> InMemoryDatabaseCreateBoundary {
        self.boundary
    }

    /// Returns the selected physical/certainty timing.
    #[must_use]
    pub const fn timing(self) -> InMemoryDatabaseCreateFaultTiming {
        self.timing
    }
}

impl fmt::Display for InMemoryDatabaseCreateFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.boundary, self.timing)
    }
}

/// Initial create-manifest requirement rejected before modeled mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryDatabaseCreateManifestError {
    /// Initial creation requires lifecycle generation one.
    LifecycleGeneration {
        /// Exact rejected generation.
        actual: u64,
    },
    /// One child format is not the exact initial successor version.
    StorageFormatVersion {
        /// Rejected role.
        role: DatabaseFileRole,
        /// Required initial version.
        expected: u16,
        /// Exact supplied version.
        actual: u16,
    },
    /// Initial creation supports no required feature bit.
    RequiredFeatures {
        /// Exact rejected feature set.
        actual: u64,
    },
}

impl fmt::Display for InMemoryDatabaseCreateManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LifecycleGeneration { actual } => write!(
                formatter,
                "initial database lifecycle generation must be 1, not {actual}"
            ),
            Self::StorageFormatVersion {
                role,
                expected,
                actual,
            } => write!(
                formatter,
                "initial {role} format must be {expected}, not {actual}"
            ),
            Self::RequiredFeatures { actual } => write!(
                formatter,
                "initial database required feature bits must be zero, not {actual:#018x}"
            ),
        }
    }
}

impl Error for InMemoryDatabaseCreateManifestError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InMemoryDatabaseCreateRequest {
    manifest_object_id: InMemoryDatabaseObjectId,
    manifest: DatabaseManifest,
    files: [InMemoryDatabaseFileObservation; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InMemoryDatabaseCreateRecord {
    database_id: DatabaseId,
    phase: InMemoryDatabaseCreatePhase,
    request: Option<InMemoryDatabaseCreateRequest>,
    selected_manifest: Option<DatabaseManifest>,
    close_candidate_manifest: Option<DatabaseManifest>,
    synchronized_manifest: Option<DatabaseManifest>,
}

impl InMemoryDatabaseFileObservation {
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
    creates: Rc<RefCell<BTreeMap<InMemoryDatabaseObjectId, InMemoryDatabaseCreateRecord>>>,
}

impl InMemoryDatabaseOwnershipWorld {
    /// Creates one empty deterministic ownership universe.
    #[must_use]
    pub fn new() -> Self {
        Self {
            states: Rc::new(RefCell::new(BTreeMap::new())),
            creates: Rc::new(RefCell::new(BTreeMap::new())),
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
        } else if state.owned.get() {
            return Err(InMemoryDatabaseOwnershipSlotError::ObjectCurrentlyOwned { object_id });
        }
        Ok(InMemoryDatabaseOwnershipSlot {
            database_id,
            object_id,
            state,
            world: Rc::clone(&self.states),
            creates: Rc::clone(&self.creates),
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
    creates: Rc<RefCell<BTreeMap<InMemoryDatabaseObjectId, InMemoryDatabaseCreateRecord>>>,
}

/// Failure to create or resume one deterministic memory database composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryDatabaseCreateError {
    /// The supplied manifest is not an exact initial create manifest.
    ManifestRequirement(InMemoryDatabaseCreateManifestError),
    /// Existing modeled ownership or physical evidence was rejected.
    Ownership(InMemoryDatabaseOwnershipError),
    /// A legal prefix belongs to another exact create request.
    EvidenceConflict {
        /// Durable phase whose evidence contradicted the request.
        phase: InMemoryDatabaseCreatePhase,
    },
    /// Private modeled state violated the legal phase/request shape.
    StateCorrupt {
        /// Durable phase whose private record was invalid.
        phase: InMemoryDatabaseCreatePhase,
    },
    /// A deterministic fault fired at one exact boundary.
    InjectedFault(InMemoryDatabaseCreateFault),
}

impl InMemoryDatabaseCreateError {
    /// Returns whether the injected report is outcome-indeterminate.
    #[must_use]
    pub const fn is_outcome_indeterminate(self) -> bool {
        match self {
            Self::InjectedFault(fault) => fault.timing().is_outcome_indeterminate(),
            Self::ManifestRequirement(_)
            | Self::Ownership(_)
            | Self::EvidenceConflict { .. }
            | Self::StateCorrupt { .. } => false,
        }
    }
}

impl fmt::Display for InMemoryDatabaseCreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestRequirement(source) => {
                write!(formatter, "memory create manifest is invalid: {source}")
            }
            Self::Ownership(source) => {
                write!(formatter, "memory create evidence is invalid: {source}")
            }
            Self::EvidenceConflict { phase } => {
                write!(
                    formatter,
                    "memory create {phase} evidence conflicts with the request"
                )
            }
            Self::StateCorrupt { phase } => {
                write!(formatter, "memory create private {phase} state is invalid")
            }
            Self::InjectedFault(fault) => {
                write!(formatter, "injected memory database create fault {fault}")
            }
        }
    }
}

impl Error for InMemoryDatabaseCreateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ManifestRequirement(source) => Some(source),
            Self::Ownership(source) => Some(source),
            Self::EvidenceConflict { .. } | Self::StateCorrupt { .. } | Self::InjectedFault(_) => {
                None
            }
        }
    }
}

/// Successful initial memory publication or exact already-published retry.
#[must_use = "created database ownership must remain inside its lifecycle typestate"]
pub enum InMemoryDatabaseCreateOutcome {
    /// This invocation completed manifest-last publication.
    Created(RecoveryRequiredInMemoryDatabase),
    /// The exact composition was already manifest-selected.
    AlreadyPublished(RecoveryRequiredInMemoryDatabase),
}

impl fmt::Debug for InMemoryDatabaseCreateOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created(database) => formatter
                .debug_tuple("Created")
                .field(&database.identity())
                .finish(),
            Self::AlreadyPublished(database) => formatter
                .debug_tuple("AlreadyPublished")
                .field(&database.identity())
                .finish(),
        }
    }
}

struct InMemoryCreateFaultController {
    armed: Option<InMemoryDatabaseCreateFault>,
}

impl InMemoryCreateFaultController {
    const fn new(armed: Option<InMemoryDatabaseCreateFault>) -> Self {
        Self { armed }
    }

    fn before(
        &mut self,
        boundary: InMemoryDatabaseCreateBoundary,
    ) -> Result<(), InMemoryDatabaseCreateError> {
        if let Some(fault) = self.armed
            && fault.boundary() == boundary
            && matches!(
                fault.timing(),
                InMemoryDatabaseCreateFaultTiming::BeforeEffect
                    | InMemoryDatabaseCreateFaultTiming::OutcomeIndeterminateBeforeEffect
            )
        {
            self.armed = None;
            return Err(InMemoryDatabaseCreateError::InjectedFault(fault));
        }
        Ok(())
    }

    fn after(
        &mut self,
        boundary: InMemoryDatabaseCreateBoundary,
    ) -> Result<(), InMemoryDatabaseCreateError> {
        if let Some(fault) = self.armed
            && fault.boundary() == boundary
            && matches!(
                fault.timing(),
                InMemoryDatabaseCreateFaultTiming::AfterEffect
                    | InMemoryDatabaseCreateFaultTiming::OutcomeIndeterminateAfterEffect
            )
        {
            self.armed = None;
            return Err(InMemoryDatabaseCreateError::InjectedFault(fault));
        }
        Ok(())
    }
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

    /// Returns the durable modeled create phase for this stable owner.
    #[must_use]
    pub fn create_phase(&self) -> InMemoryDatabaseCreatePhase {
        self.creates
            .borrow()
            .get(&self.object_id)
            .map_or(InMemoryDatabaseCreatePhase::Absent, |record| record.phase)
    }

    /// Returns the manifest currently selected by the durable memory model.
    #[must_use]
    pub fn selected_manifest(&self) -> Option<DatabaseManifest> {
        self.create_record()
            .and_then(|record| record.selected_manifest)
    }

    /// Returns the separately published clean-close manifest candidate.
    #[must_use]
    pub fn close_candidate_manifest(&self) -> Option<DatabaseManifest> {
        self.create_record()
            .and_then(|record| record.close_candidate_manifest)
    }

    /// Returns the manifest covered by the modeled publication barrier.
    #[must_use]
    pub fn synchronized_manifest(&self) -> Option<DatabaseManifest> {
        self.create_record()
            .and_then(|record| record.synchronized_manifest)
    }

    /// Creates or resumes one exact deterministic successor-format composition.
    pub fn try_create_recovery_required(
        &self,
        manifest_object_id: InMemoryDatabaseObjectId,
        manifest: DatabaseManifest,
        files: &[InMemoryDatabaseFileObservation],
        fault: Option<InMemoryDatabaseCreateFault>,
    ) -> Result<InMemoryDatabaseCreateOutcome, InMemoryDatabaseCreateError> {
        let request = prepare_memory_create_request(self, manifest_object_id, manifest, files)?;
        let mut fault = InMemoryCreateFaultController::new(fault);
        if let Some(record) = self.foreign_database_create_record() {
            return Err(InMemoryDatabaseCreateError::EvidenceConflict {
                phase: record.phase,
            });
        }
        if let Some(record) = self.create_record()
            && record.phase == InMemoryDatabaseCreatePhase::Published
        {
            require_memory_create_request(record, request)?;
            let database = self
                .try_acquire_recovery_required(
                    self.database_id,
                    request.manifest_object_id,
                    request.manifest,
                    &request.files,
                )
                .map_err(InMemoryDatabaseCreateError::Ownership)?;
            return Ok(InMemoryDatabaseCreateOutcome::AlreadyPublished(database));
        }

        let mut guard = InMemoryDatabaseOwnershipGuard::new();
        guard
            .acquire(
                self.object_id,
                Rc::clone(&self.state),
                InMemoryDatabaseObjectBinding {
                    database_id: self.database_id,
                    role: InMemoryDatabaseObjectRole::DatabaseOwner,
                },
            )
            .map_err(InMemoryDatabaseCreateError::Ownership)?;

        let mut phase = match self.create_record() {
            Some(record) => {
                if record.phase == InMemoryDatabaseCreatePhase::Published {
                    require_memory_create_request(record, request)?;
                    drop(guard);
                    let database = self
                        .try_acquire_recovery_required(
                            self.database_id,
                            request.manifest_object_id,
                            request.manifest,
                            &request.files,
                        )
                        .map_err(InMemoryDatabaseCreateError::Ownership)?;
                    return Ok(InMemoryDatabaseCreateOutcome::AlreadyPublished(database));
                }
                record.phase
            }
            None => {
                fault.before(InMemoryDatabaseCreateBoundary::OwnerPublication)?;
                guard.commit_bindings();
                self.set_create_record(InMemoryDatabaseCreatePhase::Owner, None);
                fault.after(InMemoryDatabaseCreateBoundary::OwnerPublication)?;
                InMemoryDatabaseCreatePhase::Owner
            }
        };

        if phase == InMemoryDatabaseCreatePhase::Owner {
            fault.before(InMemoryDatabaseCreateBoundary::ManifestCandidatePublication)?;
            acquire_memory_create_object(
                self,
                &mut guard,
                request.manifest_object_id,
                InMemoryDatabaseObjectRole::Manifest,
            )?;
            guard.commit_bindings();
            phase = InMemoryDatabaseCreatePhase::ManifestCandidate;
            self.set_create_record(phase, Some(request));
            fault.after(InMemoryDatabaseCreateBoundary::ManifestCandidatePublication)?;
        } else {
            require_memory_create_request(
                self.create_record()
                    .ok_or(InMemoryDatabaseCreateError::StateCorrupt { phase })?,
                request,
            )?;
            acquire_memory_create_object(
                self,
                &mut guard,
                request.manifest_object_id,
                InMemoryDatabaseObjectRole::Manifest,
            )?;
        }

        if phase < InMemoryDatabaseCreatePhase::WalCandidate {
            fault.before(InMemoryDatabaseCreateBoundary::WalCandidatePublication)?;
            acquire_memory_create_object(
                self,
                &mut guard,
                request.files[0].object_id(),
                InMemoryDatabaseObjectRole::Wal,
            )?;
            guard.commit_bindings();
            phase = InMemoryDatabaseCreatePhase::WalCandidate;
            self.set_create_record(phase, Some(request));
            fault.after(InMemoryDatabaseCreateBoundary::WalCandidatePublication)?;
        } else {
            acquire_memory_create_object(
                self,
                &mut guard,
                request.files[0].object_id(),
                InMemoryDatabaseObjectRole::Wal,
            )?;
        }

        if phase < InMemoryDatabaseCreatePhase::PageStoreCandidate {
            fault.before(InMemoryDatabaseCreateBoundary::PageStoreCandidatePublication)?;
            acquire_memory_create_object(
                self,
                &mut guard,
                request.files[1].object_id(),
                InMemoryDatabaseObjectRole::PageStore,
            )?;
            guard.commit_bindings();
            phase = InMemoryDatabaseCreatePhase::PageStoreCandidate;
            self.set_create_record(phase, Some(request));
            fault.after(InMemoryDatabaseCreateBoundary::PageStoreCandidatePublication)?;
        } else {
            acquire_memory_create_object(
                self,
                &mut guard,
                request.files[1].object_id(),
                InMemoryDatabaseObjectRole::PageStore,
            )?;
        }

        if phase < InMemoryDatabaseCreatePhase::RestartCheckpointCandidate {
            fault.before(InMemoryDatabaseCreateBoundary::RestartCheckpointCandidatePublication)?;
            acquire_memory_create_object(
                self,
                &mut guard,
                request.files[2].object_id(),
                InMemoryDatabaseObjectRole::RestartCheckpoint,
            )?;
            guard.commit_bindings();
            phase = InMemoryDatabaseCreatePhase::RestartCheckpointCandidate;
            self.set_create_record(phase, Some(request));
            fault.after(InMemoryDatabaseCreateBoundary::RestartCheckpointCandidatePublication)?;
        } else {
            acquire_memory_create_object(
                self,
                &mut guard,
                request.files[2].object_id(),
                InMemoryDatabaseObjectRole::RestartCheckpoint,
            )?;
        }

        if phase < InMemoryDatabaseCreatePhase::WalPublished {
            fault.before(InMemoryDatabaseCreateBoundary::WalPublication)?;
            phase = InMemoryDatabaseCreatePhase::WalPublished;
            self.set_create_record(phase, Some(request));
            fault.after(InMemoryDatabaseCreateBoundary::WalPublication)?;
        }
        if phase < InMemoryDatabaseCreatePhase::PageStorePublished {
            fault.before(InMemoryDatabaseCreateBoundary::PageStorePublication)?;
            phase = InMemoryDatabaseCreatePhase::PageStorePublished;
            self.set_create_record(phase, Some(request));
            fault.after(InMemoryDatabaseCreateBoundary::PageStorePublication)?;
        }
        if phase < InMemoryDatabaseCreatePhase::ChildrenPublished {
            fault.before(InMemoryDatabaseCreateBoundary::RestartCheckpointPublication)?;
            phase = InMemoryDatabaseCreatePhase::ChildrenPublished;
            self.set_create_record(phase, Some(request));
            fault.after(InMemoryDatabaseCreateBoundary::RestartCheckpointPublication)?;
        }
        fault.before(InMemoryDatabaseCreateBoundary::ManifestPublication)?;
        phase = InMemoryDatabaseCreatePhase::Published;
        self.set_create_record(phase, Some(request));
        fault.after(InMemoryDatabaseCreateBoundary::ManifestPublication)?;

        let acquired = finish_in_memory_database_acquisition(
            self.database_id,
            self.object_id,
            request.manifest_object_id,
            request.manifest,
            request.files,
            guard,
            Rc::clone(&self.creates),
        )
        .map_err(InMemoryDatabaseCreateError::Ownership)?;
        let database = acquired
            .selected
            .bind_observed_storage(acquired.observed_storage_identity)
            .map_err(|failure| {
                InMemoryDatabaseCreateError::Ownership(
                    InMemoryDatabaseOwnershipError::StorageBinding(*failure.reason()),
                )
            })?;
        Ok(InMemoryDatabaseCreateOutcome::Created(database))
    }

    fn create_record(&self) -> Option<InMemoryDatabaseCreateRecord> {
        self.creates.borrow().get(&self.object_id).copied()
    }

    fn set_create_record(
        &self,
        phase: InMemoryDatabaseCreatePhase,
        request: Option<InMemoryDatabaseCreateRequest>,
    ) {
        let previous = self.create_record();
        let selected_manifest = previous
            .and_then(|record| record.selected_manifest)
            .or_else(|| {
                if phase == InMemoryDatabaseCreatePhase::Published {
                    request.map(|request| request.manifest)
                } else {
                    None
                }
            });
        self.creates.borrow_mut().insert(
            self.object_id,
            InMemoryDatabaseCreateRecord {
                database_id: self.database_id,
                phase,
                request,
                selected_manifest,
                close_candidate_manifest: previous
                    .and_then(|record| record.close_candidate_manifest),
                synchronized_manifest: previous.and_then(|record| record.synchronized_manifest),
            },
        );
    }

    fn foreign_database_create_record(&self) -> Option<InMemoryDatabaseCreateRecord> {
        self.creates
            .borrow()
            .iter()
            .find_map(|(owner_object_id, record)| {
                (*owner_object_id != self.object_id && record.database_id == self.database_id)
                    .then_some(*record)
            })
    }

    fn require_memory_create_publication(
        &self,
        manifest_object_id: InMemoryDatabaseObjectId,
        manifest: DatabaseManifest,
        files: &[InMemoryDatabaseFileObservation],
    ) -> Result<(), InMemoryDatabaseOwnershipError> {
        if let Some(record) = self.create_record() {
            require_published_memory_selection(record, manifest_object_id, manifest, files)?;
        }
        for (owner_object_id, record) in self.creates.borrow().iter() {
            if *owner_object_id == self.object_id || record.database_id != self.database_id {
                continue;
            }
            if record.phase != InMemoryDatabaseCreatePhase::Published {
                return Err(InMemoryDatabaseOwnershipError::UnpublishedCreate {
                    phase: record.phase,
                });
            }
            return Err(InMemoryDatabaseOwnershipError::PublishedCreateSelectionMismatch);
        }
        Ok(())
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
        let acquired = self.acquire(
            expected_database_id,
            manifest_object_id,
            manifest,
            files,
            false,
        )?;
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
        let acquired = self.acquire(
            expected_database_id,
            manifest_object_id,
            manifest,
            files,
            true,
        )?;
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
        require_recovery_required: bool,
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
        self.require_memory_create_publication(manifest_object_id, manifest, files)?;
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
        if require_recovery_required
            && !matches!(
                manifest.lifecycle_state(),
                ntsql_database::DatabaseManifestLifecycleState::RecoveryRequired
            )
        {
            return Err(InMemoryDatabaseOwnershipError::ManifestLifecycle {
                actual: manifest.lifecycle_state(),
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

        finish_in_memory_database_acquisition(
            expected_database_id,
            self.object_id,
            manifest_object_id,
            manifest,
            [wal, page_store, restart_checkpoint],
            guard,
            Rc::clone(&self.creates),
        )
    }
}

fn prepare_memory_create_request(
    slot: &InMemoryDatabaseOwnershipSlot,
    manifest_object_id: InMemoryDatabaseObjectId,
    manifest: DatabaseManifest,
    files: &[InMemoryDatabaseFileObservation],
) -> Result<InMemoryDatabaseCreateRequest, InMemoryDatabaseCreateError> {
    validate_memory_create_manifest(manifest)?;
    let manifest_database_id = manifest.composition_identity().database_id();
    if manifest_database_id != slot.database_id {
        return Err(InMemoryDatabaseCreateError::Ownership(
            InMemoryDatabaseOwnershipError::ManifestDatabaseIdMismatch {
                owner: slot.database_id,
                manifest: manifest_database_id,
            },
        ));
    }
    reject_object_alias(
        InMemoryDatabaseObjectRole::DatabaseOwner,
        slot.object_id,
        InMemoryDatabaseObjectRole::Manifest,
        manifest_object_id,
    )
    .map_err(InMemoryDatabaseCreateError::Ownership)?;
    let wal = require_exact_role(files, DatabaseFileRole::Wal)
        .map_err(InMemoryDatabaseCreateError::Ownership)?;
    let page_store = require_exact_role(files, DatabaseFileRole::PageStore)
        .map_err(InMemoryDatabaseCreateError::Ownership)?;
    let restart_checkpoint = require_exact_role(files, DatabaseFileRole::RestartCheckpoint)
        .map_err(InMemoryDatabaseCreateError::Ownership)?;
    for file in [wal, page_store, restart_checkpoint] {
        validate_file(manifest, file).map_err(InMemoryDatabaseCreateError::Ownership)?;
    }
    validate_object_against_prefix(
        wal,
        &[
            (InMemoryDatabaseObjectRole::DatabaseOwner, slot.object_id),
            (InMemoryDatabaseObjectRole::Manifest, manifest_object_id),
        ],
    )
    .map_err(InMemoryDatabaseCreateError::Ownership)?;
    validate_object_against_prefix(
        page_store,
        &[
            (InMemoryDatabaseObjectRole::DatabaseOwner, slot.object_id),
            (InMemoryDatabaseObjectRole::Manifest, manifest_object_id),
            (InMemoryDatabaseObjectRole::Wal, wal.object_id),
        ],
    )
    .map_err(InMemoryDatabaseCreateError::Ownership)?;
    validate_object_against_prefix(
        restart_checkpoint,
        &[
            (InMemoryDatabaseObjectRole::DatabaseOwner, slot.object_id),
            (InMemoryDatabaseObjectRole::Manifest, manifest_object_id),
            (InMemoryDatabaseObjectRole::Wal, wal.object_id),
            (InMemoryDatabaseObjectRole::PageStore, page_store.object_id),
        ],
    )
    .map_err(InMemoryDatabaseCreateError::Ownership)?;

    let observed_files = [
        DatabaseFileIdentity::new(wal.role, wal.file_id),
        DatabaseFileIdentity::new(page_store.role, page_store.file_id),
        DatabaseFileIdentity::new(restart_checkpoint.role, restart_checkpoint.file_id),
    ];
    let observed_storage_identity =
        DatabaseStorageIdentity::new(slot.database_id, wal.persistent_log_id, &observed_files)
            .map_err(|source| {
                InMemoryDatabaseCreateError::Ownership(
                    InMemoryDatabaseOwnershipError::ObservedStorageIdentity(source),
                )
            })?;
    manifest
        .composition_identity()
        .storage_identity()
        .require_exact_match(observed_storage_identity)
        .map_err(|source| {
            InMemoryDatabaseCreateError::Ownership(InMemoryDatabaseOwnershipError::StorageBinding(
                source,
            ))
        })?;

    Ok(InMemoryDatabaseCreateRequest {
        manifest_object_id,
        manifest,
        files: [wal, page_store, restart_checkpoint],
    })
}

fn validate_memory_create_manifest(
    manifest: DatabaseManifest,
) -> Result<(), InMemoryDatabaseCreateError> {
    let generation = manifest.composition_identity().lifecycle_generation().get();
    if generation != 1 {
        return Err(InMemoryDatabaseCreateError::ManifestRequirement(
            InMemoryDatabaseCreateManifestError::LifecycleGeneration { actual: generation },
        ));
    }
    for (role, expected) in [
        (DatabaseFileRole::Wal, 5_u16),
        (DatabaseFileRole::PageStore, 2),
        (DatabaseFileRole::RestartCheckpoint, 2),
    ] {
        let actual = manifest.storage_formats().version(role).get();
        if actual != expected {
            return Err(InMemoryDatabaseCreateError::ManifestRequirement(
                InMemoryDatabaseCreateManifestError::StorageFormatVersion {
                    role,
                    expected,
                    actual,
                },
            ));
        }
    }
    let required_features = manifest.required_features().bits();
    if required_features != 0 {
        return Err(InMemoryDatabaseCreateError::ManifestRequirement(
            InMemoryDatabaseCreateManifestError::RequiredFeatures {
                actual: required_features,
            },
        ));
    }
    Ok(())
}

fn require_memory_create_request(
    record: InMemoryDatabaseCreateRecord,
    requested: InMemoryDatabaseCreateRequest,
) -> Result<(), InMemoryDatabaseCreateError> {
    let Some(stored) = record.request else {
        return Err(InMemoryDatabaseCreateError::StateCorrupt {
            phase: record.phase,
        });
    };
    if stored != requested {
        return Err(InMemoryDatabaseCreateError::EvidenceConflict {
            phase: record.phase,
        });
    }
    Ok(())
}

fn require_published_memory_selection(
    record: InMemoryDatabaseCreateRecord,
    manifest_object_id: InMemoryDatabaseObjectId,
    manifest: DatabaseManifest,
    files: &[InMemoryDatabaseFileObservation],
) -> Result<(), InMemoryDatabaseOwnershipError> {
    if record.phase != InMemoryDatabaseCreatePhase::Published {
        return Err(InMemoryDatabaseOwnershipError::UnpublishedCreate {
            phase: record.phase,
        });
    }
    let stored = record
        .request
        .ok_or(InMemoryDatabaseOwnershipError::CreateStateCorrupt {
            phase: record.phase,
        })?;
    let observed = [
        require_exact_role(files, DatabaseFileRole::Wal)?,
        require_exact_role(files, DatabaseFileRole::PageStore)?,
        require_exact_role(files, DatabaseFileRole::RestartCheckpoint)?,
    ];
    if manifest_object_id != stored.manifest_object_id || observed != stored.files {
        return Err(InMemoryDatabaseOwnershipError::PublishedCreateSelectionMismatch);
    }
    let selected_manifest =
        record
            .selected_manifest
            .ok_or(InMemoryDatabaseOwnershipError::CreateStateCorrupt {
                phase: record.phase,
            })?;
    if manifest != selected_manifest {
        return Err(InMemoryDatabaseOwnershipError::SelectedManifestMismatch);
    }
    Ok(())
}

fn acquire_memory_create_object(
    slot: &InMemoryDatabaseOwnershipSlot,
    guard: &mut InMemoryDatabaseOwnershipGuard,
    object_id: InMemoryDatabaseObjectId,
    role: InMemoryDatabaseObjectRole,
) -> Result<(), InMemoryDatabaseCreateError> {
    guard
        .acquire(
            object_id,
            resolve_world_state(&slot.world, object_id),
            InMemoryDatabaseObjectBinding {
                database_id: slot.database_id,
                role,
            },
        )
        .map_err(InMemoryDatabaseCreateError::Ownership)
}

fn finish_in_memory_database_acquisition(
    expected_database_id: DatabaseId,
    owner_object_id: InMemoryDatabaseObjectId,
    manifest_object_id: InMemoryDatabaseObjectId,
    manifest: DatabaseManifest,
    files: [InMemoryDatabaseFileObservation; 3],
    guard: InMemoryDatabaseOwnershipGuard,
    creates: Rc<RefCell<BTreeMap<InMemoryDatabaseObjectId, InMemoryDatabaseCreateRecord>>>,
) -> Result<AcquiredInMemoryDatabaseOwnership, InMemoryDatabaseOwnershipError> {
    let observed_files = [
        DatabaseFileIdentity::new(files[0].role, files[0].file_id),
        DatabaseFileIdentity::new(files[1].role, files[1].file_id),
        DatabaseFileIdentity::new(files[2].role, files[2].file_id),
    ];
    let selected_identity = manifest.composition_identity();
    let observed_storage_identity = DatabaseStorageIdentity::new(
        expected_database_id,
        files[0].persistent_log_id,
        &observed_files,
    )
    .map_err(InMemoryDatabaseOwnershipError::ObservedStorageIdentity)?;
    selected_identity
        .storage_identity()
        .require_exact_match(observed_storage_identity)
        .map_err(InMemoryDatabaseOwnershipError::StorageBinding)?;
    guard.commit_bindings();
    let owner = InMemoryDatabaseOwnership {
        manifest,
        owner_object_id,
        manifest_object_id,
        files,
        creates,
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
    owner_object_id: InMemoryDatabaseObjectId,
    manifest_object_id: InMemoryDatabaseObjectId,
    files: [InMemoryDatabaseFileObservation; 3],
    creates: Rc<RefCell<BTreeMap<InMemoryDatabaseObjectId, InMemoryDatabaseCreateRecord>>>,
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

    fn create_record(&self) -> Option<InMemoryDatabaseCreateRecord> {
        self.creates.borrow().get(&self.owner_object_id).copied()
    }

    fn set_close_record(&mut self, record: InMemoryDatabaseCreateRecord) {
        self.creates
            .borrow_mut()
            .insert(self.owner_object_id, record);
    }
}

impl fmt::Debug for InMemoryDatabaseOwnership {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryDatabaseOwnership")
            .field("manifest", &self.manifest)
            .field("owner_object_id", &self.owner_object_id)
            .field("manifest_object_id", &self.manifest_object_id)
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

/// Concrete in-memory transaction storage supplied to database recovery.
#[must_use = "unrecovered memory storage must enter database recovery or be dropped"]
pub struct InMemoryDatabaseRecoveryStorage<const N: usize> {
    log: InMemoryCommitLog<N>,
    store: InMemoryPageStore<N>,
    checkpoint: InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
}

impl<const N: usize> InMemoryDatabaseRecoveryStorage<N> {
    /// Binds one concrete WAL, page store, and completeness source for open.
    pub const fn new(
        log: InMemoryCommitLog<N>,
        store: InMemoryPageStore<N>,
        checkpoint: InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    ) -> Self {
        Self {
            log,
            store,
            checkpoint,
        }
    }
}

impl<const N: usize> fmt::Debug for InMemoryDatabaseRecoveryStorage<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryDatabaseRecoveryStorage")
            .field("log", &self.log)
            .field("store", &self.store)
            .field("checkpoint", &self.checkpoint)
            .finish()
    }
}

/// One ordered memory clean-manifest publication boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryDatabaseCloseBoundary {
    /// Publish the candidate without changing selected state.
    CandidatePublication,
    /// Atomically replace the selected manifest.
    ManifestPublication,
    /// Reobserve the selected manifest after replacement.
    SelectedManifestVerification,
    /// Record the modeled durability barrier.
    PublicationSynchronization,
}

/// Deterministic timing for one memory clean-manifest publication fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryDatabaseCloseFaultTiming {
    /// Report a definite failure before the boundary effect.
    BeforeEffect,
    /// Apply the effect and then report a definite failure.
    AfterEffect,
    /// Report an outcome-indeterminate failure without applying the effect.
    OutcomeIndeterminateBeforeEffect,
    /// Apply the effect and then report an outcome-indeterminate failure.
    OutcomeIndeterminateAfterEffect,
}

impl InMemoryDatabaseCloseFaultTiming {
    const fn is_before(self) -> bool {
        matches!(
            self,
            Self::BeforeEffect | Self::OutcomeIndeterminateBeforeEffect
        )
    }

    const fn is_after(self) -> bool {
        matches!(
            self,
            Self::AfterEffect | Self::OutcomeIndeterminateAfterEffect
        )
    }

    /// Returns whether the injected report hides the physical outcome.
    #[must_use]
    pub const fn is_outcome_indeterminate(self) -> bool {
        matches!(
            self,
            Self::OutcomeIndeterminateBeforeEffect | Self::OutcomeIndeterminateAfterEffect
        )
    }
}

/// One deterministic memory clean-manifest publication fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InMemoryDatabaseCloseFault {
    boundary: InMemoryDatabaseCloseBoundary,
    timing: InMemoryDatabaseCloseFaultTiming,
}

impl InMemoryDatabaseCloseFault {
    /// Arms one exact publication boundary and timing.
    #[must_use]
    pub const fn new(
        boundary: InMemoryDatabaseCloseBoundary,
        timing: InMemoryDatabaseCloseFaultTiming,
    ) -> Self {
        Self { boundary, timing }
    }

    /// Returns the armed boundary.
    #[must_use]
    pub const fn boundary(self) -> InMemoryDatabaseCloseBoundary {
        self.boundary
    }

    /// Returns the armed timing.
    #[must_use]
    pub const fn timing(self) -> InMemoryDatabaseCloseFaultTiming {
        self.timing
    }

    const fn publication_state(self) -> DatabaseCleanManifestPublicationState {
        match (self.boundary, self.timing) {
            (InMemoryDatabaseCloseBoundary::CandidatePublication, _) => {
                DatabaseCleanManifestPublicationState::SourceSelected
            }
            (
                InMemoryDatabaseCloseBoundary::ManifestPublication,
                InMemoryDatabaseCloseFaultTiming::BeforeEffect,
            ) => DatabaseCleanManifestPublicationState::SourceSelected,
            (
                InMemoryDatabaseCloseBoundary::ManifestPublication,
                InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateBeforeEffect
                | InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect,
            ) => DatabaseCleanManifestPublicationState::SelectionIndeterminate,
            (
                InMemoryDatabaseCloseBoundary::ManifestPublication,
                InMemoryDatabaseCloseFaultTiming::AfterEffect,
            )
            | (InMemoryDatabaseCloseBoundary::SelectedManifestVerification, _)
            | (
                InMemoryDatabaseCloseBoundary::PublicationSynchronization,
                InMemoryDatabaseCloseFaultTiming::BeforeEffect
                | InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateBeforeEffect
                | InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect,
            ) => DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
            (
                InMemoryDatabaseCloseBoundary::PublicationSynchronization,
                InMemoryDatabaseCloseFaultTiming::AfterEffect,
            ) => DatabaseCleanManifestPublicationState::TargetDurable,
        }
    }
}

impl fmt::Display for InMemoryDatabaseCloseFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?} at {:?}", self.boundary, self.timing)
    }
}

/// Memory-adapter cause for a clean-manifest publication failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryDatabaseClosePublicationError {
    /// The selected memory model was not created through the durable create path.
    MissingCreateRecord,
    /// The durable create protocol has not selected a manifest.
    CreateNotPublished {
        /// Exact create phase retained by the model.
        phase: InMemoryDatabaseCreatePhase,
    },
    /// The modeled selected source changed before candidate publication.
    SourceManifestMismatch,
    /// The permit target is not an exact lifecycle successor.
    TargetManifest(DatabaseManifestSuccessorError),
    /// Fresh selected-manifest verification contradicted the target.
    SelectedManifestMismatch,
    /// The modeled durability barrier did not cover the exact target.
    SynchronizedManifestMismatch,
    /// One deterministic fault fired.
    InjectedFault(InMemoryDatabaseCloseFault),
}

impl fmt::Display for InMemoryDatabaseClosePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCreateRecord => formatter
                .write_str("memory clean-manifest publication has no durable create record"),
            Self::CreateNotPublished { phase } => write!(
                formatter,
                "memory clean-manifest publication found create phase {phase}, not published"
            ),
            Self::SourceManifestMismatch => formatter
                .write_str("memory clean-manifest source differs from modeled selected state"),
            Self::TargetManifest(source) => {
                write!(
                    formatter,
                    "memory clean-manifest target is invalid: {source}"
                )
            }
            Self::SelectedManifestMismatch => {
                formatter.write_str("memory selected-manifest verification contradicted the target")
            }
            Self::SynchronizedManifestMismatch => {
                formatter.write_str("memory publication barrier did not cover the target")
            }
            Self::InjectedFault(fault) => {
                write!(formatter, "injected memory database close fault {fault}")
            }
        }
    }
}

impl Error for InMemoryDatabaseClosePublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TargetManifest(source) => Some(source),
            Self::MissingCreateRecord
            | Self::CreateNotPublished { .. }
            | Self::SourceManifestMismatch
            | Self::SelectedManifestMismatch
            | Self::SynchronizedManifestMismatch
            | Self::InjectedFault(_) => None,
        }
    }
}

struct InMemoryDatabaseCloseFaultController {
    armed: Option<InMemoryDatabaseCloseFault>,
}

impl InMemoryDatabaseCloseFaultController {
    const fn new(armed: Option<InMemoryDatabaseCloseFault>) -> Self {
        Self { armed }
    }

    fn before(
        &mut self,
        boundary: InMemoryDatabaseCloseBoundary,
    ) -> Result<(), DatabaseCleanManifestPublisherFailure<InMemoryDatabaseClosePublicationError>>
    {
        self.fire(boundary, InMemoryDatabaseCloseFaultTiming::is_before)
    }

    fn after(
        &mut self,
        boundary: InMemoryDatabaseCloseBoundary,
    ) -> Result<(), DatabaseCleanManifestPublisherFailure<InMemoryDatabaseClosePublicationError>>
    {
        self.fire(boundary, InMemoryDatabaseCloseFaultTiming::is_after)
    }

    fn fire(
        &mut self,
        boundary: InMemoryDatabaseCloseBoundary,
        matches_timing: fn(InMemoryDatabaseCloseFaultTiming) -> bool,
    ) -> Result<(), DatabaseCleanManifestPublisherFailure<InMemoryDatabaseClosePublicationError>>
    {
        if let Some(fault) = self.armed
            && fault.boundary() == boundary
            && matches_timing(fault.timing())
        {
            self.armed = None;
            return Err(DatabaseCleanManifestPublisherFailure::new(
                fault.publication_state(),
                InMemoryDatabaseClosePublicationError::InjectedFault(fault),
            ));
        }
        Ok(())
    }
}

/// Modeled database-wide ownership retained after transaction recovery.
#[must_use = "live memory database ownership must remain inside its database typestate"]
pub struct RecoveredInMemoryDatabaseOuterOwnership {
    ownership: InMemoryDatabaseOwnership,
    compatibility_context: CompatibilityContext,
}

impl RecoveredInMemoryDatabaseOuterOwnership {
    /// Returns the retained inert manifest.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.ownership.manifest()
    }

    /// Returns the one immutable exact-target context moved through open.
    #[must_use]
    pub const fn compatibility_context(&self) -> &CompatibilityContext {
        &self.compatibility_context
    }
}

impl DatabaseCloseSourceManifestOwner for RecoveredInMemoryDatabaseOuterOwnership {
    fn close_source_manifest(&self) -> DatabaseManifest {
        self.manifest()
    }
}

impl DatabaseCleanManifestPublisher for RecoveredInMemoryDatabaseOuterOwnership {
    type Input = Option<InMemoryDatabaseCloseFault>;
    type Error = InMemoryDatabaseClosePublicationError;

    fn publish_clean_manifest(
        &mut self,
        input: Self::Input,
        permit: DatabaseCleanManifestPublicationPermit<'_>,
    ) -> Result<
        DatabaseCleanManifestPublicationReceipt,
        DatabaseCleanManifestPublisherFailure<Self::Error>,
    > {
        let source_manifest = self.ownership.manifest;
        let target_manifest = permit.target_manifest();
        target_manifest
            .require_successor_of(source_manifest)
            .map_err(|source| {
                DatabaseCleanManifestPublisherFailure::new(
                    DatabaseCleanManifestPublicationState::SourceSelected,
                    InMemoryDatabaseClosePublicationError::TargetManifest(source),
                )
            })?;

        let mut record = self.ownership.create_record().ok_or_else(|| {
            DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SourceSelected,
                InMemoryDatabaseClosePublicationError::MissingCreateRecord,
            )
        })?;
        if record.phase != InMemoryDatabaseCreatePhase::Published {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SourceSelected,
                InMemoryDatabaseClosePublicationError::CreateNotPublished {
                    phase: record.phase,
                },
            ));
        }
        if record.selected_manifest != Some(source_manifest) {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                InMemoryDatabaseClosePublicationError::SourceManifestMismatch,
            ));
        }

        let mut fault = InMemoryDatabaseCloseFaultController::new(input);
        fault.before(InMemoryDatabaseCloseBoundary::CandidatePublication)?;
        record.close_candidate_manifest = Some(target_manifest);
        record.synchronized_manifest = None;
        self.ownership.set_close_record(record);
        fault.after(InMemoryDatabaseCloseBoundary::CandidatePublication)?;

        fault.before(InMemoryDatabaseCloseBoundary::ManifestPublication)?;
        record.selected_manifest = Some(target_manifest);
        self.ownership.manifest = target_manifest;
        self.ownership.set_close_record(record);
        fault.after(InMemoryDatabaseCloseBoundary::ManifestPublication)?;

        fault.before(InMemoryDatabaseCloseBoundary::SelectedManifestVerification)?;
        record = self.ownership.create_record().ok_or_else(|| {
            DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                InMemoryDatabaseClosePublicationError::MissingCreateRecord,
            )
        })?;
        if record.selected_manifest != Some(target_manifest) {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                InMemoryDatabaseClosePublicationError::SelectedManifestMismatch,
            ));
        }
        fault.after(InMemoryDatabaseCloseBoundary::SelectedManifestVerification)?;

        fault.before(InMemoryDatabaseCloseBoundary::PublicationSynchronization)?;
        record.synchronized_manifest = Some(target_manifest);
        self.ownership.set_close_record(record);
        fault.after(InMemoryDatabaseCloseBoundary::PublicationSynchronization)?;

        let synchronized = self.ownership.create_record().ok_or_else(|| {
            DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                InMemoryDatabaseClosePublicationError::MissingCreateRecord,
            )
        })?;
        let selected_manifest = synchronized.selected_manifest.ok_or_else(|| {
            DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                InMemoryDatabaseClosePublicationError::SelectedManifestMismatch,
            )
        })?;
        if selected_manifest != target_manifest {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                InMemoryDatabaseClosePublicationError::SelectedManifestMismatch,
            ));
        }
        let synchronized_manifest = synchronized.synchronized_manifest.ok_or_else(|| {
            DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                InMemoryDatabaseClosePublicationError::SynchronizedManifestMismatch,
            )
        })?;
        if synchronized_manifest != target_manifest {
            return Err(DatabaseCleanManifestPublisherFailure::new(
                DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                InMemoryDatabaseClosePublicationError::SynchronizedManifestMismatch,
            ));
        }

        Ok(permit.complete(selected_manifest, synchronized_manifest))
    }
}

impl fmt::Debug for RecoveredInMemoryDatabaseOuterOwnership {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveredInMemoryDatabaseOuterOwnership")
            .field("ownership", &self.ownership)
            .field(
                "compatibility_target",
                self.compatibility_context.target_id(),
            )
            .finish_non_exhaustive()
    }
}

/// Terminal memory recovery attempt retaining every modeled and concrete owner.
#[must_use = "failed memory recovery retains all database and child ownership"]
pub struct FailedInMemoryDatabaseRecoveryAttempt<const N: usize> {
    _outer_owner: RecoveredInMemoryDatabaseOuterOwnership,
    failure: FailedTransactionPageStorageRecoveryHandoff<
        InMemoryCommitLog<N>,
        InMemoryPageStore<N>,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
        N,
    >,
}

impl<const N: usize> FailedInMemoryDatabaseRecoveryAttempt<N> {
    /// Returns the first transaction recovery phase that did not complete.
    #[must_use]
    pub const fn phase(&self) -> TransactionPageStorageRecoveryHandoffPhase {
        self.failure.phase()
    }
}

impl<const N: usize> fmt::Debug for FailedInMemoryDatabaseRecoveryAttempt<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FailedInMemoryDatabaseRecoveryAttempt")
            .field("phase", &self.phase())
            .finish_non_exhaustive()
    }
}

/// Memory database open boundary crossed after one complete owning phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryDatabaseOpenPhase {
    /// Five-object modeled ownership and exact child identity were validated.
    CompositionValidated,
    /// One transaction recovery handoff phase completed.
    Recovery(TransactionPageStorageRecoveryHandoffPhase),
    /// The database domain accepted exact completion evidence and released Live.
    LiveReleased,
}

/// Observer input accepted only by the memory recovery-owner implementation.
pub struct InMemoryDatabaseRecoveryInput<'observer, Observer, const N: usize> {
    compatibility_context: CompatibilityContext,
    storage: InMemoryDatabaseRecoveryStorage<N>,
    observer: &'observer mut Observer,
}

impl<const N: usize, Observer>
    DatabaseRecoveryOwner<InMemoryDatabaseRecoveryInput<'_, Observer, N>, N>
    for InMemoryDatabaseOwnership
where
    Observer: FnMut(InMemoryDatabaseOpenPhase),
{
    type Source = InMemoryCommitLog<N>;
    type Store = InMemoryPageStore<N>;
    type CheckpointSource = InMemoryTransactionRestartCheckpointCompletenessBaselineSource;
    type RetainedOwner = RecoveredInMemoryDatabaseOuterOwnership;
    type Failure = FailedInMemoryDatabaseRecoveryAttempt<N>;

    fn complete_database_recovery(
        self,
        input: InMemoryDatabaseRecoveryInput<'_, Observer, N>,
    ) -> Result<
        (
            Self::RetainedOwner,
            WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
                Self::Source,
                Self::Store,
                Self::CheckpointSource,
                N,
            >,
        ),
        Self::Failure,
    > {
        let InMemoryDatabaseRecoveryInput {
            compatibility_context,
            storage,
            observer,
        } = input;
        let outer_owner = RecoveredInMemoryDatabaseOuterOwnership {
            ownership: self,
            compatibility_context,
        };
        let InMemoryDatabaseRecoveryStorage {
            log,
            store,
            checkpoint,
        } = storage;
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_generation_aware_restart_checkpoint_completeness(checkpoint);
        match complete_transaction_page_storage_recovery_handoff_with_observer(selection, |phase| {
            observer(InMemoryDatabaseOpenPhase::Recovery(phase))
        }) {
            Ok(transaction) => Ok((outer_owner, transaction)),
            Err(failure) => Err(FailedInMemoryDatabaseRecoveryAttempt {
                _outer_owner: outer_owner,
                failure,
            }),
        }
    }
}

type LiveInMemoryDatabaseDomainOwner<const N: usize> = RecoveredDatabaseOwnership<
    RecoveredInMemoryDatabaseOuterOwnership,
    InMemoryCommitLog<N>,
    InMemoryPageStore<N>,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    N,
>;

type FailedInMemoryDatabaseDomainRecovery<const N: usize> = FailedDatabaseRecovery<
    FailedInMemoryDatabaseRecoveryAttempt<N>,
    RecoveredInMemoryDatabaseOuterOwnership,
    InMemoryCommitLog<N>,
    InMemoryPageStore<N>,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    N,
>;

/// Recovery-complete memory database owner with one exact target context.
#[must_use = "live memory database must be closed, abandoned, or dropped"]
pub struct LiveInMemoryDatabase<const N: usize> {
    database: LiveDatabase<LiveInMemoryDatabaseDomainOwner<N>>,
}

impl<const N: usize> LiveInMemoryDatabase<N> {
    /// Returns the exact selected database composition identity.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.database.identity()
    }

    /// Returns the database lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        self.database.stage()
    }

    /// Returns the one immutable exact-target context moved through open.
    #[must_use]
    pub const fn compatibility_context(&self) -> &CompatibilityContext {
        self.database.owner().outer_owner().compatibility_context()
    }

    /// Returns the exact manifest retained by modeled ownership.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.database.owner().outer_owner().manifest()
    }

    /// Borrows the recovered coordinator, WAL, and page store.
    #[must_use]
    pub const fn transaction_parts(
        &self,
    ) -> (
        &TransactionCoordinator,
        &InMemoryCommitLog<N>,
        &InMemoryPageStore<N>,
    ) {
        self.database.owner().transaction().parts()
    }

    /// Borrows the recovered coordinator, WAL, and page store for live work.
    pub const fn transaction_parts_mut(
        &mut self,
    ) -> (
        &mut TransactionCoordinator,
        &mut InMemoryCommitLog<N>,
        &mut InMemoryPageStore<N>,
    ) {
        self.database.owner_mut().transaction_mut().parts_mut()
    }

    /// Borrows the exact completion and WAL-retention handoff owner.
    pub const fn recovery_handoff(
        &self,
    ) -> &WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
        InMemoryCommitLog<N>,
        InMemoryPageStore<N>,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
        N,
    > {
        self.database.owner().transaction()
    }

    /// Consumes Live and binds fresh transaction close evidence to this database.
    pub fn prepare_close(
        self,
    ) -> Result<ClosePendingInMemoryDatabase<N>, FailedInMemoryDatabaseClosePreparation<N>> {
        self.database
            .prepare_close()
            .map(|database| ClosePendingInMemoryDatabase { database })
    }

    /// Relinquishes live ownership without publishing any clean state.
    pub fn abandon(self) -> AbandonedDatabase {
        self.database.abandon()
    }
}

impl<const N: usize> fmt::Debug for LiveInMemoryDatabase<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveInMemoryDatabase")
            .field("identity", &self.identity())
            .field(
                "compatibility_target",
                self.compatibility_context().target_id(),
            )
            .finish_non_exhaustive()
    }
}

type PreparedInMemoryDatabaseCloseOwnership<const N: usize> = PreparedDatabaseCloseOwnership<
    RecoveredInMemoryDatabaseOuterOwnership,
    InMemoryCommitLog<N>,
    InMemoryPageStore<N>,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    N,
>;

type PublishedInMemoryDatabaseCloseOwnership<const N: usize> = PublishedDatabaseCloseOwnership<
    RecoveredInMemoryDatabaseOuterOwnership,
    InMemoryCommitLog<N>,
    InMemoryPageStore<N>,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    N,
>;

/// Terminal memory-database owner retained when close preparation fails.
pub type FailedInMemoryDatabaseClosePreparation<const N: usize> = FailedDatabaseClosePreparation<
    RecoveredInMemoryDatabaseOuterOwnership,
    InMemoryCommitLog<N>,
    InMemoryPageStore<N>,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    N,
>;

/// Terminal memory-database owner retained when manifest publication fails.
pub type FailedInMemoryDatabaseClosePublication<const N: usize> = FailedDatabaseClosePublication<
    RecoveredInMemoryDatabaseOuterOwnership,
    InMemoryCommitLog<N>,
    InMemoryPageStore<N>,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    InMemoryDatabaseClosePublicationError,
    N,
>;

/// Memory database whose exact clean certificate awaits manifest publication.
#[must_use = "close-pending memory database must publish or be explicitly abandoned"]
pub struct ClosePendingInMemoryDatabase<const N: usize> {
    database: ClosePendingDatabase<PreparedInMemoryDatabaseCloseOwnership<N>>,
}

impl<const N: usize> ClosePendingInMemoryDatabase<N> {
    /// Returns the recovery-required source composition.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.database.identity()
    }

    /// Returns the exact adjacent composition targeted by clean publication.
    #[must_use]
    pub const fn target_identity(&self) -> DatabaseCompositionIdentity {
        self.database.prepared().target_identity()
    }

    /// Returns the exact adjacent clean manifest awaiting publication.
    #[must_use]
    pub const fn target_manifest(&self) -> DatabaseManifest {
        self.database.prepared().target_manifest()
    }

    /// Returns the exact clean-close certificate derived from transaction proof.
    #[must_use]
    pub const fn certificate(&self) -> DatabaseCleanCloseCertificate {
        self.database.prepared().certificate()
    }

    /// Returns the selected recovery-required manifest retained by ownership.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.database.prepared().outer_owner().manifest()
    }

    /// Returns the immutable exact-target compatibility context.
    #[must_use]
    pub const fn compatibility_context(&self) -> &CompatibilityContext {
        self.database
            .prepared()
            .outer_owner()
            .compatibility_context()
    }

    /// Returns the database lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        self.database.stage()
    }

    /// Publishes and synchronizes the exact clean manifest.
    pub fn close(
        self,
    ) -> Result<ClosedInMemoryDatabase<N>, FailedInMemoryDatabaseClosePublication<N>> {
        self.database
            .close(None)
            .map(|database| ClosedInMemoryDatabase { database })
    }

    /// Publishes with one deterministic memory fault.
    pub fn close_with_fault(
        self,
        fault: InMemoryDatabaseCloseFault,
    ) -> Result<ClosedInMemoryDatabase<N>, FailedInMemoryDatabaseClosePublication<N>> {
        self.database
            .close(Some(fault))
            .map(|database| ClosedInMemoryDatabase { database })
    }

    /// Relinquishes close-pending ownership without publishing clean state.
    pub fn abandon(self) -> AbandonedDatabase {
        self.database.abandon()
    }
}

impl<const N: usize> fmt::Debug for ClosePendingInMemoryDatabase<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClosePendingInMemoryDatabase")
            .field("identity", &self.identity())
            .field("target_identity", &self.target_identity())
            .field("certificate", &self.certificate())
            .finish_non_exhaustive()
    }
}

/// Memory database retained after exact clean-manifest durability.
#[must_use = "closed memory database ownership must remain retained or be dropped"]
pub struct ClosedInMemoryDatabase<const N: usize> {
    database: ClosedDatabase<PublishedInMemoryDatabaseCloseOwnership<N>>,
}

impl<const N: usize> ClosedInMemoryDatabase<N> {
    /// Returns the exact selected clean composition.
    #[must_use]
    pub const fn identity(&self) -> DatabaseCompositionIdentity {
        self.database.identity()
    }

    /// Returns the exact selected and synchronized clean manifest.
    #[must_use]
    pub const fn manifest(&self) -> DatabaseManifest {
        self.database.published().manifest()
    }

    /// Returns the database lifecycle stage.
    #[must_use]
    pub const fn stage(&self) -> DatabaseLifecycleStage {
        self.database.stage()
    }

    /// Returns the retained immutable exact-target context.
    #[must_use]
    pub const fn compatibility_context(&self) -> &CompatibilityContext {
        self.database
            .published()
            .prepared()
            .outer_owner()
            .compatibility_context()
    }
}

impl<const N: usize> fmt::Debug for ClosedInMemoryDatabase<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClosedInMemoryDatabase")
            .field("identity", &self.identity())
            .field("manifest", &self.manifest())
            .finish_non_exhaustive()
    }
}

/// Terminal inert result after relinquishing a failed memory close publication.
pub type AbandonedInMemoryDatabaseClosePublication = AbandonedDatabaseClosePublication;

/// Failure before or during fail-closed memory database open.
#[must_use = "failed memory database open may retain every database owner"]
pub enum InMemoryDatabaseLiveOpenError<const N: usize> {
    /// Modeled database ownership or structural composition validation failed.
    Ownership(InMemoryDatabaseOwnershipError),
    /// Transaction recovery or exact completion-evidence binding failed.
    Recovery(FailedInMemoryDatabaseDomainRecovery<N>),
}

impl<const N: usize> InMemoryDatabaseLiveOpenError<N> {
    /// Returns the transaction recovery phase for an adapter-operation failure.
    #[must_use]
    pub const fn recovery_phase(&self) -> Option<TransactionPageStorageRecoveryHandoffPhase> {
        match self {
            Self::Ownership(_) => None,
            Self::Recovery(failure) => match failure.cause() {
                DatabaseRecoveryFailureCause::Operation(failure) => Some(failure.phase()),
                DatabaseRecoveryFailureCause::Evidence(_) => None,
            },
        }
    }
}

impl<const N: usize> fmt::Debug for InMemoryDatabaseLiveOpenError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ownership(error) => formatter
                .debug_tuple("InMemoryDatabaseLiveOpenError::Ownership")
                .field(error)
                .finish(),
            Self::Recovery(error) => formatter
                .debug_tuple("InMemoryDatabaseLiveOpenError::Recovery")
                .field(error)
                .finish(),
        }
    }
}

impl<const N: usize> fmt::Display for InMemoryDatabaseLiveOpenError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ownership(error) => write!(formatter, "memory database open failed: {error}"),
            Self::Recovery(error) => match error.cause() {
                DatabaseRecoveryFailureCause::Operation(failure) => write!(
                    formatter,
                    "memory database recovery failed before completing {:?}",
                    failure.phase()
                ),
                DatabaseRecoveryFailureCause::Evidence(error) => {
                    write!(
                        formatter,
                        "memory database recovery evidence failed: {error}"
                    )
                }
            },
        }
    }
}

impl<const N: usize> Error for InMemoryDatabaseLiveOpenError<N> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ownership(error) => Some(error),
            Self::Recovery(_) => None,
        }
    }
}

/// Complete composition-root input for one fail-closed memory database open.
#[must_use = "a live-open request must be consumed by the recovery gate"]
pub struct InMemoryDatabaseLiveOpenRequest<'owner, const N: usize> {
    slot: &'owner InMemoryDatabaseOwnershipSlot,
    expected_database_id: DatabaseId,
    manifest_object_id: InMemoryDatabaseObjectId,
    manifest: DatabaseManifest,
    files: &'owner [InMemoryDatabaseFileObservation],
    storage: InMemoryDatabaseRecoveryStorage<N>,
    compatibility_context: CompatibilityContext,
}

impl<'owner, const N: usize> InMemoryDatabaseLiveOpenRequest<'owner, N> {
    /// Binds one exact compatibility decision to the modeled and concrete owners.
    pub const fn new(
        slot: &'owner InMemoryDatabaseOwnershipSlot,
        expected_database_id: DatabaseId,
        manifest_object_id: InMemoryDatabaseObjectId,
        manifest: DatabaseManifest,
        files: &'owner [InMemoryDatabaseFileObservation],
        storage: InMemoryDatabaseRecoveryStorage<N>,
        compatibility_context: CompatibilityContext,
    ) -> Self {
        Self {
            slot,
            expected_database_id,
            manifest_object_id,
            manifest,
            files,
            storage,
            compatibility_context,
        }
    }
}

impl<const N: usize> fmt::Debug for InMemoryDatabaseLiveOpenRequest<'_, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryDatabaseLiveOpenRequest")
            .field("expected_database_id", &self.expected_database_id)
            .field("manifest_object_id", &self.manifest_object_id)
            .field("manifest", &self.manifest)
            .field("files", &self.files)
            .field("storage", &self.storage)
            .field("compatibility_context", &self.compatibility_context)
            .finish_non_exhaustive()
    }
}

/// Acquires one modeled composition and recovers concrete memory storage to Live.
pub fn open_live_in_memory_database<const N: usize>(
    request: InMemoryDatabaseLiveOpenRequest<'_, N>,
) -> Result<LiveInMemoryDatabase<N>, InMemoryDatabaseLiveOpenError<N>> {
    open_live_in_memory_database_with_observer(request, |_| {})
}

/// Opens memory storage through recovery while reporting completed owning phases.
pub fn open_live_in_memory_database_with_observer<const N: usize, Observer>(
    request: InMemoryDatabaseLiveOpenRequest<'_, N>,
    mut observer: Observer,
) -> Result<LiveInMemoryDatabase<N>, InMemoryDatabaseLiveOpenError<N>>
where
    Observer: FnMut(InMemoryDatabaseOpenPhase),
{
    let InMemoryDatabaseLiveOpenRequest {
        slot,
        expected_database_id,
        manifest_object_id,
        manifest,
        files,
        storage,
        compatibility_context,
    } = request;
    let recovery_required = slot
        .try_acquire_recovery_required(expected_database_id, manifest_object_id, manifest, files)
        .map_err(InMemoryDatabaseLiveOpenError::Ownership)?;
    observer(InMemoryDatabaseOpenPhase::CompositionValidated);
    let database = recovery_required
        .complete_recovery::<_, N>(InMemoryDatabaseRecoveryInput {
            compatibility_context,
            storage,
            observer: &mut observer,
        })
        .map_err(InMemoryDatabaseLiveOpenError::Recovery)?;
    observer(InMemoryDatabaseOpenPhase::LiveReleased);
    Ok(LiveInMemoryDatabase { database })
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
    /// A modeled create has not selected its final manifest yet.
    UnpublishedCreate {
        /// Exact durable create phase that remains unselected.
        phase: InMemoryDatabaseCreatePhase,
    },
    /// A published create record lacks its exact selected-object evidence.
    CreateStateCorrupt {
        /// Exact durable create phase whose evidence is invalid.
        phase: InMemoryDatabaseCreatePhase,
    },
    /// Supplied objects differ from the objects selected by published create.
    PublishedCreateSelectionMismatch,
    /// The supplied manifest differs from the manifest selected by the model.
    SelectedManifestMismatch,
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
    /// Recovery-required acquisition received another manifest lifecycle.
    ManifestLifecycle {
        /// Exact rejected lifecycle state.
        actual: ntsql_database::DatabaseManifestLifecycleState,
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
            Self::UnpublishedCreate { phase } => write!(
                formatter,
                "modeled database create remains unpublished at {phase}"
            ),
            Self::CreateStateCorrupt { phase } => {
                write!(
                    formatter,
                    "modeled database create {phase} state is invalid"
                )
            }
            Self::PublishedCreateSelectionMismatch => formatter
                .write_str("modeled objects differ from the published database create selection"),
            Self::SelectedManifestMismatch => {
                formatter.write_str("supplied manifest differs from the selected memory manifest")
            }
            Self::ObjectAlias { first, second } => {
                write!(formatter, "modeled database {second} aliases {first}")
            }
            Self::ManifestDatabaseIdMismatch { owner, manifest } => write!(
                formatter,
                "database manifest identity {} does not match owner {}",
                manifest.get(),
                owner.get()
            ),
            Self::ManifestLifecycle { actual } => write!(
                formatter,
                "recovery-required memory open cannot select {actual} manifest"
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
            | Self::UnpublishedCreate { .. }
            | Self::CreateStateCorrupt { .. }
            | Self::PublishedCreateSelectionMismatch
            | Self::SelectedManifestMismatch
            | Self::ObjectAlias { .. }
            | Self::ManifestDatabaseIdMismatch { .. }
            | Self::ManifestLifecycle { .. }
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
