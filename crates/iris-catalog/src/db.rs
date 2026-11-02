//! The SQLite connection and every statement run against it. Used only from
//! the catalog thread.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, named_params, params};
use tracing::warn;

use crate::types::{
    CameraRow, CatalogError, CatalogStats, Change, EventId, EventRow, NewSegment, SegmentId,
    SegmentRow, TimeRange,
};
use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;

const SCHEMA_VERSION: i64 = 2;

const SCHEMA_V1: &str = "
CREATE TABLE schema_version (version INTEGER NOT NULL);
CREATE TABLE camera (
    id     TEXT PRIMARY KEY,
    name   TEXT NOT NULL,
    vendor TEXT
);
CREATE TABLE sample_entry (
    id     INTEGER PRIMARY KEY,
    hash   INTEGER NOT NULL UNIQUE,
    codecs TEXT NOT NULL,
    width  INTEGER NOT NULL,
    height INTEGER NOT NULL,
    config BLOB NOT NULL
);
CREATE TABLE segment (
    id              INTEGER PRIMARY KEY,
    camera_id       TEXT NOT NULL,
    kind            TEXT NOT NULL,
    path            TEXT NOT NULL UNIQUE,
    run_seq         INTEGER NOT NULL,
    discontinuity   INTEGER NOT NULL,
    wall_start_us   INTEGER NOT NULL,
    wall_end_us     INTEGER NOT NULL,
    media_start     INTEGER NOT NULL,
    media_end       INTEGER NOT NULL,
    init_len        INTEGER NOT NULL,
    durable_len     INTEGER NOT NULL,
    fragments       INTEGER NOT NULL,
    bytes           INTEGER NOT NULL,
    sealed          INTEGER NOT NULL,
    sample_entry_id INTEGER NOT NULL REFERENCES sample_entry(id)
);
CREATE INDEX segment_stream_time ON segment(camera_id, kind, wall_start_us);
CREATE TABLE event (
    id             INTEGER PRIMARY KEY,
    camera_id      TEXT NOT NULL,
    source         TEXT NOT NULL,
    kind           TEXT NOT NULL,
    state          TEXT NOT NULL,
    nvr_time_us    INTEGER NOT NULL,
    camera_time_us INTEGER,
    data           TEXT
);
CREATE INDEX event_camera_time ON event(camera_id, nvr_time_us);
";

/// Version 2: an event may be retained, which keeps the footage around it from
/// retention until the given time.
const SCHEMA_V2: &str = "ALTER TABLE event ADD COLUMN retain_until_us INTEGER;";

const SEGMENT_COLUMNS: &str = "s.id, s.camera_id, s.kind, s.path, s.run_seq, s.discontinuity, \
     s.wall_start_us, s.wall_end_us, s.media_start, s.media_end, s.init_len, s.durable_len, \
     s.fragments, s.bytes, s.sealed, e.codecs, e.width, e.height, e.hash";

/// The open database.
pub struct Db {
    conn: Connection,
    path: PathBuf,
}

impl Db {
    /// Opens (creating if needed) and migrates the database at `path`.
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut db = Self {
            conn,
            path: path.to_path_buf(),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<(), CatalogError> {
        let has_version: bool = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
            [],
            |row| row.get::<_, i64>(0).map(|n| n > 0),
        )?;
        let version: i64 = if has_version {
            self.conn.query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |r| r.get(0),
            )?
        } else {
            0
        };
        if version < SCHEMA_VERSION {
            let tx = self.conn.transaction()?;
            if version < 1 {
                tx.execute_batch(SCHEMA_V1)?;
            }
            if version < 2 {
                tx.execute_batch(SCHEMA_V2)?;
            }
            tx.execute("DELETE FROM schema_version", [])?;
            tx.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Applies a batch of changes in one transaction.
    pub fn apply(&mut self, changes: &[Change]) -> Result<(), CatalogError> {
        let tx = self.conn.transaction()?;
        for change in changes {
            match change {
                Change::SegmentOpened {
                    key,
                    path,
                    run_seq,
                    discontinuity,
                    wall_start,
                    media_start,
                    init_len,
                    sample_entry,
                } => {
                    let entry_id = upsert_sample_entry(&tx, Some(sample_entry))?;
                    tx.execute(
                        "INSERT INTO segment (camera_id, kind, path, run_seq, discontinuity, \
                         wall_start_us, wall_end_us, media_start, media_end, init_len, \
                         durable_len, fragments, bytes, sealed, sample_entry_id) \
                         VALUES (:camera, :kind, :path, :run_seq, :disc, :wall, :wall, :media, \
                         :media, :init_len, :init_len, 0, :init_len, 0, :entry) \
                         ON CONFLICT(path) DO UPDATE SET camera_id = excluded.camera_id, \
                         kind = excluded.kind, run_seq = excluded.run_seq, \
                         discontinuity = excluded.discontinuity, \
                         wall_start_us = excluded.wall_start_us, \
                         wall_end_us = excluded.wall_end_us, media_start = excluded.media_start, \
                         media_end = excluded.media_end, init_len = excluded.init_len, \
                         durable_len = excluded.durable_len, fragments = 0, \
                         bytes = excluded.bytes, sealed = 0, \
                         sample_entry_id = excluded.sample_entry_id",
                        named_params! {
                            ":camera": key.camera.as_str(),
                            ":kind": key.kind.as_str(),
                            ":path": path_text(path),
                            ":run_seq": as_i64(*run_seq),
                            ":disc": *discontinuity,
                            ":wall": wall_start.get(),
                            ":media": as_i64(*media_start),
                            ":init_len": i64::from(*init_len),
                            ":entry": entry_id,
                        },
                    )?;
                }
                Change::SegmentProgress {
                    path,
                    durable_len,
                    fragments,
                    wall_end,
                    media_end,
                } => {
                    let updated = tx.execute(
                        "UPDATE segment SET durable_len = :durable, fragments = :fragments, \
                         wall_end_us = :wall_end, media_end = :media_end, \
                         bytes = MAX(bytes, :durable) WHERE path = :path",
                        named_params! {
                            ":durable": as_i64(*durable_len),
                            ":fragments": i64::from(*fragments),
                            ":wall_end": wall_end.get(),
                            ":media_end": as_i64(*media_end),
                            ":path": path_text(path),
                        },
                    )?;
                    if updated == 0 {
                        warn!(path = %path.display(), "progress for a segment the catalog does not know");
                    }
                }
                Change::SegmentSealed {
                    path,
                    len,
                    fragments,
                    wall_end,
                    media_end,
                } => {
                    let updated = tx.execute(
                        "UPDATE segment SET sealed = 1, bytes = :len, durable_len = :len, \
                         fragments = :fragments, wall_end_us = :wall_end, media_end = :media_end \
                         WHERE path = :path",
                        named_params! {
                            ":len": as_i64(*len),
                            ":fragments": i64::from(*fragments),
                            ":wall_end": wall_end.get(),
                            ":media_end": as_i64(*media_end),
                            ":path": path_text(path),
                        },
                    )?;
                    if updated == 0 {
                        warn!(path = %path.display(), "seal for a segment the catalog does not know");
                    }
                }
                Change::Event(event) => {
                    insert_event(&tx, event)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Inserts or updates a camera.
    pub fn ensure_camera(
        &mut self,
        id: &CameraId,
        name: &str,
        vendor: Option<&str>,
    ) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO camera (id, name, vendor) VALUES (?1, ?2, ?3) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, vendor = excluded.vendor",
            params![id.as_str(), name, vendor],
        )?;
        Ok(())
    }

    /// Lists cameras by id.
    pub fn cameras(&mut self) -> Result<Vec<CameraRow>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, name, vendor FROM camera ORDER BY id")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, name, vendor)| {
                CameraId::try_new(&id)
                    .ok()
                    .map(|id| CameraRow { id, name, vendor })
            })
            .collect())
    }

    /// Segments of a stream overlapping `range`, by start time. An unsealed
    /// segment is treated as extending to `now`.
    pub fn segments(
        &mut self,
        key: &StreamKey,
        range: TimeRange,
    ) -> Result<Vec<SegmentRow>, CatalogError> {
        let now = WallUs::now();
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} FROM segment s JOIN sample_entry e ON e.id = s.sample_entry_id \
             WHERE s.camera_id = :camera AND s.kind = :kind AND s.wall_start_us < :to \
             AND (CASE WHEN s.sealed = 0 THEN :now ELSE s.wall_end_us END) > :from \
             ORDER BY s.wall_start_us"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":camera": key.camera.as_str(),
                    ":kind": key.kind.as_str(),
                    ":to": range.to.get(),
                    ":from": range.from.get(),
                    ":now": now.get(),
                },
                segment_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// One segment by id.
    pub fn segment(&mut self, id: SegmentId) -> Result<Option<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} FROM segment s JOIN sample_entry e ON e.id = s.sample_entry_id \
             WHERE s.id = ?1"
        );
        let row = self
            .conn
            .query_row(&sql, params![id.0], segment_row)
            .optional()?;
        Ok(row.flatten())
    }

    /// One segment by path.
    pub fn segment_by_path(&mut self, path: &Path) -> Result<Option<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} FROM segment s JOIN sample_entry e ON e.id = s.sample_entry_id \
             WHERE s.path = ?1"
        );
        let row = self
            .conn
            .query_row(&sql, params![path_text(path)], segment_row)
            .optional()?;
        Ok(row.flatten())
    }

    /// Unsealed segments, by start time.
    pub fn writing_segments(&mut self) -> Result<Vec<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} FROM segment s JOIN sample_entry e ON e.id = s.sample_entry_id \
             WHERE s.sealed = 0 ORDER BY s.wall_start_us"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map([], segment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// The oldest sealed segments, by start time.
    pub fn oldest_sealed(&mut self, limit: usize) -> Result<Vec<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} FROM segment s JOIN sample_entry e ON e.id = s.sample_entry_id \
             WHERE s.sealed = 1 ORDER BY s.wall_start_us LIMIT ?1"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![limit], segment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// The sealed segment of `key` that ends last.
    pub fn newest_sealed(&mut self, key: &StreamKey) -> Result<Option<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} FROM segment s JOIN sample_entry e ON e.id = s.sample_entry_id \
             WHERE s.camera_id = ?1 AND s.kind = ?2 AND s.sealed = 1 \
             ORDER BY s.wall_end_us DESC LIMIT 1"
        );
        let row = self
            .conn
            .query_row(
                &sql,
                params![key.camera.as_str(), key.kind.as_str()],
                segment_row,
            )
            .optional()?;
        Ok(row.flatten())
    }

    /// Bytes of every segment in the catalog, open files at their written length.
    pub fn total_bytes(&mut self) -> Result<u64, CatalogError> {
        let total: i64 =
            self.conn
                .query_row("SELECT COALESCE(SUM(bytes), 0) FROM segment", [], |row| {
                    row.get(0)
                })?;
        Ok(u64::try_from(total).unwrap_or(0))
    }

    /// Deletes a segment row.
    pub fn delete_segment(&mut self, id: SegmentId) -> Result<(), CatalogError> {
        self.conn
            .execute("DELETE FROM segment WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    /// Marks a segment sealed with its final lengths.
    pub fn mark_sealed(
        &mut self,
        path: &Path,
        len: u64,
        fragments: u32,
        wall_end: WallUs,
        media_end: u64,
    ) -> Result<(), CatalogError> {
        let updated = self.conn.execute(
            "UPDATE segment SET sealed = 1, bytes = :len, durable_len = :len, fragments = :fragments, \
             wall_end_us = :wall_end, media_end = :media_end WHERE path = :path",
            named_params! {
                ":len": as_i64(len),
                ":fragments": i64::from(fragments),
                ":wall_end": wall_end.get(),
                ":media_end": as_i64(media_end),
                ":path": path_text(path),
            },
        )?;
        if updated == 0 {
            return Err(CatalogError::NotFound(path.to_path_buf()));
        }
        Ok(())
    }

    /// Inserts a sealed segment found on disk.
    pub fn adopt_segment(&mut self, row: &NewSegment) -> Result<SegmentId, CatalogError> {
        let tx = self.conn.transaction()?;
        let entry_id = upsert_sample_entry(&tx, row.sample_entry.as_deref())?;
        tx.execute(
            "INSERT INTO segment (camera_id, kind, path, run_seq, discontinuity, wall_start_us, \
             wall_end_us, media_start, media_end, init_len, durable_len, fragments, bytes, sealed, \
             sample_entry_id) VALUES (:camera, :kind, :path, :run_seq, :disc, :wall_start, \
             :wall_end, :media_start, :media_end, :init_len, :len, :fragments, :len, 1, :entry) \
             ON CONFLICT(path) DO UPDATE SET run_seq = excluded.run_seq, \
             discontinuity = excluded.discontinuity, wall_start_us = excluded.wall_start_us, \
             wall_end_us = excluded.wall_end_us, media_start = excluded.media_start, \
             media_end = excluded.media_end, init_len = excluded.init_len, \
             durable_len = excluded.durable_len, fragments = excluded.fragments, \
             bytes = excluded.bytes, sealed = 1, sample_entry_id = excluded.sample_entry_id",
            named_params! {
                ":camera": row.key.camera.as_str(),
                ":kind": row.key.kind.as_str(),
                ":path": path_text(&row.path),
                ":run_seq": as_i64(row.run_seq),
                ":disc": row.discontinuity,
                ":wall_start": row.wall_start.get(),
                ":wall_end": row.wall_end.get(),
                ":media_start": as_i64(row.media_start),
                ":media_end": as_i64(row.media_end),
                ":init_len": i64::from(row.init_len),
                ":len": as_i64(row.len),
                ":fragments": i64::from(row.fragments),
                ":entry": entry_id,
            },
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM segment WHERE path = ?1",
            params![path_text(&row.path)],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(SegmentId(id))
    }

    /// Events in `range`, optionally for one camera, oldest first.
    pub fn events(
        &mut self,
        camera: Option<&CameraId>,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<EventRow>, CatalogError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, camera_id, source, kind, state, nvr_time_us, camera_time_us, data, \
             retain_until_us FROM event \
             WHERE (:camera IS NULL OR camera_id = :camera) AND nvr_time_us >= :from \
             AND nvr_time_us < :to ORDER BY nvr_time_us, id LIMIT :limit",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(
                named_params! {
                    ":camera": camera.map(CameraId::as_str),
                    ":from": range.from.get(),
                    ":to": range.to.get(),
                    ":limit": limit,
                },
                event_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// Sets (or clears, with `None`) the time until which an event's footage
    /// is kept, and returns the event.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::UnknownEvent`] when no event has that id.
    pub fn retain_event(
        &mut self,
        id: EventId,
        until: Option<WallUs>,
    ) -> Result<EventRow, CatalogError> {
        let changed = self.conn.execute(
            "UPDATE event SET retain_until_us = ?1 WHERE id = ?2",
            params![until.map(WallUs::get), id.0],
        )?;
        if changed == 0 {
            return Err(CatalogError::UnknownEvent(id.0));
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, camera_id, source, kind, state, nvr_time_us, camera_time_us, data, \
             retain_until_us FROM event WHERE id = ?1",
        )?;
        let row = stmt.query_row(params![id.0], event_row)?;
        row.ok_or(CatalogError::UnknownEvent(id.0))
    }

    /// Events whose retention time is after `now`, oldest first.
    pub fn retained_events(&mut self, now: WallUs) -> Result<Vec<EventRow>, CatalogError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, camera_id, source, kind, state, nvr_time_us, camera_time_us, data, \
             retain_until_us FROM event WHERE retain_until_us > ?1 ORDER BY nvr_time_us, id",
        )?;
        let rows = stmt
            .query_map(params![now.get()], event_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// The oldest sealed segments starting after `after`, at most `limit`.
    pub fn oldest_sealed_after(
        &mut self,
        after: WallUs,
        limit: usize,
    ) -> Result<Vec<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} FROM segment s JOIN sample_entry e ON e.id = s.sample_entry_id \
             WHERE s.sealed = 1 AND s.wall_start_us > ?1 ORDER BY s.wall_start_us LIMIT ?2"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![after.get(), limit], segment_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// Inserts an event and returns its id.
    pub fn insert_event(&mut self, event: &Event) -> Result<EventId, CatalogError> {
        let tx = self.conn.transaction()?;
        let id = insert_event(&tx, event)?;
        tx.commit()?;
        Ok(id)
    }

    /// Aggregate counts.
    pub fn stats(&mut self) -> Result<CatalogStats, CatalogError> {
        let (segments, sealed_bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN sealed = 1 THEN bytes ELSE 0 END), 0) FROM segment",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let events: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?;
        Ok(CatalogStats {
            segments: segments.max(0) as u64,
            sealed_bytes: sealed_bytes.max(0) as u64,
            events: events.max(0) as u64,
            db_bytes: file_len(&self.path),
            wal_bytes: file_len(&wal_path(&self.path)),
        })
    }
}

fn upsert_sample_entry(
    tx: &rusqlite::Transaction<'_>,
    entry: Option<&SampleEntry>,
) -> Result<i64, CatalogError> {
    let (hash, codecs, width, height, config): (i64, &str, i64, i64, &[u8]) = match entry {
        Some(e) => (
            i64::from(e.hash),
            &e.codecs,
            i64::from(e.width),
            i64::from(e.height),
            &e.bytes,
        ),
        None => (0, "unknown", 0, 0, &[]),
    };
    tx.execute(
        "INSERT INTO sample_entry (hash, codecs, width, height, config) VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(hash) DO NOTHING",
        params![hash, codecs, width, height, config],
    )?;
    let id: i64 = tx.query_row(
        "SELECT id FROM sample_entry WHERE hash = ?1",
        params![hash],
        |r| r.get(0),
    )?;
    Ok(id)
}

fn insert_event(tx: &rusqlite::Transaction<'_>, event: &Event) -> Result<EventId, CatalogError> {
    let bounded = event.clone().bounded();
    tx.execute(
        "INSERT INTO event (camera_id, source, kind, state, nvr_time_us, camera_time_us, data) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            bounded.camera.as_str(),
            source_name(bounded.source),
            kind_name(&bounded.kind),
            state_name(bounded.state),
            bounded.nvr_time.get(),
            bounded.camera_time.map(WallUs::get),
            bounded.data,
        ],
    )?;
    Ok(EventId(tx.last_insert_rowid()))
}

/// Length of the file at `path`, or zero when it is absent or unreadable.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// The write-ahead log that SQLite keeps next to `db`: its name with `-wal` appended.
fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// Builds a [`SegmentRow`]; `None` when a stored id or kind is malformed.
fn segment_row(row: &Row<'_>) -> rusqlite::Result<Option<SegmentRow>> {
    let camera = CameraId::try_new(&row.get::<_, String>(1)?).ok();
    let kind = row.get::<_, String>(2)?.parse::<StreamKind>().ok();
    let (Some(camera), Some(kind)) = (camera, kind) else {
        return Ok(None);
    };
    Ok(Some(SegmentRow {
        id: SegmentId(row.get(0)?),
        key: StreamKey { camera, kind },
        path: PathBuf::from(row.get::<_, String>(3)?),
        run_seq: as_u64(row.get(4)?),
        discontinuity: row.get(5)?,
        wall_start: WallUs::new(row.get(6)?),
        wall_end: WallUs::new(row.get(7)?),
        media_start: as_u64(row.get(8)?),
        media_end: as_u64(row.get(9)?),
        init_len: as_u32(row.get(10)?),
        durable_len: as_u64(row.get(11)?),
        fragments: as_u32(row.get(12)?),
        bytes: as_u64(row.get(13)?),
        sealed: row.get(14)?,
        codecs: row.get(15)?,
        width: as_u16(row.get(16)?),
        height: as_u16(row.get(17)?),
        sample_entry_hash: as_u32(row.get(18)?),
    }))
}

/// Builds an [`EventRow`]; `None` when the stored camera id is malformed.
fn event_row(row: &Row<'_>) -> rusqlite::Result<Option<EventRow>> {
    let Ok(camera) = CameraId::try_new(&row.get::<_, String>(1)?) else {
        return Ok(None);
    };
    let source = source_from_name(&row.get::<_, String>(2)?);
    let kind = kind_from_name(&row.get::<_, String>(3)?);
    let state = state_from_name(&row.get::<_, String>(4)?);
    Ok(Some(EventRow {
        id: EventId(row.get(0)?),
        event: Event {
            camera,
            source,
            kind,
            state,
            nvr_time: WallUs::new(row.get(5)?),
            camera_time: row.get::<_, Option<i64>>(6)?.map(WallUs::new),
            data: row.get(7)?,
        },
        retain_until: row.get::<_, Option<i64>>(8)?.map(WallUs::new),
    }))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn as_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn as_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn as_u32(v: i64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn as_u16(v: i64) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

/// The serde name of a source (`dahua_cgi`, …).
fn source_name(source: EventSource) -> String {
    serde_name(&source)
}

fn source_from_name(name: &str) -> EventSource {
    serde_from_name(name).unwrap_or(EventSource::Manual)
}

fn state_name(state: EventState) -> String {
    serde_name(&state)
}

fn state_from_name(name: &str) -> EventState {
    serde_from_name(name).unwrap_or(EventState::Pulse)
}

/// The serde name of a kind; `Other(code)` is stored as the bare code.
fn kind_name(kind: &EventKind) -> String {
    match kind {
        EventKind::Other(code) => code.clone(),
        other => serde_name(other),
    }
}

fn kind_from_name(name: &str) -> EventKind {
    serde_from_name(name).unwrap_or_else(|| EventKind::Other(name.to_owned()))
}

fn serde_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => "unknown".to_owned(),
    }
}

fn serde_from_name<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version-1 database gains the retention column on open.
    #[test]
    fn a_version_one_database_migrates_to_two() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])
                .unwrap();
        }
        let mut db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let columns: Vec<String> = db
            .conn
            .prepare("PRAGMA table_info(event)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            columns.iter().any(|c| c == "retain_until_us"),
            "{columns:?}"
        );
        assert_eq!(db.retained_events(WallUs::new(0)).unwrap().len(), 0);
    }
}
