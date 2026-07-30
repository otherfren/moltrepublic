// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N1 contract: the ticket-salted secp256k1 transport identity
//! (`molt_net::nostr_identity`, concept §3). One recovery phrase, three
//! anchors — the nostr key is salted with the seat's TICKET so the same
//! person presents a different npub in every republic (no cross-republic
//! correlation handle), and the derivation is deterministic so founding-time
//! derivation and any later re-derivation from the same material agree.

use molt_net::nostr_identity;

#[test]
fn derivation_is_deterministic_and_anchor_separated() {
    let entropy_a = [7u8; 32];
    let entropy_b = [8u8; 32];

    let (sk1, pk1) = nostr_identity(&entropy_a, "deadbeef");
    let (sk2, pk2) = nostr_identity(&entropy_a, "deadbeef");
    assert_eq!(sk1, sk2, "deterministic secret");
    assert_eq!(pk1, pk2, "deterministic anchor");

    assert_eq!(pk1.len(), 64, "32-byte x-only key, lowercase hex");
    assert_eq!(pk1, pk1.to_lowercase());

    let (_, pk_other_ticket) = nostr_identity(&entropy_a, "beefdead");
    assert_ne!(
        pk_other_ticket, pk1,
        "the ticket salts the derivation — one npub per republic"
    );
    let (_, pk_other_phrase) = nostr_identity(&entropy_b, "deadbeef");
    assert_ne!(pk_other_phrase, pk1, "the phrase owns the key");
}

#[test]
fn derived_keys_are_valid_bip340_and_consistent() {
    let (sk, pk_hex) = nostr_identity(&[9u8; 32], "deadbeef");
    let secret = nostr::SecretKey::from_slice(&sk).expect("valid secp256k1 scalar");
    let keys = nostr::Keys::new(secret);
    assert_eq!(
        keys.public_key().to_hex(),
        pk_hex,
        "the anchored pk IS the x-only public key of the derived secret"
    );
    nostr::PublicKey::from_hex(&pk_hex).expect("parses as a BIP-340 x-only key");
}
