# Contributing to ntsql

Read `docs/governance.md` before opening an issue or pull request. Do not submit
confidential material, leaked material, decompiled or disassembled output,
third-party implementation code or tests, proprietary fixtures, customer data,
credentials, or copied product documentation.

External contribution intake is fail-closed while
`legal-review-contribution-policy` is `pending`. Pull requests may be discussed,
but they must not be merged until a qualified human reviewer approves the
recorded contribution mechanism.

## Developer Certificate of Origin

DCO 1.1 is the selected contribution mechanism, pending legal approval. Once
activated, certify each commit by adding your own sign-off:

```text
Signed-off-by: Your Name <your.email@example.com>
```

Create it with `git commit -s`. The sign-off certifies the Developer Certificate
of Origin 1.1 referenced by `prov-dco-1.1`; it is not a copyright assignment.
Only the contributor may provide their sign-off. Maintainers, bots, and AI
agents must not add or repair it on another person's behalf.

## Pull Requests

- Keep each change narrowly scoped and explain why each dependency is necessary.
- Complete the pull-request governance fields and identify all provenance and legal-review IDs.
- Keep observer and implementer roles separate for behavior derived from an oracle or external implementation input.
- Register every fixture before adding it and include its exact SHA-256 digest.
- Do not copy external prose, code, tests, exact messages, or binary content into comments or commits.
- Run the workspace tests, Clippy, formatting, dependency, advisory, fixture, and SBOM checks.
- Preserve `LICENSE`, `NOTICE`, third-party notices, and generated SBOM evidence as required.

Report suspected contamination privately to the repository owner. Do not place
the suspect content in a public or repository-hosted report.