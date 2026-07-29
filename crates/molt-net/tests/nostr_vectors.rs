// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N0 pin: rust-nostr's NIP-44 v2 against the OFFICIAL reference vectors
//! (`tests/vectors/nip44.vectors.json`, vendored verbatim from
//! paulmillr/nip44), plus a NIP-59 gift-wrap roundtrip property.
//!
//! These are the byte fixtures the Nostr transport concept (§12 "spec churn")
//! demands: if a rust-nostr upgrade changes envelope crypto behavior, a test
//! here goes red before anything reaches a relay. Encryption is pinned
//! byte-exactly by injecting the vector nonce through a fixed `RngCore`.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use nostr::nips::nip44::{self, v2};
use nostr::secp256k1::rand::{CryptoRng, RngCore};
use nostr::{EventBuilder, Keys, Kind, PublicKey, SecretKey};
use sha2::{Digest, Sha256};

const VECTORS: &str = include_str!("vectors/nip44.vectors.json");

fn vectors() -> serde_json::Value {
    let root: serde_json::Value = serde_json::from_str(VECTORS).expect("vectors json parses");
    root.get("v2").expect("v2 section").clone()
}

fn field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or_else(|| panic!("string field {key}"))
}

fn conv_key(v: &serde_json::Value) -> v2::ConversationKey {
    let bytes = hex::decode(field(v, "conversation_key")).expect("conversation_key hex");
    v2::ConversationKey::from_slice(&bytes).expect("conversation key")
}

fn nonce32(v: &serde_json::Value) -> [u8; 32] {
    let bytes = hex::decode(field(v, "nonce")).expect("nonce hex");
    bytes.try_into().expect("32-byte nonce")
}

/// An `RngCore` that yields exactly one fixed 32-byte nonce — the reference
/// vectors specify the nonce, `v2::encrypt_to_bytes_with_rng` draws it from
/// the rng, so this pins the ENCRYPT direction byte-exactly.
struct FixedNonce([u8; 32]);

impl RngCore for FixedNonce {
    fn next_u32(&mut self) -> u32 {
        unreachable!("nip44 encrypt draws the nonce via fill_bytes only")
    }
    fn next_u64(&mut self) -> u64 {
        unreachable!("nip44 encrypt draws the nonce via fill_bytes only")
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        assert_eq!(dest.len(), 32, "nip44 draws exactly the 32-byte nonce");
        dest.copy_from_slice(&self.0);
    }
    fn try_fill_bytes(
        &mut self,
        dest: &mut [u8],
    ) -> Result<(), nostr::secp256k1::rand::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

// The fixed nonce is not cryptographically random — that is the point of a
// test vector. Marking it CryptoRng is confined to this test crate.
impl CryptoRng for FixedNonce {}

/// The public decrypt flow (base64 → version byte → v2), replicated for
/// vectors that carry a conversation key instead of a key pair — identical
/// dispatch to `nip44::decrypt_to_bytes`.
fn decrypt_b64(ck: &v2::ConversationKey, payload: &str) -> Result<Vec<u8>, String> {
    let bytes = B64.decode(payload).map_err(|e| format!("base64: {e}"))?;
    match bytes.first() {
        Some(2) => v2::decrypt_to_bytes(ck, &bytes).map_err(|e| format!("v2: {e}")),
        Some(v) => Err(format!("unknown version {v}")),
        None => Err("empty payload".into()),
    }
}

#[test]
fn valid_conversation_keys_derive_exactly() {
    let v2s = vectors();
    let cases = v2s["valid"]["get_conversation_key"]
        .as_array()
        .expect("cases");
    assert_eq!(cases.len(), 35, "official vector count");
    for case in cases {
        let sec1 = SecretKey::from_hex(field(case, "sec1")).expect("sec1");
        let pub2 = PublicKey::from_hex(field(case, "pub2")).expect("pub2");
        let ck = v2::ConversationKey::derive(&sec1, &pub2).expect("derive");
        assert_eq!(
            hex::encode(ck.as_bytes()),
            field(case, "conversation_key"),
            "conversation key mismatch for sec1={}",
            field(case, "sec1"),
        );
    }
}

#[test]
fn invalid_conversation_keys_are_rejected() {
    let v2s = vectors();
    let cases = v2s["invalid"]["get_conversation_key"]
        .as_array()
        .expect("cases");
    assert_eq!(cases.len(), 8, "official vector count");
    for case in cases {
        let note = field(case, "note");
        let sec1 = SecretKey::from_hex(field(case, "sec1"));
        let pub2 = PublicKey::from_hex(field(case, "pub2"));
        let derived = match (sec1, pub2) {
            (Ok(s), Ok(p)) => v2::ConversationKey::derive(&s, &p).map(|_| ()),
            // rejected already at key parse — that IS the required rejection
            _ => Err(nip44::Error::UnknownVersion(0)),
        };
        assert!(derived.is_err(), "must reject: {note}");
    }
}

#[test]
fn valid_encrypt_decrypt_byte_exact() {
    let v2s = vectors();
    let cases = v2s["valid"]["encrypt_decrypt"].as_array().expect("cases");
    assert_eq!(cases.len(), 10, "official vector count");
    for case in cases {
        let sec1 = SecretKey::from_hex(field(case, "sec1")).expect("sec1");
        let sec2 = SecretKey::from_hex(field(case, "sec2")).expect("sec2");
        let pk1 = Keys::new(sec1.clone()).public_key();
        let pk2 = Keys::new(sec2.clone()).public_key();
        let plaintext = field(case, "plaintext");
        let payload = field(case, "payload");

        // both derivation directions agree with the vector
        let ck12 = v2::ConversationKey::derive(&sec1, &pk2).expect("derive 1->2");
        let ck21 = v2::ConversationKey::derive(&sec2, &pk1).expect("derive 2->1");
        assert_eq!(hex::encode(ck12.as_bytes()), field(case, "conversation_key"));
        assert_eq!(ck12.as_bytes(), ck21.as_bytes(), "ECDH must be symmetric");

        // encrypt with the vector nonce → byte-exact payload
        let encrypted =
            v2::encrypt_to_bytes_with_rng(&mut FixedNonce(nonce32(case)), &ck12, plaintext.as_bytes())
                .expect("encrypt");
        assert_eq!(B64.encode(&encrypted), payload, "payload mismatch for {plaintext:?}");

        // decrypt through the conversation-key path and the keypair path
        let via_ck = decrypt_b64(&ck12, payload).expect("decrypt via ck");
        assert_eq!(via_ck, plaintext.as_bytes());
        let via_keys = nip44::decrypt(&sec2, &pk1, payload).expect("decrypt via keys");
        assert_eq!(via_keys, plaintext);
    }
}

/// rust-nostr's `pad()` caps plaintext at `65536 - 128` = 65408 bytes, but
/// the NIP-44 spec (and the official vectors) allow up to 65535 — verified
/// unfixed in 0.44.6 AND 0.45.0-alpha.7. The decrypt path has no such cap,
/// so the deviation is send-side only. Harmless for us (the 445-level
/// chunker stays far below 64 KiB) but pinned here as a canary: ALL THREE
/// official long-message vectors (65535, 65535, and 4×16383 = 65532 bytes)
/// are over the cap today. When an upgrade lifts it, the full byte-exact
/// pin below activates by itself — then update the pinned count.
const RUST_NOSTR_MAX_PLAINTEXT: usize = 65536 - 128;

#[test]
fn valid_encrypt_decrypt_long_messages() {
    let v2s = vectors();
    let cases = v2s["valid"]["encrypt_decrypt_long_msg"]
        .as_array()
        .expect("cases");
    assert_eq!(cases.len(), 3, "official vector count");
    let mut pinned_full = 0usize;
    for case in cases {
        let ck = conv_key(case);
        let pattern = field(case, "pattern");
        let repeat = case["repeat"].as_u64().expect("repeat count");
        let repeat = usize::try_from(repeat).expect("repeat fits usize");
        let plaintext = pattern.repeat(repeat);

        let plaintext_sha = hex::encode(Sha256::digest(plaintext.as_bytes()));
        assert_eq!(plaintext_sha, field(case, "plaintext_sha256"), "plaintext hash");

        let encrypted = v2::encrypt_to_bytes_with_rng(
            &mut FixedNonce(nonce32(case)),
            &ck,
            plaintext.as_bytes(),
        );
        if plaintext.len() > RUST_NOSTR_MAX_PLAINTEXT {
            // KNOWN UPSTREAM DEVIATION (see RUST_NOSTR_MAX_PLAINTEXT)
            assert!(
                encrypted.is_err(),
                "deviation healed upstream — promote this vector to the full pin"
            );
            continue;
        }
        let encrypted = encrypted.expect("encrypt");
        let payload = B64.encode(&encrypted);
        let payload_sha = hex::encode(Sha256::digest(payload.as_bytes()));
        assert_eq!(payload_sha, field(case, "payload_sha256"), "payload hash");

        let decrypted = decrypt_b64(&ck, &payload).expect("decrypt");
        assert_eq!(decrypted, plaintext.as_bytes());
        pinned_full += 1;
    }
    assert_eq!(
        pinned_full, 0,
        "cap lifted upstream — the long vectors are now byte-pinned; \
         update this count and RUST_NOSTR_MAX_PLAINTEXT"
    );
}

/// The exact boundary of the deviation: 65408 encrypts, 65409 is rejected.
/// The spec max is 65535 — if this test starts failing after a rust-nostr
/// upgrade, the cap moved (hopefully to the spec value): update
/// `RUST_NOSTR_MAX_PLAINTEXT` and promote the long-message vectors.
#[test]
fn rust_nostr_max_plaintext_deviation_canary() {
    let ck = v2::ConversationKey::new([7u8; 32]);
    let at_cap = vec![b'x'; RUST_NOSTR_MAX_PLAINTEXT];
    assert!(v2::encrypt_to_bytes(&ck, &at_cap).is_ok(), "65408 must encrypt");
    let over_cap = vec![b'x'; RUST_NOSTR_MAX_PLAINTEXT + 1];
    assert!(
        v2::encrypt_to_bytes(&ck, &over_cap).is_err(),
        "cap moved — re-pin RUST_NOSTR_MAX_PLAINTEXT and the long vectors"
    );
}

/// Pins the vectors' `get_message_keys` section (HKDF-expand of the
/// conversation key + nonce into chacha_key ‖ chacha_nonce ‖ hmac_key).
/// The keys are private in rust-nostr, so the pin is indirect but exact:
/// recompute the expected payload FROM the reference keys (ChaCha20 over the
/// padded plaintext, HMAC over nonce‖ciphertext) and require the library's
/// fixed-nonce encryption to produce the identical bytes — any deviation in
/// the key schedule changes the payload.
#[test]
fn message_key_schedule_matches_reference() {
    use chacha20::cipher::{KeyIvInit, StreamCipher};
    use hmac::{Hmac, Mac};

    let v2s = vectors();
    let section = &v2s["valid"]["get_message_keys"];
    let ck_bytes = hex::decode(field(section, "conversation_key")).expect("ck hex");
    let ck = v2::ConversationKey::from_slice(&ck_bytes).expect("conversation key");
    let cases = section["keys"].as_array().expect("keys");
    assert_eq!(cases.len(), 32, "official vector count");

    // plaintext "a" pads to exactly [len_be16=1] ‖ 'a' ‖ 29 zeros (32-byte pad)
    let mut padded = vec![0u8, 1, b'a'];
    padded.resize(2 + 32, 0);

    for case in cases {
        let nonce = nonce32(case);
        let chacha_key: [u8; 32] = hex::decode(field(case, "chacha_key"))
            .expect("chacha_key hex")
            .try_into()
            .expect("32 bytes");
        let chacha_nonce: [u8; 12] = hex::decode(field(case, "chacha_nonce"))
            .expect("chacha_nonce hex")
            .try_into()
            .expect("12 bytes");
        let hmac_key = hex::decode(field(case, "hmac_key")).expect("hmac_key hex");

        let mut ciphertext = padded.clone();
        chacha20::ChaCha20::new(&chacha_key.into(), &chacha_nonce.into())
            .apply_keystream(&mut ciphertext);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&hmac_key).expect("hmac key length");
        mac.update(&nonce);
        mac.update(&ciphertext);
        let tag = mac.finalize().into_bytes();

        let mut expected = vec![2u8];
        expected.extend_from_slice(&nonce);
        expected.extend_from_slice(&ciphertext);
        expected.extend_from_slice(&tag);

        let actual = v2::encrypt_to_bytes_with_rng(&mut FixedNonce(nonce), &ck, b"a")
            .expect("encrypt");
        assert_eq!(
            actual,
            expected,
            "message-key schedule mismatch for nonce {}",
            field(case, "nonce"),
        );
    }
}

#[test]
fn invalid_payloads_are_rejected() {
    let v2s = vectors();
    let cases = v2s["invalid"]["decrypt"].as_array().expect("cases");
    assert_eq!(cases.len(), 12, "official vector count");
    for case in cases {
        let ck = conv_key(case);
        let note = field(case, "note");
        let result = decrypt_b64(&ck, field(case, "payload"));
        assert!(result.is_err(), "must reject: {note}");
    }
}

#[test]
fn invalid_message_lengths_are_rejected() {
    let v2s = vectors();
    let lengths = v2s["invalid"]["encrypt_msg_lengths"]
        .as_array()
        .expect("lengths");
    assert_eq!(lengths.len(), 4, "official vector count");
    let ck = v2::ConversationKey::new([7u8; 32]);
    for len in lengths {
        let len = usize::try_from(len.as_u64().expect("length")).expect("fits usize");
        let plaintext = vec![b'x'; len];
        let result = v2::encrypt_to_bytes(&ck, &plaintext);
        assert!(result.is_err(), "must reject plaintext of length {len}");
    }
}

#[test]
fn padding_scheme_matches_reference() {
    let v2s = vectors();
    let cases = v2s["valid"]["calc_padded_len"].as_array().expect("cases");
    assert_eq!(cases.len(), 24, "official vector count");
    let ck = v2::ConversationKey::new([7u8; 32]);
    for pair in cases {
        let unpadded =
            usize::try_from(pair[0].as_u64().expect("unpadded")).expect("fits usize");
        let padded = usize::try_from(pair[1].as_u64().expect("padded")).expect("fits usize");
        if unpadded > RUST_NOSTR_MAX_PLAINTEXT {
            // the [65536, 65536] pair exercises only the padding FUNCTION,
            // which is private in rust-nostr; unreachable through encrypt
            // (over the spec max anyway) — nothing to pin via the public API
            continue;
        }
        let plaintext = vec![b'x'; unpadded];
        let encrypted =
            v2::encrypt_to_bytes_with_rng(&mut FixedNonce([9u8; 32]), &ck, &plaintext)
                .expect("encrypt");
        // payload = version(1) ‖ nonce(32) ‖ [len(2) ‖ padded content] ‖ hmac(32)
        assert_eq!(
            encrypted.len(),
            1 + 32 + 2 + padded + 32,
            "padded length mismatch for unpadded={unpadded}",
        );
    }
}

/// NIP-59 has no fixed reference vectors (timestamps and ephemeral keys are
/// randomized by design) — the pin is the roundtrip property plus the
/// structural facts the transport depends on: outer kind 1059, ephemeral
/// (non-sender) outer author, and third parties being unable to unwrap.
#[tokio::test]
async fn nip59_gift_wrap_roundtrip() {
    let sender = Keys::generate();
    let receiver = Keys::generate();
    let rumor = EventBuilder::text_note("ritual envelope test").build(sender.public_key());

    let wrap = EventBuilder::gift_wrap(&sender, &receiver.public_key(), rumor.clone(), [])
        .await
        .expect("gift wrap");

    assert_eq!(wrap.kind, Kind::GiftWrap, "outer kind must be 1059");
    assert_ne!(
        wrap.pubkey,
        sender.public_key(),
        "outer author must be an ephemeral key, not the sender"
    );
    wrap.verify().expect("outer event must verify");

    let unwrapped = nostr::nips::nip59::UnwrappedGift::from_gift_wrap(&receiver, &wrap)
        .await
        .expect("receiver unwraps");
    assert_eq!(unwrapped.sender, sender.public_key(), "seal binds the sender");
    assert_eq!(unwrapped.rumor.content, rumor.content);
    assert_eq!(unwrapped.rumor.kind, rumor.kind);
    assert_eq!(unwrapped.rumor.pubkey, sender.public_key());

    let outsider = Keys::generate();
    let stolen = nostr::nips::nip59::UnwrappedGift::from_gift_wrap(&outsider, &wrap).await;
    assert!(stolen.is_err(), "a third party must not be able to unwrap");
}
