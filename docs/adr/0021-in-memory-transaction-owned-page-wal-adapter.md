# ADR 0021: In-Memory Transaction-Owned Page WAL Adapter

- Status: Accepted
- Date: 2026-08-06
- Issue: #94
- Extends: ADR 0008, ADR 0010, ADR 0011, ADR 0016, ADR 0019, ADR 0020
- Extended by: ADR 0022, ADR 0023, ADR 0024

## Context

ADR 0020 defined the transaction-owned no-steal page typestate in
`ntsql-transaction`: `TransactionPageLog<N>` documents a shared-frontier append
port for transaction-owned page images, `TransactionDirtyPage<N>` cannot reach a
page store until its owning transaction is durably committed, and
`flush_committed_page` is the only committed gate. ADR 0020 explicitly deferred
any memory or filesystem adapter implementation of that port.

ADR 0008 established the deterministic in-memory commit log with one lineage,
one monotonic position allocator, one durable-prefix frontier, and one armed
one-shot fault plan. ADR 0016 made that log const-generic over the page width so
transaction commit records and full page-image records share exactly that single
frontier, and added the separate `InMemoryPageStore<N>`. ADR 0019 added
non-authorizing durable page reconciliation and its adapter projections, keeping
the page WAL record deliberately commit-agnostic. ADR 0010 established
authoritative commit-outcome resolution, and ADR 0011 bound log positions to
their lineage.

The smallest correctness-first next step is the outer in-memory adapter slice
that implements `TransactionPageLog<N>` for `InMemoryCommitLog<N>` and makes the
transaction-owned page record inspectable, without changing any filesystem
format or byte layout and without adding recovery authorization.

## Crate and Dependency Boundary

No crate is added and no dependency edge changes. `ntsql-storage-memory` already
depends on exactly `ntsql-page`, `ntsql-transaction`, and `ntsql-wal`, so the
reviewed architecture graph and the `ntsql-architecture-check` allow-list are
unchanged. The adapter imports `TransactionPageLog` and
`TransactionPageWriteRecord` from `ntsql-transaction` and implements the port on
its existing log type. This ADR therefore does not extend the crate-boundary
ADR 0001.

## Owner Versus Commit Accessor Separation

`InMemoryLogRecordKind<N>` gains a third variant,
`TransactionPageWrite(InMemoryTransactionPageWriteRecord<N>)`, alongside the
existing `TransactionCommit` and nontransactional `PageWrite`.
`InMemoryTransactionPageWriteRecord<N>` owns the exact owning `TransactionId`
plus the same `InMemoryPageWriteRecord<N>` payload used by the nontransactional
record. It is private-constructible only through the adapter's append path; a
compile-fail doctest proves safe downstream code cannot forge one.

The record accessors keep an exact separation between page ownership and durable
commitment:

- `transaction_id()` remains commit-only. It returns `Some` for a
  `TransactionCommit` record and `None` for both page variants. This is critical
  because `TransactionRecoverySource::lookup_durable_commit` scans this accessor;
  a transaction-owned page must never be mistaken for a durable commit record,
  and must never produce a duplicate match when a real commit for the same
  identity is also present.
- `page_owner_transaction_id()` returns `Some` only for a transaction-owned page
  record and `None` for commit and nontransactional page records. It is a page
  ownership tag, never a durable commit signal.
- `transaction_page_write()` returns the typed owned record when present.
- `page_write()` returns the identical page payload for both page variants and
  `None` for a commit record.
- `page_recovery_observation()` projects both page variants exactly and projects
  a commit record to `None`. The owning identity is intentionally not carried
  into `DurablePageWalObservation`: ADR 0019 reconciliation stays
  commit-agnostic and non-authorizing.

## One Shared Structural Plan

`TransactionPageLog<N>::append_transaction_page` mirrors the raw `PageLog<N>`
append exactly. It rejects a foreign page lineage before any fault, capacity, or
position effect, then copies the owner and page fields and routes through the
same single private `append_record`. It adds no allocator, no separate position
space, no separate durable frontier, and no additional fault state. Transaction
commit records, nontransactional page records, and transaction-owned page
records therefore share one `LogLineage`, one monotonic position allocator and
physical order, one durable-prefix frontier, and the existing before/after
append and flush faults. The raw `PageLog<N>` implementation is unchanged.

## Commit Prefix Coverage

Because commit records and owned page records share one contiguous durable
prefix, a later commit flush also makes every prior owned page record durable.
A staged owned page at position *p* followed by a commit at position *c > p* has
`c` strictly after `p`; flushing through `c` covers `p` in the same prefix. This
is the adapter honoring the ADR 0020 shared-frontier obligation, exercised by a
full stage → commit → `flush_committed_page` path over one log instance.

## Durability Is Not Authorization

A durable owned page record is not a commit and grants no write authority.
Flushing an owned page record into the durable prefix before any commit leaves
`transaction_id()` `None`, `lookup_durable_commit` `Absent`, and the page store
empty. Only `flush_committed_page` with a `&CommittedTransaction` can reach the
store, so manual WAL durability alone never authorizes a page-store write.

## Restart, Reopen, and High-Water

Restart discards the volatile suffix, including volatile owned page records,
while retaining the position high-water mark. A durable owned page and its
commit survive restart with exact records and positions. Persistent reopen
reconstructs both the owned page and commit positions under the persistent
lineage with the exact owner, page payload, and preserved allocator high-water
mark. Existing commit-only and nontransactional page restart/reopen behavior is
unchanged.

## Stale-Wrapper Reflush Guard

The retained position high-water mark is the exact guard against a stale
transaction-owned wrapper. If an owned page is staged and then lost by restart
while the caller retains its `ActiveTransaction` and `TransactionDirtyPage`,
committing the retained active token on the restarted log yields a later commit
position from the high-water allocator. `flush_committed_page` then attempts to
flush through the wrapper's exact stale required position, which no surviving
record owns, so it returns `LogFlush` carrying
`InMemoryCommitLogError::UnknownFlushPosition` for that exact stale position,
retains the retryable dirty wrapper, and never calls the page store. This exact
required-position reflush is the intended barrier and must not be bypassed or
reinterpreted.

## Error Retention

- A foreign page lineage is rejected before any fault, position, or record
  effect and retains the caller's active token and page.
- An owned-page `BeforeAppend` fault leaves no record and no position effect,
  consumes the fault, and records `PageAppendIndeterminate`.
- An owned-page `AfterAppend` fault leaves one volatile owned record and records
  `PageAppendIndeterminate`.
- A commit `BeforeFlush` after a staged owned page leaves both records volatile,
  is commit-indeterminate, and yields a volatile-commit lookup error with no
  store effect.
- A commit `AfterFlush` makes the owned page and commit durable, is
  commit-indeterminate, resolves as `Committed` against the same log, and then
  permits a successful `flush_committed_page`.
- A committed-flush WAL-flush error retains the retryable transaction-owned
  dirty wrapper and never calls the store.
- A committed-flush page-store `BeforeWrite` or `AfterWrite` error preserves the
  committed owner and a terminal `IndeterminatePageWrite` with exact physical
  effects: `BeforeWrite` leaves the store empty, `AfterWrite` mutates the store
  and then reports failure.

## Evidence Boundary

The adapter operates only on repository-authored domain values and injected
ports. It consults no external product, driver, SDK, fixture, oracle, or native
MDF/NDF/LDF/BAK format. Internal transaction, page, and log identities are never
exposed as SQL Server values. The inspectable model records exist for tests and
do not elevate the adapter into an external oracle.

## Test Boundaries

- An owned record snapshots the exact owner, number, version, bytes, and
  position; private construction by fields remains impossible, proven by a
  compile-fail doctest.
- A durable owned page without a commit reports `transaction_id()` `None`, owner
  `Some`, an exact page projection, and `lookup_durable_commit` `Absent`.
- An owned page plus a real commit for the same identity looks up `Found` at the
  commit position, never `Duplicate`, with the page position before the commit
  position and both durable in one prefix.
- A full stage → commit → `flush_committed_page` over one log instance produces
  a transaction-owned clean page and a store snapshot with the exact owner,
  page, position, and bytes.
- A manual page flush before commit is durable but leaves the store empty and
  `lookup_durable_commit` `Absent`.
- Owned-page `BeforeAppend` and `AfterAppend` faults are terminal
  `PageAppendIndeterminate` with the exact record and position effects.
- A commit `BeforeFlush` after a staged owned page leaves both records volatile
  with a volatile-commit lookup error.
- A commit `AfterFlush` resolves `Committed` against the same log and then
  flushes the owned page.
- Committed-flush store faults preserve the committed owner and terminal
  indeterminate write with exact physical effects.
- The restart stale-wrapper reflush returns `LogFlush` with the exact
  `UnknownFlushPosition`, retains the dirty wrapper, and never calls the store.
- Restart and persistent reopen retain the durable owned page and commit records,
  positions, owner, payload, and allocator high-water mark.
- The coordinator rejects a foreign page lineage before the transaction-page
  port, preserving its armed fault and position allocator. The adapter
  implementation independently mirrors the raw page port's lineage guard.

## Non-Goals

This ADR does not:

- change any filesystem format, byte layout, or persistent record shape, or add
  a filesystem transaction-owned page WAL adapter;
- add recovery authorization, redo/undo execution, rollback, abort records,
  checkpoints, or compensation;
- define visibility, isolation, buffering, eviction, force-at-commit, or
  multi-version flush ordering;
- carry the owning transaction identity into `DurablePageWalObservation` or make
  ADR 0019 reconciliation commit-aware or authorizing; or
- make any SQL Server transaction, page, LSN, recovery, crash, diagnostic, or
  native file-format claim.

## Consequences

The in-memory adapter can now stage transaction-owned page images into the
shared WAL frontier, keep page ownership strictly separate from durable commit
evidence, and flush an owned image to the page store only after a durable commit,
with append, flush, and store ambiguities kept terminal and distinct from commit
indeterminacy. Because owner and commit accessors are separated, recovery commit
scans remain correct in the presence of owned page records. A filesystem
transaction-owned page WAL format and recovery-time redo remain blocked on later
work that supplies complete durable ownership evidence.
