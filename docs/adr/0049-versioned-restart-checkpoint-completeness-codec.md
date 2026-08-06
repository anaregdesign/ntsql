# ADR 0049: Versioned Restart Checkpoint Completeness Codec

- Status: Accepted
- Date: 2026-08-06
- Issue: #151
- Extends: ADR 0039, ADR 0044, ADR 0048
- Extended by: ADR 0050
- Follows: #149

## Context

ADR 0048 gives the restart-analyzed storage owner one authoritative,
persistent-lineage-bound `DurableTransactionRestartCheckpointCompletenessBaseline`
that nests a persistable transaction table, a strict-page-number page table,
and one replay-start candidate from a single ADR 0047 evidence window. No
reviewed byte format can yet carry that value across a process restart.

ADR 0044 already defines a reviewed, versioned, checksum-protected byte
boundary for the narrower transaction-only `DurableTransactionRestartCheckpointBaseline`.
Reusing, extending, or reinterpreting its `NTSQCKP1` bytes for the wider
completeness baseline would silently change already published bytes and
couple two independently evolving contracts: a future reader could no longer
tell which shape a blob claims to be without also inferring format history.

Before adding a completeness source, publisher, or startup consumer, the
filesystem-format adapter needs a second, completely independent pure-memory
codec. Encoding must accept only the authoritative completeness baseline;
decoding must return only an owned untrusted transaction/page/replay
observation and preserve semantically invalid raw values for a later
source-relative validator, mirroring ADR 0039's untrusted-observation
boundary and ADR 0044's structural/semantic split.

## Crate and Dependency Boundary

`ntsql-transaction` gains new private-field observation types beside the
existing ADR 0039 shapes. `ntsql-storage-file` gains a second pure codec
module beside ADR 0044's:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No crate, dependency edge, architecture registration, third-party dependency,
or domain I/O changes. The new codec module contains no `File`, `Path`,
source, publisher, lock, or storage-owner value in its public signatures, and
it does not touch ADR 0044's module, constants, or golden bytes.

The public operations are:

- `encode_restart_checkpoint_completeness_baseline(&DurableTransactionRestartCheckpointCompletenessBaseline)`;
  and
- `decode_restart_checkpoint_completeness_baseline(&[u8])`.

## Untrusted Completeness Observations

`ntsql-transaction` adds borrowed
`DurableTransactionRestartCheckpointCompletenessBaselineObservation<'evidence>`
and owned `OwnedDurableTransactionRestartCheckpointCompletenessBaselineObservation`.
Both nest the exact existing ADR 0039
`DurableTransactionRestartCheckpointBaselineObservation`/
`OwnedDurableTransactionRestartCheckpointBaselineObservation` for their
transaction fields, reusing persistent-ID, frontier, and entry decoding
unchanged rather than duplicating it.

Each also privately owns:

- a slice or vector of new
  `DurableTransactionRestartCheckpointCompletenessBaselinePageObservation`
  values, each independently retaining a raw `u64` page number, a raw page
  `..PageStateObservation` discriminant (`NoRequiredImage`/`StoreMissing`/
  `StoreCurrent`/`StoreBehind`), an `Option<..RequiredImageObservation>`, and
  an `Option<u64>` stored position; and
- one new `..ReplayObservation`, independently retaining a raw
  `..ReplayKindObservation` (`AfterFrontier`/`AtPosition`), an
  `Option<u64>` frontier, an `Option<u64>` position, and an
  `Option<..ReplayCauseObservation>`.

`..RequiredImageObservation` distinguishes `Raw { page_position }` from
`CommittedTransaction { epoch, sequence, page_position, commit_position }`
using raw `u64` fields, including zero; it does not reuse
`DurableTransactionIdentityObservation`, whose constructor rejects zero.
`..ReplayCauseObservation` distinguishes `StoreMissingPage { page_number }`,
`StoreBehindPage { page_number }`, and
`UncommittedTransaction { epoch, sequence }`, again with raw `u64` fields.

Every field on every new type is independent by construction: the page-state
discriminant does not derive from, and is not validated against, the optional
required-image or stored-position fields, and the replay kind does not derive
from, or get validated against, the optional frontier, position, or cause.
Structurally canonical but semantically contradictory decoded combinations
(for example `StoreCurrent` with an absent required image, or `AtPosition`
with an absent position) construct successfully and remain unchanged so a
later validator can reject them.

All constructors are infallible and allocate nothing beyond the caller-owned
slice or vector. `as_observation` borrows the owned shape through the
borrowed shape exactly like ADR 0039. Compile-fail tests on both new types
reject conversion into the authoritative completeness baseline, active
transaction state, `DirtyPage`, `PageWritePermit`,
`CommittedTransactionPageRecoveryWritePermit`, `RecoveredTransactionPageStorage`,
`RestartAnalyzedTransactionPageStorage`, the transaction-baseline publication
receipt, `LogDurability`, and `LogSequenceNumber`; narrower compile-fail tests
on the page-state, required-image, page, replay-cause, replay-kind, and
replay observation types reject conversion into their authoritative
counterparts. No validator, port, or I/O is added by this ADR.

## Independent Version 1 Namespace

`ntsql-storage-file` adds `restart_checkpoint_completeness_codec`, a second
pure-memory module beside ADR 0044's `restart_checkpoint_codec`. It has no
shared magic, footer, geometry constant, or version dispatch with `NTSQCKP1`/
`NTSQCKE1`; the two codecs cannot misinterpret each other's bytes.

The fixed 128-byte header is:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | completeness magic, ASCII `NTSQCMP1` |
| 8 | 2 | format version, exactly 1 |
| 10 | 2 | header length, exactly 128 |
| 12 | 2 | transaction-entry length, exactly 64 |
| 14 | 2 | page-entry length, exactly 64 |
| 16 | 2 | footer length, exactly 16 |
| 18 | 6 | reserved zero bytes |
| 24 | 16 | raw persistent log ID |
| 40 | 8 | durable-frontier payload |
| 48 | 1 | frontier presence: 0 absent, 1 present |
| 49 | 7 | reserved zero bytes |
| 56 | 8 | transaction-entry count |
| 64 | 8 | page-entry count |
| 72 | 8 | total blob length |
| 80 | 1 | replay kind: 0 after-frontier, 1 at-position |
| 81 | 1 | replay-frontier presence: 0 absent, 1 present |
| 82 | 1 | replay-position presence: 0 absent, 1 present |
| 83 | 1 | replay-cause: 0 absent, 1 store-missing, 2 store-behind, 3 uncommitted |
| 84 | 4 | reserved zero bytes |
| 88 | 8 | replay-frontier payload |
| 96 | 8 | replay-position payload |
| 104 | 8 | replay-cause page-number payload |
| 112 | 8 | replay-cause epoch payload |
| 120 | 8 | replay-cause sequence payload |

The header is followed by exactly the declared number of fixed 64-byte
transaction entries, then exactly the declared number of fixed 64-byte page
entries. The final 16-byte footer is:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | completeness footer magic, ASCII `NTSQCME1` |
| 8 | 8 | checksum of every preceding byte, including footer magic |

The total length is exactly:

```text
128 + transaction_count * 64 + page_count * 64 + 16
```

Transaction/page count-to-`u64`, count multiplication, and length addition
are all checked. The declared total must equal that geometry and the
supplied slice length exactly, matching ADR 0044's truncation/trailing-data
rules.

## Fixed Transaction Entry

Each 64-byte transaction entry keeps ADR 0044's exact field layout — epoch,
sequence, optional first/last owned-page position, owned-page record count,
commit-position payload, state discriminant, position-presence bytes, and
reserved bytes at the same offsets — as an independent copy inside this
blob. It is not the same bytes as an ADR 0044 checkpoint; it is a
byte-for-byte compatible layout chosen because reinventing an equivalent
shape would add no value and would only invite drift.

## Fixed Page Entry

Each 64-byte page entry is:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | raw page number |
| 8 | 1 | page state: 0 no-required-image, 1 store-missing, 2 store-current, 3 store-behind |
| 9 | 1 | required-image presence: 0 absent, 1 present |
| 10 | 1 | required-image kind: 0 raw, 1 committed transaction |
| 11 | 1 | stored-position presence: 0 absent, 1 present |
| 12 | 4 | reserved zero bytes |
| 16 | 8 | required-image page-position payload |
| 24 | 8 | required-image epoch payload |
| 32 | 8 | required-image sequence payload |
| 40 | 8 | required-image commit-position payload |
| 48 | 8 | stored-position payload |
| 56 | 8 | reserved zero bytes |

The page-state discriminant, required-image presence/kind, and
stored-position presence are three independent structural fields. The codec
does not require the discriminant to agree with which optional fields are
present; that agreement remains later validator work. An absent required
image must carry a zero kind byte and zero page-position/epoch/sequence/
commit-position payloads. A present raw-kind required image must carry zero
epoch/sequence/commit-position payloads; its page-position payload is free,
including zero. A present committed-transaction-kind required image leaves
all four payloads free, including zero. An absent stored position must
carry a zero payload.

## Independent Replay Fields

The header's replay kind, frontier presence/payload, position
presence/payload, and cause discriminant/payloads are five independent
structural groups, matching the page entry's independence. An absent replay
frontier or position must carry a zero payload. An absent replay cause must
carry zero page-number, epoch, and sequence payloads. A present store-missing
or store-behind cause must carry a zero epoch and zero sequence payload; its
page-number payload is free, including zero. A present uncommitted-transaction
cause must carry a zero page-number payload; its epoch and sequence payloads
are free, including zero.

## Complete-Blob Checksum

Version 1 reuses the exact same repository-owned checksum arithmetic as ADR
0044 and ADR 0013, applied to this blob's own protected bytes: the complete
header, every complete transaction entry, every complete page entry, and the
eight-byte footer magic, excluding the final eight checksum bytes. Reuse is
limited to the arithmetic; this blob's checksum is never compared against, or
substituted for, an ADR 0044 checksum.

## Fallible Authoritative Encoding

The public encoder accepts only an authoritative
`DurableTransactionRestartCheckpointCompletenessBaseline`. It converts the
exact transaction and page counts to `u64`, checks fixed-width length
arithmetic across both counts, converts the complete length to the format
field, performs one `try_reserve_exact` for the complete output before
writing any output bytes, writes the header and every entry without another
allocation, and appends the footer magic and checksum. Typed errors
distinguish transaction-count, page-count, and length representation/overflow
from output capacity exhaustion. No partial byte vector is returned.

## Structurally Non-Authorizing Decoding

The decoder validates, in order: minimum header size; header magic, version,
and fixed geometry (header/transaction-entry/page-entry/footer lengths);
checked expected length from both counts; declared versus expected length;
exact supplied length; footer magic; the complete-blob checksum; every
reserved byte; every discriminant and presence bit; and every canonical-zero
payload rule above. Only after all of that does it reserve the exact
transaction and page counts and construct the owned result.

It never sorts, deduplicates, infers a missing value, compares page or
transaction relationships, or calls `PageNumber::new`,
`PersistentLogId::new`, or `DurableTransactionIdentityObservation::new`.
Zero identities, zero page numbers, contradictory state/optional-field
combinations, duplicate page numbers, and unsorted entries all decode
successfully as untrusted evidence for a later validator.

## Error and Authority Boundary

`RestartCheckpointCompletenessBaselineEncodeError` and
`RestartCheckpointCompletenessBaselineDecodeError` retain exact counts,
lengths, offsets, discriminants, payloads, magic, or checksum values for
their stage, with full `Display` and `Error` implementations and no
fabricated nested source, matching ADR 0044.

The input baseline, encoded bytes, decoded observations, and codec errors
cannot create or satisfy the same authority list ADR 0044 and ADR 0048
already exclude: transaction lifecycle or coordinator state; WAL append,
flush, restart-analysis, or lineage authority; page-store or committed-page
recovery write authority; recovered or restart-analyzed storage ownership;
checkpoint publication, source selection, or a success receipt; dirty-page
tables, replay execution, redo, undo, rollback, or compensation; and
retention floors, truncation, compaction, or reclamation. Compile-fail tests
on `encode_restart_checkpoint_completeness_baseline` and
`decode_restart_checkpoint_completeness_baseline` reject untrusted encoder
input and decoded conversion to each of those.

## Evidence and Compatibility Boundary

The format, checksum reuse, values, and errors are repository-authored. No
external product documentation, driver, SDK, fixture, oracle, proprietary
governance tool, or native MDF/NDF/LDF/BAK format is consulted. These bytes
define no SQL Server checkpoint record, transaction/page table, LSN, recovery
phase, corruption response, diagnostic, or compatibility result.

## Test Boundaries

- An authoritative empty baseline (no transactions, no pages, `AfterFrontier
  { None }`) has exact complete golden bytes; repeated encoding is
  deterministic.
- A real authoritative round trip covers committed and uncommitted
  transactions, all four page states, raw and committed-transaction required
  images, and is confirmed untrusted (the decoded observation exposes only
  accessors, never an authoritative conversion).
- Separate authoritative round trips exercise the `StoreBehindPage` and
  `UncommittedTransaction` replay causes alone, alongside the primary
  scenario's `StoreMissingPage` cause.
- Every prefix shorter than one complete valid blob returns truncation; one
  additional byte returns trailing data.
- Header/footer magic, version, each geometry field, declared length, and
  checksum fail distinctly.
- Every discriminant (frontier presence, transaction state/position
  presence, page state, required-image presence/kind, stored-position
  presence, replay kind/frontier-presence/position-presence/cause), every
  canonical-zero absent/raw-kind/unused-cause payload, and every reserved
  byte fails distinctly.
- Structurally canonical but semantically contradictory raw fields (zero
  identities, a page state whose optional fields disagree with its
  discriminant, a replay kind whose optional fields disagree with its
  discriminant) decode unchanged.
- Impossible transaction/page count and synthetic capacity failures retain
  typed errors with `Display`/`Error` coverage.
- Existing ADR 0044 golden-byte, structural, and non-regression tests remain
  unchanged and pass.

## Non-Goals

This ADR does not:

- create, open, read, write, synchronize, rename, or remove a filesystem
  path;
- implement a completeness checkpoint source, publisher, or startup
  consumer;
- change ADR 0044's bytes, module, constants, or public API;
- add source-relative semantic validation for the completeness baseline;
- add checkpoint generations, selection, fallback, repair, or retention;
- add a startup checkpoint gate, replay execution, redo, undo, or page
  repair;
- truncate, compact, or reclaim a WAL; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql now has a reviewed, versioned, integrity-checked, and completely
independent byte boundary for the transaction/page/replay restart checkpoint
completeness baseline. Encoding requires the authoritative ADR 0048 value;
decoding returns only a new untrusted observation that nests ADR 0039's
existing transaction shape and adds independent raw page and replay fields.

A completeness source, publisher, and any startup consumption of decoded
completeness fields remain blocked on a separately reviewed source-relative
validator — mirroring ADR 0039's role for the transaction-only baseline —
plus atomic filesystem publication and startup-ownership decisions.
