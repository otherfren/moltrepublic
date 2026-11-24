// SPDX-License-Identifier: GPL-3.0-or-later

//! The branch choice's signed bytes (`docs/chain/chain_rescue_plan.md` §1).

use serde::Deserialize;
use serde_json::Value;

use super::{approval_bytes, ChainChange, DroppedRun};

fn put(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&u32::try_from(b.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(b);
}

fn choice() -> ChainChange {
    ChainChange::BranchChosen {
        base_height: 4,
        base_hash: "b4".to_string(),
        tip_height: 6,
        tip_hash: "t6".to_string(),
        chosen_root: "r5".to_string(),
        dropped: vec![DroppedRun {
            from_height: 5,
            first_hash: "x5".to_string(),
            head_height: 8,
            head_hash: "x8".to_string(),
        }],
    }
}

#[test]
fn branch_chosen_approval_bytes_pin() {
    let mut want = b"molt-chain-change-v2\0".to_vec();
    put(&mut want, b"f00");
    want.extend_from_slice(&7u64.to_le_bytes());
    want.push(6);
    want.extend_from_slice(&4u64.to_le_bytes());
    put(&mut want, b"b4");
    want.extend_from_slice(&6u64.to_le_bytes());
    put(&mut want, b"t6");
    put(&mut want, b"r5");
    want.extend_from_slice(&1u64.to_le_bytes());
    want.extend_from_slice(&5u64.to_le_bytes());
    put(&mut want, b"x5");
    want.extend_from_slice(&8u64.to_le_bytes());
    put(&mut want, b"x8");
    assert_eq!(approval_bytes("f00", 7, &choice()), want);
}

/// Every field is signed: changing any one changes the bytes.
#[test]
fn every_branch_field_is_signed() {
    let base = approval_bytes("f00", 7, &choice());
    let ChainChange::BranchChosen { base_height, base_hash, tip_height, tip_hash, chosen_root, dropped } = choice()
    else {
        unreachable!("a branch choice");
    };
    let variants = [
        ChainChange::BranchChosen { base_height: 3, base_hash: base_hash.clone(), tip_height, tip_hash: tip_hash.clone(), chosen_root: chosen_root.clone(), dropped: dropped.clone() },
        ChainChange::BranchChosen { base_height, base_hash: "b9".to_string(), tip_height, tip_hash: tip_hash.clone(), chosen_root: chosen_root.clone(), dropped: dropped.clone() },
        ChainChange::BranchChosen { base_height, base_hash: base_hash.clone(), tip_height: 5, tip_hash: tip_hash.clone(), chosen_root: chosen_root.clone(), dropped: dropped.clone() },
        ChainChange::BranchChosen { base_height, base_hash: base_hash.clone(), tip_height, tip_hash: "t9".to_string(), chosen_root: chosen_root.clone(), dropped: dropped.clone() },
        ChainChange::BranchChosen { base_height, base_hash: base_hash.clone(), tip_height, tip_hash: tip_hash.clone(), chosen_root: String::new(), dropped: dropped.clone() },
        ChainChange::BranchChosen { base_height, base_hash, tip_height, tip_hash, chosen_root, dropped: Vec::new() },
    ];
    for v in &variants {
        assert_ne!(approval_bytes("f00", 7, v), base, "{v:?}");
    }
    assert_ne!(approval_bytes("f00", 8, &choice()), base, "the height");
    assert_ne!(approval_bytes("f01", 7, &choice()), base, "the republic");
}

/// The additive-only rule: a build that predates the variant refuses the
/// block instead of reading it as something else.
#[test]
fn an_older_reader_stops_at_branch_chosen() {
    #[derive(Debug, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    #[allow(dead_code)]
    enum Older {
        Genesis {},
        Applied {},
        Membership {},
        Checkpoint {},
        CheckpointFolded {},
        CheckpointVault {},
    }
    let json = serde_json::to_value(choice()).expect("serializes");
    assert_eq!(json.get("kind").and_then(Value::as_str), Some("branch_chosen"));
    assert!(serde_json::from_value::<Older>(json.clone()).is_err());
    assert_eq!(serde_json::from_value::<ChainChange>(json).expect("round trip"), choice());
}
