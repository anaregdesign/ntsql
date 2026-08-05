# ADR 0026: Committed Transaction-Page Physical Reconciliation

- Status: Accepted
- Date: 2026-08-06
- Issue: #104
- Extends: ADR 0019, ADR 0023, ADR 0024, ADR 0025
- Extended by: ADR 0027, ADR 0028

## Context

ADR 0019 compares every physical full-image page WAL record with the current
stored snapshot. Its result is intentionally commit-agnostic: the latest
physical record may be raw or transaction-owned but uncommitted. ADR 0025
selects the latest committed transaction-owned record but does not inspect raw
physical records or the page store.

Neither result alone can safely describe whether the store reflects the latest
committed transaction image. Treating ADR 0019 `ExactCurrent` as committed could
accept a store backed by raw or uncommitted data. Treating its `StoreBehind`
latest physical position as a replay target could expose the same data.
Conversely, a later raw or uncommitted WAL record should not make a store that
matches the latest committed record appear behind committed state.

The smallest safe next step is an I/O-free transaction-domain reconciliation
that combines both evidence views, classifies the snapshot's physical backing,
and reports state relative to the latest committed transaction-owned record. It
remains observational and cannot authorize mutation.

## Crate and Dependency Boundary

`ntsql-transaction` owns the operation because it combines commit
classification and selection with page-domain physical observations. Its
reviewed direct dependency set remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal
```

The implementation reuses public ADR 0019 values and reconciliation through the
existing dependency. No adapter type enters a domain crate, no crate or
dependency edge changes, and the architecture allow-list and reverse-edge tests
remain unchanged.

## Complete Dual-Projection Input Contract

`reconcile_committed_transaction_page` receives:

1. one expected `LogLineage`;
2. one expected `PageNumber`;
3. zero or one `StoredPageSnapshotObservation<N>`;
4. a borrowed slice containing the ADR 0019 physical projection of every durable
   full-image page WAL record for that page in strictly increasing order;
5. a borrowed slice containing every ADR 0024 owner-aware transaction-page
   projection for that page in strictly increasing order; and
6. the complete borrowed durable commit slice.

The physical slice includes raw page records and the physical projection of
every transaction-owned record. It is not a raw-only filter. An owned record
therefore appears once through each projection surface:

- its owner-free physical projection participates in page/store integrity; and
- its owner-aware projection participates in transaction classification.

This deliberate cross-projection use is not the grouping-time double counting
prohibited by ADR 0024. The function does not treat the two values as two
physical records or advance the WAL frontier twice. It compares their shared
position and payload as two views of one record.

Both slices and the commit slice must come from the same complete durable
prefix. The domain values cannot prove this. In particular, an unmatched
physical position is raw only under the complete owner-aware projection
contract. A genuinely raw record and an omitted owner projection are
indistinguishable from these values. This limitation is explicit and no
success result proves caller completeness.

## Deterministic Validation Priority

Validation proceeds in four stages:

1. ADR 0025 selection validates the complete commit prefix and every owner-aware
   page observation.
2. ADR 0019 reconciliation validates the snapshot and every physical page
   observation.
3. A bounded cross-projection merge validates that every owner-aware record has
   an exact physical counterpart.
4. The snapshot's backing kind is classified and resolved against the selected
   latest committed record.

An earlier stage's failure takes priority over every later-stage defect.
Selection failures are retained under a boxed typed source. ADR 0019 snapshot,
page, lineage, order, backing, and payload failures are retained under a
separate boxed typed source.

## Reused Domain Validation

Stage one calls `select_latest_committed_transaction_page` over the exact owned
and commit slices. It preserves ADRs 0023 and 0025 in full:

- the complete commit prefix validates even with no owned pages;
- every owned record has the requested page and lineage and advances strictly;
- each record is classified through ADR 0023;
- uncommitted records remain validated but are excluded from selection; and
- the selected record is greatest by owned-page WAL position, not
  `PageVersion`.

Stage two calls `reconcile_durable_page` over the snapshot and complete physical
slice. This establishes physical page/lineage/order validity and proves that any
snapshot is backed by an exact physical position, version, and image.

The ADR 0019 classification itself is discarded intentionally. Its
`ExactCurrent`, `StoreBehind`, and latest position are relative to all physical
records, including raw and uncommitted records. This ADR computes distinct
committed-relative state only after identifying the snapshot backing kind.

## Cross-Projection Integrity

After both slices independently validate, one monotonic merge checks every
owner-aware observation against the physical slice.

- No physical record at the owned position is
  `OwnedPagePositionUnbacked`.
- A shared position with differing page version or bytes is
  `OwnedPagePayloadContradiction`.
- Exact position, version, and bytes establish the required relationship.
- Physical positions with no owner-aware match remain candidates for raw
  records under the completeness contract.

This integrity gate does not choose a success variant. It detects mismatched
prefixes, files, or projection inputs before snapshot ownership is inferred.
It runs after physical validation, so numeric merge comparisons occur only
within the expected lineage and strict order.

## Snapshot-Backing Classification

When a snapshot exists, ADR 0019 has already proven an exact physical backing at
its required position. A binary search of the validated owner-aware slice then
classifies the backing kind.

No owner-aware match means the snapshot is backed by a raw physical record under
the complete projection contract. Reconciliation fails with
`SnapshotBackedByRawPage`.

An owner-aware match is classified again through ADR 0023:

- `Uncommitted` fails as
  `SnapshotBackedByUncommittedTransactionPage`, retaining the owner and page
  position.
- `Committed` supplies the exact stored page and commit positions used by the
  committed-relative decision.
- A classification error is retained as a boxed typed source.

The repeated classification result is load-bearing because it distinguishes an
earlier committed store backing from an earlier uncommitted backing. A
classification failure is defensive after stage-one validation but remains
explicit rather than using `unwrap`, `expect`, panic, or a success-shaped
fallback.

## Committed-Relative Decision Matrix

The final result is determined by snapshot-backing kind and ADR 0025 selection:

| Snapshot backing | Selection | Result |
| --- | --- | --- |
| absent | no committed page | `NoCommittedPage` |
| absent | latest committed page | `StoreMissing` |
| raw physical page | either | `SnapshotBackedByRawPage` error |
| uncommitted owned page | either | `SnapshotBackedByUncommittedTransactionPage` error |
| committed owned page | latest at equal page position | `ExactCurrent` |
| committed owned page | latest at greater page position | `StoreBehind` |
| committed owned page | no committed selection | defensive contradiction |
| committed owned page | backing after selected position | defensive contradiction |

The two defensive contradiction cells are unreachable under complete immutable
inputs because stage-one selection classifies the same owned slice. They are
typed errors so future refactoring cannot turn an impossible state into a
panic or fabricated reconciliation.

`StoreBehind` retains the stored owned-page position, its matching commit
position, and the exact later selected observation plus its commit position.
`ExactCurrent` and `StoreMissing` retain the exact selected observation and
commit position through `LatestCommittedTransactionPage<N>`.

## Later Raw and Uncommitted Records

Committed-relative state is intentionally different from physical-latest state.
If the snapshot matches the selected latest committed record, later raw or
uncommitted physical records do not make the committed store state behind.
Every such later record still participates in physical validation, and every
later owned record still participates in ADR 0023 classification.

If the store snapshot instead points to a later raw or uncommitted record, its
backing kind fails closed. The function never proposes the earlier committed
image as a replay command and never overwrites the suspect store.

## Lifetimes, Allocation, and Complexity

The reconciliation output lifetime depends only on the selected owner-aware
observation. Snapshot, physical-page, and commit inputs may be dropped after the
call because required positions are cloned into the result.

Success paths build no collection and retain constant state. Box allocation
occurs only when preserving a nested typed failure while keeping the public
`Result` error bounded in size.

For `O` owned observations, `C` commits, and `P` physical observations:

- ADR 0025 selection costs `O(C + O*C)`;
- ADR 0019 physical validation costs `O(P)`;
- cross-projection integrity costs `O(P + O)`;
- snapshot owner lookup costs `O(log O)`; and
- snapshot-backing classification costs one additional `O(C)` scan.

Additional success state is `O(1)`. No owner index or input-sized collection is
allocated.

## Authority Boundary

The reconciliation values are data, not lifecycle, visibility, or recovery
authority. They contain no mutation port, callback, replay command, or store
capability and cannot create or convert into:

- `TransactionId`, `CommittedTransaction`, or another lifecycle token;
- `DirtyPage`, `TransactionDirtyPage`, or `PageWritePermit`; or
- any page-store write operation.

Compile-fail tests preserve these boundaries. Private fields on the selected
wrapper prevent construction of selected evidence without ADR 0025, but even a
publicly constructible reconciliation value would remain non-authorizing.

In particular, `StoreMissing` and `StoreBehind` describe committed-relative
physical evidence only. They do not permit redo, establish idempotence, or say
that replacing the current store is safe.

## Evidence Boundary

The operation consumes only repository-authored domain observations and
performs no I/O. It does not consult an external product, driver, SDK, fixture,
oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format. It
defines no external SQL Server transaction, visibility, recovery, LSN, page, or
diagnostic behavior.

## Test Boundaries

- Empty evidence and raw/uncommitted physical evidence without a snapshot report
  no committed page.
- One committed owned record without a snapshot reports a missing store, and the
  output remains valid after physical and commit inputs are dropped.
- A snapshot at the selected committed position remains exact despite later raw
  and uncommitted physical records.
- An earlier committed snapshot backing reports behind state with exact stored
  page/commit and selected page/commit positions.
- A lower page version at the later committed position still wins.
- Snapshots backed by later raw and later uncommitted records fail distinctly.
- A snapshot with no physical backing retains the ADR 0019 typed source.
- Foreign or malformed commit evidence retains the ADR 0025 typed source.
- Missing and payload-contradictory owner/physical projections fail distinctly.
- Both defensive committed-backing contradictions are explicit typed errors.
- Pointer identity proves results borrow the exact selected observation.
- Compile-fail tests prevent transaction, dirty-page, and write-permit
  conversions.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- change memory or filesystem adapters or add an orchestration API;
- allocate or expose a whole-prefix grouping or owner index;
- prove that dual projections or commit evidence are complete;
- change raw-page or stored uncommitted-page policy beyond fail-closed
  classification;
- create a recovery candidate, replay command, or mutation capability;
- define idempotence, redo, undo, rollback, abort, compensation, checkpoints,
  transaction tables, or dirty-page tables;
- define page reads, isolation, locking, buffering, eviction, or
  force-at-commit; or
- define external SQL Server values or native file formats.

## Consequences

The transaction domain can now distinguish missing, exact, and behind stored
state relative to the latest committed transaction-owned page while rejecting
raw or uncommitted snapshot backing and preserving all non-authorizing
boundaries.

The next slice may exercise this function from memory restart/persistent reopen
and filesystem v3 reopen using both projections from the same
`durable_records()` prefix and real stored snapshots. Recovery mutation remains
blocked on idempotence and a separately reviewed recovery-only write gate.
