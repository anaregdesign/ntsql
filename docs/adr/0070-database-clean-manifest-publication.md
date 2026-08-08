# ADR 0070: Database Clean-Manifest Publication

- Status: Accepted
- Date: 2026-08-08
- Issue: #186
- Extends: ADR 0067, ADR 0068, ADR 0069

## Context

ADR 0069 consumes `LiveDatabase` and retains one exact clean-close certificate,
the adjacent `Clean` Manifest V2, the transaction clean-close checkpoint
candidate, and every database/transaction owner in `ClosePendingDatabase`.
It deliberately provides no route to `ClosedDatabase`.

The remaining transition is effectful. A manifest replacement can fail before
selection, after selection but before its durability barrier, or while the
caller cannot determine whether selection occurred. Treating every error as
"the recovery-required source is still selected" would create false recovery
authority. Conversely, constructing `ClosedDatabase` from an attempted write,
an identity-only comparison, or a caller-created receipt would advertise clean
durability without proof.

The filesystem transaction adapter also lacks ADR 0068's dedicated clean-close
checkpoint candidate. Reusing the recovery-selected checkpoint would allow a
failed close attempt to mutate restart authority before the clean manifest is
durable.

## Decision

### One-use publication authority

Add the only close-completion transition:

```text
ClosePendingDatabase<PreparedDatabaseCloseOwnership<...>>
    -> ClosedDatabase<PublishedDatabaseCloseOwnership<...>>
```

`ClosePendingDatabase::close` consumes ClosePending at entry. Immediately before
granting publication authority it reobserves the selected source manifest
through the retained database-wide owner and requires exact equality with the
source retained during close preparation. The retained source must still:

1. identify the exact source composition;
2. be `RecoveryRequired`;
3. produce the retained certificate and target through
   `source.next_clean(certificate)`; and
4. have the target as its exact adjacent full-manifest successor.

The domain then creates a private, non-cloneable, attempt-branded
`DatabaseCleanManifestPublicationPermit`. Only an implementation of
`DatabaseCleanManifestPublisher` receives that permit. Completing it records
the publisher's independently observed selected and synchronized manifests in
a private `DatabaseCleanManifestPublicationReceipt`.

The domain accepts the receipt only when both complete manifests exactly equal
the retained target, including lifecycle generation, lifecycle state,
certificate, storage formats, required features, and every stable database,
child-file, and persistent-WAL identity. Identity-only equality is
insufficient.

Success retains the prepared database and transaction owners together with the
receipt in `PublishedDatabaseCloseOwnership`. `ClosedDatabase` reports the
target composition and is constructible only after that exact receipt passes.

### Terminal publication failures

`DatabaseCleanManifestPublicationState` classifies the durable selection
knowledge at the return boundary:

- `SourceSelected`: no target-manifest selection effect can have occurred;
- `SelectionIndeterminate`: the caller cannot know whether source or target is
  selected;
- `TargetSelectedDurabilityIndeterminate`: target selection is known, but its
  durability barrier is not;
- `TargetDurable`: the target and its containing-directory barrier completed,
  although a later injected or bookkeeping error prevented success.

`FailedDatabaseClosePublication` retains every prepared database and
transaction owner and the publisher's exact typed cause. It exposes borrowed
classification and evidence only. It has no owner extraction, same-owner retry,
or path to `ClosedDatabase`.

`AbandonedDatabaseClosePublication` is distinct from ADR 0069's
`AbandonedDatabase`. It records source identity, target identity, and the final
publication-state classification, then relinquishes all owners. It does not
claim that the recovery-required source remains selected. Recovery after any
publication error requires a fresh open under newly acquired ownership.

A publisher success receipt that contradicts the target is classified as
`SelectionIndeterminate`, never `SourceSelected`, because an effectful publisher
already consumed the permit.

### Memory publication model

The memory database slot owns the selected manifest instead of treating the
opening caller's manifest as selected by assertion. Acquisition requires the
supplied manifest to equal that stored manifest exactly.

Clean publication models four ordered boundaries:

1. write the close candidate;
2. atomically select it as the manifest;
3. reobserve the selected manifest;
4. synchronize the publication.

Each boundary supports deterministic before-effect, after-effect,
outcome-indeterminate-before-effect, and outcome-indeterminate-after-effect
faults. The selected manifest changes only at boundary 2. The model retains the
candidate separately so tests can distinguish candidate publication from
selection.

### Filesystem clean-close checkpoint candidate

For a recovery-selected checkpoint directory `checkpoint`, derive the
clean-close checkpoint directory as the sibling
`checkpoint.close-candidate`. It owns its own locked control file, stable
checkpoint file identity, persistent-WAL identity, and atomic `candidate` to
`current` replacement protocol.

The selected checkpoint source and clean-close candidate source never share
`current`, candidate files, publication faults, or load faults. Clean-close
publication may create, reconcile, replace, and load only the adjacent
clean-close directory. It never mutates the recovery-selected directory.

Before database acquisition, the adapter derives every selected path and
create, reclamation, manifest-close, and checkpoint-close candidate path. Any
lexical collision fails before a lock or child object is mutated.

### Filesystem manifest publication

For selected manifest path `manifest`, derive `manifest.close-candidate`.
Under retained database-wide and child locks, publication performs:

1. revalidate the exact source and target;
2. reconcile the stale close-candidate entry;
3. create and exclusively lock a new candidate file;
4. write the exact Manifest V2 frame;
5. synchronize the candidate descriptor;
6. atomically rename the candidate over the selected manifest;
7. verify that the retained candidate descriptor is the selected inode and
   decodes to the exact target;
8. synchronize the containing directory; and
9. only then replace the retained old manifest descriptor in memory and issue
   the exact receipt.

After candidate creation, both the old selected descriptor and the candidate
descriptor remain retained through every error. After rename, the candidate
descriptor remains locked even though its old pathname no longer exists.
Owners and locks are released only when the caller explicitly abandons or drops
the terminal result.

Filesystem fault injection covers candidate cleanup, create, write,
candidate synchronization, manifest replacement, selected verification, and
parent-directory synchronization with before, after, and
outcome-indeterminate timings. Low-level I/O errors are classified by the last
known completed boundary rather than collapsed into one generic error.

## Authority and Effect Ordering

The complete database close order is:

1. consume Live and revalidate the recovery-required source;
2. quiesce transaction state and publish/reload the disjoint transaction
   clean-close checkpoint candidate;
3. derive the certificate and exact adjacent clean target while retaining all
   owners;
4. consume ClosePending and reobserve the exact source;
5. write and synchronize a manifest close candidate;
6. atomically select that target;
7. verify exact selected bytes and identity;
8. synchronize the selected manifest's containing directory;
9. validate the private exact receipt; and
10. construct Closed while retaining every owner.

No operation releases database or child ownership before a terminal result.
No manifest selection begins before transaction clean-close proof succeeds.

## Tests

Repository-authored tests prove:

- empty and nonempty memory databases publish the exact clean target and retain
  ownership in Closed;
- every memory boundary/timing reports the required classification and never
  yields Closed on error;
- memory acquisition rejects a caller manifest that differs from the selected
  slot manifest;
- filesystem clean-close checkpoint publication and reload use only the
  physically disjoint adjacent directory;
- filesystem close publishes exact Manifest V2 bytes, retains locks through
  Closed, and leaves the recovery-selected checkpoint unchanged;
- filesystem faults before selection, at indeterminate replacement, after
  selection, and after the parent barrier preserve owners and report the exact
  durable-state classification; and
- malformed or colliding candidate paths fail before child mutation.

Compile-fail documentation rejects permit construction, receipt construction,
owner extraction, and stale same-owner retry.

## Evidence and Compatibility Boundary

All typestate rules, publication ordering, frame handling, filesystem effects,
fault points, and tests are repository-authored. No external product
documentation, SDK, driver, fixture, oracle, captured output, proprietary
governance tool, or native database/log format was consulted.

This transition makes no SQL Server close, checkpoint, recovery, LSN,
file-format, or protocol compatibility claim.

## Non-Goals

This decision does not:

- reopen a selected `Clean` Manifest V2 as a validated Closed database;
- publish the immediate recovery-required successor required before new Live
  work;
- select the clean-close checkpoint as restart authority;
- reclaim WAL;
- define subprocess lock inheritance; or
- add externally observable MSSQL compatibility behavior.

Until the clean-open phase lands, a successfully closed filesystem database
must remain represented by its returned Closed owner or be treated as requiring
the next lifecycle implementation before reusable open. The publication format
is nevertheless decoded by acquisition so a clean manifest is rejected by
lifecycle rather than by frame length.

## Consequences

ClosePending can now become Closed only after exact clean-manifest durability.
Every error preserves enough ownership and state classification to avoid a
false source-selected claim. Memory and filesystem adapters can execute the
same domain authority protocol while keeping the recovery-selected checkpoint
untouched.

The next #186 phase can validate a selected clean manifest and its certificate
against the dedicated clean-close checkpoint, construct reopened Closed
authority, and then publish the adjacent recovery-required generation required
before resuming Live work.
