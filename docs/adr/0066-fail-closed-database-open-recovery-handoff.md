# ADR 0066: Fail-Closed Database Open Recovery Handoff

- Status: Accepted
- Date: 2026-08-08
- Issue: #185
- Extends: ADR 0001, ADR 0037, ADR 0052, ADR 0053, ADR 0058, ADR 0059,
  ADR 0060, ADR 0062, ADR 0063, ADR 0064, ADR 0065
- Extended by: ADR 0067, ADR 0068, ADR 0069
- Follows: #184

## Context

ADR 0065 publishes one exact database composition with a manifest that remains
`RecoveryRequired`. Ordinary filesystem open already acquires the database
owner, manifest, WAL, page store, and restart-checkpoint control in fixed order,
validates the selected final objects, rejects aliases and create candidates, and
completes the manifest-parent durability barrier before returning exact
composition ownership. The memory adapter models the same five-object exclusion
and stable child identities.

That owner is intentionally not live. The transaction domain has separately
reviewed owners for generation-aware checkpoint selection, selected replay,
page repair, transaction restoration, restart completion, WAL retention
analysis, and optional atomic prefix reclamation. No database lifecycle gate
currently consumes those owners, and `LiveDatabase` has no public constructor.

A newly created composition has a valid but absent completeness slot. It cannot
enter selected replay until complete committed-page recovery and durable restart
analysis establish and publish the first exact completeness baseline. A
rejected checkpoint is different: failure of selected evidence must not become
permission to run a fallback or search another source.

## Decision

Database open consumes one exact recovery-required owner and releases
`LiveDatabase` only after:

1. generation-aware checkpoint selection;
2. selected replay planning;
3. read-only repair preparation;
4. whole-plan page repair;
5. transaction-state restoration into one fresh coordinator;
6. final restart completion;
7. whole-store WAL retention analysis; and
8. exact database/transaction persistent-WAL identity comparison.

Success retains
`WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay`. That owner
already contains the coordinator, WAL, page store, checkpoint source, private
completion evidence, and non-authorizing retention analysis. It also carries
the separately reviewed reclamation transition, but open does not invoke that
effect. Becoming live therefore does not replace the WAL or increment its
physical generation.

The manifest remains durably `RecoveryRequired` while the owner is live. A
crash before, during, or after live release consequently enters the same
fail-closed recovery path. Issue #186 owns any successor clean/unclean manifest
state and orderly-close evidence.

## Absent-Checkpoint Bootstrap

An absent checkpoint is accepted only for generation zero through the existing
generation-aware selector. Under the same retained adapters and locks, the
transaction domain:

1. explicitly acknowledges absence;
2. runs complete committed-page recovery;
3. validates the complete durable restart prefix;
4. publishes one current completeness baseline through the retained checkpoint
   source;
5. reassembles the unrecovered owner without reopening an adapter;
6. repeats generation-aware selection; and
7. requires the just-published checkpoint to select exactly before entering the
   ordinary selected path.

Publication failure is terminal even when the adapter may have installed the
new baseline. Unexpected absence or rejection during fresh reselection is also
terminal. Resolution is drop and complete reopen.

An initially rejected checkpoint never invokes
`continue_with_full_recovery`, even when standalone generation-zero evidence
would expose that operation. A pruned generation continues to deny fallback by
construction.

## Transaction Handoff Owner

`ntsql-transaction` owns one I/O-free consuming orchestration over its existing
staged types. It adds no adapter call, permit, repair rule, checkpoint field,
retention rule, or WAL effect beyond those existing transitions.

Every failure variant retains the exact stage owner:

- initial checkpoint rejection;
- bootstrap full-recovery failure;
- bootstrap restart-analysis failure;
- bootstrap publication or reselection failure;
- replay-planning failure;
- repair-preparation or repair-execution failure;
- deterministic or source transaction-restoration failure;
- final completion failure; or
- retention-analysis failure.

Database adapters keep the failure owner private and expose no same-owner retry,
fallback, or adapter extraction. Inert completed-phase observations support
process-exit tests. They contain no resource, evidence, permit, or callback
result and grant no authority.

## Database Live Gate

`ntsql-database` defines a `DatabaseRecoveryOwner<Input, N>` port. The concrete
owner type, rather than a caller-supplied success closure, selects the recovery
implementation. Repository filesystem and memory owners implement the port;
Rust coherence prevents downstream code from replacing either implementation.

The port must return:

- one retained outer database owner; and
- the exact private-constructible retention-analyzed transaction owner.

The database gate compares the transaction completion evidence's
`PersistentLogId` with the selected `DatabaseCompositionIdentity`. This is a
lineage cross-check, not a substitute for the already consumed database owner.
Database ID, role-bound file IDs, and manifest generation remain established by
the earlier structural binding. Only success constructs
`RecoveredDatabaseOwnership` and the private `LiveDatabase`.

Operation failure retains the adapter's exact failed owner. A lineage mismatch
retains both the recovered outer owner and completed transaction owner in a
terminal evidence failure. Neither path offers retry or owner extraction.

Only `LiveDatabase` exposes shared and mutable borrows of its retained owner.
There is no consuming extraction; issue #186 must consume the live owner through
the reviewed close transition.

## Compatibility Context

The filesystem and memory composition roots each require one caller-constructed
`CompatibilityContext` by value. They move that exact immutable value through
recovery into the live outer owner and expose only a shared borrow.

Recovery itself has no target-specific behavior. The context is retained so
later request composition can open branded scopes from the same selected target
without reconstructing selectors. There is no process-global context, implicit
default, baseline target, environment lookup, or version conditional in the
recovery path.

`ntsql-database` does not depend on `ntsql-compatibility`; target ownership stays
at the outer composition root.

## Crate and Dependency Boundary

The exact completion type is the non-forgeable proof accepted by the database
gate. This decision therefore amends ADR 0062's provisional prohibition on
database-domain transaction policy and adds one narrow repository-owned edge:

```text
ntsql-database -------> ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-database ------------------------------------------> ntsql-wal

ntsql-storage-file ---> ntsql-compatibility
ntsql-storage-file ---> ntsql-database
ntsql-storage-file ---> ntsql-page
ntsql-storage-file ---> ntsql-transaction
ntsql-storage-file ---> ntsql-wal

ntsql-storage-memory -> ntsql-compatibility
ntsql-storage-memory -> ntsql-database
ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

`ntsql-database` continues to reject dependencies on compatibility selection,
contracts, diagnostics, serialization, and persistence adapters. Transaction
and database code remain I/O-free. The storage adapters own physical/model
state, context retention, and composition-root error projection.

## Filesystem Lock Continuity

Filesystem open preserves ADR 0065's order:

1. database-owner control;
2. selected manifest;
3. selected WAL;
4. selected page store; and
5. selected restart-checkpoint control.

The WAL, page, and checkpoint values move into transaction recovery while the
database-owner and manifest files remain held by the outer owner. No phase
closes, clones, reopens, replaces, or reacquires a descriptor. Success retains
all five locks. Recovery failure retains the two outer locks plus the exact
transaction failure owner that holds the three child locks. Evidence mismatch
retains all five completed owners.

Ordinary open selects only the exact final paths. Missing, partial, corrupt,
unsupported, foreign, swapped, aliased, contradictory, or unpublished
candidate evidence fails before recovery or remains terminal at its exact
content-validation phase. No alternate path is searched and no candidate is
adopted.

## Memory Parity

The memory composition root first acquires the existing modeled five-object
owner and exact stable child observations. The recovery input supplies concrete
`InMemoryCommitLog`, `InMemoryPageStore`, and completeness-source owners.

Those concrete values pass through the same transaction orchestration as the
filesystem values. Source/store lineage, checkpoint identity, selected content,
repair results, completion evidence, and the final persistent-WAL cross-check
are validated by the same domain gates. Modeled observations cannot synthesize
completion evidence or release Live by themselves.

Failure retains both the modeled database guard and the exact concrete
transaction owner until drop.

## Error and Diagnostic Boundary

Filesystem I/O, memory faults, transaction evidence contradictions, repair
outcomes, checkpoint publication uncertainty, and internal owner types remain
startup/storage errors. This decision adds no `ClientDiagnostic`, protocol
token, wire error, backtrace field, or transport mapping.

## Test Boundaries

- Transaction tests cover absent bootstrap, selected success, initial rejection
  without fallback, every owning failure stage, and exact phase order.
- Filesystem tests cover new empty composition open, nonempty selected replay,
  all-lock contention after Live, candidate non-selection, structural
  contradictions, injected recovery failures, repeated fresh open, and
  child-process exit after every reported phase.
- Memory tests cover the same success/failure phase contract, exact target
  retention, concrete/model lineage contradiction, contention, and repeated
  fresh compositions.
- Compile-fail tests prevent numeric or decoded evidence from constructing
  Live, prevent recovery bypass, and prevent owner extraction from success or
  failure states.
- Architecture tests admit only the dependency edges recorded above and retain
  negative checks for reverse adapter and external-policy dependencies.

## Non-Goals

This decision does not add clean-close publication, tombstones, drop, online
recovery, concurrent sessions, background reclamation, automatic WAL
replacement, allocation, buffering, protocol login, client diagnostics, native
Microsoft startup behavior, or a performance shortcut around structural or
content validation.
