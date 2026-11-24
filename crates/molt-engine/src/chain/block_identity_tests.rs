// SPDX-License-Identifier: GPL-3.0-or-later

//! Block identity v2 (`docs_archive/chain/block_identity_plan.md`): the layout
//! switch by activation height, its boundary, and the holder's duplicate
//! rule.

use super::test_support::*;
use super::*;
use molt_core::chain::{block_id_bytes, BLOCK_V2_FEATURE};

const SEATS3: [&str; 3] = ["petra", "walter", "dora"];

/// A 2-of-3 chain whose block 1 enables v2 (active from height 2), grown
/// to `len` blocks, every block sealed by petra and walter.
fn enabled(len: usize) -> Builder {
    let mut b = Builder::new(&SEATS3, 2);
    b.commit_org(1, "set_features", &format!("memory {BLOCK_V2_FEATURE}"), &["petra", "walter"]);
    while b.blocks.len() < len {
        let id = u64::try_from(b.blocks.len()).expect("small") + 10;
        b.commit_applied(id, &["petra", "walter"]);
    }
    b
}

/// The same chain without the enabling block.
fn plain(len: usize) -> Builder {
    let mut b = Builder::new(&SEATS3, 2);
    while b.blocks.len() < len {
        let id = u64::try_from(b.blocks.len()).expect("small") + 10;
        b.commit_applied(id, &["petra", "walter"]);
    }
    b
}

/// `blocks[h]` re-attested by `signers`.
fn reattested(b: &Builder, h: usize, signers: &[&str]) -> ChainBlock {
    let v = &b.blocks[h];
    b.seal_on(v.height, &v.prev, v.change.clone(), signers)
}

fn v2_id(b: &Builder, block: &ChainBlock) -> String {
    molt_storage::content_hash(&block_id_bytes(&b.republic_id, block))
}

struct OlderBuild;

impl OlderBuild {
    fn on() -> Self {
        OLDER_BUILD.with(|c| c.set(true));
        OlderBuild
    }
}

impl Drop for OlderBuild {
    fn drop(&mut self) {
        OLDER_BUILD.with(|c| c.set(false));
    }
}

#[test]
fn the_boundary_links_v1_below_and_v2_from_the_activation() {
    let b = enabled(5);
    let head = verify_chain(&b.blocks).expect("verifies");
    assert_eq!(head.block_v2_from, Some(2));
    assert_eq!(b.blocks[2].prev, block_hash(&b.republic_id, &b.blocks[1]), "H links to the v1 hash of H-1");
    assert_eq!(b.blocks[3].prev, v2_id(&b, &b.blocks[2]), "H+1 links to the v2 identity of H");
    assert_eq!(head.hash, v2_id(&b, &b.blocks[4]));
    assert_eq!(verify_chain(&plain(5).blocks).expect("verifies").block_v2_from, None);
}

#[test]
fn another_attestation_keeps_the_identity_from_the_activation() {
    let mut b = enabled(5);
    let variant = reattested(&b, 3, &["walter", "dora"]);
    assert_eq!(v2_id(&b, &variant), v2_id(&b, &b.blocks[3]));
    b.blocks[3] = variant;
    verify_chain(&b.blocks).expect("any m-subset attests the same block");
    // counter: before the activation the signer set is in the link
    let mut p = plain(5);
    p.blocks[3] = reattested(&p, 3, &["walter", "dora"]);
    let err = verify_chain(&p.blocks).expect_err("a v1 variant moves the link");
    assert!(err.contains("does not link"), "{err}");
}

#[test]
fn a_variant_below_the_activation_still_moves_the_link() {
    let mut b = enabled(4);
    b.blocks[1] = reattested(&b, 1, &["walter", "dora"]);
    let err = verify_chain(&b.blocks).expect_err("the enabling block is v1");
    assert!(err.contains("block 2 does not link"), "{err}");
}

#[test]
fn the_identity_does_not_vouch_for_the_attestation() {
    let mut b = enabled(4);
    let mut forged = b.blocks[3].clone();
    forged.sigs.retain(|a| a.member == "petra");
    forged.sigs.push(RosterAttestation { member: "dora".to_string(), sig: "00".repeat(64) });
    assert_eq!(v2_id(&b, &forged), v2_id(&b, &b.blocks[3]));
    b.blocks[3] = forged;
    let err = verify_chain(&b.blocks).expect_err("m valid signers stay required");
    assert!(err.contains("valid approvals"), "{err}");
}

#[test]
fn an_older_build_applies_the_enabling_block_and_stops_after_it() {
    let b = enabled(4);
    let _older = OlderBuild::on();
    verify_chain(&b.blocks[..2]).expect("the enabling block applies");
    let err = verify_chain(&b.blocks[..3]).expect_err("H is signed over its prev");
    assert_eq!(refusal_rule(&err), "threshold", "{err}");
}

#[test]
fn a_founding_that_ratifies_it_is_v2_from_height_one() {
    let mut b = Builder::new_with_features(&SEATS3, 2, &[BLOCK_V2_FEATURE], false);
    b.commit_applied(10, &["petra", "walter"]);
    b.commit_applied(11, &["petra", "dora"]);
    let head = verify_chain(&b.blocks).expect("verifies");
    assert_eq!(head.block_v2_from, Some(1));
    assert_eq!(b.blocks[1].prev, block_hash(&b.republic_id, &b.blocks[0]), "the genesis keeps its v1 hash");
    assert_eq!(b.blocks[2].prev, v2_id(&b, &b.blocks[1]));
}

/// A cut below, at and above H: the suffix walk reaches the full walk's
/// head identity.
#[test]
fn a_cut_on_either_side_of_the_activation_walks_alike() {
    for (enable_at, cut_at) in [(3u64, 2u64), (1, 2), (1, 4)] {
        let mut b = Builder::new(&SEATS3, 2);
        let mut blob = None;
        for h in 1..=6u64 {
            if h == enable_at {
                b.commit_org(h, "set_features", &format!("memory {BLOCK_V2_FEATURE}"), &["petra", "walter"]);
            } else if h == cut_at {
                let state = checkpoint_state(&b.blocks, h - 1).expect("state");
                let cut = b.seal(h, ChainChange::Checkpoint { upto: h - 1, state_hash: checkpoint_state_hash(&state) }, &SEATS3);
                b.push(cut);
                blob = Some(state);
            } else {
                b.commit_applied(h + 20, &["petra", "walter"]);
            }
        }
        let full = verify_chain(&b.blocks).expect("full walk");
        let blob = blob.expect("a cut");
        let at = usize::try_from(cut_at).expect("small");
        let suffix = verify_suffix_chain(&blob, &b.blocks[at..], &b.republic_id, None)
            .unwrap_or_else(|e| panic!("enable {enable_at} cut {cut_at}: {e}"));
        assert_eq!(suffix.hash, full.hash, "enable {enable_at} cut {cut_at}");
        assert_eq!(suffix.block_v2_from.is_some(), full.block_v2_from.is_some());
    }
}

/// The holder keeps the first attestation of a v2 block: another one is a
/// duplicate, neither a fork nor a divergence.
#[test]
fn a_second_attestation_is_a_duplicate_not_a_fork() {
    let b = enabled(4);
    let mut walter = chain_peer_3("walter", &b);
    let held = walter.chain.blocks[3].sigs.clone();
    for (seq, h) in [(10u64, 3usize), (11, 2)] {
        let variant = reattested(&b, h, &["petra", "dora"]);
        wire(&mut walter, "dora", seq, WorkspaceEvent::Committed(variant));
    }
    assert_eq!(walter.chain.fork_decisions, 0);
    assert!(walter.chain.diverged.is_empty() && walter.chain.fork_candidates.is_empty());
    assert_eq!(walter.chain.blocks[3].sigs, held, "the first attestation stays");
    assert_eq!(walter.chain.head.as_ref().expect("head").hash, v2_id(&b, &b.blocks[3]));
}

/// Counter-test: before the activation another attestation of the tip is
/// a fork decision (a relink).
#[test]
fn before_the_activation_a_second_attestation_is_a_fork() {
    let mut b = plain(2);
    let change = ChainChange::Applied {
        proposal_id: 50,
        surface: Surface::Memory,
        payload: serde_json::json!({ "op": "add_note", "id": 50 }),
    };
    let mut variants: Vec<ChainBlock> = [["petra", "walter"], ["petra", "dora"], ["walter", "dora"]]
        .iter()
        .map(|s| b.seal(2, change.clone(), s))
        .collect();
    variants.sort_by_key(|v| block_hash(&b.republic_id, v));
    let low = variants.remove(0);
    b.push(variants.pop().expect("the highest"));
    let mut walter = chain_peer_3("walter", &b);
    wire(&mut walter, "dora", 10, WorkspaceEvent::Committed(low));
    assert_eq!(walter.chain.fork_decisions, 1, "a v1 variant is decided");
}

// --- S2: activation -------------------------------------------------------

fn enable_payload() -> serde_json::Value {
    serde_json::json!({ "op": "set_features", "value": format!("memory {BLOCK_V2_FEATURE}") })
}

/// Proposing and signing the enabling vote waits for every seat's
/// capability; another feature vote does not.
#[test]
fn enabling_needs_every_seat_capability() {
    let b = plain(1);
    let mut petra = genesis_seat("petra", &b, b.blocks.clone());
    petra.chain.block_v2_caps_override = Some(false);
    let err = petra
        .cmd_propose(Surface::Organization, enable_payload())
        .expect_err("a seat cannot follow");
    assert!(err.to_string().contains("older build"), "{err}");
    let change = ChainChange::Applied { proposal_id: 7, surface: Surface::Organization, payload: enable_payload() };
    assert!(petra.block_v2_enable_blocker(&change).is_some());
    assert_eq!(petra.chain_sync().block_v2_wait, "a seat runs an older build");
    let other = ChainChange::Applied {
        proposal_id: 8,
        surface: Surface::Organization,
        payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
    };
    assert!(petra.block_v2_enable_blocker(&other).is_none(), "another feature is not held");
    petra.chain.block_v2_caps_override = Some(true);
    assert!(petra.block_v2_enable_blocker(&change).is_none());
    petra.cmd_propose(Surface::Organization, enable_payload()).expect("every seat can follow");
}

/// Without an announcement a seat counts as not capable.
#[test]
fn an_unheard_seat_keeps_it_off() {
    let b = plain(1);
    let petra = genesis_seat("petra", &b, b.blocks.clone());
    assert!(petra.block_v2_not_capable().is_some_and(|w| w.ends_with("has not announced")));
}

/// A holder follows the chain block by block across the activation and
/// reports it.
#[test]
fn a_holder_follows_the_chain_across_the_activation() {
    let b = enabled(6);
    let mut walter = chain_peer_3("walter", &plain(1));
    for blk in &b.blocks[1..] {
        walter.receive_block(blk.clone());
    }
    let head = walter.chain.head.as_ref().expect("head");
    assert_eq!((head.height, head.hash.as_str()), (5, b.head_hash.as_str()));
    assert_eq!(walter.chain_sync().block_v2_since, 2);
    assert_eq!(walter.chain_sync().head_hash, v2_id(&b, &b.blocks[5]));
}

/// A rejoiner verifies a served v2 chain, full or anchored on a cut above
/// the activation, to the same head identity.
#[test]
fn a_rejoiner_verifies_a_served_chain_after_the_activation() {
    let mut b = enabled(4);
    let state = checkpoint_state(&b.blocks, 3).expect("state");
    let cut = b.seal(4, ChainChange::Checkpoint { upto: 3, state_hash: checkpoint_state_hash(&state) }, &SEATS3);
    b.push(cut);
    b.commit_applied(40, &["walter", "dora"]);
    let (full, _) = verify_served(None, &b.blocks, Some(&b.republic_id)).expect("full");
    let (anchored, _) = verify_served(Some(&state), &b.blocks[4..], Some(&b.republic_id)).expect("anchored");
    assert_eq!(full.hash, anchored.hash);
    assert_eq!(full.hash, v2_id(&b, &b.blocks[5]));
}

// --- S4: `prev` in the signed bytes ---------------------------------------

/// `molt-chain-change-v3`: a signature made on one predecessor does not
/// count on another, though the link holds.
#[test]
fn a_signature_on_one_predecessor_is_refused_on_another() {
    let b = enabled(3);
    let mut other = Builder::new(&SEATS3, 2);
    other.commit_org(1, "set_features", &format!("memory {BLOCK_V2_FEATURE}"), &["walter", "dora"]);
    let mut moved = b.blocks[2].clone();
    moved.prev = other.head_hash.clone();
    other.push(moved);
    let err = verify_chain(&other.blocks).expect_err("the signatures name the other predecessor");
    assert_eq!(refusal_rule(&err), "threshold", "{err}");
    // counter: below the activation the same move verifies
    let p = plain(3);
    let mut q = Builder::new(&SEATS3, 2);
    q.commit_applied(11, &["walter", "dora"]);
    let mut moved = p.blocks[2].clone();
    moved.prev = q.head_hash.clone();
    q.push(moved);
    verify_chain(&q.blocks).expect("a v2 signature does not cover prev");
}

/// A tip swap at the same height re-signs the standing approval on the
/// new predecessor.
#[test]
fn a_standing_vote_re_signs_after_a_tip_swap_at_the_same_height() {
    let mut b = enabled(2);
    let change = |id: u64| ChainChange::Applied {
        proposal_id: id,
        surface: Surface::Memory,
        payload: serde_json::json!({ "op": "add_note", "id": id }),
    };
    let mut tips = vec![b.seal(2, change(60), &["walter", "dora"]), b.seal(2, change(61), &["walter", "dora"])];
    tips.sort_by_key(|t| v2_id(&b, t));
    let high = tips.pop().expect("high");
    let low = tips.pop().expect("low");
    let mut held = b.blocks.clone();
    held.push(high.clone());
    let mut petra = genesis_seat("petra", &b, held);
    let id = propose(&mut petra, Surface::Memory, serde_json::json!({ "op": "add_note", "title": "x" }));
    let first = petra.chain.pending_sigs.get(&id).expect("signed").clone();
    assert_eq!((first.height, first.prev.as_str()), (3, v2_id(&b, &high).as_str()));
    wire(&mut petra, "dora", 10, WorkspaceEvent::Committed(low.clone()));
    let head = petra.chain.head.clone().expect("head");
    assert_eq!(head.hash, v2_id(&b, &low), "the lower identity holds the tip");
    let now = petra.chain.pending_sigs.get(&id).expect("re-signed");
    assert_eq!((now.height, now.prev.as_str()), (3, head.hash.as_str()));
    let own = now.sigs.iter().find(|a| a.member == "petra").expect("own signature");
    let change = petra.proposal_change(id).expect("change");
    let bytes = molt_core::chain::approval_bytes_v3(&b.republic_id, 3, &head.hash, &change);
    assert!(molt_storage::identity_verify(&b.pk("petra"), &bytes, &own.sig), "signed on the new tip");
    b.push(low);
}

/// Review: a PEER's signature bound to the old tip leaves with the swap.
#[test]
fn a_foreign_signature_on_the_old_tip_is_dropped_at_a_swap() {
    let b = enabled(2);
    let change = |id: u64| ChainChange::Applied {
        proposal_id: id,
        surface: Surface::Memory,
        payload: serde_json::json!({ "op": "add_note", "id": id }),
    };
    let mut tips = [b.seal(2, change(60), &["walter", "dora"]), b.seal(2, change(61), &["walter", "dora"])];
    tips.sort_by_key(|t| v2_id(&b, t));
    let (high, low) = (tips[1].clone(), tips[0].clone());
    let mut held = b.blocks.clone();
    held.push(high.clone());
    let mut petra = genesis_seat("petra", &b, held);
    // petra holds the card without a vote: one signature does not seal
    petra.config.self_cosign = false;
    let id = propose(&mut petra, Surface::Memory, serde_json::json!({ "op": "add_note", "title": "x" }));
    let card = petra.proposal_change(id).expect("change");
    let on_high = molt_core::chain::approval_bytes_v3(&b.republic_id, 3, &v2_id(&b, &high), &card);
    let sig = molt_storage::identity_sign(b.key("walter"), &on_high);
    petra.receive_approval(id, "walter", 3, &sig);
    let holds = |st: &crate::State| st.chain.pending_sigs.get(&id).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));
    assert!(holds(&petra));
    wire(&mut petra, "dora", 10, WorkspaceEvent::Committed(low));
    assert!(!holds(&petra), "walter's signature named the displaced tip");
    assert_eq!(petra.chain.blocks.len(), 3, "and sealed nothing");
}

/// Review: a later `set_features` re-listing `block-v2` keeps H, one
/// omitting it cannot turn it off.
#[test]
fn a_later_feature_vote_neither_moves_nor_ends_the_activation() {
    let mut b = enabled(3);
    b.commit_org(40, "set_features", &format!("memory quests {BLOCK_V2_FEATURE}"), &["petra", "walter"]);
    b.commit_org(41, "set_features", "memory quests", &["petra", "walter"]);
    b.commit_applied(42, &["walter", "dora"]);
    assert_eq!(b.v2_from, Some(2));
    let head = verify_chain(&b.blocks).expect("verifies");
    assert_eq!(head.block_v2_from, Some(2));
    assert_eq!(head.hash, v2_id(&b, &b.blocks[5]));
}

// --- S5: the keystone -----------------------------------------------------

/// Every block of `n` sealed three times by different 2-of-3 subsets, each
/// seat first adopting its own seal, then receiving the other two.
/// Returns the fork decisions summed over the seats and their heads.
fn three_seals_per_block(b0: &Builder, n: u64) -> (u64, Vec<String>) {
    let mut b = b0.clone();
    let mut seats: Vec<crate::State> = SEATS3.iter().map(|m| chain_peer_3(m, b0)).collect();
    let mut seq = 100u64;
    for i in 0..n {
        let h = u64::try_from(b.blocks.len()).expect("small");
        let change = ChainChange::Applied {
            proposal_id: 200 + i,
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "id": 200 + i }),
        };
        let subsets = [["petra", "walter"], ["petra", "dora"], ["walter", "dora"]];
        let seals: Vec<ChainBlock> = subsets.iter().map(|s| b.seal(h, change.clone(), s)).collect();
        for round in 0..3 {
            for (k, seat) in seats.iter_mut().enumerate() {
                let j = (k + round) % 3;
                seq += 1;
                wire(seat, subsets[j][0], seq, WorkspaceEvent::Committed(seals[j].clone()));
            }
        }
        let tip = seats[0].chain.blocks.last().expect("tip").clone();
        b.push(tip);
    }
    let forks = seats.iter().map(|s| s.chain.fork_decisions).sum();
    (forks, seats.iter().map(|s| s.chain_sync().head_hash).collect())
}

/// S5 keystone: after the activation one decision sealed by different
/// subsets on different seats is one block - zero fork decisions, one head.
#[test]
fn different_signer_subsets_never_fork_after_the_activation() {
    let (forks, heads) = three_seals_per_block(&enabled(2), 6);
    assert_eq!(forks, 0);
    assert!(heads.iter().all(|h| *h == heads[0]), "{heads:?}");
}

/// Counter-test: the same run before the activation decides variants.
#[test]
fn different_signer_subsets_fork_before_the_activation() {
    let (forks, _) = three_seals_per_block(&plain(2), 6);
    assert!(forks > 0, "v1 variants are fork decisions");
}

// --- review L1: one layout per block in a wiki export ---------------------

const PAGE_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+one\n";

fn page_tree() -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([("a.md".to_string(), "one\n".to_string())])
}

fn wiki_change(id: u64) -> ChainChange {
    ChainChange::Applied { proposal_id: id, surface: Surface::Memory, payload: serde_json::json!({ "op": "wiki_patch", "value": PAGE_A }) }
}

fn export_of(blocks: &[ChainBlock]) -> String {
    serde_json::to_string(&crate::wiki_export::bundle_from_chain(blocks).expect("genesis")).expect("json")
}

/// An export of an activated republic verifies, and its patch must carry
/// m signatures of ONE layout: a v2 and a v3 signature do not add up.
#[test]
fn a_wiki_export_counts_one_layout_per_block() {
    let mut b = enabled(2);
    let page = b.seal(2, wiki_change(30), &["petra", "walter"]);
    b.push(page);
    verify_wiki_export(&export_of(&b.blocks), &page_tree()).expect("an honest export verifies");
    let mut mixed = b.blocks.clone();
    let v2 = approval_bytes(&b.republic_id, 2, &wiki_change(30));
    mixed[2].sigs = vec![
        mixed[2].sigs.iter().find(|a| a.member == "petra").expect("petra").clone(),
        RosterAttestation { member: "walter".to_string(), sig: molt_storage::identity_sign(b.key("walter"), &v2) },
    ];
    let err = verify_wiki_export(&export_of(&mixed), &page_tree()).expect_err("one v2 and one v3 signature");
    assert!(err.contains("valid approvals"), "{err}");
}

// --- review M3: an older device does not rejoin an activated republic ---

fn recover_request(coord: &mut crate::State, b: &Builder, caps: Vec<String>) {
    let kp_hex = kp_hex_for(b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", &kp_hex, &b.republic_id, "", &[]);
    coord
        .cmd_net_recover_requested(
            "dora".to_string(),
            b.pk("dora"),
            kp_hex,
            "t".to_string(),
            proof,
            String::new(),
            Vec::new(),
            String::new(),
            String::new(),
            String::new(),
            (false, 0, caps),
            None,
        )
        .expect("ack");
}

#[test]
fn a_device_without_block_v2_is_refused_a_restore() {
    let b = enabled(3);
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    coord.recovery.tickets.insert("t".to_string(), "dora".to_string());
    recover_request(&mut coord, &b, Vec::new());
    assert_eq!(coord.session.notice, "recover-refused:dora:dora's device runs an older build");
    assert!(coord.recovery.tickets.contains_key("t"), "the ticket stays live");
    coord.session.notice.clear();
    recover_request(&mut coord, &b, vec![molt_core::chain::BLOCK_V2_CAP.to_string()]);
    assert!(!coord.session.notice.starts_with("recover-refused"), "{}", coord.session.notice);
}

/// Counter-test: before the activation the same device is not refused.
#[test]
fn before_the_activation_an_older_device_may_restore() {
    let b = plain(3);
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    coord.recovery.tickets.insert("t".to_string(), "dora".to_string());
    recover_request(&mut coord, &b, Vec::new());
    assert!(!coord.session.notice.starts_with("recover-refused"), "{}", coord.session.notice);
}

/// Review, the adversarial order: k valid seals at one height for other
/// cards, released in DESCENDING identity order. Each one swaps the tip
/// and re-signs every standing card; it ends on the lowest identity, the
/// cost bounded by k x open cards.
#[test]
fn seals_released_in_descending_identity_order_converge() {
    let b = enabled(2);
    let mut petra = genesis_seat("petra", &b, b.blocks.clone());
    let open: Vec<u64> = (0..5)
        .map(|i| propose(&mut petra, Surface::Memory, serde_json::json!({ "op": "add_note", "title": format!("o{i}") })))
        .collect();
    let mut seals: Vec<ChainBlock> = (0..6u64)
        .map(|i| {
            let change = ChainChange::Applied { proposal_id: 700 + i, surface: Surface::Memory, payload: serde_json::json!({ "op": "add_note", "id": i }) };
            b.seal(2, change, &["walter", "dora"])
        })
        .collect();
    seals.sort_by_key(|s| std::cmp::Reverse(v2_id(&b, s)));
    RECORDED_CHAIN.with(|r| r.borrow_mut().clear());
    for (i, s) in seals.iter().enumerate() {
        wire(&mut petra, "dora", 100 + u64::try_from(i).expect("small"), WorkspaceEvent::Committed(s.clone()));
    }
    let lowest = seals.last().expect("seals");
    assert_eq!(petra.chain.head.as_ref().expect("head").hash, v2_id(&b, lowest), "the lowest identity holds");
    assert_eq!(petra.chain.fork_decisions, 5, "one swap per later seal");
    let resigns = RECORDED_CHAIN.with(|r| r.borrow().iter().filter(|(k, h)| *k == "approved" && *h == 3).count());
    assert_eq!(resigns, seals.len() * open.len(), "every head move re-signs every standing card once");
    for id in &open {
        assert!(petra.own_signature_stands(*id), "{id} stands on the final tip");
    }
}
