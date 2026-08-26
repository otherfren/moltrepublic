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
    assert!(!walter.proposal_changes.contains_key(&hostile), "nothing registered");
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
        peer.chain_head.as_ref().map(|h| h.height),
        Some(1),
        "the membership block adopted"
    );
    peer.proposals.insert(
        4,
        molt_core::ProposalRecord {
            surface: Surface::Organization,
            payload: json!({ "op": "restore_member", "member": "dora" }),
            approvals: 0,
            state: ProposalState::Applied,
            declined_at: 0,
            declined_by: String::new(),
            decliners: Vec::new(),
            voted: Vec::new(),
            by: String::new(),
            superseded: false,
            withdrawn: false,
        },
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
    walter2.receive_membership_proposal(6, MembershipOp::Joined, "dora", &"cd".repeat(32), None, Vec::new(), None);
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
    let kp_hex = "beef";

    // the returning member (dora) signs the seat proof with its OWN key
    let good = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
    let id = coord
        .verify_and_propose_restore(true, "dora", &b.pk("dora"), kp_hex, ticket, &good, "", &[], "", "")
        .expect("a valid seat proof re-admits");
    assert!(matches!(
        coord.proposal_changes.get(&id),
        Some(ChainChange::Membership {
            op: MembershipOp::Restored,
            ..
        })
    ));
    // a verified request registers the pending recovery (the MLS re-key
    // consumes it the moment the block commits — even synchronously)
    assert!(coord.pending_recovery.contains_key("dora"));

    // a proof signed by the WRONG key (petra forging dora's) is rejected
    let forged = crate::make_seat_proof(b.key("petra"), ticket, kp_hex, &rid, "", &[]);
    assert!(coord
        .verify_and_propose_restore(true, "dora", &b.pk("dora"), kp_hex, ticket, &forged, "", &[], "", "")
        .is_err());

    // a request that re-keys the seat to a DIFFERENT identity is rejected —
    // recovery re-derives the SAME key
    assert!(coord
        .verify_and_propose_restore(true, "dora", &b.pk("walter"), kp_hex, ticket, &good, "", &[], "", "")
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
    let head = petra.chain_head.as_ref().expect("head");
    assert_eq!(head.height, 1, "petra's signature + walter's consent reach 2-of-2");
    verify_chain(&petra.chain).expect("an adopting reader accepts the consented block");
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
    walter.cmd_approve(ProposalId(5)).expect("the survivor can approve");
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
    let kp_hex = "beef";
    let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
    let id = coord
        .verify_and_propose_restore(
            true,
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
            .pending_sigs
            .get(&id)
            .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "a consent-less restore never auto-signs - the human vote is the content"
    );
    let petra_sig = coord
        .pending_sigs
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
        walter.chain_head.as_ref().expect("head").height,
        0,
        "1 signature + no consent stays open"
    );

    // the PUBLIC approve — the exact call that answered UnknownProposal
    // before the record existed
    walter.cmd_approve(ProposalId(id)).expect("approve accepts the id");

    // petra + walter = 2-of-3: sealed, settled
    assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
    assert_eq!(
        walter.proposals.get(&id).map(|p| p.state),
        Some(ProposalState::Applied),
        "the commit settles the record"
    );
    assert!(
        !walter.pending_sigs.contains_key(&id) && !walter.proposal_changes.contains_key(&id),
        "the vote bookkeeping is dropped"
    );
    verify_chain(&walter.chain).expect("the sealed chain verifies from zero");
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
    let kp_hex = "beef";
    let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
    let consent = consent_for(&b, "dora", "");
    let id = coord
        .verify_and_propose_restore(
            true,
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
            .pending_sigs
            .get(&id)
            .is_some_and(|p| p.sigs.iter().any(|a| a.member == "walter")),
        "a verified consent auto-signs on receipt"
    );
    // …and petra's gossiped signature completes the threshold: petra +
    // walter + dora's consent = 3-of-4, sealed with no cmd_approve call
    let petra_sig = coord
        .pending_sigs
        .get(&id)
        .expect("petra's pending set")
        .sigs
        .iter()
        .find(|a| a.member == "petra")
        .expect("petra co-signed")
        .sig
        .clone();
    walter.receive_approval(id, "petra", 1, &petra_sig);
    assert_eq!(walter.chain_head.as_ref().expect("head").height, 1);
    assert_eq!(
        walter.proposals.get(&id).map(|p| p.state),
        Some(ProposalState::Applied),
        "the commit settles the record without a human approve"
    );
    verify_chain(&walter.chain).expect("the sealed chain verifies from zero");
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
    assert_eq!(petra.chain_head.as_ref().expect("head").height, 1, "sealed");
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
    let kp_hex = "beef";
    let proof = crate::make_seat_proof(b.key("dora"), ticket, kp_hex, &rid, "", &[]);
    let consent = consent_for(&b, "dora", "");
    let id = coord
        .verify_and_propose_restore(
            true,
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
            .pending_sigs
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
            .pending_sigs
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
    let kp_hex = "beef";
    let declared = vec!["wss://relay.two.example".to_string()];

    // republic pool: relay.one only — dora's declared relay bridges nobody
    let b = Builder::new_on_relays(
        &["petra", "walter", "dora"],
        2,
        vec!["wss://relay.one".to_string()],
    );
    let mut coord = chain_signer("petra", &b, b.blocks.clone());
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
            true,
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
            true,
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
        coord2.proposal_changes.get(&id),
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
    coord.pending_recovery.insert(
        "walter".to_string(),
        PendingRecovery {
            ticketed: true,
            member: "walter".to_string(),
            key_package: "beef".to_string(),
            reply: String::new(),
        },
    );

    // build a Restored block for walter and hand it to the coordinator
    let change = ChainChange::Membership {
        op: MembershipOp::Restored,
        member: "walter".to_string(),
        identity_pk: walter_pk,
        nostr_pk: None,
        relays: Vec::new(),
        consent: None,
    };
    let block = b.seal(1, change, &["petra", "walter"]);
    coord.receive_block(block);

    assert_eq!(coord.chain_head.as_ref().expect("head").height, 1);
    assert!(
        !coord.pending_recovery.contains_key("walter"),
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

/// The stamp the commit is KEYED with is the stamp it is carried at.
///
/// `CommitKey(created_at, digest)` breaks a concurrent same-epoch race,
/// and both ends must derive it from the same value — the 445 receive side
/// reads the real `created_at` off the wire. A coordinator that let the
/// outbox pick the publish time would key its own commit at one value
/// while every receiver keys it at another, and the two would pick
/// different winners under ONE epoch number, silently.
#[test]
fn the_rekey_carries_the_stamp_it_was_keyed_with() {
    let (coord, _survivor, _dora) = mls_trio();
    let returning =
        molt_net::MlsMember::new(&SigningKey::from_bytes(&[2u8; 32]), "walter").expect("kp");
    let kp = returning.key_package().expect("key package");

    let pinned = 1_759_123_456;
    let mls = std::sync::Mutex::new(coord);
    let rekey = nostr_rekey(&mls, "walter", &kp, pinned).expect("the re-key runs");
    assert_eq!(
        rekey.stamp, pinned,
        "the re-key must carry its own pinned stamp - the delivery has no other \
         source for it, and re-reading a clock is exactly the divergence"
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
    node.pending_recovery.insert(
        "dora".to_string(),
        PendingRecovery {
            ticketed: true,
            member: "dora".to_string(),
            key_package: "beef".to_string(),
            reply: String::new(),
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
    assert_eq!(node.chain_head.as_ref().expect("head").height, 1);
    // … but the re-key trigger stayed inert: no envelope of any kind was
    // recorded (make_env is the only seq stamp, so an MlsCommit broadcast
    // or a chat notice would have advanced next_seq) …
    assert_eq!(node.next_seq, seq_before, "no MlsCommit/notice was recorded");
    // … the recovery mesh window was never armed …
    assert!(node.recovery_mesh_window.is_empty());
    // … and only walter's (absent) entry was consulted — dora's pending
    // recovery is untouched
    assert!(node.pending_recovery.contains_key("dora"));
}
