# JSON Schema Contract Corpus

This directory contains workspace-authored positive and negative instances for
the eight published JSON Schemas. It contains no SQL Server observations,
third-party examples, or imported test data.

Each corpus starts from either a repository-relative `source` document or an
inline synthetic document. Cases apply the document-preserving subset of RFC
6902 `add`, `remove`, `replace`, and `copy` operations. Root `add`, `replace`,
and `copy` are supported; root `remove` is excluded because every case must
leave a JSON instance to validate. A case records four independent expectations:

- `json_schema`: the expected result from a Draft 2020-12 validator
- `rust_deserialize`: whether the instance maps to the published Rust type
- `rust_schema_semantics`: whether deserialization plus the typed schema-boundary validator accepts it
- `rust_full_validation`: whether all standalone and supplied cross-record checks accept it

Inline legal-review, legal-decision authority, specification-review authority,
and behavior-admission documents
are synthetic schema-boundary instances. Their `approved` values do not record,
imply, authenticate, or replace a qualified human legal decision or technical
specification review for any source, behavior, dependency, tool, or project
activity.

The `format` keyword is annotation-only under the Draft 2020-12 default
meta-schema. Lexical date and timestamp patterns are asserted; calendar and
timezone semantics are not asserted by this corpus.

The first-party Rust test runner applies every patch and checks the three Rust
expectations. It requires exactly one corpus for each published schema and
binds each corpus `schema_id` directly to a published schema `$id`. For
instances that deserialize, `json_schema` and `rust_schema_semantics` must have
the same expected result. Draft 2020-12 `integer` also accepts mathematically
integral decimal and exponent representations such as `1.0` and `1e0`; the
fixed Rust integer wire types reject those JSON representations at the separate
deserialization boundary. Those prefilter differences are explicit cases, not
schema-semantic mismatches.

The runner does not claim to compile or execute the JSON Schemas. A standards
validator may be connected only after dependency provenance, license, security,
SBOM, and qualified legal-review gates are satisfied. That gate is currently
pending under `legal-review-third-party-dependencies`.

The corpus standardizes acceptance, not validator diagnostics. JSON Schema
implementations expose different keyword, location, and error-code formats;
Rust violation codes remain covered by first-party Rust tests instead of being
presented as portable JSON Schema results.

Context-bound checks that cannot be decided from one corpus instance and its
declared companion documents remain Rust-only tests. These include
authenticated reviewer selection, governed-use authorization, conformance
references, and implementation-input authorization.
