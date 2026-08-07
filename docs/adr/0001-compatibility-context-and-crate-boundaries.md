# ADR 0001: Compatibility Context and Crate Boundaries

- Status: Accepted
- Date: 2026-08-04
- Issue: #32
- Extended by: ADR 0002, ADR 0004, ADR 0005, ADR 0006, ADR 0008, ADR 0009,
  ADR 0010, ADR 0011, ADR 0012, ADR 0013, ADR 0014, ADR 0015, ADR 0016,
  ADR 0017, ADR 0018, ADR 0019, ADR 0020, ADR 0061, ADR 0062, ADR 0063

## Context

Every observable behavior must be interpreted against one exact target. Product
version, servicing update, compatibility level, collation, language, timezone,
and session defaults can all change results. Passing these selectors separately
would permit internally inconsistent requests and hidden process-global state.

The published JSON contracts are external representations. They require
serialization and schema concerns that do not belong in parser, binder,
catalog, execution, transaction, storage, or protocol policy. A generic `core`
crate would obscure those responsibilities and allow unrelated dependencies to
accumulate at the center of the system.

## Decision

`ntsql-compatibility` owns the I/O-free compatibility policy. It has no external
dependencies and exposes immutable value types, `CompatibilityContext`, and the
canonical seven observation dimensions. It does not read files, deserialize
JSON, contact an oracle, or choose a process-wide target.

`CompatibilityContext::with_scope` creates a fresh invariant brand for one
request through a higher-ranked callback. `CompatibilityScope` borrows the
selected context, has no public constructor, and cannot escape that callback.
Future staged request types carry the brand so independently opened scopes
cannot be combined accidentally through public APIs.

`ntsql-contract` owns the published JSON representation and its validation. It
is the adapter from a raw `TargetMatrix` to `ValidatedTargetMatrix`. Promotion
to that typestate runs all target-matrix invariants before constructing any
contexts. Callers cannot mutate either the validated matrix or its contexts.

The runtime composition root will perform these steps in order:

1. Load and deserialize the target matrix through an outer adapter.
2. Promote it with `ValidatedTargetMatrix::try_from`.
3. Require an exact target ID from startup configuration.
4. Select it with `select_context` and inject the resulting immutable context
   into request-scoped engine components.
5. Open `with_scope` and complete every compatibility-dependent request stage
   inside that branded callback.

Production startup must not silently fall back to the baseline target.
`baseline_context` exists for contract verification and deliberately requested
baseline operation. Switching targets selects another immutable context; it
does not mutate global state or an existing request.

The allowed direct dependency graph is:

```text
ntsql-contract --------> ntsql-compatibility
      |                           |
      +--> serde, serde_json      +--> standard library only

ntsql-testkit ----------> ntsql-contract

ntsql-database ---------> ntsql-wal

ntsql-transaction ------> ntsql-wal

ntsql-page -------------> ntsql-wal

ntsql-wal --------------> standard library only

ntsql-recovery-model ----> standard library only

ntsql-storage-file -----> ntsql-database, ntsql-page, ntsql-transaction, ntsql-wal
ntsql-storage-memory ---> ntsql-database, ntsql-page, ntsql-transaction, ntsql-wal

ntsql-storage-file -. development only .-> ntsql-recovery-model
ntsql-storage-memory -. development only .-> ntsql-recovery-model

ntsql-architecture-check -------> standard library only
```

Dependencies point toward policy. Domain crates must not depend on contract,
serialization, filesystem, network, oracle, protocol-host, or persistence
adapters. An outer adapter may depend on a domain port; the domain must never
depend on that adapter.

`ntsql-testkit` owns only deterministic orchestration for synthetic conformance
cases. It accepts two in-memory observation sources and input-digest
verification through injected ports, requires a plan for all seven dimensions,
and returns only locally validated `ConformanceRecord` values. It does not own a
real product oracle, cryptographic implementation, filesystem or network
access, an ambient clock, or external fixtures.

`ntsql-recovery-model` owns only the repository-authored, I/O-free bounded state
machine and deterministic trace inputs used to verify crash/restart invariants.
It has no engine, adapter, contract, serialization, randomness, filesystem, or
network dependency. Memory and filesystem adapters depend on it only from their
development targets, so concrete runners remain outside the model and production
adapter graphs remain unchanged.

`ntsql-database` owns only repository-authored logical database identity,
required file-role identity, exact composition comparison, lifecycle generation,
validated manifest lifecycle/format/feature requirements, and staged database
ownership. It depends only on `ntsql-wal` for the existing `PersistentLogId`
invariant. It owns no paths, handles, locks, bytes, codec, recovery effect,
global identity allocator, or client diagnostic. Memory and filesystem adapters
may depend inward on this boundary; the database domain never depends back on
either adapter.

`ntsql-wal` owns the I/O-free ordering invariant that a commit record is
appended and its exact assigned position is flushed before a durable
acknowledgement can exist. It owns no filesystem, byte format, transaction
semantics, recovery policy, or client diagnostic. Runtime log positions carry
their originating `LogLineage`; equal numeric positions from independent logs
are not equal or interchangeable. A trusted outer adapter may supply a stable
nonzero lineage ID so the same capability can be reconstructed after runtime
pointer identity is lost.

`ntsql-transaction` owns I/O-free transaction coordination and lifecycle state.
Its coordinator requires an injected persistence-lineage epoch, issues monotonic
nonzero sequences within that epoch, binds active tokens to one separate private
runtime identity, records every commit attempt before crossing the WAL port, and
never reconstructs terminal state as active. Commit consumes active state once,
creates committed state inside the durable callback, and otherwise returns
indeterminate state with no transition back to active. It also owns the recovery
lookup port and validates coordinator ownership, retained lifecycle, and log
lineage before authoritative durable-record evidence can create terminal
resolved state.

`ntsql-page` owns the I/O-free page lifecycle and write-ahead ordering boundary.
It binds internal page addresses to one log lineage, retains the exact WAL
position required by a dirty image, and permits a page-store write only after
that position reports durable. It owns no filesystem, persistent page format,
checkpoint, buffer replacement, redo/undo, or client diagnostic.

`ntsql-storage-memory` is an outer synthetic persistence adapter. It implements
the transaction commit-log, epoch-source, recovery-lookup, page-log, and
page-store ports, depends inward on their three owning domain crates, and
provides deterministic before/after-effect fault injection. No domain crate may
depend on it.

`ntsql-storage-file` is the outer filesystem persistence adapter. It owns
versioned ntsql-specific transaction/page WAL and page-store bytes plus
synchronous file barriers, implements transaction and page domain ports, and
depends inward on their three owning crates. It holds a standard-library
advisory exclusive lock on each open file but owns no SQL Server file format,
client diagnostic, or domain policy. It also owns the pure fixed database
manifest codec; decoding returns only a validated inert `ntsql-database` value
and performs no file I/O or authority transition. No domain crate may depend on
it.

`ntsql-architecture-check` is a build-time tool, not an engine dependency. It
compares every workspace package's complete set of direct normal, build, and
development dependencies with separate reviewed allowlists for each dependency
kind. A development-only edge cannot silently become a normal or build edge. It
rejects reverse edges, unregistered workspace packages, missing required edges,
and unreviewed external dependencies. Its negative self-test proves that a
`ntsql-compatibility -> ntsql-contract` edge is rejected. Focused tests also
reject extra `ntsql-page` dependencies, the reverse `ntsql-wal -> ntsql-page`
edge, production adapter dependencies on `ntsql-recovery-model`, and every
model-to-adapter edge. It also rejects every database-domain edge except
`ntsql-wal` and proves that both persistence adapters may depend inward on
`ntsql-database`.

## Package Evolution Rules

A new crate is added only when a concrete responsibility and ownership boundary
exist. Its pull request must:

1. State the responsibility and public boundary in an ADR update.
2. Add the exact direct dependency set to the architecture checker.
3. Add a focused test that fails for the prohibited reverse dependency.

Empty future-layer scaffolds are not added. Parser, binder, catalog, execution,
transaction, storage, and protocol responsibilities will become separate
crates only when their first behavior requires the boundary. Shared types stay
with the component that owns their invariants; they are not moved into a
catch-all package solely to avoid an explicit dependency.

## Test Boundaries

- `ntsql-compatibility` unit tests use repository-authored synthetic profiles
  and verify policy without JSON or I/O.
- Compile-fail scope tests reject mixing independently opened scopes and reject
  returning or type-erasing a scope token from its higher-ranked callback.
- `ntsql-contract` integration tests verify full validation, exact mapping, and
  target selection at the adapter boundary.
- `ntsql-testkit` tests use repository-authored in-memory sources and explicit
  timestamps, input identities, and normalization plans.
- `ntsql-recovery-model` unit tests exhaust bounded repository-authored logical
  traces without opening an adapter. Adapter development tests execute the same
  canonical seeds and compare concrete durable observations with model state.
- `ntsql-database` unit tests validate nonzero identities, exact successor
  generations, complete distinct file-role sets, stable exact-composition
  mismatches, manifest requirements, successor regression, and fail-closed staged
  selection/binding. Compile-fail tests reject identity construction, owner
  extraction, cloning staged owners, and promotion to live authority. Filesystem
  adapter tests exhaust every manifest field and boundary through a pure-memory
  golden codec.
- `ntsql-page` tests prove exact WAL-before-page call ordering, lineage
  rejection before either port call, and terminal store-write ambiguity.
  Compile-fail tests reject construction of the private write permit and clean
  or retryable states.
- External conformance tests may enter later only through approved, sanitized,
  provenance-linked behavior specifications.
- The architecture checker runs in local verification and CI before engine
  packages may rely on a changed graph.

## Consequences

Behavior selectors cannot drift independently after context construction, and
engine policy remains usable without Serde or infrastructure dependencies.
Adding or changing a package edge requires an explicit architecture decision.
The validated adapter owns some duplicated immutable strings; this bounded cost
is accepted to keep the domain independent from the public wire model.