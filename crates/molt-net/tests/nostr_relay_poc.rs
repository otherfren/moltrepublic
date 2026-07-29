// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N0 PoC: publish + subscribe through the rust-nostr relay pool, shaped like
//! the future NIP-EE traffic — a kind-445-style event whose `h` tag selects
//! the group and whose content is NIP-44 ciphertext.
//!
//! Two twins of the same roundtrip (the `ritual_over_smp` pattern):
//! - `roundtrip_over_local_relay` runs against an in-process relay
//!   (`nostr-relay-builder`), fast and offline — the future "LoopbackHub"
//!   equivalent for the Nostr runtime.
//! - `roundtrip_over_real_relay` is `#[ignore]` (real network):
//!
//! ```text
//! MOLT_NOSTR_RELAY=wss://... cargo test -p molt-net --test nostr_relay_poc -- --ignored --nocapture
//! ```

use std::time::Duration;

use nostr::nips::nip44;
use nostr::{
    Alphabet, EventBuilder, Filter, Keys, Kind, SingleLetterTag, Tag, Timestamp,
};
use nostr_relay_builder::MockRelay;
use nostr_relay_pool::pool::{RelayPool, RelayPoolNotification};
use nostr_relay_pool::relay::options::{RelayOptions, SubscribeOptions};

/// The group-message kind NIP-EE uses (Marmot: MLS application messages).
const KIND_GROUP_MESSAGE: Kind = Kind::Custom(445);

/// Publish an h-tagged kind-445-style event carrying NIP-44 ciphertext from
/// one pool, receive it on a second pool subscribed to the same `h` tag, and
/// decrypt. Exercises exactly what the transport needs from a relay:
/// publish, tag-filtered subscribe, delivery, and ciphertext fidelity.
async fn roundtrip(relay_url: &str) {
    let sender_keys = Keys::generate();
    let receiver_keys = Keys::generate();
    let group_h_tag = "746573742d67726f7570"; // opaque hex, like a rotated h tag

    // receiver pool subscribes to the group's h tag first
    let receiver_pool = RelayPool::new();
    receiver_pool
        .add_relay(relay_url, RelayOptions::default())
        .await
        .expect("receiver add_relay");
    receiver_pool.connect().await;
    let mut notifications = receiver_pool.notifications();
    let filter = Filter::new()
        .kind(KIND_GROUP_MESSAGE)
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), group_h_tag)
        .since(Timestamp::now() - Duration::from_secs(60));
    let sub = receiver_pool
        .subscribe(vec![filter], SubscribeOptions::default())
        .await
        .expect("subscribe");
    assert!(
        !sub.success.is_empty(),
        "at least one relay must accept the REQ (failed: {:?})",
        sub.failed
    );

    // sender publishes from an independent pool (a different "device")
    let plaintext = "molt PoC: greift die Republik ans Netz?";
    let ciphertext = nip44::encrypt(
        sender_keys.secret_key(),
        &receiver_keys.public_key(),
        plaintext,
        nip44::Version::V2,
    )
    .expect("nip44 encrypt");
    let event = EventBuilder::new(KIND_GROUP_MESSAGE, &ciphertext)
        .tag(Tag::parse(["h", group_h_tag]).expect("h tag"))
        .sign_with_keys(&sender_keys)
        .expect("sign");
    let event_id = event.id;

    let sender_pool = RelayPool::new();
    sender_pool
        .add_relay(relay_url, RelayOptions::default())
        .await
        .expect("sender add_relay");
    sender_pool.connect().await;
    let output = sender_pool.send_event(&event).await.expect("send_event");
    assert!(
        !output.success.is_empty(),
        "at least one relay must accept the publish (failed: {:?})",
        output.failed
    );

    // the subscribed receiver gets the event and decrypts it
    let received = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match notifications.recv().await.expect("notification stream") {
                RelayPoolNotification::Event { event, .. } if event.id == event_id => {
                    break *event;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("event must arrive within 20s");

    assert_eq!(received.kind, KIND_GROUP_MESSAGE);
    let decrypted = nip44::decrypt(
        receiver_keys.secret_key(),
        &sender_keys.public_key(),
        &received.content,
    )
    .expect("nip44 decrypt");
    assert_eq!(decrypted, plaintext);

    receiver_pool.disconnect().await;
    sender_pool.disconnect().await;
}

#[tokio::test]
async fn roundtrip_over_local_relay() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await;
    roundtrip(url.as_str()).await;
}

#[tokio::test]
#[ignore = "makes a real WebSocket connection to a public Nostr relay"]
async fn roundtrip_over_real_relay() {
    let url = std::env::var("MOLT_NOSTR_RELAY")
        .unwrap_or_else(|_| String::from("wss://relay.damus.io"));
    roundtrip(&url).await;
}
