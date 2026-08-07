# ADR 0034: Non-Authorizing Durable Restart Analysis

- Status: Accepted
- Date: 2026-08-06
- Issue: #120
- Extends: ADR 0023, ADR 0025, ADR 0033
- Extended by: ADR 0035, ADR 0036, ADR 0037, ADR 0038, ADR 0047, ADR 0054, ADR 0057,
  ADR 0059, ADR 0060

## Context

ADR 0033 makes committed-page recovery a consuming storage-ownership transition.
Only a complete deterministic page pass releases the WAL and page store for live
use. That transition still reports pages rather than reconstructing transactions.
It cannot say which persisted transaction identities committed, which have only
durable owned-page records, or what durable prefix was analyzed.

A future checkpoint cannot safely choose a restart boundary from separate page
and commit collections. Separate collections can each be ordered while hiding a
cross-kind position collision or an inconsistent interleaving. They also omit
raw page records whose positions still occupy the shared logical WAL sequence.

The smallest next step is an I/O-free, non-authorizing restart analysis over one
unified complete durable logical-record stream. It constructs point-in-time
transaction metadata only. It adds no concrete adapter projection, checkpoint
record, dirty-page table, replay, undo, or log reclamation.

## Crate and Dependency Boundary

`ntsql-transaction` owns the source port, unified observations, validation,
transaction table, and errors. Its reviewed direct dependency set remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal
```

No adapter type enters the domain crate. No crate, dependency edge, persistent
format, or architecture registration changes.

## Unified Logical Observation Stream

`DurableTransactionRestartObservation<N>` represents exactly one logical record
using one of three variants:

- `Page`, containing an existing nontransactional
  `DurablePageWalObservation<N>`;
- `TransactionPage`, containing an existing
  `DurableTransactionPageObservation<N>`; or
- `Commit`, containing an existing
  `DurableTransactionCommitObservation`.

The variants cover every logical record currently exposed by the
repository-authored memory and filesystem transaction/page WALs. A raw page is
included even though it has no transaction-table entry because its position is
part of the same global order and may be the durable tail.

Every nested observation has already validated its nonzero position and exact
page or transaction fields. The unified enum adds only a common `kind` and
`position` view. It creates no new lifecycle or page capability.

## Stable Complete-Prefix Source

`DurableTransactionRestartAnalysisSource<N>` exposes:

1. the exact source `LogLineage`; and
2. one higher-ranked callback receiving an optional durable frontier plus a
   borrowed slice of unified observations.

The implementation must project every durable logical record exactly once in
strict physical order and keep the prefix unchanged for the callback's complete
duration. The higher-ranked evidence lifetime prevents the borrowed frontier or
stream from entering the owned result.

`None` has one meaning: the logical-record prefix is authoritatively empty. A
nonempty stream must supply `Some` with the exact last durable logical-record
position. Epoch-allocation frames, physical data frames, and durable marker
frames are format implementation details rather than logical records and do not
appear in this stream.

Completeness, correct variant projection, stability, and callback-result honesty
remain trusted port contracts. The domain can validate the supplied shape and
tail but cannot prove that a defective source did not omit or relabel a record.

## Frontier and Global-Order Validation

The source lineage is cloned before entering the callback. Validation then
proceeds in this order:

1. if a frontier exists, reject a foreign lineage before numeric comparison;
2. reject a zero nonempty frontier;
3. reject `None` with observations and `Some` without observations;
4. for every observation, reject a foreign lineage before comparing positions;
5. require every numeric position to advance strictly beyond its predecessor;
6. at an equal position, distinguish a full identical duplicate from
   contradictory evidence, including a cross-kind collision;
7. reject a numerically decreasing position; and
8. require the final observation position to equal the supplied frontier.

Position gaps are valid. This decision does not require dense allocation and
does not infer a missing record solely from a numeric gap.

The complete stream shape is validated before transaction-table reservation or
transaction-specific checks. A later foreign, duplicate, contradictory, or
decreasing record therefore cannot be hidden by an earlier commit.

For a valid empty stream, analysis returns the exact lineage, no frontier, and
an empty transaction table without allocating a table.

## Deterministic Transaction Table

`DurableTransactionRestartAnalysis` owns:

- the exact analyzed lineage;
- the exact optional durable frontier; and
- `DurableTransactionRestartEntry` values in strict persisted-identity order.

Each entry owns:

- one `DurableTransactionIdentityObservation`;
- the optional first and last owned-page WAL positions;
- the exact owned-page record count; and
- `Uncommitted` or `Committed { commit_position }`.

A raw page participates in global validation but creates no transaction entry.
A transaction-owned page creates or updates its owner's first/last range and
count. Repeated owned-page records, including repeated page numbers, are valid
before a commit because this analysis does not introduce a persistent uniqueness
rule beyond existing observation contracts.

One commit changes an existing page owner to `Committed`, or creates a valid
commit-only entry with no page range and count zero. Such an entry describes a
durable transaction that changed no transaction-owned page in this baseline; it
does not claim that no other future WAL record kind belonged to the transaction.

A second commit for one identity is contradictory. An owned-page record after
that identity's commit is also contradictory. Strict global stream order proves
that a valid commit position is after every owned-page position retained in the
same entry.

Entries remain sorted by numeric persisted `(epoch, sequence)` identity through
reserved-vector binary search and insertion. Identity order is output
determinism only; it never substitutes for WAL physical order.

## Allocation and Complexity Boundary

After the complete stream shape and frontier validate, analysis fallibly
reserves transaction capacity equal to the total logical record count. Every
possible entry is caused by at least one supplied record, so this is a complete
upper bound. All later vector insertion uses that reservation.

No `BTreeMap`, `HashMap`, per-entry page vector, or success-shaped allocation
fallback is used. The owned-page count advances with checked arithmetic. A
capacity or count failure is typed.

Maintaining the sorted vector uses binary search plus insertion. Search is
logarithmic in the current transaction count, but insertion may shift entries,
so construction is worst-case quadratic in the number of distinct identities.
This correctness-first baseline makes no throughput claim. A later index may
replace it only behind the same deterministic and fallible-allocation contract.

The output deliberately omits page numbers, versions, and images. Those remain
in the source observations and existing page-recovery path. This table is
transaction restart metadata, not a dirty-page table or redo plan.

## Error and Source-Result Boundary

`DurableTransactionRestartAnalysisError` distinguishes:

- an exact source failure; and
- one boxed `DurableTransactionRestartAnalysisEvidenceError`.

Boxing keeps the public `Result` error bounded while preserving the exact nested
frontier, position, identity, and transaction contradiction. `Error::source`
retains either cause.

The callback performs no physical mutation. If a source invokes the callback
and later returns a source error instead of its output, the source error remains
authoritative. Unlike ADR 0029, there is no attempted store effect whose result
must override that error.

Within valid global stream shape, transaction-specific validation stops at the
first duplicate commit, post-commit page, or count failure in physical record
order. No partial table is returned.

## Ownership and Authority Boundary

Observations, entries, states, and complete analysis are immutable
point-in-time data. Safe code cannot convert them into:

- `TransactionId`, `ActiveTransaction`, `CommittedTransaction`, or another
  coordinator lifecycle token;
- `DirtyPage`, `TransactionDirtyPage`, `CleanPage`, or
  `TransactionCleanPage`;
- a live `PageWritePermit` or recovery-only committed-page permit;
- a stable-prefix callback, source, store, or recovered storage owner;
- a redo, undo, rollback, abort, or compensation command;
- a checkpoint record or checkpoint-validity proof; or
- log flush, truncation, or reclamation authority.

The exposed lineage-bound positions are inert references or owned metadata. The
complete analysis itself is not a `LogSequenceNumber` and cannot be passed to a
durability port.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, page, WAL, recovery, and
ownership contracts. No external product documentation, driver, SDK, fixture,
oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format is
consulted.

This decision defines no SQL Server transaction table, checkpoint, analysis
pass, LSN, recovery phase, error, diagnostic, startup order, or compatibility
behavior.

## Test Boundaries

- An authoritatively empty prefix returns the exact lineage and no entries.
- A gapped, interleaved raw/owned/commit stream yields identity-sorted committed,
  uncommitted, and commit-only entries with exact page ranges and counts.
- Repeated pre-commit owned pages remain valid.
- Foreign and zero frontiers fail before shape validation.
- Missing and unexpected frontiers fail explicitly.
- A tail/frontier mismatch retains both exact positions.
- Foreign records, identical duplicates, cross-kind contradictory positions, and
  decreasing positions fail distinctly.
- Complete global shape validation takes priority over an earlier duplicate
  transaction commit.
- Duplicate commits and post-commit owned pages retain exact identities and
  positions.
- Source failures before and after the callback remain exact nested causes.
- Compile-fail tests prevent evidence escape and conversion into lifecycle,
  recovery-write, or log-durability authority.
- Existing transaction, committed-page recovery, ownership, adapter, format,
  poison, repair, and architecture tests remain unchanged.

## Non-Goals

This ADR does not:

- implement the source port in memory or filesystem adapters;
- bind analysis to `RecoveredTransactionPageStorage`;
- add or change a WAL/page-store header, frame, marker, checksum, repair, poison,
  or lock contract;
- persist a checkpoint or validate a prior checkpoint;
- build a dirty-page table, replay plan, transaction coordinator, or page index;
- execute redo, undo, rollback, abort, compensation, or store mutation;
- choose a replay start, retention floor, truncation boundary, or reclaim a log;
- make raw page APIs transactional or decide raw/store-only recovery policy;
- define database lifecycle, multi-process startup, online recovery, buffering,
  eviction, allocation, isolation, or force-at-commit; or
- define external SQL Server values or native file compatibility.

## Consequences

The transaction domain can now validate one complete unified durable logical
prefix and reconstruct deterministic committed/uncommitted transaction metadata
without granting replay or persistence authority.

ADR 0035 implements one-pass stable-prefix projection in the memory WAL adapter,
and ADR 0036 implements the same contract under the filesystem WAL's lifetime
lock. ADR 0037 makes successful analysis the consuming transition from
page-recovered ownership to live storage without granting the analysis any
runtime authority. Persisted checkpoints, dirty-page analysis, replay start,
undo, and log reclamation remain separately reviewed work.
