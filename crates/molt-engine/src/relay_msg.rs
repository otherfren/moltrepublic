// SPDX-License-Identifier: GPL-3.0-or-later

//! Operator-facing sentences for the relay gate.
//!
//! `molt_core::relay` decides WHETHER a relay can be dialed and WHY NOT; this
//! module is the only place that turns those verdicts into words. The split
//! is deliberate: the sentences name a GUI tab ("Settings › Nostr relays")
//! and a config key (`clearnet_enabled`), and a contract crate with no I/O
//! must not know that a GUI or a `config.toml` exists. Keeping the classifier
//! pure also leaves the door open for a localized surface to render the same
//! `InviteRelayBlock` through molt-ui's `lexicon!` instead of this text.
//!
//! The wording exists because of a 2026-08-01 report: a relay hand-written
//! into `config.toml` as `confirmed = true` without `clearnet_enabled = true`
//! is undialable, and every refusal said "no confirmed relay on this node" —
//! telling the operator to repeat the one act they had already performed. A
//! refusal that names the wrong fix is worse than a refusal that names none.

use molt_core::relay::{InviteRelayBlock, InviteRelayVerdict, PoolGap, RelayEntry};

/// The config key that lifts the non-onion block — named once, in the summary
/// line, never repeated per relay.
const CLEARNET_KEY: &str = "[transport.nostr] clearnet_enabled";

/// Why this node has nothing to dial at all — for the founding/recovery
/// prerequisites, which fail before any invite is involved.
pub(crate) fn pool_gap_reason(gap: PoolGap) -> String {
    match gap {
        PoolGap::Empty => "no relay configured".to_string(),
        PoolGap::Unconfirmed => "no relay confirmed".to_string(),
        PoolGap::NonOnionOff => {
            format!("clearnet/local dialing off ({CLEARNET_KEY})")
        }
    }
}

/// The per-relay fault — two or three words, aligned into a scannable column.
/// Not an instruction: the remedy belongs in the summary line once.
fn fault_for(blocked: Option<InviteRelayBlock>) -> &'static str {
    match blocked {
        None => "dialable",
        Some(InviteRelayBlock::NotInPool) => "not in relay pool",
        Some(InviteRelayBlock::Unconfirmed) => "not confirmed",
        Some(InviteRelayBlock::ClearnetOff) => "clearnet/local dialing off",
    }
}

/// The refusal for a join whose invite names no relay this node can dial.
///
/// One line per relay, `url` then a two-word fault, aligned into a column so
/// the eye finds the odd one out without reading. The remedy is ONE summary
/// line — repeating it per relay is what made the previous version a wall of
/// text. The `→ `/`✗ ` prefixes are the run log's tone protocol (molt-ui
/// colours each line from its first character).
pub(crate) struct JoinRelayRefusal {
    /// One line per relay the invite names, in the invite's order.
    pub(crate) detail: Vec<String>,
    /// The terminal `✗ join failed: …` summary — the only line carrying a fix.
    pub(crate) headline: String,
}

/// Render the verdicts. Takes them already computed so the caller's dial set
/// and this message can never come from two different judgements of the same
/// relay.
pub(crate) fn join_relay_refusal(
    verdicts: &[InviteRelayVerdict],
    pool: &[RelayEntry],
    clearnet_session: bool,
) -> JoinRelayRefusal {
    // align the fault column: the relay that differs should be findable
    // without reading a single word
    let width = verdicts.iter().map(|v| v.url.chars().count()).max().unwrap_or(0);
    let mut detail: Vec<String> = verdicts
        .iter()
        .map(|v| format!("→ {:<width$}  {}", v.url, fault_for(v.blocked), width = width))
        .collect();
    let dialable = molt_core::relay::dialable(pool, clearnet_session);
    if !dialable.is_empty() {
        detail.push(format!("→ dialable here: {}", dialable.join(", ")));
    }
    // "no relay in common" is only true when every named relay is unknown
    // here — a relay the operator can SEE in their settings IS in common, it
    // is merely not dialable.
    let all_unknown = verdicts
        .iter()
        .all(|v| v.blocked == Some(InviteRelayBlock::NotInPool));
    let switch_blocks = verdicts
        .iter()
        .any(|v| v.blocked == Some(InviteRelayBlock::ClearnetOff));
    let headline = if all_unknown {
        "no relay in common with this invite".to_string()
    } else if switch_blocks {
        // the one case the operator cannot deduce from their own config
        format!("no dialable relay — clearnet/local dialing off ({CLEARNET_KEY})")
    } else {
        "no dialable relay for this invite".to_string()
    };
    JoinRelayRefusal { detail, headline }
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::relay::diagnose_invite_relays;

    fn entry(url: &str, confirmed: bool) -> RelayEntry {
        RelayEntry { url: url.to_string(), confirmed }
    }

    /// Each relay gets its own two-word fault, the remedy appears once, and
    /// the summary may only claim "no relay in common" when every named relay
    /// really is unknown here.
    #[test]
    fn each_relay_gets_a_short_fault_and_the_fix_is_stated_once() {
        let pool = vec![entry("wss://unconfirmed.example", false), entry("wss://dark.example", true)];
        let offered = vec![
            "wss://never-heard-of.example".to_string(),
            "wss://unconfirmed.example".to_string(),
            "wss://dark.example".to_string(),
        ];
        let v = diagnose_invite_relays(&offered, &pool, false);
        let r = join_relay_refusal(&v, &pool, false);
        assert_eq!(r.detail.len(), 3, "one line per relay");
        assert!(r.detail[0].ends_with("not in relay pool"), "{}", r.detail[0]);
        assert!(r.detail[1].ends_with("not confirmed"), "{}", r.detail[1]);
        assert!(r.detail[2].ends_with("clearnet/local dialing off"), "{}", r.detail[2]);
        // compact: a detail line is the url plus a short fault, nothing more
        for l in &r.detail {
            assert!(l.len() < 70, "detail line is not prose: {l}");
        }
        // the remedy is stated ONCE, in the summary
        assert_eq!(
            r.detail.iter().filter(|l| l.contains("clearnet_enabled")).count(),
            0,
            "the config key never repeats per relay: {:?}",
            r.detail
        );
        assert!(r.headline.contains("clearnet_enabled"), "{}", r.headline);
        assert!(!r.headline.contains("no relay in common"), "two ARE in common: {}", r.headline);

        // a genuinely disjoint pair says exactly that, and names what IS dialable
        let mine = vec![entry("wss://mine.example", true)];
        let v = diagnose_invite_relays(&["wss://never-heard-of.example".to_string()], &mine, true);
        let r = join_relay_refusal(&v, &mine, true);
        assert_eq!(r.headline, "no relay in common with this invite");
        assert!(r.detail.iter().any(|l| l.contains("dialable here: wss://mine.example")), "{:?}", r.detail);
    }

    /// The founding prerequisite names the fault in three words — and the
    /// switch, not a confirmation that already exists.
    #[test]
    fn the_pool_gap_reason_is_short_and_names_the_switch() {
        assert_eq!(pool_gap_reason(PoolGap::Empty), "no relay configured");
        assert_eq!(pool_gap_reason(PoolGap::Unconfirmed), "no relay confirmed");
        let r = pool_gap_reason(PoolGap::NonOnionOff);
        assert!(r.contains("clearnet_enabled") && r.len() < 70, "{r}");
    }
}
