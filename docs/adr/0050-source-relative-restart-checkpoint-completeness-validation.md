# ADR 0050: Source-Relative Restart Checkpoint Completeness Validation

- Status: Accepted
- Date: 2026-08-06
- Issue: #153
- Extends: ADR 0039, ADR 0047, ADR 0048, ADR 0049
- Extended by: ADR 0051
- Follows: #151

## Context

ADR 0049 decodes integrity-checked `NTSQCMP1` bytes into an owned but
untrusted transaction/page/replay completeness observation. A valid checksum
proves only structural integrity: zero identities, duplicate or unordered
pages, false page-store classifications, inconsistent replay causes, and a
foreign or nonexistent WAL frontier can still be encoded deliberately or by an
untrusted writer.

ADR 0039 already re-derives one authoritative transaction-only baseline for an
exact decoded prefix and compares every field. That validator cannot be reused
unchanged for the wider completeness baseline: it has no page-store access and
never invokes ADR 0047's completeness derivation or ADR 0048's baseline
preparation. Reimplementing prefix selection for the completeness baseline
would duplicate ADR 0039's empty, future, numeric-gap, malformed-current, and
exact-boundary rules and risk divergent behavior between the two validators.

The smallest next step is one additional final-owner operation that shares
ADR 0039's selected-prefix boundary logic, then reuses ADR 0047's completeness
derivation and ADR 0048's baseline preparation unchanged. It is correctness-
first and snapshot-relative: it succeeds only when the selected WAL prefix and
the *current* page store re-derive the exact persisted transaction, page, and
replay fields. It must reject rather than infer historical persistence or
looser dominance whenever the current store no longer agrees.

## Crate and Dependency Boundary

Only `ntsql-transaction` production code and tests plus memory-adapter
integration change:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal
```

No crate, dependency edge, architecture registration, adapter port, file, byte
format, checksum, lock, fault point, or synchronization operation changes.
`ntsql-storage-file`'s codec module, `NTSQCKP1`/`NTSQCMP1` bytes, and the
checkpoint source/publisher ports are untouched. The transaction domain
remains I/O-free.

## Shared Selected-Prefix Logic

A private `select_restart_checkpoint_prefix` function now owns exactly the
boundary logic ADR 0039 already specified: a decoded `None` frontier selects
the canonical empty window `(None, &[])`; a decoded `Some(F)` rejects a
missing/foreign/zero/recordless current frontier, rejects `F` beyond the
current durable frontier, and otherwise scans for the first logical
observation at position `F`. When no exact boundary exists, it analyzes the
complete current stream to distinguish a valid numeric gap
(`CheckpointFrontierNotRecordBoundary`) from a malformed current stream
(`CurrentPrefix`), matching ADR 0039 exactly. On an exact boundary it returns
only the selected frontier and the WAL slice ending at that boundary; it does
not itself analyze that slice.

Both validators call this one selector and then run their own authoritative
analysis over the returned window:

- the transaction-only validator calls the existing
  `analyze_durable_transaction_restart_evidence` on the selected window,
  exactly as before refactoring;
- the completeness validator calls the existing
  `analyze_durable_transaction_restart_completeness_evidence` on the same
  selected window and the current shared store.

The selector's error type is the exact existing
`DurableTransactionRestartCheckpointBaselineValidationEvidenceError`, reused
unchanged rather than duplicated: `CurrentPrefix`,
`CheckpointBeyondDurableFrontier`, and `CheckpointFrontierNotRecordBoundary`
keep their existing shape, fields, `Display`, and `Error::source` regardless of
which validator produced them. Existing transaction-only validation behavior,
its public error variants, its callback count, and its suffix-isolation
guarantee are unchanged; every existing ADR 0039 test continues to pass
unmodified.

## Final-Owner Completeness Validation Operation

`RestartAnalyzedTransactionPageStorage::
validate_restart_checkpoint_completeness_baseline_against_current_prefix` is
available when the owned source implements
`DurableTransactionRestartAnalysisSource<N>` and the owned store implements
`DurablePageStoreSnapshotSource<N>` (the same bounds ADR 0047 already
requires). It accepts a borrowed
`DurableTransactionRestartCheckpointCompletenessBaselineObservation<'_>` and
returns only a newly re-derived
`DurableTransactionRestartCheckpointCompletenessBaseline` on exact success.

Before any callback or store observation it rejects, in order:

1. source/store lineage disagreement (the exact ADR 0047 `LineageMismatch`
   check);
2. ephemeral source lineage (reusing
   `CurrentPersistentLineageRequired`);
3. a decoded nested transaction observation with a zero or foreign persistent
   identity (reusing `ZeroPersistentLogId`/`ForeignPersistentLogId`); and
4. a decoded `Some(0)` frontier (reusing `ZeroCheckpointFrontier`).

Only after all four checks pass does it invoke the source's stable-prefix
callback, exactly once. Inside that one callback it:

1. selects the prefix with `select_restart_checkpoint_prefix`;
2. calls the existing `analyze_durable_transaction_restart_completeness_evidence`
   on exactly that selected frontier/slice and the current shared store —
   reusing ADR 0047's transaction analysis, page inventory, required-image
   selection, snapshot classification, and replay-floor derivation
   unchanged; then
3. calls the existing `prepare_restart_checkpoint_completeness_baseline` on
   the resulting analysis — reusing ADR 0048's baseline assembly unchanged.

No transaction analysis, page inventory, snapshot classification, replay
derivation, or portable count projection is reimplemented, and no second
callback or store pass occurs.

## Exhaustive Comparison and First Typed Mismatch

The re-derived authoritative completeness baseline is compared against the
decoded observation field by field, returning the first mismatch:

1. the nested transaction observation, via the exact existing
   `compare_restart_checkpoint_baseline_observation` helper — reusing ADR
   0039's frontier, transaction-count, transaction-order, and per-entry field
   comparison unchanged, wrapped as `NestedTransaction`;
2. the decoded page-table length, as `PageCountMismatch`;
3. each page entry in numeric order — raw page number, state discriminant,
   optional required-image kind/owner/positions, and optional stored
   position, each compared independently because the decoded observation
   keeps them as independent fields — as `PageEntryMismatch`; and
4. the replay-lower-bound kind, frontier, position, and cause, each compared
   independently for the same reason, as `ReplayMismatch`.

No partial baseline is returned from any stage. Success returns the complete
re-derived baseline, never a value built from decoded fields.

## Strict Snapshot-Relative Stale Behavior

An older selected WAL frontier may still validate successfully against an
unrelated later WAL suffix: the selector's boundary search ignores every
record after the selected position, so an unrelated raw page appended after
that boundary does not participate in either the transaction analysis or the
page inventory.

The store, however, has no historical view. If the *current* snapshot for any
selected-prefix page has since advanced — for example because a later commit
wrote a new image for a page the older checkpoint also covers — re-running
`analyze_durable_transaction_restart_completeness_evidence` on the *same*
selected frontier observes the *current* store snapshot and finds its
position beyond the selected frontier. ADR 0047's existing
`SnapshotBeyondFrontier` evidence failure already covers exactly this case;
this ADR does not add new detection logic for it. Validation therefore fails
closed automatically, through the same completeness-evidence path used for any
other selected-prefix contradiction. No behavior here infers an unobservable
historical store state or accepts a looser monotonic/dominance relation in its
place.

## Error Priority and Staged Errors

`DurableTransactionRestartCheckpointCompletenessBaselineValidationError<
SourceError, StoreError>` distinguishes, at the top level:

- `LineageMismatch`, checked before any callback;
- `Source`, an exact source failure whether it occurs before or after the
  callback runs — a post-callback source error is authoritative and overrides
  any callback-computed result, exactly as ADR 0039 and ADR 0047 already
  guarantee; and
- `Evidence`, one boxed
  `DurableTransactionRestartCheckpointCompletenessBaselineValidationEvidenceError`.

That inner evidence error distinguishes:

- `Baseline`, the exact reused ADR 0039 pre-callback identity/frontier
  rejection or selected-prefix rejection;
- `CompletenessEvidence`, the exact reused ADR 0047
  `DurableTransactionRestartCompletenessError` (selected-prefix analysis
  failure or one page-store observation failure with its page number);
- `BaselinePreparation`, the exact reused ADR 0048
  `DurableTransactionRestartCheckpointBaselineError`;
- `NestedTransaction`, `PageCountMismatch`, `PageEntryMismatch`, and
  `ReplayMismatch`, the new comparison-stage mismatches described above.

Every variant has a complete `Display` and `Error::source` chain back to its
nested cause. The generic `SourceError`/`StoreError` parameters carry only the
adapter's own error types through `Box`; neither is required to implement
`Clone` or `Eq`, matching the existing ADR 0039/ADR 0047/ADR 0048 pattern.

## Allocation and Complexity

This operation allocates nothing beyond what ADR 0047's completeness analysis
and ADR 0048's baseline preparation already allocate for the selected window;
comparison itself is allocation-free. It performs exactly one source callback
and exactly one page-store observation per distinct selected-prefix page,
matching ADR 0047's existing one-window guarantee.

## Authority Boundary

The decoded completeness observation, its nested page/replay observations,
this operation's errors, and the returned baseline cannot directly create or
satisfy the same authority list ADR 0039, ADR 0047, and ADR 0048 already
exclude: transaction lifecycle or coordinator state; dirty, clean, or
recovery-permitted pages; a committed-page recovery write permit; recovered or
restart-analyzed storage ownership; `LogLineage`, `LogSequenceNumber`, WAL
append, or durability fences; a checkpoint publication permit or receipt;
redo, undo, rollback, or compensation; and retention floors, truncation, or
reclamation.

The observation type's existing compile-fail tests already prevent it from
becoming the authoritative baseline or any of that authority. A new
compile-fail test additionally proves a detached observation cannot invoke
this validation or preparation operation itself; only the final restart-
analyzed owner exposes it.

## Evidence and Compatibility Boundary

All behavior uses repository-authored WAL, page, transaction, recovery,
restart-analysis, completeness, baseline, and storage-ownership contracts. No
external product documentation, driver, SDK, fixture, oracle, proprietary
governance tool, or native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server checkpoint validation, transaction/page
table, dirty-page table, LSN, recovery phase, persistent format, error,
diagnostic, or compatibility behavior.

## Test Boundaries

- Exact empty, exact current, and real stale prefixes re-derive
  byte-for-field equivalent authoritative completeness baselines, invoking
  exactly one source callback and exactly one store observation per distinct
  selected-prefix page.
- An unrelated later WAL suffix does not change validation of an older
  selected prefix; a subsequent advance of a *selected* page's current store
  snapshot fails validation of that same older prefix through
  `SnapshotBeyondFrontier`, proving strict snapshot-relative behavior rather
  than historical inference.
- Source/store lineage mismatch, ephemeral source lineage, zero/foreign
  decoded persistent identity, and zero decoded frontier fail before any
  callback or store observation, with zero callbacks and zero observations.
- Future, numeric-gap, and malformed/foreign/zero/missing current frontiers
  fail distinctly, reusing the exact ADR 0039 evidence variants.
- Nested transaction, page-count, page-entry (page number, state, required
  image, stored position), and replay (kind, frontier, position, cause)
  mismatches each return the first typed error with authoritative expected and
  exact decoded actual values.
- Source errors before and after the callback remain authoritative, and a
  page-store observation failure retains its exact page number and cause;
  validation succeeds again once one-shot faults clear, and the immutable
  startup analysis and page-store contents remain unchanged throughout.
- Compile-fail tests confirm a detached observation cannot call this
  operation itself and that the returned baseline remains bound by the
  existing ADR 0039/ADR 0047/ADR 0048 authority exclusions.
- Real memory-adapter integration proves exact current and safe-stale
  validation succeed without any WAL append or page-store write, and that
  advancing a selected page's current store snapshot fails the same older
  decoded checkpoint closed.
- Existing analysis, preparation, recovery, ownership, adapter, format,
  architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- add memory or filesystem completeness-checkpoint storage, publication,
  selection, reopen, repair, or quarantine;
- make completeness validation a startup gate or failure typestate;
- select among generations or add a digest, checksum, timestamp, marker,
  synchronization point, temporary file, or atomic rename;
- add dirty-page analysis, replay start, redo, undo, rollback, compensation,
  coordinator restoration, or active transaction reconstruction beyond what
  ADR 0047 already derives as inert metadata;
- choose a retention floor, truncate, compact, or reclaim a log;
- make an empty or exact baseline proof of current-source or current-store
  health beyond the exact fields it re-derives;
- infer an unobservable historical page-store state or relax the strict
  snapshot-relative comparison into a looser monotonic/dominance rule;
- replace complete-prefix validation needed by future recovery; or
- define external SQL Server values or native file compatibility.

## Consequences

A future adapter can decode complete checkpoint completeness fields and ask
the final restart-analyzed owner to prove that every transaction, page, and
replay field exactly matches the currently retained WAL prefix and current
page store, using the same boundary logic and error shapes already reviewed
for the transaction-only baseline.

The next separately reviewed work can define a completeness checkpoint
source/publisher pair and any startup consumption of a validated completeness
baseline. Replay execution, dirty-page repair, and WAL retention/reclamation
remain later boundaries; this decision grants none of them.
