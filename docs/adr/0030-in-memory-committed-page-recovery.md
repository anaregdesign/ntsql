# ADR 0030: In-Memory Committed-Page Recovery

- Status: Accepted
- Date: 2026-08-06
- Issue: #112
- Extends: ADR 0027, ADR 0029

## Context

ADR 0029 defines adapter-neutral recovery source and store ports, fresh
committed-page planning, a private one-attempt permit, atomic exact-source
replacement, and terminal ambiguity after any invoked store error. It does not
implement those trusted contracts in an adapter.

ADR 0027 already proves that the deterministic memory WAL can project complete
physical, owner-aware, and commit evidence from one `durable_records()` pass
after restart and persistent reopen. Recovery still requires callers to allocate
those projections and reconcile them manually, and `InMemoryPageStore` cannot
yet accept the recovery-only permit.

The smallest concrete implementation is the in-memory adapter. Its exclusive
ownership model can make stable-prefix and atomic store behavior explicit
without introducing filesystem locking or changing a persistent format.

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

No domain crate imports an adapter, no crate or dependency edge changes, and no
filesystem API or format changes.

## Stable Durable-Prefix Projection

`InMemoryCommitLog<N>` implements
`DurableTransactionPageRecoverySource<N>`. Its lineage is the log's exact
runtime capability.

Before scanning, the implementation fallibly reserves `durable_len` elements in
each of three vectors:

1. commit-agnostic physical page observations;
2. owner-aware transaction-page observations; and
3. durable commit observations.

Reserving the full upper bound ensures no vector growth can allocate during the
scan or callback. Capacity failure identifies the affected projection through
`InMemoryPageRecoveryProjection`.

The adapter then performs one loop over exactly one `durable_records()` iterator.
For each record:

- a physical page record matching the requested `PageNumber` contributes its
  physical projection;
- a transaction-owned record matching that page contributes its owner-aware
  projection as well; and
- every commit record contributes to the complete commit projection.

Records for another page do not enter either page projection. Commit evidence is
not page-filtered. Each matching transaction-owned record therefore contributes
two views from one physical record, preserving the ADR 0026/0027
cross-projection contract without double-counting a WAL position.

Physical, owner-aware, and commit conversion failures remain distinct typed
errors and retain their exact boxed causes. The privately constructed in-memory
record model ordinarily makes those failures unreachable, but the adapter does
not turn a future invariant violation into a panic or omitted observation.

After the pass, the adapter invokes the higher-ranked callback once with all
three slices and returns its output directly.

## Stability Boundary

`with_durable_page_evidence` holds `&mut InMemoryCommitLog<N>` from before vector
allocation through callback return. The log exposes no interior-mutable append
or flush path, so safe in-process code cannot advance or replace the durable
prefix during the callback.

The in-memory model has no cross-process writer or operating-system durability
claim. Its continuous mutable borrow is its entire authoritative stability
boundary. A later filesystem implementation must provide a stronger
cooperating-writer lock contract rather than copying this assumption.

## Current Store Observation

`InMemoryPageStore<N>` implements the ADR 0029 observation operation by looking
up the current page-number entry and projecting it through
`StoredPageSnapshotObservation<N>`. Absence remains `None`; an invalid present
snapshot retains the exact projection error.

This first observation occurs in the domain gate before candidate creation. It
does not replace the second authoritative observation inside
`compare_and_replace`.

## Atomic Compare and Replace

`InMemoryPageStore<N>::compare_and_replace` receives only the private recovery
permit and exact ADR 0028 candidate. Under one uninterrupted `&mut self` hold it
performs this validation order:

1. require the candidate target page position to share the store lineage;
2. require the target commit position to share that lineage;
3. require both permit positions to share that lineage;
4. require the permit page position to equal the target page position;
5. require the permit commit position to equal the target commit position;
6. re-project authoritative current store state;
7. compare it with the candidate and require exactly `SourceMatches`; and
8. reserve one page-table slot when the source is absent.

Every stage precedes fault consumption and physical mutation. Projection,
candidate-comparison, non-source success, position, lineage, and capacity
failures remain distinct adapter errors.

`TargetAlreadyPresent` at step seven is not success. It means state changed
after the gate's first observation, so this stale attempt is rejected. A fresh
gate invocation will instead rerun WAL reconciliation and return
`AlreadyCurrent` before calling the store.

After validation, the store consumes the before-write fault, replaces or inserts
the exact target, then consumes the after-write fault. The stored snapshot is:

- the target page number;
- target `PageVersion`;
- exact target bytes; and
- the target page WAL position.

The matching commit position and transaction owner remain in the domain outcome,
not the page-store snapshot. Storing the commit position as the page's required
position would contradict the candidate on the next reconciliation and is
therefore prohibited.

No check/mutation gap exists inside this single-threaded model. The mutable
method is the adapter's atomic exact-source replacement boundary.

## Fault Reuse and Terminal Ambiguity

Recovery and live page writes reuse
`PageStoreFaultPoint::{BeforeWrite, AfterWrite}` because both identify the next
matching physical page-store mutation boundary:

- `BeforeWrite` fires after all recovery validation and reservation but before
  page-table replacement; and
- `AfterWrite` fires after the current snapshot becomes the exact target.

No-write planning, exact-current, or rejected-source paths do not consume an
armed fault.

Both faults return `InMemoryCommittedPageRecoveryStoreError::InjectedFault`.
Because `compare_and_replace` was invoked, the ADR 0029 gate converts either into
terminal `IndeterminateCommittedTransactionPageRecovery`; the adapter does not
claim that its deterministic test boundary makes a generic store error
retryable.

A fresh invocation resolves current evidence:

- unchanged state after `BeforeWrite` can authorize a new replacement; and
- exact target state after `AfterWrite` returns `AlreadyCurrent` without
  consuming another armed fault.

## Volatile-Suffix Restart Scenario

The integration scenario first creates:

1. committed image A at page/commit positions 1/2 with version 10;
2. committed image B at positions 3/4 with version 1;
3. durable uncommitted image C at position 5;
4. durable raw image D at position 6; and
5. C's physically complete but volatile commit at position 7.

Memory restart removes position 7 while retaining the allocator high-water, and
persistent reopen reconstructs positions 1 through 6 under the persistent
lineage.

The test then makes volatile exclusion independently observable after reopen. It
adds another transaction-owned image at position 8, flushes that page record,
and appends its complete commit at volatile position 9 under a `BeforeFlush`
fault. The physical record list contains eight records, while the durable
iterator contains seven and skips position 9.

The recovery source yields:

```text
physical page observations: 1, 3, 5, 6, 8
owner-aware observations:   1, 3, 5, 8
commit observations:        2, 4
```

If the implementation incorrectly scans `records()`, commit position 9 makes
the position-8 image the latest committed target. Scanning `durable_records()`
keeps B at positions 3/4 authoritative, proving that volatile exclusion rather
than restart truncation determines the result.

## Recovery Outcomes

Using the real ADR 0029 gate after reopen:

- an A-backed store recovers B despite B's lower page version;
- an empty store recovers B;
- a B-backed store returns `AlreadyCurrent` without consuming an armed fault;
- a D-backed raw store fails planning without consuming an armed fault;
- a before-effect fault retains exact absent source and B target state, and a
  fresh invocation recovers B; and
- an after-effect fault retains exact A source and B target state, while a fresh
  invocation observes B and performs no write.

Returned targets retain B's exact owner, page number, version, bytes, page
position, and commit position. Later durable uncommitted and raw records and the
complete volatile commit never become the recovery target.

## Authority and Evidence Boundary

The adapter projects evidence and implements a trusted effect port. It does not
construct a recovery candidate or permit. It cannot substitute the live
`PageWritePermit`, and its successful or failed output grants no lifecycle,
dirty-page, callback, or further store authority.

All state and behavior are repository-authored. This ADR consults no external
product, driver, SDK, fixture, oracle, proprietary governance tool, or native
MDF/NDF/LDF/BAK format. It defines no SQL Server recovery, transaction, page,
LSN, locking, error, or diagnostic behavior.

## Test Boundaries

- Direct source invocation proves one page-filtered dual projection and complete
  commits from the durable prefix.
- A post-reopen complete volatile commit would change the selected target if the
  source used the physical record list.
- Projection-error wrappers retain exact physical, owner-aware, and commit
  causes; capacity failure identifies its projection.
- Real-gate missing and behind recovery persist exact B bytes and page position.
- Returned metadata retains B's owner and commit position.
- Exact-current and raw-backed planning paths preserve an armed store fault.
- Before- and after-effect failures retain exact source, target, and cause.
- Fresh reruns prove unchanged-source reauthorization and target-present
  idempotence.
- Existing live WAL, transaction-page, page-store, restart, and fault tests
  remain unchanged.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- implement the recovery ports in `ntsql-storage-file`;
- define cross-process or multi-store lock ordering;
- change WAL or page-store bytes, markers, checksums, synchronization, repair, or
  reopen behavior;
- add a production whole-prefix index or cache;
- change raw or uncommitted page policy;
- define multi-page recovery, checkpoints, redo/undo tables, rollback, abort,
  compensation, allocation, buffering, eviction, isolation, or
  force-at-commit; or
- define external SQL Server values or native file compatibility.

## Consequences

The deterministic memory adapter now exercises the full ADR 0029 recovery path
across restart/reopen, exact-source mutation, injected ambiguity, and fresh
idempotent resolution without weakening the domain authority boundary.

The next slice may implement the same ports in the filesystem WAL v3 and
append-only page store. That design must hold the authoritative durable prefix
stable across the callback and perform source recheck plus durable append under
continuous cooperating-writer exclusion.
