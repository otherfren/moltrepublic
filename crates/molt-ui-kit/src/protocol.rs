// SPDX-License-Identifier: GPL-3.0-or-later
//! The remote engine client's protocol, without a socket
//! (`docs/web_gui.md` §5.1): frames out, frames in, who waits for which
//! reply. A transport adapter (a browser WebSocket, tokio-tungstenite in
//! tests) feeds it what arrives and writes what it returns - so the logic
//! that ships to the browser is the logic the native tests exercise.

use std::collections::BTreeMap;

use futures_channel::oneshot;
use molt_core::wire::{Caps, Chunk, ClientFrame, ServerFrame, WIRE_VERSION};
use molt_core::{Command, Event, MoltError, Reply};

/// What a command answers with.
pub type Answer = Result<Reply, MoltError>;

/// The most bytes one reply may assemble; the server's ceiling
/// (`molt-remote`) is the same number.
pub const MAX_TRANSFER: usize = 64 * 1024 * 1024;

/// Something the server said that is not a reply.
#[derive(Debug)]
pub enum Notice {
    /// The hello was accepted.
    Welcome {
        /// The server's version.
        build: String,
        /// Process facts the engine does not know.
        caps: Caps,
    },
    /// An engine event: re-read what it invalidates.
    Event(Box<Event>),
    /// Events were dropped: re-read everything.
    Lag,
}

/// The server broke the protocol; the connection is not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken(pub &'static str);

/// A reply waiting for its bytes.
struct Assembling {
    waiter: oneshot::Sender<Answer>,
    bytes: Vec<u8>,
}

/// One connection's state.
#[derive(Default)]
pub struct Protocol {
    next_id: u64,
    waiting: BTreeMap<u64, oneshot::Sender<Answer>>,
    assembling: BTreeMap<u64, Assembling>,
    welcomed: bool,
}

impl Protocol {
    /// A fresh connection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The first frame to send.
    #[must_use]
    pub fn hello(token: &str) -> String {
        encode(&ClientFrame::Hello { v: WIRE_VERSION, token: token.to_string() })
    }

    /// Whether the server accepted the hello.
    #[must_use]
    pub fn welcomed(&self) -> bool {
        self.welcomed
    }

    /// Commands that have no answer yet.
    #[must_use]
    pub fn open_requests(&self) -> usize {
        self.waiting.len() + self.assembling.len()
    }

    /// Register a command: the frame to send, and where its answer lands.
    pub fn request(&mut self, cmd: Command) -> (String, oneshot::Receiver<Answer>) {
        let (tx, rx) = oneshot::channel();
        (self.request_into(cmd, tx), rx)
    }

    /// The same, answering into a waiter the caller already holds.
    pub fn request_into(&mut self, cmd: Command, waiter: oneshot::Sender<Answer>) -> String {
        self.next_id += 1;
        let id = self.next_id;
        self.waiting.insert(id, waiter);
        encode(&ClientFrame::Cmd { id, cmd: Box::new(cmd) })
    }

    /// A text frame arrived.
    pub fn on_text(&mut self, text: &str) -> Result<Option<Notice>, Broken> {
        let frame: ServerFrame = serde_json::from_str(text).map_err(|_| Broken("unreadable frame"))?;
        match frame {
            ServerFrame::Welcome { v, build, caps } => {
                if v != WIRE_VERSION || self.welcomed {
                    return Err(Broken("unexpected welcome"));
                }
                self.welcomed = true;
                Ok(Some(Notice::Welcome { build, caps }))
            }
            _ if !self.welcomed => Err(Broken("a frame before welcome")),
            ServerFrame::Reply { id, result } => {
                let waiter = self.waiting.remove(&id).ok_or(Broken("a reply nobody asked for"))?;
                match result.map(|r| *r) {
                    // the bytes follow as chunks
                    Ok(Reply::UploadBytes { .. }) => {
                        self.assembling.insert(id, Assembling { waiter, bytes: Vec::new() });
                    }
                    // a caller that stopped waiting is not an error
                    answer => drop(waiter.send(answer)),
                }
                Ok(None)
            }
            ServerFrame::Event(e) => Ok(Some(Notice::Event(e))),
            ServerFrame::Lag => Ok(Some(Notice::Lag)),
        }
    }

    /// A binary frame arrived: bytes of a reply.
    pub fn on_binary(&mut self, frame: &[u8]) -> Result<(), Broken> {
        let chunk = Chunk::decode(frame).ok_or(Broken("malformed chunk"))?;
        let slot = self.assembling.get_mut(&chunk.id).ok_or(Broken("bytes nobody asked for"))?;
        let in_order = u64::try_from(slot.bytes.len()).is_ok_and(|have| have == chunk.offset);
        if !in_order || slot.bytes.len() + chunk.data.len() > MAX_TRANSFER {
            return Err(Broken("chunk out of order or over the ceiling"));
        }
        slot.bytes.extend_from_slice(chunk.data);
        if chunk.last {
            if let Some(done) = self.assembling.remove(&chunk.id) {
                drop(done.waiter.send(Ok(Reply::UploadBytes { bytes: done.bytes })));
            }
        }
        Ok(())
    }

    /// The connection ended: nobody waits forever.
    pub fn on_close(&mut self) {
        let gone = || Err(MoltError::Engine("connection closed".into()));
        for (_, waiter) in std::mem::take(&mut self.waiting) {
            drop(waiter.send(gone()));
        }
        for (_, slot) in std::mem::take(&mut self.assembling) {
            drop(slot.waiter.send(gone()));
        }
    }
}

fn encode(frame: &ClientFrame) -> String {
    // the contract types serialize infallibly (string keys, no fallible Serialize impl)
    serde_json::to_string(frame).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(frame: &ServerFrame) -> String {
        serde_json::to_string(frame).expect("json")
    }

    fn welcomed() -> Protocol {
        let mut p = Protocol::new();
        let w = ServerFrame::Welcome { v: WIRE_VERSION, build: "1".into(), caps: Caps::default() };
        assert!(matches!(p.on_text(&server(&w)), Ok(Some(Notice::Welcome { .. }))));
        p
    }

    fn id_of(frame: &str) -> u64 {
        match serde_json::from_str(frame).expect("a client frame") {
            ClientFrame::Cmd { id, .. } => id,
            ClientFrame::Hello { .. } => panic!("not a command"),
        }
    }

    fn reply(id: u64, r: Answer) -> String {
        server(&ServerFrame::Reply { id, result: r.map(Box::new) })
    }

    #[test]
    fn hello_is_the_wire_hello() {
        assert_eq!(Protocol::hello("t"), r#"{"hello":{"v":1,"token":"t"}}"#);
    }

    #[test]
    fn replies_reach_their_own_waiter_in_any_order() {
        let mut p = welcomed();
        let (f1, mut r1) = p.request(Command::ReadSession);
        let (f2, mut r2) = p.request(Command::ReadMirror);
        assert_ne!(id_of(&f1), id_of(&f2));
        assert_eq!(p.on_text(&reply(id_of(&f2), Err(MoltError::Engine("two".into())))).map(|n| n.is_none()), Ok(true));
        assert!(matches!(r1.try_recv(), Ok(None)), "the first still waits");
        assert!(matches!(r2.try_recv(), Ok(Some(Err(MoltError::Engine(t)))) if t == "two"));
        p.on_text(&reply(id_of(&f1), Ok(Reply::Ack))).expect("ok");
        assert!(matches!(r1.try_recv(), Ok(Some(Ok(Reply::Ack)))));
        assert_eq!(p.open_requests(), 0);
    }

    #[test]
    fn events_and_lag_come_out_as_notices() {
        let mut p = welcomed();
        let e = molt_core::Event::SessionChanged { scope: molt_core::SessionScope::Full };
        assert!(matches!(p.on_text(&server(&ServerFrame::Event(Box::new(e)))), Ok(Some(Notice::Event(_)))));
        assert!(matches!(p.on_text(&server(&ServerFrame::Lag)), Ok(Some(Notice::Lag))));
    }

    #[test]
    fn bytes_arrive_as_chunks_after_their_reply() {
        let mut p = welcomed();
        let (f, mut r) = p.request(Command::ReadUploadBytes { checksum: "c".into(), cap: 10 });
        let id = id_of(&f);
        p.on_text(&reply(id, Ok(Reply::UploadBytes { bytes: Vec::new() }))).expect("ok");
        assert!(matches!(r.try_recv(), Ok(None)), "not answered before the bytes are in");
        p.on_binary(&Chunk { id, offset: 0, last: false, data: b"on" }.encode()).expect("ok");
        p.on_binary(&Chunk { id, offset: 2, last: true, data: b"e" }.encode()).expect("ok");
        assert!(matches!(r.try_recv(), Ok(Some(Ok(Reply::UploadBytes { bytes }))) if bytes == b"one"));
        assert_eq!(p.open_requests(), 0);
    }

    #[test]
    fn a_broken_server_is_noticed() {
        let mut p = Protocol::new();
        assert_eq!(p.on_text(&reply(1, Ok(Reply::Ack))).err(), Some(Broken("a frame before welcome")));
        let mut p = welcomed();
        assert_eq!(p.on_text("{").err(), Some(Broken("unreadable frame")));
        assert_eq!(p.on_text(&reply(99, Ok(Reply::Ack))).err(), Some(Broken("a reply nobody asked for")));
        assert_eq!(p.on_binary(b"short").err(), Some(Broken("malformed chunk")));
        let stray = Chunk { id: 5, offset: 0, last: true, data: b"x" }.encode();
        assert_eq!(p.on_binary(&stray).err(), Some(Broken("bytes nobody asked for")));
        let again = ServerFrame::Welcome { v: WIRE_VERSION, build: "1".into(), caps: Caps::default() };
        assert_eq!(p.on_text(&server(&again)).err(), Some(Broken("unexpected welcome")));

        let (f, _r) = p.request(Command::ReadUploadBytes { checksum: "c".into(), cap: 10 });
        let id = id_of(&f);
        p.on_text(&reply(id, Ok(Reply::UploadBytes { bytes: Vec::new() }))).expect("ok");
        let gap = Chunk { id, offset: 3, last: true, data: b"x" }.encode();
        assert!(p.on_binary(&gap).is_err(), "a gap is not papered over");
    }

    #[test]
    fn a_wrong_version_welcome_is_refused() {
        let mut p = Protocol::new();
        let w = ServerFrame::Welcome { v: WIRE_VERSION + 1, build: "1".into(), caps: Caps::default() };
        assert!(p.on_text(&server(&w)).is_err());
        assert!(!p.welcomed());
    }

    #[test]
    fn a_closed_connection_answers_everyone() {
        let mut p = welcomed();
        let (_, mut plain) = p.request(Command::ReadSession);
        let (f, mut bytes) = p.request(Command::ReadUploadBytes { checksum: "c".into(), cap: 10 });
        p.on_text(&reply(id_of(&f), Ok(Reply::UploadBytes { bytes: Vec::new() }))).expect("ok");
        p.on_close();
        assert!(matches!(plain.try_recv(), Ok(Some(Err(MoltError::Engine(_))))));
        assert!(matches!(bytes.try_recv(), Ok(Some(Err(MoltError::Engine(_))))));
        assert_eq!(p.open_requests(), 0);
    }

    #[test]
    fn a_caller_that_stopped_waiting_breaks_nothing() {
        let mut p = welcomed();
        let (f, r) = p.request(Command::ReadSession);
        drop(r);
        assert!(p.on_text(&reply(id_of(&f), Ok(Reply::Ack))).is_ok());
    }
}
