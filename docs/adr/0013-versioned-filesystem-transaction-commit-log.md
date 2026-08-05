# ADR 0013: Versioned Filesystem Transaction Commit Log

- Status: Accepted
- Date: 2026-08-05
- Issue: #76
- Extends: ADR 0001, ADR 0005, ADR 0008, ADR 0009, ADR 0010, ADR 0011,
  ADR 0012

## Context

The transaction and WAL domains already require an exact append/flush fence,
lineage-local coordinator epochs, lineage-bound positions, and authoritative
commit-record lookup. The memory adapter makes ambiguous physical effects
deterministic, but no state survives an operating-system process or reconstructs
those capabilities from bytes.

The first filesystem boundary must remain ntsql-owned. Native SQL Server
MDF/NDF/LDF/BAK formats, SQL Server LSNs, page records, and undocumented recovery
behavior are outside this decision.

## Crate and Dependency Boundary

Add `ntsql-storage-file` as a standard-library-only outer adapter with exactly:

```text
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

It implements `CommitLog<TransactionCommitRecord>`,
`TransactionEpochSource`, and `TransactionRecoverySource`. Both domain crates
remain I/O-free and cannot depend on this adapter. The architecture checker
registers the complete direct edge set and rejects reverse or extra edges.

## Version 1 Byte Format

Every integer is unsigned and big-endian. Every byte described as reserved must
be zero. There is no native alignment, padding, pointer width, or host-endian
field.

The immutable header is exactly 64 bytes:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | format magic, ASCII `NTSQLOG1` |
| 8 | 2 | format version, exactly 1 |
| 10 | 2 | header length, exactly 64 |
| 12 | 4 | flags, exactly zero |
| 16 | 16 | nonzero `PersistentLogId` |
| 32 | 24 | reserved zero bytes |
| 56 | 8 | checksum of bytes 0 through 55 |

Every following frame is exactly 56 bytes:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 4 | frame magic, ASCII `NTSQ` |
| 4 | 2 | frame kind |
| 6 | 2 | frame version, exactly 1 |
| 8 | 4 | flags, exactly zero |
| 12 | 2 | frame length, exactly 56 |
| 14 | 2 | reserved zero bytes |
| 16 | 8 | payload A |
| 24 | 8 | payload B |
| 32 | 8 | payload C |
| 40 | 8 | reserved zero bytes |
| 48 | 8 | checksum of bytes 0 through 47 |

Version 1 owns three frame kinds:

- epoch allocation: A is one nonzero coordinator epoch; B and C are zero;
- transaction commit: A is one nonzero log position, B is its nonzero
  coordinator epoch, and C is its nonzero coordinator sequence; and
- durable-through: A is one previously appended commit position; B and C are
  zero.

Epochs and commit positions begin at one and are strictly contiguous. A
durable-through marker must strictly advance the prior marker and cannot name a
missing or future record. Repeated complete transaction identities are
corruption.

The repository-owned version 1 checksum is deterministic corruption detection,
not authentication or collision resistance. It starts from
`0x4e5453514c434b31`. For each protected byte in order it:

1. XORs the state with the byte widened to `u64`;
2. multiplies with wrapping arithmetic by `0x4e5453514c57414d`; and
3. rotates left by seven bits, then XORs `0x434845434b53554d`.

The result is XORed with the protected byte count accumulated as a wrapping
`u64`. Header and frame checksums protect bytes 0 through 55 and 0 through 47,
respectively. These exact constants and operations are part of format version 1
and are covered by byte-level tests. Changing them requires another version.

## Creation and Exclusive Ownership

Creation requires an existing parent directory and caller-supplied nonzero
`PersistentLogId`. It uses create-new semantics, writes the complete header,
synchronizes the file, then opens and synchronizes the parent directory before
returning success. It never replaces a path, creates a parent, or generates an
identity.

A failure may leave a path requiring explicit open/reconciliation; it is never
reported as successful creation. Parent-directory synchronization uses
standard-library filesystem behavior. An unsupported platform or filesystem
returns the original typed I/O error.

The caller must establish a trusted path and exclusive single-writer ownership
for the adapter lifetime. This version adds no cross-process lock and cannot
prove that another process is not writing the file.

## Open, Validation, and Tail Repair

Open first synchronizes the existing file, then reads the header and every
complete frame. It validates all magic, version, length, flags, reserved bytes,
checksums, nonzero fields, ordering, identities, and marker references before
exposing any domain port.

Only a final byte count shorter than one complete frame is repairable. After
validating the complete prefix, open truncates that incomplete tail,
synchronizes the truncation, and seeks to the validated end. A complete malformed
frame, malformed header, duplicate identity, ordering violation, or marker
violation returns a typed format error without truncation. Capacity exhaustion
also fails before the adapter is exposed.

Complete unmarked commit frames remain physical but indeterminate. Open does not
promote them merely because its initial synchronization made their bytes stable.
Only durable-through markers establish the recovered durable frontier.

## Epoch and Commit Durability

Epoch allocation appends its frame and synchronizes the file before returning
the epoch and reconstructed lineage. A failed or uncertain epoch write never
constructs a coordinator. A complete epoch that is found on reopen remains
consumed even when its original call reported an error.

Commit append writes one complete frame and assigns its lineage-bound position
without claiming durability. For a new `flush_through` request:

1. synchronize the file containing the complete commit prefix;
2. append a durable-through marker for the requested position;
3. synchronize the marker; and
4. only then advance the in-memory durable frontier and return success.

The first barrier prevents a durable marker from intentionally preceding the
commit bytes it covers. The second makes the recovered frontier durable.
Idempotent flush of an already marked position performs no I/O.

An uncertain frame write or epoch/marker barrier poisons the current writer so
it cannot continue from an incomplete in-memory view. Reopen is required. A
commit-prefix barrier failure before marker append may be retried because it has
not changed the structural stream.

## Fault and Recovery Model

The adapter supports one transient, one-shot fault at a time at the same four
semantic boundaries as the memory model:

- before append: no commit frame;
- after append: one complete unmarked frame;
- before flush: no marker or durable-frontier advancement; and
- after flush: both barriers and frontier advancement complete before the
  injected error.

Invalid, foreign, unknown, poisoned, and idempotent operations do not consume an
unreached fault. Arming a second fault never silently replaces the first.

Authoritative lookup searches the complete transaction identity:

- exactly one marker-covered record returns its reconstructed position;
- no physical record returns absence;
- one complete unmarked record returns an indeterminate-evidence error; and
- duplicate evidence returns an error.

Corruption, partial validation, poisoned state, and I/O failure never return
authoritative absence.

## Compatibility Boundary

Header values, frame kinds, checksums, positions, epochs, durable markers, error
text, and repair behavior are ntsql-internal. They define no SQL Server file,
LSN, transaction ID, commit acknowledgement timing, diagnostic, or compatibility
status. No native-format evidence or external fixture enters this change.

## Test Boundaries

- Create/open reconstruct lineage, durable records, next epoch, and next
  position; an existing path is never replaced.
- Pre-open coordinators and positions work only with a reopened file carrying the
  same stable ID.
- All four injected faults preserve their exact physical effects and recovery
  outcome.
- Later marked commits can cover earlier complete unmarked commits.
- Incomplete tails truncate only after complete-prefix validation.
- Complete checksum, version, structure, ordering, duplicate, and marker
  corruption fail closed without truncation.
- Foreign and unknown positions fail before I/O or fault consumption; marked
  positions are idempotent.
- Architecture tests enforce the exact inward dependency set.
- Tests create temporary files dynamically and commit no format fixture.

## Consequences

ntsql now owns a minimal persistent transaction commit-log stream with
reconstructable identity, allocator epochs, positions, barriers, and recovery
evidence. It is not yet a page WAL or database storage engine. File locking,
multi-file database creation, page/record formats, checkpoints, analysis,
redo/undo, group commit, torn-page handling, backup/restore, and client-visible
transaction behavior remain later Issue #9 work.
