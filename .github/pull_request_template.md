## Summary

Describe the externally visible change and its scope.

## Governance

- Provenance IDs: N/A
- Legal-review IDs: N/A
- Observer: N/A
- Specification reviewer: N/A
- Implementer: N/A
- [ ] No prohibited, confidential, copied, decompiled, or disassembled input was used.
- [ ] Every governed use is explicitly approved; `pending` is not approval.
- [ ] Observer and implementer are different people, or the change uses no externally derived behavior.
- [ ] Every new fixture is registered and its SHA-256 digest matches.
- [ ] Every dependency or CI action change includes source, license, digest, necessity, and review updates.
- [ ] Compatibility and trademark wording is limited to an approved, exact target claim.

## Authenticated Legal-decision Review

- Legal-review records changed: N/A
- Proposed attestation IDs: N/A
- Referenced evidence pull requests: N/A
- [ ] All decisions remain `pending`, or every non-pending decision requests review of the exact current head commit.
- [ ] Every non-pending decision includes the exact current provenance records for all direct sources and their complete recursive parent lineage.
- [ ] No pull-request author, automation, or repository administrator is represented as a qualified reviewer without an independently protected designation.

This pull-request description is not decision evidence. For each non-pending
decision, a designated qualified reviewer independently compares the complete
ledger record with the exact head commit, then submits an `Approve` review whose
body contains the following marker and JSON shape. Replace every angle-bracket
token; the two numeric fields must be unquoted JSON integers. Do not submit the
placeholder form.

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

## Validation

List the exact commands and results used to validate this change.
