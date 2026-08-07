# ADR 0033: Recovery-Gated Storage Ownership

- Status: Accepted
- Date: 2026-08-06
- Issue: #118
- Extends: ADR 0029, ADR 0031, ADR 0032
- Extended by: ADR 0034, ADR 0037, ADR 0045, ADR 0053, ADR 0055, ADR 0061

## Context

ADR 0032 defines a deterministic complete-inventory recovery operation, but a
caller that opens a WAL and page store separately still owns both adapters
before invoking it. Nothing in that call shape distinguishes a store that has
completed startup recovery from one that has not. An outer component could
forget recovery, inspect stale pages, or continue after a partial failure.

The next boundary must make recovery a consuming ownership transition. It must
retain both adapters and their locks after failure so the only continuation is a
fresh complete batch, while exposing live storage only after success. The
filesystem composition path must also acquire the pair in one documented order
and release the first lock if the second open fails.

This decision adds those ownership states and one filesystem pair opener. It
does not make two files atomic or remove the low-level adapter entrypoints used
by adapter tests and separately reviewed composition.

## Crate and Dependency Boundary

`ntsql-transaction` owns the adapter-neutral startup typestate because it
controls the transition through ADR 0032 and must not depend on a concrete
adapter. `ntsql-storage-file` owns only the fixed-order physical open:

```text
ntsql-transaction -> ntsql-page -> ntsql-wal
ntsql-transaction ---------------> ntsql-wal

ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

The memory adapter exercises the generic states directly. No crate, dependency
edge, persistent format, or architecture registration changes.

## Unrecovered Ownership State

`UnrecoveredTransactionPageStorage<Source, Store, N>` privately owns one source
that implements both ADR 0032 inventory and ADR 0029 stable evidence, plus one
ADR 0029 recovery store.

Construction consumes both values. The unrecovered state exposes:

- no source or store reference;
- no mutable parts;
- no `into_parts` escape;
- no page operation;
- no candidate or permit; and
- no recovered-state constructor.

Its original authority-bearing operation is consuming `recover`. ADR 0053 adds
one separately bounded consuming completeness-checkpoint selection before that
operation, but it exposes neither adapter and every decline, absence, or
rejection fallback delegates back to this exact `recover` transition. Dropping
either owner simply drops its retained adapters. Compile-fail coverage makes
unavailable accessors load-bearing outside the owning crate.

ADR 0055 later adds a read-only preparation branch after selected replay
planning. Prepared and failed states still expose neither adapter nor recovery
authority. Their explicit fallbacks destroy all private repair evidence before
delegating to this same complete `recover` transition.

The generic state cannot prevent a caller from deliberately bypassing it and
using the existing low-level adapter constructors. It is the reviewed
composition boundary for startup code, not a claim that all independently
opened adapter values are globally inaccessible.

## Consuming Recovery Transition

`recover(self)` invokes `recover_committed_transaction_pages` exactly once over
the privately owned pair.

On success it returns
`RecoveredTransactionPageStorage<Source, Store, N>`, which retains:

- the exact complete ordered ADR 0032 report; and
- both source and store values under the recovered typestate.

Only this state exposes shared parts, mutable parts, and consuming `into_parts`.
No separate boolean, global flag, baseline fallback, or target-specific branch
can mark storage recovered.

The report is immutable evidence of the startup transition. It is not a
continuing assertion that WAL and page store remain current after live access is
released.

## Failed Ownership and Fresh Retry

On failure, the transition returns
`FailedTransactionPageStorageRecovery<Source, Store, N>`. It privately retains:

- the same source;
- the same store;
- their existing lifetime locks where applicable; and
- the exact ADR 0032 lineage, inventory, ordering, capacity, or page error.

The error accessor exposes only the inert nested error. Manual `Debug` output
does not reveal or require formatting either adapter. `Display` and
`Error::source` preserve the complete batch cause chain.

The failed state has no source/store access or `into_parts`. It may be dropped,
which releases both adapters, or consumed by `retry`.

`retry(self)` discards no physical state and grants no one-page continuation. It
calls the unrecovered state's complete transition again, causing ADR 0032 to:

1. recheck lineage;
2. obtain a new complete inventory;
3. reserve a new result vector;
4. start again at the first page; and
5. re-enter ADR 0029 independently for every page.

Earlier durable writes therefore resolve from authoritative current state,
normally as `AlreadyCurrent`, before the formerly failing page is reached. The
completed prefix in the old error remains diagnostic data and is never used as
a resume cursor.

## Filesystem Pair Open

`open_transaction_page_storage` is the filesystem composition entrypoint. It
performs:

1. `FileCommitLog::open_transaction_page_capable` for WAL v3;
2. `FilePageStore::open` for page-store v1; and
3. construction of an unrecovered owner.

The successful output is
`UnrecoveredFileTransactionPageStorage<N>`, an alias of the generic domain
state. Neither raw adapter is returned.

`FileTransactionPageStorageOpenError` distinguishes commit-log and page-store
stages while preserving the exact nested `FileOpenError` or
`PageStoreOpenError`.

If the first stage fails, the page-store path is not opened. If the second stage
fails, normal ownership drop releases the already-opened WAL and its advisory
lock before the error reaches the caller. A test immediately reopens that WAL,
making release rather than eventual process teardown observable.

The order is fixed as WAL then page store. The function does not wait, retry,
open both atomically, acquire a database-wide lock, or return a partially opened
pair. Existing nonblocking advisory lock and hostile-writer limitations from
ADR 0031 remain unchanged.

## Lock Lifetime Through Recovery

Both file adapters retain their exclusive advisory locks for their complete
value lifetimes. Moving them into unrecovered, failed, or recovered wrappers
does not close, reopen, clone, or unlock either file descriptor.

Consequently:

- the WAL lock remains held from first-stage open through inventory, every
  evidence callback, failure inspection, and retry;
- the page-store lock remains held from second-stage open through observation,
  exact-source replacement, failure inspection, and retry; and
- successful live access receives the same continuously locked adapter values.

There is still no atomic two-file acquisition. Another process may acquire the
page-store lock between WAL-open failure and a later caller retry, or vice versa
for independently used low-level APIs. Fixed nonblocking order prevents this
entrypoint from waiting in a lock cycle but does not establish a global protocol
for other code.

## Startup Report and Later WAL Advancement

The recovered report describes exactly the durable prefix observed during its
transition. Once mutable parts are released, normal live appends and flushes may
advance the durable frontier.

In particular, a later successful flush through a greater position also makes
any physically complete contiguous suffix before that position durable. A page
and commit that were outside the startup inventory can therefore become durable
after the transition. The old report does not classify that later prefix and
must not be reused as recovery or store-currency evidence.

This is ordinary WAL frontier behavior, not a startup-state failure. A later
crash or reopen must enter a new unrecovered owner and run a fresh complete
batch. The recovered typestate proves that one startup transition completed; it
does not freeze storage for the remainder of the process.

## Concrete Scenarios

The domain fake scenario fails at the second of three sorted pages after the
first replacement. The failed owner retains an exact one-page completed prefix
and nested write state. Consuming retry re-inventories, observes the first page
as current, reaches the formerly failing page, completes the third page, and
only then releases both fake adapters.

The real memory and filesystem scenarios contain exact, missing,
behind/lower-version, uncommitted-only, raw-only, and fully volatile suffix
records. A page-store fault after the exact first page produces a failed owner.
Fresh retry completes with the exact sorted report while preserving the prior
durable prefix.

After memory success, mutable recovered parts perform a normal live WAL/page
store operation. Filesystem coverage separately opens a missing committed page
through the pair entrypoint, recovers it, performs a normal committed live
flush, drops the recovered owner, reopens the pair, and observes both pages as
`AlreadyCurrent` with exact persisted bytes.

## Authority and Error Boundary

Unrecovered and failed wrappers own adapters but expose no operation that can
construct or substitute:

- a transaction lifecycle token;
- a dirty page;
- a live `PageWritePermit`;
- an ADR 0029 recovery permit;
- a stable-prefix callback;
- a one-page retry; or
- a recovered state.

The recovered wrapper does not create new domain authority. It only releases
the original adapters after ADR 0032 reports complete success. Subsequent live
operations must still obtain their normal transaction and page permits.

Open errors and failed recovery states are not client diagnostics. Filesystem
paths, I/O stages, lock failures, nested adapter causes, and batch details remain
outside `ClientDiagnostic`.

## Evidence and Compatibility Boundary

All behavior uses repository-authored WAL, page-store, transaction, recovery,
lock, and deterministic-fault contracts. No external product documentation,
driver, SDK, fixture, oracle, proprietary governance tool, or native
MDF/NDF/LDF/BAK format is consulted.

This decision defines no SQL Server database-open sequence, startup state,
recovery ownership, locking, LSN, error, diagnostic, or compatibility behavior.

## Test Boundaries

- Compile-fail tests reject adapter access from unrecovered and failed states and
  direct recovered-state construction.
- Fake recovery proves exact failure retention, cause chaining, full restart on
  retry, and adapter release only after success.
- Memory recovery uses the owning failure/retry transition and then performs a
  live operation through mutable recovered parts.
- Filesystem recovery uses the owning failure/retry transition without releasing
  either locked adapter.
- Commit-log open failure is distinguished before page-store open.
- Page-store open failure is distinguished and releases the first WAL lock
  before return.
- The filesystem pair opener recovers a missing page, releases live parts,
  persists another normal committed page, and reopens both as exact current
  state.
- Existing direct ADR 0029/0032 tests, individual adapter APIs, poison behavior,
  tail repair, and lock tests remain valid.
- Architecture validation proves the dependency graph did not change.

## Non-Goals

This ADR does not:

- remove or restrict low-level individual adapter constructors and openers;
- create/open/drop a database directory or assign paths;
- make two-file open, recovery, or live operations atomic;
- add a database-wide lock, lock registry, wait protocol, or multi-process
  startup coordinator;
- add checkpoints, analysis tables, dirty-page tables, transaction tables, redo,
  undo, rollback, abort, compensation, or log truncation;
- freeze the WAL or make a startup report authoritative after live mutation;
- change raw, uncommitted, or store-only page policy;
- change WAL v1/v2/v3 or page-store v1 bytes, markers, checksums, synchronization,
  poison, repair, or reopen behavior; or
- define external SQL Server values or native file compatibility.

## Consequences

Reviewed startup code can now own a WAL/page-store pair without exposing either
until deterministic committed-page recovery completes. Partial failure retains
both locked adapters and exact diagnostics, and the only continuation is a fresh
whole-batch retry.

ADR 0037 narrows adapter release so this page-recovered owner must also complete
durable restart analysis before live use. The resulting analyzed owner is a
suitable composition boundary for separately reviewed checkpoint metadata.
Database lifecycle, global startup exclusion, log truncation, dirty-page
analysis, replay, and undo remain future work.
