// SPDX-License-Identifier: GPL-3.0-or-later
//! The listener: the public bundle as static files and the upgrade to the
//! WebSocket (`docs/web_gui.md` §5.3). HTTP is hyper's, the WebSocket
//! tungstenite's; the glue between them is the accept key and nothing more.
//! The bundle is served in the coding a request offers, and every write
//! on a connection is under a stall timeout, never a total (§5.6).

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{self, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use molt_core::wire::Caps;
use molt_engine::WalletHandle;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use crate::gate::Gate;
use crate::origin::{loopback_host, origin_matches};
use crate::{pack, session};

/// Open WebSocket sessions at once.
const SESSIONS: usize = 64;
/// Open TCP connections at once (static requests are short-lived and must
/// not be able to starve the sessions, which hold their own slots).
const CONNECTIONS: usize = 256;
/// SO_SNDBUF of an accepted socket (Linux accounts twice the value). The
/// stall timer's grain is this buffer: a blocked write is retried when
/// the kernel reports the socket writable again, which Linux does once
/// about a third of the buffer is free. Left to autotuning (megabytes on
/// loopback) that third is more than a slow tunnel takes in `stall`.
/// Loopback has no round trip to cover, so a small buffer costs nothing;
/// it also bounds the kernel memory one unread connection pins. Measured
/// with a 500 ms stall: a reader taking 50 KiB per stall is served, one
/// taking 25 KiB is not (`docs/web_gui.md` §5.6).
const SEND_BUFFER: usize = 32 * 1024;
/// One control frame; also the ceiling for a binary frame.
pub(crate) const MAX_FRAME: usize = 1 << 20;

/// The bundle: a fixed list, never a directory walk. The page's script
/// and style are files of their own because the CSP below admits nothing
/// inline.
const BUNDLE: [(&str, &str, &str); 6] = [
    ("/", "index.html", "text/html; charset=utf-8"),
    ("/index.html", "index.html", "text/html; charset=utf-8"),
    ("/page.js", "page.js", "text/javascript; charset=utf-8"),
    ("/page.css", "page.css", "text/css; charset=utf-8"),
    ("/molt_web.js", "molt_web.js", "text/javascript; charset=utf-8"),
    ("/molt_web_bg.wasm", "molt_web_bg.wasm", "application/wasm"),
];

/// The files a bundle directory must hold, each once.
pub const BUNDLE_FILES: [&str; 5] = ["index.html", "page.js", "page.css", "molt_web.js", "molt_web_bg.wasm"];

const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; frame-ancestors 'none'";

/// What a listener serves.
#[derive(Debug, Clone)]
pub struct WebDoor {
    /// The server's version, told to the client in `welcome`.
    pub build: String,
    /// Process facts for `welcome`.
    pub caps: Caps,
    /// The directory holding the bundle.
    pub dir: PathBuf,
    /// How long a socket may stay without a valid `hello`.
    pub pre_auth: Duration,
    /// How long a connection may take next to nothing of what is being
    /// written to it - a file's body, a frame - before it is dropped. A
    /// stall, not a total: a client that frees a third of the door's
    /// small send buffer within it is never cut off (`docs/web_gui.md`
    /// §5.6).
    pub stall: Duration,
}

struct Shared {
    handle: WalletHandle,
    door: WebDoor,
    gate: Gate,
    sessions: Arc<Semaphore>,
    /// The bundle by file name, read and encoded once at start.
    bundle: Vec<Served>,
}

/// One bundle file as it is served: itself, and each content coding that
/// made it smaller.
struct Served {
    name: &'static str,
    plain: Bytes,
    gzip: Option<Bytes>,
    brotli: Option<Bytes>,
    /// A hash of the file: the root of each body's `ETag`.
    tag: String,
}

impl Served {
    /// Encode `plain` both ways, side by side (the module takes about a
    /// second each). A coding that saves nothing, or failed, is not kept.
    fn new(name: &'static str, plain: Vec<u8>) -> Self {
        let (gzip, brotli) = std::thread::scope(|scope| {
            let gzip = scope.spawn(|| pack::gzip(&plain));
            let brotli = pack::brotli(&plain);
            (gzip.join().ok().and_then(Result::ok), brotli.ok())
        });
        let smaller = |coded: Option<Vec<u8>>| coded.filter(|c| c.len() < plain.len()).map(Bytes::from);
        let (gzip, brotli) = (smaller(gzip), smaller(brotli));
        tracing::debug!(
            file = name,
            bytes = plain.len(),
            gzip = gzip.as_ref().map(Bytes::len),
            br = brotli.as_ref().map(Bytes::len),
            "web door: bundle file"
        );
        use sha2::Digest as _;
        let tag = hex::encode(&sha2::Sha256::digest(&plain)[..16]);
        Served { name, plain: Bytes::from(plain), gzip, brotli, tag }
    }

    /// The body for a request offering `offer`, and its
    /// `Content-Encoding`: the smallest the request takes. That is brotli
    /// for the module, but not for every text.
    fn body(&self, offer: Offer) -> (&Bytes, Option<&'static str>) {
        let brotli = self.brotli.as_ref().filter(|_| offer.brotli).map(|body| (body, Some("br")));
        let gzip = self.gzip.as_ref().filter(|_| offer.gzip).map(|body| (body, Some("gzip")));
        // either was only kept for being smaller than the file itself
        [brotli, gzip].into_iter().flatten().min_by_key(|(body, _)| body.len()).unwrap_or((&self.plain, None))
    }

    /// The validator of one body: each coding is its own representation.
    fn etag(&self, coding: Option<&str>) -> String {
        match coding {
            Some(coding) => format!("\"{}-{coding}\"", self.tag),
            None => format!("\"{}\"", self.tag),
        }
    }
}

/// Whether an `If-None-Match` value names `etag` (RFC 9110 §13.1.2: a
/// list, compared weakly; the door's own tags hold no comma).
fn names_etag(value: &str, etag: &str) -> bool {
    value.split(',').map(str::trim).any(|one| one == "*" || one.strip_prefix("W/").unwrap_or(one) == etag)
}

#[cfg(test)]
mod etag_tests {
    use super::names_etag;

    #[test]
    fn a_validator_is_found_in_a_list_weak_or_strong() {
        assert!(names_etag("\"a\"", "\"a\""));
        assert!(names_etag("\"x\", W/\"a\"", "\"a\""));
        assert!(names_etag("*", "\"a\""));
        assert!(!names_etag("\"ab\"", "\"a\""));
        assert!(!names_etag("a", "\"a\""));
        assert!(!names_etag("", "\"a\""));
    }
}

/// The content codings a request takes, of the two the door has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Offer {
    gzip: bool,
    brotli: bool,
}

impl Offer {
    /// Read a request's `Accept-Encoding` lines (RFC 9110 §12.5.3): a
    /// comma list of codings, each with an optional `;q=` weight. A
    /// coding is offered when it is named with a weight above zero; the
    /// weights do not rank (the smaller body wins). `*`
    /// is not read as an offer, and a weight that does not parse offers
    /// nothing: a misread here costs bytes, never correctness.
    fn read<'a>(lines: impl Iterator<Item = &'a str>) -> Self {
        let mut offer = Offer::default();
        for item in lines.flat_map(|line| line.split(',')) {
            let mut parts = item.split(';');
            let coding = parts.next().unwrap_or_default().trim();
            let wanted = parts.all(|param| match param.split_once('=') {
                Some((name, weight)) if name.trim().eq_ignore_ascii_case("q") => {
                    weight.trim().parse::<f32>().is_ok_and(|q| q > 0.0 && q <= 1.0)
                }
                _ => true,
            });
            if coding.eq_ignore_ascii_case("gzip") {
                offer.gzip = wanted;
            } else if coding.eq_ignore_ascii_case("br") {
                offer.brotli = wanted;
            }
        }
        offer
    }
}

/// Read the bundle once, refusing anything that is not ours alone.
///
/// The seat token is typed into this page, so the bundle is trusted like
/// the binary (`docs/web_gui.md` §5.4): the directory and every file must
/// be a plain file or directory - no symlink - owned by this user or
/// root and writable by nobody else. Read once, so a later swap on disk
/// changes nothing a running door serves; encoded once, here, so no
/// request pays for it.
fn load_bundle(dir: &std::path::Path) -> std::io::Result<Vec<Served>> {
    use std::os::unix::fs::MetadataExt;
    let me = rustix::process::geteuid().as_raw();
    let ours = |path: &std::path::Path, want_dir: bool| -> std::io::Result<()> {
        let meta = std::fs::symlink_metadata(path)?;
        let kind_ok = if want_dir { meta.is_dir() } else { meta.is_file() };
        if !kind_ok || (meta.uid() != me && meta.uid() != 0) || meta.mode() & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{}: not a private plain {}", path.display(), if want_dir { "directory" } else { "file" }),
            ));
        }
        Ok(())
    };
    ours(dir, true)?;
    BUNDLE_FILES
        .into_iter()
        .map(|file| {
            let path = dir.join(file);
            ours(&path, false)?;
            Ok(Served::new(file, std::fs::read(&path)?))
        })
        .collect()
}

/// Bind `127.0.0.1:port` and serve the door. The bundle is checked BEFORE
/// the port opens: an untrusted directory binds nothing. Loopback is not
/// a parameter (`docs/web_gui.md` D2).
pub async fn serve_loopback(port: u16, handle: WalletHandle, door: WebDoor) -> std::io::Result<()> {
    let bundle = load(&door).await?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    tracing::info!(port, "web door on loopback");
    serve_with(listener, handle, door, bundle).await
}

/// Serve the door on `listener` until the task is dropped. Fails at once
/// when the bundle directory is not trustworthy (see [`load_bundle`]).
pub async fn serve(listener: TcpListener, handle: WalletHandle, door: WebDoor) -> std::io::Result<()> {
    let bundle = load(&door).await?;
    serve_with(listener, handle, door, bundle).await
}

/// [`load_bundle`] off the runtime's threads: encoding the module takes
/// about a second.
async fn load(door: &WebDoor) -> std::io::Result<Vec<Served>> {
    let dir = door.dir.clone();
    tokio::task::spawn_blocking(move || load_bundle(&dir)).await.map_err(std::io::Error::other)?
}

async fn serve_with(
    listener: TcpListener,
    handle: WalletHandle,
    door: WebDoor,
    bundle: Vec<Served>,
) -> std::io::Result<()> {
    let header_timeout = door.pre_auth;
    let stall = door.stall;
    let sessions = Arc::new(Semaphore::new(SESSIONS));
    let shared = Arc::new(Shared { handle, door, gate: Gate::new(), sessions, bundle });
    let connections = Arc::new(Semaphore::new(CONNECTIONS));
    loop {
        let (sock, peer) = listener.accept().await?;
        let Ok(slot) = connections.clone().try_acquire_owned() else {
            tracing::debug!(%peer, "web door: connection limit");
            continue;
        };
        // without the small buffer the stall rule does not hold: no service
        if let Err(e) = socket2::SockRef::from(&sock).set_send_buffer_size(SEND_BUFFER) {
            tracing::debug!(%peer, error = %e, "web door: send buffer");
            continue;
        }
        let shared = shared.clone();
        // One timer for everything this connection is ever sent, a file's
        // body and (hyper's upgrade hands the same socket on) every frame
        // of a session: it starts when a write finds the send buffer full
        // and restarts when the kernel takes more, that is once the peer
        // has freed about a third of `SEND_BUFFER`. A client that will
        // not read loses its slot; one that reads slowly keeps it.
        let mut sock = tokio_io_timeout::TimeoutWriter::new(sock);
        sock.set_timeout(Some(stall));
        let sock = Box::pin(sock);
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let shared = shared.clone();
                async move { Ok::<_, Infallible>(route(&shared, req).await) }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(header_timeout)
                .serve_connection(TokioIo::new(sock), svc)
                .with_upgrades();
            // no total: a request that never comes ends at the header
            // timeout, an answer nobody reads at the stall timeout
            if let Err(e) = conn.await {
                tracing::debug!(%peer, error = %e, "web door: connection ended");
            }
            drop(slot);
        });
    }
}

fn plain(status: StatusCode) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = status;
    r
}

fn header_str(req: &Request<Incoming>, name: header::HeaderName) -> Option<&str> {
    req.headers().get(name).and_then(|v| v.to_str().ok())
}

async fn route(shared: &Arc<Shared>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    // a name that is not loopback gets nothing: this is the rebinding guard
    let Some(host) = header_str(&req, header::HOST).and_then(loopback_host) else {
        return plain(StatusCode::FORBIDDEN);
    };
    let origin = header_str(&req, header::ORIGIN);
    if req.method() != Method::GET {
        return plain(StatusCode::METHOD_NOT_ALLOWED);
    }
    // no seat key, no door: nothing is served, not even the page
    if !seat_key_set(&shared.handle).await {
        return plain(StatusCode::SERVICE_UNAVAILABLE);
    }
    if req.uri().path() == "/ws" {
        if !origin_matches(origin, &host) {
            return plain(StatusCode::FORBIDDEN);
        }
        return upgrade(shared, req);
    }
    // a navigation sends no Origin; one that is sent must be ours. A page
    // of another site must not even learn that a node runs here.
    let cross_site = header_str(&req, header::HeaderName::from_static("sec-fetch-site")) == Some("cross-site");
    if cross_site || (origin.is_some() && !origin_matches(origin, &host)) {
        return plain(StatusCode::FORBIDDEN);
    }
    // matched on the raw request target: no decoding, no normalizing
    let target = req.uri().path_and_query().map_or("", http::uri::PathAndQuery::as_str);
    let Some((_, file, mime)) = BUNDLE.iter().find(|(path, _, _)| *path == target) else {
        return plain(StatusCode::NOT_FOUND);
    };
    let Some(served) = shared.bundle.iter().find(|served| served.name == *file) else {
        return plain(StatusCode::NOT_FOUND);
    };
    let offer = Offer::read(req.headers().get_all(header::ACCEPT_ENCODING).iter().filter_map(|v| v.to_str().ok()));
    let (body, coding) = served.body(offer);
    // the page is fetched fresh every time. Its files may be kept, but a
    // browser asks before each use, so a kept one is never a stale one
    let etag = (*file != "index.html").then(|| served.etag(coding));
    let kept = etag.as_deref().is_some_and(|etag| {
        req.headers().get_all(header::IF_NONE_MATCH).iter().filter_map(|v| v.to_str().ok()).any(|v| names_etag(v, etag))
    });
    let mut r = Response::new(Full::new(if kept { Bytes::new() } else { body.clone() }));
    if kept {
        *r.status_mut() = StatusCode::NOT_MODIFIED;
    }
    let h = r.headers_mut();
    if let Some(coding) = coding.filter(|_| !kept) {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
    }
    // the answer depends on the request, whichever body went out
    h.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    match etag.and_then(|etag| HeaderValue::from_str(&etag).ok()) {
        Some(etag) => {
            h.insert(header::ETAG, etag);
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        }
        None => {
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
    }
    h.insert(header::HeaderName::from_static("cross-origin-resource-policy"), HeaderValue::from_static("same-origin"));
    r
}

async fn seat_key_set(handle: &WalletHandle) -> bool {
    match handle.execute(molt_core::Command::ReadSession).await {
        Ok(molt_core::Reply::Session(s)) => !s.settings.mcp_token.is_empty(),
        _ => false,
    }
}

/// RFC 6455 §4.2.1, the server's side: the request must ask for exactly
/// this upgrade; the answer is the accept key.
fn upgrade(shared: &Arc<Shared>, mut req: Request<Incoming>) -> Response<Full<Bytes>> {
    let has = |name: header::HeaderName, token: &str| {
        header_str(&req, name).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    };
    let key = req.headers().get(header::SEC_WEBSOCKET_KEY).map(|k| derive_accept_key(k.as_bytes()));
    let (true, true, true, Some(accept)) = (
        has(header::CONNECTION, "upgrade"),
        has(header::UPGRADE, "websocket"),
        has(header::SEC_WEBSOCKET_VERSION, "13"),
        key,
    ) else {
        return plain(StatusCode::BAD_REQUEST);
    };
    let Ok(accept) = HeaderValue::from_str(&accept) else {
        return plain(StatusCode::BAD_REQUEST);
    };
    let Ok(slot) = shared.sessions.clone().try_acquire_owned() else {
        return plain(StatusCode::SERVICE_UNAVAILABLE);
    };
    let shared = shared.clone();
    let upgraded = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        match upgraded.await {
            Ok(io) => {
                // explicit bounds: tungstenite's defaults are 64 MiB a message
                let config =
                    WebSocketConfig::default().max_message_size(Some(MAX_FRAME)).max_frame_size(Some(MAX_FRAME));
                let ws = WebSocketStream::from_raw_socket(TokioIo::new(io), Role::Server, Some(config)).await;
                session::run(ws, &shared.handle, &shared.gate, &shared.door).await;
            }
            Err(e) => tracing::debug!(error = %e, "web door: upgrade failed"),
        }
        drop(slot);
    });
    let mut r = plain(StatusCode::SWITCHING_PROTOCOLS);
    let h = r.headers_mut();
    h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offer_is_what_the_header_names_with_a_weight_above_zero() {
        let read = |lines: &[&str]| {
            let offer = Offer::read(lines.iter().copied());
            (offer.gzip, offer.brotli)
        };
        assert_eq!(read(&[]), (false, false));
        assert_eq!(read(&["gzip, deflate"]), (true, false));
        assert_eq!(read(&["gzip, deflate, br, zstd"]), (true, true));
        assert_eq!(read(&[" BR ;Q=0.1 "]), (false, true));
        assert_eq!(read(&["gzip;q=0, br;q=0.0"]), (false, false));
        assert_eq!(read(&["gzip", "br"]), (true, true), "two lines are one list");
        assert_eq!(read(&["*", "identity", "x-gzip", "brotli", ""]), (false, false));
        assert_eq!(read(&["gzip;q=1.5, br;q=-1", "gzip;q=", "br;q=nan"]), (false, false), "no weight, no offer");
        assert_eq!(read(&["gzip;q=0, gzip"]), (true, false), "the last word on a coding stands");
        assert_eq!(read(&["gzip;x=0;q=1"]), (true, false), "only q weighs");
    }

    /// A coding is served only where it was offered AND kept.
    #[test]
    fn a_body_is_the_smallest_coding_offered() {
        let text = Served::new("index.html", "one two one two ".repeat(500).into_bytes());
        let (gzip, brotli) = (text.gzip.clone().expect("kept"), text.brotli.clone().expect("kept"));
        assert!(brotli.len() < text.plain.len() && gzip.len() < text.plain.len());
        let offer = |gzip, brotli| Offer { gzip, brotli };
        assert_eq!(text.body(offer(false, false)), (&text.plain, None));
        assert_eq!(text.body(offer(true, false)), (&gzip, Some("gzip")));
        assert_eq!(text.body(offer(false, true)), (&brotli, Some("br")));
        let (both, _) = text.body(offer(true, true));
        assert_eq!(both.len(), gzip.len().min(brotli.len()), "the smaller of the two");

        // whichever of the two is larger loses, by construction
        let swapped = Served { name: "x", plain: text.plain.clone(), gzip: Some(brotli.clone()), brotli: Some(text.plain.slice(..1)), tag: String::new() };
        assert_eq!(swapped.body(offer(true, true)).1, Some("br"));
        let swapped = Served { gzip: Some(text.plain.slice(..1)), brotli: Some(gzip.clone()), ..swapped };
        assert_eq!(swapped.body(offer(true, true)).1, Some("gzip"));

        let tiny = Served::new("page.css", b"x".to_vec());
        assert_eq!((&tiny.gzip, &tiny.brotli), (&None, &None), "a coding that saves nothing is not kept");
        assert_eq!(tiny.body(offer(true, true)), (&tiny.plain, None));
        let empty = Served::new("page.css", Vec::new());
        assert_eq!(empty.body(offer(true, true)), (&empty.plain, None));
    }

    /// Every route serves a file the bundle check read, and every file
    /// read has a route: the two lists cannot drift.
    #[test]
    fn every_route_has_its_file_and_every_file_a_route() {
        for (route, file, _) in BUNDLE {
            assert!(BUNDLE_FILES.contains(&file), "{route} serves {file}, which is never loaded");
            assert!(route == "/" || route.strip_prefix('/') == Some(file), "{route} is not {file}'s own name");
        }
        for file in BUNDLE_FILES {
            assert!(BUNDLE.iter().any(|(_, served, _)| *served == file), "{file} is loaded and never served");
            assert_eq!(BUNDLE_FILES.iter().filter(|f| **f == file).count(), 1, "{file} is listed twice");
        }
    }
}
