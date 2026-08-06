# ADR 0046: Atomic Filesystem Restart Checkpoint Publication

- Status: Accepted
- Date: 2026-08-06
- Issue: #144
- Extends: ADR 0042, ADR 0044, ADR 0045

## Context

ADR 0045 provides a database-bound checkpoint slot, stable lineaged control
lock, optional `current` blob, and exact filesystem read source. It deliberately
does not implement ADR 0042's sibling publisher.

A filesystem publisher must satisfy a stronger postcondition than writing
decodable bytes. `Ok(())` must mean the selected current path durably names
exactly the supplied authoritative baseline. Replacing an old current blob
cannot introduce a remove-then-create absence window, and the stable lock cannot
move from `control` to either temporary or selected data.

Publication also has unavoidable intermediate states. A process or injected
failure may leave a stale temporary file, may occur after selected-path
replacement but before directory synchronization, or may occur after the
replacement is durable but before success is returned. ADR 0042 already
classifies every invoked publisher error as outcome-indeterminate. The adapter
must preserve that classification while making each physical stage testable.

## Crate and Dependency Boundary

Only `ntsql-storage-file`, its tests, and the owning ADRs change:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No crate, dependency edge, architecture registration, third-party dependency,
domain I/O, WAL format, page-store format, control format, or checkpoint blob
format changes. The existing filesystem source implements the existing domain
publisher port.

## Sibling Source and Publisher

`FileRestartCheckpointBaselineSource` now implements both:

- `DurableTransactionRestartCheckpointBaselineSource`; and
- `DurableTransactionRestartCheckpointBaselinePublisher`.

The adapter retains the same immutable `control` file lock and slot-directory
handle for its lifetime. Publication does not lock, truncate, or retain an open
handle to `current`. Read behavior, untrusted output, exact absence rules, and
ADR 0039 validation remain unchanged.

Implementing both sibling ports does not make a loaded observation
authoritative and does not allow direct publication without the private owner
permit.

## Permit and Slot Checks Before Effect

The publisher first compares the supplied authoritative baseline with its
invariant owner permit:

- persistent log ID;
- optional numeric durable frontier; and
- transaction-entry count.

Any disagreement returns
`FileRestartCheckpointBaselinePublicationError::PublicationPermitMismatch`.
The publisher then compares the baseline persistent ID with the immutable slot
control ID. A mismatch returns `SlotPersistentLogIdMismatch`.

Both checks occur before encoding, fault consumption, candidate cleanup, path
creation, write, synchronization, or replacement. A wrong database slot
therefore cannot destroy a stale candidate or selected value. An armed fault
also remains armed.

The permit is still not a filesystem, durability, validity, replay, or retention
proof. It identifies only the owner-authorized call and baseline.

## Authoritative Encoding Before Filesystem Mutation

After identifier checks, the publisher calls only ADR 0044's
`encode_restart_checkpoint_baseline`. Encoding accepts the private-field
authoritative baseline and performs its complete fallible reservation before
returning bytes.

Encoding failure is preserved as the exact nested `Encode` cause and occurs
before filesystem mutation or fault consumption. The domain still wraps it as
outcome-indeterminate because the abstract publisher was invoked; callers may
not use the adapter stage to weaken ADR 0042.

## Fixed Unselected Candidate

The ADR 0045 slot reserves one additional fixed name:

- `candidate`: temporary bytes that are never selected by the read source.

Only `current` is selected. A complete, partial, empty, malformed, or stale
`candidate` has no checkpoint, fallback, generation, recovery, or retention
authority. Open and load continue to ignore it.

Each publication begins by unlinking the candidate entry with
`remove_file`. `NotFound` means there is no stale candidate. Any other error is
reported at `RemoveCandidateFile`.

Unlink-before-create is deliberate. It removes a stale regular file, symlink, or
hard-link directory entry without opening or following it. A directory or other
entry that cannot be unlinked as a file fails explicitly. After cleanup, the
publisher creates a fresh candidate with `create_new`; it never truncates or
follows an existing candidate.

This cleanup is safe because candidate is explicitly unselected. It does not
apply to `current`.

## Publication Sequence

After checks and encoding, one attempt performs:

1. remove a stale unselected `candidate`, accepting only `NotFound`;
2. create a fresh `candidate` exclusively for writing;
3. write every ADR 0044 encoded byte;
4. synchronize the candidate file with `sync_all`;
5. close the candidate file handle;
6. rename `candidate` to `current` within the same slot directory;
7. synchronize the retained slot-directory handle with `sync_all`; and
8. report `Ok(())`.

Each standard-library failure retains an exact
`FileRestartCheckpointSlotIoStage`:

- `RemoveCandidateFile`;
- `CreateCandidateFile`;
- `WriteCandidateFile`;
- `SyncCandidateFile`;
- `ReplaceCurrentFile`; or
- `SyncPublishedSlotDirectory`.

Closing before rename avoids retaining the temporary handle across replacement
and follows the same physical sequence on every supported platform.

## Atomic Replacement Without Delete Fallback

The publisher calls `rename(candidate, current)` directly. Both paths are fixed
children of the same trusted slot directory.

It never removes `current` first and never falls back to
remove-then-rename. If the platform or filesystem cannot replace the existing
file through the standard-library rename operation, publication returns the
exact replacement I/O error. The old selected entry or another
outcome-indeterminate physical state is then interpreted only by a later fresh
load; no false success is reported.

When rename succeeds, the selected path names the complete synchronized
candidate bytes. Success is still withheld until slot-directory synchronization
succeeds, making the selected directory entry durable according to the required
standard-library operations.

The control file and its lock are not renamed. Current replacement therefore
does not change cooperating exclusion.

## Success and Error Postconditions

`Ok(())` is returned only after:

- exact permit and slot identity checks;
- complete ADR 0044 encoding;
- complete candidate write;
- candidate synchronization and close;
- successful selected-path replacement; and
- slot-directory synchronization.

At that point `current` is exactly the supplied baseline bytes and a later
sibling load structurally lowers those bytes without normalization. The domain
constructs its non-cloneable publication receipt only from this unit success.

Every adapter error after invocation is exposed through ADR 0042 as
`DurableTransactionRestartCheckpointBaselineCurrentPublicationError::Publication`
with the exact attempted identifiers and indeterminate token. This remains true
for permit, slot, encode, injected before-effect, and physically typed I/O
errors. The stage is diagnostic evidence, not retry or non-effect authority.

## Deterministic Fault Boundaries

The adapter supports one armed publication fault:

| Fault | Physical state when returned |
| --- | --- |
| `BeforeCandidateCleanup` | prior candidate and current unchanged |
| `AfterCandidateCleanup` | candidate absent; current unchanged |
| `AfterCandidateCreate` | empty candidate; current unchanged |
| `AfterCandidateWrite` | complete unsynchronized candidate; current unchanged |
| `AfterCandidateSync` | synchronized closed candidate; current unchanged |
| `AfterCurrentReplace` | candidate absent; current names new bytes; directory not synchronized by this attempt |
| `AfterDirectorySync` | candidate absent; current names durably synchronized new bytes |

Fault arming never replaces an existing plan. Permit mismatch, slot mismatch,
and encode failure do not consume the plan because its physical boundary was not
reached. A matching fault is one-shot.

The last two fault points deliberately demonstrate why an error cannot mean the
old slot remained selected. `AfterDirectorySync` also demonstrates that an
error cannot mean the new value is not durable.

## Fresh Owner Operation After Failure

The adapter has no direct retry method and does not convert an indeterminate
token into a baseline. A caller may later invoke a new
`publish_restart_checkpoint_baseline_from_current_prefix` owner operation. That
operation performs fresh current-WAL analysis, prepares a new authoritative
baseline, creates a new invariant permit, and invokes the publisher anew.

The new physical attempt safely unlinks any prior unselected candidate and
replaces whichever `current` entry is then selected. Tests exercise this fresh
operation after every injected fault and after typed cleanup and replacement
failures.

This is not proof that the earlier attempt had or had not taken effect. It is a
new independently authorized replacement.

## Trusted Path and Filesystem Boundary

ADR 0045's advisory control lock and trusted-path boundary remains authoritative.
A malicious or noncooperating actor can race unlink/create/rename operations,
replace path components, mutate synchronized bytes, ignore the control lock, or
choose unsupported filesystem semantics.

The composition root must provide:

- a trusted stable slot path;
- one filesystem namespace for candidate and current;
- standard-library file and directory synchronization;
- replacement semantics capable of satisfying successful rename; and
- cooperation by all ntsql publishers through the control lock.

No secure directory traversal, mandatory locking, hostile-writer defense,
network-filesystem guarantee, distributed lease, or external durability oracle
is claimed. Unsupported operations fail explicitly.

## Authority and Error Boundary

Publication faults and adapter errors remain internal operational data. Their
paths, OS causes, bytes, and physical stage do not enter `ClientDiagnostic`.
`Error::source` preserves encode and I/O causes; mismatch and injected-fault
variants fabricate no nested cause.

The filesystem publisher, candidate, current bytes, receipt, and indeterminate
token cannot create or satisfy:

- transaction lifecycle or coordinator state;
- WAL append, flush, restart-analysis, or lineage authority;
- page-store or committed-page recovery write authority;
- recovered or restart-analyzed storage ownership;
- decoded checkpoint validity or startup selection;
- dirty-page tables, replay starts, redo, undo, rollback, or compensation; or
- retention floors, truncation, compaction, or reclamation.

Existing compile-fail coverage rejects direct publication without the private
permit and publisher substitution for WAL, page-store, recovery-store,
transaction, or restart-analyzed owner authority.

## Evidence and Compatibility Boundary

The candidate protocol, synchronization sequence, fault effects, errors, and
tests are repository-authored. No external product documentation, driver, SDK,
fixture, oracle, proprietary governance tool, or native MDF/NDF/LDF/BAK format
is consulted.

This decision defines no SQL Server checkpoint write, file replacement,
durability point, startup behavior, recovery phase, error, diagnostic, or
compatibility result.

## Test Boundaries

- Success removes a stale candidate, publishes exact empty bytes, returns exact
  receipt identifiers, and leaves no candidate.
- A second success replaces current with an exact nonempty baseline.
- Sibling load returns exact untrusted fields and real current-WAL validation is
  still required.
- Every fault point exposes the exact candidate/current bytes in the table,
  exact indeterminate identifiers, and complete cause chain.
- A fresh owner operation succeeds after every fault and removes any stale
  candidate.
- Wrong-slot publication preserves candidate/current state and its armed fault.
- A candidate directory fails at cleanup without changing current.
- A current directory fails at replacement, remains present, and proves no
  delete fallback occurred.
- After removing the obstructing directory, a fresh operation reconciles the
  prior candidate and succeeds.
- On supported Unix test filesystems, a stale candidate symlink is unlinked
  without changing its target.
- Existing source, codec, memory publisher, WAL, page-store, recovery, restart,
  architecture, and governance tests remain valid.

## Non-Goals

This ADR does not:

- add generation numbers, multiple slots, history, fallback, or selection;
- expose candidate as source data or recovery evidence;
- add automatic retry, backoff, cleanup on drop, or indeterminate resolution;
- guarantee atomic replacement after a returned error;
- make checkpoint presence or validity a startup gate;
- add a dirty-page table, replay start, redo, undo, rollback, compensation, or
  coordinator restoration;
- truncate, compact, or reclaim WAL;
- define database-wide atomicity across WAL, page store, and checkpoint; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql now has a fail-closed persistent implementation of the temporary
single-current-slot publisher. Successful publication durably selects exact
repository-encoded baseline bytes under the stable lineaged control lock, while
all failed calls retain ADR 0042's outcome-indeterminate boundary.

This transaction-only baseline still cannot shorten restart replay or authorize
WAL reclamation. The next checkpoint-domain slice must define independently
reviewed dirty-page/replay-start completeness before startup can use persisted
checkpoint state for recovery work.
