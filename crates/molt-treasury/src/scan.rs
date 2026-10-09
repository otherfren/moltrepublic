// SPDX-License-Identifier: GPL-3.0-or-later
//! The scan core (design §7, §9): the standard scanner behind the
//! treasury's own trait, the seen output keys, reorg detection over the
//! recent block hashes, and the confirmation split.

use std::collections::{BTreeMap, BTreeSet};

use dalek_ff_group::EdwardsPoint;
use monero_wallet::ed25519::Scalar;
use monero_wallet::interface::ScannableBlock;
use monero_wallet::{ScanError, Scanner};
use zeroize::Zeroizing;

use crate::keys::view_pair;
use crate::TreasuryError;

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
    /// Its transaction hash.
    pub tx: [u8; 32],
    /// Its position in that transaction.
    pub index_in_tx: u64,
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
}

impl BlockScanner for StandardScanner {
    fn scan_block(&mut self, block: ScannableBlock) -> Result<Vec<Received>, ScanFault> {
        let height = u64::try_from(block.block.number())
            .map_err(|_| ScanFault::Daemon("block number".into()))?;
        Ok(self
            .0
            .scan(block)?
            .ignore_additional_timelock()
            .iter()
            .map(|o| Received {
                key: o.key().compress().to_bytes(),
                amount: o.commitment().amount,
                height,
                tx: o.transaction(),
                index_in_tx: o.index_in_transaction(),
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
    birthday: u64,
    next: u64,
    recent: BTreeMap<u64, [u8; 32]>,
    seen: BTreeSet<[u8; 32]>,
    outputs: Vec<Received>,
}

impl ScanState {
    /// Nothing scanned yet.
    pub fn new(birthday: u64) -> Self {
        Self {
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

    /// The outputs held, first seen first.
    pub fn outputs(&self) -> &[Received] {
        &self.outputs
    }

    /// Block `height` with its hash, its parent's hash and what the scanner found.
    /// A repeated output key is dropped (burning bug: only the first counts).
    pub fn apply(
        &mut self,
        height: u64,
        hash: [u8; 32],
        previous: [u8; 32],
        found: Vec<Received>,
    ) -> Step {
        if height != self.next {
            return Step::OutOfOrder;
        }
        let parent = height.checked_sub(1);
        if let Some(held) = parent.and_then(|p| self.recent.get(&p)) {
            if *held != previous {
                self.rewind();
                return Step::Reorg;
            }
        }
        self.recent.insert(height, hash);
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

    /// `(balance, pending)` in piconero at the daemon's chain height.
    pub fn balance(&self, daemon_height: u64) -> (u64, u64) {
        self.outputs.iter().fold((0, 0), |(ok, wait), r| {
            if daemon_height.saturating_sub(r.height) >= CONFIRMATIONS {
                (ok.saturating_add(r.amount), wait)
            } else {
                (ok, wait.saturating_add(r.amount))
            }
        })
    }

    /// Drop the newest block; past the remembered window, start over.
    fn rewind(&mut self) {
        let Some((top, _)) = self.recent.pop_last() else {
            *self = Self::new(self.birthday);
            return;
        };
        if self.recent.is_empty() {
            *self = Self::new(self.birthday);
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
