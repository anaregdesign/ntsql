# ADR 0071: Transaction-Storage Clean-Reopen Validation

- Status: Accepted
- Date: 2026-08-08
- Issue: #186
- Extends: ADR 0058, ADR 0059, ADR 0068, ADR 0070

## Context

ADR 0070 can durably select a `Clean` Manifest V2 whose certificate identifies
the exact dedicated clean-close checkpoint published under ADR 0068. After all
owners are released, acquisition can decode that complete clean manifest, but
no current authority yet proves that its checkpoint still describes the
exclusive WAL and page-store owners being reopened.

The ordinary recovery-required checkpoint cannot supply that proof. In a
reclaimed WAL generation, physical generation metadata anchors the predecessor
checkpoint selected for crash recovery. The clean-close candidate is a later,
independent checkpoint and its frontier may equal the logical high-water even
when the physical record at that frontier has already been reclaimed. Comparing
the clean candidate's anchor with recovery metadata, or requiring its frontier
record to remain physically present, would reject a valid clean close.

Conversely, merely decoding the clean candidate or checking its fingerprint
would trust stale or contradictory transaction/page state. Clean reopen must
freshly prove that every transaction is terminal, every required page is
current, no replay is required, and the current allocator metadata agrees with
the candidate before a database layer can compare it with the manifest
certificate.

## Decision

### Consuming read-only validation

Add
`UnrecoveredTransactionPageStorage::validate_clean_reopen`. It consumes the
exclusive unrecovered WAL/page owner and a dedicated
`TransactionPageStorageCleanCloseCheckpointSource`.

The operation:

1. loads exactly one fresh clean-close checkpoint candidate;
2. observes the current physical WAL generation;
3. validates the candidate against fresh generation-appropriate WAL and page
   evidence;
4. requires clean-close semantic predicates;
5. observes and validates current allocator/retention metadata; and
6. returns the same non-forgeable `TransactionPageStorageCleanCloseProof` shape
   used to create a manifest certificate.

The operation is observational. It does not append, truncate, repair, replay,
publish, select, synchronize, or reclaim anything. It grants no database
lifecycle authority by itself.

### Generation-zero validation

Generation zero remains a complete logical prefix. Validation reuses the
complete-prefix checkpoint validator, which rederives transaction and page
completeness from the current WAL callback and page store and compares every
candidate field exactly.

### Reclaimed-generation validation

For a nonzero physical generation, validation keeps two anchors distinct:

- physical generation metadata continues to identify the predecessor recovery
  checkpoint; and
- the clean candidate supplies its own independently materialized anchor for
  later comparison with the clean manifest certificate.

The validator checks the physical generation metadata and its predecessor
anchor structurally, but does not compare that anchor with the clean candidate.
It materializes the clean candidate using the metadata's lineage and allocator
high-water, then requires the candidate frontier to equal the current logical
position high-water.

Inside one stable retained-suffix callback, it:

1. validates the retained boundary, high-water, lineage, ordering, and
   transaction epochs;
2. does not require the clean frontier record to be physically retained;
3. seeds current transaction analysis from the clean candidate;
4. checks every retained pre-frontier transaction page and commit against that
   candidate;
5. rederives the exact transaction baseline, requires every candidate page
   classified `StoreCurrent` to retain its exact current store position, and
   verifies payloads when the required record remains in the retained suffix;
   and
6. requires the rederived transaction baseline and revalidated candidate
   page/replay classification to equal the candidate exactly.

Physical generation metadata is reobserved after the stable callback and must
remain identical. Its allocator high-water must also equal the independently
observed retention metadata used by the returned proof.

### Clean semantics and exact proof

The validated baseline must contain:

- no uncommitted transaction entry;
- only `StoreCurrent` page entries; and
- an `AfterFrontier` replay classification.

The proof binds the persistent WAL identity, exact optional logical
high-water/frontier, allocator epoch high-water, versioned clean-candidate
anchor, and portable transaction/page counts.

`ValidatedTransactionPageStorageCleanReopen` privately retains the exact
unrecovered WAL/page owner, clean checkpoint source, validated candidate, and
proof. It exposes only borrowed proof evidence. It has no owner extraction,
recovery, replay, publication, or Live transition.

### Terminal failures

Every load, source, evidence, metadata, or semantic failure returns
`FailedTransactionPageStorageCleanReopenValidation`, which privately retains the
same WAL/page owner and clean checkpoint source together with the exact typed
cause.

Although validation has no publication effect and therefore needs no
outcome-indeterminate classification, failure remains terminal for that owner.
There is no same-owner retry or adapter extraction. A caller must drop the
failed owner and begin a fresh database-wide acquisition so the later database
gate cannot combine evidence from different ownership windows.

## Authority and Effect Ordering

The transaction-storage order is:

1. consume the unrecovered owner and clean checkpoint source;
2. load the clean candidate;
3. observe generation and current WAL/page evidence;
4. validate clean semantics;
5. observe allocator/retention metadata; and
6. construct the retained proof owner.

No failure releases an adapter or creates a proof. No callback evidence,
decoded checkpoint field, or allocator observation can escape as independent
authority.

## Tests

Repository-authored tests prove:

- generation-zero candidates require exact current complete-prefix evidence;
- a reclaimed generation accepts an independent clean-candidate anchor and does
  not require the clean frontier record to remain physically retained;
- missing, malformed, stale, nonterminal, replay-requiring, non-current-page,
  generation-changing, and allocator-contradictory evidence fails closed;
- success binds the exact candidate anchor, frontier, allocator high-water, and
  counts; and
- compile-fail examples reject owner extraction, proof cloning, and same-owner
  retry after failure.

## Evidence and Compatibility Boundary

All validation rules, typestates, ordering, fixtures, and fault models are
repository-authored. No external product documentation, SDK, driver, fixture,
oracle, captured output, proprietary governance tool, or native database/log
format was consulted.

This decision makes no SQL Server clean-shutdown, checkpoint, recovery, LSN,
file-format, or protocol compatibility claim.

## Non-Goals

This decision does not:

- inspect or accept a database manifest;
- compare the proof with a clean manifest certificate;
- construct reopened `ClosedDatabase` authority;
- publish the adjacent `RecoveryRequired` manifest generation;
- restore a live transaction coordinator;
- repair pages or WAL; or
- add externally observable MSSQL compatibility behavior.

The next #186 phase can consume this retained validator together with the exact
selected clean manifest, require full certificate equality, and construct
reopened Closed authority in the database domain and memory/filesystem
composition roots.

## Consequences

Transaction storage now has one read-only, owner-retaining path from raw
exclusive ownership to exact clean-reopen proof. Reclaimed WAL validation no
longer conflates the recovery checkpoint anchor with the later clean candidate
or assumes that the clean frontier record must remain physically retained.

Database lifecycle authority remains unavailable until a later domain gate
matches every proof field to the selected clean manifest certificate.
