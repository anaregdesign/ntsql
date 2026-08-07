# ADR 0052: Atomic Filesystem Restart Checkpoint Completeness Publication

- Status: Accepted
- Date: 2026-08-06
- Issue: #157
- Extends: ADR 0046, ADR 0049, ADR 0051
- Extended by: ADR 0053, ADR 0060
- Follows: #155

## Context

ADR 0051 defines completeness-specific source and publisher ports and proves
them with a deterministic in-memory adapter. ADR 0049 defines independent
`NTSQCMP1` bytes for the same baseline. No persistent adapter binds those
contracts together.

The ADR 0045/0046 filesystem checkpoint slot cannot be reused for this. Its
selected `current` value is an ADR 0044 `NTSQCKP1` blob, so sharing that slot
would either make one format overwrite the other or make an empty slot
type-ambiguous: an absent `current` carries no format evidence at all, and the
only stable database-bound object in the slot is its `control` file.

This decision adds a second, separately locked completeness slot with its own
control namespace, an exact untrusted read path, and an ADR 0046-equivalent
atomic publication sequence inside that separate namespace. It remains
persistence-only: startup selection, replay execution, repair, retention, and
reclamation stay separate.

## Crate and Dependency Boundary

Only `ntsql-storage-file`, its tests, and the owning ADRs change:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No crate, dependency edge, architecture registration, third-party dependency,
domain I/O, WAL bytes, page-store bytes, `NTSQCKP1` bytes, `NTSQCMP1` bytes, or
`NTSQCKS1` control bytes change. The new adapter depends inward on the existing
ADR 0051 ports and returns their existing owned untrusted observation.

## Separate Completeness Slot

`FileRestartCheckpointCompletenessBaselineSource` owns one caller-supplied
completeness slot directory whose parent already exists. It is a distinct type
from `FileRestartCheckpointBaselineSource` and implements both:

- `DurableTransactionRestartCheckpointCompletenessBaselineSource`; and
- `DurableTransactionRestartCheckpointCompletenessBaselinePublisher`.

The adapter owns three fixed names inside its own namespace:

- `control`: required immutable lineaged control file;
- `current`: optional complete ADR 0049 completeness blob; and
- `candidate`: fixed unselected publication temporary.

The completeness slot is never the transaction-only slot. This adapter never
opens, reads, replaces, empties, or removes an ADR 0045 slot entry, and the
transaction-only adapter never reads a completeness entry. No generation,
history, fallback, selection, quarantine, or retention meaning is assigned to
any other directory entry, and unknown entries are neither enumerated nor
removed.

The two slot paths are an ntsql-internal composition choice. They define no SQL
Server path, file group, database file, checkpoint file, or native format.

## Independent Version 1 Control Header

`control` is exactly one 64-byte header. All multibyte integers are unsigned
big-endian:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | independent completeness control magic, ASCII `NTSQCMS1` |
| 8 | 2 | format version, exactly 1 |
| 10 | 2 | header length, exactly 64 |
| 12 | 4 | flags, exactly zero |
| 16 | 16 | nonzero persistent log ID |
| 32 | 24 | reserved zero bytes |
| 56 | 8 | checksum of bytes 0 through 55 |

Only the magic differs from ADR 0045's control header. Version, geometry,
flags, nonzero-identity, reserved-byte, and checksum discipline are the exact
reviewed rules, and the checksum reuses only the repository-owned arithmetic
from ADRs 0013, 0044, and 0045.

The exact header for persistent ID
`0x0102030405060708090a0b0c0d0e0f10` is golden-tested, including checksum
`0xba49eecff9b84d5a`. The ADR 0045 golden header and its `NTSQCKS1` magic are
unchanged.

Because the magic is compared before every other control field, opening a
completeness slot as a transaction-only slot — or the reverse — fails at
`HeaderMagic` with the exact foreign eight bytes. That remains true when the
optional `current` entry is absent, which is what makes an empty slot
unambiguous. Changing either control magic, geometry, canonical fields, or
checksum arithmetic requires a new control format version.

The nonzero control identity is configuration evidence for this physical slot.
It is not a WAL position, transaction identity, completeness validity proof,
replay authority, or recovery permit.

## Fail-Closed Creation and Lock-Before-Parse Open

`create_new` and `open` reuse the exact ADR 0045/0046 mechanics through shared
private functions parameterized only by control magic. No public
transaction-only type, error, stage, message, lock semantic, or test changes.

`create_new` performs:

1. create the complete slot directory exclusively;
2. create `control` exclusively for read/write access;
3. acquire a nonblocking exclusive lock on `control`;
4. write the complete version 1 header;
5. synchronize `control`;
6. open and synchronize the slot directory;
7. open and synchronize the slot directory's parent; and
8. return a locked source whose `current` slot is absent.

`open` performs:

1. open existing `control` for read/write access;
2. acquire its nonblocking exclusive lock;
3. read metadata and require exact 64-byte length;
4. read and parse the complete control header, magic first;
5. open the slot directory; and
6. retain the control file, directory handle, path, and persistent ID for the
   adapter lifetime.

Lock failure precedes metadata inspection, header parsing, current access,
repair, and mutation. Every step keeps an exact
`FileRestartCheckpointSlotIoStage`. Creation performs no rollback or
best-effort cleanup; a failure after directory creation may leave a directory,
empty or partial control file, or synchronized prefix for explicit caller
reconciliation, and is never reported as success. Creating an already existing
directory fails at the first stage without adopting, repairing, truncating, or
overwriting an existing slot.

The lifetime lock belongs to the immutable `control` file. `current` and
`candidate` are never lock targets, so replacing the selected entry cannot move
cooperating exclusion to an obsolete inode. As in ADRs 0014, 0045, and 0046 the
lock is cooperative and advisory; trusted paths, supported filesystem
semantics, and universal ntsql cooperation remain composition-root obligations.

## Exact Optional Completeness Read

`load_restart_checkpoint_completeness_baseline` reads the fixed `current` path
afresh on every call; no decoded cache or prior success is reused. It:

1. opens `current` for reading;
2. treats it as absent only when the `current` directory entry is absent and
   the trusted slot directory still exists as a directory;
3. obtains the opened file's exact `u64` length;
4. converts that length to the host `usize` with a typed overflow error;
5. performs one `try_reserve_exact` for the complete byte length;
6. reads exactly that many bytes;
7. rechecks metadata and rejects a changed length; and
8. calls only `decode_restart_checkpoint_completeness_baseline`.

A dangling `current` symlink, missing slot directory, current directory, access
failure, short read, metadata failure, length overflow, capacity exhaustion,
length race, foreign `NTSQCKP1` bytes, or structural decode defect is not
absence. `Ok(None)` is reserved for one existing locked completeness slot with
no current entry. The unselected `candidate` is never read.

`FileRestartCheckpointCompletenessBaselineSourceError` distinguishes `Io`,
`CurrentLengthOutOfRange`, `CurrentCapacityExhausted`, `CurrentLengthChanged`,
and `Decode`, preserving the exact I/O stage or ADR 0049 decoder cause through
`Error::source` and fabricating no nested cause for the numeric variants.

Successful load returns only
`OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation`. It
preserves every raw transaction, page, required-image, stored-position, and
replay field without normalization, remains untrusted, and acquires authority
only through ADR 0050 validation against the current WAL prefix and current
page store.

## Permit and Slot Checks Before Effect

Publication first compares the supplied authoritative baseline with its
invariant ADR 0051 owner permit:

- persistent log ID;
- optional numeric durable frontier;
- transaction-entry count; and
- page-entry count.

Any disagreement returns
`FileRestartCheckpointCompletenessBaselinePublicationError::PublicationPermitMismatch`
with both complete quadruples. The publisher then compares the baseline
persistent ID with the immutable slot control ID; a mismatch returns
`SlotPersistentLogIdMismatch`.

Both checks occur before encoding, fault consumption, candidate cleanup, path
creation, write, synchronization, or replacement. A wrong database slot
therefore cannot destroy a stale candidate or selected value, and an armed
fault remains armed.

The permit is not a filesystem, durability, validity, replay, or retention
proof. It identifies only the owner-authorized call and baseline.

## Authoritative Encoding Before Filesystem Mutation

After the identifier checks the publisher calls only
`encode_restart_checkpoint_completeness_baseline`, which accepts the
private-field authoritative baseline and performs its complete fallible
reservation before returning bytes. Encoding failure is preserved as the exact
nested `Encode` cause and occurs before filesystem mutation or fault
consumption. ADR 0051 still classifies it as outcome-indeterminate because the
abstract publisher was invoked.

## Publication Sequence Inside the Completeness Slot

One attempt performs exactly the ADR 0046 physical sequence, but on the
completeness slot's own entries:

1. remove a stale unselected `candidate`, accepting only `NotFound`;
2. create a fresh `candidate` exclusively for writing;
3. write every ADR 0049 encoded byte;
4. synchronize the candidate file with `sync_all`;
5. close the candidate file handle;
6. rename `candidate` to `current` within the same slot directory;
7. synchronize the retained slot-directory handle with `sync_all`; and
8. report `Ok(())`.

Unlink-before-create removes a stale regular file, symlink, or hard-link
directory entry without opening or following it; a directory or other entry
that cannot be unlinked as a file fails explicitly. The publisher never
truncates or follows an existing candidate. This cleanup applies only to the
explicitly unselected `candidate`.

The publisher calls `rename(candidate, current)` directly, never removes
`current` first, and never falls back to remove-then-rename. If the platform or
filesystem cannot replace the existing entry, publication returns the exact
`ReplaceCurrentFile` I/O error and no false success. Success is withheld until
slot-directory synchronization succeeds. The control file and its lock are not
renamed.

Each standard-library failure retains the exact existing
`FileRestartCheckpointSlotIoStage`: `RemoveCandidateFile`,
`CreateCandidateFile`, `WriteCandidateFile`, `SyncCandidateFile`,
`ReplaceCurrentFile`, or `SyncPublishedSlotDirectory`.

## Deterministic Fault Boundaries

The adapter supports one armed completeness publication fault, independent of
the transaction-only adapter's plan:

| Fault | Physical state when returned |
| --- | --- |
| `BeforeCandidateCleanup` | prior candidate and current unchanged |
| `AfterCandidateCleanup` | candidate absent; current unchanged |
| `AfterCandidateCreate` | empty candidate; current unchanged |
| `AfterCandidateWrite` | complete unsynchronized candidate; current unchanged |
| `AfterCandidateSync` | synchronized closed candidate; current unchanged |
| `AfterCurrentReplace` | candidate absent; current names new bytes; directory not synchronized by this attempt |
| `AfterDirectorySync` | candidate absent; current names durably synchronized new bytes |

Arming never replaces an existing plan;
`FileRestartCheckpointCompletenessBaselinePublicationFaultAlreadyArmed` retains
the armed and rejected points. Permit mismatch, slot mismatch, and encode
failure do not consume the plan because its physical boundary was not reached.
A matching fault is one-shot.

The last two fault points deliberately demonstrate that an error cannot mean
the old value remained selected, and that an error cannot mean the new value is
not durable.

## Fresh Owner Operation After Failure

The adapter has no retry method and does not convert an indeterminate token
into a baseline. A caller may later invoke a new
`publish_restart_checkpoint_completeness_baseline_from_current_prefix` owner
operation, which performs fresh current analysis, prepares a new authoritative
baseline, creates a new invariant permit, and invokes the publisher anew. The
new attempt safely unlinks any prior unselected candidate and replaces
whichever `current` entry is then selected. This is a new independently
authorized replacement, not proof about the earlier attempt.

## Fixed Three-Object Open Order

`open_transaction_page_storage_with_completeness_checkpoint` acquires lifetime
locks in one fixed order:

1. transaction-page WAL;
2. page store; and
3. completeness `control`.

After opening WAL and page store it requires their persistent IDs to match
before opening the completeness slot; after opening that slot it requires its
control ID to match the storage ID before returning ownership.
`FileTransactionPageStorageCompletenessCheckpointOpenError` distinguishes WAL
open, page-store open, storage identity mismatch, completeness open, and
completeness identity mismatch, and any later-stage failure drops every already
opened value before return.

The successful
`UnrecoveredFileTransactionPageStorageWithCompletenessCheckpoint` privately
retains the existing unrecovered WAL/page-store owner and the completeness
source. ADR 0053 removes its former `into_parts` escape: consuming pre-recovery
selection now moves all three values directly into `Selected`, `Absent`, or
`Rejected` ownership and retains them through any explicit full-recovery
fallback.

This order is independent of ADR 0051's operation data dependencies:
validation reads the checkpoint and then the WAL and store, while publication
analyzes the WAL and store and then invokes the publisher. Neither operation
acquires a lifetime lock. The existing two-object opener and the ADR 0045
transaction-only composition remain unchanged and available; this decision does
not define a combined transaction-only plus completeness opener.

## Error and Authority Boundary

Create, open, source, publication, and composition errors preserve exact
internal stages and standard-library, encoder, or decoder causes through
`Error::source`. Format, permit, slot, and injected-fault variants fabricate no
nested cause. Paths, OS failures, control bytes, lock state, physical stage,
and decoded corruption remain outside `ClientDiagnostic`.

Every invoked publisher error remains ADR 0051 outcome-indeterminate through
the final-owner operation, which exposes only the four identifying values. The
adapter stage is diagnostic evidence, not retry or non-effect authority.

The completeness source, control ID, loaded observation, published bytes,
receipt, indeterminate token, and composed opener cannot create or satisfy:

- the transaction-only checkpoint source, publisher, permit, or receipt;
- transaction lifecycle or coordinator state;
- WAL append, flush, restart-analysis, or lineage authority;
- page-store or committed-page recovery write authority;
- a recovered or restart-analyzed storage owner;
- an authoritative completeness baseline without ADR 0050 validation;
- checkpoint startup selection, replay execution, redo, undo, rollback, or
  compensation;
- a dirty-page table or replay start; or
- retention floors, truncation, compaction, or reclamation.

Compile-fail tests reject using the completeness source as authoritative
encoder input, publishing without the private completeness permit, and
substituting it for the transaction-only source, the transaction-only
publisher, WAL durability, page-store write, committed-page recovery write,
transaction lifecycle, or restart-analyzed storage ownership.

## Evidence and Compatibility Boundary

The control format, paths, ordering, read behavior, publication sequence, fault
effects, errors, and tests are repository-authored. No external product
documentation, driver, SDK, fixture, oracle, proprietary governance tool, or
native MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server checkpoint file, control file, lock,
database-open order, startup selection, recovery phase, error, diagnostic, or
compatibility result.

## Test Boundaries

- A new completeness slot returns an empty source, exact golden `NTSQCMS1`
  control bytes, and no `current` or `candidate` entry; duplicate creation
  fails without adoption; a second open contends until the first source drops.
- An empty completeness slot and an empty transaction-only slot reject each
  other's opener at the exact foreign control magic and still open with their
  own.
- A repository-encoded `NTSQCMP1` blob written directly to `current` loads
  every transaction, page, and replay field, remains untrusted, and passes only
  through real-owner current-prefix validation; a present `candidate` is
  ignored by the read and by validation.
- Replacing `current` neither moves nor releases the control lock.
- Truncated bytes, complete foreign `NTSQCKP1` bytes, a current directory, a
  dangling current link, a removed slot, and the numeric length/capacity
  variants remain distinct, and only the I/O and decode variants carry a nested
  cause.
- Lock contention precedes malformed-control parsing, and the same bytes fail
  as a typed format error after the first source drops.
- Fixed-order composition distinguishes each stage, releases every acquired
  prefix after error, retains all three locks on success, rejects WAL/store and
  storage/completeness identity mismatches before returning ownership, and
  permits the existing recovery and restart transition only through consuming
  selection and an explicit selected-decline, absence, or rejection fallback.
- Publication replaces a stale candidate, publishes exact empty and exact
  nonempty baselines byte-for-byte equal to the codec, returns receipt
  identifiers including page count, and leaves no candidate.
- A published baseline loads back as an untrusted observation and becomes
  authoritative only through real owner validation; validation still succeeds
  after an unrelated later WAL suffix, and fails closed once a selected page's
  current store snapshot advances beyond the selected frontier.
- Every fault point exposes the exact candidate and current bytes in the table,
  exact indeterminate identifiers, a complete cause chain, and a successful
  fresh owner operation afterwards.
- Wrong-slot publication preserves candidate, current, and its armed fault, and
  arming twice is rejected without changing the plan.
- A candidate directory fails at cleanup without changing current; a current
  directory fails at replacement, remains present, proves no delete fallback,
  and is reconciled by a later fresh operation.
- On supported Unix test filesystems, a stale candidate symlink is unlinked
  without changing its target.
- Existing ADR 0044/0045/0046/0049 source, codec, publication, golden-byte,
  memory-adapter, WAL, page-store, recovery, restart, architecture, and
  governance tests remain valid and unchanged.

## Non-Goals

This ADR does not:

- change `NTSQCMP1`, `NTSQCKP1`, `NTSQCKS1`, WAL, or page-store bytes;
- change the ADR 0051 source or publisher port shapes;
- share one selected slot between transaction-only and completeness formats, or
  compose both checkpoint slots in one opener;
- add generations, history, fallback, selection, quarantine, or retention;
- add automatic retry, backoff, cleanup on drop, or indeterminate resolution;
- guarantee atomic replacement after a returned error;
- make completeness presence or validity a startup gate or failure typestate;
- execute replay, redo, undo, rollback, compensation, page repair, transaction
  restoration, or coordinator reconstruction;
- choose a retention floor, truncate, compact, or reclaim a WAL;
- define database-wide atomicity across WAL, page store, and both slots; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql now has a fail-closed persistent implementation of both ADR 0051
completeness ports. A separately locked, separately lineaged completeness slot
durably selects exact repository-encoded `NTSQCMP1` bytes under its own
`NTSQCMS1` control lock, while the transaction-only slot, its bytes, its lock
semantics, and its tests are untouched.

Loaded completeness bytes remain untrusted and acquire authority only through
ADR 0050 validation against the current WAL prefix and current page store.
ADR 0053 later adds consuming startup selection while retaining this same locked
source. Replay execution, dirty-page repair, indeterminate resolution, and WAL
retention or reclamation remain later independently reviewed boundaries.
