# ADR 0004: First Executable Query Slice Boundaries

- Status: Accepted
- Date: 2026-08-05
- Issue: #46
- Extends: ADR 0001, ADR 0002

## Context

The compatibility inventory currently classifies broad Database Engine
categories but does not identify a small case that can enter the first
parse-bind-plan-execute path. Adding parser, type, binder, planner, or executor
crates before their first behavior would create the empty scaffolds prohibited
by ADR 0001. Implementing a query behavior from pending product documentation or
oracle output would also bypass the clean-room gates in `docs/governance.md`.

Inventory metadata may be made more precise without claiming behavior or using a
source as implementation input. The first candidate slice therefore needs stable
feature identities and a complete future dependency direction before any
semantic code enters.

## Decision

The feature matrix identifies three independently owned surfaces:

| Feature ID | Owner | Surface |
| --- | --- | --- |
| `language.query.select` | #8 | statement syntax, source spans, and binding entry |
| `data-types.literal.integer` | #6 | literal type derivation and value representation |
| `query-processing.constant-projection` | #14 | logical projection and deterministic execution |

All three records remain `not-tested`. Their evidence is inventory-only
metadata; it is not an approved behavior specification, conformance result, or
authorization to implement from the referenced source.

When approved behavior first enters, the staged types and their constructors
will be owned as follows:

1. `ntsql-syntax` owns source text, valid source spans, tokens, syntax nodes, and
   `ParsedBatch`. Only its parser can construct a parsed batch.
2. `ntsql-types` owns typed scalar values, type identity, and literal derivation.
3. `ntsql-binder` owns the catalog port, resolved typed expressions, and
   `BoundBatch`. It consumes `ParsedBatch`; only the binder can construct a
   bound batch.
4. `ntsql-planner` owns output-schema-preserving logical plans. It consumes
   `BoundBatch`; only the planner can construct an executable logical plan.
5. `ntsql-executor` owns execution state and typed results. It consumes a
   logical plan and cannot accept parsed or bound input directly.

The complete intended direct dependency graph for that first slice is:

```text
ntsql-syntax   -> ntsql-compatibility, ntsql-diagnostics
ntsql-types    -> ntsql-compatibility, ntsql-diagnostics
ntsql-binder   -> ntsql-syntax, ntsql-types,
                  ntsql-compatibility, ntsql-diagnostics
ntsql-planner  -> ntsql-binder, ntsql-types,
                  ntsql-compatibility, ntsql-diagnostics
ntsql-executor -> ntsql-planner, ntsql-types,
                  ntsql-compatibility, ntsql-diagnostics
```

These are planned edges, not architecture-check registrations. Each crate and
its exact then-current edge set enter only with its first owned behavior,
focused falsifying test, ADR update, architecture policy, and negative
dependency test. The later catalog, transaction, storage, clock, and resource
ports enter only with behavior that needs them.

The composition root opens `CompatibilityContext::with_scope` for the selected
target. The parser receives `CompatibilityScope<'ctx, 'scope>` and stores it
privately inside `ParsedBatch<'ctx, 'scope>`. `BoundBatch`, the logical plan, and
executor state consume the preceding type and propagate that exact invariant
brand; binder, planner, and executor public APIs do not accept a second scope or
context. Independently opened brands cannot satisfy one staged API, and the
brand cannot escape the higher-ranked callback. Compatibility-dependent
adaptation, including protocol output, completes inside that callback; only
fully decided unbranded output may leave it.

Branding prevents accidental mixing through public APIs, not arbitrary
malicious implementation code. Each stage also has a reviewed invariant: it may
only propagate the private scope from its input. It may not choose a target,
clone or reconstruct selectors, invoke `CompatibilityContext::try_new`, read
global target state, or fall back to the baseline. The first implementation
tests this propagation with two distinct synthetic contexts and scopes. Domain
crates must not depend on `ntsql-contract`, Serde, `ntsql-testkit`, filesystems,
networks, product oracles, or protocol hosts.

## Implementation Admission Gate

The procedure in `docs/governance.md` remains authoritative. No semantic
implementation for these feature IDs may begin until all of the following
artifacts and decisions exist:

1. Every proposed source and retained artifact has a provenance record and
   content digest, and the exact implementation, conformance-evidence, and
   fixture uses have qualified legal approval.
2. The observer, specification reviewer, implementer, and conformance reviewer
   are assigned before observation, with the observer and implementer held by
   different people.
3. The observer records the exact target, input bytes, commands, session
   settings, environment, raw-evidence digest, and cleanup result without
   implementing the case.
4. A separate specification reviewer approves a sanitized, typed behavior
   specification, its digest, path, and complete provenance lineage.
5. The audit record identifies the issue and case, every role, timestamps,
   provenance and legal-review IDs, target, commands, raw-evidence digest,
   specification path and digest, review decision, and cleanup or deletion
   events.
6. The assigned implementer receives only that approved behavior specification,
   the feature IDs above, and public repository interfaces.
7. An independently authored conformance case names its parent provenance and
   states expected syntax, typed result, metadata, diagnostic, side-effect, and
   operational observations without importing protected raw evidence.

The current ledgers do not record an approved behavior specification or the
required role separation for this case. Consequently, this ADR authorizes
inventory and architecture planning only. It does not authorize a query parser,
integer-literal rule, result value, metadata field, error value, or compatibility
claim.

## Test Boundaries

- Published-contract tests pin each feature ID, category, owner, baseline target,
  `not-tested` status, inventory provenance, empty differences, and null
  feature-level legal-review field. The source-use legal gate remains pending.
- Existing feature-matrix validation continues to enforce unique IDs, complete
  categories, target references, and status invariants.
- The first semantic PR must add a test that fails before implementation and
  names the approved behavior specification. Synthetic examples alone cannot
  promote a feature status or support a compatibility claim.

## Consequences

The first executable path has an explicit staged design without placeholder
packages or invented behavior. Work on a later stage cannot bypass an earlier
type, and every compatibility-dependent decision receives one exact target
context. Semantic implementation remains blocked until the clean-room procedure
produces an approved, provenance-linked specification.
