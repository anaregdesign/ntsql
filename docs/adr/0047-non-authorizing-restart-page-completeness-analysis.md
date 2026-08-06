# ADR 0047: Non-Authorizing Restart Page-Completeness Analysis

- Status: Accepted
- Date: 2026-08-06
- Issue: #146
- Extends: ADR 0032, ADR 0034, ADR 0037, ADR 0046

## Context

ADR 0046 publishes a transaction-only restart checkpoint baseline. It explicitly
cannot shorten restart replay or authorize WAL reclamation because it contains no
page-store completeness evidence or replay lower bound.

Deriving those fields from separate calls is unsafe. The transaction table, WAL
page inventory, page-store snapshots, and candidate replay start could describe
different moments if the source advances between calls. Forcing only the pages
selected by ADR 0032 is also insufficient: that inventory intentionally excludes
raw page records, and a transaction uncommitted at the analysis frontier may
commit later while depending on an earlier page image.

The smallest next step is one read-only, non-authorizing analysis under one
stable complete-WAL callback. It combines the existing ADR 0034 transaction
table with deterministic page classifications and an inert numeric replay lower
bound. It does not persist those fields, select a checkpoint for startup, replay
records, or reclaim WAL.

## Crate and Dependency Boundary

`ntsql-transaction` owns the read-only page-store port, evidence model,
validation, and owning operation. Existing memory and filesystem adapters
implement the port and test the same domain behavior:

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

No crate, dependency edge, architecture registration, WAL format, page-store
format, checkpoint format, file, frame, marker, checksum, fault point, or
synchronization protocol changes.

## Read-Only Page-Store Port

`DurablePageStoreSnapshotSource<N>` owns only:

- the exact protected `LogLineage`;
- an associated observation error; and
- `observe_page(&self, page_number)`.

`CommittedTransactionPageRecoveryStore<N>` now extends that trait and retains
its separate mutable `compare_and_replace` method and write error. Memory,
filesystem, and test implementations split their existing observation and write
implementations accordingly.

The completeness operation requires only the read-only trait. Its generic bound
cannot invoke recovery replacement, and the operation holds the store through a
shared borrow. Safe code using the same owner therefore cannot mutate the store
during the source callback. Interior mutation, non-cooperating writers, and
adapters that do not keep observations stable under their declared ownership
remain violations of the trusted port contract.

## One Stability Window

`RestartAnalyzedTransactionPageStorage::analyze_current_restart_completeness`
first clones both adapter lineages and rejects a mismatch before invoking either
source. It then calls
`with_durable_transaction_restart_observations` exactly once.

Inside that callback, in order, the operation:

1. applies the complete-stream and transaction-table validation from ADR 0034;
2. derives every distinct page number represented by a raw or
   transaction-owned full-image record;
3. sorts and deduplicates that inventory by numeric page number;
4. observes each inventoried store page exactly once;
5. validates and classifies each snapshot; and
6. derives one replay start from the transaction and page tables.

The callback's higher-ranked evidence lifetime still prevents the WAL slice or
frontier from escaping. The shared store borrow remains live for the callback's
duration. The result owns only cloned, adapter-neutral metadata.

This inventory covers every page represented in the complete durable WAL
stream. It does not enumerate store-only pages because the read-only port has no
store inventory. Before any future WAL reclamation, a separate reviewed boundary
must either retain the complete backing history assumed here or add authoritative
store inventory and reconciliation.

## Required Full-Image Rule

For each inventoried page, the latest required image is the greatest WAL
position among:

- every raw full-image page record; and
- every transaction-owned full-image record whose owner is committed at the
  analyzed frontier.

The validated stream is already in strictly increasing physical order, so one
forward pass can retain the latest eligible record. `PageVersion` is payload
metadata and is never used for recency. A later raw image supersedes an earlier
committed image, and a later committed image supersedes an earlier raw image.

An uncommitted transaction-owned image is not currently required because its
owner has no durable commit at this frontier. Its transaction still contributes
to the replay lower bound so a later commit cannot make its pre-frontier page
history unreachable.

The rule relies on the repository's current complete full-image WAL contract. It
does not infer partial-page deltas, store-only history, or a native database
dirty-page algorithm.

## Snapshot Validation and Page States

Every returned snapshot is validated before it can influence classification:

1. its page number must equal the requested page;
2. its required position must belong to the analyzed lineage;
3. the position must not exceed the durable frontier;
4. an exact raw or transaction-owned full-image record must back the position;
5. page version and bytes must match that backing record; and
6. a transaction-backed snapshot must belong to an owner committed at the
   frontier.

The sixth rule preserves the existing no-steal boundary. A snapshot backed by an
uncommitted transaction image is a contradiction rather than a current or
behind page.

Each page then receives exactly one state:

- `NoRequiredImage`: only uncommitted transaction images exist and the store is
  absent;
- `StoreMissing`: a required image exists and the store is absent;
- `StoreCurrent`: the snapshot position equals the latest required image; or
- `StoreBehind`: a valid snapshot precedes the latest required image.

A valid snapshot without a required image and a snapshot after the selected
required image are defensive contradictions. Missing backing records, payload
contradictions, foreign lineage, and beyond-frontier snapshots are also explicit
failures. No success-shaped fallback is returned.

## Replay-Start Derivation

`DurableTransactionRestartReplayStart` is either:

- `AfterFrontier { frontier }` when no earlier record is required; or
- `AtPosition { position, cause }` for the minimum exact inclusive position
  required by current evidence.

The inclusive floor considers:

- the required image position of every `StoreMissing` page;
- the required image position of every `StoreBehind` page; and
- the first owned-page position of every transaction uncommitted at the
  frontier.

The uncommitted floor is required even when all currently committed images are
in the store. That transaction may commit after the analysis; replay must then
still be able to find its earlier full images.

No `frontier + 1` value is constructed. Position gaps are valid, and a frontier
of `u64::MAX` is representable without overflow. `AfterFrontier` expresses a
strict relation, while `AtPosition` preserves the exact inclusive numeric floor
and deterministic cause.

Both forms are inert metadata. They are not lineage-bound runtime capabilities
and cannot be passed to WAL durability, replay, truncation, or reclamation
operations.

## Allocation and Complexity

Existing ADR 0034 analysis first fallibly reserves transaction capacity from the
logical record count. The completeness pass then:

- fallibly reserves a page-number inventory with the same record-count upper
  bound;
- sorts and deduplicates it;
- fallibly reserves the exact output page count; and
- scans the validated observation slice for each page to derive and validate
  evidence.

The correctness-first implementation is worst-case quadratic in the number of
logical records and pages. It performs no hidden infallible growth after the
declared reservations. Capacity failures retain the exact attempted bound.
A later index may improve complexity only behind the same ordering,
allocation-failure, and evidence contracts.

## Error Priority and Source Result

`DurableTransactionRestartCompletenessError` distinguishes:

- WAL/store lineage mismatch;
- exact restart-source failure;
- exact page-store observation failure with page number; and
- boxed domain evidence failure.

Lineage mismatch precedes the callback. Complete ADR 0034 stream validation
precedes page inventory allocation and every store observation. Inventory/table
allocation precedes page-number-ordered snapshot observation. Within a page,
required-image derivation precedes store observation and snapshot validation.

The callback is read-only. If a source invokes it and then returns a source
error instead of the callback result, the source error is authoritative. There
is no attempted store mutation whose indeterminate outcome could override it.
No partial table or replay start escapes any failure.

## Ownership and Authority Boundary

The operation exists only on the restart-analyzed owner, preserving both
adapter lifetimes and any filesystem locks. It re-analyzes the current stable
prefix; it does not reuse or mutate the immutable startup analysis already held
by that owner.

`DurableTransactionRestartCompletenessAnalysis`, its page entries, required
images, and replay start cannot create or substitute:

- transaction lifecycle tokens or coordinator state;
- dirty, clean, live-permitted, or recovery-permitted pages;
- a committed-page recovery write permit;
- recovered or restart-analyzed storage ownership;
- checkpoint publication permits, receipts, or validity;
- redo, undo, rollback, abort, or compensation commands; or
- WAL flush, retention, truncation, compaction, or reclamation authority.

Fields that bind the transaction table, page table, and replay start remain
private. Compile-fail tests cover direct construction and authority conversion.

## Adapter Integration

`InMemoryPageStore` and `FilePageStore` implement the shared snapshot port using
their existing exact stored-page observations. Recovery replacement remains in
the mutable recovery-store implementation.

Memory integration covers raw-only pages, earlier and later raw images,
committed current/missing/behind pages, uncommitted replay floors, deterministic
ordering, and unchanged store state. Filesystem integration creates real WAL and
page-store files, reopens them through the owning startup sequence, derives
completeness, and confirms that observation does not change the stored page
sequence. Existing ownership tests continue to prove both filesystem locks
remain held by the restart-analyzed owner.

## Evidence and Compatibility Boundary

All behavior derives from repository-authored WAL, page, transaction, recovery,
ownership, and adapter contracts. No external product documentation, driver,
SDK, fixture, oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK
format is consulted.

This decision defines no SQL Server checkpoint table, dirty-page table, analysis
pass, LSN, recovery phase, startup behavior, error, diagnostic, ordering, or
compatibility result.

## Test Boundaries

- Empty evidence yields empty transaction/page tables and
  `AfterFrontier { None }` without store observation.
- WAL and store lineage mismatch occurs before the source callback.
- Complete-stream failure occurs before every store observation.
- Raw, committed, and uncommitted page histories select the exact required image
  and deterministic page state.
- Later raw records supersede earlier committed records.
- Missing and behind pages lower replay to their required image.
- Every uncommitted transaction lowers replay to its first owned page.
- Snapshot page, lineage, frontier, backing record, payload, commit state, and
  required-image contradictions fail distinctly.
- Store failures retain the exact page and source; a post-callback source failure
  remains authoritative.
- The source callback and each distinct page observation occur exactly once.
- Memory and reopened filesystem adapters leave store state unchanged.
- Compile-fail tests reject mutation through the snapshot-only port, forged
  output, and conversion to page, recovery, publication, or WAL authority.
- Existing recovery, restart, checkpoint, adapter, format, lock, architecture,
  and governance tests remain valid.

## Non-Goals

This ADR does not:

- encode, persist, publish, load, or validate the completeness result;
- change the ADR 0044 checkpoint codec or ADR 0046 slot;
- make checkpoint presence or completeness a startup gate;
- execute redo, undo, rollback, abort, compensation, or page repair;
- restore transaction coordinator or runtime transaction state;
- enumerate or reconcile store-only pages;
- choose a checkpoint among generations or add fallback;
- truncate, compact, retain, or reclaim WAL;
- define database lifecycle, online checkpointing, concurrency with
  non-cooperating writers, or multi-process coordination; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql can now derive one coherent, deterministic transaction table, WAL-backed
page-completeness table, and exact replay lower bound from a single stable
durable-prefix window without granting write or recovery authority.

The result is intentionally transient. The next checkpoint slice must define a
versioned codec and publication contract for these additional fields before
startup can consume them, and a later reviewed recovery boundary must still
execute replay and establish any WAL-retention or reclamation authority.
