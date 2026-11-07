# AGENTS.md — iris-store

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The on-disk layout (`src/layout.rs`), the one-`pwritev`-per-fragment write, the
  preallocation and truncate-at-seal rule, the sync cadence, and the durability order
  implement `docs/research/04-storage-format.md §3–§6 and §8`; change them only with that
  document and `tests/store.rs`.
- Recovery (`src/recover.rs`) implements `04 §9`: reject media/sidecar inode aliases, build and
  sync the index in an exclusively claimed sibling temporary, atomically replace the sidecar,
  then truncate and sync the CRC-verified media prefix while keeping surviving flags. Recovery
  authoritatively CRC-scans every complete media fragment before publication; a bounded sidecar
  may supply only flags and raw-RTP metadata at matching offsets, never prefix validity or catalog
  times. Equal-length truncation and the media sync are required even when the complete file needs
  no logical shortening, before catalog seal. The rebuilt header keeps each nonzero run id and
  sample-entry hash of the prior header, else the caller's `IndexBinding`, else zero. A file over
  `MAX_PERSISTED_INDEX_RECORDS` fragments is refused before any byte changes.
- Every open of a marker, media file, or sidecar beneath a root goes through `fs::open_regular_beneath`:
  nonblocking, no-follow, beneath, no-cross-mount, regular files only. Removal never opens a
  special file, and a missing layout directory under a verified root is absence, not an error.
- The writer never appends a fragment to a file that holds `MAX_PERSISTED_INDEX_RECORDS`
  fragments, and rotates at the next sync sample once a file holds 4,096 fewer. A sample entry
  whose bytes differ always rotates; its CRC-32C alone never decides.
- A free-space probe error for lack of a process resource (`EMFILE`, `ENFILE`, `ENOMEM`,
  `EAGAIN`, `EINTR`) never abandons files; every other probe error does.
- Created files are 0600 and created directories 0700, independent of the umask.
- Initialization claims media and sidecar exclusively, makes their bytes and directory entries
  durable before `SegmentOpened`, and rolls back only the attempt-owned pair on failure. An
  approved writer pins its verified root before layout creation and performs every component and
  final open descriptor-relatively with no-follow, beneath, and no-cross-mount resolution.
- Daemon roots are admitted against `.irisd-root-id`, their catalog identity, and physical
  device/inode before writer threads. Approved-root free-space admission reopens and verifies that
  root before descriptor-based `fstatvfs`. Approved-root removal descends no-follow beneath one
  verified root descriptor and unlinks both names through one pinned parent descriptor.
- The writer thread must never block on anything but the disk; it owns the channel's
  receiving end and the open files, and the observer runs on it.
- `WriterStats` accessor names are read by `irisd`'s `writer` statistics line and by
  `iris-tools daemon-stats`.
- The observer payloads (`SegmentOpened`, `SegmentProgress`, `SegmentSealed`) are the
  catalog's input; their fields map one to one onto `iris_catalog::Change`.
- Multiple roots form an ordered chain of bounded queues, with shared statistics. A failed
  root forwards in order to its fallback and remains retired until restart; the final root
  retries at sync after a one-second cooldown. A fallback never opens on a dependent fragment.
  Loss at a full forwarding edge is remembered per stream until a sync fragment is successfully
  forwarded as a new run. The cached free-space floor is checked before every fragment append.
  Shutdown cascades its marker after forwarded fragments and joins every worker.

## Where things are

- `src/writer.rs` — `WriterHandle`, `WriterStats`, the channel, the thread body (write,
  sync sweep, seal on shutdown), slow-I/O warnings.
- `src/writer/worker.rs` — one root's disk worker, free-space checks, ordered failover,
  terminal-root retries, and cascading shutdown.
- `src/stream.rs` — `StreamWriter`: one stream's open file, rotation decisions, abandon on
  error.
- `src/segment.rs` — `SegmentFile`: create with init segment, append a fragment, sync,
  seal.
- `src/policy.rs` — `StoragePolicy` from `StorageConfig`.
- `src/layout.rs` — path naming.
- `src/observer.rs` — the `StoreObserver` trait and its notification types.
- `src/recover.rs` — `recover_segment`.
- `src/fs.rs` — root identity/admission, `pwritev_all`, preallocation, directory sync,
  approved-root free-space and contained opens, and descriptor-relative pair removal.
- `tests/store.rs` — integration tests against a temporary root.

## How to verify a change

```
cargo fmt -p iris-store
cargo clippy -p iris-store --all-targets -- -D warnings
cargo test -p iris-store
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-store --no-deps
```

Tests use temporary directories only. A change to what the writer writes must keep
`iris_mp4::reader::scan` with `verify_crc` reporting every fragment of a sealed file, and
`irisd inspect` is the field check on real files.

## Pitfalls

- The queue counters are incremented before `try_send` and rolled back on failure so the
  thread's decrement can never observe a missing increment; keep that order.
- `maybe_sync` returns `Ok(true)` only when it synced; latency and the observer's progress
  notification key off that value.
- A complete media append is acknowledged even if the following sidecar append or
  preallocation fails. `maybe_sync` surfaces that error immediately, before its due-time
  check; the worker must run this sweep before taking another fragment. Only incomplete
  media writes are retried on a fallback, so recovery cannot discover duplicate fragments.
- A write or sync error abandons the root's files for recovery and retires the root. Its
  queued fragments proceed to the fallback; the terminal root retries after a cooldown.
- Rotation happens only at a sync sample: a fragment that does not start with a sync
  sample never opens a file and is dropped (`Written::Dropped`).
- `seal` truncates to the written length; the sidecar must be complete before the data
  file's final `fsync` and the directory sync.
- Preallocation uses `KEEP_SIZE`, so the file length is always the written length; a
  scanner never sees preallocated space, but recovery can see zero-filled tails from a
  crash mid-write.
- The writer thread is also where the trailer CRC is computed; `docs/research/12 §5`
  records why it stays there and which implementation it uses (`iris_mp4::crc`).

## Related documents

- `docs/research/04-storage-format.md`, `11-multi-camera-results.md`, `12-ingest-cost.md §5`,
  `docs/design/phase2-interfaces.md`.
- Depends on `iris-core` and `iris-mp4`. Depended on by `irisd` (writer, retention,
  recovery at start-up).
