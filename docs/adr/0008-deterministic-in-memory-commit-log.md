# ADR 0008: Deterministic In-Memory Commit Log

- Status: Accepted
- Date: 2026-08-05
- Issue: #63
- Extends: ADR 0001, ADR 0005, ADR 0006, ADR 0007
- Extended by: ADR 0009, ADR 0010, ADR 0011, ADR 0012, ADR 0013

## Context

The WAL and transaction domain crates define a commit durability fence and
fail-closed transaction lifecycle, but their focused tests use private fakes.
Recovery work needs one reusable outer adapter that can distinguish a returned
error from the physical effect that may already have occurred.

No approved behavior specification defines SQL Server WAL bytes, physical LSNs,
flush barriers, commit acknowledgement timing, or crash recovery. A filesystem
adapter would also require format, platform, and provenance decisions that are
not part of this boundary.

## Decision

Add `ntsql-storage-memory` as a standard-library-only synthetic persistence
adapter with these exact direct dependencies:

```text
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

The adapter implements `CommitLog<TransactionCommitRecord>`,
`TransactionEpochSource`, and the authoritative `TransactionRecoverySource`
added by ADR 0010. It copies only the opaque transaction identity into immutable
record snapshots, assigns nonzero strictly increasing in-memory positions and
coordinator epochs, owns one opaque runtime `LogLineage`, and tracks a durable
prefix. It never constructs transaction identity, transaction terminal state, or
a durability acknowledgement. Each assigned position is created through the
adapter's lineage capability.

One fault may be armed at a time. It is consumed exactly once when its matching
stage is reached:

- `BeforeAppend` reports append failure without adding a record;
- `AfterAppend` adds a volatile record and then reports append failure;
- `BeforeFlush` reports flush failure without advancing durability; and
- `AfterFlush` advances the durable prefix and then reports flush failure.

An invalid or foreign-lineage flush position is rejected before a fault is
consumed. Flushing a known position already inside the durable prefix is an
idempotent success, because the port promises durability through at least that
position.

`restart` is a model transformation, not process or filesystem behavior. It
retains exactly the marked durable prefix, discards the volatile suffix, and
clears the transient fault. Position and coordinator-epoch allocator high-water
marks survive so copied or persisted identities cannot alias future records.
Intentional position gaps may therefore appear after restart.
The same log lineage is retained so coordinators opened before and after the
model transformation remain bound to this lineage rather than an independent
log.

ADR 0012 adds a separate `reopen` transformation for logs created with a
`PersistentLogId`. It validates that ID before mutation, discards the volatile
suffix, clears the transient fault, and reconstructs every durable position from
a fresh lineage value carrying the same ID. Epoch and position high-water marks
remain unchanged. An ephemeral log rejects reopen without discarding state.

## Ambiguity and Recovery Boundary

The adapter makes the existing ambiguity contract observable:

- an append error does not prove whether a volatile record exists;
- a flush error does not prove whether the requested prefix became durable; and
- a transaction can remain `Indeterminate` while its commit record is present in
  the durable prefix.

The recovery lookup searches the complete epoch-qualified identity. Exactly one
matching record in the durable prefix returns its position, and no physical
match returns authoritative absence. A matching record only in the volatile
suffix is an error because it may later become durable or be discarded.
Duplicate physical matches are also an error rather than a guessed verdict.

The transaction coordinator, not this adapter, consumes those results and
changes terminal state. Restart can make a volatile match authoritatively absent
by discarding it; a later exact flush can instead make that same match durable.
Neither result decides an external outcome or retry.

## Compatibility Boundary

All positions, records, faults, and restart effects are ntsql test-model values.
They define no SQL Server value, diagnostic, commit point, file format, barrier,
power-loss outcome, or compatibility status. Native MDF/NDF/LDF/BAK formats
remain blocked by the legal contract.

## Test Boundaries

- Normal coordinator commit appends once and flushes the exact assigned
  position.
- Each before/after fault proves both the returned phase-specific error and the
  corresponding physical effect.
- `AfterFlush` jointly proves a durable record and an indeterminate coordinator
  phase.
- Restart keeps only the durable prefix and never reuses a discarded position.
- Unknown positions do not consume a pending fault, while repeated valid flushes
  are idempotent.
- Equal numeric positions from independent logs are unequal, and a foreign one
  is rejected before record lookup, fault consumption, or durability mutation.
- Persistent reopen preserves durable records and allocator high-water marks;
  ephemeral reopen fails before changing records or faults.
- Position exhaustion and attempts to replace an armed fault return typed
  errors.
- Resolution distinguishes the four fault boundaries, retains an indeterminate
  token for volatile or duplicate records, and uses the complete transaction
  identity rather than its coordinator-local sequence.
- Architecture tests allow exactly the two inward dependencies and reject
  reverse adapter dependencies.

## Consequences

Subsequent recovery tests have a deterministic shared adapter instead of
reimplementing ambiguous fakes. The model is intentionally synchronous and
in-memory. Its authoritative lookup is only a model of durable-record presence;
ADR 0013 separately persists lineages, epoch allocation, commit records, and the
durable frontier. Page WAL, checkpoints, redo/undo, group commit, and
client-visible outcomes remain later Issue #9 work.
