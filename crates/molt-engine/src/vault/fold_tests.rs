// SPDX-License-Identifier: GPL-3.0-or-later

//! The vault cut, the held base and base-pending (plan stage S5).

use super::*;
use crate::chain::test_support::Builder;
use crate::chain::Held;
use crate::net::vault_payload::NamedPayload;
use molt_core::vault::{
    grant_id, secret_id, vault_base_canonical_bytes, SecretText, VaultCtx, VaultDeposit, VaultGrant,
};
use molt_core::{approval_bytes, ChainBlock, ChainChange, MemberIdentity, ProposalId, ProposalRecord, ProposalState};
use serde_json::Value;

const SEATS: [&str; 4] = ["a", "b", "c", "d"];

fn idx(member: &str) -> usize {
    SEATS.iter().position(|s| *s == member).expect("a seat")
}

fn seed_of(member: &str) -> [u8; 32] {
    let (entropy, npk, _) = Builder::seat_keys(idx(member));
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime")
}

/// A seat of `b`'s republic holding `chain` (pruned onto `blob` when given).
fn seat_on(member: &str, b: &Builder, blob: Option<molt_core::CheckpointState>, chain: Vec<ChainBlock>) -> crate::State {
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
    st.set_checkpoint_blob(blob);
    st.adopt_chain(chain);
    st.identity_sk = Some(b.key(member).clone());
    st.vault_seed = Some(zeroize::Zeroizing::new(seed_of(member)));
    st
}

fn seat(member: &str, b: &Builder) -> crate::State {
    seat_on(member, b, None, b.blocks.clone())
}

fn deposit(b: &Builder, depositor: &str, signer: &str, name: &str, text: &str) -> (VaultDeposit, Vec<u8>) {
    deposit_over(b, depositor, signer, name, text, "")
}

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
    let mut rng = crate::vault::deposit::os_rng().expect("rng");
    molt_vault::build_deposit(&input, &seed_of(signer), b.key(signer), &mut rng).expect("built")
}

fn op(dep: &VaultDeposit) -> Value {
    serde_json::to_value(VaultOp::Deposit(dep.clone())).expect("json")
}

fn grant_op(sid: &str, reader: &str, id: u64) -> Value {
    let g = VaultGrant { grant_id: grant_id(sid, reader, id), secret_id: sid.to_string(), reader: reader.into() };
    serde_json::to_value(VaultOp::Grant(g)).expect("json")
}

fn applied(proposal_id: u64, payload: Value) -> ChainChange {
    ChainChange::Applied { proposal_id, surface: Surface::Vault, payload }
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

/// Propose the cut at `st`'s head, co-sign it, and seal it with every
/// other seat's signature (a cut is n-of-n). Returns the sealed change.
fn seal_cut(st: &mut crate::State, b: &Builder, wiki: bool) -> ChainChange {
    let upto = st.chain.head.as_ref().expect("head").height;
    let kind = st.cut_kind(wiki);
    let hash = st.own_cut_hash(upto, kind).expect("own projection");
    let id = 50 + upto;
    st.receive_checkpoint_proposal(id, upto, &hash, wiki);
    let change = kind.change(upto, hash);
    let bytes = approval_bytes(&b.republic_id, upto + 1, &change);
    let me = st.member();
    for m in SEATS.iter().filter(|m| **m != me) {
        let sig = molt_storage::identity_sign(b.key(m), &bytes);
        st.receive_approval(id, m, upto + 1, &sig);
    }
    assert_eq!(st.chain.head.as_ref().expect("head").height, upto + 1, "the cut sealed");
    change
}

fn vault_group(blob: &molt_core::CheckpointState) -> Vec<(u64, Value)> {
    blob.applied
        .iter()
        .find(|(s, _)| *s == Surface::Vault)
        .map(|(_, g)| g.clone())
        .expect("a vault group")
}

/// A republic with one deposit `x` by `a`, and `a` holding it after a cut.
fn cut_with_x() -> (Builder, crate::State, VaultDeposit, Vec<u8>) {
    let mut b = Builder::vault();
    let (x, xf) = deposit(&b, "a", "a", "x", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let mut st = seat("a", &b);
    hold(&mut st, &b, &x, &xf);
    seal_cut(&mut st, &b, false);
    // the builder follows the holder past the cut
    b.push(st.chain.blocks[0].clone());
    (b, st, x, xf)
}

#[test]
fn a_cut_folds_the_vault_to_one_base_entry() {
    let rt = runtime();
    let _g = rt.enter();
    let (b, st, x, _) = cut_with_x();
    assert!(matches!(
        st.chain.blocks[0].change,
        ChainChange::CheckpointVault { wiki_folded: false, .. }
    ));
    assert_eq!(st.chain.blocks.len(), 1, "history below the cut is dropped");
    let blob = st.chain.checkpoint_blob.clone().expect("blob");
    let group = vault_group(&blob);
    assert_eq!(group.len(), 1, "one entry");
    let base = st.chain.vault_base.clone().expect("the base is held");
    assert_eq!(
        crate::chain::vault_base::base_commitment_of(&group[0].1),
        Some(crate::chain::vault_base::commitment(&base))
    );
    assert!(molt_core::checkpoint_canonical_bytes(&blob).starts_with(b"molt-chain-checkpoint-v10\0"));
    assert!(!st.vault_base_pending());
    let sid = secret_id(&b.republic_id, &x);
    assert!(st.vault_state().is_current(&sid), "the deposit reads from the base");
    assert_eq!(st.vault_view().deposits.len(), 1);

    // a republic that never got a deposit folds to the empty base, which
    // is derived, never fetched
    let empty = Builder::vault();
    let mut e = seat("a", &empty);
    seal_cut(&mut e, &empty, false);
    let blob = e.chain.checkpoint_blob.clone().expect("blob");
    assert_eq!(
        crate::chain::vault_base::base_commitment_of(&vault_group(&blob)[0].1),
        Some(crate::chain::vault_base::commitment(&molt_core::vault::VaultBase::default()))
    );
    let fresh = seat_on("b", &empty, Some(blob), e.chain.blocks.clone());
    assert!(!fresh.vault_base_pending(), "the empty base needs no fetch");
}

/// D16 through the engine: the replaced version and its grant are gone.
#[test]
fn the_fold_keeps_only_the_current_version_and_its_grants() {
    let rt = runtime();
    let _g = rt.enter();
    let mut b = Builder::vault();
    let (v1, _) = deposit(&b, "a", "a", "x", "one");
    let (v2, _) = deposit_over(&b, "a", "a", "x", "two", &secret_id(&b.republic_id, &v1));
    let s1 = secret_id(&b.republic_id, &v1);
    let s2 = secret_id(&b.republic_id, &v2);
    b.commit(applied(10, op(&v1)), &["a", "b"]);
    b.commit(applied(11, grant_op(&s1, "c", 11)), &["a", "b"]);
    b.commit(applied(12, op(&v2)), &["a", "b"]);
    b.commit(applied(13, grant_op(&s2, "d", 13)), &["a", "b"]);
    let mut st = seat("b", &b);
    seal_cut(&mut st, &b, false);
    let base = st.chain.vault_base.clone().expect("held");
    assert_eq!(base.deposits.len(), 1);
    assert_eq!(base.deposits[0].deposit, v2);
    assert_eq!(base.deposits[0].grants.len(), 1);
    assert_eq!(base.deposits[0].grants[0].proposal_id, 13);
    let vs = st.vault_state();
    assert_eq!(vs.grants.len(), 1);
    assert_eq!(vs.grants[0].grant.reader, "d");
    assert!(!vs.versions.contains_key(&s1));
}

/// The §4.9.5 lesson: the fold and the walk take the base held NOW.
#[test]
fn the_fold_takes_the_base_held_now() {
    let rt = runtime();
    let _g = rt.enter();
    let (mut b, mut st, _, _) = cut_with_x();
    let (y, _) = deposit(&b, "b", "b", "y", "two");
    let block = b.commit(applied(12, op(&y)), &["a", "b"]);
    st.receive_block(block);
    let upto = st.chain.head.as_ref().expect("head").height;
    let kind = st.cut_kind(false);
    let base = st.chain.vault_base.clone().expect("held");
    let hash = st.own_cut_hash(upto, kind).expect("folds onto the held base");

    // without the base: no fold, no verdict
    st.set_vault_base(None);
    assert!(st.vault_base_pending());
    assert!(st.own_cut_hash(upto, kind).is_err());
    st.set_vault_base(Some(base.clone()));
    assert_eq!(st.own_cut_hash(upto, kind).expect("again"), hash);

    // a suffix walk meeting the second cut needs the base as a parameter
    let blob = st.chain.checkpoint_blob.clone().expect("blob");
    let cut = b.seal(upto + 1, kind.change(upto, hash), &SEATS);
    let mut suffix = st.chain.blocks.clone();
    suffix.push(cut);
    let held = Held { wiki: None, vault: Some(&base) };
    assert!(crate::chain::walk_suffix_chain(&blob, &suffix, &b.republic_id, held).is_ok());
    let err = crate::chain::walk_suffix_chain(&blob, &suffix, &b.republic_id, Held::default())
        .err()
        .expect("no base, no verdict");
    assert!(err.contains("vault base"), "{err}");
}

/// Plan 1.3.6: one variant per republic kind, and once folded, always folded.
#[test]
fn a_legacy_checkpoint_in_a_vault_chain_is_refused() {
    let rt = runtime();
    let _g = rt.enter();
    let junk = "00".repeat(32);
    let mut b = Builder::vault();
    b.commit_wiki(1, "a.md", "A", &["a", "b"]);
    for legacy in [
        ChainChange::Checkpoint { upto: 1, state_hash: junk.clone() },
        ChainChange::CheckpointFolded { upto: 1, state_hash: junk.clone() },
    ] {
        let mut chain = b.blocks.clone();
        chain.push(b.seal(2, legacy, &SEATS));
        let err = crate::chain::verify_chain(&chain).expect_err("a legacy cut");
        assert!(err.contains("legacy checkpoint in a vault republic"), "{err}");
    }
    // the vault cut verifies, the wiki folded
    let mut st = seat("a", &b);
    let cut = seal_cut(&mut st, &b, true);
    let mut chain = b.blocks.clone();
    chain.push(b.seal(2, cut, &SEATS));
    crate::chain::verify_chain(&chain).expect("the vault cut verifies");
    b.push(chain[2].clone());
    // an unfolded wiki after a folded cut
    let mut later = b.blocks.clone();
    later.push(b.seal(3, ChainChange::CheckpointVault { upto: 2, state_hash: junk.clone(), wiki_folded: false }, &SEATS));
    let err = crate::chain::verify_chain(&later).expect_err("once folded, always folded");
    assert!(err.contains("after a folded cut"), "{err}");

    // and the vault cut is refused outside a vault republic
    let mut plain = Builder::new(&SEATS, 2);
    plain.commit_applied(1, &["a", "b"]);
    let mut chain = plain.blocks.clone();
    chain.push(plain.seal(2, ChainChange::CheckpointVault { upto: 1, state_hash: junk, wiki_folded: false }, &SEATS));
    let err = crate::chain::verify_chain(&chain).expect_err("a vault cut outside a vault");
    assert!(err.contains("outside a vault republic"), "{err}");
}

/// A suffix seat of `cut_with_x` that does not hold the base.
fn pending_seat(member: &str, b: &Builder, holder: &crate::State) -> crate::State {
    let blob = holder.chain.checkpoint_blob.clone().expect("blob");
    let st = seat_on(member, b, Some(blob), holder.chain.blocks.clone());
    assert!(st.chain.head.is_some(), "the suffix adopted");
    assert!(st.vault_base_pending());
    st
}

#[test]
fn base_pending_is_a_typed_refusal_not_an_empty_list() {
    let rt = runtime();
    let _g = rt.enter();
    let (b, holder, x, _) = cut_with_x();
    let mut st = pending_seat("b", &b, &holder);
    let view = st.vault_view();
    let size = holder.vault_base_committed().expect("committed").1;
    assert_eq!(view.base_pending, Some(molt_core::vault::VaultBaseProgress { have: 0, size }));
    assert!(view.deposits.is_empty() && view.grants.is_empty());

    let sid = secret_id(&b.republic_id, &x);
    let pending = |r: Result<molt_core::Reply, MoltError>| {
        assert!(matches!(r, Err(MoltError::VaultBasePending { .. })), "{r:?}");
    };
    pending(st.cmd_vault_read(sid.clone()));
    pending(st.cmd_vault_grant(sid, "c".to_string()));
    pending(st.cmd_propose_checkpoint());
    let (y, _) = deposit(&b, "c", "c", "y", "two");
    st.proposals.insert(
        60,
        ProposalRecord {
            surface: Surface::Vault,
            payload: op(&y),
            approvals: 0,
            state: ProposalState::Proposed,
            applied_at: 0,
            declined_at: 0,
            declined_by: String::new(),
            decliners: Vec::new(),
            voted: Vec::new(),
            by: "c".to_string(),
            superseded: false,
            superseded_kind: None,
            withdrawn: false,
            wiki_rev: None,
        },
    );
    pending(st.cmd_approve(ProposalId(60), None));
}

#[test]
fn base_pending_retires_no_payload_and_no_receipt() {
    let rt = runtime();
    let _g = rt.enter();
    let (b, holder, x, xf) = cut_with_x();
    let mut st = pending_seat("b", &b, &holder);
    hold(&mut st, &b, &x, &xf);
    let sid = secret_id(&b.republic_id, &x);
    st.vault_rx.status.receipts.entry(sid.clone()).or_default().insert(
        "c".to_string(),
        molt_core::vault::VaultReceipt { verified: true, rev: 1 },
    );
    st.vault_retire_at_cut();
    assert!(st.vault_payload_held(&sid), "nothing retires against a missing base");
    assert!(!crate::vault::receipts::prune_at_cut(&mut st));
    assert!(st.vault_rx.status.receipts.contains_key(&sid));
}

#[test]
fn a_base_pending_node_does_not_cosign_a_cut() {
    let rt = runtime();
    let _g = rt.enter();
    let (b, mut holder, _, _) = cut_with_x();
    let (y, _) = deposit(&b, "b", "b", "y", "two");
    let mut b = b;
    let block = b.commit(applied(12, op(&y)), &["a", "b"]);
    holder.receive_block(block);
    let upto = holder.chain.head.as_ref().expect("head").height;
    let hash = holder.own_cut_hash(upto, holder.cut_kind(false)).expect("the holder folds");
    let mut st = pending_seat("c", &b, &holder);
    st.receive_checkpoint_proposal(70, upto, &hash, false);
    assert!(st.chain.pending_sigs.is_empty(), "no co-signature without the base");
}

#[test]
fn a_tampered_vault_base_is_refetched() {
    let rt = runtime();
    let _g = rt.enter();
    let (b, holder, _, _) = cut_with_x();
    let mut st = pending_seat("b", &b, &holder);
    let bytes = vault_base_canonical_bytes(holder.chain.vault_base.as_ref().expect("held"));
    let mut tampered = bytes.clone();
    let last = tampered.len() - 2;
    tampered[last] ^= 0x01;
    let _tmp = crate::net::vault_payload::tests::attach_storage(&mut st, &[]);
    crate::net::vault_payload::tests::with_nostr(&mut st);
    let dir = st.active.as_ref().expect("storage").dir.clone();
    let handle = st.active.as_ref().expect("storage").handle.clone();
    assert!(handle.persist_vault_base_blocking(Some(tampered.clone())));
    let ws_file = dir.join("vault_base.bin");
    assert!(ws_file.exists());
    assert!(!st.adopt_vault_base(Some(tampered)));
    assert!(!ws_file.exists(), "the refused bytes are deleted");
    assert!(st.vault_base_pending(), "still pending: the beat fetches again");
    st.files.vault_base.next_try = 0;
    st.vault_base_tick();
    let want = st.vault_base_committed().map(|(h, _)| h);
    assert_eq!(st.files.vault_base.fetching, want, "a new fetch started");
    assert!(st.cmd_net_vault_base_fetched(bytes).is_ok());
    assert!(!st.vault_base_pending());
    assert_eq!(st.vault_view().deposits.len(), 1);
}

/// Plan 1.3.16: a base whose commitment m signers attested is still
/// re-verified, record by record.
#[test]
fn a_base_with_a_forged_deposit_is_never_adopted() {
    let rt = runtime();
    let _g = rt.enter();
    let mut b = Builder::vault();
    let (x, _) = deposit(&b, "a", "a", "x", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let st = seat("a", &b);
    let upto = st.chain.head.as_ref().expect("head").height;
    // `c` deals a record naming `b` as depositor
    let (forged, _) = deposit(&b, "b", "c", "y", "two");
    let base = molt_core::vault::VaultBase {
        deposits: vec![molt_core::vault::VaultBaseDeposit { deposit: forged, grants: Vec::new() }],
    };
    let (hash, size) = crate::chain::vault_base::commitment(&base);
    let mut blob = st.own_checkpoint_state(upto, false).expect("state");
    if let Some((_, g)) = blob.applied.iter_mut().find(|(s, _)| *s == Surface::Vault) {
        *g = vec![(0, serde_json::json!({ "op": "vault_base", "hash": hash, "size": size }))];
    }
    let anchor = b.seal(
        upto + 1,
        ChainChange::CheckpointVault {
            upto,
            state_hash: crate::chain::checkpoint_state_hash(&blob),
            wiki_folded: false,
        },
        &SEATS,
    );
    let mut fresh = seat_on("d", &b, Some(blob), vec![anchor]);
    assert!(fresh.vault_base_pending());
    assert!(!fresh.adopt_vault_base(Some(vault_base_canonical_bytes(&base))));
    assert!(fresh.vault_base_pending(), "never adopted on its hash alone");
}

#[test]
fn a_replaced_payload_is_retired_at_the_cut() {
    let rt = runtime();
    let _g = rt.enter();
    let mut b = Builder::vault();
    let (v1, f1) = deposit(&b, "a", "a", "x", "one");
    let (v2, f2) = deposit_over(&b, "a", "a", "x", "two", &secret_id(&b.republic_id, &v1));
    b.commit(applied(10, op(&v1)), &["a", "b"]);
    b.commit(applied(12, op(&v2)), &["a", "b"]);
    let mut st = seat("c", &b);
    hold(&mut st, &b, &v1, &f1);
    hold(&mut st, &b, &v2, &f2);
    let (s1, s2) = (secret_id(&b.republic_id, &v1), secret_id(&b.republic_id, &v2));
    assert!(st.vault_payload_held(&s1), "kept until the cut (plan 1.3.14)");
    seal_cut(&mut st, &b, false);
    assert!(st.vault_payload_held(&s1), "not before the pruned chain is written");
    st.vault_payload_tick();
    assert!(!st.vault_payload_held(&s1), "the replaced version retired");
    assert!(st.vault_payload_held(&s2), "the current one stays");
}

type Version = (VaultDeposit, Vec<u8>);

/// Two versions of `x` on the full chain, and the holder `a` that sealed
/// the cut over them.
fn two_versions_cut() -> (Builder, crate::State, [Version; 2]) {
    let mut b = Builder::vault();
    let v1 = deposit(&b, "a", "a", "x", "one");
    let v2 = deposit_over(&b, "a", "a", "x", "two", &secret_id(&b.republic_id, &v1.0));
    b.commit(applied(10, op(&v1.0)), &["a", "b"]);
    b.commit(applied(12, op(&v2.0)), &["a", "b"]);
    let mut holder = seat("a", &b);
    seal_cut(&mut holder, &b, false);
    (b, holder, [v1, v2])
}

/// `st` holds both versions; returns their secret ids.
fn hold_both(st: &mut crate::State, b: &Builder, vs: &[Version; 2]) -> (String, String) {
    for (dep, file) in vs {
        hold(st, b, dep, file);
    }
    (secret_id(&b.republic_id, &vs[0].0), secret_id(&b.republic_id, &vs[1].0))
}

fn base_bytes(holder: &crate::State) -> Vec<u8> {
    vault_base_canonical_bytes(holder.chain.vault_base.as_ref().expect("held"))
}

/// Plan 1.3.14 on the catch-up path: a seat offline at the cut adopts
/// the pruned suffix, then its base; the replaced version retires then.
#[test]
fn a_replaced_payload_retires_after_a_catch_up_onto_the_cut() {
    let rt = runtime();
    let _g = rt.enter();
    let (b, holder, vs) = two_versions_cut();
    let mut lag = seat("c", &b);
    let (s1, s2) = hold_both(&mut lag, &b, &vs);
    let anchor = holder.chain.blocks[0].clone();
    lag.chain.pending_served_blob = holder.chain.checkpoint_blob.clone();
    lag.chain.pending_blocks.insert(anchor.height, anchor.clone());
    lag.try_adopt_from_blob();
    assert_eq!(lag.chain.head.as_ref().expect("head").height, anchor.height, "the suffix adopted");
    assert!(lag.vault_base_pending());
    lag.vault_payload_tick();
    assert!(lag.vault_payload_held(&s1), "nothing retires against a missing base");
    lag.cmd_net_vault_base_fetched(base_bytes(&holder)).expect("ack");
    assert!(!lag.vault_base_pending());
    lag.vault_payload_tick();
    assert!(!lag.vault_payload_held(&s1), "the replaced version retired");
    assert!(lag.vault_payload_held(&s2), "the current one stays");
}

/// The same once a base-pending seat's base arrives.
#[test]
fn a_replaced_payload_retires_once_a_pending_base_arrives() {
    let rt = runtime();
    let _g = rt.enter();
    let (b, holder, vs) = two_versions_cut();
    let mut st = seat_on("c", &b, holder.chain.checkpoint_blob.clone(), holder.chain.blocks.clone());
    assert!(st.vault_base_pending());
    let (s1, s2) = hold_both(&mut st, &b, &vs);
    st.vault_payload_tick();
    assert!(st.vault_payload_held(&s1), "nothing retires against a missing base");
    st.cmd_net_vault_base_fetched(base_bytes(&holder)).expect("ack");
    st.vault_payload_tick();
    assert!(!st.vault_payload_held(&s1), "the replaced version retired");
    assert!(st.vault_payload_held(&s2), "the current one stays");
}

#[test]
fn holder_x_coordinates_survive_a_cut_and_a_recovery() {
    let rt = runtime();
    let _g = rt.enter();
    let mut b = Builder::vault();
    let before = ctx_of(&b);
    let (x, _) = deposit(&b, "a", "a", "x", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let mut st = seat("a", &b);
    seal_cut(&mut st, &b, false);
    assert_eq!(st.vault_ctx(), Some(before.clone()), "the anchor's founding table");
    b.push(st.chain.blocks[0].clone());
    // c recovers with a fresh transport key
    let fresh_npk = molt_net::nostr_identity(&[9u8; 32], "t").1;
    b.commit_restored("c", &fresh_npk, &["a", "b"]);
    st.receive_block(b.blocks.last().expect("restore").clone());
    assert_eq!(st.chain.head.as_ref().expect("head").height, 3, "the restore applied");
    let after = st.vault_ctx().expect("still a vault");
    assert_eq!(after, before);
    for s in SEATS {
        assert_eq!(
            crate::vault::grant::seat_x(&after, s),
            Some(u8::try_from(idx(s) + 1).expect("x")),
            "{s}"
        );
    }
    let (sk, _) = molt_vault::vault_keypair(&seed_of("c"));
    molt_vault::check_my_share(&x, &b.republic_id, &after, "c", &sk).expect("c's share still opens");
}

/// The stored `vault_seed` once the spawned persist had its turn.
async fn stored_seed(st: &crate::State) -> Option<Vec<u8>> {
    let handle = st.active.as_ref().expect("storage").handle.clone();
    for _ in 0..100 {
        if let Some(seed) = handle.load_transport_state().await.vault_seed {
            return Some(seed.0.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    None
}

#[tokio::test]
async fn a_recovered_seat_without_a_matching_seed_refuses_to_persist_it() {
    // the fixture's keys were derived for another identity: a mismatch
    let b = Builder::vault();
    let (entropy, _, _) = Builder::seat_keys(0);
    let mut st = seat("a", &b);
    let _tmp = crate::net::vault_payload::tests::attach_storage(&mut st, &[]);
    st.vault_seed = None;
    assert!(!st.vault_seed_from_entropy(&entropy));
    assert!(st.vault_seed.is_none(), "a mismatch is never kept");
    assert_eq!(stored_seed(&st).await, None, "nor stored");

    // a founding table keyed for this seat's real identity: re-derived,
    // and after a cut it reads the anchor's founding table
    let mut b = Builder::vault();
    let (mut ids, _) = genesis_of(&b);
    let npk = ids[0].nostr_pk.clone();
    let pk = ids[0].identity_pk.clone();
    ids[0].vault_pk = crate::vault::seat_vault_key(&entropy, &npk, &pk).1;
    b.reseal_genesis(ids);
    let mut holder = seat("b", &b);
    seal_cut(&mut holder, &b, false);
    let blob = holder.chain.checkpoint_blob.clone().expect("blob");
    let mut st = seat_on("a", &b, Some(blob), holder.chain.blocks.clone());
    let _tmp = crate::net::vault_payload::tests::attach_storage(&mut st, &[]);
    st.vault_seed = None;
    assert!(st.vault_seed_from_entropy(&entropy));
    let want = molt_vault::derive_vault_seed(&entropy, &npk, &pk);
    assert_eq!(st.vault_seed.as_deref(), Some(&*want));
    assert_eq!(stored_seed(&st).await.as_deref(), Some(&want[..]), "and stored");
}

/// A cut is all-or-nothing on disk: a vault base that fails to land
/// takes the wiki base written before it back.
#[test]
fn a_failed_vault_base_write_rolls_the_wiki_base_back() {
    let rt = runtime();
    let _g = rt.enter();
    let mut b = Builder::vault();
    b.commit_wiki(1, "a.md", "A", &["a", "b"]);
    let (x, xf) = deposit(&b, "a", "a", "x", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let mut st = seat("a", &b);
    let _tmp = crate::net::vault_payload::tests::attach_storage(&mut st, &[]);
    hold(&mut st, &b, &x, &xf);
    let dir = st.active.as_ref().expect("storage").dir.clone();
    std::fs::create_dir_all(dir.join("vault_base.bin").join("x")).expect("a blocking directory");

    seal_cut(&mut st, &b, true);
    assert_eq!(st.chain.blocks.len(), b.blocks.len() + 1, "full history kept");
    assert!(st.chain.wiki_base.is_none());
    assert!(!dir.join("wiki_base.bin").exists(), "the new wiki base stayed on disk");
}

/// A prepared republic (keyed, `memory` only) and `a` pruned onto its
/// first cut.
fn prepared_cut() -> (Builder, crate::State) {
    let mut b = Builder::new_with_features(&SEATS, 2, &["memory"], true);
    b.commit_org(5, "set_name", "x", &["a", "b"]);
    let mut st = seat("a", &b);
    seal_cut(&mut st, &b, false);
    b.push(st.chain.blocks[0].clone());
    (b, st)
}

/// Review HIGH-1: an anchor blob with keys but no feature set is refused
/// before anything else is trusted.
#[test]
fn an_anchor_with_keys_and_no_feature_set_is_refused() {
    let (b, st) = prepared_cut();
    let mut blob = st.chain.checkpoint_blob.clone().expect("pruned onto the cut");
    blob.founding_features = None;
    // every seat re-signs the forged state, so only the roster rule is left
    let anchor = &st.chain.blocks[0];
    let ChainChange::CheckpointVault { upto, wiki_folded, .. } = anchor.change.clone() else {
        panic!("a vault cut");
    };
    let state_hash = crate::chain::checkpoint_state_hash(&blob);
    let forged = b.seal(anchor.height, ChainChange::CheckpointVault { upto, state_hash, wiki_folded }, &SEATS);
    let err = crate::chain::verify_suffix_chain(&blob, &[forged], &b.republic_id, None)
        .expect_err("keys without a feature set");
    assert!(err.contains("feature set"), "{err}");
}

/// Review LOW-4 (E5): a suffix holder on a prepared blob refuses a vault
/// block before the enabling block, and takes it after.
#[test]
fn a_suffix_holder_refuses_a_vault_block_before_enabling() {
    let (b, mut st) = prepared_cut();
    let (dep, _) = deposit(&b, "a", "a", "n", "one");
    let mut early = b.clone();
    let blk = early.commit(applied(10, op(&dep)), &["a", "b"]);
    let len = st.chain.blocks.len();
    st.receive_block(blk);
    assert_eq!(st.chain.blocks.len(), len, "not adopted before enabling");

    let mut late = b.clone();
    let on = late.commit(
        ChainChange::Applied {
            proposal_id: 9,
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_features", "value": "memory vault" }),
        },
        &["a", "b"],
    );
    let blk = late.commit(applied(10, op(&dep)), &["a", "b"]);
    st.receive_block(on);
    st.receive_block(blk);
    assert_eq!(st.chain.blocks.len(), len + 2, "adopted after the enabling block");
    assert!(st.is_vault_republic());
}
