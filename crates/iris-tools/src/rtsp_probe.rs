//! `rtsp-probe`: speaks RTSP over TCP to a camera (digest authentication),
//! sets up the video track interleaved and the ONVIF metadata track when
//! present, streams for a while, and reports the packetisation, marker use,
//! sequence gaps, timestamp deltas, clock drift, GOP lengths, RTCP sender
//! reports, and metadata traffic.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Write as _};
use std::fs::File;
use std::io::{self, ErrorKind, Read as _, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::ops::Range;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use md5::{Digest, Md5};
use rustix::fs::{Mode, OFlags};

use crate::logs;
use crate::probe_parser::{
    self, DigestChallenge, Interleaved, MAX_RTSP_BODY, MAX_RTSP_HEADER, PayloadKind, SenderReport,
    TimestampUnwrapper,
};
use crate::secret::{Password, password_from_env};
use crate::time;

/// The receive buffer: one response at its limits, or interleaved frames.
const MAX_BUFFER: usize = MAX_RTSP_BODY + MAX_RTSP_HEADER + 4;
/// Most bytes one socket read takes.
const READ_CHUNK: usize = 256 * 1024;
/// Bytes of the metadata track kept as the sample.
const META_SAMPLE_BYTES: usize = 6_000;
/// Characters of the metadata sample printed.
const META_EXCERPT_CHARS: usize = 700;
/// The RTSP port the probe connects to.
const RTSP_PORT: u16 = 554;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The deadline for the whole setup, OPTIONS through PLAY.
const SETUP_DEADLINE: Duration = Duration::from_secs(30);
const TEARDOWN_DEADLINE: Duration = Duration::from_secs(2);
/// The RTP clock rate of H.265 video (RFC 7798 §7.1).
const VIDEO_CLOCK_HZ: f64 = 90_000.0;
/// Most distinct values a histogram keeps; later ones are counted together.
const MAX_HISTOGRAM_BINS: usize = 4_096;
/// Most frames the clock fit keeps, about 9.7 hours at 30 fps.
const MAX_FIT_POINTS: usize = 1 << 20;

fn stream_seconds(value: &str) -> Result<f64, String> {
    bounded_float(value, 0.1, 86_400.0, "stream duration")
}

fn keepalive_seconds(value: &str) -> Result<f64, String> {
    bounded_float(value, 0.0, 3_600.0, "keepalive")
}

fn timeout_seconds(value: &str) -> Result<f64, String> {
    bounded_float(value, 0.1, 300.0, "socket timeout")
}

fn bounded_float(value: &str, min: f64, max: f64, label: &str) -> Result<f64, String> {
    let value = value.parse::<f64>().map_err(|error| error.to_string())?;
    if value.is_finite() && (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "{label} must be finite and between {min} and {max}"
        ))
    }
}

/// Arguments of `rtsp-probe`. `Debug` redacts the password.
#[derive(Debug, Parser)]
pub struct Args {
    /// Environment variable holding the RTSP password, which stays out of
    /// process listings and shell history.
    #[arg(long = "password-env", value_name = "VAR", value_parser = password_from_env)]
    password: Password,
    /// The camera's literal IPv4 or IPv6 address; the probe connects to RTSP
    /// port 554.
    host: String,
    /// RTSP user.
    user: String,
    /// How long to stream, in seconds (0.1 to 86400).
    #[arg(value_parser = stream_seconds)]
    seconds: f64,
    /// The `subtype` of the vendor's `realmonitor` path.
    subtype: String,
    /// Where the metadata track's sample is written: an absent path, or a
    /// caller-owned single-link regular file. It is emptied when the run
    /// starts, so it holds only this run's sample. Symlinks, FIFOs, and
    /// devices are refused.
    meta_out: PathBuf,
    /// `GET_PARAMETER` keepalive period in seconds; 0 disables it.
    #[arg(default_value_t = 25.0, value_parser = keepalive_seconds)]
    keepalive: f64,
    /// The RTSP path, or `-` for the vendor's `realmonitor` path.
    #[arg(default_value = "-")]
    path: String,
    /// Seconds without data that end the run.
    #[arg(default_value_t = 8.0, value_parser = timeout_seconds)]
    socket_timeout: f64,
}

/// Why a read or write on the session ended without data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quiet {
    /// The operation's absolute deadline passed; while streaming, the end of the run.
    Deadline,
    /// No data arrived for the whole socket timeout.
    Stall,
}

impl fmt::Display for Quiet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Deadline => "RTSP operation exceeded its absolute deadline",
            Self::Stall => "no RTSP data arrived within the socket timeout",
        })
    }
}

impl Error for Quiet {}

fn quiet(error: &anyhow::Error) -> Option<Quiet> {
    error.downcast_ref::<Quiet>().copied()
}

struct Conn {
    stream: TcpStream,
    /// Received bytes; those before `start` are consumed.
    buf: Vec<u8>,
    start: usize,
    chunk: Box<[u8]>,
    /// When the last read returned, the arrival time of what it brought.
    read_at: Instant,
    cseq: u32,
    challenge: Option<DigestChallenge>,
    session: Option<String>,
    user: String,
    password: Password,
    deadline: Instant,
    io_timeout: Duration,
}

enum Event {
    Data(u8, Vec<u8>),
    Rtsp(String, Vec<u8>),
}

fn safe_digest_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| (b' '..=b'~').contains(&byte) && !matches!(byte, b'"' | b'\\'))
}

fn safe_session_value(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic())
}

impl Conn {
    fn new(stream: TcpStream, user: &str, password: &Password, io_timeout: Duration) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            start: 0,
            chunk: vec![0u8; READ_CHUNK].into_boxed_slice(),
            read_at: Instant::now(),
            cseq: 0,
            challenge: None,
            session: None,
            user: user.to_owned(),
            password: password.clone(),
            deadline: Instant::now() + SETUP_DEADLINE,
            io_timeout,
        }
    }

    /// Arms the socket timeouts with the smaller of the socket timeout and
    /// what remains before the deadline, and returns it.
    fn arm(&self) -> anyhow::Result<Duration> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(Quiet::Deadline)?;
        let timeout = remaining.min(self.io_timeout);
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.set_write_timeout(Some(timeout))?;
        Ok(timeout)
    }

    fn fill(&mut self) -> anyhow::Result<()> {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let room = MAX_BUFFER - self.buf.len();
        anyhow::ensure!(room > 0, "RTSP receive buffer exceeded its limit");
        let timeout = self.arm()?;
        let n = match self.stream.read(&mut self.chunk[..room.min(READ_CHUNK)]) {
            Ok(n) => n,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                // A wait cut short by the deadline is the deadline, not a stall.
                return Err(if timeout < self.io_timeout {
                    Quiet::Deadline
                } else {
                    Quiet::Stall
                }
                .into());
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(n > 0, "server closed");
        self.read_at = Instant::now();
        self.buf.extend_from_slice(&self.chunk[..n]);
        Ok(())
    }

    fn next_event(&mut self) -> anyhow::Result<Event> {
        loop {
            let base = self.start;
            let item = probe_parser::demultiplex(&self.buf[base..])?;
            let bytes =
                |range: &Range<usize>| self.buf[base + range.start..base + range.end].to_vec();
            let event = match &item {
                Interleaved::Incomplete => {
                    self.fill()?;
                    continue;
                }
                Interleaved::Data { channel, payload } => {
                    Some(Event::Data(*channel, bytes(payload)))
                }
                // `demultiplex` checked that the head is UTF-8.
                Interleaved::Response(frame) => Some(Event::Rtsp(
                    String::from_utf8_lossy(&bytes(&(0..frame.head_end))).into_owned(),
                    bytes(&frame.body),
                )),
                Interleaved::Skip(_) => None,
            };
            self.start += item.consumed();
            if let Some(event) = event {
                return Ok(event);
            }
        }
    }

    fn send(&mut self, method: &str, url: &str, extra: &str) -> anyhow::Result<()> {
        self.arm()?;
        anyhow::ensure!(
            !method.contains(['\r', '\n'])
                && url.bytes().all(|byte| byte.is_ascii_graphic())
                && !extra.contains("\r\n\r\n"),
            "invalid RTSP request field"
        );
        self.cseq += 1;
        let mut head = format!(
            "{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: irisd-probe\r\n",
            self.cseq
        );
        if let Some(session) = &self.session {
            anyhow::ensure!(safe_session_value(session), "invalid RTSP session value");
            let _ = write!(head, "Session: {session}\r\n");
        }
        if let Some(DigestChallenge { realm, nonce }) = &self.challenge {
            anyhow::ensure!(
                safe_digest_value(&self.user)
                    && safe_digest_value(realm)
                    && safe_digest_value(nonce),
                "invalid RTSP digest field"
            );
            let ha1 = hex::encode(Md5::digest(format!(
                "{}:{realm}:{}",
                self.user,
                self.password.expose()
            )));
            let ha2 = hex::encode(Md5::digest(format!("{method}:{url}")));
            let response = hex::encode(Md5::digest(format!("{ha1}:{nonce}:{ha2}")));
            let _ = write!(
                head,
                "Authorization: Digest username=\"{}\", realm=\"{realm}\", nonce=\"{nonce}\", \
                 uri=\"{url}\", response=\"{response}\"\r\n",
                self.user
            );
        }
        self.stream
            .write_all(format!("{head}{extra}\r\n").as_bytes())?;
        Ok(())
    }

    /// Sends a request and returns its response. Interleaved frames and
    /// responses to earlier requests, such as a keepalive's, are skipped.
    fn rtsp(&mut self, method: &str, url: &str, extra: &str) -> anyhow::Result<(String, Vec<u8>)> {
        self.send(method, url, extra)?;
        let expected = self.cseq;
        loop {
            if let Event::Rtsp(head, body) = self.next_event()? {
                let cseq = probe_parser::response_cseq(&head)?;
                anyhow::ensure!(
                    cseq <= expected,
                    "RTSP response CSeq {cseq} does not match request {expected}"
                );
                if cseq == expected {
                    return Ok((head, body));
                }
            }
        }
    }
}

fn status_line(head: &str) -> &str {
    head.lines().next().unwrap_or_default()
}

fn require_success(operation: &str, head: &str) -> anyhow::Result<()> {
    let status = probe_parser::response_status(head)?;
    anyhow::ensure!(
        (200..300).contains(&status),
        "{operation} returned RTSP {status}: {}",
        logs::terminal_safe(status_line(head))
    );
    Ok(())
}

/// The header lines named `names`, for printing with `{:?}`, which escapes them.
fn header_lines<'a>(head: &'a str, names: &[&str]) -> Vec<&'a str> {
    head.lines()
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(name, _)| names.contains(&name.trim()))
        })
        .collect()
}

/// Writes camera-controlled `bytes` to `path` without following a symlink,
/// blocking on a FIFO, or writing through another name: the path must be absent,
/// or name a caller-owned regular file with a single link, which is then truncated.
fn write_output(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let fd = rustix::fs::open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )
    .with_context(|| format!("opening {} without following links", path.display()))?;
    let mut file = File::from(fd);
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == rustix::process::geteuid().as_raw(),
        "{} must be a new file or a caller-owned single-link regular file",
        path.display()
    );
    // Truncate only after the checks, so a hard link to another file survives.
    file.set_len(0)?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))
}

/// Counts that keep a bounded number of distinct values.
#[derive(Debug, Default)]
struct Histogram {
    bins: BTreeMap<i64, u64>,
    /// Values that arrived once [`MAX_HISTOGRAM_BINS`] were in use.
    overflow: u64,
}

impl Histogram {
    fn add(&mut self, value: i64) {
        if let Some(count) = self.bins.get_mut(&value) {
            *count += 1;
        } else if self.bins.len() < MAX_HISTOGRAM_BINS {
            self.bins.insert(value, 1);
        } else {
            self.overflow += 1;
        }
    }

    /// The `n` most frequent values with their counts.
    fn top(&self, n: usize) -> Vec<(i64, u64)> {
        let mut values: Vec<(i64, u64)> = self.bins.iter().map(|(v, c)| (*v, *c)).collect();
        values.sort_by_key(|(_, count)| Reverse(*count));
        values.truncate(n);
        values
    }
}

/// One access unit: the packets that share an RTP timestamp.
struct Frame {
    /// The RTP timestamp on the extended timeline.
    timestamp: i64,
    size: usize,
    key: bool,
    arrival: Instant,
    had_marker: bool,
}

/// RTP packet counts.
#[derive(Debug, Default)]
struct Packets {
    count: u64,
    bytes: u64,
    fragments: u64,
    aggregations: u64,
    singles: u64,
    empty: u64,
    markers: u64,
    max_payload: usize,
    sequence_gaps: u64,
    padded: u64,
    extended: u64,
}

/// Running figures over the finished frames.
#[derive(Debug, Default)]
struct Frames {
    count: u64,
    keys: u64,
    without_marker: u64,
    key_bytes: u64,
    other_bytes: u64,
    /// Arrival and timestamp of the first frame.
    first: Option<(Instant, i64)>,
    /// Arrival and timestamp of the last finished frame.
    last: Option<(Instant, i64)>,
    /// Timestamp steps between consecutive frames.
    deltas: Histogram,
    /// Steps that went back in time.
    backward_steps: u64,
    /// Frames from one key frame to the next.
    gops: Histogram,
    last_key: Option<u64>,
    /// Arrival seconds against RTP seconds since the first frame, for the
    /// clock fit; at most [`MAX_FIT_POINTS`].
    fit: Vec<(f64, f64)>,
}

impl Frames {
    fn finish(&mut self, frame: &Frame) {
        let index = self.count;
        self.count += 1;
        let size = frame.size as u64;
        if frame.key {
            self.keys += 1;
            self.key_bytes = self.key_bytes.saturating_add(size);
            if let Some(last) = self.last_key {
                self.gops.add((index - last) as i64);
            }
            self.last_key = Some(index);
        } else {
            self.other_bytes = self.other_bytes.saturating_add(size);
        }
        if !frame.had_marker {
            self.without_marker += 1;
        }
        if let Some((_, previous)) = self.last {
            let step = frame.timestamp.saturating_sub(previous);
            if step < 0 {
                self.backward_steps += 1;
            }
            self.deltas.add(step);
        }
        let (first_arrival, first_timestamp) =
            *self.first.get_or_insert((frame.arrival, frame.timestamp));
        if self.fit.len() < MAX_FIT_POINTS {
            self.fit.push((
                frame.arrival.duration_since(first_arrival).as_secs_f64(),
                frame.timestamp.saturating_sub(first_timestamp) as f64 / VIDEO_CLOCK_HZ,
            ));
        }
        self.last = Some((frame.arrival, frame.timestamp));
    }

    /// Seconds from the first frame's arrival to the last one's.
    fn span(&self) -> f64 {
        match (self.first, self.last) {
            (Some((first, _)), Some((last, _))) => last.duration_since(first).as_secs_f64(),
            _ => 0.0,
        }
        .max(1e-9)
    }
}

/// Running figures over the RTCP sender reports.
#[derive(Debug, Default)]
struct SenderReports {
    count: u64,
    /// Host wall time and report of the first one.
    first: Option<(f64, SenderReport)>,
    /// Host wall time and report of the last one so far.
    last: Option<(f64, SenderReport)>,
    timeline: TimestampUnwrapper,
    first_rtp: i64,
    last_rtp: i64,
    offset_sum: f64,
    offset_min: f64,
    offset_max: f64,
}

impl SenderReports {
    fn add(&mut self, wall: f64, report: SenderReport) {
        let rtp = self.timeline.extend(report.rtp_timestamp);
        let offset = report.ntp_seconds - wall;
        if self.first.is_none() {
            self.first = Some((wall, report));
            self.first_rtp = rtp;
            self.offset_min = offset;
            self.offset_max = offset;
        }
        self.count += 1;
        self.last = Some((wall, report));
        self.last_rtp = rtp;
        self.offset_sum += offset;
        self.offset_min = self.offset_min.min(offset);
        self.offset_max = self.offset_max.max(offset);
    }
}

/// The interleaved channels the session's tracks arrive on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Channels {
    video_rtp: u8,
    video_rtcp: u8,
    metadata_rtp: Option<u8>,
}

/// Everything the probe measures while streaming.
#[derive(Default)]
struct Measurements {
    packets: Packets,
    sequence: Option<u16>,
    timeline: TimestampUnwrapper,
    current: Option<Frame>,
    frames: Frames,
    nal_types: BTreeMap<u8, u64>,
    rtcp_types: BTreeMap<u8, u64>,
    reports: SenderReports,
    metadata_packets: u64,
    metadata_bytes: u64,
    metadata_sample: Vec<u8>,
}

impl Measurements {
    fn on_data(
        &mut self,
        channel: u8,
        packet: &[u8],
        channels: Channels,
        arrival: Instant,
    ) -> anyhow::Result<()> {
        if channel == channels.video_rtp {
            self.on_video(packet, arrival).context("video RTP packet")
        } else if channel == channels.video_rtcp {
            self.on_rtcp(packet).context("video RTCP packet")
        } else if Some(channel) == channels.metadata_rtp {
            self.on_metadata(packet).context("metadata RTP packet")
        } else {
            Ok(())
        }
    }

    fn on_video(&mut self, packet: &[u8], arrival: Instant) -> anyhow::Result<()> {
        let rtp = probe_parser::rtp_packet(packet)?;
        let payload = &packet[rtp.payload.clone()];
        let counts = &mut self.packets;
        counts.count += 1;
        counts.bytes += packet.len() as u64;
        counts.max_payload = counts.max_payload.max(payload.len());
        counts.extended += u64::from(rtp.extension);
        counts.padded += u64::from(rtp.padded);
        if self
            .sequence
            .is_some_and(|previous| rtp.sequence.wrapping_sub(previous) != 1)
        {
            counts.sequence_gaps += 1;
        }
        self.sequence = Some(rtp.sequence);
        let timestamp = self.timeline.extend(rtp.timestamp);
        if let Some(done) = self.current.take_if(|frame| frame.timestamp != timestamp) {
            self.frames.finish(&done);
        }
        let frame = self.current.get_or_insert(Frame {
            timestamp,
            size: 0,
            key: false,
            arrival,
            had_marker: false,
        });
        frame.size = frame
            .size
            .checked_add(payload.len())
            .context("RTP frame size overflow")?;
        frame.arrival = arrival;
        if payload.is_empty() {
            self.packets.empty += 1;
        } else {
            let units = probe_parser::h265_payload(payload)?;
            match units.kind {
                PayloadKind::Single => self.packets.singles += 1,
                PayloadKind::Aggregation => self.packets.aggregations += 1,
                PayloadKind::Fragment => self.packets.fragments += 1,
            }
            for nal_type in units.nal_types {
                *self.nal_types.entry(nal_type).or_default() += 1;
                frame.key |= iris_h265::nal_type(nal_type << 1).is_irap();
            }
        }
        if rtp.marker {
            self.packets.markers += 1;
            if let Some(mut done) = self.current.take() {
                done.had_marker = true;
                self.frames.finish(&done);
            }
        }
        Ok(())
    }

    fn on_rtcp(&mut self, data: &[u8]) -> anyhow::Result<()> {
        let wall = time::now();
        for packet in probe_parser::rtcp_packets(data)? {
            *self.rtcp_types.entry(packet.packet_type).or_default() += 1;
            if let Some(report) = packet.sender_report {
                self.reports.add(wall, report);
            }
        }
        Ok(())
    }

    fn on_metadata(&mut self, packet: &[u8]) -> anyhow::Result<()> {
        let rtp = probe_parser::rtp_packet(packet)?;
        self.metadata_packets += 1;
        self.metadata_bytes += packet.len() as u64;
        let room = META_SAMPLE_BYTES.saturating_sub(self.metadata_sample.len());
        let payload = &packet[rtp.payload];
        self.metadata_sample
            .extend_from_slice(&payload[..payload.len().min(room)]);
        Ok(())
    }

    fn print_rtp_and_frames(&self, out: &mut impl Write) -> io::Result<()> {
        let (packets, frames) = (&self.packets, &self.frames);
        let span = frames.span();
        writeln!(
            out,
            "\n== RTP: pkts={} bytes={} ({:.2} Mbps over {span:.1}s) maxpayload={} FU={} AP={} \
             single={} empty={} marker={} seqgaps={} ext={} pad={}",
            packets.count,
            packets.bytes,
            packets.bytes as f64 * 8.0 / span / 1e6,
            packets.max_payload,
            packets.fragments,
            packets.aggregations,
            packets.singles,
            packets.empty,
            packets.markers,
            packets.sequence_gaps,
            packets.extended,
            packets.padded
        )?;
        let average = |bytes: u64, count: u64| {
            if count == 0 {
                0.0
            } else {
                bytes as f64 / count as f64
            }
        };
        writeln!(
            out,
            "== frames={} ({:.2} fps) keyframes={} frames_without_marker={} avgI={:.0}B \
             avgP={:.0}B",
            frames.count,
            frames.count as f64 / span,
            frames.keys,
            frames.without_marker,
            average(frames.key_bytes, frames.keys),
            average(frames.other_bytes, frames.count - frames.keys)
        )?;
        writeln!(out, "== NAL types: {:?}", self.nal_types)?;
        writeln!(
            out,
            "== RTP ts deltas (ticks:count): {:?}; backward steps={}{}",
            frames.deltas.top(10),
            frames.backward_steps,
            overflow_note(frames.deltas.overflow)
        )?;
        writeln!(
            out,
            "== GOP lengths (frames): {:?}{}",
            frames.gops.bins,
            overflow_note(frames.gops.overflow)
        )
    }

    fn print_clock(&self, out: &mut impl Write) -> io::Result<()> {
        let points = &self.frames.fit;
        let n = points.len() as f64;
        if points.len() < 2 {
            return Ok(());
        }
        let (mx, my) = (
            points.iter().map(|(x, _)| x).sum::<f64>() / n,
            points.iter().map(|(_, y)| y).sum::<f64>() / n,
        );
        let den: f64 = points.iter().map(|(x, _)| (x - mx).powi(2)).sum();
        let slope = if den > 0.0 {
            points.iter().map(|(x, y)| (x - mx) * (y - my)).sum::<f64>() / den
        } else {
            1.0
        };
        let residuals: Vec<f64> = points
            .iter()
            .map(|(x, y)| y - (my + slope * (x - mx)))
            .collect();
        let stdev = (residuals.iter().map(|r| r * r).sum::<f64>() / n).sqrt();
        writeln!(
            out,
            "== RTP clock vs host monotonic: slope={slope:.6} ({:+.0} ppm), residual stdev={:.1} \
             ms, max|res|={:.1} ms{}",
            (slope - 1.0) * 1e6,
            stdev * 1000.0,
            residuals.iter().map(|r| r.abs()).fold(0.0, f64::max) * 1000.0,
            if self.frames.count > points.len() as u64 {
                format!(" (over the first {} frames)", points.len())
            } else {
                String::new()
            }
        )
    }

    fn print_reports(&self, out: &mut impl Write) -> io::Result<()> {
        let reports = &self.reports;
        writeln!(
            out,
            "== RTCP packets by type: {:?}; SRs={}",
            self.rtcp_types, reports.count
        )?;
        let (Some((first_wall, first)), Some((last_wall, last))) = (reports.first, reports.last)
        else {
            return Ok(());
        };
        let intervals = reports.count.saturating_sub(1);
        writeln!(
            out,
            "   SR interval mean={:.1}s; SR NTP minus host wall: mean={:+.3}s min={:+.3} \
             max={:+.3}",
            if intervals == 0 {
                0.0
            } else {
                (last_wall - first_wall) / intervals as f64
            },
            reports.offset_sum / reports.count as f64,
            reports.offset_min,
            reports.offset_max
        )?;
        writeln!(
            out,
            "   first SR: ntp={} rtp_ts={} pkts={} octets={}",
            time::iso(first.ntp_seconds),
            first.rtp_timestamp,
            first.packet_count,
            first.octet_count
        )?;
        let dt = last.ntp_seconds - first.ntp_seconds;
        if dt > 0.0 {
            writeln!(
                out,
                "   SR-implied RTP rate: {:.1} Hz over {dt:.1}s (nominal 90000)",
                (reports.last_rtp - reports.first_rtp) as f64 / dt
            )?;
        }
        Ok(())
    }

    fn print_metadata(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(
            out,
            "== ONVIF metadata track: pkts={} bytes={}",
            self.metadata_packets, self.metadata_bytes
        )?;
        if !self.metadata_sample.is_empty() {
            let text: String = String::from_utf8_lossy(&self.metadata_sample)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            writeln!(
                out,
                "   sample: {}",
                logs::terminal_safe(&text.chars().take(META_EXCERPT_CHARS).collect::<String>())
            )?;
        }
        Ok(())
    }

    /// Prints the report over whatever was received.
    fn print(&self, out: &mut impl Write) -> io::Result<()> {
        self.print_rtp_and_frames(out)?;
        self.print_clock(out)?;
        self.print_reports(out)?;
        self.print_metadata(out)
    }
}

fn overflow_note(overflow: u64) -> String {
    if overflow == 0 {
        String::new()
    } else {
        format!(" (+{overflow} beyond the first {MAX_HISTOGRAM_BINS} distinct values)")
    }
}

/// The session's URLs and video channels after a successful video `SETUP`.
struct Session {
    url: String,
    metadata_control: Option<String>,
    channels: Channels,
}

/// A keepalive sent and still unanswered.
#[derive(Clone, Copy, Debug)]
struct Keepalive {
    cseq: u32,
    sent: Instant,
}

/// How streaming ended.
#[derive(Debug)]
enum Outcome {
    /// The run lasted its full time.
    Completed,
    /// No data arrived for the socket timeout, `at` seconds into the run.
    Stalled { at: f64 },
    /// The run stopped on an error.
    Failed(anyhow::Error),
}

/// OPTIONS, DESCRIBE (answering a digest challenge), and the video `SETUP`;
/// once this returns, the session needs a `TEARDOWN`.
fn setup_video(conn: &mut Conn, url: &str) -> anyhow::Result<Session> {
    let (options, _) = conn.rtsp("OPTIONS", url, "")?;
    require_success("OPTIONS", &options)?;
    let (mut head, mut body) = conn.rtsp("DESCRIBE", url, "Accept: application/sdp\r\n")?;
    if probe_parser::response_status(&head)? == 401 {
        conn.challenge = Some(
            probe_parser::digest_challenge(&head)
                .context("DESCRIBE authentication challenge is incomplete")?,
        );
        (head, body) = conn.rtsp("DESCRIBE", url, "Accept: application/sdp\r\n")?;
    }
    require_success("DESCRIBE", &head)?;
    println!(
        "DESCRIBE headers: {:?}",
        header_lines(
            &head,
            &["Server", "Content-Base", "Content-Type", "Cache-Control"]
        )
    );
    let base = probe_parser::header_value(&head, "Content-Base")
        .map_or_else(|| format!("{url}/"), str::to_owned);
    let tracks = probe_parser::sdp_controls(&String::from_utf8_lossy(&body));
    println!("track controls: {tracks:?}");
    let control = |kind: &str| {
        tracks
            .iter()
            .find(|(track, _)| track == kind)
            .and_then(|(_, control)| control.as_deref())
            .map(|control| {
                if control.starts_with("rtsp://") {
                    control.to_owned()
                } else {
                    format!("{base}{control}")
                }
            })
    };
    let video = control("video").context("no video track")?;
    let (head, _) = conn.rtsp(
        "SETUP",
        &video,
        "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n",
    )?;
    require_success("video SETUP", &head)?;
    println!(
        "SETUP video: {} | {:?}",
        logs::terminal_safe(status_line(&head)),
        header_lines(&head, &["Transport", "Session"])
    );
    conn.session = probe_parser::header_value(&head, "Session")
        .map(|value| probe_parser::session_id(value).to_owned());
    anyhow::ensure!(
        conn.session.as_deref().is_some_and(safe_session_value),
        "video SETUP response has no valid session"
    );
    let video_channels = probe_parser::header_value(&head, "Transport")
        .and_then(probe_parser::interleaved_channels)
        .unwrap_or((0, 1));
    Ok(Session {
        url: url.to_owned(),
        metadata_control: control("application"),
        channels: Channels {
            video_rtp: video_channels.0,
            video_rtcp: video_channels.1,
            metadata_rtp: None,
        },
    })
}

/// The metadata `SETUP`, when the camera offers the track, and `PLAY`.
fn play(conn: &mut Conn, session: &mut Session) -> anyhow::Result<()> {
    if let Some(control) = &session.metadata_control {
        let (head, _) = conn.rtsp(
            "SETUP",
            control,
            "Transport: RTP/AVP/TCP;unicast;interleaved=4-5\r\n",
        )?;
        require_success("metadata SETUP", &head)?;
        println!(
            "SETUP metadata: {}",
            logs::terminal_safe(status_line(&head))
        );
        session.channels.metadata_rtp = Some(
            probe_parser::header_value(&head, "Transport")
                .and_then(probe_parser::interleaved_channels)
                .map_or(4, |(rtp, _)| rtp),
        );
    }
    let (head, _) = conn.rtsp("PLAY", &session.url, "Range: npt=0.000-\r\n")?;
    require_success("PLAY", &head)?;
    println!(
        "PLAY: {} | {:?}",
        logs::terminal_safe(status_line(&head)),
        header_lines(&head, &["RTP-Info", "Range"])
    );
    Ok(())
}

/// Streams for `seconds`, sending a keepalive every `keepalive` seconds, and
/// returns how streaming ended with the keepalive still unanswered, if any.
fn receive(
    conn: &mut Conn,
    session: &Session,
    seconds: f64,
    keepalive: f64,
    measurements: &mut Measurements,
) -> (Outcome, Option<Keepalive>) {
    let start = Instant::now();
    let end = start + Duration::from_secs_f64(seconds);
    conn.deadline = end;
    let period = (keepalive > 0.0).then(|| Duration::from_secs_f64(keepalive));
    let mut next_keepalive = period.map(|period| start + period);
    let mut pending: Option<Keepalive> = None;
    let outcome = loop {
        if Instant::now() >= end {
            break Outcome::Completed;
        }
        if pending.is_none()
            && let Some(due) = next_keepalive
            && Instant::now() >= due
        {
            match conn.send("GET_PARAMETER", &session.url, "") {
                Ok(()) => {
                    pending = Some(Keepalive {
                        cseq: conn.cseq,
                        sent: Instant::now(),
                    });
                    next_keepalive = None;
                }
                Err(error) if quiet(&error) == Some(Quiet::Deadline) => break Outcome::Completed,
                Err(error) => break Outcome::Failed(error.context("sending the keepalive")),
            }
        }
        let event = match conn.next_event() {
            Ok(event) => event,
            Err(error) => match quiet(&error) {
                Some(Quiet::Deadline) => break Outcome::Completed,
                Some(Quiet::Stall) => {
                    break Outcome::Stalled {
                        at: start.elapsed().as_secs_f64(),
                    };
                }
                None => break Outcome::Failed(error),
            },
        };
        match event {
            Event::Rtsp(head, _) => {
                let answered = probe_parser::response_cseq(&head).and_then(|cseq| {
                    pending
                        .filter(|keepalive| keepalive.cseq == cseq)
                        .with_context(|| {
                            format!("unsolicited RTSP response (CSeq {cseq}) while streaming")
                        })?;
                    require_success("GET_PARAMETER", &head)
                });
                if let Err(error) = answered {
                    break Outcome::Failed(error);
                }
                pending = None;
                next_keepalive = period.map(|period| Instant::now() + period);
                println!(
                    "[keepalive acknowledged at {:.1}s]",
                    start.elapsed().as_secs_f64()
                );
            }
            Event::Data(channel, packet) => {
                if let Err(error) =
                    measurements.on_data(channel, &packet, session.channels, conn.read_at)
                {
                    break Outcome::Failed(error);
                }
            }
        }
    };
    (outcome, pending)
}

/// The session URL and the camera's socket address, from validated arguments.
fn session_target(args: &Args) -> anyhow::Result<(String, SocketAddr)> {
    anyhow::ensure!(
        !args.subtype.contains(['\r', '\n']) && !args.path.contains(['\r', '\n']),
        "RTSP path fields cannot contain line breaks"
    );
    anyhow::ensure!(safe_digest_value(&args.user), "invalid RTSP username");
    let path = if args.path == "-" {
        format!(
            "/cam/realmonitor?channel=1&subtype={}&unicast=true&proto=Onvif",
            args.subtype
        )
    } else {
        args.path.clone()
    };
    anyhow::ensure!(path.starts_with('/'), "RTSP path must begin with /");
    let host = args
        .host
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .context("camera host must be a literal IP address")?;
    let authority = match host {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => format!("[{address}]"),
    };
    Ok((
        format!("rtsp://{authority}:{RTSP_PORT}{path}"),
        SocketAddr::new(host, RTSP_PORT),
    ))
}

/// The run's verdict once the report is printed: a failure, or a keepalive
/// the camera left unanswered for the whole socket timeout, fails the run.
fn verdict(
    outcome: Outcome,
    unanswered: Option<Keepalive>,
    frames: u64,
    io_timeout: Duration,
) -> anyhow::Result<()> {
    let ignored = unanswered.filter(|keepalive| keepalive.sent.elapsed() >= io_timeout);
    match outcome {
        Outcome::Failed(error) => Err(error),
        _ if ignored.is_some() => anyhow::bail!(
            "the camera did not acknowledge GET_PARAMETER (CSeq {}) within the socket timeout",
            ignored.map_or(0, |keepalive| keepalive.cseq)
        ),
        _ if frames < 2 => anyhow::bail!("fewer than two frames received"),
        Outcome::Completed | Outcome::Stalled { .. } => Ok(()),
    }
}

/// Streams a session that `setup_video` opened, prints the report over what
/// arrived, and writes the metadata sample.
fn measure(conn: &mut Conn, mut session: Session, args: &Args) -> anyhow::Result<()> {
    play(conn, &mut session)?;
    let mut measurements = Measurements::default();
    let (outcome, unanswered) = receive(
        conn,
        &session,
        args.seconds,
        args.keepalive,
        &mut measurements,
    );
    if let Outcome::Stalled { at } = outcome {
        println!(
            "!! recv timeout (no data for {:.0} s) at {at:.1}s; frames so far={}",
            args.socket_timeout, measurements.frames.count
        );
    }
    if let Some(keepalive) = unanswered {
        println!(
            "[keepalive CSeq {} unanswered after {:.1}s]",
            keepalive.cseq,
            keepalive.sent.elapsed().as_secs_f64()
        );
    }
    let stdout = io::stdout();
    measurements.print(&mut stdout.lock())?;
    if !measurements.metadata_sample.is_empty() {
        write_output(&args.meta_out, &measurements.metadata_sample)?;
    }
    verdict(
        outcome,
        unanswered,
        measurements.frames.count,
        conn.io_timeout,
    )
}

/// Opens one RTSP session to the camera at `HOST`, port 554; streams for
/// `SECONDS`; prints the report over whatever arrived; writes the metadata
/// sample to `META_OUT`; and tears the session down. A refused request, a
/// malformed response or packet, too few frames, or a keepalive left
/// unanswered fails the run after the report.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let (url, address) = session_target(args)?;
    write_output(&args.meta_out, b"")?;
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).context("connecting")?;
    let mut conn = Conn::new(
        stream,
        &args.user,
        &args.password,
        Duration::from_secs_f64(args.socket_timeout),
    );
    let session = setup_video(&mut conn, &url)?;
    let measured = measure(&mut conn, session, args);
    conn.deadline = Instant::now() + TEARDOWN_DEADLINE;
    let teardown = conn
        .rtsp("TEARDOWN", &url, "")
        .and_then(|(head, _)| require_success("TEARDOWN", &head));
    match (measured, teardown) {
        (Err(error), Err(teardown)) => {
            Err(error.context(format!("TEARDOWN also failed: {teardown:#}")))
        }
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    use iris_replay::media::Media;
    use iris_replay::rtsp::MountName;
    use iris_replay::server::{Options, Server};
    use iris_replay::synth::{self, Plan};
    use tokio::runtime::Runtime;

    use super::*;

    fn parse(password_env: &str) -> Result<Args, clap::Error> {
        Args::try_parse_from([
            "rtsp-probe",
            "--password-env",
            password_env,
            "192.0.2.10",
            "user",
            "90",
            "0",
            "meta.xml",
        ])
    }

    #[test]
    fn the_password_comes_from_the_environment_and_stays_out_of_debug() {
        let expected = std::env::var("PATH").expect("tests run with PATH set");
        let args = parse("PATH").unwrap();
        assert_eq!(args.password.expose(), expected);
        assert_eq!(args.user, "user");
        assert!(!format!("{args:?}").contains(&expected));
    }

    #[test]
    fn an_unset_password_variable_is_named() {
        let error = parse("IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A") && error.contains("not set"),
            "{error}"
        );
    }

    #[test]
    fn the_password_variable_is_required() {
        assert!(
            Args::try_parse_from(["rtsp-probe", "192.0.2.10", "user", "90", "0", "meta.xml"])
                .is_err()
        );
    }

    #[test]
    fn the_target_is_a_literal_address_on_port_554() {
        let args = parse("PATH").unwrap();
        let (url, address) = session_target(&args).unwrap();
        assert!(
            url.starts_with("rtsp://192.0.2.10:554/cam/realmonitor?"),
            "{url}"
        );
        assert_eq!(address, "192.0.2.10:554".parse().unwrap());
        let mut named = parse("PATH").unwrap();
        named.host = "camera.example".into();
        assert!(session_target(&named).is_err());
    }

    #[test]
    fn metadata_output_refuses_links() {
        let directory = tempfile::tempdir().unwrap();
        let victim = directory.path().join("victim.conf");
        std::fs::write(&victim, b"original contents").unwrap();
        let linked = directory.path().join("linked.xml");
        std::os::unix::fs::symlink(&victim, &linked).unwrap();
        assert!(write_output(&linked, b"<camera-controlled/>").is_err());
        let hard = directory.path().join("hard.xml");
        std::fs::hard_link(&victim, &hard).unwrap();
        assert!(write_output(&hard, b"<camera-controlled/>").is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"original contents");
    }

    #[test]
    fn metadata_output_refuses_a_fifo_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo.xml");
        rustix::fs::mkfifoat(rustix::fs::CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        let started = Instant::now();
        assert!(write_output(&fifo, b"<camera-controlled/>").is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a FIFO never blocks"
        );
    }

    #[test]
    fn metadata_output_replaces_a_private_file_and_creates_an_absent_one() {
        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("meta.xml");
        std::fs::write(&regular, b"a much longer earlier sample").unwrap();
        write_output(&regular, b"<new/>").unwrap();
        assert_eq!(std::fs::read(&regular).unwrap(), b"<new/>");
        let fresh = directory.path().join("fresh.xml");
        write_output(&fresh, b"<fresh/>").unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"<fresh/>");
    }

    #[test]
    fn stream_seconds_must_be_finite_and_in_range() {
        for invalid in ["NaN", "inf", "-1", "86401"] {
            assert!(stream_seconds(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn a_keepalive_of_zero_disables_it_but_a_timeout_cannot_be_zero() {
        assert_eq!(keepalive_seconds("0").unwrap(), 0.0);
        assert!(timeout_seconds("0").is_err());
    }

    #[test]
    fn digest_values_reject_quotes_and_line_breaks() {
        assert!(safe_digest_value("camera-user"));
        assert!(!safe_digest_value("camera\r\nInjected: yes"));
        assert!(!safe_digest_value("camera\"user"));
    }

    #[test]
    fn session_values_reject_spaces() {
        assert!(safe_session_value("ABC123-._"));
        assert!(!safe_session_value("ABC 123"));
    }

    fn video(sequence: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        // Version 2; the marker bit over payload type 96.
        let mut packet = vec![0x80, (u8::from(marker) << 7) | 0x60];
        packet.extend_from_slice(&sequence.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&[0, 0, 0, 1]);
        packet.extend_from_slice(payload);
        packet
    }

    const IDR: [u8; 3] = [19 << 1, 1, 0];
    const TRAIL: [u8; 3] = [1 << 1, 1, 0];

    #[test]
    fn a_backward_timestamp_step_is_counted_without_panicking() {
        let mut measurements = Measurements::default();
        let channels = Channels {
            video_rtp: 0,
            video_rtcp: 1,
            metadata_rtp: None,
        };
        let now = Instant::now();
        for (sequence, timestamp, payload) in
            [(1, 9_000, IDR), (2, 6_000, TRAIL), (3, 12_000, TRAIL)]
        {
            measurements
                .on_data(
                    0,
                    &video(sequence, timestamp, true, &payload),
                    channels,
                    now,
                )
                .unwrap();
        }
        let frames = &measurements.frames;
        assert_eq!(frames.count, 3);
        assert_eq!(frames.keys, 1);
        assert_eq!(frames.backward_steps, 1);
        assert_eq!(frames.deltas.bins.get(&-3_000), Some(&1));
    }

    #[test]
    fn a_frame_ends_at_its_marker_or_at_the_next_timestamp() {
        let mut measurements = Measurements::default();
        let channels = Channels {
            video_rtp: 0,
            video_rtcp: 1,
            metadata_rtp: None,
        };
        let now = Instant::now();
        for (sequence, timestamp, marker) in [
            (1, 0, false),
            (2, 0, true),
            (3, 3_000, false),
            (4, 6_000, true),
        ] {
            measurements
                .on_data(
                    0,
                    &video(sequence, timestamp, marker, &TRAIL),
                    channels,
                    now,
                )
                .unwrap();
        }
        assert_eq!(measurements.frames.count, 3);
        assert_eq!(measurements.frames.without_marker, 1);
        assert_eq!(measurements.packets.markers, 2);
    }

    #[test]
    fn sender_report_rates_unwrap_the_rtp_timestamp() {
        let mut reports = SenderReports::default();
        let report = |ntp_seconds: f64, rtp_timestamp: u32| SenderReport {
            ntp_seconds,
            rtp_timestamp,
            packet_count: 0,
            octet_count: 0,
        };
        reports.add(0.0, report(0.0, u32::MAX - 89_999));
        reports.add(1.0, report(1.0, 0));
        assert_eq!(reports.last_rtp - reports.first_rtp, 90_000);
    }

    /// Serves a synthetic camera on loopback with `options`, on a runtime the
    /// caller keeps alive.
    fn replay(options: Options) -> (Runtime, SocketAddr) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let media = Media::try_from(synth::recording(Plan::default()).unwrap()).unwrap();
        let mounts = HashMap::from([(MountName::try_new("main").unwrap(), Arc::new(media))]);
        let server = runtime
            .block_on(Server::bind(
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
                mounts,
                options,
            ))
            .unwrap();
        let address = server.local_addr().unwrap();
        runtime.spawn(server.run());
        (runtime, address)
    }

    fn connect(address: SocketAddr, io_timeout: Duration) -> Conn {
        let stream = TcpStream::connect(address).unwrap();
        Conn::new(
            stream,
            "user",
            &password_from_env("PATH").unwrap(),
            io_timeout,
        )
    }

    #[test]
    fn a_full_run_against_a_replayed_camera_ends_cleanly_at_its_deadline() {
        let (_runtime, address) = replay(Options::default());
        let url = format!("rtsp://{address}/main");
        let mut conn = connect(address, Duration::from_secs(8));
        let mut session = setup_video(&mut conn, &url).unwrap();
        play(&mut conn, &mut session).unwrap();
        let mut measurements = Measurements::default();
        let (outcome, unanswered) = receive(&mut conn, &session, 1.5, 0.4, &mut measurements);
        assert!(matches!(outcome, Outcome::Completed), "{outcome:?}");
        assert!(
            measurements.frames.count >= 30,
            "{:?}",
            measurements.frames.count
        );
        assert!(measurements.frames.keys >= 1);
        assert_eq!(measurements.packets.sequence_gaps, 0);
        verdict(
            outcome,
            unanswered,
            measurements.frames.count,
            conn.io_timeout,
        )
        .unwrap();
        conn.deadline = Instant::now() + TEARDOWN_DEADLINE;
        let (head, _) = conn.rtsp("TEARDOWN", &url, "").unwrap();
        require_success("TEARDOWN", &head).unwrap();
    }

    #[test]
    fn a_stalled_camera_is_reported_with_what_arrived_before() {
        let (_runtime, address) = replay(Options {
            stall_after: Some(Duration::from_millis(500)),
            stall_for: Duration::from_secs(5),
            ..Options::default()
        });
        let url = format!("rtsp://{address}/main");
        let mut conn = connect(address, Duration::from_millis(400));
        let mut session = setup_video(&mut conn, &url).unwrap();
        play(&mut conn, &mut session).unwrap();
        let mut measurements = Measurements::default();
        let (outcome, _) = receive(&mut conn, &session, 4.0, 0.0, &mut measurements);
        let Outcome::Stalled { at } = outcome else {
            panic!("{outcome:?}");
        };
        assert!(at < 3.0, "{at}");
        assert!(measurements.frames.count >= 5);
    }
}
