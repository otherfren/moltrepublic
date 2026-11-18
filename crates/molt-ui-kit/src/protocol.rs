// SPDX-License-Identifier: GPL-3.0-or-later
//! The remote engine client's protocol, without a socket
//! (`docs/web_gui.md` §5.1): frames out, frames in, who waits for which
//! reply, and a packed frame back into the text it stands for (§5.6). A
//! transport adapter (a browser WebSocket, tokio-tungstenite in
//! tests) feeds it what arrives and writes what it returns - so the logic
//! that ships to the browser is the logic the native tests exercise.

use std::collections::BTreeMap;
use std::io::Read;

use futures_channel::oneshot;
use molt_core::wire::{Caps, Chunk, ClientFrame, ServerFrame, CHUNK, MAX_UNPACKED, WIRE_VERSION};
use molt_core::{Command, Event, MoltError, Reply};

/// What a command answers with.
pub type Answer = Result<Reply, MoltError>;

/// The most bytes one reply may assemble: [`molt_core::wire::MAX_TRANSFER`]
/// (pinned equal by test), the ceiling the server and the engine keep. A
/// packed frame's bytes on the wire are held to it too.
pub const MAX_TRANSFER: usize = 64 * 1024 * 1024;

/// What a command answers when its connection ended first.
pub(crate) const GONE: &str = "connection closed";

/// What one request puts on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    /// The command frame.
    pub text: String,
    /// The chunk frame that follows it: an upload's bytes travel beside
    /// their command, never inside its JSON.
    pub binary: Option<Vec<u8>>,
}

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
    /// The packed frame arriving: the server's count of it, its bytes so
    /// far. One at a time - the server sends a packed frame's chunks back
    /// to back.
    packed: Option<(u64, Vec<u8>)>,
    welcomed: bool,
}

impl Protocol {
    /// A fresh connection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The first frame to send. It asks for packed frames: this client
    /// reads them ([`Protocol::on_binary`]), and a server that predates
    /// them ignores the word.
    #[must_use]
    pub fn hello(token: &str) -> String {
        encode(&ClientFrame::Hello { v: WIRE_VERSION, token: token.to_string(), packed: true })
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

    /// Register a command: what to send, and where its answer lands.
    /// `None` when the command was answered here and nothing is sent.
    pub fn request(&mut self, cmd: Command) -> (Option<Outbound>, oneshot::Receiver<Answer>) {
        let (tx, rx) = oneshot::channel();
        (self.request_into(cmd, tx), rx)
    }

    /// The same, answering into a waiter the caller already holds.
    pub fn request_into(&mut self, mut cmd: Command, waiter: oneshot::Sender<Answer>) -> Option<Outbound> {
        // an upload's bytes leave the command and follow it as one chunk
        let chunk = match &mut cmd {
            Command::UploadChunk { offset, bytes, last, .. } => {
                if bytes.len() > CHUNK {
                    // the server would end the whole connection over it
                    let refused = MoltError::BadPayload(format!("upload: a chunk is {CHUNK} bytes at most"));
                    drop(waiter.send(Err(refused)));
                    return None;
                }
                Some((*offset, *last, std::mem::take(bytes)))
            }
            _ => None,
        };
        self.next_id += 1;
        let id = self.next_id;
        self.waiting.insert(id, waiter);
        let text = encode(&ClientFrame::Cmd { id, cmd: Box::new(cmd) });
        let binary = chunk.map(|(offset, last, data)| Chunk { id, offset, last, data: &data }.encode());
        Some(Outbound { text, binary })
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

    /// A binary frame arrived: bytes of a reply, or a piece of a packed
    /// text frame - whose last piece is answered like the text frame it
    /// stands for ([`Protocol::on_text`]).
    pub fn on_binary(&mut self, frame: &[u8]) -> Result<Option<Notice>, Broken> {
        let (chunk, packed) = Chunk::decode_any(frame).ok_or(Broken("malformed chunk"))?;
        if packed {
            return self.on_packed(&chunk);
        }
        let slot = self.assembling.get_mut(&chunk.id).ok_or(Broken("bytes nobody asked for"))?;
        Self::append(&mut slot.bytes, &chunk)?;
        if chunk.last {
            if let Some(done) = self.assembling.remove(&chunk.id) {
                drop(done.waiter.send(Ok(Reply::UploadBytes { bytes: done.bytes })));
            }
        }
        Ok(None)
    }

    /// The one rule every chunk passes, bytes or packed: in order, and
    /// never past the ceiling.
    fn append(have: &mut Vec<u8>, chunk: &Chunk<'_>) -> Result<(), Broken> {
        let in_order = u64::try_from(have.len()).is_ok_and(|len| len == chunk.offset);
        if !in_order || have.len() + chunk.data.len() > MAX_TRANSFER {
            return Err(Broken("chunk out of order or over the ceiling"));
        }
        have.extend_from_slice(chunk.data);
        Ok(())
    }

    fn on_packed(&mut self, chunk: &Chunk<'_>) -> Result<Option<Notice>, Broken> {
        let (id, mut bytes) = self.packed.take().unwrap_or((chunk.id, Vec::new()));
        if id != chunk.id {
            return Err(Broken("a packed frame inside another"));
        }
        Self::append(&mut bytes, chunk)?;
        if !chunk.last {
            self.packed = Some((id, bytes));
            return Ok(None);
        }
        let text = unpack(&bytes)?;
        drop(bytes);
        self.on_text(&text)
    }

    /// The connection ended: nobody waits forever.
    pub fn on_close(&mut self) {
        let gone = || Err(MoltError::Engine(GONE.into()));
        for (_, waiter) in std::mem::take(&mut self.waiting) {
            drop(waiter.send(gone()));
        }
        for (_, slot) in std::mem::take(&mut self.assembling) {
            drop(slot.waiter.send(gone()));
        }
        self.packed = None;
    }
}

/// Inflate a packed frame to its text, stopping at
/// [`MAX_UNPACKED`]: however little arrived, it never asks for more
/// memory than that (deflate reaches about 1000:1).
fn unpack(packed: &[u8]) -> Result<String, Broken> {
    let mut text = Vec::new();
    let limit = u64::try_from(MAX_UNPACKED).unwrap_or(u64::MAX).saturating_add(1);
    flate2::read::DeflateDecoder::new(packed)
        .take(limit)
        .read_to_end(&mut text)
        .map_err(|_| Broken("unreadable packed frame"))?;
    if text.len() > MAX_UNPACKED {
        return Err(Broken("packed frame over the ceiling"));
    }
    String::from_utf8(text).map_err(|_| Broken("unreadable packed frame"))
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

    fn id_of(out: &Option<Outbound>) -> u64 {
        let frame = &out.as_ref().expect("a frame to send").text;
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
        assert_eq!(Protocol::hello("t"), r#"{"hello":{"v":1,"token":"t","packed":true}}"#);
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

    /// The upload direction mirrors the reply direction: the command
    /// frame carries no bytes, one chunk frame with the command's id does.
    #[test]
    fn an_upload_chunk_goes_out_as_a_command_and_one_chunk_frame() {
        let mut p = welcomed();
        let cmd = Command::UploadChunk { upload: "u".into(), offset: 5, bytes: b"abc".to_vec(), last: true };
        let (out, _r) = p.request(cmd);
        let id = id_of(&out);
        let out = out.expect("sent");
        match serde_json::from_str(&out.text).expect("a client frame") {
            ClientFrame::Cmd { cmd, .. } => match *cmd {
                Command::UploadChunk { upload, offset: 5, bytes, last: true } => {
                    assert_eq!(upload, "u");
                    assert!(bytes.is_empty(), "the bytes are not in the JSON");
                }
                other => panic!("unexpected: {other:?}"),
            },
            ClientFrame::Hello { .. } => panic!("not a command"),
        }
        let frame = out.binary.expect("a chunk frame");
        assert_eq!(Chunk::decode(&frame), Some(Chunk { id, offset: 5, last: true, data: b"abc" }));

        // an empty last chunk (a zero-byte file) still has its frame
        let (out, _r) = p.request(Command::UploadChunk { upload: "u".into(), offset: 0, bytes: Vec::new(), last: true });
        let frame = out.expect("sent").binary.expect("a chunk frame");
        assert!(Chunk::decode(&frame).is_some_and(|c| c.last && c.data.is_empty()));

        // every other command is its text frame alone
        let (out, _r) = p.request(Command::UploadBegin { name: "a".into(), size: 3 });
        assert_eq!(out.expect("sent").binary, None);
    }

    /// The server ends a connection over an oversized chunk, so the
    /// client never sends one: the caller is told, nothing leaves.
    #[test]
    fn an_oversized_chunk_is_answered_here_and_not_sent() {
        let mut p = welcomed();
        let fat = Command::UploadChunk { upload: "u".into(), offset: 0, bytes: vec![0; CHUNK + 1], last: false };
        let (out, mut r) = p.request(fat);
        assert_eq!(out, None);
        assert!(matches!(r.try_recv(), Ok(Some(Err(MoltError::BadPayload(_))))));
        assert_eq!(p.open_requests(), 0);
        let full = Command::UploadChunk { upload: "u".into(), offset: 0, bytes: vec![0; CHUNK], last: false };
        assert!(p.request(full).0.is_some(), "the bound itself is fine");
    }

    #[test]
    fn the_ceiling_is_the_wires() {
        assert_eq!(u64::try_from(MAX_TRANSFER).ok(), Some(molt_core::wire::MAX_TRANSFER));
    }

    #[test]
    fn a_broken_server_is_noticed() {
        let mut p = Protocol::new();
        assert_eq!(p.on_text(&reply(1, Ok(Reply::Ack))).err(), Some(Broken("a frame before welcome")));
        let mut p = welcomed();
        assert_eq!(p.on_text("{").err(), Some(Broken("unreadable frame")));
        assert_eq!(p.on_text(&reply(99, Ok(Reply::Ack))).err(), Some(Broken("a reply nobody asked for")));
        assert_eq!(p.on_binary(b"short").err(), Some(Broken("malformed chunk")));
        let mut flagged = Chunk { id: 5, offset: 0, last: true, data: b"x" }.encode();
        flagged[16] = 4;
        assert_eq!(p.on_binary(&flagged).err(), Some(Broken("malformed chunk")), "an unknown flag");
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

    /// Raw deflate, as the door packs a frame.
    fn deflated(text: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(3));
        e.write_all(text).expect("deflate");
        e.finish().expect("deflate")
    }

    /// `packed` as the chunk frames of packed frame `seq`, `piece` bytes each.
    fn packed_frames(seq: u64, packed: &[u8], piece: usize) -> Vec<Vec<u8>> {
        let count = packed.len().div_ceil(piece).max(1);
        (0..count)
            .map(|i| {
                let data = packed.chunks(piece).nth(i).unwrap_or_default();
                let offset = u64::try_from(i * piece).expect("small");
                Chunk { id: seq, offset, last: i + 1 == count, data }.encode_packed()
            })
            .collect()
    }

    /// A packed frame is the text frame it stands for: a reply reaches
    /// its waiter, an event and a lag come out as notices - and only
    /// once the last piece is in.
    #[test]
    fn a_packed_frame_is_read_as_its_text() {
        let mut p = welcomed();
        let (f, mut r) = p.request(Command::ReadSession);
        let big = MoltError::Engine("word ".repeat(40_000));
        let frames = packed_frames(1, &deflated(reply(id_of(&f), Err(big.clone())).as_bytes()), 100);
        assert!(frames.len() > 2, "several pieces");
        let (last, first) = frames.split_last().expect("frames");
        for frame in first {
            assert!(matches!(p.on_binary(frame), Ok(None)));
            assert!(matches!(r.try_recv(), Ok(None)), "not answered before the last piece");
        }
        assert!(matches!(p.on_binary(last), Ok(None)));
        assert!(matches!(r.try_recv(), Ok(Some(Err(MoltError::Engine(t)))) if t.len() == 200_000));
        assert_eq!(p.open_requests(), 0);

        let e = molt_core::Event::SessionChanged { scope: molt_core::SessionScope::Full };
        let event = packed_frames(2, &deflated(server(&ServerFrame::Event(Box::new(e))).as_bytes()), CHUNK);
        assert!(matches!(p.on_binary(&event[0]), Ok(Some(Notice::Event(_)))));
        let lag = packed_frames(3, &deflated(server(&ServerFrame::Lag).as_bytes()), CHUNK);
        assert!(matches!(p.on_binary(&lag[0]), Ok(Some(Notice::Lag))));
    }

    /// Packed frames and byte chunks are two kinds on one path: a byte
    /// read assembling under its request id is not disturbed by a packed
    /// frame whose count happens to be the same number.
    #[test]
    fn a_packed_frame_and_a_byte_read_do_not_mix() {
        let mut p = welcomed();
        let (f, mut bytes) = p.request(Command::ReadUploadBytes { checksum: "c".into(), cap: 10 });
        let id = id_of(&f);
        p.on_text(&reply(id, Ok(Reply::UploadBytes { bytes: Vec::new() }))).expect("ok");
        p.on_binary(&Chunk { id, offset: 0, last: false, data: b"on" }.encode()).expect("ok");
        let lag = packed_frames(id, &deflated(server(&ServerFrame::Lag).as_bytes()), CHUNK);
        assert!(matches!(p.on_binary(&lag[0]), Ok(Some(Notice::Lag))));
        p.on_binary(&Chunk { id, offset: 2, last: true, data: b"e" }.encode()).expect("ok");
        assert!(matches!(bytes.try_recv(), Ok(Some(Ok(Reply::UploadBytes { bytes }))) if bytes == b"one"));
    }

    /// **The bomb bound.** 65 MiB of zeros is some 64 KiB of deflate: the
    /// inflate stops at [`MAX_UNPACKED`] and the connection is broken,
    /// whoever sent it. Right at the bound a frame is still read (and
    /// refused for what it says, not for its size).
    #[test]
    fn a_packed_frame_never_inflates_past_the_ceiling() {
        let bomb = deflated(&vec![0u8; MAX_UNPACKED + 1024 * 1024]);
        assert!(bomb.len() < 128 * 1024, "{} bytes on the wire", bomb.len());
        let mut p = welcomed();
        let frames = packed_frames(1, &bomb, CHUNK);
        assert_eq!(p.on_binary(&frames[0]).err(), Some(Broken("packed frame over the ceiling")));

        assert_eq!(unpack(&deflated(&vec![b' '; MAX_UNPACKED + 1])).err(), Some(Broken("packed frame over the ceiling")));
        assert_eq!(unpack(&deflated(&vec![b' '; MAX_UNPACKED])).map(|t| t.len()), Ok(MAX_UNPACKED), "the bound itself");
        assert_eq!(MAX_UNPACKED, 64 * 1024 * 1024);
    }

    #[test]
    fn a_broken_packed_frame_is_noticed() {
        let whole = deflated(server(&ServerFrame::Lag).as_bytes());
        let lag = |seq, offset, last| Chunk { id: seq, offset, last, data: &whole }.encode_packed();
        let mut p = welcomed();
        assert_eq!(p.on_binary(&lag(1, 3, true)).err(), Some(Broken("chunk out of order or over the ceiling")));
        let mut p = welcomed();
        assert!(matches!(p.on_binary(&lag(1, 0, false)), Ok(None)));
        assert_eq!(p.on_binary(&lag(2, 0, true)).err(), Some(Broken("a packed frame inside another")));

        let garbage = |data: &[u8]| Chunk { id: 1, offset: 0, last: true, data }.encode_packed();
        let mut p = welcomed();
        assert_eq!(p.on_binary(&garbage(&[0xff; 40])).err(), Some(Broken("unreadable packed frame")));
        let mut p = welcomed();
        // a cut-off stream inflates to a cut-off text, and that is no frame
        assert_eq!(p.on_binary(&garbage(&whole[..whole.len() / 2])).err(), Some(Broken("unreadable frame")));
        let mut p = welcomed();
        assert_eq!(p.on_binary(&garbage(&deflated(&[0xff, 0xfe]))).err(), Some(Broken("unreadable packed frame")));
        let mut p = welcomed();
        assert_eq!(p.on_binary(&garbage(&deflated(b"{"))).err(), Some(Broken("unreadable frame")), "the text is judged");
        // nothing but a welcome before the welcome, packed or not
        let mut p = Protocol::new();
        assert_eq!(p.on_binary(&lag(1, 0, true)).err(), Some(Broken("a frame before welcome")));
    }

    /// A packed frame still arriving when the connection ends is dropped
    /// with it.
    #[test]
    fn a_closed_connection_forgets_a_half_packed_frame() {
        let mut p = welcomed();
        let whole = deflated(server(&ServerFrame::Lag).as_bytes());
        assert!(matches!(p.on_binary(&Chunk { id: 1, offset: 0, last: false, data: &whole }.encode_packed()), Ok(None)));
        p.on_close();
        assert!(p.packed.is_none());
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
