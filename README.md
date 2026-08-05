# ntsql

ntsql is an independent Rust implementation targeting externally observable compatibility with Microsoft SQL Server Database Engine. Compatibility is tracked against exact, immutable oracle targets; the project does not currently claim complete SQL Server compatibility.

ntsql is not affiliated with, sponsored by, or endorsed by Microsoft. Product names are used only to identify compatibility targets. This candidate notice and all release-facing trademark wording remain subject to `legal-review-trademark-policy`.

The first deliverable is the versioned [compatibility contract](docs/compatibility-contract.md). Its machine-readable target matrix, feature inventory, provenance ledger, legal-review ledger, and JSON Schemas live under `contracts/` and are validated by the `ntsql-contract` crate. All contributors must follow the [clean-room and supply-chain policy](docs/governance.md).

## Development

The exact development and CI toolchain is pinned in `rust-toolchain.toml`.
Rust/Cargo 1.97.1 is also the workspace MSRV; older compilers are unsupported.
With rustup shims on `PATH`, repository commands select and install the pin
automatically. `rustup show active-toolchain` confirms the selected release.

```sh
rustc --version
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo run --locked -p ntsql-architecture-check
cargo deny check
cargo audit --deny warnings
cargo run --locked -p ntsql-contract --bin ntsql-governance -- fixtures
cargo run --locked -p ntsql-contract --bin ntsql-governance -- provenance-offline
```

External dependencies are exceptional. The current direct dependencies, `serde` and `serde_json`, are restricted to the public JSON contract boundary.
The reviewed crate responsibilities and dependency direction are recorded in
[ADR 0001](docs/adr/0001-compatibility-context-and-crate-boundaries.md) and
enforced by `ntsql-architecture-check`.
The toolchain pin, MSRV decision, and bootstrap trust boundary are recorded in
[ADR 0003](docs/adr/0003-pinned-rust-toolchain.md).

Apache-2.0 is the selected project-license candidate. The standard text is in `LICENSE`; adoption and the proposed DCO 1.1 contribution process remain pending qualified human legal review as recorded in the legal-review ledger.
