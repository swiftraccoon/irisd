# iris-transport

The Linux viewer's transport: the requests of `iris-client` over reqwest and tungstenite, the
export download, and the live feed loop. `iris-client` (`crates/iris-client`) builds each
request, checks each answer, and defines the live protocol with its reconnect policy, without
input or output. This crate carries them for the Linux viewer (`crates/iris-gui`), the one crate
that depends on it. The planned iOS and macOS viewer that `docs/research/15-apple-clients.md` §4
designs carries its requests and feeds through its platform, and takes nothing from this crate.

## What it provides

- `Bearer::try_new(token)` — the root bearer, checked once. An empty token, or one with a
  character outside visible ASCII (0x21 to 0x7e), is refused with a `BearerError`, whose text
  holds no part of the token. Visible ASCII is what the daemon's configuration admits in a
  token. The length of a token is the daemon's rule and is not checked. The `Debug` output of a
  `Bearer` redacts the token, and the type has no `Display`.
- `Client::new(origin, token, tls)` — a client of the API origin. It keeps the TLS trust it is
  given, and sends every request with the `Bearer` it is given, if any, in `Authorization`. It
  follows no redirect, gives up connecting after 5 s, and waits `READ_IDLE` for an answer's head
  or its next bytes (`STREAM_LOOKUP_READ_IDLE` for a stream lookup). Its `Debug` output redacts
  the token, and a clone shares its connection pools.
- A method of `Client` for each call: `cameras`, `add_camera`, `remove_camera`, `segments`,
  `events`, `retain_event`, `keyframe`, `vod_playlist`, `discover`, and `stream_urls`. Each
  takes a `Window` where its call names a time range. It builds the request through the origin's
  builder, reads the part of the body that `HttpRequest::body_to_read` names, and returns what the
  call's check returns.
- `ClientError` — why a call failed. `Api` carries the refusal of `iris-client` and has the text
  of its `ApiError`. `Transport` says that the request or the head of its answer did not travel,
  `Read` that the body of a value did not arrive, and `Reserve` that its memory could not be
  reserved. Each of those three names the call and keeps the error it met as its source.
- `Client::export(camera, kind, window, path, progress)` — the MP4 export of a `Window`,
  streamed to an exclusive temporary beside `path`, synced, and renamed over `path`. The rename
  replaces a file or a symbolic link there without writing through it. Before the first byte and
  every further 64 MiB, the export reads the destination's free space with `fstatvfs`. It stops
  when that is below the larger of 1 GiB and 2 % of the filesystem.
- `ExportError` — why an export stopped, with the destination left as it was and the temporary
  removed. A temporary that cannot be removed stays, and is logged as "cannot remove an export
  temporary" with its path and the error.
- `Feed` and `Feed::run(sink, events, cancel)` — a stream's live WebSocket feed, pushed into a
  `LiveSink` until `cancel` fires. The bearer travels in the handshake, and a `wss` server is
  verified with the feed's `tls`. The WebSocket admits a message of at most
  `LIVE_MESSAGE_BYTES`; a longer one, or a 1009 close, ends the session `Oversized`. After each
  session the feed does what the `Reconnect` of `iris-client` answers, sends `events` the
  `FeedEvent`s the answer calls for, and logs the ending with the feed's camera. The `Debug`
  output of a `Feed` redacts the token.
- `is_sensitive_log_target(target)` — whether the events of a log target may carry the bearer.
  The tungstenite crate writes the whole handshake request of a WebSocket client at trace level,
  the feed's `Authorization` included, under `tungstenite::handshake::client`, which the
  function names. An application that installs a log subscriber must drop that target whatever
  its filter says, as the Linux viewer's output layer does.

## How it is used

```rust
use std::error::Error;
use std::sync::Arc;

use iris_client::origin::{ApiOrigin, PlainHttp};
use iris_core::id::{CameraId, StreamKind};
use iris_tls::ClientConfig;
use iris_transport::{Bearer, Client, Feed};

fn main() -> Result<(), Box<dyn Error>> {
    let origin = ApiOrigin::parse("https://nvr.example:8443", PlainHttp::LoopbackOnly)?;
    // One trust for the calls and the feeds: the client keeps a copy, and each feed shares it.
    let tls = iris_tls::client_config(None)?;
    // The Linux viewer takes its token from `IRISD_TOKEN`.
    let token = Some(Bearer::try_new("t0ken")?);
    let client = Client::new(origin.clone(), token.clone(), ClientConfig::clone(&tls));
    assert_eq!(client.origin(), &origin, "the client keeps its origin");
    let feed = Feed {
        origin,
        token,
        camera: CameraId::try_new("cam1")?,
        kind: StreamKind::Sub,
        tls: Arc::clone(&tls),
    };
    let live = "wss://nvr.example:8443/api/streams/cam1/sub/live";
    assert_eq!(
        feed.url().as_str(),
        live,
        "an https origin gives a wss feed"
    );
    Ok(())
}
```

The Linux viewer builds one client and the feed of each tile from its `--api` origin and one TLS
trust. It awaits the client's calls on its runtime, and spawns the `run` of each feed with the
tile's source as the sink.

## Invariants and limits

- The client follows no redirect, so an answer's URL is the request's. It reads a body no
  further than `HttpRequest::body_to_read` says. A refusal's message that fails to arrive is
  dropped, and the refusal stands without it. A value's body that fails to arrive fails the
  call.
- The text of a `ClientError` or an `ExportError` is the message the Linux viewer shows, and a
  failure the transport met is its source. The viewer prints it through `anyhow` with `{:#}`: the
  message, then each source after a colon.
- An export writes only to an exclusive temporary beside its destination, whose name holds 128
  random bits. The file appears at the destination only through the rename, after it is whole
  and synced. A failure removes the temporary and leaves the destination as it was, and so does
  a cancellation before the rename.
- The rename runs on the blocking pool and goes on when the export is cancelled during it. The
  destination then holds either what it held before or the whole export, never a part of one.
- The removal of a temporary runs on the blocking pool, never on a runtime worker, which the
  live feeds share. An export that stops returns once its temporary is gone, and a cancelled
  one hands the removal to the pool. A removal the pool drops unrun, as a runtime that is
  shutting down drops it, removes the file where it is dropped.
- A feed waits for room in `events` for each event, so a bounded channel loses none. It ends at
  once when a send finds the receiver closed, since no view is left for it, or when `cancel`
  fires while it waits for room.
- The feed hands each session's ending to the policy once, as a `SessionEnd`, and logs it once
  at the target `iris_transport::feed` with the feed's `camera`. The Linux viewer logs these
  lines:
  - "the camera has no live audio", when the policy stops the feed;
  - "live frame exceeds the message ceiling", with `limit_bytes` and `retry_s`, for a frame
    above the ceiling;
  - "live feed ended", for a closed session;
  - "live feed failed", with the failure and its sources as `error`, for a failed or refused
    session.
- The feed calls the policy's `delivered()` when its sink took a fragment, not when one arrived.
- A feed's bearer travels in its handshake, and one line of the handshake's trace holds it:
  tungstenite writes the whole request at trace level, under the target that
  `is_sensitive_log_target` names. A log subscriber that does not drop that target writes the
  root bearer under `RUST_LOG=trace`; the Linux viewer's drops it.
- `Client::new` panics in one case alone, which no run of a tested build reaches. The builder
  of reqwest refuses the client's options only for a TLS value that is not the
  `rustls::ClientConfig` of reqwest. The trust of `iris-tls` is that type while the lockfile
  holds one `rustls`, and a test fails a build in which it is not.
- The calls of a `Client` and `Feed::run` need a Tokio runtime with its I/O and time drivers,
  and panic when polled outside one. `Client::new` and `Bearer::try_new` need no runtime.
- The crate compiles C. The `rustls` feature of reqwest takes its cryptography from `aws-lc-rs`,
  whose `aws-lc-sys` builds C, and the `wss` fixture of the tests takes `rcgen`, whose `ring`
  builds C. `iris-client` keeps all of it out. `just cross-test`, which builds without a C
  compiler, does not run this crate's tests.

The crate's dependencies, each with why the transport takes it and the license its registry
manifest states; the workspace's crates carry the workspace's license:

| Dependency | For | License |
|---|---|---|
| `iris-client` | the requests, the checks of their answers, and the live protocol and its reconnect policy | GPL-3.0-or-later (the workspace's) |
| `iris-core` | camera ids, stream kinds, and wall-clock times | GPL-3.0-or-later (the workspace's) |
| `iris-tls` | the TLS trust of the client and the `wss` feeds | GPL-3.0-or-later (the workspace's) |
| `reqwest` | the HTTP client | MIT OR Apache-2.0 |
| `tokio-tungstenite` | the live WebSocket, with its `rustls-tls-native-roots` feature, whose `rustls` and `rustls-native-certs` (Apache-2.0 OR ISC OR MIT) the lock carries for reqwest | MIT |
| `tokio` | the runtime: the blocking pool, files, timers, and `select!` | MIT |
| `tokio-util` | the cancellation token that ends a feed | MIT |
| `futures` | the streams of an export's body and of a WebSocket | MIT OR Apache-2.0 |
| `async-channel` | the channel a feed sends its events on | Apache-2.0 OR MIT |
| `tracing` | the feed's log lines and the export's warning | MIT |
| `thiserror` | `BearerError`, `ClientError`, `ExportError`, and the session's error | MIT OR Apache-2.0 |
| `url` | the URLs of the requests and the live feed | MIT OR Apache-2.0 |
| `rustix` | the export's `fstatvfs` | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| `getrandom` | the random names of the export temporaries | MIT OR Apache-2.0 |
| `axum` (tests) | the fixture servers and their WebSockets | MIT |
| `tempfile` (tests) | the directories an export writes to | MIT OR Apache-2.0 |
| `tracing-subscriber` (tests) | the captured log and the handshake's recorded trace | MIT |
| `serde_json` (tests) | the bodies the fixtures read and answer | MIT OR Apache-2.0 |
| `rcgen` (tests) | the `wss` fixture's self-signed certificate | MIT OR Apache-2.0 |
| `tokio-rustls` (tests) | the `wss` fixture's TLS server | MIT OR Apache-2.0 |

## Tests and measurements

`cargo test -p iris-transport` runs every test of the crate. All of them run on the loopback,
and none needs a daemon or a camera. Most build the trust of the platform with
`iris_tls::client_config(None)`, which on Linux fails without the host's CA bundle.

- The tests of `src/client.rs` run each call against a fixture server. They check what the
  client sends, the bearer of every call included, what it returns through the call's check,
  and the text and source of each failure. Two raw fixtures, `fixture::serve_raw` and
  `fixture::serve_cut_short`, write an answer byte for byte, so that no HTTP library decides its
  bytes or their order. They serve a head that declares a length and sends no body, and a body
  cut short.
- The tests of `src/export.rs` run exports into temporary directories. They check what the
  destination and the temporary hold after a complete export and after each way an export
  stops. They run the free-space checks against a probe the test sets, and check the thread a
  temporary is removed on. The tests of a symbolic link at the destination and of a temporary
  that cannot be synced are Unix alone. Those of a local write that fails, on `/dev/full`, are
  Linux alone.
- The tests of `src/feed.rs` run feeds against loopback WebSocket servers, one of them `wss`
  behind a self-signed certificate. They check the handshake, the message limit, each ending
  with the pause and the events after it, a full and a closed channel, cancellation, and the
  line each ending logs.
- The tests of `is_sensitive_log_target` run a handshake with a sentinel bearer under a
  subscriber that records every target at trace level. They check that the bearer appears under
  the target the function names alone, and that a layer filtered by the function holds none.
- Doctests: the rustdoc examples, which compile without a run where they need a daemon, and the
  example of this document, which `src/lib.rs` includes under `cfg(doctest)`.
- `just mutants FILE` runs mutation testing on a source file; no mutant is missed in
  `src/client.rs`, `src/export.rs`, and `src/feed.rs`.

## Further reading

- `docs/research/15-apple-clients.md` §4 (the Linux viewer's transport), §5, and §11
- `crates/iris-client/README.md` (the requests, the checks of their answers, the live protocol,
  and the reconnect policy the transport carries)
- `crates/iris-tls/README.md` (the trust the client and the feeds verify with)
- `crates/iris-api/README.md` (the routes, and the live WebSocket's messages and its 1009 close)
- `crates/iris-gui/README.md` (the Linux viewer)
