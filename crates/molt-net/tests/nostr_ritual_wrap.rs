// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N4a step 2 (`docs_archive/transport/nostr_n4_plan.md` §2): the kind-446 ritual
//! rumor — every founder↔joiner pre-group leg (JoinRequest, JoinAccepted,
//! LinkSpent) rides the existing `RitualMsg` JSON vocabulary inside a NIP-59
//! gift wrap. The peel chain is fail-closed like the 444's, and — §2.1 —
//! the peeled sender is CRYPTOGRAPHICALLY the seal author: a wrap whose
//! rumor claims a different author than the key that sealed it is refused,
//! which is what upgrades the third anchor to proof-of-possession at join.

use molt_net::ritual_wrap::{peel_ritual, wrap_ritual, RitualWrapError, KIND_RITUAL};
use molt_net::RitualMsg;
use nostr::Keys;

/// KEYSTONE — a RitualMsg roundtrips to the intended recipient only; the
/// outer 1059 is ephemeral-authored; the peeled sender is the sealer's key.
#[tokio::test]
async fn a_ritual_msg_reaches_its_recipient_and_names_its_sealer() {
    let joiner = Keys::generate();
    let founder = Keys::generate();
    let outsider = Keys::generate();

    let msg = RitualMsg::JoinAccepted { seat: 3 };
    let wrap = wrap_ritual(&joiner, &founder.public_key(), &msg)
        .await
        .expect("wrap");

    assert_eq!(wrap.kind.as_u16(), 1059, "outer kind is the NIP-59 gift wrap");
    assert_ne!(
        wrap.pubkey,
        joiner.public_key(),
        "the outer author is ephemeral — the sender's anchor never appears"
    );
    wrap.verify().expect("the outer event verifies");

    let (peeled, from) = peel_ritual(&founder, &wrap).await.expect("recipient peels");
    assert!(matches!(peeled, RitualMsg::JoinAccepted { seat: 3 }));
    assert_eq!(
        from,
        joiner.public_key(),
        "the peeled sender is the key that sealed the wrap"
    );

    assert!(matches!(
        peel_ritual(&outsider, &wrap).await,
        Err(RitualWrapError::NotForUs)
    ));
}

/// KEYSTONE — fail-closed negatives: wrong inner kind (a 444 Welcome peeled
/// as a ritual message), and content that is not the RitualMsg vocabulary.
#[tokio::test]
async fn the_ritual_peel_refuses_wrong_kind_and_wrong_vocabulary() {
    use nostr::{EventBuilder, Kind};

    let sender = Keys::generate();
    let recipient = Keys::generate();

    // right wrapper, wrong inner kind
    let rumor =
        EventBuilder::new(Kind::Custom(444), "0abc").build(sender.public_key());
    let wrap = EventBuilder::gift_wrap(&sender, &recipient.public_key(), rumor, [])
        .await
        .expect("wrap");
    assert!(matches!(
        peel_ritual(&recipient, &wrap).await,
        Err(RitualWrapError::NotARitual { kind: 444 })
    ));

    // right kind, content that is not a RitualMsg
    let rumor = EventBuilder::new(Kind::Custom(KIND_RITUAL), "{\"kind\":\"nope\"}")
        .build(sender.public_key());
    let wrap = EventBuilder::gift_wrap(&sender, &recipient.public_key(), rumor, [])
        .await
        .expect("wrap");
    assert!(matches!(
        peel_ritual(&recipient, &wrap).await,
        Err(RitualWrapError::Payload(_))
    ));
}

/// KEYSTONE (§2.1, the PoP pin) — a wrap whose rumor CLAIMS one author but
/// was sealed by a different key is refused on peel. This is the property
/// the founder's JoinRequest ingest leans on: the claimed `nostr_pk` in the
/// request must equal the peeled sender, and the peeled sender cannot be
/// forged without the corresponding secret.
#[tokio::test]
async fn a_wrap_sealed_by_a_key_other_than_its_claimed_author_is_refused() {
    use nostr::{EventBuilder, Kind};

    let real_sealer = Keys::generate();
    let claimed_author = Keys::generate(); // the lie
    let recipient = Keys::generate();

    // rumor claims `claimed_author`, but the seal is made and signed by
    // `real_sealer`
    let rumor = EventBuilder::new(
        Kind::Custom(KIND_RITUAL),
        serde_json::to_string(&RitualMsg::JoinAccepted { seat: 1 }).expect("encode"),
    )
    .build(claimed_author.public_key());
    let seal = EventBuilder::seal(&real_sealer, &recipient.public_key(), rumor)
        .await
        .expect("seal")
        .sign_with_keys(&real_sealer)
        .expect("sign seal");
    let wrap = EventBuilder::gift_wrap_from_seal(&recipient.public_key(), &seal, [])
        .expect("wrap from seal");

    assert!(
        peel_ritual(&recipient, &wrap).await.is_err(),
        "a rumor/seal author mismatch must refuse — the peeled sender is proven, not claimed"
    );
}

/// B4: a full anchor piece (24 KiB of event JSON, base64 data as a blob
/// part carries it) stays under 60 KiB once gift-wrapped - two NIP-44
/// layers grow it by about 16/9 - and still peels.
#[tokio::test]
async fn a_full_anchor_part_stays_under_sixty_kib_on_the_wire() {
    use base64::Engine as _;
    let raw: Vec<u8> = (0..18_000u32).map(|i| u8::try_from(i % 251).expect("byte")).collect();
    let data = base64::engine::general_purpose::STANDARD.encode(&raw);
    let event = format!(
        r#"{{"kind":"checkpoint_served_part","blob_hash":"{}","index":7,"total":9,"data":"{data}"}}"#,
        "ab".repeat(32)
    );
    assert!(event.len() <= 24 * 1024, "{}", event.len());
    let msg = RitualMsg::RecoverAnchor { member: "petra".into(), index: 7, total: 9, event };
    let coordinator = Keys::generate();
    let rejoiner = Keys::generate();
    let wrap = wrap_ritual(&coordinator, &rejoiner.public_key(), &msg).await.expect("wrap");
    let wire = nostr::JsonUtil::as_json(&wrap).len();
    eprintln!("anchor part: frame={} wrapped={wire}", serde_json::to_string(&msg).expect("json").len());
    assert!(wire <= 60 * 1024, "a gift-wrapped part is {wire} B");
    let (peeled, _) = peel_ritual(&rejoiner, &wrap).await.expect("peels");
    assert_eq!(peeled, msg);
}
