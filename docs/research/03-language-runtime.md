# 03 — Language, runtime, and dependency decisions

Full notes with citations: `appendix/E-language-runtime.md`. Versions checked on crates.io
2026-10-31.

## 1. Decision

**Rust (stable, edition 2024), no C core.** Runtime: **one tokio `current_thread` runtime**
owning every socket and timer; **one dedicated `std::thread` writer per configured storage
root** doing batched `pwritev` + `fdatasync` from a bounded channel; **one `std::thread` owning
the SQLite connection**; **no io_uring**.

## 2. Why Rust and not C, C++, Zig, or Go

| Criterion | Evidence |
|---|---|
| Hostile input | The dominant CVE class in C/C++ camera and NVR stacks is parser memory corruption in exactly the layers irisd must parse: live555 RTSP/SDP (≈ 26 CVEs incl. CVE-2019-7314, CVE-2021-38381/2, CVE-2023-37117, CVE-2025-65406, CVE-2026-1200), ffmpeg `rtpdec_h264.c` sprop parsing (CVE-2017-14767), `rtpenc_h264_hevc.c` (CVE-2026-30754), hevcdec (CVE-2024-32228), GStreamer demuxers (29 CVEs from one fuzzing campaign, e.g. CVE-2024-47537), gSOAP/ONVIF "Devil's Ivy" (CVE-2017-9765), libde265 (CVE-2026-45383), ZoneMinder `zmu` (CVE-2019-6991), Hikvision discovery overflow (CVE-2025-66177), Dahua request handler (CVE-2025-31700). A camera on the LAN is an untrusted peer. |
| Ecosystem | Rust is the only non-GC language with every needed library maintained: `retina` (RTSP + H.265 depacketiser + hvcC builder, in production in Moonfire), hyper/axum/h2, quinn/h3, rustls, tokio-tungstenite, str0m/webrtc, rusqlite (bundled SQLite), quick-xml/yaserde + `oxvif`/onvif-rs, sd-notify, landlock. Only gap: no maintained fMP4 *muxer* crate; hand-roll ≈ 500 lines of box writing (Moonfire does), optionally on `mp4-atom`. |
| Proof at scale | Moonfire NVR: six 1080p30 streams under 10 % of a Raspberry Pi 2 with this exact stack. |
| Zig 0.17 (2026-10) | `std.Io` async: `Io.Evented` "experimental… does not yet implement networking", `Io.Uring` proof-of-concept; no RTSP/RTP/H.265/MP4/QUIC libraries; no temporal memory safety (the live555 bug class). Not viable for a long-lived network daemon in 2026. |
| C / C++ | live555, libavformat RTSP, GStreamer give maintained RTSP but import the CVE class above plus GLib/ffmpeg dependency trees. |
| Go (go2rtc) | memory-safe, but GC CPU scales with allocation rate and RSS ≈ 2× live heap at `GOGC=100`; cgo for SQLite. Off-target for a "near-zero CPU, minimal RSS" goal, not disqualifying. |
| C core for the hot path? | No. RTP depacketise + write is memcpy-bound: retina's single-thread benchmark does 4.6 GiB/s on an M2 against a required 0.007 GiB/s. A C core re-introduces the bug class at the one boundary facing hostile input and complicates the sandbox and aarch64 builds for an unmeasurable gain. |

Rust's own advisory record is DoS/logic class (quick-xml quadratic attribute check and
`NsReader` memory exhaustion, h2 unbounded empty DATA frames, rustls brotli cert
decompression, a tokio `Interval` race in 2026) — so irisd still needs `cargo audit`/`cargo
deny` in CI, body/XML size caps, `panic = "abort"`, per-camera task isolation, and
`cargo-fuzz` targets for every parser it owns.

## 3. Why tokio without io_uring

- Workload: 12 long-lived TCP sockets at ≈ 56 Mbps aggregate ≈ 5,000 packets/s (cam1 main
  stream alone is 707 packets/s). A readiness loop costs 2–4 `recv` + an amortised
  `epoll_wait` per frame burst at 1–2 µs each: well under 1 % of one core.
- io_uring wins (multishot recv, provided buffer rings, zero syscalls per packet, zero-copy
  rx) are built for high-connection servers and ≥ 100 Gbps NICs; benchmarks show +4–6 %
  throughput at best for echo-style loads.
- The io_uring runtimes are stale or pre-stable: tokio-uring 0.5.0 (2024-05, "still very
  young"), glommio 0.9.0 (2024-03), monoio 0.2.4 (2024-08 crate; `!Send` tasks and its own I/O
  traits are incompatible with retina/hyper), compio 0.19/0.20-beta ("not on a stable release
  rhythm"). The raw `io-uring` 0.7.15 crate is healthy if a reason ever appears.
- SQPOLL is a dedicated kernel polling thread: the opposite of near-zero CPU.
- File writes: a writer thread turns 30 frames into one `pwritev` per fragment, i.e. 0.5
  syscalls/s per stream. `tokio::fs` (a thread hand-off per operation) is wrong for per-frame
  writes; a dedicated blocking thread also gives non-blocking `fdatasync`/`fallocate` for free.
- Plain `std::thread` + blocking `read` per camera would be equally efficient for ingest but
  retina is tokio-only and HTTP/WebSocket/ONVIF need async anyway; two concurrency models is a
  maintainability loss for no gain.

Allocation and wake-up rules: frames as `bytes::Bytes` slices of large read buffers (retina
already does this), `Vec<Bytes>` → `writev`, no per-packet `Vec`. With timer-driven reads one
shared read ticker wakes every stream, and its tick also checks each session's inactivity; each
session keeps a keepalive deadline, and the event feeds and HTTP connections keep their own
deadlines. Rotation is checked when a fragment reaches the writer, not by a timer.
`current_thread` also avoids glibc per-thread arenas inflating RSS on ARM.

## 4. Crate candidates (versions and release dates checked 2026-10-31)

The candidates the phase-0 research weighed; the workspace `Cargo.toml` names the crates chosen,
and several rows here were not adopted.

| Crate | Purpose | Version | Released | Notes |
|---|---|---|---|---|
| tokio | runtime | 1.54.1 | 2026-10-21 | |
| bytes | zero-copy buffers | 1.12.1 | 2026-07-08 | |
| retina | RTSP client, H.264/H.265/ONVIF-metadata depacketisers | 0.4.21 | 2026-10-04 | **enable feature `h265`** (not default). Pin a git rev if CRA/BLA random-access detection (unreleased on main) is needed; cam1 uses IDR only |
| hevc_parser | VPS/SPS/PPS, slice header (`first_slice_segment_in_pic_flag`), 0 unsafe | 0.6.12 | 2026-08-29 | author says "mostly for my own tools": vendor or rely on retina's parser |
| bitstream-io | Exp-Golomb bit reader | 4.10.0 | 2026-04-14 | |
| mp4-atom | ISO-BMFF box codec for the hand-rolled fMP4 writer/reader | 0.17.0 | 2026-10-30 | the `mp4` crate (2023) is stale; `h265` crate is an empty placeholder |
| hls_m3u8 / m3u8-rs | playlist generation (or hand-roll) | 0.7.0 / 6.0.1 | 2026-05 / 2026-07 | |
| hyper / hyper-util / h2 | HTTP/1.1 + HTTP/2 | 1.12.0 / 0.1.21 / 0.4.20 | 2026-10 | |
| axum / tower-http | API routing | 0.8.9 / 0.7.1 | 2026-04 / 2026-08 | |
| tokio-tungstenite | WebSocket | 0.30.0 | 2026-07-11 | |
| rustls / tokio-rustls | TLS | 0.23.46 / 0.26.6 | 2026-10 / 2026-09 | |
| quinn / h3 | QUIC / HTTP/3 (later, iOS transport option) | 0.11.12 / 0.0.8 | 2026-09 / 2025-05 | h3 pre-0.1 |
| str0m / webrtc | WebRTC (later, remote live view) | 0.24.1 / 0.21.1 | 2026-10 | |
| rusqlite (`bundled`) | catalog | 0.41.1 | 2026-10-28 | |
| oxvif | ONVIF: discovery, device/media/media2/events (PullPoint, Renew, Unsubscribe), WS-Security + Digest | 0.17.0 | 2026-09-14 | 9k downloads; keep a reqwest + quick-xml escape hatch. lumeohq/onvif-rs is git-only and lacks Renew/Unsubscribe |
| quick-xml / yaserde | SOAP | 0.42.0 / 0.13.2 | 2026-08 / 2026-10 | cap input sizes |
| reqwest (rustls) | Dahua CGI, snapshots | 0.13 | | digest auth via `diqwest` or hand-rolled |
| serde / toml / clap | config, CLI | 1.0.229 / 1.1.8 / 4.6.7 | 2026 | |
| tracing / tracing-subscriber / tracing-journald | structured logs to journald | 0.1.44 / 0.3.23 / 0.3.2 | | |
| metrics | counters/gauges | 0.24.6 | 2026-05 | Prometheus exporter optional |
| sd-notify | READY/WATCHDOG | 0.5.0 | 2026-03 | zero deps |
| landlock | in-process fs/net sandbox (ABI V7 on 6.17) | 0.4.7 | 2026-07 | apply after opening dirs and binding |
| rustix | descriptor-relative filesystem access, `openat2`, `pwritev`, `fallocate`, process control | 1.1.6 | 2026-10-27 | safe Linux syscall wrappers |
| crc-fast | fragment trailers (CRC-32C; `crc32c` is the tests' oracle) | 1.10 | | hardware CRC32C |
| GUI: gtk4 (gtk4-rs), gstreamer / gstreamer-app / gst-plugin-gtk4 | Linux client | 0.25.4 / 0.15.2 | 2026-09 / 2026-05 | `gst-plugin-gtk4` is itself Rust and links statically |
| libmpv2 | fallback video widget | 7.0.0 | 2026-10-30 | |

Do not use: `tokio-uring`, `glommio`, `monoio` (stale or incompatible), `mp4` 0.14 (2023),
`h265` 0.0.0, `onvif-cam-rs` (2023), `seccompiler` (repo archived 2026-08; use systemd
`SystemCallFilter=` instead).

## 5. Build and deployment

- Profile: `panic = "abort"`, `lto = "fat"`, `codegen-units = 1`, `strip = true`; expect a
  4–8 MB static-ish binary with rustls + bundled SQLite.
- ARM64 SBCs: CI type-checks every crate but the GUI for `aarch64-unknown-linux-gnu` with the
  GNU cross toolchain (`just cross-check`), so a portability break surfaces on the change that
  makes it; release binaries for a board come from `cargo zigbuild --target
  aarch64-unknown-linux-gnu.2.28` (or musl for a static binary), cross-rs as fallback. The GUI
  needs a GTK sysroot for the target and is built on the board.
- systemd unit: `Type=notify`, `WatchdogSec=60` (ping only when every ingest task reported
  progress, so a wedged runtime is killed), `Restart=always`, static `User=irisd` via
  `sysusers.d`, `StateDirectory=irisd` for the catalog, `ReadWritePaths=` for the video root,
  and the hardening set: `ProtectSystem=strict ProtectHome=yes PrivateTmp=yes
  PrivateDevices=yes ProtectKernelTunables=yes ProtectKernelModules=yes
  ProtectKernelLogs=yes ProtectControlGroups=yes ProtectClock=yes ProtectHostname=yes
  ProtectProc=invisible ProcSubset=pid NoNewPrivileges=yes RestrictNamespaces=yes
  RestrictRealtime=yes LockPersonality=yes MemoryDenyWriteExecute=yes
  RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX SystemCallArchitectures=native
  SystemCallFilter=@system-service SystemCallFilter=~@privileged @resources
  CapabilityBoundingSet= UMask=0077 StateDirectoryMode=0700 LimitNOFILE=65536
  SyslogLevelPrefix=no IPAddressDeny=any IPAddressAllow=localhost`. That address
  policy denies every address except loopback until the operator adds the camera subnet, and the
  API clients' subnet when the API listens on the LAN, with a drop-in (`packaging/README.md`);
  the Landlock network rules below limit TCP ports for any destination address, so this policy
  is what confines destinations.
  Verify with `systemd-analyze security`. Avoid `DynamicUser=` when recordings live outside
  `StateDirectory` (UID reallocation breaks ownership).
- Leave `RestrictSUIDSGID` disabled: its systemd syscall filter rejects `openat2`, while
  descriptor-relative no-follow and beneath-root resolution require `openat2` throughout the
  recording lifecycle. `NoNewPrivileges`, an empty capability set, and read-only system paths
  retain the relevant privilege boundary.
- Landlock in-process, before the daemon spawns any thread: read the configuration directory,
  `/etc`, and `/proc`; read/write only the video roots and state dir; TCP connect only to
  camera ports; TCP bind only to the API port. Filesystem rights (Landlock ABI 1) and TCP
  bind/connect rights (ABI 4, Linux 6.7) are required: on a kernel without them, or with
  Landlock disabled at boot, `irisd run` refuses to start unless `--no-sandbox` is given.
  Rights from later ABIs up to 7 are applied when the kernel has them; a partially enforced
  ruleset is a warning naming the missing rights, and the `process` statistics line reports
  `sandbox_enforced`. The network rules limit TCP ports for every destination address (with
  `[provisioning] enabled`, the ports in `provisioning.connect_ports` are allowed too, so a camera
  added at run time may use only those); peer addresses are limited by the unit's
  `IPAddressDeny=` and `IPAddressAllow=`.
- Startup permission checks: before SQLite opens the catalog, the daemon requires the state
  directory, `irisd.db`, `-wal`, `-shm`, `capability.key`, and `cameras.toml` to belong to the
  service account (a directory, or a regular file with one link; symbolic links, hard links, and
  special files fail startup) and removes group and other permissions; SQLite copies the database
  file's mode to new WAL and shared-memory files. The configuration file draws a warning when users
  other than the service account and its group can read it. Startup also raises the soft descriptor
  limit to the hard limit.
- Linux features that matter: descriptor-relative `openat2`, `fallocate`, `fdatasync`,
  large-folio ext4 sequential-write gains,
  improved HDD readahead. Features that do not matter at this scale: io_uring zero-copy rx,
  SQPOLL, registered buffers, cachestat, sched_ext.
