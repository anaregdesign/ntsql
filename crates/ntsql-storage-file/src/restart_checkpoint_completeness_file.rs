use std::{
    error::Error,
    fmt,
    fs::File,
    io,
    path::{Path, PathBuf},
};

use ntsql_database::{DatabaseFileHeaderIdentity, DatabaseStorageIdentity};
use ntsql_transaction::{
    DurableTransactionRestartCheckpointCompletenessBaseline,
    DurableTransactionRestartCheckpointCompletenessBaselinePublicationPermit,
    DurableTransactionRestartCheckpointCompletenessBaselinePublisher,
    DurableTransactionRestartCheckpointCompletenessBaselineSource,
    OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation,
    TransactionPageStorageRestartCheckpointCompletenessSelection,
    UnrecoveredTransactionPageStorage,
};
use ntsql_wal::PersistentLogId;

use super::{
    FileCommitLog, FileOpenError, FilePageStore, PageStoreOpenError,
    RestartCheckpointCompletenessBaselineDecodeError,
    RestartCheckpointCompletenessBaselineEncodeError,
    decode_restart_checkpoint_completeness_baseline,
    encode_restart_checkpoint_completeness_baseline,
    restart_checkpoint_file::{
        FileRestartCheckpointSlotCreateError, FileRestartCheckpointSlotIoError,
        FileRestartCheckpointSlotOpenError, SlotCurrentReadError, SlotPublicationError,
        SlotPublicationStep, create_locked_control_slot, create_locked_database_control_slot,
        open_locked_control_slot, publish_slot_current_bytes, read_current_slot_bytes,
    },
};

const COMPLETENESS_CONTROL_MAGIC: [u8; 8] = *b"NTSQCMS1";

/// Failure to load one optional current filesystem completeness baseline.
#[derive(Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointCompletenessBaselineSourceError {
    /// A stage-specific filesystem operation failed.
    Io(FileRestartCheckpointSlotIoError),
    /// The current file length cannot be represented on this host.
    CurrentLengthOutOfRange {
        /// Exact filesystem length that was rejected.
        actual: u64,
    },
    /// The complete current byte buffer could not reserve its exact length.
    CurrentCapacityExhausted {
        /// Exact host-sized byte length that required reservation.
        length: usize,
    },
    /// The current file changed length while its already-open handle was read.
    CurrentLengthChanged {
        /// File length observed before the read.
        before: u64,
        /// File length observed after the read.
        after: u64,
    },
    /// The complete bytes failed ADR 0049 structural decoding.
    Decode(RestartCheckpointCompletenessBaselineDecodeError),
}

impl fmt::Display for FileRestartCheckpointCompletenessBaselineSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::CurrentLengthOutOfRange { actual } => write!(
                formatter,
                "current restart checkpoint completeness length {actual} is not representable on this host"
            ),
            Self::CurrentCapacityExhausted { length } => write!(
                formatter,
                "current restart checkpoint completeness byte capacity is exhausted for {length} bytes"
            ),
            Self::CurrentLengthChanged { before, after } => write!(
                formatter,
                "current restart checkpoint completeness length changed while reading: {before} to {after}"
            ),
            Self::Decode(source) => write!(
                formatter,
                "current restart checkpoint completeness structural decoding failed: {source}"
            ),
        }
    }
}

impl Error for FileRestartCheckpointCompletenessBaselineSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Decode(source) => Some(source),
            Self::CurrentLengthOutOfRange { .. }
            | Self::CurrentCapacityExhausted { .. }
            | Self::CurrentLengthChanged { .. } => None,
        }
    }
}

impl From<SlotCurrentReadError> for FileRestartCheckpointCompletenessBaselineSourceError {
    fn from(source: SlotCurrentReadError) -> Self {
        match source {
            SlotCurrentReadError::Io(source) => Self::Io(source),
            SlotCurrentReadError::CurrentLengthOutOfRange { actual } => {
                Self::CurrentLengthOutOfRange { actual }
            }
            SlotCurrentReadError::CurrentCapacityExhausted { length } => {
                Self::CurrentCapacityExhausted { length }
            }
            SlotCurrentReadError::CurrentLengthChanged { before, after } => {
                Self::CurrentLengthChanged { before, after }
            }
        }
    }
}

/// Exact deterministic failure point in completeness checkpoint publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointCompletenessBaselinePublicationFaultPoint {
    /// Fail after encoding but before stale-candidate cleanup.
    BeforeCandidateCleanup,
    /// Fail after stale-candidate cleanup and before candidate creation.
    AfterCandidateCleanup,
    /// Fail after creating an empty candidate and before writing bytes.
    AfterCandidateCreate,
    /// Fail after writing all bytes and before candidate synchronization.
    AfterCandidateWrite,
    /// Fail after synchronizing and closing the candidate, before replacement.
    AfterCandidateSync,
    /// Fail after replacing `current` and before directory synchronization.
    AfterCurrentReplace,
    /// Fail after directory synchronization instead of reporting success.
    AfterDirectorySync,
}

impl FileRestartCheckpointCompletenessBaselinePublicationFaultPoint {
    const fn from_step(step: SlotPublicationStep) -> Self {
        match step {
            SlotPublicationStep::BeforeCandidateCleanup => Self::BeforeCandidateCleanup,
            SlotPublicationStep::AfterCandidateCleanup => Self::AfterCandidateCleanup,
            SlotPublicationStep::AfterCandidateCreate => Self::AfterCandidateCreate,
            SlotPublicationStep::AfterCandidateWrite => Self::AfterCandidateWrite,
            SlotPublicationStep::AfterCandidateSync => Self::AfterCandidateSync,
            SlotPublicationStep::AfterCurrentReplace => Self::AfterCurrentReplace,
            SlotPublicationStep::AfterDirectorySync => Self::AfterDirectorySync,
        }
    }
}

impl fmt::Display for FileRestartCheckpointCompletenessBaselinePublicationFaultPoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeCandidateCleanup => formatter.write_str("before stale-candidate cleanup"),
            Self::AfterCandidateCleanup => formatter.write_str("after stale-candidate cleanup"),
            Self::AfterCandidateCreate => formatter.write_str("after candidate creation"),
            Self::AfterCandidateWrite => formatter.write_str("after candidate write"),
            Self::AfterCandidateSync => formatter.write_str("after candidate synchronization"),
            Self::AfterCurrentReplace => formatter.write_str("after current-entry replacement"),
            Self::AfterDirectorySync => formatter.write_str("after slot-directory synchronization"),
        }
    }
}

/// Rejected attempt to replace an already armed completeness publication fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileRestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {
    armed: FileRestartCheckpointCompletenessBaselinePublicationFaultPoint,
    requested: FileRestartCheckpointCompletenessBaselinePublicationFaultPoint,
}

impl FileRestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {
    /// Returns the retained existing fault.
    #[must_use]
    pub const fn armed(&self) -> FileRestartCheckpointCompletenessBaselinePublicationFaultPoint {
        self.armed
    }

    /// Returns the rejected replacement fault.
    #[must_use]
    pub const fn requested(
        &self,
    ) -> FileRestartCheckpointCompletenessBaselinePublicationFaultPoint {
        self.requested
    }
}

impl fmt::Display for FileRestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "filesystem completeness checkpoint publication fault {} is already armed; cannot arm {}",
            self.armed, self.requested
        )
    }
}

impl Error for FileRestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {}

/// Outcome-indeterminate filesystem completeness publication failure.
#[derive(Debug, Eq, PartialEq)]
pub enum FileRestartCheckpointCompletenessBaselinePublicationError {
    /// The owner permit did not identify the supplied baseline exactly.
    PublicationPermitMismatch {
        /// Persistent log ID carried by the authoritative baseline.
        baseline_persistent_log_id: u128,
        /// Persistent log ID carried by the owner permit.
        permit_persistent_log_id: u128,
        /// Optional frontier carried by the authoritative baseline.
        baseline_durable_frontier: Option<u64>,
        /// Optional frontier carried by the owner permit.
        permit_durable_frontier: Option<u64>,
        /// Transaction count carried by the authoritative baseline.
        baseline_transaction_count: usize,
        /// Transaction count carried by the owner permit.
        permit_transaction_count: usize,
        /// Page count carried by the authoritative baseline.
        baseline_page_count: usize,
        /// Page count carried by the owner permit.
        permit_page_count: usize,
    },
    /// The authoritative baseline belongs to another lineaged slot.
    SlotPersistentLogIdMismatch {
        /// Persistent log ID bound to the immutable slot control header.
        slot: PersistentLogId,
        /// Persistent log ID carried by the authoritative baseline.
        baseline: PersistentLogId,
    },
    /// ADR 0049 encoding failed before filesystem mutation.
    Encode(RestartCheckpointCompletenessBaselineEncodeError),
    /// A deterministic test fault fired at one exact physical boundary.
    InjectedFault(FileRestartCheckpointCompletenessBaselinePublicationFaultPoint),
    /// One exact filesystem operation failed.
    Io(FileRestartCheckpointSlotIoError),
}

impl fmt::Display for FileRestartCheckpointCompletenessBaselinePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublicationPermitMismatch {
                baseline_persistent_log_id,
                permit_persistent_log_id,
                baseline_durable_frontier,
                permit_durable_frontier,
                baseline_transaction_count,
                permit_transaction_count,
                baseline_page_count,
                permit_page_count,
            } => write!(
                formatter,
                "filesystem completeness checkpoint publication permit mismatch: baseline id {baseline_persistent_log_id:#034x}, frontier {baseline_durable_frontier:?}, transactions {baseline_transaction_count}, pages {baseline_page_count}; permit id {permit_persistent_log_id:#034x}, frontier {permit_durable_frontier:?}, transactions {permit_transaction_count}, pages {permit_page_count}"
            ),
            Self::SlotPersistentLogIdMismatch { slot, baseline } => write!(
                formatter,
                "filesystem completeness checkpoint slot persistent log ID {} does not match baseline persistent log ID {}",
                slot.get(),
                baseline.get()
            ),
            Self::Encode(source) => write!(
                formatter,
                "filesystem completeness checkpoint encoding failed: {source}"
            ),
            Self::InjectedFault(point) => write!(
                formatter,
                "injected filesystem completeness checkpoint publication failure {point}"
            ),
            Self::Io(source) => source.fmt(formatter),
        }
    }
}

impl Error for FileRestartCheckpointCompletenessBaselinePublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Encode(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::PublicationPermitMismatch { .. }
            | Self::SlotPersistentLogIdMismatch { .. }
            | Self::InjectedFault(_) => None,
        }
    }
}

/// Locked filesystem source and publisher for one current completeness baseline.
///
/// This adapter owns a completeness slot directory that is completely separate
/// from the transaction-only [`FileRestartCheckpointBaselineSource`] slot. Its
/// immutable `control` file uses the independent `NTSQCMS1` magic and its
/// selected `current` entry holds only ADR 0049 `NTSQCMP1` bytes, so neither
/// slot type can be opened, published, or emptied as the other.
///
/// [`FileRestartCheckpointBaselineSource`]: super::FileRestartCheckpointBaselineSource
///
/// Loaded bytes remain untrusted and cannot be used as an authoritative encoder
/// input:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     FileRestartCheckpointCompletenessBaselineSource,
///     encode_restart_checkpoint_completeness_baseline,
/// };
///
/// fn cannot_encode_source(source: &FileRestartCheckpointCompletenessBaselineSource) {
///     let _ = encode_restart_checkpoint_completeness_baseline(source);
/// }
/// ```
///
/// The completeness publication port still cannot be invoked without its owner
/// permit:
///
/// ```compile_fail
/// use ntsql_storage_file::FileRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::{
///     DurableTransactionRestartCheckpointCompletenessBaseline,
///     DurableTransactionRestartCheckpointCompletenessBaselinePublisher,
/// };
///
/// fn cannot_publish(
///     source: &mut FileRestartCheckpointCompletenessBaselineSource,
///     baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
/// ) {
///     let _ = source.publish_restart_checkpoint_completeness_baseline(baseline);
/// }
/// ```
///
/// It is not the transaction-only ADR 0040 read source:
///
/// ```compile_fail
/// use ntsql_storage_file::FileRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaselineSource;
///
/// fn require_transaction_source<Source: DurableTransactionRestartCheckpointBaselineSource>(
///     _source: &mut Source,
/// ) {
/// }
///
/// fn cannot_substitute_for_transaction_source(
///     source: &mut FileRestartCheckpointCompletenessBaselineSource,
/// ) {
///     require_transaction_source(source);
/// }
/// ```
///
/// It is not the transaction-only ADR 0042 publisher:
///
/// ```compile_fail
/// use ntsql_storage_file::FileRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::DurableTransactionRestartCheckpointBaselinePublisher;
///
/// fn require_transaction_publisher<
///     Publisher: DurableTransactionRestartCheckpointBaselinePublisher,
/// >(
///     _publisher: &mut Publisher,
/// ) {
/// }
///
/// fn cannot_substitute_for_transaction_publisher(
///     source: &mut FileRestartCheckpointCompletenessBaselineSource,
/// ) {
///     require_transaction_publisher(source);
/// }
/// ```
///
/// It is not WAL durability authority:
///
/// ```compile_fail
/// use ntsql_storage_file::FileRestartCheckpointCompletenessBaselineSource;
/// use ntsql_wal::LogDurability;
///
/// fn require_log<Log: LogDurability>(_log: &mut Log) {}
///
/// fn cannot_use_as_log(source: &mut FileRestartCheckpointCompletenessBaselineSource) {
///     require_log(source);
/// }
/// ```
///
/// It is not page-store write authority:
///
/// ```compile_fail
/// use ntsql_page::PageStore;
/// use ntsql_storage_file::FileRestartCheckpointCompletenessBaselineSource;
///
/// fn require_page_store<Store: PageStore<1>>(_store: &mut Store) {}
///
/// fn cannot_use_as_page_store(source: &mut FileRestartCheckpointCompletenessBaselineSource) {
///     require_page_store(source);
/// }
/// ```
///
/// It is not committed-page recovery write authority:
///
/// ```compile_fail
/// use ntsql_storage_file::FileRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::CommittedTransactionPageRecoveryStore;
///
/// fn require_recovery_store<Store: CommittedTransactionPageRecoveryStore<1>>(
///     _store: &mut Store,
/// ) {
/// }
///
/// fn cannot_use_as_recovery_store(
///     source: &mut FileRestartCheckpointCompletenessBaselineSource,
/// ) {
///     require_recovery_store(source);
/// }
/// ```
///
/// It cannot become transaction lifecycle state:
///
/// ```compile_fail
/// use ntsql_storage_file::FileRestartCheckpointCompletenessBaselineSource;
/// use ntsql_transaction::ActiveTransaction;
///
/// fn cannot_activate(
///     source: FileRestartCheckpointCompletenessBaselineSource,
/// ) -> ActiveTransaction {
///     source.into()
/// }
/// ```
///
/// It cannot become the restart-analyzed storage owner:
///
/// ```compile_fail
/// use ntsql_storage_file::{
///     FileCommitLog, FilePageStore, FileRestartCheckpointCompletenessBaselineSource,
/// };
/// use ntsql_transaction::RestartAnalyzedTransactionPageStorage;
///
/// fn cannot_release_storage(
///     source: FileRestartCheckpointCompletenessBaselineSource,
/// ) -> RestartAnalyzedTransactionPageStorage<FileCommitLog<1>, FilePageStore<1>, 1> {
///     source.into()
/// }
/// ```
#[derive(Debug)]
pub struct FileRestartCheckpointCompletenessBaselineSource {
    slot_directory: PathBuf,
    _control_file: File,
    directory: File,
    persistent_log_id: PersistentLogId,
    control_format_version: u16,
    database_file_identity: Option<DatabaseFileHeaderIdentity>,
    armed_publication_fault: Option<FileRestartCheckpointCompletenessBaselinePublicationFaultPoint>,
}

impl FileRestartCheckpointCompletenessBaselineSource {
    /// Creates and locks one new empty completeness checkpoint slot.
    ///
    /// Any error after directory creation may leave a partial slot requiring
    /// explicit caller reconciliation. It is never reported as successful.
    pub fn create_new<P>(
        slot_directory: P,
        persistent_log_id: PersistentLogId,
    ) -> Result<Self, FileRestartCheckpointSlotCreateError>
    where
        P: AsRef<Path>,
    {
        let slot_directory = slot_directory.as_ref().to_path_buf();
        let (control_file, directory) = create_locked_control_slot(
            &slot_directory,
            COMPLETENESS_CONTROL_MAGIC,
            persistent_log_id,
        )?;

        Ok(Self {
            slot_directory,
            _control_file: control_file,
            directory,
            persistent_log_id,
            control_format_version: super::restart_checkpoint_file::CONTROL_FORMAT_VERSION,
            database_file_identity: None,
            armed_publication_fault: None,
        })
    }

    /// Creates and locks one empty V2 completeness slot with stable database identity.
    pub fn create_new_database<P>(
        slot_directory: P,
        storage_identity: DatabaseStorageIdentity,
    ) -> Result<Self, FileRestartCheckpointSlotCreateError>
    where
        P: AsRef<Path>,
    {
        let slot_directory = slot_directory.as_ref().to_path_buf();
        let (control_file, directory, metadata) = create_locked_database_control_slot(
            &slot_directory,
            COMPLETENESS_CONTROL_MAGIC,
            storage_identity,
        )?;
        Ok(Self {
            slot_directory,
            _control_file: control_file,
            directory,
            persistent_log_id: metadata.persistent_log_id(),
            control_format_version: metadata.format_version(),
            database_file_identity: metadata.database_file_identity(),
            armed_publication_fault: None,
        })
    }

    /// Opens, locks, and validates one existing completeness checkpoint slot.
    ///
    /// A transaction-only `NTSQCKS1` slot fails at the control header magic,
    /// including when its `current` entry is absent.
    pub fn open<P>(slot_directory: P) -> Result<Self, FileRestartCheckpointSlotOpenError>
    where
        P: AsRef<Path>,
    {
        let slot_directory = slot_directory.as_ref().to_path_buf();
        let (control_file, directory, metadata) =
            open_locked_control_slot(&slot_directory, COMPLETENESS_CONTROL_MAGIC)?;

        Ok(Self {
            slot_directory,
            _control_file: control_file,
            directory,
            persistent_log_id: metadata.persistent_log_id(),
            control_format_version: metadata.format_version(),
            database_file_identity: metadata.database_file_identity(),
            armed_publication_fault: None,
        })
    }

    /// Returns the immutable persistent log identity bound to this slot.
    #[must_use]
    pub const fn persistent_log_id(&self) -> PersistentLogId {
        self.persistent_log_id
    }

    /// Returns the physically parsed completeness-control format version.
    #[must_use]
    pub const fn control_format_version(&self) -> u16 {
        self.control_format_version
    }

    /// Returns stable database-file identity when the control uses V2.
    #[must_use]
    pub const fn database_file_identity(&self) -> Option<DatabaseFileHeaderIdentity> {
        self.database_file_identity
    }

    pub(crate) fn control_metadata(&self) -> io::Result<std::fs::Metadata> {
        self._control_file.metadata()
    }

    /// Returns the caller-selected completeness slot directory.
    #[must_use]
    pub fn slot_directory(&self) -> &Path {
        &self.slot_directory
    }

    /// Arms one completeness publication fault without replacing an existing plan.
    pub fn arm_publication_fault(
        &mut self,
        fault: FileRestartCheckpointCompletenessBaselinePublicationFaultPoint,
    ) -> Result<(), FileRestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed> {
        if let Some(armed) = self.armed_publication_fault {
            return Err(
                FileRestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed {
                    armed,
                    requested: fault,
                },
            );
        }
        self.armed_publication_fault = Some(fault);
        Ok(())
    }

    /// Returns the one-shot publication fault that has not reached its stage.
    #[must_use]
    pub const fn armed_publication_fault(
        &self,
    ) -> Option<FileRestartCheckpointCompletenessBaselinePublicationFaultPoint> {
        self.armed_publication_fault
    }
}

impl DurableTransactionRestartCheckpointCompletenessBaselineSource
    for FileRestartCheckpointCompletenessBaselineSource
{
    type Error = FileRestartCheckpointCompletenessBaselineSourceError;

    fn load_restart_checkpoint_completeness_baseline(
        &mut self,
    ) -> Result<
        Option<OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation>,
        Self::Error,
    > {
        let Some(bytes) = read_current_slot_bytes(&self.slot_directory)? else {
            return Ok(None);
        };
        decode_restart_checkpoint_completeness_baseline(&bytes)
            .map(Some)
            .map_err(FileRestartCheckpointCompletenessBaselineSourceError::Decode)
    }
}

impl DurableTransactionRestartCheckpointCompletenessBaselinePublisher
    for FileRestartCheckpointCompletenessBaselineSource
{
    type Error = FileRestartCheckpointCompletenessBaselinePublicationError;

    fn publish_restart_checkpoint_completeness_baseline(
        &mut self,
        baseline: &DurableTransactionRestartCheckpointCompletenessBaseline,
        permit: DurableTransactionRestartCheckpointCompletenessBaselinePublicationPermit<'_>,
    ) -> Result<(), Self::Error> {
        let baseline_persistent_log_id = baseline.persistent_log_id().get();
        let permit_persistent_log_id = permit.persistent_log_id().get();
        let baseline_durable_frontier = baseline.durable_frontier();
        let permit_durable_frontier = permit.durable_frontier();
        let baseline_transaction_count = baseline.transactions().len();
        let permit_transaction_count = permit.transaction_count();
        let baseline_page_count = baseline.pages().len();
        let permit_page_count = permit.page_count();
        if baseline_persistent_log_id != permit_persistent_log_id
            || baseline_durable_frontier != permit_durable_frontier
            || baseline_transaction_count != permit_transaction_count
            || baseline_page_count != permit_page_count
        {
            return Err(
                FileRestartCheckpointCompletenessBaselinePublicationError::PublicationPermitMismatch {
                    baseline_persistent_log_id,
                    permit_persistent_log_id,
                    baseline_durable_frontier,
                    permit_durable_frontier,
                    baseline_transaction_count,
                    permit_transaction_count,
                    baseline_page_count,
                    permit_page_count,
                },
            );
        }
        if self.persistent_log_id != baseline.persistent_log_id() {
            return Err(
                FileRestartCheckpointCompletenessBaselinePublicationError::SlotPersistentLogIdMismatch {
                    slot: self.persistent_log_id,
                    baseline: baseline.persistent_log_id(),
                },
            );
        }

        let encoded = encode_restart_checkpoint_completeness_baseline(baseline)
            .map_err(FileRestartCheckpointCompletenessBaselinePublicationError::Encode)?;

        let Self {
            slot_directory,
            directory,
            armed_publication_fault,
            ..
        } = self;
        publish_slot_current_bytes(slot_directory, directory, &encoded, |step| {
            let point =
                FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::from_step(step);
            if *armed_publication_fault == Some(point) {
                *armed_publication_fault = None;
                true
            } else {
                false
            }
        })
        .map_err(|source| match source {
            SlotPublicationError::InjectedFault(step) => {
                FileRestartCheckpointCompletenessBaselinePublicationError::InjectedFault(
                    FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::from_step(step),
                )
            }
            SlotPublicationError::Io(source) => {
                FileRestartCheckpointCompletenessBaselinePublicationError::Io(source)
            }
        })
    }
}

/// Locked unrecovered WAL/page-store owner paired with its completeness slot.
///
/// Construction is available only through
/// [`open_transaction_page_storage_with_completeness_checkpoint`], which
/// acquires all three lifetime locks in one fixed order.
pub struct UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint<const N: usize> {
    storage: UnrecoveredTransactionPageStorage<FileCommitLog<N>, FilePageStore<N>, N>,
    checkpoint: FileRestartCheckpointCompletenessBaselineSource,
}

/// Owning pre-recovery completeness selection for the filesystem composition.
pub type FileTransactionPageStorageRestartCheckpointCompletenessSelection<const N: usize> =
    TransactionPageStorageRestartCheckpointCompletenessSelection<
        FileCommitLog<N>,
        FilePageStore<N>,
        FileRestartCheckpointCompletenessBaselineSource,
        N,
    >;

impl<const N: usize> fmt::Debug
    for UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint<N>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint")
            .field(
                "checkpoint_persistent_log_id",
                &self.checkpoint.persistent_log_id(),
            )
            .finish_non_exhaustive()
    }
}

impl<const N: usize> UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint<N> {
    pub(crate) fn from_locked_parts(
        log: FileCommitLog<N>,
        store: FilePageStore<N>,
        checkpoint: FileRestartCheckpointCompletenessBaselineSource,
    ) -> Self {
        Self {
            storage: UnrecoveredTransactionPageStorage::new(log, store),
            checkpoint,
        }
    }

    /// Loads and validates the locked completeness slot before recovery writes.
    pub fn select_restart_checkpoint_completeness(
        self,
    ) -> FileTransactionPageStorageRestartCheckpointCompletenessSelection<N> {
        self.storage
            .select_generation_aware_restart_checkpoint_completeness(self.checkpoint)
    }
}

/// Failure while opening the fixed-order WAL/page-store/completeness composition.
#[derive(Debug, Eq, PartialEq)]
pub enum FileTransactionPageStorageCompletenessCheckpointOpenError {
    /// The transaction-page-capable WAL could not be opened first.
    CommitLog(FileOpenError),
    /// The page store could not be opened second.
    PageStore(PageStoreOpenError),
    /// WAL and page-store control headers identify different persistent logs.
    StoragePersistentLogIdMismatch {
        /// Persistent log ID read from the WAL.
        commit_log: PersistentLogId,
        /// Persistent log ID read from the page store.
        page_store: PersistentLogId,
    },
    /// The completeness checkpoint slot could not be opened third.
    Checkpoint(FileRestartCheckpointSlotOpenError),
    /// The completeness control file identifies a different persistent log.
    CheckpointPersistentLogIdMismatch {
        /// Persistent log ID shared by the WAL and page store.
        storage: PersistentLogId,
        /// Persistent log ID read from the completeness control file.
        checkpoint: PersistentLogId,
    },
}

impl fmt::Display for FileTransactionPageStorageCompletenessCheckpointOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommitLog(source) => {
                write!(formatter, "transaction-page WAL open failed: {source}")
            }
            Self::PageStore(source) => write!(formatter, "page-store open failed: {source}"),
            Self::StoragePersistentLogIdMismatch {
                commit_log,
                page_store,
            } => write!(
                formatter,
                "WAL persistent log ID {} does not match page-store persistent log ID {}",
                commit_log.get(),
                page_store.get()
            ),
            Self::Checkpoint(source) => write!(
                formatter,
                "restart checkpoint completeness slot open failed: {source}"
            ),
            Self::CheckpointPersistentLogIdMismatch {
                storage,
                checkpoint,
            } => write!(
                formatter,
                "storage persistent log ID {} does not match completeness checkpoint persistent log ID {}",
                storage.get(),
                checkpoint.get()
            ),
        }
    }
}

impl Error for FileTransactionPageStorageCompletenessCheckpointOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CommitLog(source) => Some(source),
            Self::PageStore(source) => Some(source),
            Self::Checkpoint(source) => Some(source),
            Self::StoragePersistentLogIdMismatch { .. }
            | Self::CheckpointPersistentLogIdMismatch { .. } => None,
        }
    }
}

/// Opens and locks a WAL, page store, and lineaged completeness slot in that order.
///
/// A later-stage failure drops every earlier adapter before returning. The
/// operation is nonblocking and does not derive its lock order from
/// completeness validation or publication touch order. It neither opens nor
/// reinterprets a transaction-only checkpoint slot.
pub fn open_transaction_page_storage_with_completeness_checkpoint<
    const N: usize,
    LogPath,
    StorePath,
    CheckpointPath,
>(
    log_path: LogPath,
    store_path: StorePath,
    checkpoint_path: CheckpointPath,
) -> Result<
    UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint<N>,
    FileTransactionPageStorageCompletenessCheckpointOpenError,
>
where
    LogPath: AsRef<Path>,
    StorePath: AsRef<Path>,
    CheckpointPath: AsRef<Path>,
{
    let log = FileCommitLog::<N>::open_transaction_page_capable(log_path)
        .map_err(FileTransactionPageStorageCompletenessCheckpointOpenError::CommitLog)?;
    let store = FilePageStore::<N>::open(store_path)
        .map_err(FileTransactionPageStorageCompletenessCheckpointOpenError::PageStore)?;
    if log.persistent_id() != store.persistent_id() {
        return Err(
            FileTransactionPageStorageCompletenessCheckpointOpenError::StoragePersistentLogIdMismatch {
                commit_log: log.persistent_id(),
                page_store: store.persistent_id(),
            },
        );
    }
    let checkpoint = FileRestartCheckpointCompletenessBaselineSource::open(checkpoint_path)
        .map_err(FileTransactionPageStorageCompletenessCheckpointOpenError::Checkpoint)?;
    if checkpoint.persistent_log_id() != log.persistent_id() {
        return Err(
            FileTransactionPageStorageCompletenessCheckpointOpenError::CheckpointPersistentLogIdMismatch {
                storage: log.persistent_id(),
                checkpoint: checkpoint.persistent_log_id(),
            },
        );
    }

    Ok(
        UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint::from_locked_parts(
            log, store, checkpoint,
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::io;

    use ntsql_database::{DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole, DatabaseId};

    use super::{
        super::restart_checkpoint_file::{
            build_slot_control_header, build_slot_control_header_v2, parse_slot_control_header,
            parse_slot_control_header_v2,
        },
        *,
    };
    use crate::{checksum_v1, write_u64};

    #[test]
    fn completeness_control_header_has_exact_independent_golden_bytes() -> Result<(), io::Error> {
        let persistent_log_id = PersistentLogId::new(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)
            .ok_or_else(|| io::Error::other("test persistent log ID is zero"))?;
        let header = build_slot_control_header(COMPLETENESS_CONTROL_MAGIC, persistent_log_id);
        let expected = [
            0x4e, 0x54, 0x53, 0x51, 0x43, 0x4d, 0x53, 0x31, 0x00, 0x01, 0x00, 0x40, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xba, 0x49, 0xee, 0xcf, 0xf9, 0xb8, 0x4d, 0x5a,
        ];

        assert_eq!(header, expected);
        assert_eq!(&header[..8], b"NTSQCMS1");
        assert_eq!(
            parse_slot_control_header(COMPLETENESS_CONTROL_MAGIC, &header),
            Ok(persistent_log_id)
        );
        Ok(())
    }

    #[test]
    fn completeness_control_v2_has_exact_database_identity_golden_bytes()
    -> Result<(), Box<dyn Error>> {
        let persistent_log_id = PersistentLogId::new(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)
            .ok_or_else(|| io::Error::other("test persistent log ID is zero"))?;
        let database_id = DatabaseId::new(0x1112_1314_1516_1718_191a_1b1c_1d1e_1f20)
            .ok_or_else(|| io::Error::other("test database ID is zero"))?;
        let file_id = DatabaseFileId::new(0x2122_2324_2526_2728_292a_2b2c_2d2e_2f30)
            .ok_or_else(|| io::Error::other("test file ID is zero"))?;
        let identity = DatabaseFileHeaderIdentity::new(
            database_id,
            DatabaseFileIdentity::new(DatabaseFileRole::RestartCheckpoint, file_id),
        );
        let header =
            build_slot_control_header_v2(COMPLETENESS_CONTROL_MAGIC, persistent_log_id, identity);
        let mut expected = [0_u8; 128];
        expected[..8].copy_from_slice(b"NTSQCMS1");
        expected[8..12].copy_from_slice(&[0, 2, 0, 128]);
        expected[16..32].copy_from_slice(&persistent_log_id.get().to_be_bytes());
        expected[64..112].copy_from_slice(&[
            0x4e, 0x54, 0x53, 0x51, 0x43, 0x46, 0x49, 0x31, 0x00, 0x01, 0x00, 0x30, 0x03, 0x00,
            0x00, 0x00, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
            0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a,
            0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30,
        ]);
        expected[120..128].copy_from_slice(&0x4e80_b59f_985a_57a3_u64.to_be_bytes());
        assert_eq!(header, expected);

        let metadata = parse_slot_control_header_v2(COMPLETENESS_CONTROL_MAGIC, &header)?;
        assert_eq!(metadata.persistent_log_id(), persistent_log_id);
        assert_eq!(metadata.format_version(), 2);
        assert_eq!(metadata.database_file_identity(), Some(identity));

        let mut reserved = header;
        reserved[112] = 1;
        let checksum = checksum_v1(&reserved[..120]);
        write_u64(&mut reserved, 120, checksum);
        assert!(parse_slot_control_header_v2(COMPLETENESS_CONTROL_MAGIC, &reserved).is_err());
        let mut bad_checksum = header;
        bad_checksum[120] ^= 1;
        assert!(parse_slot_control_header_v2(COMPLETENESS_CONTROL_MAGIC, &bad_checksum).is_err());
        Ok(())
    }

    #[test]
    fn completeness_and_transaction_control_namespaces_reject_each_other() -> Result<(), io::Error>
    {
        let persistent_log_id =
            PersistentLogId::new(157).ok_or_else(|| io::Error::other("test ID is zero"))?;
        let completeness = build_slot_control_header(COMPLETENESS_CONTROL_MAGIC, persistent_log_id);
        let transaction = build_slot_control_header(*b"NTSQCKS1", persistent_log_id);

        assert_ne!(completeness, transaction);
        assert!(parse_slot_control_header(*b"NTSQCKS1", &completeness).is_err());
        assert!(parse_slot_control_header(COMPLETENESS_CONTROL_MAGIC, &transaction).is_err());
        Ok(())
    }
}
