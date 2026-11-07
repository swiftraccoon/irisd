//! SQLite catalog of recordings and events.
//!
//! One thread named `iris-catalog` owns the connection. Writers that must never
//! block (the store's writer thread, ingest tasks, event feeds) send
//! fire-and-forget [`Change`]s with [`CatalogHandle::notify`]; everything else
//! uses the async query methods, which run on the catalog thread and reply
//! through a oneshot channel. The single ordering domain is bounded by command
//! count and estimated owned input, with reserved segment-lifecycle capacity.
//! Commands are applied in send order, so a query issued after an admitted
//! notification observes it.
//!
//! # Types
//!
//! - [`CatalogHandle`]: open, notify, query, mutate, read [`CatalogMetrics`],
//!   shut down. Clones share one thread.
//! - [`Change`]: what the store's observer and the event feeds report,
//!   batched into one transaction per 100 ms (or 1,000 changes, or the next
//!   query).
//! - [`SegmentRow`], [`EventRow`], [`EventProtection`], [`RetentionPage`],
//!   [`CameraRow`], [`NewSegment`]: rows and retention projections;
//!   [`SegmentId`] and [`EventId`] their keys; [`EventCursor`] and
//!   [`EventProtectionCursor`] the listing and active-protection
//!   continuations; [`DeleteMarkResult`] the conditional-deletion outcome;
//!   [`TimeRange`] a half-open wall range.
//! - [`CatalogStats`] and [`CatalogMetrics`]: aggregate counts and the thread's
//!   latency, queue, overload, event-state, database, and WAL measurements.
//! - [`CatalogError`]: SQLite, I/O, corruption, unsupported schema, overload,
//!   capacity, not found, and closed errors.
//! - [`OPEN_SEGMENT_GRACE`]: how far past its last durable progress an
//!   unsealed segment extends in range listings.
//!
//! # Invariants
//!
//! - One row per recording file, none per fragment; a file's byte ranges come
//!   from its sidecar index, not from here.
//! - Retention commits deletion intent before filesystem mutation. Those rows
//!   are hidden from ordinary queries and remain available to startup until
//!   durable pair removal permits deleting the row.
//! - Conditional deletion checks active retained-event overlap and updates
//!   deletion intent in one FULL-synchronous transaction.
//! - Event listings have aggregate row and owned-byte limits and use stable
//!   `(time, id)` cursors. Protection pages use `(retain_until, id)`. Event
//!   lifecycle maintenance scans bounded inactive batches by recording horizon,
//!   age, row count, and logical string bytes and carries its scan cursor even
//!   when a page deletes nothing;
//!   active retention marks are preserved and separately capacity-limited.
//!   Event admission that cannot fit after a bounded pruning step is omitted
//!   from an async batch or returns [`CatalogError::EventStorageLimit`] from a
//!   direct insertion without rolling back segment lifecycle changes.
//! - Database open refuses a schema version above the supported value before
//!   persistent pragmas or writes. Persisted values use checked decoding and
//!   corruption is returned to the caller.
//! - WAL mode with `synchronous = NORMAL`: a process crash preserves committed
//!   WAL transactions. A system or power loss can lose recently committed
//!   ordinary batches; deletion intent uses a scoped FULL commit. Temporary
//!   query state stays in memory so the catalog needs no sandbox-external temp
//!   directory.
//! - A failed batch is counted and dropped; the next batch starts clean.
//!   Write transactions begin `IMMEDIATE`, so another connection briefly
//!   holding the write lock delays a batch (up to the 5 s busy timeout)
//!   instead of failing it.
//! - Range listings list an unsealed segment only once it has a durable
//!   fragment and end it at the earlier of now and [`OPEN_SEGMENT_GRACE`] past
//!   its last progress, so an open file whose writer stopped reporting does not
//!   claim later windows.
//! - Opening creates missing parent directories 0700 and a missing database
//!   file 0600; SQLite gives the WAL and shared-memory files the database's mode.
//!
//! # Example
//!
//! ```no_run
//! use iris_catalog::CatalogHandle;
//! use iris_core::id::CameraId;
//!
//! # async fn example() -> Result<(), iris_catalog::CatalogError> {
//! let catalog = CatalogHandle::open(std::path::Path::new("irisd.db"))?;
//! let camera = CameraId::try_new("cam1").expect("valid id");
//! catalog.ensure_camera(&camera, "Example", Some("dahua")).await?;
//! assert_eq!(catalog.cameras().await?.len(), 1);
//! catalog.shutdown();
//! # Ok(())
//! # }
//! ```

mod db;
mod handle;
mod metrics;
mod types;

pub use db::{CatalogRootExpectation, CatalogRootRegistry, OPEN_SEGMENT_GRACE};
pub use handle::CatalogHandle;
pub use metrics::CatalogMetrics;
pub use types::{
    CameraRow, CatalogError, CatalogStats, Change, DeleteMarkResult, EventCursor, EventId,
    EventProtection, EventProtectionCursor, EventRow, NewSegment, RetentionPage, SegmentId,
    SegmentRow, TimeRange,
};
