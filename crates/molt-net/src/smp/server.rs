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

/// One host endpoint of a server: a clearnet or `.onion` host and its port.
/// SimpleX advertises the same server (one fingerprint) as both a clearnet
/// and an `.onion` host; the alternates ride in [`SmpServer::alt_hosts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPort {
    /// Host (clearnet or `.onion`).
    pub host: String,
    /// Port (defaults to [`SMP_PORT`]).
    pub port: u16,
}

/// A parsed `smp://fp@host[:port][,alt[:port]…]` server address. One
/// fingerprint pins the (shared) CA; the first host is the primary
/// (clearnet by convention) and any `.onion` alternate is onion-preferred
/// when Tor is on (transport concept §3.1, T4 §P4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmpServer {
    /// The pinned offline-CA fingerprint, base64url no-pad (SHA-256 of the
    /// CA cert DER), kept in its original text form for display + re-render.
    pub fingerprint: String,
    /// Primary host (clearnet or `.onion`).
    pub host: String,
    /// Primary port (defaults to [`SMP_PORT`]).
    pub port: u16,
    /// Alternate host endpoints under the same fingerprint (e.g. the server's
    /// `.onion`). Empty for a single-host URL (backward compatible).
    pub alt_hosts: Vec<HostPort>,
}

/// Split one `host[:port]` segment, treating only a trailing numeric tail as
/// the port (an `.onion`/ipv6 host without a port has none).
fn parse_host_port(seg: &str) -> Result<HostPort, NetError> {
    let (host, port) = match seg.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| NetError::Framing("bad smp port".into()))?,
        ),
        _ => (seg.to_string(), SMP_PORT),
    };
    if host.is_empty() {
        return Err(NetError::Framing("smp server has empty host".into()));
    }
    Ok(HostPort { host, port })
}

/// Render one `host[:port]` segment (port elided at the default).
fn render_host_port(host: &str, port: u16) -> String {
    if port == SMP_PORT {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

impl SmpServer {
    /// Parse `smp://<fingerprint>@<host>[:<port>][,<host2>[:<port2>]…]`. The
    /// fingerprint must be valid base64url decoding to 32 bytes (a SHA-256);
    /// the first host is the primary, any others are [`Self::alt_hosts`].
    pub fn parse(s: &str) -> Result<SmpServer, NetError> {
        let rest = s
            .trim()
            .strip_prefix("smp://")
            .ok_or_else(|| NetError::Framing("smp server must start with smp://".into()))?;
        let (fingerprint, hostlist) = rest
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
        let mut hosts = hostlist.split(',');
        let primary = parse_host_port(
            hosts
                .next()
                .ok_or_else(|| NetError::Framing("smp server has empty host".into()))?,
        )?;
        let alt_hosts = hosts.map(parse_host_port).collect::<Result<Vec<_>, _>>()?;
        Ok(SmpServer {
            fingerprint: fingerprint.to_string(),
            host: primary.host,
            port: primary.port,
            alt_hosts,
        })
    }

    /// Re-render to `smp://fp@host[:port][,alt[:port]…]` (each port elided
    /// when default). Round-trips every host so config `smp_url` and the
    /// genesis-embedded server survive.
    pub fn render(&self) -> String {
        let mut hosts = render_host_port(&self.host, self.port);
        for alt in &self.alt_hosts {
            hosts.push(',');
            hosts.push_str(&render_host_port(&alt.host, alt.port));
        }
        format!("smp://{}@{}", self.fingerprint, hosts)
    }

    /// `host:port` of the **primary** endpoint, for dialing / error messages.
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The endpoint to actually dial. With Tor on, an `.onion` host (an
    /// alternate, or the primary when it is itself onion) is preferred — no
    /// exit node, reaches onion-only servers. Otherwise the primary (a
    /// clearnet dialer has no `.onion` resolver, so it never targets onion via
    /// an alternate). The SNI/pin is unchanged — cert verification ignores the
    /// hostname (fingerprint pin).
    pub fn dial_target(&self, tor_on: bool) -> (&str, u16) {
        if tor_on {
            if let Some(hp) = self.alt_hosts.iter().find(|h| h.host.ends_with(".onion")) {
                return (&hp.host, hp.port);
            }
        }
        (&self.host, self.port)
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
    fn smp_server_parses_comma_hosts_and_prefers_onion_under_tor() {
        let url = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io,\
             y3ubosvyvz4kxmifduqp6iigjckorzz46ralfwlqnu6qxzlakd3mpmqd.onion";
        let s = SmpServer::parse(url).expect("parse comma hosts");
        assert_eq!(s.host, "smp.konkin.io");
        assert_eq!(s.alt_hosts.len(), 1);
        assert!(s.alt_hosts[0].host.ends_with(".onion"));
        // onion-preferred under Tor, clearnet primary without it
        assert_eq!(s.dial_target(true).0, s.alt_hosts[0].host);
        assert_eq!(s.dial_target(false), ("smp.konkin.io", SMP_PORT));
        // round-trips through render (both hosts survive)
        assert_eq!(s.render(), url);
        assert_eq!(SmpServer::parse(&s.render()).expect("re-parse"), s);
    }

    #[test]
    fn comma_hosts_round_trip_explicit_ports() {
        let url = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@\
             clear.example.com:5223,abcd.onion:9999";
        let s = SmpServer::parse(url).expect("parse");
        assert_eq!(s.port, SMP_PORT);
        assert_eq!(s.alt_hosts[0].port, 9999);
        assert_eq!(s.dial_target(true), ("abcd.onion", 9999));
        // the clear primary's default port is elided, the onion's kept
        assert_eq!(
            s.render(),
            "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@clear.example.com,abcd.onion:9999"
        );
    }

    #[test]
    fn direct_never_targets_onion_via_alternate() {
        // clear primary + onion alt: without Tor the target is the clearnet
        // primary — the onion is never reached by a resolver-less direct dial.
        let s = SmpServer::parse(
            "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@clear.example.com,abcd.onion",
        )
        .expect("parse");
        assert_eq!(s.dial_target(false).0, "clear.example.com");
        // an onion-only server: the primary IS the onion, so dial_target
        // surfaces it — the caller (a Direct dialer) must then reject cleanly.
        let onion_only = SmpServer::parse(
            "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@abcd.onion",
        )
        .expect("parse onion-only");
        assert!(onion_only.dial_target(false).0.ends_with(".onion"));
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
