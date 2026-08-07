# ADR 0059: Selected-Restart WAL Retention Analysis

- Status: Accepted
- Date: 2026-08-07
- Issue: #169
- Extends: ADR 0011, ADR 0018, ADR 0034, ADR 0037, ADR 0047, ADR 0053, ADR 0058
- Extended by: ADR 0060, ADR 0061, ADR 0068
- Follows: #168

## Context

ADR 0058 releases a selected-checkpoint restart only after revalidating its
private selected baseline, complete-current logical WAL analysis, transaction
restoration, repair decisions and outcomes, final page snapshots, coordinator,
and retained adapters. Its completed owner intentionally keeps that exact
startup evidence private for retention analysis.

Existing page-completeness evidence covers only pages named by logical WAL and
the selected checkpoint. It does not enumerate a store-only page. Existing
logical restart analysis also excludes physical restart-epoch allocation
frames. A logical frontier therefore cannot prove either that every current
page is backed by retained WAL or that the restored coordinator's physical
allocator state can be preserved.

This decision adds complete page-store inventory and allocator metadata ports,
then consumes one unused ADR 0058 completed owner to derive an immutable,
inclusive, non-authorizing WAL retention analysis. It does not truncate,
rewrite, compact, replace, or delete WAL. Issue #170 must independently
revalidate this point-in-time analysis before creating any reclamation permit.

## Crate and Dependency Boundary

`ntsql-transaction` owns both ports, all validation, the retention vocabulary,
the success and failure owners, and the consuming transition:

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

No crate or direct dependency edge changes. Domain code performs no I/O and
depends only on repository-owned ports. No logical record, physical frame,
page-store record, checkpoint codec, file header, checksum, marker, path, lock,
or synchronization format changes.

## Complete Page-Store Inventory Port

`DurablePageStoreInventorySource<N>` extends the existing current-snapshot
source. Its exclusive method returns an owned vector containing every current
durable page snapshot exactly once, strictly increasing by `PageNumber`, from
one stable store view.

Each observation retains exact page number, page version, all bytes, and the
lineage-bound logical position backing that snapshot. The vector and its
entries are untrusted evidence. Implementing the port, returning a vector, or
constructing an observation grants no page-write, durability, retention, or
reclamation authority.

The domain independently rejects:

- duplicate or descending page numbers;
- a store lineage different from current logical WAL;
- a snapshot position from another lineage;
- a position beyond the exact durable frontier;
- a position that is not a current logical record boundary;
- a record for another page;
- contradictory page version or bytes; and
- a transaction-owned page whose owner is unresolved.

Inventory allocation and projection remain fallible. Memory reports capacity
or projection failure. Filesystem additionally rejects a poisoned page-store
writer until reopen. Both adapters sort their owned current-page projection
deterministically without exposing their mutable page table.

## Retention Metadata Port

`DurableTransactionRestartRetentionMetadataSource` is implemented by the same
WAL adapter already owned by the completed restart. It reports:

- the exact WAL lineage;
- the greatest durably allocated transaction/coordinator epoch; and
- an optional oldest logical record required by source format or migration
  metadata.

Allocator high-water is physical persistence metadata. It is not a logical WAL
record, does not advance the logical durable frontier, and is never inferred
from that frontier. The memory adapter projects its retained allocator state.
The filesystem adapter projects the state reconstructed from synchronized
epoch-allocation frames and rejects poisoned state until reopen.

The optional logical constraint is an inclusive, lineage-bound record
requirement. Physical frame positions and any format-local rematerialization
recipe remain adapter concerns for issue #170; they are not invented as logical
records here.

## Observation Order and Coherence

`CompletedTransactionPageStorageRestartCheckpointReplay::
analyze_wal_retention(self)` consumes one completed owner that has not yet
released mutable live access.

The transition:

1. materializes the complete current page-store inventory;
2. captures the store lineage;
3. observes allocator and optional source-retention metadata;
4. enters the existing stable complete logical-WAL callback exactly once;
5. re-runs logical restart analysis and ADR 0058 completion validation;
6. reconciles every WAL or inventory page;
7. validates every candidate record boundary and derives the minimum; and
8. only then returns the retention-analyzed owner.

These observations are sequential, not an atomic multi-file snapshot. They are
coherent because the completed owner exclusively owns all three adapters, has
not released mutable references, and filesystem adapters retain their existing
exclusive locks. A cooperating writer cannot mutate any source between steps.
Non-cooperating filesystem writers remain outside the existing advisory-lock
trust boundary.

The transition writes nothing, allocates no transaction epoch, appends no
record, changes no page, and publishes no checkpoint.

## Revalidation of ADR 0058 Completion

Fresh logical evidence must still form one valid complete prefix. The transition
then directly consumes ADR 0058's private selected baseline, retained
complete-current analysis, restoration summary, and coordinator state. It does
not reconstruct them from aggregate public accessors.

The exact ADR 0058 checks are repeated:

- retained and fresh WAL lineage, frontier, transaction count, and ordered
  entries are equal;
- the selected/current transaction relationship still validates;
- restoration counts and persisted identity high-water remain exact;
- coordinator lineage and epoch still match;
- the coordinator remains unused at local sequence one with no lifecycle or
  staged-page entries; and
- the allocator metadata high-water equals that coordinator epoch.

Any live transaction, logical WAL append, transaction-table change, later
physical epoch allocation, or metadata lineage change makes completion stale.

## Whole-Store Page Reconciliation

The domain builds the deterministic union of page numbers named by complete
logical WAL and complete store inventory. For each page it computes the latest
required full image using existing ADR 0047 rules.

After successful ADR 0058 repair, every union entry must be exactly one of:

- no required raw or committed image and no current store snapshot; or
- one latest required raw or committed image and one exact current store
  snapshot backed by that image's logical record.

A required image with no snapshot is rejected. A valid older snapshot is
rejected as behind. A store-only snapshot is validated first and then rejected;
unbacked, foreign, contradictory, unresolved-owned, and beyond-frontier details
remain exact typed causes. This pass covers pages absent from the selected
checkpoint and pages never named by prior completeness analysis.

Page versions remain payload identity rather than a recency oracle. Logical
record order determines the latest required image.

## Inclusive Retention Requirements

The analysis records every exact requirement in deterministic order:

1. the selected checkpoint frontier, when present;
2. the selected inclusive replay start, when present;
3. each current stored-page backing record in `PageNumber` order;
4. each unresolved transaction's first owned-page record in persisted identity
   order; and
5. the optional source/format logical constraint.

The active selected checkpoint is therefore represented directly by its exact
frontier and replay boundary. No later mutable checkpoint observation can
substitute for the checkpoint that governed startup.

Every candidate must name a same-lineage logical record boundary at or before
the exact current frontier. The retained floor is the minimum candidate and is
inclusive: that record and every later record remain required.

When no candidate exists, `retained_first_record` is `None`, explicitly meaning
that no logical record in the analyzed prefix is required. The implementation
never computes `frontier + 1`, assumes dense positions, or uses a numeric
sentinel. An exact `u64::MAX` record remains a valid inclusive floor without
overflow.

Allocator high-water is retained independently from this logical floor. A
logical no-record requirement never permits loss of physical allocator state.

## Success Owner and Point-in-Time Evidence

Success returns
`WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay`. It owns
the complete ADR 0058 owner plus one privately constructed
`DurableTransactionRestartWalRetentionAnalysis`.

The analysis exposes inert inspection only:

- persistent identity and exact analyzed lineage;
- optional logical durable frontier;
- opaque inclusive floor;
- physical allocator epoch high-water;
- ordered exact requirements;
- complete store-page count; and
- unresolved transaction count.

The wrapper preserves shared and mutable live access and checkpoint publication
by delegating to the retained completed owner. Those later operations do not
update either startup completion evidence or retention evidence. Issue #170
must detect such staleness before authorizing reclamation.

Consuming `into_parts` transfers the exact coordinator, WAL, page store,
completion evidence, retention analysis, and checkpoint source. Private fields
prevent assembling a success owner or substituting separately obtained
evidence.

## Failure, Drop, and Reopen

`FailedTransactionPageStorageRestartCheckpointWalRetentionAnalysis` privately
retains the complete ADR 0058 owner and exactly one cause:

- page inventory source failure;
- retention metadata source failure;
- stable logical WAL source failure; or
- deterministic evidence contradiction.

The failure exposes only the exact error and inert restoration summary. It has
no retry, adapter accessor, `into_parts`, publication, live mutation, complete
recovery fallback, or success-shaped downgrade.

Because inventory and metadata observations can fail due to poisoned or
externally repaired state, same-owner retry would advertise freshness without a
new startup boundary. Resolution requires dropping the failure, correcting the
external cause if possible, reopening all adapters, and repeating selection,
repair, restoration, completion, and retention analysis.

## Filesystem Lock Continuity

ADR 0052's WAL, page-store, completeness-control acquisition order is unchanged.
Moving exact owners through inventory, metadata, stable WAL observation,
success, failure, live mutation, publication, and `into_parts` does not close,
clone, reopen, replace, or reacquire any descriptor.

Independent opens remain excluded while analysis runs and after success. Drop
releases all three locks. Fresh reopen reconstructs allocator high-water from
physical epoch frames, selects the published logical checkpoint, allocates a
strictly later coordinator epoch, and derives fresh retention evidence.

## Authority Boundary

The analysis, its floor, individual requirements, and both ownership wrappers
cannot become or substitute:

- a WAL append or durability position;
- a WAL truncation, compaction, replacement, deletion, or reclamation permit;
- a page-write or repair permit;
- a checkpoint baseline, publication permit, or receipt;
- a transaction coordinator, active lifecycle token, commit permit, or restored
  transaction;
- a complete committed-page recovery report;
- a client-visible diagnostic; or
- native format or external compatibility evidence.

Compile-fail boundaries reject floor and analysis construction, success-owner
construction, adapter release from failure, retry from failure, and use of the
analysis as durability or reclamation authority.

## Evidence and Compatibility Boundary

All behavior uses repository-authored WAL, transaction, page, checkpoint,
repair, restoration, ownership, allocator, lock, and deterministic-fault
contracts. No external product documentation, driver, SDK, fixture, oracle,
proprietary governance tool, or native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server recovery phase, checkpoint algorithm,
retention policy, backup policy, replication behavior, LSN value, error number,
diagnostic, persistent compatibility claim, or native file interpretation.

## Test Boundaries

- Domain tests derive exact requirements for checkpoint, raw, committed,
  unresolved, stored, source-constrained, empty, and maximum-position evidence.
- Duplicate, missing, behind, store-only, contradictory, unresolved-backed,
  foreign, unbacked, beyond-frontier, stale coordinator, stale allocator, and
  source failures remain fail-closed typed errors.
- Memory inventory sorts exact current snapshots and carries a completed
  selected repair through retention analysis, live mutation, publication,
  ownership transfer, and restart.
- Filesystem inventory rejects poison, sorts exact current snapshots, and
  preserves all three locks through analysis, live mutation, publication,
  transfer, fresh reopen, and a later allocator high-water.
- Before- and after-write repair retry both continue through successful
  completion and retention analysis.
- Existing recovery, checkpoint, codec, format, poison, lock, architecture, and
  governance tests remain valid.

## Non-Goals

This ADR does not:

- truncate, rewrite, compact, replace, delete, or reclaim WAL;
- define online reclamation or concurrent reader epochs;
- define backup, replication, HA, CDC, or long-running-query retention;
- persist the analysis or add a retention checkpoint format;
- automatically invalidate, delete, or republish the selected checkpoint;
- execute rollback, undo, abort, compensation, or lock-table reconstruction;
- expose persisted transactions as active runtime state;
- add a database-wide lock or atomic multi-file snapshot; or
- define external SQL Server semantics or native compatibility.

## Consequences

One completed selected restart can now prove, without mutation, that its entire
current page store is exactly backed by current logical WAL, its coordinator is
still fresh, and its physical allocator high-water is known independently of
the logical frontier. The result records one conservative inclusive floor
without overflow and remains incapable of reclaiming anything.

The next boundary is issue #170: revalidate this point-in-time evidence and
translate it into one branded, one-attempt, format-aware reclamation operation
that preserves required logical records and physical allocator metadata.
