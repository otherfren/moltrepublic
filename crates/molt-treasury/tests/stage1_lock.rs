// SPDX-License-Identifier: GPL-3.0-or-later
//! The Stage 1 stack locked against the compiler (plan §6).

use std::collections::BTreeMap;

use molt_treasury::rand_core::SeedableRng;
use molt_treasury::{dkg, keys, Ed25519, MoneroScalar, Network, ThresholdKeys, TreasuryError};
use monero_wallet::address::{AddressType, MoneroAddress};
use monero_wallet::ed25519::Point;
use rand_chacha::ChaCha20Rng;
use zeroize::Zeroizing;

const T: u16 = 2;
const N: u16 = 3;

fn run_dkg(context: [u8; 32]) -> Vec<ThresholdKeys<Ed25519>> {
    let mut rng = ChaCha20Rng::from_seed([7; 32]);
    let params: Vec<_> = (1..=N)
        .map(|i| dkg::params(T, N, i).expect("params"))
        .collect();

    let mut r1_machines = Vec::new();
    let mut r1_bytes = BTreeMap::new();
    for p in &params {
        let (m, bytes) = dkg::round1(*p, context, &mut rng);
        r1_machines.push(m);
        r1_bytes.insert(u16::from(p.i()), bytes);
    }

    let mut r2_machines = Vec::new();
    let mut inbox: BTreeMap<u16, BTreeMap<u16, Vec<u8>>> = BTreeMap::new();
    for (m, p) in r1_machines.into_iter().zip(&params) {
        let me = u16::from(p.i());
        let others = r1_bytes
            .iter()
            .filter(|(l, _)| **l != me)
            .map(|(l, b)| (*l, b.clone()))
            .collect();
        let (km, shares) = dkg::round2(m, *p, &others, &mut rng).expect("round 2");
        for (to, bytes) in shares {
            inbox.entry(to).or_default().insert(me, bytes);
        }
        r2_machines.push(km);
    }

    r2_machines
        .into_iter()
        .zip(&params)
        .map(|(km, p)| {
            let mine = &inbox[&u16::from(p.i())];
            dkg::complete(km, *p, mine, &mut rng).expect("complete")
        })
        .collect()
}

#[test]
fn a_three_seat_pedpop_dkg_agrees_and_yields_a_standard_main_address() {
    let keys = run_dkg([1; 32]);
    let group = keys[0].group_key();
    assert!(
        keys.iter().all(|k| k.group_key() == group),
        "every seat derives the same group key"
    );

    let bytes = keys[1].serialize();
    let back = ThresholdKeys::<Ed25519>::read(&mut bytes.as_slice()).expect("read");
    assert_eq!(back.group_key(), group);
    assert_eq!(back.params(), keys[1].params());

    let view = Zeroizing::new(MoneroScalar::hash(b"stage1 lock view"));
    let addr = keys::standard_address(group, view, Network::Mainnet).expect("address");
    assert_eq!(*addr.kind(), AddressType::Legacy);
    assert!(
        addr.spend() == Point::from(group.0),
        "spend key is the group key"
    );
    let text = addr.to_string();
    assert_eq!(text.len(), 95);
    assert!(text.starts_with('4'));
    let parsed = MoneroAddress::from_str(Network::Mainnet, &text).expect("parse");
    assert!(parsed.spend() == addr.spend());
}

#[test]
fn a_round_one_frame_under_another_context_is_refused() {
    let mut rng = ChaCha20Rng::from_seed([9; 32]);
    let p: Vec<_> = (1..=N)
        .map(|i| dkg::params(T, N, i).expect("params"))
        .collect();
    let (_, foreign) = dkg::round1(p[0], [1; 32], &mut rng);
    let (seat2, _) = dkg::round1(p[1], [2; 32], &mut rng);
    let (_, third) = dkg::round1(p[2], [2; 32], &mut rng);
    let frames = BTreeMap::from([(1, foreign), (3, third)]);
    let err = dkg::round2(seat2, p[1], &frames, &mut rng).expect_err("context mismatch");
    assert!(
        matches!(err, TreasuryError::Dkg(ref e) if e.contains("InvalidCommitments")),
        "{err:?}"
    );
}

#[derive(Clone)]
struct Offline;

impl monero_daemon_rpc::HttpTransport for Offline {
    #[allow(clippy::manual_async_fn)]
    fn post(
        &self,
        _route: &str,
        _body: Vec<u8>,
        _response_size_limit: Option<usize>,
    ) -> impl Send
           + std::future::Future<Output = Result<Vec<u8>, monero_wallet::interface::InterfaceError>>
    {
        async {
            Err(monero_wallet::interface::InterfaceError::InterfaceError(
                "offline".into(),
            ))
        }
    }
}

#[test]
fn the_daemon_rpc_transport_trait_is_the_locked_shape() {
    drop(monero_daemon_rpc::MoneroDaemon::new(Offline));
}

#[test]
fn an_identity_group_key_has_no_address() {
    use ciphersuite::group::Group;
    let view = Zeroizing::new(MoneroScalar::hash(b"stage1 lock view"));
    let err = keys::standard_address(
        molt_treasury::EdwardsPoint::identity(),
        view,
        Network::Mainnet,
    )
    .expect_err("identity spend key");
    assert_eq!(err, TreasuryError::SpendKey);
}

#[test]
fn a_garbage_frame_names_its_sender_and_seat_zero_is_refused() {
    let mut rng = ChaCha20Rng::from_seed([3; 32]);
    assert!(matches!(
        dkg::params(T, N, 0),
        Err(TreasuryError::Params(_))
    ));
    let p: Vec<_> = (1..=N)
        .map(|i| dkg::params(T, N, i).expect("params"))
        .collect();

    let (seat1, _) = dkg::round1(p[0], [1; 32], &mut rng);
    let (_, r1_2) = dkg::round1(p[1], [1; 32], &mut rng);
    let frames = BTreeMap::from([(2, r1_2.clone()), (3, vec![0xff; 7])]);
    let err = dkg::round2(seat1, p[0], &frames, &mut rng).expect_err("garbage round 1");
    assert_eq!(err, TreasuryError::Frame(3));

    let (seat1, _) = dkg::round1(p[0], [1; 32], &mut rng);
    let (_, r1_3) = dkg::round1(p[2], [1; 32], &mut rng);
    let frames = BTreeMap::from([(2, r1_2), (3, r1_3)]);
    let (km, _) = dkg::round2(seat1, p[0], &frames, &mut rng).expect("round 2");
    let shares = BTreeMap::from([(2, vec![0xff; 5]), (3, vec![0xff; 5])]);
    let err = dkg::complete(km, p[0], &shares, &mut rng).expect_err("garbage share");
    assert_eq!(err, TreasuryError::Frame(2));
}
