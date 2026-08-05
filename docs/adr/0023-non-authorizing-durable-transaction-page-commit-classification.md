# ADR 0023: Non-Authorizing Durable Transaction-Page Commit Classification

- Status: Accepted
- Date: 2026-08-06
- Issue: #98
- Extends: ADR 0019, ADR 0020, ADR 0021, ADR 0022
- Extended by: ADR 0024, ADR 0025, ADR 0026

## Context

ADR 0020 associates a live full-image page write with one unforgeable
`TransactionId` and prevents that image from reaching the page store until the
same transaction is durably committed. ADR 0021 persists the association in the
memory adapter, and ADR 0022 adds explicit filesystem WAL v3 owner records.

After filesystem reopen, the durable owner is represented by exact nonzero epoch
and sequence fields. Those fields must not reconstruct a `TransactionId`,
because that token carries coordinator lifecycle authority and intentionally has
no public constructor. Existing `TransactionRecoverySource` lookup requires a
caller-supplied `TransactionId`, so using it for an owner discovered only from
persistent fields would require the forbidden reconstruction.

ADR 0019 can compare page WAL and page-store observations physically, but its
page observation deliberately omits ownership and commitment. Replaying its
latest physical record could therefore expose an uncommitted image. The smallest
safe next step is a separate I/O-free, allocation-free transaction-domain
classification that correlates one owned page observation with complete durable
commit observations without authorizing any mutation.

## Crate and Dependency Boundary

`ntsql-transaction` owns the classification because it already owns transaction
identity, commit ordering, and transaction-owned page semantics. Its reviewed
direct dependency set remains:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal
```

`ntsql-page` remains commit-agnostic and non-authorizing. No adapter type enters
a domain crate, no new crate or dependency edge is added, and the existing
architecture allow-list and reverse-edge tests remain unchanged.

## Persisted Identity Observation

`DurableTransactionIdentityObservation` owns exact nonzero epoch and sequence
fields. Its public constructor rejects zero fields and retains both raw inputs
plus a typed reason. The value exposes the numeric fields and may compare them
with a caller-supplied `TransactionId`.

The observation is data, not authority:

- it has no conversion to `TransactionId`;
- it cannot construct an active, committed, indeterminate, or coordinator-owned
  lifecycle token;
- it does not prove that an epoch was allocated or that a record is durable; and
- public construction does not prove adapter provenance.

Adapters remain responsible for projecting only fields from their validated
durable record kinds. Compile-fail tests protect the no-reconstruction boundary.

## Durable Page and Commit Observations

`DurableTransactionPageObservation<N>` pairs:

- one persisted owner identity observation; and
- one existing validated `DurablePageWalObservation<N>`.

The nested page observation retains the exact nonzero page number, page version,
nonempty full image, and nonzero lineage-bound WAL position defined by ADR 0019.
The pairing is still only an observation and grants no commit or replay
authority.

`DurableTransactionCommitObservation` pairs:

- one persisted transaction identity observation; and
- one nonzero lineage-bound durable commit position.

Its constructor rejects a zero position and retains both inputs. This type is
distinct from `DurableCommitLookup`: the lookup resolves one caller-supplied
live `TransactionId`, while the new observation supports inert comparison of
persisted fields without manufacturing such a token.

## Complete-Prefix Input Contract

`classify_durable_transaction_page` receives:

1. one expected `LogLineage`;
2. one durable transaction-owned page observation; and
3. every durable commit observation in the complete matching prefix, in
   strictly increasing physical order.

The commit iterator may contain identities unrelated to the selected page and
may have numeric gaps because page, epoch, and marker records share the log. It
must not omit a durable commit record. Adapter durability selection, whole-prefix
iteration, and construction of the observations remain caller responsibilities.

Completeness cannot be proven by the value types. An uncommitted result is
authoritative only under the complete-prefix input contract, and the result
still grants no lifecycle or mutation authority.

## Validation and Error Priority

The page position and every commit position must belong to the expected lineage.
Lineage is checked before numeric positions or identity fields are compared.
The complete commit iterator is validated even after a matching commit appears,
so a later foreign, duplicate, contradictory, decreasing, or second matching
record cannot be hidden by an early result.

For each commit observation, validation proceeds in this order:

1. reject a foreign lineage;
2. if the identity matches the page owner and a prior match exists, reject the
   duplicate matching commit;
3. reject an equal adjacent position as either an identical duplicate or a
   contradictory different identity; and
4. reject a decreasing position.

The duplicate-owner check precedes position-shape errors for a second matching
record because two candidate commits for the selected owner are already
ambiguous, regardless of their order. Distinct-position duplicate identities
unrelated to the selected page are not retained or diagnosed by this bounded
per-page classifier; adapter validation and classification of those owners
remain separate.

After the entire iterator validates, a sole matching commit must occur strictly
after the owned page position. An equal or earlier match contradicts the shared
frontier established by ADRs 0020 through 0022 and is a typed error, not
uncommitted state.

## Per-Record Classifications

Exactly one inert classification is returned:

- `Committed` carries the owned page position and the sole strictly later
  matching commit position; or
- `Uncommitted` carries the owned page position when the complete supplied
  commit prefix contains no match.

Unrelated commits are validated but do not affect identity matching. Two
different WAL lineages may contain identical numeric epoch, sequence, and
position values; lineage equality is therefore required before those numbers
have meaning.

The function classifies one owned page record. The persistent formats may
contain repeated `(owner, page)` records, and each record can be classified
independently. This ADR does not select the latest record, define supersession,
or impose persistent page-record uniqueness.

## Allocation and Authority Boundary

The function performs one pass over borrowed commit observations. It builds no
collection, uses no `Vec`, performs no fallible reservation, and retains only
the previous commit observation and at most one matching position.

The classification contains no owner token, page image, `DirtyPage`,
`PageWritePermit`, `TransactionDirtyPage`, `CommittedTransaction`, callback,
adapter, replay command, or page-store operation. Compile-fail tests prevent
conversion to committed, dirty, or write-authorizing state.

In particular, `Committed` means only that the supplied durable evidence has one
matching later commit. It does not establish the final visible image for a page
and does not cross ADR 0020's live `flush_committed_page` gate.

## Evidence Boundary

The observations and classification operate only on repository-authored domain
values and workspace-owned format evidence. They do not consult an external
product, driver, SDK, fixture, oracle, proprietary governance tool, or native
MDF/NDF/LDF/BAK format. The internal classifications make no SQL Server
transaction, visibility, LSN, page, recovery, crash, or diagnostic claim.

## Test Boundaries

- Nonzero identity observations preserve exact epoch and sequence fields and
  compare with, but cannot reconstruct, a live `TransactionId`.
- Zero epoch and zero sequence failures retain both raw inputs and distinct
  typed reasons.
- Zero commit positions retain the identity and lineage-bound position.
- One sole later matching commit classifies committed; empty and unrelated-only
  complete prefixes classify uncommitted.
- A second matching commit fails closed without choosing a position.
- Equal and earlier matching commits are distinct commit-after-page
  contradictions.
- Foreign page and commit lineages fail before numeric comparison.
- Identical duplicate, contradictory duplicate, and decreasing commit positions
  have distinct typed failures.
- A matching commit followed by invalid unrelated evidence still fails because
  the complete iterator is validated.
- Identical numeric identities from different lineages do not match.
- Repeated owned page records are classified independently.
- Compile-fail tests prove persisted identity cannot create lifecycle authority
  and classification cannot create committed, dirty, or write-authorizing
  state.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- project memory or filesystem records into the observations;
- prove adapter durability or complete-prefix selection;
- group an entire prefix by page or transaction;
- select a latest committed page image or define multi-image ordering;
- combine commit classification with stored-page physical reconciliation;
- create recovery authority or execute page-store mutation;
- define redo, undo, rollback, abort, compensation, checkpoints, transaction
  tables, dirty-page tables, or idempotence;
- make raw nontransactional page APIs globally unavailable;
- define visibility for reads, isolation, locking, buffering, eviction, or
  force-at-commit; or
- define any external SQL Server value or native file-format behavior.

## Consequences

The transaction domain can now distinguish committed from uncommitted durable
transaction-owned page observations using complete commit evidence while
preserving the nonforgeable lifecycle boundary and granting no replay authority.

The next adapter slice may project validated memory and filesystem owner and
commit records into these observations and prove classification across restart
and reopen. Whole-prefix final-image selection and mutation-capable recovery
remain blocked on explicit ordering, idempotence, stored raw/uncommitted state,
and a separately reviewed recovery-authority typestate.
