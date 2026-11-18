// SPDX-License-Identifier: GPL-3.0-or-later
//! One authenticated WebSocket session (`docs/web_gui.md` §5.1-5.3):
//! hello, then commands in, replies and events out, until the socket or
//! the key ends.

use std::time::Duration;

use futures_util::stream::FuturesUnordered;
use futures_util::{SinkExt, StreamExt};
use molt_core::wire::{Chunk, ClientFrame, ServerFrame, WIRE_VERSION};
use molt_core::{Command, Event, MoltError, Reply, SessionScope};
use molt_engine::WalletHandle;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::gate::Gate;
use crate::server::WebDoor;

/// The key is missing, wrong, or no longer the seat key.
const CLOSE_UNAUTHORIZED: u16 = 4401;
/// The client broke the protocol.
const CLOSE_PROTOCOL: u16 = 4400;
/// A hello is a version and a token: anything larger is not one.
const MAX_HELLO: usize = 4096;
/// Commands one session may have at the engine at once; past it the
/// socket is not read, which is the backpressure.
const IN_FLIGHT: usize = 32;
/// Bytes of a reply per binary frame.
const CHUNK: usize = 256 * 1024;
/// The most bytes one read may ask the engine for. The engine has no
/// default file cap, so the door carries its own; larger files go
/// through the server-path flow.
const MAX_TRANSFER: u64 = 64 * 1024 * 1024;
/// A client that takes longer than this to accept one frame is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

type Ws<S> = WebSocketStream<S>;

/// Why a session ended; the close code the client sees.
struct End(Option<u16>);

pub(crate) async fn run<S>(mut ws: Ws<S>, handle: &WalletHandle, gate: &Gate, door: &WebDoor)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // subscribed before the key is checked: a rotation that lands after
    // the check is then an event this session sees, and dies on
    let events = handle.subscribe();
    let End(code) = match authenticate(&mut ws, handle, door).await {
        Ok(token) => serve(&mut ws, handle, gate, &token, events).await,
        Err(end) => end,
    };
    if let Some(code) = code {
        let frame = CloseFrame { code: CloseCode::from(code), reason: "".into() };
        let _ = tokio::time::timeout(WRITE_TIMEOUT, ws.close(Some(frame))).await;
    }
}

/// The first frame must be a hello carrying the live seat key. Returns the
/// key the session presented, to be re-checked for as long as it lives.
async fn authenticate<S>(ws: &mut Ws<S>, handle: &WalletHandle, door: &WebDoor) -> Result<String, End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let denied = End(Some(CLOSE_UNAUTHORIZED));
    let first = match tokio::time::timeout(door.pre_auth, ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) if t.len() <= MAX_HELLO => t,
        Ok(None | Some(Err(_))) => return Err(End(None)),
        _ => return Err(denied),
    };
    let Ok(ClientFrame::Hello { v, token }) = serde_json::from_str(first.as_str()) else {
        return Err(denied);
    };
    if v != WIRE_VERSION {
        return Err(End(Some(CLOSE_PROTOCOL)));
    }
    if !molt_mcp::seat_token_matches(handle, &token).await {
        tracing::warn!("web door: hello refused");
        return Err(denied);
    }
    let welcome = ServerFrame::Welcome { v: WIRE_VERSION, build: door.build.clone(), caps: door.caps.clone() };
    send(ws, &welcome).await?;
    Ok(token)
}

async fn serve<S>(
    ws: &mut Ws<S>,
    handle: &WalletHandle,
    gate: &Gate,
    token: &str,
    mut events: tokio::sync::broadcast::Receiver<Event>,
) -> End
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut in_flight = FuturesUnordered::new();
    loop {
        tokio::select! {
            incoming = ws.next(), if in_flight.len() < IN_FLIGHT => {
                let text = match incoming {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return End(None),
                    // uploads are not built yet (W4): bytes from a client are a protocol error
                    Some(Ok(_)) => return End(Some(CLOSE_PROTOCOL)),
                };
                let Ok(ClientFrame::Cmd { id, mut cmd }) = serde_json::from_str(text.as_str()) else {
                    return End(Some(CLOSE_PROTOCOL));
                };
                // the key is re-checked on every frame: a rotated key ends the session
                if !molt_mcp::seat_token_matches(handle, token).await {
                    return End(Some(CLOSE_UNAUTHORIZED));
                }
                if let Some(why) = gate.refusal(&cmd) {
                    let name: &'static str = (&*cmd).into();
                    tracing::warn!(cmd = name, why, "web door: refused");
                    let refused = ServerFrame::Reply { id, result: Err(MoltError::Refused(why.into())) };
                    if let Err(end) = send(ws, &refused).await {
                        return end;
                    }
                    continue;
                }
                if let Command::ReadUploadBytes { cap, .. } = &mut *cmd {
                    *cap = (*cap).min(MAX_TRANSFER);
                }
                let handle = handle.clone();
                in_flight.push(async move { (id, handle.execute(*cmd).await) });
            }
            Some((id, result)) = in_flight.next() => {
                // an answer (a phrase, say) never goes to a key that died meanwhile
                if !molt_mcp::seat_token_matches(handle, token).await {
                    return End(Some(CLOSE_UNAUTHORIZED));
                }
                if let Err(end) = send_reply(ws, id, result).await {
                    return end;
                }
            }
            event = events.recv() => {
                let frame = match event {
                    Ok(e) => {
                        // a push channel must not outlive its key: anything that
                        // can have changed it is checked BEFORE it is forwarded
                        let may_have_rotated = matches!(e, Event::SessionChanged { scope: SessionScope::Full });
                        if may_have_rotated && !molt_mcp::seat_token_matches(handle, token).await {
                            return End(Some(CLOSE_UNAUTHORIZED));
                        }
                        ServerFrame::Event(Box::new(e))
                    }
                    Err(RecvError::Lagged(_)) => {
                        if !molt_mcp::seat_token_matches(handle, token).await {
                            return End(Some(CLOSE_UNAUTHORIZED));
                        }
                        ServerFrame::Lag
                    }
                    Err(RecvError::Closed) => return End(None),
                };
                if let Err(end) = send(ws, &frame).await {
                    return end;
                }
            }
        }
    }
}

/// A reply; one that carries bulk bytes sends them after it as chunks, so
/// no frame holds a whole file as a JSON number array.
async fn send_reply<S>(ws: &mut Ws<S>, id: u64, result: Result<Reply, MoltError>) -> Result<(), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (result, bytes) = match result {
        Ok(Reply::UploadBytes { bytes }) => (Ok(Reply::UploadBytes { bytes: Vec::new() }), Some(bytes)),
        other => (other, None),
    };
    send(ws, &ServerFrame::Reply { id, result: result.map(Box::new) }).await?;
    if let Some(bytes) = bytes {
        // encoded one at a time: never a second copy of the whole file
        for frame in chunks(id, &bytes) {
            write(ws, Message::binary(frame)).await?;
        }
    }
    Ok(())
}

/// `bytes` as chunk frames; always at least one, the last one flagged.
fn chunks(id: u64, bytes: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    // an empty reply is still one (empty, last) chunk
    let count = bytes.len().div_ceil(CHUNK).max(1);
    let mut offset = 0u64;
    (0..count).map(move |i| {
        let data = bytes.chunks(CHUNK).nth(i).unwrap_or_default();
        let frame = Chunk { id, offset, last: i + 1 == count, data }.encode();
        offset += u64::try_from(data.len()).unwrap_or(u64::MAX);
        frame
    })
}

async fn send<S>(ws: &mut Ws<S>, frame: &ServerFrame) -> Result<(), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(text) = serde_json::to_string(frame) else {
        return Err(End(Some(CLOSE_PROTOCOL)));
    };
    write(ws, Message::text(text)).await
}

async fn write<S>(ws: &mut Ws<S>, message: Message) -> Result<(), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::time::timeout(WRITE_TIMEOUT, ws.send(message)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(End(None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_split_into_bounded_chunks_that_reassemble() {
        let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(2 * CHUNK + 5).collect();
        let frames: Vec<_> = chunks(9, &bytes).collect();
        assert_eq!(frames.len(), 3);
        let mut back = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            let c = Chunk::decode(f).expect("a chunk");
            assert_eq!(c.id, 9);
            assert_eq!(usize::try_from(c.offset).ok(), Some(back.len()));
            assert_eq!(c.last, i == 2);
            assert!(c.data.len() <= CHUNK);
            back.extend_from_slice(c.data);
        }
        assert_eq!(back, bytes);
    }

    #[test]
    fn an_exact_multiple_and_an_empty_reply_end_with_a_last_chunk() {
        let frames: Vec<_> = chunks(1, &vec![7u8; CHUNK]).collect();
        assert_eq!(frames.len(), 1);
        assert!(Chunk::decode(&frames[0]).expect("a chunk").last);
        let frames: Vec<_> = chunks(1, &[]).collect();
        assert_eq!(frames.len(), 1);
        let c = Chunk::decode(&frames[0]).expect("a chunk");
        assert!(c.last && c.data.is_empty());
    }
}
