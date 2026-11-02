# AGENTS.md — iris-store

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The on-disk layout (`src/layout.rs`), the one-`pwritev`-per-fragment write, the
  preallocation and truncate-at-seal rule, the sync cadence, and the durability order
  implement `docs/research/04-storage-format.md §3–§6 and §8`; change them only with that
  document and `tests/store.rs`.
- Recovery (`src/recover.rs`) implements `04 §9`: truncate to the valid prefix found by a
  CRC-verified scan, rewrite the index, keep surviving flags.
- The writer thread must never block on anything but the disk; it owns the channel's
  receiving end and the open files, and the observer runs on it.
- `WriterStats` accessor names are read by `irisd`'s `writer` statistics line and by
  `iris-tools daemon-stats`.
- The observer payloads (`SegmentOpened`, `SegmentProgress`, `SegmentSealed`) are the
  catalog's input; their fields map one to one onto `iris_catalog::Change`.

## Where things are

- `src/writer.rs` — `WriterHandle`, `WriterStats`, the channel, the thread body (write,
  sync sweep, seal on shutdown), slow-I/O warnings.
- `src/stream.rs` — `StreamWriter`: one stream's open file, rotation decisions, abandon on
  error.
- `src/segment.rs` — `SegmentFile`: create with init segment, append a fragment, sync,
  seal.
- `src/policy.rs` — `StoragePolicy` from `StorageConfig`.
- `src/layout.rs` — path naming.
- `src/observer.rs` — the `StoreObserver` trait and its notification types.
- `src/recover.rs` — `recover_segment`.
- `src/fs.rs` — `pwritev_all`, preallocation, directory sync, `free_space`,
  `remove_segment`.
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
- A write or sync error abandons the file (the handle is forgotten, recovery repairs it);
  it does not stop the thread or the other streams.
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
