// SPDX-License-Identifier: GPL-3.0-or-later

//! A minimal, pure-Rust S3 client (mock_todo §5): SigV4-signed requests over
//! the engine's fail-closed [`Dialer`] (so S3 traffic routes exactly like SMP
//! traffic — through Tor when Tor is configured, never a silent clearnet
//! fallback), TLS 1.3 via rustls + the RustCrypto provider with the public
//! WebPKI roots (`webpki-roots`, Mozilla's store as data).
//!
//! Supported today: custom endpoints (MinIO & friends, `http://…` onion
//! services included), path-style addressing, region inference from AWS
//! hostnames, and one cheap authenticated operation — [`S3Client::probe_bucket`]
//! (`HEAD /bucket`), the settings panel's "Test connection". The signing and
//! transport core ([`S3Client::request`]) is operation-agnostic so the backup
//! stories can add ListObjectsV2 / GET / PUT without touching the plumbing.
//!
//! Honest limits: virtual-hosted addressing and custom/self-signed TLS CAs
//! are not supported yet (a MinIO with a private CA fails the TLS class with
//! a clear reason). TLS mirrors the SMP client's posture: TLS 1.3 with
//! X25519 — the same rustcrypto-alpha constraint documented in
//! `smp/tls.rs::pinned_config`.

pub mod http;
pub mod sigv4;

use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use sha2::{Digest, Sha256};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::smp::tls::Dialer;
pub use http::HttpResponse;

/// TLS handshake deadline (the dial itself carries the Tor-sized connect
/// deadline inside [`Dialer::dial_host`]).
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Everything the S3 client distinguishes — the error classes are the
/// product surface (the settings verdict renders them), so they stay honest
/// and separate: configuration, reachability, TLS, HTTP status, protocol.
#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    /// The endpoint/credentials configuration is unusable (parse/validation,
    /// caught before any network I/O).
    #[error("endpoint: {0}")]
    Endpoint(String),
    /// The TCP/SOCKS/Tor connection could not be established (DNS, refused,
    /// timeout, Tor misconfigured/unavailable — the inner reason says which).
    #[error("connect: {0}")]
    Connect(String),
    /// The TLS handshake failed (bad certificate, name mismatch, protocol).
    #[error("tls: {0}")]
    Tls(String),
    /// The server answered with a non-success HTTP status; `hint` is the
    /// honest interpretation (403 bad credentials vs 404 missing bucket …).
    #[error("http {status}: {hint}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// What that status means for the operator.
        hint: String,
    },
    /// The byte stream did not parse as HTTP (or timed out mid-exchange).
    #[error("protocol: {0}")]
    Protocol(String),
}

/// URL scheme of an S3 endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S3Scheme {
    /// TLS against the public WebPKI (the normal case).
    Https,
    /// Plaintext HTTP — for `.onion` endpoints (Tor is the transport
    /// encryption) and LAN MinIO setups.
    Http,
}

/// A parsed S3 endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Endpoint {
    /// `https` or `http`.
    pub scheme: S3Scheme,
    /// Hostname (or IP literal).
    pub host: String,
    /// TCP port (defaulted from the scheme when absent).
    pub port: u16,
    /// Normalized base path (no trailing `/`; empty for none) — for
    /// endpoints living under a reverse-proxy prefix.
    pub base_path: String,
}

impl S3Endpoint {
    /// Parse `[scheme://]host[:port][/prefix]`. A missing scheme defaults to
    /// `https` (never a silent plaintext downgrade); explicit `http://` is
    /// allowed for `.onion`/LAN endpoints.
    pub fn parse(raw: &str) -> Result<S3Endpoint, S3Error> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(S3Error::Endpoint("no endpoint configured".to_string()));
        }
        let (scheme, rest) = match raw.split_once("://") {
            Some(("https", rest)) => (S3Scheme::Https, rest),
            Some(("http", rest)) => (S3Scheme::Http, rest),
            Some((other, _)) => {
                return Err(S3Error::Endpoint(format!(
                    "unsupported scheme `{other}` (use https:// or http://)"
                )))
            }
            None => (S3Scheme::Https, raw),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        // host[:port], with [v6-literal]:port support
        let (host, port_str) = if let Some(rest) = authority.strip_prefix('[') {
            let end = rest.find(']').ok_or_else(|| {
                S3Error::Endpoint("unterminated [IPv6] literal".to_string())
            })?;
            (&rest[..end], rest[end + 1..].strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(S3Error::Endpoint("no host in endpoint".to_string()));
        }
        let port = match port_str {
            Some(p) => p
                .parse::<u16>()
                .map_err(|_| S3Error::Endpoint(format!("bad port `{p}`")))?,
            None => match scheme {
                S3Scheme::Https => 443,
                S3Scheme::Http => 80,
            },
        };
        let base_path = path.trim_end_matches('/').to_string();
        Ok(S3Endpoint {
            scheme,
            host: host.to_string(),
            port,
            base_path,
        })
    }

    /// The `Host` header value: the port rides along only when it is not the
    /// scheme default (it is part of the signed headers, so it must match
    /// the wire exactly).
    pub fn host_header(&self) -> String {
        let default = match self.scheme {
            S3Scheme::Https => 443,
            S3Scheme::Http => 80,
        };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Best-effort region from an AWS endpoint hostname
/// (`s3.eu-central-1.amazonaws.com`, the legacy `s3-eu-west-1` dash form,
/// dualstack included); everything else — MinIO etc. — signs as the
/// conventional `us-east-1`, which S3-compatible stores accept.
pub fn infer_region(host: &str) -> String {
    let default = "us-east-1".to_string();
    let Some(prefix) = host.strip_suffix(".amazonaws.com") else {
        return default;
    };
    // strip an optional leading "<bucket>." down to the s3 label
    let s3_part = match prefix.find(".s3") {
        Some(i) if prefix[i + 3..].is_empty() || prefix[i + 3..].starts_with(['.', '-']) => {
            &prefix[i + 1..]
        }
        _ => prefix,
    };
    let region = s3_part
        .strip_prefix("s3.")
        .or_else(|| s3_part.strip_prefix("s3-"))
        .unwrap_or("");
    let region = region.strip_prefix("dualstack.").unwrap_or(region);
    if region.is_empty() || region == "dualstack" {
        default
    } else {
        region.to_string()
    }
}

/// The full client configuration for one S3 target.
#[derive(Debug, Clone)]
pub struct S3Config {
    /// Where to connect.
    pub endpoint: S3Endpoint,
    /// SigV4 signing region.
    pub region: String,
    /// Access key id.
    pub access_key: String,
    /// Secret access key.
    pub secret_key: String,
    /// Bucket name (path-style addressed).
    pub bucket: String,
}

impl S3Config {
    /// Build a config from the persisted backup settings, validating that
    /// every needed piece is present and inferring the region from the
    /// endpoint host. Fails fast (in-actor) — before any network I/O.
    pub fn from_settings(
        endpoint: &str,
        access_key: &str,
        secret_key: &str,
        bucket: &str,
    ) -> Result<S3Config, S3Error> {
        let endpoint = S3Endpoint::parse(endpoint)?;
        if access_key.trim().is_empty() {
            return Err(S3Error::Endpoint("no access key configured".to_string()));
        }
        if secret_key.trim().is_empty() {
            return Err(S3Error::Endpoint("no secret key configured".to_string()));
        }
        let bucket = bucket.trim();
        if bucket.is_empty() {
            return Err(S3Error::Endpoint("no bucket configured".to_string()));
        }
        let region = infer_region(&endpoint.host);
        Ok(S3Config {
            endpoint,
            region,
            access_key: access_key.trim().to_string(),
            secret_key: secret_key.trim().to_string(),
            bucket: bucket.to_string(),
        })
    }
}

/// The S3 client: an [`S3Config`] plus the engine's resolved [`Dialer`].
/// One connection per request (probe/backup traffic is rare); the fail-closed
/// routing guarantee lives entirely in the dialer.
pub struct S3Client {
    config: S3Config,
    dialer: Dialer,
}

impl S3Client {
    /// A client for `config`, dialing through `dialer`.
    pub fn new(config: S3Config, dialer: Dialer) -> S3Client {
        S3Client { config, dialer }
    }

    /// The cheap authenticated probe behind the settings panel's
    /// "Test connection": `HEAD /bucket` (path-style). Success means the
    /// endpoint is reachable, TLS verified, the credentials were accepted
    /// and the bucket exists; every failure carries its honest class.
    pub async fn probe_bucket(&self) -> Result<(), S3Error> {
        let path = format!("{}/{}", self.config.endpoint.base_path, self.config.bucket);
        let resp = self.request("HEAD", &path, &[], &[]).await?;
        match resp.status {
            200..=299 => Ok(()),
            301 | 307 | 308 => Err(S3Error::Http {
                status: resp.status,
                hint: "bucket lives at another endpoint/region (redirect)".to_string(),
            }),
            400 => Err(S3Error::Http {
                status: 400,
                hint: "bad request — often a region mismatch for this endpoint".to_string(),
            }),
            401 | 403 => Err(S3Error::Http {
                status: resp.status,
                hint: "access denied — check access key and secret".to_string(),
            }),
            404 => Err(S3Error::Http {
                status: 404,
                hint: format!("bucket `{}` not found", self.config.bucket),
            }),
            s => Err(S3Error::Http {
                status: s,
                hint: "unexpected status".to_string(),
            }),
        }
    }

    /// The operation-agnostic core every S3 call goes through: SigV4-sign
    /// (`host`, `x-amz-content-sha256`, `x-amz-date`), dial through the
    /// fail-closed dialer, TLS for `https`, one HTTP exchange. `path` is the
    /// absolute, unencoded URI path; `query` are unencoded pairs. Later
    /// operations (ListObjectsV2, GET, PUT) are thin wrappers over this.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: &[u8],
    ) -> Result<HttpResponse, S3Error> {
        let cfg = &self.config;
        let path = if path.is_empty() { "/" } else { path };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| S3Error::Protocol(format!("system clock before 1970: {e}")))?
            .as_secs();
        let datetime = sigv4::amz_datetime(now);
        let payload_hash = if body.is_empty() {
            sigv4::EMPTY_PAYLOAD_SHA256.to_string()
        } else {
            hex::encode(Sha256::digest(body))
        };
        let signed_headers = vec![
            ("host".to_string(), cfg.endpoint.host_header()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), datetime.clone()),
        ];
        let auth = sigv4::authorization_header(&sigv4::SignParams {
            method,
            uri: path,
            query,
            headers: &signed_headers,
            payload_hash: &payload_hash,
            datetime: &datetime,
            region: &cfg.region,
            service: "s3",
            access_key: &cfg.access_key,
            secret_key: &cfg.secret_key,
        });
        let mut wire_headers = signed_headers;
        wire_headers.push(("Authorization".to_string(), auth));

        // the wire path/query must be byte-identical to what was signed
        let mut path_and_query = sigv4::uri_encode(path, false);
        let cq = sigv4::canonical_query(query);
        if !cq.is_empty() {
            path_and_query.push('?');
            path_and_query.push_str(&cq);
        }

        let stream = self
            .dialer
            .dial_host(&cfg.endpoint.host, cfg.endpoint.port)
            .await
            .map_err(|e| S3Error::Connect(e.to_string()))?;
        match cfg.endpoint.scheme {
            S3Scheme::Https => {
                let connector = TlsConnector::from(public_tls_config()?);
                let sni = ServerName::try_from(cfg.endpoint.host.clone())
                    .map_err(|_| S3Error::Endpoint(format!("bad host `{}`", cfg.endpoint.host)))?;
                let mut tls = timeout(TLS_HANDSHAKE_TIMEOUT, connector.connect(sni, stream))
                    .await
                    .map_err(|_| S3Error::Tls("handshake timed out".to_string()))?
                    .map_err(|e| S3Error::Tls(e.to_string()))?;
                http::roundtrip(&mut tls, method, &path_and_query, &wire_headers, body).await
            }
            S3Scheme::Http => {
                let mut tcp = stream;
                http::roundtrip(&mut tcp, method, &path_and_query, &wire_headers, body).await
            }
        }
    }
}

/// A rustls config verifying against the public WebPKI (`webpki-roots`).
/// Same pure-Rust provider posture as the SMP client, and the same alpha-
/// provider constraint: TLS 1.3 with X25519 (see `smp/tls.rs::pinned_config`
/// for why offering more makes strict servers abort).
fn public_tls_config() -> Result<Arc<ClientConfig>, S3Error> {
    let mut base = rustls_rustcrypto::provider();
    base.kx_groups
        .retain(|g| g.name() == rustls::NamedGroup::X25519);
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder_with_provider(Arc::new(base))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| S3Error::Tls(format!("rustls provider: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parse_covers_the_shapes() {
        let e = S3Endpoint::parse("https://s3.eu-central-1.amazonaws.com").expect("aws");
        assert_eq!(e.scheme, S3Scheme::Https);
        assert_eq!(e.host, "s3.eu-central-1.amazonaws.com");
        assert_eq!(e.port, 443);
        assert_eq!(e.base_path, "");
        assert_eq!(e.host_header(), "s3.eu-central-1.amazonaws.com");

        let e = S3Endpoint::parse("http://minio.local:9000").expect("minio");
        assert_eq!(e.scheme, S3Scheme::Http);
        assert_eq!(e.port, 9000);
        assert_eq!(e.host_header(), "minio.local:9000");

        // no scheme defaults to https (never a plaintext downgrade)
        let e = S3Endpoint::parse("s3.example.org").expect("bare");
        assert_eq!(e.scheme, S3Scheme::Https);
        assert_eq!(e.port, 443);

        // reverse-proxy prefix is kept, normalized without the trailing /
        let e = S3Endpoint::parse("https://gate.example.org/s3/").expect("prefix");
        assert_eq!(e.base_path, "/s3");

        // onion endpoints ride plain http (Tor is the transport encryption)
        let e = S3Endpoint::parse("http://abcdefexample.onion").expect("onion");
        assert_eq!(e.scheme, S3Scheme::Http);
        assert_eq!(e.port, 80);

        assert!(S3Endpoint::parse("").is_err());
        assert!(S3Endpoint::parse("ftp://host").is_err());
        assert!(S3Endpoint::parse("https://host:notaport").is_err());
        assert!(S3Endpoint::parse("https://").is_err());
    }

    #[test]
    fn region_inference_covers_aws_shapes_and_defaults() {
        assert_eq!(infer_region("s3.eu-central-1.amazonaws.com"), "eu-central-1");
        assert_eq!(infer_region("s3-eu-west-1.amazonaws.com"), "eu-west-1");
        assert_eq!(infer_region("s3.dualstack.ap-south-1.amazonaws.com"), "ap-south-1");
        assert_eq!(infer_region("s3.amazonaws.com"), "us-east-1");
        assert_eq!(infer_region("bucket.s3.us-west-2.amazonaws.com"), "us-west-2");
        assert_eq!(infer_region("minio.local"), "us-east-1");
        assert_eq!(infer_region("storage.example.onion"), "us-east-1");
    }

    #[test]
    fn from_settings_fails_fast_on_missing_pieces() {
        let ok = S3Config::from_settings("https://s3.amazonaws.com", "AK", "SK", "b");
        assert!(ok.is_ok());
        for (e, a, s, b) in [
            ("", "AK", "SK", "b"),
            ("https://s3.amazonaws.com", "", "SK", "b"),
            ("https://s3.amazonaws.com", "AK", "", "b"),
            ("https://s3.amazonaws.com", "AK", "SK", " "),
        ] {
            assert!(
                matches!(S3Config::from_settings(e, a, s, b), Err(S3Error::Endpoint(_))),
                "expected fail-fast for ({e:?},{a:?},{s:?},{b:?})"
            );
        }
    }
}
