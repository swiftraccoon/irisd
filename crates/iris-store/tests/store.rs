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
use iris_mp4::index::read_index;
use iris_mp4::init::SampleEntry;
use iris_mp4::reader::{ScanOptions, scan};
use iris_store::recover::recover_segment;
use iris_store::{
    QueueFull, SegmentOpened, SegmentProgress, SegmentSealed, StoragePolicy, StoreObserver,
    WriterHandle, free_space, remove_segment,
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
    assert!(files[0].ends_with("cam1/main/2026-10-31/153558Z.mp4"));
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
    assert_eq!(stats.write_latency().snapshot().count(), 3);
    assert_eq!(stats.sync_latency().snapshot().count(), stats.syncs());
    assert_eq!(stats.slow_writes(), 0);
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
        .try_send(key(), fragment(1, 1_793_460_960, true, &entry, 64))
        .unwrap();
    writer.shutdown().unwrap();
    let files = mp4_files(dir.path());
    assert_eq!(files.len(), 2, "{files:?}");
    let (first, _) = read_index(&files[0].with_extension("idx")).unwrap();
    let (second, records) = read_index(&files[1].with_extension("idx")).unwrap();
    assert_eq!((first.run_id, second.run_id), (1, 2));
    assert_eq!(records.len(), 1);
    assert_ne!(
        records[0].flags & iris_mp4::index::FLAG_DISCONTINUITY_BEFORE,
        0
    );
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
    assert_eq!(
        remove_segment(&mp4).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn free_space_reports_the_file_system() {
    let dir = tempfile::tempdir().unwrap();
    let space = free_space(dir.path()).unwrap();
    assert!(space.total > 0);
    assert!(space.available <= space.total);
}
