# ADR 0045: Locked Filesystem Restart Checkpoint Source

- Status: Accepted
- Date: 2026-08-06
- Issue: #142
- Extends: ADR 0014, ADR 0033, ADR 0040, ADR 0042, ADR 0044
- Extended by: ADR 0046, ADR 0064, ADR 0065

## Context

ADR 0044 defines one complete versioned restart checkpoint baseline blob, but
there is no filesystem object that can own those bytes or implement ADR 0040's
optional source port.

Locking the selected blob itself would create an unsafe future composition.
ADR 0042 requires successful publication to replace one current slot
atomically. If that replacement changes the selected path's inode while the
adapter retains a lock on the old inode, another cooperating process could open
and lock the new inode. The original lifetime lock would no longer exclude the
physical object selected by the path.

An empty optional slot also needs an identity. Without immutable lineage outside
the optional blob, supplying the empty slot directory for another database would
look exactly like a valid absence. A later publisher would have no stable
database-bound object against which to reject the wrong authoritative baseline
before physical effect.

This decision adds a lineaged, stable-lock checkpoint slot and read source. It
does not add the publisher or choose its replacement protocol.

## Crate and Dependency Boundary

Only `ntsql-storage-file`, its tests, and the owning ADRs change:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No crate, dependency edge, architecture registration, third-party dependency,
domain I/O, WAL bytes, page-store bytes, or ADR 0044 checkpoint blob bytes
change. The filesystem adapter depends inward on the existing source port and
returns its existing owned untrusted observation.

## Dedicated Slot Directory

The caller supplies one trusted checkpoint slot directory path whose parent
already exists. The adapter owns two fixed names inside that namespace:

- `control`: required immutable lineaged control file; and
- `current`: optional complete ADR 0044 checkpoint blob.

No generation, history, fallback, selection, quarantine, or retention meaning
is assigned to any other directory entry. The adapter neither enumerates nor
removes unknown entries.

The directory is separate from the WAL and page-store files. Its path is an
ntsql-internal composition choice and defines no SQL Server path, file group,
database file, checkpoint file, or native format.

## Version 1 Control Header

`control` is exactly one 64-byte header. All multibyte integers are unsigned
big-endian:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | independent magic, ASCII `NTSQCKS1` |
| 8 | 2 | format version, exactly 1 |
| 10 | 2 | header length, exactly 64 |
| 12 | 4 | flags, exactly zero |
| 16 | 16 | nonzero persistent log ID |
| 32 | 24 | reserved zero bytes |
| 56 | 8 | checksum of bytes 0 through 55 |

The checksum reuses only the repository-owned arithmetic from ADRs 0013 and
0044. Control and checkpoint-blob magic remain independent. Changing control
magic, geometry, canonical fields, or checksum arithmetic requires a new
control format version.

The exact header for persistent ID
`0x0102030405060708090a0b0c0d0e0f10` is golden-tested, including checksum
`0xeed74cabff69c4ff`.

The nonzero control identity is configuration evidence for this physical slot.
It is not a WAL position, transaction identity, checkpoint validity proof, or
recovery permit.

## Fail-Closed Creation

`FileRestartCheckpointBaselineSource::create_new` performs:

1. create the complete slot directory exclusively;
2. create `control` exclusively for read/write access;
3. acquire a nonblocking exclusive lock on `control`;
4. write the complete version 1 header;
5. synchronize `control`;
6. open and synchronize the slot directory;
7. open and synchronize the slot directory's parent; and
8. return a locked source whose `current` slot is absent.

The control lock is acquired before header write or synchronization. Another
cooperating adapter therefore cannot observe creation through a successfully
opened source while creation is still in progress.

Every step has an exact typed I/O stage. A failure after directory creation may
leave a directory, empty or partial control file, or synchronized prefix for
explicit caller reconciliation. Creation performs no rollback or best-effort
cleanup and never returns such a state as success.

Creating an already existing directory fails at the first stage. Creation never
opens, adopts, repairs, truncates, or overwrites an existing slot.

## Lock-Before-Parse Open

`FileRestartCheckpointBaselineSource::open` performs:

1. open existing `control` for read/write access;
2. acquire its nonblocking exclusive lock;
3. read metadata and require exact 64-byte length;
4. read and parse the complete control header;
5. open the slot directory; and
6. retain the control file, directory handle, path, and persistent ID.

Lock failure occurs before metadata inspection, header parsing, current-file
access, repair, or any mutation. Tests deliberately corrupt a locked control
file through a noncooperating descriptor and prove a second open reports lock
contention rather than parsing those bytes. After the first adapter drops, the
same bytes fail as a typed format error.

Length, magic, version, encoded length, flags, zero ID, each reserved byte, and
checksum defects fail closed. Open performs no control repair, synchronization,
or current-file load.

## Stable Lifetime Lock

The required advisory lock belongs to the immutable `control` file and remains
held for the complete adapter lifetime. The optional `current` inode is never
the lock target.

Consequently, replacing `current` does not release or relocate the lock. A test
replaces that path while one source lives and proves a second cooperating open
still contends on `control`. On supported Unix test filesystems, a hard-link
alias of `control` also contends and becomes openable only after the first source
drops.

The retained slot-directory handle is ownership for later directory
synchronization; it is not the lock object. This ADR does not yet authorize a
publisher to write, rename, or synchronize a candidate.

As in ADR 0014, the lock is cooperative and advisory. A malicious or
noncooperating actor can ignore it, mutate or unlink entries, replace path
components, or use unsupported filesystem semantics. The composition root must
supply trusted paths, a filesystem with the required standard-library
operations, and a policy that all ntsql writers use the control lock. No secure
path traversal, mandatory access control, distributed lease, process identity,
wait protocol, or stale-lock recovery is claimed.

## Optional Current Read

The source implements
`DurableTransactionRestartCheckpointBaselineSource::load_restart_checkpoint_baseline`.
Each call reads the path afresh; no decoded cache or prior success is reused.

The operation:

1. opens `current` for reading;
2. treats it as absent only when the `current` directory entry is absent and the
   trusted slot directory still exists as a directory;
3. obtains the opened file's exact `u64` length;
4. converts that length to the host `usize` with a typed overflow error;
5. performs one `try_reserve_exact` for the complete byte length;
6. reads exactly that many bytes;
7. rechecks metadata and rejects a changed length; and
8. calls the exact ADR 0044 decoder.

A dangling `current` symlink, missing slot directory, current directory, access
failure, short read, metadata failure, length overflow, capacity exhaustion,
length race, or structural decode defect is not absence. `Ok(None)` is reserved
for one existing locked slot with no current entry.

The length recheck and checksum detect reviewed accidental races or corruption;
they are not hostile-writer exclusion. The control lock is the cooperating
writer protocol.

Successful load returns only
`OwnedDurableTransactionRestartCheckpointBaselineObservation`. It preserves raw
ID, frontier, entry order, positions, counts, and states without normalization.
It remains untrusted and must pass ADR 0039 validation against the current WAL
prefix before becoming an authoritative baseline.

## Fixed Three-Object Open Order

`open_transaction_page_storage_with_checkpoint` is the reviewed filesystem
composition entrypoint. It acquires lifetime locks in one fixed order:

1. transaction-page WAL;
2. page store; and
3. checkpoint `control`.

After opening WAL and page store, it requires their persistent IDs to match
before opening the checkpoint slot. After opening the checkpoint slot, it
requires its control ID to match the storage ID before returning.

The successful
`UnrecoveredFileTransactionPageStorageWithCheckpoint` privately retains the
existing unrecovered WAL/page-store owner and checkpoint source. `into_parts`
separates those already locked values so the existing consuming recovery and
restart-analysis transitions can proceed while the caller retains the
checkpoint source.

Errors distinguish WAL open, page-store open, storage identity mismatch,
checkpoint open, and checkpoint identity mismatch. Any later-stage failure
drops every already opened value before return; tests immediately reopen each
earlier object to prove lock release.

This order is independent of ADR 0042's operation data dependencies:

- validation reads checkpoint and then WAL; and
- publication analyzes WAL and then invokes a checkpoint publisher.

Neither operation acquires a lifetime lock. All three locks are already held in
the fixed open order before either operation begins. No callback or method touch
order changes the lock hierarchy.

The existing two-object opener and low-level adapter constructors remain
available for their reviewed scopes and focused tests.

## Error and Authority Boundary

Create, open, source, and composition errors preserve exact internal stages and
standard-library or decoder causes through `Error::source`. Format and identity
mismatches have no fabricated nested cause. Paths, OS failures, control bytes,
lock state, and decoded corruption remain outside `ClientDiagnostic`.

The source, control ID, loaded observation, and composed opener cannot create or
satisfy:

- transaction lifecycle or coordinator state;
- WAL append, flush, restart-analysis, or lineage authority;
- page-store or committed-page recovery write authority;
- a recovered or restart-analyzed storage owner;
- an authoritative checkpoint baseline or publication permit;
- checkpoint publication, success receipt, or indeterminate resolution;
- dirty-page tables, replay starts, redo, undo, rollback, or compensation; or
- retention floors, truncation, compaction, or reclamation.

Compile-fail tests reject using the source as authoritative encoder input or as
the sibling publisher. Existing domain compile-fail coverage independently
rejects using loaded observations as transaction, WAL, page, recovery, or
storage-owner authority.

## Evidence and Compatibility Boundary

The control format, paths, ordering, source behavior, errors, and tests are
repository-authored. No external product documentation, driver, SDK, fixture,
oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format is
consulted.

This decision defines no SQL Server checkpoint file, control file, lock,
database-open order, startup selection, recovery phase, error, diagnostic, or
compatibility result.

## Test Boundaries

- New create returns an empty source and exact golden control bytes.
- Duplicate create fails without adopting the existing slot.
- Open reconstructs the exact persistent ID.
- A second open and a hard-link control alias contend until the first source
  drops.
- Lock contention precedes malformed-control parsing.
- Every control field has exact unit-level corruption coverage.
- A repository-encoded nonempty current blob loads every field, remains
  untrusted, and passes only through real-owner current-prefix validation.
- Truncated current bytes fail through the exact ADR 0044 decoder cause.
- Current-entry I/O, dangling link, removed slot, synthetic capacity, and absent
  current outcomes remain distinct.
- Replacing `current` does not move or release the control lock.
- Fixed-order composition distinguishes each stage, releases all acquired
  prefixes after error, retains all three locks on success, and permits the
  existing recovery/restart transition only after explicit separation.
- WAL/page-store and storage/checkpoint identity mismatches fail before
  ownership is returned.
- Existing WAL, page-store, recovery, restart, checkpoint, architecture, and
  governance tests remain valid.

## Non-Goals

This ADR does not:

- implement `DurableTransactionRestartCheckpointBaselinePublisher`;
- create, write, synchronize, rename, replace, repair, or remove `current`;
- choose candidate naming, replacement atomicity, or post-rename barriers;
- classify physical state after an indeterminate publication error;
- add generations, fallback, history, selection, quarantine, or retention;
- make checkpoint presence or validity a startup gate;
- add a dirty-page table, replay start, redo, undo, rollback, compensation, or
  coordinator restoration;
- truncate, compact, or reclaim WAL;
- define a complete database-directory lifecycle or database-wide lock; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql now has one database-bound filesystem checkpoint namespace, a stable
lifetime lock that survives future selected-blob replacement, an exact optional
read source, and one global WAL/page-store/checkpoint open order.

The next filesystem slice can review a single-candidate write, file
synchronization, replacement, directory synchronization, fault effects, and
outcome-indeterminate reopening against this stable slot. Checkpoint-based
startup or WAL reclamation remains blocked on independently reviewed
dirty-page/replay-start completeness and recovery authority.
