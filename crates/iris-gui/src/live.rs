//! The live feed of one tile: a WebSocket client that pushes the daemon's
//! fragmented MP4 byte stream into the tile's `appsrc`, reconnecting until
//! it is cancelled.

use std::time::Duration;

use futures::StreamExt as _;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::http::header;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

use crate::tile::{SOURCE_QUEUE_BYTES, Source, TileEvent};

/// Message type byte of an init segment (the API's `MSG_INIT`).
const MSG_INIT: u8 = 0x01;
/// Message type byte of a media fragment (the API's `MSG_FRAGMENT`).
const MSG_FRAGMENT: u8 = 0x02;
/// Pause between reconnection attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Largest wire message: one discriminator plus one full source payload.
const LIVE_MESSAGE_BYTES: usize = SOURCE_QUEUE_BYTES as usize + 1;

/// Where a feed connects.
#[derive(Clone, Debug)]
pub struct Feed {
    /// Base URL of the API.
    pub api: Url,
    /// Bearer token, sent in the WebSocket handshake's Authorization header.
    pub token: Option<String>,
    /// Camera id.
    pub camera: String,
    /// Stream kind.
    pub kind: String,
}

impl Feed {
    /// The WebSocket URL of the live endpoint.
    pub fn url(&self) -> anyhow::Result<Url> {
        let mut url = self
            .api
            .join(&format!("/api/streams/{}/{}/live", self.camera, self.kind))?;
        let scheme = match url.scheme() {
            "https" => "wss",
            _ => "ws",
        };
        url.set_scheme(scheme)
            .map_err(|()| anyhow::anyhow!("cannot set the WebSocket scheme"))?;
        Ok(url)
    }
}

/// Runs the feed until `cancel` fires: connects, pushes every message's
/// payload into `source`, and on any failure tells the tile to restart its
/// pipeline and reconnects after a pause.
pub async fn run(
    feed: Feed,
    source: Source,
    tile_events: async_channel::Sender<TileEvent>,
    cancel: CancellationToken,
) {
    let url = match feed.url() {
        Ok(url) => url,
        Err(error) => {
            warn!(camera = %feed.camera, %error, "live feed URL");
            return;
        }
    };
    loop {
        let outcome = tokio::select! {
            () = cancel.cancelled() => return,
            outcome = session(&url, feed.token.as_deref(), &source) => outcome,
        };
        match outcome {
            Ok(()) => debug!(camera = %feed.camera, "live feed ended"),
            Err(error) => warn!(camera = %feed.camera, %error, "live feed failed"),
        }
        let _ = tile_events.send(TileEvent::Restart).await;
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
    }
}

fn connect_request(url: &Url, token: Option<&str>) -> anyhow::Result<Request<()>> {
    let mut request = url.as_str().into_client_request()?;
    if let Some(token) = token {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))?;
        request.headers_mut().insert(header::AUTHORIZATION, value);
    }
    Ok(request)
}

async fn session(url: &Url, token: Option<&str>, source: &Source) -> anyhow::Result<()> {
    let request = connect_request(url, token)?;
    let (stream, _) =
        tokio_tungstenite::connect_async_with_config(request, Some(websocket_config()), false)
            .await?;
    let (_, mut read) = stream.split();
    source.reset();
    while let Some(message) = read.next().await {
        match message? {
            Message::Binary(bytes) => source.push(live_payload(&bytes)?)?,
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(LIVE_MESSAGE_BYTES))
        .max_frame_size(Some(LIVE_MESSAGE_BYTES))
}

fn live_payload(message: &[u8]) -> anyhow::Result<&[u8]> {
    let Some((&kind, payload)) = message.split_first() else {
        anyhow::bail!("empty live message");
    };
    match kind {
        MSG_INIT | MSG_FRAGMENT => Ok(payload),
        other => anyhow::bail!("unknown live message type {other:#04x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_url_switches_scheme_and_keeps_the_bearer_in_a_header() {
        let feed = Feed {
            api: Url::parse("http://192.0.2.10:8554").unwrap(),
            token: Some("a b".into()),
            camera: "cam1".into(),
            kind: "sub".into(),
        };
        assert_eq!(
            feed.url().unwrap().as_str(),
            "ws://192.0.2.10:8554/api/streams/cam1/sub/live"
        );
        let request = connect_request(&feed.url().unwrap(), feed.token.as_deref()).unwrap();
        assert_eq!(request.headers()[header::AUTHORIZATION], "Bearer a b");
        let secure = Feed {
            api: Url::parse("https://example.invalid/").unwrap(),
            token: None,
            ..feed
        };
        assert_eq!(
            secure.url().unwrap().as_str(),
            "wss://example.invalid/api/streams/cam1/sub/live"
        );
    }

    #[test]
    fn websocket_admission_matches_the_source_payload_limit() {
        let config = websocket_config();
        assert_eq!(config.max_message_size, Some(LIVE_MESSAGE_BYTES));
        assert_eq!(config.max_frame_size, Some(LIVE_MESSAGE_BYTES));
        assert_eq!(LIVE_MESSAGE_BYTES, SOURCE_QUEUE_BYTES as usize + 1);
    }

    #[test]
    fn malformed_fixed_protocol_messages_end_the_session() {
        assert!(live_payload(&[]).is_err());
        assert!(live_payload(&[0xff]).is_err());
        assert_eq!(live_payload(&[MSG_INIT, 7]).unwrap(), &[7]);
        assert_eq!(live_payload(&[MSG_FRAGMENT, 8]).unwrap(), &[8]);
    }
}
