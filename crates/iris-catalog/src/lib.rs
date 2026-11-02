//! SQLite catalog of recordings and events.
//!
//! One thread named `iris-catalog` owns the connection. Writers that must never
//! block (the store's writer thread, ingest tasks, event feeds) send
//! fire-and-forget [`Change`]s with [`CatalogHandle::notify`]; everything else
//! uses the async query methods, which run on the catalog thread and reply
//! through a oneshot channel. Commands are applied in the order they were
//! sent, so a query issued after a notification observes it.
//!
//! # Types
//!
//! - [`CatalogHandle`]: open, notify, query, mutate, read [`CatalogMetrics`],
//!   shut down. Clones share one thread.
//! - [`Change`]: what the store's observer and the event feeds report,
//!   batched into one transaction per 100 ms (or 1,000 changes, or the next
//!   query).
//! - [`SegmentRow`], [`EventRow`], [`CameraRow`], [`NewSegment`]: the rows;
//!   [`SegmentId`] and [`EventId`] their keys; [`TimeRange`] a half-open wall
//!   range.
//! - [`CatalogStats`] and [`CatalogMetrics`]: aggregate counts and the thread's
//!   latency histograms.
//! - [`CatalogError`]: SQLite, I/O, not found, closed.
//!
//! # Invariants
//!
//! - One row per recording file, none per fragment; a file's byte ranges come
//!   from its sidecar index, not from here.
//! - WAL mode with `synchronous = NORMAL`: a crash loses at most the batches
//!   not yet checkpointed, never the schema.
//! - A failed batch is counted and dropped; the next batch starts clean.
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

pub use handle::CatalogHandle;
pub use metrics::CatalogMetrics;
pub use types::{
    CameraRow, CatalogError, CatalogStats, Change, EventId, EventRow, NewSegment, SegmentId,
    SegmentRow, TimeRange,
};
