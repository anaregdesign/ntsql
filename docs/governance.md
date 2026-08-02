# Clean-room, Legal, and Supply-chain Governance

This policy controls which inputs may be used to design, implement, test, and
describe ntsql. It is an engineering control, not legal advice. A machine-valid
record is not a legal conclusion.

The authoritative source inventory is
`contracts/compatibility/provenance.json`. Human legal decisions are recorded in
`contracts/compatibility/legal-reviews.json`. All legal reviews are `pending` at
the initial publication of this policy. No automation, contributor, or AI agent
may change a review to `approved` or invent a reviewer, date, or rationale.

## Fail-closed Decision Model

The repository distinguishes recording a source from authorizing its use.

| Status | Effect |
| --- | --- |
| `pending` | Metadata may be inventoried, but no governed use is authorized. |
| `approved` | Only the uses listed in `approved_uses` are authorized, subject to any individually reviewed uses. |
| `rejected` | The source is not used. Affected work is removed or independently reimplemented. |

Approval requires a named qualified human reviewer, a decision date, explicit
uses, and a rationale. Silence, a passing CI job, a public URL, an open-source
license identifier, or a patent promise is not approval. `ntsql-contract`
rejects pending, rejected, undeclared, or individually unreviewed uses when a
governed activity is requested.

## Input Classes

Inventory-only metadata may be recorded while review is pending: title, public
URL, revision, retrieval date, author, method, environment, declared license,
SHA-256 digest, intended use, lineage, and review ID. Full external documents
are not copied into the repository unless redistribution has been explicitly
approved.

The following inputs are prohibited by project policy:

- leaked, confidential, internal, NDA-controlled, or otherwise non-public material
- Microsoft or third-party product source code not expressly licensed for this project
- code, tests, fixtures, or documentation copied from another implementation
- decompiled or disassembled output, binary string or symbol extraction, memory scraping, or unauthorized binary-format inference
- customer, production, personal, credential, or secret-bearing data
- fixtures without an approved provenance record and a matching content digest
- logos, artwork, trade dress, or branding that could imply affiliation or endorsement

The following activities require an explicit qualified legal decision before
they begin or their outputs enter the repository:

- using product documentation or an Open Specification as implementation input
- operating a proprietary oracle, capturing its output, or redistributing observations
- implementing or inferring MDF, NDF, LDF, BAK, undocumented protocols, CLR behavior, or external component behavior
- reproducing exact error text, localized strings, system-object definitions, screen text, or other potentially expressive material
- including a third-party dependency or executing a third-party governance tool
- applying project-license or contribution terms
- using product names in release, compatibility, marketing, site, or CLI claims

## Clean-room Roles

Separation is applied per behavior case. If independent people cannot fill the
required roles, the case remains blocked.

| Role | Access and responsibility |
| --- | --- |
| Source custodian / observer | Accesses only approved source classes and approved oracle environments. Captures factual inputs, outputs, environment facts, and hashes. Does not implement the same case. |
| Specification reviewer | May inspect approved raw evidence. Removes expressive content and verifies that the behavior specification contains facts rather than copied implementation or prose. |
| Implementer | Receives only the approved, provenance-linked behavior specification. Has no access to raw proprietary material for that case and does not operate the oracle for it. |
| Conformance reviewer | Runs independently authored tests, checks lineage and legal uses, and determines only technical comparison status. Does not make a legal decision. |

An observer and implementer for the same case must be different people. Merely
using separate branches, accounts, prompts, or AI sessions does not create the
required independence.

## Observation-to-Implementation Procedure

1. Register every proposed source and its digest in the provenance ledger.
2. Obtain an explicit legal decision for the exact intended use. Pending work stops here.
3. Assign an observer and an implementer before observation begins.
4. Record the immutable oracle target, commands, session settings, raw evidence hash, and cleanup result.
5. Produce a behavior specification containing only necessary factual behavior and typed observations.
6. Have the specification reviewer approve the sanitized specification and its lineage.
7. Give the implementer only that approved specification and public project interfaces.
8. Review implementation and conformance evidence against the provenance and legal ledgers.
9. Permit a release claim only when the compatibility contract's complete-claim gate also passes.

The audit record for each case must identify the issue and case ID, observer,
specification reviewer, implementer, timestamps, provenance IDs, legal-review
ID, exact target ID, commands, raw evidence digest, specification path and
digest, review decision, and any deletion or cleanup event. Sensitive material
itself is never placed in an issue, pull request, CI log, or audit record.

## Behavior Specifications and Tests

A behavior specification may record inputs, typed results, protocol fields,
error numbers, severity, state, ordering, transaction state, side effects, and
other externally observable facts. It must not reproduce source code,
implementation structure, explanatory prose, or more expressive text than is
necessary to state the behavior.

Exact error messages, localized strings, system-object text, metadata names,
and display text remain individually gated until a qualified reviewer records
how they may be observed, retained, tested, and distributed. Until then, tests
use non-expressive structured fields and mark exact-text comparison as
`not-observed` or `blocked-legal` as applicable.

Every test derived from an external source or oracle names its parent
provenance records. Repository-authored tests with no external input remain
traceable through signed Git history and the pull-request audit fields.

## Fixture Controls

A fixture is any regular file whose repository-relative path contains a
`fixtures` component. Each fixture requires exactly one provenance record with:

- `source_kind` set to `fixture`
- the exact repository-relative `artifact_path`
- intended use `fixture`
- a SHA-256 digest matching the file bytes
- lineage to approved inputs and an approved legal review for fixture use

The governance scanner rejects unregistered files, duplicate records, wrong
source kinds, missing fixture use, digest mismatches, missing files, symlinks,
and pending or rejected use. Native database files and customer-derived data do
not become acceptable merely by being registered as fixtures.

## Dependency and CI Controls

The standard library and workspace code are preferred. A dependency is admitted
only for a necessary interoperability boundary, a materially safer
implementation, or a capability whose correct local implementation would be
disproportionate. Convenience alone is insufficient.

The technical license allowlist is `Apache-2.0`, `MIT`, `Unicode-3.0`, and
`Unlicense`. An allowlisted SPDX identifier is not a legal approval. All other,
unknown, low-confidence, or exceptional licenses fail until policy and legal
records are deliberately updated.

`deny.toml` rejects unlisted package versions, duplicate versions, wildcard
requirements, unknown registries, and all Git dependencies. Direct dependencies
are centralized in the workspace manifest and all resolved dependencies are
locked in `Cargo.lock`. `cargo deny check` and `cargo audit --deny warnings`
must pass. New or changed dependencies require:

1. a necessity explanation and minimal feature selection
2. exact upstream version, source, license expression, and archive digest
3. provenance and legal-review updates for direct or tool dependencies
4. lockfile and fail-closed `deny.toml` updates
5. advisory and license checks
6. a regenerated and validated CycloneDX 1.5 SBOM
7. required license text and attribution updates

CI tools are installed at exact versions with their own lockfiles. Third-party
actions are pinned to full commit SHAs. The workflow validates that the SBOM is
nonempty, identifies the pinned generator, and gives every component a name,
version, license, and SHA-256 hash before uploading it as evidence. The SBOM is
generated in CI and is not committed because UUID, timestamp, and environment
metadata are run-specific.

While `legal-review-third-party-dependencies` is pending, the workflow executes
only a first-party blocking step and the full governance job is statically
disabled. Enabling that job requires a dedicated, reviewed change that records
qualified approval for `dependency-inclusion` and
`supply-chain-verification`. The repository must also protect the legal ledger
and workflow paths with trusted required reviewers before activation. A ledger
edit, workflow success, or repository administrator action by itself is not
evidence that the reviewer is qualified or that a decision is authentic.

## Project License and Contributions

Apache License 2.0 is the selected project-license candidate and its official
text is stored in `LICENSE`. The repository copy is byte-identical to the
official text recorded by `prov-apache-license-2.0`. Adoption remains pending
under `legal-review-project-license`; this policy does not represent legal
approval of ownership, patent, notice, or distribution questions.

DCO 1.1 is the selected contribution mechanism. Activation remains pending
under `legal-review-contribution-policy`. Until that review is approved,
external contributions must not be merged. After activation, every commit must
carry the contributor's own `Signed-off-by` certification as described in
`CONTRIBUTING.md`; bots and maintainers may not fabricate it.

## Trademarks and Compatibility Statements

The candidate factual notice is:

> ntsql is an independent project and is not affiliated with, sponsored by, or
> endorsed by Microsoft. Product names are used only to identify compatibility
> targets.

This wording is not approved for release or marketing use while
`legal-review-trademark-policy` is pending. Microsoft logos and confusingly
similar branding are prohibited. Every compatibility statement also follows
the narrower claim and withdrawal rules in the compatibility contract. No
release may claim unqualified SQL Server compatibility.

## Legal-gated Surfaces

`storage.native-file-formats` remains `blocked-legal`, and no MDF, NDF, LDF, or
BAK implementation, fixture, parser, writer, or claim may enter the build.
Undocumented protocols, CLR internals, and external components require their
own feature record and legal review before implementation. A future reviewer
may authorize research without authorizing distribution; such work must live in
an access-restricted repository outside this workspace and must not be a Cargo
workspace member or release input.

## Contamination Response

1. Stop viewing, copying, building, or discussing the suspect material.
2. Do not attach it to an issue, pull request, chat, log, or CI artifact. Notify the maintainer privately with only an opaque incident identifier.
3. Revoke access and move the material to an access-restricted location outside this repository under counsel or incident-owner direction.
4. Identify affected people, commits, branches, caches, artifacts, releases, and downstream specifications without reproducing the material.
5. Delete contaminated repository and build copies and, when directed, purge Git history and published artifacts.
6. Mark affected provenance and legal records rejected or pending only through a qualified human decision; withdraw related compatibility claims immediately.
7. Assign an implementer who was not exposed and restart from independently approved public inputs and a newly reviewed behavior specification.
8. Record hashes, actions, dates, owners, and verification of deletion without retaining prohibited content.

The incident is closed only after the qualified reviewer and repository owner
confirm containment and the independently reimplemented result passes the full
technical and governance gates.