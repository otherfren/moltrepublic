// SPDX-License-Identifier: GPL-3.0-or-later
//! The browser's WebSocket as the two ends `molt-ui-kit`'s pump runs on
//! (`docs/web_gui.md` §6.1). The socket itself is `gloo-net`'s; this file
//! only maps its messages onto the pump's [`Wire`] and keeps the close
//! code, which is how the door says why it hung up.

use std::cell::Cell;
use std::pin::Pin;
use std::rc::Rc;

use futures_util::future::ready;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket::{Message, WebSocketError};
use molt_ui_kit::client::Wire;

/// The code the socket closed with, once it has.
pub(crate) type CloseCode = Rc<Cell<Option<u16>>>;

/// One open (or opening) socket: what arrives and what leaves. Dropping
/// both ends closes it.
pub(crate) struct Socket {
    pub(crate) incoming: Pin<Box<dyn Stream<Item = Wire>>>,
    pub(crate) outgoing: Pin<Box<dyn Sink<Wire, Error = WebSocketError>>>,
}

/// Dial `url`. The answer is immediate; a send waits for the socket to
/// open, and a door that cannot be reached ends the incoming stream.
/// `closed` is cleared and then holds this socket's close code.
pub(crate) fn open(url: &str, closed: &CloseCode) -> Option<Socket> {
    closed.set(None);
    let (sink, stream) = WebSocket::open(url).ok()?.split();
    let seen = closed.clone();
    let incoming = futures_util::stream::unfold(stream, move |mut stream| {
        let seen = seen.clone();
        async move {
            loop {
                match stream.next().await? {
                    Ok(Message::Text(text)) => return Some((Wire::Text(text), stream)),
                    Ok(Message::Bytes(bytes)) => return Some((Wire::Binary(bytes), stream)),
                    Err(WebSocketError::ConnectionClose(event)) => {
                        seen.set(Some(event.code));
                        return None;
                    }
                    // an `error` event says nothing; the `close` after it does
                    Err(_) => {}
                }
            }
        }
    });
    let outgoing = sink.with(|wire: Wire| {
        ready(Ok::<_, WebSocketError>(match wire {
            Wire::Text(text) => Message::Text(text),
            Wire::Binary(bytes) => Message::Bytes(bytes),
        }))
    });
    Some(Socket { incoming: Box::pin(incoming), outgoing: Box::pin(outgoing) })
}
