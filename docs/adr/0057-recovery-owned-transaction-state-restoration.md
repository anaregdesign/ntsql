# ADR 0057: Recovery-Owned Transaction State Restoration

- Status: Accepted
- Date: 2026-08-06
- Issue: #167
- Extends: ADR 0007, ADR 0009, ADR 0034, ADR 0056
- Follows: #166

## Context

ADR 0056 leaves a successfully repaired selected-checkpoint restart in a
deliberately non-live owner. That owner still retains the complete-current
`DurableTransactionRestartAnalysis`, but the analysis is inert metadata. It
does not allocate a fresh coordinator epoch, preserve an explicit unresolved
transaction policy, or prove that a new coordinator cannot reuse an epoch
represented by the selected checkpoint or complete durable stream.

Reconstructing persisted observations as `ActiveTransaction` values would
violate ADR 0007's private runtime brand. Opening an ordinary coordinator
without checking retained durable high-water evidence would also trust an
allocator result that could collide with recovery metadata. The transition
therefore needs a restart-only allocation port, a repository-authored policy
for every current restart state, and owning success and failure states that
release no live authority.

This decision restores only recovery-owned transaction state and one private
fresh coordinator. Final adapter and coordinator release remains ADR 0058
follow-up work under issue #168.

## Crate and Dependency Boundary

`ntsql-transaction` owns:

- the restart-only epoch-source port and allocation result vocabulary;
- selected/current transaction-evidence validation;
- the repository-authored committed and unresolved policy;
- inert aggregate restoration evidence;
- the private restored coordinator owner;
- deterministic rejection and indeterminate allocation failure owners; and
- the consuming transition and retry boundary.

`ntsql-storage-memory` and `ntsql-storage-file` implement only the new epoch
source port:

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

No crate, direct dependency edge, architecture registration, logical WAL
record, page-store record, checkpoint codec, file header, frame kind, checksum,
marker, lock primitive, or path changes. The file adapter reuses ADR 0013's
existing synchronized epoch-allocation frame.

## Consuming Restoration Transition

`RepairedTransactionPageStorageRestartCheckpointReplay::
restore_transaction_state(self)` is the only entry point. It consumes the
complete successful page-repair owner. Prepared, planned, selected, failed
repair, and complete-recovery states cannot invoke it.

The transition first validates retained transaction evidence without calling
an adapter. It then invokes the restart-only epoch port exactly once. Only
after a valid result is returned does the transaction domain privately
construct a fresh empty `TransactionCoordinator`.

No outcome exposes:

- the coordinator;
- its private runtime identity or mutation methods;
- an `ActiveTransaction`, `CommittedTransaction`, or request token;
- a persisted transaction entry as lifecycle authority;
- WAL, page-store, or checkpoint-source adapters;
- page images, replay records, repair permits, or repair candidates;
- checkpoint publication; or
- WAL flush, retention, truncation, or reclamation authority.

## Retained Transaction Policy

The authoritative transaction table remains the exact complete-current
`DurableTransactionRestartAnalysis` already held inside the repaired owner. It
is not copied into the coordinator registry and is not lowered into live
tokens.

Every current state is classified exhaustively:

- `Committed { commit_position }` remains terminal recovery metadata with its
  exact first and last owned-page positions, record count, and commit position.
- `Uncommitted` remains an unresolved recovery obligation with its exact first
  and last owned-page positions and record count. It is not called rolled back,
  aborted, active, retryable, or client-visible.

The state enum is repository-owned and privately constructed from validated
logical WAL evidence. There is no wildcard classification. Adding another
terminal state makes this transition non-exhaustive at compile time until a
new ADR-backed policy is added. An adapter or decoded checkpoint cannot inject
an unknown state into the current closed enum. Unsupported codec versions and
invalid observations continue to fail before this owner exists.

`TransactionRestartRestorationSummary` exposes only inert counts, the greatest
persisted identity, the fresh coordinator epoch, and the number of earlier
indeterminate allocation attempts. Exact entry ranges and commit positions
remain private for later reviewed undo, abort, or diagnostic work. They do not
enter `ClientDiagnostic`.

## Selected and Complete-Current Consistency

The selected transaction baseline and complete-current analysis are immutable
private values, but the restoration boundary defensively validates their
prefix relationship before any epoch allocation:

1. both must carry the same nonzero persistent lineage identity;
2. a nonempty selected frontier requires a complete-current frontier and may
   not be after it;
3. every selected transaction must exist in the complete-current table;
4. its first owned-page position must remain exact;
5. a selected committed entry is terminal, so its last position, count, and
   commit position must remain exact;
6. a selected unresolved entry may gain later pages or a commit only after the
   selected frontier, but may not lose pages, regress its last position, or be
   committed inside the prefix that classified it unresolved;
7. every selected page or commit position must be at or before the selected
   frontier; and
8. every complete-current transaction whose first page or commit lies inside
   the selected frontier must also occur in the selected table.

The complete-current analysis already guarantees strict logical WAL order,
valid entry shapes, unique identities, exact ranges, and valid commit
placement under ADR 0034. This transition does not reread WAL or invent
missing per-record evidence. It checks only relationships provable from the
two retained owned projections.

Any contradiction returns a deterministic rejected owner before the epoch
port is invoked.

## Restart-Only Epoch Allocation

`TransactionRestartCoordinatorEpochSource` receives the greatest retained
persisted epoch, or `None` for an empty union of selected and complete-current
transaction tables. A conforming implementation must atomically return:

- one nonzero epoch strictly greater than that high-water mark; and
- the exact persistence lineage in which the epoch is unique.

`TransactionRestartCoordinatorEpochAllocationError` separates:

- `Source`, whose physical allocation result may be unknown;
- `IdentitySpaceExhausted`, when no greater epoch exists; and
- `PersistedEpochHighWaterNotAdvanced`, when the adapter's next epoch is stale.

The last two are contractual pre-effect deterministic rejections. The domain
still validates every successful result. An epoch at or below retained
high-water, or a lineage different from complete-current analysis, becomes a
deterministic rejection. The returned epoch is never released as a
coordinator in either case.

`TransactionCoordinator::from_allocated_epoch` remains private. Both ordinary
`TransactionCoordinator::open` and this restoration transition use it, so
coordinator initialization has one implementation while only the restart path
performs retained high-water validation.

The coordinator starts with sequence one and an empty in-process lifecycle
registry. Its epoch is strictly greater than every selected or
complete-current persisted epoch, so no identity it can issue equals a
retained `(epoch, sequence)` pair. Persisted entries are not inserted into its
registry because registry membership and private runtime branding are live
authority, not recovery metadata.

## Failure, Retry, and Reopen

`RejectedTransactionPageStorageRestartCheckpointRestoration` owns the repaired
state, all three adapters, complete page outcomes, exact deterministic cause,
and any earlier allocation uncertainty. It has no retry or fallback method.
Evidence contradictions, allocator exhaustion, stale high-water, defensive
result rejection, and attempt-count exhaustion cannot change within the same
retained startup evidence. Resolution requires dropping the owner and
reopening the complete startup composition after the underlying input is
changed or repaired.

`FailedTransactionPageStorageRestartCheckpointRestoration` is reserved for
`Source` failures. The generic domain cannot prove whether a returned source
error happened before or after durable epoch allocation. It therefore:

- retains the entire repaired owner and exact source cause;
- increments a cumulative indeterminate-attempt count;
- releases no coordinator or adapter; and
- permits only a consuming retry of the complete allocation boundary.

A retry uses the same immutable persisted high-water evidence. If an earlier
epoch was consumed, a conforming allocator returns a later one. If it was not,
the same still-fresh epoch may be returned. Either result is accepted only
after the same strict high-water and lineage checks. A later success retains
the cumulative uncertainty count in its inert summary.

The filesystem writer remains poisoned after uncertain real I/O. Retrying that
same owner therefore returns the existing poisoned-writer source error.
Dropping and reopening reconstructs the next epoch from synchronized epoch
frames, including a frame that may have become durable before an error.

## Successful Non-Live Ownership

`RestoredTransactionPageStorageRestartCheckpointReplay` privately owns:

- the complete ADR 0056 repaired owner and ordered page outcomes;
- the selected baseline and complete-current transaction analysis nested
  inside it;
- the exact WAL, page store, and checkpoint source with their existing locks;
- one fresh `TransactionCoordinator`; and
- one inert `TransactionRestartRestorationSummary`.

Public methods expose only persistent identity, selected/current frontiers,
page outcomes, and the inert summary. The coordinator field is never returned
or borrowed. Final startup validation under issue #168 must consume this exact
owner before ordinary live interfaces can exist.

## Memory Adapter

`InMemoryCommitLog` checks its retained `next_epoch` before mutation. `None`
returns deterministic exhaustion. A value at or below the supplied high-water
returns deterministic stale-high-water evidence without advancing the
allocator. A valid value advances the in-memory high-water without wrap and is
paired with the current lineage.

The existing `restart` and `reopen` model retains the epoch allocator
high-water. Focused tests allocate above retained evidence, restart and reopen,
allocate again without reuse, and prove a stale request neither advances the
allocator nor creates authority.

## Filesystem Adapter and Lock Continuity

`FileCommitLog` checks poison, exhaustion, and retained high-water around the
same private epoch-frame allocation helper used by ordinary coordinator open.
A valid allocation writes the existing epoch frame, synchronizes the file,
advances `next_epoch`, and returns the held lineage. No new persistent bytes or
format interpretation is introduced.

The restored, rejected, and failed owners retain ADR 0052's existing WAL,
page-store, and completeness-control locks. No transition closes, clones,
reopens, or reacquires an adapter.

Filesystem integration performs a selected replay repair and restoration while
all three independent opens remain blocked. Dropping the successful non-live
owner releases the composition. A fresh three-object reopen selects the same
checkpoint, recognizes the already-current page target, and allocates a later
coordinator epoch. This proves durable epoch non-reuse across repeated
restoration attempts without final live-owner release.

The locks remain cooperative advisory locks. This decision adds no
database-wide lock, wait protocol, hostile path defense, atomic multi-file
acquisition, or unsupported-filesystem guarantee.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, WAL, page, checkpoint,
repair, ownership, fault, and filesystem contracts. No external product
documentation, driver, SDK, fixture, oracle, proprietary governance tool, or
native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server transaction table, recovery phase,
rollback, abort, undo, transaction ID, LSN, diagnostic, error number, startup
order, persistent format, or compatibility claim.

## Test Boundaries

- Empty, commit-only, unresolved-only, and mixed transaction tables restore
  exact policy counts and a fresh greater coordinator epoch.
- Mixed tests preserve exact first and last owned-page positions, record
  counts, and commit positions inside the recovery-owned analysis.
- Selected/current missing-entry, regressed-field, prefix, frontier, and
  lineage contradictions are typed before allocation.
- Stale allocator high-water and `u64::MAX` exhaustion are deterministic
  rejections with no coordinator release.
- Before- and after-allocation source failures retain exact causes, cumulative
  uncertainty, and retry without identity reuse.
- The domain defensively rejects a successful stale epoch or foreign lineage.
- Memory restart and reopen preserve allocator high-water.
- Memory selected-checkpoint integration covers committed and unresolved
  entries plus maximum persisted epoch exhaustion.
- Filesystem allocation survives reopen, and deterministic rejection writes no
  bytes.
- Filesystem selected-checkpoint integration preserves all three locks and
  allocates distinct epochs across drop/reopen restoration attempts.
- Compile-fail tests prevent restored-state construction, persisted-entry
  conversion to active state, adapter extraction, coordinator activation,
  deterministic-rejection retry, checkpoint publication, and WAL authority.
- Existing epoch, recovery, checkpoint, repair, persistent-format, poison,
  lock, architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- release live adapters or coordinator mutation authority;
- execute undo, rollback, abort, compensation, or lock-table reconstruction;
- create client diagnostics or session-visible transaction state;
- re-observe or mutate replay pages;
- invalidate, delete, or republish the selected checkpoint;
- choose a WAL retention floor or truncate, compact, reclaim, or rewrite WAL;
- change WAL, page-store, or checkpoint persistent formats;
- define database lifecycle, online recovery, isolation, concurrency, or
  force-at-commit policy; or
- define external SQL Server behavior or native file compatibility.

## Consequences

A successfully repaired selected-checkpoint restart can now preserve exact
committed and unresolved transaction obligations while privately opening a
coordinator whose epoch is above every retained persisted identity. Source
uncertainty and deterministic contradictions remain distinct owning states,
and no live token or adapter authority escapes.

The resulting owner is still non-live. Final coherence validation and ordinary
live-owner construction remain the next separately reviewed boundary under
issue #168.
