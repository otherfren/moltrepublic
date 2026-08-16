// SPDX-License-Identifier: GPL-3.0-or-later

//! The Nostr relay pool and its dial policy (`docs_archive/transport/relay_pool.md`).
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
    /// The validated, normalized relay URL (`wss://…`; `ws://…` only for
    /// onion and local addresses).
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
    /// A relay on a loopback, RFC1918-private, link-local or unique-local
    /// address (or `localhost`) — a self-hosted relay on the operator's own
    /// machine or network. Reached DIRECTLY, never over Tor, so it rides the
    /// same explicit gate as clearnet (§10.14, decided 2026-07-31): confirm
    /// with the exposure acknowledgement, which also switches non-onion
    /// dialing on and remembers it — never a silent dial.
    Local,
}

/// Why a relay is not currently dialable — the honest per-entry reason the
/// GUI and MCP show instead of a silent omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayBlock {
    /// The user has not confirmed this relay yet.
    Unconfirmed,
    /// A confirmed non-onion relay (clearnet or local — both are reached
    /// outside Tor) while this node does not dial outside Tor at all.
    ///
    /// The name is historical: the activation used to be session-only and
    /// reset on every start. Since the ADR-0004 amendment (2026-08-01) the
    /// decision is REMEMBERED in both directions — the variant is kept
    /// (surfaces serialize it as `clearnet_session_locked`, and MCP agents
    /// read that string) but it now means "the switch is off", not "the
    /// switch has not been flipped this session".
    ClearnetSessionLocked,
}

/// What is wrong with a relay URL. Rejected at ingest so a malformed or
/// unsafe URL can never reach the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayUrlError {
    /// Not `wss://` or `ws://`.
    Scheme,
    /// No usable host between the scheme and the path (also: an address
    /// nothing can listen on — unspecified, broadcast, multicast).
    Host,
    /// Plaintext `ws://` to a clearnet host — that publishes every
    /// subscription to anyone on the path. Allowed for onion (the Tor
    /// circuit already encrypts and authenticates) and for local/private
    /// addresses (no CA certifies those; exposure ends at the local path).
    PlaintextClearnet,
    /// Whitespace or control characters in the URL.
    Junk,
    /// A host ending in `.onion` that is not a real v3 onion address (56
    /// base32 characters). Refused rather than quietly demoted to clearnet:
    /// the address cannot resolve anywhere, so a clearnet badge and an
    /// IP-exposure warning would both be lies.
    OnionAddress,
    /// Credentials (`user[:pass]@`) in the URL — never part of a relay
    /// address, and historically the exact spelling that made a clearnet
    /// host read as something else.
    Userinfo,
    /// A `#fragment` — meaningless in a WebSocket URL, refused like MDK's
    /// validator does.
    Fragment,
    /// Longer than [`MAX_URL_LEN`] bytes.
    TooLong,
    /// The spelling is not what the WHATWG parser would dial: a percent
    /// escape, an alternate IPv4 notation (`0x7f.1`, plain integer, octal,
    /// leading zeros), an odd port spelling. Every real client dials the
    /// REWRITTEN form, so storing the original would keep two readings of
    /// one address. Write it canonically instead.
    NonCanonical,
}

impl core::fmt::Display for RelayUrlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scheme => f.write_str(
                "a relay URL must start with wss:// (or ws:// for .onion and local addresses)",
            ),
            Self::Host => f.write_str("the relay URL has no usable host"),
            Self::PlaintextClearnet => f.write_str(
                "ws:// is plaintext and only allowed for .onion or local relays — use wss:// here",
            ),
            Self::Junk => {
                f.write_str("the relay URL contains whitespace or control characters")
            }
            Self::OnionAddress => f.write_str(
                "not a valid onion address — a v3 onion is 56 characters (a-z, 2-7) before .onion",
            ),
            Self::Userinfo => f.write_str("credentials do not belong in a relay URL"),
            Self::Fragment => f.write_str("a relay URL cannot carry a #fragment"),
            Self::TooLong => f.write_str("the relay URL is longer than 512 bytes"),
            Self::NonCanonical => f.write_str(
                "not the canonical spelling of this address — write host, IP and port plainly \
                 (e.g. wss://relay.example.org or ws://192.168.1.5:7777)",
            ),
        }
    }
}

/// The MDK relay-validator bound (`mdk_evaluation.md` §2.2): nothing longer
/// than this is a relay URL.
pub const MAX_URL_LEN: usize = 512;

/// Validate and NORMALIZE a relay URL: lowercase, no trailing slash, no
/// redundant default port, canonical path — so two spellings of one relay
/// cannot enter the pool as two entries.
pub fn normalize_relay_url(raw: &str) -> Result<String, RelayUrlError> {
    classified(raw).map(|(url, _)| url)
}

/// The kind of a relay URL — derived, never stored.
///
/// This is the security-critical direction: `Onion` means "may be dialed with
/// no user interaction at all". It runs the SAME single parse as
/// [`normalize_relay_url`] — one code path, so the classifier and the ingest
/// can never disagree — and does not assume its input passed ingest: a
/// hand-edited `config.toml` reaches the pool unvalidated. Anything that
/// parse refuses counts as [`RelayKind::Clearnet`]: the side of the gate
/// that asks the user first.
pub fn relay_kind(url: &str) -> RelayKind {
    classified(url).map_or(RelayKind::Clearnet, |(_, kind)| kind)
}

/// The one shared pass behind [`normalize_relay_url`] and [`relay_kind`].
///
/// The host is taken from the **WHATWG parser** (`url` — the parser every
/// real WebSocket/Nostr client dials with), never from hand-rolled string
/// slicing: the 2026-07-31 review found two CRITICAL onion spoofs
/// (`\`-authority, `userinfo@`) that existed precisely because our reading
/// of the authority differed from the dialing client's
/// (`mdk_evaluation.md` §6). Strictness on TOP of the parse (ASCII only, no
/// backslash, canonical spelling, conservative domain labels) narrows what
/// we accept — but what we accept is always exactly what the parser read.
fn classified(raw: &str) -> Result<(String, RelayKind), RelayUrlError> {
    let trimmed = raw.trim();
    // A backslash has no place in a relay URL; WHATWG rewrites it to `/` for
    // ws/wss, so a stored one would not be the URL that gets dialed. Refuse
    // it outright rather than store an ambiguity.
    if trimmed
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(RelayUrlError::Junk);
    }
    // ASCII-only, so lowercasing is byte-stable and the authority cannot
    // carry homoglyphs or IDN forms the parser would resolve differently.
    if !trimmed.is_ascii() {
        return Err(RelayUrlError::Junk);
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(RelayUrlError::TooLong);
    }
    let lower = trimmed.to_ascii_lowercase();
    let (scheme, after_scheme) = if let Some(rest) = lower.strip_prefix("wss://") {
        ("wss://", rest)
    } else if let Some(rest) = lower.strip_prefix("ws://") {
        ("ws://", rest)
    } else {
        return Err(RelayUrlError::Scheme);
    };
    // THE parse — the same algorithm every dialing client runs.
    let parsed = url::Url::parse(&lower).map_err(|_| RelayUrlError::Host)?;
    if parsed.fragment().is_some() {
        return Err(RelayUrlError::Fragment);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RelayUrlError::Userinfo);
    }
    let host = parsed.host().ok_or(RelayUrlError::Host)?;
    let host_str = parsed.host_str().ok_or(RelayUrlError::Host)?;
    // port 0 parses, but nothing can listen on it
    if parsed.port() == Some(0) {
        return Err(RelayUrlError::Host);
    }
    // One spelling per endpoint: the input authority must already read as
    // the parser re-serializes it. An explicit default port is the one
    // redundancy we accept — and drop, so `wss://r:443` and `wss://r` are
    // one pool entry (MDK hit that "two spellings, two routes" bug in
    // production). Anything else the parser had to rewrite — a percent
    // escape, an alternate IPv4 notation, an odd port spelling — is refused
    // rather than stored as a second reading of the same address.
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let input_authority = &after_scheme[..authority_end];
    let canonical_authority = match parsed.port() {
        Some(p) => format!("{host_str}:{p}"),
        None => host_str.to_string(),
    };
    let default_port: u16 = if scheme == "wss://" { 443 } else { 80 };
    let spelled_default = format!("{host_str}:{default_port}");
    if input_authority != canonical_authority
        && !(parsed.port().is_none() && input_authority == spelled_default)
    {
        return Err(RelayUrlError::NonCanonical);
    }
    let kind = match host {
        url::Host::Domain(d) => domain_kind(d)?,
        url::Host::Ipv4(a) => ipv4_kind(a)?,
        url::Host::Ipv6(a) => ipv6_kind(a)?,
    };
    if scheme == "ws://" && kind == RelayKind::Clearnet {
        return Err(RelayUrlError::PlaintextClearnet);
    }
    // canonical path + query from the parse (dot-segments collapsed — the
    // form every client requests); only a trailing slash is dropped so "…/"
    // and "…" are one entry
    let mut tail = parsed.path().trim_end_matches('/').to_string();
    if let Some(q) = parsed.query() {
        tail.push('?');
        tail.push_str(q);
    }
    // FIXPOINT: escapes the parser ADDS (e.g. `{` → `%7B`) come out
    // uppercase, while pre-existing escapes pass through verbatim — so a
    // stored `%7B` would re-normalize to `%7b` and the pool key would drift
    // (two spellings of one endpoint, unaddressable entries). Lowercasing
    // the assembled tail (input is ASCII throughout) makes
    // normalize(normalize(x)) == normalize(x) hold for every accepted x.
    let out = format!("{scheme}{canonical_authority}{}", tail.to_ascii_lowercase());
    // …and the length bound holds for what we STORE, not just what was
    // typed: parser-added escapes triple a byte, so a short input can
    // canonicalize past the cap
    if out.len() > MAX_URL_LEN {
        return Err(RelayUrlError::TooLong);
    }
    Ok((out, kind))
}

/// Classify a PARSED domain host. On top of the parse, only conservative
/// label shapes are accepted (letters, digits, `-`; no empty label, no
/// trailing dot): the parser also tolerates `_` and friends, and every extra
/// character is gate surface we don't need.
fn domain_kind(host: &str) -> Result<RelayKind, RelayUrlError> {
    if host.is_empty() || host.ends_with('.') {
        return Err(RelayUrlError::Host);
    }
    for label in host.split('.') {
        if label.is_empty()
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(RelayUrlError::Host);
        }
    }
    // RFC 6761: localhost names resolve to loopback
    if host == "localhost" || host.ends_with(".localhost") {
        return Ok(RelayKind::Local);
    }
    // `Onion` — and with it the right to be dialed with no user interaction
    // — is granted only to a syntactically real **v3 onion address**: 56
    // base32 characters (`a-z2-7`) before the `.onion` label. A name that
    // merely CLAIMS .onion cannot resolve anywhere, so it is refused rather
    // than quietly demoted to clearnet. Retired v2 addresses (16 chars) do
    // not qualify — Tor dropped them.
    const V3_LEN: usize = 56;
    let mut labels = host.rsplit('.');
    if labels.next() == Some("onion") {
        return match labels.next() {
            Some(addr)
                if addr.len() == V3_LEN
                    && addr
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)) =>
            {
                Ok(RelayKind::Onion)
            }
            _ => Err(RelayUrlError::OnionAddress),
        };
    }
    Ok(RelayKind::Clearnet)
}

/// Classify a parsed IPv4 literal (§10.14): loopback/private/link-local are
/// `Local`; addresses nothing can listen on are refused.
fn ipv4_kind(a: core::net::Ipv4Addr) -> Result<RelayKind, RelayUrlError> {
    if a.is_unspecified() || a.is_broadcast() || a.is_multicast() {
        return Err(RelayUrlError::Host);
    }
    if a.is_loopback() || a.is_private() || a.is_link_local() {
        return Ok(RelayKind::Local);
    }
    Ok(RelayKind::Clearnet)
}

/// Classify a parsed IPv6 literal (§10.14). fc00::/7 (unique-local) and
/// fe80::/10 (link-local) are computed from the leading segment — the std
/// helpers for them are not stable on our MSRV.
fn ipv6_kind(a: core::net::Ipv6Addr) -> Result<RelayKind, RelayUrlError> {
    if a.is_unspecified() || a.is_multicast() {
        return Err(RelayUrlError::Host);
    }
    let seg0 = a.segments()[0];
    let unique_local = (seg0 & 0xfe00) == 0xfc00;
    let link_local = (seg0 & 0xffc0) == 0xfe80;
    if a.is_loopback() || unique_local || link_local {
        return Ok(RelayKind::Local);
    }
    Ok(RelayKind::Clearnet)
}

/// One relay as the surfaces SEE it: the stored fields plus everything the
/// policy derives, so a GUI or an MCP agent never has to re-implement the
/// rules to explain why a relay is or is not in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayStatus {
    /// The relay URL (normalized).
    pub url: String,
    /// Derived from the URL — onion, clearnet or local.
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
        // duplicates collapse onto the FIRST entry (the file order is the
        // dial priority), but a confirmation on ANY spelling survives the
        // merge: when normalization re-keys two previously-distinct
        // spellings (`wss://r:443` + `wss://r`) onto one endpoint, the
        // operator's explicit "yes" must not be lost to entry order.
        if let Some(kept) = out.iter_mut().find(|kept| kept.url == url) {
            kept.confirmed |= entry.confirmed;
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
/// `clearnet_session` is the activation of non-Tor dialing. It gates
/// everything that is not an onion service: clearnet AND local relays
/// (§10.14 — a local relay is reached directly, bypassing Tor). Since the
/// ADR-0004 amendment it is persisted (`[transport.nostr] clearnet_enabled`),
/// so the operator decides once rather than after every restart; the
/// parameter name is kept because every caller threads it through under that
/// name.
pub fn relay_block(entry: &RelayEntry, clearnet_session: bool) -> Option<RelayBlock> {
    if !entry.confirmed {
        return Some(RelayBlock::Unconfirmed);
    }
    if relay_kind(&entry.url) != RelayKind::Onion && !clearnet_session {
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

/// Why NOTHING in this pool can be dialed — the node-level counterpart to
/// [`RelayBlock`], which answers the same question per entry.
///
/// This is the one classifier for that question. It used to exist three
/// times — `tor_probe::target_gap`, an inline predicate in the GUI's Tor
/// panel, and a third copy added with the join diagnosis — which is how a
/// pool could be described one way by the Tor panel and another way by a
/// refused founding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolGap {
    /// No relay in the pool at all.
    Empty,
    /// Relays exist, none of them confirmed.
    Unconfirmed,
    /// Confirmed relays exist, but every one of them is non-onion while this
    /// node does not dial outside Tor. The block an operator cannot deduce
    /// from their own config, where the relay plainly reads `confirmed`.
    NonOnionOff,
}

/// Classify a pool that yields no dialable relay. `None` = something IS
/// dialable, so there is no gap to explain.
///
/// Derived, never assumed from the caller's context: a diagnosis that can be
/// wrong is worse than none.
pub fn pool_gap(pool: &[RelayEntry], clearnet_session: bool) -> Option<PoolGap> {
    if pool.iter().any(|e| relay_block(e, clearnet_session).is_none()) {
        return None;
    }
    if pool.is_empty() {
        Some(PoolGap::Empty)
    } else if !pool.iter().any(|e| e.confirmed) {
        Some(PoolGap::Unconfirmed)
    } else {
        // a confirmed ONION relay is always dialable, so if nothing is and
        // something is confirmed, every confirmed entry is non-onion and the
        // switch is off
        Some(PoolGap::NonOnionOff)
    }
}

/// Why this node cannot dial a relay that somebody ELSE named — an invite's
/// relay set measured against the local pool. `None` means "dialable".
///
/// This is [`RelayBlock`] plus the case a pool entry cannot express: a relay
/// this node has never heard of. Keeping them in one verdict is the point —
/// the three need three different next steps, and collapsing them into "no
/// relay in common" is what made a refused join unactionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InviteRelayBlock {
    /// This node's pool does not contain the relay at all.
    NotInPool,
    /// In the pool, but the operator has never confirmed it.
    Unconfirmed,
    /// Confirmed, but non-onion (clearnet or local) while this node does not
    /// dial outside Tor.
    ClearnetOff,
}

impl From<RelayBlock> for InviteRelayBlock {
    /// The two blocks a pool entry can carry mean the same thing whether the
    /// relay came from this node's settings or from somebody's invite — one
    /// named place for that correspondence, so a new [`RelayBlock`] variant
    /// breaks the build here instead of silently mis-classifying.
    fn from(block: RelayBlock) -> Self {
        match block {
            RelayBlock::Unconfirmed => Self::Unconfirmed,
            RelayBlock::ClearnetSessionLocked => Self::ClearnetOff,
        }
    }
}

/// One invite relay and this node's verdict on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRelayVerdict {
    /// The relay URL as the invite spells it.
    pub url: String,
    /// `None` = this node can dial it; otherwise why not.
    pub blocked: Option<InviteRelayBlock>,
}

/// Judge every relay an invite names against this node's pool, in the
/// invite's own order.
///
/// Both sides are normalized by the same [`normalize_relay_url`] gate (the
/// invite at parse time, the pool at ingest and in [`sanitize_pool`]), so a
/// plain string match is the whole comparison.
///
/// This is the CLASSIFICATION only. Turning a verdict into a sentence is the
/// surfaces' job: the words name a settings tab and a config key, which a
/// crate with no I/O has no business knowing, and the same verdict has to
/// reach a German GUI, an English run log and an MCP agent.
pub fn diagnose_invite_relays(
    offered: &[String],
    pool: &[RelayEntry],
    clearnet_session: bool,
) -> Vec<InviteRelayVerdict> {
    offered
        .iter()
        .map(|url| {
            let blocked = match pool.iter().find(|e| &e.url == url) {
                None => Some(InviteRelayBlock::NotInPool),
                Some(entry) => relay_block(entry, clearnet_session).map(Into::into),
            };
            InviteRelayVerdict { url: url.clone(), blocked }
        })
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

    /// An empty dial set has three different causes and three different
    /// fixes — and the one an operator cannot deduce from their own config
    /// (where the relay plainly reads `confirmed = true`) is the node-level
    /// switch. `None` means there is no gap to explain at all.
    #[test]
    fn an_empty_dial_set_is_classified_by_which_of_the_three_it_is() {
        const ONION: &str =
            "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion";
        assert_eq!(pool_gap(&[], false), Some(PoolGap::Empty));

        let unconfirmed = vec![entry("wss://relay.example.org", false)];
        assert_eq!(pool_gap(&unconfirmed, true), Some(PoolGap::Unconfirmed));

        let confirmed = vec![entry("wss://relay.example.org", true)];
        assert_eq!(pool_gap(&confirmed, false), Some(PoolGap::NonOnionOff));
        assert_eq!(pool_gap(&confirmed, true), None, "switched on, nothing to explain");

        // a confirmed onion relay is dialable whatever the switch says, so a
        // pool holding one has no gap even beside a blocked clearnet entry
        let mixed = vec![entry(ONION, true), entry("wss://relay.example.org", true)];
        assert_eq!(pool_gap(&mixed, false), None);
    }

    /// The three ways an invite's relay can be undialable here are three
    /// DIFFERENT problems — the verdict must keep them apart, and must not
    /// call a relay the operator can see in their own pool "not in common".
    #[test]
    fn every_invite_relay_is_judged_on_its_own_against_the_pool() {
        const ONION: &str =
            "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion";
        let pool = vec![entry("wss://unconfirmed.example", false), entry("wss://dark.example", true)];
        let offered = vec![
            "wss://never-heard-of.example".to_string(),
            "wss://unconfirmed.example".to_string(),
            "wss://dark.example".to_string(),
        ];
        let v = diagnose_invite_relays(&offered, &pool, false);
        assert_eq!(
            v.iter().map(|v| v.blocked).collect::<Vec<_>>(),
            vec![
                Some(InviteRelayBlock::NotInPool),
                Some(InviteRelayBlock::Unconfirmed),
                Some(InviteRelayBlock::ClearnetOff),
            ],
            "the verdicts keep the invite's own order"
        );
        assert_eq!(v[0].url, offered[0], "each verdict carries its own relay");
        // the clearnet switch is the ONLY thing between the third and a join
        assert_eq!(diagnose_invite_relays(&offered, &pool, true)[2].blocked, None);
        // an onion relay needs no switch at all
        let pool = vec![entry(ONION, true)];
        assert_eq!(diagnose_invite_relays(&[ONION.to_string()], &pool, false)[0].blocked, None);
    }

    /// KEYSTONE — onion connects by itself, clearnet never does without the
    /// node-level non-onion dialing switch.
    #[test]
    fn onion_dials_automatically_but_clearnet_needs_the_session_unlock() {
        let onion = entry("wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion", true);
        let clearnet = entry("wss://relay.example.org", true);
        // background/startup: non-onion dialing off
        assert_eq!(relay_block(&onion, false), None, "onion is background-dialable");
        assert_eq!(
            relay_block(&clearnet, false),
            Some(RelayBlock::ClearnetSessionLocked),
            "a confirmed clearnet relay is still NOT dialed automatically"
        );
        let pool = vec![onion.clone(), clearnet.clone()];
        assert_eq!(dialable(&pool, false), vec!["wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion"]);
        // the user switches non-onion dialing on
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
            // port 0 parses, but nothing listens on it — undialable
            "wss://relay.example.org:0",
        ] {
            assert!(normalize_relay_url(bad).is_err(), "must refuse {bad:?}");
        }
    }

    /// KEYSTONE — §10.14 (user decision 2026-07-31): a relay on a private,
    /// loopback or link-local address is a legitimate self-host target. It is
    /// classified [`RelayKind::Local`], never dialed silently, and rides the
    /// SAME gate as clearnet — it bypasses Tor by nature, so the
    /// explicit activation is what keeps "no un-acknowledged non-Tor dial"
    /// true. Plaintext `ws://` is allowed here: no CA issues certificates for
    /// private addresses, the exposure is bounded to the local path, and the
    /// acknowledgement has already happened.
    #[test]
    fn local_addresses_are_classified_and_gated_like_clearnet() {
        for local in [
            "wss://127.0.0.1:7777",
            "ws://127.0.0.1:7777",
            "ws://127.1.2.3",
            "ws://192.168.1.5:7777",
            "ws://10.0.0.2",
            "ws://172.16.0.1",
            "ws://169.254.7.7",
            "ws://[::1]:8080",
            "ws://[fd00::1]",
            "ws://localhost:7777",
            "ws://relay.localhost",
        ] {
            let n = normalize_relay_url(local)
                .unwrap_or_else(|e| panic!("{local:?} must be a valid local relay: {e}"));
            assert_eq!(relay_kind(&n), RelayKind::Local, "{local:?}");
            assert_ne!(relay_kind(local), RelayKind::Onion, "{local:?} is never onion");
            let confirmed = RelayEntry { url: n.clone(), confirmed: true };
            assert_eq!(
                relay_block(&confirmed, false),
                Some(RelayBlock::ClearnetSessionLocked),
                "{local:?} must wait for the non-onion dialing switch"
            );
            assert_eq!(relay_block(&confirmed, true), None, "{local:?} unlocks with the session");
            assert_eq!(
                relay_block(&RelayEntry { url: n, confirmed: false }, true),
                Some(RelayBlock::Unconfirmed),
                "confirmation still comes first"
            );
        }
        // a PUBLIC IP literal is a clearnet relay: wss only, clearnet gate
        let public = normalize_relay_url("wss://203.0.113.7:8443").expect("public IP relay");
        assert_eq!(relay_kind(&public), RelayKind::Clearnet);
        assert_eq!(
            normalize_relay_url("ws://203.0.113.7"),
            Err(RelayUrlError::PlaintextClearnet),
            "plaintext to a PUBLIC address stays refused"
        );
        // …and addresses nothing can listen on are not relays at all
        for dead in ["wss://0.0.0.0", "wss://255.255.255.255", "wss://224.0.0.1", "wss://[ff02::1]"] {
            assert!(normalize_relay_url(dead).is_err(), "must refuse {dead:?}");
        }
    }

    /// The WHATWG parser resolves alternate IPv4 spellings (hex, octal, plain
    /// integer, leading zeros, shorthand) to an address — every real client
    /// dials the RESOLVED address, so accepting the alternate spelling would
    /// store a URL that reads differently here than on the wire (and hides a
    /// LOCAL address behind a clearnet-looking name). Only the canonical
    /// spelling enters the pool; `url` is the authority for what that is.
    #[test]
    fn alternate_ip_spellings_are_refused() {
        for weird in [
            "wss://0x7f.1",          // hex + short → 127.0.0.1
            "wss://2130706433",      // plain integer → 127.0.0.1
            "wss://0177.0.0.1",      // octal → 127.0.0.1
            "wss://192.168.001.005", // leading zeros
            "wss://127.1",           // shorthand
            "ws://0x7f.1",
        ] {
            assert!(normalize_relay_url(weird).is_err(), "must refuse {weird:?}");
            assert_ne!(relay_kind(weird), RelayKind::Onion, "{weird:?}");
        }
        assert_eq!(
            normalize_relay_url("wss://127.0.0.1").expect("canonical"),
            "wss://127.0.0.1"
        );
    }

    /// MDK's relay-URL validator, adopted 2026-07-31 (`mdk_evaluation.md`
    /// §2.2): credentials and fragments have no place in a relay URL, and
    /// anything over 512 bytes is refused before it can wander into configs
    /// and subscriptions.
    #[test]
    fn credentials_fragments_and_oversize_are_refused() {
        assert_eq!(
            normalize_relay_url("wss://user:pass@relay.example.org"),
            Err(RelayUrlError::Userinfo)
        );
        assert_eq!(
            normalize_relay_url("wss://user@relay.example.org"),
            Err(RelayUrlError::Userinfo)
        );
        assert_eq!(
            normalize_relay_url("wss://relay.example.org/x#frag"),
            Err(RelayUrlError::Fragment)
        );
        let long = format!("wss://relay.example.org/{}", "a".repeat(600));
        assert_eq!(normalize_relay_url(&long), Err(RelayUrlError::TooLong));
    }

    /// A default port is a redundant spelling: the WHATWG parser (what every
    /// client dials with) treats `wss://r:443` and `wss://r` as the same
    /// endpoint, and MDK hit the "two spellings, two routes" bug in
    /// production. One endpoint, one pool entry.
    #[test]
    fn a_default_port_collapses_into_the_bare_spelling() {
        assert_eq!(
            normalize_relay_url("wss://relay.example.org:443").expect("default wss port"),
            "wss://relay.example.org"
        );
        assert_eq!(
            normalize_relay_url(
                "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion:80"
            )
            .expect("default ws port"),
            "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion"
        );
        assert_eq!(
            normalize_relay_url("wss://relay.example.org:8443").expect("real port"),
            "wss://relay.example.org:8443"
        );
    }

    /// `url` collapses dot-segments in the path (`/a/../b` → `/b`) — every
    /// client requests the collapsed path, so the pool stores it. This was a
    /// recorded residual divergence of the hand-rolled parser
    /// (`mdk_evaluation.md` §6).
    #[test]
    fn paths_are_stored_canonical() {
        assert_eq!(
            normalize_relay_url("wss://relay.example.org/a/../nostr").expect("dot segments"),
            "wss://relay.example.org/nostr"
        );
    }

    /// KEYSTONE (review finding 2026-07-31, HIGH) — normalization is a
    /// FIXPOINT: the stored form re-normalizes to ITSELF. Without this,
    /// parser-ADDED escapes (uppercase `%7B`) re-key the entry on the next
    /// load (`%7b`), commands stop addressing it, and the duplicate guard
    /// can mint two entries for one endpoint — the exact MDK
    /// "two spellings, two routes" bug this rebuild exists to kill.
    #[test]
    fn normalization_is_a_fixpoint() {
        for raw in [
            "wss://relay.example.org/a{b",   // the parser escapes `{` → %7b
            "wss://relay.example.org/a%7Bb", // pre-existing uppercase escape
            "wss://relay.example.org?a<b",   // query escape
            "wss://relay.example.org/a/../b",
            "wss://relay.example.org:443/x/",
            "ws://192.168.1.5:7777",
            "ws://[::1]:8080",
            "ws://localhost:7777",
            "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion/nostr",
        ] {
            let once = normalize_relay_url(raw).unwrap_or_else(|e| panic!("{raw:?}: {e}"));
            assert_eq!(
                normalize_relay_url(&once).as_deref(),
                Ok(once.as_str()),
                "stored form of {raw:?} must survive re-normalization unchanged"
            );
            assert_eq!(
                relay_kind(&once),
                relay_kind(raw.trim()),
                "…and classify identically before and after storage"
            );
        }
    }

    /// REVIEW FINDING (2026-07-31, MEDIUM) — the length cap binds the STORED
    /// form, not just the typed one: a parser-added escape turns one byte
    /// into three, so a short input can canonicalize past the cap and the
    /// stored entry would be rejected — and silently dropped — forever after.
    #[test]
    fn the_length_cap_binds_the_stored_form() {
        let raw = format!("wss://relay.example.org/{}", "{".repeat(200));
        assert!(raw.len() <= MAX_URL_LEN, "the INPUT is under the cap");
        assert_eq!(normalize_relay_url(&raw), Err(RelayUrlError::TooLong));
    }

    /// REVIEW FINDING (2026-07-31, LOW) — when normalization re-keys two
    /// previously-distinct spellings onto one endpoint, a confirmation on
    /// EITHER survives the merge (first position wins, the "yes" is OR-ed) —
    /// an upgrade must not silently un-confirm the operator's relay.
    #[test]
    fn sanitize_merges_spellings_without_losing_the_confirmation() {
        let raw = vec![
            RelayEntry { url: "wss://relay.example.org:443".to_string(), confirmed: false },
            RelayEntry { url: "wss://relay.example.org".to_string(), confirmed: true },
        ];
        assert_eq!(
            sanitize_pool(&raw),
            vec![RelayEntry { url: "wss://relay.example.org".to_string(), confirmed: true }]
        );
        // a Local entry round-trips the pool unchanged, kind intact
        let local = vec![RelayEntry { url: "ws://192.168.1.5:7777".to_string(), confirmed: true }];
        let clean = sanitize_pool(&local);
        assert_eq!(clean, local);
        assert_eq!(relay_kind(&clean[0].url), RelayKind::Local);
        // and the default-port collapse holds for IPv6 literals too
        assert_eq!(normalize_relay_url("wss://[::1]:443").expect("v6 default"), "wss://[::1]");
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
            // WHATWG skips surplus slashes and would read host "nostr" —
            // not what the typed authority says, so: not canonical
            ("wss:///nostr", RelayUrlError::NonCanonical),
            ("wss://relay example.org", RelayUrlError::Junk),
            ("wss://relay\u{7}.example.org", RelayUrlError::Junk),
        ] {
            assert_eq!(normalize_relay_url(bad), Err(want), "must reject {bad:?}");
        }
    }
}
