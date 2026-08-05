# ADR 0012: Adapter-Stable Log Lineage Identities

- Status: Accepted
- Date: 2026-08-05
- Issue: #74
- Extends: ADR 0001, ADR 0005, ADR 0008, ADR 0009, ADR 0010, ADR 0011
- Extended by: ADR 0013, ADR 0015, ADR 0016, ADR 0017

## Context

ADR 0011 binds every runtime log position to `LogLineage`, but the only lineage
authority was an `Arc` pointer. That identity survives cloning and the in-memory
`restart` transformation, but cannot be reconstructed after a process loses all
runtime pointers. A future filesystem adapter therefore needs a stable value to
recover before it can reconstruct branded positions or epoch authority.

The WAL domain must not choose randomness, read a clock, allocate global IDs, or
define a storage encoding. Those responsibilities belong to the outer adapter
that owns persistence and log creation.

## Decision

`ntsql-wal` owns `PersistentLogId`, an opaque nonzero `u128` value. Construction
is explicit and caller-supplied; zero returns `None`, fields remain private, and
the numeric value is exposed read-only for adapter bookkeeping.

`LogLineage` supports two identity modes:

- ephemeral lineages compare only by `Arc` pointer identity; and
- persistent lineages compare by `PersistentLogId`.

Two independently constructed `LogLineage::persistent` values carrying the same
ID identify the same logical log. Consequently, positions reconstructed with the
same ID and numeric value compare equal across storage runtimes. Different IDs,
different ephemeral pointers, and mixed persistent/ephemeral values do not
match.

The outer adapter is responsible for allocating, durably storing, and never
reusing the ID for an independent log. The domain does not attempt to detect an
adapter that opens unrelated storage with the same ID.

## Deterministic Memory Reopen Model

`InMemoryCommitLog::with_persistent_lineage` creates a model log with an injected
ID. Its mutable `reopen` transformation:

1. rejects an ephemeral lineage before mutation;
2. reconstructs a lineage capability from the persistent ID;
3. retains only the durable record prefix;
4. reconstructs each retained position from the new capability;
5. clears the transient armed fault; and
6. preserves position and coordinator-epoch allocator high-water marks.

The existing consuming `restart` transformation remains available for ephemeral
fault tests and preserves its original runtime lineage directly. `reopen` models
only loss and reconstruction of storage runtime identity; it does not model an
operating-system process, file descriptor, or transaction coordinator process.

## Trust and Persistence Boundary

`PersistentLogId` is an identity contract, not a generator, file format,
authorization token, checksum, or secret. Reusing one ID for independent logs
causes them to compare as the same lineage and is a trusted adapter violation.
The nonzero `u128` space reduces no such contractual responsibility.

This ADR defines no endianness, byte width on disk, header, version, checksum,
atomic creation, directory flush, or migration behavior. ADR 0013 makes those
choices for the first ntsql-owned transaction commit-log format before its
adapter reconstructs this capability.

## Compatibility Boundary

Persistent IDs, lineages, positions, and memory reopen effects are
ntsql-internal. They define no SQL Server database ID, LSN, file header, recovery
behavior, diagnostic, or compatibility status.

## Test Boundaries

- Zero IDs are rejected and private fields prevent direct construction.
- Separately reconstructed lineages and positions match for one persistent ID.
- Different persistent IDs and ephemeral identities do not match.
- Persistent memory reopen drops a volatile suffix, clears faults, preserves
  durable positions, retains epoch/position high-water marks, and accepts a
  coordinator bound before reopen.
- Ephemeral reopen preserves records and faults while returning a typed error.
- Existing foreign-lineage, commit, recovery, restart, exhaustion, duplicate,
  volatile, and fault tests remain valid.

## Consequences

The domain now has the stable identity primitive required by a later filesystem
format without taking ownership of generation or I/O. ADR 0013 owns the first
persistent encoding, durable epoch allocation, commit framing, checksums,
barriers, and recovered durable frontier. Page WAL, checkpoints, and redo/undo
remain Issue #9 work.
