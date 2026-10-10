// SPDX-License-Identifier: GPL-3.0-or-later
//! The run's DKG layer: context, view key, transcript, equivocation and the
//! identity-point gate (plan §10.1-7, §10.13).

mod common;

use std::collections::BTreeMap;

use common::{others, run, run_id};
use molt_core::{put_bytes, put_count};
use molt_treasury::dkg::{self, Frames, Inbox};
use molt_treasury::keys;
use molt_treasury::rand_core::SeedableRng;
use molt_treasury::{Ed25519, Network, RunId, ThresholdKeys, TreasuryError};
use monero_wallet::address::{AddressType, MoneroAddress};
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};

const IDENTITY: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 1;
    b
};

fn hex32(b: [u8; 32]) -> String {
    hex::encode(b)
}

#[test]
fn a_three_seat_dkg_agrees_on_key_view_and_address() {
    let id = run_id(1);
    let r = run(&id, 2, 3, 11);
    let group = r.keys[0].group_key();
    assert!(r.keys.iter().all(|k| k.group_key() == group));

    let view = keys::view_key(&id, &r.round1, 3).expect("view");
    let addr = keys::standard_address(group, view.clone(), Network::Mainnet).expect("address");
    for k in &r.keys {
        let back = ThresholdKeys::<Ed25519>::read(&mut k.serialize().as_slice()).expect("read");
        assert_eq!(back.group_key(), group);
        let again = keys::view_key(&id, &r.round1, 3).expect("view");
        assert!(*again == *view);
        let a = keys::standard_address(back.group_key(), again, Network::Mainnet).expect("addr");
        assert_eq!(a.to_string(), addr.to_string());
    }
}

#[test]
fn the_context_binds_republic_init_and_run() {
    let base = run_id(1);
    let c = dkg::context(&base, 2, 3);
    let variants = [
        RunId {
            republic_id: [2; 32],
            ..base
        },
        RunId { init_id: 8, ..base },
        RunId {
            run: [0; 32],
            ..base
        },
    ];
    for v in variants {
        assert_ne!(dkg::context(&v, 2, 3), c);
    }
    assert_ne!(dkg::context(&base, 3, 3), c);
    assert_ne!(dkg::context(&base, 2, 4), c);
    assert_eq!(dkg::context(&base, 2, 3), c);
}

#[test]
fn view_key_depends_on_every_contribution() {
    let id = run_id(3);
    let r = run(&id, 2, 3, 12);
    let view = keys::view_key(&id, &r.round1, 3).expect("view");
    for i in 1..=3u16 {
        let mut f = r.round1.clone();
        f.get_mut(&i).expect("frame")[0] ^= 1;
        let other = keys::view_key(&id, &f, 3).expect("view");
        assert!(*other != *view, "contribution {i} ignored");
    }
    let mut short = r.round1.clone();
    short.remove(&2);
    assert!(keys::view_key(&id, &short, 3).is_err());
    let other_run = RunId { run: [9; 32], ..id };
    assert!(*keys::view_key(&other_run, &r.round1, 3).expect("view") != *view);
}

#[test]
fn a_differing_transcript_aborts() {
    let id = run_id(4);
    let r = run(&id, 2, 3, 13);
    let t = dkg::transcript(&id.run, &r.round1, 3).expect("transcript");
    assert_eq!(dkg::transcript(&id.run, &r.round1, 3).expect("again"), t);
    dkg::check_transcript(&t, 2, &t).expect("same transcript");

    let mut f = r.round1.clone();
    *f.get_mut(&3).expect("frame").last_mut().expect("byte") ^= 1;
    let other = dkg::transcript(&id.run, &f, 3).expect("transcript");
    assert_ne!(other, t);
    assert_eq!(
        dkg::check_transcript(&t, 2, &other),
        Err(TreasuryError::Transcript(2))
    );
    assert_ne!(dkg::transcript(&[0; 32], &r.round1, 3).expect("t"), t);
    let mut short = r.round1.clone();
    short.remove(&1);
    assert!(dkg::transcript(&id.run, &short, 3).is_err());
}

#[test]
fn two_round_one_frames_from_one_sender_abort() {
    let id = run_id(5);
    let r = run(&id, 2, 3, 14);
    let mut inbox = Inbox::new(3);
    assert_eq!(inbox.record(1, r.round1[&1].clone()), Ok(true));
    assert_eq!(inbox.record(1, r.round1[&1].clone()), Ok(false));
    assert!(!inbox.is_complete());
    let mut forged = r.round1[&1].clone();
    forged[40] ^= 1;
    assert_eq!(inbox.record(1, forged), Err(TreasuryError::Equivocation(1)));
    assert_eq!(inbox.record(0, vec![1]), Err(TreasuryError::Frame(0)));
    assert_eq!(inbox.record(4, vec![1]), Err(TreasuryError::Frame(4)));
    inbox.record(2, r.round1[&2].clone()).expect("2");
    inbox.record(3, r.round1[&3].clone()).expect("3");
    assert!(inbox.is_complete());
    assert_eq!(inbox.frames(), &r.round1);

    let mut shares = Inbox::others(3, 1);
    assert_eq!(shares.record(1, vec![1]), Err(TreasuryError::Frame(1)));
    shares.record(2, r.shares[&1][&2].clone()).expect("2");
    assert_eq!(
        shares.record(2, vec![0; 128]),
        Err(TreasuryError::Equivocation(2))
    );
    shares.record(3, r.shares[&1][&3].clone()).expect("3");
    assert!(shares.is_complete());
}

#[test]
fn an_inbox_shows_its_senders_not_their_bytes() {
    let mut inbox = Inbox::new(3);
    inbox.record(1, vec![0xab; 64]).expect("1");
    let shown = format!("{inbox:?}");
    assert!(!shown.contains("171"), "{shown}");
    assert!(shown.contains("Inbox"), "{shown}");
}

/// A grown buffer leaves an unwiped copy of its secret behind.
#[test]
fn secret_buffers_are_sized_once() {
    let id = run_id(6);
    let r = run(&id, 2, 3, 15);
    let pre = keys::view_preimage(&id, &r.round1, 3).expect("preimage");
    assert_eq!(pre.capacity(), pre.len());
    let pre = dkg::transcript_preimage(&id.run, &r.round1, 3).expect("transcript");
    assert_eq!(pre.capacity(), pre.len());
    let mut rng = ChaCha20Rng::seed_from_u64(1);
    let p = dkg::params(2, 3, 1).expect("params");
    let (_, msg) = dkg::round1(p, dkg::context(&id, 2, 3), &mut rng);
    assert_eq!(msg.capacity(), msg.len());
}

/// Every 32-byte point position of a `t`-of-`n` round-1 message
/// (`c ‖ t commitments ‖ PoK R ‖ PoK s ‖ encryption key`).
fn round1_points(t: usize) -> Vec<usize> {
    let mut at: Vec<usize> = (0..t).map(|k| 32 + 32 * k).collect();
    at.push(32 + 32 * t);
    at.push(32 + 32 * t + 64);
    at
}

#[test]
fn an_identity_point_in_either_frame_aborts() {
    let (t, n) = (2u16, 3u16);
    let id = run_id(6);
    let ctx = dkg::context(&id, t, n);
    let p: Vec<_> = (1..=n).map(|i| dkg::params(t, n, i).expect("p")).collect();
    let mut rng = ChaCha20Rng::from_seed([15; 32]);
    let msgs: Vec<_> = p.iter().map(|pi| dkg::round1(*pi, ctx, &mut rng)).collect();
    let round1: Frames = msgs
        .iter()
        .enumerate()
        .map(|(k, (_, m))| (u16::try_from(k + 1).expect("k"), m.to_vec()))
        .collect();

    for at in round1_points(usize::from(t)) {
        let (seat1, _) = dkg::round1(p[0], ctx, &mut rng);
        let mut f = others(&round1, 1);
        f.get_mut(&3).expect("3")[at..at + 32].copy_from_slice(&IDENTITY);
        let err = dkg::round2(seat1, p[0], &f, &mut rng).expect_err("identity");
        assert_eq!(err, TreasuryError::Identity(3), "round-1 offset {at}");
    }

    // round 2: the per-message key and the PoP nonce
    for at in [0usize, 32] {
        let mut rng = ChaCha20Rng::from_seed([16; 32]);
        let msgs: Vec<_> = p.iter().map(|pi| dkg::round1(*pi, ctx, &mut rng)).collect();
        let round1: Frames = msgs
            .iter()
            .enumerate()
            .map(|(k, (_, m))| (u16::try_from(k + 1).expect("k"), m.to_vec()))
            .collect();
        let mut machines = msgs.into_iter().map(|(m, _)| m);
        let (km1, _) = dkg::round2(
            machines.next().expect("1"),
            p[0],
            &others(&round1, 1),
            &mut rng,
        )
        .expect("r2 seat 1");
        let mut to1 = Frames::new();
        for (k, m) in machines.enumerate() {
            let me = u16::try_from(k + 2).expect("k");
            let (_, out) =
                dkg::round2(m, p[usize::from(me) - 1], &others(&round1, me), &mut rng).expect("r2");
            to1.insert(me, out[&1].clone());
        }
        to1.get_mut(&2).expect("2")[at..at + 32].copy_from_slice(&IDENTITY);
        let err = dkg::complete(km1, p[0], &to1, &mut rng).expect_err("identity");
        assert_eq!(err, TreasuryError::Identity(2), "round-2 offset {at}");
    }
}

#[test]
fn wire_indexes_are_validated_before_the_library() {
    let (t, n) = (2u16, 3u16);
    let id = run_id(7);
    let ctx = dkg::context(&id, t, n);
    let p: Vec<_> = (1..=n).map(|i| dkg::params(t, n, i).expect("p")).collect();
    let mut rng = ChaCha20Rng::from_seed([17; 32]);
    let r: Vec<_> = p
        .iter()
        .map(|pi| dkg::round1(*pi, ctx, &mut rng).1.to_vec())
        .collect();
    for bad in [
        BTreeMap::from([(2, r[1].clone()), (9, r[2].clone())]),
        BTreeMap::from([(1, r[0].clone()), (2, r[1].clone()), (3, r[2].clone())]),
        BTreeMap::from([(2, r[1].clone())]),
        BTreeMap::from([(0, r[0].clone()), (2, r[1].clone()), (3, r[2].clone())]),
    ] {
        let (seat1, _) = dkg::round1(p[0], ctx, &mut rng);
        assert_eq!(
            dkg::round2(seat1, p[0], &bad, &mut rng).expect_err("indexes"),
            TreasuryError::Participants
        );
    }
    // shares: a sender outside 1..=n never reaches the library's blame path
    let id2 = run(&id, t, n, 19);
    let (seat1, _) = dkg::round1(p[0], ctx, &mut rng);
    let (km, _) = dkg::round2(seat1, p[0], &others(&id2.round1, 1), &mut rng).expect("r2");
    let mut shares = id2.shares[&1].clone();
    let s3 = shares.remove(&3).expect("3");
    shares.insert(9, s3);
    assert_eq!(
        dkg::complete(km, p[0], &shares, &mut rng).expect_err("indexes"),
        TreasuryError::Participants
    );

    let (seat1, _) = dkg::round1(p[0], ctx, &mut rng);
    let mut long = BTreeMap::from([(2, r[1].clone()), (3, r[2].clone())]);
    long.get_mut(&3).expect("3").push(0);
    assert_eq!(
        dkg::round2(seat1, p[0], &long, &mut rng).expect_err("length"),
        TreasuryError::Frame(3)
    );
}

#[test]
fn the_address_is_a_standard_main_address() {
    let id = run_id(8);
    let r = run(&id, 2, 3, 18);
    let view = keys::view_key(&id, &r.round1, 3).expect("view");
    let group = r.keys[0].group_key();
    for (net, first) in [
        (Network::Mainnet, "4"),
        (Network::Stagenet, "5"),
        (Network::Testnet, "9A"),
    ] {
        let a = keys::standard_address(group, view.clone(), net).expect("address");
        assert_eq!(*a.kind(), AddressType::Legacy);
        let s = a.to_string();
        assert_eq!(s.len(), 95);
        assert!(s.starts_with(|c| first.contains(c)), "{net:?}: {s}");
        let back = MoneroAddress::from_str(net, &s).expect("parses");
        assert_eq!(*back.kind(), AddressType::Legacy);
    }
}

#[test]
fn the_context_transcript_and_view_layouts_are_pinned() {
    let id = RunId {
        republic_id: [0xaa; 32],
        init_id: 0x0102_0304_0506_0708,
        run: [0xbb; 32],
    };
    let mut want = Vec::new();
    put_bytes(&mut want, b"molt-wallet-dkg-v1");
    want.extend_from_slice(&[0xaa; 32]);
    want.extend_from_slice(&[8, 7, 6, 5, 4, 3, 2, 1]);
    want.extend_from_slice(&[0xbb; 32]);
    want.extend_from_slice(&[2, 0, 3, 0]);
    assert_eq!(dkg::context_preimage(&id, 2, 3), want);
    assert_eq!(
        dkg::context(&id, 2, 3),
        <[u8; 32]>::from(Sha256::digest(&want))
    );
    assert_eq!(
        hex32(dkg::context(&id, 2, 3)),
        "89599f758c44f9fc3cd6d2c09ab94694da7b8cab65bde3318786b735a3ac25b6"
    );

    let frames: Frames = BTreeMap::from([(1, vec![0x11; 3]), (2, vec![0x22; 2])]);
    let mut want = Vec::new();
    put_bytes(&mut want, b"molt-wallet-transcript-v1");
    want.extend_from_slice(&[0xbb; 32]);
    put_count(&mut want, 2);
    want.extend_from_slice(&[1, 0]);
    put_bytes(&mut want, &[0x11; 3]);
    want.extend_from_slice(&[2, 0]);
    put_bytes(&mut want, &[0x22; 2]);
    assert_eq!(
        *dkg::transcript_preimage(&id.run, &frames, 2).expect("pre"),
        want
    );

    let frames: Frames = BTreeMap::from([(1, vec![0x11; 40]), (2, vec![0x22; 40])]);
    let mut want = Vec::new();
    put_bytes(&mut want, b"molt-wallet-view-v1");
    want.extend_from_slice(&[0xaa; 32]);
    want.extend_from_slice(&[8, 7, 6, 5, 4, 3, 2, 1]);
    want.extend_from_slice(&[0xbb; 32]);
    put_count(&mut want, 2);
    want.extend_from_slice(&[1, 0]);
    want.extend_from_slice(&[0x11; 32]);
    want.extend_from_slice(&[2, 0]);
    want.extend_from_slice(&[0x22; 32]);
    assert_eq!(*keys::view_preimage(&id, &frames, 2).expect("pre"), want);
}
