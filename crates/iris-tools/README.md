# iris-tools

The operations and research tools beside the daemon, as one binary with subcommands. They
need no hardware except the two camera probes, and every tool is read-only towards cameras
and the recording service except where its name says otherwise. Passwords and tokens come
from environment variables named on the command line (`--password-env`, `--token-env`), so
nothing secret is stored in a file, shown in process listings, or kept in shell history.

## What it provides

- `iris-tools daemon-stats LOG` — summarises an `irisd run --stats-interval` log: median,
  minimum, and maximum frames per second per stream between samples, stall intervals
  (samples under half the median), holds, gaps, and camera clock offsets, each event feed keyed
  by camera and source,
  the writer and catalog latency and queue peaks, catalog event-state/pruning/admission
  totals, the process CPU and RSS range with loop-lag and
  retention peaks, every `gap`, `selfcheck`, `retention pass`, and slow-I/O line, and every
  other line deduplicated. ANSI colour codes are stripped; a `journalctl -o cat` export works
  as input.
- `iris-tools soak-sample --out DIR --unit UNIT [--disk DEV] [--iface IF] [--subnet CIDR]
  [--api URL --token-env VAR --api-check-every MIN]` — one host-side sample per run, meant
  for a systemd user timer every minute beside a running service: the unit's state and
  restart count, the service cgroup's CPU user/system split, memory split, and bytes written
  per device (write amplification against the daemon's payload counter), `/proc/diskstats`,
  pressure stall information, NIC counters, a TCP retransmit and RTT summary for the camera
  subnet, the NTP offset, and the process's RSS, threads, faults, and context switches, as
  one JSON line in `host.jsonl`; the unit's journal lines since the previous run appended to
  `journal.log` through a cursor file; and on the configured cadence an API check (`health`,
  selected catalog failure/admission metrics, cameras, the last 15 minutes of segments, and a
  10-minute VOD playlist per stream with byte-range continuity) in `api.jsonl`. Every
  request, the `/metrics` scrape included, carries the bearer token when `--token-env`
  names one, since `/metrics` requires the root token unless the daemon sets
  `[api] public_metrics`; without a token the scrape works only against public metrics. The
  API base must be
  a literal loopback URL; redirects, oversized responses, duplicate identities, excessive
  fan-out, and checks beyond the absolute request budget fail explicitly. A missing output
  directory is created with mode 0700; a directory already there must be owned by the caller
  and not writable by its group or others, and its mode is never changed. The directory is
  locked and accessed without following artifact symlinks.
- `iris-tools soak-report DIR [--since TIME] [--until TIME]` — turns a sampler directory
  into the soak figures: runs and restarts with downtime, process CPU and RSS trend with a
  slope per day, loop-lag and retention peaks, writer and catalog latency/queue peaks,
  catalog event-state and admission totals, every
  stream (rate, stalls, reconnects, gaps, holds, camera clock offset range) and each event feed
  keyed by camera and source,
  every gap with its cause, retention passes, self-checks, host-side figures (cgroup CPU
  split and memory, write amplification as the cgroup's bytes written over the payload bytes
  received, container overhead, pressure, NIC and TCP errors, NTP offset), the API checks,
  and a criteria table covering restarts, gaps, drops/disconnections, resource trends,
  catalog and retention failures/rejections, writer errors, watchdog/self-check state, and API
  problems. A VOD discontinuity is expected while the checked playlist still spans a logged
  daemon restart; outside that bounded horizon it is an API problem. Platform-specific playback
  validation and filesystem comparison remain separate phase-2 evidence.
  Unavailable-root startup warnings, sandbox decisions, and catalog-open messages begin
  the starting run's prefix; reconciliation after a prior run's statistics also detects
  a restart without a `stopped` line. Recovery, warnings, and cumulative counters stay
  separate per run. The interval from the preceding run's last observed line to the
  startup prefix includes possible downtime and is labelled as unobserved after an
  unclean stop. Windowed cumulative values use a pre-window baseline and counter resets do
  not become rates. Missing, corrupt, or truncated sampler series are reported; cgroup write
  amplification stays unmeasured unless two compatible samples establish it. A journal
  export without process identities cannot establish exact crash
  times or distinguish attempts that emit only repeated unavailable-root messages;
  arbitrary warnings do not prove a restart. The journal and the sampler series are read
  line by line: statistics rows outside the window are dropped as they arrive, keeping
  only the last row before `--since` as the baseline and one row after `--until`, so a
  short window costs little memory however long the soak. `daemon-stats` also reads line
  by line. Both tools fail on a line longer than 1 MiB.
- `iris-tools replay-bench --file RECORDING --cameras N --seconds S --root SCRATCH
  --irisd BIN --replay BIN [--replay-arg=--pace] [--read-interval-ms MS]
  [--recv-low-water-kib KIB] [--extra-arg FLAG] [--perf SECONDS] [--label TEXT] [--keep]`
  — measures the daemon against synthetic cameras served by `iris-replay` from a recording,
  with no hardware involved, and prints one JSON line: payload rate, CPU user and system
  percent of one core, wake-ups per second, RSS, frames, reconnects, drops, and cost per
  megabit. Payload, CPU, and wake-ups use the same interval after 20 s of warm-up:
  timestamped counter snapshots determine the shared endpoints, and counters interpolate
  to those endpoints before subtraction. A run needs two statistics snapshots and a
  positive shared interval; otherwise it fails with a request to lengthen the run or
  shorten `--stats-interval`. `--perf` samples the daemon with
  `perf record -g` (through `sudo -n`, kernel frames included) after the warm-up. The tool
  creates `ROOT/perf.data` itself (exclusively, mode 0600, never through a symlink) and hands it to
  `perf record -o -` as standard output, so the privileged process resolves no path inside
  the root and nothing is `chown`ed; afterwards the tool checks through its own descriptor
  that the file is nonempty and still named `perf.data`. The file holds perf's pipe format:
  read it with `perf report -i - < ROOT/perf.data`, or convert it to the seekable format with
  `perf inject -i - -o FILE < ROOT/perf.data`. The root must be fresh, or carry the tool's
  ownership marker and have mode 0700, and its parent must be owned by the caller or root and
  not writable by others without the sticky bit; broad, overlapping, symlinked, unmarked, and
  group- or other-accessible roots are rejected. Child
  process groups have bounded shutdown, replay remains monitored, and a result requires the
  expected positive stream counters without a realtime clock step. Only the root's `run`
  recordings are deleted afterwards unless `--keep` is given. Results belong in `docs/`
  with the date and the exact command
  (`AGENTS.md` rule 3).
- `iris-tools rtsp-probe --password-env VAR HOST USER SECONDS SUBTYPE META_OUT [KEEPALIVE|0]
  [PATH|-] [SOCKET_TIMEOUT]` — speaks RTSP over TCP (digest authentication with the password
  read from the environment variable `VAR`), sets up the video track
  interleaved and the ONVIF metadata track when present, then reports RTP packetisation
  (FU/AP/single), marker use, sequence gaps, the RTP timestamp delta histogram, RTP clock
  against host clock drift (ppm), GOP lengths, RTCP sender-report cadence and NTP sanity,
  and ONVIF metadata traffic. The vendor's `realmonitor` path is built from `SUBTYPE`;
  `PATH` overrides it. `KEEPALIVE` sets the `GET_PARAMETER` period (default 25 s, 0 disables
  it) and `SOCKET_TIMEOUT` (default 8 s) ends the run when no data arrives, which is how
  session stalls are measured. Connect, request, response, and overall deadlines are bounded;
  RTSP status/CSeq, control-message limits, and every RTP/RTCP length are validated, and a
  bounded `TEARDOWN` is attempted after every successful setup. `META_OUT` receives
  camera-controlled bytes, so it must be an absent path or a caller-owned regular file with a
  single link, which is replaced; a symlink, FIFO, device, or hard-linked file is refused
  before anything is written. Keep it in a directory only the caller can write.
- `iris-tools onvif-call --password-env VAR USER SERVICE_URL BODY_XML LIMIT [OFFSET_SECONDS]`
  — one ONVIF SOAP call with a WS-UsernameToken password digest over the password in `VAR`,
  compensating for the camera's
  clock offset by first calling the unauthenticated `GetSystemDateAndTime`. Some devices
  misreport UTC by an hour yet check the token against their true clock: pass `0` to use
  host time, or any explicit offset in seconds. Redirects are disabled, both HTTP stages must
  succeed within bounded response sizes, camera UTC is checked, and XML text is escaped.
- `iris-tools gh-commit -m HEADLINE [--body TEXT] [--repo OWNER/NAME] [--branch NAME]
  [--hostname HOST] [--dry-run]` — commits the difference between the remote branch head and
  the pinned local `HEAD`
  through the GitHub GraphQL API (`createCommitOnBranch`, through `gh`), so GitHub signs the
  commit. Before adopting it, the tool requires the mutation's answer to show a valid
  signature made by GitHub, exactly one parent equal to the remote head it built on, and the
  requested headline and body; a published commit that fails is reported as published but
  not adopted, keeps its recovery state, and the tool exits non-zero. Recovering a pending
  publication, or adopting a remote head that already carries the local tree, requires the
  REST API's signature verification and a single parent: the recorded remote head, or a
  commit already in the local history. It then verifies the exact tree and
  compare-and-swap adopts that OID locally. The
  checked-out branch and requested target must match; an identifiable GitHub origin must also
  match, while repositories without one require explicit host/repository arguments. Every `gh`
  and network Git operation is host-qualified, bounded, and supervised. Durable recovery state
  preserves a returned published OID until local adoption completes. The working tree and
  submodules must be clean: commit locally first, then publish. The API builds on the remote
  head, so a root commit cannot be made this way.

## How it is used

```sh
cargo build --release -p irisd -p iris-replay -p iris-tools
target/release/iris-tools daemon-stats daemon.log
target/release/iris-tools soak-sample --out /srv/soak --unit irisd --disk sda --iface eth0 \
  --subnet 192.0.2.0/24 --api http://127.0.0.1:8554 --token-env IRISD_API_TOKEN --api-check-every 60
target/release/iris-tools soak-report /srv/soak --since 2026-01-01T00:00:00Z
target/release/iris-tools replay-bench --file RECORDING.mp4 --cameras 14 --seconds 90 \
  --root SCRATCH --irisd target/release/irisd --replay target/release/iris-replay --replay-arg=--pace
read -rs CAMERA_PASSWORD && export CAMERA_PASSWORD   # reads it without echo or shell history
mkdir -m 700 -p probe
target/release/iris-tools rtsp-probe --password-env CAMERA_PASSWORD CAM user 90 0 probe/meta.xml
target/release/iris-tools onvif-call --password-env CAMERA_PASSWORD user \
  http://CAM/onvif/event_service \
  '<GetEventProperties xmlns="http://www.onvif.org/ver10/events/wsdl"/>' 60000
target/release/iris-tools gh-commit -m 'fix(store): keep the sync phase across rotations'
```

`iris-replay` on its own: `iris-replay --listen 127.0.0.1:8554 --mount main=recording.mp4`
serves `rtsp://127.0.0.1:8554/main` to at most `--max-connections` (default 256) clients at
once; a listener outside loopback needs `--allow-remote`, since the server has no
authentication; `--synth cam` serves a synthetic 1080p stream without a
file; `--drop-every N` and `--stall-after 30s --stall-for 10s` inject faults; `--once` stops
at the end of the file instead of repeating it. By default every RTP packet is written with
its own system call, as cameras send one TCP segment per packet, which reproduces the
receiver's per-packet wake-ups over loopback; `--burst` writes a whole frame at once and
`--pace` spreads a frame's packets over the frame interval as a camera on a slow link
delivers them. Pass replay flags through the driver as `--replay-arg=--pace`.

## Invariants and limits

- The statistics line field names the daemon writes (`07-architecture.md §9`) are the
  contract between `irisd` and `daemon-stats` and `soak-report`; a renamed field changes
  all three.
- `soak-sample` never writes anywhere but its descriptor-pinned output directory, never opens
  a camera session, and asks the API only over a literal loopback address; a failed section
  becomes an `error` field in the sample rather than a false successful measurement.
- `replay-bench` binds only loopback ports and removes only the marker-owned scratch root's
  recording subtree; every child is supervised as a process group and reaped on exit.
- The two camera probes open one RTSP session or one HTTP request and tear it down; the
  limits in the root `AGENTS.md` (rule 6) apply to running them.
- `gh-commit` reads additions from committed Git blobs and rejects added or changed
  symlinks, executable files, and submodules: the commit API cannot preserve their modes.
  It never follows a worktree symlink to obtain publishable bytes or overwrites a branch that
  moved after planning.

## Tests and measurements

Unit tests cover terminal-safe log parsing, strict RFC 3339 and camera UTC parsing, playlist
and RTSP/RTP bounds, output-path confinement, report completeness and window semantics (the
writer's fractional `mib` total included), benchmark root/config/clock safety and the perf
command line, sampler directory modes and the authenticated metrics scrape, passwords read
from the environment and redacted from `Debug`, metadata output refusing links and FIFOs,
ONVIF time and digest, GitHub target/mode parsing, and the gh-commit signature, parent, and
message checks. A
libFuzzer target exercises the camera UTC, RTP, and RTSP response parser seams. The reports
were checked line for line
against the earlier Python implementations over the same soak window before those were
removed. Measured results are in `docs/research/11-multi-camera-results.md` (soak) and
`12-ingest-cost.md` (bench).

Report memory, measured 2026-11-07 with debug builds on a copy of a soak sampler directory
spanning 2026-10-31 to 2026-11-07 (`journal.log` 140,890,767 bytes in 268,713 lines,
`host.jsonl` 20,031,867 bytes) as the maximum resident set from `/usr/bin/time -f %M`,
before and after line-by-line reading; the reports were byte-identical. The `--since` window
covers the last 3.1 hours.

| Command | Whole files read | Line by line |
|---|---:|---:|
| `iris-tools soak-report --since 2026-11-07T17:31:15Z DIR` | 826,784 KiB | 28,300 KiB |
| `iris-tools soak-report DIR` | 923,608 KiB | 923,540 KiB |
| `iris-tools daemon-stats DIR/journal.log` | 698,972 KiB | 695,324 KiB |

The whole-soak report and `daemon-stats` keep every statistics row of the window they
summarise, and those rows, not the input text, set their peak.

## Further reading

- [`docs/research/07-architecture.md`](../../docs/research/07-architecture.md) §9 — the
  statistics lines.
- [`docs/research/12-ingest-cost.md`](../../docs/research/12-ingest-cost.md) — the
  benchmark method and results.
- [`docs/research/01-camera-probe.md`](../../docs/research/01-camera-probe.md) — the probe
  session the camera tools were written for.
- [`crates/iris-replay/README.md`](../iris-replay/README.md) — the replay server.
- [`STANDARDS.md`](../../STANDARDS.md) §9 — the commit policy `gh-commit` serves.
