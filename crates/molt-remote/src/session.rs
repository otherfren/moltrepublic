// SPDX-License-Identifier: GPL-3.0-or-later
//! One authenticated WebSocket session (`docs/web_gui.md` §5.1-5.3):
//! hello, then commands in, replies and events out, until the socket or
//! the key ends. A large frame goes out packed to a client that asked
//! (§5.6).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::FuturesUnordered;
use futures_util::{SinkExt, StreamExt};
use std::collections::BTreeSet;

use molt_core::wire::{
    delta, reply_frame, Caps, Chunk, ClientFrame, ServerFrame, CHUNK, CLOSE_PROTOCOL, CLOSE_UNAUTHORIZED, MAX_TRANSFER,
    MAX_UNPACKED, PACK_MIN, PATCH_MIN,
    WIRE_VERSION,
};
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

/// A hello is a version and a token: anything larger is not one.
const MAX_HELLO: usize = 4096;
/// Commands one session may have at the engine at once; past it the
/// socket is not read, which is the backpressure.
const IN_FLIGHT: usize = 32;
/// From this size on a frame is deflated on a blocking thread, not on
/// the session's own.
const PACK_INLINE: usize = 64 * 1024;

/// A command still at the engine when its session ends is given this
/// long to answer, so an upload it opened is not left without an owner.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a refused key keeps its session slot before the close. The
/// door has 64 slots, so this is the rate a key can be guessed at: 64 a
/// second, however many sockets ask. A minted key never needed it; a key
/// somebody typed into the config may.
const REFUSED_DELAY: Duration = Duration::from_secs(1);

/// The unix second the last refused key was logged in: one line a second
/// at most, whatever a local process sends.
static REFUSAL_LOGGED: AtomicU64 = AtomicU64::new(0);

/// Log a refused key, unless one was logged this second already.
fn log_refusal() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if REFUSAL_LOGGED.swap(now, Ordering::Relaxed) != now {
        tracing::warn!("web door: hello refused");
    }
}

type Ws<S> = WebSocketStream<S>;
/// A command's answer: its id, the earlier request its client still
/// holds the reply of, the upload a success spends, the result.
type Answer = (u64, Option<u64>, Option<String>, Result<Reply, MoltError>);

/// The reply texts one session may be asked to patch, at most.
const KEEP_BYTES: usize = 96 * 1024 * 1024;

/// The large replies a session was sent, by request: what a later
/// request naming one (`like`) is answered against (`docs/web_gui.md`
/// §5.7). Oldest out first; a reply that is gone is answered whole.
#[derive(Default)]
struct Kept {
    texts: std::collections::VecDeque<(u64, String)>,
    bytes: usize,
}

impl Kept {
    /// The reply of `request`, which is spent by being built on.
    fn take(&mut self, request: u64) -> Option<String> {
        let at = self.texts.iter().position(|(id, _)| *id == request)?;
        let (_, text) = self.texts.remove(at)?;
        self.bytes -= text.len();
        Some(text)
    }

    fn keep(&mut self, request: u64, text: String) {
        self.bytes += text.len();
        self.texts.push_back((request, text));
        while self.bytes > KEEP_BYTES {
            let Some((_, old)) = self.texts.pop_front() else { break };
            self.bytes -= old.len();
        }
    }
}
/// The commands one session has at the engine.
type InFlight = FuturesUnordered<BoxFuture<'static, Answer>>;

/// Why a session ended; the close code the client sees.
struct End(Option<u16>);

/// What a welcomed session writes with.
struct Out<'a> {
    handle: &'a WalletHandle,
    /// The key the session presented, re-checked for as long as it lives.
    token: &'a str,
    /// Its client reads packed frames.
    packs: bool,
    /// How many packed frames it was sent.
    packed: u64,
    /// The replies it may name.
    kept: Kept,
}

pub(crate) async fn run<S>(mut ws: Ws<S>, handle: &WalletHandle, gate: &Gate, door: &WebDoor)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // subscribed before the key is checked: a rotation that lands after
    // the check is then an event this session sees, and dies on
    let events = handle.subscribe();
    let End(code) = match authenticate(&mut ws, handle, door).await {
        Ok((token, packed)) => {
            let mut uploads = BTreeSet::new();
            let mut in_flight = InFlight::new();
            let mut out = Out { handle, token: &token, packs: packed, packed: 0, kept: Kept::default() };
            let end = serve(&mut ws, gate, events, &mut uploads, &mut in_flight, &mut out).await;
            // the engine runs what was sent whether or not anyone reads
            // the answer: an upload begun on the way out is this session's too
            while let Ok(Some((_, _, spent, result))) = tokio::time::timeout(SETTLE_TIMEOUT, in_flight.next()).await {
                note_upload(&mut uploads, &result, spent);
            }
            // a session that ends takes its unshared uploads along; one
            // the engine no longer has, or already shared, refuses this
            for upload in uploads {
                let _ = handle.execute(Command::UploadAbort { upload }).await;
            }
            end
        }
        Err(end) => end,
    };
    if let Some(code) = code {
        let frame = CloseFrame { code: CloseCode::from(code), reason: "".into() };
        // the one write with a total: nothing may hold a finished session
        let _ = tokio::time::timeout(door.stall, ws.close(Some(frame))).await;
    }
}

/// The first frame must be a hello carrying the live seat key. Returns the
/// key the session presented, to be re-checked for as long as it lives,
/// and whether its client reads packed frames.
async fn authenticate<S>(ws: &mut Ws<S>, handle: &WalletHandle, door: &WebDoor) -> Result<(String, bool), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let denied = End(Some(CLOSE_UNAUTHORIZED));
    let first = match tokio::time::timeout(door.pre_auth, ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) if t.len() <= MAX_HELLO => t,
        Ok(None | Some(Err(_))) => return Err(End(None)),
        _ => return Err(denied),
    };
    let Ok(ClientFrame::Hello { v, token, packed }) = serde_json::from_str(first.as_str()) else {
        return Err(denied);
    };
    if v != WIRE_VERSION {
        return Err(End(Some(CLOSE_PROTOCOL)));
    }
    if !molt_mcp::seat_token_matches(handle, &token).await {
        log_refusal();
        // the answer waits, and the slot with it: see `REFUSED_DELAY`
        tokio::time::sleep(REFUSED_DELAY).await;
        return Err(denied);
    }
    let welcome = ServerFrame::Welcome { v: WIRE_VERSION, build: door.build.clone(), caps: Caps { patch: true, ..door.caps.clone() } };
    // the welcome is small: a text frame for every client
    let Ok(welcome) = serde_json::to_string(&welcome) else {
        return Err(End(Some(CLOSE_PROTOCOL)));
    };
    write(ws, Message::text(welcome)).await?;
    Ok((token, packed))
}

/// An `upload_chunk` command whose bytes are the next frame.
struct Parked {
    id: u64,
    /// `None`: the command was refused; its bytes are still taken off
    /// the wire, so the session goes on.
    cmd: Option<Box<Command>>,
}

/// The upload a command acts on, if it names one.
fn upload_of(cmd: &Command) -> Option<&str> {
    match cmd {
        Command::UploadChunk { upload, .. }
        | Command::UploadAbort { upload }
        | Command::ShareUpload { upload, .. } => Some(upload),
        // a picture proposal may take an upload as its source
        Command::Propose { payload, .. } => payload.get("upload").and_then(|u| u.as_str()),
        _ => None,
    }
}

/// What the session's own rules refuse, past the gate: `uploads` are the
/// ones this session began. An upload is its session's - a second tab
/// cannot write into, share or drop another's.
fn session_refusal(cmd: &Command, uploads: &BTreeSet<String>) -> Option<&'static str> {
    if matches!(cmd, Command::UploadBegin { size, .. } if *size > MAX_TRANSFER) {
        return Some("too large");
    }
    upload_of(cmd).is_some_and(|u| !uploads.contains(u)).then_some("unknown upload")
}

/// Keep the session's set of uploads in step with one answer: a begun
/// (or written) upload is its own, a spent one no longer is.
fn note_upload(uploads: &mut BTreeSet<String>, result: &Result<Reply, MoltError>, spent: Option<String>) {
    match (result, spent) {
        (Ok(Reply::UploadProgress { upload, .. }), _) => {
            uploads.insert(upload.clone());
        }
        (Ok(_), Some(upload)) => {
            uploads.remove(&upload);
        }
        _ => {}
    }
}

/// The door's ceiling on every read that answers bulk bytes.
fn clamp_cap(cmd: &mut Command) {
    if let Command::ReadUploadBytes { cap, .. }
    | Command::ReadFileBytes { cap, .. }
    | Command::ReadImageBytes { cap, .. }
    | Command::ReadExportBytes { cap } = cmd
    {
        *cap = (*cap).min(MAX_TRANSFER);
    }
}

/// The chunk frame of a parked `upload_chunk`: it must be exactly the
/// command's other half. `Ok(None)` for a refused command's bytes.
fn fill_chunk(parked: Parked, frame: &[u8]) -> Result<Option<(u64, Box<Command>)>, End> {
    let broken = End(Some(CLOSE_PROTOCOL));
    let Some(chunk) = Chunk::decode(frame) else { return Err(broken) };
    if chunk.id != parked.id || chunk.data.len() > CHUNK {
        return Err(broken);
    }
    let Some(mut cmd) = parked.cmd else { return Ok(None) };
    match &mut *cmd {
        Command::UploadChunk { offset, bytes, last, .. } if *offset == chunk.offset && *last == chunk.last => {
            *bytes = chunk.data.to_vec();
        }
        _ => return Err(broken),
    }
    Ok(Some((parked.id, cmd)))
}

async fn serve<S>(
    ws: &mut Ws<S>,
    gate: &Gate,
    mut events: tokio::sync::broadcast::Receiver<Event>,
    uploads: &mut BTreeSet<String>,
    in_flight: &mut InFlight,
    out: &mut Out<'_>,
) -> End
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (handle, token) = (out.handle, out.token);
    let mut parked: Option<Parked> = None;
    loop {
        tokio::select! {
            incoming = ws.next(), if in_flight.len() < IN_FLIGHT => {
                let (id, mut cmd, filled, like) = match (incoming, parked.take()) {
                    (Some(Ok(Message::Ping(_) | Message::Pong(_))), waiting) => {
                        parked = waiting;
                        continue;
                    }
                    (Some(Ok(Message::Close(_))) | None | Some(Err(_)), _) => return End(None),
                    // the bytes of the upload_chunk just read
                    (Some(Ok(Message::Binary(frame))), Some(waiting)) => match fill_chunk(waiting, &frame) {
                        Ok(Some((id, cmd))) => (id, cmd, true, None),
                        Ok(None) => continue,
                        Err(end) => return end,
                    },
                    (Some(Ok(Message::Text(text))), None) => {
                        let Ok(ClientFrame::Cmd { id, cmd, like }) = serde_json::from_str(text.as_str()) else {
                            return End(Some(CLOSE_PROTOCOL));
                        };
                        (id, cmd, false, like)
                    }
                    // bytes nobody announced, or anything but the bytes that were due
                    (Some(Ok(_)), _) => return End(Some(CLOSE_PROTOCOL)),
                };
                // the key is re-checked on every frame: a rotated key ends the session
                if !molt_mcp::seat_token_matches(handle, token).await {
                    return End(Some(CLOSE_UNAUTHORIZED));
                }
                // a filled chunk passed the gate when its command was read
                if !filled {
                    let chunk = match &*cmd {
                        // an upload's bytes ride their own frame, never the JSON
                        Command::UploadChunk { bytes, .. } if !bytes.is_empty() => return End(Some(CLOSE_PROTOCOL)),
                        Command::UploadChunk { .. } => true,
                        _ => false,
                    };
                    if let Some(why) = gate.refusal(&cmd).or_else(|| session_refusal(&cmd, uploads)) {
                        let name: &'static str = (&*cmd).into();
                        tracing::warn!(cmd = name, why, "web door: refused");
                        if chunk {
                            parked = Some(Parked { id, cmd: None });
                        }
                        let refused = ServerFrame::Reply { id, result: Err(MoltError::Refused(why.into())) };
                        if let Err(end) = send(ws, out, &refused).await {
                            return end;
                        }
                        continue;
                    }
                    if chunk {
                        // executed once its bytes arrive
                        parked = Some(Parked { id, cmd: Some(cmd) });
                        continue;
                    }
                }
                clamp_cap(&mut cmd);
                // which upload a success takes out of this session's hands
                let spent = match &*cmd {
                    Command::UploadAbort { .. } | Command::ShareUpload { .. } | Command::Propose { .. } => {
                        upload_of(&cmd).map(str::to_string)
                    }
                    _ => None,
                };
                let handle = handle.clone();
                in_flight.push(Box::pin(async move { (id, like, spent, handle.execute(*cmd).await) }));
            }
            Some((id, like, spent, result)) = in_flight.next() => {
                // an answer (a phrase, say) never goes to a key that died meanwhile
                if !molt_mcp::seat_token_matches(handle, token).await {
                    return End(Some(CLOSE_UNAUTHORIZED));
                }
                note_upload(uploads, &result, spent);
                if let Err(end) = send_reply(ws, out, id, like, result).await {
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
                if let Err(end) = send(ws, out, &frame).await {
                    return end;
                }
            }
        }
    }
}

/// A reply; one that carries bulk bytes sends them after it as chunks, so
/// no frame holds a whole file as a JSON number array.
///
/// A large reply is kept, and one whose client names the reply it still
/// holds (`like`) goes out as what changed since (§5.7).
async fn send_reply<S>(
    ws: &mut Ws<S>,
    out: &mut Out<'_>,
    id: u64,
    like: Option<u64>,
    result: Result<Reply, MoltError>,
) -> Result<(), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (result, bytes) = match result {
        Ok(Reply::UploadBytes { bytes }) => (Ok(Reply::UploadBytes { bytes: Vec::new() }), Some(bytes)),
        other => (other, None),
    };
    let Ok(text) = serde_json::to_string(&result) else {
        return Err(End(Some(CLOSE_PROTOCOL)));
    };
    // named or not, a base is spent by the request that names it
    let base = like.and_then(|like| Some((like, out.kept.take(like)?)));
    let (text, frame) = patch_frame(id, base, text).await;
    let frame = frame.unwrap_or_else(|| reply_frame(id, &text));
    if (PATCH_MIN..=MAX_UNPACKED).contains(&text.len()) {
        out.kept.keep(id, text);
    }
    send_text(ws, out, frame).await?;
    match bytes {
        Some(bytes) => write_chunks(ws, out, id, &bytes, false).await,
        None => Ok(()),
    }
}

/// `bytes` onto the socket as chunk frames, encoded one at a time (never
/// a second copy of the whole). Each chunk waits for the socket, so on a
/// slow link a transfer takes minutes: from the second chunk on the key
/// is checked first, and a transfer stops with its key.
async fn write_chunks<S>(ws: &mut Ws<S>, out: &Out<'_>, id: u64, bytes: &[u8], packed: bool) -> Result<(), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    for (i, frame) in chunks(id, bytes, packed).enumerate() {
        if i > 0 && !molt_mcp::seat_token_matches(out.handle, out.token).await {
            return Err(End(Some(CLOSE_UNAUTHORIZED)));
        }
        write(ws, Message::binary(frame)).await?;
    }
    Ok(())
}

/// `bytes` as chunk frames; always at least one, the last one flagged.
/// `packed`: the bytes are a deflated text frame.
fn chunks(id: u64, bytes: &[u8], packed: bool) -> impl Iterator<Item = Vec<u8>> + '_ {
    // an empty reply is still one (empty, last) chunk
    let count = bytes.len().div_ceil(CHUNK).max(1);
    let mut offset = 0u64;
    (0..count).map(move |i| {
        let data = bytes.chunks(CHUNK).nth(i).unwrap_or_default();
        let chunk = Chunk { id, offset, last: i + 1 == count, data };
        offset += u64::try_from(data.len()).unwrap_or(u64::MAX);
        if packed {
            chunk.encode_packed()
        } else {
            chunk.encode()
        }
    })
}

/// `text` deflated, if it is in the size window and got smaller; the text
/// comes back either way. `None`: the blocking pool is gone, and the
/// process with it.
async fn packed(text: String) -> Option<(String, Option<Vec<u8>>)> {
    if !(PACK_MIN..=MAX_UNPACKED).contains(&text.len()) {
        return Some((text, None));
    }
    let deflate = |text: String| {
        let packed = crate::pack::deflate(text.as_bytes()).ok().filter(|p| p.len() < text.len());
        (text, packed)
    };
    if text.len() < PACK_INLINE {
        return Some(deflate(text));
    }
    // megabytes of JSON: off the runtime's threads
    tokio::task::spawn_blocking(move || deflate(text)).await.ok()
}

/// The patch frame of a large reply on the reply its client holds, if
/// that saves at least half of it. Off the session's thread: a delta of
/// megabytes takes a moment.
async fn patch_frame(id: u64, base: Option<(u64, String)>, text: String) -> (String, Option<String>) {
    let Some((base, old)) = base.filter(|_| (PATCH_MIN..=MAX_UNPACKED).contains(&text.len())) else {
        return (text, None);
    };
    let made = tokio::task::spawn_blocking(move || {
        let frame = delta(&old, &text)
            .filter(|delta| delta.len() < text.len() / 2)
            .and_then(|delta| serde_json::to_string(&ServerFrame::Patch { id, base, delta }).ok());
        (text, frame)
    });
    // a text lost with its thread cannot be sent either way
    made.await.unwrap_or_default()
}

async fn send<S>(ws: &mut Ws<S>, out: &mut Out<'_>, frame: &ServerFrame) -> Result<(), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(text) = serde_json::to_string(frame) else {
        return Err(End(Some(CLOSE_PROTOCOL)));
    };
    send_text(ws, out, text).await
}

async fn send_text<S>(ws: &mut Ws<S>, out: &mut Out<'_>, text: String) -> Result<(), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if !out.packs {
        return write(ws, Message::text(text)).await;
    }
    match packed(text).await {
        Some((_, Some(bytes))) => {
            out.packed += 1;
            // back to back: the client holds one packed frame at a time
            write_chunks(ws, out, out.packed, &bytes, true).await
        }
        Some((text, None)) => write(ws, Message::text(text)).await,
        None => Err(End(None)),
    }
}

/// One frame onto the socket. No timeout here: the socket itself drops a
/// peer that takes nothing for the door's stall time (`server.rs`), and a
/// peer that keeps reading may take as long as its link needs.
async fn write<S>(ws: &mut Ws<S>, message: Message) -> Result<(), End>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    ws.send(message).await.map_err(|_| End(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bulk_read_is_clamped_to_the_ceiling() {
        let id = molt_core::MessageId([1; 16]);
        for mut cmd in [
            Command::ReadUploadBytes { checksum: "c".into(), cap: u64::MAX },
            Command::ReadFileBytes { id, cap: u64::MAX },
            Command::ReadImageBytes { key: "k".into(), cap: u64::MAX },
            Command::ReadExportBytes { cap: u64::MAX },
        ] {
            clamp_cap(&mut cmd);
            let (Command::ReadUploadBytes { cap, .. }
            | Command::ReadFileBytes { cap, .. }
            | Command::ReadImageBytes { cap, .. }
            | Command::ReadExportBytes { cap }) = cmd
            else {
                panic!("a read");
            };
            assert_eq!(cap, MAX_TRANSFER);
        }
        let mut small = Command::ReadExportBytes { cap: 7 };
        clamp_cap(&mut small);
        assert!(matches!(small, Command::ReadExportBytes { cap: 7 }), "a smaller cap stays");
    }

    #[test]
    fn an_upload_is_its_sessions() {
        let mine = BTreeSet::from(["u1".to_string()]);
        let chunk = |u: &str| Command::UploadChunk { upload: u.into(), offset: 0, bytes: Vec::new(), last: false };
        assert_eq!(session_refusal(&chunk("u1"), &mine), None);
        assert_eq!(session_refusal(&chunk("u2"), &mine), Some("unknown upload"));
        assert_eq!(session_refusal(&Command::UploadAbort { upload: "u2".into() }, &mine), Some("unknown upload"));
        let share = Command::ShareUpload { upload: "u2".into(), channel: Default::default() };
        assert_eq!(session_refusal(&share, &mine), Some("unknown upload"));
        let propose = |payload| Command::Propose { surface: molt_core::Surface::Organization, payload };
        assert_eq!(session_refusal(&propose(serde_json::json!({ "upload": "u2" })), &mine), Some("unknown upload"));
        assert_eq!(session_refusal(&propose(serde_json::json!({ "upload": "u1" })), &mine), None);
        assert_eq!(session_refusal(&propose(serde_json::json!({ "bytes_b64": "x" })), &mine), None);
        assert_eq!(session_refusal(&Command::UploadBegin { name: "a".into(), size: MAX_TRANSFER }, &mine), None);
        assert_eq!(
            session_refusal(&Command::UploadBegin { name: "a".into(), size: MAX_TRANSFER + 1 }, &mine),
            Some("too large")
        );
        assert_eq!(session_refusal(&Command::ReadSession, &mine), None);
    }

    #[test]
    fn a_chunk_frame_fills_only_its_own_command() {
        let cmd = || Box::new(Command::UploadChunk { upload: "u".into(), offset: 4, bytes: Vec::new(), last: true });
        let frame = |id, offset, last, data: &[u8]| Chunk { id, offset, last, data }.encode();
        let parked = |cmd| Parked { id: 9, cmd };
        match fill_chunk(parked(Some(cmd())), &frame(9, 4, true, b"abc")) {
            Ok(Some((9, filled))) => {
                assert!(matches!(*filled, Command::UploadChunk { ref bytes, offset: 4, last: true, .. } if bytes == b"abc"));
            }
            _ => panic!("the matching frame fills the command"),
        }
        for bad in [
            frame(8, 4, true, b"abc"),
            frame(9, 5, true, b"abc"),
            frame(9, 4, false, b"abc"),
            frame(9, 4, true, &vec![0u8; CHUNK + 1]),
            vec![1, 2, 3],
        ] {
            assert!(matches!(fill_chunk(parked(Some(cmd())), &bad), Err(End(Some(CLOSE_PROTOCOL)))));
        }
        // a refused command's bytes are swallowed, whatever they say
        assert!(matches!(fill_chunk(parked(None), &frame(9, 0, false, b"zzz")), Ok(None)));
        assert!(fill_chunk(parked(None), &frame(8, 0, false, b"zzz")).is_err(), "but not another id's");
    }

    #[test]
    fn bytes_split_into_bounded_chunks_that_reassemble() {
        let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(2 * CHUNK + 5).collect();
        let frames: Vec<_> = chunks(9, &bytes, false).collect();
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

    /// A frame is packed only inside the size window, and only if that
    /// made it smaller. (Only for a client that asked: `slow_link.rs`.)
    #[tokio::test]
    async fn only_a_frame_in_the_window_that_shrinks_is_packed() {
        use std::io::Read;
        let large = "one two ".repeat(PACK_INLINE / 4);
        assert!(large.len() > PACK_INLINE, "the blocking-thread path");
        let (text, bytes) = packed(large.clone()).await.expect("answered");
        let bytes = bytes.expect("packed");
        assert_eq!(text, large, "the text comes back");
        assert!(bytes.len() * 10 < large.len());
        let mut back = String::new();
        flate2::read::DeflateDecoder::new(bytes.as_slice()).read_to_string(&mut back).expect("raw deflate");
        assert_eq!(back, large);

        let at = |len: usize| "a".repeat(len);
        assert!(packed(at(PACK_MIN - 1)).await.expect("answered").1.is_none(), "a small frame stays as it is");
        assert!(packed(at(PACK_MIN)).await.expect("answered").1.is_some(), "the window opens at PACK_MIN");
        // what deflate cannot shrink goes out as it is
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let noise: String = (0..PACK_MIN * 2)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from_u32(0x4e00 + u32::try_from(x % 20_000).expect("small")).expect("a char")
            })
            .collect();
        let (text, bytes) = packed(noise.clone()).await.expect("answered");
        assert_eq!(text, noise);
        assert!(bytes.is_none_or(|b| b.len() < noise.len()), "never a larger frame");
    }

    /// A security property, not a tuning choice (`docs/web_gui.md` §5.6
    /// C): with one deflate stream per connection the second frame below
    /// would be a few bytes naming the first, and a frame's length would
    /// tell what an earlier one held.
    #[tokio::test]
    async fn every_packed_frame_is_its_own_stream() {
        use std::io::Read;
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let text: String = (0..PACK_MIN * 8)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from(b'a' + u8::try_from(x % 16).expect("small"))
            })
            .collect();
        let first = packed(text.clone()).await.expect("answered").1.expect("packed");
        let second = packed(text.clone()).await.expect("answered").1.expect("packed");
        assert_eq!(second, first, "the same text packs the same, whatever went before");
        let mut back = String::new();
        flate2::read::DeflateDecoder::new(second.as_slice()).read_to_string(&mut back).expect("inflates alone");
        assert_eq!(back, text);
    }

    #[test]
    fn a_packed_frame_splits_into_flagged_chunks() {
        let bytes = vec![5u8; CHUNK + 1];
        let frames: Vec<_> = chunks(4, &bytes, true).collect();
        assert_eq!(frames.len(), 2);
        for (i, f) in frames.iter().enumerate() {
            assert!(Chunk::decode(f).is_none(), "not a chunk of bytes");
            let (c, packed) = Chunk::decode_any(f).expect("a chunk");
            assert!(packed);
            assert_eq!((c.id, c.last), (4, i == 1));
        }
    }

    #[test]
    fn an_exact_multiple_and_an_empty_reply_end_with_a_last_chunk() {
        let frames: Vec<_> = chunks(1, &vec![7u8; CHUNK], false).collect();
        assert_eq!(frames.len(), 1);
        assert!(Chunk::decode(&frames[0]).expect("a chunk").last);
        let frames: Vec<_> = chunks(1, &[], false).collect();
        assert_eq!(frames.len(), 1);
        let c = Chunk::decode(&frames[0]).expect("a chunk");
        assert!(c.last && c.data.is_empty());
    }
}
