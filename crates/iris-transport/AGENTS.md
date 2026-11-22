# AGENTS.md — iris-transport

Scope: this crate only. Read the root `AGENTS.md` first. This crate is the Linux viewer's
transport. It carries the requests that `iris-client` (`crates/iris-client`) builds and the
answers that `iris-client` checks, downloads an export, and runs the live feed loop that the
reconnect policy of `iris-client` drives. Keep the rules in `iris-client`, which does no input
or output. Keep the fetching here, and the toolkit in the viewer (`crates/iris-gui`), the one
crate that depends on this one.

## Invariants

- The crate's behaviour is the Linux viewer's: every request it sends, every log line, every
  reconnect. `iris-client` writes the texts of its refusals
  (`crates/iris-client/src/api/error.rs`), and this crate writes only those of the transport's
  failures. Each failure it reports through `ClientError` (`Transport`, `Read`, `Reserve`) or
  `ExportError` is one variant. The variant's text is the message the viewer shows, and a
  failure the transport met is its source. Keep the texts byte for byte and `git grep` them
  before a change.
- The viewer prints a failure through `anyhow` with `{:#}`: the message, then each source after
  a colon. `ClientError::Api` is `#[error(transparent)]`, so a refusal of `iris-client` has the
  text of its `ApiError` and that error's source. The error of a live session is private;
  `failure_text` builds its text, the error and each source after a colon, as `anyhow` writes a
  chain.
- The requests, the statuses and limits of each call, and the checks of their answers belong to
  `iris-client` (`crates/iris-client/src/api/`). A change of what a call sends or accepts
  belongs there and in its tests, not here.
- `Client::new` panics in no run of a tested build. The `build` of reqwest refuses the options
  of `http_client` only for a TLS value that is not the `rustls::ClientConfig` of reqwest.
  `iris_tls::ClientConfig` is that type while the lockfile holds one `rustls`, which
  `the_shared_trust_is_the_tls_configuration_reqwest_takes` checks. An upgrade that gives
  `iris-tls` and reqwest two versions of `rustls` fails that test; keep them on one rather than
  make the panic an error no caller can act on.
- An option added to the builder that reqwest can refuse at run time (a user agent, a header, a
  proxy, a certificate) makes the constructor fallible: it is then `try_new` (`STANDARDS.md`
  §3).
- The client reads a body as `HttpRequest::body_to_read` says, along one path for every call
  (`Client::fetch`). A refusal's message that fails to arrive is dropped and the refusal stands.
  A value that fails to arrive fails the call with `ClientError::Read`. An export's body stays
  unread until its head passes, and then streams to the file. The client follows no redirect,
  so an answer's final URL is the request's.
- An export writes only an exclusive temporary beside its destination (`.irisd-export-`, 32
  random hexadecimal digits, and `.tmp`). It renames the temporary over the destination once
  the file is whole and synced. A name already taken is tried again under another, up to 16
  tries, and any other failure to create the temporary ends the export.
- An export reads the free space with `fstatvfs` on a duplicate of the temporary's descriptor,
  on the blocking pool. It does so before the first byte and once the bytes written reach each
  further 64 MiB.
- No teardown of a temporary runs on a runtime worker, which the live feeds share
  (C-DTOR-BLOCK). An export that stops, after a failed commit too, awaits
  `ExportTemporary::discard`, which closes the file and removes it on the blocking pool. The
  export therefore returns with the temporary gone, as
  `a_discarded_temporary_is_removed_on_the_blocking_pool` tests. A commit closes the file on
  the pool once the rename has placed it.
- A temporary dropped without a commit or a discard, as a cancellation drops it, hands its
  `Removal` to the blocking pool when a runtime is current, and removes the file at once when
  none is. That holds for one dropped while its blocking open runs too. A `Removal` the pool
  drops unrun, as a runtime that is shutting down drops it, removes the file where it is
  dropped. The four paths have one test each, in that order:
  `a_dropped_temporary_is_removed_on_the_blocking_pool`,
  `a_temporary_dropped_where_no_runtime_runs_is_removed_at_once`,
  `a_temporary_cancelled_while_it_is_created_is_removed`, and
  `a_removal_the_pool_refuses_removes_the_file_where_it_is_dropped`.
- A removal that fails for any reason but the file being gone logs "cannot remove an export
  temporary" with `path` and `error`, the line the viewer logs.
- The feed loop (`Feed::run`) follows the `live::Reconnect` of `iris-client`
  (`crates/iris-client/src/live/reconnect.rs`). It calls `delivered()` when its sink took a
  fragment, and then `after()` with how the session ended. It announces `FeedEvent::Live` once
  the next session's sink takes its first fragment while `is_backing_off()` is true. A change of
  the policy belongs in `iris-client`.
- The tests `an_oversized_stream_backs_off_instead_of_reconnecting_every_two_seconds` and
  `an_audio_feed_the_daemon_does_not_have_ends` drive the loop through the policy, so a change
  of an answer changes what the viewer waits for and shows.
- The loop sends each `FeedEvent` with `send().await`, never `try_send`. The view clears its
  oversized state on `Live` alone, so a full channel makes the feed wait, and the session's
  reading with it, rather than lose an event. Every send races `cancel` (`deliver`, and the
  session inside the loop's `select!`), so a cancelled feed never waits for room.
- A send that finds the receiver closed ends the feed at once, since no view is left to restart
  or show anything. For `Live` that is mid-session (`Ending::Unheard`, which has no
  `SessionEnd`). The Linux viewer's receivers close only when its main loop drops its futures.
- These tests check the sends: `a_feed_whose_receiver_closed_ends_after_a_closed_session`,
  `a_feed_whose_receiver_closed_ends_after_a_frame_above_the_ceiling`,
  `a_feed_whose_receiver_closed_ends_as_it_announces_video`,
  `the_return_of_video_waits_for_room_in_a_full_channel`,
  `a_feed_cancelled_while_it_waits_for_room_ends`, and
  `a_feed_cancelled_while_it_waits_to_report_an_oversized_frame_ends`. The last two race the
  send of a restart and of an oversized frame against `cancel`.
- The loop hands each ending to the policy once, as a `SessionEnd`, and logs it once. The line
  is "the camera has no live audio" when the policy stops the feed, and "live frame exceeds the
  message ceiling" for an `Oversized` end. Otherwise the session's outcome decides: "live feed
  ended" for a closed session, and "live feed failed" for a failed or refused one.
- The feed's lines carry the target `iris_transport::feed` and the feed's `camera`, with the
  texts the viewer logs. `a_closed_session_logs_its_line`,
  `a_frame_above_the_ceiling_logs_its_line`, `an_audio_feed_that_ends_logs_its_line`, and
  `a_refused_payload_fails_the_session_with_the_sinks_reason` check each line. The texts of the
  `LiveError` of `iris-client` are the session's failures:
  `an_unknown_message_type_fails_the_session` matches "unknown live message type".
- The WebSocket admits a message of at most the `LIVE_MESSAGE_BYTES` of `iris-client`, the
  value of the daemon's `MAX_LIVE_MESSAGE_BYTES`, so a payload at the ceiling fits a sink that
  queues `LIVE_PAYLOAD_BYTES`. A longer message, or a 1009 close, ends the session `Oversized`.
- The feed's bearer travels in each session's WebSocket handshake. The tungstenite crate writes
  the whole serialized request, `Authorization` included, at trace level under the target
  `tungstenite::handshake::client`, the one line of the handshake's trace with the bearer.
  `is_sensitive_log_target` names that target and no other.
- The Linux viewer's output layer drops what `is_sensitive_log_target` names, whatever
  `RUST_LOG` says. Any other application that installs a log subscriber over the transport must
  drop it too, or `RUST_LOG=trace` logs the root bearer (root `AGENTS.md` rule 2).
- The tests `tungstenite_traces_the_bearer_under_the_sensitive_target_alone`,
  `a_layer_filtered_by_the_guard_holds_no_bearer`, and
  `a_layer_filtered_by_the_guard_keeps_the_handshakes_other_lines` run a real handshake with a
  sentinel bearer, under a subscriber that records every target at trace level. The first finds
  the bearer under that target alone. The other two find a layer filtered by the guard without
  the bearer and with the handshake's other lines. A tungstenite that writes the request
  elsewhere fails the first before any viewer logs the bearer.
- `Bearer` is the root bearer, checked once. `Bearer::try_new` refuses an empty token and any
  character outside visible ASCII (0x21 to 0x7e), before any call sends it. Those are the
  characters the daemon's configuration admits in a token (`crates/iris-core/src/config.rs`).
  It does not check the length, which is the daemon's rule.
- `Bearer` redacts the token in `Debug` and has no `Display`, and the text of `BearerError`
  holds no part of the token. `Client` and `Feed` keep the `Bearer` they are given and redact it
  in `Debug` too.

## Where things are

- `src/lib.rs` — the crate documentation, which declares `client`, `export`, and `feed` and
  re-exports their public items, declares the fixtures of the tests, and compiles the example of
  `README.md` as a doctest.
- `src/client.rs` — `Bearer` and `BearerError`, `Client` with its calls, `ClientError`, the reads
  of a body that `HttpRequest::body_to_read` calls for, and their tests against fixture servers.
- `src/fixture.rs` — the fixtures the tests of the three files share, each called through the
  module (`fixture::client`, `fixture::logged`). They are the loopback server, a client of the
  server without a bearer, the platform's trust (`fixture::trust`), and a bearer
  (`fixture::bearer`). They also make a camera id, an event id (`fixture::event_id`), a time, a
  window (`fixture::window`), and an MP4 answer. The module also has a server that writes an
  answer byte for byte (`fixture::serve_raw`), one whose answer is cut short
  (`fixture::serve_cut_short`), and the captured log.
- `src/export.rs` — `Client::export`, `ExportError`, the free-space probe and reserve, and the
  exclusive temporary: the name bytes a test may choose (`create_named`), its commit, its
  discard, and the `Removal` its drop hands the blocking pool. Their tests are there too.
- `src/feed.rs` — `Feed` with its loop `run`, the session with what it gives back (`Session`)
  and its private error, and `is_sensitive_log_target`. Their tests run over loopback
  WebSockets, with the `RecordingSink` they push into and the `Recorder` that keeps every event
  of a handshake.

## How to verify a change

```sh
cargo fmt -p iris-transport
cargo clippy -p iris-transport --all-targets -- -D warnings
cargo test -p iris-transport    # against fixture servers and WebSockets on the loopback
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-transport --no-deps
just mutants crates/iris-transport/src/client.rs crates/iris-transport/src/export.rs \
    crates/iris-transport/src/feed.rs    # after a change there: none missed
```

The Linux viewer builds on the crate, so a change here is checked there too, on a host with the
GTK 4 and GStreamer development libraries. `just check` runs the workspace's clippy and tests.
`just display-test` runs the viewer's display checks, whose `playback_origin` test finds the
refusal of each of three playlists in the viewer's log.

## Pitfalls

- A fixture body that fails at once fails no read of a body. The hyper crate closes the
  connection before it writes the head, and the call fails with `ClientError::Transport`. The
  last source of that error is the `IncompleteMessage` kind of hyper, "connection closed before
  message completed" (`hyper::Error::is_incomplete_message`). A fixture meant to fail the read
  of a body writes the head and the body's first bytes on a raw connection and then closes it,
  as `fixture::serve_cut_short` does. No HTTP library then decides their order.
- The target of a `tracing` line is its module, so the transport's lines are
  `iris_transport::*`: `RUST_LOG=iris_transport=debug` shows the feed's debug lines.
- The tungstenite and tokio-tungstenite crates write through `log`, not `tracing`. Their records
  reach a subscriber only through the `log` bridge that `init`, `try_init`, and `set_default` of
  `tracing_subscriber` install for the whole process. The first two install it at the level of
  the subscriber's filter, and the third at every level.
- `tracing::subscriber::set_global_default`, which the test log `fixture::logged` uses, installs
  no bridge. The guard's test installs one through `set_default`. Once it has run, the records
  of the dependencies at debug level and above reach that log as well, so each test that reads
  the log looks for a word no other test logs.
- A bridged record is an event of the target `log` that carries the record's target in the
  field `log.target`, and a filter reads the record's target.
- The platform verifier refuses an untrusted certificate in the system's words. On Linux the
  reason is `UnknownIssuer`. On macOS it is a `CertificateError::Other` with the system's
  description and status code, which `rustls-platform-verifier` gives for a trust failure it has
  no named reason for. For the self-signed certificate of the `wss` test, that text ends
  "certificate is not trusted: -67843"; the code is `errSecNotTrusted`. The test therefore pins
  the Linux reason on Linux alone.
- `{:#}` of a `thiserror` error is its `Display` alone; the chain a viewer shows comes from the
  `{:#}` of `anyhow` or from walking `source()`, as `failure_text` does.

## Related documents

- `docs/research/15-apple-clients.md` §4, §5, and §11.
- `crates/iris-client/README.md`, the requests, the checks of their answers, and the live
  protocol and reconnect policy this crate carries.
- `crates/iris-tls/README.md`, the trust the client and the feeds verify with.
- `crates/iris-api/README.md`, the route table the requests name and the live WebSocket's
  messages.
- Dependents: `iris-gui`.
