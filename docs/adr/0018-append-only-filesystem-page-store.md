# ADR 0018: Append-Only Filesystem Page Store

- Status: Accepted
- Date: 2026-08-06
- Issue: #86
- Extends: ADR 0001, ADR 0012, ADR 0014, ADR 0015, ADR 0016, ADR 0017
- Extended by: ADR 0019

## Context

ADR 0017 persists full page images in the shared transaction/page WAL but does
not write a data page. ADR 0015 already requires the exact WAL position for a
dirty image to become durable before a page-store attempt can begin. The first
filesystem store must honor that permit without hiding write-ahead ordering in
the adapter or defining checkpoint and recovery policy prematurely.

An in-place page layout would require allocation, torn-write, replacement,
free-space, and page-read decisions that do not yet have owning components.
An append-only snapshot file establishes the durable write boundary with a
smaller failure surface: a complete synchronized group is one durable snapshot,
while an incomplete final group is removable without changing an earlier
snapshot.

## Crate and Port Boundary

`FilePageStore<const N: usize>` remains in `ntsql-storage-file`, whose complete
direct dependency set stays:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

The adapter implements the existing `PageStore<N>` port. `ntsql-page` continues
to own lineage validation before composition, WAL-before-store call ordering,
the unforgeable one-attempt permit, and terminal store-error state. The file
adapter owns only bytes, file synchronization, reopening, corruption detection,
tail repair, and inspectable current snapshots. Reconstructed snapshots retain a
full lineage-bound `LogSequenceNumber`, not an interchangeable raw integer.

## Header Format

The page store is a separate file from WAL. Every integer is unsigned and
big-endian. Its version 1 header is exactly 64 bytes:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | page-store magic, ASCII `NTSQPGS1` |
| 8 | 2 | format version, exactly 1 |
| 10 | 2 | header length, exactly 64 |
| 12 | 4 | flags, exactly zero |
| 16 | 16 | nonzero WAL `PersistentLogId` |
| 32 | 8 | nonzero exact page-image width `N` |
| 40 | 16 | reserved zero bytes |
| 56 | 8 | checksum of bytes 0 through 55 |

Creation converts `N` to `u64` and rejects zero or an unrepresentable value
before creating a path. Open requires the caller's exact const width and rejects
a mismatch before frame scanning or tail repair. The persistent ID reconstructs
the same `LogLineage` used by the corresponding WAL; no second page-store
identity or implicit lineage fallback exists.

## Snapshot Frames

Every physical frame is exactly 56 bytes:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 4 | page-store frame magic, ASCII `NTSP` |
| 4 | 2 | frame kind |
| 6 | 2 | frame version, exactly 1 |
| 8 | 4 | flags, exactly zero |
| 12 | 2 | frame length, exactly 56 |
| 14 | 2 | reserved zero bytes |
| 16 | 8 | payload A |
| 24 | 8 | payload B |
| 32 | 8 | payload C or raw chunk bytes |
| 40 | 8 | reserved zero bytes |
| 48 | 8 | checksum of bytes 0 through 47 |

The checksum is the same ntsql-owned corruption-detection algorithm used by ADR
0013 and ADR 0017. Reuse does not merge the formats: the distinct header and
frame magic values prevent a WAL file from opening as a page store.

One logical snapshot group contains, in order:

1. kind 1, snapshot header: A is a nonzero contiguous store sequence, B is a
   nonzero `PageNumber`, and C is the adapter-assigned `PageVersion`;
2. kind 2, required WAL position: A repeats the store sequence, B is the
   nonzero numeric `LogSequenceNumber`, and C is zero; and
3. exactly `ceil(N / 8)` kind 3 page-data frames: A repeats the store sequence,
   B is the contiguous zero-based chunk index, and bytes 32 through 39 contain
   eight raw image bytes.

Unused bytes after the final image byte are zero. The store sequence binds every
frame to one physical group independently of page number, version, or WAL
position. It advances only after a complete group and is never reused after a
successful reopen. A complete group at `u64::MAX` is valid and exhausts the
sequence; another write or physical group then fails explicitly.

## Write and Synchronization Contract

`FilePageStore::write_page` validates, before fault consumption or I/O:

1. the writer is not poisoned;
2. the dirty page belongs to the store lineage;
3. the permit position belongs to the store lineage; and
4. the permit position equals the dirty page's exact required position; and
5. the required WAL position is nonzero.

The adapter constructs the const-sized snapshot and fallibly reserves any needed
inspectable snapshot capacity before file mutation. It then writes the snapshot
header, required-position frame, and all data frames and calls `sync_all`. Only
after synchronization succeeds does it replace the current snapshot for that
page number, advance the sequence, and return success. No allocation that can
fail remains after the file write begins.

`Ok(())` means the complete group is durable, never merely queued. An uncertain
frame write or synchronization error poisons the live writer because its final
physical effect is unknown. Reopen is required before another mutation. The
deterministic `BeforeWrite` fault occurs before file or state mutation;
`AfterWrite` occurs after synchronization and state advancement. Both errors
remain terminal at the ADR 0015 domain boundary.

A rewrite appends a new complete group. Reopen applies complete groups in store
sequence order and exposes only the latest snapshot for each page number. This
defines no monotonic page-version policy and performs no in-place overwrite,
garbage collection, or compaction.

## Open and Tail Repair

Open acquires the advisory exclusive lock, synchronizes the file, validates the
exact header, and scans every complete fixed-size frame. A pending group retains
its snapshot-header offset, expected sequence, required position, next chunk
index, and partially copied const image.

After the complete prefix validates, open may repair only:

- a final physical byte tail shorter than 56 bytes; or
- a final logical snapshot group missing its required-position frame or one or
  more data frames.

If a logical group is pending, repair truncates to its snapshot-header offset;
otherwise it truncates only the incomplete physical frame. The truncation is
synchronized before append resumes. A complete unexpected kind, repeated or
skipped sequence, parent mismatch, invalid index, nonzero reserved payload or
padding, invalid checksum, or zero required identity fails without truncation.

## Lock, Allocation, and Trust Boundaries

Creation and open use the ADR 0014 nonblocking standard-library exclusive lock
and retain it for the file lifetime. A same-inode hard-link alias cannot create a
second cooperating writer on supported platforms. The lock remains advisory;
trusted path setup, non-cooperating writers, filesystem guarantees, and
multi-file database ownership remain composition responsibilities.

Current snapshots use fallible `Vec` reservation before physical mutation and
reopen returns explicit capacity exhaustion instead of infallible insertion.
Images remain exact `[u8; N]` values. Fixed frames use checked conversions and no
unsafe code. Successful standard-library synchronization is the durability
claim; an unsupported or failed operation remains a typed I/O error.

## Recovery and Evidence Boundary

The page store retains the required WAL position with each snapshot so a later
recovery component can compare durable page state with WAL records. This ADR
does not perform that comparison, resolve an indeterminate write, select a redo
start point, or authorize retry.

The format is repository-authored. Its page numbers, versions, widths, store
sequences, WAL positions, chunks, checksums, barriers, and errors define no SQL
Server page or LSN value, page checksum, allocation unit, MDF/NDF/LDF/BAK
representation, crash result, diagnostic, or compatibility claim. No external
format evidence or fixture enters this decision.

## Test Boundaries

- Golden tests cover the exact header, snapshot header, required-position frame,
  first and final data frames, and checksums.
- End-to-end tests execute `stage_page_write -> flush_dirty_page -> file store`,
  reopen WAL and store, and retain the exact page bytes and required position.
- Reopen and rewrite tests prove latest-snapshot selection and store-sequence
  high-water continuation without in-place mutation.
- Exhaustion tests prove `u64::MAX` can complete once and can never wrap or be
  reused by a later write or physical group.
- Before/after write faults prove exact returned errors and persistent effects.
- Width and lineage validation fail before fault consumption or tail mutation.
- Every incomplete physical and logical final boundary repairs to its snapshot
  header while preserving the validated prefix.
- Kind, sequence, parent, index, padding, reserved-field, position, and checksum
  corruption fail without truncation.
- Same-path and hard-link tests prove lock acquisition precedes validation and
  repair and that drop releases the lock.
- Existing WAL v1/v2 byte, recovery, repair, and fault tests remain unchanged.

## Consequences

The filesystem adapter can now complete the domain's durable
WAL-before-page-store path and reconstruct current full-image snapshots after
reopen. The append-only file grows without bound and is not yet a queryable page
source. A later slice must define recovery comparison between WAL and page-store
snapshots before checkpoints, redo/undo, compaction, allocation, buffering, or
eviction can safely enter.
