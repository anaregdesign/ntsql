# ADR 0039: Source-Relative Restart Checkpoint Validation

- Status: Accepted
- Date: 2026-08-06
- Issue: #130
- Extends: ADR 0038
- Extended by: ADR 0040, ADR 0049, ADR 0050

## Context

ADR 0038 lets the final restart-analyzed storage owner prepare one inert
persistent-lineage checkpoint baseline from its immutable startup analysis.
Future adapters can encode the baseline's public numeric fields, but bytes read
back from storage are untrusted. A decoder must not construct the authoritative
private baseline type or normalize invalid values before domain validation.

The current WAL adapters still retain their complete durable logical record
prefixes. That permits a correctness-first validator to re-derive the exact
baseline claimed by decoded fields and compare every field. This decision adds
that source-relative validation boundary before choosing any checkpoint format.

Validation remains non-authorizing and optional. A mismatch does not make the
stored startup analysis false, release a new adapter, or grant recovery,
replay, or log-reclamation authority.

## Crate and Dependency Boundary

Only `ntsql-transaction` owns decoded observations, validation, comparison, and
typed errors. Existing memory integration exercises the generic operation:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

No crate, dependency edge, architecture registration, adapter port, persistent
format, file, checksum, synchronization point, marker, repair rule, poison
rule, or lock changes. Domain crates remain I/O-free.

## Untrusted Decoded Observations

`DurableTransactionRestartCheckpointBaselineObservation<'evidence>` borrows an
adapter-owned slice of decoded entry observations and retains:

- raw `u128` persistent log identity;
- raw optional `u64` frontier; and
- decoded entries in persisted order.

Each entry observation retains raw:

- epoch and sequence;
- optional first and last owned-page positions;
- `u64` owned-page count; and
- a distinct untrusted state observation with an optional raw commit position.

Constructors are infallible. Zero identities, zero positions, contradictory
range/count/state combinations, duplicates, and unsorted entries remain
unchanged so exact validation can reject rather than normalize them. Decoded
state, entry, and baseline observations are separate types from ADR 0038's
authoritative values and cannot convert into them.

The observation borrows memory owned independently by the checkpoint decoder.
The validator returns only a newly re-derived owned baseline, so neither
decoded memory nor WAL callback evidence enters the result.

## Final-Owner Current-Prefix Operation

`RestartAnalyzedTransactionPageStorage::
validate_restart_checkpoint_baseline_against_current_prefix` is available when
the owned source implements `DurableTransactionRestartAnalysisSource<N>`.

The method mutably borrows only the current WAL source and invokes its
stable-prefix callback at most once. The page store and immutable startup
analysis are untouched.

The method name is explicit: validation uses the source's **current** durable
prefix. It does not compare against `restart_analysis()`, whose point-in-time
frontier may be older after live activity.

There is no public free validator or authoritative-baseline constructor.
Private helpers keep the same comparison implementation behind the final owner.

## Pre-Callback Identity and Frontier Rejection

Validation performs cheap candidate-independent and decoded-field checks before
asking the source to project its current prefix:

1. the current source lineage must expose a `PersistentLogId`;
2. decoded raw persistent ID must be nonzero;
3. decoded and current persistent IDs must match exactly; and
4. a decoded `Some` frontier must be nonzero.

Each failure is typed and invokes no callback or allocation in this operation.
The validator never invents, hashes, defaults, or aliases a persistent
identity.

## Empty Prefix

A decoded `None` frontier selects the empty prefix before logical record zero,
independent of the current WAL length. The validator re-derives the canonical
empty baseline and requires zero decoded transaction entries.

This match is intentionally near-vacuous: it proves only that the decoded value
is the empty shape for the same persistent lineage. It says nothing about the
health of later current records and is not recovery, replay, or truncation
evidence.

The source callback is still invoked, so a source failure before or after the
callback remains authoritative even for the empty selection.

## Numeric Frontier Selection

For decoded `Some(F)`, validation:

1. rejects a missing, foreign-lineage, zero, or recordless current frontier;
2. rejects `F` numerically beyond the current durable frontier;
3. scans for the first logical observation at exact numeric position `F`; and
4. analyzes only the prefix ending at that observation with a lineage-bound
   frontier at `F`.

Position gaps remain valid WAL allocation behavior. A value inside a gap is not
a checkpoint record boundary. When no exact boundary exists, the validator
analyzes the complete current stream: valid current evidence produces
`CheckpointFrontierNotRecordBoundary`, while malformed current evidence retains
its exact existing restart-analysis cause.

The selected prefix analysis reuses ADR 0034. It validates exact lineage,
global order, tail equality, duplicate/cross-kind positions, transaction
ordering, page/commit state, and fallible table construction before an expected
baseline exists.

## Suffix Isolation

Once an exact boundary is found, records after it do not participate in this
baseline comparison. A transaction contradiction or malformed order wholly
after a valid stale boundary does not make the older summarized prefix
different.

This is a narrow statement about decoded-baseline fidelity. Successful
validation does not declare the suffix healthy or authorize recovery through
it. Selecting a boundary that includes the malformed record fails with the
exact selected-prefix analysis cause. Asking for a missing boundary validates
the current stream before classifying the gap.

## Exhaustive Comparison

The authoritative selected-prefix analysis is projected through ADR 0038's
private baseline builder. Validation then compares:

- exact persistent ID, checked before the callback;
- frontier `None`/`Some` shape and numeric value;
- transaction count and persisted order;
- every epoch and sequence;
- first and last owned-page `None`/`Some` shape and numeric value;
- exact `u64` owned-page count;
- uncommitted/committed state variant; and
- exact commit position.

The first mismatch returns its index plus a boxed authoritative entry and exact
copied decoded entry. No partial baseline escapes.

Success returns the authoritative re-derived baseline, never a value built from
decoded fields. The result remains the inert ADR 0038 type and gains no
additional authority from validation.

## Error and Source Precedence

`DurableTransactionRestartCheckpointBaselineValidationError` distinguishes:

- an exact source failure; and
- one boxed validation-evidence error.

Evidence errors preserve identity, frontier, current-prefix, selected-prefix,
baseline-preparation, transaction-count, and entry mismatch causes.
`Error::source` retains nested source, restart-analysis, and preparation errors.

If the source invokes the callback and then returns an error instead of its
output, that source error is authoritative. No decoded mismatch or partially
computed baseline escapes. Validation does not mutate the source or store, so a
subsequent explicit call may proceed if the source remains usable.

## Allocation and Complexity

Decoded entry storage belongs to the outer decoder. Observation construction
allocates nothing.

For an exact boundary, the validator analyzes only the selected prefix and then
fallibly reserves the authoritative baseline transaction count. A future or
missing boundary does not allocate a decoded table. A valid numeric gap may
require complete current-stream analysis to prove that the source itself is
well formed.

The operation is correctness-first and may rescan source observations already
projected by the adapter. It makes no throughput claim and introduces no
success-shaped allocation fallback.

## Complete-WAL Retention Dependency

Source-relative re-derivation is sound only while the WAL source retains every
logical observation through the selected frontier. Current adapters do so
because no log truncation or reclamation exists.

A future retention boundary may remove records summarized by a checkpoint.
That work must not reuse this validator and interpret missing observations as a
mismatch. It requires a separately reviewed owning checkpoint authority,
generation/selection policy, retained anchor, and recovery contract before any
prefix can be reclaimed.

## Authority Boundary

Decoded observations, validation errors, and the returned baseline cannot
directly create or satisfy:

- transaction lifecycle or coordinator state;
- dirty/clean pages or live/recovery write permits;
- `LogLineage`, `LogSequenceNumber`, WAL append, or durability fences;
- source/store ownership or a checkpoint adapter;
- redo, undo, rollback, compensation, or replay;
- a dirty-page table, replay start, retention floor, truncation, or
  reclamation.

Validation failure does not consume or downgrade the live owner. This remains
safe only because no behavior uses a validated baseline as recovery or
retention authority.

## Evidence and Compatibility Boundary

All behavior uses repository-authored WAL, restart-analysis, baseline, and
storage-ownership contracts. No external product documentation, driver, SDK,
fixture, oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format
is consulted.

This decision defines no SQL Server checkpoint validation, transaction table,
dirty-page table, LSN, recovery phase, persistent format, error, diagnostic, or
compatibility behavior.

## Test Boundaries

- Exact current and real stale memory baselines re-derive byte-for-field
  equivalent authoritative values while leaving the page store unchanged.
- Empty and raw-page-only stale prefixes remain distinct.
- Ephemeral current lineage, zero/foreign decoded identity, and zero decoded
  frontier fail before callback.
- Future, valid-gap, missing, foreign, and zero current frontiers fail
  distinctly.
- Epoch, sequence, first/last range shape and value, count, state variant, commit
  position, and transaction-count mismatches retain exact values.
- A malformed suffix is ignored for an earlier exact boundary, fails when
  selected, and remains a current-prefix error for a missing boundary.
- Source errors before and after callback remain authoritative and later
  validation can proceed after one-shot test faults clear.
- Compile-fail tests prevent decoded state/entry/baseline observations from
  becoming lifecycle, page, recovery, WAL, or storage authority.
- Existing analysis, preparation, recovery, ownership, adapter, format,
  architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- add memory or filesystem checkpoint encoding, decoding, storage, publication,
  selection, reopen, repair, or quarantine;
- make checkpoint validation a startup gate or failure typestate;
- select among generations or add a digest, checksum, timestamp, marker,
  synchronization point, temporary file, or atomic rename;
- add dirty-page analysis, replay start, redo, undo, rollback, compensation,
  coordinator restoration, or active transaction reconstruction;
- choose a retention floor, truncate, compact, or reclaim a log;
- make an empty baseline proof of current-source health;
- replace complete-prefix validation needed by future recovery; or
- define external SQL Server values or native file compatibility.

## Consequences

A future adapter can decode raw checkpoint fields into non-authorizing borrowed
observations and ask the final owner to prove that every field exactly matches
the claimed currently retained WAL prefix.

The next separately reviewed work can define one deterministic in-memory
checkpoint persistence port/adapter around these observations. Filesystem
encoding, atomic publication, corruption handling, startup ownership, dirty
pages, replay, and log retention remain later boundaries.
