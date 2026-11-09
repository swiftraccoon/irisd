# iris-events

Camera event feeds for irisd. Events come from the camera, never from decoding video on
the recorder: two sources are implemented, the Dahua-style HTTP CGI push feed and ONVIF
pull-point subscriptions. Both normalise what they receive into the shared
`iris_core::event::Event` record, place it on the recorder's clock, and hand it to an
`EventSink`, which in the daemon writes it to the catalog and fans it out to API clients.

## What it provides

- `run_dahua_cgi` — attaches to `eventManager.cgi?action=attach` with HTTP digest
  authentication, parses the `multipart/x-mixed-replace` stream, and reconnects with
  backoff; heartbeats keep the session alive and detect stalls.
- `snapshot_url` — the URL an ONVIF camera's Media service names for snapshots (the first
  profile's `GetSnapshotUri`), reached through the same transport as events: WS-Security with
  the camera's clock (host time on a clock-related refusal), the advertised URL rebased onto
  the configured camera, each call under the 20 s deadline and the whole lookup under 60 s.
  The API's snapshot route uses it for a camera whose `snapshot` method resolves to ONVIF.
- `stream_urls` and `ProfileStream` — the RTSP URLs of an ONVIF camera's media profiles
  (`GetProfiles`, then `GetStreamUri` per profile, at most eight), each moved onto the
  configured host with its advertised port, path, and query; the same transport, clock
  fallback, 20 s call deadline, and 60 s lookup deadline as `snapshot_url`. Every advertised
  URL is at most `MAX_ADVERTISED_URL_LEN` (2,048) bytes. `MediaUrlError` names what went wrong
  in both lookups, with `kind` for logs and API answers that must carry no camera text.
- `discover` and `DiscoveredCamera` — one bounded WS-Discovery probe (`oxvif`, at most 256
  answer datagrams and 4 MiB), mapped to the host and port of each camera's device service and
  its name, hardware, and location scopes: percent-decoded, control characters removed, 128
  characters at most, from the first 16 scopes and 16 service URLs of an answer; at most 64
  distinct cameras are listed. `DiscoveredCamera::from_parts` accepts any text a peer could
  send and has a fuzz target.
- `run_onvif_pullpoint` — creates a pull-point subscription, pulls messages in a loop, renews
  the subscription before it expires, and resubscribes with backoff on failure.
- `CameraEndpoint` — how to reach one camera's HTTP interface (host, port, TLS, credentials;
  both credentials are redacted from `Debug`). Hosts are bare names or IP addresses as
  `iris_core::config::is_valid_host` accepts them; `base_url` adds brackets around IPv6 when
  building the HTTP authority, and the ONVIF entry points return
  `onvif::SessionError::InvalidEndpoint` for any other host instead of building a URL from it.
- `EventSink` — the trait the sources deliver to; implementations must not block.
- `EventStats` — lock-free counters per source: events, messages (CGI parts including
  heartbeats; ONVIF successful pulls, empty ones included, so a pull carrying several
  notifications counts once), failed-session retry cycles, renewals, failed sessions, pull
  latency, time of the last event and message.
- `dahua` — parsing and normalisation of CGI event parts (`parse_part`, `normalize`, code and
  action mapping; a missing `action` reads `Pulse`, a missing or non-numeric `index` 0).
- `onvif` — the pull-point session, topic mapping, notification normalisation, and ISO 8601
  parsing for pull-point messages (a zone-less time is read as UTC).
- `media` — the Media service lookups above and `MediaUrlError`.
- `discovery` — the WS-Discovery mapping above.
- `multipart` — a bounded, incremental `multipart/x-mixed-replace` reader whose error is
  sticky: parts completed before the failure are returned first, then every push repeats it.
- `digest` — HTTP digest authentication (RFC 7616, MD5 and MD5-sess; a challenge with another
  algorithm is skipped for the next one) for cameras that require it, with random client
  nonces from `iris_core::nonce`.

## How it is used

The daemon builds one `CameraEndpoint` per camera from its configuration and spawns one task
per configured source. A sink that prints events looks like this:

```rust
use iris_core::event::Event;
use iris_events::{CameraEndpoint, EventSink, EventStats, run_dahua_cgi};
use std::sync::Arc;

struct Print;

impl EventSink for Print {
    fn publish(&self, event: Event) {
        println!("{:?} {:?} {:?}", event.camera, event.kind, event.state);
    }
}

async fn feed(endpoint: CameraEndpoint, shutdown: tokio_util::sync::CancellationToken) {
    let stats = Arc::new(EventStats::new());
    run_dahua_cgi(endpoint, Arc::new(Print), stats, shutdown).await;
}
```

Addresses in examples use the RFC 5737 range, for example `host: "192.0.2.10".into()`.

## Invariants and limits

- Every event carries the recorder's receipt time; the camera's own timestamp, when present,
  is kept beside it and never used as the time base
  (`docs/research/05-ingest-events.md §3`).
- CGI feed: heartbeat every 5 s, a stall after 2 × heartbeat + 5 s without a part, a 30 s pause
  after an authentication failure so a wrong password cannot lock the account out, parts bounded
  at 256 KiB. A 10 s deadline covers attachment and digest authentication through receipt of
  response headers; the streaming body uses an absolute deadline since the last completed part.
  Attachment answers one fresh digest challenge and one more that only reports a stale nonce;
  the 401 after that is a rejected credential, and a 401 without a digest challenge irisd can
  answer ends the attempt without the authentication pause. Redirects are never followed: a 3xx
  fails the session with its status and is retried with backoff, so a camera that redirects
  HTTP to HTTPS needs `https = true`. Multipart scanning is linear across transport
  fragmentation and bounds body plus syntax before appending.
- ONVIF: requests a 10 min subscription and renews at 80 % of the lifetime granted by the
  camera, clamped between a 0.75 s anti-spin floor and the measured 8 min cadence. Zero or tiny
  leases use the floor; missing, malformed, or reversed lease times use the 8 min fallback.
  Renewal deadlines remain active while a pull or cooldown is pending.
  `PullMessages` uses a 5 s timeout, 100 messages per pull, a 0.75 s cooldown, and a 20 s
  outer deadline because the SOAP transport bounds each request at 10 s; the unsubscribe at
  session end has 5 s. WS-Security tokens use the camera's measured clock offset. Host time is
  retried only after a structured clock-authentication fault whose code, subcode, or reason
  identifies the clock rejection (`MessageExpired`, `InvalidTimestamp`, `ClockSkew`, or the
  "time check" wording of `docs/research/11-multi-camera-results.md`); free-form Fault Detail is
  diagnostic only. HTTP 401 and authentication failures identified by those structured fields
  wait at least 30 s; a digest exchange the transport cannot complete (no challenge, or none
  irisd can answer) is reported as such and takes the ordinary backoff.
- Both sources apply `CameraEndpoint::tls_verify` to HTTPS calls, and neither uses a proxy, so
  `HTTP_PROXY` and its relatives in the daemon's environment never see camera credentials.
  ONVIF uses a configurable SOAP transport with the same 10 s HTTP request bound and digest
  authentication as the upstream transport; normal certificate verification remains enabled
  unless the endpoint explicitly disables it. Camera-advertised service and subscription URLs
  are rebased onto the configured scheme, host, and port, keeping their path and query, so
  cameras behind NAT or a port forward, or configured by host name, still subscribe, and
  credentials only reach the configured authority; an HTTPS configuration is never downgraded.
  URLs with userinfo, a fragment, or a scheme other than HTTP(S), and URLs longer than 2,048
  bytes, are rejected, and authenticated SOAP follows no redirects. Every SOAP response is
  capped at 1 MiB in a buffer that never reserves beyond that; XML depth, nodes, attributes,
  and retained text are independently capped at 64, 16,384, 32,768, and 1 MiB in the vendored
  `oxvif` parser.
- Camera text reaches a log line only through `iris_core::text::LogSafe` (one escaped line of
  at most 512 characters): ONVIF faults and errors, topics, and CGI codes and actions. Only
  `MediaUrlError::kind` reaches logs and API answers for the media lookups.
- Cancellation after subscription performs one unsubscribe attempt bounded to 5 s and clears
  connected state. Reconnection backoff is 1 s doubling to 30 s with independently seeded
  ±20 % jitter, reset only after a minute of actual connection.
- Event payloads are valid JSON text. Invalid vendor text is quoted, structured ONVIF
  `ElementItem` content (including child-adjacent mixed-content whitespace) is retained without
  interpreting its analytics schema, Dahua indexes are retained, and an encoding beyond 64 KiB
  becomes `{"truncated":true}`.
- The sources never open RTSP sessions; the per-camera limit of three RTSP sessions in the
  root `AGENTS.md` is unaffected by them, but a wrong password against ONVIF counts toward some
  cameras' lockout counters.

## Tests and measurements

Unit tests cover CGI part parsing and valid JSON normalisation (`dahua`), multipart exact limits,
linear work across fragmentation, and the sticky error (`multipart`), digest responses against
the RFC 2617 and RFC 7616 vectors, the skipped SHA-256 challenge of RFC 7616 §3.9.1, quoted-value
escaping, random client nonces, and debug redaction (`digest`), ONVIF URL rebasing, cancellation
cleanup, granted leases, topic/property semantics, structured items, clock-fault classification
including the hardware wording, bounded error lines, and ISO 8601 parsing (`onvif`), the media
lookups' failure paths through a mocked transport: no Media service, no profile, unusable and
mixed URLs, the eight-profile cap, the host-time retry, and the 60 s deadline under paused time
(`media`), the camera-list bound (`discovery`), backoff pacing (`backoff`), and the counters
(`stats`). Hardware validation of both sources, including the clock-offset fallback and the 5 s
pull timeout, is recorded in `docs/research/11-multi-camera-results.md`; the daemon's `events`
statistics line is described in `docs/research/07-architecture.md §9`.

Loopback HTTP tests cover a connected server withholding attachment headers, the CGI digest
handshake (a fresh challenge, a stale nonce, a second fresh challenge, a `Basic`-only camera),
bounded SOAP bodies on every status, a SOAP 401 with only a `Basic` challenge, CGI and SOAP
redirect refusal (the redirect target receives nothing), and SOAP fault forwarding. A loopback
ONVIF camera that advertises a foreign address, whether the recorder is configured by IP address
or by host name, is subscribed, pulled, and unsubscribed at its configured address while a
listener at the advertised one receives nothing. A TLS fixture generates a temporary self-signed
certificate and checks both certificate policies through the ONVIF session. The `fuzzing`
feature exposes the multipart reader's work counter to the `multipart` fuzz target and nothing
else.

The SOAP adapter uses `async-trait` (MIT OR Apache-2.0) for the locally hardened, vendored
`oxvif` 0.17.0 transport interface (MIT) and `diqwest` (MIT) to preserve digest authentication.
Test-only `rcgen` and
`tokio-rustls` (both MIT OR Apache-2.0) provide ephemeral certificates and the loopback TLS
server without external programs or stored private keys.

## Further reading

- [`docs/research/05-ingest-events.md`](../../docs/research/05-ingest-events.md) §3 — the
  event model and source priority; §5 — discovery and the media lookups.
- [`docs/research/11-multi-camera-results.md`](../../docs/research/11-multi-camera-results.md)
  — hardware validation of both sources and of the lookups.
- [`docs/research/01-camera-probe.md`](../../docs/research/01-camera-probe.md) — the measured
  camera behaviour the timeouts come from.
- [`docs/design/phase2-interfaces.md`](../../docs/design/phase2-interfaces.md) — the crate
  contract.
- [`STANDARDS.md`](../../STANDARDS.md) — engineering standards.
