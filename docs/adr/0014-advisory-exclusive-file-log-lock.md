# ADR 0014: Advisory Exclusive File-Log Lock

- Status: Accepted
- Date: 2026-08-05
- Issue: #78
- Extends: ADR 0001, ADR 0013
- Extended by: ADR 0017, ADR 0018, ADR 0022, ADR 0031, ADR 0036

## Context

ADR 0013 required the caller to establish exclusive single-writer ownership of
one transaction commit-log path. Two cooperating processes could still open the
same inode, independently validate the same high-water marks, and append
conflicting epochs, positions, or frames. Checksums detect some resulting
corruption only after the unsafe concurrent mutation has occurred.

Rust 1.97 exposes standard-library file locking, so cooperative exclusion does
not require an external dependency, unsafe platform call, sidecar lock file,
process ID, clock, or stale-lock recovery policy.

## Decision

`FileCommitLog::create_new` and `FileCommitLog::open` call `File::try_lock`
immediately after obtaining the read/write file descriptor. The lock is:

- exclusive, so another cooperating reader/writer adapter cannot enter;
- nonblocking, so contention returns immediately instead of hanging startup;
- acquired before header write, file synchronization, metadata inspection,
  validation, tail repair, epoch allocation, append, or recovery access; and
- retained by the owned `File` for the complete adapter lifetime.

Dropping the adapter closes the file and releases the operating-system lock.
There is no explicit unlock path that could expose a still-live unlocked
adapter.

Lock failure is preserved as an internal `FileIoError` at
`FileIoStage::AcquireExclusiveLock`. Create/open returns that typed error and
never falls back to unlocked operation. If a platform or filesystem does not
support the standard operation, its original I/O cause is returned at the same
stage.

Create-new obtains the inode before it can lock it. A lock failure may therefore
leave a new empty path requiring explicit reconciliation, just as a later header
or directory-sync failure may leave a partial creation. It is never reported as
successful creation.

## Advisory and Path Boundary

The lock is advisory, not authorization or mandatory access control. A malicious
or non-cooperating process can ignore it, use another API, mutate the file, or
replace path components. The composition root must still supply a trusted path,
unique stable lineage ID, appropriate filesystem, and a policy that all writers
cooperate with this lock.

The lock follows the opened file identity. A cooperating open through a hard-link
alias therefore contends on platforms where the standard filesystem lock and
hard links are supported. This ADR makes no claim for network filesystems or
platforms whose lock semantics do not provide that behavior; failure is explicit.

## Mutation and Recovery Ordering

An existing-file lock failure occurs before `sync_all`, parsing, or incomplete
tail truncation. Contention therefore cannot cause validation, repair, or any
other file mutation. After lock acquisition, all ADR 0013 format, barrier,
poison, and authoritative recovery rules remain unchanged.

## Compatibility Boundary

Lock stages, contention errors, and cooperative exclusion are ntsql-internal
operational behavior. They define no SQL Server database lock, diagnostic,
startup behavior, file-sharing mode, or compatibility status.

## Test Boundaries

- A second open of one path fails at the acquire-lock stage while the first
  adapter lives.
- Bytes including an externally added incomplete tail remain unchanged after the
  contending open, proving lock failure precedes repair.
- Open succeeds and repairs that tail after the first adapter is dropped.
- On supported Unix test platforms, a hard-link alias cannot bypass the lock and
  becomes openable after release.
- Existing format, corruption, epoch, commit, barrier, fault, recovery, poison,
  high-water, and lineage tests remain valid.

## Consequences

Cooperating `FileCommitLog` instances no longer rely solely on caller discipline
for single-writer exclusion and fail fast on contention. Mandatory exclusion,
secure path traversal, multi-file database locking, lock-owner diagnostics, and
distributed/network-filesystem leases remain future composition and storage
work.
