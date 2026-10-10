// SPDX-License-Identifier: GPL-3.0-or-later
//! The shared view key, the purse's standard main address and the keys record.

use ciphersuite::group::Group;
use dalek_ff_group::{Ed25519, EdwardsPoint};
use molt_core::{put_bytes, put_count};
use monero_wallet::address::{AddressType, MoneroAddress, Network};
use monero_wallet::ed25519::{Point, Scalar};
use monero_wallet::ViewPair;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::dkg::{expect_all, Frames, CONTRIBUTION_LEN};
use crate::{network_byte, network_from_byte, Reader, RunId, ThresholdKeys, TreasuryError};

const VIEW_TAG: &[u8] = b"molt-wallet-view-v1";
const KEYS_TAG: &[u8] = b"molt-wallet-keys-v1";

/// `tag ‖ republic_id ‖ init_id ‖ r ‖ count ‖ (i ‖ c_i)` over all n contributions.
pub fn view_preimage(
    run: &RunId,
    round1: &Frames,
    n: u16,
) -> Result<Zeroizing<Vec<u8>>, TreasuryError> {
    expect_all(round1, n)?;
    let mut head = Vec::new();
    put_bytes(&mut head, VIEW_TAG);
    run.put(&mut head);
    put_count(&mut head, round1.len());
    let mut out = Zeroizing::new(Vec::with_capacity(
        head.len() + round1.len() * (2 + CONTRIBUTION_LEN),
    ));
    out.extend_from_slice(&head);
    for (i, msg) in round1 {
        let c = msg
            .get(..CONTRIBUTION_LEN)
            .ok_or(TreasuryError::Frame(*i))?;
        out.extend_from_slice(&i.to_le_bytes());
        out.extend_from_slice(c);
    }
    Ok(out)
}

/// The shared private view key `H_s(view preimage)` from the round-1 messages.
pub fn view_key(run: &RunId, round1: &Frames, n: u16) -> Result<Zeroizing<Scalar>, TreasuryError> {
    Ok(Zeroizing::new(Scalar::hash(&*view_preimage(
        run, round1, n,
    )?)))
}

/// A scalar's canonical bytes.
pub fn scalar_bytes(s: &Scalar) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    s.write(&mut &mut out[..])
        .expect("32 bytes fit a 32-byte buffer");
    out
}

/// The standard (legacy) main address of `(group spend key, private view key)`.
pub fn standard_address(
    group_key: EdwardsPoint,
    view: Zeroizing<Scalar>,
    network: Network,
) -> Result<MoneroAddress, TreasuryError> {
    view_pair(group_key, view).map(|p| p.legacy_address(network))
}

/// Does `view` open the standard address `address` on `network`: `view·G`
/// is its view key (design §5, a key a peer handed over)?
pub fn view_matches(address: &str, network: Network, view: &[u8; 32]) -> bool {
    let Ok(addr) = MoneroAddress::from_str(network, address) else {
        return false;
    };
    let Ok(scalar) = Scalar::read(&mut view.as_slice()) else {
        return false;
    };
    matches!(addr.kind(), AddressType::Legacy)
        && ViewPair::new(addr.spend(), Zeroizing::new(scalar)).is_ok_and(|p| p.view() == addr.view())
}

/// The pair the standard scanner runs on.
pub(crate) fn view_pair(
    group_key: EdwardsPoint,
    view: Zeroizing<Scalar>,
) -> Result<ViewPair, TreasuryError> {
    // `ViewPair::new` checks torsion only; an identity spend key is spendable by anyone.
    if bool::from(group_key.is_identity()) {
        return Err(TreasuryError::SpendKey);
    }
    ViewPair::new(Point::from(group_key.0), view).map_err(|_| TreasuryError::SpendKey)
}

/// What a seat persists for a run before it attests (design §3.5).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct KeysRecord {
    /// The run.
    pub run: RunId,
    /// Its transcript hash `T`.
    pub transcript: [u8; 32],
    /// The standard main address.
    pub address: String,
    /// The network the address is for.
    pub network: Network,
    /// The spend threshold (`rule_m`).
    pub m: u16,
    /// The seat count.
    pub n: u16,
    /// The init's birthday height.
    pub birthday: u64,
    /// This seat's attestation.
    pub attestation: [u8; 64],
    /// This seat's serialized `ThresholdKeys`.
    pub share: Zeroizing<Vec<u8>>,
    /// The private view key.
    pub view: Zeroizing<[u8; 32]>,
}

impl KeysRecord {
    /// The tagged layout; fixed fields, then the share and the view key.
    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut head = Vec::new();
        put_bytes(&mut head, KEYS_TAG);
        self.run.put(&mut head);
        head.extend_from_slice(&self.transcript);
        put_bytes(&mut head, self.address.as_bytes());
        head.push(network_byte(self.network));
        head.extend_from_slice(&self.m.to_le_bytes());
        head.extend_from_slice(&self.n.to_le_bytes());
        head.extend_from_slice(&self.birthday.to_le_bytes());
        head.extend_from_slice(&self.attestation);
        let mut out = Zeroizing::new(Vec::with_capacity(
            head.len() + 4 + self.share.len() + self.view.len(),
        ));
        out.extend_from_slice(&head);
        put_bytes(&mut out, &self.share);
        out.extend_from_slice(&*self.view);
        out
    }

    /// The record back, only if whole and self-consistent: the share is a
    /// `t = m`, `n`-seat key and the address is its group key's with this view key.
    pub fn decode(bytes: &[u8]) -> Result<Self, TreasuryError> {
        let rec = Self::parse(bytes).ok_or(TreasuryError::Record)?;
        let keys = rec.threshold_keys()?;
        let p = keys.params();
        if p.t() != rec.m || p.n() != rec.n {
            return Err(TreasuryError::Record);
        }
        let address = standard_address(keys.group_key(), rec.view_key()?, rec.network)
            .map_err(|_| TreasuryError::Record)?;
        if address.to_string() != rec.address {
            return Err(TreasuryError::Record);
        }
        Ok(rec)
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        if r.bytes()? != KEYS_TAG {
            return None;
        }
        let rec = Self {
            run: RunId::take(&mut r)?,
            transcript: r.array()?,
            address: String::from_utf8(r.bytes()?.to_vec()).ok()?,
            network: network_from_byte(r.u8()?)?,
            m: r.u16()?,
            n: r.u16()?,
            birthday: r.u64()?,
            attestation: r.array()?,
            share: Zeroizing::new(r.bytes()?.to_vec()),
            view: Zeroizing::new(r.array()?),
        };
        r.done().then_some(rec)
    }

    /// The share, parsed.
    pub fn threshold_keys(&self) -> Result<ThresholdKeys<Ed25519>, TreasuryError> {
        let mut s = self.share.as_slice();
        let keys = ThresholdKeys::read(&mut s).map_err(|_| TreasuryError::Record)?;
        if s.is_empty() {
            Ok(keys)
        } else {
            Err(TreasuryError::Record)
        }
    }

    /// The view key, parsed.
    pub fn view_key(&self) -> Result<Zeroizing<Scalar>, TreasuryError> {
        Scalar::read(&mut self.view.as_slice())
            .map(Zeroizing::new)
            .map_err(|_| TreasuryError::Record)
    }
}
