//! The SQLite connection and every statement run against it. Used by the
//! catalog thread, and by the synchronous root registry before that thread
//! starts.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;
use std::fs::{DirBuilder, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, named_params, params,
};
use tracing::warn;

use crate::types::{
    CameraRow, CatalogError, CatalogStats, Change, DeleteMarkResult, EventCursor, EventId,
    EventRow, NewSegment, RetentionPage, SampleEntryId, SegmentCursor, SegmentId, SegmentRow,
    StartupRow, TimeRange,
};
use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::{CameraId, RootIdentity, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;

const SCHEMA_VERSION: i64 = 10;

/// Largest number of event rows one listing materializes.
const MAX_EVENT_READ_ROWS: usize = 10_000;
/// Largest aggregate of owned event strings one listing materializes.
const MAX_EVENT_READ_BYTES: usize = 8 * 1024 * 1024;
/// Event rows retained in the state database before oldest inactive rows are pruned.
const MAX_EVENT_ROWS: u64 = 2_000_000;
/// Logical bytes retained across event strings before oldest inactive rows are pruned.
const MAX_EVENT_BYTES: u64 = 512 * 1024 * 1024;
/// Active retention marks allowed at once.
const MAX_RETAINED_EVENT_ROWS: u64 = 100_000;
/// Logical bytes represented by active retention marks.
const MAX_RETAINED_EVENT_BYTES: u64 = 128 * 1024 * 1024;
/// Event metadata age ceiling when recording history does not end first.
const MAX_EVENT_AGE_US: i64 = 90 * 86_400_000_000;
/// Event footage protection/recording-horizon margin.
const EVENT_RECORDING_MARGIN_US: i64 = 30 * 1_000_000;
/// Rows pruned or expired marks cleared in one maintenance transaction.
const EVENT_PRUNE_BATCH: usize = 1_000;
/// Maximum segment rows held by one startup-reconciliation page.
const RECONCILE_PAGE_ROWS: usize = 512;

/// The default of how long after its last durable progress an unsealed segment
/// still extends toward the present in range listings
/// ([`crate::CatalogHandle::segments_limited`]).
///
/// The writer reports progress after each sync (every five seconds by default)
/// and a fragment spans at most two and a half seconds by default, so thirty
/// seconds covers two sync intervals, one fragment, and catalog batching with
/// margin. A deployment with longer intervals opens the catalog with
/// [`crate::CatalogHandle::open_with_grace`] and a grace derived from them. An
/// open row whose progress stops for longer (a camera outage, a file the
/// writer abandoned, or a row startup recovery could not reach) ends this long
/// after its last progress, so it drops out of later windows instead of
/// claiming to cover them. Catalog overload that starves progress for longer
/// than the grace also hides a growing row until its next progress.
pub const OPEN_SEGMENT_GRACE: Duration = Duration::from_secs(30);

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

/// Version 3: retention records durable deletion intent before unlinking a
/// media/index pair.
const SCHEMA_V3: &str = "ALTER TABLE segment ADD COLUMN deleting INTEGER NOT NULL DEFAULT 0;";

/// Version 4: known event kinds and vendor-specific `Other` values occupy
/// separate columns so their persistence is injective.
const SCHEMA_V4: &str = "
ALTER TABLE event ADD COLUMN kind_other TEXT;
UPDATE event SET kind_other = kind, kind = 'other'
WHERE kind NOT IN ('motion', 'human', 'vehicle', 'line_cross', 'intrusion', 'tamper',
                   'audio', 'face', 'video_loss', 'time_change', 'disconnect');
";

/// Version 5: indexed event ordering plus root-aware and global retention
/// candidate ordering. Version 8 rebuilds these indexes with explicit tie keys.
const SCHEMA_V5: &str = "
ALTER TABLE segment ADD COLUMN root TEXT;
CREATE INDEX event_time ON event(nvr_time_us);
CREATE INDEX event_retained_time ON event(nvr_time_us)
    WHERE retain_until_us IS NOT NULL;
CREATE INDEX segment_retention_time ON segment(wall_start_us)
    WHERE sealed = 1 AND deleting = 0;
CREATE INDEX segment_retention_root_time ON segment(root, wall_start_us)
    WHERE sealed = 1 AND deleting = 0;
";

/// Version 6: transactional event capacity accounting and an inactive-event
/// ordering index used by bounded lifecycle maintenance.
const SCHEMA_V6: &str = "
ALTER TABLE event ADD COLUMN storage_bytes INTEGER NOT NULL DEFAULT 0
    CHECK(storage_bytes >= 0);
UPDATE event SET storage_bytes =
    length(CAST(camera_id AS BLOB)) + length(CAST(source AS BLOB)) +
    length(CAST(kind AS BLOB)) + length(CAST(COALESCE(kind_other, '') AS BLOB)) +
    length(CAST(state AS BLOB)) + length(CAST(COALESCE(data, '') AS BLOB));
CREATE TABLE catalog_state (
    singleton     INTEGER PRIMARY KEY CHECK(singleton = 1),
    event_rows   INTEGER NOT NULL CHECK(event_rows >= 0),
    event_bytes  INTEGER NOT NULL CHECK(event_bytes >= 0),
    pruned_events INTEGER NOT NULL CHECK(pruned_events >= 0)
);
INSERT INTO catalog_state(singleton, event_rows, event_bytes, pruned_events)
SELECT 1, COUNT(*), COALESCE(SUM(storage_bytes), 0), 0 FROM event;
CREATE TRIGGER event_account_insert AFTER INSERT ON event BEGIN
    UPDATE catalog_state SET event_rows = event_rows + 1,
        event_bytes = event_bytes + NEW.storage_bytes WHERE singleton = 1;
END;
CREATE TRIGGER event_account_delete AFTER DELETE ON event BEGIN
    UPDATE catalog_state SET event_rows = event_rows - 1,
        event_bytes = event_bytes - OLD.storage_bytes,
        pruned_events = pruned_events + 1 WHERE singleton = 1;
END;
CREATE INDEX event_inactive_time ON event(nvr_time_us)
    WHERE retain_until_us IS NULL;
CREATE INDEX event_retain_until ON event(retain_until_us)
    WHERE retain_until_us IS NOT NULL;
";

/// Version 7: camera-local recording horizons use a fixed-work partial-index seek.
const SCHEMA_V7: &str = "
CREATE INDEX segment_camera_horizon ON segment(camera_id, wall_start_us)
    WHERE deleting = 0;
";

/// Version 8: cursor indexes carry explicit tie keys, retention age pages
/// have their own partial ordering index, and protection checks seek past
/// inactive and expired history before testing event time overlap.
const SCHEMA_V8: &str = "
DROP INDEX segment_stream_time;
CREATE INDEX segment_stream_time ON segment(camera_id, kind, wall_start_us, id);
DROP INDEX segment_retention_time;
CREATE INDEX segment_retention_time ON segment(wall_start_us, id)
    WHERE sealed = 1 AND deleting = 0;
DROP INDEX segment_retention_root_time;
CREATE INDEX segment_retention_root_time ON segment(root, wall_start_us, id)
    WHERE sealed = 1 AND deleting = 0;
CREATE INDEX segment_retention_stream_time ON segment(camera_id, kind, wall_start_us, id)
    WHERE sealed = 1 AND deleting = 0;
DROP INDEX event_camera_time;
CREATE INDEX event_camera_time ON event(camera_id, nvr_time_us, id);
DROP INDEX event_time;
CREATE INDEX event_time ON event(nvr_time_us, id);
DROP INDEX event_retained_time;
CREATE INDEX event_retained_time ON event(nvr_time_us, id)
    WHERE retain_until_us IS NOT NULL;
DROP INDEX event_inactive_time;
CREATE INDEX event_inactive_time ON event(nvr_time_us, id)
    WHERE retain_until_us IS NULL;
DROP INDEX event_retain_until;
CREATE INDEX event_retain_until ON event(retain_until_us, id)
    WHERE retain_until_us IS NOT NULL;
CREATE INDEX event_active_camera_deadline_time
    ON event(camera_id, retain_until_us, nvr_time_us, id)
    WHERE retain_until_us IS NOT NULL;
";

/// Version 9: durable identities prevent a missing or replaced mount from
/// being admitted as a previously configured recording root.
const SCHEMA_V9: &str = "
CREATE TABLE recording_root (
    path     TEXT PRIMARY KEY,
    identity TEXT NOT NULL UNIQUE CHECK(length(identity) = 32)
);
CREATE TABLE recording_root_state (
    singleton             INTEGER PRIMARY KEY CHECK(singleton = 1),
    pre_identity_catalog  INTEGER NOT NULL CHECK(pre_identity_catalog IN (0, 1))
);
";

/// Version 10: sample entries are unique by CRC-32C and bytes together, so two
/// entries whose CRCs collide keep their own rows and metadata, and the unknown
/// placeholder (CRC 0, no bytes) never merges with a real entry. The table is
/// rebuilt with its ids, so `segment.sample_entry_id` stays valid; the rebuild
/// drops a referenced table, which needs foreign-key enforcement off, so the
/// migration checks the keys itself before it commits.
const SCHEMA_V10: &str = "
CREATE TABLE sample_entry_v10 (
    id     INTEGER PRIMARY KEY,
    hash   INTEGER NOT NULL,
    codecs TEXT NOT NULL,
    width  INTEGER NOT NULL,
    height INTEGER NOT NULL,
    config BLOB NOT NULL,
    UNIQUE(hash, config)
);
INSERT INTO sample_entry_v10 (id, hash, codecs, width, height, config)
    SELECT id, hash, codecs, width, height, config FROM sample_entry;
DROP TABLE sample_entry;
ALTER TABLE sample_entry_v10 RENAME TO sample_entry;
";

#[derive(Clone, Copy)]
struct EventLimits {
    rows: u64,
    bytes: u64,
    prune_batch: usize,
}

pub(crate) struct ApplyOutcome {
    pub(crate) committed_events: Vec<WallUs>,
    pub(crate) rejected_events: usize,
}

/// Pre-thread catalog knowledge for one configured recording root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogRootExpectation {
    /// Durable identity registered by a previous successful start.
    Known(RootIdentity),
    /// No identity is registered for this root, and the catalog predates root
    /// identities or holds rows beneath the root; an operator marker is
    /// required.
    LegacyOwned,
    /// No catalog state belongs to this root, so it may be initialized.
    Fresh,
}

/// Synchronous catalog preflight used before Landlock and before any thread is
/// spawned. It migrates the schema, reads the recording-root registry, and
/// pages through the segment rows to find the roots that own rows.
pub struct CatalogRootRegistry {
    db: Db,
}

impl fmt::Debug for CatalogRootRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CatalogRootRegistry")
            .finish_non_exhaustive()
    }
}

impl CatalogRootRegistry {
    /// Opens and migrates the catalog without spawning a worker thread.
    ///
    /// # Errors
    ///
    /// The errors of [`crate::CatalogHandle::open`].
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        Ok(Self {
            db: Db::open(path)?,
        })
    }

    /// Resolves catalog expectations in input order using bounded segment
    /// pages. `page_complete` runs after every page so startup can extend its
    /// external deadline even before the async catalog thread exists.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Corrupt`] for a registered identity that is not 32
    /// lowercase hexadecimal characters or a row that fails checked decoding,
    /// [`CatalogError::NonUtf8Path`] for a configured root that is not UTF-8,
    /// and [`CatalogError::Sqlite`] for a statement failure.
    pub fn expectations_with_progress(
        &mut self,
        roots: &[PathBuf],
        page_complete: impl FnMut(),
    ) -> Result<Vec<CatalogRootExpectation>, CatalogError> {
        self.db.root_expectations(roots, page_complete)
    }
}

const EVENT_LIMITS: EventLimits = EventLimits {
    rows: MAX_EVENT_ROWS,
    bytes: MAX_EVENT_BYTES,
    prune_batch: EVENT_PRUNE_BATCH,
};

const SEGMENT_COLUMNS: &str = "s.id, s.camera_id, s.kind, s.path, s.run_seq, s.discontinuity, \
     s.wall_start_us, s.wall_end_us, s.media_start, s.media_end, s.init_len, s.durable_len, \
     s.fragments, s.bytes, s.sealed, s.deleting, e.codecs, e.width, e.height, e.hash, e.id";

/// Index of a column a query appends after [`SEGMENT_COLUMNS`] (the root or
/// the protection result); [`segment_row`] reads the 21 columns before it.
const APPENDED_COLUMN: usize = 21;

/// The tables [`SEGMENT_COLUMNS`] read from.
const SEGMENT_FROM: &str = "FROM segment s JOIN sample_entry e ON e.id = s.sample_entry_id";

/// `NOT EXISTS (...)`: no active retained event of the same camera overlaps
/// the segment aliased `segment`, widened by the `margin` parameter on both
/// sides, where "active" means a deadline after the `now` parameter. Every
/// statement that decides protection uses this one predicate.
fn protection_predicate(segment: &str, now: &str, margin: &str) -> String {
    format!(
        "NOT EXISTS ( \
           SELECT 1 FROM event protected \
           WHERE protected.camera_id = {segment}.camera_id \
             AND protected.retain_until_us > {now} \
             AND protected.nvr_time_us > {segment}.wall_start_us - {margin} \
             AND protected.nvr_time_us < {segment}.wall_end_us + {margin} \
         )"
    )
}

/// The atomic deletion mark: a sealed, ordinary row that the protection
/// predicate admits, optionally only at the persisted `:root`.
fn mark_sql(root_bound: bool) -> String {
    let root_clause = if root_bound {
        "AND candidate.root = :root "
    } else {
        ""
    };
    format!(
        "UPDATE segment AS candidate SET deleting = 1 \
         WHERE candidate.id = :id AND candidate.sealed = 1 \
           AND candidate.deleting = 0 {root_clause}AND {}",
        protection_predicate("candidate", ":now", ":margin")
    )
}

const EVENT_COLUMNS: &str = "id, camera_id, source, kind, kind_other, state, nvr_time_us, \
     camera_time_us, data, retain_until_us";

fn event_query_sql(camera: bool, seek_after: bool) -> String {
    match (camera, seek_after) {
        (true, false) => format!(
            "SELECT {EVENT_COLUMNS} FROM event \
             WHERE camera_id = ?1 AND nvr_time_us >= ?2 AND nvr_time_us < ?3 \
             ORDER BY nvr_time_us, id LIMIT ?6"
        ),
        (true, true) => format!(
            "SELECT {EVENT_COLUMNS} FROM event \
             WHERE camera_id = ?1 AND nvr_time_us = ?4 AND id > ?5 AND nvr_time_us < ?3 \
             UNION ALL \
             SELECT {EVENT_COLUMNS} FROM event \
             WHERE camera_id = ?1 AND nvr_time_us > ?4 AND nvr_time_us < ?3 \
             ORDER BY nvr_time_us, id LIMIT ?6"
        ),
        (false, false) => format!(
            "SELECT {EVENT_COLUMNS} FROM event \
             WHERE nvr_time_us >= ?2 AND nvr_time_us < ?3 \
             ORDER BY nvr_time_us, id LIMIT ?6"
        ),
        (false, true) => format!(
            "SELECT {EVENT_COLUMNS} FROM event \
             WHERE nvr_time_us = ?4 AND id > ?5 AND nvr_time_us < ?3 \
             UNION ALL \
             SELECT {EVENT_COLUMNS} FROM event \
             WHERE nvr_time_us > ?4 AND nvr_time_us < ?3 \
             ORDER BY nvr_time_us, id LIMIT ?6"
        ),
    }
}

fn event_query_lower_bound(from: WallUs, after: Option<EventCursor>) -> i64 {
    after.map_or(from.get(), |cursor| from.get().max(cursor.time.get()))
}

fn retention_candidate_query_sql(root: bool, after: bool) -> String {
    let root_clause = if root { "AND s.root = ?1" } else { "" };
    let candidates = if after {
        format!(
            "SELECT s.id, s.wall_start_us FROM segment s \
             WHERE s.sealed = 1 AND s.deleting = 0 {root_clause} \
               AND s.wall_start_us = ?2 AND s.id > ?3 \
             UNION ALL \
             SELECT s.id, s.wall_start_us FROM segment s \
             WHERE s.sealed = 1 AND s.deleting = 0 {root_clause} \
               AND s.wall_start_us > ?2 \
             ORDER BY wall_start_us, id LIMIT ?6"
        )
    } else {
        format!(
            "SELECT s.id, s.wall_start_us FROM segment s \
             WHERE s.sealed = 1 AND s.deleting = 0 {root_clause} \
             ORDER BY s.wall_start_us, s.id LIMIT ?6"
        )
    };
    let unprotected = protection_predicate("s", "?4", "?5");
    format!(
        "WITH candidates(id, wall_start_us) AS MATERIALIZED ( \
           {candidates} \
         ) \
         SELECT {SEGMENT_COLUMNS}, {unprotected} \
         FROM candidates c \
         JOIN segment s ON s.id = c.id \
         JOIN sample_entry e ON e.id = s.sample_entry_id \
         ORDER BY s.wall_start_us, s.id"
    )
}

fn expired_candidate_query_sql(after: bool) -> String {
    let candidates = if after {
        "SELECT s.id, s.wall_start_us FROM segment s \
         WHERE s.camera_id = ?1 AND s.kind = ?2 \
           AND s.sealed = 1 AND s.deleting = 0 \
           AND s.wall_start_us = ?4 AND s.id > ?5 AND s.wall_start_us < ?3 \
         UNION ALL \
         SELECT s.id, s.wall_start_us FROM segment s \
         WHERE s.camera_id = ?1 AND s.kind = ?2 \
           AND s.sealed = 1 AND s.deleting = 0 \
           AND s.wall_start_us > ?4 AND s.wall_start_us < ?3 \
         ORDER BY wall_start_us, id LIMIT ?8"
    } else {
        "SELECT s.id, s.wall_start_us FROM segment s \
         WHERE s.camera_id = ?1 AND s.kind = ?2 \
           AND s.sealed = 1 AND s.deleting = 0 AND s.wall_start_us < ?3 \
         ORDER BY s.wall_start_us, s.id LIMIT ?8"
    };
    let unprotected = protection_predicate("s", "?6", "?7");
    format!(
        "WITH candidates(id, wall_start_us) AS MATERIALIZED ( \
           {candidates} \
         ) \
         SELECT {SEGMENT_COLUMNS}, (s.wall_end_us < ?3 AND {unprotected}) \
         FROM candidates c \
         JOIN segment s ON s.id = c.id \
         JOIN sample_entry e ON e.id = s.sample_entry_id \
         ORDER BY s.wall_start_us, s.id"
    )
}

const CLEAR_EXPIRED_MARKS_SQL: &str = "UPDATE event SET retain_until_us = NULL WHERE id IN ( \
     SELECT id FROM event WHERE retain_until_us IS NOT NULL AND retain_until_us <= ?1 \
     ORDER BY retain_until_us, id LIMIT ?2 \
   )";

const EVENT_STATE_SQL: &str =
    "SELECT event_rows, event_bytes FROM catalog_state WHERE singleton = 1";

const EVENT_HORIZON_SQL: &str = "SELECT MIN(wall_start_us) FROM segment \
     WHERE camera_id = ?1 AND deleting = 0";

fn inactive_event_query_sql(after: bool) -> &'static str {
    if after {
        "SELECT id, camera_id, nvr_time_us FROM event \
         WHERE retain_until_us IS NULL AND nvr_time_us = ?1 AND id > ?2 \
         UNION ALL \
         SELECT id, camera_id, nvr_time_us FROM event \
         WHERE retain_until_us IS NULL AND nvr_time_us > ?1 \
         ORDER BY nvr_time_us, id LIMIT ?3"
    } else {
        "SELECT id, camera_id, nvr_time_us FROM event \
         WHERE retain_until_us IS NULL ORDER BY nvr_time_us, id LIMIT ?3"
    }
}

/// The open database.
pub struct Db {
    conn: Connection,
    path: PathBuf,
    roots: Vec<PathBuf>,
    open_segment_grace: Duration,
    event_maintenance_cursor: Option<EventCursor>,
}

impl Db {
    /// Opens (creating if needed) and migrates the database at `path` with the
    /// default [`OPEN_SEGMENT_GRACE`].
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        Self::open_with_grace(path, OPEN_SEGMENT_GRACE)
    }

    /// Opens (creating if needed) and migrates the database at `path`.
    ///
    /// Missing parent directories are created with mode 0700 and a missing
    /// database file with mode 0600, whatever the process umask. SQLite gives
    /// the write-ahead log and shared-memory files the database file's mode, so
    /// a fresh catalog stays readable by its owner only. `open_segment_grace`
    /// is how far an unsealed row extends past its last progress in range
    /// listings.
    pub fn open_with_grace(
        path: &Path,
        open_segment_grace: Duration,
    ) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        create_private_database(path)?;
        let conn = Connection::open(path)?;
        // A connection setting, not a persistent pragma: set before the first
        // read so it covers an exclusive lock held by another connection.
        conn.busy_timeout(Duration::from_secs(5))?;
        // The daemon's filesystem sandbox intentionally admits only configured
        // state and recording roots. Keep SQLite's transient sorter and
        // materialized-query state inside the process instead of relying on an
        // ambient /tmp or /var/tmp path outside that authority.
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        let version = stored_schema_version(&conn)?;
        if version > SCHEMA_VERSION {
            return Err(CatalogError::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version >= 1 {
            // `deleting` exists from version 3 on and arrives with a default
            // of zero, so a migrated column never needs a second check.
            validate_segment_booleans(&conn, /* validate_deleting */ version >= 3)?;
        }
        // `PRAGMA journal_mode` keeps the previous mode when WAL cannot be
        // enabled and reports the result; the durability statements of this
        // crate hold only in WAL mode.
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(CatalogError::JournalMode(mode));
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mut db = Self {
            conn,
            path: path.to_path_buf(),
            roots: Vec::new(),
            open_segment_grace,
            event_maintenance_cursor: None,
        };
        // The bundled SQLite enforces foreign keys by default. A migration that
        // rebuilds a referenced table needs them off (the rebuild drops the old
        // table) and checks the keys itself before it commits; enforcement is
        // back on for everything after.
        db.conn.pragma_update(None, "foreign_keys", "OFF")?;
        db.migrate(version)?;
        db.conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(db)
    }

    /// Brings the schema to [`SCHEMA_VERSION`]. `observed` is the version read
    /// before persistent pragmas; the authoritative version is read again under
    /// the write lock, so a concurrent opener that migrated first is not
    /// migrated twice.
    fn migrate(&mut self, observed: i64) -> Result<(), CatalogError> {
        if observed >= SCHEMA_VERSION {
            return Ok(());
        }
        let tx = write_transaction(&mut self.conn)?;
        let version = stored_schema_version(&tx)?;
        if version > SCHEMA_VERSION {
            return Err(CatalogError::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version < SCHEMA_VERSION {
            if version < 1 {
                tx.execute_batch(SCHEMA_V1)?;
            }
            if version < 2 {
                tx.execute_batch(SCHEMA_V2)?;
            }
            if version < 3 {
                tx.execute_batch(SCHEMA_V3)?;
            }
            if version < 4 {
                tx.execute_batch(SCHEMA_V4)?;
            }
            if version < 5 {
                tx.execute_batch(SCHEMA_V5)?;
            }
            if version < 6 {
                tx.execute_batch(SCHEMA_V6)?;
            }
            if version < 7 {
                tx.execute_batch(SCHEMA_V7)?;
            }
            if version < 8 {
                tx.execute_batch(SCHEMA_V8)?;
            }
            if version < 9 {
                tx.execute_batch(SCHEMA_V9)?;
                tx.execute(
                    "INSERT INTO recording_root_state(singleton, pre_identity_catalog) \
                     VALUES (1, ?1)",
                    params![i64::from(version != 0)],
                )?;
            }
            if version < 10 {
                tx.execute_batch(SCHEMA_V10)?;
                check_foreign_keys(&tx)?;
            }
            tx.execute("DELETE FROM schema_version", [])?;
            tx.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Durably binds one configured root `path` to its `identity`.
    /// Registering the same pair again changes nothing.
    pub fn register_recording_root(
        &mut self,
        path: &Path,
        identity: RootIdentity,
    ) -> Result<(), CatalogError> {
        let path = path_text(path)?;
        let identity = identity.to_string();
        self.with_full_synchronous(|connection| {
            let transaction = write_transaction(connection)?;
            let existing = transaction
                .query_row(
                    "SELECT identity FROM recording_root WHERE path = ?1",
                    params![path],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                if existing != identity {
                    return Err(CatalogError::RootIdentityMismatch(PathBuf::from(path)));
                }
                transaction.commit()?;
                return Ok(());
            }
            let existing_path = transaction
                .query_row(
                    "SELECT path FROM recording_root WHERE identity = ?1",
                    params![identity],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(existing) = existing_path.filter(|existing| existing != path) {
                return Err(CatalogError::RootIdentityInUse(PathBuf::from(existing)));
            }
            transaction.execute(
                "INSERT INTO recording_root(path, identity) VALUES (?1, ?2)",
                params![path, identity],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    /// Assigns persisted rows to configured roots with component-aware path
    /// matching. The exact root text is indexed; retention never relies on a
    /// SQL string prefix.
    pub fn configure_video_roots(&mut self, roots: &[PathBuf]) -> Result<(), CatalogError> {
        let mut roots = roots.to_vec();
        roots.sort_by_key(|root| (Reverse(root.components().count()), root.clone()));
        roots.dedup();
        let mut cursor: Option<i64> = None;
        loop {
            let assignments = {
                // Two statements rather than `?1 IS NULL OR id > ?1`: the OR
                // form cannot use the rowid index, so every page would scan
                // from the first row.
                let sql = if cursor.is_some() {
                    "SELECT id, path FROM segment WHERE root IS NULL AND id > ?1 \
                     ORDER BY id LIMIT ?2"
                } else {
                    "SELECT id, path FROM segment WHERE root IS NULL ORDER BY id LIMIT ?2"
                };
                let mut statement = self.conn.prepare_cached(sql)?;
                let rows = statement
                    .query_map(
                        params![
                            cursor,
                            i64::try_from(RECONCILE_PAGE_ROWS).unwrap_or(i64::MAX)
                        ],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                    )?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(row_error)?;
                rows.into_iter()
                    .map(|(id, path)| {
                        let root = root_for_path(&roots, Path::new(&path))
                            .map(path_text)
                            .transpose()?
                            .map(str::to_owned);
                        Ok((id, root))
                    })
                    .collect::<Result<Vec<_>, CatalogError>>()?
            };
            if assignments.is_empty() {
                break;
            }
            cursor = assignments.last().map(|(id, _)| *id);
            let count = assignments.len();
            let tx = write_transaction(&mut self.conn)?;
            for (id, root) in assignments {
                tx.execute(
                    "UPDATE segment SET root = ?1 WHERE id = ?2 AND root IS NULL",
                    params![root, id],
                )?;
            }
            tx.commit()?;
            if count < RECONCILE_PAGE_ROWS {
                break;
            }
        }
        self.roots = roots;
        Ok(())
    }

    fn root_expectations(
        &mut self,
        roots: &[PathBuf],
        mut page_complete: impl FnMut(),
    ) -> Result<Vec<CatalogRootExpectation>, CatalogError> {
        let pre_identity_catalog = self
            .conn
            .query_row(
                "SELECT pre_identity_catalog FROM recording_root_state WHERE singleton = 1",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(row_error)?;
        let root_texts = roots
            .iter()
            .map(|root| path_text(root))
            .collect::<Result<Vec<_>, _>>()?;
        let mut expectations = Vec::with_capacity(roots.len());
        for root in &root_texts {
            let identity = self
                .conn
                .query_row(
                    "SELECT identity FROM recording_root WHERE path = ?1",
                    params![root],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(row_error)?;
            let expectation = match identity {
                Some(identity) => {
                    CatalogRootExpectation::Known(identity.parse::<RootIdentity>().map_err(
                        |_| corrupt("recording_root.identity is not a recording-root identity"),
                    )?)
                }
                None if pre_identity_catalog => CatalogRootExpectation::LegacyOwned,
                None => CatalogRootExpectation::Fresh,
            };
            expectations.push(expectation);
        }

        let mut owned = vec![false; roots.len()];
        let mut cursor: Option<i64> = None;
        loop {
            let rows = {
                // Two statements rather than `?1 IS NULL OR id > ?1`, so each
                // page is a rowid seek instead of a scan from the first row.
                let sql = if cursor.is_some() {
                    "SELECT id, path, root FROM segment WHERE id > ?1 ORDER BY id LIMIT ?2"
                } else {
                    "SELECT id, path, root FROM segment ORDER BY id LIMIT ?2"
                };
                let mut statement = self.conn.prepare_cached(sql)?;
                statement
                    .query_map(
                        params![
                            cursor,
                            i64::try_from(RECONCILE_PAGE_ROWS).unwrap_or(i64::MAX)
                        ],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(row_error)?
            };
            if rows.is_empty() {
                break;
            }
            for (id, path, assigned_root) in &rows {
                cursor = Some(*id);
                if let Some(index) = assigned_root
                    .as_deref()
                    .and_then(|assigned| root_texts.iter().position(|root| *root == assigned))
                {
                    owned[index] = true;
                }
                if let Some(index) = root_index_for_path(roots, Path::new(path)) {
                    owned[index] = true;
                }
            }
            page_complete();
            if rows.len() < RECONCILE_PAGE_ROWS {
                break;
            }
        }
        for (expectation, owned) in expectations.iter_mut().zip(owned) {
            if owned && matches!(expectation, CatalogRootExpectation::Fresh) {
                *expectation = CatalogRootExpectation::LegacyOwned;
            }
        }
        Ok(expectations)
    }

    /// Applies a batch of changes in one transaction.
    pub fn apply(&mut self, changes: &[Change]) -> Result<ApplyOutcome, CatalogError> {
        let roots = &self.roots;
        let tx = write_transaction(&mut self.conn)?;
        let now = WallUs::now();
        let mut event_work = EVENT_PRUNE_BATCH;
        let mut rejected_events = 0usize;
        let mut committed_events = Vec::new();
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
                } => apply_segment_opened(
                    &tx,
                    roots,
                    OpenedSegment {
                        key,
                        path,
                        run_seq: *run_seq,
                        discontinuity: *discontinuity,
                        wall_start: *wall_start,
                        media_start: *media_start,
                        init_len: *init_len,
                        sample_entry,
                    },
                )?,
                Change::SegmentProgress {
                    path,
                    durable_len,
                    fragments,
                    wall_end,
                    media_end,
                } => apply_segment_progress(
                    &tx,
                    path,
                    *durable_len,
                    *fragments,
                    *wall_end,
                    *media_end,
                )?,
                Change::SegmentSealed {
                    path,
                    len,
                    fragments,
                    wall_end,
                    media_end,
                } => apply_segment_sealed(&tx, path, *len, *fragments, *wall_end, *media_end)?,
                Change::Event(event) => {
                    if try_insert_event(&tx, event, now, EVENT_LIMITS, &mut event_work)?.is_none() {
                        rejected_events += 1;
                    } else {
                        committed_events.push(event.nvr_time);
                    }
                }
            }
        }
        tx.commit()?;
        if rejected_events > 0 {
            warn!(
                rejected_events,
                "events omitted at the state-database capacity"
            );
        }
        Ok(ApplyOutcome {
            committed_events,
            rejected_events,
        })
    }

    /// Inserts or updates a camera.
    pub fn ensure_camera(
        &mut self,
        id: &CameraId,
        name: &str,
        vendor: Option<&str>,
    ) -> Result<(), CatalogError> {
        let tx = write_transaction(&mut self.conn)?;
        tx.execute(
            "INSERT INTO camera (id, name, vendor) VALUES (?1, ?2, ?3) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, vendor = excluded.vendor",
            params![id.as_str(), name, vendor],
        )?;
        tx.commit()?;
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
            .collect::<Result<Vec<_>, _>>()
            .map_err(row_error)?;
        rows.into_iter()
            .map(|(id, name, vendor)| {
                let id = CameraId::try_new(&id)
                    .map_err(|_| corrupt("camera.id is not a valid camera identifier"))?;
                Ok(CameraRow { id, name, vendor })
            })
            .collect()
    }

    /// Every segment of a stream overlapping `range`, by start time, for the
    /// tests of this module; serving callers use [`Db::segments_limited`].
    #[cfg(test)]
    pub fn segments(
        &mut self,
        key: &StreamKey,
        range: TimeRange,
    ) -> Result<Vec<SegmentRow>, CatalogError> {
        self.segments_limited(key, range, usize::MAX)
    }

    /// At most `limit` segments of a stream overlapping `range`, by start time
    /// and id, capped before rows are materialized. An unsealed segment is
    /// listed only once it has a durable fragment, and it extends to the
    /// earlier of now and the open-segment grace past its last progress.
    pub fn segments_limited(
        &mut self,
        key: &StreamKey,
        range: TimeRange,
        limit: usize,
    ) -> Result<Vec<SegmentRow>, CatalogError> {
        let now = WallUs::now();
        // An open row without a durable fragment has nothing to serve, and one
        // whose progress stopped must not claim later windows.
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} {SEGMENT_FROM} \
             WHERE s.deleting = 0 AND s.camera_id = :camera AND s.kind = :kind \
             AND s.wall_start_us < :to \
             AND (s.sealed = 1 OR s.fragments > 0) \
             AND (CASE WHEN s.sealed = 0 THEN MIN(:now, s.wall_end_us + :grace) \
                  ELSE s.wall_end_us END) > :from \
             ORDER BY s.wall_start_us, s.id LIMIT :limit"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let grace = i64::try_from(self.open_segment_grace.as_micros()).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(
                named_params! {
                    ":camera": key.camera.as_str(),
                    ":kind": key.kind.as_str(),
                    ":to": range.to.get(),
                    ":from": range.from.get(),
                    ":now": now.get(),
                    ":grace": grace,
                    ":limit": limit,
                },
                segment_row,
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(row_error)?;
        rows.into_iter().map(decode_segment).collect()
    }

    /// One segment by id; `None` for an unknown id and for a row in deletion
    /// intent.
    pub fn segment(&mut self, id: SegmentId) -> Result<Option<SegmentRow>, CatalogError> {
        let sql = format!("SELECT {SEGMENT_COLUMNS} {SEGMENT_FROM} WHERE s.id = ?1");
        let row = self
            .conn
            .query_row(&sql, params![id.0], segment_row)
            .optional()
            .map_err(row_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if segment_deleting(&row)? {
            return Ok(None);
        }
        decode_segment(row).map(Some)
    }

    /// One segment by path; `None` for an unknown path and for a row in
    /// deletion intent.
    pub fn segment_by_path(&mut self, path: &Path) -> Result<Option<SegmentRow>, CatalogError> {
        let sql = format!("SELECT {SEGMENT_COLUMNS} {SEGMENT_FROM} WHERE s.path = ?1");
        let row = self
            .conn
            .query_row(&sql, params![path_text(path)?], segment_row)
            .optional()
            .map_err(row_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if segment_deleting(&row)? {
            return Ok(None);
        }
        decode_segment(row).map(Some)
    }

    /// Whether any segment row, including deletion intent, owns `path`.
    pub fn contains_segment_path(&mut self, path: &Path) -> Result<bool, CatalogError> {
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM segment WHERE path = ?1)",
                params![path_text(path)?],
                |row| row.get(0),
            )
            .map_err(row_error)
    }

    /// Persisted recording-root spelling for one segment.
    pub fn segment_root(&mut self, id: SegmentId) -> Result<Option<PathBuf>, CatalogError> {
        self.conn
            .query_row(
                "SELECT root FROM segment WHERE id = ?1",
                params![id.0],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|root| root.flatten().map(PathBuf::from))
            .map_err(row_error)
    }

    /// Every unsealed segment, by start time; test support, the daemon pages
    /// with [`Db::writing_segments_page`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn writing_segments(&mut self) -> Result<Vec<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} {SEGMENT_FROM} \
             WHERE s.sealed = 0 AND s.deleting = 0 ORDER BY s.wall_start_us, s.id"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map([], segment_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(row_error)?;
        rows.into_iter().map(decode_segment).collect()
    }

    /// One stable bounded page of open segments after the exclusive cursor,
    /// ordered by `(wall_start, id)`; `limit` is clamped to 1 to 512. Open
    /// rows are few (one per recording stream), so the page scans the table
    /// rather than carrying an index for them.
    pub fn writing_segments_page(
        &mut self,
        after: Option<SegmentCursor>,
        limit: usize,
    ) -> Result<Vec<StartupRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS}, s.root {SEGMENT_FROM} \
             WHERE s.sealed = 0 AND s.deleting = 0 AND \
               (?1 IS NULL OR s.wall_start_us > ?1 OR (s.wall_start_us = ?1 AND s.id > ?2)) \
             ORDER BY s.wall_start_us, s.id LIMIT ?3"
        );
        self.startup_page(&sql, after, limit)
    }

    /// Runs one startup page statement with the cursor and limit parameters.
    fn startup_page(
        &mut self,
        sql: &str,
        after: Option<SegmentCursor>,
        limit: usize,
    ) -> Result<Vec<StartupRow>, CatalogError> {
        let (time, id) = after.map_or((None, None), |cursor| {
            (Some(cursor.wall_start.get()), Some(cursor.id.0))
        });
        let limit = limit.clamp(1, RECONCILE_PAGE_ROWS);
        let mut statement = self.conn.prepare_cached(sql)?;
        let rows = statement
            .query_map(
                params![time, id, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| {
                    Ok((
                        segment_row(row)?,
                        row.get::<_, Option<String>>(APPENDED_COLUMN)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(row_error)?;
        rows.into_iter()
            .map(|(row, root)| {
                Ok(StartupRow {
                    row: decode_segment(row)?,
                    root: root.map(PathBuf::from),
                })
            })
            .collect()
    }

    /// The oldest sealed segments by start time, ignoring protection and
    /// roots; test support, retention uses [`Db::retention_candidates`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn oldest_sealed(&mut self, limit: usize) -> Result<Vec<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} {SEGMENT_FROM} \
             WHERE s.sealed = 1 AND s.deleting = 0 \
             ORDER BY s.wall_start_us, s.id LIMIT ?1"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![limit], segment_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(row_error)?;
        rows.into_iter().map(decode_segment).collect()
    }

    /// The sealed segment of `key` that ends last.
    pub fn newest_sealed(&mut self, key: &StreamKey) -> Result<Option<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} {SEGMENT_FROM} \
             WHERE s.camera_id = ?1 AND s.kind = ?2 AND s.sealed = 1 AND s.deleting = 0 \
             ORDER BY s.wall_end_us DESC, s.id DESC LIMIT 1"
        );
        let row = self
            .conn
            .query_row(
                &sql,
                params![key.camera.as_str(), key.kind.as_str()],
                segment_row,
            )
            .optional()
            .map_err(row_error)?;
        row.map(decode_segment).transpose()
    }

    /// Bytes of every segment in the catalog, open files at their written length.
    pub fn total_bytes(&mut self) -> Result<u64, CatalogError> {
        let (total, invalid): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0), \
                 COALESCE(SUM(CASE WHEN typeof(bytes) = 'integer' AND bytes >= 0 \
                                   THEN 0 ELSE 1 END), 0) \
                 FROM segment",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(row_error)?;
        if invalid != 0 {
            return Err(corrupt("segment.bytes is not a nonnegative integer"));
        }
        checked_u64(total, "segment byte total")
    }

    /// Deletes a row in deletion intent once its files are gone. A row that
    /// left deletion intent in the meantime (a writer reopened the path and
    /// `SegmentOpened` reset it) is kept.
    pub fn delete_segment(&mut self, id: SegmentId) -> Result<(), CatalogError> {
        let tx = write_transaction(&mut self.conn)?;
        tx.execute(
            "DELETE FROM segment WHERE id = ?1 AND deleting = 1",
            params![id.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Durably marks a sealed segment for deletion without the protection
    /// check; test support for exercising deletion intents.
    #[cfg(any(test, feature = "test-support"))]
    pub fn mark_deleting(&mut self, id: SegmentId) -> Result<DeleteMarkResult, CatalogError> {
        self.with_full_synchronous(|conn| {
            let tx = write_transaction(conn)?;
            let changed = tx.execute(
                "UPDATE segment SET deleting = 1 WHERE id = ?1 AND sealed = 1 AND deleting = 0",
                params![id.0],
            )?;
            tx.commit()?;
            Ok(if changed == 1 {
                DeleteMarkResult::Marked
            } else {
                DeleteMarkResult::NotFound
            })
        })
    }

    /// Durably transitions an open segment into startup deletion intent,
    /// whatever its fragment count. Startup calls it for an open row whose
    /// recording pair is empty or confirmed absent beneath an approved root.
    pub fn mark_empty_writing_deleting(
        &mut self,
        id: SegmentId,
    ) -> Result<DeleteMarkResult, CatalogError> {
        self.with_full_synchronous(|connection| {
            let transaction = write_transaction(connection)?;
            let changed = transaction.execute(
                "UPDATE segment SET deleting = 1 \
                 WHERE id = ?1 AND sealed = 0 AND deleting = 0",
                params![id.0],
            )?;
            let result = if changed == 1 {
                DeleteMarkResult::Marked
            } else {
                DeleteMarkResult::NotFound
            };
            transaction.commit()?;
            Ok(result)
        })
    }

    /// Durably marks a sealed segment only when no active retained event for
    /// the same camera overlaps its interval widened by `margin_us`.
    pub fn mark_deleting_if_unprotected(
        &mut self,
        id: SegmentId,
        now: WallUs,
        margin_us: i64,
    ) -> Result<DeleteMarkResult, CatalogError> {
        self.with_full_synchronous(|conn| {
            let tx = write_transaction(conn)?;
            let changed = tx
                .prepare_cached(&mark_sql(false))?
                .execute(named_params! {
                    ":id": id.0,
                    ":now": now.get(),
                    ":margin": margin_us,
                })?;
            let result = if changed == 1 {
                DeleteMarkResult::Marked
            } else {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM segment \
                     WHERE id = ?1 AND sealed = 1 AND deleting = 0)",
                    params![id.0],
                    |row| row.get(0),
                )?;
                if exists {
                    DeleteMarkResult::Protected
                } else {
                    DeleteMarkResult::NotFound
                }
            };
            tx.commit()?;
            Ok(result)
        })
    }

    /// Root-bound variant of [`Db::mark_deleting_if_unprotected`]. The FULL
    /// transaction refuses to mark a row whose persisted root differs.
    pub fn mark_deleting_if_unprotected_at_root(
        &mut self,
        id: SegmentId,
        expected_root: &Path,
        now: WallUs,
        margin_us: i64,
    ) -> Result<DeleteMarkResult, CatalogError> {
        let expected_root = path_text(expected_root)?;
        self.with_full_synchronous(|conn| {
            let tx = write_transaction(conn)?;
            let changed = tx.prepare_cached(&mark_sql(true))?.execute(named_params! {
                ":id": id.0,
                ":root": expected_root,
                ":now": now.get(),
                ":margin": margin_us,
            })?;
            let result = if changed == 1 {
                DeleteMarkResult::Marked
            } else {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM segment \
                     WHERE id = ?1 AND root = ?2 AND sealed = 1 AND deleting = 0)",
                    params![id.0, expected_root],
                    |row| row.get(0),
                )?;
                if exists {
                    DeleteMarkResult::Protected
                } else {
                    DeleteMarkResult::NotFound
                }
            };
            tx.commit()?;
            Ok(result)
        })
    }

    /// Runs one operation with WAL commits synced before returning, restoring
    /// the catalog's normal high-rate setting afterwards.
    fn with_full_synchronous<T>(
        &mut self,
        operation: impl FnOnce(&mut Connection) -> Result<T, CatalogError>,
    ) -> Result<T, CatalogError> {
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let result = operation(&mut self.conn);
        let restore = self.conn.pragma_update(None, "synchronous", "NORMAL");
        match (result, restore) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) => Err(error.into()),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(restore_error)) => {
                warn!(%restore_error, "cannot restore catalog synchronous mode after failed operation");
                Err(error)
            }
        }
    }

    /// Every segment in deletion intent, by start time; test support, the
    /// daemon pages with [`Db::deleting_segments_page`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn deleting_segments(&mut self) -> Result<Vec<SegmentRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS} {SEGMENT_FROM} \
             WHERE s.deleting != 0 ORDER BY s.wall_start_us, s.id"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt
            .query_map([], segment_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(row_error)?;
        rows.into_iter().map(decode_segment).collect()
    }

    /// One stable bounded page of deletion intents after the exclusive cursor,
    /// ordered by `(wall_start, id)`; `limit` is clamped to 1 to 512. Rows in
    /// deletion intent are few, so the page scans the table.
    pub fn deleting_segments_page(
        &mut self,
        after: Option<SegmentCursor>,
        limit: usize,
    ) -> Result<Vec<StartupRow>, CatalogError> {
        let sql = format!(
            "SELECT {SEGMENT_COLUMNS}, s.root {SEGMENT_FROM} \
             WHERE s.deleting != 0 AND \
               (?1 IS NULL OR s.wall_start_us > ?1 OR (s.wall_start_us = ?1 AND s.id > ?2)) \
             ORDER BY s.wall_start_us, s.id LIMIT ?3"
        );
        self.startup_page(&sql, after, limit)
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
        let tx = write_transaction(&mut self.conn)?;
        let updated = tx.execute(
            "UPDATE segment SET sealed = 1, bytes = :len, durable_len = :len, \
             fragments = :fragments, wall_end_us = :wall_end, media_end = :media_end \
             WHERE path = :path",
            named_params! {
                ":len": as_i64(len),
                ":fragments": i64::from(fragments),
                ":wall_end": wall_end.get(),
                ":media_end": as_i64(media_end),
                ":path": path_text(path)?,
            },
        )?;
        if updated == 0 {
            return Err(CatalogError::NotFound(path.to_path_buf()));
        }
        tx.commit()?;
        Ok(())
    }

    /// Inserts a sealed segment found on disk: an upsert by path that replaces
    /// a row the path already has and clears its deletion intent.
    pub fn adopt_segment(&mut self, row: &NewSegment) -> Result<SegmentId, CatalogError> {
        let path = path_text(&row.path)?;
        let root = root_for_path(&self.roots, &row.path)
            .map(path_text)
            .transpose()?;
        let tx = write_transaction(&mut self.conn)?;
        let entry_id = upsert_sample_entry(&tx, row.sample_entry.as_deref())?;
        tx.execute(
            "INSERT INTO segment (camera_id, kind, path, root, run_seq, discontinuity, \
             wall_start_us, wall_end_us, media_start, media_end, init_len, durable_len, \
             fragments, bytes, sealed, sample_entry_id) \
             VALUES (:camera, :kind, :path, :root, :run_seq, :disc, :wall_start, :wall_end, \
             :media_start, :media_end, :init_len, :len, :fragments, :len, 1, :entry) \
             ON CONFLICT(path) DO UPDATE SET run_seq = excluded.run_seq, \
             discontinuity = excluded.discontinuity, wall_start_us = excluded.wall_start_us, \
             wall_end_us = excluded.wall_end_us, media_start = excluded.media_start, \
             media_end = excluded.media_end, init_len = excluded.init_len, \
             durable_len = excluded.durable_len, fragments = excluded.fragments, \
             bytes = excluded.bytes, sealed = 1, deleting = 0, \
             sample_entry_id = excluded.sample_entry_id",
            named_params! {
                ":camera": row.key.camera.as_str(),
                ":kind": row.key.kind.as_str(),
                ":path": path,
                ":root": root,
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
            params![path],
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
        after: Option<EventCursor>,
        limit: usize,
    ) -> Result<Vec<EventRow>, CatalogError> {
        let seek_after = after.is_some_and(|cursor| cursor.time >= range.from);
        let sql = event_query_sql(camera.is_some(), seek_after);
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let limit = i64::try_from(limit.min(MAX_EVENT_READ_ROWS)).unwrap_or(i64::MAX);
        let lower_bound = event_query_lower_bound(range.from, after);
        let mut rows = stmt.query(params![
            camera.map(CameraId::as_str),
            lower_bound,
            range.to.get(),
            after.map(|cursor| cursor.time.get()),
            after.map(|cursor| cursor.id.0),
            limit,
        ])?;
        let mut result = Vec::with_capacity(usize::try_from(limit).unwrap_or(0));
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            let event = decode_event(event_row(row).map_err(row_error)?)?;
            let row_bytes = event_owned_bytes(&event);
            if row_bytes > MAX_EVENT_READ_BYTES {
                return Err(CatalogError::EventReadTooLarge {
                    bytes: row_bytes,
                    max_bytes: MAX_EVENT_READ_BYTES,
                });
            }
            let Some(next_bytes) = bytes.checked_add(row_bytes) else {
                break;
            };
            if next_bytes > MAX_EVENT_READ_BYTES {
                break;
            }
            bytes = next_bytes;
            result.push(event);
        }
        Ok(result)
    }

    /// Sets (or clears, with `None`) the time until which an event's footage
    /// is kept, and returns the event.
    ///
    /// # Errors
    ///
    /// [`CatalogError::UnknownEvent`] when no event has that id;
    /// [`CatalogError::EventRetentionLimit`] when a mark that becomes active
    /// would exceed 100,000 active marks or 128 MiB of their logical bytes;
    /// [`CatalogError::Corrupt`] for malformed accounting.
    pub fn retain_event(
        &mut self,
        id: EventId,
        until: Option<WallUs>,
    ) -> Result<EventRow, CatalogError> {
        let tx = write_transaction(&mut self.conn)?;
        let stored: Option<(i64, Option<i64>)> = tx
            .query_row(
                "SELECT storage_bytes, retain_until_us FROM event WHERE id = ?1",
                params![id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(row_error)?;
        let Some((storage_bytes, previous_until)) = stored else {
            return Err(CatalogError::UnknownEvent(id));
        };
        let now = WallUs::now().get();
        if until.is_some_and(|value| value.get() > now)
            && previous_until.is_none_or(|value| value <= now)
        {
            let (rows, bytes, invalid): (i64, i64, i64) = tx
                .query_row(
                    "SELECT COUNT(*), COALESCE(SUM(storage_bytes), 0), \
                     COALESCE(SUM(CASE WHEN typeof(storage_bytes) = 'integer' \
                                           AND storage_bytes >= 0 THEN 0 ELSE 1 END), 0) \
                     FROM event \
                 WHERE retain_until_us > ?1",
                    params![now],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(row_error)?;
            if invalid != 0 {
                return Err(corrupt("event.storage_bytes is not a nonnegative integer"));
            }
            let rows = checked_u64(rows, "active retained-event count")?;
            let bytes = checked_u64(bytes, "active retained-event bytes")?;
            let row_bytes = checked_u64(storage_bytes, "event.storage_bytes")?;
            if rows >= MAX_RETAINED_EVENT_ROWS
                || row_bytes > MAX_RETAINED_EVENT_BYTES.saturating_sub(bytes)
            {
                return Err(CatalogError::EventRetentionLimit);
            }
        }
        tx.execute(
            "UPDATE event SET retain_until_us = ?1 WHERE id = ?2",
            params![until.map(WallUs::get), id.0],
        )?;
        let sql = format!("SELECT {EVENT_COLUMNS} FROM event WHERE id = ?1");
        let row = tx
            .query_row(&sql, params![id.0], event_row)
            .map_err(row_error)?;
        let row = decode_event(row)?;
        tx.commit()?;
        Ok(row)
    }

    /// Root-aware retention page with raw indexed scan progress after an
    /// optional stable cursor.
    pub fn retention_candidates(
        &mut self,
        root: Option<&Path>,
        after: Option<SegmentCursor>,
        now: WallUs,
        margin_us: i64,
        limit: usize,
    ) -> Result<RetentionPage, CatalogError> {
        let root = root.map(path_text).transpose()?;
        let sql = retention_candidate_query_sql(root.is_some(), after.is_some());
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(
                params![
                    root,
                    after.map(|cursor| cursor.wall_start.get()),
                    after.map(|cursor| cursor.id.0),
                    now.get(),
                    margin_us,
                    limit,
                ],
                |row| Ok((segment_row(row)?, row.get::<_, i64>(APPENDED_COLUMN)?)),
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(row_error)?;
        decode_retention_page(rows)
    }

    /// Expired retention page with raw indexed scan progress for one stream
    /// after an optional stable cursor.
    pub fn expired_candidates(
        &mut self,
        key: &StreamKey,
        cutoff: WallUs,
        after: Option<SegmentCursor>,
        now: WallUs,
        margin_us: i64,
        limit: usize,
    ) -> Result<RetentionPage, CatalogError> {
        let sql = expired_candidate_query_sql(after.is_some());
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(
                params![
                    key.camera.as_str(),
                    key.kind.as_str(),
                    cutoff.get(),
                    after.map(|cursor| cursor.wall_start.get()),
                    after.map(|cursor| cursor.id.0),
                    now.get(),
                    margin_us,
                    limit,
                ],
                |row| Ok((segment_row(row)?, row.get::<_, i64>(APPENDED_COLUMN)?)),
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(row_error)?;
        decode_retention_page(rows)
    }

    /// Inserts an event and returns its id.
    pub fn insert_event(&mut self, event: &Event) -> Result<EventId, CatalogError> {
        let tx = write_transaction(&mut self.conn)?;
        let mut remaining = EVENT_PRUNE_BATCH;
        let id = try_insert_event(&tx, event, WallUs::now(), EVENT_LIMITS, &mut remaining)?;
        tx.commit()?;
        id.ok_or(CatalogError::EventStorageLimit)
    }

    /// Runs one bounded event-lifecycle maintenance step, including the
    /// recording-horizon rule.
    pub fn maintain_events(&mut self, now: WallUs) -> Result<(), CatalogError> {
        let tx = write_transaction(&mut self.conn)?;
        let next = maintain_events_tx(
            &tx,
            now,
            /* recording_horizon */ true,
            EVENT_LIMITS,
            self.event_maintenance_cursor,
        )?;
        tx.commit()?;
        self.event_maintenance_cursor = next;
        Ok(())
    }

    /// Aggregate counts.
    pub fn stats(&mut self) -> Result<CatalogStats, CatalogError> {
        let (segments, sealed_bytes, deleting_segments, deleting_bytes, invalid_segments): (
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = self
            .conn
            .query_row(
                "SELECT COUNT(*), \
                 COALESCE(SUM(CASE WHEN sealed = 1 THEN bytes ELSE 0 END), 0), \
                 COALESCE(SUM(CASE WHEN deleting != 0 THEN 1 ELSE 0 END), 0), \
                 COALESCE(SUM(CASE WHEN deleting != 0 THEN bytes ELSE 0 END), 0), \
                 COALESCE(SUM(CASE WHEN typeof(sealed) = 'integer' AND sealed IN (0, 1) \
                                        AND typeof(bytes) = 'integer' AND bytes >= 0 \
                                   THEN 0 ELSE 1 END), 0) FROM segment",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .map_err(row_error)?;
        if invalid_segments != 0 {
            return Err(corrupt(
                "segment.sealed or segment.bytes violates its persisted type",
            ));
        }
        let (events, event_bytes, pruned_events): (i64, i64, i64) = self
            .conn
            .query_row(
                "SELECT event_rows, event_bytes, pruned_events FROM catalog_state \
                 WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(row_error)?;
        let retained_events: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM event WHERE retain_until_us > ?1",
                params![WallUs::now().get()],
                |row| row.get(0),
            )
            .map_err(row_error)?;
        Ok(CatalogStats {
            segments: checked_u64(segments, "segment count")?,
            sealed_bytes: checked_u64(sealed_bytes, "sealed byte total")?,
            deleting_segments: checked_u64(deleting_segments, "deleting segment count")?,
            deleting_bytes: checked_u64(deleting_bytes, "deleting byte total")?,
            events: checked_u64(events, "event count")?,
            event_bytes: checked_u64(event_bytes, "event logical byte total")?,
            retained_events: checked_u64(retained_events, "active retained-event count")?,
            pruned_events: checked_u64(pruned_events, "pruned-event count")?,
            db_bytes: file_len(&self.path),
            wal_bytes: file_len(&wal_path(&self.path)),
        })
    }
}

/// Records durable progress of an open file; a path the catalog does not
/// know, or one in deletion intent, is logged and skipped.
fn apply_segment_progress(
    tx: &Transaction<'_>,
    path: &Path,
    durable_len: u64,
    fragments: u32,
    wall_end: WallUs,
    media_end: u64,
) -> Result<(), CatalogError> {
    let updated = tx.execute(
        "UPDATE segment SET durable_len = :durable, fragments = :fragments, \
         wall_end_us = :wall_end, media_end = :media_end, \
         bytes = MAX(bytes, :durable) WHERE path = :path AND deleting = 0",
        named_params! {
            ":durable": as_i64(durable_len),
            ":fragments": i64::from(fragments),
            ":wall_end": wall_end.get(),
            ":media_end": as_i64(media_end),
            ":path": path_text(path)?,
        },
    )?;
    if updated == 0 {
        warn!(
            path = %path.display(),
            "progress for a segment the catalog does not know"
        );
    }
    Ok(())
}

/// Seals an open file with its final lengths; a path the catalog does not
/// know, or one in deletion intent, is logged and skipped.
fn apply_segment_sealed(
    tx: &Transaction<'_>,
    path: &Path,
    len: u64,
    fragments: u32,
    wall_end: WallUs,
    media_end: u64,
) -> Result<(), CatalogError> {
    let updated = tx.execute(
        "UPDATE segment SET sealed = 1, bytes = :len, durable_len = :len, \
         fragments = :fragments, wall_end_us = :wall_end, media_end = :media_end \
         WHERE path = :path AND deleting = 0",
        named_params! {
            ":len": as_i64(len),
            ":fragments": i64::from(fragments),
            ":wall_end": wall_end.get(),
            ":media_end": as_i64(media_end),
            ":path": path_text(path)?,
        },
    )?;
    if updated == 0 {
        warn!(
            path = %path.display(),
            "seal for a segment the catalog does not know"
        );
    }
    Ok(())
}

/// The fields of a [`Change::SegmentOpened`], borrowed from the batch.
#[derive(Clone, Copy)]
struct OpenedSegment<'a> {
    key: &'a StreamKey,
    path: &'a Path,
    run_seq: u64,
    discontinuity: bool,
    wall_start: WallUs,
    media_start: u64,
    init_len: u32,
    sample_entry: &'a SampleEntry,
}

/// Inserts the row of an opened file, or resets the row its path already has
/// (fragments, seal, and deletion intent cleared).
fn apply_segment_opened(
    tx: &Transaction<'_>,
    roots: &[PathBuf],
    opened: OpenedSegment<'_>,
) -> Result<(), CatalogError> {
    let OpenedSegment {
        key,
        path,
        run_seq,
        discontinuity,
        wall_start,
        media_start,
        init_len,
        sample_entry,
    } = opened;
    let root = root_for_path(roots, path).map(path_text).transpose()?;
    let entry_id = upsert_sample_entry(tx, Some(sample_entry))?;
    tx.execute(
        "INSERT INTO segment (camera_id, kind, path, root, run_seq, discontinuity, \
         wall_start_us, wall_end_us, media_start, media_end, init_len, \
         durable_len, fragments, bytes, sealed, sample_entry_id) \
         VALUES (:camera, :kind, :path, :root, :run_seq, :disc, :wall, :wall, :media, \
         :media, :init_len, :init_len, 0, :init_len, 0, :entry) \
         ON CONFLICT(path) DO UPDATE SET camera_id = excluded.camera_id, \
         kind = excluded.kind, root = excluded.root, run_seq = excluded.run_seq, \
         discontinuity = excluded.discontinuity, wall_start_us = excluded.wall_start_us, \
         wall_end_us = excluded.wall_end_us, media_start = excluded.media_start, \
         media_end = excluded.media_end, init_len = excluded.init_len, \
         durable_len = excluded.durable_len, fragments = 0, bytes = excluded.bytes, \
         sealed = 0, deleting = 0, sample_entry_id = excluded.sample_entry_id",
        named_params! {
            ":camera": key.camera.as_str(),
            ":kind": key.kind.as_str(),
            ":path": path_text(path)?,
            ":root": root,
            ":run_seq": as_i64(run_seq),
            ":disc": discontinuity,
            ":wall": wall_start.get(),
            ":media": as_i64(media_start),
            ":init_len": i64::from(init_len),
            ":entry": entry_id,
        },
    )?;
    Ok(())
}

fn upsert_sample_entry(
    tx: &Transaction<'_>,
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
         ON CONFLICT(hash, config) DO NOTHING",
        params![hash, codecs, width, height, config],
    )?;
    let id: i64 = tx.query_row(
        "SELECT id FROM sample_entry WHERE hash = ?1 AND config = ?2",
        params![hash, config],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Inserts one event that [`Event::bounded`] has already bounded.
fn insert_event_row(tx: &Transaction<'_>, event: &Event) -> Result<EventId, CatalogError> {
    let (kind, kind_other) = kind_storage(&event.kind);
    let storage_bytes = i64::try_from(event_storage_bytes(event))
        .expect("a bounded event is far below i64::MAX bytes");
    tx.execute(
        "INSERT INTO event (camera_id, source, kind, kind_other, state, nvr_time_us, \
         camera_time_us, data, storage_bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            event.camera.as_str(),
            event.source.as_str(),
            kind,
            kind_other,
            event.state.as_str(),
            event.nvr_time.get(),
            event.camera_time.map(WallUs::get),
            event.data,
            storage_bytes,
        ],
    )?;
    Ok(EventId(tx.last_insert_rowid()))
}

fn try_insert_event(
    tx: &Transaction<'_>,
    event: &Event,
    now: WallUs,
    limits: EventLimits,
    remaining: &mut usize,
) -> Result<Option<EventId>, CatalogError> {
    let event = event.clone().bounded();
    let incoming_bytes = u64::try_from(event_storage_bytes(&event))
        .expect("a bounded event is far below u64::MAX bytes");
    if limits.rows == 0 || incoming_bytes > limits.bytes {
        return Ok(None);
    }
    if !event_fits(tx, incoming_bytes, limits)? {
        clear_expired_marks(tx, now, remaining)?;
    }
    if !event_fits(tx, incoming_bytes, limits)? {
        prune_for_event(tx, incoming_bytes, limits, remaining)?;
    }
    if event_fits(tx, incoming_bytes, limits)? {
        insert_event_row(tx, &event).map(Some)
    } else {
        Ok(None)
    }
}

fn event_fits(
    tx: &Transaction<'_>,
    incoming_bytes: u64,
    limits: EventLimits,
) -> Result<bool, CatalogError> {
    let (rows, bytes) = event_state(tx)?;
    Ok(rows < limits.rows && incoming_bytes <= limits.bytes.saturating_sub(bytes))
}

fn event_state(tx: &Transaction<'_>) -> Result<(u64, u64), CatalogError> {
    let mut stmt = tx.prepare_cached(EVENT_STATE_SQL)?;
    let (rows, bytes): (i64, i64) = stmt
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(row_error)?;
    Ok((
        checked_u64(rows, "event lifecycle row count")?,
        checked_u64(bytes, "event lifecycle byte count")?,
    ))
}

fn clear_expired_marks(
    tx: &Transaction<'_>,
    now: WallUs,
    remaining: &mut usize,
) -> Result<(), CatalogError> {
    if *remaining == 0 {
        return Ok(());
    }
    let mut stmt = tx.prepare_cached(CLEAR_EXPIRED_MARKS_SQL)?;
    let cleared = stmt.execute(params![
        now.get(),
        i64::try_from(*remaining).unwrap_or(i64::MAX)
    ])?;
    *remaining = remaining.saturating_sub(cleared);
    Ok(())
}

fn prune_for_event(
    tx: &Transaction<'_>,
    incoming_bytes: u64,
    limits: EventLimits,
    remaining: &mut usize,
) -> Result<(), CatalogError> {
    if *remaining == 0 {
        return Ok(());
    }
    let (rows, bytes) = event_state(tx)?;
    let mut stmt = tx.prepare(
        "SELECT storage_bytes FROM event WHERE retain_until_us IS NULL \
         ORDER BY nvr_time_us, id LIMIT ?1",
    )?;
    let candidates = stmt
        .query_map(
            params![i64::try_from(*remaining).unwrap_or(i64::MAX)],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<Result<Vec<_>, _>>()
        .map_err(row_error)?;
    let mut remove = 0usize;
    let mut freed = 0u64;
    for candidate in candidates {
        remove += 1;
        freed = freed.saturating_add(checked_u64(candidate, "event.storage_bytes")?);
        if rows.saturating_sub(u64::try_from(remove).unwrap_or(u64::MAX)) < limits.rows
            && incoming_bytes <= limits.bytes.saturating_sub(bytes.saturating_sub(freed))
        {
            break;
        }
    }
    if remove == 0 {
        return Ok(());
    }
    let deleted = tx.execute(
        "DELETE FROM event WHERE id IN ( \
           SELECT id FROM event WHERE retain_until_us IS NULL \
           ORDER BY nvr_time_us, id LIMIT ?1 \
         )",
        params![i64::try_from(remove).unwrap_or(i64::MAX)],
    )?;
    *remaining = remaining.saturating_sub(deleted);
    Ok(())
}

fn prune_event_horizon(
    tx: &Transaction<'_>,
    now: WallUs,
    after: Option<EventCursor>,
    limit: usize,
) -> Result<(Option<EventCursor>, usize), CatalogError> {
    let sql = inactive_event_query_sql(after.is_some());
    let candidates = {
        let mut stmt = tx.prepare_cached(sql)?;
        stmt.query_map(
            params![
                after.map(|cursor| cursor.time.get()),
                after.map(|cursor| cursor.id.0),
                i64::try_from(limit).unwrap_or(i64::MAX),
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()
        .map_err(row_error)?
    };
    let scanned = candidates.len();
    let Some(last) = candidates.last() else {
        return Ok((None, 0));
    };
    let next = EventCursor {
        time: WallUs::new(last.2),
        id: EventId(checked_id(last.0, "event.id")?),
    };

    let age_cutoff = now.get().saturating_sub(MAX_EVENT_AGE_US);
    let mut horizons = HashMap::<String, Option<i64>>::new();
    let mut horizon_stmt = tx.prepare_cached(EVENT_HORIZON_SQL)?;
    let mut delete_stmt =
        tx.prepare_cached("DELETE FROM event WHERE id = ?1 AND retain_until_us IS NULL")?;
    for (id, camera, time) in candidates {
        CameraId::try_new(&camera)
            .map_err(|_| corrupt("event.camera_id is not a valid camera identifier"))?;
        let expired_by_age = time < age_cutoff;
        let expired_by_horizon = if expired_by_age {
            false
        } else if let Some(horizon) = horizons.get(&camera) {
            horizon.is_some_and(|start| time < start.saturating_sub(EVENT_RECORDING_MARGIN_US))
        } else {
            let horizon = horizon_stmt
                .query_row(params![camera], |row| row.get::<_, Option<i64>>(0))
                .map_err(row_error)?;
            horizons.insert(camera.clone(), horizon);
            horizon.is_some_and(|start| time < start.saturating_sub(EVENT_RECORDING_MARGIN_US))
        };
        if expired_by_age || expired_by_horizon {
            delete_stmt.execute(params![id])?;
        }
    }
    Ok((Some(next), scanned))
}

fn maintain_events_tx(
    tx: &Transaction<'_>,
    now: WallUs,
    recording_horizon: bool,
    limits: EventLimits,
    after: Option<EventCursor>,
) -> Result<Option<EventCursor>, CatalogError> {
    let mut remaining = limits.prune_batch;
    clear_expired_marks(tx, now, &mut remaining)?;
    if remaining == 0 {
        return Ok(after);
    }

    let next = if recording_horizon {
        let (next, scanned) = prune_event_horizon(tx, now, after, remaining)?;
        remaining = remaining.saturating_sub(scanned);
        next
    } else {
        after
    };
    if remaining == 0 {
        return Ok(next);
    }

    let mut state_stmt = tx.prepare_cached(EVENT_STATE_SQL)?;
    let (rows, bytes): (i64, i64) = state_stmt
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(row_error)?;
    let rows = checked_u64(rows, "event lifecycle row count")?;
    let bytes = checked_u64(bytes, "event lifecycle byte count")?;
    if rows <= limits.rows && bytes <= limits.bytes {
        return Ok(next);
    }
    tx.execute(
        "DELETE FROM event WHERE id IN ( \
           SELECT id FROM event WHERE retain_until_us IS NULL \
           ORDER BY nvr_time_us, id LIMIT ?1 \
         )",
        params![i64::try_from(remaining).unwrap_or(i64::MAX)],
    )?;
    Ok(next)
}

/// Length of the file at `path`, or zero when it is absent or unreadable.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn event_owned_bytes(row: &EventRow) -> usize {
    let kind = match &row.event.kind {
        EventKind::Other(code) => code.len(),
        _ => 0,
    };
    size_of::<EventRow>()
        .saturating_add(row.event.camera.as_str().len())
        .saturating_add(kind)
        .saturating_add(row.event.data.as_ref().map_or(0, String::len))
}

fn event_storage_bytes(event: &Event) -> usize {
    let kind_bytes = match &event.kind {
        EventKind::Other(code) => "other".len().saturating_add(code.len()),
        kind => kind.name().len(),
    };
    event
        .camera
        .as_str()
        .len()
        .saturating_add(event.source.as_str().len())
        .saturating_add(kind_bytes)
        .saturating_add(event.state.as_str().len())
        .saturating_add(event.data.as_ref().map_or(0, String::len))
}

fn root_for_path<'a>(roots: &'a [PathBuf], path: &Path) -> Option<&'a Path> {
    root_index_for_path(roots, path).map(|index| roots[index].as_path())
}

fn root_index_for_path(roots: &[PathBuf], path: &Path) -> Option<usize> {
    roots
        .iter()
        .enumerate()
        .filter_map(|(index, root)| {
            let relative = path.strip_prefix(root).ok()?;
            relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
                .then_some((index, root.components().count()))
        })
        .max_by_key(|(_, depth)| *depth)
        .map(|(index, _)| index)
}

/// The write-ahead log that SQLite keeps next to `db`: its name with `-wal` appended.
fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

struct PersistedSegment {
    id: i64,
    camera: String,
    kind: String,
    path: String,
    run_seq: i64,
    discontinuity: i64,
    wall_start: i64,
    wall_end: i64,
    media_start: i64,
    media_end: i64,
    init_len: i64,
    durable_len: i64,
    fragments: i64,
    bytes: i64,
    sealed: i64,
    deleting: i64,
    codecs: String,
    width: i64,
    height: i64,
    sample_entry_hash: i64,
    sample_entry_id: i64,
}

/// Reads raw SQLite values. Semantic conversion happens outside rusqlite's
/// callback so corruption remains a typed catalog error.
fn segment_row(row: &Row<'_>) -> rusqlite::Result<PersistedSegment> {
    Ok(PersistedSegment {
        id: row.get(0)?,
        camera: row.get(1)?,
        kind: row.get(2)?,
        path: row.get(3)?,
        run_seq: row.get(4)?,
        discontinuity: row.get(5)?,
        wall_start: row.get(6)?,
        wall_end: row.get(7)?,
        media_start: row.get(8)?,
        media_end: row.get(9)?,
        init_len: row.get(10)?,
        durable_len: row.get(11)?,
        fragments: row.get(12)?,
        bytes: row.get(13)?,
        sealed: row.get(14)?,
        deleting: row.get(15)?,
        codecs: row.get(16)?,
        width: row.get(17)?,
        height: row.get(18)?,
        sample_entry_hash: row.get(19)?,
        sample_entry_id: row.get(20)?,
    })
}

fn segment_deleting(row: &PersistedSegment) -> Result<bool, CatalogError> {
    checked_bool(row.deleting, "segment.deleting")
}

fn decode_segment(row: PersistedSegment) -> Result<SegmentRow, CatalogError> {
    let camera = CameraId::try_new(&row.camera)
        .map_err(|_| corrupt("segment.camera_id is not a valid camera identifier"))?;
    let kind = row
        .kind
        .parse::<StreamKind>()
        .map_err(|_| corrupt("segment.kind is not a valid stream kind"))?;
    segment_deleting(&row)?;
    Ok(SegmentRow {
        id: SegmentId(checked_id(row.id, "segment.id")?),
        key: StreamKey { camera, kind },
        path: PathBuf::from(row.path),
        run_seq: checked_u64(row.run_seq, "segment.run_seq")?,
        discontinuity: checked_bool(row.discontinuity, "segment.discontinuity")?,
        wall_start: WallUs::new(row.wall_start),
        wall_end: WallUs::new(row.wall_end),
        media_start: checked_u64(row.media_start, "segment.media_start")?,
        media_end: checked_u64(row.media_end, "segment.media_end")?,
        init_len: checked_u32(row.init_len, "segment.init_len")?,
        durable_len: checked_u64(row.durable_len, "segment.durable_len")?,
        fragments: checked_u32(row.fragments, "segment.fragments")?,
        bytes: checked_u64(row.bytes, "segment.bytes")?,
        sealed: checked_bool(row.sealed, "segment.sealed")?,
        codecs: row.codecs,
        width: checked_u16(row.width, "sample_entry.width")?,
        height: checked_u16(row.height, "sample_entry.height")?,
        sample_entry_hash: checked_u32(row.sample_entry_hash, "sample_entry.hash")?,
        sample_entry_id: SampleEntryId(checked_id(row.sample_entry_id, "segment.sample_entry_id")?),
    })
}

fn decode_retention_page(
    rows: Vec<(PersistedSegment, i64)>,
) -> Result<RetentionPage, CatalogError> {
    let scanned = rows.len();
    let mut result = Vec::with_capacity(scanned);
    let mut next = None;
    for (row, unprotected) in rows {
        let row = decode_segment(row)?;
        next = Some(SegmentCursor::from(&row));
        if checked_bool(unprotected, "retention protection result")? {
            result.push(row);
        }
    }
    Ok(RetentionPage {
        rows: result,
        next,
        scanned,
    })
}

struct PersistedEvent {
    id: i64,
    camera: String,
    source: String,
    kind: String,
    kind_other: Option<String>,
    state: String,
    nvr_time: i64,
    camera_time: Option<i64>,
    data: Option<String>,
    retain_until: Option<i64>,
}

fn event_row(row: &Row<'_>) -> rusqlite::Result<PersistedEvent> {
    Ok(PersistedEvent {
        id: row.get(0)?,
        camera: row.get(1)?,
        source: row.get(2)?,
        kind: row.get(3)?,
        kind_other: row.get(4)?,
        state: row.get(5)?,
        nvr_time: row.get(6)?,
        camera_time: row.get(7)?,
        data: row.get(8)?,
        retain_until: row.get(9)?,
    })
}

fn decode_event(row: PersistedEvent) -> Result<EventRow, CatalogError> {
    let camera = CameraId::try_new(&row.camera)
        .map_err(|_| corrupt("event.camera_id is not a valid camera identifier"))?;
    Ok(EventRow {
        id: EventId(checked_id(row.id, "event.id")?),
        event: Event {
            camera,
            source: source_from_name(&row.source)?,
            kind: kind_from_storage(&row.kind, row.kind_other)?,
            state: state_from_name(&row.state)?,
            nvr_time: WallUs::new(row.nvr_time),
            camera_time: row.camera_time.map(WallUs::new),
            data: row.data,
        },
        retain_until: row.retain_until.map(WallUs::new),
    })
}

/// The text a path is stored as. The path columns hold text, and a lossy
/// conversion would merge distinct non-UTF-8 paths, so such a path is refused.
fn path_text(path: &Path) -> Result<&str, CatalogError> {
    path.to_str()
        .ok_or_else(|| CatalogError::NonUtf8Path(path.to_path_buf()))
}

fn as_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn checked_id(value: i64, field: &str) -> Result<i64, CatalogError> {
    if value > 0 {
        Ok(value)
    } else {
        Err(corrupt(&format!("{field} must be positive")))
    }
}

fn checked_u64(value: i64, field: &str) -> Result<u64, CatalogError> {
    u64::try_from(value).map_err(|_| corrupt(&format!("{field} must be nonnegative")))
}

fn checked_u32(value: i64, field: &str) -> Result<u32, CatalogError> {
    u32::try_from(value).map_err(|_| corrupt(&format!("{field} is outside the u32 range")))
}

fn checked_u16(value: i64, field: &str) -> Result<u16, CatalogError> {
    u16::try_from(value).map_err(|_| corrupt(&format!("{field} is outside the u16 range")))
}

fn checked_bool(value: i64, field: &str) -> Result<bool, CatalogError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(corrupt(&format!("{field} must be zero or one"))),
    }
}

fn source_from_name(name: &str) -> Result<EventSource, CatalogError> {
    name.parse()
        .map_err(|_| corrupt("event.source is not a known event source"))
}

fn state_from_name(name: &str) -> Result<EventState, CatalogError> {
    name.parse()
        .map_err(|_| corrupt("event.state is not a known event state"))
}

/// The persisted kind discriminator and optional vendor-specific value: a
/// built-in kind's name, or `other` with the vendor code beside it.
fn kind_storage(kind: &EventKind) -> (&str, Option<&str>) {
    match kind {
        EventKind::Other(code) => ("other", Some(code)),
        other => (other.name(), None),
    }
}

fn kind_from_storage(name: &str, other: Option<String>) -> Result<EventKind, CatalogError> {
    if name == "other" {
        return other
            .map(EventKind::Other)
            .ok_or_else(|| corrupt("event.kind_other is missing for an other event"));
    }
    if other.is_some() {
        return Err(corrupt(
            "event.kind_other is present for a built-in event kind",
        ));
    }
    name.parse()
        .map_err(|_| corrupt("event.kind is not a known event kind"))
}

fn corrupt(detail: &str) -> CatalogError {
    CatalogError::Corrupt(detail.to_owned())
}

/// Turns a value that does not decode as its column's type into a corruption
/// report that names the column.
fn row_error(error: rusqlite::Error) -> CatalogError {
    match error {
        rusqlite::Error::FromSqlConversionFailure(index, ty, source) => corrupt(&format!(
            "column {index} holds a {ty} value that does not convert: {source}"
        )),
        rusqlite::Error::IntegralValueOutOfRange(index, value) => {
            corrupt(&format!("column {index} holds {value}, outside its range"))
        }
        rusqlite::Error::InvalidColumnType(index, name, ty) => {
            corrupt(&format!("column {name} ({index}) holds a {ty} value"))
        }
        other => CatalogError::Sqlite(other),
    }
}

/// Creates an empty owner-only database file when none exists; SQLite opens an
/// empty file as a new database. A file already at the path is left for SQLite
/// to open.
fn create_private_database(path: &Path) -> std::io::Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Begins a write transaction that takes SQLite's write lock before its first
/// statement.
///
/// A deferred transaction that reads first already holds a read snapshot when
/// it upgrades to a writer; SQLite then returns `SQLITE_BUSY` at once instead of
/// invoking the busy handler, because waiting could deadlock against another
/// writer. `BEGIN IMMEDIATE` acquires the lock up front, so the connection's
/// busy timeout covers lock acquisition on every write path, including an
/// operator shell, a checkpoint, or a backup holding the lock briefly.
fn write_transaction(conn: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}

/// Fails when any row references a missing parent; run after a migration that
/// rebuilt a referenced table with foreign-key enforcement off.
fn check_foreign_keys(tx: &Transaction<'_>) -> Result<(), CatalogError> {
    let violations: i64 =
        tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations == 0 {
        Ok(())
    } else {
        Err(corrupt(&format!(
            "{violations} rows reference a missing parent after migration"
        )))
    }
}

/// Reads the stored schema version without changing persistent SQLite pragmas.
fn stored_schema_version(conn: &Connection) -> Result<i64, CatalogError> {
    let has_version: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get::<_, i64>(0).map(|count| count == 1),
    )?;
    if !has_version {
        return Ok(0);
    }
    let mut stmt = conn.prepare("SELECT version FROM schema_version")?;
    let versions = stmt
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(row_error)?;
    let [version] = versions.as_slice() else {
        return Err(corrupt(
            "schema_version must contain exactly one version row",
        ));
    };
    if *version < 1 {
        return Err(corrupt("schema_version must be positive"));
    }
    Ok(*version)
}

/// Rejects a `segment` table whose `sealed` (and, when the column exists,
/// `deleting`) values are not integer booleans, in one scan of the table.
fn validate_segment_booleans(
    conn: &Connection,
    validate_deleting: bool,
) -> Result<(), CatalogError> {
    let deleting_clause = if validate_deleting {
        ", EXISTS(SELECT 1 FROM segment \
           WHERE typeof(deleting) != 'integer' OR deleting NOT IN (0, 1))"
    } else {
        ", 0"
    };
    let (invalid_sealed, invalid_deleting): (i64, i64) = conn
        .query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM segment \
                   WHERE typeof(sealed) != 'integer' OR sealed NOT IN (0, 1)){deleting_clause}"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(row_error)?;
    if invalid_sealed != 0 {
        return Err(corrupt("segment.sealed is not an integer boolean"));
    }
    if invalid_deleting != 0 {
        return Err(corrupt("segment.deleting is not an integer boolean"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn segment(path: PathBuf) -> NewSegment {
        NewSegment {
            key: StreamKey {
                camera: CameraId::try_new("cam1").unwrap(),
                kind: StreamKind::Main,
            },
            path,
            run_seq: 1,
            discontinuity: true,
            wall_start: WallUs::new(1_000_000),
            wall_end: WallUs::new(2_000_000),
            media_start: 0,
            media_end: 90_000,
            init_len: 700,
            len: 12_345,
            fragments: 3,
            sample_entry: None,
        }
    }

    fn seed_inactive_events(db: &Connection, count: i64, camera: &str, start: i64) {
        db.execute(
            "WITH RECURSIVE seq(value) AS ( \
               VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < ?1 \
             ) INSERT INTO event( \
               camera_id, source, kind, state, nvr_time_us, storage_bytes \
             ) SELECT ?2, 'manual', 'motion', 'pulse', ?3 + value, 0 FROM seq",
            params![count, camera, start],
        )
        .unwrap();
    }

    fn cursor(wall_start: i64, id: i64) -> SegmentCursor {
        SegmentCursor {
            wall_start: WallUs::new(wall_start),
            id: SegmentId(id),
        }
    }

    /// The `(type, name, table, sql)` rows of `sqlite_master`, sorted.
    fn schema_rows(conn: &Connection) -> Vec<(String, String, String, Option<String>)> {
        let mut rows = conn
            .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows.sort();
        rows
    }

    /// The `EXPLAIN QUERY PLAN` detail lines of `sql`, joined; the statement's
    /// parameters stay unbound, which the planner treats as unknown values.
    fn explain(conn: &Connection, sql: &str) -> String {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let mut rows = statement.raw_query();
        let mut details = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            details.push(row.get::<_, String>(3).unwrap());
        }
        details.join("; ")
    }

    fn statement_steps(conn: &Connection, sql: &str) -> i32 {
        conn.prepare_cached(sql)
            .unwrap()
            .get_status(rusqlite::StatementStatus::VmStep)
    }

    fn reset_statement_steps(conn: &Connection, sql: &str) {
        conn.prepare_cached(sql)
            .unwrap()
            .reset_status(rusqlite::StatementStatus::VmStep);
    }

    /// A version-1 database gains both later lifecycle columns on open.
    #[test]
    fn a_version_one_database_migrates_to_the_supported_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
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
        let segment_columns: Vec<String> = db
            .conn
            .prepare("PRAGMA table_info(segment)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(segment_columns.iter().any(|column| column == "deleting"));
    }

    fn stream_range() -> (StreamKey, TimeRange) {
        (
            StreamKey {
                camera: CameraId::try_new("cam1").unwrap(),
                kind: StreamKind::Main,
            },
            TimeRange {
                from: WallUs::new(0),
                to: WallUs::new(10_000_000),
            },
        )
    }

    fn segment_at(path: &str, second: i64, entry: Option<Arc<SampleEntry>>) -> NewSegment {
        let mut row = segment(PathBuf::from(path));
        row.wall_start = WallUs::new(second * 1_000_000);
        row.wall_end = WallUs::new((second + 1) * 1_000_000);
        row.sample_entry = entry;
        row
    }

    fn entry(bytes: &'static [u8], codecs: &str, width: u16) -> SampleEntry {
        SampleEntry::new(
            bytes::Bytes::from_static(bytes),
            codecs.to_owned(),
            width,
            480,
        )
    }

    /// Sample entries whose CRC-32C collide are different entries: each keeps
    /// its row and metadata, and byte-identical entries share one row.
    #[test]
    fn colliding_sample_entries_keep_their_own_rows_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("collide.db")).unwrap();
        let first = Arc::new(entry(b"first entry", "hvc1.1.6.L120.B0", 1920));
        let mut forged = entry(b"other entry", "hvc1.1.6.L153.B0", 3840);
        forged.hash = first.hash;
        let forged = Arc::new(forged);
        let (key, range) = stream_range();
        let one = db
            .adopt_segment(&segment_at(
                "/video/cam1/main/one.mp4",
                1,
                Some(first.clone()),
            ))
            .unwrap();
        let two = db
            .adopt_segment(&segment_at("/video/cam1/main/two.mp4", 3, Some(forged)))
            .unwrap();
        let three = db
            .adopt_segment(&segment_at("/video/cam1/main/three.mp4", 5, Some(first)))
            .unwrap();
        let rows = db.segments(&key, range).unwrap();
        let row = |id: SegmentId| rows.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row(one).sample_entry_hash, row(two).sample_entry_hash);
        assert_ne!(row(one).sample_entry_id, row(two).sample_entry_id);
        assert_eq!(row(one).codecs, "hvc1.1.6.L120.B0");
        assert_eq!(row(one).width, 1920);
        assert_eq!(row(two).codecs, "hvc1.1.6.L153.B0");
        assert_eq!(row(two).width, 3840);
        assert_eq!(row(three).sample_entry_id, row(one).sample_entry_id);
        let entries: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM sample_entry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(entries, 2);
    }

    /// A real entry whose CRC-32C is zero never merges with the placeholder for
    /// an unknown entry.
    #[test]
    fn a_crc_zero_sample_entry_stays_distinct_from_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("zero.db")).unwrap();
        let mut zero = entry(b"zero hash entry", "hvc1.1.6.L93.B0", 640);
        zero.hash = 0;
        let (key, range) = stream_range();
        let known = db
            .adopt_segment(&segment_at(
                "/video/cam1/main/known.mp4",
                1,
                Some(Arc::new(zero)),
            ))
            .unwrap();
        let unknown = db
            .adopt_segment(&segment_at("/video/cam1/main/unknown.mp4", 3, None))
            .unwrap();
        let rows = db.segments(&key, range).unwrap();
        let row = |id: SegmentId| rows.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row(known).sample_entry_hash, 0);
        assert_eq!(row(unknown).sample_entry_hash, 0);
        assert_ne!(row(known).sample_entry_id, row(unknown).sample_entry_id);
        assert_eq!(row(known).codecs, "hvc1.1.6.L93.B0");
        assert_eq!(row(unknown).codecs, "unknown");
    }

    /// The v9 to v10 rebuild keeps every sample entry id, so segments still
    /// point at their entries, and enforcement is back on afterwards.
    #[test]
    fn a_version_nine_database_rebuilds_sample_entries_with_their_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v9.db");
        {
            let conn = Connection::open(&path).unwrap();
            for schema in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
                SCHEMA_V8, SCHEMA_V9,
            ] {
                conn.execute_batch(schema).unwrap();
            }
            conn.execute(
                "INSERT INTO recording_root_state(singleton, pre_identity_catalog) VALUES (1, 0)",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO schema_version (version) VALUES (9)", [])
                .unwrap();
            conn.execute(
                "INSERT INTO sample_entry (id, hash, codecs, width, height, config) VALUES \
                 (5, 77, 'hvc1', 640, 480, x'0102'), (9, 0, 'unknown', 0, 0, x'')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO segment (camera_id, kind, path, run_seq, discontinuity, \
                 wall_start_us, wall_end_us, media_start, media_end, init_len, durable_len, \
                 fragments, bytes, sealed, sample_entry_id) VALUES \
                 ('cam1', 'main', '/video/cam1/main/a.mp4', 1, 1, 1000000, 2000000, 0, 90000, \
                 700, 12345, 3, 12345, 1, 5), \
                 ('cam1', 'main', '/video/cam1/main/b.mp4', 1, 1, 3000000, 4000000, 0, 90000, \
                 700, 12345, 3, 12345, 1, 9)",
                [],
            )
            .unwrap();
        }
        let mut db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let violations: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
        let enforced: i64 = db
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(enforced, 1, "enforcement is back on after the rebuild");
        let (key, range) = stream_range();
        let rows = db.segments(&key, range).unwrap();
        let by_path = |name: &str| {
            rows.iter()
                .find(|row| row.path.ends_with(name))
                .unwrap()
                .sample_entry_id
        };
        assert_eq!(by_path("a.mp4"), SampleEntryId(5));
        assert_eq!(by_path("b.mp4"), SampleEntryId(9));
        db.conn
            .execute(
                "INSERT INTO sample_entry (hash, codecs, width, height, config) \
                 VALUES (77, 'hvc1', 1280, 720, x'0304')",
                [],
            )
            .expect("the same CRC with other bytes is another entry");
        assert!(
            db.conn
                .execute(
                    "INSERT INTO sample_entry (hash, codecs, width, height, config) \
                     VALUES (77, 'hvc1', 1280, 720, x'0102')",
                    [],
                )
                .is_err(),
            "the same CRC and bytes are one entry"
        );
        assert!(
            db.conn
                .execute("DELETE FROM sample_entry WHERE id = 5", [])
                .is_err(),
            "a referenced entry cannot be deleted once enforcement is on"
        );
    }

    #[test]
    fn an_empty_pre_identity_catalog_never_treats_unregistered_roots_as_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v8-empty.db");
        {
            let conn = Connection::open(&path).unwrap();
            for schema in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
                SCHEMA_V8,
            ] {
                conn.execute_batch(schema).unwrap();
            }
            conn.execute("INSERT INTO schema_version (version) VALUES (8)", [])
                .unwrap();
        }
        let absent = dir.path().join("absent-root");
        let recreated = dir.path().join("recreated-root");
        std::fs::create_dir(&recreated).unwrap();
        for root in [&absent, &recreated] {
            let mut registry = CatalogRootRegistry::open(&path).unwrap();
            assert_eq!(
                registry
                    .expectations_with_progress(std::slice::from_ref(root), || {})
                    .unwrap(),
                [CatalogRootExpectation::LegacyOwned]
            );
        }
    }

    #[test]
    fn a_catalog_created_with_root_identities_treats_unregistered_roots_as_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.db");
        let root = dir.path().join("root");
        let mut registry = CatalogRootRegistry::open(&path).unwrap();
        assert_eq!(
            registry
                .expectations_with_progress(std::slice::from_ref(&root), || {})
                .unwrap(),
            [CatalogRootExpectation::Fresh]
        );
    }

    /// 600 open rows that share one start time under `/video`.
    fn six_hundred_open_rows(name: &str) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join(name)).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO sample_entry(id, hash, codecs, width, height, config) \
                 VALUES (1, 1, 'hvc1.test', 1, 1, X'00'); \
                 WITH RECURSIVE seq(value) AS ( \
                   VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < 600 \
                 ) INSERT INTO segment( \
                   camera_id, kind, path, root, run_seq, discontinuity, wall_start_us, \
                   wall_end_us, media_start, media_end, init_len, durable_len, fragments, bytes, \
                   sealed, deleting, sample_entry_id \
                 ) SELECT 'cam1', 'main', printf('/video/cam1/main/2026-01-01/%04d.mp4', value), \
                          '/video', 1, 0, 100, 200, 0, 90, 700, 800, 1, 800, 0, 0, 1 \
                   FROM seq;",
            )
            .unwrap();
        (dir, db)
    }

    /// Every row id of the pages `next_page` yields, following the cursor.
    fn paged_ids(
        mut next_page: impl FnMut(Option<SegmentCursor>) -> Vec<StartupRow>,
        page_size: usize,
    ) -> Vec<SegmentId> {
        let mut ids = Vec::new();
        let mut cursor = None;
        loop {
            let page = next_page(cursor);
            assert!(page.len() <= page_size, "a page of {}", page.len());
            let Some(last) = page.last() else {
                break;
            };
            cursor = Some(SegmentCursor::from(&last.row));
            ids.extend(page.iter().map(|entry| entry.row.id));
        }
        ids
    }

    #[test]
    fn root_expectations_page_through_every_row_in_512_row_pages() {
        let (_dir, mut db) = six_hundred_open_rows("expectations.db");
        let roots = [PathBuf::from("/video")];
        let mut registry_pages = 0usize;
        assert_eq!(
            db.root_expectations(&roots, || registry_pages += 1)
                .unwrap(),
            [CatalogRootExpectation::LegacyOwned]
        );
        assert_eq!(registry_pages, 2, "600 rows require two 512-row pages");
    }

    #[test]
    fn writing_pages_are_bounded_stable_and_exactly_once_across_equal_times() {
        let (_dir, mut db) = six_hundred_open_rows("writing-pages.db");
        let ids = paged_ids(|cursor| db.writing_segments_page(cursor, 31).unwrap(), 31);
        assert_eq!(ids.len(), 600);
        assert!(ids.windows(2).all(|ids| ids[0] < ids[1]), "{ids:?}");
    }

    #[test]
    fn deleting_pages_visit_the_same_rows_as_writing_pages_once_marked() {
        let (_dir, mut db) = six_hundred_open_rows("deleting-pages.db");
        let writing_ids = paged_ids(|cursor| db.writing_segments_page(cursor, 31).unwrap(), 31);
        db.conn
            .execute("UPDATE segment SET deleting = 1", [])
            .unwrap();
        let deleting_ids = paged_ids(|cursor| db.deleting_segments_page(cursor, 29).unwrap(), 29);
        assert_eq!(deleting_ids, writing_ids);
    }

    #[test]
    fn id_paged_startup_scans_seek_by_rowid() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("id-pages.db")).unwrap();
        for sql in [
            "SELECT id, path, root FROM segment WHERE id > ?1 ORDER BY id LIMIT ?2",
            "SELECT id, path FROM segment WHERE root IS NULL AND id > ?1 ORDER BY id LIMIT ?2",
        ] {
            let plan = explain(&db.conn, sql);
            assert!(plan.contains("rowid>?"), "{sql}: {plan}");
        }
    }

    #[test]
    fn open_runs_in_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("wal.db")).unwrap();
        let mode: String = db
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn the_v4_built_in_kind_list_names_every_built_in_event_kind() {
        for kind in &EventKind::BUILT_IN {
            let quoted = format!("'{}'", kind.name());
            assert!(
                SCHEMA_V4.contains(&quoted),
                "{quoted} missing from SCHEMA_V4"
            );
        }
        assert_eq!(
            SCHEMA_V4.matches('\'').count(),
            2 * (EventKind::BUILT_IN.len() + 1)
        );
    }

    /// The schema constants in migration order.
    const SCHEMAS: [&str; 10] = [
        SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8,
        SCHEMA_V9, SCHEMA_V10,
    ];

    /// Creates a database at schema `version` holding a camera, a sample
    /// entry, a sealed and an open segment, and a built-in and a vendor event
    /// with data, inserted as that version stored them.
    fn database_at_version(path: &Path, version: usize) {
        let conn = Connection::open(path).unwrap();
        for schema in &SCHEMAS[..version] {
            conn.execute_batch(schema).unwrap();
        }
        if version >= 9 {
            conn.execute(
                "INSERT INTO recording_root_state(singleton, pre_identity_catalog) VALUES (1, 0)",
                [],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![i64::try_from(version).unwrap()],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO camera (id, name, vendor) VALUES ('cam1', 'front', NULL); \
             INSERT INTO sample_entry (id, hash, codecs, width, height, config) \
               VALUES (1, 7, 'hvc1.1.6.L120.B0', 640, 480, x'0102'); \
             INSERT INTO segment (camera_id, kind, path, run_seq, discontinuity, \
               wall_start_us, wall_end_us, media_start, media_end, init_len, durable_len, \
               fragments, bytes, sealed, sample_entry_id) VALUES \
               ('cam1', 'main', '/video/cam1/main/a.mp4', 1, 1, 1000000, 2000000, 0, 90000, \
                700, 12345, 3, 12345, 1, 1), \
               ('cam1', 'main', '/video/cam1/main/b.mp4', 1, 0, 3000000, 3000000, 90000, \
                90000, 700, 700, 0, 700, 0, 1);",
        )
        .unwrap();
        let storage_bytes = "length(CAST(?1 AS BLOB)) + length(CAST(?2 AS BLOB)) \
                             + length(CAST(?3 AS BLOB)) + length(CAST(COALESCE(?4, '') AS BLOB)) \
                             + length(CAST(?5 AS BLOB)) + length(CAST(COALESCE(?7, '') AS BLOB))";
        for (kind, kind_other, data) in [
            ("motion", None, Some("{\"region\":[0]}")),
            if version >= 4 {
                ("other", Some("Vendor/温度"), None)
            } else {
                ("Vendor/温度", None, None)
            },
        ] {
            let (columns, values) = match version {
                0..=3 => (
                    "camera_id, source, kind, state, nvr_time_us, data",
                    "?1, ?2, ?3, ?5, ?6, ?7".to_owned(),
                ),
                4..=5 => (
                    "camera_id, source, kind, kind_other, state, nvr_time_us, data",
                    "?1, ?2, ?3, ?4, ?5, ?6, ?7".to_owned(),
                ),
                _ => (
                    "camera_id, source, kind, kind_other, state, nvr_time_us, data, storage_bytes",
                    format!("?1, ?2, ?3, ?4, ?5, ?6, ?7, {storage_bytes}"),
                ),
            };
            conn.execute(
                &format!("INSERT INTO event ({columns}) VALUES ({values})"),
                params![
                    "cam1",
                    "dahua_cgi",
                    kind,
                    kind_other,
                    "pulse",
                    1_500_000,
                    data
                ],
            )
            .unwrap();
        }
    }

    #[test]
    fn every_starting_version_migrates_to_the_same_schema_and_accounting() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = Db::open(&dir.path().join("fresh.db")).unwrap();
        let expected_schema = schema_rows(&fresh.conn);
        drop(fresh);
        for version in 1..=9 {
            let path = dir.path().join(format!("v{version}.db"));
            database_at_version(&path, version);
            let mut db = Db::open(&path).unwrap();
            assert_eq!(
                schema_rows(&db.conn),
                expected_schema,
                "schema after migrating from version {version}"
            );
            assert_eq!(
                stored_schema_version(&db.conn).unwrap(),
                SCHEMA_VERSION,
                "from version {version}"
            );
            let events = db
                .events(
                    None,
                    TimeRange {
                        from: WallUs::new(0),
                        to: WallUs::new(i64::MAX),
                    },
                    None,
                    10,
                )
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .map(|row| row.event.kind.clone())
                    .collect::<Vec<_>>(),
                [EventKind::Motion, EventKind::Other("Vendor/温度".into())],
                "events after migrating from version {version}"
            );
            let (rows, bytes): (i64, i64) = db
                .conn
                .query_row(EVENT_STATE_SQL, [], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap();
            let expected_bytes: usize = events
                .iter()
                .map(|row| event_storage_bytes(&row.event))
                .sum();
            assert_eq!(
                (rows, bytes),
                (2, i64::try_from(expected_bytes).unwrap()),
                "event accounting after migrating from version {version}"
            );
            let (key, range) = stream_range();
            let segments = db.segments(&key, range).unwrap();
            assert_eq!(
                segments.len(),
                1,
                "the open row has no fragment: {segments:?}"
            );
            assert!(segments[0].sealed);
            assert_eq!(segments[0].codecs, "hvc1.1.6.L120.B0");
            assert_eq!(db.writing_segments().unwrap().len(), 1);
        }
    }

    #[test]
    fn configure_video_roots_assigns_the_deepest_root_and_keeps_assigned_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("roots.db")).unwrap();
        for path in [
            "/video/cam1/main/a.mp4",
            "/video/nested/cam1/main/b.mp4",
            "/video2/cam1/main/c.mp4",
            "/elsewhere/d.mp4",
        ] {
            db.adopt_segment(&segment(PathBuf::from(path))).unwrap();
        }
        db.configure_video_roots(&[
            PathBuf::from("/video"),
            PathBuf::from("/video/nested"),
            PathBuf::from("/video"),
        ])
        .unwrap();
        let mut root_of = |path: &str| {
            let id = db.segment_by_path(Path::new(path)).unwrap().unwrap().id;
            db.segment_root(id).unwrap()
        };
        assert_eq!(
            root_of("/video/cam1/main/a.mp4"),
            Some(PathBuf::from("/video"))
        );
        assert_eq!(
            root_of("/video/nested/cam1/main/b.mp4"),
            Some(PathBuf::from("/video/nested")),
            "the deepest root wins"
        );
        assert_eq!(
            root_of("/video2/cam1/main/c.mp4"),
            None,
            "a textual prefix is not a parent directory"
        );
        assert_eq!(root_of("/elsewhere/d.mp4"), None);

        db.configure_video_roots(&[PathBuf::from("/video/cam1")])
            .unwrap();
        let id = db
            .segment_by_path(Path::new("/video/cam1/main/a.mp4"))
            .unwrap()
            .unwrap()
            .id;
        assert_eq!(
            db.segment_root(id).unwrap(),
            Some(PathBuf::from("/video")),
            "a non-null root is never rewritten"
        );
    }

    #[test]
    fn root_bound_marks_refuse_other_roots_and_honour_protection() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("root-marks.db")).unwrap();
        db.configure_video_roots(&[PathBuf::from("/video")])
            .unwrap();
        let id = db
            .adopt_segment(&segment(PathBuf::from("/video/cam1/main/a.mp4")))
            .unwrap();
        let now = WallUs::new(10);
        assert_eq!(
            db.mark_deleting_if_unprotected_at_root(id, Path::new("/other"), now, 0)
                .unwrap(),
            DeleteMarkResult::NotFound,
            "another root never marks the row"
        );
        let event_id = db
            .insert_event(&Event {
                camera: CameraId::try_new("cam1").unwrap(),
                source: EventSource::Manual,
                kind: EventKind::Motion,
                state: EventState::Pulse,
                nvr_time: WallUs::new(1_500_000),
                camera_time: None,
                data: None,
            })
            .unwrap();
        db.retain_event(event_id, Some(WallUs::new(i64::MAX)))
            .unwrap();
        assert_eq!(
            db.mark_deleting_if_unprotected_at_root(id, Path::new("/video"), now, 0)
                .unwrap(),
            DeleteMarkResult::Protected
        );
        db.retain_event(event_id, None).unwrap();
        assert_eq!(
            db.mark_deleting_if_unprotected_at_root(id, Path::new("/video"), now, 0)
                .unwrap(),
            DeleteMarkResult::Marked
        );
        assert_eq!(
            db.mark_deleting_if_unprotected_at_root(id, Path::new("/video"), now, 0)
                .unwrap(),
            DeleteMarkResult::NotFound,
            "a marked row is not marked twice"
        );
    }

    #[test]
    fn a_version_two_database_gains_durable_deletion_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v2.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute("INSERT INTO schema_version (version) VALUES (2)", [])
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let deleting: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('segment') WHERE name = 'deleting'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(deleting, 1);
    }

    #[test]
    fn a_version_three_database_migrates_legacy_other_kinds_without_guessing_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v3.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute_batch(SCHEMA_V3).unwrap();
            conn.execute("INSERT INTO schema_version (version) VALUES (3)", [])
                .unwrap();
            conn.execute(
                "INSERT INTO event (camera_id, source, kind, state, nvr_time_us) \
                 VALUES ('cam1', 'dahua_cgi', 'Vendor/温度', 'pulse', 1), \
                        ('cam1', 'dahua_cgi', 'motion', 'pulse', 2)",
                [],
            )
            .unwrap();
        }
        let mut db = Db::open(&path).unwrap();
        let rows = db
            .events(
                None,
                TimeRange {
                    from: WallUs::new(0),
                    to: WallUs::new(3),
                },
                None,
                10,
            )
            .unwrap();
        assert_eq!(rows[0].event.kind, EventKind::Other("Vendor/温度".into()));
        assert_eq!(rows[1].event.kind, EventKind::Motion);
    }

    #[test]
    fn a_version_six_database_gains_the_camera_horizon_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v6.db");
        {
            let conn = Connection::open(&path).unwrap();
            for schema in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
            ] {
                conn.execute_batch(schema).unwrap();
            }
            conn.execute("INSERT INTO schema_version (version) VALUES (6)", [])
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let indexes: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'segment_camera_horizon'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 1);
    }

    #[test]
    fn a_version_seven_database_gains_explicit_cursor_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v7.db");
        {
            let conn = Connection::open(&path).unwrap();
            for schema in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
            ] {
                conn.execute_batch(schema).unwrap();
            }
            conn.execute("INSERT INTO schema_version (version) VALUES (7)", [])
                .unwrap();
            conn.execute_batch(
                "INSERT INTO sample_entry(id, hash, codecs, width, height, config) \
                   VALUES (1, 1, 'hvc1.test', 1, 1, X'00'); \
                 INSERT INTO segment( \
                   id, camera_id, kind, path, root, run_seq, discontinuity, wall_start_us, \
                   wall_end_us, media_start, media_end, init_len, durable_len, fragments, bytes, \
                   sealed, deleting, sample_entry_id \
                 ) VALUES \
                   (1, 'cam1', 'main', '/video/writing.mp4', '/video', 1, 0, 1, 2, 0, 1, \
                    1, 1, 1, 1, 0, 0, 1), \
                   (2, 'cam1', 'main', '/video/deleting.mp4', '/video', 1, 0, 2, 3, 0, 1, \
                    1, 1, 1, 1, 1, 1, 1);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let index_columns = |name: &str| -> Vec<String> {
            db.conn
                .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
                .unwrap()
                .query_map(params![name], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        for (name, expected) in [
            (
                "segment_stream_time",
                &["camera_id", "kind", "wall_start_us", "id"][..],
            ),
            ("segment_retention_time", &["wall_start_us", "id"][..]),
            (
                "segment_retention_root_time",
                &["root", "wall_start_us", "id"][..],
            ),
            (
                "segment_retention_stream_time",
                &["camera_id", "kind", "wall_start_us", "id"][..],
            ),
            ("event_camera_time", &["camera_id", "nvr_time_us", "id"][..]),
            ("event_time", &["nvr_time_us", "id"][..]),
            ("event_inactive_time", &["nvr_time_us", "id"][..]),
            ("event_retain_until", &["retain_until_us", "id"][..]),
            (
                "event_active_camera_deadline_time",
                &["camera_id", "retain_until_us", "nvr_time_us", "id"][..],
            ),
        ] {
            assert_eq!(index_columns(name), expected, "{name}");
        }
        let version: i64 = db
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn invalid_sealed_blocks_version_eight_migration_before_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid-v7.db");
        {
            let conn = Connection::open(&path).unwrap();
            for schema in [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
            ] {
                conn.execute_batch(schema).unwrap();
            }
            conn.execute("INSERT INTO schema_version (version) VALUES (7)", [])
                .unwrap();
            conn.execute_batch(
                "INSERT INTO sample_entry(id, hash, codecs, width, height, config) \
                   VALUES (1, 1, 'hvc1.test', 1, 1, X'00'); \
                 INSERT INTO segment( \
                   camera_id, kind, path, root, run_seq, discontinuity, wall_start_us, \
                   wall_end_us, media_start, media_end, init_len, durable_len, fragments, bytes, \
                   sealed, deleting, sample_entry_id \
                 ) VALUES ('cam1', 'main', '/video/bad.mp4', '/video', 1, 0, 1, 2, 0, 1, \
                           1, 1, 1, 1, 0, 0, 1);",
            )
            .unwrap();
            conn.execute(
                "UPDATE segment SET sealed = ?1",
                params![rusqlite::types::Value::Real(0.5)],
            )
            .unwrap();
        }
        assert!(matches!(Db::open(&path), Err(CatalogError::Corrupt(_))));
        assert!(!wal_path(&path).exists());
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 7);
        let v8_indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'segment_retention_stream_time'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v8_indexes, 0);
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }

    #[test]
    fn a_schema_above_the_supported_version_is_rejected_before_persistent_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL);")
                .unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION + 1],
            )
            .unwrap();
            let mode: String = conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "delete");
        }

        assert!(matches!(
            Db::open(&path),
            Err(CatalogError::UnsupportedSchema { found, supported })
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
        ));
        assert!(!wal_path(&path).exists());
        let conn = Connection::open(&path).unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
    }

    #[test]
    fn temporary_query_state_never_requires_an_ambient_directory() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("memory-temp.db")).unwrap();
        let temp_store: i64 = db
            .conn
            .pragma_query_value(None, "temp_store", |row| row.get(0))
            .unwrap();
        assert_eq!(temp_store, 2, "SQLite temp_store must be MEMORY");

        // Exercise a materialized retention page and ordered event lifecycle
        // scan: these are the production paths that otherwise make SQLite
        // create etilqs files once their temporary b-trees grow.
        seed_inactive_events(&db.conn, 50_000, "cam1", 0);
        let mut db = db;
        db.maintain_events(WallUs::new(0)).unwrap();
        let page = db
            .retention_candidates(None, None, WallUs::new(0), 0, 16)
            .unwrap();
        assert_eq!(page.rows.len(), 0);
    }

    #[test]
    fn a_complete_schema_above_the_supported_version_is_rejected_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future-complete.db");
        {
            let db = Db::open(&path).unwrap();
            drop(db);
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE schema_version SET version = ?1",
                params![SCHEMA_VERSION + 7],
            )
            .unwrap();
            conn.execute_batch(
                "CREATE TABLE future_state (payload TEXT NOT NULL); \
                 INSERT INTO future_state VALUES ('sentinel');",
            )
            .unwrap();
        }
        assert!(matches!(
            Db::open(&path),
            Err(CatalogError::UnsupportedSchema { found, supported })
                if found == SCHEMA_VERSION + 7 && supported == SCHEMA_VERSION
        ));
        let conn = Connection::open(&path).unwrap();
        let sentinel: String = conn
            .query_row("SELECT payload FROM future_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sentinel, "sentinel");
    }

    #[test]
    fn ambiguous_schema_version_state_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambiguous.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL); \
             INSERT INTO schema_version VALUES (3), (4);",
        )
        .unwrap();
        drop(conn);
        assert!(matches!(Db::open(&path), Err(CatalogError::Corrupt(_))));
    }

    #[test]
    fn event_and_retention_query_plans_use_ordering_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("plans.db")).unwrap();

        // The production statements themselves, so a changed predicate or a
        // dropped tie key fails here.
        for (sql, index) in [
            (event_query_sql(true, false), "event_camera_time"),
            (event_query_sql(true, true), "event_camera_time"),
            (event_query_sql(false, false), "event_time"),
            (event_query_sql(false, true), "event_time"),
        ] {
            let plan = explain(&db.conn, &sql);
            assert!(plan.contains(index), "{sql}: {plan}");
            assert!(!plan.contains("TEMP B-TREE"), "{sql}: {plan}");
            if sql.contains("UNION ALL") {
                assert!(plan.contains("MERGE (UNION ALL)"), "{sql}: {plan}");
            }
        }

        for (sql, index) in [
            (
                retention_candidate_query_sql(false, false),
                "segment_retention_time",
            ),
            (
                retention_candidate_query_sql(false, true),
                "segment_retention_time",
            ),
            (
                retention_candidate_query_sql(true, false),
                "segment_retention_root_time",
            ),
            (
                retention_candidate_query_sql(true, true),
                "segment_retention_root_time",
            ),
            (
                expired_candidate_query_sql(false),
                "segment_retention_stream_time",
            ),
            (
                expired_candidate_query_sql(true),
                "segment_retention_stream_time",
            ),
        ] {
            let plan = explain(&db.conn, &sql);
            assert!(plan.contains(index), "{sql}: {plan}");
            assert!(
                plan.contains("event_active_camera_deadline_time"),
                "protection seeks the deadline index: {plan}"
            );
        }

        let camera_horizon = explain(&db.conn, EVENT_HORIZON_SQL);
        assert!(
            camera_horizon.contains("segment_camera_horizon"),
            "{camera_horizon}"
        );
    }

    #[test]
    fn protection_query_plans_use_deadline_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("protection-plans.db")).unwrap();
        for sql in [mark_sql(false), mark_sql(true)] {
            let atomic_mark = explain(&db.conn, &sql);
            assert!(
                atomic_mark.contains("event_active_camera_deadline_time"),
                "{sql}: {atomic_mark}"
            );
        }
    }

    #[test]
    fn event_cursor_query_stays_bounded_at_scale() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("event-scale.db")).unwrap();
        db.conn
            .execute_batch(
                "WITH RECURSIVE seq(value) AS ( \
                   VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < 50000 \
                 ) INSERT INTO event( \
                   camera_id, source, kind, state, nvr_time_us, storage_bytes \
                 ) SELECT CASE value % 2 WHEN 0 THEN 'cam1' ELSE 'cam2' END, \
                          'manual', 'motion', 'pulse', value, 21 FROM seq;",
            )
            .unwrap();
        let range = TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(50_001),
        };
        let after = EventCursor {
            time: WallUs::new(49_900),
            id: EventId(49_900),
        };
        let query_steps = |camera: Option<&str>| {
            let sql = event_query_sql(camera.is_some(), true);
            let mut stmt = db.conn.prepare(&sql).unwrap();
            let count = {
                let mut rows = stmt
                    .query(params![
                        camera,
                        event_query_lower_bound(range.from, Some(after)),
                        range.to.get(),
                        after.time.get(),
                        after.id.0,
                        32,
                    ])
                    .unwrap();
                let mut count = 0;
                while rows.next().unwrap().is_some() {
                    count += 1;
                }
                count
            };
            assert_eq!(count, 32);
            stmt.get_status(rusqlite::StatementStatus::VmStep)
        };
        for (camera, label) in [(Some("cam1"), "camera"), (None, "global")] {
            let steps = query_steps(camera);
            assert!(steps < 10_000, "{label} cursor page used {steps} VM steps");
        }
        let page = db
            .events(
                Some(&CameraId::try_new("cam1").unwrap()),
                range,
                Some(after),
                32,
            )
            .unwrap();
        assert_eq!(page.len(), 32);
        assert!(page.windows(2).all(|rows| {
            (rows[0].event.nvr_time, rows[0].id) < (rows[1].event.nvr_time, rows[1].id)
        }));
        let global = db.events(None, range, Some(after), 32).unwrap();
        assert_eq!(global.len(), 32);
        assert_eq!(global[0].id, EventId(49_901));
    }

    fn tied_event_continuation_steps(count: i64) -> (i32, i32, Vec<EventId>) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("event-ties.db")).unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(value) AS ( \
                   VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < ?1 \
                 ) INSERT INTO event( \
                   id, camera_id, source, kind, state, nvr_time_us, storage_bytes \
                 ) SELECT value, 'cam1', 'manual', 'motion', 'pulse', 100, 0 FROM seq",
                params![count],
            )
            .unwrap();
        let range = TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(200),
        };
        let after = EventCursor {
            time: WallUs::new(100),
            id: EventId(count / 2),
        };
        let camera_sql = event_query_sql(true, true);
        reset_statement_steps(&db.conn, &camera_sql);
        let camera = db
            .events(
                Some(&CameraId::try_new("cam1").unwrap()),
                range,
                Some(after),
                32,
            )
            .unwrap();
        let camera_steps = statement_steps(&db.conn, &camera_sql);
        let global_sql = event_query_sql(false, true);
        reset_statement_steps(&db.conn, &global_sql);
        let global = db.events(None, range, Some(after), 32).unwrap();
        let global_steps = statement_steps(&db.conn, &global_sql);
        let ids: Vec<EventId> = global.iter().map(|row| row.id).collect();
        assert_eq!(camera.iter().map(|row| row.id).collect::<Vec<_>>(), ids);
        assert_eq!(ids.first(), Some(&EventId(count / 2 + 1)));
        assert_eq!(ids.last(), Some(&EventId(count / 2 + 32)));
        (camera_steps, global_steps, ids)
    }

    #[test]
    fn tied_event_continuations_seek_to_the_midpoint_at_scale() {
        let (small_camera, small_global, small_ids) = tied_event_continuation_steps(1_000);
        let (large_camera, large_global, large_ids) = tied_event_continuation_steps(50_000);
        for (label, small, large) in [
            ("camera", small_camera, large_camera),
            ("global", small_global, large_global),
        ] {
            assert!(small < 5_000, "small {label} page used {small} VM steps");
            assert!(large < 5_000, "large {label} page used {large} VM steps");
            assert!(
                large <= small + 256,
                "{label} page scaled from {small} to {large} VM steps"
            );
        }
        assert_eq!(small_ids.len(), 32);
        assert_eq!(large_ids.len(), 32);
    }

    fn tied_segment_continuation_steps(count: i64) -> ([i32; 3], Vec<SegmentId>) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("segment-ties.db")).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO sample_entry(id, hash, codecs, width, height, config) \
                   VALUES (1, 1, 'hvc1.test', 1, 1, X'00');",
            )
            .unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(value) AS ( \
                   VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < ?1 \
                 ) INSERT INTO segment( \
                   id, camera_id, kind, path, root, run_seq, discontinuity, wall_start_us, \
                   wall_end_us, media_start, media_end, init_len, durable_len, fragments, bytes, \
                   sealed, deleting, sample_entry_id \
                 ) SELECT value, 'cam1', 'main', '/video/' || value || '.mp4', '/video', 1, 0, \
                          100, 101, 0, 1, 1, 1, 1, 1, 1, 0, 1 FROM seq",
                params![count],
            )
            .unwrap();
        let after = Some(cursor(100, count / 2));
        let global_sql = retention_candidate_query_sql(false, true);
        reset_statement_steps(&db.conn, &global_sql);
        let global = db
            .retention_candidates(None, after, WallUs::new(1), 1, 16)
            .unwrap();
        let global_steps = statement_steps(&db.conn, &global_sql);
        let root_sql = retention_candidate_query_sql(true, true);
        reset_statement_steps(&db.conn, &root_sql);
        let root = db
            .retention_candidates(Some(Path::new("/video")), after, WallUs::new(1), 1, 16)
            .unwrap();
        let root_steps = statement_steps(&db.conn, &root_sql);
        let age_sql = expired_candidate_query_sql(true);
        reset_statement_steps(&db.conn, &age_sql);
        let age = db
            .expired_candidates(
                &StreamKey {
                    camera: CameraId::try_new("cam1").unwrap(),
                    kind: StreamKind::Main,
                },
                WallUs::new(200),
                after,
                WallUs::new(1),
                1,
                16,
            )
            .unwrap();
        let age_steps = statement_steps(&db.conn, &age_sql);
        let ids: Vec<SegmentId> = global.rows.iter().map(|row| row.id).collect();
        assert_eq!(root.rows.iter().map(|row| row.id).collect::<Vec<_>>(), ids);
        assert_eq!(age.rows.iter().map(|row| row.id).collect::<Vec<_>>(), ids);
        assert_eq!(ids.first(), Some(&SegmentId(count / 2 + 1)));
        assert_eq!(ids.last(), Some(&SegmentId(count / 2 + 16)));
        ([global_steps, root_steps, age_steps], ids)
    }

    #[test]
    fn tied_segment_continuations_seek_to_the_midpoint_at_scale() {
        let (small_steps, small_ids) = tied_segment_continuation_steps(1_000);
        let (large_steps, large_ids) = tied_segment_continuation_steps(50_000);
        for (label, small, large) in ["global", "root", "age"]
            .into_iter()
            .zip(small_steps)
            .zip(large_steps)
            .map(|((label, small), large)| (label, small, large))
        {
            assert!(small < 10_000, "small {label} page used {small} VM steps");
            assert!(large < 10_000, "large {label} page used {large} VM steps");
            assert!(
                large <= small + 256,
                "{label} page scaled from {small} to {large} VM steps"
            );
        }
        assert_eq!(small_ids.len(), 16);
        assert_eq!(large_ids.len(), 16);
    }

    fn zero_match_maintenance_steps(count: i64) -> (i32, EventCursor) {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("maintenance-scale.db")).unwrap();
        seed_inactive_events(&db.conn, count, "cam1", 0);
        for sql in [
            CLEAR_EXPIRED_MARKS_SQL,
            inactive_event_query_sql(false),
            EVENT_HORIZON_SQL,
            EVENT_STATE_SQL,
        ] {
            reset_statement_steps(&db.conn, sql);
        }
        let tx = write_transaction(&mut db.conn).unwrap();
        let cursor = maintain_events_tx(
            &tx,
            WallUs::new(0),
            true,
            EventLimits {
                rows: MAX_EVENT_ROWS,
                bytes: MAX_EVENT_BYTES,
                prune_batch: 64,
            },
            None,
        )
        .unwrap()
        .expect("nonempty maintenance page");
        tx.commit().unwrap();
        let steps = [
            CLEAR_EXPIRED_MARKS_SQL,
            inactive_event_query_sql(false),
            EVENT_HORIZON_SQL,
            EVENT_STATE_SQL,
        ]
        .into_iter()
        .map(|sql| statement_steps(&db.conn, sql))
        .sum();
        assert_eq!(
            db.events(
                None,
                TimeRange {
                    from: WallUs::new(i64::MIN),
                    to: WallUs::new(i64::MAX),
                },
                None,
                10_000
            )
            .unwrap()
            .len(),
            count.min(10_000) as usize
        );
        (steps, cursor)
    }

    #[test]
    fn zero_match_event_maintenance_has_fixed_work_and_advances() {
        let (small_steps, small_cursor) = zero_match_maintenance_steps(1_000);
        let (large_steps, large_cursor) = zero_match_maintenance_steps(100_000);
        assert_eq!(small_cursor, large_cursor);
        assert_eq!(small_cursor.id, EventId(64));
        assert!(
            small_steps < 10_000,
            "small page used {small_steps} VM steps"
        );
        assert!(
            large_steps < 10_000,
            "large page used {large_steps} VM steps"
        );
        assert_eq!(small_steps, large_steps);
    }

    #[test]
    fn event_maintenance_reaches_a_later_camera_after_zero_match_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("maintenance-progress.db")).unwrap();
        seed_inactive_events(&db.conn, 24, "cam-a", 0);
        seed_inactive_events(&db.conn, 1, "cam-b", 24);
        let mut protected_camera_segment = segment(PathBuf::from("/video/cam-b/main/one.mp4"));
        protected_camera_segment.key.camera = CameraId::try_new("cam-b").unwrap();
        protected_camera_segment.wall_start = WallUs::new(100_000_000);
        protected_camera_segment.wall_end = WallUs::new(101_000_000);
        db.adopt_segment(&protected_camera_segment).unwrap();

        let mut cursor = None;
        for pass in 0..4 {
            let tx = write_transaction(&mut db.conn).unwrap();
            cursor = maintain_events_tx(
                &tx,
                WallUs::new(0),
                true,
                EventLimits {
                    rows: MAX_EVENT_ROWS,
                    bytes: MAX_EVENT_BYTES,
                    prune_batch: 8,
                },
                cursor,
            )
            .unwrap();
            tx.commit().unwrap();
            if pass == 0 {
                assert_eq!(cursor.expect("scan progress").id, EventId(8));
            }
        }
        let remaining_a: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM event WHERE camera_id = 'cam-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let remaining_b: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM event WHERE camera_id = 'cam-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining_a, 24);
        assert_eq!(remaining_b, 0);
    }

    fn inactive_protection_history_steps(count: i64, retain_until: Option<i64>) -> i32 {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("protection-history.db")).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO sample_entry(id, hash, codecs, width, height, config) \
                   VALUES (1, 1, 'hvc1.test', 1, 1, X'00'); \
                 INSERT INTO segment( \
                   id, camera_id, kind, path, root, run_seq, discontinuity, wall_start_us, \
                   wall_end_us, media_start, media_end, init_len, durable_len, fragments, bytes, \
                   sealed, deleting, sample_entry_id \
                 ) VALUES (1, 'cam1', 'main', '/video/one.mp4', '/video', 1, 0, 100, 200, \
                           0, 1, 1, 1, 1, 1, 1, 0, 1);",
            )
            .unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(value) AS ( \
                   VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < ?1 \
                 ) INSERT INTO event( \
                   camera_id, source, kind, state, nvr_time_us, retain_until_us, storage_bytes \
                 ) SELECT 'cam1', 'manual', 'motion', 'pulse', 150, ?2, 0 FROM seq",
                params![count, retain_until],
            )
            .unwrap();
        let sql = retention_candidate_query_sql(false, false);
        reset_statement_steps(&db.conn, &sql);
        let page = db
            .retention_candidates(None, None, WallUs::new(2), 1, 1)
            .unwrap();
        let steps = statement_steps(&db.conn, &sql);
        assert_eq!(page.scanned, 1);
        assert_eq!(page.next, Some(cursor(100, 1)));
        assert_eq!(page.rows.len(), 1);
        assert_eq!(
            db.mark_deleting_if_unprotected(SegmentId(1), WallUs::new(2), 1)
                .unwrap(),
            DeleteMarkResult::Marked
        );
        steps
    }

    #[test]
    fn protection_checks_skip_inactive_and_expired_history_at_scale() {
        for (label, deadline) in [("inactive", None), ("expired", Some(1))] {
            let small = inactive_protection_history_steps(1_000, deadline);
            let large = inactive_protection_history_steps(50_000, deadline);
            assert!(small < 2_000, "small {label} history used {small} VM steps");
            assert!(large < 2_000, "large {label} history used {large} VM steps");
            assert!(
                large <= small + 128,
                "{label} history scaled from {small} to {large} VM steps"
            );
        }
    }

    fn all_newer_age_page_steps(count: i64) -> i32 {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("age-newer.db")).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO sample_entry(id, hash, codecs, width, height, config) \
                   VALUES (1, 1, 'hvc1.test', 1, 1, X'00');",
            )
            .unwrap();
        db.conn
            .execute(
                "WITH RECURSIVE seq(value) AS ( \
                   VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < ?1 \
                 ) INSERT INTO segment( \
                   id, camera_id, kind, path, root, run_seq, discontinuity, wall_start_us, \
                   wall_end_us, media_start, media_end, init_len, durable_len, fragments, bytes, \
                   sealed, deleting, sample_entry_id \
                 ) SELECT value, 'cam1', 'main', '/video/' || value || '.mp4', '/video', 1, 0, \
                          1000 + value, 1001 + value, 0, 1, 1, 1, 1, 1, 1, 0, 1 FROM seq",
                params![count],
            )
            .unwrap();
        let sql = expired_candidate_query_sql(false);
        reset_statement_steps(&db.conn, &sql);
        let page = db
            .expired_candidates(
                &StreamKey {
                    camera: CameraId::try_new("cam1").unwrap(),
                    kind: StreamKind::Main,
                },
                WallUs::new(1_000),
                None,
                WallUs::new(1),
                1,
                16,
            )
            .unwrap();
        let steps = statement_steps(&db.conn, &sql);
        assert_eq!(page.rows, Vec::<SegmentRow>::new());
        assert_eq!(page.scanned, 0);
        assert_eq!(page.next, None);
        steps
    }

    #[test]
    fn all_newer_age_candidates_have_fixed_zero_output_work() {
        let small = all_newer_age_page_steps(1_000);
        let large = all_newer_age_page_steps(50_000);
        assert!(small < 2_000, "small page used {small} VM steps");
        assert!(large < 2_000, "large page used {large} VM steps");
        assert!(
            large <= small + 128,
            "page scaled from {small} to {large} VM steps"
        );
    }

    #[test]
    fn age_pages_advance_past_newer_ending_raw_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("age-progress.db")).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO sample_entry(id, hash, codecs, width, height, config) \
                   VALUES (1, 1, 'hvc1.test', 1, 1, X'00'); \
                 WITH RECURSIVE seq(value) AS ( \
                   VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < 17 \
                 ) INSERT INTO segment( \
                   id, camera_id, kind, path, root, run_seq, discontinuity, wall_start_us, \
                   wall_end_us, media_start, media_end, init_len, durable_len, fragments, bytes, \
                   sealed, deleting, sample_entry_id \
                 ) SELECT value, 'cam1', 'main', '/video/' || value || '.mp4', '/video', 1, 0, \
                          value, CASE WHEN value <= 16 THEN 200 ELSE 18 END, \
                          0, 1, 1, 1, 1, 1, 1, 0, 1 FROM seq;",
            )
            .unwrap();
        let key = StreamKey {
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Main,
        };
        let first = db
            .expired_candidates(&key, WallUs::new(100), None, WallUs::new(1), 1, 16)
            .unwrap();
        assert_eq!(first.rows, Vec::<SegmentRow>::new());
        assert_eq!(first.scanned, 16);
        assert_eq!(first.next, Some(cursor(16, 16)));
        let second = db
            .expired_candidates(&key, WallUs::new(100), first.next, WallUs::new(1), 1, 16)
            .unwrap();
        assert_eq!(second.scanned, 1);
        assert_eq!(second.rows.len(), 1);
        assert_eq!(second.rows[0].id, SegmentId(17));
    }

    #[test]
    fn protected_retention_page_has_fixed_work_and_raw_progress_at_scale() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("retention-scale.db")).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO sample_entry(id, hash, codecs, width, height, config) \
                   VALUES (1, 1, 'hvc1.test', 1, 1, X'00'); \
                 WITH RECURSIVE seq(value) AS ( \
                   VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < 50000 \
                 ) INSERT INTO segment( \
                   id, camera_id, kind, path, root, run_seq, discontinuity, \
                   wall_start_us, wall_end_us, media_start, media_end, init_len, durable_len, \
                   fragments, bytes, sealed, deleting, sample_entry_id \
                 ) SELECT value, 'cam1', 'main', '/video/' || value || '.mp4', '/video', 1, 0, \
                          value, value + 1, 0, 1, 1, 1, 1, 1, 1, 0, 1 FROM seq; \
                 INSERT INTO event( \
                   camera_id, source, kind, state, nvr_time_us, retain_until_us, storage_bytes \
                 ) VALUES ('cam1', 'manual', 'motion', 'pulse', 25000, 1000, 0);",
            )
            .unwrap();
        let first_sql = retention_candidate_query_sql(true, false);
        let bounded_plan = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {first_sql}"))
            .unwrap()
            .query_map(
                params!["/video", None::<i64>, None::<i64>, 1, 100_000, 16],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("; ");
        assert!(
            bounded_plan.contains("segment_retention_root_time"),
            "{bounded_plan}"
        );
        reset_statement_steps(&db.conn, &first_sql);
        let first = db
            .retention_candidates(Some(Path::new("/video")), None, WallUs::new(1), 100_000, 16)
            .unwrap();
        let first_steps = statement_steps(&db.conn, &first_sql);
        assert_eq!(first.rows, Vec::<SegmentRow>::new());
        assert_eq!(first.scanned, 16);
        assert_eq!(first.next, Some(cursor(16, 16)));
        assert!(
            first_steps < 10_000,
            "first page used {first_steps} VM steps"
        );

        let next_sql = retention_candidate_query_sql(true, true);
        reset_statement_steps(&db.conn, &next_sql);
        let second = db
            .retention_candidates(
                Some(Path::new("/video")),
                first.next,
                WallUs::new(1),
                100_000,
                16,
            )
            .unwrap();
        let second_steps = statement_steps(&db.conn, &next_sql);
        assert_eq!(second.rows, Vec::<SegmentRow>::new());
        assert_eq!(second.scanned, 16);
        assert_eq!(second.next, Some(cursor(32, 32)));
        assert!(
            second_steps < 10_000,
            "next page used {second_steps} VM steps"
        );
    }

    #[test]
    fn bounded_event_lifecycle_reuses_database_pages_and_preserves_active_marks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lifecycle.db");
        let mut db = Db::open(&path).unwrap();
        let limits = EventLimits {
            rows: 32,
            bytes: 32 * 1024,
            prune_batch: 64,
        };
        let mut sizes = Vec::new();
        for cycle in 0..8 {
            let tx = write_transaction(&mut db.conn).unwrap();
            for row in 0..64 {
                let value = Event {
                    camera: CameraId::try_new("cam1").unwrap(),
                    source: EventSource::DahuaCgi,
                    kind: EventKind::Other(format!("vendor-{cycle}-{row}")),
                    state: EventState::Pulse,
                    nvr_time: WallUs::new(1_700_000_000_000_000 + cycle * 100 + row),
                    camera_time: None,
                    data: Some("x".repeat(1024)),
                };
                insert_event_row(&tx, &value).unwrap();
            }
            maintain_events_tx(&tx, WallUs::new(1_700_000_100_000_000), false, limits, None)
                .unwrap();
            tx.commit().unwrap();
            let (rows, bytes): (i64, i64) = db
                .conn
                .query_row(
                    "SELECT event_rows, event_bytes FROM catalog_state",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert!(u64::try_from(rows).unwrap() <= limits.rows);
            assert!(u64::try_from(bytes).unwrap() <= limits.bytes);
            db.conn
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            sizes.push(std::fs::metadata(&path).unwrap().len());
            assert_eq!(file_len(&wal_path(&path)), 0);
        }
        let plateau = &sizes[2..];
        assert_eq!(plateau.iter().min(), plateau.iter().max(), "{sizes:?}");

        let active_id = db
            .insert_event(&Event {
                camera: CameraId::try_new("cam1").unwrap(),
                source: EventSource::Manual,
                kind: EventKind::Motion,
                state: EventState::Pulse,
                nvr_time: WallUs::new(1),
                camera_time: None,
                data: Some("retained".into()),
            })
            .unwrap();
        db.retain_event(active_id, Some(WallUs::new(i64::MAX)))
            .unwrap();
        let tx = write_transaction(&mut db.conn).unwrap();
        maintain_events_tx(
            &tx,
            WallUs::new(1_700_000_100_000_000),
            true,
            EventLimits {
                rows: 0,
                bytes: 0,
                prune_batch: 10,
            },
            None,
        )
        .unwrap();
        tx.commit().unwrap();
        let retain_until: Option<i64> = db
            .conn
            .query_row(
                "SELECT retain_until_us FROM event WHERE id = ?1",
                params![active_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retain_until,
            Some(i64::MAX),
            "the active mark survived maintenance"
        );
    }

    #[test]
    fn mixed_event_sizes_cannot_drift_past_the_byte_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("mixed-lifecycle.db")).unwrap();
        let tiny = Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: EventSource::Manual,
            kind: EventKind::Motion,
            state: EventState::Pulse,
            nvr_time: WallUs::new(1),
            camera_time: None,
            data: None,
        };
        let mut large = tiny.clone();
        large.data = Some("x".repeat(1024));
        let large_bytes = u64::try_from(event_storage_bytes(&large)).unwrap();
        let limits = EventLimits {
            rows: 4,
            bytes: large_bytes * 2 - 1,
            prune_batch: 2,
        };
        let active_id = {
            let tx = write_transaction(&mut db.conn).unwrap();
            let mut first = None;
            for at in 1..=4 {
                let mut event = tiny.clone();
                event.nvr_time = WallUs::new(at);
                let id = insert_event_row(&tx, &event).unwrap();
                first.get_or_insert(id);
            }
            tx.commit().unwrap();
            first.unwrap()
        };
        db.conn
            .execute(
                "UPDATE event SET retain_until_us = CASE WHEN id = ?1 THEN ?2 ELSE 5 END",
                params![active_id.0, i64::MAX],
            )
            .unwrap();

        let mut accepted = 0usize;
        let mut rejected = 0usize;
        for at in 100..120 {
            let tx = write_transaction(&mut db.conn).unwrap();
            let mut event = large.clone();
            event.nvr_time = WallUs::new(at);
            let mut remaining = limits.prune_batch;
            if try_insert_event(&tx, &event, WallUs::new(10), limits, &mut remaining)
                .unwrap()
                .is_some()
            {
                accepted += 1;
            } else {
                rejected += 1;
            }
            let (rows, bytes) = event_state(&tx).unwrap();
            assert!(rows <= limits.rows, "rows={rows}");
            assert!(bytes <= limits.bytes, "bytes={bytes}");
            tx.commit().unwrap();
        }
        assert!(accepted > 0, "no large event was admitted");
        assert!(rejected > 0, "capacity pressure never rejected an event");
        let retained: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM event WHERE id = ?1 AND retain_until_us = ?2",
                params![active_id.0, i64::MAX],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 1);
    }

    #[test]
    fn event_capacity_rejection_does_not_roll_back_segment_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("segment.mp4");
        let mut db = Db::open(&dir.path().join("batch-capacity.db")).unwrap();
        let id = db.adopt_segment(&segment(path.clone())).unwrap();
        db.conn
            .execute(
                "UPDATE catalog_state SET event_bytes = ?1 WHERE singleton = 1",
                params![i64::try_from(MAX_EVENT_BYTES).unwrap()],
            )
            .unwrap();
        let outcome = db
            .apply(&[
                Change::Event(Event {
                    camera: CameraId::try_new("cam1").unwrap(),
                    source: EventSource::Manual,
                    kind: EventKind::Motion,
                    state: EventState::Pulse,
                    nvr_time: WallUs::new(1),
                    camera_time: None,
                    data: None,
                }),
                Change::SegmentSealed {
                    path,
                    len: 99,
                    fragments: 4,
                    wall_end: WallUs::new(3_000_000),
                    media_end: 180_000,
                },
            ])
            .unwrap();
        assert_eq!(outcome.rejected_events, 1);
        assert_eq!(outcome.committed_events, Vec::new());
        let row = db.segment(id).unwrap().unwrap();
        assert_eq!(row.bytes, 99);
        assert_eq!(row.fragments, 4);
    }

    #[test]
    fn active_retention_rejects_an_event_over_the_byte_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("retention-limit.db")).unwrap();
        let id = db
            .insert_event(&Event {
                camera: CameraId::try_new("cam1").unwrap(),
                source: EventSource::Manual,
                kind: EventKind::Motion,
                state: EventState::Pulse,
                nvr_time: WallUs::new(1),
                camera_time: None,
                data: None,
            })
            .unwrap();
        db.conn
            .execute(
                "UPDATE event SET storage_bytes = ?1 WHERE id = ?2",
                params![i64::try_from(MAX_RETAINED_EVENT_BYTES + 1).unwrap(), id.0],
            )
            .unwrap();
        assert!(matches!(
            db.retain_event(id, Some(WallUs::new(i64::MAX))),
            Err(CatalogError::EventRetentionLimit)
        ));
    }

    #[test]
    fn deletion_intent_is_durable_and_hidden_from_ordinary_queries() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("catalog.db");
        let media = dir.path().join("recording.mp4");
        let id = {
            let mut db = Db::open(&database).unwrap();
            let id = db.adopt_segment(&segment(media.clone())).unwrap();
            assert_eq!(db.mark_deleting(id).unwrap(), DeleteMarkResult::Marked);
            let synchronous: i64 = db
                .conn
                .pragma_query_value(None, "synchronous", |row| row.get(0))
                .unwrap();
            assert_eq!(synchronous, 1, "ordinary work must return to NORMAL");
            assert_eq!(db.segment(id).unwrap(), None);
            assert_eq!(db.segment_by_path(&media).unwrap(), None);
            assert_eq!(
                db.segments(
                    &segment(media.clone()).key,
                    TimeRange {
                        from: WallUs::new(0),
                        to: WallUs::new(i64::MAX),
                    }
                )
                .unwrap(),
                Vec::<SegmentRow>::new()
            );
            assert_eq!(db.oldest_sealed(10).unwrap(), Vec::<SegmentRow>::new());
            assert_eq!(db.deleting_segments().unwrap().len(), 1);
            assert_eq!(db.total_bytes().unwrap(), 12_345);
            id
        };

        let mut reopened = Db::open(&database).unwrap();
        assert_eq!(reopened.deleting_segments().unwrap()[0].id, id);
        reopened.delete_segment(id).unwrap();
        assert_eq!(
            reopened.deleting_segments().unwrap(),
            Vec::<SegmentRow>::new()
        );
        assert_eq!(reopened.total_bytes().unwrap(), 0);
    }

    #[test]
    fn full_synchronous_scope_is_observable_and_restored_after_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("catalog.db")).unwrap();
        let synchronous = |conn: &Connection| {
            conn.pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(synchronous(&db.conn), 1);

        db.with_full_synchronous(|conn| {
            assert_eq!(synchronous(conn), 2, "the operation must run under FULL");
            Ok(())
        })
        .unwrap();
        assert_eq!(synchronous(&db.conn), 1);

        let result: Result<(), CatalogError> = db.with_full_synchronous(|conn| {
            assert_eq!(
                synchronous(conn),
                2,
                "the failing operation must run under FULL"
            );
            Err(std::io::Error::other("injected before deletion intent commit").into())
        });
        assert!(result.is_err());
        assert_eq!(synchronous(&db.conn), 1, "failure must restore NORMAL");
    }

    #[test]
    fn migration_rereads_the_schema_version_under_the_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("catalog.db")).unwrap();
        // A second opener that read version zero before another process's
        // migration committed must not replay the schema over migrated tables.
        db.migrate(0).unwrap();
        assert_eq!(stored_schema_version(&db.conn).unwrap(), SCHEMA_VERSION);
        let state_rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM recording_root_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state_rows, 1);

        db.conn
            .execute(
                "UPDATE schema_version SET version = ?1",
                params![SCHEMA_VERSION + 1],
            )
            .unwrap();
        assert!(matches!(
            db.migrate(0),
            Err(CatalogError::UnsupportedSchema { found, supported })
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
        ));
    }

    #[test]
    fn open_creates_owner_only_state_whatever_the_umask() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let path = state.join("nested").join("catalog.db");
        let mut db = Db::open(&path).unwrap();
        db.ensure_camera(&CameraId::try_new("cam1").unwrap(), "front", None)
            .unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        for directory in [state.clone(), state.join("nested")] {
            assert_eq!(
                mode(&directory) & 0o077,
                0,
                "{} has mode {:o}",
                directory.display(),
                mode(&directory)
            );
        }
        let shm = PathBuf::from(format!("{}-shm", path.display()));
        for file in [path.clone(), wal_path(&path), shm] {
            assert_eq!(
                mode(&file) & 0o077,
                0,
                "{} has mode {:o}",
                file.display(),
                mode(&file)
            );
        }
    }

    fn open_row(db: &mut Db, path: &Path, fragments: u32) -> SegmentId {
        db.apply(&[
            Change::SegmentOpened {
                key: segment(path.to_path_buf()).key,
                path: path.to_path_buf(),
                run_seq: 1,
                discontinuity: false,
                wall_start: WallUs::new(1_000_000),
                media_start: 0,
                init_len: 700,
                sample_entry: Arc::new(SampleEntry::new(
                    bytes::Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
                    "hvc1".into(),
                    16,
                    16,
                )),
            },
            Change::SegmentProgress {
                path: path.to_path_buf(),
                durable_len: 5_000,
                fragments,
                wall_end: WallUs::new(2_000_000),
                media_end: 90_000,
            },
        ])
        .unwrap();
        db.segment_by_path(path).unwrap().unwrap().id
    }

    #[test]
    fn open_rows_enter_deletion_intent_whatever_their_fragment_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("catalog.db")).unwrap();
        let empty = open_row(&mut db, &dir.path().join("empty.mp4"), 0);
        let recorded = open_row(&mut db, &dir.path().join("recorded.mp4"), 3);
        let sealed = db
            .adopt_segment(&segment(dir.path().join("sealed.mp4")))
            .unwrap();
        for id in [empty, recorded] {
            assert_eq!(
                db.mark_empty_writing_deleting(id).unwrap(),
                DeleteMarkResult::Marked
            );
            assert_eq!(
                db.mark_empty_writing_deleting(id).unwrap(),
                DeleteMarkResult::NotFound,
                "a tombstone is not marked twice"
            );
        }
        assert_eq!(
            db.mark_empty_writing_deleting(sealed).unwrap(),
            DeleteMarkResult::NotFound
        );
        assert_eq!(
            db.mark_empty_writing_deleting(SegmentId(i64::MAX)).unwrap(),
            DeleteMarkResult::NotFound
        );
        let mut deleting: Vec<SegmentId> = db
            .deleting_segments()
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        deleting.sort();
        assert_eq!(deleting, vec![empty, recorded]);
        assert!(db.segment(sealed).unwrap().is_some());
        assert_eq!(db.writing_segments().unwrap(), Vec::<SegmentRow>::new());
    }

    #[test]
    fn stats_count_deletion_intents_and_their_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("catalog.db")).unwrap();
        db.adopt_segment(&segment(dir.path().join("kept.mp4")))
            .unwrap();
        let removed = db
            .adopt_segment(&segment(dir.path().join("removed.mp4")))
            .unwrap();
        let stats = db.stats().unwrap();
        assert_eq!((stats.deleting_segments, stats.deleting_bytes), (0, 0));

        assert_eq!(db.mark_deleting(removed).unwrap(), DeleteMarkResult::Marked);
        let stats = db.stats().unwrap();
        assert_eq!(
            (
                stats.segments,
                stats.deleting_segments,
                stats.deleting_bytes
            ),
            (2, 1, 12_345)
        );
        assert_eq!(
            db.total_bytes().unwrap(),
            2 * 12_345,
            "a tombstone counts toward the quota total until its row is removed"
        );

        db.delete_segment(removed).unwrap();
        let stats = db.stats().unwrap();
        assert_eq!(
            (
                stats.segments,
                stats.deleting_segments,
                stats.deleting_bytes
            ),
            (1, 0, 0)
        );
    }
}
