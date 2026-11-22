// SPDX-License-Identifier: GPL-3.0-or-later

//! Tests for [`super::membership`]: the recovery re-admission, its
//! consent-verified auto-approval and the coordinator's re-key.

use super::test_support::*;
use super::*;
use super::membership::nostr_rekey;
use molt_core::{ChainChange, MembershipOp, Surface};
use molt_storage::SigningKey;
use serde_json::json;

/// **A wire membership proposal passes its gates BEFORE it is recorded.**
/// Recording first persisted a phantom card per frame and let one
/// `id = u64::MAX - 1` set `next_id = u64::MAX` on every node — after
/// which every further proposal in the republic silently vanished
/// (review 2026-08-25, HIGH).
#[test]
fn a_membership_proposal_with_an_implausible_id_is_not_recorded() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let next_before = walter.next_id;
    let hostile = u64::MAX - 1;
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(hostile),
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        },
    );
    assert_eq!(walter.next_id, next_before, "next_id is not poisoned");
    assert!(!walter.proposals.contains_key(&hostile), "no phantom card");
    assert!(!walter.chain.proposal_changes.contains_key(&hostile), "nothing registered");
}

/// An applied MEMBERSHIP card reads its voters from the sealed block
/// too — matched by content (op + member), since a Membership block
/// carries no proposal id (review 2026-08-09, finding 7).
#[test]
fn an_applied_membership_card_reports_the_block_signers() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    let block = b.seal(
        1,
        ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b
                .keys
                .iter()
                .find(|(m, _)| m == "dora")
                .map(|(_, sk)| hex::encode(sk.verifying_key().to_bytes()))
                .expect("dora's key"),
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        },
        &["petra", "walter"],
    );
    b.push(block);
    let mut peer = chain_peer_3("walter", &b);
    assert_eq!(
        peer.chain.head.as_ref().map(|h| h.height),
        Some(1),
        "the membership block adopted"
    );
    peer.proposals.insert(
        4,
        molt_core::ProposalRecord { state: ProposalState::Applied, ..molt_core::ProposalRecord::new(Surface::Organization, json!({ "op": "restore_member", "member": "dora" }), String::new()) },
    );
    let p = peer.proposals.get(&4).cloned().expect("card");
    let v = peer.view(4, &p);
    assert_eq!(v.approvals, 2, "the membership block's signature count");
}

/// SECURITY (total-review 2026-07-18): a peer-chosen id must never let
/// a MEMBERSHIP proposal hijack a surface proposal's approvals — the
/// same forge the checkpoint arm was hardened against, on the older
/// membership arm. And symmetrically a surface proposal must not shadow
/// a pending chain change.
#[test]
fn a_membership_proposal_cannot_hijack_a_colliding_surface_id() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    // honest surface proposal id 5, awaiting approvals
    walter.receive_proposed(5, Surface::Memory, json!({"op": "add_note"}), "peer");
    // attacker gossips a membership change under the SAME id
    walter.receive_membership_proposal(5, MembershipOp::Joined, "mallory", &"ab".repeat(32), None, Vec::new(), None);
    // the id still resolves to the SURFACE proposal — approving it can
    // never sign membership bytes
    assert!(matches!(
        walter.proposal_change(5),
        Some(ChainChange::Applied { .. })
    ));
    // the reverse: a surface proposal cannot shadow a pending membership
    let mut walter2 = chain_signer("walter", &b, b.blocks.clone());
    walter2.receive_membership_proposal(6, MembershipOp::Restored, "petra", &b.pk("petra"), None, Vec::new(), None);
    walter2.receive_proposed(6, Surface::Memory, json!({"op": "add_note"}), "peer");
    assert!(matches!(
        walter2.proposal_change(6),
        Some(ChainChange::Membership { .. })
    ));
    assert!(!walter2.proposals.contains_key(&6), "surface proposal refused");
}

/// Recovery step ❸: a coordinator re-admits a returning member ONLY on a
/// valid seat proof against the anchored identity — a forged proof, or a
/// request that would re-key to a different identity, is refused. A pass
/// proposes the threshold Restored block.
#[test]
fn a_coordinator_re_admits_only_a_valid_seat_proof() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let ticket = "recovery-ticket-xyz";
    let kp_hex = &kp_hex_for(&b, "dora");

    // the returning member (dora) signs the seat proof with its OWN key
    let good = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
    let id = coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, ticket, &good, "", &[], "", "")
        .expect("a valid seat proof re-admits");
    assert!(matches!(
        coord.chain.proposal_changes.get(&id),
        Some(ChainChange::Membership {
            op: MembershipOp::Restored,
            ..
        })
    ));
    // a verified request registers the pending recovery (the MLS re-key
    // consumes it the moment the block commits — even synchronously)
    assert!(coord.recovery.pending.contains_key("dora"));

    // a proof signed by the WRONG key (petra forging dora's) is rejected
    let forged = crate::make_seat_proof(b.key("petra"), ticket, kp_hex, &rid, "", &[]);
    assert!(coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, ticket, &forged, "", &[], "", "")
        .is_err());

    // a request that re-keys the seat to a DIFFERENT identity is rejected —
    // recovery re-derives the SAME key
    assert!(coord
        .verify_and_propose_restore("dora", &b.pk("walter"), kp_hex, ticket, &good, "", &[], "", "")
        .is_err());
}

/// The rejoiner's consent counts as ONE distinct signer (recovery
/// approval design, 2026-08-08): at m = n the coordinator's single
/// surviving signature plus a valid consent seals the Restored block —
/// the case that was a structural dead end before — and the sealed
/// chain verifies from zero on an adopting reader.
#[test]
fn a_consented_restore_seals_at_m_equals_n() {
    let b = Builder::new(&["petra", "walter"], 2);
    let walter_pk = b.pk("walter");
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    let consent = consent_for(&b, "walter", "");
    petra.propose_membership(
        MembershipOp::Restored,
        "walter",
        &walter_pk,
        None,
        Vec::new(),
        Some(consent),
    );
    let head = petra.chain.head.as_ref().expect("head");
    assert_eq!(head.height, 1, "petra's signature + walter's consent reach 2-of-2");
    verify_chain(&petra.chain.blocks).expect("an adopting reader accepts the consented block");
}

/// The approval surface (recovery approval design, 2026-08-08): a
/// verified request creates a HUMAN-visible proposal record, a survivor
/// approves it through the PUBLIC `cmd_approve`, and the commit settles
/// the record to `Applied` with the vote bookkeeping dropped.
#[test]
fn a_wire_membership_proposal_is_votable_without_hand_applying() {
    // D3: the applier runs only for the proposer's OWN log, so the wire
    // arm must create the human-facing record itself — without it a
    // receiver held no card, cmd_approve said UnknownProposal, and an
    // m>=3 recovery stalled (coordinator co-sign + rejoiner consent are
    // only 2 distinct signers).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let consent = consent_for(&b, "dora", "");
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(5),
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: None,
            relays: Vec::new(),
            consent: Some(consent),
        },
    );
    assert!(
        walter.proposals.contains_key(&5),
        "the receiver holds the votable card"
    );
    walter.cmd_approve(ProposalId(5), None).expect("the survivor can approve");
}

#[test]
fn a_membership_proposal_is_a_visible_approvable_record() {
    // CONSENT-LESS (a legacy rejoiner): the one restore shape that still
    // needs the human vote — auto-approval only ever signs a consent this
    // node verified itself (recovery_auto_approval.md §2).
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let ticket = "recovery-ticket-xyz";
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
    let id = coord
        .verify_and_propose_restore(
            "dora",
            &b.pk("dora"),
            kp_hex,
            ticket,
            &proof,
            "",
            &[],
            "",
            "",
        )
        .expect("a valid request proposes");

    // visible on the proposer: a real record with the reserved op
    let rec = coord.proposals.get(&id).expect("the proposer holds a record");
    assert_eq!(rec.payload["op"], "restore_member");
    assert_eq!(rec.payload["member"], "dora");
    assert_eq!(rec.state, ProposalState::Proposed, "1 of 2 voices - still open");

    // …and on a receiver: the gossip's log event creates the SAME record
    let env = walter.make_env(
        "petra".to_string(),
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(id),
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        },
    );
    walter.apply(&env);
    walter.receive_membership_proposal(
        id,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        None,
        Vec::new(),
        None,
    );
    assert_eq!(
        walter.proposals.get(&id).map(|p| p.state),
        Some(ProposalState::Proposed),
        "the receiver sees an open, votable record"
    );
    assert!(
        !walter
            .chain.pending_sigs
            .get(&id)
            .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "a consent-less restore never auto-signs - the human vote is the content"
    );
    let petra_sig = coord
        .chain.pending_sigs
        .get(&id)
        .expect("petra's pending set")
        .sigs
        .iter()
        .find(|a| a.member == "petra")
        .expect("petra co-signed")
        .sig
        .clone();
    walter.receive_approval(id, "petra", 1, &petra_sig);
    assert_eq!(
        walter.chain.head.as_ref().expect("head").height,
        0,
        "1 signature + no consent stays open"
    );

    // the PUBLIC approve — the exact call that answered UnknownProposal
    // before the record existed
    walter.cmd_approve(ProposalId(id), None).expect("approve accepts the id");

    // petra + walter = 2-of-3: sealed, settled
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 1);
    assert_eq!(
        walter.proposals.get(&id).map(|p| p.state),
        Some(ProposalState::Applied),
        "the commit settles the record"
    );
    assert!(
        !walter.chain.pending_sigs.contains_key(&id) && !walter.chain.proposal_changes.contains_key(&id),
        "the vote bookkeeping is dropped"
    );
    verify_chain(&walter.chain.blocks).expect("the sealed chain verifies from zero");
}

/// Auto-approval (recovery_auto_approval.md §3): a survivor that RECEIVES
/// a `Restored` proposal carrying a consent it can verify itself signs it
/// without a human — the recovery completes as soon as m survivors are
/// online, no card-clicking required. The seal needs no `cmd_approve`.
#[test]
fn a_consented_restore_is_approved_without_a_human() {
    let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let ticket = "recovery-ticket-xyz";
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "dora", "", &[], 0, &"cc".repeat(32));
    let id = coord
        .verify_and_propose_restore(
            "dora",
            &b.pk("dora"),
            kp_hex,
            ticket,
            &proof,
            "",
            &[],
            &consent,
            "",
        )
        .expect("a valid request proposes");

    let env = walter.make_env(
        "petra".to_string(),
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(id),
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: None,
            relays: Vec::new(),
            consent: Some(consent.clone()),
        },
    );
    walter.apply(&env);
    walter.receive_membership_proposal(
        id,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        None,
        Vec::new(),
        Some(consent),
    );
    // the receipt alone put walter's REAL signature into the pending set
    assert!(
        walter
            .chain.pending_sigs
            .get(&id)
            .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "a verified consent auto-signs on receipt"
    );
    // …and petra's gossiped signature completes the threshold: petra +
    // walter + dora's consent = 3-of-4, sealed with no cmd_approve call
    let petra_sig = coord
        .chain.pending_sigs
        .get(&id)
        .expect("petra's pending set")
        .sigs
        .iter()
        .find(|a| a.member == "petra")
        .expect("petra co-signed")
        .sig
        .clone();
    walter.receive_approval(id, "petra", 1, &petra_sig);
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 1);
    assert_eq!(
        walter.proposals.get(&id).map(|p| p.state),
        Some(ProposalState::Applied),
        "the commit settles the record without a human approve"
    );
    verify_chain(&walter.chain.blocks).expect("the sealed chain verifies from zero");
}

/// A v1 consent binds neither relays nor position (audit 2026-10-10 #5,
/// #6): it still registers and still counts as a voice, but no survivor
/// signs it unattended (`recovery_auto_approval.md` §7e).
#[test]
fn a_v1_consent_is_left_to_the_human_vote() {
    let b = Builder::new(&["petra", "walter", "dora"], 3);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    walter.receive_membership_proposal(
        7,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        Some("dd".repeat(32)),
        vec!["wss://any".to_string()],
        Some(consent_for(&b, "dora", &"dd".repeat(32))),
    );
    assert!(walter.chain.proposal_changes.contains_key(&7), "the card registers");
    assert!(
        !walter.chain.pending_sigs.get(&7).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "a v1 consent never auto-signs"
    );
}

/// Audit #5: a survivor signs only the relays the rejoiner declared.
#[test]
fn a_v2_consent_beside_swapped_relays_never_auto_signs() {
    let b = Builder::new(&["petra", "walter", "dora"], 3);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let npk = "dd".repeat(32);
    let signed = vec!["wss://a".to_string()];
    let consent = consent_v2_for(&b, "dora", &npk, &signed, 0, &"cc".repeat(32));
    walter.receive_membership_proposal(
        7,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        Some(npk.clone()),
        vec!["wss://evil".to_string()],
        Some(consent.clone()),
    );
    assert!(!walter.chain.proposal_changes.contains_key(&7), "an unsealable card is refused");
    assert!(!walter.chain.pending_sigs.get(&7).is_some_and(|p| !p.sigs.is_empty()));
    // the honest relays sign
    walter.receive_membership_proposal(
        8,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        Some(npk),
        signed,
        Some(consent),
    );
    assert!(
        walter.chain.pending_sigs.get(&8).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "the declared relays auto-sign"
    );
}

/// Audit #6 at the coordinator: a sealed v2 consent never proposes again,
/// and one minted for a far-off height is stale.
#[test]
fn a_spent_or_stale_v2_consent_is_refused_at_the_coordinator() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let kp_hex = &kp_hex_for(&b, "dora");
    let npk = "dd".repeat(32);
    let relays = vec!["wss://a".to_string()];
    let propose = |coord: &mut crate::State, ticket: &str, consent: &str| {
        let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, &npk, &relays);
        coord.verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, ticket, &proof, &npk, &relays, consent, "")
    };
    let stale = consent_v2_for(&b, "dora", &npk, &relays, 5, &"cc".repeat(32));
    let err = propose(&mut coord, "t0", &stale).expect_err("an anchor above the head is refused");
    assert!(err.contains("stale"), "{err}");

    let consent = consent_v2_for(&b, "dora", &npk, &relays, 0, &"cc".repeat(32));
    propose(&mut coord, "t1", &consent).expect("a fresh consent proposes");
    assert_eq!(coord.chain.head.as_ref().expect("head").height, 1, "petra + consent seal at m = 2");
    let ChainChange::Membership { relays: carried, .. } = &coord.chain.blocks[1].change else {
        panic!("a membership block");
    };
    assert_eq!(carried, &relays, "the block carries the signed declaration");
    let err = propose(&mut coord, "t2", &consent).expect_err("the replay is refused");
    assert!(err.contains("spent"), "{err}");
    verify_chain(&coord.chain.blocks).expect("the sealed chain verifies");
}

/// A v1 consent at the coordinator gets the human card too (§7e): no
/// self-cosign, so on m = 2 it does not seal with the consent alone.
#[test]
fn a_v1_consent_is_not_cosigned_by_the_coordinator() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", kp_hex, &rid, "", &[]);
    let consent = consent_for(&b, "dora", "");
    let id = coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], &consent, "")
        .expect("a v1 request still proposes");
    assert_eq!(coord.chain.head.as_ref().expect("head").height, 0, "nothing sealed");
    assert!(
        !coord.chain.pending_sigs.get(&id).is_some_and(|p| p.sigs.iter().any(|a| a.member == "petra")),
        "the coordinator does not sign a v1 consent unattended"
    );
}

fn register_v2(st: &mut crate::State, b: &Builder, id: u64, npk: &str, anchor_height: u64) {
    let consent = consent_v2_for(b, "dora", npk, &[], anchor_height, &"cc".repeat(32));
    st.receive_membership_proposal(
        id,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        Some(npk.to_string()),
        Vec::new(),
        Some(consent),
    );
}

/// A registered v2 card that can no longer seal (spent by a sibling
/// restore) leaves the registry, so it never pins the seat's pending cap.
#[test]
fn a_spent_v2_card_is_swept_from_the_registry() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora", "erika"], 4);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    for (i, id) in [7u64, 8, 9].into_iter().enumerate() {
        register_v2(&mut walter, &b, id, &format!("d{i}").repeat(32), 0);
    }
    assert_eq!(walter.chain.proposal_changes.len(), 3, "three cards register");
    let block = b.commit(
        ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: Some("ee".repeat(32)),
            relays: Vec::new(),
            consent: None,
        },
        &["petra", "walter", "erika", "dora"],
    );
    assert!(walter.append_committed_block(block.clone()), "the sibling restore applies");
    walter.after_block_applied(&block);
    assert!(walter.chain.proposal_changes.is_empty(), "spent cards are swept");
    // the cap is free again for a consent over the new anchor
    let consent = consent_v2_for(&b, "dora", &"ff".repeat(32), &[], 1, &"ee".repeat(32));
    walter.receive_membership_proposal(
        10,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        Some("ff".repeat(32)),
        Vec::new(),
        Some(consent),
    );
    assert!(walter.chain.proposal_changes.contains_key(&10), "a fresh consent registers");
}

/// A registered v2 card whose window closed leaves the registry.
#[test]
fn a_stale_v2_card_is_swept_from_the_registry() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let w = molt_core::chain::RESTORE_CONSENT_WINDOW;
    let mut b = Builder::new(&["petra", "walter", "dora"], 3);
    for id in 0..w - 1 {
        b.commit_applied(100 + id, &["petra", "walter", "dora"]);
    }
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    register_v2(&mut walter, &b, 7, &"dd".repeat(32), 0);
    assert!(walter.chain.proposal_changes.contains_key(&7), "the last fresh height registers");
    b.commit_applied(100 + w, &["petra", "walter", "dora"]);
    let block = b.blocks.last().cloned().expect("block");
    assert!(walter.append_committed_block(block.clone()));
    walter.after_block_applied(&block);
    assert!(!walter.chain.proposal_changes.contains_key(&7), "a stale card is swept");
}

/// The coordinator's swept card takes its recovery with it: the seat is no
/// longer in flight (R3), and no progress is reported for it.
#[test]
fn a_swept_v2_card_drops_the_coordinators_pending_recovery() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let w = molt_core::chain::RESTORE_CONSENT_WINDOW;
    let mut b = Builder::new(&["petra", "walter", "dora"], 3);
    for id in 0..w - 1 {
        b.commit_applied(100 + id, &["petra", "walter", "dora"]);
    }
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "dora", "", &[], 0, &"cc".repeat(32));
    let id = coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], &consent, "")
        .expect("the last fresh height proposes");
    assert!(coord.readmission_in_flight("dora"), "in flight while the card can seal");
    b.commit_applied(100 + w, &["petra", "walter", "dora"]);
    let block = b.blocks.last().cloned().expect("block");
    assert!(coord.append_committed_block(block.clone()));
    coord.after_block_applied(&block);
    assert!(!coord.chain.proposal_changes.contains_key(&id), "the stale card is swept");
    assert!(!coord.recovery.pending.contains_key("dora"), "its recovery goes with it");
    assert!(!coord.readmission_in_flight("dora"), "the seat is no longer in flight");
}

/// A swept card leaves a live sibling's recovery alone: the pending entry
/// is per seat, and the other card can still seal.
#[test]
fn a_swept_v2_card_keeps_a_live_siblings_pending_recovery() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let w = molt_core::chain::RESTORE_CONSENT_WINDOW;
    let mut b = Builder::new(&["petra", "walter", "dora"], 3);
    for id in 0..w - 1 {
        b.commit_applied(100 + id, &["petra", "walter", "dora"]);
    }
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "dora", "", &[], 0, &"cc".repeat(32));
    let stale = coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], &consent, "")
        .expect("the last fresh height proposes");
    let fresh = consent_v2_for(&b, "dora", "", &[], w - 1, &"cc".repeat(32));
    let proof = crate::make_seat_proof(b.key("dora"), "u", kp_hex, &rid, "", &[]);
    let live = coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "u", &proof, "", &[], &fresh, "")
        .expect("a fresh sibling proposes");
    b.commit_applied(100 + w, &["petra", "walter", "dora"]);
    let block = b.blocks.last().cloned().expect("block");
    assert!(coord.append_committed_block(block.clone()));
    coord.after_block_applied(&block);
    assert!(!coord.chain.proposal_changes.contains_key(&stale), "the stale card is swept");
    assert!(coord.chain.proposal_changes.contains_key(&live), "the sibling stays");
    assert!(coord.recovery.pending.contains_key("dora"), "and keeps the recovery");
}

/// Petra coordinates dora's v2 restore one height before its window closes;
/// `foreign` is a consent-less card for dora some other node planted.
fn coordinator_with_a_foreign_card() -> (Builder, crate::State, u64, ChainChange) {
    let w = molt_core::chain::RESTORE_CONSENT_WINDOW;
    let mut b = Builder::new(&["petra", "walter", "dora"], 3);
    for id in 0..w - 1 {
        b.commit_applied(100 + id, &["petra", "walter", "dora"]);
    }
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "dora", "", &[], 0, &"cc".repeat(32));
    let own = coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], &consent, "")
        .expect("the last fresh height proposes");
    let foreign = ChainChange::Membership {
        op: MembershipOp::Restored,
        member: "dora".to_string(),
        identity_pk: b.pk("dora"),
        nostr_pk: None,
        relays: Vec::new(),
        consent: None,
    };
    coord.chain.proposal_changes.insert(own + 1_000, foreign.clone());
    (b, coord, own, foreign)
}

/// Only this node's own card carries its recovery: a card someone else
/// registered for the seat neither keeps a swept recovery in flight (R3)
/// nor, once it seals, re-keys with the dead request's KeyPackage.
#[test]
fn a_foreign_card_neither_keeps_nor_consumes_the_coordinators_recovery() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let w = molt_core::chain::RESTORE_CONSENT_WINDOW;
    let (mut b, mut coord, own, foreign) = coordinator_with_a_foreign_card();
    b.commit_applied(100 + w, &["petra", "walter", "dora"]);
    let block = b.blocks.last().cloned().expect("block");
    assert!(coord.append_committed_block(block.clone()));
    coord.after_block_applied(&block);
    assert!(!coord.chain.proposal_changes.contains_key(&own), "the own card is swept");
    assert!(!coord.recovery.pending.contains_key("dora"), "its recovery goes with it");
    assert!(!coord.readmission_in_flight("dora"), "a foreign card keeps nothing in flight");

    let height = u64::try_from(b.blocks.len()).expect("height");
    coord.after_block_applied(&b.seal(height, foreign, &["petra", "walter", "dora"]));
    assert!(coord.recovery.swept.contains_key("dora"), "no re-key with the dead request");
}

/// Another card for the seat sealing re-keys nothing here (that is its
/// coordinator's job) and sets this node's recovery aside: kept for its own
/// card, no longer holding the seat in flight.
#[test]
fn a_foreign_restore_block_sets_the_coordinators_recovery_aside() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let (b, mut coord, _, foreign) = coordinator_with_a_foreign_card();
    let height = u64::try_from(b.blocks.len()).expect("height");
    coord.after_block_applied(&b.seal(height, foreign, &["petra", "walter", "dora"]));
    assert!(!coord.recovery.pending.contains_key("dora"), "no longer pending");
    assert!(coord.recovery.swept.contains_key("dora"), "not consumed by another card's seal");
}

/// A card swept as dead that seals after all (a reorg re-served it) still
/// gets this coordinator's Welcome: the swept recovery is kept aside.
#[test]
fn a_swept_card_that_seals_after_all_still_rekeys() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let w = molt_core::chain::RESTORE_CONSENT_WINDOW;
    let (mut b, mut coord, own, _) = coordinator_with_a_foreign_card();
    let card = coord.chain.proposal_changes.get(&own).cloned().expect("own card");
    b.commit_applied(100 + w, &["petra", "walter", "dora"]);
    let block = b.blocks.last().cloned().expect("block");
    assert!(coord.append_committed_block(block.clone()));
    coord.after_block_applied(&block);
    assert!(!coord.recovery.pending.contains_key("dora"), "swept");
    assert!(coord.recovery.swept.contains_key("dora"), "kept aside");

    let height = u64::try_from(b.blocks.len()).expect("height");
    coord.after_block_applied(&b.seal(height, card, &["petra", "walter", "dora"]));
    assert!(!coord.recovery.swept.contains_key("dora"), "the re-key consumed the swept recovery");
    assert!(!coord.recovery.pending.contains_key("dora"), "and left nothing in flight");
}

/// A survivor one block behind the minter registers the card instead of
/// calling it stale, and auto-signs once it reaches the anchor.
#[test]
fn a_lagging_survivor_registers_and_signs_a_v2_card_on_arrival() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora"], 3);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    register_v2(&mut walter, &b, 7, &"dd".repeat(32), 1);
    assert!(walter.chain.proposal_changes.contains_key(&7), "not yet reachable is not stale");
    b.commit_applied(100, &["petra", "walter", "dora"]);
    let block = b.blocks.last().cloned().expect("block");
    assert!(walter.append_committed_block(block.clone()));
    walter.after_block_applied(&block);
    assert!(
        walter.chain.pending_sigs.get(&7).is_some_and(|p| p.height == 2 && p.sigs.iter().any(|a| a.member == "walter")),
        "the survivor signs once the anchor is reached"
    );
}

/// The chain IS the replay register (field storm 2026-08-24): every
/// anchor that was ever anchored — genesis or a Restored block — refuses
/// a replayed self-service request; a fresh salt passes.
#[test]
fn a_chain_known_anchor_is_a_replay() {
    let b = Builder::new(&["petra", "walter"], 2);
    let mut petra = chain_signer("petra", &b, b.blocks.clone());
    let genesis_anchor = "cc".repeat(32);
    assert!(petra.anchor_seen_in_chain(&genesis_anchor), "genesis anchors count");
    let consent = consent_for(&b, "walter", "ab");
    petra.propose_membership(
        MembershipOp::Restored,
        "walter",
        &b.pk("walter"),
        Some("ab".to_string()),
        Vec::new(),
        Some(consent),
    );
    assert_eq!(petra.chain.head.as_ref().expect("head").height, 1, "sealed");
    assert!(petra.anchor_seen_in_chain("ab"), "a Restored block's anchor counts");
    assert!(!petra.anchor_seen_in_chain(&"99".repeat(32)), "a fresh salt passes");
    assert!(!petra.anchor_seen_in_chain(""), "empty is never a hit");
}

/// The coordinator's vote report toward the waiting rejoiner
/// (recovery_auto_approval.md §4): roster in roster order, the counted
/// voices (its own co-signature + the consent), the threshold — and
/// nothing for a proposal it does not coordinate.
#[test]
fn the_coordinator_reports_the_vote_progress_for_a_pending_recovery() {
    let b = Builder::new(&["petra", "walter", "dora"], 3);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let rid = b.republic_id.clone();
    let ticket = "recovery-ticket-xyz";
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
    let consent = consent_v2_for(&b, "dora", "", &[], 0, &"cc".repeat(32));
    let id = coord
        .verify_and_propose_restore(
            "dora",
            &b.pk("dora"),
            kp_hex,
            ticket,
            &proof,
            "",
            &[],
            &consent,
            "",
        )
        .expect("a valid request proposes");
    let report = coord.recover_progress_for(id).expect("a coordinated recovery reports");
    assert_eq!(report.member, "dora");
    assert_eq!(report.need, 3);
    assert_eq!(report.roster, vec!["petra", "walter", "dora"], "roster order");
    assert_eq!(
        report.approved,
        vec!["dora", "petra"],
        "the coordinator's co-signature and the consent are counted; walter is not"
    );
    // a proposal this node does not coordinate reports nothing
    assert!(coord.recover_progress_for(id + 1).is_none());
}

/// The auto-approval trusts NOTHING the coordinator claims: a consent
/// that does not verify against the seat's anchored key never auto-signs
/// (a malicious coordinator would otherwise harvest m unattended
/// signatures for a block the verifier then rejects — or worse, for a
/// change nobody consented to).
#[test]
fn a_forged_consent_never_auto_signs() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    // signed by the WRONG seat's key (petra's), claiming dora's consent
    let forged = molt_storage::identity_sign(
        b.key("petra"),
        &molt_core::chain::restore_consent_bytes(&b.republic_id, "dora", &b.pk("dora"), ""),
    );
    walter.receive_membership_proposal(
        7,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        None,
        Vec::new(),
        Some(forged),
    );
    assert!(
        !walter
            .chain.pending_sigs
            .get(&7)
            .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "a forged consent must wait for a human, never auto-sign"
    );
}

/// A restore claiming a transport anchor another living seat already
/// holds (or one that is not even canonical) never auto-signs — the
/// coordinator's ingest checks this, but auto-approval re-checks it
/// because it must not trust the coordinator.
#[test]
fn a_restore_claiming_a_foreign_anchor_never_auto_signs() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    // every Builder seat carries this anchor — dora claiming it collides
    // with petra's and walter's living seats (and a non-canonical string
    // refuses on the same guard ladder)
    let taken = "cc".repeat(32);
    let consent = consent_for(&b, "dora", &taken);
    walter.receive_membership_proposal(
        7,
        MembershipOp::Restored,
        "dora",
        &b.pk("dora"),
        Some(taken),
        Vec::new(),
        Some(consent),
    );
    assert!(
        !walter
            .chain.pending_sigs
            .get(&7)
            .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "an anchor collision must wait for a human, never auto-sign"
    );
}

/// R5 — the re-join gate: a declaration that shares no relay with some
/// member is refused, NAMING the relay the others must add — that
/// message is the whole feature. The same declaration passes once the
/// pool carries the relay.
#[test]
fn a_rejoin_over_a_foreign_relay_is_refused_naming_it() {
    let ticket = "recovery-ticket-r5";
    let declared = vec!["wss://relay.two.example".to_string()];

    // republic pool: relay.one only — dora's declared relay bridges nobody
    let b = Builder::new_on_relays(
        &["petra", "walter", "dora"],
        2,
        vec!["wss://relay.one".to_string()],
    );
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(
        b.key("dora"),
        ticket,
        kp_hex,
        &b.republic_id,
        "",
        &declared,
    );
    let err = coord
        .verify_and_propose_restore(
            "dora",
            &b.pk("dora"),
            kp_hex,
            ticket,
            &proof,
            "",
            &declared,
            "",
            "",
        )
        .expect_err("a declaration bridging nobody must be refused");
    assert!(
        err.contains("wss://relay.two.example") && err.contains("add"),
        "the refusal names the relay the others must add: {err}"
    );

    // the SAME declaration passes once the pool carries the relay
    let b2 = Builder::new_on_relays(
        &["petra", "walter", "dora"],
        2,
        vec!["wss://relay.one".to_string(), "wss://relay.two.example".to_string()],
    );
    let mut coord2 = chain_signer("petra", &b2, b2.blocks.clone());
    let kp_hex = &kp_hex_for(&b2, "dora");
    let proof2 = crate::make_seat_proof(
        b2.key("dora"),
        ticket,
        kp_hex,
        &b2.republic_id,
        "",
        &declared,
    );
    let id = coord2
        .verify_and_propose_restore(
            "dora",
            &b2.pk("dora"),
            kp_hex,
            ticket,
            &proof2,
            "",
            &declared,
            "",
            "",
        )
        .expect("the same declaration passes once the pool carries it");
    // …and the block carries the seat's OWN declaration (its ledger entry)
    assert!(matches!(
        coord2.chain.proposal_changes.get(&id),
        Some(ChainChange::Membership { relays, .. }) if *relays == declared
    ));
}

/// When a `Restored` block commits, the coordinator (the node holding the
/// pending recovery for that member) consumes it to drive the MLS re-key;
/// a node without a pending recovery for that member does nothing. Here
/// there is no runtime group, so the re-key is a logged no-op — but the
/// trigger CONDITION (consume the pending recovery on commit) is exercised.
#[test]
fn a_restored_commit_triggers_the_coordinators_rekey() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let walter_pk = b.pk("walter");
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    // build a Restored block for walter and hand it to the coordinator
    let change = ChainChange::Membership {
        op: MembershipOp::Restored,
        member: "walter".to_string(),
        identity_pk: walter_pk,
        nostr_pk: None,
        relays: Vec::new(),
        consent: None,
    };
    coord.recovery.pending.insert(
        "walter".to_string(),
        PendingRecovery {
            key_package: "beef".to_string(),
            reply: String::new(),
            card: change.clone(),
            ticket: String::new(),
        },
    );
    let block = b.seal(1, change, &["petra", "walter"]);
    coord.receive_block(block);

    assert_eq!(coord.chain.head.as_ref().expect("head").height, 1);
    assert!(
        !coord.recovery.pending.contains_key("walter"),
        "the coordinator consumed the pending recovery on the Restored commit"
    );
}

/// A three-member MLS group (`coord`, `walter`, `dora`) — the shape a
/// coordinator re-keys from.
fn mls_trio() -> (molt_net::MlsMember, molt_net::MlsMember, molt_net::MlsMember) {
    let key = |n: u8| SigningKey::from_bytes(&[n; 32]);
    let mut coord = molt_net::MlsMember::new(&key(1), "coord").expect("coord");
    let walter = molt_net::MlsMember::new(&key(2), "walter").expect("walter");
    let dora = molt_net::MlsMember::new(&key(3), "dora").expect("dora");
    coord.create_group().expect("create");
    let welcome = coord
        .add_members(&[
            walter.key_package().expect("walter kp"),
            dora.key_package().expect("dora kp"),
        ])
        .expect("add")
        .expect("welcome");
    let (mut walter, mut dora) = (walter, dora);
    walter.join_from_welcome(&welcome).expect("walter joins");
    dora.join_from_welcome(&welcome).expect("dora joins");
    (coord, walter, dora)
}

/// **The Nostr re-key seals under the epoch its recipients are still at**
/// (N4b step 6c, the `9900f36` lesson re-pinned at the production entry
/// point).
///
/// A receiver's exporter ring reaches BACKWARD only. So a commit whose
/// outer layer is sealed at the epoch the coordinator just moved TO is
/// opaque to exactly the members it exists to move forward — and the
/// whole recovery is undeliverable, silently, because an opaque frame
/// looks like relay spam. The negative half is the test: the NEW epoch's
/// exporter must NOT open it.
#[test]
fn a_nostr_rekey_commit_opens_for_the_survivors_it_is_meant_for() {
    // dora is the SURVIVOR here — walter is the seat being restored, and
    // its old leaf is evicted by the very commit under test
    let (coord, _walter, survivor) = mls_trio();
    // walter lost everything and re-derives the SAME identity
    let returning =
        molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
    let kp = returning.key_package().expect("key package");

    let survivor_secrets = {
        let mut v = vec![survivor.exporter_secret().expect("survivor exporter")];
        v.extend_from_slice(survivor.exporter_ring());
        v
    };
    let mls = std::sync::Mutex::new(coord);
    let rekey = nostr_rekey(&mls, "walter", &kp, 1_759_000_000).expect("the re-key runs");

    // the commit, sealed the way the delivery task seals it…
    let sealed = molt_net::envelope::seal_outer(&rekey.prev_exporter, &rekey.commit)
        .expect("seal the commit");
    assert!(
        molt_net::envelope::open_outer(&survivor_secrets, &sealed).is_ok(),
        "a survivor that has NOT yet merged the commit cannot open it - the whole \
         re-key is undeliverable to exactly the members it is for"
    );
    // …and the counter-case: the epoch the coordinator moved TO must not
    // be what it sealed under, or the assertion above passes by accident
    let new_epoch = mls.lock().expect("lock").exporter_secret().expect("new exporter");
    assert_ne!(
        new_epoch, rekey.prev_exporter,
        "the commit was sealed at the coordinator's NEW epoch - backward-only \
         exporter rings make that opaque to every survivor"
    );
}

/// The re-key carries the stamp it was pinned at, so the original publish
/// and every resend share one `h` window.
#[test]
fn the_rekey_carries_the_stamp_it_was_pinned_at() {
    let (coord, _survivor, _dora) = mls_trio();
    let returning =
        molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
    let kp = returning.key_package().expect("key package");

    let pinned = 1_759_123_456;
    let mls = std::sync::Mutex::new(coord);
    let rekey = nostr_rekey(&mls, "walter", &kp, pinned).expect("the re-key runs");
    assert_eq!(
        rekey.stamp, pinned,
        "the re-key must carry its own pinned stamp - the delivery has no other source for it"
    );
}

/// The Welcome really admits the returning seat: it is the whole point of
/// the re-key, and a commit that produced an unusable Welcome would still
/// satisfy both tests above.
#[test]
fn the_rekey_welcome_puts_the_returning_seat_back_in_the_group() {
    let (coord, _walter, mut survivor) = mls_trio();
    let mut returning =
        molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
    let kp = returning.key_package().expect("key package");

    let mls = std::sync::Mutex::new(coord);
    let rekey = nostr_rekey(&mls, "walter", &kp, 1_759_000_000).expect("the re-key runs");

    // the survivor merges the commit and reaches the new epoch
    match survivor.decrypt(&rekey.commit).expect("survivor processes the commit") {
        molt_net::mls::MlsIncoming::Commit { .. } => {}
        other => panic!("expected a commit, got {other:?}"),
    }
    returning.join_from_welcome(&rekey.welcome).expect("the seat rejoins");
    // …and the two can now talk, which is what "recovered" means
    let ct = returning.encrypt(b"back").expect("encrypt");
    match survivor.decrypt(&ct).expect("survivor reads the rejoiner") {
        molt_net::mls::MlsIncoming::Application { from, plaintext } => {
            assert_eq!(from, "walter");
            assert_eq!(plaintext, b"back");
        }
        other => panic!("expected an application message, got {other:?}"),
    }
}

/// **Re-mint failover, engine level: a survivor (or a restarted, amnesiac
/// coordinator) adopting a committed `Restored` block it holds NO pending
/// recovery for is inert.** The chain extends normally, but
/// `coordinator_rekey` never runs: nothing is recorded (no
/// `WorkspaceEvent::MlsCommit` broadcast), the mesh window is not armed,
/// and a pending recovery for a DIFFERENT member is left untouched. This
/// is the crash-before-re-key case: the block committed, the coordinator
/// died, and the re-mint failover's second round supplies the re-key.
#[test]
fn a_restored_commit_without_a_pending_recovery_is_inert() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let walter_pk = b.pk("walter");
    let mut node = chain_signer("petra", &b, b.blocks.clone());
    // a pending recovery for ANOTHER member must survive walter's commit
    node.recovery.pending.insert(
        "dora".to_string(),
        PendingRecovery {
            key_package: "beef".to_string(),
            reply: String::new(),
            ticket: String::new(),
            card: ChainChange::Membership {
                op: MembershipOp::Restored,
                member: "dora".to_string(),
                identity_pk: b.pk("dora"),
                nostr_pk: None,
                relays: Vec::new(),
                consent: None,
            },
        },
    );
    let seq_before = node.next_seq;

    // a Restored block for walter — committed elsewhere — arrives; this
    // node holds no pending recovery for walter
    let change = ChainChange::Membership {
        op: MembershipOp::Restored,
        member: "walter".to_string(),
        identity_pk: walter_pk,
        nostr_pk: None,
        relays: Vec::new(),
        consent: None,
    };
    let block = b.seal(1, change, &["petra", "walter"]);
    node.receive_block(block);

    // the chain extends …
    assert_eq!(node.chain.head.as_ref().expect("head").height, 1);
    // … but the re-key trigger stayed inert: no envelope of any kind was
    // recorded (make_env is the only seq stamp, so an MlsCommit broadcast
    // or a chat notice would have advanced next_seq) …
    assert_eq!(node.next_seq, seq_before, "no MlsCommit/notice was recorded");
    // … the recovery mesh window was never armed …
    assert!(node.recovery.mesh_window.is_empty());
    // … and only walter's (absent) entry was consulted — dora's pending
    // recovery is untouched
    assert!(node.recovery.pending.contains_key("dora"));
}

/// A rejoiner adopts the chain before the day's gossip replays: a restore
/// proposal whose block is already sealed must arrive settled, never as an
/// open vote (field 2026-09-12: three stale "Restore seat" cards).
#[test]
fn a_restore_proposal_already_sealed_arrives_settled() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    let (_, anchor) = molt_net::nostr_identity(&[7u8; 16], "restore-ticket");
    let block = b.seal(
        1,
        ChainChange::Membership {
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: Some(anchor.clone()),
            relays: Vec::new(),
            consent: None,
        },
        &["petra", "walter"],
    );
    b.push(block);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(7),
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: Some(anchor),
            relays: Vec::new(),
            consent: None,
        },
    );
    let card = walter.proposals.get(&7).expect("the card exists");
    assert_ne!(card.state, ProposalState::Proposed, "a sealed restore is no open vote");
}

/// A KeyPackage that is not the seat's own (a foreign signature key, or
/// another seat's name) is refused before anything is proposed: survivors
/// refuse such a leaf, so re-keying with it strands the coordinator alone.
#[test]
fn a_restore_with_a_key_package_not_bound_to_the_seat_is_refused() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let ticket = "recovery-ticket-kp";
    let try_kp = |coord: &mut crate::State, kp: &str| {
        let proof = crate::make_seat_proof(b.key("dora"), ticket, kp, &b.republic_id, "", &[]);
        coord.verify_and_propose_restore("dora", &b.pk("dora"), kp, ticket, &proof, "", &[], "", "")
    };
    let foreign_key = hex::encode(
        molt_net::MlsMember::new(b.key("petra"), "dora")
            .expect("mls")
            .key_package()
            .expect("kp"),
    );
    assert!(try_kp(&mut coord, &foreign_key).is_err(), "dora's name under petra's key");
    let other_name = hex::encode(
        molt_net::MlsMember::new(b.key("dora"), "walter")
            .expect("mls")
            .key_package()
            .expect("kp"),
    );
    assert!(try_kp(&mut coord, &other_name).is_err(), "dora's key under walter's name");
    assert!(try_kp(&mut coord, "beef").is_err(), "not a key package");
    assert!(coord.recovery.pending.is_empty(), "nothing registered");
    try_kp(&mut coord, &kp_hex_for(&b, "dora")).expect("the seat's own key package passes");
}

/// A Restored change no recovery could seal must not enter the registry:
/// a swapped identity, a consent that does not verify, a reserved Joined,
/// or a second consent-less card for the same seat.
#[test]
fn an_unsealable_membership_proposal_is_not_registered() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let restored = |id: u64, identity_pk: String, consent: Option<String>| {
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(id),
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk,
            nostr_pk: None,
            relays: Vec::new(),
            consent,
        }
    };
    wire(&mut walter, "petra", 1, restored(11, "00".to_string(), None));
    wire(&mut walter, "petra", 2, restored(12, b.pk("dora"), Some(consent_for(&b, "erika", ""))));
    wire(
        &mut walter,
        "petra",
        3,
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(13),
            op: MembershipOp::Joined,
            member: "mallory".to_string(),
            identity_pk: "ab".repeat(32),
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        },
    );
    wire(&mut walter, "petra", 4, restored(14, b.pk("dora"), None));
    wire(&mut walter, "petra", 5, restored(15, b.pk("dora"), None));
    for id in [11, 12, 13, 15] {
        assert!(!walter.chain.proposal_changes.contains_key(&id), "{id} registered");
        assert!(!walter.proposals.contains_key(&id), "{id} carded");
    }
    assert!(walter.chain.proposal_changes.contains_key(&14), "one consent-less card stays votable");
}

/// One re-admission at a time (R3) must not be a lever for any member:
/// a consent-less card another node planted does not block this node's
/// verified recovery, while a genuine consented one in flight still does.
#[test]
fn a_planted_restore_card_does_not_block_a_recovery() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
    let request = |walter: &mut crate::State, ticket: &str| {
        walter.recovery.tickets.insert(ticket.to_string(), "dora".to_string());
        let kp = kp_hex_for(&b, "dora");
        let proof = crate::make_seat_proof(b.key("dora"), ticket, &kp, &b.republic_id, "", &[]);
        walter
            .cmd_net_recover_requested(
                "dora".to_string(),
                b.pk("dora"),
                kp,
                ticket.to_string(),
                proof,
                String::new(),
                Vec::new(),
                consent_for(&b, "dora", ""),
                String::new(),
                String::new(),
                (false, 0),
                None,
            )
            .expect("ack");
        walter.recovery.pending.contains_key("dora")
    };

    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(21),
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: None,
            relays: Vec::new(),
            consent: None,
        },
    );
    assert!(request(&mut walter, "t-1"), "a planted card does not block the recovery");

    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::MembershipProposed {
            id: ProposalId(22),
            op: MembershipOp::Restored,
            member: "dora".to_string(),
            identity_pk: b.pk("dora"),
            nostr_pk: None,
            relays: Vec::new(),
            consent: Some(consent_for(&b, "dora", "")),
        },
    );
    assert!(!request(&mut walter, "t-2"), "a consented re-admission in flight still blocks");
}

/// One member's own restore cards must not exhaust the registry for every
/// other seat: the bound is per seat, so a flood of self-consented cards
/// leaves another seat's genuine recovery votable.
#[test]
fn a_flood_of_one_seats_restore_cards_leaves_room_for_another_seat() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora", "erika"], 3);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    let restore = |id: u64, member: &str, relay: String| WorkspaceEvent::MembershipProposed {
        id: ProposalId(id),
        op: MembershipOp::Restored,
        member: member.to_string(),
        identity_pk: b.pk(member),
        nostr_pk: None,
        relays: vec![relay],
        consent: Some(consent_for(&b, member, "")),
    };
    for i in 0..16u64 {
        wire(&mut walter, "erika", i + 1, restore(31 + i, "erika", format!("wss://r{i}.example")));
    }
    let erika_cards = walter
        .chain
        .proposal_changes
        .values()
        .filter(|c| matches!(c, ChainChange::Membership { member, .. } if member == "erika"))
        .count();
    assert!(erika_cards < 16, "the flood is bounded per seat");
    wire(&mut walter, "petra", 100, restore(60, "dora", "wss://relay.one".to_string()));
    assert!(
        walter.chain.proposal_changes.contains_key(&60),
        "dora's recovery is registered despite erika's flood"
    );
}

/// A member's status frame as it lands from the wire, with `caps`.
fn announce(st: &mut crate::State, from: &str, caps: &[&str]) {
    st.cmd_net_mirror_decl(
        &from.to_string(),
        true,
        0,
        0,
        caps.iter().map(|c| (*c).to_string()).collect(),
    )
    .expect("declaration");
}

/// The consent a rejoiner signs from what `coord` would mint for dora.
fn minted_consent_kind(coord: &crate::State, b: &Builder) -> crate::chain::ConsentKind {
    let anchor = coord.consent_anchor_for("dora");
    let consent = crate::founding::make_restore_consent(
        b.key("dora"),
        &b.republic_id,
        "dora",
        &b.pk("dora"),
        "",
        &[],
        anchor.as_ref(),
    );
    let height = coord.chain.head.as_ref().expect("head").height + 1;
    crate::chain::verify_restore_consent(&b.republic_id, &b.pk("dora"), height, "dora", None, &[], &consent)
        .expect("the consent verifies")
}

const ROSTER: [&str; 3] = ["petra", "walter", "dora"];

/// §7e version skew: one seat that never announced the v2 capability, or
/// announced a status without the field (an older build), keeps the mint
/// at v1.
#[test]
fn a_seat_without_the_v2_capability_keeps_the_consent_at_v1() {
    let cap = molt_core::chain::RESTORE_CONSENT_V2_CAP;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&ROSTER, 2);
    let (mut petra, _tmp, _dir) = crate::tests::support::stored_chain_signer(&b, "petra", &ROSTER);
    assert!(petra.consent_anchor_for("dora").is_none(), "nobody announced");
    announce(&mut petra, "walter", &[cap]);
    assert!(petra.consent_anchor_for("dora").is_none(), "dora never announced");
    announce(&mut petra, "dora", &[]);
    assert!(petra.consent_anchor_for("dora").is_none(), "an old peer is not capable");
    assert_eq!(minted_consent_kind(&petra, &b), crate::chain::ConsentKind::V1);
}

/// Every other seat announced the capability (this node is capable by
/// running this build): the mint carries the anchor, the consent is v2.
#[test]
fn every_seat_announcing_the_v2_capability_mints_a_v2_consent() {
    let cap = molt_core::chain::RESTORE_CONSENT_V2_CAP;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&ROSTER, 2);
    let (mut petra, _tmp, _dir) = crate::tests::support::stored_chain_signer(&b, "petra", &ROSTER);
    announce(&mut petra, "walter", &["something-else", cap]);
    announce(&mut petra, "dora", &[cap]);
    assert!(petra.consent_anchor_for("dora").is_some());
    assert!(matches!(minted_consent_kind(&petra, &b), crate::chain::ConsentKind::V2 { .. }));
    // a later status without the field (a downgrade) withdraws it
    announce(&mut petra, "walter", &[]);
    assert!(petra.consent_anchor_for("dora").is_none(), "the last status wins");
}

/// Only capabilities this build knows are kept: a peer's oversized list
/// must not push `prefs.toml` past its read cap (which would reset every
/// local pref at the next open).
#[test]
fn only_known_capabilities_reach_the_prefs() {
    let cap = molt_core::chain::RESTORE_CONSENT_V2_CAP;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&ROSTER, 2);
    let (mut petra, _tmp, dir) = crate::tests::support::stored_chain_signer(&b, "petra", &ROSTER);
    let junk: Vec<String> = (0..2_000).map(|i| format!("future-cap-{i:040}")).collect();
    let mut caps: Vec<&str> = junk.iter().map(String::as_str).collect();
    caps.push(cap);
    announce(&mut petra, "walter", &caps);
    announce(&mut petra, "dora", &junk.iter().map(String::as_str).collect::<Vec<_>>());
    let known = &petra.active.as_ref().expect("active").prefs.seat_caps;
    assert_eq!(known.get("walter"), Some(&vec![cap.to_string()]));
    assert!(!known.contains_key("dora"), "unknown capabilities only = none");
    let active = petra.active.take().expect("active");
    active.handle.close(None);
    assert_eq!(molt_storage::read_prefs(&dir).seat_caps.get("walter"), Some(&vec![cap.to_string()]));
}

/// The mirror revision lives in `transport.state`, which a recovered seat
/// starts over: its capabilities follow every declaration, whatever its
/// revision, so a downgrade (or an upgrade) after a recovery is seen.
#[test]
fn capabilities_follow_a_declaration_with_a_lower_revision() {
    let cap = molt_core::chain::RESTORE_CONSENT_V2_CAP;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&ROSTER, 2);
    let (mut petra, _tmp, _dir) = crate::tests::support::stored_chain_signer(&b, "petra", &ROSTER);
    let decl = |st: &mut crate::State, from: &str, rev: u64, caps: &[&str]| {
        st.cmd_net_mirror_decl(&from.to_string(), true, 0, rev, caps.iter().map(|c| (*c).to_string()).collect())
            .expect("declaration");
    };
    decl(&mut petra, "walter", 1_700_000_000, &[cap]);
    decl(&mut petra, "dora", 1_700_000_000, &[cap]);
    assert!(petra.consent_anchor_for("dora").is_some());
    decl(&mut petra, "walter", 0, &[]);
    assert!(petra.consent_anchor_for("dora").is_none(), "a restored older build withdraws it");
    decl(&mut petra, "walter", 0, &[cap]);
    assert!(petra.consent_anchor_for("dora").is_some(), "a restored newer build announces it");
}

/// The last seen capability lives in `prefs.toml`, so a reopened node (or
/// a restored backup) mints v2 before any peer speaks again.
#[test]
fn the_v2_capability_survives_a_reopen() {
    let cap = molt_core::chain::RESTORE_CONSENT_V2_CAP;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&ROSTER, 2);
    let (mut petra, _tmp, dir) = crate::tests::support::stored_chain_signer(&b, "petra", &ROSTER);
    announce(&mut petra, "walter", &[cap]);
    announce(&mut petra, "dora", &[cap]);
    let active = petra.active.take().expect("active");
    active.handle.close(None);
    drop(petra);
    let ws = crate::tests::support::reopen(&dir);
    let mut back = chain_signer("petra", &b, b.blocks.clone());
    assert!(back.consent_anchor_for("dora").is_none(), "no prefs, no capability");
    back.active = Some(crate::ActiveStorage {
        id: "w-store".to_string(),
        dir: dir.clone(),
        prefs: molt_storage::read_prefs(&dir),
        handle: molt_storage::start_writer(ws),
    });
    assert!(back.consent_anchor_for("dora").is_some(), "the capability came back from disk");
}

/// S4a: a sealed restore whose seat was never heard from since is
/// recognized by its own latest anchor only.
#[test]
fn an_unfinished_restore_is_recognized_by_its_anchor() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    let anchor = "ab".repeat(32);
    b.commit_restored("dora", &anchor, &["petra", "walter"]);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    let height = walter.chain.blocks.last().expect("block").height;
    walter.chain.block_ts.insert(height, 1_000);
    assert!(walter.restore_never_finished("dora", &anchor));
    assert!(!walter.restore_never_finished("dora", &"cd".repeat(32)), "only its own anchor");
    assert!(!walter.restore_never_finished("petra", &anchor), "only the restored seat");
}

/// B3 fixture: `st`'s presence clock reads `now`, and it heard `member` at
/// `at` (an active workspace entry carrying the roster's pills).
fn heard(st: &mut crate::State, member: &str, at: u64, now: u64) {
    st.presence.clock_override = Some(now);
    let id = "w-gate".to_string();
    if st.session.workspaces.iter().all(|w| w.id != id) {
        st.session.active_workspace = id.clone();
        let members = molt_core::roster_members(&st.roster(), now, |_| molt_core::MemberInfo::NEVER);
        st.session.workspaces.push(molt_core::WorkspaceInfo {
            id: id.clone(),
            name: "Gate".to_string(),
            detail: "2-of-3".to_string(),
            synced: true,
            state: 0,
            last_sync_min: 0,
            sync_queue: 0,
            s3: false,
            size_kib: 0,
            last_backup_min: molt_core::WorkspaceInfo::NEVER,
            backup_copies: 0,
            backup_error: String::new(),
            has_seed: false,
            net: "none".to_string(),
            encrypted: false,
            restored: false,
            members,
            agenda: String::new(),
        });
    }
    let entry = st.session.workspaces.iter_mut().find(|w| w.id == id).expect("entry");
    if let Some(m) = entry.members.iter_mut().find(|m| m.name == member) {
        m.last_seen = at;
    } else {
        entry.members.push(molt_core::MemberInfo { name: member.to_string(), last_seen: at, state: 0, sync: String::new() });
    }
}

/// **B3: no restore of a seat heard seconds ago.** The field restored live
/// seats: each re-key evicted a running node. A seat heard within the
/// silence window is refused at ingest with `seat still active`; once it is
/// silent, the same request proposes.
#[test]
fn a_restore_of_a_seat_heard_seconds_ago_is_refused() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", kp_hex, &b.republic_id, "", &[]);
    heard(&mut coord, "dora", 10_000 - 10, 10_000);
    let refused = coord.verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], "", "");
    assert_eq!(refused, Err("seat still active".to_string()));
    assert!(coord.recovery.pending.is_empty() && coord.chain.proposal_changes.is_empty(), "nothing proposed");
    heard(&mut coord, "dora", 10_000 - 10, 10_000 + 120);
    coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], "", "")
        .expect("a silent seat restores");
}

/// **B3: a coordinator that sees a fork restores nobody** - its restore
/// would land on a branch the others do not hold (field N1/N2).
#[test]
fn a_diverged_coordinator_refuses_a_restore() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", kp_hex, &b.republic_id, "", &[]);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    heard(&mut coord, "walter", 10_000, 10_000);
    coord.chain.diverged.insert(
        "walter".to_string(),
        molt_core::ChainDivergence { peer: "walter".to_string(), since_height: 0, seen_ts: 1 },
    );
    let refused = coord.verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], "", "");
    assert_eq!(refused, Err("chain diverged".to_string()));
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    coord.chain.fork_candidates.insert(1, b.blocks[0].clone());
    let refused = coord.verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], "", "");
    assert_eq!(refused, Err("chain diverged".to_string()), "an unlinked fork candidate counts");
}

/// **A diverged seat gone silent blocks no restore** - least of all its
/// own: only its restored device could clear the entry. A diverged seat
/// still talking keeps blocking.
#[test]
fn a_silent_diverged_seat_does_not_block_its_own_restore() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", kp_hex, &b.republic_id, "", &[]);
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    heard(&mut coord, "walter", 10_000, 10_000);
    heard(&mut coord, "dora", 10_000 - 200, 10_000);
    coord.chain.diverged.insert(
        "dora".to_string(),
        molt_core::ChainDivergence { peer: "dora".to_string(), since_height: 0, seen_ts: 1 },
    );
    coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], "", "")
        .expect("the silent diverged seat restores");

    // a shorter activity window: dora is restorable, not yet silent
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    coord.seat_active.store(60, std::sync::atomic::Ordering::SeqCst);
    heard(&mut coord, "walter", 10_000, 10_000);
    heard(&mut coord, "dora", 10_000 - 90, 10_000);
    coord.chain.diverged.insert(
        "dora".to_string(),
        molt_core::ChainDivergence { peer: "dora".to_string(), since_height: 0, seen_ts: 1 },
    );
    coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], "", "")
        .expect("the restored seat's own entry does not count");

    let mut coord = chain_signer("petra", &b, b.blocks.clone());
    heard(&mut coord, "walter", 10_000 - 200, 10_000);
    heard(&mut coord, "dora", 10_000 - 200, 10_000);
    coord.chain.diverged.insert(
        "walter".to_string(),
        molt_core::ChainDivergence { peer: "walter".to_string(), since_height: 0, seen_ts: 1 },
    );
    coord
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], "", "")
        .expect("another silent diverged seat blocks nothing");
}

/// **B3: the ingest gate answers through the ordinary refusal**: the
/// operator's `recover-refused` notice, the ticket left unspent.
#[test]
fn a_ticketed_request_on_a_fork_is_refused_and_keeps_its_ticket() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let kp_hex = kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", &kp_hex, &b.republic_id, "", &[]);
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    coord.recovery.tickets.insert("t".to_string(), "dora".to_string());
    heard(&mut coord, "walter", 10_000, 10_000);
    coord.chain.diverged.insert(
        "walter".to_string(),
        molt_core::ChainDivergence { peer: "walter".to_string(), since_height: 0, seen_ts: 1 },
    );
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
            (false, 0),
            None,
        )
        .expect("ack");
    assert_eq!(coord.session.notice, "recover-refused:dora:chain diverged");
    assert!(coord.recovery.tickets.contains_key("t"), "the ticket stays live");
}

/// **B3: a coordinator on a fork mints no link**, and neither does one
/// that heard the seat seconds ago.
#[test]
fn a_link_is_not_minted_for_a_live_seat_or_on_a_fork() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    heard(&mut coord, "dora", 10_000 - 10, 10_000);
    // D3: a verdict known at mint time answers the command itself
    let refused = coord.cmd_recover_invite_start("dora".to_string());
    assert!(
        matches!(&refused, Err(molt_core::MoltError::Recover(why)) if why == "seat still active"),
        "{refused:?}"
    );
    assert_eq!(coord.session.notice, "recovery-link-failed:seat still active");
    assert!(coord.recovery.tickets.is_empty(), "no ticket minted");
    heard(&mut coord, "dora", 10_000 - 10, 10_000 + 120);
    heard(&mut coord, "walter", 10_000 + 120, 10_000 + 120);
    coord.chain.diverged.insert(
        "walter".to_string(),
        molt_core::ChainDivergence { peer: "walter".to_string(), since_height: 0, seen_ts: 1 },
    );
    let refused = coord.cmd_recover_invite_start("dora".to_string());
    assert!(
        matches!(&refused, Err(molt_core::MoltError::Recover(why)) if why == "chain diverged"),
        "{refused:?}"
    );
    assert_eq!(coord.session.notice, "recovery-link-failed:chain diverged");
}

/// **B3: a link-less request - an older build restoring itself - is
/// refused, never proposed**, and a relay replay of it is answered once.
#[test]
fn a_link_less_request_is_refused_and_never_proposed() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let kp_hex = kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "self", &kp_hex, &b.republic_id, "", &[]);
    let consent = consent_v2_for(&b, "dora", "", &[], 0, &"cc".repeat(32));
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    for _ in 0..2 {
        coord
            .cmd_net_recover_requested(
                "dora".to_string(),
                b.pk("dora"),
                kp_hex.clone(),
                "self".to_string(),
                proof.clone(),
                String::new(),
                Vec::new(),
                consent.clone(),
                String::new(),
                String::new(),
                (false, 0),
                None,
            )
            .expect("ack");
    }
    assert!(coord.chain.proposal_changes.is_empty() && coord.recovery.pending.is_empty(), "never proposed");
    assert_eq!(coord.recovery.unsolicited_cooldown.len(), 1, "answered once");
}

/// **B3: a `Restored` card proposed elsewhere (an older peer has no gate)
/// does not seal here while the seat is active**, and this node does not
/// co-sign it; it seals once the seat has gone silent.
#[test]
fn a_restored_card_does_not_seal_while_the_seat_is_active() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut older = chain_signer("petra", &b, b.blocks.clone());
    older.config.self_cosign = false;
    let kp_hex = &kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), "t", kp_hex, &b.republic_id, "", &[]);
    let consent = consent_v2_for(&b, "dora", "", &[], 0, &"cc".repeat(32));
    let id = older
        .verify_and_propose_restore("dora", &b.pk("dora"), kp_hex, "t", &proof, "", &[], &consent, "")
        .expect("proposed");
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    heard(&mut walter, "dora", 10_000 - 10, 10_000);
    walter.receive_membership_proposal(id, MembershipOp::Restored, "dora", &b.pk("dora"), None, Vec::new(), Some(consent));
    assert!(
        !walter.chain.pending_sigs.get(&id).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "no co-signature for the restore of a live seat"
    );
    let card = walter.chain.proposal_changes.get(&id).cloned().expect("card");
    let petra_sig = molt_storage::identity_sign(b.key("petra"), &molt_core::chain::approval_bytes(&b.republic_id, 1, &card));
    walter.receive_approval(id, "petra", 1, &petra_sig);
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 0, "held while dora is heard");
    assert!(walter.chain.proposal_changes.contains_key(&id), "the card stays open");
    // dora goes silent: the held approval and the held seal both go through
    heard(&mut walter, "dora", 10_000 - 10, 10_000 + 120);
    walter.retry_held_restore_approvals();
    walter.drain_held_seals();
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 1, "sealed once the seat is silent");
    verify_chain(&walter.chain.blocks).expect("verifies");
}

/// F2: a node still catching up (a peer claims a higher head) judges a
/// restore card against an old head - it holds its automatic approval
/// and signs once level.
#[test]
fn a_node_that_is_behind_holds_its_restore_approval() {
    let b = Builder::new(&["petra", "walter", "dora"], 3);
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    walter.note_peer_height("petra", 40);
    register_v2(&mut walter, &b, 7, &"dd".repeat(32), 0);
    let signed = |w: &crate::State| w.chain.pending_sigs.get(&7).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));
    assert!(walter.chain.proposal_changes.contains_key(&7), "the card registers");
    assert!(!signed(&walter), "behind: held");
    walter.chain.peer_heights.clear();
    walter.retry_held_restore_approvals();
    assert!(signed(&walter), "level: signed");
}

/// The `MembershipProposed` frame of a v2 card for dora, as petra (the
/// coordinator) sends it, and the change it registers.
fn dora_card_frame(b: &Builder, id: u64) -> (molt_core::WorkspaceEvent, ChainChange) {
    let npk = "dd".repeat(32);
    let consent = consent_v2_for(b, "dora", &npk, &[], 0, &"cc".repeat(32));
    let frame = molt_core::WorkspaceEvent::MembershipProposed {
        id: molt_core::ProposalId(id),
        op: MembershipOp::Restored,
        member: "dora".to_string(),
        identity_pk: b.pk("dora"),
        nostr_pk: Some(npk.clone()),
        relays: Vec::new(),
        consent: Some(consent.clone()),
    };
    let change = ChainChange::Membership {
        op: MembershipOp::Restored,
        member: "dora".to_string(),
        identity_pk: b.pk("dora"),
        nostr_pk: Some(npk),
        relays: Vec::new(),
        consent: Some(consent),
    };
    (frame, change)
}

/// D1 (v0.1.0 acceptance): a bystander that saw the proposer's withdrawal
/// never opens, signs, holds or seals that restore card again - whatever
/// arrives later (a re-served card, a peer's signature, the gate opening,
/// the card aging out).
#[test]
fn a_withdrawn_restore_stays_withdrawn_on_a_bystander() {
    use molt_core::{ProposalId, ProposalState, WorkspaceEvent};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    // sorted [dora, petra, walter]: petra mints ids = 2 mod 3
    let id = 8;
    let (frame, change) = dora_card_frame(&b, id);
    let petra_sig = molt_storage::identity_sign(
        b.key("petra"),
        &molt_core::chain::approval_bytes(&b.republic_id, 1, &change),
    );
    let approval = WorkspaceEvent::Approved { id: ProposalId(id), by: "petra".to_string(), height: 1, sig: petra_sig };
    let withdrawn = WorkspaceEvent::Withdrawn { id: ProposalId(id), by: "petra".to_string() };
    let own_sig =
        |w: &crate::State| w.chain.pending_sigs.get(&id).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));

    // the card first (held: dora was heard), then the withdrawal
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    heard(&mut walter, "dora", 10_000 - 10, 10_000);
    wire(&mut walter, "petra", 1, frame.clone());
    assert!(walter.recovery.held_approvals.contains(&id), "held while dora is heard");
    wire(&mut walter, "petra", 2, withdrawn.clone());
    // …then everything that used to be able to reopen it
    wire(&mut walter, "petra", 3, frame.clone());
    wire(&mut walter, "petra", 4, approval);
    heard(&mut walter, "dora", 10_000 - 10, 10_000 + 120);
    walter.retry_held_restore_approvals();
    walter.drain_held_seals();
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 0, "never sealed");
    assert!(!own_sig(&walter), "never signed");
    assert_eq!(walter.proposals.get(&id).map(|p| (p.state, p.withdrawn)), Some((ProposalState::Rejected, true)));
    assert!(!walter.chain.proposal_changes.contains_key(&id), "no longer registered");
    assert!(walter.recovery.held_approvals.is_empty() && walter.chain.seal_held.is_empty(), "nothing held");
}

/// D1: [`a_withdrawn_restore_stays_withdrawn_on_a_bystander`] with the
/// withdrawal ahead of the card, and with the card aged out.
#[test]
fn a_withdrawn_restore_stays_withdrawn_in_any_order() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let id = 8;
    let (frame, _) = dora_card_frame(&b, id);
    let withdrawn = molt_core::WorkspaceEvent::Withdrawn { id: molt_core::ProposalId(id), by: "petra".to_string() };
    let own_sig =
        |w: &crate::State| w.chain.pending_sigs.get(&id).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));

    // the withdrawal first, the card after it
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    wire(&mut walter, "petra", 1, withdrawn.clone());
    wire(&mut walter, "petra", 2, frame.clone());
    assert!(walter.proposals.get(&id).is_some_and(|p| p.withdrawn), "the parked withdrawal stands");
    assert!(!own_sig(&walter), "never signed");
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 0);

    // the withdrawn card aged out with its discussion: a late re-serve
    // must not bring it back as a fresh vote
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    heard(&mut walter, "dora", 10_000 - 10, 10_000);
    wire(&mut walter, "petra", 1, frame.clone());
    wire(&mut walter, "petra", 2, withdrawn);
    walter.retire_decided_cards(u64::MAX, 10_000);
    heard(&mut walter, "dora", 10_000 - 10, 10_000 + 120);
    wire(&mut walter, "petra", 3, frame);
    walter.retry_held_restore_approvals();
    walter.drain_held_seals();
    assert!(!own_sig(&walter), "a retired card is never signed");
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 0, "…nor sealed");
    assert!(!walter.proposals.contains_key(&id), "…nor reopened");
}

/// A link minted on `coord` for dora: the ticket registered and the link
/// shown, as the inbox task reports it.
fn mint_link_for_dora(coord: &mut crate::State, b: &Builder, ticket: &str) -> String {
    coord.recovery.tickets.insert(ticket.to_string(), "dora".to_string());
    let link = crate::recovery::RecoveryInvite {
        republic: "Chess Club".to_string(),
        member: "dora".to_string(),
        ticket: ticket.to_string(),
        server: "loopback".to_string(),
        queue_id: "ab".repeat(16),
        wrap: "cd".repeat(32),
        republic_id: b.republic_id.clone(),
        handover: None,
        consent_anchor: None,
    }
    .render();
    coord.cmd_net_recover_link_ready("dora".to_string(), link.clone(), None).expect("ack");
    link
}

/// D1b + D2 on the coordinator: a cancelled run withdraws the card and
/// shows its link again (the ticket is live); when the card seals
/// elsewhere anyway, nothing is re-keyed, the operator is told and the
/// old link is dead - a new link restores the seat.
#[test]
fn a_withdrawn_restore_sealed_elsewhere_is_reported_not_rekeyed() {
    use molt_core::ProposalState;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    coord.config.self_cosign = false;
    let ticket = "11".repeat(16);
    let link = mint_link_for_dora(&mut coord, &b, &ticket);
    let npk = "dd".repeat(32);
    let kp_hex = kp_hex_for(&b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), &ticket, &kp_hex, &b.republic_id, &npk, &[]);
    let consent = consent_v2_for(&b, "dora", &npk, &[], 0, &"cc".repeat(32));
    let id = coord
        .verify_and_propose_restore("dora", &b.pk("dora"), &kp_hex, &ticket, &proof, &npk, &[], &consent, "")
        .expect("proposed");
    // what the Ready did: the ticket spent, the link off the notice
    coord.recovery.tickets.remove(&ticket);
    coord.recovery.spent_tickets.insert(ticket.clone());
    coord.clear_link_notice(&ticket);
    assert_eq!(coord.session.notice, "", "a spent link is off the notice");
    let card = coord.chain.proposal_changes.get(&id).cloned().expect("card");

    let attempt = molt_net::invite::recover_attempt_id(&kp_hex);
    coord.cmd_net_recover_peer_failed("dora", "cancelled", "0.0.21", &attempt, &npk).expect("ack");
    assert!(coord.proposals.get(&id).is_some_and(|p| p.withdrawn), "withdrawn");
    assert!(coord.recovery.tickets.contains_key(&ticket), "the ticket is live again");
    assert_eq!(coord.session.notice, format!("recovery-link:{link}"), "D2: the live link shows again");

    // walter had signed before the withdrawal reached him: his block lands
    let seq = coord.next_seq;
    coord.receive_block(b.seal(1, card, &["walter"]));
    assert_eq!(coord.chain.head.as_ref().expect("head").height, 1, "a sealed block stands");
    assert_eq!(coord.session.notice, "restore-sealed-after-withdrawal:dora");
    assert!(coord.recovery.pending.is_empty() && coord.recovery.mesh_window.is_empty(), "no re-key");
    assert_eq!(coord.next_seq, seq, "nothing recorded: no MlsCommit");
    assert!(!coord.recovery.tickets.contains_key(&ticket), "the old link is dead");
    assert_eq!(
        coord.proposals.get(&id).map(|p| (p.state, p.withdrawn)),
        Some((ProposalState::Applied, false)),
        "the card reads as what the chain says"
    );
    // the seat restores again with a new link
    let fresh = "22".repeat(16);
    coord.recovery.tickets.insert(fresh.clone(), "dora".to_string());
    let npk2 = "ee".repeat(32);
    let kp2 = kp_hex_for(&b, "dora");
    let proof2 = crate::make_seat_proof(b.key("dora"), &fresh, &kp2, &b.republic_id, &npk2, &[]);
    let consent2 = consent_v2_for(&b, "dora", &npk2, &[], 1, &npk);
    coord
        .verify_and_propose_restore("dora", &b.pk("dora"), &kp2, &fresh, &proof2, &npk2, &[], &consent2, "")
        .expect("a new link restores the seat");
}

/// A Nostr transport on `st` (the seal pacing and the restore settle are
/// off without one), its presence clock at `now`.
fn on_nostr(st: &mut crate::State, now: u64) {
    st.nostr = Some(crate::NostrTransport {
        sk: zeroize::Zeroizing::new(vec![7u8; 32]),
        relays: vec!["ws://relay.example".to_string()],
        rotation_seed: [0u8; 32],
    });
    st.presence.clock_override = Some(now);
    // the head moved long ago: no seal pacing round in the way
    st.chain.head_moved_at = 0;
}

/// **F1 (v0.1.1):** a bystander does not sign or seal a consented restore
/// the instant it sees the card. A withdrawal right behind the card - in
/// the same burst, or seconds later - wins; without one the card signs
/// and seals once the settle has passed.
#[test]
fn a_consented_restore_settles_before_a_bystander_signs_or_seals() {
    use molt_core::{ProposalId, WorkspaceEvent};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let id = 8;
    let (frame, change) = dora_card_frame(&b, id);
    let petra_sig =
        molt_storage::identity_sign(b.key("petra"), &molt_core::chain::approval_bytes(&b.republic_id, 1, &change));
    let approval = WorkspaceEvent::Approved { id: ProposalId(id), by: "petra".to_string(), height: 1, sig: petra_sig };
    let withdrawn = WorkspaceEvent::Withdrawn { id: ProposalId(id), by: "petra".to_string() };
    let own_sig =
        |w: &crate::State| w.chain.pending_sigs.get(&id).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));
    let height = |w: &crate::State| w.chain.head.as_ref().expect("head").height;
    let settle = super::membership::RESTORE_SETTLE_SECS;

    // the withdrawal in the same burst as the card
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    on_nostr(&mut walter, 10_000);
    wire(&mut walter, "petra", 1, frame.clone());
    wire(&mut walter, "petra", 2, approval.clone());
    assert!(!own_sig(&walter), "not signed on sight");
    assert_eq!(height(&walter), 0, "not sealed on sight");
    wire(&mut walter, "petra", 3, withdrawn.clone());
    walter.presence.clock_override = Some(10_000 + settle + 1);
    walter.cmd_net_delivery_tick().expect("tick");
    assert!(!own_sig(&walter) && height(&walter) == 0, "the withdrawal won");

    // the withdrawal seconds behind the card
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    on_nostr(&mut walter, 10_000);
    wire(&mut walter, "petra", 1, frame.clone());
    wire(&mut walter, "petra", 2, approval.clone());
    walter.presence.clock_override = Some(10_000 + settle - 1);
    walter.cmd_net_delivery_tick().expect("tick");
    assert!(!own_sig(&walter) && height(&walter) == 0, "still settling");
    wire(&mut walter, "petra", 3, withdrawn);
    walter.presence.clock_override = Some(10_000 + settle + 1);
    walter.cmd_net_delivery_tick().expect("tick");
    assert!(!own_sig(&walter) && height(&walter) == 0, "the withdrawal won");

    // no withdrawal: the settle ends on a delivery tick
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    on_nostr(&mut walter, 10_000);
    wire(&mut walter, "petra", 1, frame);
    wire(&mut walter, "petra", 2, approval);
    walter.presence.clock_override = Some(10_000 + settle);
    walter.cmd_net_delivery_tick().expect("tick");
    assert_eq!(height(&walter), 1, "sealed after the settle");
    let sealed = walter.chain.blocks.last().expect("block");
    assert!(sealed.sigs.iter().any(|a| a.member == "walter"), "signed after the settle");
    verify_chain(&walter.chain.blocks).expect("verifies");
}

/// **G2:** the coordinator's withdrawal may sit in a gap of its stream (a
/// later frame of the coordinator arrived, the withdrawal did not yet): the
/// settle keeps waiting while that stream is held, so the withdrawal still
/// wins after the 5 s.
#[test]
fn a_restore_keeps_settling_while_the_coordinators_stream_has_a_gap() {
    use molt_core::{EventEnvelope, ProposalId, WorkspaceEvent};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let id = 8;
    let (frame, change) = dora_card_frame(&b, id);
    let petra_sig =
        molt_storage::identity_sign(b.key("petra"), &molt_core::chain::approval_bytes(&b.republic_id, 1, &change));
    let approval = WorkspaceEvent::Approved { id: ProposalId(id), by: "petra".to_string(), height: 1, sig: petra_sig };
    let env = |seq: u64, prev: u64, body: WorkspaceEvent| EventEnvelope {
        seq,
        ts: 1_751_000_000 + seq,
        by: "petra".to_string(),
        body,
        prev_seq: prev,
    };
    let own_sig =
        |w: &crate::State| w.chain.pending_sigs.get(&id).is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter"));
    let settle = super::membership::RESTORE_SETTLE_SECS;
    let mut walter = chain_signer("walter", &b, b.blocks.clone());
    on_nostr(&mut walter, 10_000);
    let deliver = |w: &mut crate::State, e: EventEnvelope| {
        w.cmd_net_delivered("petra".to_string(), e, None).expect("delivered");
    };
    deliver(&mut walter, env(1, 0, frame));
    deliver(&mut walter, env(2, 1, approval));
    // seq 3 (the withdrawal) is in flight; seq 4 already here
    deliver(&mut walter, env(4, 3, WorkspaceEvent::ChainRequest { from_height: 1, known: Vec::new() }));
    walter.presence.clock_override = Some(10_000 + settle + 1);
    walter.cmd_net_delivery_tick().expect("tick");
    assert!(!own_sig(&walter), "no signature while the coordinator's stream has a gap");
    deliver(&mut walter, env(3, 2, WorkspaceEvent::Withdrawn { id: ProposalId(id), by: "petra".to_string() }));
    walter.presence.clock_override = Some(10_000 + settle + 2);
    walter.cmd_net_delivery_tick().expect("tick");
    assert!(!own_sig(&walter), "the withdrawal won");
    assert_eq!(walter.chain.head.as_ref().expect("head").height, 0);
}

/// The coordinator side of a dora restore: a minted ticket, a proposed v2
/// card; returns `(id, ticket, anchor, key package)`.
fn propose_dora(coord: &mut crate::State, b: &Builder) -> (u64, String, String, String) {
    let ticket = "11".repeat(16);
    mint_link_for_dora(coord, b, &ticket);
    let npk = "dd".repeat(32);
    let kp_hex = kp_hex_for(b, "dora");
    let proof = crate::make_seat_proof(b.key("dora"), &ticket, &kp_hex, &b.republic_id, &npk, &[]);
    let consent = consent_v2_for(b, "dora", &npk, &[], 0, &"cc".repeat(32));
    let id = coord
        .verify_and_propose_restore("dora", &b.pk("dora"), &kp_hex, &ticket, &proof, &npk, &[], &consent, "")
        .expect("proposed");
    // what the Ready did
    coord.recovery.tickets.remove(&ticket);
    coord.recovery.spent_tickets.insert(ticket.clone());
    (id, ticket, npk, kp_hex)
}

/// **G6:** a re-key that never happened is not remembered as one - a bad
/// KeyPackage, or no group to re-key - so a later failure of that run is
/// not reported as `restore-abandoned`.
#[test]
fn a_failed_rekey_is_not_remembered_as_a_rekeyed_run() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    for kp in ["zz", "beef"] {
        let mut coord = genesis_seat("petra", &b, b.blocks.clone());
        on_nostr(&mut coord, 10_000);
        let _ = propose_dora(&mut coord, &b);
        coord.recovery.pending.get_mut("dora").expect("pending").key_package = kp.to_string();
        coord.coordinator_rekey("dora");
        assert!(coord.recovery.rekeyed.is_empty(), "kp {kp}: no re-key, no record");
    }
}

/// **F1 on the coordinator:** its own co-signature plus the consent can
/// seal at once in 2-of-3; the settle holds that seal too, so a cancel
/// right after Ready withdraws the card instead.
#[test]
fn a_cancel_right_after_ready_withdraws_before_the_coordinator_seals() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    coord.config.self_cosign = true;
    on_nostr(&mut coord, 10_000);
    let (id, ticket, npk, kp_hex) = propose_dora(&mut coord, &b);
    assert_eq!(coord.chain.head.as_ref().expect("head").height, 0, "settling, not sealed");

    coord.presence.clock_override = Some(10_001);
    let attempt = molt_net::invite::recover_attempt_id(&kp_hex);
    coord.cmd_net_recover_peer_failed("dora", "cancelled", "0.0.21", &attempt, &npk).expect("ack");
    assert!(coord.proposals.get(&id).is_some_and(|p| p.withdrawn), "withdrawn");
    coord.presence.clock_override = Some(10_000 + super::membership::RESTORE_SETTLE_SECS + 1);
    coord.cmd_net_delivery_tick().expect("tick");
    assert_eq!(coord.chain.head.as_ref().expect("head").height, 0, "nothing sealed");
    assert!(coord.recovery.tickets.contains_key(&ticket), "the ticket is live again");
}

/// **F2 (v0.1.1):** a run cancelled after its block sealed and the group
/// re-keyed for it is reported on the coordinator, not only logged; the
/// seat is not "heard", so a new link mints and restores it. Another
/// run's frame, or one after the seat was heard again, reports nothing.
#[test]
fn a_run_abandoned_after_its_rekey_is_reported_and_a_new_link_restores_it() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    coord.config.self_cosign = false;
    let (id, _ticket, npk, kp_hex) = propose_dora(&mut coord, &b);
    let card = coord.chain.proposal_changes.get(&id).cloned().expect("card");
    let run = coord.recovery.pending.get("dora").cloned().expect("pending");
    coord.receive_block(b.seal(1, card, &["walter"]));
    assert_eq!(coord.chain.head.as_ref().expect("head").height, 1, "sealed");
    assert!(coord.recovery.pending.is_empty(), "the re-key consumed the run");
    // this harness has no group to re-key: record what a real re-key does
    coord.note_rekeyed_run("dora", &run);
    coord.session.notice.clear();

    let other = molt_net::invite::recover_attempt_id(&kp_hex_for(&b, "dora"));
    coord.cmd_net_recover_peer_failed("dora", "cancelled", "0.0.21", &other, &npk).expect("ack");
    assert_eq!(coord.session.notice, "", "another run reports nothing");

    let attempt = molt_net::invite::recover_attempt_id(&kp_hex);
    coord.cmd_net_recover_peer_failed("dora", "cancelled", "0.0.21", &attempt, &npk).expect("ack");
    assert_eq!(coord.session.notice, "restore-abandoned:dora");
    coord.session.notice.clear();
    coord.cmd_net_recover_peer_failed("dora", "cancelled", "0.0.21", &attempt, &npk).expect("ack");
    assert_eq!(coord.session.notice, "", "reported once");

    // the seat is not heard: a new link mints and restores it
    assert_eq!(coord.restore_gate("dora"), None, "no `seat still active`");
    let fresh = "22".repeat(16);
    coord.recovery.tickets.insert(fresh.clone(), "dora".to_string());
    let npk2 = "ee".repeat(32);
    let kp2 = kp_hex_for(&b, "dora");
    let proof2 = crate::make_seat_proof(b.key("dora"), &fresh, &kp2, &b.republic_id, &npk2, &[]);
    let consent2 = consent_v2_for(&b, "dora", &npk2, &[], 1, &npk);
    coord
        .verify_and_propose_restore("dora", &b.pk("dora"), &kp2, &fresh, &proof2, &npk2, &[], &consent2, "")
        .expect("a new link restores the seat");
}

/// F2: a withdrawal that arrives after the restored seat was heard from
/// its new leaf is a stale run's - nothing to report.
#[test]
fn a_late_withdrawal_after_the_seat_came_back_reports_nothing() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut coord = genesis_seat("petra", &b, b.blocks.clone());
    coord.config.self_cosign = false;
    coord.presence.clock_override = Some(10_000);
    let (id, _ticket, npk, kp_hex) = propose_dora(&mut coord, &b);
    let card = coord.chain.proposal_changes.get(&id).cloned().expect("card");
    coord.receive_block(b.seal(1, card, &["walter"]));
    coord.session.notice.clear();
    coord.presence.clock_override = Some(10_010);
    coord.note_frame_heard("dora", 10_005);
    let attempt = molt_net::invite::recover_attempt_id(&kp_hex);
    coord.cmd_net_recover_peer_failed("dora", "cancelled", "0.0.21", &attempt, &npk).expect("ack");
    assert_eq!(coord.session.notice, "", "the seat is back");
}
