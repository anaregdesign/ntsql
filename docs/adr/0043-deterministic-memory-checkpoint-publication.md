# ADR 0043: Deterministic Memory Checkpoint Publication

- Status: Accepted
- Date: 2026-08-06
- Issue: #138
- Extends: ADR 0042

## Context

ADR 0041 supplies one separate constructor-seeded memory checkpoint read slot.
ADR 0042 supplies a sibling publisher port with a private owner permit, an
all-or-nothing single-slot success postcondition, and outcome-indeterminate
errors. No concrete adapter joins those two boundaries.

The first publisher implementation should exercise exact replacement and
publish-then-load validation without prematurely choosing a persistent format,
filesystem replacement mechanism, synchronization policy, or checkpoint
generation model. The existing memory slot is the narrowest adapter that can do
so.

## Crate and Dependency Boundary

Only `ntsql-storage-memory` production code and tests change. The reviewed graph
remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

No crate, dependency edge, architecture registration, domain I/O, filesystem
API, byte format, or physical lock changes.

## One Separate Read and Publication Slot

`InMemoryTransactionRestartCheckpointBaselineSource` continues to own one
optional untrusted observation independently of `InMemoryCommitLog` and
`InMemoryPageStore`. It now implements both sibling ports:

- `DurableTransactionRestartCheckpointBaselineSource`; and
- `DurableTransactionRestartCheckpointBaselinePublisher`.

The type name records its original read responsibility but does not make the
ports inherit from one another. Generic code may still require either port
alone. Implementing both does not couple checkpoint activity to WAL durability
or page storage.

`empty()` and `seeded(observation)` remain deterministic fixture construction.
A seed is not a publication, receipt, validated baseline, startup choice, or
durability proof. Runtime replacement is reachable only when the
restart-analyzed owner supplies ADR 0042's private publication permit.

`slot()` now observes the exact current untrusted slot, whether fixture-seeded
or structurally lowered from a publication attempt. It is test inspection, not
an authority conversion.

## Permit Verification Before Effect

Before allocation, slot mutation, or publication-fault consumption, the adapter
compares all permit identifiers with the supplied authoritative baseline:

- `PersistentLogId::get()`;
- optional durable frontier; and
- transaction-entry count.

Any difference returns
`InMemoryTransactionRestartCheckpointBaselinePublicationError::
PublicationPermitMismatch` containing both complete identifier triples. The
slot and all fault plans remain unchanged.

Safe external code cannot construct a mismatched permit. The check nevertheless
enforces the publisher port's adapter obligation and fails closed if the domain
boundary is ever changed incorrectly.

## Complete Candidate Before Replacement

After permit verification and the optional before-replacement fault, the
adapter constructs one complete owned observation:

1. read the exact baseline transaction count;
2. reserve exactly that many entries in a new vector with
   `try_reserve_exact`;
3. lower every entry in its existing order;
4. copy epoch, sequence, first and last owned-page positions, record count, and
   committed or uncommitted state without normalization; and
5. construct an owned observation from the raw persistent ID, unchanged
   frontier, and complete vector.

Reservation failure returns
`TransactionCapacityExhausted { transaction_count }`. The prior slot remains
unchanged and no partial candidate is exposed.

Only after candidate construction succeeds does one assignment replace the
optional slot. In this deterministic single-threaded adapter, that assignment
is the physical mechanism satisfying ADR 0042's abstract all-or-nothing success
postcondition. It is not a claim about filesystem or cross-process atomicity.

## Deterministic Publication Faults

The publication path has an independent one-shot fault plan:

- `BeforeReplace` returns its exact injected error before candidate allocation
  or slot replacement, preserving the prior slot.
- `AfterReplace` installs the complete exact candidate and then returns its
  exact injected error.

Reaching either matching point clears only the publication plan. A load fault
may remain armed across publication, and a publication fault may remain armed
across loads. Attempting to replace an already armed publication fault returns
`RestartCheckpointBaselinePublicationFaultAlreadyArmed` with the retained and
rejected points and changes neither plan.

These different physical effects are visible only through direct memory-adapter
test inspection. Once the publisher has been invoked, the transaction-domain
owner maps both errors to the same outcome-indeterminate publication boundary.
The before-effect label therefore grants no domain retry permission, and the
after-effect label grants no authoritative resolution.

## Success and Structural Read-Back

Publisher `Ok(())` means the selected memory slot is the exact structurally
lowered supplied baseline. The owner returns ADR 0042's receipt containing only
the matching persistent ID, frontier, and transaction count.

A later load still:

1. applies the independent pre-load fault;
2. reserves a fresh exact vector;
3. returns a new owned untrusted observation; and
4. leaves the selected slot unchanged.

Even immediately after successful publication, that observation is not
authoritative. The real restart-analyzed owner must validate it against the
current retained WAL prefix under ADR 0039. Neither this adapter, the publisher
result, nor the receipt bypasses that validation.

## Error and Authority Boundary

`InMemoryTransactionRestartCheckpointBaselinePublicationError` distinguishes
adapter facts:

- complete permit identifier mismatch before effect;
- injected before- or after-replacement failure; and
- exact candidate-vector capacity exhaustion.

All are outcome-indeterminate at the outer ADR 0042 boundary because the
publisher was invoked. Adapter-specific distinctions support deterministic
tests and diagnostics only. They define no retry, resolution, repair, or
selection policy.

The combined memory slot cannot create or satisfy:

- transaction lifecycle or coordinator state;
- WAL append, flush, restart-analysis, or lineage authority;
- page-store or committed-page recovery write authority;
- a recovered or restart-analyzed storage owner;
- an authoritative checkpoint baseline without current-prefix validation;
- checkpoint startup selection, replay, redo, undo, rollback, or compensation;
- a dirty-page table or replay start; or
- retention floors, truncation, compaction, or reclamation.

Existing compile-fail tests retain the WAL, page-store, restart-analysis,
baseline, and storage-owner boundaries. ADR 0042's compile-fail tests continue
to prevent direct publisher invocation without the private permit.

## Evidence and Compatibility Boundary

All behavior uses repository-authored baselines, observations, owner
composition, publisher/source ports, and deterministic memory adapters. No
external product documentation, driver, SDK, fixture, oracle, proprietary
governance tool, or native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server checkpoint bytes, publication point,
transaction table, recovery phase, error, diagnostic, or compatibility result.

## Test Boundaries

- Success replaces an arbitrary existing seed with exact non-empty current-WAL
  fields and returns matching receipt identifiers.
- A load fault remains armed across success; the first load fails without
  changing the slot, repeated later loads are fresh and non-consuming, and real
  owner validation returns the exact authoritative baseline.
- `BeforeReplace` preserves the old slot while `AfterReplace` installs the full
  new slot; both become the same outer outcome-indeterminate variant with exact
  attempted identifiers and nested adapter source.
- Publication-fault replacement is refused, matching faults are one-shot, and
  load/publication plans remain independent.
- Empty current-WAL publication exactly replaces a non-empty fixture identity
  with the persistent ID, no frontier, and zero entries.
- Startup analysis and page-store snapshots remain unchanged across successful
  publication and read-back validation.
- Capacity errors retain the exact requested transaction count and have no
  nested source.
- Existing memory WAL, page storage, restart, checkpoint, recovery,
  architecture, compile-fail, and governance tests remain valid.

## Non-Goals

This ADR does not:

- encode or decode checkpoint bytes or define a checksum;
- implement filesystem or cross-process atomic replacement;
- add synchronization, locks, concurrent access, or a global lock order;
- add generations, selection, fallback, history, deletion, or retention;
- authorize retry or resolution after indeterminate publication;
- make checkpoint presence or validity a startup gate;
- add dirty-page analysis, replay start, redo, undo, rollback, compensation, or
  coordinator restoration;
- choose a retention floor, truncate, compact, or reclaim a log; or
- define external SQL Server values or native file compatibility.

## Consequences

The existing isolated memory slot now exercises ADR 0042's real publisher and
read-back boundary with deterministic success, before-effect failure, and
after-effect failure. Successful values remain untrusted on load and acquire
authority only by exact current-prefix validation.

A persistent publisher is still blocked on separately reviewed bytes, integrity
protection, physical replacement, synchronization, startup selection, repair,
and lock ordering. Checkpoint completeness also still requires an independently
designed dirty-page/replay-start boundary before any recovery or WAL reclamation
claim.
