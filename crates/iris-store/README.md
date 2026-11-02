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
  verification, truncate it to its valid prefix, and rewrite its index.
- `layout::segment_path` and `layout::index_path` — the on-disk naming,
  `<root>/<camera>/<kind>/<YYYY-MM-DD>/<HHMMSS>Z.mp4` plus `.idx`.
- `free_space`, `FreeSpace`, `remove_segment` — the file-system calls retention needs.

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
- **Durability order**: data written → `fdatasync` data and index → observer told; seal:
  truncate → `fsync` file → `fsync` directory → observer told (`04 §6`).
- **Recovery** truncates to the last fragment whose trailer and CRC verify and rewrites
  the index from the scan, preserving the surviving records' flags (`04 §9`).
- **Paths are UTC**: the file name is the wall time of the first fragment.

## Tests and measurements

- `tests/store.rs`: round trip of fragments through the writer and the scanner, queue
  depth and latency accounting, a new run opening a second file, recovery of a truncated
  file, full-queue drops, observer notifications in order, `remove_segment`, and
  `free_space`.
- Unit tests in `src/layout.rs` (dated paths) and `src/writer.rs`.
- Measurements: `docs/research/11-multi-camera-results.md` (twelve and fourteen streams
  for hours, sealed files checked with `irisd inspect`) and `12-ingest-cost.md §5` (the
  writer thread's share of CPU, the checksum).

## Further reading

- `docs/research/04-storage-format.md` §3 (layout), §4–5 (file and index bytes), §8 (I/O
  policy), §9 (recovery)
- `docs/research/12-ingest-cost.md` §5
- `docs/design/phase2-interfaces.md` (the observer contract)
- `STANDARDS.md`
