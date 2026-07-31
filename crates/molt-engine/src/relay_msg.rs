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

/// Where the switch and the confirmation live, named once. A renamed tab or
/// config key is one edit, not six.
const SETTINGS_TAB: &str = "Settings › Nostr relays";
const CLEARNET_FIX: &str = "switch that on under Settings › Nostr relays, or set \
                            clearnet_enabled = true under [transport.nostr] in config.toml";

/// Why this node has nothing to dial at all — for the founding/recovery
/// prerequisites, which fail before any invite is involved.
pub(crate) fn pool_gap_reason(gap: PoolGap) -> String {
    match gap {
        PoolGap::Empty => format!(
            "no relay is configured — add one under {SETTINGS_TAB} and confirm \
             it (nothing is pre-configured, by design)"
        ),
        PoolGap::Unconfirmed => {
            format!("no relay is confirmed — confirm one under {SETTINGS_TAB}")
        }
        // the case the old message could not say: the operator DID confirm,
        // and the block is a node-level switch they never saw
        PoolGap::NonOnionOff => format!(
            "the confirmed relays are all clearnet or local, and this node does \
             not dial outside Tor — {CLEARNET_FIX}"
        ),
    }
}

/// The single action that would make THIS relay usable.
fn action_for(blocked: Option<InviteRelayBlock>) -> String {
    match blocked {
        None => "this node can dial this one".to_string(),
        Some(InviteRelayBlock::NotInPool) => {
            format!("not in this node's relay pool — add it under {SETTINGS_TAB}, then confirm it")
        }
        Some(InviteRelayBlock::Unconfirmed) => format!(
            "in this node's pool, but not confirmed — confirm it under \
             {SETTINGS_TAB} (a clearnet or local relay needs the exposure \
             acknowledgement)"
        ),
        Some(InviteRelayBlock::ClearnetOff) => format!(
            "confirmed, but this node does not dial clearnet or local relays — \
             {CLEARNET_FIX}"
        ),
    }
}

/// The refusal for a join whose invite names no relay this node can dial.
///
/// Line-oriented because the run log is rendered one line at a time, and a
/// paragraph naming three relays and three different fixes is a wall of text
/// nobody reads to the end. The `→ `/`✗ ` prefixes are the run log's own tone
/// protocol (molt-ui colours each line from its first character).
pub(crate) struct JoinRelayRefusal {
    /// One line per relay the invite names, in the invite's order, plus what
    /// this node CAN dial when that is not nothing.
    pub(crate) detail: Vec<String>,
    /// The terminal `✗ join failed: …` summary.
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
    let mut detail: Vec<String> = verdicts
        .iter()
        .map(|v| format!("→ {} — {}", v.url, action_for(v.blocked)))
        .collect();
    let dialable = molt_core::relay::dialable(pool, clearnet_session);
    if !dialable.is_empty() {
        detail.push(format!("→ this node can dial: {}", dialable.join(", ")));
    }
    // "no relay in common" is only true when every named relay is unknown
    // here. A relay the operator can SEE in their own settings IS in common —
    // it is merely not dialable, and calling that "no relay in common" sends
    // them looking for the wrong problem.
    let all_unknown = verdicts
        .iter()
        .all(|v| v.blocked == Some(InviteRelayBlock::NotInPool));
    let n = verdicts.len();
    let headline = if all_unknown {
        format!(
            "no relay in common with this invite — it names {n} relay(s), none \
             of them in this node's pool"
        )
    } else {
        // claims nothing about "none is dialable", so it stays true even if a
        // caller ever renders a partially-blocked set
        format!("this invite's {n} relay(s) are not usable on this node — the lines above say what each one needs")
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

    /// Each verdict names the ONE action that fixes it — and the summary may
    /// only claim "no relay in common" when every named relay really is
    /// unknown here.
    #[test]
    fn every_relay_gets_its_own_fix_and_the_summary_overclaims_nothing() {
        let pool = vec![entry("wss://unconfirmed.example", false), entry("wss://dark.example", true)];
        let offered = vec![
            "wss://never-heard-of.example".to_string(),
            "wss://unconfirmed.example".to_string(),
            "wss://dark.example".to_string(),
        ];
        let v = diagnose_invite_relays(&offered, &pool, false);
        let r = join_relay_refusal(&v, &pool, false);
        assert_eq!(r.detail.len(), 3, "one line per relay, never one paragraph");
        assert!(r.detail[0].contains("not in this node's relay pool"), "{}", r.detail[0]);
        assert!(r.detail[1].contains("not confirmed"), "{}", r.detail[1]);
        assert!(r.detail[2].contains("clearnet_enabled = true"), "{}", r.detail[2]);
        assert!(
            !r.headline.contains("no relay in common"),
            "two of the three ARE in common: {}",
            r.headline
        );

        // a genuinely disjoint pair still says exactly that, and names what
        // this node CAN dial
        let mine = vec![entry("wss://mine.example", true)];
        let offered = vec!["wss://never-heard-of.example".to_string()];
        let v = diagnose_invite_relays(&offered, &mine, true);
        let r = join_relay_refusal(&v, &mine, true);
        assert!(r.headline.contains("no relay in common"), "{}", r.headline);
        assert!(
            r.detail.iter().any(|l| l.contains("can dial: wss://mine.example")),
            "{:?}",
            r.detail
        );
    }

    /// The founding prerequisite must not re-demand a confirmation that
    /// exists — the switch is the block, so the switch is what it names.
    #[test]
    fn the_pool_gap_reason_names_the_switch_not_the_confirmation() {
        assert!(pool_gap_reason(PoolGap::Empty).contains("no relay is configured"));
        let r = pool_gap_reason(PoolGap::Unconfirmed);
        assert!(r.contains("no relay is confirmed") && !r.contains("clearnet_enabled"), "{r}");
        let r = pool_gap_reason(PoolGap::NonOnionOff);
        assert!(
            r.contains("clearnet_enabled = true") && !r.contains("confirm one"),
            "the switch is named, the existing confirmation is not re-demanded: {r}"
        );
    }
}
