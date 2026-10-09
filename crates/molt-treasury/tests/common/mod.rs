// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(dead_code, missing_docs)]

//! A whole PedPoP run among `n` local seats, frames passed by hand.

use std::collections::BTreeMap;

use molt_treasury::dkg::{self, Frames};
use molt_treasury::rand_core::SeedableRng;
use molt_treasury::{Ed25519, RunId, ThresholdKeys};
use rand_chacha::ChaCha20Rng;

pub fn run_id(tag: u8) -> RunId {
    RunId {
        republic_id: [tag; 32],
        init_id: 7,
        run: [tag ^ 0x55; 32],
    }
}

pub struct Run {
    pub keys: Vec<ThresholdKeys<Ed25519>>,
    /// Every seat's round-1 message, as all seats saw it.
    pub round1: Frames,
    /// `shares[to][from]`.
    pub shares: BTreeMap<u16, Frames>,
}

pub fn others(frames: &Frames, me: u16) -> Frames {
    frames
        .iter()
        .filter(|(l, _)| **l != me)
        .map(|(l, b)| (*l, b.clone()))
        .collect()
}

pub fn run(id: &RunId, t: u16, n: u16, seed: u8) -> Run {
    let mut rng = ChaCha20Rng::from_seed([seed; 32]);
    let ctx = dkg::context(id, t, n);
    let params: Vec<_> = (1..=n)
        .map(|i| dkg::params(t, n, i).expect("params"))
        .collect();
    let mut machines = Vec::new();
    let mut round1 = Frames::new();
    for p in &params {
        let (m, msg) = dkg::round1(*p, ctx, &mut rng);
        machines.push(m);
        round1.insert(u16::from(p.i()), msg.to_vec());
    }
    let mut key_machines = Vec::new();
    let mut shares: BTreeMap<u16, Frames> = BTreeMap::new();
    for (m, p) in machines.into_iter().zip(&params) {
        let me = u16::from(p.i());
        let (km, out) = dkg::round2(m, *p, &others(&round1, me), &mut rng).expect("round 2");
        for (to, bytes) in out {
            shares.entry(to).or_default().insert(me, bytes);
        }
        key_machines.push(km);
    }
    let keys = key_machines
        .into_iter()
        .zip(&params)
        .map(|(km, p)| {
            dkg::complete(km, *p, &shares[&u16::from(p.i())], &mut rng).expect("complete")
        })
        .collect();
    Run {
        keys,
        round1,
        shares,
    }
}
