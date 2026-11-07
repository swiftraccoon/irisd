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
- `run_onvif_pullpoint` — creates a pull-point subscription, pulls messages in a loop, renews
  the subscription before it expires, and resubscribes with backoff on failure.
- `CameraEndpoint` — how to reach one camera's HTTP interface (host, port, TLS, credentials;
  the password is redacted from `Debug`). Hosts are bare names or IP addresses; `base_url`
  adds brackets around IPv6 when building the HTTP authority.
- `EventSink` — the trait the sources deliver to; implementations must not block.
- `EventStats` — lock-free counters per source: events, messages (heartbeats and empty pulls
  included), failed-session retry cycles, renewals, failed sessions, pull latency, time of the
  last event and message.
- `dahua` — parsing and normalisation of CGI event parts (`parse_part`, `normalize`, code and
  action mapping).
- `onvif` — topic mapping, notification normalisation, and ISO 8601 parsing for pull-point
  messages.
- `multipart` — a bounded, incremental `multipart/x-mixed-replace` reader.
- `digest` — HTTP digest authentication (RFC 7616, MD5) for cameras that require it.

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
  Redirects are never followed: a 3xx fails the session with its status and is retried with
  backoff, so a camera that redirects HTTP to HTTPS needs `https = true`.
  Multipart scanning is linear across transport fragmentation and bounds body plus syntax before
  appending.
- ONVIF: requests a 10 min subscription and renews at 80 % of the lifetime granted by the
  camera, clamped between a 0.75 s anti-spin floor and the measured 8 min cadence. Zero or tiny
  leases use the floor; missing, malformed, or reversed lease times use the 8 min fallback.
  Renewal deadlines remain active while a pull or cooldown is pending.
  `PullMessages` uses a 5 s timeout, 100 messages per pull, a 0.75 s
  cooldown, and a 20 s outer deadline because the HTTP transport bounds each request at 10 s.
  WS-Security tokens use the camera's measured clock offset. Host time is retried only after a
  structured clock-authentication fault whose code, subcode, or reason identifies the clock
  rejection; free-form Fault Detail is diagnostic only. HTTP 401 and authentication failures
  identified by those structured fields wait at least 30 s.
- Both sources apply `CameraEndpoint::tls_verify` to HTTPS calls. ONVIF uses a configurable
  SOAP transport with the same 10 s HTTP request bound and digest authentication as the
  upstream transport; normal certificate verification remains enabled unless the endpoint
  explicitly disables it. Camera-advertised service and subscription URLs are rebased onto the
  configured scheme, host, and port, keeping their path and query, so cameras behind NAT or a
  port forward, or configured by host name, still subscribe, and credentials only reach the
  configured authority; an HTTPS configuration is never downgraded. URLs with userinfo, a
  fragment, or a scheme other than HTTP(S) are rejected, and authenticated SOAP follows no
  redirects. Every SOAP response is capped at 1 MiB; XML depth, nodes, attributes, and retained
  text are independently capped at 64, 16,384, 32,768, and 1 MiB in the vendored `oxvif`
  parser.
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

Unit tests cover CGI part parsing and valid JSON normalisation (`dahua`), multipart exact limits
and linear work across fragmentation (`multipart`), digest responses and debug redaction
(`digest`), ONVIF URL rebasing, cancellation cleanup, granted leases, topic/property semantics,
structured items, and strict ISO 8601 parsing (`onvif`), backoff pacing (`backoff`), and the
counters (`stats`). Hardware validation of
both sources, including the clock-offset fallback and the 5 s pull timeout, is recorded in
`docs/research/11-multi-camera-results.md`; the daemon's `events` statistics line is
described in `docs/research/07-architecture.md §9`.

Loopback HTTP tests cover a connected server withholding attachment headers, HTTP digest
authentication, bounded SOAP bodies on every status, CGI and SOAP redirect refusal (the redirect
target receives nothing), and SOAP fault forwarding. A loopback ONVIF camera that advertises a
foreign address, or an IP address while configured by host name, is subscribed, pulled, and
unsubscribed at its configured address while a listener at the advertised one receives nothing.
A TLS fixture generates a temporary self-signed certificate and checks both certificate policies
through the ONVIF session.

The SOAP adapter uses `async-trait` (MIT OR Apache-2.0) for the locally hardened, vendored
`oxvif` 0.17.0 transport interface (MIT) and `diqwest` (MIT) to preserve digest authentication.
Test-only `rcgen` and
`tokio-rustls` (both MIT OR Apache-2.0) provide ephemeral certificates and the loopback TLS
server without external programs or stored private keys.

## Further reading

- [`docs/research/05-ingest-events.md`](../../docs/research/05-ingest-events.md) §3 — the
  event model and source priority.
- [`docs/research/01-camera-probe.md`](../../docs/research/01-camera-probe.md) — the measured
  camera behaviour the timeouts come from.
- [`docs/design/phase2-interfaces.md`](../../docs/design/phase2-interfaces.md) — the crate
  contract.
- [`STANDARDS.md`](../../STANDARDS.md) — engineering standards.
