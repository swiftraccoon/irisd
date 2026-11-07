//! Segment listing and file serving.

use std::io;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use iris_catalog::{SegmentId, SegmentRow};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeekExt as _, SeekFrom};
use tokio::sync::mpsc;

use crate::error::ApiError;
use crate::hls::{MAX_SEGMENT_ROWS, PublishedIndex, published_index};
use crate::media_admission::{self, MediaPermit};
use crate::query::{TimeParams, resolve_range};
use crate::state::{ApiState, MediaClass, admitted_media_bytes};

const RAW_BODY_CHUNK_BYTES: u64 = 64 * 1024;
/// Longest raw range admitted in the bounded media class; whole files,
/// open-ended ranges, and longer ranges are streaming responses.
pub(crate) const BOUNDED_RANGE_BYTES: u64 = 1024 * 1024;
type RawBodyItem = Result<Bytes, io::Error>;

/// A segment as the API presents it (no server paths).
#[derive(Debug, Serialize)]
pub(crate) struct SegmentDto {
    id: i64,
    camera: String,
    kind: String,
    run_seq: u64,
    discontinuity: bool,
    wall_start: String,
    wall_start_us: i64,
    wall_end: String,
    wall_end_us: i64,
    media_start: u64,
    media_end: u64,
    init_len: u32,
    durable_len: u64,
    fragments: u32,
    bytes: u64,
    sealed: bool,
    codecs: String,
    width: u16,
    height: u16,
}

impl From<&SegmentRow> for SegmentDto {
    fn from(row: &SegmentRow) -> Self {
        Self {
            id: row.id.0,
            camera: row.key.camera.to_string(),
            kind: row.key.kind.to_string(),
            run_seq: row.run_seq,
            discontinuity: row.discontinuity,
            wall_start: row.wall_start.to_string(),
            wall_start_us: row.wall_start.get(),
            wall_end: row.wall_end.to_string(),
            wall_end_us: row.wall_end.get(),
            media_start: row.media_start,
            media_end: row.media_end,
            init_len: row.init_len,
            durable_len: row.durable_len,
            fragments: row.fragments,
            bytes: row.bytes,
            sealed: row.sealed,
            codecs: row.codecs.clone(),
            width: row.width,
            height: row.height,
        }
    }
}

/// Parses the `{camera}/{kind}` path pair.
pub(crate) fn stream_key(camera: &str, kind: &str) -> Result<StreamKey, ApiError> {
    let camera = CameraId::try_new(camera).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let kind: StreamKind = kind
        .parse()
        .map_err(|e: iris_core::id::UnknownStreamKind| ApiError::BadRequest(e.to_string()))?;
    Ok(StreamKey { camera, kind })
}

pub(crate) async fn list(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<TimeParams>,
) -> Result<Json<Vec<SegmentDto>>, ApiError> {
    let key = stream_key(&camera, &kind)?;
    let range = resolve_range(&params, WallUs::now())?;
    let rows = state
        .catalog
        .segments_limited(&key, range, MAX_SEGMENT_ROWS + 1)
        .await?;
    if rows.len() > MAX_SEGMENT_ROWS {
        return Err(ApiError::Unavailable(
            "recording range exceeds the segment serving budget".into(),
        ));
    }
    Ok(Json(rows.iter().map(SegmentDto::from).collect()))
}

pub(crate) async fn file(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Result<Response, ApiError> {
    let permit = state
        .stats
        .admit_media(raw_admission_class(req.headers().get(header::RANGE)))?;
    let row = state
        .catalog
        .segment(SegmentId(id))
        .await?
        .ok_or(ApiError::NotFound)?;
    let published = published_index(&state, &row, &permit).await?;
    serve_extent(row.id, published, req, permit).await
}

/// The admission class of a raw request: bounded when its `Range` names at
/// most [`BOUNDED_RANGE_BYTES`], streaming otherwise. The served length never
/// exceeds the requested one, so a bounded request cannot stream more.
fn raw_admission_class(range: Option<&HeaderValue>) -> MediaClass {
    let requested = range
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.strip_prefix("bytes="))
        .map(str::trim)
        .filter(|range| !range.contains(','))
        .and_then(|range| range.split_once('-'))
        .and_then(|(start, end)| {
            if start.is_empty() {
                return end.parse::<u64>().ok();
            }
            let start: u64 = start.parse().ok()?;
            let end: u64 = end.parse().ok()?;
            end.checked_sub(start)?.checked_add(1)
        });
    if requested.is_some_and(|len| len <= BOUNDED_RANGE_BYTES) {
        MediaClass::Bounded
    } else {
        MediaClass::Streaming
    }
}

async fn serve_extent(
    segment: SegmentId,
    published: PublishedIndex,
    req: Request,
    permit: MediaPermit,
) -> Result<Response, ApiError> {
    let extent = published.media.extent();
    let Ok(range) = parse_range(req.headers().get(header::RANGE), extent) else {
        return Ok(range_not_satisfiable(extent));
    };
    let (status, start, len, content_range) = match range {
        None => (StatusCode::OK, 0, extent, None),
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{extent}")),
        ),
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::CONTENT_LENGTH, len.to_string());
    if let Some(content_range) = content_range {
        builder = builder.header(header::CONTENT_RANGE, content_range);
    }
    if req.method() == Method::HEAD || len == 0 {
        drop(permit);
        return Ok(builder
            .body(Body::empty())
            .expect("static media response headers are valid"));
    }
    // Serve from the descriptor the sidecar was validated against; the
    // pathname is never reopened, so a later swap cannot change the bytes.
    let validated = published.file;
    let file = media_admission::spawn(&permit, async move {
        let mut file = tokio::fs::File::from_std(validated);
        file.seek(SeekFrom::Start(start)).await?;
        Ok::<_, io::Error>(file)
    })
    .await
    .map_err(io::Error::other)??;
    Ok(builder
        .body(raw_body(file, len, &permit, segment))
        .expect("static media response headers are valid"))
}

fn raw_body<R>(reader: R, len: u64, permit: &MediaPermit, segment: SegmentId) -> Body
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let (mut rx, producer) = spawn_raw_body(reader, len, permit, segment);
    // The producer deliberately outlives a cancelled response while its current
    // filesystem operation still owns the process-wide admission slot.
    drop(producer);
    let stream = futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx));
    Body::from_stream(stream)
}

fn spawn_raw_body<R>(
    reader: R,
    len: u64,
    permit: &MediaPermit,
    segment: SegmentId,
) -> (mpsc::Receiver<RawBodyItem>, tokio::task::JoinHandle<()>)
where
    R: AsyncRead + Send + Unpin + 'static,
{
    let (tx, rx) = mpsc::channel(1);
    let chunk_permit = Arc::clone(permit);
    let producer = media_admission::spawn(permit, async move {
        let mut reader = reader.take(len);
        let mut remaining = len;
        while remaining > 0 {
            let Ok(slot) = tx.reserve().await else {
                return;
            };
            let chunk_len = usize::try_from(remaining.min(RAW_BODY_CHUNK_BYTES))
                .expect("raw body chunk limit fits usize");
            let mut buffer = Vec::new();
            if let Err(error) = buffer.try_reserve_exact(chunk_len) {
                slot.send(Err(io::Error::other(error.to_string())));
                return;
            }
            buffer.resize(chunk_len, 0);
            match reader.read(&mut buffer).await {
                Ok(0) => {
                    // The file shrank under a response whose length was already
                    // declared: abort the body instead of ending it cleanly.
                    tracing::warn!(
                        segment = segment.0,
                        missing_bytes = remaining,
                        "recording ended before its validated extent; aborting the response"
                    );
                    slot.send(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "recording ended before its validated extent",
                    )));
                    return;
                }
                Ok(read) => {
                    buffer.truncate(read);
                    remaining -= u64::try_from(read).expect("read length fits u64");
                    slot.send(Ok(admitted_media_bytes(
                        Bytes::from(buffer),
                        Arc::clone(&chunk_permit),
                    )));
                }
                Err(error) => {
                    slot.send(Err(error));
                    return;
                }
            }
        }
    });
    (rx, producer)
}

fn parse_range(value: Option<&HeaderValue>, extent: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    let raw = value.to_str().map_err(|_| ())?;
    let range = raw.strip_prefix("bytes=").ok_or(())?.trim();
    if range.contains(',') {
        return Err(());
    }
    let (start, end) = range.split_once('-').ok_or(())?;
    if extent == 0 {
        return Err(());
    }
    if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let start = extent.saturating_sub(suffix);
        return Ok(Some((start, extent - 1)));
    }
    let start: u64 = start.parse().map_err(|_| ())?;
    if start >= extent {
        return Err(());
    }
    let end = if end.is_empty() {
        extent - 1
    } else {
        let end: u64 = end.parse().map_err(|_| ())?;
        if end < start {
            return Err(());
        }
        end.min(extent - 1)
    };
    Ok(Some((start, end)))
}

fn range_not_satisfiable(extent: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{extent}"))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::empty())
        .expect("static range response headers are valid")
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use iris_core::config::DEFAULT_API_STREAMING_MEDIA;
    use tokio::io::{AsyncWriteExt as _, ReadBuf};
    use tokio::sync::oneshot;

    use super::*;
    use crate::state::{ApiStats, MediaClass};

    struct StartedReader<R> {
        inner: R,
        started: Option<oneshot::Sender<()>>,
    }

    impl<R: AsyncRead + Unpin> AsyncRead for StartedReader<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            Pin::new(&mut self.inner).poll_read(cx, buffer)
        }
    }

    fn header(raw: &'static str) -> HeaderValue {
        HeaderValue::from_static(raw)
    }

    #[test]
    fn ranges_are_clamped_to_the_published_extent() {
        assert_eq!(parse_range(None, 100), Ok(None));
        assert_eq!(
            parse_range(Some(&header("bytes=10-20")), 100),
            Ok(Some((10, 20)))
        );
        assert_eq!(
            parse_range(Some(&header("bytes=90-200")), 100),
            Ok(Some((90, 99)))
        );
        assert_eq!(
            parse_range(Some(&header("bytes=90-")), 100),
            Ok(Some((90, 99)))
        );
        assert_eq!(
            parse_range(Some(&header("bytes=-10")), 100),
            Ok(Some((90, 99)))
        );
        assert_eq!(
            parse_range(Some(&header("bytes=-200")), 100),
            Ok(Some((0, 99)))
        );
        assert_eq!(parse_range(Some(&header("bytes=100-")), 100), Err(()));
        assert_eq!(parse_range(Some(&header("bytes=0-1,4-5")), 100), Err(()));
        assert_eq!(parse_range(Some(&header("items=0-1")), 100), Err(()));
    }

    #[test]
    fn short_ranges_are_bounded_and_everything_else_streams() {
        for bounded in [
            "bytes=0-1048575",
            "bytes=10-20",
            "bytes=-1048576",
            "bytes= 5-5 ",
        ] {
            assert_eq!(
                raw_admission_class(Some(&header(bounded))),
                MediaClass::Bounded,
                "{bounded}"
            );
        }
        for streaming in [
            "bytes=0-1048576",
            "bytes=-1048577",
            "bytes=0-",
            "bytes=20-10",
            "bytes=0-1,4-5",
            "items=0-1",
            "bytes=x-y",
        ] {
            assert_eq!(
                raw_admission_class(Some(&header(streaming))),
                MediaClass::Streaming,
                "{streaming}"
            );
        }
        assert_eq!(raw_admission_class(None), MediaClass::Streaming);
    }

    #[tokio::test]
    async fn an_early_end_of_file_aborts_the_raw_body_with_an_error() {
        let stats = Arc::new(ApiStats::default());
        let permit = media_admission::shared(
            stats
                .try_media(MediaClass::Streaming)
                .expect("raw response admission slot"),
        );
        let (mut rx, producer) =
            spawn_raw_body(io::Cursor::new(vec![1, 2, 3]), 10, &permit, SegmentId(1));
        drop(permit);
        let first = rx
            .recv()
            .await
            .expect("first chunk")
            .expect("available bytes are served");
        assert_eq!(&first[..], &[1, 2, 3]);
        let error = rx
            .recv()
            .await
            .expect("an error item ends the truncated body")
            .expect_err("a short file is an error, not a clean end");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(rx.recv().await.is_none());
        producer.await.expect("raw producer task");
    }

    #[tokio::test]
    async fn cancelled_raw_bodies_keep_running_reads_within_eight_slots() {
        let stats = Arc::new(ApiStats::default());
        let mut writers = Vec::new();
        let mut producers = Vec::new();

        for _ in 0..DEFAULT_API_STREAMING_MEDIA {
            let permit = media_admission::shared(
                stats
                    .try_media(MediaClass::Streaming)
                    .expect("request within the process-wide limit"),
            );
            let (writer, reader) = tokio::io::duplex(1);
            let (started_tx, started_rx) = oneshot::channel();
            let reader = StartedReader {
                inner: reader,
                started: Some(started_tx),
            };
            let (rx, producer) = spawn_raw_body(reader, 1, &permit, SegmentId(1));
            drop(permit);
            tokio::time::timeout(Duration::from_secs(2), started_rx)
                .await
                .expect("raw read started")
                .expect("raw producer signalled its read");
            drop(rx);
            writers.push(writer);
            producers.push(producer);
        }

        assert!(
            stats.try_media(MediaClass::Streaming).is_none(),
            "a ninth raw read must not start while cancelled bodies have work in flight"
        );

        writers[0]
            .write_all(&[1])
            .await
            .expect("release one admitted raw read");
        let mut recovered = vec![
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(permit) = stats.try_media(MediaClass::Streaming) {
                        break permit;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("one slot returned after its real read ended"),
        ];
        assert!(
            stats.try_media(MediaClass::Streaming).is_none(),
            "the other seven running reads must retain their admission slots"
        );

        for writer in &mut writers[1..] {
            writer
                .write_all(&[1])
                .await
                .expect("release admitted raw read");
        }
        for producer in producers {
            producer.await.expect("raw producer task");
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while recovered.len() < DEFAULT_API_STREAMING_MEDIA {
                if let Some(permit) = stats.try_media(MediaClass::Streaming) {
                    recovered.push(permit);
                } else {
                    tokio::task::yield_now().await;
                }
            }
        })
        .await
        .expect("every raw-read admission slot recovered");
    }

    #[tokio::test]
    async fn emitted_raw_chunk_owns_admission_after_the_producer_ends() {
        let stats = Arc::new(ApiStats::default());
        let permit = media_admission::shared(
            stats
                .try_media(MediaClass::Streaming)
                .expect("raw response admission slot"),
        );
        let held: Vec<_> = (1..DEFAULT_API_STREAMING_MEDIA)
            .map(|_| {
                stats
                    .try_media(MediaClass::Streaming)
                    .expect("remaining admission slot")
            })
            .collect();
        let (mut rx, producer) = spawn_raw_body(io::Cursor::new(vec![7]), 1, &permit, SegmentId(1));
        drop(permit);
        let chunk = rx
            .recv()
            .await
            .expect("one raw chunk")
            .expect("raw chunk read");
        assert_eq!(&chunk[..], &[7]);
        assert!(rx.recv().await.is_none(), "raw producer reached its extent");
        producer.await.expect("raw producer task");
        assert!(
            stats.try_media(MediaClass::Streaming).is_none(),
            "transport-held bytes must retain the response admission slot"
        );

        drop(chunk);
        let recovered = stats
            .try_media(MediaClass::Streaming)
            .expect("the final chunk drop returns admission");
        drop(recovered);
        drop(held);
    }
}
