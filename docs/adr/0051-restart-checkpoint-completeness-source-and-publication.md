# ADR 0051: Restart Checkpoint Completeness Source and Publication

- Status: Accepted
- Date: 2026-08-06
- Issue: #155
- Extends: ADR 0043, ADR 0048, ADR 0049, ADR 0050
- Extended by: ADR 0052, ADR 0053, ADR 0068

## Context

ADR 0050 validates one *borrowed* decoded completeness observation against an
exact retained WAL prefix and the current page store. Nothing can retrieve that
owned observation, and nothing can durably select an authoritative completeness
baseline.

The transaction-only ADR 0040 source and ADR 0042 publisher cannot be reused
for this. Their port shapes carry only the persistent identity, optional
frontier, and transaction entries. Routing a completeness baseline through them
would erase the page table and replay lower bound, or silently reinterpret the
independent ADR 0049 `NTSQCMP1` format as the ADR 0044 `NTSQCKP1` one.

The next boundary is therefore a completeness-specific read and publication
contract in the I/O-free transaction domain, proved by a deterministic
in-memory adapter, before any filesystem slot, startup consumer, replay
execution, repair, retention, or reclamation decision.

## Crate and Dependency Boundary

Only `ntsql-transaction` and `ntsql-storage-memory` production code and tests
change. The reviewed graph remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

No crate, dependency edge, architecture registration, filesystem API, byte
format, checksum, lock, or synchronization operation changes.
`ntsql-storage-file`'s codec module, `NTSQCKP1`/`NTSQCMP1` bytes, checkpoint
slot/path/control files, and the transaction-only source/publisher ports and
adapter are untouched. The transaction domain remains I/O-free.

## Sibling Completeness Retrieval Port

`DurableTransactionRestartCheckpointCompletenessBaselineSource` exposes:

```text
load_restart_checkpoint_completeness_baseline()
    -> Result<Option<OwnedDecodedCompleteness>, SourceError>
```

It is a sibling of `DurableTransactionRestartCheckpointBaselineSource`, not its
subtrait, and not the subtrait of the completeness publisher. A concrete
adapter may implement either, both, or neither; generic code may still require
exactly one.

`Ok(None)` means the source has no current completeness slot. `Ok(Some(...))`
returns one complete owned untrusted ADR 0049 observation, retaining every
decoded transaction, page, required-image, stored-position, and replay field
unnormalized. `Err` returns no candidate.

Like ADR 0040's port, this deliberately models one optional current slot and
defines no generation, ordering, selection, fallback, history, replacement,
retention, or concurrency semantics. A later multi-generation or retention
design is expected to supersede rather than reinterpret it. The port also
defines no byte encoding: a memory adapter may structurally copy raw fields
while a future filesystem adapter decodes its own reviewed format.

## Sequential Source Validation

`RestartAnalyzedTransactionPageStorage::
validate_restart_checkpoint_completeness_baseline_from_source` is available on
exactly the final-owner impl that already requires both
`DurableTransactionRestartAnalysisSource<N>` and
`DurablePageStoreSnapshotSource<N>`. It performs two non-overlapping source
operations:

1. call the completeness source and obtain `None` or one complete owned
   snapshot;
2. end that mutable checkpoint-source borrow;
3. return `Ok(None)` immediately for absence; or
4. borrow the owned snapshot and invoke the existing unchanged ADR 0050
   validation against the current WAL source and current page store.

No checkpoint-source callback surrounds the WAL callback, and no decoded borrow
comes from either source while both are active. Absence and checkpoint-source
failure therefore prove zero WAL callbacks and zero page-store observations.

Success returns only a newly re-derived authoritative
`DurableTransactionRestartCheckpointCompletenessBaseline`, never a value built
from decoded fields, because every comparison stage stays inside ADR 0050. No
prefix selection, transaction analysis, page inventory, snapshot
classification, replay derivation, or comparison rule is reimplemented here.

`DurableTransactionRestartCheckpointCompletenessBaselineSourceValidationError<
CheckpointSourceError, WalSourceError, StoreError>` distinguishes an exact
`CheckpointSource` failure from one boxed `BaselineValidation` failure, with a
complete `Display` and `Error::source` chain back to the exact ADR 0050 cause.
This decision adds no lock acquisition or lock-order contract.

## Sibling Completeness Publisher Port

`DurableTransactionRestartCheckpointCompletenessBaselinePublisher` accepts a
borrowed authoritative completeness baseline plus one private
`DurableTransactionRestartCheckpointCompletenessBaselinePublicationPermit`. It
returns unit success or its exact adapter error and does not construct the
public receipt itself.

`Ok(())` reports an all-or-nothing durable replacement: the adapter's temporary
selected slot is exactly the supplied baseline. As in ADR 0042 that guarantee
lasts only until another publication attempt is invoked, because a later failed
attempt may nevertheless have installed its own value. Every publisher `Err`
after invocation is outcome-indeterminate; the domain never reclassifies a
nominally before-effect adapter error as retryable, because the abstract port
cannot independently verify physical non-effect.

This ADR defines no temp file, rename, frame, checksum, synchronization, or
repair algorithm. An adapter reporting success without satisfying the port
contract is an adapter defect outside the domain proof.

## Invariant Completeness Publication Permit

The permit carries only identifying metadata copied from the owner-prepared
baseline:

- persistent log ID;
- optional numeric durable frontier;
- transaction-entry count; and
- page-entry count.

The page count is what makes it a genuinely independent permit rather than a
widened transaction-only one. The permit deliberately exposes no page number,
state, required image, stored position, or replay field: those belong to the
baseline the publisher already borrows, and re-exposing them through the permit
would create a second, weaker path to replay-shaped data.

Private fields prevent construction, the type is not `Clone`, and an invariant
higher-ranked attempt brand prevents lifetime widening or escape from the owner
operation. A publisher must reject identifying permit fields that do not match
the supplied baseline before physical effect.

The permit proves only that this call was initiated through the restart-
analyzed owner with those identifiers. It does not independently prove baseline
contents, adapter honesty, durability, replay safety, or retention authority.

## One-Window Preparation Then Publication

`RestartAnalyzedTransactionPageStorage::
publish_restart_checkpoint_completeness_baseline_from_current_prefix`:

1. calls only the existing ADR 0047/ADR 0048 one-window
   `prepare_restart_checkpoint_completeness_baseline_from_current_prefix`;
2. ends every WAL callback and shared page-store borrow;
3. creates one invariant permit from the four baseline identifiers;
4. invokes the separate publisher exactly once; and
5. converts unit success or publisher error into the staged domain result.

The immutable startup analysis is not replaced and the page store is never
mutated. A preparation failure — lineage mismatch, current analysis failure, a
page-store observation failure, or ephemeral-lineage baseline preparation —
proves the publisher was not invoked and leaves the owner available for a later
fresh attempt.

As in ADR 0042 the two completeness compositions necessarily touch adapters in
opposite orders (validation reads the checkpoint then the WAL/store;
publication reads the WAL/store then writes the checkpoint). Neither operation
acquires an adapter lock or holds one source operation while entering the
other, so these orders express data dependencies, not a lock hierarchy. A
future filesystem composition must define one global object-open and lock
acquisition order independently.

## Receipt and Outcome-Indeterminate Attempt

Publisher success constructs a non-cloneable
`DurableTransactionRestartCheckpointCompletenessBaselinePublicationReceipt`
that privately owns the exact baseline used in the call but publicly exposes
only the persistent log ID, optional frontier, transaction count, and page
count. Its debug representation contains only those four identifiers. It
exposes neither the baseline, its transaction entries, its page table, nor its
replay lower bound.

Any publisher error constructs an
`IndeterminateDurableTransactionRestartCheckpointCompletenessBaselinePublication`
with the same four accessors, the same restricted debug output, and no baseline
extraction or direct retry method. It retains the exact attempted baseline
behind one private indirection so the terminal error stays cheap to move; that
indirection changes no exposed field and creates no additional capability.

`DurableTransactionRestartCheckpointCompletenessBaselinePublicationError<
PublisherError>` pairs that token with the exact publisher cause. The outer
`DurableTransactionRestartCheckpointCompletenessBaselineCurrentPublicationError<
WalSourceError, StoreError, PublisherError>` distinguishes:

- `Preparation`, the exact reused ADR 0048 current-preparation failure before
  publisher invocation; and
- `Publication`, after invocation and therefore indeterminate.

Every `Error::source` layer remains available. No resolution, retry,
replacement, quarantine, or repair decision follows from the indeterminate
token in this ADR. As in ADR 0042, token and receipt non-clonability is API and
diagnostic discipline, not a global linearity proof: the authoritative baseline
itself remains cloneable inert metadata that can be re-derived from current
evidence.

## Deterministic Memory Completeness Slot

`InMemoryTransactionRestartCheckpointCompletenessBaselineSource` is a distinct
adapter that owns one optional untrusted completeness observation and
implements both new sibling ports. The existing
`InMemoryTransactionRestartCheckpointBaselineSource` is unchanged and keeps its
own slot, load fault plan, and publication fault plan; the two adapters share
no state and neither can substitute for the other's ports.

`empty()` and `seeded(observation)` remain deterministic fixture construction.
A seed is not a publication, receipt, validated baseline, startup choice, or
durability proof. `slot()` observes the exact current untrusted slot for test
inspection only.

### Fallible Exact Reservation

Both directions reserve exactly, and neither relies on an infallible
`Vec::clone` on a success path:

- loading reserves the nested transaction-entry vector with
  `try_reserve_exact`, failing as `TransactionCapacityExhausted`, then reserves
  the page vector, failing as `PageCapacityExhausted`; and
- publication lowering does the same before any slot replacement, failing as
  the matching publication-error variants.

Loading never consumes or mutates the slot, so repeated loads return fresh
equal owned values.

### Exact Structural Lowering

Publication lowering preserves every field without normalization:

- persistent log ID as the raw `u128`, and the optional frontier unchanged;
- per transaction entry: epoch, sequence, first and last owned-page positions,
  owned-page record count, and committed or uncommitted state with its exact
  commit position;
- per page entry, in unchanged numeric order: raw page number, the state
  discriminant, the optional required image (raw versus committed-transaction
  kind with its epoch, sequence, page position, and commit position), and the
  optional stored position, each lowered as the independent decoded fields ADR
  0049 defines; and
- the replay lower bound: kind, optional frontier, optional inclusive position,
  and the optional cause with its exact page number or owner epoch/sequence.

### Permit Verification and Deterministic Faults

Before allocation, slot mutation, or publication-fault consumption, the adapter
compares all four permit identifiers with the supplied baseline. Any difference
returns `PublicationPermitMismatch` containing both complete quadruples and
leaves the slot and every fault plan unchanged. Safe external code cannot
construct a mismatched permit; the check enforces the port's adapter obligation
and fails closed if the domain boundary is ever changed incorrectly.

The slot carries two independent one-shot fault plans:

- `BeforeLoad` fails the next load before inspecting or copying the slot;
- `BeforeReplace` returns its exact injected error before candidate allocation
  or slot replacement, preserving the old slot; and
- `AfterReplace` installs the complete exact new untrusted slot and then
  returns its exact injected error.

Reaching a matching point clears only that plan, so a load fault may remain
armed across publication and vice versa. Attempting to replace an already armed
plan returns the matching `...FaultAlreadyArmed` error with the retained and
rejected points and changes neither plan.

These different physical effects are visible only through direct adapter
inspection. Once the publisher has been invoked, the transaction-domain owner
maps both errors to the same outcome-indeterminate boundary, so the
before-effect label grants no domain retry permission and the after-effect
label grants no authoritative resolution.

## Untrusted Read-Back

Even immediately after successful publication, a later load returns an
untrusted `OwnedDurableTransactionRestartCheckpointCompletenessBaseline
Observation`. The real restart-analyzed owner must still validate it against
the current retained WAL prefix and current page store under ADR 0050. Neither
this adapter, the publisher result, nor the receipt bypasses that validation.

## Allocation and Complexity

Domain source validation allocates nothing beyond what ADR 0050 already
allocates: `as_observation()` is allocation-free and the checkpoint source owns
its own allocation boundary. Domain publication allocates nothing beyond ADR
0047's analysis and ADR 0048's exact baseline reservation, plus one
error-path indirection for the indeterminate token.

Both final-owner operations perform exactly one WAL callback and exactly one
page-store observation per distinct selected page, matching ADR 0047's existing
one-window guarantee. No success-shaped allocation fallback or throughput claim
is introduced.

## Authority Boundary

The completeness source port, publisher port, permit, receipt, indeterminate
token, and every new error cannot directly create or satisfy:

- transaction lifecycle or coordinator state;
- dirty, clean, or recovery-permitted pages, or a committed-page recovery write
  permit;
- `LogLineage`, `LogSequenceNumber`, WAL append, flush, or durability fences;
- a recovered or restart-analyzed storage owner;
- an authoritative completeness baseline without current-prefix validation;
- the transaction-only checkpoint source, publisher, permit, or receipt;
- checkpoint startup selection, replay, redo, undo, rollback, or compensation;
- a dirty-page table or replay start; or
- retention floors, truncation, compaction, or reclamation.

Compile-fail tests enforce these boundaries: permit construction, cloning,
widening, retention, and replay-field exposure; receipt and token construction,
cloning, baseline/page/replay extraction, and direct retry; direct publisher
invocation without the private permit; source-versus-publisher port
substitution; detached observation or baseline self-validation and
self-publication; and substitution for WAL durability, restart-analysis, page
store, page snapshot, recovery store, active transaction, and storage-owner
authority.

## Evidence and Compatibility Boundary

All behavior uses repository-authored WAL, page, transaction, recovery,
restart-analysis, completeness, baseline, storage-ownership, and
persistence-port contracts plus the deterministic memory adapter. No external
product documentation, driver, SDK, fixture, oracle, proprietary governance
tool, or native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server checkpoint source, publication point,
transaction/page table, dirty-page table, LSN, recovery phase, persistent
format, error, diagnostic, or compatibility behavior.

## Test Boundaries

- Fake completeness retrieval proves strict `checkpoint` then `wal` operation
  order with no nested callback, and exact present, absent, invalid, and
  source-error slots preserve call counts and nested causes.
- Absence and checkpoint-source failure add zero WAL callbacks and zero
  page-store observations; a zero decoded persistent identity still fails
  before the callback.
- Successful publication observes exact `wal` then `checkpoint-publish` order,
  invokes the publisher exactly once, and returns receipt identifiers —
  including page count — for the current, not startup, frontier.
- The fake publisher verifies all four permit identifiers before effect and
  structurally lowers the exact baseline into its sibling untrusted read slot;
  loading that slot does not authorize it, and a subsequent checkpoint-read
  then WAL/store validation returns the exact authoritative baseline.
- Preparation and validation each perform one WAL callback and one page-store
  observation per distinct selected page, and the immutable startup analysis,
  page-store contents, and WAL records are unchanged across every path.
- Current WAL failure, page-store observation failure, and ephemeral-lineage
  baseline preparation each prove zero publisher calls and leave the owner
  usable; before-effect and after-effect publisher errors produce the same
  outer outcome-indeterminate variant with exact attempted identifiers and
  exact sources.
- Real memory-adapter integration proves exact publish, non-consuming
  read-back, and real-owner validation round-trip every transaction, page,
  required-image, stored-position, and replay field; `BeforeReplace` preserves
  the old slot while `AfterReplace` installs the full new slot; matching faults
  are one-shot and load/publication plans remain independent; and a later fresh
  attempt succeeds once a fault clears.
- An empty current prefix publishes and validates an exact empty baseline with
  no frontier, no transactions, no pages, and an `AfterFrontier` replay bound.
- Capacity and permit-mismatch errors retain their exact counts and identifier
  quadruples and have no nested source.
- Existing analysis, preparation, validation, recovery, ownership, adapter,
  format, architecture, compile-fail, and governance tests remain valid.

## Non-Goals

This ADR does not:

- add a filesystem completeness source or publisher, checkpoint slot, path,
  control file, temporary file, or atomic rename;
- change `NTSQCMP1`/`NTSQCKP1` bytes, add a codec, checksum, digest, timestamp,
  marker, or synchronization point;
- add generation selection, history, fallback, deletion, or retention;
- define publication retry or authoritative indeterminate resolution;
- make completeness presence or validity a startup gate or failure typestate;
- execute replay, redo, undo, rollback, compensation, page repair, transaction
  restoration, or coordinator reconstruction;
- choose a retention floor, truncate, compact, or reclaim a log;
- acquire locks or choose a global filesystem lock order; or
- define external SQL Server values, native file formats, or compatibility
  claims.

## Consequences

The transaction domain can now retrieve one owned untrusted completeness slot
without nesting its operation around WAL/store validation, and can separate a
retryable one-window completeness preparation from an outcome-indeterminate
publication attempt that exposes only identifying metadata.

A deterministic memory adapter exercises both boundaries with exact
replacement, structural read-back, fallible exact reservation, and independent
before/after-effect faults, while published values stay untrusted on load and
acquire authority only through ADR 0050 validation.

ADR 0052 later supplies the persistent completeness source, publisher,
replacement, synchronization, and global lock order. ADR 0053 consumes this
unchanged source port before page recovery and retains the same concrete source
through fallback and later publication. Replay execution, dirty-page repair, and
WAL retention/reclamation remain later boundaries.
