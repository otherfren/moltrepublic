// SPDX-License-Identifier: GPL-3.0-or-later
//! The anonymity panel and the header pill: network/Tor-mode index maps,
//! the Tor probe's tone + verdict copy ("only a proven circuit is green"),
//! and the transport-health pill mapping.

use molt_core::NetHealth;

use crate::i18n::{localize_tor_detail, Lexicon};

/// Map an anonymity-network name to its ComboBox index. The dropdown
/// offers tor and none (nym was removed from the UI 2026-07-18 — never
/// implemented); a lingering "nym" in an old config displays as none
/// (fail-closed would silently DIAL, so the honest reading is "no
/// anonymity network configured").
pub(crate) fn net_index(s: &str) -> i32 {
    match s {
        "none" | "nym" => 1,
        _ => 0,
    }
}

/// Map a ComboBox index back to an anonymity-network name.
pub(crate) fn net_name(i: i32) -> String {
    match i {
        1 => "none",
        _ => "tor",
    }
    .to_string()
}

/// Map a Tor-mode name to its ComboBox index.
pub(crate) fn mode_index(s: &str) -> i32 {
    match s {
        "embedded" => 1,
        "whonix" => 2,
        _ => 0,
    }
}

/// Map a ComboBox index back to a Tor-mode name.
pub(crate) fn mode_name(i: i32) -> String {
    match i {
        1 => "embedded",
        2 => "whonix",
        _ => "local",
    }
    .to_string()
}

/// Tone codes for a streamed verdict line (`cfg-tor-test-tone` on the Slint
/// side). Keeping the mapping in Rust keeps the `.slint` a plain colour
/// lookup instead of a nine-way string comparison — and makes the honesty
/// rule ("only a proven circuit is green") a testable statement.
pub(crate) const TONE_NEUTRAL: i32 = 0;
/// Proven: the only tone that may read as success.
pub(crate) const TONE_GOOD: i32 = 1;
/// Partial: something answered, but the thing that matters is unproven.
pub(crate) const TONE_WARN: i32 = 2;
/// Failed or refused by the configuration.
pub(crate) const TONE_BAD: i32 = 3;

/// The colour tone of a Tor probe verdict.
///
/// The whole point of the ladder ([`molt_core::TorTestState`]) is that a green
/// light never claims more than was proven: ONLY a completed circuit through
/// Tor is good. A SOCKS port that merely answers is amber — a socket is there,
/// nothing was routed through it. Idle/Testing/Off are not verdicts at all
/// (nothing was probed), so they stay neutral rather than pretending failure.
pub(crate) fn tor_test_tone(state: molt_core::TorTestState) -> i32 {
    use molt_core::TorTestState as S;
    match state {
        S::Circuit => TONE_GOOD,
        S::ProxyOnly => TONE_WARN,
        S::Idle | S::Testing | S::Off => TONE_NEUTRAL,
        S::Misconfigured | S::NoProxy | S::NoTarget | S::CircuitFailed => TONE_BAD,
        // a deadline says "no answer yet", not "broken" — a cold embedded
        // Tor bootstrap takes minutes (review finding 2026-07-31)
        S::CircuitTimeout => TONE_WARN,
    }
}

/// The localized sentence for one rung of the Tor ladder. Each rung says
/// exactly what it proved and nothing more — the partial rung denies the
/// circuit out loud, and only [`molt_core::TorTestState::Circuit`] says Tor
/// works.
/// The verdict sentence. `detail` decides between the two shapes of a failed
/// circuit: a DEADLINE (a cold embedded-Tor bootstrap legitimately takes
/// minutes, and `dial.rs` deliberately puts no cap on it) reads differently
/// from a refusal — and neither may claim a proxy rung that never ran on the
/// embedded path (review findings 2026-07-31).
pub(crate) fn tor_verdict_copy_for(
    lang: i32,
    state: molt_core::TorTestState,
    session_locked: bool,
) -> &'static str {
    let l = if lang == 1 { Lexicon::de() } else { Lexicon::en() };
    use molt_core::TorTestState as S;
    match state {
        S::ProxyOnly if session_locked => l.tor_v_proxy_only_locked,
        other => tor_verdict_copy(lang, other),
    }
}

pub(crate) fn tor_verdict_copy(lang: i32, state: molt_core::TorTestState) -> &'static str {
    use molt_core::TorTestState as S;
    let l = if lang == 1 { Lexicon::de() } else { Lexicon::en() };
    match state {
        S::Idle => l.tor_v_idle,
        S::Testing => l.tor_v_testing,
        S::Off => l.tor_v_off,
        S::Misconfigured => l.tor_v_misconfigured,
        S::NoProxy => l.tor_v_no_proxy,
        S::ProxyOnly => l.tor_v_proxy_only,
        S::NoTarget => l.tor_v_no_target,
        S::CircuitFailed => l.tor_v_circuit_failed,
        S::CircuitTimeout => l.tor_v_timeout,
        S::Circuit => l.tor_v_circuit,
    }
}

/// The technical second line under a Tor verdict: the SOCKS address that was
/// probed, the relay that was dialed, the circuit's dial time and the engine's
/// own reason — every part omitted when the engine did not report it, so the
/// line can never suggest a probe that did not happen. The verdict phrases
/// localize (E6); raw rung tails (socket errors, hosts) stay verbatim.
pub(crate) fn tor_test_detail(lang: i32, t: &molt_core::TorTest) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !t.proxy.is_empty() {
        parts.push(format!("socks {}", t.proxy));
    }
    if !t.target.is_empty() {
        parts.push(format!("relay {}", t.target));
    }
    // a duration is only meaningful for a circuit that actually completed —
    // printing it next to a failure would read as a working connection
    if t.state == molt_core::TorTestState::Circuit && t.ms > 0 {
        parts.push(format!("{} ms", t.ms));
    }
    if !t.detail.is_empty() {
        parts.push(localize_tor_detail(lang, &t.detail));
    }
    parts.join(" · ")
}

/// The `NetTestTor` arguments for the anonymity panel's DRAFT (the form the
/// user is looking at), not the saved settings: changing the anonymity network
/// is restart-required, so the user will normally not have saved yet. A draft
/// port outside the wire type collapses to the engine's "not given" marker
/// (`0`, on which nothing can listen) instead of wrapping into a valid port.
pub(crate) fn tor_probe_args(network_index: i32, mode_index: i32, port: i32) -> (String, String, u16) {
    (
        net_name(network_index),
        mode_name(mode_index),
        u16::try_from(port).unwrap_or(0),
    )
}

/// The tor-mode dropdown's per-row `enabled` flags (parallel to the model
/// `["local", "embedded", "whonix"]`). `local` and `whonix` route to a system
/// SOCKS proxy and are always available; `embedded` needs the in-process arti
/// dialer, which only exists when the binary was built with the `embedded-tor`
/// feature — so it is greyed (like nym) unless `embedded_available` is true
/// (the compile-time truth crossing the app→ui seam, P3).
pub(crate) fn tor_mode_enabled(embedded_available: bool) -> [bool; 3] {
    [true, embedded_available, true]
}

/// Map the transport-health state onto the header "chat" pill's tone index and
/// hover tooltip (P6). Tone index: `0` = good/green, `1` = warn/amber,
/// `2` = bad/red. The nominal `Ok` state carries no tooltip; the impaired and
/// down states carry the engine's reason string.
pub(crate) fn net_health_pill(health: &NetHealth) -> (i32, String) {
    match health {
        NetHealth::Ok => (0, String::new()),
        NetHealth::Degraded { reason } => (1, reason.clone()),
        NetHealth::Down { reason } => (2, reason.clone()),
    }
}
