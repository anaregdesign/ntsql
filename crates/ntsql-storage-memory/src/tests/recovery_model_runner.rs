use std::{error::Error, io};

use ntsql_page::{PageAddress, PageImage, PageLog};
use ntsql_recovery_model::{
    CI_SEEDS, CheckpointAnchor, LogId, PageId as ModelPageId, PageVersion as ModelPageVersion,
    RecoveryModel, RecoveryPhase, TransactionId as ModelTransactionId, WalRecordKind,
};
use ntsql_transaction::{
    DurableTransactionRestartAnalysisSource, DurableTransactionRestartCheckpointBaselineSource,
    DurableTransactionRestartCheckpointCompletenessBaselineSource,
    DurableTransactionRestartPrunedGenerationSource, DurableTransactionRestartWalReclamationError,
    DurableTransactionRestartWalReclamationOutcomeIndeterminateError,
    RestartAnalyzedTransactionPageStorage, TransactionCoordinator,
    TransactionPageStorageRestartCheckpointCompletenessSelection,
    TransactionPageStorageRestartCheckpointPageRepairExecution,
    TransactionPageStorageRestartCheckpointRepairPreparation,
    TransactionPageStorageRestartCheckpointRestoration, UnrecoveredTransactionPageStorage,
};
use ntsql_wal::{LogDurability, PersistentLogId};

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnedPhase {
    Recovery,
    Live,
}

#[derive(Debug, Eq, PartialEq)]
struct RecordFact {
    position: u64,
    kind: WalRecordKind,
    transaction: Option<(u64, u64)>,
    page: Option<u64>,
    value: Option<u64>,
    version: Option<u64>,
}

#[derive(Debug, Eq, PartialEq)]
struct PageFact {
    page: u64,
    version: u64,
    value: u64,
    required_position: u64,
}

#[derive(Debug, Eq, PartialEq)]
struct OwnedObservation {
    log_id: u128,
    phase: RecoveryPhase,
    ownership: OwnedPhase,
    generation: u64,
    // Model and adapter digests are intentionally opaque, separate namespaces.
    // Presence is the comparable fact; the canonical trace separately checks
    // that the adapter's exact tuple survives reopen unchanged.
    generation_anchor_present: bool,
    retained_first: Option<u64>,
    logical_high_water: Option<u64>,
    next_logical_position: Option<u64>,
    epoch_high_water: Option<u64>,
    records: Vec<RecordFact>,
    pages: Vec<PageFact>,
    checkpoint_frontier: Option<Option<u64>>,
}

fn trace_context(seed: u64, operation: &str) -> String {
    format!("seed={seed}, prefix=[{operation}]")
}

fn model_log_id(value: u128) -> Result<LogId, Box<dyn Error>> {
    LogId::new(value)
        .ok_or_else(|| io::Error::other("model log identity is zero"))
        .map_err(Into::into)
}

fn persistent_log_id(value: u128) -> Result<PersistentLogId, Box<dyn Error>> {
    PersistentLogId::new(value)
        .ok_or_else(|| io::Error::other("memory log identity is zero"))
        .map_err(Into::into)
}

fn model_page_id(value: u64) -> Result<ModelPageId, Box<dyn Error>> {
    ModelPageId::new(value)
        .ok_or_else(|| io::Error::other("model page identity is zero"))
        .map_err(Into::into)
}

fn subject_page(
    log: &InMemoryCommitLog<1>,
    page: u64,
    version: u64,
    value: u8,
) -> Result<UnloggedPage<1>, Box<dyn Error>> {
    let page_number =
        PageNumber::new(page).ok_or_else(|| io::Error::other("subject page identity is zero"))?;
    Ok(UnloggedPage::new(
        PageAddress::new(LogDurability::lineage(log), page_number),
        PageVersion::new(version),
        PageImage::new([value])?,
    ))
}

fn transaction_fact(transaction: ntsql_transaction::TransactionId) -> (u64, u64) {
    (transaction.epoch().get(), transaction.sequence())
}

fn model_transaction_fact(transaction: ModelTransactionId) -> (u64, u64) {
    (transaction.epoch(), transaction.sequence())
}

fn observe_model(model: &RecoveryModel, ownership: OwnedPhase) -> OwnedObservation {
    let records: Vec<RecordFact> = model
        .wal_records()
        .iter()
        .filter(|record| {
            model
                .flush_position()
                .is_some_and(|frontier| record.position <= frontier)
        })
        .map(|record| RecordFact {
            position: record.position.get(),
            kind: record.kind,
            transaction: record.transaction.map(model_transaction_fact),
            page: record.page.map(ModelPageId::get),
            value: record.page_value,
            version: record.page_version.map(ModelPageVersion::get),
        })
        .collect();
    let pages = model
        .page_store()
        .values()
        .map(|page| PageFact {
            page: page.page_id.get(),
            version: page.version.get(),
            value: page.value,
            required_position: page.written_at.get(),
        })
        .collect();
    OwnedObservation {
        log_id: model.log_id().get(),
        phase: model.phase(),
        ownership,
        generation: model.wal_generation().get(),
        generation_anchor_present: model.generation_anchor().is_some(),
        retained_first: records.first().map(|record| record.position),
        logical_high_water: model.logical_high_water().map(|position| position.get()),
        next_logical_position: model.next_logical_position(),
        epoch_high_water: model.epoch_high_water(),
        records,
        pages,
        checkpoint_frontier: model.checkpoint_slot().map(|checkpoint| {
            checkpoint
                .frontier
                .position()
                .map(|position| position.get())
        }),
    }
}

fn observe_subject(
    log: &mut InMemoryCommitLog<1>,
    store: &InMemoryPageStore<1>,
    phase: RecoveryPhase,
    ownership: OwnedPhase,
    checkpoint_frontier: Option<Option<u64>>,
) -> Result<OwnedObservation, Box<dyn Error>> {
    let metadata =
        DurableTransactionRestartPrunedGenerationSource::<1>::observe_restart_pruned_generation(
            log,
        )?;
    let mut records = Vec::new();
    for record in log.durable_records() {
        let (kind, transaction, page, value, version) = match record.kind() {
            InMemoryLogRecordKind::TransactionCommit { transaction_id } => (
                WalRecordKind::TransactionCommit,
                Some(transaction_fact(*transaction_id)),
                None,
                None,
                None,
            ),
            InMemoryLogRecordKind::PageWrite(page) => (
                WalRecordKind::RawPage,
                None,
                Some(page.page_number().get()),
                Some(u64::from(page.bytes()[0])),
                Some(page.page_version().get()),
            ),
            InMemoryLogRecordKind::TransactionPageWrite(page) => (
                WalRecordKind::TransactionPage,
                Some(transaction_fact(page.transaction_id())),
                Some(page.page_write().page_number().get()),
                Some(u64::from(page.page_write().bytes()[0])),
                Some(page.page_write().page_version().get()),
            ),
        };
        records.push(RecordFact {
            position: record.position().get(),
            kind,
            transaction,
            page,
            value,
            version,
        });
    }
    let mut pages = store
        .pages()
        .iter()
        .map(|page| PageFact {
            page: page.page_number().get(),
            version: page.page_version().get(),
            value: u64::from(page.bytes()[0]),
            required_position: page.required_position().get(),
        })
        .collect::<Vec<_>>();
    pages.sort_by_key(|page| page.page);
    let log_id = LogDurability::lineage(log)
        .persistent_id()
        .ok_or_else(|| io::Error::other("subject log has no persistent identity"))?
        .get();
    Ok(OwnedObservation {
        log_id,
        phase,
        ownership,
        generation: metadata.source_generation(),
        generation_anchor_present: metadata.selected_checkpoint_anchor_version().is_some()
            && metadata.selected_checkpoint_anchor_value().is_some(),
        retained_first: metadata
            .retained_first_logical_record()
            .map(|position| position.get()),
        logical_high_water: metadata
            .logical_position_high_water()
            .map(|position| position.get()),
        next_logical_position: log.next_logical_position(),
        epoch_high_water: Some(metadata.allocated_epoch_high_water().get()),
        records,
        pages,
        checkpoint_frontier,
    })
}

fn compare_owned(
    seed: u64,
    operation: &str,
    expected: &OwnedObservation,
    actual: &OwnedObservation,
) -> Result<(), Box<dyn Error>> {
    let mut contradictions = Vec::new();
    macro_rules! compare {
        ($field:ident) => {
            if expected.$field != actual.$field {
                contradictions.push(format!(
                    "{}: expected {:?}, got {:?}",
                    stringify!($field),
                    expected.$field,
                    actual.$field
                ));
            }
        };
    }
    compare!(log_id);
    compare!(phase);
    compare!(ownership);
    compare!(generation);
    compare!(generation_anchor_present);
    compare!(retained_first);
    compare!(logical_high_water);
    compare!(next_logical_position);
    compare!(epoch_high_water);
    compare!(records);
    compare!(pages);
    compare!(checkpoint_frontier);
    if contradictions.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{}\n{}",
            trace_context(seed, operation),
            contradictions.join("\n")
        ))
        .into())
    }
}

fn compare_pair(
    seed: u64,
    operation: &str,
    model: &RecoveryModel,
    log: &mut InMemoryCommitLog<1>,
    store: &InMemoryPageStore<1>,
    phase: RecoveryPhase,
    checkpoint_frontier: Option<Option<u64>>,
) -> Result<(), Box<dyn Error>> {
    let ownership = if phase == RecoveryPhase::Unrecovered {
        OwnedPhase::Recovery
    } else {
        OwnedPhase::Live
    };
    let expected = observe_model(model, ownership);
    let actual = observe_subject(log, store, phase, ownership, checkpoint_frontier)?;
    compare_owned(seed, operation, &expected, &actual)
}

fn seed_payload(seed: u64) -> u8 {
    seed.rotate_left(17).to_le_bytes()[0]
}

fn clean_checkpoint_owner(
    persistent_log_id: PersistentLogId,
    _page_number: u64,
    _value: u8,
) -> Result<
    RestartAnalyzedTransactionPageStorage<InMemoryCommitLog<1>, InMemoryPageStore<1>, 1>,
    Box<dyn Error>,
> {
    let mut log = InMemoryCommitLog::with_persistent_lineage_id(persistent_log_id);
    let store = InMemoryPageStore::new(&log);
    log.allocate_transaction_epoch()?;
    log.reopen()?;
    Ok(UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?)
}

fn clean_model(
    log_id: LogId,
    _page_number: u64,
    _value: u8,
) -> Result<RecoveryModel, Box<dyn Error>> {
    let mut model = RecoveryModel::new(log_id);
    model.allocate_coordinator_epoch()?;
    Ok(model)
}

fn stored_page_checkpoint_owner(
    persistent_log_id: PersistentLogId,
    page_number: u64,
    value: u8,
) -> Result<
    RestartAnalyzedTransactionPageStorage<InMemoryCommitLog<1>, InMemoryPageStore<1>, 1>,
    Box<dyn Error>,
> {
    let mut log = InMemoryCommitLog::with_persistent_lineage_id(persistent_log_id);
    let mut store = InMemoryPageStore::new(&log);
    log.allocate_transaction_epoch()?;
    let page = subject_page(&log, page_number, 1, value)?;
    let dirty = ntsql_page::stage_page_write(&mut log, page)?;
    ntsql_page::flush_dirty_page(&mut log, &mut store, dirty)?;
    log.reopen()?;
    Ok(UnrecoveredTransactionPageStorage::new(log, store)
        .recover()?
        .analyze_restart()?)
}

fn stored_page_model(
    log_id: LogId,
    page_number: u64,
    value: u8,
) -> Result<RecoveryModel, Box<dyn Error>> {
    let mut model = RecoveryModel::new(log_id);
    model.allocate_coordinator_epoch()?;
    let position = model.append_raw_page(
        model_page_id(page_number)?,
        u64::from(value),
        ModelPageVersion::new(1),
    )?;
    model.flush_wal()?;
    model.write_page_store(position)?;
    Ok(model)
}

fn checkpoint_publication_model(
    log_id: LogId,
    first_page_number: u64,
) -> Result<RecoveryModel, Box<dyn Error>> {
    let mut model = RecoveryModel::new(log_id);
    model.allocate_coordinator_epoch()?;
    let committed = model.begin_transaction()?;
    let committed_position = model.append_transaction_page(
        committed,
        model_page_id(first_page_number)?,
        0xA1,
        ModelPageVersion::new(1),
    )?;
    model.append_transaction_commit(committed)?;
    let uncommitted = model.begin_transaction()?;
    model.append_transaction_page(
        uncommitted,
        model_page_id(first_page_number + 1)?,
        0xA2,
        ModelPageVersion::new(2),
    )?;
    model.flush_wal()?;
    model.write_page_store(committed_position)?;
    Ok(model)
}

fn complete_selected_recovery(
    selected: ntsql_transaction::SelectedTransactionPageStorageRestartCheckpointCompleteness<
        InMemoryCommitLog<1>,
        InMemoryPageStore<1>,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
        1,
    >,
) -> Result<
    ntsql_transaction::CompletedTransactionPageStorageRestartCheckpointReplay<
        InMemoryCommitLog<1>,
        InMemoryPageStore<1>,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
        1,
    >,
    Box<dyn Error>,
> {
    let planned = selected
        .plan_replay_window()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let TransactionPageStorageRestartCheckpointRepairPreparation::Prepared(prepared) =
        planned.prepare_page_repairs()
    else {
        return Err(io::Error::other("page repair preparation was rejected").into());
    };
    let TransactionPageStorageRestartCheckpointPageRepairExecution::Repaired(repaired) =
        prepared.execute_page_repairs()
    else {
        return Err(io::Error::other("page repair execution failed").into());
    };
    let TransactionPageStorageRestartCheckpointRestoration::Restored(restored) =
        repaired.restore_transaction_state()
    else {
        return Err(io::Error::other("transaction restoration failed").into());
    };
    restored.complete_restart().map_err(Into::into)
}

#[test]
fn seeded_repeated_recovery_model_comparison_after_reopen() -> Result<(), Box<dyn Error>> {
    for seed in CI_SEEDS {
        let value = seed_payload(seed);
        let id_value = 0x1710_0000_u128 + u128::from(seed);
        let persistent_id = persistent_log_id(id_value)?;
        let page_number = 100_u64
            .checked_add(seed % 10_000)
            .ok_or_else(|| io::Error::other("canonical page number overflow"))?;
        let mut subject = clean_checkpoint_owner(persistent_id, page_number, value)?;
        let mut model = clean_model(model_log_id(id_value)?, page_number, value)?;

        let baseline =
            subject.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let checkpoint_frontier = Some(baseline.durable_frontier());
        let owned_checkpoint = owned_completeness_checkpoint(&baseline);
        let mut checkpoint =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
        let _receipt = subject
            .publish_restart_checkpoint_completeness_baseline_from_current_prefix(
                &mut checkpoint,
            )?;
        let anchor = CheckpointAnchor::new(1, u128::from(seed) | 1);
        model.publish_checkpoint(anchor)?;

        let (mut log, store, _, _) = subject.into_parts();
        log.reopen()?;
        model.crash()?;
        model.reopen()?;
        compare_pair(
            seed,
            "canonical-prefix,second-reopen",
            &model,
            &mut log,
            &store,
            RecoveryPhase::Unrecovered,
            checkpoint_frontier,
        )?;
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_generation_aware_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
            selection
        else {
            return Err(io::Error::other(format!(
                "{}: current checkpoint was not selected",
                trace_context(seed, "select")
            ))
            .into());
        };
        model.select()?;
        model.plan_replay()?;
        model.repair_pages()?;
        model.restore_transactions()?;
        model.complete()?;
        let completed = complete_selected_recovery(selected)?;
        let analyzed = completed
            .analyze_wal_retention()
            .map_err(|error| io::Error::other(error.to_string()))?;
        model.analyze_retention()?;
        let reclaimed = analyzed
            .reclaim_wal_prefix()
            .map_err(|failure| io::Error::other(format!("{:?}", failure.error())))?;
        model.reclaim()?;
        assert_eq!(
            reclaimed
                .reclamation_receipt()
                .retained_logical_record_count(),
            0,
            "{}",
            trace_context(seed, "empty-retained-suffix")
        );

        let (_, mut log, store, _) = reclaimed.into_parts();
        let installed_anchor = log
            .selected_checkpoint_anchor()
            .ok_or_else(|| io::Error::other("reclamation did not install an opaque anchor"))?;
        log.reopen()?;
        model.crash()?;
        model.reopen()?;
        compare_pair(
            seed,
            "canonical-prefix,reclaim,reopen",
            &model,
            &mut log,
            &store,
            RecoveryPhase::Unrecovered,
            checkpoint_frontier,
        )?;
        assert_eq!(
            log.selected_checkpoint_anchor(),
            Some(installed_anchor),
            "{}",
            trace_context(seed, "opaque-anchor-stability")
        );
        let complete_prefix =
            DurableTransactionRestartAnalysisSource::<1>::with_durable_transaction_restart_observations(
                &mut log,
                |_frontier, _records| (),
            );
        assert!(
            matches!(
                complete_prefix,
                Err(InMemoryTransactionRestartAnalysisSourceError::PrunedGenerationRequiresCheckpoint {
                    generation: 1
                })
            ),
            "{}: pruned source exposed complete-prefix fallback",
            trace_context(seed, "pruned-fallback-denial")
        );

        let checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            owned_checkpoint.clone(),
        );
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_generation_aware_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
            selection
        else {
            return Err(io::Error::other("pruned checkpoint retry was not selected").into());
        };
        model.select()?;
        model.plan_replay()?;
        model.repair_pages()?;
        model.restore_transactions()?;
        model.complete()?;
        let completed = complete_selected_recovery(selected)?;
        let analyzed = completed
            .analyze_wal_retention()
            .map_err(|error| io::Error::other(error.to_string()))?;
        model.analyze_retention()?;
        let reclaimed = analyzed
            .reclaim_wal_prefix()
            .map_err(|failure| io::Error::other(format!("{:?}", failure.error())))?;
        model.reclaim()?;
        assert_eq!(reclaimed.reclamation_receipt().new_generation(), 2);
        let (_, mut log, store, _) = reclaimed.into_parts();
        let expected_position = model
            .logical_high_water()
            .map_or(Some(1), |position| position.get().checked_add(1))
            .ok_or_else(|| io::Error::other("continuation position exhausted"))?;
        let continuation_value = value.wrapping_add(1);
        let continuation_page = page_number + 1;
        let continuation = subject_page(&log, continuation_page, 2, continuation_value)?;
        let continuation_position = log.append_page(&continuation)?;
        assert_eq!(
            continuation_position.get(),
            expected_position,
            "{}",
            trace_context(seed, "empty-suffix-continuation")
        );
        log.flush_through(&continuation_position)?;
        log.reopen()?;
        let mut expected = observe_model(&model, OwnedPhase::Recovery);
        expected.phase = RecoveryPhase::Unrecovered;
        expected.retained_first = Some(expected_position);
        expected.logical_high_water = Some(expected_position);
        expected.next_logical_position = expected_position.checked_add(1);
        expected.records.push(RecordFact {
            position: expected_position,
            kind: WalRecordKind::RawPage,
            transaction: None,
            page: Some(continuation_page),
            value: Some(u64::from(continuation_value)),
            version: Some(2),
        });
        let actual = observe_subject(
            &mut log,
            &store,
            RecoveryPhase::Unrecovered,
            OwnedPhase::Recovery,
            checkpoint_frontier,
        )?;
        compare_owned(
            seed,
            "canonical-prefix,second-reclaim,empty-suffix-continuation,reopen",
            &expected,
            &actual,
        )?;
    }
    Ok(())
}

#[test]
fn corrected_retention_candidates_match_memory_after_reopen() -> Result<(), Box<dyn Error>> {
    for seed in CI_SEEDS {
        let value = seed_payload(seed);
        let id_value = 0x1719_0000_u128 + u128::from(seed);
        let page_number = 10_000_u64
            .checked_add(seed)
            .ok_or_else(|| io::Error::other("retention page number overflow"))?;
        let persistent_id = persistent_log_id(id_value)?;
        let mut subject = stored_page_checkpoint_owner(persistent_id, page_number, value)?;
        let baseline =
            subject.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let checkpoint_frontier = Some(baseline.durable_frontier());
        let mut checkpoint =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
        let _receipt = subject
            .publish_restart_checkpoint_completeness_baseline_from_current_prefix(
                &mut checkpoint,
            )?;
        let mut model = stored_page_model(model_log_id(id_value)?, page_number, value)?;
        model.publish_checkpoint(CheckpointAnchor::new(1, u128::from(seed) | 1))?;

        let (mut log, store, _, _) = subject.into_parts();
        log.reopen()?;
        model.crash()?;
        model.reopen()?;
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_generation_aware_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
            selection
        else {
            return Err(io::Error::other("retention checkpoint was not selected").into());
        };
        model.select()?;
        model.plan_replay()?;
        model.repair_pages()?;
        model.restore_transactions()?;
        model.complete()?;
        let completed = complete_selected_recovery(selected)?;
        let analyzed = completed
            .analyze_wal_retention()
            .map_err(|error| io::Error::other(error.to_string()))?;
        model.analyze_retention()?;
        let reclaimed = analyzed
            .reclaim_wal_prefix()
            .map_err(|failure| io::Error::other(format!("{:?}", failure.error())))?;
        model.reclaim()?;
        assert_eq!(
            reclaimed
                .reclamation_receipt()
                .retained_first_logical_record(),
            model.retained_first().map(|position| position.get()),
            "{}",
            trace_context(seed, "corrected-inclusive-retention")
        );
        assert_eq!(
            reclaimed
                .reclamation_receipt()
                .retained_logical_record_count(),
            1,
            "{}",
            trace_context(seed, "checkpoint-frontier-and-store-backing-candidate")
        );
        let (_, mut log, store, _) = reclaimed.into_parts();
        log.reopen()?;
        model.crash()?;
        model.reopen()?;
        compare_pair(
            seed,
            "corrected-inclusive-retention,reclaim,reopen",
            &model,
            &mut log,
            &store,
            RecoveryPhase::Unrecovered,
            checkpoint_frontier,
        )?;
    }
    Ok(())
}

#[test]
fn corrected_retention_omits_unbacked_post_frontier_commit() -> Result<(), Box<dyn Error>> {
    let seed = CI_SEEDS[0];
    let id_value = 0x171A_0000_u128 + u128::from(seed);
    let persistent_id = persistent_log_id(id_value)?;
    let mut subject = clean_checkpoint_owner(persistent_id, 1, 0)?;
    let baseline =
        subject.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
    let checkpoint_frontier = Some(baseline.durable_frontier());
    let mut checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
    let _receipt = subject
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut checkpoint)?;
    {
        let (log, _) = subject.parts_mut();
        let mut coordinator = TransactionCoordinator::open(log)?;
        let active = coordinator.begin()?;
        let _committed = coordinator.commit(active, log)?;
    }

    let mut model = clean_model(model_log_id(id_value)?, 1, 0)?;
    model.publish_checkpoint(CheckpointAnchor::new(1, u128::from(seed) | 1))?;
    model.allocate_coordinator_epoch()?;
    let transaction = model.begin_transaction()?;
    model.append_transaction_commit(transaction)?;
    model.flush_wal()?;

    let (mut log, store, _, _) = subject.into_parts();
    log.reopen()?;
    model.crash()?;
    model.reopen()?;
    compare_pair(
        seed,
        "empty-checkpoint,post-frontier-commit,reopen",
        &model,
        &mut log,
        &store,
        RecoveryPhase::Unrecovered,
        checkpoint_frontier,
    )?;
    let selection = UnrecoveredTransactionPageStorage::new(log, store)
        .select_generation_aware_restart_checkpoint_completeness(checkpoint);
    let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
        selection
    else {
        return Err(io::Error::other("empty-frontier checkpoint was not selected").into());
    };
    model.select()?;
    model.plan_replay()?;
    model.repair_pages()?;
    model.restore_transactions()?;
    model.complete()?;
    let completed = complete_selected_recovery(selected)?;
    let analyzed = completed
        .analyze_wal_retention()
        .map_err(|error| io::Error::other(error.to_string()))?;
    model.analyze_retention()?;
    let reclaimed = analyzed
        .reclaim_wal_prefix()
        .map_err(|failure| io::Error::other(format!("{:?}", failure.error())))?;
    model.reclaim()?;
    assert_eq!(
        reclaimed
            .reclamation_receipt()
            .retained_logical_record_count(),
        0,
        "{}",
        trace_context(seed, "unbacked-post-frontier-commit-is-not-a-candidate")
    );
    let (_, mut log, store, _) = reclaimed.into_parts();
    log.reopen()?;
    model.crash()?;
    model.reopen()?;
    compare_pair(
        seed,
        "post-frontier-commit-pruned,reopen",
        &model,
        &mut log,
        &store,
        RecoveryPhase::Unrecovered,
        checkpoint_frontier,
    )?;
    Ok(())
}

#[test]
fn wal_and_page_fault_matrix_matches_model_after_reopen() -> Result<(), Box<dyn Error>> {
    for (case, fault, applied) in [
        ("wal-before-append", FaultPoint::BeforeAppend, false),
        ("wal-after-append", FaultPoint::AfterAppend, true),
    ] {
        let id_value = 0x1711_0000_u128 + u128::from(applied);
        let mut log = InMemoryCommitLog::with_persistent_lineage_id(persistent_log_id(id_value)?);
        let store = InMemoryPageStore::new(&log);
        let mut model = RecoveryModel::new(model_log_id(id_value)?);
        log.allocate_transaction_epoch()?;
        model.allocate_coordinator_epoch()?;
        let page = subject_page(&log, 1, 1, 0x11)?;
        log.arm_fault(fault)?;
        assert!(
            log.append_page(&page).is_err(),
            "{}",
            trace_context(0, case)
        );
        if applied {
            model.append_raw_page(model_page_id(1)?, 0x11, ModelPageVersion::new(1))?;
        }
        log.reopen()?;
        model.crash()?;
        model.reopen()?;
        compare_pair(
            0,
            case,
            &model,
            &mut log,
            &store,
            RecoveryPhase::Unrecovered,
            None,
        )?;
        model.select()?;
        assert_eq!(model.plan_replay()?, 0);
        model.repair_pages()?;
        model.restore_transactions()?;
        model.complete()?;
        let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
            .recover()?
            .analyze_restart()?;
        let continuation_page = 101 + u64::from(applied);
        let (log, _) = owner.parts_mut();
        let coordinator = TransactionCoordinator::open(log)?;
        drop(coordinator);
        let continuation = subject_page(log, continuation_page, 2, 0x31)?;
        let actual_position = log.append_page(&continuation)?;
        let expected_position = model.append_raw_page(
            model_page_id(continuation_page)?,
            0x31,
            ModelPageVersion::new(2),
        )?;
        assert_eq!(actual_position.get(), expected_position.get());
        log.flush_through(&actual_position)?;
        model.flush_wal()?;
        let (mut log, store, _, _) = owner.into_parts();
        log.reopen()?;
        model.crash()?;
        model.reopen()?;
        compare_pair(
            0,
            &format!("{case}-continuation"),
            &model,
            &mut log,
            &store,
            RecoveryPhase::Unrecovered,
            None,
        )?;
    }

    for (case, fault, applied) in [
        ("wal-before-flush", FaultPoint::BeforeFlush, false),
        ("wal-after-flush", FaultPoint::AfterFlush, true),
    ] {
        let id_value = 0x1712_0000_u128 + u128::from(applied);
        let mut log = InMemoryCommitLog::with_persistent_lineage_id(persistent_log_id(id_value)?);
        let store = InMemoryPageStore::new(&log);
        let mut model = RecoveryModel::new(model_log_id(id_value)?);
        log.allocate_transaction_epoch()?;
        model.allocate_coordinator_epoch()?;
        let page = subject_page(&log, 2, 1, 0x22)?;
        let position = log.append_page(&page)?;
        model.append_raw_page(model_page_id(2)?, 0x22, ModelPageVersion::new(1))?;
        log.arm_fault(fault)?;
        assert!(
            log.flush_through(&position).is_err(),
            "{}",
            trace_context(0, case)
        );
        if applied {
            model.flush_wal()?;
        }
        log.reopen()?;
        model.crash()?;
        model.reopen()?;
        compare_pair(
            0,
            case,
            &model,
            &mut log,
            &store,
            RecoveryPhase::Unrecovered,
            None,
        )?;
        if !applied {
            model.select()?;
            assert_eq!(model.plan_replay()?, 0);
            model.repair_pages()?;
            model.restore_transactions()?;
            model.complete()?;
            let mut owner = UnrecoveredTransactionPageStorage::new(log, store)
                .recover()?
                .analyze_restart()?;
            let (log, _) = owner.parts_mut();
            let coordinator = TransactionCoordinator::open(log)?;
            drop(coordinator);
            let continuation = subject_page(log, 202, 2, 0x42)?;
            let actual_position = log.append_page(&continuation)?;
            let expected_position =
                model.append_raw_page(model_page_id(202)?, 0x42, ModelPageVersion::new(2))?;
            assert_eq!(actual_position.get(), expected_position.get());
            log.flush_through(&actual_position)?;
            model.flush_wal()?;
            let (mut log, store, _, _) = owner.into_parts();
            log.reopen()?;
            model.crash()?;
            model.reopen()?;
            compare_pair(
                0,
                &format!("{case}-continuation"),
                &model,
                &mut log,
                &store,
                RecoveryPhase::Unrecovered,
                None,
            )?;
        }
    }

    for (case, fault, applied) in [
        ("page-before-write", PageStoreFaultPoint::BeforeWrite, false),
        ("page-after-write", PageStoreFaultPoint::AfterWrite, true),
    ] {
        let id_value = 0x1713_0000_u128 + u128::from(applied);
        let mut log = InMemoryCommitLog::with_persistent_lineage_id(persistent_log_id(id_value)?);
        let mut store = InMemoryPageStore::new(&log);
        let mut model = RecoveryModel::new(model_log_id(id_value)?);
        log.allocate_transaction_epoch()?;
        model.allocate_coordinator_epoch()?;
        let page = subject_page(&log, 3, 1, 0x33)?;
        let dirty = ntsql_page::stage_page_write(&mut log, page)?;
        let model_position =
            model.append_raw_page(model_page_id(3)?, 0x33, ModelPageVersion::new(1))?;
        store.arm_fault(fault)?;
        assert!(
            ntsql_page::flush_dirty_page(&mut log, &mut store, dirty).is_err(),
            "{}",
            trace_context(0, case)
        );
        model.flush_wal()?;
        if applied {
            model.write_page_store(model_position)?;
        }
        log.reopen()?;
        model.crash()?;
        model.reopen()?;
        compare_pair(
            0,
            case,
            &model,
            &mut log,
            &store,
            RecoveryPhase::Unrecovered,
            None,
        )?;
    }
    Ok(())
}

#[test]
fn checkpoint_fault_matrix_matches_model_state() -> Result<(), Box<dyn Error>> {
    for (offset, publication_fault, applied) in [
        (
            0_u128,
            RestartCheckpointBaselinePublicationFaultPoint::BeforeReplace,
            false,
        ),
        (
            1,
            RestartCheckpointBaselinePublicationFaultPoint::AfterReplace,
            true,
        ),
    ] {
        let id_value = 0x1714_0000_u128 + offset;
        let persistent_id = persistent_log_id(id_value)?;
        let mut subject = checkpoint_publication_owner(persistent_id, 500 + offset as u64 * 2)?;
        let mut model =
            checkpoint_publication_model(model_log_id(id_value)?, 500 + offset as u64 * 2)?;
        let mut checkpoint = InMemoryTransactionRestartCheckpointBaselineSource::empty();
        checkpoint.arm_publication_fault(publication_fault)?;
        assert!(
            subject
                .publish_restart_checkpoint_baseline_from_current_prefix(&mut checkpoint)
                .is_err()
        );
        if applied {
            model.publish_checkpoint(CheckpointAnchor::new(1, id_value))?;
        }
        let (log, store) = subject.parts_mut();
        compare_pair(
            0,
            "baseline-publication-fault",
            &model,
            log,
            store,
            RecoveryPhase::Live,
            checkpoint.slot().map(|slot| slot.durable_frontier()),
        )?;
    }

    for (offset, publication_fault, applied) in [
        (
            0_u128,
            RestartCheckpointCompletenessBaselinePublicationFaultPoint::BeforeReplace,
            false,
        ),
        (
            1,
            RestartCheckpointCompletenessBaselinePublicationFaultPoint::AfterReplace,
            true,
        ),
    ] {
        let id_value = 0x1715_0000_u128 + offset;
        let persistent_id = persistent_log_id(id_value)?;
        let mut subject = checkpoint_publication_owner(persistent_id, 600 + offset as u64 * 2)?;
        let mut model =
            checkpoint_publication_model(model_log_id(id_value)?, 600 + offset as u64 * 2)?;
        let mut checkpoint =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
        checkpoint.arm_publication_fault(publication_fault)?;
        assert!(
            subject
                .publish_restart_checkpoint_completeness_baseline_from_current_prefix(
                    &mut checkpoint,
                )
                .is_err()
        );
        if applied {
            model.publish_checkpoint(CheckpointAnchor::new(1, id_value))?;
        }
        let (log, store) = subject.parts_mut();
        compare_pair(
            0,
            "completeness-publication-fault",
            &model,
            log,
            store,
            RecoveryPhase::Live,
            checkpoint
                .slot()
                .map(|slot| slot.transactions().durable_frontier()),
        )?;
    }

    let id_value = 0x1716_0000_u128;
    let persistent_id = persistent_log_id(id_value)?;
    let mut subject = checkpoint_publication_owner(persistent_id, 700)?;
    let mut model = checkpoint_publication_model(model_log_id(id_value)?, 700)?;
    let mut baseline = InMemoryTransactionRestartCheckpointBaselineSource::empty();
    let _receipt =
        subject.publish_restart_checkpoint_baseline_from_current_prefix(&mut baseline)?;
    model.publish_checkpoint(CheckpointAnchor::new(1, id_value))?;
    baseline.arm_fault(RestartCheckpointBaselineSourceFaultPoint::BeforeLoad)?;
    assert!(baseline.load_restart_checkpoint_baseline().is_err());
    let (log, store) = subject.parts_mut();
    compare_pair(
        0,
        "baseline-source-before-load",
        &model,
        log,
        store,
        RecoveryPhase::Live,
        baseline.slot().map(|slot| slot.durable_frontier()),
    )?;

    let id_value = 0x1717_0000_u128;
    let persistent_id = persistent_log_id(id_value)?;
    let mut subject = checkpoint_publication_owner(persistent_id, 800)?;
    let mut model = checkpoint_publication_model(model_log_id(id_value)?, 800)?;
    let mut completeness = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
    let _receipt = subject
        .publish_restart_checkpoint_completeness_baseline_from_current_prefix(&mut completeness)?;
    model.publish_checkpoint(CheckpointAnchor::new(1, id_value))?;
    completeness.arm_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)?;
    assert!(
        completeness
            .load_restart_checkpoint_completeness_baseline()
            .is_err()
    );
    let (log, store) = subject.parts_mut();
    compare_pair(
        0,
        "completeness-source-before-load",
        &model,
        log,
        store,
        RecoveryPhase::Live,
        completeness
            .slot()
            .map(|slot| slot.transactions().durable_frontier()),
    )?;
    Ok(())
}

#[test]
fn generation_swap_fault_oracle_matches_model_boundaries() -> Result<(), Box<dyn Error>> {
    for (offset, fault, applied) in [
        (0_u128, FaultPoint::BeforeGenerationSwap, false),
        (1_u128, FaultPoint::AfterGenerationSwap, true),
    ] {
        let id_value = 0x1718_0000_u128 + offset;
        let persistent_id = persistent_log_id(id_value)?;
        let mut owner = clean_checkpoint_owner(persistent_id, 900 + offset as u64, 0x91)?;
        let baseline =
            owner.prepare_restart_checkpoint_completeness_baseline_from_current_prefix()?;
        let checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::seeded(
            owned_completeness_checkpoint(&baseline),
        );
        let (log, store, _, _) = owner.into_parts();
        let selection = UnrecoveredTransactionPageStorage::new(log, store)
            .select_restart_checkpoint_completeness(checkpoint);
        let TransactionPageStorageRestartCheckpointCompletenessSelection::Selected(selected) =
            selection
        else {
            return Err(io::Error::other("generation fault checkpoint was not selected").into());
        };
        let completed = complete_selected_recovery(selected)?;
        let mut analyzed = completed
            .analyze_wal_retention()
            .map_err(|error| io::Error::other(error.to_string()))?;
        analyzed.parts_mut().1.arm_fault(fault)?;
        let failure = analyzed
            .reclaim_wal_prefix()
            .err()
            .ok_or_else(|| io::Error::other("generation swap fault reported success"))?;
        assert!(matches!(
            failure.error(),
            DurableTransactionRestartWalReclamationError::OutcomeIndeterminate(
                DurableTransactionRestartWalReclamationOutcomeIndeterminateError::Effect(
                    InMemoryWalReclamationSourceError::InjectedFault(observation)
                )
            ) if observation.fault_point() == fault
        ));
        let mut model = clean_model(model_log_id(id_value)?, 900 + offset as u64, 0x91)?;
        model.publish_checkpoint(CheckpointAnchor::new(1, id_value))?;
        model.crash()?;
        model.reopen()?;
        model.select()?;
        model.plan_replay()?;
        model.repair_pages()?;
        model.restore_transactions()?;
        model.complete()?;
        model.analyze_retention()?;
        if applied {
            model.reclaim()?;
        }
        let observation = match failure.error() {
            DurableTransactionRestartWalReclamationError::OutcomeIndeterminate(
                DurableTransactionRestartWalReclamationOutcomeIndeterminateError::Effect(
                    InMemoryWalReclamationSourceError::InjectedFault(observation),
                ),
            ) => observation,
            _ => {
                return Err(
                    io::Error::other("generation fault returned the wrong evidence").into(),
                );
            }
        };
        assert_eq!(
            observation.source_generation(),
            model.wal_generation().get()
        );
        assert_eq!(
            observation.retained_first(),
            model
                .wal_records()
                .first()
                .map(|record| record.position.get())
        );
        assert_eq!(
            observation.logical_high_water(),
            model.logical_high_water().map(|position| position.get())
        );
        assert_eq!(
            observation.allocated_epoch_high_water().get(),
            model
                .epoch_high_water()
                .ok_or_else(|| io::Error::other("model has no epoch high-water"))?
        );
        assert_eq!(
            observation.durable_record_count(),
            model.wal_records().len()
        );
        assert_eq!(
            observation.has_selected_checkpoint_anchor(),
            model.generation_anchor().is_some()
        );
    }
    Ok(())
}
