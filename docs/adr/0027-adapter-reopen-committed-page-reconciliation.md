# ADR 0027: Adapter Reopen Committed-Page Reconciliation

- Status: Accepted
- Date: 2026-08-06
- Issue: #106
- Extends: ADR 0024, ADR 0026

## Context

ADR 0026 defines committed-relative reconciliation from complete physical,
owner-aware, commit, and stored-snapshot evidence. Its completeness and
same-prefix obligations are caller contracts. Domain unit tests cannot prove
that the memory and filesystem adapters project both views of each owned record
from the same marker-covered durable prefix after restart or reopen.

The storage adapters already expose every required record-level projection and
explicit `durable_records()` iteration. The smallest next step is integration
coverage that makes one durable-prefix pass, dual projection, volatile-suffix
exclusion, reopened lineage, and real stored snapshots load-bearing. No
production adapter API or format changes.

## Crate and Dependency Boundary

Only existing adapter tests change. The reviewed graph remains:

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

No domain crate imports an adapter, no crate or dependency edge changes, and no
orchestration API enters production code.

## Shared Integration Scenario

Each adapter test uses one persistent lineage and one page number. The shared WAL
frontier contains:

1. transaction-owned image A at position 1;
2. A's durable commit at position 2;
3. transaction-owned image B at position 3;
4. B's durable commit at position 4;
5. transaction-owned image C at position 5 with no durable commit;
6. raw image D at position 6; and
7. C's complete matching commit at volatile position 7.

Image A has page version 10. Later committed image B has page version 1, proving
that WAL position rather than page version defines committed recency. Image C
and raw image D occur after B so committed-relative exactness differs from
physical-latest state.

Four same-lineage page stores retain:

- A as a behind snapshot;
- B as an exact committed snapshot;
- no snapshot; and
- raw image D as a raw-backed snapshot.

A and B reach their stores only through `flush_committed_page`. D reaches its
store through the existing raw page path. No test writes transaction-owned C to
a store or bypasses a permit.

## Volatile Commit Boundary

After D is durable through position 6, the tests arm `BeforeFlush` and attempt
C's commit. Append completes at position 7, but the durability fence fails.
There are seven complete physical records and only six durable records.

Using every physical record and commit incorrectly treats C as committed. The
exact-B snapshot then appears behind C at page position 5 with commit position
7. This deliberately wrong comparison makes inclusion of the volatile suffix
observable.

Authoritative reconciliation instead uses only `durable_records()`. Its commit
set contains A and B at positions 2 and 4 but excludes C's position-7 commit.
C remains uncommitted, and B remains the selected latest committed image.

## One-Pass Dual Projection

After restart or reopen, each authoritative integration executes one loop over
one `durable_records()` iterator. For every record in that loop it asks for:

- the ADR 0019 physical page projection;
- the ADR 0024 owner-aware transaction-page projection; and
- the ADR 0024 durable commit projection.

The resulting positions are:

```text
physical page observations: 1, 3, 5, 6
owner-aware observations:   1, 3, 5
commit observations:        2, 4
```

Owned images A, B, and C each contribute both page views from one physical
record. Raw D contributes only the physical view. Commit records contribute only
commit observations. This is ADR 0026 cross-projection integrity, not two
physical records or two frontier positions.

Every projected position retains the reopened log's exact lineage capability.
No projection reconstructs a position from its numeric component, substitutes
the current lineage, or infers durability from record completeness.

## Memory Restart and Reopen

The memory test starts from `with_persistent_lineage_id`, creates all records and
stores, then calls `restart` followed by `reopen`.

Restart removes the volatile position-7 commit, so reopened `records()` and
`durable_records()` both contain the six marker-covered records. The position
allocator nevertheless preserves its high-water mark: a later append receives
position 8 rather than reusing discarded position 7.

The stores are independent same-lineage in-memory observations retained across
the log restart. Reconciliation is performed only after the restarted log has
reopened and fresh record observations have been projected.

## Filesystem V3 and Page-Store Reopen

The filesystem test creates one WAL v3 file and four page-store files with the
same persistent identity. It drops every file handle and opens the WAL through
`open_transaction_page_capable` and each store through `FilePageStore::open`.

Filesystem reopen preserves all seven complete physical records, including the
unmarked position-7 commit. Its explicit durable iterator still stops at
position 6. A later append receives position 8, preserving the physical
high-water mark.

This distinction from memory restart is intentional:

- memory restart discards the volatile suffix while preserving high-water; and
- filesystem reopen retains complete volatile records for inspection while
  excluding them from durable recovery evidence.

Both adapters therefore yield the same authoritative committed-relative result
from different physical restart models.

## Reopened Reconciliation Outcomes

Fresh dual projections and reopened snapshots establish:

- store A is `StoreBehind`, backed by page position 1 and commit position 2,
  with B selected at page position 3 and commit position 4;
- store B is `ExactCurrent` even though uncommitted C and raw D are later
  physical page records;
- the empty store is `StoreMissing` with B selected; and
- store D fails as `SnapshotBackedByRawPage` at position 6.

Pointer identity confirms each successful result borrows B from the owner-aware
vector built by the authoritative durable-prefix pass. B's lower page version
does not affect selection.

## Allocation and Authority Boundary

The integration harness allocates vectors to group per-page adapter projections.
That allocation belongs to outer test orchestration and does not change ADR
0026's allocation-free success path. No production grouping API or owner index
is added.

The tests consume reconciliation values only for inspection. They create no
`TransactionId`, `CommittedTransaction`, `DirtyPage`, `TransactionDirtyPage`,
`PageWritePermit`, callback, replay command, or recovery store capability.
Existing compile-fail domain tests remain the authority boundary.

## Evidence Boundary

The tests use only repository-authored memory state, WAL v3, page-store formats,
and domain observations. They do not consult an external product, driver, SDK,
fixture, oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format.
They make no external SQL Server transaction, recovery, page, LSN, or diagnostic
claim.

## Test Boundaries

- A/B committed, C uncommitted, D raw, and C's volatile commit occupy exact
  shared positions 1 through 7.
- Projection through all physical records changes the selected committed image,
  proving that `durable_records()` is load-bearing.
- One authoritative loop produces both page views and commit evidence.
- Owned and raw record kinds project through the exact intended surfaces.
- Reopened observations retain exact lineage-bound positions.
- A lower-version B wins over A by WAL position.
- A, B, empty, and D stores reproduce behind, exact, missing, and raw-backed
  outcomes.
- Later C and D do not make B physically latest but do leave it latest committed.
- Memory removes the volatile suffix without reusing its position.
- Filesystem v3 retains the volatile suffix while excluding it from the durable
  iterator and high-water reuse.
- Existing adapter and domain APIs remain unchanged.

## Non-Goals

This ADR does not:

- add or change a memory/file adapter API;
- change WAL v1/v2/v3 or page-store bytes, scanning, repair, markers, or locks;
- add a production whole-prefix grouping API or owner index;
- prove completeness beyond the explicit adapter durable iterator;
- change raw-page or stored uncommitted-page policy;
- define a recovery candidate, replay command, or mutation authority;
- define idempotence, redo, undo, rollback, abort, compensation, or checkpoints;
- define external SQL Server values or native file formats.

## Consequences

Both persistent adapter models now reproduce ADR 0026 committed-relative
reconciliation from one explicit durable-prefix projection pass and real stored
snapshots. Volatile commit evidence demonstrably changes the answer if callers
select the wrong prefix.

The next slice may define a non-authorizing recovery candidate from
`StoreMissing` or `StoreBehind`. It must separately establish exact target
identity and idempotence and still cannot create a write permit; mutation remains
a later reviewed recovery-only gate.
