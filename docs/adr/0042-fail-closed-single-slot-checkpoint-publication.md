# ADR 0042: Fail-Closed Single-Slot Checkpoint Publication

- Status: Accepted
- Date: 2026-08-06
- Issue: #136
- Extends: ADR 0041
- Extended by: ADR 0043

## Context

ADRs 0038 through 0041 establish an inert authoritative transaction restart
baseline, source-relative validation of decoded fields, owned optional-slot
retrieval, current-WAL preparation, and a deterministic memory read source. No
reviewed operation can publish a baseline.

Publication has two distinct failure regions. Current WAL analysis or baseline
preparation can fail before a checkpoint publisher is called. Once a publisher
is called, any returned error does not prove whether physical state stayed old,
became new, became absent, or became unreadable.

Success also requires a precise contract. Returning the baseline itself would
not distinguish inert metadata from a publisher-reported durable result. The
first publication boundary therefore needs a private attempt permit, a separate
receipt, and terminal outcome-indeterminate state.

## Crate and Dependency Boundary

Only `ntsql-transaction` production code and tests change. The reviewed graph
remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

No crate, dependency edge, architecture registration, adapter implementation,
I/O operation, byte format, synchronization primitive, or physical lock changes.
The transaction domain remains I/O-free.

## Sibling Single-Slot Publisher Port

`DurableTransactionRestartCheckpointBaselinePublisher` is a sibling of
`DurableTransactionRestartCheckpointBaselineSource`. It does not inherit the
temporary read port and does not require every publisher to be readable. A
concrete adapter may implement either or both.

The publisher receives:

- a borrowed authoritative
  `DurableTransactionRestartCheckpointBaseline`; and
- one private
  `DurableTransactionRestartCheckpointBaselinePublicationPermit`.

The port returns unit success or its exact adapter error. It does not construct
the public publication receipt itself.

This is explicitly a temporary single-current-slot contract. A future
multi-generation design is expected to supersede it rather than infer
generation, history, fallback, or retention semantics from this port.

## Explicit Atomic Success Postcondition

`Ok(())` reports an all-or-nothing durable replacement: the adapter's temporary
selected slot is exactly the supplied baseline. The selected value remains
guaranteed only until another publication attempt is invoked.

A later attempt that returns an error may nevertheless have durably installed
its new value. Consequently, a prior successful receipt says nothing about slot
state after any later attempted publication. The guarantee is not phrased as
"until a later success."

The all-or-nothing success postcondition is deliberate. A future adapter must
choose and review a physical mechanism capable of meeting it. This ADR defines
no temp file, rename, frame, checksum, synchronization, or repair algorithm.
An adapter reporting success without satisfying the port contract is an adapter
defect outside the domain proof.

Every publisher `Err` is outcome-indeterminate after invocation. The domain
never reclassifies a nominally before-effect adapter error as retryable, because
the abstract port cannot independently verify physical non-effect.

## Invariant Publication Permit

The publication permit contains only identifying metadata copied from the
owner-prepared baseline:

- persistent log ID;
- optional numeric durable frontier; and
- transaction-entry count.

Private fields prevent construction. It is not `Clone`, and an invariant
higher-ranked attempt brand prevents lifetime widening or escape from the owner
operation. A publisher must reject identifying permit fields that do not match
the supplied baseline before physical effect.

The permit proves only that this call was initiated through the
restart-analyzed owner with those identifiers. It does not independently prove
baseline contents, adapter honesty, durability, replay safety, or retention
authority.

## Current-WAL Publication Composition

`RestartAnalyzedTransactionPageStorage::
publish_restart_checkpoint_baseline_from_current_prefix`:

1. runs exact current durable restart analysis;
2. privately prepares its authoritative baseline;
3. ends the WAL-source callback and mutable borrow;
4. creates one invariant permit from baseline identifiers;
5. invokes the separate publisher exactly once; and
6. converts unit success or publisher error into the staged domain result.

The immutable startup analysis is not replaced. The page store is never
inspected or mutated. Current analysis and baseline-preparation failures prove
the publisher was not invoked and leave the owner available for a later fresh
attempt.

No WAL callback encloses publisher execution, and no publisher callback encloses
WAL execution.

## Opposite Touch Orders Are Not Lock Orders

The two checkpoint compositions necessarily touch adapters in opposite orders:

- validation: checkpoint read, then WAL read;
- publication: WAL read, then checkpoint write.

Neither domain operation acquires an adapter lock or holds one source operation
while entering the other. The orders express data dependencies, not a lock
hierarchy.

A future filesystem composition must define one global object-open and lock
acquisition order independent of these per-operation touch orders. It must
acquire any lifetime locks consistently before either composition begins and
must not derive lock ordering from callback or method call order. This ADR does
not choose that global order.

## Publication Receipt

Publisher success constructs a non-cloneable
`DurableTransactionRestartCheckpointBaselinePublicationReceipt`. It privately
owns the exact baseline used in the call but publicly exposes only:

- persistent log ID;
- optional durable frontier; and
- transaction-entry count.

It exposes neither the baseline nor its entry slice. Its debug representation
also contains only those identifiers. The receipt proves only that the injected
publisher reported the exact all-or-nothing success required by the port. It
grants no startup, replay, recovery, or reclamation authority.

The authoritative baseline itself remains cloneable inert metadata and can be
re-derived from current WAL analysis. Therefore receipt non-clonability and
baseline non-extraction are API and diagnostic discipline, not global prevention
of repeated publication attempts.

## Outcome-Indeterminate Failure

Any publisher error constructs an
`IndeterminateDurableTransactionRestartCheckpointBaselinePublication` that
privately owns the exact attempted baseline. It exposes the same three
identifiers as the receipt, but no baseline extraction or direct retry method.

`DurableTransactionRestartCheckpointBaselinePublicationError` pairs that token
with the exact publisher cause. The outer
`DurableTransactionRestartCheckpointBaselineCurrentPublicationError`
distinguishes:

- `Preparation`, before publisher invocation; and
- `Publication`, after invocation and therefore indeterminate.

Every `Error::source` layer remains available. No resolution, retry,
replacement, quarantine, or repair decision follows from the indeterminate
token in this ADR.

As with the receipt, token non-clonability is not a global linearity proof
because inert baseline copies may already exist. It prevents the failed result
itself from becoming a success-shaped baseline or direct retry capability.

## Structural Read-Back Boundary

When one adapter implements both sibling ports, successful publication followed
by load must structurally lower the selected baseline without normalization:

- `PersistentLogId::get()` becomes the raw `u128` ID;
- the optional numeric frontier is unchanged;
- entry order is unchanged; and
- every epoch, sequence, page range, count, and state field is unchanged.

The loaded value remains an
`OwnedDurableTransactionRestartCheckpointBaselineObservation`. Even when it came
from a prior successful publication, it is untrusted source data and must pass
ADR 0039 current-prefix validation before the private authoritative baseline is
returned.

Neither the publisher result nor receipt bypasses that validation.

## Authority and Compatibility Boundary

The permit, publisher, receipt, indeterminate token, and errors cannot create or
satisfy:

- transaction lifecycle or coordinator state;
- WAL append, flush, restart-analysis, or lineage authority;
- page-store or recovery write authority;
- a recovered or restart-analyzed storage owner;
- decoded checkpoint validity, startup selection, replay, redo, undo, rollback,
  or compensation;
- dirty-page tables or replay starts; or
- retention floors, truncation, compaction, or reclamation.

All behavior uses repository-authored baseline, WAL, storage-owner, and
persistence-port contracts. No external product documentation, driver, SDK,
fixture, oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format
is consulted.

This decision defines no SQL Server checkpoint bytes, publication point,
transaction table, recovery phase, error, diagnostic, or compatibility result.

## Test Boundaries

- Successful publication observes exact `wal` then `checkpoint-publish` order
  and returns only receipt identifiers for the current, not startup, frontier.
- The fake publisher verifies permit identifiers before effect and structurally
  lowers the exact baseline into its sibling untrusted read slot.
- Loading that slot does not authorize it; a subsequent checkpoint-read then WAL
  validation returns the exact authoritative baseline.
- Current source failure and ephemeral-lineage baseline preparation both prove
  zero publisher calls.
- Before-effect and after-effect fake errors produce the same outer
  outcome-indeterminate variant, exact attempted identifiers, and exact source;
  their deliberately different fake slot effects remain observable.
- Preparation failure leaves the owner usable for a later fresh operation.
- Startup analysis and page-store observations and attempts remain unchanged
  across every path.
- Compile-fail tests reject permit construction, cloning, widening, direct port
  invocation or retention, receipt/token construction or cloning, baseline
  extraction, direct retry, and WAL, page-store, recovery-store, transaction, or
  restart-analyzed-storage authority substitution.
- Existing restart, checkpoint, recovery, adapter, architecture, and governance
  tests remain valid.

## Non-Goals

This ADR does not:

- implement a memory or filesystem publisher;
- define a physical atomic-replacement algorithm;
- add checkpoint bytes, checksum, synchronization, repair, or quarantine;
- add generation, selection, fallback, history, deletion, or retention;
- define publication retry or authoritative indeterminate resolution;
- make checkpoint presence or validity a startup gate;
- add dirty-page analysis, replay start, redo, undo, rollback, compensation, or
  coordinator restoration;
- acquire locks or choose global filesystem lock order;
- choose a retention floor, truncate, compact, or reclaim a log; or
- define external SQL Server values or native file compatibility.

## Consequences

The transaction domain now separates retryable current-baseline preparation from
an outcome-indeterminate publication attempt and exposes a precise
publisher-reported success receipt without returning the baseline as that
receipt.

The next adapter slice can implement the sibling publisher in deterministic
memory, including before/after effect faults and publish-then-load validation.
A filesystem publisher remains blocked on a separately reviewed physical
format, atomic replacement mechanism, synchronization, repair, and global lock
order.
