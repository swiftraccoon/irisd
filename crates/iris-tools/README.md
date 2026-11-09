# iris-tools

The operations and research tools beside the daemon, as one binary with subcommands. They
need no hardware except the two camera probes. No tool changes the recording service or a
camera's configuration: `onvif-call` sends only `Get…` operations and the PullPoint
subscription operations, and refuses any other body before it sends a request. Passwords and
tokens come from environment variables named on the command line (`--password-env`,
`--token-env`), so no operator secret is stored in a file, shown in process listings, or kept
in shell history; `replay-bench` writes the API token it generates for the daemon under test
into its private root.

## What it provides

- `iris-tools daemon-stats LOG` — summarises an `irisd run --stats-interval` log: median,
  minimum, and maximum frames per second per stream between samples, stall intervals
  (samples under half the median), holds, gaps, and camera clock offsets, each event feed keyed
  by camera and source, the writer and catalog latency and queue peaks, catalog
  event-state/pruning/admission totals, the process CPU and RSS range with loop-lag and
  retention peaks, the first 200 `gap`, `selfcheck`, `retention pass`, and slow-I/O lines
  (each cut at 220 characters, then a count of the rest), and every other line deduplicated
  with its timestamps masked (cut at 200 characters). ANSI colour codes are stripped; a
  `journalctl -o cat` export works as input.
- `iris-tools soak-sample --out DIR --unit UNIT [--disk DEV] [--iface IF] [--subnet CIDR]
  [--no-journal] [--print] [--api URL [--token-env VAR] [--ca-file PEM] [--api-check-every MIN]
  [--kinds main,sub]]` — one host-side sample per run, meant for a systemd user timer every
  minute beside a running service. `UNIT` is a system service: its cgroup is read under
  `system.slice`, and `systemctl show` and `journalctl -u` ask the system manager. A sample
  holds the unit's state and restart count, the service cgroup's CPU user/system split, memory
  split, and bytes written per device (write amplification against the daemon's payload
  counter), `/proc/diskstats`, pressure stall information, NIC counters, a TCP retransmit and
  RTT summary for the camera subnet, the NTP offset, the process's RSS, threads, faults, and
  context switches, and the host's available, cached, dirty, and writeback memory, load, and
  uptime, as one JSON line in `host.jsonl`. The unit's journal lines since the previous run
  are appended to `journal.log` through a cursor file; the first export, with no cursor yet,
  covers only the service's running invocation, and a failed export is removed again, so the
  next run exports those lines once. A section that cannot be read becomes an `error` field,
  and a value that cannot be parsed is `null`, never a zero. API checks are off unless
  `--api-check-every` is positive; a check runs in each minute since the epoch that the value
  divides (`health`, selected catalog failure/admission metrics, cameras, the last 15 minutes
  of segments, and a 10-minute VOD playlist per stream with byte-range continuity) and lands
  in `api.jsonl`. Every request, the `/metrics` scrape included, carries the bearer token when
  `--token-env` names one, since `/metrics` requires the root token unless the daemon sets
  `[api] public_metrics`; without a token the scrape works only against public metrics. An
  `https` API behind a proxy with a private CA is verified with the platform's trust store plus
  the authorities in `--ca-file` (the `iris-tls` configuration the viewer uses), built when a
  check is due, so a trust failure is that check's `error`. The API base must be a literal
  loopback URL; redirects, oversized responses, camera ids and stream kinds the daemon would
  not accept, duplicate identities, excessive fan-out, and checks beyond the absolute request
  budget fail explicitly. A missing output directory is created with mode 0700; a directory
  already there must be owned by the caller and not writable by its group or others, and its
  mode is never changed. The directory is locked and accessed without following artifact
  symlinks.
- `iris-tools soak-report DIR [--since TIME] [--until TIME]` — turns a sampler directory into
  the soak figures: runs and restarts with downtime, process CPU and RSS trend with a slope per
  day and first- and last-hour means, loop-lag and retention peaks, writer and catalog
  latency/queue peaks, catalog event-state and admission totals, the container overhead
  between the daemon's file and payload counters, every stream (rate, stalls, reconnects,
  gaps, holds, camera clock offset range) and each event feed keyed by camera and source, gap
  totals per stream and cause with the first 50 gap lines, retention passes, self-checks
  (passed, failed, skipped), host-side figures (cgroup CPU split and memory, write
  amplification as the cgroup's bytes written over the payload received in the same sampled
  intervals, with the share of the span those intervals cover, pressure, NIC and TCP errors,
  NTP offset), the API checks, and a criteria table: soak length and stream count, restarts,
  gaps, drops and disconnections, root free space, API problems, write amplification,
  container overhead, resource trends, catalog and retention failures/rejections, writer
  errors, watchdog/self-check state, and journal problems. Recovery after a crash or power cut
  and the playback, export, and event-latency criteria of `docs/research/08-roadmap.md` need
  evidence of their own. A VOD discontinuity is expected while the checked playlist still
  spans a daemon start the journal shows; outside that bounded horizon, and after the start of
  a capture or a window, it is an API problem. retina's `unknown rtcp ssrc` warnings are
  counted apart from the journal problems: a test camera's sender reports carry an SSRC that
  does not match its stream, and the daemon's clock is authoritative.
  The startup prefix (configuration warnings, the descriptor limit, state-mode repairs,
  unavailable roots, the sandbox decision, and the catalog opening) begins the starting run;
  reconciliation after a prior run's statistics also detects a restart without a `stopped`
  line. Recovery, warnings, and cumulative counters stay separate per run. The interval from
  the preceding run's last observed line to the startup prefix includes possible downtime and
  is labelled as unobserved after an unclean stop. Windowed cumulative values start from the
  last row before `--since`, which no gauge, peak, or sample count includes, and counter
  resets do not become rates. Missing and truncated sampler series, and records that are not
  JSON with a valid `ts`, are reported rather than failing the report; a journal line that is
  not UTF-8 or has an invalid timestamp is skipped and counted. Cgroup write amplification
  stays unmeasured unless two compatible samples establish it. A journal export without
  process identities cannot establish exact crash times or distinguish attempts that emit only
  repeated unavailable-root messages; arbitrary warnings do not prove a restart. The journal
  and the sampler series are read line by line: statistics rows outside the window are dropped
  as they arrive, keeping only the last row before `--since` as the baseline, so a short window
  costs little memory however long the soak. `daemon-stats` also reads line by line. Both
  tools fail on a line longer than 1 MiB.
- `iris-tools replay-bench --file RECORDING --root SCRATCH --irisd BIN --replay BIN
  [--cameras N] [--seconds S] [--port PORT] [--api-port PORT] [--stats-interval 30s]
  [--read-interval-ms MS] [--recv-low-water-kib KIB] [--extra-arg FLAG] [--replay-arg=FLAG]
  [--perf SECONDS] [--label TEXT] [--keep]` — measures the daemon against synthetic cameras
  served by `iris-replay` from a recording, with no hardware involved, and prints one JSON
  line: payload rate, CPU user and system percent of one core, wake-ups per second, RSS,
  frames, reconnects, drops, and cost per megabit. The defaults are one camera, 120 s (25 s to
  a day), replay port 18554, API port 18600, and a 30 s statistics interval (a number with an
  `ms`, `s`, or `m` suffix, from 0.1 s to 1 h). Payload, CPU, and wake-ups use the same
  interval after 20 s of warm-up: timestamped counter snapshots determine the shared
  endpoints, and counters interpolate to those endpoints before subtraction. A run needs two
  statistics snapshots and a positive shared interval; otherwise it fails with a request to
  lengthen the run or shorten `--stats-interval`. The daemon's standard output and error share
  one descriptor of `ROOT/irisd.log`. `--perf` samples the daemon with `perf record -g`
  (through `sudo -n`, kernel frames included) after the warm-up. The tool creates
  `ROOT/perf.data` itself (exclusively, mode 0600, never through a symlink) and hands it to
  `perf record -o -` as standard output, so the privileged process resolves no path inside the
  root and nothing is `chown`ed; afterwards the tool checks through its own descriptor that the
  file is nonempty and still named `perf.data`. The file holds perf's pipe format: read it with
  `perf report -i - < ROOT/perf.data`, or convert it to the seekable format with
  `perf inject -i - -o FILE < ROOT/perf.data`. The root must be at least three components below
  `/`, not named `tmp`, `home`, `var`, `mnt`, or `target`, and neither inside nor containing the
  working directory. It must be fresh, or carry the tool's ownership marker and have mode 0700,
  and its parent must be owned by the caller or root and not writable by others without the
  sticky bit; symlinked, unmarked, and group- or other-accessible roots are rejected. Before a
  run the tool resets `run/`, `state/`, and the previous run's `bench.toml`, logs, and
  `perf.data` in the root; afterwards it deletes `run/` unless `--keep` is given. Child process
  groups have bounded shutdown, SIGINT and SIGTERM stop the run with the same cleanup, replay
  remains monitored, and a result requires the expected positive stream counters without a
  realtime clock step. Results belong in `docs/` with the date and the exact command
  (`AGENTS.md` rule 3).
- `iris-tools rtsp-probe --password-env VAR HOST_IP USER SECONDS SUBTYPE META_OUT
  [KEEPALIVE|0] [PATH|-] [SOCKET_TIMEOUT]` — speaks RTSP over TCP to the literal IPv4 or IPv6
  address `HOST_IP` on port 554 (digest authentication with the password read from the
  environment variable `VAR`), sets up the video track interleaved and the ONVIF metadata
  track when present, and streams for `SECONDS` (0.1 to 86,400). It then reports RTP
  packetisation (FU/AP/single), marker use, sequence gaps, the RTP timestamp delta histogram
  with backward steps counted, RTP clock against host clock drift (ppm), key frames and GOP
  lengths by the H.265 random-access types the daemon uses, RTCP sender-report cadence and
  NTP sanity, and ONVIF metadata traffic. The vendor's `realmonitor` path is built from
  `SUBTYPE`; `PATH` overrides it. `KEEPALIVE` sets the `GET_PARAMETER` period (default 25 s,
  0 disables it) and `SOCKET_TIMEOUT` (default 8 s) ends the run when no data arrives, which is
  how session stalls are measured. The report covers whatever arrived, also when the run stops
  on a stall or an error; a refused request, a malformed response or packet, fewer than two
  frames, or a keepalive left unanswered for the socket timeout then fails the run. Connect,
  request, response, and overall deadlines are bounded, and the end of the run is its own
  outcome rather than a stall or an error. RTSP status/CSeq, control-message limits, and every
  RTP/RTCP length are validated, the channels the camera's `SETUP` answer names are followed,
  the figures are running summaries of bounded size, and a bounded `TEARDOWN` is attempted
  after every successful setup, its failure reported beside any other. `META_OUT` receives
  camera-controlled bytes, so it must be an absent path or a caller-owned regular file with a
  single link; it is emptied when the run starts and then holds this run's sample. A symlink,
  FIFO, device, or hard-linked file is refused before anything is written. Keep it in a
  directory only the caller can write.
- `iris-tools onvif-call --password-env VAR [--no-tls-verify] USER SERVICE_URL BODY_XML LIMIT
  [OFFSET_SECONDS]` — one ONVIF SOAP call with a WS-UsernameToken password digest over the
  password in `VAR`. `BODY_XML` is one `Get…` operation or one of
  `CreatePullPointSubscription`, `PullMessages`, `Renew`, and `Unsubscribe`, whose
  subscriptions expire; any other body is refused before a request is sent. Without
  `OFFSET_SECONDS` the tool first sends the device service the unauthenticated
  `GetSystemDateAndTime` to compensate for the camera's clock offset, so a call makes at most
  two HTTP requests. Some devices misreport UTC by an hour yet check the token against their
  true clock: pass `0` to use host time, or any explicit offset in seconds. At most `LIMIT`
  bytes (1 to 4 MiB) of the response are printed, escaped for the terminal; a longer response
  is cut there and marked, and an HTTP error status is reported with the start of its body.
  `--no-tls-verify` turns certificate verification off (any certificate is accepted), for an
  `https` camera with a self-signed certificate, as the daemon does with `tls_verify = false`.
  Redirects are disabled, the clock response is bounded, camera UTC is checked, and XML text
  is escaped.
- `iris-tools gh-commit -m HEADLINE [--body TEXT] [--repo OWNER/NAME] [--branch NAME]
  [--hostname HOST] [--dry-run]` — commits the difference between the remote branch head and
  the pinned local `HEAD` through the GitHub GraphQL API (`createCommitOnBranch`, through
  `gh`), so GitHub signs the commit. Each run first plans, changing no branch and no recovery
  state; `--dry-run` prints what it would publish, recover, or adopt and stops there. Before
  adopting a published commit, the tool requires the mutation's answer to show a valid
  signature made by GitHub, exactly one parent equal to the remote head it built on, and the
  requested headline and body; a published commit that fails is reported as published but not
  adopted, keeps its recovery state, and the tool exits non-zero. Recovering a pending
  publication, or adopting a remote head that already carries the local tree, requires the
  REST API's signature verification and a single parent: the recorded remote head, or a commit
  already in the local history. It then verifies the exact tree and compare-and-swap adopts
  that OID locally. The checked-out branch and requested target must match; an identifiable
  GitHub origin must also match, while repositories without one require explicit
  host/repository arguments. Every `gh` call is host-qualified and the git fetches name the
  `origin` remote; each is bounded and supervised, output pipes included, and remote output
  is escaped before it is printed. Durable recovery state preserves a returned published OID
  until local adoption completes. The working tree and submodules must be clean: commit
  locally first, then publish. The API builds on the remote head, so a root commit cannot be
  made this way.

## How it is used

```sh
cargo build --release -p irisd -p iris-replay -p iris-tools
target/release/iris-tools daemon-stats daemon.log
target/release/iris-tools soak-sample --out SOAK_DIR --unit irisd --disk DISK --iface IFACE \
  --subnet 192.0.2.0/24 --api http://127.0.0.1:8554 --token-env IRISD_API_TOKEN --api-check-every 60
target/release/iris-tools soak-report SOAK_DIR --since 2026-01-01T00:00:00Z
target/release/iris-tools replay-bench --file RECORDING.mp4 --cameras 14 --seconds 90 \
  --root SCRATCH --irisd target/release/irisd --replay target/release/iris-replay --replay-arg=--pace
read -rs CAMERA_PASSWORD && export CAMERA_PASSWORD   # reads it without echo or shell history
mkdir -m 700 -p probe
target/release/iris-tools rtsp-probe --password-env CAMERA_PASSWORD HOST_IP user 90 0 probe/meta.xml
target/release/iris-tools onvif-call --password-env CAMERA_PASSWORD user \
  http://HOST_IP/onvif/event_service \
  '<GetEventProperties xmlns="http://www.onvif.org/ver10/events/wsdl"/>' 60000
target/release/iris-tools gh-commit -m 'fix(store): keep the sync phase across rotations'
```

`SCRATCH` lies at least three components below `/` and outside the working tree.

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

- The statistics line field names and the startup messages the daemon writes
  (`07-architecture.md §9`) are the contract between `irisd` and `daemon-stats` and
  `soak-report`; a renamed field or reworded message changes all three.
- `soak-sample` never writes anywhere but its descriptor-pinned output directory, never opens
  a camera session, and asks the API only over a literal loopback address; a failed section
  becomes an `error` field in the sample rather than a false successful measurement.
- `replay-bench` binds only loopback ports and deletes only inside its marker-owned root;
  every child is supervised as a process group and reaped on exit.
- `rtsp-probe` opens one RTSP session and tears it down; `onvif-call` makes at most two
  sequential HTTP requests, the clock read unless `OFFSET_SECONDS` is given and then the call.
  The limits in the root `AGENTS.md` (rule 6) apply to running them.
- `gh-commit` reads additions from committed Git blobs and rejects added or changed
  symlinks, executable files, and submodules: the commit API cannot preserve their modes.
  It never follows a worktree symlink to obtain publishable bytes or overwrites a branch that
  moved after planning.

## Tests and measurements

Unit tests cover terminal-safe log parsing and statistics classification, strict RFC 3339
and camera UTC parsing, playlist and RTSP/RTP/RTCP/H.265 payload bounds, output-path
confinement, report completeness and window semantics (the baseline kept out of gauges, the
restart horizon, the writer's fractional `mib` total, container overhead across daemon
versions, write amplification over matched intervals), benchmark root/config/clock safety and
the perf command line, sampler directory modes and the authenticated metrics scrape,
passwords read from the environment and redacted from `Debug`, metadata output refusing links
and FIFOs, the `onvif-call` operation check and WS-Security digest, GitHub target/mode
parsing, the gh-commit plan, signature, parent, and message checks, and the probe's receive
path end to end against `iris-replay` on loopback, a clean end at the deadline and a stall
included. Two libFuzzer targets include the parser files by path: `tools_protocol` (the
probes' camera-facing parsers) and `tools_logs` (the log parsers). Measured results are in
`docs/research/11-multi-camera-results.md` (soak, and the reports' memory, which follows the
window rather than the soak) and `12-ingest-cost.md` (bench).

## Further reading

- [`docs/research/07-architecture.md`](../../docs/research/07-architecture.md) §9 — the
  statistics lines.
- [`docs/research/12-ingest-cost.md`](../../docs/research/12-ingest-cost.md) — the
  benchmark method and results.
- [`docs/research/01-camera-probe.md`](../../docs/research/01-camera-probe.md) — the probe
  session the camera tools were written for.
- [`crates/iris-replay/README.md`](../iris-replay/README.md) — the replay server.
- [`STANDARDS.md`](../../STANDARDS.md) §9 — the commit policy `gh-commit` serves.
