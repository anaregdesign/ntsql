# ntsql

ntsql is an independent Rust implementation targeting externally observable compatibility with Microsoft SQL Server Database Engine. Compatibility is tracked against exact, immutable oracle targets; the project does not currently claim complete SQL Server compatibility.

The first deliverable is the versioned [compatibility contract](docs/compatibility-contract.md). Its machine-readable target matrix, feature inventory, and JSON Schemas live under `contracts/` and are validated by the `ntsql-contract` crate.

## Development

The workspace requires Rust 1.88 or later.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

External dependencies are exceptional. The current direct dependencies, `serde` and `serde_json`, are restricted to the public JSON contract boundary.
