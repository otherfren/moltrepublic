// SPDX-License-Identifier: GPL-3.0-or-later

//! SMP server addressing (transport concept §3.1).
//!
//! `smp://<fingerprint>@<host>[:5223]` — `fingerprint` pins the server's
//! **offline CA certificate** (base64url of its SHA-256 DER). TLS is
//! verified against that pin only, never WebPKI (a public CA is
//! irrelevant here and OCSP would leak metadata).

use crate::NetError;

/// The default SMP port (SimpleX standard).
pub const SMP_PORT: u16 = 5223;

/// A parsed `smp://fp@host[:port]` server address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmpServer {
    /// The pinned offline-CA fingerprint, base64url no-pad (SHA-256 of the
    /// CA cert DER), kept in its original text form for display + re-render.
    pub fingerprint: String,
    /// Host (clearnet or `.onion`).
    pub host: String,
    /// Port (defaults to [`SMP_PORT`]).
    pub port: u16,
}

impl SmpServer {
    /// Parse `smp://<fingerprint>@<host>[:<port>]`. The fingerprint must be
    /// valid base64url decoding to 32 bytes (a SHA-256).
    pub fn parse(s: &str) -> Result<SmpServer, NetError> {
        let rest = s
            .trim()
            .strip_prefix("smp://")
            .ok_or_else(|| NetError::Framing("smp server must start with smp://".into()))?;
        let (fingerprint, hostport) = rest
            .split_once('@')
            .ok_or_else(|| NetError::Framing("smp server missing @host".into()))?;
        if fingerprint.is_empty() {
            return Err(NetError::Framing("smp server has empty fingerprint".into()));
        }
        // validate the fingerprint decodes to 32 bytes
        let raw = fingerprint_bytes(fingerprint)?;
        if raw.len() != 32 {
            return Err(NetError::Framing(format!(
                "smp fingerprint must be a 32-byte SHA-256, got {} bytes",
                raw.len()
            )));
        }
        let (host, port) = match hostport.rsplit_once(':') {
            // an ipv6 or onion without a port has no ':contains-digits' tail;
            // only treat a trailing numeric segment as a port
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| NetError::Framing("bad smp port".into()))?,
            ),
            _ => (hostport.to_string(), SMP_PORT),
        };
        if host.is_empty() {
            return Err(NetError::Framing("smp server has empty host".into()));
        }
        Ok(SmpServer {
            fingerprint: fingerprint.to_string(),
            host,
            port,
        })
    }

    /// Re-render to `smp://fp@host[:port]` (port elided when default).
    pub fn render(&self) -> String {
        if self.port == SMP_PORT {
            format!("smp://{}@{}", self.fingerprint, self.host)
        } else {
            format!("smp://{}@{}:{}", self.fingerprint, self.host, self.port)
        }
    }

    /// `host:port`, for dialing.
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The pinned fingerprint as raw 32 bytes.
    pub fn fingerprint_raw(&self) -> [u8; 32] {
        // validated at parse; re-decode defensively
        let mut out = [0u8; 32];
        if let Ok(v) = fingerprint_bytes(&self.fingerprint) {
            if v.len() == 32 {
                out.copy_from_slice(&v);
            }
        }
        out
    }
}

/// Decode a SimpleX fingerprint: base64url (SimpleX uses the standard
/// base64 alphabet with `+/` and `=` padding for KeyHash, but also accepts
/// url-safe). Try url-safe (no-pad) first, then standard.
pub(crate) fn fingerprint_bytes(fp: &str) -> Result<Vec<u8>, NetError> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE
        .decode(fp)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(fp))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(fp))
        .map_err(|e| NetError::Framing(format!("bad smp fingerprint base64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KONKIN: &str =
        "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";

    #[test]
    fn parses_konkin_server() {
        let s = SmpServer::parse(KONKIN).expect("parse");
        assert_eq!(s.host, "smp.konkin.io");
        assert_eq!(s.port, SMP_PORT);
        assert_eq!(s.fingerprint_raw().len(), 32);
        // round-trips (port elided at default)
        assert_eq!(s.render(), KONKIN);
    }

    #[test]
    fn parses_explicit_port_and_onion() {
        let s = SmpServer::parse(
            "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@y3ubosvyvz4kxmifduqp6iigjckorzz46ralfwlqnu6qxzlakd3mpmqd.onion:5223",
        )
        .expect("parse onion");
        assert!(s.host.ends_with(".onion"));
        assert_eq!(s.port, 5223);
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "https://x@y",
            "smp://@host",                 // empty fingerprint
            "smp://f4nx4eK5=@",            // empty host
            "smp://notbase64!!!@host",     // bad base64
            "smp://YWJj@host",             // decodes to 3 bytes, not 32
        ] {
            assert!(SmpServer::parse(bad).is_err(), "should reject `{bad}`");
        }
    }
}
