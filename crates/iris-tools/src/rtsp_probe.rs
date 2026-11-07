//! `rtsp-probe`: speaks RTSP over TCP to a camera (digest authentication),
//! sets up the video track interleaved and the ONVIF metadata track when
//! present, streams for a while, and reports the packetisation, marker use,
//! sequence gaps, timestamp deltas, clock drift, GOP lengths, RTCP sender
//! reports, and metadata traffic.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::fs::MetadataExt as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Parser;
use md5::{Digest, Md5};

use iris_tools::probe_parser::{rtp_payload_bounds, rtsp_response_bounds};

use crate::logs::terminal_safe;
use crate::secret::{Password, password_from_env};

const MAX_RTSP_HEADER: usize = 64 * 1024;
const MAX_RTSP_BODY: usize = 4 * 1024 * 1024;
const MAX_BUFFER: usize = MAX_RTSP_BODY + MAX_RTSP_HEADER + 4;

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
    /// Camera host.
    host: String,
    /// RTSP user.
    user: String,
    /// How long to stream.
    #[arg(value_parser = stream_seconds)]
    seconds: f64,
    /// The `subtype` of the vendor's `realmonitor` path.
    subtype: String,
    /// Where a sample of the metadata track is written: an absent path, or a
    /// caller-owned single-link regular file that is replaced. Symlinks, FIFOs,
    /// and devices are refused.
    meta_out: std::path::PathBuf,
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

struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
    chunk: Box<[u8]>,
    cseq: u32,
    realm: Option<String>,
    nonce: Option<String>,
    session: Option<String>,
    user: String,
    password: String,
    deadline: Instant,
    io_timeout: Duration,
}

enum Event {
    Data(u8, Vec<u8>),
    Rtsp(String, Vec<u8>),
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_owned())
    })
}

fn quoted(head: &str, key: &str) -> Option<String> {
    let start = head.find(&format!("{key}=\""))? + key.len() + 2;
    let end = head[start..].find('"')? + start;
    Some(head[start..end].to_owned())
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
    fn fill(&mut self) -> anyhow::Result<()> {
        self.apply_deadline()?;
        let n = self.stream.read(&mut self.chunk)?;
        anyhow::ensure!(n > 0, "server closed");
        let (buf, chunk) = (&mut self.buf, &self.chunk);
        anyhow::ensure!(
            buf.len()
                .checked_add(n)
                .is_some_and(|size| size <= MAX_BUFFER),
            "RTSP receive buffer exceeded its limit"
        );
        buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn apply_deadline(&self) -> anyhow::Result<()> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .context("RTSP operation exceeded its absolute deadline")?;
        let timeout = remaining.min(self.io_timeout);
        anyhow::ensure!(!timeout.is_zero(), "RTSP operation deadline expired");
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.set_write_timeout(Some(timeout))?;
        Ok(())
    }

    fn read_response(&mut self) -> anyhow::Result<(String, Vec<u8>)> {
        let i = loop {
            if let Some(i) = find(&self.buf, b"\r\n\r\n") {
                break i;
            }
            anyhow::ensure!(
                self.buf.len() <= MAX_RTSP_HEADER,
                "RTSP header exceeds 64 KiB"
            );
            self.fill()?;
        };
        anyhow::ensure!(i <= MAX_RTSP_HEADER, "RTSP header exceeds 64 KiB");
        let head = std::str::from_utf8(&self.buf[..i])
            .context("RTSP header is not UTF-8")?
            .to_owned();
        let n: usize = header_value(&head, "Content-Length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        anyhow::ensure!(n <= MAX_RTSP_BODY, "RTSP body exceeds 4 MiB");
        let total = i
            .checked_add(4)
            .and_then(|value| value.checked_add(n))
            .context("RTSP response length overflow")?;
        while self.buf.len() < total {
            self.fill()?;
        }
        let (body_start, parsed_length) = rtsp_response_bounds(&self.buf[..total])?;
        anyhow::ensure!(
            body_start == i + 4 && parsed_length == n,
            "inconsistent RTSP framing"
        );
        let body = self.buf[body_start..total].to_vec();
        self.buf.drain(..total);
        Ok((head, body))
    }

    fn next_event(&mut self) -> anyhow::Result<Event> {
        loop {
            self.apply_deadline()?;
            if self.buf.len() < 4 {
                self.fill()?;
                continue;
            }
            if self.buf[0] == b'$' {
                let ch = self.buf[1];
                let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                if self.buf.len() < 4 + len {
                    self.fill()?;
                    continue;
                }
                let payload: Vec<u8> = self.buf[4..4 + len].to_vec();
                self.buf.drain(..4 + len);
                return Ok(Event::Data(ch, payload));
            }
            if self.buf.starts_with(b"RTSP/") {
                let (head, body) = self.read_response()?;
                return Ok(Event::Rtsp(head, body));
            }
            let next_data = self.buf[1..]
                .iter()
                .position(|byte| *byte == b'$')
                .map(|position| position + 1);
            let next_rtsp = find(&self.buf[1..], b"RTSP/").map(|position| position + 1);
            if let Some(next) = next_data.into_iter().chain(next_rtsp).min() {
                self.buf.drain(..next);
            } else if self.buf.len() > 4 {
                let discard = self.buf.len() - 4;
                self.buf.drain(..discard);
            } else {
                self.fill()?;
            }
        }
    }

    fn send(&mut self, method: &str, url: &str, extra: &str) -> anyhow::Result<()> {
        self.apply_deadline()?;
        anyhow::ensure!(
            !method.contains(['\r', '\n'])
                && url.bytes().all(|byte| byte.is_ascii_graphic())
                && !extra.contains("\r\n\r\n"),
            "invalid RTSP request field"
        );
        self.cseq += 1;
        let mut h = format!(
            "{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: irisd-probe\r\n",
            self.cseq
        );
        if let Some(session) = &self.session {
            anyhow::ensure!(safe_session_value(session), "invalid RTSP session value");
            let _ = write!(h, "Session: {session}\r\n");
        }
        if let (Some(realm), Some(nonce)) = (&self.realm, &self.nonce) {
            anyhow::ensure!(
                safe_digest_value(&self.user)
                    && safe_digest_value(realm)
                    && safe_digest_value(nonce),
                "invalid RTSP digest field"
            );
            let ha1 = hex::encode(Md5::digest(format!(
                "{}:{realm}:{}",
                self.user, self.password
            )));
            let ha2 = hex::encode(Md5::digest(format!("{method}:{url}")));
            let response = hex::encode(Md5::digest(format!("{ha1}:{nonce}:{ha2}")));
            let _ = write!(
                h,
                "Authorization: Digest username=\"{}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{url}\", response=\"{response}\"\r\n",
                self.user
            );
        }
        self.stream
            .write_all(format!("{h}{extra}\r\n").as_bytes())?;
        Ok(())
    }

    fn rtsp(&mut self, method: &str, url: &str, extra: &str) -> anyhow::Result<(String, Vec<u8>)> {
        self.send(method, url, extra)?;
        let expected_cseq = self.cseq;
        loop {
            if let Event::Rtsp(head, body) = self.next_event()? {
                validate_cseq(&head, expected_cseq)?;
                return Ok((head, body));
            }
        }
    }
}

fn validate_cseq(head: &str, expected: u32) -> anyhow::Result<()> {
    let cseqs = head
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("CSeq")
                .then_some(value.trim())
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(cseqs.len() == 1, "RTSP response must contain one CSeq");
    let received = cseqs[0]
        .parse::<u32>()
        .context("RTSP response has an invalid CSeq")?;
    anyhow::ensure!(
        received == expected,
        "RTSP response CSeq {received} does not match request {expected}"
    );
    Ok(())
}

fn status_line(head: &str) -> &str {
    head.lines().next().unwrap_or("")
}

fn status_code(head: &str) -> anyhow::Result<u16> {
    let mut fields = status_line(head).split_whitespace();
    anyhow::ensure!(
        fields.next() == Some("RTSP/1.0"),
        "invalid RTSP status line"
    );
    fields
        .next()
        .context("missing RTSP status code")?
        .parse::<u16>()
        .context("invalid RTSP status code")
}

fn require_success(operation: &str, head: &str) -> anyhow::Result<()> {
    let status = status_code(head)?;
    anyhow::ensure!(
        (200..300).contains(&status),
        "{operation} returned RTSP {status}: {}",
        terminal_safe(status_line(head))
    );
    Ok(())
}

struct Frame {
    ts: u64,
    size: usize,
    key: bool,
    arrival: Instant,
    had_marker: bool,
}

#[derive(Default)]
struct Rtp {
    pkts: u64,
    bytes: u64,
    fu: u64,
    ap: u64,
    single: u64,
    marker: u64,
    maxpay: usize,
    gaps: u64,
    pad: u64,
    ext: u64,
}

/// Writes camera-controlled `bytes` to `path` without following a symlink,
/// blocking on a FIFO, or writing through another name: the path must be absent,
/// or name a caller-owned regular file with a single link, which is then truncated.
fn write_output(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::from_raw_mode(0o644),
    )
    .with_context(|| format!("opening {} without following links", path.display()))?;
    let mut file = std::fs::File::from(fd);
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == rustix::process::getuid().as_raw(),
        "{} must be a new file or a caller-owned single-link regular file",
        path.display()
    );
    // Truncate only after the checks, so a hard link to another file survives.
    file.set_len(0)?;
    file.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))
}

/// Runs the tool.
#[allow(clippy::too_many_lines)]
pub fn run(args: &Args) -> anyhow::Result<()> {
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
    let url = format!("rtsp://{authority}:554{path}");
    let stream = TcpStream::connect_timeout(&SocketAddr::new(host, 554), Duration::from_secs(10))
        .context("connecting")?;
    let io_timeout = Duration::from_secs_f64(args.socket_timeout);
    let mut conn = Conn {
        stream,
        buf: Vec::new(),
        chunk: vec![0u8; 262_144].into_boxed_slice(),
        cseq: 0,
        realm: None,
        nonce: None,
        session: None,
        user: args.user.clone(),
        password: args.password.expose().to_owned(),
        deadline: Instant::now() + Duration::from_secs(30),
        io_timeout,
    };
    let (options, _) = conn.rtsp("OPTIONS", &url, "")?;
    require_success("OPTIONS", &options)?;
    let (mut head, mut body) = conn.rtsp("DESCRIBE", &url, "Accept: application/sdp\r\n")?;
    if status_code(&head)? == 401 {
        conn.realm = quoted(&head, "realm");
        conn.nonce = quoted(&head, "nonce");
        anyhow::ensure!(
            conn.realm.is_some() && conn.nonce.is_some(),
            "DESCRIBE authentication challenge is incomplete"
        );
        (head, body) = conn.rtsp("DESCRIBE", &url, "Accept: application/sdp\r\n")?;
    }
    require_success("DESCRIBE", &head)?;
    let shown: Vec<&str> = head
        .lines()
        .filter(|l| {
            ["Server", "Content-Base", "Content-Type", "Cache-Control"]
                .contains(&l.split(':').next().unwrap_or(""))
        })
        .collect();
    println!("DESCRIBE headers: {shown:?}");
    let base = header_value(&head, "Content-Base").unwrap_or_else(|| format!("{url}/"));
    let sdp = String::from_utf8_lossy(&body).into_owned();
    let mut ctrl: BTreeMap<String, Option<String>> = BTreeMap::new();
    for block in sdp.split("\nm=").skip(1) {
        let kind = block.split_whitespace().next().unwrap_or("").to_owned();
        let control = block
            .lines()
            .find_map(|l| l.trim().strip_prefix("a=control:").map(str::to_owned));
        ctrl.insert(kind, control);
    }
    println!("track controls: {ctrl:?}");
    let ctrl_url = |control: &str| {
        if control.starts_with("rtsp://") {
            control.to_owned()
        } else {
            format!("{base}{control}")
        }
    };
    let video = ctrl
        .get("video")
        .cloned()
        .flatten()
        .context("no video track")?;
    let (head, _) = conn.rtsp(
        "SETUP",
        &ctrl_url(&video),
        "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n",
    )?;
    require_success("video SETUP", &head)?;
    let outcome = (|| -> anyhow::Result<()> {
        let transport: Vec<&str> = head
            .lines()
            .filter(|l| l.starts_with("Transport") || l.starts_with("Session"))
            .collect();
        println!(
            "SETUP video: {} | {transport:?}",
            terminal_safe(status_line(&head))
        );
        conn.session = header_value(&head, "Session")
            .map(|s| s.split(';').next().unwrap_or("").trim().to_owned());
        anyhow::ensure!(
            conn.session
                .as_ref()
                .is_some_and(|session| { !session.is_empty() && safe_session_value(session) }),
            "video SETUP response has no valid session"
        );
        let have_meta = ctrl.get("application").cloned().flatten();
        if let Some(meta) = &have_meta {
            let (head, _) = conn.rtsp(
                "SETUP",
                &ctrl_url(meta),
                "Transport: RTP/AVP/TCP;unicast;interleaved=4-5\r\n",
            )?;
            require_success("metadata SETUP", &head)?;
            println!("SETUP metadata: {}", terminal_safe(status_line(&head)));
        }
        let (head, _) = conn.rtsp("PLAY", &url, "Range: npt=0.000-\r\n")?;
        require_success("PLAY", &head)?;
        let info: Vec<&str> = head
            .lines()
            .filter(|l| l.starts_with("RTP-Info") || l.starts_with("Range"))
            .collect();
        println!("PLAY: {} | {info:?}", terminal_safe(status_line(&head)));

        let start = Instant::now();
        let t_end = start + Duration::from_secs_f64(args.seconds);
        conn.deadline = t_end;
        let mut next_ka = if args.keepalive > 0.0 {
            Some(start + Duration::from_secs_f64(args.keepalive))
        } else {
            None
        };
        let mut pending_ka = None;
        let mut rtp = Rtp::default();
        let mut seq_prev: Option<u16> = None;
        let mut frames: Vec<Frame> = Vec::new();
        let mut cur: Option<Frame> = None;
        let mut wrap: u64 = 0;
        let mut last32: Option<u32> = None;
        let mut nal_types: BTreeMap<u8, u64> = BTreeMap::new();
        let mut srs: Vec<(f64, f64, u32, u32, u32)> = Vec::new();
        let mut rtcp_types: BTreeMap<u8, u64> = BTreeMap::new();
        let (mut meta_pkts, mut meta_bytes) = (0u64, 0u64);
        let mut meta_sample: Vec<u8> = Vec::new();
        while Instant::now() < t_end {
            if pending_ka.is_none()
                && let Some(ka) = next_ka
                && Instant::now() > ka
            {
                conn.send("GET_PARAMETER", &url, "")?;
                pending_ka = Some(conn.cseq);
                next_ka = None;
            }
            let ev = match conn.next_event() {
                Ok(ev) => ev,
                Err(e)
                    if e.downcast_ref::<std::io::Error>().is_some_and(|io| {
                        matches!(
                            io.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        )
                    }) =>
                {
                    println!(
                        "!! recv timeout (no data for {:.0} s) at {:.1}s; frames so far={}",
                        args.socket_timeout,
                        start.elapsed().as_secs_f64(),
                        frames.len()
                    );
                    break;
                }
                Err(e) => return Err(e),
            };
            let (ch, pkt) = match ev {
                Event::Rtsp(head, _) => {
                    let expected = pending_ka
                        .take()
                        .context("unsolicited RTSP response while streaming")?;
                    validate_cseq(&head, expected)?;
                    require_success("GET_PARAMETER", &head)?;
                    next_ka = Some(Instant::now() + Duration::from_secs_f64(args.keepalive));
                    println!(
                        "[keepalive acknowledged at {:.1}s]",
                        start.elapsed().as_secs_f64()
                    );
                    continue;
                }
                Event::Data(ch, pkt) => (ch, pkt),
            };
            let now_i = Instant::now();
            if ch == 0 {
                let (off, end) = rtp_payload_bounds(&pkt)?;
                let (b0, b1) = (pkt[0], pkt[1]);
                let mk = b1 >> 7;
                let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
                let ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
                if b0 & 0x10 != 0 {
                    rtp.ext += 1;
                }
                let pay = &pkt[off..end];
                if b0 & 0x20 != 0 {
                    rtp.pad += 1;
                }
                rtp.pkts += 1;
                rtp.bytes += pkt.len() as u64;
                rtp.maxpay = rtp.maxpay.max(pay.len());
                if seq_prev.is_some_and(|prev| seq.wrapping_sub(prev) != 1) {
                    rtp.gaps += 1;
                }
                seq_prev = Some(seq);
                if let Some(last) = last32
                    && ts < last
                    && last - ts > 1 << 31
                {
                    wrap += 1 << 32;
                }
                last32 = Some(ts);
                let tsu = wrap + u64::from(ts);
                if cur.as_ref().is_some_and(|f| f.ts != tsu) {
                    frames.push(cur.take().expect("checked"));
                }
                let frame = cur.get_or_insert(Frame {
                    ts: tsu,
                    size: 0,
                    key: false,
                    arrival: now_i,
                    had_marker: false,
                });
                frame.size = frame
                    .size
                    .checked_add(pay.len())
                    .context("RTP frame size overflow")?;
                frame.arrival = now_i;
                if pay.is_empty() {
                    continue;
                }
                let t = (pay[0] >> 1) & 0x3f;
                let is_irap = |nt: u8| (19..=21).contains(&nt);
                if t == 49 {
                    anyhow::ensure!(pay.len() >= 3, "truncated H.265 fragmentation unit");
                    let fh = pay[2];
                    let nt = fh & 0x3f;
                    rtp.fu += 1;
                    if fh & 0x80 != 0 {
                        *nal_types.entry(nt).or_default() += 1;
                        frame.key |= is_irap(nt);
                    }
                } else if t == 48 {
                    rtp.ap += 1;
                    let mut o = 2;
                    let mut units = 0usize;
                    while o + 3 <= pay.len() {
                        let sz = u16::from_be_bytes([pay[o], pay[o + 1]]) as usize;
                        let next = o
                            .checked_add(2)
                            .and_then(|value| value.checked_add(sz))
                            .context("H.265 aggregation packet overflow")?;
                        anyhow::ensure!(
                            sz >= 2 && next <= pay.len(),
                            "truncated H.265 aggregation unit"
                        );
                        let nt = (pay[o + 2] >> 1) & 0x3f;
                        *nal_types.entry(nt).or_default() += 1;
                        frame.key |= is_irap(nt);
                        o = next;
                        units += 1;
                    }
                    anyhow::ensure!(
                        units > 0 && o == pay.len(),
                        "empty or trailing H.265 aggregation bytes"
                    );
                } else {
                    rtp.single += 1;
                    *nal_types.entry(t).or_default() += 1;
                    frame.key |= is_irap(t);
                }
                if mk == 1 {
                    rtp.marker += 1;
                    if let Some(mut done) = cur.take() {
                        done.had_marker = true;
                        frames.push(done);
                    }
                }
            } else if ch == 1 {
                let mut o = 0;
                while o + 4 <= pkt.len() {
                    let pt = pkt[o + 1];
                    let ln = u16::from_be_bytes([pkt[o + 2], pkt[o + 3]]) as usize;
                    let sz = (ln + 1).checked_mul(4).context("RTCP length overflow")?;
                    anyhow::ensure!(
                        o.checked_add(sz).is_some_and(|end| end <= pkt.len()),
                        "truncated RTCP packet"
                    );
                    *rtcp_types.entry(pt).or_default() += 1;
                    if pt == 200 && sz >= 28 {
                        let w = |i: usize| {
                            u32::from_be_bytes([
                                pkt[o + i],
                                pkt[o + i + 1],
                                pkt[o + i + 2],
                                pkt[o + i + 3],
                            ])
                        };
                        let (msw, lsw, rts, pc, oc) = (w(8), w(12), w(16), w(20), w(24));
                        let wall = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_or(0.0, |d| d.as_secs_f64());
                        srs.push((
                            wall,
                            (f64::from(msw) - 2_208_988_800.0) + f64::from(lsw) / 4_294_967_296.0,
                            rts,
                            pc,
                            oc,
                        ));
                    }
                    o += sz;
                }
                anyhow::ensure!(o == pkt.len(), "trailing RTCP bytes");
            } else if ch == 4 {
                let (payload_start, payload_end) = rtp_payload_bounds(&pkt)?;
                meta_pkts += 1;
                meta_bytes += pkt.len() as u64;
                if meta_sample.len() < 6000 {
                    let remaining = 6000 - meta_sample.len();
                    let payload = &pkt[payload_start..payload_end];
                    meta_sample.extend_from_slice(&payload[..payload.len().min(remaining)]);
                }
            }
        }
        anyhow::ensure!(
            pending_ka.is_none(),
            "stream ended before GET_PARAMETER was acknowledged"
        );
        anyhow::ensure!(frames.len() > 1, "fewer than two frames received");
        let span = frames[frames.len() - 1]
            .arrival
            .duration_since(frames[0].arrival)
            .as_secs_f64()
            .max(1e-9);
        let keys: Vec<&Frame> = frames.iter().filter(|f| f.key).collect();
        println!(
            "\n== RTP: pkts={} bytes={} ({:.2} Mbps over {span:.1}s) maxpayload={} FU={} AP={} single={} marker={} seqgaps={} ext={} pad={}",
            rtp.pkts,
            rtp.bytes,
            rtp.bytes as f64 * 8.0 / span / 1e6,
            rtp.maxpay,
            rtp.fu,
            rtp.ap,
            rtp.single,
            rtp.marker,
            rtp.gaps,
            rtp.ext,
            rtp.pad
        );
        let avg = |fs: &[&Frame]| {
            if fs.is_empty() {
                0.0
            } else {
                fs.iter().map(|f| f.size as f64).sum::<f64>() / fs.len() as f64
            }
        };
        let p_frames: Vec<&Frame> = frames.iter().filter(|f| !f.key).collect();
        println!(
            "== frames={} ({:.2} fps) keyframes={} frames_without_marker={} avgI={:.0}B avgP={:.0}B",
            frames.len(),
            frames.len() as f64 / span,
            keys.len(),
            frames.iter().filter(|f| !f.had_marker).count(),
            avg(&keys),
            avg(&p_frames)
        );
        println!("== NAL types: {nal_types:?}");
        let mut deltas: BTreeMap<u64, u64> = BTreeMap::new();
        for w in frames.windows(2) {
            *deltas.entry(w[1].ts.wrapping_sub(w[0].ts)).or_default() += 1;
        }
        let mut top: Vec<(u64, u64)> = deltas.into_iter().collect();
        top.sort_by_key(|d| std::cmp::Reverse(d.1));
        top.truncate(10);
        println!("== RTP ts deltas (ticks:count): {top:?}");
        let xs: Vec<f64> = frames
            .iter()
            .map(|f| f.arrival.duration_since(frames[0].arrival).as_secs_f64())
            .collect();
        let ys: Vec<f64> = frames
            .iter()
            .map(|f| (f.ts - frames[0].ts) as f64 / 90_000.0)
            .collect();
        let n = xs.len() as f64;
        let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
        let den: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
        let slope = if den > 0.0 {
            xs.iter()
                .zip(&ys)
                .map(|(x, y)| (x - mx) * (y - my))
                .sum::<f64>()
                / den
        } else {
            1.0
        };
        let res: Vec<f64> = xs
            .iter()
            .zip(&ys)
            .map(|(x, y)| y - (my + slope * (x - mx)))
            .collect();
        let stdev = (res.iter().map(|r| r * r).sum::<f64>() / n).sqrt();
        println!(
            "== RTP clock vs host monotonic: slope={slope:.6} ({:+.0} ppm), residual stdev={:.1} ms, max|res|={:.1} ms",
            (slope - 1.0) * 1e6,
            stdev * 1000.0,
            res.iter().map(|r| r.abs()).fold(0.0, f64::max) * 1000.0
        );
        let ki: Vec<usize> = frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.key)
            .map(|(i, _)| i)
            .collect();
        let mut gops: BTreeMap<usize, u64> = BTreeMap::new();
        for w in ki.windows(2) {
            *gops.entry(w[1] - w[0]).or_default() += 1;
        }
        println!("== GOP lengths (frames): {gops:?}");
        println!("== RTCP packets by type: {rtcp_types:?}; SRs={}", srs.len());
        if !srs.is_empty() {
            let iv: Vec<f64> = srs.windows(2).map(|w| w[1].0 - w[0].0).collect();
            let offs: Vec<f64> = srs.iter().map(|sr| sr.1 - sr.0).collect();
            println!(
                "   SR interval mean={:.1}s; SR NTP minus host wall: mean={:+.3}s min={:+.3} max={:+.3}",
                if iv.is_empty() {
                    0.0
                } else {
                    iv.iter().sum::<f64>() / iv.len() as f64
                },
                offs.iter().sum::<f64>() / offs.len() as f64,
                offs.iter().copied().fold(f64::INFINITY, f64::min),
                offs.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            );
            println!(
                "   first SR: ntp={} rtp_ts={} pkts={} octets={}",
                crate::time::iso(srs[0].1),
                srs[0].2,
                srs[0].3,
                srs[0].4
            );
            if srs.len() > 1 {
                let (a, b) = (srs[0], srs[srs.len() - 1]);
                let dt = b.1 - a.1;
                if dt > 0.0 {
                    println!(
                        "   SR-implied RTP rate: {:.1} Hz over {dt:.1}s (nominal 90000)",
                        f64::from(b.2.wrapping_sub(a.2)) / dt
                    );
                }
            }
        }
        println!("== ONVIF metadata track: pkts={meta_pkts} bytes={meta_bytes}");
        if !meta_sample.is_empty() {
            write_output(&args.meta_out, &meta_sample)?;
            let text: String = String::from_utf8_lossy(&meta_sample)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            println!(
                "   sample: {}",
                terminal_safe(&text.chars().take(700).collect::<String>())
            );
        }
        Ok(())
    })();
    conn.deadline = Instant::now() + Duration::from_secs(2);
    let teardown = conn
        .rtsp("TEARDOWN", &url, "")
        .and_then(|(head, _)| require_success("TEARDOWN", &head));
    match outcome {
        Err(error) => {
            let _ = teardown;
            Err(error)
        }
        Ok(()) => teardown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_rtp_padding_and_extensions_are_rejected_without_panicking() {
        let mut packet = vec![0u8; 13];
        packet[0] = 0xa0;
        packet[12] = 255;
        assert!(rtp_payload_bounds(&packet).is_err());

        let mut zero_padding = vec![0u8; 13];
        zero_padding[0] = 0xa0;
        assert!(rtp_payload_bounds(&zero_padding).is_err());

        let mut extension = vec![0u8; 16];
        extension[0] = 0x90;
        extension[14..16].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(rtp_payload_bounds(&extension).is_err());

        let valid = [0x80, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 42];
        assert_eq!(rtp_payload_bounds(&valid).unwrap(), (12, 13));
    }

    #[test]
    fn the_password_comes_from_the_environment_and_stays_out_of_debug() {
        let expected = std::env::var("PATH").expect("tests run with PATH set");
        let args = Args::try_parse_from([
            "rtsp-probe",
            "--password-env",
            "PATH",
            "192.0.2.10",
            "user",
            "90",
            "0",
            "meta.xml",
        ])
        .unwrap();
        assert_eq!(args.password.expose(), expected);
        assert_eq!(args.user, "user");
        assert!(!format!("{args:?}").contains(&expected));

        let error = Args::try_parse_from([
            "rtsp-probe",
            "--password-env",
            "IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A",
            "192.0.2.10",
            "user",
            "90",
            "0",
            "meta.xml",
        ])
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("IRIS_TOOLS_TEST_UNSET_PASSWORD_8F3A") && error.contains("not set"),
            "{error}"
        );
        assert!(
            Args::try_parse_from(["rtsp-probe", "192.0.2.10", "user", "90", "0", "meta.xml"])
                .is_err(),
            "the password variable is required"
        );
    }

    #[test]
    fn metadata_output_refuses_links_fifos_and_shared_files() {
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

        let fifo = directory.path().join("fifo.xml");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let started = Instant::now();
        assert!(write_output(&fifo, b"<camera-controlled/>").is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a FIFO never blocks"
        );

        let regular = directory.path().join("meta.xml");
        std::fs::write(&regular, b"a much longer earlier sample").unwrap();
        write_output(&regular, b"<new/>").unwrap();
        assert_eq!(std::fs::read(&regular).unwrap(), b"<new/>");
        let fresh = directory.path().join("fresh.xml");
        write_output(&fresh, b"<fresh/>").unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"<fresh/>");
    }

    #[test]
    fn float_arguments_reject_nonfinite_negative_and_excessive_values() {
        for invalid in ["NaN", "inf", "-1", "86401"] {
            assert!(stream_seconds(invalid).is_err(), "{invalid}");
        }
        assert_eq!(keepalive_seconds("0").unwrap(), 0.0);
        assert!(timeout_seconds("0").is_err());
        assert!(safe_digest_value("camera-user"));
        assert!(!safe_digest_value("camera\r\nInjected: yes"));
        assert!(!safe_digest_value("camera\"user"));
        assert!(safe_session_value("ABC123-._"));
        assert!(!safe_session_value("ABC 123"));
        assert!(validate_cseq("RTSP/1.0 200 OK\r\nCSeq: 7", 7).is_ok());
        assert!(validate_cseq("RTSP/1.0 200 OK\r\nCSeq: 6", 7).is_err());
        assert!(validate_cseq("RTSP/1.0 200 OK\r\nCSeq: 7\r\nCSeq: 7", 7).is_err());
    }
}
