// SPDX-License-Identifier: GPL-3.0-or-later
//! The scan core: repeat-ignore, reorgs, confirmations, the fork pause
//! (design §7, §9; plan §10.11-12).

use molt_treasury::scan::{
    BlockScanner, Lock, Received, ScanFault, ScanState, StandardScanner, Step, CONFIRMATIONS,
    RECENT_BLOCKS,
};
use molt_treasury::{EdwardsPoint, MoneroScalar};
use monero_wallet::block::{Block, BlockHeader};
use monero_wallet::ed25519::Point;
use monero_wallet::extra::ExtraField;
use monero_wallet::interface::ScannableBlock;
use monero_wallet::transaction::{Input, Output, Timelock, Transaction, TransactionPrefix};
use zeroize::Zeroizing;

fn out(key: u8, amount: u64, height: u64) -> Received {
    Received {
        key: [key; 32],
        amount,
        height,
        at: 0,
        tx: [key; 32],
        index_in_tx: 0,
        lock: Lock::None,
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
fn a_locked_output_stays_pending_until_it_unlocks() {
    let mut s = ScanState::new(10);
    let mined = Received {
        lock: Lock::Block(70),
        ..out(1, 5, 10)
    };
    let timed = Received {
        lock: Lock::Time(1_700_000_000),
        ..out(2, 3, 10)
    };
    s.apply(10, hash(10, 0), hash(9, 0), vec![mined, timed]);
    assert_eq!(s.balance(30), (0, 8));
    assert_eq!(s.balance(69), (0, 8));
    assert_eq!(s.balance(70), (5, 3));
}

#[test]
fn the_window_bounds_how_far_a_reorg_steps_back() {
    let top = u64::try_from(RECENT_BLOCKS).expect("window") + 50;
    let mut s = ScanState::new(0);
    for h in 0..top {
        s.apply(h, hash(h, 0), hash(h.wrapping_sub(1), 0), vec![]);
    }
    // each foreign parent drops one remembered block; the window's last one restarts
    for k in 1..=RECENT_BLOCKS {
        let at = s.next_height();
        assert_eq!(
            s.apply(at, hash(at, 1), hash(at - 1, 1), vec![]),
            Step::Reorg
        );
        let want = if k == RECENT_BLOCKS {
            0
        } else {
            top - u64::try_from(k).expect("k")
        };
        assert_eq!(s.next_height(), want, "reorg {k}");
    }
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

/// A miner transaction paying `scanner()`'s address, locked like a coinbase.
fn mined_block(number: usize, amount: u64) -> ScannableBlock {
    use ciphersuite::group::Group;
    let g = EdwardsPoint::generator().0;
    let r = dalek_ff_group::Scalar::from(7u64);
    let tx_key = r * g;
    let view = MoneroScalar::hash(b"scan test view").into();
    let mut derivation = (view * tx_key)
        .mul_by_cofactor()
        .compress()
        .to_bytes()
        .to_vec();
    derivation.push(0); // varint output index 0
    let shared = MoneroScalar::hash(&derivation).into();
    let key = Point::from(shared * g + g).compress();
    let miner = Transaction::V2 {
        prefix: TransactionPrefix {
            additional_timelock: Timelock::Block(number + 60),
            inputs: vec![Input::Gen(number)],
            outputs: vec![Output {
                amount: Some(amount),
                key,
                view_tag: None,
            }],
            extra: ExtraField::PublicKey(Point::from(tx_key).compress()).serialize(),
        },
        proofs: None,
    };
    let header = BlockHeader {
        hardfork_version: 16,
        hardfork_signal: 16,
        timestamp: 1_600_000_000 + u64::try_from(number).expect("number"),
        previous: [0; 32],
        nonce: 0,
    };
    ScannableBlock {
        block: Block::new(header, miner, vec![]).expect("block"),
        transactions: vec![],
        output_index_for_first_ringct_output: Some(0),
    }
}

#[test]
fn a_mined_output_carries_its_lock() {
    let found = scanner().scan_block(mined_block(500, 9)).expect("scan");
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].amount, found[0].height), (9, 500));
    assert_eq!(found[0].lock, Lock::Block(560));
    assert_eq!(found[0].at, 1_600_000_500, "the block's time");

    let mut s = ScanState::new(500);
    s.apply(500, hash(500, 0), hash(499, 0), found);
    assert_eq!(s.balance(520), (0, 9));
    assert_eq!(s.balance(560), (9, 0));
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

#[test]
fn a_scanner_opens_from_the_address_and_its_view_key() {
    use ciphersuite::group::Group;
    use molt_treasury::keys::{scalar_bytes, standard_address};
    use molt_treasury::Network;
    let view = Zeroizing::new(MoneroScalar::hash(b"scan test view"));
    let address = standard_address(EdwardsPoint::generator(), view.clone(), Network::Mainnet)
        .expect("address")
        .to_string();
    let key = scalar_bytes(&view);
    let mut sc = StandardScanner::for_address(&address, Network::Mainnet, &key).expect("opens");
    let found = sc.scan_block(mined_block(500, 9)).expect("scan");
    assert_eq!(found.len(), 1, "the same purse as from the keys");
    let other = scalar_bytes(&MoneroScalar::hash(b"another view"));
    assert!(StandardScanner::for_address(&address, Network::Mainnet, &other).is_err(), "a view key the address does not carry");
    assert!(StandardScanner::for_address(&address, Network::Testnet, &key).is_err(), "another network");
}
