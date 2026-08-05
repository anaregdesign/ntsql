# ADR 0003: Pinned Rust Toolchain and MSRV

- Status: Accepted
- Date: 2026-08-05
- Issue: #40

## Context

The workspace previously named Rust 1.88 inconsistently across Cargo metadata,
developer instructions, and CI commands. A growing crate graph would make a
later compiler migration harder to isolate. Selecting whichever compiler is
already installed would also make formatting, lints, and accepted language
features non-reproducible.

The Rust distribution is an external build input. Its exact version,
components, manifest integrity, license metadata, and bootstrap trust boundary
must be visible without treating inventory metadata as legal approval.

## Decision

`rust-toolchain.toml` is the authoritative development and CI configuration. It
pins Rust 1.97.1 with the `minimal` profile plus `clippy` and `rustfmt`.
The workspace `rust-version` is also 1.97.1, so this release is both the
required validation toolchain and the MSRV. No separate older-compiler lane is
maintained.

Repository commands rely on rustup's directory override instead of setting a
process-global default. Offline provenance validation requires the Cargo MSRV
to equal the exact channel pin, rejects the higher-precedence legacy
`rust-toolchain` filename, and requires the corresponding distribution manifest
record. Online provenance validation derives the immutable manifest URL from the
pin, downloads it without redirects, and verifies its SHA-256 digest. The
recorded digest was also compared with the official adjacent `.sha256` file.

Rustup remains the bootstrap and component installer supplied by the developer
environment or GitHub runner. Rustup verifies component hashes from the pinned
channel manifest. The runner image, rustup bootstrap, operating-system trust
store, HTTPS transport, and Rust distribution host remain environmental trust
boundaries. The repository-built verifier is defense in depth, not an
independent trust anchor, and the existing qualified legal-review gate remains
pending.

## Consequences

All local and CI validation selects one compiler, Cargo, formatter, and Clippy
release. A toolchain update requires a dedicated issue that changes the pin,
MSRV, manifest provenance, documentation, and validation evidence together.
`Cargo.lock` and third-party crate versions do not change merely because the
toolchain changes.
