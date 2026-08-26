// SPDX-License-Identifier: GPL-3.0-or-later
//! The anonymity panel: Tor tones and copy, the header pill.

use super::*;

/// The tor-mode dropdown greys "embedded" unless the binary was built with
/// the `embedded-tor` feature (P3). local + whonix are always selectable;
/// only the middle (embedded) row tracks the compile-time truth passed
/// through the app→ui seam.
#[test]
fn embedded_row_is_disabled_when_feature_off() {
    // model is ["local", "embedded", "whonix"]
    assert_eq!(tor_mode_enabled(false), [true, false, true]);
    assert_eq!(tor_mode_enabled(true), [true, true, true]);
}

/// The header "chat" pill mirrors transport health (P6): Ok → good/green
/// with no tooltip; Degraded → warn/amber; Down → bad/red — the latter two
/// carrying the engine's reason string as the hover tooltip.
#[test]
fn net_health_maps_to_pill_tone() {
    use molt_core::NetHealth;
    // tone index: 0 = good (green), 1 = warn (amber), 2 = bad (red)
    assert_eq!(net_health_pill(&NetHealth::Ok), (0, String::new()));
    assert_eq!(
        net_health_pill(&NetHealth::Degraded {
            reason: "Tor circuit timed out".to_string(),
        }),
        (1, "Tor circuit timed out".to_string()),
    );
    assert_eq!(
        net_health_pill(&NetHealth::Down {
            reason: "embedded Tor not built into this binary".to_string(),
        }),
        (2, "embedded Tor not built into this binary".to_string()),
    );
}

/// The honesty invariant of the Tor probe, in colour: ONLY a proven
/// circuit may read as "good". A SOCKS port that merely answers is amber
/// (something is there, nothing is proven), and every rung that failed or
/// refused is red or neutral — never green.
#[test]
fn only_a_proven_tor_circuit_is_toned_good() {
    use molt_core::TorTestState as S;
    assert_eq!(tor_test_tone(S::Circuit), TONE_GOOD);
    assert_eq!(tor_test_tone(S::ProxyOnly), TONE_WARN, "a listening port proves no circuit");
    for s in [S::Idle, S::Testing, S::Off] {
        assert_eq!(tor_test_tone(s), TONE_NEUTRAL, "{s:?} is not a verdict");
    }
    for s in [S::Misconfigured, S::NoProxy, S::NoTarget, S::CircuitFailed] {
        assert_eq!(tor_test_tone(s), TONE_BAD, "{s:?} is a failure");
    }
    for s in [
        S::Idle,
        S::Testing,
        S::Off,
        S::Misconfigured,
        S::NoProxy,
        S::ProxyOnly,
        S::NoTarget,
        S::CircuitFailed,
    ] {
        assert_ne!(tor_test_tone(s), TONE_GOOD, "{s:?} must never read as success");
    }
}

/// Every rung of the ladder reaches the user in their own language, and no
/// two rungs share a sentence — the whole point is that the user learns
/// WHICH rung was reached. The partial rung must say out loud that no
/// circuit is proven.
#[test]
fn every_tor_rung_has_its_own_honest_copy_in_both_languages() {
    use molt_core::TorTestState as S;
    let all = [
        S::Idle,
        S::Testing,
        S::Off,
        S::Misconfigured,
        S::NoProxy,
        S::ProxyOnly,
        S::NoTarget,
        S::CircuitFailed,
        S::Circuit,
    ];
    for lang in [0, 1] {
        for (i, a) in all.iter().enumerate() {
            assert!(!tor_verdict_copy(lang, *a).is_empty(), "{a:?} needs copy");
            for b in all.iter().skip(i + 1) {
                assert_ne!(
                    tor_verdict_copy(lang, *a),
                    tor_verdict_copy(lang, *b),
                    "{a:?} and {b:?} must not read the same"
                );
            }
        }
        // German is a real translation, not the English string
        assert_ne!(tor_verdict_copy(0, *all.last().expect("non-empty")), tor_verdict_copy(1, *all.last().expect("non-empty")));
    }
    // the partial rung states the missing proof, in both languages
    assert!(
        tor_verdict_copy(0, S::ProxyOnly).contains("no circuit"),
        "EN must deny the circuit outright"
    );
    assert!(
        tor_verdict_copy(1, S::ProxyOnly).contains("Circuit"),
        "DE must deny the circuit outright"
    );
    // …and no rung short of Circuit may claim Tor works
    for s in all.iter().filter(|s| **s != S::Circuit) {
        let en = tor_verdict_copy(0, *s).to_lowercase();
        assert!(!en.contains("tor works"), "{s:?} must not claim Tor works");
    }
}

/// The technical second line never invents anything: it names only what
/// the engine actually reported. A duration is shown for the rung it is
/// meaningful on (the completed circuit) and nowhere else.
#[test]
fn the_tor_detail_line_states_only_what_was_probed() {
    use molt_core::{TorTest, TorTestState as S};
    assert_eq!(tor_test_detail(0, &TorTest::default()), "");
    let probed = TorTest {
        state: S::ProxyOnly,
        detail: "no confirmed relay to dial".into(),
        proxy: "127.0.0.1:9050".into(),
        target: String::new(),
        ms: 0,
    };
    let line = tor_test_detail(0, &probed);
    assert!(line.contains("127.0.0.1:9050"), "the probed SOCKS address is named");
    assert!(line.contains("no confirmed relay to dial"), "the engine's reason rides along");
    assert!(!line.contains("ms"), "no duration where none was measured");
    let circuit = TorTest {
        state: S::Circuit,
        detail: String::new(),
        proxy: "127.0.0.1:9050".into(),
        target: "wss://relay.onion".into(),
        ms: 812,
    };
    let line = tor_test_detail(0, &circuit);
    assert!(line.contains("wss://relay.onion"), "the relay that was reached is named");
    assert!(line.contains("812 ms"), "the circuit's dial time");
    // a duration measured on a rung that never completed a circuit is NOT
    // shown — it would read as a working connection
    let failed = TorTest { state: S::CircuitFailed, ms: 812, ..circuit.clone() };
    assert!(!tor_test_detail(0, &failed).contains("812 ms"));
}

/// The panel's button tests the DRAFT, not the saved settings: changing
/// the anonymity network is restart-required, so the user will usually not
/// have saved yet. The port is clamped into the wire type instead of
/// wrapping — a garbage port must not silently become a valid one.
#[test]
fn the_tor_button_probes_the_draft_the_user_is_looking_at() {
    assert_eq!(tor_probe_args(0, 0, 9050), ("tor".to_string(), "local".to_string(), 9050));
    assert_eq!(
        tor_probe_args(0, 1, 9050),
        ("tor".to_string(), "embedded".to_string(), 9050)
    );
    assert_eq!(tor_probe_args(0, 2, 9050), ("tor".to_string(), "whonix".to_string(), 9050));
    // "none" is answered honestly by the engine (Off) — the GUI does not
    // silently rewrite it into a tor probe
    assert_eq!(tor_probe_args(1, 0, 9050), ("none".to_string(), "local".to_string(), 9050));
    // out-of-range drafts clamp to the "not given" marker, never wrap
    assert_eq!(tor_probe_args(0, 0, -1).2, 0);
    assert_eq!(tor_probe_args(0, 0, 70000).2, 0);
    assert_eq!(tor_probe_args(0, 0, 0).2, 0);
}
