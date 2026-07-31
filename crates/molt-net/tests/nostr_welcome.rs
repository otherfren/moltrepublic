// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N3 step 6 + N4a step 2 (`docs/transport/nostr_n4_plan.md` §4): the
//! kind-444 Welcome, NIP-59 gift-wrapped to the invitee's transport anchor.
//! Since N4 the rumor content is the VERSIONED payload v2 — the MLS Welcome
//! plus the group's `rotation_seed` and relay list, "delivered only inside
//! the authenticated Welcome, never before" (§4.2 finding 9). The inner
//! rumor stays deliberately UNSIGNED — a leaked 444 is not publishable —
//! and the peel chain is fail-closed: every step that cannot be verified
//! refuses.

use molt_net::welcome::{peel_welcome, wrap_welcome, WelcomeError, WelcomePayload};
use nostr::Keys;

fn payload() -> WelcomePayload {
    WelcomePayload {
        welcome: b"an MLS Welcome blob".to_vec(),
        rotation_seed: [7u8; 32],
        relays: vec![
            "wss://relay.example".to_string(),
            "ws://127.0.0.1:8080".to_string(),
        ],
    }
}

/// KEYSTONE — the Welcome payload roundtrips to the intended recipient
/// only, and the wrapper leaks neither the sender nor the payload: the
/// outer event is kind 1059 authored by a FRESH ephemeral key (never the
/// founder's anchor), so a relay sees an anonymous gift to one pubkey.
#[tokio::test]
async fn a_welcome_reaches_its_invitee_and_names_no_sender() {
    let founder = Keys::generate();
    let invitee = Keys::generate();
    let outsider = Keys::generate();
    let sent = payload();

    let wrap = wrap_welcome(&founder, &invitee.public_key(), &sent)
        .await
        .expect("wrap");

    assert_eq!(wrap.kind.as_u16(), 1059, "outer kind is the NIP-59 gift wrap");
    assert_ne!(
        wrap.pubkey,
        founder.public_key(),
        "the outer author is ephemeral — the founder's anchor never appears"
    );
    wrap.verify().expect("the outer event verifies");
    let json = serde_json::to_string(&wrap).expect("serialize");
    assert!(
        !json.contains(&hex::encode(&sent.welcome)),
        "the Welcome bytes must not be readable on the wire"
    );
    assert!(
        !json.contains(&hex::encode(sent.rotation_seed)),
        "the rotation seed must not be readable on the wire"
    );

    let (peeled, from) = peel_welcome(&invitee, &wrap).await.expect("invitee peels");
    assert_eq!(peeled.welcome, sent.welcome);
    assert_eq!(peeled.rotation_seed, sent.rotation_seed);
    assert_eq!(peeled.relays, sent.relays);
    assert_eq!(
        from,
        founder.public_key(),
        "the inner rumor authenticates WHO invited — the founder's anchor"
    );

    // …and nobody else can open it
    assert!(matches!(
        peel_welcome(&outsider, &wrap).await,
        Err(WelcomeError::NotForUs)
    ));
}

/// KEYSTONE — the peel chain is FAIL-CLOSED at every step: a gift whose
/// inner rumor is not a kind-444 Welcome, whose payload is not the v2
/// encoding (including the PRE-N4 bare-hex form — nothing ever produced
/// those in production, so they are refused, not grandfathered), or whose
/// fields are malformed, is refused rather than half-interpreted. A leaked
/// 444 is also not publishable — the rumor carries no signature.
#[tokio::test]
async fn the_peel_chain_refuses_anything_that_is_not_a_v2_welcome() {
    use nostr::{EventBuilder, Kind};

    let founder = Keys::generate();
    let invitee = Keys::generate();

    // right wrapper, wrong inner kind
    let rumor = EventBuilder::new(Kind::Custom(1), "not a welcome").build(founder.public_key());
    let wrap = EventBuilder::gift_wrap(&founder, &invitee.public_key(), rumor, [])
        .await
        .expect("wrap");
    assert!(matches!(
        peel_welcome(&invitee, &wrap).await,
        Err(WelcomeError::NotAWelcome { .. })
    ));

    // the pre-N4 bare-hex payload — refused, the encoding is versioned now
    let rumor =
        EventBuilder::new(Kind::Custom(444), hex::encode(b"blob")).build(founder.public_key());
    let wrap = EventBuilder::gift_wrap(&founder, &invitee.public_key(), rumor, [])
        .await
        .expect("wrap");
    assert!(matches!(
        peel_welcome(&invitee, &wrap).await,
        Err(WelcomeError::Payload(_))
    ));

    // versioned but wrong version
    let rumor = EventBuilder::new(
        Kind::Custom(444),
        r#"{"v":1,"welcome":"0a","rotation_seed":"00","relays":[]}"#,
    )
    .build(founder.public_key());
    let wrap = EventBuilder::gift_wrap(&founder, &invitee.public_key(), rumor, [])
        .await
        .expect("wrap");
    assert!(matches!(
        peel_welcome(&invitee, &wrap).await,
        Err(WelcomeError::Payload(_))
    ));

    // v2 shape, malformed fields: empty welcome / short seed / oversized relay list
    for bad in [
        format!(
            r#"{{"v":2,"welcome":"","rotation_seed":"{}","relays":[]}}"#,
            "07".repeat(32)
        ),
        r#"{"v":2,"welcome":"0a","rotation_seed":"0707","relays":[]}"#.to_string(),
        format!(
            r#"{{"v":2,"welcome":"0a","rotation_seed":"{}","relays":[{}]}}"#,
            "07".repeat(32),
            (0..9)
                .map(|i| format!("\"wss://r{i}.example\""))
                .collect::<Vec<_>>()
                .join(",")
        ),
    ] {
        let rumor = EventBuilder::new(Kind::Custom(444), bad.clone()).build(founder.public_key());
        let wrap = EventBuilder::gift_wrap(&founder, &invitee.public_key(), rumor, [])
            .await
            .expect("wrap");
        assert!(
            matches!(
                peel_welcome(&invitee, &wrap).await,
                Err(WelcomeError::Payload(_))
            ),
            "malformed v2 payload must refuse: {bad}"
        );
    }

    // the inner rumor of a real Welcome is UNSIGNED: a leaked 444 cannot be
    // republished as a valid event by whoever finds it
    let wrap = wrap_welcome(&founder, &invitee.public_key(), &payload())
        .await
        .expect("wrap");
    let unwrapped = nostr::nips::nip59::UnwrappedGift::from_gift_wrap(&invitee, &wrap)
        .await
        .expect("unwrap");
    let dumped = serde_json::to_string(&unwrapped.rumor).expect("serialize rumor");
    assert!(!dumped.contains("\"sig\""), "an unsigned rumor: {dumped}");
}

/// KEYSTONE (§4 size honesty) — a payload whose encoding exceeds the NIP-44
/// plaintext cap is refused LOUDLY at wrap time with the measured size, not
/// pushed into rust-nostr to fail as an opaque encrypt error (or worse,
/// into a relay to be dropped after the cursor advanced). A too-big
/// republic fails its founding with a real error message.
#[tokio::test]
async fn an_oversized_welcome_payload_is_refused_at_wrap_time() {
    let founder = Keys::generate();
    let invitee = Keys::generate();
    let fat = WelcomePayload {
        // hex doubles it: 40_000 bytes -> 80_000 chars > 65_408 cap
        welcome: vec![0xabu8; 40_000],
        rotation_seed: [7u8; 32],
        relays: vec!["wss://relay.example".to_string()],
    };
    match wrap_welcome(&founder, &invitee.public_key(), &fat).await {
        Err(WelcomeError::TooLarge { bytes, cap }) => {
            assert!(bytes > cap, "the error names the measured size and the cap");
        }
        other => panic!("an oversized payload must refuse with TooLarge, got {other:?}"),
    }
}
