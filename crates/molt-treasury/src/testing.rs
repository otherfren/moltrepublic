// SPDX-License-Identifier: GPL-3.0-or-later
//! Real blocks for the stub daemon (feature `test-blocks`, tests only).
//! A payment is a miner transaction without its lock: the scanner reads
//! both the same way, and a block needs no ring signature for it.

use monero_wallet::address::{MoneroAddress, Network};
use monero_wallet::block::{Block, BlockHeader};
use monero_wallet::ed25519::{Point, Scalar};
use monero_wallet::extra::ExtraField;
use monero_wallet::transaction::{Input, Output, Transaction, TransactionPrefix};
pub use monero_wallet::transaction::Timelock;

/// One serialized block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBlock {
    /// Its number.
    pub number: u64,
    /// The consensus encoding.
    pub blob: Vec<u8>,
    /// Its id.
    pub hash: [u8; 32],
    /// Its miner transaction's outputs.
    pub outputs: usize,
}

/// What a block pays to.
#[derive(Debug, Clone, Copy)]
pub struct Pay<'a> {
    /// A standard address.
    pub address: &'a str,
    /// Its network.
    pub network: Network,
    /// Piconero.
    pub amount: u64,
    /// The output's own lock.
    pub lock: Timelock,
}

/// Block `number` on `previous`; `salt` tells two forks apart.
///
/// # Panics
/// On an address that does not parse (a test bug).
pub fn block(number: u64, previous: [u8; 32], hardfork: u8, salt: u32, pay: Option<Pay<'_>>) -> TestBlock {
    let n = usize::try_from(number).expect("number");
    let (outputs, extra, lock) = match pay {
        Some(p) => {
            let addr = MoneroAddress::from_str(p.network, p.address).expect("address");
            let g = <dalek_ff_group::EdwardsPoint as ciphersuite::group::Group>::generator().0;
            let r: dalek_ff_group::Scalar =
                Scalar::hash([b"molt-test-tx-key".as_slice(), &number.to_le_bytes(), &salt.to_le_bytes()].concat()).into();
            let mut derivation = (r * addr.view().into()).mul_by_cofactor().compress().to_bytes().to_vec();
            derivation.push(0);
            let shared: dalek_ff_group::Scalar = Scalar::hash(&derivation).into();
            let key = Point::from(shared * g + addr.spend().into()).compress();
            let extra = ExtraField::PublicKey(Point::from(r * g).compress()).serialize();
            (vec![Output { amount: Some(p.amount), key, view_tag: None }], extra, p.lock)
        }
        // a miner output to nobody: the scanner checks the version only where outputs are
        None => {
            let g = <dalek_ff_group::EdwardsPoint as ciphersuite::group::Group>::generator().0;
            let k: dalek_ff_group::Scalar =
                Scalar::hash([b"molt-test-nobody".as_slice(), &number.to_le_bytes(), &salt.to_le_bytes()].concat()).into();
            let out = Output { amount: Some(1), key: Point::from(k * g).compress(), view_tag: None };
            (vec![out], ExtraField::PublicKey(Point::from(k * g).compress()).serialize(), Timelock::None)
        }
    };
    let count = outputs.len();
    let miner = Transaction::V2 {
        prefix: TransactionPrefix { additional_timelock: lock, inputs: vec![Input::Gen(n)], outputs, extra },
        proofs: None,
    };
    let header = BlockHeader {
        hardfork_version: hardfork,
        hardfork_signal: hardfork,
        timestamp: 1_600_000_000 + number * 120,
        previous,
        nonce: salt,
    };
    let b = Block::new(header, miner, vec![]).expect("block");
    TestBlock { number, blob: b.serialize(), hash: b.hash(), outputs: count }
}

/// A standard address nobody in the test holds the keys of.
///
/// # Panics
/// Never for a seed whose hash is a valid scalar (all of them).
pub fn any_address(network: Network, seed: &[u8]) -> String {
    let g = <dalek_ff_group::EdwardsPoint as ciphersuite::group::Group>::generator();
    let spend: dalek_ff_group::Scalar = Scalar::hash([b"molt-test-spend".as_slice(), seed].concat()).into();
    let view = zeroize::Zeroizing::new(Scalar::hash([b"molt-test-view".as_slice(), seed].concat()));
    crate::keys::standard_address(g * spend, view, network).expect("address").to_string()
}

/// A chain from `from` to `to` inclusive on `previous`, paying at the given heights.
pub fn chain(from: u64, to: u64, previous: [u8; 32], salt: u32, pays: &[(u64, Pay<'_>)]) -> Vec<TestBlock> {
    let mut prev = previous;
    (from..=to)
        .map(|h| {
            let pay = pays.iter().find(|(at, _)| *at == h).map(|(_, p)| *p);
            let b = block(h, prev, 16, salt, pay);
            prev = b.hash;
            b
        })
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{BlockScanner, Lock, StandardScanner};
    use ciphersuite::group::Group;
    use monero_wallet::interface::ScannableBlock;
    use zeroize::Zeroizing;

    #[test]
    fn a_test_block_pays_its_address() {
        let spend = dalek_ff_group::EdwardsPoint::generator();
        let view = Zeroizing::new(Scalar::hash(b"testing view"));
        let address = crate::keys::standard_address(spend, view.clone(), Network::Testnet)
            .expect("address")
            .to_string();
        let pay = Pay { address: &address, network: Network::Testnet, amount: 7, lock: Timelock::None };
        let b = block(40, [3; 32], 16, 0, Some(pay));
        let nobody = block(41, b.hash, 17, 0, None);
        assert_eq!(nobody.outputs, 1, "every block has its miner output");
        let parsed = Block::read(&mut b.blob.as_slice()).expect("decodes");
        assert_eq!((parsed.hash(), parsed.number(), b.outputs), (b.hash, 40, 1));
        let scannable =
            ScannableBlock { block: parsed, transactions: vec![], output_index_for_first_ringct_output: Some(0) };
        let mut sc = StandardScanner::new(spend, view).expect("scanner");
        let found = sc.scan_block(scannable).expect("scan");
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].amount, found[0].height, found[0].lock), (7, 40, Lock::None));
        let fork = Block::read(&mut nobody.blob.as_slice()).expect("decodes");
        let fork = ScannableBlock { block: fork, transactions: vec![], output_index_for_first_ringct_output: Some(1) };
        assert_eq!(sc.scan_block(fork), Err(crate::scan::ScanFault::UpdateNeeded(17)));
    }
}
