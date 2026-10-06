// SPDX-License-Identifier: GPL-3.0-or-later

//! Deposit unit tests (vault build plan S3b): the verify-gated approve, the
//! wire and chain checks, replace and reorg, the view and the text scan.

use super::*;
use crate::chain::test_support::{wire, Builder};
use molt_core::vault::{deposit_signing_bytes, grant_id, share_aad, VaultPayloadRef};
use molt_core::{MemberIdentity, ProposalId, ProposalRecord, WorkspaceEvent};
use serde_json::json;
use std::sync::{Arc, Mutex};

const SEATS: [&str; 4] = ["a", "b", "c", "d"];

fn idx(member: &str) -> usize {
    SEATS.iter().position(|s| *s == member).expect("a seat")
}

fn seed_of(i: usize) -> [u8; 32] {
    let (entropy, npk, _) = Builder::seat_keys(i);
    *molt_vault::derive_vault_seed(&entropy, &npk, "id")
}

fn genesis_of(b: &Builder) -> (Vec<MemberIdentity>, Option<Vec<String>>) {
    let ChainChange::Genesis { identities, features, .. } = &b.blocks[0].change else {
        panic!("block 0 is not a genesis");
    };
    (identities.clone(), features.clone())
}

fn ctx_of(b: &Builder) -> VaultCtx {
    crate::vault::ctx_from_founding(2, &genesis_of(b).0).expect("a v6 genesis")
}

/// A seat of `b`'s republic holding `chain`, its identity key and its seed.
fn seat(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
    let (identities, features) = genesis_of(b);
    let mut st = crate::tests::plain_state();
    st.replica = Some(crate::ReplicaState {
        name: "Chess Club".to_string(),
        member: member.to_string(),
        roster: SEATS.iter().map(ToString::to_string).collect(),
        rule_m: 2,
        identities,
        agenda: "play chess".to_string(),
        features,
        republic_id: b.republic_id.clone(),
        founded_ts: 0,
    });
    st.adopt_chain(chain);
    st.identity_sk = Some(b.key(member).clone());
    st.vault_seed = Some(zeroize::Zeroizing::new(seed_of(idx(member))));
    st
}

/// A deposit of `depositor`, built and signed by `signer` (a forger when
/// the two differ).
fn deposit(b: &Builder, depositor: &str, signer: &str, name: &str, text: &str) -> (VaultDeposit, Vec<u8>) {
    deposit_over(b, depositor, signer, name, text, "")
}

/// [`deposit`] replacing the version `replaces`.
fn deposit_over(b: &Builder, depositor: &str, signer: &str, name: &str, text: &str, replaces: &str) -> (VaultDeposit, Vec<u8>) {
    let ctx = ctx_of(b);
    let text = SecretText(text.to_string());
    let input = molt_vault::DepositInput {
        republic_id: &b.republic_id,
        depositor,
        name,
        kind: "text",
        replaces,
        text: &text,
        ctx: &ctx,
    };
    let mut rng = os_rng().expect("rng");
    molt_vault::build_deposit(&input, &seed_of(idx(signer)), b.key(signer), &mut rng).expect("built")
}

fn op(dep: &VaultDeposit) -> Value {
    serde_json::to_value(VaultOp::Deposit(dep.clone())).expect("json")
}

fn applied(proposal_id: u64, payload: Value) -> ChainChange {
    ChainChange::Applied { proposal_id, surface: Surface::Vault, payload }
}

fn resign(b: &Builder, signer: &str, dep: &mut VaultDeposit) {
    dep.sig_depositor = molt_storage::identity_sign(b.key(signer), &deposit_signing_bytes(&b.republic_id, dep));
}

fn hold(st: &mut crate::State, b: &Builder, dep: &VaultDeposit, file: &[u8]) {
    let named = NamedPayload {
        secret_id: secret_id(&b.republic_id, dep),
        hash: dep.payload.hash.clone(),
        size: dep.payload.size,
    };
    st.vault_sync_held();
    assert!(st.vault_store_payload(&named, file), "held");
}

fn card(payload: Value, by: &str) -> ProposalRecord {
    ProposalRecord {
        surface: Surface::Vault,
        payload,
        approvals: 0,
        state: ProposalState::Proposed,
        applied_at: 0,
        declined_at: 0,
        declined_by: String::new(),
        decliners: Vec::new(),
        voted: Vec::new(),
        by: by.to_string(),
        superseded: false,
        superseded_kind: None,
        withdrawn: false,
        wiki_rev: None,
    }
}

fn signed_by(st: &crate::State, id: u64) -> Vec<String> {
    st.chain
        .pending_sigs
        .get(&id)
        .map(|p| p.sigs.iter().map(|a| a.member.clone()).collect())
        .unwrap_or_default()
}

fn refused(r: Result<Reply, MoltError>, want: &VaultRefusal) {
    match r {
        Err(MoltError::Vault(got)) if &got == want => {}
        other => panic!("expected `{want}`, got {other:?}"),
    }
}

#[test]
fn a_deposit_is_refused_outside_a_vault_republic() {
    for features in [&["vault"][..], &["memory"][..]] {
        let b = Builder::new_with_features(&SEATS, 2, features, false);
        let mut st = seat("a", &b, b.blocks.clone());
        assert!(!st.is_vault_republic());
        refused(
            st.cmd_vault_seal("n".into(), "text".into(), SecretText("one".into())),
            &VaultRefusal::NoVault,
        );
        assert!(st.proposals.is_empty());
    }
}

#[test]
fn approve_waits_for_the_payload() {
    let b = Builder::vault();
    let mut st = seat("b", &b, b.blocks.clone());
    let (dep, file) = deposit(&b, "a", "a", "n", "one");
    assert!(st.receive_proposed(5, Surface::Vault, op(&dep), "a"));
    refused(st.cmd_approve(ProposalId(5), None), &VaultRefusal::PayloadNotHeld);
    assert!(signed_by(&st, 5).is_empty(), "no signature left this seat");

    st.cmd_net_vault_payload_fetched(dep.payload.hash.clone(), file).expect("ack");
    assert!(st.vault_payload_held(&secret_id(&b.republic_id, &dep)));
    st.cmd_approve(ProposalId(5), None).expect("approved once held");
    assert_eq!(signed_by(&st, 5), ["b"]);
}

#[test]
fn approve_refuses_a_share_that_fails_feldman() {
    let b = Builder::vault();
    let (mut dep, file) = deposit(&b, "a", "a", "n", "one");
    // b's share replaced by one off the polynomial, properly sealed to b
    let sid = secret_id(&b.republic_id, &dep);
    let b_pk = &ctx_of(&b).holders_in_genesis_order[1].2;
    let bad = molt_vault::seal_share(b_pk, &molt_vault::Share::from_bytes([9; 32]), &share_aad(&sid, "b"), &[3; 32])
        .expect("sealed");
    let i = dep.holders.iter().position(|h| h == "b").expect("b holds");
    dep.enc_share[i] = hex::encode(bad);
    resign(&b, "a", &mut dep);

    let mut st = seat("b", &b, b.blocks.clone());
    assert!(st.receive_proposed(5, Surface::Vault, op(&dep), "a"), "well-formed and signed");
    hold(&mut st, &b, &dep, &file);
    refused(st.cmd_approve(ProposalId(5), None), &VaultRefusal::NotVerified);
    assert!(signed_by(&st, 5).is_empty());

    // c's share is intact
    let mut c = seat("c", &b, b.blocks.clone());
    assert!(c.receive_proposed(5, Surface::Vault, op(&dep), "a"));
    hold(&mut c, &b, &dep, &file);
    c.cmd_approve(ProposalId(5), None).expect("c verifies its own share");
}

#[test]
fn a_forged_depositor_is_refused_by_the_approver() {
    let b = Builder::vault();
    // c seals a record naming a as the depositor
    let (dep, file) = deposit(&b, "a", "c", "n", "one");
    let mut st = seat("b", &b, b.blocks.clone());
    assert!(!st.receive_proposed(5, Surface::Vault, op(&dep), "c"), "dropped at ingest");
    assert!(!st.proposals.contains_key(&5));

    // the same card reaching the store without ingest (a replay, a re-vote)
    st.proposals.insert(5, card(op(&dep), "c"));
    hold(&mut st, &b, &dep, &file);
    refused(st.cmd_approve(ProposalId(5), None), &VaultRefusal::NotVerified);
    st.chain_sign_and_gossip_approval(5);
    assert!(signed_by(&st, 5).is_empty(), "no path signs it");
}

#[test]
fn a_seat_without_a_vault_seed_refuses_to_approve() {
    let b = Builder::vault();
    let (dep, file) = deposit(&b, "a", "a", "n", "one");
    let mut st = seat("b", &b, b.blocks.clone());
    st.vault_seed = None;
    assert!(st.receive_proposed(5, Surface::Vault, op(&dep), "a"));
    hold(&mut st, &b, &dep, &file);
    refused(st.cmd_approve(ProposalId(5), None), &VaultRefusal::NoVaultKey);
    assert!(signed_by(&st, 5).is_empty());
    refused(
        st.cmd_vault_seal("m".into(), "text".into(), SecretText("two".into())),
        &VaultRefusal::NoVaultKey,
    );
}

#[test]
fn verify_chain_accepts_an_honest_deposit() {
    let mut b = Builder::vault();
    let (dep, _) = deposit(&b, "a", "a", "n", "one");
    b.commit(applied(10, op(&dep)), &["a", "b"]);
    crate::chain::verify_chain(&b.blocks).expect("an honest deposit verifies");
}

#[test]
fn verify_chain_rejects_a_deposit_with_a_forged_signature() {
    let mut b = Builder::vault();
    let (dep, _) = deposit(&b, "a", "c", "n", "one");
    b.commit(applied(10, op(&dep)), &["a", "b"]);
    let err = crate::chain::verify_chain(&b.blocks).expect_err("a forged depositor");
    assert!(err.contains("signature"), "{err}");
}

#[test]
fn verify_chain_rejects_a_deposit_with_a_wrong_holder_order() {
    let mut b = Builder::vault();
    let (mut dep, _) = deposit(&b, "a", "a", "n", "one");
    dep.holders.swap(0, 1);
    dep.enc_share.swap(0, 1);
    resign(&b, "a", &mut dep);
    b.commit(applied(10, op(&dep)), &["a", "b"]);
    let err = crate::chain::verify_chain(&b.blocks).expect_err("holders out of genesis order");
    assert!(err.contains("holders"), "{err}");
}

#[test]
fn verify_chain_rejects_a_vault_base_op_outside_a_cut() {
    let mut b = Builder::vault();
    let base = serde_json::to_value(VaultOp::VaultBase(VaultPayloadRef { hash: "ab".repeat(32), size: 64 }))
        .expect("json");
    b.commit(applied(10, base), &["a", "b"]);
    let err = crate::chain::verify_chain(&b.blocks).expect_err("vault_base in a block");
    assert!(err.contains("vault_base"), "{err}");
}

#[test]
fn verify_chain_rejects_an_unknown_vault_op_in_a_vault_chain() {
    let mock = json!({ "op": "seal_secret", "name": "x" });
    let mut b = Builder::vault();
    b.commit(applied(10, mock.clone()), &["a", "b"]);
    let err = crate::chain::verify_chain(&b.blocks).expect_err("seal_secret in a v6 chain");
    assert!(err.contains("unknown op"), "{err}");

    // the v5 mock keeps its generic op
    let mut v5 = Builder::new_with_features(&SEATS, 2, &["vault"], false);
    v5.commit(applied(10, mock), &["a", "b"]);
    crate::chain::verify_chain(&v5.blocks).expect("the mock verifies");
}

#[test]
fn verify_chain_rejects_a_grant_whose_id_does_not_recompute() {
    let mut b = Builder::vault();
    let (dep, _) = deposit(&b, "a", "a", "n", "one");
    b.commit(applied(10, op(&dep)), &["a", "b"]);
    let sid = secret_id(&b.republic_id, &dep);
    let grant = |gid: String, reader: &str| {
        serde_json::to_value(VaultOp::Grant(VaultGrant { grant_id: gid, secret_id: sid.clone(), reader: reader.into() }))
            .expect("json")
    };
    let mut ok = b.clone();
    ok.commit(applied(12, grant(grant_id(&sid, "d", 12), "d")), &["a", "b"]);
    crate::chain::verify_chain(&ok.blocks).expect("an honest grant verifies");

    let mut bad = b.clone();
    bad.commit(applied(12, grant(grant_id(&sid, "d", 13), "d")), &["a", "b"]);
    assert!(crate::chain::verify_chain(&bad.blocks).is_err());
    let mut stranger = b;
    stranger.commit(applied(12, grant(grant_id(&sid, "e", 12), "e")), &["a", "b"]);
    assert!(crate::chain::verify_chain(&stranger.blocks).is_err());
}

#[test]
fn a_rejoiner_refuses_a_chain_with_a_forged_deposit() {
    let base = Builder::vault();
    let mut forged = base.clone();
    let (dep, _) = deposit(&base, "a", "c", "n", "one");
    forged.commit(applied(10, op(&dep)), &["a", "b"]);

    // a fresh node: no adopted chain, the candidate judged by its own genesis
    let mut st = seat("d", &base, Vec::new());
    assert!(st.chain.head.is_none());
    assert!(st.walk_own(&forged.blocks).is_err());
    st.adopt_chain(forged.blocks.clone());
    assert!(st.chain.head.is_none(), "the forged chain is not adopted");

    let mut honest = base.clone();
    let (dep, _) = deposit(&base, "a", "a", "n", "one");
    honest.commit(applied(10, op(&dep)), &["a", "b"]);
    st.adopt_chain(honest.blocks.clone());
    assert!(st.chain.head.is_some());
}

#[test]
fn a_replace_supersedes_pending_grants_and_keeps_the_old_payload() {
    let mut b = Builder::vault();
    let (x, xf) = deposit(&b, "a", "a", "n", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let mut st = seat("c", &b, b.blocks.clone());
    hold(&mut st, &b, &x, &xf);
    let sid_x = secret_id(&b.republic_id, &x);
    let grant = VaultGrant { grant_id: grant_id(&sid_x, "d", 12), secret_id: sid_x.clone(), reader: "d".into() };
    let gop = serde_json::to_value(VaultOp::Grant(grant)).expect("json");
    assert!(st.receive_proposed(12, Surface::Vault, gop.clone(), "d"));

    let (y, _) = deposit_over(&b, "a", "a", "n", "two", &secret_id(&b.republic_id, &x));
    let block = b.commit(applied(14, op(&y)), &["a", "b"]);
    wire(&mut st, "a", 1, WorkspaceEvent::Committed(block));
    assert_eq!(st.chain.head.as_ref().map(|h| h.height), Some(2), "the replace applied");

    let g = st.proposals.get(&12).expect("the grant card");
    assert_eq!(g.state, ProposalState::Rejected);
    assert!(g.superseded);
    assert!(st.vault_payload_held(&sid_x), "the old payload stays until the cut");
    assert!(st.vault_named_payloads().iter().any(|n| n.secret_id == sid_x), "still held mandatorily");

    let view = st.vault_view();
    let sid_y = secret_id(&b.republic_id, &y);
    assert_eq!(view.deposits.len(), 1);
    assert_eq!(view.deposits[0].secret_id, sid_y);
    assert_eq!(view.deposits[0].replaces.as_deref(), Some(sid_x.as_str()));

    // a grant on the old version learned late registers superseded too
    assert!(st.receive_proposed(16, Surface::Vault, {
        let g = VaultGrant { grant_id: grant_id(&sid_x, "d", 16), secret_id: sid_x.clone(), reader: "d".into() };
        serde_json::to_value(VaultOp::Grant(g)).expect("json")
    }, "d"));
    assert!(st.proposals.get(&16).is_some_and(|p| p.superseded));
}

fn grant_op(sid: &str, reader: &str, id: u64) -> Value {
    let g = VaultGrant { grant_id: grant_id(sid, reader, id), secret_id: sid.to_string(), reader: reader.into() };
    serde_json::to_value(VaultOp::Grant(g)).expect("json")
}

/// The reopen path (cards replay first, then the chain re-projects)
/// reaches the same superseded state a live node did.
#[test]
fn a_replace_supersede_survives_a_reopen() {
    let mut b = Builder::vault();
    let (x, _) = deposit(&b, "a", "a", "n", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let sid_x = secret_id(&b.republic_id, &x);
    let (y, _) = deposit_over(&b, "a", "a", "n", "two", &secret_id(&b.republic_id, &x));
    b.commit(applied(14, op(&y)), &["a", "b"]);

    let mut st = seat("c", &b, Vec::new());
    st.proposals.insert(12, card(grant_op(&sid_x, "d", 12), "d"));
    st.adopt_chain(b.blocks.clone());
    assert!(st.chain.head.is_some());
    let g = st.proposals.get(&12).expect("the grant card");
    assert_eq!(g.state, ProposalState::Rejected);
    assert!(g.superseded);
}

#[test]
fn a_reorged_replace_keeps_the_old_payload() {
    let mut b = Builder::vault();
    let (x, xf) = deposit(&b, "a", "a", "n", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let fork_base = b.clone();
    let (y, yf) = deposit_over(&b, "a", "a", "n", "two", &secret_id(&b.republic_id, &x));
    let y_block = b.commit(applied(14, op(&y)), &["a", "b"]);
    let mut st = seat("c", &b, b.blocks.clone());
    hold(&mut st, &b, &x, &xf);
    hold(&mut st, &b, &y, &yf);
    let (sid_x, sid_y) = (secret_id(&b.republic_id, &x), secret_id(&b.republic_id, &y));
    assert!(st.vault_state().is_current(&sid_y));
    assert!(st.receive_proposed(16, Surface::Vault, grant_op(&sid_x, "d", 16), "d"));
    assert!(st.proposals.get(&16).is_some_and(|p| p.superseded), "a grant on the replaced version");

    // a contender at the replace's height that wins the tip tie-break
    let link = |blk: &ChainBlock| {
        molt_storage::content_hash(&molt_core::chain::block_link_bytes(&b.republic_id, blk))
    };
    let z = (100..)
        .map(|n| {
            fork_base.seal(
                2,
                ChainChange::Applied { proposal_id: n, surface: Surface::Memory, payload: json!({ "op": "add_note", "id": n }) },
                &["a", "b"],
            )
        })
        .find(|z| link(z) < link(&y_block))
        .expect("a smaller contender");
    wire(&mut st, "b", 1, WorkspaceEvent::Committed(z.clone()));
    assert_eq!(st.chain.blocks.last(), Some(&z), "the replace was displaced");

    let vs = st.vault_state();
    assert!(vs.is_current(&sid_x), "the old version is current again");
    assert!(!vs.versions.contains_key(&sid_y));
    assert!(st.vault_payload_held(&sid_x), "and its file is still here");
    assert!(st.vault_payload_held(&sid_y), "nothing was deleted");
    let g = st.proposals.get(&16).expect("the grant card");
    assert_eq!(g.state, ProposalState::Proposed, "the grant is a vote again");
    assert!(!g.superseded && g.superseded_kind.is_none());
}

/// The deep re-base (`try_reorg`): both branches' payloads stay, the
/// projection follows the adopted branch, a grant the new branch
/// re-commits is not displaced, one it drops is.
#[test]
fn a_deep_reorg_follows_the_adopted_branch() {
    let mut base = Builder::vault();
    let (x, xf) = deposit(&base, "a", "a", "n", "one");
    base.commit(applied(10, op(&x)), &["a", "b"]);
    let rid = base.republic_id.clone();
    let sid_x = secret_id(&rid, &x);
    let (y, yf) = deposit_over(&base, "a", "a", "n", "two", &sid_x);
    let sid_y = secret_id(&rid, &y);

    let mut lose = base.clone();
    lose.commit(applied(12, grant_op(&sid_x, "d", 12)), &["a", "b"]);
    lose.commit(applied(13, grant_op(&sid_x, "b", 13)), &["a", "b"]);
    lose.commit(applied(14, op(&y)), &["a", "b"]);
    let note = |n: u64| ChainChange::Applied {
        proposal_id: n,
        surface: Surface::Memory,
        payload: json!({ "op": "add_note", "id": n }),
    };
    let lost_at_2 = crate::chain::block_hash(&rid, &lose.blocks[2]);
    let mut win = (100..)
        .map(|n| {
            let mut w = base.clone();
            w.commit(note(n), &["a", "b"]);
            w
        })
        .find(|w| crate::chain::block_hash(&rid, &w.blocks[2]) < lost_at_2)
        .expect("a smaller fork block");
    win.commit(applied(12, grant_op(&sid_x, "d", 12)), &["a", "b"]);
    win.commit(note(99), &["a", "b"]);

    let mut st = seat("c", &lose, lose.blocks.clone());
    hold(&mut st, &lose, &x, &xf);
    hold(&mut st, &lose, &y, &yf);
    assert!(st.receive_proposed(16, Surface::Vault, grant_op(&sid_x, "c", 16), "c"));
    assert!(st.proposals.get(&16).is_some_and(|p| p.superseded));

    for blk in [&win.blocks[4], &win.blocks[3], &win.blocks[2]] {
        st.receive_block_from("b", blk.clone());
    }
    assert_eq!(st.chain.blocks, win.blocks, "re-based onto the other branch");
    let vs = st.vault_state();
    assert!(vs.is_current(&sid_x));
    assert!(!vs.versions.contains_key(&sid_y));
    assert_eq!(vs.grants.len(), 1);
    assert!(st.vault_payload_held(&sid_x) && st.vault_payload_held(&sid_y));
    assert_eq!(
        st.vault_seams.displaced_grants(),
        [grant_id(&sid_x, "b", 13)],
        "only the grant the new branch drops"
    );
    assert_eq!(st.proposals.get(&16).map(|p| p.state), Some(ProposalState::Proposed));
}

/// Plan 1.3.15: a node without the base signs no cut, its own included.
#[test]
fn a_base_pending_node_signs_no_cut() {
    let mut b = Builder::new(&["petra", "walter"], 2);
    b.commit_applied(1, &["petra", "walter"]);
    let mut petra = crate::chain::test_support::chain_signer("petra", &b, b.blocks.clone());
    let mut walter = crate::chain::test_support::chain_signer("walter", &b, b.blocks.clone());
    let id = match petra.cmd_propose_checkpoint().expect("propose") {
        Reply::Proposed { id, .. } => id.0,
        other => panic!("unexpected: {other:?}"),
    };
    let Some(ChainChange::Checkpoint { upto, state_hash }) = petra.chain.proposal_changes.get(&id).cloned() else {
        panic!("a cut");
    };

    let mut pending = crate::chain::test_support::chain_signer("petra", &b, b.blocks.clone());
    pending.vault_seams.set_base_pending(true);
    assert!(
        matches!(pending.cmd_propose_checkpoint(), Err(MoltError::VaultBasePending { .. })),
        "no own cut"
    );
    assert!(pending.chain.pending_sigs.is_empty(), "no own signature");

    walter.vault_seams.set_base_pending(true);
    walter.receive_checkpoint_proposal(id, upto, &state_hash, false);
    assert!(!walter.chain.pending_sigs.contains_key(&id), "no co-signature");
    walter.vault_seams.set_base_pending(false);
    walter.receive_checkpoint_proposal(id, upto, &state_hash, false);
    assert!(walter.chain.pending_sigs.contains_key(&id), "signs once the base is held");
}

#[test]
fn generic_propose_on_vault_is_refused_in_a_vault_republic() {
    let b = Builder::vault();
    let mut st = seat("a", &b, b.blocks.clone());
    let (dep, _) = deposit(&b, "a", "a", "n", "one");
    refused(st.cmd_propose(Surface::Vault, op(&dep)), &VaultRefusal::UseVaultSeal);
    refused(st.cmd_propose(Surface::Vault, json!({ "op": "seal_secret", "name": "x" })), &VaultRefusal::UseVaultSeal);
    assert!(st.proposals.is_empty());

    let v5 = Builder::new_with_features(&SEATS, 2, &["vault"], false);
    let mut mock = seat("a", &v5, v5.blocks.clone());
    mock.cmd_propose(Surface::Vault, json!({ "op": "seal_secret", "name": "x" }))
        .expect("the v5 mock keeps its generic op");
}

#[test]
fn the_view_lists_name_kind_and_size_but_never_text() {
    let b = Builder::vault();
    let mut st = seat("a", &b, b.blocks.clone());
    let text = "correct horse battery staple";
    st.cmd_vault_seal("n".into(), "password".into(), SecretText(text.into())).expect("sealed");
    let view = st.vault_view();
    assert!(view.real);
    assert_eq!(view.deposits.len(), 1);
    let d = &view.deposits[0];
    assert_eq!((d.name.as_str(), d.kind.as_str()), ("n", "password"));
    assert_eq!(d.size, u64::try_from(text.len() + molt_vault::PAYLOAD_OVERHEAD).expect("size"));
    assert_eq!(d.state, VaultDepositState::Pending);
    assert!(d.mine && d.held && d.proposal.is_some());
    assert_eq!(d.holders, 3);
    assert_eq!(signed_by(&st, d.proposal.expect("pending")), ["a"], "the depositor co-signs");
    let json = serde_json::to_string(&view).expect("json");
    assert!(!json.contains(text));
}

/// Collects every formatted tracing line.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut b) = self.0.lock() {
            b.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn the_sealed_text_is_never_persisted() {
    let b = Builder::vault();
    let text = "zebra-umbrella-42";
    let captured = Captured::default();
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .finish();
    let (a, c) = tracing::subscriber::with_default(subscriber, || {
        let mut a = seat("a", &b, b.blocks.clone());
        let cmd = molt_core::Command::VaultSeal {
            name: "n".into(),
            kind: "text".into(),
            text: SecretText(text.into()),
        };
        tracing::debug!(?cmd, "dispatch");
        a.cmd_vault_seal("n".into(), "text".into(), SecretText(text.into())).expect("sealed");
        let (id, p) = a.proposals.iter().next().map(|(i, p)| (*i, p.clone())).expect("the card");
        let mut c = seat("c", &b, b.blocks.clone());
        assert!(c.receive_proposed(id, Surface::Vault, p.payload.clone(), "a"));
        c.cmd_approve(ProposalId(id), None).expect_err("not held yet");
        (a, c)
    });
    let logs = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
    assert!(logs.contains("dispatch"), "the capture works");
    assert!(!logs.contains(text), "a log line carries the text");
    for st in [&a, &c] {
        let cards = serde_json::to_string(&st.proposals).expect("json");
        let chain = serde_json::to_string(&st.chain.blocks).expect("json");
        assert!(!cards.contains(text) && !chain.contains(text));
        assert!(!format!("{:?}", st.proposals).contains(text));
    }
}

/// Risk 6 of the plan: n = 13, m at its bound, the longest labels.
#[test]
fn the_largest_roster_deposit_fits_the_proposal_budget() {
    let names: Vec<String> = (0..13).map(|i| format!("seat-{i:02}-with-a-longer-name")).collect();
    let mut keys = Vec::new();
    let mut holders = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let seed = [u8::try_from(i + 1).expect("small"); 32];
        let (sk, pk) = molt_storage::derive_identity_key(&seed, n);
        holders.push((n.clone(), pk, molt_vault::vault_keypair(&seed).1));
        keys.push((sk, seed));
    }
    let ctx = VaultCtx { m: 11, holders_in_genesis_order: holders };
    let text = SecretText("x".repeat(molt_core::vault::VAULT_PAYLOAD_MAX));
    let name = "n".repeat(molt_core::vault::VAULT_NAME_MAX);
    let kind = "k".repeat(molt_core::vault::VAULT_KIND_MAX);
    let input = molt_vault::DepositInput {
        republic_id: &"ab".repeat(32),
        depositor: &names[0],
        name: &name,
        kind: &kind,
        replaces: "",
        text: &text,
        ctx: &ctx,
    };
    let (dep, _) =
        molt_vault::build_deposit(&input, &keys[0].1, &keys[0].0, &mut os_rng().expect("rng")).expect("built");
    assert_eq!(dep.enc_share.len(), 12);
    assert!(crate::proposals::payload_fits(Surface::Vault, &op(&dep), &names));
}

/// `x` committed, then `y` replacing it, then a grant on `y` (proposal 16).
fn replaced() -> (Builder, VaultDeposit, VaultDeposit, String, String) {
    let mut b = Builder::vault();
    let (x, _) = deposit(&b, "a", "a", "n", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let sid_x = secret_id(&b.republic_id, &x);
    let (y, _) = deposit_over(&b, "a", "a", "n", "two", &sid_x);
    b.commit(applied(14, op(&y)), &["a", "b"]);
    let sid_y = secret_id(&b.republic_id, &y);
    b.commit(applied(16, grant_op(&sid_y, "d", 16)), &["a", "b"]);
    (b, x, y, sid_x, sid_y)
}

fn vault_group(st: &crate::State) -> Vec<(u64, Value)> {
    st.chain
        .blocks
        .iter()
        .filter_map(|blk| match &blk.change {
            ChainChange::Applied { proposal_id, surface: Surface::Vault, payload } => Some((*proposal_id, payload.clone())),
            _ => None,
        })
        .collect()
}

/// A replayed signed record (an older version, or the current one again)
/// commits void: the slot and its grants stay, live and at the cut.
#[test]
fn a_replayed_deposit_commits_void_and_changes_nothing() {
    for replay in [0usize, 1] {
        let (mut b, x, y, _, sid_y) = replaced();
        let again = [&x, &y][replay];
        b.commit(applied(20, op(again)), &["a", "b"]);
        let st = seat("c", &b, b.blocks.clone());
        let vs = st.vault_state();
        assert!(vs.is_current(&sid_y), "replay {replay} rolled the slot");
        assert_eq!(vs.grants.len(), 1);
        assert!(!vs.grants[0].void, "replay {replay} voided the grant");

        let (_, base) = crate::chain::vault_base::summarize(&vault_group(&st), None, &b.republic_id).expect("folds");
        assert_eq!(base.deposits.len(), 1);
        assert_eq!(base.deposits[0].deposit, y, "replay {replay} at the cut");
        assert_eq!(base.deposits[0].grants.len(), 1);
    }
}

#[test]
fn approve_and_the_wire_refuse_a_stale_deposit() {
    let (b, x, y, sid_x, _) = replaced();
    let mut st = seat("c", &b, b.blocks.clone());
    for (id, dep) in [(20u64, &x), (22, &y)] {
        assert!(st.vault_wire_check(id, &op(dep)).is_err(), "the wire took a replay");
        st.proposals.insert(id, card(op(dep), "d"));
        refused(st.cmd_approve(ProposalId(id), None), &VaultRefusal::Stale);
    }
    // a replace naming a version this node has not seen yet is not judged
    let (z, _) = deposit_over(&b, "a", "a", "n", "three", &"ab".repeat(32));
    assert!(st.vault_wire_check(24, &op(&z)).is_ok());
    // a stale replace of the old version is
    let (w, _) = deposit_over(&b, "a", "a", "n", "four", &sid_x);
    assert!(st.vault_wire_check(26, &op(&w)).is_err());
}

#[test]
fn a_seal_names_the_current_version() {
    let (b, _, _, _, sid_y) = replaced();
    let mut st = seat("a", &b, b.blocks.clone());
    let _tmp = crate::net::vault_payload::tests::attach_storage(&mut st, &[]);
    st.cmd_vault_seal("n".into(), "text".into(), SecretText("three".into())).expect("sealed");
    st.cmd_vault_seal("m".into(), "text".into(), SecretText("one".into())).expect("sealed");
    let deps: Vec<VaultDeposit> = st
        .proposals
        .values()
        .filter_map(|p| match serde_json::from_value::<VaultOp>(p.payload.clone()) {
            Ok(VaultOp::Deposit(d)) => Some(d),
            _ => None,
        })
        .collect();
    let of = |name: &str| deps.iter().find(|d| d.name == name).map(|d| d.replaces.clone()).expect("a card");
    assert_eq!(of("n"), sid_y);
    assert_eq!(of("m"), "");
}

#[test]
fn a_racing_pending_replace_is_superseded_by_the_commit() {
    let mut b = Builder::vault();
    let (x, _) = deposit(&b, "a", "a", "n", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let sid_x = secret_id(&b.republic_id, &x);
    let mut st = seat("c", &b, b.blocks.clone());
    let (z, _) = deposit_over(&b, "a", "a", "n", "three", &sid_x);
    assert!(st.receive_proposed(18, Surface::Vault, op(&z), "a"));
    let (y, _) = deposit_over(&b, "a", "a", "n", "two", &sid_x);
    let block = b.commit(applied(14, op(&y)), &["a", "b"]);
    wire(&mut st, "a", 1, WorkspaceEvent::Committed(block));
    let p = st.proposals.get(&18).expect("the card");
    assert!(p.state == ProposalState::Rejected && p.superseded, "the racing replace is superseded");
}
