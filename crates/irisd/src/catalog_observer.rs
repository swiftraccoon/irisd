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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use iris_catalog::TimeRange;
    use iris_core::event::{EventKind, EventSource, EventState};
    use iris_core::id::{StreamKey, StreamKind};
    use iris_core::time::WallUs;
    use iris_mp4::fragment::{Fragment, Sample};
    use iris_mp4::init::SampleEntry;
    use iris_store::{StoragePolicy, WriterHandle};

    use super::*;

    fn fragment(index: u64, entry: &Arc<SampleEntry>) -> Fragment {
        Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 90_000,
                is_sync: true,
            }],
            decode_time: index * 90_000,
            wall: WallUs::new(1_700_000_000_000_000 + i64::try_from(index).unwrap() * 1_000_000),
            raw_rtp: 0,
            new_run: index == 0,
            sample_entry: Arc::clone(entry),
        }
    }

    /// What the writer reports reaches the catalog: a file that is being
    /// written has a row from its first fragment, and a sealed one carries
    /// its final length, fragment count, and bounds.
    #[tokio::test]
    async fn the_writers_notifications_become_catalog_rows() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("state").join("irisd.db")).unwrap();
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        };
        catalog
            .ensure_camera(&key.camera, "cam1", None)
            .await
            .unwrap();
        let entry = Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1.1.6.L150.B0".into(),
            640,
            480,
        ));
        let writer = WriterHandle::spawn_with_observer(
            dir.path().join("video"),
            StoragePolicy {
                sync_interval: Duration::from_millis(20),
                preallocate_bytes: 0,
                rotate_bytes: 1 << 30,
                rotate_age: Duration::from_secs(3_600),
            },
            8,
            Arc::new(CatalogObserver::new(catalog.clone())),
        )
        .unwrap();
        writer.try_send(key.clone(), fragment(0, &entry)).unwrap();
        writer.try_send(key.clone(), fragment(1, &entry)).unwrap();

        // The open file is listed once its fragments are durable.
        let open = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let rows = catalog.writing_segments().await.unwrap();
                if rows.first().is_some_and(|row| row.fragments == 2) {
                    break rows;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the open file's progress reached the catalog");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].key, key);
        assert!(!open[0].sealed && open[0].discontinuity);
        assert_eq!(open[0].wall_start, WallUs::new(1_700_000_000_000_000));
        assert_eq!(open[0].media_start, 0);
        assert_eq!((open[0].width, open[0].height), (640, 480));

        writer.try_send(key.clone(), fragment(2, &entry)).unwrap();
        tokio::task::spawn_blocking(move || writer.shutdown())
            .await
            .unwrap()
            .unwrap();
        let row = catalog.segment(open[0].id).await.unwrap().unwrap();
        let len = std::fs::metadata(&row.path).unwrap().len();
        assert!(row.sealed);
        assert_eq!((row.bytes, row.durable_len), (len, len));
        assert_eq!(row.fragments, 3);
        assert_eq!(row.media_end, 270_000);
        assert_eq!(row.wall_end, WallUs::new(1_700_000_003_000_000));
        assert_eq!(catalog.writing_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    }

    /// An event is stored and sent to the live subscribers, bounded on the
    /// way; without subscribers it is still stored.
    #[tokio::test]
    async fn events_are_stored_and_fanned_out() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("state").join("irisd.db")).unwrap();
        let camera: iris_core::id::CameraId = "cam1".parse().unwrap();
        catalog.ensure_camera(&camera, "cam1", None).await.unwrap();
        let (sender, mut receiver) = broadcast::channel(4);
        let sink = CatalogEventSink::new(catalog.clone(), sender);
        let event = |time: i64| Event {
            camera: camera.clone(),
            source: EventSource::DahuaCgi,
            kind: EventKind::Motion,
            state: EventState::Start,
            nvr_time: WallUs::new(time),
            camera_time: None,
            data: Some("not json".to_owned()),
        };
        sink.publish(event(1_000));
        let live = receiver.recv().await.unwrap();
        assert_eq!(live.nvr_time, WallUs::new(1_000));
        assert_eq!(
            live.data.as_deref(),
            Some("\"not json\""),
            "bounded as JSON"
        );
        drop(receiver);
        sink.publish(event(2_000));

        let everything = TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(10_000),
        };
        let stored = catalog
            .events_after(None, everything, None, 10)
            .await
            .unwrap();
        let times: Vec<i64> = stored.iter().map(|row| row.event.nvr_time.get()).collect();
        assert_eq!(times, [1_000, 2_000]);
        assert_eq!(stored[0].event.data.as_deref(), Some("\"not json\""));
        catalog.shutdown();
    }
}
