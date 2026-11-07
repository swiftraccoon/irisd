# Appendix E — Language and runtime evaluation notes (unedited, 2026-10-31)

Research notes produced by the language/runtime thread. Tags: **VP** verified-primary ·
**S** secondary · **I** inferred/estimate. Versions/dates from crates.io API and GitHub API on
2026-10-31. Summarised in `../03-language-runtime.md`.

## 1. Language

**1.1 Parser memory-corruption bugs are the dominant CVE class in C/C++ camera/NVR stacks, in exactly the layers irisd must parse.** live555: ~26 CVEs incl. UAFs in RTSP SETUP/teardown (CVE-2019-7314, CVE-2021-38381/38382, CVE-2023-37117), heap overflow in SDP construction (CVE-2025-65406, Dec 2025), still arriving in 2026 (CVE-2026-1200). ffmpeg libavformat: heap overflow parsing SDP `sprop-parameter-sets` in rtpdec_h264.c (CVE-2017-14767); negative-size memcpy in rtpenc_h264_hevc.c (CVE-2026-30754, Sep 2026); hevcdec OOB (CVE-2024-32228). GStreamer: 29 CVEs from one fuzzing campaign, mostly qtdemux/matroska (OOB write CVE-2024-47537). gSOAP "Devil's Ivy" CVE-2017-9765: stack overflow in the SOAP parser behind ONVIF on 249 Axis models (S). libde265 heap OOB in WPP slice decode (CVE-2026-45383). ZoneMinder `zmu` stack overflow (CVE-2019-6991, CVSS 9.8). NUUO NVRMini2 stack overflow (CVE-2018-1149). Hikvision NVR/DVR/IPC LAN-discovery buffer overflow (CVE-2025-66177, 8.8, Jan 2026). Dahua IPC buffer overflow, reported as the ONVIF request handler (CVE-2025-31700, 8.1; ONVIF detail S). https://notcve.org/vendors/live555/ · https://cveawg.mitre.org/api/cve/CVE-2017-14767 · https://github.blog/security/vulnerability-research/uncovering-gstreamer-secrets/ · https://cveawg.mitre.org/api/cve/CVE-2025-66177 · https://thehackernews.com/2025/07/critical-dahua-camera-flaws-enable.html **VP**

**1.2 Rust is the only non-GC language with every required library existing and maintained.** retina (RTSP; in production in Moonfire NVR; H.265 since 0.4.12, 2025-01-28; in-tree VPS/SPS/PPS parsing and hvcC builder in `src/codec/h265/record.rs`; IRAP detection; ring-buffer reads since 0.4.20), hyper/h2/axum, quinn/h3, rustls, tokio-tungstenite, str0m/webrtc, rusqlite bundled, quick-xml/yaserde + onvif-rs, sd-notify, landlock. Prior art: Moonfire NVR "handles six 1080p/30fps streams on a Raspberry Pi 2, using less than 10% of the machine's total CPU" (tokio+retina+rusqlite, H.265 since v0.7.19). Gap: no maintained fMP4 *muxer* (alfg/mp4 last release 2023-08-01, last push 2024-06; re_mp4 read-only; mp4-atom is a low-level box codec, release 2026-10-30) → hand-roll ~500 lines as Moonfire does, optionally on mp4-atom. https://github.com/scottlamb/retina/blob/main/CHANGELOG.md · https://github.com/scottlamb/moonfire-nvr **VP**

**1.3 Rust's own advisory record is DoS/logic class, not memory corruption — so limits and fuzzing are still required.** 2026 RUSTSEC: quick-xml quadratic attribute check and NsReader memory exhaustion (2026-0194/0195); h2 unbounded empty DATA frames (2026-0258); rustls brotli cert decompression up to 1 GiB pre-auth (2026-0348); tokio `Interval` race (2026-0384). Use `cargo audit`/`cargo deny`, body/XML size caps, cargo-fuzz on every parser. https://rustsec.org/packages/quick-xml.html **VP**

**1.4 Zig (0.17.0, 2026-10-01) is not viable for this daemon in 2026.** std.Io landed in 0.16 (Apr 2026): only `Io.Threaded` is "feature-complete"; `Io.Evented` is "work-in-progress, experimental" and "does not yet implement networking"; `Io.Uring` is "proof-of-concept… lacking Networking". 0.17 notes change nothing here (only `Semaphore.waitTimeout`); package management remains roadmap item 8/9 to 1.0; 0.17 broke ZLS; the notes say "working on a non-trivial project using Zig may require participating in the development process". No RTSP/RTP/H.265/MP4/QUIC libraries found (only a 0-star "zonic" pinned to Zig 0.13); TLS server only via third-party tls.zig (S). Safety: bounds/overflow checks in safe modes, no temporal (UAF) protection — the live555 bug class. https://ziglang.org/download/0.16.0/release-notes.html · https://ziglang.org/download/0.17.0/release-notes.html **VP** (status) / **S** (ecosystem)

**1.5 C/C++ libraries (live555, libavformat RTSP, GStreamer rtspsrc) deliver maintained RTSP but import the bug class in 1.1**; live555 is a single-threaded event loop with its own scheduler, libavformat pulls the ffmpeg dependency tree, GStreamer adds GLib + plugin loading. **VP** (CVEs) / **I** (footprint)

**1.6 Go (go2rtc) is memory-safe but off-target: GC CPU scales with allocation rate ("doubling GOGC will double heap memory overheads and roughly halve GC CPU cost"), RSS ≈ 2× live heap at GOGC=100, cgo for SQLite.** https://go.dev/doc/gc-guide **VP** (quote) / **I**

**1.7 Cross-compilation is solved:** `cargo zigbuild --target aarch64-unknown-linux-gnu.2.28` (zig as linker; `*-musl` for static), or cross-rs (active 2026-10-30). Bundled SQLite compiles through `zig cc` (I). https://github.com/rust-cross/cargo-zigbuild **VP**

## 2. Rust runtime

**2.1 At 12 sockets / 5 MB/s the I/O path costs ~1–3k syscalls/s ≈ <1% of one core; io_uring cannot show a measurable win.** Estimate: 6 cams × 2 streams × ~25 fps ≈ 300 frame bursts/s; readiness loop ≈ 2–4 `recv` + amortised `epoll_wait` per burst, ~1–2 µs each (I). Retina's single-thread H.264 benchmark does 4.6 GiB/s on an M2 — ~1000× headroom. Benchmarks: 1-connection echo io_uring 176k vs epoll 169k req/s (+4%); Netty 200 conns +6.5% throughput with epoll 21% better p99. io_uring's wins (multishot recv, provided buffer rings, zero syscalls/packet) target high-connection servers. https://github.com/joakimthun/io-uring-echo-server · https://netty.io/news/2020/11/16/io_uring-0-0-1-Final.html · https://github.com/axboe/liburing/wiki/io_uring-and-networking-in-2023 **S** / **I**

**2.2 io_uring runtimes are stale or pre-stable; tokio is what the media crates target.** tokio 1.54.1 (2026-10-21). tokio-uring 0.5.0 (2024-05-27; README "still very young"; last push 2025-07-07). glommio 0.9.0 (2024-03-25; push 2026-08-31). monoio 0.2.4 (2024-08-20; repo active 2026-10-16 but 26 months without a release; !Send tasks and own I/O traits, incompatible with retina/hyper). compio 0.19.2 (2026-08-18) + 0.20.0-beta.1; README: "isn't on a stable release rhythm yet". Raw `io-uring` 0.7.15 (2026-09-07) is healthy if ever needed. **VP**

**2.3 Recommended model: tokio `current_thread` (or `multi_thread`, `worker_threads=2` on ≥4-core boxes) owning all sockets; one dedicated `std::thread` per storage disk doing batched `writev` + `fdatasync` from a bounded channel; SQLite via `spawn_blocking`.** Tokio task = "a single allocation and 64 bytes". `tokio::fs` is a thread handoff per op — wrong for per-frame writes; a writer thread batches access units (20–40 KB) into ≥1 MB writes → tens of syscalls/s. An idle tokio runtime parks in `epoll_wait` until the next timer; no periodic tick (I). https://tokio.rs/tokio/tutorial/spawning **VP** / **I**

**2.4 Allocation:** use retina's `Demuxed` API (frames as `Bytes`); never `tokio::io::copy` per packet; writer takes `Vec<Bytes>` → `writev`; `fallocate` segments; single-threaded runtime also avoids glibc per-thread arenas inflating RSS on ARM. **I**

**2.5 Timers:** one keepalive per session (retina: half session-timeout), one idle deadline per stream reset on data (not per packet), one segment-rotation timer per stream, one watchdog ping; `Interval` + `MissedTickBehavior::Delay`. <20 timers total. **VP** (retina v0.4.1) / **I**

**2.6 std::thread + blocking read per camera is equally efficient for ingest (12 threads, ~16–64 KiB touched stack each) but not simpler overall:** retina is tokio-only, and HTTP/WS/HLS/ONVIF need async anyway, so you'd maintain two concurrency models plus `SO_RCVTIMEO`/`shutdown()` cancellation. Maintainability call, not performance. **I**

## 3. Recommended crates — see table below.

## 4. Deployment & hardening

**4.1 Unit:** `Type=notify NotifyAccess=main WatchdogSec=30 Restart=always`; static `User=irisd` via sysusers.d; `ReadWritePaths=/srv/irisd` (HDD) + `StateDirectory=irisd` (SQLite); `ProtectSystem=strict ProtectHome=yes PrivateTmp=yes PrivateDevices=yes ProtectKernelTunables=yes ProtectKernelModules=yes ProtectKernelLogs=yes ProtectControlGroups=yes ProtectClock=yes ProtectHostname=yes ProtectProc=invisible ProcSubset=pid NoNewPrivileges=yes RestrictNamespaces=yes RestrictRealtime=yes RestrictSUIDSGID=no LockPersonality=yes MemoryDenyWriteExecute=yes RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX SystemCallArchitectures=native SystemCallFilter=@system-service SystemCallFilter=~@privileged @resources CapabilityBoundingSet= UMask=0077 IPAddressDeny=any IPAddressAllow=<LAN> localhost` (IPAddress* in systemd.resource-control(5), S). `RestrictSUIDSGID` stays disabled because systemd 259 rejects `openat2` under that filter, while root admission and descriptor-relative storage require it; `NoNewPrivileges` and the empty capability set retain the privilege boundary. Check with `systemd-analyze security`. MDWE is fine (no JIT). DynamicUser= makes ownership of a persistent HDD tree awkward across reallocated UIDs; use it only if all data lives under StateDirectory (I). https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html **VP**

**4.2 Watchdog:** `READY=1` after sessions start; `WATCHDOG=1` at ≤ WatchdogSec/2 only when ingest tasks reported progress, so a wedged runtime is killed. sd-notify 0.5.0 (zero deps). https://www.freedesktop.org/software/systemd/man/latest/sd_notify.html **VP**

**4.3 Logging:** tracing + tracing-journald 0.3.2 (structured fields, no log files). **VP**

**4.4 In-process sandbox:** Landlock ABI V7 on 6.17 (fs + TCP bind/connect + ioctl + scoped signals/abstract-unix + audit; V8 needs 7.0); landlock 0.4.7 enforces best-effort compat; apply after binding/opening dirs. seccomp: rust-vmm/seccompiler standalone repo archived 2026-08-18 (moved to rust-vmm monorepo; crate 0.5.0, 2025-03); libseccomp 0.4.0 (FFI, 2025-04). Pragmatic: systemd `SystemCallFilter=` for seccomp, Landlock in-process. https://docs.rs/landlock/latest/landlock/enum.ABI.html · https://github.com/rust-vmm/seccompiler **VP**

**4.5 Build:** cargo-zigbuild, `panic=abort lto=fat codegen-units=1 strip`, `-C target-cpu` for the SBC; expect ~4–8 MB binary with rustls+SQLite (I). **VP**/**I**

## 5. Linux 6.17 features

**5.1 Matter:** `RWF_DONTCACHE` uncached buffered I/O (base 6.14; "ext4: Support uncached buffered I/O" listed in 6.17) — write through the page cache, folios dropped after writeback: no cache pollution, no O_DIRECT alignment rules; `fallocate` for contiguous segments; `fdatasync`/`sync_file_range` cadence per segment; `posix_fadvise(DONTNEED)` fallback. Verify rustix exposes RWF_DONTCACHE (unverified) else `libc::pwritev2`. https://kernelnewbies.org/Linux_6.17 · https://lwn.net/Articles/998783 **VP**/**S**

**5.2 Don't matter:** io_uring zero-copy rx (6.15; needs NIC flow steering into dedicated HW queues, SINGLE_ISSUER/DEFER_TASKRUN/CQE32; built for ≥100 Gbps); SQPOLL (dedicated kernel polling thread — the opposite of near-zero CPU); registered buffers; 6.17 multishot-recv len cap / vectorized send; sendfile/splice (hyper cannot use them; a user-space copy at 5 MB/s ≈ 0.1% CPU); cachestat (metrics only); MADV_* (no mmap); sched_ext (6.12; only if you write a BPF scheduler). https://docs.kernel.org/6.17/networking/iou-zcrx.html · https://man7.org/linux/man-pages/man2/io_uring_setup.2.html **VP** (semantics) / **I** (relevance)

## 6. C core for the hot path?

**No.** RTP depacketize → write is memcpy-bound and already runs at GiB/s in safe Rust (retina 4.6 GiB/s single-thread vs 0.005 GiB/s required). Real CPU at this scale is TLS for API clients, SQLite fsync and page-cache writeback — none helped by C. A C core re-introduces the 1.1 bug class at the one boundary facing hostile input, adds an `unsafe` FFI seam, complicates aarch64 builds and sandbox reasoning. Gain: unmeasurable. Loss: the safety goal. **I** (grounded on 1.1, 2.1)

## Decision and rationale
**Rust; tokio `current_thread` (opt-in 2 workers); one std writer thread per disk; SQLite on `spawn_blocking`; no io_uring** (revisit only if profiling shows >5% CPU in syscalls). Zig lacks async networking and every needed library; C/C++ libraries carry the exact CVE class; Go's GC/RSS violate the goal. Moonfire NVR proves the stack at the target scale.

## Recommended crates

| crate | purpose | version | last release | maintained? |
|---|---|---|---|---|
| tokio | runtime | 1.54.1 | 2026-10-21 | yes |
| bytes | zero-copy buffers | 1.12.1 | 2026-07-08 | yes |
| retina | RTSP client, H.264/H.265/ONVIF-metadata depacketizers | 0.4.21 | 2026-10-04 | yes (scottlamb) |
| rtp / rtcp / sdp (webrtc-rs) | if bypassing retina | 0.17.2 | 2026-07-20 | yes |
| h264-reader | H.264 NAL parsing (retina dep) | 0.9.0 | 2026-09-14 | yes |
| hevc-parser | VPS/SPS/PPS/slice (`first_slice_in_pic_flag`), 0 unsafe | 0.6.12 | 2026-08-29 | yes, but "mostly for my own tools" — vendor or rely on retina |
| h265 | — | 0.0.0 | 2017-09-19 | **placeholder, do not use** |
| bitstream-io | exp-Golomb bit reader | 4.10.0 | 2026-04-14 | yes |
| mp4-atom | ISO-BMFF box encode/decode for a hand-rolled fMP4 muxer | 0.17.0 | 2026-10-30 | yes |
| mp4 (alfg) | — | 0.14.0 | 2023-08-01 | **stale** |
| re_mp4 | read-only | 0.5.1 | 2026-07-08 | yes (not a muxer) |
| hls_m3u8 / m3u8-rs | playlist gen (or hand-roll) | 0.7.0 / 6.0.1 | 2026-05-06 / 2026-07-30 | yes |
| hyper / hyper-util / h2 | HTTP/1.1+2 | 1.12.0 / 0.1.21 / 0.4.20 | 2026-10-06 / 09-24 / 10-06 | yes |
| axum / tower-http | API routing | 0.8.9 / 0.7.1 | 2026-04-14 / 08-31 | yes |
| tokio-tungstenite | WebSocket | 0.30.0 | 2026-07-11 | yes |
| fastwebsockets | alt. WS | 0.10.0 | 2025-01-14 (0.8.1 backport 2026-01) | slower |
| rustls / tokio-rustls | TLS | 0.23.46 / 0.26.6 | 2026-10-14 / 09-28 | yes |
| quinn / h3 / h3-quinn | QUIC/HTTP3 (later) | 0.11.12 / 0.0.8 / 0.0.10 | 2026-09-14 / 2025-05-06 | yes; h3 pre-0.1 but repo active 2026-10 |
| str0m / webrtc | WebRTC (later) | 0.24.1 / 0.21.1 | 2026-10-03 / 10-11 | yes |
| rusqlite (bundled) | SQLite index | 0.41.1 | 2026-10-28 | yes |
| quick-xml / yaserde | ONVIF SOAP | 0.42.0 / 0.13.2 | 2026-08-22 / 10-28 | yes |
| lumeohq/onvif-rs (git only) | ONVIF client, schema-generated, events example, 0 unsafe | — | push 2026-08-12 | moderate |
| serde / toml / clap | config/CLI | 1.0.229 / 1.1.8 / 4.6.7 | 2026-07 / 10 / 09 | yes |
| tracing / tracing-subscriber / tracing-journald | logs | 0.1.44 / 0.3.23 / 0.3.2 | 2025-12 / 2026-03 / 2025-11 | yes |
| metrics | counters | 0.24.6 | 2026-05-13 | yes |
| sd-notify | READY/WATCHDOG | 0.5.0 | 2026-03-09 | yes |
| libsystemd | heavier alt. | 0.7.2 | 2025-04-30 | slow |
| landlock | fs/net sandbox | 0.4.7 | 2026-07-27 | yes |
| seccompiler | seccomp BPF | 0.5.0 | 2025-03-07 | repo archived → rust-vmm monorepo |
| libseccomp | seccomp FFI alt. | 0.4.0 | 2025-04-05 | slow |
| rustix | fallocate/fadvise/sync_file_range | 1.1.6 | 2026-10-27 | yes |
| io-uring | raw ring (only if ever needed) | 0.7.15 | 2026-09-07 | yes |
| tokio-uring / glommio / monoio / compio | — | 0.5.0 / 0.9.0 / 0.2.4 / 0.19.2 | 2024-05 / 2024-03 / 2024-08 / 2026-08 | not recommended |

## Anti-patterns to avoid
- `tokio::fs` or `tokio::io::copy` per frame/packet; per-packet `Vec` allocation.
- Default multi_thread worker count on a 4-core SBC (idle wakeups, glibc arenas → RSS).
- SQPOLL or io_uring "because faster"; O_DIRECT.
- `mp4` (stale), `h265` (placeholder), taking `hevc-parser` as a hard dependency.
- Unbounded HTTP/XML bodies from cameras (quick-xml NsReader DoS); treat ONVIF peers as hostile.
- `panic=unwind` + `unwrap` in parsers — one camera can crash the daemon; use `panic=abort`, fuzzing, per-camera task isolation with backoff restart.
- DynamicUser with data outside StateDirectory.

## Open questions
- Retina vs Dahua IPC H.265 quirks (0.4.21 fixed Dahua *NVR* SSRC radix); test main+sub streams and ONVIF metadata early.
- fMP4: hand-roll vs mp4-atom; `hvc1`+hvcC required for iOS AVPlayer; HLS vs WebSocket-fMP4 for the Linux GUI.
- ONVIF events: PullPoint (onvif-rs, git-only) vs WS-BaseNotification push (camera-initiated HTTP conflicts with IPAddressAllow).
- Recording filesystem: ext4 vs XFS re DONTCACHE/fallocate; rustix RWF_DONTCACHE exposure unverified.
- Could not verify: Dahua CVE-2025-31700 ONVIF-handler attribution (CNA text is generic); Zig third-party TLS/HTTP library maturity; tokio idle-timer behaviour (inferred from design).
