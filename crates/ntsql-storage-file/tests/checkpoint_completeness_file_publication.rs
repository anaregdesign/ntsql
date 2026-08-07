use std::{
    cell::Cell,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use ntsql_page::{
    PageAddress, PageImage, PageNumber, PageVersion, StoredPageSnapshotObservation, UnloggedPage,
};
use ntsql_storage_file::{
    FileCommitLog, FileCommittedPageRecoveryObservationError, FilePageStore,
    FileRestartCheckpointCompletenessBaselinePublicationError,
    FileRestartCheckpointCompletenessBaselinePublicationFaultPoint,
    FileRestartCheckpointCompletenessBaselineSource, FileRestartCheckpointSlotIoStage,
    FileTransactionRestartAnalysisSourceError, encode_restart_checkpoint_completeness_baseline,
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
    DurableTransactionRestartPageState, RestartAnalyzedTransactionPageStorage,
    TransactionCoordinator, TransactionPageStorageRestartCheckpointCompletenessSelection,
    TransactionPageStorageRestartCheckpointRepairPreparation, UnrecoveredTransactionPageStorage,
    flush_committed_page,
};
use ntsql_wal::{LogDurability, LogLineage, PersistentLogId};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

type FileOwner = RestartAnalyzedTransactionPageStorage<FileCommitLog<2>, FilePageStore<2>, 2>;

type FilePublicationError =
    DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError<
        FileTransactionRestartAnalysisSourceError<2>,
        FileCommittedPageRecoveryObservationError<2>,
        FileRestartCheckpointCompletenessBaselinePublicationError,
    >;

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

    let uncheckpointed = selected.decline_checkpoint();
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

    let uncheckpointed = prepared.decline_page_repairs();
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let recovered = uncheckpointed.recover()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert!(recovered.recovery_report().pages().iter().any(|page| {
        matches!(
            page,
            CommittedTransactionPageRecoveryOutcome::Recovered { .. }
        ) && page.page_number().get() == suffix_page
    }));
    let analyzed = recovered.analyze_restart()?;
    assert_file_composition_locked(directory.path(), &slot_path)?;
    assert_eq!(
        analyzed
            .restart_analysis()
            .durable_frontier()
            .map(ntsql_wal::LogSequenceNumber::get),
        Some(current_frontier.get())
    );

    let (log, store, _, _, checkpoint) = analyzed.into_parts();
    assert_file_composition_locked(directory.path(), &slot_path)?;
    let suffix_page =
        PageNumber::new(suffix_page).ok_or_else(|| io::Error::other("suffix page is zero"))?;
    let stored = store
        .page(suffix_page)
        .ok_or_else(|| io::Error::other("planned suffix was not recovered"))?;
    assert_eq!(stored.page_version(), PageVersion::new(2));
    assert_eq!(stored.bytes(), &[0x16, 0x12]);
    assert_eq!(checkpoint.persistent_log_id(), persistent_log_id);
    drop((log, store, checkpoint));

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

    let (uncheckpointed, error) = failed.continue_with_full_recovery();
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
    let (uncheckpointed, rejection) = rejected.continue_with_full_recovery();
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
