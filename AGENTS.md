# Repository Instructions

These instructions apply to the entire repository. Read the owning ADR and the
nearest tests before changing a boundary. Keep changes small, reviewable, and
linked to an issue with explicit acceptance criteria.

## Toolchain and Validation

- Use Rust 1.88.0, edition 2024, and the committed `Cargo.lock`.
- Before a pull request, run:

```sh
rustup run 1.88.0 cargo fmt --all -- --check
rustup run 1.88.0 cargo test --workspace --all-features --locked
rustup run 1.88.0 cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
rustup run 1.88.0 cargo run --locked -p ntsql-architecture-check
rustup run 1.88.0 cargo run --locked -p ntsql-contract --bin ntsql-governance -- fixtures
git diff --check
```

- Do not weaken a required check to obtain a merge. Record external blockers in
  an open issue and continue with independently valid work on a stacked branch.

## Architecture

- Keep domain crates I/O-free. Domain code must not depend on JSON contracts,
  Serde, filesystems, networks, clocks, oracles, protocol hosts, or persistence
  adapters.
- Use one immutable, exact-target `CompatibilityContext`. Do not add global
  target state, baseline fallback, or scattered version/edition conditionals.
- Keep client-visible diagnostics in `ntsql-diagnostics`. Internal causes,
  backtraces, logging context, transport failures, and wire tokens stay outside
  `ClientDiagnostic`.
- Add a crate only for a concrete owned responsibility. Update the owning ADR,
  register its complete direct dependency set in `ntsql-architecture-check`,
  and add a negative dependency test in the same change.
- Prefer the standard library and workspace-owned code. Do not add a dependency
  for convenience or create a catch-all shared/core crate.
- Preserve staged types and dependency direction. Outer adapters may depend on
  domain ports; domain crates never depend on adapters.
- For client input, corruption, disconnects, and resource exhaustion, return
  explicit errors instead of panicking. `unsafe`, `unwrap`, `expect`, `panic`,
  `todo`, and `unimplemented` are prohibited by workspace lint policy.

## Compatibility and Governance

- Implement only workspace-authored behavior or behavior backed by an approved,
  provenance-linked specification. `pending` is not approval.
- Do not consult or execute unapproved product documentation, proprietary
  oracles, SDKs, drivers, captured output, external fixtures, or governance
  tools. Do not infer native MDF/NDF/LDF/BAK formats or undocumented protocols.
- Never invent a legal decision, reviewer, approval date, attestation, source
  digest, compatibility result, or trademark conclusion.
- Keep raw external evidence outside the repository. A behavior specification
  may enter only through the observer/reviewer/implementer separation defined in
  `docs/governance.md`.
- Do not add or update third-party dependencies, CI actions, fixtures, licenses,
  or release-facing compatibility claims without their required provenance,
  digest, advisory, SBOM, and authenticated legal-review records.
- Preserve exact externally observable values only when the approved contract
  requires them. Do not speculate about error numbers, text, ranges, metadata,
  ordering, or target-specific semantics.

## Delivery

- Use focused tests that first falsify the local behavior hypothesis. Scale test
  coverage with the blast radius and rerun the narrow check after each edit.
- Keep pull requests small and merge stacked work from the tip toward the
  integration branch. Do not close an issue unless every acceptance criterion is
  satisfied; use `Refs` and leave a concrete follow-up issue for remaining work.
- Do not mix unrelated refactors, generated files, downloaded evidence, or
  formatting churn into a feature change.
- Update downstream issues when implementation reveals a new dependency,
  security boundary, legal gate, or measurable acceptance criterion.

The full policies remain authoritative in `docs/governance.md`,
`docs/compatibility-contract.md`, and `docs/adr/`.