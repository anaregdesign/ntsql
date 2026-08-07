# ADR 0063: Versioned Database Manifest Codec

- Status: Accepted
- Date: 2026-08-07
- Issue: #182
- Extends: ADR 0001, ADR 0062
- Follows: #181

## Context

ADR 0062 defines inert database, lifecycle-generation, file-role, and persistent
WAL identities plus staged ownership. No reviewed persistent record binds those
identities into one database composition. Opening independent child files by
path or trusting their self-reported numeric fields cannot establish a
database-wide identity.

This issue must define the pure data boundary before any path, lock, atomic
publication, create, open, recovery, close, or drop effect exists. The codec must
not turn bytes into a selected owner or reserve underspecified clean/tombstone
semantics merely because later issues will need those states.

## Decision

Extend `ntsql-database` with inert manifest-domain values:

- `DatabaseStorageFormatVersion`, a checked nonzero `u16`;
- `DatabaseStorageFormatRequirements`, one version for each fixed file role;
- `DatabaseRequiredFeatures`, a checked required-feature bit set;
- `DatabaseManifestLifecycleState`; and
- `DatabaseManifest`, which binds those values to one
  `DatabaseCompositionIdentity`.

Manifest version 1 supports only `RecoveryRequired`. `DatabaseRequiredFeatures`
supports only the empty set. Later clean-close and tombstone work must introduce
the evidence fields and version policy that make those states authoritative; it
may not reinterpret a version-1 state value or currently unknown feature bit.

Add `database_manifest_codec` to `ntsql-storage-file`. It is a pure-memory,
allocation-free encoder/decoder with no `File`, `Path`, lock, synchronization,
publication, recovery, or storage-owner value in its signatures.

## Dependency Boundary

No dependency edge changes. ADR 0062 already established and the architecture
checker already requires:

```text
ntsql-database -------> ntsql-wal
ntsql-storage-file ---> ntsql-database, ntsql-page, ntsql-transaction, ntsql-wal
```

The database domain owns validated logical values and never depends on the
codec. The filesystem adapter owns bytes and depends inward. Existing negative
tests reject the reverse edge in every Cargo dependency kind.

## Version 1 Frame

Version 1 is exactly 160 bytes. Every multibyte value is unsigned big-endian.
There is no variable section, count, padding outside the frame, or accepted
trailing data.

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | header magic, ASCII `NTSQDBM1` |
| 8 | 2 | manifest format version, exactly `1` |
| 10 | 2 | frame length, exactly `160` |
| 12 | 4 | header flags, exactly zero |
| 16 | 16 | nonzero repository-owned database ID |
| 32 | 8 | nonzero lifecycle generation |
| 40 | 1 | lifecycle state, `1` = recovery required |
| 41 | 7 | reserved zero bytes |
| 48 | 16 | nonzero persistent WAL identity |
| 64 | 16 | nonzero WAL file identity |
| 80 | 16 | nonzero page-store file identity |
| 96 | 16 | nonzero restart-checkpoint file identity |
| 112 | 2 | nonzero required WAL format version |
| 114 | 2 | nonzero required page-store format version |
| 116 | 2 | nonzero required restart-checkpoint format version |
| 118 | 2 | reserved zero bytes |
| 120 | 8 | required feature bits, exactly zero in version 1 |
| 128 | 16 | reserved zero bytes |
| 144 | 8 | footer magic, ASCII `NTSQDBE1` |
| 152 | 8 | checksum of bytes `0..152` |

The three file IDs are globally distinct within one composition. Their
namespaces remain separate from `DatabaseId` and `PersistentLogId`, so equal
numbers across those other domains have no implicit relationship.

Header flags and required feature bits are distinct. Version 1 rejects every
nonzero value in both fields. A future optional or required capability must
receive a reviewed semantic definition and format/version decision before its
bit is accepted.

## Checksum

The frame reuses the repository-owned checksum arithmetic already specified by
ADRs 0013, 0044, and 0049:

1. initialize state to `0x4e5453514c434b31`;
2. XOR each protected byte;
3. wrapping-multiply by `0x4e5453514c57414d`;
4. rotate left seven and XOR `0x434845434b53554d`; and
5. XOR the final state with the wrapping protected-byte count.

Protection covers the complete header, every identity/requirement/reserved byte,
and footer magic. Only the final eight checksum bytes are excluded. Reuse is
limited to arithmetic; manifest bytes have independent magic and version
dispatch and cannot be confused with WAL, page-store, or checkpoint bytes.

## Encoding

`encode_database_manifest` accepts only a validated `DatabaseManifest` and
returns `[u8; 160]`. It performs no allocation and cannot fail:

- every scalar was checked before manifest construction;
- the only supported lifecycle state and feature set have canonical codes;
- all reserved and header-flag bytes start at zero; and
- fixed offsets and width remove length arithmetic.

The returned array is inert. Encoding is not publication, durability,
selection, recovery, or lifecycle authority.

## Decoding and Validation Order

`decode_database_manifest` validates:

1. supplied length, returning truncation for every prefix and trailing-data for
   every suffix;
2. header magic, exact supported version, exact declared frame length, and zero
   header flags;
3. footer magic and checksum;
4. every reserved byte in ascending range order;
5. nonzero database ID and lifecycle generation;
6. the supported lifecycle-state discriminant;
7. nonzero persistent WAL identity;
8. each nonzero file-role identity and the complete distinct role set;
9. each nonzero required child-format version; and
10. the required-feature bit set.

Every failure is a typed `DatabaseManifestDecodeError` retaining the exact
length, bytes, version, flags, offset, discriminant, role, checksum, or nested
domain error relevant to that boundary. The decoder does not normalize, infer,
choose a fallback version, default an identity, accept a merely larger
generation, or fabricate a client diagnostic.

Success returns a validated inert `DatabaseManifest`, not an unbound, selected,
recovery-required, live, close-pending, or drop-pending owner. In particular,
decoded numbers cannot invoke the private transitions reserved by ADR 0062.

## Lifecycle Successor Validation

An isolated frame has no previous generation against which it could detect
regression. `DatabaseManifest::require_successor_of` explicitly compares a
proposed manifest with the retained previous manifest:

1. database, child-file, and persistent-WAL identities must be unchanged;
2. lifecycle generation must be the exact adjacent successor;
3. every required child-format version must be unchanged; and
4. required feature bits must be unchanged.

Regression/equality, skipped generations, exhaustion, foreign identities,
format changes, and feature changes are distinct typed errors. Migration may
change formats/features only through a later reviewed migration authority, not
through this lifecycle-successor path.

## Golden Evidence and Tests

The repository-authored version-1 golden frame fixes every byte for distinct,
human-readable identity ranges, WAL/page/checkpoint versions `4/1/1`, empty
features, and recovery-required state. It is independent from external product
bytes.

Focused tests cover:

- deterministic exact golden encoding and round-trip;
- every truncated prefix and one trailing byte;
- header/footer magic, format version, declared length, header flags, and
  checksum independently;
- every reserved byte with a recomputed checksum;
- zero database, lifecycle, persistent-WAL, and each file-role identity;
- all three cross-role duplicate file-ID pairs;
- unsupported lifecycle-state values;
- zero required format version for every role;
- low and high unknown required-feature bits;
- maximum nonzero host-independent values; and
- decoded lifecycle regression against an exact previous manifest.

Domain unit and compile-fail tests separately cover checked feature/format
construction, exact successor preservation, exhaustion, and inability to
promote a manifest into live authority.

## Evidence and Compatibility Boundary

All bytes, fields, checksum use, states, feature policy, errors, and fixtures are
repository-authored. No external product documentation, SDK, driver, fixture,
oracle, captured output, proprietary governance tool, or native file is
consulted.

This format is not MDF, NDF, LDF, BAK, a SQL Server boot page, a database ID, a
file ID, an LSN, a recovery state, an error code, or a compatibility claim.

## Non-Goals

This ADR does not:

- read, write, create, replace, synchronize, select, or lock a manifest file;
- define candidate filenames, atomic publication, or crash outcomes;
- create child WAL/page/checkpoint files;
- acquire database-wide or child-file ownership;
- execute recovery or expose live storage;
- define a clean-close certificate or tombstone;
- support format/feature migration or version fallback;
- define allocation, heap/index, buffering, backup, or protocol behavior; or
- define native Microsoft persistent bytes or undocumented behavior.

## Consequences

Database identity now has one exact, corruption-detecting, versioned persistent
representation without broadening domain authority or adding I/O. Atomic
publication and opened-object validation remain separate. Issues #184 and #185
may consume only a successfully decoded inert manifest while retaining the
database-wide owner introduced by #183 and required to turn later evidence into
staged authority.
