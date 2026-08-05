# ADR 0005: Write-Ahead Commit Durability Fence

- Status: Accepted
- Date: 2026-08-05
- Issue: #50
- Extends: ADR 0001
- Extended by: ADR 0006, ADR 0007, ADR 0008, ADR 0009, ADR 0010, ADR 0011,
  ADR 0012, ADR 0013

## Context

A transaction coordinator must not acknowledge a commit before its commit record
is durable. Filesystem APIs, WAL bytes, transaction identity, checkpoints, and
recovery algorithms are not yet owned by concrete components, and choosing them
in the first change would mix several responsibilities. Native SQL Server file
and log formats also remain outside the authorized scope.

The first storage behavior can nevertheless make one independent safety
property explicit: append a caller-owned commit record, flush the log through
the exact returned position, and make acknowledgement construction impossible
until both operations succeed.

## Decision

`ntsql-wal` is an I/O-free domain crate with no direct dependencies. It owns:

- `LogSequenceNumber`, an opaque ntsql-internal adapter-assigned position bound
  to one runtime lineage, with no SQL Server, wire, or persistent byte
  representation;
- `LogLineage`, an opaque identity shared by ports for one logical log, using
  either ephemeral runtime pointer identity or an adapter-supplied persistent
  ID;
- `CommitLog<Record>`, the inward port for appending a caller-owned record and
  flushing through a position, together with its lineage identity;
- `CommitError`, which distinguishes append failure from flush failure and
  retains the unacknowledged position on the latter; and
- `CommitAcknowledgement`, whose private, generatively branded value exists only
  in a callback reached after append and exact-position flush both report
  success.

The port is synchronous for this deterministic domain boundary. That does not
require a persistence adapter to block an async runtime thread: a future outer
composition layer may run the complete synchronous fence on a blocking worker.
`flush_through` may return success only after durable completion; merely
enqueueing, scheduling, or batching future I/O is not success. No fallback
treats an append or flush failure as success.

ADR 0009 uses `LogLineage` to reject a transaction coordinator paired with a
different log before append. ADR 0011 additionally makes every
`LogSequenceNumber` carry that runtime capability. Positions are non-`Copy`,
created through `LogLineage::position`, and equal only when both lineage and
numeric value match.

ADR 0010 also requires an authoritative recovery lookup to return its lineage
with the lookup result in one operation. The transaction coordinator accepts a
recovered position only after the source lineage, position lineage, and
indeterminate token lineage all match.

The durability fence snapshots the log lineage before append. It rejects both a
position from another lineage and a log whose lineage changes during append,
before calling flush or constructing an acknowledgement. This is fail-closed
adapter validation, not proof that an arbitrary adapter is honest.

ADR 0012 adds `PersistentLogId` so a later storage runtime can reconstruct the
same lineage capability. The WAL domain neither allocates nor encodes this ID;
the outer adapter must durably store it and prevent reuse across independent
logs.

ADR 0013 adds the first concrete adapter. A successful file-log flush uses one
barrier for the complete commit prefix followed by a checksummed durable-through
marker and a second barrier. The marker is the recovered durable frontier; no
adapter success is reported before both barriers complete.

The intended dependency direction is:

```text
ntsql-storage-file ------> ntsql-wal <------ ntsql-storage-memory
                               ^
                               |
                       ntsql-transaction

ntsql-wal --------------> standard library only
```

The transaction component owns its commit-record type and passes it to a generic
`CommitLog<Record>` implementation. Concrete persistence adapters depend on
`ntsql-wal` to implement that port. `ntsql-wal` never depends on
transaction, filesystem, protocol, contract, serialization, or adapter crates.

## Evidence Boundary

The feature inventory record `storage-recovery.write-ahead-commit` remains
`not-tested`. It names future externally observable durability/recovery work but
does not claim a SQL Server commit point, LSN value, crash outcome, diagnostic,
or compatibility status. The repository-authored call-order tests establish
only this internal safety invariant.

Page formats, checkpoints, redo/undo, group commit, broader transaction
lifecycle, and externally observable crash-recovery outcomes require their own
behavior, format, provenance, and fault-injection decisions. ADR 0013 defines
only the ntsql-internal transaction commit-log format and standard-library file
barriers.

## Test Boundaries

- An in-memory fake records every port call and proves success is exactly
  append followed by flush of the returned position.
- Append failure proves no flush occurs.
- A foreign append position or append-time lineage rotation proves no flush or
  acknowledgement occurs.
- Append and flush failure prove the branded callback is not entered. Append
  failure does not assume whether the adapter changed physical state.
- Flush failure preserves the exact appended position and original cause.
- Compile-fail doctests prove safe downstream code cannot construct, clone, or
  return an acknowledgement from its generative attempt callback.
- Architecture tests reject any direct dependency from `ntsql-wal`.
- Transaction tests reject a mismatched log lineage before either port method is
  called and retain the active token.
- Recovery tests reject a durable-record lookup from another lineage and retain
  indeterminate state without accepting its position or absence result.
- Compile-fail tests reject raw position construction and implicit position
  copying.

## Consequences

Future transaction code can require a durable acknowledgement rather than a
boolean or convention. Persistence choices remain outside the domain and may be
tested with deterministic fault injection later. This fence alone is not a
complete WAL, transaction, or recovery implementation and supports no external
compatibility claim.
