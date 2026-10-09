// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse's daemon (`docs/chain/wallet_treasury_design.md` §7):
//! monero-daemon-rpc's [`HttpTransport`] over the crate's own HTTP client
//! ([`crate::s3::http`]) and the fail-closed [`Dialer`]. An onion daemon
//! rides Tor; a local or clearnet one is dialed only when it was confirmed
//! and non-onion dialing is on, like a relay. One request per connection;
//! monerod's `--rpc-login` is answered with HTTP digest (`http-auth`).

use std::sync::{Arc, Mutex};

use molt_core::relay::RelayKind;
use monero_daemon_rpc::prelude::{InterfaceError, MoneroDaemon, ProvidesBlockchainMeta};
use monero_daemon_rpc::HttpTransport;
use zeroize::Zeroizing;

use crate::dial::Dialer;
use crate::s3::http;

/// monerod's own response queue bound; also the cap when a caller asks for
/// no per-call limit.
const MAX_DAEMON_RESPONSE: usize = 100 * 1024 * 1024;

/// Why the daemon could not be asked. The display is the one line a user
/// sees.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DaemonError {
    /// No daemon configured.
    #[error("no daemon")]
    None,
    /// The URL is not a daemon URL.
    #[error("daemon: bad url")]
    BadUrl,
    /// A non-onion daemon the operator has not confirmed.
    #[error("daemon: not confirmed")]
    Unconfirmed,
    /// A non-onion daemon while non-onion dialing is off.
    #[error("daemon: non-onion dialing off")]
    NonOnionOff,
    /// The daemon wants a login this node lacks or it refused ours.
    #[error("daemon: login refused")]
    Login,
    /// Dial, HTTP or RPC failure.
    #[error("daemon: {0}")]
    Rpc(String),
}

/// A daemon reachable under this node's policy.
#[derive(Clone)]
pub struct DaemonTransport {
    inner: Arc<Inner>,
}

struct Inner {
    host: String,
    port: u16,
    tls: bool,
    host_header: String,
    base_path: String,
    dialer: Dialer,
    login: Option<(Zeroizing<String>, Zeroizing<String>)>,
    /// The last digest challenge, answered again with the next nonce count.
    digest: Mutex<Option<http_auth::PasswordClient>>,
}

impl std::fmt::Debug for DaemonTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonTransport")
            .field("host", &self.inner.host)
            .field("port", &self.inner.port)
            .field("via", &self.inner.dialer.route())
            .finish_non_exhaustive()
    }
}

impl DaemonTransport {
    /// The policy gate: `url` is classified by the relay host rule, an onion
    /// daemon dials through `base` (Tor, or refused when Tor is off), a local
    /// one directly, a clearnet one through `base` - the last two only when
    /// `confirmed` and `clearnet_enabled`. `login` is `user:password`.
    ///
    /// # Errors
    /// The refusal, before any I/O.
    pub fn new(
        url: &str,
        login: &str,
        confirmed: bool,
        clearnet_enabled: bool,
        base: &Dialer,
    ) -> Result<Self, DaemonError> {
        if url.trim().is_empty() {
            return Err(DaemonError::None);
        }
        let (canon, kind) = molt_core::relay::daemon_kind(url).map_err(|_| DaemonError::BadUrl)?;
        if kind != RelayKind::Onion {
            if !confirmed {
                return Err(DaemonError::Unconfirmed);
            }
            if !clearnet_enabled {
                return Err(DaemonError::NonOnionOff);
            }
        }
        let dialer = match kind {
            RelayKind::Local => Dialer::Direct,
            RelayKind::Onion | RelayKind::Clearnet => base.isolated("wallet-daemon"),
        };
        let parsed = url::Url::parse(&canon).map_err(|_| DaemonError::BadUrl)?;
        let host = parsed
            .host_str()
            .ok_or(DaemonError::BadUrl)?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = parsed.port_or_known_default().ok_or(DaemonError::BadUrl)?;
        let host_header = match parsed.port() {
            Some(p) => format!("{}:{p}", parsed.host_str().unwrap_or_default()),
            None => parsed.host_str().unwrap_or_default().to_string(),
        };
        let login = (!login.is_empty()).then(|| {
            let (u, p) = login.split_once(':').unwrap_or((login, ""));
            (Zeroizing::new(u.to_string()), Zeroizing::new(p.to_string()))
        });
        Ok(Self {
            inner: Arc::new(Inner {
                host,
                port,
                tls: parsed.scheme() == "https",
                host_header,
                base_path: parsed.path().trim_end_matches('/').to_string(),
                dialer,
                login,
                digest: Mutex::new(None),
            }),
        })
    }

    async fn post_inner(
        &self,
        route: &str,
        body: &[u8],
        limit: Option<usize>,
    ) -> Result<Vec<u8>, DaemonError> {
        let path = format!("{}/{route}", self.inner.base_path);
        let max = limit.unwrap_or(MAX_DAEMON_RESPONSE).min(MAX_DAEMON_RESPONSE);
        let mut auth = self.answer(None, &path, body)?;
        for fresh in [false, true] {
            let resp = self.exchange(&path, body, auth.as_deref(), max).await?;
            match resp.status {
                200 => return Ok(resp.body),
                401 if !fresh => {
                    let challenges: Vec<&str> = resp
                        .headers
                        .iter()
                        .filter(|(k, _)| k == "www-authenticate")
                        .map(|(_, v)| v.as_str())
                        .collect();
                    auth = self.answer(Some(&challenges.join(", ")), &path, body)?;
                    if auth.is_none() {
                        return Err(DaemonError::Login);
                    }
                }
                401 => return Err(DaemonError::Login),
                s => return Err(DaemonError::Rpc(format!("http {s}"))),
            }
        }
        Err(DaemonError::Login)
    }

    /// The `Authorization` value: from a fresh `challenge`, else the cached
    /// one; `None` without a login or a challenge.
    fn answer(&self, challenge: Option<&str>, path: &str, body: &[u8]) -> Result<Option<String>, DaemonError> {
        let Some((user, pass)) = &self.inner.login else {
            return Ok(None);
        };
        let mut slot = self.inner.digest.lock().map_err(|_| DaemonError::Login)?;
        if let Some(c) = challenge {
            *slot = Some(http_auth::PasswordClient::try_from(c).map_err(|_| DaemonError::Login)?);
        }
        let Some(client) = slot.as_mut() else {
            return Ok(None);
        };
        let params = http_auth::PasswordParams {
            username: user,
            password: pass,
            uri: path,
            method: "POST",
            body: Some(body),
        };
        client.respond(&params).map(Some).map_err(|_| DaemonError::Login)
    }

    async fn exchange(
        &self,
        path: &str,
        body: &[u8],
        auth: Option<&str>,
        max: usize,
    ) -> Result<http::HttpResponse, DaemonError> {
        let i = &self.inner;
        let mut stream = crate::relay_ws::dial_maybe_tls(&i.dialer, &i.host, i.port, i.tls)
            .await
            .map_err(|e| DaemonError::Rpc(e.to_string()))?;
        let kind = if path.ends_with(".bin") { "application/octet-stream" } else { "application/json" };
        let mut headers = vec![
            ("Host".to_string(), i.host_header.clone()),
            ("Content-Type".to_string(), kind.to_string()),
        ];
        if body.is_empty() {
            headers.push(("Content-Length".to_string(), "0".to_string()));
        }
        if let Some(a) = auth {
            headers.push(("Authorization".to_string(), a.to_string()));
        }
        http::roundtrip(&mut stream, "POST", path, &headers, body, max)
            .await
            .map_err(|e| DaemonError::Rpc(e.0))
    }
}

impl HttpTransport for DaemonTransport {
    fn post(
        &self,
        route: &str,
        body: Vec<u8>,
        response_size_limit: Option<usize>,
    ) -> impl Send + std::future::Future<Output = Result<Vec<u8>, InterfaceError>> {
        let this = self.clone();
        let route = route.to_string();
        async move {
            this.post_inner(&route, &body, response_size_limit)
                .await
                .map_err(|e| InterfaceError::InterfaceError(e.to_string()))
        }
    }
}

/// The daemon's latest block number.
///
/// # Errors
/// Dial, login or RPC failure, one line.
pub async fn daemon_height(transport: DaemonTransport) -> Result<u64, DaemonError> {
    let daemon = MoneroDaemon::new(transport).await.map_err(|e| unwrap_error(&e))?;
    let n = daemon.latest_block_number().await.map_err(|e| unwrap_error(&e))?;
    u64::try_from(n).map_err(|_| DaemonError::Rpc("height".to_string()))
}

/// The transport's own error comes back wrapped by the library.
fn unwrap_error(e: &InterfaceError) -> DaemonError {
    match e {
        InterfaceError::InterfaceError(s) if *s == DaemonError::Login.to_string() => DaemonError::Login,
        InterfaceError::InterfaceError(s) => {
            DaemonError::Rpc(s.strip_prefix("daemon: ").unwrap_or(s).to_string())
        }
        other => DaemonError::Rpc(other.to_string()),
    }
}

/// An in-process monerod double: `json_rpc` and `get_height`, optionally
/// behind a digest login. Test-only.
#[cfg(any(test, feature = "stub-daemon"))]
pub mod stub {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    use md5::{Digest, Md5};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const REALM: &str = "monero-rpc";
    const NONCE: &str = "6f3a";

    /// A running stub; dropped, it stops.
    pub struct StubDaemon {
        /// `http://127.0.0.1:<port>`.
        pub url: String,
        height: Arc<AtomicU64>,
        hits: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    /// How the stub answers.
    #[derive(Clone, Default)]
    pub struct StubConfig {
        /// `(user, password)` it demands.
        pub login: Option<(String, String)>,
        /// Extra bytes in the height answer.
        pub pad: usize,
    }

    impl Drop for StubDaemon {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl StubDaemon {
        /// A stub at `height` (the latest block number).
        pub async fn start(height: u64, config: StubConfig) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let url = format!("http://{}", listener.local_addr().expect("addr"));
            let height = Arc::new(AtomicU64::new(height));
            let hits = Arc::new(AtomicUsize::new(0));
            let (h, n) = (height.clone(), hits.clone());
            let task = tokio::spawn(async move {
                while let Ok((s, _)) = listener.accept().await {
                    let (h, n, c) = (h.clone(), n.clone(), config.clone());
                    tokio::spawn(async move {
                        let _ = serve(s, &h, &n, &c).await;
                    });
                }
            });
            Self { url, height, hits, task }
        }

        /// Move the chain tip.
        pub fn set_height(&self, height: u64) {
            self.height.store(height, Ordering::SeqCst);
        }

        /// Requests answered with 200.
        pub fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    fn md5_hex(s: &str) -> String {
        hex::encode(Md5::digest(s.as_bytes()))
    }

    fn param<'a>(header: &'a str, key: &str) -> Option<&'a str> {
        header.split(',').find_map(|kv| {
            let (k, v) = kv.trim().trim_start_matches("Digest ").split_once('=')?;
            (k.trim() == key).then(|| v.trim().trim_matches('"'))
        })
    }

    fn authorized(auth: Option<&str>, login: &(String, String), uri: &str) -> bool {
        let Some(a) = auth else { return false };
        let get = |k| param(a, k).unwrap_or_default();
        if get("username") != login.0 || get("uri") != uri || get("nonce") != NONCE {
            return false;
        }
        let ha1 = md5_hex(&format!("{}:{REALM}:{}", login.0, login.1));
        let ha2 = md5_hex(&format!("POST:{uri}"));
        let want = md5_hex(&format!("{ha1}:{NONCE}:{}:{}:{}:{ha2}", get("nc"), get("cnonce"), get("qop")));
        get("response") == want
    }

    async fn serve(
        mut s: TcpStream,
        height: &AtomicU64,
        hits: &AtomicUsize,
        config: &StubConfig,
    ) -> std::io::Result<()> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let (head, body_len) = loop {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let len = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                    .unwrap_or(0usize);
                break (head, end + 4 + len);
            }
        };
        while buf.len() < body_len {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();
        let auth = head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("authorization").then(|| v.trim())
        });
        if let Some(login) = &config.login {
            if !authorized(auth, login, &path) {
                let challenge = format!(
                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest qop=\"auth\",algorithm=MD5,realm=\"{REALM}\",nonce=\"{NONCE}\",stale=false\r\nContent-Length: 0\r\n\r\n"
                );
                return s.write_all(challenge.as_bytes()).await;
            }
        }
        let body = match path.as_str() {
            "/json_rpc" => "[]".to_string(),
            "/get_height" => format!(
                "{{\"height\":{},\"status\":\"OK\",\"pad\":\"{}\"}}",
                height.load(Ordering::SeqCst) + 1,
                "x".repeat(config.pad)
            ),
            _ => {
                return s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
            }
        };
        hits.fetch_add(1, Ordering::SeqCst);
        let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        s.write_all(resp.as_bytes()).await
    }
}

#[cfg(test)]
mod tests {
    use super::stub::{StubConfig, StubDaemon};
    use super::*;

    fn local(url: &str, login: &str) -> DaemonTransport {
        DaemonTransport::new(url, login, true, true, &Dialer::Direct).expect("allowed")
    }

    #[tokio::test]
    async fn the_height_probe_reads_a_stub_daemon() {
        let d = StubDaemon::start(3000, StubConfig::default()).await;
        assert_eq!(daemon_height(local(&d.url, "")).await, Ok(3000));
        d.set_height(3005);
        assert_eq!(daemon_height(local(&d.url, "")).await, Ok(3005));
    }

    #[tokio::test]
    async fn a_digest_login_answers_the_challenge() {
        let cfg = StubConfig { login: Some(("u".into(), "pw".into())), pad: 0 };
        let d = StubDaemon::start(42, cfg).await;
        assert_eq!(daemon_height(local(&d.url, "u:pw")).await, Ok(42));
        assert_eq!(daemon_height(local(&d.url, "u:nope")).await, Err(DaemonError::Login));
        assert_eq!(daemon_height(local(&d.url, "")).await, Err(DaemonError::Login));
    }

    /// The per-call limit holds: a padded height answer is refused.
    #[tokio::test]
    async fn an_oversized_answer_is_refused() {
        let d = StubDaemon::start(7, StubConfig { login: None, pad: 2 * 1024 * 1024 }).await;
        let got = daemon_height(local(&d.url, "")).await;
        assert!(matches!(&got, Err(DaemonError::Rpc(e)) if e.contains("exceeds")), "{got:?}");
    }

    /// The step-5 gap: a non-onion daemon needs both switches, refused before
    /// any dial; an onion daemon without Tor fails closed.
    #[tokio::test]
    async fn a_non_onion_daemon_needs_confirmation_and_clearnet() {
        let d = StubDaemon::start(1, StubConfig::default()).await;
        for (confirmed, clearnet, want) in [
            (false, true, DaemonError::Unconfirmed),
            (true, false, DaemonError::NonOnionOff),
            (false, false, DaemonError::Unconfirmed),
        ] {
            let got = DaemonTransport::new(&d.url, "", confirmed, clearnet, &Dialer::Direct).map(|_| ());
            assert_eq!(got, Err(want));
        }
        assert_eq!(d.hits(), 0);
        assert_eq!(
            DaemonTransport::new("https://u:p@node.example.org", "", true, true, &Dialer::Direct).map(|_| ()),
            Err(DaemonError::BadUrl)
        );
        assert_eq!(DaemonTransport::new("", "", true, true, &Dialer::Direct).map(|_| ()), Err(DaemonError::None));
        let onion = format!("http://{}.onion:18081", "a".repeat(56));
        let t = DaemonTransport::new(&onion, "", false, false, &Dialer::Direct).expect("an onion needs no consent");
        let got = daemon_height(t).await;
        assert!(matches!(&got, Err(DaemonError::Rpc(e)) if e.contains("Tor is off")), "{got:?}");
    }

    #[test]
    fn refusals_are_one_line() {
        assert_eq!(DaemonError::NonOnionOff.to_string(), "daemon: non-onion dialing off");
        assert_eq!(DaemonError::Rpc("http 500".into()).to_string(), "daemon: http 500");
    }
}
