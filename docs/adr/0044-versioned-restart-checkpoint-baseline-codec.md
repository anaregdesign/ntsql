# ADR 0044: Versioned Restart Checkpoint Baseline Codec

- Status: Accepted
- Date: 2026-08-06
- Issue: #140
- Extends: ADR 0043
- Extended by: ADR 0045

## Context

ADR 0043 completes the deterministic memory implementation of the transaction
restart checkpoint baseline source and publisher. It identifies reviewed bytes
and integrity protection as the first blockers for a persistent adapter.

The transaction domain already owns both sides of the authority boundary:

- `DurableTransactionRestartCheckpointBaseline` is the private-field,
  authoritative encoder input prepared by a restart-analyzed owner; and
- `OwnedDurableTransactionRestartCheckpointBaselineObservation` is the complete
  owned but untrusted decoder output accepted by ADR 0039 validation.

The next slice defines bytes between those types without adding a file,
publication, selection, or recovery operation.

## Crate and Dependency Boundary

`ntsql-storage-file` owns the pure codec because it already owns repository
filesystem format bytes and depends inward on the transaction domain:

```text
ntsql-storage-file -> ntsql-page
ntsql-storage-file -> ntsql-transaction
ntsql-storage-file -> ntsql-wal
```

No crate, dependency edge, architecture registration, third-party dependency, or
domain I/O changes. The codec module contains no `File`, `Path`, source,
publisher, lock, or storage-owner value in its public signatures.

The public operations are:

- `encode_restart_checkpoint_baseline(&DurableTransactionRestartCheckpointBaseline)`;
  and
- `decode_restart_checkpoint_baseline(&[u8])`.

Encoding and decoding are pure memory transformations. Their placement in an
I/O-capable outer adapter does not make either operation a filesystem effect.

## Independent Version 1 Namespace

The checkpoint blob has an independent namespace. It does not reuse WAL
`NTSQLOG1`/`NTSQ`, page-store `NTSQPGS1`/`NTSP`, or either format's version
dispatch. All multibyte integers are unsigned big-endian values.

The fixed 64-byte header is:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | checkpoint magic, ASCII `NTSQCKP1` |
| 8 | 2 | format version, exactly 1 |
| 10 | 2 | header length, exactly 64 |
| 12 | 2 | transaction-entry length, exactly 64 |
| 14 | 2 | footer length, exactly 16 |
| 16 | 16 | raw persistent log ID |
| 32 | 8 | durable-frontier payload |
| 40 | 1 | frontier presence: 0 absent, 1 present |
| 41 | 7 | reserved zero bytes |
| 48 | 8 | transaction-entry count |
| 56 | 8 | total blob length |

The raw persistent ID is not required to be nonzero by the codec. An absent
frontier must have a zero payload. A present frontier may contain zero because
semantic invalidity remains ADR 0039's responsibility.

The header is followed by exactly the declared number of fixed 64-byte entries.
The final 16-byte footer is:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | checkpoint footer magic, ASCII `NTSQCKE1` |
| 8 | 8 | checksum of every preceding byte, including footer magic |

The total length is exactly:

```text
64 + transaction_count * 64 + 16
```

Count multiplication and length addition are checked. The declared total must
equal that geometry and the supplied slice length exactly. Short input is
truncated; additional input is trailing data rather than another concatenated
blob.

## Fixed Transaction Entry

Each 64-byte transaction entry is:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | raw coordinator epoch |
| 8 | 8 | raw coordinator-local sequence |
| 16 | 8 | first owned-page-position payload |
| 24 | 8 | last owned-page-position payload |
| 32 | 8 | raw owned-page-record count |
| 40 | 8 | commit-position payload |
| 48 | 1 | state: 0 uncommitted, 1 committed |
| 49 | 1 | first-position presence: 0 absent, 1 present |
| 50 | 1 | last-position presence: 0 absent, 1 present |
| 51 | 13 | reserved zero bytes |

An absent optional position must carry a zero payload. A present position may be
zero and is retained. An uncommitted entry must carry a zero commit payload. A
committed entry may carry a zero position so current-WAL validation, not the
codec, reports that semantic defect.

Every entry remains in its supplied order. The decoder does not sort,
deduplicate, normalize, validate identity order, infer missing positions, compare
ranges, or reconcile state with counts.

## Complete-Blob Checksum

Version 1 reuses the repository-owned deterministic checksum algorithm from ADR
0013. Reuse is limited to the arithmetic:

1. initialize state to `0x4e5453514c434b31`;
2. for each protected byte, XOR its `u64` value into state;
3. wrapping-multiply by `0x4e5453514c57414d`;
4. rotate left seven bits and XOR `0x434845434b53554d`; and
5. XOR the wrapping protected-byte count after the fold.

The protected bytes are the complete header, every complete entry, and the
eight-byte footer magic. The final eight checksum bytes are excluded.

This is deterministic corruption detection, not authentication, malicious-write
defense, collision resistance, or a substitute for trusted paths and locks.
Changing magic, geometry, canonical fields, or checksum arithmetic requires a
new checkpoint format version.

## Fallible Authoritative Encoding

The public encoder accepts only an authoritative
`DurableTransactionRestartCheckpointBaseline`. It:

1. converts the exact transaction count to `u64`;
2. checks fixed-width length arithmetic;
3. converts the complete length to the format field;
4. performs one `try_reserve_exact` for the complete output before writing any
   output bytes;
5. writes the header and each entry without another allocation; and
6. appends footer magic and the complete-blob checksum.

Typed errors distinguish count representation, host length overflow, format
length representation, and output capacity exhaustion. No partial byte vector
is returned.

The encoder maps authoritative committed and uncommitted variants structurally.
It does not expose or weaken baseline construction.

## Structural, Non-Authorizing Decoding

The decoder first validates enough outer framing to determine one exact blob:

1. minimum header size;
2. header magic, version, and fixed geometry;
3. count and checked expected length;
4. declared versus expected length;
5. exact supplied length with no trailing bytes;
6. footer magic; and
7. complete-blob checksum.

It then validates canonical structural fields: presence/state discriminants,
zero absent payloads, zero uncommitted commit payloads, and all reserved bytes.
Only after outer length and checksum validation does it reserve the exact entry
count and construct the owned result.

The output retains raw:

- `u128` persistent ID;
- absent or present numeric frontier;
- entry order;
- epoch and sequence;
- absent or present first/last page positions;
- record count; and
- uncommitted or committed state and numeric commit position.

Zero IDs, present-zero values, zero identities, contradictory ranges or counts,
duplicates, unsorted entries, and invalid transaction relationships remain
untrusted decoded evidence. The codec does not call `PersistentLogId::new` and
cannot return `DurableTransactionRestartCheckpointBaseline`.

Such structurally valid but semantically invalid bytes decode successfully and
then fail through ADR 0039's exact current-WAL validation. Neither a valid
checksum nor successful decode grants source-relative validity.

## Error and Authority Boundary

`RestartCheckpointBaselineEncodeError` and
`RestartCheckpointBaselineDecodeError` retain exact counts, lengths, offsets,
discriminants, payloads, magic, or checksum values for their stage. They have no
fabricated nested source because this codec performs no I/O.

The input baseline, encoded bytes, decoded observation, and codec errors cannot
create or satisfy:

- transaction lifecycle or coordinator state;
- WAL append, flush, restart-analysis, or lineage authority;
- page-store or committed-page recovery write authority;
- recovered or restart-analyzed storage ownership;
- checkpoint publication, source selection, or a success receipt;
- dirty-page tables, replay starts, redo, undo, rollback, or compensation; or
- retention floors, truncation, compaction, or reclamation.

Compile-fail tests reject untrusted encoder input and decoded conversion to an
authoritative baseline, active transaction, page permit, WAL, recovery store, or
restart-analyzed owner. Existing observation compile-fail tests independently
retain recovered-owner and log-position boundaries.

## Evidence and Compatibility Boundary

The format, checksum, values, and errors are repository-authored. No external
product documentation, driver, SDK, fixture, oracle, proprietary governance
tool, or native MDF/NDF/LDF/BAK format is consulted.

These bytes define no SQL Server checkpoint record, transaction table, LSN,
recovery phase, corruption response, diagnostic, or compatibility result.

## Test Boundaries

- An authoritative empty baseline has exact complete golden bytes and checksum;
  repeated encoding is deterministic.
- A nonempty authoritative baseline containing committed and uncommitted entries
  round-trips every field and preserves strict entry order.
- Both authoritative round trips remain untrusted until exact real-owner
  current-prefix validation.
- `None` and `Some(0)` survive distinctly.
- Structurally canonical zero and contradictory raw fields decode unchanged.
- Every prefix shorter than one complete valid blob returns truncation, and one
  additional byte returns trailing data.
- Header/footer magic, version, each geometry field, declared length, checksum,
  each presence/state discriminant, absent payloads, uncommitted commit payload,
  and header/entry reserved bytes fail distinctly.
- Impossible count/length and synthetic capacity failures retain typed errors.
- A valid-checksum blob with a zero persistent ID decodes, then fails only at the
  current-WAL validation boundary.
- Existing WAL, page-store, restart, recovery, checkpoint, architecture, and
  governance tests remain valid.

## Non-Goals

This ADR does not:

- create, open, read, write, synchronize, rename, or remove a filesystem path;
- implement a checkpoint source or publisher;
- claim path replacement or cross-process atomicity;
- acquire a lock or define global lock order;
- add checkpoint generations, selection, fallback, repair, or retention;
- authorize retry or resolution after indeterminate publication;
- add a dirty-page table, replay start, redo, undo, or startup checkpoint gate;
- truncate, compact, or reclaim a WAL; or
- define external SQL Server behavior or native file compatibility.

## Consequences

ntsql now has a reviewed, versioned, integrity-checked byte boundary for the
transaction restart checkpoint baseline. Encoding requires authoritative input;
decoding returns only the existing untrusted shape.

A filesystem adapter remains blocked on separately reviewed path ownership,
create/open behavior, locking, atomic replacement, synchronization, fault
effects, and source error semantics. The transaction-only blob also remains
insufficient for checkpoint-based replay or WAL reclamation until a separately
reviewed dirty-page/replay-start boundary exists.
