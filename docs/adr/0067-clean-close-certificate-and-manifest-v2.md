# ADR 0067: Clean-Close Certificate and Manifest V2

- Status: Accepted
- Date: 2026-08-08
- Issue: #186
- Extends: ADR 0001, ADR 0062, ADR 0063, ADR 0066
- Follows: #185

## Context

ADR 0066 keeps a live database's manifest durably `RecoveryRequired`; no
lifecycle gate currently publishes an orderly-close outcome. Issue #186 owns
`Live -> ClosePending -> Closed`, deriving a repository-owned close
certificate from fresh locked observations, flush/synchronize ordering,
close-manifest publication, and fresh reopen after every outcome.

That scope is too large for one reviewable change. This decision extracts the
first slice: the inert domain values a close certificate and a successor
manifest state need, plus the pure-memory codec that can encode and decode
them. It does not implement transaction close orchestration, the
`Live -> ClosePending` transition, filesystem close publication, or clean-open
authority. Those remain later PRs within #186.

ADR 0063 fixes manifest version 1 at exactly 160 bytes and supports only
`RecoveryRequired`. Reusing that frame for a second lifecycle state would
either grow a byte-for-byte-frozen format or silently reinterpret existing
bytes. Neither is acceptable: V1 golden bytes and every V1 decode failure must
stay exact, and a `Clean` state must never be reachable through stale V1
bytes.

## Decision

Extend `ntsql-database` with one inert, `Copy` clean-close certificate and one
additional manifest lifecycle state:

- `DatabaseCleanCloseCertificate` binds exactly:
  - the source `RecoveryRequired` `DatabaseLifecycleGeneration` the
    certificate was derived from;
  - an optional durable WAL frontier, represented canonically (`Some` must be
    nonzero; absence is the only zero-shaped value);
  - a nonzero allocated transaction-epoch high-water;
  - a nonzero selected completeness-checkpoint anchor version;
  - the selected completeness-checkpoint anchor value (`u128`, zero allowed);
  - a portable `u64` transaction-entry count; and
  - a portable `u64` page-entry count.
- `DatabaseManifestLifecycleState` gains `Clean(DatabaseCleanCloseCertificate)`
  alongside the existing `RecoveryRequired`.
- `DatabaseManifest::clean` and `DatabaseManifest::next_clean` construct a
  `Clean` manifest only when the certificate's source generation is the exact
  predecessor of the target manifest generation, reusing
  `DatabaseLifecycleGeneration::require_successor`.
- `DatabaseManifest::require_successor_of` continues to enforce exact adjacent
  generation for both states and additionally rejects `Clean -> Clean` as an
  invalid lifecycle-state pairing.
  `RecoveryRequired -> RecoveryRequired`, `RecoveryRequired -> Clean`, and
  `Clean -> RecoveryRequired` remain allowed; the last one is a later PR's
  concern to invoke, not this one's to forbid.

The certificate and the `Clean` manifest state are inert. Neither a
caller-built nor a decoded value grants live, close-pending, closed, or
clean-open authority; they select a lifecycle-state value only.

Add `database_manifest_codec_v2` to `ntsql-storage-file`:

- `encode_database_manifest_v2` and `decode_database_manifest_v2` are pure,
  allocation-free, fixed-size functions with independent magic, version, and
  exports from V1.
- The V1 encoder becomes fallible: `encode_database_manifest` now returns
  `Result<[u8; 160], DatabaseManifestV1UnsupportedLifecycleState>` and rejects
  `Clean` explicitly rather than encoding it or relying on an unreachable
  assumption. V1 golden bytes and every V1 decode failure remain exact.
- Filesystem open remains V1-only in this PR; `Clean` cannot be selected by
  any existing open path.

## Dependency Boundary

No dependency edge changes. `ntsql-database` remains I/O-free; the new
certificate and lifecycle-state values depend only on existing domain types
already reviewed by ADR 0062. `ntsql-storage-file` already depends on
`ntsql-database`, so the new V2 codec module adds no edge:

```text
ntsql-database -------> ntsql-wal
ntsql-storage-file ---> ntsql-database, ntsql-page, ntsql-transaction, ntsql-wal
```

## Version 2 Frame

Version 2 is exactly 256 bytes, independent of V1's 160-byte frame. Every
multibyte value is unsigned big-endian. Offsets `0..128` keep the same
semantic fields and offsets as V1's `0..128`, except that the lifecycle-state
byte at offset 40 now accepts two codes:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | header magic, ASCII `NTSQDBM2` |
| 8 | 2 | manifest format version, exactly `2` |
| 10 | 2 | frame length, exactly `256` |
| 12 | 4 | header flags, exactly zero |
| 16 | 16 | nonzero repository-owned database ID |
| 32 | 8 | nonzero lifecycle generation |
| 40 | 1 | lifecycle state, `1` = recovery required, `2` = clean |
| 41 | 7 | reserved zero bytes |
| 48 | 16 | nonzero persistent WAL identity |
| 64 | 16 | nonzero WAL file identity |
| 80 | 16 | nonzero page-store file identity |
| 96 | 16 | nonzero restart-checkpoint file identity |
| 112 | 2 | nonzero required WAL format version |
| 114 | 2 | nonzero required page-store format version |
| 116 | 2 | nonzero required restart-checkpoint format version |
| 118 | 2 | reserved zero bytes |
| 120 | 8 | required feature bits, exactly zero in version 2 |
| 128 | 8 | certificate source generation |
| 136 | 1 | frontier-presence flag, `0` or `1` |
| 137 | 7 | reserved zero bytes |
| 144 | 8 | durable WAL frontier, canonical zero iff absent |
| 152 | 8 | allocated transaction-epoch high-water |
| 160 | 2 | checkpoint anchor version |
| 162 | 6 | reserved zero bytes |
| 168 | 16 | checkpoint anchor value |
| 184 | 8 | transaction-entry count |
| 192 | 8 | page-entry count |
| 200 | 40 | reserved zero bytes |
| 240 | 8 | footer magic, ASCII `NTSQDBE2` |
| 248 | 8 | checksum of bytes `0..248` |

When lifecycle state is `1` (`RecoveryRequired`), the entire certificate area
`128..240` must be exactly zero; any nonzero byte in that range is rejected
before the certificate is even considered. When lifecycle state is `2`
(`Clean`), every certificate field is decoded and validated, and the
certificate's source generation must be the exact predecessor of the
manifest's own lifecycle generation at offset `32`.

Header flags and required feature bits keep the same zero policy as V1;
version 2 defines no additional capability bit.

## Checksum

The V2 frame reuses the exact repository-owned checksum arithmetic already
specified by ADR 0063 and applied unchanged over the wider `0..248` range;
only the final eight checksum bytes are excluded.

## Encoding

`encode_database_manifest_v2` accepts a validated `DatabaseManifest` in either
supported lifecycle state and returns `[u8; 256]`. It performs no allocation
and cannot fail: every scalar was checked before manifest or certificate
construction, both lifecycle-state codes are canonical, an absent frontier
encodes as canonical zero, and all reserved/header-flag bytes start at zero.

The returned array is inert, exactly like the V1 encoding: it is not
publication, durability, selection, recovery, or lifecycle authority.

## Decoding and Validation Order

`decode_database_manifest_v2` validates, in order:

1. supplied length, returning truncation for every prefix and trailing-data
   for every suffix;
2. header magic, exact supported version, exact declared frame length, and
   zero header flags;
3. footer magic and checksum;
4. every common reserved byte (`41..48`, `118..120`) in ascending range order;
5. nonzero database ID and lifecycle generation;
6. the supported lifecycle-state discriminant (`1` or `2`);
7. nonzero persistent WAL identity;
8. each nonzero file-role identity and the complete distinct role set;
9. each nonzero required child-format version;
10. the required-feature bit set; and then, depending on the decoded
    lifecycle-state discriminant:
    - `RecoveryRequired`: the entire certificate area `128..240` must be zero;
    - `Clean`: certificate-specific reserved bytes (`137..144`, `162..168`,
      `200..240`), the frontier-presence flag, canonical-zero frontier
      absence, nonzero certificate source generation, the domain
      certificate's own validated construction, and the certificate's exact
      predecessor relation to the manifest's lifecycle generation.

Every failure is a typed `DatabaseManifestV2DecodeError` retaining the exact
length, bytes, version, flags, offset, discriminant, role, checksum, or
nested domain error relevant to that boundary, mirroring `DatabaseManifestDecodeError`'s
style. The decoder does not normalize, infer, choose a fallback version,
default an identity, accept a merely larger generation, or fabricate a client
diagnostic.

Success returns a validated inert `DatabaseManifest`, never an unbound,
selected, live, close-pending, or clean-open-authorized owner. Decoded
numbers cannot invoke the private transitions reserved by ADR 0062 or ADR
0066; this is proven by compile-fail tests in both the domain and codec
layers.

## Golden Evidence and Tests

Repository-authored version-2 golden frames fix every byte for both lifecycle
states, reusing distinct human-readable identity ranges independent from
external product bytes. Focused tests cover:

- deterministic exact golden encoding and round-trip for `RecoveryRequired`
  and `Clean`;
- every truncated prefix and one trailing byte;
- header/footer magic, format version, declared length, header flags, and
  checksum independently;
- every common reserved byte and every certificate-specific reserved byte with
  a recomputed checksum;
- the entire certificate area rejected as nonzero under `RecoveryRequired`;
- zero database, lifecycle, persistent-WAL, and each file-role identity;
- all three cross-role duplicate file-ID pairs;
- unsupported lifecycle-state values;
- zero required format version for every role and unknown required-feature
  bits;
- unsupported frontier-presence values and non-canonical absent-frontier
  bytes;
- zero certificate source generation, zero allocated epoch, and zero
  checkpoint anchor version;
- certificate source-generation skip and regression against the manifest's
  own generation;
- maximum nonzero host-independent values for both lifecycle states; and
- decoded lifecycle regression against an exact previous manifest.

Domain unit and compile-fail tests separately cover checked certificate
construction, all state transitions (`RecoveryRequired -> RecoveryRequired`,
`RecoveryRequired -> Clean`, `Clean -> RecoveryRequired`, and the rejected
`Clean -> Clean`), source-generation mismatch/regression/skip/exhaustion,
optional-frontier zero rejection, zero epoch/anchor-version rejection, maxima,
and inability to promote either the certificate or the manifest into live or
closed authority.

## Evidence and Compatibility Boundary

All bytes, fields, checksum use, states, feature policy, errors, and fixtures
are repository-authored. No external product documentation, SDK, driver,
fixture, oracle, captured output, proprietary governance tool, or native file
was consulted in this decision or its implementation.

This format is not MDF, NDF, LDF, BAK, a SQL Server boot page, a database ID,
a file ID, an LSN, a recovery state, an error code, or a compatibility claim.

## Non-Goals

This ADR does not:

- implement `Live -> ClosePending -> Closed` transitions or any consuming
  close orchestration;
- derive a certificate from live WAL, page, checkpoint, or transaction state;
- flush, synchronize, or publish a close-manifest file;
- define a close-manifest candidate namespace;
- grant clean-open authority or change fresh-open's structural/recovery
  validation;
- change filesystem or memory create/open to select version 2; open remains
  V1-only after this PR;
- support format/feature migration or version fallback beyond the two fixed
  codes defined here; or
- define native Microsoft persistent bytes or undocumented behavior.

## Consequences

`ntsql-database` and `ntsql-storage-file` now carry the inert domain and codec
building blocks a later close transition needs, without granting any new
authority or changing any existing open path. V1 stays byte-for-byte
unchanged and V1 decoding still supports only `RecoveryRequired`. The
remaining #186 acceptance criteria — certificate derivation from live state,
flush/synchronize ordering, close-manifest publication, clean-open authority,
and destructor/retry policy — are explicitly deferred to later PRs within the
same issue.
