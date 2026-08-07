# ADR 0062: Database Identity and Lifecycle Ownership

- Status: Accepted
- Date: 2026-08-07
- Issue: #181
- Extends: ADR 0001, ADR 0012, ADR 0058, ADR 0061
- Extended by: ADR 0063, ADR 0064

## Context

The completed recovery path owns an exact WAL source, page store, checkpoint
source, transaction coordinator, and recovery evidence. Those components do not
yet identify one logical database. They can be opened independently, and their
numeric metadata cannot establish database-wide ownership or prove that three
file roles belong to one composition.

Database create/open/close/drop will need a persistent manifest, a stable
database-wide lock, and effectful transitions. Those later boundaries must not
force paths, handles, bytes, synchronization, decoded contracts, or adapter
failure into a domain crate. They also must not create live authority directly
from caller-supplied numbers.

## Decision

Add `ntsql-database`, an I/O-free domain crate responsible only for:

- repository-owned database and file identities;
- monotonically adjacent lifecycle generations;
- the exact required WAL, page-store, and restart-checkpoint role set;
- binding that set to the existing persistent WAL identity;
- stable exact-composition comparison; and
- non-cloneable staged database ownership.

It has one normal dependency on `ntsql-wal` so compositions reuse
`PersistentLogId` rather than defining a second WAL-lineage identity. The
database crate has no build or development dependencies.

`ntsql-storage-memory` and `ntsql-storage-file` add normal inward dependencies on
`ntsql-database`. This issue adds no adapter behavior. The edges establish the
reviewed direction before later manifest and lock adapters consume the domain
types.

## Identity Domains

`DatabaseId` and `DatabaseFileId` are separate opaque nonzero `u128` domains.
Their allocation, persistence, uniqueness, and non-reuse belong to a trusted
outer adapter. They define no random generator, clock, global registry, secret,
Microsoft identifier, or byte encoding.

`DatabaseLifecycleGeneration` is an opaque nonzero `u64`. Generation one is the
first publishable lifecycle record. `checked_next` reports exhaustion at
`u64::MAX`; `require_successor` rejects equal/lower generations and skipped
generations separately. No transition wraps, saturates, defaults to one, or
accepts a merely larger generation.

All scalar identities have stable value equality and ordering. Equal numeric
values in the database, file, and persistent-WAL namespaces remain distinct
concepts.

## Exact Composition Identity

`DatabaseCompositionIdentity::new` accepts inert role entries and requires
exactly one:

1. WAL file identity;
2. page-store file identity; and
3. restart-checkpoint file identity.

Duplicate roles, missing roles, and one file identity reused by two roles are
typed errors. Input order has no meaning; accessors return the stable role order
listed above.

The composition also binds one `DatabaseId`, one lifecycle generation, and one
`PersistentLogId`. Exact comparison checks database, lifecycle generation, each
file role in stable order, and persistent WAL identity. It returns the first
typed contradiction. The value contains no path, descriptor, lock, manifest
bytes, checksum, decoded frame, or authority.

## Staged Ownership

The crate defines distinct, non-cloneable owners for:

1. unbound database ownership;
2. selected manifest identity;
3. exact composition bound but recovery required;
4. recovery-complete live ownership;
5. close pending;
6. orderly closed;
7. drop pending; and
8. terminal dropped identity.

Fields remain private. Successful staged owners expose identity observations but
not their generic outer owner. A failed manifest selection retains the unbound
owner and rejected identity; a failed exact binding retains the selected owner,
observed identity, and first contradiction. The caller may drop that whole state
or recover the earlier inert state and retry with fresh evidence.

This issue publicly permits only:

- `UnboundDatabase -> ManifestSelectedDatabase` after the requested database ID
  matches; and
- `ManifestSelectedDatabase -> RecoveryRequiredDatabase` after exact composition
  comparison.

There is deliberately no public transition from recovery-required to live, live
to close-pending, closed to drop-pending, or drop-pending to dropped. Issues
#185, #186, and #187 may add those constructors only inside effectful,
fail-closed gates that retain the exact database-wide owner and operation
evidence. A decoded manifest, `DatabaseLifecycleStage`, numeric identity, or
independently assembled resource set cannot invoke those transitions.

## Failure and Authority Boundary

Identity-set validation, generation validation, manifest selection, and exact
composition binding return typed errors. They do not panic, silently normalize,
choose a fallback database, substitute a role, skip a generation, or return a
success-shaped owner.

The generic owner is an opaque value retained by typestate; this crate does not
claim that an arbitrary caller-supplied value is a filesystem lock. Issue #183
must define the trusted adapter gate that places the real database-wide and
child-lock owner into these stages. Until then, every reachable state remains
non-live.

## Architecture Enforcement

The reviewed normal graph adds:

```text
ntsql-database -------> ntsql-wal

ntsql-storage-memory -> ntsql-database
ntsql-storage-file ---> ntsql-database
```

The architecture checker records complete normal/build/development dependency
sets. Negative tests reject database-domain dependencies on compatibility
selection, contracts, diagnostics, transaction policy, serialization, and both
persistence adapters in every dependency kind. Adapter tests prove the inward
database dependency is permitted while existing reverse-edge checks remain.

## Compatibility and Evidence Boundary

All identities, roles, stages, and errors are repository-authored. No external
product documentation, driver, SDK, fixture, oracle, captured output, native
file, or proprietary governance tool is consulted.

This decision defines no SQL Server database/file ID, recovery phase, error
number, path layout, MDF/NDF/LDF/BAK bytes, protocol behavior, or compatibility
claim.

## Test Boundaries

- Scalar tests reject zero, preserve exact values, and prove stable ordering.
- Generation tests accept only one exact successor and report regression, skip,
  and exhaustion independently.
- Composition tests accept arbitrary input order and reject missing/duplicate
  roles and duplicate file identities.
- Exact comparison tests cover every database, generation, role, and WAL-lineage
  mismatch in stable order.
- Staged tests prove foreign manifest and child composition evidence retain the
  prior owner for exact retry.
- Compile-fail tests reject scalar field construction, staged-owner cloning,
  outer-owner extraction, live-owner construction, and recovery bypass.
- Architecture tests enforce the dependency direction in every Cargo dependency
  kind.

## Non-Goals

This ADR does not:

- encode, decode, persist, select, replace, or migrate a manifest;
- allocate identities or define random/clock-based generation;
- open paths or acquire database/file locks;
- create, recover, close, or drop physical storage;
- expose recovery-complete transaction/page owners;
- define clean/unclean markers, close certificates, or tombstones;
- add allocation, heap/index, buffer, backup, or migration behavior; or
- define native Microsoft behavior or persistent format compatibility.

## Consequences

Later lifecycle effects now have one exact I/O-free identity contract and a
typestate vocabulary that cannot be promoted to live authority by public data
constructors. The cost is an intentionally incomplete public transition graph:
effectful child issues must extend the same crate rather than bypassing it or
assembling database authority in an adapter.
