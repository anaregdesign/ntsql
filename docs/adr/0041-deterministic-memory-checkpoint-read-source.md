# ADR 0041: Deterministic Memory Checkpoint Read Source

- Status: Accepted
- Date: 2026-08-06
- Issue: #134
- Extends: ADR 0040

## Context

ADR 0040 defines one temporary optional-slot source port that returns a complete
owned but untrusted restart checkpoint observation. The final restart-analyzed
storage owner completes that read before entering current-WAL validation, so a
checkpoint adapter does not establish a nested checkpoint-source-to-WAL-source
callback or lock order.

The port needs one concrete deterministic adapter before a persistent checkpoint
format or publication protocol is designed. Combining the slot with
`InMemoryCommitLog` would make the sequencing boundary untestable as two
independent mutable sources and could imply that WAL durability publishes a
checkpoint. The memory read source must therefore be a separate object.

## Crate and Dependency Boundary

Only `ntsql-storage-memory` production code and tests change. The reviewed graph
remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

No crate, dependency edge, architecture registration, domain I/O, filesystem
API, persistent format, or existing memory WAL/page-store operation changes.

## Separate Constructor-Seeded Source

`InMemoryTransactionRestartCheckpointBaselineSource` owns:

- one optional
  `OwnedDurableTransactionRestartCheckpointBaselineObservation`; and
- one optional deterministic pre-load fault.

`empty()` constructs an absent slot. `seeded(observation)` moves one already
allocated untrusted observation into the adapter without inspecting or copying
its fields. `slot()` permits read-only test inspection.

The constructor seed is fixture setup, not a runtime publication operation. The
adapter exposes no setter, replacement method, store port, write callback,
publication receipt, generation, selection, history, or retention operation.
The source is a separate value from `InMemoryCommitLog` and
`InMemoryPageStore`.

## Exact Fresh Loads

An absent load returns `Ok(None)` before constructing or reserving a vector.

For a present slot, every successful load:

1. reads the exact seeded transaction-entry count;
2. calls `try_reserve_exact` for that count on a new vector;
3. copies every `Copy` entry in its seeded order without conversion; and
4. constructs a fresh owned observation from the unchanged raw persistent ID,
   optional frontier, and copied entries.

Zero IDs, zero frontiers, zero identities, contradictory ranges, arbitrary
counts, state contradictions, and unsorted or duplicate entries remain exact
untrusted values. The adapter performs no validation, normalization, sorting,
deduplication, or authority conversion.

The explicit reservation ensures `extend_from_slice` needs no further growth.
A reservation failure returns
`TransactionCapacityExhausted { transaction_count }` and no partial candidate.
The seeded slot remains unchanged and can be read again.

Successful loads are repeatable and non-consuming. Each returned allocation is
owned independently, while the fixture slot remains available for inspection
and later reads.

## Deterministic Read Fault

`RestartCheckpointBaselineSourceFaultPoint::BeforeLoad` is checked before the
adapter inspects slot presence or allocates a result. Reaching it clears the
one-shot plan and returns an exact `InjectedFault(BeforeLoad)` with no candidate.
The slot is unchanged, so retry reaches the normal absent or present behavior.

Only one fault may be armed. Attempting to arm another while one remains pending
returns `RestartCheckpointBaselineSourceFaultAlreadyArmed` with both the retained
and rejected plans and changes neither. `armed_fault()` provides read-only test
inspection.

This is a synthetic operation failure only. It models no disk read, torn bytes,
checksum, quarantine, process failure, concurrency, or external diagnostic.

## Sequential Owner Integration

The real memory source is passed separately to
`RestartAnalyzedTransactionPageStorage::
validate_restart_checkpoint_baseline_from_source`.

The composition remains:

1. load a fresh owned memory snapshot or absence;
2. end the memory-source mutable borrow;
3. return immediately for absence or source failure; and
4. validate a present snapshot against the memory WAL's claimed retained prefix.

The adapter itself never borrows or calls the WAL. The owner never stores a
borrow into the adapter. No checkpoint-source operation is active while the WAL
source callback runs.

An exact present snapshot returns only the authoritative private ADR 0038
baseline re-derived by ADR 0039. A structurally copied but invalid snapshot
returns the existing baseline-validation error. The memory source never upgrades
its own slot into authoritative state.

## Error and Authority Boundary

`InMemoryTransactionRestartCheckpointBaselineSourceError` distinguishes only:

- the exact injected pre-load fault; and
- present-slot entry-vector capacity exhaustion with the requested count.

Both return no candidate. Neither changes the seeded slot. The outer ADR 0040
composition continues to distinguish this source error from boxed WAL/baseline
validation errors.

The adapter and loaded observation cannot satisfy or create:

- transaction lifecycle or coordinator state;
- WAL append, restart-analysis, durability, flush, or lineage capabilities;
- page-store or recovery write authority;
- a recovered or restart-analyzed storage owner;
- checkpoint publication, replacement, selection, or startup ownership;
- dirty-page tables, replay starts, redo, undo, rollback, or compensation; or
- retention floors, truncation, compaction, or reclamation.

Compile-fail tests reject the memory source as a WAL durability source, page
store, authoritative restart-analysis source, or baseline. The existing ADR 0040
compile-fail boundary applies independently to every loaded observation.

## Evidence and Compatibility Boundary

All behavior uses repository-authored checkpoint observations, source ports,
memory adapters, WAL evidence, and restart validation. No external product
documentation, driver, SDK, fixture, oracle, proprietary governance tool, or
native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server checkpoint bytes, publication point,
recovery phase, transaction table, error, diagnostic, or compatibility result.

## Test Boundaries

- A seeded observation containing zero and contradictory fields survives the
  first and repeated loads exactly and remains present in the source.
- A pre-load fault refuses replacement, returns its exact typed error once,
  preserves the seed, and permits a successful retry.
- An empty source repeatedly returns absence without manufacturing a value.
- A capacity error retains the exact requested entry count and has no nested
  source.
- Real owner integration validates exact present snapshots repeatedly and leaves
  the source slot, immutable startup analysis, and page store unchanged.
- A deliberately malformed current WAL proves absent and checkpoint-source-error
  paths return without invoking WAL analysis.
- After repairing that test evidence, the faulted source retries successfully;
  an invalid present entry returns the baseline-validation error instead.
- Compile-fail tests preserve WAL, page-store, restart-analysis, baseline, and
  storage authority boundaries.
- Existing memory WAL, recovery, restart, checkpoint, architecture, and
  governance tests remain valid.

## Non-Goals

This ADR does not:

- add a checkpoint write/store/publication port or receipt;
- define runtime slot mutation, replacement, concurrency, or synchronization;
- add multiple generations, selection, fallback, history, or retention;
- encode or decode bytes or define a checksum, repair, or quarantine rule;
- implement a filesystem checkpoint source;
- make checkpoint presence or validity a startup gate;
- add dirty-page analysis, replay start, redo, undo, rollback, compensation, or
  coordinator restoration;
- choose a retention floor, truncate, compact, or reclaim a log; or
- define external SQL Server values or native file compatibility.

## Consequences

The deterministic memory adapter now exercises the real ADR 0040 read boundary
without conflating fixture setup with publication. Repeated exact loads, source
failure, absence, and invalid evidence can be tested against a real memory WAL
while the two mutable sources remain non-overlapping.

Checkpoint publication remains a separate design problem. Its write operation
must treat returned errors as outcome-indeterminate, define what a successful
publication receipt proves, and choose replacement or generation semantics
before either memory or filesystem write adapters are added.
