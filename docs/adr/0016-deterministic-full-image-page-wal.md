# ADR 0016: Deterministic Full-Image Page WAL

- Status: Accepted
- Date: 2026-08-06
- Issue: #82
- Extends: ADR 0001, ADR 0008, ADR 0011, ADR 0012, ADR 0015
- Extended by: ADR 0017, ADR 0018, ADR 0019

## Context

ADR 0015 proves that an already dirty page cannot reach its store before the
exact required WAL prefix is durable. Its initial public constructor could still
pair arbitrary same-lineage page bytes and a caller-created position without
evidence that the bytes were appended. Recovery work also needs one
deterministic adapter in which transaction commits and page changes share an
observable log order.

An append error does not reveal whether the record was physically added. As
with a page-store error, returning a directly retryable input would authorize a
duplicate append without first resolving that effect.

## Decision

`ntsql-page` adds these I/O-free stages and ports:

- `UnloggedPage<const N>` owns one address, adapter-assigned version, and exact
  nonempty image before any WAL position exists.
- `PageLog<N>` extends `LogDurability` with append of one complete page image.
- `stage_page_write` validates lineage, appends the image, validates the exact
  returned position and unchanged post-append lineage, and only then constructs
  `DirtyPage`.
- `IndeterminatePageLogAppend` is the terminal state after any invoked append
  returns an error or invalid evidence.

A foreign log is rejected before append and returns the unchanged unlogged page.
After append is invoked, no direct conversion back to unlogged, dirty, or clean
state exists. An adapter error is retained unchanged. A successful append that
returns a foreign position or rotates lineage retains the observed position and
is also terminal. `DirtyPage` construction is private, so safe downstream code
cannot bypass this sequence.

The complete in-memory page path is:

```text
UnloggedPage
    -> append full image
    -> validate returned lineage-bound position
    -> DirtyPage
    -> flush through exact position
    -> write page with private permit
    -> CleanPage
```

`ntsql-storage-memory` gains the direct dependency `ntsql-page`. Its synthetic
log is const-generic over the page width while retaining the existing default
for transaction-only callers. Transaction commit records and full page-image
records share:

- one `LogLineage`;
- one monotonic position allocator and physical record order;
- one durable-prefix frontier;
- the existing before/after append and flush faults; and
- restart, persistent reopen, and allocator high-water behavior.

Record kind is explicit. A page record snapshots exactly its `PageNumber`,
`PageVersion`, and `[u8; N]`; it has no dynamic page-size fallback.

The separate `InMemoryPageStore<N>` is created for the same lineage and keeps
one durable snapshot per page number. It validates page lineage and the exact
permit position before mutation, explicitly reports capacity exhaustion, and
replaces an existing snapshot only when invoked with a valid later write.
`BeforeWrite` fails without changing a page; `AfterWrite` changes the snapshot
and then reports failure. Both are terminal at the ADR 0015 domain boundary
because callers cannot infer physical effect from the common error shape.

## Allocation and Trust Boundaries

The adapter uses `Vec` only for inspectable model records and stored pages. It
reserves capacity fallibly before insertion and returns a typed exhaustion
error. Const-sized page bytes are copied only after capacity is available.

The ports remain trusted. The domain validates observable lineage and position
evidence but cannot prove that an arbitrary adapter persisted the exact bytes or
honored a successful flush. The memory adapter makes its physical effects
inspectable for tests; that does not elevate it into an external oracle.

## Restart and Recovery Boundary

Memory-log restart removes its volatile suffix, including volatile page records,
while retaining position high-water marks. Persistent reopen reconstructs every
durable commit and page-record position under the same persistent identity.
Page-store snapshots represent writes that already returned durable success or
an inspectable after-effect fault.

This change does not perform analysis, redo, undo, checkpointing, or resolution
of either indeterminate append or page write. A later recovery component must
decide how validated durable page records relate to page-store contents.

## Evidence Boundary

The full-image record is repository-authored test-model data. It defines no SQL
Server page size, page header, checksum, LSN, log record, buffer policy,
checkpoint, diagnostic, crash outcome, or MDF/NDF/LDF/BAK representation. No
external observation or proprietary format was consulted.

## Test Boundaries

- Domain tests prove `append -> dirty` staging, pre-append rejection, terminal
  append failure, foreign returned positions, and append-time lineage rotation.
- Compile-fail tests reject direct dirty-page construction and every terminal
  state-to-retry bypass.
- Memory tests prove transaction and page records share one exact position order
  and durable prefix.
- Restart/reopen tests prove volatile loss, durable position reconstruction, and
  no position reuse.
- End-to-end tests prove `append -> flush -> store -> clean` and exact retained
  address, version, image, and position.
- Before/after page-store faults prove different inspectable effects while both
  return terminal indeterminate state.
- Architecture tests allow exactly `ntsql-page`, `ntsql-transaction`, and
  `ntsql-wal` for the memory adapter and reject the reverse page-to-adapter edge.

## Consequences

The deterministic model can now falsify page-WAL ordering and ambiguity
hypotheses before a filesystem format exists. ADR 0017 specifies versioned
persistent page-log records, and ADR 0018 specifies the separate filesystem
page-store barrier without changing the domain staging sequence. Checkpoints,
redo/undo, page eviction, and external compatibility remain separate work.
