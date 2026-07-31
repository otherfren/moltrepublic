// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N3 envelope keystones (`docs/transport/nostr_n3_plan.md` §2/§3): the
//! outer sealing of a kind-445 event under the epoch's exporter secret, and
//! its opening across the bounded exporter ring.

use molt_net::envelope::{open_outer, seal_outer, EnvelopeError};

const SECRET_A: [u8; 32] = [0x11; 32];
const SECRET_B: [u8; 32] = [0x22; 32];

/// KEYSTONE — the §10.11 shape roundtrips, and the ciphertext is
/// non-deterministic: the per-event nonce means two sealings of the SAME
/// plaintext under the SAME key never share bytes, so a relay cannot tell
/// repeated content apart.
#[test]
fn the_outer_layer_roundtrips_and_never_repeats_itself() {
    let plaintext = b"an MLS frame, opaque to the relay";
    let a = seal_outer(&SECRET_A, plaintext).expect("seal");
    let b = seal_outer(&SECRET_A, plaintext).expect("seal again");
    assert_ne!(a, b, "a fresh nonce per event — no repeated ciphertext");
    assert_eq!(
        open_outer(&[SECRET_A], &a).expect("open"),
        plaintext,
        "the sealed bytes come back exactly"
    );
    assert_eq!(open_outer(&[SECRET_A], &b).expect("open"), plaintext);
}

/// KEYSTONE — the ring is what makes catch-up across a re-key possible: an
/// event sealed under a PAST epoch's secret opens as long as that secret is
/// still in the ring, and the current secret is tried first (the common
/// case costs one AEAD attempt).
#[test]
fn a_past_epoch_event_opens_while_its_secret_is_in_the_ring() {
    let sealed_old = seal_outer(&SECRET_A, b"from the old epoch").expect("seal");
    // current secret first, then the ring
    assert_eq!(
        open_outer(&[SECRET_B, SECRET_A], &sealed_old).expect("ring hit"),
        b"from the old epoch"
    );
    // …and once the secret falls out of the ring the event is OPAQUE — a
    // distinct, honest error, never a silent skip (G4)
    assert_eq!(
        open_outer(&[SECRET_B], &sealed_old),
        Err(EnvelopeError::EpochOpaque)
    );
    assert_eq!(open_outer(&[], &sealed_old), Err(EnvelopeError::EpochOpaque));
}

/// Malformed input is refused by SHAPE before any key is tried, and every
/// tampered byte fails authentication — the AEAD tag covers the whole
/// ciphertext, so a relay cannot edit a frame in flight.
#[test]
fn malformed_and_tampered_frames_are_refused() {
    for junk in ["", "!!!!", "AAAA", &"A".repeat(30)] {
        assert!(
            matches!(open_outer(&[SECRET_A], junk), Err(EnvelopeError::Shape(_))),
            "must refuse {junk:?} on shape alone"
        );
    }
    let sealed = seal_outer(&SECRET_A, b"authentic").expect("seal");
    let raw = molt_net::envelope::decode_base64(&sealed).expect("decode");
    for flip in [0usize, 5, 12, raw.len() - 1] {
        let mut tampered = raw.clone();
        tampered[flip] ^= 0x01;
        let re = molt_net::envelope::encode_base64(&tampered);
        assert_eq!(
            open_outer(&[SECRET_A], &re),
            Err(EnvelopeError::EpochOpaque),
            "a flipped byte at {flip} must not authenticate"
        );
    }
}
