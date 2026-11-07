# iris-store

Recording files for irisd. A dedicated writer thread per video root takes finished
fragments from ingest, appends them to per-stream fragmented MP4 files with a sidecar index,
preallocates space ahead of the write position, `fdatasync`s on a timer, rotates files by
size or age, seals them on shutdown, and repairs a file that was open when the process died.
In the record path (RTSP in → fMP4 on disk → byte-range serving) this crate is "fMP4 on
disk": it owns every byte written under a video root and reports each file's lifecycle to an
observer (the daemon's catalog).

## What it provides

- `WriterHandle` — spawns the writer thread for a root (`spawn`, `spawn_with_observer`),
  an ordered chain of roots (`spawn_with_roots`), or a daemon chain already bound to durable
  root identities (`spawn_with_approved_roots`),
  queues fragments without blocking (`try_send`), exposes `WriterStats`, and seals every
  open file on `shutdown`.
- `WriterStats` — fragments, bytes, syncs, drops, files opened, write errors, queue depth and
  queued bytes with peaks, write and `fdatasync` latency histograms, slow I/O count.
- `QueueFull` — the error `try_send` returns when the bounded queue is full; the fragment is
  dropped and counted.
- `StoragePolicy` — sync interval, preallocation size, and rotation thresholds, derived from
  `StorageConfig`.
- `StoreObserver`, `SegmentOpened`, `SegmentProgress`, `SegmentSealed`, `NoopObserver` —
  file lifecycle notifications, delivered on the writer thread after each open, sync, and
  seal.
- `recover::recover_segment` and `recover::RecoveryReport` — scan a file with CRC
  verification, truncate it to its valid prefix, and rewrite its index. A valid contiguous
  index limits the CRC scan to its last indexed fragment and unindexed tail.
- `layout::segment_path` and `layout::index_path` — the on-disk naming,
  `<root>/<camera>/<kind>/<YYYY-MM-DD>/<HHMMSS.ffffff>Z.mp4` plus `.idx`.
- `RootIdentity`, `RootExpectation`, `ApprovedRoot`, and `admit_recording_root` — bind a daemon
  root to its durable `.irisd-root-id` marker and physical directory before the sandbox,
  reconciliation, or writer threads.
- `create_recording_root` — creates a standalone configured root and syncs each created directory
  entry through its parent before a caller publishes a writer or applies its sandbox.
- `free_space`, `free_space_approved`, `FreeSpace`, `remove_segment`, and
  `remove_segment_beneath` — the file-system calls retention needs; approved-root space probes
  reopen and verify the root before descriptor-based `fstatvfs`, while approved-root removal
  opens each layout component without following links and unlinks the pair through one pinned
  parent descriptor.

## How it is used

```rust
use std::path::PathBuf;

use iris_store::{StoragePolicy, WriterHandle};

let policy = StoragePolicy::from(&iris_core::config::StorageConfig::default());
let writer = WriterHandle::spawn(PathBuf::from("/srv/irisd/video"), policy, 64)?;
// Ingest calls this on its own task; a full queue returns QueueFull instead of waiting.
// writer.try_send(key, fragment)?;
writer.shutdown()?;
# Ok::<(), std::io::Error>(())
```

The daemon spawns the writer with a `StoreObserver` that forwards every notification to
the catalog, and wraps the handle in its `FragmentSink` for ingest.

## Invariants and limits

- **One writer thread per root**, fed by a bounded channel of complete fragments; the
  socket-reading tasks are never blocked on disk. A full queue drops the fragment
  (`docs/research/04-storage-format.md §8`).
- **Daemon root admission** requires a catalog expectation and a 128-bit durable marker. A
  previously registered or legacy-owned root is never created or initialized when missing,
  and a changed marker fails closed across restarts. Admission pins the current physical
  device/inode for the daemon run, rejects aliases and later pathname replacement, and does not
  treat that process-local identity as durable state. Each approved segment open pins that verified
  root, then creates layout directories and exclusively claims the pair descriptor-relatively with
  no-follow, beneath, and no-cross-mount resolution. Its free-space probe uses the same verified-root
  admission. Destructive pair removal has the same containment guarantees.
- **Root failover** tries roots in configuration order. Each root has a queue bounded by
  the requested capacity; aggregate queue counters cover the entire chain. Workers check
  the free-space floor before every fragment with a one-second cache. Approved roots retain
  the identity check even when the configured floor is zero. A root below the floor seals its
  open files and forwards to the next root; a write or sync error abandons its
  files for recovery and forwards subsequent fragments. An incomplete media write is
  retried on the next root. If complete media is followed by a sidecar or preallocation
  error, that fragment is counted once and left for recovery; the worker retires the root
  before taking another fragment. A fallback file starts only at a sync sample, so dependent
  fragments before that point are dropped. A full downstream queue records the same loss
  epoch at the forwarding edge, discards later dependents, and marks the first successfully
  forwarded sync fragment as a new run. Forwarding preserves order, and shutdown drains
  every root's queue before joining all writer threads. An unavailable root with a fallback
  remains retired until restart. The final root retries at the next sync fragment after a
  one-second cooldown and marks the recording gap as a discontinuity; drops and the reason
  are logged and counted. Roots that cannot be created do not prevent using later roots.
- **One `pwritev` per fragment**: `prft` + `moof` + `mdat` header, the sample payloads,
  and the 24-byte CRC trailer, then one sidecar record (`04 §4–5`).
- **Preallocation** with `fallocate(KEEP_SIZE)` ahead of the write position; files are
  truncated to their real length at seal, so a sealed file's length equals its valid
  length (`04 §8`).
- **Sync cadence**: every dirty data file and its index are `fdatasync`ed every
  `sync_interval` (default 5 s), staggered: each stream syncs at its own phase of the
  interval (its slot in order of first appearance, sixteen slots), and a file keeps that
  phase across rotations and across syncs it skips while clean, so fourteen streams do
  not hit the disk together. The observer learns the durable length after each sync.
  A write or sync over one second is counted and logged as slow (`04 §8`, `12 §5`).
- **Rotation** at `rotate_bytes` (default 1 GiB) or `rotate_secs` (default 1 h),
  whichever comes first, at the next sync sample; a new run (discontinuity) always opens a
  new file (`04 §3`).
- **Durability order**: newly created layout directories are synced through their parents;
  approved writers do so beneath the pinned admitted-root descriptor. Media and index paths are
  claimed exclusively, the initialization and index header are written, both files and the
  segment directory are synced, then the observer is told. A failed initialization removes only
  the pair claimed by that attempt through its pinned parent descriptor. Periodic data follows
  data written → `fdatasync` data and index → observer told; seal follows truncate → `fsync`
  file → `fsync` directory → observer told (`04 §6`).
- **Recovery** truncates to the last fragment whose trailer and CRC verify and rewrites
  the index from the scan, preserving the surviving records' flags (`04 §9`). It rejects
  media/sidecar identity aliases, publishes the rebuilt sidecar through an exclusively
  claimed, synced sibling temporary and atomic rename, then truncates and syncs the media;
  a publication failure therefore leaves media and arbitrary alias targets untouched. Recovery
  authoritatively CRC-scans every complete media fragment. A bounded sidecar may preserve only
  flags and raw-RTP metadata at matching offsets; its timing, length, and prefix facts never
  authorize publication or catalog sealing. Recovery always truncates to the valid length (also releasing a `KEEP_SIZE`
  reservation when the logical length is unchanged) and then syncs the media; the already
  published index describes only that CRC-verified prefix, so the catalog seals only after both
  are durable. A file without complete
  `ftyp` + `moov` initialization is rejected without changing its bytes or sidecar.
- **Paths are UTC**: the file name is the wall time of the first fragment to microsecond
  precision; exclusive numeric suffixes resolve true equal-timestamp collisions.

## Tests and measurements

- `tests/store.rs`: round trip of fragments through the writer and the scanner, queue
  depth and latency accounting, a new run opening a second file, recovery of a truncated
  file, full-queue drops, observer notifications in order, `remove_segment`, and
  `free_space`.
- Unit tests in `src/layout.rs` (dated paths) and `src/writer.rs`.
- Writer regressions cover ordered failover after write failures and low space, sync
  boundaries across streams and rotation, blocked fallback loss, continuation-only pressure,
  unavailable startup roots, terminal-root recovery, aggregate queue accounting, and
  cascading shutdown. Creation fault stages, pre-existing sidecars, allocation release,
  index-tail recovery, microsecond rotations, and basename-relative APIs have focused tests.
- Measurements: `docs/research/11-multi-camera-results.md` (twelve and fourteen streams
  for hours, sealed files checked with `irisd inspect`) and `12-ingest-cost.md §5` (the
  writer thread's share of CPU, the checksum).

## Further reading

- `docs/research/04-storage-format.md` §3 (layout), §4–5 (file and index bytes), §8 (I/O
  policy), §9 (recovery)
- `docs/research/12-ingest-cost.md` §5
- `docs/design/phase2-interfaces.md` (the observer contract)
- `STANDARDS.md`
