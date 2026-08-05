# ADR 0007: Replay-Safe In-Process Transaction Coordination

- Status: Accepted
- Date: 2026-08-05
- Issue: #59
- Extends: ADR 0001, ADR 0005, ADR 0006
- Extended by: ADR 0009, ADR 0010, ADR 0011, ADR 0020

## Context

ADR 0006 consumes each active transaction value once and makes a failed WAL
attempt indeterminate. Its temporary public constructors still allowed safe
downstream code to create the same numeric transaction identity more than once
or reconstruct active state after an attempt. Typestate on one value cannot
prevent that coordinator-level replay.

No approved behavior specification defines SQL Server-visible transaction
identity, retry, rollback, recovery, or session state. The next boundary is
therefore limited to deterministic in-process issuance and attempt tracking.

## Decision

`ntsql-transaction` owns `TransactionCoordinator` in the existing domain crate.
The coordinator:

- issues monotonically increasing, nonzero `TransactionId` values and fails
  explicitly after issuing `u64::MAX`;
- is neither cloneable nor copyable;
- gives every instance a private `Arc<()>` runtime identity without randomness,
  clocks, I/O, or global state;
- returns non-cloneable `ActiveTransaction` tokens carrying that private
  identity;
- rejects a token from another coordinator before calling the WAL port and
  returns the still-active token for routing to its owner;
- records `CommitAttempted` before invoking `CommitLog`, then records only the
  phase `Committed` or `Indeterminate`; and
- retains every issued identity for its in-process lifetime so it cannot wrap,
  reuse, or reconstruct terminal state.

`TransactionId` and `ActiveTransaction` no longer have public constructors.
`ActiveTransaction` no longer exposes `commit`; the coordinator is the only
public commit entry point. The existing `commit_durability` callback remains the
sole constructor path for `CommittedTransaction`.

The read-only lifecycle registry exposes only phase. It does not expose the
durable log position, make a recovery decision, or substitute for
`CommittedTransaction`. Registry retention is intentionally unbounded in this
pre-product in-process model. A bounded or persistent registry requires a
separate owned recovery design and must fail closed if an expected entry is
absent.

ADR 0010 adds coordinator-owned resolution for the existing indeterminate phase.
It checks the private runtime brand and retained phase before querying recovery,
then accepts only an atomically lineage-paired authoritative result. Presence
changes the phase to committed; authoritative absence changes it to
`NoDurableCommitRecord`. Source failure, foreign lineage, and other rejection
paths retain both the token and indeterminate phase. Neither terminal phase can
be reconstructed as active.

ADR 0011 makes the log position stored by `CommittedTransaction` lineage-bound
and non-`Copy`. Recovery presence is accepted only when its position carries the
same lineage atomically reported by the source and retained by the token.

## Dependency Direction

The direct dependency graph is unchanged:

```text
ntsql-transaction -> ntsql-wal -> standard library
```

The coordinator belongs to transaction-domain policy. A future persistence
adapter implements `CommitLog<TransactionCommitRecord>` outside both domain
crates and depends inward on their public contracts.

## Compatibility Boundary

The feature `transactions-concurrency.commit-lifecycle` remains `not-tested`.
Numeric IDs, runtime coordinator identity, lifecycle phases, and log positions
are ntsql-internal values. They define no SQL Server transaction count, state,
diagnostic, retry rule, commit point, or crash outcome.

ADR 0009 qualifies each coordinator-local sequence with an injected
persistence-lineage epoch. Multiple coordinators may still issue the same
sequence value, but a conforming epoch source makes their complete
`TransactionId` values distinct within one lineage. Active tokens additionally
cannot cross the private runtime identity boundary. Independent persistence
lineages may reuse epoch values; global uniqueness is not claimed.

## Test Boundaries

- Focused tests issue multiple coexisting active tokens with distinct nonzero
  IDs and prove terminal exhaustion without wrap.
- A foreign coordinator performs no append or flush and returns the token, which
  still commits through its issuing coordinator.
- Success and both WAL failure phases preserve identity, call order, exact
  durable position or original cause, and terminal registry phase.
- Compile-fail tests reject coordinator or active-token cloning, direct identity
  construction, double commit, and indeterminate retry.
- Workspace architecture tests continue enforcing the exact
  `ntsql-transaction -> ntsql-wal` edge.
- Source-backed construction and memory-adapter tests prove distinct epochs
  across coordinator and model-restart boundaries.
- Recovery tests prove that only the issuing coordinator can resolve an
  indeterminate token and that lifecycle mismatches are rejected before lookup.
- Recovery tests retain the token when a source reports the expected lineage but
  returns a position carrying another lineage.

## Consequences

Safe downstream code cannot directly construct a coordinator epoch, transaction
identity, or active state, and an attempted token cannot be replayed through
another coordinator. Epoch uniqueness remains a persistence-port obligation. A
panic in an outer WAL adapter would leave the in-memory registry at
`CommitAttempted`, which is fail-closed but is not a recovery verdict. The future
recovery protocol introduced by ADR 0010 reconciles an existing indeterminate
token with epoch-qualified durable evidence, but remains in-process and defines
no external outcome or retry.
