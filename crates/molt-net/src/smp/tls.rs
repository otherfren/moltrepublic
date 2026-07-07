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

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{alg_id, AlgorithmIdentifier, CertificateDer, InvalidSignature, ServerName, SignatureVerificationAlgorithm, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, TlsConnector};

use crate::smp::ed448;
use crate::smp::server::SmpServer;
use crate::NetError;

/// ALPN protocol identifier for SMP v1.
const ALPN_SMP: &[u8] = b"smp/1";
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

/// How to reach an SMP server's TCP socket. `Direct` is clearnet/loopback;
/// `Socks5` routes through a SOCKS5h proxy (Tor `local`/`whonix`), one circuit
/// per server host (stream isolation, concept §4). Default is `Direct` so the
/// clearnet path is unchanged until Tor is explicitly configured.
#[derive(Clone, Debug, Default)]
pub enum Dialer {
    /// Direct TCP — no Tor.
    #[default]
    Direct,
    /// SOCKS5h through `proxy` (e.g. `127.0.0.1:9050`).
    Socks5 {
        /// The proxy's `host:port` (a Tor SOCKS listener).
        proxy: String,
    },
}

impl Dialer {
    /// Map the transport config to a dialer: `local` → SOCKS5h at
    /// `127.0.0.1:<tor_port>`, `whonix` → the gateway's SOCKS listener,
    /// everything else (`off`/`direct`/`embedded`-arti-later/unknown) → direct.
    /// This is the one place that decides whether SMP goes through Tor; wiring
    /// it into the engine's `SmpTransport` construction is the enable step.
    pub fn from_config(tor_mode: &str, tor_port: u16) -> Dialer {
        match tor_mode {
            "local" => Dialer::Socks5 {
                proxy: format!("127.0.0.1:{tor_port}"),
            },
            "whonix" => Dialer::Socks5 {
                proxy: "10.152.152.10:9050".to_string(),
            },
            _ => Dialer::Direct,
        }
    }

    /// Open the raw TCP socket to `server` per this dialer.
    pub async fn dial(&self, server: &SmpServer) -> Result<TcpStream, NetError> {
        match self {
            Dialer::Direct => TcpStream::connect(server.addr())
                .await
                .map_err(|e| NetError::Unreachable(format!("tcp {}: {e}", server.addr()))),
            Dialer::Socks5 { proxy } => {
                // one Tor circuit per server host: the isolation token is the host
                let isolation = format!("molt-{}", server.host);
                crate::socks5::socks5h_connect(proxy, &server.host, server.port, &isolation).await
            }
        }
    }
}

/// Dial `server` over TCP+TLS 1.3 (through `dialer`), verifying the pinned
/// fingerprint and negotiating ALPN `smp/1`. Returns the established TLS stream.
pub async fn connect_tls(
    dialer: &Dialer,
    server: &SmpServer,
) -> Result<TlsStream<TcpStream>, NetError> {
    let config = pinned_config(server)?;
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = dialer.dial(server).await?;
    // the SNI name is the host; cert verification ignores it (we pin), but
    // rustls requires a valid ServerName
    let sni = ServerName::try_from(server.host.clone())
        .map_err(|_| NetError::Framing(format!("invalid host for SNI: {}", server.host)))?;
    let tls = connector
        .connect(sni, tcp)
        .await
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

/// A one-shot connectivity check for the settings "Test connection"
/// button: dial, pin, ALPN — report success or the concrete reason. Does
/// not run any SMP commands.
pub async fn test_connection(server: &SmpServer) -> Result<(), NetError> {
    // the settings probe dials directly; testing over Tor is a later toggle
    let _tls = connect_tls(&Dialer::Direct, server).await?;
    Ok(())
}
