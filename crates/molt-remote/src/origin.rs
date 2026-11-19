// SPDX-License-Identifier: GPL-3.0-or-later
//! Who may open the socket (`docs/web_gui.md` §5.3). WebSockets are not
//! covered by CORS, so the `Host` and `Origin` checks are what stands
//! between a hostile page in the same browser and the port: `Host` must
//! name loopback (a rebinding name does not), `Origin` must be the very
//! page this listener served. Both are parsed by `url`, compared as
//! parsed values.

/// A loopback authority a request named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loopback {
    host: url::Host<String>,
    port: u16,
}

/// The `Host` header as a loopback authority: `localhost`, `127.0.0.1` or
/// `[::1]`, any port (an SSH tunnel lands on another one). `None` for
/// anything else.
#[must_use]
pub fn loopback_host(host: &str) -> Option<Loopback> {
    authority("http", host)
}

/// Whether an upgrade's `Origin` is the page served under `host`: scheme
/// `http`, same host, same port. Absent and `null` are refused.
#[must_use]
pub fn origin_matches(origin: Option<&str>, host: &Loopback) -> bool {
    let Some(origin) = origin else { return false };
    let Some((scheme, rest)) = origin.split_once("://") else { return false };
    scheme == "http" && authority(scheme, rest).as_ref() == Some(host)
}

/// `text` as a bare loopback authority under `scheme`. The WHATWG parser
/// decides what the host IS; this only refuses everything that is more
/// than an authority (userinfo, a path, a query) or not loopback.
fn authority(scheme: &str, text: &str) -> Option<Loopback> {
    // nothing here may be reinterpreted as a delimiter by the parser
    if text.is_empty() || text.chars().any(|c| matches!(c, '/' | '\\' | '?' | '#' | '@') || c.is_whitespace()) {
        return None;
    }
    let url = url::Url::parse(&format!("{scheme}://{text}/")).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.path() != "/" || url.query().is_some() {
        return None;
    }
    // "localhost:" parses as the default port; a header that says so is malformed
    if text.ends_with(':') {
        return None;
    }
    let host = match url.host()? {
        url::Host::Domain(d) if d == "localhost" => url::Host::Domain(d.to_string()),
        url::Host::Ipv4(ip) if ip == std::net::Ipv4Addr::LOCALHOST => url::Host::Ipv4(ip),
        url::Host::Ipv6(ip) if ip == std::net::Ipv6Addr::LOCALHOST => url::Host::Ipv6(ip),
        _ => return None,
    };
    let port = url.port_or_known_default().filter(|p| *p != 0)?;
    // canonical spelling only: what a browser sends, never `127.1`,
    // `0x7f.0.0.1`, a percent-escape or a zero-padded port
    let name = url.host_str()?;
    let given = text.to_ascii_lowercase();
    if given != name && given != format!("{name}:{port}") {
        return None;
    }
    Some(Loopback { host, port })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(h: &str) -> Loopback {
        loopback_host(h).unwrap_or_else(|| panic!("{h} is loopback"))
    }

    #[test]
    fn loopback_names_pass_on_any_port() {
        for h in ["localhost", "localhost:4042", "127.0.0.1:4042", "[::1]:4042", "127.0.0.1", "LOCALHOST:9000"] {
            assert!(loopback_host(h).is_some(), "{h}");
        }
    }

    #[test]
    fn everything_else_is_not_loopback() {
        for h in [
            "",
            "evil.example",
            "evil.example:4042",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "evil.example@127.0.0.1",
            "user@localhost",
            "127.0.0.1:4042/x",
            "127.0.0.1\\@evil.example",
            "localhost.",
            "127.0.0.2",
            "192.168.1.5:4042",
            "[::2]:4042",
            "localhost:99999",
            "localhost:",
            " localhost",
            "local host",
            "127.0.0.1#x",
            "127.0.0.1?x",
            "127.1",
            "2130706433",
            "0x7f.0.0.1",
            "0177.0.0.1",
            "127.0.0.1.",
            "127.000.000.001",
            "%6cocalhost",
            "localhost:080",
            "localhost:0",
            "[0:0:0:0:0:0:0:1]:1",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(loopback_host(h).is_none(), "{h:?}");
        }
    }

    #[test]
    fn the_origin_must_be_the_served_page() {
        let h = host("127.0.0.1:4042");
        assert!(origin_matches(Some("http://127.0.0.1:4042"), &h));
        // a tunnel on another local port: Host and Origin agree on it
        assert!(origin_matches(Some("http://localhost:9000"), &host("localhost:9000")));
        assert!(origin_matches(Some("http://localhost"), &host("localhost")));
        assert!(origin_matches(Some("http://localhost"), &host("localhost:80")));
        assert!(origin_matches(Some("http://[::1]:4042"), &host("[::1]:4042")));
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        let h = host("127.0.0.1:4042");
        for o in [
            None,
            Some("null"),
            Some(""),
            Some("http://evil.example"),
            Some("http://evil.example:4042"),
            Some("http://127.0.0.1:4043"),
            Some("http://127.0.0.1"),
            Some("http://localhost:4042"),
            Some("https://127.0.0.1:4042"),
            Some("http://127.0.0.1.evil.example:4042"),
            Some("http://127.0.0.1:4042@evil.example"),
            Some("http://evil.example@127.0.0.1:4042"),
            Some("http://127.0.0.1:4042/path"),
            Some("file://127.0.0.1:4042"),
            Some("127.0.0.1:4042"),
            Some("http://127.1:4042"),
            Some("http://%3127.0.0.1:4042"),
            Some("http://127.0.0.1:04042"),
        ] {
            assert!(!origin_matches(o, &h), "{o:?}");
        }
    }
}
