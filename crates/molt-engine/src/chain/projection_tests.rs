// SPDX-License-Identifier: GPL-3.0-or-later

//! Tests for [`super::projection`]: adopting and folding a chain into
//! `State`, the effective pool / feature set, card settling and the
//! Chain-History read.

use super::test_support::*;
use super::*;
use molt_core::{ChainChange, MembershipOp, Surface};
use molt_storage::identity_sign;
use serde_json::json;

/// E1 residual: the mint counter clears every id the chain consumed.
#[test]
fn the_max_applied_proposal_id_reads_the_chain() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    assert_eq!(crate::State::max_applied_proposal_id(&b.blocks), None);
    b.commit_applied(7, &["petra", "walter"]);
    b.commit_applied(3, &["petra", "walter"]);
    assert_eq!(crate::State::max_applied_proposal_id(&b.blocks), Some(7));
}

/// A member that only holds the genesis receives a peer's broadcast commit
/// block, verifies + adopts it, and its persistent state converges (the
/// `receive_block` path that a non-committer follows).
/// WP1: the chain projection feeds the snapshot's parallel id track —
/// a committed `Applied` block's `proposal_id` reaches the read contract
/// positionally next to its payload.
/// N4b step 3 — the WORKING transport anchor is a chain projection.
///
/// A recovered seat's key changes; the roster's genesis anchor does not
/// (it is the immutable founding record). Every gift-wrap send must
/// resolve through the projection, because a sender reaching for the
/// obvious `identities[i].nostr_pk` would address a key the recovered
/// member no longer holds — SILENTLY, which is exactly why the plan
/// rejected "infer the anchor from live traffic".
#[test]
fn the_working_anchor_follows_a_restored_block_while_the_roster_does_not() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut st = crate::tests::plain_state();
    st.replica = Some(crate::ReplicaState {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        roster: vec!["petra".to_string(), "walter".to_string()],
        rule_m: 2,
        identities: Vec::new(),
        agenda: "play chess".to_string(),
        features: None,
        republic_id: b.republic_id.clone(),
        founded_ts: 0,
    });
    st.adopt_chain(b.blocks.clone());
    let founding = st
        .replica
        .as_ref()
        .and_then(|r| r.identities.iter().find(|i| i.member == "petra"))
        .map(|i| i.nostr_pk.clone())
        .expect("petra is anchored");
    assert_eq!(
        st.working_nostr_pk("petra"),
        founding,
        "before any recovery the projection IS the roster anchor"
    );

    // petra recovers with a FRESH transport key
    let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
    let restored = b.seal(
        1,
        ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "petra".to_string(),
            identity_pk: b.pk("petra"),
            nostr_pk: Some(fresh.clone()),
            relays: Vec::new(),
            consent: None,
        },
        &["petra", "walter"],
    );
    st.adopt_chain({
        let mut c = b.blocks.clone();
        c.push(restored);
        c
    });

    assert_eq!(
        st.working_nostr_pk("petra"),
        fresh,
        "after a Restored block the projection returns the NEW key"
    );
    assert_eq!(
        st.replica
            .as_ref()
            .and_then(|r| r.identities.iter().find(|i| i.member == "petra"))
            .map(|i| i.nostr_pk.clone())
            .expect("still anchored"),
        founding,
        "…while the roster keeps the immutable FOUNDING anchor"
    );
    // an unknown member resolves to nothing, never to somebody else's key
    assert_eq!(st.working_nostr_pk("nobody"), "");
}

/// **…and it survives the compaction that drops the block it came from.**
///
/// The `Restored` block is what re-anchors a seat, and a cut drops it.
/// The roster in the blob keeps the FOUNDING anchor by design
/// (`apply_membership` refuses to move a seat's identity key), so without
/// the summary carrying the working anchors explicitly, a compaction
/// makes every recovered member addressable ONLY at the key it no longer
/// holds — silently, which is the exact failure `ChainProjection::anchors`
/// documents itself as existing to prevent.
///
/// Reachable in the ordinary course: `AUTO_CHECKPOINT_MIN_LEN` is 32.
#[test]
fn a_compaction_keeps_the_working_anchor_of_a_recovered_seat() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
    b.commit_restored("petra", &fresh, &["petra", "walter"]);

    // cut ABOVE the Restored block, so the block itself is dropped
    let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
    assert_eq!(
        blob.anchors,
        vec![("petra".to_string(), fresh.clone())],
        "the summary must carry the anchors the dropped blocks established"
    );
    let cut = b.seal(
        2,
        ChainChange::Checkpoint {
            upto: 1,
            state_hash: checkpoint_state_hash(&blob),
        },
        &["petra", "walter"],
    );
    b.push(cut);

    // the pruned holder: blob + the suffix from the anchor block on
    let mut st = crate::tests::plain_state();
    st.replica = Some(crate::ReplicaState {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        roster: vec!["petra".to_string(), "walter".to_string()],
        rule_m: 2,
        identities: Vec::new(),
        agenda: "play chess".to_string(),
        features: None,
        republic_id: b.republic_id.clone(),
        founded_ts: 0,
    });
    st.set_checkpoint_blob(Some(blob));
    st.adopt_chain(b.blocks[2..].to_vec());

    assert_eq!(
        st.working_nostr_pk("petra"),
        fresh,
        "after the cut the seat is addressable only at the key it no longer holds"
    );
}

/// R3b — the relay LEDGER: every member's chain answers "which relays is
/// this seat on record as reaching". A founding member is covered by the
/// ratified genesis pool; a restored seat's threshold-signed declaration
/// overrides it — for EVERY member reading the same chain; and a
/// compaction cut must not forget a declaration (checkpoint-v6), because
/// split detection (R4) runs on exactly this data.
#[test]
fn the_ledger_reports_declared_relays_and_survives_a_cut() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
    let rid = b.republic_id.clone();
    let replica = move || {
        Some(crate::ReplicaState {
            name: "Chess Club".to_string(),
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            rule_m: 2,
            identities: Vec::new(),
            agenda: "play chess".to_string(),
            features: None,
            republic_id: rid.clone(),
            founded_ts: 0,
        })
    };
    let mut st = crate::tests::plain_state();
    st.replica = replica();
    st.adopt_chain(b.blocks.clone());
    assert_eq!(
        st.member_relays("walter"),
        pool,
        "a founding member is covered by the ratified pool"
    );

    // petra re-joins over a DIFFERENT relay and declares it in the block
    let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
    let declared = vec!["wss://relay.two.example".to_string()];
    let restored = b.seal(
        1,
        ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "petra".to_string(),
            identity_pk: b.pk("petra"),
            nostr_pk: Some(fresh),
            relays: declared.clone(),
            consent: None,
        },
        &["petra", "walter"],
    );
    b.push(restored);
    st.adopt_chain(b.blocks.clone());
    assert_eq!(
        st.member_relays("petra"),
        declared,
        "every member's ledger reports the declared pool"
    );
    assert_eq!(st.member_relays("walter"), pool, "the others stay on the ratified pool");

    // cut ABOVE the Restored block: the declaration must ride the summary
    let blob = checkpoint_state(&b.blocks, 1).expect("state@1");
    assert_eq!(
        blob.member_relays,
        vec![("petra".to_string(), declared.clone())],
        "the summary carries the declarations the dropped blocks established"
    );
    let cut = b.seal(
        2,
        ChainChange::Checkpoint { upto: 1, state_hash: checkpoint_state_hash(&blob) },
        &["petra", "walter"],
    );
    b.push(cut);
    let mut pruned = crate::tests::plain_state();
    pruned.replica = replica();
    pruned.set_checkpoint_blob(Some(blob));
    pruned.adopt_chain(b.blocks[2..].to_vec());
    assert_eq!(
        pruned.member_relays("petra"),
        declared,
        "a cut must not forget a declaration"
    );
    assert_eq!(pruned.member_relays("walter"), pool, "…nor the ratified fallback");
}

/// R4 — split detection: two members whose effective relay sets are
/// disjoint produce a verdict naming both, and the members surface says
/// so per member — compactly, with the relay that would bridge — rather
/// than staying a silence while the threshold quietly cannot assemble.
#[test]
fn disjoint_relay_sets_produce_a_split_verdict_naming_the_bridge() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
    let rid = b.republic_id.clone();
    let mut st = crate::tests::plain_state();
    st.replica = Some(crate::ReplicaState {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        roster: vec!["petra".to_string(), "walter".to_string()],
        rule_m: 2,
        identities: Vec::new(),
        agenda: "play chess".to_string(),
        features: None,
        republic_id: rid,
        founded_ts: 0,
    });
    st.adopt_chain(b.blocks.clone());
    assert!(st.relay_splits().is_empty(), "one shared pool - no split");

    // petra re-joins over a relay NOBODY else carries
    let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
    let restored = b.seal(
        1,
        ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "petra".to_string(),
            identity_pk: b.pk("petra"),
            nostr_pk: Some(fresh),
            relays: vec!["wss://relay.two.example".to_string()],
            consent: None,
        },
        &["petra", "walter"],
    );
    b.push(restored);
    st.adopt_chain(b.blocks.clone());

    let splits = st.relay_splits();
    assert_eq!(
        splits,
        vec![("petra".to_string(), "walter".to_string())],
        "the verdict names both seats"
    );
    // …and the members surface carries the marker, naming the bridge
    let view = st.members_view();
    let row = |m: &str| {
        view.iter()
            .find(|v| v.member == m)
            .unwrap_or_else(|| panic!("{m} row"))
            .split
            .clone()
    };
    assert!(
        row("petra").contains("walter") && row("petra").contains("wss://relay.two.example"),
        "petra's marker names the counterpart and her odd relay: {:?}",
        row("petra")
    );
    assert!(
        row("walter").contains("petra") && row("walter").contains("wss://relay.one"),
        "walter's marker mirrors it: {:?}",
        row("walter")
    );
}

#[test]
fn chain_applied_entries_carry_their_proposal_id() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_applied(7, &["petra", "walter"]);
    let mut peer = crate::tests::plain_state();
    peer.adopt_chain(b.blocks.clone());
    let snap = peer.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.applied.len(), 1, "one committed Applied block");
    assert_eq!(
        snap.applied_ids,
        vec![Some(7)],
        "the block's proposal id rides the id track"
    );
}

/// The deliberation gossip is ephemeral RAM on every RECEIVER — only the
/// proposer's own log records `Proposed`. A holder that adopts an
/// Applied block without the card (reopen replay, catch-up past lost
/// gossip) must materialize the record FROM the block: the chain
/// carries payload and signers. Without it the Accepted view degraded
/// to an id-less row — no voters, and the raw multi-line patch dumped
/// into the value cell (field report 2026-08-16, the dev republic).
#[test]
fn an_adopted_applied_block_materializes_its_accepted_card() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    let block = b.seal(
        1,
        ChainChange::Applied {
            proposal_id: 4,
            surface: Surface::Memory,
            payload: json!({
                "op": "wiki_patch",
                "value": "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+hi\n",
                "summary": "+1 -0 →0 ~1",
            }),
        },
        &["petra", "walter"],
    );
    b.push(block);

    // a reopen: the chain comes from disk, the ephemeral proposal map
    // is empty (this peer never was the proposer)
    let mut peer = crate::tests::plain_state();
    peer.replica = Some(crate::ReplicaState {
        name: "Chess Club".to_string(),
        member: "dora".to_string(),
        roster: vec!["petra".to_string(), "walter".to_string(), "dora".to_string()],
        rule_m: 2,
        identities: Vec::new(),
        agenda: String::new(),
        features: None,
        republic_id: b.republic_id.clone(),
        founded_ts: 0,
    });
    peer.adopt_chain(b.blocks.clone());

    let snap = peer.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.applied_ids, vec![Some(4)]);
    assert_eq!(snap.accepted.len(), 1, "the accepted card exists again");
    let card = &snap.accepted[0];
    assert_eq!(card.id.0, 4);
    assert_eq!(card.state, molt_core::ProposalState::Applied);
    assert_eq!(card.approvals, 2, "the sealed block's signer count");
    let approved: Vec<&str> = card
        .votes
        .iter()
        .filter(|v| v.vote == molt_core::VoteState::Approved)
        .map(|v| v.member.as_str())
        .collect();
    assert_eq!(approved, vec!["petra", "walter"], "who voted is chain-proven");
    assert_eq!(
        card.payload["op"],
        json!("wiki_patch"),
        "the payload keeps its shape (the GUI's patch rendering keys on it)"
    );

    // the LIVE twin: a broadcast block for a proposal this node never
    // heard of (its gossip was lost) materializes the card the same way
    let late = b.seal(
        2,
        ChainChange::Applied {
            proposal_id: 9,
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "title": "minutes" }),
        },
        &["walter", "dora"],
    );
    peer.receive_block(late);
    let snap = peer.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.accepted.len(), 2, "both cards stand");
    let card = snap
        .accepted
        .iter()
        .find(|c| c.id.0 == 9)
        .expect("the late block's card");
    assert_eq!(card.approvals, 2);
    assert!(
        card.votes
            .iter()
            .any(|v| v.member == "dora" && v.vote == molt_core::VoteState::Approved),
        "the live-adopted card names its signers too"
    );
}

/// A reopen replays the proposal CARDS from the persisted gossip first
/// and adopts the chain second (`open_stored_workspace`) — adoption must
/// settle every card the chain already consumed, or each restart
/// resurrects decided votes as open cards (live incident 2026-08-09: a
/// sealed `set_relays` vote came back 'proposed' on every launch of the
/// proposer's node, its `restore_member` twins with it).
#[test]
fn adopting_a_chain_settles_replayed_proposal_cards() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let genesis = b.blocks.clone();
    b.commit_applied(7, &["petra", "walter"]);
    b.commit_restored("petra", &"ab".repeat(32), &["petra", "walter"]);

    // the reopen shape: gossip-replayed cards, THEN the chain
    let mut reopened = chain_peer("walter", &b, genesis);
    reopened.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "peer");
    reopened.receive_proposed(
        8,
        Surface::Organization,
        json!({ "op": "restore_member", "member": "petra" }),
        "peer",
    );
    reopened.adopt_chain(b.blocks.clone());

    let card = reopened.proposals.get(&7).expect("card survives");
    assert_eq!(card.state, ProposalState::Applied, "the chain consumed id 7");
    let restore = reopened.proposals.get(&8).expect("restore card survives");
    assert_eq!(
        restore.state,
        ProposalState::Applied,
        "the Restored block settles the membership card"
    );

    // the LIVE twin: a late (resent) Proposed for a consumed id must not
    // re-open a card. Adoption already materialized the APPLIED record
    // from the block (ensure_applied_record) — the resend must neither
    // create a second one nor flip it back to open.
    let mut live = chain_peer("walter", &b, b.blocks.clone());
    assert!(
        !live.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "peer"),
        "a consumed id must not open a fresh card"
    );
    assert_eq!(
        live.proposals.get(&7).map(|p| p.state),
        Some(ProposalState::Applied),
        "the consumed id stays a settled, chain-proven card"
    );
}

/// The incrementally-folded projection must equal the whole-chain
/// rebuild — including across a Membership block, where the anchors map
/// and the roster move too.
///
/// This is the property `project_one` trades a refold for; a full rebuild
/// re-clones every payload in the chain per block, which made a drain of
/// N blocks clone the applied log N²/2 times.
#[test]
fn the_appended_projection_equals_the_whole_chain_rebuild() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    let fresh = molt_net::nostr_identity(b"walter-recovered", "new-ticket").1;
    b.commit_restored("walter", &fresh, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());
    for block in b.blocks[1..].iter().rev() {
        peer.receive_block(block.clone());
    }
    assert_eq!(peer.chain.blocks.len(), 4, "the whole suffix drained");

    let incremental = (peer.chain.applied.clone(), peer.chain.anchors.clone());
    peer.apply_chain_to_state();
    assert_eq!(
        incremental,
        (peer.chain.applied.clone(), peer.chain.anchors.clone()),
        "the appended projection must equal the whole-chain rebuild"
    );
}

/// The supersede walk (shared_memory_real.md §4): sealing one wiki
/// patch deterministically retires every OVERLAPPING pending patch —
/// terminal and unattributed (no vote forged: `declined_by` stays
/// empty) — keeps the DISJOINT one approvable and applying, and a
/// stale patch learned late (catch-up) registers superseded right
/// away. Approving a superseded card is refused honestly.
#[test]
fn a_sealed_wiki_patch_supersedes_overlapping_pending_patches() {
    const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
    const EDIT_A_1: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+hallo\n world\n";
    const EDIT_A_2: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+servus\n world\n";
    const ADD_B: &str = "diff --git a/b.md b/b.md\nnew file mode 100644\n--- /dev/null\n+++ b/b.md\n@@ -0,0 +1,1 @@\n+disjoint\n";
    let wp = |p: &str| json!({"op": "wiki_patch", "summary": "x", "value": p});

    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    seal_wiki(&mut walter, &b, "petra", 10, wp(ADD_A));
    assert_eq!(
        walter.wiki_tree().get("a.md").map(String::as_str),
        Some("hello\nworld\n"),
        "the fold serves the sealed base"
    );

    // two pending edits of the SAME region, one disjoint add
    walter.receive_proposed(11, Surface::Memory, wp(EDIT_A_1), "petra");
    walter.receive_proposed(12, Surface::Memory, wp(EDIT_A_2), "petra");
    walter.receive_proposed(13, Surface::Memory, wp(ADD_B), "petra");

    seal_wiki(&mut walter, &b, "petra", 11, wp(EDIT_A_1));
    let p12 = walter.proposals.get(&12).cloned().expect("card 12");
    assert_eq!(p12.state, ProposalState::Rejected, "overlap retires");
    assert!(p12.superseded, "…as SUPERSEDED, not declined");
    assert!(p12.declined_by.is_empty(), "no vote is forged");
    assert!(walter.view(12, &p12).superseded);
    assert!(
        matches!(
            walter.cmd_approve(molt_core::ProposalId(12)),
            Err(molt_core::MoltError::AlreadyTerminal(_, _))
        ),
        "approving a superseded card is refused"
    );
    let p13 = walter.proposals.get(&13).cloned().expect("card 13");
    assert_eq!(p13.state, ProposalState::Proposed, "disjoint stays open");
    assert!(!p13.superseded);

    // …and the disjoint one still seals and folds
    seal_wiki(&mut walter, &b, "petra", 13, wp(ADD_B));
    assert_eq!(
        walter.wiki_tree().get("b.md").map(String::as_str),
        Some("disjoint\n")
    );
    assert_eq!(
        walter.wiki_tree().get("a.md").map(String::as_str),
        Some("hallo\nworld\n")
    );

    // a stale patch learned LATE registers superseded immediately
    walter.receive_proposed(14, Surface::Memory, wp(EDIT_A_2), "petra");
    let p14 = walter.proposals.get(&14).cloned().expect("card 14");
    assert_eq!(p14.state, ProposalState::Rejected);
    assert!(p14.superseded);

    // …and the READ serves the same base to GUI and MCP alike
    // (co-equality: one projection, shared_memory_real.md WP-B)
    let snap = walter.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.wiki_rev, 3, "ADD_A + EDIT_A_1 + ADD_B applied");
    assert_eq!(
        snap.wiki_tree,
        vec![
            molt_core::WikiDoc {
                path: "a.md".to_string(),
                content: "hallo\nworld\n".to_string()
            },
            molt_core::WikiDoc {
                path: "b.md".to_string(),
                content: "disjoint\n".to_string()
            },
        ]
    );
}

/// `knowledge_base_scale.md` §4.3: the wiki is read PAGED — a prefix
/// selects a folder, the cursor walks the pages without gaps or repeats,
/// the limit is clamped, and an unknown path is an error.
#[test]
fn the_wiki_reads_page_by_prefix_and_cursor() {
    let add = |path: &str, body: &str| {
        format!("diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{body}\n")
    };
    let wp = |p: String| json!({"op": "wiki_patch", "summary": "x", "value": p});
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    for (i, (path, body)) in [
        ("people/anna.md", "# Anna"),
        ("people/bob.md", "# Bob"),
        ("root.md", "# Root"),
        ("zoo/last.md", "no heading"),
    ]
    .into_iter()
    .enumerate()
    {
        seal_wiki(
            &mut walter,
            &b,
            "petra",
            20 + u64::try_from(i).expect("small"),
            wp(add(path, body)),
        );
    }

    // a whole listing: path-sorted, titles from the first heading
    let molt_core::Reply::WikiList {
        docs, total, next_cursor, ..
    } = walter.cmd_wiki_list(None, None, 0).expect("list")
    else {
        panic!("wrong reply");
    };
    assert_eq!(total, 4);
    assert_eq!(next_cursor, None, "one page holds all four");
    let paths: Vec<&str> = docs.iter().map(|d| d.path.as_str()).collect();
    assert_eq!(
        paths,
        ["people/anna.md", "people/bob.md", "root.md", "zoo/last.md"]
    );
    assert_eq!(docs[0].title.as_deref(), Some("Anna"));
    assert_eq!(docs[3].title, None, "no heading, no title");
    assert_eq!(docs[0].bytes, 7, "\"# Anna\\n\" is seven bytes");

    // the cursor walks the pages: no gap, no repeat, and it ends
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = None;
    loop {
        let molt_core::Reply::WikiList {
            docs, next_cursor, ..
        } = walter.cmd_wiki_list(None, cursor, 1).expect("page")
        else {
            panic!("wrong reply");
        };
        seen.extend(docs.into_iter().map(|d| d.path));
        match next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
        assert!(seen.len() <= 4, "the cursor did not advance");
    }
    assert_eq!(seen, paths, "paged == whole");

    // a prefix selects the folder, and `total` counts only it
    let molt_core::Reply::WikiList { docs, total, .. } = walter
        .cmd_wiki_list(Some("people/".to_string()), None, 0)
        .expect("prefixed")
    else {
        panic!("wrong reply");
    };
    assert_eq!(total, 2);
    assert_eq!(docs.len(), 2);

    // the limit is clamped, never trusted
    let molt_core::Reply::WikiList { docs, .. } =
        walter.cmd_wiki_list(None, None, 99_999).expect("clamped")
    else {
        panic!("wrong reply");
    };
    assert_eq!(docs.len(), 4);

    // one document, in full — and an unknown path is an error
    let molt_core::Reply::WikiDocument { content, path, .. } = walter
        .cmd_wiki_get("people/bob.md".to_string())
        .expect("get")
    else {
        panic!("wrong reply");
    };
    assert_eq!(path, "people/bob.md");
    assert_eq!(content, "# Bob\n");
    assert!(
        walter.cmd_wiki_get("nope.md".to_string()).is_err(),
        "an unknown path must not read as an empty document"
    );
}

/// `knowledge_base_scale.md` §4.6 over the REAL applied base: the index
/// follows the applied patches, the facets narrow, and an edit leaves no
/// ghost of the old text behind.
#[test]
fn the_search_index_follows_the_applied_base() {
    let add = |path: &str, body: &str| {
        let lines: Vec<&str> = body.split('\n').collect();
        let mut patch = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for l in lines {
            patch.push('+');
            patch.push_str(l);
            patch.push('\n');
        }
        patch
    };
    let wp = |p: String| json!({"op": "wiki_patch", "summary": "x", "value": p});
    let hits = |r: molt_core::Reply| -> Vec<String> {
        match r {
            molt_core::Reply::WikiSearch { hits, .. } => {
                hits.into_iter().map(|h| h.path).collect()
            }
            other => panic!("wrong reply: {other:?}"),
        }
    };
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    seal_wiki(
        &mut walter,
        &b,
        "petra",
        40,
        wp(add(
            "people/anna.md",
            "---\ntype: person\ntags: [gruender]\n---\n# Anna\nAnna baut die Zentrale.",
        )),
    );
    seal_wiki(
        &mut walter,
        &b,
        "petra",
        41,
        wp(add("orte/berlin.md", "---\ntype: place\n---\n# Berlin\nEine Zentrale.")),
    );

    let found = hits(
        walter
            .cmd_wiki_search("Zentrale".to_string(), vec![], None, None, 0, 0)
            .expect("search"),
    );
    assert_eq!(found.len(), 2, "both documents carry the word: {found:?}");

    // the facets narrow, and so does the folder
    let found = hits(
        walter
            .cmd_wiki_search(
                "Zentrale".to_string(),
                vec!["gruender".to_string()],
                Some("person".to_string()),
                Some("people".to_string()),
                0,
                0,
            )
            .expect("search"),
    );
    assert_eq!(found, vec!["people/anna.md"]);

    // an EDIT must not leave the old text findable
    let edit = "diff --git a/orte/berlin.md b/orte/berlin.md\n--- a/orte/berlin.md\n+++ b/orte/berlin.md\n@@ -1,5 +1,5 @@\n ---\n type: place\n ---\n # Berlin\n-Eine Zentrale.\n+Eine Aussenstelle.\n";
    seal_wiki(&mut walter, &b, "petra", 42, wp(edit.to_string()));
    let found = hits(
        walter
            .cmd_wiki_search("Zentrale".to_string(), vec![], None, None, 0, 0)
            .expect("search"),
    );
    assert_eq!(found, vec!["people/anna.md"], "the edited text is gone");
    let found = hits(
        walter
            .cmd_wiki_search("Aussenstelle".to_string(), vec![], None, None, 0, 0)
            .expect("search"),
    );
    assert_eq!(found, vec!["orte/berlin.md"], "…and the new text is there");

    // an empty query with no filter finds nothing, never everything
    assert!(hits(
        walter
            .cmd_wiki_search(String::new(), vec![], None, None, 0, 0)
            .expect("search")
    )
    .is_empty());
}

/// `knowledge_base_scale.md` §4.4-4.5 over the REAL applied base: the
/// header's typed relations and the body's links reach `wiki_links` and
/// `wiki_neighbors`, the graph follows an applied patch, and a header
/// outside the subset warns the proposer without voiding anything.
#[test]
fn the_link_graph_follows_the_applied_base() {
    let add = |path: &str, body: &str| {
        let lines: Vec<&str> = body.split('\n').collect();
        let mut patch = format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for l in lines {
            patch.push('+');
            patch.push_str(l);
            patch.push('\n');
        }
        patch
    };
    let wp = |p: String| json!({"op": "wiki_patch", "summary": "x", "value": p});
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    seal_wiki(
        &mut walter,
        &b,
        "petra",
        30,
        wp(add("anna.md", "---\nworks_at: \"[[acme]]\"\n---\n# Anna\nsee [acme](acme.md)")),
    );
    seal_wiki(&mut walter, &b, "petra", 31, wp(add("acme.md", "# Acme")));

    let molt_core::Reply::WikiLinks { edges, index_rev, .. } = walter
        .cmd_wiki_links("anna.md".to_string(), Some("out".to_string()), None, 0, 0)
        .expect("links")
    else {
        panic!("wrong reply");
    };
    assert_eq!(index_rev, 2, "the graph reflects both applied patches");
    let mut seen: Vec<(String, Option<String>)> = edges
        .iter()
        .map(|e| (e.path.clone(), e.predicate.clone()))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("acme.md".to_string(), None),
            ("acme.md".to_string(), Some("works_at".to_string())),
        ],
        "the header relation and the body link both land"
    );

    // the predicate filter, and the other side of the same edges
    let molt_core::Reply::WikiLinks { edges, .. } = walter
        .cmd_wiki_links(
            "anna.md".to_string(),
            Some("out".to_string()),
            Some("works_at".to_string()),
            0,
            0,
        )
        .expect("filtered")
    else {
        panic!("wrong reply");
    };
    assert_eq!(edges.len(), 1);
    let molt_core::Reply::WikiLinks { edges, .. } = walter
        .cmd_wiki_links("acme.md".to_string(), Some("in".to_string()), None, 0, 0)
        .expect("in-edges")
    else {
        panic!("wrong reply");
    };
    assert_eq!(edges.len(), 2);
    assert!(edges.iter().all(|e| e.path == "anna.md" && e.direction == "in"));

    let molt_core::Reply::WikiNeighbors { docs, .. } = walter
        .cmd_wiki_neighbors("anna.md".to_string(), 1, 0)
        .expect("neighbors")
    else {
        panic!("wrong reply");
    };
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].path, "acme.md");
    assert_eq!(docs[0].distance, 1);

    // an unknown document is an error on both, never an empty answer
    assert!(walter
        .cmd_wiki_links("nope.md".to_string(), None, None, 0, 0)
        .is_err());
    assert!(walter
        .cmd_wiki_neighbors("nope.md".to_string(), 1, 0)
        .is_err());

    // …and wiki_get carries the parsed header plus both link counts
    let molt_core::Reply::WikiDocument {
        props,
        links_out,
        links_in,
        ..
    } = walter.cmd_wiki_get("anna.md".to_string()).expect("get")
    else {
        panic!("wrong reply");
    };
    assert_eq!(props["works_at"], json!("[[acme]]"));
    assert_eq!((links_out, links_in), (2, 0));

    // a header outside the subset WARNS the proposer and voids nothing
    let molt_core::Reply::Proposed { warnings, .. } = walter
        .cmd_propose(
            Surface::Memory,
            wp(add("broken.md", "---\n- not a mapping\n---\n# B")),
        )
        .expect("propose")
    else {
        panic!("wrong reply");
    };
    assert_eq!(warnings.len(), 1, "one document, one warning: {warnings:?}");
    assert!(warnings[0].starts_with("broken.md: "), "{warnings:?}");
}

/// `knowledge_base_scale.md` §4.1: the fold cache is a pure DERIVATION.
/// After every applied block — cache warm, cache dropped, and across a
/// wholesale re-projection — the served base equals a fresh fold of the
/// applied log, revision included.
#[test]
fn the_fold_cache_equals_a_fresh_fold_after_every_block() {
    const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
    const ADD_B: &str = "diff --git a/b.md b/b.md\nnew file mode 100644\n--- /dev/null\n+++ b/b.md\n@@ -0,0 +1,1 @@\n+disjoint\n";
    const EDIT_A: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n-hello\n+hallo\n world\n";
    const RENAME_A: &str = "diff --git a/a.md b/c.md\nsimilarity index 100%\nrename from a.md\nrename to c.md\n";
    let wp = |p: &str| json!({"op": "wiki_patch", "summary": "x", "value": p});
    // the ORACLE is the pre-cache code path: fold the whole log from scratch
    let fresh = |s: &crate::State| {
        let payloads: Vec<serde_json::Value> = s
            .applied_values(Surface::Memory, None, None)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        molt_core::wiki_fold::wiki_fold_with_rev(&payloads)
    };

    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    for (id, patch) in [(10u64, ADD_A), (11, ADD_B), (12, EDIT_A), (13, RENAME_A)] {
        seal_wiki(&mut walter, &b, "petra", id, wp(patch));
        let (tree, rev) = walter.wiki_base();
        assert_eq!(
            (tree.into_owned(), rev),
            fresh(&walter),
            "the warm cache drifted after block {id}"
        );
        // …and a DROPPED cache serves the same base: the fallback fold is
        // the same function, so a missed refresh costs time, never a tree
        walter.wiki_cache = None;
        let (tree, rev) = walter.wiki_base();
        assert_eq!(
            (tree.into_owned(), rev),
            fresh(&walter),
            "the cold path drifted after block {id}"
        );
        walter.refresh_wiki_cache();
    }
    assert_eq!(
        walter.wiki_tree().get("c.md").map(String::as_str),
        Some("hallo\nworld\n"),
        "add, edit and rename all folded"
    );
    assert_eq!(walter.wiki_base().1, 4, "four patches applied");

    // a wholesale re-projection can REMOVE entries — the cache must refold
    // across it rather than extend
    walter.apply_chain_to_state();
    let (tree, rev) = walter.wiki_base();
    assert_eq!(
        (tree.into_owned(), rev),
        fresh(&walter),
        "the cache survived a rebuild it cannot describe"
    );
}

/// `seal_one`'s wiki twin: drive `payload` through the real chain
/// machinery to a sealed Applied block.
fn seal_wiki(
    s: &mut crate::State,
    b: &Builder,
    peer: &str,
    id: u64,
    payload: serde_json::Value,
) {
    let target = s.chain.head.as_ref().expect("head").height + 1;
    s.receive_proposed(id, Surface::Memory, payload.clone(), "peer");
    let change = ChainChange::Applied {
        proposal_id: id,
        surface: Surface::Memory,
        payload,
    };
    let bytes = approval_bytes(&b.republic_id, target, &change);
    let sig = identity_sign(b.key(peer), &bytes);
    s.receive_approval(id, peer, target, &sig);
    s.chain_sign_and_gossip_approval(id);
    assert_eq!(s.chain.head.as_ref().expect("head").height, target, "sealed");
}

/// The pull-back visibility gate: a record remembers who proposed it,
/// and `mine` is reader-relative — true only when the reader IS that
/// member ("" matches nobody).
#[test]
fn proposal_views_know_their_proposer_and_mine_is_reader_relative() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    walter.receive_proposed(5, Surface::Memory, json!({"op": "add_note"}), "petra");
    let p = walter.proposals.get(&5).cloned().expect("record");
    assert_eq!(p.by, "petra");
    assert!(!walter.view(5, &p).mine, "petra's proposal is not walter's");
    // walter's own: the record carries his name, the view says mine
    walter.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "walter");
    let own = walter.proposals.get(&6).cloned().expect("record");
    assert!(walter.view(6, &own).mine);
    // a pre-field record ("" proposer) is nobody's
    let mut blank = walter.proposals.get(&5).cloned().expect("record");
    blank.by = String::new();
    assert!(!walter.view(5, &blank).mine);
}

/// R6 — the pool is group state any member can move and no member can
/// move alone: a `set_relays` edit is an ordinary gated Organization
/// proposal; below threshold the effective pool does not move, at m it
/// does — for every member folding the same chain.
#[test]
fn a_pool_edit_commits_under_threshold_and_moves_the_effective_pool() {
    let pool = vec!["wss://relay.one".to_string()];
    let b = Builder::new_on_relays(&["petra", "walter"], 2, pool.clone());
    // WALTER — not the founder — raises the edit
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    walter
        .cmd_propose(
            Surface::Organization,
            serde_json::json!({
                "op": "set_relays",
                "value": "wss://relay.one wss://relay.three.example",
            }),
        )
        .expect("a member may propose a pool edit");
    let (id, surface, payload) = {
        let (id, rec) = walter.proposals.iter().next().expect("open proposal");
        (*id, rec.surface, rec.payload.clone())
    };
    assert_eq!(
        walter.effective_relays(),
        pool,
        "below threshold the pool must not move"
    );
    // petra learns the proposal + walter's signature, then co-signs
    petra.receive_proposed(id, surface, payload, "peer");
    let walter_sig = walter
        .chain.pending_sigs
        .get(&id)
        .expect("walter's pending set")
        .sigs
        .iter()
        .find(|a| a.member == "walter")
        .expect("walter signed")
        .sig
        .clone();
    petra.receive_approval(id, "walter", 1, &walter_sig);
    petra.chain_sign_and_gossip_approval(id);
    assert_eq!(petra.chain.head.as_ref().expect("head").height, 1, "sealed at m");
    assert_eq!(
        petra.effective_relays(),
        vec!["wss://relay.one".to_string(), "wss://relay.three.example".to_string()],
        "at m the pool moves"
    );
}

/// R6 make-before-break (found LIVE 2026-08-09): a pool edit sharing NO
/// relay with the effective pool is refused outright. The commit that
/// moves the pool travels over the OLD pool; a member that has not
/// applied it yet keeps listening there while the members that have
/// rebuild onto the new pool only — with zero overlap the two sides can
/// never meet again (a throwaway republic split exactly this way).
/// A full migration is two votes: add the new relay, then drop the old.
#[test]
fn a_pool_edit_sharing_no_relay_with_the_current_pool_is_refused() {
    let pool = vec!["wss://relay.one".to_string()];
    let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let err = walter
        .cmd_propose(
            Surface::Organization,
            serde_json::json!({
                "op": "set_relays",
                "value": "wss://relay.two.example",
            }),
        )
        .expect_err("a zero-overlap pool edit must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("wss://relay.one"),
        "the refusal names a current relay to keep: {msg}"
    );
    // …and the same target relay passes as make-before-break step one
    walter
        .cmd_propose(
            Surface::Organization,
            serde_json::json!({
                "op": "set_relays",
                "value": "wss://relay.one wss://relay.two.example",
            }),
        )
        .expect("keeping one shared relay is the legal migration step");
}

/// Make-before-break holds at the FOLD, not only at propose (review
/// 2026-08-09): the propose gate is local courtesy — a peer on another
/// build can gossip a zero-overlap edit, and two individually-legal
/// pending edits can compose into one. The fold is the only place every
/// node passes deterministically, so an applied `set_relays` sharing no
/// relay with the pool accumulated SO FAR is a no-op — a pure function
/// of chain content, identical on every holder.
#[test]
fn a_zero_overlap_pool_block_folds_as_a_no_op() {
    let r_a = "wss://relay.one".to_string();
    let r_b = "wss://relay.two.example".to_string();
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, vec![r_a.clone()]);
    let block = |b: &Builder, h, value: &str| {
        b.seal(
            h,
            ChainChange::Applied {
                proposal_id: h,
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_relays", "value": value }),
            },
            &["petra", "walter"],
        )
    };
    // height 1: zero overlap with [A] — must keep [A]
    let zero = block(&b, 1, &r_b);
    b.push(zero);
    let walter = chain_signer("walter", &b, b.blocks.clone());
    assert_eq!(walter.effective_relays(), vec![r_a.clone()], "zero overlap folds as no-op");
    // height 2: [A B] overlaps via A — applies; height 3: [B] overlaps
    // via B — applies. The legal two-vote migration lands on [B].
    let step = block(&b, 2, &format!("{r_a} {r_b}"));
    b.push(step);
    let done = block(&b, 3, &r_b);
    b.push(done);
    let walter = chain_signer("walter", &b, b.blocks.clone());
    assert_eq!(walter.effective_relays(), vec![r_b], "the two-vote migration applies");
}

/// Charter features D5: a `set_features` edit is an ordinary gated
/// Organization proposal — below threshold the effective set does not
/// move, at m it does, for every member folding the same chain. The
/// legacy baseline (D6) is Shared Memory: this republic was founded
/// pre-v5 (`features: None`), so `memory` is on and everything else off
/// until voted in.
#[test]
fn a_feature_edit_commits_under_threshold_and_moves_the_effective_set() {
    let pool = vec!["wss://relay.one".to_string()];
    let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    assert_eq!(
        walter.effective_features(),
        vec!["memory".to_string()],
        "the legacy baseline is Shared Memory"
    );
    // WALTER — not the founder — raises the edit, deliberately unsorted
    // with a duplicate: the proposal is stored canonicalized
    walter
        .cmd_propose(
            Surface::Organization,
            serde_json::json!({
                "op": "set_features",
                "value": "quests memory quests",
            }),
        )
        .expect("a member may propose a feature edit");
    let (id, surface, payload) = {
        let (id, rec) = walter.proposals.iter().next().expect("open proposal");
        (*id, rec.surface, rec.payload.clone())
    };
    assert_eq!(
        payload.get("value").and_then(serde_json::Value::as_str),
        Some("memory quests"),
        "the proposal carries the canonical set"
    );
    assert_eq!(
        walter.effective_features(),
        vec!["memory".to_string()],
        "below threshold the set must not move"
    );
    petra.receive_proposed(id, surface, payload, "peer");
    let walter_sig = walter
        .chain.pending_sigs
        .get(&id)
        .expect("walter's pending set")
        .sigs
        .iter()
        .find(|a| a.member == "walter")
        .expect("walter signed")
        .sig
        .clone();
    petra.receive_approval(id, "walter", 1, &walter_sig);
    petra.chain_sign_and_gossip_approval(id);
    assert_eq!(petra.chain.head.as_ref().expect("head").height, 1, "sealed at m");
    assert_eq!(
        petra.effective_features(),
        vec!["memory".to_string(), "quests".to_string()],
        "at m the set moves"
    );
}

/// Enable-only at propose time: dropping an enabled feature, re-enabling
/// the current set unchanged, and an unknown key are all refused before
/// anything reaches the members.
#[test]
fn a_feature_edit_that_shrinks_repeats_or_invents_is_refused() {
    let pool = vec!["wss://relay.one".to_string()];
    let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let propose = |st: &mut crate::State, value: &str| {
        st.cmd_propose(
            Surface::Organization,
            serde_json::json!({ "op": "set_features", "value": value }),
        )
    };
    let err = propose(&mut walter, "quests").expect_err("dropping memory must be refused");
    assert!(format!("{err:?}").contains("memory: cannot be disabled"), "{err:?}");
    let err = propose(&mut walter, "memory").expect_err("a no-op must be refused");
    assert!(format!("{err:?}").contains("already enabled"), "{err:?}");
    let err = propose(&mut walter, "memory kanban").expect_err("an unknown key must be refused");
    assert!(format!("{err:?}").contains("unknown feature: kanban"), "{err:?}");
    let err = propose(&mut walter, "").expect_err("an empty edit must be refused");
    assert!(format!("{err:?}").contains("nothing to enable"), "{err:?}");
}

/// Enable-only holds at the FOLD, not only at propose: the fold is a
/// UNION, so a hand-built block that "drops" a feature (bypassing every
/// propose-time gate) folds as pure addition on every holder. This is
/// the deterministic twin — without it, "features can never be switched
/// off" would be local courtesy.
#[test]
fn a_feature_dropping_block_folds_as_a_union() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    // height 1: "quests" alone — as a REPLACEMENT it would drop memory
    let drop = b.seal(
        1,
        ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_features", "value": "quests" }),
        },
        &["petra", "walter"],
    );
    b.push(drop);
    let walter = chain_signer("walter", &b, b.blocks.clone());
    assert_eq!(
        walter.effective_features(),
        vec!["memory".to_string(), "quests".to_string()],
        "a dropping block folds as a union - nothing is ever disabled"
    );
}

/// D7: the engine refuses selecting and proposing on a surface the
/// charter has not enabled — the co-equal twin of the nav hiding it —
/// and an enabling block opens the same gate for every holder.
#[test]
fn a_disabled_surface_refuses_select_and_propose_until_enabled() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    // legacy baseline {memory}: memory passes, quests is refused
    walter.cmd_select_surface(Surface::Memory).expect("memory is enabled");
    let err = walter
        .cmd_select_surface(Surface::Quests)
        .expect_err("selecting a disabled surface must be refused");
    assert_eq!(format!("{err}"), "quests: not enabled");
    let err = walter
        .cmd_select_view(Surface::Quests, "board".to_string())
        .expect_err("selecting a disabled surface's view must be refused");
    assert_eq!(format!("{err}"), "quests: not enabled");
    let err = walter
        .cmd_propose(Surface::Quests, serde_json::json!({ "op": "x", "value": "y" }))
        .expect_err("proposing on a disabled surface must be refused");
    assert_eq!(format!("{err}"), "quests: not enabled");
    // the enabling block opens the gate
    let enable = b.seal(
        1,
        ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
        },
        &["petra", "walter"],
    );
    b.push(enable);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    walter.cmd_select_surface(Surface::Quests).expect("an enabled surface passes");
    // …and the effective set is on the status surface (co-equal read)
    assert_eq!(
        walter.status().features,
        vec!["memory".to_string(), "quests".to_string()],
    );
}

/// D7's approve half (review 2026-08-12): a peer's proposal on a
/// disabled surface lands in the pool (ingest is tolerant — the
/// enabling block may simply not have applied here yet), but no
/// signature leaves this node for it, so it can never reach m honest
/// seats. Once the feature is enabled the same approval passes.
#[test]
fn an_approval_on_a_disabled_surface_is_refused_until_enabled() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    // a peer proposal on quests (disabled: legacy baseline is {memory})
    walter.receive_proposed(
        9,
        Surface::Quests,
        serde_json::json!({ "op": "add_quest", "title": "t" }),
        "peer",
    );
    let err = walter
        .cmd_approve(molt_core::ProposalId(9))
        .expect_err("no signature may leave for a disabled surface");
    assert_eq!(format!("{err}"), "quests: not enabled");
    // the enabling block opens the gate for the SAME proposal
    let enable = b.seal(
        1,
        ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
        },
        &["petra", "walter"],
    );
    b.push(enable);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    walter.receive_proposed(
        9,
        Surface::Quests,
        serde_json::json!({ "op": "add_quest", "title": "t" }),
        "peer",
    );
    walter
        .cmd_approve(molt_core::ProposalId(9))
        .expect("an enabled surface accepts the approval");
}

/// Review 2026-08-12 (mixed versions): an unknown key can become
/// effective here via a NEWER build's applied block (wire ingest never
/// runs this build's validate). The enable-only gate must not demand a
/// key this build cannot name — validate would refuse it — and the
/// union fold keeps it regardless, so feature governance keeps working.
#[test]
fn an_unknown_effective_key_does_not_brick_feature_governance() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let newer = b.seal(
        1,
        ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_features", "value": "memory zzz" }),
        },
        &["petra", "walter"],
    );
    b.push(newer);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    assert!(
        walter.effective_features().iter().any(|f| f == "zzz"),
        "the unknown key is effective (union fold keeps it)"
    );
    // this build proposes WITHOUT the key it cannot name — accepted
    walter
        .cmd_propose(
            Surface::Organization,
            serde_json::json!({ "op": "set_features", "value": "memory quests" }),
        )
        .expect("an unknown effective key must not brick the gates");
    // …and the fold still keeps zzz alongside the new enable. Select
    // the OPEN card: adoption materialized the applied block's card too
    let (id, surface, payload) = {
        let (id, rec) = walter
            .proposals
            .iter()
            .find(|(_, p)| p.state == ProposalState::Proposed)
            .expect("open proposal");
        (*id, rec.surface, rec.payload.clone())
    };
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    petra.receive_proposed(id, surface, payload, "peer");
    let walter_sig = walter
        .chain.pending_sigs
        .get(&id)
        .expect("walter's pending set")
        .sigs
        .iter()
        .find(|a| a.member == "walter")
        .expect("walter signed")
        .sig
        .clone();
    petra.receive_approval(id, "walter", 2, &walter_sig);
    petra.chain_sign_and_gossip_approval(id);
    assert_eq!(
        petra.effective_features(),
        vec![
            "memory".to_string(),
            "quests".to_string(),
            "zzz".to_string()
        ],
        "the union keeps what this build cannot name"
    );
}

/// The mint counter stays ahead of chain-consumed proposal ids (review
/// 2026-08-12): a holder that adopted its chain WITHOUT the ephemeral
/// event log (a blob-seeded rejoiner after total loss) would otherwise
/// mint an id the chain already decided — every peer's ingest refuses
/// that as a stale resend, so the proposal could never seal: a silent
/// governance-liveness hole.
#[test]
fn a_fresh_adopter_never_mints_a_chain_consumed_proposal_id() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let enable = b.seal(
        1,
        ChainChange::Applied {
            proposal_id: 1,
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
        },
        &["petra", "walter"],
    );
    b.push(enable);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    walter
        .cmd_propose(
            Surface::Organization,
            serde_json::json!({ "op": "set_features", "value": "memory quests vault" }),
        )
        .expect("propose");
    // the OPEN card (adoption materialized the applied block's card too)
    let (id, rec) = walter
        .proposals
        .iter()
        .find(|(_, p)| p.state == ProposalState::Proposed)
        .expect("open proposal");
    assert!(*id > 1, "the consumed id 1 must be skipped, got {id}");
    // and a peer registers it instead of refusing a "stale resend"
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    assert!(
        petra.receive_proposed(*id, rec.surface, rec.payload.clone(), "peer"),
        "the peer registers the freshly minted id"
    );
}

/// The baseline rule (D6): a v5 founding's ratified selection IS the
/// baseline — `Some([])` means "nothing optional", never the legacy
/// Shared-Memory grandfather, and an explicit selection replaces it.
#[test]
fn the_feature_baseline_follows_the_ratified_selection() {
    let b = Builder::new_on_relays(&["petra", "walter"], 2, Vec::new());
    let mut st = chain_signer("walter", &b, b.blocks.clone());
    if let Some(r) = st.replica.as_mut() {
        r.features = Some(Vec::new());
    }
    assert_eq!(
        st.effective_features(),
        Vec::<String>::new(),
        "an explicitly empty selection enables nothing"
    );
    if let Some(r) = st.replica.as_mut() {
        r.features = Some(vec!["wallet".to_string()]);
    }
    assert_eq!(st.effective_features(), vec!["wallet".to_string()]);
}

/// R6: an edit that would strand a member — a new pool sharing no relay
/// with what that member is on record as reaching — is refused at
/// propose time, naming the member and its relay (the R4 split it would
/// otherwise commit).
#[test]
fn a_pool_edit_that_would_strand_a_member_is_refused() {
    let pool = vec!["wss://relay.one".to_string()];
    let mut b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    // petra is on record as reaching ONLY relay.two
    let fresh = molt_net::nostr_identity(b"petra-recovered", "new-ticket").1;
    let restored = b.seal(
        1,
        ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "petra".to_string(),
            identity_pk: b.pk("petra"),
            nostr_pk: Some(fresh),
            relays: vec!["wss://relay.two.example".to_string()],
            consent: None,
        },
        &["petra", "walter"],
    );
    b.push(restored);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let err = walter
        .cmd_propose(
            Surface::Organization,
            serde_json::json!({
                "op": "set_relays",
                "value": "wss://relay.three.example",
            }),
        )
        .expect_err("a pool that strands a member must be refused");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("petra") && msg.contains("wss://relay.two.example"),
        "the refusal names the stranded member and its relay: {msg}"
    );
}

/// The co-equal Chain-History read (`Command::ReadChain`): every committed
/// block newest first with the right kinds, the checkpoint block visible —
/// and after the auto-drop, the pruned holder still lists the pre-cut
/// applied entries as synthetic views from its checkpoint blob (height 0:
/// the per-entry heights are gone with the history).
#[test]
fn read_chain_lists_blocks_newest_first_and_survives_the_prune() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());

    // full holder: genesis + the two applied blocks, newest first
    let molt_core::Reply::Chain { blocks } = walter.cmd_read_chain().expect("read") else {
        panic!("read_chain answers Reply::Chain");
    };
    assert_eq!(
        blocks
            .iter()
            .map(|v| (v.height, v.kind.as_str()))
            .collect::<Vec<_>>(),
        vec![(2, "applied"), (1, "applied"), (0, "genesis")]
    );
    assert_eq!(blocks[0].proposal_id, 2, "the applied view names its proposal");
    assert_eq!(blocks[0].surface, "memory");
    assert_eq!(blocks[0].payload["op"], json!("add_note"));
    assert_eq!(
        blocks[0].signers,
        vec!["petra".to_string(), "walter".to_string()],
        "the signers ride the view in block order"
    );
    assert_eq!(blocks[2].payload, json!("Chess Club"), "the genesis shows the name");
    assert_eq!(blocks[2].surface, "");
    assert_eq!(blocks[2].proposal_id, 0);

    // seal the checkpoint cut at the head (stage-3 mechanics) → auto-drop
    let hash = checkpoint_state_hash(&checkpoint_state(&b.blocks, 2).expect("state"));
    walter.receive_checkpoint_proposal(40, 2, &hash);
    let change = ChainChange::Checkpoint { upto: 2, state_hash: hash };
    let bytes = approval_bytes(&b.republic_id, 3, &change);
    let petra_sig = identity_sign(b.key("petra"), &bytes);
    walter.receive_approval(40, "petra", 3, &petra_sig);
    assert_eq!(walter.chain.blocks.len(), 1, "history below the cut is dropped");

    // pruned holder: the real anchor keeps its height, then the synthetic
    // pre-cut applied views (newest first, signers gone), genesis last
    let molt_core::Reply::Chain { blocks } = walter.cmd_read_chain().expect("read") else {
        panic!("read_chain answers Reply::Chain");
    };
    assert_eq!(
        blocks
            .iter()
            .map(|v| (v.height, v.kind.as_str()))
            .collect::<Vec<_>>(),
        vec![(3, "checkpoint"), (0, "applied"), (0, "applied"), (0, "genesis")]
    );
    assert_eq!(blocks[0].payload, json!(2), "the checkpoint view shows the upto");
    assert_eq!(blocks[0].signers.len(), 2, "the anchor block keeps its m signers");
    assert_eq!(blocks[1].proposal_id, 2, "pre-cut entries stay listed, newest first");
    assert_eq!(blocks[2].proposal_id, 1);
    assert_eq!(blocks[1].surface, "memory");
    assert!(
        blocks[1].signers.is_empty() && blocks[2].signers.is_empty(),
        "the pre-cut block signatures are gone with the history"
    );
    assert_eq!(blocks[3].payload, json!("Chess Club"));
    assert_eq!(
        blocks[3].signers,
        vec!["petra".to_string(), "walter".to_string()],
        "the genesis view rebuilds from the blob's founding table"
    );
}
