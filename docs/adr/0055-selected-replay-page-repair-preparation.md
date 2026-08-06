# ADR 0055: Selected Replay Page-Repair Preparation

- Status: Accepted
- Date: 2026-08-06
- Issue: #163
- Extends: ADR 0033, ADR 0037, ADR 0047, ADR 0053, ADR 0054
- Follows: #161

## Context

ADR 0054 owns one exact selected-checkpoint replay window and the complete-current
transaction analysis from the same stable WAL callback. Its private full-image
records are sufficient to avoid re-entering the WAL, but they are not yet page
repairs. Startup must still determine:

- which transaction-owned images have an exact later commit;
- which eligible image is physically latest for each replay page;
- whether the selected checkpoint already describes a sufficient current image;
- whether the page store still has the exact source state previously selected; and
- which exact source/target pairs would require a later atomic replacement.

Deriving those answers while writing would mix deterministic evidence or
allocation failure with outcome-indeterminate page-store effects. Such a mixed
operation could not safely fall back to ADR 0033 complete recovery after an error:
some pages might already have changed.

This decision therefore adds a consuming, read-only preparation transition. It
privately retains one ordered decision for every page represented in the owned
replay window. It invokes no effectful port and grants no repair permit.

## Crate and Dependency Boundary

`ntsql-transaction` owns the generic transition, private source/target evidence,
prepared and failed states, and typed errors. The existing memory and filesystem
adapters exercise the transition through the unchanged read-only page snapshot
port:

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

No crate, direct dependency edge, architecture registration, persistent byte,
WAL or page-store format, checkpoint slot, checksum, marker, path, lock primitive,
source port, write port, synchronization operation, or recovery algorithm changes.
The transaction domain remains I/O-free.

## Consuming Read-Only Transition

`PlannedTransactionPageStorageRestartCheckpointReplay::
prepare_page_repairs(self)` consumes the only ADR 0054 plan. It is available when
the retained store implements `DurablePageStoreSnapshotSource<N>`.

The transition uses only:

- the privately retained selected completeness baseline;
- the privately retained complete-current transaction analysis;
- the privately owned replay observations; and
- shared page-store observations.

It does not call `with_durable_transaction_restart_observations` or any other WAL
projection. It does not append, flush, reopen, reload the checkpoint, publish,
invoke committed-page recovery, or invoke a page-store write. The generic bound
requires only the read-only snapshot trait, so the implementation cannot call the
mutable recovery replacement port.

Preparation first clones the retained analysis and store lineages. A mismatch
fails before allocation or store observation. Adapter stability between ADR 0054
planning and this transition comes from the private unrecovered owner and any
adapter-lifetime locks, not from a new WAL callback.

## Inventory, Allocation, and Order

The transition fallibly reserves a page-number inventory with the replay-record
count as its upper bound. It inserts page numbers from raw and transaction-owned
records, ignores commits, sorts numerically, and deduplicates.

It then fallibly reserves the exact decision count and creates one sorted private
target slot per page. One physical-order pass over the replay records locates the
slot by binary search and replaces it whenever that page has a later eligible
image. Exact commit positions and checkpoint-required image positions are found
by binary search over the already validated physically ordered replay records.
The implementation does not rescan the complete window for every page or commit.

After target derivation it validates every checkpoint-dirty required image
against the owned replay records and combines each target with its selected
checkpoint source expectation. All inventory capacity, decision capacity,
retained commit relation, and dirty-image ownership checks complete before the
first store observation.

Only after that precomputation does preparation observe each inventoried page
exactly once in strict `PageNumber` order. A page that exists only in the store
and has no raw or transaction-owned replay record is neither inventoried nor
observed.

The implementation uses `try_reserve_exact`; capacity errors retain the attempted
record or page count. Count-only helpers make `usize::MAX` failures testable
without constructing impossible replay data.

## Eligible Replay Targets

Target derivation scans the already ordered owned replay window in physical order.
It never compares `PageVersion` to determine recency.

- A raw full-image record is always eligible.
- A transaction-owned full-image record is eligible only when the exact persisted
  owner is committed in the retained complete-current analysis and the owned
  replay window contains the exact matching commit at that retained commit
  position physically after the image.
- An uncommitted transaction-owned image is not eligible.
- The last eligible image for the page is the replay-derived target.

The complete-current transaction entry cannot substitute for the commit record.
Requiring both catches a malformed retained relationship and ensures the prepared
target owns all image and commit evidence needed by a later executor.

A raw target privately owns its page number, version, full `[u8; N]` image, and
lineage-bound page position. A committed target also owns the persisted
transaction identity and matching commit position.

## Combining the Selected Checkpoint

The selected baseline and replay target are combined per inventoried page:

- `NoRequiredImage` expects the page to remain absent unless a later eligible
  replay target exists.
- `StoreMissing` expects absence and requires its checkpoint-required image to be
  present exactly in the owned replay window.
- `StoreBehind` expects the exact selected stored position and likewise requires
  the checkpoint-required target image to be owned.
- `StoreCurrent` expects the exact selected stored position. It remains unchanged
  when no replay target is physically later than the checkpoint-required image;
  otherwise the later replay image becomes the target.
- A page absent from the selected baseline is suffix-only and derives its source
  solely from current store observation plus owned replay evidence.

ADR 0047's replay floor makes an exact required image for `StoreMissing` or
`StoreBehind` expected to be present. Its absence is nevertheless a typed
defensive failure rather than an unchecked assumption.

Checkpoint source snapshots can predate the replay window. Their payload was
validated immediately before ADR 0054 materialized the plan, and the private owner
and locks prevent safe caller mutation. Preparation therefore requires their
exact selected stored position. When that position also appears in the replay
window, it additionally revalidates version and bytes against the owned record.

## Snapshot Validation and Resolutions

Each returned snapshot must first name the requested page and retained lineage.
Preparation then requires the selected-checkpoint source expectation:

- selected missing remains absent;
- selected current or behind remains at its exact stored position; and
- suffix-only may be absent or use an owned replay-backed position.

A suffix-only snapshot must be backed by an exact owned raw or eligible committed
transaction page record with identical version and bytes. An uncommitted backing
record is a contradiction.

After source validation, every page receives exactly one private resolution:

- `NoRequiredImage`, when no eligible image is required and the source is absent;
- `CheckpointCurrent`, when the exact selected current image remains sufficient;
- `AlreadyCurrent`, when the current snapshot exactly equals the replay target;
  or
- `Candidate`, retaining exact source absence/snapshot and the exact full-image
  target for a later executor.

A snapshot at the target position with different payload is rejected. A snapshot
physically after the target is rejected rather than treated as current or behind.
A valid earlier source becomes a candidate only after its replay/checkpoint
backing relationship is validated.

`AlreadyCurrent` is expected principally for suffix-only pages that were flushed
after the selected prefix, because ADR 0053 did not observe them. It remains an
exact defensive classification for every page.

## Prepared Ownership and Memory Cost

Successful preparation returns
`PreparedTransactionPageStorageRestartCheckpointRepairs`. It privately retains:

- the complete original ADR 0054 plan;
- the unrecovered WAL and page store;
- the selected checkpoint source, lock, and publisher capability;
- the selected baseline and complete-current analysis;
- every owned replay record; and
- one ordered private resolution per replay page.

It exposes only persistent ID, checkpoint/current frontiers, page count, and
counts for no-required, checkpoint-current, already-current, and candidate
resolutions. It exposes no page number, record, image, source snapshot, target,
analysis, baseline, adapter, or checkpoint source.

The correctness-first representation copies each target's fixed-width image into
its private decision while ADR 0054's original record remains retained. A target
image is therefore temporarily owned twice. This simplifies exact source/target
ownership for the next consuming boundary at a per-target `N`-byte cost.

This decision makes no bounded-memory, streaming, spilling, or throughput claim.
A later private representation may remove the duplicate only if it preserves
fallible allocation before observation, the single physical-order target pass,
complete original-plan ownership, and nonextractable exact target evidence.

## Failed Ownership and Explicit Fallback

Any preparation error returns
`FailedTransactionPageStorageRestartCheckpointRepairPreparation`. It privately
retains the complete original ADR 0054 plan and exact error. It exposes only
`error()` and has no retry, partial decision, adapter accessor, or direct recovery.

Because preparation invokes no write, both outcomes can safely choose unchanged
full recovery:

- `Prepared::decline_page_repairs` destroys every decision, copied target, replay
  record, current analysis, and selected baseline before returning ADR 0053's
  uncheckpointed full-recovery owner.
- `Failed::continue_with_full_recovery` destroys the same private plan and returns
  both that uncheckpointed owner and the exact preparation error.

Neither transition can retain a prepared target while obtaining ADR 0033 recovery
authority. Complete recovery then re-inventories and resolves the current durable
source through its existing independent rules.

## Error Priority

`DurableTransactionRestartCheckpointRepairPreparationError` distinguishes:

- retained analysis/store lineage mismatch;
- read-only store observation failure with exact page number; and
- boxed retained-evidence or snapshot contradiction.

The evidence error distinguishes:

1. inventory and decision capacity exhaustion;
2. a replay page absent from the sorted inventory derived from that same window;
3. replay transaction absent from complete-current analysis;
4. committed replay page without its exact later owned commit;
5. selected dirty required image absent from the owned window;
6. unexpected snapshot page or foreign lineage;
7. changed selected-checkpoint source state;
8. suffix snapshot without an owned backing record;
9. eligible backing record omitted from target derivation;
10. snapshot payload contradiction;
11. snapshot backed by an uncommitted transaction; and
12. snapshot physically after the selected target.

Pre-observation evidence and allocation errors precede every adapter call.
Observation and validation errors then follow strict page-number order. `Display`
identifies the stage and `Error::source` preserves the exact nested adapter or
evidence cause. No failure becomes absence or an empty success.

## Filesystem Lock Continuity

The ADR 0052 acquisition order remains:

1. transaction-page WAL;
2. page store; and
3. completeness control.

Moving the same values from `Planned` to `Prepared` or `Failed` does not close,
clone, reopen, replace, or unlock a descriptor. All three locks remain held during
ordered page observation, failure inspection, and explicit fallback. The
fallback retains them through complete recovery and restart analysis.

The locks remain cooperative advisory locks. This decision adds no waiting,
atomic three-object acquisition, database-wide lock, hostile path defense, or
unsupported-filesystem guarantee.

## Authority Boundary

No preparation outcome, accessor, error, private decision, source snapshot,
target, replay record, analysis, or selected baseline can directly create or
substitute:

- transaction lifecycle or coordinator state;
- dirty, clean, live-permitted, or recovery-permitted pages;
- ordinary page-write or committed-page recovery permits;
- a page repair, compare-and-replace attempt, or partial retry;
- recovered or restart-analyzed live storage;
- checkpoint publication permits or receipts;
- WAL append, durability, retention, truncation, or reclamation authority; or
- native format or external compatibility evidence.

Private fields prevent prepared/failed construction and extraction of records,
source/target evidence, analyses, baselines, adapters, or checkpoint source.
Compile-fail tests also reject direct recovery, capability substitution,
transaction restoration, and WAL retention/reclamation.

## Adapter Integration

Memory integration prepares two replay pages: one uncommitted page has no required
image and one later committed suffix page is an exact repair candidate. The
checkpoint source remains owned, the WAL record count remains unchanged, and
explicit fallback alone performs existing complete recovery.

Filesystem integration reopens the three-object composition over one
store-current checkpoint and one missing committed suffix page. Preparation
returns one candidate, the page-store file bytes remain unchanged, and independent
open attempts prove all three locks remain continuously held. Explicit fallback
then repairs the suffix through the unchanged recovery path.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, page, WAL, recovery,
restart-analysis, completeness, checkpoint, ownership, lock, and fault contracts.
No external product documentation, driver, SDK, fixture, oracle, proprietary
governance tool, or native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server replay or repair algorithm, checkpoint table,
dirty-page table, recovery phase, LSN, error, diagnostic, persistent format, or
compatibility behavior.

## Test Boundaries

- Empty replay produces no decisions and no store observation.
- Store-only pages absent from replay are not enumerated or observed.
- Raw, committed, uncommitted-only, and mixed raw/committed histories derive
  targets by physical order, never page version.
- A transaction target requires both complete-current committed state and the
  exact later owned commit record.
- Checkpoint no-required, missing, behind, and current states combine with replay
  targets into exact deterministic resolutions.
- Suffix-only absent, behind, and target-current snapshots become candidate or
  already-current only with exact owned backing.
- Unexpected page, foreign lineage, changed checkpoint source, unbacked snapshot,
  payload contradiction, uncommitted backing, and after-target snapshot fail
  distinctly.
- Dirty-image, commit-relation, and capacity failures occur before the first store
  observation.
- Store failures retain the exact ordered page and nested cause.
- Prepared and failed fallbacks retain the source and invoke only unchanged full
  recovery.
- Compile-fail tests reject construction, evidence/adapters escape, direct
  recovery, permits, transaction restoration, and WAL authority.
- Memory and filesystem integrations prove no preparation write and filesystem
  three-lock continuity.
- Existing recovery, restart analysis, completeness, selection, planning,
  publication, codec, adapter, lock, architecture, and governance tests remain
  valid.

## Non-Goals

This ADR does not:

- invoke a page-store write or define the private effectful repair permit;
- add an atomic compare-and-replace repair port or its outcome-indeterminate
  failure semantics;
- execute redo, undo, rollback, abort, compensation, or transaction restoration;
- construct the final recovered/restart-analyzed live owner;
- enumerate or reconcile store-only pages;
- invalidate, quarantine, delete, or republish the selected checkpoint;
- choose a retention floor or truncate, compact, reclaim, or rewrite WAL;
- add streaming, spilling, batching, bounded-memory, or duplicate-image indexing;
- change persistent bytes, locks, synchronization, or external contracts; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql can now consume one exact selected replay window into deterministic,
page-number-ordered, fully owned source/target repair decisions without another
WAL projection or any persistent effect. Every preparation failure can still
destroy the plan and enter unchanged complete recovery safely.

The prepared owner remains private and non-authorizing. Separately reviewed work
must introduce a one-attempt repair permit and atomic compare-and-replace port
with explicit indeterminate-write semantics before any candidate can mutate the
page store. Transaction restoration, final live ownership, retention, and WAL
reclamation remain later boundaries.
