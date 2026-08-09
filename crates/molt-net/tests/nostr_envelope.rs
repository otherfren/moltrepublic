// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! N3 envelope keystones (`docs_archive/transport/nostr_n3_plan.md` §2/§3): the
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

/// KEYSTONE (N3 §3) — the kind-445 SHAPE is exact, and its validator counts
/// OCCURRENCES rather than taking the first match: the peeler's 13-case
/// rejection table (`mdk_evaluation.md` §2.1), which is precisely the class
/// of code our own two CRITICAL findings came from. Nothing but one `h` tag
/// and at most one `expiration` may ride on a group event.
#[test]
fn the_445_tag_shape_is_exact() {
    use molt_net::envelope::{parse_445_tags, TagError};

    let h = "a".repeat(64);
    // the only two accepted shapes
    assert_eq!(
        parse_445_tags(&[vec!["h".into(), h.clone()]]).expect("bare h"),
        (h.clone(), None)
    );
    assert_eq!(
        parse_445_tags(&[
            vec!["h".into(), h.clone()],
            vec!["expiration".into(), "1760000000".into()],
        ])
        .expect("h + expiration"),
        (h.clone(), Some(1_760_000_000))
    );

    for (tags, want) in [
        (vec![], TagError::MissingH),
        (vec![vec!["h".into(), h.clone()], vec!["h".into(), h.clone()]], TagError::DuplicateH),
        (vec![vec!["h".into()]], TagError::ValuelessH),
        (
            vec![vec!["h".into(), h.clone(), "extra".into()]],
            TagError::OversizedH,
        ),
        (vec![vec![]], TagError::EmptyTag),
        (
            vec![vec!["h".into(), h.clone()], vec!["p".into(), "x".into()]],
            TagError::UnknownTag("p".into()),
        ),
        // the h value is lowercase hex of exactly 32 bytes
        (vec![vec!["h".into(), h.to_uppercase()]], TagError::BadHValue),
        (vec![vec!["h".into(), "a".repeat(62)]], TagError::BadHValue),
        (vec![vec!["h".into(), "z".repeat(64)]], TagError::BadHValue),
        (vec![vec!["h".into(), String::new()]], TagError::BadHValue),
        // expiration: at most one, a plain non-negative integer that fits
        (
            vec![
                vec!["h".into(), h.clone()],
                vec!["expiration".into(), "1".into()],
                vec!["expiration".into(), "2".into()],
            ],
            TagError::DuplicateExpiration,
        ),
        (
            vec![vec!["h".into(), h.clone()], vec!["expiration".into(), "-1".into()]],
            TagError::BadExpiration,
        ),
        (
            vec![
                vec!["h".into(), h.clone()],
                vec!["expiration".into(), "99999999999999999999999".into()],
            ],
            TagError::BadExpiration,
        ),
        (
            vec![vec!["h".into(), h.clone()], vec!["expiration".into()]],
            TagError::BadExpiration,
        ),
    ] {
        assert_eq!(parse_445_tags(&tags), Err(want.clone()), "tags {tags:?}");
    }
}

/// KEYSTONE (N3 §3, concept §4.4) — the h tag rotates DETERMINISTICALLY:
/// `h(window) = KDF(rotation_seed, floor(unix/86400))`, uniform 24h UTC
/// windows for every DAO (the crowd effect), no announcement and no grace.
/// An offline member re-derives the current tag AND every window it missed,
/// so nobody is ever stranded — that is what makes an announced rotation
/// (and its linkability) unnecessary.
#[test]
fn the_h_tag_rotates_deterministically_by_utc_day() {
    use molt_net::envelope::{h_tag, h_tags_for_catchup, H_WINDOW};

    let seed = [0x5a; 32];
    let other = [0x5b; 32];
    // midnight UTC boundary: 1760054400 is a multiple of 86400
    let boundary = 1_760_054_400;
    assert_eq!(boundary % H_WINDOW, 0, "the fixture sits on a UTC day boundary");

    let before = h_tag(&seed, boundary - 1);
    let at = h_tag(&seed, boundary);
    let later_same_day = h_tag(&seed, boundary + H_WINDOW - 1);
    assert_eq!(at, later_same_day, "one tag per 24h window");
    assert_ne!(before, at, "…and it changes at the boundary");
    assert_eq!(h_tag(&seed, boundary + H_WINDOW), h_tag(&seed, boundary + H_WINDOW));
    assert_ne!(h_tag(&seed, boundary + H_WINDOW), at, "the next window differs");

    // the tag VALUE is per-group secret (only the timing is uniform), and it
    // is a canonical 445 h value
    assert_ne!(h_tag(&other, boundary), at, "another group's tag is unrelated");
    assert_eq!(at.len(), 64);
    assert!(at.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert!(molt_net::envelope::parse_445_tags(&[vec!["h".into(), at.clone()]]).is_ok());

    // an offline member re-derives every window it missed — oldest first,
    // current last, bounded by the caller's horizon
    let missed = h_tags_for_catchup(&seed, boundary - 3 * H_WINDOW, boundary, 10);
    assert_eq!(missed.len(), 4, "three missed windows plus the current one");
    assert_eq!(missed.last(), Some(&at), "the current window closes the list");
    assert_eq!(missed[0], h_tag(&seed, boundary - 3 * H_WINDOW));
    // …and the horizon bounds it (a long-absent member does not ask a relay
    // for a year of tags)
    let capped = h_tags_for_catchup(&seed, 0, boundary, 5);
    assert_eq!(capped.len(), 5, "never more than the horizon");
    assert_eq!(capped.last(), Some(&at), "still anchored at the current window");
}
