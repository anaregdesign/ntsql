# ADR 0009: Recovery-Stable Transaction Coordinator Epochs

- Status: Accepted
- Date: 2026-08-05
- Issue: #66
- Extends: ADR 0001, ADR 0007, ADR 0008
- Extended by: ADR 0010

## Context

ADR 0007 prevents active-token replay inside one coordinator runtime, but every
new coordinator begins its numeric transaction sequence at one. Two coordinators
using the same persistence adapter could therefore append indistinguishable
transaction identities. A later recovery scan could not safely match an
indeterminate attempt to its durable commit record.

Randomness, clocks, global counters, and invented SQL Server transaction values
are not acceptable identity authorities. The outer persistence lineage already
owns the state that must survive model restart.

## Decision

`ntsql-transaction` defines an I/O-free `TransactionEpochSource` port. One call
atomically returns a `NonZeroU64` and the `LogLineage` in which it is unique. The
source is contractually responsible for never reissuing that value within the
lineage. `TransactionCoordinator::open` privately wraps the value as
`TransactionEpoch` and retains the paired lineage.

`TransactionCoordinator::new` and `Default` are removed. Safe downstream code
cannot directly construct `TransactionEpoch`, `TransactionId`, or a coordinator
that bypasses the source. A source implementation remains a trusted port: safe
Rust cannot prove that a malicious or broken adapter honors persistent
uniqueness.

`TransactionId` is the ordered pair of:

- the source-assigned coordinator epoch; and
- a nonzero monotonic sequence issued once within that coordinator.

Both values are available read-only for persistence bookkeeping. Construction
remains private to the transaction crate. The private `Arc` runtime identity is
retained independently; a persisted epoch does not authorize a live token to
cross coordinators.

The WAL port also exposes an opaque runtime `LogLineage`. Returning the epoch and
lineage from one allocation call prevents a source rotation from pairing an
epoch with the wrong log. Commit rejects a different lineage before registry
mutation or WAL calls and returns the still-active token. This prevents a
coordinator opened from independent log A from writing an epoch into log B where
it could collide with B's allocator.

`ntsql-storage-memory` implements the epoch-source port. Epochs start at one,
increase without wrap, and fail with a typed sticky exhaustion error after
`u64::MAX`. Model restart preserves the allocator high-water mark.

## Trust and Lineage Boundary

The guarantee is lineage-local. Two coordinators opened from the same conforming
source have distinct complete transaction identities even when each sequence is
one. A newly created independent log is a separate lineage and may start its
epochs at one.

`LogSequenceNumber` remains an unbranded adapter value with a public numeric
constructor. The coordinator checks the log lineage at commit, but direct raw
positions still must not be compared or substituted across logs. Persistent log
branding, filesystem-persisted epoch state, and validation of a production epoch
source remain explicit follow-up work.

## Compatibility Boundary

Epochs and sequences are ntsql-internal recovery keys. They define no SQL Server
transaction ID, LSN, session value, diagnostic, retry rule, commit point, or
crash outcome. No behavior feature or compatibility status changes.

## Test Boundaries

- Two coordinators opened from one memory log receive different epochs and each
  starts at sequence one.
- Their persisted record snapshots retain different complete transaction IDs.
- Restart preserves the next epoch and sticky exhaustion.
- Source failures return without constructing a coordinator.
- A foreign log lineage is rejected before append and retains the active token.
- Direct epoch, identity, and source-bypassing coordinator construction fail to
  compile.
- Existing runtime ownership, exact-flush commit, and indeterminate failure tests
  continue unchanged except for source-backed setup.

## Consequences

Durable records can distinguish coordinator lifetimes within one persistence
lineage. ADR 0010 uses that identity to resolve an `IndeterminateTransaction`
from authoritative durable-record evidence. It retains the token on lookup
failure and treats authoritative absence as a terminal internal state, not as
rollback, retry permission, or a client-visible outcome.
