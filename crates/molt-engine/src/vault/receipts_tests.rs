// SPDX-License-Identifier: GPL-3.0-or-later

//! Receipt and complaint unit tests (vault build plan S3c): what a card
//! counts, last-wins per holder, the reveal verdicts and the re-seal.

use super::*;
use crate::chain::test_support::Builder;
use crate::net::vault_payload::NamedPayload;
use molt_core::vault::{
    secret_id, VaultComplaintStatus, VaultComplaintView, VaultCtx, VaultDeposit, VaultDepositState,
    VaultDepositView, VaultOp,
};
use molt_core::{ChainBlock, ChainChange, MemberIdentity, Surface, TransportState};
use serde_json::Value;
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

fn deposit(b: &Builder, depositor: &str, name: &str, text: &str) -> (VaultDeposit, Vec<u8>) {
    deposit_over(b, depositor, name, text, "")
}

fn deposit_over(b: &Builder, depositor: &str, name: &str, text: &str, replaces: &str) -> (VaultDeposit, Vec<u8>) {
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
    let mut rng = {
        use rand_chacha::rand_core::SeedableRng as _;
        rand_chacha::ChaCha20Rng::from_seed([7; 32])
    };
    molt_vault::build_deposit(&input, &seed_of(idx(depositor)), b.key(depositor), &mut rng).expect("built")
}

fn op(dep: &VaultDeposit) -> Value {
    serde_json::to_value(VaultOp::Deposit(dep.clone())).expect("json")
}

/// A 2-of-4 vault republic with one committed deposit of `a`.
fn committed() -> (Builder, VaultDeposit, Vec<u8>, String) {
    let mut b = Builder::vault();
    let (dep, file) = deposit(&b, "a", "n", "one");
    b.commit(
        ChainChange::Applied { proposal_id: 10, surface: Surface::Vault, payload: op(&dep) },
        &["a", "b"],
    );
    let sid = secret_id(&b.republic_id, &dep);
    (b, dep, file, sid)
}

fn hold(st: &mut crate::State, dep: &VaultDeposit, sid: &str, file: &[u8]) {
    let named = NamedPayload { secret_id: sid.to_string(), hash: dep.payload.hash.clone(), size: dep.payload.size };
    st.vault_sync_held();
    assert!(st.vault_store_payload(&named, file), "held");
}

fn receipt(st: &mut crate::State, from: &str, sid: &str, verdict: &str, rev: u64) {
    st.cmd_net_vault_receipt(&from.to_string(), sid.to_string(), verdict.to_string(), rev).expect("ack");
}

/// `a`'s honest reveal of `holder`'s share.
fn reveal(st: &mut crate::State, b: &Builder, dep: &VaultDeposit, holder: &str) {
    let (share, ikm) =
        molt_vault::rederive_share(dep, &b.republic_id, &ctx_of(b), &seed_of(0), holder).expect("re-dealt");
    st.cmd_net_vault_reveal(
        &"a".to_string(),
        secret_id(&b.republic_id, dep),
        holder.to_string(),
        SecretHex(hex::encode(share.as_bytes())),
        SecretHex(hex::encode(*ikm)),
    )
    .expect("ack");
}

fn card(st: &crate::State, sid: &str) -> VaultDepositView {
    st.vault_view().deposits.into_iter().find(|d| d.secret_id == sid).expect("the card")
}

fn line(holder: &str, status: VaultComplaintStatus) -> VaultComplaintView {
    VaultComplaintView { holder: holder.to_string(), status }
}

#[test]
fn sealed_counts_holders_other_than_the_depositor() {
    let (b, _, _, sid) = committed();
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "a", &sid, "verified", 1);
    receipt(&mut st, "b", &sid, "verified", 1);
    let c = card(&st, &sid);
    assert_eq!((c.verified, c.holders), (1, 3), "the depositor's own receipt does not count");
    assert_eq!(c.state, VaultDepositState::Committed);
    receipt(&mut st, "c", &sid, "verified", 1);
    let c = card(&st, &sid);
    assert_eq!(c.verified, 2);
    assert_eq!(c.state, VaultDepositState::Sealed);
}

#[test]
fn hardened_needs_all_n_minus_1() {
    let (b, _, _, sid) = committed();
    let mut st = seat("a", &b, b.blocks.clone());
    receipt(&mut st, "b", &sid, "verified", 1);
    receipt(&mut st, "c", &sid, "verified", 1);
    assert_eq!(card(&st, &sid).state, VaultDepositState::Sealed);
    receipt(&mut st, "d", &sid, "verified", 1);
    let c = card(&st, &sid);
    assert_eq!(c.verified, 3);
    assert_eq!(c.state, VaultDepositState::Hardened);
    assert!(c.complaints.is_empty());
    assert_eq!(c.readable_by, 2);
}

#[test]
fn a_complaint_holds_the_card_below_sealed() {
    let (b, _, _, sid) = committed();
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "b", &sid, "verified", 1);
    receipt(&mut st, "c", &sid, "verified", 1);
    assert_eq!(card(&st, &sid).state, VaultDepositState::Sealed);
    receipt(&mut st, "c", &sid, "complaint", 2);
    let c = card(&st, &sid);
    assert_eq!(c.state, VaultDepositState::Committed);
    assert_eq!(c.verified, 1);
    assert_eq!(c.complaints, [line("c", VaultComplaintStatus::Open)]);
    assert!(!c.reseal);
}

#[test]
fn receipts_are_last_wins_per_holder_and_survive_a_restart() {
    let (b, _, _, sid) = committed();
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "b", &sid, "verified", 5);
    receipt(&mut st, "b", &sid, "complaint", 3);
    assert_eq!(card(&st, &sid).verified, 1, "an older revision loses");
    receipt(&mut st, "c", &sid, "verified", 1);
    receipt(&mut st, "c", &sid, "complaint", 2);
    let before = card(&st, &sid);
    assert_eq!(before.verified, 1);
    assert_eq!(before.complaints, [line("c", VaultComplaintStatus::Open)]);

    // the store rides transport.state: a restart reads it back
    let ts = TransportState { vault_status: st.vault_rx.status.clone(), ..TransportState::default() };
    let ts: TransportState =
        serde_json::from_str(&serde_json::to_string(&ts).expect("json")).expect("parsed");
    let mut again = seat("d", &b, b.blocks.clone());
    again.vault_rx = ReceiptRuntime::loaded(ts.vault_status);
    assert_eq!(card(&again, &sid), before);
}

#[test]
fn a_receipt_from_a_non_holder_is_ignored() {
    let (b, _, _, sid) = committed();
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "a", &sid, "verified", 1);
    receipt(&mut st, "e", &sid, "verified", 1);
    receipt(&mut st, "b", &sid, "bogus", 1);
    assert!(st.vault_rx.status.receipts.get(&sid).map_or(true, BTreeMap::is_empty));
    receipt(&mut st, "b", &sid, "verified", 1);
    let held: Vec<&String> = st.vault_rx.status.receipts.get(&sid).expect("b").keys().collect();
    assert_eq!(held, ["b"]);
}

#[test]
fn chained_false_complaints_floor_readable_by_at_zero() {
    let (b, dep, _, sid) = committed();
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "b", &sid, "complaint", 1);
    receipt(&mut st, "c", &sid, "complaint", 1);
    assert_eq!(card(&st, &sid).readable_by, 2);
    reveal(&mut st, &b, &dep, "b");
    let c = card(&st, &sid);
    assert_eq!(c.readable_by, 1);
    assert_eq!(c.complaints, [line("b", VaultComplaintStatus::False), line("c", VaultComplaintStatus::Open)]);
    reveal(&mut st, &b, &dep, "c");
    assert_eq!(card(&st, &sid).readable_by, 0);
    reveal(&mut st, &b, &dep, "d");
    let c = card(&st, &sid);
    assert_eq!(c.readable_by, 0, "floored");
    assert_eq!(c.complaints.len(), 2, "d never complained");
    assert!(!c.reseal, "not this seat's deposit");
}

#[test]
fn an_unsolicited_reveal_names_no_one_but_still_counts() {
    let (b, dep, _, sid) = committed();
    let mut st = seat("c", &b, b.blocks.clone());
    receipt(&mut st, "d", &sid, "verified", 1);
    reveal(&mut st, &b, &dep, "d");
    let c = card(&st, &sid);
    assert!(c.complaints.is_empty(), "d never complained");
    assert_eq!(c.readable_by, 1, "d's share is public");
}

/// A reveal of `holder` by `a`: its honest share with `c`'s ephemeral
/// (a lie that still publishes the real share), or a flipped share with
/// the right ephemeral (a lie that publishes nothing usable).
fn lie(st: &mut crate::State, b: &Builder, dep: &VaultDeposit, holder: &str, share_valid: bool) {
    let ctx = ctx_of(b);
    let (share, ikm) = molt_vault::rederive_share(dep, &b.republic_id, &ctx, &seed_of(0), holder).expect("dealt");
    let (_, other_ikm) = molt_vault::rederive_share(dep, &b.republic_id, &ctx, &seed_of(0), "c").expect("c");
    let mut bytes = *share.as_bytes();
    let ikm = if share_valid {
        *other_ikm
    } else {
        bytes[0] ^= 1;
        *ikm
    };
    st.cmd_net_vault_reveal(
        &"a".to_string(),
        secret_id(&b.republic_id, dep),
        holder.to_string(),
        SecretHex(hex::encode(bytes)),
        SecretHex(hex::encode(ikm)),
    )
    .expect("ack");
}

fn restarted(st: &crate::State, b: &Builder, member: &str) -> crate::State {
    let json = serde_json::to_string(&st.vault_rx.status).expect("json");
    let mut again = seat(member, b, b.blocks.clone());
    again.vault_rx = ReceiptRuntime::loaded(serde_json::from_str(&json).expect("parsed"));
    again
}

#[test]
fn a_lie_that_publishes_the_share_counts_after_a_restart() {
    let (b, dep, _, sid) = committed();
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "b", &sid, "complaint", 1);
    lie(&mut st, &b, &dep, "b", true);
    assert_eq!(card(&st, &sid).readable_by, 1);
    let again = restarted(&st, &b, "d");
    let c = card(&again, &sid);
    assert_eq!(c.complaints, [line("b", VaultComplaintStatus::Lie)]);
    assert_eq!(c.readable_by, 1, "the published share survives the restart");
}

#[test]
fn an_honest_reveal_after_a_lie_counts_the_share() {
    let (b, dep, _, sid) = committed();
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "b", &sid, "complaint", 1);
    lie(&mut st, &b, &dep, "b", false);
    assert_eq!(card(&st, &sid).readable_by, 2, "the flipped share is no share");
    reveal(&mut st, &b, &dep, "b");
    let c = card(&st, &sid);
    assert_eq!(c.complaints, [line("b", VaultComplaintStatus::Lie)], "a lie stays a lie");
    assert_eq!(c.readable_by, 1);
    assert_eq!(card(&restarted(&st, &b, "d"), &sid).readable_by, 1);
}

#[test]
fn a_replaced_version_gets_no_receipt() {
    let (mut b, old_dep, old_file, old) = committed();
    let (y, _) = deposit_over(&b, "a", "n", "two", &old);
    b.commit(ChainChange::Applied { proposal_id: 12, surface: Surface::Vault, payload: op(&y) }, &["a", "b"]);
    let mut st = seat("c", &b, b.blocks.clone());
    hold(&mut st, &old_dep, &old, &old_file);
    on_payload_held(&mut st);
    assert!(!st.vault_rx.status.receipts.contains_key(&old), "replaced");
}

#[test]
fn a_reveal_names_a_bad_share_and_a_lie() {
    let (b, dep, _, sid) = committed();
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "b", &sid, "complaint", 1);
    receipt(&mut st, "c", &sid, "complaint", 1);
    // b: the honest share with the wrong ephemeral recomputes nothing
    let (share, _) = molt_vault::rederive_share(&dep, &b.republic_id, &ctx_of(&b), &seed_of(0), "b").expect("b");
    let (_, other_ikm) = molt_vault::rederive_share(&dep, &b.republic_id, &ctx_of(&b), &seed_of(0), "c").expect("c");
    st.cmd_net_vault_reveal(
        &"a".to_string(),
        sid.clone(),
        "b".to_string(),
        SecretHex(hex::encode(share.as_bytes())),
        SecretHex(hex::encode(*other_ikm)),
    )
    .expect("ack");
    // a reveal sent by anyone but the depositor is dropped
    st.cmd_net_vault_reveal(
        &"b".to_string(),
        sid.clone(),
        "c".to_string(),
        SecretHex(hex::encode(share.as_bytes())),
        SecretHex(hex::encode(*other_ikm)),
    )
    .expect("ack");
    let c = card(&st, &sid);
    assert_eq!(c.complaints, [line("b", VaultComplaintStatus::Lie), line("c", VaultComplaintStatus::Open)]);
    assert_eq!(c.readable_by, 1, "the lie still published b's real share");
}

#[test]
fn the_depositor_answers_a_complaint_and_is_offered_a_reseal() {
    let (b, _, _, sid) = committed();
    let mut st = seat("a", &b, b.blocks.clone());
    receipt(&mut st, "b", &sid, "complaint", 1);
    let c = card(&st, &sid);
    assert_eq!(c.complaints, [line("b", VaultComplaintStatus::False)], "decided at the depositor too");
    assert_eq!(c.readable_by, 1);
    assert!(c.reseal && c.mine);
}

#[test]
fn a_holder_checks_once_the_payload_is_held() {
    let (b, dep, file, sid) = committed();
    let mut st = seat("c", &b, b.blocks.clone());
    assert!(!st.vault_rx.checked.contains_key(&sid), "no payload, no verdict");
    hold(&mut st, &dep, &sid, &file);
    on_payload_held(&mut st);
    assert_eq!(st.vault_rx.checked.get(&sid), Some(&true));
    let own = st.vault_rx.status.receipts.get(&sid).and_then(|r| r.get("c")).expect("its own receipt");
    assert!(own.verified);
    assert_eq!(card(&st, &sid).verified, 1, "its own receipt counts here");

    // the lab switch: the receipt complains, the check stays real
    let mut lab = seat("d", &b, b.blocks.clone());
    lab.vault_seams.lab.complain.store(true, std::sync::atomic::Ordering::SeqCst);
    hold(&mut lab, &dep, &sid, &file);
    on_payload_held(&mut lab);
    let own = lab.vault_rx.status.receipts.get(&sid).and_then(|r| r.get("d")).expect("its own receipt");
    assert!(!own.verified);
    assert_eq!(lab.vault_rx.checked.get(&sid), Some(&true));
}

#[test]
fn a_seat_without_a_vault_seed_sends_no_receipt() {
    let (b, dep, file, sid) = committed();
    let mut st = seat("c", &b, b.blocks.clone());
    st.vault_seed = None;
    hold(&mut st, &dep, &sid, &file);
    on_payload_held(&mut st);
    assert!(st.vault_rx.status.receipts.is_empty());
}

#[test]
fn receipts_of_replaced_versions_are_pruned_at_a_cut() {
    let (mut b, _, _, old) = committed();
    let (y, _) = deposit_over(&b, "a", "n", "two", &old);
    b.commit(ChainChange::Applied { proposal_id: 12, surface: Surface::Vault, payload: op(&y) }, &["a", "b"]);
    let new = secret_id(&b.republic_id, &y);
    let mut st = seat("d", &b, b.blocks.clone());
    receipt(&mut st, "b", &old, "verified", 1);
    receipt(&mut st, "b", &new, "verified", 1);

    st.vault_seams.set_base_pending(true);
    prune_at_cut(&mut st);
    assert!(st.vault_rx.status.receipts.contains_key(&old), "never while base-pending");
    st.vault_seams.set_base_pending(false);
    prune_at_cut(&mut st);
    assert!(!st.vault_rx.status.receipts.contains_key(&old));
    assert!(st.vault_rx.status.receipts.contains_key(&new));
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
fn the_resealed_text_is_never_persisted() {
    let mut b = Builder::vault();
    let text = "violet-anchor-58";
    let (dep, file) = deposit(&b, "a", "n", text);
    b.commit(ChainChange::Applied { proposal_id: 10, surface: Surface::Vault, payload: op(&dep) }, &["a", "b"]);
    let sid = secret_id(&b.republic_id, &dep);
    let captured = Captured::default();
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .finish();
    let st = tracing::subscriber::with_default(subscriber, || {
        let mut st = seat("a", &b, b.blocks.clone());
        hold(&mut st, &dep, &sid, &file);
        receipt(&mut st, "b", &sid, "complaint", 1);
        let opened = reseal_text(&dep, &b.republic_id, &seed_of(0), &file).expect("re-opened");
        tracing::debug!(?opened, "reseal");
        st.cmd_vault_seal(dep.name.clone(), dep.kind.clone(), opened).expect("re-sealed");
        st
    });
    let logs = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf8");
    assert!(logs.contains("reseal"), "the capture works");
    assert!(!logs.contains(text), "a log line carries the text");
    let cards = serde_json::to_string(&st.proposals).expect("json");
    let chain = serde_json::to_string(&st.chain.blocks).expect("json");
    let status = serde_json::to_string(&st.vault_rx.status).expect("json");
    for (what, s) in [("cards", &cards), ("chain", &chain), ("status", &status)] {
        assert!(!s.contains(text), "the text is in the {what}");
    }
    let resealed = st
        .proposals
        .values()
        .filter(|p| p.state == molt_core::ProposalState::Proposed)
        .find_map(|p| match serde_json::from_value::<VaultOp>(p.payload.clone()) {
            Ok(VaultOp::Deposit(d)) => Some(d),
            _ => None,
        })
        .expect("the re-seal card");
    assert_ne!(resealed.nonce, dep.nonce, "a fresh nonce");
    assert_ne!(resealed.commitments[0], dep.commitments[0], "a fresh secret");
}

#[test]
fn a_reseal_is_refused_for_a_deposit_of_another_seat() {
    let (b, dep, file, sid) = committed();
    let mut st = seat("b", &b, b.blocks.clone());
    hold(&mut st, &dep, &sid, &file);
    let before = st.proposals.len();
    assert!(st.cmd_vault_reseal(sid).is_err());
    assert_eq!(st.proposals.len(), before);
}

#[test]
fn the_lab_env_var_is_ignored_without_the_feature() {
    assert!(!lab_complain_from(None));
    assert!(!lab_complain_from(Some("0")));
    assert_eq!(lab_complain_from(Some("1")), cfg!(feature = "vault-lab"));
}

#[test]
fn vault_lab_feature_is_off_by_default() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for manifest in ["crates/molt-engine/Cargo.toml", "crates/molt-app/Cargo.toml"] {
        let text = std::fs::read_to_string(root.join(manifest)).expect("manifest");
        let default = text.lines().find(|l| l.trim_start().starts_with("default")).unwrap_or_default();
        assert!(!default.contains("vault-lab"), "{manifest}: vault-lab is a default");
        assert!(text.contains("vault-lab"), "{manifest}: the feature exists");
    }
    let release = std::fs::read_to_string(root.join("scripts/build-release.sh")).expect("release script");
    assert!(!release.contains("vault-lab"), "the release build names vault-lab");
}

fn queue(st: &mut crate::State, room: usize) {
    st.vault_rx.test_queue.get_or_insert_with(TestQueue::default).room = room;
}

fn sent_receipts(st: &crate::State) -> Vec<String> {
    st.vault_rx
        .test_queue
        .as_ref()
        .map(|q| q.sent.iter().filter_map(|f| match f {
            VaultFrame::Receipt(r) => Some(r.secret_id.clone()),
            _ => None,
        }).collect())
        .unwrap_or_default()
}

fn pending_card(payload: Value, by: &str) -> molt_core::ProposalRecord {
    molt_core::ProposalRecord {
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

/// `c` holds a verified receipt of each of `n` pending deposits of `a`.
fn many_receipts(n: u64) -> crate::State {
    let b = Builder::vault();
    let mut st = seat("c", &b, b.blocks.clone());
    for i in 0..n {
        let (dep, _) = deposit(&b, "a", &format!("n{i}"), "one");
        let sid = secret_id(&b.republic_id, &dep);
        st.proposals.insert(100 + i, pending_card(op(&dep), "a"));
        st.vault_rx.status.receipts.entry(sid.clone()).or_default().insert("c".to_string(), VaultReceipt { verified: true, rev: 1 });
        st.vault_rx.checked.insert(sid, true);
    }
    st
}

/// The resend never floods the control queue: a small batch per beat, a
/// full queue resumes where it stopped, and the round ends once.
#[test]
fn the_resend_is_paced_and_resumes_after_a_full_queue() {
    let mut st = many_receipts(40);
    queue(&mut st, 10);
    tick(&mut st, 1_000);
    assert_eq!(sent_receipts(&st).len(), 10, "stopped at the full queue");
    assert_eq!(st.vault_rx.resent_at, 0, "the round is not done");
    for _ in 0..3 {
        queue(&mut st, 64);
        tick(&mut st, 1_001);
        assert!(st.vault_rx.test_queue.as_ref().map_or(0, |q| q.room) >= 64 - RESEND_BATCH, "one batch per beat");
    }
    let sent = sent_receipts(&st);
    assert_eq!(sent.len(), 40, "each once");
    assert_eq!(sent.iter().collect::<BTreeSet<_>>().len(), 40);
    assert_eq!(st.vault_rx.resent_at, 1_001);
    tick(&mut st, 1_002);
    assert_eq!(sent_receipts(&st).len(), 40, "not again before the next round");
}

/// The startup sweep re-checks a deposit whose receipt is stored; the
/// unchanged receipt goes out once, with the resend round.
#[test]
fn an_unchanged_receipt_is_not_sent_twice_at_start() {
    let (b, dep, file, sid) = committed();
    let mut st = seat("c", &b, b.blocks.clone());
    hold(&mut st, &dep, &sid, &file);
    st.vault_rx.status.receipts.entry(sid.clone()).or_default().insert("c".to_string(), VaultReceipt { verified: true, rev: 5 });
    queue(&mut st, 64);
    tick(&mut st, 1_000);
    assert_eq!(sent_receipts(&st), [sid]);
}
