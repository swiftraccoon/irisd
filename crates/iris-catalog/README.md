# iris-catalog

The SQLite catalog of irisd: which recording files exist, what each covers in wall and
media time, whether it is sealed, and every camera event. One thread named `iris-catalog`
owns the connection; the writer thread and ingest tasks post fire-and-forget changes that
never block, and the API, retention, and the self-check run async queries that answer on
the same thread in send order. In the record path (RTSP in → fMP4 on disk → byte-range
serving) this crate is the index that turns a wall-clock range into byte ranges of files.

## What it provides

- `CatalogHandle` — `open` creates or migrates the database and starts the thread;
  `notify` queues a `Change`; async queries (`cameras`, `segments`, `segment`,
  `segment_by_path`, `writing_segments`, `oldest_sealed`, `newest_sealed`, `total_bytes`,
  `events`, `retained_events`, `oldest_sealed_after`, `stats`) and mutations
  (`ensure_camera`, `mark_sealed`, `adopt_segment`, `delete_segment`, `insert_event`,
  `retain_event`); `metrics`; `shutdown`.
- `Change` — `SegmentOpened`, `SegmentProgress`, `SegmentSealed`, `Event`: what the store's
  observer and the event feeds report, batched into one transaction per 100 ms.
- `SegmentRow`, `EventRow`, `CameraRow`, `NewSegment`, `SegmentId`, `EventId`, `TimeRange`
  (half-open `[from, to)` in wall microseconds) — the row types.
- `CatalogStats` — segment count, sealed bytes, event count, database and WAL sizes.
- `CatalogMetrics` — transaction, query, queue-wait, and event receipt-to-commit latency
  histograms with batch counters, shared by every clone of the handle.
- `CatalogError` — SQLite, I/O, not-found, and closed-handle errors.

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
- **Batching.** Changes are committed within 100 ms of the first pending one, or at 1,000
  pending, or before the next query, in one transaction; a failed batch is counted and
  dropped, never retried (`docs/research/04-storage-format.md §6`).
- **Durability settings.** WAL mode, `synchronous = NORMAL`, foreign keys on, a 5 s busy
  timeout; the database belongs on an SSD (`04 §6`).
- **Schema.** `camera`, `sample_entry` (codec configurations keyed by hash), `segment`
  (one row per file, no per-fragment rows; index on `(camera_id, kind, wall_start_us)`),
  `event` (index on `(camera_id, nvr_time_us)`; version 2 adds `retain_until_us`),
  `schema_version` (migrations run on open, each version's statements in a transaction)
  (`docs/design/phase2-interfaces.md`).
- **Segment rows** carry wall and media extents, the durable length (updated after each
  sync), the final length and fragment count at seal, and the sealed flag; `bytes` of an
  open file is its written length, so `total_bytes` is what a quota compares against.
- **Retention queries** (`oldest_sealed`, `total_bytes`, `segments` by range) are what
  `irisd`'s retention pass uses; the self-check uses `newest_sealed`.
- **Event data is bounded** before insertion (`Event::bounded`), so a camera cannot grow a
  row without limit.

## Tests and measurements

- `tests/catalog.rs`: camera round trip; the segment lifecycle observed in order (opened,
  progress, sealed); retention ordering and deletion; sealing and adoption; event filters
  by camera, range, and limit; bounded event data; idempotent shutdown and closed clones;
  reopening with the schema intact; metrics counting batches, queries, and waits; event
  commit latency; `newest_sealed`; database and WAL sizes in `stats`.
- Measurements: `docs/research/12-ingest-cost.md` (transaction p99 under load) and the soak
  figures in `docs/research/11-multi-camera-results.md`.

## Further reading

- `docs/research/04-storage-format.md` §6 (catalog), §10 (serving from the format)
- `docs/design/phase2-interfaces.md` (the handle contract and the schema)
- `docs/research/07-architecture.md` §3 (threads) and §9 (observability)
- `STANDARDS.md`
