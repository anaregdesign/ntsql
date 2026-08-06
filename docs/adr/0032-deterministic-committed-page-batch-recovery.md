# ADR 0032: Deterministic Committed-Page Batch Recovery

- Status: Accepted
- Date: 2026-08-06
- Issue: #116
- Extends: ADR 0029, ADR 0030, ADR 0031
- Extended by: ADR 0033, ADR 0047

## Context

ADR 0029 owns one recovery-only committed-page write gate. ADRs 0030 and 0031
implement its stable durable-prefix source and exact-source replacement ports in
memory and filesystem adapters. A caller can therefore reconcile any known page
without acquiring direct mutation authority.

Startup recovery does not yet know which pages to pass to that gate. Guessing a
range, consulting raw page records, or letting each adapter choose its own
traversal would make omissions and first-failure behavior nondeterministic. A
complete run needs one authoritative inventory, one ordering, and one
fail-closed orchestration path without duplicating candidate, permit, or store
logic.

This decision adds that inventory and orchestration. It deliberately permits a
durable prefix of successful page recoveries before a later page fails. It does
not claim multi-page atomicity.

## Crate and Dependency Boundary

`ntsql-transaction` owns the adapter-neutral inventory port, batch outcomes,
errors, and orchestration. The existing adapters implement only the inventory
port:

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

No crate, dependency edge, persistent format, or architecture registration
changes.

## Owned Durable Inventory

`DurableTransactionPageRecoveryInventory<N>` returns an owned `Vec<PageNumber>`
containing every distinct page with at least one transaction-owned record in the
authoritative durable WAL prefix. The result must be strictly increasing by
numeric page number.

The inventory:

- includes a page even when none of its durable owned records has a matching
  durable commit;
- excludes page numbers found only in raw page records;
- excludes every transaction-owned record in the volatile suffix;
- contains no bytes, owner, commit, candidate, permit, callback lifetime, store,
  or mutation authority; and
- returns a typed adapter error rather than a partial list.

Including an uncommitted-only page is intentional. The existing single-page gate
must determine whether it produces `NoCommittedPage` or a typed planning error
from complete evidence. Inventory must not perform or cache that decision.

The batch gate defensively validates strict ordering and uniqueness before any
store operation. A defective implementation that returns a duplicate or
descending page fails with the adjacent offending values. The domain neither
sorts nor silently repairs an untrusted inventory because doing so would hide a
port-contract violation.

## Adapter Inventory Projection

`InMemoryCommitLog<N>` fallibly reserves its complete `durable_len` upper bound,
then performs exactly one loop over exactly one `durable_records()` iterator. It
pushes only transaction-owned page numbers, sorts them, and deduplicates them.
The reservation guarantees that scanning itself does not grow the vector.

`FileCommitLog<N>` uses the same one-pass selection, sorting, and deduplication.
It rejects a poisoned writer and WAL v1 or v2 before allocation or scanning.
Only transaction-page-capable WAL v3 may provide inventory. The unsupported
format error retains the exact opened version.

Neither implementation projects bytes or commits, consumes an injected fault,
mutates a record, advances a durable frontier, consults `records()`, or infers an
owner from a raw page record.

## Prefix Stability and Locks

The source object must keep the inventoried durable prefix unchanged until the
outer batch call returns. The domain holds one continuous `&mut Source` borrow
across inventory and every single-page invocation. Safe external in-process code
therefore cannot mutate the same adapter during the run.

The in-memory adapter has no other mutation path or cross-process claim. The
filesystem adapter additionally retains the WAL file's advisory exclusive lock
for its complete lifetime. Inventory and every subsequent evidence projection
therefore occur while the same file descriptor and cooperating-writer exclusion
remain held.

The batch adds no database-wide lock, lock acquisition, wait protocol, or
multi-file atomic boundary. The WAL and page store are still distinct,
already-opened adapters with the lock topology recorded by ADR 0031.

Completeness and stability remain trusted port contracts for arbitrary
implementations. The generic domain cannot prove that an adapter did not change
state internally between method calls.

## Batch Gate Order

`recover_committed_transaction_pages` performs these stages:

1. Clone the recovery source lineage and reject a different store lineage before
   inventory or store observation.
2. Obtain one complete owned inventory.
3. Validate every adjacent pair as strictly increasing.
4. Fallibly reserve the complete batch outcome capacity.
5. Invoke `recover_committed_transaction_page` once for each page in inventory
   order.
6. Append each exact `NoCommittedPage`, `AlreadyCurrent`, or `Recovered` outcome
   to the completed prefix.
7. Stop immediately when one single-page invocation fails.

The outer gate does not construct a recovery candidate or permit. It does not
observe or write the store directly. Every no-write decision and every mutation
continues to pass through the complete ADR 0029 gate, including fresh evidence
projection, exact-source planning, private permit construction, and adapter
recheck.

The returned successful outcome owns all per-page outcomes in strict inventory
order. `CommittedTransactionPageRecoveryOutcome::page_number` exposes a uniform
inert identifier without changing the authority of any variant.

## Allocation Boundary

Both concrete inventories reserve the durable-record upper bound before their
one-pass scan. The batch then reserves `inventory.len()` outcome slots before
invoking the first page gate. Invalid inventory and outcome-capacity exhaustion
therefore occur before any page-store observation or mutation.

After the first page begins, the outer batch performs no further fallible
bookkeeping allocation: pushing an outcome uses the already reserved capacity,
and failure moves the same vector into the completed-prefix value.

Existing single-page source projection remains independently fallible before
that page's store attempt. Such a later projection-capacity error is preserved
as the exact nested `Source` failure for that page. This decision does not turn
adapter evidence allocation into an infallible operation or a success-shaped
fallback.

## Fail-Fast Partial Progress

The first failed page returns:

- every exact outcome completed before it;
- the failing `PageNumber`; and
- the complete nested ADR 0029 source, observation, planning, comparison, or
  indeterminate write error.

No later page gate is invoked. Earlier `Recovered` outcomes may already describe
durable page-store effects and are not rolled back. The error value grants no
continuation, retry permit, or ability to skip directly to the failed page.

A fresh whole-batch invocation is the only continuation path. It obtains a new
inventory, starts again at the first page, and re-enters the single-page gate for
every item. Earlier recovered pages resolve through fresh authoritative
reconciliation as `AlreadyCurrent`, while a prior no-write result is recomputed.
The formerly failing page receives no special authority from the old error.

## Concrete Multi-Page Scenario

The memory and filesystem scenarios use nonnumeric WAL encounter order and one
sorted inventory:

1. page 83 image A is committed and stored at version 100;
2. page 81 is committed and stored exactly;
3. page 82 is committed but missing from the store;
4. page 83 image B is committed later at version 1 but not flushed to the store;
5. page 84 has a durable transaction-owned record without a durable commit;
6. page 85 has only a durable raw record; and
7. page 86 has physically appended transaction-page and commit records entirely
   beyond the durable frontier.

The inventory is exactly `[81, 82, 83, 84]`. Raw page 85 and volatile page 86 do
not enter it. Ascending traversal yields:

- page 81: `AlreadyCurrent`;
- page 82: `Recovered` from a missing source;
- page 83: `Recovered` from A to the later lower-version B; and
- page 84: `NoCommittedPage`.

A fault armed before the first run remains armed across exact page 81, fails at
missing page 82, retains page 81 as the completed prefix, and leaves pages 83
and 84 untouched. A fresh run resolves page 81 again, recovers pages 82 and 83,
and returns the no-commit outcome for page 84. A subsequent complete run reports
the three committed pages as `AlreadyCurrent` without another replacement.

For the filesystem adapter, page-store sequence numbers remain unchanged across
that idempotent run. Dropping and reopening the store retains exact bytes,
versions, page WAL positions, and sequences, including page 83's later version-1
target.

## Error and Authority Boundary

Batch errors distinguish lineage mismatch, inventory failure, malformed
inventory, outcome-capacity exhaustion, and one exact nested page failure.
`Error::source` exposes adapter inventory errors and nested single-page errors
without flattening their causes. Ordering and capacity errors have no fabricated
source.

Inventory values, completed prefixes, successful outcomes, and outer errors are
inert data. They cannot create transaction lifecycle tokens, dirty pages, live
or recovery permits, stable-prefix callbacks, store references, or another
attempt. The only mutation authority remains the private one-attempt permit
inside `recover_committed_transaction_page`.

## Evidence and Compatibility Boundary

All behavior uses repository-authored WAL, page-store, transaction, recovery,
and deterministic-fault contracts. No external product documentation, driver,
SDK, fixture, oracle, proprietary governance tool, or native
MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server startup, analysis, redo, undo, checkpoint,
page ordering, LSN, transaction, locking, error, diagnostic, or compatibility
behavior.

## Test Boundaries

- Fake ports reject lineage mismatch before inventory and store access.
- Inventory failure, descending order, and duplicates fail before any page
  callback, observation, or write.
- Successful fake traversal records strictly ascending callbacks, observations,
  attempts, and exact outcomes.
- A later fake store failure retains the exact completed prefix and nested
  indeterminate state while proving no later observation.
- A fresh fake batch rerun resolves the prefix idempotently before reaching the
  formerly failing page.
- Memory inventory includes durable uncommitted owned records and excludes raw
  and fully volatile records.
- Filesystem inventory rejects WAL v1/v2 and poisoned v3 state before scanning.
- Real memory and filesystem runs cover exact, missing, behind/lower-version,
  uncommitted-only, raw-only, volatile-suffix, partial-progress, and idempotent
  outcomes.
- Filesystem recovery remains exact after page-store reopen and does not consume
  another store sequence during the idempotent run.
- Existing single-page authority, live flush, format, marker, repair, poison,
  and lock tests remain valid.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- make multi-page recovery atomic or compensate an earlier recovered page;
- add checkpoints, analysis tables, dirty-page tables, transaction tables, redo,
  undo, rollback, abort, log truncation, or restart completion;
- establish database-wide ownership, global lock ordering, online recovery,
  concurrent access, buffering, eviction, allocation, isolation, or
  force-at-commit;
- recover raw-only or store-only pages or change uncommitted-page policy;
- cache recovery evidence or add a production page index;
- change WAL v1/v2/v3 or page-store v1 bytes, markers, checksums,
  synchronization, repair, or open behavior;
- resolve an old page failure without a fresh complete batch; or
- define external SQL Server values or native file compatibility.

## Consequences

Startup code now has one deterministic, fail-fast operation for every
transaction-owned page in an authoritative durable prefix. It retains exact
per-page results, preserves completed durable progress on failure, and reuses the
single-page gate as the sole recovery mutation authority.

Checkpoint analysis, transaction-table reconstruction, raw/store-only policy,
redo/undo, and a database-level startup owner remain separately reviewed
follow-up work.
