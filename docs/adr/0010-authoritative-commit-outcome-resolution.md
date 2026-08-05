# ADR 0010: Authoritative Commit Outcome Resolution

- Status: Accepted
- Date: 2026-08-05
- Issue: #68
- Extends: ADR 0001, ADR 0005, ADR 0007, ADR 0008, ADR 0009
- Extended by: ADR 0011, ADR 0012

## Context

An append or flush error consumes active transaction state and returns an
`IndeterminateTransaction`. The deterministic memory adapter shows why the
original error is not a verdict: a matching record may be absent, volatile, or
already durable. A later flush may make a volatile record durable, while restart
may discard it.

ADR 0009 makes complete transaction identity stable across coordinator
lifetimes in one log lineage. The remaining local boundary is to reconcile that
identity with authoritative durable-record evidence without creating a retry,
rollback, or client-visible recovery rule.

## Decision

`ntsql-transaction` owns the I/O-free `TransactionRecoverySource` port. One
lookup takes the complete epoch-qualified `TransactionId` and atomically returns:

- the `LogLineage` that was searched; and
- either exactly one durable internal position or authoritative absence.

`Absent` is valid only when the source has completely and conclusively searched
the matching lineage for that attempt. A partial, corrupt, duplicated, unstable,
or otherwise uncertain view must return an error. The port is trusted: safe Rust
cannot prove that an arbitrary adapter searched complete durable state or
reported it honestly.

`IndeterminateTransaction` retains the private coordinator runtime brand and the
expected log lineage in addition to its transaction identity. Resolution remains
coordinator-owned and follows this order:

1. Reject a foreign runtime brand.
2. Reject a lifecycle other than `Indeterminate`.
3. Ask the injected source for the lineage-paired durable lookup.
4. Retain the token and phase on source failure or a different lineage.
5. Only then change the registry to `Committed` or
   `NoDurableCommitRecord` and construct the corresponding terminal value.

The source result is adapter data, not independently sufficient proof. Safe
downstream code cannot directly construct either terminal transaction type, and
no failed or absent lookup recreates `ActiveTransaction`.

## Deterministic Memory Adapter

`ntsql-storage-memory` implements the recovery port by searching physical record
snapshots for the complete `TransactionId`:

- one match inside the durable prefix returns its exact position;
- no physical match returns authoritative absence;
- one matching volatile record returns a typed error; and
- duplicate matches anywhere return a typed error.

A later flush can move a matching volatile record into the durable prefix.
`restart` instead discards the volatile suffix while preserving lineage and
allocator high-water marks, after which the same identity is authoritatively
absent. Equal coordinator-local sequences from different epochs are distinct
lookups.

## Trust and Persistence Boundary

Pairing lineage and lookup in one source operation avoids accepting a result
separately observed from a rotating source. The coordinator compares the returned
lineage with the token before accepting either presence or absence. A malicious
source can still return a false pair; this is an adapter contract violation.

ADR 0011 binds `LogSequenceNumber` to the runtime `LogLineage` and requires
recovery presence to match both the source lineage and position lineage.
ADR 0012 allows those capabilities to be reconstructed from a trusted stable ID,
but does not persist or encode it. This ADR does not reconstruct coordinator
registries after process loss, define a recovery scan cutoff for a filesystem
log, or validate production storage. Those require a persistent format design.

## Compatibility Boundary

The lookup, terminal types, lifecycle phases, record positions, and memory
restart effects are ntsql-internal. `NoDurableCommitRecord` means only that the
trusted source found no internal durable commit record. It does not mean SQL
Server rollback, retry safety, `XACT_STATE()`, `@@TRANCOUNT`, an acknowledged
commit point, or any client-visible outcome. No compatibility feature status or
diagnostic changes.

## Test Boundaries

- Before-append failure resolves to no durable record in the exact memory model.
- After-flush failure resolves to committed at the exact durable position.
- A volatile match retains the token until restart discards it or a later flush
  makes it durable.
- Duplicate records and source failures retain indeterminate state.
- Foreign coordinators and lifecycle mismatches are rejected before lookup.
- A lineage mismatch is rejected before registry mutation.
- A source that reports the expected lineage but returns a foreign-lineage
  position is also rejected before registry mutation.
- Equal local sequences from different epochs cannot alias.
- Compile-fail tests reject direct terminal-state construction and
  indeterminate commit retry.
- Existing commit ordering, fault injection, restart, and epoch tests remain
  unchanged.

## Consequences

The in-memory model can now reconcile ambiguous commit-port failures without
guessing from the original error. The resolution API remains deliberately
terminal and internal. Persistent recovered-log views, process-restored
transaction tables, checkpoint analysis, redo/undo, page state, external commit
semantics, and client diagnostics remain later transaction and storage work.
