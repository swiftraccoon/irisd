# Crates

The workspace, one crate per responsibility. Each crate has its own `README.md` (what it
provides, invariants, tests) and `AGENTS.md` (rules for changing it); `STANDARDS.md` at the
root applies to all of them.

| Crate | Contents |
|---|---|
| `iris-core` | ids, time units (`WallUs`, `MediaTicks`), TOML configuration, latency histograms |
| `iris-h265` | NAL unit classification and length-prefixed access-unit helpers |
| `iris-mp4` | init segment, fragment encoder, scanner, sidecar index, HLS playlists, decoder configuration, CRC-32C |
| `iris-ingest` | RTSP sessions, timestamp regulariser, fragment assembly, live ring |
| `iris-store` | segment files, preallocation, sync policy, writer thread, recovery |
| `iris-catalog` | SQLite catalog on its own thread: segments, runs, events, retention queries |
| `iris-events` | camera event feeds: vendor CGI push and ONVIF pull-point |
| `iris-api` | HTTP API: HLS, live WebSocket, export, snapshots, events, metrics |
| `irisd` | the binary: `run`, `record`, `inspect`, `clocks`, `recover`, `hls`, `serve` |
| `iris-replay` | RTSP replay server: recordings as synthetic cameras for benchmarks and fault injection |
| `iris-tls` | the TLS client trust the viewer and the tools share: platform roots plus an optional CA file |
| `iris-client` | the rules a viewer applies to the daemon's API and to what the daemon serves, without input or output. The crate contains the API origin, the API's requests and the checks of their answers, the answer types and validators, and the VOD playlist rules. It contains the timeline model and the playback selection, which gives each playlist body the name a player requests. It contains the media time of a live fragment and the rule that places a camera's live sound against its picture. It contains the live feed's message framing, reconnect policy, and byte budget. It contains the live decoder, which reads a live message into a decoder configuration or one sample. The Linux viewer uses the crate, which is shaped for the planned iOS and macOS viewer |
| `iris-transport` | the Linux viewer's transport: the requests of `iris-client` over reqwest and tungstenite, the export download, and the live feed loop |
| `iris-gui` | Linux viewer: GTK 4 live grid, focus, playback timeline, event retention, thumbnails, export |
| `iris-tools` | the tools binary: log summaries, the soak sampler and report, the benchmark driver, camera probes, signed commits, the standards checks, the workspace's documentation |

## The rest of the repository

| Path | Contents |
|---|---|
| `../third_party/` | vendored dependencies with local patches (see its README) |
| `../packaging/` | systemd unit, sysusers file, and the install guide |
| `../fuzz/` | cargo-fuzz targets for the parsers, with their own workspace, lockfile, and pinned nightly |
| `../config/` | `cameras.example.toml`; local copies are gitignored |
| `../docs/research/` | design research, decisions, and dated measurement records |
| `../docs/design/` | crate contracts |
| `../.rust-reference/` | the Rust project reference pack: policies and vendored guides; `STANDARDS.md` is the digest |
| `../docs/standards/ietf/` | the IETF's protocol and address-range documents, as published |
| `../.github/` | CI workflows (check, MSRV, audit, fuzz build, aarch64 clippy, references; rustdoc to GitHub Pages) and the weekly action updates |
