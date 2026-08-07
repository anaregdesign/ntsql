# ADR 0053: Pre-Recovery Restart Checkpoint Completeness Selection

- Status: Accepted
- Date: 2026-08-06
- Issue: #159
- Extends: ADR 0033, ADR 0037, ADR 0050, ADR 0051, ADR 0052
- Extended by: ADR 0054, ADR 0055, ADR 0058, ADR 0059, ADR 0060, ADR 0061
- Follows: #157

## Context

ADR 0052 opens one WAL, page store, and persistent completeness slot in a fixed
lock order. Its original wrapper could separate the locked completeness source
from the unrecovered WAL/page-store owner, so the only existing owning path ran
ADR 0033 committed-page recovery before ADR 0051 source validation.

That order loses the observation boundary a completeness checkpoint records.
ADR 0047/0048 baselines deliberately classify each selected-prefix page as
`NoRequiredImage`, `StoreMissing`, `StoreBehind`, or `StoreCurrent` and retain an
exact inert replay lower bound. Full committed-page recovery may replace missing
or behind snapshots before validation, making the persisted classification no
longer reproducible even when it exactly matched crash-time state.

The checkpoint must therefore be loaded and validated while the page store is
still unrecovered. This is only a candidate-selection boundary. It must not
execute replay, repair from checkpoint metadata, restore transaction runtime
state, weaken ADR 0050 comparison, or bypass the existing complete recovery
algorithm.

The concrete completeness source is also the lifetime lock holder and
publisher. Selection cannot drop and reopen it without releasing the third lock
and creating a race. Every outcome and every explicit full-recovery fallback
must retain the exact same source value.

## Crate and Dependency Boundary

The adapter-neutral ownership states belong to `ntsql-transaction`.
`ntsql-storage-memory` exercises them with the existing deterministic source and
publisher. `ntsql-storage-file` changes only its existing three-object
composition wrapper and integration tests:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal

ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No crate, direct dependency edge, architecture registration, persistent byte,
control header, checksum, path, lock primitive, fault point, synchronization
operation, WAL rule, page-store rule, or checkpoint port changes. The transaction
domain remains I/O-free.

## Consuming Pre-Recovery Selection

`UnrecoveredTransactionPageStorage::
select_restart_checkpoint_completeness(self, checkpoint_source)` is available
only when:

- the owned source implements the existing complete page-recovery inventory and
  source ports plus `DurableTransactionRestartAnalysisSource`;
- the store implements `CommittedTransactionPageRecoveryStore`, which already
  includes current snapshot observation; and
- the supplied owned checkpoint source implements
  `DurableTransactionRestartCheckpointCompletenessBaselineSource`.

The transition consumes both owners. It exposes no source, store, checkpoint
source, or mutable reference before returning one owning outcome.

The operation performs this exact order:

1. call `load_restart_checkpoint_completeness_baseline` exactly once;
2. end the mutable checkpoint-source borrow while retaining the returned owned
   untrusted observation;
3. return `Absent` immediately when the slot is empty;
4. otherwise call the unchanged ADR 0050
   `validate_restart_checkpoint_completeness_baseline_against_current_prefix`
   helper with the still-unrecovered WAL and current page store; and
5. retain either its newly re-derived authoritative baseline or its exact
   failure.

Absence and source failure perform zero WAL callbacks and zero page
observations. Present validation keeps ADR 0050's one selected-prefix callback,
at most one observation for each distinct selected-prefix page, strict
field-for-field comparison, and source-error precedence. Selection performs no
store write, WAL append, durability operation, slot mutation, publication, or
reopen.

## Three Owning Outcomes

`TransactionPageStorageRestartCheckpointCompletenessSelection` distinguishes
`Selected`, `Absent`, and `Rejected`. Each variant owns:

- the exact original unrecovered WAL/page-store owner; and
- the exact supplied checkpoint source, including any concrete lock and
  publisher capability.

The variants are public for exhaustive handling, but their payload fields and
constructors remain private. Safe code cannot forge a selected or fallback
owner.

### Selected

`SelectedTransactionPageStorageRestartCheckpointCompleteness` additionally
retains the authoritative baseline newly returned by ADR 0050. It exposes only:

- persistent log ID;
- optional numeric durable frontier;
- transaction count; and
- page count.

It exposes no baseline, transaction entry, page entry, required image, replay
bound, adapter, checkpoint source, or recovery operation.

`decline_checkpoint(self)` is the only transition from `Selected` into the
existing full-recovery path. It explicitly destructures and discards the
selected baseline, then returns an `UncheckpointedTransactionPageStorage`
containing only the unrecovered owner and retained checkpoint source. No
selected-baseline field survives recovery.

Dropping `Selected` drops all retained values. It does not mutate or invalidate
the slot.

### Absent

`AbsentTransactionPageStorageRestartCheckpointCompleteness` represents one
successfully opened and locked source whose current slot is unpublished. Its
`continue_with_full_recovery(self)` transition acknowledges absence and returns
the same uncheckpointed owner.

Absence is not synthesized from a source or semantic failure, and it grants no
special empty-database authority.

### Rejected

`RejectedTransactionPageStorageRestartCheckpointCompleteness` retains one exact
`DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError`.
Its variants remain:

- `CheckpointSource`, for the exact load failure; and
- boxed `BaselineValidation`, for the exact ADR 0050 source, identity, frontier,
  completeness-evidence, preparation, or mismatch failure.

`error()` borrows that cause. `Display` and `Error::source` preserve the complete
chain without requiring either adapter to implement formatting or error traits.

`continue_with_full_recovery(self)` explicitly bypasses the rejected checkpoint
and returns both:

- the uncheckpointed owner retaining all three values; and
- the exact owned rejection cause.

Rejection never becomes absence, never erases or quarantines the slot, and never
silently starts recovery.

## No Selection Retry

No selection outcome has a retry operation. The operation performs no writes and
retains exclusive ownership of all three values. Re-running semantic validation
against those frozen values would advertise progress without changing an input.
A source failure that requires reopen cannot be repaired while its locked value
is retained.

The caller may drop the outcome and explicitly reopen the complete composition,
or consume absence, decline, or rejection into full recovery. This decision
assigns no automatic retry, backoff, invalidation, or source repair policy.

## Exact Existing Full-Recovery Fallback

`UncheckpointedTransactionPageStorage::recover(self)` delegates only to the
existing `UnrecoveredTransactionPageStorage::recover`. It introduces no page
selection, ordering, write permit, recovery plan, resume cursor, or error.

Success wraps the exact existing page-recovered owner with the same checkpoint
source. Failure wraps the exact existing failed-recovery owner with that source.
The failure exposes only the existing error and one `retry(self)` that delegates
only to the existing fresh complete recovery retry. The source remains retained
across partial physical success and every retry.

The page-recovered wrapper exposes only the immutable existing recovery report
and consuming `analyze_restart`. It does not expose adapters or the checkpoint
source. Restart analysis delegates only to ADR 0037:

- success retains the same source beside the exact final analyzed owner; and
- failure retains it beside the exact fail-closed analysis owner until drop.

No new release path exists before complete recovery and restart analysis
succeed.

## Retained Publication Capability

`RestartAnalyzedTransactionPageStorageWithCompletenessCheckpoint` is the final
successful wrapper. It exposes the existing recovery report, restart analysis,
and WAL/page-store accessors. Its consuming `into_parts` returns both adapters,
both immutable startup evidence values, and the same checkpoint source.

When that source also implements the existing ADR 0051 publisher port,
`publish_restart_checkpoint_completeness_baseline_from_current_prefix` delegates
to the unchanged final-owner publication operation with a mutable borrow of the
retained source. No reopen or lock reacquisition occurs.

This publication derives a fresh current baseline after full recovery. It does
not reuse the declined or rejected startup candidate, and its success or
indeterminate failure has exactly ADR 0051/0052 semantics.

## Filesystem Lock Continuity

`UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint` no longer
exposes `into_parts`. Its only consuming operation is filesystem-specialized
pre-recovery selection.

The ADR 0052 acquisition order remains:

1. transaction-page WAL;
2. page store; and
3. completeness control.

Moving those values through selection, explicit fallback, recovery failure and
retry, page-recovered state, restart analysis, and final publication does not
close, clone, reopen, replace, or unlock any descriptor. A second opener for
each object remains excluded throughout. Final `into_parts` transfers the
still-locked values; drop releases them.

The locks remain cooperative advisory locks. This decision adds no atomic
three-object acquisition, waiting protocol, database-wide lock, hostile path
defense, or guarantee for unsupported filesystems.

## Exact Acceptance Window

Selection is strict snapshot-relative validation, not a monotonic checkpoint
rule:

- `StoreMissing` selects when the covered page is still missing;
- `StoreBehind` selects when the same exact older snapshot is still current;
- `StoreCurrent` selects when the same exact selected-prefix snapshot is still
  current; and
- `NoRequiredImage` selects only when every other persisted field still matches.

An unrelated later WAL suffix can remain outside the selected window under ADR
0050. A current snapshot for any covered page beyond the checkpoint frontier
fails through the existing `SnapshotBeyondFrontier` evidence path. The caller
may explicitly retain that rejection and run full recovery, which independently
uses the complete current WAL.

Normal post-publication page flushing can therefore make old checkpoints
frequently rejected. This decision does not infer historical page-store state,
accept dominance, or treat a later snapshot as equivalent. Quiesced publication
or any broader acceptance relation requires separate evidence and review.

## Error and Authority Boundary

Selection and fallback errors are internal startup evidence. Adapter paths,
physical stages, lock failures, decoded fields, WAL positions, page identities,
and recovery details remain outside `ClientDiagnostic`.

The selection enum, every outcome, identifying accessor, authoritative retained
baseline, rejection, fallback wrapper, recovery report, restart analysis, and
checkpoint source cannot directly create or substitute:

- transaction lifecycle or coordinator state;
- dirty, clean, live-permitted, or recovery-permitted pages;
- page-write or committed-page recovery permits;
- WAL positions, append, flush, or durability authority;
- completeness publication permits or receipts;
- replay, redo, undo, rollback, compensation, or page repair commands;
- retention floors, truncation, compaction, or reclamation authority; or
- native format or external compatibility evidence.

Compile-fail coverage rejects outcome forging, adapter and source escape,
baseline/page/replay extraction, direct selected or rejected recovery, and
substitution for page-write, WAL, publication, replay, retention, or reclamation
authority.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, page, WAL, recovery,
restart-analysis, completeness, source, publication, ownership, lock, and fault
contracts. No external product documentation, driver, SDK, fixture, oracle,
proprietary governance tool, or native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server startup selection, checkpoint, transaction
table, dirty-page table, recovery phase, page repair, LSN, error, diagnostic,
persistent format, or compatibility behavior.

## Test Boundaries

- Domain fakes prove source load precedes WAL validation, occurs exactly once,
  and ends before one WAL callback and one observation per selected page.
- Exact `StoreMissing` and `StoreBehind` baselines select without a recovery
  write; selected metadata is exact and explicit decline discards the baseline.
- Empty-slot absence and source rejection perform no WAL callback or page
  observation before explicit fallback.
- Zero and foreign persistent identities reject before callback; an empty
  frontier with a claimed stored page rejects as an exact page-count mismatch.
- A covered snapshot beyond the selected frontier retains the exact
  `SnapshotBeyondFrontier` chain and then completes explicit full recovery.
- A one-shot full-recovery write failure retains the source through the existing
  fresh retry, restart analysis, and subsequent publication.
- Memory integration proves selected, absent, source-rejected, and
  advanced-snapshot paths preserve the same source and current data.
- Filesystem integration proves a persisted `StoreMissing` baseline selects
  before recovery, all three locks remain held through decline, recovery,
  analysis, and publication, and the same source publishes without reopen.
- Filesystem integration also proves advanced selected-page rejection followed
  by explicit current-WAL full recovery and fresh publication.
- Existing recovery, restart analysis, completeness validation, source,
  publication, codec, lock, architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- execute or schedule replay, redo, undo, rollback, compensation, or page repair;
- use checkpoint metadata as a committed-page recovery target or write permit;
- restore active, committed, aborted, or coordinator runtime transaction state;
- add slot invalidation, deletion, quarantine, automatic republish, repair, or
  indeterminate-attempt resolution;
- add checkpoint generations, history, ordering, fallback selection, or
  multi-slot choice;
- choose a retention floor or truncate, compact, reclaim, or rewrite a WAL;
- change full committed-page recovery or restart-analysis algorithms;
- relax exact validation into monotonic or dominance acceptance;
- change any persistent bytes, locks, synchronization, or external contract; or
- define native SQL Server formats or compatibility.

## Consequences

ntsql can now inspect one completeness checkpoint at the only point where its
crash-time page classifications are still observable. Exact candidates,
absence, and rejection remain distinct owning states, while every explicit
fallback uses the unchanged complete recovery and retains the same locked source
through final publication.

ADR 0054 adds the separately reviewed consuming replay-planning transition, and
ADR 0055 consumes that private plan into read-only page-repair decisions. The
selected baseline remains private, no repair is executed, and transaction
restoration, retention, and WAL reclamation still require later boundaries.
