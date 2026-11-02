# 12 — Ingest cost: where the CPU goes and the read policy

Measured 2026-11-01 on the x86-64 development host (16 hardware threads), release build,
the commit that added `iris-replay`. Numbers come from `iris-tools replay-bench` driving
`irisd run` against `iris-replay` serving one 4K H.265 recording (15.5 Mbit/s, 30 fps, 2 s
GOP) to N sessions over loopback, 90 s per run with the first 20 s discarded; the daemon's
CPU and wake-ups (voluntary context switches of the runtime thread) are read from `/proc`.
Live figures come from the seven-camera soak (14 streams, 100 Mbit/s) through
`iris-tools soak-sample`.

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

## 3. Timer-driven reads (`ingest.read_interval_ms`)

Instead of reading a socket as soon as a packet arrives, each session drains what is
readable without arming a wake-up, then sleeps until the next tick of a clock shared by
every stream (05 §2). One read then covers an interval's worth of packets and all streams
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

The two changes, in `iris-ingest` and the vendored retina:

- **One ticker for all streams** (`iris_ingest::ReadTicker`): a single timer task wakes
  every waiting stream through a `Notify`; the daemon creates it once and hands it to every
  session through `IngestOptions::ticker`. Removes the 600 unparks per second.
- **Sockets leave the reactor once `PLAY` succeeds** (`PlayOptions::detach_from_reactor`
  in the patched retina, `third_party/`): the tokio stream turns back into a plain
  non-blocking socket, so reads that would block return `Pending` without a waker and
  nothing wakes `epoll` for the socket; the loop polls on the tick as before. Keepalives
  and `TEARDOWN` write blocking, a few hundred bytes into an idle send buffer. Only the
  timer-driven mode detaches; packet-driven reads keep the reactor.
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

## 7. What remains

The measured figure is 0.021 % per Mbit/s at 132 Mbit/s against the 07 §7 budget of
0.036 %. What is left, in order:

1. Clock reads (2 % of samples in §5): retina stamps every packet with wall and monotonic
   time.
2. Upstream the retina changes (the low-water mark, the detached socket, the short-read
   stop) so the vendored copy can go.
3. The remaining 930 system calls a second are 700 reads, 110 `epoll_wait` returns (the
   ticker, the writer and catalog replies, the API), and the rest bookkeeping; the
   kernel's TCP copy is the floor.

Ruled out: the kernel's page zeroing (3 % of samples, writer thread) is not allocator
churn. Its call chain is `pwritev` → XFS buffered write → `folio_alloc`: the page cache
zeroes the part of each new large folio that a write does not cover, once per folio.
glibc's `trim_threshold`/`mmap_threshold` tunables changed nothing (6.55 % against 6.57 %),
and the cost is about 0.2 % of a core, inherent to buffered writes.
