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

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, TlsConnector};

use crate::smp::server::SmpServer;
use crate::NetError;

/// ALPN protocol identifier for SMP v1.
const ALPN_SMP: &[u8] = b"smp/1";

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

/// Verify `leaf` is signed by `ca` (Ed25519 SMP certs), using x509-parser's
/// signature check.
fn verify_signed_by(
    leaf: &CertificateDer<'_>,
    ca: &CertificateDer<'_>,
) -> Result<(), rustls::Error> {
    use x509_parser::prelude::FromDer;
    let (_, leaf_c) = x509_parser::certificate::X509Certificate::from_der(leaf.as_ref())
        .map_err(|_| rustls::Error::General("SMP leaf certificate is malformed".into()))?;
    let (_, ca_c) = x509_parser::certificate::X509Certificate::from_der(ca.as_ref())
        .map_err(|_| rustls::Error::General("SMP CA certificate is malformed".into()))?;
    leaf_c
        .verify_signature(Some(ca_c.public_key()))
        .map_err(|e| {
            rustls::Error::General(format!(
                "SMP online certificate is not signed by the pinned CA: {e}"
            ))
        })
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

/// Dial `server` over TCP+TLS 1.3, verifying the pinned fingerprint and
/// negotiating ALPN `smp/1`. Returns the established TLS stream. (Tor
/// dialing via SOCKS is milestone T4; this connects directly for now.)
pub async fn connect_tls(server: &SmpServer) -> Result<TlsStream<TcpStream>, NetError> {
    let config = pinned_config(server)?;
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(server.addr())
        .await
        .map_err(|e| NetError::Unreachable(format!("tcp {}: {e}", server.addr())))?;
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
    let _tls = connect_tls(server).await?;
    Ok(())
}
