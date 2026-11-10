//! The writer on a filesystem that runs out of space.
//!
//! The test mounts a 3 MiB tmpfs in a user and mount namespace of its own
//! (`unshare -Urm`, no privilege needed) and runs its body there, so the
//! kernel returns the real failed preallocations, short writes, and `ENOSPC`.
//! Where the host forbids unprivileged user namespaces the test reports that
//! and passes without running the body.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::init::SampleEntry;
use iris_mp4::reader::{self, ScanOptions};
use iris_store::recover::recover_segment;
use iris_store::{StoragePolicy, WriterHandle};

/// Names the tmpfs mount point for the body; set only inside the namespace.
const ROOT_VARIABLE: &str = "IRIS_FULL_DISK_ROOT";
/// The exit status of the wrapper when the tmpfs cannot be mounted.
const NO_MOUNT: i32 = 77;
/// The shell's exit status when it cannot execute the test binary, as for a
/// binary of another architecture run under emulation without a registered
/// interpreter.
const NO_EXEC: i32 = 126;
/// Payload bytes per fragment: fifteen of them fill the filesystem.
const PAYLOAD: usize = 200_000;

fn entry() -> Arc<SampleEntry> {
    Arc::new(SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
        "hvc1.1.6.L150.B0".into(),
        640,
        480,
    ))
}

/// One length-prefixed NAL of `header` type with `body` bytes of filler.
fn nal(header: u8, body: usize) -> Bytes {
    let len = u32::try_from(2 + body).unwrap();
    let mut v = len.to_be_bytes().to_vec();
    v.extend_from_slice(&[header, 1]);
    v.resize(4 + 2 + body, 0xaa);
    Bytes::from(v)
}

/// A fragment that starts with a sync sample, two seconds after the one before.
fn fragment(index: u64, new_run: bool, entry: &Arc<SampleEntry>) -> Fragment {
    Fragment {
        samples: vec![
            Sample {
                data: nal(0x26, PAYLOAD / 2),
                duration: 3000,
                is_sync: true,
            },
            Sample {
                data: nal(0x02, PAYLOAD / 2),
                duration: 3000,
                is_sync: false,
            },
        ],
        decode_time: index * 6000,
        wall: WallUs::new((1_793_460_958 + 2 * i64::try_from(index).unwrap()) * 1_000_000),
        raw_rtp: 42,
        new_run,
        sample_entry: Arc::clone(entry),
    }
}

fn key() -> StreamKey {
    StreamKey {
        camera: CameraId::try_new("cam1").unwrap(),
        kind: StreamKind::Main,
    }
}

fn mp4_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|x| x == "mp4") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

/// Waits until the writer has taken everything queued, at most ten seconds.
fn drain(writer: &WriterHandle) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while writer.stats().queued() > 0 {
        assert!(Instant::now() < deadline, "the writer did not drain");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Fragments a scan accepts in `path`, CRC included.
fn valid_fragments(path: &Path) -> usize {
    reader::scan(
        File::open(path).unwrap(),
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap()
    .fragments
    .len()
}

/// Runs this test again inside a namespace whose tmpfs is mounted at a
/// temporary directory. Returns `false` when the host cannot provide one.
fn rerun_on_a_small_filesystem(test: &str) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let probe = Command::new("unshare").args(["-Urm", "true"]).status();
    if !probe.as_ref().is_ok_and(ExitStatus::success) {
        eprintln!("skipped: no unprivileged user namespace ({probe:?})");
        return false;
    }
    let status = Command::new("unshare")
        .args(["-Urm", "sh", "-c"])
        .arg(format!(
            "mount -t tmpfs -o size=3m,mode=700 tmpfs \"$0\" || exit {NO_MOUNT}; exec \"$@\""
        ))
        .arg(dir.path())
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(ROOT_VARIABLE, dir.path())
        .status()
        .unwrap();
    if status.code() == Some(NO_MOUNT) {
        eprintln!("skipped: tmpfs cannot be mounted in the namespace");
        return false;
    }
    if status.code() == Some(NO_EXEC) {
        eprintln!("skipped: the namespace cannot execute this test binary");
        return false;
    }
    assert!(
        status.success(),
        "the body failed in the namespace: {status}"
    );
    true
}

#[test]
fn a_full_filesystem_loses_no_acknowledged_fragment_and_writing_resumes() {
    let Some(mount) = std::env::var_os(ROOT_VARIABLE).map(PathBuf::from) else {
        rerun_on_a_small_filesystem(
            "a_full_filesystem_loses_no_acknowledged_fragment_and_writing_resumes",
        );
        return;
    };
    // With preallocation the reservation fails first; without it the
    // kernel cuts a fragment's write short.
    for (name, preallocate_bytes) in [("preallocated", 256 * 1024), ("plain", 0)] {
        let root = mount.join(name);
        let torn = fill_free_and_resume(&root, preallocate_bytes);
        println!("full-disk body ran: {name}, torn bytes recovered: {torn}");
        fs::remove_dir_all(&root).unwrap();
    }
}

/// Fills the filesystem through a writer on `root`, frees space, and checks
/// the outcome. Returns the bytes recovery cut from torn tails.
fn fill_free_and_resume(root: &Path, preallocate_bytes: u64) -> u64 {
    let policy = StoragePolicy {
        sync_interval: Duration::from_millis(50),
        preallocate_bytes,
        rotate_bytes: 1 << 20,
        rotate_age: Duration::from_secs(3600),
    };
    let writer = WriterHandle::spawn(root.to_path_buf(), policy, 8).unwrap();
    let entry = entry();

    // Fill the filesystem: 3 MiB holds fewer than sixteen fragments.
    let mut index = 0;
    while writer.stats().write_errors() == 0 {
        assert!(index < 40, "the filesystem never filled");
        writer
            .try_send(key(), fragment(index, index == 0, &entry))
            .unwrap();
        drain(&writer);
        index += 1;
    }
    let files = mp4_files(root);
    assert!(files.len() >= 2, "rotation left several files: {files:?}");

    // Free space as retention does, by removing the oldest recording.
    let oldest = &files[0];
    let removed = valid_fragments(oldest);
    assert!(removed >= 1, "the oldest file holds complete fragments");
    fs::remove_file(oldest).unwrap();
    fs::remove_file(oldest.with_extension("idx")).unwrap();

    // The final root retries at a sync fragment after its one-second cooldown.
    let before = writer.stats().fragments();
    let deadline = Instant::now() + Duration::from_secs(10);
    while writer.stats().fragments() == before {
        assert!(Instant::now() < deadline, "writing did not resume");
        std::thread::sleep(Duration::from_millis(200));
        writer
            .try_send(key(), fragment(index, true, &entry))
            .unwrap();
        drain(&writer);
        index += 1;
    }
    writer.shutdown().unwrap();

    // Every fragment the writer counted is on disk, and recovery leaves
    // every file a clean recording.
    let mut on_disk = removed;
    let mut torn = 0;
    for path in mp4_files(root) {
        let report = recover_segment(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), report.valid_len);
        assert_eq!(
            valid_fragments(&path),
            report.fragments,
            "{}",
            path.display()
        );
        on_disk += report.fragments;
        torn += report.original_len - report.valid_len;
    }
    let counted = usize::try_from(writer.stats().fragments()).unwrap();
    assert!(
        on_disk >= counted,
        "{counted} fragments counted, {on_disk} on disk"
    );
    torn
}
