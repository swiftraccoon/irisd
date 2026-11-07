//! Inbound limits shared by the live and event WebSocket endpoints.
//!
//! Both endpoints only send. Clients may answer pings and close, but a data
//! message, one larger than [`INBOUND_LIMIT_BYTES`], or any other protocol
//! failure ends the session with close code 1008 (policy violation).

use axum::extract::ws::{CloseFrame, Message, WebSocketUpgrade, close_code};

/// Inbound message, frame, and read-buffer limit. Control frames carry at most
/// 125 bytes (RFC 6455 §5.5), and clients send no data messages.
pub(crate) const INBOUND_LIMIT_BYTES: usize = 4 * 1024;
/// Write-buffer target before a flush, tungstenite's default.
const WRITE_BUFFER_BYTES: usize = 128 * 1024;
/// Largest WebSocket frame header (RFC 6455 §5.2).
const FRAME_HEADER_BYTES: usize = 14;

/// What one inbound item means for a server that only sends.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Inbound {
    /// A ping or pong; tungstenite answers pings itself.
    Control,
    /// The peer closed, or the connection ended.
    Closed,
    /// A data message, a message over the inbound limit, or a protocol error.
    Violation,
}

/// Applies the inbound limits and bounds the write buffer to one outbound
/// message of at most `max_outbound_message` bytes plus the buffer target.
pub(crate) fn limit(upgrade: WebSocketUpgrade, max_outbound_message: usize) -> WebSocketUpgrade {
    upgrade
        .read_buffer_size(INBOUND_LIMIT_BYTES)
        .max_message_size(INBOUND_LIMIT_BYTES)
        .max_frame_size(INBOUND_LIMIT_BYTES)
        .write_buffer_size(WRITE_BUFFER_BYTES)
        .max_write_buffer_size(WRITE_BUFFER_BYTES + max_outbound_message + FRAME_HEADER_BYTES)
}

/// Classifies one result of `WebSocket::recv`.
pub(crate) fn classify(incoming: Option<&Result<Message, axum::Error>>) -> Inbound {
    match incoming {
        None | Some(Ok(Message::Close(_))) => Inbound::Closed,
        Some(Ok(Message::Ping(_) | Message::Pong(_))) => Inbound::Control,
        Some(Ok(Message::Text(_) | Message::Binary(_)) | Err(_)) => Inbound::Violation,
    }
}

/// The close frame sent after an inbound violation (code 1008).
pub(crate) fn policy_close() -> Message {
    Message::Close(Some(CloseFrame {
        code: close_code::POLICY,
        reason: "this endpoint accepts no client data messages".into(),
    }))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[test]
    fn only_control_frames_and_closes_are_accepted_from_clients() {
        assert_eq!(
            classify(Some(&Ok(Message::Ping(Bytes::from_static(b"p"))))),
            Inbound::Control
        );
        assert_eq!(
            classify(Some(&Ok(Message::Pong(Bytes::new())))),
            Inbound::Control
        );
        assert_eq!(classify(Some(&Ok(Message::Close(None)))), Inbound::Closed);
        assert_eq!(classify(None), Inbound::Closed);
        assert_eq!(
            classify(Some(&Ok(Message::Text("hello".into())))),
            Inbound::Violation
        );
        assert_eq!(
            classify(Some(&Ok(Message::Binary(Bytes::from_static(b"\x01"))))),
            Inbound::Violation
        );
        assert_eq!(
            classify(Some(&Err(axum::Error::new(std::io::Error::other("reset"))))),
            Inbound::Violation
        );
        let Message::Close(Some(frame)) = policy_close() else {
            panic!("a policy close carries a frame");
        };
        assert_eq!(frame.code, close_code::POLICY);
        assert!(frame.reason.len() <= 123, "WebSocket close reason bound");
    }
}
