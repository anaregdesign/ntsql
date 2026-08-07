# ADR 0064: Database-Wide Cooperative Ownership

- Status: Accepted
- Date: 2026-08-07
- Issue: #183
- Extends: ADR 0001, ADR 0014, ADR 0045, ADR 0062, ADR 0063
- Follows: #182
- Extended by: ADR 0065, ADR 0066

## Context

The WAL, page store, and restart-checkpoint completeness source already retain
independent cooperative lifetime locks. Opening those adapters separately does
not establish one database owner, prevent two processes from selecting different
manifest generations, or prove that the selected paths form one composition.

The selected manifest inode cannot be the stable database lock. Later lifecycle
work must replace the manifest path atomically. A lock retained on the old inode
would not exclude a cooperating opener of the replacement inode. Conversely, a
filename or decoded lock token cannot be authority because either can be copied
without retaining an operating-system lock.

This issue must establish the outer ownership and lock topology before atomic
create or recovery handoff. It must reuse the complete unrecovered child
adapters: retaining raw locked descriptors would require issue #185 either to
reopen them, creating an ownership gap, or to duplicate their reconstruction
invariants.

## Decision

`ntsql-storage-file` owns one stable database-owner control file and acquires all
database locks in this order:

1. immutable database-owner control;
2. currently selected manifest inode;
3. transaction/page WAL;
4. page store; and
5. restart-checkpoint completeness `control`.

The database-owner control is the stable cooperative exclusion boundary across
manifest replacement. The manifest lock protects the exact selected inode while
it is decoded and retained. The three existing child adapters retain their own
locks and reconstructed unrecovered state.

`open_file_database_ownership` returns
`FileDatabaseOwnershipSelection`, a private newtype around
`ManifestSelectedDatabase<FileDatabaseOwnership<N>>`. The private outer owner
retains the child composition, manifest descriptor, database-owner descriptor,
decoded manifest, and trusted path selection. No API extracts the inner selected
owner, explicitly unlocks it, invokes exact-composition binding, or promotes it
to live state. Issue #184 must add physical child identity evidence before this
owner may reach `RecoveryRequiredDatabase`; issue #185 must then consume that
same retained owner when it adds the recovery-to-live gate.

## Stable Owner-Control Format

Owner-control version 1 is exactly 64 bytes. All multibyte fields are unsigned
big-endian.

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | independent magic, ASCII `NTSQDBO1` |
| 8 | 2 | format version, exactly `1` |
| 10 | 2 | frame length, exactly `64` |
| 12 | 4 | header flags, exactly zero |
| 16 | 16 | nonzero repository-owned database ID |
| 32 | 24 | reserved zero bytes |
| 56 | 8 | checksum of bytes `0..56` |

The checksum reuses only the repository-owned arithmetic specified by ADRs 0013,
0044, 0049, and 0063. The owner-control magic and dispatch remain independent
from manifest, WAL, page-store, and checkpoint formats.

The exact frame for database ID
`0x0102030405060708090a0b0c0d0e0f10` is golden-tested, including checksum
`0xccc2c3c6b6e8ec40`.

Encoding and decoding are pure byte operations. Encoded bytes are inert and do
not create a file, allocate an identity, acquire a lock, publish a database, or
grant lifecycle authority. Decoding validates exact supplied length, magic,
version, encoded length, flags, checksum, every reserved byte, and the nonzero
database ID with typed failures.

## Trusted Layout and Opened-Object Identity

`FileDatabaseLayout` is an inert caller-supplied selection of five trusted paths.
It is cloneable because a path is neither a persistent identity nor lock
authority. The restart-checkpoint path names the completeness slot introduced by
ADRs 0051 and 0053; its stable lock target is that slot's `control`, not the
legacy baseline slot and not its replaceable `current` entry.

After locking and validating owner control and manifest, the opener obtains
metadata from each selected child object without parsing or mutating it. On Unix,
device/inode identity rejects every later role that aliases an earlier owner,
manifest, or child object. Each child is then opened through its existing
lock-before-parse adapter. Its locked descriptor must identify the same object as
the preflight descriptor, rejecting a path replacement between selection and
locked open.

These checks use opened descriptors rather than path spelling. Distinct path
strings, lexical normalization, or canonicalized names are not treated as
identity.

## Validation and Reconstruction Order

The acquisition gate performs:

1. validate page geometry without opening child storage;
2. open and nonblocking-lock owner control before reading its bytes;
3. require its database ID to equal the caller-selected database ID;
4. open the manifest, reject alias with owner control, lock it, decode its exact
   fixed frame, and require the same database ID;
5. preflight all child opened-object identities and the derived WAL reclamation
   candidate in lock order;
6. lock and inspect the WAL without tail repair or candidate cleanup, then
   require the selected object, manifest WAL format, and persistent WAL ID;
7. lock and inspect the page store without tail repair, then require the
   selected object, its format, and the same persistent WAL ID;
8. lock and reconstruct the completeness checkpoint slot, then require its
   selected object, control format, and the same persistent WAL ID;
9. only after all validation, finalize the WAL and page store, including their
   reviewed incomplete-tail repair and safe WAL-candidate cleanup;
10. move those already locked adapters directly into the existing complete
    unrecovered composition without reopening; and
11. pass the retained outer owner only through ADR 0062's manifest-selection
    gate.

The existing public WAL and page-store openers use the same internal
inspect/finalize split. The split changes no accepted bytes or repair rule. The
database gate keeps every inspected child locked while it validates the complete
set; a late child rejection therefore cannot truncate an earlier WAL/page tail
or delete the WAL reclamation candidate. Finalization is adapter reconstruction,
not database crash recovery.

The derived `<wal>.reclaim-candidate` path is an auxiliary mutation target even
though it is not a lifetime lock. Before WAL inspection, it must not lexically
select or resolve to owner control, manifest, WAL, page store, or checkpoint
control. This prevents ordinary WAL open cleanup from unlinking another selected
database role.

Failures distinguish outer I/O stage, owner-control structure or identity,
opened-object alias/replacement, manifest structure or identity, each child
adapter open, required format, persistent WAL identity, and domain staging.
Acquisition is nonblocking. There is no wait, retry, timeout, lock stealing,
fallback manifest, substitute role, or success-shaped error.

On every failure, ordinary ownership destruction releases the latest child and
all earlier retained locks. The successful value has no explicit unlock path;
dropping its lifecycle typestate releases the complete set.

## Legacy and Successor File-Identity Boundaries

The legacy WAL V3/V4, page-store V1, and completeness-control V1 formats persist
their shared `PersistentLogId` and role-specific format magic, but they do not
persist `DatabaseId` or `DatabaseFileId`. The legacy-compatible
`open_file_database_ownership` gate can therefore physically establish:

- the stable owner-control database ID;
- the manifest database ID and logical role-to-file-ID association;
- one distinct opened object for each trusted layout role where supported;
- each role's repository-owned format; and
- one shared persistent WAL identity.

The manifest file IDs remain logical selected-role identities at that boundary;
they are not independently re-read from legacy child headers. A same-role
substitute with the same physical format and persistent WAL ID is therefore not
distinguishable solely through legacy formats. The legacy-compatible gate does
not copy manifest fields into alleged observations and returns only
manifest-selected authority.

Issue #184 adds WAL V5, page-store V2, and completeness-control V2. Each persists
one checksummed child extension containing the stable `DatabaseId`, exact
`DatabaseFileRole`, and `DatabaseFileId`. The extension excludes lifecycle
generation. Generation belongs only to the replaceable manifest and may advance
on clean-close or other adjacent manifest publication without rewriting all
children. WAL reclamation has its own independent WAL generation and preserves
the stable child extension.

`open_recovery_required_file_database` accepts only those successor versions. It
parses the physical version and child identity from each locked adapter, checks
database ID, exact role, file ID, required format, persistent WAL ID, and complete
stable-storage identity, then consumes the retained manifest-selected owner into
`RecoveryRequiredDatabase`. A legacy or mixed composition remains openable only
through the weaker manifest-selected gate. Existing format versions are never
reinterpreted to contain successor fields.

Content-level staleness, replay correctness, and the recovery-to-live transition
remain owned by issue #185.

## Filesystem Assumptions

All locks are standard-library, nonblocking, exclusive, advisory locks.
Correctness requires trusted paths, a filesystem implementing the required
operations, and a policy that all ntsql database lifecycle writers acquire the
stable owner lock before a manifest or child lock.

On supported Unix filesystems, opened-object equality uses device and inode
metadata and hard-link aliases are rejected. The standard library exposes no
equivalent portable file identity used here on other targets, so this ADR claims
no same-file alias or selection-to-lock replacement detection there. A platform
or filesystem that cannot provide the required locking behavior returns its
original typed I/O failure.

This protocol is not a hostile-path sandbox. A privileged or noncooperating actor
can ignore advisory locks, mutate an opened file, replace path components,
unlink an owner-control inode, or exploit filesystem-specific semantics. No
network-filesystem, symlink confinement, mandatory lock, process identity,
distributed lease, or stale-lock recovery guarantee is made.

## Deterministic Memory Semantics

`ntsql-storage-memory` provides an explicit ownership world. Equal modeled object
IDs resolve to one shared private slot state inside that world; callers cannot
recreate an independent slot merely by repeating public numeric IDs. Separate
worlds represent separate model executions. Acquisition guards owner, manifest,
WAL, page store, and checkpoint object states in the filesystem lock order.
Owner contention fails before inspecting supplied evidence; later-object
contention fails at that role, and every rejection releases the complete
acquired prefix. Successful validation permanently binds each object state to
its database and role while the five guards remain held.

Synthetic opened-object IDs model owner/manifest/child alias detection without
pretending to be operating-system descriptors. File observations validate the
exact role set, logical file IDs, required formats, and persistent WAL identity
in the same stable order. The legacy-compatible operation returns a private
`InMemoryDatabaseOwnershipSelection` around manifest-selected ownership.
`try_acquire_recovery_required` additionally binds the complete stable-storage
observation and returns recovery-required authority while retaining all guards.
Adjacent manifest generations accept the same unchanged child observations.

The memory adapter models the stronger identity evidence that issue #184 must
persist in filesystem child headers. It does not emulate paths, inodes, advisory
lock implementation details, or filesystem races.

## Architecture and Compatibility Boundary

No crate or dependency edge changes. The existing reviewed graph remains:

```text
ntsql-database -------> ntsql-wal
ntsql-storage-file ---> ntsql-database, ntsql-page, ntsql-transaction, ntsql-wal
ntsql-storage-memory -> ntsql-database, ntsql-page, ntsql-transaction, ntsql-wal
```

All control bytes, lock roles, ordering, errors, and tests are repository-owned.
No external product documentation, driver, SDK, fixture, oracle, captured
output, proprietary governance tool, or native database file is consulted.

This decision defines no SQL Server lock, database ID, file ID, startup phase,
error number, diagnostic, MDF/NDF/LDF/BAK format, or compatibility claim.

## Test Boundaries

- Owner-control encoding has exact golden bytes, full prefix/trailing checks,
  independent header fields, every reserved byte, checksum, and zero-ID tests.
- Stable-owner contention precedes manifest parsing.
- Manifest contention releases stable ownership.
- Missing WAL, page-store contention, and checkpoint contention release every
  earlier lock and permit immediate reopen.
- A late foreign checkpoint leaves earlier incomplete WAL/page tails and a WAL
  reclamation candidate byte-for-byte untouched.
- Owner, manifest, and every successor child reject foreign physical database,
  role, and file identity evidence.
- Every manifest-required child format is checked in stable role order.
- Missing and reversed layout roles fail without fallback.
- On Unix, all ten later-to-earlier hard-link alias pairs are rejected from
  opened-object identity.
- The derived WAL reclamation candidate cannot lexically select or hard-link
  alias any of the five database lock targets.
- Success retains all five lock targets until the database typestate is dropped,
  exposes only the stage justified by the selected opener, and permits repeated
  open after drop.
- Successor-only exact open reaches recovery-required authority; legacy or mixed
  formats cannot claim that transition.
- WAL V5 reclamation remains V5, advances only its independent WAL generation,
  and preserves the exact stable child identity under the complete header
  checksum.
- Memory tests cover world-stable identity and role binding, contention/release
  across distinct owner slots sharing a child, owner/manifest identity, missing
  and duplicate roles, reversed file IDs, every foreign lineage/format, and all
  ten modeled alias pairs.

## Non-Goals

This ADR does not:

- create, replace, synchronize, publish, migrate, or repair a manifest;
- reinterpret legacy headers as if they persisted child database/file identities;
- define or execute database crash recovery or live release;
- add clean/unclean close evidence or a tombstone/removal protocol;
- define waiting, lock stealing, mandatory locking, or distributed ownership;
- provide hostile path traversal or unknown-entry cleanup; or
- define native Microsoft behavior or persistent format compatibility.

## Consequences

Database lifecycle work now has one stable outer cooperative owner and one fixed
lock order that retains the complete existing unrecovered child composition
without a reopen gap or pre-validation repair. Atomic create can build and
publish that exact topology; open/recovery can later consume it without
recreating authority from paths or decoded values.

Legacy child formats deliberately remain weaker than the successor formats.
Issue #184's successor headers close the stable-identity gap without making
manifest generation immutable in children; issue #185 still owns content-level
recovery validation before live authority.
