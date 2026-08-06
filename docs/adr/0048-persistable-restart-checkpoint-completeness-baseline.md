# ADR 0048: Persistable Restart Checkpoint Completeness Baseline

- Status: Accepted
- Date: 2026-08-06
- Issue: #149
- Extends: ADR 0038, ADR 0047
- Extended by: ADR 0049, ADR 0050

## Context

ADR 0047 derives one coherent current transaction table, WAL-backed page table,
and replay-start candidate under a single stable complete-WAL callback. Its
`DurableTransactionRestartCompletenessAnalysis` is still analysis evidence, not
an approved checkpoint encoder input. The nested transaction analysis retains a
runtime `LogLineage`, lineage-branded positions, and platform-width transaction
page counts.

ADR 0038 already owns the lossless persistent projection of that transaction
analysis. Reimplementing the projection for a complete checkpoint would duplicate
persistent-identity, position-width, count-width, ordering, and error rules.
Independently recomputing page or replay fields after preparing the transaction
baseline could also mix evidence windows.

The smallest next step is an I/O-free, private-field baseline that nests the
existing ADR 0038 transaction baseline and moves the exact ADR 0047 page table
and replay start from the same owned result. It creates an approved future codec
input shape without adding bytes, a source, publication, startup selection,
replay execution, or WAL reclamation.

## Crate and Dependency Boundary

Only `ntsql-transaction` production code and tests plus memory-adapter integration
change:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

No crate, dependency edge, architecture registration, adapter port, file, byte
format, frame, checksum, marker, lock, fault point, synchronization operation, or
publication protocol changes. The transaction domain remains I/O-free.

## Baseline Shape

`DurableTransactionRestartCheckpointCompletenessBaseline` privately owns:

- one exact `DurableTransactionRestartCheckpointBaseline`;
- the exact strict-page-number `DurableTransactionRestartPageEntry` vector from
  the same completeness analysis; and
- the exact `DurableTransactionRestartReplayStart` from that analysis.

The nested ADR 0038 baseline is the sole owner of:

- the nonzero adapter-owned `PersistentLogId`;
- the optional numeric durable frontier; and
- the persistable transaction table.

The outer baseline does not duplicate ID or frontier fields, so private
construction cannot create disagreement between transaction, page, and replay
metadata. Convenience accessors delegate persistent ID, frontier, and
transaction entries to the nested baseline.

The page and replay types already contain portable-width inert data:

- `PageNumber` is a nonzero `u64` identity;
- required, stored, frontier, and replay positions are numeric `u64`;
- transaction owners are immutable
  `DurableTransactionIdentityObservation` values; and
- page and replay causes contain no runtime lineage or adapter capability.

They are therefore moved unchanged rather than lowered into duplicate types.
Private outer fields remain the binding proof that those otherwise inspectable
values came from one analysis.

## Final-Owner Preparation Gate

`RestartAnalyzedTransactionPageStorage::
prepare_restart_checkpoint_completeness_baseline_from_current_prefix` is the
only public preparation operation.

It first checks whether the retained source lineage has an adapter-owned
`PersistentLogId`. An ephemeral lineage returns the existing
`PersistentLineageRequired` preparation failure before:

- entering the WAL callback;
- validating the current stream;
- observing the page store; or
- allocating a transaction baseline.

A valid persistent source then invokes
`analyze_current_restart_completeness` exactly once. ADR 0047 therefore still
provides:

- one complete current WAL stability window;
- transaction analysis before store observation;
- one shared store borrow;
- exactly one observation per distinct WAL page; and
- one coherent transaction/page/replay result.

No public operation prepares from a detached
`DurableTransactionRestartCompletenessAnalysis`. The private helper consumes
that result, preventing later caller mutation or substitution.

## Reuse of ADR 0038 Projection

The private preparation helper destructures the owned completeness analysis and
passes its transaction analysis to the existing
`prepare_restart_checkpoint_baseline` helper.

That reuse preserves exactly:

- persistent-lineage validation;
- optional frontier projection;
- strict transaction-identity order;
- first/last owned-page positions;
- checked `usize` to `u64` owned-page count conversion;
- committed/uncommitted state and commit positions;
- fallible exact transaction-vector reservation; and
- `DurableTransactionRestartCheckpointBaselineError`.

Only after that projection succeeds does preparation return the outer value with
the already allocated page vector and replay start. No second WAL callback,
store observation, page-table allocation, sorting, derivation, or replay-floor
selection occurs.

The method's early persistent-lineage check is repeated defensively by the reused
private ADR 0038 helper. The first check establishes fail-fast owner behavior;
the helper check preserves its standalone internal invariant.

## Point-in-Time Currency

The baseline describes the exact current WAL and store evidence observed during
its single preparation call. It does not use the immutable startup analysis
retained by the owner.

After preparation returns, later WAL appends, durability fences, page writes, or
store changes do not update the baseline. A clone remains the same inert
historical metadata. Repeating preparation performs a fresh current completeness
analysis and may produce a different frontier, page state, or replay start.

No startup-only preparation method is added. The retained startup transaction
analysis has no co-timed page-store observation and cannot supply ADR 0047
completeness.

## Error Boundary

`DurableTransactionRestartCheckpointCompletenessBaselineCurrentPreparationError`
has two stages:

- `CompletenessAnalysis`, retaining the exact ADR 0047 source, store, lineage,
  stream, snapshot, or allocation failure; and
- `BaselinePreparation`, retaining the exact ADR 0038 persistent-lineage,
  transaction-capacity, or count-width failure.

`Display` identifies the stage and `Error::source` preserves the complete nested
cause chain. No partial transaction baseline, page table, or replay start is
returned.

Ephemeral lineage intentionally has priority over current completeness errors
because no persistent baseline can be formed regardless of WAL or store state.
For persistent lineage, ADR 0047's established lineage, stream, allocation,
page-order, store-observation, and source-result priorities remain unchanged.

## Authority Boundary

The completeness baseline is cloneable because it is immutable data, not a
linear capability. It exposes read-only identifiers, transaction/page slices,
and replay metadata only.

Neither the outer baseline nor its nested values can create or satisfy:

- `TransactionId`, active/committed transaction state, or coordinator state;
- dirty, clean, live-permitted, or recovery-permitted pages;
- a committed-page recovery write permit;
- recovered or restart-analyzed storage ownership;
- a checkpoint publication permit, receipt, selected slot, or decoded validity;
- a lineage-bound `LogSequenceNumber`, WAL append, or durability fence;
- redo, undo, rollback, abort, compensation, or replay execution; or
- a retention floor, truncation, compaction, or reclamation operation.

The existing transaction-baseline publisher still requires its private
generative permit. Access to the nested transaction baseline does not authorize
direct publication, and this ADR adds no completeness publisher.

Compile-fail tests reject direct construction, detached preparation, and
conversion to transaction, dirty-page, storage-owner, publication-receipt, or
WAL authority.

## Adapter Integration

The deterministic memory integration prepares the baseline from a live
restart-analyzed owner after page recovery and additional current WAL/store
activity. It proves:

- exact persistent log ID and current frontier;
- exact equality with a separately observed current page table and replay start;
- exact transaction-table delegation;
- unchanged page-store cardinality; and
- no write or recovery effect from preparation.

Filesystem behavior requires no implementation change. ADR 0047 already proves
the same current completeness analysis after real reopen and under retained file
locks. A later codec and filesystem publication slice must use this new
authoritative baseline explicitly rather than reinterpret the transaction-only
ADR 0044 blob.

## Evidence and Compatibility Boundary

All behavior derives from repository-authored transaction, WAL, page, recovery,
ownership, and adapter contracts. No external product documentation, driver,
SDK, fixture, oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK
format is consulted.

This decision defines no SQL Server checkpoint, dirty-page table, replay start,
LSN, MinLSN, recovery phase, startup behavior, error, diagnostic, or
compatibility result.

## Test Boundaries

- A persistent empty prefix preserves `None`, empty transaction/page tables, and
  `AfterFrontier { None }`.
- Interleaved raw, committed, and uncommitted histories preserve exact sorted
  page states and the exact minimum replay cause.
- The nested transaction baseline retains the same persistent ID, frontier,
  entries, ranges, counts, and states as ADR 0038.
- One valid preparation adds exactly one WAL callback and one observation per
  distinct WAL page, with no store write.
- Source and store failures remain nested under `CompletenessAnalysis`.
- Ephemeral lineage returns `PersistentLineageRequired` without another callback
  or store observation.
- Startup analysis and store contents remain unchanged.
- Real memory-adapter integration preserves current page/replay evidence and
  store state.
- Compile-fail tests reject forged binding and capability substitution.
- Existing recovery, restart, checkpoint, codec, source, publisher, adapter,
  architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- encode, decode, validate, persist, publish, load, repair, or select the
  completeness baseline;
- change ADR 0044 checkpoint bytes or ADR 0046 filesystem slot behavior;
- add a source, publisher, permit, receipt, or indeterminate publication state;
- make checkpoint presence or completeness a startup gate;
- execute redo, undo, rollback, abort, compensation, replay, or page repair;
- restore transaction coordinator or active transaction state;
- enumerate or reconcile store-only pages;
- choose generations, history, fallback, or retention policy;
- truncate, compact, or reclaim WAL; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql now has an authoritative, persistent-lineage-bound input shape for a
future complete checkpoint codec. It preserves transaction, WAL-backed page, and
replay metadata from exactly one current ADR 0047 window while remaining inert.

The next focused slice can define independent versioned bytes and an untrusted
decoded observation for this baseline. Source-relative validation and atomic
publication must remain separate reviewed boundaries, and no decoded checkpoint
may influence startup replay before those validations exist.
