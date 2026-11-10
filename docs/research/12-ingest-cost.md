# 12 — Ingest cost: where the CPU goes and the read policy

Measured 2026-11-01 on the x86-64 development host (16 hardware threads), release build,
the commit that added `iris-replay`. Numbers come from `iris-tools replay-bench` driving
`irisd run` against `iris-replay` serving one 4K H.265 recording (15.5 Mbit/s, 30 fps, 2 s
GOP) to N sessions over loopback, 90 s per run with the first 20 s discarded; the daemon's
CPU and wake-ups (voluntary context switches of the runtime thread) are read from `/proc`.
Live figures come from the seven-camera soak (14 streams, 100 Mbit/s) through
`iris-tools soak-sample`.

The harness computes payload rate from differences between timestamped stream counters,
then intersects the streams' coverage with process samples after the 20 s warm-up.
Payload bytes, CPU ticks, and voluntary context switches interpolate to the same two
endpoints and divide by that interval. At least two stream statistics snapshots and a
positive shared interval are required; shorten `--stats-interval` or lengthen the run
when these are absent. Counter resets invalidate the run. Terminal cumulative byte
counts cannot be divided by a process-sample interval that begins after recording starts.
The dated tables below retain their measurement provenance; bitrate and cost per megabit
require remeasurement with this window calculation before reuse as validated budgets.

## 1. The live cost and its shape

| Source | Streams | Mbit/s | CPU % of one core (user / system) | % per Mbit/s | Wake-ups/s |
|---|---|---|---|---|---|
| Live cameras, packet-driven reads | 14 | 100 | 16.6 (7.5 / 9.4) | 0.166 | 7,211 |

The runtime thread woke about once per RTP packet (100 Mbit/s of 1,400-byte packets is
about 8,900 packets/s) and spent 13 µs of kernel time per wake-up. Kernel time exceeded
user time: the cost is the receive path, not parsing.

## 2. Harness fidelity

The replay must deliver packets the way cameras do or it understates the cost by four times:

| Replay delivery (7 sessions, 50.5 Mbit/s) | CPU % (user / system) | % per Mbit/s | Wake-ups/s |
|---|---|---|---|
| One write per frame (`--burst`) | 2.18 (1.25 / 0.94) | 0.043 | 38 |
| One write per packet, back to back | 6.55 (2.42 / 4.14) | 0.130 | 808 |
| One write per packet, spread over the frame interval (`--pace`) | 8.41 (3.29 / 5.12) | 0.167 | 959 |

Paced per-packet delivery reproduces the live cost per megabit (0.167 against 0.166) and is
the harness setting for cost measurements. The wake-up column is lower than live because
the replay's packets for all sessions leave one process in lock step and the loopback
device coalesces them; the per-megabit cost, not the wake-up count, is the figure to compare.

Replay admission fails closed before this delivery path: mounts must be CRC-clean through
exact EOF with complete access units, an initial sync sample, compatible `hvcC`, and a checked
nonoverlapping timeline; MTU is limited to 256–65,535 and packetization streams one packet at a
time. RTSP binds a session to one mount and its negotiated channels, and samples each RTCP
wall/RTP clock pair at one instant. These guards leave the valid-media packet order, per-packet
writes, pacing, and the deliberate post-stall catch-up burst used by the measurements unchanged.

## 3. Timer-driven reads (`ingest.read_interval_ms`)

Instead of reading a socket as soon as a packet arrives, each session drains what is
readable without arming a wake-up, then sleeps until the next tick of a clock shared by
every stream (05 §1). One read then covers an interval's worth of packets and all streams
are read in the same wake-up.

| Sessions | Mbit/s | Read interval | CPU % (user / system) | % per Mbit/s | Change |
|---|---|---|---|---|---|
| 7 | 50.5 | 0 (per packet) | 8.41 (3.29 / 5.12) | 0.167 | |
| 7 | 50.5 | 10 ms | 6.72 (2.68 / 4.05) | 0.133 | −20 % |
| 7 | 50.5 | 20 ms | 6.20 (2.65 / 3.55) | 0.123 | −26 % |
| 7 | 50.5 | 40 ms | 6.01 (2.68 / 3.34) | 0.119 | −29 % |
| 14 | 101 | 0 (per packet) | 14.83 (5.68 / 9.15) | 0.147 | |
| 14 | 101 | 20 ms | 11.38 (4.97 / 6.41) | 0.113 | −23 % |

Twenty milliseconds is the knee and the default: it adds 20 ms of live latency against a
300 ms budget and keeps the receive buffer under 50 KiB per stream at 18 Mbit/s. No frames
were lost or reordered in any run (0 reconnects, 0 drops, 0 loss markers), and the
two-mode integration test in `iris-ingest` records from the replay server both ways.

Command:

    iris-tools replay-bench --file RECORDING --cameras 7 --seconds 90 \
      --root SCRATCH --irisd target/release/irisd --replay target/release/iris-replay \
      --replay-arg=--pace --read-interval-ms 20

## 4. The receive low-water mark (`ingest.recv_low_water_kib`)

With timer-driven reads the thread still returned from `epoll_wait` once per packet: the
socket's edge-triggered interest stays registered, so every arriving segment woke the
runtime thread even when nothing read (the wake-up counter rose to one per packet while CPU
fell, because the dispatch was empty). `SO_RCVLOWAT` makes the kernel report a socket
readable only once at least N bytes are queued. retina 0.4.21 owns the socket and has no
option for it, so the workspace carries a patched copy under `third_party/retina` that
adds `PlayOptions::recv_low_water` and sets the option once `PLAY` succeeds on a TCP
session (set earlier, small RTSP responses would never trigger readiness).

| Sessions | Mbit/s | Read interval | Low-water | CPU % (user / system) | % per Mbit/s | Against packet-driven |
|---|---|---|---|---|---|---|
| 7 | 50.5 | 20 ms | 0 | 6.21 (2.68 / 3.54) | 0.123 | −26 % |
| 7 | 50.5 | 20 ms | 16 KiB | 4.34 (2.11 / 2.23) | 0.086 | −48 % |
| 7 | 50.5 | 20 ms | 32 KiB | 3.92 (1.88 / 2.05) | 0.078 | −53 % |
| 7 | 50.5 | 20 ms | 64 KiB | 3.52 (1.80 / 1.72) | 0.070 | −58 % |
| 14 | 101 | 20 ms | 32 KiB | 7.00 (3.48 / 3.52) | 0.069 | −53 % |
| 14 | 101 | 20 ms | 64 KiB | 6.12 (3.20 / 2.92) | 0.061 | −59 % |

User time falls too: fewer, larger reads mean fewer passes through the demuxer per byte.
No frames were lost in any run. 32 KiB is the default: it halves the cost and delays a
1 Mbit/s sub stream by a quarter second at most, within the 300 ms live budget; 64 KiB
buys another 10 % at twice that delay and suits installations without live viewing.
Wake-ups do not fall in proportion because `epoll` wakes the waiter on every segment and
only then re-evaluates the low-water mark; those empty wake-ups are cheap but not free.

Command:

    iris-tools replay-bench --file RECORDING --cameras 14 --seconds 90 \
      --root SCRATCH --irisd target/release/irisd --replay target/release/iris-replay \
      --replay-arg=--pace --read-interval-ms 20 --recv-low-water-kib 32

## 5. The user-side profile and the checksum

`perf record -g` on the daemon (14 paced sessions, 20 ms reads, 32 KiB low-water, 40 s at
997 Hz, `iris-tools replay-bench --perf`) split the samples as follows:

| Where | Share of samples |
|---|---|
| Kernel, runtime thread (TCP receive copy `rep_movs` 7.7 %, syscall entry and return 7.4 %, page zeroing 2.8 %, `epoll` 2.9 %, scheduler 4 %) | 48 % |
| Daemon code, runtime thread (retina depacketizer and RTP parsing about 9 %, tokio 3 %, clock reads 2 %) | 20 % |
| Daemon code, writer thread: CRC-32C of every fragment | 14 % |
| Kernel, writer thread (`pwritev`, page cache) | 9 % |
| libc (`memmove` of packet payloads into frames 4 %, malloc 1 %) | 7 % |

The single largest user-side item was the trailer checksum: the `crc32c` crate's
`crc32q` loop ran at about 1.5 GB/s, so checksumming 16 MB/s of payload cost more than the
whole RTSP depacketizer. Two remedies were measured:

| Checksum | 14 sessions, 101 Mbit/s | 7 sessions, 50.5 Mbit/s |
|---|---|---|
| `crc32c` crate in the writer thread | 7.00 % (3.48 / 3.52) | 3.92 % (1.88 / 2.05) |
| Running CRC at receipt on the runtime thread, one combine per fragment | 7.41 % (3.78 / 3.63) | 3.97 % (2.23 / 1.98) |
| `crc-fast` (carry-less-multiply folding) in the writer thread | 6.43 % (2.88 / 3.55) | 3.69 % (1.71 / 1.98) |

Computing the checksum at receipt did not help: the cost is the implementation, not cache
locality, and moving the work onto the runtime thread removed the writer thread's
parallelism. (Combining per-sample checksums is worse still: `crc32c_combine` is a 32×32
bit-matrix exponentiation of about 100 µs, and per sample it dominated the writer thread
at 39 % of samples.) Replacing the implementation keeps the format and the writer-thread
placement: `iris_mp4::crc` wraps `crc-fast`'s CRC-32C (iSCSI polynomial), a test checks it
against the reference crate at every alignment, and the daemon's user time fell 17 %.

## 6. The system-call mix, and two wake-ups that were not needed

Measured 2026-11-02 on the same host and harness (14 sessions, 132 Mbit/s, 20 ms reads,
32 KiB low-water mark), with `perf record -e raw_syscalls:sys_enter -p PID` over five
seconds and the daemon built with line tables for the call stacks (`strace -k` where perf's
unwinder stopped at an inlined frame):

| System call | Before | Cause | After |
|---|---|---|---|
| `epoll_wait` | 5,680 /s, 93 % returning one event | `epoll` is edge-triggered and fires once per arriving TCP segment while a socket sits above the low-water mark; the timer-driven loop never waits for readiness, so every one of these returns was unused | 111 /s |
| `write` | 605 /s | one `eventfd` write per stream per tick: each stream re-armed its own 20 ms sleep, and tokio unparks the I/O driver for every timer registered earlier than its stale next-wake estimate | 51 /s |
| `readv` | 640 /s | one read plus one `EAGAIN` per stream per tick | 700 /s |
| all | 6,420 /s | | 930 /s |

Checked on 2026-11-09: the three "Before" rows sum to 6,925 /s, more than the 6,420 /s total,
and a read plus an `EAGAIN` per stream per tick would be about 1,400 /s for 14 streams at 50
ticks a second, not 640. No perf output is kept with the record, so it cannot settle which
figure is wrong; the "After" column is consistent with its total.

The two changes, in `iris-ingest` and the vendored retina:

- **One ticker for all streams** (`iris_ingest::ReadTicker`): a single timer task wakes
  every waiting stream through a `Notify`; the daemon creates it once and hands it to every
  session through `IngestOptions::ticker`. Removes the 600 unparks per second.
- **Sockets leave the reactor once `PLAY` succeeds** (`PlayOptions::detach_from_reactor`
  in the patched retina, `third_party/`): the tokio stream turns back into a plain
  non-blocking socket, so reads that would block return `Pending` without a waker and
  nothing wakes `epoll` for the socket; the loop polls on the tick as before. Writes never
  block: a keepalive that does not fit the send buffer returns `Pending`, keeps its unsent
  bytes, and continues on later ticks, and a camera that stops reading fails the session at
  the next keepalive deadline instead of stalling the runtime thread (a blocking write would
  hold the thread for as long as the camera kept the connection open). A `TEARDOWN` on the
  connection first returns the socket to the reactor. The read path, and so the measurements
  below, are the same either way: the session writes only its keepalive, every 30 s on cam1.
  Only the timer-driven mode detaches; packet-driven reads keep the reactor.
- **One read per tick**: a detached read shorter than the space offered means the socket's
  queue is empty, so the connection answers the rest of that tick's polls with `Pending`
  instead of a read that would return `EAGAIN`; the flag clears with that `Pending`, and
  the next tick reads again.

| Build | CPU % (user / system) | % per Mbit/s | Wake-ups /s |
|---|---|---|---|
| §5 result (crc-fast), re-measured | 4.69 (1.89 / 2.80) | 0.0355 | 5,400 |
| one ticker | 4.58 (1.78 / 2.80) | 0.0347 | 5,450 |
| one ticker, detached sockets (two runs) | 2.93 (1.29 / 1.64), 2.84 (1.31 / 1.53) | 0.0222, 0.0215 | 69, 68 |
| the above, one read per tick (two runs) | 2.82 (1.24 / 1.58), 2.82 (1.29 / 1.53) | 0.0214, 0.0214 | 66, 68 |

Against the 07 §7 budget of 0.036 % per Mbit/s, ingest sits at 0.022: under budget for
the first time. The per-megabit figures here are lower than §5's 0.064 because this run
replays a 4K main stream at 132 Mbit/s with the fuzz targets running niced in the
background; compare rows within a table, not across sections.

With the detached socket the `readv` count first doubled, since a detached read ran until
`EAGAIN` where the reactor path had stopped at the readiness edge; the short-read stop
brought it to one per stream per tick, 930 system calls a second in all for fourteen
streams against 6,420 before this section.

### One clock read per socket read (2026-11-08)

retina stamped every decoded message with the wall and monotonic clock; with a read
delivering many packets, that was two clock reads per packet. The vendored copy takes
both once per socket read and stamps every message decoded from those bytes with them.
Within one read the difference was microseconds, and with timer-driven reads the stamp
already marked the read rather than the arrival, so the time model is unchanged. Measured
with the same harness (14 replayed cameras at 143 Mbit/s payload, 20 ms reads, 32 KiB
low-water mark, two runs each, the six fuzz targets running niced in the background):

| Build | CPU % (user / system) | % per Mbit/s | Wake-ups /s |
|---|---|---|---|
| before, two runs | 3.58 (1.69 / 1.89), 3.51 (1.71 / 1.80) | 0.0250, 0.0245 | 66, 69 |
| one stamp per read, two runs | 3.56 (1.65 / 1.91), 3.42 (1.51 / 1.91) | 0.0249, 0.0238 | 68, 66 |

User time fell by about 0.1 percentage points of a core, the size the §5 sample share
predicted; the totals sit within the run-to-run spread of 0.1. The figures above are
higher than §6's because this replay carries 143 rather than 132 Mbit/s and the daemon
of this round bounds and escapes every camera-supplied diagnostic; compare rows within a
table, not across sections.

## 7. What remains

The measured figure is 0.024 % per Mbit/s at 143 Mbit/s against the 07 §7 budget of
0.036 %. What is left:

1. Upstream the retina changes (the low-water mark, the detached socket, the short-read
   stop, the one stamp per read, the bounded fields and diagnostics) so the vendored copy
   can go.
2. The remaining 930 system calls a second are 700 reads, 110 `epoll_wait` returns (the
   ticker, the writer and catalog replies, the API), and the rest bookkeeping; the
   kernel's TCP copy is the floor.

Ruled out: the kernel's page zeroing (3 % of samples, writer thread) is not allocator
churn. Its call chain is `pwritev` → XFS buffered write → `folio_alloc`: the page cache
zeroes the part of each new large folio that a write does not cover, once per folio.
glibc's `trim_threshold`/`mmap_threshold` tunables changed nothing (6.55 % against 6.57 %),
and the cost is about 0.2 % of a core, inherent to buffered writes.

## 8. Resident memory (2026-11-10)

Measured on the same host with the release build of the audited tree and the harness's RSS
figure (the maximum `VmRSS` over the measurement window), 150 s per run, 20 ms reads, 32 KiB
low-water mark, thirteen fuzz targets running niced:

    iris-tools replay-bench --file RECORDING.mp4 --cameras N --seconds 150 \
      --read-interval-ms 20 --recv-low-water-kib 32 --root SCRATCH \
      --irisd target/release/irisd --replay target/release/iris-replay --replay-arg=--pace

| Replayed 4K streams | Payload | RSS |
|---|---|---|
| 1 | 10.2 Mbit/s | 17.9 MiB |
| 6 (two runs) | 61.5 Mbit/s | 47.1, 45.9 MiB |
| 14 (two runs) | 143.5 Mbit/s | 90.7, 91.1 MiB |

The figures fit about 12 MiB plus 0.55 MiB per Mbit/s. glibc's `mallinfo2`, sampled every
10 s in a fourteen-stream run through a preloaded diagnostic library (four samples, RSS
89.1 MiB), splits the part that grows:

- 28 to 35 MiB in use on the main heap, one 2 s GOP of every stream (35.9 MiB at this rate);
- 3 to 17 MiB of free heap that the allocator keeps;
- fourteen mappings of 2 MiB, 28.7 MiB together, one per stream: by size and count the
  vendored retina's read ring of each connection, which grows by powers of two and never
  shrinks (`third_party/retina/src/buf.rs`). One sample showed seven more mappings, 8.2 MiB
  together. A ring that shrank would grow again at the next key frame, so that was not tried.

So the cost per stream follows its GOP size and the size its read ring reached. Six streams
at 61.5 Mbit/s hold 46 to 47 MiB, inside the 07 §7 budget of 64 MB (61 MiB) at a tenth more
than its 56 Mbit/s.
The service's seven cameras write about 93 Mbit/s and sit at 79 MiB, where the fit gives
63 MiB. The difference was not broken down; the service also runs fourteen sessions, seven
event feeds, and the catalog with retention. Of its 81 MiB, 61 MiB is the main heap.

glibc's allocator settings trade memory for system time at fourteen streams. The defaults
stay, and the first setting below is an operator's option (`packaging/README.md`):

| `GLIBC_TUNABLES` | CPU (user / system) | RSS |
|---|---|---|
| default (four runs) | 2.89 (1.26 / 1.63), 2.81, 2.74 (1.17 / 1.57), 2.78 (1.24 / 1.54) % | 90.7, 91.1, 93.7, 92.5 MiB |
| `glibc.malloc.mmap_threshold=65536` (two runs) | 3.13 (1.30 / 1.83), 3.09 (1.27 / 1.82) % | 75.4, 76.0 MiB |
| `glibc.malloc.mmap_threshold=16384` | 3.47 (1.29 / 2.18) % | 75.8 MiB |
| `glibc.malloc.trim_threshold=262144:glibc.malloc.top_pad=65536` | 3.03 (1.27 / 1.76) % | 79.4 MiB |

The heap keeps its high-water mark, which the low-latency playlist raises: a feeder holds
three target durations of a stream for as long as a client polls it and 30 s more. With every
one of the fourteen streams polled for 40 s (`live.m3u8` every 2 s) and then left alone:

| `GLIBC_TUNABLES` | RSS before | while polled | 75 s after the last request |
|---|---|---|---|
| default | 90 MiB | 265 MiB | 163 MiB |
| `glibc.malloc.mmap_threshold=65536` | 72 MiB | 260 MiB | 108 MiB |

The service showed the same on 2026-11-10: 174 MiB five minutes after every stream had been
polled, 158 MiB of it heap, against 79 MiB before. Nothing leaks, since a later viewer reuses
the pages, but they return to the system only at a restart. Fixing the threshold at 64 KiB
sends larger frames to their own mappings, which do return, for about 0.3 points of a core at
143 Mbit/s (12 % of ingest, 0.022 % per Mbit/s, inside the 07 §7 budget).
