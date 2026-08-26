// SPDX-License-Identifier: GPL-3.0-or-later

//! Tests for [`super::governance`]: approvals, declines, withdraws,
//! sealing, the re-base and the open-governance re-serve.

use super::test_support::*;
use super::*;
use super::governance::OPEN_CARDS_PER_PROPOSER_MAX;
use molt_core::{ChainChange, MembershipOp, Surface};
use molt_storage::identity_sign;
use serde_json::json;

/// Attach real (temp-dir) storage to a test peer; `dead_writer` closes
/// the writer first, so every blocking persist honestly reports `false`.
fn attach_storage(peer: &mut crate::State, dead_writer: bool) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tmp");
    let seed =
        molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("phrase"))
            .expect("entropy");
    let genesis = molt_core::EventEnvelope {
        prev_seq: 0,
        seq: 1,
        ts: 10,
        by: "walter".to_string(),
        body: molt_core::WorkspaceEvent::Founded {
            name: "Chess Club".to_string(),
            rule_m: 2,
            rule_n: 2,
            member: "walter".to_string(),
            roster: vec!["petra".to_string(), "walter".to_string()],
            identities: Vec::new(),
            attestations: Vec::new(),
            republic_id: String::new(),
            agenda: String::new(),
            relays: Vec::new(),
            features: None,
        },
    };
    let ws = molt_storage::create_workspace(tmp.path(), &seed, &genesis).expect("create");
    let dir = ws.dir().to_path_buf();
    let handle = molt_storage::start_writer(ws);
    if dead_writer {
        handle.clone().close(None);
    }
    peer.active = Some(crate::ActiveStorage {
        id: "w-h3".to_string(),
        dir,
        prefs: molt_core::WorkspacePrefs::default(),
        handle,
    });
    tmp
}

/// **H3 second half (total_review.md): the governance broadcast waits
/// for the durable persist.** A threshold-sealed block whose write did
/// NOT reach the disk is still appended and projected locally — the
/// signatures are real — but it is NOT broadcast: no `Committed`
/// envelope, no decision summary. The peers seal the byte-identical
/// block from the approval gossip themselves; this node must not
/// spread history it does not durably hold. A durable seal broadcasts
/// exactly as before.
#[test]
fn a_block_that_missed_the_disk_is_not_broadcast() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let genesis = b.blocks.clone();
    b.commit_applied(1, &["petra", "walter"]);
    let block = b.blocks[1].clone();

    // (1) the writer is gone — sealed, appended, NOT broadcast
    let mut peer = chain_peer("walter", &b, genesis.clone());
    let _tmp = attach_storage(&mut peer, true);
    let seq_before = peer.next_seq;
    let chat_before = peer.chat.len();
    peer.adopt_committed_block(block.clone(), 1);
    assert_eq!(peer.chain.len(), 2, "the sealed block is appended locally");
    assert_eq!(
        peer.next_seq, seq_before,
        "no envelope may be minted for a block the disk never took"
    );
    assert_eq!(peer.chat.len(), chat_before, "no decision summary either");

    // (2) the writer lives — durable, broadcast as before
    let mut peer = chain_peer("petra", &b, genesis);
    let _tmp = attach_storage(&mut peer, false);
    let seq_before = peer.next_seq;
    peer.adopt_committed_block(block, 1);
    assert_eq!(peer.chain.len(), 2);
    assert!(
        peer.next_seq > seq_before,
        "a durable seal broadcasts its Committed envelope"
    );
    peer.active.take().expect("active").handle.close(None);
}

/// WP3, the wire side of the decodability gate: a peer's `set_image`
/// gossip with undecodable bytes is dropped with a warning, never
/// recorded as a pending proposal (convergence before enforcement —
/// the same posture as the byte-cap guard it extends).
#[test]
fn an_undecodable_peer_set_image_is_dropped_not_recorded() {
    use base64::Engine as _;
    let b = Builder::new(&["petra", "walter"], 2);
    let mut peer = chain_peer("walter", &b, b.blocks.clone());
    let deliver = |peer: &mut crate::State, id: u64, b64: String| {
        let env = molt_core::EventEnvelope { prev_seq: 0,
            seq: 90 + id,
            ts: 1_751_000_000,
            by: "petra".to_string(),
            body: WorkspaceEvent::Proposed {
                id: ProposalId(id),
                surface: Surface::Organization,
                payload: json!({ "op": "set_image", "value": "x.png", "bytes_b64": b64 }),
            },
        };
        peer.cmd_net_delivered("petra".to_string(), env, None)
            .expect("a wire drop acks, never errors");
    };
    // garbage within the byte cap: dropped, not recorded
    let garbage = base64::engine::general_purpose::STANDARD.encode(b"not an image");
    deliver(&mut peer, 9, garbage);
    assert!(
        !peer.proposals.contains_key(&9),
        "undecodable peer bytes must never become a pending proposal"
    );
    // a real 2x2 png is recorded
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
    deliver(&mut peer, 10, png.to_string());
    assert!(
        peer.proposals.contains_key(&10),
        "a decodable peer set_image is recorded as pending"
    );
}

/// **Self-edit, on the wire** (`member_profiles_plan.md` §2): a member
/// profile belongs to its member, and the link identity is the only
/// proof of authorship — so a profile proposal claiming ANOTHER seat is
/// dropped, never recorded. Node-independent like the drops beside it,
/// so every honest holder drops the same frame. The picture ops carry
/// the decodable+square verdict onto the wire too.
#[test]
fn a_profile_proposal_claiming_another_member_is_dropped() {
    use base64::Engine as _;
    let b = Builder::new(&["petra", "walter"], 2);
    let mut peer = chain_peer("walter", &b, b.blocks.clone());
    let deliver = |peer: &mut crate::State, id: u64, from: &str, payload: serde_json::Value| {
        let env = molt_core::EventEnvelope {
            prev_seq: 0,
            seq: 300 + id,
            ts: 1_751_000_000,
            by: from.to_string(),
            body: WorkspaceEvent::Proposed {
                id: ProposalId(id),
                surface: Surface::Organization,
                payload,
            },
        };
        peer.cmd_net_delivered(from.to_string(), env, None)
            .expect("a wire drop acks, never errors");
    };
    // A profile op arriving under ANOTHER member's link is the normal
    // WP2 shape, not a forgery: `serve_open_governance` re-serves every
    // open card under the SERVING peer's identity (make_env(me, body)),
    // so a catching-up holder meets walter's edit with from = petra.
    // Dropping on `payload.member != from` blinded exactly that holder -
    // it could never see, let alone vote on, another seat's profile
    // proposal. Authorship is unauthenticated by design here
    // (`ProposalRecord.by` is a DISPLAY hint); the self-edit rule is
    // enforced where it IS decidable, at the propose gate.
    deliver(&mut peer, 20, "petra", json!({ "op": "set_member_desc", "member": "walter", "value": "hi" }));
    assert!(
        peer.proposals.contains_key(&20),
        "a re-served profile card must reach a catching-up holder"
    );
    // petra editing her own: recorded
    deliver(&mut peer, 21, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": "hi" }));
    assert!(peer.proposals.contains_key(&21), "a member's own profile edit is recorded");
    // what IS node-independently decidable: the seat must exist. A
    // profile op for a stranger could never fold onto anything
    deliver(&mut peer, 26, "petra", json!({ "op": "set_member_desc", "member": "ghost", "value": "hi" }));
    assert!(
        !peer.proposals.contains_key(&26),
        "a profile op for a seat that is not in the roster is dropped"
    );
    deliver(&mut peer, 27, "petra", json!({ "op": "set_member_desc", "value": "hi" }));
    assert!(
        !peer.proposals.contains_key(&27),
        "a profile op naming no seat is dropped"
    );
    // the picture ops carry the square rule onto the wire
    let square = base64::engine::general_purpose::STANDARD
        .encode(crate::tests::tiny_bmp_header(2, 2));
    let wide = base64::engine::general_purpose::STANDARD
        .encode(crate::tests::tiny_bmp_header(4, 2));
    deliver(&mut peer, 22, "petra", json!({ "op": "set_member_image", "member": "petra", "value": "f.bmp", "bytes_b64": wide }));
    assert!(!peer.proposals.contains_key(&22), "a non-square peer avatar is dropped");
    deliver(&mut peer, 23, "petra", json!({ "op": "set_member_image", "member": "petra", "value": "f.bmp", "bytes_b64": square }));
    assert!(peer.proposals.contains_key(&23), "a square, decodable peer avatar is recorded");
    // the length cap is a contract, not a local preference: a description
    // the propose gate refuses must not walk in through the wire door
    let long = "x".repeat(crate::proposals::DESC_MAX + 1);
    deliver(&mut peer, 24, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": long }));
    assert!(
        !peer.proposals.contains_key(&24),
        "an over-long description is dropped at the wire too"
    );
    let edge = "x".repeat(crate::proposals::DESC_MAX);
    deliver(&mut peer, 25, "petra", json!({ "op": "set_member_desc", "member": "petra", "value": edge }));
    assert!(peer.proposals.contains_key(&25), "a description at the limit is recorded");
}

/// **The proposer pulls a proposal back** (the ProposalCard's "pull
/// back"): terminal like a rejection, but no vote is forged — the
/// verdict is `withdrawn`, never "declined by".
#[test]
fn a_withdraw_turns_the_card_terminal_without_forging_a_vote() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    let id = match peer
        .cmd_propose(
            Surface::Organization,
            json!({ "op": "set_name", "value": "Mine" }),
        )
        .expect("propose")
    {
        molt_core::Reply::Proposed { id } => id,
        other => panic!("unexpected reply {other:?}"),
    };
    peer.cmd_withdraw(id).expect("withdraw");
    let p = peer.proposals.get(&id.0).expect("card");
    assert_eq!(p.state, ProposalState::Rejected);
    assert!(p.withdrawn, "the verdict is its own, not a decline");
    assert!(p.decliners.is_empty(), "no vote forged");
    assert_eq!(p.declined_by, "", "no decliner named");
    assert!(
        !peer.pending_sigs.contains_key(&id.0),
        "collected signatures are cleared"
    );
    // terminal: a second withdraw refuses
    assert!(peer.cmd_withdraw(id).is_err());
}

/// Only the proposer withdraws: the local command refuses a foreign
/// card, and the wire arm counts a withdraw only when the link
/// identity IS the recorded proposer (no signature — same posture as
/// declines, plus the proposer check).
#[test]
fn only_the_proposer_may_withdraw() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(9),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Petras" }),
        },
    );
    // walter is not the proposer — the command refuses
    assert!(matches!(
        peer.cmd_withdraw(ProposalId(9)),
        Err(molt_core::MoltError::NotTheProposer(_))
    ));
    // forgery: dora's link carries petra's withdraw — dropped
    wire(
        &mut peer,
        "dora",
        1,
        WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
    );
    assert_eq!(
        peer.proposals.get(&9).expect("card").state,
        ProposalState::Proposed
    );
    // dora withdrawing petra's card as herself — not the proposer, dropped
    wire(
        &mut peer,
        "dora",
        2,
        WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "dora".to_string() },
    );
    assert_eq!(
        peer.proposals.get(&9).expect("card").state,
        ProposalState::Proposed
    );
    // the real proposer pulls it back
    wire(
        &mut peer,
        "petra",
        2,
        WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
    );
    let p = peer.proposals.get(&9).expect("card");
    assert_eq!(p.state, ProposalState::Rejected);
    assert!(p.withdrawn);
}

/// A withdraw ahead of its proposal parks (G7 orders per sender only)
/// and lands the moment the card arrives — the verdict must never be
/// lost to arrival order, exactly like a parked decline.
#[test]
fn a_withdraw_ahead_of_its_proposal_parks_and_registers() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Withdrawn { id: ProposalId(9), by: "petra".to_string() },
    );
    wire(
        &mut peer,
        "petra",
        2,
        WorkspaceEvent::Proposed {
            id: ProposalId(9),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Gone" }),
        },
    );
    let p = peer.proposals.get(&9).expect("card");
    assert_eq!(p.state, ProposalState::Rejected);
    assert!(p.withdrawn, "the parked withdraw registered on arrival");
}

/// The own withdraw re-serves with the open governance — a peer that
/// was closed while the card died must still learn the verdict.
#[test]
fn open_governance_reserves_the_own_withdraw() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    let id = match peer
        .cmd_propose(
            Surface::Organization,
            json!({ "op": "set_name", "value": "Short-lived" }),
        )
        .expect("propose")
    {
        molt_core::Reply::Proposed { id } => id,
        other => panic!("unexpected reply {other:?}"),
    };
    peer.cmd_withdraw(id).expect("withdraw");
    // keep the card inside the display retention (fixture ts is historic)
    peer.proposals.get_mut(&id.0).expect("card").declined_at = crate::now_secs();
    let events = peer.open_governance_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            WorkspaceEvent::Withdrawn { id: wid, by } if *wid == id && by == "walter"
        )),
        "the own withdraw re-serves"
    );
}

/// Live incident 2026-08-09 (defect 6): a decline is a VOTE — it must
/// converge like an approval. Two wire declines in a 2-of-3 kill the
/// proposal on every node; before the receive arm existed they were
/// acked and DROPPED, so a majority-declined vote stayed pending forever
/// on every node but the decliner's own.
#[test]
fn wire_declines_converge_and_reject_at_the_veto_threshold() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(9),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "New Name" }),
        },
    );
    wire(
        &mut peer,
        "dora",
        1,
        WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
    );
    let p = peer.proposals.get(&9).expect("registered");
    assert_eq!(p.decliners, vec!["dora".to_string()], "the wire decline counts");
    assert_eq!(p.state, ProposalState::Proposed, "one decline in 2-of-3 leaves room");
    // forgery: the body claims dora again, but the link says petra — dropped,
    // a peer can only ever decline as itself
    wire(
        &mut peer,
        "petra",
        2,
        WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
    );
    let p = peer.proposals.get(&9).expect("still there");
    assert_eq!(p.decliners.len(), 1, "a decline must carry its link identity");
    // a duplicate of dora's decline (resend) stays ONE voice
    wire(
        &mut peer,
        "dora",
        2,
        WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
    );
    assert_eq!(peer.proposals.get(&9).expect("still there").decliners.len(), 1);
    // petra's real decline tips it: 2 > n − m = 1 → Rejected
    wire(
        &mut peer,
        "petra",
        3,
        WorkspaceEvent::Declined { id: ProposalId(9), by: "petra".to_string(), hash: String::new() },
    );
    let p = peer.proposals.get(&9).expect("still there");
    assert_eq!(p.state, ProposalState::Rejected, "a majority decline is terminal");
    assert_eq!(p.declined_by, "petra", "the tipping decliner is named");
    assert!(p.declined_at > 0, "the decline timestamp is the envelope's");
}

/// D7: a decline the FULL park would shed must stay UNACKED — the
/// accept point ran before the park admission, so a shed voice was
/// ACKed and the at-least-once guarantee was already spent on it: the
/// sender trims it and the voice is gone for good. Left unacked, the
/// resend machinery re-earns it once the park has room (or the
/// proposal lands). The implausible-id garbage case deliberately stays
/// accept-and-drop — a u64::MAX decline must not ride resend forever.
#[test]
fn a_shed_decline_stays_unacked_for_the_resend() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let base = walter.next_id;
    let per_member = u64::try_from(crate::proposals::PARKED_DECLINES_PER_MEMBER_MAX)
        .expect("cap fits");
    // fill petra's whole per-member allowance with plausible unknown ids
    for i in 0..per_member {
        wire(
            &mut walter,
            "petra",
            i + 1,
            WorkspaceEvent::Declined { id: ProposalId(base + i), by: "petra".to_string(), hash: String::new() },
        );
    }
    let accepted = |st: &crate::State, seq: u64| {
        st.accepted.get("petra").is_some_and(|w| w.is_accepted(seq))
    };
    assert!(accepted(&walter, per_member), "parked voices are accepted and acked");
    // the voice the park sheds must NOT be marked accepted
    wire(
        &mut walter,
        "petra",
        per_member + 1,
        WorkspaceEvent::Declined {
            id: ProposalId(base + per_member),
            by: "petra".to_string(),
            hash: String::new(),
        },
    );
    assert!(
        !accepted(&walter, per_member + 1),
        "a shed voice stays unacked so the resend re-earns it"
    );
    // …while garbage far past the mint window stays accept-and-drop
    wire(
        &mut walter,
        "petra",
        per_member + 2,
        WorkspaceEvent::Declined { id: ProposalId(u64::MAX), by: "petra".to_string(), hash: String::new() },
    );
    assert!(
        accepted(&walter, per_member + 2),
        "implausible-id garbage is accepted and dropped, never resent"
    );
}

/// D4: a park drain speaks with EVERY drained voice — one
/// `Event::Declined` per registered member (never one event naming
/// `decliners.last()`), and a drain that tips emits the voices AND the
/// `Rejected`. An event-stream consumer must not undercount votes.
#[test]
fn a_park_drain_emits_one_declined_event_per_voice() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    // veto_room = 4 - 2 = 2: two parked voices stay a Voice drain
    let b = Builder::new(&["petra", "walter", "dora", "erika"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut walter,
        "dora",
        1,
        WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
    );
    wire(
        &mut walter,
        "erika",
        1,
        WorkspaceEvent::Declined { id: ProposalId(4), by: "erika".to_string(), hash: String::new() },
    );
    let mut ev = walter.subscribe_events();
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(4),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Late" }),
        },
    );
    let mut declined: Vec<String> = Vec::new();
    let mut rejected = 0;
    while let Ok(e) = ev.try_recv() {
        match e {
            crate::Event::Declined { id, by } if id.0 == 4 => declined.push(by),
            crate::Event::Rejected { id } if id.0 == 4 => rejected += 1,
            _ => {}
        }
    }
    declined.sort_unstable();
    assert_eq!(
        declined,
        vec!["dora".to_string(), "erika".to_string()],
        "one event per drained voice"
    );
    assert_eq!(rejected, 0, "two voices in veto room 2 do not tip");

    // …and a drain that TIPS still speaks every voice, then the verdict
    let mut peer = chain_signer("walter", &b, b.blocks.clone());
    for (i, who) in ["dora", "erika", "petra"].iter().enumerate() {
        wire(
            &mut peer,
            who,
            u64::try_from(i).expect("i") + 1,
            WorkspaceEvent::Declined {
                id: ProposalId(4),
                by: (*who).to_string(),
                hash: String::new(),
            },
        );
    }
    let mut ev = peer.subscribe_events();
    wire(
        &mut peer,
        "petra",
        2,
        WorkspaceEvent::Proposed {
            id: ProposalId(4),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Late" }),
        },
    );
    let (mut declined, mut rejected) = (0, 0);
    while let Ok(e) = ev.try_recv() {
        match e {
            crate::Event::Declined { id, .. } if id.0 == 4 => declined += 1,
            crate::Event::Rejected { id } if id.0 == 4 => rejected += 1,
            _ => {}
        }
    }
    assert_eq!((declined, rejected), (3, 1), "3 voices + the verdict");
}

/// D5: the decision line of a DECLINED vote is minted under a
/// DETERMINISTIC message id — whoever tips posts it, concurrent
/// posters collapse via the ordinary duplicate-id drop, and a wire tip
/// posts too (it used to stay silent, so a vote tipped by a received
/// decline had no decision line anywhere).
#[test]
fn a_wire_tipped_decline_posts_its_summary_exactly_once() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    // veto_room = 3 - 2 = 1: the SECOND decline tips
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(4),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "X" }),
        },
    );
    let summaries = |st: &crate::State| {
        st.chat_visible()
            .filter(|m| {
                m.kind == molt_core::ChatKind::System
                    && matches!(&m.channel, molt_core::ChannelRef::Patch { id } if id.0 == 4)
            })
            .count()
    };
    walter.cmd_decline(ProposalId(4)).expect("own voice, no tip");
    assert_eq!(summaries(&walter), 0, "one voice does not decide");
    wire(
        &mut walter,
        "dora",
        1,
        WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
    );
    assert_eq!(
        summaries(&walter),
        1,
        "the wire tip posts the decision line"
    );
    // the OTHER tipper's copy arrives under the SAME deterministic id —
    // the ordinary duplicate-id drop collapses it
    let sid = crate::chat::decision_summary_id(&b.republic_id, 4, true);
    let copy = molt_core::ChatMessage::text(sid, "dora".to_string(), "⚖ #4 ⊘ …".to_string(), crate::now_secs())
        .with_channel(molt_core::ChannelRef::Patch { id: ProposalId(4) })
        .with_kind(molt_core::ChatKind::System);
    wire(&mut walter, "dora", 2, WorkspaceEvent::Chat(copy));
    assert_eq!(summaries(&walter), 1, "concurrent posters collapse to one line");
}

/// D1: a decline binds the payload the decliner SAW, not a bare id —
/// two proposers minting the same id in one gossip round-trip must
/// not let a voice register against a proposal the decliner never
/// judged. An empty hash (older sender) keeps id-only semantics, and
/// the park stores the hash so a drained voice is checked too.
#[test]
fn a_decline_carrying_a_foreign_payload_hash_does_not_register() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(4),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "X" }),
        },
    );
    let h = |v: &serde_json::Value| crate::State::decline_payload_hash(v);
    wire(
        &mut walter,
        "dora",
        1,
        WorkspaceEvent::Declined {
            id: ProposalId(4),
            by: "dora".to_string(),
            hash: h(&json!({ "op": "set_name", "value": "Y" })),
        },
    );
    let p = walter.proposals.get(&4).expect("card");
    assert!(p.decliners.is_empty(), "a mismatching hash must not register");
    wire(
        &mut walter,
        "dora",
        2,
        WorkspaceEvent::Declined {
            id: ProposalId(4),
            by: "dora".to_string(),
            hash: h(&json!({ "op": "set_name", "value": "X" })),
        },
    );
    assert_eq!(
        walter.proposals.get(&4).expect("card").decliners,
        vec!["dora".to_string()],
        "the matching hash registers"
    );
    wire(
        &mut walter,
        "petra",
        2,
        WorkspaceEvent::Declined {
            id: ProposalId(4),
            by: "petra".to_string(),
            hash: String::new(),
        },
    );
    assert_eq!(
        walter.proposals.get(&4).expect("card").decliners.len(),
        2,
        "an empty hash (older sender) keeps id-only semantics"
    );
    // the PARK stores the hash: a parked mismatch never registers either
    wire(
        &mut walter,
        "dora",
        3,
        WorkspaceEvent::Declined {
            id: ProposalId(9),
            by: "dora".to_string(),
            hash: h(&json!({ "op": "set_name", "value": "Z" })),
        },
    );
    wire(
        &mut walter,
        "petra",
        3,
        WorkspaceEvent::Proposed {
            id: ProposalId(9),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "W" }),
        },
    );
    assert!(
        walter.proposals.get(&9).expect("card").decliners.is_empty(),
        "a drained parked voice is hash-checked too"
    );
}

/// A decline can outrun its proposal on the wire (G7 orders per sender
/// only) and it replays from the own log before a re-served proposal
/// returns: either way it PARKS and registers the moment the proposal
/// is known — a vote must never be lost to arrival order.
#[test]
fn a_decline_ahead_of_its_proposal_parks_and_registers() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    wire(
        &mut peer,
        "dora",
        1,
        WorkspaceEvent::Declined { id: ProposalId(4), by: "dora".to_string(), hash: String::new() },
    );
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Declined { id: ProposalId(4), by: "petra".to_string(), hash: String::new() },
    );
    assert!(peer.proposals.is_empty(), "no card yet - the declines wait");
    wire(
        &mut peer,
        "petra",
        2,
        WorkspaceEvent::Proposed {
            id: ProposalId(4),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Late" }),
        },
    );
    let p = peer.proposals.get(&4).expect("registered");
    assert_eq!(p.decliners.len(), 2, "both parked declines registered");
    assert_eq!(p.state, ProposalState::Rejected, "and they tip it immediately");
}

/// WP2 re-serve carries the OWN decline: a vote against survives RAM
/// loss like a collected signature does, so a rejoiner (or a node whose
/// pre-fix engine dropped the gossip) can still converge. Foreign
/// declines are NOT re-attested — only the link identity vouches a
/// decline — and a REJECTED card still serves the own voice.
#[test]
fn open_governance_reserves_the_own_decline() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    // an open card walter declined (1 ≤ veto room → still Proposed)
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(7),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Open" }),
        },
    );
    peer.cmd_decline(ProposalId(7)).expect("decline");
    // a card that went Rejected (petra's wire decline tips it)
    wire(
        &mut peer,
        "petra",
        2,
        WorkspaceEvent::Proposed {
            id: ProposalId(8),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Dead" }),
        },
    );
    peer.cmd_decline(ProposalId(8)).expect("decline");
    wire(
        &mut peer,
        "petra",
        3,
        WorkspaceEvent::Declined { id: ProposalId(8), by: "petra".to_string(), hash: String::new() },
    );
    assert_eq!(
        peer.proposals.get(&8).expect("card").state,
        ProposalState::Rejected
    );
    // the fixture's wire ts is historic — stamp the decline fresh, or
    // the retention gate below would age the card out immediately
    peer.proposals.get_mut(&8).expect("card").declined_at = crate::now_secs();
    // a parked own decline (own-log replay raced a re-served proposal)
    peer.register_decline(11, "walter", 1_751_000_000, "");
    let events = peer.open_governance_events();
    let own_declines: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            WorkspaceEvent::Declined { id, by, .. } if by == "walter" => Some(id.0),
            _ => None,
        })
        .collect();
    assert!(own_declines.contains(&7), "the open card's own decline re-serves");
    assert!(own_declines.contains(&8), "the rejected card's own decline re-serves");
    assert!(own_declines.contains(&11), "the parked own decline re-serves");
    assert!(
        !events.iter().any(
            |e| matches!(e, WorkspaceEvent::Declined { by, .. } if by == "petra")
        ),
        "a foreign decline is never re-attested"
    );
    // a rejected card past the display retention has no convergence
    // audience — its voice leaves the batch (review 2026-08-09,
    // finding 12), so the re-serve stays bounded
    peer.proposals.get_mut(&8).expect("card").declined_at = 1;
    let aged: Vec<u64> = peer
        .open_governance_events()
        .iter()
        .filter_map(|e| match e {
            WorkspaceEvent::Declined { id, by, .. } if by == "walter" => Some(id.0),
            _ => None,
        })
        .collect();
    assert!(!aged.contains(&8), "an aged-out rejected voice stops re-serving");
    assert!(aged.contains(&7), "the open card's voice stays");
}

/// Answering a ChainRequest re-records the served Proposed envelopes
/// through the applier — that must not clobber the live card: an
/// unconditional insert wiped every collected foreign decline on the
/// SERVING node (review 2026-08-09, finding 1).
#[test]
fn serving_open_governance_keeps_the_own_cards_decliners() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(9),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Kept" }),
        },
    );
    wire(
        &mut peer,
        "dora",
        1,
        WorkspaceEvent::Declined { id: ProposalId(9), by: "dora".to_string(), hash: String::new() },
    );
    peer.serve_open_governance();
    assert_eq!(
        peer.proposals.get(&9).expect("card").decliners,
        vec!["dora".to_string()],
        "serving must not wipe the collected voices"
    );
}

/// A decline referencing an id far past the mint counter is garbage —
/// parking it would poison `next_id` (one u64::MAX frame froze every
/// later local mint; review 2026-08-09, finding 2).
#[test]
fn a_decline_for_an_implausible_id_is_dropped() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    let before = peer.next_id;
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Declined { id: ProposalId(u64::MAX), by: "petra".to_string(), hash: String::new() },
    );
    assert!(peer.pending_declines.is_empty(), "garbage never parks");
    assert_eq!(peer.next_id, before, "and never moves the mint counter");
}

/// Decline-after-approve stays ALLOWED — it is how a proposer
/// withdraws (the auto-cosign would otherwise lock every proposal
/// open); the summary test pins the terminal effect. The view still
/// reports the own stance so frontends can gray per what the engine
/// actually refuses (approve-after-decline, re-decline).
#[test]
fn a_decline_after_the_own_approval_still_works() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    peer.identity_sk = b
        .keys
        .iter()
        .find(|(m, _)| m == "walter")
        .map(|(_, sk)| sk.clone());
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(9),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Erst ja" }),
        },
    );
    peer.cmd_approve(ProposalId(9)).expect("approve signs");
    let p = peer.proposals.get(&9).cloned().expect("card");
    assert!(peer.view(9, &p).approved_by_me, "the signature is collected");
    peer.cmd_decline(ProposalId(9)).expect("the withdrawal path stays open");
    let p = peer.proposals.get(&9).cloned().expect("card");
    let v = peer.view(9, &p);
    assert!(v.declined_by_me, "the stance the frontend grays on");
    // D2 (last vote counts): the decline RETRACTED the collected
    // signature — one member holds one stance, never both
    assert!(
        !peer
            .pending_sigs
            .get(&9)
            .is_some_and(|s| s.sigs.iter().any(|a| a.member == "walter")),
        "the own signature is retracted by the decline"
    );
    assert!(!v.approved_by_me, "…and the view says so");
}

/// L3 headline: `receive_checkpoint_proposal` ran `id + 1` BEFORE any
/// guard — with overflow-checks + panic=abort in release, one hostile
/// frame from any roster peer ABORTED the process. And every wire
/// receive fn bumped the mint counter before its guards, so an
/// in-window absurd id poisoned every later local mint.
#[test]
fn implausible_wire_ids_neither_abort_nor_poison_the_mint() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let before = walter.next_id;
    // the former one-frame remote abort
    walter.receive_checkpoint_proposal(u64::MAX, 0, "00");
    assert_eq!(walter.next_id, before, "no mint poison from the cut");
    // the surface twin
    assert!(!walter.receive_proposed(
        u64::MAX,
        Surface::Memory,
        json!({ "op": "add_note" }),
        "petra"
    ));
    assert_eq!(walter.next_id, before, "no mint poison from a proposal");
    // …and the membership twin
    walter.receive_membership_proposal(
        u64::MAX,
        MembershipOp::Restored,
        "petra",
        &b.pk("petra"),
        None,
        Vec::new(),
        None,
    );
    assert_eq!(walter.next_id, before, "no mint poison from membership");
}

/// L3: signatures collect only for ROSTER members — dedup is by the
/// free-form member string, so distinct fake names grew one Vec
/// without bound (~96 KiB of wire per entry).
#[test]
fn approvals_from_non_members_never_enter_the_pending_set() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 1 }),
        },
    );
    for i in 0..100u64 {
        walter.receive_approval(1, &format!("ghost{i}"), 1, "ff");
    }
    assert!(
        walter.pending_sigs.get(&1).map_or(0, |p| p.sigs.len()) <= 2,
        "ghost names must not grow the set"
    );
}

/// L3: a flooding proposer crowds only ITSELF — the newest own card is
/// refused at the cap, another member's card still lands.
#[test]
fn a_wire_proposal_flood_is_bounded_per_proposer() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    for i in 0..200u64 {
        walter.receive_proposed(
            100 + i,
            Surface::Memory,
            json!({ "op": "add_note", "i": i }),
            "petra",
        );
    }
    let open_petra = walter
        .proposals
        .values()
        .filter(|p| p.state == ProposalState::Proposed && p.by == "petra")
        .count();
    assert_eq!(open_petra, OPEN_CARDS_PER_PROPOSER_MAX, "the cap holds");
    assert!(
        walter.receive_proposed(
            900,
            Surface::Memory,
            json!({ "op": "add_note" }),
            "walter"
        ),
        "another member's honest card still lands"
    );
}

/// L2: the DISPLAYED approval count and pills read only signatures
/// that VERIFY — a peer gossiping junk must not inflate progress or
/// paint a forged stance onto a named seat. Sealing was always safe
/// (`try_commit` filters); this pins the display.
#[test]
fn an_unverifiable_approval_is_not_displayed_as_consent() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let payload = json!({ "op": "add_note", "id": 1 });
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Memory,
            payload: payload.clone(),
        },
    );
    // junk: parses as no valid signature over the approval bytes
    walter.receive_approval(1, "petra", 1, "deadbeef");
    assert_eq!(walter.chain_approval_count(1), 0, "junk shows no progress");
    let p = walter.proposals.get(&1).cloned().expect("card");
    let v = walter.view(1, &p);
    assert_eq!(v.approvals, 0);
    let petra_row = v
        .votes
        .iter()
        .find(|mv| mv.member == "petra")
        .map(|mv| mv.vote)
        .expect("row");
    assert_eq!(petra_row, molt_core::VoteState::Open, "no forged pill");
    // …the genuine signature counts, and the vote still seals (liveness)
    let change = ChainChange::Applied {
        proposal_id: 1,
        surface: Surface::Memory,
        payload: payload.clone(),
    };
    let bytes = approval_bytes(&b.republic_id, 1, &change);
    walter.receive_approval(1, "petra", 1, &identity_sign(b.key("petra"), &bytes));
    assert_eq!(walter.chain_approval_count(1), 1, "the genuine one displays");
    walter.chain_sign_and_gossip_approval(1);
    assert_eq!(
        walter.chain_head.as_ref().expect("head").height,
        1,
        "verification costs no liveness - the block seals"
    );
}

/// **A forged approval under THIS node's name is never re-signed.**
///
/// Review 2026-08-25 (CRITICAL): "this node approved X" was inferred
/// from the wire-collected set, which any member fills with junk under
/// any roster name. At the next re-base the node then signed X with its
/// REAL key — a threshold bypass by one insider, no human decision.
/// The decision register is local: only `cmd_approve`'s own signing
/// path writes it.
#[test]
fn a_forged_own_approval_is_not_re_signed_at_the_rebase() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let hostile = json!({ "op": "add_note", "id": 1 });
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Memory,
            payload: hostile.clone(),
        },
    );
    // petra gossips a junk approval UNDER WALTER'S NAME
    walter.receive_approval(1, "walter", 1, "deadbeef");
    // an unrelated block seals at height 1 (petra + dora) — the re-base
    // sweeps every pending set at the old height
    wire(
        &mut walter,
        "petra",
        2,
        WorkspaceEvent::Proposed {
            id: ProposalId(2),
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 2 }),
        },
    );
    b.commit_applied(2, &["petra", "dora"]);
    walter.receive_block(b.blocks[1].clone());
    assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
    let mine = walter
        .pending_sigs
        .get(&1)
        .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));
    assert!(!mine, "walter never decided on #1 - the re-base must not sign it");
    assert_eq!(walter.chain_approval_count(1), 0, "no forged progress");
}

/// **A retracted approval is not re-signed at the re-base.** D2: a
/// decline retracts this member's signature; the decision register
/// must forget it too, or the next block puts the signature straight
/// back while the member is listed as a decliner.
#[test]
fn a_declined_own_approval_is_not_re_signed_at_the_rebase() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora", "eve", "finn"], 3);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 1 }),
        },
    );
    walter.cmd_approve(ProposalId(1)).expect("walter approves");
    assert!(walter.own_approvals.contains(&1));
    walter.cmd_decline(ProposalId(1)).expect("…then retracts");
    assert!(!walter.own_approvals.contains(&1), "the register forgets");
    wire(
        &mut walter,
        "petra",
        2,
        WorkspaceEvent::Proposed {
            id: ProposalId(2),
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 2 }),
        },
    );
    b.commit_applied(2, &["petra", "dora", "eve"]);
    walter.receive_block(b.blocks[1].clone());
    let mine = walter
        .pending_sigs
        .get(&1)
        .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));
    assert!(!mine, "a retracted approval must not come back at the re-base");
    assert_eq!(walter.chain_approval_count(1), 0);
}

/// The decision register is ephemeral; an own `Approved` replayed from
/// the log (a restart) rebuilds it, an own `Declined` clears it.
#[test]
fn the_own_log_rebuilds_the_decision_register() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    walter.apply(&molt_core::EventEnvelope {
        prev_seq: 0,
        seq: 1,
        ts: 1,
        by: "petra".to_string(),
        body: WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 1 }),
        },
    });
    walter.apply(&molt_core::EventEnvelope {
        prev_seq: 0,
        seq: 2,
        ts: 2,
        by: "walter".to_string(),
        body: WorkspaceEvent::Approved {
            id: ProposalId(1),
            by: "walter".to_string(),
            height: 1,
            sig: "irrelevant-for-the-register".to_string(),
        },
    });
    assert!(walter.own_approvals.contains(&1), "an own Approved rebuilds it");
    walter.apply(&molt_core::EventEnvelope {
        prev_seq: 0,
        seq: 3,
        ts: 3,
        by: "walter".to_string(),
        body: WorkspaceEvent::Declined {
            id: ProposalId(1),
            by: "walter".to_string(),
            hash: String::new(),
        },
    });
    assert!(!walter.own_approvals.contains(&1), "an own Declined clears it");
}

/// **Junk never evicts a verified signature.** "Latest wins" let one
/// insider replace every member's genuine approval with garbage at the
/// same height — a vote that never reaches m anywhere (review
/// 2026-08-25, HIGH).
#[test]
fn junk_does_not_evict_a_verified_signature() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let payload = json!({ "op": "add_note", "id": 1 });
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Memory,
            payload: payload.clone(),
        },
    );
    let change = ChainChange::Applied {
        proposal_id: 1,
        surface: Surface::Memory,
        payload,
    };
    let bytes = approval_bytes(&b.republic_id, 1, &change);
    let genuine = identity_sign(b.key("petra"), &bytes);
    walter.receive_approval(1, "petra", 1, &genuine);
    assert_eq!(walter.chain_approval_count(1), 1);
    // dora (or anyone) gossips junk under petra's name at the same height
    walter.receive_approval(1, "petra", 1, "deadbeef");
    assert_eq!(walter.chain_approval_count(1), 1, "the genuine one stands");
    let kept = walter
        .pending_sigs
        .get(&1)
        .and_then(|p| p.sigs.iter().find(|a| a.member == "petra"))
        .map(|a| a.sig.clone());
    assert_eq!(kept.as_deref(), Some(genuine.as_str()));
}

/// L2 liveness twin: an approval that OUTRAN its card is collected but
/// not displayed, and becomes displayable the moment the card lands —
/// the naive drop-on-unverifiable fix would wedge gossip ordering.
#[test]
fn an_approval_that_outran_its_card_counts_once_the_card_lands() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let payload = json!({ "op": "set_name", "value": "Early" });
    let change = ChainChange::Applied {
        proposal_id: 1,
        surface: Surface::Organization,
        payload: payload.clone(),
    };
    let bytes = approval_bytes(&b.republic_id, 1, &change);
    walter.receive_approval(1, "petra", 1, &identity_sign(b.key("petra"), &bytes));
    assert_eq!(
        walter.chain_approval_count(1),
        0,
        "not verifiable yet - the card has not landed"
    );
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Organization,
            payload,
        },
    );
    assert_eq!(
        walter.chain_approval_count(1),
        1,
        "the card landed - the collected signature displays"
    );
}

/// D2: `try_commit` excludes CURRENT decliners — a stale re-served
/// signature of a member whose standing decline this node holds must
/// not count toward m, or a majority-declined proposal seals on
/// whichever node collected the leftovers.
#[test]
fn a_current_decliners_stale_signature_does_not_seal() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let payload = json!({ "op": "set_name", "value": "Contested" });
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Organization,
            payload: payload.clone(),
        },
    );
    // dora's decline stands…
    wire(
        &mut walter,
        "dora",
        1,
        WorkspaceEvent::Declined { id: ProposalId(1), by: "dora".to_string(), hash: String::new() },
    );
    // …then her STALE signature arrives (re-served by a peer that
    // missed the decline) and collects
    let change = ChainChange::Applied {
        proposal_id: 1,
        surface: Surface::Organization,
        payload: payload.clone(),
    };
    let bytes = approval_bytes(&b.republic_id, 1, &change);
    let dora_sig = identity_sign(b.key("dora"), &bytes);
    walter.receive_approval(1, "dora", 1, &dora_sig);
    // walter co-signs: 2 collected — but dora is a CURRENT decliner
    walter.chain_sign_and_gossip_approval(1);
    assert_eq!(
        walter.chain_head.as_ref().expect("head").height,
        0,
        "no block seals while a counted signer's decline stands"
    );
    assert!(
        matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Proposed),
        "the card stays open"
    );
}

/// D2 (last vote counts, decided 2026-08-16): approving over the own
/// standing decline RETRACTS the decline — the newest stance wins,
/// mirroring the decline's signature retraction. One member, one
/// stance, changeable until the vote seals.
#[test]
fn an_approve_retracts_the_standing_own_decline() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(9),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Beides?" }),
        },
    );
    peer.identity_sk = b
        .keys
        .iter()
        .find(|(m, _)| m == "walter")
        .map(|(_, sk)| sk.clone());
    peer.cmd_decline(ProposalId(9)).expect("decline");
    peer.cmd_approve(ProposalId(9)).expect("the newest stance wins");
    let p = peer.proposals.get(&9).cloned().expect("card");
    let v = peer.view(9, &p);
    assert!(!v.declined_by_me, "the decline is retracted");
    assert!(v.approved_by_me, "…and the approval stands");
    assert!(
        !p.decliners.iter().any(|d| d == "walter"),
        "the decliner list no longer names the member"
    );
}

/// An APPLIED card keeps naming its voters: the sealed block carries
/// the signatures (the ephemeral collection is cleared at commit), so
/// the view reads them from the chain — live incident 2026-08-09,
/// defect 7: the applied history showed "0 approvals, every pill open".
#[test]
fn an_over_subscribed_voter_still_reads_approved_on_the_applied_card() {
    // D6: try_commit seals only the m lowest-named signatures (chain
    // truth), but a voter whose signature fell off the block must not
    // read Open on a vote they cast — the collected set survives as
    // record-side DISPLAY data at the seal.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 1 }),
        },
    );
    peer.cmd_approve(ProposalId(1)).expect("walter signs - 1 of 2 locally");
    // the block seals from the other side, signed by petra and dora
    b.commit_applied(1, &["petra", "dora"]);
    peer.receive_block(b.blocks[1].clone());
    let p = peer.proposals.get(&1).cloned().expect("card");
    assert_eq!(p.state, ProposalState::Applied);
    let v = peer.view(1, &p);
    assert_eq!(v.approvals, 2, "the chain-proven count stays the block's");
    assert!(v.approved_by_me, "walter cast a vote and must see it");
    let walter_row = v
        .votes
        .iter()
        .find(|mv| mv.member == "walter")
        .map(|mv| mv.vote)
        .expect("roster row");
    assert_eq!(
        walter_row,
        molt_core::VoteState::Approved,
        "the over-subscribed voter's pill"
    );
    // a post-seal approval must feed the display, never resurrect the
    // ephemeral collection on a terminal card
    wire(
        &mut peer,
        "dora",
        1,
        WorkspaceEvent::Approved {
            id: ProposalId(1),
            by: "dora".to_string(),
            height: 1,
            sig: "ff".to_string(),
        },
    );
    assert!(
        !peer.pending_sigs.contains_key(&1),
        "a terminal card collects no pending signatures"
    );
}

#[test]
fn an_applied_card_reports_the_block_signers() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut peer = chain_peer_3("walter", &b);
    // the card arrives as gossip; the sealed block (signed by petra and
    // dora) commits it — walter himself never collected a signature
    wire(
        &mut peer,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(1),
            surface: Surface::Memory,
            payload: json!({ "op": "add_note", "id": 1 }),
        },
    );
    b.commit_applied(1, &["petra", "dora"]);
    peer.receive_block(b.blocks[1].clone());
    let p = peer.proposals.get(&1).cloned().expect("card");
    assert_eq!(p.state, ProposalState::Applied);
    let v = peer.view(1, &p);
    assert_eq!(v.approvals, 2, "the block's signature count");
    let vote_of = |m: &str| {
        v.votes
            .iter()
            .find(|mv| mv.member == m)
            .map(|mv| mv.vote)
            .expect("roster row")
    };
    assert_eq!(vote_of("petra"), molt_core::VoteState::Approved);
    assert_eq!(vote_of("dora"), molt_core::VoteState::Approved);
    assert_eq!(vote_of("walter"), molt_core::VoteState::Open);
    assert!(!v.approved_by_me, "walter did not sign");
    // the read contract serves the applied proposals too (the Accepted
    // table renders from the snapshot, co-equal for every frontend)
    let snap = peer.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.accepted.len(), 1, "the applied card is in the snapshot");
    assert_eq!(snap.accepted[0].id, ProposalId(1));
    assert_eq!(snap.accepted[0].approvals, 2, "with its block-sourced voters");
}

/// **The `seen` trap.** Once a checkpoint drops the history below the cut,
/// the double-apply guard can no longer be read off `self.chain`: the
/// blocks carrying those proposal ids are gone. It has to come from the
/// blob's `consumed_ids` — which is exactly what a walk state carried
/// across the prune, or rebuilt from the surviving blocks, gets wrong.
///
/// `verify_suffix_chain` seeds it correctly today
/// (`a_suffix_chain_bootstraps_from_a_checkpoint`), and this is the
/// ENGINE-level twin: the guard must survive the prune on a live holder,
/// which is the property an incremental verifier has to preserve.
#[test]
fn an_id_consumed_below_the_cut_cannot_replay_on_a_pruned_holder() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    b.commit_applied(2, &["petra", "walter"]);
    let pre_cut = b.blocks.clone();
    let blob = checkpoint_state(&b.blocks, 2).expect("state@2");
    let anchor = b.seal(
        3,
        ChainChange::Checkpoint {
            upto: 2,
            state_hash: checkpoint_state_hash(&blob),
        },
        &["petra", "walter"],
    );
    b.push(anchor.clone());

    let mut peer = chain_peer("walter", &b, pre_cut);
    peer.receive_block(anchor);
    assert!(peer.checkpoint_blob.is_some(), "the cut sealed and anchored");
    assert_eq!(peer.chain.len(), 1, "history below the cut is dropped");
    assert_eq!(peer.chain_head.as_ref().expect("head").height, 3);

    // proposal 1 was consumed at height 1 — a block this holder no longer
    // has. Re-offering it must still be refused.
    b.commit_applied(1, &["petra", "walter"]);
    let replay = b.blocks.last().expect("the replay block").clone();
    assert_eq!(replay.height, 4, "the replay sits on top of the anchor");
    peer.receive_block(replay);
    assert_eq!(
        peer.chain_head.as_ref().expect("head").height,
        3,
        "an id consumed below the cut cannot re-apply after the prune"
    );
    assert_eq!(peer.chain.len(), 1, "the refused block is not retained");

    // …while a FRESH id on the same suffix still extends it, so the test
    // cannot pass by the holder having stopped accepting anything
    b.blocks.pop();
    b.head_hash = block_hash(&b.republic_id, &peer.chain[0]);
    b.commit_applied(9, &["petra", "walter"]);
    peer.receive_block(b.blocks.last().expect("fresh block").clone());
    assert_eq!(
        peer.chain_head.as_ref().expect("head").height,
        4,
        "a fresh id extends the pruned holder"
    );
}

/// A **refused** block must leave the walk byte-identical, because the
/// walk is now cached across calls.
///
/// The order inside `verify_next` is what makes this sharp: the
/// double-apply guard is consulted BEFORE the signatures are checked. An
/// implementation that recorded the id while checking would let one
/// unsigned block burn a proposal id forever — this holder would then
/// refuse a block every other node accepts, which is a fork produced by
/// bookkeeping alone.
#[test]
fn a_refused_block_does_not_poison_the_walk() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    let genesis = b.blocks.clone();
    let mut peer = chain_peer("walter", &b, genesis);

    // a well-formed block for proposal 7, signed by ONE of two — refused
    // at the threshold, but only after the guard has seen the id
    let change = ChainChange::Applied {
        proposal_id: 7,
        surface: Surface::Memory,
        payload: json!({ "op": "add_note", "id": 7 }),
    };
    peer.receive_block(b.seal(1, change, &["petra"]));
    assert_eq!(
        peer.chain_head.as_ref().expect("head").height,
        0,
        "a below-threshold block is refused"
    );

    // …and the legitimate block for the SAME proposal still lands
    b.commit_applied(7, &["petra", "walter"]);
    peer.receive_block(b.blocks[1].clone());
    assert_eq!(
        peer.chain_head.as_ref().expect("head").height,
        1,
        "a refused block must not burn its proposal id"
    );
}

/// Extending incrementally must equal verifying from the anchor — at
/// EVERY prefix, not just the end. The cached walk is only sound while
/// that holds, and it is the property a second implementation would
/// silently drift from.
#[test]
fn incremental_extension_equals_full_verification_at_every_prefix() {
    let b = grown_chain(12);
    let mut peer = chain_peer("walter", &b, b.blocks[..1].to_vec());
    for (i, block) in b.blocks[1..].iter().enumerate() {
        peer.receive_block(block.clone());
        let full = verify_chain(&b.blocks[..=i + 1]).expect("the prefix verifies in full");
        let cached = peer.chain_head.as_ref().expect("head");
        assert_eq!(cached.height, full.height);
        assert_eq!(cached.hash, full.hash, "prefix {} diverged", i + 1);
        assert_eq!(cached.identities, full.identities);
        assert!(
            peer.chain_walk
                .as_ref()
                .expect("the walk is kept")
                .describes(&peer.chain, peer.checkpoint_blob.as_ref()),
            "the cached walk must describe the chain it was built on"
        );
    }
}

/// WP2 pin: the catch-up re-gossip relies on the receive side being
/// idempotent — a duplicated `Proposed` stays ONE pending entry, a
/// duplicated `Approved` stays ONE signature per member, and neither
/// resurrects a proposal whose block already committed.
#[test]
fn regossiped_proposals_and_approvals_are_idempotent() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let payload = json!({ "op": "add_note", "title": "minutes" });

    // a re-gossiped Proposed lands once
    walter.receive_proposed(1, Surface::Memory, payload.clone(), "peer");
    walter.receive_proposed(1, Surface::Memory, payload.clone(), "peer");
    let pending: Vec<_> = walter
        .proposals
        .iter()
        .filter(|(_, p)| p.state == ProposalState::Proposed)
        .collect();
    assert_eq!(pending.len(), 1, "one entry, not two");

    // a re-gossiped Approved lands as ONE signature for that member
    let change = ChainChange::Applied {
        proposal_id: 1,
        surface: Surface::Memory,
        payload: payload.clone(),
    };
    let bytes = approval_bytes(&b.republic_id, 1, &change);
    let petra_sig = identity_sign(b.key("petra"), &bytes);
    walter.receive_approval(1, "petra", 1, &petra_sig);
    walter.receive_approval(1, "petra", 1, &petra_sig);
    let sigs = &walter.pending_sigs.get(&1).expect("pending set").sigs;
    assert_eq!(sigs.len(), 1, "one signature per member: {sigs:?}");

    // walter co-signs — the block seals at 2-of-3
    walter.chain_sign_and_gossip_approval(1);
    assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
    assert!(
        matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Applied),
        "the proposal committed"
    );

    // LATE re-gossip (another answering peer) must not resurrect it
    walter.receive_proposed(1, Surface::Memory, payload, "peer");
    walter.receive_approval(1, "petra", 1, &petra_sig);
    assert!(
        matches!(walter.proposals.get(&1), Some(p) if p.state == ProposalState::Applied),
        "a committed proposal stays committed"
    );
    assert_eq!(
        walter.chain_head.as_ref().expect("head").height,
        1,
        "no second block for the same proposal"
    );
}

/// WP2: whoever answers a `ChainRequest` also re-serves the OPEN
/// governance state — per open proposal a regular `Proposed` plus the
/// already-collected `Approved` signatures (verbatim, position-bound —
/// nothing is re-signed). A reopened member replays those through its
/// normal receive arms and can then co-sign; the block seals at m.
#[test]
fn a_catchup_answer_reserves_open_governance() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    let payload = json!({ "op": "add_note", "title": "minutes" });
    petra
        .cmd_propose(Surface::Memory, payload.clone())
        .expect("petra proposes");

    // what petra's catch-up answer re-gossips: the open proposal and
    // her own collected co-signature
    let bodies = petra.open_governance_events();
    let (mut saw_proposed, mut relayed_sig) = (false, None);
    for body in &bodies {
        match body {
            WorkspaceEvent::Proposed { id, surface, payload: p } => {
                assert_eq!((id.0, *surface), (1, Surface::Memory));
                assert_eq!(p, &payload, "the payload rides unchanged");
                saw_proposed = true;
            }
            WorkspaceEvent::Approved { id, by, height, sig } => {
                assert_eq!((id.0, by.as_str(), *height), (1, "petra", 1));
                relayed_sig = Some(sig.clone());
            }
            other => panic!("unexpected re-gossip event: {other:?}"),
        }
    }
    assert!(saw_proposed, "the open proposal is re-served");
    let relayed_sig = relayed_sig.expect("petra's collected signature is re-served");

    // walter — the reopened member: RAM lost the gossip, the chain has
    // only the genesis. The re-gossip restores proposal + count, then
    // his own co-signature seals the block (2-of-2).
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    walter.receive_proposed(1, Surface::Memory, payload, "peer");
    walter.receive_approval(1, "petra", 1, &relayed_sig);
    assert_eq!(
        walter.pending_sigs.get(&1).map(|s| s.sigs.len()),
        Some(1),
        "the reopened member sees the collected approval count"
    );
    walter.chain_sign_and_gossip_approval(1);
    assert_eq!(
        walter.chain_head.as_ref().expect("head").height,
        1,
        "the recovered proposal is fully approvable - the block seals"
    );
}

/// A DECIDED vote appends its summary to its discussion (story
/// 2026-08-09): the SEALER posts one System message into the patch
/// channel — so "Discussion" on an accepted vote says what exactly was
/// decided, and the notice replicates like any chat message instead of
/// being minted once per node.
#[test]
fn a_sealed_vote_appends_its_summary_to_the_discussion() {
    let pool = vec!["wss://relay.one".to_string()];
    let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
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
        .expect("proposes");
    let (id, surface, payload) = {
        let (id, rec) = walter.proposals.iter().next().expect("open proposal");
        (*id, rec.surface, rec.payload.clone())
    };
    petra.receive_proposed(id, surface, payload, "peer");
    let walter_sig = walter
        .pending_sigs
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
    assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed at m");
    // the SEALER's log carries the summary, in the vote's own channel
    let sum = petra
        .chat_visible()
        .find(|m| {
            m.kind == molt_core::ChatKind::System
                && matches!(&m.channel, molt_core::ChannelRef::Patch { id: p } if p.0 == id)
        })
        .expect("the sealed vote posts its summary into the discussion")
        .clone();
    assert!(
        sum.body.contains('✓') && sum.body.contains("relay.three.example"),
        "the summary names the outcome and the decided content: {}",
        sum.body
    );
    // …and the proposer does NOT mint its own copy (it receives the
    // sealer's message over the wire like any chat)
    assert!(
        walter.chat_visible().all(|m| m.kind != molt_core::ChatKind::System),
        "only the sealer appends"
    );
}

/// The negative outcome gets the same treatment: the decline that makes
/// approval unreachable posts the summary, naming the decliner.
#[test]
fn a_terminal_decline_appends_its_summary_to_the_discussion() {
    let pool = vec!["wss://relay.one".to_string()];
    let b = Builder::new_on_relays(&["petra", "walter"], 2, pool);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    walter
        .cmd_propose(
            Surface::Organization,
            serde_json::json!({ "op": "set_name", "value": "NewName" }),
        )
        .expect("proposes");
    // n = 2, m = 2: one decline makes the threshold unreachable
    walter.cmd_decline(ProposalId(1)).expect("declines");
    let sum = walter
        .chat_visible()
        .find(|m| {
            m.kind == molt_core::ChatKind::System
                && matches!(&m.channel, molt_core::ChannelRef::Patch { id: p } if p.0 == 1)
        })
        .expect("the terminal decline posts its summary")
        .clone();
    assert!(
        sum.body.contains('⊘')
            && sum.body.contains("walter")
            && sum.body.contains("NewName"),
        "the summary names the outcome, the decliner and the content: {}",
        sum.body
    );
}
