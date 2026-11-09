# AGENTS.md — iris-core

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- `time::TIMESCALE` is 90 000 and every recording uses it; changing it changes the on-disk
  format (`docs/research/04-storage-format.md` §4, §7).
- `WallUs` is microseconds since the Unix epoch on the NVR clock; NTP64 round trips must
  stay exact to the microsecond from 1970 through 2104 (tests
  `every_microsecond_fraction_round_trips_exactly` and
  `ntp_conversions_cross_the_2036_era_boundary_exactly`): `to_ntp64` wraps into era 1 at
  2036-02-07T06:28:16Z and `from_ntp64` reads seconds with the top bit clear as era 1.
- `CameraId` rules (1 to 32 characters of `[a-z0-9_-]`) are relied on by the storage layout
  and the API routes (`07-architecture.md` §4).
- Configuration keys and defaults are documented in `config/cameras.example.toml`; an
  added key or a changed default updates that file and the design document that fixes the value.
- Configuration rejects duplicate camera ids, repeated recorded kinds or event sources per
  camera, free-space percentages above 50, sync intervals outside 100..=60,000 ms, rotation
  outside 60..=86,400 s or 64 MiB..=64 GiB, preallocation above the smaller of the rotation
  size and 4 GiB, a zero quota, fragment durations outside 1..=60,000 ms, queues above 4,096
  complete fragments per stream, read intervals above 1,000 ms, receive low-water marks above
  1,024 KiB, camera URLs that embed credentials, and retention outside 1..=106,751,991 days.
  The API needs a
  token of at least 32 visible ASCII characters that is not the example placeholder, or the
  `allow_unauthenticated` switch with no token on a loopback listener. Derived queue and
  media-tick arithmetic is checked before side effects. Parse errors must retain no input or
  value-bearing parser message, including through `Debug` or error sources; token errors name
  a position, never a value; `CameraConfig`'s hand-written `Debug` redacts the password and
  destructures every field, so a field added later must be placed explicitly.
- `CameraConfig::host` returns bare IPv6; HTTP URL authority builders add brackets.
  `CameraConfig::validate` accepts only hosts that pass `is_valid_host`, so every crate that
  builds a URL from a configured host may rely on it parsing.
- Text a camera sends reaches a log line only through `text::LogSafe`; crates share this one
  adapter rather than escaping on their own.
- `Event` placement uses `nvr_time`; `camera_time` is diagnostic only (`04 §7`).
- `Event::bounded` keeps data as valid JSON: invalid text is quoted and an encoding beyond
  64 KiB becomes `{"truncated":true}`. It bounds unmapped kinds at a 1 KiB UTF-8 boundary and
  releases excess backing capacity. Catalog notification and direct-insert paths both call it.

## Where things are

- `id.rs` — `CameraId`, `StreamKind`, `StreamKey`, their parsing and display.
- `time.rs` — `WallUs`, `MediaTicks`, `CivilTime`, NTP64 and RFC 3339 conversions.
- `event.rs` — `Event`, its source, kind, and state enums, and camera-controlled string limits.
- `config.rs` — the TOML schema, defaults, and derived helpers (`sub_url`, `rtsp_ports`,
  `connect_ports`, `http_base`, `url_for`, `ProvisioningConfig::refused_port`).
- `metrics.rs` — `Histogram`, `HistogramSnapshot`, `Peak`.
- `text.rs` — `LogSafe`, the one way peer-controlled text reaches a log line.
- `nonce.rs` — `client_nonce`, random digest client nonces (`getrandom`).

## How to verify a change

```sh
cargo fmt -p iris-core
cargo clippy -p iris-core --all-targets -- -D warnings
cargo test -p iris-core
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-core --no-deps
```

Most crates depend on this one, so finish with `just check` before handing a change over.

## Pitfalls

- `MediaTicks::as_micros` rounds toward negative infinity; converting back and forth is
  not an identity for arbitrary tick counts (test `ticks_convert_to_micros_exactly` fixes
  the exact cases). It multiplies in `i64` and can overflow for tick counts near the limit;
  `checked_as_micros` computes in `i128` and refuses a result that does not fit.
- `Config` uses `deny_unknown_fields`; a misspelt key is an error, not a silent default.
- `CameraId` deserialization validates; do not add a bypass constructor.
- `Histogram::take_max` resets only the interval maximum; only one reporter may call it.
  Publish each bucket sample after its cumulative bound, and derive counts from buckets.
- Doc comments and messages follow the timeless standard (`STANDARDS.md`); no dates or
  words such as "currently" in this crate.

## Related documents

- `docs/research/04-storage-format.md` §7, `07-architecture.md` §4 and §9,
  `12-ingest-cost.md`
- Dependents: `iris-mp4`, `iris-ingest`, `iris-store`, `iris-catalog`, `iris-events`,
  `iris-api`, `iris-replay`, `irisd`.
