// SPDX-License-Identifier: GPL-3.0-or-later
//! A seat's identity-key attestation of the run's result (design §3.5, W8).

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use molt_core::put_bytes;

use crate::{network_byte, Network, RunId, TreasuryError};

const ATTEST_TAG: &[u8] = b"molt-wallet-attest-v1";

/// The values every seat attests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attested<'a> {
    /// The run.
    pub run: RunId,
    /// Its transcript hash `T`.
    pub transcript: [u8; 32],
    /// The standard main address.
    pub address: &'a str,
    /// The address's network.
    pub network: Network,
    /// The spend threshold (`rule_m`).
    pub m: u16,
    /// The seat count.
    pub n: u16,
    /// The init's birthday height.
    pub birthday: u64,
}

/// `tag ‖ republic_id ‖ init_id ‖ r ‖ T ‖ address ‖ network ‖ m ‖ n ‖ birthday`.
pub fn attestation_bytes(a: &Attested<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, ATTEST_TAG);
    a.run.put(&mut out);
    out.extend_from_slice(&a.transcript);
    put_bytes(&mut out, a.address.as_bytes());
    out.push(network_byte(a.network));
    out.extend_from_slice(&a.m.to_le_bytes());
    out.extend_from_slice(&a.n.to_le_bytes());
    out.extend_from_slice(&a.birthday.to_le_bytes());
    out
}

/// This seat's attestation under its roster identity key.
pub fn sign(identity: &SigningKey, a: &Attested<'_>) -> [u8; 64] {
    identity.sign(&attestation_bytes(a)).to_bytes()
}

/// All n attestations, each under the identity key (hex) of its founding
/// position, both lists in participant order.
pub fn verify_all(
    a: &Attested<'_>,
    identity_pks: &[impl AsRef<str>],
    sigs: &[[u8; 64]],
) -> Result<(), TreasuryError> {
    let n = usize::from(a.n);
    if identity_pks.len() != n || sigs.len() != n {
        return Err(TreasuryError::Participants);
    }
    let bytes = attestation_bytes(a);
    for (k, (pk, sig)) in identity_pks.iter().zip(sigs).enumerate() {
        let i = u16::try_from(k + 1).map_err(|_| TreasuryError::Participants)?;
        let ok = hex::decode(pk.as_ref())
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .and_then(|b| VerifyingKey::from_bytes(&b).ok())
            .is_some_and(|vk| {
                vk.verify_strict(&bytes, &Signature::from_bytes(sig))
                    .is_ok()
            });
        if !ok {
            return Err(TreasuryError::Attestation(i));
        }
    }
    Ok(())
}
