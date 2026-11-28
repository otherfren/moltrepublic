// SPDX-License-Identifier: GPL-3.0-or-later
//! A files vote whose share is gone on the voting seat: a 2-of-3 where the
//! proposer's seat was restored on a new device after proposing a persist.

use crate::chain::test_support::*;
use molt_core::{ChatMessage, FileMeta, MessageId, MoltError, ProposalId, ProposalState, Surface, WorkspaceEvent};
use serde_json::json;

const SEATS3: [&str; 3] = ["a", "b", "c"];

fn share_msg(id: MessageId, from: &str) -> ChatMessage {
    let mut msg = ChatMessage::text(id, from, "a share", crate::now_secs());
    msg.file = Some(FileMeta {
        name: "a.pdf".to_string(),
        size: 7,
        kind: "PDF".to_string(),
        modified: 100,
        available: true,
        checksum: "ab".repeat(32),
        key_b64: String::new(),
        pieces: 0,
        root: String::new(),
    });
    msg
}

/// Per-receiver wire seqs, one counter per sender.
#[derive(Default)]
struct Seqs(std::collections::HashMap<(String, String), u64>);

impl Seqs {
    fn send(&mut self, to: &mut crate::State, from: &str, body: WorkspaceEvent) {
        let n = self.0.entry((to.member(), from.to_string())).or_default();
        *n += 1;
        wire(to, from, *n, body);
    }

    fn reserve(&mut self, from: &crate::State, to: &mut crate::State) {
        for body in from.open_governance_events() {
            self.send(to, &from.member(), body);
        }
    }
}

struct Field {
    a: crate::State,
    b: crate::State,
    /// c's NEW device: the chain, nothing of the old log.
    c: crate::State,
    pid: ProposalId,
    share: MessageId,
    seqs: Seqs,
    dir: tempfile::TempDir,
}

/// c shares a file and proposes its persist (co-signing at height 1), a
/// and b see both; then c's seat is restored (the Restored block takes
/// height 1), a approves at height 2, b declines, and c's new device gets
/// the open card re-served.
fn restored_proposer() -> Field {
    let dir = tempfile::tempdir().expect("tmp");
    let mut bld = Builder::new(&SEATS3, 2);
    let mut a = genesis_seat("a", &bld, bld.blocks.clone());
    let mut b = genesis_seat("b", &bld, bld.blocks.clone());
    let mut c_old = genesis_seat("c", &bld, bld.blocks.clone());
    let mut seqs = Seqs::default();

    let share = MessageId([5u8; 16]);
    let path = dir.path().join("a.pdf");
    std::fs::write(&path, b"minutes").expect("write");
    // One message for all seats: its stamp is part of the share identity.
    let msg = share_msg(share, "c");
    let env = c_old.make_env("c".to_string(), WorkspaceEvent::Chat(msg.clone()));
    c_old.apply(&env);
    c_old.files.share_paths.insert(share, path);
    for peer in [&mut a, &mut b] {
        seqs.send(peer, "c", WorkspaceEvent::Chat(msg.clone()));
    }
    let pid = ProposalId(propose(&mut c_old, Surface::Files, json!({"op": "persist", "id": share.to_string()})));
    let payload = c_old.proposals[&pid.0].payload.clone();
    let own = c_old.chain.pending_sigs[&pid.0].clone();
    assert_eq!(own.height, 1, "the proposer co-signs at head+1");
    for peer in [&mut a, &mut b] {
        seqs.send(peer, "c", WorkspaceEvent::Proposed { id: pid, surface: Surface::Files, payload: payload.clone() });
        for s in &own.sigs {
            seqs.send(peer, "c", WorkspaceEvent::Approved { id: pid, by: s.member.clone(), height: own.height, sig: s.sig.clone() });
        }
    }
    assert_eq!(a.chain_approval_count(pid.0), 1, "c's co-signature reached a");
    drop(c_old);

    bld.commit_restored("c", &molt_net::nostr_identity(&[9u8; 32], "r").1, &["a", "b"]);
    let restored = bld.blocks.last().expect("restored").clone();
    a.receive_block(restored.clone());
    b.receive_block(restored);
    assert_eq!(a.chain_approval_count(pid.0), 0, "c's height-1 signature is stale at height 2");

    a.cmd_approve(pid, None).expect("a still holds the share");
    b.cmd_decline(pid, None).expect("b declines");
    seqs.send(&mut a, "b", WorkspaceEvent::Declined { id: pid, by: "b".to_string(), hash: crate::State::decline_payload_hash(&payload) });
    let mut c = genesis_seat("c", &bld, bld.blocks.clone());
    seqs.reserve(&a, &mut c);
    seqs.reserve(&b, &mut c);
    Field { a, b, c, pid, share, seqs, dir }
}

/// The field shape: one approval (a), one decline (b), the proposer open.
#[test]
fn a_restored_proposer_holds_its_card_open_with_its_own_voice_gone() {
    let f = restored_proposer();
    let p = &f.c.proposals[&f.pid.0];
    assert_eq!(p.state, ProposalState::Proposed);
    assert_eq!(p.by, "c", "the re-served card names its minter");
    let v = f.c.view(f.pid.0, p);
    let stance = |m: &str| v.votes.iter().find(|x| x.member == m).map(|x| x.vote);
    assert_eq!(stance("a"), Some(molt_core::VoteState::Approved));
    assert_eq!(stance("b"), Some(molt_core::VoteState::Declined));
    assert_eq!(stance("c"), Some(molt_core::VoteState::Open), "the old device's signature died with its height");
    assert!(!f.c.chain.own_approvals.contains(&f.pid.0), "the new device never decided it");
    assert!(f.c.chat_by_id(&f.share).is_err(), "the share message stayed on the old device");
}

/// The approve refusal on c names the way out.
#[test]
fn the_restored_proposer_cannot_approve_and_is_told_what_to_do() {
    let mut f = restored_proposer();
    match f.c.cmd_approve(f.pid, None) {
        Err(e) => assert_eq!(e.to_string(), "share is gone here - decline or withdraw this vote"),
        Ok(r) => panic!("c holds no share to sign for: {r:?}"),
    }
}

/// What a and b answer on the same card.
#[test]
fn the_other_seats_answer_as_before() {
    let mut f = restored_proposer();
    assert!(f.a.cmd_approve(f.pid, None).is_ok(), "a repeat approve re-answers the record");
    assert!(matches!(f.a.cmd_withdraw(f.pid, None), Err(MoltError::NotTheProposer(_))));
    assert!(matches!(f.b.cmd_decline(f.pid, None), Err(MoltError::AlreadyDeclined(_))));
    assert!(matches!(f.b.cmd_withdraw(f.pid, None), Err(MoltError::NotTheProposer(_))));
    if let Err(e) = f.b.cmd_approve(f.pid, None) {
        panic!("b's approve replaces its decline: {e:?}");
    }
}

/// Withdraw on the restored proposer: the card turns terminal everywhere.
#[test]
fn the_restored_proposer_withdraws_its_card() {
    let mut f = restored_proposer();
    f.c.cmd_withdraw(f.pid, None).expect("the proposer pulls back its own card");
    assert!(f.c.proposals[&f.pid.0].withdrawn);
    let body = WorkspaceEvent::Withdrawn { id: f.pid, by: "c".to_string() };
    f.seqs.send(&mut f.a, "c", body.clone());
    f.seqs.send(&mut f.b, "c", body);
    for st in [&f.a, &f.b] {
        assert!(st.proposals[&f.pid.0].withdrawn, "{} sees it pulled back", st.member());
    }
}

/// The workaround on a 2-of-3: the proposer's decline is the second one -
/// n - m + 1 = 2 declines reject the card, share or no share.
#[test]
fn the_proposers_decline_is_the_second_and_rejects_the_card() {
    let mut f = restored_proposer();
    f.c.cmd_decline(f.pid, None).expect("the proposer may decline its own card");
    assert_eq!(f.c.proposals[&f.pid.0].state, ProposalState::Rejected);
    let p = f.c.proposals[&f.pid.0].clone();
    let hash = crate::State::decline_payload_hash(&p.payload);
    let body = WorkspaceEvent::Declined { id: f.pid, by: "c".to_string(), hash };
    f.seqs.send(&mut f.a, "c", body.clone());
    f.seqs.send(&mut f.b, "c", body);
    for st in [&f.a, &f.b] {
        assert_eq!(st.proposals[&f.pid.0].state, ProposalState::Rejected, "{}", st.member());
    }
}

/// The other workaround: an approver's decline retracts its signature and
/// counts as a decline - with b's that is two.
#[test]
fn an_approver_takes_its_approval_back_by_declining() {
    let mut f = restored_proposer();
    f.a.cmd_decline(f.pid, None).expect("decline over the own approval");
    assert_eq!(f.a.chain_approval_count(f.pid.0), 0, "the signature is retracted");
    assert_eq!(f.a.proposals[&f.pid.0].state, ProposalState::Rejected, "a's and b's declines reject it");
}

/// The view marks the card moot on the seat that cannot approve it, and
/// only there.
#[test]
fn the_card_reads_moot_where_the_share_is_gone() {
    let f = restored_proposer();
    let on = |st: &crate::State| st.view(f.pid.0, &st.proposals[&f.pid.0]).moot;
    assert_eq!(on(&f.c).as_deref(), Some("gone"));
    assert_eq!(on(&f.a), None, "a holds the share");
    assert_eq!(on(&f.b), None);
}

/// A moot vote blocks no new persist: the share shared again is a new id,
/// and the restored proposer puts it to the vote.
#[test]
fn a_moot_vote_blocks_no_new_persist() {
    let mut f = restored_proposer();
    let again = MessageId([6u8; 16]);
    let path = f.dir.path().join("again.pdf");
    std::fs::write(&path, b"minutes").expect("write");
    let env = f.c.make_env("c".to_string(), WorkspaceEvent::Chat(share_msg(again, "c")));
    f.c.apply(&env);
    f.c.files.share_paths.insert(again, path);
    propose(&mut f.c, Surface::Files, json!({"op": "persist", "id": again.to_string()}));
}

/// Each reason a seat can never approve a persist vote, and a live share
/// that is no reason at all.
#[test]
fn the_moot_reason_names_why_this_seat_cannot_approve() {
    let mut st = crate::tests::plain_state();
    let card = |st: &mut crate::State, n: u8, ts: u64| {
        let id = MessageId([n; 16]);
        let mut msg = share_msg(id, "peer-1");
        msg.ts = ts;
        let env = st.make_env("peer-1".to_string(), WorkspaceEvent::Chat(msg));
        st.apply(&env);
        let payload = json!({
            "op": "persist", "id": id.to_string(), "by": "peer-1", "name": "a.pdf", "kind": "PDF",
            "size": 7, "checksum": "ab".repeat(32), "shared_ts": ts
        });
        st.proposals.insert(u64::from(n), molt_core::ProposalRecord::new(Surface::Files, payload.clone(), "peer-1".to_string()));
        (u64::from(n), payload)
    };
    let moot = |st: &crate::State, pid: u64| st.view(pid, &st.proposals[&pid]).moot;
    let now = crate::now_secs();

    let (live, _) = card(&mut st, 1, now);
    assert_eq!(moot(&st, live), None, "a live share is votable");

    let (old, _) = card(&mut st, 2, now - 30 * 86_400);
    assert_eq!(moot(&st, old).as_deref(), Some("expired"));

    let (pinned, block) = card(&mut st, 3, now);
    st.applied.entry(Surface::Files).or_default().push((None, block));
    assert_eq!(moot(&st, pinned).as_deref(), Some("persistent"));

    let (gone, mut del) = card(&mut st, 4, now);
    del["op"] = json!("delete");
    st.applied.entry(Surface::Files).or_default().push((None, del));
    assert_eq!(moot(&st, gone).as_deref(), Some("deleted"));

    st.proposals.get_mut(&old).expect("card").state = ProposalState::Rejected;
    assert_eq!(moot(&st, old), None, "a decided card is never moot");
}
