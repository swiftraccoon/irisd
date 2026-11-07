# iris-catalog

The SQLite catalog of irisd: which recording files exist, what each covers in wall and
media time, whether it is sealed, and every camera event. One thread named `iris-catalog`
owns the connection; the writer thread and ingest tasks post fire-and-forget changes that
never block, and the API, retention, and the self-check run async queries that answer on
the same thread in send order. In the record path (RTSP in → fMP4 on disk → byte-range
serving) this crate is the index that turns a wall-clock range into byte ranges of files.

## What it provides

- `CatalogRootRegistry` — synchronously migrates and resolves configured-root expectations
  before Landlock or any worker thread is created.
- `CatalogHandle` — `open` creates or migrates the database and starts the thread;
  `notify` queues a `Change`; async queries (`cameras`, `segments`, `segments_limited`, `segment`,
  `segment_by_path`, paged writing/deleting rows, exact path membership, `oldest_sealed`,
  `newest_sealed`, `total_bytes`,
  `events_after`, `event_protections`, `retention_candidates`, `expired_candidates`,
  `stats`) and mutations (`ensure_camera`, `mark_sealed`, `adopt_segment`,
  `mark_deleting_if_unprotected`, `delete_segment`, `insert_event`, `retain_event`,
  `maintain_events`), plus durable root registration, bounded root assignment, and the
  open-row deletion-intent transition for startup reconciliation; `metrics`; `shutdown`.
- `OPEN_SEGMENT_GRACE` — how long past its last durable progress an unsealed row still
  extends toward the present in range listings (30 s).
- `Change` — `SegmentOpened`, `SegmentProgress`, `SegmentSealed`, `Event`: what the store's
  observer and the event feeds report, batched into one transaction per 100 ms.
- `SegmentRow`, `EventRow`, `EventProtection`, `RetentionPage`, `CameraRow`, `NewSegment`,
  `SegmentId`, `EventId`, `EventCursor`, `EventProtectionCursor`, `DeleteMarkResult`, and
  `TimeRange` (half-open `[from, to)` in wall microseconds) — rows, stable cursors, scan
  progress, and mutation outcomes.
- `CatalogStats` — segment and sealed-byte totals; rows and bytes in deletion intent; event
  row, logical-byte, active-mark, and prune totals; database and WAL sizes.
- `CatalogMetrics` — transaction, query, queue-wait, and event receipt-to-commit latency
  histograms; batch counters; queue count/owned-byte gauges and peaks; coalescing, rejection,
  and rejection counters plus the reconciliation-required flag, shared by every clone of the
  handle.
- `CatalogError` — SQLite, I/O, corruption, unsupported-schema, overload, event-capacity,
  not-found, and closed-handle errors.

## How it is used

```rust
use iris_catalog::{CatalogHandle, Change, TimeRange};
use iris_core::id::{CameraId, StreamKey};
use iris_core::time::WallUs;

# async fn example() -> Result<(), iris_catalog::CatalogError> {
let catalog = CatalogHandle::open(std::path::Path::new("irisd.db"))?;
let camera = CameraId::try_new("cam1").expect("valid id");
catalog.ensure_camera(&camera, "Front", Some("dahua")).await?;

// The store's observer posts changes from the writer thread; this never blocks.
// catalog.notify(Change::SegmentOpened { .. });

// Queries run after every change sent before them.
let key: StreamKey = "cam1/main".parse().expect("key");
let now = WallUs::now();
let rows = catalog.segments(&key, TimeRange { from: now.offset(-3_600_000_000), to: now }).await?;
println!("{} segments in the last hour", rows.len());
catalog.shutdown();
# Ok(())
# }
```

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
- **Connection settings.** WAL mode, `synchronous = NORMAL` for ordinary work, foreign keys
  on, a 5 s busy timeout, and `temp_store = MEMORY` so bounded sorters and materialized queries
  never need an ambient temporary directory outside the filesystem sandbox. Every write
  transaction begins `IMMEDIATE`, so the busy timeout also covers taking the write lock: a
  second writer (an operator shell, a checkpoint, a backup) delays a batch by up to 5 s instead
  of failing it at once. Migration rereads the schema version under that lock. Deletion intent
  commits under scoped `synchronous = FULL` before filesystem mutation and restores `NORMAL`;
  the database belongs on an SSD (`04 §6`).
- **File modes.** Open creates missing parent directories with mode 0700 and a missing
  database file with mode 0600, whatever the umask; SQLite gives the WAL and shared-memory
  files the database file's mode.
- **Schema.** Version 9 contains `camera`, `sample_entry`, `segment`, `event`,
  `catalog_state`, `recording_root`, `recording_root_state`, and `schema_version`. Migrations add event retention (v2), deletion intent
  (v3), injective vendor-kind storage (v4), root/global time indexes (v5), and transactional
  event-capacity accounting (v6), the camera-horizon index (v7), then explicit cursor tie keys
  plus age/protection access paths (v8), then configured-path to durable-root-identity binding
  (v9). Open rejects a schema version above nine and rejects
  invalid `segment.sealed` or `segment.deleting` domains before persistent pragmas or writes.
  Persisted values pass through checked conversions; invalid identities, enums, booleans,
  numerics, or aggregates return `CatalogError::Corrupt`.
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
- **Startup reconciliation.** Root assignment, open rows, and deletion intents are paged by
  stable ids with at most 512 rows per page. Existing non-null root ownership is never rewritten.
  An open row whose pair is empty or confirmed absent enters FULL-synchronous deletion intent
  before pair removal, whatever its fragment count, and exact-path probes replace a
  cardinality-wide in-memory deletion set.
- **Bounded serving query.** `segments_limited` applies its row limit in SQLite before rows and
  their owned strings are materialized. Media-serving callers use it to reject ranges above their
  documented row budget; `segments` is the unbounded internal query for callers that enforce
  their own limits. A sealed row covers its recorded interval. An unsealed row is listed only
  once it has a durable fragment and extends to the earlier of now and `OPEN_SEGMENT_GRACE`
  (30 s) past its last progress, so an open file whose writer stopped reporting (a camera
  outage, an abandoned file, a row recovery could not reach) drops out of later windows; rows
  keep their recorded `wall_end`. Catalog overload that starves progress longer than the grace
  hides a growing file until its next progress.
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

- `tests/catalog.rs`: ordered segment lifecycle; open-row listing grace and zero-fragment
  exclusion; retention ordering, root isolation, atomic protection, and deletion; event filters,
  tied-time cursors, aggregate read budgets, bounded strings, and lifecycle plateau; bounded
  ingress overload; write paths waiting for an external write lock; idempotent shutdown;
  metrics; database and WAL sizes. Database unit tests cover migrations through version 9,
  the version reread under the write lock, unsupported schema rejection before side effects,
  owner-only file creation, checked corruption errors, injective vendor kinds, query plans,
  durable deletion intent for sealed and open rows, and deletion-intent statistics.
- Measurements: `docs/research/12-ingest-cost.md` (transaction p99 under load) and the soak
  figures in `docs/research/11-multi-camera-results.md`.

## Further reading

- `docs/research/04-storage-format.md` §6 (catalog), §10 (serving from the format)
- `docs/design/phase2-interfaces.md` (the handle contract and the schema)
- `docs/research/07-architecture.md` §3 (threads) and §9 (observability)
- `STANDARDS.md`
