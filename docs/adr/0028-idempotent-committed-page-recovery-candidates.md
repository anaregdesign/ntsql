# ADR 0028: Idempotent Committed-Page Recovery Candidates

- Status: Accepted
- Date: 2026-08-06
- Issue: #108
- Extends: ADR 0026, ADR 0027

## Context

ADR 0026 validates complete physical, owner-aware, commit, and stored-snapshot
evidence and reports state relative to the latest committed transaction-owned
page. ADR 0027 proves the memory and filesystem adapters construct those inputs
from one explicit durable-prefix pass after restart or reopen.

`StoreMissing` and `StoreBehind` identify a possible recovery need, but neither
result binds the validated source-store state to an exact target or distinguishes
an unchanged source from a target that is already present after a repeated
attempt. Treating either result as a replay command would also skip the separate
authority and terminal-write-ambiguity design required before recovery may
mutate a page store.

The smallest safe next step is an I/O-free candidate that retains exact
point-in-time source and target evidence and compares it with a newly observed
store state. It remains data only. It neither establishes that the target is
still current in a later WAL prefix nor authorizes a write.

## Crate and Dependency Boundary

`ntsql-transaction` owns the candidate because it derives from committed-relative
transaction-page reconciliation. Its reviewed direct dependency set remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal
```

The implementation reuses adapter-neutral observations and the existing ADR
0026 operation. No adapter type enters a domain crate, no crate or dependency
edge changes, and no I/O, clock, randomness, global state, or external
dependency is introduced.

## Candidate Derivation

`derive_committed_transaction_page_recovery_candidate` accepts the same:

1. expected log lineage;
2. expected page number;
3. optional stored snapshot;
4. complete physical page projection;
5. complete owner-aware transaction-page projection; and
6. complete durable commit projection

as `reconcile_committed_transaction_page`. It calls that operation once and
retains its deterministic validation priority and errors under a typed
`Reconciliation` source.

The successful reconciliation result maps as follows:

| ADR 0026 result | Recovery decision |
| --- | --- |
| `NoCommittedPage` | explicit `NoCommittedPage`; no candidate |
| `ExactCurrent` | explicit `ExactCurrent`; no candidate |
| `StoreMissing` | candidate with an absent source precondition |
| `StoreBehind` | candidate with the exact validated snapshot source precondition |

Candidate derivation does not turn a no-op state into fabricated work.
Defensive typed errors reject any impossible disagreement between the successful
reconciliation result and the snapshot supplied to the same call:

- an absent-store result with a supplied snapshot;
- a present-store result without a supplied snapshot; or
- a present snapshot position different from the reconciliation position.

The last check is load-bearing even though the current implementation obtains
both values from one call. It prevents future refactoring from silently binding
a different source snapshot to a successful reconciliation.

## Exact Source Preconditions

`DurableCommittedTransactionPageRecoveryPrecondition` has two forms.

`StoreMissing` records exact absence. A later absent observation matches this
source. Any present snapshot other than the exact target means the store changed.

`ExactSnapshot` borrows the exact snapshot supplied to successful `StoreBehind`
reconciliation and retains its matching durable commit position. The snapshot
provides:

- page number;
- log lineage through its required position;
- page WAL position;
- page version; and
- complete image bytes.

The retained source commit position records why ADR 0026 classified this
snapshot backing as committed. A page-store snapshot does not itself contain a
transaction owner or commit position, so later store comparison does not
re-verify that commit evidence. Any future authority gate must re-run the
complete commit-aware reconciliation.

The source snapshot is borrowed rather than copied. `PageImage` and
`StoredPageSnapshotObservation` deliberately are not cloneable, and a full page
copy would make the candidate large without increasing authority or certainty.

## Exact Committed Target

Every candidate privately retains the exact
`LatestCommittedTransactionPage` produced by ADR 0026. Through that wrapper the
target includes:

- persisted transaction owner fields;
- page number;
- log lineage;
- page WAL position;
- page version;
- complete image bytes; and
- matching durable commit position.

The wrapper borrows the exact owner-aware observation selected by physical WAL
order. A later target with a numerically lower `PageVersion` remains valid;
`PageVersion` is payload identity, not committed recency.

Private candidate fields prevent direct candidate construction without the ADR
0026 derivation path. Public accessors expose only inert evidence.

## Store Comparison and Deterministic Priority

`compare_committed_transaction_page_recovery_candidate` receives a candidate and
a newly observed optional stored snapshot. It performs no I/O and accepts no
store port.

An absent current snapshot is classified first by source kind:

- absent plus `StoreMissing` is `SourceMatches`;
- absent plus `ExactSnapshot` is `StoreChanged`.

For a present snapshot, validation and comparison proceed in this exact order:

1. reject an unexpected page number;
2. reject a foreign log lineage;
3. if the numeric position equals the target position, require exact target
   version and bytes;
4. if the numeric position equals an exact source position, require exact source
   version and bytes; and
5. otherwise report `StoreChanged` with optional expected-source and actual
   positions.

Page and lineage checks precede numeric position checks. This preserves a
distinct foreign-lineage error when an unrelated log uses the same numeric
position.

The comparison outcomes are:

- `SourceMatches`: the exact absent or exact snapshot source precondition still
  holds; and
- `TargetAlreadyPresent`: page number, lineage, target page position, version,
  and complete image bytes exactly match the candidate target.

If a current snapshot reuses the target or source position with different
version or bytes, comparison returns a specific payload contradiction rather
than generic stale state. A different position or an unexpected presence or
absence is `StoreChanged`.

## Idempotence and WAL Currency

`TargetAlreadyPresent` is the idempotent retry classification: observing the
exact target again does not propose another candidate effect. The name describes
current bytes and position only. It does not claim that this candidate caused
the state or that a previous write returned success.

Candidate comparison deliberately does not accept or scan current WAL evidence.
After candidate creation, a later committed page can supersede the retained
target. Comparing the old candidate with a store that still contains its target
continues to report `TargetAlreadyPresent`, while fresh ADR 0026 reconciliation
correctly reports that store behind the newer committed target.

This apparent difference is required. The candidate is a point-in-time
comparison value, not a lease, lock, durable frontier, transaction table, or
currentness proof. A future mutation gate must re-read authoritative durable
evidence and re-run complete-prefix reconciliation immediately before any
compare-and-replace write. Candidate comparison alone can never satisfy that
gate.

## Lifetimes, Allocation, and Complexity

The candidate has independent lifetimes:

- the target lifetime borrows the selected owner-aware WAL observation; and
- a behind-store source lifetime borrows the exact validated stored snapshot.

A missing-store candidate does not retain a snapshot despite carrying the
generic source lifetime. Commit and position evidence is cloned only where the
existing reconciliation already returns owned lineage-bound values.

Derivation adds no input-sized collection and delegates the dominant work to ADR
0026. Candidate construction is constant additional work and state.
Comparison is `O(N)` only for fixed-size page-byte equality and otherwise uses
constant state. No heap allocation occurs on a successful candidate or
comparison path. A boxed source is allocated only when retaining an ADR 0026
failure in a bounded planning error.

## Authority Boundary

Candidates, decisions, preconditions, comparisons, and errors are inert domain
data. They contain no mutation port, callback, replay operation, store handle, or
generative attempt brand and cannot create or convert into:

- `TransactionId`, `CommittedTransaction`, or another lifecycle token;
- `DirtyPage`, `TransactionDirtyPage`, or `PageWritePermit`; or
- a page-store write operation.

Compile-fail tests preserve transaction, dirty-page, and write-permit
boundaries for both the candidate and comparison result.

In particular, `SourceMatches` does not authorize an attempted write, and
`TargetAlreadyPresent` does not acknowledge one. A later recovery-only mutation
design must define fresh evidence validation, exclusive store coordination,
compare-and-replace semantics, WAL durability, terminal write ambiguity, and
restart verification without reusing a live transaction capability by
convenience.

## Evidence Boundary

The operation consumes only repository-authored domain observations and performs
no I/O. It does not consult an external product, driver, SDK, fixture, oracle,
proprietary governance tool, or native MDF/NDF/LDF/BAK format. It defines no
external SQL Server recovery, transaction, page, LSN, diagnostic, or
idempotence behavior.

## Test Boundaries

- Missing-store derivation retains exact selected target pointer and commit
  evidence.
- An absent store matches the missing source and an exact target snapshot is
  already present.
- Behind-store derivation borrows the exact source snapshot and retains its
  matching commit position.
- Exact behind source and exact target each classify distinctly.
- A later lower-version target remains selected by WAL order.
- Empty and exact-current reconciliation produce explicit no-candidate
  decisions.
- Missing current state for a behind candidate fails as changed.
- Wrong-page and same-numeric-position foreign-lineage snapshots retain
  deterministic priority.
- Same-position target and source payload contradictions fail distinctly.
- Other newly observed positions fail as changed.
- A stale candidate can still report its exact target present while fresh
  reconciliation selects a later committed target, proving comparison is not a
  WAL-currency check.
- Compile-fail tests reject lifecycle, dirty-page, and write-permit conversions.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- add or change an adapter API or persistence format;
- read, lock, compare-and-replace, or mutate a page store;
- create a replay command, write permit, callback, or recovery capability;
- prove candidate freshness after its original complete-prefix reconciliation;
- define terminal write ambiguity, redo, undo, rollback, abort, compensation,
  checkpoints, transaction tables, or dirty-page tables;
- change raw-page or stored uncommitted-page policy;
- define page reads, isolation, locking, buffering, eviction, or
  force-at-commit; or
- define external SQL Server values or native file formats.

## Consequences

The transaction domain can now describe an exact missing-or-behind recovery
candidate and distinguish an unchanged source from an exact target already
present. This establishes compare-only idempotence without granting physical
recovery authority.

The next slice may define a separately reviewed recovery-only mutation gate. It
must re-run complete-prefix reconciliation immediately before mutation, bind the
same source and target identities to exclusive store coordination, and preserve
terminal ambiguity for any invoked write.
