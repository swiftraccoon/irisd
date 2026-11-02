//! The catalog thread and the handle that talks to it.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tracing::{debug, error, info};

use crate::db::Db;
use crate::metrics::CatalogMetrics;
use crate::types::{
    CameraRow, CatalogError, CatalogStats, Change, EventId, EventRow, NewSegment, SegmentId,
    SegmentRow, TimeRange,
};
use iris_core::event::Event;
use iris_core::id::{CameraId, StreamKey};
use iris_core::time::WallUs;

/// Longest a change waits before it is committed.
const BATCH_INTERVAL: Duration = Duration::from_millis(100);
/// Changes that force an early commit.
const MAX_BATCH: usize = 1_000;
/// Wake-up interval while idle (only to notice a dropped channel).
const IDLE_WAIT: Duration = Duration::from_secs(1);

type Query = Box<dyn FnOnce(&mut Db) + Send + 'static>;

enum Command {
    Change(Change),
    /// A query and the instant it was sent.
    Query(Query, Instant),
    Stop,
}

struct Inner {
    tx: Sender<Command>,
    join: Mutex<Option<JoinHandle<()>>>,
    metrics: Arc<CatalogMetrics>,
}

/// A cheap, cloneable handle to the catalog thread.
///
/// Commands are processed in send order. A query sent after
/// [`CatalogHandle::notify`] therefore observes that change, because the
/// thread commits every pending change before running a query.
#[derive(Clone)]
pub struct CatalogHandle {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CatalogHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogHandle").finish_non_exhaustive()
    }
}

impl CatalogHandle {
    /// Opens (creating and migrating if needed) the database at `path` and
    /// starts the catalog thread.
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        let (tx, rx) = mpsc::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), CatalogError>>();
        let path: PathBuf = path.to_path_buf();
        let metrics = Arc::new(CatalogMetrics::new());
        let thread_metrics = Arc::clone(&metrics);
        let join = std::thread::Builder::new()
            .name("iris-catalog".into())
            .spawn(move || {
                let mut db = match Db::open(&path) {
                    Ok(db) => {
                        let _ = ready_tx.send(Ok(()));
                        db
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                info!(path = %path.display(), "catalog open");
                run(&mut db, &rx, &thread_metrics);
                info!("catalog thread exiting");
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = join.join();
                return Err(e);
            }
            Err(_) => {
                let _ = join.join();
                return Err(CatalogError::Closed);
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                tx,
                join: Mutex::new(Some(join)),
                metrics,
            }),
        })
    }

    /// Queues a change; never blocks. Changes are committed within 100 ms or
    /// before the next query, whichever comes first.
    pub fn notify(&self, change: Change) {
        if self.inner.tx.send(Command::Change(change)).is_err() {
            error!("catalog change dropped: catalog is closed");
        }
    }

    async fn query<T, F>(&self, f: F) -> Result<T, CatalogError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T, CatalogError> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        // The duration is recorded before the reply is sent, so a caller that
        // reads the metrics right after the result sees the query counted.
        let metrics = Arc::clone(&self.inner.metrics);
        let task: Query = Box::new(move |db: &mut Db| {
            let started = Instant::now();
            let result = f(db);
            metrics.query().record(started.elapsed());
            let _ = reply_tx.send(result);
        });
        self.inner
            .tx
            .send(Command::Query(task, Instant::now()))
            .map_err(|_| CatalogError::Closed)?;
        reply_rx.await.map_err(|_| CatalogError::Closed)?
    }

    /// Inserts or updates a camera.
    pub async fn ensure_camera(
        &self,
        id: &CameraId,
        name: &str,
        vendor: Option<&str>,
    ) -> Result<(), CatalogError> {
        let id = id.clone();
        let name = name.to_owned();
        let vendor = vendor.map(str::to_owned);
        self.query(move |db| db.ensure_camera(&id, &name, vendor.as_deref()))
            .await
    }

    /// Lists cameras.
    pub async fn cameras(&self) -> Result<Vec<CameraRow>, CatalogError> {
        self.query(Db::cameras).await
    }

    /// Segments of a stream overlapping `range`, ordered by start time.
    pub async fn segments(
        &self,
        key: &StreamKey,
        range: TimeRange,
    ) -> Result<Vec<SegmentRow>, CatalogError> {
        let key = key.clone();
        self.query(move |db| db.segments(&key, range)).await
    }

    /// One segment by id.
    pub async fn segment(&self, id: SegmentId) -> Result<Option<SegmentRow>, CatalogError> {
        self.query(move |db| db.segment(id)).await
    }

    /// One segment by path.
    pub async fn segment_by_path(&self, path: &Path) -> Result<Option<SegmentRow>, CatalogError> {
        let path = path.to_path_buf();
        self.query(move |db| db.segment_by_path(&path)).await
    }

    /// Segments not yet sealed (open when the daemon last ran).
    pub async fn writing_segments(&self) -> Result<Vec<SegmentRow>, CatalogError> {
        self.query(Db::writing_segments).await
    }

    /// The oldest sealed segments, candidates for retention.
    pub async fn oldest_sealed(&self, limit: usize) -> Result<Vec<SegmentRow>, CatalogError> {
        self.query(move |db| db.oldest_sealed(limit)).await
    }

    /// The sealed segment of `key` that ends last.
    pub async fn newest_sealed(&self, key: &StreamKey) -> Result<Option<SegmentRow>, CatalogError> {
        let key = key.clone();
        self.query(move |db| db.newest_sealed(&key)).await
    }

    /// Bytes of every segment in the catalog, open files at their written length.
    pub async fn total_bytes(&self) -> Result<u64, CatalogError> {
        self.query(Db::total_bytes).await
    }

    /// Deletes a segment row (the caller removes the files).
    pub async fn delete_segment(&self, id: SegmentId) -> Result<(), CatalogError> {
        self.query(move |db| db.delete_segment(id)).await
    }

    /// Marks a segment sealed with its final lengths.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when no segment has that path.
    pub async fn mark_sealed(
        &self,
        path: &Path,
        len: u64,
        fragments: u32,
        wall_end: WallUs,
        media_end: u64,
    ) -> Result<(), CatalogError> {
        let path = path.to_path_buf();
        self.query(move |db| db.mark_sealed(&path, len, fragments, wall_end, media_end))
            .await
    }

    /// Adopts a sealed file found on disk.
    pub async fn adopt_segment(&self, row: NewSegment) -> Result<SegmentId, CatalogError> {
        self.query(move |db| db.adopt_segment(&row)).await
    }

    /// Events in `range`, oldest first, at most `limit`.
    pub async fn events(
        &self,
        camera: Option<&CameraId>,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<EventRow>, CatalogError> {
        let camera = camera.cloned();
        self.query(move |db| db.events(camera.as_ref(), range, limit))
            .await
    }

    /// Sets (or clears) the time until which an event's footage is kept.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::UnknownEvent`] when no event has that id.
    pub async fn retain_event(
        &self,
        id: EventId,
        until: Option<WallUs>,
    ) -> Result<EventRow, CatalogError> {
        self.query(move |db| db.retain_event(id, until)).await
    }

    /// Events whose retention time is after `now`.
    pub async fn retained_events(&self, now: WallUs) -> Result<Vec<EventRow>, CatalogError> {
        self.query(move |db| db.retained_events(now)).await
    }

    /// The oldest sealed segments starting after `after`, at most `limit`.
    pub async fn oldest_sealed_after(
        &self,
        after: WallUs,
        limit: usize,
    ) -> Result<Vec<SegmentRow>, CatalogError> {
        self.query(move |db| db.oldest_sealed_after(after, limit))
            .await
    }

    /// Inserts an event and returns its id.
    pub async fn insert_event(&self, event: &Event) -> Result<EventId, CatalogError> {
        let event = event.clone();
        self.query(move |db| db.insert_event(&event)).await
    }

    /// Aggregate counts.
    pub async fn stats(&self) -> Result<CatalogStats, CatalogError> {
        self.query(Db::stats).await
    }

    /// The catalog thread's latency and batch counters, shared by every clone.
    #[must_use]
    pub fn metrics(&self) -> Arc<CatalogMetrics> {
        Arc::clone(&self.inner.metrics)
    }

    /// Commits pending changes and stops the thread. Later calls are no-ops;
    /// later queries from any clone return [`CatalogError::Closed`].
    pub fn shutdown(&self) {
        let handle = self
            .inner
            .join
            .lock()
            .map_or(None, |mut guard| guard.take());
        let Some(handle) = handle else {
            return;
        };
        let _ = self.inner.tx.send(Command::Stop);
        if handle.join().is_err() {
            error!("catalog thread panicked");
        }
    }
}

/// The thread body: batches changes, runs queries in order, stops on request.
fn run(db: &mut Db, rx: &mpsc::Receiver<Command>, metrics: &CatalogMetrics) {
    let mut pending: Vec<Change> = Vec::new();
    let mut deadline: Option<Instant> = None;
    loop {
        let wait = deadline.map_or(IDLE_WAIT, |d| d.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(wait) {
            Ok(Command::Change(change)) => {
                if pending.is_empty() {
                    deadline = Some(Instant::now() + BATCH_INTERVAL);
                }
                pending.push(change);
                if pending.len() >= MAX_BATCH {
                    flush(db, &mut pending, &mut deadline, metrics);
                }
            }
            Ok(Command::Query(query, sent)) => {
                metrics.wait().record(sent.elapsed());
                flush(db, &mut pending, &mut deadline, metrics);
                query(db);
            }
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => {
                flush(db, &mut pending, &mut deadline, metrics);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    flush(db, &mut pending, &mut deadline, metrics);
                }
            }
        }
    }
}

fn flush(
    db: &mut Db,
    pending: &mut Vec<Change>,
    deadline: &mut Option<Instant>,
    metrics: &CatalogMetrics,
) {
    *deadline = None;
    if pending.is_empty() {
        return;
    }
    let started = Instant::now();
    let result = db.apply(pending);
    metrics.record_batch(pending.len(), started.elapsed(), result.is_ok());
    match result {
        Ok(()) => {
            debug!(changes = pending.len(), "catalog batch committed");
            let committed = WallUs::now();
            for change in pending.iter() {
                if let Change::Event(event) = change {
                    let delay = committed.get().saturating_sub(event.nvr_time.get()).max(0);
                    metrics
                        .event_commit()
                        .record(Duration::from_micros(u64::try_from(delay).unwrap_or(0)));
                }
            }
        }
        Err(e) => error!(changes = pending.len(), error = %e, "catalog batch failed"),
    }
    pending.clear();
}
