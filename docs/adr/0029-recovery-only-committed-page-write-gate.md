# ADR 0029: Recovery-Only Committed-Page Write Gate

- Status: Accepted
- Date: 2026-08-06
- Issue: #110
- Extends: ADR 0015, ADR 0018, ADR 0026, ADR 0028

## Context

ADR 0026 reconciles one stored page with complete physical, owner-aware, and
commit projections. ADR 0028 turns missing or behind state into an exact
point-in-time source/target candidate, but deliberately grants no mutation
authority and does not prove that its WAL target remains current.

A recovery write therefore cannot safely begin from a retained candidate alone.
It must read a fresh authoritative durable prefix, keep that prefix stable while
planning and writing, and atomically recheck the candidate's exact store source
before replacing it. A store error is an effect boundary just as it is for the
live ADR 0015 page path: once the store method is invoked, an error does not
prove whether the target became durable.

The first recovery mutation slice defines those domain ports, ordering,
typestate, outcomes, and failure semantics with fake implementations. Concrete
memory and filesystem adapter implementations remain later decisions.

## Crate and Dependency Boundary

`ntsql-transaction` owns the gate because it combines committed transaction-page
reconciliation with recovery-only authority. Its reviewed dependency graph
remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal
```

No adapter type enters the domain crate, no crate or dependency edge changes,
and the gate performs I/O only through inward ports.

## Stable Durable-Prefix Source

`DurableTransactionPageRecoverySource<N>` exposes one exact `LogLineage` and a
higher-ranked `with_durable_page_evidence` callback. For one requested page, the
callback receives:

1. the complete physical full-image projection;
2. the complete owner-aware transaction-page projection; and
3. the complete durable commit projection.

All slices must come from one authoritative durable prefix. The implementation
must hold that prefix stable for the callback's full duration and return the
callback output directly after invoking it at most once. The higher-ranked
evidence lifetime prevents the output from borrowing any slice. Because the
method is generic over an unconstrained output, a safe implementation cannot
produce a successful output without obtaining it from the callback.

`&mut` source access excludes ordinary in-process advancement through the same
value. It is not a cross-process lock. A persistent adapter must also retain its
cooperating-writer exclusion continuously across projection, callback execution,
store observation, planning, and any store attempt. Projection completeness,
prefix stability, and callback-result honesty remain trusted adapter contracts.

## Recovery Store and Permit

`CommittedTransactionPageRecoveryStore<N>` is distinct from the source port. It
exposes the store lineage, observes one authoritative current snapshot, and
accepts mutation only through:

```text
compare_and_replace(candidate, recovery_permit)
```

The adapter must, under one continuous exclusive store hold:

1. validate that the permit's page and commit positions equal the candidate's
   exact committed target;
2. re-observe authoritative current state;
3. compare it with the candidate's exact source precondition;
4. reject any changed source; and
5. durably replace a matching source with the candidate's exact target before
   returning success.

There may be no unlock, stale-cache gap, or separately coordinated check between
steps two and five. This adapter recheck is the load-bearing TOCTOU guard.

`CommittedTransactionPageRecoveryWritePermit` is private to construction,
invariant in a generative attempt lifetime, non-`Clone`, non-`Copy`, and consumed
by one call. It owns the target's exact page and commit positions. It is
unrelated to ADR 0015 `PageWritePermit`; neither permit converts into or
substitutes for the other.

The gate takes source and store through separate mutable references. They must be
distinct objects or disjoint split borrows. It assumes no combined mutable
self-alias, shared global target, or hidden baseline fallback.

## Gate Order

`recover_committed_transaction_page` performs exactly these stages:

1. Clone the source lineage and reject a different store lineage before source
   projection, store observation, or mutation.
2. Enter the source's stable-prefix callback.
3. Observe the current store snapshot.
4. Derive a fresh ADR 0028 decision, which reruns complete ADR 0026
   reconciliation over the callback evidence and that exact snapshot.
5. Return explicit no-write outcomes for `NoCommittedPage` and `ExactCurrent`.
6. For a candidate, compare it with the same observed snapshot and require
   `SourceMatches`.
7. Copy exact inert source and target identities, privately create one recovery
   permit from the target positions, and invoke `compare_and_replace` once.
8. Return `Recovered` only after the store reports durable success.
9. Convert every invoked store error into terminal indeterminate recovery state.

Step six is a defensive self-consistency check. It detects disagreement between
planning and the observation supplied to that same planning call, but cannot
exclude a later store change. Only the adapter's atomic recheck in step seven
closes that race.

The selected target remains ordered by committed page WAL position. A later
committed record with a numerically lower `PageVersion` is still the recovery
target.

## Outcomes and Pre-Write Failures

Successful outcomes are owned, inert metadata:

- `NoCommittedPage` records the requested page and proves no write was attempted;
- `AlreadyCurrent` owns the exact latest committed target already present; and
- `Recovered` owns the exact target the store reported durable.

Before store invocation, failures remain retryable only by rerunning the whole
gate:

- source/store lineage mismatch retains both lineages;
- source projection failure retains the exact source cause;
- store observation failure retains the exact observation cause;
- ADR 0028 planning and comparison failures retain boxed typed sources; and
- defensive unexpected comparison or missing-attempt-marker states fail
  explicitly.

No error is converted into a success-shaped fallback.

## Store Attempt and Source-Result Safety

The store attempt occurs inside the source callback, but its result is recorded
in gate-owned state outside the callback output. After the source method
returns, recorded attempt state has priority over that return:

- a recorded successful attempt returns `Recovered`; and
- a recorded failed attempt returns terminal indeterminate state.

This priority is required even if a defective source invokes the callback,
allows the store attempt, then discards its output and returns a source error.
Without the independent record, a completed or uncertain physical effect could
be misclassified as a pre-write source failure and blindly retried.

If no store method was invoked, a source error remains a pre-write source
failure. A successful write marker without recorded attempt state is a typed
defensive error rather than fabricated success.

## Terminal Ambiguity and Fresh Resolution

Any `compare_and_replace` error is
`IndeterminateCommittedTransactionPageRecovery`, regardless of whether a test
adapter injected it before or after physical replacement. It owns:

- the exact absent or prior-snapshot source state;
- the exact committed target; and
- the original adapter cause.

The value has inspection and decomposition only. It cannot recreate a recovery
permit, call a store, or authorize a direct retry. A fresh gate invocation must
reacquire stable WAL evidence and re-observe the store:

- after an after-effect failure, exact target presence produces
  `AlreadyCurrent` without another store attempt; and
- after a before-effect failure, an unchanged source may produce a newly
  authorized attempt.

The original error never decides which boundary occurred.

## Authority Boundary

Recovery outcomes, source/target metadata, pre-write errors, and indeterminate
state are data only. Safe downstream code cannot use them to create:

- `TransactionId`, `CommittedTransaction`, or another lifecycle token;
- `DirtyPage`, `TransactionDirtyPage`, or live `PageWritePermit`;
- another recovery permit;
- a durable-prefix callback; or
- source or store capability.

Private fields, the higher-ranked callback, the invariant permit brand, distinct
permit types, and compile-fail tests preserve these boundaries. The gate creates
authority only within one call and consumes it at the store boundary.

## Trust, Evidence, and Compatibility Boundary

The domain proves ordering and authority for safe composition. It cannot prove
that an arbitrary adapter supplied complete projections, held its prefix and
store locks continuously, performed the atomic source recheck, wrote the exact
target, or reported durability honestly. Violating those port contracts is an
adapter defect.

All evidence and semantics are repository-authored. This ADR consults no
external product, driver, SDK, fixture, oracle, proprietary governance tool, or
native MDF/NDF/LDF/BAK format. It defines no SQL Server recovery, transaction,
page, LSN, locking, error, or diagnostic behavior.

## Test Boundaries

- HRTB compile-fail coverage prevents evidence escape.
- Foreign source/store lineages fail before either effectful port operation.
- Source and observation faults retain exact causes before mutation.
- Planning failure retains the nested ADR 0026/0028 typed source.
- Empty and exact-current evidence return explicit no-write outcomes.
- Missing and behind stores each attempt one exact replacement.
- A later lower-version committed target is recovered by WAL order.
- Store mutation between observation and atomic recheck is rejected and becomes
  terminal indeterminate state without overwriting the changed source.
- Before- and after-effect faults retain exact source, target, and causes.
- Store attempt success or failure takes priority over a post-callback source
  error.
- Fresh reruns resolve before- and after-effect fault boundaries from current
  evidence.
- Compile-fail tests reject permit forging, cloning, widening, omission, live
  permit substitution, output authority conversion, and indeterminate retry.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- implement or change memory or filesystem adapter APIs;
- change a WAL or page-store format, marker, checksum, repair, or reopen rule;
- add multi-page recovery orchestration or establish lock ordering across
  multiple source/store pairs;
- change raw or uncommitted page policy;
- define checkpoints, redo/undo tables, rollback, abort, compensation,
  allocation, buffering, eviction, isolation, or force-at-commit;
- resolve an ADR 0015 live indeterminate page write directly; or
- define external SQL Server values or native file compatibility.

## Consequences

The transaction domain now owns a recovery-only gate that refreshes committed
evidence, authorizes one atomic exact-source replacement, preserves terminal
write ambiguity, and makes repeated recovery idempotent through fresh
reconciliation.

The next slice may implement both ports in the deterministic memory adapter.
Filesystem implementation must later preserve the same source-stability and
atomic store-recheck contracts across WAL v3/page-store reopen before checkpoint
or multi-page orchestration begins.
