//! I/O-free bounded logical recovery model.
//!
//! Every type in this crate is ntsql-internal. The crate defines no SQL Server
//! recovery semantics, checkpoint format, WAL encoding, or persistent byte
//! representation. It models the logical state machine that a recovery
//! subsystem must implement, including crash/reopen, checkpoint publication,
//! WAL truncation, and deterministic bounded trace generation for testing.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroU128;

// ---------------------------------------------------------------------------
// Persistent and ephemeral identity
// ---------------------------------------------------------------------------

/// Persistent log identity (nonzero u128, 16-byte filesystem field).
///
/// Assigned once at database creation and never changed. Every WAL record,
/// page snapshot, checkpoint, generation, and selected anchor belongs to
/// exactly one persistent source identified by its `LogId`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LogId(NonZeroU128);

impl LogId {
    /// Creates a log identity. Returns `None` for zero.
    #[must_use]
    pub const fn new(value: u128) -> Option<Self> {
        match NonZeroU128::new(value) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
    /// Raw numeric value.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0.get()
    }
}

impl fmt::Display for LogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "log:{}", self.0)
    }
}

/// Ephemeral runtime lineage identity.
///
/// Incremented on each `reopen`. Positions from different lineages must not
/// be treated as equal even when numerically identical.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LineageId(u64);

impl LineageId {
    /// Creates a lineage identity.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    /// Raw numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
    /// Checked increment.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

impl fmt::Display for LineageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lineage:{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Identity and position types
// ---------------------------------------------------------------------------

/// Monotonically assigned WAL position (nonzero, starts at 1).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WalPosition(u64);

impl WalPosition {
    /// Creates a position from a nonzero raw value.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }
    /// Raw numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WalPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pos:{}", self.0)
    }
}

/// Transaction identity as epoch/sequence pair (both nonzero).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TransactionId {
    epoch: u64,
    sequence: u64,
}

impl TransactionId {
    /// Creates a transaction identity. Returns `None` if either is zero.
    #[must_use]
    pub const fn new(epoch: u64, sequence: u64) -> Option<Self> {
        if epoch == 0 || sequence == 0 {
            None
        } else {
            Some(Self { epoch, sequence })
        }
    }
    /// Coordinator epoch.
    #[must_use]
    pub const fn epoch(self) -> u64 {
        self.epoch
    }
    /// Per-epoch sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "txn:{}:{}", self.epoch, self.sequence)
    }
}

/// Opaque page identifier (nonzero).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PageId(u64);

impl PageId {
    /// Creates a page identifier. Returns `None` for zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }
    /// Raw numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "page:{}", self.0)
    }
}

/// Physical WAL generation. Starts at 0 and increments exactly once on
/// successful reclamation. Generation zero is a fresh database before any
/// reclamation. A nonzero generation stores/binds the exact selected
/// checkpoint anchor that authorized the reclamation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Generation(u64);

impl Generation {
    /// Generation zero.
    pub const ZERO: Self = Self(0);
    /// Creates a generation from a raw value.
    #[must_use]
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }
    /// Raw numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
    /// Whether this is the initial generation.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
    /// Checked increment.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gen:{}", self.0)
    }
}

/// Page version carried by a WAL page record. Assigned by the caller.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PageVersion(u64);

impl PageVersion {
    /// Creates a page version.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    /// Raw numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PageVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pv:{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// WAL record types (Point 8: includes log_id + lineage_id)
// ---------------------------------------------------------------------------

/// Kind of a logical WAL record.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WalRecordKind {
    /// Page write owned by a transaction.
    TransactionPage,
    /// Transaction commit marker.
    TransactionCommit,
    /// Raw (non-transactional) page write.
    RawPage,
}

impl fmt::Display for WalRecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TransactionPage => "txn-page",
            Self::TransactionCommit => "txn-commit",
            Self::RawPage => "raw-page",
        })
    }
}

/// Logical WAL record with explicit monotonic position, bound to log and lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalRecord {
    /// Persistent log identity.
    pub log_id: LogId,
    /// Ephemeral lineage identity.
    pub lineage_id: LineageId,
    /// Position in the WAL.
    pub position: WalPosition,
    /// Kind of record.
    pub kind: WalRecordKind,
    /// Owning transaction, if any.
    pub transaction: Option<TransactionId>,
    /// Affected page, if any.
    pub page: Option<PageId>,
    /// Abstract byte value, if any.
    pub page_value: Option<u64>,
    /// Page version, if any.
    pub page_version: Option<PageVersion>,
}

// ---------------------------------------------------------------------------
// Page snapshot (Point 8: includes log_id + lineage_id)
// ---------------------------------------------------------------------------

/// Snapshot of a page in the page store, bound to log and lineage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageSnapshot {
    /// Persistent log identity.
    pub log_id: LogId,
    /// Ephemeral lineage identity.
    pub lineage_id: LineageId,
    /// Page identifier.
    pub page_id: PageId,
    /// Abstract byte value.
    pub value: u64,
    /// WAL position that wrote this value.
    pub written_at: WalPosition,
    /// Version from the WAL page record.
    pub version: PageVersion,
}

// ---------------------------------------------------------------------------
// Checkpoint types (Point 14: corrected frontier docs)
// ---------------------------------------------------------------------------

/// Checkpoint frontier: WAL position up to which checkpoint coverage extends.
///
/// Pages at or before the frontier may still be missing or behind in the
/// checkpoint page store when `replay_start` is present. `None` represents an
/// empty checkpoint published before any WAL record.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct CheckpointFrontier(Option<WalPosition>);

impl CheckpointFrontier {
    /// Creates a frontier at the given position.
    #[must_use]
    pub const fn at(position: WalPosition) -> Self {
        Self(Some(position))
    }
    /// An empty frontier (no WAL records covered).
    #[must_use]
    pub const fn empty() -> Self {
        Self(None)
    }
    /// WAL position, if any.
    #[must_use]
    pub const fn position(self) -> Option<WalPosition> {
        self.0
    }
}

impl fmt::Display for CheckpointFrontier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(p) => write!(f, "frontier:{p}"),
            None => f.write_str("frontier:empty"),
        }
    }
}

/// Content-derived checkpoint identity.
///
/// Opaque `(version, digest)` anchor supplied by the runner/adapter.
/// The model never computes this internally.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct CheckpointAnchor {
    /// Format version.
    pub version: u16,
    /// Content digest.
    pub digest: u128,
}

impl CheckpointAnchor {
    /// Constructs an anchor from explicit parts.
    #[must_use]
    pub const fn new(version: u16, digest: u128) -> Self {
        Self { version, digest }
    }
}

impl fmt::Display for CheckpointAnchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "anchor(v:{},d:{:x})", self.version, self.digest)
    }
}

/// Summary of transaction state for checkpoint seeding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionSummary {
    /// Active transactions and their positions.
    pub active: BTreeMap<TransactionId, Vec<WalPosition>>,
    /// Epoch high-water at summary time.
    pub epoch_high_water: Option<u64>,
}

/// Checkpoint snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointSnapshot {
    /// Persistent log identity.
    pub log_id: LogId,
    /// Ephemeral lineage identity.
    pub lineage_id: LineageId,
    /// Opaque anchor identity.
    pub anchor: CheckpointAnchor,
    /// Frontier.
    pub frontier: CheckpointFrontier,
    /// Inclusive replay start derived from missing/behind prefix pages.
    ///
    /// `Some(pos)` when at least one durable page record at or before the
    /// frontier has no corresponding page-store entry (or the store entry is
    /// behind). Replay must include `pos` and everything after it.
    /// `None` when every durable page record is represented in the store;
    /// replay begins strictly after the frontier.
    pub replay_start: Option<WalPosition>,
    /// Page snapshots at checkpoint time.
    pub pages: BTreeMap<PageId, PageSnapshot>,
    /// Transaction summary for pruned-generation restart.
    pub transaction_summary: TransactionSummary,
}

// ---------------------------------------------------------------------------
// Replacement stages and candidate state (Points 6, 7)
// ---------------------------------------------------------------------------

/// Filesystem replacement stage (shared by checkpoint and WAL replacement).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ReplacementStage {
    BeforeCleanup,
    AfterCleanup,
    AfterCreate,
    AfterWrite,
    BeforeSync,
    AfterSync,
    BeforeRename,
    AfterCurrentReplace,
    DuringDirectorySync,
    AfterDirectorySync,
}

/// Complete ordered list of replacement stages.
pub const REPLACEMENT_STAGES: [ReplacementStage; 10] = [
    ReplacementStage::BeforeCleanup,
    ReplacementStage::AfterCleanup,
    ReplacementStage::AfterCreate,
    ReplacementStage::AfterWrite,
    ReplacementStage::BeforeSync,
    ReplacementStage::AfterSync,
    ReplacementStage::BeforeRename,
    ReplacementStage::AfterCurrentReplace,
    ReplacementStage::DuringDirectorySync,
    ReplacementStage::AfterDirectorySync,
];

impl ReplacementStage {
    /// Next stage in the protocol, or `None` at terminal.
    #[must_use]
    pub fn next(self) -> Option<Self> {
        match self {
            Self::BeforeCleanup => Some(Self::AfterCleanup),
            Self::AfterCleanup => Some(Self::AfterCreate),
            Self::AfterCreate => Some(Self::AfterWrite),
            Self::AfterWrite => Some(Self::BeforeSync),
            Self::BeforeSync => Some(Self::AfterSync),
            Self::AfterSync => Some(Self::BeforeRename),
            Self::BeforeRename => Some(Self::AfterCurrentReplace),
            Self::AfterCurrentReplace => Some(Self::DuringDirectorySync),
            Self::DuringDirectorySync => Some(Self::AfterDirectorySync),
            Self::AfterDirectorySync => None,
        }
    }
    /// Whether this stage is before the rename point.
    #[must_use]
    pub fn is_before_rename(self) -> bool {
        matches!(
            self,
            Self::BeforeCleanup
                | Self::AfterCleanup
                | Self::AfterCreate
                | Self::AfterWrite
                | Self::BeforeSync
                | Self::AfterSync
                | Self::BeforeRename
        )
    }
    /// Index of this stage (0-based).
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::BeforeCleanup => 0,
            Self::AfterCleanup => 1,
            Self::AfterCreate => 2,
            Self::AfterWrite => 3,
            Self::BeforeSync => 4,
            Self::AfterSync => 5,
            Self::BeforeRename => 6,
            Self::AfterCurrentReplace => 7,
            Self::DuringDirectorySync => 8,
            Self::AfterDirectorySync => 9,
        }
    }
}

impl fmt::Display for ReplacementStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BeforeCleanup => "before-cleanup",
            Self::AfterCleanup => "after-cleanup",
            Self::AfterCreate => "after-create",
            Self::AfterWrite => "after-write",
            Self::BeforeSync => "before-sync",
            Self::AfterSync => "after-sync",
            Self::BeforeRename => "before-rename",
            Self::AfterCurrentReplace => "after-current-replace",
            Self::DuringDirectorySync => "during-dir-sync",
            Self::AfterDirectorySync => "after-dir-sync",
        })
    }
}

/// Candidate file entry classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateEntry {
    /// No candidate file exists.
    Absent,
    /// Valid candidate matching expected content.
    Valid,
    /// Valid candidate with higher/newer content than selected.
    ValidHigher,
    /// Corrupt candidate.
    Corrupt,
    /// Partial/truncated candidate.
    PartialWrite,
    /// Dangling symlink.
    DanglingSymlink,
    /// Inode alias of selected.
    InodeAlias,
}

impl CandidateEntry {
    /// Whether this entry is cleanable on fresh open.
    #[must_use]
    pub fn is_cleanable(&self) -> bool {
        matches!(
            self,
            Self::Valid
                | Self::ValidHigher
                | Self::Corrupt
                | Self::PartialWrite
                | Self::DanglingSymlink
        )
    }
}

impl fmt::Display for CandidateEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Absent => "absent",
            Self::Valid => "valid",
            Self::ValidHigher => "valid-higher",
            Self::Corrupt => "corrupt",
            Self::PartialWrite => "partial",
            Self::DanglingSymlink => "dangling-symlink",
            Self::InodeAlias => "inode-alias",
        })
    }
}

/// Checkpoint replacement-attempt state, independent from the selected checkpoint.
///
/// `Present` means the replacement protocol is still in progress. Its `entry`
/// can be [`CandidateEntry::Absent`] after cleanup or after rename.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointCandidateState {
    Absent,
    Present {
        snapshot: CheckpointSnapshot,
        stage: ReplacementStage,
        entry: CandidateEntry,
    },
}

impl fmt::Display for CheckpointCandidateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("cp-candidate:absent"),
            Self::Present {
                snapshot,
                stage,
                entry,
                ..
            } => {
                write!(f, "cp-candidate:{}({},{stage})", entry, snapshot.anchor)
            }
        }
    }
}

/// WAL candidate state for generation replacement during reclamation.
///
/// Models the atomic WAL file replacement. At rename
/// (`AfterCurrentReplace`), the selected generation/anchor/retained suffix
/// switches to new values and the nested entry becomes
/// [`CandidateEntry::Absent`]. The enclosing `Present` attempt and both inode
/// locks remain until directory synchronization completes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WalCandidateState {
    Absent,
    Present {
        target_generation: Generation,
        anchor: CheckpointAnchor,
        retained_suffix: Vec<WalRecord>,
        retained_first: Option<WalPosition>,
        format_version: u16,
        logical_high_water: Option<WalPosition>,
        epoch_high_water: Option<u64>,
        stage: ReplacementStage,
        entry: CandidateEntry,
    },
}

/// Complete target state for one WAL generation replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalReplacement {
    pub target_generation: Generation,
    pub anchor: CheckpointAnchor,
    pub retained_suffix: Vec<WalRecord>,
    pub retained_first: Option<WalPosition>,
    pub format_version: u16,
    pub logical_high_water: Option<WalPosition>,
    pub epoch_high_water: Option<u64>,
}

impl fmt::Display for WalCandidateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("wal-candidate:absent"),
            Self::Present {
                target_generation,
                stage,
                ..
            } => {
                write!(f, "wal-candidate:{target_generation},{stage}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lock ownership (Point 7: WAL inode overlap)
// ---------------------------------------------------------------------------

/// Lock ownership observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockOwnership {
    Free,
    Recovery,
    Live,
}

impl fmt::Display for LockOwnership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Free => "free",
            Self::Recovery => "recovery",
            Self::Live => "live",
        })
    }
}

/// Independent lock state for WAL, page store, checkpoint, and WAL inode
/// overlap during replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Locks {
    /// WAL selected file lock.
    pub wal: LockOwnership,
    /// Page store lock.
    pub page_store: LockOwnership,
    /// Checkpoint lock.
    pub checkpoint: LockOwnership,
    /// Old WAL inode lock (held during replacement).
    pub wal_old_inode: LockOwnership,
    /// New WAL candidate inode lock (held during replacement).
    pub wal_new_inode: LockOwnership,
}

impl Locks {
    fn all_live() -> Self {
        Self {
            wal: LockOwnership::Live,
            page_store: LockOwnership::Live,
            checkpoint: LockOwnership::Live,
            wal_old_inode: LockOwnership::Free,
            wal_new_inode: LockOwnership::Free,
        }
    }
    fn all_free() -> Self {
        Self {
            wal: LockOwnership::Free,
            page_store: LockOwnership::Free,
            checkpoint: LockOwnership::Free,
            wal_old_inode: LockOwnership::Free,
            wal_new_inode: LockOwnership::Free,
        }
    }
    fn all_recovery() -> Self {
        Self {
            wal: LockOwnership::Recovery,
            page_store: LockOwnership::Recovery,
            checkpoint: LockOwnership::Recovery,
            wal_old_inode: LockOwnership::Free,
            wal_new_inode: LockOwnership::Free,
        }
    }
}

// ---------------------------------------------------------------------------
// Recovery phase / Applied / Completion / Retention / Metadata
// ---------------------------------------------------------------------------

/// Recovery phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryPhase {
    Live,
    Crashed,
    Unrecovered,
    Selected,
    ReplayPlanned,
    PagesRepaired,
    TransactionsRestored,
    CompleteLive,
    RetentionAnalyzed,
    Reclaimed,
}

impl fmt::Display for RecoveryPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Live => "live",
            Self::Crashed => "crashed",
            Self::Unrecovered => "unrecovered",
            Self::Selected => "selected",
            Self::ReplayPlanned => "replay-planned",
            Self::PagesRepaired => "pages-repaired",
            Self::TransactionsRestored => "txns-restored",
            Self::CompleteLive => "complete-live",
            Self::RetentionAnalyzed => "retention-analyzed",
            Self::Reclaimed => "reclaimed",
        })
    }
}

/// Result of an operation that may or may not have been applied before an
/// error was observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Applied<T, E> {
    /// Operation succeeded.
    Ok(T),
    /// No mutation occurred; error before application.
    Unapplied(E),
    /// Fully applied, then error observed.
    AppliedThenError { value: T, error: E },
}

impl<T, E> Applied<T, E> {
    /// Whether the operation mutated state.
    #[must_use]
    pub fn was_applied(&self) -> bool {
        matches!(self, Self::Ok(_) | Self::AppliedThenError { .. })
    }
    /// Extracts the value if applied.
    #[must_use]
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Ok(v) | Self::AppliedThenError { value: v, .. } => Some(v),
            Self::Unapplied(_) => None,
        }
    }
    /// Extracts the error if one was reported.
    #[must_use]
    pub fn error(&self) -> Option<&E> {
        match self {
            Self::Unapplied(e) | Self::AppliedThenError { error: e, .. } => Some(e),
            Self::Ok(_) => None,
        }
    }
}

/// Evidence recorded at recovery completion (Point 4: stale tracking).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionEvidence {
    /// Frontier at completion time.
    pub frontier: Option<WalPosition>,
    /// Generation at completion.
    pub generation: Generation,
    /// Selected checkpoint anchor.
    pub selected_anchor: Option<CheckpointAnchor>,
    /// Epoch high-water at completion.
    pub epoch_high_water: Option<u64>,
    /// Whether this evidence is stale due to live mutation after completion.
    pub stale: bool,
    /// Reason for staleness, if stale.
    pub stale_reason: Option<&'static str>,
}

/// Privately derived retention analysis, consumed exactly once by `reclaim`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RetentionAnalysis {
    retained_first: Option<WalPosition>,
    checkpoint_anchor: CheckpointAnchor,
    checkpoint_log_id: LogId,
}

/// Minimal generation metadata observed before selection (Point 5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MinimalMetadata {
    /// Persistent log identity.
    pub log_id: LogId,
    /// Physical WAL generation.
    pub generation: Generation,
}

/// Full generation metadata observed before selection (Point 5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullMetadata {
    /// Minimal fields.
    pub minimal: MinimalMetadata,
    /// Physical WAL format version.
    pub format_version: u16,
    /// First retained WAL position.
    pub retained_first: Option<WalPosition>,
    /// Logical high-water mark.
    pub logical_high_water: Option<WalPosition>,
    /// Epoch high-water mark.
    pub epoch_high_water: Option<u64>,
    /// Required anchor for nonzero generation.
    pub required_anchor: CheckpointAnchor,
}

/// Persisted transaction observations from checkpoint and WAL restoration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestorationSummary {
    /// Fresh coordinator epoch allocated above persisted high-water.
    pub fresh_epoch: u64,
    /// Active transactions recovered from checkpoint + WAL.
    pub active_transactions: BTreeMap<TransactionId, Vec<WalPosition>>,
}

// ---------------------------------------------------------------------------
// Model errors
// ---------------------------------------------------------------------------

/// Errors from model operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelError {
    InvalidPhase {
        current: RecoveryPhase,
        operation: &'static str,
    },
    PositionExhausted,
    GenerationExhausted,
    EpochExhausted,
    SequenceExhausted,
    LineageExhausted,
    TransactionNotFound {
        id: TransactionId,
    },
    TransactionAlreadyCommitted {
        id: TransactionId,
    },
    DuplicateTransactionId {
        id: TransactionId,
    },
    NoActiveEpoch,
    CheckpointAnchorMismatch {
        expected: Box<CheckpointAnchor>,
        actual: Option<Box<CheckpointAnchor>>,
    },
    NoCheckpointForNonzeroGeneration {
        generation: Generation,
    },
    MissingGenerationAnchor {
        generation: Generation,
    },
    ReplacementComplete,
    NoPendingCandidate,
    InvalidSelectedOnOpen {
        reason: &'static str,
    },
    NoFlushPosition,
    NoCheckpointForReclamation,
    NoCompletionEvidence,
    NoRetentionAnalysis,
    StaleCompletionEvidence {
        reason: &'static str,
    },
    RepairFault,
    ForeignLogId {
        expected: LogId,
        actual: LogId,
        context: &'static str,
    },
    VolatileSuffixPresent,
    PageNotDurable {
        page: PageId,
    },
    PageUncommitted {
        page: PageId,
        txn: TransactionId,
    },
    InvalidTransactionIndex {
        index: usize,
    },
    InvalidPageId,
    DuplicateWalPosition {
        position: WalPosition,
    },
    PageVersionExhausted,
    InvalidRecordShape {
        reason: &'static str,
    },
    DurableRecordMissing {
        position: WalPosition,
    },
    RetentionBoundaryMissing {
        position: WalPosition,
    },
    PageCheckpointContradiction {
        page: PageId,
    },
    PageStoreRegression {
        page: PageId,
        current: WalPosition,
        attempted: WalPosition,
    },
    RepairCountExhausted,
    InodeAliasCandidate {
        context: &'static str,
    },
    MetadataMismatch {
        field: &'static str,
    },
    ForeignSourceMetadata,
    /// Hard bound exceeded.
    BoundsExceeded {
        field: &'static str,
        value: u64,
        max: u64,
    },
    /// Mandatory skeleton requires more capacity than caller bounds allow.
    SkeletonRequiresMoreCapacity {
        field: &'static str,
        required: u64,
        allowed: u64,
    },
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPhase { current, operation } => {
                write!(f, "invalid phase {current} for {operation}")
            }
            Self::PositionExhausted => f.write_str("WAL position space exhausted"),
            Self::GenerationExhausted => f.write_str("generation space exhausted"),
            Self::EpochExhausted => f.write_str("epoch space exhausted"),
            Self::SequenceExhausted => f.write_str("sequence space exhausted"),
            Self::LineageExhausted => f.write_str("lineage space exhausted"),
            Self::TransactionNotFound { id } => write!(f, "txn not found: {id}"),
            Self::TransactionAlreadyCommitted { id } => write!(f, "txn committed: {id}"),
            Self::DuplicateTransactionId { id } => write!(f, "duplicate txn: {id}"),
            Self::NoActiveEpoch => f.write_str("no active epoch"),
            Self::CheckpointAnchorMismatch { expected, actual } => {
                write!(f, "anchor mismatch: expected {expected}, actual {actual:?}")
            }
            Self::NoCheckpointForNonzeroGeneration { generation } => {
                write!(f, "no checkpoint for nonzero {generation}")
            }
            Self::MissingGenerationAnchor { generation } => {
                write!(f, "missing anchor for nonzero {generation}")
            }
            Self::ReplacementComplete => f.write_str("replacement complete"),
            Self::NoPendingCandidate => f.write_str("no pending candidate"),
            Self::InvalidSelectedOnOpen { reason } => {
                write!(f, "invalid selected on open: {reason}")
            }
            Self::NoFlushPosition => f.write_str("no flush position"),
            Self::NoCheckpointForReclamation => f.write_str("no checkpoint for reclamation"),
            Self::NoCompletionEvidence => f.write_str("no completion evidence"),
            Self::NoRetentionAnalysis => f.write_str("no retention analysis"),
            Self::StaleCompletionEvidence { reason } => {
                write!(f, "stale completion: {reason}")
            }
            Self::RepairFault => f.write_str("repair fault"),
            Self::ForeignLogId {
                expected,
                actual,
                context,
            } => write!(
                f,
                "foreign log: expected {expected}, got {actual} ({context})"
            ),
            Self::VolatileSuffixPresent => f.write_str("volatile suffix present"),
            Self::PageNotDurable { page } => write!(f, "page not durable: {page}"),
            Self::PageUncommitted { page, txn } => {
                write!(f, "page {page} uncommitted txn {txn}")
            }
            Self::InvalidTransactionIndex { index } => {
                write!(f, "invalid txn index: {index}")
            }
            Self::InvalidPageId => f.write_str("invalid page id"),
            Self::DuplicateWalPosition { position } => {
                write!(f, "duplicate WAL position: {position}")
            }
            Self::PageVersionExhausted => f.write_str("page version exhausted"),
            Self::InvalidRecordShape { reason } => write!(f, "invalid record: {reason}"),
            Self::DurableRecordMissing { position } => {
                write!(f, "durable record missing at {position}")
            }
            Self::RetentionBoundaryMissing { position } => {
                write!(f, "retention boundary missing at {position}")
            }
            Self::PageCheckpointContradiction { page } => {
                write!(f, "checkpoint contradicts current {page}")
            }
            Self::PageStoreRegression {
                page,
                current,
                attempted,
            } => write!(
                f,
                "page-store regression for {page}: current {current}, attempted {attempted}"
            ),
            Self::RepairCountExhausted => f.write_str("repair count exhausted"),
            Self::InodeAliasCandidate { context } => write!(f, "inode alias: {context}"),
            Self::MetadataMismatch { field } => write!(f, "metadata mismatch: {field}"),
            Self::ForeignSourceMetadata => f.write_str("foreign source metadata"),
            Self::BoundsExceeded { field, value, max } => {
                write!(f, "bounds exceeded: {field}={value} > {max}")
            }
            Self::SkeletonRequiresMoreCapacity {
                field,
                required,
                allowed,
            } => write!(f, "skeleton needs {field}={required} but allowed={allowed}"),
        }
    }
}

impl Error for ModelError {}

// ---------------------------------------------------------------------------
// Recovery model
// ---------------------------------------------------------------------------

/// The logical recovery model state machine.
#[derive(Clone, Debug)]
pub struct RecoveryModel {
    log_id: LogId,
    lineage_id: LineageId,
    phase: RecoveryPhase,

    // WAL state
    wal_records: Vec<WalRecord>,
    flush_position: Option<WalPosition>,
    next_position: Option<u64>, // None = exhausted
    logical_high_water: Option<WalPosition>,

    // Epoch/transaction state
    current_epoch: Option<u64>,
    next_epoch: Option<u64>,    // None = exhausted
    next_sequence: Option<u64>, // None = exhausted
    epoch_high_water: Option<u64>,
    active_transactions: BTreeMap<TransactionId, Vec<WalPosition>>,
    persisted_transactions: BTreeMap<TransactionId, Vec<WalPosition>>,
    committed_transactions: BTreeMap<TransactionId, WalPosition>,

    // Page store
    page_store: BTreeMap<PageId, PageSnapshot>,

    // Checkpoint (Point A.1: slot vs selected)
    checkpoint_slot: Option<CheckpointSnapshot>,
    selected_checkpoint: Option<CheckpointSnapshot>,

    // Generation
    wal_generation: Generation,
    wal_format_version: u16,
    generation_anchor: Option<CheckpointAnchor>,
    retained_first: Option<WalPosition>,
    replacement_logical_high_water: Option<WalPosition>,
    replacement_epoch_high_water: Option<u64>,

    // Completion / retention (Point A.2/A.3: stale tracking)
    completion_evidence: Option<CompletionEvidence>,
    retention_analysis: Option<RetentionAnalysis>,

    // Candidates (Point 6: independent checkpoint + WAL)
    checkpoint_candidate: CheckpointCandidateState,
    wal_candidate: WalCandidateState,

    // Locks (Point 7: WAL inode overlap)
    locks: Locks,

    // Recovery replay state
    replay_plan: Option<Vec<WalRecord>>,
    restoration_summary: Option<RestorationSummary>,
    selected_wal_entry: SelectedEntryState,
    selected_checkpoint_entry: SelectedEntryState,
}

/// Validity of a selected persistent entry on fresh open.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectedEntryState {
    Valid,
    PartialWrite,
    Corrupt,
}

/// Complete-prefix WAL format before the first generation replacement.
pub const COMPLETE_PREFIX_FORMAT_VERSION: u16 = 3;
/// Generation-aware WAL format installed by reclamation.
pub const GENERATION_FORMAT_VERSION: u16 = 4;

impl RecoveryModel {
    /// Creates a fresh model with the given persistent log identity.
    pub fn new(log_id: LogId) -> Self {
        Self {
            log_id,
            lineage_id: LineageId::new(0),
            phase: RecoveryPhase::Live,
            wal_records: Vec::new(),
            flush_position: None,
            next_position: Some(1),
            logical_high_water: None,
            current_epoch: None,
            next_epoch: Some(1),
            next_sequence: None,
            epoch_high_water: None,
            active_transactions: BTreeMap::new(),
            persisted_transactions: BTreeMap::new(),
            committed_transactions: BTreeMap::new(),
            page_store: BTreeMap::new(),
            checkpoint_slot: None,
            selected_checkpoint: None,
            wal_generation: Generation::ZERO,
            wal_format_version: COMPLETE_PREFIX_FORMAT_VERSION,
            generation_anchor: None,
            retained_first: None,
            replacement_logical_high_water: None,
            replacement_epoch_high_water: None,
            completion_evidence: None,
            retention_analysis: None,
            checkpoint_candidate: CheckpointCandidateState::Absent,
            wal_candidate: WalCandidateState::Absent,
            locks: Locks::all_live(),
            replay_plan: None,
            restoration_summary: None,
            selected_wal_entry: SelectedEntryState::Valid,
            selected_checkpoint_entry: SelectedEntryState::Valid,
        }
    }

    // -- Accessors --

    /// Persistent log identity.
    #[must_use]
    pub fn log_id(&self) -> LogId {
        self.log_id
    }
    /// Current lineage identity.
    #[must_use]
    pub fn lineage_id(&self) -> LineageId {
        self.lineage_id
    }
    /// Current phase.
    #[must_use]
    pub fn phase(&self) -> RecoveryPhase {
        self.phase
    }
    /// WAL records.
    #[must_use]
    pub fn wal_records(&self) -> &[WalRecord] {
        &self.wal_records
    }
    /// Records covered by the current durable frontier.
    pub fn durable_wal_records(&self) -> impl Iterator<Item = &WalRecord> {
        self.wal_records.iter().take_while(|record| {
            self.flush_position
                .is_some_and(|frontier| record.position <= frontier)
        })
    }
    /// Flush position.
    #[must_use]
    pub fn flush_position(&self) -> Option<WalPosition> {
        self.flush_position
    }
    /// Logical high-water mark.
    #[must_use]
    pub fn logical_high_water(&self) -> Option<WalPosition> {
        self.logical_high_water
    }
    /// Next logical WAL position, or `None` when position space is exhausted.
    #[must_use]
    pub const fn next_logical_position(&self) -> Option<u64> {
        self.next_position
    }
    /// Epoch high-water mark.
    #[must_use]
    pub fn epoch_high_water(&self) -> Option<u64> {
        self.epoch_high_water
    }
    /// Page store.
    #[must_use]
    pub fn page_store(&self) -> &BTreeMap<PageId, PageSnapshot> {
        &self.page_store
    }
    /// Checkpoint publication slot.
    #[must_use]
    pub fn checkpoint_slot(&self) -> Option<&CheckpointSnapshot> {
        self.checkpoint_slot.as_ref()
    }
    /// Frozen selected checkpoint.
    #[must_use]
    pub fn selected_checkpoint(&self) -> Option<&CheckpointSnapshot> {
        self.selected_checkpoint.as_ref()
    }
    /// Physical WAL generation.
    #[must_use]
    pub fn wal_generation(&self) -> Generation {
        self.wal_generation
    }
    /// Physical WAL format version.
    #[must_use]
    pub fn wal_format_version(&self) -> u16 {
        self.wal_format_version
    }
    /// Generation anchor.
    #[must_use]
    pub fn generation_anchor(&self) -> Option<&CheckpointAnchor> {
        self.generation_anchor.as_ref()
    }
    /// First retained WAL position encoded by the installed replacement header.
    #[must_use]
    pub fn retained_first(&self) -> Option<WalPosition> {
        self.retained_first
    }
    /// Logical high-water encoded by the installed replacement header.
    #[must_use]
    pub fn replacement_logical_high_water(&self) -> Option<WalPosition> {
        self.replacement_logical_high_water
    }
    /// Epoch high-water encoded by the installed replacement header.
    #[must_use]
    pub fn replacement_epoch_high_water(&self) -> Option<u64> {
        self.replacement_epoch_high_water
    }
    /// Completion evidence.
    #[must_use]
    pub fn completion_evidence(&self) -> Option<&CompletionEvidence> {
        self.completion_evidence.as_ref()
    }
    /// Checkpoint candidate state.
    #[must_use]
    pub fn checkpoint_candidate(&self) -> &CheckpointCandidateState {
        &self.checkpoint_candidate
    }
    /// WAL candidate state.
    #[must_use]
    pub fn wal_candidate(&self) -> &WalCandidateState {
        &self.wal_candidate
    }
    /// Current locks.
    #[must_use]
    pub fn locks(&self) -> &Locks {
        &self.locks
    }
    /// Active transactions.
    #[must_use]
    pub fn active_transactions(&self) -> &BTreeMap<TransactionId, Vec<WalPosition>> {
        &self.active_transactions
    }
    /// Current epoch.
    #[must_use]
    pub fn current_epoch(&self) -> Option<u64> {
        self.current_epoch
    }
    /// Restoration summary.
    #[must_use]
    pub fn restoration_summary(&self) -> Option<&RestorationSummary> {
        self.restoration_summary.as_ref()
    }
    /// Replay plan.
    #[must_use]
    pub fn replay_plan(&self) -> Option<&[WalRecord]> {
        self.replay_plan.as_deref()
    }

    /// Sets selected-entry validity for a bounded crash/open scenario.
    pub fn set_selected_entries_for_open(
        &mut self,
        wal: SelectedEntryState,
        checkpoint: SelectedEntryState,
    ) -> Result<(), ModelError> {
        self.require_phase(&[RecoveryPhase::Crashed], "set_selected_entries_for_open")?;
        self.selected_wal_entry = wal;
        self.selected_checkpoint_entry = checkpoint;
        Ok(())
    }

    /// Installs a modeled checkpoint candidate before a fresh open.
    pub fn set_checkpoint_candidate_for_open(
        &mut self,
        candidate: CheckpointCandidateState,
    ) -> Result<(), ModelError> {
        self.require_phase(
            &[RecoveryPhase::Crashed],
            "set_checkpoint_candidate_for_open",
        )?;
        self.checkpoint_candidate = candidate;
        Ok(())
    }

    /// Installs a modeled WAL candidate before a fresh open.
    pub fn set_wal_candidate_for_open(
        &mut self,
        candidate: WalCandidateState,
    ) -> Result<(), ModelError> {
        self.require_phase(&[RecoveryPhase::Crashed], "set_wal_candidate_for_open")?;
        self.wal_candidate = candidate;
        Ok(())
    }

    // -- Internal helpers --

    fn require_phase(
        &self,
        allowed: &[RecoveryPhase],
        operation: &'static str,
    ) -> Result<(), ModelError> {
        if allowed.contains(&self.phase) {
            Ok(())
        } else {
            Err(ModelError::InvalidPhase {
                current: self.phase,
                operation,
            })
        }
    }

    fn require_live_or_complete(&self, op: &'static str) -> Result<(), ModelError> {
        self.require_phase(&[RecoveryPhase::Live, RecoveryPhase::CompleteLive], op)
    }

    fn allocate_position(&mut self) -> Result<WalPosition, ModelError> {
        let v = self.next_position.ok_or(ModelError::PositionExhausted)?;
        self.next_position = v.checked_add(1);
        let pos = match WalPosition::new(v) {
            Some(p) => p,
            None => return Err(ModelError::PositionExhausted),
        };
        Ok(pos)
    }

    /// Mark completion evidence stale (Point 4: don't erase).
    fn mark_completion_stale(&mut self, reason: &'static str) {
        if let Some(ref mut ev) = self.completion_evidence {
            ev.stale = true;
            ev.stale_reason = Some(reason);
        }
    }

    fn validate_record_shape(record: &WalRecord) -> Result<(), ModelError> {
        let valid = match record.kind {
            WalRecordKind::TransactionPage => {
                record.transaction.is_some()
                    && record.page.is_some()
                    && record.page_value.is_some()
                    && record.page_version.is_some()
            }
            WalRecordKind::TransactionCommit => {
                record.transaction.is_some()
                    && record.page.is_none()
                    && record.page_value.is_none()
                    && record.page_version.is_none()
            }
            WalRecordKind::RawPage => {
                record.transaction.is_none()
                    && record.page.is_some()
                    && record.page_value.is_some()
                    && record.page_version.is_some()
            }
        };
        if valid {
            Ok(())
        } else {
            Err(ModelError::InvalidRecordShape {
                reason: "WAL record fields do not match its kind",
            })
        }
    }

    // -- Live operations --

    /// Allocate a coordinator epoch. Advances epoch high-water even without
    /// any transaction commit.
    pub fn allocate_coordinator_epoch(&mut self) -> Result<u64, ModelError> {
        self.require_live_or_complete("allocate_epoch")?;
        let e = self.next_epoch.ok_or(ModelError::EpochExhausted)?;
        self.next_epoch = e.checked_add(1);
        self.current_epoch = Some(e);
        self.next_sequence = Some(1);
        // Epoch high-water advances on allocation
        match self.epoch_high_water {
            Some(hw) if hw >= e => {}
            _ => self.epoch_high_water = Some(e),
        }
        self.mark_completion_stale("epoch allocated");
        Ok(e)
    }

    /// Begin a new transaction under the current epoch.
    pub fn begin_transaction(&mut self) -> Result<TransactionId, ModelError> {
        self.require_live_or_complete("begin_transaction")?;
        let epoch = self.current_epoch.ok_or(ModelError::NoActiveEpoch)?;
        let seq = self.next_sequence.ok_or(ModelError::SequenceExhausted)?;
        self.next_sequence = seq.checked_add(1);
        let id = match TransactionId::new(epoch, seq) {
            Some(id) => id,
            None => return Err(ModelError::NoActiveEpoch),
        };
        if self.active_transactions.contains_key(&id) {
            return Err(ModelError::DuplicateTransactionId { id });
        }
        self.active_transactions.insert(id, Vec::new());
        self.mark_completion_stale("transaction begun");
        Ok(id)
    }

    /// Append a transaction page record. `page_version` is the version carried
    /// by the WAL record, `page_value` the abstract page bytes.
    pub fn append_transaction_page(
        &mut self,
        txn: TransactionId,
        page: PageId,
        page_value: u64,
        page_version: PageVersion,
    ) -> Result<WalPosition, ModelError> {
        self.require_live_or_complete("append_txn_page")?;
        if !self.active_transactions.contains_key(&txn) {
            return Err(ModelError::TransactionNotFound { id: txn });
        }
        let pos = self.allocate_position()?;
        let record = WalRecord {
            log_id: self.log_id,
            lineage_id: self.lineage_id,
            position: pos,
            kind: WalRecordKind::TransactionPage,
            transaction: Some(txn),
            page: Some(page),
            page_value: Some(page_value),
            page_version: Some(page_version),
        };
        self.wal_records.push(record);
        if let Some(positions) = self.active_transactions.get_mut(&txn) {
            positions.push(pos);
        }
        self.mark_completion_stale("page appended");
        Ok(pos)
    }

    /// Append a transaction commit marker.
    pub fn append_transaction_commit(
        &mut self,
        txn: TransactionId,
    ) -> Result<WalPosition, ModelError> {
        self.require_live_or_complete("append_commit")?;
        if !self.active_transactions.contains_key(&txn) {
            return Err(ModelError::TransactionNotFound { id: txn });
        }
        if self.committed_transactions.contains_key(&txn) {
            return Err(ModelError::TransactionAlreadyCommitted { id: txn });
        }
        let pos = self.allocate_position()?;
        let record = WalRecord {
            log_id: self.log_id,
            lineage_id: self.lineage_id,
            position: pos,
            kind: WalRecordKind::TransactionCommit,
            transaction: Some(txn),
            page: None,
            page_value: None,
            page_version: None,
        };
        self.wal_records.push(record);
        self.committed_transactions.insert(txn, pos);
        self.active_transactions.remove(&txn);
        self.mark_completion_stale("commit appended");
        Ok(pos)
    }

    /// Append a raw (non-transactional) page record.
    pub fn append_raw_page(
        &mut self,
        page: PageId,
        page_value: u64,
        page_version: PageVersion,
    ) -> Result<WalPosition, ModelError> {
        self.require_live_or_complete("append_raw_page")?;
        let pos = self.allocate_position()?;
        let record = WalRecord {
            log_id: self.log_id,
            lineage_id: self.lineage_id,
            position: pos,
            kind: WalRecordKind::RawPage,
            transaction: None,
            page: Some(page),
            page_value: Some(page_value),
            page_version: Some(page_version),
        };
        self.wal_records.push(record);
        self.mark_completion_stale("raw page appended");
        Ok(pos)
    }

    /// Flush WAL to make all appended records durable up to current position.
    pub fn flush_wal(&mut self) -> Result<Option<WalPosition>, ModelError> {
        self.require_live_or_complete("flush_wal")?;
        if let Some(position) = self.wal_records.last().map(|record| record.position) {
            self.flush_position = Some(position);
            self.logical_high_water = Some(position);
        }
        Ok(self.flush_position)
    }

    /// Write a page to the page store from a durable WAL record (Point 10:
    /// exhaustive shape validation).
    pub fn write_page_store(&mut self, position: WalPosition) -> Result<PageId, ModelError> {
        self.require_live_or_complete("write_page_store")?;
        let record = self
            .wal_records
            .iter()
            .find(|r| r.position == position)
            .ok_or(ModelError::DurableRecordMissing { position })?;
        // Validate identity
        if record.log_id != self.log_id {
            return Err(ModelError::ForeignLogId {
                expected: self.log_id,
                actual: record.log_id,
                context: "write_page_store record",
            });
        }
        Self::validate_record_shape(record)?;
        // Validate durability
        let durable_frontier = match self.flush_position {
            Some(fp) if fp >= position => fp,
            _ => match record.page {
                Some(page) => return Err(ModelError::PageNotDurable { page }),
                None => return Err(ModelError::DurableRecordMissing { position }),
            },
        };
        // Validate exact shape by kind (Point 10)
        match record.kind {
            WalRecordKind::TransactionPage => {
                let txn = record.transaction.ok_or(ModelError::InvalidRecordShape {
                    reason: "TransactionPage missing transaction",
                })?;
                let page = record.page.ok_or(ModelError::InvalidRecordShape {
                    reason: "TransactionPage missing page",
                })?;
                let value = record.page_value.ok_or(ModelError::InvalidRecordShape {
                    reason: "TransactionPage missing value",
                })?;
                let version = record.page_version.ok_or(ModelError::InvalidRecordShape {
                    reason: "TransactionPage missing version",
                })?;
                // Transaction page requires an exact durable commit.
                let durable_commit = self.wal_records.iter().any(|candidate| {
                    candidate.position <= durable_frontier
                        && candidate.kind == WalRecordKind::TransactionCommit
                        && candidate.transaction == Some(txn)
                        && candidate.page.is_none()
                        && candidate.page_value.is_none()
                        && candidate.page_version.is_none()
                        && candidate.log_id == self.log_id
                        && candidate.lineage_id == self.lineage_id
                });
                if !durable_commit {
                    return Err(ModelError::PageUncommitted { page, txn });
                }
                if let Some(current) = self.page_store.get(&page)
                    && current.written_at > position
                {
                    return Err(ModelError::PageStoreRegression {
                        page,
                        current: current.written_at,
                        attempted: position,
                    });
                }
                self.page_store.insert(
                    page,
                    PageSnapshot {
                        log_id: self.log_id,
                        lineage_id: self.lineage_id,
                        page_id: page,
                        value,
                        written_at: position,
                        version,
                    },
                );
                self.mark_completion_stale("page store written");
                Ok(page)
            }
            WalRecordKind::RawPage => {
                if record.transaction.is_some() {
                    return Err(ModelError::InvalidRecordShape {
                        reason: "RawPage has transaction",
                    });
                }
                let page = record.page.ok_or(ModelError::InvalidRecordShape {
                    reason: "RawPage missing page",
                })?;
                let value = record.page_value.ok_or(ModelError::InvalidRecordShape {
                    reason: "RawPage missing value",
                })?;
                let version = record.page_version.ok_or(ModelError::InvalidRecordShape {
                    reason: "RawPage missing version",
                })?;
                if let Some(current) = self.page_store.get(&page)
                    && current.written_at > position
                {
                    return Err(ModelError::PageStoreRegression {
                        page,
                        current: current.written_at,
                        attempted: position,
                    });
                }
                self.page_store.insert(
                    page,
                    PageSnapshot {
                        log_id: self.log_id,
                        lineage_id: self.lineage_id,
                        page_id: page,
                        value,
                        written_at: position,
                        version,
                    },
                );
                self.mark_completion_stale("page store written");
                Ok(page)
            }
            WalRecordKind::TransactionCommit => Err(ModelError::InvalidRecordShape {
                reason: "TransactionCommit has no page to write",
            }),
        }
    }

    /// Compute replay start from current durable records vs page store.
    fn compute_replay_start(&self) -> Option<WalPosition> {
        let frontier_pos = self.flush_position?;
        let mut earliest: Option<WalPosition> = None;
        for r in &self.wal_records {
            if r.position > frontier_pos {
                continue;
            }
            let page = match r.page {
                Some(p) => p,
                None => continue,
            };
            if !matches!(
                r.kind,
                WalRecordKind::TransactionPage | WalRecordKind::RawPage
            ) {
                continue;
            }
            let needs_replay = match self.page_store.get(&page) {
                None => true,
                Some(snap) => snap.written_at < r.position,
            };
            if needs_replay {
                match earliest {
                    None => earliest = Some(r.position),
                    Some(e) if r.position < e => earliest = Some(r.position),
                    _ => {}
                }
            }
        }
        earliest
    }

    /// Publish a checkpoint from the current flushed state.
    pub fn publish_checkpoint(&mut self, anchor: CheckpointAnchor) -> Result<(), ModelError> {
        self.require_live_or_complete("publish_checkpoint")?;
        let frontier = match self.flush_position {
            Some(fp) => CheckpointFrontier::at(fp),
            None => CheckpointFrontier::empty(),
        };
        let replay_start = self.compute_replay_start();
        let mut checkpoint_transactions = self.persisted_transactions.clone();
        for (transaction, positions) in &self.active_transactions {
            checkpoint_transactions.insert(*transaction, positions.clone());
        }
        let summary = TransactionSummary {
            active: checkpoint_transactions,
            epoch_high_water: self.epoch_high_water,
        };
        let cp = CheckpointSnapshot {
            log_id: self.log_id,
            lineage_id: self.lineage_id,
            anchor,
            frontier,
            replay_start,
            pages: self.page_store.clone(),
            transaction_summary: summary,
        };
        self.checkpoint_slot = Some(cp);
        // Point 4: publishing after completion marks it stale
        self.mark_completion_stale("checkpoint published");
        Ok(())
    }

    // -- Checkpoint candidate operations --

    /// Begin a checkpoint replacement.
    pub fn begin_checkpoint_candidate(
        &mut self,
        snapshot: CheckpointSnapshot,
    ) -> Result<(), ModelError> {
        self.require_live_or_complete("begin_cp_candidate")?;
        self.checkpoint_candidate = CheckpointCandidateState::Present {
            snapshot,
            stage: ReplacementStage::BeforeCleanup,
            entry: CandidateEntry::Absent,
        };
        self.mark_completion_stale("checkpoint replacement begun");
        Ok(())
    }

    /// Advance checkpoint candidate to next stage.
    pub fn advance_checkpoint_candidate(&mut self) -> Result<ReplacementStage, ModelError> {
        match &self.checkpoint_candidate {
            CheckpointCandidateState::Present {
                snapshot,
                stage,
                entry,
            } => {
                let next = stage.next().ok_or(ModelError::ReplacementComplete)?;
                let new_snapshot = snapshot.clone();
                let mut new_entry = match next {
                    ReplacementStage::AfterCleanup => CandidateEntry::Absent,
                    ReplacementStage::AfterCreate => CandidateEntry::PartialWrite,
                    ReplacementStage::AfterWrite
                    | ReplacementStage::BeforeSync
                    | ReplacementStage::AfterSync
                    | ReplacementStage::BeforeRename => CandidateEntry::Valid,
                    ReplacementStage::AfterCurrentReplace
                    | ReplacementStage::DuringDirectorySync
                    | ReplacementStage::AfterDirectorySync
                    | ReplacementStage::BeforeCleanup => entry.clone(),
                };

                if next == ReplacementStage::AfterCurrentReplace {
                    // At rename: new checkpoint becomes selected
                    self.checkpoint_slot = Some(new_snapshot.clone());
                    new_entry = CandidateEntry::Absent;
                    self.mark_completion_stale("checkpoint replaced");
                }

                if next == ReplacementStage::AfterDirectorySync {
                    self.checkpoint_candidate = CheckpointCandidateState::Absent;
                    return Ok(next);
                }

                self.checkpoint_candidate = CheckpointCandidateState::Present {
                    snapshot: new_snapshot,
                    stage: next,
                    entry: new_entry,
                };
                Ok(next)
            }
            CheckpointCandidateState::Absent => Err(ModelError::NoPendingCandidate),
        }
    }

    /// Reclassifies an interrupted checkpoint candidate for an open scenario.
    pub fn set_checkpoint_candidate_entry(
        &mut self,
        entry: CandidateEntry,
    ) -> Result<(), ModelError> {
        match &mut self.checkpoint_candidate {
            CheckpointCandidateState::Present { entry: current, .. } => {
                *current = entry;
                Ok(())
            }
            CheckpointCandidateState::Absent => Err(ModelError::NoPendingCandidate),
        }
    }

    // -- WAL candidate operations (Point 6) --

    /// Begin a WAL generation replacement candidate.
    pub fn begin_wal_candidate(&mut self, replacement: WalReplacement) -> Result<(), ModelError> {
        self.require_phase(&[RecoveryPhase::RetentionAnalyzed], "begin_wal_candidate")?;
        let expected_generation = self
            .wal_generation
            .checked_next()
            .ok_or(ModelError::GenerationExhausted)?;
        if replacement.target_generation != expected_generation {
            return Err(ModelError::MetadataMismatch {
                field: "candidate_generation",
            });
        }
        if replacement.format_version != GENERATION_FORMAT_VERSION {
            return Err(ModelError::MetadataMismatch {
                field: "candidate_format_version",
            });
        }
        if replacement
            .retained_suffix
            .iter()
            .any(|record| record.log_id != self.log_id || record.lineage_id != self.lineage_id)
        {
            return Err(ModelError::ForeignSourceMetadata);
        }
        // Lock both old and new inode during replacement (Point 7)
        self.locks.wal_old_inode = LockOwnership::Live;
        self.locks.wal_new_inode = LockOwnership::Live;

        self.wal_candidate = WalCandidateState::Present {
            target_generation: replacement.target_generation,
            anchor: replacement.anchor,
            retained_suffix: replacement.retained_suffix,
            retained_first: replacement.retained_first,
            format_version: replacement.format_version,
            logical_high_water: replacement.logical_high_water,
            epoch_high_water: replacement.epoch_high_water,
            stage: ReplacementStage::BeforeCleanup,
            entry: CandidateEntry::Absent,
        };
        Ok(())
    }

    /// Advance WAL candidate to next stage. At rename, switches selected
    /// generation/anchor/retained suffix.
    pub fn advance_wal_candidate(&mut self) -> Result<ReplacementStage, ModelError> {
        match &self.wal_candidate {
            WalCandidateState::Present {
                target_generation,
                anchor,
                retained_suffix,
                retained_first,
                format_version,
                logical_high_water,
                epoch_high_water,
                stage,
                ..
            } => {
                let next = stage.next().ok_or(ModelError::ReplacementComplete)?;
                let tg = *target_generation;
                let a = *anchor;
                let rs = retained_suffix.clone();
                let rf = *retained_first;
                let fv = *format_version;
                let lhw = *logical_high_water;
                let ehw = *epoch_high_water;

                if next == ReplacementStage::AfterCurrentReplace {
                    // At rename: switch selected generation/anchor/retained
                    self.wal_generation = tg;
                    self.wal_format_version = fv;
                    self.generation_anchor = Some(a);
                    self.wal_records = rs.clone();
                    self.retained_first = rf;
                    self.replacement_logical_high_water = lhw;
                    self.replacement_epoch_high_water = ehw;
                    self.logical_high_water = lhw;
                    self.flush_position = lhw;
                    self.epoch_high_water = ehw;
                }

                if next == ReplacementStage::AfterDirectorySync {
                    self.wal_candidate = WalCandidateState::Absent;
                    self.locks.wal_old_inode = LockOwnership::Free;
                    self.locks.wal_new_inode = LockOwnership::Free;
                    return Ok(next);
                }

                let entry = match next {
                    ReplacementStage::AfterCleanup => CandidateEntry::Absent,
                    ReplacementStage::AfterCreate => CandidateEntry::PartialWrite,
                    ReplacementStage::AfterWrite
                    | ReplacementStage::BeforeSync
                    | ReplacementStage::AfterSync
                    | ReplacementStage::BeforeRename => CandidateEntry::Valid,
                    ReplacementStage::AfterCurrentReplace
                    | ReplacementStage::DuringDirectorySync
                    | ReplacementStage::AfterDirectorySync => CandidateEntry::Absent,
                    ReplacementStage::BeforeCleanup => CandidateEntry::Absent,
                };
                self.wal_candidate = WalCandidateState::Present {
                    target_generation: tg,
                    anchor: a,
                    retained_suffix: rs,
                    retained_first: rf,
                    format_version: fv,
                    logical_high_water: lhw,
                    epoch_high_water: ehw,
                    stage: next,
                    entry,
                };
                Ok(next)
            }
            WalCandidateState::Absent => Err(ModelError::NoPendingCandidate),
        }
    }

    /// Reclassifies an interrupted WAL candidate for an open scenario.
    pub fn set_wal_candidate_entry(&mut self, entry: CandidateEntry) -> Result<(), ModelError> {
        match &mut self.wal_candidate {
            WalCandidateState::Present { entry: current, .. } => {
                *current = entry;
                Ok(())
            }
            WalCandidateState::Absent => Err(ModelError::NoPendingCandidate),
        }
    }

    // -- Crash / Reopen --

    fn validate_selected_on_open(&self) -> Result<(), ModelError> {
        if self.selected_wal_entry != SelectedEntryState::Valid {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "selected WAL is partial or corrupt",
            });
        }
        if self.wal_generation.is_zero() {
            if self.wal_format_version != COMPLETE_PREFIX_FORMAT_VERSION
                || self.generation_anchor.is_some()
                || self.retained_first.is_some()
                || self.replacement_logical_high_water.is_some()
                || self.replacement_epoch_high_water.is_some()
            {
                return Err(ModelError::InvalidSelectedOnOpen {
                    reason: "generation-zero WAL metadata is inconsistent",
                });
            }
        } else if self.wal_format_version != GENERATION_FORMAT_VERSION
            || self.generation_anchor.is_none()
        {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "pruned WAL metadata is incomplete",
            });
        }
        let mut previous = None;
        for record in &self.wal_records {
            if record.log_id != self.log_id || record.lineage_id != self.lineage_id {
                return Err(ModelError::ForeignSourceMetadata);
            }
            Self::validate_record_shape(record)?;
            if previous.is_some_and(|position| position >= record.position) {
                return Err(ModelError::InvalidSelectedOnOpen {
                    reason: "selected WAL positions are not strictly increasing",
                });
            }
            previous = Some(record.position);
        }
        let current_retained_first = self
            .durable_wal_records()
            .next()
            .map(|record| record.position);
        if !self.wal_generation.is_zero()
            && self.retained_first.is_some()
            && current_retained_first != self.retained_first
        {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "selected WAL retained-first metadata is inconsistent",
            });
        }
        if !self.wal_generation.is_zero()
            && self.retained_first.is_none()
            && current_retained_first.is_some_and(|current| {
                self.replacement_logical_high_water
                    .is_some_and(|baseline| current <= baseline)
            })
        {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "selected WAL post-replacement suffix overlaps header high-water",
            });
        }
        if !self.wal_generation.is_zero()
            && self.replacement_logical_high_water.is_some_and(|baseline| {
                self.logical_high_water
                    .is_none_or(|current| current < baseline)
            })
        {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "selected WAL current high-water precedes replacement header",
            });
        }
        if !self.wal_generation.is_zero()
            && self.replacement_epoch_high_water.is_some_and(|baseline| {
                self.epoch_high_water
                    .is_none_or(|current| current < baseline)
            })
        {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "selected WAL current epoch precedes replacement header",
            });
        }
        if self.flush_position != self.logical_high_water {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "selected WAL high-water metadata is inconsistent",
            });
        }
        for (page, snapshot) in &self.page_store {
            if snapshot.log_id != self.log_id
                || snapshot.lineage_id != self.lineage_id
                || snapshot.page_id != *page
            {
                return Err(ModelError::ForeignSourceMetadata);
            }
        }
        if self.selected_checkpoint_entry != SelectedEntryState::Valid {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "selected checkpoint is partial or corrupt",
            });
        }
        if let Some(checkpoint) = &self.checkpoint_slot {
            if checkpoint.log_id != self.log_id || checkpoint.lineage_id != self.lineage_id {
                return Err(ModelError::ForeignLogId {
                    expected: self.log_id,
                    actual: checkpoint.log_id,
                    context: "selected checkpoint on open",
                });
            }
            for (page, snapshot) in &checkpoint.pages {
                if snapshot.log_id != self.log_id
                    || snapshot.lineage_id != self.lineage_id
                    || snapshot.page_id != *page
                {
                    return Err(ModelError::ForeignSourceMetadata);
                }
            }
        }
        if !self.wal_generation.is_zero() {
            let checkpoint =
                self.checkpoint_slot
                    .as_ref()
                    .ok_or(ModelError::InvalidSelectedOnOpen {
                        reason: "pruned WAL has no selected checkpoint",
                    })?;
            if Some(checkpoint.anchor) != self.generation_anchor {
                return Err(ModelError::InvalidSelectedOnOpen {
                    reason: "pruned WAL checkpoint anchor does not match generation",
                });
            }
        } else if let Some(checkpoint) = &self.checkpoint_slot {
            self.validate_complete_prefix_checkpoint(checkpoint)?;
        }
        Ok(())
    }

    fn candidate_alias_context(&self) -> Option<&'static str> {
        if matches!(
            self.checkpoint_candidate,
            CheckpointCandidateState::Present {
                entry: CandidateEntry::InodeAlias,
                ..
            }
        ) {
            return Some("checkpoint candidate");
        }
        if matches!(
            self.wal_candidate,
            WalCandidateState::Present {
                entry: CandidateEntry::InodeAlias,
                ..
            }
        ) {
            return Some("WAL candidate");
        }
        None
    }

    fn rebrand_checkpoint(checkpoint: &mut CheckpointSnapshot, lineage: LineageId) {
        checkpoint.lineage_id = lineage;
        for snapshot in checkpoint.pages.values_mut() {
            snapshot.lineage_id = lineage;
        }
    }

    fn validate_complete_prefix_checkpoint(
        &self,
        checkpoint: &CheckpointSnapshot,
    ) -> Result<(), ModelError> {
        let frontier = checkpoint.frontier.position();
        if frontier > self.logical_high_water {
            return Err(ModelError::InvalidSelectedOnOpen {
                reason: "checkpoint frontier exceeds complete WAL prefix",
            });
        }
        if let Some(replay_start) = checkpoint.replay_start
            && (Some(replay_start) > frontier
                || !self
                    .wal_records
                    .iter()
                    .any(|record| record.position == replay_start))
        {
            return Err(ModelError::RetentionBoundaryMissing {
                position: replay_start,
            });
        }
        for (page, snapshot) in &checkpoint.pages {
            if snapshot.page_id != *page
                || snapshot.log_id != self.log_id
                || snapshot.lineage_id != self.lineage_id
                || Some(snapshot.written_at) > frontier
            {
                return Err(ModelError::PageCheckpointContradiction { page: *page });
            }
            let record = self
                .wal_records
                .iter()
                .find(|record| record.position == snapshot.written_at)
                .ok_or(ModelError::RetentionBoundaryMissing {
                    position: snapshot.written_at,
                })?;
            Self::validate_record_shape(record)?;
            if record.page != Some(*page)
                || record.page_value != Some(snapshot.value)
                || record.page_version != Some(snapshot.version)
            {
                return Err(ModelError::PageCheckpointContradiction { page: *page });
            }
            if record.kind == WalRecordKind::TransactionPage {
                let transaction = record.transaction.ok_or(ModelError::InvalidRecordShape {
                    reason: "checkpoint transaction page has no owner",
                })?;
                if !self.wal_records.iter().any(|candidate| {
                    candidate.kind == WalRecordKind::TransactionCommit
                        && candidate.transaction == Some(transaction)
                        && frontier.is_some_and(|position| candidate.position <= position)
                }) {
                    return Err(ModelError::PageUncommitted {
                        page: *page,
                        txn: transaction,
                    });
                }
            }
        }
        for (transaction, positions) in &checkpoint.transaction_summary.active {
            for position in positions {
                let record = self
                    .wal_records
                    .iter()
                    .find(|record| record.position == *position)
                    .ok_or(ModelError::RetentionBoundaryMissing {
                        position: *position,
                    })?;
                if record.kind != WalRecordKind::TransactionPage
                    || record.transaction != Some(*transaction)
                    || Some(record.position) > frontier
                {
                    return Err(ModelError::InvalidRecordShape {
                        reason: "checkpoint active transaction position is not exact",
                    });
                }
            }
        }
        if checkpoint.transaction_summary.epoch_high_water > self.epoch_high_water {
            return Err(ModelError::MetadataMismatch {
                field: "checkpoint_epoch_high_water",
            });
        }
        Ok(())
    }

    fn crash_with_complete_tail(&mut self, preserve_complete_tail: bool) -> Result<(), ModelError> {
        self.require_phase(
            &[
                RecoveryPhase::Live,
                RecoveryPhase::CompleteLive,
                RecoveryPhase::RetentionAnalyzed,
                RecoveryPhase::Reclaimed,
                RecoveryPhase::Unrecovered,
                RecoveryPhase::Selected,
                RecoveryPhase::ReplayPlanned,
                RecoveryPhase::PagesRepaired,
                RecoveryPhase::TransactionsRestored,
            ],
            "crash",
        )?;
        if !preserve_complete_tail {
            if let Some(fp) = self.flush_position {
                self.wal_records.retain(|record| record.position <= fp);
            } else {
                self.wal_records.clear();
            }
        }
        self.logical_high_water = self.flush_position;
        self.active_transactions.clear();
        self.persisted_transactions.clear();
        self.committed_transactions.clear();
        self.current_epoch = None;
        self.next_sequence = None;
        self.completion_evidence = None;
        self.retention_analysis = None;
        self.replay_plan = None;
        self.restoration_summary = None;
        self.selected_checkpoint = None;
        self.locks = Locks::all_free();
        self.phase = RecoveryPhase::Crashed;
        Ok(())
    }

    /// Crash and discard complete but unflushed WAL records while preserving
    /// the consumed position allocator range.
    pub fn crash(&mut self) -> Result<(), ModelError> {
        self.crash_with_complete_tail(false)
    }

    /// Crash while retaining a physically complete, unflushed WAL tail.
    ///
    /// Filesystem reopen can retain complete frames beyond the last durability
    /// marker. Recovery ignores that tail until a later flush covers it.
    pub fn crash_preserving_complete_wal_tail(&mut self) -> Result<(), ModelError> {
        self.crash_with_complete_tail(true)
    }

    /// Reopen after crash: assigns new lineage, transitions to Unrecovered.
    /// Requires explicit Crashed phase (Point C.14).
    pub fn reopen(&mut self) -> Result<LineageId, ModelError> {
        self.require_phase(&[RecoveryPhase::Crashed], "reopen")?;
        let new_lineage = self
            .lineage_id
            .checked_next()
            .ok_or(ModelError::LineageExhausted)?;
        self.validate_selected_on_open()?;
        if let Some(context) = self.candidate_alias_context() {
            return Err(ModelError::InodeAliasCandidate { context });
        }
        self.lineage_id = new_lineage;

        // Rebrand persisted observations with new lineage (Point 8)
        for r in &mut self.wal_records {
            r.lineage_id = new_lineage;
        }
        for snap in self.page_store.values_mut() {
            snap.lineage_id = new_lineage;
        }
        if let Some(checkpoint) = &mut self.checkpoint_slot {
            Self::rebrand_checkpoint(checkpoint, new_lineage);
        }
        self.checkpoint_candidate = CheckpointCandidateState::Absent;
        self.wal_candidate = WalCandidateState::Absent;

        self.locks = Locks::all_recovery();
        self.phase = RecoveryPhase::Unrecovered;
        Ok(new_lineage)
    }

    // -- Selection (Point 5: minimal/full metadata protocol) --

    /// Observe minimal metadata for selection validation.
    #[must_use]
    pub fn observe_minimal_metadata(&self) -> MinimalMetadata {
        MinimalMetadata {
            log_id: self.log_id,
            generation: self.wal_generation,
        }
    }

    /// Observe full metadata for selection validation.
    #[must_use]
    pub fn observe_full_metadata(&self) -> Option<FullMetadata> {
        let required_anchor = self.generation_anchor?;
        Some(FullMetadata {
            minimal: self.observe_minimal_metadata(),
            format_version: self.wal_format_version,
            retained_first: self
                .durable_wal_records()
                .next()
                .map(|record| record.position),
            logical_high_water: self.logical_high_water,
            epoch_high_water: self.epoch_high_water,
            required_anchor,
        })
    }

    /// Select a checkpoint for recovery using observed metadata protocol.
    ///
    /// Validates stability of minimal/full observations and exact current
    /// source metadata before freezing checkpoint. Generation zero is the
    /// only complete-prefix path.
    pub fn select_observed(
        &mut self,
        minimal: &MinimalMetadata,
        full: Option<&FullMetadata>,
    ) -> Result<(), ModelError> {
        self.require_phase(&[RecoveryPhase::Unrecovered], "select_observed")?;

        // Validate source identity
        if minimal.log_id != self.log_id {
            return Err(ModelError::ForeignSourceMetadata);
        }
        // Validate against current state
        let current = self.observe_minimal_metadata();
        if minimal != &current {
            return Err(ModelError::MetadataMismatch {
                field: "current_minimal_metadata",
            });
        }

        if self.wal_generation.is_zero() {
            if full.is_some() {
                return Err(ModelError::MetadataMismatch {
                    field: "generation_zero_full_metadata",
                });
            }
            if let Some(checkpoint) = &self.checkpoint_slot {
                self.validate_complete_prefix_checkpoint(checkpoint)?;
            }
        } else {
            let full = full.ok_or(ModelError::MissingGenerationAnchor {
                generation: self.wal_generation,
            })?;
            if full.minimal.log_id != self.log_id {
                return Err(ModelError::ForeignSourceMetadata);
            }
            if &full.minimal != minimal {
                return Err(ModelError::MetadataMismatch {
                    field: "minimal_full_generation",
                });
            }
            if full.format_version != self.wal_format_version
                || full.retained_first
                    != self
                        .durable_wal_records()
                        .next()
                        .map(|record| record.position)
                || full.logical_high_water != self.logical_high_water
                || full.epoch_high_water != self.epoch_high_water
            {
                return Err(ModelError::MetadataMismatch {
                    field: "current_full_metadata",
                });
            }
            if full.format_version != GENERATION_FORMAT_VERSION {
                return Err(ModelError::MetadataMismatch {
                    field: "pruned_format_version",
                });
            }
            let required_anchor = full.required_anchor;
            let gen_anchor = self
                .generation_anchor
                .ok_or(ModelError::MissingGenerationAnchor {
                    generation: self.wal_generation,
                })?;
            if required_anchor != gen_anchor {
                return Err(ModelError::CheckpointAnchorMismatch {
                    expected: Box::new(gen_anchor),
                    actual: Some(Box::new(required_anchor)),
                });
            }
            // Nonzero requires a checkpoint
            let cp = self.checkpoint_slot.as_ref().ok_or(
                ModelError::NoCheckpointForNonzeroGeneration {
                    generation: self.wal_generation,
                },
            )?;
            // Checkpoint must match the generation anchor
            if cp.anchor != gen_anchor {
                return Err(ModelError::CheckpointAnchorMismatch {
                    expected: Box::new(gen_anchor),
                    actual: Some(Box::new(cp.anchor)),
                });
            }
            // Validate checkpoint identity
            if cp.log_id != self.log_id {
                return Err(ModelError::ForeignLogId {
                    expected: self.log_id,
                    actual: cp.log_id,
                    context: "nonzero generation checkpoint",
                });
            }
        }

        if let Some(checkpoint) = &self.checkpoint_slot
            && (checkpoint.log_id != self.log_id || checkpoint.lineage_id != self.lineage_id)
        {
            return Err(ModelError::ForeignLogId {
                expected: self.log_id,
                actual: checkpoint.log_id,
                context: "checkpoint selection",
            });
        }

        // Freeze selected checkpoint
        self.selected_checkpoint = self.checkpoint_slot.clone();
        self.phase = RecoveryPhase::Selected;
        Ok(())
    }

    /// Convenience: select using local consistent observations.
    pub fn select(&mut self) -> Result<(), ModelError> {
        let minimal = self.observe_minimal_metadata();
        let full = self.observe_full_metadata();
        self.select_observed(&minimal, full.as_ref())
    }

    // -- Replay / Repair / Restore --

    /// Plan replay: determine which records need replaying.
    /// Uses inclusive/strict predicates without arithmetic (Point 11).
    pub fn plan_replay(&mut self) -> Result<usize, ModelError> {
        self.require_phase(&[RecoveryPhase::Selected], "plan_replay")?;
        let plan: Vec<WalRecord> = match &self.selected_checkpoint {
            Some(cp) => {
                let replay_start = cp.replay_start;
                let frontier_pos = cp.frontier.position();
                self.durable_wal_records()
                    .filter(|r| {
                        match replay_start {
                            Some(rs) => r.position >= rs,
                            None => match frontier_pos {
                                Some(fp) => r.position > fp,
                                None => true, // empty frontier: replay all
                            },
                        }
                    })
                    .cloned()
                    .collect()
            }
            None => {
                // No checkpoint: replay everything
                self.durable_wal_records().cloned().collect()
            }
        };
        let count = plan.len();
        self.replay_plan = Some(plan);
        self.phase = RecoveryPhase::ReplayPlanned;
        Ok(count)
    }

    /// Repair pages from the replay plan.
    ///
    /// Prepares exact decisions: only installs pages that are missing or
    /// behind in the current page store. Preserves already-current pages.
    pub fn repair_pages(&mut self) -> Result<usize, ModelError> {
        self.require_phase(&[RecoveryPhase::ReplayPlanned], "repair_pages")?;
        let repaired = self.repair_pages_inner()?;
        self.phase = RecoveryPhase::PagesRepaired;
        Ok(repaired)
    }

    /// Repair pages with a simulated fault: no page mutation + error reported.
    pub fn repair_pages_fault(&mut self) -> Result<(), ModelError> {
        self.require_phase(&[RecoveryPhase::ReplayPlanned], "repair_pages_fault")?;
        // No mutation: phase stays ReplayPlanned for retry
        Err(ModelError::RepairFault)
    }

    /// Repair pages with applied-then-error semantics.
    pub fn repair_pages_applied_fault(&mut self) -> Applied<usize, ModelError> {
        if let Err(e) = self.require_phase(
            &[RecoveryPhase::ReplayPlanned],
            "repair_pages_applied_fault",
        ) {
            return Applied::Unapplied(e);
        }
        // Actually apply the repair
        let repaired = match self.repair_pages_inner() {
            Ok(n) => n,
            Err(e) => return Applied::Unapplied(e),
        };
        self.phase = RecoveryPhase::PagesRepaired;
        Applied::AppliedThenError {
            value: repaired,
            error: ModelError::RepairFault,
        }
    }

    fn repair_pages_inner(&mut self) -> Result<usize, ModelError> {
        let plan = match &self.replay_plan {
            Some(p) => p.clone(),
            None => {
                return Err(ModelError::InvalidPhase {
                    current: self.phase,
                    operation: "repair_pages_inner (no replay plan)",
                });
            }
        };
        let mut repaired = 0usize;
        if let Some(cp) = &self.selected_checkpoint {
            for (pid, snap) in &cp.pages {
                match self.page_store.get(pid) {
                    None => {
                        let mut current = snap.clone();
                        current.lineage_id = self.lineage_id;
                        self.page_store.insert(*pid, current);
                        repaired = repaired
                            .checked_add(1)
                            .ok_or(ModelError::RepairCountExhausted)?;
                    }
                    Some(existing) if existing.written_at < snap.written_at => {
                        let mut current = snap.clone();
                        current.lineage_id = self.lineage_id;
                        self.page_store.insert(*pid, current);
                        repaired = repaired
                            .checked_add(1)
                            .ok_or(ModelError::RepairCountExhausted)?;
                    }
                    _ => {}
                }
            }
        }
        for r in &plan {
            if r.log_id != self.log_id || r.lineage_id != self.lineage_id {
                return Err(ModelError::ForeignSourceMetadata);
            }
            Self::validate_record_shape(r)?;
            let should_apply = match r.kind {
                WalRecordKind::RawPage => true,
                WalRecordKind::TransactionPage => {
                    let txn = r.transaction.ok_or(ModelError::InvalidRecordShape {
                        reason: "TransactionPage missing transaction during repair",
                    })?;
                    self.wal_records.iter().any(|candidate| {
                        candidate.kind == WalRecordKind::TransactionCommit
                            && candidate.transaction == Some(txn)
                            && self
                                .logical_high_water
                                .is_some_and(|frontier| candidate.position <= frontier)
                    })
                }
                WalRecordKind::TransactionCommit => false,
            };
            if !should_apply {
                continue;
            }
            let page = r.page.ok_or(ModelError::InvalidRecordShape {
                reason: "page record missing page during repair",
            })?;
            let value = r.page_value.ok_or(ModelError::InvalidRecordShape {
                reason: "page record missing value during repair",
            })?;
            let version = r.page_version.ok_or(ModelError::InvalidRecordShape {
                reason: "page record missing version during repair",
            })?;
            let should_install = match self.page_store.get(&page) {
                None => true,
                Some(existing) => existing.written_at < r.position,
            };
            if should_install {
                self.page_store.insert(
                    page,
                    PageSnapshot {
                        log_id: self.log_id,
                        lineage_id: self.lineage_id,
                        page_id: page,
                        value,
                        written_at: r.position,
                        version,
                    },
                );
                repaired = repaired
                    .checked_add(1)
                    .ok_or(ModelError::RepairCountExhausted)?;
            }
        }
        Ok(repaired)
    }

    /// Restore transactions from selected checkpoint + retained WAL suffix.
    ///
    /// Seeds persisted transaction observations from checkpoint
    /// `transaction_summary`, overlays WAL records. Allocates a fresh epoch
    /// strictly above persisted high-water (Point 2).
    pub fn restore_transactions(&mut self) -> Result<RestorationSummary, ModelError> {
        self.require_phase(&[RecoveryPhase::PagesRepaired], "restore_transactions")?;

        // Determine persisted epoch high-water: max of model hw and checkpoint summary hw
        let cp_epoch_hw = self
            .selected_checkpoint
            .as_ref()
            .and_then(|cp| cp.transaction_summary.epoch_high_water);
        let persisted_hw = match (self.epoch_high_water, cp_epoch_hw) {
            (Some(a), Some(b)) => Some(if a > b { a } else { b }),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        // Allocate fresh epoch strictly above persisted high-water
        let fresh_epoch = match persisted_hw {
            Some(hw) => hw.checked_add(1).ok_or(ModelError::EpochExhausted)?,
            None => 1,
        };
        // Check exhaustion before mutation
        let next_after_fresh = fresh_epoch.checked_add(1);

        // Seed persisted observations from the frozen selected checkpoint.
        let mut active = BTreeMap::new();
        if let Some(cp) = &self.selected_checkpoint {
            for (tid, positions) in &cp.transaction_summary.active {
                active.insert(*tid, positions.clone());
            }
        }
        let mut committed = BTreeMap::new();
        let replay = self
            .replay_plan
            .as_deref()
            .ok_or(ModelError::InvalidPhase {
                current: self.phase,
                operation: "restore_transactions (no replay plan)",
            })?;
        for record in replay {
            if record.log_id != self.log_id || record.lineage_id != self.lineage_id {
                return Err(ModelError::ForeignSourceMetadata);
            }
            Self::validate_record_shape(record)?;
            match record.kind {
                WalRecordKind::TransactionPage => {
                    let txn = record.transaction.ok_or(ModelError::InvalidRecordShape {
                        reason: "TransactionPage missing transaction during restoration",
                    })?;
                    let positions = active.entry(txn).or_insert_with(Vec::new);
                    if !positions.contains(&record.position) {
                        positions.push(record.position);
                    }
                }
                WalRecordKind::TransactionCommit => {
                    let txn = record.transaction.ok_or(ModelError::InvalidRecordShape {
                        reason: "TransactionCommit missing transaction during restoration",
                    })?;
                    active.remove(&txn);
                    committed.insert(txn, record.position);
                }
                WalRecordKind::RawPage => {
                    if record.transaction.is_some() {
                        return Err(ModelError::InvalidRecordShape {
                            reason: "RawPage has transaction during restoration",
                        });
                    }
                }
            }
        }

        // Apply state changes
        self.current_epoch = Some(fresh_epoch);
        self.next_epoch = next_after_fresh;
        self.next_sequence = Some(1);
        self.epoch_high_water = Some(fresh_epoch);
        self.active_transactions.clear();
        self.persisted_transactions = active.clone();
        self.committed_transactions = committed;

        let summary = RestorationSummary {
            fresh_epoch,
            active_transactions: active,
        };
        self.restoration_summary = Some(summary.clone());
        self.phase = RecoveryPhase::TransactionsRestored;
        Ok(summary)
    }

    /// Complete recovery. Records completion evidence.
    pub fn complete(&mut self) -> Result<(), ModelError> {
        self.require_phase(&[RecoveryPhase::TransactionsRestored], "complete")?;
        self.completion_evidence = Some(CompletionEvidence {
            frontier: self.flush_position,
            generation: self.wal_generation,
            selected_anchor: self.selected_checkpoint.as_ref().map(|cp| cp.anchor),
            epoch_high_water: self.epoch_high_water,
            stale: false,
            stale_reason: None,
        });
        self.locks = Locks::all_live();
        self.phase = RecoveryPhase::CompleteLive;
        Ok(())
    }

    /// Analyze retention (Point 3: conservative inclusive minimum derivation).
    ///
    /// Derives the retained first position from:
    /// - Frozen selected checkpoint replay_start (missing/behind prefix pages)
    /// - Current durable post-frontier suffix requirements
    /// - Exact page-store records not already in checkpoint
    /// - Unresolved transaction requirements
    ///
    /// All-checkpointed/no-suffix → `None` (empty retained).
    /// Missing-prefix → inclusive replay_start.
    /// Post-frontier → earliest required suffix position.
    pub fn analyze_retention(&mut self) -> Result<(), ModelError> {
        self.require_phase(&[RecoveryPhase::CompleteLive], "analyze_retention")?;

        // Must have non-stale completion evidence (Point 4)
        let evidence = self
            .completion_evidence
            .as_ref()
            .ok_or(ModelError::NoCompletionEvidence)?;
        if evidence.stale {
            let reason = match evidence.stale_reason {
                Some(reason) => reason,
                None => {
                    return Err(ModelError::InvalidRecordShape {
                        reason: "stale completion evidence has no reason",
                    });
                }
            };
            return Err(ModelError::StaleCompletionEvidence { reason });
        }

        let cp = self
            .selected_checkpoint
            .as_ref()
            .ok_or(ModelError::NoCheckpointForReclamation)?;

        // Validate checkpoint identity
        if cp.log_id != self.log_id || cp.lineage_id != self.lineage_id {
            return Err(ModelError::ForeignLogId {
                expected: self.log_id,
                actual: cp.log_id,
                context: "retention checkpoint",
            });
        }

        let anchor = cp.anchor;
        let frontier_pos = cp.frontier.position();
        let mut earliest_needed: Option<WalPosition> = None;

        if let Some(frontier) = frontier_pos {
            if !self
                .wal_records
                .iter()
                .any(|record| record.position == frontier)
            {
                return Err(ModelError::RetentionBoundaryMissing { position: frontier });
            }
            earliest_needed = Some(frontier);
        }

        if let Some(rs) = cp.replay_start {
            if !self.wal_records.iter().any(|record| record.position == rs) {
                return Err(ModelError::RetentionBoundaryMissing { position: rs });
            }
            earliest_needed = Some(earliest_needed.map_or(rs, |earliest| earliest.min(rs)));
        }

        for (transaction, positions) in &self.persisted_transactions {
            let Some(&pos) = positions.first() else {
                continue;
            };
            let record = self
                .wal_records
                .iter()
                .find(|record| record.position == pos)
                .ok_or(ModelError::RetentionBoundaryMissing { position: pos })?;
            if record.kind != WalRecordKind::TransactionPage
                || record.transaction != Some(*transaction)
                || record.log_id != self.log_id
                || record.lineage_id != self.lineage_id
            {
                return Err(ModelError::InvalidRecordShape {
                    reason: "active transaction retention boundary is not exact",
                });
            }
            earliest_needed = Some(earliest_needed.map_or(pos, |earliest| earliest.min(pos)));
        }

        for snap in self.page_store.values() {
            if cp.pages.get(&snap.page_id).is_some_and(|checkpoint| {
                checkpoint != snap && checkpoint.written_at >= snap.written_at
            }) {
                return Err(ModelError::PageCheckpointContradiction { page: snap.page_id });
            }
            let record = self
                .wal_records
                .iter()
                .find(|record| record.position == snap.written_at)
                .ok_or(ModelError::RetentionBoundaryMissing {
                    position: snap.written_at,
                })?;
            if record.log_id != snap.log_id
                || record.lineage_id != snap.lineage_id
                || record.page != Some(snap.page_id)
                || record.page_value != Some(snap.value)
                || record.page_version != Some(snap.version)
            {
                return Err(ModelError::PageCheckpointContradiction { page: snap.page_id });
            }
            if record.kind == WalRecordKind::TransactionPage {
                let transaction = record.transaction.ok_or(ModelError::InvalidRecordShape {
                    reason: "retained transaction page has no owner",
                })?;
                let committed = self.wal_records.iter().any(|candidate| {
                    candidate.kind == WalRecordKind::TransactionCommit
                        && candidate.transaction == Some(transaction)
                        && self
                            .logical_high_water
                            .is_some_and(|frontier| candidate.position <= frontier)
                });
                if !committed {
                    return Err(ModelError::PageUncommitted {
                        page: snap.page_id,
                        txn: transaction,
                    });
                }
            } else if record.kind != WalRecordKind::RawPage || record.transaction.is_some() {
                return Err(ModelError::InvalidRecordShape {
                    reason: "page-store retention boundary has invalid record kind",
                });
            }
            earliest_needed = Some(
                earliest_needed.map_or(snap.written_at, |earliest| earliest.min(snap.written_at)),
            );
        }

        if let Some(source_constraint) = self.retained_first {
            if !self
                .wal_records
                .iter()
                .any(|record| record.position == source_constraint)
            {
                return Err(ModelError::RetentionBoundaryMissing {
                    position: source_constraint,
                });
            }
            earliest_needed = Some(earliest_needed.map_or(source_constraint, |earliest| {
                earliest.min(source_constraint)
            }));
        }

        self.retention_analysis = Some(RetentionAnalysis {
            retained_first: earliest_needed,
            checkpoint_anchor: anchor,
            checkpoint_log_id: cp.log_id,
        });
        self.phase = RecoveryPhase::RetentionAnalyzed;
        Ok(())
    }

    /// Reclaim: consume the privately derived retention analysis.
    ///
    /// Runs the modeled atomic WAL install via WAL candidate lifecycle
    /// (Point 6). Requires no volatile suffix (Point A.4). Checks
    /// generation overflow before mutation.
    pub fn reclaim(&mut self) -> Result<Generation, ModelError> {
        self.require_phase(&[RecoveryPhase::RetentionAnalyzed], "reclaim")?;

        // Reject volatile suffix (Point A.4)
        if self.wal_records.last().map(|record| record.position) != self.flush_position
            && !self.wal_records.is_empty()
        {
            return Err(ModelError::VolatileSuffixPresent);
        }

        let analysis = self
            .retention_analysis
            .as_ref()
            .ok_or(ModelError::NoRetentionAnalysis)?;

        // Check generation overflow before mutation (Point A.4)
        let new_gen = self
            .wal_generation
            .checked_next()
            .ok_or(ModelError::GenerationExhausted)?;

        // Must have a checkpoint
        let cp = self
            .selected_checkpoint
            .as_ref()
            .ok_or(ModelError::NoCheckpointForReclamation)?;
        if cp.log_id != self.log_id || cp.log_id != analysis.checkpoint_log_id {
            return Err(ModelError::ForeignLogId {
                expected: self.log_id,
                actual: cp.log_id,
                context: "reclamation checkpoint",
            });
        }
        if cp.anchor != analysis.checkpoint_anchor {
            return Err(ModelError::CheckpointAnchorMismatch {
                expected: Box::new(analysis.checkpoint_anchor),
                actual: Some(Box::new(cp.anchor)),
            });
        }

        // Build retained suffix
        let retained_suffix: Vec<WalRecord> = match analysis.retained_first {
            Some(rf) => self
                .wal_records
                .iter()
                .filter(|r| r.position >= rf)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        if retained_suffix.first().map(|record| record.position) != analysis.retained_first
            && let Some(position) = analysis.retained_first
        {
            return Err(ModelError::RetentionBoundaryMissing { position });
        }
        if !matches!(self.wal_candidate, WalCandidateState::Absent) {
            return Err(ModelError::NoPendingCandidate);
        }
        let retained_first = analysis.retained_first;
        let checkpoint_anchor = analysis.checkpoint_anchor;

        // Run atomic install via WAL candidate (Point 6)
        self.begin_wal_candidate(WalReplacement {
            target_generation: new_gen,
            anchor: checkpoint_anchor,
            retained_suffix,
            retained_first,
            format_version: GENERATION_FORMAT_VERSION,
            logical_high_water: self.logical_high_water,
            epoch_high_water: self.epoch_high_water,
        })?;
        self.retention_analysis = None;

        // Advance through all stages to completion
        loop {
            match self.advance_wal_candidate() {
                Ok(stage) => {
                    if stage == ReplacementStage::AfterDirectorySync {
                        break;
                    }
                }
                Err(ModelError::ReplacementComplete) => break,
                Err(e) => return Err(e),
            }
        }

        self.phase = RecoveryPhase::Reclaimed;
        Ok(new_gen)
    }

    // -- Metadata observation helpers (Point 5) --

    /// Observe minimal metadata (public convenience).
    #[must_use]
    pub fn minimal_metadata(&self) -> MinimalMetadata {
        self.observe_minimal_metadata()
    }

    /// Observe full metadata (public convenience).
    #[must_use]
    pub fn full_metadata(&self) -> Option<FullMetadata> {
        self.observe_full_metadata()
    }
}

// ---------------------------------------------------------------------------
// Duplicate transaction table validation (Point 9)
// ---------------------------------------------------------------------------

/// An untrusted transaction table entry for duplicate detection.
///
/// Each entry represents one row in an untrusted transaction table.
/// Duplicate `TransactionId` keys indicate table corruption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UntrustedTransactionEntry {
    /// Transaction identity (the table key).
    pub id: TransactionId,
    /// Opaque data (position, state, etc.).
    pub data: u64,
}

/// Find duplicate transaction IDs in an untrusted transaction table.
///
/// Repeated transaction IDs across WAL records are normal. This function
/// validates an untrusted ordered transaction table where each entry carries
/// one complete observation; duplicate keys indicate corruption.
pub fn find_duplicate_in_untrusted_transaction_table(
    entries: &[UntrustedTransactionEntry],
) -> Option<TransactionId> {
    let mut seen = BTreeMap::new();
    for entry in entries {
        if seen.contains_key(&entry.id) {
            return Some(entry.id);
        }
        seen.insert(entry.id, entry.data);
    }
    None
}

// ---------------------------------------------------------------------------
// Observation comparison (Points 8, 11, 16)
// ---------------------------------------------------------------------------

/// Observable durable record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRecordObservation {
    pub log_id: LogId,
    pub lineage_id: LineageId,
    pub position: WalPosition,
    pub kind: WalRecordKind,
    pub transaction: Option<TransactionId>,
    pub page: Option<PageId>,
    pub page_value: Option<u64>,
    pub page_version: Option<PageVersion>,
}

/// Observable page entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageObservation {
    pub log_id: LogId,
    pub lineage_id: LineageId,
    pub page_id: PageId,
    pub value: u64,
    pub written_at: WalPosition,
    pub version: PageVersion,
}

/// Observation of the full model state for adapter comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation {
    pub log_id: LogId,
    pub lineage_id: LineageId,
    pub phase: RecoveryPhase,
    pub wal_generation: Generation,
    pub wal_format_version: u16,
    pub generation_anchor: Option<CheckpointAnchor>,
    pub current_retained_first: Option<WalPosition>,
    pub retained_first: Option<WalPosition>,
    pub replacement_logical_high_water: Option<WalPosition>,
    pub replacement_epoch_high_water: Option<u64>,
    pub logical_high_water: Option<WalPosition>,
    pub next_logical_position: Option<u64>,
    pub epoch_high_water: Option<u64>,
    pub current_epoch: Option<u64>,
    pub runtime_active_transactions: BTreeMap<TransactionId, Vec<WalPosition>>,
    pub persisted_transactions: BTreeMap<TransactionId, Vec<WalPosition>>,
    pub restoration_summary: Option<RestorationSummary>,
    pub durable_records: Vec<DurableRecordObservation>,
    pub pages: Vec<PageObservation>,
    pub checkpoint_slot: Option<CheckpointAnchor>,
    pub checkpoint_slot_lineage: Option<LineageId>,
    pub checkpoint_slot_frontier: Option<CheckpointFrontier>,
    pub selected_checkpoint: Option<CheckpointAnchor>,
    pub selected_checkpoint_lineage: Option<LineageId>,
    pub selected_frontier: Option<CheckpointFrontier>,
    pub completion_evidence: Option<CompletionEvidence>,
    pub checkpoint_candidate: CheckpointCandidateState,
    pub wal_candidate: WalCandidateState,
    pub has_retention_analysis: bool,
    pub selected_wal_entry: SelectedEntryState,
    pub selected_checkpoint_entry: SelectedEntryState,
    pub locks: Locks,
}

impl Observation {
    /// Build an observation from a model.
    #[must_use]
    pub fn from_model(m: &RecoveryModel) -> Self {
        let durable_records: Vec<DurableRecordObservation> = m
            .wal_records
            .iter()
            .filter(|r| match m.flush_position {
                Some(fp) => r.position <= fp,
                None => false,
            })
            .map(|r| DurableRecordObservation {
                log_id: r.log_id,
                lineage_id: r.lineage_id,
                position: r.position,
                kind: r.kind,
                transaction: r.transaction,
                page: r.page,
                page_value: r.page_value,
                page_version: r.page_version,
            })
            .collect();

        let pages: Vec<PageObservation> = m
            .page_store
            .values()
            .map(|s| PageObservation {
                log_id: s.log_id,
                lineage_id: s.lineage_id,
                page_id: s.page_id,
                value: s.value,
                written_at: s.written_at,
                version: s.version,
            })
            .collect();

        Self {
            log_id: m.log_id,
            lineage_id: m.lineage_id,
            phase: m.phase,
            wal_generation: m.wal_generation,
            wal_format_version: m.wal_format_version,
            generation_anchor: m.generation_anchor,
            current_retained_first: m.durable_wal_records().next().map(|record| record.position),
            retained_first: m.retained_first,
            replacement_logical_high_water: m.replacement_logical_high_water,
            replacement_epoch_high_water: m.replacement_epoch_high_water,
            logical_high_water: m.logical_high_water,
            next_logical_position: m.next_position,
            epoch_high_water: m.epoch_high_water,
            current_epoch: m.current_epoch,
            runtime_active_transactions: m.active_transactions.clone(),
            persisted_transactions: m.persisted_transactions.clone(),
            restoration_summary: m.restoration_summary.clone(),
            durable_records,
            pages,
            checkpoint_slot: m.checkpoint_slot.as_ref().map(|c| c.anchor),
            checkpoint_slot_lineage: m.checkpoint_slot.as_ref().map(|c| c.lineage_id),
            checkpoint_slot_frontier: m.checkpoint_slot.as_ref().map(|c| c.frontier),
            selected_checkpoint: m.selected_checkpoint.as_ref().map(|c| c.anchor),
            selected_checkpoint_lineage: m.selected_checkpoint.as_ref().map(|c| c.lineage_id),
            selected_frontier: m.selected_checkpoint.as_ref().map(|c| c.frontier),
            completion_evidence: m.completion_evidence.clone(),
            checkpoint_candidate: m.checkpoint_candidate.clone(),
            wal_candidate: m.wal_candidate.clone(),
            has_retention_analysis: m.retention_analysis.is_some(),
            selected_wal_entry: m.selected_wal_entry,
            selected_checkpoint_entry: m.selected_checkpoint_entry,
            locks: m.locks,
        }
    }
}

/// A typed contradiction found during observation comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Contradiction {
    pub field: String,
    pub expected: String,
    pub actual: String,
}

impl fmt::Display for Contradiction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: expected {}, got {}",
            self.field, self.expected, self.actual
        )
    }
}

/// Compare two observations, returning all contradictions.
///
/// On foreign log, skips position equality but collects every
/// identity-independent contradiction.
pub fn compare_observations(a: &Observation, b: &Observation) -> Vec<Contradiction> {
    let mut contradictions = Vec::new();
    let same_log = a.log_id == b.log_id;
    macro_rules! check {
        ($field:ident) => {
            if a.$field != b.$field {
                contradictions.push(Contradiction {
                    field: stringify!($field).into(),
                    expected: format!("{:?}", a.$field),
                    actual: format!("{:?}", b.$field),
                });
            }
        };
    }
    check!(log_id);
    check!(lineage_id);
    check!(phase);
    check!(wal_generation);
    check!(wal_format_version);
    check!(generation_anchor);
    check!(epoch_high_water);
    check!(current_epoch);
    check!(runtime_active_transactions);
    check!(persisted_transactions);
    check!(restoration_summary);
    check!(checkpoint_slot);
    check!(checkpoint_slot_lineage);
    check!(checkpoint_slot_frontier);
    check!(selected_checkpoint);
    check!(selected_checkpoint_lineage);
    check!(selected_frontier);
    check!(completion_evidence);
    check!(has_retention_analysis);
    check!(selected_wal_entry);
    check!(selected_checkpoint_entry);
    check!(locks);
    if a.pages.len() != b.pages.len() {
        contradictions.push(Contradiction {
            field: "page_count".into(),
            expected: a.pages.len().to_string(),
            actual: b.pages.len().to_string(),
        });
    }
    if a.durable_records.len() != b.durable_records.len() {
        contradictions.push(Contradiction {
            field: "durable_record_count".into(),
            expected: a.durable_records.len().to_string(),
            actual: b.durable_records.len().to_string(),
        });
    }
    if same_log {
        check!(current_retained_first);
        check!(retained_first);
        check!(replacement_logical_high_water);
        check!(replacement_epoch_high_water);
        check!(logical_high_water);
        check!(next_logical_position);
        check!(durable_records);
        check!(pages);
        check!(checkpoint_candidate);
        check!(wal_candidate);
    }

    contradictions
}

// ---------------------------------------------------------------------------
// Trace generation (Points 12, 17, 18)
// ---------------------------------------------------------------------------

/// Hard compile-time upper bounds.
pub const HARD_MAX_OPS: u64 = 10_000;
pub const HARD_MAX_TXNS: u64 = 1_000;
pub const HARD_MAX_PAGES: u64 = 10_000;
pub const HARD_MAX_POST_CP_RECORDS: u64 = 5_000;
pub const HARD_MAX_CRASH_CYCLES: u64 = 100;
pub const HARD_MAX_LOCAL_SEEDS: u64 = 100_000;

/// Trace operation.
#[derive(Clone, Debug)]
pub enum TraceOp {
    AllocateEpoch,
    BeginTransaction,
    AppendTransactionPage {
        txn_index: usize,
        page_id: u64,
        value: u64,
        version: u64,
    },
    AppendTransactionCommit {
        txn_index: usize,
    },
    AppendRawPage {
        page_id: u64,
        value: u64,
        version: u64,
    },
    FlushWal,
    WritePageStore {
        position_index: usize,
    },
    PublishCheckpoint {
        anchor: CheckpointAnchor,
    },
    Crash,
    Reopen,
    Select,
    PlanReplay,
    RepairPages,
    RepairPagesFault,
    RepairPagesAppliedFault,
    RestoreTransactions,
    Complete,
    AnalyzeRetention,
    AnalyzeRetentionExpectStale,
    Reclaim,
    // Post-completion live mutation for stale evidence testing
    LiveMutationAfterComplete {
        page_id: u64,
        value: u64,
        version: u64,
    },
}

impl fmt::Display for TraceOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllocateEpoch => f.write_str("allocate-epoch"),
            Self::BeginTransaction => f.write_str("begin-txn"),
            Self::AppendTransactionPage {
                txn_index, page_id, ..
            } => {
                write!(f, "append-txn-page(ti:{txn_index},p:{page_id})")
            }
            Self::AppendTransactionCommit { txn_index } => {
                write!(f, "append-commit(ti:{txn_index})")
            }
            Self::AppendRawPage { page_id, .. } => write!(f, "append-raw(p:{page_id})"),
            Self::FlushWal => f.write_str("flush"),
            Self::WritePageStore { position_index } => {
                write!(f, "write-ps(pi:{position_index})")
            }
            Self::PublishCheckpoint { anchor } => write!(f, "publish-cp({anchor})"),
            Self::Crash => f.write_str("crash"),
            Self::Reopen => f.write_str("reopen"),
            Self::Select => f.write_str("select"),
            Self::PlanReplay => f.write_str("plan-replay"),
            Self::RepairPages => f.write_str("repair"),
            Self::RepairPagesFault => f.write_str("repair-fault"),
            Self::RepairPagesAppliedFault => f.write_str("repair-applied-fault"),
            Self::RestoreTransactions => f.write_str("restore-txns"),
            Self::Complete => f.write_str("complete"),
            Self::AnalyzeRetention => f.write_str("analyze-retention"),
            Self::AnalyzeRetentionExpectStale => f.write_str("analyze-retention-expect-stale"),
            Self::Reclaim => f.write_str("reclaim"),
            Self::LiveMutationAfterComplete { page_id, .. } => {
                write!(f, "live-mutation(p:{page_id})")
            }
        }
    }
}

/// Checked configurable bounds for trace generation (Point 12).
#[derive(Clone, Debug)]
pub struct TraceBounds {
    pub max_ops: u64,
    pub max_txns: u64,
    pub max_pages: u64,
    pub max_post_checkpoint_records: u64,
    pub max_crash_cycles: u64,
}

/// Mandatory skeleton requires at least these minimums.
const SKELETON_MIN_OPS: u64 = 53;
const SKELETON_MIN_PAGES: u64 = 2;
const SKELETON_MIN_CRASH_CYCLES: u64 = 6;

impl TraceBounds {
    /// Validate bounds against hard caps and skeleton minimums.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.max_ops > HARD_MAX_OPS {
            return Err(ModelError::BoundsExceeded {
                field: "max_ops",
                value: self.max_ops,
                max: HARD_MAX_OPS,
            });
        }
        if self.max_txns > HARD_MAX_TXNS {
            return Err(ModelError::BoundsExceeded {
                field: "max_txns",
                value: self.max_txns,
                max: HARD_MAX_TXNS,
            });
        }
        if self.max_pages > HARD_MAX_PAGES {
            return Err(ModelError::BoundsExceeded {
                field: "max_pages",
                value: self.max_pages,
                max: HARD_MAX_PAGES,
            });
        }
        if self.max_post_checkpoint_records > HARD_MAX_POST_CP_RECORDS {
            return Err(ModelError::BoundsExceeded {
                field: "max_post_checkpoint_records",
                value: self.max_post_checkpoint_records,
                max: HARD_MAX_POST_CP_RECORDS,
            });
        }
        if self.max_crash_cycles > HARD_MAX_CRASH_CYCLES {
            return Err(ModelError::BoundsExceeded {
                field: "max_crash_cycles",
                value: self.max_crash_cycles,
                max: HARD_MAX_CRASH_CYCLES,
            });
        }
        if self.max_txns < 1 {
            return Err(ModelError::SkeletonRequiresMoreCapacity {
                field: "max_txns",
                required: 1,
                allowed: self.max_txns,
            });
        }
        if self.max_post_checkpoint_records < 1 {
            return Err(ModelError::SkeletonRequiresMoreCapacity {
                field: "max_post_checkpoint_records",
                required: 1,
                allowed: self.max_post_checkpoint_records,
            });
        }
        // Skeleton minimums
        if self.max_ops < SKELETON_MIN_OPS {
            return Err(ModelError::SkeletonRequiresMoreCapacity {
                field: "max_ops",
                required: SKELETON_MIN_OPS,
                allowed: self.max_ops,
            });
        }
        if self.max_pages < SKELETON_MIN_PAGES {
            return Err(ModelError::SkeletonRequiresMoreCapacity {
                field: "max_pages",
                required: SKELETON_MIN_PAGES,
                allowed: self.max_pages,
            });
        }
        if self.max_crash_cycles < SKELETON_MIN_CRASH_CYCLES {
            return Err(ModelError::SkeletonRequiresMoreCapacity {
                field: "max_crash_cycles",
                required: SKELETON_MIN_CRASH_CYCLES,
                allowed: self.max_crash_cycles,
            });
        }
        Ok(())
    }
}

/// Simple deterministic RNG from a u64 seed.
struct SimpleRng(u64);

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }
    fn next_u64(&mut self) -> u64 {
        // xorshift64
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn next_bounded(&mut self, bound: u64) -> Result<u64, ModelError> {
        if bound == 0 {
            return Err(ModelError::BoundsExceeded {
                field: "rng_bound",
                value: 0,
                max: 0,
            });
        }
        Ok(self.next_u64() % bound)
    }
}

/// Generate a deterministic legal trace from a seed.
pub fn generate_trace(seed: u64, bounds: &TraceBounds) -> Result<Vec<TraceOp>, ModelError> {
    bounds.validate()?;
    let mut rng = SimpleRng::new(seed);
    let mut ops: Vec<TraceOp> = Vec::new();

    // Use checked page counters
    let page1_raw: u64 = 1;
    let page2_raw: u64 = 2;

    // Bound-based anchor generation
    let bound_anchor = |rng: &mut SimpleRng| -> CheckpointAnchor {
        CheckpointAnchor::new(1, u128::from(rng.next_u64()) | 1)
    };

    // Phase 1: Basic work + checkpoint
    let anchor1 = bound_anchor(&mut rng);
    ops.push(TraceOp::AllocateEpoch);
    ops.push(TraceOp::BeginTransaction);
    let pv1 = rng.next_u64();
    ops.push(TraceOp::AppendTransactionPage {
        txn_index: 0,
        page_id: page1_raw,
        value: pv1,
        version: 1,
    });
    ops.push(TraceOp::AppendTransactionCommit { txn_index: 0 });
    let rv1 = rng.next_u64();
    ops.push(TraceOp::AppendRawPage {
        page_id: page2_raw,
        value: rv1,
        version: 1,
    });
    ops.push(TraceOp::FlushWal);
    ops.push(TraceOp::WritePageStore { position_index: 0 }); // txn page
    ops.push(TraceOp::WritePageStore { position_index: 2 }); // raw page
    ops.push(TraceOp::PublishCheckpoint { anchor: anchor1 });

    // Phase 2: Crash + recovery with repair-fault + retry
    ops.push(TraceOp::Crash);
    ops.push(TraceOp::Reopen);
    ops.push(TraceOp::Select);
    ops.push(TraceOp::PlanReplay);
    ops.push(TraceOp::RepairPagesFault); // fault: no mutation
    ops.push(TraceOp::Crash);
    ops.push(TraceOp::Reopen);
    ops.push(TraceOp::Select);
    ops.push(TraceOp::PlanReplay);
    ops.push(TraceOp::RepairPages); // successful retry after fault
    ops.push(TraceOp::RestoreTransactions);
    ops.push(TraceOp::Complete);

    // Phase 3: Stale evidence test - live mutation after completion
    ops.push(TraceOp::LiveMutationAfterComplete {
        page_id: page1_raw,
        value: rng.next_u64(),
        version: 2,
    });
    ops.push(TraceOp::AnalyzeRetentionExpectStale);

    // Phase 4: Fresh crash/reopen/recovery to fix stale evidence
    ops.push(TraceOp::Crash);
    ops.push(TraceOp::Reopen);
    ops.push(TraceOp::Select);
    ops.push(TraceOp::PlanReplay);
    // Applied fault for before/after snapshot testing
    ops.push(TraceOp::RepairPagesAppliedFault);
    ops.push(TraceOp::Crash);
    ops.push(TraceOp::Reopen);
    ops.push(TraceOp::Select);
    ops.push(TraceOp::PlanReplay);
    ops.push(TraceOp::RepairPages);
    ops.push(TraceOp::RestoreTransactions);
    ops.push(TraceOp::Complete);

    // Phase 5: Successful retention + reclamation
    ops.push(TraceOp::AnalyzeRetention);
    ops.push(TraceOp::Reclaim);

    // Phase 6: Post-reclaim crash + recovery (repeated reopen)
    ops.push(TraceOp::Crash);
    ops.push(TraceOp::Reopen);
    ops.push(TraceOp::Select);
    ops.push(TraceOp::PlanReplay);
    ops.push(TraceOp::RepairPages);
    ops.push(TraceOp::RestoreTransactions);
    ops.push(TraceOp::Complete);

    // Phase 7: Second retention + reclaim + crash + recovery
    ops.push(TraceOp::AnalyzeRetention);
    ops.push(TraceOp::Reclaim);
    ops.push(TraceOp::Crash);
    ops.push(TraceOp::Reopen);
    ops.push(TraceOp::Select);
    ops.push(TraceOp::PlanReplay);
    ops.push(TraceOp::RepairPages);
    ops.push(TraceOp::RestoreTransactions);
    ops.push(TraceOp::Complete);

    // Add bounded random variations if we have budget
    let used = u64::try_from(ops.len()).map_err(|_| ModelError::BoundsExceeded {
        field: "generated_ops",
        value: u64::MAX,
        max: bounds.max_ops,
    })?;
    if used < bounds.max_ops {
        let budget = bounds
            .max_ops
            .checked_sub(used)
            .ok_or(ModelError::BoundsExceeded {
                field: "generated_ops",
                value: used,
                max: bounds.max_ops,
            })?;
        let max_extra = (budget / 2).min(20);
        let extra_raw = if max_extra == 0 {
            0
        } else {
            rng.next_bounded(max_extra.checked_add(1).ok_or(ModelError::BoundsExceeded {
                field: "random_extra_ops",
                value: max_extra,
                max: 20,
            })?)?
        };
        let extra = usize::try_from(extra_raw).map_err(|_| ModelError::BoundsExceeded {
            field: "random_extra_ops",
            value: extra_raw,
            max: max_extra,
        })?;
        let max_ops = usize::try_from(bounds.max_ops).map_err(|_| ModelError::BoundsExceeded {
            field: "max_ops",
            value: bounds.max_ops,
            max: HARD_MAX_OPS,
        })?;
        // Insert random raw page appends before the first crash
        for i in 0..extra {
            let pidx = rng.next_bounded(bounds.max_pages)?.checked_add(1).ok_or(
                ModelError::BoundsExceeded {
                    field: "generated_page_id",
                    value: bounds.max_pages,
                    max: HARD_MAX_PAGES,
                },
            )?;
            let val = rng.next_u64();
            let ver_raw = u64::try_from(i).map_err(|_| ModelError::PageVersionExhausted)?;
            let ver = ver_raw
                .checked_add(100)
                .ok_or(ModelError::PageVersionExhausted)?;
            ops.insert(
                8, // before the first publish
                TraceOp::AppendRawPage {
                    page_id: pidx,
                    value: val,
                    version: ver,
                },
            );
            // Also insert a flush + write_page_store after it
            if ops.len() < max_ops {
                ops.insert(9, TraceOp::FlushWal);
            }
        }
    }

    validate_trace_against_bounds(&ops, bounds)?;
    Ok(ops)
}

fn validate_trace_against_bounds(ops: &[TraceOp], bounds: &TraceBounds) -> Result<(), ModelError> {
    let op_count = u64::try_from(ops.len()).map_err(|_| ModelError::BoundsExceeded {
        field: "trace_ops",
        value: u64::MAX,
        max: bounds.max_ops,
    })?;
    if op_count > bounds.max_ops {
        return Err(ModelError::BoundsExceeded {
            field: "trace_ops",
            value: op_count,
            max: bounds.max_ops,
        });
    }
    let mut transactions = 0u64;
    let mut crashes = 0u64;
    let mut pages = BTreeSet::new();
    let mut checkpoint_seen = false;
    let mut post_checkpoint_records = 0u64;
    for operation in ops {
        match operation {
            TraceOp::BeginTransaction => {
                transactions = transactions
                    .checked_add(1)
                    .ok_or(ModelError::BoundsExceeded {
                        field: "trace_transactions",
                        value: u64::MAX,
                        max: bounds.max_txns,
                    })?;
            }
            TraceOp::Crash => {
                crashes = crashes.checked_add(1).ok_or(ModelError::BoundsExceeded {
                    field: "trace_crashes",
                    value: u64::MAX,
                    max: bounds.max_crash_cycles,
                })?;
            }
            TraceOp::AppendTransactionPage { page_id, .. }
            | TraceOp::AppendRawPage { page_id, .. }
            | TraceOp::LiveMutationAfterComplete { page_id, .. } => {
                pages.insert(*page_id);
                if checkpoint_seen {
                    post_checkpoint_records = post_checkpoint_records.checked_add(1).ok_or(
                        ModelError::BoundsExceeded {
                            field: "trace_post_checkpoint_records",
                            value: u64::MAX,
                            max: bounds.max_post_checkpoint_records,
                        },
                    )?;
                }
            }
            TraceOp::AppendTransactionCommit { .. } if checkpoint_seen => {
                post_checkpoint_records =
                    post_checkpoint_records
                        .checked_add(1)
                        .ok_or(ModelError::BoundsExceeded {
                            field: "trace_post_checkpoint_records",
                            value: u64::MAX,
                            max: bounds.max_post_checkpoint_records,
                        })?;
            }
            TraceOp::PublishCheckpoint { .. } => {
                checkpoint_seen = true;
                post_checkpoint_records = 0;
            }
            _ => {}
        }
    }
    let page_count = u64::try_from(pages.len()).map_err(|_| ModelError::BoundsExceeded {
        field: "trace_pages",
        value: u64::MAX,
        max: bounds.max_pages,
    })?;
    for (field, value, max) in [
        ("trace_transactions", transactions, bounds.max_txns),
        ("trace_pages", page_count, bounds.max_pages),
        ("trace_crashes", crashes, bounds.max_crash_cycles),
        (
            "trace_post_checkpoint_records",
            post_checkpoint_records,
            bounds.max_post_checkpoint_records,
        ),
    ] {
        if value > max {
            return Err(ModelError::BoundsExceeded { field, value, max });
        }
    }
    Ok(())
}

/// Canonical CI seeds.
pub const CI_SEEDS: [u64; 5] = [1, 42, 100, 12345, 99999];

/// Local profile seed iterator (larger coverage).
pub fn local_seed_iter(count: u64) -> Result<impl Iterator<Item = u64>, ModelError> {
    if count > HARD_MAX_LOCAL_SEEDS {
        return Err(ModelError::BoundsExceeded {
            field: "local_seed_count",
            value: count,
            max: HARD_MAX_LOCAL_SEEDS,
        });
    }
    Ok(0..count)
}

// ---------------------------------------------------------------------------
// Trace execution (Points 13, 18)
// ---------------------------------------------------------------------------

/// Error during trace execution.
#[derive(Clone, Debug)]
pub struct TraceExecutionError {
    pub seed: u64,
    pub op_index: usize,
    pub operation: String,
    pub error: ModelError,
    pub prefix: Vec<String>,
}

impl fmt::Display for TraceExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "trace(seed={},op={},err={})\nprefix: [{}]",
            self.seed,
            self.op_index,
            self.error,
            self.prefix.join(", "),
        )
    }
}

impl Error for TraceExecutionError {}

/// Execute a trace against a fresh model.
pub fn execute_trace(
    seed: u64,
    ops: &[TraceOp],
    log_id: LogId,
) -> Result<RecoveryModel, Box<TraceExecutionError>> {
    let hard_bounds = TraceBounds {
        max_ops: HARD_MAX_OPS,
        max_txns: HARD_MAX_TXNS,
        max_pages: HARD_MAX_PAGES,
        max_post_checkpoint_records: HARD_MAX_POST_CP_RECORDS,
        max_crash_cycles: HARD_MAX_CRASH_CYCLES,
    };
    if let Err(error) = validate_trace_against_bounds(ops, &hard_bounds) {
        return Err(Box::new(TraceExecutionError {
            seed,
            op_index: 0,
            operation: "trace-bounds".into(),
            error,
            prefix: Vec::new(),
        }));
    }
    let mut model = RecoveryModel::new(log_id);
    let mut txn_ids: Vec<TransactionId> = Vec::new();
    let mut page_positions: Vec<WalPosition> = Vec::new();
    let mut page_version_counter: u64 = 0;

    for (i, op) in ops.iter().enumerate() {
        let prefix: Vec<String> = ops[..=i].iter().map(|o| format!("{o}")).collect();
        let map_err = |e: ModelError| -> Box<TraceExecutionError> {
            Box::new(TraceExecutionError {
                seed,
                op_index: i,
                operation: format!("{op}"),
                error: e,
                prefix: prefix.clone(),
            })
        };

        match op {
            TraceOp::AllocateEpoch => {
                model.allocate_coordinator_epoch().map_err(map_err)?;
            }
            TraceOp::BeginTransaction => {
                let id = model.begin_transaction().map_err(map_err)?;
                txn_ids.push(id);
            }
            TraceOp::AppendTransactionPage {
                txn_index,
                page_id,
                value,
                version,
            } => {
                if *txn_index >= txn_ids.len() {
                    return Err(map_err(ModelError::InvalidTransactionIndex {
                        index: *txn_index,
                    }));
                }
                let tid = txn_ids[*txn_index];
                let pid =
                    PageId::new(*page_id).ok_or_else(|| map_err(ModelError::InvalidPageId))?;
                let pv = PageVersion::new(*version);
                let pos = model
                    .append_transaction_page(tid, pid, *value, pv)
                    .map_err(map_err)?;
                page_positions.push(pos);
            }
            TraceOp::AppendTransactionCommit { txn_index } => {
                if *txn_index >= txn_ids.len() {
                    return Err(map_err(ModelError::InvalidTransactionIndex {
                        index: *txn_index,
                    }));
                }
                let tid = txn_ids[*txn_index];
                let pos = model.append_transaction_commit(tid).map_err(map_err)?;
                page_positions.push(pos);
            }
            TraceOp::AppendRawPage {
                page_id,
                value,
                version,
            } => {
                let pid =
                    PageId::new(*page_id).ok_or_else(|| map_err(ModelError::InvalidPageId))?;
                let pv = PageVersion::new(*version);
                let pos = model.append_raw_page(pid, *value, pv).map_err(map_err)?;
                page_positions.push(pos);
            }
            TraceOp::FlushWal => {
                model.flush_wal().map_err(map_err)?;
            }
            TraceOp::WritePageStore { position_index } => {
                if *position_index >= page_positions.len() {
                    return Err(map_err(ModelError::InvalidTransactionIndex {
                        index: *position_index,
                    }));
                }
                let pos = page_positions[*position_index];
                model.write_page_store(pos).map_err(map_err)?;
            }
            TraceOp::PublishCheckpoint { anchor } => {
                model.publish_checkpoint(*anchor).map_err(map_err)?;
            }
            TraceOp::Crash => {
                model.crash().map_err(map_err)?;
                // Clear execution state
                txn_ids.clear();
                page_positions.clear();
                page_version_counter = 0;
            }
            TraceOp::Reopen => {
                model.reopen().map_err(map_err)?;
                // Rebuild page_positions from model's WAL records
                page_positions = model.wal_records().iter().map(|r| r.position).collect();
            }
            TraceOp::Select => {
                model.select().map_err(map_err)?;
            }
            TraceOp::PlanReplay => {
                model.plan_replay().map_err(map_err)?;
            }
            TraceOp::RepairPages => {
                model.repair_pages().map_err(map_err)?;
            }
            TraceOp::RepairPagesFault => {
                // Expected to return RepairFault error
                match model.repair_pages_fault() {
                    Err(ModelError::RepairFault) => {} // expected
                    Err(e) => return Err(map_err(e)),
                    Ok(()) => {
                        return Err(map_err(ModelError::InvalidPhase {
                            current: model.phase(),
                            operation: "repair_fault should have failed",
                        }));
                    }
                }
            }
            TraceOp::RepairPagesAppliedFault => {
                match model.repair_pages_applied_fault() {
                    Applied::AppliedThenError { .. } => {} // expected
                    Applied::Unapplied(e) => return Err(map_err(e)),
                    Applied::Ok(_) => {
                        return Err(map_err(ModelError::InvalidPhase {
                            current: model.phase(),
                            operation: "applied_fault should have error",
                        }));
                    }
                }
            }
            TraceOp::RestoreTransactions => {
                model.restore_transactions().map_err(map_err)?;
            }
            TraceOp::Complete => {
                model.complete().map_err(map_err)?;
                page_version_counter = 0;
            }
            TraceOp::AnalyzeRetention => {
                model.analyze_retention().map_err(map_err)?;
            }
            TraceOp::AnalyzeRetentionExpectStale => match model.analyze_retention() {
                Err(ModelError::StaleCompletionEvidence { .. }) => {}
                Err(error) => return Err(map_err(error)),
                Ok(()) => {
                    return Err(map_err(ModelError::InvalidPhase {
                        current: model.phase(),
                        operation: "retention analysis unexpectedly accepted stale evidence",
                    }));
                }
            },
            TraceOp::Reclaim => {
                model.reclaim().map_err(map_err)?;
            }
            TraceOp::LiveMutationAfterComplete {
                page_id,
                value,
                version,
            } => {
                // Perform live mutation: append raw page + flush
                let pid =
                    PageId::new(*page_id).ok_or_else(|| map_err(ModelError::InvalidPageId))?;
                page_version_counter = page_version_counter
                    .checked_add(1)
                    .ok_or_else(|| map_err(ModelError::PageVersionExhausted))?;
                let pv = PageVersion::new(*version);
                let pos = model.append_raw_page(pid, *value, pv).map_err(map_err)?;
                model.flush_wal().map_err(map_err)?;
                page_positions.push(pos);
            }
        }
    }

    Ok(model)
}

/// Error during prefix search.
#[derive(Clone, Debug)]
pub struct PrefixSearchError {
    pub seed: u64,
    pub op_index: usize,
    pub error: ModelError,
    pub prefix: Vec<String>,
}

impl fmt::Display for PrefixSearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "prefix-search(seed={},op={},err={})\nprefix: [{}]",
            self.seed,
            self.op_index,
            self.error,
            self.prefix.join(", "),
        )
    }
}

impl Error for PrefixSearchError {}

/// Find a minimal failing prefix for a predicate.
///
/// Does not claim global minimality. Returns the shortest prefix that
/// satisfies the predicate when executed.
pub fn find_minimal_prefix<F>(
    seed: u64,
    ops: &[TraceOp],
    log_id: LogId,
    predicate: F,
) -> Result<Option<usize>, Box<PrefixSearchError>>
where
    F: Fn(&RecoveryModel) -> bool,
{
    for len in 1..=ops.len() {
        let prefix = &ops[..len];
        match execute_trace(seed, prefix, log_id) {
            Ok(model) => {
                if predicate(&model) {
                    return Ok(Some(len));
                }
            }
            Err(e) => {
                return Err(Box::new(PrefixSearchError {
                    seed: e.seed,
                    op_index: e.op_index,
                    error: e.error,
                    prefix: e.prefix,
                }));
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn lid() -> LogId {
        LogId(NonZeroU128::MIN)
    }

    fn lid2() -> Result<LogId, ModelError> {
        LogId::new(2).ok_or(ModelError::InvalidRecordShape {
            reason: "test log identity is zero",
        })
    }

    fn default_bounds() -> TraceBounds {
        TraceBounds {
            max_ops: 200,
            max_txns: 50,
            max_pages: 10,
            max_post_checkpoint_records: 100,
            max_crash_cycles: 10,
        }
    }

    // Helper: run a full cycle and return model in CompleteLive phase
    fn full_cycle(m: &mut RecoveryModel) -> Result<(), ModelError> {
        m.allocate_coordinator_epoch()?;
        let txn = m.begin_transaction()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_transaction_page(txn, pid, 100, PageVersion::new(1))?;
        m.append_transaction_commit(txn)?;
        let pid2 = PageId::new(2).ok_or(ModelError::InvalidPageId)?;
        m.append_raw_page(pid2, 200, PageVersion::new(1))?;
        m.flush_wal()?;
        // Write page store for txn page and raw page
        let records: Vec<WalPosition> = m
            .wal_records()
            .iter()
            .filter(|record| record.page.is_some())
            .map(|record| record.position)
            .collect();
        for &pos in &records {
            m.write_page_store(pos)?;
        }
        let anchor = CheckpointAnchor::new(1, 0xdead);
        m.publish_checkpoint(anchor)?;
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        m.restore_transactions()?;
        m.complete()?;
        Ok(())
    }

    #[test]
    fn test_full_cycle() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        assert_eq!(m.phase(), RecoveryPhase::CompleteLive);
        assert!(m.completion_evidence().is_some());
        Ok(())
    }

    #[test]
    fn test_retention_and_reclaim() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        m.analyze_retention()?;
        assert_eq!(m.phase(), RecoveryPhase::RetentionAnalyzed);
        let generation = m.reclaim()?;
        assert_eq!(generation.get(), 1);
        assert_eq!(m.phase(), RecoveryPhase::Reclaimed);
        Ok(())
    }

    #[test]
    fn test_repeated_reclamation() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        m.analyze_retention()?;
        let gen1 = m.reclaim()?;
        assert_eq!(gen1.get(), 1);

        // Crash + full recovery + reclaim again
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        m.restore_transactions()?;
        m.complete()?;

        m.analyze_retention()?;
        let gen2 = m.reclaim()?;
        assert_eq!(gen2.get(), 2);
        Ok(())
    }

    #[test]
    fn test_crash_truncates_volatile() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let txn = m.begin_transaction()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_transaction_page(txn, pid, 42, PageVersion::new(1))?;
        // Don't flush - record is volatile
        assert_eq!(m.wal_records().len(), 1);
        m.crash()?;
        assert!(m.wal_records().is_empty());
        Ok(())
    }

    #[test]
    fn test_crash_discards_tail_without_reusing_its_position() -> Result<(), ModelError> {
        let mut model = RecoveryModel::new(lid());
        model.allocate_coordinator_epoch()?;
        let page = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        assert_eq!(
            model.append_raw_page(page, 1, PageVersion::new(1))?.get(),
            1
        );
        model.crash()?;
        model.reopen()?;
        model.select()?;
        assert_eq!(model.plan_replay()?, 0);
        model.repair_pages()?;
        model.restore_transactions()?;
        model.complete()?;
        assert_eq!(
            model.append_raw_page(page, 2, PageVersion::new(2))?.get(),
            2
        );
        model.flush_wal()?;
        assert_eq!(
            model
                .durable_wal_records()
                .map(|record| record.position.get())
                .collect::<Vec<_>>(),
            vec![2]
        );
        Ok(())
    }

    #[test]
    fn test_complete_unflushed_tail_is_not_replayed_until_later_flush() -> Result<(), ModelError> {
        let mut model = RecoveryModel::new(lid());
        model.allocate_coordinator_epoch()?;
        let page = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        assert_eq!(
            model.append_raw_page(page, 1, PageVersion::new(1))?.get(),
            1
        );
        model.crash_preserving_complete_wal_tail()?;
        model.reopen()?;
        assert_eq!(model.wal_records().len(), 1);
        assert_eq!(model.durable_wal_records().count(), 0);
        model.select()?;
        assert_eq!(model.plan_replay()?, 0);
        model.repair_pages()?;
        model.restore_transactions()?;
        model.complete()?;
        assert_eq!(
            model.append_raw_page(page, 2, PageVersion::new(2))?.get(),
            2
        );
        model.flush_wal()?;
        assert_eq!(
            model
                .durable_wal_records()
                .map(|record| record.position.get())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        Ok(())
    }

    #[test]
    fn test_crash_from_each_recovery_phase_converges() -> Result<(), ModelError> {
        let page = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        let mut base = RecoveryModel::new(lid());
        base.allocate_coordinator_epoch()?;
        base.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        base.append_raw_page(page, 42, PageVersion::new(1))?;
        base.flush_wal()?;
        base.crash()?;
        base.reopen()?;

        for target in [
            RecoveryPhase::Unrecovered,
            RecoveryPhase::Selected,
            RecoveryPhase::ReplayPlanned,
            RecoveryPhase::PagesRepaired,
            RecoveryPhase::TransactionsRestored,
        ] {
            let mut model = base.clone();
            match target {
                RecoveryPhase::Unrecovered => {}
                RecoveryPhase::Selected => {
                    model.select()?;
                }
                RecoveryPhase::ReplayPlanned => {
                    model.select()?;
                    model.plan_replay()?;
                }
                RecoveryPhase::PagesRepaired => {
                    model.select()?;
                    model.plan_replay()?;
                    model.repair_pages()?;
                }
                RecoveryPhase::TransactionsRestored => {
                    model.select()?;
                    model.plan_replay()?;
                    model.repair_pages()?;
                    model.restore_transactions()?;
                }
                _ => {
                    return Err(ModelError::InvalidPhase {
                        current: target,
                        operation: "recovery crash test target",
                    });
                }
            }
            assert_eq!(model.phase(), target);
            let repaired_before_crash = model.page_store().contains_key(&page);
            let epoch_before_crash = model.epoch_high_water();

            model.crash()?;
            assert_eq!(model.phase(), RecoveryPhase::Crashed);
            assert_eq!(
                model.page_store().contains_key(&page),
                repaired_before_crash
            );
            model.reopen()?;
            model.select()?;
            model.plan_replay()?;
            model.repair_pages()?;
            let restoration = model.restore_transactions()?;
            if target == RecoveryPhase::TransactionsRestored {
                assert!(
                    epoch_before_crash
                        .is_some_and(|high_water| restoration.fresh_epoch > high_water)
                );
            }
            model.complete()?;
            assert_eq!(model.phase(), RecoveryPhase::CompleteLive);
            assert_eq!(
                model.page_store().get(&page).map(|snapshot| snapshot.value),
                Some(42)
            );
        }
        Ok(())
    }

    #[test]
    fn test_reclamation_preserves_high_water() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        let hw_before = m.logical_high_water();
        let ehw_before = m.epoch_high_water();
        m.analyze_retention()?;
        m.reclaim()?;
        // High-water marks preserved after reclamation
        assert_eq!(m.logical_high_water(), hw_before);
        assert_eq!(m.epoch_high_water(), ehw_before);
        Ok(())
    }

    #[test]
    fn test_generation_overflow() {
        let mut m = RecoveryModel::new(lid());
        m.wal_generation = Generation::from_raw(u64::MAX);
        m.phase = RecoveryPhase::RetentionAnalyzed;
        m.retention_analysis = Some(RetentionAnalysis {
            retained_first: None,
            checkpoint_anchor: CheckpointAnchor::new(1, 1),
            checkpoint_log_id: lid(),
        });
        m.checkpoint_slot = Some(CheckpointSnapshot {
            log_id: lid(),
            lineage_id: m.lineage_id(),
            anchor: CheckpointAnchor::new(1, 1),
            frontier: CheckpointFrontier::empty(),
            replay_start: None,
            pages: BTreeMap::new(),
            transaction_summary: TransactionSummary {
                active: BTreeMap::new(),
                epoch_high_water: None,
            },
        });
        let obs_before = Observation::from_model(&m);
        let err = m.reclaim();
        assert_eq!(err, Err(ModelError::GenerationExhausted));
        // Model unchanged after failed reclaim (Point 13)
        let obs_after = Observation::from_model(&m);
        assert_eq!(obs_before, obs_after);
    }

    #[test]
    fn test_position_max_once() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.next_position = Some(u64::MAX);
        m.allocate_coordinator_epoch()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        // MAX assigns once
        let pos = m.append_raw_page(pid, 1, PageVersion::new(1))?;
        assert_eq!(pos.get(), u64::MAX);
        // Next allocation fails
        let err = m.append_raw_page(pid, 2, PageVersion::new(2));
        assert_eq!(err, Err(ModelError::PositionExhausted));
        Ok(())
    }

    #[test]
    fn test_epoch_max_once() {
        let mut m = RecoveryModel::new(lid());
        m.next_epoch = Some(u64::MAX);
        let e = m.allocate_coordinator_epoch();
        assert!(e.is_ok());
        assert_eq!(e.ok(), Some(u64::MAX));
        let e2 = m.allocate_coordinator_epoch();
        assert_eq!(e2, Err(ModelError::EpochExhausted));
    }

    #[test]
    fn test_epoch_hw_advances_on_allocation() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        assert_eq!(m.epoch_high_water(), None);
        m.allocate_coordinator_epoch()?;
        assert_eq!(m.epoch_high_water(), Some(1));
        m.allocate_coordinator_epoch()?;
        assert_eq!(m.epoch_high_water(), Some(2));
        Ok(())
    }

    #[test]
    fn test_stale_completion_rejects_retention() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        // Live mutation marks completion stale
        let pid = PageId::new(3).ok_or(ModelError::InvalidPageId)?;
        m.append_raw_page(pid, 999, PageVersion::new(10))?;
        let err = m.analyze_retention();
        assert!(matches!(
            err,
            Err(ModelError::StaleCompletionEvidence { .. })
        ));
        Ok(())
    }

    #[test]
    fn test_publish_after_completion_stales_evidence() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        assert!(!m.completion_evidence().is_none_or(|e| e.stale));
        let anchor = CheckpointAnchor::new(2, 0xbeef);
        m.publish_checkpoint(anchor)?;
        assert!(m.completion_evidence().is_some_and(|e| e.stale));
        let err = m.analyze_retention();
        assert!(matches!(
            err,
            Err(ModelError::StaleCompletionEvidence { .. })
        ));
        Ok(())
    }

    #[test]
    fn test_reopen_requires_crashed() {
        let mut m = RecoveryModel::new(lid());
        let err = m.reopen();
        assert!(matches!(
            err,
            Err(ModelError::InvalidPhase {
                current: RecoveryPhase::Live,
                ..
            })
        ));
    }

    #[test]
    fn test_foreign_log_checkpoint_rejected() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_raw_page(pid, 1, PageVersion::new(1))?;
        m.flush_wal()?;
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        // Tamper: change checkpoint log_id
        if let Some(ref mut cp) = m.checkpoint_slot {
            cp.log_id = lid2()?;
        }
        m.crash()?;
        let error = m.reopen();
        assert!(matches!(error, Err(ModelError::ForeignLogId { .. })));
        Ok(())
    }

    #[test]
    fn test_nonzero_missing_anchor() {
        let mut m = RecoveryModel::new(lid());
        m.wal_generation = Generation::from_raw(1);
        m.generation_anchor = None;
        m.phase = RecoveryPhase::Unrecovered;
        let err = m.select();
        assert!(matches!(
            err,
            Err(ModelError::MissingGenerationAnchor { .. })
        ));
    }

    #[test]
    fn test_nonzero_anchor_mismatch() {
        let mut m = RecoveryModel::new(lid());
        m.wal_generation = Generation::from_raw(1);
        m.wal_format_version = GENERATION_FORMAT_VERSION;
        m.generation_anchor = Some(CheckpointAnchor::new(1, 100));
        m.checkpoint_slot = Some(CheckpointSnapshot {
            log_id: lid(),
            lineage_id: m.lineage_id(),
            anchor: CheckpointAnchor::new(1, 200), // different
            frontier: CheckpointFrontier::empty(),
            replay_start: None,
            pages: BTreeMap::new(),
            transaction_summary: TransactionSummary {
                active: BTreeMap::new(),
                epoch_high_water: None,
            },
        });
        m.phase = RecoveryPhase::Unrecovered;
        let err = m.select();
        assert!(matches!(
            err,
            Err(ModelError::CheckpointAnchorMismatch { .. })
        ));
    }

    #[test]
    fn test_minimal_full_metadata_mismatch() {
        let mut m = RecoveryModel::new(lid());
        m.wal_generation = Generation::from_raw(1);
        m.wal_format_version = GENERATION_FORMAT_VERSION;
        m.generation_anchor = Some(CheckpointAnchor::new(1, 5));
        m.phase = RecoveryPhase::Unrecovered;
        let minimal = m.minimal_metadata();
        let full = FullMetadata {
            minimal: MinimalMetadata {
                log_id: lid(),
                generation: Generation::from_raw(2),
            },
            format_version: GENERATION_FORMAT_VERSION,
            retained_first: None,
            logical_high_water: None,
            epoch_high_water: None,
            required_anchor: CheckpointAnchor::new(1, 5),
        };
        let err = m.select_observed(&minimal, Some(&full));
        assert!(matches!(err, Err(ModelError::MetadataMismatch { .. })));
    }

    #[test]
    fn test_replacement_stages() {
        assert_eq!(REPLACEMENT_STAGES.len(), 10);
        for (i, s) in REPLACEMENT_STAGES.iter().enumerate() {
            assert_eq!(s.index(), i);
        }
        assert!(ReplacementStage::BeforeCleanup.is_before_rename());
        assert!(!ReplacementStage::AfterCurrentReplace.is_before_rename());
    }

    #[test]
    fn test_checkpoint_replacement_stages() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        let cp = CheckpointSnapshot {
            log_id: lid(),
            lineage_id: m.lineage_id(),
            anchor: CheckpointAnchor::new(1, 42),
            frontier: CheckpointFrontier::empty(),
            replay_start: None,
            pages: BTreeMap::new(),
            transaction_summary: TransactionSummary {
                active: BTreeMap::new(),
                epoch_high_water: None,
            },
        };
        m.begin_checkpoint_candidate(cp)?;
        // Advance through all stages
        let mut stages = Vec::new();
        loop {
            match m.advance_checkpoint_candidate() {
                Ok(s) => {
                    stages.push(s);
                    if matches!(m.checkpoint_candidate(), CheckpointCandidateState::Absent) {
                        break;
                    }
                }
                Err(ModelError::NoPendingCandidate) => break,
                Err(e) => return Err(e),
            }
        }
        assert_eq!(stages.as_slice(), &REPLACEMENT_STAGES[1..]);
        Ok(())
    }

    #[test]
    fn test_before_after_repair_snapshots() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let txn = m.begin_transaction()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_transaction_page(txn, pid, 42, PageVersion::new(1))?;
        m.append_transaction_commit(txn)?;
        m.flush_wal()?;
        // Don't write page store - page missing
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        let before = m.page_store().len();
        let applied = m.repair_pages_applied_fault();
        match applied {
            Applied::AppliedThenError { value, .. } => {
                // Pages were installed, then error reported
                assert!(value > 0 || m.page_store().len() >= before);
            }
            _ => {
                return Err(ModelError::InvalidPhase {
                    current: m.phase(),
                    operation: "expected AppliedThenError",
                });
            }
        }
        Ok(())
    }

    #[test]
    fn test_restore_seeds_from_checkpoint() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let txn = m.begin_transaction()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_transaction_page(txn, pid, 42, PageVersion::new(1))?;
        // Don't commit - transaction is active
        m.flush_wal()?;
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        let summary = m.restore_transactions()?;
        // Fresh epoch must be > persisted high-water
        assert!(summary.fresh_epoch > 0);
        // Active transaction from checkpoint summary should be present
        // (it was active, uncommitted at checkpoint time)
        assert!(!summary.active_transactions.is_empty());
        Ok(())
    }

    #[test]
    fn test_restore_fresh_epoch_above_hw() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        // epoch_high_water should be at least 1
        let hw = m.epoch_high_water();
        assert!(hw.is_some());
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        let summary = m.restore_transactions()?;
        let retained_high_water = hw.ok_or(ModelError::NoActiveEpoch)?;
        assert!(summary.fresh_epoch > retained_high_water);
        Ok(())
    }

    #[test]
    fn test_empty_checkpoint_commit_only_suffix_can_be_reclaimed() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        m.allocate_coordinator_epoch()?;
        let transaction = m.begin_transaction()?;
        m.append_transaction_commit(transaction)?;
        m.flush_wal()?;
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        m.restore_transactions()?;
        m.complete()?;
        m.analyze_retention()?;
        let generation = m.reclaim()?;
        assert_eq!(generation.get(), 1);
        assert_eq!(m.retained_first(), None);
        assert!(m.wal_records().is_empty());
        assert_eq!(
            m.replacement_logical_high_water().map(WalPosition::get),
            Some(1)
        );
        assert_eq!(m.replacement_epoch_high_water(), Some(2));

        m.crash()?;
        m.reopen()?;
        m.select()?;
        assert_eq!(m.plan_replay()?, 0);
        m.repair_pages()?;
        let restoration = m.restore_transactions()?;
        assert_eq!(restoration.fresh_epoch, 3);
        m.complete()?;
        assert_eq!(m.flush_wal()?.map(WalPosition::get), Some(1));
        assert_eq!(m.logical_high_water().map(WalPosition::get), Some(1));
        let appended = m.append_raw_page(
            PageId::new(1).ok_or(ModelError::InvalidPageId)?,
            42,
            PageVersion::new(1),
        )?;
        assert_eq!(appended.get(), 2);
        m.flush_wal()?;

        let current = m
            .observe_full_metadata()
            .ok_or(ModelError::MissingGenerationAnchor {
                generation: m.wal_generation(),
            })?;
        assert_eq!(current.retained_first, Some(appended));
        assert_eq!(current.logical_high_water, Some(appended));
        assert_eq!(current.epoch_high_water, Some(3));
        assert_eq!(m.retained_first(), None);
        assert_eq!(
            m.replacement_logical_high_water().map(WalPosition::get),
            Some(1)
        );
        assert_eq!(m.replacement_epoch_high_water(), Some(2));

        m.crash()?;
        m.reopen()?;
        m.select()?;
        assert_eq!(m.plan_replay()?, 1);
        Ok(())
    }

    #[test]
    fn test_empty_checkpoint_gen_zero() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        // Publish empty checkpoint (no WAL records)
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        assert!(m.epoch_high_water().is_none());
        assert!(m.observe_full_metadata().is_none());
        m.crash()?;
        m.reopen()?;
        // Generation zero with empty checkpoint
        m.select()?;
        assert_eq!(m.phase(), RecoveryPhase::Selected);
        assert!(m.epoch_high_water().is_none());
        Ok(())
    }

    #[test]
    fn test_absent_checkpoint_gen_zero_before_epoch_allocation() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        assert!(m.checkpoint_slot().is_none());
        assert!(m.epoch_high_water().is_none());
        assert!(m.observe_full_metadata().is_none());
        m.crash()?;
        m.reopen()?;
        m.select()?;
        assert_eq!(m.phase(), RecoveryPhase::Selected);
        assert!(m.selected_checkpoint().is_none());
        assert_eq!(m.plan_replay()?, 0);
        Ok(())
    }

    #[test]
    fn test_candidate_entry_types() {
        assert!(CandidateEntry::Valid.is_cleanable());
        assert!(CandidateEntry::ValidHigher.is_cleanable());
        assert!(CandidateEntry::Corrupt.is_cleanable());
        assert!(CandidateEntry::PartialWrite.is_cleanable());
        assert!(CandidateEntry::DanglingSymlink.is_cleanable());
        assert!(!CandidateEntry::InodeAlias.is_cleanable());
        assert!(!CandidateEntry::Absent.is_cleanable());
    }

    #[test]
    fn test_wal_candidate_lifecycle() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        let anchor = CheckpointAnchor::new(1, 42);
        m.phase = RecoveryPhase::RetentionAnalyzed;
        m.begin_wal_candidate(WalReplacement {
            target_generation: Generation::from_raw(1),
            anchor,
            retained_suffix: Vec::new(),
            retained_first: None,
            format_version: GENERATION_FORMAT_VERSION,
            logical_high_water: None,
            epoch_high_water: None,
        })?;
        // During replacement both old+new inode locks held
        assert_eq!(m.locks().wal_old_inode, LockOwnership::Live);
        assert_eq!(m.locks().wal_new_inode, LockOwnership::Live);

        // Rename changes the selected generation but keeps both inode handles
        // owned until directory synchronization finishes.
        loop {
            assert_eq!(m.locks().wal_old_inode, LockOwnership::Live);
            assert_eq!(m.locks().wal_new_inode, LockOwnership::Live);
            let stage = m.advance_wal_candidate()?;
            if stage == ReplacementStage::AfterCurrentReplace {
                assert!(matches!(
                    m.wal_candidate(),
                    WalCandidateState::Present {
                        entry: CandidateEntry::Absent,
                        ..
                    }
                ));
                assert_eq!(m.wal_generation().get(), 1);
            }
            if stage == ReplacementStage::AfterDirectorySync {
                break;
            }
        }
        // After directory synchronization: the attempt and inode overlap end.
        assert!(matches!(m.wal_candidate(), WalCandidateState::Absent));
        assert_eq!(m.wal_generation().get(), 1);
        assert_eq!(m.wal_format_version(), GENERATION_FORMAT_VERSION);
        assert_eq!(m.locks().wal, LockOwnership::Live);
        assert_eq!(m.locks().wal_old_inode, LockOwnership::Free);
        assert_eq!(m.locks().wal_new_inode, LockOwnership::Free);
        Ok(())
    }

    #[test]
    fn test_locks_crash_frees_all() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        assert_eq!(m.locks().wal, LockOwnership::Live);
        m.crash()?;
        assert_eq!(m.locks().wal, LockOwnership::Free);
        assert_eq!(m.locks().page_store, LockOwnership::Free);
        assert_eq!(m.locks().checkpoint, LockOwnership::Free);
        assert_eq!(m.locks().wal_old_inode, LockOwnership::Free);
        assert_eq!(m.locks().wal_new_inode, LockOwnership::Free);
        Ok(())
    }

    #[test]
    fn test_locks_reopen_recovery() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.crash()?;
        m.reopen()?;
        assert_eq!(m.locks().wal, LockOwnership::Recovery);
        assert_eq!(m.locks().page_store, LockOwnership::Recovery);
        Ok(())
    }

    #[test]
    fn test_write_page_store_invalid_shapes() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let txn = m.begin_transaction()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_transaction_page(txn, pid, 42, PageVersion::new(1))?;
        m.flush_wal()?;
        // Transaction page without commit should fail
        let pos = m.wal_records()[0].position;
        let err = m.write_page_store(pos);
        assert!(matches!(err, Err(ModelError::PageUncommitted { .. })));
        Ok(())
    }

    #[test]
    fn test_write_page_store_requires_durable_commit() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let transaction = m.begin_transaction()?;
        let page = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        let page_position =
            m.append_transaction_page(transaction, page, 42, PageVersion::new(1))?;
        m.flush_wal()?;
        m.append_transaction_commit(transaction)?;
        assert!(matches!(
            m.write_page_store(page_position),
            Err(ModelError::PageUncommitted { .. })
        ));
        Ok(())
    }

    #[test]
    fn test_page_store_write_stales_completion_and_rejects_regression() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        let latest = m
            .wal_records()
            .iter()
            .rfind(|record| record.page.is_some())
            .map(|record| record.position)
            .ok_or(ModelError::NoFlushPosition)?;
        m.write_page_store(latest)?;
        assert!(
            m.completion_evidence()
                .is_some_and(|evidence| evidence.stale)
        );

        let mut regression = RecoveryModel::new(lid());
        regression.allocate_coordinator_epoch()?;
        let page = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        let old = regression.append_raw_page(page, 1, PageVersion::new(1))?;
        regression.flush_wal()?;
        regression.write_page_store(old)?;
        let new = regression.append_raw_page(page, 2, PageVersion::new(2))?;
        regression.flush_wal()?;
        regression.write_page_store(new)?;
        assert!(matches!(
            regression.write_page_store(old),
            Err(ModelError::PageStoreRegression { .. })
        ));
        assert_eq!(
            regression
                .page_store()
                .get(&page)
                .map(|snapshot| snapshot.written_at),
            Some(new)
        );
        Ok(())
    }

    #[test]
    fn test_repair_skips_uncommitted_page_and_restores_observation() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let transaction = m.begin_transaction()?;
        let page = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_transaction_page(transaction, page, 42, PageVersion::new(1))?;
        m.flush_wal()?;
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        assert!(!m.page_store().contains_key(&page));
        let summary = m.restore_transactions()?;
        assert!(summary.active_transactions.contains_key(&transaction));
        assert!(matches!(
            m.append_transaction_page(transaction, page, 43, PageVersion::new(2)),
            Err(ModelError::InvalidPhase { .. })
        ));
        m.complete()?;
        assert!(matches!(
            m.append_transaction_page(transaction, page, 43, PageVersion::new(2)),
            Err(ModelError::TransactionNotFound { .. })
        ));
        Ok(())
    }

    #[test]
    fn test_checkpoint_candidate_matrix_never_promotes() -> Result<(), ModelError> {
        for entry in [
            CandidateEntry::Valid,
            CandidateEntry::ValidHigher,
            CandidateEntry::Corrupt,
            CandidateEntry::PartialWrite,
            CandidateEntry::DanglingSymlink,
        ] {
            let mut m = RecoveryModel::new(lid());
            full_cycle(&mut m)?;
            let selected = m
                .checkpoint_slot()
                .cloned()
                .ok_or(ModelError::NoCheckpointForReclamation)?;
            let mut candidate = selected.clone();
            candidate.anchor = CheckpointAnchor::new(9, 9);
            m.begin_checkpoint_candidate(candidate)?;
            m.set_checkpoint_candidate_entry(entry)?;
            m.crash()?;
            m.reopen()?;
            assert!(matches!(
                m.checkpoint_candidate(),
                CheckpointCandidateState::Absent
            ));
            assert_eq!(
                m.checkpoint_slot().map(|checkpoint| checkpoint.anchor),
                Some(selected.anchor)
            );
        }
        Ok(())
    }

    #[test]
    fn test_wal_candidate_matrix_never_promotes() -> Result<(), ModelError> {
        for entry in [
            CandidateEntry::Valid,
            CandidateEntry::ValidHigher,
            CandidateEntry::Corrupt,
            CandidateEntry::PartialWrite,
            CandidateEntry::DanglingSymlink,
        ] {
            let mut m = RecoveryModel::new(lid());
            full_cycle(&mut m)?;
            m.analyze_retention()?;
            m.begin_wal_candidate(WalReplacement {
                target_generation: Generation::from_raw(1),
                anchor: CheckpointAnchor::new(1, 0xdead),
                retained_suffix: Vec::new(),
                retained_first: None,
                format_version: GENERATION_FORMAT_VERSION,
                logical_high_water: m.logical_high_water(),
                epoch_high_water: m.epoch_high_water(),
            })?;
            m.set_wal_candidate_entry(entry)?;
            m.crash()?;
            m.reopen()?;
            assert_eq!(m.wal_generation(), Generation::ZERO);
            assert!(matches!(m.wal_candidate(), WalCandidateState::Absent));
        }
        Ok(())
    }

    #[test]
    fn test_inode_alias_rejected_and_preserved() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        let candidate = m
            .checkpoint_slot()
            .cloned()
            .ok_or(ModelError::NoCheckpointForReclamation)?;
        m.begin_checkpoint_candidate(candidate)?;
        m.set_checkpoint_candidate_entry(CandidateEntry::InodeAlias)?;
        m.crash()?;
        assert!(matches!(
            m.reopen(),
            Err(ModelError::InodeAliasCandidate { .. })
        ));
        assert!(matches!(
            m.checkpoint_candidate(),
            CheckpointCandidateState::Present { .. }
        ));
        Ok(())
    }

    #[test]
    fn test_invalid_selected_rejected_before_candidate_cleanup() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        let candidate = m
            .checkpoint_slot()
            .cloned()
            .ok_or(ModelError::NoCheckpointForReclamation)?;
        m.begin_checkpoint_candidate(candidate)?;
        m.crash()?;
        m.set_selected_entries_for_open(SelectedEntryState::Valid, SelectedEntryState::Corrupt)?;
        assert!(matches!(
            m.reopen(),
            Err(ModelError::InvalidSelectedOnOpen { .. })
        ));
        assert!(matches!(
            m.checkpoint_candidate(),
            CheckpointCandidateState::Present { .. }
        ));
        Ok(())
    }

    #[test]
    fn test_invalid_selected_wal_rejected_before_candidate_cleanup() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        let candidate = m
            .checkpoint_slot()
            .cloned()
            .ok_or(ModelError::NoCheckpointForReclamation)?;
        m.begin_checkpoint_candidate(candidate)?;
        m.crash()?;
        m.set_selected_entries_for_open(SelectedEntryState::Corrupt, SelectedEntryState::Valid)?;
        assert!(matches!(
            m.reopen(),
            Err(ModelError::InvalidSelectedOnOpen { .. })
        ));
        assert!(matches!(
            m.checkpoint_candidate(),
            CheckpointCandidateState::Present { .. }
        ));
        Ok(())
    }

    #[test]
    fn test_checkpoint_only_active_transaction_survives_pruning() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let transaction = m.begin_transaction()?;
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        let first = m.restore_transactions()?;
        assert!(first.active_transactions.contains_key(&transaction));
        m.complete()?;
        m.analyze_retention()?;
        m.reclaim()?;
        assert!(m.wal_records().is_empty());
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        let second = m.restore_transactions()?;
        assert!(second.active_transactions.contains_key(&transaction));
        Ok(())
    }

    #[test]
    fn test_observation_comparison() {
        let mut m1 = RecoveryModel::new(lid());
        let m2 = RecoveryModel::new(lid());
        let o1 = Observation::from_model(&m1);
        let o2 = Observation::from_model(&m2);
        let c = compare_observations(&o1, &o2);
        assert!(c.is_empty());

        // Create a difference
        assert_eq!(m1.allocate_coordinator_epoch(), Ok(1));
        let o1b = Observation::from_model(&m1);
        let c2 = compare_observations(&o1b, &o2);
        assert!(!c2.is_empty()); // epoch_high_water differs
    }

    #[test]
    fn test_observation_foreign_log() -> Result<(), ModelError> {
        let m1 = RecoveryModel::new(lid());
        let m2 = RecoveryModel::new(lid2()?);
        let o1 = Observation::from_model(&m1);
        let o2 = Observation::from_model(&m2);
        let c = compare_observations(&o1, &o2);
        assert!(c.iter().any(|c| c.field == "log_id"));
        // Position-based checks skipped for foreign log
        assert!(!c.iter().any(|c| c.field == "logical_high_water"));
        Ok(())
    }

    #[test]
    fn test_untrusted_duplicate_transaction_table() -> Result<(), ModelError> {
        let tid = TransactionId::new(1, 1).ok_or(ModelError::NoActiveEpoch)?;
        let entries = vec![
            UntrustedTransactionEntry { id: tid, data: 1 },
            UntrustedTransactionEntry { id: tid, data: 2 }, // duplicate
        ];
        let dup = find_duplicate_in_untrusted_transaction_table(&entries);
        assert_eq!(dup, Some(tid));

        // No duplicates
        let tid2 = TransactionId::new(1, 2).ok_or(ModelError::NoActiveEpoch)?;
        let entries2 = vec![
            UntrustedTransactionEntry { id: tid, data: 1 },
            UntrustedTransactionEntry { id: tid2, data: 2 },
        ];
        assert_eq!(
            find_duplicate_in_untrusted_transaction_table(&entries2),
            None
        );
        Ok(())
    }

    #[test]
    fn test_plan_replay_max_frontier() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        // Set up a model with MAX position frontier
        m.next_position = Some(u64::MAX);
        m.allocate_coordinator_epoch()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_raw_page(pid, 1, PageVersion::new(1))?;
        m.flush_wal()?;
        // Write page to store so checkpoint has it
        let pos = m.wal_records()[0].position;
        m.write_page_store(pos)?;
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        m.crash()?;
        m.reopen()?;
        m.select()?;
        // plan_replay with MAX frontier should not overflow (Point 11)
        let count = m.plan_replay()?;
        // Page is in checkpoint store, no replay needed, no overflow
        assert_eq!(count, 0);
        Ok(())
    }

    #[test]
    fn test_hard_bounds_rejection() {
        let bounds = TraceBounds {
            max_ops: HARD_MAX_OPS + 1,
            max_txns: 10,
            max_pages: 10,
            max_post_checkpoint_records: 10,
            max_crash_cycles: 10,
        };
        assert!(matches!(
            bounds.validate(),
            Err(ModelError::BoundsExceeded { .. })
        ));
    }

    #[test]
    fn test_skeleton_minimum_rejection() {
        let bounds = TraceBounds {
            max_ops: 10, // too small for skeleton
            max_txns: 10,
            max_pages: 10,
            max_post_checkpoint_records: 10,
            max_crash_cycles: 10,
        };
        assert!(matches!(
            bounds.validate(),
            Err(ModelError::SkeletonRequiresMoreCapacity { .. })
        ));
    }

    #[test]
    fn test_skeleton_crash_cycles_minimum() {
        let bounds = TraceBounds {
            max_ops: 200,
            max_txns: 10,
            max_pages: 10,
            max_post_checkpoint_records: 10,
            max_crash_cycles: 0, // skeleton needs >= 3
        };
        assert!(matches!(
            bounds.validate(),
            Err(ModelError::SkeletonRequiresMoreCapacity { .. })
        ));
    }

    #[test]
    fn test_deterministic_same_seed() -> Result<(), ModelError> {
        let bounds = default_bounds();
        let t1 = generate_trace(42, &bounds)?;
        let t2 = generate_trace(42, &bounds)?;
        assert_eq!(t1.len(), t2.len());
        for (a, b) in t1.iter().zip(t2.iter()) {
            assert_eq!(format!("{a}"), format!("{b}"));
        }
        Ok(())
    }

    #[test]
    fn test_deterministic_different_seeds() -> Result<(), ModelError> {
        let bounds = default_bounds();
        let t1 = generate_trace(1, &bounds)?;
        let t2 = generate_trace(999, &bounds)?;
        // Different seeds may produce different traces
        // At minimum they should both be valid
        assert!(!t1.is_empty());
        assert!(!t2.is_empty());
        Ok(())
    }

    #[test]
    fn test_ci_seeds_execute() -> Result<(), Box<dyn Error>> {
        let bounds = default_bounds();
        for &seed in &CI_SEEDS {
            let ops = generate_trace(seed, &bounds)?;
            execute_trace(seed, &ops, lid())?;
        }
        Ok(())
    }

    #[test]
    fn test_ci_trace_coverage() -> Result<(), Box<dyn Error>> {
        let bounds = default_bounds();
        for &seed in &CI_SEEDS {
            let ops = generate_trace(seed, &bounds)?;
            let ops_strs: Vec<String> = ops.iter().map(|o| format!("{o}")).collect();
            // Must contain required operations
            assert!(ops_strs.iter().any(|s| s.contains("begin-txn")));
            assert!(ops_strs.iter().any(|s| s.contains("append-txn-page")));
            assert!(ops_strs.iter().any(|s| s.contains("append-commit")));
            assert!(ops_strs.iter().any(|s| s.contains("append-raw")));
            assert!(ops_strs.iter().any(|s| s.contains("flush")));
            assert!(ops_strs.iter().any(|s| s.contains("write-ps")));
            assert!(ops_strs.iter().any(|s| s.contains("publish-cp")));
            assert!(ops_strs.iter().any(|s| s.contains("crash")));
            assert!(ops_strs.iter().any(|s| s.contains("reopen")));
            assert!(ops_strs.iter().any(|s| s.contains("select")));
            assert!(ops_strs.iter().any(|s| s.contains("plan-replay")));
            assert!(ops_strs.iter().any(|s| s.contains("repair")));
            assert!(ops_strs.iter().any(|s| s.contains("restore-txns")));
            assert!(ops_strs.iter().any(|s| s.contains("complete")));
            assert!(ops_strs.iter().any(|s| s.contains("analyze-retention")));
            assert!(ops_strs.iter().any(|s| s.contains("reclaim")));
            // Repair fault
            assert!(ops_strs.iter().any(|s| s.contains("repair-fault")));
            // Stale evidence / live mutation
            assert!(ops_strs.iter().any(|s| s.contains("live-mutation")));
        }
        Ok(())
    }

    #[test]
    fn test_minimal_prefix_search() -> Result<(), Box<dyn Error>> {
        let bounds = default_bounds();
        let ops = generate_trace(42, &bounds)?;
        let result =
            find_minimal_prefix(42, &ops, lid(), |m| m.phase() == RecoveryPhase::Reclaimed)?;
        assert!(result.is_some());
        Ok(())
    }

    #[test]
    fn test_missing_prefix_page_replayed() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let txn = m.begin_transaction()?;
        let pid1 = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        let pid2 = PageId::new(2).ok_or(ModelError::InvalidPageId)?;
        m.append_transaction_page(txn, pid1, 10, PageVersion::new(1))?;
        m.append_transaction_commit(txn)?;
        m.append_raw_page(pid2, 20, PageVersion::new(1))?;
        m.flush_wal()?;
        // Write only page 1, not page 2
        let pos1 = m.wal_records()[0].position;
        m.write_page_store(pos1)?;
        // Publish checkpoint - page 2 is missing in store
        m.publish_checkpoint(CheckpointAnchor::new(1, 1))?;
        let cp = m.checkpoint_slot().cloned();
        assert!(cp.is_some());
        let cp = cp.ok_or(ModelError::NoFlushPosition)?;
        // replay_start should point to the position of page 2's record
        assert!(cp.replay_start.is_some());

        m.crash()?;
        m.reopen()?;
        m.select()?;
        let count = m.plan_replay()?;
        // Plan should include the missing page 2 record
        assert!(count > 0);
        m.repair_pages()?;
        // Page 2 should now be in the store
        assert!(m.page_store().contains_key(&pid2));
        Ok(())
    }

    #[test]
    fn test_volatile_suffix_rejected_before_reclaim() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        m.analyze_retention()?;
        let position = m
            .logical_high_water()
            .and_then(|high_water| high_water.get().checked_add(1))
            .and_then(WalPosition::new)
            .ok_or(ModelError::PositionExhausted)?;
        let page = PageId::new(5).ok_or(ModelError::InvalidPageId)?;
        m.wal_records.push(WalRecord {
            log_id: m.log_id(),
            lineage_id: m.lineage_id(),
            position,
            kind: WalRecordKind::RawPage,
            transaction: None,
            page: Some(page),
            page_value: Some(1),
            page_version: Some(PageVersion::new(1)),
        });
        let before = Observation::from_model(&m);
        let record_count = m.wal_records().len();
        assert_eq!(m.reclaim(), Err(ModelError::VolatileSuffixPresent));
        assert_eq!(Observation::from_model(&m), before);
        assert_eq!(m.wal_records().len(), record_count);
        Ok(())
    }

    #[test]
    fn test_lineage_rebranding() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        m.allocate_coordinator_epoch()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_raw_page(pid, 1, PageVersion::new(1))?;
        m.flush_wal()?;
        m.write_page_store(m.wal_records()[0].position)?;
        let orig_lineage = m.lineage_id();
        m.crash()?;
        m.reopen()?;
        let new_lineage = m.lineage_id();
        assert!(new_lineage.get() > orig_lineage.get());
        // Persisted records rebranded
        for r in m.wal_records() {
            assert_eq!(r.lineage_id, new_lineage);
        }
        for snap in m.page_store().values() {
            assert_eq!(snap.lineage_id, new_lineage);
        }
        Ok(())
    }

    #[test]
    fn test_local_seed_iter_bounds() {
        assert!(local_seed_iter(HARD_MAX_LOCAL_SEEDS).is_ok());
        assert!(local_seed_iter(HARD_MAX_LOCAL_SEEDS + 1).is_err());
    }

    #[test]
    fn test_logid_zero() {
        assert!(LogId::new(0).is_none());
        assert!(LogId::new(1).is_some());
    }

    #[test]
    fn test_inode_alias_not_cleanable() {
        let alias = CandidateEntry::InodeAlias;
        assert!(!alias.is_cleanable());
    }

    #[test]
    fn test_trace_execution_error_display() {
        let err = TraceExecutionError {
            seed: 42,
            op_index: 3,
            operation: "crash".into(),
            error: ModelError::PositionExhausted,
            prefix: vec!["op1".into(), "op2".into(), "op3".into(), "crash".into()],
        };
        let display = format!("{err}");
        assert!(display.contains("seed=42"));
        assert!(display.contains("op=3"));
        assert!(display.contains("prefix:"));
    }

    #[test]
    fn test_prefix_search_error_display() {
        let err = PrefixSearchError {
            seed: 1,
            op_index: 0,
            error: ModelError::PositionExhausted,
            prefix: vec!["op1".into()],
        };
        let display = format!("{err}");
        assert!(display.contains("seed=1"));
        assert!(display.contains("prefix:"));
    }

    #[test]
    fn test_live_initial_cannot_analyze() {
        let mut m = RecoveryModel::new(lid());
        let err = m.analyze_retention();
        assert!(matches!(
            err,
            Err(ModelError::InvalidPhase {
                current: RecoveryPhase::Live,
                ..
            })
        ));
    }

    #[test]
    fn test_reclaimed_cannot_analyze() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        full_cycle(&mut m)?;
        m.analyze_retention()?;
        m.reclaim()?;
        let err = m.analyze_retention();
        assert!(matches!(
            err,
            Err(ModelError::InvalidPhase {
                current: RecoveryPhase::Reclaimed,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn test_old_checkpoint_post_frontier_fresh_recovery() -> Result<(), ModelError> {
        let mut m = RecoveryModel::new(lid());
        // Create initial state with checkpoint
        m.allocate_coordinator_epoch()?;
        let txn = m.begin_transaction()?;
        let pid = PageId::new(1).ok_or(ModelError::InvalidPageId)?;
        m.append_transaction_page(txn, pid, 1, PageVersion::new(1))?;
        m.append_transaction_commit(txn)?;
        m.flush_wal()?;
        let pos0 = m.wal_records()[0].position;
        m.write_page_store(pos0)?;
        let anchor1 = CheckpointAnchor::new(1, 0x100);
        m.publish_checkpoint(anchor1)?;

        // Add post-frontier work
        let pid2 = PageId::new(2).ok_or(ModelError::InvalidPageId)?;
        m.append_raw_page(pid2, 99, PageVersion::new(1))?;
        m.flush_wal()?;

        // Crash + recovery against old checkpoint
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        m.restore_transactions()?;
        m.complete()?;

        // Retention should succeed without republishing
        m.analyze_retention()?;
        m.reclaim()?;

        // Repeated cycle
        m.crash()?;
        m.reopen()?;
        m.select()?;
        m.plan_replay()?;
        m.repair_pages()?;
        m.restore_transactions()?;
        m.complete()?;
        m.analyze_retention()?;
        m.reclaim()?;
        assert_eq!(m.wal_generation().get(), 2);
        Ok(())
    }

    #[test]
    #[ignore]
    fn longer_local_profile() -> Result<(), Box<dyn Error>> {
        let bounds = TraceBounds {
            max_ops: 500,
            max_txns: 100,
            max_pages: 50,
            max_post_checkpoint_records: 200,
            max_crash_cycles: 20,
        };
        let seeds = local_seed_iter(1000)?;
        let mut count = 0u64;
        for seed in seeds {
            let ops = generate_trace(seed, &bounds)?;
            execute_trace(seed, &ops, lid())?;
            count = count.checked_add(1).ok_or("count overflow")?;
        }
        assert!(count > 0);
        Ok(())
    }
}
