// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse's daemon (`docs/chain/wallet_treasury_design.md` §7):
//! monero-daemon-rpc's [`HttpTransport`] over the crate's own HTTP client
//! ([`crate::s3::http`]) and the fail-closed [`Dialer`]. An onion daemon
//! rides Tor; a local or clearnet one is dialed only when it was confirmed
//! and non-onion dialing is on, like a relay. One request per connection;
//! monerod's `--rpc-login` is answered with HTTP digest (`http-auth`).

use std::sync::{Arc, Mutex};

use molt_core::relay::RelayKind;
use monero_daemon_rpc::prelude::{
    InterfaceError, MoneroDaemon, ProvidesBlockchainMeta, ProvidesScannableBlocks, ScannableBlock,
};
use monero_daemon_rpc::HttpTransport;
use zeroize::Zeroizing;

use crate::dial::Dialer;
use crate::s3::http;

/// monerod's own response queue bound; also the cap when a caller asks for
/// no per-call limit.
const MAX_DAEMON_RESPONSE: usize = 100 * 1024 * 1024;
/// `get_info` is a few hundred bytes.
const GET_INFO_MAX: usize = 64 * 1024;

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
    /// The daemon has not caught up with the network.
    #[error("daemon: syncing")]
    Syncing,
    /// Dial, HTTP or RPC failure.
    #[error("daemon: {0}")]
    Rpc(String),
    /// The daemon answered something that does not decode or check out.
    #[error("daemon fault: {0}")]
    Fault(String),
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
    if !synchronized(&transport).await? {
        return Err(DaemonError::Syncing);
    }
    let daemon = MoneroDaemon::new(transport).await.map_err(|e| unwrap_error(&e))?;
    let n = daemon.latest_block_number().await.map_err(|e| unwrap_error(&e))?;
    u64::try_from(n).map_err(|_| DaemonError::Rpc("height".to_string()))
}

/// Blocks `from..=to`, each building on the one before (one
/// `get_blocks.bin` per round trip, at most the blocks asked for).
///
/// # Errors
/// [`DaemonError::Fault`] when the answer does not decode or check out,
/// else the dial, login or RPC failure.
pub async fn scannable_blocks(transport: DaemonTransport, from: u64, to: u64) -> Result<Vec<ScannableBlock>, DaemonError> {
    let range = usize::try_from(from).map_err(|_| DaemonError::Rpc("height".to_string()))?
        ..=usize::try_from(to).map_err(|_| DaemonError::Rpc("height".to_string()))?;
    let daemon = MoneroDaemon::new(transport).await.map_err(|e| unwrap_error(&e))?;
    ProvidesScannableBlocks::contiguous_scannable_blocks(&daemon, range)
        .await
        .map_err(|e| unwrap_error(&e))
}

/// `get_info`'s `synchronized`; a daemon that omits it counts as caught up.
async fn synchronized(transport: &DaemonTransport) -> Result<bool, DaemonError> {
    const REQ: &str = r#"{"jsonrpc":"2.0","id":0,"method":"get_info"}"#;
    let body = transport.post_inner("json_rpc", REQ.as_bytes(), Some(GET_INFO_MAX)).await?;
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| DaemonError::Rpc("get_info".to_string()))?;
    Ok(v.pointer("/result/synchronized").and_then(serde_json::Value::as_bool).unwrap_or(true))
}

/// The transport's own error comes back wrapped by the library.
fn unwrap_error(e: &InterfaceError) -> DaemonError {
    match e {
        InterfaceError::InterfaceError(s) if *s == DaemonError::Login.to_string() => DaemonError::Login,
        InterfaceError::InterfaceError(s) => {
            DaemonError::Rpc(s.strip_prefix("daemon: ").unwrap_or(s).to_string())
        }
        InterfaceError::InvalidInterface(s) | InterfaceError::InternalError(s) => DaemonError::Fault(s.clone()),
    }
}

/// An in-process monerod double: `json_rpc`, `get_height` and
/// `get_blocks.bin` over a chain the test sets, optionally behind a digest
/// login. Test-only.
#[cfg(any(test, feature = "stub-daemon"))]
pub mod stub {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use md5::{Digest, Md5};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const REALM: &str = "monero-rpc";
    const NONCE: &str = "6f3a";

    /// A running stub; dropped, it stops.
    pub struct StubDaemon {
        /// `http://127.0.0.1:<port>`.
        pub url: String,
        chain: Arc<Chain>,
        hits: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    /// One block the stub serves: its encoding and its miner transaction's output count.
    #[derive(Debug, Clone)]
    pub struct StubBlock {
        /// The consensus encoding.
        pub blob: Vec<u8>,
        /// Outputs of its miner transaction.
        pub outputs: usize,
    }

    #[derive(Default)]
    struct Chain {
        height: AtomicU64,
        /// Blocks from `base` on; `get_height` follows them once set.
        blocks: Mutex<(u64, Vec<StubBlock>)>,
        /// `get_blocks.bin` answers bytes that do not decode.
        garbage: AtomicBool,
    }

    /// How the stub answers.
    #[derive(Clone, Default)]
    pub struct StubConfig {
        /// `(user, password)` it demands.
        pub login: Option<(String, String)>,
        /// Extra bytes in the height answer.
        pub pad: usize,
        /// `get_info` says the daemon is still syncing.
        pub syncing: bool,
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
            let chain = Arc::new(Chain { height: AtomicU64::new(height), ..Chain::default() });
            let hits = Arc::new(AtomicUsize::new(0));
            let (h, n) = (chain.clone(), hits.clone());
            let task = tokio::spawn(async move {
                while let Ok((s, _)) = listener.accept().await {
                    let (h, n, c) = (h.clone(), n.clone(), config.clone());
                    tokio::spawn(async move {
                        let _ = serve(s, &h, &n, &c).await;
                    });
                }
            });
            Self { url, chain, hits, task }
        }

        /// Move the chain tip.
        pub fn set_height(&self, height: u64) {
            self.chain.height.store(height, Ordering::SeqCst);
        }

        /// Serve `blocks` as blocks `base..`; the tip is the last one.
        pub fn set_chain(&self, base: u64, blocks: Vec<StubBlock>) {
            let tip = (base + u64::try_from(blocks.len()).expect("len")).saturating_sub(1);
            if let Ok(mut c) = self.chain.blocks.lock() {
                *c = (base, blocks);
            }
            self.chain.height.store(tip, Ordering::SeqCst);
        }

        /// Answer `get_blocks.bin` with bytes that do not decode.
        pub fn set_garbage(&self, on: bool) {
            self.chain.garbage.store(on, Ordering::SeqCst);
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

    fn varint(out: &mut Vec<u8>, v: usize) {
        let v = u64::try_from(v).expect("small");
        let (len, mark) = match v {
            0..=63 => (1, 0),
            64..=16_383 => (2, 1),
            16_384..=1_073_741_823 => (4, 2),
            _ => (8, 3),
        };
        out.extend_from_slice(&((v << 2) | mark).to_le_bytes()[..len]);
    }

    fn key(out: &mut Vec<u8>, k: &str, kind: u8) {
        out.push(u8::try_from(k.len()).expect("short key"));
        out.extend_from_slice(k.as_bytes());
        out.push(kind);
    }

    const STRING: u8 = 10;
    const OBJECT: u8 = 12;
    const UINT64: u8 = 5;
    const ARRAY: u8 = 0x80;

    /// The u64 after `name` in an epee request (key, type byte, then 8 bytes).
    fn request_u64(body: &[u8], name: &[u8]) -> Option<u64> {
        let at = body.windows(name.len()).position(|w| w == name)? + name.len() + 1;
        Some(u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?))
    }

    /// `get_blocks.bin`: the blocks from `start_height`, at most `max_block_count`.
    fn blocks_bin(chain: &Chain, body: &[u8]) -> Vec<u8> {
        let start = request_u64(body, b"start_height").unwrap_or(0);
        let max = request_u64(body, b"max_block_count").unwrap_or(u64::MAX).max(1);
        let (base, blocks) = chain.blocks.lock().map(|c| c.clone()).unwrap_or_default();
        let from = usize::try_from(start.saturating_sub(base)).unwrap_or(usize::MAX);
        let mut index: u64 = blocks.iter().take(from).map(|b| u64::try_from(b.outputs).unwrap_or(0)).sum();
        let served: Vec<&StubBlock> = blocks.iter().skip(from).take(usize::try_from(max).unwrap_or(usize::MAX)).collect();
        let mut out = vec![0x01, 0x11, 0x01, 0x01, 0x01, 0x01, 0x02, 0x01, 1];
        varint(&mut out, 3);
        key(&mut out, "status", STRING);
        varint(&mut out, 2);
        out.extend_from_slice(b"OK");
        key(&mut out, "blocks", OBJECT | ARRAY);
        varint(&mut out, served.len());
        for b in &served {
            varint(&mut out, 1);
            key(&mut out, "block", STRING);
            if chain.garbage.load(Ordering::SeqCst) {
                varint(&mut out, 3);
                out.extend_from_slice(&[0xff; 3]);
            } else {
                varint(&mut out, b.blob.len());
                out.extend_from_slice(&b.blob);
            }
        }
        key(&mut out, "output_indices", OBJECT | ARRAY);
        varint(&mut out, served.len());
        for b in &served {
            varint(&mut out, 1);
            key(&mut out, "indices", OBJECT | ARRAY);
            varint(&mut out, 1);
            if b.outputs == 0 {
                varint(&mut out, 0);
                continue;
            }
            varint(&mut out, 1);
            key(&mut out, "indices", UINT64 | ARRAY);
            varint(&mut out, b.outputs);
            for _ in 0..b.outputs {
                out.extend_from_slice(&index.to_le_bytes());
                index += 1;
            }
        }
        out
    }

    async fn serve(
        mut s: TcpStream,
        chain: &Chain,
        hits: &AtomicUsize,
        config: &StubConfig,
    ) -> std::io::Result<()> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let (head, body_at, body_len) = loop {
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
                break (head, end + 4, end + 4 + len);
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
        if path == "/get_blocks.bin" {
            let body = blocks_bin(chain, &buf[body_at..]);
            hits.fetch_add(1, Ordering::SeqCst);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            s.write_all(head.as_bytes()).await?;
            return s.write_all(&body).await;
        }
        let height = &chain.height;
        let body = match path.as_str() {
            "/json_rpc" if String::from_utf8_lossy(&buf[body_at..]).contains("get_info") => format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{{\"synchronized\":{},\"status\":\"OK\"}}}}",
                !config.syncing
            ),
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

    /// A syncing daemon's height is not the network's: it answers nothing.
    #[tokio::test]
    async fn a_syncing_daemon_gives_no_height() {
        let d = StubDaemon::start(3000, StubConfig { syncing: true, ..StubConfig::default() }).await;
        assert_eq!(daemon_height(local(&d.url, "")).await, Err(DaemonError::Syncing));
    }

    #[tokio::test]
    async fn a_digest_login_answers_the_challenge() {
        let cfg = StubConfig { login: Some(("u".into(), "pw".into())), ..StubConfig::default() };
        let d = StubDaemon::start(42, cfg).await;
        assert_eq!(daemon_height(local(&d.url, "u:pw")).await, Ok(42));
        assert_eq!(daemon_height(local(&d.url, "u:nope")).await, Err(DaemonError::Login));
        assert_eq!(daemon_height(local(&d.url, "")).await, Err(DaemonError::Login));
    }

    /// The per-call limit holds: a padded height answer is refused.
    #[tokio::test]
    async fn an_oversized_answer_is_refused() {
        let d = StubDaemon::start(7, StubConfig { pad: 2 * 1024 * 1024, ..StubConfig::default() }).await;
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

    /// A block that does not decode is the daemon's fault, not a lost connection.
    #[tokio::test]
    async fn an_undecodable_block_is_a_daemon_fault() {
        let d = StubDaemon::start(0, StubConfig::default()).await;
        d.set_chain(100, vec![super::stub::StubBlock { blob: vec![0xff; 3], outputs: 0 }]);
        let got = scannable_blocks(local(&d.url, ""), 100, 100).await;
        assert!(matches!(&got, Err(DaemonError::Fault(_))), "{got:?}");
        d.set_garbage(true);
        let got = scannable_blocks(local(&d.url, ""), 100, 100).await;
        assert!(matches!(&got, Err(DaemonError::Fault(_))), "{got:?}");
        assert_eq!(daemon_height(local(&d.url, "")).await, Ok(100), "the tip follows the chain");
        drop(d);
    }

    #[tokio::test]
    async fn an_unreachable_daemon_is_no_fault() {
        let d = StubDaemon::start(0, StubConfig::default()).await;
        let url = d.url.clone();
        drop(d);
        tokio::task::yield_now().await;
        let got = scannable_blocks(local(&url, ""), 1, 1).await;
        assert!(matches!(&got, Err(DaemonError::Rpc(_))), "{got:?}");
    }

    #[test]
    fn refusals_are_one_line() {
        assert_eq!(DaemonError::NonOnionOff.to_string(), "daemon: non-onion dialing off");
        assert_eq!(DaemonError::Rpc("http 500".into()).to_string(), "daemon: http 500");
    }
}
