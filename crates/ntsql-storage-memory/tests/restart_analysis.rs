use std::{error::Error, io};

use ntsql_page::{PageAddress, PageImage, PageNumber, PageVersion, UnloggedPage, stage_page_write};
use ntsql_storage_memory::{FaultPoint, InMemoryCommitLog};
use ntsql_transaction::{
    CoordinatedCommitError, DurableTransactionRestartAnalysis,
    DurableTransactionRestartAnalysisSource, DurableTransactionRestartObservation,
    DurableTransactionRestartObservationKind, DurableTransactionRestartState,
    TransactionCoordinator, TransactionId, analyze_durable_transaction_restart,
};
use ntsql_wal::{LogDurability, LogLineage, PersistentLogId};

#[test]
fn empty_memory_prefix_has_no_frontier_or_transactions() -> Result<(), Box<dyn Error>> {
    let id = persistent_log_id(122)?;
    let mut log = InMemoryCommitLog::<2>::with_persistent_lineage_id(id);
    let lineage = LogDurability::lineage(&log).clone();
    let mut callbacks = 0;

    DurableTransactionRestartAnalysisSource::with_durable_transaction_restart_observations(
        &mut log,
        |frontier, observations| {
            callbacks += 1;
            assert_eq!(frontier, None);
            assert!(observations.is_empty());
        },
    )?;

    assert_eq!(callbacks, 1);
    let analysis = analyze_durable_transaction_restart(&mut log)?;
    assert!(analysis.lineage().same_lineage(&lineage));
    assert_eq!(analysis.durable_frontier(), None);
    assert!(analysis.transactions().is_empty());
    Ok(())
}

#[test]
fn durable_memory_prefix_projects_once_and_reopens_without_volatile_commit()
-> Result<(), Box<dyn Error>> {
    let id = persistent_log_id(123)?;
    let mut log = InMemoryCommitLog::<2>::with_persistent_lineage_id(id);
    let mut coordinator = TransactionCoordinator::open(&mut log)?;

    let committed_with_page = coordinator.begin()?;
    let committed_with_page_id = committed_with_page.transaction_id();
    let (committed_with_page, first_owned) = coordinator.stage_page_write(
        committed_with_page,
        unlogged_page(LogDurability::lineage(&log), 11, 3, [1, 1])?,
        &mut log,
    )?;
    assert_eq!(first_owned.required_position().get(), 1);

    let middle_raw_page = unlogged_page(LogDurability::lineage(&log), 12, 4, [2, 2])?;
    let middle_raw = stage_page_write(&mut log, middle_raw_page)?;
    assert_eq!(middle_raw.required_position().get(), 2);

    let committed = coordinator.commit(committed_with_page, &mut log)?;
    assert_eq!(committed.log_position().get(), 3);

    let uncommitted = coordinator.begin()?;
    let uncommitted_id = uncommitted.transaction_id();
    let (uncommitted, second_owned) = coordinator.stage_page_write(
        uncommitted,
        unlogged_page(LogDurability::lineage(&log), 13, 5, [3, 3])?,
        &mut log,
    )?;
    assert_eq!(second_owned.required_position().get(), 4);
    log.flush_through(second_owned.required_position())?;

    let commit_only = coordinator.begin()?;
    let commit_only_id = commit_only.transaction_id();
    let commit_only = coordinator.commit(commit_only, &mut log)?;
    assert_eq!(commit_only.log_position().get(), 5);

    let tail_raw_page = unlogged_page(LogDurability::lineage(&log), 14, 6, [4, 4])?;
    let tail_raw = stage_page_write(&mut log, tail_raw_page)?;
    assert_eq!(tail_raw.required_position().get(), 6);
    log.flush_through(tail_raw.required_position())?;

    log.arm_fault(FaultPoint::BeforeFlush)?;
    let volatile_commit = coordinator
        .commit(uncommitted, &mut log)
        .err()
        .ok_or_else(|| io::Error::other("volatile commit unexpectedly became durable"))?;
    assert!(matches!(
        volatile_commit,
        CoordinatedCommitError::Indeterminate(_)
    ));
    assert_eq!(log.records().len(), 7);
    assert_eq!(log.durable_records().len(), 6);
    assert_eq!(
        log.records()[6].transaction_id(),
        Some(uncommitted_id),
        "the excluded suffix must contain the classification-changing commit"
    );

    let lineage = LogDurability::lineage(&log).clone();
    let mut callbacks = 0;
    DurableTransactionRestartAnalysisSource::with_durable_transaction_restart_observations(
        &mut log,
        |frontier, observations| -> Result<(), io::Error> {
            callbacks += 1;
            assert_eq!(frontier.map(|position| position.get()), Some(6));
            assert_eq!(
                observations
                    .iter()
                    .map(|observation| observation.kind())
                    .collect::<Vec<_>>(),
                [
                    DurableTransactionRestartObservationKind::TransactionPage,
                    DurableTransactionRestartObservationKind::Page,
                    DurableTransactionRestartObservationKind::Commit,
                    DurableTransactionRestartObservationKind::TransactionPage,
                    DurableTransactionRestartObservationKind::Commit,
                    DurableTransactionRestartObservationKind::Page,
                ]
            );
            assert_eq!(
                observations
                    .iter()
                    .map(|observation| observation.position().get())
                    .collect::<Vec<_>>(),
                [1, 2, 3, 4, 5, 6]
            );
            assert_transaction_page(&observations[0], committed_with_page_id, 11, 3, [1, 1])?;
            assert_raw_page(&observations[1], 12, 4, [2, 2])?;
            assert_commit(&observations[2], committed_with_page_id)?;
            assert_transaction_page(&observations[3], uncommitted_id, 13, 5, [3, 3])?;
            assert_commit(&observations[4], commit_only_id)?;
            assert_raw_page(&observations[5], 14, 6, [4, 4])?;
            assert!(
                observations
                    .iter()
                    .all(|observation| { observation.position().lineage().same_lineage(&lineage) })
            );
            Ok(())
        },
    )??;
    assert_eq!(callbacks, 1);

    let analysis = analyze_durable_transaction_restart(&mut log)?;
    assert_analysis(
        &analysis,
        &lineage,
        committed_with_page_id,
        uncommitted_id,
        commit_only_id,
    );

    let mut reopened = log.restart();
    reopened.reopen()?;
    assert_eq!(reopened.records().len(), 6);
    assert_eq!(reopened.durable_records().len(), 6);

    let reopened_analysis = analyze_durable_transaction_restart(&mut reopened)?;
    assert_analysis(
        &reopened_analysis,
        LogDurability::lineage(&reopened),
        committed_with_page_id,
        uncommitted_id,
        commit_only_id,
    );
    Ok(())
}

fn assert_analysis(
    analysis: &DurableTransactionRestartAnalysis,
    lineage: &LogLineage,
    committed_with_page: TransactionId,
    uncommitted: TransactionId,
    commit_only: TransactionId,
) {
    assert!(analysis.lineage().same_lineage(lineage));
    assert_eq!(
        analysis.durable_frontier().map(|position| position.get()),
        Some(6)
    );
    let transactions = analysis.transactions();
    assert_eq!(transactions.len(), 3);
    assert_eq!(
        transactions
            .iter()
            .map(|entry| (entry.transaction().epoch(), entry.transaction().sequence()))
            .collect::<Vec<_>>(),
        [
            (
                committed_with_page.epoch().get(),
                committed_with_page.sequence()
            ),
            (uncommitted.epoch().get(), uncommitted.sequence()),
            (commit_only.epoch().get(), commit_only.sequence()),
        ]
    );

    assert_eq!(
        transactions[0]
            .first_owned_page_position()
            .map(|position| position.get()),
        Some(1)
    );
    assert_eq!(
        transactions[0]
            .last_owned_page_position()
            .map(|position| position.get()),
        Some(1)
    );
    assert_eq!(transactions[0].owned_page_record_count(), 1);
    assert_eq!(
        transactions[0]
            .state()
            .commit_position()
            .map(|position| position.get()),
        Some(3)
    );

    assert_eq!(
        transactions[1]
            .first_owned_page_position()
            .map(|position| position.get()),
        Some(4)
    );
    assert_eq!(
        transactions[1]
            .last_owned_page_position()
            .map(|position| position.get()),
        Some(4)
    );
    assert_eq!(transactions[1].owned_page_record_count(), 1);
    assert!(matches!(
        transactions[1].state(),
        DurableTransactionRestartState::Uncommitted
    ));

    assert_eq!(transactions[2].first_owned_page_position(), None);
    assert_eq!(transactions[2].last_owned_page_position(), None);
    assert_eq!(transactions[2].owned_page_record_count(), 0);
    assert_eq!(
        transactions[2]
            .state()
            .commit_position()
            .map(|position| position.get()),
        Some(5)
    );
}

fn assert_transaction_page(
    observation: &DurableTransactionRestartObservation<2>,
    owner: TransactionId,
    page_number: u64,
    page_version: u64,
    bytes: [u8; 2],
) -> Result<(), io::Error> {
    match observation {
        DurableTransactionRestartObservation::TransactionPage(observation) => {
            assert_eq!(observation.owner().epoch(), owner.epoch().get());
            assert_eq!(observation.owner().sequence(), owner.sequence());
            assert_eq!(observation.page().page_number().get(), page_number);
            assert_eq!(observation.page().page_version().get(), page_version);
            assert_eq!(observation.page().image().bytes(), &bytes);
            Ok(())
        }
        DurableTransactionRestartObservation::Page(_)
        | DurableTransactionRestartObservation::Commit(_) => Err(io::Error::other(
            "expected one transaction-owned page observation",
        )),
    }
}

fn assert_raw_page(
    observation: &DurableTransactionRestartObservation<2>,
    page_number: u64,
    page_version: u64,
    bytes: [u8; 2],
) -> Result<(), io::Error> {
    match observation {
        DurableTransactionRestartObservation::Page(observation) => {
            assert_eq!(observation.page_number().get(), page_number);
            assert_eq!(observation.page_version().get(), page_version);
            assert_eq!(observation.image().bytes(), &bytes);
            Ok(())
        }
        DurableTransactionRestartObservation::TransactionPage(_)
        | DurableTransactionRestartObservation::Commit(_) => {
            Err(io::Error::other("expected one raw page observation"))
        }
    }
}

fn assert_commit(
    observation: &DurableTransactionRestartObservation<2>,
    transaction: TransactionId,
) -> Result<(), io::Error> {
    match observation {
        DurableTransactionRestartObservation::Commit(observation) => {
            assert_eq!(observation.transaction().epoch(), transaction.epoch().get());
            assert_eq!(observation.transaction().sequence(), transaction.sequence());
            Ok(())
        }
        DurableTransactionRestartObservation::Page(_)
        | DurableTransactionRestartObservation::TransactionPage(_) => {
            Err(io::Error::other("expected one commit observation"))
        }
    }
}

fn persistent_log_id(value: u128) -> Result<PersistentLogId, io::Error> {
    PersistentLogId::new(value)
        .ok_or_else(|| io::Error::other("nonzero persistent log ID was rejected"))
}

fn unlogged_page<const N: usize>(
    lineage: &LogLineage,
    number: u64,
    version: u64,
    bytes: [u8; N],
) -> Result<UnloggedPage<N>, io::Error> {
    let page_number = PageNumber::new(number)
        .ok_or_else(|| io::Error::other("nonzero page number was rejected"))?;
    let image = PageImage::new(bytes).map_err(io::Error::other)?;
    Ok(UnloggedPage::new(
        PageAddress::new(lineage, page_number),
        PageVersion::new(version),
        image,
    ))
}
