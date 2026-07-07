// SPDX-License-Identifier: GPL-3.0-or-later

//! SOCKS5h dialing (transport concept §4 — Tor). A minimal SOCKS5 (RFC 1928)
//! client with username/password auth (RFC 1929) used for **stream isolation**:
//! Tor's `IsolateSOCKSAuth` puts each distinct auth pair on its own circuit, so
//! per-queue-host credentials mean two of our queues never share an exit/timing
//! fingerprint. `h` = the hostname is resolved **proxy-side** (address type
//! `DOMAINNAME`), so no DNS ever leaves this machine in the clear.
//!
//! The wire logic (request bytes + reply parsing) is split into pure functions
//! so the protocol is unit-tested without a live proxy; [`socks5h_connect`] is
//! the thin async glue over a `TcpStream`.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::NetError;

const VER: u8 = 0x05;
const AUTH_USERPASS: u8 = 0x02;
const AUTH_NONE: u8 = 0x00;
const AUTH_UNACCEPTABLE: u8 = 0xff;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// The method-negotiation greeting: we offer username/password (for isolation)
/// AND no-auth, so a proxy that ignores isolation still works.
fn greeting() -> [u8; 4] {
    [VER, 2, AUTH_USERPASS, AUTH_NONE]
}

/// The RFC 1929 username/password sub-negotiation request. `user`/`pass` are
/// each capped at 255 bytes (the isolation token is short by construction).
fn auth_request(user: &[u8], pass: &[u8]) -> Vec<u8> {
    let ulen = u8::try_from(user.len().min(255)).unwrap_or(255);
    let plen = u8::try_from(pass.len().min(255)).unwrap_or(255);
    let mut v = Vec::with_capacity(3 + user.len() + pass.len());
    v.push(0x01); // auth sub-negotiation version
    v.push(ulen);
    v.extend_from_slice(&user[..usize::from(ulen)]);
    v.push(plen);
    v.extend_from_slice(&pass[..usize::from(plen)]);
    v
}

/// A CONNECT request to `host:port` with `DOMAINNAME` addressing — the proxy
/// resolves the host (SOCKS5**h**), never this node.
fn connect_request(host: &str, port: u16) -> Result<Vec<u8>, NetError> {
    let host = host.as_bytes();
    if host.is_empty() || host.len() > 255 {
        return Err(NetError::Framing(format!(
            "socks: host length {} out of range",
            host.len()
        )));
    }
    let mut v = Vec::with_capacity(7 + host.len());
    v.extend_from_slice(&[VER, CMD_CONNECT, 0x00, ATYP_DOMAIN]);
    v.push(u8::try_from(host.len()).unwrap_or(0));
    v.extend_from_slice(host);
    v.extend_from_slice(&port.to_be_bytes());
    Ok(v)
}

/// Parse the server's method selection (`[VER, METHOD]`). Returns the chosen
/// method, or an error if the proxy rejected us or spoke a wrong version.
fn parse_method_selection(reply: &[u8]) -> Result<u8, NetError> {
    match reply {
        [VER, m] if *m != AUTH_UNACCEPTABLE => Ok(*m),
        [VER, _] => Err(NetError::Unreachable("socks: no acceptable auth method".into())),
        _ => Err(NetError::Framing("socks: bad method-selection reply".into())),
    }
}

/// Parse the RFC 1929 auth reply (`[VER, STATUS]`, status 0 = success).
fn parse_auth_reply(reply: &[u8]) -> Result<(), NetError> {
    match reply {
        [0x01, 0x00] => Ok(()),
        [0x01, s] => Err(NetError::Unreachable(format!("socks: auth rejected (status {s})"))),
        _ => Err(NetError::Framing("socks: bad auth reply".into())),
    }
}

/// A SOCKS5 reply code → human reason (RFC 1928 §6).
fn reply_reason(rep: u8) -> &'static str {
    match rep {
        0x01 => "general failure",
        0x02 => "connection not allowed",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown",
    }
}

/// The length of the bound-address + port that follows a reply header, by its
/// address type — so the caller reads exactly the reply and not into the tunnel.
fn bound_addr_len(atyp: u8, first_octet: u8) -> Result<usize, NetError> {
    match atyp {
        ATYP_IPV4 => Ok(4 + 2),
        ATYP_IPV6 => Ok(16 + 2),
        // domain: first octet is the length, then that many bytes + 2 port
        ATYP_DOMAIN => Ok(1 + usize::from(first_octet) + 2 - 1),
        other => Err(NetError::Framing(format!("socks: bad reply atyp {other:#x}"))),
    }
}

/// Validate a CONNECT reply header (`[VER, REP, RSV, ATYP, ...]`): `REP == 0`.
fn parse_connect_reply_header(header: &[u8]) -> Result<u8, NetError> {
    match header {
        [VER, 0x00, _rsv, atyp] => Ok(*atyp),
        [VER, rep, _, _] => Err(NetError::Unreachable(format!(
            "socks: CONNECT failed — {}",
            reply_reason(*rep)
        ))),
        _ => Err(NetError::Framing("socks: bad CONNECT reply header".into())),
    }
}

/// Dial `host:port` through the SOCKS5 proxy at `proxy` (e.g. `127.0.0.1:9050`),
/// isolating the circuit by `isolation` (used as the SOCKS username). The host
/// is resolved proxy-side (SOCKS5h). Returns the tunnelled [`TcpStream`], ready
/// for the TLS handshake to the SMP server.
pub async fn socks5h_connect(
    proxy: &str,
    host: &str,
    port: u16,
    isolation: &str,
) -> Result<TcpStream, NetError> {
    let mut s = TcpStream::connect(proxy)
        .await
        .map_err(|e| NetError::Unreachable(format!("socks proxy {proxy}: {e}")))?;

    // 1. method negotiation
    s.write_all(&greeting()).await.map_err(sock_err)?;
    let mut sel = [0u8; 2];
    s.read_exact(&mut sel).await.map_err(sock_err)?;
    let method = parse_method_selection(&sel)?;

    // 2. username/password auth if the proxy asked for it (isolation token as
    //    the username; a fixed non-empty password keeps some servers happy)
    if method == AUTH_USERPASS {
        s.write_all(&auth_request(isolation.as_bytes(), b"molt"))
            .await
            .map_err(sock_err)?;
        let mut ar = [0u8; 2];
        s.read_exact(&mut ar).await.map_err(sock_err)?;
        parse_auth_reply(&ar)?;
    }

    // 3. CONNECT request (proxy-side DNS)
    s.write_all(&connect_request(host, port)?).await.map_err(sock_err)?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head).await.map_err(sock_err)?;
    let atyp = parse_connect_reply_header(&head)?;
    // drain the bound address + port so the stream is positioned at the tunnel
    let mut first = [0u8; 1];
    let addr_tail = if atyp == ATYP_DOMAIN {
        s.read_exact(&mut first).await.map_err(sock_err)?;
        bound_addr_len(atyp, first[0])?
    } else {
        bound_addr_len(atyp, 0)?
    };
    let mut drain = vec![0u8; addr_tail];
    s.read_exact(&mut drain).await.map_err(sock_err)?;
    Ok(s)
}

fn sock_err(e: std::io::Error) -> NetError {
    NetError::Unreachable(format!("socks io: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_maps_tor_mode_to_a_dialer() {
        use crate::smp::tls::Dialer;
        assert!(matches!(
            Dialer::from_config("local", 9150),
            Dialer::Socks5 { proxy } if proxy == "127.0.0.1:9150"
        ));
        assert!(matches!(Dialer::from_config("whonix", 9050), Dialer::Socks5 { .. }));
        // off / direct / unknown / arti-later all dial direct
        for m in ["off", "direct", "embedded", "", "nonsense"] {
            assert!(matches!(Dialer::from_config(m, 9050), Dialer::Direct), "{m}");
        }
    }

    #[test]
    fn greeting_offers_userpass_then_none() {
        assert_eq!(greeting(), [0x05, 2, 0x02, 0x00]);
    }

    #[test]
    fn auth_request_frames_user_and_pass() {
        assert_eq!(
            auth_request(b"molt-abc", b"molt"),
            [0x01, 8, b'm', b'o', b'l', b't', b'-', b'a', b'b', b'c', 4, b'm', b'o', b'l', b't']
        );
    }

    #[test]
    fn connect_request_is_domain_addressed_with_be_port() {
        let req = connect_request("smp.example.onion", 5223).expect("req");
        assert_eq!(req[..4], [0x05, 0x01, 0x00, 0x03]);
        assert_eq!(usize::from(req[4]), "smp.example.onion".len());
        assert_eq!(&req[5..5 + 17], b"smp.example.onion");
        assert_eq!(&req[req.len() - 2..], &5223u16.to_be_bytes());
    }

    #[test]
    fn connect_request_rejects_oversized_or_empty_host() {
        assert!(connect_request("", 1).is_err());
        assert!(connect_request(&"x".repeat(256), 1).is_err());
    }

    #[test]
    fn method_selection_accepts_or_rejects() {
        assert_eq!(parse_method_selection(&[0x05, 0x00]).expect("none"), 0x00);
        assert_eq!(parse_method_selection(&[0x05, 0x02]).expect("userpass"), 0x02);
        assert!(parse_method_selection(&[0x05, 0xff]).is_err()); // no acceptable method
        assert!(parse_method_selection(&[0x04, 0x00]).is_err()); // wrong version
        assert!(parse_method_selection(&[0x05]).is_err()); // truncated
    }

    #[test]
    fn auth_reply_status_gate() {
        assert!(parse_auth_reply(&[0x01, 0x00]).is_ok());
        assert!(parse_auth_reply(&[0x01, 0x01]).is_err());
        assert!(parse_auth_reply(&[0x00, 0x00]).is_err());
    }

    #[test]
    fn connect_reply_header_and_bound_len() {
        assert_eq!(parse_connect_reply_header(&[0x05, 0x00, 0x00, ATYP_IPV4]).expect("ok"), ATYP_IPV4);
        // failure code maps to a reason, not a panic
        let e = parse_connect_reply_header(&[0x05, 0x05, 0x00, 0x01]).expect_err("failure code");
        assert!(format!("{e}").contains("refused"));
        // bound address + port lengths
        assert_eq!(bound_addr_len(ATYP_IPV4, 0).expect("v4"), 6);
        assert_eq!(bound_addr_len(ATYP_IPV6, 0).expect("v6"), 18);
        assert_eq!(bound_addr_len(ATYP_DOMAIN, 9).expect("domain"), 9 + 2); // 1 len byte already read
        assert!(bound_addr_len(0x09, 0).is_err());
    }
}
