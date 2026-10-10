// SPDX-License-Identifier: GPL-3.0-or-later
//! The scan core (design §7, §9): the standard scanner behind the
//! treasury's own trait, the seen output keys, reorg detection over the
//! recent block hashes, and the confirmation split.

use std::collections::{BTreeMap, BTreeSet};

use dalek_ff_group::EdwardsPoint;
use monero_wallet::ed25519::Scalar;
use monero_wallet::address::{MoneroAddress, Network};
use monero_wallet::interface::ScannableBlock;
use monero_wallet::transaction::Timelock;
use monero_wallet::{ScanError, Scanner};
use zeroize::Zeroizing;

use molt_core::{put_bytes, put_count};

use crate::keys::view_pair;
use crate::{Reader, TreasuryError};

const SCAN_TAG: &[u8] = b"molt-wallet-scan-v3";

/// Confirmations before an output counts as balance.
pub const CONFIRMATIONS: u64 = 20;
/// Block hashes kept for reorg detection.
pub const RECENT_BLOCKS: usize = 100;

/// One output received by the purse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    /// The one-time output key (the burning-bug identity).
    pub key: [u8; 32],
    /// Piconero.
    pub amount: u64,
    /// The block it is in.
    pub height: u64,
    /// That block's time, unix seconds.
    pub at: u64,
    /// Its transaction hash.
    pub tx: [u8; 32],
    /// Its position in that transaction.
    pub index_in_tx: u64,
    /// Its additional timelock (a mined output's 60 blocks, or a sender's).
    pub lock: Lock,
}

/// An output's additional timelock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    /// None beyond the network's default.
    None,
    /// Until the chain reaches this height.
    Block(u64),
    /// Until this Unix time on the chain's clock.
    Time(u64),
}

impl Lock {
    /// Spendable with `chain_height` blocks in the chain whose newest
    /// known block is from `chain_time`.
    pub fn open_at(self, chain_height: u64, chain_time: u64) -> bool {
        match self {
            Self::None => true,
            Self::Block(h) => h <= chain_height,
            Self::Time(t) => t <= chain_time,
        }
    }
}

impl From<Timelock> for Lock {
    fn from(t: Timelock) -> Self {
        match t {
            Timelock::None => Self::None,
            Timelock::Block(h) => Self::Block(u64::try_from(h).unwrap_or(u64::MAX)),
            Timelock::Time(s) => Self::Time(s),
        }
    }
}

/// Why a block yielded nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScanFault {
    /// A hard fork this scanner does not know: pause, funds are safe.
    #[error("update needed")]
    UpdateNeeded(u8),
    /// The daemon served something that does not scan or decode.
    #[error("daemon fault: {0}")]
    Daemon(String),
}

impl ScanFault {
    /// A block or transaction the daemon sent did not decode.
    pub fn decode(e: &dyn core::fmt::Display) -> Self {
        Self::Daemon(e.to_string())
    }

    /// The paused line for the purse view, if scanning is paused.
    pub fn paused(&self) -> Option<&'static str> {
        match self {
            Self::UpdateNeeded(_) => Some("update needed"),
            Self::Daemon(_) => None,
        }
    }
}

impl From<ScanError> for ScanFault {
    fn from(e: ScanError) -> Self {
        match e {
            ScanError::UnsupportedProtocol(v) => Self::UpdateNeeded(v),
            ScanError::InvalidScannableBlock(why) => Self::Daemon(why.to_string()),
        }
    }
}

/// Finds the purse's outputs in one block; the Carrot upgrade swaps this.
pub trait BlockScanner {
    /// Every output of `block` paying the purse.
    fn scan_block(&mut self, block: ScannableBlock) -> Result<Vec<Received>, ScanFault>;
}

/// monero-wallet's standard `Scanner` (not the guaranteed one, design §7).
pub struct StandardScanner(Scanner);

impl StandardScanner {
    /// The scanner of `(group spend key, private view key)`; main address only.
    pub fn new(spend: EdwardsPoint, view: Zeroizing<Scalar>) -> Result<Self, TreasuryError> {
        Ok(Self(Scanner::new(view_pair(spend, view)?)))
    }

    /// The scanner of a standard address whose view key `view` opens it.
    pub fn for_address(address: &str, network: Network, view: &[u8; 32]) -> Result<Self, TreasuryError> {
        if !crate::keys::view_matches(address, network, view) {
            return Err(TreasuryError::SpendKey);
        }
        let addr = MoneroAddress::from_str(network, address).map_err(|_| TreasuryError::SpendKey)?;
        let scalar = Scalar::read(&mut view.as_slice()).map_err(|_| TreasuryError::SpendKey)?;
        Self::new(EdwardsPoint(addr.spend().into()), Zeroizing::new(scalar))
    }
}

impl BlockScanner for StandardScanner {
    fn scan_block(&mut self, block: ScannableBlock) -> Result<Vec<Received>, ScanFault> {
        let height = u64::try_from(block.block.number())
            .map_err(|_| ScanFault::Daemon("block number".into()))?;
        let at = block.block.header.timestamp;
        Ok(self
            .0
            .scan(block)?
            .ignore_additional_timelock()
            .iter()
            .map(|o| Received {
                key: o.key().compress().to_bytes(),
                amount: o.commitment().amount,
                height,
                at,
                tx: o.transaction(),
                index_in_tx: o.index_in_transaction(),
                lock: o.additional_timelock().into(),
            })
            .collect())
    }
}

/// What [`ScanState::apply`] did with a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Recorded; the outputs not seen before.
    Applied(Vec<Received>),
    /// Its parent is not the block we hold: rewound, rescan from
    /// [`ScanState::next_height`].
    Reorg,
    /// Not the block at [`ScanState::next_height`]; nothing changed.
    OutOfOrder,
}

/// The scanner's progress from the birthday on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanState {
    /// The purse's address: progress never carries over to another purse.
    address: String,
    birthday: u64,
    next: u64,
    /// Height to (hash, block time).
    recent: BTreeMap<u64, ([u8; 32], u64)>,
    seen: BTreeSet<[u8; 32]>,
    outputs: Vec<Received>,
}

impl ScanState {
    /// Nothing scanned yet for the purse at `address`.
    pub fn new(address: &str, birthday: u64) -> Self {
        Self {
            address: address.to_string(),
            birthday,
            next: birthday,
            recent: BTreeMap::new(),
            seen: BTreeSet::new(),
            outputs: Vec::new(),
        }
    }

    /// The block to scan next.
    pub fn next_height(&self) -> u64 {
        self.next
    }

    /// The purse this progress belongs to.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The purse's birthday height.
    pub fn birthday(&self) -> u64 {
        self.birthday
    }

    /// The outputs held, first seen first.
    pub fn outputs(&self) -> &[Received] {
        &self.outputs
    }

    /// Block `height` with its hash, its parent's hash, its time and what
    /// the scanner found. A repeated output key is dropped (burning bug:
    /// only the first counts).
    pub fn apply(
        &mut self,
        height: u64,
        hash: [u8; 32],
        previous: [u8; 32],
        at: u64,
        found: Vec<Received>,
    ) -> Step {
        if height != self.next {
            return Step::OutOfOrder;
        }
        let parent = height.checked_sub(1);
        if let Some((held, _)) = parent.and_then(|p| self.recent.get(&p)) {
            if *held != previous {
                self.rewind();
                return Step::Reorg;
            }
        }
        self.recent.insert(height, (hash, at));
        while self.recent.len() > RECENT_BLOCKS {
            self.recent.pop_first();
        }
        let new: Vec<Received> = found
            .into_iter()
            .filter(|r| self.seen.insert(r.key))
            .collect();
        self.outputs.extend(new.iter().cloned());
        self.next = height + 1;
        Step::Applied(new)
    }

    /// `(balance, pending)` in piconero with `chain_height` blocks in the
    /// chain (the daemon's `get_height`); pending until confirmed and
    /// unlocked, a time lock against the newest scanned block's time.
    pub fn balance(&self, chain_height: u64) -> (u64, u64) {
        let chain_time = self.recent.last_key_value().map_or(0, |(_, (_, at))| *at);
        self.outputs.iter().fold((0, 0), |(ok, wait), r| {
            if chain_height.saturating_sub(r.height) >= CONFIRMATIONS
                && r.lock.open_at(chain_height, chain_time)
            {
                (ok.saturating_add(r.amount), wait)
            } else {
                (ok, wait.saturating_add(r.amount))
            }
        })
    }

    /// The scan file's bytes: `tag ‖ address ‖ birthday ‖ next ‖ count ‖
    /// (height ‖ hash ‖ time)* ‖ count ‖ (key ‖ amount ‖ height ‖ at ‖ tx ‖ index ‖ lock kind ‖
    /// lock value)*`; the seen keys are the outputs' keys.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_bytes(&mut out, SCAN_TAG);
        put_bytes(&mut out, self.address.as_bytes());
        out.extend_from_slice(&self.birthday.to_le_bytes());
        out.extend_from_slice(&self.next.to_le_bytes());
        put_count(&mut out, self.recent.len());
        for (h, (hash, at)) in &self.recent {
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(hash);
            out.extend_from_slice(&at.to_le_bytes());
        }
        put_count(&mut out, self.outputs.len());
        for r in &self.outputs {
            out.extend_from_slice(&r.key);
            out.extend_from_slice(&r.amount.to_le_bytes());
            out.extend_from_slice(&r.height.to_le_bytes());
            out.extend_from_slice(&r.at.to_le_bytes());
            out.extend_from_slice(&r.tx);
            out.extend_from_slice(&r.index_in_tx.to_le_bytes());
            let (kind, value) = match r.lock {
                Lock::None => (0u8, 0),
                Lock::Block(h) => (1, h),
                Lock::Time(t) => (2, t),
            };
            out.push(kind);
            out.extend_from_slice(&value.to_le_bytes());
        }
        out
    }

    /// The state back, only if whole and consistent with [`Self::apply`]'s
    /// invariants; anything else costs a rescan from the birthday.
    pub fn decode(bytes: &[u8]) -> Result<Self, TreasuryError> {
        Self::parse(bytes).ok_or(TreasuryError::ScanState)
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader(bytes);
        if r.bytes()? != SCAN_TAG {
            return None;
        }
        let address = core::str::from_utf8(r.bytes()?).ok()?;
        let birthday = r.u64()?;
        let next = r.u64()?;
        if next < birthday {
            return None;
        }
        let mut s = Self::new(address, birthday);
        s.next = next;
        let blocks = usize::try_from(u32::from_le_bytes(r.array()?)).ok()?;
        if blocks > RECENT_BLOCKS {
            return None;
        }
        for _ in 0..blocks {
            let h = r.u64()?;
            let hash = r.array()?;
            let at = r.u64()?;
            let ascending = !matches!(s.recent.last_key_value(), Some((top, _)) if *top >= h);
            if !ascending || h < birthday || h >= next {
                return None;
            }
            s.recent.insert(h, (hash, at));
        }
        let outputs = u32::from_le_bytes(r.array()?);
        for _ in 0..outputs {
            let key = r.array()?;
            let amount = r.u64()?;
            let height = r.u64()?;
            let at = r.u64()?;
            let tx = r.array()?;
            let index_in_tx = r.u64()?;
            let lock = match (r.u8()?, r.u64()?) {
                (0, 0) => Lock::None,
                (1, h) => Lock::Block(h),
                (2, t) => Lock::Time(t),
                _ => return None,
            };
            if height < birthday || height >= next || !s.seen.insert(key) {
                return None;
            }
            s.outputs.push(Received {
                key,
                amount,
                height,
                at,
                tx,
                index_in_tx,
                lock,
            });
        }
        r.done().then_some(s)
    }

    /// Drop the newest block; past the remembered window, start over.
    fn rewind(&mut self) {
        let Some((top, _)) = self.recent.pop_last() else {
            *self = Self::new(&self.address, self.birthday);
            return;
        };
        if self.recent.is_empty() {
            *self = Self::new(&self.address, self.birthday);
            return;
        }
        let seen = &mut self.seen;
        self.outputs.retain(|r| {
            let keep = r.height < top;
            if !keep {
                seen.remove(&r.key);
            }
            keep
        });
        self.next = top;
    }
}
