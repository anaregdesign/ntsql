# ADR 0006: Fail-Closed Transaction Commit Typestate

- Status: Accepted
- Date: 2026-08-05
- Issue: #52
- Extends: ADR 0001, ADR 0005
- Extended by: ADR 0007

## Context

ADR 0005 makes a durable commit acknowledgement available only inside a
generative callback after commit-record append and exact-position flush. A
transaction boundary must consume that proof without permitting the same active
value to commit twice or turning an ambiguous persistence error back into active
state.

No approved behavior specification currently defines SQL Server-visible
transaction counts, commit points, rollback behavior, diagnostics, isolation, or
crash outcomes. The first transaction behavior is therefore limited to an
internal fail-closed lifecycle invariant.

## Decision

`ntsql-transaction` is an I/O-free domain crate whose sole direct dependency is
`ntsql-wal`. This ADR initially introduced five staged values:

- `TransactionId`, an opaque coordinator-assigned internal identity with no
  wire, session, or persistent representation;
- `ActiveTransaction`, the only state that can begin a commit, which is consumed
  by the coordinator;
- `TransactionCommitRecord`, constructed privately from the consumed active
  identity and borrowed by a `CommitLog<TransactionCommitRecord>` port;
- `CommittedTransaction`, constructed only inside the generative WAL durability
  callback and retaining the exact confirmed log position; and
- `IndeterminateTransaction`, returned on either WAL failure with no commit or
  rollback operation.

`TransactionCommitError` owns both the indeterminate state and the original
append- or flush-specific `CommitError`. It never reconstructs active state. An
append error does not imply that no bytes were persisted, and a flush error does
not imply that the record can never become durable. Both outcomes are therefore
indeterminate and unsafe to retry without a later resolution protocol.

This typestate by itself enforces linear use of each `ActiveTransaction` value,
not transaction-ID uniqueness. ADR 0007 removes the temporary public
construction path and adds coordinator-owned unique issuance, runtime token
binding, and an attempt registry without changing the WAL durability proof.

The dependency direction is:

```text
transaction coordination and lifecycle state -> ntsql-transaction -> ntsql-wal
future persistence adapter ------------------> ntsql-transaction
future persistence adapter --------------------------------------> ntsql-wal

ntsql-transaction -> standard library
ntsql-wal         -> standard library
```

`ntsql-wal` remains independent of transaction types. The generic record
parameter lets the persistence adapter implement the port for
`TransactionCommitRecord` while depending inward on both domain contracts at
the composition boundary.

## Compatibility Boundary

The feature `transactions-concurrency.commit-lifecycle` remains `not-tested`.
This internal lifecycle does not define autocommit, nested transactions,
savepoints, `@@TRANCOUNT`, `XACT_STATE()`, rollback, isolation, locking,
connection behavior, client diagnostics, commit acknowledgment timing, or crash
recovery compatibility.

Those behaviors require approved specifications and the branded
`CompatibilityScope` when they first enter. Internal transaction and log
identities must never be exposed as SQL Server values.

## Test Boundaries

- Success consumes active state, appends exactly one record containing the same
  internal identity, flushes the exact assigned position, and returns committed
  state with both values preserved.
- Append and flush failure each consume active state and return the same identity
  in indeterminate state with the original phase-specific cause.
- Compile-fail tests reject direct identity construction, cloning coordinators
  or active values, committing one active value twice, and calling commit on
  indeterminate state.
- Architecture tests enforce the sole `ntsql-transaction -> ntsql-wal` edge and
  continue rejecting the reverse edge.

## Consequences

Transaction coordination cannot represent a successful commit without passing
the WAL durability fence. This crate neither retries an ambiguous attempt nor
converts its indeterminate result back into active state. Rollback, resolution,
recovery, persistent identity across coordinator lifetimes, concurrency policy,
and all external semantics remain explicit future responsibilities rather than
fallback behavior hidden in this state machine.
