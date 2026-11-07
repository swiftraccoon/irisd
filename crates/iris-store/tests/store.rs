//! End-to-end tests over a temporary directory.

use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::index::{MAX_PERSISTED_INDEX_RECORDS, read_index, read_index_file_bound_to_media};
use iris_mp4::init::SampleEntry;
use iris_mp4::reader::{ScanOptions, scan};
use iris_store::recover::recover_segment;
use iris_store::{
    NoopObserver, QueueFull, RootExpectation, SegmentOpened, SegmentProgress, SegmentSealed,
    StoragePolicy, StoreObserver, WriterHandle, admit_recording_root, free_space, remove_segment,
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

fn fragment(
    index: u64,
    wall_secs: i64,
    new_run: bool,
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
        new_run,
        sample_entry: Arc::clone(entry),
    }
}

fn policy() -> StoragePolicy {
    StoragePolicy {
        sync_interval: Duration::from_millis(50),
        preallocate_bytes: 1 << 20,
        rotate_bytes: 1 << 30,
        rotate_secs: 3600,
    }
}

fn mp4_files(root: &Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
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
                Err(iris_store::recover::RecoverError::NotARecording(_))
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
        let (_, records) = read_index(&media.with_extension("idx")).unwrap();
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
            .try_send(key(), fragment(i, 1_793_460_958, false, &entry, 64))
            .unwrap();
    }
    writer.shutdown().unwrap();
    writer.shutdown().unwrap(); // second call is a no-op

    let files = mp4_files(dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with("cam1/main/2026-10-31/153558.000000Z.mp4"));
    let report = scan(
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

    let (header, records) = read_index(&files[0].with_extension("idx")).unwrap();
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
            .try_send(key(), fragment(i, 1_793_460_958, false, &entry, 64))
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
    let mut fragment = fragment(index, wall_secs, false, entry, payload);
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
        .try_send(key(), fragment(0, 1_793_460_958, false, &entry, 8 * 1024))
        .unwrap();
    // Over the size limit, but not a sync sample: stays in the open file.
    writer
        .try_send(key(), non_sync_fragment(1, 1_793_460_960, &entry, 8 * 1024))
        .unwrap();
    // The next sync sample rotates.
    writer
        .try_send(key(), fragment(2, 1_793_460_962, false, &entry, 64))
        .unwrap();
    writer.shutdown().unwrap();
    let stats = writer.stats();
    assert_eq!(
        (stats.fragments(), stats.files_opened(), stats.dropped()),
        (3, 2, 0)
    );
    let files = mp4_files(dir.path());
    assert_eq!(files.len(), 2, "{files:?}");
    let (_, first) = read_index(&files[0].with_extension("idx")).unwrap();
    let (_, second) = read_index(&files[1].with_extension("idx")).unwrap();
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
        .try_send(key(), fragment(0, 1_793_460_958, false, &entry, 64))
        .unwrap();
    writer
        .try_send(key(), fragment(1, 1_793_460_958, true, &entry, 64))
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
    let (first, _) = read_index(&first_path.with_extension("idx")).unwrap();
    let (second, records) = read_index(&second_path.with_extension("idx")).unwrap();
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
        rotate_secs: u64::MAX,
    };
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy, 256).unwrap();
    let entry = entry();
    let base = 1_793_460_958_000_000_i64;
    for i in 0..101_u64 {
        let mut next = fragment(i, 1_793_460_958, true, &entry, 16);
        next.wall = WallUs::new(base + i64::try_from(i).unwrap());
        writer.try_send(key(), next).unwrap();
    }
    let other = StreamKey {
        camera: CameraId::try_new("cam2").unwrap(),
        kind: StreamKind::Main,
    };
    let mut other_fragment = fragment(0, 1_793_460_958, false, &entry, 16);
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
            .try_send(key(), fragment(i, 1_793_460_958, false, &entry, 64))
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
    assert!(report.index_rewritten);
    let (_, records) = read_index(&path.with_extension("idx")).unwrap();
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
            .try_send(key(), fragment(i, 1_793_460_958, false, &entry, 64))
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
    let (_, records) = read_index(&path.with_extension("idx")).unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(
        records.iter().map(|r| r.decode_time).collect::<Vec<_>>(),
        [0, 6000, 12000]
    );
    assert_eq!(records[0].raw_rtp, 42);
}

#[test]
fn full_queue_drops_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 1).unwrap();
    let entry = entry();
    // Build first so the send loop is far faster than a 1 MiB write.
    let template = fragment(0, 1_793_460_958, false, &entry, 1 << 20);
    let fragments: Vec<Fragment> = (0..64u64)
        .map(|i| Fragment {
            decode_time: i * 6000,
            ..template.clone()
        })
        .collect();
    let mut failures = 0;
    for f in fragments {
        if writer.try_send(key(), f) == Err(QueueFull) {
            failures += 1;
        }
    }
    assert!(
        failures > 0,
        "sixty-four 1 MiB fragments through a one-slot queue never overflowed"
    );
    assert_eq!(writer.stats().dropped(), failures);
    writer.shutdown().unwrap();
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
        .try_send(key(), fragment(0, 1_793_460_958, false, &sample_entry, 64))
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
    remove_segment(Path::new(&basename)).unwrap();
    assert!(!Path::new(&basename).exists());
    assert!(!Path::new(&basename).with_extension("idx").exists());

    std::env::set_current_dir(original).unwrap();
}

#[derive(Default)]
struct Recorder {
    opened: Mutex<Vec<SegmentOpened>>,
    progress: Mutex<Vec<SegmentProgress>>,
    sealed: Mutex<Vec<SegmentSealed>>,
}

impl StoreObserver for Recorder {
    fn segment_opened(&self, info: SegmentOpened) {
        self.opened.lock().unwrap().push(info);
    }

    fn segment_progress(&self, info: SegmentProgress) {
        self.progress.lock().unwrap().push(info);
    }

    fn segment_sealed(&self, info: SegmentSealed) {
        self.sealed.lock().unwrap().push(info);
    }
}

#[test]
fn observer_sees_open_progress_and_seal() {
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
            .try_send(key(), fragment(i, 100 + i as i64 * 2, i == 0, &e, 2000))
            .unwrap();
        // The writer loop wakes every 500 ms and syncs after the 50 ms interval.
        thread::sleep(Duration::from_millis(700));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while recorder.progress.lock().unwrap().len() < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    writer.shutdown().unwrap();

    let opened = recorder.opened.lock().unwrap();
    assert_eq!(opened.len(), 1);
    let opened = &opened[0];
    assert_eq!(opened.key, key());
    assert_eq!(opened.run_seq, 1);
    assert!(opened.discontinuity);
    assert_eq!(opened.media_start, 0);
    assert_eq!(opened.wall_start, WallUs::new(100 * 1_000_000));
    assert!(opened.init_len > 0);
    assert_eq!(opened.sample_entry.hash, e.hash);
    assert!(opened.path.exists());

    let progress = recorder.progress.lock().unwrap();
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

    let sealed = recorder.sealed.lock().unwrap();
    assert_eq!(sealed.len(), 1);
    let sealed = &sealed[0];
    assert_eq!(sealed.path, opened.path);
    assert_eq!(sealed.fragments, 3);
    assert_eq!(sealed.len, fs::metadata(&sealed.path).unwrap().len());
    assert_eq!(sealed.media_end, 2 * 6000 + 6000);
    // 6000 ticks at 90 kHz is 66 666 µs past the last fragment's wall time.
    assert_eq!(sealed.wall_end, WallUs::new(104 * 1_000_000 + 66_666));
    assert!(progress.last().unwrap().durable_len <= sealed.len);
}

#[test]
fn remove_segment_deletes_both_files_and_reports_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy(), 16).unwrap();
    let e = entry();
    writer
        .try_send(key(), fragment(0, 7, true, &e, 2000))
        .unwrap();
    writer.shutdown().unwrap();
    let mp4 = mp4_files(dir.path()).remove(0);
    let idx = iris_store::layout::index_path(&mp4);
    let expected = fs::metadata(&mp4).unwrap().len() + fs::metadata(&idx).unwrap().len();
    assert_eq!(remove_segment(&mp4).unwrap(), expected);
    assert!(!mp4.exists() && !idx.exists());
    assert_eq!(remove_segment(&mp4).unwrap(), 0);
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
            rotate_secs: u64::MAX,
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
        let report = scan(&mut file, ScanOptions::default()).unwrap();
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
fn recordings_and_their_directories_are_private_whatever_the_umask() {
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
            .try_send(key(), fragment(0, 1_793_460_958, true, &entry(), 64))
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
