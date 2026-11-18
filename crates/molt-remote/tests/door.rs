// SPDX-License-Identifier: GPL-3.0-or-later
//! The web door against a real engine over a real loopback socket
//! (`docs/web_gui.md` §5): who gets in, what they may send, what reaches
//! them, and what ends the session.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use molt_core::wire::{Caps, ClientFrame, ServerFrame, WIRE_VERSION};
use molt_core::{Command, GroupConfig, MoltError, Reply, SessionView};
use molt_engine::WalletHandle;
use molt_remote::{serve, WebDoor};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const SEAT: &str = "seat-key";
type Ws = WebSocketStream<TcpStream>;

struct Node {
    port: u16,
    handle: WalletHandle,
    _dir: tempfile::TempDir,
}

async fn node_with(seat: &str, pre_auth: Duration) -> Node {
    let mut sv = SessionView::default();
    sv.settings.mcp_token = seat.to_string();
    sv.settings.mcp_read_token = "read-key".to_string();
    let handle = molt_engine::spawn(GroupConfig::demo(), sv);
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("index.html"), "<!doctype html><canvas id=canvas>").expect("index");
    std::fs::write(dir.path().join("molt_web.js"), "export default 1").expect("js");
    std::fs::write(dir.path().join("molt_web_bg.wasm"), [0u8, 97, 115, 109]).expect("wasm");
    std::fs::write(dir.path().join("secret.txt"), "not served").expect("extra");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let door = WebDoor {
        build: "9.9.9".into(),
        caps: Caps { embedded_tor: true },
        dir: dir.path().to_path_buf(),
        pre_auth,
    };
    tokio::spawn(serve(listener, handle.clone(), door));
    Node { port, handle, _dir: dir }
}

async fn node() -> Node {
    node_with(SEAT, Duration::from_secs(10)).await
}

/// An upgrade with the given Host and Origin; `Err(status)` when refused.
async fn upgrade(port: u16, host: &str, origin: Option<&str>) -> Result<Ws, u16> {
    let mut req = format!("ws://{host}/ws").into_client_request().expect("request");
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().expect("origin"));
    }
    let stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    match tokio_tungstenite::client_async(req, stream).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("unexpected upgrade error: {e}"),
    }
}

async fn open(n: &Node) -> Ws {
    let host = format!("127.0.0.1:{}", n.port);
    upgrade(n.port, &host, Some(&format!("http://{host}"))).await.expect("upgrade")
}

async fn send(ws: &mut Ws, frame: &ClientFrame) {
    ws.send(Message::text(serde_json::to_string(frame).expect("json"))).await.expect("send");
}

/// The next frame, with a deadline so a hung door fails the test.
async fn next(ws: &mut Ws) -> Message {
    tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("a frame within 10 s")
        .expect("the socket is open")
        .expect("a clean frame")
}

async fn next_frame(ws: &mut Ws) -> ServerFrame {
    match next(ws).await {
        Message::Text(t) => serde_json::from_str(t.as_str()).expect("a server frame"),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

fn close_code(m: &Message) -> Option<u16> {
    match m {
        Message::Close(Some(c)) => Some(c.code.into()),
        _ => None,
    }
}

async fn hello(ws: &mut Ws, token: &str) {
    send(ws, &ClientFrame::Hello { v: WIRE_VERSION, token: token.into() }).await;
}

async fn welcomed(n: &Node) -> Ws {
    let mut ws = open(n).await;
    hello(&mut ws, SEAT).await;
    assert!(matches!(next_frame(&mut ws).await, ServerFrame::Welcome { .. }));
    ws
}

/// The reply to `id`, skipping events that arrive in between.
async fn reply(ws: &mut Ws, id: u64) -> Result<Box<Reply>, MoltError> {
    loop {
        if let ServerFrame::Reply { id: got, result } = next_frame(ws).await {
            assert_eq!(got, id);
            return result;
        }
    }
}

async fn get(port: u16, host: &str, path: &str) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .expect("write");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("read");
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("headers end");
    let head = String::from_utf8_lossy(&buf[..split]).to_lowercase();
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).expect("status");
    (status, head, buf[split + 4..].to_vec())
}

#[tokio::test]
async fn the_seat_key_opens_the_door_and_commands_answer() {
    let n = node().await;
    let mut ws = open(&n).await;
    hello(&mut ws, SEAT).await;
    match next_frame(&mut ws).await {
        ServerFrame::Welcome { v, build, caps } => {
            assert_eq!(v, WIRE_VERSION);
            assert_eq!(build, "9.9.9");
            assert!(caps.embedded_tor);
        }
        other => panic!("expected welcome, got {other:?}"),
    }
    send(&mut ws, &ClientFrame::Cmd { id: 7, cmd: Box::new(Command::ReadSession) }).await;
    let r = reply(&mut ws, 7).await.expect("the session");
    assert!(matches!(*r, Reply::Session(_)));
}

#[tokio::test]
async fn a_wrong_key_is_closed_without_a_welcome() {
    let n = node().await;
    for key in ["nope", "read-key", ""] {
        let mut ws = open(&n).await;
        hello(&mut ws, key).await;
        assert_eq!(close_code(&next(&mut ws).await), Some(4401), "{key:?}");
    }
}

#[tokio::test]
async fn a_node_without_a_seat_key_serves_nothing() {
    let n = node_with("", Duration::from_secs(10)).await;
    let host = format!("127.0.0.1:{}", n.port);
    assert_eq!(upgrade(n.port, &host, Some(&format!("http://{host}"))).await.err(), Some(503));
    assert_eq!(get(n.port, &host, "/").await.0, 503);
}

#[tokio::test]
async fn a_command_before_hello_or_a_wrong_version_is_closed() {
    let n = node().await;
    let mut ws = open(&n).await;
    send(&mut ws, &ClientFrame::Cmd { id: 1, cmd: Box::new(Command::ReadSession) }).await;
    assert_eq!(close_code(&next(&mut ws).await), Some(4401));

    let mut ws = open(&n).await;
    send(&mut ws, &ClientFrame::Hello { v: WIRE_VERSION + 1, token: SEAT.into() }).await;
    assert_eq!(close_code(&next(&mut ws).await), Some(4400));
}

#[tokio::test]
async fn an_oversized_hello_is_closed() {
    let n = node().await;
    let mut ws = open(&n).await;
    hello(&mut ws, &"x".repeat(8 * 1024)).await;
    assert_eq!(close_code(&next(&mut ws).await), Some(4401));
}

#[tokio::test]
async fn a_silent_socket_is_closed_at_the_deadline() {
    let n = node_with(SEAT, Duration::from_millis(200)).await;
    let mut ws = open(&n).await;
    assert_eq!(close_code(&next(&mut ws).await), Some(4401));
}

#[tokio::test]
async fn only_the_served_page_may_upgrade() {
    let n = node().await;
    let own = format!("127.0.0.1:{}", n.port);
    let p = n.port;
    // a tunnel on another local port: Host and Origin agree on it
    assert!(upgrade(p, "localhost:9000", Some("http://localhost:9000")).await.is_ok());
    for (host, origin) in [
        (own.as_str(), None),
        (own.as_str(), Some("null")),
        (own.as_str(), Some("http://evil.example")),
        (own.as_str(), Some("http://127.0.0.1:1")),
        (own.as_str(), Some("https://127.0.0.1")),
        ("evil.example", Some("http://evil.example")),
        ("127.0.0.1.evil.example", Some("http://127.0.0.1.evil.example")),
    ] {
        assert_eq!(upgrade(p, host, origin).await.err(), Some(403), "{host} {origin:?}");
    }
}

#[tokio::test]
async fn an_internal_command_is_refused_before_the_engine() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    let forged = json!({"cmd": {"id": 3, "cmd": {"cmd": "ui_publish", "snapshot": {}}}});
    ws.send(Message::text(forged.to_string())).await.expect("send");
    assert!(matches!(reply(&mut ws, 3).await, Err(MoltError::Refused(_))));
    // the session survives a refusal
    send(&mut ws, &ClientFrame::Cmd { id: 4, cmd: Box::new(Command::ReadSession) }).await;
    assert!(reply(&mut ws, 4).await.is_ok());
}

#[tokio::test]
async fn the_door_cannot_empty_its_own_seat_key() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    let patch = Command::PatchSettings { patch: json!({ "mcp_token": "" }) };
    send(&mut ws, &ClientFrame::Cmd { id: 1, cmd: Box::new(patch) }).await;
    assert!(matches!(reply(&mut ws, 1).await, Err(MoltError::Refused(_))));
    assert!(molt_mcp::seat_token_matches(&n.handle, SEAT).await, "the key is untouched");
}

#[tokio::test]
async fn garbage_ends_the_session() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    ws.send(Message::text("{not json")).await.expect("send");
    assert_eq!(close_code(&next(&mut ws).await), Some(4400));
}

#[tokio::test]
async fn engine_events_are_pushed() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    n.handle.execute(Command::SetLanguage { lang: "de".into() }).await.expect("language");
    loop {
        if let ServerFrame::Event(e) = next_frame(&mut ws).await {
            assert!(matches!(*e, molt_core::Event::SessionChanged { .. }));
            break;
        }
    }
}

/// The socket is a push channel: a session that sends nothing must still
/// die with its key, and see nothing after the rotation.
#[tokio::test]
async fn a_rotated_key_closes_a_silent_session() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    n.handle
        .execute(Command::PatchSettings { patch: json!({ "mcp_token": "rotated" }) })
        .await
        .expect("rotates");
    let m = next(&mut ws).await;
    assert_eq!(close_code(&m), Some(4401), "first frame after the rotation: {m:?}");
}

#[tokio::test]
async fn the_bundle_is_a_fixed_list() {
    let n = node().await;
    let host = format!("127.0.0.1:{}", n.port);
    let (status, head, body) = get(n.port, &host, "/").await;
    assert_eq!(status, 200);
    assert!(body.starts_with(b"<!doctype html>"));
    assert!(head.contains("content-type: text/html"));
    assert!(head.contains("content-security-policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; frame-ancestors 'none'"));
    assert!(head.contains("x-content-type-options: nosniff"));
    assert!(head.contains("cache-control: no-store"));
    let (status, head, body) = get(n.port, &host, "/molt_web_bg.wasm").await;
    assert_eq!((status, body), (200, vec![0u8, 97, 115, 109]));
    assert!(head.contains("content-type: application/wasm"));
    assert_eq!(get(n.port, &host, "/molt_web.js").await.0, 200);
    for path in ["/secret.txt", "/../Cargo.toml", "/index.html/..", "/x", "//etc/passwd", "/%2e%2e/secret.txt"] {
        assert_eq!(get(n.port, &host, path).await.0, 404, "{path}");
    }
    assert_eq!(get(n.port, "evil.example", "/").await.0, 403, "a rebinding name gets nothing");
    assert_eq!(get(n.port, &host, "/?x=1").await.0, 404, "the target is matched whole");
    assert!(head.contains("cross-origin-resource-policy: same-origin"));
}

/// Another site's page must not even learn that a node runs here.
#[tokio::test]
async fn a_foreign_page_gets_no_static_file() {
    let n = node().await;
    let host = format!("127.0.0.1:{}", n.port);
    for extra in ["Origin: http://evil.example", "Sec-Fetch-Site: cross-site"] {
        let mut s = TcpStream::connect(("127.0.0.1", n.port)).await.expect("connect");
        s.write_all(format!("GET /molt_web.js HTTP/1.1\r\nHost: {host}\r\n{extra}\r\nConnection: close\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.expect("read");
        assert!(buf.starts_with(b"HTTP/1.1 403"), "{extra}");
    }
    let mut s = TcpStream::connect(("127.0.0.1", n.port)).await.expect("connect");
    s.write_all(format!("POST / HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .expect("write");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("read");
    assert!(buf.starts_with(b"HTTP/1.1 405"));
}

/// The page takes the seat token: a bundle someone else can write, or a
/// link out of the directory, opens no door at all.
#[tokio::test]
async fn an_untrusted_bundle_directory_opens_no_door() {
    use std::os::unix::fs::PermissionsExt;
    let handle = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    let door = |dir: &std::path::Path| WebDoor {
        build: "x".into(),
        caps: Caps::default(),
        dir: dir.to_path_buf(),
        pre_auth: Duration::from_secs(1),
    };
    let fill = |dir: &std::path::Path| {
        for f in ["index.html", "molt_web.js", "molt_web_bg.wasm"] {
            std::fs::write(dir.join(f), "x").expect("file");
        }
    };
    let try_serve = |dir: std::path::PathBuf| {
        let handle = handle.clone();
        async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            tokio::time::timeout(Duration::from_millis(300), serve(listener, handle, door(&dir))).await
        }
    };

    let writable = tempfile::tempdir().expect("tempdir");
    fill(writable.path());
    let f = writable.path().join("molt_web.js");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).expect("chmod");
    assert!(matches!(try_serve(writable.path().to_path_buf()).await, Ok(Err(_))), "a world-writable file");

    let linked = tempfile::tempdir().expect("tempdir");
    fill(linked.path());
    std::fs::remove_file(linked.path().join("index.html")).expect("rm");
    std::os::unix::fs::symlink("/etc/hostname", linked.path().join("index.html")).expect("symlink");
    assert!(matches!(try_serve(linked.path().to_path_buf()).await, Ok(Err(_))), "a symlink out");

    let missing = tempfile::tempdir().expect("tempdir");
    assert!(matches!(try_serve(missing.path().to_path_buf()).await, Ok(Err(_))), "no bundle");

    let good = tempfile::tempdir().expect("tempdir");
    fill(good.path());
    assert!(try_serve(good.path().to_path_buf()).await.is_err(), "a private bundle serves (until the timeout)");
}

#[tokio::test]
async fn bytes_from_a_client_and_a_second_hello_end_the_session() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    ws.send(Message::binary(vec![1u8, 2, 3])).await.expect("send");
    assert_eq!(close_code(&next(&mut ws).await), Some(4400));
    let mut ws = welcomed(&n).await;
    hello(&mut ws, SEAT).await;
    assert_eq!(close_code(&next(&mut ws).await), Some(4400));
}

/// A key cleared through the posture (the GUI's Save path) ends a live
/// session like a rotation does.
#[tokio::test]
async fn a_cleared_key_closes_the_session() {
    let n = node().await;
    let mut ws = welcomed(&n).await;
    let mut posture = molt_core::NodePosture::of(&SessionView::default().settings);
    posture.mcp_token = Some(String::new());
    n.handle.execute(Command::SetNodePosture { posture }).await.expect("clears");
    assert_eq!(close_code(&next(&mut ws).await), Some(4401));
}

/// The protocol core the browser ships (`molt-ui-kit`), driven over a
/// real socket against the real door: the two halves agree.
#[tokio::test]
async fn the_shipped_client_core_speaks_to_the_door() {
    use molt_ui_kit::protocol::{Notice, Protocol};
    let n = node().await;
    let mut ws = open(&n).await;
    let mut p = Protocol::new();
    ws.send(Message::text(Protocol::hello(SEAT))).await.expect("hello");
    /// Feed frames until `done` says so.
    async fn pump(ws: &mut Ws, p: &mut Protocol, notices: &mut Vec<Notice>, mut done: impl FnMut(&[Notice]) -> bool) {
        while !done(notices) {
            match next(ws).await {
                Message::Text(t) => notices.extend(p.on_text(t.as_str()).expect("a sound frame")),
                Message::Binary(b) => p.on_binary(&b).expect("a sound chunk"),
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    let mut notices = Vec::new();
    pump(&mut ws, &mut p, &mut notices, |n| !n.is_empty()).await;
    assert!(matches!(&notices[0], Notice::Welcome { build, caps } if build == "9.9.9" && caps.embedded_tor));

    // two commands at once, answered to their own waiters
    let (f1, mut r1) = p.request(Command::ReadSession);
    let (f2, mut r2) = p.request(Command::UiPublish { snapshot: Default::default() });
    ws.send(Message::text(f1)).await.expect("send");
    ws.send(Message::text(f2)).await.expect("send");
    while p.open_requests() > 0 {
        pump(&mut ws, &mut p, &mut notices, |_| true).await;
        match next(&mut ws).await {
            Message::Text(t) => notices.extend(p.on_text(t.as_str()).expect("a sound frame")),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(matches!(r1.try_recv(), Ok(Some(Ok(Reply::Session(_))))));
    assert!(matches!(r2.try_recv(), Ok(Some(Err(MoltError::Refused(_))))));

    // an engine event reaches the client as a notice
    n.handle.execute(Command::SetLanguage { lang: "de".into() }).await.expect("language");
    let before = notices.len();
    pump(&mut ws, &mut p, &mut notices, |n| n.len() > before).await;
    assert!(notices[before..].iter().any(|n| matches!(n, Notice::Event(_))));
}

