// SPDX-License-Identifier: GPL-3.0-or-later

//! N2 (`docs/transport/nostr_n2_plan.md` §2): ONE relay connection — dial
//! through the T4 fail-closed dialer, WebSocket upgrade, typed NIP-01
//! message I/O (`nostr::ClientMessage` / `nostr::RelayMessage` — never
//! hand-rolled JSON framing). A dumb pipe with a typed edge: no policy, no
//! retry, no cursor here; that is `relay_runtime`'s job.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr::{ClientMessage, JsonUtil, RelayMessage};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::dial::{DialStream, Dialer};
use crate::NetError;

/// One live relay connection with typed NIP-01 I/O.
pub struct RelayWs {
    ws: WebSocketStream<DialStream>,
}

impl RelayWs {
    /// Dial `url` (`ws://…` or `wss://…`) through the fail-closed dialer and
    /// perform the WebSocket upgrade. The URL is parsed with the SAME WHATWG
    /// parser the pool policy validated it with, so the dialed host can never
    /// differ from the classified one.
    ///
    /// `wss://` TLS (rustls-rustcrypto over the dialed stream) lands with
    /// plan step 8 — until then a `wss` URL fails honestly instead of
    /// dialing anything.
    pub async fn connect(dialer: &Dialer, url: &str) -> Result<Self, NetError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| NetError::Framing(format!("relay url {url}: {e}")))?;
        let scheme = parsed.scheme().to_string();
        if scheme != "ws" && scheme != "wss" {
            return Err(NetError::Framing(format!(
                "relay url {url}: scheme must be ws or wss"
            )));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| NetError::Framing(format!("relay url {url}: no host")))?
            .to_string();
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| NetError::Framing(format!("relay url {url}: no port")))?;
        if scheme == "wss" {
            return Err(NetError::Framing(
                "wss:// TLS over the dialer is not wired yet (N2 plan step 8)".into(),
            ));
        }
        let stream = dialer.dial_host(&host, port).await?;
        let (ws, _resp) = tokio_tungstenite::client_async(url, stream)
            .await
            .map_err(|e| NetError::Unreachable(format!("ws upgrade {url}: {e}")))?;
        Ok(Self { ws })
    }

    /// Send one typed client message.
    pub async fn send(&mut self, msg: ClientMessage<'_>) -> Result<(), NetError> {
        self.ws
            .send(Message::text(msg.as_json()))
            .await
            .map_err(|e| NetError::Unreachable(format!("ws send: {e}")))
    }

    /// Receive the next TYPED relay message, skipping transport frames
    /// (ping/pong — tungstenite queues the pong reply itself). `Err` on
    /// timeout, close, or a frame that is not valid NIP-01.
    pub async fn recv(&mut self, timeout: Duration) -> Result<RelayMessage<'static>, NetError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let frame = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .map_err(|_| NetError::Unreachable("ws recv: timed out".into()))?
                .ok_or_else(|| NetError::Unreachable("ws recv: connection closed".into()))?
                .map_err(|e| NetError::Unreachable(format!("ws recv: {e}")))?;
            match frame {
                Message::Text(text) => {
                    // RelayMessage's Deserialize is lifetime-unconstrained
                    // (owned Cows), so 'static types directly
                    return RelayMessage::from_json(text.as_str())
                        .map_err(|e| NetError::Framing(format!("relay frame: {e}")));
                }
                Message::Close(_) => {
                    return Err(NetError::Unreachable("ws recv: relay closed".into()))
                }
                // binary frames are not NIP-01; pings/pongs are transport
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }

    /// Close the connection politely (best effort).
    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}
