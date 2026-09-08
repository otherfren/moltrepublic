// SPDX-License-Identifier: GPL-3.0-or-later

//! Tests for [`super::sync`]: catch-up buffering and drain, the served
//! suffix, the anchor bootstrap and the tie-break.

use super::test_support::*;
use super::*;
use molt_core::{ChainChange, Surface};
use serde_json::json;

/// C3: one requester is served a catch-up at most once per debounce,
/// and never for a height above the head.
#[test]
fn a_catch_up_request_is_served_once_per_debounce() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_applied(1, &["petra", "dora"]);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    walter.presence.clock_override = Some(1_000);
    let before = walter.next_seq;
    wire(&mut walter, "petra", 1, WorkspaceEvent::ChainRequest { from_height: 0, known: vec![] });
    let served = walter.next_seq;
    assert!(served > before, "the first request is served");
    wire(&mut walter, "petra", 2, WorkspaceEvent::ChainRequest { from_height: 0, known: vec![] });
    assert_eq!(walter.next_seq, served, "a repeat inside the debounce serves nothing");
    wire(&mut walter, "petra", 3, WorkspaceEvent::ChainRequest { from_height: 99, known: vec![] });
    assert_eq!(walter.next_seq, served, "nothing above the head is served");
    walter.presence.clock_override = Some(1_000 + crate::net::CHAIN_SERVE_DEBOUNCE_SECS);
    wire(&mut walter, "petra", 4, WorkspaceEvent::ChainRequest { from_height: 0, known: vec![] });
    assert!(walter.next_seq > served, "after the debounce it is served again");
}

/// C6: a headless node adopts only ITS republic's genesis — a valid
/// genesis is trivially forgeable.
#[test]
fn a_headless_node_refuses_another_republics_genesis() {
    let b = Builder::new(&["petra", "walter"], 2);
    let other = Builder::new(&["mallory", "walter"], 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    walter.chain.blocks.clear();
    walter.chain.head = None;
    walter.chain.walk = None;
    walter.receive_block(other.blocks[0].clone());
    assert!(walter.chain.head.is_none(), "a foreign genesis is not adopted");
    walter.receive_block(b.blocks[0].clone());
    assert!(walter.chain.head.is_some(), "the own genesis is");
}

/// KEYSTONE for `tie_break` (previously untested): two members seal
/// competing blocks at the same height; the lower hash wins the tip.
/// A record MATERIALIZED from the displaced block must VANISH with it
/// (review 2026-08-16: flipping it to Proposed minted a permanent,
/// unowned open card — unwithdrawable, re-gossiped forever, and it
/// blocked auto-checkpoints on that holder).
#[test]
fn tie_break_drops_a_materialized_card_with_its_displaced_block() {
    let b = Builder::new(&["petra", "walter"], 2);
    let genesis = b.blocks.clone();
    let block_a = b.seal(
        1,
        ChainChange::Applied {
            proposal_id: 7,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "title": "a" }),
        },
        &["petra", "walter"],
    );
    let block_b = b.seal(
        1,
        ChainChange::Applied {
            proposal_id: 9,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "title": "b" }),
        },
        &["petra", "walter"],
    );
    let rid = b.republic_id.clone();
    let hash = |blk: &ChainBlock| molt_storage::content_hash(&block_link_bytes(&rid, blk));
    let (winner, loser) = if hash(&block_a) < hash(&block_b) {
        (block_a, block_b)
    } else {
        (block_b, block_a)
    };
    let (loser_id, winner_id) = match (&loser.change, &winner.change) {
        (
            ChainChange::Applied { proposal_id: l, .. },
            ChainChange::Applied { proposal_id: w, .. },
        ) => (*l, *w),
        _ => unreachable!("both are Applied"),
    };

    // the peer adopts the LOSER first (its card is materialized from
    // the block — this holder never saw the gossip), then the winner
    // arrives and takes the tip
    let mut peer = crate::tests::plain_state();
    peer.replica = Some(crate::ReplicaState {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        roster: vec!["petra".to_string(), "walter".to_string()],
        rule_m: 2,
        identities: Vec::new(),
        agenda: String::new(),
        features: None,
        republic_id: rid.clone(),
        founded_ts: 0,
    });
    peer.adopt_chain(genesis);
    peer.receive_block(loser);
    assert_eq!(
        peer.proposals.get(&loser_id).map(|p| p.state),
        Some(ProposalState::Applied),
        "the loser's card is materialized while its block stands"
    );

    peer.receive_block(winner);
    assert_eq!(peer.chain.blocks.len(), 2);
    let tip = peer.chain.blocks.last().expect("tip");
    assert!(
        matches!(&tip.change, ChainChange::Applied { proposal_id, .. } if *proposal_id == winner_id),
        "the lower hash holds the tip"
    );
    assert!(
        !peer.proposals.contains_key(&loser_id),
        "the materialized card vanished with its displaced block - no phantom open card"
    );
    assert_eq!(
        peer.proposals.get(&winner_id).map(|p| p.state),
        Some(ProposalState::Applied),
        "the winner's card stands, chain-proven"
    );
}

#[test]
fn a_peer_adopts_a_broadcast_block_and_converges() {
    let b = Builder::new(&["petra", "walter"], 2);
    let genesis = b.blocks.clone();
    // a block committed elsewhere: an Applied change signed by both members
    let change = ChainChange::Applied {
        proposal_id: 1,
        surface: Surface::Memory,
        payload: json!({ "op": "add_note", "title": "minutes" }),
    };
    let block = b.seal(1, change, &["petra", "walter"]);

    // walter holds only the genesis, then the block arrives over the mesh
    let mut peer = crate::tests::plain_state();
    peer.replica = Some(crate::ReplicaState {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        roster: vec!["petra".to_string(), "walter".to_string()],
        rule_m: 2,
        identities: Vec::new(), // adopt_chain fills these from the verified head
        agenda: "play chess".to_string(),
        features: None,
        republic_id: b.republic_id.clone(),
        founded_ts: 0,
    });
    peer.adopt_chain(genesis);
    assert!(peer.is_chain_governed());
    assert_eq!(peer.chain.head.as_ref().expect("head").height, 0);

    peer.receive_block(block);
    assert_eq!(peer.chain.blocks.len(), 2, "the peer adopted the broadcast block");
    assert_eq!(peer.chain.head.as_ref().expect("head").height, 1);
    let applied = peer
        .chain.applied
        .get(&Surface::Memory)
        .expect("memory projection");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].0, Some(1), "the projection keeps the proposal id");
    assert_eq!(applied[0].1["title"], json!("minutes"));

    // an invalid block (tampered payload, sigs no longer match) is rejected
    let mut forged = b.seal(
        2,
        ChainChange::Applied {
            proposal_id: 2,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "title": "real" }),
        },
        &["petra", "walter"],
    );
    forged.prev = peer.chain.head.as_ref().expect("head").hash.clone();
    if let ChainChange::Applied { payload, .. } = &mut forged.change {
        *payload = json!({ "op": "add_note", "title": "forged" });
    }
    peer.receive_block(forged);
    assert_eq!(peer.chain.blocks.len(), 2, "a tampered block is hard-rejected");
}

/// A block arriving ahead of our head is buffered, then applied once the
/// gap fills — catch-up converges regardless of arrival order.
#[test]
fn out_of_order_blocks_buffer_and_converge() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let genesis = b.blocks.clone();
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    let block1 = b.blocks[1].clone();
    let block2 = b.blocks[2].clone();

    let mut peer = chain_peer("walter", &b, genesis);
    // the height-2 block arrives first — a gap, so it is buffered
    peer.receive_block(block2);
    assert_eq!(
        peer.chain.head.as_ref().expect("head").height,
        0,
        "a gap block is buffered, not applied"
    );
    assert_eq!(peer.chain.pending_blocks.len(), 1);
    // the height-1 block fills the gap; the buffered height-2 drains behind it
    peer.receive_block(block1);
    assert_eq!(peer.chain.head.as_ref().expect("head").height, 2);
    assert!(peer.chain.pending_blocks.is_empty(), "the buffer drained");
}

/// One survivor holding the full chain re-serves a lagging member the whole
/// missing suffix — the resilience property (any survivor suffices), and the
/// suffix applies even delivered out of order.
#[test]
fn a_survivor_serves_a_lagging_member_the_full_suffix() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let genesis = b.blocks.clone();
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    let full = b.blocks.clone();

    let mut peer = chain_peer("walter", &b, genesis);
    assert_eq!(peer.chain.head.as_ref().expect("head").height, 0);

    // the survivor serves every block from the peer's head+1 (=1) onward,
    // straight out of its own chain — exactly what serve_chain_from does
    let served: Vec<ChainBlock> = full.iter().filter(|bl| bl.height >= 1).cloned().collect();
    assert_eq!(served.len(), 2, "survivor serves b1 + b2 from its chain");
    for bl in served.into_iter().rev() {
        peer.receive_block(bl); // delivered newest-first to exercise buffering
    }
    assert_eq!(
        peer.chain.head.as_ref().expect("head").height,
        2,
        "the lagging member caught up to the survivor"
    );
    assert!(peer.chain.pending_blocks.is_empty());
}

/// Split a bootstrap offer back into the shape `verify_served` takes.
fn split_bootstrap(
    events: &[WorkspaceEvent],
) -> (Option<molt_core::CheckpointState>, Vec<ChainBlock>) {
    let mut blob = None;
    let mut blocks = Vec::new();
    for ev in events {
        match ev {
            WorkspaceEvent::CheckpointServed { blob: b } => blob = Some(b.clone()),
            WorkspaceEvent::Committed(bl) => blocks.push(bl.clone()),
            other => panic!("a bootstrap offer carries nothing else: {other:?}"),
        }
    }
    (blob, blocks)
}

/// **The anchor is the smallest prefix that verifies standalone.**
///
/// A rejoiner cannot be handed the whole chain — one `set_image` block
/// exceeds the gift-wrap cap — and cannot be handed a bare head, because
/// `verify_chain` is all-or-nothing from the anchor and a headless node
/// drops every block served to it. So it is handed the ANCHOR, and asks
/// for the rest over the ordinary catch-up once it has a workspace.
///
/// The COUNT is asserted, not merely the verification: an implementation
/// that served the whole chain would verify perfectly well and quietly
/// reintroduce the size cliff this exists to avoid.
#[test]
fn the_served_anchor_is_the_smallest_prefix_that_verifies() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);

    // a FULL holder offers the genesis, alone
    let full = chain_peer("walter", &b, b.blocks.clone());
    let offer = full.anchor_bootstrap();
    let (blob, blocks) = split_bootstrap(&offer);
    assert!(blob.is_none(), "a full holder has no blob to offer");
    assert_eq!(blocks.len(), 1, "the genesis and nothing else - not the chain");
    assert_eq!(blocks[0].height, 0);
    let (head, sealed) = verify_served(blob.as_ref(), &blocks, Some(&b.republic_id))
        .expect("the genesis verifies standalone");
    assert_eq!(head.height, 0, "a length-1 chain is a valid chain");
    assert_eq!(sealed.republic_id, b.republic_id);

    // …and a PRUNED holder offers its blob plus its anchor block, because
    // by then no node anywhere still holds a genesis
    let blob_at_2 = checkpoint_state(&b.blocks, 2).expect("state@2");
    let anchor = b.seal(
        3,
        ChainChange::Checkpoint {
            upto: 2,
            state_hash: checkpoint_state_hash(&blob_at_2),
        },
        &["petra", "walter"],
    );
    b.push(anchor.clone());
    let mut pruned = chain_peer("walter", &b, b.blocks[..3].to_vec());
    pruned.receive_block(anchor);
    assert!(pruned.chain.checkpoint_blob.is_some(), "the holder pruned");

    let offer = pruned.anchor_bootstrap();
    let (blob, blocks) = split_bootstrap(&offer);
    assert!(blob.is_some(), "a pruned holder's blob IS its trust root");
    assert_eq!(blocks.len(), 1, "the anchor block alone, not the suffix");
    assert_eq!(blocks[0].height, 3);
    let (head, _) = verify_served(blob.as_ref(), &blocks, Some(&b.republic_id))
        .expect("blob + anchor verify standalone under the suffix rules");
    assert_eq!(head.height, 3, "the rejoiner starts at the cut, not at zero");

    // …and broadcasting it must not disturb the SERVER: a 445 reaches
    // every member, so the offer travels back through this node's own
    // apply path (and through every survivor's) as a duplicate
    let before = (pruned.chain.blocks.clone(), pruned.chain.head.clone());
    pruned.serve_chain_anchor();
    assert_eq!(pruned.chain.blocks, before.0, "serving must not move the server's chain");
    assert_eq!(
        pruned.chain.head.as_ref().map(|h| h.height),
        before.1.as_ref().map(|h| h.height),
        "nor its head"
    );
}

/// L3: the future-block buffer holds only heights the drain could ever
/// reach and stays size-capped — an unverified far-future block was
/// buffered forever (and one such block froze auto-compaction).
#[test]
fn a_far_future_block_is_refused_not_buffered() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let junk = |height: u64| ChainBlock {
        height,
        prev: "00".to_string(),
        change: ChainChange::Applied {
            proposal_id: height,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note" }),
        },
        sigs: Vec::new(),
    };
    walter.receive_block(junk(u64::MAX / 2));
    assert!(
        walter.chain.pending_blocks.is_empty(),
        "a block far past the head never buffers"
    );
    walter.receive_block(junk(3));
    assert_eq!(walter.chain.pending_blocks.len(), 1, "a near gap buffers for the drain");
}

/// **The catch-up is linear.** Draining a buffered suffix used to verify
/// the whole chain from the anchor for every block, and TWICE per block
/// (a probe clone, then the append) — `2nN + m·N(N+1)` signature checks,
/// all inside one uninterruptible actor turn. A node catching up then
/// looked exactly like a dead one to its peers, which is what the
/// delivery guarantee escalates on.
///
/// Counted, not timed: the assertion is the point of the whole change.
#[test]
fn catching_up_verifies_each_block_once() {
    const N: usize = 40;
    let b = grown_chain(N + 1);
    let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());

    // reverse order, so every block buffers and the whole suffix drains
    // in ONE turn — the shape a real catch-up has
    VERIFY_STEPS.with(|c| c.set(0));
    CHAIN_PERSISTS.with(|c| c.set(0));
    for block in b.blocks[1..].iter().rev() {
        peer.receive_block(block.clone());
    }
    let steps = VERIFY_STEPS.with(std::cell::Cell::get);
    let writes = CHAIN_PERSISTS.with(std::cell::Cell::get);

    assert_eq!(
        peer.chain.head.as_ref().expect("head").height,
        u64::try_from(N).expect("small chain"),
        "the whole suffix drained"
    );
    assert_eq!(
        steps, N,
        "each block is verified exactly once - a re-walk per block would \
         cost {} here, and 7M at N=1000",
        N * (N + 1)
    );
    assert_eq!(
        writes, 1,
        "the drained batch is written ONCE - the write blocks on the \
         storage writer's ack, so {N} of them would sit inside one turn"
    );
}

/// Batching the write must not turn "once per block" into "never".
///
/// Every path that accepts a block ends in exactly one
/// `persist_chain_now`; this is the guard for a future third caller that
/// forgets, which would leave accepted blocks unwritten and silently
/// re-fetched on every restart.
#[test]
fn an_accepted_block_is_written_once_and_a_refused_one_not_at_all() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let genesis = b.blocks.clone();
    b.commit_applied(1, &["petra", "walter"]);
    let mut peer = chain_peer("walter", &b, genesis);

    CHAIN_PERSISTS.with(|c| c.set(0));
    peer.receive_block(b.blocks[1].clone());
    assert_eq!(peer.chain.head.as_ref().expect("head").height, 1);
    assert_eq!(
        CHAIN_PERSISTS.with(std::cell::Cell::get),
        1,
        "an accepted block is written"
    );

    let refused = b.seal(
        2,
        ChainChange::Applied {
            proposal_id: 2,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 2 }),
        },
        &["petra"],
    );
    CHAIN_PERSISTS.with(|c| c.set(0));
    peer.receive_block(refused);
    assert_eq!(peer.chain.head.as_ref().expect("head").height, 1);
    assert_eq!(
        CHAIN_PERSISTS.with(std::cell::Cell::get),
        0,
        "a refused block writes nothing"
    );
}

/// A rejoiner that lost everything (no chain, no head) bootstraps from the
/// genesis a survivor serves and then catches up the whole chain — even when
/// later blocks arrive before the genesis (they buffer until it lands). The
/// state-recovery core of Phase 4.
#[test]
fn a_headless_rejoiner_bootstraps_from_a_served_genesis() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let genesis_block = b.blocks[0].clone();
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    let block1 = b.blocks[1].clone();
    let block2 = b.blocks[2].clone();

    let mut rejoiner = crate::tests::plain_state();
    assert!(!rejoiner.is_chain_governed());

    // a block arrives before the genesis — buffered, still headless
    rejoiner.receive_block(block2);
    assert!(!rejoiner.is_chain_governed());
    assert_eq!(rejoiner.chain.pending_blocks.len(), 1);

    // the survivor serves the genesis — adopt it as the root
    rejoiner.receive_block(genesis_block);
    assert!(rejoiner.is_chain_governed(), "adopted the served genesis");
    assert_eq!(rejoiner.chain.head.as_ref().expect("head").height, 0);

    // the middle block fills the gap; the buffered tail drains behind it
    rejoiner.receive_block(block1);
    assert_eq!(
        rejoiner.chain.head.as_ref().expect("head").height,
        2,
        "the rejoiner caught up the full chain from genesis"
    );
    assert!(rejoiner.chain.pending_blocks.is_empty());
}

/// **§4.9.8: a blob that does not fit a frame is not served.** An
/// over-budget `WorkspaceEvent` is a permanent publish stall - the holder
/// would write nothing more, across restarts - so the honest answer to a
/// peer's request is silence, not a self-inflicted outbox freeze.
#[test]
fn an_oversized_checkpoint_blob_is_never_served() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
    assert!(
        crate::State::served_blob_fits(&blob),
        "an ordinary blob is servable"
    );

    // a blob whose applied history alone outgrows one frame
    let mut fat = blob.clone();
    fat.applied = vec![(
        Surface::Memory,
        (0..2_000u64)
            .map(|i| (i, serde_json::json!({ "op": "add_note", "text": "x".repeat(200) })))
            .collect(),
    )];
    assert!(
        !crate::State::served_blob_fits(&fat),
        "a blob past the frame budget is refused before it can stall the outbox"
    );
}

/// A2.1 (mcp_agent_friction_fixes.md): a block from another branch - one
/// whose `prev` is not our block below it, or a contender below the tip -
/// flags its SENDER as diverged since the lowest height the two chains can
/// still share, on every surface (chain state, read_chain, status, the
/// session notice). A block from that peer that extends our chain clears
/// the flag again.
#[test]
fn a_block_from_another_branch_flags_its_sender() {
    let base = Builder::new(&["petra", "walter"], 2);
    let mut a = base.clone();
    a.commit_applied(1, &["petra", "walter"]);
    a.commit_applied(2, &["petra", "walter"]);
    let mut b = base.clone();
    b.commit_applied(9, &["petra", "walter"]);
    b.commit_applied(10, &["petra", "walter"]);
    let mut walter = chain_peer("walter", &a, a.blocks.clone());

    // b's block at OUR tip height links to b's height 1, not ours
    walter.receive_block_from("petra", b.blocks[2].clone());
    let since = |st: &crate::State| st.chain.diverged.get("petra").map(|d| d.since_height);
    assert_eq!(since(&walter), Some(1), "the branches part at height 1");
    assert_eq!(walter.chain.blocks.len(), 3, "nothing foreign was adopted");
    let molt_core::Reply::Chain { diverged, .. } = walter.cmd_read_chain().expect("read") else {
        panic!("read_chain answers Reply::Chain");
    };
    assert_eq!((diverged[0].peer.as_str(), diverged[0].since_height), ("petra", 1));
    assert_eq!(walter.status().chain_diverged.len(), 1);
    assert_eq!(walter.session.notice, "chain-diverged:petra:1");

    // b's height-1 block, below our tip: the same signal, the height stays
    walter.receive_block_from("petra", b.blocks[1].clone());
    assert_eq!(since(&walter), Some(1));
    assert_eq!(walter.chain.blocks.len(), 3);

    // a block that EXTENDS our chain from petra clears the flag
    a.commit_applied(3, &["petra", "walter"]);
    walter.receive_block_from("petra", a.blocks[3].clone());
    assert_eq!(walter.chain.blocks.len(), 4);
    assert!(walter.chain.diverged.is_empty(), "petra is back on our branch");
    assert!(walter.status().chain_diverged.is_empty());
}

/// The ordinary race - two seals at the tip on the SAME prev - is what the
/// tie-break resolves; it never reads as divergence.
#[test]
fn a_tip_tie_on_a_shared_prev_is_not_divergence() {
    let base = Builder::new(&["petra", "walter"], 2);
    let mut a = base.clone();
    a.commit_applied(1, &["petra", "walter"]);
    let mut b = base.clone();
    b.commit_applied(9, &["petra", "walter"]);
    let mut walter = chain_peer("walter", &a, a.blocks.clone());
    walter.receive_block_from("petra", b.blocks[1].clone());
    assert!(walter.chain.diverged.is_empty());
    assert_eq!(walter.chain.blocks.len(), 2, "one of the two tip blocks stands");
}

/// A2.3 (`docs_archive/chain/chain_reorg.md` R1-R4): two branches fork at height 1
/// and both grow to 3. The holder of the LARGER-hash branch receives the
/// other branch block by block, in any order, and re-bases onto it: the
/// smaller hash at the first divergent height wins, displaced proposals
/// return to the vote, the flag clears. The holder of the smaller branch
/// keeps its chain when the larger one arrives.
#[test]
fn a_deeper_fork_reorgs_onto_the_smaller_branch() {
    let base = Builder::new(&["petra", "walter"], 2);
    let mut a = base.clone();
    for id in [1, 2, 3] {
        a.commit_applied(id, &["petra", "walter"]);
    }
    let mut b = base.clone();
    for id in [11, 12, 13] {
        b.commit_applied(id, &["petra", "walter"]);
    }
    let rid = &base.republic_id;
    // which branch wins at the fork height 1 (R1)
    let (win, lose) = if block_hash(rid, &a.blocks[1]) < block_hash(rid, &b.blocks[1]) {
        (a, b)
    } else {
        (b, a)
    };
    let applied_ids = |st: &crate::State| -> Vec<u64> {
        st.chain
            .blocks
            .iter()
            .filter_map(|blk| match &blk.change {
                ChainChange::Applied { proposal_id, .. } => Some(*proposal_id),
                _ => None,
            })
            .collect()
    };

    // the loser holds its own branch (3 blocks + genesis); the winner's
    // blocks arrive newest first - the order a live broadcast produces
    let mut loser = chain_peer("walter", &lose, lose.blocks.clone());
    let lost_ids = applied_ids(&loser);
    // two of the displaced cards had a deliberation here, one is a record
    // materialized from the block alone (`by` empty)
    for id in &lost_ids[..2] {
        loser.proposals.get_mut(id).expect("card").by = "walter".to_string();
    }
    for blk in [&win.blocks[3], &win.blocks[2], &win.blocks[1]] {
        loser.receive_block_from("petra", blk.clone());
    }
    assert_eq!(loser.chain.blocks, win.blocks, "re-based onto the smaller branch");
    assert_eq!(applied_ids(&loser), applied_ids(&chain_peer("walter", &win, win.blocks.clone())));
    for id in &lost_ids[..2] {
        assert_eq!(
            loser.proposals.get(id).map(|p| p.state),
            Some(ProposalState::Proposed),
            "displaced proposal {id} returns to the vote"
        );
    }
    assert!(!loser.proposals.contains_key(&lost_ids[2]), "a materialized record has no vote to return to");
    assert!(loser.chain.diverged.is_empty(), "the peer is on our branch now");
    assert!(loser.chain.fork_candidates.is_empty());

    // the winner sees the loser's branch and does not move
    let mut winner = chain_peer("petra", &win, win.blocks.clone());
    for blk in [&lose.blocks[1], &lose.blocks[2], &lose.blocks[3]] {
        winner.receive_block_from("walter", blk.clone());
    }
    assert_eq!(winner.chain.blocks, win.blocks, "the smaller branch stands");
}

/// R2: a pruned holder cannot verify below its anchor, so a fork below the
/// cut is left alone - flagged, never adopted.
#[test]
fn a_reorg_never_crosses_the_cut() {
    let base = Builder::new(&["petra", "walter"], 2);
    let mut b = base.clone();
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    let blob_at_2 = checkpoint_state(&b.blocks, 2).expect("state@2");
    let anchor = b.seal(
        3,
        ChainChange::Checkpoint { upto: 2, state_hash: checkpoint_state_hash(&blob_at_2) },
        &["petra", "walter"],
    );
    b.push(anchor.clone());
    let mut pruned = chain_peer("walter", &b, b.blocks[..3].to_vec());
    pruned.receive_block(anchor);
    assert!(pruned.chain.checkpoint_blob.is_some(), "the holder pruned");
    let before = pruned.chain.blocks.clone();

    // another branch that forked at height 2, below the cut
    let mut c = base.clone();
    c.commit_applied(1, &["petra", "walter"]);
    c.commit_applied(22, &["petra", "walter"]);
    c.commit_applied(23, &["petra", "walter"]);
    c.commit_applied(24, &["petra", "walter"]);
    for blk in [&c.blocks[4], &c.blocks[3], &c.blocks[2]] {
        pruned.receive_block_from("petra", blk.clone());
    }
    assert_eq!(pruned.chain.blocks, before, "nothing below the cut is re-based");
    assert!(pruned.chain.checkpoint_blob.is_some());
}

/// R3: a winning branch with one block below the threshold is not a branch
/// at all - the candidate is verified as a whole before anything moves.
#[test]
fn an_unverifiable_candidate_is_dropped() {
    let base = Builder::new(&["petra", "walter"], 2);
    let rid = &base.republic_id;
    let mut a = base.clone();
    for id in [1, 2, 3] {
        a.commit_applied(id, &["petra", "walter"]);
    }
    // a branch whose block 1 wins the fork by hash - found by trying ids
    let mut w = None;
    for id in [11u64, 21, 31, 41, 51, 61, 71, 81] {
        let mut cand = base.clone();
        cand.commit_applied(id, &["petra", "walter"]);
        if block_hash(rid, &cand.blocks[1]) < block_hash(rid, &a.blocks[1]) {
            w = Some(cand);
            break;
        }
    }
    let mut w = w.expect("one of eight ids hashes below the other branch");
    // …with a forged block 2 (one signature, m = 2) that block 3 links to
    let forged = w.seal(
        2,
        ChainChange::Applied {
            proposal_id: 12,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 12 }),
        },
        &["petra"],
    );
    w.push(forged);
    w.commit_applied(13, &["petra", "walter"]);
    let mut loser = chain_peer("walter", &a, a.blocks.clone());
    for blk in [&w.blocks[3], &w.blocks[2], &w.blocks[1]] {
        loser.receive_block_from("petra", blk.clone());
    }
    assert_eq!(loser.chain.blocks, a.blocks, "an unverifiable branch never adopts");
    assert!(loser.chain.fork_candidates.is_empty(), "and is dropped, not kept");
}

/// R5: a fork-aware request names what the requester holds; the server
/// serves from one above the highest sample it shares, not from the
/// requester's estimate - and a plain request still serves from
/// `from_height`.
#[test]
fn a_chain_request_names_the_fork_point() {
    let base = Builder::new(&["petra", "walter"], 2);
    let rid = base.republic_id.clone();
    let mut a = base.clone();
    for id in [1, 2, 3, 4] {
        a.commit_applied(id, &["petra", "walter"]);
    }
    let mut b = base.clone();
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    b.commit_applied(33, &["petra", "walter"]);
    b.commit_applied(34, &["petra", "walter"]);
    let server = chain_peer("walter", &a, a.blocks.clone());
    let requester = chain_peer("petra", &b, b.blocks.clone());
    let known = requester.known_heads();
    assert_eq!(
        known.iter().map(|k| k.height).collect::<Vec<_>>(),
        vec![4, 3, 2, 0],
        "head, then back geometrically"
    );
    assert_eq!(known[2].hash, block_hash(&rid, &a.blocks[2]), "the shared prefix matches");
    assert_eq!(server.serve_from_for(5, &known), 3, "serve from the block above the last shared one");
    assert_eq!(server.serve_from_for(5, &[]), 5, "a plain request keeps its estimate");
}

/// A5 KEYSTONE (round 3, D9/R3): a seat the republic ran on without reads
/// its DISTANCE, not a divergence - the peers' blocks arrive and are
/// refused, and `peers_ahead` says how far behind this node is.
#[test]
fn a_seat_the_republic_ran_past_reads_the_distance() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_applied(1, &["petra", "dora"]);
    // this seat stops here; the others run on
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    let head_here = walter.chain.head.as_ref().expect("a head").height;
    b.commit_applied(2, &["petra", "dora"]);
    b.commit_applied(3, &["petra", "dora"]);
    assert_eq!(walter.chain_lag().peers_ahead, 0, "nothing heard yet, nothing claimed");
    for block in &b.blocks[2..] {
        wire(&mut walter, "petra", 10 + block.height, WorkspaceEvent::Committed(block.clone()));
    }
    let lag = walter.chain_lag();
    assert_eq!(
        walter.chain.head.as_ref().expect("a head").height - head_here,
        2,
        "these blocks fit - the seat catches up and the lag is the point of the FIELD, not the test transport"
    );
    assert_eq!(lag.peers_ahead, 0, "having applied them, it is level again");

    // now a block it CANNOT apply (a gap): the claim still counts
    b.commit_applied(4, &["petra", "dora"]);
    b.commit_applied(5, &["petra", "dora"]);
    let far = b.blocks.last().expect("a block").clone();
    wire(&mut walter, "petra", 40, WorkspaceEvent::Committed(far));
    assert!(
        walter.chain_lag().peers_ahead >= 2,
        "a peer two blocks up is two blocks ahead: {:?}",
        walter.chain_lag()
    );
    assert!(
        walter.chain.diverged.is_empty(),
        "distance is not contradiction - `diverged` must stay empty"
    );
}

/// A seat never seen here, in a republic whose founding date this node
/// does not know (a recovery from an older coordinator): silent, with an
/// age of 0 = unknown - never "since the epoch".
#[test]
fn a_never_seen_seat_of_unknown_founding_reads_silent_with_no_age() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_applied(1, &["petra", "dora"]);
    let walter = chain_peer("walter", &b, b.blocks.clone());
    assert_eq!(walter.replica.as_ref().map(|r| r.founded_ts), Some(0), "the fixture knows no founding");
    let lag = walter.chain_lag();
    let petra = lag.silent.iter().find(|s| s.member == "petra").expect("never seen = silent");
    assert_eq!(petra.secs, 0, "unknown age reads 0, not the seconds since 1970");
    assert!(lag.silent.iter().all(|s| s.member != "walter"), "never the own seat");
}

/// A6 KEYSTONE: while the peers are ahead, this seat's vote would be
/// signed at a height the republic has left - so the vote is REFUSED here
/// instead of answering like a success (round 3: eleven approvals into a
/// void).
#[test]
fn a_vote_is_refused_while_the_peers_are_ahead() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    walter.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "petra");
    assert!(
        walter.cmd_approve(molt_core::ProposalId(7), None).is_ok(),
        "level with the republic, the vote counts"
    );
    walter.note_peer_height("petra", 3);
    let err = walter
        .cmd_decline(molt_core::ProposalId(7), None)
        .expect_err("a lagging vote is refused");
    assert!(
        matches!(err, molt_core::MoltError::ChainLagging { peers_ahead } if peers_ahead >= 2),
        "the refusal names the distance: {err:?}"
    );
}

/// A5: a seat that stopped co-signing shows up as SILENT (stale signer),
/// which is a different fact from a fork - and a seat that merely lost the
/// race to one seal is NOT silent: an m-of-n block keeps the signatures it
/// held, the late ones never land, so a signature this node verified
/// counts, sealed or not.
#[test]
fn a_seat_that_stopped_signing_is_reported_stale() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_applied(1, &["petra", "dora"]);
    let one = chain_peer("walter", &b, b.blocks.clone());
    assert!(
        one.stale_signers().is_empty(),
        "one missed block is a lost race, not silence: {:?}",
        one.stale_signers()
    );
    b.commit_applied(2, &["petra", "dora"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let stale = walter.stale_signers();
    assert!(
        stale.iter().any(|s| s.member == "walter" && s.last_signed_height == 0),
        "walter signed only the genesis: {stale:?}"
    );
    assert!(
        !stale.iter().any(|s| s.member == "petra"),
        "petra signed the head, she is not stale: {stale:?}"
    );
    walter.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "petra");
    walter
        .cmd_approve(molt_core::ProposalId(7), None)
        .expect("level with the republic, the vote counts");
    assert!(
        !walter.stale_signers().iter().any(|s| s.member == "walter"),
        "a verified signature counts before it seals: {:?}",
        walter.stale_signers()
    );
}
