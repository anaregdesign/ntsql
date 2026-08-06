# ADR 0038: Non-Authorizing Restart Checkpoint Baseline

- Status: Accepted
- Date: 2026-08-06
- Issue: #128
- Extends: ADR 0034, ADR 0037
- Extended by: ADR 0039, ADR 0048

## Context

ADR 0034 reconstructs an exact immutable transaction table from one complete
durable logical WAL prefix. ADR 0037 retains that result inside the only
reviewed live storage owner after committed-page recovery and restart analysis
both succeed.

The analysis is not yet a stable persistable baseline. Its `LogLineage` may be
an ephemeral runtime capability, its positions retain runtime lineage branding,
and its owned-page counts use platform-width `usize`. A persistence adapter
must not invent a durable identity for an ephemeral analysis or silently choose
a persistent width.

The next boundary is a lossless adapter-neutral projection prepared from the
final analyzed owner. It is deliberately called a restart checkpoint
**baseline**, not a complete checkpoint. It contains only transaction restart
metadata. It has no dirty-page table, replay start, encoded bytes, atomic
publication evidence, or recovery authority.

## Crate and Dependency Boundary

Only `ntsql-transaction` owns the baseline, projection, and typed errors. Its
reviewed dependency direction remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal
```

The existing `ntsql-wal` dependency supplies `PersistentLogId`. No crate,
dependency edge, architecture registration, adapter port, format, file, frame,
checksum, marker, synchronization point, repair rule, or poison rule changes.
Domain crates remain I/O-free.

## Owning Preparation Gate

`RestartAnalyzedTransactionPageStorage::prepare_restart_checkpoint_baseline`
borrows the final owner immutably and projects its exact stored
`DurableTransactionRestartAnalysis`.

No public constructor or free projection operation is added. The shared
projection helper remains private to avoid duplicating field conversion while
keeping detached analyses outside this preparation API. Private final-owner and
baseline fields prevent callers from substituting independent evidence or
forging a successful preparation result.

This is a scope gate over inert data, not a new runtime capability. The owner
and both adapters remain available for live operations after preparation. The
baseline neither consumes nor mutates them.

## Persistent Lineage Requirement

Preparation first asks the analyzed `LogLineage` for its exact
`PersistentLogId`. An ephemeral lineage fails with
`PersistentLineageRequired` before transaction-vector allocation. The
projection does not generate, hash, infer, or default an identity.

The baseline exposes the persistent ID as data but exposes no `LogLineage` or
`LogSequenceNumber`. A trusted outer adapter can deliberately reconstruct a
persistent lineage from the public WAL primitives; this ADR does not claim
that reconstruction is impossible. The narrower API prevents implicit
conversion and keeps lineage-bearing runtime values out of the baseline
contract.

## Lossless Numeric Projection

`DurableTransactionRestartCheckpointBaseline` owns:

- the exact `PersistentLogId`;
- the optional durable frontier as `Option<u64>`; and
- transaction entries in the analysis' strict persisted-identity order.

Each `DurableTransactionRestartCheckpointBaselineEntry` owns:

- the exact existing `DurableTransactionIdentityObservation`;
- optional first and last owned-page positions as numeric `u64`;
- the exact owned-page record count as `u64`; and
- `Uncommitted` or `Committed { commit_position: u64 }`.

One top-level persistent ID plus numeric positions is lossless because ADR
0034 validates the frontier lineage before numeric comparison and rejects every
foreign observation lineage before constructing the analysis. A future change
that admits mixed-lineage analyses must revisit this projection rather than
silently attributing all positions to one ID.

The existing identity observation is reused rather than introducing duplicate
epoch and sequence fields. It is already immutable persisted data and cannot
become a coordinator lifecycle token.

`u64` is the stable in-memory width for a future adapter encoding. Preparation
uses checked conversion from `usize`. `OwnedPageCountWidthExceeded` preserves
the identity and rejected count if a target wider than 64-bit `usize` is ever
supported; the branch is defensive on current supported targets. This decision
defines no byte order, field encoding, or on-disk compatibility.

## Frontier and Empty-Prefix Meaning

The optional numeric frontier is projected independently from the transaction
table:

- `None` means ADR 0034 analyzed zero durable logical observations;
- `Some` retains the exact nonzero final logical-record position; and
- the transaction table may still be empty when a nonempty prefix contains
  only raw page records.

The baseline therefore never infers prefix emptiness from transaction count.
It does not retain raw page records themselves. Their contribution is the
validated global order and exact frontier established by ADR 0034.

## Allocation and Error Boundary

After persistent-lineage validation, preparation fallibly reserves exact
capacity for the already analyzed transaction count. Failure returns
`TransactionCapacityExhausted` with the required count. Entry conversion then
uses the existing reserved capacity and checked count-width conversion.

No partial baseline escapes any failure. There is no infallible allocation,
panic, success-shaped fallback, retry, or source/store access. The persistent
lineage error intentionally takes priority over allocation because an
ephemeral analysis can never form this baseline regardless of capacity.

## Point-in-Time Currency

The baseline copies the immutable startup analysis. Once returned, later live
WAL appends, durability fences, page writes, or store changes do not update it.
Its frontier and entries continue to describe only the original analyzed
prefix.

Preparation can be repeated while the owner is held, but it always projects the
same stored startup analysis. It does not re-read the current source or claim
that the earlier frontier remains the durable tail.

## Authority Boundary

The baseline, entries, states, persistent ID, and numeric positions cannot
directly create or satisfy:

- `TransactionId`, active/committed transaction state, or a coordinator;
- a dirty, clean, live-permitted, or recovery-permitted page;
- `LogLineage`, `LogSequenceNumber`, WAL append, or durability-fence input;
- a stable-prefix source, recovered storage owner, or live adapter pair;
- encoded checkpoint bytes or proof that a checkpoint was published;
- redo, undo, rollback, abort, compensation, or replay;
- a dirty-page table, replay start, retention floor, truncation, or log
  reclamation.

The type name's `CheckpointBaseline` suffix is load-bearing. This value is a
candidate input for later separately reviewed checkpoint storage, not a
complete checkpoint or checkpoint-validity proof.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, WAL, restart-analysis, and
storage-ownership contracts. No external product documentation, driver, SDK,
fixture, oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format
is consulted.

This decision defines no SQL Server checkpoint, transaction table, dirty-page
table, LSN, MinLSN, recovery phase, persistent format, error, diagnostic, or
compatibility behavior.

## Test Boundaries

- Persistent empty and raw-page-only prefixes remain distinct.
- Interleaved committed, uncommitted, and commit-only identities preserve exact
  ordering, ranges, counts, states, and commit positions.
- An ephemeral final owner returns the exact typed lineage error.
- A real `try_reserve_exact(usize::MAX)` path returns the capacity error without
  fabricating the variant.
- Memory integration prepares through the final owner, proves exact persistent
  identity and entries, advances the live WAL, and retains the old baseline.
- Compile-fail tests reject direct construction, preparation from a detached
  analysis, runtime lineage/position extraction, lifecycle/page conversion, and
  log-durability use.
- Existing analysis, recovery, ownership, adapter, format, architecture, and
  governance tests remain valid.

## Non-Goals

This ADR does not:

- persist, encode, decode, publish, reopen, validate, repair, or select a
  checkpoint;
- add a memory or filesystem checkpoint store;
- add a dirty-page table, page image, page index, replay start, redo/undo plan,
  transaction coordinator, or active transaction restoration;
- add a generation, timestamp, digest, checksum, marker, synchronization point,
  temporary file, rename, or corruption response;
- choose a retention floor, truncate, compact, or reclaim a log;
- make the startup analysis current after live operations;
- change WAL or page-store bytes, version eligibility, locks, faults, or open
  order; or
- define external SQL Server values or native file compatibility.

## Consequences

The final analyzed storage owner can now produce one immutable, portable-width,
persistent-lineage-bound transaction restart baseline without I/O or new
authority.

A later adapter can define durable storage and decoded validation around this
shape only through a separate issue and ADR. A complete recovery checkpoint
still requires independently reviewed dirty-page and replay boundaries before
it can bound recovery or log retention.
