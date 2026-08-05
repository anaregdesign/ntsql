# ADR 0024: Adapter Projection of Durable Transaction-Page Evidence

- Status: Accepted
- Date: 2026-08-06
- Issue: #100
- Extends: ADR 0019, ADR 0021, ADR 0022, ADR 0023
- Extended by: ADR 0025

## Context

ADR 0023 defines adapter-neutral observations for persisted transaction identity,
transaction-owned full-image pages, and durable commits. Its allocation-free
classifier can distinguish one owned page with exactly one later matching commit
from an owned page with no matching commit in a complete durable prefix.

The memory and filesystem adapters already expose validated record snapshots,
exact lineage-bound positions, and explicit durable-prefix iterators. They do
not yet project their owner and commit fields into the ADR 0023 observations.
Callers would otherwise need to duplicate validation or reconstruct positions
from numeric values, weakening the adapter-neutral boundary.

The smallest next step is raw-field domain construction plus allocation-free
record-level projection in both adapters, followed by restart and reopen tests
over each adapter's explicit durable prefix. Projection remains observational
and grants no lifecycle or mutation authority.

## Crate and Dependency Boundary

No crate or dependency edge changes:

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

Raw validation remains in the domain observation types. Each outer adapter only
copies fields from its own immutable record snapshot. Domain crates import no
adapter types, and the existing architecture allow-list and negative dependency
tests remain unchanged.

## Raw Owned-Page Construction

`DurableTransactionPageObservation::from_bytes` accepts:

- one raw owner epoch;
- one raw owner sequence;
- one nonzero `PageNumber`;
- one `PageVersion`;
- one exact `[u8; N]`;
- one lineage-bound `LogSequenceNumber`.

Owner fields are validated first through the ADR 0023 identity rules. Page
fields are then validated through ADR 0019's existing raw page-observation
rules. Success produces the same value as pairing a validated identity and
`DurablePageWalObservation<N>` through the existing constructor.

`DurableTransactionPageObservationBytesError<N>` retains every supplied owner
field, page field, byte, and position regardless of which validation fails. Its
reason distinguishes:

- `Identity(ZeroEpoch | ZeroSequence)`; and
- `Page(ZeroPageWidth | ZeroPosition)`.

The flat retained-input shape is intentional. Wrapping only the first nested
error would discard the fields that validation had not yet consumed. When page
validation fails after moving bytes and position, its existing typed error
returns those values through `into_parts` so the composite error remains
lossless.

## Raw Commit Construction

`DurableTransactionCommitObservation::from_fields` accepts raw epoch, sequence,
and one lineage-bound position. Identity validation precedes the existing
nonzero commit-position validation.

Its separate fields-error type retains all three raw inputs and distinguishes an
identity reason from `ZeroPosition`. The existing constructor from a validated
identity and its existing zero-position error do not change.

Raw constructors are public value validators, not provenance proofs. Safe code
may construct observations that did not originate from an adapter. The values
remain non-authorizing, and callers remain responsible for supplying a complete
validated durable prefix.

## Record-Kind Projection

`InMemoryLogRecord<N>` and `FileLogRecord<N>` each expose two projection
methods.

`transaction_page_recovery_observation`:

- returns `Ok(Some(...))` only for a transaction-owned page record;
- copies the exact owner epoch and sequence, page number, page version, and
  bytes;
- clones the record's existing `LogSequenceNumber`;
- returns `Ok(None)` for a commit or raw page record.

`transaction_commit_recovery_observation`:

- returns `Ok(Some(...))` only for a commit record;
- copies its exact epoch and sequence;
- clones the record's existing `LogSequenceNumber`;
- returns `Ok(None)` for both page record kinds.

No projection reconstructs a position with `lineage.position(position.get())`,
substitutes a current log lineage, reads storage, allocates a collection, or
inspects another record.

## Dual Page Projection

A transaction-owned page intentionally projects through two different methods:

- ADR 0019 `page_recovery_observation` returns the owner-free physical page view;
  and
- this ADR's `transaction_page_recovery_observation` returns the owner-aware
  transaction view.

These surfaces are not mutually exclusive record filters. Callers must choose
the view appropriate to physical reconciliation or transaction classification
and must not double-count one owned record when grouping a prefix.

Raw pages project only through the ADR 0019 surface. Commit records project only
through the commit surface. Ownership never appears through a commit-only
accessor, and commitment never appears through an owned-page accessor.

## Durable-Prefix Responsibility

Projection methods inspect one immutable record and make no durability
inference. Calling them on `records()` is structurally allowed and produces an
observation of a complete volatile record. A recovery caller must instead
iterate `durable_records()`, which is the adapter's explicit marker-covered
prefix.

The tests make this distinction load-bearing. Each adapter contains:

1. a transaction-owned page followed by its later durable commit;
2. another transaction-owned page made durable without its commit;
3. an interleaved raw page made durable; and
4. a complete matching commit in the volatile suffix.

Projecting only `durable_records()` classifies the first page committed and the
second uncommitted. Projecting all physical records would classify the second
page committed, proving that prefix selection is not cosmetic.

## Memory Restart and Persistent Reopen

The memory test uses a persistent lineage. Before restart, the complete volatile
commit remains present in `records()` but excluded by `durable_records()`, so
the exclusion is observable and changes classification.

`restart` removes the volatile suffix. `reopen` reconstructs positions under the
persistent lineage capability. Fresh projections from the reopened durable
records retain the exact page/commit positions and reproduce committed and
uncommitted classifications. Observations are not carried across reopen and no
position is rebuilt from its numeric component.

## Filesystem V3 Reopen

The filesystem test writes the same logical sequence through explicit v3
entrypoints. Durable-through markers cover the two owned pages and raw page but
not the final complete commit. After drop and reopen:

- every complete logical record remains inspectable in `records()`;
- `durable_records()` stops before the unmarked commit;
- owner and commit projections use the exact reopened positions;
- the durable prefix classifies the first page committed and the second
  uncommitted; and
- all physical records demonstrate the opposite second result if the volatile
  suffix is incorrectly included.

The test does not change v1/v2 bytes, v3 framing, scanner validation, repair, or
marker semantics.

## Authority and Evidence Boundary

Projection produces data only. It does not:

- prove that the caller selected a durable or complete prefix;
- reconstruct `TransactionId` or any coordinator lifecycle token;
- produce `CommittedTransaction`, `DirtyPage`, `TransactionDirtyPage`, or
  `PageWritePermit`;
- select a final visible page image;
- create a replay command, callback, adapter capability, or store operation.

Existing ADR 0023 compile-fail tests continue to protect the lifecycle and write
authority boundary. Existing commit-only accessors and
`TransactionRecoverySource` behavior do not change and continue to ignore page
ownership.

The implementation uses only repository-authored observations and workspace
formats. It consults no external product, driver, SDK, fixture, oracle,
proprietary governance tool, or native MDF/NDF/LDF/BAK format and makes no
external SQL Server recovery or compatibility claim.

## Test Boundaries

- Raw owned-page construction succeeds with exact fields.
- Owner zero-epoch and zero-sequence errors retain all owner and page inputs.
- Page zero-width and zero-position errors retain all owner and page inputs.
- Raw commit construction succeeds with exact fields.
- Commit identity and zero-position errors retain exact fields and lineage.
- Memory and file record-kind tests prove owned-page, commit, and raw-page
  projections are distinct while owned pages retain their ADR 0019 projection.
- Adapter projections clone the source record position and preserve its lineage.
- A volatile matching commit changes the result only when callers intentionally
  project all physical records.
- Memory restart/reopen and filesystem v3 reopen reproduce committed and
  uncommitted classifications from their durable prefixes.
- Existing authoritative commit lookup still ignores owned-page records.
- Existing compile-fail tests keep observations and classifications
  non-authorizing.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- add an allocating whole-prefix projection or grouping API;
- select one latest committed image among repeated page records;
- combine transaction classification with page-store reconciliation;
- create recovery authority or mutate a page store;
- define replay, redo, undo, rollback, abort, compensation, checkpoints,
  transaction tables, dirty-page tables, or idempotence;
- remove raw page APIs or define stored raw/uncommitted-page policy;
- define read visibility, isolation, locking, buffering, eviction, or
  force-at-commit; or
- define external SQL Server values or native file-format behavior.

## Consequences

The memory and filesystem adapters can now project exact owner and commit
evidence from their explicit durable prefixes into ADR 0023 and reproduce
committed/uncommitted classification across restart and reopen without
reconstructing lifecycle authority.

The next domain slice may define whole-prefix per-page selection over committed
owned records and reconcile that selection with physical page-store evidence.
Mutation remains blocked on explicit repeated-record ordering, idempotence,
stored raw/uncommitted state, and a separately reviewed recovery-only write
gate.
