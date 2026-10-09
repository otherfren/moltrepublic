// SPDX-License-Identifier: GPL-3.0-or-later
//! The scan core: repeat-ignore, reorgs, confirmations, the fork pause
//! (design §7, §9; plan §10.11-12).

use molt_treasury::scan::{
    BlockScanner, Received, ScanFault, ScanState, StandardScanner, Step, CONFIRMATIONS,
};
use molt_treasury::{EdwardsPoint, MoneroScalar};
use monero_wallet::block::{Block, BlockHeader};
use monero_wallet::interface::ScannableBlock;
use monero_wallet::transaction::{Input, Timelock, Transaction, TransactionPrefix};
use zeroize::Zeroizing;

fn out(key: u8, amount: u64, height: u64) -> Received {
    Received {
        key: [key; 32],
        amount,
        height,
        tx: [key; 32],
        index_in_tx: 0,
    }
}

fn hash(h: u64, fork: u8) -> [u8; 32] {
    let mut b = [fork; 32];
    b[..8].copy_from_slice(&h.to_le_bytes());
    b
}

#[test]
fn a_repeated_output_key_is_ignored() {
    let mut s = ScanState::new(100);
    assert_eq!(s.next_height(), 100);
    assert_eq!(
        s.apply(100, hash(100, 0), hash(99, 0), vec![out(1, 5, 100)]),
        Step::Applied(vec![out(1, 5, 100)])
    );
    assert_eq!(
        s.apply(
            101,
            hash(101, 0),
            hash(100, 0),
            vec![out(1, 7, 101), out(2, 3, 101)]
        ),
        Step::Applied(vec![out(2, 3, 101)])
    );
    assert_eq!(s.outputs(), &[out(1, 5, 100), out(2, 3, 101)]);
    assert_eq!(s.next_height(), 102);
}

#[test]
fn twenty_confirmations_split_balance_and_pending() {
    let mut s = ScanState::new(10);
    s.apply(10, hash(10, 0), hash(9, 0), vec![out(1, 5, 10)]);
    s.apply(11, hash(11, 0), hash(10, 0), vec![out(2, 3, 11)]);
    assert_eq!(CONFIRMATIONS, 20);
    assert_eq!(s.balance(30), (5, 3));
    assert_eq!(s.balance(31), (8, 0));
    assert_eq!(s.balance(12), (0, 8));
}

#[test]
fn a_reorg_rewinds_and_a_deep_one_rescans_from_the_birthday() {
    let mut s = ScanState::new(10);
    for h in 10..15 {
        s.apply(
            h,
            hash(h, 0),
            hash(h - 1, 0),
            vec![out(u8::try_from(h).expect("h"), 1, h)],
        );
    }
    // block 15 builds on a different 14: rewind one, refetch 14
    assert_eq!(s.apply(15, hash(15, 1), hash(14, 1), vec![]), Step::Reorg);
    assert_eq!(s.next_height(), 14);
    assert_eq!(s.outputs().len(), 4);
    // the key seen in the orphaned block counts again on the new chain
    assert_eq!(
        s.apply(14, hash(14, 1), hash(13, 0), vec![out(14, 2, 14)]),
        Step::Applied(vec![out(14, 2, 14)])
    );
    assert_eq!(s.next_height(), 15);

    // a fork below every remembered block: start over at the birthday
    let mut s = ScanState::new(10);
    s.apply(10, hash(10, 0), hash(9, 0), vec![out(1, 1, 10)]);
    assert_eq!(s.apply(11, hash(11, 1), hash(10, 1), vec![]), Step::Reorg);
    assert_eq!(s.next_height(), 10);
    assert!(s.outputs().is_empty());
    assert_eq!(
        s.apply(10, hash(10, 1), hash(9, 1), vec![out(1, 1, 10)]),
        Step::Applied(vec![out(1, 1, 10)])
    );

    // a block for another height is refused, nothing changes
    assert_eq!(
        s.apply(42, hash(42, 0), hash(41, 0), vec![]),
        Step::OutOfOrder
    );
    assert_eq!(s.next_height(), 11);
}

fn block(hardfork: u8, number: usize, claimed_txs: usize) -> ScannableBlock {
    let miner = Transaction::V2 {
        prefix: TransactionPrefix {
            additional_timelock: Timelock::Block(number + 60),
            inputs: vec![Input::Gen(number)],
            outputs: vec![],
            extra: vec![],
        },
        proofs: None,
    };
    let header = BlockHeader {
        hardfork_version: hardfork,
        hardfork_signal: hardfork,
        timestamp: 0,
        previous: [0; 32],
        nonce: 0,
    };
    ScannableBlock {
        block: Block::new(header, miner, vec![[0; 32]; claimed_txs]).expect("block"),
        transactions: vec![],
        output_index_for_first_ringct_output: Some(0),
    }
}

fn scanner() -> StandardScanner {
    use ciphersuite::group::Group;
    let spend = EdwardsPoint::generator();
    let view = Zeroizing::new(MoneroScalar::hash(b"scan test view"));
    StandardScanner::new(spend, view).expect("scanner")
}

#[test]
fn unsupported_protocol_pauses_and_a_decode_error_does_not() {
    let mut sc = scanner();
    assert_eq!(sc.scan_block(block(16, 5, 0)), Ok(vec![]));

    let paused = sc.scan_block(block(17, 6, 0)).expect_err("fork");
    assert_eq!(paused, ScanFault::UpdateNeeded(17));
    assert_eq!(paused.paused(), Some("update needed"));

    let invalid = sc
        .scan_block(block(17, 7, 1))
        .expect_err("inconsistent block");
    assert!(matches!(invalid, ScanFault::Daemon(_)), "{invalid:?}");
    assert_eq!(invalid.paused(), None);

    let undecodable = ScanFault::decode(&std::io::Error::other("unknown output type"));
    assert!(matches!(undecodable, ScanFault::Daemon(_)));
    assert_eq!(undecodable.paused(), None);
}

#[test]
fn an_identity_spend_key_has_no_scanner() {
    use ciphersuite::group::Group;
    let view = Zeroizing::new(MoneroScalar::hash(b"scan test view"));
    assert!(StandardScanner::new(EdwardsPoint::identity(), view).is_err());
}
