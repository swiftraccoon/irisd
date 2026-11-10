# iris-store

Recording files for irisd. A dedicated writer thread per video root takes finished
fragments from ingest, appends them to per-stream fragmented MP4 files with a sidecar index,
preallocates space ahead of the write position, `fdatasync`s on a timer, rotates files by
size or age, and seals them on shutdown; the recovery module repairs a file that was open when
the process died, and the root-identity model admits each recording root against the
catalog's expectation. In the record path (RTSP in → fMP4 on disk → byte-range serving) this
crate is "fMP4 on disk": it owns every byte written under a video root and reports each
file's lifecycle to an observer (the daemon's catalog).

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
- `recover::recover_segment`, `recover::recover_segment_beneath`, `recover::IndexBinding`, and
  `recover::RecoveryReport` — scan every complete fragment of a file with CRC verification,
  truncate it to its valid prefix, and rewrite its index; the daemon variant works through an
  approved root and takes the catalog row's run and sample-entry hash for the rebuilt header.
- `layout::segment_path` and `layout::index_path` — the on-disk naming,
  `<root>/<camera>/<kind>/<YYYY-MM-DD>/<HHMMSS.ffffff>Z.mp4` plus `.idx`.
- `RootIdentity`, `RootExpectation`, `ApprovedRoot`, and `admit_recording_root` — bind a daemon
  root to its durable `.irisd-root-id` marker and physical directory before the sandbox,
  reconciliation, or writer threads.
- `create_recording_root` — creates a standalone configured root and syncs each created directory
  entry through its parent before a caller publishes a writer or applies its sandbox.
- `free_space`, `free_space_approved`, `FreeSpace`, and `remove_segment_beneath` — the
  file-system calls retention needs; approved-root space probes reopen and verify the root
  before descriptor-based `fstatvfs`, while approved-root removal opens each layout component
  without following links and unlinks the pair through one pinned parent descriptor.
- `ApprovedRoot`, `RootIdentity`, `RootExpectation`, `PhysicalIdentity`, `ROOT_IDENTITY_MARKER`,
  `admit_recording_root`, `verify_approved_root`, and `open_approved_root_directory` — the
  durable identity marker of a root and the daemon's admission against the catalog's
  expectation; `InvalidRootIdentity` is a malformed identity string.
- `open_segment_beneath`, `open_segment_pair_beneath`, and `open_regular_file` — opens of
  recordings and sidecars on disk that never follow a final symlink and never block: a FIFO,
  socket, or device is rejected with `InvalidData` ("not a regular file") before any read, and
  the returned descriptor performs ordinary blocking I/O.
- `Fragment` — the re-exported `iris_mp4::fragment::Fragment` the writer takes.

## How it is used

```rust
use std::path::PathBuf;

use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_store::{StoragePolicy, WriterHandle};

let policy = StoragePolicy::from(&iris_core::config::StorageConfig::default());
let writer = WriterHandle::spawn(PathBuf::from("/srv/irisd/video"), policy, 64)?;
let key = StreamKey {
    camera: CameraId::try_new("cam1")?,
    kind: StreamKind::Main,
};
// Ingest calls this on its own task with each completed fragment; a full queue
// returns QueueFull instead of waiting.
writer.try_send(key, fragment)?;
writer.shutdown()?;
```

The daemon spawns the writer with a `StoreObserver` that forwards every notification to
the catalog, and wraps the handle in its `FragmentSink` for ingest.

## Invariants and limits

- **One writer thread per root**, fed by a bounded channel of complete fragments; the
  socket-reading tasks are never blocked on disk. A full queue drops the fragment
  (`docs/research/04-storage-format.md §8`).
- **Daemon root admission** requires a catalog expectation and a 128-bit durable marker. A
  previously registered or legacy-owned root is never created or initialized when missing, and a
  changed marker fails closed across restarts. The marker is a regular file holding 32 lowercase
  hexadecimal characters, optionally followed by one line feed; irisd writes it without the line
  feed, and any other framing is rejected with the length found. The marker open is nonblocking and
  no-follow, so a FIFO or symlink marker fails admission and every later probe at once. A
  configured root path must hold no symbolic link in any component, not only at the end:
  resolution refuses links everywhere, so an operator configures the resolved physical path. A
  missing fresh root is made through directory descriptors from the deepest ancestor that exists
  without following any link, and admission continues from the created descriptor. Admission
  pins the current physical device/inode (`PhysicalIdentity`) for the daemon run, rejects
  aliases and later pathname replacement, and does not treat that process-local identity as
  durable state. Each approved segment open pins that verified root, then creates layout
  directories and exclusively claims the pair descriptor-relatively with no-follow, beneath, and
  no-cross-mount resolution. Its free-space probe uses the same verified-root admission.
  Destructive pair removal has the same containment guarantees.
- **Opens beneath an approved root** of an existing media file, sidecar, or marker add
  no-follow, close-on-exec, no-controlling-terminal, and nonblocking flags, resolve beneath the
  pinned directory without symlinks, magic links, or mount crossings, and accept only a regular
  file before restoring blocking I/O. Creations beneath an approved root use `O_CREAT | O_EXCL`
  through the same resolution, so nothing existing is opened, followed, or truncated. Pair
  removal never opens a FIFO, socket, device, or symlink in place of a member: it counts zero
  bytes and unlinks the name without following it. Once the root is reopened and verified, a
  missing layout directory means both members are absent: removal syncs the deepest ancestor
  that exists and reports zero bytes, while a missing root remains an error. Standalone writers
  (`WriterHandle::spawn`, as `irisd record` uses it) have no admitted root: they resolve the
  root and its layout directories by name and follow directory symlinks.
- **Private modes**: media, sidecars, and recovery temporaries are created 0600 and layout and
  root directories 0700; the process umask can only narrow those modes, never widen them.
- **Root failover** tries roots in configuration order. Each root has a queue bounded by
  the requested capacity; aggregate queue counters cover the entire chain. Workers check
  the free-space floor before every fragment with a one-second cache. Approved roots retain
  the identity check even when the configured floor is zero. A probe that fails for lack of a
  process resource (`EMFILE`, `ENFILE`, `ENOMEM`, `EAGAIN`, `EINTR`) keeps the open files, logs a
  warning, and probes again after the cadence; any other probe failure abandons the root like
  an I/O error. A root below the floor seals its
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
- **One vectored write per fragment**: `prft` + `moof` + `mdat` header, the sample payloads,
  and the 24-byte CRC trailer, then one sidecar record (`04 §4–5`). A `pwritev` takes at most
  1,024 slices, so a fragment of more than 1,022 samples continues in further calls.
- **Preallocation** with `fallocate(KEEP_SIZE)` ahead of the write position; files are
  truncated to their real length at seal, so a sealed file's length equals its valid
  length (`04 §8`).
- **Sync cadence**: every dirty data file and its index are `fdatasync`ed every
  `sync_interval` (default 5 s), staggered: each stream syncs at its own phase of the
  interval (its slot in order of first appearance, sixteen slots), and a file keeps that
  phase across rotations and across syncs it skips while clean, so fourteen streams do
  not hit the disk together. The observer learns the durable length after each sync.
  A write or sync over one second is counted and logged as slow (`04 §8`, `12 §5`).
- **Rotation** at `rotate_bytes` (default 1 GiB), `rotate_secs` (default 1 h), or 126,976
  fragments, whichever comes first, at the next sync sample; a new run (discontinuity) or a
  sample entry whose bytes differ always opens a new file, even when the CRC-32C matches. A
  file holding `MAX_PERSISTED_INDEX_RECORDS` (131,072) fragments takes no more: dependent
  fragments are dropped, counted, and logged once until a sync sample opens a new file as a new
  run, so every sidecar stays within its readers' record ceiling (`04 §3`).
- **Durability order**: newly created layout directories are synced through their parents;
  approved writers do so beneath the pinned admitted-root descriptor. Media and index paths are
  claimed exclusively, the initialization and index header are written, both files and the
  segment directory are synced, then the observer is told. A failed initialization removes only
  the pair claimed by that attempt through its pinned parent descriptor. Periodic data follows
  data written → `fdatasync` data and index → observer told; seal follows truncate → `fsync`
  file → `fsync` directory → observer told (`04 §6`).
- **Recovery** truncates to the last fragment whose trailer and CRC verify and rewrites the index
  from the scan, preserving the surviving records' flags (`04 §9`). It rejects media/sidecar
  identity aliases, publishes the rebuilt sidecar through an exclusively claimed, synced sibling
  temporary and atomic rename, then truncates and syncs the media; a publication failure therefore
  leaves media and arbitrary alias targets untouched. Recovery authoritatively CRC-scans every
  complete media fragment. A bounded sidecar may preserve only flags and raw-RTP metadata at
  matching offsets; its timing, length, and prefix facts never authorize publication or catalog
  sealing. Recovery always truncates to the valid length (also releasing a `KEEP_SIZE` reservation
  when the logical length is unchanged) and then syncs the media; the already published index
  describes only that CRC-verified prefix, so the catalog seals only after both are durable. A file
  without complete `ftyp` + `moov` initialization is rejected without changing its bytes or sidecar,
  and so is a file holding more than 131,072 fragments. The rebuilt header keeps the prior header's
  nonzero run id and sample-entry hash, else takes the caller's `IndexBinding` (the catalog row's),
  else zero, so a recovered file stays servable. The sidecar is opened through the pinned directory
  descriptor without following links or blocking; a missing, symlinked, or special sidecar counts
  as absent and is replaced, a sidecar that cannot be read for lack of a process resource fails
  recovery so the daemon retries the row later, and any other read failure is logged and counts
  as absent. `RecoverError::Absent` reports a missing media name or layout directory beneath the
  verified root; `Refused` a sidecar path that cannot be used; `InvalidTimeline` fragments whose
  decode times do not form a timeline. A crash between creating the rebuilt sidecar's temporary
  (`.<sidecar>.irisd-recover-<pid>-<n>.tmp`) and renaming it leaves that temporary behind;
  nothing removes it.
- **Paths are UTC**: the file name is the wall time of the first fragment to microsecond
  precision; exclusive numeric suffixes resolve true equal-timestamp collisions.

## Tests and measurements

- `tests/store.rs`: round trip of fragments through the writer and the scanner, queue
  depth and latency accounting, a new run opening a second file, recovery of a truncated file, a
  zero-filled tail, a torn last fragment, and a writer killed with SIGKILL mid-recording,
  deterministic full-queue drops, observer notifications in order, `remove_segment_beneath`, and
  `free_space`; 131,073 sync fragments rotating into two files that each bind to their sidecar,
  and private modes from both writers.
- `tests/writer_properties.rs`: seeded property test of the writer. A generated sequence of
  sync-start, continuation, and new-run fragments goes through many rotations; the files hold
  the fragments in send order, each opens at a sync fragment and binds its sidecar, no
  sync-start fragment is dropped, and the counters equal what a scan finds.
- `tests/recover_properties.rs`: seeded property test of recovery. A recording from the writer
  is cut, zero-filled, or overwritten with generated bytes from a generated offset, with its
  sidecar intact, missing, cut, or extended with noise; recovery keeps exactly the fragments
  the damage left alone, byte for byte, binds a sidecar to them, and changes nothing when run
  again.
- `tests/full_disk.rs`: the writer on a 3 MiB tmpfs that fills up, mounted in a user and mount
  namespace of the test's own (`unshare -Urm`), with and without preallocation: every fragment
  the writer counted is on disk, recovery cuts the torn tail a short write leaves, and writing
  resumes once the oldest recording is removed. A host without unprivileged user namespaces
  skips the body and says so.
- Unit tests in `src/layout.rs` (dated paths) and `src/writer.rs`; `src/fs.rs` covers marker
  framing, FIFO members and markers under two-second deadlines, special-file and
  missing-directory removal, private modes, and symlinked ancestors; `src/recover.rs` covers
  binding precedence, FIFO and symlinked sidecars, absence, and the fragment limit;
  `src/stream.rs` covers record-count rotation and forged CRC-32C collisions;
  `src/writer/worker.rs` covers transient and fatal probe errors.
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
