# Crates

The workspace, one crate per responsibility. Each crate has its own `README.md` (what it
provides, invariants, tests) and `AGENTS.md` (rules for changing it); `STANDARDS.md` at the
root applies to all of them.

| Crate | Contents |
|---|---|
| `iris-core` | ids, time units (`WallUs`, `MediaTicks`), TOML configuration, latency histograms |
| `iris-h265` | NAL unit classification and length-prefixed access-unit helpers |
| `iris-mp4` | init segment, fragment encoder, scanner, sidecar index, HLS playlists, CRC-32C |
| `iris-ingest` | RTSP sessions, timestamp regulariser, fragment assembly, live ring |
| `iris-store` | segment files, preallocation, sync policy, writer thread, recovery |
| `iris-catalog` | SQLite catalog on its own thread: segments, runs, events, retention queries |
| `iris-events` | camera event feeds: vendor CGI push and ONVIF pull-point |
| `iris-api` | HTTP API: HLS, live WebSocket, export, snapshots, events, metrics |
| `irisd` | the binary: `run`, `record`, `inspect`, `recover`, `hls`, `serve` |
| `iris-replay` | RTSP replay server: recordings as synthetic cameras for benchmarks and fault injection |
| `iris-gui` | Linux viewer: GTK 4 live grid, focus, playback timeline, event retention, thumbnails, export |
| `iris-tools` | the tools binary: log summaries, the soak sampler and report, the benchmark driver, camera probes, signed commits |

## The rest of the repository

| Path | Contents |
|---|---|
| `../third_party/` | vendored dependencies with local patches (see its README) |
| `../packaging/` | systemd unit and sysusers file |
| `../config/` | `cameras.example.toml`; local copies are gitignored |
| `../docs/research/` | design research, decisions, and dated measurement records |
| `../docs/design/` | crate contracts |
| `../.rust-reference/` | the Rust project reference pack: policies and vendored guides; `STANDARDS.md` is the digest |
| `../docs/standards/ietf/` | the protocol documents, as published |
