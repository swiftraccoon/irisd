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

The workspace uses this copy through `[patch.crates-io]` in the root `Cargo.toml`, so the
dependency version stays `0.17.0`. Remove the vendored directory and patch entry only when an
upstream release supplies every contract listed above.

## retina 0.4.21 (MIT OR Apache-2.0)

`retina/` is the published `retina` 0.4.21 crate (RTSP client, by the Retina Authors,
<https://github.com/scottlamb/retina>) with the benches and lockfile removed and these
local changes:

- `PlayOptions::recv_low_water(bytes)` sets `SO_RCVLOWAT` on the RTSP socket after `PLAY`
  succeeds on a TCP session.
- `PlayOptions::detach_from_reactor(bool)` takes the socket out of tokio's reactor for
  callers that poll the session on their own timer. Reads that would block return `Pending`
  without a waker, a short read ends the tick's reads, and writes block briefly.
- H.265 depacketization limits both pinned RTP payload and projected access-unit output to
  64 MiB, uses checked accounting for every packetization form, and releases partial access
  unit state on error. This is the ingress form of `iris-ingest`'s 64 MiB frame limit.
- Production RTSP connections limit one control message, head and body together, to 1 MiB.
  The parser applies the limit while lines and bodies accumulate, and socket reads do not
  reserve beyond the remaining allowance. Interleaved RTP keeps its protocol length limit.

The workspace uses this copy through `[patch.crates-io]` in the root `Cargo.toml`, so the
dependency version stays `0.4.21` and the patch applies to every crate in the workspace.
`retina-0.4.21-irisd.patch` carries the socket-policy changes for upstream submission.
Remove the vendored directory and patch entry only when an upstream release supplies every
local contract listed above.

Why: with a continuous interleaved RTP flow the kernel reports the socket readable on
every segment, so an `epoll`-driven client wakes once per packet. The low-water mark makes
readiness fire once per `bytes` queued instead (docs/research/12-ingest-cost.md §4).
