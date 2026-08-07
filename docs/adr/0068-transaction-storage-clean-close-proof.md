# ADR 0068: Transaction-Storage Clean-Close Proof

- Status: Accepted
- Date: 2026-08-08
- Issue: #186
- Extends: ADR 0020, ADR 0047, ADR 0048, ADR 0051, ADR 0058, ADR 0059,
  ADR 0060, ADR 0066, ADR 0067
- Extended by: ADR 0069

## Context

ADR 0067 defines the inert fields a database clean-close certificate must bind,
but deliberately does not authorize deriving those fields from live
transaction storage. A safe derivation cannot reuse startup completion or WAL
retention analysis: live work may have appended WAL, changed page snapshots, or
advanced coordinator lifecycle since those values were frozen.

The live database owner established by ADR 0066 retains a
`WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay`. That owner
contains the exact coordinator, WAL source, page store, selected completeness
checkpoint, checkpoint source/publisher, and generation-aware validation path.
It is therefore the narrowest owner that can prove current transaction storage
is clean without adding global state or giving an inert analysis new authority.

WAL generation zero can be reanalyzed as a complete logical prefix. A
reclaimed generation cannot: its selected checkpoint is the authority for the
pruned prefix, and only its retained suffix remains physically observable.
Clean close must preserve both paths rather than falling back from an anchored
generation to a complete-prefix claim.

The coordinator also retained one staged `(transaction, page)` entry after a
successful ordinary `flush_committed_page`. That free function correctly owns
WAL-before-store ordering but cannot mutate coordinator state. Treating every
staged entry as unresolved without a coordinated success path would make an
otherwise orderly live page write permanently uncloseable.

## Decision

### Consuming close preparation

Add
`WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay::prepare_clean_close`.
It consumes the live transaction-storage owner. Success returns
`PreparedTransactionPageStorageCleanClose`, which privately retains:

- the complete consumed transaction-storage owner;
- the exact reloaded clean-close checkpoint candidate; and
- one private-constructible `TransactionPageStorageCleanCloseProof`.

The prepared owner exposes the proof only by shared borrow and has no adapter,
checkpoint, receipt, or analyzed-owner extraction. The proof is neither
`Clone` nor `Copy` and has no public constructor. A later database transition
must consume the whole prepared owner; detached fields cannot authorize clean
manifest publication.

Every failure returns
`FailedTransactionPageStorageCleanClosePreparation`, which privately retains
the consumed owner and exposes only typed diagnostic evidence. It has no retry
or owner-extraction method. The composition root must drop and reopen from
durable state.

### Coordinator quiescence

Close preparation first rejects, in deterministic transaction order:

- `Active`;
- `CommitAttempted`;
- `Indeterminate`; and
- `PageAppendIndeterminate`.

Only `Committed` and `NoDurableCommitRecord` are terminal coordinator
lifecycles. Any remaining staged page rejects close even when its transaction
is committed.

Terminal status alone is insufficient. Every current-coordinator-epoch
`Committed` lifecycle must have an exact committed entry in the fresh durable
transaction table, every `NoDurableCommitRecord` lifecycle must have no such
entry, and every durable committed entry in the current coordinator epoch must
have a matching `Committed` lifecycle. Historical transactions from earlier
epochs need no live registry entry. This bidirectional check rejects a
same-lineage but incorrectly wired WAL port instead of treating the
coordinator and WAL as independently clean.

Add `TransactionCoordinator::flush_staged_committed_page` for live database
paths. It validates the commit/page identity, coordinator WAL lineage,
`Committed` registry state, and exact staged-page membership before invoking
the existing WAL-before-store flush. It removes the staged obligation only
after that flush returns success. Rejection and WAL/store failure retain the
obligation. The existing free `flush_committed_page` remains valid for detached
uses but deliberately cannot discharge coordinator-owned close evidence.

### Fresh generation-aware completeness

Close preparation derives a new completeness baseline from current evidence:

- `CompletePrefix` reuses the complete logical WAL callback and current page
  snapshot analysis from ADR 0047.
- `Anchored` invokes the selected checkpoint's private retained-suffix replay
  planner. It uses the planner's full transaction analysis plus its owned
  retained observations to derive the current page table. Page numbers from
  the selected baseline seed the fresh validation set, so losing an older
  retained backing record cannot make that page disappear from the new
  checkpoint. It never invokes the complete-prefix callback and never falls
  back to generation zero behavior.

The common completeness derivation now accepts either borrowed complete-prefix
observations or owned retained-suffix observations together with one private
current transaction analysis. This is an internal refactoring only; detached
analyses still cannot prepare a checkpoint or close proof.

The resulting baseline must have:

- zero `Uncommitted` durable transaction entries;
- every page classified `StoreCurrent`;
- replay start exactly `AfterFrontier`; and
- transaction and page entry counts representable as portable `u64` values.

`NoRequiredImage`, `StoreMissing`, `StoreBehind`, and `AtPosition` are explicit
typed rejections. Count conversion remains checked even though supported Rust
targets have at most 64-bit `usize`.

### Publication and revalidation order

After the pre-publication checks:

1. Observe fresh retention metadata and require its lineage to match the WAL,
   its allocated epoch high-water to equal the live coordinator epoch, and any
   source-format constraint to share the lineage. Every durable transaction
   epoch must be at or below that allocator high-water.
2. Publish the exact current completeness baseline into a dedicated durable
   clean-close candidate through the retained checkpoint owner.
3. Reload that candidate through its sibling read port.
4. Rederive generation-aware current completeness from a second fresh WAL/page
   window.
5. Require the second baseline to equal the published baseline exactly.
6. Require every reloaded transaction, page, replay, frontier, and persistent
   identity field to equal the second baseline exactly.
7. Reobserve retention metadata, validate it again, and require it to equal the
   pre-publication observation.
8. Only then construct the proof and successful prepared owner.

The candidate namespace is physically disjoint from the restart-selected
checkpoint slot. Publication never replaces the baseline named by a pruned WAL
generation's persisted anchor. A crash before the later clean-manifest commit
therefore leaves the recovery-required path able to select its predecessor
checkpoint, while a successful clean manifest can later select the exact
candidate by the certificate anchor. This separation is required even when live
work changes the current baseline anchor after an anchored restart.

The proof binds exactly:

- persistent WAL identity;
- optional current durable WAL frontier;
- allocated transaction epoch high-water;
- versioned selected completeness-checkpoint anchor;
- portable transaction-entry count; and
- portable page-entry count.

These are the transaction-owned fields required by
`DatabaseCleanCloseCertificate`. The later database transition additionally
binds the source manifest lifecycle generation.

Close preparation does not reclaim or replace WAL and does not change the
restart-selected checkpoint or its WAL-generation anchor. It only observes the
generation-aware source and publishes the dedicated clean-close candidate. WAL
reclamation remains the separate consuming authority of ADR 0060.

### Effect classification

Errors before the checkpoint publisher is invoked are
`BeforePublication`. They prove no close-checkpoint publication attempt
occurred.

The candidate publisher contract defines every returned error as
outcome-indeterminate for candidate state. Therefore publisher failure, reload failure or absence,
post-publication completeness failure or change, reloaded-field contradiction,
and post-publication metadata failure or change are all
`OutcomeIndeterminate`. Adapter-specific before/after knowledge does not weaken
that classification.

Candidate indeterminacy does not make the predecessor restart checkpoint
indeterminate: the port contract forbids modifying that slot. The failed close
owner is nevertheless terminal, and recovery must reopen through the durable
manifest state that remained selected.

Neither class grants a retry path. In particular, a publisher error never
manufactures a publication receipt, and a successful publish without exact
reload/revalidation never manufactures a clean-close proof.

## Dependency Boundary

No crate or dependency edge changes. All new authority and validation remain
inside I/O-free `ntsql-transaction` and use its existing `ntsql-page` and
`ntsql-wal` domain ports. Concrete memory and filesystem publication remain
adapter responsibilities in later #186 changes.

## Tests

Repository-authored unit and compile-fail tests cover:

- exact proof fields and publish/read/reanalysis ordering;
- complete-prefix and anchored pruned-generation success;
- anchored live changes publishing a different close-candidate anchor without
  replacing the WAL-selected recovery checkpoint;
- crash after candidate replacement leaving anchored recovery selection valid;
- anchored rejection when a selected page's retained backing record disappears;
- all four nonterminal and both terminal coordinator lifecycles;
- exact current-epoch agreement between coordinator lifecycle, durable commit
  state, and allocator high-water, while allowing historical committed entries;
- detached flush retaining, and coordinated flush discharging, staged-page
  obligations;
- unresolved durable transactions;
- missing, behind, and current page classifications;
- non-`AfterFrontier` replay;
- portable count overflow;
- metadata source failure before publication;
- publisher failures on both sides of an adapter effect;
- reload failure, absence, and exact-field mismatch;
- current-baseline and metadata change across publication;
- inability to clone or forge the proof; and
- inability to extract or retry prepared and failed owners.

## Evidence and Compatibility Boundary

All lifecycle rules, ordering, error classifications, proof fields, and tests
are repository-authored. No external product documentation, SDK, driver,
fixture, oracle, captured output, proprietary governance tool, or native
database/log format was consulted.

This proof does not claim SQL Server transaction, checkpoint, shutdown, LSN,
recovery, or file-format behavior.

## Non-Goals

This decision does not:

- add the database `Live -> ClosePending -> Closed` typestate transition;
- construct or publish `DatabaseCleanCloseCertificate` or Manifest V2;
- flush or synchronize database manifest candidates;
- release filesystem locks or define subprocess lock inheritance;
- authorize clean reopen;
- perform WAL reclamation for close;
- define destructor-triggered close or retry; or
- add externally observable MSSQL compatibility behavior.

## Consequences

The transaction layer can now prove one exact current, replay-free,
page-current state across a durable checkpoint round trip while retaining all
live owners. Issue #186 can next consume this owner at the database typestate
boundary, then implement memory and filesystem clean-manifest publication
without reconstructing transaction evidence or trusting stale startup values.
