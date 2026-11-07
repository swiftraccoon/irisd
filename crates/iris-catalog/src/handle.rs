//! The catalog thread and the handle that talks to it.

use std::collections::VecDeque;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tracing::{debug, error, info};

use crate::db::Db;
use crate::metrics::CatalogMetrics;
use crate::types::{
    CameraRow, CatalogError, CatalogStats, Change, DeleteMarkResult, EventCursor, EventId,
    EventProtection, EventProtectionCursor, EventRow, NewSegment, RetentionPage, SegmentId,
    SegmentRow, TimeRange,
};
use iris_core::event::Event;
use iris_core::id::{CameraId, StreamKey};
use iris_core::time::WallUs;

/// Longest a change waits before it is committed.
const BATCH_INTERVAL: Duration = Duration::from_millis(100);
/// Changes that force an early commit.
const MAX_BATCH: usize = 1_000;
/// Wake-up interval while idle.
const IDLE_WAIT: Duration = Duration::from_secs(1);
/// Hard bound for commands admitted but not completed.
const MAX_QUEUED_COMMANDS: usize = 2_048;
/// Hard bound for estimated owned bytes admitted but not completed.
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
/// Capacity unavailable to events, progress, and queries so lifecycle changes
/// can still enter the single ordering domain during overload.
const RESERVED_COMMANDS: usize = 128;
/// Owned-byte reserve paired with [`RESERVED_COMMANDS`].
const RESERVED_BYTES: usize = 1024 * 1024;
/// Conservative allocation overhead for a boxed query closure and reply path.
const QUERY_OVERHEAD: usize = 256;

type Query = Box<dyn FnOnce(&mut Db) + Send + 'static>;

enum Command {
    Change(Change),
    /// A query and the instant it was sent.
    Query(Query, Instant),
}

struct QueuedCommand {
    command: Command,
    bytes: usize,
}

#[derive(Default)]
struct QueueState {
    commands: VecDeque<QueuedCommand>,
    outstanding: usize,
    bytes: usize,
    closed: bool,
    overloaded: bool,
}

struct CommandQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
    metrics: Arc<CatalogMetrics>,
}

enum ChangeAdmission {
    Queued,
    Coalesced,
    Dropped,
    Closed,
    Reconcile,
}

enum QueryAdmission {
    Queued,
    Overloaded,
    Closed,
}

enum Receive {
    Command(QueuedCommand),
    Timeout,
    Closed,
}

impl CommandQueue {
    fn new(metrics: Arc<CatalogMetrics>) -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
            metrics,
        }
    }

    fn push_change(&self, change: Change) -> ChangeAdmission {
        let change = match change {
            Change::Event(event) => Change::Event(event.bounded()),
            other => other,
        };
        let bytes = change_owned_bytes(&change);
        let progress_path = match &change {
            Change::SegmentProgress { path, .. } => Some(path.clone()),
            _ => None,
        };
        let critical = matches!(
            change,
            Change::SegmentOpened { .. } | Change::SegmentSealed { .. }
        );
        let event = matches!(change, Change::Event(_));
        let Ok(mut state) = self.state.lock() else {
            return ChangeAdmission::Closed;
        };
        if state.closed {
            return ChangeAdmission::Closed;
        }

        if let Some(path) = progress_path.as_deref()
            && coalesce_progress(&mut state, path, change.clone(), bytes, &self.metrics)
        {
            return ChangeAdmission::Coalesced;
        }

        if critical {
            while !fits(&state, bytes, MAX_QUEUED_COMMANDS, MAX_QUEUED_BYTES) {
                let first_evictable = state
                    .commands
                    .iter()
                    .rposition(|queued| matches!(queued.command, Command::Query(_, _)))
                    .map_or(0, |index| index + 1);
                let Some(index) = state
                    .commands
                    .iter()
                    .enumerate()
                    .skip(first_evictable)
                    .find_map(|(index, queued)| {
                        matches!(
                            queued.command,
                            Command::Change(Change::Event(_) | Change::SegmentProgress { .. })
                        )
                        .then_some(index)
                    })
                else {
                    break;
                };
                let removed = state
                    .commands
                    .remove(index)
                    .expect("position came from the same queue");
                state.outstanding -= 1;
                state.bytes -= removed.bytes;
                state.overloaded = true;
                match removed.command {
                    Command::Change(Change::Event(_)) => self.metrics.reject_event(),
                    Command::Change(Change::SegmentProgress { .. }) => {
                        self.metrics.reject_progress();
                    }
                    _ => unreachable!("only discardable commands are evicted"),
                }
            }
            if !fits(&state, bytes, MAX_QUEUED_COMMANDS, MAX_QUEUED_BYTES) {
                self.metrics.set_queue_usage(state.outstanding, state.bytes);
                self.metrics.reject_critical();
                return ChangeAdmission::Reconcile;
            }
        } else if !fits(
            &state,
            bytes,
            MAX_QUEUED_COMMANDS - RESERVED_COMMANDS,
            MAX_QUEUED_BYTES - RESERVED_BYTES,
        ) {
            state.overloaded = true;
            if event {
                self.metrics.reject_event();
            } else {
                self.metrics.reject_progress();
            }
            return ChangeAdmission::Dropped;
        }

        state.commands.push_back(QueuedCommand {
            command: Command::Change(change),
            bytes,
        });
        admit(&mut state, bytes, &self.metrics);
        drop(state);
        self.ready.notify_one();
        ChangeAdmission::Queued
    }

    fn push_query(&self, command: Command, bytes: usize) -> QueryAdmission {
        let Ok(mut state) = self.state.lock() else {
            return QueryAdmission::Closed;
        };
        if state.closed {
            return QueryAdmission::Closed;
        }
        if state.overloaded
            || !fits(
                &state,
                bytes,
                MAX_QUEUED_COMMANDS - RESERVED_COMMANDS,
                MAX_QUEUED_BYTES - RESERVED_BYTES,
            )
        {
            self.metrics.reject_query();
            return QueryAdmission::Overloaded;
        }
        state.commands.push_back(QueuedCommand { command, bytes });
        admit(&mut state, bytes, &self.metrics);
        drop(state);
        self.ready.notify_one();
        QueryAdmission::Queued
    }

    fn receive(&self, wait: Duration) -> Receive {
        let deadline = Instant::now() + wait;
        let Ok(mut state) = self.state.lock() else {
            return Receive::Closed;
        };
        loop {
            if let Some(command) = state.commands.pop_front() {
                return Receive::Command(command);
            }
            if state.closed {
                return Receive::Closed;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Receive::Timeout;
            }
            let Ok((next, timed_out)) = self.ready.wait_timeout(state, remaining) else {
                return Receive::Closed;
            };
            state = next;
            if timed_out.timed_out() && state.commands.is_empty() {
                return Receive::Timeout;
            }
        }
    }

    fn complete(&self, count: usize, bytes: usize) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.outstanding = state.outstanding.saturating_sub(count);
        state.bytes = state.bytes.saturating_sub(bytes);
        if state.outstanding < (MAX_QUEUED_COMMANDS - RESERVED_COMMANDS) / 2
            && state.bytes < (MAX_QUEUED_BYTES - RESERVED_BYTES) / 2
        {
            state.overloaded = false;
        }
        self.metrics.set_queue_usage(state.outstanding, state.bytes);
    }

    fn close(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.closed = true;
        drop(state);
        self.ready.notify_all();
    }
}

fn fits(state: &QueueState, bytes: usize, count_limit: usize, byte_limit: usize) -> bool {
    state.outstanding < count_limit && bytes <= byte_limit.saturating_sub(state.bytes)
}

fn admit(state: &mut QueueState, bytes: usize, metrics: &CatalogMetrics) {
    state.outstanding += 1;
    state.bytes += bytes;
    metrics.set_queue_usage(state.outstanding, state.bytes);
}

fn coalesce_progress(
    state: &mut QueueState,
    path: &Path,
    replacement: Change,
    bytes: usize,
    metrics: &CatalogMetrics,
) -> bool {
    for queued in state.commands.iter_mut().rev() {
        match &mut queued.command {
            Command::Query(_, _) => break,
            Command::Change(Change::SegmentProgress {
                path: queued_path, ..
            }) if queued_path == path => {
                let adjusted = state
                    .bytes
                    .saturating_sub(queued.bytes)
                    .saturating_add(bytes);
                if adjusted > MAX_QUEUED_BYTES {
                    return false;
                }
                queued.command = Command::Change(replacement);
                state.bytes = adjusted;
                queued.bytes = bytes;
                metrics.record_coalesced();
                metrics.set_queue_usage(state.outstanding, state.bytes);
                return true;
            }
            Command::Change(
                Change::SegmentOpened {
                    path: queued_path, ..
                }
                | Change::SegmentSealed {
                    path: queued_path, ..
                },
            ) if queued_path == path => break,
            Command::Change(_) => {}
        }
    }
    false
}

fn change_owned_bytes(change: &Change) -> usize {
    let dynamic = match change {
        Change::SegmentOpened {
            key,
            path,
            sample_entry,
            ..
        } => key_owned_bytes(key)
            .saturating_add(path_owned_bytes(path))
            .saturating_add(sample_entry.codecs.len())
            .saturating_add(sample_entry.bytes.len()),
        Change::SegmentProgress { path, .. } | Change::SegmentSealed { path, .. } => {
            path_owned_bytes(path)
        }
        Change::Event(event) => event_owned_bytes(event),
    };
    size_of::<QueuedCommand>()
        .saturating_add(size_of::<Change>())
        .saturating_add(dynamic)
}

fn path_owned_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

fn key_owned_bytes(key: &StreamKey) -> usize {
    key.camera.as_str().len()
}

fn event_owned_bytes(event: &Event) -> usize {
    let kind = match &event.kind {
        iris_core::event::EventKind::Other(code) => code.capacity(),
        _ => 0,
    };
    event
        .camera
        .as_str()
        .len()
        .saturating_add(kind)
        .saturating_add(event.data.as_ref().map_or(0, String::capacity))
}

fn new_segment_owned_bytes(row: &NewSegment) -> usize {
    let sample_entry = row.sample_entry.as_ref().map_or(0, |entry| {
        entry.codecs.capacity().saturating_add(entry.bytes.len())
    });
    key_owned_bytes(&row.key)
        .saturating_add(path_owned_bytes(&row.path))
        .saturating_add(sample_entry)
}

struct Inner {
    queue: Arc<CommandQueue>,
    join: Mutex<Option<JoinHandle<()>>>,
    metrics: Arc<CatalogMetrics>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.queue.close();
        let handle = match self.join.get_mut() {
            Ok(slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if handle.is_some_and(|handle| handle.join().is_err()) {
            error!("catalog thread panicked");
        }
    }
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
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), CatalogError>>();
        let path: PathBuf = path.to_path_buf();
        let metrics = Arc::new(CatalogMetrics::new());
        let thread_metrics = Arc::clone(&metrics);
        let queue = Arc::new(CommandQueue::new(Arc::clone(&metrics)));
        let thread_queue = Arc::clone(&queue);
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
                run(&mut db, &thread_queue, &thread_metrics);
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
                queue,
                join: Mutex::new(Some(join)),
                metrics,
            }),
        })
    }

    /// Queues a change; never blocks. Changes are committed within 100 ms or
    /// before the next query, whichever comes first.
    pub fn notify(&self, change: Change) {
        match self.inner.queue.push_change(change) {
            ChangeAdmission::Queued | ChangeAdmission::Coalesced => {}
            ChangeAdmission::Dropped => {
                debug!("catalog event or progress dropped under bounded overload");
            }
            ChangeAdmission::Closed => error!("catalog change dropped: catalog is closed"),
            ChangeAdmission::Reconcile => {
                error!("catalog lifecycle change rejected: startup reconciliation is required");
            }
        }
    }

    async fn query<T, F>(&self, f: F) -> Result<T, CatalogError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T, CatalogError> + Send + 'static,
    {
        self.query_with_owned_bytes(0, f).await
    }

    async fn query_with_owned_bytes<T, F>(
        &self,
        extra_owned_bytes: usize,
        f: F,
    ) -> Result<T, CatalogError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T, CatalogError> + Send + 'static,
    {
        let command_bytes = QUERY_OVERHEAD
            .saturating_add(size_of::<F>())
            .saturating_add(extra_owned_bytes);
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
        match self
            .inner
            .queue
            .push_query(Command::Query(task, Instant::now()), command_bytes)
        {
            QueryAdmission::Queued => {}
            QueryAdmission::Overloaded => return Err(CatalogError::Overloaded),
            QueryAdmission::Closed => return Err(CatalogError::Closed),
        }
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
        let owned_bytes = id
            .as_str()
            .len()
            .saturating_add(name.capacity())
            .saturating_add(vendor.as_ref().map_or(0, String::capacity));
        self.query_with_owned_bytes(owned_bytes, move |db| {
            db.ensure_camera(&id, &name, vendor.as_deref())
        })
        .await
    }

    /// Registers the configured recording roots used by root-specific
    /// retention queries and assigns pre-existing rows to those exact roots.
    pub async fn register_recording_root(
        &self,
        path: &Path,
        identity: &str,
    ) -> Result<(), CatalogError> {
        let path = path.to_path_buf();
        let identity = identity.to_owned();
        let owned_bytes = path_owned_bytes(&path).saturating_add(identity.len());
        self.query_with_owned_bytes(owned_bytes, move |db| {
            db.register_recording_root(&path, &identity)
        })
        .await
    }

    /// Assigns persisted rows to the exact configured roots used by
    /// root-specific retention queries.
    pub async fn configure_video_roots(&self, roots: &[PathBuf]) -> Result<(), CatalogError> {
        let roots = roots.to_vec();
        let owned_bytes = roots.iter().fold(
            roots.len().saturating_mul(size_of::<PathBuf>()),
            |sum, root| sum.saturating_add(path_owned_bytes(root)),
        );
        self.query_with_owned_bytes(owned_bytes, move |db| db.configure_video_roots(&roots))
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
        let owned_bytes = key_owned_bytes(&key);
        self.query_with_owned_bytes(owned_bytes, move |db| db.segments(&key, range))
            .await
    }

    /// At most `limit` segments of a stream overlapping `range`, ordered by start time.
    ///
    /// The SQL limit is applied before catalog rows and their owned strings are
    /// materialized, so serving callers can probe with `budget + 1` and reject
    /// excessive cardinality without first allocating the full result.
    pub async fn segments_limited(
        &self,
        key: &StreamKey,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<SegmentRow>, CatalogError> {
        let key = key.clone();
        let owned_bytes = key_owned_bytes(&key);
        self.query_with_owned_bytes(owned_bytes, move |db| {
            db.segments_limited(&key, range, limit)
        })
        .await
    }

    /// One segment by id.
    pub async fn segment(&self, id: SegmentId) -> Result<Option<SegmentRow>, CatalogError> {
        self.query(move |db| db.segment(id)).await
    }

    /// One segment by path.
    pub async fn segment_by_path(&self, path: &Path) -> Result<Option<SegmentRow>, CatalogError> {
        let path = path.to_path_buf();
        let owned_bytes = path_owned_bytes(&path);
        self.query_with_owned_bytes(owned_bytes, move |db| db.segment_by_path(&path))
            .await
    }

    /// Segments not yet sealed (open when the daemon last ran).
    pub async fn writing_segments(&self) -> Result<Vec<SegmentRow>, CatalogError> {
        self.query(Db::writing_segments).await
    }

    /// One stable bounded startup page of open segments.
    pub async fn writing_segments_page(
        &self,
        after: Option<(WallUs, SegmentId)>,
        limit: usize,
    ) -> Result<Vec<(SegmentRow, Option<PathBuf>)>, CatalogError> {
        self.query(move |db| db.writing_segments_page(after, limit))
            .await
    }

    /// The oldest sealed segments, candidates for retention.
    pub async fn oldest_sealed(&self, limit: usize) -> Result<Vec<SegmentRow>, CatalogError> {
        self.query(move |db| db.oldest_sealed(limit)).await
    }

    /// The sealed segment of `key` that ends last.
    pub async fn newest_sealed(&self, key: &StreamKey) -> Result<Option<SegmentRow>, CatalogError> {
        let key = key.clone();
        let owned_bytes = key_owned_bytes(&key);
        self.query_with_owned_bytes(owned_bytes, move |db| db.newest_sealed(&key))
            .await
    }

    /// Bytes of every segment in the catalog, open files at their written length.
    pub async fn total_bytes(&self) -> Result<u64, CatalogError> {
        self.query(Db::total_bytes).await
    }

    /// Deletes a segment row (the caller removes the files).
    pub async fn delete_segment(&self, id: SegmentId) -> Result<(), CatalogError> {
        self.query(move |db| db.delete_segment(id)).await
    }

    /// Durably marks a sealed segment for deletion before the caller mutates
    /// its media/index pair.
    pub async fn mark_deleting(&self, id: SegmentId) -> Result<(), CatalogError> {
        self.query(move |db| db.mark_deleting(id)).await
    }

    /// Durably marks an empty open segment for retryable startup cleanup.
    pub async fn mark_empty_writing_deleting(
        &self,
        id: SegmentId,
    ) -> Result<DeleteMarkResult, CatalogError> {
        self.query(move |db| db.mark_empty_writing_deleting(id))
            .await
    }

    /// Durably marks a sealed segment for deletion only if current event
    /// protection does not overlap it.
    ///
    /// The protection check and deletion-intent update share one FULL-synchronous
    /// transaction. A successful [`CatalogHandle::retain_event`] ordered before
    /// this call is therefore honored before this result is returned.
    pub async fn mark_deleting_if_unprotected(
        &self,
        id: SegmentId,
        now: WallUs,
        margin: Duration,
    ) -> Result<DeleteMarkResult, CatalogError> {
        let margin_us = i64::try_from(margin.as_micros()).unwrap_or(i64::MAX);
        self.query(move |db| db.mark_deleting_if_unprotected(id, now, margin_us))
            .await
    }

    /// Root-bound durable deletion mark. A row whose persisted root differs
    /// from `expected_root` is returned as [`DeleteMarkResult::NotFound`].
    pub async fn mark_deleting_if_unprotected_at_root(
        &self,
        id: SegmentId,
        expected_root: &Path,
        now: WallUs,
        margin: Duration,
    ) -> Result<DeleteMarkResult, CatalogError> {
        let expected_root = expected_root.to_path_buf();
        let owned_bytes = path_owned_bytes(&expected_root);
        let margin_us = i64::try_from(margin.as_micros()).unwrap_or(i64::MAX);
        self.query_with_owned_bytes(owned_bytes, move |db| {
            db.mark_deleting_if_unprotected_at_root(id, &expected_root, now, margin_us)
        })
        .await
    }

    /// Segments with deletion intent that startup must finish.
    pub async fn deleting_segments(&self) -> Result<Vec<SegmentRow>, CatalogError> {
        self.query(Db::deleting_segments).await
    }

    /// One stable bounded startup page of deletion intents.
    pub async fn deleting_segments_page(
        &self,
        after: Option<(WallUs, SegmentId)>,
        limit: usize,
    ) -> Result<Vec<(SegmentRow, Option<PathBuf>)>, CatalogError> {
        self.query(move |db| db.deleting_segments_page(after, limit))
            .await
    }

    /// Whether any ordinary or deletion-intent segment owns this exact path.
    pub async fn contains_segment_path(&self, path: &Path) -> Result<bool, CatalogError> {
        let path = path.to_path_buf();
        let owned_bytes = path_owned_bytes(&path);
        self.query_with_owned_bytes(owned_bytes, move |db| db.contains_segment_path(&path))
            .await
    }

    /// Persisted recording-root spelling for one segment id.
    pub async fn segment_root(&self, id: SegmentId) -> Result<Option<PathBuf>, CatalogError> {
        self.query(move |db| db.segment_root(id)).await
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
        let owned_bytes = path_owned_bytes(&path);
        self.query_with_owned_bytes(owned_bytes, move |db| {
            db.mark_sealed(&path, len, fragments, wall_end, media_end)
        })
        .await
    }

    /// Adopts a sealed file found on disk.
    pub async fn adopt_segment(&self, row: NewSegment) -> Result<SegmentId, CatalogError> {
        let owned_bytes = new_segment_owned_bytes(&row);
        self.query_with_owned_bytes(owned_bytes, move |db| db.adopt_segment(&row))
            .await
    }

    /// Events in `range`, oldest first, at most `limit`.
    pub async fn events(
        &self,
        camera: Option<&CameraId>,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<EventRow>, CatalogError> {
        self.events_after(camera, range, None, limit).await
    }

    /// Events in `range` after an optional `(receipt time, id)` cursor, oldest
    /// first, at most `limit`.
    ///
    /// A cursor is exclusive. Use the final row of one page as the next
    /// cursor so pages split inside an equal-timestamp group without repeats
    /// or omissions.
    pub async fn events_after(
        &self,
        camera: Option<&CameraId>,
        range: TimeRange,
        after: Option<EventCursor>,
        limit: usize,
    ) -> Result<Vec<EventRow>, CatalogError> {
        let camera = camera.cloned();
        let owned_bytes = camera.as_ref().map_or(0, |id| id.as_str().len());
        self.query_with_owned_bytes(owned_bytes, move |db| {
            db.events(camera.as_ref(), range, after, limit)
        })
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

    /// A bounded page of active event-protection intervals containing no
    /// vendor payload, source, state, or kind strings.
    pub async fn event_protections(
        &self,
        now: WallUs,
        after: Option<EventProtectionCursor>,
        limit: usize,
    ) -> Result<Vec<EventProtection>, CatalogError> {
        self.query(move |db| db.event_protections(now, after, limit))
            .await
    }

    /// The oldest sealed segments after the `(start time, id)` cursor, at most
    /// `limit`. The id makes pagination lossless when start times are equal.
    pub async fn oldest_sealed_after(
        &self,
        after: (WallUs, SegmentId),
        limit: usize,
    ) -> Result<Vec<SegmentRow>, CatalogError> {
        self.query(move |db| db.oldest_sealed_after(after, limit))
            .await
    }

    /// Root-aware sealed retention page after an optional stable cursor.
    ///
    /// Raw indexed input is capped before event-protection filtering. The
    /// returned cursor and scanned count therefore advance even when every
    /// inspected row is protected.
    pub async fn retention_candidates(
        &self,
        root: Option<&Path>,
        after: Option<(WallUs, SegmentId)>,
        now: WallUs,
        margin: Duration,
        limit: usize,
    ) -> Result<RetentionPage, CatalogError> {
        let root = root.map(Path::to_path_buf);
        let owned_bytes = root.as_deref().map_or(0, path_owned_bytes);
        let margin_us = i64::try_from(margin.as_micros()).unwrap_or(i64::MAX);
        self.query_with_owned_bytes(owned_bytes, move |db| {
            db.retention_candidates(root.as_deref(), after, now, margin_us, limit)
        })
        .await
    }

    /// Expired sealed retention page for one stream after an optional stable
    /// cursor, with raw scan progress independent of protection filtering.
    pub async fn expired_candidates(
        &self,
        key: &StreamKey,
        cutoff: WallUs,
        after: Option<(WallUs, SegmentId)>,
        now: WallUs,
        margin: Duration,
        limit: usize,
    ) -> Result<RetentionPage, CatalogError> {
        let key = key.clone();
        let owned_bytes = key_owned_bytes(&key);
        let margin_us = i64::try_from(margin.as_micros()).unwrap_or(i64::MAX);
        self.query_with_owned_bytes(owned_bytes, move |db| {
            db.expired_candidates(&key, cutoff, after, now, margin_us, limit)
        })
        .await
    }

    /// Inserts an event and returns its id.
    pub async fn insert_event(&self, event: &Event) -> Result<EventId, CatalogError> {
        let event = event.clone().bounded();
        let owned_bytes = event_owned_bytes(&event);
        self.query_with_owned_bytes(owned_bytes, move |db| db.insert_event(&event))
            .await
    }

    /// Runs one bounded event-lifecycle maintenance step using the recording
    /// horizon and fixed state-database row, byte, and age ceilings.
    pub async fn maintain_events(&self, now: WallUs) -> Result<(), CatalogError> {
        self.query(move |db| db.maintain_events(now)).await
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
        self.inner.queue.close();
        if handle.join().is_err() {
            error!("catalog thread panicked");
        }
    }
}

/// The thread body: batches changes, runs queries in order, stops on request.
fn run(db: &mut Db, queue: &CommandQueue, metrics: &CatalogMetrics) {
    let mut pending: Vec<Change> = Vec::new();
    let mut pending_bytes = 0usize;
    let mut deadline: Option<Instant> = None;
    loop {
        let wait = deadline.map_or(IDLE_WAIT, |d| d.saturating_duration_since(Instant::now()));
        match queue.receive(wait) {
            Receive::Command(QueuedCommand {
                command: Command::Change(change),
                bytes,
            }) => {
                if pending.is_empty() {
                    deadline = Some(Instant::now() + BATCH_INTERVAL);
                }
                pending.push(change);
                pending_bytes = pending_bytes.saturating_add(bytes);
                if pending.len() >= MAX_BATCH {
                    flush(
                        db,
                        &mut pending,
                        &mut pending_bytes,
                        &mut deadline,
                        queue,
                        metrics,
                    );
                }
            }
            Receive::Command(QueuedCommand {
                command: Command::Query(query, sent),
                bytes,
            }) => {
                metrics.wait().record(sent.elapsed());
                flush(
                    db,
                    &mut pending,
                    &mut pending_bytes,
                    &mut deadline,
                    queue,
                    metrics,
                );
                query(db);
                queue.complete(1, bytes);
            }
            Receive::Closed => {
                flush(
                    db,
                    &mut pending,
                    &mut pending_bytes,
                    &mut deadline,
                    queue,
                    metrics,
                );
                return;
            }
            Receive::Timeout => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    flush(
                        db,
                        &mut pending,
                        &mut pending_bytes,
                        &mut deadline,
                        queue,
                        metrics,
                    );
                }
            }
        }
    }
}

fn flush(
    db: &mut Db,
    pending: &mut Vec<Change>,
    pending_bytes: &mut usize,
    deadline: &mut Option<Instant>,
    queue: &CommandQueue,
    metrics: &CatalogMetrics,
) {
    *deadline = None;
    if pending.is_empty() {
        return;
    }
    let started = Instant::now();
    let result = db.apply(pending);
    let elapsed = started.elapsed();
    match result {
        Ok(outcome) => {
            let committed_changes = pending.len().saturating_sub(outcome.rejected_events);
            metrics.record_batch(committed_changes, elapsed, true);
            metrics.reject_events(outcome.rejected_events);
            debug!(
                changes = committed_changes,
                rejected_events = outcome.rejected_events,
                "catalog batch committed"
            );
            let committed = WallUs::now();
            for nvr_time in outcome.committed_events {
                let delay = committed.get().saturating_sub(nvr_time.get()).max(0);
                metrics
                    .event_commit()
                    .record(Duration::from_micros(u64::try_from(delay).unwrap_or(0)));
            }
        }
        Err(e) => {
            metrics.record_batch(pending.len(), elapsed, false);
            error!(changes = pending.len(), error = ?e, "catalog batch failed");
        }
    }
    let count = pending.len();
    pending.clear();
    queue.complete(count, *pending_bytes);
    *pending_bytes = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_core::event::{EventKind, EventSource, EventState};

    fn event(data_len: usize) -> Change {
        let data = match data_len {
            0 => String::new(),
            1 => "0".to_owned(),
            length => format!("\"{}\"", "x".repeat(length - 2)),
        };
        assert_eq!(data.len(), data_len);
        Change::Event(Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: EventSource::DahuaCgi,
            kind: EventKind::Motion,
            state: EventState::Pulse,
            nvr_time: WallUs::new(1),
            camera_time: None,
            data: Some(data),
        })
    }

    fn progress(value: u64) -> Change {
        Change::SegmentProgress {
            path: PathBuf::from("/video/open.mp4"),
            durable_len: value,
            fragments: u32::try_from(value).unwrap(),
            wall_end: WallUs::new(i64::try_from(value).unwrap()),
            media_end: value,
        }
    }

    #[test]
    fn stalled_consumer_stays_within_count_and_owned_byte_budgets() {
        let metrics = Arc::new(CatalogMetrics::new());
        let queue = CommandQueue::new(Arc::clone(&metrics));
        let mut queued = 0usize;
        for _ in 0..1_000 {
            match queue.push_change(event(iris_core::event::MAX_EVENT_DATA_LEN)) {
                ChangeAdmission::Queued => queued += 1,
                ChangeAdmission::Dropped => {}
                other => panic!("unexpected admission result: {}", admission_name(&other)),
            }
        }
        assert!(queued > 0);
        assert!(metrics.queue_depth() <= MAX_QUEUED_COMMANDS as u64);
        assert!(metrics.queue_bytes() <= MAX_QUEUED_BYTES as u64);
        assert!(metrics.rejected_events() > 0);

        let query: Query = Box::new(|_| {});
        assert!(matches!(
            queue.push_query(Command::Query(query, Instant::now()), QUERY_OVERHEAD),
            QueryAdmission::Overloaded
        ));

        assert!(matches!(
            queue.push_change(Change::SegmentSealed {
                path: PathBuf::from("/video/critical.mp4"),
                len: 1,
                fragments: 1,
                wall_end: WallUs::new(2),
                media_end: 1,
            }),
            ChangeAdmission::Queued
        ));
        let mut saw_critical = false;
        while let Receive::Command(command) = queue.receive(Duration::ZERO) {
            saw_critical |= matches!(
                command.command,
                Command::Change(Change::SegmentSealed { .. })
            );
            queue.complete(1, command.bytes);
        }
        assert!(saw_critical, "reserved lifecycle command was not retained");
        assert_eq!(metrics.queue_depth(), 0);
        assert_eq!(metrics.queue_bytes(), 0);
    }

    #[test]
    fn progress_is_coalesced_only_within_the_unqueried_tail() {
        let metrics = Arc::new(CatalogMetrics::new());
        let queue = CommandQueue::new(Arc::clone(&metrics));
        assert!(matches!(
            queue.push_change(progress(1)),
            ChangeAdmission::Queued
        ));
        assert!(matches!(
            queue.push_change(progress(2)),
            ChangeAdmission::Coalesced
        ));
        assert_eq!(metrics.queue_depth(), 1);
        assert_eq!(metrics.coalesced(), 1);
        let Receive::Command(command) = queue.receive(Duration::ZERO) else {
            panic!("coalesced progress missing");
        };
        let Command::Change(Change::SegmentProgress { durable_len, .. }) = command.command else {
            panic!("wrong command after progress coalescing");
        };
        assert_eq!(durable_len, 2);
        queue.complete(1, command.bytes);
    }

    #[test]
    fn exhausted_lifecycle_reserve_requests_reconciliation() {
        let metrics = Arc::new(CatalogMetrics::new());
        let queue = CommandQueue::new(Arc::clone(&metrics));
        for index in 0..MAX_QUEUED_COMMANDS {
            assert!(matches!(
                queue.push_change(Change::SegmentSealed {
                    path: PathBuf::from(format!("/video/{index}.mp4")),
                    len: 1,
                    fragments: 1,
                    wall_end: WallUs::new(2),
                    media_end: 1,
                }),
                ChangeAdmission::Queued
            ));
        }
        assert!(matches!(
            queue.push_change(Change::SegmentSealed {
                path: PathBuf::from("/video/overflow.mp4"),
                len: 1,
                fragments: 1,
                wall_end: WallUs::new(2),
                media_end: 1,
            }),
            ChangeAdmission::Reconcile
        ));
        assert_eq!(metrics.rejected_critical(), 1);
        assert!(metrics.reconciliation_required());
    }

    #[test]
    fn lifecycle_eviction_never_crosses_a_query_barrier() {
        let metrics = Arc::new(CatalogMetrics::new());
        let queue = CommandQueue::new(Arc::clone(&metrics));
        assert!(matches!(
            queue.push_change(event(1)),
            ChangeAdmission::Queued
        ));
        let query: Query = Box::new(|_| {});
        assert!(matches!(
            queue.push_query(Command::Query(query, Instant::now()), QUERY_OVERHEAD),
            QueryAdmission::Queued
        ));
        for index in 0..(MAX_QUEUED_COMMANDS - 2) {
            assert!(matches!(
                queue.push_change(Change::SegmentSealed {
                    path: PathBuf::from(format!("/video/{index}.mp4")),
                    len: 1,
                    fragments: 1,
                    wall_end: WallUs::new(2),
                    media_end: 1,
                }),
                ChangeAdmission::Queued
            ));
        }
        assert!(matches!(
            queue.push_change(Change::SegmentSealed {
                path: PathBuf::from("/video/overflow.mp4"),
                len: 1,
                fragments: 1,
                wall_end: WallUs::new(2),
                media_end: 1,
            }),
            ChangeAdmission::Reconcile
        ));
        let state = queue.state.lock().unwrap();
        assert!(matches!(
            &state.commands[0].command,
            Command::Change(Change::Event(_))
        ));
        assert!(matches!(&state.commands[1].command, Command::Query(_, _)));
        assert_eq!(metrics.rejected_events(), 0);
    }

    #[test]
    fn event_boundary_releases_unaccounted_backing_capacity() {
        let metrics = Arc::new(CatalogMetrics::new());
        let queue = CommandQueue::new(Arc::clone(&metrics));
        let mut value = event(1);
        let Change::Event(event) = &mut value else {
            unreachable!();
        };
        let data = event.data.as_mut().unwrap();
        data.reserve(MAX_QUEUED_BYTES * 2);
        assert!(data.capacity() > MAX_QUEUED_BYTES);
        assert!(matches!(queue.push_change(value), ChangeAdmission::Queued));
        assert!(metrics.queue_bytes() < 1024 * 1024);
    }

    #[test]
    fn dropping_the_final_handle_closes_and_joins_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("drop.db")).unwrap();
        let queue = Arc::downgrade(&catalog.inner.queue);
        drop(catalog);
        assert!(queue.upgrade().is_none());
    }

    fn admission_name(admission: &ChangeAdmission) -> &'static str {
        match admission {
            ChangeAdmission::Queued => "queued",
            ChangeAdmission::Coalesced => "coalesced",
            ChangeAdmission::Dropped => "dropped",
            ChangeAdmission::Closed => "closed",
            ChangeAdmission::Reconcile => "reconcile",
        }
    }
}
