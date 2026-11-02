//! Low-latency HLS from the live ring: a live playlist with partial segments
//! built in memory, blocking playlist reload, and preload hints
//! (`docs/standards/ietf/draft-pantos-hls-rfc8216bis-22.txt` §4.4.3.7–8,
//! §4.4.4.9, §4.4.5.3, §6.2.5.2).
//!
//! One feeder task per requested stream subscribes to the stream's
//! [`LiveRing`] and keeps the frames of the last few groups of pictures: each
//! GOP is one media segment (its media sequence number is assigned when its
//! sync frame arrives), and each segment is cut into partial segments of
//! about [`PART_TARGET_TICKS`] by frame count, so a part's content never
//! changes once published. Segments and parts are served from memory as
//! sequences of single-sample fragments, exactly what the WebSocket feed
//! sends; their URIs end in `.m4s` because `ffmpeg` accepts no other
//! extension. The feeder stops when nobody has asked for the stream for
//! [`IDLE_TIMEOUT`], and a later request starts a fresh one.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Extension, Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use iris_core::id::StreamKey;
use iris_core::time::{TIMESCALE, WallUs};
use iris_ingest::{LiveError, LiveFrame, LiveRing};
use iris_mp4::init::build_init_segment;
use serde::Deserialize;
use tokio::sync::Notify;

use crate::auth::AuthToken;
use crate::error::ApiError;
use crate::hls::percent_encode;
use crate::live::encode_live_fragment;
use crate::segments::stream_key;
use crate::state::ApiState;

/// Target length of a partial segment in media ticks (0.5 s).
pub(crate) const PART_TARGET_TICKS: u64 = 45_000;
/// Complete segments kept behind the one being filled.
const KEEP_SEGMENTS: usize = 3;
/// Segments that list their partial segments: the last ones, as the
/// specification asks for parts within three target durations of the end.
const SEGMENTS_WITH_PARTS: usize = 3;
/// A feeder with no request for this long stops.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a blocking request waits before answering 503.
const BLOCK_TIMEOUT: Duration = Duration::from_secs(6);

/// The frames of one group of pictures, one media segment.
#[derive(Debug)]
struct Gop {
    msn: u64,
    /// Frames with the `mfhd` sequence number each was encoded with.
    frames: Vec<(Arc<LiveFrame>, u32)>,
    complete: bool,
}

impl Gop {
    /// Frames per partial segment: the target length over the first frame's
    /// provisional duration, at least one.
    fn frames_per_part(&self) -> usize {
        let duration = self
            .frames
            .first()
            .map_or(1, |(f, _)| u64::from(f.duration.max(1)));
        usize::try_from(PART_TARGET_TICKS / duration)
            .unwrap_or(1)
            .max(1)
    }

    /// Partial segments available so far: `(index, frame range, complete)`.
    fn parts(&self) -> Vec<Part> {
        let n = self.frames_per_part();
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
        self.frames.iter().map(|(f, _)| u64::from(f.duration)).sum()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Part {
    index: usize,
    range: std::ops::Range<usize>,
    complete: bool,
}

#[derive(Debug, Default)]
struct Inner {
    gops: VecDeque<Gop>,
    next_msn: u64,
    next_seq: u32,
    init: Option<Bytes>,
    init_hash: Option<u32>,
    last_access: Option<Instant>,
    closed: bool,
}

/// The live playlist state of one stream.
#[derive(Debug)]
pub struct LiveHls {
    inner: Mutex<Inner>,
    notify: Notify,
}

impl LiveHls {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            notify: Notify::new(),
        })
    }

    fn touch(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last_access = Some(Instant::now());
        }
    }

    /// Feeds frames from `ring` until the ring closes or nobody asks for the
    /// stream for [`IDLE_TIMEOUT`].
    async fn feed(self: Arc<Self>, ring: LiveRing) {
        let mut subscription = ring.subscribe();
        let mut waiting_for_sync = true;
        loop {
            let frame = match subscription.next().await {
                Ok(frame) => frame,
                Err(LiveError::Lagged(_)) => {
                    waiting_for_sync = true;
                    if let Ok(mut inner) = self.inner.lock() {
                        inner.gops.clear();
                    }
                    continue;
                }
                Err(LiveError::Closed) => break,
            };
            let Ok(mut inner) = self.inner.lock() else {
                break;
            };
            if inner
                .last_access
                .is_some_and(|t| t.elapsed() > IDLE_TIMEOUT)
            {
                break;
            }
            if inner.init_hash != Some(frame.sample_entry.hash) {
                inner.init = Some(build_init_segment(&frame.sample_entry));
                inner.init_hash = Some(frame.sample_entry.hash);
                inner.gops.clear();
                waiting_for_sync = true;
            }
            if frame.is_sync {
                if let Some(current) = inner.gops.back_mut() {
                    current.complete = true;
                }
                let msn = inner.next_msn;
                inner.next_msn += 1;
                inner.gops.push_back(Gop {
                    msn,
                    frames: Vec::new(),
                    complete: false,
                });
                while inner.gops.len() > KEEP_SEGMENTS + 1 {
                    inner.gops.pop_front();
                }
                waiting_for_sync = false;
            }
            if waiting_for_sync {
                continue;
            }
            let seq = inner.next_seq.wrapping_add(1);
            inner.next_seq = seq;
            if let Some(current) = inner.gops.back_mut() {
                current.frames.push((frame, seq));
            }
            drop(inner);
            self.notify.notify_waiters();
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.closed = true;
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
        let deadline = tokio::time::Instant::now() + timeout;
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
pub struct LiveHlsRegistry {
    streams: Mutex<HashMap<StreamKey, Arc<LiveHls>>>,
}

impl LiveHlsRegistry {
    /// The state for `key`, starting a feeder on `ring` when there is none
    /// or the previous one stopped.
    fn get_or_start(&self, key: &StreamKey, ring: LiveRing) -> Arc<LiveHls> {
        let mut streams = self.streams.lock().expect("live hls lock");
        if let Some(existing) = streams.get(key)
            && !existing.inner.lock().is_ok_and(|i| i.closed)
        {
            existing.touch();
            return Arc::clone(existing);
        }
        let hls = LiveHls::new();
        hls.touch();
        tokio::spawn(Arc::clone(&hls).feed(ring));
        streams.insert(key.clone(), Arc::clone(&hls));
        hls
    }
}

fn lookup(state: &ApiState, camera: &str, kind: &str) -> Result<Arc<LiveHls>, ApiError> {
    let key = stream_key(camera, kind)?;
    let ring = state.live.get(&key).ok_or(ApiError::NotFound)?;
    Ok(state.live.hls().get_or_start(&key, ring))
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
    Extension(token): Extension<AuthToken>,
) -> Result<Response, ApiError> {
    let hls = lookup(&state, &camera, &kind)?;
    if params.part.is_some() && params.msn.is_none() {
        return Err(ApiError::BadRequest("_HLS_part without _HLS_msn".into()));
    }
    let token = token.0.clone();
    // A client may ask for a segment up to two beyond the newest; further is an error.
    if let Some(msn) = params.msn {
        let last = hls
            .inner
            .lock()
            .map_err(|_| ApiError::NotFound)?
            .gops
            .back()
            .map(|g| g.msn);
        if last.is_some_and(|last| msn > last + 2) {
            return Err(ApiError::BadRequest(format!(
                "_HLS_msn {msn} is too far ahead"
            )));
        }
    }
    // A playlist needs a segment to play from: the first request waits for
    // the first complete GOP (at most one GOP length), later ones answer at
    // once unless a directive asks for more.
    let ready = hls
        .wait_until(BLOCK_TIMEOUT, |inner| {
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
    let text = {
        let inner = hls.inner.lock().map_err(|_| ApiError::NotFound)?;
        render(&inner, token.as_deref())
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        text,
    )
        .into_response())
}

/// Whether the playlist holds segment `msn` (complete, or with part `part`
/// complete). A part index past the segment's last part means part 0 of
/// the next segment (§6.2.5.2).
fn reload_satisfied(inner: &Inner, msn: u64, part: Option<usize>) -> bool {
    let Some(gop) = inner.gops.iter().find(|g| g.msn == msn) else {
        // Already gone from the playlist, or not yet started: served when newer exists.
        return inner.gops.back().is_some_and(|g| g.msn > msn);
    };
    match part {
        None => gop.complete,
        Some(index) => {
            let parts = gop.parts();
            match parts.get(index) {
                Some(p) => p.complete,
                None if gop.complete => inner
                    .gops
                    .iter()
                    .any(|g| g.msn == msn + 1 && g.parts().first().is_some_and(|p| p.complete)),
                None => false,
            }
        }
    }
}

fn uri(path: &str, token: Option<&str>) -> String {
    match token {
        Some(t) => format!("{path}?token={}", percent_encode(t)),
        None => path.to_owned(),
    }
}

fn seconds(ticks: u64) -> f64 {
    ticks as f64 / f64::from(TIMESCALE)
}

/// Renders the live playlist. URIs are relative to the playlist's own path.
fn render(inner: &Inner, token: Option<&str>) -> String {
    let target_ticks = inner
        .gops
        .iter()
        .filter(|g| g.complete)
        .map(Gop::duration_ticks)
        .max()
        .unwrap_or(2 * u64::from(TIMESCALE));
    let target = target_ticks.div_ceil(u64::from(TIMESCALE)).max(1);
    let part_target = seconds(PART_TARGET_TICKS);
    let first_msn = inner.gops.front().map_or(0, |g| g.msn);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "#EXTM3U\n#EXT-X-VERSION:9\n#EXT-X-TARGETDURATION:{target}"
    );
    let _ = writeln!(
        out,
        "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK={:.3}",
        part_target * 3.0
    );
    let _ = writeln!(out, "#EXT-X-PART-INF:PART-TARGET={part_target:.3}");
    let _ = writeln!(
        out,
        "#EXT-X-MEDIA-SEQUENCE:{first_msn}\n#EXT-X-INDEPENDENT-SEGMENTS"
    );
    let _ = writeln!(out, "#EXT-X-MAP:URI=\"{}\"", uri("init.mp4", token));
    let with_parts_from = inner.gops.len().saturating_sub(SEGMENTS_WITH_PARTS);
    for (i, gop) in inner.gops.iter().enumerate() {
        if let Some((first, _)) = gop.frames.first()
            && i == 0
        {
            let _ = writeln!(out, "#EXT-X-PROGRAM-DATE-TIME:{}", rfc3339(first.wall));
        }
        if i >= with_parts_from {
            for part in gop.parts().into_iter().filter(|p| p.complete) {
                let frames = &gop.frames[part.range.clone()];
                let duration: u64 = frames.iter().map(|(f, _)| u64::from(f.duration)).sum();
                let independent = frames.first().is_some_and(|(f, _)| f.is_sync);
                let _ = write!(
                    out,
                    "#EXT-X-PART:DURATION={:.5},URI=\"{}\"",
                    seconds(duration),
                    uri(&format!("part/{}/{}/part.m4s", gop.msn, part.index), token)
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
                uri(&format!("seg/{}/media.m4s", gop.msn), token)
            );
        } else {
            let next = gop.parts().into_iter().filter(|p| p.complete).count();
            let _ = writeln!(
                out,
                "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"{}\"",
                uri(&format!("part/{}/{}/part.m4s", gop.msn, next), token)
            );
        }
    }
    out
}

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
        .wait_until(BLOCK_TIMEOUT, |inner| inner.init.is_some())
        .await?;
    if !ready {
        return Err(ApiError::Unavailable("no frames yet".into()));
    }
    let init = hls
        .inner
        .lock()
        .map_err(|_| ApiError::NotFound)?
        .init
        .clone()
        .ok_or(ApiError::NotFound)?;
    Ok(media(init))
}

/// `GET .../hls/seg/{msn}`: a complete media segment, waiting for it to
/// complete when it is the one being filled.
pub(crate) async fn segment(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind, msn)): Path<(String, String, u64)>,
) -> Result<Response, ApiError> {
    let hls = lookup(&state, &camera, &kind)?;
    let ready = hls
        .wait_until(BLOCK_TIMEOUT, |inner| {
            inner.gops.iter().any(|g| g.msn == msn && g.complete)
                || inner.gops.front().is_some_and(|g| g.msn > msn)
        })
        .await?;
    if !ready {
        return Err(ApiError::Unavailable("segment not complete".into()));
    }
    let bytes = {
        let inner = hls.inner.lock().map_err(|_| ApiError::NotFound)?;
        let gop = inner
            .gops
            .iter()
            .find(|g| g.msn == msn)
            .ok_or(ApiError::NotFound)?;
        encode(&gop.frames)?
    };
    Ok(media(bytes))
}

/// `GET .../hls/part/{msn}/{index}`: a partial segment, waiting for it to
/// complete (the preload hint names the next one before it exists).
pub(crate) async fn part(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind, msn, index)): Path<(String, String, u64, usize)>,
) -> Result<Response, ApiError> {
    let hls = lookup(&state, &camera, &kind)?;
    let ready = hls
        .wait_until(BLOCK_TIMEOUT, |inner| {
            let Some(gop) = inner.gops.iter().find(|g| g.msn == msn) else {
                return inner.gops.front().is_some_and(|g| g.msn > msn);
            };
            gop.parts().get(index).is_some_and(|p| p.complete)
                || (gop.complete && gop.parts().len() <= index)
        })
        .await?;
    if !ready {
        return Err(ApiError::Unavailable("part not complete".into()));
    }
    let bytes = {
        let inner = hls.inner.lock().map_err(|_| ApiError::NotFound)?;
        let gop = inner
            .gops
            .iter()
            .find(|g| g.msn == msn)
            .ok_or(ApiError::NotFound)?;
        let part = gop
            .parts()
            .into_iter()
            .nth(index)
            .ok_or(ApiError::NotFound)?;
        encode(&gop.frames[part.range])?
    };
    Ok(media(bytes))
}

fn encode(frames: &[(Arc<LiveFrame>, u32)]) -> Result<Bytes, ApiError> {
    let mut out = Vec::new();
    for (frame, seq) in frames {
        let fragment = encode_live_fragment(frame, *seq)
            .map_err(|e| ApiError::Io(std::io::Error::other(e.to_string())))?;
        out.extend_from_slice(&fragment);
    }
    Ok(Bytes::from(out))
}

fn media(bytes: Bytes) -> Response {
    (
        [
            (header::CONTENT_TYPE, "video/iso.segment"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_mp4::init::SampleEntry;

    fn frame(decode_time: u64, is_sync: bool) -> Arc<LiveFrame> {
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
            data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
            duration: 3000,
            is_sync,
            decode_time,
            wall: WallUs::new(
                1_700_000_000_000_000 + i64::try_from(decode_time).unwrap() * 100 / 9,
            ),
        })
    }

    fn gop(msn: u64, frames: usize, complete: bool) -> Gop {
        Gop {
            msn,
            frames: (0..frames)
                .map(|i| (frame(i as u64 * 3000, i == 0), i as u32 + 1))
                .collect(),
            complete,
        }
    }

    #[test]
    fn parts_are_cut_by_frame_count_and_the_tail_waits_for_completion() {
        let g = gop(5, 40, false);
        let parts = g.parts();
        assert_eq!(g.frames_per_part(), 15);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].range, 0..15);
        assert!(parts[0].complete && parts[1].complete && !parts[2].complete);
        let done = gop(5, 40, true);
        assert!(done.parts()[2].complete);
        assert_eq!(done.parts()[2].range, 30..40);
    }

    #[test]
    fn the_playlist_lists_segments_parts_and_a_preload_hint() {
        let mut inner = Inner::default();
        inner.gops.push_back(gop(7, 60, true));
        inner.gops.push_back(gop(8, 60, true));
        inner.gops.push_back(gop(9, 20, false));
        inner.next_msn = 10;
        let text = render(&inner, Some("t k"));
        assert!(text.contains("#EXT-X-TARGETDURATION:2\n"), "{text}");
        assert!(text.contains("#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.500\n"));
        assert!(text.contains("#EXT-X-PART-INF:PART-TARGET=0.500\n"));
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:7\n"));
        assert!(text.contains("#EXT-X-MAP:URI=\"init.mp4?token=t%20k\"\n"));
        assert!(text.contains("#EXT-X-PROGRAM-DATE-TIME:"));
        assert!(text.contains(
            "#EXT-X-PART:DURATION=0.50000,URI=\"part/7/0/part.m4s?token=t%20k\",INDEPENDENT=YES\n"
        ));
        assert!(text.contains("#EXTINF:2.00000,\nseg/7/media.m4s?token=t%20k\n"));
        assert!(text.contains(
            "#EXT-X-PART:DURATION=0.50000,URI=\"part/9/0/part.m4s?token=t%20k\",INDEPENDENT=YES\n"
        ));
        assert!(!text.contains("part/9/1/part.m4s\""), "{text}");
        assert!(
            text.ends_with("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/9/1/part.m4s?token=t%20k\"\n"),
            "{text}"
        );
        assert!(!text.contains("ENDLIST"));
    }

    #[test]
    fn blocking_reload_conditions_follow_the_specification() {
        let mut inner = Inner::default();
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
}
