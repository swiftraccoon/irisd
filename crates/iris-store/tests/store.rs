//! End-to-end tests over a temporary directory.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::index::{
    MAX_PERSISTED_INDEX_RECORDS, read_index_bounded, read_index_file_bound_to_media,
};
use iris_mp4::init::SampleEntry;
use iris_mp4::reader::{self, ScanOptions, ScanStop};
use iris_store::recover::{RecoverError, recover_segment};
use iris_store::{
    NoopObserver, QueueFull, RootExpectation, SegmentOpened, SegmentProgress, SegmentSealed,
    StoragePolicy, StoreObserver, WriterHandle, admit_recording_root, free_space,
    remove_segment_beneath,
};

fn entry() -> Arc<SampleEntry> {
    Arc::new(SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
        "hvc1.1.6.L150.B0".into(),
        640,
        480,
    ))
}

/// One length-prefixed NAL of `header` type with `body` bytes of filler.
fn nal(header: u8, filler: u8, body: usize) -> Vec<u8> {
    let len = u32::try_from(2 + body).unwrap();
    let mut v = len.to_be_bytes().to_vec();
    v.extend_from_slice(&[header, 1]);
    v.resize(4 + 2 + body, filler);
    v
}

fn key() -> StreamKey {
    StreamKey {
        camera: CameraId::try_new("cam1").unwrap(),
        kind: StreamKind::Main,
    }
}

/// Whether a test fragment starts a run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    New,
    Continues,
}

fn fragment(
    index: u64,
    wall_secs: i64,
    run: Run,
    entry: &Arc<SampleEntry>,
    payload: usize,
) -> Fragment {
    let idr = nal(0x26, 0xaa, payload / 2);
    let p = nal(0x02, 0xbb, payload / 2);
    Fragment {
        samples: vec![
            Sample {
                data: Bytes::from(idr),
                duration: 3000,
                is_sync: true,
            },
            Sample {
                data: Bytes::from(p),
                duration: 3000,
                is_sync: false,
            },
        ],
        decode_time: index * 6000,
        wall: WallUs::new(wall_secs * 1_000_000),
        raw_rtp: 42,
        new_run: run == Run::New,
        sample_entry: Arc::clone(entry),
    }
}

fn policy() -> StoragePolicy {
    StoragePolicy {
        sync_interval: Duration::from_millis(50),
        preallocate_bytes: 1 << 20,
        rotate_bytes: 1 << 30,
        rotate_age: Duration::from_secs(3600),
    }
}

fn mp4_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "mp4") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

#[test]
fn recovery_rejects_incomplete_init_without_changing_media_or_sidecar() {
    let init = iris_mp4::init::build_init_segment(&entry());
    let ftyp = iris_mp4::boxes::BoxHeader::parse(&init)
        .unwrap()
        .size
        .unwrap() as usize;
    let mut oversized = init[..ftyp + 8].to_vec();
    oversized[ftyp..ftyp + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    for bytes in [
        init[..ftyp].to_vec(),
        [init[..ftyp].to_vec(), b"\0\0\0\x08free".to_vec()].concat(),
        init[..ftyp + 12].to_vec(),
        oversized,
    ] {
        for sidecar_exists in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let media = dir.path().join("incomplete.mp4");
            let sidecar = media.with_extension("idx");
            fs::write(&media, &bytes).unwrap();
            if sidecar_exists {
                fs::write(&sidecar, b"preserve-sidecar").unwrap();
            }
            assert!(matches!(
                recover_segment(&media),
                Err(RecoverError::NotARecording(_))
            ));
            assert_eq!(fs::read(&media).unwrap(), bytes);
            if sidecar_exists {
                assert_eq!(fs::read(&sidecar).unwrap(), b"preserve-sidecar");
            } else {
                assert!(!sidecar.exists());
            }
        }
    }
}

#[test]
fn recovery_preserves_complete_init_before_a_missing_or_torn_fragment() {
    let init = iris_mp4::init::build_init_segment(&entry());
    for suffix in [vec![], b"\0\0\0\x20prft".to_vec()] {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("complete-init.mp4");
        let mut bytes = init.to_vec();
        bytes.extend(suffix);
        fs::write(&media, bytes).unwrap();
        let report = recover_segment(&media).unwrap();
        assert_eq!(report.valid_len as usize, init.len());
        assert_eq!(report.fragments, 0);
        assert_eq!(fs::read(&media).unwrap(), init);
        let (_, records) =
            read_index_bounded(&media.with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
        assert_eq!(records, []);
    }
}

#[test]
fn round_trips_three_fragments() {
    let dir = tempfile::tempdir().unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 16).unwrap();
    let entry = entry();
    for i in 0..3 {
        writer
            .try_send(
                key(),
                fragment(i, 1_793_460_958, Run::Continues, &entry, 64),
            )
            .unwrap();
    }
    writer.shutdown().unwrap();
    writer.shutdown().unwrap(); // second call is a no-op

    let files = mp4_files(dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with("cam1/main/2026-10-31/153558.000000Z.mp4"));
    let report = reader::scan(
        File::open(&files[0]).unwrap(),
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    assert_eq!(report.stop, None);
    assert_eq!(report.fragments.len(), 3);
    assert_eq!(
        report.valid_len,
        fs::metadata(&files[0]).unwrap().len(),
        "sealed file has no slack"
    );

    let (header, records) =
        read_index_bounded(&files[0].with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    assert_eq!(header.init_len, report.init_len);
    assert_eq!(header.sample_entry_hash, entry.hash);
    assert_eq!(records.len(), 3);
    for (record, info) in records.iter().zip(&report.fragments) {
        assert_eq!(
            (record.offset, record.len, record.decode_time),
            (info.offset, info.len, info.decode_time)
        );
        assert_eq!(record.raw_rtp, 42);
        assert!(record.starts_with_sync());
    }
    let stats = writer.stats();
    assert_eq!(
        (stats.fragments(), stats.files_opened(), stats.dropped()),
        (3, 1, 0)
    );
}

#[test]
fn counts_queue_depth_and_io_latency() {
    let dir = tempfile::tempdir().unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 16).unwrap();
    let entry = entry();
    for i in 0..3 {
        writer
            .try_send(
                key(),
                fragment(i, 1_793_460_958, Run::Continues, &entry, 64),
            )
            .unwrap();
    }
    writer.shutdown().unwrap();
    let stats = writer.stats();
    assert_eq!(stats.fragments(), 3);
    assert_eq!(stats.queued(), 0);
    assert_eq!(stats.queued_bytes(), 0);
    assert!(stats.queue_peak().get() >= 1);
    assert!(stats.queue_bytes_peak().get() > 0);
    let write_observations = stats.write_latency().snapshot().count();
    let sync_observations = stats.sync_latency().snapshot().count();
    assert_eq!(write_observations, 3);
    assert_eq!(sync_observations, stats.syncs());
    assert!(stats.slow_writes() <= write_observations + sync_observations);
}

/// A fragment whose first sample is not a sync sample, as the assembler emits
/// when a long GOP is split at the fragment cap.
fn non_sync_fragment(
    index: u64,
    wall_secs: i64,
    entry: &Arc<SampleEntry>,
    payload: usize,
) -> Fragment {
    let mut fragment = fragment(index, wall_secs, Run::Continues, entry, payload);
    fragment.samples.remove(0);
    fragment.samples[0].is_sync = false;
    fragment
}

#[test]
fn size_rotation_waits_for_a_sync_sample() {
    let dir = tempfile::tempdir().unwrap();
    let policy = StoragePolicy {
        rotate_bytes: 10 * 1024,
        ..policy()
    };
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy, 16).unwrap();
    let entry = entry();
    writer
        .try_send(
            key(),
            fragment(0, 1_793_460_958, Run::Continues, &entry, 8 * 1024),
        )
        .unwrap();
    // Over the size limit, but not a sync sample: stays in the open file.
    writer
        .try_send(key(), non_sync_fragment(1, 1_793_460_960, &entry, 8 * 1024))
        .unwrap();
    // The next sync sample rotates.
    writer
        .try_send(
            key(),
            fragment(2, 1_793_460_962, Run::Continues, &entry, 64),
        )
        .unwrap();
    writer.shutdown().unwrap();
    let stats = writer.stats();
    assert_eq!(
        (stats.fragments(), stats.files_opened(), stats.dropped()),
        (3, 2, 0)
    );
    let files = mp4_files(dir.path());
    assert_eq!(files.len(), 2, "{files:?}");
    let (_, first) =
        read_index_bounded(&files[0].with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    let (_, second) =
        read_index_bounded(&files[1].with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    assert_eq!((first.len(), second.len()), (2, 1));
    assert!(!first[1].starts_with_sync());
    assert!(second[0].starts_with_sync());
}

#[test]
fn new_run_opens_a_second_file() {
    let dir = tempfile::tempdir().unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 16).unwrap();
    let entry = entry();
    writer
        .try_send(
            key(),
            fragment(0, 1_793_460_958, Run::Continues, &entry, 64),
        )
        .unwrap();
    writer
        .try_send(key(), fragment(1, 1_793_460_958, Run::New, &entry, 64))
        .unwrap();
    writer.shutdown().unwrap();
    let files = mp4_files(dir.path());
    assert_eq!(files.len(), 2, "{files:?}");
    let first_path = files
        .iter()
        .find(|path| path.ends_with("153558.000000Z.mp4"))
        .unwrap();
    let second_path = files
        .iter()
        .find(|path| path.ends_with("153558.000000Z-1.mp4"))
        .unwrap();
    let (first, _) = read_index_bounded(
        &first_path.with_extension("idx"),
        MAX_PERSISTED_INDEX_RECORDS,
    )
    .unwrap();
    let (second, records) = read_index_bounded(
        &second_path.with_extension("idx"),
        MAX_PERSISTED_INDEX_RECORDS,
    )
    .unwrap();
    assert_eq!((first.run_id, second.run_id), (1, 2));
    assert_eq!(records.len(), 1);
    assert_ne!(
        records[0].flags & iris_mp4::index::FLAG_DISCONTINUITY_BEFORE,
        0
    );
}

#[test]
fn more_than_one_hundred_distinct_microsecond_rotations_keep_the_root_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let policy = StoragePolicy {
        sync_interval: Duration::from_secs(3_600),
        preallocate_bytes: 0,
        rotate_bytes: u64::MAX,
        rotate_age: Duration::MAX,
    };
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy, 256).unwrap();
    let entry = entry();
    let base = 1_793_460_958_000_000_i64;
    for i in 0..101_u64 {
        let mut next = fragment(i, 1_793_460_958, Run::New, &entry, 16);
        next.wall = WallUs::new(base + i64::try_from(i).unwrap());
        writer.try_send(key(), next).unwrap();
    }
    let other = StreamKey {
        camera: CameraId::try_new("cam2").unwrap(),
        kind: StreamKind::Main,
    };
    let mut other_fragment = fragment(0, 1_793_460_958, Run::Continues, &entry, 16);
    other_fragment.wall = WallUs::new(base + 500_000);
    writer.try_send(other, other_fragment).unwrap();
    writer.shutdown().unwrap();

    let files = mp4_files(dir.path());
    assert_eq!(files.len(), 102, "{files:?}");
    assert_eq!(writer.stats().fragments(), 102);
    assert_eq!(writer.stats().files_opened(), 102);
    assert_eq!(writer.stats().write_errors(), 0);
    assert_eq!(writer.stats().dropped(), 0);
}

#[test]
fn recovers_a_truncated_file() {
    let dir = tempfile::tempdir().unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 16).unwrap();
    let entry = entry();
    for i in 0..3 {
        writer
            .try_send(
                key(),
                fragment(i, 1_793_460_958, Run::Continues, &entry, 64),
            )
            .unwrap();
    }
    writer.shutdown().unwrap();
    let path = mp4_files(dir.path()).remove(0);
    let len = fs::metadata(&path).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(len - 10)
        .unwrap();

    let report = recover_segment(&path).unwrap();
    assert!(report.truncated);
    assert_eq!(report.fragments, 2);
    assert_eq!(report.original_len, len - 10);
    assert_eq!(fs::metadata(&path).unwrap().len(), report.valid_len);
    let (_, records) =
        read_index_bounded(&path.with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0].raw_rtp, 42,
        "camera timestamps survive from the old index"
    );

    let again = recover_segment(&path).unwrap();
    assert!(!again.truncated);
    assert_eq!(again.fragments, 2);
}

#[test]
fn recovery_keeps_complete_media_after_a_missing_index_append() {
    let dir = tempfile::tempdir().unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 16).unwrap();
    let entry = entry();
    for i in 0..3 {
        writer
            .try_send(
                key(),
                fragment(i, 1_793_460_958, Run::Continues, &entry, 64),
            )
            .unwrap();
    }
    writer.shutdown().unwrap();
    let path = mp4_files(dir.path()).remove(0);
    let media = fs::read(&path).unwrap();
    // A failed sidecar append can leave a partial record after complete media.
    OpenOptions::new()
        .write(true)
        .open(path.with_extension("idx"))
        .unwrap()
        .set_len((iris_mp4::index::HEADER_LEN + iris_mp4::index::RECORD_LEN + 7) as u64)
        .unwrap();
    let report = recover_segment(&path).unwrap();
    assert!(!report.truncated);
    assert_eq!(report.fragments, 3);
    assert_eq!(fs::read(&path).unwrap(), media);
    let (_, records) =
        read_index_bounded(&path.with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(
        records.iter().map(|r| r.decode_time).collect::<Vec<_>>(),
        [0, 6000, 12000]
    );
    assert_eq!(records[0].raw_rtp, 42);
}

/// Holds the writer thread inside `segment_opened` until the test releases it,
/// so the queue fills deterministically.
struct GatedObserver {
    entered: Mutex<Option<mpsc::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl StoreObserver for GatedObserver {
    fn segment_opened(&self, _info: SegmentOpened) {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        let _ = self.release.lock().unwrap().recv();
    }

    fn segment_progress(&self, _info: SegmentProgress) {}

    fn segment_sealed(&self, _info: SegmentSealed) {}
}

/// Writes `count` sync fragments into a fresh root and returns the sealed media path.
fn sealed_recording(dir: &Path, count: u64) -> PathBuf {
    let writer = WriterHandle::spawn(dir.to_path_buf(), policy(), 16).unwrap();
    let entry = entry();
    for i in 0..count {
        writer
            .try_send(
                key(),
                fragment(i, 1_793_460_958, Run::Continues, &entry, 64),
            )
            .unwrap();
    }
    writer.shutdown().unwrap();
    mp4_files(dir).remove(0)
}

#[test]
fn recovery_stops_at_a_zero_filled_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = sealed_recording(dir.path(), 3);
    let sealed_len = fs::metadata(&path).unwrap().len();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&vec![0u8; 64 << 10])
        .unwrap();

    let report = recover_segment(&path).unwrap();
    assert_eq!(report.stop, Some(ScanStop::ZeroFill { offset: sealed_len }));
    assert!(report.truncated);
    assert_eq!(report.fragments, 3);
    assert_eq!(report.valid_len, sealed_len);
    assert_eq!(fs::metadata(&path).unwrap().len(), sealed_len);
    let (_, records) =
        read_index_bounded(&path.with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    assert_eq!(records.len(), 3);
}

#[test]
fn recovery_drops_a_torn_last_fragment() {
    let dir = tempfile::tempdir().unwrap();
    let path = sealed_recording(dir.path(), 3);
    let (_, records) =
        read_index_bounded(&path.with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    let last = records[2];
    // Zero payload bytes just before the last fragment's 24-byte trailer; its CRC no
    // longer matches.
    let torn = OpenOptions::new().write(true).open(&path).unwrap();
    torn.write_all_at(&[0u8; 8], last.offset + u64::from(last.len) - 32)
        .unwrap();
    drop(torn);

    let report = recover_segment(&path).unwrap();
    assert_eq!(
        report.stop,
        Some(ScanStop::CrcMismatch {
            offset: last.offset
        })
    );
    assert_eq!(report.fragments, 2);
    assert_eq!(report.valid_len, last.offset);
    assert_eq!(fs::metadata(&path).unwrap().len(), last.offset);
    let (_, records) =
        read_index_bounded(&path.with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    assert_eq!(records.len(), 2);
}

/// Environment variable naming the root the SIGKILL child writes into.
const SIGKILL_ROOT: &str = "IRIS_STORE_SIGKILL_ROOT";

#[test]
fn a_writer_killed_mid_recording_leaves_a_recoverable_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "sigkill_child_writes_until_killed"])
        .env(SIGKILL_ROOT, dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let path = loop {
        assert!(
            Instant::now() < deadline,
            "the child never wrote a recording"
        );
        if let Some(path) = mp4_files(dir.path()).first()
            && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 8 << 10)
        {
            break path.clone();
        }
        thread::sleep(Duration::from_millis(20));
    };
    child.kill().unwrap();
    child.wait().unwrap();

    let report = recover_segment(&path).unwrap();
    assert!(report.fragments >= 1, "{report:?}");
    assert_eq!(fs::metadata(&path).unwrap().len(), report.valid_len);
    let scanned = reader::scan(
        File::open(&path).unwrap(),
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    assert_eq!(scanned.fragments.len(), report.fragments);
    assert_eq!(scanned.stop, None, "every surviving fragment verifies");
    let (_, records) =
        read_index_bounded(&path.with_extension("idx"), MAX_PERSISTED_INDEX_RECORDS).unwrap();
    assert_eq!(
        records.len(),
        report.fragments,
        "the sidecar describes the scan exactly"
    );
}

#[test]
#[ignore = "runs as the child of the SIGKILL regression"]
fn sigkill_child_writes_until_killed() {
    let Some(root) = std::env::var_os(SIGKILL_ROOT) else {
        return;
    };
    let writer = WriterHandle::spawn(
        PathBuf::from(root),
        StoragePolicy {
            sync_interval: Duration::from_secs(3600),
            preallocate_bytes: 1 << 20,
            rotate_bytes: 1 << 30,
            rotate_age: Duration::from_secs(3600),
        },
        16,
    )
    .unwrap();
    let entry = entry();
    let started = Instant::now();
    let mut index = 0;
    while started.elapsed() < Duration::from_secs(60) {
        let _ = writer.try_send(
            key(),
            fragment(index, 1_793_460_958, Run::Continues, &entry, 512),
        );
        index += 1;
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn full_queue_drops_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let observer = Arc::new(GatedObserver {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    });
    let writer =
        WriterHandle::spawn_with_observer(dir.path().to_path_buf(), policy(), 1, observer).unwrap();
    let entry = entry();
    writer
        .try_send(key(), fragment(0, 1_793_460_958, Run::New, &entry, 64))
        .unwrap();
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the writer thread opens the first file");
    // The thread is parked inside the observer, so exactly one more fragment fits.
    let mut failures = 0;
    for i in 1..64u64 {
        if writer.try_send(
            key(),
            fragment(i, 1_793_460_958, Run::Continues, &entry, 64),
        ) == Err(QueueFull)
        {
            failures += 1;
        }
    }
    assert_eq!(
        failures, 62,
        "one slot takes one fragment; the rest are refused"
    );
    assert_eq!(writer.stats().dropped(), 62);
    release.send(()).unwrap();
    writer.shutdown().unwrap();
    assert_eq!(writer.stats().fragments(), 2);
}

#[test]
fn basename_relative_recovery_and_removal_succeed() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "basename_relative_recovery_and_removal_child",
        ])
        .env("IRIS_STORE_RELATIVE_CHILD", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "runs in an isolated child process from the parent regression"]
fn basename_relative_recovery_and_removal_child() {
    if std::env::var_os("IRIS_STORE_RELATIVE_CHILD").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 8).unwrap();
    let sample_entry = entry();
    writer
        .try_send(
            key(),
            fragment(0, 1_793_460_958, Run::Continues, &sample_entry, 64),
        )
        .unwrap();
    writer.shutdown().unwrap();
    let media = mp4_files(dir.path()).remove(0);
    let parent = media.parent().unwrap().to_path_buf();
    let basename = media.file_name().unwrap().to_owned();
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(&parent).unwrap();

    let report = recover_segment(Path::new(&basename)).unwrap();
    assert_eq!(report.fragments, 1);
    assert!(Path::new(&basename).with_extension("idx").exists());
    fs::remove_file(Path::new(&basename)).unwrap();
    fs::remove_file(Path::new(&basename).with_extension("idx")).unwrap();
    assert!(!Path::new(&basename).exists());
    assert!(!Path::new(&basename).with_extension("idx").exists());

    std::env::set_current_dir(original).unwrap();
}

/// One observer notification, in the order the writer thread delivered it.
#[derive(Debug)]
enum Notification {
    Opened(SegmentOpened),
    Progress(SegmentProgress),
    Sealed(SegmentSealed),
}

#[derive(Default)]
struct Recorder {
    notifications: Mutex<Vec<Notification>>,
}

impl StoreObserver for Recorder {
    fn segment_opened(&self, info: SegmentOpened) {
        self.notifications
            .lock()
            .unwrap()
            .push(Notification::Opened(info));
    }

    fn segment_progress(&self, info: SegmentProgress) {
        self.notifications
            .lock()
            .unwrap()
            .push(Notification::Progress(info));
    }

    fn segment_sealed(&self, info: SegmentSealed) {
        self.notifications
            .lock()
            .unwrap()
            .push(Notification::Sealed(info));
    }
}

fn run_of(index: u64) -> Run {
    if index == 0 { Run::New } else { Run::Continues }
}

#[test]
fn observer_sees_open_progress_and_seal_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let writer = WriterHandle::spawn_with_observer(
        dir.path().to_path_buf(),
        policy(),
        16,
        Arc::clone(&recorder) as Arc<dyn StoreObserver>,
    )
    .unwrap();
    let e = entry();
    for i in 0..3 {
        writer
            .try_send(key(), fragment(i, 100 + i as i64 * 2, run_of(i), &e, 2000))
            .unwrap();
        // The writer loop wakes every 500 ms and syncs after the 50 ms interval.
        thread::sleep(Duration::from_millis(700));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let progress_count = || {
        recorder
            .notifications
            .lock()
            .unwrap()
            .iter()
            .filter(|n| matches!(n, Notification::Progress(_)))
            .count()
    };
    while progress_count() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    writer.shutdown().unwrap();

    let notifications = recorder.notifications.lock().unwrap();
    let Some((Notification::Opened(opened), rest)) = notifications.split_first() else {
        panic!("the first notification is the open: {notifications:?}");
    };
    let Some((Notification::Sealed(sealed), middle)) = rest.split_last() else {
        panic!("the last notification is the seal: {notifications:?}");
    };
    let progress: Vec<&SegmentProgress> = middle
        .iter()
        .map(|n| match n {
            Notification::Progress(progress) => progress,
            other => panic!("only progress sits between open and seal: {other:?}"),
        })
        .collect();

    assert_eq!(opened.key, key());
    assert_eq!(opened.run_seq, 1);
    assert!(opened.discontinuity);
    assert_eq!(opened.media_start, 0);
    assert_eq!(opened.wall_start, WallUs::new(100 * 1_000_000));
    assert_eq!(
        opened.init_len as usize,
        iris_mp4::init::build_init_segment(&e).len(),
        "the catalog serves the initialization section by this length"
    );
    assert_eq!(opened.sample_entry.hash, e.hash);
    assert!(opened.path.exists());

    assert!(
        progress.len() >= 2,
        "expected at least two syncs, saw {}",
        progress.len()
    );
    assert!(progress.iter().all(|p| p.path == opened.path));
    assert!(
        progress
            .windows(2)
            .all(|w| w[0].durable_len <= w[1].durable_len)
    );
    assert!(progress.last().unwrap().durable_len > progress[0].durable_len);
    assert!(progress.last().unwrap().fragments <= 3);

    assert_eq!(sealed.path, opened.path);
    assert_eq!(sealed.fragments, 3);
    assert_eq!(sealed.len, fs::metadata(&sealed.path).unwrap().len());
    assert_eq!(sealed.media_end, 2 * 6000 + 6000);
    // 6000 ticks at 90 kHz is 66 666 µs past the last fragment's wall time.
    assert_eq!(sealed.wall_end, WallUs::new(104 * 1_000_000 + 66_666));
    assert!(progress.last().unwrap().durable_len <= sealed.len);
}

#[test]
fn pair_removal_beneath_the_root_deletes_both_files_and_reports_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let approved = admit_recording_root(dir.path(), RootExpectation::Fresh).unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 16).unwrap();
    let e = entry();
    writer
        .try_send(key(), fragment(0, 7, Run::New, &e, 2000))
        .unwrap();
    writer.shutdown().unwrap();
    let mp4 = mp4_files(dir.path()).remove(0);
    let idx = iris_store::layout::index_path(&mp4);
    let expected = fs::metadata(&mp4).unwrap().len() + fs::metadata(&idx).unwrap().len();
    assert_eq!(remove_segment_beneath(&approved, &mp4).unwrap(), expected);
    assert!(!mp4.exists() && !idx.exists());
    assert_eq!(remove_segment_beneath(&approved, &mp4).unwrap(), 0);
}

#[test]
fn free_space_reports_the_file_system() {
    let dir = tempfile::tempdir().unwrap();
    let space = free_space(dir.path()).unwrap();
    assert!(space.total > 0);
    assert!(space.available <= space.total);
}

#[test]
fn the_writer_rotates_before_the_sidecar_record_limit() {
    let dir = tempfile::tempdir().unwrap();
    let total = MAX_PERSISTED_INDEX_RECORDS + 1;
    let writer = WriterHandle::spawn(
        dir.path().to_path_buf(),
        StoragePolicy {
            sync_interval: Duration::from_secs(3_600),
            preallocate_bytes: 0,
            rotate_bytes: u64::MAX,
            rotate_age: Duration::MAX,
        },
        total,
    )
    .unwrap();
    let sample_entry = entry();
    for index in 0..total {
        let index = u64::try_from(index).unwrap();
        let fragment = Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 4, 0x26, 1, 0xaa, 0xbb]),
                duration: 3_000,
                is_sync: true,
            }],
            decode_time: index * 3_000,
            wall: WallUs::new(1_700_000_000_000_000 + i64::try_from(index).unwrap() * 33_333),
            raw_rtp: 0,
            new_run: index == 0,
            sample_entry: Arc::clone(&sample_entry),
        };
        writer.try_send(key(), fragment).unwrap();
    }
    writer.shutdown().unwrap();
    assert_eq!(writer.stats().dropped(), 0);

    let files = mp4_files(dir.path());
    let mut counts = Vec::new();
    for media in &files {
        let mut file = File::open(media).unwrap();
        let media_len = file.metadata().unwrap().len();
        let report = reader::scan(&mut file, ScanOptions::default()).unwrap();
        assert_eq!(report.stop, None, "{}", media.display());
        let index = File::open(media.with_extension("idx")).unwrap();
        let (_, records) =
            read_index_file_bound_to_media(index, MAX_PERSISTED_INDEX_RECORDS, &report, media_len)
                .unwrap()
                .expect("every sidecar is within the reader limit and bound to its media");
        counts.push(records.len());
    }
    assert_eq!(counts, [MAX_PERSISTED_INDEX_RECORDS - 4_096, 4_097]);
}

#[test]
fn recordings_and_their_directories_are_private() {
    use std::os::unix::fs::PermissionsExt as _;

    for approved in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let writer = if approved {
            let admitted = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
            WriterHandle::spawn_with_approved_roots(
                vec![admitted],
                policy(),
                8,
                0,
                Arc::new(NoopObserver),
            )
            .unwrap()
        } else {
            WriterHandle::spawn(root.clone(), policy(), 8).unwrap()
        };
        writer
            .try_send(key(), fragment(0, 1_793_460_958, Run::New, &entry(), 64))
            .unwrap();
        writer.shutdown().unwrap();
        let media = mp4_files(&root).remove(0);
        let date = media.parent().unwrap();
        let kind = date.parent().unwrap();
        let camera = kind.parent().unwrap();
        for path in [
            media.as_path(),
            media.with_extension("idx").as_path(),
            date,
            kind,
            camera,
            root.as_path(),
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{} has mode {mode:o}", path.display());
        }
    }
}
