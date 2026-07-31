// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N3 step 6 (`docs/transport/nostr_n3_plan.md` §4): the kind-444 Welcome,
//! NIP-59 gift-wrapped to the invitee's transport anchor. The inner rumor is
//! deliberately UNSIGNED — a leaked 444 is not publishable — and the peel
//! chain is fail-closed: every step that cannot be verified refuses.

use molt_net::welcome::{peel_welcome, wrap_welcome, WelcomeError};
use nostr::Keys;

/// KEYSTONE — the Welcome roundtrips to the intended recipient only, and
/// the wrapper leaks neither the sender nor the payload: the outer event is
/// kind 1059 authored by a FRESH ephemeral key (never the founder's anchor),
/// so a relay sees an anonymous gift to one pubkey and nothing else.
#[tokio::test]
async fn a_welcome_reaches_its_invitee_and_names_no_sender() {
    let founder = Keys::generate();
    let invitee = Keys::generate();
    let outsider = Keys::generate();
    let welcome_bytes = b"an MLS Welcome blob".to_vec();

    let wrap = wrap_welcome(&founder, &invitee.public_key(), &welcome_bytes)
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
        !json.contains(&hex::encode(&welcome_bytes)),
        "the Welcome bytes must not be readable on the wire"
    );

    let (peeled, from) = peel_welcome(&invitee, &wrap).await.expect("invitee peels");
    assert_eq!(peeled, welcome_bytes);
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
/// inner rumor is not a kind-444 Welcome, or whose payload is not the
/// agreed encoding, is refused rather than half-interpreted. A leaked 444
/// is also not publishable — the rumor carries no signature.
#[tokio::test]
async fn the_peel_chain_refuses_anything_that_is_not_a_welcome() {
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

    // right kind, payload that is not the agreed encoding
    let rumor = EventBuilder::new(Kind::Custom(444), "!!! not hex !!!").build(founder.public_key());
    let wrap = EventBuilder::gift_wrap(&founder, &invitee.public_key(), rumor, [])
        .await
        .expect("wrap");
    assert!(matches!(
        peel_welcome(&invitee, &wrap).await,
        Err(WelcomeError::Payload(_))
    ));

    // the inner rumor of a real Welcome is UNSIGNED: a leaked 444 cannot be
    // republished as a valid event by whoever finds it
    let wrap = wrap_welcome(&founder, &invitee.public_key(), b"blob")
        .await
        .expect("wrap");
    let unwrapped = nostr::nips::nip59::UnwrappedGift::from_gift_wrap(&invitee, &wrap)
        .await
        .expect("unwrap");
    // the rumor is an `UnsignedEvent` by TYPE — there is no signature field
    // to fill, so a leaked 444 cannot be republished as a valid event. The
    // serialized form must therefore carry no `sig` either.
    let dumped = serde_json::to_string(&unwrapped.rumor).expect("serialize rumor");
    assert!(!dumped.contains("\"sig\""), "an unsigned rumor: {dumped}");
}
