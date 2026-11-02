//! `rtsp-probe`: speaks RTSP over TCP to a camera (digest authentication),
//! sets up the video track interleaved and the ONVIF metadata track when
//! present, streams for a while, and reports the packetisation, marker use,
//! sequence gaps, timestamp deltas, clock drift, GOP lengths, RTCP sender
//! reports, and metadata traffic.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Parser;
use md5::{Digest, Md5};

/// Arguments of `rtsp-probe`.
#[derive(Debug, Parser)]
pub struct Args {
    /// Camera host.
    host: String,
    /// RTSP user.
    user: String,
    /// RTSP password (an argument so nothing secret is stored in a file).
    password: String,
    /// How long to stream.
    seconds: f64,
    /// The `subtype` of the vendor's `realmonitor` path.
    subtype: String,
    /// Where a sample of the metadata track is written.
    meta_out: std::path::PathBuf,
    /// `GET_PARAMETER` keepalive period in seconds; 0 disables it.
    #[arg(default_value_t = 25.0)]
    keepalive: f64,
    /// The RTSP path, or `-` for the vendor's `realmonitor` path.
    #[arg(default_value = "-")]
    path: String,
    /// Seconds without data that end the run.
    #[arg(default_value_t = 8.0)]
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

impl Conn {
    fn fill(&mut self) -> anyhow::Result<()> {
        let n = self.stream.read(&mut self.chunk)?;
        anyhow::ensure!(n > 0, "server closed");
        let (buf, chunk) = (&mut self.buf, &self.chunk);
        buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn read_response(&mut self) -> anyhow::Result<(String, Vec<u8>)> {
        let i = loop {
            if let Some(i) = find(&self.buf, b"\r\n\r\n") {
                break i;
            }
            self.fill()?;
        };
        let head = String::from_utf8_lossy(&self.buf[..i]).into_owned();
        self.buf.drain(..i + 4);
        let n: usize = header_value(&head, "Content-Length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while self.buf.len() < n {
            self.fill()?;
        }
        let body: Vec<u8> = self.buf.drain(..n).collect();
        Ok((head, body))
    }

    fn next_event(&mut self) -> anyhow::Result<Event> {
        loop {
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
            self.buf.remove(0);
        }
    }

    fn send(&mut self, method: &str, url: &str, extra: &str) -> anyhow::Result<()> {
        self.cseq += 1;
        let mut h = format!(
            "{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: irisd-probe\r\n",
            self.cseq
        );
        if let Some(session) = &self.session {
            let _ = write!(h, "Session: {session}\r\n");
        }
        if let (Some(realm), Some(nonce)) = (&self.realm, &self.nonce) {
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
        loop {
            if let Event::Rtsp(head, body) = self.next_event()? {
                return Ok((head, body));
            }
        }
    }
}

fn status_line(head: &str) -> &str {
    head.lines().next().unwrap_or("")
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

/// Runs the tool.
#[allow(clippy::too_many_lines)]
pub fn run(args: &Args) -> anyhow::Result<()> {
    let path = if args.path == "-" {
        format!(
            "/cam/realmonitor?channel=1&subtype={}&unicast=true&proto=Onvif",
            args.subtype
        )
    } else {
        args.path.clone()
    };
    let url = format!("rtsp://{}:554{path}", args.host);
    let stream = TcpStream::connect((args.host.as_str(), 554)).context("connecting")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut conn = Conn {
        stream,
        buf: Vec::new(),
        chunk: vec![0u8; 262_144].into_boxed_slice(),
        cseq: 0,
        realm: None,
        nonce: None,
        session: None,
        user: args.user.clone(),
        password: args.password.clone(),
    };
    conn.rtsp("OPTIONS", &url, "")?;
    let (mut head, mut body) = conn.rtsp("DESCRIBE", &url, "Accept: application/sdp\r\n")?;
    if status_line(&head).contains(" 401 ") {
        conn.realm = quoted(&head, "realm");
        conn.nonce = quoted(&head, "nonce");
        (head, body) = conn.rtsp("DESCRIBE", &url, "Accept: application/sdp\r\n")?;
    }
    anyhow::ensure!(
        status_line(&head).contains(" 200 "),
        "DESCRIBE failed: {head}"
    );
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
    let transport: Vec<&str> = head
        .lines()
        .filter(|l| l.starts_with("Transport") || l.starts_with("Session"))
        .collect();
    println!("SETUP video: {} | {transport:?}", status_line(&head));
    conn.session =
        header_value(&head, "Session").map(|s| s.split(';').next().unwrap_or("").trim().to_owned());
    let have_meta = ctrl.get("application").cloned().flatten();
    if let Some(meta) = &have_meta {
        let (head, _) = conn.rtsp(
            "SETUP",
            &ctrl_url(meta),
            "Transport: RTP/AVP/TCP;unicast;interleaved=4-5\r\n",
        )?;
        println!("SETUP metadata: {}", status_line(&head));
    }
    let (head, _) = conn.rtsp("PLAY", &url, "Range: npt=0.000-\r\n")?;
    let info: Vec<&str> = head
        .lines()
        .filter(|l| l.starts_with("RTP-Info") || l.starts_with("Range"))
        .collect();
    println!("PLAY: {} | {info:?}", status_line(&head));

    conn.stream
        .set_read_timeout(Some(Duration::from_secs_f64(args.socket_timeout)))?;
    let start = Instant::now();
    let t_end = start + Duration::from_secs_f64(args.seconds);
    let mut next_ka = if args.keepalive > 0.0 {
        Some(start + Duration::from_secs_f64(args.keepalive))
    } else {
        None
    };
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
        if let Some(ka) = next_ka
            && Instant::now() > ka
        {
            conn.send("GET_PARAMETER", &url, "")?;
            next_ka = Some(ka + Duration::from_secs_f64(args.keepalive));
            println!("[keepalive sent at {:.1}s]", start.elapsed().as_secs_f64());
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
        let Event::Data(ch, pkt) = ev else { continue };
        let now_i = Instant::now();
        if ch == 0 && pkt.len() >= 12 {
            let (b0, b1) = (pkt[0], pkt[1]);
            let cc = (b0 & 0xf) as usize;
            let has_ext = (b0 >> 4) & 1;
            let pad = (b0 >> 5) & 1;
            let mk = b1 >> 7;
            let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
            let ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
            let mut off = 12 + 4 * cc;
            if has_ext == 1 && pkt.len() >= off + 4 {
                off += 4 + 4 * u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]) as usize;
                rtp.ext += 1;
            }
            let end = pkt.len()
                - if pad == 1 {
                    *pkt.last().unwrap_or(&0) as usize
                } else {
                    0
                };
            if off > end {
                continue;
            }
            let pay = &pkt[off..end];
            if pad == 1 {
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
            frame.size += pay.len();
            frame.arrival = now_i;
            if pay.is_empty() {
                continue;
            }
            let t = (pay[0] >> 1) & 0x3f;
            let is_irap = |nt: u8| (19..=21).contains(&nt);
            if t == 49 && pay.len() > 2 {
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
                while o + 3 <= pay.len() {
                    let sz = u16::from_be_bytes([pay[o], pay[o + 1]]) as usize;
                    let nt = (pay[o + 2] >> 1) & 0x3f;
                    *nal_types.entry(nt).or_default() += 1;
                    frame.key |= is_irap(nt);
                    o += 2 + sz;
                }
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
                let sz = (ln + 1) * 4;
                *rtcp_types.entry(pt).or_default() += 1;
                if pt == 200 && o + 28 <= pkt.len() {
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
        } else if ch == 4 {
            meta_pkts += 1;
            meta_bytes += pkt.len() as u64;
            if meta_sample.len() < 6000 && pkt.len() > 12 {
                meta_sample.extend_from_slice(&pkt[12..]);
            }
        }
    }
    let _ = conn.rtsp("TEARDOWN", &url, "");

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
        std::fs::write(&args.meta_out, &meta_sample)?;
        let text: String = String::from_utf8_lossy(&meta_sample)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        println!("   sample: {}", text.chars().take(700).collect::<String>());
    }
    Ok(())
}
