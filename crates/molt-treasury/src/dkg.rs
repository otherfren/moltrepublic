// SPDX-License-Identifier: GPL-3.0-or-later
//! One seat's side of a PedPoP run, over the wire bytes the frames carry.
//!
//! A round-1 message is `c ‖ pedpop commitments message`, `c` the seat's
//! 32-byte view contribution. Every wire index and every point is checked
//! here before the library sees it: its blame path panics on an index
//! outside 1..=n, and its reader accepts the identity (design §12).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ciphersuite::group::Group;
use ciphersuite::Ciphersuite;
use dalek_ff_group::Ed25519;
use dkg_pedpop::{
    Commitments, EncryptedMessage, EncryptionKeyMessage, KeyGenMachine, KeyMachine, Participant,
    SecretShare, SecretShareMachine, ThresholdKeys, ThresholdParams,
};
use molt_core::{put_bytes, put_count};
use rand_chacha::rand_core::{CryptoRng, RngCore};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::{RunId, TreasuryError};

type Scalar = <Ed25519 as Ciphersuite>::F;

/// Wire frames keyed by 1-based participant index.
pub type Frames = BTreeMap<u16, Vec<u8>>;

const CONTEXT_TAG: &[u8] = b"molt-wallet-dkg-v1";
const TRANSCRIPT_TAG: &[u8] = b"molt-wallet-transcript-v1";
/// The view contribution heading a round-1 message.
pub const CONTRIBUTION_LEN: usize = 32;
/// Key, PoP nonce, PoP scalar, encrypted share.
const SHARE_LEN: usize = 128;

/// Seat `i` (1-based founding position) of a `t`-of-`n` run.
pub fn params(t: u16, n: u16, i: u16) -> Result<ThresholdParams, TreasuryError> {
    let i = Participant::new(i).ok_or_else(|| TreasuryError::Params("participant 0".into()))?;
    ThresholdParams::new(t, n, i).map_err(|e| TreasuryError::Params(e.to_string()))
}

/// `tag ‖ republic_id ‖ init_id ‖ r ‖ t ‖ n`.
pub fn context_preimage(run: &RunId, t: u16, n: u16) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, CONTEXT_TAG);
    run.put(&mut out);
    out.extend_from_slice(&t.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out
}

/// The PedPoP context of one run: unique per republic, init and nonce.
pub fn context(run: &RunId, t: u16, n: u16) -> [u8; 32] {
    Sha256::digest(context_preimage(run, t, n)).into()
}

/// Round 1: this seat's message (fresh view contribution + commitments).
pub fn round1(
    params: ThresholdParams,
    context: [u8; 32],
    rng: &mut (impl RngCore + CryptoRng),
) -> (SecretShareMachine<Ed25519>, Zeroizing<Vec<u8>>) {
    let (machine, commitments) =
        KeyGenMachine::<Ed25519>::new(params, context).generate_coefficients(rng);
    let commitments = commitments.serialize();
    let mut msg = Zeroizing::new(Vec::with_capacity(CONTRIBUTION_LEN + commitments.len()));
    msg.resize(CONTRIBUTION_LEN, 0);
    rng.fill_bytes(&mut msg);
    msg.extend_from_slice(&commitments);
    (machine, msg)
}

/// Round 2: every other seat's round-1 message in, one share frame per recipient out.
pub fn round2(
    machine: SecretShareMachine<Ed25519>,
    params: ThresholdParams,
    round1: &Frames,
    rng: &mut (impl RngCore + CryptoRng),
) -> Result<(KeyMachine<Ed25519>, Frames), TreasuryError> {
    expect_others(params, round1)?;
    let t = usize::from(params.t());
    let mut msgs = HashMap::new();
    for (&l, bytes) in round1 {
        if bytes.len() != CONTRIBUTION_LEN + 32 * t + 96 {
            return Err(TreasuryError::Frame(l));
        }
        let body = &bytes[CONTRIBUTION_LEN..];
        // t commitments, PoK nonce, (PoK scalar), encryption key
        let points = (0..t).map(|k| 32 * k).chain([32 * t, 32 * t + 64]);
        no_identity(l, body, points)?;
        let msg =
            EncryptionKeyMessage::<Ed25519, Commitments<Ed25519>>::read(&mut &body[..], params)
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
    expect_others(params, shares)?;
    let mut msgs = HashMap::new();
    for (&l, bytes) in shares {
        if bytes.len() != SHARE_LEN {
            return Err(TreasuryError::Frame(l));
        }
        // per-message key, PoP nonce
        no_identity(l, bytes, [0, 32])?;
        let msg =
            EncryptedMessage::<Ed25519, SecretShare<Scalar>>::read(&mut bytes.as_slice(), params)
                .map_err(|_| TreasuryError::Frame(l))?;
        msgs.insert(participant(l)?, msg);
    }
    // the blame machine is dropped: its `blame` indexes by participant and panics
    let keys = machine
        .calculate_share(rng, msgs)
        .map_err(|e| TreasuryError::Dkg(format!("{e:?}")))?
        .complete();
    if bool::from(keys.group_key().is_identity()) {
        return Err(TreasuryError::SpendKey);
    }
    Ok(keys)
}

/// `tag ‖ r ‖ count ‖ (i ‖ message)` over all n round-1 messages.
pub fn transcript_preimage(
    run: &[u8; 32],
    round1: &Frames,
    n: u16,
) -> Result<Vec<u8>, TreasuryError> {
    expect_all(round1, n)?;
    let mut out = Vec::new();
    put_bytes(&mut out, TRANSCRIPT_TAG);
    out.extend_from_slice(run);
    put_count(&mut out, round1.len());
    for (i, msg) in round1 {
        out.extend_from_slice(&i.to_le_bytes());
        put_bytes(&mut out, msg);
    }
    Ok(out)
}

/// The transcript hash `T` every seat sends in round 2 and attests.
pub fn transcript(run: &[u8; 32], round1: &Frames, n: u16) -> Result<[u8; 32], TreasuryError> {
    Ok(Sha256::digest(transcript_preimage(run, round1, n)?).into())
}

/// Participant `from`'s transcript hash against ours.
pub fn check_transcript(
    mine: &[u8; 32],
    from: u16,
    theirs: &[u8; 32],
) -> Result<(), TreasuryError> {
    if mine == theirs {
        Ok(())
    } else {
        Err(TreasuryError::Transcript(from))
    }
}

/// The frames of one round received so far, one per sender; wiped on drop
/// (round 1 carries every view contribution).
#[derive(Clone)]
pub struct Inbox {
    from: BTreeSet<u16>,
    frames: Frames,
}

impl Inbox {
    /// Round 1 of an `n`-seat run: every seat, this one included.
    pub fn new(n: u16) -> Self {
        Self {
            from: (1..=n).collect(),
            frames: Frames::new(),
        }
    }

    /// Round 2 at seat `me`: every other seat.
    pub fn others(n: u16, me: u16) -> Self {
        Self {
            from: (1..=n).filter(|l| *l != me).collect(),
            frames: Frames::new(),
        }
    }

    /// `Ok(true)` for a first frame, `Ok(false)` for an identical resend;
    /// a different second frame from one sender is equivocation.
    pub fn record(&mut self, from: u16, msg: Vec<u8>) -> Result<bool, TreasuryError> {
        let mut msg = Zeroizing::new(msg);
        if !self.from.contains(&from) {
            return Err(TreasuryError::Frame(from));
        }
        match self.frames.get(&from) {
            None => {
                self.frames.insert(from, core::mem::take(&mut *msg));
                Ok(true)
            }
            Some(seen) if *seen == *msg => Ok(false),
            Some(_) => Err(TreasuryError::Equivocation(from)),
        }
    }

    /// Every expected sender's frame is in.
    pub fn is_complete(&self) -> bool {
        self.frames.len() == self.from.len()
    }

    /// The frames by sender.
    pub fn frames(&self) -> &Frames {
        &self.frames
    }
}

impl core::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Inbox")
            .field("from", &self.from)
            .field("received", &self.frames.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.frames.values_mut().for_each(Zeroize::zeroize);
    }
}

/// The keys are exactly 1..=n.
pub(crate) fn expect_all(frames: &Frames, n: u16) -> Result<(), TreasuryError> {
    if frames.keys().copied().eq(1..=n) {
        Ok(())
    } else {
        Err(TreasuryError::Participants)
    }
}

/// The keys are exactly 1..=n without this seat.
fn expect_others(params: ThresholdParams, frames: &Frames) -> Result<(), TreasuryError> {
    let me = u16::from(params.i());
    let want: BTreeSet<u16> = (1..=params.n()).filter(|l| *l != me).collect();
    if frames.keys().copied().eq(want) {
        Ok(())
    } else {
        Err(TreasuryError::Participants)
    }
}

fn no_identity(
    from: u16,
    bytes: &[u8],
    offsets: impl IntoIterator<Item = usize>,
) -> Result<(), TreasuryError> {
    for at in offsets {
        let mut point = bytes.get(at..at + 32).ok_or(TreasuryError::Frame(from))?;
        let p = Ed25519::read_G(&mut point).map_err(|_| TreasuryError::Frame(from))?;
        if bool::from(p.is_identity()) {
            return Err(TreasuryError::Identity(from));
        }
    }
    Ok(())
}

fn participant(l: u16) -> Result<Participant, TreasuryError> {
    Participant::new(l).ok_or(TreasuryError::Frame(l))
}
