# Third-party sources carried in this repository

## oxvif 0.17.0 (MIT)

`oxvif/` is the published `oxvif` 0.17.0 library source with non-library targets omitted and
these local hardening changes:

- SOAP request and response traces retain action, URL, and response size metadata without
  emitting credential-verifier-bearing request bodies or camera-controlled response bodies.
- XML parsing rejects documents beyond depth 64, 16,384 nodes, 32,768 attributes, or 1 MiB of
  accumulated text before building an oversized tree.
- pull-point create and renewal responses retain the device's current and termination times.
- ONVIF `ElementItem` payloads retain their name and a bounded, namespace-normalized canonical
  representation, including significant child-adjacent mixed-content whitespace, rather than
  disappearing from notifications.
- `WsSecurityToken`'s `Debug` output redacts the username, password digest, and nonce.
- `TransportError::Authentication { kind }` names a digest-authentication step that could
  not proceed (a missing, unparseable, or unsupported challenge, or a request that could not
  be retried) without server text, instead of a fabricated HTTP 401.
- A multicast probe keeps at most `MAX_PROBE_DATAGRAMS` (256) answer datagrams and
  `MAX_PROBE_BYTES` (4 MiB) over every listener, so a host that floods the probe's source
  port cannot grow memory for the length of the wait.

The workspace uses this copy through `[patch.crates-io]` in the root `Cargo.toml`, so the
dependency version stays `0.17.0`, and excludes the directory from its members, so the crate's
own tests run standalone: `cargo test --locked --manifest-path third_party/oxvif/Cargo.toml`.
The `Cargo.lock` beside it, generated for those runs, is tracked. Remove the vendored directory
and patch entry only when an upstream release supplies every contract listed above.

## retina 0.4.21 (MIT OR Apache-2.0)

`retina/` is the published `retina` 0.4.21 crate (RTSP client, by the Retina Authors,
<https://github.com/scottlamb/retina>) with the benches removed and these local changes:

- `PlayOptions::recv_low_water(bytes)` sets `SO_RCVLOWAT` on the RTSP socket after `PLAY`
  succeeds on a TCP session. This is the one `unsafe` block the local changes add: a
  `libc::setsockopt` call with a `SAFETY:` comment, justified by
  `docs/research/12-ingest-cost.md` §4, because neither `socket2` 0.6 nor `rustix` 1.1 exposes
  `SO_RCVLOWAT`. It adds a direct `libc` dependency (MIT OR Apache-2.0); the published crate
  carries two `unsafe` blocks of its own.
- `PlayOptions::detach_from_reactor(bool)` takes the socket out of tokio's reactor for
  callers that poll the session on their own timer. Reads that would block return `Pending`
  without a waker and a short read ends the tick's reads. Writes never block: a flush that
  would block returns `Pending`, keeps the unsent bytes, and continues on a later tick, so a
  server that stops reading fails the session at the next keepalive deadline. A message is
  started only once the previous one is fully written. A detach that fails loses the socket,
  so it fails the `PLAY`.
- A `TEARDOWN` on the session's own connection first returns its socket to the reactor and
  its low-water mark to one byte, so the request and its short response wake the task and
  the 5 s attempt deadline holds. `TEARDOWN`s on fresh connections go to the address the
  session's connection reached, never to the host of an advertised `Content-Base` (which
  stays the Request-URI), so credentials reach only that server. A background `TEARDOWN`
  stops after the session timeout or 60 s, whichever is shorter, and at most
  `client::MAX_BACKGROUND_TEARDOWNS` (64) run in a process; `client::background_teardowns()`
  reports how many do. A session dropped beyond that bound is not torn down, with a warning.
- Server-supplied fields are bounded while parsing, and a longer one fails the session with
  an error naming the field: `Content-Base`, `Content-Location`, and joined control URLs at
  2,048 bytes, the `Session` identifier at 512, `WWW-Authenticate` at 4,096, and an SDP at 64
  media sections.
- Server-supplied text reaches log records and error messages only as escaped excerpts of at
  most 512 bytes, written on one line (`\n`, `\r`, `\t`, and other control and non-ASCII
  bytes escaped). Interpreting a `DESCRIBE` logs at most one warning, a count with the first
  problem, and its error quotes such an excerpt of the SDP, never the whole of it. A `SETUP`
  or `PLAY` response logs each kind of problem once, and a session logs the first request it
  receives in state `Playing` once, as its method and sizes; the stream goes on after such a
  request. The `Debug` output of `Tool` is such an excerpt, and that of `Credentials`
  redacts both fields.
- The wall and monotonic clock are read once per socket read, and every message decoded
  from the bytes that read delivered carries that time (`RtspMessageContext::received`);
  the published crate read both clocks for every message.
- SDP lines not in `type=value` form are ignored and noted rather than passed to
  `sdp-types` 0.1.8, which panics when one precedes any line but `m=`, and a media section
  without a payload type is unusable rather than a panic.
- H.265 depacketization limits both pinned RTP payload and projected access-unit output to
  64 MiB, uses checked accounting for every packetization form, and releases partial access
  unit state on error. This is the ingress form of `iris-ingest`'s 64 MiB frame limit.
- Production RTSP connections limit one control message, head and body together, to 1 MiB.
  The parser applies the limit while lines and bodies accumulate, and socket reads do not
  reserve beyond the remaining allowance. Interleaved RTP keeps its protocol length limit.
- The test helpers use the standard library instead of the `pretty_assertions` and `mylog`
  crates, so the crate's own tests run standalone:
  `cargo test --locked --manifest-path third_party/retina/Cargo.toml`. `testutil` also
  exposes `parse_describe_response` for the `describe_diagnostics` fuzz target and
  `packet_meta` for `depacketize_h265`.

The workspace uses this copy through `[patch.crates-io]` in the root `Cargo.toml`, so the
dependency version stays `0.4.21` and the patch applies to every crate in the workspace, and
excludes the directory from its members. The `Cargo.lock` beside it, generated for standalone
test runs, is tracked.
`retina-0.4.21-irisd.patch` holds every local change as a diff against the published crate,
except the removed benches and the tracked lockfile: applied with `patch -p0` to a copy of the
published source without its `benches/`, it reproduces `retina/`. Regenerate it from the
published source after changing any file here.
Remove the vendored directory and patch entry only when an upstream release supplies every
local contract listed above.

Why: with a continuous interleaved RTP flow the kernel reports the socket readable on
every segment, so an `epoll`-driven client wakes once per packet. The low-water mark makes
readiness fire once per `bytes` queued instead (docs/research/12-ingest-cost.md §4).
