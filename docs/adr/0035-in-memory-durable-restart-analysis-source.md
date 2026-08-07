# ADR 0035: In-Memory Durable Restart-Analysis Source

- Status: Accepted
- Date: 2026-08-06
- Issue: #122
- Extends: ADR 0030, ADR 0034
- Extended by: ADR 0036, ADR 0037, ADR 0060

## Context

ADR 0034 defines one adapter-neutral source port for a complete durable logical
WAL prefix. The domain validates a unified stream of raw page,
transaction-owned page, and commit observations before constructing inert
transaction restart metadata. It intentionally leaves concrete projection to
the adapters.

`InMemoryCommitLog<N>` already stores those three logical record kinds under one
lineage, identifies an exact durable prefix with `durable_len`, and exposes
`durable_records()` in physical order. Its exclusive mutable borrow can protect
that prefix through a higher-ranked callback. The smallest concrete source is
therefore the deterministic memory adapter, without changing a persistent
format or recovery authority.

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

No domain crate imports the adapter. No crate, dependency edge, architecture
registration, filesystem API, or persistent format changes.

## Exact Durable Frontier

Before allocation or scanning, the source snapshots:

- `durable_len`, the exact number of logical records in the durable prefix; and
- `durable_position()`, the exact existing lineage-bound tail position.

An empty prefix supplies `None` and an empty observation slice. A nonempty prefix
supplies the position already owned by its final durable record. The source does
not reconstruct a position from its numeric value, infer a frontier from the
physical record count, or include a complete record from the volatile suffix.

The memory format contains no nonlogical frame records. Consequently,
`durable_len` is both the durable logical-record count and a complete allocation
upper bound for the unified observation stream.

## One-Pass Mutually Exclusive Projection

The source fallibly reserves exactly `durable_len` elements in one vector before
scanning. Capacity failure retains the requested record count and invokes no
callback.

It then creates exactly one `durable_records()` iterator and scans it once in
physical order. Each memory record enters exactly one unified variant:

- `PageWrite` becomes `DurableTransactionRestartObservation::Page`;
- `TransactionPageWrite` becomes
  `DurableTransactionRestartObservation::TransactionPage`; and
- `TransactionCommit` becomes
  `DurableTransactionRestartObservation::Commit`.

Although a transaction-owned page can also expose the owner-free physical view
used by committed-page reconciliation, this source never uses that dual
projection. Emitting both views would create a duplicate WAL position and
violate the complete logical stream contract.

Every projection copies exact record fields and clones the record's existing
`LogSequenceNumber`. No record is reordered, filtered by page number, grouped,
deduplicated, or reconstructed.

## Stability and Callback Boundary

`with_durable_transaction_restart_observations` holds
`&mut InMemoryCommitLog<N>` from before the frontier snapshot and reservation
through callback return. The log has no interior-mutable append or flush path,
so safe in-process code cannot advance or replace the prefix during the
callback.

The callback is invoked exactly once after the complete vector is built. A
projection or capacity failure invokes it zero times. The observations and
frontier borrow only callback-local owned values and cannot escape through the
higher-ranked source contract.

This continuous mutable borrow is the entire stability boundary for the
deterministic adapter. It provides no cross-thread, cross-process,
operating-system flush, or filesystem locking claim.

## Errors

`InMemoryTransactionRestartAnalysisSourceError<N>` distinguishes:

- unified observation capacity exhaustion, retaining `record_count`;
- raw-page projection failure;
- transaction-page projection failure; and
- commit projection failure.

Each record conversion remains fallible even though private memory-record
construction ordinarily preserves its invariants. Projection variants box and
retain the exact domain error through `Error::source`. A malformed record is
never skipped, relabeled, or converted into an empty successful prefix.

The source performs no physical effect. If it fails, it returns no callback
output and grants no partial-analysis authority.

## Restart and Reopen Boundary

The integration scenario persists this logical order:

```text
1 transaction-owned page for transaction A
2 raw page
3 commit for transaction A
4 transaction-owned page for transaction B
5 commit-only transaction C
6 raw page
```

It then appends a complete commit for B at position 7 while a before-flush fault
keeps that record volatile. A correct source returns frontier 6 and classifies B
as uncommitted. Scanning `records()` would include position 7 and incorrectly
classify B as committed, so volatile exclusion is load-bearing.

`restart()` removes position 7. Persistent `reopen()` reconstructs positions
1 through 6 under the persistent lineage capability. Fresh analysis reproduces
the exact frontier, variant order, identity order, page ranges and counts, and
commit classifications. No observation is carried across reopen.

## Authority and Evidence Boundary

The memory adapter supplies trusted evidence to ADR 0034 and nothing more. It
does not construct:

- a transaction lifecycle token;
- a live or recovery page-write permit;
- a recovered storage owner;
- a replay, redo, undo, rollback, or compensation command;
- a checkpoint or checkpoint-validity proof; or
- a log flush, truncation, or reclamation capability.

The analysis remains immutable point-in-time metadata. This source does not bind
it to the ADR 0033 recovered owner and does not make later WAL advancement
impossible.

All behavior is workspace-authored. No external product documentation, driver,
SDK, fixture, oracle, proprietary governance tool, or native
MDF/NDF/LDF/BAK format is consulted. This ADR defines no SQL Server recovery,
transaction-table, LSN, error, diagnostic, or compatibility behavior.

## Test Boundaries

- An empty memory log invokes the callback once with no frontier or records and
  yields an empty analysis.
- The interleaved prefix projects exact mutually exclusive kinds and positions
  1 through 6 in physical order.
- The transaction-owned records at positions 1 and 4 are not also emitted as
  raw pages.
- The complete volatile commit at position 7 is excluded from the frontier,
  stream, and transaction classification.
- Analysis yields identity-sorted committed A, uncommitted B, and zero-page
  committed C entries with exact page ranges, counts, and commit positions.
- Persistent restart/reopen reproduces the same durable result and lineage.
- Projection-error wrappers retain exact raw-page, transaction-page, and commit
  causes; capacity failure retains the requested record count.
- Existing WAL, page recovery, startup ownership, domain, and architecture tests
  remain unchanged.

## Non-Goals

This ADR does not:

- implement the source in `ntsql-storage-file`;
- bind restart analysis to `RecoveredTransactionPageStorage`;
- change memory WAL append, flush, restart, reopen, or fault behavior;
- change filesystem bytes, frames, markers, checksums, synchronization, repair,
  poison, or lock behavior;
- add a checkpoint, dirty-page table, replay plan, page index, or transaction
  coordinator restoration;
- execute redo, undo, rollback, abort, compensation, or page-store mutation;
- choose a replay start, retention floor, truncation boundary, or reclaim a log;
  or
- define external SQL Server values or native file compatibility.

## Consequences

The deterministic memory WAL now satisfies the complete-prefix source contract
and can drive ADR 0034 analysis without manual projection or authority leakage.

ADR 0036 implements the same contract in the filesystem WAL while holding its
cooperating-writer lock continuously across frontier capture, projection, and
callback return. ADR 0037 binds both source implementations behind recovered
storage ownership.
