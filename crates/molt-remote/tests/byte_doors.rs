// SPDX-License-Identifier: GPL-3.0-or-later
//! The byte doors through the web door (`docs/web_gui.md` §5.1, §7): a
//! file goes up as chunk frames into the engine's upload store and comes
//! back as chunk frames, against a real engine with a workspace on disk.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use molt_core::wire::{Caps, Chunk, ClientFrame, ServerFrame, CHUNK, MAX_TRANSFER, WIRE_VERSION};
use molt_core::{ChannelRef, Command, GroupConfig, MoltError, Reply, SessionView};
use molt_engine::WalletHandle;
use molt_remote::{serve, WebDoor};
use molt_ui_kit::client::{remote, RemoteHandle, Wire};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const SEAT: &str = "seat-key";
type Ws = WebSocketStream<TcpStream>;

struct Node {
    port: u16,
    /// The engine behind the door.
    handle: WalletHandle,
    /// `<workspace>/uploads`.
    store: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn session(handle: &WalletHandle) -> Box<SessionView> {
    match handle.execute(Command::ReadSession).await.expect("session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// A node with a founded republic on disk and a web door in front of it.
async fn node() -> Node {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut sv = SessionView::default();
    sv.settings.mcp_token = SEAT.to_string();
    sv.settings.workspace_dir = tmp.path().join("workspaces").display().to_string();
    let handle = molt_engine::__spawn_sim_founding(GroupConfig::demo(), sv, true);
    handle
        .execute(Command::CreateStart {
            name: "Store".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
    let mut confirmed = false;
    loop {
        let s = session(&handle).await;
        match s.create.run.outcome {
            1 => break,
            2 => panic!("founding failed: {:?}", s.create.run.log),
            _ => {}
        }
        if !confirmed && !s.create.seed.is_empty() {
            handle.execute(Command::ConfirmSeedBackup { phrase: s.create.seed.clone() }).await.expect("confirm");
            confirmed = true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.execute(Command::CreateFinish).await.expect("finish");
    let id = session(&handle).await.active_workspace.clone();
    let dir = molt_storage_dir(&tmp.path().join("workspaces"), &id);

    let bundle = tmp.path().join("web");
    std::fs::create_dir(&bundle).expect("bundle dir");
    std::fs::write(bundle.join("index.html"), "<!doctype html><canvas id=canvas>").expect("index");
    std::fs::write(bundle.join("molt_web.js"), "export default 1").expect("js");
    std::fs::write(bundle.join("molt_web_bg.wasm"), [0u8, 97, 115, 109]).expect("wasm");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let door = WebDoor { build: "9.9.9".into(), caps: Caps::default(), dir: bundle, pre_auth: Duration::from_secs(10) };
    tokio::spawn(serve(listener, handle.clone(), door));
    Node { port, handle, store: dir.join("uploads"), _tmp: tmp }
}

/// The one workspace directory under `root` (its name is the engine's).
fn molt_storage_dir(root: &Path, id: &str) -> PathBuf {
    let dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .expect("the workspace root")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    assert_eq!(dirs.len(), 1, "one workspace ({id}) under {}", root.display());
    dirs[0].clone()
}

async fn open(n: &Node) -> Ws {
    let host = format!("127.0.0.1:{}", n.port);
    let mut req = format!("ws://{host}/ws").into_client_request().expect("request");
    req.headers_mut().insert("origin", format!("http://{host}").parse().expect("origin"));
    let stream = TcpStream::connect(("127.0.0.1", n.port)).await.expect("connect");
    tokio_tungstenite::client_async(req, stream).await.expect("upgrade").0
}

async fn next(ws: &mut Ws) -> Message {
    tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("a frame within 10 s")
        .expect("the socket is open")
        .expect("a clean frame")
}

fn close_code(m: &Message) -> Option<u16> {
    match m {
        Message::Close(Some(c)) => Some(c.code.into()),
        _ => None,
    }
}

/// A welcomed raw session: the frames are written by hand.
async fn welcomed(n: &Node) -> Ws {
    let mut ws = open(n).await;
    let hello = ClientFrame::Hello { v: WIRE_VERSION, token: SEAT.into() };
    ws.send(Message::text(serde_json::to_string(&hello).expect("json"))).await.expect("hello");
    match next(&mut ws).await {
        Message::Text(t) => assert!(t.as_str().contains("welcome"), "{t}"),
        other => panic!("unexpected {other:?}"),
    }
    ws
}

async fn cmd(ws: &mut Ws, id: u64, cmd: Command) {
    let frame = ClientFrame::Cmd { id, cmd: Box::new(cmd) };
    ws.send(Message::text(serde_json::to_string(&frame).expect("json"))).await.expect("send");
}

/// The reply to `id`, skipping events.
async fn reply(ws: &mut Ws, id: u64) -> Result<Box<Reply>, MoltError> {
    loop {
        match next(ws).await {
            Message::Text(t) => {
                if let ServerFrame::Reply { id: got, result } = serde_json::from_str(t.as_str()).expect("a frame") {
                    assert_eq!(got, id);
                    return result;
                }
            }
            other => panic!("expected a reply to {id}, got {other:?}"),
        }
    }
}

/// The session's next close, skipping events.
async fn closed(ws: &mut Ws) -> Option<u16> {
    loop {
        match next(ws).await {
            Message::Text(_) => {}
            other => return close_code(&other),
        }
    }
}

async fn begin(ws: &mut Ws, id: u64, name: &str, size: u64) -> String {
    cmd(ws, id, Command::UploadBegin { name: name.to_string(), size }).await;
    match *reply(ws, id).await.expect("begun") {
        Reply::UploadProgress { upload, .. } => upload,
        other => panic!("unexpected: {other:?}"),
    }
}

/// One chunk the way the protocol wants it: the command, then its bytes.
async fn chunk(ws: &mut Ws, id: u64, upload: &str, offset: u64, data: &[u8], last: bool) {
    cmd(ws, id, Command::UploadChunk { upload: upload.to_string(), offset, bytes: Vec::new(), last }).await;
    ws.send(Message::binary(Chunk { id, offset, last, data }.encode())).await.expect("bytes");
}

async fn await_gone(path: &Path) {
    for _ in 0..500 {
        if !path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{} is still there", path.display());
}

/// The client the browser ships, on a real socket to the door.
async fn client(n: &Node) -> RemoteHandle {
    let ws = open(n).await;
    let (sink, stream) = ws.split();
    let incoming = stream
        .filter_map(|m| async move {
            match m {
                Ok(Message::Text(t)) => Some(Wire::Text(t.as_str().to_string())),
                Ok(Message::Binary(b)) => Some(Wire::Binary(b.to_vec())),
                _ => None,
            }
        })
        .boxed();
    let outgoing = Box::pin(sink.with(|w: Wire| async move {
        Ok::<_, tokio_tungstenite::tungstenite::Error>(match w {
            Wire::Text(t) => Message::text(t),
            Wire::Binary(b) => Message::binary(b),
        })
    }));
    let (handle, pump) = remote();
    let (tx, rx) = futures_channel::oneshot::channel();
    tokio::spawn(async move { pump.run(SEAT, incoming, outgoing, tx).await });
    rx.await.expect("welcomed");
    handle
}

/// The keystone: the shipped client puts a three-chunk file through the
/// door, shares it, and reads it back by the share's id - chunk frames
/// both ways, no path anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_goes_up_is_shared_and_comes_back_by_id() {
    let n = node().await;
    let c = client(&n).await;
    let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(2 * CHUNK + 7).collect();
    let up = c.upload("notes.txt", &bytes).await.expect("uploaded");
    assert_eq!(std::fs::read(n.store.join(&up).join("notes.txt")).expect("landed"), bytes);

    let id = match c.execute(Command::ShareUpload { upload: up, channel: ChannelRef::default() }).await.expect("shared") {
        Reply::Sent { id } => id,
        other => panic!("unexpected: {other:?}"),
    };
    // the share posts once its hash is done
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let got = loop {
        match c.execute(Command::ReadFileBytes { id, cap: u64::MAX }).await {
            Ok(Reply::UploadBytes { bytes }) => break bytes,
            Ok(other) => panic!("unexpected: {other:?}"),
            Err(e) => assert!(tokio::time::Instant::now() < deadline, "the share never read back: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(got, bytes);
}

/// A session that ends takes what it did not finish along: the partial
/// upload and the whole one nobody shared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_socket_deletes_its_unshared_uploads() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    let half = begin(&mut ws, 1, "half.bin", 6).await;
    chunk(&mut ws, 2, &half, 0, b"abc", false).await;
    assert!(matches!(*reply(&mut ws, 2).await.expect("written"), Reply::UploadProgress { received: 3, done: false, .. }));
    let whole = begin(&mut ws, 3, "whole.bin", 3).await;
    chunk(&mut ws, 4, &whole, 0, b"abc", true).await;
    assert!(matches!(*reply(&mut ws, 4).await.expect("written"), Reply::UploadProgress { done: true, .. }));
    assert!(n.store.join(&half).exists() && n.store.join(&whole).join("whole.bin").exists());
    drop(ws);
    await_gone(&n.store.join(&half)).await;
    await_gone(&n.store.join(&whole)).await;
}

/// Uploads the engine still lets a client open: every one begun here is
/// dropped again.
async fn free_slots(handle: &WalletHandle) -> usize {
    let mut begun = Vec::new();
    while let Ok(Reply::UploadProgress { upload, .. }) =
        handle.execute(Command::UploadBegin { name: "probe.bin".into(), size: 1 }).await
    {
        begun.push(upload);
    }
    for upload in &begun {
        handle.execute(Command::UploadAbort { upload: upload.clone() }).await.expect("abort");
    }
    begun.len()
}

/// A socket that closes with `upload_begin`s still unanswered leaves no
/// upload behind: the engine ran them, so the session's end drops them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_closed_before_its_begins_are_answered_leaves_nothing_open() {
    let n = node().await;
    let all = free_slots(&n.handle).await;
    assert!(all > 1, "the engine opens several uploads at once");
    for _ in 0..12 {
        let mut ws = welcomed(&n).await;
        for id in 1..=u64::try_from(all).expect("small") {
            cmd(&mut ws, id, Command::UploadBegin { name: "a.bin".into(), size: 6 }).await;
        }
        // gone without reading one reply
        drop(ws);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let free = free_slots(&n.handle).await;
            if free == all {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{} uploads nobody owns", all - free);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// An upload belongs to the session that began it: another session's
/// chunk, abort, share or picture proposal for it is refused before the
/// engine - and the refused chunk's bytes are taken off the wire, so the
/// session goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_sessions_upload_is_unknown() {
    let n = node().await;
    let mut a = welcomed(&n).await;
    let mut b = welcomed(&n).await;
    let up = begin(&mut a, 1, "a.bin", 6).await;

    let refused = |r: Result<Box<Reply>, MoltError>| match r {
        Err(MoltError::Refused(why)) => why.to_string(),
        other => panic!("expected a refusal, got {other:?}"),
    };
    chunk(&mut b, 1, &up, 0, b"zzz", false).await;
    assert_eq!(refused(reply(&mut b, 1).await), "unknown upload");
    cmd(&mut b, 2, Command::UploadAbort { upload: up.clone() }).await;
    assert_eq!(refused(reply(&mut b, 2).await), "unknown upload");
    cmd(&mut b, 3, Command::ShareUpload { upload: up.clone(), channel: ChannelRef::default() }).await;
    assert_eq!(refused(reply(&mut b, 3).await), "unknown upload");
    let propose = Command::Propose {
        surface: molt_core::Surface::Organization,
        payload: serde_json::json!({ "op": "set_image", "upload": up }),
    };
    cmd(&mut b, 4, propose).await;
    assert_eq!(refused(reply(&mut b, 4).await), "unknown upload");
    cmd(&mut b, 5, Command::ReadSession).await;
    assert!(matches!(*reply(&mut b, 5).await.expect("still served"), Reply::Session(_)));

    // the owner's upload is untouched
    chunk(&mut a, 2, &up, 0, b"abc", false).await;
    reply(&mut a, 2).await.expect("first");
    chunk(&mut a, 3, &up, 3, b"def", true).await;
    assert!(matches!(*reply(&mut a, 3).await.expect("last"), Reply::UploadProgress { received: 6, done: true, .. }));
    assert_eq!(std::fs::read(n.store.join(&up).join("a.bin")).expect("landed"), b"abcdef");
}

/// The chunk frame is the command's other half: anything that is not
/// exactly it ends the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chunk_frame_that_does_not_match_its_command_ends_the_session() {
    let n = node().await;
    let chunk_cmd = |upload: &str| Command::UploadChunk { upload: upload.to_string(), offset: 0, bytes: Vec::new(), last: false };

    // another id
    let mut ws = welcomed(&n).await;
    let up = begin(&mut ws, 1, "a.bin", 6).await;
    cmd(&mut ws, 2, chunk_cmd(&up)).await;
    ws.send(Message::binary(Chunk { id: 3, offset: 0, last: false, data: b"abc" }.encode())).await.expect("send");
    assert_eq!(closed(&mut ws).await, Some(4400));

    // another offset than the command named
    let mut ws = welcomed(&n).await;
    let up = begin(&mut ws, 1, "a.bin", 6).await;
    cmd(&mut ws, 2, chunk_cmd(&up)).await;
    ws.send(Message::binary(Chunk { id: 2, offset: 3, last: false, data: b"abc" }.encode())).await.expect("send");
    assert_eq!(closed(&mut ws).await, Some(4400));

    // another last flag
    let mut ws = welcomed(&n).await;
    let up = begin(&mut ws, 1, "a.bin", 6).await;
    cmd(&mut ws, 2, chunk_cmd(&up)).await;
    ws.send(Message::binary(Chunk { id: 2, offset: 0, last: true, data: b"abc" }.encode())).await.expect("send");
    assert_eq!(closed(&mut ws).await, Some(4400));

    // more than a chunk
    let mut ws = welcomed(&n).await;
    let up = begin(&mut ws, 1, "a.bin", 600_000).await;
    cmd(&mut ws, 2, chunk_cmd(&up)).await;
    let fat = vec![0u8; CHUNK + 1];
    ws.send(Message::binary(Chunk { id: 2, offset: 0, last: false, data: &fat }.encode())).await.expect("send");
    assert_eq!(closed(&mut ws).await, Some(4400));

    // a command where the bytes were due
    let mut ws = welcomed(&n).await;
    let up = begin(&mut ws, 1, "a.bin", 6).await;
    cmd(&mut ws, 2, chunk_cmd(&up)).await;
    cmd(&mut ws, 3, Command::ReadSession).await;
    assert_eq!(closed(&mut ws).await, Some(4400));

    // bytes inside the JSON
    let mut ws = welcomed(&n).await;
    let up = begin(&mut ws, 1, "a.bin", 6).await;
    cmd(&mut ws, 2, Command::UploadChunk { upload: up, offset: 0, bytes: b"abc".to_vec(), last: false }).await;
    assert_eq!(closed(&mut ws).await, Some(4400));

    // every one of those sessions left its upload behind: all gone
    for _ in 0..500 {
        if !n.store.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the ended sessions' uploads are still in the store");
}

/// The door's own ceiling, before the engine is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_over_the_ceiling_is_refused_at_the_door() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    cmd(&mut ws, 1, Command::UploadBegin { name: "big.bin".into(), size: MAX_TRANSFER + 1 }).await;
    assert!(matches!(reply(&mut ws, 1).await, Err(MoltError::Refused(why)) if why == "too large"));
    assert!(!n.store.exists());
    // the engine refuses a path as a name through this door as through any
    cmd(&mut ws, 2, Command::UploadBegin { name: "../escape".into(), size: 1 }).await;
    assert!(matches!(reply(&mut ws, 2).await, Err(MoltError::BadPayload(_))));
    assert!(!n.store.exists());
}
