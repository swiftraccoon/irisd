// Copyright (C) The Retina Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! tokio-based [`Connection`].
//!
//! In theory there could be a similar async-std-based implementation.

use futures::{Sink, Stream};
use std::time::Instant;
use tokio::io::AsyncWrite;
use tokio::net::{TcpStream, UdpSocket};
use url::Host;

use crate::buf::MarkBuf;
use crate::inputs::{Input as _, Split};
use crate::rtsp::msg::OwnedMessage;
use crate::{Error, ErrorInt, RtspMessageContext};

use super::{ConnectionContext, ReceivedMessage, WallTime};

/// Default initial capacity for the read ring buffer (64 KiB).
const DEFAULT_READ_CAPACITY: usize = 64 * 1024;

/// The socket behind a connection: registered with tokio's reactor, or
/// detached from it for a caller that polls on its own schedule.
enum Io {
    /// A tokio stream: reads and writes wake through `epoll`.
    Reactor(TcpStream),
    /// A plain non-blocking socket. Reads and writes that would block return
    /// `Pending` without registering a waker, so the caller must poll again on
    /// its own timer; a write keeps its unsent remainder for that next poll.
    /// Nothing wakes `epoll` for this socket any more, and nothing blocks.
    Detached(std::net::TcpStream),
    /// The socket was consumed by a mode change that failed (`into_std` or
    /// `from_std`); every later operation fails.
    Detaching,
}

impl Io {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        match self {
            Io::Reactor(s) => s.as_raw_fd(),
            Io::Detached(s) => s.as_raw_fd(),
            Io::Detaching => -1,
        }
    }
}

/// A RTSP connection which implements `Stream`, `Sink`, and `Unpin`.
pub(crate) struct Connection {
    stream: Io,
    /// A detached socket's last read was short, so its queue is empty.
    drained: bool,
    /// When the last read returned; every message decoded from the bytes it
    /// delivered carries this time, so the clock is read once per read rather
    /// than once per message.
    read_stamp: ReadStamp,
    ctx: ConnectionContext,
    parser: crate::rtsp::parse::Parser,
    read_buf: MarkBuf,
    /// Write buffer for outgoing messages (written then flushed).
    write_buf: Vec<u8>,
}

impl Connection {
    pub(crate) async fn connect(host: Host<&str>, port: u16) -> Result<Self, std::io::Error> {
        let stream = match host {
            Host::Domain(h) => TcpStream::connect((h, port)).await,
            Host::Ipv4(h) => TcpStream::connect((h, port)).await,
            Host::Ipv6(h) => TcpStream::connect((h, port)).await,
        }?;
        Self::from_stream(stream)
    }

    pub(crate) fn from_stream(stream: TcpStream) -> Result<Self, std::io::Error> {
        let established_wall = WallTime::now();
        let local_addr = stream.local_addr()?;
        let peer_addr = stream.peer_addr()?;
        Ok(Self {
            stream: Io::Reactor(stream),
            drained: false,
            read_stamp: ReadStamp::now(),
            ctx: ConnectionContext {
                local_addr,
                peer_addr,
                established_wall,
            },
            parser: crate::rtsp::parse::Parser::builder()
                .max_message_size(crate::rtsp::parse::MAX_CONTROL_MESSAGE_BYTES)
                .build(),
            read_buf: MarkBuf::new(DEFAULT_READ_CAPACITY),
            write_buf: Vec::new(),
        })
    }

    pub(crate) fn ctx(&self) -> &ConnectionContext {
        &self.ctx
    }

    /// Sets `SO_RCVLOWAT` on the RTSP socket: `poll`/`epoll` then report the
    /// socket readable only once at least `bytes` are queued, so a receiver
    /// of a steady RTP flow wakes once per `bytes` instead of once per
    /// packet. Only meaningful once data flows continuously (after `PLAY`);
    /// set earlier, small RTSP responses would never trigger readiness.
    pub(crate) fn set_recv_low_water(&self, bytes: u32) -> std::io::Result<()> {
        let value =
            libc::c_int::try_from(bytes).map_err(|_| std::io::Error::other("value too large"))?;
        // SAFETY: `value` is a live `c_int` for the whole call and the length
        // passed is its size. The descriptor is borrowed from `self.stream`
        // (-1 after a failed mode change, which `setsockopt` rejects with
        // `EBADF`).
        let rc = unsafe {
            libc::setsockopt(
                self.stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVLOWAT,
                std::ptr::from_ref(&value).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Takes the socket out of tokio's reactor. From then on reads and writes
    /// that would block return `Pending` without a waker and never wake
    /// `epoll`; the caller polls the connection on its own timer, and a flush
    /// continues from its unsent remainder on each poll. Idempotent. A failure
    /// loses the socket (`Io::Detaching`).
    pub(crate) fn detach_from_reactor(&mut self) -> std::io::Result<()> {
        match std::mem::replace(&mut self.stream, Io::Detaching) {
            // `into_std` deregisters the descriptor from the reactor and
            // returns it in non-blocking mode.
            Io::Reactor(reactor) => self.stream = Io::Detached(reactor.into_std()?),
            other => self.stream = other,
        }
        Ok(())
    }

    /// Undoes the playing-state socket policy for a request whose caller
    /// awaits it, such as a `TEARDOWN`: resets the receive low-water mark to
    /// one byte and returns a detached socket to tokio's reactor, so the
    /// request's writes and its short response wake the calling task. Must
    /// be called within a tokio runtime. Idempotent. A failure to register
    /// the socket loses it (`Io::Detaching`).
    pub(crate) fn return_to_reactor(&mut self) -> std::io::Result<()> {
        self.set_recv_low_water(1)?;
        match std::mem::replace(&mut self.stream, Io::Detaching) {
            Io::Detached(std_stream) => {
                self.stream = Io::Reactor(TcpStream::from_std(std_stream)?);
                self.drained = false;
            }
            other => self.stream = other,
        }
        Ok(())
    }

    pub(crate) fn read_buf(&self) -> &MarkBuf {
        &self.read_buf
    }

    /// Reads body data from the ring buffer as `Bytes`.
    ///
    /// This copies the data out. Use for cold paths (RTSP responses) or the
    /// public `PacketItem` API. The `Demuxed` path reads directly from the
    /// ring buffer to avoid this copy.
    pub(crate) fn body_bytes(&self, body_pos: u64, body_len: u32) -> bytes::Bytes {
        let (s1, s2) = self.read_buf.split(body_pos, body_len as usize).slices();
        let mut buf = Vec::with_capacity(body_len as usize);
        buf.extend_from_slice(s1);
        buf.extend_from_slice(s2);
        bytes::Bytes::from(buf)
    }

    pub(crate) fn eof_ctx(&self) -> RtspMessageContext {
        RtspMessageContext {
            pos: self.parser.stream_pos() + self.read_buf.unparsed_len() as u64,
            received_wall: WallTime::now(),
            received: Instant::now(),
        }
    }
}

/// The wall and monotonic time a read returned, stamped on every message the
/// read delivered.
#[derive(Clone, Copy)]
struct ReadStamp {
    wall: WallTime,
    instant: Instant,
}

impl ReadStamp {
    fn now() -> Self {
        Self {
            wall: WallTime::now(),
            instant: Instant::now(),
        }
    }
}

/// Tries to decode a message from the read buffer without I/O.
///
/// Separated from `Connection` methods to avoid self-borrow issues.
///
/// When `eof` is true, the TCP stream has closed. `Incomplete` and empty
/// `Ok(None)` with leftover data are promoted to errors. A decoded message
/// is stamped with `stamp`, the time of the read that delivered its bytes.
fn try_decode(
    parser: &mut crate::rtsp::parse::Parser,
    read_buf: &mut MarkBuf,
    eof: bool,
    stamp: ReadStamp,
) -> Result<Option<ReceivedMessage>, CodecError> {
    use crate::rtsp::parse::FeedError;

    let pos = parser.stream_pos();
    let unparsed = read_buf.unparsed();
    let mut input = {
        let split = read_buf.unparsed_split();
        let (first, second) = split.slices();
        Split::new(first, second)
    };
    let initial_len = input.len();

    match parser.feed(&mut input) {
        Ok(None) => Ok(None), // idle, no data; caller distinguishes EOF from "need more"
        Ok(Some((msg, body_slice))) => {
            let consumed = initial_len - input.len();
            let body_len = body_slice.len();
            let body_pos = unparsed + (consumed - body_len) as u64;
            read_buf.advance_unparsed(unparsed + consumed as u64);
            Ok(Some(ReceivedMessage {
                msg,
                body_pos,
                body_len: body_len as u32,
                ctx: RtspMessageContext {
                    pos,
                    received_wall: stamp.wall,
                    received: stamp.instant,
                },
            }))
        }
        Err(FeedError::Incomplete(_)) if !eof => {
            // On Incomplete, the parser may have stably consumed some bytes
            // (e.g. header lines). Advance past them.
            let consumed = initial_len - input.len();
            if consumed > 0 {
                read_buf.advance_unparsed(unparsed + consumed as u64);
            }
            Ok(None)
        }
        Err(FeedError::Incomplete(inc)) => {
            // EOF with incomplete data: the message is truncated.
            Err(CodecError::ParseError {
                description: format!(
                    "Incomplete RTSP message at EOF ({inc}); buffered:\n{:#?}",
                    crate::hex::LimitedHex::from_split(read_buf.unparsed_split(), 128),
                ),
                pos,
            })
        }
        Err(FeedError::Invalid(inv)) => Err(CodecError::ParseError {
            description: format!(
                "Invalid RTSP message: {inv}; buffered:\n{:#?}",
                crate::hex::LimitedHex::from_split(read_buf.unparsed_split(), 128),
            ),
            pos: inv.pos,
        }),
    }
}

/// An intermediate error type used internally.
#[derive(Debug)]
#[allow(dead_code)] // IoError reserved for future use
enum CodecError {
    IoError(std::io::Error),
    ParseError { description: String, pos: u64 },
}

impl Stream for Connection {
    type Item = Result<ReceivedMessage, Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            // Try decoding from existing buffered data.
            match try_decode(&mut this.parser, &mut this.read_buf, false, this.read_stamp) {
                Ok(Some(msg)) => return std::task::Poll::Ready(Some(Ok(msg))),
                // A detached read shorter than the space offered left the
                // socket's queue empty: the caller, which polls until
                // `Pending` on every tick, gets `Pending` without a read that
                // would only return `EAGAIN`. The flag clears here so the next
                // poll (the next tick) reads again.
                Ok(None) if this.drained => {
                    this.drained = false;
                    return std::task::Poll::Pending;
                }
                Ok(None) => {}
                Err(e) => {
                    return std::task::Poll::Ready(Some(Err(codec_err_to_error(
                        &this.ctx,
                        &this.parser,
                        &this.read_buf,
                        e,
                    ))));
                }
            }

            // Need more data. Read from the stream into the ring buffer
            // using vectored I/O to fill both halves of the ring.
            if let Io::Reactor(stream) = &this.stream {
                match stream.poll_read_ready(cx) {
                    std::task::Poll::Ready(Ok(())) => {}
                    std::task::Poll::Ready(Err(error)) => {
                        let pos = this.parser.stream_pos() + this.read_buf.unparsed_len() as u64;
                        return std::task::Poll::Ready(Some(Err(wrap!(ErrorInt::RtspReadError {
                            conn_ctx: this.ctx,
                            msg_ctx: RtspMessageContext {
                                pos,
                                received_wall: WallTime::now(),
                                received: Instant::now(),
                            },
                            source: error,
                        }))));
                    }
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                }
            }
            let buffered = this.read_buf.unparsed_len();
            let remaining = this.parser.max_buffered_bytes().saturating_sub(buffered);
            if remaining == 0 {
                let error = CodecError::ParseError {
                    description: "RTSP control message exceeds 1 MiB".to_owned(),
                    pos: this.parser.stream_pos() + buffered as u64,
                };
                return std::task::Poll::Ready(Some(Err(codec_err_to_error(
                    &this.ctx,
                    &this.parser,
                    &this.read_buf,
                    error,
                ))));
            }
            let (first, second) = this.read_buf.spare_capacity(remaining.min(4096));
            let first_len = first.len().min(remaining);
            let second_len = second.len().min(remaining - first_len);
            let mut bufs = [
                std::io::IoSliceMut::new(&mut first[..first_len]),
                std::io::IoSliceMut::new(&mut second[..second_len]),
            ];
            let offered = bufs[0].len() + bufs[1].len();
            let read = match &mut this.stream {
                Io::Reactor(stream) => stream.try_read_vectored(&mut bufs),
                Io::Detached(stream) => {
                    use std::io::Read as _;
                    stream.read_vectored(&mut bufs)
                }
                Io::Detaching => Err(std::io::Error::other("connection is detaching")),
            };
            match read {
                Ok(0) => {
                    // EOF. Try decode with eof=true.
                    return match try_decode(
                        &mut this.parser,
                        &mut this.read_buf,
                        true,
                        ReadStamp::now(),
                    ) {
                        Ok(Some(msg)) => std::task::Poll::Ready(Some(Ok(msg))),
                        Ok(None) => std::task::Poll::Ready(None),
                        Err(e) => std::task::Poll::Ready(Some(Err(codec_err_to_error(
                            &this.ctx,
                            &this.parser,
                            &this.read_buf,
                            e,
                        )))),
                    };
                }
                Ok(n) => {
                    this.read_buf.advance_end(n);
                    this.read_stamp = ReadStamp::now();
                    this.drained = matches!(this.stream, Io::Detached(_)) && n < offered;
                    // Loop to try decoding again.
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    match this.stream {
                        // Spurious readiness; re-register and wait.
                        Io::Reactor(_) => continue,
                        // Drained; the caller polls again on its own schedule.
                        Io::Detached(_) | Io::Detaching => return std::task::Poll::Pending,
                    }
                }
                Err(error) => {
                    let pos = this.parser.stream_pos() + this.read_buf.unparsed_len() as u64;
                    return std::task::Poll::Ready(Some(Err(wrap!(ErrorInt::RtspReadError {
                        conn_ctx: this.ctx,
                        msg_ctx: RtspMessageContext {
                            pos,
                            received_wall: WallTime::now(),
                            received: Instant::now(),
                        },
                        source: error,
                    }))));
                }
            }
        }
    }
}

fn codec_err_to_error(
    ctx: &ConnectionContext,
    parser: &crate::rtsp::parse::Parser,
    read_buf: &MarkBuf,
    e: CodecError,
) -> Error {
    wrap!(match e {
        CodecError::IoError(error) => ErrorInt::RtspReadError {
            conn_ctx: *ctx,
            msg_ctx: RtspMessageContext {
                pos: parser.stream_pos() + read_buf.unparsed_len() as u64,
                received_wall: WallTime::now(),
                received: Instant::now(),
            },
            source: error,
        },
        CodecError::ParseError { description, pos } => ErrorInt::RtspFramingError {
            conn_ctx: *ctx,
            msg_ctx: RtspMessageContext {
                pos,
                received_wall: WallTime::now(),
                received: Instant::now(),
            },
            description,
        },
    })
}

impl Sink<OwnedMessage> for Connection {
    type Error = ErrorInt;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        // `start_send` replaces the buffer, so first finish the previous
        // message: a `TEARDOWN` must not follow half a keepalive.
        if self.write_buf.is_empty() {
            std::task::Poll::Ready(Ok(()))
        } else {
            self.poll_flush(cx)
        }
    }

    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        item: OwnedMessage,
    ) -> Result<(), Self::Error> {
        self.write_buf.clear();
        item.write(&mut self.write_buf)
            .expect("Vec Writer is infallible");
        Ok(())
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let this = &mut *self;
        while !this.write_buf.is_empty() {
            let written = match &mut this.stream {
                Io::Reactor(stream) => std::pin::Pin::new(stream).poll_write(cx, &this.write_buf),
                Io::Detached(stream) => {
                    use std::io::Write as _;
                    // Never block: a peer that stops reading must not stall
                    // the caller's thread. The unsent remainder stays in
                    // `write_buf` for the caller's next poll on its timer.
                    match stream.write(&this.write_buf) {
                        Ok(0) => std::task::Poll::Ready(Err(std::io::ErrorKind::WriteZero.into())),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::task::Poll::Pending
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        written => std::task::Poll::Ready(written),
                    }
                }
                Io::Detaching => {
                    std::task::Poll::Ready(Err(std::io::Error::other("connection is detaching")))
                }
            };
            match written {
                std::task::Poll::Ready(Ok(n)) => {
                    this.write_buf.drain(..n);
                }
                std::task::Poll::Ready(Err(e)) => {
                    return std::task::Poll::Ready(Err(ErrorInt::WriteError {
                        conn_ctx: this.ctx,
                        source: e,
                    }));
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
        let flushed = match &mut this.stream {
            Io::Reactor(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            Io::Detached(_) | Io::Detaching => std::task::Poll::Ready(Ok(())),
        };
        match flushed {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(ErrorInt::WriteError {
                conn_ctx: this.ctx,
                source: e,
            })),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.as_mut().poll_flush(cx) {
            std::task::Poll::Ready(Ok(())) => {}
            other => return other,
        }
        let this = &mut *self;
        let shut = match &mut this.stream {
            Io::Reactor(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            Io::Detached(stream) => {
                std::task::Poll::Ready(stream.shutdown(std::net::Shutdown::Write))
            }
            Io::Detaching => std::task::Poll::Ready(Ok(())),
        };
        match shut {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(ErrorInt::WriteError {
                conn_ctx: this.ctx,
                source: e,
            })),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// tokio-specific version of [`crate::UdpPair`].
pub(crate) struct UdpPair {
    pub(crate) rtp_port: u16,
    pub(crate) rtp_socket: UdpSocket,
    pub(crate) rtcp_socket: UdpSocket,
}

impl UdpPair {
    pub(crate) fn for_ip(ip_addr: std::net::IpAddr) -> Result<Self, std::io::Error> {
        let inner = crate::UdpPair::for_ip(ip_addr)?;
        inner.rtp_socket.set_nonblocking(true)?;
        inner.rtcp_socket.set_nonblocking(true)?;
        Ok(Self {
            rtp_port: inner.rtp_port,
            rtp_socket: UdpSocket::from_std(inner.rtp_socket)?,
            rtcp_socket: UdpSocket::from_std(inner.rtcp_socket)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn crlf_data() {
        let mut read_buf = MarkBuf::new(64);
        let data = b"\r\n$\x00\x00\x04asdfrest";
        let (first, _) = read_buf.spare_capacity(data.len());
        first[..data.len()].copy_from_slice(data);
        read_buf.advance_end(data.len());

        let mut parser = crate::rtsp::parse::Parser::default();
        let msg = try_decode(&mut parser, &mut read_buf, false, ReadStamp::now())
            .unwrap()
            .unwrap();
        // Read body from ring buffer.
        let (s1, s2) = read_buf.split(msg.body_pos, msg.body_len as usize).slices();
        assert_eq!(s1, b"asdf");
        assert!(s2.is_empty());
        assert_eq!(read_buf.unparsed_len(), 4); // "rest" remains
    }

    #[test]
    fn response_decode() {
        let mut read_buf = MarkBuf::new(64);
        let data = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n";
        let (first, _) = read_buf.spare_capacity(data.len());
        first[..data.len()].copy_from_slice(data);
        read_buf.advance_end(data.len());

        let mut parser = crate::rtsp::parse::Parser::default();
        let msg = try_decode(&mut parser, &mut read_buf, false, ReadStamp::now())
            .unwrap()
            .unwrap();
        assert!(matches!(msg.msg, crate::rtsp::msg::Message::Response(_)));
        assert_eq!(msg.body_len, 0);
        assert_eq!(read_buf.unparsed_len(), 0);
    }

    #[tokio::test]
    async fn production_limit_accepts_an_exact_control_message() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut body_len = crate::rtsp::parse::MAX_CONTROL_MESSAGE_BYTES;
            let head = loop {
                let head =
                    format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {body_len}\r\n\r\n");
                let adjusted = crate::rtsp::parse::MAX_CONTROL_MESSAGE_BYTES - head.len();
                if adjusted == body_len {
                    break head;
                }
                body_len = adjusted;
            };
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&vec![b'x'; body_len]).await.unwrap();
            body_len
        });

        let stream = TcpStream::connect(address).await.unwrap();
        let mut connection = Connection::from_stream(stream).unwrap();
        let message = tokio::time::timeout(Duration::from_secs(2), connection.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let body_len = server.await.unwrap();
        assert_eq!(message.body_len as usize, body_len);
        assert_eq!(
            connection.parser.stream_pos() as usize,
            crate::rtsp::parse::MAX_CONTROL_MESSAGE_BYTES
        );
    }

    /// A connection whose peer has a small receive buffer and reads nothing
    /// until told to; the connection's own send buffer is small too, so a
    /// large write cannot complete while the peer stalls.
    async fn stalled_peer() -> (Connection, std::net::TcpStream) {
        let socket =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        socket.set_recv_buffer_size(4096).unwrap();
        let address: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        socket.bind(&address.into()).unwrap();
        socket.listen(1).unwrap();
        let listener: std::net::TcpListener = socket.into();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        socket2::SockRef::from(&stream)
            .set_send_buffer_size(4096)
            .unwrap();
        let (peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        (Connection::from_stream(stream).unwrap(), peer)
    }

    /// Polls once with a waker that does nothing, as a timer-driven caller does.
    fn poll_once<T>(
        connection: &mut Connection,
        poll: impl FnOnce(
            std::pin::Pin<&mut Connection>,
            &mut std::task::Context<'_>,
        ) -> std::task::Poll<T>,
    ) -> std::task::Poll<T> {
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        poll(std::pin::Pin::new(connection), &mut cx)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn detached_flush_never_blocks_and_keeps_the_unsent_remainder() {
        let (mut connection, mut peer) = stalled_peer().await;
        connection.detach_from_reactor().unwrap();
        let message = pattern(1 << 20);
        connection.write_buf = message.clone();
        // Should a flush block, this starts reading after 10 s, which lets the
        // blocked write finish, so the test fails instead of hanging.
        let (done, watchdog) = std::sync::mpsc::channel::<()>();
        let mut stalled = peer.try_clone().unwrap();
        std::thread::spawn(move || {
            use std::io::Read as _;
            if watchdog.recv_timeout(Duration::from_secs(10)).is_err() {
                let mut sink = vec![0; 64 * 1024];
                while matches!(stalled.read(&mut sink), Ok(n) if n > 0) {}
            }
        });

        for _ in 0..5 {
            let started = Instant::now();
            assert!(poll_once(&mut connection, |c, cx| c.poll_flush(cx)).is_pending());
            assert!(poll_once(&mut connection, |c, cx| c.poll_ready(cx)).is_pending());
            assert!(
                started.elapsed() < Duration::from_millis(100),
                "a flush to a stalled peer took {:?}",
                started.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let unsent = connection.write_buf.len();
        assert!(
            0 < unsent && unsent < message.len(),
            "{unsent} bytes unsent"
        );
        assert!(
            connection.write_buf == message[message.len() - unsent..],
            "the unsent bytes are not the message's tail"
        );
        done.send(()).unwrap();

        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut received = Vec::new();
            peer.read_to_end(&mut received).map(|_| received)
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while poll_once(&mut connection, |c, cx| c.poll_flush(cx)).is_pending() {
            assert!(Instant::now() < deadline, "the flush never finished");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(connection.write_buf.is_empty());
        assert!(matches!(
            poll_once(&mut connection, |c, cx| c.poll_close(cx)),
            std::task::Poll::Ready(Ok(()))
        ));
        let received = reader.join().unwrap().unwrap();
        assert!(received == message, "the peer received other bytes");
    }

    #[tokio::test]
    async fn returning_to_the_reactor_wakes_for_writes_and_a_short_response() {
        let (mut connection, mut peer) = stalled_peer().await;
        connection.set_recv_low_water(32 * 1024).unwrap();
        connection.detach_from_reactor().unwrap();
        let message = pattern(256 * 1024);
        connection.write_buf = message.clone();
        // The peer starts reading when told to, or after 10 s, which also
        // ends a write that blocks, so the test fails instead of hanging.
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let expected = message.len();
        let peer = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let _ = gate.recv_timeout(Duration::from_secs(10));
            let mut received = vec![0; expected];
            peer.read_exact(&mut received)?;
            peer.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n")?;
            // Hold the connection open until the client closes it.
            let mut rest = Vec::new();
            peer.read_to_end(&mut rest)?;
            Ok::<_, std::io::Error>(received)
        });
        assert!(poll_once(&mut connection, |c, cx| c.poll_flush(cx)).is_pending());

        connection.return_to_reactor().unwrap();
        let _ = go.send(());
        // No timer polls here: only the reactor's wake-ups make progress.
        tokio::time::timeout(
            Duration::from_secs(10),
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut connection).poll_ready(cx)),
        )
        .await
        .expect("the reactor woke the flush")
        .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(10), connection.next())
            .await
            .expect("a short response wakes the reader")
            .unwrap()
            .unwrap();
        assert!(matches!(
            response.msg,
            crate::rtsp::msg::Message::Response(_)
        ));
        drop(connection);
        assert!(peer.join().unwrap().unwrap() == message);
    }
}
