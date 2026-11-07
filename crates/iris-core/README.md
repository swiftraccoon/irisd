# iris-core

Shared identifiers, time units, event types, configuration, and lock-free metrics for
irisd. Every other crate in the workspace speaks in these types, so a wall-clock
microsecond is never confused with a 90 kHz media tick and a camera id is never confused
with a stream key. The crate sits under the whole record path (RTSP in → fMP4 on disk →
byte-range serving) and has no I/O of its own beyond reading a TOML file.

## What it provides

- `id::CameraId` — a validated camera identifier (1 to 32 characters of `[a-z0-9_-]`), safe
  as a directory name and a URL path component.
- `id::StreamKind` and `id::StreamKey` — the main/sub video kinds, the reserved audio kind
  used by catalog data types, and the camera-plus-kind pair that names a stream. Configuration
  rejects audio recording in the 0.1 alpha.
- `time::WallUs` — microseconds since the Unix epoch on the NVR's clock, with NTP64 and
  RFC 3339 conversions; the only wall clock the system trusts.
- `time::MediaTicks` and `time::TIMESCALE` — 90 kHz media time, the scale of every
  recording.
- `event::Event` with `EventSource`, `EventKind`, and `EventState` — one normalised camera
  event, placed on the NVR clock; `Event::bounded` limits camera-controlled strings on UTF-8
  boundaries.
- `config::Config` — the TOML configuration: `StorageConfig`, `ApiConfig`, `IngestConfig`,
  and one `CameraConfig` per camera, with derived helpers such as `CameraConfig::sub_url`
  and `CameraConfig::rtsp_ports`.
- `metrics::Histogram`, `metrics::HistogramSnapshot`, and `metrics::Peak` — latency
  histograms with per-interval percentiles and high-water marks using atomic updates.

## How it is used

```rust
use iris_core::config::Config;
use iris_core::time::{MediaTicks, WallUs};

let config = Config::from_toml_str(r#"
[[camera]]
id = "cam1"
name = "Example"
rtsp_url = "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0"
username = "user"
password = "secret"
"#)?;
assert_eq!(config.cameras[0].rtsp_ports(), vec![554]);

let gop = MediaTicks::from_secs(2);
let later = WallUs::now().offset(gop.as_micros());
# Ok::<(), iris_core::config::ConfigError>(())
```

`Config::load` reads a file. Core rejects unknown fields, duplicate camera ids, repeated
recorded kinds or event sources within a camera, `storage.min_free_percent` outside 0..=100,
`storage.max_fragment_ms` outside 1..=60,000, writer queues above 4,096 fragments per stream,
audio in the recorded-stream list, non-subscribable `onvif_metadata` and `manual` event sources,
and camera retention above 106,751,991 days. Zero disables
the free-space floor; a zero writer queue retains one slot. Parse errors retain a safe byte span
and generic syntax/schema description, without source excerpts or parser messages that can
disclose credentials.

## Invariants and limits

- Media time is 90 kHz everywhere (`TIMESCALE`); one millisecond is exactly 90 ticks
  (`docs/research/04-storage-format.md §7`).
- `WallUs` is the authority for placement, search, and retention; camera clocks are
  recorded, never used (`04 §7`, item 1).
- `WallUs::to_ntp64` truncates and `WallUs::from_ntp64` rounds, so a wall time written to a
  `prft` box reads back exactly (`04 §4`).
- A camera id is 1 to 32 characters of `[a-z0-9_-]`; it appears in paths and URLs unescaped
  (`07-architecture.md §4`).
- `CameraConfig::host` returns a bare host, including IPv6 without brackets. HTTP URL
  builders enclose IPv6 in brackets; RTSP authority parsing stops at `/`, `?`, or `#`.
- Event payloads are valid JSON capped at `event::MAX_EVENT_DATA_LEN` (64 KiB): invalid text is
  quoted and an over-limit encoding becomes `{"truncated":true}`. Unmapped vendor kind codes are
  capped at `event::MAX_EVENT_KIND_LEN` (1 KiB) on a UTF-8 boundary. Bounding rebuilds strings so
  a short value cannot retain a large backing allocation.
- Configuration defaults are the measured ones: 5 s sync interval, 2 s fragment cap,
  256 MiB preallocation, 1 GiB or 1 h file rotation, 20 ms socket read interval, 32 KiB
  receive low-water mark (`04 §8`, `12-ingest-cost.md`).
- `Histogram` buckets are fixed logarithmic bounds from 100 µs to 10 s plus one open bucket;
  percentiles are upper bounds of the bucket that holds the sample. Counts and ranks use
  published bucket samples; open-bucket bounds use a cumulative maximum independent of the
  resettable interval maximum. Concurrent updates can skew interval cutoffs and means.
- Era-0 NTP timestamps before 1970 clamp to Unix zero, including fractional seconds.

## Tests and measurements

Unit tests live beside each module: id validation and round trips, RFC 3339 and NTP64
conversions including a microsecond sweep, tick arithmetic, configuration defaults,
derived URLs and ports, configuration validation and error redaction, histogram buckets,
deltas, concurrent publication and maximum resets. The doctests
in `config` and `time` are part of the suite. The read interval and low-water defaults
come from `docs/research/12-ingest-cost.md`.

## Further reading

- `docs/research/04-storage-format.md` §7 (time model) and §8 (I/O policy defaults)
- `docs/research/07-architecture.md` §4 (core types) and §9 (observability)
- `docs/research/12-ingest-cost.md` (the ingest defaults)
- `STANDARDS.md`
