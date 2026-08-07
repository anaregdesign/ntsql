use std::{
    cell::Cell,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_database::{
    DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole, DatabaseId, DatabaseStorageIdentity,
};
use ntsql_page::{
    PageAddress, PageImage, PageLog, PageNumber, PageVersion, StoredPageSnapshotObservation,
    UnloggedPage,
};
use ntsql_recovery_model::{
    Applied as ModelApplied, CandidateEntry as ModelCandidateEntry,
    CheckpointAnchor as ModelCheckpointAnchor,
    CheckpointCandidateState as ModelCheckpointCandidateState, LogId as ModelLogId, ModelError,
    PageId as ModelPageId, PageVersion as ModelPageVersion, RecoveryModel,
    SelectedEntryState as ModelSelectedEntryState, WalRecordKind as ModelWalRecordKind,
};
use ntsql_storage_file::{
    FaultPoint, FileCommitLog, FileCommitLogError, FileCommittedPageRecoveryObservationError,
    FilePageStore, FilePageStoreError, FileRestartCheckpointCompletenessBaselinePublicationError,
    FileRestartCheckpointCompletenessBaselinePublicationFaultPoint,
    FileRestartCheckpointCompletenessBaselineSource, FileRestartCheckpointPageRepairStoreError,
    FileRestartCheckpointSlotIoStage, FileTransactionRestartAnalysisSourceError,
    FileTransactionRestartWalReclamationError, PageStoreFaultPoint,
    encode_restart_checkpoint_completeness_baseline,
    open_transaction_page_storage_with_completeness_checkpoint,
};
use ntsql_transaction::{
    CommittedTransactionPageRecoveryOutcome, CommittedTransactionPageRecoveryStore,
    CommittedTransactionPageRecoveryWritePermit, DurableCommittedTransactionPageRecoveryCandidate,
    DurablePageStoreSnapshotSource,
    DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError,
    DurableTransactionRestartCheckpointCompletenessBaselineSource,
    DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError,
    DurableTransactionRestartCheckpointCompletenessBaselineValidationError,
    DurableTransactionRestartCheckpointCompletenessBaselineValidationEvidenceError,
    DurableTransactionRestartCompletenessError, DurableTransactionRestartCompletenessEvidenceError,
    DurableTransactionRestartPageState, DurableTransactionRestartWalReclamationError,
    DurableTransactionRestartWalReclamationOutcomeIndeterminateError,
    DurableTransactionRestartWalReclamationSource, RestartAnalyzedTransactionPageStorage,
    TransactionCoordinator, TransactionPageStorageRestartCheckpointCompletenessSelection,
    TransactionPageStorageRestartCheckpointPageRepairExecution,
    TransactionPageStorageRestartCheckpointRepairPreparation,
    TransactionPageStorageRestartCheckpointRestoration,
    TransactionRestartCheckpointPageRepairFailureCause,
    TransactionRestartCheckpointPageRepairOutcome, UnrecoveredTransactionPageStorage,
    WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay, flush_committed_page,
};
use ntsql_wal::{LogDurability, LogLineage, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

type FileOwner = RestartAnalyzedTransactionPageStorage<FileCommitLog<2>, FilePageStore<2>, 2>;
type FileReclamationOwner = WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay<
    FileCommitLog<2>,
    FilePageStore<2>,
    FileRestartCheckpointCompletenessBaselineSource,
    2,
>;

type FilePublicationError =
    DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError<
        FileTransactionRestartAnalysisSourceError<2>,
        FileCommittedPageRecoveryObservationError<2>,
        FileRestartCheckpointCompletenessBaselinePublicationError,
    >;

#[derive(Debug, Eq, PartialEq)]
enum NormalizedRecordKind {
    TransactionCommit {
        epoch: u64,
        sequence: u64,
    },
    RawPage {
        page: u64,
        version: u64,
        value: u64,
    },
    TransactionPage {
        epoch: u64,
        sequence: u64,
        page: u64,
        version: u64,
        value: u64,
    },
}

#[derive(Debug, Eq, PartialEq)]
struct NormalizedRecord {
    position: u64,
    kind: NormalizedRecordKind,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct NormalizedPage {
    page: u64,
    version: u64,
    value: u64,
    required_position: u64,
}

struct OneShotObservationFaultFilePageStore<const N: usize> {
    inner: FilePageStore<N>,
    fault_page: PageNumber,
    armed: Cell<bool>,
}

impl<const N: usize> OneShotObservationFaultFilePageStore<N> {
    fn new(inner: FilePageStore<N>, fault_page: PageNumber) -> Self {
        Self {
            inner,
            fault_page,
            armed: Cell::new(true),
        }
    }

    fn into_inner(self) -> FilePageStore<N> {
        self.inner
    }
}

impl<const N: usize> DurablePageStoreSnapshotSource<N> for OneShotObservationFaultFilePageStore<N> {
    type ObservationError = io::Error;

    fn lineage(&self) -> &LogLineage {
        DurablePageStoreSnapshotSource::lineage(&self.inner)
    }

    fn observe_page(
        &self,
        page_number: PageNumber,
    ) -> Result<Option<StoredPageSnapshotObservation<N>>, Self::ObservationError> {
        if page_number == self.fault_page && self.armed.replace(false) {
            return Err(io::Error::other(
                "injected filesystem repair preparation observation failure",
            ));
        }
        DurablePageStoreSnapshotSource::observe_page(&self.inner, page_number)
            .map_err(|source| io::Error::other(source.to_string()))
    }
}

impl<const N: usize> CommittedTransactionPageRecoveryStore<N>
    for OneShotObservationFaultFilePageStore<N>
{
    type WriteError = <FilePageStore<N> as CommittedTransactionPageRecoveryStore<N>>::WriteError;

    fn compare_and_replace(
        &mut self,
        candidate: &DurableCommittedTransactionPageRecoveryCandidate<'_, '_, N>,
        permit: CommittedTransactionPageRecoveryWritePermit<'_>,
    ) -> Result<(), Self::WriteError> {
        self.inner.compare_and_replace(candidate, permit)
    }
}

#[test]
fn publication_reconciles_stale_candidate_replaces_current_and_loads_untrusted()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("publish-replace")?;
    let persistent_log_id = persistent_log_id(15701)?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let slot_path = directory.path().join("completeness");
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;
    write_synced_new(&slot_path.join("candidate"), b"stale candidate")?;

    let empty_expected =
        owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let empty_bytes = encode_restart_checkpoint_completeness_baseline(&empty_expected)?;
    assert!(empty_expected.transactions().is_empty());
    assert!(empty_expected.pages().is_empty());
    assert_eq!(empty_expected.durable_frontier(), None);

    let empty_receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(
        empty_receipt.persistent_log_id(),
        empty_expected.persistent_log_id()
    );
    assert_eq!(
        empty_receipt.durable_frontier(),
        empty_expected.durable_frontier()
    );
    assert_eq!(
        empty_receipt.transaction_count(),
        empty_expected.transactions().len()
    );
    assert_eq!(empty_receipt.page_count(), empty_expected.pages().len());
    assert_eq!(fs::read(slot_path.join("current"))?, empty_bytes);
    assert!(!slot_path.join("candidate").exists());

    append_committed_page(&mut owner, 157, 1, [0x15, 0x71])?;
    let replacement =
        owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let replacement_bytes = encode_restart_checkpoint_completeness_baseline(&replacement)?;
    assert_eq!(replacement.transactions().len(), 1);
    assert_eq!(replacement.pages().len(), 1);
    assert_ne!(replacement_bytes, empty_bytes);

    let replacement_receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(
        replacement_receipt.persistent_log_id(),
        replacement.persistent_log_id()
    );
    assert_eq!(
        replacement_receipt.durable_frontier(),
        replacement.durable_frontier()
    );
    assert_eq!(
        replacement_receipt.transaction_count(),
        replacement.transactions().len()
    );
    assert_eq!(replacement_receipt.page_count(), replacement.pages().len());
    assert_eq!(fs::read(slot_path.join("current"))?, replacement_bytes);
    assert!(!slot_path.join("candidate").exists());

    let loaded = checkpoint
        .load_restart_checkpoint_completeness_baseline()?
        .ok_or_else(|| io::Error::other("published completeness slot reported absent"))?;
    assert_eq!(
        loaded.transactions().persistent_log_id(),
        persistent_log_id.get()
    );
    assert_eq!(
        loaded.transactions().transactions().len(),
        replacement.transactions().len()
    );
    assert_eq!(loaded.pages().len(), replacement.pages().len());
    assert_eq!(
        loaded.pages()[0].page_number(),
        replacement.pages()[0].page_number().get()
    );
    assert_eq!(
        loaded.replay().position(),
        replacement.replay_start().position()
    );
    assert_eq!(
        owner.validate_restart_checkpoint_completeness_baseline_against_current_prefix(
            &loaded.as_observation()
        )?,
        replacement
    );
    assert_eq!(
        owner.validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)?,
        Some(replacement)
    );
    assert_eq!(checkpoint.armed_publication_fault(), None);
    Ok(())
}

#[test]
fn pre_recovery_selection_accepts_missing_page_then_retains_slot_through_full_recovery()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("pre-recovery-selected")?;
    let persistent_log_id = persistent_log_id(15901)?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let page_number = 159_u64;
    append_committed_page_without_store_flush(&mut owner, page_number, 1, [0x15, 0x91])?;
    let baseline = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    assert!(matches!(
        baseline.pages(),
        [entry]
            if entry.page_number().get() == page_number
                && matches!(
                    entry.state(),
                    DurableTransactionRestartPageState::StoreMissing { .. }
                )
    ));

    let slot_path = directory.path().join("completeness");
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;
    let receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(receipt.page_count(), 1);
    drop(owner);
    drop(checkpoint);

    let opened = open_transaction_page_storage_with_completeness_checkpoint::<2, _, _, _>(
        directory.path().join("wal.bin"),
        directory.path().join("pages.bin"),
        &slot_path,
    )?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let selection = opened.select_restart_checkpoint_completeness();
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        selection
    else {
        return Err(io::Error::other("missing-page checkpoint was not selected").into());
    };
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(selected.persistent_log_id(), persistent_log_id);
    assert_eq!(selected.durable_frontier(), baseline.durable_frontier());
    assert_eq!(selected.transaction_count(), baseline.transactions().len());
    assert_eq!(selected.page_count(), 1);

    let uncheckpointed = selected
        .decline_checkpoint()
        .map_err(|_| io::Error::other("generation-zero checkpoint denied full recovery"))?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let recovered = uncheckpointed.recover()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert!(matches!(
        recovered.recovery_report().pages(),
        [CommittedTransactionPageRecoveryOutcome::Recovered { .. }]
    ));
    let mut analyzed = recovered.analyze_restart()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let replacement =
        analyzed.publish_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    assert_eq!(replacement.persistent_log_id(), persistent_log_id);
    assert_eq!(replacement.page_count(), 1);
    assert_file_composition_locked(directory.path(), &slot_path)?;

    let (log, store, _, _, checkpoint) = analyzed.into_parts();
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let page_number =
        PageNumber::new(page_number).ok_or_else(|| io::Error::other("page number is zero"))?;
    let stored = store
        .page(page_number)
        .ok_or_else(|| io::Error::other("full recovery did not persist selected page"))?;
    assert_eq!(stored.page_version(), PageVersion::new(1));
    assert_eq!(stored.bytes(), &[0x15, 0x91]);
    assert_eq!(checkpoint.persistent_log_id(), persistent_log_id);
    drop((log, store, checkpoint));
    drop(FileCommitLog::<2>::open_transaction_page_capable(
        directory.path().join("wal.bin"),
    )?);
    drop(FilePageStore::<2>::open(
        directory.path().join("pages.bin"),
    )?);
    drop(FileRestartCheckpointCompletenessBaselineSource::open(
        &slot_path,
    )?);
    Ok(())
}

#[test]
fn selected_checkpoint_plans_suffix_without_releasing_filesystem_locks()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("selected-replay-plan")?;
    let persistent_log_id = persistent_log_id(16101)?;
    let page_store_path = directory.path().join("pages.bin");
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let checkpoint_page = 161_u64;
    let suffix_page = 162_u64;
    append_committed_page(&mut owner, checkpoint_page, 1, [0x16, 0x11])?;
    let baseline = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let checkpoint_frontier = baseline
        .durable_frontier()
        .ok_or_else(|| io::Error::other("filesystem replay checkpoint frontier is empty"))?;
    assert_eq!(baseline.replay_start().position(), None);

    let slot_path = directory.path().join("completeness");
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;
    let receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(receipt.durable_frontier(), Some(checkpoint_frontier));
    append_committed_page_without_store_flush(&mut owner, suffix_page, 2, [0x16, 0x12])?;
    let current_frontier = owner
        .parts()
        .0
        .durable_position()
        .ok_or_else(|| io::Error::other("filesystem replay suffix is not durable"))?;
    assert!(current_frontier.get() > checkpoint_frontier);
    drop(owner);
    drop(checkpoint);

    let opened = open_transaction_page_storage_with_completeness_checkpoint::<2, _, _, _>(
        directory.path().join("wal.bin"),
        &page_store_path,
        &slot_path,
    )?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let selection = opened.select_restart_checkpoint_completeness();
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        selection
    else {
        return Err(io::Error::other("filesystem replay checkpoint was not selected").into());
    };
    assert_file_composition_locked(directory.path(), &slot_path)?;

    let planned = selected.plan_replay_window()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(planned.persistent_log_id(), persistent_log_id);
    assert_eq!(planned.checkpoint_frontier(), Some(checkpoint_frontier));
    assert_eq!(planned.current_frontier(), Some(current_frontier.get()));
    assert_eq!(planned.inclusive_replay_start(), None);
    assert_eq!(planned.replay_record_count(), 2);
    assert_eq!(planned.current_transaction_count(), 2);

    let page_store_before_preparation = fs::read(&page_store_path)?;
    let preparation = planned.prepare_page_repairs();
    let TransactionPageStorageRestartCheckpointRepairPreparation::Prepared(prepared) = preparation
    else {
        return Err(io::Error::other("filesystem replay page repair preparation failed").into());
    };
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(prepared.persistent_log_id(), persistent_log_id);
    assert_eq!(prepared.checkpoint_frontier(), Some(checkpoint_frontier));
    assert_eq!(prepared.current_frontier(), Some(current_frontier.get()));
    assert_eq!(prepared.page_count(), 1);
    assert_eq!(prepared.no_required_image_count(), 0);
    assert_eq!(prepared.unchanged_checkpoint_current_count(), 0);
    assert_eq!(prepared.already_current_count(), 0);
    assert_eq!(prepared.repair_candidate_count(), 1);
    assert_eq!(fs::read(&page_store_path)?, page_store_before_preparation);

    let suffix_page =
        PageNumber::new(suffix_page).ok_or_else(|| io::Error::other("suffix page is zero"))?;
    let TransactionPageStorageRestartCheckpointPageRepairExecution::Repaired(repaired) =
        prepared.execute_page_repairs()
    else {
        return Err(io::Error::other("filesystem replay page repair execution failed").into());
    };
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        repaired.page_outcomes(),
        [TransactionRestartCheckpointPageRepairOutcome::Repaired {
            page_number: suffix_page,
        }]
    );
    assert_ne!(fs::read(&page_store_path)?, page_store_before_preparation);
    let TransactionPageStorageRestartCheckpointRestoration::Restored(restored) =
        repaired.restore_transaction_state()
    else {
        return Err(io::Error::other("filesystem transaction restoration failed").into());
    };
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let first_restoration = restored.transaction_summary();
    assert_eq!(first_restoration.transaction_count(), 2);
    assert_eq!(first_restoration.committed_count(), 2);
    assert_eq!(first_restoration.unresolved_count(), 0);
    assert_eq!(first_restoration.coordinator_epoch().get(), 3);
    assert_eq!(
        first_restoration
            .highest_persisted_transaction()
            .map(|transaction| transaction.epoch()),
        Some(2)
    );
    let completed = restored.complete_restart()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        completed.completion_evidence().current_frontier(),
        Some(current_frontier.get())
    );
    assert_eq!(
        completed.completion_evidence().page_outcomes(),
        [TransactionRestartCheckpointPageRepairOutcome::Repaired {
            page_number: suffix_page,
        }]
    );
    let mut retained = completed.analyze_wal_retention()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        retained.retention_analysis().persistent_log_id(),
        persistent_log_id
    );
    assert_eq!(
        retained.retention_analysis().durable_frontier(),
        Some(current_frontier.get())
    );
    assert_eq!(
        retained.retention_analysis().allocated_epoch_high_water(),
        3
    );
    assert_eq!(
        retained
            .retention_analysis()
            .floor()
            .retained_first_record(),
        Some(1)
    );
    assert_eq!(retained.retention_analysis().store_page_count(), 2);
    assert_eq!(
        retained.retention_analysis().unresolved_transaction_count(),
        0
    );

    let live_page = PageNumber::new(163).ok_or_else(|| io::Error::other("live page is zero"))?;
    {
        let (coordinator, log, store) = retained.parts_mut();
        let lineage = LogDurability::lineage(log).clone();
        let active = coordinator.begin()?;
        assert_eq!(active.transaction_id().epoch().get(), 3);
        assert_eq!(active.transaction_id().sequence(), 1);
        let (active, dirty) = coordinator.stage_page_write(
            active,
            unlogged_page(&lineage, live_page.get(), 3, [0x16, 0x13])?,
            log,
        )?;
        let committed = coordinator.commit(active, log)?;
        flush_committed_page(&committed, log, store, dirty)?;
    }
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        retained.completion_evidence().current_frontier(),
        Some(current_frontier.get())
    );

    let publication =
        retained.publish_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let published_frontier = publication
        .durable_frontier()
        .ok_or_else(|| io::Error::other("completed filesystem frontier is empty"))?;
    assert!(published_frontier > current_frontier.get());
    assert_eq!(publication.transaction_count(), 3);
    assert_eq!(publication.page_count(), 3);
    assert_file_composition_locked(directory.path(), &slot_path)?;

    let (coordinator, log, store, completion_evidence, retention_analysis, checkpoint) =
        retained.into_parts();
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        completion_evidence.current_frontier(),
        Some(current_frontier.get())
    );
    assert_eq!(retention_analysis.floor().retained_first_record(), Some(1));
    drop((
        coordinator,
        log,
        store,
        completion_evidence,
        retention_analysis,
        checkpoint,
    ));

    let reopened = open_transaction_page_storage_with_completeness_checkpoint::<2, _, _, _>(
        directory.path().join("wal.bin"),
        &page_store_path,
        &slot_path,
    )?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let selection = reopened.select_restart_checkpoint_completeness();
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        selection
    else {
        return Err(io::Error::other("reopened completion checkpoint was not selected").into());
    };
    assert_eq!(selected.durable_frontier(), Some(published_frontier));
    assert_eq!(selected.transaction_count(), 3);
    assert_eq!(selected.page_count(), 3);
    let planned = selected.plan_replay_window()?;
    assert_eq!(planned.current_frontier(), Some(published_frontier));
    assert_eq!(planned.replay_record_count(), 0);
    let TransactionPageStorageRestartCheckpointRepairPreparation::Prepared(prepared) =
        planned.prepare_page_repairs()
    else {
        return Err(io::Error::other("reopened completion preparation failed").into());
    };
    assert_eq!(prepared.page_count(), 0);
    let TransactionPageStorageRestartCheckpointPageRepairExecution::Repaired(repaired) =
        prepared.execute_page_repairs()
    else {
        return Err(io::Error::other("reopened completion execution failed").into());
    };
    assert!(repaired.page_outcomes().is_empty());
    let TransactionPageStorageRestartCheckpointRestoration::Restored(restored) =
        repaired.restore_transaction_state()
    else {
        return Err(io::Error::other("reopened completion restoration failed").into());
    };
    let second_restoration = restored.transaction_summary();
    assert_eq!(second_restoration.transaction_count(), 3);
    assert_eq!(second_restoration.committed_count(), 3);
    assert_eq!(second_restoration.unresolved_count(), 0);
    assert_eq!(second_restoration.coordinator_epoch().get(), 4);
    assert!(
        second_restoration.coordinator_epoch().get() > first_restoration.coordinator_epoch().get()
    );
    let completed = restored.complete_restart()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        completed.completion_evidence().current_frontier(),
        Some(published_frontier)
    );
    let retained = completed.analyze_wal_retention()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        retained.retention_analysis().allocated_epoch_high_water(),
        4
    );
    assert_eq!(retained.retention_analysis().store_page_count(), 3);

    let (coordinator, log, store, completion_evidence, retention_analysis, checkpoint) =
        retained.into_parts();
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        completion_evidence.current_frontier(),
        Some(published_frontier)
    );
    let stored = store
        .page(suffix_page)
        .ok_or_else(|| io::Error::other("executed suffix repair was not durable"))?;
    assert_eq!(stored.page_version(), PageVersion::new(2));
    assert_eq!(stored.bytes(), &[0x16, 0x12]);
    let stored_live = store
        .page(live_page)
        .ok_or_else(|| io::Error::other("live filesystem page was not durable after reopen"))?;
    assert_eq!(stored_live.page_version(), PageVersion::new(3));
    assert_eq!(stored_live.bytes(), &[0x16, 0x13]);
    assert_eq!(checkpoint.persistent_log_id(), persistent_log_id);
    assert_eq!(
        retention_analysis.durable_frontier(),
        Some(published_frontier)
    );
    drop((
        coordinator,
        log,
        store,
        completion_evidence,
        retention_analysis,
        checkpoint,
    ));

    drop(FileCommitLog::<2>::open_transaction_page_capable(
        directory.path().join("wal.bin"),
    )?);
    drop(FilePageStore::<2>::open(&page_store_path)?);
    drop(FileRestartCheckpointCompletenessBaselineSource::open(
        &slot_path,
    )?);
    Ok(())
}

#[test]
fn filesystem_wal_append_and_flush_faults_match_model_after_reopen() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("wal-model-faults")?;
    for (index, fault) in [
        FaultPoint::BeforeAppend,
        FaultPoint::AfterAppend,
        FaultPoint::BeforeFlush,
        FaultPoint::AfterFlush,
    ]
    .into_iter()
    .enumerate()
    {
        let case_path = directory.path().join(format!("case-{index}"));
        fs::create_dir(&case_path)?;
        let persistent_log_id = persistent_log_id(16900 + index as u128)?;
        let wal_path = case_path.join("wal.bin");
        let page_store_path = case_path.join("pages.bin");
        let mut log =
            FileCommitLog::<2>::create_new_transaction_page_capable(&wal_path, persistent_log_id)?;
        let store = FilePageStore::<2>::create_new(&page_store_path, persistent_log_id)?;
        let coordinator = TransactionCoordinator::open(&mut log)?;
        drop(coordinator);
        let model_log_id = ModelLogId::new(persistent_log_id.get())
            .ok_or_else(|| io::Error::other("model persistent log ID is zero"))?;
        let mut model = RecoveryModel::new(model_log_id);
        model.allocate_coordinator_epoch()?;
        let page_number = 169 + index as u64;
        let bytes = [0x16, index as u8];
        let model_page = ModelPageId::new(page_number)
            .ok_or_else(|| io::Error::other("model page number is zero"))?;
        let page = unlogged_page(log.lineage(), page_number, 1, bytes)?;

        match fault {
            FaultPoint::BeforeAppend | FaultPoint::AfterAppend => {
                log.arm_fault(fault)?;
                let error = PageLog::append_page(&mut log, &page)
                    .err()
                    .ok_or_else(|| io::Error::other(format!("fault {fault} reported success")))?;
                assert_eq!(error, FileCommitLogError::InjectedFault(fault));
                if fault == FaultPoint::AfterAppend {
                    model.append_raw_page(
                        model_page,
                        model_page_value(bytes),
                        ModelPageVersion::new(1),
                    )?;
                }
            }
            FaultPoint::BeforeFlush | FaultPoint::AfterFlush => {
                let position = PageLog::append_page(&mut log, &page)?;
                model.append_raw_page(
                    model_page,
                    model_page_value(bytes),
                    ModelPageVersion::new(1),
                )?;
                log.arm_fault(fault)?;
                let error = log
                    .flush_through(&position)
                    .err()
                    .ok_or_else(|| io::Error::other(format!("fault {fault} reported success")))?;
                assert_eq!(error, FileCommitLogError::InjectedFault(fault));
                if fault == FaultPoint::AfterFlush {
                    model.flush_wal()?;
                }
            }
            _ => {
                return Err(io::Error::other("non-WAL fault entered the WAL model matrix").into());
            }
        }

        drop(log);
        model.crash_preserving_complete_wal_tail()?;
        model.reopen()?;
        let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
        assert_file_subject_matches_model(
            &mut reopened,
            &store,
            &model,
            &format!("WAL fault {fault}"),
        )?;
        if fault != FaultPoint::AfterFlush {
            model.select()?;
            assert_eq!(model.plan_replay()?, 0);
            model.repair_pages()?;
            model.restore_transactions()?;
            model.complete()?;
            let coordinator = TransactionCoordinator::open(&mut reopened)?;
            drop(coordinator);

            let continuation_page_number = page_number + 1_000;
            let continuation_bytes = [0x26, index as u8];
            let continuation = unlogged_page(
                reopened.lineage(),
                continuation_page_number,
                2,
                continuation_bytes,
            )?;
            let actual_position = PageLog::append_page(&mut reopened, &continuation)?;
            let expected_position = model.append_raw_page(
                ModelPageId::new(continuation_page_number)
                    .ok_or_else(|| io::Error::other("continuation model page is zero"))?,
                model_page_value(continuation_bytes),
                ModelPageVersion::new(2),
            )?;
            assert_eq!(actual_position.get(), expected_position.get());
            reopened.flush_through(&actual_position)?;
            model.flush_wal()?;
            drop(reopened);
            model.crash()?;
            model.reopen()?;
            let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
            assert_file_subject_matches_model(
                &mut reopened,
                &store,
                &model,
                &format!("WAL fault {fault} after continuation flush"),
            )?;
        }
    }
    Ok(())
}

#[test]
fn wal_reclamation_replaces_v3_and_v4_generations_without_renumbering() -> Result<(), Box<dyn Error>>
{
    let directory = TestDirectory::new("wal-reclamation-generations")?;
    let persistent_log_id = persistent_log_id(17001)?;
    let wal_path = directory.path().join("wal.bin");
    let page_store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("completeness");
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let mut model = reclamation_model(persistent_log_id, Some((170, [0x17, 0x01])))?;

    let pruned_position = append_committed_transaction(&mut owner)?;
    assert_eq!(pruned_position, 1);
    append_committed_page(&mut owner, 170, 1, [0x17, 0x01])?;
    let durable_frontier = owner
        .parts()
        .0
        .durable_position()
        .ok_or_else(|| io::Error::other("reclamation source frontier is empty"))?;
    assert_eq!(durable_frontier.get(), 3);

    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;
    let published = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(published.durable_frontier(), Some(durable_frontier.get()));
    drop((owner, checkpoint));

    let opened = open_transaction_page_storage_with_completeness_checkpoint::<2, _, _, _>(
        &wal_path,
        &page_store_path,
        &slot_path,
    )?;
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        opened.select_restart_checkpoint_completeness()
    else {
        return Err(io::Error::other("V3 reclamation checkpoint was not selected").into());
    };
    let planned = selected.plan_replay_window()?;
    assert_eq!(planned.replay_record_count(), 0);
    let TransactionPageStorageRestartCheckpointRepairPreparation::Prepared(prepared) =
        planned.prepare_page_repairs()
    else {
        return Err(io::Error::other("V3 reclamation preparation failed").into());
    };
    let TransactionPageStorageRestartCheckpointPageRepairExecution::Repaired(repaired) =
        prepared.execute_page_repairs()
    else {
        return Err(io::Error::other("V3 reclamation page execution failed").into());
    };
    let TransactionPageStorageRestartCheckpointRestoration::Restored(restored) =
        repaired.restore_transaction_state()
    else {
        return Err(io::Error::other("V3 reclamation restoration failed").into());
    };
    assert_eq!(restored.transaction_summary().coordinator_epoch().get(), 3);
    let completed = restored.complete_restart()?;
    let analyzed = completed.analyze_wal_retention()?;
    assert_eq!(
        analyzed
            .retention_analysis()
            .floor()
            .retained_first_record(),
        Some(2)
    );
    let reclaimed = analyzed
        .reclaim_wal_prefix()
        .map_err(|failed| io::Error::other(format!("{:?}", failed.error())))?;
    model.reclaim()?;
    let receipt = reclaimed.reclamation_receipt();
    assert_eq!(receipt.source_physical_format_version(), 3);
    assert_eq!(receipt.replacement_physical_format_version(), 4);
    assert_eq!(receipt.old_generation(), 0);
    assert_eq!(receipt.new_generation(), 1);
    assert_eq!(receipt.retained_first_logical_record(), Some(2));
    assert_eq!(
        receipt.logical_position_high_water(),
        Some(durable_frontier.get())
    );
    assert_eq!(receipt.retained_logical_record_count(), 2);
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let (coordinator, mut log, store, evidence) = reclaimed.into_parts();
    assert_file_subject_matches_model(&mut log, &store, &model, "V3-to-V4 reclamation")?;
    assert_eq!(
        log.durable_records()
            .map(|record| record.position().get())
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    drop((coordinator, log, store, evidence));

    let reopened = open_transaction_page_storage_with_completeness_checkpoint::<2, _, _, _>(
        &wal_path,
        &page_store_path,
        &slot_path,
    )?;
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        reopened.select_restart_checkpoint_completeness()
    else {
        return Err(io::Error::other("anchored V4 checkpoint was not selected").into());
    };
    let planned = selected.plan_replay_window()?;
    assert_eq!(planned.replay_record_count(), 0);
    let TransactionPageStorageRestartCheckpointRepairPreparation::Prepared(prepared) =
        planned.prepare_page_repairs()
    else {
        return Err(io::Error::other("V4 reclamation preparation failed").into());
    };
    let TransactionPageStorageRestartCheckpointPageRepairExecution::Repaired(repaired) =
        prepared.execute_page_repairs()
    else {
        return Err(io::Error::other("V4 reclamation page execution failed").into());
    };
    let TransactionPageStorageRestartCheckpointRestoration::Restored(restored) =
        repaired.restore_transaction_state()
    else {
        return Err(io::Error::other("V4 reclamation restoration failed").into());
    };
    assert_eq!(restored.transaction_summary().coordinator_epoch().get(), 4);
    let analyzed = restored.complete_restart()?.analyze_wal_retention()?;
    model.crash()?;
    model.reopen()?;
    model.select()?;
    model.plan_replay()?;
    model.repair_pages()?;
    model.restore_transactions()?;
    model.complete()?;
    model.analyze_retention()?;
    let reclaimed = analyzed
        .reclaim_wal_prefix()
        .map_err(|failed| io::Error::other(format!("{:?}", failed.error())))?;
    model.reclaim()?;
    let receipt = reclaimed.reclamation_receipt();
    assert_eq!(receipt.source_physical_format_version(), 4);
    assert_eq!(receipt.replacement_physical_format_version(), 4);
    assert_eq!(receipt.old_generation(), 1);
    assert_eq!(receipt.new_generation(), 2);
    assert_eq!(receipt.retained_first_logical_record(), Some(2));
    assert_eq!(
        receipt.logical_position_high_water(),
        Some(durable_frontier.get())
    );
    assert_file_composition_locked(directory.path(), &slot_path)?;

    let (coordinator, mut log, store, evidence) = reclaimed.into_parts();
    assert_file_subject_matches_model(&mut log, &store, &model, "V4-to-V4 reclamation")?;
    drop(coordinator);
    let mut next_coordinator = TransactionCoordinator::open(&mut log)?;
    let active = next_coordinator.begin()?;
    assert_eq!(active.transaction_id().epoch().get(), 5);
    let committed = next_coordinator.commit(active, &mut log)?;
    assert_eq!(committed.log_position().get(), 4);
    log.flush_through(committed.log_position())?;
    drop((next_coordinator, log, store, evidence));

    let reopened = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
    assert_eq!(
        reopened
            .durable_records()
            .map(|record| record.position().get())
            .collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    assert_eq!(
        reopened.durable_position().map(|position| position.get()),
        Some(4)
    );
    Ok(())
}

#[test]
fn wal_v5_reclamation_preserves_stable_database_identity() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("wal-v5-reclamation-identity")?;
    let persistent_log_id = persistent_log_id(17002)?;
    let storage_identity = database_storage_identity(persistent_log_id)?;
    let wal_path = directory.path().join("wal.bin");
    let page_store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("completeness");
    let mut owner = analyzed_database_owner(directory.path(), persistent_log_id, storage_identity)?;

    assert_eq!(append_committed_transaction(&mut owner)?, 1);
    append_committed_page(&mut owner, 171, 1, [0x17, 0x02])?;
    let mut checkpoint = FileRestartCheckpointCompletenessBaselineSource::create_new_database(
        &slot_path,
        storage_identity,
    )?;
    let _receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    drop((owner, checkpoint));

    let opened = open_transaction_page_storage_with_completeness_checkpoint::<2, _, _, _>(
        &wal_path,
        &page_store_path,
        &slot_path,
    )?;
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        opened.select_restart_checkpoint_completeness()
    else {
        return Err(io::Error::other("V5 reclamation checkpoint was not selected").into());
    };
    let planned = selected.plan_replay_window()?;
    let TransactionPageStorageRestartCheckpointRepairPreparation::Prepared(prepared) =
        planned.prepare_page_repairs()
    else {
        return Err(io::Error::other("V5 reclamation preparation failed").into());
    };
    let TransactionPageStorageRestartCheckpointPageRepairExecution::Repaired(repaired) =
        prepared.execute_page_repairs()
    else {
        return Err(io::Error::other("V5 reclamation page execution failed").into());
    };
    let TransactionPageStorageRestartCheckpointRestoration::Restored(restored) =
        repaired.restore_transaction_state()
    else {
        return Err(io::Error::other("V5 reclamation restoration failed").into());
    };
    let reclaimed = restored
        .complete_restart()?
        .analyze_wal_retention()?
        .reclaim_wal_prefix()
        .map_err(|failed| io::Error::other(format!("{:?}", failed.error())))?;
    let receipt = reclaimed.reclamation_receipt();
    assert_eq!(receipt.source_physical_format_version(), 5);
    assert_eq!(receipt.replacement_physical_format_version(), 5);
    assert_eq!(receipt.old_generation(), 0);
    assert_eq!(receipt.new_generation(), 1);
    let (coordinator, log, store, evidence) = reclaimed.into_parts();
    assert_eq!(log.physical_format_version(), 5);
    assert_eq!(
        log.database_file_identity(),
        Some(storage_identity.file_header_identity(DatabaseFileRole::Wal))
    );
    assert_eq!(store.physical_format_version(), 2);
    assert_eq!(
        store.database_file_identity(),
        Some(storage_identity.file_header_identity(DatabaseFileRole::PageStore))
    );
    drop((coordinator, log, store, evidence));

    let log = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
    assert_eq!(log.physical_format_version(), 5);
    assert_eq!(
        log.database_file_identity(),
        Some(storage_identity.file_header_identity(DatabaseFileRole::Wal))
    );
    drop(log);
    let checkpoint = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
    assert_eq!(checkpoint.control_format_version(), 2);
    assert_eq!(
        checkpoint.database_file_identity(),
        Some(storage_identity.file_header_identity(DatabaseFileRole::RestartCheckpoint))
    );
    Ok(())
}

#[test]
fn empty_wal_suffix_preserves_high_water_and_future_allocation() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("wal-reclamation-empty")?;
    let persistent_log_id = persistent_log_id(17002)?;
    let wal_path = directory.path().join("wal.bin");
    let page_store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("completeness");
    let analyzed = prepare_reclamation_owner(directory.path(), persistent_log_id, None)?;
    let mut model = reclamation_model(persistent_log_id, None)?;
    assert_eq!(analyzed.retention_analysis().durable_frontier(), Some(1));
    assert_eq!(
        analyzed
            .retention_analysis()
            .floor()
            .retained_first_record(),
        None
    );

    let reclaimed = analyzed
        .reclaim_wal_prefix()
        .map_err(|failed| io::Error::other(format!("{:?}", failed.error())))?;
    model.reclaim()?;
    let receipt = reclaimed.reclamation_receipt();
    assert_eq!(receipt.source_physical_format_version(), 3);
    assert_eq!(receipt.replacement_physical_format_version(), 4);
    assert_eq!(receipt.old_generation(), 0);
    assert_eq!(receipt.new_generation(), 1);
    assert_eq!(receipt.retained_first_logical_record(), None);
    assert_eq!(receipt.logical_position_high_water(), Some(1));
    assert_eq!(receipt.retained_logical_record_count(), 0);
    assert_eq!(receipt.retained_physical_unit_count(), 0);
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let (coordinator, mut log, store, evidence) = reclaimed.into_parts();
    assert_file_subject_matches_model(&mut log, &store, &model, "empty-suffix reclamation")?;
    assert!(log.durable_records().next().is_none());
    assert_eq!(
        log.durable_position().map(|position| position.get()),
        Some(1)
    );
    drop((coordinator, log, store, evidence));

    model.crash()?;
    model.reopen()?;
    let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
    let store = FilePageStore::<2>::open(&page_store_path)?;
    assert_file_subject_matches_model(&mut reopened, &store, &model, "empty-suffix fresh reopen")?;
    assert_eq!(reopened.physical_format_version(), 4);
    assert_eq!(reopened.generation(), 1);
    assert!(reopened.durable_records().next().is_none());
    assert_eq!(
        reopened.durable_position().map(|position| position.get()),
        Some(1)
    );
    model.select()?;
    assert_eq!(model.plan_replay()?, 0);
    model.repair_pages()?;
    let restoration = model.restore_transactions()?;
    assert_eq!(restoration.fresh_epoch, 3);
    model.complete()?;
    drop(store);
    let mut coordinator = TransactionCoordinator::open(&mut reopened)?;
    let active = coordinator.begin()?;
    assert_eq!(active.transaction_id().epoch().get(), 3);
    let committed = coordinator.commit(active, &mut reopened)?;
    assert_eq!(committed.log_position().get(), 2);
    reopened.flush_through(committed.log_position())?;
    drop((coordinator, reopened));

    let transaction = model.begin_transaction()?;
    let committed_position = model.append_transaction_commit(transaction)?;
    assert_eq!(committed_position.get(), 2);
    model.flush_wal()?;
    model.crash()?;
    model.reopen()?;

    let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
    let store = FilePageStore::<2>::open(&page_store_path)?;
    assert_file_subject_matches_model(
        &mut reopened,
        &store,
        &model,
        "empty-suffix continuation with divergent V4 header baseline",
    )?;
    assert_eq!(
        reopened
            .durable_records()
            .map(|record| record.position().get())
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        reopened.durable_position().map(|position| position.get()),
        Some(2)
    );
    let checkpoint = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
    let selection = UnrecoveredTransactionPageStorage::new(reopened, store)
        .select_generation_aware_restart_checkpoint_completeness(checkpoint);
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(_selected) =
        selection
    else {
        return Err(io::Error::other(
            "continued V4 generation did not select its anchored checkpoint",
        )
        .into());
    };
    model.select()?;
    Ok(())
}

#[test]
fn corrupt_selected_v4_rejects_before_candidate_cleanup_and_matches_model()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("corrupt-selected-v4")?;
    let persistent_log_id = persistent_log_id(17003)?;
    let wal_path = directory.path().join("wal.bin");
    let page_store_path = directory.path().join("pages.bin");
    let candidate_path = directory.path().join("wal.bin.reclaim-candidate");
    let analyzed = prepare_reclamation_owner(
        directory.path(),
        persistent_log_id,
        Some((173, [0x17, 0x03])),
    )?;
    let reclaimed = analyzed
        .reclaim_wal_prefix()
        .map_err(|failed| io::Error::other(format!("{:?}", failed.error())))?;
    let mut model = reclamation_model(persistent_log_id, Some((173, [0x17, 0x03])))?;
    model.reclaim()?;
    drop(reclaimed);

    let selected_bytes = fs::read(&wal_path)?;
    write_synced_new(&candidate_path, &selected_bytes)?;
    let mut corrupt_bytes = selected_bytes.clone();
    let first = corrupt_bytes
        .first_mut()
        .ok_or_else(|| io::Error::other("selected V4 WAL is unexpectedly empty"))?;
    *first ^= 0xFF;
    write_synced_replace(&wal_path, &corrupt_bytes)?;

    model.crash()?;
    model.set_selected_entries_for_open(
        ModelSelectedEntryState::Corrupt,
        ModelSelectedEntryState::Valid,
    )?;
    assert!(matches!(
        model.reopen(),
        Err(ModelError::InvalidSelectedOnOpen { .. })
    ));
    assert!(
        FileCommitLog::<2>::open_transaction_page_capable(&wal_path).is_err(),
        "corrupt selected V4 WAL unexpectedly opened"
    );
    assert_eq!(
        fs::read(&candidate_path)?,
        selected_bytes,
        "candidate changed before the corrupt selected generation was rejected"
    );

    write_synced_replace(&wal_path, &selected_bytes)?;
    model.set_selected_entries_for_open(
        ModelSelectedEntryState::Valid,
        ModelSelectedEntryState::Valid,
    )?;
    model.reopen()?;
    let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
    let store = FilePageStore::<2>::open(&page_store_path)?;
    assert!(
        !candidate_path.exists(),
        "fresh open did not clean the unselected candidate"
    );
    assert_file_subject_matches_model(
        &mut reopened,
        &store,
        &model,
        "restored selected V4 after corrupt-open rejection",
    )?;
    Ok(())
}

#[test]
fn every_wal_reclamation_fault_reopens_one_selected_generation() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("wal-reclamation-faults")?;
    for (index, fault) in [
        FaultPoint::BeforeReclamationCandidateCleanup,
        FaultPoint::BeforeReclamationWrite,
        FaultPoint::DuringReclamationCopy,
        FaultPoint::BeforeReclamationCandidateSync,
        FaultPoint::AfterReclamationCandidateSync,
        FaultPoint::BeforeReclamationRename,
        FaultPoint::AfterReclamationRename,
        FaultPoint::DuringReclamationDirectorySync,
    ]
    .into_iter()
    .enumerate()
    {
        let case_path = directory.path().join(format!("case-{index}"));
        fs::create_dir(&case_path)?;
        let persistent_log_id = persistent_log_id(17100 + index as u128)?;
        let wal_path = case_path.join("wal.bin");
        let old_wal_alias_path = case_path.join("wal-old-inode.bin");
        let slot_path = case_path.join("completeness");
        let candidate_path = case_path.join("wal.bin.reclaim-candidate");
        let mut analyzed = prepare_reclamation_owner(
            &case_path,
            persistent_log_id,
            Some((171 + index as u64, [0x17, index as u8])),
        )?;
        fs::hard_link(&wal_path, &old_wal_alias_path)?;
        let mut model = reclamation_model(
            persistent_log_id,
            Some((171 + index as u64, [0x17, index as u8])),
        )?;
        if fault == FaultPoint::BeforeReclamationCandidateCleanup {
            write_synced_new(&candidate_path, b"stale unselected candidate")?;
        }
        analyzed.parts_mut().1.arm_fault(fault)?;

        let failed = analyzed
            .reclaim_wal_prefix()
            .err()
            .ok_or_else(|| io::Error::other("injected reclamation fault reported success"))?;
        assert!(matches!(
            failed.error(),
            DurableTransactionRestartWalReclamationError::OutcomeIndeterminate(
                DurableTransactionRestartWalReclamationOutcomeIndeterminateError::Effect(
                    FileTransactionRestartWalReclamationError::InjectedFault(actual)
                )
            ) if *actual == fault
        ));
        assert_file_composition_locked(&case_path, &slot_path)?;
        let renamed = matches!(
            fault,
            FaultPoint::AfterReclamationRename | FaultPoint::DuringReclamationDirectorySync
        );
        assert_eq!(candidate_path.exists(), !renamed);
        if renamed {
            assert!(
                FileCommitLog::<2>::open_transaction_page_capable(&old_wal_alias_path).is_err(),
                "post-rename failure released the old WAL inode lock at {fault}"
            );
        }
        drop(failed);

        if renamed {
            model.reclaim()?;
        }
        model.crash()?;
        model.reopen()?;
        let mut reopened = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
        let store = FilePageStore::<2>::open(case_path.join("pages.bin"))?;
        assert_file_subject_matches_model(
            &mut reopened,
            &store,
            &model,
            &format!("reclamation fault {fault}"),
        )?;
        assert_eq!(
            reopened.physical_format_version(),
            if renamed { 4 } else { 3 }
        );
        assert_eq!(reopened.generation(), u64::from(renamed));
        assert_eq!(
            reopened
                .durable_records()
                .map(|record| record.position().get())
                .collect::<Vec<_>>(),
            if renamed { vec![2, 3] } else { vec![1, 2, 3] }
        );
        assert!(
            !candidate_path.exists(),
            "a candidate survived fresh selected-file open after {fault}"
        );
        drop((reopened, store));
        let reopened = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
        assert_eq!(
            reopened.physical_format_version(),
            if renamed { 4 } else { 3 }
        );
        assert_eq!(reopened.generation(), u64::from(renamed));
        assert!(
            !candidate_path.exists(),
            "candidate cleanup did not converge after repeated reopen for {fault}"
        );
    }
    Ok(())
}

#[test]
fn filesystem_replay_page_repair_faults_retry_from_page_one_under_all_three_locks()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("replay-repair-faults")?;
    for (index, fault) in [
        PageStoreFaultPoint::BeforeWrite,
        PageStoreFaultPoint::AfterWrite,
    ]
    .into_iter()
    .enumerate()
    {
        let case_path = directory.path().join(format!("case-{index}"));
        fs::create_dir(&case_path)?;
        let persistent_log_id = persistent_log_id(16600 + index as u128)?;
        let wal_path = case_path.join("wal.bin");
        let page_store_path = case_path.join("pages.bin");
        let slot_path = case_path.join("completeness");
        let suffix_page = PageNumber::new(166 + index as u64)
            .ok_or_else(|| io::Error::other("repair fault suffix page is zero"))?;

        let mut owner = analyzed_owner(&case_path, persistent_log_id)?;
        let baseline =
            owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        assert!(baseline.pages().is_empty());
        let mut checkpoint = FileRestartCheckpointCompletenessBaselineSource::create_new(
            &slot_path,
            persistent_log_id,
        )?;
        let receipt = owner.publish_restart_checkpoint_completeness_baseline_from_current_prefix(
            &mut checkpoint,
        )?;
        assert_eq!(receipt.durable_frontier(), None);
        append_committed_page_without_store_flush(
            &mut owner,
            suffix_page.get(),
            1,
            [0x16, index as u8],
        )?;
        let mut model =
            page_repair_model(persistent_log_id, suffix_page.get(), [0x16, index as u8])?;
        let current_frontier = owner
            .parts()
            .0
            .durable_position()
            .ok_or_else(|| io::Error::other("repair fault suffix is not durable"))?;
        drop(owner);
        drop(checkpoint);

        let log = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
        let mut store = FilePageStore::<2>::open(&page_store_path)?;
        store.arm_fault(fault)?;
        let checkpoint = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
            selection
        else {
            return Err(io::Error::other("repair fault checkpoint was not selected").into());
        };
        assert_file_composition_locked(&case_path, &slot_path)?;
        let planned = selected.plan_replay_window()?;
        assert_eq!(planned.current_frontier(), Some(current_frontier.get()));
        assert_eq!(planned.replay_record_count(), 2);
        let TransactionPageStorageRestartCheckpointRepairPreparation::Prepared(prepared) =
            planned.prepare_page_repairs()
        else {
            return Err(io::Error::other("repair fault preparation failed").into());
        };
        assert_eq!(prepared.page_count(), 1);
        assert_eq!(prepared.repair_candidate_count(), 1);
        let before_attempt = fs::read(&page_store_path)?;

        let TransactionPageStorageRestartCheckpointPageRepairExecution::Failed(failed) =
            prepared.execute_page_repairs()
        else {
            return Err(io::Error::other("filesystem repair fault did not fire").into());
        };
        assert!(failed.completed_prefix().is_empty());
        assert!(failed.has_indeterminate_result());
        assert!(matches!(
            failed.cause(),
            TransactionRestartCheckpointPageRepairFailureCause::StoreWrite {
                page_number,
                source:
                    FileRestartCheckpointPageRepairStoreError::PageStore(
                        FilePageStoreError::InjectedFault(actual)
                    ),
            } if *page_number == suffix_page && *actual == fault
        ));
        assert_file_composition_locked(&case_path, &slot_path)?;
        let after_failure = fs::read(&page_store_path)?;
        match fault {
            PageStoreFaultPoint::BeforeWrite => {
                assert_eq!(after_failure, before_attempt);
                assert_eq!(model.repair_pages_fault(), Err(ModelError::RepairFault));
            }
            PageStoreFaultPoint::AfterWrite => {
                assert_ne!(after_failure, before_attempt);
                assert!(matches!(
                    model.repair_pages_applied_fault(),
                    ModelApplied::AppliedThenError { .. }
                ));
            }
        }

        let TransactionPageStorageRestartCheckpointPageRepairExecution::Repaired(repaired) =
            failed.retry()
        else {
            return Err(io::Error::other("filesystem repair whole-plan retry failed").into());
        };
        let expected = match fault {
            PageStoreFaultPoint::BeforeWrite => {
                TransactionRestartCheckpointPageRepairOutcome::Repaired {
                    page_number: suffix_page,
                }
            }
            PageStoreFaultPoint::AfterWrite => {
                TransactionRestartCheckpointPageRepairOutcome::TargetAlreadyPresent {
                    page_number: suffix_page,
                }
            }
        };
        assert_eq!(repaired.page_outcomes(), [expected]);
        assert_file_composition_locked(&case_path, &slot_path)?;
        let after_retry = fs::read(&page_store_path)?;
        assert_ne!(after_retry, before_attempt);
        if fault == PageStoreFaultPoint::AfterWrite {
            assert_eq!(after_retry, after_failure);
        } else {
            model.repair_pages()?;
        }
        model.restore_transactions()?;
        model.complete()?;
        model.analyze_retention()?;
        let TransactionPageStorageRestartCheckpointRestoration::Restored(restored) =
            repaired.restore_transaction_state()
        else {
            return Err(io::Error::other("repair retry restoration failed").into());
        };
        assert_file_composition_locked(&case_path, &slot_path)?;
        assert_eq!(restored.transaction_summary().transaction_count(), 1);
        assert_eq!(restored.transaction_summary().committed_count(), 1);
        assert_eq!(restored.transaction_summary().coordinator_epoch().get(), 2);
        let completed = restored.complete_restart()?;
        assert_file_composition_locked(&case_path, &slot_path)?;
        assert_eq!(completed.completion_evidence().page_outcomes().len(), 1);
        let retained = completed.analyze_wal_retention()?;
        assert_file_composition_locked(&case_path, &slot_path)?;
        assert_eq!(retained.retention_analysis().store_page_count(), 1);
        assert_eq!(
            retained.retention_analysis().allocated_epoch_high_water(),
            2
        );
        let (coordinator, mut log, store, completion_evidence, retention_analysis, checkpoint) =
            retained.into_parts();
        assert_file_subject_matches_model(
            &mut log,
            &store,
            &model,
            &format!("page repair fault {fault:?}"),
        )?;
        assert_file_composition_locked(&case_path, &slot_path)?;
        assert_eq!(
            completion_evidence.current_frontier(),
            Some(current_frontier.get())
        );
        let stored = store
            .page(suffix_page)
            .ok_or_else(|| io::Error::other("repaired filesystem page is missing after reopen"))?;
        assert_eq!(stored.page_version(), PageVersion::new(1));
        assert_eq!(stored.bytes(), &[0x16, index as u8]);
        drop((
            coordinator,
            log,
            store,
            completion_evidence,
            retention_analysis,
            checkpoint,
        ));

        let store = FilePageStore::<2>::open(&page_store_path)?;
        let stored = store
            .page(suffix_page)
            .ok_or_else(|| io::Error::other("repaired filesystem page is missing after release"))?;
        assert_eq!(stored.page_version(), PageVersion::new(1));
        assert_eq!(stored.bytes(), &[0x16, index as u8]);
        drop(store);
        drop(FileCommitLog::<2>::open_transaction_page_capable(
            &wal_path,
        )?);
        drop(FileRestartCheckpointCompletenessBaselineSource::open(
            &slot_path,
        )?);
    }
    Ok(())
}

#[test]
fn failed_page_repair_preparation_retains_filesystem_locks_through_fallback()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("failed-repair-preparation")?;
    let persistent_log_id = persistent_log_id(16301)?;
    let wal_path = directory.path().join("wal.bin");
    let page_store_path = directory.path().join("pages.bin");
    let slot_path = directory.path().join("completeness");
    let suffix_page =
        PageNumber::new(163).ok_or_else(|| io::Error::other("suffix page is zero"))?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let baseline = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    assert!(baseline.transactions().is_empty());
    assert!(baseline.pages().is_empty());
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;
    let receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(receipt.durable_frontier(), None);
    append_committed_page_without_store_flush(&mut owner, suffix_page.get(), 1, [0x16, 0x31])?;
    let current_frontier = owner
        .parts()
        .0
        .durable_position()
        .ok_or_else(|| io::Error::other("failed preparation suffix is not durable"))?;
    drop(owner);
    drop(checkpoint);

    let log = FileCommitLog::<2>::open_transaction_page_capable(&wal_path)?;
    let store = OneShotObservationFaultFilePageStore::new(
        FilePageStore::<2>::open(&page_store_path)?,
        suffix_page,
    );
    let checkpoint = FileRestartCheckpointCompletenessBaselineSource::open(&slot_path)?;
    let selection = UnrecoveredTransactionPageStorage::new(log, store)
        .select_restart_checkpoint_completeness(checkpoint);
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        selection
    else {
        return Err(
            io::Error::other("filesystem repair-failure checkpoint was not selected").into(),
        );
    };
    assert_file_composition_locked(directory.path(), &slot_path)?;

    let planned = selected.plan_replay_window()?;
    assert_eq!(planned.replay_record_count(), 2);
    assert_eq!(planned.current_frontier(), Some(current_frontier.get()));
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let page_store_before_preparation = fs::read(&page_store_path)?;

    let preparation = planned.prepare_page_repairs();
    let TransactionPageStorageRestartCheckpointRepairPreparation::Failed(failed) = preparation
    else {
        return Err(io::Error::other("injected filesystem repair observation succeeded").into());
    };
    assert!(matches!(
        failed.error(),
        ntsql_transaction::DurableTransactionRestartCheckpointRepairPreparationError::StoreObservation {
            page_number,
            source,
        } if *page_number == suffix_page
            && source.to_string()
                == "injected filesystem repair preparation observation failure"
    ));
    assert_eq!(fs::read(&page_store_path)?, page_store_before_preparation);
    assert_file_composition_locked(directory.path(), &slot_path)?;

    let (uncheckpointed, error) = failed.continue_with_full_recovery()?;
    assert!(matches!(
        error,
        ntsql_transaction::DurableTransactionRestartCheckpointRepairPreparationError::StoreObservation {
            page_number,
            ..
        } if page_number == suffix_page
    ));
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let recovered = uncheckpointed.recover()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert!(recovered.recovery_report().pages().iter().any(|page| {
        matches!(
            page,
            CommittedTransactionPageRecoveryOutcome::Recovered { .. }
        ) && page.page_number() == suffix_page
    }));
    let analyzed = recovered.analyze_restart()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let (log, store, _, analysis, checkpoint) = analyzed.into_parts();
    assert_eq!(
        analysis
            .durable_frontier()
            .map(ntsql_wal::LogSequenceNumber::get),
        Some(current_frontier.get())
    );
    let store = store.into_inner();
    let stored = store
        .page(suffix_page)
        .ok_or_else(|| io::Error::other("fallback did not recover suffix page"))?;
    assert_eq!(stored.page_version(), PageVersion::new(1));
    assert_eq!(stored.bytes(), &[0x16, 0x31]);
    drop((log, store, checkpoint));

    drop(FileCommitLog::<2>::open_transaction_page_capable(
        &wal_path,
    )?);
    drop(FilePageStore::<2>::open(&page_store_path)?);
    drop(FileRestartCheckpointCompletenessBaselineSource::open(
        &slot_path,
    )?);
    Ok(())
}

#[test]
fn published_completeness_validates_after_safe_suffix_and_rejects_advanced_selected_page()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("stale-suffix")?;
    let persistent_log_id = persistent_log_id(15702)?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let slot_path = directory.path().join("completeness");
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;

    let selected_page = 157_u64;
    append_committed_page(&mut owner, selected_page, 1, [0x15, 0x72])?;
    let published = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let published_bytes = encode_restart_checkpoint_completeness_baseline(&published)?;
    let _receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(fs::read(slot_path.join("current"))?, published_bytes);
    let selected_frontier = published
        .durable_frontier()
        .ok_or_else(|| io::Error::other("published completeness baseline has no frontier"))?;

    append_committed_page(&mut owner, 158, 1, [0x15, 0x73])?;
    let advanced_prefix =
        owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    assert!(
        advanced_prefix
            .durable_frontier()
            .is_some_and(|frontier| frontier > selected_frontier)
    );
    assert_eq!(
        owner.validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)?,
        Some(published.clone())
    );

    append_committed_page(&mut owner, selected_page, 2, [0x15, 0x74])?;
    let advanced = owner
        .validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)
        .err()
        .ok_or_else(|| io::Error::other("advanced selected page validated unchanged"))?;
    let DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError::BaselineValidation(
        advanced,
    ) = &advanced
    else {
        return Err(io::Error::other("advanced page failed as a checkpoint-source error").into());
    };
    let DurableTransactionRestartCheckpointCompletenessBaselineValidationError::Evidence(evidence) =
        advanced.as_ref()
    else {
        return Err(io::Error::other("advanced page did not fail as evidence").into());
    };
    let DurableTransactionRestartCheckpointCompletenessBaselineValidationEvidenceError::CompletenessEvidence(
        completeness,
    ) = evidence.as_ref()
    else {
        return Err(io::Error::other("advanced page changed evidence category").into());
    };
    let DurableTransactionRestartCompletenessError::Evidence(completeness) = completeness.as_ref()
    else {
        return Err(io::Error::other("advanced page changed completeness category").into());
    };
    let DurableTransactionRestartCompletenessEvidenceError::SnapshotBeyondFrontier {
        page_number,
        position,
        frontier,
    } = completeness.as_ref()
    else {
        return Err(io::Error::other("advanced page was not a snapshot-frontier failure").into());
    };
    assert_eq!(page_number.get(), selected_page);
    assert_eq!(*frontier, selected_frontier);
    assert!(*position > selected_frontier);
    assert_eq!(fs::read(slot_path.join("current"))?, published_bytes);

    drop(owner);
    drop(checkpoint);
    let opened = open_transaction_page_storage_with_completeness_checkpoint::<2, _, _, _>(
        directory.path().join("wal.bin"),
        directory.path().join("pages.bin"),
        &slot_path,
    )?;
    let selection = opened.select_restart_checkpoint_completeness();
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Rejected(rejected) =
        selection
    else {
        return Err(io::Error::other(
            "advanced selected page checkpoint was not rejected before recovery",
        )
        .into());
    };
    assert!(matches!(
        rejected.error(),
        DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError::BaselineValidation(
            validation
        ) if matches!(
            validation.as_ref(),
            DurableTransactionRestartCheckpointCompletenessBaselineValidationError::Evidence(
                evidence
            ) if matches!(
                evidence.as_ref(),
                DurableTransactionRestartCheckpointCompletenessBaselineValidationEvidenceError::CompletenessEvidence(
                    completeness
                ) if matches!(
                    completeness.as_ref(),
                    DurableTransactionRestartCompletenessError::Evidence(completeness)
                        if matches!(
                            completeness.as_ref(),
                            DurableTransactionRestartCompletenessEvidenceError::SnapshotBeyondFrontier {
                                page_number,
                                ..
                            } if page_number.get() == selected_page
                        )
                )
            )
        )
    ));
    let (uncheckpointed, rejection) = rejected.continue_with_full_recovery()?;
    assert!(matches!(
        rejection,
        DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError::BaselineValidation(_)
    ));
    let recovered = uncheckpointed.recover()?;
    assert!(recovered.recovery_report().pages().iter().all(|page| {
        matches!(
            page,
            CommittedTransactionPageRecoveryOutcome::AlreadyCurrent { .. }
        )
    }));
    let mut analyzed = recovered.analyze_restart()?;
    let replacement =
        analyzed.publish_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    assert_eq!(replacement.persistent_log_id(), persistent_log_id);
    assert_eq!(replacement.page_count(), 2);
    Ok(())
}

#[test]
fn every_completeness_publication_fault_has_exact_effect_then_fresh_success()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("publication-faults")?;
    let points = [
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeCandidateCleanup,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateCleanup,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateCreate,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateWrite,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateSync,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCurrentReplace,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterDirectorySync,
    ];

    for (index, point) in points.into_iter().enumerate() {
        let case_path = directory.path().join(format!("case-{index}"));
        fs::create_dir(&case_path)?;
        let persistent_log_id = persistent_log_id(15800 + index as u128)?;
        let mut owner = analyzed_owner(&case_path, persistent_log_id)?;
        let slot_path = case_path.join("completeness");
        let mut checkpoint = FileRestartCheckpointCompletenessBaselineSource::create_new(
            &slot_path,
            persistent_log_id,
        )?;

        let old = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let old_bytes = encode_restart_checkpoint_completeness_baseline(&old)?;
        let _final_receipt = owner
            .publish_restart_checkpoint_completeness_baseline_from_current_prefix(
                &mut checkpoint,
            )?;
        append_committed_page(&mut owner, 300 + index as u64, 1, [0x15, index as u8])?;
        let expected =
            owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let expected_bytes = encode_restart_checkpoint_completeness_baseline(&expected)?;
        let mut model = checkpoint_publication_model(
            persistent_log_id,
            300 + index as u64,
            [0x15, index as u8],
        )?;
        write_synced_new(&slot_path.join("candidate"), b"stale")?;

        checkpoint.arm_publication_fault(point)?;
        let error = owner
            .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)
            .err()
            .ok_or_else(|| io::Error::other(format!("fault {point} reported success")))?;
        let DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError::Publication(
            failure,
        ) = &error
        else {
            return Err(io::Error::other(format!("fault {point} became preparation")).into());
        };
        assert_eq!(
            failure.cause(),
            &FileRestartCheckpointCompletenessBaselinePublicationError::InjectedFault(point)
        );
        assert_eq!(
            failure.publication().persistent_log_id(),
            expected.persistent_log_id()
        );
        assert_eq!(
            failure.publication().durable_frontier(),
            expected.durable_frontier()
        );
        assert_eq!(
            failure.publication().transaction_count(),
            expected.transactions().len()
        );
        assert_eq!(failure.publication().page_count(), expected.pages().len());
        assert!(Error::source(&error).is_some());
        assert!(Error::source(failure).is_some());
        assert!(Error::source(failure.cause()).is_none());
        assert_eq!(checkpoint.armed_publication_fault(), None);
        advance_checkpoint_publication_model(&mut model, point)?;

        let candidate_path = slot_path.join("candidate");
        let current_path = slot_path.join("current");
        match point {
            FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeCandidateCleanup => {
                assert_eq!(fs::read(&candidate_path)?, b"stale");
                assert_eq!(fs::read(&current_path)?, old_bytes);
            }
            FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateCleanup => {
                assert!(!candidate_path.exists());
                assert_eq!(fs::read(&current_path)?, old_bytes);
            }
            FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateCreate => {
                assert_eq!(fs::read(&candidate_path)?, []);
                assert_eq!(fs::read(&current_path)?, old_bytes);
            }
            FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateWrite
            | FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateSync => {
                assert_eq!(fs::read(&candidate_path)?, expected_bytes);
                assert_eq!(fs::read(&current_path)?, old_bytes);
            }
            FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCurrentReplace
            | FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterDirectorySync => {
                assert!(!candidate_path.exists());
                assert_eq!(fs::read(&current_path)?, expected_bytes);
            }
        }
        assert_eq!(
            file_checkpoint_candidate_entry(&candidate_path, &expected_bytes)?,
            model_checkpoint_candidate_entry(&model),
            "checkpoint candidate state contradicted the model after {point}"
        );
        let loaded = checkpoint
            .load_restart_checkpoint_completeness_baseline()?
            .ok_or_else(|| io::Error::other("checkpoint current file disappeared after fault"))?;
        let model_checkpoint = model
            .checkpoint_slot()
            .ok_or_else(|| io::Error::other("model checkpoint current slot disappeared"))?;
        assert_eq!(
            loaded.transactions().durable_frontier(),
            model_checkpoint
                .frontier
                .position()
                .map(|position| position.get()),
            "checkpoint frontier contradicted the model after {point}"
        );
        assert_eq!(
            loaded.pages().len(),
            model_checkpoint.pages.len(),
            "checkpoint page count contradicted the model after {point}"
        );

        let receipt = owner.publish_restart_checkpoint_completeness_baseline_from_current_prefix(
            &mut checkpoint,
        )?;
        assert_eq!(receipt.persistent_log_id(), expected.persistent_log_id());
        assert_eq!(receipt.durable_frontier(), expected.durable_frontier());
        assert_eq!(receipt.transaction_count(), expected.transactions().len());
        assert_eq!(receipt.page_count(), expected.pages().len());
        assert_eq!(fs::read(current_path)?, expected_bytes);
        assert!(!candidate_path.exists());
        assert_eq!(
            owner.validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)?,
            Some(expected)
        );
    }
    Ok(())
}

#[test]
fn wrong_slot_rejects_before_candidate_effect_or_fault_consumption() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("wrong-slot")?;
    let owner_log_id = persistent_log_id(15901)?;
    let slot_log_id = persistent_log_id(15902)?;
    let mut owner = analyzed_owner(directory.path(), owner_log_id)?;
    let expected = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let slot_path = directory.path().join("completeness");
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, slot_log_id)?;
    write_synced_new(&slot_path.join("candidate"), b"retained")?;
    checkpoint.arm_publication_fault(
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeCandidateCleanup,
    )?;
    let already_armed = checkpoint
        .arm_publication_fault(
            FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterDirectorySync,
        )
        .err()
        .ok_or_else(|| io::Error::other("armed publication fault was replaced"))?;
    assert_eq!(
        already_armed.armed(),
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeCandidateCleanup
    );
    assert_eq!(
        already_armed.requested(),
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterDirectorySync
    );

    let error = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)
        .err()
        .ok_or_else(|| io::Error::other("wrong completeness slot accepted publication"))?;
    let DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError::Publication(
        failure,
    ) = &error
    else {
        return Err(io::Error::other("wrong slot became preparation failure").into());
    };
    assert_eq!(
        failure.cause(),
        &FileRestartCheckpointCompletenessBaselinePublicationError::SlotPersistentLogIdMismatch {
            slot: slot_log_id,
            baseline: owner_log_id,
        }
    );
    assert_eq!(
        failure.publication().persistent_log_id(),
        expected.persistent_log_id()
    );
    assert_eq!(failure.publication().page_count(), expected.pages().len());
    assert_eq!(fs::read(slot_path.join("candidate"))?, b"retained");
    assert!(!slot_path.join("current").exists());
    assert_eq!(
        checkpoint.armed_publication_fault(),
        Some(
            FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeCandidateCleanup
        )
    );
    assert!(Error::source(failure.cause()).is_none());
    Ok(())
}

#[test]
fn candidate_and_replace_io_failures_preserve_exact_stage_without_delete_fallback()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("publication-io")?;
    let persistent_log_id = persistent_log_id(16001)?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let slot_path = directory.path().join("completeness");
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;
    let old = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let old_bytes = encode_restart_checkpoint_completeness_baseline(&old)?;
    let _old_receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    append_committed_page(&mut owner, 160, 1, [0x16, 0x01])?;
    let expected = owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let expected_bytes = encode_restart_checkpoint_completeness_baseline(&expected)?;

    fs::create_dir(slot_path.join("candidate"))?;
    let cleanup_error = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)
        .err()
        .ok_or_else(|| io::Error::other("candidate directory was removed as a file"))?;
    assert_publication_io_stage(
        &cleanup_error,
        FileRestartCheckpointSlotIoStage::RemoveCandidateFile,
    )?;
    assert_eq!(fs::read(slot_path.join("current"))?, old_bytes);
    assert!(slot_path.join("candidate").is_dir());
    fs::remove_dir(slot_path.join("candidate"))?;

    fs::remove_file(slot_path.join("current"))?;
    fs::create_dir(slot_path.join("current"))?;
    let replace_error = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)
        .err()
        .ok_or_else(|| io::Error::other("publisher replaced a directory as current"))?;
    assert_publication_io_stage(
        &replace_error,
        FileRestartCheckpointSlotIoStage::ReplaceCurrentFile,
    )?;
    assert!(slot_path.join("current").is_dir());
    assert_eq!(fs::read(slot_path.join("candidate"))?, expected_bytes);

    fs::remove_dir(slot_path.join("current"))?;
    let _final_receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(fs::read(slot_path.join("current"))?, expected_bytes);
    assert!(!slot_path.join("candidate").exists());
    assert_eq!(
        owner.validate_restart_checkpoint_completeness_baseline_from_source(&mut checkpoint)?,
        Some(expected)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn stale_candidate_symlink_is_unlinked_without_following_target() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new("candidate-symlink")?;
    let persistent_log_id = persistent_log_id(16101)?;
    let mut owner = analyzed_owner(directory.path(), persistent_log_id)?;
    let slot_path = directory.path().join("completeness");
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;
    let sentinel_path = directory.path().join("sentinel");
    write_synced_new(&sentinel_path, b"sentinel")?;
    std::os::unix::fs::symlink(&sentinel_path, slot_path.join("candidate"))?;

    let _receipt = owner
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    assert_eq!(fs::read(sentinel_path)?, b"sentinel");
    assert!(!slot_path.join("candidate").exists());
    assert!(slot_path.join("current").is_file());
    Ok(())
}

fn assert_publication_io_stage(
    error: &FilePublicationError,
    expected: FileRestartCheckpointSlotIoStage,
) -> Result<(), io::Error> {
    let DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError::Publication(
        failure,
    ) = error
    else {
        return Err(io::Error::other(
            "filesystem completeness publication I/O error became preparation",
        ));
    };
    let FileRestartCheckpointCompletenessBaselinePublicationError::Io(source) = failure.cause()
    else {
        return Err(io::Error::other(
            "filesystem completeness publication I/O error changed category",
        ));
    };
    if source.stage() != expected {
        return Err(io::Error::other(format!(
            "filesystem completeness publication I/O stage {:?} did not match {expected:?}",
            source.stage()
        )));
    }
    if Error::source(error).is_none()
        || Error::source(failure).is_none()
        || Error::source(failure.cause()).is_none()
        || Error::source(source).is_none()
    {
        return Err(io::Error::other(
            "filesystem completeness publication I/O cause chain is incomplete",
        ));
    }
    Ok(())
}

fn reclamation_model(
    persistent_log_id: PersistentLogId,
    retained_page: Option<(u64, [u8; 2])>,
) -> Result<RecoveryModel, Box<dyn Error>> {
    let log_id = ModelLogId::new(persistent_log_id.get())
        .ok_or_else(|| io::Error::other("model persistent log ID is zero"))?;
    let mut model = RecoveryModel::new(log_id);
    let anchor = ModelCheckpointAnchor::new(1, persistent_log_id.get());

    if retained_page.is_none() {
        model.publish_checkpoint(anchor)?;
    }

    model.allocate_coordinator_epoch()?;
    let transaction = model.begin_transaction()?;
    model.append_transaction_commit(transaction)?;
    model.flush_wal()?;

    if let Some((page_number, bytes)) = retained_page {
        model.allocate_coordinator_epoch()?;
        let transaction = model.begin_transaction()?;
        let page = ModelPageId::new(page_number)
            .ok_or_else(|| io::Error::other("model page number is zero"))?;
        let page_position = model.append_transaction_page(
            transaction,
            page,
            model_page_value(bytes),
            ModelPageVersion::new(1),
        )?;
        model.append_transaction_commit(transaction)?;
        model.flush_wal()?;
        model.write_page_store(page_position)?;
        model.publish_checkpoint(anchor)?;
    }

    model.crash()?;
    model.reopen()?;
    model.select()?;
    model.plan_replay()?;
    model.repair_pages()?;
    model.restore_transactions()?;
    model.complete()?;
    model.analyze_retention()?;
    Ok(model)
}

fn page_repair_model(
    persistent_log_id: PersistentLogId,
    page_number: u64,
    bytes: [u8; 2],
) -> Result<RecoveryModel, Box<dyn Error>> {
    let log_id = ModelLogId::new(persistent_log_id.get())
        .ok_or_else(|| io::Error::other("model persistent log ID is zero"))?;
    let mut model = RecoveryModel::new(log_id);
    model.publish_checkpoint(ModelCheckpointAnchor::new(1, persistent_log_id.get()))?;
    model.allocate_coordinator_epoch()?;
    let transaction = model.begin_transaction()?;
    let page = ModelPageId::new(page_number)
        .ok_or_else(|| io::Error::other("model page number is zero"))?;
    model.append_transaction_page(
        transaction,
        page,
        model_page_value(bytes),
        ModelPageVersion::new(1),
    )?;
    model.append_transaction_commit(transaction)?;
    model.flush_wal()?;
    model.crash()?;
    model.reopen()?;
    model.select()?;
    model.plan_replay()?;
    Ok(model)
}

fn checkpoint_publication_model(
    persistent_log_id: PersistentLogId,
    page_number: u64,
    bytes: [u8; 2],
) -> Result<RecoveryModel, Box<dyn Error>> {
    let log_id = ModelLogId::new(persistent_log_id.get())
        .ok_or_else(|| io::Error::other("model persistent log ID is zero"))?;
    let mut model = RecoveryModel::new(log_id);
    model.publish_checkpoint(ModelCheckpointAnchor::new(1, persistent_log_id.get()))?;
    model.allocate_coordinator_epoch()?;
    let transaction = model.begin_transaction()?;
    let page = ModelPageId::new(page_number)
        .ok_or_else(|| io::Error::other("model page number is zero"))?;
    let position = model.append_transaction_page(
        transaction,
        page,
        model_page_value(bytes),
        ModelPageVersion::new(1),
    )?;
    model.append_transaction_commit(transaction)?;
    model.flush_wal()?;
    model.write_page_store(position)?;

    let mut replacement = model.clone();
    replacement.publish_checkpoint(ModelCheckpointAnchor::new(
        1,
        persistent_log_id
            .get()
            .checked_add(1)
            .ok_or_else(|| io::Error::other("model checkpoint anchor overflow"))?,
    ))?;
    let snapshot = replacement
        .checkpoint_slot()
        .cloned()
        .ok_or_else(|| io::Error::other("model replacement checkpoint is absent"))?;
    model.begin_checkpoint_candidate(snapshot)?;
    Ok(model)
}

fn advance_checkpoint_publication_model(
    model: &mut RecoveryModel,
    point: FileRestartCheckpointCompletenessBaselinePublicationFaultPoint,
) -> Result<(), Box<dyn Error>> {
    let steps = match point {
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeCandidateCleanup => {
            model.set_checkpoint_candidate_entry(ModelCandidateEntry::Corrupt)?;
            0
        }
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateCleanup => 1,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateCreate => 2,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateWrite => 3,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCandidateSync => 5,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterCurrentReplace => 7,
        FileRestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterDirectorySync => 9,
    };
    for _ in 0..steps {
        model.advance_checkpoint_candidate()?;
    }
    Ok(())
}

fn model_checkpoint_candidate_entry(model: &RecoveryModel) -> ModelCandidateEntry {
    match model.checkpoint_candidate() {
        ModelCheckpointCandidateState::Absent => ModelCandidateEntry::Absent,
        ModelCheckpointCandidateState::Present { entry, .. } => entry.clone(),
    }
}

fn file_checkpoint_candidate_entry(
    path: &Path,
    expected_bytes: &[u8],
) -> Result<ModelCandidateEntry, io::Error> {
    if !path.exists() {
        return Ok(ModelCandidateEntry::Absent);
    }
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        Ok(ModelCandidateEntry::PartialWrite)
    } else if bytes == expected_bytes {
        Ok(ModelCandidateEntry::Valid)
    } else {
        Ok(ModelCandidateEntry::Corrupt)
    }
}

fn assert_file_subject_matches_model(
    log: &mut FileCommitLog<2>,
    store: &FilePageStore<2>,
    model: &RecoveryModel,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    let source =
        DurableTransactionRestartWalReclamationSource::observe_restart_wal_reclamation_source(log)?;
    let mut contradictions = Vec::new();
    let actual_log_id = LogDurability::lineage(log)
        .persistent_id()
        .map(PersistentLogId::get);
    compare_field(
        &mut contradictions,
        "persistent_log_id",
        Some(model.log_id().get()),
        actual_log_id,
    );
    compare_field(
        &mut contradictions,
        "physical_format_version",
        model.wal_format_version(),
        source.physical_format_version(),
    );
    compare_field(
        &mut contradictions,
        "generation",
        model.wal_generation().get(),
        source.source_generation(),
    );
    compare_field(
        &mut contradictions,
        "replacement_header_retained_first",
        model.retained_first().map(|position| position.get()),
        log.replacement_header_retained_first()
            .map(|position| position.get()),
    );
    compare_field(
        &mut contradictions,
        "replacement_header_logical_high_water",
        model
            .replacement_logical_high_water()
            .map(|position| position.get()),
        log.replacement_header_logical_high_water()
            .map(|position| position.get()),
    );
    compare_field(
        &mut contradictions,
        "replacement_header_allocated_epoch_high_water",
        model.replacement_epoch_high_water(),
        log.replacement_header_allocated_epoch_high_water()
            .map(|epoch| epoch.get()),
    );
    compare_field(
        &mut contradictions,
        "current_retained_first",
        model
            .durable_wal_records()
            .next()
            .map(|record| record.position.get()),
        source
            .retained_first_logical_record()
            .map(|position| position.get()),
    );
    compare_field(
        &mut contradictions,
        "logical_high_water",
        model.logical_high_water().map(|position| position.get()),
        source
            .logical_position_high_water()
            .map(|position| position.get()),
    );
    compare_field(
        &mut contradictions,
        "next_logical_position",
        model.next_logical_position(),
        log.next_logical_position(),
    );
    compare_field(
        &mut contradictions,
        "allocated_epoch_high_water",
        model.epoch_high_water(),
        Some(source.allocated_epoch_high_water().get()),
    );
    compare_field(
        &mut contradictions,
        "generation_anchor_presence",
        model.generation_anchor().is_some(),
        source.selected_checkpoint_anchor_version().is_some()
            && source.selected_checkpoint_anchor_value().is_some(),
    );

    let expected_records = normalized_model_records(model)?;
    let actual_records = normalized_file_records(log)?;
    compare_field(
        &mut contradictions,
        "durable_records",
        expected_records,
        actual_records,
    );

    let expected_pages = normalized_model_pages(model);
    let actual_pages = normalized_file_pages(store);
    compare_field(
        &mut contradictions,
        "stored_pages",
        expected_pages,
        actual_pages,
    );

    if contradictions.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{context} contradicted the recovery model:\n{}",
            contradictions.join("\n")
        ))
        .into())
    }
}

fn normalized_model_records(model: &RecoveryModel) -> Result<Vec<NormalizedRecord>, io::Error> {
    model
        .durable_wal_records()
        .map(|record| {
            let kind = match record.kind {
                ModelWalRecordKind::TransactionCommit => {
                    let transaction = record.transaction.ok_or_else(|| {
                        io::Error::other("model commit has no transaction identity")
                    })?;
                    NormalizedRecordKind::TransactionCommit {
                        epoch: transaction.epoch(),
                        sequence: transaction.sequence(),
                    }
                }
                ModelWalRecordKind::RawPage => NormalizedRecordKind::RawPage {
                    page: record
                        .page
                        .ok_or_else(|| io::Error::other("model raw page has no page identity"))?
                        .get(),
                    version: record
                        .page_version
                        .ok_or_else(|| io::Error::other("model raw page has no version"))?
                        .get(),
                    value: record
                        .page_value
                        .ok_or_else(|| io::Error::other("model raw page has no value"))?,
                },
                ModelWalRecordKind::TransactionPage => {
                    let transaction = record
                        .transaction
                        .ok_or_else(|| io::Error::other("model transaction page has no owner"))?;
                    NormalizedRecordKind::TransactionPage {
                        epoch: transaction.epoch(),
                        sequence: transaction.sequence(),
                        page: record
                            .page
                            .ok_or_else(|| {
                                io::Error::other("model transaction page has no page identity")
                            })?
                            .get(),
                        version: record
                            .page_version
                            .ok_or_else(|| {
                                io::Error::other("model transaction page has no version")
                            })?
                            .get(),
                        value: record.page_value.ok_or_else(|| {
                            io::Error::other("model transaction page has no value")
                        })?,
                    }
                }
            };
            Ok(NormalizedRecord {
                position: record.position.get(),
                kind,
            })
        })
        .collect()
}

fn normalized_file_records(log: &FileCommitLog<2>) -> Result<Vec<NormalizedRecord>, io::Error> {
    log.durable_records()
        .map(|record| {
            let kind = if let (Some(epoch), Some(sequence)) =
                (record.transaction_epoch(), record.transaction_sequence())
            {
                NormalizedRecordKind::TransactionCommit { epoch, sequence }
            } else {
                let page = record
                    .page_write()
                    .ok_or_else(|| io::Error::other("filesystem page record has no payload"))?;
                match (
                    record.page_owner_transaction_epoch(),
                    record.page_owner_transaction_sequence(),
                ) {
                    (Some(epoch), Some(sequence)) => NormalizedRecordKind::TransactionPage {
                        epoch,
                        sequence,
                        page: page.page_number().get(),
                        version: page.page_version().get(),
                        value: model_page_value(*page.bytes()),
                    },
                    (None, None) => NormalizedRecordKind::RawPage {
                        page: page.page_number().get(),
                        version: page.page_version().get(),
                        value: model_page_value(*page.bytes()),
                    },
                    _ => {
                        return Err(io::Error::other(
                            "filesystem page owner identity is incomplete",
                        ));
                    }
                }
            };
            Ok(NormalizedRecord {
                position: record.position().get(),
                kind,
            })
        })
        .collect()
}

fn normalized_model_pages(model: &RecoveryModel) -> Vec<NormalizedPage> {
    model
        .page_store()
        .values()
        .map(|page| NormalizedPage {
            page: page.page_id.get(),
            version: page.version.get(),
            value: page.value,
            required_position: page.written_at.get(),
        })
        .collect()
}

fn normalized_file_pages(store: &FilePageStore<2>) -> Vec<NormalizedPage> {
    let mut pages = store
        .pages()
        .iter()
        .map(|page| NormalizedPage {
            page: page.page_number().get(),
            version: page.page_version().get(),
            value: model_page_value(*page.bytes()),
            required_position: page.required_position().get(),
        })
        .collect::<Vec<_>>();
    pages.sort();
    pages
}

fn compare_field<T: fmt::Debug + PartialEq>(
    contradictions: &mut Vec<String>,
    field: &str,
    expected: T,
    actual: T,
) {
    if expected != actual {
        contradictions.push(format!("{field}: expected {expected:?}, actual {actual:?}"));
    }
}

const fn model_page_value(bytes: [u8; 2]) -> u64 {
    (bytes[0] as u64) << 8 | bytes[1] as u64
}

fn analyzed_owner(
    directory: &Path,
    persistent_log_id: PersistentLogId,
) -> Result<FileOwner, Box<dyn Error>> {
    let log = FileCommitLog::<2>::create_new_transaction_page_capable(
        directory.join("wal.bin"),
        persistent_log_id,
    )?;
    let store = FilePageStore::<2>::create_new(directory.join("pages.bin"), persistent_log_id)?;
    Ok(UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?)
}

fn analyzed_database_owner(
    directory: &Path,
    persistent_log_id: PersistentLogId,
    storage_identity: DatabaseStorageIdentity,
) -> Result<FileOwner, Box<dyn Error>> {
    let log = FileCommitLog::<2>::create_new_database_transaction_page_capable(
        directory.join("wal.bin"),
        storage_identity,
    )?;
    let store =
        FilePageStore::<2>::create_new_database(directory.join("pages.bin"), storage_identity)?;
    if log.persistent_id() != persistent_log_id || store.persistent_id() != persistent_log_id {
        return Err(io::Error::other("database child persistent log ID changed").into());
    }
    Ok(UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?)
}

fn database_storage_identity(
    persistent_log_id: PersistentLogId,
) -> Result<DatabaseStorageIdentity, Box<dyn Error>> {
    let database_id =
        DatabaseId::new(17_002).ok_or_else(|| io::Error::other("test database ID is zero"))?;
    let files = [
        DatabaseFileIdentity::new(
            DatabaseFileRole::Wal,
            DatabaseFileId::new(27_002)
                .ok_or_else(|| io::Error::other("test WAL file ID is zero"))?,
        ),
        DatabaseFileIdentity::new(
            DatabaseFileRole::PageStore,
            DatabaseFileId::new(37_002)
                .ok_or_else(|| io::Error::other("test page file ID is zero"))?,
        ),
        DatabaseFileIdentity::new(
            DatabaseFileRole::RestartCheckpoint,
            DatabaseFileId::new(47_002)
                .ok_or_else(|| io::Error::other("test checkpoint file ID is zero"))?,
        ),
    ];
    Ok(DatabaseStorageIdentity::new(
        database_id,
        persistent_log_id,
        &files,
    )?)
}

fn append_committed_page(
    owner: &mut FileOwner,
    page_number: u64,
    page_version: u64,
    bytes: [u8; 2],
) -> Result<(), Box<dyn Error>> {
    let (log, store) = owner.parts_mut();
    let lineage = LogDurability::lineage(log).clone();
    let mut coordinator = TransactionCoordinator::open(log)?;
    let active = coordinator.begin()?;
    let (active, dirty) = coordinator.stage_page_write(
        active,
        unlogged_page(&lineage, page_number, page_version, bytes)?,
        log,
    )?;
    let committed = coordinator.commit(active, log)?;
    flush_committed_page(&committed, log, store, dirty)?;
    Ok(())
}

fn append_committed_transaction(owner: &mut FileOwner) -> Result<u64, Box<dyn Error>> {
    let (log, _) = owner.parts_mut();
    let mut coordinator = TransactionCoordinator::open(log)?;
    let active = coordinator.begin()?;
    let committed = coordinator.commit(active, log)?;
    let position = committed.log_position().clone();
    log.flush_through(&position)?;
    Ok(position.get())
}

fn prepare_reclamation_owner(
    directory: &Path,
    persistent_log_id: PersistentLogId,
    retained_page: Option<(u64, [u8; 2])>,
) -> Result<FileReclamationOwner, Box<dyn Error>> {
    let mut owner = analyzed_owner(directory, persistent_log_id)?;
    let slot_path = directory.join("completeness");
    let mut checkpoint =
        FileRestartCheckpointCompletenessBaselineSource::create_new(&slot_path, persistent_log_id)?;
    if retained_page.is_none() {
        let _ = owner.publish_restart_checkpoint_completeness_baseline_from_current_prefix(
            &mut checkpoint,
        )?;
    }
    assert_eq!(append_committed_transaction(&mut owner)?, 1);
    if let Some((page_number, bytes)) = retained_page {
        append_committed_page(&mut owner, page_number, 1, bytes)?;
        let _ = owner.publish_restart_checkpoint_completeness_baseline_from_current_prefix(
            &mut checkpoint,
        )?;
    }
    drop((owner, checkpoint));

    let opened = open_transaction_page_storage_with_completeness_checkpoint::<2, _, _, _>(
        directory.join("wal.bin"),
        directory.join("pages.bin"),
        &slot_path,
    )?;
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        opened.select_restart_checkpoint_completeness()
    else {
        return Err(io::Error::other("reclamation helper checkpoint was not selected").into());
    };
    let planned = selected.plan_replay_window()?;
    let TransactionPageStorageRestartCheckpointRepairPreparation::Prepared(prepared) =
        planned.prepare_page_repairs()
    else {
        return Err(io::Error::other("reclamation helper preparation failed").into());
    };
    let TransactionPageStorageRestartCheckpointPageRepairExecution::Repaired(repaired) =
        prepared.execute_page_repairs()
    else {
        return Err(io::Error::other("reclamation helper page execution failed").into());
    };
    let TransactionPageStorageRestartCheckpointRestoration::Restored(restored) =
        repaired.restore_transaction_state()
    else {
        return Err(io::Error::other("reclamation helper restoration failed").into());
    };
    Ok(restored.complete_restart()?.analyze_wal_retention()?)
}

fn append_committed_page_without_store_flush(
    owner: &mut FileOwner,
    page_number: u64,
    page_version: u64,
    bytes: [u8; 2],
) -> Result<(), Box<dyn Error>> {
    let (log, _) = owner.parts_mut();
    let lineage = LogDurability::lineage(log).clone();
    let mut coordinator = TransactionCoordinator::open(log)?;
    let active = coordinator.begin()?;
    let (active, dirty) = coordinator.stage_page_write(
        active,
        unlogged_page(&lineage, page_number, page_version, bytes)?,
        log,
    )?;
    let committed = coordinator.commit(active, log)?;
    drop((committed, dirty));
    Ok(())
}

fn assert_file_composition_locked(directory: &Path, slot_path: &Path) -> Result<(), io::Error> {
    if FileCommitLog::<2>::open_transaction_page_capable(directory.join("wal.bin")).is_ok() {
        return Err(io::Error::other(
            "filesystem selection released its WAL lock",
        ));
    }
    if FilePageStore::<2>::open(directory.join("pages.bin")).is_ok() {
        return Err(io::Error::other(
            "filesystem selection released its page-store lock",
        ));
    }
    if FileRestartCheckpointCompletenessBaselineSource::open(slot_path).is_ok() {
        return Err(io::Error::other(
            "filesystem selection released its completeness-control lock",
        ));
    }
    Ok(())
}

fn unlogged_page(
    lineage: &ntsql_wal::LogLineage,
    page_number: u64,
    page_version: u64,
    bytes: [u8; 2],
) -> Result<UnloggedPage<2>, Box<dyn Error>> {
    let page_number =
        PageNumber::new(page_number).ok_or_else(|| io::Error::other("page number is zero"))?;
    Ok(UnloggedPage::new(
        PageAddress::new(lineage, page_number),
        PageVersion::new(page_version),
        PageImage::new(bytes)?,
    ))
}

fn persistent_log_id(value: u128) -> Result<PersistentLogId, io::Error> {
    PersistentLogId::new(value).ok_or_else(|| io::Error::other("persistent log ID is zero"))
}

fn write_synced_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn write_synced_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(name: &str) -> io::Result<Self> {
        let unique = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ntsql-completeness-file-publication-{}-{name}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
