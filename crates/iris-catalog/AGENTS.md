# AGENTS.md — iris-catalog

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The schema and the handle contract are fixed by `docs/design/phase2-interfaces.md`; a
  schema change needs a migration step keyed on `schema_version` and an update to that
  document and to `docs/research/04-storage-format.md §6`.
- Command ordering: changes and queries are one FIFO; `flush` runs before every query so a
  reader observes earlier notifications. Do not add a second channel or a fast path that
  bypasses the queue.
- Admission counts commands and their estimated owned input until completion, capped at 2,048
  commands and 16 MiB. Segment open/seal transitions reserve 128 commands and 1 MiB; exhaustion
  sets `reconciliation_required`. Events reject under overload, progress coalesces only with a
  same-path command in the unqueried tail, eviction never crosses the last query barrier, and
  queries return `CatalogError::Overloaded`.
- `notify` never blocks. A rejected event/progress update increments its rejection counter;
  rejecting a lifecycle transition also emits an error requiring startup reconciliation.
- Pragmas (WAL, confirmed at open, `synchronous = NORMAL`, foreign keys, a busy timeout set
  before the first read, in-memory temporary query state) implement `04 §6` without requiring
  a sandbox-external temp directory; deletion intent and recording-root registration commit
  under scoped `synchronous = FULL` before filesystem mutation or writer startup. Every write
  goes through `write_transaction` (`BEGIN IMMEDIATE`), so the busy timeout covers lock
  acquisition; migration rereads the schema version under that lock.
- `Db::open` creates missing parents 0700 and a missing database file 0600 before SQLite
  opens it; SQLite copies the database file's mode to the WAL and shared-memory files.
- Retention atomically rechecks active same-camera event protection and commits `deleting`
  under FULL synchronization, through one shared predicate (`protection_predicate`) that the
  candidate pages use too. Filesystem mutation follows only `DeleteMarkResult::Marked`.
  Deleting rows are absent from serving and candidate queries, remain visible to
  `deleting_segments_page`, and contribute to byte totals until durable pair deletion removes
  them; `CatalogStats::deleting_segments` and `deleting_bytes` count them. The daemon retries
  them in every retention pass and at startup. `delete_segment` removes only a row still in
  deletion intent.
- `SegmentOpened` and `adopt_segment` are upserts by path: a row the path already has is reset,
  deletion intent included. Startup finishes deletion intents before it adopts.
- `mark_empty_writing_deleting` marks any open, non-deleting row whatever its fragment count
  (startup calls it for a pair that is empty or confirmed absent); keep its name and
  signature, which startup reconciliation depends on.
- Range listings (`segments_limited`) list an unsealed row only with `fragments > 0` and
  extend it to `MIN(now, wall_end_us + grace)`, the grace being `OPEN_SEGMENT_GRACE` or the
  value `open_with_grace` received; sealed rows cover their recorded interval. The rows keep
  their recorded `wall_end`.
- Admission latches an overload on any drop or eviction and releases it only when outstanding
  work falls below half the ordinary bound (`OVERLOAD_RELEASE_COMMANDS`,
  `OVERLOAD_RELEASE_BYTES`); the latch refuses queries, never notifications.
- Event listing has both row and aggregate-owned-byte limits and pages by the exclusive
  `(nvr_time_us, id)` cursor. Event protection is checked in SQL alone (`protection_predicate`
  in the candidate pages and the deletion mark); no query projects the active marks. Event
  maintenance scans bounded inactive input, advances after zero-match pages, and preserves
  active marks.
- Retention candidate queries cap raw indexed segment input before protection filtering and
  return the last scanned cursor/count even for an empty output page. Pass row budgets count
  that raw input; age pages seek by start before testing end time, and active protection seeks
  past inactive and expired history. Explicit tie keys plus split seek branches keep deep
  equal-key continuations bounded. The atomic final mark remains the deletion authority.
- Media-serving callers use `segments_limited`; its SQL limit is applied before `SegmentRow`
  materialization. Do not emulate the serving bound by truncating an unbounded `segments` result.
- Capacity admission must not let mixed-size replacement grow logical event bytes past the
  ceiling. Expired marks are cleared incrementally; if bounded pruning cannot make room, an
  async event is omitted without failing segment changes and direct insertion returns
  `EventStorageLimit`.
- Open rejects a schema version above the supported value before persistent pragmas or writes;
  it also validates the persisted `segment.sealed` and `segment.deleting` domains before state
  filters can hide a row. Every persisted conversion reports `CatalogError::Corrupt` on
  violation, naming the column where SQLite reports one. Caller faults are their own variants
  (`RootIdentityMismatch`, `RootIdentityInUse`, `NonUtf8Path`), never `Corrupt`.
- Root identities cross the API as `iris_core::id::RootIdentity`, never as text; a stored
  identity that does not parse is `Corrupt`.
- `CatalogStats` and `CatalogMetrics` accessor names are read by `irisd`'s `catalog`
  statistics line and `iris-tools daemon-stats`.

## Where things are

- `src/handle.rs` — `CatalogHandle`, the bounded command queue, the thread body (`run`),
  batching (`flush`), the query wrapper, shutdown.
- `src/db.rs` — `Db`: open and migrate, pragmas, the schema, every SQL statement (the shared
  `protection_predicate` and `mark_sql` included), `apply` (one transaction per batch),
  `stats`; `CatalogRootRegistry`, `CatalogRootExpectation`, `OPEN_SEGMENT_GRACE`.
- `src/types.rs` — row types, `SegmentCursor`, `StartupRow`, `Change`, `NewSegment`,
  `TimeRange`, `CatalogError`.
- `src/metrics.rs` — `CatalogMetrics`.
- `tests/catalog.rs` — integration tests against a temporary database, through the methods
  the daemon and the API use.

## How to verify a change

```
cargo fmt -p iris-catalog
cargo clippy -p iris-catalog --all-targets -- -D warnings
cargo test -p iris-catalog
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-catalog --no-deps
```

Tests use `tempfile` directories only. A query change should come with a test in
`tests/catalog.rs` that exercises ordering against a preceding `notify`, and a changed
statement keeps the plan tests green, which explain the production SQL builders themselves.
The `test-support` feature (whole-table listings, an unprotected `mark_deleting`) exists for
the tests of dependent crates; the daemon never enables it.

## Pitfalls

- Never open a write transaction with `Connection::transaction()` (DEFERRED). A deferred
  transaction that reads before it writes gets `SQLITE_BUSY` at once when another connection
  holds the write lock, because SQLite skips the busy handler for a reader upgrading to a
  writer; the failed batch is dropped. Use `write_transaction`.
- `Change::SegmentProgress` and `SegmentSealed` find their row by path; the writer and the
  catalog must agree on the exact `PathBuf` (no normalisation on either side).
- `SegmentOpened` interns the sample entry by CRC-32C and bytes together (`UNIQUE(hash,
  config)`, schema v10), so colliding CRCs keep separate rows. Compare `sample_entry_id`,
  never `sample_entry_hash`, for identity.
- `events_after` returns oldest-first pages after an exclusive `(nvr_time_us, id)` cursor;
  advancing only the time can skip rows that share a timestamp.
- Sealed retention pagination orders by `(wall_start_us, id)` and continues from a
  `SegmentCursor`, which carries both; a time-only cursor can skip recordings with equal start
  times.
- The event commit latency metric is measured from `Event::nvr_time` to the end of the
  committing transaction; it includes the batch interval by design.
- `shutdown` is idempotent and joins the thread; every clone's later query returns
  `CatalogError::Closed`.
- Dropping the final handle closes admission and joins the thread even without `shutdown`.
- The database file path is remembered for `stats`; the WAL file is the path with `-wal`
  appended and may legitimately be absent.
- `CatalogMetrics::queue_peak` and `queue_bytes_peak` are reset by the daemon statistics
  reporter; Prometheus reads them without resetting them.

## Related documents

- `docs/design/phase2-interfaces.md`, `docs/research/04-storage-format.md §6 and §10`,
  `07-architecture.md §3 and §9`.
- Depends on `iris-core` and `iris-mp4` (sample entries). Depended on by `irisd`
  (observer, retention, adoption, self-check) and `iris-api` (listing, playlists, export,
  events).
