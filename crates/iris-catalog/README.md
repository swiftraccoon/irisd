# iris-catalog

The SQLite catalog of irisd: which recording files exist, what each covers in wall and
media time, whether it is sealed, and every camera event. One thread named `iris-catalog`
owns the connection; the writer thread and ingest tasks post fire-and-forget changes that
never block, and the API, retention, and the self-check run async queries that answer on
the same thread in send order. In the record path (RTSP in → fMP4 on disk → byte-range
serving) this crate is the index that turns a wall-clock range into byte ranges of files.

## What it provides

- `CatalogRootRegistry` and `CatalogRootExpectation` — synchronously migrate the schema and
  resolve what the catalog knows about each configured root (`Known(RootIdentity)`,
  `LegacyOwned`, `Fresh`) before Landlock or any worker thread is created.
- `CatalogHandle` — `open` (or `open_with_grace`) creates or migrates the database and starts
  the thread; `notify` offers a `Change`; async queries (`cameras`, `segments_limited`,
  `segment`, `segment_by_path`, `writing_segments_page`, `deleting_segments_page`,
  `contains_segment_path`, `segment_root`, `newest_sealed`, `total_bytes`, `events_after`,
  `retention_candidates`, `expired_candidates`, `stats`) and mutations
  (`ensure_camera`, `register_recording_root`, `configure_video_roots`, `mark_sealed`,
  `adopt_segment`, `mark_deleting_if_unprotected` and its root-bound variant,
  `mark_empty_writing_deleting`, `delete_segment`, `insert_event`, `retain_event`,
  `maintain_events`); `metrics`; `shutdown`. The `test-support` feature adds whole-table
  listings (`writing_segments`, `deleting_segments`, `oldest_sealed`) and an unprotected
  `mark_deleting` for the tests of dependent crates.
- `OPEN_SEGMENT_GRACE` — the default of how long past its last durable progress an unsealed
  row still extends toward the present in range listings (30 s); the daemon derives a longer
  grace from its sync interval and fragment cap and passes it to `open_with_grace`.
- `Change` — `SegmentOpened`, `SegmentProgress`, `SegmentSealed`, `Event`: what the store's
  observer and the event feeds report, batched into one transaction per 100 ms.
- `SegmentRow`, `EventRow`, `RetentionPage`, `StartupRow`, `CameraRow`, `NewSegment`,
  `SegmentId`, `EventId`, `SampleEntryId`, `SegmentCursor`, `EventCursor`, `DeleteMarkResult`,
  and `TimeRange` (half-open `[from, to)` in wall microseconds) — rows, stable cursors, scan
  progress, and mutation outcomes.
- `CatalogStats` — segment and sealed-byte totals; rows and bytes in deletion intent; event
  row, logical-byte, active-mark, and prune totals; database and WAL sizes.
- `CatalogMetrics` — transaction, query, queue-wait, and event receipt-to-commit latency
  histograms; batch counters; queue count/owned-byte gauges and peaks; the coalesced counter,
  the total and per-kind rejection counters (`rejected`, `rejected_events`,
  `rejected_progress`, `rejected_queries`, `rejected_lifecycle`), and the
  reconciliation-required flag, shared by every clone of the handle.
- `CatalogError` — SQLite, I/O, corruption, unsupported-schema, journal-mode, root-identity,
  path-encoding, overload, event-capacity, event-read-budget, not-found, unknown-event, and
  closed-handle errors.

## How it is used

```rust
use iris_catalog::{CatalogHandle, TimeRange};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_current_thread().build()?;
    runtime.block_on(async {
        let catalog = CatalogHandle::open(&dir.path().join("irisd.db"))?;
        let camera = CameraId::try_new("cam1")?;
        catalog.ensure_camera(&camera, "Front", Some("dahua")).await?;

        // The store's observer posts changes from the writer thread; `notify`
        // never blocks: catalog.notify(Change::SegmentOpened { .. }).

        // Queries run after every change sent before them. Serving callers
        // bound the rows they materialize.
        let key = StreamKey {
            camera,
            kind: StreamKind::Main,
        };
        let now = WallUs::now();
        let last_hour = TimeRange {
            from: now.offset(-3_600_000_000),
            to: now,
        };
        let rows = catalog.segments_limited(&key, last_hour, 1_000).await?;
        println!("{} segments in the last hour", rows.len());
        catalog.shutdown();
        Ok(())
    })
}
```

The example is compiled with the crate's doctests.

## Invariants and limits

- **Ordering.** Commands are processed in send order; a query issued after `notify`
  observes that change because pending changes are committed before any query runs
  (`docs/design/phase2-interfaces.md`).
- **Bounded admission.** The ordering domain admits at most 2,048 commands and 16 MiB of
  estimated owned input, including commands already running. It reserves 128 commands and
  1 MiB for segment open/seal transitions. Events reject under overload, same-path progress
  may coalesce only within the unqueried tail, lifecycle eviction never crosses a query, and
  queries return `CatalogError::Overloaded`. Exhausting the lifecycle reserve latches
  `reconciliation_required`; explicit shutdown or dropping the final handle drains only the
  admitted bounded tail and joins the thread.
- **Batching.** Changes are committed within 100 ms of the first pending one, or at 1,000
  pending, or before the next query, in one transaction; a failed batch is counted and
  dropped, never retried (`docs/research/04-storage-format.md §6`).
- **Connection settings.** WAL mode (confirmed at open; any other journal mode is an error),
  `synchronous = NORMAL` for ordinary work, foreign keys on, a 5 s busy timeout set before the
  first read, and `temp_store = MEMORY` so bounded sorters and materialized queries never need
  an ambient temporary directory outside the filesystem sandbox. Every write runs in a
  transaction that begins `IMMEDIATE`, so the busy timeout also covers taking the write lock: a
  second writer (an operator shell, a checkpoint, a backup) delays a write by up to 5 s instead
  of failing it at once. Migration rereads the schema version under that lock. Deletion intent
  and recording-root registration commit under scoped `synchronous = FULL` before filesystem
  mutation or writer startup and restore `NORMAL`; the database belongs on an SSD (`04 §6`).
- **Paths.** Path columns hold text; a path that is not UTF-8 is refused
  (`CatalogError::NonUtf8Path`) rather than converted lossily, so two distinct paths can never
  merge into one row.
- **File modes.** Open creates missing parent directories with mode 0700 and a missing
  database file with mode 0600, whatever the umask; SQLite gives the WAL and shared-memory
  files the database file's mode.
- **Schema.** Version 10 contains `camera`, `sample_entry`, `segment`, `event`, `catalog_state`,
  `recording_root`, `recording_root_state`, and `schema_version`. Migrations add event retention
  (v2), deletion intent (v3), injective vendor-kind storage (v4), root/global time indexes (v5), and
  transactional event-capacity accounting (v6), the camera-horizon index (v7), then explicit cursor
  tie keys plus age/protection access paths (v8), then configured-path to durable-root-identity
  binding (v9), then sample entries unique by CRC-32C and bytes together (v10), a rebuild that keeps
  every id and checks the foreign keys before it commits, so colliding CRCs keep their own rows and
  the unknown placeholder never merges with a real entry; `SegmentRow` carries the entry's row id
  for identity comparisons. Open rejects a schema version above ten and rejects invalid
  `segment.sealed` or `segment.deleting` domains before persistent pragmas or writes. Persisted
  values pass through checked conversions; invalid identities, enums, booleans, numerics, or
  aggregates return `CatalogError::Corrupt`.
- **Legacy root state.** A catalog migrated from any pre-v9 schema keeps a durable
  `pre_identity_catalog` bit. Every configured path without a registered identity therefore
  remains `LegacyOwned`, including a migrated catalog with no segment rows; it cannot be mistaken
  for a fresh root after a crash or later restart.
- **Segment rows** carry wall and media extents, the durable length (updated after each
  sync), the final length and fragment count at seal, the sealed flag, and durable deletion
  intent. Deleting rows are hidden from serving and candidate queries but retain their bytes
  until pair removal and row deletion complete, so a failed attempt cannot claim quota
  progress; `stats` reports their count and bytes, and the daemon retries them in every
  retention pass as well as at startup. `bytes` of an open file is its written length, so
  `total_bytes` is what a quota compares against.
- **Startup reconciliation.** Root assignment and the registry's ownership scan page by row id
  with a rowid seek per page (two statements, so no page scans from the first row); open rows
  and deletion intents page by `(wall_start, id)` with at most 512 rows per page. A non-null
  root assignment is never rewritten; the deepest configured root that is a parent directory of
  the path wins. An open row whose pair is empty or confirmed absent enters FULL-synchronous
  deletion intent before pair removal, whatever its fragment count, and exact-path probes
  replace a cardinality-wide in-memory deletion set. `delete_segment` removes only a row still
  in deletion intent, so a path a writer reopened in the meantime keeps its row.
- **Bounded serving query.** `segments_limited` applies its row limit in SQLite before rows and
  their owned strings are materialized; media-serving callers use it to reject ranges above
  their documented row budget. A sealed row covers its recorded interval. An unsealed row is
  listed only once it has a durable fragment and extends to the earlier of now and the
  open-segment grace (`OPEN_SEGMENT_GRACE`, 30 s, unless `open_with_grace` set more) past its
  last progress, so an open file whose writer stopped reporting (a camera outage, an abandoned
  file, a row recovery could not reach) drops out of later windows; rows keep their recorded
  `wall_end`. Catalog overload that starves progress longer than the grace hides a growing file
  until its next progress.
- **Overload latch.** Once admission drops an event or progress report, or evicts one for a
  lifecycle change, queries are refused with `CatalogError::Overloaded` until the outstanding
  work falls below half the ordinary bound (960 commands and 7.5 MiB); notifications keep
  being admitted throughout.
- **Retention.** Candidate SQL is indexed, root-aware, and paged by `(wall_start, segment id)`.
  Each call caps raw indexed input before active-protection filtering and returns the last
  scanned cursor/count even when no row passes. Age pages bound raw rows by start before
  testing end time. Protection checks seek past inactive and expired history before testing
  overlap. Explicit tie-key indexes and split seek branches bound continuation inside equal
  start groups. `mark_deleting_if_unprotected` repeats the protection check and records
  deletion intent atomically under FULL synchronization, returning `Marked`, `Protected`, or
  `NotFound`.
- **Event reads.** Listings use separate camera/global indexed statements, stable
  `(nvr_time_us, id)` cursors, at most 10,000 rows, and at most 8 MiB of aggregate owned
  materialized-row bytes (fixed row storage plus owned camera, kind, and payload strings).
  Protection pages contain only id, camera, event time, and retention deadline, contain at
  most 512 rows, and continue by `(retain_until_us, id)` through the deadline-leading index.
- **Event lifecycle.** `Event::bounded` limits payloads to 64 KiB and unmapped kind codes to
  1 KiB. Maintenance spends at most 1,000 row-work units per transaction, carrying scan
  progress across zero-match inactive pages while applying the 90-day or recording-horizon
  boundary and enforcing 2,000,000 rows or 512 MiB of logical event strings.
  Active marks are preserved and admitted up to 100,000 rows or 128 MiB. Capacity admission
  prunes at most the same bounded work, then rejects a direct insertion or omits an async event
  without rolling back segment lifecycle changes.

## Tests and measurements

- `tests/retention_model.rs`: seeded model test of event protection. Over generated
  segments, events, and retain marks on a time grid, the candidate pages (whole and in small
  pages), the per-stream age pages, and the atomic deletion mark each agree with one overlap
  rule written out in the test, at exact bounds and for a mark that lapses at the instant.
- `tests/catalog.rs`: ordered segment lifecycle; open-row listing grace and zero-fragment
  exclusion; retention pages oldest first across equal start times, the atomic mark refusing a
  protected row and marking it once the mark is cleared, root-bound marks and pages, hidden
  marked rows and their deletion, an unmarked row surviving `delete_segment`; event filters,
  tied-time cursors, aggregate read budgets, bounded strings, vendor kinds; checked corruption
  from the handle; bounded ingress overload under a stalled consumer; every write path waiting
  for an external write lock; root registration idempotence and conflicts; idempotent
  shutdown; metrics; database and WAL sizes.
- `src/db.rs` unit tests: every starting schema version 1 to 9 migrating to the same
  `sqlite_master` and event accounting as a fresh database, the v4 built-in kind list against
  `EventKind::BUILT_IN`, the v9 to v10 rebuild keeping ids, the version reread under the write
  lock, schema versions above the supported one rejected before side effects, WAL mode,
  `temp_store`, owner-only file creation, checked corruption errors, sample-entry identity by
  CRC and bytes, root expectations and ownership assignment (deepest root, prefix is not a
  parent, assignments preserved), root-bound marks, the production statements' query plans and
  their VM-step bounds at scale, rowid seeks for the id-paged startup scans, event lifecycle
  plateau and capacity, durable deletion intent for sealed and open rows, and deletion-intent
  statistics.
- `src/handle.rs` unit tests: the admission budgets, overload refusal and its release
  threshold, the lifecycle reserve and eviction (including across a query barrier), progress
  coalescing and its query and same-path barriers, capacity release by bounding, and the thread
  joining when the final handle drops.
- Measurements: `docs/research/12-ingest-cost.md` (transaction p99 under load) and the soak
  figures in `docs/research/11-multi-camera-results.md`.

## Further reading

- `docs/research/04-storage-format.md` §6 (catalog), §10 (serving from the format)
- `docs/design/phase2-interfaces.md` (the handle contract and the schema)
- `docs/research/07-architecture.md` §3 (threads) and §9 (observability)
- `STANDARDS.md`
