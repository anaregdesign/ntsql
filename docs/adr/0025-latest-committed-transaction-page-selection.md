# ADR 0025: Latest Committed Transaction-Page Selection

- Status: Accepted
- Date: 2026-08-06
- Issue: #102
- Extends: ADR 0023, ADR 0024

## Context

ADR 0023 classifies one durable transaction-owned full-image page record against
complete durable commit evidence. ADR 0024 lets the memory and filesystem
adapters project their validated durable record kinds into those observations.
Neither decision identifies the final committed full image when one page has
several owned records.

Selecting the last physical owned record without checking its transaction could
expose an uncommitted image. Selecting by `PageVersion` would also invent a
recency rule that the repository has not established: page versions are
adapter-owned equality evidence, while WAL positions define physical order.

The smallest safe next step is an I/O-free transaction-domain operation that
validates complete durable evidence and borrows the greatest committed owned
record for one page. It remains observational and does not reconcile a page
store or authorize recovery.

## Crate and Dependency Boundary

`ntsql-transaction` owns the selection because it combines transaction identity,
commit ordering, and transaction-owned page semantics. Its reviewed direct
dependency set remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal
```

No adapter enters the domain, no crate or dependency edge changes, and the
architecture allow-list and reverse-edge tests remain unchanged.

## Complete Evidence Inputs

`select_latest_committed_transaction_page` receives:

1. one expected `LogLineage`;
2. one expected `PageNumber`;
3. every durable `DurableTransactionPageObservation<N>` for that page in
   strictly increasing physical WAL order; and
4. a borrowed slice containing every
   `DurableTransactionCommitObservation` in the complete matching durable
   prefix.

The owned-page iterator may be empty. The commit slice may contain unrelated
identities and numeric gaps because all record kinds share one logical frontier.
Callers remain responsible for selecting an authoritative durable prefix and
supplying every owned record for the requested page. The value types cannot
prove either completeness property.

The complete commit slice is validated before the owned-page iterator is read.
Malformed commit evidence therefore fails even when there are no owned pages and
takes deterministic priority over any later page-input defect.

## Reusable Commit Scan

ADR 0023's commit iteration is factored into one private bounded-state scan. The
scan always validates:

1. expected lineage before numeric comparison;
2. equal adjacent positions as identical duplicates or contradictory
   identities; and
3. decreasing positions as non-advancing evidence.

When supplied a page owner, the scan also retains at most one matching position.
A second matching identity fails before equal-position or decreasing-position
checks, preserving ADR 0023's existing error priority exactly.

Selection first invokes the scan without an owner so the complete slice can be
validated independently of a page. Consequently, equal or decreasing same-owner
commit records fail as commit-position shape errors during this pre-scan. The
standalone per-record classifier continues to report its earlier
`DuplicateMatchingCommit` priority for the same corrupt shape. Distinct-position
duplicate commits pass the owner-free shape scan but fail as
`DuplicateMatchingCommit` when any page owned by that identity is classified.
All forms fail closed without selecting an image.

## Owned-Page Validation

Every owned observation is validated, including records that will classify
uncommitted and records after the current selection.

For each observation:

1. its page number must equal the requested page;
2. its position must belong to the expected lineage;
3. its position must advance strictly beyond the prior owned observation; and
4. ADR 0023 per-record classification must succeed against the complete commit
   slice.

Page number and lineage fail before numeric position comparison. At an equal
owned-page position, owner, page version, and full-image bytes are compared:

- equality of all three is an identical duplicate; and
- any difference is contradictory evidence.

Both are typed errors. A lower later position is a distinct non-advancing error.
Equal positions are impossible in a valid shared WAL frontier; the distinction
is defensive and preserves the exact supplied evidence shape.

Page version and bytes participate only in this equal-position distinction.
They are never compared for recency.

## Per-Record Classification and Failure Closure

Each validly ordered owned record is passed through
`classify_durable_transaction_page`; selection does not duplicate or weaken ADR
0023 semantics.

- A record with one sole strictly later matching commit is committed.
- A record with no matching commit in the complete prefix is uncommitted.
- Foreign, duplicate, contradictory, decreasing, duplicate-owner, or
  commit-not-after-page evidence is an error.

The commit pre-scan surfaces whole-prefix lineage and position-shape failures as
`CommitPrefix`. Page-specific ADR 0023 failures are retained under
`PageClassification` with the page number and page position. Nested errors
remain available through `Error::source`.

A later uncommitted record is valid and does not hide an earlier committed
record. By contrast, a same-owner record physically after that owner's sole
commit is not ordinary uncommitted state: ADR 0023 reports
`CommitNotAfterPage`, and the entire selection fails closed.

## Physical-Order Selection

Uncommitted observations are excluded from selected state but do not terminate
validation. Every committed observation replaces the prior candidate. Because
owned positions must advance strictly, the final candidate is exactly the
committed observation with the greatest owned-page WAL position.

This rule applies across different owners and to repeated `(owner, page)`
records. If several records for one owner all precede its sole later commit,
each is committed and the greatest physical page-record position wins. A later
committed record wins even when its `PageVersion` is numerically lower or its
bytes differ.

The result is:

- `NoCommittedPage { page_number }` after all evidence validates and no owned
  record classifies committed; or
- `LatestCommitted`, containing a privately constructed
  `LatestCommittedTransactionPage<N>`.

The selected value borrows the exact input
`DurableTransactionPageObservation<N>` and owns a clone of its matching
lineage-bound commit position. It does not copy or reconstruct the page image or
position.

## Allocation and Complexity Bound

The success and `NoCommittedPage` paths build no collection and retain only the
previous owned observation plus one selected borrowed observation and commit
position. State is constant with respect to both input sizes. A classification
failure boxes its exact typed nested source so the public `Result` error remains
bounded in size; no input-sized collection or owner index is allocated.

The complete commit slice is scanned once before page iteration and then once
per owned record through ADR 0023 classification. For `P` owned pages and `C`
commits, time is `O(C + P*C)` and additional state is `O(1)`. This ADR does not
claim a single overall pass or add an allocating owner index.

## Authority Boundary

Selection is evidence, not recovery authority. Neither result variant nor the
selected wrapper can create or convert into:

- `TransactionId`, `CommittedTransaction`, or another lifecycle token;
- `DirtyPage`, `TransactionDirtyPage`, or `PageWritePermit`;
- a callback, replay command, adapter capability, or store operation.

Private selected-wrapper fields prevent callers from manufacturing a validated
latest result directly. Compile-fail tests preserve the lifecycle, dirty-page,
and write-permit boundaries.

In particular, `LatestCommitted` means only that the complete supplied evidence
contains a greatest owned record with one matching later commit. It does not say
whether a raw or uncommitted physical page record supersedes that image, whether
the page store is current, or whether mutation is safe.

## Evidence Boundary

The operation consumes only repository-authored domain observations and makes no
I/O call. It does not consult an external product, driver, SDK, fixture, oracle,
proprietary governance tool, or native MDF/NDF/LDF/BAK format. It defines no
external SQL Server transaction, visibility, recovery, LSN, page, or diagnostic
behavior.

## Test Boundaries

- Empty and all-uncommitted owned inputs return `NoCommittedPage` only after the
  commit slice validates.
- Foreign, duplicate, contradictory, and decreasing commit evidence fails with
  no owned page present.
- Mixed uncommitted and committed records select the greatest committed WAL
  position across owners.
- A later committed record with a lower page version wins.
- A later uncommitted record does not hide the selected committed record.
- Repeated same-owner records before one later commit select the later owned
  position.
- Wrong-page and foreign-lineage errors precede owned-position comparison.
- Identical, contradictory, and decreasing owned-position shapes have distinct
  failures.
- Duplicate matching commits and commit-not-after-page failures retain their ADR
  0023 source.
- A same-owner post-commit record poisons an otherwise valid earlier selection.
- Pointer identity proves the result borrows the exact chosen observation.
- Compile-fail tests prevent conversion to transaction, dirty-page, or
  write-authorizing state.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- change either storage adapter or allocate a whole-prefix grouping/index;
- select or reconcile raw physical page WAL records;
- inspect or reconcile a stored page snapshot;
- define a recovery candidate, replay command, or mutation authority;
- define idempotence, redo, undo, rollback, abort, compensation, checkpoints,
  transaction tables, or dirty-page tables;
- remove raw page APIs or decide stored raw/uncommitted-page policy;
- define read visibility, isolation, locking, buffering, eviction, or
  force-at-commit;
- create an external compatibility claim or define native SQL Server file
  formats.

## Consequences

The transaction domain can now identify the greatest committed transaction-owned
full-image record for one page from complete durable evidence without creating
lifecycle or replay authority.

The next slice may combine this selected committed observation with all physical
page WAL evidence and the current stored snapshot. It must fail closed when raw
or uncommitted evidence backs the store and remain non-authorizing until
idempotence and a separately reviewed recovery-only write gate exist.
