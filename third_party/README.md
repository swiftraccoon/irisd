# Third-party sources carried in this repository

## retina 0.4.21 (MIT OR Apache-2.0)

`retina/` is the published `retina` 0.4.21 crate (RTSP client, by the Retina Authors,
<https://github.com/scottlamb/retina>) with the benches and lockfile removed and two
additions: `PlayOptions::recv_low_water(bytes)`, which sets `SO_RCVLOWAT` on the RTSP socket
once `PLAY` succeeds on a TCP session, and `PlayOptions::detach_from_reactor(bool)`, which
then takes the socket out of tokio's reactor for callers that poll the session on their own
timer (reads that would block return `Pending` without a waker, a short read ends the
tick's reads, writes block briefly). The workspace uses it through `[patch.crates-io]` in
the root `Cargo.toml`, so the dependency version stays `0.4.21` and the patch applies to
every crate in the workspace. The diff against the published crate is
`retina-0.4.21-irisd.patch`, kept for upstream submission; once an upstream release
carries the option, delete this directory and the patch entry.

Why: with a continuous interleaved RTP flow the kernel reports the socket readable on
every segment, so an `epoll`-driven client wakes once per packet. The low-water mark makes
readiness fire once per `bytes` queued instead (docs/research/12-ingest-cost.md §4).
