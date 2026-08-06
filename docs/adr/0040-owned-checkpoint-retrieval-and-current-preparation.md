# ADR 0040: Owned Checkpoint Retrieval and Current Preparation

- Status: Accepted
- Date: 2026-08-06
- Issue: #132
- Extends: ADR 0039
- Extended by: ADR 0041, ADR 0045

## Context

ADR 0039 validates one borrowed decoded checkpoint observation against a claimed
retained WAL prefix. A future persistence adapter must first obtain those
decoded fields.

Nesting a checkpoint-source callback around ADR 0039's WAL-source callback
would hold two stable source operations simultaneously and silently establish a
checkpoint-to-WAL ordering contract. That would be premature before checkpoint
files, open ordering, or lock semantics exist.

The first retrieval boundary therefore returns an owned untrusted snapshot and
ends the checkpoint-source call before WAL validation begins. This decision
also adds an explicit operation for preparing a baseline from the current WAL,
distinct from ADR 0038's immutable startup baseline.

Checkpoint publication remains separate. Read-side retrieval and current
preparation can be reviewed without choosing write-outcome, atomic replacement,
or generation semantics.

## Crate and Dependency Boundary

Only `ntsql-transaction` owns the owned observation, retrieval port,
current-preparation error, and sequential validation composition. Existing
memory integration exercises current preparation:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

No crate, dependency edge, architecture registration, adapter implementation,
I/O operation, format, file, checksum, marker, lock, synchronization point,
repair rule, or poison rule changes. Domain crates remain I/O-free.

## Owned Untrusted Observation

`OwnedDurableTransactionRestartCheckpointBaselineObservation` owns:

- raw decoded `u128` persistent log identity;
- raw optional numeric frontier; and
- a `Vec` of ADR 0039 entry observations in decoded order.

Construction is infallible and performs no normalization. Zero IDs, zero
frontiers, invalid identities, contradictory ranges, arbitrary counts, and
state contradictions remain exact untrusted data.

Read-only accessors expose the raw fields. `as_observation()` borrows the owned
value through ADR 0039's existing validation shape without copying or
allocation. The owned and borrowed observation types cannot become an
authoritative baseline, transaction/page capability, log position, or storage
owner.

The outer checkpoint source owns any allocation and decoding failures needed to
produce this complete value. Returning it does not prove that its fields are
valid or that any checkpoint was durably published.

## Temporary Single-Slot Retrieval Port

`DurableTransactionRestartCheckpointBaselineSource` exposes:

```text
load_restart_checkpoint_baseline()
    -> Result<Option<OwnedDecodedCheckpoint>, SourceError>
```

`Ok(None)` means the source has no current checkpoint slot. `Ok(Some(...))`
returns one complete owned untrusted snapshot. `Err` returns no candidate.

The port deliberately models one optional current slot. It defines no
generation, ordering, selection, fallback, history, replacement, retention, or
concurrency semantics. A later multi-generation or retention design is
expected to supersede rather than reinterpret this temporary shape.

The port defines no byte encoding. An in-memory adapter may structurally copy
raw fields; a future filesystem adapter may decode its own reviewed format.
Both still return the same untrusted domain observation.

## Current-Prefix Preparation

`RestartAnalyzedTransactionPageStorage::
prepare_restart_checkpoint_baseline_from_current_prefix`:

1. mutably borrows only the owned WAL source;
2. invokes ADR 0034 current durable restart analysis exactly once;
3. projects the successful analysis through ADR 0038's private baseline helper;
4. leaves the page store untouched; and
5. does not update the immutable startup analysis.

The method name makes currency explicit. ADR 0038's
`prepare_restart_checkpoint_baseline` continues to project the startup
analysis. The new operation re-reads the current durable prefix, which may have
advanced through live work.

The result is still inert metadata. Re-analysis does not make it a publication
receipt or recovery authority.

## Sequential Source Validation

`validate_restart_checkpoint_baseline_from_source` performs two non-overlapping
source operations:

1. call the checkpoint source and obtain `None` or one complete owned snapshot;
2. end that mutable checkpoint-source borrow;
3. return `Ok(None)` immediately for absence; or
4. borrow the owned snapshot and invoke ADR 0039 validation against the current
   WAL source.

No checkpoint-source callback surrounds the WAL callback. No decoded borrow
comes from either source while both are active.

This decision adds no lock acquisition or lock-order contract. A future
filesystem composition must separately define object open order and any
lifetime locks. Existing WAL/page-store ownership remains unchanged.

## Error Boundary

Current preparation distinguishes:

- exact current analysis/source/evidence failure; and
- exact persistent-lineage, capacity, or count-width baseline-preparation
  failure.

Owned-source validation distinguishes:

- exact checkpoint-source failure; and
- one boxed ADR 0039 WAL/baseline-validation failure.

`Error::source` retains every nested cause. An absent source invokes no WAL
callback. A checkpoint-source failure also invokes no WAL callback and returns
no partial observation. A decoded mismatch leaves both the owner and owned
snapshot non-authorizing.

Neither operation consumes, downgrades, or mutates the final owner. A later
explicit call may proceed after a one-shot source failure clears.

## Allocation and Complexity

Owned observation construction accepts an already allocated entry vector.
Checkpoint-source implementations must use their own fallible allocation
boundary before returning success.

`as_observation()` allocates nothing. Sequential validation then has exactly
the ADR 0039 prefix-analysis and authoritative-baseline allocation behavior.
Current preparation has ADR 0034 analysis allocation followed by ADR 0038 exact
baseline reservation.

No success-shaped allocation fallback or throughput claim is introduced.

## Authority Boundary

The owned snapshot, source port, preparation result, validation result, and
errors cannot directly create or satisfy:

- transaction lifecycle or coordinator state;
- dirty/clean pages or live/recovery write permits;
- `LogLineage`, `LogSequenceNumber`, WAL append, or durability fences;
- checkpoint publication, persistence, selection, or startup ownership;
- redo, undo, rollback, compensation, or replay;
- dirty-page tables, replay starts, retention floors, truncation, or
  reclamation.

The returned current or validated baseline remains exactly the inert ADR 0038
type.

## Evidence and Compatibility Boundary

All behavior uses repository-authored WAL, restart-analysis, checkpoint
baseline, decoded observation, and storage-owner contracts. No external product
documentation, driver, SDK, fixture, oracle, proprietary governance tool, or
native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server checkpoint source, transaction table,
recovery phase, persistent format, error, diagnostic, or compatibility
behavior.

## Test Boundaries

- Owned decoded observations retain zero and contradictory raw fields exactly.
- Current preparation advances beyond immutable startup evidence after a real
  memory WAL write, includes the transaction suffix made durable by that later
  fence, and leaves the page store unchanged.
- Malformed current analysis and ephemeral current lineage retain exact nested
  errors, and the owner remains usable after test evidence is repaired.
- Fake checkpoint retrieval proves strict `checkpoint` then `wal` operation
  order with no nested callback.
- Exact present, absent, invalid, and source-error slots preserve call counts,
  nested causes, WAL non-invocation where required, and unchanged page-store
  counters.
- Compile-fail tests prevent owned observations and source ports from becoming
  authoritative baseline, WAL, or storage capabilities.
- Existing preparation, validation, analysis, recovery, ownership, adapter,
  format, architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- add a checkpoint store/publication port or write-outcome semantics;
- implement a memory or filesystem checkpoint adapter;
- add multiple generations, selection, fallback, replacement, or retention;
- define encoding, decoding, checksum, atomic publication, synchronization,
  repair, or quarantine;
- make checkpoint presence or validation a startup gate;
- add dirty-page analysis, replay start, redo, undo, rollback, compensation, or
  coordinator restoration;
- choose a retention floor, truncate, compact, or reclaim a log;
- define cross-adapter open or lock order; or
- define external SQL Server values or native file compatibility.

## Consequences

A future checkpoint source can return one complete owned untrusted slot without
nesting its operation around WAL validation. The final owner can also prepare a
baseline from the actual current durable prefix rather than only startup
evidence.

The next separately reviewed work can implement the single-slot read side in
the deterministic memory adapter. Publication/store semantics remain a
different issue so indeterminate write outcomes and replacement policy can be
designed explicitly.
