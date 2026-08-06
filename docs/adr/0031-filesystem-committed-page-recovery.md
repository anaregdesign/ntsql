# ADR 0031: Filesystem Committed-Page Recovery

- Status: Accepted
- Date: 2026-08-06
- Issue: #114
- Extends: ADR 0014, ADR 0017, ADR 0018, ADR 0027, ADR 0029, ADR 0030
- Extended by: ADR 0032, ADR 0033

## Context

ADR 0029 defines the recovery-only domain gate, stable durable-prefix source,
atomic exact-source store replacement, one-attempt permit, and terminal
ambiguity. ADR 0030 implements those trusted contracts only in deterministic
memory.

The filesystem adapters already provide every required persistent primitive:

- WAL v3 persists transaction-owned full-image pages and complete transaction
  commits in one ordered record stream;
- durable-through markers identify the authoritative WAL prefix;
- the append-only page store persists exact snapshots and their required page
  WAL positions;
- both files hold advisory exclusive locks for their complete adapter lifetimes;
  and
- uncertain writes poison an adapter until reopen.

ADR 0027 proves that manually projecting one reopened WAL v3 durable prefix
produces correct reconciliation evidence, but it grants no write authority.
The next slice must implement both ADR 0029 ports without changing a persistent
format, duplicating the page-store writer, weakening poison handling, or adding
an unlock gap around the source or store recheck.

## Crate and Dependency Boundary

Only `ntsql-storage-file` production code, tests, and owning ADRs change. Its
complete direct dependency set remains:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No domain crate imports the adapter, no crate or dependency edge changes, and no
new filesystem or synchronization dependency is introduced.

## V3 Recovery Source Eligibility

`FileCommitLog<N>` implements
`DurableTransactionPageRecoverySource<N>`. The trait implementation exists for
the generic adapter type, but authoritative committed-page recovery is accepted
only when the opened format is transaction-page-capable WAL v3.

WAL v1 has no page records and WAL v2 has no persisted transaction owner.
Either older format returns a typed error containing its exact version before
allocation, scanning, or callback invocation. There is no migration, inferred
owner, raw-page fallback, or empty-evidence success.

A poisoned WAL also fails before allocation, scanning, or callback invocation.
An uncertain earlier append or synchronization result cannot become
authoritative recovery evidence merely because the in-memory record vector is
inspectable. Reopen must synchronize, validate, repair any allowed incomplete
tail, and reconstruct unpoisoned state first.

## One-Pass Durable Projection

Before scanning, the source fallibly reserves `durable_len` elements in each of
three vectors:

1. commit-agnostic physical page observations;
2. owner-aware transaction-page observations; and
3. complete durable commit observations.

Capacity failure identifies the affected projection through
`FilePageRecoveryProjection`. The source then performs exactly one loop over one
`durable_records()` iterator.

Physical and owner-aware observations are included only for the requested
`PageNumber`. Every commit in the durable prefix is retained because owner
classification requires the complete commit projection. One transaction-owned
record contributes both physical and owner-aware views from the same position;
this preserves ADR 0026 cross-projection integrity and does not represent two
physical records.

Physical, owner-aware, and commit conversion failures remain distinct typed
errors with their exact boxed causes. The validated file scanner ordinarily
makes those failures unreachable, but a future invariant violation cannot
silently omit evidence or become a panic.

After the pass, the higher-ranked callback is invoked once with all three
complete slices and its output is returned directly.

## WAL Stability and Lifetime Lock

The `FileCommitLog` owns the locked file descriptor. Creation or open acquires
the ADR 0014 nonblocking advisory exclusive lock before format work and retains
it until the adapter is dropped. `with_durable_page_evidence` never unlocks or
reopens that descriptor.

The source therefore holds both:

- an exclusive `&mut FileCommitLog<N>` borrow from before projection through
  callback return; and
- the cooperating-writer file lock for the adapter's entire lifetime.

No safe in-process operation through that value can advance the durable
frontier during the callback, and no cooperating second file adapter can obtain
the WAL inode. Because the store attempt occurs inside that callback, the WAL
lock remains held across store observation, candidate planning, atomic recheck,
append, synchronization, and returned attempt state.

The lock remains advisory. A non-cooperating writer, hostile path replacement,
unsupported filesystem lock semantics, or storage that violates successful
`sync_all` guarantees remains outside the trusted adapter contract.

## Authoritative Store Observation

`FilePageStore<N>` implements the ADR 0029 store port. Its first observation
rejects a poisoned writer, looks up the exact current snapshot for the requested
page, and projects it through `StoredPageSnapshotObservation<N>`.

Absence remains `None`. A present but unprojectable snapshot retains its exact
typed cause. This observation feeds fresh domain planning but does not replace
the second authoritative observation inside `compare_and_replace`.

The page store owns a separate file descriptor whose advisory exclusive lock is
also retained for the adapter's complete lifetime.

## Locked Exact-Source Compare and Replace

`FilePageStore<N>::compare_and_replace` first rejects a poisoned writer. Under
one uninterrupted mutable hold and lifetime file lock it then performs this
order:

1. require the candidate target page position to share the store lineage;
2. require the target commit position to share that lineage;
3. require both permit positions to share that lineage;
4. require the permit page position to equal the target page position;
5. require the permit commit position to equal the target commit position;
6. re-project the authoritative current page snapshot;
7. compare it with the candidate and require exactly `SourceMatches`;
8. validate page layout and available store sequence; and
9. fallibly reserve a page-table slot when the source is absent.

Every stage precedes fault consumption and file mutation. Projection,
comparison, changed-source, lineage, position, width, sequence, and capacity
failures remain typed.

`TargetAlreadyPresent` at step seven is changed source, not success. The stale
attempt returns an error without another append. A fresh full gate invocation
will observe that target during planning and return `AlreadyCurrent`.

There is no unlock, stale-cache gap, or second file handle between current-state
re-observation and physical append. The store's in-memory page table is
authoritative because it was reconstructed under the same continuously held
file lock and is published only after synchronized writes.

## Shared Snapshot-Group Writer

Live ADR 0015 flush and ADR 0029 recovery use one private
`write_snapshot_group` implementation. Both paths therefore share:

- page-store v1 snapshot-header, required-position, and page-data framing;
- exact final-chunk padding;
- contiguous store-sequence advancement;
- before/after fault positions;
- frame-write and `sync_all` stages;
- poison behavior; and
- post-sync in-memory publication.

Recovery constructs one `FileStoredPage` from the exact committed target:

- target page number;
- target `PageVersion`;
- exact target bytes; and
- target page WAL position as the required position.

The matching commit position and transaction owner remain in the domain
recovery target. They are not fields in the existing page-store format. The
commit position must not replace the page position in the stored snapshot.

The writer emits the complete snapshot group and calls `sync_all` before
replacing or inserting the in-memory current snapshot and advancing
`next_sequence`. No fallible allocation remains after file mutation begins.
Successful recovery therefore has the same persistent meaning as successful
live page flush without duplicating or changing format logic.

## Fault, Poison, and Reopen Semantics

`PageStoreFaultPoint::BeforeWrite` fires after validation and reservation but
before the first snapshot frame. It changes neither file bytes nor in-memory
state. `AfterWrite` fires only after the complete group is synchronized, current
state is published, and the sequence advances.

Both are returned through
`FileCommittedPageRecoveryStoreError::PageStore`. Because the store method was
invoked, ADR 0029 converts either error into terminal indeterminate state. A
fresh invocation may:

- authorize another append after `BeforeWrite`; or
- observe the target and return `AlreadyCurrent` after `AfterWrite`.

An uncertain frame write or `sync_all` failure poisons the page-store adapter
before returning its exact I/O stage and source. Neither observation nor another
compare-and-replace may proceed until reopen. Reopen may truncate only an
incomplete final group under ADR 0018; a complete synchronized recovery group
reconstructs as the latest snapshot with its consumed store sequence.

No-write outcomes, planning errors, unsupported WAL formats, poisoned source or
observation errors, and rejected source comparisons do not consume an armed
page-store fault.

## Two-File Lock Topology

The recovery gate receives already opened, distinct WAL and page-store values.
It acquires no new lock and releases neither lifetime lock. Thus one invocation
continuously owns both cooperating-writer exclusions while it reads the WAL and
conditionally appends the page store.

Creation and open still acquire each file independently and nonblockingly. A
composition root must acquire the intended pair consistently and drop any
partially acquired adapter when opening the other file fails. This ADR does not
add a database-wide lock, wait protocol, multi-file transaction, lock upgrade,
or global lock-order registry.

Because acquisition uses `try_lock`, competing startup attempts fail rather
than wait in a cycle. A process that owns only one member of a pair does not have
authority to invoke this recovery operation.

## Reopened Volatile-Suffix Scenario

The integration scenario persists one page under one lineage:

1. committed image A at page/commit positions 1/2 with version 10;
2. committed image B at positions 3/4 with version 1;
3. durable uncommitted image C at position 5; and
4. durable raw image D at position 6.

Every WAL and page-store handle is dropped and reopened through the exact v3 and
page-store entrypoints. After reopen, another transaction-owned image is
appended at position 7 and made durable, while its complete commit at position 8
remains volatile under a `BeforeFlush` fault.

The physical record view contains eight records. The recovery source produces:

```text
physical page observations: 1, 3, 5, 6, 7
owner-aware observations:   1, 3, 5, 7
commit observations:        2, 4
```

Scanning `records()` would include commit position 8 and incorrectly select the
position-7 image. Scanning `durable_records()` keeps B at positions 3/4 as the
latest committed target, making volatile exclusion load-bearing after a real
filesystem reopen.

## Recovery and Persistence Outcomes

Using the real ADR 0029 gate:

- an A-backed store appends B as store sequence 2;
- an empty store appends B as store sequence 1;
- both recovered stores retain exact B bytes, version, and page position after a
  second filesystem reopen;
- a B-backed store returns `AlreadyCurrent` without consuming an armed fault;
- a D-backed raw store fails planning without consuming an armed fault;
- before- and after-effect errors retain exact source, target, and nested
  page-store cause;
- fresh reruns resolve both deterministic fault boundaries; and
- a target that appears between initial observation and adapter recheck is
  rejected as changed source without a second write.

B wins despite its lower page version because committed WAL position, not
`PageVersion`, defines recency. Later durable uncommitted, raw, and
volatile-commit evidence never becomes the recovery target.

## Error and Authority Boundary

Filesystem-specific source errors distinguish unsupported format, poison,
projection capacity, physical projection, owner-aware projection, and commit
projection. Observation errors distinguish poison from snapshot projection.
Store errors distinguish target/permit validation, current observation,
candidate comparison, changed source, and the exact nested physical page-store
failure.

The adapter never creates a candidate or either domain permit. It receives the
private recovery permit once and cannot substitute the live
`PageWritePermit`. Source/store errors, successful outcomes, and indeterminate
state are inert data and grant no transaction lifecycle, dirty-page, callback,
or retry authority.

## Evidence and Compatibility Boundary

All behavior uses repository-authored WAL v3, page-store v1, domain evidence,
and deterministic faults. No external product documentation, driver, SDK,
fixture, oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format
is consulted.

This decision defines no SQL Server page, LSN, transaction, recovery, locking,
file-sharing, synchronization, error, diagnostic, crash, or compatibility
behavior.

## Test Boundaries

- WAL v1/v2 rejection occurs before callback invocation.
- Poisoned WAL projection, page-store observation, and store comparison fail
  before fault consumption or mutation.
- Projection wrappers retain exact physical, owner-aware, commit, and snapshot
  causes; capacity failure identifies its projection.
- Direct source inspection proves one filtered dual projection, complete commits,
  and exclusion of a complete volatile suffix.
- Real-gate behind and missing recovery persist exact lower-version B across a
  second reopen.
- Exact-current and raw-backed planning paths preserve an armed fault.
- Before- and after-effect failures retain exact source, target, and physical
  cause; fresh reruns prove idempotent resolution.
- A target appearing before the locked adapter recheck is rejected without a
  duplicate append.
- Existing page-store golden bytes, live flush, sequence, fault, poison, tail
  repair, reopen, and lock tests remain valid through the shared writer.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- change WAL v1/v2/v3 or page-store v1 bytes, markers, checksums, repair, or open
  entrypoints;
- migrate older WAL formats or infer missing transaction ownership;
- add a database-wide or mandatory lock, multi-file atomic commit, or global
  lock-order protocol;
- change raw or uncommitted page policy;
- add multi-page recovery orchestration, checkpoints, redo/undo tables,
  rollback, abort, compensation, allocation, buffering, eviction, isolation, or
  force-at-commit;
- resolve a prior live or recovery indeterminate write without fresh
  authoritative reconciliation; or
- define external SQL Server values or native file compatibility.

## Consequences

The filesystem WAL v3 and append-only page store now implement the complete ADR
0029 committed-page recovery path. One invocation retains stable durable WAL
evidence, atomically rechecks the exact page-store source, durably appends the
selected committed image, and preserves terminal ambiguity and idempotent fresh
resolution.

The next crash-recovery orchestration slice may invoke this single-page
operation while designing checkpoint analysis, page enumeration, multi-page
ordering, and explicit database-level ownership separately.
