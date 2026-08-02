# ntsql

ntsql is an independent Rust implementation targeting externally observable compatibility with Microsoft SQL Server Database Engine. Compatibility is tracked against exact, immutable oracle targets; the project does not currently claim complete SQL Server compatibility.

ntsql is not affiliated with, sponsored by, or endorsed by Microsoft. Product names are used only to identify compatibility targets. This candidate notice and all release-facing trademark wording remain subject to `legal-review-trademark-policy`.

The first deliverable is the versioned [compatibility contract](docs/compatibility-contract.md). Its machine-readable target matrix, feature inventory, provenance ledger, legal-review ledger, and JSON Schemas live under `contracts/` and are validated by the `ntsql-contract` crate. All contributors must follow the [clean-room and supply-chain policy](docs/governance.md).

## Development

The workspace requires Rust 1.88 or later.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
cargo audit --deny warnings
cargo run --locked -p ntsql-contract --bin ntsql-governance -- fixtures
```

External dependencies are exceptional. The current direct dependencies, `serde` and `serde_json`, are restricted to the public JSON contract boundary.

Apache-2.0 is the selected project-license candidate. The standard text is in `LICENSE`; adoption and the proposed DCO 1.1 contribution process remain pending qualified human legal review as recorded in the legal-review ledger.
