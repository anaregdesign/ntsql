# ADR 0061: Bounded Crash-Recovery Model and Trace Runners

- Status: Accepted
- Date: 2026-08-07
- Issue: #171
- Extends: ADR 0001, ADR 0008, ADR 0013, ADR 0014, ADR 0033, ADR 0034,
  ADR 0035, ADR 0036, ADR 0039, ADR 0052, ADR 0053, ADR 0056, ADR 0057,
  ADR 0058, ADR 0059, ADR 0060

## Context

The selected-checkpoint recovery path now has individually tested boundaries for
publication, crash/reopen selection, replay planning, page repair, transaction
restoration, live ownership, retention analysis, and atomic WAL-prefix
reclamation. Individual examples do not prove that those boundaries compose
across repeated interruptions.

The remaining risk is repository logic rather than undocumented product
behavior. A page-store effect may be installed while its caller receives an
error; a checkpoint candidate may survive before replacement; a WAL candidate
may become the selected generation before directory synchronization; and an
empty retained suffix still owns independent logical-position and allocator
high-water marks. A test that checks only the returned error can miss a durable
state contradiction that appears on the next reopen.

This work must not introduce an external database oracle, captured product
output, ambient randomness, unbounded fuzzing, or external fixtures. It also
must not move adapter concepts into `ntsql-transaction` or broaden
`ntsql-testkit`, whose reviewed responsibility is synthetic external conformance
orchestration.

## Decision

Add `ntsql-recovery-model`, a standard-library-only workspace crate that owns a
bounded repository-authored logical durability state machine. The model is an
inert test policy:

- it performs no I/O;
- it has no engine, adapter, contract, serialization, network, clock, or random
  dependency;
- it defines no SQL Server behavior or native file-format claim;
- it grants no runtime transaction, page, checkpoint, recovery, or reclamation
  authority; and
- it cannot open or mutate a concrete adapter.

Concrete runners stay in the development targets of
`ntsql-storage-memory` and `ntsql-storage-file`. Each adapter has a
development-only dependency on the model. The model never depends back on an
adapter.

## Dependency-Kind Boundary

The architecture checker validates direct normal, build, and development edges
separately. The reviewed graph adds only:

```text
ntsql-recovery-model ----> standard library only

ntsql-storage-memory -. development only .-> ntsql-recovery-model
ntsql-storage-file -. development only .-> ntsql-recovery-model
```

A development-only model edge is forbidden as a normal or build dependency.
Every model-to-domain or model-to-adapter edge is forbidden in every dependency
kind. Negative architecture tests enforce both directions.

This distinction is part of the architecture policy, not a Cargo convention:
the checker invokes dependency discovery independently for each edge kind and
compares it with a separate complete allowlist.

## Model State

The model stores only exact repository-authored logical facts:

- monotonically assigned WAL positions and record kinds;
- transaction epoch/sequence identities;
- transaction-owned page, raw page, and commit records;
- the complete record vector and its exact durable frontier;
- logical-position and allocator epoch high-water marks independently from
  retained records;
- immutable V4 replacement-header retained-first, logical-high-water, and
  epoch-high-water baselines separately from the current retained first and
  later append/allocator progress;
- current page snapshots with version, bytes, and required WAL position;
- the mutable checkpoint slot separately from the frozen checkpoint selected for
  the current recovery, including identity, lineage, frontier, replay start, and
  opaque anchor;
- recovery phase and independent WAL, page-store, checkpoint, old-WAL-inode, and
  new-WAL-inode ownership;
- physical WAL generation and the checkpoint anchor authorizing it; and
- independent selected, checkpoint-candidate, and WAL-candidate replacement
  state, including partial, corrupt, dangling, aliased, valid, and valid-higher
  entries without candidate selection authority.

Physical WAL generation starts at zero and is independent from checkpoint
publication. Publishing a checkpoint may replace the selected baseline and
therefore its derived anchor, but it does not advance physical WAL generation.
Only an installed WAL-prefix reclamation advances physical generation, and every
nonzero generation records the exact selected-checkpoint anchor that authorized
it.

The model never reconstructs a removed WAL prefix from a retained suffix. A
nonzero generation remains bound to the exact checkpoint anchor that
authorized its floor.

Retention candidates follow ADR 0059 exactly: the selected checkpoint frontier,
its inclusive replay start, every current stored-page backing record, every
unresolved transaction's first owned-page record, and any source-format
constraint. Checkpoint coverage does not exempt a current store or unresolved
transaction requirement. A record merely being newer than the checkpoint does
not make it a retention requirement.

## Operations and Legal Phases

Bounded traces use explicit operations for:

1. allocate a coordinator epoch;
2. begin a transaction;
3. append a transaction-owned page, raw page, or commit record;
4. flush the current WAL through its exact last position;
5. write one page-store snapshot;
6. publish one completeness checkpoint;
7. crash, discarding only volatile logical records;
8. reopen into unrecovered ownership;
9. select the checkpoint;
10. plan replay;
11. repair required pages;
12. restore transaction/coordinator state;
13. complete restart into live ownership;
14. analyze WAL retention;
15. reclaim one exact retained floor; and
16. crash/reopen the resulting generation again.

Illegal phase transitions, foreign or missing identities, nonmonotonic
positions, stale checkpoint identities, missing retained floors, and arithmetic
exhaustion return typed model errors. They do not panic or silently normalize
the trace.

A crash is legal from every recovery phase. It discards the frozen selection,
replay plan, restoration summary, and runtime ownership while preserving WAL
durability, already-installed page repairs, and any allocated epoch high-water.
The next reopen reselects and replays from durable state; a crash after
restoration therefore allocates a newer epoch rather than reusing the interrupted
one.

Position allocation never rewinds across a crash, including when an appended
record is not durable. The memory adapter discards its complete unflushed tail;
the filesystem adapter may retain complete frames beyond its last durability
marker. The model represents both outcomes explicitly, excludes the retained
tail from selection/replay, and permits a later flush to cover it. A flush with
no current record preserves an independently stored logical high-water instead
of replacing it with absence.

## Atomic Failure Semantics

Every modeled effect distinguishes:

- **before effect**: no logical or physical mutation is installed;
- **after effect**: the complete mutation is installed although an error is
  reported; and
- **success**: the complete mutation is installed and reported.

The caller decides whether the surrounding domain classifies an adapter error
as definite or outcome-indeterminate. The model records only physical effect.
An in-memory generation-swap fault carries an immutable observation of the
actual generation, retained first/count, anchor presence, and logical/epoch
high-water state at the boundary; tests compare that faulted source state
directly instead of substituting a clean control.

Filesystem replacement additionally records these interruption stages:

1. before candidate cleanup;
2. after cleanup;
3. after candidate creation;
4. after candidate write/copy;
5. after candidate synchronization;
6. before selected-path replacement;
7. after selected-path replacement;
8. during or after directory synchronization.

Before selected-path replacement, fresh open must validate the old selected
generation and remove, never promote, the candidate. After selected-path
replacement, fresh open must validate the new selected generation. Candidate
contents do not select a generation, including when they encode a valid higher
generation.

Replacement-attempt state is separate from candidate-path state. Cleanup and
post-rename stages may therefore retain an in-progress attempt whose candidate
entry is absent. Rename switches the selected entry and removes the candidate
path, but successful replacement does not complete and the old/new inode lock
overlap does not end until directory synchronization completes.

The filesystem adapter stores the locked pre-rename file handle separately from
the selected replacement handle until parent-directory synchronization returns
success. Post-rename fault tests retain a hard-link alias to the old inode and
prove that both the selected path and old alias remain locked by the failed
owner.

The model records selected WAL, page-store, and checkpoint ownership
independently. A concrete filesystem runner maps those facts to held cooperative
locks, checks the existing acquisition order, and checks the no-gap inode lock
handoff across rename.

## Generation-Zero and Pruned Selection

Generation-zero traces observe only the minimal physical generation before
choosing the complete-prefix path. An absent checkpoint and a present empty
checkpoint both remain valid before the first allocator epoch.

A nonzero observation requires the full anchored metadata observation. A change
between minimal and full observations rejects the source. Missing, changed,
foreign, or structurally invalid checkpoint anchors never expose complete-prefix
fallback.

## Deterministic Trace Inputs

The model uses a small checked-in canonical seed set for CI. A dependency-free
deterministic generator expands each seed into a bounded legal operation
sequence. Caller bounds and compile-time hard caps are validated before generation and cap
operations, transactions, pages, post-checkpoint records, recovery cycles, and
local-profile seed counts.

The checked-in seeds are the reviewed CI inputs. A larger deterministic local
profile expands additional seeds but does not silently change CI coverage.
Promoting a newly discovered seed requires adding it to the canonical set in a
reviewed change.

Only the I/O-free model interprets the generated `TraceOp` language. Concrete
adapters retain staged ownership types whose purpose is to make illegal dynamic
transitions unrepresentable, so their runners use repository-authored typed
scenarios and exhaustive declared-fault matrices instead of a generic operation
interpreter. Memory scenarios use the same canonical seed identifiers for stable
payload variation, but this does not claim that they execute the generated
operation vector. Each concrete reopen is still compared with an independently
advanced model state.

Failure output contains:

- the exact seed;
- the failing operation index;
- the stable operation prefix; and
- every model/subject contradiction.

Prefix reduction finds the shortest failing prefix for the supplied predicate.
It does not claim a globally minimal subsequence and does not reorder effects.

## Concrete Runner Obligations

After every concrete reopen, a runner compares all observable facts it owns with
the model:

- durable logical positions and record kinds;
- page numbers, versions, bytes, and required positions;
- checkpoint presence/identity/frontier and any bound anchor;
- physical generation;
- logical high-water, next logical position, and allocator epoch continuation;
- selected checkpoint/WAL state and both fixed-candidate states;
- recovery phase; and
- cooperative lock ownership.

One mismatch returns a complete contradiction list for that observation rather
than stopping at the first field. Adapter errors remain adapter errors and are
reported with the trace prefix; they are not converted into success-shaped model
observations.

The memory runner covers every memory WAL, page-store, checkpoint source,
checkpoint publication, and generation-swap fault. It verifies volatile
truncation, before/after effects, empty suffix continuation, repeated
reclamation, and pruned-source fallback denial.

The filesystem runner covers every WAL, page-store, checkpoint publication, and
reclamation fault. It also covers:

- truncated/torn tails and checksum corruption;
- corrupt selected generations;
- stale, dangling, aliased, partial, and valid-higher-generation candidates;
- V4 replacement-time header boundaries followed by later append progress;
- empty retained suffix append/epoch continuation;
- post-rename lock continuity; and
- repeated reopen convergence.

Existing focused adapter tests remain authoritative for their local byte and
typestate boundaries. Model runners compose those boundaries; they do not
replace golden-format, compile-fail, or exact I/O-stage tests.

## Bounded Profiles

The default workspace test executes only the canonical seed set and explicit
fault matrix. Its bounds and case count are constants reviewed with the model.
The existing Governance job's 30-minute timeout is the outer CI execution
budget; correctness never depends on elapsed time.

The crate also provides an ignored deterministic longer profile. It is run
locally with:

```sh
cargo test -p ntsql-recovery-model --all-features --locked \
  tests::longer_local_profile -- --ignored --exact
```

Adapter-specific longer profiles use the same canonical trace renderer and state
comparison. No profile writes a fixture or depends on execution time for
correctness.

## Governance

All model behavior is workspace-authored and provenance-free from external
products. Seeds describe repository operations, not observed SQL Server output.
No raw evidence, SDK, driver, native file, network service, proprietary oracle,
or external governance tool is consulted.

The new crate adds no third-party dependency. Its package registration,
dependency-kind allowlist, negative architecture tests, SBOM, and ADR are
delivered in the same change.

## Test Boundaries

- Model unit tests cover every legal phase, illegal transition, before/after
  effect, crash truncation, empty retained suffix, repeated reclamation,
  position/epoch/generation exhaustion, invalid floor, replacement interruption,
  candidate non-promotion, observation contradiction, deterministic generation,
  and failing-prefix rendering.
- Canonical seeds run in ordinary CI.
- The ignored longer profile expands deterministic local coverage.
- Memory and filesystem development tests execute canonical traces and fault
  matrices against their concrete public ports.
- The architecture checker rejects normal/build promotion of the adapter model
  edges and rejects all reverse model dependencies.
- The full pinned Technical Governance suite remains required.

## Non-Goals

This ADR does not:

- model SQL Server recovery, ARIES, undo, compensation, native formats, or
  client-visible compatibility;
- prove an unbounded state space;
- add online recovery, concurrent reclamation, reader epochs, backup, HA, or
  replication;
- infer candidate selection from file names or generation numbers;
- add external fuzzing/property dependencies or nondeterministic CI;
- turn test observations into runtime authority; or
- weaken any existing fail-closed owner or compile-fail boundary.

## Consequences

The complete selected-checkpoint path now has one bounded, reproducible logical
oracle independent from both persistence adapters. Concrete tests can state the
exact durable model expected after each interrupted effect and reopen instead of
repeating ad hoc assertions.

The model is bounded evidence, not a proof of all possible executions. New
effect stages, fault points, persistent fields, or ownership phases require an
explicit model operation and runner coverage in the same change.
