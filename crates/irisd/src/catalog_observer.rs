//! Adapters that feed the catalog: store notifications and camera events.

use std::sync::Arc;

use iris_catalog::{CatalogHandle, Change};
use iris_core::event::Event;
use iris_events::EventSink;
use iris_store::{SegmentOpened, SegmentProgress, SegmentSealed, StoreObserver};
use tokio::sync::broadcast;

/// Forwards writer-thread notifications to the catalog without blocking.
pub struct CatalogObserver {
    catalog: CatalogHandle,
}

impl CatalogObserver {
    /// Wraps a catalog handle.
    pub fn new(catalog: CatalogHandle) -> Self {
        Self { catalog }
    }
}

impl StoreObserver for CatalogObserver {
    fn segment_opened(&self, info: SegmentOpened) {
        self.catalog.notify(Change::SegmentOpened {
            key: info.key,
            path: info.path,
            run_seq: info.run_seq,
            discontinuity: info.discontinuity,
            wall_start: info.wall_start,
            media_start: info.media_start,
            init_len: info.init_len,
            sample_entry: info.sample_entry,
        });
    }

    fn segment_progress(&self, info: SegmentProgress) {
        self.catalog.notify(Change::SegmentProgress {
            path: info.path,
            durable_len: info.durable_len,
            fragments: info.fragments,
            wall_end: info.wall_end,
            media_end: info.media_end,
        });
    }

    fn segment_sealed(&self, info: SegmentSealed) {
        self.catalog.notify(Change::SegmentSealed {
            path: info.path,
            len: info.len,
            fragments: info.fragments,
            wall_end: info.wall_end,
            media_end: info.media_end,
        });
    }
}

/// Records events in the catalog and fans them out to API subscribers. The
/// live fan-out is independent of persistence: an event the catalog rejects
/// under overload still reaches the subscribers.
pub struct CatalogEventSink {
    catalog: CatalogHandle,
    events: broadcast::Sender<Arc<Event>>,
}

impl CatalogEventSink {
    /// Wraps a catalog handle and the broadcast channel the API listens on.
    pub fn new(catalog: CatalogHandle, events: broadcast::Sender<Arc<Event>>) -> Self {
        Self { catalog, events }
    }
}

impl EventSink for CatalogEventSink {
    fn publish(&self, event: Event) {
        let event = event.bounded();
        self.catalog.notify(Change::Event(event.clone()));
        // No subscribers is not an error.
        let _ = self.events.send(Arc::new(event));
    }
}
