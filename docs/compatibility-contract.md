# Compatibility Contract

ntsql targets externally observable compatibility with Microsoft SQL Server Database Engine. Compatibility is always a claim about an exact, immutable target in `contracts/compatibility/targets.json`; it is never an unqualified claim about every SQL Server release, edition, platform, collation, or configuration.

The initial target is SQL Server 2022 CU26 Developer edition, build `16.0.4265.3`, running the pinned Ubuntu 22.04 x86-64 container image at database compatibility level 160. The target also fixes collation, language, LCID, timezone, and session settings. A host default that is absent from the target record cannot support a compatibility claim.

## Scope

The contract covers Database Engine behavior visible to a client or administrator:

- TDS connection, authentication negotiation, requests, responses, metadata, diagnostics, and connection state
- T-SQL syntax, types, expressions, statements, schema objects, programmability, catalogs, and metadata APIs
- query results, ordering guarantees, conversions, collations, transactions, locking, concurrency, and persistent side effects
- Database Engine security, configuration, observability, backup, restore, recovery, availability, and data-distribution surfaces
- documented public Database Engine integration surfaces

SQL Server Analysis Services, Integration Services, Reporting Services, Master Data Services, Data Quality Services, graphical management tools, Azure control planes, and client-library implementations are separate products or components and are outside this contract. They can enter scope only through a separately versioned target and feature inventory.

Native MDF, NDF, LDF, and BAK binary compatibility is `blocked-legal`. No implementation or compatibility claim for those proprietary formats may proceed until the clean-room policy records an explicit legal review that authorizes the proposed method. Logical backup and restore behavior may be implemented independently where it does not require those formats.

## Observations

Every conformance case records all seven dimensions:

1. `syntax`
2. `wire`
3. `result`
4. `metadata`
5. `diagnostic`
6. `transactional_side_effect`
7. `operational`

An observation is either `observed`, with distinct raw oracle and subject
evidence, normalized oracle and subject payloads, the exact normalization-rule
revisions applied in order, and a comparison status, or `not-observed`, with a
nonempty reason. A missing dimension is invalid evidence. `not-observed` is
acceptable while work is incomplete but cannot support a
complete-compatibility claim.

Expected behavior is classified as `documented`, `version-dependent`, `unspecified`, or `implementation-dependent`. Public authoritative specifications take precedence. Oracle observations fill gaps only where public documentation does not define the behavior; they do not convert implementation-dependent behavior into a public contract.

## Comparison Rules

The harness retains the raw input and raw observations, then compares a typed
normalized representation. Synthetic or redistributable structured evidence
may be stored inline. Other raw bytes are represented by an evidence-store ID,
artifact ID, SHA-256 digest, byte length, media type, and `public` or `protected`
access classification. Protected evidence remains outside the repository and
public build artifacts; the record retains enough immutable metadata to detect
substitution or absence.

A case may normalize a nondeterministic value only when the normalization rule
is defined in that record with a stable ID, positive revision, provenance ID,
and nonempty description. Each observed dimension references exact rule
revisions in application order. An empty reference list means no normalization
rule was applied, so each inline raw value must equal its normalized value
exactly. Artifact evidence always requires an explicit projection rule because
the retained bytes cannot be compared from the record alone. An empty list never
permits silent transformation or field removal. Unknown, duplicate, or unused
rules invalidate the record. Changing a rule creates a new revision and new
conformance evidence unless a versioned contract migration explicitly preserves
its meaning. Timestamps, generated identifiers, paths, process identifiers, and
message text are not silently discarded.

`compatible` requires exact equality between the normalized oracle and subject
values, including JSON numeric kind and IEEE signed zero. `divergent` requires
an inequality; `partial` records a deliberately incomplete comparison. A
zero-byte artifact is valid evidence only with the SHA-256 digest of empty
content, so absence cannot masquerade as an empty observation. Conformance
digests use canonical lowercase hexadecimal, and inline JSON numbers retain
their arbitrary-precision lexical identity.

Every record also names one feature and its owning issue, one exact target and
evidence provenance record, the exact runner and subject Git revisions and
artifact SHA-256 digests, a deterministic case seed, the SHA-256 digest of the
input bytes, complete name/value environment facts, and a shell-free runner
argument vector whose elements are nonempty and NUL-free. The feature, owner,
target, case provenance, and normalization provenance references must resolve
against the published ledgers before the record can become governed evidence. A
feature in `blocked-legal` state cannot produce a valid conformance record.

| Dimension | Required comparison |
| --- | --- |
| `syntax` | Exact input bytes and encoding, batch boundaries, acceptance or rejection point, statement affected, and parse or bind diagnostics. Client-side rewriting is forbidden. |
| `wire` | Pre-login and login negotiation, ordered request and response messages, TDS token identity and fields, result-set boundaries, `ENVCHANGE`, `DONE` status and row counts, attention handling, and final connection state. Raw transcript hashes are retained. Packet segmentation is recorded separately and is required only where the negotiated protocol or public specification makes it significant. |
| `result` | Number and order of result sets, row cardinality, typed value including `NULL`, and row sequence. A documented ordering guarantee requires exact sequence equality. Without such a guarantee, semantic comparison uses a typed row multiset while retaining the observed sequence as implementation-dependent evidence. |
| `metadata` | Column ordinal, name, SQL type identity, wire type, length, precision, scale, nullability, collation, code page, and update or identity flags where exposed. Display strings do not substitute for typed metadata. |
| `diagnostic` | Ordered warnings and errors, message number, severity, state, text, procedure, line, `@@ERROR` at the defined observation point, affected-row counts, and whether the connection remains usable. Any documented localization or generated-name normalization is case-specific and retains the raw text. |
| `transactional_side_effect` | Initial and final `@@TRANCOUNT` and `XACT_STATE()`, commit or rollback outcome, schema and data changes, generated values, and durable side effects after reconnect. The harness observes through an independent session when isolation or visibility is relevant. |
| `operational` | Configuration values and scopes, permissions, lifecycle transitions, restart requirements, files or logical artifacts produced through public interfaces, and administrative diagnostics. Environment facts are part of the target rather than normalized away. |

Integer and exact numeric values compare by SQL type and mathematical value plus declared precision and scale. Approximate numerics retain their IEEE bit pattern and compare under a case-specific documented tolerance only where SQL Server documents a nondeterministic approximation. Character and binary values compare length and code units; collation equivalence is tested as behavior and does not rewrite the returned value. Date and time values compare SQL type, precision, offset where present, and represented value. XML, JSON text, spatial values, and other structured payloads remain byte- or text-exact unless a public contract defines a canonical form.

An oracle query used to inspect side effects must not alter the state under test. Setup, action, observation, and cleanup are distinct phases. A failed cleanup invalidates environment reuse but does not erase the captured failure. Each comparison is made against one exact `target_id`; combining observations from several targets into a synthetic expected result is forbidden.

## Feature Status

- `compatible`: all required observations match every listed target and no known difference exists.
- `partial`: a documented subset matches and every excluded behavior is listed in `differences`.
- `divergent`: at least one required observation differs and every known difference is listed.
- `blocked-legal`: work is isolated behind a named legal-review record.
- `not-tested`: evidence is insufficient to make a compatibility judgment.

Every feature belongs to exactly one of the 18 top-level Database Engine categories represented by `category.*` roots in `contracts/compatibility/features.json`. Category roots are classification anchors, not claims that the category is exhaustively enumerated or implemented. New behavior must be added as a feature record before implementation work can claim coverage.

## Complete-Compatibility Claim

ntsql may claim complete compatibility only for named target identifiers for which all of the following are true:

1. The target, feature, provenance, and legal ledgers pass their machine validators.
2. The feature inventory is exhaustive for the documented Database Engine surface of that target and contains no unclassified behavior.
3. Every in-scope feature is `compatible`; none is `partial`, `divergent`, `blocked-legal`, or `not-tested`.
4. Every applicable case has all seven dimensions in the `observed` state with `compatible` comparisons.
5. The complete conformance corpus passes on a freshly provisioned oracle and ntsql build.
6. No open defect or known difference contradicts the claim.
7. The release notes name the exact target identifiers and contract version.

Passing a subset, a category root, or a client smoke test must be described as that narrower result. It must not be presented as SQL Server compatibility in general.

## Regression And Withdrawal

A newly discovered mismatch immediately invalidates the affected compatibility claim, even if a released binary has not changed. The owning feature is changed to `partial` or `divergent`, the observed difference and affected targets are published, and release automation blocks the claim until corrected evidence passes. Historical evidence remains immutable; a correction creates a new record rather than rewriting an old result.

When a target image, servicing build, operating system, edition, compatibility level, collation, language, timezone, or required session setting changes, it becomes a new target identifier. Evidence from another target does not transfer implicitly.

## Versioning

The contract and each machine-readable ledger use semantic versions independently of the ntsql product version.

- Contract major: an existing field, status, dimension, category, or rule changes meaning or becomes invalid.
- Contract minor: backward-compatible fields, evidence types, or classifications are added.
- Contract patch: wording or constraints are clarified without changing valid records.

Before the first complete target certification, ntsql product releases remain in the `0.y.z` series. Product version increments never restore an invalidated compatibility claim; only passing evidence for the named target can do so.

## Known Differences

Known differences are public contract data. Each difference names the externally visible behavior, affected targets, owner issue, and evidence. `partial` and `divergent` records without a difference are invalid. Resolved differences remain available through version control and immutable conformance records.

## Dependency Policy

The Rust standard library and workspace-owned code are the default. An external dependency is admitted only when it provides a necessary interoperability boundary, a materially safer implementation than a local substitute, or a capability whose correct reimplementation is disproportionate to the project.

Every admitted dependency must have a compatible license, minimal feature set, lockfile entry, maintained upstream, provenance record, and security review appropriate to its role. A dependency used only for convenience is rejected. Direct dependencies are centralized in the workspace manifest. The current `serde` and `serde_json` dependencies are limited to the public JSON contract boundary; ntsql does not use them as a reason to introduce a broader framework dependency. The complete admission and CI rules are defined in `docs/governance.md`.

## Machine-Readable Files

- `contracts/compatibility/behavior-specification-admissions.json`: clean-room roles, observation audit, sanitized specification review, handoff, and derived-test inventory
- `contracts/compatibility/targets.json`: immutable oracle targets and expansion order
- `contracts/compatibility/features.json`: classified feature inventory and current status
- `contracts/compatibility/provenance.json`: source, artifact, dependency, lineage, digest, and intended-use inventory
- `contracts/compatibility/legal-reviews.json`: qualified human legal decisions and unresolved gates
- `contracts/schemas/behavior-specification-admission-ledger.schema.json`: clean-room implementation-admission interchange schema
- `contracts/schemas/conformance-record.schema.json`: one comparison record
- `contracts/schemas/feature-matrix.schema.json`: feature matrix interchange schema
- `contracts/schemas/legal-decision-authority.schema.json`: authenticated out-of-branch legal-decision evidence
- `contracts/schemas/legal-review-ledger.schema.json`: legal-review ledger interchange schema
- `contracts/schemas/provenance-ledger.schema.json`: provenance ledger interchange schema
- `contracts/schemas/target-matrix.schema.json`: target matrix interchange schema
- `contracts/schema-corpus/`: validator-neutral positive, negative, boundary, and Rust-only contract cases

Validation has four distinct boundaries. JSON deserialization checks the Rust
wire shape. Draft 2020-12 JSON Schema is the interoperability prefilter.
`validate_schema_semantics` is the matching first-party Rust boundary for
constraints expressible by those schemas. Full Rust validation is authoritative
for cross-record, graph, authenticated-context, and governed-use invariants such
as unique identifiers, target and provenance references, complete category
coverage, contiguous expansion order, exact provenance closure, and trusted
candidate binding.

The behavior-specification admission ledger is intentionally empty until the
clean-room procedure produces a real case. Structural `approved` metadata is not
an authority: implementation admission still requires authenticated legal
authorization and the protected specification-review authority tracked by
Issue #55. Exact feature and target IDs are mandatory, so neither feature-only
approval nor baseline fallback can authorize semantic work.

Conformance records currently use schema version `2.0.0`. Version 2 makes raw
evidence, normalization rules, feature ownership, and reproduction metadata
mandatory; a version 1 normalized-only record is not lossless evidence and is
therefore not valid under version 2.

The shared corpus records separate expectations for all four boundaries and
requires JSON Schema and typed Rust schema-semantic expectations to agree for
instances that deserialize. JSON Schema remains a prefilter: its `integer` type
also accepts mathematically integral decimal or exponent representations, while
the Rust wire types require JSON integer representations. The corpus records
that deserialization boundary explicitly.

The first-party runner currently exercises the Rust boundaries, binds corpus
IDs to the published schema IDs, and verifies local and relative
schema-reference reachability. It does not claim standards-level schema
compilation or instance validation. Executing a third-party Draft 2020-12
validator remains blocked by
`legal-review-third-party-dependencies`; selecting or running one requires the
dependency provenance, license, security, SBOM, and authenticated qualified
legal-review controls in `docs/governance.md`.

Draft 2020-12 `format` is annotation-only under the default meta-schema. The
published schemas and shared corpus therefore assert the contract's lexical
date and UTC timestamp forms but do not silently elevate calendar or timezone
interpretation into a portable schema assertion.