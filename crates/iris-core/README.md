# iris-core

Shared identifiers, time units, event types, configuration, and lock-free metrics for irisd. The
recorder's crates speak in these types, so a wall-clock microsecond is never confused with a 90 kHz
media tick and a camera id is never confused with a stream key. The crate sits under the whole
record path (RTSP in → fMP4 on disk → byte-range serving) and has no I/O of its own beyond
reading a TOML file.

## What it provides

- `id::CameraId` — a validated camera identifier (1 to 32 characters of `[a-z0-9_-]`), safe
  as a directory name and a URL path component.
- `id::StreamKind` and `id::StreamKey` — the main/sub video kinds, the reserved audio kind
  used by catalog data types, and the camera-plus-kind pair that names a stream (serialized
  as `{camera, kind}`). Configuration rejects audio recording.
- `time::WallUs` — microseconds since the Unix epoch on the NVR's clock, with NTP64 (both
  eras) and RFC 3339 conversions; the only wall clock the system trusts.
- `time::days_from_civil` and `time::days_in_month` — the civil-date arithmetic the API's and
  the ONVIF timestamp parsers share, the inverse of the conversion `WallUs` formats with.
- `time::MediaTicks` and `time::TIMESCALE` — 90 kHz media time, the scale of every
  recording.
- `event::Event` with `EventSource`, `EventKind`, and `EventState` — one normalised camera
  event, placed on the NVR clock; `Event::bounded` limits camera-controlled strings on UTF-8
  boundaries. The source and state enums share one spelling across serde, `Display`, and
  `FromStr` (`dahua_cgi`, `start`, …); `EventKind::name` gives a kind's name or vendor code.
- `config::Config` — the TOML configuration: `StorageConfig`, `ApiConfig`, `IngestConfig`,
  `ProvisioningConfig` (cameras added through the API: the switch, the limit, and the TCP
  ports they may use), and one `CameraConfig` per camera, with derived helpers such as
  `CameraConfig::sub_url`, `CameraConfig::rtsp_ports`, and `CameraConfig::connect_ports`,
  `ProvisioningConfig::refused_port`, `CameraConfig::validate` for one
  camera on its own, and `CameraConfig::snapshot_method` (`SnapshotMethod` and
  `ResolvedSnapshot`: the Dahua CGI endpoint, the ONVIF Media service, or none).
- `metrics::Histogram`, `metrics::HistogramSnapshot`, and `metrics::Peak` — latency
  histograms with per-interval percentiles and high-water marks using atomic updates.
- `text::LogSafe` — any `Display` value as one bounded, escaped log line (printable ASCII
  passes; line breaks, other controls, and non-ASCII are escaped; cut to 512 characters with a
  `...` marker), for text a camera or another peer controls.
- `nonce::client_nonce` — a random 64-bit hexadecimal token from the operating system's
  random source, for HTTP digest client nonces.
- `config::is_valid_host` and `config::MAX_HOST_BYTES` — whether a text is a bare host name or
  IP address (what `http://{host}/` parses back to, at most 253 bytes), applied to every camera
  host the daemon accepts from configuration or the API.

## How it is used

```rust
use iris_core::config::{Config, ConfigError};
use iris_core::time::{MediaTicks, WallUs};

fn main() -> Result<(), ConfigError> {
    let config = Config::from_toml_str(
        r#"
[api]
allow_unauthenticated = true

[[camera]]
id = "cam1"
name = "Example"
rtsp_url = "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0"
username = "user"
password = "secret"
"#,
    )?;
    assert_eq!(config.cameras[0].rtsp_ports(), vec![554]);

    let gop = MediaTicks::from_secs(2);
    let later = WallUs::now().offset(gop.as_micros());
    assert!(later > WallUs::now().offset(-1));
    Ok(())
}
```

`Config::load` reads a file. Every `ConfigError` variant names one rule; the ranges are:
`storage.min_free_percent` 0..=50, `storage.sync_interval_ms` 100..=60,000,
`storage.rotate_secs` 60..=86,400, `storage.rotate_bytes` 64 MiB..=64 GiB,
`storage.preallocate_bytes` at most the smaller of `rotate_bytes` and 4 GiB, `storage.quota_gib`
at least 1 when set, `storage.max_fragment_ms` 1..=60,000, `storage.max_queued_fragments` at
most 4,096, `ingest.read_interval_ms` at most 1,000, `ingest.recv_low_water_kib` at most 1,024,
`api.max_connections` 16..=65,536, `api.max_streaming_media` and `api.max_bounded_media`
1..=1,024, `provisioning.max_cameras` 1..=64, and camera `retention_days` 1..=106,751,991. Core
also rejects unknown keys, duplicate camera ids, malformed camera ids, repeated recorded kinds
or event sources within a camera, audio in the recorded-stream list, the non-subscribable
`onvif_metadata` and `manual` event sources, a camera URL that is not `rtsp://` with a valid
host or that embeds credentials, port 0 in `provisioning.connect_ports`, and an empty
`connect_ports` with provisioning enabled. The API needs at least one bearer token of 32 or more
visible ASCII characters (0x21 to 0x7e) that is not the example placeholder, or
`api.allow_unauthenticated = true` with no token on a loopback listener; a blank token and a
token list combined with the switch are rejected, and token errors name the entry's position,
never its value. Zero disables the free-space floor; a zero writer queue retains one slot. Parse
errors retain a safe byte range and a generic syntax/schema description, without source
excerpts or parser messages that can disclose credentials. `CameraConfig`'s `Debug` output shows
the password as `<redacted>`.

## Invariants and limits

- Media time is 90 kHz everywhere (`TIMESCALE`); one millisecond is exactly 90 ticks
  (`docs/research/04-storage-format.md §7`).
- `WallUs` is the authority for placement, search, and retention; camera clocks are
  recorded, never used (`04 §7`, item 1).
- `WallUs::to_ntp64` truncates and `WallUs::from_ntp64` rounds, so a wall time written to a
  `prft` box reads back exactly; the RFC 4330 era rule (NTP seconds with the top bit set are
  era 0, the others era 1) makes that hold from 1970 through 2104 (`04 §4`).
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
- Era-0 NTP timestamps before 1970 clamp to Unix zero, including fractional seconds; NTP
  second zero is 2036-02-07T06:28:16Z, the start of era 1.

## Tests and measurements

Unit tests live beside each module: id validation, round trips, and stream-key serialization,
RFC 3339 and NTP64 conversions including a microsecond sweep and the 2036 era boundary, tick
arithmetic, configuration defaults, derived URLs and ports, configuration validation (storage and
ingest range boundaries, host validity, embedded credentials, API token rules, the example
configuration with a generated token) and error and `Debug` redaction, event bounding (UTF-8 cut,
JSON quoting, the encoded 64 KiB limit, released capacity) and event names, histogram buckets,
deltas, concurrent publication and maximum resets, log-line escaping and truncation, and nonce
format. The doctests in the crate root and `config` are part of the suite. The read interval and
low-water defaults come from `docs/research/12-ingest-cost.md`.

## Further reading

- `docs/research/04-storage-format.md` §7 (time model) and §8 (I/O policy defaults)
- `docs/research/07-architecture.md` §4 (core types) and §9 (observability)
- `docs/research/12-ingest-cost.md` (the ingest defaults)
- `STANDARDS.md`
