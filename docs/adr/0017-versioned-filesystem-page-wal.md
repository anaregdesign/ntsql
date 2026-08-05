# ADR 0017: Versioned Filesystem Page WAL

- Status: Accepted
- Date: 2026-08-06
- Issue: #84
- Extends: ADR 0001, ADR 0008, ADR 0012, ADR 0013, ADR 0014, ADR 0016
- Extended by: ADR 0018

## Context

ADR 0013 defines a persistent transaction-only WAL format whose version 1 bytes
and recovery behavior are already accepted. ADR 0016 adds full-image page
records and a shared transaction/page order only to the deterministic memory
model. Replacing or silently extending version 1 would invalidate its strict
kind, length, and reserved-byte rules.

The first persistent page WAL must therefore use a new explicit format version,
preserve version 1 byte-for-byte, and recover a partial multi-frame page append
without treating a physically complete fragment as a complete logical record.

## Crate and Port Boundary

`ntsql-storage-file` adds the inward dependency `ntsql-page`, making its exact
direct set:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

The page-capable adapter implements `PageLog<N>` alongside the existing commit,
epoch, durability, and transaction-recovery ports. Domain crates remain I/O-free
and cannot depend on the adapter.

## Version 1 Preservation

Version 1 creation, opening, header and frame bytes, three frame kinds, checksum
vectors, contiguous commit positions, durable markers, tail repair, fault
effects, poisoning, and recovery results do not change. A version 1 instance
rejects page append before fault consumption, capacity reservation, file I/O,
or position advancement.

Version-specific open entrypoints reject the other header version before frame
scanning or tail mutation. No implicit migration or width fallback exists.

## Version 2 Header

Every integer is unsigned and big-endian. The immutable header remains exactly
64 bytes:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | format magic, ASCII `NTSQLOG1` |
| 8 | 2 | format version, exactly 2 |
| 10 | 2 | header length, exactly 64 |
| 12 | 4 | flags, exactly zero |
| 16 | 16 | nonzero `PersistentLogId` |
| 32 | 8 | nonzero const page-image width `N` |
| 40 | 16 | reserved zero bytes |
| 56 | 8 | checksum of bytes 0 through 55 |

Creation converts `N` explicitly to `u64` and rejects zero or an unrepresentable
value before creating a path. Open validates the stored width equals its exact
const `N` before any repair.

## Version 2 Frames

Version 2 retains the 56-byte common frame layout and the ADR 0013 checksum
algorithm:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 4 | frame magic, ASCII `NTSQ` |
| 4 | 2 | frame kind |
| 6 | 2 | frame version, exactly 2 |
| 8 | 4 | flags, exactly zero |
| 12 | 2 | frame length, exactly 56 |
| 14 | 2 | reserved zero bytes |
| 16 | 8 | payload A |
| 24 | 8 | payload B |
| 32 | 8 | payload C or raw chunk bytes |
| 40 | 8 | reserved zero bytes |
| 48 | 8 | checksum of bytes 0 through 47 |

Kinds 1 through 3 retain epoch-allocation, transaction-commit, and
durable-through payloads. Version 2 adds:

- kind 4, page-record header: A is the nonzero shared log position, B is the
  nonzero `PageNumber`, and C is the adapter-assigned `PageVersion`; and
- kind 5, page data: A repeats the parent log position, B is the contiguous
  zero-based chunk index, and bytes 32 through 39 are eight raw image bytes.

One page header is followed immediately by exactly `ceil(N / 8)` data frames.
Unused bytes after the final image byte are zero. Position and index fields bind
each chunk to one logical record and order. A data frame without a pending
header, another kind inside a pending group, a different parent, a skipped or
repeated index, nonzero final padding, or an invalid checksum is corruption.

The checksum remains deterministic corruption detection, not authentication.
Changing its constants or operations requires another format version.

## Shared Ordering and Durability

Completed commit and page records share one strictly contiguous position
allocator and inspectable logical record order. Physical page-data frames do not
consume positions. A durable-through marker can reference any completed logical
record and cannot name a pending or future record.

Page append validates the address lineage, reserves the in-memory snapshot, then
writes the header and every data frame. Only the complete group is added to the
logical record view and returned as a lineage-bound position. Any uncertain
frame write poisons the writer. The before/after append and flush fault meanings
remain identical to the memory model and transaction format.

The existing two-barrier flush sequence covers every physical frame through the
requested logical position before its durable marker can become authoritative.
Transaction recovery searches only commit records while retaining the shared
physical ordering and marker coverage.

## Open and Tail Repair

Open acquires the ADR 0014 lock, synchronizes the file, validates the exact
header, and scans complete fixed-size frames. A pending page state retains its
header offset, expected parent position, next chunk index, and partially copied
const image.

After validating the complete prefix, open may repair only:

- a final physical byte tail shorter than 56 bytes; or
- a final logical page group whose valid header has fewer than the required
  complete data frames.

The repair truncates to the earlier of the incomplete physical frame or pending
page-header offset, synchronizes the truncation, and seeks to the validated end.
A complete unexpected or malformed frame fails without truncation. Allocator
high-water state advances only for complete logical records.

## Allocation, Lock, and Trust Boundaries

Logical record snapshots reserve `Vec` capacity fallibly before mutation. Page
images remain exact `[u8; N]` values with no dynamic-size fallback or unchecked
conversion. Fixed physical frames are built and validated without unsafe code.

The advisory lock, trusted-path, and non-cooperating-writer boundaries remain
ADR 0014 responsibilities. Checksums are not a malicious-writer defense.
Successful standard-library synchronization is the adapter's durability claim;
unsupported filesystem behavior remains an explicit I/O error.

## Evidence Boundary

Version 2 is an ntsql-owned format. Its page width, frame kinds, positions,
chunks, checksums, barriers, error text, and repair behavior make no SQL Server
page, LSN, log-record, MDF/NDF/LDF/BAK, diagnostic, or compatibility claim. No
external format evidence or fixture enters this decision.

## Test Boundaries

- All version 1 golden vectors and recovery tests remain unchanged.
- Version 2 golden tests cover the header, page header, first and final data
  frames, and durable marker.
- Mixed commit/page/commit tests prove one logical order, marker frontier,
  reopen, and transaction lookup that ignores page records.
- Append and flush fault tests prove both returned errors and exact persistent
  effects.
- Width, parent, index, padding, kind, checksum, and ordering corruption fail
  without truncation.
- Every final incomplete physical and logical page tail repairs to the complete
  validated prefix and preserves position high-water rules.
- Same-inode lock tests continue to exclude cooperating writers before
  validation or repair.
- Architecture tests allow exactly the three inward dependencies and reject the
  reverse page-to-file-adapter edge.

## Consequences

The filesystem WAL can now persist a reconstructable transaction/page order and
durable frontier. It still does not write a data page. The next slice must define
a separate page-store file format and barrier, then recovery can compare durable
WAL records with stored page versions. Checkpoints, redo/undo, eviction, and
external compatibility remain later work.
