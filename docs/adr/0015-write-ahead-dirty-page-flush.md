# ADR 0015: Write-Ahead Dirty-Page Flush

- Status: Accepted
- Date: 2026-08-06
- Issue: #80
- Extends: ADR 0001, ADR 0005, ADR 0011, ADR 0012
- Extended by: ADR 0016, ADR 0018, ADR 0019, ADR 0020

## Context

ADR 0005 prevents commit acknowledgement before the exact commit-log position
is durable. A page store introduces a second write-ahead boundary: it must not
report a dirty page clean before the log prefix required by that image is
durable. Implementing this rule inside a filesystem adapter would hide domain
ordering in infrastructure and couple page lifecycle to a physical format.

A page-store error is also an effect boundary. Once a write was attempted, an
error does not prove whether physical state changed. Returning the original
dirty state would authorize a blind retry and could overwrite newer state.

## Decision

`ntsql-wal` separates durable-prefix authority from append authority:

- `LogDurability` owns the adapter error, lineage, and
  `flush_through(position)` operation.
- `CommitLog<Record>` extends `LogDurability` with `append_commit(record)`.

This preserves the existing commit fence while allowing page policy to request
durability without appending transaction records.

`ntsql-page` is an I/O-free domain crate whose only direct dependency is
`ntsql-wal`. It owns:

- a nonzero `PageNumber` and a `PageAddress` whose equality includes one
  `LogLineage`;
- an adapter-assigned `PageVersion` with no external representation;
- `PageImage<const N: usize>`, which owns exactly `[u8; N]` and rejects `N = 0`
  without allocation or a fallback size;
- staged `DirtyPage`, privately constructed `CleanPage`, and terminal
  `IndeterminatePageWrite` values; and
- the `PageStore` inward port and `flush_dirty_page` ordering operation.

Dirty-page construction requires the page address and exact required
`LogSequenceNumber` to share one lineage. ADR 0016 makes that constructor
private: safe downstream code first supplies an `UnloggedPage` to `PageLog`, and
only validated append evidence can create `DirtyPage`.

`flush_dirty_page` performs exactly these stages:

1. Reject a log or page store from another lineage before calling either port.
2. Call `LogDurability::flush_through` with the dirty page's exact required
   position.
3. After success, create a private generatively branded `PageWritePermit`.
4. Pass the dirty page and permit to `PageStore::write_page`.
5. Construct `CleanPage` only after the store reports durable success.

Safe downstream code cannot construct or widen the permit, directly construct a
clean page, or convert an indeterminate write back into dirty state. The permit
is consumed by one write attempt.

## Failure and Trust Boundaries

Foreign-lineage rejection and WAL failure occur before the page store is
called. Both retain the unchanged `DirtyPage`; a caller may retry the
idempotent WAL flush. The original WAL error is preserved.

Any page-store error occurs after WAL success and returns the original store
cause with `IndeterminatePageWrite`. It offers inspection but no direct retry or
conversion to dirty or clean state. A later recovery design must resolve the
physical effect before another write is authorized.

The domain proves staging and call order for safe composition; it does not prove
an arbitrary adapter honest. `LogDurability` may report success only after the
requested prefix is durable. `PageStore` may report success only after the page
write is durable and must honor the supplied page and permit. Violating either
port contract is an adapter defect outside this typestate proof.

## Evidence Boundary

These are ntsql-internal identities and bytes. They make no claim about SQL
Server page IDs, LSN values, page sizes, headers, checksums, allocation maps,
buffer management, checkpoint behavior, redo/undo, file formats, diagnostics,
or crash-recovery compatibility. No external observation or proprietary format
was consulted.

## Test Boundaries

- Runtime tests record calls and prove exact `flush -> write -> clean` order.
- Foreign log and store lineages prove rejection before either port call.
- WAL failure proves no store call and preserves the retryable dirty page and
  exact cause.
- Store failure proves WAL already succeeded and produces terminal
  indeterminate state with the exact cause.
- Construction tests prove nonzero page numbers, nonempty fixed images, and
  lineage-bound page identity.
- Compile-fail tests reject direct dirty-page, permit, and clean-page
  construction, permit widening, write calls without a permit, and
  indeterminate-to-dirty bypass.
- Architecture tests reject every page dependency except `ntsql-wal` and reject
  the reverse `ntsql-wal -> ntsql-page` edge.

## Consequences

Future page adapters can implement storage without owning write-ahead policy,
and later buffer or checkpoint components can consume explicit staged outcomes.
Store ambiguity cannot be silently retried. Persistent page bytes, page
recovery, eviction, checkpoints, and redo/undo remain separate decisions.
