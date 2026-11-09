# AGENTS.md — iris-events

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- Events are placed on the recorder's clock (`nvr_time`); a camera timestamp is carried
  alongside, never substituted. Changing this changes `docs/research/05-ingest-events.md §3`.
- The ONVIF pull timeout stays below the 10 s request bound of the SOAP transport, and the
  subscription is renewed from its granted camera-clock lifetime before it ends, never faster
  than the named 750 ms renewal floor. The timing constants at the top of `onvif.rs`,
  `media.rs`, and `feed.rs` come from measurements or from Home Assistant's constants
  (`docs/research/appendix/D-camera-integration.md`); a change needs a measurement in
  `docs/research/11-multi-camera-results.md` or a successor.
- Credentials never appear in logs, URLs, errors, or `Debug` output (`CameraEndpoint` and
  `DigestAuth` redact both credentials). Camera text reaches a log line only through
  `iris_core::text::LogSafe`; never log a `SessionError`, a topic, or a CGI field with `%`
  directly.
- `CameraEndpoint::host` is bare, including IPv6, and passes `iris_core::config::is_valid_host`;
  `base_url` brackets IPv6 authorities, and `SoapTransport::new` refuses any other host with
  `SessionError::InvalidEndpoint` rather than building a URL it cannot parse.
- Stream URLs move onto the configured host, keeping the advertised port, path, and query;
  only `rtsp` without userinfo or fragment, at most 2,048 bytes; at most eight profiles and 60 s
  per lookup (snapshot URL lookups too); profile text cleaned and cut to 128 characters; only
  `MediaUrlError::kind` reaches logs and API answers.
- `EventSink::publish` is called from receive loops and must stay non-blocking.
- CGI attachment has a 10 s header/authentication deadline; do not apply an overall request
  timeout to the indefinite feed body. Its stall clock advances only after a complete part.
  Both CGI and ONVIF honor `CameraEndpoint::tls_verify`, and neither follows redirects.
- ONVIF-discovered service and subscription URLs are rebased onto the configured full origin
  (`SoapTransport::rebase_url`), keeping path and query; userinfo, fragments, and non-HTTP
  schemes are rejected. Rebase the events URL before creating the subscription and the
  reference before any later call. SOAP has independent byte and XML-structure caps.
  Clock/authentication classification uses Fault code, subcode, and reason, never free-form
  Detail text.
- The `EventStats` accessors feed the daemon's `events` statistics line; renaming or
  removing one changes `iris-tools daemon-stats` and `soak-report`.

## Where things are

- `lib.rs` — crate overview and re-exports.
- `endpoint.rs` — `CameraEndpoint`.
- `sink.rs` — the `EventSink` trait.
- `feed.rs` — the CGI push feed session loop, its digest handshake, and reconnection.
- `dahua.rs` — CGI part parsing and normalisation into `Event`.
- `multipart.rs` — incremental `multipart/x-mixed-replace` reader with a part-size bound.
- `digest.rs` — HTTP digest authentication.
- `onvif.rs` — pull-point subscription loop, the shared device client and clock measurement,
  topic mapping, ISO 8601 parsing.
- `media.rs` — Media service snapshot and stream URL lookups, `MediaUrlError`.
- `discovery.rs` — bounded WS-Discovery mapping.
- `text.rs` — the text cleaning shared by discovery and the media lookups.
- `backoff.rs` — reconnection pacing.
- `stats.rs` — per-source counters and the pull-latency histogram.
- `transport.rs` — ONVIF SOAP HTTP transport with endpoint TLS policy, digest authentication,
  and the advertised-URL bound.
- `loopback.rs` — the test-only loopback ONVIF camera and its canned replies.

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
  clock; keep the host-time fallback restricted to structured clock-authentication faults.
- Once an ONVIF subscription reference exists, cancellation runs one bounded unsubscribe
  epilogue and clears connected state before the source task exits.
- Network-facing parsers must not panic on any input (root `AGENTS.md` rule 8); the crate's
  parsers have fuzz targets under `fuzz/`: `multipart` (the reader), `digest_challenge` (the
  challenge parser and the header built from it), `dahua_part` (CGI parts and their
  normalisation), `onvif_utc` (timestamps and topic mapping), `onvif_notification` (the
  notification normaliser), `onvif_soap` (the vendored SOAP/XML parser), and `discovery_parts`
  (probe answers). Run the ones that cover the parser you changed.

## Related documents

- `docs/research/05-ingest-events.md` §3, `docs/research/01-camera-probe.md`,
  `docs/research/11-multi-camera-results.md`, `docs/design/phase2-interfaces.md`.
