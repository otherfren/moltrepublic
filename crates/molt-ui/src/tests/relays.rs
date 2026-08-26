// SPDX-License-Identifier: GPL-3.0-or-later
//! The relay pool's field validation.

use super::*;

/// Every way the pool refuses a URL reaches the user as a readable line
/// under the field — in their language, never as a silent no-op. The
/// classification comes from molt-core's own parser, so the message and
/// the engine's gate can never drift apart.
#[test]
fn a_refused_relay_url_gets_a_localized_message_under_the_field() {
    let pool = vec!["wss://relay.example.org".to_string()];
    for lang in [0, 1] {
        assert_eq!(
            relay_add_check(lang, "wss://fresh.example.org", &pool).as_deref(),
            Ok("wss://fresh.example.org")
        );
        assert!(
            relay_add_check(
                lang,
                "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion",
                &pool
            )
            .is_ok(),
            "plaintext to an onion service is fine - Tor encrypts it"
        );
        // …and every refusal names its reason
        for bad in [
            "https://relay.example.org",
            "relay.example.org",
            "wss://",
            "ws://relay.example.org",
            "wss://relay example.org",
            // a .onion host that is not a real v3 address
            "wss://aaa.onion",
            // already in the pool (normalized: same relay, other spelling)
            "WSS://Relay.Example.ORG/",
        ] {
            let msg = relay_add_check(lang, bad, &pool)
                .err()
                .unwrap_or_else(|| panic!("{bad:?} must be refused with a message"));
            assert!(!msg.is_empty());
        }
    }
    // the five parser verdicts and the duplicate are DISTINCT messages,
    // so the user learns what to fix
    let msgs = [
        relay_add_check(0, "https://relay.example.org", &pool).err(),
        relay_add_check(0, "wss://", &pool).err(),
        relay_add_check(0, "ws://relay.example.org", &pool).err(),
        relay_add_check(0, "wss://relay example.org", &pool).err(),
        relay_add_check(0, "wss://aaa.onion", &pool).err(),
        relay_add_check(0, "wss://relay.example.org", &pool).err(),
    ];
    for (i, a) in msgs.iter().enumerate() {
        for b in msgs.iter().skip(i + 1) {
            assert_ne!(a, b, "each refusal reads differently");
        }
    }
    // German is a real translation, not the English string
    assert_ne!(
        relay_add_check(0, "wss://", &pool).err(),
        relay_add_check(1, "wss://", &pool).err(),
    );
}
