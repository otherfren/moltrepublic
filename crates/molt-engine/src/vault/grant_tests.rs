// SPDX-License-Identifier: GPL-3.0-or-later

//! Grant and read unit tests (vault build plan S4): the grant id, void
//! grants, answer attribution and AAD, the founding reader key, bad
//! answers, nothing secret persisted, and the displaced audit (D17).

use super::*;
use crate::chain::test_support::{wire, Builder};
use molt_core::vault::{secret_id, SecretText, VaultGrantState};
use molt_core::{ChainBlock, ChainChange, MemberIdentity, ProposalId, ProposalRecord, WorkspaceEvent};
use std::sync::{Arc, Mutex};

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

fn seat(member: &str, b: &Builder, chain: Vec<ChainBlock>) -> crate::State {
    let (identities, features) = genesis_of(b);
    let mut st = crate::tests::plain_state();
    st.replica = Some(crate::ReplicaState {
        name: "R".to_string(),
        member: member.to_string(),
        roster: SEATS.iter().map(ToString::to_string).collect(),
        rule_m: 2,
        identities,
        agenda: "one".to_string(),
        features,
        republic_id: b.republic_id.clone(),
        founded_ts: 0,
    });
    st.adopt_chain(chain);
    st.identity_sk = Some(b.key(member).clone());
    st.vault_seed = Some(zeroize::Zeroizing::new(seed_of(member)));
    st
}

fn deposit(b: &Builder, depositor: &str, name: &str, text: &str) -> (VaultDeposit, Vec<u8>) {
    let ctx = ctx_of(b);
    let text = SecretText(text.to_string());
    let input = molt_vault::DepositInput { republic_id: &b.republic_id, depositor, name, kind: "text", text: &text, ctx: &ctx };
    let mut rng = os_rng().expect("rng");
    molt_vault::build_deposit(&input, &seed_of(depositor), b.key(depositor), &mut rng).expect("built")
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

/// Hold the payload, and keep its bytes for a read.
fn hold(st: &mut crate::State, b: &Builder, dep: &VaultDeposit, file: &[u8]) {
    let named = crate::net::vault_payload::NamedPayload {
        secret_id: secret_id(&b.republic_id, dep),
        hash: dep.payload.hash.clone(),
        size: dep.payload.size,
    };
    st.vault_sync_held();
    assert!(st.vault_store_payload(&named, file), "held");
    st.grants_rt().payloads.insert(named.secret_id, file.to_vec());
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

fn refused(r: Result<Reply, MoltError>, want: &VaultRefusal) {
    match r {
        Err(MoltError::Vault(got)) if &got == want => {}
        other => panic!("expected `{want}`, got {other:?}"),
    }
}

/// Every answer `st` published, `(grant_id, enc hex)`.
fn answers_sent(st: &crate::State) -> Vec<(String, String)> {
    st.vault_grants
        .sent
        .iter()
        .filter_map(|f| match f {
            VaultFrame::Resp(r) => Some((r.grant_id.clone(), r.enc.0.clone())),
            _ => None,
        })
        .collect()
}

fn grant_card_of<'a>(v: &'a VaultView, gid: &str) -> &'a VaultGrantView {
    v.grants.iter().find(|g| g.grant_id == gid).expect("the grant card")
}

/// A deposit `n` of `a` committed at height 1.
fn deposited() -> (Builder, VaultDeposit, Vec<u8>, String) {
    let mut b = Builder::vault();
    let (x, xf) = deposit(&b, "a", "n", "one");
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let sid = secret_id(&b.republic_id, &x);
    (b, x, xf, sid)
}

#[test]
fn grant_id_recomputes_or_the_grant_is_refused() {
    let (b, _, _, sid) = deposited();
    let mut st = seat("b", &b, b.blocks.clone());
    let forged = grant_op(&sid, "d", 13);
    assert!(!st.receive_proposed(12, Surface::Vault, forged.clone(), "c"), "dropped at ingest");
    st.proposals.insert(12, card(forged, "c"));
    refused(st.cmd_approve(ProposalId(12), None), &VaultRefusal::NotVerified);

    let mut c = seat("c", &b, b.blocks.clone());
    let id = match c.cmd_vault_grant(sid.clone(), "d".into()).expect("proposed") {
        Reply::Proposed { id, .. } => id.0,
        other => panic!("unexpected: {other:?}"),
    };
    let payload = c.proposals.get(&id).expect("the card").payload.clone();
    assert_eq!(payload, grant_op(&sid, "d", id), "grant_id binds the minted id");
    assert!(c.chain.pending_sigs.get(&id).is_some_and(|p| p.sigs.iter().any(|s| s.member == "c")));
    assert!(st.receive_proposed(id, Surface::Vault, payload.clone(), "c"));
    st.cmd_approve(ProposalId(id), None).expect("an honest grant is approved");

    // the same payload on a card whose id it does not bind is never signed
    let mut twin = seat("b", &b, b.blocks.clone());
    twin.proposals.insert(id, card(payload.clone(), "c"));
    twin.proposals.insert(id + 4, card(payload, "c"));
    refused(twin.cmd_approve(ProposalId(id + 4), None), &VaultRefusal::NotVerified);

    assert!(matches!(c.cmd_vault_grant(sid.clone(), "e".into()), Err(MoltError::BadPayload(_))));
    assert!(matches!(c.cmd_vault_grant("ab".repeat(32), "d".into()), Err(MoltError::BadPayload(_))));
}

#[test]
fn a_grant_on_a_replaced_version_is_void_and_unanswered() {
    let (mut b, _, _, sid_x) = deposited();
    let live = b.commit(applied(12, grant_op(&sid_x, "d", 12)), &["a", "b"]);
    let mut c = seat("c", &b, b.blocks[..2].to_vec());
    wire(&mut c, "a", 1, WorkspaceEvent::Committed(live));
    assert_eq!(answers_sent(&c).len(), 1, "a valid grant is answered on commit");

    let (y, _) = deposit(&b, "a", "n", "two");
    let replace = b.commit(applied(14, op(&y)), &["a", "b"]);
    let void = b.commit(applied(16, grant_op(&sid_x, "d", 16)), &["a", "b"]);
    wire(&mut c, "a", 2, WorkspaceEvent::Committed(replace));
    wire(&mut c, "a", 3, WorkspaceEvent::Committed(void));
    assert_eq!(c.chain.head.as_ref().map(|h| h.height), Some(4));
    assert_eq!(answers_sent(&c).len(), 1, "a void grant is not answered");

    let view = c.vault_view();
    for gid in [grant_id(&sid_x, "d", 12), grant_id(&sid_x, "d", 16)] {
        assert_eq!(grant_card_of(&view, &gid).state, VaultGrantState::Void);
        c.cmd_net_vault_ask(&"d".to_string(), gid).expect("ack");
    }
    assert_eq!(answers_sent(&c).len(), 1, "nobody answers a replaced version");

    let mut d = seat("d", &b, b.blocks.clone());
    refused(d.cmd_vault_read(sid_x), &VaultRefusal::NotTheReader);
}

/// `d` reads; `b` and `c` hold shares and answer. Height 1 the deposit,
/// then one grant to `d` per id in `ids`.
fn granted(ids: &[u64]) -> (Builder, VaultDeposit, Vec<u8>, String) {
    let (mut b, x, xf, sid) = deposited();
    for id in ids {
        b.commit(applied(*id, grant_op(&sid, "d", *id)), &["a", "b"]);
    }
    (b, x, xf, sid)
}

/// `member`'s answers to every grant of `b`, as a holder applying them.
fn answers_of(member: &str, b: &Builder) -> Vec<(String, String)> {
    let mut st = seat(member, b, b.blocks[..2].to_vec());
    for (i, blk) in b.blocks[2..].iter().enumerate() {
        wire(&mut st, "a", u64::try_from(i + 1).expect("small"), WorkspaceEvent::Committed(blk.clone()));
    }
    answers_sent(&st)
}

fn reader(b: &Builder, x: &VaultDeposit, xf: &[u8]) -> crate::State {
    let mut d = seat("d", b, b.blocks.clone());
    hold(&mut d, b, x, xf);
    d
}

#[test]
fn an_answer_for_another_grant_does_not_open() {
    let (b, x, xf, sid) = granted(&[12, 14]);
    let (g1, g2) = (grant_id(&sid, "d", 12), grant_id(&sid, "d", 14));
    let from_c = answers_of("c", &b);
    let enc1 = from_c.iter().find(|(g, _)| *g == g1).map(|(_, e)| e.clone()).expect("c answered g1");
    let mut d = reader(&b, &x, &xf);
    let c = "c".to_string();

    d.cmd_net_vault_resp(&c, g2.clone(), SecretHex(enc1.clone())).expect("ack");
    let v = d.vault_view();
    assert_eq!(grant_card_of(&v, &g2).bad_answers, ["c"]);
    assert_eq!(d.vault_have(&sid), 1, "only the own share");

    d.cmd_net_vault_resp(&c, g1.clone(), SecretHex(enc1)).expect("ack");
    assert_eq!(d.vault_have(&sid), 2);
    assert!(grant_card_of(&d.vault_view(), &g1).bad_answers.is_empty());
}

#[test]
fn an_answer_is_attributed_to_its_mls_sender() {
    let (b, x, xf, sid) = granted(&[12]);
    let g = grant_id(&sid, "d", 12);
    let (_, enc_b) = answers_of("b", &b).pop().expect("b answered");
    let mut d = reader(&b, &x, &xf);

    // c relays b's answer: sealed for seat b, it opens nothing as c
    d.cmd_net_vault_resp(&"c".to_string(), g.clone(), SecretHex(enc_b.clone())).expect("ack");
    assert_eq!(d.vault_have(&sid), 1);
    assert_eq!(grant_card_of(&d.vault_view(), &g).bad_answers, ["c"]);

    d.cmd_net_vault_resp(&"b".to_string(), g.clone(), SecretHex(enc_b)).expect("ack");
    assert_eq!(d.vault_have(&sid), 2, "b's own answer counts, once");
    // a non-holder's frame is not even judged
    d.cmd_net_vault_resp(&"a".to_string(), g.clone(), SecretHex("00".repeat(80))).expect("ack");
    assert_eq!(grant_card_of(&d.vault_view(), &g).bad_answers, ["c"]);
}

#[test]
fn answers_seal_to_the_founding_vault_pk_of_the_reader() {
    let (b, _, _, sid) = granted(&[12]);
    let g = grant_id(&sid, "d", 12);
    let mut c = seat("c", &b, b.blocks[..2].to_vec());
    // a working table naming another key for d is never used
    if let Some(r) = c.replica.as_mut() {
        if let Some(i) = r.identities.iter_mut().find(|i| i.member == "d") {
            i.vault_pk = molt_vault::vault_keypair(&seed_of("b")).1;
        }
    }
    wire(&mut c, "a", 1, WorkspaceEvent::Committed(b.blocks[2].clone()));
    let (_, enc) = answers_sent(&c).pop().expect("c answered");
    let enc = hex::decode(enc).expect("hex");
    let aad = resp_aad(&b.republic_id, &g, "c");
    let (d_sk, _) = molt_vault::vault_keypair(&seed_of("d"));
    molt_vault::open_resp(&d_sk, &enc, &aad).expect("opens with d's founding key");
    for other in ["a", "b"] {
        let (sk, _) = molt_vault::vault_keypair(&seed_of(other));
        for seat in SEATS {
            assert!(
                molt_vault::open_resp(&sk, &enc, &resp_aad(&b.republic_id, &g, seat)).is_err(),
                "{other} opened an answer for d"
            );
        }
    }
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

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("lock").clone()).expect("utf8")
    }
}

fn captured<T>(f: impl FnOnce() -> T) -> (T, String) {
    let cap = Captured::default();
    let sink = cap.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    (out, cap.text())
}

#[test]
fn a_bad_answer_names_its_seat_and_the_read_still_succeeds_with_m_good() {
    let (b, x, xf, sid) = granted(&[12]);
    let g = grant_id(&sid, "d", 12);
    let mut d = reader(&b, &x, &xf);
    let mut events = d.subscribe_events();

    // c seals a share off the polynomial, properly, to d
    let d_pk = &ctx_of(&b).holders_in_genesis_order[3].2;
    let mut rng = os_rng().expect("rng");
    let garbage = molt_vault::seal_resp(d_pk, &Share::from_bytes([9; 32]), &resp_aad(&b.republic_id, &g, "c"), &mut rng)
        .expect("sealed");
    d.cmd_net_vault_resp(&"c".to_string(), g.clone(), SecretHex(hex::encode(garbage))).expect("ack");
    match d.cmd_vault_read(sid.clone()).expect("pending") {
        Reply::VaultPending { have: 1, need: 2, .. } => {}
        other => panic!("unexpected: {other:?}"),
    }
    assert!(d.vault_grants.sent.iter().any(|f| matches!(f, VaultFrame::Ask(a) if a.grant_id == g)), "it asked");
    assert!(!std::iter::from_fn(|| events.try_recv().ok()).any(|e| matches!(e, molt_core::Event::VaultReadable { .. })));

    let (_, enc_b) = answers_of("b", &b).pop().expect("b answered");
    d.cmd_net_vault_resp(&"b".to_string(), g.clone(), SecretHex(enc_b)).expect("ack");
    assert!(std::iter::from_fn(|| events.try_recv().ok())
        .any(|e| matches!(e, molt_core::Event::VaultReadable { secret_id } if secret_id == sid)));
    let card = grant_card_of(&d.vault_view(), &g).clone();
    assert_eq!((card.answers, card.need, card.state), (2, 2, VaultGrantState::Committed));
    assert_eq!(card.bad_answers, ["c"]);
    match d.cmd_vault_read(sid.clone()).expect("read") {
        Reply::VaultText { text, name, .. } => assert_eq!((text.0.as_str(), name.as_str()), ("one", "n")),
        other => panic!("unexpected: {other:?}"),
    }

    let mut b_seat = seat("b", &b, b.blocks.clone());
    refused(b_seat.cmd_vault_read(sid), &VaultRefusal::NotTheReader);
}

/// What `st` would write: the snapshot its log replays to, the grant part
/// of `transport.state`, the cards, the chain and the view.
fn persisted(st: &crate::State) -> Vec<(&'static str, String)> {
    let mut ts = molt_core::TransportState::default();
    merge_displaced(&mut ts, &st.vault_grants.displaced);
    vec![
        ("snapshot", serde_json::to_string(&st.snapshot_now()).expect("json")),
        ("transport", serde_json::to_string(&ts).expect("json")),
        ("cards", serde_json::to_string(&st.proposals).expect("json")),
        ("chain", serde_json::to_string(&st.chain.blocks).expect("json")),
        ("view", serde_json::to_string(&st.vault_view()).expect("json")),
    ]
}

#[test]
fn the_plaintext_is_never_persisted() {
    let mut b = Builder::vault();
    let text = "quartz-lantern-73";
    let (x, xf) = deposit(&b, "a", "n", text);
    b.commit(applied(10, op(&x)), &["a", "b"]);
    let sid = secret_id(&b.republic_id, &x);
    b.commit(applied(12, grant_op(&sid, "d", 12)), &["a", "b"]);
    let (_, enc_c) = answers_of("c", &b).pop().expect("c answered");
    let mut d = reader(&b, &x, &xf);
    let (reply, logs) = captured(|| {
        d.cmd_net_vault_resp(&"c".to_string(), grant_id(&sid, "d", 12), SecretHex(enc_c)).expect("ack");
        let reply = d.cmd_vault_read(sid.clone()).expect("read");
        tracing::debug!(?reply, "dispatch");
        reply
    });
    assert!(matches!(&reply, Reply::VaultText { text: t, .. } if t.0 == text));
    assert!(logs.contains("dispatch"), "the capture works");
    assert!(!logs.contains(text), "a log line carries the text");
    assert!(!format!("{reply:?}").contains(text));
    for (what, s) in persisted(&d) {
        assert!(!s.contains(text), "the text is in the {what}");
    }
}

#[test]
fn opened_shares_are_never_persisted() {
    let (mut b, x, xf, sid) = deposited();
    let ctx = ctx_of(&b);
    let c_share = {
        let c = seat("c", &b, b.blocks.clone());
        hex::encode(c.vault_my_share(&x, &ctx).expect("c's share").as_bytes())
    };
    let (states, logs) = captured(|| {
        // c verifies a pending deposit of the same version
        let mut v = seat("c", &b, b.blocks[..1].to_vec());
        assert!(v.receive_proposed(10, Surface::Vault, op(&x), "a"));
        hold(&mut v, &b, &x, &xf);
        v.cmd_approve(ProposalId(10), None).expect("c verifies");

        b.commit(applied(12, grant_op(&sid, "d", 12)), &["a", "b"]);
        let mut c = seat("c", &b, b.blocks[..2].to_vec());
        wire(&mut c, "a", 1, WorkspaceEvent::Committed(b.blocks[2].clone()));
        let (_, enc) = answers_sent(&c).pop().expect("c answered");
        let mut d = reader(&b, &x, &xf);
        d.cmd_net_vault_resp(&"c".to_string(), grant_id(&sid, "d", 12), SecretHex(enc)).expect("ack");
        d.cmd_vault_read(sid.clone()).expect("read");
        vec![v, c, d]
    });
    assert!(!logs.contains(&c_share), "a log line carries the share");
    for st in &states {
        let frames = st.vault_grants.sent.iter().map(|f| String::from_utf8_lossy(&f.to_frame()).to_string()).collect::<String>();
        for (what, s) in persisted(st).into_iter().chain([("frames", frames)]) {
            assert!(!s.contains(&c_share), "the share is in the {what}");
        }
    }
}

#[test]
fn a_reorged_grant_is_not_answered_twice_and_stays_audited() {
    let (b, _, _, sid) = deposited();
    let fork_base = b.clone();
    let gid = grant_id(&sid, "d", 12);
    let mut lose = b.clone();
    let g_block = lose.commit(applied(12, grant_op(&sid, "d", 12)), &["a", "b"]);
    let mut c = seat("c", &b, b.blocks.clone());
    wire(&mut c, "a", 1, WorkspaceEvent::Committed(g_block.clone()));
    assert_eq!(answers_sent(&c).len(), 1);

    let z = contender(&fork_base, &g_block);
    wire(&mut c, "b", 1, WorkspaceEvent::Committed(z.clone()));
    assert_eq!(c.chain.blocks.last(), Some(&z), "the grant was displaced");
    let card = grant_card_of(&c.vault_view(), &gid).clone();
    assert_eq!((card.state, card.name.as_str(), card.reader.as_str()), (VaultGrantState::Displaced, "n", "d"));
    assert_eq!(c.vault_grants.displaced.len(), 1, "the audit line is kept");

    // the re-vote commits on the new branch: audited, not answered again
    let mut win = fork_base;
    win.push(z);
    let again = win.commit(applied(12, grant_op(&sid, "d", 12)), &["a", "b"]);
    wire(&mut c, "b", 2, WorkspaceEvent::Committed(again));
    assert_eq!(c.chain.head.as_ref().map(|h| h.height), Some(3));
    assert_eq!(answers_sent(&c).len(), 1, "answered once");
    let v = c.vault_view();
    assert_eq!(v.grants.iter().filter(|g| g.grant_id == gid).count(), 1);
    assert_eq!(grant_card_of(&v, &gid).state, VaultGrantState::Committed);

    // the reader may still ask
    c.cmd_net_vault_ask(&"d".to_string(), gid.clone()).expect("ack");
    assert_eq!(answers_sent(&c).len(), 2, "an ask is answered");
    c.cmd_net_vault_ask(&"d".to_string(), gid.clone()).expect("ack");
    c.cmd_net_vault_ask(&"b".to_string(), gid).expect("ack");
    assert_eq!(answers_sent(&c).len(), 2, "once per ten seconds, and only to the reader");
}

#[test]
fn a_seat_without_a_vault_seed_neither_grants_nor_answers() {
    let (b, _, _, sid) = granted(&[12]);
    let mut c = seat("c", &b, b.blocks[..2].to_vec());
    c.vault_seed = None;
    refused(c.cmd_vault_grant(sid, "d".into()), &VaultRefusal::NoVaultKey);
    wire(&mut c, "a", 1, WorkspaceEvent::Committed(b.blocks[2].clone()));
    assert!(answers_sent(&c).is_empty());
}

#[test]
fn a_base_pending_seat_queues_its_answer() {
    let (b, _, _, _) = granted(&[12]);
    let mut c = seat("c", &b, b.blocks[..2].to_vec());
    c.vault_seams.set_base_pending(true);
    wire(&mut c, "a", 1, WorkspaceEvent::Committed(b.blocks[2].clone()));
    assert!(answers_sent(&c).is_empty());
    c.vault_seams.set_base_pending(false);
    c.vault_flush_queued();
    assert_eq!(answers_sent(&c).len(), 1);

    let mut d = seat("d", &b, b.blocks.clone());
    d.vault_seams.set_base_pending(true);
    assert!(matches!(d.cmd_vault_read(String::new()), Err(MoltError::VaultBasePending { .. })));
}

fn line(gid: &str) -> VaultDisplacedGrant {
    VaultDisplacedGrant { grant_id: gid.to_string(), name: "n".to_string(), reader: "d".to_string() }
}

#[test]
fn displaced_persists_merge_in_any_order_and_survive_a_reopen() {
    let (b, _, _, sid) = deposited();
    let (g1, g2) = (grant_id(&sid, "d", 12), grant_id(&sid, "d", 14));
    // one reorg, two persist tasks: the longer snapshot lands first
    let mut ts = molt_core::TransportState::default();
    assert!(merge_displaced(&mut ts, &[line(&g1), line(&g2)]));
    merge_displaced(&mut ts, &[line(&g1)]);
    assert!(!merge_displaced(&mut ts, &[line(&g2)]), "nothing new");
    let on_disk: molt_core::TransportState =
        serde_json::from_str(&serde_json::to_string(&ts).expect("json")).expect("json");

    let mut c = seat("c", &b, b.blocks.clone());
    c.vault_load_displaced(on_disk.vault_displaced);
    let v = c.vault_view();
    for g in [&g1, &g2] {
        assert_eq!(grant_card_of(&v, g).state, VaultGrantState::Displaced);
    }
}

#[test]
fn a_pending_read_asks_once_per_ten_seconds() {
    let (b, x, xf, sid) = granted(&[12]);
    let mut d = reader(&b, &x, &xf);
    for _ in 0..2 {
        assert!(matches!(d.cmd_vault_read(sid.clone()), Ok(Reply::VaultPending { have: 1, need: 2, .. })));
    }
    let asks = d.vault_grants.sent.iter().filter(|f| matches!(f, VaultFrame::Ask(_))).count();
    assert_eq!(asks, 1);
}

#[test]
fn a_grant_recommitted_after_a_reopen_is_not_answered_again() {
    let (b, _, _, sid) = deposited();
    let change = applied(12, grant_op(&sid, "d", 12));
    let link = |blk: &ChainBlock| molt_storage::content_hash(&molt_core::chain::block_link_bytes(&b.republic_id, blk));
    let mut twins: Vec<ChainBlock> = [["a", "b"], ["a", "c"], ["a", "d"], ["b", "c"], ["b", "d"], ["c", "d"]]
        .iter()
        .map(|s| b.seal(2, change.clone(), s))
        .collect();
    twins.sort_by_key(|blk| link(blk));
    let (win, lose) = (twins[0].clone(), twins[5].clone());
    // c answered `lose` before it closed; it reopens on that chain
    let mut chain = b.blocks.clone();
    chain.push(lose);
    let mut c = seat("c", &b, chain);
    c.vault_load_displaced(Vec::new());

    // the same grant at the same height wins the tie-break: kept, re-applied
    wire(&mut c, "b", 1, WorkspaceEvent::Committed(win.clone()));
    assert_eq!(c.chain.blocks.last(), Some(&win), "the twin was adopted");
    assert!(answers_sent(&c).is_empty(), "answered again after a reopen");
}

#[test]
fn an_answer_that_was_not_queued_is_not_counted() {
    let (b, _, _, sid) = granted(&[12]);
    let gid = grant_id(&sid, "d", 12);
    let mut c = seat("c", &b, b.blocks[..2].to_vec());
    c.grants_rt().publish_fails = true;
    wire(&mut c, "a", 1, WorkspaceEvent::Committed(b.blocks[2].clone()));
    assert!(!c.vault_grants.answered.contains(&gid), "a dropped answer counts as answered");

    c.cmd_net_vault_ask(&"d".to_string(), gid.clone()).expect("ack");
    c.grants_rt().publish_fails = false;
    c.cmd_net_vault_ask(&"d".to_string(), gid).expect("ack");
    assert_eq!(answers_sent(&c).len(), 3, "a dropped answer throttles the next ask");
}

#[test]
fn a_grant_queued_then_displaced_is_answered_once_when_it_recommits() {
    let (b, _, _, sid) = deposited();
    let fork_base = b.clone();
    let mut lose = b.clone();
    let g_block = lose.commit(applied(12, grant_op(&sid, "d", 12)), &["a", "b"]);
    let mut c = seat("c", &b, b.blocks.clone());
    c.vault_seams.set_base_pending(true);
    wire(&mut c, "a", 1, WorkspaceEvent::Committed(g_block.clone()));
    assert!(answers_sent(&c).is_empty(), "queued");

    let z = contender(&fork_base, &g_block);
    wire(&mut c, "b", 1, WorkspaceEvent::Committed(z.clone()));
    assert_eq!(c.vault_grants.displaced.len(), 1, "the audit line is kept");

    c.vault_seams.set_base_pending(false);
    c.vault_flush_queued();
    let mut win = fork_base;
    win.push(z);
    let again = win.commit(applied(12, grant_op(&sid, "d", 12)), &["a", "b"]);
    wire(&mut c, "b", 2, WorkspaceEvent::Committed(again));
    assert_eq!(answers_sent(&c).len(), 1, "answered exactly once");
}

/// A block at `g_block`'s height on `fork_base` that wins the tip tie-break.
fn contender(fork_base: &Builder, g_block: &ChainBlock) -> ChainBlock {
    let link = |blk: &ChainBlock| molt_storage::content_hash(&molt_core::chain::block_link_bytes(&fork_base.republic_id, blk));
    (100..)
        .map(|n| {
            fork_base.seal(
                2,
                ChainChange::Applied { proposal_id: n, surface: Surface::Memory, payload: serde_json::json!({ "op": "add_note", "id": n }) },
                &["a", "b"],
            )
        })
        .find(|z| link(z) < link(g_block))
        .expect("a smaller contender")
}

#[tokio::test]
async fn a_displaced_line_reaches_the_disk_and_survives_a_reopen() {
    let (b, _, _, sid) = deposited();
    let mut lose = b.clone();
    let g_block = lose.commit(applied(12, grant_op(&sid, "d", 12)), &["a", "b"]);
    let mut c = seat("c", &b, b.blocks.clone());
    let _tmp = crate::net::vault_payload::tests::attach_storage(&mut c, &[]);
    let dir = c.active.as_ref().expect("active").dir.clone();
    wire(&mut c, "a", 1, WorkspaceEvent::Committed(g_block.clone()));
    wire(&mut c, "b", 1, WorkspaceEvent::Committed(contender(&b, &g_block)));
    assert_eq!(c.vault_grants.displaced.len(), 1);

    let handle = c.active.as_ref().expect("active").handle.clone();
    let mut on_disk = Vec::new();
    for _ in 0..200 {
        on_disk = handle.load_transport_state().await.vault_displaced;
        if !on_disk.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(on_disk.len(), 1, "the audit line was written");
    handle.close(None);
    let (ws, _) = molt_storage::open_workspace(&dir).expect("reopen");
    assert_eq!(ws.read_transport_state().vault_displaced, c.vault_grants.displaced);
}
