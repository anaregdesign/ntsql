# ADR 0054: Selected Checkpoint Replay-Window Planning

- Status: Accepted
- Date: 2026-08-06
- Issue: #161
- Extends: ADR 0034, ADR 0037, ADR 0047, ADR 0048, ADR 0050, ADR 0053
- Extended by: ADR 0055
- Follows: #159

## Context

ADR 0053 selects one exact completeness checkpoint while the WAL, page store,
and checkpoint source are still owned and the store remains unrecovered. The
selected state deliberately exposes only identifying metadata. It cannot expose
the baseline's replay start, source observations, or a recovery operation.

That selection is not yet a replay plan. ADR 0050 intentionally validates only
the WAL prefix ending at the checkpoint frontier. A later durable logical suffix
may be unrelated to the selected prefix and is ignored by that validator. Startup
must not ignore the same suffix: it may contain raw page images, transaction-owned
page images, commits, or malformed history that changes the complete current
transaction analysis.

The source port guarantees one stable complete logical view only for the duration
of one higher-ranked callback. Combining a replay start from one call with
observations or analysis from another would mix evidence windows. The next
boundary must therefore:

1. rederive the exact selected checkpoint against the current shared store;
2. validate the complete current logical WAL, including its suffix; and
3. own every record in the exact replay window

inside one source callback. It still grants no page write, replay execution,
transaction restoration, live adapter access, or log reclamation.

## Crate and Dependency Boundary

`ntsql-transaction` owns the generic planning states, private replay
observations, validation composition, and errors. Existing memory and filesystem
adapters exercise the transition without changing their ports:

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
frame, checksum, marker, path, lock primitive, synchronization operation, source
port, page-store port, recovery algorithm, or publication protocol changes. The
transaction domain remains I/O-free.

## Consuming Planning Transition

`SelectedTransactionPageStorageRestartCheckpointCompleteness::
plan_replay_window(self)` consumes the only selected owner. It is available when
the retained source implements `DurableTransactionRestartAnalysisSource<N>` and
the retained store implements `DurablePageStoreSnapshotSource<N>`.

The transition first clones the source and store lineages and rejects a mismatch
without entering the callback. It then invokes
`with_durable_transaction_restart_observations` exactly once for this planning
attempt.

Inside that one stable callback, in order, it:

1. calls the ADR 0050 selected-prefix boundary logic with the privately retained
   checkpoint frontier;
2. calls the ADR 0047 completeness-evidence helper on exactly that selected
   prefix and the current shared store;
3. calls the ADR 0048 baseline-preparation helper on that result;
4. requires the complete rederived authoritative baseline to equal the privately
   retained selected baseline;
5. calls the ADR 0034 complete-current analysis helper on the callback's full
   frontier and observation slice;
6. resolves the retained replay start against that already validated full slice;
7. fallibly reserves the exact replay-record count; and
8. copies each selected logical observation once in physical order.

The ADR 0050 public validator now shares a private rederivation helper with this
transition. Its decoded-field comparison, public errors, selected-prefix
semantics, callback count, and strict snapshot-relative behavior do not change.
Planning compares two authoritative baselines instead of attempting to convert
the retained baseline back into an untrusted decoded observation.

The selected-prefix completeness pass observes each distinct selected-prefix
page at most once. Complete-current ADR 0034 analysis is WAL-internal and performs
no page-store observation. Planning performs no store write, WAL append,
durability operation, checkpoint load, publication, reopen, or lock acquisition.

## Selected Prefix and Complete Current Stream

The two validations intentionally have different scopes:

- selected-prefix revalidation proves that the checkpoint's transaction table,
  page classifications, and replay start still exactly match the prefix and
  current snapshots it covers; and
- complete-current analysis proves that every logical record through the current
  durable frontier has valid lineage, strict global order, exact tail/frontier
  agreement, and noncontradictory transaction history.

A valid suffix cannot change selected-prefix equality merely by existing. A
malformed suffix must still reject planning through the exact ADR 0034 evidence
cause. Suffix pages are not reconciled with the store in this decision. ADR 0055 adds
the separately reviewed read-only reconciliation and repair-preparation
transition.

Revalidation may succeed during ADR 0053 selection and differ during planning
only if the generic source or store changes between callbacks despite remaining
privately owned. The source trait promises stability within, not across,
callbacks. Planning therefore performs the comparison rather than relying on
adapter-specific lock behavior.

When valid rederived evidence differs from the retained baseline,
`DurableTransactionRestartCheckpointReplayBaselineMismatch` privately retains
the complete rederived baseline. It exposes only persistent identity, frontier,
transaction/page counts, and optional inclusive replay position. The failed
owner separately retains the original selected baseline, so the exact pair is
not discarded and neither baseline becomes caller-extractable authority.

## Exact Replay Window

The retained `DurableTransactionRestartReplayStart` is interpreted only after
complete-current analysis succeeds:

- `AtPosition(P)` selects the record exactly at `P` and every later logical
  record through the current frontier. `P` may precede the checkpoint frontier.
- `AfterFrontier(Some(F))` locates the exact logical record boundary `F` and
  selects only physically later logical records.
- `AfterFrontier(None)` selects the complete current logical stream.

No numeric successor is calculated. Position gaps remain valid, and
`u64::MAX` does not require overflow-prone arithmetic. An unchanged source may
therefore produce an empty window after `Some(F)`, while an empty checkpoint and
nonempty current source produce the complete current stream.

Missing inclusive and checkpoint boundaries are defensive typed failures. They
should be unreachable after successful baseline rederivation and complete-current
analysis, but no unchecked index, panic, or success-shaped fallback is used.

The window always ends at the current durable frontier supplied in the same
callback. It never stops merely because the checkpoint frontier was older.

## Owned Full-Image Observations

The callback's borrowed observation slice cannot escape. The plan therefore
stores a private
`OwnedDurableTransactionRestartReplayObservation<N>` for every selected record:

- a raw page retains page number, page version, exact `[u8; N]` image, and
  lineage-bound position;
- a transaction page additionally retains the persisted owner identity; and
- a commit retains the persisted transaction identity and lineage-bound
  position.

The implementation copies the fixed-width image bytes directly from already
validated observations. It does not add `Clone` to `PageImage`,
`DurablePageWalObservation`, `DurableTransactionPageObservation`, or
`DurableTransactionRestartObservation`.

Retaining full images is an intentional correctness-first cost. A future
consuming page-repair transition can use exactly the evidence from this callback
without re-entering the WAL source. This decision makes no throughput,
streaming, spill-to-disk, or bounded-memory claim. A later optimization must
preserve the same single-window ownership and fallible-allocation contract.

The vector performs one `try_reserve_exact` for the selected record count before
copying. Every later insertion fits that reservation. Capacity failure retains
the exact attempted count. A count-only reservation helper permits deterministic
`usize::MAX` testing without constructing an impossible observation slice.

## Planned and Failed Owners

Successful planning returns
`PlannedTransactionPageStorageRestartCheckpointReplay`. It privately retains:

- the complete original selected owner;
- the exact unrecovered WAL/page-store owner;
- the exact checkpoint source, lock, and publisher capability;
- the exact selected baseline;
- the exact complete-current ADR 0034 analysis; and
- the exact owned replay-observation vector.

It exposes only:

- persistent log ID;
- selected checkpoint frontier;
- complete current frontier;
- optional inclusive replay start;
- replay-record count; and
- complete-current transaction count.

The complete-current analysis is private evidence for a future consuming
repair/restoration successor. It is not exposed as current live-storage evidence
and does not bypass ADR 0037. This decision does not yet define the later
transition that will bind repaired checkpoint startup to a final
restart-analyzed live owner.

Any failure returns
`FailedTransactionPageStorageRestartCheckpointReplayPlanning`. It retains the
complete original selected owner and exact planning error. It exposes only
`error()`. It has no retry, adapter accessor, evidence accessor, or direct
recovery method.

## Explicit Full-Recovery Fallback

`Planned::decline_replay_plan` destructures and drops the complete-current
analysis, every owned replay observation, and the selected baseline before
calling ADR 0053's `decline_checkpoint`. It returns only the unchanged
`UncheckpointedTransactionPageStorage`.

`Failed::continue_with_full_recovery` performs the same baseline-destroying
transition and also returns the exact owned planning error. Neither path can keep
the plan while obtaining full-recovery authority.

Both fallbacks delegate only to ADR 0053's existing complete recovery, retry,
restart-analysis, and publication wrappers. No selected replay record becomes a
recovery target or write permit.

## Error Priority and Source Result

`DurableTransactionRestartCheckpointReplayPlanningError` distinguishes:

- pre-callback source/store `LineageMismatch`;
- exact source failure before or after callback; and
- boxed stable-callback evidence failure.

The evidence error distinguishes, in execution order:

1. selected-prefix revalidation evidence;
2. a valid but different authoritative selected baseline;
3. complete-current ADR 0034 evidence;
4. replay-boundary evidence; and
5. replay-vector capacity exhaustion.

Selected-prefix errors retain the exact ADR 0050 nested completeness,
page-store, preparation, or boundary cause. Complete-current errors retain the
exact ADR 0034 cause. Replay-boundary and capacity errors retain exact numeric
fields.

If the source invokes the callback and then returns an error instead of its
output, that source error is authoritative. Any candidate plan computed inside
the callback is dropped, and the failed owner retains the unchanged original
selected state. `Display` identifies every stage and `Error::source` preserves
the complete nested chain.

## Filesystem Lock Continuity

The ADR 0052/0053 acquisition order remains:

1. transaction-page WAL;
2. page store; and
3. completeness control.

Moving the three exact values from `Selected` into `Planned` or `Failed` does not
close, clone, reopen, replace, or unlock a descriptor. The WAL lifetime lock
protects the complete planning callback. The page-store and checkpoint-control
locks remain held by the owner throughout. Explicit fallback retains all three
locks through complete recovery and restart analysis. Final `into_parts` only
transfers the still-locked values.

The locks remain cooperative advisory locks. This decision adds no waiting,
atomic three-object acquisition, hostile path defense, or unsupported-filesystem
guarantee.

## Authority Boundary

No plan, failure, accessor, error, selected baseline, complete-current analysis,
or owned replay observation can directly create or substitute:

- `TransactionId`, active/committed transaction state, or coordinator state;
- dirty, clean, live-permitted, or recovery-permitted pages;
- page-write or committed-page recovery permits;
- recovered or restart-analyzed live storage;
- checkpoint publication permits or receipts;
- WAL positions accepted by append or durability ports;
- replay execution, redo, undo, rollback, compensation, or page repair;
- retention floors, truncation, compaction, or reclamation authority; or
- native format or external compatibility evidence.

Private fields prevent plan/failure construction and extraction of baseline,
page, replay, record, analysis, adapter, or checkpoint-source values.
Compile-fail tests also reject direct recovery, retry, and capability
substitution.

## Adapter Integration

Memory integration publishes an exact checkpoint through an uncommitted page,
then appends and durably commits a later transaction-owned page. Planning retains
the old inclusive replay floor while extending through the new durable frontier
and owning all three required records. Explicit fallback uses unchanged complete
recovery, repairs the later committed page, preserves the full log, and retains
the exact checkpoint source.

Filesystem integration publishes a store-current checkpoint, appends a later
durable committed page without flushing its store image, drops and reopens the
three-object composition, selects the checkpoint, and plans exactly the two
suffix records. Independent open attempts prove all three locks remain held in
selected, planned, fallback, page-recovered, analyzed, and final transferred
states. Full fallback then repairs the suffix page through the unchanged
recovery path.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, page, WAL, recovery,
restart-analysis, completeness, checkpoint, ownership, lock, and fault
contracts. No external product documentation, driver, SDK, fixture, oracle,
proprietary governance tool, or native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server replay algorithm, checkpoint, transaction
table, dirty-page table, recovery phase, LSN, error, diagnostic, persistent
format, or compatibility behavior.

## Test Boundaries

- Empty current evidence produces an empty `AfterFrontier(None)` plan.
- An empty selected checkpoint over a nonempty current stream owns the complete
  stream.
- `AfterFrontier(Some(F))` produces both exact-empty and suffix-only windows.
- `AtPosition(P)` includes `P`, including when it precedes the checkpoint
  frontier, and extends through a later commit.
- Owned raw, transaction-page, and commit records retain exact kind, fields,
  bytes, lineage, and physical order.
- A malformed suffix fails through exact complete-current ADR 0034 evidence.
- A source that changes its selected prefix between callbacks fails with the
  opaque exact rederived baseline.
- Selected-prefix store failure, source failure before callback, and source
  failure after callback retain distinct exact stages and causes.
- One planning callback and one selected-page observation pass occur; no store
  write occurs.
- Deterministic `usize::MAX` reservation failure retains the exact count.
- Planned and failed explicit fallbacks retain the original source and use only
  unchanged complete recovery.
- Memory and filesystem integrations cover durable suffixes, exact replay
  counts/frontiers, store repair only after explicit fallback, checkpoint-source
  retention, and filesystem three-lock continuity.
- Existing recovery, restart analysis, completeness validation, selection,
  publication, codec, adapter, lock, architecture, and governance tests remain
  valid.

## Non-Goals

This ADR does not:

- execute replay, redo, undo, rollback, compensation, or page repair;
- mutate the page store, WAL, checkpoint slot, or selected baseline during
  planning;
- restore transaction coordinator or runtime transaction state;
- execute the ADR 0055 prepared repairs or construct the final live owner;
- add replay streaming, spilling, indexing, batching, or a memory limit;
- enumerate or reconcile store-only pages or suffix page snapshots;
- add checkpoint invalidation, quarantine, republish, generations, or fallback
  selection;
- choose a retention floor or truncate, compact, reclaim, or rewrite WAL;
- change persistent bytes, lock order, synchronization, source ports, or recovery
  algorithms; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql can now turn one exact selected completeness checkpoint into an owned,
complete-current replay window without mixing WAL callbacks or releasing any
startup adapter. A malformed suffix is no longer hidden by valid checkpoint
selection, and later work can consume exact full-image evidence without
reprojecting the WAL.

The plan remains private and non-authorizing. ADR 0055 consumes it into exact
read-only page-repair decisions without reprojecting the WAL. Separately reviewed
work is still required to execute those repairs, restore transaction runtime
state, establish a live storage owner, choose retention, and reclaim WAL.
