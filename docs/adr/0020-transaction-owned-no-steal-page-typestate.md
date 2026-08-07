# ADR 0020: Transaction-Owned No-Steal Page Typestate

- Status: Accepted
- Date: 2026-08-06
- Issue: #92
- Extends: ADR 0001, ADR 0006, ADR 0007, ADR 0015, ADR 0016, ADR 0019
- Extended by: ADR 0021, ADR 0022, ADR 0023, ADR 0068

## Context

ADR 0006 and ADR 0007 established the fail-closed transaction commit lifecycle
and stated that `ntsql-wal` was the sole `ntsql-transaction` dependency. ADR
0015 and ADR 0016 established the write-ahead dirty-page flush and a
deterministic full-image page WAL inside `ntsql-page`, whose only dependency is
`ntsql-wal`. Those page values carry no transaction identity, so nothing yet
prevents an uncommitted image from reaching the page store.

The smallest correctness-first next step is a transaction-owned page-write
lifecycle that adopts a local no-steal rule: a transaction-owned dirty page
cannot reach `PageStore` until the same transaction holds a durable
`CommittedTransaction`. This keeps a future redo-only baseline reachable
because uncommitted transaction-owned images may enter the WAL but cannot enter
the page store through this path. It changes no persistence format and does not
make the raw nontransactional page APIs globally unavailable.

## Superseded Claim

This ADR explicitly supersedes the ADR 0006 and ADR 0007 statements that
`ntsql-wal` is the sole `ntsql-transaction` dependency. The reviewed direct
dependency set of `ntsql-transaction` is now exactly `ntsql-page` and
`ntsql-wal`.

## Crate and Dependency Boundary

`ntsql-transaction` gains one reviewed inward domain edge:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal
```

`ntsql-page` keeps `ntsql-wal` as its only dependency, so the graph stays
acyclic. Architecture enforcement adds `ntsql-page` to the transaction
allow-list, keeps the reviewed graph in sync, and the page negative test now
explicitly rejects the reverse `ntsql-page -> ntsql-transaction` edge along with
adapter, Serde, filesystem, protocol, and contract edges. No adapter, contract,
or serialization type enters either domain crate.

## Token Flow

`TransactionCoordinator::stage_page_write` consumes one `ActiveTransaction` and
one `UnloggedPage<N>` and borrows a `&mut TransactionPageLog<N>`. Before calling
the append port it validates, in order, coordinator ownership, coordinator/log
lineage, page/log lineage, and the retained `Active` phase. Every pre-append
rejection retains the unchanged active token and the exact unlogged page in a
typed reason and never calls the append port.

On success the coordinator returns the same non-cloneable active token plus one
`TransactionDirtyPage<N>` and leaves the registry `Active`, so a caller can
thread the active token through additional pages and then commit. The success
path reuses the existing `ntsql_page::stage_page_write` evidence checks through
a private bridge that presents the `TransactionPageLog` as a `PageLog` and
appends a private `TransactionPageWriteRecord` carrying the owning
`TransactionId`. Safe downstream code cannot construct that record.

`TransactionPageWriteRecord<'page, N>` exposes read-only transaction and page
inspection but has no public constructor. `TransactionDirtyPage<N>` owns the
identity and the domain `DirtyPage`, is neither `Clone` nor `Copy`, exposes no
raw `DirtyPage`, `PageWritePermit`, or store capability, and offers only
read-only owner, address, version, image, and required-position accessors.
The coordinator reserves each successfully staged page number under its
transaction identity. A second image for that page and transaction is rejected
before append while retaining the active token and exact page.

## Append Ambiguity Poison

Once the append port is invoked, an adapter error, a foreign returned position,
or an append-time lineage change is terminal. The coordinator records the
distinct `TransactionLifecycleStatus::PageAppendIndeterminate` phase, returns
terminal page evidence that retains the `TransactionId`, the existing
`IndeterminatePageLogAppend`, and the exact cause or reason, and provides no
path back to `ActiveTransaction`.

`PageAppendIndeterminate` is deliberately distinct from the commit
`Indeterminate` phase. `commit` accepts only `Active`, and commit-outcome
`resolve` gates only on `Indeterminate`, so neither can accept or reinterpret a
page-append poison as a commit attempt. If the domain page port somehow
re-rejects the composition before invoking the append effect, that is mapped to
a retryable rejection that retains the active token without poisoning the
lifecycle.

## Shared-Frontier Trust Obligation

`TransactionPageLog<N>` documents a hard adapter obligation: transaction-page
appends and transaction commit appends for one lineage share exactly one
monotonic position space and one durable frontier, so a later commit flush also
makes every prior transaction-page record durable. The domain validates that
returned positions are lineage-bound; it cannot prove that an adapter honors
this shared frontier. The page port extends
`CommitLog<TransactionCommitRecord>`, requiring one adapter surface to provide
both append operations. An adapter that assigns handles sharing a lineage to
independent spaces or frontiers still violates this port even if each record is
individually well formed.

## Local No-Steal and No-Force Framing

The no-steal rule is local and structural: a transaction-owned wrapper cannot
reach `PageStore` until the owning transaction is committed, because the only
flush entrypoint requires a `&CommittedTransaction`. The rule is local because
the raw nontransactional page APIs remain public and unchanged; this slice does
not make stealing globally impossible.

Dropping a `TransactionDirtyPage<N>` before commit performs no page-store write
and defines no force-at-commit requirement. This slice supports at most one
staged image per page per transaction; multi-version flush ordering remains a
later buffer/recovery policy.

## Committed Gate

`flush_committed_page` requires a `&CommittedTransaction`, a `LogDurability`
log, a `PageStore<N>`, and a `TransactionDirtyPage<N>`. Before touching any port
it validates exact `TransactionId` equality, that the committed position shares
the page's WAL lineage, and that the committed position is strictly after the
page WAL position on the shared frontier. Identity equality alone is
insufficient because identities can repeat across independent lineages; a test
constructs two coordinators over different lineages that issue the same
`TransactionId` to prove the lineage and ordering checks are required. Each
pre-port rejection retains the wrapper.

On success it delegates to the existing WAL-before-store `flush_dirty_page`,
preserving the exact `flush_through` then `write_page` order, and returns a
transaction-owned `TransactionCleanPage<N>` that retains the owning identity.

## Error Retention

- A pre-append stage rejection retains the exact active token and unlogged page.
- An invoked append error or invalid evidence is terminal with no active path,
  records `PageAppendIndeterminate`, and preserves the page evidence and cause.
- A committed-flush pre-port rejection retains the transaction-owned wrapper.
- A WAL-flush failure retains a retryable transaction-owned dirty page and the
  exact cause because the store was not called.
- A store-write failure is terminal: it retains the committed identity, an
  `IndeterminatePageWrite`, and the exact cause and never manufactures success.

## Evidence Boundary

Every type operates only on repository-authored domain values and injected
ports. No external product, driver, SDK, fixture, oracle, or native
MDF/NDF/LDF/BAK format is consulted. Internal transaction, page, and log
identities are never exposed as SQL Server values.

## Test Boundaries

- Compile-fail tests reject forging `TransactionPageWriteRecord`,
  `TransactionDirtyPage`, and `TransactionCleanPage`, cloning
  `TransactionDirtyPage`, extracting a raw `DirtyPage`, and flushing with an
  active rather than committed token.
- Successful staging returns the active token plus one owned dirty page, keeps
  the lifecycle `Active`, appends the exact owner and page, and calls no store.
- Two different pages thread the returned active token before a successful
  commit.
- Dropping the wrapper before commit performs no store write.
- Foreign coordinator, foreign log lineage, foreign page lineage, and lifecycle
  mismatch each retain the exact active token and page and never append.
- A second image for the same page and transaction is rejected before append;
  the retained active token can still commit.
- An append source failure, a foreign returned position, and an append-time
  lineage rotation each produce terminal page evidence, record
  `PageAppendIndeterminate`, expose no active path, and cannot be reinterpreted
  by commit or resolve.
- A committed flush preserves `flush_through` then `write_page` order and
  positions and returns a transaction-owned clean page.
- A wrong transaction identity, a foreign commit lineage over an equal
  `TransactionId`, and a commit position not strictly after the page position
  each reject before any port and retain the wrapper.
- A WAL-flush failure retains a retryable dirty wrapper and exact cause; a
  store-write failure retains the committed identity, an
  `IndeterminatePageWrite`, and exact cause.
- Architecture tests accept the reviewed `ntsql-transaction -> ntsql-page` edge
  and reject the reverse `ntsql-page -> ntsql-transaction` edge.

## Non-Goals

This ADR does not:

- add a memory or filesystem adapter implementation of the transaction-page
  port or change any adapter record format or filesystem bytes;
- define rollback, abort records, checkpoints, redo/undo execution, or
  compensation;
- define visibility, isolation, buffering, eviction, or multi-version flush
  ordering;
- define force-at-commit or global no-steal removal of the raw page APIs; or
- make any SQL Server transaction, page, LSN, recovery, crash, diagnostic, or
  native file-format claim.

## Consequences

Transaction coordination can now stage a transaction-owned page image into the
WAL, keep the active token linear across multiple pages, and flush that image to
the page store only after a durable commit, with append and store ambiguities
kept terminal and distinct from commit indeterminacy. The transaction crate now
depends inward on `ntsql-page`, superseding the earlier sole-`ntsql-wal` claim,
while the dependency graph stays acyclic. Adapter implementation of the
transaction-page port, a persistent transaction-owner WAL format, and
recovery-time redo remain blocked on later work that supplies complete durable
ownership evidence.
