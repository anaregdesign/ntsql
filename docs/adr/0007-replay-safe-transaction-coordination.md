# ADR 0007: Replay-Safe In-Process Transaction Coordination

- Status: Accepted
- Date: 2026-08-05
- Issue: #59
- Extends: ADR 0001, ADR 0005, ADR 0006

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

Multiple coordinator instances may issue the same numeric value, but their
active tokens cannot cross the private runtime identity boundary. Persistent
uniqueness across process or coordinator lifetimes requires a future
recovery-backed epoch or allocator and is not claimed here.

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

## Consequences

Safe downstream code cannot reissue or reconstruct active state within one
coordinator lifetime, and an attempted token cannot be replayed through another
coordinator. A panic in an outer WAL adapter would leave the in-memory registry
at `CommitAttempted`, which is fail-closed but is not a recovery verdict. The
future recovery protocol must reconcile attempted identities with durable log
state before permitting any external outcome or retry.
