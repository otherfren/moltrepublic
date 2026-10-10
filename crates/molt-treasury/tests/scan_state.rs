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
        at: 1_700_000_000 + height,
        tx: [key ^ 0xff; 32],
        index_in_tx: u64::from(key),
        lock,
    }
}

const ADDR: &str = "the purse";

fn hash(h: u64) -> [u8; 32] {
    let mut b = [7; 32];
    b[..8].copy_from_slice(&h.to_le_bytes());
    b
}

fn scanned() -> ScanState {
    let mut s = ScanState::new(ADDR, 10);
    s.apply(10, hash(10), hash(9), 0, vec![out(1, 5, 10, Lock::Block(70))]);
    s.apply(11, hash(11), hash(10), 0, vec![]);
    s.apply(
        12,
        hash(12),
        hash(11),
        0,
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
    let step = back.apply(13, hash(13), hash(12), 0, vec![out(2, 9, 13, Lock::None)]);
    assert_eq!(step, molt_treasury::scan::Step::Applied(vec![]));
    assert_eq!(
        ScanState::decode(&ScanState::new(ADDR, 5).encode()).expect("empty"),
        ScanState::new(ADDR, 5)
    );
    assert_eq!(back.address(), ADDR, "the purse it was scanned for");
}

#[test]
fn the_scan_state_layout_is_pinned() {
    let mut s = ScanState::new(ADDR, 10);
    s.apply(10, hash(10), hash(9), 1_700_000_010, vec![out(1, 5, 10, Lock::Block(70))]);
    let mut want = Vec::new();
    put_bytes(&mut want, b"molt-wallet-scan-v3");
    put_bytes(&mut want, ADDR.as_bytes());
    want.extend_from_slice(&10u64.to_le_bytes());
    want.extend_from_slice(&11u64.to_le_bytes());
    want.extend_from_slice(&1u32.to_le_bytes());
    want.extend_from_slice(&10u64.to_le_bytes());
    want.extend_from_slice(&hash(10));
    want.extend_from_slice(&1_700_000_010u64.to_le_bytes());
    want.extend_from_slice(&1u32.to_le_bytes());
    want.extend_from_slice(&[1; 32]);
    want.extend_from_slice(&5u64.to_le_bytes());
    want.extend_from_slice(&10u64.to_le_bytes());
    want.extend_from_slice(&1_700_000_010u64.to_le_bytes());
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
    // a lock-free output with a value, `next` not past what it holds
    let mut s = ScanState::new(ADDR, 10);
    s.apply(10, hash(10), hash(9), 0, vec![out(1, 5, 10, Lock::None)]);
    let base = s.encode();
    let at = |field_from_end: usize| base.len() - field_from_end;
    let mut low = base.clone();
    let h = at(8 + 1 + 8 + 32 + 8 + 8);
    low[h..h + 8].copy_from_slice(&9u64.to_le_bytes());
    assert!(refused(&low), "output below the birthday");
    let mut lock = base.clone();
    let k = at(8 + 1);
    lock[k] = 3;
    assert!(refused(&lock), "unknown lock kind");
    let mut none_value = base.clone();
    none_value[at(8)] = 1;
    assert!(refused(&none_value), "a lock-free output carries no value");

    let mut twice = ScanState::new(ADDR, 10);
    twice.apply(10, hash(10), hash(9), 0, vec![out(1, 5, 10, Lock::None)]);
    twice.apply(11, hash(11), hash(10), 0, vec![out(2, 5, 11, Lock::None)]);
    let mut dup = twice.encode();
    let second = dup.len() - (32 + 8 + 8 + 8 + 32 + 8 + 1 + 8);
    dup[second..second + 32].copy_from_slice(&[1; 32]);
    assert!(refused(&dup), "a repeated output key");

    let mut ahead = base;
    // `next` sits after the tag and the birthday
    let n = 4 + b"molt-wallet-scan-v3".len() + 4 + ADDR.len() + 8;
    ahead[n..n + 8].copy_from_slice(&10u64.to_le_bytes());
    assert!(refused(&ahead), "next at a held block and output");
}

/// An output-free image: `birthday`, `next`, and the remembered heights.
fn window(birthday: u64, next: u64, heights: &[u64]) -> Vec<u8> {
    let mut b = Vec::new();
    put_bytes(&mut b, b"molt-wallet-scan-v3");
    put_bytes(&mut b, ADDR.as_bytes());
    b.extend_from_slice(&birthday.to_le_bytes());
    b.extend_from_slice(&next.to_le_bytes());
    b.extend_from_slice(&u32::try_from(heights.len()).expect("count").to_le_bytes());
    for h in heights {
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&hash(*h));
        b.extend_from_slice(&0u64.to_le_bytes());
    }
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

#[test]
fn a_block_window_apply_could_not_produce_is_refused() {
    let decodes = |b: &[u8]| ScanState::decode(b).is_ok();
    assert!(decodes(&window(10, 13, &[10, 11, 12])), "the honest window");
    assert!(!decodes(&window(10, 9, &[])), "next below the birthday");
    assert!(!decodes(&window(10, 13, &[9, 11, 12])), "a block below the birthday");
    assert!(!decodes(&window(10, 13, &[10, 11, 13])), "a block at next");
    assert!(!decodes(&window(10, 13, &[10, 12, 11])), "out of order");
    assert!(!decodes(&window(10, 13, &[10, 11, 11])), "a repeated height");
    let cap = u64::try_from(molt_treasury::scan::RECENT_BLOCKS).expect("cap");
    let full: Vec<u64> = (10..10 + cap).collect();
    assert!(decodes(&window(10, 10 + cap, &full)), "a full window");
    let over: Vec<u64> = (10..11 + cap).collect();
    assert!(!decodes(&window(10, 11 + cap, &over)), "beyond the window");
}

/// A purse holds at most `MAX_OUTPUTS`: a block past it changes nothing
/// (a hostile daemon cannot grow the state without bound).
#[test]
fn the_outputs_are_bounded() {
    use molt_treasury::scan::{Step, MAX_OUTPUTS};
    let many = |h: u64, n: usize| -> Vec<Received> {
        (0..n)
            .map(|i| {
                let mut key = [0u8; 32];
                key[..8].copy_from_slice(&h.to_le_bytes());
                key[8..16].copy_from_slice(&u64::try_from(i).expect("small").to_le_bytes());
                Received { key, ..out(0, 1, h, Lock::None) }
            })
            .collect()
    };
    let mut s = ScanState::new(ADDR, 10);
    assert!(matches!(s.apply(10, hash(10), hash(9), 0, many(10, MAX_OUTPUTS - 1)), Step::Applied(_)));
    let before = s.clone();
    assert!(matches!(s.apply(11, hash(11), hash(10), 0, many(11, 2)), Step::Full));
    assert_eq!(s, before, "nothing changed");
    assert!(matches!(s.apply(11, hash(11), hash(10), 0, many(11, 1)), Step::Applied(_)));
    assert_eq!(s.outputs().len(), MAX_OUTPUTS);
}
