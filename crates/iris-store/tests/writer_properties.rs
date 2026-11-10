//! Seeded property test of the writer: a generated sequence of fragments
//! (sync or continuation starts, new runs) written through many rotations
//! lands on disk in order, every file opens at a sync fragment, a fragment
//! that starts with a sync sample is never dropped, and the counters match
//! what a scan of the files finds.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::index::{MAX_PERSISTED_INDEX_RECORDS, read_index_file_bound_to_media};
use iris_mp4::init::SampleEntry;
use iris_mp4::reader::{self, FragmentInfo, ScanOptions};
use iris_store::{StoragePolicy, WriterHandle};

/// A small deterministic generator (xorshift64*), so a failure reproduces
/// from its seed.
struct Rng(u64);

impl Rng {
    /// A value in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }
}

/// What the generator sent as fragment `index`; its decode time is
/// `index * 6000`, which names it on disk.
#[derive(Clone, Copy, Debug)]
struct Sent {
    starts_with_sync: bool,
    new_run: bool,
}

fn fragment(index: u64, sent: Sent, payload: usize, entry: &Arc<SampleEntry>) -> Fragment {
    let nal = |header: u8| {
        let mut data = u32::try_from(2 + payload).unwrap().to_be_bytes().to_vec();
        data.extend_from_slice(&[header, 1]);
        data.resize(4 + 2 + payload, 0xaa);
        Bytes::from(data)
    };
    Fragment {
        samples: vec![
            Sample {
                data: nal(if sent.starts_with_sync { 0x26 } else { 0x02 }),
                duration: 3000,
                is_sync: sent.starts_with_sync,
            },
            Sample {
                data: nal(0x02),
                duration: 3000,
                is_sync: false,
            },
        ],
        decode_time: index * 6000,
        // Four fragments a second, so files rotate within one directory day.
        wall: WallUs::new(1_793_460_958_000_000 + i64::try_from(index).unwrap() * 250_000),
        raw_rtp: 42,
        new_run: sent.new_run,
        sample_entry: Arc::clone(entry),
    }
}

fn mp4_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|x| x == "mp4") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Scans one recording and returns its fragments, after checking what holds
/// for every file: the scan reaches the end, the file opens at a sync
/// fragment, its sequence numbers count from one, and its sidecar is bound
/// to it with a record per fragment.
fn checked_file(path: &Path, seed: u64) -> Vec<FragmentInfo> {
    let mut file = File::open(path).unwrap();
    let len = file.metadata().unwrap().len();
    let report = reader::scan(
        &mut file,
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    let context = format!("seed {seed}: {}", path.display());
    assert_eq!(report.stop, None, "{context}");
    assert_eq!(report.valid_len, len, "{context}");
    let first = report.fragments.first().expect("no empty recording");
    assert!(first.starts_with_sync, "{context}");
    assert!(
        report
            .fragments
            .iter()
            .map(|info| info.sequence)
            .eq(1..=report.fragments.len() as u32),
        "{context}"
    );
    let sidecar = File::open(path.with_extension("idx")).unwrap();
    let (_, records) =
        read_index_file_bound_to_media(sidecar, MAX_PERSISTED_INDEX_RECORDS, &report, len)
            .unwrap()
            .unwrap_or_else(|| panic!("{context}: sidecar not bound"));
    assert_eq!(records.len(), report.fragments.len(), "{context}");
    report.fragments
}

#[test]
fn a_generated_fragment_sequence_lands_in_order_and_keeps_every_sync_start() {
    for seed in 1..=60 {
        let mut rng = Rng(seed);
        let dir = tempfile::tempdir().unwrap();
        let policy = StoragePolicy {
            sync_interval: Duration::from_millis(20),
            preallocate_bytes: 4_096,
            // A few fragments per file.
            rotate_bytes: 6_000,
            rotate_age: Duration::from_secs(3600),
        };
        let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy, 64).unwrap();
        let entry = Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1.1.6.L150.B0".into(),
            640,
            480,
        ));
        let key = StreamKey {
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Main,
        };
        let sent: Vec<Sent> = (0..80)
            .map(|_| Sent {
                starts_with_sync: rng.below(10) < 7,
                new_run: rng.below(20) == 0,
            })
            .collect();
        for (index, sent) in sent.iter().enumerate() {
            let payload = 100 + usize::try_from(rng.below(900)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            // The queue never fills, so nothing is dropped for lack of room.
            while writer.stats().queued() >= 32 {
                assert!(Instant::now() < deadline, "seed {seed}: the writer stalled");
                std::thread::sleep(Duration::from_millis(1));
            }
            writer
                .try_send(key.clone(), fragment(index as u64, *sent, payload, &entry))
                .unwrap();
        }
        writer.shutdown().unwrap();

        let files = mp4_files(dir.path());
        assert!(files.len() >= 5, "seed {seed}: {} files", files.len());
        let mut written = Vec::new();
        let mut fragment_bytes = 0;
        for path in files {
            let fragments = checked_file(&path, seed);
            written.extend(
                fragments
                    .iter()
                    .map(|info| usize::try_from(info.decode_time / 6000).unwrap()),
            );
            fragment_bytes += fragments
                .iter()
                .map(|info| u64::from(info.len))
                .sum::<u64>();
        }

        // Files sort by start time, so the fragments read back in send order.
        assert!(written.is_sorted(), "seed {seed}: {written:?}");
        let stats = writer.stats();
        assert_eq!(stats.fragments(), written.len() as u64, "seed {seed}");
        assert_eq!(
            stats.dropped(),
            (sent.len() - written.len()) as u64,
            "seed {seed}"
        );
        assert_eq!(stats.write_errors(), 0, "seed {seed}");
        assert_eq!(stats.bytes(), fragment_bytes, "seed {seed}");
        assert!(stats.syncs() >= 1, "seed {seed}");
        assert_eq!(
            (stats.queued(), stats.queued_bytes()),
            (0, 0),
            "seed {seed}"
        );
        for (index, sent) in sent.iter().enumerate() {
            if sent.starts_with_sync {
                assert!(
                    written.contains(&index),
                    "seed {seed}: sync-start fragment {index} was dropped"
                );
            }
        }
    }
}
