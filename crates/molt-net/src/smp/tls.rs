// SPDX-License-Identifier: GPL-3.0-or-later

//! Pinned-fingerprint TLS 1.3 for SMP (transport concept §3.1).
//!
//! SMP servers present a self-signed chain `[online cert, offline CA]`.
//! We verify **against the pinned CA fingerprint only** (`smp://<fp>@…`):
//! the CA cert in the presented chain must hash to the pin, and the online
//! (end-entity) cert must be validly signed by that CA. No WebPKI, no CA
//! store, no OCSP. ALPN is `smp/1`.
//!
//! Pure-Rust posture: rustls with the RustCrypto provider (no ring/aws-lc,
//! so no C toolchain — matches the reproducible-build envelope).

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{alg_id, AlgorithmIdentifier, CertificateDer, InvalidSignature, ServerName, SignatureVerificationAlgorithm, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::{client::TlsStream, TlsConnector};

use crate::smp::ed448;
use crate::smp::server::SmpServer;
use crate::NetError;

/// ALPN protocol identifier for SMP v1.
const ALPN_SMP: &[u8] = b"smp/1";

/// Deadline for opening the raw socket (TCP connect incl. SOCKS negotiation /
/// Tor circuit build) — sized for a cold Tor circuit (T4 §P5).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for the TLS 1.3 handshake once the socket is open (T4 §P5).
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Ed448 signature/key OID (1.3.101.113), dotted form for cert dispatch.
const OID_ED448: &str = "1.3.101.113";

/// The Ed448 signature-verification algorithm the pure-Rust rustcrypto
/// provider is missing. Backed by our RFC-8032-validated verifier so the
/// client advertises AND verifies Ed448 — supporting the official
/// simplex.im servers (Ed448 certs) without a C toolchain.
#[derive(Debug)]
struct Ed448Sva;

impl SignatureVerificationAlgorithm for Ed448Sva {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        if ed448::verify(public_key, message, signature) {
            Ok(())
        } else {
            Err(InvalidSignature)
        }
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::ED448
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        alg_id::ED448
    }
}

static ED448_SVA: Ed448Sva = Ed448Sva;

/// The rustcrypto signature-verification set, augmented with Ed448 (built
/// once; the `'static` slices are leaked intentionally — a single small
/// allocation for the process).
fn sig_algs_with_ed448() -> WebPkiSupportedAlgorithms {
    static AUGMENTED: OnceLock<WebPkiSupportedAlgorithms> = OnceLock::new();
    *AUGMENTED.get_or_init(|| {
        let base = rustls_rustcrypto::provider().signature_verification_algorithms;
        let ed448: &'static dyn SignatureVerificationAlgorithm = &ED448_SVA;
        let mut all: Vec<&'static dyn SignatureVerificationAlgorithm> = base.all.to_vec();
        all.push(ed448);
        let all: &'static [&'static dyn SignatureVerificationAlgorithm] =
            Box::leak(all.into_boxed_slice());
        let ed448_only: &'static [&'static dyn SignatureVerificationAlgorithm] =
            Box::leak(vec![ed448].into_boxed_slice());
        let mut mapping = base.mapping.to_vec();
        mapping.push((SignatureScheme::ED448, ed448_only));
        let mapping: &'static [(SignatureScheme, &'static [&'static dyn SignatureVerificationAlgorithm])] =
            Box::leak(mapping.into_boxed_slice());
        WebPkiSupportedAlgorithms { all, mapping }
    })
}

/// A verifier that trusts exactly one CA — the one whose SHA-256 matches
/// the pinned fingerprint — and checks the presented chain against it.
#[derive(Debug)]
struct PinnedCaVerifier {
    pin: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCaVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // find the pinned CA among the presented certs (end-entity or an
        // intermediate). SimpleX presents [online, offline-CA].
        let all: Vec<&CertificateDer<'_>> =
            std::iter::once(end_entity).chain(intermediates).collect();
        let ca = all
            .iter()
            .find(|c| Sha256::digest(c.as_ref()).as_slice() == self.pin)
            .ok_or_else(|| {
                rustls::Error::General(
                    "SMP server certificate does not match the pinned fingerprint \
                     (server downgrade or MITM) — refusing"
                        .to_string(),
                )
            })?;

        // the end-entity must be validly signed by the pinned CA. When the
        // CA *is* the end-entity (single self-signed cert), the pin match
        // already establishes trust.
        if Sha256::digest(end_entity.as_ref()).as_slice() != self.pin {
            verify_signed_by(end_entity, ca)?;
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Verify `leaf` is signed by `ca`. SMP certs are Ed25519 (konkin and
/// most self-hosted) or **Ed448** (official simplex.im) — x509-parser's
/// built-in check does not do Ed448, so that case is verified with our
/// RFC-8032 verifier over the raw TBS bytes.
fn verify_signed_by(
    leaf: &CertificateDer<'_>,
    ca: &CertificateDer<'_>,
) -> Result<(), rustls::Error> {
    use x509_parser::prelude::FromDer;
    let (_, leaf_c) = x509_parser::certificate::X509Certificate::from_der(leaf.as_ref())
        .map_err(|_| rustls::Error::General("SMP leaf certificate is malformed".into()))?;
    let (_, ca_c) = x509_parser::certificate::X509Certificate::from_der(ca.as_ref())
        .map_err(|_| rustls::Error::General("SMP CA certificate is malformed".into()))?;

    if leaf_c.signature_algorithm.oid().to_id_string() == OID_ED448 {
        // Ed448: verify the CA's key over the DER-encoded TBSCertificate
        let tbs = leaf_c.tbs_certificate.as_ref();
        let sig = leaf_c.signature_value.as_ref();
        let ca_key = ca_c.public_key().subject_public_key.as_ref();
        if ed448::verify(ca_key, tbs, sig) {
            Ok(())
        } else {
            Err(rustls::Error::General(
                "SMP online certificate (Ed448) is not signed by the pinned CA".into(),
            ))
        }
    } else {
        leaf_c
            .verify_signature(Some(ca_c.public_key()))
            .map_err(|e| {
                rustls::Error::General(format!(
                    "SMP online certificate is not signed by the pinned CA: {e}"
                ))
            })
    }
}

/// Build a rustls client config that pins `server`'s CA fingerprint and
/// offers ALPN `smp/1`.
fn pinned_config(server: &SmpServer) -> Result<ClientConfig, NetError> {
    // Restrict key exchange to X25519 — what every SMP server negotiates.
    // Offering the alpha provider's full group set makes strict servers
    // (smp8) abort the handshake (a group triggering HelloRetryRequest the
    // provider mishandles); pinning X25519 avoids the retry entirely.
    let mut base = rustls_rustcrypto::provider();
    base.kx_groups
        .retain(|g| g.name() == rustls::NamedGroup::X25519);
    // add Ed448 so the client advertises it in signature_algorithms AND
    // can verify the server's Ed448 CertificateVerify (official servers)
    base.signature_verification_algorithms = sig_algs_with_ed448();
    let provider = Arc::new(base);
    let verifier = Arc::new(PinnedCaVerifier {
        pin: server.fingerprint_raw(),
        provider: provider.clone(),
    });
    // SMP mandates TLS 1.3 (offering 1.2 makes strict servers abort with
    // HandshakeFailure).
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| NetError::Crypto(format!("rustls provider: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![ALPN_SMP.to_vec()];
    Ok(config)
}

/// A handle to the process-global in-process arti Tor client. Only exists under
/// `--features embedded-tor`. Holds a shared reference to the lazily-bootstrapped
/// [`TorClient`](arti_client::TorClient) and its per-server-host isolation-token
/// map (`crate::tor_embedded::ArtiShared`); cloning it is cheap (an `Arc`).
#[cfg(feature = "embedded-tor")]
#[derive(Clone)]
pub struct ArtiHandle {
    /// The shared, lazily-bootstrapped arti client + isolation map (§4).
    shared: Arc<crate::tor_embedded::ArtiShared>,
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

/// A dialed byte stream to an SMP server: a direct / SOCKS5h `TcpStream`, or
/// (only on the `embedded-tor` build) an in-process arti
/// [`DataStream`](arti_client::DataStream). Both concrete streams are
/// `AsyncRead + AsyncWrite + Unpin + Send`; unifying them here lets
/// [`connect_tls`] TLS-handshake over either without boxing, and keeps the
/// non-Tor path byte-identical (`Tcp` is a zero-cost `TcpStream` wrapper).
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

/// How to reach an SMP server's TCP socket. `Direct` is clearnet/loopback;
/// `Socks5` routes through a SOCKS5h proxy (Tor `local`/`whonix`); `Arti`
/// (embedded build only) routes through an in-process Tor client. `Direct`
/// is the default so the clearnet path is unchanged until Tor is configured.
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
        /// `molt-<session>-<host>` so each server host gets its own Tor
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
    fn tor_on(&self) -> bool {
        match self {
            Dialer::Direct => false,
            Dialer::Socks5 { .. } => true,
            #[cfg(feature = "embedded-tor")]
            Dialer::Arti(_) => true,
        }
    }

    /// Open the raw byte stream to `server` per this dialer, honouring the
    /// onion-preferred [`SmpServer::dial_target`] and a Tor-sized connect
    /// deadline (T4 §P5). Returns a [`DialStream`] so the caller handshakes TLS
    /// over a `TcpStream` (Direct/Socks5) or an arti `DataStream` (embedded)
    /// uniformly.
    pub async fn dial(&self, server: &SmpServer) -> Result<DialStream, NetError> {
        let (host, port) = server.dial_target(self.tor_on());
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
                // one Tor circuit per server host: molt-<session>-<host>
                let isolation = format!("molt-{session}-{host}");
                let tcp = timeout(
                    CONNECT_TIMEOUT,
                    crate::socks5::socks5h_connect(proxy, host, port, &isolation),
                )
                .await
                .map_err(|_| {
                    NetError::TorUnavailable(format!(
                        "tor circuit to {host} via {proxy} timed out"
                    ))
                })??;
                Ok(DialStream::Tcp(tcp))
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

/// Dial `server` over TCP+TLS 1.3 (through `dialer`), verifying the pinned
/// fingerprint and negotiating ALPN `smp/1`. Returns the established TLS stream.
pub async fn connect_tls(
    dialer: &Dialer,
    server: &SmpServer,
) -> Result<TlsStream<DialStream>, NetError> {
    let config = pinned_config(server)?;
    let connector = TlsConnector::from(Arc::new(config));
    let stream = dialer.dial(server).await?;
    // the SNI name is the host; cert verification ignores it (we pin), but
    // rustls requires a valid ServerName
    let sni = ServerName::try_from(server.host.clone())
        .map_err(|_| NetError::Framing(format!("invalid host for SNI: {}", server.host)))?;
    let tls = timeout(TLS_HANDSHAKE_TIMEOUT, connector.connect(sni, stream))
        .await
        .map_err(|_| {
            NetError::TorUnavailable(format!("tls handshake with {} timed out", server.host))
        })?
        .map_err(|e| NetError::Crypto(format!("tls handshake with {}: {e}", server.host)))?;
    // confirm the server actually spoke smp/1
    let (_, conn) = tls.get_ref();
    match conn.alpn_protocol() {
        Some(p) if p == ALPN_SMP => Ok(tls),
        other => Err(NetError::Crypto(format!(
            "SMP server did not negotiate ALPN smp/1 (got {other:?})"
        ))),
    }
}

/// A one-shot connectivity check for the settings "Test connection" button:
/// dial (through the resolved `dialer`, honouring onion-preferred routing),
/// pin, ALPN — report success or the concrete reason. Does not run any SMP
/// commands. An onion-only target under a `Direct` dialer fails closed with a
/// "requires Tor" reason instead of a clearnet dial (T4 §P7).
pub async fn test_connection(dialer: &Dialer, server: &SmpServer) -> Result<(), NetError> {
    let _tls = connect_tls(dialer, server).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=";

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
        // an onion-only server dialed Direct fails closed (no clearnet dial,
        // no hang) — the resolver-less path cannot reach .onion.
        let onion = SmpServer::parse(&format!("smp://{FP}@abcd.onion")).expect("onion");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let res = rt.block_on(Dialer::Direct.dial(&onion));
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
        let server = SmpServer::parse(&format!("smp://{FP}@example.invalid")).expect("server");
        let started = tokio::time::Instant::now();
        let res = dialer.dial(&server).await;
        let elapsed = started.elapsed();
        // returns cleanly (never an infinite await), bounded by the deadline.
        assert!(matches!(res, Err(NetError::TorUnavailable(_))), "got {res:?}");
        assert!(
            elapsed <= CONNECT_TIMEOUT + Duration::from_secs(5),
            "elapsed {elapsed:?} exceeds the connect deadline"
        );
    }
}
