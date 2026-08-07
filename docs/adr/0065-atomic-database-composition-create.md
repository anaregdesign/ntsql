# ADR 0065: Atomic Database Composition Create

- Status: Accepted
- Date: 2026-08-07
- Issue: #184
- Extends: ADR 0013, ADR 0014, ADR 0045, ADR 0052, ADR 0062, ADR 0063,
  ADR 0064
- Follows: PR #192
- Extended by: #185, #186, #187, #188

## Context

ADRs 0062 through 0064 define an inert database manifest, one immutable
database-owner control, a fixed five-role lock order, and successor child formats
that persist the exact stable database/file/role identity. PR #192 made those
successor children independently observable and bindable, but their standalone
constructors still expose separately published objects. A database must not
become selectable while only a prefix of its WAL, page store, checkpoint slot,
and manifest exists.

Create also has to survive an error or process crash after every filesystem
effect. Removing every remnant is not a safe rollback: an I/O error can make the
effect indeterminate, and deleting exact evidence can destroy the only durable
fact needed to decide a retry. The create operation therefore needs one
manifest-last publication protocol and a strict resolver for its own exact
candidate namespace.

This decision owns only initial creation in `RecoveryRequired` state. It does not
run recovery, grant live authority, publish clean-close evidence, drop a
database, infer native Microsoft formats, or define SQL Server behavior.

## Preconditions and Fixed Names

The caller supplies one already validated manifest and one trusted
`FileDatabaseLayout`. Before filesystem mutation, create requires:

- lifecycle generation exactly `1`;
- lifecycle state exactly `RecoveryRequired`;
- WAL format version `5`;
- page-store format version `2`;
- restart-checkpoint completeness-control version `2`; and
- no required feature bits.

The caller supplies every identity. Create generates no database, WAL, or file
identity and never substitutes a path.

Four fixed unselected names are derived by appending `.create-candidate` to the
selected manifest, WAL, page-store, and checkpoint-slot paths. The stable owner
control has no candidate and is created directly at its final path. The create
candidates are independent from the WAL's `.reclaim-candidate` namespace and
from the checkpoint slot's internal `candidate` entry.

Before mutation, every selected path, create-candidate path, checkpoint
`control` path, and WAL reclamation-candidate path must be lexically distinct.
Existing opened objects are also compared by the platform identity support from
ADR 0064. A same-object alias, candidate/final coexistence, dangling entry,
foreign byte sequence, partial header, unknown checkpoint entry, or unsupported
object shape is terminal evidence. Create does not unlink, truncate, repair,
replace, quarantine, or normalize it.

## Stable Ownership and Lock Order

An entirely absent namespace first creates the owner control exclusively, locks
it before writing, writes the complete ADR 0064 frame, synchronizes the file,
and synchronizes its parent. A retry opens, locks, and validates the exact owner
instead. Owner creation is the only legal durable state before a manifest
candidate exists.

After acquiring the stable owner lock, create re-observes the selected manifest
before acquiring any later lock. An exact already-published composition takes a
strict initial successor-only resolver path and returns `AlreadyPublished`
without releasing the owner lock. It does not invoke the repair-capable ordinary
open path; a different, incomplete, or non-initial final composition is a
conflict.

For an unpublished attempt the retained lock order is:

1. stable database owner;
2. manifest create candidate;
3. candidate or final WAL;
4. candidate or final page store; and
5. candidate or final checkpoint `control`.

An existing object is locked before its bytes are parsed. Existing WAL and page
objects must contain only their exact initial successor header, with no complete
record and no repairable tail. An existing checkpoint slot must contain exactly
one complete `control` entry and no `current`, internal `candidate`, or unknown
entry. Existing files and directories are synchronized again before reuse so a
retry re-establishes their durability assumptions without changing bytes.
Create-specific WAL finalization never runs ordinary reclamation-candidate
cleanup.

## Legal Durable Prefixes

Only these exact namespace states are resumable:

| Phase | Exact entries besides the stable owner |
| --- | --- |
| `Owner` | none |
| `ManifestCandidate` | manifest candidate |
| `WalCandidate` | manifest and WAL candidates |
| `PageStoreCandidate` | manifest, WAL, and page candidates |
| `RestartCheckpointCandidate` | all four candidates |
| `WalPublished` | final WAL plus the other three candidates |
| `PageStorePublished` | final WAL/page plus checkpoint and manifest candidates |
| `ChildrenPublished` | all three final children plus manifest candidate |
| `Published` | all four final selected entries and no create candidate |

The all-absent observation precedes owner creation. Every other combination is
a conflict, including an out-of-order final child, a skipped candidate, a final
manifest without all exact final children, or any candidate remaining beside a
final manifest. Resolver decisions use freshly observed namespace and bytes;
the caller's prior result, requested phase, or cached path observation is not
authority.

## Child-First, Manifest-Last Publication

Candidate construction writes and synchronizes, in order:

1. manifest candidate;
2. WAL candidate;
3. page-store candidate; and
4. restart-checkpoint candidate control and slot directory.

Each creation also synchronizes the containing parent directory. Publication
then renames and parent-synchronizes, in order:

1. WAL candidate to selected WAL;
2. page candidate to selected page store;
3. checkpoint candidate directory to selected checkpoint directory; and
4. manifest candidate to selected manifest.

Immediately before every rename, the selected destination is rechecked as
absent. `std::fs::rename` supplies the namespace switch, while the stable owner
lock excludes every cooperating lifecycle writer. The standard library does not
offer a portable atomic no-replace rename, so trusted paths and exclusion of
noncooperating namespace writers remain composition-root requirements. A
hard-link-then-unlink protocol is rejected because a crash would expose
candidate/final inode aliases as another apparent phase.

The manifest parent-directory synchronization is the only direct create success
point. Before the manifest rename, even three durable final children remain
unpublished and fresh open fails because no selected manifest exists. A manifest
may be visible after rename but before that parent sync completes. Every ordinary
ownership acquisition therefore synchronizes the validated selected manifest
and its parent while holding all five locks, after validating child evidence but
before repairing any child or returning authority. It either completes the same
durability barrier or returns no recovery authority. Create retains every child
and manifest inode lock across rename. It rebinds the WAL's selected path and
checkpoint slot directory after their namespace switches so later reclamation
and checkpoint publication derive names from the selected paths.

Successful create directly constructs the existing unrecovered composition,
selects the retained manifest, binds the independently observed stable child
identity, and returns `RecoveryRequiredDatabase`. It never reopens a child or
introduces an ownership gap.

## Retry and Already-Published Semantics

An explicit create retry may resume only an exact legal prefix for the same
manifest and stable storage identity. It reacquires all existing objects in
lock order and completes only the missing suffix. It never searches alternate
paths or adopts bytes merely because they decode.

An exact `Published` composition returns a distinct `AlreadyPublished` outcome
while retaining the same recovery-required ownership returned by `Created`.
Missing, corrupt, foreign, stale, aliased, non-initial, or contradictory evidence
returns a typed failure. No error is converted into `Created`, and no prior
success-shaped result substitutes for fresh observation.

Ordinary fresh open never reads a `.create-candidate` path. Candidates are
unselected evidence available only to the explicit create resolver.
Deterministic memory acquisition likewise rejects every unpublished create phase
and may select only the modeled objects retained by a `Published` create record,
regardless of which modeled owner slot initiates acquisition.

## Fault and I/O Semantics

The deterministic create fault model names each durable boundary:

- owner publication;
- manifest-candidate publication;
- WAL-candidate publication;
- page-candidate publication;
- checkpoint-candidate publication;
- WAL publication;
- page-store publication;
- checkpoint publication; and
- manifest publication.

Each boundary supports four timings:

- `BeforeEffect`: report a definite injected failure before changing the phase;
- `AfterEffect`: install and durably synchronize the complete phase, then report
  a definite injected failure;
- `OutcomeIndeterminateBeforeEffect`: leave the prior phase but report that the
  caller may not rely on no effect; and
- `OutcomeIndeterminateAfterEffect`: install the complete phase but report that
  the caller may not rely on the returned observation.

Injected faults never produce partial bytes. Actual create, open, read, write,
lock, metadata, synchronization, and rename failures preserve their exact I/O
stage and are outcome-indeterminate. The current owner is dropped on error; only
a new attempt that reacquires the owner and observes the complete namespace can
decide what happened.

## Deterministic Memory Parity

`ntsql-storage-memory` stores the same legal phase and exact typed evidence in
its shared ownership world. It uses the same boundary/timing vocabulary,
child-first order, manifest-last success point, retry rules, and
`Created`/`AlreadyPublished` distinction. Candidate and final names are modeled
as phase-selected entries over one stable object identity, not as fabricated
filesystem paths or descriptors.

Each modeled create record carries its database identity. Once any owner phase
is published, another modeled owner slot for that database may neither create a
parallel composition nor use ordinary acquisition to bypass the recorded phase.
The memory model permanently binds each modeled object to one database role only
after exact evidence is installed. It rejects conflicting retries, duplicate
object identities, foreign manifests, and non-successor format requirements.
It does not model inode numbers, symlinks, directory handles, or operating-system
rename implementation details.

## Architecture and Compatibility Boundary

No crate or dependency edge changes:

```text
ntsql-database -------> ntsql-wal
ntsql-storage-file ---> ntsql-database, ntsql-page, ntsql-transaction, ntsql-wal
ntsql-storage-memory -> ntsql-database, ntsql-page, ntsql-transaction, ntsql-wal
```

Domain crates remain I/O-free. All candidate names, phases, format
prerequisites, synchronization points, errors, and test faults are
repository-owned. They define no SQL Server create transaction, file layout,
database state, error number, compatibility result, or native format.

## Test Boundaries

- Every legal prefix resumes to one exact published composition.
- Every boundary/timing pair leaves the exact expected fresh phase and succeeds
  on an unfaulted retry.
- Repeated create returns `AlreadyPublished` and retains all five locks.
- A fresh ordinary open never selects a valid candidate-only composition.
- A visible manifest whose create-side parent sync did not complete is
  parent-synchronized by ordinary ownership acquisition before child repair.
- Partial, malformed, foreign, non-initial, unknown-entry, mixed
  candidate/final, out-of-order final, and same-object alias evidence fails
  without repair or cleanup.
- Candidate publication retains inode locks and rebinds WAL/checkpoint paths.
- Manifest publication is last; no earlier phase opens as a database.
- Memory and filesystem runners agree on every legal phase and declared fault.
- Process-reopen tests cover every publication boundary and preserve exact
  identity, bytes, and namespace evidence.
