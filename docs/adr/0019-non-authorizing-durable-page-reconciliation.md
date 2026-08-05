# ADR 0019: Non-Authorizing Durable Page Reconciliation

- Status: Accepted
- Date: 2026-08-06
- Issue: #88
- Extends: ADR 0001, ADR 0011, ADR 0015, ADR 0016, ADR 0017, ADR 0018
- Extended by: ADR 0020, ADR 0021, ADR 0022, ADR 0023, ADR 0024

## Context

ADR 0017 reconstructs durable full-image page WAL records, and ADR 0018
reconstructs the latest durable page-store snapshot for each page. Both retain
the exact lineage-bound WAL position. Recovery can now compare these two
evidence sources without consulting a physical file format.

The current WAL page record deliberately has no transaction identity, commit
association, undo image, or visibility state. Treating a physical comparison as
permission to replay a page could therefore expose an uncommitted image or
destroy state needed by later undo. Checkpoint selection and mutation-capable
redo/undo must remain separate until transaction ownership is explicit.

The smallest safe next step is an I/O-free, allocation-free comparison that
describes physical agreement or contradiction but cannot authorize a write.

## Crate and Dependency Boundary

`ntsql-page` owns the comparison because it already owns page identity, exact
images, WAL-before-store ordering, and the unforgeable write permit. Its direct
dependency set remains unchanged:

```text
ntsql-page -> ntsql-wal
```

No adapter type enters the domain crate. Adapters may later project their
validated records and snapshots into the domain observations through public
constructors. This ADR does not add those projections, a recovery crate, or any
new architecture edge.

## Observation Values

`DurablePageWalObservation<N>` owns:

- one nonzero `PageNumber`;
- one adapter-assigned `PageVersion`;
- one exact nonempty `PageImage<N>`; and
- one nonzero lineage-bound `LogSequenceNumber`.

`StoredPageSnapshotObservation<N>` owns the same values, with its position
interpreted as the exact WAL position required by the stored snapshot.

The values are adapter-neutral observations, not durability proofs. Their
constructors reject zero positions and retain every input in the typed error.
`PageImage<N>` already rejects zero width. The private common representation
prevents the two observation roles from being interchanged accidentally while
keeping their value checks identical.

## Per-Page Input Contract

`reconcile_durable_page` receives:

1. one expected `LogLineage`;
2. one expected `PageNumber`;
3. zero or one current stored snapshot observation; and
4. every durable full-image WAL observation for that page in increasing physical
   WAL order.

The WAL iterator may have numeric gaps because transaction records and other
pages share the log. It must not omit a durable page record for the selected
page. Whole-prefix grouping and adapter iteration remain caller
responsibilities.

Before comparing raw numeric positions, the function validates that the
snapshot and every WAL observation belong to the expected page and lineage.
Numeric position order is meaningful only after the lineage check.

## Ordering and Evidence Rules

WAL observations for one page must advance strictly by position:

- a lower later position is a non-advancing-order error;
- an equal position with identical version and bytes is a duplicate error; and
- an equal position with different version or bytes is a contradictory-position
  error.

`PageVersion` does not define recency. A numerically lower version at a later
same-lineage WAL position is still the later physical observation. Version and
image bytes are equality evidence only.

When a snapshot exists, its required position must match one supplied durable
page WAL observation for the same page. At that exact position, page version and
every image byte must agree. A missing match is an unbacked-position error; this
includes a snapshot ahead of the durable page prefix.

Because this domain input contains page observations only, an unbacked position
cannot distinguish a future position, a transaction-record position, an
omitted page observation, or mismatched files. The reduced diagnosis is
intentional and fail-closed.

## Physical Classifications

After all observations validate, exactly one inert classification is returned:

- `NoDurableState`: no snapshot and no durable page WAL observation;
- `StoreMissing`: at least one durable page WAL observation and no snapshot;
- `ExactCurrent`: the snapshot matches the durable observation at its required
  position, and that position is the highest supplied position; or
- `StoreBehind`: the snapshot matches its backing durable observation, and a
  strictly later durable full image exists.

`StoreBehind` compares the snapshot with the older WAL record that actually
backs it. Differing version or bytes in the latest record are expected and are
not corruption.

## Allocation and Authority Boundary

The function performs one pass over borrowed observations. It builds no
collection, uses no `Vec`, performs no fallible reservation, and retains only
the previous observation, latest numeric position, and whether the snapshot was
backed. Returned positions remain bound to the expected lineage.

The result contains no page image, `DirtyPage`, `PageWritePermit`, callback,
mutation port, or replay command. Compile-fail tests prevent conversion to dirty
or write-authorizing state. A classification is not redo analysis and cannot
establish transaction visibility.

This ADR does not:

- project memory or filesystem adapter records into observations;
- iterate or group an entire durable prefix;
- resolve an indeterminate append or page-store write;
- mutate a page store;
- define checkpoint, analysis, redo, undo, compensation, or idempotence rules;
- define transaction ownership, commit visibility, or rollback;
- define page reads, buffering, allocation, compaction, or eviction; or
- define any SQL Server page, LSN, recovery, crash, diagnostic, or native file
  behavior.

## Evidence Boundary

The comparison operates only on repository-authored domain values and
workspace-owned format observations. It does not consult an external product,
driver, SDK, fixture, oracle, or native MDF/NDF/LDF/BAK format. Its outcomes are
internal physical states and make no compatibility claim.

## Adapter Projection Extension

Issue #90 adds allocation-free raw-byte constructors for both observation roles.
They validate zero width and zero position before constructing `PageImage<N>`
and retain the exact page number, version, `[u8; N]`, and lineage-bound position
in a typed error. The existing `PageImage`-based constructors and reconciliation
rules do not change.

`InMemoryLogRecord` and `FileLogRecord` project a page record by copying its
exact fields and cloning its existing `LogSequenceNumber`. A transaction record
returns `Ok(None)` explicitly. Projection does not itself prove durability;
callers must select the adapter's validated durable prefix.

`InMemoryStoredPage` and `FileStoredPage` project their exact current snapshot,
including the existing required-position lineage. No projection reconstructs a
position from its numeric value, substitutes another lineage, reads a file,
allocates a collection, or infers absent data.

Memory restart and filesystem reopen integration tests project complete durable
prefixes and current snapshots into this ADR's domain function. They establish
exact-current state, a store behind a later WAL-only full image across an
interleaved transaction record, and a missing snapshot from an empty store.
These adapter integrations remain observational and grant no mutation
authority.

## Test Boundaries

- Exact-current tests require equality at the latest durable position.
- Behind-store tests match the older backing record while the later record has
  different bytes and a lower page version.
- Missing-store and no-state tests cover both absence classifications.
- Unbacked snapshots fail whether their numeric position is within or beyond
  the observed page range.
- Snapshot version or byte disagreement at the backing position fails closed.
- Foreign snapshot and WAL lineages fail before numeric order is considered.
- Wrong-page observations fail explicitly.
- Identical duplicate, contradictory duplicate, and decreasing WAL positions
  have distinct typed failures.
- Zero-position construction retains all supplied values.
- Raw-byte projection tests retain zero-width and zero-position inputs.
- Memory and filesystem tests preserve exact record/snapshot lineages through
  restart or reopen before reconciliation.
- Interleaved transaction records project to `None` and do not disturb page
  position ordering.
- Compile-fail tests prove comparison cannot create a permit or dirty page.
- Architecture validation proves the dependency graph did not change.

## Consequences

The domain can now establish whether supplied durable page evidence is
physically current, behind, missing, absent, or contradictory without granting
write authority. The memory and filesystem adapters can project their validated
records and snapshots into these observations and exercise restart/reopen
comparisons. Mutation-capable recovery remains blocked on explicit
transaction-owned page records and visibility/undo semantics.
