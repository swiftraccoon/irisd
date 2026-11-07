//! Public row types, identifiers, and the change enum.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize, Serializer};
use thiserror::Error;

use iris_core::event::Event;
use iris_core::id::{CameraId, StreamKey};
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;

/// Row id of a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SegmentId(pub i64);

/// Row id of an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub i64);

/// Stable continuation position for event listings ordered by receipt time and row id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventCursor {
    /// Receipt time of the last event returned by the preceding page.
    pub time: WallUs,
    /// Row id of the last event returned by the preceding page.
    pub id: EventId,
}

/// Stable continuation position for active protections ordered by retention deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventProtectionCursor {
    /// Retention deadline of the last protection returned by the preceding page.
    pub retain_until: WallUs,
    /// Row id of the last protection returned by the preceding page.
    pub id: EventId,
}

/// Minimal event fields needed to decide whether footage is protected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventProtection {
    /// Stable event row id used for paging equal timestamps.
    pub id: EventId,
    /// Camera whose footage the event protects.
    pub camera: CameraId,
    /// Authoritative event placement on the NVR clock.
    pub time: WallUs,
    /// Time through which the protection remains active.
    pub retain_until: WallUs,
}

/// One bounded page of sealed segments inspected for retention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPage {
    /// Rows that were unprotected when the catalog query ran.
    pub rows: Vec<SegmentRow>,
    /// Last raw indexed segment inspected, including when every inspected row was protected.
    pub next: Option<(WallUs, SegmentId)>,
    /// Number of raw indexed segments inspected before protection filtering.
    pub scanned: usize,
}

/// Outcome of conditionally recording durable deletion intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum DeleteMarkResult {
    /// Deletion intent was committed under the FULL durability barrier.
    Marked,
    /// An active retained event overlaps the segment.
    Protected,
    /// No ordinary sealed row has this id.
    NotFound,
}

/// A half-open wall-clock interval `[from, to)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimeRange {
    /// Inclusive start.
    pub from: WallUs,
    /// Exclusive end.
    pub to: WallUs,
}

/// One recording file as the catalog knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SegmentRow {
    /// Row id.
    pub id: SegmentId,
    /// Owning camera and stream kind.
    #[serde(serialize_with = "serialize_key")]
    pub key: StreamKey,
    /// Path of the `.mp4` file.
    pub path: PathBuf,
    /// The writer's run counter when the file was opened.
    pub run_seq: u64,
    /// Whether a timing discontinuity precedes the file.
    pub discontinuity: bool,
    /// Wall time of the first fragment.
    pub wall_start: WallUs,
    /// Wall time of the last fragment written (equals `wall_start` until the first progress report).
    pub wall_end: WallUs,
    /// Decode time of the first fragment, in 90 kHz ticks.
    pub media_start: u64,
    /// End of the last fragment's decode time, in 90 kHz ticks.
    pub media_end: u64,
    /// Length of `ftyp` + `moov`.
    pub init_len: u32,
    /// Bytes known to be on the device.
    pub durable_len: u64,
    /// Fragments written.
    pub fragments: u32,
    /// File length (final once sealed).
    pub bytes: u64,
    /// Whether the file is closed and complete.
    pub sealed: bool,
    /// RFC 6381 codecs string of the sample entry.
    pub codecs: String,
    /// Display width.
    pub width: u16,
    /// Display height.
    pub height: u16,
    /// CRC-32C of the sample entry.
    pub sample_entry_hash: u32,
}

fn serialize_key<S: Serializer>(key: &StreamKey, serializer: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Key<'a> {
        camera: &'a CameraId,
        kind: iris_core::id::StreamKind,
    }
    Key {
        camera: &key.camera,
        kind: key.kind,
    }
    .serialize(serializer)
}

/// A stored event with its row id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EventRow {
    /// Row id.
    pub id: EventId,
    /// The event.
    pub event: Event,
    /// Until when the footage around the event is kept from retention, when set.
    pub retain_until: Option<WallUs>,
}

/// A camera known to the catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraRow {
    /// Stable identifier.
    pub id: CameraId,
    /// Human-readable name.
    pub name: String,
    /// Vendor hint, if any.
    pub vendor: Option<String>,
}

/// Aggregate counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogStats {
    /// Segment rows.
    pub segments: u64,
    /// Bytes of sealed segments.
    pub sealed_bytes: u64,
    /// Segment rows with durable deletion intent whose pair removal has not
    /// completed. Retention retries them every pass; a persistent count points at
    /// a removal fault or an unavailable root.
    pub deleting_segments: u64,
    /// Bytes of the rows counted by `deleting_segments`; they still count toward
    /// the quota total until their rows are removed.
    pub deleting_bytes: u64,
    /// Event rows.
    pub events: u64,
    /// Logical bytes owned by event strings.
    pub event_bytes: u64,
    /// Events with an active retention mark.
    pub retained_events: u64,
    /// Event rows removed by lifecycle maintenance since schema version 6.
    pub pruned_events: u64,
    /// Size of the database file.
    pub db_bytes: u64,
    /// Size of the write-ahead log file, zero when absent.
    pub wal_bytes: u64,
}

/// A fire-and-forget change, applied in send order and committed with the
/// other pending changes in one transaction within 100 ms, at 1,000 pending,
/// or before the next query, whichever comes first.
#[derive(Clone, Debug)]
pub enum Change {
    /// A recording file was created.
    SegmentOpened {
        /// Owning stream.
        key: StreamKey,
        /// Path of the `.mp4` file.
        path: PathBuf,
        /// The writer's run counter.
        run_seq: u64,
        /// Whether a timing discontinuity precedes the file.
        discontinuity: bool,
        /// Wall time of the first fragment.
        wall_start: WallUs,
        /// Decode time of the first fragment.
        media_start: u64,
        /// Length of `ftyp` + `moov`.
        init_len: u32,
        /// Codec configuration in force.
        sample_entry: Arc<SampleEntry>,
    },
    /// Data up to `durable_len` reached the device.
    SegmentProgress {
        /// Path of the `.mp4` file.
        path: PathBuf,
        /// Bytes durable.
        durable_len: u64,
        /// Fragments durable.
        fragments: u32,
        /// Wall time of the last durable fragment.
        wall_end: WallUs,
        /// End of the last durable fragment's decode time.
        media_end: u64,
    },
    /// The file was closed and is complete.
    SegmentSealed {
        /// Path of the `.mp4` file.
        path: PathBuf,
        /// Final file length.
        len: u64,
        /// Final fragment count.
        fragments: u32,
        /// Wall time of the last fragment.
        wall_end: WallUs,
        /// End of the last fragment's decode time.
        media_end: u64,
    },
    /// A camera event.
    Event(Event),
}

/// A sealed file found on disk, to be adopted into the catalog.
#[derive(Clone, Debug)]
pub struct NewSegment {
    /// Owning stream.
    pub key: StreamKey,
    /// Path of the `.mp4` file.
    pub path: PathBuf,
    /// The writer's run counter, if known.
    pub run_seq: u64,
    /// Whether a timing discontinuity precedes the file.
    pub discontinuity: bool,
    /// Wall time of the first fragment.
    pub wall_start: WallUs,
    /// Wall time of the last fragment.
    pub wall_end: WallUs,
    /// Decode time of the first fragment.
    pub media_start: u64,
    /// End of the last fragment's decode time.
    pub media_end: u64,
    /// Length of `ftyp` + `moov`.
    pub init_len: u32,
    /// File length.
    pub len: u64,
    /// Fragment count.
    pub fragments: u32,
    /// Codec configuration, when the scan recovered it.
    pub sample_entry: Option<Arc<SampleEntry>>,
}

/// Errors from the catalog.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// SQLite failed.
    #[error("catalog database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database was created by a catalog schema this binary cannot safely write.
    #[error("unsupported catalog schema version {found}; this binary supports up to {supported}")]
    UnsupportedSchema {
        /// Version advertised by the database.
        found: i64,
        /// Highest version this binary understands.
        supported: i64,
    },
    /// A persisted value violates the catalog schema's typed invariants.
    #[error("catalog is corrupt: {0}")]
    Corrupt(String),
    /// A single persisted row exceeds the aggregate event-read budget.
    #[error("catalog event row requires {bytes} bytes; read budget is {max_bytes} bytes")]
    EventReadTooLarge {
        /// Approximate owned bytes required by the row.
        bytes: usize,
        /// Maximum aggregate owned bytes for one event listing.
        max_bytes: usize,
    },
    /// The bounded catalog command ingress has no ordinary admission capacity.
    #[error("catalog command queue is overloaded")]
    Overloaded,
    /// Active event-retention marks reached their state-database budget.
    #[error("active event retention capacity is exhausted")]
    EventRetentionLimit,
    /// An event could not fit after one bounded inactive-pruning step.
    #[error("event state capacity is exhausted")]
    EventStorageLimit,
    /// The catalog thread has stopped.
    #[error("catalog is closed")]
    Closed,
    /// Filesystem failure while opening the database.
    #[error("catalog I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A path the caller named is not in the catalog.
    #[error("no segment at path {0}")]
    NotFound(PathBuf),
    /// No event has the id.
    #[error("no event {0}")]
    UnknownEvent(i64),
}
