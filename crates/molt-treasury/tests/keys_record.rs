// SPDX-License-Identifier: GPL-3.0-or-later
//! The keys record: one per attested run, the share's only home (design §3.5, §6).

mod common;

use common::{run, run_id};
use molt_core::put_bytes;
use molt_treasury::keys::{self, KeysRecord};
use molt_treasury::{Network, TreasuryError};
use zeroize::Zeroize;

fn record() -> KeysRecord {
    let id = run_id(21);
    let r = run(&id, 2, 3, 21);
    let view = keys::view_key(&id, &r.round1, 3).expect("view");
    let address = keys::standard_address(r.keys[1].group_key(), view.clone(), Network::Stagenet)
        .expect("address")
        .to_string();
    KeysRecord {
        run: id,
        transcript: [4; 32],
        address,
        network: Network::Stagenet,
        m: 2,
        n: 3,
        birthday: 1_500_000,
        attestation: [6; 64],
        share: r.keys[1].serialize(),
        view: keys::scalar_bytes(&view),
    }
}

#[test]
fn wallet_keys_round_trip_and_zeroize() {
    let rec = record();
    let bytes = rec.encode();
    assert_eq!(
        bytes.capacity(),
        bytes.len(),
        "grown, an unwiped copy stays behind"
    );
    let back = KeysRecord::decode(&bytes).expect("decode");
    assert_eq!(back.encode(), bytes);
    assert_eq!(back.address, rec.address);
    assert_eq!(back.run, rec.run);
    let keys = back.threshold_keys().expect("keys");
    assert_eq!(u16::from(keys.params().i()), 2);

    let mut z = back;
    z.zeroize();
    assert!(z.share.iter().all(|b| *b == 0));
    assert_eq!(*z.view, [0; 32]);
    assert!(z.address.is_empty());
}

#[test]
fn a_damaged_or_inconsistent_record_is_refused() {
    let bytes = record().encode();
    for cut in [0, 10, bytes.len() - 1] {
        assert_eq!(
            KeysRecord::decode(&bytes[..cut]).err(),
            Some(TreasuryError::Record)
        );
    }
    let mut long = bytes.to_vec();
    long.push(0);
    assert_eq!(KeysRecord::decode(&long).err(), Some(TreasuryError::Record));
    let mut tag = bytes.to_vec();
    tag[8] ^= 1;
    assert_eq!(KeysRecord::decode(&tag).err(), Some(TreasuryError::Record));

    let mut other_view = record();
    other_view.view[0] ^= 1;
    assert_eq!(
        KeysRecord::decode(&other_view.encode()).err(),
        Some(TreasuryError::Record),
        "the address must be the share's and the view key's"
    );
    let mut other_m = record();
    other_m.m = 3;
    assert_eq!(
        KeysRecord::decode(&other_m.encode()).err(),
        Some(TreasuryError::Record)
    );
}

#[test]
fn the_keys_record_layout_is_pinned() {
    let rec = record();
    let mut want = Vec::new();
    put_bytes(&mut want, b"molt-wallet-keys-v1");
    want.extend_from_slice(&[21; 32]);
    want.extend_from_slice(&7u64.to_le_bytes());
    want.extend_from_slice(&[21 ^ 0x55; 32]);
    want.extend_from_slice(&[4; 32]);
    put_bytes(&mut want, rec.address.as_bytes());
    want.push(2);
    want.extend_from_slice(&[2, 0, 3, 0]);
    want.extend_from_slice(&1_500_000u64.to_le_bytes());
    want.extend_from_slice(&[6; 64]);
    put_bytes(&mut want, &rec.share);
    want.extend_from_slice(&*rec.view);
    assert_eq!(*rec.encode(), want);
}

/// Design §5: a view key a peer hands over counts only when `view·G` is
/// the address's view key, on the address's network.
#[test]
fn a_view_key_is_checked_against_the_address() {
    let rec = record();
    assert!(keys::view_matches(&rec.address, Network::Stagenet, &rec.view));
    let mut other = *rec.view;
    other[0] ^= 1;
    assert!(!keys::view_matches(&rec.address, Network::Stagenet, &other), "another key");
    assert!(!keys::view_matches(&rec.address, Network::Mainnet, &rec.view), "another network");
    assert!(!keys::view_matches("4x", Network::Stagenet, &rec.view), "no address");
    assert!(!keys::view_matches(&rec.address, Network::Stagenet, &[0xff; 32]), "not a scalar");
}
