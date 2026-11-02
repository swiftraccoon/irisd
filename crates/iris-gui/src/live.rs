//! The live feed of one tile: a WebSocket client that pushes the daemon's
//! fragmented MP4 byte stream into the tile's `appsrc`, reconnecting until
//! it is cancelled.

use std::time::Duration;

use futures::StreamExt as _;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

use crate::tile::{Source, TileEvent};

/// Message type byte of an init segment (the API's `MSG_INIT`).
const MSG_INIT: u8 = 0x01;
/// Message type byte of a media fragment (the API's `MSG_FRAGMENT`).
const MSG_FRAGMENT: u8 = 0x02;
/// Pause between reconnection attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Where a feed connects.
#[derive(Clone, Debug)]
pub struct Feed {
    /// Base URL of the API.
    pub api: Url,
    /// Bearer token, sent as the `token` query parameter because WebSocket
    /// clients cannot always set headers.
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
        if let Some(token) = &self.token {
            url.query_pairs_mut().append_pair("token", token);
        }
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
            outcome = session(&url, &source) => outcome,
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

async fn session(url: &Url, source: &Source) -> anyhow::Result<()> {
    let (stream, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let (_, mut read) = stream.split();
    source.reset();
    while let Some(message) = read.next().await {
        match message? {
            Message::Binary(bytes) => match bytes.first() {
                Some(&(MSG_INIT | MSG_FRAGMENT)) => source.push(&bytes[1..])?,
                Some(other) => warn!(kind = other, "unknown live message type"),
                None => {}
            },
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_url_switches_scheme_and_carries_the_token() {
        let feed = Feed {
            api: Url::parse("http://192.0.2.10:8554").unwrap(),
            token: Some("a b".into()),
            camera: "cam1".into(),
            kind: "sub".into(),
        };
        assert_eq!(
            feed.url().unwrap().as_str(),
            "ws://192.0.2.10:8554/api/streams/cam1/sub/live?token=a+b"
        );
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
}
