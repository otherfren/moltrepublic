// SPDX-License-Identifier: GPL-3.0-or-later

//! N2 (`docs/transport/nostr_n2_plan.md` §2): ONE relay connection — dial
//! through the T4 fail-closed dialer, WebSocket upgrade, typed NIP-01
//! message I/O (`nostr::ClientMessage` / `nostr::RelayMessage` — never
//! hand-rolled JSON framing). A dumb pipe with a typed edge: no policy, no
//! retry, no cursor here; that is `relay_runtime`'s job.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr::{ClientMessage, JsonUtil, RelayMessage};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::dial::{DialStream, Dialer};
use crate::NetError;

/// TLS handshake bound (the dial itself has its own deadline).
const TLS_TIMEOUT: Duration = Duration::from_secs(10);

/// A dialed stream, plain (`ws://` — loopback/LAN per §10.14, or onion where
/// the Tor circuit already encrypts) or wrapped in the crate's ONE TLS
/// posture (`wss://` — rustls-rustcrypto over the same dialed stream, so
/// Tor routing and TLS compose instead of competing).
pub(crate) enum MaybeTls {
    Plain(DialStream),
    Tls(Box<tokio_rustls::client::TlsStream<DialStream>>),
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MaybeTls::Tls(s) => Pin::new(&mut **s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MaybeTls::Tls(s) => Pin::new(&mut **s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_flush(cx),
            MaybeTls::Tls(s) => Pin::new(&mut **s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MaybeTls::Tls(s) => Pin::new(&mut **s).poll_shutdown(cx),
        }
    }
}

/// Dial `host:port` per the fail-closed dialer, then wrap in TLS iff `tls`.
/// The ONE stream-building path shared by the WS connection and the NIP-11
/// probe — SNI comes from the parsed host, the trust root is the crate's
/// public-WebPKI config.
pub(crate) async fn dial_maybe_tls(
    dialer: &Dialer,
    host: &str,
    port: u16,
    tls: bool,
) -> Result<MaybeTls, NetError> {
    let stream = dialer.dial_host(host, port).await?;
    if !tls {
        return Ok(MaybeTls::Plain(stream));
    }
    let connector = tokio_rustls::TlsConnector::from(crate::dial::public_tls_config()?);
    let sni = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| NetError::Framing(format!("bad TLS server name `{host}`")))?;
    let tls_stream = tokio::time::timeout(TLS_TIMEOUT, connector.connect(sni, stream))
        .await
        .map_err(|_| NetError::Unreachable(format!("tls {host}: handshake timed out")))?
        .map_err(|e| NetError::Unreachable(format!("tls {host}: {e}")))?;
    Ok(MaybeTls::Tls(Box::new(tls_stream)))
}

/// One live relay connection with typed NIP-01 I/O.
pub struct RelayWs {
    ws: WebSocketStream<MaybeTls>,
}

impl RelayWs {
    /// Dial `url` (`ws://…` or `wss://…`) through the fail-closed dialer,
    /// wrap `wss` in the crate's rustls-rustcrypto TLS over the dialed
    /// stream, and perform the WebSocket upgrade. The URL is parsed with the
    /// SAME WHATWG parser the pool policy validated it with, so the dialed
    /// host can never differ from the classified one.
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
        let stream = dial_maybe_tls(dialer, &host, port, scheme == "wss").await?;
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
