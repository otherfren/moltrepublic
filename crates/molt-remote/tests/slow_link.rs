// SPDX-License-Identifier: GPL-3.0-or-later
//! The web door on a slow link (`docs/web_gui.md` §5.6), against a real
//! engine over real loopback sockets: a reader that keeps taking bytes
//! gets a large reply and a large file whole, one that stops is dropped;
//! the bundle is served in the coding a request offers; a large frame
//! crosses packed to a client that asked and plain to one that did not.

use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use molt_core::wire::{Caps, ClientFrame, ServerFrame, CHUNK};
use molt_core::{ChannelRef, Command, GroupConfig, Reply, SessionView, Surface};
use molt_engine::WalletHandle;
use molt_remote::{serve, WebDoor};
use molt_ui_kit::client::{remote, RemoteHandle, Wire};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const SEAT: &str = "seat-key";
/// The hello every client sent before packed frames existed, byte for byte.
const OLD_HELLO: &str = r#"{"hello":{"v":1,"token":"seat-key"}}"#;
/// The hello of a client that can unpack.
const NEW_HELLO: &str = r#"{"hello":{"v":1,"token":"seat-key","packed":true}}"#;
/// The stall timeout these doors run with: short, so a test can outlast it.
const STALL: Duration = Duration::from_millis(500);

/// Repeatable bytes that do not compress.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Repeatable invented prose of about `len` bytes: it compresses the way
/// a chat log does.
fn prose(len: usize, seed: u64) -> String {
    const WORDS: [&str; 16] = [
        "agenda", "vote", "relay", "the", "a", "of", "meeting", "tomorrow", "draft", "please", "review", "done",
        "thanks", "board", "task", "update",
    ];
    let mut x = seed | 1;
    let mut out = String::with_capacity(len + 16);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push_str(WORDS[usize::try_from(x % 16).expect("small")]);
        out.push(' ');
    }
    out
}

struct Node {
    port: u16,
    handle: WalletHandle,
    /// What the door serves as `molt_web_bg.wasm` and as `molt_web.js`.
    wasm: Vec<u8>,
    script: String,
    _dir: tempfile::TempDir,
}

/// A door with a bundle worth measuring: a module that does not compress
/// and a script that does.
async fn node_with(pre_auth: Duration, wasm_len: usize) -> Node {
    let mut sv = SessionView::default();
    sv.settings.mcp_token = SEAT.to_string();
    let handle = molt_engine::spawn(GroupConfig::demo(), sv);
    let dir = tempfile::tempdir().expect("tempdir");
    let wasm = noise(wasm_len);
    let script = prose(200_000, 7);
    std::fs::write(dir.path().join("index.html"), "<!doctype html><canvas id=canvas>").expect("index");
    std::fs::write(dir.path().join("molt_web.js"), &script).expect("js");
    std::fs::write(dir.path().join("molt_web_bg.wasm"), &wasm).expect("wasm");
    std::fs::write(dir.path().join("page.js"), "import './molt_web.js'").expect("page js");
    std::fs::write(dir.path().join("page.css"), "body{margin:0}").expect("page css");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let door = WebDoor {
        build: "9.9.9".into(),
        caps: Caps::default(),
        dir: dir.path().to_path_buf(),
        pre_auth,
        stall: STALL,
    };
    tokio::spawn(serve(listener, handle.clone(), door));
    // the door encodes its bundle before it serves (slow in a test
    // build): no test's clock starts before the first answer
    assert_eq!(get(port, "/page.css", &[]).await.0, 200);
    Node { port, handle, wasm, script, _dir: dir }
}

async fn node() -> Node {
    node_with(Duration::from_secs(10), 4096).await
}

/// Put about `bytes` of chat into the engine and answer the read of it.
async fn fill_chat(handle: &WalletHandle, bytes: usize) -> Command {
    let each = 100_000;
    for i in 0..bytes.div_ceil(each) {
        let body = prose(each, 1000 + u64::try_from(i).expect("small"));
        handle.execute(Command::Chat { body, quote: None, channel: ChannelRef::Group }).await.expect("chat");
    }
    read_chat()
}

fn read_chat() -> Command {
    Command::ReadState { surface: Surface::Chat, channel: None, view: None }
}

/// What the engine's own handle answers, as JSON: the reference a reply
/// through the door must equal.
async fn direct(handle: &WalletHandle) -> serde_json::Value {
    serde_json::to_value(handle.execute(read_chat()).await.expect("a state")).expect("json")
}

/// A reader that takes `step` bytes, waits `pause`, and reads on: slow,
/// but never stalled.
struct Throttled {
    inner: TcpStream,
    step: usize,
    pause: Duration,
    wait: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl AsyncRead for Throttled {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if let Some(wait) = self.wait.as_mut() {
            if wait.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.wait = None;
        }
        let mut small = [0u8; 16 * 1024];
        let take = self.step.min(small.len()).min(buf.remaining());
        let mut part = ReadBuf::new(&mut small[..take]);
        match Pin::new(&mut self.inner).poll_read(cx, &mut part) {
            Poll::Ready(Ok(())) => {
                buf.put_slice(part.filled());
                self.wait = Some(Box::pin(tokio::time::sleep(self.pause)));
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl tokio::io::AsyncWrite for Throttled {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A connection whose receive buffer is small: what it does not read
/// stays on the server's side of the link.
async fn narrow(port: u16) -> TcpStream {
    let socket = TcpSocket::new_v4().expect("socket");
    socket.set_recv_buffer_size(16 * 1024).expect("a small buffer");
    socket.connect((std::net::Ipv4Addr::LOCALHOST, port).into()).await.expect("connect")
}

fn slow(inner: TcpStream) -> Throttled {
    Throttled { inner, step: 16 * 1024, pause: Duration::from_millis(5), wait: None }
}

async fn upgrade<S>(port: u16, stream: S) -> WebSocketStream<S>
where
    S: AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let host = format!("127.0.0.1:{port}");
    let mut req = format!("ws://{host}/ws").into_client_request().expect("request");
    req.headers_mut().insert("origin", format!("http://{host}").parse().expect("origin"));
    tokio_tungstenite::client_async(req, stream).await.expect("upgrade").0
}

async fn open(n: &Node) -> WebSocketStream<TcpStream> {
    upgrade(n.port, TcpStream::connect(("127.0.0.1", n.port)).await.expect("connect")).await
}

/// Say `hello` and take the welcome, which is a text frame either way.
async fn welcomed<S>(ws: &mut WebSocketStream<S>, hello: &str)
where
    S: AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    ws.send(Message::text(hello)).await.expect("hello");
    match ws.next().await.expect("open").expect("clean") {
        Message::Text(t) => assert!(t.as_str().starts_with(r#"{"welcome""#), "{t}"),
        other => panic!("expected the welcome, got {other:?}"),
    }
}

async fn ask<S>(ws: &mut WebSocketStream<S>, id: u64, cmd: Command)
where
    S: AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let frame = ClientFrame::Cmd { id, cmd: Box::new(cmd), like: None };
    ws.send(Message::text(serde_json::to_string(&frame).expect("json"))).await.expect("send");
}

/// The reply to `id` as a text frame's JSON, skipping events; the text
/// frame's length beside it.
async fn text_reply<S>(ws: &mut WebSocketStream<S>, id: u64) -> (serde_json::Value, usize)
where
    S: AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match ws.next().await.expect("open").expect("clean") {
            Message::Text(t) => {
                if let ServerFrame::Reply { id: got, result } = serde_json::from_str(t.as_str()).expect("a frame") {
                    assert_eq!(got, id);
                    return (serde_json::to_value(*result.expect("answered")).expect("json"), t.len());
                }
            }
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// The client the browser ships on a real socket to the door, and what
/// crossed towards it: (text bytes, binary bytes, binary frames).
async fn client(n: &Node) -> (RemoteHandle, Arc<[AtomicUsize; 3]>) {
    let ws = open(n).await;
    let (sink, stream) = ws.split();
    let seen = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)]);
    let count = seen.clone();
    let incoming = stream
        .filter_map(move |m| {
            let count = count.clone();
            async move {
                match m {
                    Ok(Message::Text(t)) => {
                        count[0].fetch_add(t.len(), Ordering::SeqCst);
                        Some(Wire::Text(t.as_str().to_string()))
                    }
                    Ok(Message::Binary(b)) => {
                        count[1].fetch_add(b.len(), Ordering::SeqCst);
                        count[2].fetch_add(1, Ordering::SeqCst);
                        Some(Wire::Binary(b.to_vec()))
                    }
                    _ => None,
                }
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
    let (welcomed, welcome) = futures_channel::oneshot::channel();
    tokio::spawn(async move { pump.run(SEAT, incoming, outgoing, welcomed).await });
    welcome.await.expect("welcomed");
    (handle, seen)
}

/// One GET with extra header lines: (status, lower-cased head, body).
async fn get(port: u16, path: &str, extra: &[&str]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    let extra: String = extra.iter().map(|l| format!("{l}\r\n")).collect();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{extra}Connection: close\r\n\r\n").as_bytes())
        .await
        .expect("write");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("read");
    split_response(&buf)
}

fn split_response(buf: &[u8]) -> (u16, String, Vec<u8>) {
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("headers end");
    let head = String::from_utf8_lossy(&buf[..split]).to_lowercase();
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).expect("status");
    (status, head, buf[split + 4..].to_vec())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| l.strip_prefix(name).and_then(|rest| rest.strip_prefix(':'))).map(str::trim)
}

// ---- A. a stall, not a total ----

/// The reply that could never cross: larger than every buffer between
/// the two ends, to a reader slower in total than the stall timeout by a
/// wide margin, but never idle. It arrives whole.
#[tokio::test]
async fn a_slow_but_steady_reader_gets_a_large_reply_whole() {
    let n = node().await;
    let read = fill_chat(&n.handle, 12_000_000).await;
    let mut ws = upgrade(n.port, slow(narrow(n.port).await)).await;
    welcomed(&mut ws, OLD_HELLO).await;
    let began = Instant::now();
    ask(&mut ws, 1, read).await;
    let (got, len) = text_reply(&mut ws, 1).await;
    assert!(len > 12_000_000, "one frame of {len} bytes");
    assert!(began.elapsed() > 3 * STALL, "took {:?}: not slower than the timeout", began.elapsed());
    assert_eq!(got, direct(&n.handle).await);
}

/// What the old totals protected stays: a reader that takes nothing for
/// the timeout loses its session, and the reply is not held for it.
#[tokio::test]
async fn a_reader_that_stops_is_dropped() {
    let n = node().await;
    let read = fill_chat(&n.handle, 12_000_000).await;
    let mut ws = upgrade(n.port, narrow(n.port).await).await;
    welcomed(&mut ws, OLD_HELLO).await;
    ask(&mut ws, 1, read).await;
    tokio::time::sleep(4 * STALL).await;
    // reading on now finds a connection the door has closed mid-frame
    let ended = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) if t.len() > 1_000_000 => return false,
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return true,
            }
        }
    })
    .await
    .expect("the socket ends");
    assert!(ended, "a stalled reader still got the whole reply");
}

/// The module is the largest thing the door serves: a slow reader gets
/// every byte of it.
#[tokio::test]
async fn a_slow_but_steady_reader_gets_a_large_file_whole() {
    let n = node_with(Duration::from_secs(10), 12_000_000).await;
    let mut s = slow(narrow(n.port).await);
    let request = format!("GET /molt_web_bg.wasm HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", n.port);
    s.inner.write_all(request.as_bytes()).await.expect("write");
    let began = Instant::now();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("read");
    assert!(began.elapsed() > 3 * STALL, "took {:?}: not slower than the timeout", began.elapsed());
    let (status, head, body) = split_response(&buf);
    assert_eq!(status, 200);
    assert_eq!(header(&head, "content-length"), Some("12000000"));
    assert!(body == n.wasm, "the body is the file ({} of {} bytes)", body.len(), n.wasm.len());
}

/// The link this was built for: a reader that takes far less per stall
/// timeout than a kernel-tuned send buffer (megabytes on loopback) frees
/// before it reports the socket writable again. The door's own small
/// buffer keeps the timer's grain below what such a reader takes.
#[tokio::test]
async fn a_reader_far_slower_than_a_tuned_buffer_gets_a_large_file_whole() {
    let n = node_with(Duration::from_secs(10), 6_000_000).await;
    // 150 KiB per stall timeout
    let mut s = Throttled { inner: narrow(n.port).await, step: 3 * 1024, pause: Duration::from_millis(10), wait: None };
    let request = format!("GET /molt_web_bg.wasm HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", n.port);
    s.inner.write_all(request.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    // a cut connection ends the read early, as an error or as a short body
    let _ = s.read_to_end(&mut buf).await;
    let (status, _, body) = split_response(&buf);
    assert_eq!(status, 200);
    assert!(body == n.wasm, "the body is the file ({} of {} bytes)", body.len(), n.wasm.len());
}

#[tokio::test]
async fn a_reader_that_stops_gets_no_file() {
    let n = node_with(Duration::from_secs(10), 12_000_000).await;
    let mut s = narrow(n.port).await;
    let request = format!("GET /molt_web_bg.wasm HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", n.port);
    s.write_all(request.as_bytes()).await.expect("write");
    tokio::time::sleep(4 * STALL).await;
    let mut buf = Vec::new();
    // a reset is as good an end as a close
    let _ = tokio::time::timeout(Duration::from_secs(20), s.read_to_end(&mut buf)).await.expect("the socket ends");
    assert!(buf.len() < n.wasm.len(), "a stalled reader still got {} bytes", buf.len());
}

/// No total is left on an HTTP connection, so what ends an idle one must
/// be pinned: a client that never asks, and one that asked once and then
/// sits on its keep-alive connection, are both closed at the header
/// deadline.
#[tokio::test]
async fn an_idle_http_connection_is_closed() {
    let n = node_with(Duration::from_millis(300), 4096).await;
    let mut silent = TcpStream::connect(("127.0.0.1", n.port)).await.expect("connect");
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), silent.read_to_end(&mut buf))
        .await
        .expect("a silent connection is closed")
        .expect("read");

    let mut kept = TcpStream::connect(("127.0.0.1", n.port)).await.expect("connect");
    kept.write_all(format!("GET /page.css HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", n.port).as_bytes())
        .await
        .expect("write");
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), kept.read_to_end(&mut buf))
        .await
        .expect("an idle keep-alive connection is closed")
        .expect("read");
    assert_eq!(split_response(&buf).0, 200);
}

// ---- B. the bundle in the coding a request offers ----

fn gunzip(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body).read_to_end(&mut out).expect("gzip");
    out
}

fn unbrotli(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::Decompressor::new(body, 4096).read_to_end(&mut out).expect("brotli");
    out
}

#[tokio::test]
async fn a_file_is_served_in_the_coding_the_request_offers() {
    let n = node().await;
    let plain = n.script.as_bytes();

    // nobody asked: the file itself, and a cache is told the answer varies
    let (status, head, body) = get(n.port, "/molt_web.js", &[]).await;
    assert_eq!((status, body.as_slice()), (200, plain));
    assert_eq!(header(&head, "content-encoding"), None);
    assert_eq!(header(&head, "vary"), Some("accept-encoding"));
    assert_eq!(header(&head, "content-length"), Some(plain.len().to_string().as_str()));

    let (status, head, gz) = get(n.port, "/molt_web.js", &["Accept-Encoding: gzip, deflate"]).await;
    assert_eq!(status, 200);
    assert_eq!(header(&head, "content-encoding"), Some("gzip"));
    assert_eq!(header(&head, "vary"), Some("accept-encoding"));
    assert_eq!(header(&head, "content-length"), Some(gz.len().to_string().as_str()), "the length of what is sent");
    assert_eq!(header(&head, "content-type"), Some("text/javascript; charset=utf-8"));
    assert!(gz.len() * 2 < plain.len(), "gzip: {} of {}", gz.len(), plain.len());
    assert_eq!(gunzip(&gz), plain);

    let (_, head, br) = get(n.port, "/molt_web.js", &["Accept-Encoding: br"]).await;
    assert_eq!(header(&head, "content-encoding"), Some("br"));
    assert_eq!(header(&head, "content-length"), Some(br.len().to_string().as_str()));
    assert!(br.len() * 2 < plain.len(), "brotli: {} of {}", br.len(), plain.len());
    assert_eq!(unbrotli(&br), plain);

    // what a browser sends on localhost: the smaller of the two it takes
    let best = if br.len() < gz.len() { "br" } else { "gzip" };
    let (_, head, body) = get(n.port, "/molt_web.js", &["Accept-Encoding: gzip, deflate, br, zstd"]).await;
    assert_eq!(header(&head, "content-encoding"), Some(best));
    assert_eq!(body.len(), br.len().min(gz.len()));

    for (offer, coding) in [
        ("BR;q=0.5, GZIP;q=1.0", Some(best)),
        ("gzip;q=0, br", Some("br")),
        ("br;q=0, gzip", Some("gzip")),
        ("br ; q=0.000 , gzip ; q=0.8", Some("gzip")),
        ("gzip;q=0, br;q=0", None),
        ("identity", None),
        ("*", None),
        ("zstd", None),
        ("gzipped, brotli", None),
        ("gzip;q=x", None),
        ("", None),
    ] {
        let (status, head, body) = get(n.port, "/molt_web.js", &[&format!("Accept-Encoding: {offer}")]).await;
        assert_eq!(status, 200, "{offer:?}");
        assert_eq!(header(&head, "content-encoding"), coding, "{offer:?}");
        if coding.is_none() {
            assert!(body == plain, "{offer:?}: the file itself");
        }
    }

    // two header lines are one list
    let (_, head, _) = get(n.port, "/molt_web.js", &["Accept-Encoding: gzip", "Accept-Encoding: br"]).await;
    assert_eq!(header(&head, "content-encoding"), Some(best));
}

/// A coding that saves nothing is not served: the module here is noise,
/// the page files a few bytes.
#[tokio::test]
async fn a_file_that_does_not_shrink_is_served_as_it_is() {
    let n = node_with(Duration::from_secs(10), 300_000).await;
    for path in ["/molt_web_bg.wasm", "/page.css"] {
        let (status, head, body) = get(n.port, path, &["Accept-Encoding: gzip, br"]).await;
        assert_eq!(status, 200);
        assert_eq!(header(&head, "content-encoding"), None, "{path}");
        assert_eq!(header(&head, "vary"), Some("accept-encoding"), "{path}");
        assert_eq!(header(&head, "content-length"), Some(body.len().to_string().as_str()));
        if path == "/molt_web_bg.wasm" {
            assert!(body == n.wasm);
        }
    }
    // the door's own headers ride every coding
    let (_, head, _) = get(n.port, "/molt_web.js", &["Accept-Encoding: br"]).await;
    assert_eq!(header(&head, "content-encoding"), Some("br"));
    assert_eq!(header(&head, "cache-control"), Some("no-cache"));
    assert_eq!(header(&head, "x-content-type-options"), Some("nosniff"));
    assert!(header(&head, "content-security-policy").is_some_and(|c| c.starts_with("default-src 'self'")));
}

// ---- C. packed frames, old and new hello ----

/// **Old client, new server.** A hello without `packed` is answered as
/// it always was: the large reply is one text frame and no binary frame
/// crosses.
#[tokio::test]
async fn an_old_hello_gets_every_frame_as_text() {
    let n = node().await;
    let read = fill_chat(&n.handle, 3_000_000).await;
    let mut ws = open(&n).await;
    welcomed(&mut ws, OLD_HELLO).await;
    ask(&mut ws, 1, read).await;
    let (got, len) = text_reply(&mut ws, 1).await;
    assert!(len > 3_000_000, "one text frame of {len} bytes");
    assert_eq!(got, direct(&n.handle).await);
}

/// **New client, new server.** The same reply to a hello that asked:
/// bounded binary frames flagged as packed, a fraction of the bytes; a
/// small frame stays text.
#[tokio::test]
async fn a_new_hello_gets_a_large_reply_packed_and_a_small_one_as_text() {
    let n = node().await;
    let read = fill_chat(&n.handle, 3_000_000).await;
    let mut ws = open(&n).await;
    welcomed(&mut ws, NEW_HELLO).await;

    ask(&mut ws, 1, Command::SetLanguage { lang: "de".into() }).await;
    let (small, len) = text_reply(&mut ws, 1).await;
    assert!(len < 1024, "{small}");

    ask(&mut ws, 2, read).await;
    let mut protocol = molt_ui_kit::protocol::Protocol::new();
    let welcome = ServerFrame::Welcome { v: 1, build: "9.9.9".into(), caps: Caps::default() };
    protocol.on_text(&serde_json::to_string(&welcome).expect("json")).expect("welcomed");
    // the waiter for id 2: this protocol's first two requests
    let (_, _first) = protocol.request(Command::ReadSession);
    let (_, mut answer) = protocol.request(read_chat());
    let (mut wire, mut frames) = (0usize, 0usize);
    let got = loop {
        match ws.next().await.expect("open").expect("clean") {
            Message::Binary(b) => {
                assert!(b.len() <= 17 + CHUNK, "a frame of {} bytes", b.len());
                assert_eq!(b[16] & 2, 2, "flagged as packed");
                wire += b.len();
                frames += 1;
                protocol.on_binary(&b).expect("a sound packed chunk");
            }
            // events (the language change) are small: text
            Message::Text(t) => assert!(t.len() < 1024, "a text frame of {} bytes", t.len()),
            other => panic!("unexpected {other:?}"),
        }
        if let Ok(Some(answer)) = answer.try_recv() {
            break serde_json::to_value(answer.expect("answered")).expect("json");
        }
    };
    assert!(frames > 1, "{frames} frames: more than one chunk");
    assert!(wire * 2 < 3_000_000, "{wire} bytes on the wire");
    assert_eq!(got, direct(&n.handle).await);
}

/// The client the browser ships asks for packed frames and reads them:
/// a multi-megabyte reply arrives intact, as binary frames.
#[tokio::test]
async fn the_shipped_client_reads_a_large_reply_packed() {
    let n = node().await;
    let read = fill_chat(&n.handle, 3_000_000).await;
    let (handle, seen) = client(&n).await;
    let before = seen[1].load(Ordering::SeqCst);
    let got = serde_json::to_value(handle.execute(read).await.expect("a state")).expect("json");
    assert_eq!(got, direct(&n.handle).await);
    let packed = seen[1].load(Ordering::SeqCst) - before;
    assert!(packed > 0 && packed * 2 < 3_000_000, "{packed} binary bytes for the reply");
    assert!(seen[0].load(Ordering::SeqCst) < 100_000, "the reply did not also cross as text");

    // and a later command is still answered on the same session
    assert!(matches!(handle.execute(Command::ReadSession).await, Ok(Reply::Session(_))));
}

/// A slow reader on the packed path: the chunks arrive one stall-timed
/// write after another and the reply is whole.
#[tokio::test]
async fn a_slow_but_steady_reader_gets_a_packed_reply_whole() {
    let n = node().await;
    // noise in the bodies: packed, this reply is still megabytes
    for i in 0..120u64 {
        let body: String = noise(100_000).iter().map(|b| char::from(b'a' + (b ^ i.to_le_bytes()[0]) % 26)).collect();
        n.handle.execute(Command::Chat { body, quote: None, channel: ChannelRef::Group }).await.expect("chat");
    }
    let mut ws = upgrade(n.port, slow(narrow(n.port).await)).await;
    welcomed(&mut ws, NEW_HELLO).await;
    let mut protocol = molt_ui_kit::protocol::Protocol::new();
    let welcome = ServerFrame::Welcome { v: 1, build: "9.9.9".into(), caps: Caps::default() };
    protocol.on_text(&serde_json::to_string(&welcome).expect("json")).expect("welcomed");
    let (out, mut answer) = protocol.request(read_chat());
    let began = Instant::now();
    ws.send(Message::text(out.expect("a frame").text)).await.expect("send");
    let got = loop {
        match ws.next().await.expect("open").expect("clean") {
            Message::Binary(b) => drop(protocol.on_binary(&b).expect("a sound packed chunk")),
            other => panic!("unexpected {other:?}"),
        }
        if let Ok(Some(answer)) = answer.try_recv() {
            break serde_json::to_value(answer.expect("answered")).expect("json");
        }
    };
    assert!(began.elapsed() > 3 * STALL, "took {:?}: not slower than the timeout", began.elapsed());
    assert_eq!(got, direct(&n.handle).await);
}

/// A reply may now take minutes to cross, and the socket is still a push
/// channel that dies with its key: a key rotated while a packed reply is
/// on its way stops it at the next chunk, with the code for a dead key.
#[tokio::test]
async fn a_rotated_key_stops_a_reply_on_its_way() {
    let n = node().await;
    for i in 0..120u64 {
        let body: String = noise(100_000).iter().map(|b| char::from(b'a' + (b ^ i.to_le_bytes()[0]) % 26)).collect();
        n.handle.execute(Command::Chat { body, quote: None, channel: ChannelRef::Group }).await.expect("chat");
    }
    let mut ws = upgrade(n.port, slow(narrow(n.port).await)).await;
    welcomed(&mut ws, NEW_HELLO).await;
    ask(&mut ws, 1, read_chat()).await;
    let mut rotated = false;
    let closed = loop {
        match ws.next().await.expect("open").expect("clean") {
            Message::Binary(b) => {
                assert_eq!(b[16] & 1, 0, "the reply arrived whole after its key died");
                if !std::mem::replace(&mut rotated, true) {
                    let patch = Command::PatchSettings { patch: serde_json::json!({ "mcp_token": "rotated" }) };
                    n.handle.execute(patch).await.expect("rotates");
                }
            }
            Message::Close(Some(close)) => break u16::from(close.code),
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(closed, 4401);
}

/// A browser may keep a bundle file but must ask each time; an unchanged
/// file is answered without its body. The page itself is never kept.
#[tokio::test]
async fn a_kept_file_is_revalidated_not_sent_again() {
    let n = node().await;
    let (status, head, body) = get(n.port, "/molt_web.js", &["Accept-Encoding: br"]).await;
    assert_eq!(status, 200);
    assert_eq!(header(&head, "cache-control"), Some("no-cache"));
    let tag = header(&head, "etag").expect("a validator").to_string();
    assert!(tag.starts_with('"') && tag.ends_with('"') && tag.len() > 20, "{tag}");

    // the same request with the validator: no body
    let (status, head, again) = get(n.port, "/molt_web.js", &["Accept-Encoding: br", &format!("If-None-Match: {tag}")]).await;
    assert_eq!((status, again.len()), (304, 0));
    assert_eq!(header(&head, "etag"), Some(tag.as_str()));
    assert_eq!(header(&head, "cache-control"), Some("no-cache"));
    assert_eq!(header(&head, "vary"), Some("accept-encoding"));
    // in a list, and as a weak validator
    let (status, _, _) = get(n.port, "/molt_web.js", &["Accept-Encoding: br", &format!("If-None-Match: \"x\", W/{tag}")]).await;
    assert_eq!(status, 304);

    // another coding is another body: its own validator, sent whole
    let (status, head, plain) = get(n.port, "/molt_web.js", &[&format!("If-None-Match: {tag}")]).await;
    assert_eq!(status, 200);
    assert_ne!(header(&head, "etag"), Some(tag.as_str()));
    assert_ne!(plain, body);
    // a stale validator gets the file
    let (status, _, _) = get(n.port, "/molt_web.js", &["Accept-Encoding: br", "If-None-Match: \"stale\""]).await;
    assert_eq!(status, 200);

    // the page is fetched fresh every time
    for path in ["/", "/index.html"] {
        let (status, head, _) = get(n.port, path, &[]).await;
        assert_eq!(status, 200);
        assert_eq!(header(&head, "cache-control"), Some("no-store"), "{path}");
        assert_eq!(header(&head, "etag"), None, "{path}");
    }
}

/// A window re-reads a whole surface after every change. The second
/// answer to the same command crosses as what changed, not as megabytes
/// (docs/web_gui.md §5.7), and it is the reply the engine gives.
#[tokio::test]
async fn the_shipped_client_gets_a_repeated_read_as_what_changed() {
    let n = node().await;
    let read = fill_chat(&n.handle, 3_000_000).await;
    let (handle, seen) = client(&n).await;
    let crossed = || seen[0].load(Ordering::SeqCst) + seen[1].load(Ordering::SeqCst);
    let state = |r: Result<Reply, molt_core::MoltError>| serde_json::to_value(r.expect("a state")).expect("json");

    let first = state(handle.execute(read.clone()).await);
    assert_eq!(first, direct(&n.handle).await);
    let whole = crossed();
    assert!(whole > 100_000, "{whole} bytes for the first read");

    // nothing changed: next to nothing crosses
    assert_eq!(state(handle.execute(read.clone()).await), first);
    let same = crossed() - whole;
    assert!(same < 2_000, "{same} bytes for an unchanged read");

    // one line more: about that line crosses, and the reply is the engine's
    let body = "one more line".to_string();
    n.handle.execute(Command::Chat { body, quote: None, channel: ChannelRef::Group }).await.expect("chat");
    let before = crossed();
    let third = state(handle.execute(read.clone()).await);
    assert_eq!(third, direct(&n.handle).await);
    assert_ne!(third, first);
    // the events of that line cross too
    let changed = crossed() - before;
    assert!(changed < 20_000, "{changed} bytes for one more line");

    // and again on top of a patched reply
    assert_eq!(state(handle.execute(read).await), third);
}

/// What the door sends on a raw socket: a patch only to a request that
/// names a reply this session was sent, and a whole reply otherwise.
#[tokio::test]
async fn a_patch_answers_only_a_request_that_names_a_held_reply() {
    let n = node().await;
    let read = fill_chat(&n.handle, 300_000).await;
    let mut ws = open(&n).await;
    // the welcome says the door patches
    ws.send(Message::text(OLD_HELLO)).await.expect("hello");
    let Message::Text(w) = ws.next().await.expect("open").expect("clean") else { panic!("a welcome") };
    assert!(matches!(serde_json::from_str(w.as_str()), Ok(ServerFrame::Welcome { caps: Caps { patch: true, .. }, .. })));
    let say = |id: u64, like: Option<u64>| {
        let frame = ClientFrame::Cmd { id, cmd: Box::new(read.clone()), like };
        Message::text(serde_json::to_string(&frame).expect("json"))
    };
    let frame_of = async |ws: &mut WebSocketStream<TcpStream>, m: Message| -> ServerFrame {
        ws.send(m).await.expect("send");
        loop {
            let Message::Text(t) = ws.next().await.expect("open").expect("clean") else { panic!("a text frame") };
            match serde_json::from_str(t.as_str()).expect("a frame") {
                ServerFrame::Event(_) | ServerFrame::Lag => {}
                frame => return frame,
            }
        }
    };
    // a client that names nothing gets whole replies, as ever
    assert!(matches!(frame_of(&mut ws, say(1, None)).await, ServerFrame::Reply { id: 1, .. }));
    assert!(matches!(frame_of(&mut ws, say(2, None)).await, ServerFrame::Reply { id: 2, .. }));
    // a reply this session never sent is not a base
    assert!(matches!(frame_of(&mut ws, say(3, Some(77))).await, ServerFrame::Reply { id: 3, .. }));
    // a held one is, once
    let ServerFrame::Patch { id: 4, base: 2, delta } = frame_of(&mut ws, say(4, Some(2))).await else {
        panic!("a patch on reply 2")
    };
    assert!(delta.len() < 1_000, "{} bytes of delta for an unchanged read", delta.len());
    assert!(matches!(frame_of(&mut ws, say(5, Some(2))).await, ServerFrame::Reply { id: 5, .. }), "spent");
}
