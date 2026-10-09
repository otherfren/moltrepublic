// SPDX-License-Identifier: GPL-3.0-or-later
//! The scan file's bytes (design §6, plan §9): the scanner's progress
//! survives a restart; a damaged image is refused, never half-read.

use molt_core::put_bytes;
use molt_treasury::scan::{Lock, Received, ScanState};
use molt_treasury::TreasuryError;

fn out(key: u8, amount: u64, height: u64, lock: Lock) -> Received {
    Received {
        key: [key; 32],
        amount,
        height,
        tx: [key ^ 0xff; 32],
        index_in_tx: u64::from(key),
        lock,
    }
}

fn hash(h: u64) -> [u8; 32] {
    let mut b = [7; 32];
    b[..8].copy_from_slice(&h.to_le_bytes());
    b
}

fn scanned() -> ScanState {
    let mut s = ScanState::new(10);
    s.apply(10, hash(10), hash(9), vec![out(1, 5, 10, Lock::Block(70))]);
    s.apply(11, hash(11), hash(10), vec![]);
    s.apply(
        12,
        hash(12),
        hash(11),
        vec![out(2, 3, 12, Lock::Time(9)), out(3, 4, 12, Lock::None)],
    );
    s
}

#[test]
fn the_scan_state_round_trips() {
    let s = scanned();
    let back = ScanState::decode(&s.encode()).expect("decodes");
    assert_eq!(back, s);
    // the seen keys come back too: a repeat stays ignored after a restart
    let mut back = back;
    let step = back.apply(13, hash(13), hash(12), vec![out(2, 9, 13, Lock::None)]);
    assert_eq!(step, molt_treasury::scan::Step::Applied(vec![]));
    assert_eq!(
        ScanState::decode(&ScanState::new(5).encode()).expect("empty"),
        ScanState::new(5)
    );
}

#[test]
fn the_scan_state_layout_is_pinned() {
    let mut s = ScanState::new(10);
    s.apply(10, hash(10), hash(9), vec![out(1, 5, 10, Lock::Block(70))]);
    let mut want = Vec::new();
    put_bytes(&mut want, b"molt-wallet-scan-v1");
    want.extend_from_slice(&10u64.to_le_bytes());
    want.extend_from_slice(&11u64.to_le_bytes());
    want.extend_from_slice(&1u32.to_le_bytes());
    want.extend_from_slice(&10u64.to_le_bytes());
    want.extend_from_slice(&hash(10));
    want.extend_from_slice(&1u32.to_le_bytes());
    want.extend_from_slice(&[1; 32]);
    want.extend_from_slice(&5u64.to_le_bytes());
    want.extend_from_slice(&10u64.to_le_bytes());
    want.extend_from_slice(&[0xfe; 32]);
    want.extend_from_slice(&1u64.to_le_bytes());
    want.push(1);
    want.extend_from_slice(&70u64.to_le_bytes());
    assert_eq!(s.encode(), want);
}

#[test]
fn a_damaged_scan_state_is_refused() {
    let good = scanned().encode();
    let refused = |b: &[u8]| ScanState::decode(b) == Err(TreasuryError::ScanState);
    assert!(refused(&good[..good.len() - 1]), "truncated");
    assert!(refused(&[good.as_slice(), &[0]].concat()), "trailing byte");
    let mut tag = good.clone();
    tag[4] ^= 1;
    assert!(refused(&tag), "foreign tag");

    // an output outside [birthday, next), a repeated key, an unknown lock,
    // a lock-free output with a value, a block not below `next`
    let mut s = ScanState::new(10);
    s.apply(10, hash(10), hash(9), vec![out(1, 5, 10, Lock::None)]);
    let base = s.encode();
    let at = |field_from_end: usize| base.len() - field_from_end;
    let mut low = base.clone();
    let h = at(8 + 1 + 8 + 32 + 8);
    low[h..h + 8].copy_from_slice(&9u64.to_le_bytes());
    assert!(refused(&low), "output below the birthday");
    let mut lock = base.clone();
    let k = at(8 + 1);
    lock[k] = 3;
    assert!(refused(&lock), "unknown lock kind");
    let mut none_value = base.clone();
    none_value[at(8)] = 1;
    assert!(refused(&none_value), "a lock-free output carries no value");

    let mut twice = ScanState::new(10);
    twice.apply(10, hash(10), hash(9), vec![out(1, 5, 10, Lock::None)]);
    twice.apply(11, hash(11), hash(10), vec![out(2, 5, 11, Lock::None)]);
    let mut dup = twice.encode();
    let second = dup.len() - (32 + 8 + 8 + 32 + 8 + 1 + 8);
    dup[second..second + 32].copy_from_slice(&[1; 32]);
    assert!(refused(&dup), "a repeated output key");

    let mut ahead = base;
    // `next` sits after the tag and the birthday
    let n = 4 + b"molt-wallet-scan-v1".len() + 8;
    ahead[n..n + 8].copy_from_slice(&10u64.to_le_bytes());
    assert!(refused(&ahead), "a remembered block at or past next");
}
