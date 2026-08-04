# ADR 0001: Compatibility Context and Crate Boundaries

- Status: Accepted
- Date: 2026-08-04
- Issue: #32
- Extended by: ADR 0002

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

Production startup must not silently fall back to the baseline target.
`baseline_context` exists for contract verification and deliberately requested
baseline operation. Switching targets selects another immutable context; it
does not mutate global state or an existing request.

The allowed direct dependency graph is:

```text
ntsql-contract --------> ntsql-compatibility
      |                           |
      +--> serde, serde_json      +--> standard library only

ntsql-architecture-check -------> standard library only
```

Dependencies point toward policy. Domain crates must not depend on contract,
serialization, filesystem, network, oracle, protocol-host, or persistence
adapters. An outer adapter may depend on a domain port; the domain must never
depend on that adapter.

`ntsql-architecture-check` is a build-time tool, not an engine dependency. It
compares every workspace package's complete set of direct normal, build, and
development dependencies with the reviewed allowlist. It rejects reverse
edges, unregistered workspace packages, missing required edges, and unreviewed
external dependencies. Its negative self-test proves that a
`ntsql-compatibility -> ntsql-contract` edge is rejected.

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
- `ntsql-contract` integration tests verify full validation, exact mapping, and
  target selection at the adapter boundary.
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