# ADR 0060: Atomic WAL-Prefix Reclamation and Anchored Restart

- Status: Accepted
- Date: 2026-08-07
- Issue: #170
- Extends: ADR 0011, ADR 0013, ADR 0014, ADR 0034, ADR 0035, ADR 0036,
  ADR 0039, ADR 0052, ADR 0053, ADR 0058, ADR 0059
- Follows: #169

## Context

ADR 0059 consumes one completed selected-checkpoint restart and derives an
inclusive WAL-retention floor from the exact selected checkpoint, complete
page-store inventory, current logical WAL, unresolved transaction state,
format-local constraints, and the persisted allocator epoch high-water. That
analysis is deliberately inert and point-in-time. It cannot delete, truncate,
rewrite, replace, or compact a WAL.

Physical reclamation changes the evidence available at the next restart. Once a
prefix has been removed, ADR 0039's complete-prefix source-relative checkpoint
validation is no longer sound: the absent observations were intentionally
summarized by the selected checkpoint and cannot be reconstructed from the
retained suffix. Treating that absence as an ordinary mismatch would reject a
valid reclaimed generation; falling back to an unanchored complete recovery
would be worse because it would interpret a suffix as a complete history.

The effect also crosses a filesystem replacement boundary. A successful-looking
adapter report is insufficient if the selected path still names the old inode,
the replacement contains different records, or a post-rename synchronization
failed. Every result after invocation must therefore be terminal until the
installed source has been observed and compared exactly.

This decision adds:

- one generation-aware startup-selection boundary;
- one deterministic selected-checkpoint anchor;
- one retained-suffix restart source;
- one consuming, branded, single-attempt reclamation authority;
- exact post-effect installed-source validation;
- deterministic memory generations; and
- a versioned filesystem generation and crash-safe replacement protocol.

It does not add online reclamation, a segmented WAL, background compaction,
backup or replication retention, point-in-time restore, or an external database
format.

## Crate and Dependency Boundary

`ntsql-transaction` owns all authority, validation, generation-neutral
observations, anchored replay materialization, success/failure owners, and typed
evidence errors:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-memory -> ntsql-page
ntsql-storage-memory -> ntsql-transaction
ntsql-storage-memory -> ntsql-wal

ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

The adapters implement narrow ports and own their physical state. They cannot
construct a retention analysis, selected baseline, reclamation permit, success
owner, or receipt. No crate or direct dependency edge changes.

Domain crates remain free of filesystems, clocks, networks, serialization
dependencies, global target state, and adapter-specific path or frame concepts.

## Two Explicit WAL Source Modes

`DurableTransactionRestartWalReclamationSourceObservation` reports exact,
untrusted current physical metadata:

- WAL lineage;
- physical format version;
- source generation;
- optional first retained logical record;
- optional logical-position high-water;
- allocated epoch high-water; and
- optional selected-checkpoint anchor.

Generation zero is the unreclaimed, complete-prefix form. It has no selected
checkpoint anchor and continues to use ADR 0039/0050 complete-prefix validation.

Every nonzero generation is pruned and anchored. It must expose:

- a nonzero physical format version;
- one exact selected-checkpoint anchor;
- retained-first and high-water metadata whose optional shapes remain distinct;
- the allocator high-water independently from logical records; and
- the complete retained logical suffix through the reported high-water.

An empty retained suffix is valid when no logical record remains required. Its
retained-first value is `None`, while its logical high-water may still be
`Some(F)`. This preserves the next logical allocation position without inventing
a sentinel record.

Generation-aware selection rejects a pruned generation when its completeness
checkpoint is absent, unreadable, foreign, changed, or structurally invalid. It
never classifies that condition as ordinary checkpoint absence and never exposes
the complete-prefix recovery fallback.

Selection first observes only the physical generation. That minimal observation
does not require an allocated epoch, retained boundary, logical high-water, or
anchor. Generation zero therefore selects an absent or present empty checkpoint
through complete-prefix validation before the allocator has issued its first
epoch. A nonzero observation requires the complete metadata observation above;
the two observed generation values must match or selection rejects the unstable
source without fallback.

The existing standalone complete-prefix source remains unchanged for generation
zero. A nonzero generation is usable only through the retained-suffix port and
the owning anchored-checkpoint path.

## Versioned Selected-Checkpoint Anchor

`DurableTransactionRestartSelectedCheckpointAnchor` is an inert `(version,
value)` pair. Version 1 is a repository-authored deterministic 128-bit fold over
canonical authoritative baseline fields in their already validated order:

- persistent log ID and optional checkpoint frontier;
- transaction count and, for every identity-sorted transaction, epoch, sequence,
  optional first/last owned-page positions, owned-page count, state, and exact
  commit position when committed;
- page count and, for every page-number-sorted entry, page number, state,
  required-image kind and positions/transaction identity, and optional stored
  position; and
- replay-start kind, optional frontier or inclusive position, and exact cause.

Every optional field includes an explicit presence contribution. Collection
indexes and counts are included so concatenation or regrouping cannot alias.
Variant tags distinguish values that otherwise share numeric payloads.

The anchor deliberately does **not** hash page image bytes. The authoritative
completeness baseline does not own those bytes. Image bytes continue to be
validated from the exact retained WAL observations and current page-store
snapshots during planning, repair, completion, retention, and reclamation.
Claiming byte coverage here would create authority that the checkpoint format
does not contain.

The anchor is not a cryptographic signature, legal attestation, corruption
oracle, compatibility digest, or page-integrity checksum. Its only authority
comes from private construction over a freshly validated baseline and later
exact comparison within the owning restart path.

## Anchored Retained-Suffix Materialization

For a nonzero generation, replay planning loads and validates the exact selected
checkpoint against generation metadata rather than re-deriving the missing
prefix.

The domain:

1. validates persistent identity, physical version, generation, anchor shape,
   retained-first shape, logical high-water, and allocator high-water;
2. verifies that the freshly loaded authoritative selected baseline hashes to
   the persisted anchor;
3. observes the complete retained suffix exactly once under the source's
   exclusive mutable borrow;
4. validates strict lineage, increasing physical order, record kinds, retained
   first boundary, and exact high-water;
5. seeds transaction state from the selected checkpoint baseline;
6. validates checkpoint-frontier records that remain in the suffix without
   applying them twice;
7. applies only records after the selected frontier to current transaction
   state; and
8. retains both the complete suffix and the smaller replay window as separate
   owned projections.

The replay window begins at the selected baseline's inclusive replay start.
Records before that window may still be required: for example, a current stored
page may be backed by a retained checkpoint-prefix page record. Completion,
retention, and reclamation therefore re-observe and validate the complete
retained suffix, not only replay records.

Every anchored selected, planned, prepared, repaired, restored, completed,
retention-analyzed, and rejected owner remains anchored. No transition can
discard the anchor and reinterpret the source as a complete prefix.

## Revalidation Before Authority

Only
`WalRetentionAnalyzedTransactionPageStorageRestartCheckpointReplay` exposes the
consuming reclamation transition.

Before invoking an adapter effect, the domain repeats the evidence sequence under
the still-owned WAL, page store, checkpoint source, and restored coordinator:

1. materialize the complete page-store inventory;
2. observe retention metadata;
3. observe the current source generation and physical metadata;
4. re-observe either the complete generation-zero prefix or complete retained
   suffix according to the frozen mode;
5. repeat selected-restart completion validation;
6. re-derive every retention requirement and inclusive floor; and
7. compare the fresh analysis field-for-field with ADR 0059's frozen analysis.

It also requires:

- unchanged lineage and persistent identity;
- unchanged physical format and generation;
- unchanged selected-checkpoint anchor;
- exact source retained-first and high-water values;
- exact allocator high-water;
- no volatile logical records beyond the durable frontier;
- an exact retained boundary when the floor is present;
- no retained record below the authorized floor in the replacement plan; and
- a representable retained-record count and `generation + 1`.

Any stale page, transaction, checkpoint, allocator, source, format, generation,
frontier, or floor evidence rejects before effect invocation. The owner becomes
a terminal failed owner; it does not downgrade to completed live access or full
recovery.

## Branded Single-Attempt Permit

After every fallible domain allocation and comparison succeeds, the domain
creates one invariant-lifetime
`DurableTransactionRestartWalReclamationPermit<'attempt>` inside a private
higher-ranked callback. It binds:

- persistent log ID and exact lineage;
- analyzed durable frontier;
- optional inclusive retained-first logical record;
- allocated epoch high-water;
- source physical format version;
- source generation; and
- selected-checkpoint anchor.

The permit is neither public-constructible nor cloneable. Its lifetime cannot be
widened beyond one adapter invocation. Numeric positions, a retention floor,
source observation, checkpoint observation, or prior receipt cannot substitute
for it.

The adapter chooses only a reviewed replacement physical format supported by its
implementation. It must preserve exact logical record positions, retained
record kinds and payloads, logical high-water, allocator high-water, persistent
identity, and anchor. The returned effect observation is untrusted.

## Terminal Effect Boundary

Calling `reclaim_restart_wal_prefix` is the physical attempt boundary. Every
adapter error returned from or after that invocation is
`OutcomeIndeterminate`, including an error that the adapter describes as
occurring before its first durable write. The domain does not infer physical
side-effect timing from adapter error variants.

A reported effect must match the permit and preflight plan exactly:

- old generation equals the observed source generation;
- new generation is exactly old generation plus one;
- replacement format is nonzero and allowed by the adapter;
- retained-first and logical high-water preserve optional shape and value;
- retained count equals the exact prepared suffix;
- allocator high-water is unchanged; and
- selected checkpoint anchor remains exact.

A malformed success report is outcome-indeterminate, not a definite preflight
rejection.

The failed owner retains the entire retention-analyzed owner, adapters,
checkpoint source, frozen analysis, and exact cause. It has no retry, fallback,
publication, live mutation, adapter release, or success-shaped conversion.
Recovery requires dropping it and reopening the complete composition.

## Installed-Source Verification

An effect receipt alone does not prove which bytes or in-memory generation were
installed. Before returning success, the domain re-observes the adapter:

1. observe installed physical metadata;
2. require generation `old + 1`, replacement format, anchor, retained-first,
   logical high-water, and allocator high-water to match exactly;
3. observe the complete installed retained suffix;
4. compare record count, kind, position, owner identity, page version, and every
   payload byte with the prepared replacement observations; and
5. observe physical metadata again and require it to remain unchanged across the
   retained-suffix callback.

Any source error, missing or extra record, reordered record, changed position,
changed owner, changed page payload, unstable metadata, or success-shaped
unapplied effect is outcome-indeterminate.

Only this complete verification constructs
`DurableTransactionRestartWalReclamationReceipt` and the successful reclaimed
owner. The receipt is immutable inspection evidence, not authority for another
reclamation.

## Deterministic Memory Generations

`InMemoryCommitLog` stores generation metadata separately from its logical record
vector:

- physical format version and generation;
- optional retained-first and logical high-water;
- allocated epoch high-water; and
- optional selected-checkpoint anchor.

Generation zero projects its existing complete durable prefix. Reclamation first
fallibly prepares the exact retained durable records without mutating the source,
then swaps the prepared records and metadata in one exclusive effect step.

Logical positions are never renumbered. `next_position` advances from the
preserved logical high-water, not the retained vector length. `next_epoch`
advances from the preserved allocator high-water even when no logical record is
retained.

A deterministic after-effect fault may report an error after the swap. The
domain treats both before- and after-effect adapter errors as terminal. A fresh
memory reopen receives a new runtime lineage capability while preserving the
same persistent identity, generation metadata, exact retained positions, and
allocator continuation.

## Filesystem V4 Generation

Legacy V3 transaction-page WAL creation and open remain generation zero. The
first filesystem reclamation writes V4; later reclamation may replace V4 with
V4.

V4 has a checksummed, explicitly longer header containing:

- format version and exact header geometry;
- persistent log ID and exact page width;
- nonzero generation;
- explicit presence and value for retained-first;
- explicit presence and value for logical high-water;
- nonzero allocator epoch high-water;
- anchor version and value; and
- canonical zero reserved bytes.

All multibyte values remain big-endian and host-word-size independent. Retained
logical record groups reuse the reviewed V3 frame encoding and preserve their
exact numeric positions and payloads. V4 rematerializes the allocator high-water
and the replacement-time durable logical high-water from authenticated header
metadata rather than copying removed epoch-allocation or obsolete marker history
as logical records.

The retained-first and logical-high-water header fields describe the installed
generation at replacement time. They are not rewritten by later appends. When
that installed suffix was empty, later records may establish a current first
record after the header high-water. Later durable markers may likewise advance
the current high-water beyond the header value. The generation-aware source
reports these current record-derived boundaries while retaining the immutable
generation and checkpoint anchor from the header.

Open validates the selected V4 header before using its metadata, then validates
every complete retained record group, the installed first retained boundary,
strict position order, record payload, durable extent, nonregressing logical
high-water, and allocator continuation. A nonempty installed suffix must still
begin at its header boundary. An initially empty suffix may begin only through
valid later position-contiguous appends, and the current durable high-water may
only equal or advance beyond the installed header high-water. Open never assumes
the first retained position is one and never derives next position from physical
record count.

A corrupt, anchorless, generation-zero V4, inconsistent floor/high-water,
unbacked high-water, zero allocator high-water, malformed retained group, or
foreign selected file is rejected. V3 parsing and bytes remain unchanged.

## Filesystem Atomic Replacement

The adapter retains the caller-selected WAL path, its selected inode handle and
exclusive lock, and the parent-directory handle required for synchronization.
Reclamation is offline with respect to cooperating writers because the complete
startup owner still exclusively owns the WAL, page store, and checkpoint source.

The replacement uses one fixed sibling candidate that is never a selectable
generation:

1. fully scan and validate the locked selected source and build a bounded exact
   retained-frame plan;
2. reject generation overflow, a volatile complete suffix, a stale permit, or
   selected/candidate inode aliasing before changing the selected path;
3. remove only the fixed leftover candidate under the reviewed cleanup rule;
4. create the candidate exclusively and acquire its inode lock;
5. cross the explicit pre-write fault boundary, then write the complete V4
   header and exact retained frame groups;
6. synchronize the candidate;
7. rename the candidate over the selected path without delete fallback;
8. immediately move the still-locked candidate handle into the poisoned owner,
   then release the old selected inode handle so every post-rename failure still
   retains the selected inode lock;
9. synchronize the retained parent directory; and
10. install the new in-memory format, generation, boundaries, and record state.

The old handle remains locked through rename. The new handle is locked before
rename and remains locked afterward, so cooperative exclusion has no unlocked
interval, including an after-rename or directory-sync error.

A crash or error may leave the selected path naming either the old complete
generation or the new complete generation and may leave a fixed candidate.
Open never promotes candidate bytes, even if they contain a valid higher
generation. Only after the selected file is locked and fully validated may open
remove that exact sibling candidate and synchronize the directory. An invalid
selected file is not repaired from the candidate.

Every failure after adapter invocation is terminal to the domain. Filesystem
poisoning prevents later mutation through an uncertain same owner. Fresh reopen
selects only the exact `selected` path and converges on one fully validated
generation or returns a typed error.

## Lock and Concurrency Boundary

This decision preserves ADR 0052's acquisition order:

1. WAL selected inode;
2. page store; and
3. completeness checkpoint control.

All three owners remain private through selection, planning, repair,
restoration, completion, retention analysis, reclamation, installed-source
verification, and terminal failure. Reclamation does not add a database-wide
lock or online-reader epoch.

Locks remain cooperative, advisory, process-lifetime locks. Non-cooperating
writers, hostile path replacement, unsupported network-filesystem rename or
locking semantics, and direct external byte mutation remain outside the trust
boundary and fail later validation where observable.

## Error and Authority Boundary

Pre-effect failures distinguish checkpoint-source, inventory, retention
metadata, source observation, retained-suffix observation, completion,
retention, generation, format, floor, anchor, and exact record evidence.

Post-invocation failures are explicitly outcome-indeterminate and additionally
distinguish adapter effect, reported receipt, installed metadata, installed
records, callback stability, and second-observation causes.

These errors remain internal startup/storage evidence. They do not enter
`ClientDiagnostic`.

The source observations, anchor, retention analysis, floor, retained record
projection, permit accessors, effect observation, receipt, success owner, and
failure owner cannot directly create or substitute:

- a transaction lifecycle token or active transaction;
- a coordinator or reusable epoch;
- a page-write, repair, or publication permit;
- a WAL append position or durability fence;
- another selected checkpoint or reclamation permit;
- a backup, replication, restore, or compatibility artifact; or
- a client-visible diagnostic.

## Evidence and Compatibility Boundary

All behavior uses repository-authored transaction, page, WAL, checkpoint,
restart, repair, restoration, retention, lock, checksum, and deterministic-fault
contracts.

No external product documentation, proprietary oracle, SDK, driver, native
fixture, captured output, or undocumented format is consulted. V4 is an ntsql
internal format. It is not MDF, NDF, LDF, BAK, TDS, or SQL Server compatibility
evidence.

The anchor algorithm and V4 bytes are versioned so later repository-authored
changes require a new reviewed version rather than reinterpretation.

## Test Boundaries

- Domain tests reject every stale preflight class before invoking the effect.
- Generation-aware selection uses complete-prefix validation only for generation
  zero, accepts absent and present empty generation-zero checkpoints without an
  allocated epoch, rejects minimal/full generation instability, and cannot
  downgrade a pruned selected or rejected state.
- Anchored tests cover checkpoint seeding, pre-frontier validation,
  post-frontier application, empty retained suffixes, independent high-water,
  and page-backing records before the replay window.
- Permit and owning-state compile-fail tests reject construction, cloning,
  widening, reuse, arbitrary-floor substitution, adapter escape, direct
  reclamation, fallback, and live mutation.
- Effect tests cover exact receipts, generation exhaustion, V3-to-V4 and
  V4-to-V4 transitions, malformed reports, success-shaped unapplied effects,
  installed metadata changes, and exact retained-record mismatch.
- Memory tests cover nonempty and empty suffixes, logical-position and epoch
  continuation, deterministic before/after faults, reopen, and repeated
  reclamation.
- Filesystem tests cover V4 golden bytes, big-endian fields, V3-to-V4 and
  V4-to-V4 reopen, exact retained frame groups, empty suffix metadata, append and
  epoch continuation, all replacement fault points including the locked
  candidate's pre-write boundary, candidate non-promotion, directory
  synchronization, lock continuity, and repeated reopen convergence.
- Existing V1/V2/V3, page-store, checkpoint codec/publication, recovery,
  architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- reclaim a WAL without one completed selected-checkpoint restart and ADR 0059
  retention analysis;
- add online or concurrent reclamation, segmented logs, reader epochs, or a
  background compactor;
- retain history for backup, replication, CDC, log shipping, HA, or PITR;
- select, promote, or recover from candidate files;
- renumber logical positions or transaction identities;
- infer page image bytes from checkpoint metadata;
- execute undo, rollback, compensation, or lock-table reconstruction;
- define cleanup for arbitrary files or directories;
- weaken advisory-lock or atomic-rename assumptions; or
- define native SQL Server formats or externally observable compatibility.

## Consequences

One completed selected-checkpoint restart can now consume its exact retention
analysis and replace an eligible WAL prefix once. Every retained logical record,
allocator high-water, logical position high-water, persistent identity, and
selected-checkpoint anchor survives the replacement without renumbering.

Fresh startup explicitly distinguishes complete generation zero from anchored
pruned generations. It can reconstruct transaction state from the selected
checkpoint plus retained suffix without pretending the suffix is a complete
history.

The remaining recovery boundary is issue #171: model the complete
publish/crash/select/replay/repair/restore/complete/retain/reclaim/reopen state
machine across repeated interruption points.
