# AGENTS.md — iris-events

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- Events are placed on the recorder's clock (`nvr_time`); a camera timestamp is carried
  alongside, never substituted. Changing this changes `docs/research/05-ingest-events.md §3`.
- The ONVIF pull timeout stays below the 10 s request bound of the HTTP transport, and the
  subscription is renewed before its lifetime ends. The timing constants at the top of
  `onvif.rs` and `feed.rs` are measured values; a change needs a measurement in
  `docs/research/11-multi-camera-results.md` or a successor.
- Credentials never appear in logs, URLs, errors, or `Debug` output (`CameraEndpoint`
  redacts its password).
- `EventSink::publish` is called from receive loops and must stay non-blocking.
- The `EventStats` accessors feed the daemon's `events` statistics line; renaming or
  removing one changes `iris-tools daemon-stats` and `soak-report`.

## Where things are

- `lib.rs` — crate overview and re-exports.
- `endpoint.rs` — `CameraEndpoint` and the `EventSink` trait.
- `feed.rs` — the CGI push feed session loop and reconnection.
- `dahua.rs` — CGI part parsing and normalisation into `Event`.
- `multipart.rs` — incremental `multipart/x-mixed-replace` reader with a part-size bound.
- `digest.rs` — HTTP digest authentication.
- `onvif.rs` — pull-point subscription loop, topic mapping, ISO 8601 parsing.
- `backoff.rs` — reconnection pacing.
- `stats.rs` — per-source counters and the pull-latency histogram.

## How to verify a change

```sh
cargo fmt -p iris-events
cargo clippy -p iris-events --all-targets -- -D warnings
cargo test -p iris-events
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-events --no-deps
```

Never probe cameras from tests. The parsers are tested from captured text; hardware checks
of a source are run against the configured cameras through the daemon, with the limits in
the root `AGENTS.md`, and their results go in `docs/research`.

## Pitfalls

- Heartbeats are messages but not events: count them in `messages`, not `events`.
- A clean camera-side close of the CGI feed is an error to the session loop (it reconnects);
  do not treat it as success.
- Multipart `Content-Length` headers arrive without a space after the colon on some firmware,
  and blank lines vary; the reader tolerates both and the tests pin that.
- An ONVIF camera may report UTC an hour off while checking token time against its true
  clock; keep the host-time fallback.
- Network-facing parsers must not panic on any input (root `AGENTS.md` rule 8); the multipart
  and digest parsers have fuzz targets under `fuzz/` (`multipart`, `digest_challenge`, `dahua_part`, `onvif_utc`); run them after a parser change.

## Related documents

- `docs/research/05-ingest-events.md` §3, `docs/research/01-camera-probe.md`,
  `docs/research/11-multi-camera-results.md`, `docs/design/phase2-interfaces.md`.
