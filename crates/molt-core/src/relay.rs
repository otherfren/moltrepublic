// SPDX-License-Identifier: GPL-3.0-or-later

//! The Nostr relay pool and its dial policy (`docs/transport/relay_pool.md`).
//!
//! **Nothing is pre-trusted.** MoltRepublic ships with an empty pool: the app
//! connects to no relay until its operator has named one and confirmed it. A
//! default relay list would be a default surveillance point and would make
//! every node identifiable by its first outbound packet.
//!
//! This module is the ONE place the policy lives — pure, no I/O, so the
//! runtime cannot grow a second opinion about what it may dial.

use serde::{Deserialize, Serialize};

/// One relay in the pool. The list's ORDER is the priority (position 0 is
/// tried first); the kind is never stored — see [`RelayKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayEntry {
    /// The validated, normalized relay URL (`wss://…`, or `ws://…` for onion).
    pub url: String,
    /// The user's persisted "yes, use this relay". Never set by the app
    /// itself — an unconfirmed relay is never dialed.
    #[serde(default)]
    pub confirmed: bool,
}

/// Where a relay lives, **derived from its URL, never stored**: a stored kind
/// would be a second source of truth, and a hand-edited config claiming a
/// clearnet relay is onion would walk straight through the clearnet gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelayKind {
    /// A Tor onion service — reachable without touching the clearnet, so it
    /// may be dialed automatically in the background.
    Onion,
    /// A public clearnet relay: its operator sees the node's subscriptions
    /// and, without Tor, its IP address. Never dialed automatically.
    Clearnet,
}

/// Why a relay is not currently dialable — the honest per-entry reason the
/// GUI and MCP show instead of a silent omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayBlock {
    /// The user has not confirmed this relay yet.
    Unconfirmed,
    /// A confirmed clearnet relay that this session has not activated. The
    /// activation deliberately does not survive a restart.
    ClearnetSessionLocked,
}

/// What is wrong with a relay URL. Rejected at ingest so a malformed or
/// unsafe URL can never reach the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayUrlError {
    /// Not `wss://` or `ws://`.
    Scheme,
    /// No host between the scheme and the path.
    Host,
    /// Plaintext `ws://` to a clearnet host — that publishes every
    /// subscription to anyone on the path. Allowed for onion only, where the
    /// Tor circuit already encrypts and authenticates.
    PlaintextClearnet,
    /// Whitespace or control characters in the URL.
    Junk,
    /// A host ending in `.onion` that is not a real v3 onion address (56
    /// base32 characters). Refused rather than quietly demoted to clearnet:
    /// the address cannot resolve anywhere, so a clearnet badge and an
    /// IP-exposure warning would both be lies.
    OnionAddress,
}

impl core::fmt::Display for RelayUrlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scheme => f.write_str("a relay URL must start with wss:// (or ws:// for .onion)"),
            Self::Host => f.write_str("the relay URL has no host"),
            Self::PlaintextClearnet => f.write_str(
                "ws:// is plaintext and only allowed for .onion relays — use wss:// here",
            ),
            Self::Junk => {
                f.write_str("the relay URL contains whitespace or control characters")
            }
            Self::OnionAddress => f.write_str(
                "not a valid onion address — a v3 onion is 56 characters (a-z, 2-7) before .onion",
            ),
        }
    }
}

/// Validate and NORMALIZE a relay URL: lowercase scheme+host, no trailing
/// slash, so two spellings of one relay cannot enter the pool as two entries.
/// The path/query is preserved as typed (relays may live under a path).
pub fn normalize_relay_url(raw: &str) -> Result<String, RelayUrlError> {
    let trimmed = raw.trim();
    // A backslash has no place in a relay URL, and WHATWG rewrites it to `/`
    // for ws/wss — so a stored URL containing one would not be the URL that
    // gets dialed (and two spellings of one target could sit in the pool as
    // two entries). Refuse it outright rather than store an ambiguity.
    if trimmed
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(RelayUrlError::Junk);
    }
    // ASCII-only from here on, so lowercasing cannot change byte offsets and
    // the authority cannot carry homoglyphs or IDN forms that a real parser
    // would resolve differently
    if !trimmed.is_ascii() {
        return Err(RelayUrlError::Junk);
    }
    let lower = trimmed.to_ascii_lowercase();
    let (scheme, rest) = if let Some(rest) = lower.strip_prefix("wss://") {
        ("wss://", rest)
    } else if let Some(rest) = lower.strip_prefix("ws://") {
        ("ws://", rest)
    } else {
        return Err(RelayUrlError::Scheme);
    };
    let authority_end = rest.find(AUTHORITY_END).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = valid_host(authority).ok_or(RelayUrlError::Host)?;
    // the path/query keeps the user's spelling; only a trailing slash is
    // dropped so "…/" and "…" are one entry
    let tail = lower
        .get(scheme.len() + authority_end..)
        .unwrap_or("")
        .trim_end_matches('/');
    // a name that CLAIMS .onion must actually be one; otherwise it would be
    // stored as a clearnet relay that can never resolve
    let kind = kind_of_host(host);
    if kind != RelayKind::Onion && host.rsplit('.').next() == Some("onion") {
        return Err(RelayUrlError::OnionAddress);
    }
    if scheme == "ws://" && kind != RelayKind::Onion {
        return Err(RelayUrlError::PlaintextClearnet);
    }
    Ok(format!("{scheme}{authority}{tail}"))
}

/// Where the authority ends. `\` is in here because WHATWG treats it exactly
/// like `/` for the special schemes ws/wss — leaving it out is what let
/// `wss://evil.example.org\x.onion` read as an onion host here while every
/// real client dialed `evil.example.org`.
const AUTHORITY_END: [char; 4] = ['/', '\\', '?', '#'];

/// The kind of a relay URL — derived, never stored.
///
/// This is the security-critical direction: `Onion` means "may be dialed with
/// no user interaction at all". It is therefore **independently strict** and
/// does not assume its input passed [`normalize_relay_url`] — a hand-edited
/// `config.toml` reaches the pool without ingest validation. Anything it
/// cannot parse as a plain, well-formed onion authority counts as
/// [`RelayKind::Clearnet`]: the side of the gate that asks the user first.
pub fn relay_kind(url: &str) -> RelayKind {
    let Some(rest) = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
    else {
        return RelayKind::Clearnet;
    };
    let authority_end = rest.find(AUTHORITY_END).unwrap_or(rest.len());
    match valid_host(&rest[..authority_end]) {
        Some(host) => kind_of_host(host),
        None => RelayKind::Clearnet,
    }
}

/// Validate an authority (`host` or `host:port`) by ALLOW-LIST and return the
/// bare host. A blacklist of delimiters is not enough here: every character a
/// real URL parser treats specially (`@` userinfo, `\` authority end, `%`
/// escapes, `[` `]` IP literals, non-ASCII IDN) must be refused, or our host
/// and the dialer's host can differ — and a differing host defeats the whole
/// onion/clearnet gate. Accepts letters, digits, `-` and `.` in the host, plus
/// an optional numeric port; rejects empty labels and a trailing dot so one
/// host has exactly one spelling.
fn valid_host(authority: &str) -> Option<&str> {
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if let Some(port) = port {
        // Digits only, then a real u16 range check. Neither half alone is
        // enough: `str::parse` happily accepts a leading `+`, and "at most
        // five digits" lets 65536..=99999 through — both are rejected by a
        // real URL parser, so a relay we stored would be badged dialable and
        // never connect. A leading zero is refused too, so one port has one
        // spelling (`url` would normalize `0443` to `443`, we store verbatim).
        if !port.bytes().all(|b| b.is_ascii_digit())
            || port.starts_with('0')
            || port.parse::<u16>().is_err()
        {
            return None;
        }
    }
    if host.is_empty() || host.ends_with('.') {
        return None;
    }
    for label in host.split('.') {
        if label.is_empty() {
            return None;
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
    }
    Some(host)
}

/// `Onion` — and with it the right to be dialed with no user interaction —
/// is granted only to a syntactically real **v3 onion address**: 56 base32
/// characters (`a-z2-7`) before the `.onion` label. Merely *ending* in
/// ".onion" is not enough; a name Tor could never resolve has no business
/// skipping the clearnet warning, and requiring the real shape removes a
/// whole class of "looks onion, resolves elsewhere" tricks. Retired v2
/// addresses (16 chars) do not qualify — Tor dropped them.
fn kind_of_host(host: &str) -> RelayKind {
    const V3_LEN: usize = 56;
    let mut labels = host.rsplit('.');
    if labels.next() != Some("onion") {
        return RelayKind::Clearnet;
    }
    match labels.next() {
        Some(addr)
            if addr.len() == V3_LEN
                && addr
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)) =>
        {
            RelayKind::Onion
        }
        _ => RelayKind::Clearnet,
    }
}

/// One relay as the surfaces SEE it: the stored fields plus everything the
/// policy derives, so a GUI or an MCP agent never has to re-implement the
/// rules to explain why a relay is or is not in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayStatus {
    /// The relay URL (normalized).
    pub url: String,
    /// Derived from the URL — onion or clearnet.
    pub kind: RelayKind,
    /// The user's persisted confirmation.
    pub confirmed: bool,
    /// `None` = this relay is dialable right now; otherwise the honest reason
    /// it is not.
    pub blocked: Option<RelayBlock>,
}

/// Bring a pool that did NOT come through the command surface into the same
/// shape ingest guarantees: normalize every URL, drop what fails validation,
/// and collapse duplicates (first wins, so the file order — the dial priority
/// — survives).
///
/// The `config.toml` path is exactly that case: an operator may hand-write
/// relays, and nothing there ran [`normalize_relay_url`]. Without this, an
/// unvalidated string would sit in the pool, be shown as a relay, be handed to
/// the runtime, and — because the `Relay*` commands address entries by their
/// NORMALIZED url — could not even be confirmed, moved or removed again.
pub fn sanitize_pool(raw: &[RelayEntry]) -> Vec<RelayEntry> {
    let mut out: Vec<RelayEntry> = Vec::with_capacity(raw.len());
    for entry in raw {
        let Ok(url) = normalize_relay_url(&entry.url) else {
            continue;
        };
        if out.iter().any(|kept| kept.url == url) {
            continue;
        }
        out.push(RelayEntry { url, confirmed: entry.confirmed });
    }
    out
}

/// The pool as the surfaces see it, in priority order.
pub fn pool_status(pool: &[RelayEntry], clearnet_session: bool) -> Vec<RelayStatus> {
    pool.iter()
        .map(|e| RelayStatus {
            url: e.url.clone(),
            kind: relay_kind(&e.url),
            confirmed: e.confirmed,
            blocked: relay_block(e, clearnet_session),
        })
        .collect()
}

/// Why this entry may not be dialed right now — `None` means it is dialable.
///
/// `clearnet_session` is the IN-SESSION activation of clearnet dialing; it
/// resets to `false` on every start, which is what makes "always an explicit
/// confirmation before a clearnet connection" true across restarts.
pub fn relay_block(entry: &RelayEntry, clearnet_session: bool) -> Option<RelayBlock> {
    if !entry.confirmed {
        return Some(RelayBlock::Unconfirmed);
    }
    if relay_kind(&entry.url) == RelayKind::Clearnet && !clearnet_session {
        return Some(RelayBlock::ClearnetSessionLocked);
    }
    None
}

/// The relays the runtime may dial, in priority order — the ONLY sanctioned
/// source of that list. An empty result means "connect to nothing", which is
/// exactly what a fresh install must do.
pub fn dialable(pool: &[RelayEntry], clearnet_session: bool) -> Vec<String> {
    pool.iter()
        .filter(|e| relay_block(e, clearnet_session).is_none())
        .map(|e| e.url.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(url: &str, confirmed: bool) -> RelayEntry {
        RelayEntry { url: url.to_string(), confirmed }
    }

    /// KEYSTONE — a fresh install connects to NOTHING. No hard-coded relay,
    /// and an unconfirmed one is not a relay the app may use.
    #[test]
    fn an_empty_or_unconfirmed_pool_dials_nothing() {
        assert!(dialable(&[], false).is_empty(), "a fresh install is offline");
        assert!(dialable(&[], true).is_empty(), "…even with clearnet unlocked");
        let pool = vec![
            entry("wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion", false),
            entry("wss://relay.example.org", false),
        ];
        assert!(
            dialable(&pool, true).is_empty(),
            "an unconfirmed relay is never dialed, whatever its kind"
        );
        assert_eq!(relay_block(&pool[0], true), Some(RelayBlock::Unconfirmed));
    }

    /// KEYSTONE — onion connects by itself, clearnet never does. The clearnet
    /// activation is per-session, so a restart re-arms the gate.
    #[test]
    fn onion_dials_automatically_but_clearnet_needs_the_session_unlock() {
        let onion = entry("wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion", true);
        let clearnet = entry("wss://relay.example.org", true);
        // background/startup: no session unlock
        assert_eq!(relay_block(&onion, false), None, "onion is background-dialable");
        assert_eq!(
            relay_block(&clearnet, false),
            Some(RelayBlock::ClearnetSessionLocked),
            "a confirmed clearnet relay is still NOT dialed automatically"
        );
        let pool = vec![onion.clone(), clearnet.clone()];
        assert_eq!(dialable(&pool, false), vec!["wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion"]);
        // the user actively unlocks clearnet for this session
        assert_eq!(relay_block(&clearnet, true), None);
        assert_eq!(
            dialable(&pool, true),
            vec!["wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion", "wss://relay.example.org"],
            "priority order is the pool order"
        );
    }

    #[test]
    fn priority_is_the_pool_order() {
        let pool = vec![
            entry("wss://second.example.org", true),
            entry("wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion", true),
            entry("wss://third.example.org", true),
        ];
        assert_eq!(
            dialable(&pool, true),
            vec![
                "wss://second.example.org",
                "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion",
                "wss://third.example.org"
            ],
            "the list order IS the priority — never re-sorted by kind or name"
        );
    }

    #[test]
    fn the_kind_is_derived_from_the_url() {
        assert_eq!(relay_kind("wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion"), RelayKind::Onion);
        assert_eq!(relay_kind("ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion:8080"), RelayKind::Onion);
        assert_eq!(relay_kind("wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion/nostr"), RelayKind::Onion);
        assert_eq!(relay_kind("wss://relay.example.org"), RelayKind::Clearnet);
        assert_eq!(relay_kind("wss://relay.example.org:443/x"), RelayKind::Clearnet);
        // a host merely CONTAINING ".onion" is not an onion service
        assert_eq!(relay_kind("wss://not.onion.example.org"), RelayKind::Clearnet);
        // unparseable → the safe side of the gate
        assert_eq!(relay_kind("garbage"), RelayKind::Clearnet);
    }

    /// KEYSTONE — the onion classification decides whether something is dialed
    /// WITHOUT asking the user, so it must never disagree with the parser that
    /// actually dials. Our host extraction used to end the authority at
    /// `/ ? #` only and to treat everything before the last `:` as the host,
    /// while WHATWG (what every WebSocket/Nostr client uses) also ends it at a
    /// backslash and strips `userinfo@`. Both disagreements pointed the unsafe
    /// way: `wss://evil.com\x.onion` and `wss://a.onion:1@evil.com` dial
    /// `evil.com` but classified as Onion — auto-dialed, no warning, green
    /// badge. Every spoof below must be REFUSED at ingest, and `relay_kind`
    /// must independently refuse to call any of them Onion (the config file
    /// bypasses ingest, so the classifier has to stand on its own).
    #[test]
    fn onion_classification_cannot_be_spoofed() {
        for spoof in [
            "wss://evil.example.org\\x.onion",       // WHATWG ends the host at \
            "wss://evil.example.org\\\\x.onion",
            "ws://evil.example.org\\x.onion",        // …also defeating the plaintext rule
            "wss://abcd.onion:1234@attacker.example.org", // userinfo read as host
            "wss://abcd.onion@attacker.example.org",
            "ws://abcd.onion:1@evil.example.org",
            "wss://user:pass@evil.example.org",      // userinfo at all
            "wss://a..onion",                        // empty label
            "wss://.onion",                          // no name
            "wss://x.onion.",                        // trailing dot: one spelling only
            "wss://%78.onion",                       // percent-encoding
            "wss://xn--x.onion",                     // punycode is not a v3 onion
            "wss://[::1]:443",                       // IP literal
            "wss://evil.example.org:80\\x.onion",
        ] {
            assert!(
                normalize_relay_url(spoof).is_err(),
                "ingest must refuse {spoof:?}"
            );
            assert_ne!(
                relay_kind(spoof),
                RelayKind::Onion,
                "the classifier must never call {spoof:?} onion"
            );
        }
        // punycode/IDN and unicode never pass as a host either. The Turkish
        // dotted capital I is the classic byte-length trap: `to_lowercase`
        // turns it into TWO chars, so any offset computed on the lowercased
        // string would slice the original one byte off.
        for junk in [
            "wss://ex\u{0430}mple.onion",
            "wss://exam\u{200b}ple.onion",
            "wss://\u{0130}stanbul.example.org",
        ] {
            assert!(normalize_relay_url(junk).is_err(), "must refuse {junk:?}");
            assert_ne!(relay_kind(junk), RelayKind::Onion);
        }
        // …while a real v3 onion address still works, with or without port/path
        for good in [
            "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion",
            "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion:8080",
            "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion/nostr",
            "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion",
        ] {
            let n = normalize_relay_url(good).unwrap_or_else(|e| panic!("{good:?}: {e}"));
            assert_eq!(relay_kind(&n), RelayKind::Onion, "{good:?}");
        }
    }

    /// KEYSTONE — `config.toml` is hand-editable and never ran ingest, so the
    /// pool it produces is sanitized on the way in: unusable entries are
    /// dropped rather than displayed as relays, spellings are normalized (or
    /// the `Relay*` commands, which address entries by normalized url, could
    /// not touch the row again), duplicates collapse, and the file order —
    /// the dial priority — survives.
    #[test]
    fn a_hand_written_pool_is_sanitized_on_the_way_in() {
        let onion = format!("wss://{}.onion", "a".repeat(56));
        let raw = vec![
            RelayEntry { url: "  WSS://Relay.Example.ORG/  ".to_string(), confirmed: true },
            RelayEntry { url: "not even a url".to_string(), confirmed: true },
            RelayEntry { url: "http://evil.example.org".to_string(), confirmed: true },
            RelayEntry { url: "ws://plain.example.org".to_string(), confirmed: true },
            RelayEntry { url: onion.clone(), confirmed: false },
            // the same relay again, spelled differently
            RelayEntry { url: "wss://relay.example.org".to_string(), confirmed: false },
        ];
        let clean = sanitize_pool(&raw);
        assert_eq!(
            clean,
            vec![
                RelayEntry { url: "wss://relay.example.org".to_string(), confirmed: true },
                RelayEntry { url: onion, confirmed: false },
            ],
            "invalid dropped, spellings normalized, duplicate collapsed, order kept"
        );
        // sanitizing twice changes nothing
        assert_eq!(sanitize_pool(&clean), clean);
    }

    /// The port is part of the authority, so it is validated like one: a
    /// non-numeric "port" is a parser disagreement waiting to happen.
    #[test]
    fn ports_are_validated() {
        assert!(normalize_relay_url("wss://relay.example.org:443").is_ok());
        assert!(normalize_relay_url("wss://relay.example.org:65535").is_ok());
        for bad in [
            "wss://relay.example.org:",
            "wss://relay.example.org:https",
            "wss://relay.example.org:443:443",
            "wss://relay.example.org:99999999",
            "wss://relay.example.org:-1",
            // five digits, but out of range — `url` rejects these, so a relay
            // we stored would be badged dialable and never connect
            "wss://relay.example.org:65536",
            "wss://relay.example.org:99999",
            "wss://relay.example.org:+443",
            "wss://relay.example.org:0443",
        ] {
            assert!(normalize_relay_url(bad).is_err(), "must refuse {bad:?}");
        }
    }

    #[test]
    fn urls_are_validated_and_normalized_at_ingest() {
        // normalization: one relay has one spelling in the pool
        assert_eq!(
            normalize_relay_url("  WSS://Relay.Example.ORG/  ").expect("normalizes"),
            "wss://relay.example.org"
        );
        assert_eq!(
            normalize_relay_url("wss://relay.example.org").expect("plain"),
            "wss://relay.example.org"
        );
        // a path survives (relays may live under one), the trailing slash does not
        assert_eq!(
            normalize_relay_url("wss://relay.example.org/nostr/").expect("path"),
            "wss://relay.example.org/nostr"
        );
        // plaintext is fine to an onion service (Tor encrypts + authenticates)
        assert_eq!(
            normalize_relay_url("ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion").expect("onion ws"),
            "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion"
        );
        // …and refused to the clearnet
        assert_eq!(
            normalize_relay_url("ws://relay.example.org"),
            Err(RelayUrlError::PlaintextClearnet)
        );
        // surrounding whitespace is trimmed (a pasted URL often carries a
        // trailing newline) — but anything INSIDE the URL is junk
        assert_eq!(
            normalize_relay_url("wss://relay.example.org\n").expect("pasted"),
            "wss://relay.example.org"
        );
        for (bad, want) in [
            ("https://relay.example.org", RelayUrlError::Scheme),
            ("relay.example.org", RelayUrlError::Scheme),
            ("wss://", RelayUrlError::Host),
            ("wss:///nostr", RelayUrlError::Host),
            ("wss://relay example.org", RelayUrlError::Junk),
            ("wss://relay\u{7}.example.org", RelayUrlError::Junk),
        ] {
            assert_eq!(normalize_relay_url(bad), Err(want), "must reject {bad:?}");
        }
    }
}
