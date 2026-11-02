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
  the password is redacted from `Debug`).
- `EventSink` — the trait the sources deliver to; implementations must not block.
- `EventStats` — lock-free counters per source: events, messages (heartbeats and empty pulls
  included), reconnects, renewals, errors, pull latency, time of the last event and message.
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
  at 256 KiB.
- ONVIF: subscription lifetime 10 min renewed every 8 min; `PullMessages` with a 5 s timeout,
  100 messages per pull, a 0.75 s cooldown between pulls, and a 20 s deadline on every call,
  because the HTTP transport underneath bounds each request at 10 s. WS-Security tokens are
  signed with the camera's measured clock offset, and with host time as the fallback for
  cameras that misreport UTC yet check tokens against their true clock.
- Reconnection backoff: 1 s doubling to 30 s with ±20 % jitter, reset after a minute of healthy
  session.
- The sources never open RTSP sessions; the per-camera limit of three RTSP sessions in the
  root `AGENTS.md` is unaffected by them, but a wrong password against ONVIF counts toward some
  cameras' lockout counters.

## Tests and measurements

Unit tests cover CGI part parsing and code mapping (`dahua`), the multipart reader whole and
byte by byte with runaway-part bounds (`multipart`), digest responses against the RFC 2617
and RFC 7616 examples (`digest`), ONVIF topic and state mapping and ISO 8601 variants
(`onvif`), backoff pacing (`backoff`), and the counters (`stats`). Hardware validation of
both sources, including the clock-offset fallback and the 5 s pull timeout, is recorded in
`docs/research/11-multi-camera-results.md`; the daemon's `events` statistics line is
described in `docs/research/07-architecture.md §9`.

## Further reading

- [`docs/research/05-ingest-events.md`](../../docs/research/05-ingest-events.md) §3 — the
  event model and source priority.
- [`docs/research/01-camera-probe.md`](../../docs/research/01-camera-probe.md) — the measured
  camera behaviour the timeouts come from.
- [`docs/design/phase2-interfaces.md`](../../docs/design/phase2-interfaces.md) — the crate
  contract.
- [`STANDARDS.md`](../../STANDARDS.md) — engineering standards.
