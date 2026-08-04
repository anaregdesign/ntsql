# ADR 0002: Stable Client Diagnostic Boundary

- Status: Accepted
- Date: 2026-08-04
- Issue: #35
- Extends: ADR 0001

## Context

Parser, binder, catalog, executor, transaction, and storage components will all
detect failures that may require a database client diagnostic. Their internal
errors need implementation detail such as source chains, backtraces, fault
context, and retry information. Those details are neither stable nor safe to
expose to a client.

The TDS adapter will eventually require exact client-visible fields. If a wire
token type or a general internal error type owned those fields, engine crates
would depend on an outer adapter or client behavior would drift as internal
errors changed. Target-specific number ranges and severity semantics are not
yet backed by approved behavior specifications and must not be invented here.

## Decision

`ntsql-diagnostics` owns the I/O-free client diagnostic vocabulary. It has no
external or workspace dependencies and exposes four stable values:

- `DiagnosticNumber`, preserving the full unsigned 32-bit value
- `DiagnosticSeverity`, preserving the full unsigned 8-bit value
- `DiagnosticState`, preserving the full unsigned 8-bit value
- `ClientDiagnostic`, preserving those values and exact message text

Number, severity, and state are distinct newtypes even where their storage
width matches. The crate does not normalize or reject message text, including
empty and whitespace-only text, because observable text policy belongs to an
approved target specification.

`ClientDiagnostic` is not an internal error container. It does not implement
`std::error::Error` and contains no source, backtrace, transport state,
serialization type, logging context, or protocol token. A component that needs
an internal cause owns a separate error type and associates a
`ClientDiagnostic` with it without placing the cause inside the client value.

A future request path will adapt diagnostics in this order:

1. An engine component chooses exact fields from an approved behavior
   specification under the injected `CompatibilityContext`.
2. The component retains internal failure context in its owning crate.
3. The server or protocol adapter adds connection-scoped token fields and
   encodes the client diagnostic.
4. Only the stable client fields cross the protocol boundary; internal causes
   remain available to separately governed, redacted observability code.

The dependency direction introduced by this ADR is:

```text
future syntax, binder, catalog, executor, transaction, storage
                              |
                              v
                    ntsql-diagnostics ---> standard library only
                              ^
                              |
                 future server and protocol adapters
```

`ntsql-diagnostics` must not depend on `ntsql-contract`, Serde, filesystem or
network adapters, protocol hosts, or future engine crates. Future consumers may
depend on both `ntsql-diagnostics` and `ntsql-compatibility`; neither policy
crate depends on the other because a diagnostic is request data rather than a
compatibility selector.

## Test Boundaries

- Public API tests verify exact preservation at every numeric width and for
  empty, whitespace-only, and non-normalized message text.
- Compile-fail documentation tests verify that numeric fields cannot be
  interchanged and that `ClientDiagnostic` is not an internal error type.
- `ntsql-architecture-check` registers the empty dependency allowlist and
  rejects representative contract, serialization, I/O, and protocol edges.
- Exact error numbers, messages, severities, and states enter later only from
  approved, provenance-linked behavior specifications.

## Consequences

Engine crates gain one lossless client-facing value without coupling to TDS or
internal diagnostics. A future protocol adapter must enrich and encode that
value explicitly. Some owning error types may pair an internal cause with a
client diagnostic, but that deliberate duplication keeps unstable details out
of the compatibility surface.