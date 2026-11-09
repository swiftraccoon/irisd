//! Low-latency HLS from the live ring: a live playlist with partial segments
//! built in memory, blocking playlist reload, and preload hints
//! (`docs/standards/ietf/draft-pantos-hls-rfc8216bis-22.txt` §4.4.3.1,
//! §4.4.3.7–8, §4.4.4.9, §4.4.5.3, §6.2.1–6.2.2, §6.2.5.2).
//!
//! One feeder task per requested stream subscribes to the stream's
//! [`LiveRing`], starts from the frames since the ring's last sync frame, and
//! keeps the frames of the last groups of pictures: each GOP is one media
//! segment (its media sequence number is assigned when its sync frame
//! arrives), and each segment is cut into partial segments of a fixed frame
//! count pinned from the first frame's duration, so a part's content never
//! changes once published and every non-final part lasts the declared part
//! target. The target duration is pinned when the first GOP completes and
//! never changes for that feeder; a later GOP that would outlast it is cut
//! with a discontinuity, as a GOP over the payload or frame budget is.
//! Complete segments stay listed until the complete segments after them
//! cover three target durations, so a player always has that much to choose
//! from and an unlisted part stays downloadable for as long. Segments and
//! parts are served from memory as sequences of single-sample fragments,
//! exactly what the WebSocket feed sends; their URIs end in `.m4s`, an
//! extension `ffmpeg`'s HLS demuxer allows for segments. The feeder stops when
//! nobody has asked for the stream for [`IDLE_TIMEOUT`], and a later request
//! starts a fresh one.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Extension, Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use iris_core::id::{CameraId, StreamKey};
use iris_core::time::{TIMESCALE, WallUs};
use iris_ingest::{LiveError, LiveFrame, LiveRing};
use iris_mp4::init::{SampleEntry, build_init_segment};
use serde::Deserialize;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthContext;
use crate::error::ApiError;
use crate::live::{encode_live_fragment_into, same_sample_entry};
use crate::media_admission::{self, MediaPermit};
use crate::segments::stream_key;
use crate::state::{ApiState, MediaClass};

/// Longest partial segment in media ticks (0.5 s): a part holds as many
/// whole frames as fit, at least one.
const PART_TARGET_TICKS: u64 = 45_000;
/// Complete segments a feeder keeps at most, whatever the target duration:
/// the memory bound of one watched stream is this many GOPs.
const MAX_KEPT_GOPS: usize = 12;
/// Target durations of complete segments a feeder keeps behind the newest
/// (§6.2.2: a segment is removed only when at least this much remains).
const WINDOW_TARGET_DURATIONS: u64 = 3;
/// Added to the first complete GOP's duration when the target duration is
/// pinned, so a GOP one frame longer than the first is not cut.
const TARGET_HEADROOM_TICKS: u64 = TIMESCALE as u64;
/// Segments that list their partial segments: the last ones, as the
/// specification asks for parts within three target durations of the end.
const SEGMENTS_WITH_PARTS: usize = 3;
/// Most payload retained by one GOP.
const MAX_GOP_PAYLOAD_BYTES: usize = 16 * 1_024 * 1_024;
/// Most frames retained by one GOP.
const MAX_GOP_FRAMES: usize = 600;
/// Longest media duration retained by one GOP (ten seconds at 90 kHz), and
/// the longest target duration a feeder pins.
const MAX_GOP_DURATION_TICKS: u64 = 10 * TIMESCALE as u64;
/// A feeder with no request for this long stops.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a blocking request waits before the first GOP completes: the GOP
/// budget plus slack, since a feeder seeded from the ring's last sync frame
/// completes its first GOP within one GOP length. Once the target duration is
/// pinned, requests wait three target durations (§6.2.5.2).
const INITIAL_BLOCK_TIMEOUT: Duration = Duration::from_secs(12);
/// A reload may ask for a part this many seconds past the newest one before
/// the request is an error (§6.2.5.2, the Advance Part Limit of three
/// seconds divided by the part target).
const ADVANCE_PART_LIMIT_SECS: u64 = 3;

/// The frames of one group of pictures, one media segment.
#[derive(Clone, Debug)]
struct Gop {
    msn: u64,
    /// Number of timeline discontinuities before this GOP.
    discontinuity_sequence: u64,
    /// Frames with the `mfhd` sequence number each was encoded with.
    frames: Vec<(Arc<LiveFrame>, u32)>,
    payload_bytes: usize,
    duration_ticks: u64,
    complete: bool,
}

impl Gop {
    /// Partial segments available so far, `frames_per_part` frames each:
    /// `(index, frame range, complete)`.
    fn parts(&self, frames_per_part: usize) -> Vec<Part> {
        let n = frames_per_part.max(1);
        let mut parts = Vec::new();
        let mut start = 0usize;
        while start < self.frames.len() {
            let end = (start + n).min(self.frames.len());
            let complete = end - start == n || self.complete;
            parts.push(Part {
                index: parts.len(),
                range: start..end,
                complete,
            });
            start = end;
        }
        parts
    }

    fn duration_ticks(&self) -> u64 {
        self.duration_ticks
    }

    /// Whether `frame` would take this GOP past the frame or payload budget
    /// or past `max_duration_ticks` of media.
    fn would_exceed_budget(&self, frame: &LiveFrame, max_duration_ticks: u64) -> bool {
        self.frames.len() >= MAX_GOP_FRAMES
            || self
                .payload_bytes
                .checked_add(frame.data.len())
                .is_none_or(|bytes| bytes > MAX_GOP_PAYLOAD_BYTES)
            || self
                .duration_ticks
                .checked_add(u64::from(frame.duration))
                .is_none_or(|ticks| ticks > max_duration_ticks)
    }

    fn push(&mut self, frame: Arc<LiveFrame>, sequence: u32) {
        self.payload_bytes += frame.data.len();
        self.duration_ticks += u64::from(frame.duration);
        self.frames.push((frame, sequence));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Part {
    index: usize,
    range: std::ops::Range<usize>,
    complete: bool,
}

#[derive(Clone, Debug, Default)]
struct Inner {
    gops: VecDeque<Gop>,
    next_msn: u64,
    next_seq: u32,
    init: Option<Bytes>,
    /// The sample entry `init` was built from.
    init_entry: Option<Arc<SampleEntry>>,
    last_decode_time: Option<u64>,
    discontinuity_sequence: u64,
    /// The declared target duration in ticks, pinned when the first GOP
    /// completes; `None` before.
    target_ticks: Option<u64>,
    /// Frames per partial segment, pinned from the first accepted frame.
    part_frames: Option<usize>,
    /// The declared part target in ticks: `part_frames` times the first
    /// frame's duration.
    part_target_ticks: Option<u64>,
    last_access: Option<Instant>,
    closed: bool,
}

impl Inner {
    /// Frames per partial segment: the pinned count, or one.
    fn frames_per_part(&self) -> usize {
        self.part_frames.unwrap_or(1)
    }

    /// The declared part target in ticks: the pinned value, or the default.
    fn part_target_ticks(&self) -> u64 {
        self.part_target_ticks.unwrap_or(PART_TARGET_TICKS)
    }

    /// The declared target duration in whole seconds, at least one.
    fn target_secs(&self) -> u64 {
        self.target_ticks
            .unwrap_or(2 * u64::from(TIMESCALE))
            .div_ceil(u64::from(TIMESCALE))
            .max(1)
    }

    /// The most media a GOP may hold before it is cut: the pinned target, or
    /// the GOP budget before any GOP completed.
    fn max_gop_ticks(&self) -> u64 {
        self.target_ticks.unwrap_or(MAX_GOP_DURATION_TICKS)
    }

    /// How long a blocking request waits: three target durations once the
    /// target is pinned.
    fn block_timeout(&self) -> Duration {
        match self.target_ticks {
            Some(_) => Duration::from_secs(3 * self.target_secs()),
            None => INITIAL_BLOCK_TIMEOUT,
        }
    }

    /// Pins the part size from the first frame: as many frames as fit the
    /// part target, at least one, and the target they add up to.
    fn pin_part_target(&mut self, frame: &LiveFrame) {
        if self.part_frames.is_some() {
            return;
        }
        let duration = u64::from(frame.duration.max(1));
        let frames = usize::try_from(PART_TARGET_TICKS / duration)
            .unwrap_or(1)
            .max(1);
        self.part_frames = Some(frames);
        self.part_target_ticks = Some(frames as u64 * duration);
    }

    /// Marks the open GOP complete, pinning the target duration from the
    /// first one: its duration rounded up to whole seconds plus one second of
    /// headroom, capped at the GOP budget. An open GOP without frames is
    /// dropped instead.
    fn complete_current(&mut self) {
        let Some(current) = self.gops.back_mut() else {
            return;
        };
        if current.complete {
            return;
        }
        if current.frames.is_empty() {
            self.gops.pop_back();
            return;
        }
        current.complete = true;
        if self.target_ticks.is_none() {
            let ticks = u64::from(TIMESCALE);
            let rounded = current.duration_ticks.div_ceil(ticks) * ticks;
            self.target_ticks = Some((rounded + TARGET_HEADROOM_TICKS).min(MAX_GOP_DURATION_TICKS));
        }
    }

    /// Drops the oldest complete GOPs while the complete GOPs after them
    /// cover at least three target durations, and never keeps more than
    /// [`MAX_KEPT_GOPS`] complete GOPs.
    fn evict(&mut self) {
        let window = WINDOW_TARGET_DURATIONS * self.target_secs() * u64::from(TIMESCALE);
        loop {
            let complete = self.gops.iter().filter(|g| g.complete).count();
            let Some(oldest) = self.gops.front() else {
                return;
            };
            if !oldest.complete {
                return;
            }
            let newer: u64 = self
                .gops
                .iter()
                .skip(1)
                .filter(|g| g.complete)
                .map(Gop::duration_ticks)
                .sum();
            if newer >= window || complete > MAX_KEPT_GOPS {
                self.gops.pop_front();
            } else {
                return;
            }
        }
    }

    /// Ends the open GOP where it is and starts a discontinuity epoch, so
    /// dependent frames are ignored until the next sync frame.
    fn cut(&mut self, waiting_for_sync: &mut bool) {
        self.complete_current();
        if self.last_decode_time.take().is_some() {
            self.discontinuity_sequence = self.discontinuity_sequence.saturating_add(1);
        }
        *waiting_for_sync = true;
    }

    fn on_lag(&mut self, waiting_for_sync: &mut bool) {
        self.cut(waiting_for_sync);
    }

    fn accept_frame(&mut self, frame: Arc<LiveFrame>, waiting_for_sync: &mut bool) {
        if !same_sample_entry(self.init_entry.as_ref(), &frame.sample_entry) {
            // Every listed segment refers to the one `EXT-X-MAP`, so a new
            // sample entry starts the window over.
            if self.init_entry.is_some() {
                self.discontinuity_sequence = self.discontinuity_sequence.saturating_add(1);
            }
            self.init = Some(build_init_segment(&frame.sample_entry));
            self.init_entry = Some(Arc::clone(&frame.sample_entry));
            self.gops.clear();
            self.last_decode_time = None;
            *waiting_for_sync = true;
        }
        if !*waiting_for_sync
            && self
                .last_decode_time
                .is_some_and(|previous| frame.decode_time <= previous)
        {
            self.cut(waiting_for_sync);
        }
        self.last_decode_time = Some(frame.decode_time);
        if frame.is_sync {
            self.pin_part_target(&frame);
            self.complete_current();
            let msn = self.next_msn;
            self.next_msn = self.next_msn.saturating_add(1);
            self.gops.push_back(Gop {
                msn,
                discontinuity_sequence: self.discontinuity_sequence,
                frames: Vec::new(),
                payload_bytes: 0,
                duration_ticks: 0,
                complete: false,
            });
            self.evict();
            *waiting_for_sync = false;
        }
        if *waiting_for_sync {
            return;
        }
        if self
            .gops
            .back()
            .is_some_and(|current| current.would_exceed_budget(&frame, self.max_gop_ticks()))
        {
            // Cut the over-budget GOP where it is exactly once, then ignore
            // dependent frames until a later sync starts a bounded GOP.
            self.cut(waiting_for_sync);
            return;
        }
        let sequence = self.next_seq.wrapping_add(1);
        self.next_seq = sequence;
        if let Some(current) = self.gops.back_mut() {
            current.push(frame, sequence);
        }
    }

    fn close(&mut self) {
        self.gops.clear();
        self.init = None;
        self.init_entry = None;
        self.last_decode_time = None;
        self.closed = true;
    }
}

/// The live playlist state of one stream.
#[derive(Debug)]
struct LiveHls {
    inner: Mutex<Inner>,
    notify: Notify,
    touched: Notify,
}

impl LiveHls {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            notify: Notify::new(),
            touched: Notify::new(),
        })
    }

    fn touch(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last_access = Some(Instant::now());
        }
        self.touched.notify_one();
    }

    /// How long a blocking request on this stream waits.
    fn block_timeout(&self) -> Duration {
        self.inner
            .lock()
            .map_or(INITIAL_BLOCK_TIMEOUT, |inner| inner.block_timeout())
    }

    /// Feeds frames from `ring` until the ring closes or nobody asks for the
    /// stream for [`IDLE_TIMEOUT`]. The frames since the ring's last sync
    /// frame come first, so the first GOP completes within one GOP length.
    async fn feed(self: Arc<Self>, ring: LiveRing, shutdown: CancellationToken) {
        let mut subscription = ring.subscribe();
        let mut waiting_for_sync = true;
        // The daemon runs one thread, so no frame is published between the
        // subscription and this snapshot; a frame in both would repeat.
        for frame in ring.recent_from_sync() {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            inner.accept_frame(frame, &mut waiting_for_sync);
        }
        loop {
            let touched = self.touched.notified();
            tokio::pin!(touched);
            touched.as_mut().enable();
            let idle_deadline = self
                .inner
                .lock()
                .ok()
                .and_then(|inner| inner.last_access)
                .unwrap_or_else(Instant::now)
                + IDLE_TIMEOUT;
            let next = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                () = touched => continue,
                () = tokio::time::sleep_until(idle_deadline) => {
                    let expired = self.inner.lock().map_or(true, |inner| {
                        inner.last_access.is_none_or(|last| {
                            Instant::now().saturating_duration_since(last) >= IDLE_TIMEOUT
                        })
                    });
                    if expired {
                        break;
                    }
                    continue;
                }
                next = subscription.next() => next,
            };
            match next {
                Ok(frame) => {
                    let Ok(mut inner) = self.inner.lock() else {
                        break;
                    };
                    inner.accept_frame(frame, &mut waiting_for_sync);
                }
                Err(LiveError::Lagged(_)) => {
                    let Ok(mut inner) = self.inner.lock() else {
                        break;
                    };
                    inner.on_lag(&mut waiting_for_sync);
                }
                Err(LiveError::Closed) => break,
            }
            self.notify.notify_waiters();
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.close();
        }
        self.notify.notify_waiters();
    }

    /// Waits until `ready` holds, re-checking on every new frame, for at
    /// most `timeout`; `Ok(false)` on timeout.
    async fn wait_until(
        &self,
        timeout: Duration,
        mut ready: impl FnMut(&Inner) -> bool,
    ) -> Result<bool, ApiError> {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let inner = self.inner.lock().map_err(|_| ApiError::NotFound)?;
                if ready(&inner) {
                    return Ok(true);
                }
                if inner.closed {
                    return Err(ApiError::NotFound);
                }
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep_until(deadline) => return Ok(false),
            }
        }
    }
}

/// Live playlist states by stream, started on demand.
#[derive(Debug, Default)]
pub(crate) struct LiveHlsRegistry {
    streams: Mutex<HashMap<StreamKey, Arc<LiveHls>>>,
}

impl LiveHlsRegistry {
    /// Forgets the states of `camera`'s streams. A feeder whose state is
    /// forgotten stops once its ring ends or its idle time runs out.
    pub(crate) fn remove_camera(&self, camera: &CameraId) {
        self.streams
            .lock()
            .expect("live hls lock")
            .retain(|key, _| key.camera != *camera);
    }

    /// The state for `key`, starting a feeder on `ring` when there is none
    /// or the previous one stopped.
    fn get_or_start(
        &self,
        key: &StreamKey,
        ring: LiveRing,
        shutdown: CancellationToken,
    ) -> Arc<LiveHls> {
        let mut streams = self.streams.lock().expect("live hls lock");
        if let Some(existing) = streams.get(key)
            && !existing.inner.lock().is_ok_and(|i| i.closed)
        {
            existing.touch();
            return Arc::clone(existing);
        }
        let hls = LiveHls::new();
        hls.touch();
        tokio::spawn(Arc::clone(&hls).feed(ring, shutdown));
        streams.insert(key.clone(), Arc::clone(&hls));
        hls
    }
}

fn lookup(state: &ApiState, camera: &str, kind: &str) -> Result<Arc<LiveHls>, ApiError> {
    let key = stream_key(camera, kind)?;
    let ring = state.live.get(&key).ok_or(ApiError::NotFound)?;
    Ok(state
        .live
        .hls()
        .get_or_start(&key, ring, state.shutdown.clone()))
}

/// `_HLS_msn` and `_HLS_part` as they arrive.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ReloadParams {
    #[serde(rename = "_HLS_msn")]
    msn: Option<u64>,
    #[serde(rename = "_HLS_part")]
    part: Option<usize>,
}

/// `GET /api/streams/{camera}/{kind}/hls/live.m3u8`: the live playlist, with
/// blocking reload through `_HLS_msn` and `_HLS_part`.
pub(crate) async fn playlist(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<ReloadParams>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    let hls = lookup(&state, &camera, &kind)?;
    if params.part.is_some() && params.msn.is_none() {
        return Err(ApiError::BadRequest("_HLS_part without _HLS_msn".into()));
    }
    // A client may ask for a segment up to two beyond the newest, and for a
    // part up to the advance part limit beyond a segment's newest; further is
    // an error (§6.2.5.2).
    if let Some(msn) = params.msn {
        let inner = hls.inner.lock().map_err(|_| ApiError::NotFound)?;
        let last = inner.gops.back().map(|g| g.msn);
        if last.is_some_and(|last| msn > last + 2) {
            return Err(ApiError::BadRequest(format!(
                "_HLS_msn {msn} is too far ahead"
            )));
        }
        if let (Some(part), Some(gop)) = (params.part, inner.gops.iter().find(|g| g.msn == msn))
            && part > gop.parts(inner.frames_per_part()).len() + advance_part_limit(&inner)
        {
            return Err(ApiError::BadRequest(format!(
                "_HLS_part {part} is too far ahead"
            )));
        }
    }
    // A playlist needs a segment to play from: the first request waits for
    // the first complete GOP (within one GOP length, the feeder being seeded
    // from the ring's last sync frame), later ones answer at once unless a
    // directive asks for more.
    let ready = hls
        .wait_until(hls.block_timeout(), |inner| {
            inner.gops.iter().any(|g| g.complete)
                && params
                    .msn
                    .is_none_or(|msn| reload_satisfied(inner, msn, params.part))
        })
        .await?;
    if !ready {
        return Err(ApiError::Unavailable(
            "the live playlist did not advance".into(),
        ));
    }
    let snapshot = hls.inner.lock().map_err(|_| ApiError::NotFound)?.clone();
    let base_path = format!("/api/streams/{camera}/{kind}/hls");
    let text = render(&snapshot, &auth, &base_path);
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        text,
    )
        .into_response())
}

/// How many parts past a segment's newest a reload may ask for: three
/// seconds of parts (§6.2.5.2).
fn advance_part_limit(inner: &Inner) -> usize {
    let per_second = u64::from(TIMESCALE).div_ceil(inner.part_target_ticks().max(1));
    usize::try_from(ADVANCE_PART_LIMIT_SECS * per_second.max(1)).unwrap_or(usize::MAX)
}

/// Whether the playlist holds segment `msn` (complete, or with part `part`
/// complete). A part index past the segment's last part means part 0 of
/// the next segment (§6.2.5.2).
fn reload_satisfied(inner: &Inner, msn: u64, part: Option<usize>) -> bool {
    let Some(gop) = inner.gops.iter().find(|g| g.msn == msn) else {
        // Already gone from the playlist, or not yet started: served when newer exists.
        return inner.gops.back().is_some_and(|g| g.msn > msn);
    };
    let frames_per_part = inner.frames_per_part();
    match part {
        None => gop.complete,
        Some(index) => {
            let parts = gop.parts(frames_per_part);
            match parts.get(index) {
                Some(p) => p.complete,
                None if gop.complete => inner.gops.iter().any(|g| {
                    g.msn == msn + 1 && g.parts(frames_per_part).first().is_some_and(|p| p.complete)
                }),
                None => false,
            }
        }
    }
}

fn uri(path: &str, auth: &AuthContext, base_path: &str) -> String {
    match auth.capability(&format!("{base_path}/{path}")) {
        Some(capability) => format!("{path}?token={capability}"),
        None => path.to_owned(),
    }
}

fn seconds(ticks: u64) -> f64 {
    ticks as f64 / f64::from(TIMESCALE)
}

/// Renders the live playlist. URIs are relative to the playlist's own path.
fn render(inner: &Inner, auth: &AuthContext, base_path: &str) -> String {
    let target = inner.target_secs();
    let part_target = seconds(inner.part_target_ticks());
    let frames_per_part = inner.frames_per_part();
    let first_msn = inner.gops.front().map_or(0, |g| g.msn);
    let first_discontinuity = inner
        .gops
        .front()
        .map_or(inner.discontinuity_sequence, |g| g.discontinuity_sequence);
    let mut out = String::new();
    // Version 6 is what EXT-X-MAP without I-frame playlists needs (§8).
    let _ = writeln!(
        out,
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:{target}"
    );
    let _ = writeln!(
        out,
        "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK={:.3}",
        part_target * 3.0
    );
    let _ = writeln!(out, "#EXT-X-PART-INF:PART-TARGET={part_target:.3}");
    let _ = writeln!(
        out,
        "#EXT-X-MEDIA-SEQUENCE:{first_msn}\n#EXT-X-DISCONTINUITY-SEQUENCE:{first_discontinuity}\n#EXT-X-INDEPENDENT-SEGMENTS"
    );
    let _ = writeln!(
        out,
        "#EXT-X-MAP:URI=\"{}\"",
        uri("init.mp4", auth, base_path)
    );
    let with_parts_from = inner.gops.len().saturating_sub(SEGMENTS_WITH_PARTS);
    let mut discontinuity_sequence = first_discontinuity;
    for (i, gop) in inner.gops.iter().enumerate() {
        let after_discontinuity = gop.discontinuity_sequence != discontinuity_sequence;
        if after_discontinuity {
            out.push_str("#EXT-X-DISCONTINUITY\n");
            discontinuity_sequence = gop.discontinuity_sequence;
        }
        // The first segment and every segment after a discontinuity carry
        // their wall time (§6.2.1).
        if let Some((first, _)) = gop.frames.first()
            && (i == 0 || after_discontinuity)
        {
            let _ = writeln!(out, "#EXT-X-PROGRAM-DATE-TIME:{}", rfc3339(first.wall));
        }
        if i >= with_parts_from {
            for part in gop
                .parts(frames_per_part)
                .into_iter()
                .filter(|p| p.complete)
            {
                let frames = &gop.frames[part.range.clone()];
                let duration: u64 = frames.iter().map(|(f, _)| u64::from(f.duration)).sum();
                let independent = frames.first().is_some_and(|(f, _)| f.is_sync);
                let _ = write!(
                    out,
                    "#EXT-X-PART:DURATION={:.5},URI=\"{}\"",
                    seconds(duration),
                    uri(
                        &format!("part/{}/{}/part.m4s", gop.msn, part.index),
                        auth,
                        base_path,
                    )
                );
                if independent {
                    out.push_str(",INDEPENDENT=YES");
                }
                out.push('\n');
            }
        }
        if gop.complete {
            let _ = writeln!(
                out,
                "#EXTINF:{:.5},\n{}",
                seconds(gop.duration_ticks()),
                uri(&format!("seg/{}/media.m4s", gop.msn), auth, base_path)
            );
        } else {
            let next = gop
                .parts(frames_per_part)
                .into_iter()
                .filter(|p| p.complete)
                .count();
            let _ = writeln!(
                out,
                "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"{}\"",
                uri(
                    &format!("part/{}/{}/part.m4s", gop.msn, next),
                    auth,
                    base_path,
                )
            );
        }
    }
    out
}

/// The wall time of a frame as `EXT-X-PROGRAM-DATE-TIME` carries it, to the
/// millisecond.
fn rfc3339(wall: WallUs) -> String {
    let c = wall.to_civil();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        c.year,
        c.month,
        c.day,
        c.hour,
        c.minute,
        c.second,
        (wall.get().rem_euclid(1_000_000)) / 1000
    )
}

/// `GET .../hls/init.mp4`: the initialization segment of the live stream.
pub(crate) async fn init(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let hls = lookup(&state, &camera, &kind)?;
    let ready = hls
        .wait_until(hls.block_timeout(), |inner| inner.init.is_some())
        .await?;
    if !ready {
        return Err(ApiError::Unavailable("no frames yet".into()));
    }
    let permit = state.stats.admit_media(MediaClass::Bounded)?;
    let init = hls
        .inner
        .lock()
        .map_err(|_| ApiError::NotFound)?
        .init
        .clone()
        .ok_or(ApiError::NotFound)?;
    Ok(media(init, &permit))
}

/// `GET .../hls/seg/{msn}`: a complete media segment, waiting for it to
/// complete when it is the one being filled.
pub(crate) async fn segment(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind, msn)): Path<(String, String, u64)>,
) -> Result<Response, ApiError> {
    let hls = lookup(&state, &camera, &kind)?;
    let ready = hls
        .wait_until(hls.block_timeout(), |inner| {
            inner.gops.iter().any(|g| g.msn == msn && g.complete)
                || inner.gops.front().is_some_and(|g| g.msn > msn)
        })
        .await?;
    if !ready {
        return Err(ApiError::Unavailable("segment not complete".into()));
    }
    let permit = state.stats.admit_media(MediaClass::Bounded)?;
    let frames = {
        let inner = hls.inner.lock().map_err(|_| ApiError::NotFound)?;
        let gop = inner
            .gops
            .iter()
            .find(|g| g.msn == msn)
            .ok_or(ApiError::NotFound)?;
        gop.frames.clone()
    };
    let bytes = media_admission::blocking(&permit, move || encode(&frames))
        .await
        .map_err(|error| ApiError::Io(std::io::Error::other(error)))??;
    Ok(media(bytes, &permit))
}

/// `GET .../hls/part/{msn}/{index}`: a partial segment, waiting for it to
/// complete (the preload hint names the next one before it exists).
pub(crate) async fn part(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind, msn, index)): Path<(String, String, u64, usize)>,
) -> Result<Response, ApiError> {
    let hls = lookup(&state, &camera, &kind)?;
    let ready = hls
        .wait_until(hls.block_timeout(), |inner| {
            let Some(gop) = inner.gops.iter().find(|g| g.msn == msn) else {
                return inner.gops.front().is_some_and(|g| g.msn > msn);
            };
            let parts = gop.parts(inner.frames_per_part());
            parts.get(index).is_some_and(|p| p.complete) || (gop.complete && parts.len() <= index)
        })
        .await?;
    if !ready {
        return Err(ApiError::Unavailable("part not complete".into()));
    }
    let permit = state.stats.admit_media(MediaClass::Bounded)?;
    let frames = {
        let inner = hls.inner.lock().map_err(|_| ApiError::NotFound)?;
        let gop = inner
            .gops
            .iter()
            .find(|g| g.msn == msn)
            .ok_or(ApiError::NotFound)?;
        let part = gop
            .parts(inner.frames_per_part())
            .into_iter()
            .nth(index)
            .ok_or(ApiError::NotFound)?;
        gop.frames[part.range].to_vec()
    };
    let bytes = media_admission::blocking(&permit, move || encode(&frames))
        .await
        .map_err(|error| ApiError::Io(std::io::Error::other(error)))??;
    Ok(media(bytes, &permit))
}

fn encode(frames: &[(Arc<LiveFrame>, u32)]) -> Result<Bytes, ApiError> {
    let mut out = Vec::new();
    let estimated = frames.iter().try_fold(0usize, |total, (frame, _)| {
        total.checked_add(frame.data.len().saturating_add(512))
    });
    let estimated = estimated.ok_or_else(|| ApiError::Unavailable("media is too large".into()))?;
    out.try_reserve_exact(estimated)
        .map_err(|_| ApiError::Unavailable("media allocation unavailable".into()))?;
    for (frame, seq) in frames {
        encode_live_fragment_into(&mut out, frame, *seq)
            .map_err(|error| ApiError::Io(std::io::Error::other(error)))?;
    }
    Ok(Bytes::from(out))
}

fn media(bytes: Bytes, permit: &MediaPermit) -> Response {
    (
        [
            (header::CONTENT_TYPE, "video/iso.segment"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        media_admission::attach(bytes, permit),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_mp4::init::SampleEntry;

    fn frame(decode_time: u64, is_sync: bool) -> Arc<LiveFrame> {
        frame_with(
            decode_time,
            is_sync,
            Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
            3000,
        )
    }

    fn frame_with(decode_time: u64, is_sync: bool, data: Bytes, duration: u32) -> Arc<LiveFrame> {
        Arc::new(LiveFrame {
            key: StreamKey {
                camera: "cam1".parse().unwrap(),
                kind: iris_core::id::StreamKind::Main,
            },
            sample_entry: Arc::new(SampleEntry::new(
                Bytes::from_static(&[0u8; 8]),
                "hvc1".into(),
                16,
                16,
            )),
            data,
            duration,
            is_sync,
            decode_time,
            wall: WallUs::new(
                1_700_000_000_000_000 + i64::try_from(decode_time).unwrap() * 100 / 9,
            ),
        })
    }

    /// Feeds `frames`, expects the budget breach to cut the open GOP (kept
    /// complete when it has frames, `kept` of them in all), then a dependent
    /// frame to be ignored and a sync frame to start a GOP in the next epoch.
    async fn assert_feed_budget_breach(frames: Vec<Arc<LiveFrame>>, kept: usize) {
        let ring = LiveRing::new(MAX_GOP_FRAMES + 8);
        let hls = LiveHls::new();
        let shutdown = CancellationToken::new();
        hls.touch();
        let task = tokio::spawn(Arc::clone(&hls).feed(ring.clone(), shutdown.clone()));
        tokio::task::yield_now().await;
        for frame in frames {
            hls.touch();
            ring.publish(frame);
            tokio::task::yield_now().await;
        }
        for _ in 0..100 {
            let breached = hls.inner.lock().unwrap().discontinuity_sequence == 1;
            if breached {
                break;
            }
            tokio::task::yield_now().await;
        }
        {
            let inner = hls.inner.lock().unwrap();
            assert_eq!(inner.discontinuity_sequence, 1);
            assert_eq!(inner.gops.len(), kept);
            assert!(inner.gops.iter().all(|g| g.complete));
        }

        hls.touch();
        ring.publish(frame(20_000_000, false));
        tokio::task::yield_now().await;
        {
            let inner = hls.inner.lock().unwrap();
            assert_eq!(inner.discontinuity_sequence, 1);
            assert_eq!(inner.gops.len(), kept, "a dependent frame starts nothing");
        }

        hls.touch();
        ring.publish(frame(20_003_000, true));
        for _ in 0..100 {
            if hls.inner.lock().unwrap().gops.len() > kept {
                break;
            }
            tokio::task::yield_now().await;
        }
        {
            let inner = hls.inner.lock().unwrap();
            assert_eq!(inner.gops.len(), kept + 1);
            let started = inner.gops.back().unwrap();
            assert_eq!(started.discontinuity_sequence, 1);
            assert!(started.frames[0].0.is_sync);
        }

        shutdown.cancel();
        task.await.unwrap();
        assert_eq!(ring.subscriber_count(), 0);
        let inner = hls.inner.lock().unwrap();
        assert!(inner.closed);
        assert!(inner.gops.is_empty());
        assert!(inner.init.is_none());
    }

    fn gop(msn: u64, frames: usize, complete: bool) -> Gop {
        let frames: Vec<_> = (0..frames)
            .map(|i| (frame(i as u64 * 3000, i == 0), i as u32 + 1))
            .collect();
        Gop {
            msn,
            discontinuity_sequence: 0,
            payload_bytes: frames.iter().map(|(frame, _)| frame.data.len()).sum(),
            duration_ticks: frames
                .iter()
                .map(|(frame, _)| u64::from(frame.duration))
                .sum(),
            frames,
            complete,
        }
    }

    #[test]
    fn parts_are_cut_by_frame_count_and_the_tail_waits_for_completion() {
        let g = gop(5, 40, false);
        let parts = g.parts(15);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].range, 0..15);
        assert!(parts[0].complete && parts[1].complete && !parts[2].complete);
        let done = gop(5, 40, true);
        assert!(done.parts(15)[2].complete);
        assert_eq!(done.parts(15)[2].range, 30..40);
    }

    /// The part size follows the first frame's duration: as many whole
    /// frames as fit half a second, at least one, and the declared part
    /// target is exactly what they add up to (§4.4.4.9).
    #[test]
    fn the_part_target_is_pinned_from_the_first_frame_duration() {
        for (duration, frames, target) in [
            (3_000, 15, 45_000),
            (90_000, 1, 90_000),
            (30_000, 1, 30_000),
            (18_000, 2, 36_000),
        ] {
            let mut inner = Inner::default();
            inner.pin_part_target(&frame_with(0, true, Bytes::new(), duration));
            assert_eq!(inner.frames_per_part(), frames, "{duration}");
            assert_eq!(inner.part_target_ticks(), target, "{duration}");
            // Pinned once: a later frame of another duration changes nothing.
            inner.pin_part_target(&frame_with(0, true, Bytes::new(), 1));
            assert_eq!(inner.frames_per_part(), frames, "{duration}");
        }
    }

    /// The first complete GOP pins the target duration (its length rounded
    /// up to whole seconds plus one second, at most the GOP budget), and a
    /// later GOP that would outlast it is cut with a discontinuity.
    #[test]
    fn the_target_duration_is_pinned_by_the_first_complete_gop() {
        let mut inner = Inner::default();
        let mut waiting = true;
        for index in 0..60u64 {
            inner.accept_frame(frame(index * 3000, index == 0), &mut waiting);
        }
        assert_eq!(inner.target_ticks, None, "an open GOP pins nothing");
        assert_eq!(inner.block_timeout(), INITIAL_BLOCK_TIMEOUT);
        inner.accept_frame(frame(60 * 3000, true), &mut waiting);
        assert_eq!(
            inner.target_ticks,
            Some(3 * 90_000),
            "2 s rounded up plus 1 s"
        );
        assert_eq!(inner.target_secs(), 3);
        assert_eq!(inner.block_timeout(), Duration::from_secs(9));

        // The second GOP runs for 3 s and is cut before its 91st frame.
        for index in 61..150u64 {
            inner.accept_frame(frame(index * 3000, false), &mut waiting);
        }
        assert_eq!(inner.gops.len(), 2);
        assert_eq!(inner.gops[1].frames.len(), 90);
        assert!(!waiting);
        inner.accept_frame(frame(151 * 3000, false), &mut waiting);
        assert!(waiting, "the frame past the target cuts the GOP");
        assert!(inner.gops[1].complete);
        assert_eq!(inner.gops[1].frames.len(), 90);
        assert_eq!(inner.discontinuity_sequence, 1);
        assert_eq!(
            inner.target_ticks,
            Some(3 * 90_000),
            "the target never changes"
        );

        let mut long = Inner::default();
        long.gops.push_back(gop(0, 600, false));
        long.complete_current();
        assert_eq!(
            long.target_ticks,
            Some(MAX_GOP_DURATION_TICKS),
            "capped at the GOP budget"
        );
    }

    /// A complete segment stays listed until the complete segments after it
    /// cover three target durations (§6.2.2), and never more than the cap.
    #[test]
    fn complete_segments_stay_until_three_target_durations_follow() {
        let mut inner = Inner {
            target_ticks: Some(3 * 90_000),
            ..Inner::default()
        };
        for msn in 0..5 {
            inner.gops.push_back(gop(msn, 60, true));
            inner.evict();
        }
        assert_eq!(inner.gops.len(), 5, "8 s of newer segments is under 9 s");
        inner.gops.push_back(gop(5, 60, true));
        inner.evict();
        assert_eq!(
            inner.gops.front().map(|g| g.msn),
            Some(1),
            "10 s of newer segments"
        );
        assert_eq!(inner.gops.len(), 5);

        let mut capped = Inner {
            target_ticks: Some(MAX_GOP_DURATION_TICKS),
            ..Inner::default()
        };
        for msn in 0..20 {
            capped.gops.push_back(gop(msn, 60, true));
            capped.evict();
        }
        assert_eq!(capped.gops.len(), MAX_KEPT_GOPS);
    }

    /// Ring lag ends the open segment where it is and starts a discontinuity
    /// epoch; complete segments stay listed.
    #[test]
    fn lag_ends_the_open_segment_and_keeps_the_complete_ones() {
        let mut inner = Inner::default();
        let mut waiting = true;
        for index in 0..=60u64 {
            inner.accept_frame(frame(index * 3000, index % 60 == 0), &mut waiting);
        }
        inner.accept_frame(frame(61 * 3000, false), &mut waiting);
        assert_eq!(inner.gops.len(), 2);
        inner.on_lag(&mut waiting);
        assert!(waiting);
        assert_eq!(inner.gops.len(), 2);
        assert!(inner.gops[1].complete);
        assert_eq!(inner.gops[1].frames.len(), 2);
        assert_eq!(inner.discontinuity_sequence, 1);
        inner.accept_frame(frame(70 * 3000, false), &mut waiting);
        assert_eq!(
            inner.gops.len(),
            2,
            "dependent frames are ignored until sync"
        );
        inner.accept_frame(frame(71 * 3000, true), &mut waiting);
        assert_eq!(inner.gops.len(), 3);
        assert_eq!(inner.gops[2].discontinuity_sequence, 1);
    }

    #[test]
    fn the_advance_part_limit_is_three_seconds_of_parts() {
        let mut inner = Inner {
            part_target_ticks: Some(45_000),
            ..Inner::default()
        };
        assert_eq!(advance_part_limit(&inner), 6);
        inner.part_target_ticks = Some(90_000);
        assert_eq!(advance_part_limit(&inner), 3);
        inner.part_target_ticks = Some(30_000);
        assert_eq!(advance_part_limit(&inner), 9);
    }

    /// An `Inner` as a feeder leaves it after 2 s GOPs at 30 fps.
    fn pinned(mut inner: Inner) -> Inner {
        inner.target_ticks = Some(3 * 90_000);
        inner.part_frames = Some(15);
        inner.part_target_ticks = Some(45_000);
        inner
    }

    #[test]
    fn the_playlist_lists_segments_parts_and_a_preload_hint() {
        let mut inner = pinned(Inner::default());
        inner.gops.push_back(gop(7, 60, true));
        inner.gops.push_back(gop(8, 60, true));
        inner.gops.push_back(gop(9, 20, false));
        inner.next_msn = 10;
        let auth = AuthContext::for_test(&crate::auth::test_secret(), 1_234_567);
        let base = "/api/streams/cam1/main/hls";
        let text = render(&inner, &auth, base);
        assert!(text.starts_with("#EXTM3U\n#EXT-X-VERSION:6\n"), "{text}");
        assert!(text.contains("#EXT-X-TARGETDURATION:3\n"), "{text}");
        assert!(text.contains("#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.500\n"));
        assert!(text.contains("#EXT-X-PART-INF:PART-TARGET=0.500\n"));
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:7\n"));
        let init = auth.capability(&format!("{base}/init.mp4")).unwrap();
        assert!(text.contains(&format!("#EXT-X-MAP:URI=\"init.mp4?token={init}\"\n")));
        assert!(text.contains("#EXT-X-PROGRAM-DATE-TIME:"));
        let part = auth
            .capability(&format!("{base}/part/7/0/part.m4s"))
            .unwrap();
        assert!(text.contains(&format!(
            "#EXT-X-PART:DURATION=0.50000,URI=\"part/7/0/part.m4s?token={part}\",INDEPENDENT=YES\n"
        )));
        let segment = auth.capability(&format!("{base}/seg/7/media.m4s")).unwrap();
        assert!(text.contains(&format!(
            "#EXTINF:2.00000,\nseg/7/media.m4s?token={segment}\n"
        )));
        let current_part = auth
            .capability(&format!("{base}/part/9/0/part.m4s"))
            .unwrap();
        assert!(text.contains(&format!(
            "#EXT-X-PART:DURATION=0.50000,URI=\"part/9/0/part.m4s?token={current_part}\",INDEPENDENT=YES\n"
        )));
        assert!(!text.contains("part/9/1/part.m4s\""), "{text}");
        let preload = auth
            .capability(&format!("{base}/part/9/1/part.m4s"))
            .unwrap();
        assert!(
            text.ends_with(&format!(
                "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/9/1/part.m4s?token={preload}\"\n"
            )),
            "{text}"
        );
        assert!(!text.contains("root bearer"));
        assert!(!text.contains("ENDLIST"));
    }

    #[test]
    fn blocking_reload_conditions_follow_the_specification() {
        let mut inner = pinned(Inner::default());
        inner.gops.push_back(gop(3, 60, true));
        inner.gops.push_back(gop(4, 20, false));
        assert!(reload_satisfied(&inner, 3, None));
        assert!(!reload_satisfied(&inner, 4, None));
        assert!(reload_satisfied(&inner, 4, Some(0)));
        assert!(!reload_satisfied(&inner, 4, Some(1)));
        // A part past the last part of a complete segment means part 0 of the next.
        assert!(reload_satisfied(&inner, 3, Some(9)));
        // Gone from the playlist: satisfied, the whole playlist is returned.
        assert!(reload_satisfied(&inner, 1, None));
        assert!(!reload_satisfied(&inner, 5, None));
    }

    #[test]
    fn discontinuity_sequence_survives_window_eviction() {
        let mut inner = pinned(Inner::default());
        inner.gops.push_back(gop(7, 60, true));
        let mut restarted = gop(8, 60, true);
        restarted.discontinuity_sequence = 1;
        inner.gops.push_back(restarted);
        let auth = AuthContext::disabled();
        let text = render(&inner, &auth, "/api/streams/cam1/main/hls");
        assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:0\n"));
        assert_eq!(text.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
        assert!(
            text.contains("seg/7/media.m4s\n#EXT-X-DISCONTINUITY\n#EXT-X-PROGRAM-DATE-TIME:"),
            "a segment after a discontinuity carries its wall time: {text}"
        );
        assert_eq!(text.matches("#EXT-X-PROGRAM-DATE-TIME:").count(), 2);
        inner.gops.pop_front();
        let text = render(&inner, &auth, "/api/streams/cam1/main/hls");
        assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1\n"));
        assert!(!text.contains("#EXT-X-DISCONTINUITY\n"));
    }

    #[test]
    fn each_gop_budget_rejects_its_next_frame_independently() {
        let incoming = frame(0, false);
        let mut payload = gop(0, 0, false);
        payload.payload_bytes = MAX_GOP_PAYLOAD_BYTES;
        assert!(payload.would_exceed_budget(&incoming, MAX_GOP_DURATION_TICKS));

        let frames = gop(0, MAX_GOP_FRAMES, false);
        assert!(frames.would_exceed_budget(&incoming, MAX_GOP_DURATION_TICKS));

        let mut duration = gop(0, 0, false);
        duration.duration_ticks = MAX_GOP_DURATION_TICKS;
        assert!(duration.would_exceed_budget(&incoming, MAX_GOP_DURATION_TICKS));
        assert!(!gop(0, 0, false).would_exceed_budget(&incoming, MAX_GOP_DURATION_TICKS));
        assert!(gop(0, 0, false).would_exceed_budget(&incoming, 2_999));
    }

    #[tokio::test]
    async fn payload_budget_caps_a_continuously_touched_feeder() {
        let oversized = Bytes::from(vec![0_u8; MAX_GOP_PAYLOAD_BYTES + 1]);
        assert_feed_budget_breach(vec![frame_with(0, true, oversized, 3000)], 0).await;
    }

    #[tokio::test]
    async fn frame_budget_caps_a_continuously_touched_feeder() {
        let frames = (0..=MAX_GOP_FRAMES)
            .map(|index| frame(index as u64 * 3000, index == 0))
            .collect();
        assert_feed_budget_breach(frames, 1).await;
    }

    #[tokio::test]
    async fn duration_budget_caps_a_continuously_touched_feeder() {
        let duration = u32::try_from(MAX_GOP_DURATION_TICKS + 1).unwrap();
        assert_feed_budget_breach(
            vec![frame_with(
                0,
                true,
                Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration,
            )],
            0,
        )
        .await;
    }

    /// CRC-32C (Castagnoli, reflected polynomial 0x82f63b78) lookup table.
    fn crc32c_table() -> [u32; 256] {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut crc = u32::try_from(index).expect("table index fits u32");
            for _ in 0..8 {
                crc = if crc & 1 == 0 {
                    crc >> 1
                } else {
                    (crc >> 1) ^ 0x82f6_3b78
                };
            }
            *slot = crc;
        }
        table
    }

    /// Appends four bytes to `prefix` so that the result's CRC-32C is `target`,
    /// running the register backwards from the target.
    fn forge_crc32c(prefix: &[u8], target: u32) -> Vec<u8> {
        let table = crc32c_table();
        let mut reverse = [0u32; 256];
        for (index, value) in table.iter().enumerate() {
            reverse[(value >> 24) as usize] =
                (value << 8) ^ u32::try_from(index).expect("table index fits u32");
        }
        let mut register = 0xffff_ffffu32;
        for &byte in prefix {
            register = (register >> 8) ^ table[((register ^ u32::from(byte)) & 0xff) as usize];
        }
        let mut back = target ^ 0xffff_ffff;
        for _ in 0..4 {
            back = (back << 8) ^ reverse[(back >> 24) as usize];
        }
        let mut forged = prefix.to_vec();
        forged.extend_from_slice(&(back ^ register).to_le_bytes());
        forged
    }

    fn colliding_entries() -> (Arc<SampleEntry>, Arc<SampleEntry>) {
        let first = b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0parameter-set-epoch-A".to_vec();
        let target = iris_mp4::crc::crc32c(&first);
        let second = forge_crc32c(
            b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0parameter-set-epoch-B",
            target,
        );
        let first = SampleEntry::new(Bytes::from(first), "hvc1".into(), 16, 16);
        let second = SampleEntry::new(Bytes::from(second), "hvc1".into(), 16, 16);
        assert_ne!(first.bytes, second.bytes);
        assert_eq!(first.hash, second.hash, "forged CRC-32C collision");
        (Arc::new(first), Arc::new(second))
    }

    #[test]
    fn a_crc_colliding_sample_entry_still_rebuilds_the_init_segment() {
        let (first, second) = colliding_entries();
        let with_entry = |decode_time: u64, entry: &Arc<SampleEntry>| {
            let mut next = (*frame(decode_time, true)).clone();
            next.sample_entry = Arc::clone(entry);
            Arc::new(next)
        };
        let mut inner = Inner::default();
        let mut waiting_for_sync = true;
        inner.accept_frame(with_entry(0, &first), &mut waiting_for_sync);
        assert_eq!(inner.init, Some(build_init_segment(&first)));
        assert_eq!(inner.discontinuity_sequence, 0);

        inner.accept_frame(with_entry(3_000, &second), &mut waiting_for_sync);
        assert_eq!(
            inner.init,
            Some(build_init_segment(&second)),
            "equal hashes must not keep the previous init segment"
        );
        assert_eq!(inner.discontinuity_sequence, 1);
        assert_eq!(inner.gops.len(), 1);
        assert!(Arc::ptr_eq(
            &inner.gops[0].frames[0].0.sample_entry,
            &second
        ));
    }

    #[test]
    fn budget_breach_advances_once_and_recovers_at_the_next_sync() {
        let mut inner = Inner::default();
        let mut waiting_for_sync = true;
        inner.accept_frame(frame(0, true), &mut waiting_for_sync);
        inner.gops.back_mut().unwrap().payload_bytes = MAX_GOP_PAYLOAD_BYTES;
        inner.accept_frame(frame(3000, false), &mut waiting_for_sync);
        assert!(waiting_for_sync);
        assert_eq!(inner.gops.len(), 1, "the cut GOP stays, complete");
        assert!(inner.gops[0].complete);
        assert_eq!(inner.discontinuity_sequence, 1);

        inner.accept_frame(frame(6000, false), &mut waiting_for_sync);
        assert_eq!(inner.discontinuity_sequence, 1);
        assert_eq!(inner.gops.len(), 1);

        inner.accept_frame(frame(9000, true), &mut waiting_for_sync);
        assert!(!waiting_for_sync);
        assert_eq!(inner.gops.len(), 2);
        assert_eq!(inner.gops[1].discontinuity_sequence, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn silent_feeder_expires_and_touches_extend_one_subscription() {
        let ring = LiveRing::new(8);
        let hls = LiveHls::new();
        let shutdown = CancellationToken::new();
        hls.touch();
        let task = tokio::spawn(Arc::clone(&hls).feed(ring.clone(), shutdown));
        tokio::task::yield_now().await;
        assert_eq!(ring.subscriber_count(), 1);

        tokio::time::advance(IDLE_TIMEOUT.saturating_sub(Duration::from_secs(1))).await;
        hls.touch();
        tokio::time::advance(IDLE_TIMEOUT.saturating_sub(Duration::from_secs(1))).await;
        tokio::task::yield_now().await;
        assert_eq!(ring.subscriber_count(), 1);

        tokio::time::advance(Duration::from_secs(2)).await;
        task.await.unwrap();
        assert_eq!(ring.subscriber_count(), 0);
        let inner = hls.inner.lock().unwrap();
        assert!(inner.closed);
        assert!(inner.gops.is_empty());
        assert!(inner.init.is_none());
    }
}
