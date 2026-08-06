# ADR 0037: Restart-Analyzed Storage Ownership

- Status: Accepted
- Date: 2026-08-06
- Issue: #126
- Extends: ADR 0033, ADR 0034, ADR 0035, ADR 0036
- Extended by: ADR 0038, ADR 0047, ADR 0053

## Context

ADR 0033 makes complete committed-page recovery a consuming ownership
transition. Its original recovered state then released the WAL and page store
for live use. ADR 0034 subsequently added deterministic validation and
transaction-table reconstruction over one complete durable logical WAL prefix,
and ADRs 0035 and 0036 implemented that source in both storage adapters.

Leaving the operations unrelated would preserve a reviewed path that can release
live storage without validating the unified record order. Committed-page
recovery deliberately works from per-page physical, owner-aware, and commit
projections. Those projections can be sufficient to repair page snapshots while
the complete unified stream still contains a duplicate commit, post-commit
owned page, cross-kind position collision, invalid frontier, or another
restart-analysis contradiction.

The restart analysis is non-authorizing: its result cannot create a transaction,
write a page, replay a record, or reclaim a log. Non-authorizing does not require
its validation failure to be optional. The reviewed composition path should fail
closed rather than release storage whose complete durable transaction history
did not validate.

## Crate and Dependency Boundary

Only `ntsql-transaction` owns the generic states and transition. Existing memory
and filesystem tests instantiate those states with their concrete adapters:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal

ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No crate, direct dependency edge, architecture registration, format, file,
frame, checksum, marker, repair rule, poison rule, synchronization point, fault
position, or open order changes.

## Narrowing the Page-Recovered State

`RecoveredTransactionPageStorage<Source, Store, N>` remains the successful
result of ADR 0033 committed-page recovery. It is now an intermediate rather
than live state.

It retains:

- the same privately owned source;
- the same privately owned store;
- their adapter-lifetime locks;
- the exact complete ordered page-recovery report; and
- the page-width type.

It exposes only:

- immutable `recovery_report()`; and
- consuming `analyze_restart(self)` when the source also implements
  `DurableTransactionRestartAnalysisSource<N>`.

Shared `parts`, mutable `parts_mut`, and consuming `into_parts` move to the
successor state. This intentionally narrows ADR 0033's original release point.
Keeping even a shared adapter reference here would permit page or WAL inspection
before the required transaction-history validation, so the intermediate exposes
no adapter reference.

The state name is retained because committed-page recovery did complete. It does
not mean the complete storage startup sequence is ready for live access.
Compile-fail coverage makes that distinction load-bearing so a future maintainer
cannot restore an adapter accessor based on the name alone.

The generic ADR 0033 `recover` transition does not add a restart-source bound.
A source that supports only page recovery can still produce this intermediate,
but cannot reach the reviewed live state.

## Consuming Restart-Analysis Transition

`analyze_restart(self)` takes exclusive ownership of the intermediate, then:

1. mutably borrows only its private source;
2. invokes `analyze_durable_transaction_restart` exactly once;
3. leaves the store completely untouched;
4. retains the exact page-recovery report; and
5. returns exactly one owning success or failure state.

The source callback performs its complete stable-prefix projection and domain
analysis under the same mutable borrow. For the filesystem adapter, the
descriptor holding the advisory WAL lock is never dropped, cloned, reopened, or
replaced. The page-store value and its lock remain owned but are not observed or
mutated.

Committed-page recovery itself does not append to the WAL. Because the
intermediate has exposed no source reference, safe caller code cannot advance
the durable prefix between page-recovery success and restart analysis. The
analysis therefore classifies the same source prefix used by the just-completed
recovery transition, subject to the trusted stable-source contracts already
recorded by ADRs 0029, 0032, 0034, 0035, and 0036.

## Restart-Analyzed Live State

Successful analysis returns
`RestartAnalyzedTransactionPageStorage<Source, Store, N>`. It privately owns:

- the page-recovered intermediate;
- the exact immutable page-recovery report;
- the exact immutable durable restart analysis;
- the source; and
- the store.

Only this state exposes:

- `recovery_report()`;
- `restart_analysis()`;
- shared adapter `parts()`;
- mutable adapter `parts_mut()`; and
- consuming `into_parts()`.

The mutable accessor returns only `(&mut Source, &mut Store)`. It does not expose
mutable recovery or analysis evidence. The consuming accessor returns the two
adapters and both owned evidence values, preserving rather than discarding the
startup result.

Private fields prevent direct construction or substitution of an independently
obtained analysis. The source analyzed by the transition is exactly the source
retained in the live state.

## Fail-Closed Analysis State

A source or evidence failure returns
`FailedTransactionPageStorageRestartAnalysis<Source, Store, N>`. It privately
retains:

- the same page-recovered owner;
- both adapters and their locks;
- the exact successful page-recovery report; and
- the exact `DurableTransactionRestartAnalysisError<Source::Error>`.

The failed state exposes only immutable `recovery_report()` and `error()`.
Manual `Debug` and `Display` require no formatting of either adapter, and
`Error::source` retains the exact nested source or evidence cause.

There is no:

- `retry`;
- shared or mutable adapter reference;
- `into_parts`;
- downgrade to the ADR 0033 release point;
- partial transaction table; or
- success-shaped fallback.

This is an intentional fail-closed availability decision. A prefix accepted by
committed-page recovery but rejected for duplicate commit, post-commit page,
global order, frontier, lineage, allocation, or source failure does not become
live through this composition path.

## No In-Place Retry

ADR 0033 recovery failure has a meaningful `retry`: an earlier physical store
write may have succeeded, a transient store fault may clear, and a new complete
inventory can resolve prior pages as already current.

Those semantics do not transfer to restart analysis. The failed-analysis state:

- owns the source exclusively;
- exposes no mutation;
- performs no source or store write; and
- retains the same durable prefix.

An evidence contradiction would therefore deterministically repeat. A poisoned
or otherwise unavailable source that requires reopen cannot repair itself while
the owning value remains held. Repeating the call would misleadingly advertise
progress without any state transition.

Dropping the failure releases both adapters and their locks. A later explicit
reopen starts from the unrecovered state and performs the complete reviewed
startup path again. Environmental capacity may have changed and adapter reopen
may establish new authoritative state, but this ADR promises no success. A
persistent evidence contradiction remains a persistent startup refusal.

## Point-in-Time Evidence Currency

The page-recovery report and restart analysis describe startup evidence. Once
the final state releases mutable adapters, live operations may:

- append logical WAL records;
- advance the durable frontier;
- persist new pages; and
- make an earlier physically complete suffix durable.

Neither stored result updates in place. They remain immutable historical
evidence and cannot be reused as:

- a current stable-prefix claim;
- a checkpoint-validity proof;
- a redo or undo plan;
- a current page-store reconciliation;
- a truncation or retention boundary; or
- authority to reconstruct runtime transaction tokens.

Tests make this visible by retaining the original analysis frontier after a live
operation advances the WAL.

## Lock Lifetime

Moving adapters through unrecovered, failed-page-recovery, page-recovered,
failed-analysis, and restart-analyzed states never closes, clones, reopens,
replaces, or unlocks either descriptor.

For the filesystem composition path:

1. the WAL lock is acquired before the page-store lock;
2. both remain held through committed-page recovery;
3. a second opener remains excluded while the page-recovered intermediate is
   held;
4. ADR 0036 keeps the WAL lock through the analysis callback;
5. both remain held by either analysis result state; and
6. only drop or final `into_parts` transfers/releases ownership.

The locks remain advisory and process-local safety still relies on cooperating
openers. Hostile path replacement, non-cooperating writers, unsupported lock
semantics, or storage that violates successful synchronization guarantees remain
outside this contract.

ADR 0053 later wraps this unchanged recovery/analysis path with one retained
completeness source. That wrapper does not release the WAL or page store at the
page-recovered intermediate, and it releases or transfers the third source only
with the same final `into_parts` boundary or drop.

## Concrete Scenarios

The domain fake success scenario first fails committed-page recovery after one
page, retries the complete batch, then analyzes the coherent owned-page and
commit stream for all three recovered transactions. Exact identities, page
ranges, counts, commit positions, callbacks, store observations, and store write
attempts prove that analysis used the retained source and never touched the
store. Only the analyzed state releases the fake adapters.

A separate fake prefix contains two commits for one persisted identity.
Committed-page recovery has an empty valid inventory and succeeds. Restart
analysis retains the exact `DuplicateCommit` identity and positions in a
failed-analysis owner. A source failure before callback is retained distinctly.
Neither failure offers adapters or retry.

The memory scenario contains four committed page owners, one uncommitted owner,
one raw page, and a volatile suffix. Recovery repairs the exact committed
targets. Analysis returns the exact lineage, marker-covered frontier, four
committed entries, and one uncommitted entry. A later live page flush advances
the WAL while the stored analysis frontier remains unchanged.

The filesystem scenarios perform the same recovery and analysis after real file
reopen. A second pair opener fails with
`AcquireExclusiveLock`/`WouldBlock` while the page-recovered intermediate and
again while the analyzed live owner exist; independent page-store opens prove
that the pair check is not stopping only at the WAL. A test-only source wrapper
delegates real file recovery and then injects a restart-source failure, proving
that the failed-analysis owner retains both real locks until drop. After a live
committed page and a second full reopen, the page report is already current and
a fresh analysis contains both transactions at the new frontier.

## Authority and Error Boundary

Successful validation is a prerequisite for live adapter release, but the
analysis itself remains inert. No state, report, error, or accessor can create:

- `TransactionId`, `ActiveTransaction`, `CommittedTransaction`, or coordinator
  lifecycle state;
- a dirty, clean, live-permitted, or recovery-permitted page;
- a WAL append or durability fence;
- a redo, undo, rollback, abort, or compensation command;
- a checkpoint record or checkpoint-validity proof; or
- log retention, truncation, or reclamation authority.

The failure remains internal startup evidence rather than `ClientDiagnostic`.
Adapter errors, lock stages, file paths, record positions, identities, and
capacity details do not enter the client-facing diagnostic contract.

Existing low-level adapter constructors, direct source traits, and free analysis
operation remain available for adapter tests and separately reviewed
composition, as ADR 0033 already permits. They are not a fallback or downgrade
inside this owning startup path.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, page, WAL, storage,
projection, recovery, ownership, lock, and fault contracts. No external product
documentation, driver, SDK, fixture, oracle, proprietary governance tool, or
native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server database-open sequence, recovery phase,
transaction table, corruption response, availability guarantee, lock, LSN,
error, diagnostic, or compatibility behavior.

## Test Boundaries

- Compile-fail tests reject adapter access from page-recovered and
  failed-analysis states and direct final-state construction.
- Domain fakes prove exact identities, ranges, counts, commit positions, one
  analysis call, unchanged store counters, immutable evidence, live release only
  after success, exact evidence/source error retention, and no failed-state retry
  or adapter escape.
- A page-recovery-valid duplicate commit fails closed with exact identity and
  commit positions.
- Memory integration proves page-recovery retry, exact analysis, final live
  access, later WAL advancement, and unchanged startup evidence.
- Filesystem integration proves exact reopened analysis, independent WAL and
  page-store lock exclusion in intermediate/failure/final states, live committed
  use, and fresh exact analysis after another reopen.
- Existing direct page recovery, restart source, analysis, adapter, format,
  poison, repair, fault, lock, and architecture tests remain valid.

## Non-Goals

This ADR does not:

- combine page recovery and restart analysis into one error enum or automatic
  operation;
- add in-place analysis retry, downgrade, quarantine read access, or repair;
- construct runtime transaction/coordinator state from restart entries;
- persist or validate a checkpoint;
- build a dirty-page table, replay start, redo/undo plan, compensation record,
  page index, or transaction coordinator;
- choose a retention floor, truncate, or reclaim a log;
- remove or restrict separately reviewed low-level adapter APIs;
- change database directory lifecycle, two-file atomicity, global startup
  exclusion, lock waiting, or multi-process coordination;
- change WAL/page-store bytes, version eligibility, markers, synchronization,
  poison, repair, or faults; or
- define external SQL Server values or native file compatibility.

## Consequences

The reviewed storage owner now releases live adapters only after both complete
committed-page recovery and complete durable restart analysis succeed. A
transaction-history contradiction is an explicit fail-closed startup result
rather than optional metadata.

The restart-analyzed owner is now the smallest safe composition boundary for a
separately reviewed persistent checkpoint baseline. Dirty-page analysis, replay
start, redo, undo/compensation, coordinator restoration, log reclamation,
database lifecycle, and external compatibility remain future work.

ADR 0038 adds the first persistable, non-authorizing checkpoint baseline
projection exclusively to this final owner. It does not change the startup
transition or make the stored point-in-time analysis current after live writes.
