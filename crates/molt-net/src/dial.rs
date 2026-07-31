// SPDX-License-Identifier: GPL-3.0-or-later

//! The fail-closed dialer (T4): the ONE place a raw TCP socket to a remote
//! host opens.
//!
//! [`Dialer::resolve`] turns the transport config into a routing decision —
//! under `network = tor` there is **no** path to a direct (clearnet) dial —
//! and [`Dialer::dial_host`] executes it: direct TCP, SOCKS5h through a Tor
//! proxy (per-host stream isolation, fresh per session), or the in-process
//! arti client on the opt-in `embedded-tor` build. The S3 backup client
//! drives it today; N2's WebSocket relay connections reuse it unchanged.
//!
//! Pure-Rust posture: rustls with the RustCrypto provider (no ring/aws-lc,
//! so no C toolchain — matches the reproducible-build envelope).

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::NetError;

/// Deadline for opening the raw socket (TCP connect incl. SOCKS negotiation /
/// Tor circuit build) — sized for a cold Tor circuit (T4 §P5).
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The rustcrypto provider with key exchange restricted to X25519 — the
/// shared base of every TLS client in this crate. Offering the alpha
/// provider's full group set makes strict servers abort the handshake
/// (a group triggering HelloRetryRequest the provider mishandles); pinning
/// X25519 avoids the retry entirely, and every current endpoint negotiates it.
/// A rustls config verifying against the public WebPKI (`webpki-roots`) —
/// the crate's ONE outbound TLS posture: pure-Rust rustcrypto provider,
/// TLS 1.3, X25519 (see [`x25519_provider`]). Built once and shared by the
/// S3 client and the N2 relay WebSocket layer.
pub(crate) fn public_tls_config() -> Result<std::sync::Arc<rustls::ClientConfig>, NetError> {
    static CFG: std::sync::OnceLock<std::sync::Arc<rustls::ClientConfig>> =
        std::sync::OnceLock::new();
    if let Some(cfg) = CFG.get() {
        return Ok(cfg.clone());
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        x25519_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|e| NetError::Crypto(format!("rustls provider: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(CFG.get_or_init(|| std::sync::Arc::new(config)).clone())
}

pub(crate) fn x25519_provider() -> rustls::crypto::CryptoProvider {
    let mut base = rustls_rustcrypto::provider();
    base.kx_groups
        .retain(|g| g.name() == rustls::NamedGroup::X25519);
    base
}

/// A handle to the process-global in-process arti Tor client. Only exists under
/// `--features embedded-tor`. Holds a shared reference to the lazily-bootstrapped
/// [`TorClient`](arti_client::TorClient) and its per-host isolation-token
/// map (`crate::tor_embedded::ArtiShared`); cloning it is cheap (an `Arc`).
#[cfg(feature = "embedded-tor")]
#[derive(Clone)]
pub struct ArtiHandle {
    /// The shared, lazily-bootstrapped arti client + isolation map (§4).
    shared: std::sync::Arc<crate::tor_embedded::ArtiShared>,
}

#[cfg(feature = "embedded-tor")]
impl std::fmt::Debug for ArtiHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the arti client/isolation map are not Debug; a stable tag is enough.
        f.write_str("ArtiHandle(embedded arti)")
    }
}

#[cfg(feature = "embedded-tor")]
impl ArtiHandle {
    /// A handle to the process-global embedded arti client (concept §4). Cheap:
    /// no bootstrap happens here — that is deferred to the first dial.
    fn new() -> ArtiHandle {
        ArtiHandle {
            shared: crate::tor_embedded::shared(),
        }
    }
}

/// A dialed byte stream: a direct / SOCKS5h `TcpStream`, or (only on the
/// `embedded-tor` build) an in-process arti
/// [`DataStream`](arti_client::DataStream). Both concrete streams are
/// `AsyncRead + AsyncWrite + Unpin + Send`; unifying them here lets a caller
/// TLS-handshake over either without boxing, and keeps the non-Tor path
/// byte-identical (`Tcp` is a zero-cost `TcpStream` wrapper).
#[derive(Debug)]
pub enum DialStream {
    /// A direct or SOCKS5h-tunnelled TCP stream (clearnet / system Tor / whonix).
    Tcp(TcpStream),
    /// An in-process arti Tor data stream (embedded build only). Boxed: an arti
    /// `DataStream` is ~700 bytes, and un-boxed it would bloat every `Tcp`
    /// variant (the common clearnet path) to that size (clippy
    /// `large_enum_variant`). An arti dial is rare and pooled, so the extra
    /// allocation is negligible.
    #[cfg(feature = "embedded-tor")]
    Arti(Box<arti_client::DataStream>),
}

impl AsyncRead for DialStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            DialStream::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(feature = "embedded-tor")]
            DialStream::Arti(s) => Pin::new(&mut **s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for DialStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            DialStream::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(feature = "embedded-tor")]
            DialStream::Arti(s) => Pin::new(&mut **s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            DialStream::Tcp(s) => Pin::new(s).poll_flush(cx),
            #[cfg(feature = "embedded-tor")]
            DialStream::Arti(s) => Pin::new(&mut **s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            DialStream::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(feature = "embedded-tor")]
            DialStream::Arti(s) => Pin::new(&mut **s).poll_shutdown(cx),
        }
    }
}

/// How to reach a remote TCP socket. `Direct` is clearnet/loopback; `Socks5`
/// routes through a SOCKS5h proxy (Tor `local`/`whonix`); `Arti` (embedded
/// build only) routes through an in-process Tor client. `Direct` is the
/// default so the clearnet path is unchanged until Tor is configured.
///
/// The routing decision is fail-closed and lives in exactly one place,
/// [`Dialer::resolve`]: under `network = tor` there is **no** path to
/// `Direct` (transport concept §6, T4 §P1).
#[derive(Clone, Debug, Default)]
pub enum Dialer {
    /// Direct TCP — no Tor.
    #[default]
    Direct,
    /// SOCKS5h through `proxy` (e.g. `127.0.0.1:9050`).
    Socks5 {
        /// The proxy's `host:port` (a Tor SOCKS listener).
        proxy: String,
        /// A per-session random isolation prefix. The SOCKS username is
        /// `molt-<session>-<host>` so each remote host gets its own Tor
        /// circuit (stream isolation) and the circuit set is fresh each run
        /// (no cross-session linkability). Minted once in [`Dialer::resolve`].
        session: String,
    },
    /// In-process arti Tor client (only built with `--features embedded-tor`).
    #[cfg(feature = "embedded-tor")]
    Arti(ArtiHandle),
}

/// Mint a per-session random isolation prefix (8 bytes, hex).
fn session_token() -> Result<String, NetError> {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b)
        .map_err(|e| NetError::Crypto(format!("os rng unavailable: {e}")))?;
    Ok(hex::encode(b))
}

impl Dialer {
    /// Resolve the transport config to a dialer — the **single, fail-closed**
    /// routing decision (T4 §P1):
    ///
    /// - `network = none` → [`Dialer::Direct`] (clearnet).
    /// - `network = tor, mode = local` → SOCKS5h at `127.0.0.1:<port>`.
    /// - `network = tor, mode = whonix` → SOCKS5h at the gateway `10.152.152.10:9050`.
    /// - `network = tor, mode = embedded` → the arti dialer with
    ///   `--features embedded-tor`, else [`NetError::TorMisconfigured`].
    /// - `network = tor`, unknown mode → [`NetError::TorMisconfigured`].
    /// - `network = nym` → [`NetError::TorMisconfigured`] (not implemented).
    /// - unknown network → [`NetError::TorMisconfigured`].
    ///
    /// There is **no input under `network = tor` that yields `Direct`** — the
    /// whole fail-closed guarantee, concentrated here.
    pub fn resolve(network: &str, mode: &str, port: u16) -> Result<Dialer, NetError> {
        match network {
            "none" => Ok(Dialer::Direct),
            "tor" => match mode {
                "local" => Dialer::socks5(format!("127.0.0.1:{port}")),
                "whonix" => Dialer::socks5("10.152.152.10:9050".to_string()),
                "embedded" => Dialer::resolve_embedded(port),
                other => Err(NetError::TorMisconfigured(format!(
                    "unknown tor mode `{other}` (expected local | embedded | whonix)"
                ))),
            },
            "nym" => Err(NetError::TorMisconfigured("nym not implemented".into())),
            other => Err(NetError::TorMisconfigured(format!(
                "unknown anonymity network `{other}` (expected none | tor | nym)"
            ))),
        }
    }

    /// A SOCKS5h dialer with a fresh isolation session.
    fn socks5(proxy: String) -> Result<Dialer, NetError> {
        Ok(Dialer::Socks5 {
            proxy,
            session: session_token()?,
        })
    }

    /// Resolve `mode = embedded` (arti). With the feature, route to the
    /// process-global embedded arti client (bootstrapped lazily on the first
    /// dial); without it, fail closed. The SOCKS `port` is irrelevant to the
    /// in-process client, so it is ignored here.
    #[cfg(feature = "embedded-tor")]
    fn resolve_embedded(_port: u16) -> Result<Dialer, NetError> {
        Ok(Dialer::Arti(ArtiHandle::new()))
    }

    /// Without the `embedded-tor` feature, `mode = embedded` is a clean
    /// config error — never a silent clearnet fallback.
    #[cfg(not(feature = "embedded-tor"))]
    fn resolve_embedded(_port: u16) -> Result<Dialer, NetError> {
        Err(NetError::TorMisconfigured(
            "embedded Tor not built — rebuild with --features embedded-tor".into(),
        ))
    }

    /// Whether this dialer routes over Tor (so `.onion` alternates are
    /// preferred and local DNS never happens).
    pub fn tor_on(&self) -> bool {
        match self {
            Dialer::Direct => false,
            Dialer::Socks5 { .. } => true,
            #[cfg(feature = "embedded-tor")]
            Dialer::Arti(_) => true,
        }
    }

    /// Open the raw byte stream to `host:port` per this dialer — the generic
    /// dial every network client of this crate shares (the S3 backup client
    /// today; N2's WebSocket relay connections next). All fail-closed
    /// properties hold here: an `.onion` host under `Direct` is refused
    /// (never a clearnet dial/DNS leak), SOCKS circuits are per-host
    /// isolated, and the connect deadline is Tor-sized.
    pub async fn dial_host(&self, host: &str, port: u16) -> Result<DialStream, NetError> {
        match self {
            Dialer::Direct => {
                // a resolver-less direct dial can never reach an .onion — fail
                // closed with a clear reason instead of a DNS error / hang.
                if host.ends_with(".onion") {
                    return Err(NetError::TorMisconfigured(format!(
                        "server {host} is onion-only but Tor is off — enable Tor to reach it"
                    )));
                }
                let addr = format!("{host}:{port}");
                let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
                    .await
                    .map_err(|_| NetError::Unreachable(format!("tcp {addr}: connect timed out")))?
                    .map_err(|e| NetError::Unreachable(format!("tcp {addr}: {e}")))?;
                Ok(DialStream::Tcp(tcp))
            }
            Dialer::Socks5 { proxy, session } => {
                // One Tor circuit per remote host (IsolateSOCKSAuth): the
                // SOCKS username is `molt-<session>-<host>`, capped at the
                // RFC 1929 limit (tokio-socks refuses an oversized field
                // where the old client silently truncated). The fixed
                // non-empty password keeps strict RFC 1929 servers happy.
                // `(host, port)` dials with DOMAINNAME addressing, so the
                // name resolves PROXY-side (SOCKS5h) — no local DNS.
                let mut isolation = format!("molt-{session}-{host}");
                isolation.truncate(255);
                let stream = timeout(
                    CONNECT_TIMEOUT,
                    tokio_socks::tcp::Socks5Stream::connect_with_password(
                        proxy.as_str(),
                        (host, port),
                        &isolation,
                        "molt",
                    ),
                )
                .await
                .map_err(|_| {
                    NetError::TorUnavailable(format!(
                        "tor circuit to {host} via {proxy} timed out"
                    ))
                })?
                .map_err(|e| {
                    NetError::Unreachable(format!("socks proxy {proxy} to {host}: {e}"))
                })?;
                Ok(DialStream::Tcp(stream.into_inner()))
            }
            #[cfg(feature = "embedded-tor")]
            Dialer::Arti(handle) => {
                // No outer connect deadline here: the FIRST embedded dial also
                // bootstraps the Tor directory (slow — minutes on a cold cache),
                // and arti applies its own per-circuit/connect timeouts, so a
                // 30 s cap would abort a legitimate first-run bootstrap. Later
                // dials reuse the client and are fast. Isolation is per host.
                let stream = handle.shared.connect(host, port).await?;
                Ok(DialStream::Arti(Box::new(stream)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_maps_every_mode_and_fails_closed() {
        // none -> Direct (clearnet).
        assert!(matches!(
            Dialer::resolve("none", "local", 9050).expect("none"),
            Dialer::Direct
        ));
        // tor + local -> SOCKS at 127.0.0.1:<port>.
        assert!(matches!(
            Dialer::resolve("tor", "local", 9050).expect("local"),
            Dialer::Socks5 { proxy, .. } if proxy == "127.0.0.1:9050"
        ));
        assert!(matches!(
            Dialer::resolve("tor", "local", 9150).expect("local port"),
            Dialer::Socks5 { proxy, .. } if proxy == "127.0.0.1:9150"
        ));
        // tor + whonix -> the gateway SOCKS listener.
        assert!(matches!(
            Dialer::resolve("tor", "whonix", 9050).expect("whonix"),
            Dialer::Socks5 { proxy, .. } if proxy == "10.152.152.10:9050"
        ));
        // tor + embedded WITHOUT the feature -> fail closed.
        #[cfg(not(feature = "embedded-tor"))]
        assert!(matches!(
            Dialer::resolve("tor", "embedded", 9050),
            Err(NetError::TorMisconfigured(_))
        ));
        // tor + unknown mode, nym, and unknown network all fail closed.
        assert!(matches!(
            Dialer::resolve("tor", "nonsense", 9050),
            Err(NetError::TorMisconfigured(_))
        ));
        assert!(matches!(
            Dialer::resolve("nym", "local", 9050),
            Err(NetError::TorMisconfigured(_))
        ));
        assert!(matches!(
            Dialer::resolve("bogus", "local", 9050),
            Err(NetError::TorMisconfigured(_))
        ));

        // THE fail-closed guarantee: no input under network=tor yields Direct.
        for mode in ["local", "whonix", "embedded", "", "nonsense"] {
            if let Ok(d) = Dialer::resolve("tor", mode, 9050) {
                assert!(
                    !matches!(d, Dialer::Direct),
                    "network=tor mode={mode} must never resolve to Direct"
                );
            }
        }
    }

    #[test]
    fn isolation_token_is_session_random() {
        // two resolves mint distinct isolation sessions (no cross-session
        // circuit linkability).
        let a = Dialer::resolve("tor", "local", 9050).expect("a");
        let b = Dialer::resolve("tor", "local", 9050).expect("b");
        let (Dialer::Socks5 { session: sa, .. }, Dialer::Socks5 { session: sb, .. }) = (a, b)
        else {
            panic!("expected socks dialers");
        };
        assert_ne!(sa, sb, "session token must be random per resolve");
    }

    #[test]
    fn direct_never_targets_onion() {
        // an onion-only host dialed Direct fails closed (no clearnet dial,
        // no hang) — the resolver-less path cannot reach .onion.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let res = rt.block_on(Dialer::Direct.dial_host("abcd.onion", 5223));
        assert!(
            matches!(res, Err(NetError::TorMisconfigured(_))),
            "got {res:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dial_timeout_is_bounded() {
        use tokio::net::TcpListener;
        // a black-hole SOCKS proxy: accepts the connection and never replies.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s); // hold open, never respond
            }
        });
        let dialer = Dialer::Socks5 {
            proxy: addr.to_string(),
            session: "test".to_string(),
        };
        let started = tokio::time::Instant::now();
        let res = dialer.dial_host("example.invalid", 5223).await;
        let elapsed = started.elapsed();
        // returns cleanly (never an infinite await), bounded by the deadline.
        assert!(matches!(res, Err(NetError::TorUnavailable(_))), "got {res:?}");
        assert!(
            elapsed <= CONNECT_TIMEOUT + Duration::from_secs(5),
            "elapsed {elapsed:?} exceeds the connect deadline"
        );
    }
}
