# AGENTS.md — iris-core

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- `time::TIMESCALE` is 90 000 and every recording uses it; changing it changes the on-disk
  format (`docs/research/04-storage-format.md` §4, §7).
- `WallUs` is microseconds since the Unix epoch on the NVR clock; NTP64 round trips must
  stay exact to the microsecond (test `every_microsecond_fraction_round_trips_exactly`).
- `CameraId` rules (1 to 32 characters of `[a-z0-9_-]`) are relied on by the storage layout
  and the API routes (`07-architecture.md` §4).
- Configuration keys and defaults are documented in `config/cameras.example.toml`; an
  added key or a changed default updates that file and the design document that fixes the value.
- `Event` placement uses `nvr_time`; `camera_time` is diagnostic only (`04 §7`).

## Where things are

- `id.rs` — `CameraId`, `StreamKind`, `StreamKey`, their parsing and display.
- `time.rs` — `WallUs`, `MediaTicks`, `CivilTime`, NTP64 and RFC 3339 conversions.
- `event.rs` — `Event` and its source, kind, and state enums.
- `config.rs` — the TOML schema, defaults, and derived helpers (`sub_url`, `rtsp_ports`,
  `http_base`, `url_for`).
- `metrics.rs` — `Histogram`, `HistogramSnapshot`, `Peak`.

## How to verify a change

```sh
cargo fmt -p iris-core
cargo clippy -p iris-core --all-targets -- -D warnings
cargo test -p iris-core
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-core --no-deps
```

Every crate depends on this one, so finish with `just check` before handing a change over.

## Pitfalls

- `MediaTicks::as_micros` rounds toward negative infinity; converting back and forth is
  not an identity for arbitrary tick counts (test `ticks_convert_to_micros_exactly` fixes
  the exact cases).
- `Config` uses `deny_unknown_fields`; a misspelt key is an error, not a silent default.
- `CameraId` deserialization validates; do not add a bypass constructor.
- `Histogram::take_max` resets the maximum; only one reporter may call it.
- Doc comments and messages follow the timeless standard (`STANDARDS.md`); no dates or
  words such as "currently" in this crate.

## Related documents

- `docs/research/04-storage-format.md` §7, `07-architecture.md` §4 and §9,
  `12-ingest-cost.md`
- Dependents: every other crate in the workspace (`iris-h265` excepted).
