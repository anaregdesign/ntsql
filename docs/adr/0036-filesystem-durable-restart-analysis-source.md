# ADR 0036: Filesystem Durable Restart-Analysis Source

- Status: Accepted
- Date: 2026-08-06
- Issue: #124
- Extends: ADR 0014, ADR 0031, ADR 0034, ADR 0035
- Extended by: ADR 0037

## Context

ADR 0034 defines one adapter-neutral source for a complete durable logical WAL
prefix. ADR 0035 implements it for the deterministic memory adapter under an
exclusive mutable borrow. The filesystem WAL already owns the corresponding
persistent primitives:

- validated logical `FileLogRecord<N>` snapshots;
- a durable length reconstructed only from durable-through markers;
- exact lineage-bound positions;
- poison after uncertain writes; and
- one advisory exclusive file lock retained for the adapter lifetime.

The filesystem source must preserve those boundaries rather than copy only the
memory allocation loop. In particular, physical format frames are not logical
records, older formats remain complete relative to the record kinds they
support, and the cooperating-writer lock must remain held through callback
return.

## Crate and Dependency Boundary

Only `ntsql-storage-file` production code and tests change. The complete direct
dependency set remains:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No domain crate imports the adapter. No crate, dependency edge, architecture
registration, file header, frame, marker, checksum, repair rule, open
entrypoint, or page-store behavior changes.

## Format-Relative Complete Streams

`FileCommitLog<N>` implements
`DurableTransactionRestartAnalysisSource<N>` for every validated workspace WAL
format:

- v1 contributes transaction commits;
- v2 contributes raw pages and transaction commits; and
- v3 contributes raw pages, transaction-owned pages, and transaction commits.

Unlike ADR 0031 committed-page recovery, restart analysis does not require the
format to persist transaction-owned pages. Rejecting v1 or v2 would discard
their complete valid logical streams for an unrelated capability absence.

The source never fills that absence by inference. A v2 raw page remains
`Page`; it does not receive an owner from an adjacent commit or numeric
identity. A v1 or v2 commit therefore creates the existing ADR 0034
commit-only transaction entry with no page range and count zero. This is exact
format-relative metadata, not a claim that the older format supported
transaction-owned page changes.

## Logical Records and Physical Frames

The opened adapter's `records` vector contains one value per validated logical
record. Its scanner has already consumed and validated physical implementation
frames:

- epoch-allocation frames;
- page and transaction-page header frames;
- transaction-owner continuation frames;
- page-data frames; and
- durable-through marker frames.

Those frames never enter restart observations. One multi-frame page group
produces one `FileLogRecord` and therefore one unified observation. The exact
`durable_len` reconstructed from marker frames is both the logical prefix
length and the complete allocation upper bound.

## Poison-First Source Boundary

A poisoned `FileCommitLog<N>` cannot provide authoritative restart evidence.
The source returns
`FileTransactionRestartAnalysisSourceError::PoisonedWriter` before reading the
frontier, reserving capacity, scanning records, or invoking the callback.

Reopen remains the only path that synchronizes the file, validates complete
groups, performs permitted incomplete-tail repair, reconstructs marker state,
and publishes an unpoisoned adapter. The source neither clears poison nor
reinterprets the inspectable in-memory vector as authoritative.

## Exact Frontier and One-Pass Projection

After the poison check, the source snapshots:

- `durable_len`; and
- `durable_position()`, which clones the exact position owned by the final
  marker-covered logical record.

An empty prefix supplies `None` and an empty slice. A nonempty prefix supplies
the exact marker-covered logical tail. A complete record after the latest
marker remains inspectable through `records()` but appears in neither frontier
nor observations.

The source fallibly reserves `durable_len` elements in one vector, then creates
exactly one `durable_records()` iterator and scans it once in physical logical
order. It matches the record discriminant and emits exactly one variant:

- `FileLogRecordKind::PageWrite` becomes `Page`;
- `FileLogRecordKind::TransactionPageWrite` becomes `TransactionPage`; and
- `FileLogRecordKind::TransactionCommit` becomes `Commit`.

The distinction is load-bearing. Existing committed-page reconciliation
intentionally exposes an owner-free physical view of a transaction-owned page.
The restart source must not use that optional dual-projection surface, because
emitting both views would duplicate one WAL position. It instead uses private
non-optional projection helpers selected by the record discriminant.

Those helpers are shared with the existing public record projections. The
public committed-page behavior remains unchanged: an owned page can still
project into both physical and owner-aware views when that separate contract
requires them.

Every helper clones the record's existing `LogSequenceNumber` and exact fields.
No position is reconstructed from its numeric value, and no record is filtered,
grouped, deduplicated, reordered, or relabeled.

## Lifetime Lock and Callback Stability

Creation and open acquire the ADR 0014 advisory exclusive lock before format
work. `FileCommitLog<N>` owns that locked descriptor until drop. The restart
source neither unlocks, reopens, nor replaces it.

From poison check through callback return, the source therefore holds:

- an exclusive `&mut FileCommitLog<N>` borrow, preventing safe operations
  through the same in-process value; and
- the adapter-lifetime file lock, excluding a cooperating second opener of the
  same inode.

The callback is invoked exactly once after complete projection. Capacity,
poison, or record projection failure invokes it zero times. A test attempts a
second v3 open from inside the callback and requires
`AcquireExclusiveLock`/`WouldBlock`, then continues to inspect the unchanged
frontier and observation slice.

The lock remains advisory. A non-cooperating writer, hostile path replacement,
unsupported lock semantics, or storage that violates successful `sync_all`
guarantees remains outside the trusted adapter contract.

## Errors

`FileTransactionRestartAnalysisSourceError<N>` distinguishes:

- `PoisonedWriter`;
- `ObservationCapacityExhausted { record_count }`;
- `PageProjection`;
- `TransactionPageProjection`; and
- `CommitProjection`.

Capacity failure retains the exact requested logical count. Projection
variants box and retain their exact domain causes through `Error::source`.
Private record construction and the validated scanner ordinarily make those
conversions infallible, but a future invariant violation cannot become a
skipped record, empty prefix, panic, or success-shaped fallback.

The source performs no physical effect. Every failure returns before callback
invocation and grants no partial-analysis authority.

## Reopen and Unmarked-Suffix Scenario

The v3 integration scenario first persists and reopens:

```text
1 transaction-owned page for transaction A
2 raw page
3 commit for transaction A
4 transaction-owned page for transaction B
5 commit-only transaction C
6 raw page
```

After reopen, a new-epoch transaction D appends an owned page at position 7 and
makes that page durable. Its complete commit record is appended at position 8,
but a before-flush fault prevents a covering durable marker.

The physical logical record vector contains positions 1 through 8. The restart
source returns frontier 7 and exactly seven observations, so D remains
uncommitted. Scanning `records()` would include position 8 and change D's
classification, making marker selection observable rather than cosmetic.

A second reopen retains all eight complete records but reconstructs the same
marker-covered frontier 7. Fresh analysis reproduces:

- committed A with owned-page range 1..1 and commit 3;
- uncommitted B with range 4..4;
- commit-only C with no page range and commit 5; and
- uncommitted D with range 7..7.

Entries remain sorted by persisted epoch and sequence. No observation or
runtime transaction token is carried across reopen.

Separate v1 and v2 reopen tests prove format-relative behavior. V1 yields one
commit-only entry. V2 yields one raw-page observation plus one commit and still
produces a commit-only entry rather than inferring ownership.

## Authority and Evidence Boundary

The filesystem adapter supplies trusted point-in-time evidence only. It does
not construct:

- an active, committed, recovered, or other transaction lifecycle token;
- a live or recovery page-write permit;
- a recovered storage owner;
- a replay, redo, undo, rollback, or compensation command;
- a checkpoint record or checkpoint-validity proof; or
- a log flush, truncation, retention, or reclamation capability.

Holding the file lock stabilizes the callback prefix; it does not turn the
result into continuing currency after callback return. Later live operations
may advance the WAL.

All behavior is workspace-authored. No external product documentation, driver,
SDK, fixture, oracle, proprietary governance tool, or native
MDF/NDF/LDF/BAK format is consulted. This ADR defines no SQL Server recovery,
transaction-table, LSN, lock, error, diagnostic, or compatibility behavior.

## Test Boundaries

- Empty v3 invokes the callback once with no frontier or observations and yields
  an empty analysis.
- Reopened v1 contributes one commit and yields one zero-page commit-only entry.
- Reopened v2 contributes one raw page and one commit, with no inferred owner.
- Reopened v3 projects exact mutually exclusive kinds, payloads, positions, and
  lineage for marker-covered positions 1 through 7.
- One owned page produces exactly one `TransactionPage` observation despite its
  separate physical reconciliation view.
- A complete unmarked commit at position 8 is excluded before and after a
  second reopen and cannot change transaction D's state.
- A second cooperating opener fails from inside the callback while the supplied
  frontier and slice remain unchanged.
- Poison and real `usize::MAX` reservation failure invoke no callback.
- Synthesized malformed raw-page, transaction-page, and commit records retain
  exact nested projection causes and invoke no callback.
- Existing committed-page tests continue to prove the owner-free physical view
  of a transaction-owned record, preserving the shared-helper refactor.
- Existing format bytes, scanner, marker, repair, poison, lock, fault, page
  recovery, startup ownership, and architecture tests remain unchanged.

## Non-Goals

This ADR does not:

- bind analysis to `RecoveredTransactionPageStorage`;
- change v1/v2/v3 headers, frames, checksums, markers, synchronization, tail
  repair, poison, open, or lock acquisition;
- migrate an older WAL or infer transaction ownership absent from its format;
- add a page-store read or write, multi-file transaction, database-wide lock,
  wait protocol, or global lock-order registry;
- add a checkpoint, dirty-page table, replay plan, page index, or transaction
  coordinator restoration;
- execute redo, undo, rollback, abort, compensation, or page mutation;
- choose a replay start, retention floor, truncation boundary, or reclaim a log;
  or
- define external SQL Server values or native file compatibility.

## Consequences

Both deterministic memory and persistent filesystem WAL adapters now satisfy
the ADR 0034 complete-prefix source contract without manual projection or
authority leakage.

ADR 0037 binds restart analysis to the ADR 0033 recovered storage owner.
Persistent checkpoints, dirty-page analysis, replay start, undo/compensation,
and log reclamation remain separately reviewed work.
