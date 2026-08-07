# ADR 0058: Selected-Checkpoint Restart Live Ownership

- Status: Accepted
- Date: 2026-08-07
- Issue: #168
- Extends: ADR 0037, ADR 0053, ADR 0056, ADR 0057
- Follows: #167

## Context

ADR 0057 leaves one repaired selected-checkpoint restart in a deliberately
non-live restored owner. That owner retains the selected baseline, complete
current logical WAL analysis, replay observations, page-repair decisions and
outcomes, exact WAL/page/checkpoint adapters, and one fresh private transaction
coordinator. It proves that the coordinator epoch is above persisted identity
high-water, but it does not prove that those retained inputs are still coherent
at the instant ordinary mutation authority is released.

The repair executor may have acknowledged several durable page writes. The
restart-only coordinator epoch source may also have written physical allocator
metadata. A final transition must distinguish those authorized effects from an
unexpected logical WAL or page change, revalidate the whole successful result,
and release the exact retained resources without inventing a complete-recovery
report.

This decision adds one read-only consuming completion transition, one immutable
completion-evidence value, one selected-restart live owner, and one fail-closed
failure owner. It does not choose a WAL retention floor or authorize
reclamation; the exact private evidence is retained for issue #169.

## Crate and Dependency Boundary

Only `ntsql-transaction` owns the completion validation, evidence, live owner,
failure owner, and transition. The memory and filesystem adapters require no new
port or implementation:

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

No crate, direct dependency edge, architecture registration, logical WAL
record, physical frame, page-store record, checkpoint codec, file header,
checksum, marker, path, lock primitive, synchronization contract, or fault point
changes. The transaction domain remains I/O-free and calls only existing
repository-owned ports.

## Consuming Final Transition

`RestoredTransactionPageStorageRestartCheckpointReplay::
complete_restart(self)` consumes the only restored owner. Earlier selected,
planned, prepared, repaired, failed-repair, or restoration-failure states cannot
invoke it.

The transition:

1. obtains one fresh complete logical WAL analysis from the retained source;
2. compares that analysis with the exact retained complete-current analysis;
3. revalidates selected/current transaction evidence and restoration summary;
4. verifies that the private coordinator remains unused and coherent;
5. freshly observes every prepared replay page in original page order;
6. validates each observation against its prepared terminal state and historical
   repair outcome; and
7. only then destructures the complete nested owner into one live selected-restart
   owner.

It allocates no coordinator, appends no logical or physical WAL record, writes
no page, mutates no checkpoint, and creates no durability or repair permit.

ADR 0057's filesystem epoch-allocation frame is physical allocator metadata. It
does not occur in `DurableTransactionRestartObservation`, transaction tables,
logical record counts, or logical durable frontiers. A conforming source can
therefore return an analysis exactly equal to the retained analysis after the
authorized epoch allocation. Treating that frame as logical suffix advancement
would be an adapter-contract violation rather than an accepted completion
change.

## Exact Logical WAL and Coordinator Validation

The fresh and retained analyses must have:

- the same persistent lineage;
- the same optional logical durable frontier;
- the same ordered transaction-table length; and
- equal transaction entries at every index, including identity, state, owned
  page range, record count, and commit position.

The transition re-runs ADR 0057's selected/current restoration preparation
before any page observation. This preserves exact prefix rules and re-derives
transaction counts, committed/unresolved counts, and persisted identity
high-water from the privately retained values.

The restored coordinator must:

- belong to the fresh logical WAL lineage;
- retain the epoch recorded by the restoration summary;
- remain strictly above persisted epoch high-water;
- retain next local sequence one;
- have no lifecycle registry entries; and
- have no staged pages.

The retained summary must exactly equal the re-derived counts, highest persisted
identity, coordinator epoch, and cumulative indeterminate allocation-attempt
count. A persisted transaction is never converted into coordinator registry
membership or a runtime lifecycle token.

## Final Page Validation

The store must still report the retained logical WAL lineage. The number of
ordered outcomes must equal the number of prepared page decisions, and every
outcome must name the same page and match its prepared resolution category.

Every page is then freshly observed in the original strict `PageNumber` order:

- `NoRequiredImage` must still be absent.
- `CheckpointCurrent` must still equal the exact retained checkpoint snapshot.
- `AlreadyCurrent` must equal the exact prepared target.
- `Candidate` must equal the exact prepared target after repair.

For a candidate, historical `Repaired` and `TargetAlreadyPresent` outcomes are
both valid. They describe how the successful repair attempt resolved the page,
not what a final observation should report. Final completion always requires the
exact target. An unchanged original source therefore fails as
`FinalPageTargetMissing`.

Page equality remains exact across lineage, page number, physical WAL position,
page version, and all bytes. Page versions remain payload identity rather than
recency. Any missing target, pending decision, outcome mismatch, observation
failure, or comparison contradiction fails closed.

## Selected-Restart Live Owner

Success returns
`CompletedTransactionPageStorageRestartCheckpointReplay`. It is an explicit
sibling of ADR 0037's complete-recovery live owner, not a conversion to it.
Selected replay did not execute complete committed-page recovery and cannot
fabricate a `CommittedTransactionPagesRecoveryOutcome` or recovery report.

The completed owner directly owns:

- the exact fresh coordinator allocated by ADR 0057;
- the same WAL source, page store, and checkpoint source;
- the exact selected baseline;
- the retained complete-current logical WAL analysis;
- the owned replay observations;
- the prepared page-repair decisions;
- the original ordered repair outcomes; and
- the transaction-restoration summary.

Private construction prevents evidence substitution or assembly from separately
obtained resources. Shared `parts` exposes the coordinator, WAL, and store for
inspection. Mutable `parts_mut` releases their ordinary live interfaces
together. Consuming `into_parts` transfers those exact three values, immutable
completion evidence, and the same checkpoint source.

The exact selected baseline, analyses, replay observations, page source/target
decisions, and persisted transaction ranges remain private in
`TransactionPageStorageRestartCheckpointCompletionEvidence`. Public accessors
expose only persistent identity, selected/current numeric frontiers, inert
ordered outcomes, and aggregate transaction summary. Live operations do not
update that point-in-time evidence.

## Checkpoint Publication After Completion

Only the completed selected-restart owner exposes publication through its
retained checkpoint source. Publication derives a fresh current completeness
baseline from current live WAL/store state and uses the same private
prepare/publish implementation as ADR 0053's full-recovery owner.

Publication is explicit. Completion neither republishes nor invalidates the
selected checkpoint automatically. A publication after live mutation may
advance the stored logical frontier and include new transactions and pages while
completion evidence remains unchanged.

The restored owner has no publication method, so a checkpoint cannot be
published between transaction restoration and final validation. Publication
receipts remain inert and grant no startup, retention, or reclamation authority.

## Failure, Drop, and Reopen

`FailedTransactionPageStorageRestartCheckpointCompletion` privately retains the
complete restored owner and exactly one typed cause:

- fresh restart-analysis source or evidence failure;
- final store observation failure; or
- deterministic completion-evidence contradiction.

It exposes only the exact error and inert aggregate restoration summary. It has
no retry, adapter accessor, `into_parts`, checkpoint publication, complete
recovery fallback, or success-shaped downgrade.

Completion itself is read-only, but the retained composition offers no operation
that can change frozen evidence or repair a poisoned/unavailable adapter. A
retry would therefore advertise progress without an owned state transition.
Resolution requires dropping the failed owner, repairing the external cause if
possible, and reopening the complete startup composition. The fresh restart then
repeats checkpoint selection, replay planning, repair, restoration, and
completion.

## Filesystem Lock Continuity

ADR 0052's acquisition order remains WAL, page store, then completeness control.
Moving those exact values through restored, failed-completion, completed, live
mutation, publication, and `into_parts` does not close, clone, reopen, replace,
or reacquire any descriptor.

Independent open attempts for all three files remain excluded after completion,
after live transaction/page mutation, after checkpoint publication, and after
ownership transfer through `into_parts`. Drop releases them. A fresh three-file
open selects the newly published completeness checkpoint, reaches a later fresh
coordinator epoch, and validates the durable live page.

The locks remain cooperative advisory locks. This decision adds no
database-wide lock, wait protocol, hostile path defense, atomic multi-file
acquisition, or unsupported-filesystem guarantee.

## Authority and Retention Boundary

The completion transition authorizes only ordinary live use of its directly
owned coordinator, WAL source, and page store. Neither the completed owner nor
its evidence creates or substitutes:

- a complete committed-page recovery report;
- an active lifecycle token for any persisted transaction;
- rollback, abort, undo, compensation, or lock-table state;
- a new page-write, recovery, or replay-repair permit detached from live APIs;
- client-visible diagnostics;
- a WAL retention floor, truncation point, compaction plan, or reclamation
  permit; or
- native format or external compatibility evidence.

The private completion evidence intentionally survives `into_parts` so issue
#169 can derive retention constraints from one reviewed successful-startup
boundary. This ADR assigns no interpretation or authority to that future
derivation.

Compile-fail coverage rejects restored-owner adapter access and publication,
completed-owner construction, completion-evidence construction, failed-owner
retry or release, complete-recovery substitution, and retention/reclamation use
before a separately reviewed authority exists.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, page, WAL, checkpoint,
replay, repair, restoration, ownership, lock, and deterministic-fault contracts.
No external product documentation, driver, SDK, fixture, oracle, proprietary
governance tool, or native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server recovery phase, transaction table, undo or
redo behavior, checkpoint algorithm, LSN value, error number, diagnostic,
persistent format, or compatibility claim.

## Test Boundaries

- Domain fakes complete empty and repaired selected restarts, then issue the
  first live coordinator identity and mutate WAL/page state.
- Fresh logical WAL source failure, frontier/table change, coordinator misuse,
  lineage change, page observation failure, missing target, outcome mismatch,
  and foreign store lineage all retain a failed owner.
- Failure inspection exposes no adapter and performs no retry or fallback.
- Completion evidence remains immutable after live mutation and explicit
  checkpoint publication.
- Memory integration completes a selected repair, performs a live transaction
  and page flush, publishes a new current checkpoint, transfers all parts, and
  selects that checkpoint after restart.
- Filesystem integration proves all three locks remain held through completion,
  live mutation, publication, and `into_parts`, then selects and completes the
  published checkpoint after fresh reopen.
- Before- and after-write page-repair faults both retry from page one and proceed
  through restoration and completion while all three locks remain held.
- Existing full-recovery publication, selected-repair retry, restoration,
  persistent-format, poison, lock, architecture, and governance tests remain
  valid.

## Non-Goals

This ADR does not:

- execute rollback, abort, undo, compensation, or lock-table reconstruction;
- expose persisted transactions as active runtime state;
- automatically invalidate, delete, or republish the selected checkpoint;
- add checkpoint generations, history, fallback selection, or multi-slot choice;
- choose a WAL retention floor or truncate, compact, reclaim, or rewrite WAL;
- add another adapter port, persistent frame, codec, lock, or synchronization
  point;
- define database lifecycle, online recovery, isolation, concurrency, or
  force-at-commit policy; or
- define external SQL Server behavior or native file compatibility.

## Consequences

A selected-checkpoint restart now releases one exact fresh coordinator, WAL
source, page store, and checkpoint source only after the retained logical WAL,
transaction restoration, repair outcomes, and every final page snapshot are
revalidated together. Failure keeps the entire composition fail-closed; success
preserves immutable startup evidence while permitting ordinary live work and
fresh checkpoint publication.

The next boundary is issue #169: deriving conservative WAL retention constraints
from the private successful-startup evidence without turning the completion
evidence itself into reclamation authority.
