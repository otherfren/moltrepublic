// SPDX-License-Identifier: GPL-3.0-or-later
//! The listener: the public bundle as static files and the upgrade to the
//! WebSocket (`docs/web_gui.md` §5.3). HTTP is hyper's, the WebSocket
//! tungstenite's; the glue between them is the accept key and nothing more.

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
use crate::session;

/// Open WebSocket sessions at once.
const SESSIONS: usize = 64;
/// Open TCP connections at once (static requests are short-lived and must
/// not be able to starve the sessions, which hold their own slots).
const CONNECTIONS: usize = 256;
/// One control frame; also the ceiling for a binary frame.
pub(crate) const MAX_FRAME: usize = 1 << 20;

/// A plain HTTP exchange, whole: a client that will not read its answer
/// does not keep its slot. An upgraded connection has left by then.
const HTTP_EXCHANGE: Duration = Duration::from_secs(60);

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
}

struct Shared {
    handle: WalletHandle,
    door: WebDoor,
    gate: Gate,
    sessions: Arc<Semaphore>,
    /// The bundle's bytes by file name, read once at start.
    bundle: Vec<(&'static str, Bytes)>,
}

/// Read the bundle once, refusing anything that is not ours alone.
///
/// The seat token is typed into this page, so the bundle is trusted like
/// the binary (`docs/web_gui.md` §5.4): the directory and every file must
/// be a plain file or directory - no symlink - owned by this user or
/// root and writable by nobody else. Read once, so a later swap on disk
/// changes nothing a running door serves.
fn load_bundle(dir: &std::path::Path) -> std::io::Result<Vec<(&'static str, Bytes)>> {
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
            Ok((file, Bytes::from(std::fs::read(&path)?)))
        })
        .collect()
}

/// Bind `127.0.0.1:port` and serve the door. The bundle is checked BEFORE
/// the port opens: an untrusted directory binds nothing. Loopback is not
/// a parameter (`docs/web_gui.md` D2).
pub async fn serve_loopback(port: u16, handle: WalletHandle, door: WebDoor) -> std::io::Result<()> {
    let bundle = load_bundle(&door.dir)?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    tracing::info!(port, "web door on loopback");
    serve_with(listener, handle, door, bundle).await
}

/// Serve the door on `listener` until the task is dropped. Fails at once
/// when the bundle directory is not trustworthy (see [`load_bundle`]).
pub async fn serve(listener: TcpListener, handle: WalletHandle, door: WebDoor) -> std::io::Result<()> {
    let bundle = load_bundle(&door.dir)?;
    serve_with(listener, handle, door, bundle).await
}

async fn serve_with(
    listener: TcpListener,
    handle: WalletHandle,
    door: WebDoor,
    bundle: Vec<(&'static str, Bytes)>,
) -> std::io::Result<()> {
    let header_timeout = door.pre_auth;
    let sessions = Arc::new(Semaphore::new(SESSIONS));
    let shared = Arc::new(Shared { handle, door, gate: Gate::new(), sessions, bundle });
    let connections = Arc::new(Semaphore::new(CONNECTIONS));
    loop {
        let (sock, peer) = listener.accept().await?;
        let Ok(slot) = connections.clone().try_acquire_owned() else {
            tracing::debug!(%peer, "web door: connection limit");
            continue;
        };
        let shared = shared.clone();
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
            match tokio::time::timeout(HTTP_EXCHANGE, conn).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::debug!(%peer, error = %e, "web door: connection ended"),
                Err(_) => tracing::debug!(%peer, "web door: exchange timed out"),
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
    let Some((_, body)) = shared.bundle.iter().find(|(name, _)| name == file) else {
        return plain(StatusCode::NOT_FOUND);
    };
    let mut r = Response::new(Full::new(body.clone()));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
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
