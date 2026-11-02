# AGENTS.md — iris-catalog

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The schema and the handle contract are fixed by `docs/design/phase2-interfaces.md`; a
  schema change needs a migration step keyed on `schema_version` and an update to that
  document and to `docs/research/04-storage-format.md §6`.
- Command ordering: changes and queries are one FIFO; `flush` runs before every query so a
  reader observes earlier notifications. Do not add a second channel or a fast path that
  bypasses the queue.
- `notify` never blocks and never fails loudly: a closed thread drops the change.
- Pragmas (WAL, `synchronous = NORMAL`, foreign keys, busy timeout) implement `04 §6`.
- `CatalogStats` and `CatalogMetrics` accessor names are read by `irisd`'s `catalog`
  statistics line and `iris-tools daemon-stats`.

## Where things are

- `src/handle.rs` — `CatalogHandle`, the command channel, the thread body (`run`),
  batching (`flush`), the query wrapper, shutdown.
- `src/db.rs` — `Db`: open and migrate, pragmas, the schema, every SQL statement,
  `apply` (one transaction per batch), `stats`.
- `src/types.rs` — row types, `Change`, `NewSegment`, `TimeRange`, `CatalogError`.
- `src/metrics.rs` — `CatalogMetrics`.
- `tests/catalog.rs` — integration tests against a temporary database.

## How to verify a change

```
cargo fmt -p iris-catalog
cargo clippy -p iris-catalog --all-targets -- -D warnings
cargo test -p iris-catalog
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-catalog --no-deps
```

Tests use `tempfile` directories only. A query change should come with a test in
`tests/catalog.rs` that exercises ordering against a preceding `notify`.

## Pitfalls

- `Change::SegmentProgress` and `SegmentSealed` find their row by path; the writer and the
  catalog must agree on the exact `PathBuf` (no normalisation on either side).
- `SegmentOpened` interns the sample entry by hash; a hash collision between different
  configurations would alias them (CRC-32C of the entry bytes).
- `events` returns at most `limit` rows oldest first; callers page by advancing `from`.
- The event commit latency metric is measured from `Event::nvr_time` to the end of the
  committing transaction; it includes the batch interval by design.
- `shutdown` is idempotent and joins the thread; every clone's later query returns
  `CatalogError::Closed`.
- The database file path is remembered for `stats`; the WAL file is the path with `-wal`
  appended and may legitimately be absent.

## Related documents

- `docs/design/phase2-interfaces.md`, `docs/research/04-storage-format.md §6 and §10`,
  `07-architecture.md §3 and §9`.
- Depends on `iris-core` and `iris-mp4` (sample entries). Depended on by `irisd`
  (observer, retention, adoption, self-check) and `iris-api` (listing, playlists, export,
  events).
