//! Byte-range VOD playlists spanning several recording files.

use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use iris_catalog::{SampleEntryId, SegmentRow, TimeRange};
use iris_core::id::StreamKey;
use iris_core::time::{TIMESCALE, WallUs};
use iris_mp4::fragment::{TRAILER_LEN, decode_trailer};
use iris_mp4::index::{
    FLAG_DISCONTINUITY_BEFORE, FLAG_NEW_PARAMETERS, FLAG_STARTS_WITH_SYNC, IndexHeader,
    IndexRecord, MAX_PERSISTED_INDEX_RECORDS, read_index_file_bounded,
};
use iris_mp4::reader::{FragmentInfo, parse_fragment};
use iris_store::ApprovedRoot;

use crate::auth::{AuthContext, MAX_VOD_CAPABILITY_TTL};
use crate::error::ApiError;
use crate::media_admission::{self, MediaPermit};
use crate::query::{TimeParams, resolve_range};
use crate::segments::stream_key;
use crate::state::{ApiState, MediaClass};

/// Fragments a media segment holds before the next sync fragment may start
/// another; a continuation fragment always joins the open segment, so a
/// segment is about six seconds at a two-second GOP and one whole GOP at
/// longer ones.
const FRAGMENTS_PER_MEDIA_SEGMENT: usize = 3;
/// Maximum catalog rows considered by one time-range response.
pub(crate) const MAX_SEGMENT_ROWS: usize = 4_096;
/// Maximum selected records retained across a response.
pub(crate) const MAX_SELECTED_RECORDS: usize = 100_000;
/// Maximum rendered VOD playlist size.
const MAX_PLAYLIST_BYTES: usize = 16 * 1024 * 1024;
/// Maximum initialization section read by serving paths.
pub(crate) const MAX_INIT_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum complete stored fragment read by serving paths: the 64 MiB
/// ingest payload ceiling plus the 1 MiB moof ceiling and fixed boxes.
pub(crate) const MAX_FRAGMENT_BYTES: u64 = 66 * 1024 * 1024;
const KNOWN_INDEX_FLAGS: u8 =
    FLAG_STARTS_WITH_SYNC | FLAG_DISCONTINUITY_BEFORE | FLAG_NEW_PARAMETERS;
/// Extra validity after the selected media duration for buffering, pausing,
/// and transport delay without exposing a root credential.
const VOD_CAPABILITY_MARGIN: Duration = Duration::from_mins(15);

/// A catalog row plus overlapping fragments and the sync preroll they require.
#[derive(Debug)]
pub(crate) struct SegmentFragments {
    pub(crate) row: SegmentRow,
    pub(crate) records: Vec<IndexRecord>,
    /// The validated media file and the complete extent captured with these records.
    pub(crate) media: ValidatedMedia,
    /// A catalog discontinuity or a skipped row precedes this item.
    pub(crate) discontinuity_before: bool,
}

/// A recording file validated beneath its approved root: where it lives, the
/// physical identity of the inode the sidecar was checked against, and the
/// complete published extent captured with that sidecar.
///
/// Holding this instead of an open descriptor keeps a long selection from
/// retaining thousands of files; [`ValidatedMedia::reopen`] proves that a later
/// open still names the validated inode.
#[derive(Clone, Debug)]
pub(crate) struct ValidatedMedia {
    root: ApprovedRoot,
    path: PathBuf,
    device: u64,
    inode: u64,
    extent: u64,
}

impl ValidatedMedia {
    /// The identity of `file`, read on the blocking pool, as `(device, inode, length)`.
    fn identity(file: &File) -> io::Result<(u64, u64, u64)> {
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino(), metadata.len()))
    }

    fn new(root: ApprovedRoot, path: PathBuf, device: u64, inode: u64, extent: u64) -> Self {
        Self {
            root,
            path,
            device,
            inode,
            extent,
        }
    }

    /// The complete media extent captured with the validated sidecar.
    pub(crate) fn extent(&self) -> u64 {
        self.extent
    }

    /// Reopens the recording beneath its approved root without following any
    /// symlink and requires the validated device and inode and at least the
    /// captured extent. Blocking.
    pub(crate) fn reopen(&self) -> io::Result<File> {
        let file = iris_store::open_segment_beneath(&self.root, &self.path)?;
        let metadata = file.metadata()?;
        if (metadata.dev(), metadata.ino()) != (self.device, self.inode) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recording was replaced after validation",
            ));
        }
        if metadata.len() < self.extent {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "recording is shorter than its validated extent",
            ));
        }
        Ok(file)
    }

    /// [`ValidatedMedia::reopen`] on the blocking pool, as an asynchronous file.
    pub(crate) async fn open(&self) -> io::Result<tokio::fs::File> {
        let media = self.clone();
        let file = tokio::task::spawn_blocking(move || media.reopen())
            .await
            .map_err(io::Error::other)??;
        Ok(tokio::fs::File::from_std(file))
    }
}

#[cfg(test)]
impl ValidatedMedia {
    /// Writes `bytes` as one recording beneath a fresh root in `directory` and
    /// validates it with `extent`.
    pub(crate) fn for_test(directory: &std::path::Path, bytes: &[u8], extent: u64) -> Self {
        let root = iris_store::admit_recording_root(
            &directory.join("video"),
            iris_store::RootExpectation::Fresh,
        )
        .expect("test recording root");
        let path = root.path().join("cam1/main/2023-11-14/221320.000000Z.mp4");
        std::fs::create_dir_all(path.parent().expect("layout parent")).expect("layout");
        std::fs::write(&path, bytes).expect("test recording");
        let file = File::open(&path).expect("open test recording");
        let (device, inode, _) = Self::identity(&file).expect("test recording identity");
        Self::new(root, path, device, inode, extent)
    }

    /// The recording's path beneath its root.
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// A validated sidecar snapshot, the media descriptor it was checked against,
/// and the complete published extent it binds.
#[derive(Debug)]
pub(crate) struct PublishedIndex {
    pub(crate) records: Vec<IndexRecord>,
    pub(crate) media: ValidatedMedia,
    /// The open media file the sidecar was validated against; raw bodies read
    /// only from it.
    pub(crate) file: File,
}

/// The approved root that owns `path`: the longest root path that prefixes it.
fn owning_root<'roots>(
    roots: &'roots [ApprovedRoot],
    path: &std::path::Path,
) -> Option<&'roots ApprovedRoot> {
    roots
        .iter()
        .filter(|root| path.starts_with(root.path()))
        .max_by_key(|root| root.path().components().count())
}

/// Opens a row's media and sidecar together beneath the approved root that
/// owns its path, reads the bounded sidecar from its descriptor, and binds it
/// to the catalog row and the open media file's length.
///
/// A row outside every approved root is counted and answered with
/// [`ApiError::NotFound`]; symlinks, nested mounts, and non-regular entries
/// fail the descriptor-relative opens.
pub(crate) async fn published_index(
    state: &ApiState,
    row: &SegmentRow,
    permit: &MediaPermit,
) -> Result<PublishedIndex, ApiError> {
    let Some(root) = owning_root(&state.recording_roots, &row.path) else {
        // Counted rather than warned per row: historical rows of a removed
        // root could otherwise multiply one range request into thousands of lines.
        state.stats.record_unapproved_row();
        tracing::debug!(
            segment = row.id.0,
            "recording row is outside every approved root; not serving it"
        );
        return Err(ApiError::NotFound);
    };
    let root = root.clone();
    let path = row.path.clone();
    let (root, path, file, header, records, (device, inode, media_len)) =
        media_admission::blocking(permit, move || {
            let (file, index) = iris_store::open_segment_pair_beneath(&root, &path)?;
            let index = index.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "recording has no sidecar")
            })?;
            let (header, records) = read_index_file_bounded(index, MAX_PERSISTED_INDEX_RECORDS)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let identity = ValidatedMedia::identity(&file)?;
            Ok::<_, io::Error>((root, path, file, header, records, identity))
        })
        .await
        .map_err(io::Error::other)??;
    let (records, extent) = validate_index_binding(row, &header, records, media_len)?;
    let media = ValidatedMedia::new(root, path, device, inode, extent);
    Ok(PublishedIndex {
        records,
        media,
        file,
    })
}

/// Checks a sidecar snapshot against its catalog row and media length and
/// returns the records with their complete extent.
///
/// A rebuilt header (crash recovery rewrites the sidecar from a media scan,
/// and the media carries no run identifier or sample-entry hash) stores zero
/// for those two facts. Zero is therefore "unknown" and accepted; a nonzero
/// value must equal the catalog's nonzero value. Every other fact is always
/// checked.
fn validate_index_binding(
    row: &SegmentRow,
    header: &IndexHeader,
    records: Vec<IndexRecord>,
    media_len: u64,
) -> Result<(Vec<IndexRecord>, u64), io::Error> {
    let invalid = |reason| io::Error::new(io::ErrorKind::InvalidData, reason);
    if header.timescale != TIMESCALE || header.flags != 0 || header.init_len != row.init_len {
        return Err(invalid("sidecar header does not match the recording"));
    }
    if row.run_seq != 0 && header.run_id != 0 && header.run_id != row.run_seq {
        return Err(invalid("sidecar run does not match the catalog"));
    }
    if row.sample_entry_hash != 0
        && header.sample_entry_hash != 0
        && header.sample_entry_hash != row.sample_entry_hash
    {
        return Err(invalid("sidecar sample entry does not match the catalog"));
    }
    if u64::from(header.init_len) > MAX_INIT_BYTES {
        return Err(invalid(
            "recording initialization exceeds the serving budget",
        ));
    }
    if header.start_wall != row.wall_start
        || records.first().is_some_and(|record| {
            record.wall != header.start_wall || record.decode_time != row.media_start
        })
    {
        return Err(invalid(
            "sidecar start does not match the catalog or records",
        ));
    }
    if records.iter().any(|record| {
        u64::from(record.len) > MAX_FRAGMENT_BYTES
            || record.duration == 0
            || record.sample_count == 0
            || record.flags & !KNOWN_INDEX_FLAGS != 0
    }) {
        return Err(invalid("sidecar record exceeds serving or format bounds"));
    }
    let extent = records
        .last()
        .map_or(u64::from(header.init_len), IndexRecord::end);
    if extent > media_len {
        return Err(invalid("sidecar record extends past the media file"));
    }
    if row.sealed
        && (extent != row.bytes
            || extent != row.durable_len
            || usize::try_from(row.fragments).ok() != Some(records.len()))
    {
        return Err(invalid("sealed catalog extent does not match the sidecar"));
    }
    Ok((records, extent))
}

/// Parses and checksum-verifies one bounded fragment, then cross-checks every
/// persisted fact used by a transform.
pub(crate) fn validate_fragment_record(
    fragment: &[u8],
    record: &IndexRecord,
) -> Result<FragmentInfo, io::Error> {
    let invalid = |reason| io::Error::new(io::ErrorKind::InvalidData, reason);
    if fragment.len() > usize::try_from(MAX_FRAGMENT_BYTES).unwrap_or(usize::MAX)
        || fragment.len() != record.len as usize
        || fragment.len() < TRAILER_LEN
    {
        return Err(invalid("fragment length does not match its sidecar record"));
    }
    let body_end = fragment.len() - TRAILER_LEN;
    let trailer = decode_trailer(&fragment[body_end..])
        .ok_or_else(|| invalid("fragment trailer is invalid"))?;
    if iris_mp4::crc::crc32c(&fragment[..body_end]) != trailer.crc {
        return Err(invalid("fragment checksum does not match"));
    }
    let info = parse_fragment(fragment).ok_or_else(|| invalid("fragment is malformed"))?;
    if info.len != record.len
        || info.decode_time != record.decode_time
        || info.wall != Some(record.wall)
        || info.duration != record.duration
        || info.sample_count != record.sample_count
        || info.starts_with_sync != record.starts_with_sync()
    {
        return Err(invalid("fragment facts do not match the sidecar record"));
    }
    Ok(info)
}

/// Loads overlapping fragments, extending each clipped start back to a sync fragment.
pub(crate) async fn load_fragments(
    state: &ApiState,
    key: &StreamKey,
    range: TimeRange,
    permit: &MediaPermit,
) -> Result<Vec<SegmentFragments>, ApiError> {
    let rows = state
        .catalog
        .segments_limited(key, range, MAX_SEGMENT_ROWS + 1)
        .await?;
    if rows.len() > MAX_SEGMENT_ROWS {
        return Err(ApiError::Unavailable(
            "recording range exceeds the segment serving budget".into(),
        ));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(rows.len()).map_err(|error| {
        ApiError::Unavailable(format!("cannot reserve segment selection: {error}"))
    })?;
    let mut selected_records = 0usize;
    let mut pending_gap = false;
    for row in rows {
        let published = match published_index(state, &row, permit).await {
            Ok(published) => published,
            // Outside every approved root: already counted.
            Err(ApiError::NotFound) => {
                pending_gap |= !out.is_empty();
                continue;
            }
            Err(error) => {
                tracing::warn!(path = %row.path.display(), %error, "unusable sidecar; skipping");
                pending_gap |= !out.is_empty();
                continue;
            }
        };
        let in_range = clip_records(&published.records, &range);
        if in_range.is_empty() {
            pending_gap |= !out.is_empty();
            continue;
        }
        selected_records = selected_records
            .checked_add(in_range.len())
            .filter(|&records| records <= MAX_SELECTED_RECORDS)
            .ok_or_else(|| {
                ApiError::Unavailable("recording range exceeds the fragment serving budget".into())
            })?;
        let discontinuity_before = row.discontinuity || pending_gap;
        pending_gap = false;
        // The selection keeps identity, not the descriptor: a long range would
        // otherwise hold one open file per row until the response ends.
        out.push(SegmentFragments {
            row,
            records: in_range,
            media: published.media,
            discontinuity_before,
        });
    }
    Ok(out)
}

/// A continuation fragment needs its preceding sync fragment and all intervening
/// fragments. A malformed sidecar without that preroll can resume only at a later sync.
fn clip_records(records: &[IndexRecord], range: &TimeRange) -> Vec<IndexRecord> {
    let Some(first) = records.iter().position(|r| overlaps(r, range)) else {
        return Vec::new();
    };
    let last = records
        .iter()
        .rposition(|r| overlaps(r, range))
        .expect("first overlapping fragment exists");
    let start = records[..=first]
        .iter()
        .rposition(IndexRecord::starts_with_sync)
        .or_else(|| {
            records[first..=last]
                .iter()
                .position(IndexRecord::starts_with_sync)
                .map(|index| first + index)
        });
    match start {
        Some(start) => records[start..=last].to_vec(),
        None => Vec::new(),
    }
}

fn overlaps(record: &IndexRecord, range: &TimeRange) -> bool {
    let start = i128::from(record.wall.get());
    let duration_us = i128::from(record.duration) * 1_000_000 / i128::from(TIMESCALE);
    let end = start + duration_us;
    end > i128::from(range.from.get()) && start < i128::from(range.to.get())
}

/// One media segment: a byte range covering whole fragments.
#[derive(Debug)]
struct MediaSegment {
    offset: u64,
    len: u64,
    duration_ticks: u64,
}

/// Groups fragments into media segments that start at sync fragments.
fn group(records: &[IndexRecord]) -> Vec<MediaSegment> {
    let mut groups: Vec<(MediaSegment, usize)> = Vec::new();
    for r in records {
        match groups.last_mut() {
            Some((seg, n)) if !(r.starts_with_sync() && *n >= FRAGMENTS_PER_MEDIA_SEGMENT) => {
                seg.len = r.end() - seg.offset;
                seg.duration_ticks += u64::from(r.duration);
                *n += 1;
            }
            _ => groups.push((
                MediaSegment {
                    offset: r.offset,
                    len: u64::from(r.len),
                    duration_ticks: u64::from(r.duration),
                },
                1,
            )),
        }
    }
    groups.into_iter().map(|(seg, _)| seg).collect()
}

/// The file URI for a segment, carrying a path-specific capability when auth is on.
pub(crate) fn file_uri(
    segment_id: i64,
    auth: &AuthContext,
    capability_lifetime: Duration,
) -> String {
    let path = format!("/api/segments/{segment_id}/file.mp4");
    match auth.capability_for(&path, capability_lifetime) {
        Some(capability) => format!("{path}?token={capability}"),
        None => path,
    }
}

fn vod_capability_lifetime(range: TimeRange) -> Result<Duration, ApiError> {
    let span_us = range
        .to
        .get()
        .checked_sub(range.from.get())
        .and_then(|span| u64::try_from(span).ok())
        .ok_or_else(|| ApiError::BadRequest("invalid VOD capability range".into()))?;
    Ok((Duration::from_micros(span_us) + VOD_CAPABILITY_MARGIN).min(MAX_VOD_CAPABILITY_TTL))
}

struct BoundedText {
    text: String,
    max: usize,
}

impl BoundedText {
    fn new(max: usize) -> Self {
        Self {
            text: String::new(),
            max,
        }
    }

    fn into_string(self) -> String {
        self.text
    }
}

impl fmt::Write for BoundedText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let next = self.text.len().checked_add(text.len()).ok_or(fmt::Error)?;
        if next > self.max {
            return Err(fmt::Error);
        }
        self.text.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.text.push_str(text);
        Ok(())
    }
}

fn playlist_budget_error(_: fmt::Error) -> ApiError {
    ApiError::Unavailable("VOD playlist exceeds the serving budget".into())
}

/// Renders the VOD playlist.
pub(crate) fn vod_playlist(
    segments: &[SegmentFragments],
    auth: &AuthContext,
    capability_lifetime: Duration,
) -> Result<String, ApiError> {
    let mut body = BoundedText::new(MAX_PLAYLIST_BYTES);
    let mut target_ticks = 0u64;
    let mut last_entry: Option<SampleEntryId> = None;
    for seg in segments {
        if seg
            .records
            .iter()
            .any(|record| record.end() > seg.media.extent())
        {
            return Err(ApiError::Unavailable(
                "VOD byte range exceeds its published extent".into(),
            ));
        }
        let uri = file_uri(seg.row.id.0, auth, capability_lifetime);
        // Sample entries are compared by catalog row, which is unique by CRC
        // and bytes, so a CRC collision between files still gets its marker.
        let discontinuity = last_entry.is_some()
            && (seg.discontinuity_before || last_entry != Some(seg.row.sample_entry_id));
        if discontinuity {
            body.write_str("#EXT-X-DISCONTINUITY\n")
                .map_err(playlist_budget_error)?;
        }
        writeln!(
            body,
            "#EXT-X-MAP:URI=\"{uri}\",BYTERANGE=\"{}@0\"",
            seg.row.init_len
        )
        .map_err(playlist_budget_error)?;
        for record in &seg.records {
            writeln!(
                body,
                "#IRIS-FRAGMENT:WALL-US={},DURATION-TICKS={}",
                record.wall.get(),
                record.duration
            )
            .map_err(playlist_budget_error)?;
        }
        for media in group(&seg.records) {
            target_ticks = target_ticks.max(media.duration_ticks);
            writeln!(
                body,
                "#EXTINF:{:.5},",
                media.duration_ticks as f64 / f64::from(TIMESCALE)
            )
            .map_err(playlist_budget_error)?;
            writeln!(body, "#EXT-X-BYTERANGE:{}@{}", media.len, media.offset)
                .map_err(playlist_budget_error)?;
            writeln!(body, "{uri}").map_err(playlist_budget_error)?;
        }
        last_entry = Some(seg.row.sample_entry_id);
    }
    let target = target_ticks.div_ceil(u64::from(TIMESCALE)).max(1);
    let mut playlist = BoundedText::new(MAX_PLAYLIST_BYTES);
    write!(
        playlist,
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n{}#EXT-X-ENDLIST\n",
        body.into_string()
    )
    .map_err(playlist_budget_error)?;
    Ok(playlist.into_string())
}

pub(crate) async fn vod(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<TimeParams>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Response, ApiError> {
    let permit = state.stats.admit_media(MediaClass::Bounded)?;
    let key = stream_key(&camera, &kind)?;
    let range = resolve_range(&params, WallUs::now())?;
    let capability_lifetime = vod_capability_lifetime(range)?;
    let segments = load_fragments(&state, &key, range, &permit).await?;
    if segments.is_empty() {
        return Err(ApiError::NotFound);
    }
    let text = media_admission::attach(
        Bytes::from(vod_playlist(&segments, &auth, capability_lifetime)?),
        &permit,
    );
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        text,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_catalog::SegmentId;
    use iris_core::id::{CameraId, StreamKind};
    use iris_mp4::index::FLAG_STARTS_WITH_SYNC;

    fn record(offset: u64, len: u32, sync: bool) -> IndexRecord {
        IndexRecord {
            offset,
            len,
            duration: 180_000,
            sample_count: 60,
            flags: if sync { FLAG_STARTS_WITH_SYNC } else { 0 },
            ..IndexRecord::default()
        }
    }

    #[test]
    fn groups_three_fragments_per_media_segment_at_sync_points() {
        let records: Vec<IndexRecord> =
            (0..7).map(|i| record(700 + i * 1000, 1000, true)).collect();
        let groups = group(&records);
        assert_eq!(groups.len(), 3);
        assert_eq!((groups[0].offset, groups[0].len), (700, 3000));
        assert_eq!((groups[2].offset, groups[2].len), (6700, 1000));
        assert_eq!(groups[0].duration_ticks, 540_000);
    }

    /// A segment runs to the first sync fragment after its third fragment;
    /// continuation fragments always join it.
    #[test]
    fn continuation_fragments_join_the_open_media_segment() {
        let records: Vec<IndexRecord> = [true, false, true, false, true, true, false, false, true]
            .iter()
            .enumerate()
            .map(|(i, &sync)| record(i as u64 * 1000, 1000, sync))
            .collect();
        let groups = group(&records);
        let lens: Vec<u64> = groups.iter().map(|segment| segment.len).collect();
        assert_eq!(lens, [4000, 4000, 1000], "S C S C | S S C C | S");
    }

    #[test]
    fn uses_scoped_capabilities_in_uris_without_exposing_the_root() {
        let auth = AuthContext::for_test(&crate::auth::test_secret(), 1_234_567);
        let capability = auth.capability("/api/segments/5/file.mp4").unwrap();
        assert_eq!(
            file_uri(5, &auth, Duration::from_hours(1)),
            format!("/api/segments/5/file.mp4?token={capability}")
        );
        assert!(!file_uri(5, &auth, Duration::from_hours(1)).contains("root bearer"));
        assert_eq!(
            file_uri(5, &AuthContext::disabled(), Duration::from_hours(1)),
            "/api/segments/5/file.mp4"
        );
    }

    #[test]
    fn vod_capability_lifetime_covers_the_range_plus_margin_and_caps_at_seven_days() {
        let one_hour = TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(3_600_000_000),
        };
        assert_eq!(
            vod_capability_lifetime(one_hour).unwrap(),
            Duration::from_mins(75)
        );
        let seven_days = TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(7 * 86_400 * 1_000_000),
        };
        assert_eq!(
            vod_capability_lifetime(seven_days).unwrap(),
            MAX_VOD_CAPABILITY_TTL
        );
    }

    #[test]
    fn clipping_preserves_the_sync_preroll_before_a_continuation() {
        let mut records = vec![
            record(700, 1000, true),
            record(1700, 1000, false),
            record(2700, 1000, false),
            record(3700, 1000, true),
        ];
        for (index, record) in records.iter_mut().enumerate() {
            record.wall = WallUs::new(i64::try_from(index).unwrap() * 2_000_000);
        }
        let selected = clip_records(
            &records,
            &TimeRange {
                from: WallUs::new(4_100_000),
                to: WallUs::new(5_000_000),
            },
        );
        assert_eq!(selected, records[..3]);
        assert!(selected[0].starts_with_sync());
        assert_eq!(group(&selected)[0].len, 3000);
    }

    #[test]
    fn clipping_without_preroll_skips_to_a_sync_fragment() {
        let records = vec![record(700, 1000, false), record(1700, 1000, true)];
        let selected = clip_records(
            &records,
            &TimeRange {
                from: WallUs::new(0),
                to: WallUs::new(2_000_000),
            },
        );
        assert_eq!(selected, records[1..]);
    }

    fn bound_fixture() -> (SegmentRow, IndexHeader, IndexRecord) {
        let record = IndexRecord {
            offset: 700,
            decode_time: 90_000,
            wall: WallUs::new(1_700_000_000_000_000),
            len: 100,
            duration: 180_000,
            sample_count: 60,
            flags: FLAG_STARTS_WITH_SYNC,
            raw_rtp: 0,
        };
        let header = IndexHeader {
            start_wall: record.wall,
            run_id: 7,
            init_len: 700,
            sample_entry_hash: 9,
            ..IndexHeader::default()
        };
        let row = SegmentRow {
            id: SegmentId(1),
            key: StreamKey {
                camera: CameraId::try_new("cam1").unwrap(),
                kind: StreamKind::Main,
            },
            path: "/synthetic/recording.mp4".into(),
            run_seq: 7,
            discontinuity: true,
            wall_start: record.wall,
            wall_end: record.wall,
            media_start: record.decode_time,
            media_end: record.decode_time + u64::from(record.duration),
            init_len: header.init_len,
            durable_len: record.end(),
            fragments: 1,
            bytes: record.end(),
            sealed: true,
            codecs: "hvc1".into(),
            width: 640,
            height: 480,
            sample_entry_hash: header.sample_entry_hash,
            sample_entry_id: SampleEntryId(1),
        };
        (row, header, record)
    }

    #[test]
    fn default_vod_playlist_issues_children_for_the_playback_horizon_and_margin() {
        let issued_at = 10_000;
        let range = TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(3_600_000_000),
        };
        let lifetime = vod_capability_lifetime(range).unwrap();
        let (row, _, record) = bound_fixture();
        let directory = tempfile::tempdir().expect("temporary directory");
        let segments = [SegmentFragments {
            row,
            records: vec![record],
            media: ValidatedMedia::for_test(directory.path(), &[], record.end()),
            discontinuity_before: false,
        }];
        let playlist = vod_playlist(
            &segments,
            &AuthContext::for_test_at(&crate::auth::test_secret(), issued_at),
            lifetime,
        )
        .unwrap();
        let capability = playlist
            .split("?token=")
            .nth(1)
            .and_then(|suffix| suffix.split(['"', '\n']).next())
            .expect("playlist child capability");
        let expires_at: u64 = capability
            .split('.')
            .nth(1)
            .expect("capability expiry")
            .parse()
            .unwrap();
        assert_eq!(expires_at, issued_at + Duration::from_mins(75).as_secs());
        assert!(!playlist.contains("root bearer"));
    }

    /// Two files with byte-identical sample entries share a catalog row and
    /// play through without a marker; a colliding CRC with other bytes is
    /// another row and gets `EXT-X-DISCONTINUITY`.
    #[test]
    fn vod_discontinuities_follow_the_sample_entry_row_not_its_crc() {
        let (row, _, record) = bound_fixture();
        let directory = tempfile::tempdir().expect("temporary directory");
        let segment = |id: i64, entry: SampleEntryId| SegmentFragments {
            row: SegmentRow {
                id: SegmentId(id),
                sample_entry_id: entry,
                ..row.clone()
            },
            records: vec![record],
            media: ValidatedMedia::for_test(directory.path(), &[], record.end()),
            discontinuity_before: false,
        };
        let shared = [segment(1, SampleEntryId(1)), segment(2, SampleEntryId(1))];
        let playlist =
            vod_playlist(&shared, &AuthContext::disabled(), Duration::from_hours(1)).unwrap();
        assert!(!playlist.contains("#EXT-X-DISCONTINUITY"), "{playlist}");
        let colliding = [segment(1, SampleEntryId(1)), segment(2, SampleEntryId(2))];
        let playlist = vod_playlist(
            &colliding,
            &AuthContext::disabled(),
            Duration::from_hours(1),
        )
        .unwrap();
        assert_eq!(
            playlist.matches("#EXT-X-DISCONTINUITY\n").count(),
            1,
            "{playlist}"
        );
    }

    #[test]
    fn sidecar_binding_rejects_mismatched_facts_and_extents() {
        let (row, header, record) = bound_fixture();
        assert_eq!(
            validate_index_binding(&row, &header, vec![record], record.end())
                .unwrap()
                .1,
            record.end()
        );
        for bad_header in [
            IndexHeader {
                timescale: 1,
                ..header
            },
            IndexHeader {
                init_len: 699,
                ..header
            },
            IndexHeader {
                run_id: 8,
                ..header
            },
            IndexHeader {
                sample_entry_hash: 10,
                ..header
            },
            IndexHeader {
                start_wall: WallUs::new(header.start_wall.get() + 1),
                ..header
            },
        ] {
            assert!(validate_index_binding(&row, &bad_header, vec![record], record.end()).is_err());
        }
        assert!(validate_index_binding(&row, &header, vec![record], record.end() - 1).is_err());
        let oversized = IndexRecord {
            len: u32::MAX,
            ..record
        };
        assert!(validate_index_binding(&row, &header, vec![oversized], u64::MAX).is_err());
    }

    #[test]
    fn reopening_requires_the_validated_inode_and_extent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let media = ValidatedMedia::for_test(directory.path(), &[7; 64], 64);
        let mut buffer = Vec::new();
        io::Read::read_to_end(&mut media.reopen().expect("unchanged file"), &mut buffer)
            .expect("read");
        assert_eq!(buffer, [7; 64]);

        // A replacement at the same path is a different inode, even with equal bytes.
        let moved = media.path().with_extension("moved");
        std::fs::rename(media.path(), &moved).expect("move the validated file away");
        std::fs::write(media.path(), [7; 64]).expect("replacement");
        assert_eq!(
            media.reopen().expect_err("replaced").kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_file(media.path()).expect("remove the replacement");
        std::fs::rename(&moved, media.path()).expect("restore the validated file");
        assert!(
            media.reopen().is_ok(),
            "the validated inode is accepted again"
        );

        File::options()
            .write(true)
            .open(media.path())
            .expect("open for truncation")
            .set_len(63)
            .expect("truncate");
        assert_eq!(
            media.reopen().expect_err("shorter than its extent").kind(),
            io::ErrorKind::UnexpectedEof
        );

        std::fs::remove_file(media.path()).expect("remove");
        std::os::unix::fs::symlink(&moved, media.path()).expect("symlink");
        assert!(media.reopen().is_err(), "a symlink is never followed");
    }

    #[test]
    fn rebuilt_sidecar_headers_with_unknown_run_and_sample_entry_are_accepted() {
        let (row, header, record) = bound_fixture();
        assert_ne!(row.run_seq, 0);
        assert_ne!(row.sample_entry_hash, 0);
        let rebuilt = IndexHeader {
            run_id: 0,
            sample_entry_hash: 0,
            ..header
        };
        let (records, extent) = validate_index_binding(&row, &rebuilt, vec![record], record.end())
            .expect("zero run and hash mean unknown");
        assert_eq!(records, vec![record]);
        assert_eq!(extent, record.end());
        for partly_known in [
            IndexHeader {
                run_id: 0,
                ..header
            },
            IndexHeader {
                sample_entry_hash: 0,
                ..header
            },
        ] {
            assert!(
                validate_index_binding(&row, &partly_known, vec![record], record.end()).is_ok()
            );
        }
    }

    #[test]
    fn rebuilt_sidecar_headers_still_reject_nonzero_mismatches_and_other_facts() {
        let (row, header, record) = bound_fixture();
        for mismatched in [
            IndexHeader {
                run_id: row.run_seq + 1,
                sample_entry_hash: 0,
                ..header
            },
            IndexHeader {
                run_id: 0,
                sample_entry_hash: row.sample_entry_hash + 1,
                ..header
            },
            IndexHeader {
                run_id: 0,
                sample_entry_hash: 0,
                init_len: header.init_len - 1,
                ..header
            },
            IndexHeader {
                run_id: 0,
                sample_entry_hash: 0,
                start_wall: WallUs::new(header.start_wall.get() + 1),
                ..header
            },
        ] {
            assert!(
                validate_index_binding(&row, &mismatched, vec![record], record.end()).is_err(),
                "{mismatched:?}"
            );
        }
    }

    #[test]
    fn the_owning_root_is_the_longest_component_prefix_and_outside_paths_have_none() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let admit = |relative: &str| {
            iris_store::admit_recording_root(
                &directory.path().join(relative),
                iris_store::RootExpectation::Fresh,
            )
            .expect("test root")
        };
        let outer = admit("video");
        let nested = admit("video/nested");
        let sibling = admit("video-b");
        let roots = [outer.clone(), nested.clone(), sibling.clone()];
        let layout = "cam1/main/2023-11-14/000000.000000Z.mp4";
        assert_eq!(
            owning_root(&roots, &outer.path().join(layout)),
            Some(&outer)
        );
        assert_eq!(
            owning_root(&roots, &nested.path().join(layout)),
            Some(&nested)
        );
        assert_eq!(
            owning_root(&roots, &sibling.path().join(layout)),
            Some(&sibling),
            "prefixes compare whole components"
        );
        assert_eq!(
            owning_root(&roots, &directory.path().join("elsewhere").join(layout)),
            None
        );
        assert_eq!(owning_root(&[], &outer.path().join(layout)), None);
    }

    #[test]
    fn overlap_math_is_defined_at_signed_wall_time_edges() {
        let high = IndexRecord {
            wall: WallUs::new(i64::MAX - 1),
            duration: u32::MAX,
            ..record(0, 100, true)
        };
        assert!(overlaps(
            &high,
            &TimeRange {
                from: WallUs::new(i64::MAX - 10),
                to: WallUs::new(i64::MAX),
            }
        ));
        let low = IndexRecord {
            wall: WallUs::new(i64::MIN),
            duration: 1,
            ..record(0, 100, true)
        };
        assert!(overlaps(
            &low,
            &TimeRange {
                from: WallUs::new(i64::MIN),
                to: WallUs::new(i64::MIN + 1),
            }
        ));
    }
}
