# ADR 0069: Database Clean-Close Preparation Gate

- Status: Accepted
- Date: 2026-08-08
- Issue: #186
- Extends: ADR 0062, ADR 0066, ADR 0067, ADR 0068
- Extended by: ADR 0070

## Context

ADR 0066 releases `LiveDatabase` only after one database-wide owner and the
completed transaction-storage owner agree on the selected persistent WAL.
ADR 0068 can later consume that transaction owner and produce a fresh,
non-forgeable clean-close proof, but it deliberately does not bind the proof to
the selected database lifecycle generation or advance database typestate.

The database boundary must consume Live before close observation or publication.
It must not return Live after a transaction candidate may have been replaced,
detach a transaction proof from its owners, trust caller-supplied certificate
fields, or advertise `Closed` before a clean manifest is synchronized. It also
needs an explicit way to relinquish authority without pretending Rust
destruction performed a fallible durable close.

The transaction port requires a clean-close checkpoint candidate physically
disjoint from the restart-selected checkpoint. The memory adapter previously
implemented only the selected completeness slot, so it could not execute the
database gate without violating that separation.

## Decision

### Consuming Live to ClosePending

Add the only database close-preparation transition:

```text
LiveDatabase<RecoveredDatabaseOwnership<...>>
    -> ClosePendingDatabase<PreparedDatabaseCloseOwnership<...>>
```

`LiveDatabase::prepare_close` consumes Live at entry. It first calculates the
exact adjacent composition identity while retaining the source identity. A
narrow `DatabaseCloseSourceManifestOwner` port returns the manifest already
retained by the concrete database-wide owner. The gate requires that manifest's
full composition identity to equal Live and its lifecycle to be exactly
`RecoveryRequired`. This preflight also detects lifecycle-generation exhaustion
before invoking the transaction checkpoint publisher, but it does not return
reusable Live authority on failure. Rust coherence fixes the port implementation
for each repository adapter outer-owner type.

The transition then consumes the exact
`WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay` through
ADR 0068's `prepare_clean_close`. No caller closure, detached proof, decoded
certificate, startup analysis, or supplied counter can replace that operation.

On transaction success, the database gate copies the proof coordinates only
while borrowing the prepared transaction owner and checks:

1. the proof's persistent WAL identity exactly equals the selected database
   composition's persistent WAL identity;
2. the certificate source generation is the selected recovery-required
   lifecycle generation;
3. optional durable frontier, allocated epoch high-water, checkpoint anchor,
   and portable transaction/page counts form one canonical
   `DatabaseCleanCloseCertificate`; and
4. the proposed clean composition is exactly the adjacent generation with the
   same stable database, child-role, and WAL identities.

Success creates `PreparedDatabaseCloseOwnership`, which inseparably retains the
database-wide outer owner, the prepared transaction owner and checkpoint
candidate, the exact certificate, and the exact full target manifest produced
by `source_manifest.next_clean(certificate)`. This construction preserves
storage-format requirements and required features as well as stable identity.
The
`ClosePendingDatabase` itself continues to report the selected source identity;
`target_identity` is only a proposed adjacent successor until manifest
publication completes.

The prepared owner can be borrowed for adapter inspection but has no owner,
transaction, proof, or receipt extraction path. This phase deliberately provides
no `ClosePending -> Closed` constructor.

### Terminal failure ownership

`FailedDatabaseClosePreparation` distinguishes:

- source-manifest identity/lifecycle contradiction or lifecycle-generation
  exhaustion before transaction publication;
- the exact typed transaction close-preparation failure from ADR 0068; and
- post-transaction database evidence contradiction, including persistent-WAL
  mismatch or noncanonical certificate fields.

Every variant retains the complete exact owner set. Transaction failures
preserve ADR 0068's `BeforePublication` versus `OutcomeIndeterminate`
classification. Database evidence failures occur only after the transaction
candidate was successfully published and revalidated, so they remain terminal.

The failed type exposes borrowed causes but no adapter extraction or same-owner
retry. Resolution requires explicitly abandoning or dropping the terminal owner
and reopening from the selected durable manifest. Neither path yields a
success-shaped closed owner.

### Explicit unclean abandonment

Add `AbandonedDatabase` as an inert terminal in-process outcome.
`LiveDatabase::abandon`, `ClosePendingDatabase::abandon`, and failed close
preparation's `abandon` consume and drop all retained owners without invoking a
manifest or checkpoint publication method. They report the last selected
recovery-required composition.

There is no custom `Drop` implementation on Live or ClosePending. Ordinary Rust
drop may release process resources through their adapter owners, but it performs
no hidden fallible close protocol and can never publish clean state.

`Abandoned` describes relinquished in-process authority, not a new manifest
lifecycle state. Durable open still observes `RecoveryRequired`.

### Memory clean-close candidate

Extend the memory completeness-checkpoint adapter with a second optional slot
used only by `TransactionPageStorageCleanCloseCheckpointPublisher` and
`TransactionPageStorageCleanCloseCheckpointSource`.

The existing `slot` remains the restart-selected checkpoint. Clean-close
publication replaces only `clean_close_candidate`; candidate load reads only
that field. Both ports copy complete transaction, page, replay, frontier, and
persistent-WAL observations and validate the private publication permit before
replacement.

The candidate owns an independent one-shot fault plan for before publication,
after publication, and before reload. A returned publication error remains
outcome-indeterminate at the transaction boundary even when the deterministic
memory model knows whether its assignment occurred. Candidate faults never
consume the selected-slot fault plan and never modify the selected slot.

`LiveInMemoryDatabase::prepare_close` exposes the domain gate as
`ClosePendingInMemoryDatabase`; it does not yet publish a clean memory manifest.

## Authority and Effect Ordering

The complete preparation order is:

1. consume Live;
2. reobserve the retained source manifest through the trusted outer owner;
3. require exact source identity, `RecoveryRequired`, and an adjacent lifecycle
   generation;
4. consume transaction storage through ADR 0068;
5. bind the proof's persistent WAL identity to the selected composition;
6. construct the exact source-generation certificate and full clean successor
   manifest;
7. retain source and target manifests with every owner in ClosePending.

Only step 4 can publish the transaction clean-close checkpoint candidate.
No step writes a database manifest, advances the selected manifest generation,
constructs `ClosedDatabase`, releases locks early, or changes stable child
identity.

## Tests

Repository-authored tests prove:

- empty and committed nonempty recovered memory databases reach ClosePending
  with the exact fresh frontier/counts, source generation, adjacent stable target
  identity, and proof-derived certificate;
- the database-wide modeled lock remains held throughout ClosePending;
- active coordinator state returns the nested typed pre-publication failure and
  retains ownership until explicit abandonment;
- a clean-close candidate publication fault is terminally
  outcome-indeterminate and retains ownership;
- lifecycle-generation exhaustion is reported before transaction close
  publication;
- a retained Clean source manifest is rejected before transaction close
  publication;
- explicit memory/filesystem Live and memory ClosePending abandonment release
  ownership while reporting the source recovery-required identity; and
- successful memory preparation exercises the dedicated candidate ports while
  the adapter stores selected and clean-close observations in separate fields.

Compile-fail documentation continues to reject owner construction, extraction,
and stale retry.

## Evidence and Compatibility Boundary

All typestate rules, certificate binding, memory effects, fault points, and tests
are repository-authored. No external product documentation, SDK, driver,
fixture, oracle, captured output, proprietary governance tool, or native
database/log format was consulted.

This gate makes no SQL Server close, checkpoint, recovery, LSN, file-format, or
protocol compatibility claim.

## Non-Goals

This decision does not:

- publish or synchronize a memory or filesystem clean manifest;
- implement the filesystem clean-close checkpoint candidate;
- construct `ClosedDatabase`;
- select a clean candidate during reopen;
- publish the immediate recovery-required successor required before new Live
  work;
- reclaim WAL;
- define subprocess lock inheritance; or
- add externally observable MSSQL compatibility behavior.

## Consequences

Database Live authority can now be consumed exactly once into a proof-bound,
generation-bound ClosePending owner. Memory execution proves the composition
gate without weakening recovery checkpoint selection. The next #186 phase can
consume this prepared owner for memory and filesystem manifest publication,
validate the synchronized selected manifest, and only then construct Closed.
