# ADR 0056: Atomic Selected Replay Page Repair

- Status: Accepted
- Date: 2026-08-06
- Issue: #166
- Extends: ADR 0029, ADR 0031, ADR 0032, ADR 0055
- Extended by: ADR 0057
- Follows: #163

## Context

ADR 0055 consumes one exact selected replay window into a private,
page-number-ordered repair table. Each entry retains exact prepared source state
and, when required, one raw or committed full-image target. Preparation is
read-only so every failure may still destroy the plan and enter complete
recovery.

The next boundary must apply those targets without exposing the retained WAL,
page store, checkpoint source, replay records, page images, or mutation
authority. It must also handle a process failure or adapter error between
durability and acknowledgement. A returned write error cannot prove whether the
target reached durable storage, and an earlier page may already be durable when
a later page fails.

This decision adds one consuming whole-plan executor, one replay-repair-only
compare-and-replace port, one private branded permit per store invocation, and a
retrying failure owner. It does not claim multi-page atomicity.

## Crate and Dependency Boundary

`ntsql-transaction` owns the candidate vocabulary, comparison, private permit
construction, store port, whole-plan transition, inert outcomes, successful
owner, failed owner, and typed causes. `ntsql-storage-memory` and
`ntsql-storage-file` implement only the new store port:

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

No crate, direct dependency edge, architecture registration, persistent format,
WAL record, checkpoint slot, checksum, marker, path, lock primitive, or
synchronization contract changes. The transaction domain remains I/O-free.

## Consuming Whole-Plan Transition

`PreparedTransactionPageStorageRestartCheckpointRepairs::
execute_page_repairs(self)` consumes the only prepared owner. Once called, no
outcome offers ADR 0055's `decline_page_repairs` or complete-recovery fallback.
That fallback is safe only before the effectful transition begins.

Before any page is observed, the executor:

1. clears or creates one outcome vector;
2. fallibly reserves capacity for the complete immutable page plan; and
3. verifies that retained complete-current analysis and the privately held page
   store still declare one lineage.

It then traverses every prepared entry in strict `PageNumber` order. Every entry
is freshly observed, including no-write entries:

- `NoRequiredImage` must still be absent;
- `CheckpointCurrent` must still equal its exact selected snapshot;
- `AlreadyCurrent` must still equal its exact prepared target snapshot; and
- `Candidate` must equal either the exact prepared source or exact target.

Any other page, lineage, position, version, or bytes is a typed contradiction.
No later page is observed after the first failure.

## Candidate and Comparison

`DurableTransactionRestartCheckpointPageRepairCandidate` borrows one exact
source precondition and one owned target from the private prepared table. Its
fields and construction are private. Adapter implementations may inspect it
only while the domain invokes their port; no public transition returns a
candidate or target to an ordinary caller.

A source precondition is either prepared store absence or one exact
`StoredPageSnapshotObservation`. A target owns:

- raw or committed provenance kind;
- page number and version;
- full `[u8; N]` image;
- physical page WAL position; and
- for a committed target, exact transaction identity and later commit position.

`compare_transaction_restart_checkpoint_page_repair_candidate` is read-only and
returns only `SourceMatches` or `TargetAlreadyPresent`. Target equality requires
page number, lineage, physical position, version, and every byte. Source
equality uses the same exact snapshot identity. Same-position payload
contradictions are distinct from changed source positions.

Page versions remain payload identity, never recency. ADR 0055's physical replay
order remains authoritative.

## One-Attempt Permit and Store Port

`TransactionRestartCheckpointPageRepairWritePermit<'attempt>` owns the exact
target page position and optional committed-target position. Its fields are
private, it is not cloneable, and its invariant generative lifetime cannot be
widened. The domain constructs it only after a fresh comparison returns
`SourceMatches`.

`TransactionRestartCheckpointPageRepairStore<N>` is separate from ordinary
`PageStore` writes and ADR 0029 committed-page recovery. Its
`compare_and_replace_replay_page` method must, under one continuous exclusive
store hold:

1. validate target and permit lineages;
2. validate the permit's page position and raw/committed commit shape against
   the candidate;
3. re-observe authoritative current state;
4. require exact `SourceMatches`, not `TargetAlreadyPresent`;
5. reserve any adapter bookkeeping needed for a missing page; and
6. durably replace the source with the exact target.

The executor does not invoke the port when its initial observation already sees
the exact target. This is the idempotence path for acknowledged and
outcome-indeterminate earlier attempts.

## Outcome-Indeterminate Boundary

Every error returned after `compare_and_replace_replay_page` is invoked becomes
`StoreWrite`, regardless of adapter-specific knowledge about a deterministic
before-write fault. The generic domain cannot use an adapter error to prove that
no effect occurred.

Errors before that invocation remain definite for the current page:

- outcome-capacity exhaustion;
- retained analysis/store lineage mismatch;
- unresolved internal decision or missing private target;
- current store observation failure; or
- exact source/target comparison contradiction.

The failed owner separately records whether this or any consumed earlier
attempt has an unresolved indeterminate write result. Choosing retry consumes
the earlier exact cause, but a later pre-write failure does not incorrectly
erase the fact that an older store invocation was uncertain.

## Completed Prefix and Retry

Each completed page contributes one inert outcome:

- `NoRequiredImage`;
- `CheckpointCurrent`;
- `AlreadyCurrent`;
- `TargetAlreadyPresent`; or
- `Repaired`.

`Repaired` means only that the adapter returned success. It is not a new write
permit or transaction-restoration proof by itself.

On failure,
`FailedTransactionPageStorageRestartCheckpointPageRepair` privately retains:

- the complete original ADR 0055 owner and all adapters;
- every original source and target decision;
- the strict completed prefix from this attempt;
- the exact stopping cause; and
- cumulative indeterminate-result state.

The only continuation is consuming `retry()`. It clears the completed-prefix
length while reusing its preallocated capacity, starts at page one, re-observes
every entry, and follows the same comparisons. It cannot continue at the failed
page or skip an earlier no-write entry. An exact target left by an earlier
attempt becomes `TargetAlreadyPresent`; an exact source is attempted again.

No compensation or rollback is performed for an earlier durable page. A
complete retry is successful only after every page has again resolved in plan
order.

## Successful Ownership and Authority

Complete execution returns
`RepairedTransactionPageStorageRestartCheckpointReplay`. It privately retains
the prepared owner and exposes only persistent identity, selected/current
frontiers, and the complete inert outcome slice.

Neither successful nor failed state can expose or create:

- WAL, page-store, or checkpoint-source adapters;
- replay records, analyses, baselines, source snapshots, or target images;
- ordinary page-write, committed-recovery, or replay-repair permits;
- a single-page continuation;
- complete-recovery fallback after the effectful boundary;
- transaction lifecycle tokens or a live transaction owner;
- checkpoint publication authority; or
- WAL retention, truncation, reclamation, or rewrite authority.

Compile-fail tests cover permit construction and cloning, port invocation
without a permit, prepared evidence extraction, single-page retry, and fallback
from the repaired owner.

## Memory Adapter

`InMemoryPageStore` validates permit and candidate shape, reprojects the current
snapshot, requires exact source comparison, and reserves a missing page slot
before its deterministic write boundary. It then replaces or inserts the exact
target page, version, bytes, and physical WAL position.

The existing `BeforeWrite` and `AfterWrite` fault points are reused. Both errors
are indeterminate to the domain. Whole-plan retry repairs after `BeforeWrite`
and recognizes the exact target after `AfterWrite`.

## Filesystem Adapter

`FilePageStore` performs the same validation and locked source recheck. It uses
the existing page-store v1 `write_snapshot_group` path, sequence allocation,
capacity reservation, frame construction, `sync_all`, in-memory publication,
and before/after fault points. It does not duplicate or alter persistent bytes.

`BeforeWrite` leaves the file unchanged. `AfterWrite` fires only after the
complete group is synchronized and published in memory. Whole-plan retry
therefore writes after the former and resolves `TargetAlreadyPresent` without
another group after the latter.

An uncertain real I/O error retains existing writer poisoning. A retry on that
same object fails observation until the owner is dropped and the full
three-object startup composition is reopened. Reopen uses existing incomplete
tail handling; this decision adds no hidden unpoison or adapter extraction path.

## Filesystem Lock Continuity

ADR 0052's acquisition order remains WAL, page store, then completeness
control. Moving the same three values through prepared, failed, retry, and
repaired states does not close, clone, reopen, or unlock a descriptor.
Independent open attempts fail throughout failure inspection and whole-plan
retry.

The locks remain cooperative advisory locks. This decision adds no
database-wide lock, wait protocol, hostile path defense, atomic multi-file
acquisition, or unsupported-filesystem guarantee.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, page, WAL, recovery,
checkpoint, lock, and deterministic-fault contracts. No external product
documentation, driver, SDK, fixture, oracle, proprietary governance tool, or
native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server redo, undo, transaction-table restoration,
checkpoint algorithm, LSN value, diagnostic, error number, persistent format,
or compatibility claim.

## Test Boundaries

- Empty plans complete without observation or write.
- Raw and committed targets are both written from missing and behind sources.
- Every no-write resolution is freshly revalidated.
- Strict page order and completed-prefix evidence are preserved.
- Changed absence, position, lineage, page, version, or bytes fail before port
  invocation.
- Observation failure after an earlier successful page retains that exact
  prefix and prevents later access.
- Before- and after-write failures are both indeterminate.
- Retry always starts at page one and converts exact earlier targets to
  `TargetAlreadyPresent`.
- Memory tests cover raw/committed execution, partial progress, deterministic
  faults, and retry.
- Filesystem tests prove synchronized durable bytes, no duplicate group after
  an after-write retry, exact reopen state, and continuous ownership of all
  three locks.
- Existing persistent-format, tail repair, poison, recovery, checkpoint,
  architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- make the complete page set atomic or roll back a completed prefix;
- redo, undo, abort, compensate, or restore transactions;
- create the final recovered or live storage owner;
- invalidate, delete, or republish the selected checkpoint;
- enumerate or reconcile store-only pages;
- choose a WAL retention floor or truncate, compact, reclaim, or rewrite WAL;
- add streaming, spilling, parallel repair, bounded-memory guarantees, or a
  production page index;
- change persistent bytes, lock order, synchronization, or recovery formats; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql can now atomically compare and replace every exact ADR 0055 replay target
through a dedicated capability while preserving deterministic order,
fail-closed contradictions, explicit indeterminacy, lock continuity, and
idempotent whole-plan retry.

The successful owner remains non-live. Transaction-table restoration, final
live-owner construction, WAL retention, and WAL reclamation remain separately
reviewed follow-up boundaries.
