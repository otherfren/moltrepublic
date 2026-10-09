// SPDX-License-Identifier: GPL-3.0-or-later
//! One seat's side of a PedPoP run, over the wire bytes the frames carry.

use std::collections::{BTreeMap, HashMap};

use dalek_ff_group::Ed25519;
use dkg_pedpop::{
    Commitments, EncryptedMessage, EncryptionKeyMessage, KeyGenMachine, KeyMachine, Participant,
    SecretShare, SecretShareMachine, ThresholdKeys, ThresholdParams,
};
use rand_chacha::rand_core::{CryptoRng, RngCore};

use crate::TreasuryError;

type Scalar = <Ed25519 as ciphersuite::Ciphersuite>::F;

/// Wire frames keyed by 1-based participant index.
pub type Frames = BTreeMap<u16, Vec<u8>>;

/// Seat `i` (1-based founding position) of a `t`-of-`n` run.
pub fn params(t: u16, n: u16, i: u16) -> Result<ThresholdParams, TreasuryError> {
    let i = Participant::new(i).ok_or_else(|| TreasuryError::Params("participant 0".into()))?;
    ThresholdParams::new(t, n, i).map_err(|e| TreasuryError::Params(e.to_string()))
}

/// Round 1: this seat's commitments message.
pub fn round1(
    params: ThresholdParams,
    context: [u8; 32],
    rng: &mut (impl RngCore + CryptoRng),
) -> (SecretShareMachine<Ed25519>, Vec<u8>) {
    let (machine, msg) = KeyGenMachine::<Ed25519>::new(params, context).generate_coefficients(rng);
    (machine, msg.serialize())
}

/// Round 2: every other seat's round-1 bytes in, one share frame per recipient out.
pub fn round2(
    machine: SecretShareMachine<Ed25519>,
    params: ThresholdParams,
    round1: &Frames,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<(KeyMachine<Ed25519>, Frames), TreasuryError> {
    let mut msgs = HashMap::new();
    for (&l, bytes) in round1 {
        let msg = EncryptionKeyMessage::<Ed25519, Commitments<Ed25519>>::read(
            &mut bytes.as_slice(),
            params,
        )
        .map_err(|_| TreasuryError::Frame(l))?;
        msgs.insert(participant(l)?, msg);
    }
    let (machine, shares) = machine
        .generate_secret_shares(rng, msgs)
        .map_err(|e| TreasuryError::Dkg(format!("{e:?}")))?;
    let out = shares
        .into_iter()
        .map(|(l, m)| (u16::from(l), m.serialize()))
        .collect();
    Ok((machine, out))
}

/// Completion: the shares addressed to this seat in, its `ThresholdKeys` out.
/// Not yet confirmed with the others: the transcript check comes before use.
pub fn complete(
    machine: KeyMachine<Ed25519>,
    params: ThresholdParams,
    shares: &Frames,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<ThresholdKeys<Ed25519>, TreasuryError> {
    let mut msgs = HashMap::new();
    for (&l, bytes) in shares {
        let msg =
            EncryptedMessage::<Ed25519, SecretShare<Scalar>>::read(&mut bytes.as_slice(), params)
                .map_err(|_| TreasuryError::Frame(l))?;
        msgs.insert(participant(l)?, msg);
    }
    let blame = machine
        .calculate_share(rng, msgs)
        .map_err(|e| TreasuryError::Dkg(format!("{e:?}")))?;
    Ok(blame.complete())
}

fn participant(l: u16) -> Result<Participant, TreasuryError> {
    Participant::new(l).ok_or(TreasuryError::Frame(l))
}
