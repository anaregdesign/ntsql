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

## Authenticated Legal-decision Evidence

Fields committed to `legal-reviews.json` are candidate-controlled assertions,
not proof that the named person made the decision. Every non-pending decision
therefore requires `decision_evidence` that names a repository, pull request,
and pre-agreed `attestation_id`. The decision is accepted only when an external
authority also contains a current GitHub approval that:

- came from a stable numeric account ID in the protected qualified-reviewer trust anchor
- was submitted by someone other than the pull-request author
- reviewed the exact current head commit of the referenced pull request
- remains in the `approved` state and is that reviewer's latest decisive review on that head
- contains the same `attestation_id` and complete decision snapshot as the ledger
- contains every current provenance record named by that decision and every record in their recursive parent lineage, with no missing, extra, duplicate, or altered record

The authority contract is
`contracts/schemas/legal-decision-authority.schema.json`. Its
`candidate_repository` and `candidate_commit_sha` bind the complete authority
snapshot to the candidate identified by the trusted event context. Its
`pull_requests` array carries a separate author, head commit, and authenticated
review set for every pull request referenced by the ledger. This permits
decisions to accumulate across pull requests without treating an approval of
one commit as approval of another. Every pull request and review must belong to
the authority's candidate repository, so evidence from another repository
cannot be replayed. Repository names, pull-request numbers,
review IDs, account IDs, timestamps, states, and commit SHAs must come from
authenticated GitHub API responses. Review IDs are unique across the authority;
an attestation ID may occur in at most one review for a given repository, pull
request, and reviewed commit. Historical reviews of different commits may
retain the same attestation ID, but they cannot satisfy the current-head check.
Reviewer logins are retained for audit readability, but the numeric account ID
is the stable identity.

The qualified reviewer submits an `Approve` review on the exact candidate head
commit. Its body contains exactly one block beginning with this ASCII marker,
followed immediately by a JSON object:

```text
ntsql-legal-decision-attestations:v1
```

```json
{
	"attestations": [
		{
			"attestation_id": "<pre-agreed-attestation-id>",
			"decision": {
				"id": "<legal-review-id>",
				"subject": "<exact-ledger-subject>",
				"status": "<approved-or-rejected>",
				"approved_uses": ["<approved-use-if-any>"],
				"prohibited_uses": ["<prohibited-use-if-any>"],
				"individual_review_uses": ["<individually-reviewed-use-if-any>"],
				"source_provenance_ids": ["<exact-provenance-id>"],
				"reviewed_by": {
					"github_account_id": "<replace-with-a-JSON-integer>",
					"github_login": "<authenticated-login>"
				},
				"decided_on": "<YYYY-MM-DD>",
				"decision_evidence": {
					"repository": "anaregdesign/ntsql",
					"pull_request_number": "<replace-with-a-JSON-integer>",
					"attestation_id": "<same-pre-agreed-attestation-id>"
				},
				"rationale": "<qualified-reviewer-rationale>"
			},
			"provenance_records": [
				{
					"id": "<exact-provenance-id>",
					"source_kind": "<exact-source-kind>",
					"title": "<exact-title>",
					"source_url": "<exact-HTTPS-URL-or-null>",
					"artifact_path": "<exact-repository-path-or-null>",
					"revision": "<exact-revision>",
					"retrieved_on": "<YYYY-MM-DD>",
					"author": "<exact-author>",
					"generation_method": "<exact-generation-method>",
					"environment": "<exact-environment-or-null>",
					"license": "<exact-license>",
					"content_digest": "sha256:<exact-64-hex-digest>",
					"intended_uses": ["<exact-intended-use>"],
					"parent_provenance_ids": ["<exact-parent-id-if-any>"],
					"legal_review_id": "<exact-legal-review-id>"
				}
			]
		}
	]
}
```

This is a non-submittable shape template: every angle-bracket token must be
replaced, numeric fields must be JSON integers rather than strings, empty arrays
must be used where the decision has no values, and no extra fields are allowed.
The parsed decision, including array order, must equal the ledger record. The
`provenance_records` array must equal the current records for every direct
`source_provenance_ids` entry and all recursively referenced parents. Array
order is not significant for selecting the closure, but every record and its
field values must match; unrelated records are prohibited. JSON `null`, rather
than the string `"null"`, must replace nullable provenance placeholders. The
pull-request description, comments, labels, commits, artifacts, and a copied
authority file are not attestations. An automation account, repository owner,
or administrator is not a qualified reviewer merely because it can approve or
merge.

### Producer and Consumer Boundary

The authority producer must be controlled independently from the candidate
checkout. It may be a GitHub App or a workflow whose definition and trust
configuration are loaded only from a protected source. Before any candidate
code executes, it must read the protected reviewer-ID allowlist, fetch the
referenced pull requests and their current reviews through the GitHub API,
parse the marked review-body blocks, and materialize one complete multi-PR
authority document outside the checkout. The producer records the candidate
repository and commit from the trusted event payload, not from command-line
values or files supplied by the candidate. It must use read-only permissions
and must not expose credentials or protected configuration to candidate code.

The consumer receives that document at
`$RUNNER_TEMP/legal-decision-authority.json`. The CLI rejects both a supplied
authority path inside the candidate workspace and a path whose resolved target
is inside it; symlinks cannot cross that boundary in either direction. Its
explicit interfaces are `legal-reviews <authority> <trusted-repository>
<trusted-commit>` and `fixtures <authority> <trusted-repository>
<trusted-commit>`. The repository and commit arguments must come from the
trusted event context and must match the authority's candidate target. Unknown
reviewers, self-review, stale commit approvals, cross-repository evidence,
altered decisions or provenance closure, duplicate or missing attestations,
and dismissed, changes-requested, or superseded review states fail closed. A
later `approved` or `changes-requested` review by the same reviewer on the same
head supersedes an earlier attested approval; comments and reviews of other
commits do not.
These checks authenticate who approved which exact ledger content and commit;
they do not make, infer, or validate the legal judgment itself.

The repository-built CLI is a contract implementation and local defense in
depth, not an activation trust anchor. A required check must run a pinned,
prebuilt verifier from a protected source before checkout or execution of any
candidate-controlled code. The current workflow therefore contains an
unconditional pre-checkout blocker and does not use the candidate-built CLI to
authenticate legal decisions.

### Activation Requirements and Current State

Before the authenticated gate can be activated, all of the following must be
established and independently reviewed:

1. Qualified reviewers are actually designated, and their stable numeric IDs are stored in a protected trust anchor outside candidate-controlled files.
2. The default branch requires pull requests and the governance check, dismisses stale approvals, requires approval after the latest push, and prevents unreviewed administrator bypass.
3. `legal-reviews.json`, its schemas, the governance workflow, and the reviewer trust configuration have protected ownership and change review.
4. The authority producer and pinned prebuilt verifier are loaded from protected sources, have only read permissions, bind the authority to the trusted event repository and commit, and complete before candidate checkout or execution.
5. Attack-path tests cover forged ledger metadata, candidate-supplied authority, path and symlink boundary bypasses, target and repository mismatch, provenance replay, stale, dismissed, changes-requested, and superseded reviews, reviewer login changes, self-approval, duplicate attestations, and decisions distributed across pull requests.

At the `2026-08-04` verification point, the repository was public and used
`main` as its default branch. The default branch required pull requests, one
approval, dismissal of stale approvals, approval by someone other than the
latest pusher, conversation resolution, linear history, and the strict
`Legal review required` check supplied by the GitHub Actions App. These rules
applied to administrators, and force pushes and branch deletion were disabled.
The repository still had no independently designated qualified reviewer,
protected path ownership, reviewer trust anchor, environment, or `CODEOWNERS`
file. Consequently the first-party `legal-review-gate` remains an unconditional
failure, the full governance job remains statically disabled and blocked before
checkout, no protected authority producer or prebuilt verifier is installed,
and all legal decisions remain pending. Weakening the failure to obtain a green
check is prohibited.

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