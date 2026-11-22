// SPDX-License-Identifier: GPL-3.0-or-later

//! The sender twin of the L3 open-card cap: a seat never puts up a card
//! its peers would refuse.

use super::governance::OPEN_CARDS_PER_PROPOSER_MAX;
use super::test_support::*;
use molt_core::{
    approval_bytes, AllowWarnings, MoltError, ProposalId, ProposalRecord, ProposalState, Surface,
    WikiEdit, WorkspaceEvent,
};
use molt_storage::identity_sign;
use serde_json::{json, Value};

fn note(i: usize) -> Value {
    json!({ "op": "add_note", "title": format!("n{i}") })
}

fn walter_at_cap() -> (Builder, crate::State, Vec<u64>) {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let ids = (0..OPEN_CARDS_PER_PROPOSER_MAX)
        .map(|i| propose(&mut walter, Surface::Memory, note(i)))
        .collect();
    (b, walter, ids)
}

/// `out` is the cap refusal, and nothing was recorded or minted.
fn refused_unmoved<T: std::fmt::Debug>(
    st: &mut crate::State,
    run: impl FnOnce(&mut crate::State) -> Result<T, MoltError>,
) {
    let (seq, cards, next_id) = (st.next_seq, st.proposals.len(), st.next_id);
    match run(st) {
        Err(e @ MoltError::OpenProposalsCap(_)) => {
            assert_eq!(e.to_string(), "64 open proposals - wait for a seal");
        }
        other => panic!("expected the cap refusal, got {other:?}"),
    }
    assert_eq!(st.next_seq, seq, "nothing recorded, nothing broadcast");
    assert_eq!(st.proposals.len(), cards, "no card");
    assert_eq!(st.next_id, next_id, "no id minted");
}

fn open_by(st: &crate::State, by: &str) -> usize {
    st.proposals
        .values()
        .filter(|p| p.state == ProposalState::Proposed && p.by == by)
        .count()
}

#[test]
fn the_65th_own_proposal_is_refused_until_a_card_leaves_the_vote() {
    let (b, mut walter, ids) = walter_at_cap();
    refused_unmoved(&mut walter, |w| w.cmd_propose(Surface::Memory, note(99)));

    // a withdraw frees a slot
    walter.cmd_withdraw(ProposalId(ids[0]), None).expect("withdraw");
    propose(&mut walter, Surface::Memory, note(100));
    refused_unmoved(&mut walter, |w| w.cmd_propose(Surface::Memory, note(101)));

    // a seal frees a slot
    let change = walter.proposal_change(ids[1]).expect("a registered change");
    let sig = identity_sign(b.key("petra"), &approval_bytes(&b.republic_id, 1, &change));
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Approved { id: ProposalId(ids[1]), by: "petra".to_string(), height: 1, sig },
    );
    assert_eq!(walter.chain.blocks.len(), 2, "sealed");
    propose(&mut walter, Surface::Memory, note(102));
    refused_unmoved(&mut walter, |w| w.cmd_propose(Surface::Memory, note(103)));

    // a decline that rejects frees a slot
    wire(
        &mut walter,
        "petra",
        2,
        WorkspaceEvent::Declined { id: ProposalId(ids[2]), by: "petra".to_string(), hash: String::new() },
    );
    assert_eq!(walter.proposals[&ids[2]].state, ProposalState::Rejected);
    propose(&mut walter, Surface::Memory, note(104));
    assert_eq!(open_by(&walter, "walter"), OPEN_CARDS_PER_PROPOSER_MAX);
}

#[test]
fn an_organization_change_meets_the_gate() {
    let (_, mut walter, _) = walter_at_cap();
    refused_unmoved(&mut walter, |w| {
        w.cmd_propose(Surface::Organization, json!({ "op": "set_name", "value": "Club" }))
    });
    refused_unmoved(&mut walter, |w| {
        w.cmd_propose(Surface::Organization, json!({ "op": "set_features", "value": "memory quests" }))
    });
}

/// One call is one card, so a multi-edit call is refused whole.
#[test]
fn a_wiki_edit_meets_the_gate_whole_and_a_supersede_fits() {
    let (_, mut walter, ids) = walter_at_cap();
    let edits = || {
        vec![
            WikiEdit::Create { path: "a.md".to_string(), content: "one\n".to_string() },
            WikiEdit::Create { path: "b.md".to_string(), content: "two\n".to_string() },
        ]
    };
    let allow = AllowWarnings::All(true);
    refused_unmoved(&mut walter, |w| w.cmd_wiki_edit(edits(), false, &allow, None, false));
    walter
        .cmd_wiki_edit(edits(), true, &allow, None, false)
        .expect("a dry run mints nothing and passes");
    walter
        .cmd_wiki_edit(edits(), false, &allow, Some(ProposalId(ids[0])), false)
        .expect("replacing an own card keeps the count");
    assert_eq!(open_by(&walter, "walter"), OPEN_CARDS_PER_PROPOSER_MAX);
}

#[test]
fn a_persist_vote_meets_the_gate_and_the_share_stays() {
    let (_, mut walter, _) = walter_at_cap();
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("minutes.pdf");
    std::fs::write(&path, b"minutes").expect("write");
    let id = molt_core::MessageId([4u8; 16]);
    let mut msg = molt_core::ChatMessage::text(id, "walter", "a share", crate::now_secs());
    msg.file = Some(molt_core::FileMeta {
        name: "minutes.pdf".to_string(),
        size: 7,
        kind: "PDF".to_string(),
        modified: 100,
        available: true,
        checksum: "ab".repeat(32),
        key_b64: String::new(),
        pieces: 0,
        root: String::new(),
    });
    let env = walter.make_env("walter".to_string(), WorkspaceEvent::Chat(msg));
    walter.apply(&env);
    walter.files.share_paths.insert(id, path);

    refused_unmoved(&mut walter, |w| {
        w.cmd_propose(Surface::Files, json!({ "op": "persist", "id": id.to_string() }))
    });
    refused_unmoved(&mut walter, |w| {
        w.cmd_propose(Surface::Files, json!({ "op": "delete", "id": id.to_string() }))
    });
    let row = walter.uploads_view().into_iter().find(|u| u.id == id).expect("the share stays listed");
    assert!(row.available && row.expires_ts > 0, "still a temporary share: {row:?}");
}

#[test]
fn a_kanban_changeset_meets_the_gate() {
    let b = Builder::new_with_features(&["petra", "walter"], 2, &["quests"], false);
    let mut walter = genesis_seat("walter", &b, b.blocks.clone());
    fill_to_open_cap(&mut walter);
    let add = json!({ "act": "add", "ref": "t", "title": "t", "assignees": ["walter"] });
    refused_unmoved(&mut walter, |w| {
        w.cmd_propose(
            Surface::Quests,
            json!({ "op": "kanban_ops", "summary": "s", "base_rev": 0, "ops": [add] }),
        )
    });
}

/// A checkpoint registers no card on any seat, so no receiver counts it:
/// the cut stays open to a seat at its cap.
#[test]
fn a_checkpoint_is_exempt() {
    let (_, mut walter, _) = walter_at_cap();
    walter.cmd_propose_checkpoint().expect("a cut is not a card");
}

/// Over the cap (a re-base reopened cards) a seat is not stuck: its votes
/// still move, and a new card waits until the count is below the cap.
#[test]
fn a_seat_over_its_cap_only_waits() {
    let (_, mut walter, ids) = walter_at_cap();
    for i in 0..2 {
        let id = walter.mint_proposal_id();
        walter
            .proposals
            .insert(id, ProposalRecord::new(Surface::Memory, note(200 + i), "walter".to_string()));
    }
    refused_unmoved(&mut walter, |w| w.cmd_propose(Surface::Memory, note(1)));
    walter.cmd_withdraw(ProposalId(ids[0]), None).expect("withdraw is never gated");
    refused_unmoved(&mut walter, |w| w.cmd_propose(Surface::Memory, note(2)));
    walter.cmd_withdraw(ProposalId(ids[1]), None).expect("withdraw");
    walter.cmd_withdraw(ProposalId(ids[2]), None).expect("withdraw");
    propose(&mut walter, Surface::Memory, note(3));
}

/// Every card the sender puts up, a receiver on the old receive rule
/// takes - the coordinator's membership card counts on both sides.
#[test]
fn a_sender_at_its_cap_sends_nothing_a_receiver_refuses() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    let membership = ProposalRecord::new(
        Surface::Organization,
        json!({ "op": "restore_member", "member": "dora" }),
        "walter".to_string(),
    );
    walter.proposals.insert(7, membership.clone());
    petra.proposals.insert(7, membership);
    walter.next_id = walter.next_id.max(8);

    let mut sent = 0;
    for i in 0..OPEN_CARDS_PER_PROPOSER_MAX + 5 {
        let Ok(molt_core::Reply::Proposed { id, .. }) = walter.cmd_propose(Surface::Memory, note(i))
        else {
            continue;
        };
        let p = walter.proposals[&id.0].clone();
        assert!(
            petra.receive_proposed(id.0, p.surface, p.payload, "walter"),
            "card {} was sent and refused",
            id.0
        );
        sent += 1;
    }
    assert_eq!(sent, OPEN_CARDS_PER_PROPOSER_MAX - 1, "the membership card holds one slot");
    assert_eq!(open_by(&petra, "walter"), OPEN_CARDS_PER_PROPOSER_MAX);
}

#[test]
fn status_names_the_own_open_count_and_the_cap() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    propose(&mut walter, Surface::Memory, note(0));
    propose(&mut walter, Surface::Memory, note(1));
    let id = walter.next_id + 10;
    walter
        .proposals
        .insert(id, ProposalRecord::new(Surface::Memory, note(9), "petra".to_string()));
    let s = walter.status();
    assert_eq!(s.open_proposals, 2, "own cards only");
    assert_eq!(s.open_proposals_max, 64);
    let wire = serde_json::to_value(&s).expect("json");
    assert_eq!(wire["open_proposals"], json!(2));
    assert_eq!(wire["open_proposals_max"], json!(64));
}
