# ADR 0022: Versioned Filesystem Transaction-Page WAL

- Status: Accepted
- Date: 2026-08-06
- Issue: #96
- Extends: ADR 0013, ADR 0014, ADR 0017, ADR 0019, ADR 0020, ADR 0021

## Context

ADR 0013 established the accepted version 1 transaction-only filesystem WAL.
ADR 0017 added an explicit version 2 full-image page WAL while preserving
version 1 bytes and behavior. ADR 0020 then defined the transaction-owned page
typestate and its hard adapter obligation: transaction-page records and commit
records for one lineage must use one position allocator and one durable prefix.
ADR 0021 proved that obligation in the deterministic memory adapter and kept
page ownership separate from durable commit identity.

The filesystem adapter still cannot persist the owner of a page record. A
recovery caller therefore cannot distinguish an uncommitted transaction-owned
image from a raw page image after reopen. Silently extending version 2 is not
acceptable because its strict frame-kind and version checks are already an
accepted corruption boundary.

The smallest persistent step is an explicit ntsql-owned version 3 that adds a
mandatory owner frame to a distinct transaction-page logical group, implements
`TransactionPageLog<N>`, and preserves all version 1 and version 2 entrypoints,
bytes, errors, repair rules, and behavior.

## Crate and Dependency Boundary

No crate or dependency edge changes. `ntsql-storage-file` continues to depend on
exactly:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

The adapter implements the existing `TransactionPageLog<N>` domain port. Domain
crates remain I/O-free and cannot depend on the filesystem adapter.

## Explicit Version Dispatch

Const page width `N` can distinguish version 1 (`N == 0`) from the existing
page-capable version 2 (`N > 0`), but it cannot distinguish version 2 from
version 3 because both have the same nonzero page width. Each `FileCommitLog<N>`
therefore retains its exact runtime `LogFormat`.

Every epoch-allocation, commit, durable-through, raw-page, transaction-page, and
page-data frame is emitted with that retained format. No write path derives a
format from `N` or hard-codes version 2.

Entrypoints are intentionally capability-specific:

- `create_new` and `open` create and require version 1;
- `create_new_page_capable` and `open_page_capable` create and require version
  2; and
- `create_new_transaction_page_capable` and
  `open_transaction_page_capable` create and require version 3.

No boolean flag, default, migration, fallback, or format negotiation exists.
Opening through the wrong entrypoint fails at the header version before frame
scanning, truncation, or tail repair. The ADR 0014 same-inode lock is still
acquired before header validation or repair.

## Version 1 and Version 2 Preservation

Version 1 and version 2 header and frame bytes remain exact. Their accepted
golden checksums, frame kinds, payloads, entrypoints, width validation,
append/flush faults, poisoning, shared record order, durable markers, lock
ordering, recovery lookup, corruption errors, and tail-repair rules do not
change.

Version 1 and version 2 instances reject a transaction-page append with
`TransactionPageSupportUnavailable` before fault consumption, capacity
reservation, position assignment, file I/O, or mutation. The existing raw page
behavior remains unchanged.

## Version 3 Header

Every integer is unsigned and big-endian. Version 3 keeps the 64-byte version 2
header body:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | format magic, ASCII `NTSQLOG1` |
| 8 | 2 | format version, exactly 3 |
| 10 | 2 | header length, exactly 64 |
| 12 | 4 | flags, exactly zero |
| 16 | 16 | nonzero `PersistentLogId` |
| 32 | 8 | nonzero const page-image width `N` |
| 40 | 16 | reserved zero bytes |
| 56 | 8 | checksum of bytes 0 through 55 |

The page-width, reserved-byte, checksum, trusted-path, and synchronization rules
are the same as version 2.

## Version 3 Frames

Version 3 keeps the accepted 56-byte frame envelope and checksum algorithm. All
frames in a version 3 file carry frame version 3. Kinds 1 through 5 retain their
existing meanings:

| Kind | Meaning | Payload A | Payload B | Payload C |
| ---: | --- | --- | --- | --- |
| 1 | epoch allocation | epoch | 0 | 0 |
| 2 | commit | position | transaction epoch | transaction sequence |
| 3 | durable through | position | 0 | 0 |
| 4 | raw page header | position | page number | page version |
| 5 | page data | parent position | chunk index | eight raw bytes |

Version 3 adds:

| Kind | Meaning | Payload A | Payload B | Payload C |
| ---: | --- | --- | --- | --- |
| 6 | transaction-page header | position | page number | page version |
| 7 | transaction-page owner | repeated parent position | transaction epoch | transaction sequence |

One kind 6 frame is followed immediately by exactly one kind 7 frame and then
exactly `ceil(N / 8)` existing kind 5 data frames. Chunk parent, contiguous
zero-based index, final padding, and checksum rules are unchanged.

The distinct kind 6 header is an anti-downgrade boundary. Reusing a kind 4 raw
page header followed by an optional owner would let loss of the owner frame turn
a transaction-owned image into a structurally valid raw page. A kind 6 group
cannot become a valid record without kind 7.

## Logical Position and Shared Frontier

Kind 6 consumes exactly one logical position. Kind 7 and all kind 5 data frames
consume no positions. The logical record and next position become visible only
after the complete group is written and accepted.

Raw pages, transaction-owned pages, and commits use the same retained lineage,
one monotonic logical-position allocator, one physical frame stream, one
durable-through marker sequence, one in-memory logical record order, and one
fault plan. A transaction-owned page at position *p* followed by its commit at
position *c > p* is covered by the commit flush through *c*.

Append validates version capability, poison state, exact page width, and page
lineage before fault consumption or mutation. A before-append fault leaves no
frames, logical record, or position effect. An after-append fault leaves the
complete volatile group and consumes its position. Any uncertain frame write
poisons the writer and requires reopen.

## Owner Identity Boundary

The append path copies the exact domain `TransactionId` epoch and sequence. On
reopen the scanner reconstructs those persisted numeric fields, but it does not
manufacture a domain `TransactionId`: that token intentionally has no public
constructor because safe downstream code must not forge coordinator authority.

The inspectable filesystem transaction-page record therefore exposes the owner
epoch and sequence and can compare them with a caller-supplied `TransactionId`.
It has no public constructor. This mirrors existing filesystem commit records,
which also persist and inspect epoch/sequence fields without reconstructing a
domain lifecycle token.

Ownership and commitment remain strictly separate:

- commit epoch/sequence accessors and `transaction_identity()` remain
  commit-record-only;
- `matches_transaction_id` and durable commit lookup ignore both page kinds;
- owner accessors and `page_owner_matches_transaction_id` inspect only
  transaction-owned page records;
- `transaction_page_write()` returns the typed owned record only; and
- `page_write()` projects both raw and owned records to the same page payload.

A durable owned page without a commit therefore yields authoritative commit
lookup `Absent`. Adding the real commit for the same identity yields exactly one
`Found`, never a duplicate.

## Open Scanner and Owner Validation

The scanner has explicit `AwaitingOwner` and `AwaitingData` phases.

After a valid kind 6 header it requires kind 7 next. The owner frame must:

- repeat the nonzero pending logical position;
- carry a nonzero transaction epoch;
- reference an epoch already allocated earlier in the file; and
- carry a nonzero transaction sequence.

The owner need not have a commit record. An uncommitted durable transaction page
is a required representable state. The scanner does not enforce uniqueness of
`(transaction epoch, transaction sequence, page number)` across owned records.
The live domain currently limits one image per page per transaction; persistent
multi-image visibility and redo ordering are later policy, not format validity.

An owner without a kind 6 header, a complete non-kind-7 frame after kind 6, a
duplicate owner, a parent mismatch, a zero or unallocated owner field, or a
complete non-kind-5 frame after the owner is corruption. Existing data parent,
chunk order, padding, and checksum failures remain corruption. These complete
malformed or interrupted groups fail without truncation.

## Tail Repair

Open may repair only a final incomplete physical frame or final incomplete
logical group after validating the complete prefix.

If the final kind 6 group ends before its owner, after its owner but before all
data frames, or during a partial owner/data physical frame, repair truncates to
the kind 6 offset. This removes the whole unaccepted logical record and leaves
the validated prefix and allocator high-water unchanged. A partial physical
frame outside a pending logical group keeps the existing frame-boundary repair
rule.

The lock is acquired and the header version/page width are validated before any
repair. A wrong-version or width-mismatched open cannot mutate the file.

## Recovery Projection and Authorization

Both raw and transaction-owned pages project through
`page_recovery_observation()` with their exact page number, version, bytes,
lineage, and logical position. The owner is intentionally omitted from
`DurablePageWalObservation`; ADR 0019 remains commit-agnostic and
non-authorizing.

Persisting owner evidence does not itself authorize replay or page-store access.
At runtime, only ADR 0020's `flush_committed_page` with the exact durable
`CommittedTransaction` can cross the existing WAL-before-store gate. Recovery
visibility that combines durable owner and commit evidence is separate future
work.

## Evidence Boundary

Version 3 is a repository-authored ntsql format. Its header, frame kinds,
payloads, positions, owner fields, checksums, errors, and repair behavior use no
external product documentation, driver, SDK, fixture, oracle, or proprietary
format. They make no SQL Server transaction ID, page, LSN, log record,
MDF/NDF/LDF/BAK, recovery, crash, diagnostic, or compatibility claim.

## Test Boundaries

- Existing version 1 and version 2 tests and golden vectors remain unchanged.
- Version 3 golden vectors cover its header, epoch, transaction-page header,
  owner, first/final data chunks, commit, and durable marker.
- An owned page without a commit remains `Absent`; its real commit is the sole
  `Found` record.
- Raw page, owned page, and commit records share exact contiguous positions and
  reopen in the same logical order and durable prefix.
- Version 1 and version 2 reject transaction pages without consuming the armed
  fault or position.
- Before/after append and before/after flush tests prove exact logical and
  physical effects, authoritative resolution, and committed page-store access.
- Real filesystem page-store faults preserve the committed owner and exact
  before/after physical effects.
- Missing, orphaned, duplicate, mismatched, zero, and unallocated owners fail at
  exact offsets without truncation.
- Complete group interruption, invalid chunk padding, and invalid checksums fail
  without truncation.
- Header-only, owner-only, partial-data, partial-owner, and partial-final-data
  tails repair to the kind 6 offset and permit position reuse from the validated
  prefix.
- Cross-version and lock tests prove validation/repair ordering.
- The existing architecture dependency allow-list and reverse-edge tests remain
  unchanged.

## Non-Goals

This ADR does not:

- add recovery authorization or execute redo/undo;
- add rollback, abort records, compensation, checkpoints, transaction tables,
  or dirty-page tables;
- define visibility, isolation, locking, buffering, eviction,
  force-at-commit, or multi-image replay ordering;
- expose a public constructor for `TransactionId` or treat persisted owner
  fields as a domain lifecycle token;
- carry ownership into `DurablePageWalObservation`;
- require an owned page to have a commit record or enforce persistent
  transaction/page uniqueness; or
- make any external SQL Server or native file-format claim.

## Consequences

The filesystem WAL can now persist and reopen transaction-owned full-image
pages over the same durable frontier as their commits while keeping ownership
separate from commit authority. Version 1 and version 2 remain stable, and a
missing owner cannot silently downgrade an owned image into a raw page.

The next recovery slice may define visibility over complete durable owner plus
commit evidence. Mutation-capable redo/undo remains blocked until authorization,
idempotence, uncommitted-page handling, and replay ordering are separately
approved and tested.
