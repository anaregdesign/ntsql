# ADR 0011: Lineage-Bound Runtime Log Positions

- Status: Accepted
- Date: 2026-08-05
- Issue: #72
- Extends: ADR 0001, ADR 0005, ADR 0007, ADR 0008, ADR 0009, ADR 0010
- Extended by: ADR 0012

## Context

`LogLineage` prevented a transaction coordinator from committing through an
independent log, but `LogSequenceNumber` remained a freely constructed `Copy`
wrapper around `u64`. Independent logs could both assign numeric position one,
and a direct caller could pass one log's value to another log. The memory adapter
would accept it whenever the same numeric value existed locally.

Recovery paired its lookup result with a lineage, but a `Found` result could
still carry an unrelated raw number. These gaps did not affect the coordinator's
normal append-then-flush call sequence, but they made the public internal ports
fail open to accidental cross-log substitution.

## Decision

`LogSequenceNumber` now owns:

- a cloned opaque `LogLineage` capability; and
- the adapter-assigned numeric position.

It is cloneable but not `Copy`, has no public raw numeric constructor, and is
created through `LogLineage::position`. Read-only numeric and lineage accessors
remain available for adapter bookkeeping. Equality requires both the same
lineage capability and the same numeric value.

`CommitLog::append_commit` returns this lineage-bound value and
`flush_through` borrows it. The durability fence snapshots `log.lineage()` before
append and rejects either condition before flush:

1. append returns a position from a different lineage; or
2. the log reports a different lineage after append.

The second check prevents an adapter from rotating lineage during the mutable
append call and returning a position that only matches its new state. Both cases
become `CommitError::ForeignAppendPosition`; no flush or durable callback occurs.
An arbitrary adapter remains trusted not to lie about physical effects before
returning the error.

`CommittedTransaction`, flush errors, durable acknowledgements, memory record
snapshots, and recovery `Found` results retain the complete position. Resolution
requires the atomically returned source lineage to match both the indeterminate
token and the found position before changing lifecycle state.

## Deterministic Memory Adapter

The memory adapter constructs positions only from its owned lineage. Its flush
path compares lineage before numeric record lookup, idempotency checks, fault
consumption, or durable-prefix mutation. Consequently, independent logs may each
assign numeric position one while their complete positions remain unequal and
non-interchangeable.

Restart retains the same runtime lineage and position high-water mark, so copied
positions from the pre-restart log remain valid only for retained records and
cannot alias newly assigned numeric values.

## Trust and Persistence Boundary

ADR 0012 extends `LogLineage` with an adapter-supplied persistent identity while
retaining ephemeral `Arc` identity. Code holding either lineage capability can
deliberately construct arbitrary numeric positions for that lineage, and a
malicious adapter can violate its port contract. Branding prevents accidental
foreign-lineage substitution and makes validation explicit; it is not an
authorization or cryptographic mechanism.

No lineage ID or branded position has a persistent byte representation yet. A
filesystem adapter must define, persist, validate, and recover its stable ID
before translating stored numeric positions into these runtime capabilities.

## Compatibility Boundary

Lineages and positions are ntsql-internal values. They define no SQL Server LSN,
ordering, file format, diagnostic, commit point, or recovery outcome. This change
does not alter any compatibility feature status or client-visible behavior.

## Test Boundaries

- Equal numeric positions from independent lineages compare unequal.
- A foreign memory position is rejected before fault consumption or durability
  mutation, while the local position still reaches the armed fault.
- A commit-log fake returning a foreign position never receives flush and never
  enters the durable callback.
- A fake rotating lineage during append is rejected at the same boundary.
- Append/flush errors and committed state preserve the complete position.
- Recovery rejects a found position carrying another lineage and retains the
  indeterminate token and lifecycle.
- Compile-fail tests reject direct raw position construction and implicit copy.
- Existing commit, recovery, restart, exhaustion, duplicate, volatile, and fault
  tests remain valid.

## Consequences

Current WAL, transaction, recovery, and memory ports no longer treat a bare
number as a transferable log position. Persistent lineage encoding, WAL record
formats, filesystem barriers, checkpoints, redo/undo, and SQL Server-visible LSN
semantics remain later Issue #9 work.
