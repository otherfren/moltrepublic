// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse record (plan §7.6, design §3.5) on built chains: the
//! projection proves all n from the payload alone, survives a cut, first
//! valid wins, and a seat co-signs only its own result.

use super::*;
use crate::chain::test_support::{chain_signer, genesis_seat, Builder};
use crate::wallet::WALLET_INIT;
use molt_core::wallet::{ShareStatus, WalletPhase};
use molt_core::ChainChange;
use molt_treasury::dkg::Frames;

const ABC: [&str; 3] = ["a", "b", "c"];
const INIT: u64 = 5;

fn init_payload(birthday: u64, network: &str) -> Value {
    json!({ "op": WALLET_INIT, "birthday_height": birthday, "network": network })
}

fn commit(b: &mut Builder, id: u64, payload: Value) {
    b.commit(ChainChange::Applied { proposal_id: id, surface: Surface::Wallet, payload }, &["a", "b"]);
}

/// A real 2-of-3 run of `b`'s republic: every seat's keys record (its
/// own attestation inside) and the `wallet_created` with all three.
fn purse_run(b: &Builder, tag: u8, birthday: u64, network: Network) -> (Created, Vec<KeysRecord>) {
    let mut rng = rand_chacha::ChaCha20Rng::from_seed([tag; 32]);
    let republic_id = hex32(&b.republic_id).expect("raw id");
    let run = RunId { republic_id, init_id: INIT, run: [tag; 32] };
    let ctx = dkg::context(&run, 2, 3);
    let params: Vec<_> = (1..=3).map(|i| dkg::params(2, 3, i).expect("params")).collect();
    let mut machines = Vec::new();
    let mut round1 = Frames::new();
    for p in &params {
        let (m, msg) = dkg::round1(*p, ctx, &mut rng);
        machines.push(m);
        round1.insert(u16::from(p.i()), msg.to_vec());
    }
    let others = |me: u16| -> Frames { round1.iter().filter(|(l, _)| **l != me).map(|(l, b)| (*l, b.clone())).collect() };
    let mut kms = Vec::new();
    let mut shares: BTreeMap<u16, Frames> = BTreeMap::new();
    for (m, p) in machines.into_iter().zip(&params) {
        let me = u16::from(p.i());
        let (km, out) = dkg::round2(m, *p, &others(me), &mut rng).expect("round 2");
        for (to, bytes) in out {
            shares.entry(to).or_default().insert(me, bytes);
        }
        kms.push(km);
    }
    let transcript = dkg::transcript(&run.run, &round1, 3).expect("transcript");
    let view = keys::view_key(&run, &round1, 3).expect("view");
    let mut records = Vec::new();
    for (km, p) in kms.into_iter().zip(&params) {
        let i = u16::from(p.i());
        let k = dkg::complete(km, *p, &shares[&i], &mut rng).expect("complete");
        let address = keys::standard_address(k.group_key(), view.clone(), network).expect("address").to_string();
        let a = Attested { run, transcript, address: &address, network, m: 2, n: 3, birthday };
        let attestation = attest::sign(b.key(ABC[usize::from(i) - 1]), &a);
        records.push(KeysRecord {
            run,
            transcript,
            address: address.clone(),
            network,
            m: 2,
            n: 3,
            birthday,
            attestation,
            share: k.serialize(),
            view: keys::scalar_bytes(&view),
        });
    }
    let sigs = records.iter().map(|r| r.attestation).collect();
    (record_created(&records[0], sigs), records)
}

fn with_init() -> Builder {
    let mut b = Builder::new(&ABC, 2);
    commit(&mut b, INIT, init_payload(3000, "mainnet"));
    b
}

/// Plan §10.32: fewer than n attestations, or one that does not verify
/// under its founding seat, is no purse.
#[test]
fn wallet_created_without_n_attestations_is_ignored() {
    let mut b = with_init();
    let (c, _) = purse_run(&b, 1, 3000, Network::Mainnet);
    let mut short = c.clone();
    short.sigs.pop();
    let mut swapped = c.clone();
    swapped.sigs.swap(0, 1);
    commit(&mut b, 6, created_value(&short));
    commit(&mut b, 7, created_value(&swapped));
    let st = genesis_seat("a", &b, b.blocks.clone());
    assert_eq!(st.wallet_purse(), None);
    assert_eq!(st.wallet_view().phase, WalletPhase::Init);
    commit(&mut b, 8, created_value(&c));
    let st = genesis_seat("a", &b, b.blocks.clone());
    assert_eq!(st.wallet_purse().map(|p| p.id), Some(Some(8)), "the first VALID one");
    assert_eq!(st.wallet_view().address, c.address);
}

/// Plan §10.33 (I6): m seats sign a purse of their own; the third seat's
/// attestation cannot be made, so the projection ignores it and no
/// honest seat co-signs it.
#[test]
fn m_seats_cannot_seal_a_purse_of_their_own() {
    let mut b = with_init();
    let (mut c, _) = purse_run(&b, 2, 3000, Network::Mainnet);
    let a = attested_of(&c, hex32(&b.republic_id).expect("raw"));
    c.sigs[2] = attest::sign(b.key("a"), &a);
    let forged = created_value(&c);
    let mut st = chain_signer("c", &b, b.blocks.clone());
    assert_eq!(st.wallet_created_check(&forged), Err(WalletRefusal::NotMine), "c holds no such record");
    commit(&mut b, 6, forged);
    st = genesis_seat("c", &b, b.blocks.clone());
    assert_eq!(st.wallet_purse(), None, "two seats cannot speak for the third");
}

/// Plan §10.34: the purse is read from the payload and the anchor only,
/// so a cut between init and purse, or after it, changes nothing.
#[test]
fn an_anchor_rejoiner_computes_the_same_purse() {
    /// A seat that joined from the cut at `upto`, holding the suffix after it.
    fn rejoined(b: &mut Builder, upto: u64, then: Option<Value>) -> crate::State {
        let blob = crate::chain::checkpoint_state(&b.blocks, upto).expect("state");
        let hash = crate::chain::checkpoint_state_hash(&blob);
        let anchor = b.seal(upto + 1, ChainChange::Checkpoint { upto, state_hash: hash }, &ABC);
        b.push(anchor);
        if let Some(p) = then {
            commit(b, 6, p);
        }
        let mut st = genesis_seat("b", b, Vec::new());
        st.set_checkpoint_blob(Some(blob));
        st.adopt_chain(b.blocks[usize::try_from(upto + 1).expect("index")..].to_vec());
        assert!(matches!(st.chain.blocks.first().map(|x| &x.change), Some(ChainChange::Checkpoint { .. })));
        st
    }
    let mut b = with_init();
    let (c, _) = purse_run(&b, 3, 3000, Network::Mainnet);
    let mut after = b.clone();
    let st = rejoined(&mut b, 1, Some(created_value(&c)));
    assert_eq!(st.wallet_purse().map(|p| p.created), Some(c.clone()), "cut between init and purse");

    commit(&mut after, 6, created_value(&c));
    let full = genesis_seat("c", &after, after.blocks.clone());
    let anchored = rejoined(&mut after, 2, None);
    assert_eq!(anchored.wallet_purse(), full.wallet_purse(), "cut after the purse");
    assert_eq!(anchored.wallet_view().address, c.address);
}

/// Plan §10.35: of two valid purse records the first applied wins; at its
/// commit an open sibling card dies.
#[test]
fn racing_wallet_created_cards_leave_one_purse() {
    let mut b = with_init();
    let (first, _) = purse_run(&b, 4, 3000, Network::Mainnet);
    let (second, _) = purse_run(&b, 5, 3000, Network::Mainnet);
    assert_ne!(first.address, second.address);
    let mut both = b.clone();
    commit(&mut both, 6, created_value(&first));
    commit(&mut both, 7, created_value(&second));
    let st = genesis_seat("a", &both, both.blocks.clone());
    assert_eq!(st.wallet_view().address, first.address, "first wins, later ignored");

    let mut st = genesis_seat("a", &b, b.blocks.clone());
    crate::chain::test_support::wire(
        &mut st,
        "b",
        1,
        molt_core::WorkspaceEvent::Proposed { id: molt_core::ProposalId(7), surface: Surface::Wallet, payload: created_value(&second) },
    );
    assert_eq!(st.proposals.get(&7).map(|p| p.state), Some(ProposalState::Proposed));
    commit(&mut b, 6, created_value(&first));
    let block = b.blocks.last().cloned().expect("block");
    st.adopt_chain(b.blocks.clone());
    if let ChainChange::Applied { payload, .. } = &block.change {
        st.after_wallet_applied(6, payload);
    }
    let sibling = st.proposals.get(&7).expect("card");
    assert_eq!(sibling.state, ProposalState::Rejected, "the sibling died at the commit");
    assert!(sibling.superseded);
}

/// Plan §10.38d: a purse record with another birthday or network than the
/// applied init is ignored.
#[test]
fn a_purse_with_a_foreign_birthday_or_network_is_ignored() {
    let mut b = with_init();
    let (birthday, _) = purse_run(&b, 6, 2999, Network::Mainnet);
    let (network, _) = purse_run(&b, 7, 3000, Network::Stagenet);
    commit(&mut b, 6, created_value(&birthday));
    commit(&mut b, 7, created_value(&network));
    let st = genesis_seat("a", &b, b.blocks.clone());
    assert_eq!(st.wallet_purse(), None);
}

/// Plan §10.38c: a keys record of another run than the purse (an old
/// backup) leaves this seat view only, loudly; its own record holds the part.
#[test]
fn a_restored_record_of_another_run_is_view_only() {
    let mut b = with_init();
    let (purse, mine) = purse_run(&b, 8, 3000, Network::Mainnet);
    let (_, other) = purse_run(&b, 9, 3000, Network::Mainnet);
    commit(&mut b, 6, created_value(&purse));
    let mut st = genesis_seat("b", &b, b.blocks.clone());
    st.wallet_on_open(Ok(vec![other[1].encode()]));
    assert!(st.purse.run.watch_only);
    assert!(st.session.notice.contains("view only"));
    let v = st.wallet_view();
    assert!(v.shareholders.contains(&("b".to_string(), ShareStatus::WatchOnly)));

    let mut st = genesis_seat("b", &b, b.blocks.clone());
    st.wallet_on_open(Ok(vec![mine[1].encode()]));
    assert!(!st.purse.run.watch_only);
    assert!(st.wallet_view().shareholders.contains(&("b".to_string(), ShareStatus::Held)));
}

/// I2: a seat co-signs exactly its own result - the same run, every field,
/// n valid attestations.
#[test]
fn a_seat_cosigns_only_its_own_result() {
    let b = with_init();
    let (c, records) = purse_run(&b, 10, 3000, Network::Mainnet);
    let mut st = chain_signer("c", &b, b.blocks.clone());
    st.purse.run.records.push(records[2].clone());
    assert_eq!(st.wallet_created_check(&created_value(&c)), Ok(()));
    let mut other = c.clone();
    other.address.push('x');
    assert_eq!(st.wallet_created_check(&created_value(&other)), Err(WalletRefusal::NotMine));
    let mut extra = created_value(&c);
    extra["note"] = json!("x");
    assert_eq!(parse_created(&extra), None, "one encoding only");
}

const T0: u64 = 1_800_000_000;

/// A seat of `b` ready to run: a daemon with a height, consent if asked.
fn run_seat(b: &Builder, name: &str, consent: bool) -> crate::State {
    let mut st = genesis_seat(name, b, b.blocks.clone());
    st.session.settings.wallet_daemon_url = format!("http://{name}.onion");
    st.session.settings.wallet_network = "mainnet".to_string();
    st.purse.height = Some((4000, T0));
    if consent {
        st.purse.run.consented.insert(INIT);
    }
    st.presence.clock_override = Some(T0);
    st
}

/// Positions 1..=3 as `a`, `b`, `c`; `c` consents only if asked.
fn three(consent_c: bool) -> (Builder, Vec<crate::State>) {
    let b = with_init();
    let seats = vec![run_seat(&b, "a", true), run_seat(&b, "b", true), run_seat(&b, "c", consent_c)];
    for (i, st) in seats.iter().enumerate() {
        assert_eq!(st.wallet_pos(ABC[i]), u16::try_from(i + 1).ok(), "founding order");
    }
    (b, seats)
}

/// The frames `st` sent since the last call.
fn sent(st: &crate::State) -> Vec<WalletFrame> {
    std::mem::take(&mut *st.purse.run.sent.lock().expect("sent"))
}

fn deliver(to: &mut crate::State, from: &str, frames: &[WalletFrame]) {
    for f in frames {
        to.cmd_net_wallet_frame(&from.to_string(), &f.to_frame()).expect("ack");
    }
}

/// Every seat's sent frames to every other seat, until nobody sends;
/// every frame delivered.
fn pump(seats: &mut [crate::State]) -> Vec<WalletFrame> {
    let mut all = Vec::new();
    loop {
        let mut quiet = true;
        for i in 0..seats.len() {
            let out = sent(&seats[i]);
            quiet &= out.is_empty();
            let from = seats[i].member();
            for j in (0..seats.len()).filter(|j| *j != i) {
                deliver(&mut seats[j], &from, &out);
            }
            all.extend(out);
        }
        if quiet {
            return all;
        }
    }
}

/// `a` starts `nonce` and every seat readies: each is in round 1, its
/// round-1 frame is what it sent last.
fn all_in_round1(s: &mut [crate::State], nonce: [u8; 32]) -> Vec<WalletFrame> {
    s[0].wallet_start(Some(nonce)).expect("start");
    let a = sent(&s[0]);
    deliver(&mut s[1], "a", &a);
    deliver(&mut s[2], "a", &a);
    let (b, c) = (sent(&s[1]), sent(&s[2]));
    deliver(&mut s[0], "b", &b);
    deliver(&mut s[2], "b", &b);
    deliver(&mut s[0], "c", &c);
    deliver(&mut s[1], "c", &c);
    s.iter()
        .map(|st| {
            assert_eq!(stage_of(st), Some(RunStage::Round1));
            sent(st).into_iter().find(|f| matches!(f, WalletFrame::Round1(_))).expect("a round-1 frame")
        })
        .collect()
}

fn stage_of(st: &crate::State) -> Option<RunStage> {
    st.purse.run.run.as_ref().map(|r| r.stage)
}

fn reason_of(st: &crate::State) -> Option<String> {
    st.purse.run.run.as_ref().and_then(|r| r.reason.clone())
}

fn set_clock(s: &mut [crate::State], now: u64) {
    for st in s {
        st.presence.clock_override = Some(now);
    }
}

/// Design §3.3: a seat that lost a run in the rounds never rejoins it;
/// the starter's resends carry no start once it left readiness.
#[test]
fn a_reopened_seat_never_rejoins_a_run_in_the_rounds() {
    let (b, mut s) = three(true);
    all_in_round1(&mut s, [7; 32]);
    let resend = s[0].purse.run.run.as_ref().expect("run").out.clone();
    let mut reopened = run_seat(&b, "b", true);
    deliver(&mut reopened, "a", &resend);
    assert!(reopened.purse.run.run.is_none(), "not rejoined");
    let out = sent(&reopened);
    assert!(out.iter().any(|f| matches!(f, WalletFrame::Abort(x) if x.reason == "restart")), "{out:?}");
}

/// Plan §7.4: readiness and each round have their own deadline.
#[test]
fn each_round_has_its_own_deadline() {
    let (_, mut s) = three(true);
    let r1 = all_in_round1(&mut s, [7; 32]);
    set_clock(&mut s, T0 + 170);
    deliver(&mut s[0], "b", &r1[1..2]);
    deliver(&mut s[0], "c", &r1[2..3]);
    assert_eq!(stage_of(&s[0]), Some(RunStage::Round2));
    s[0].wallet_deadlines(T0 + 185);
    assert_eq!(stage_of(&s[0]), Some(RunStage::Round2), "round 2 has a full window");
    s[0].wallet_deadlines(T0 + 170 + RUN_DEADLINE_SECS);
    assert_eq!(reason_of(&s[0]).as_deref(), Some("timeout"));
}

/// Design §11: a decline whose frame was lost still reaches the others
/// as a decline, through the answer to their resends.
#[test]
fn a_lost_decline_is_answered_with_the_decline() {
    let (_, mut s) = three(false);
    s[0].wallet_start(Some([7; 32])).expect("start");
    let a = sent(&s[0]);
    deliver(&mut s[1], "a", &a);
    deliver(&mut s[2], "a", &a);
    let b = sent(&s[1]);
    deliver(&mut s[0], "b", &b);
    s[2].cmd_wallet_consent(false).expect("decline");
    assert_eq!(reason_of(&s[2]).as_deref(), Some("declined"));
    let _lost = sent(&s[2]);
    set_clock(&mut s, T0 + RESEND_SECS);
    let resend = s[0].purse.run.run.as_ref().expect("run").out.clone();
    deliver(&mut s[2], "a", &resend);
    let answer = sent(&s[2]);
    deliver(&mut s[0], "c", &answer);
    assert_eq!(reason_of(&s[0]).as_deref(), Some("declined"), "{answer:?}");
    assert_eq!(s[0].purse.run.run.as_ref().map(|r| r.missing.clone()), Some(vec![3]));
}

/// Design §3.5: an open card this seat cannot co-sign does not hold back
/// its own valid one.
#[test]
fn a_bogus_open_card_does_not_hold_back_the_valid_one() {
    let b = with_init();
    let (c, records) = purse_run(&b, 11, 3000, Network::Mainnet);
    let mut bogus = c.clone();
    bogus.transcript = [0xee; 32];
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    crate::chain::test_support::wire(
        &mut st,
        "b",
        1,
        molt_core::WorkspaceEvent::Proposed { id: molt_core::ProposalId(7), surface: Surface::Wallet, payload: created_value(&bogus) },
    );
    assert_eq!(st.proposals.get(&7).map(|p| p.state), Some(ProposalState::Proposed));
    st.purse.run.records.push(records[0].clone());
    for (pos, sig) in (1u16..).zip(&c.sigs) {
        st.wallet_on_attest(pos, c.run, *sig);
    }
    let valid = created_value(&c);
    assert!(
        st.proposals.values().any(|p| p.state == ProposalState::Proposed && p.payload == valid),
        "the valid card is proposed"
    );
}

/// Plan §7.5: a seat that re-anchors onto the purse announces it.
#[test]
fn a_re_anchor_onto_the_purse_announces_it() {
    let mut b = with_init();
    let (c, records) = purse_run(&b, 14, 3000, Network::Mainnet);
    let mut lag = genesis_seat("c", &b, b.blocks.clone());
    lag.purse.run.records.push(records[2].clone());
    commit(&mut b, 6, created_value(&c));
    let blob = crate::chain::checkpoint_state(&b.blocks, 2).expect("state");
    let hash = crate::chain::checkpoint_state_hash(&blob);
    let anchor = b.seal(3, ChainChange::Checkpoint { upto: 2, state_hash: hash }, &ABC);
    let mut rx = lag.ev_tx.subscribe();
    lag.chain.pending_served_blob = Some(blob);
    lag.chain.pending_blocks.insert(3, anchor);
    lag.try_adopt_from_blob();
    assert_eq!(lag.wallet_purse().map(|p| p.created.address), Some(c.address.clone()), "re-anchored");
    let mut created = false;
    while let Ok(ev) = rx.try_recv() {
        if let Event::WalletCreated { address } = ev {
            created = address == c.address;
        }
    }
    assert!(created, "WalletCreated after the re-anchor");
}

/// The purse RNG fails closed: no nonce, no round-1 secret.
#[test]
fn an_unavailable_rng_ends_the_run() {
    let (_, mut s) = three(true);
    RNG_DOWN.with(|d| d.set(true));
    assert_eq!(s[0].wallet_start(None), Err(WalletRefusal::Rng));
    s[0].wallet_start(Some([7; 32])).expect("start");
    let a = sent(&s[0]);
    deliver(&mut s[1], "a", &a);
    deliver(&mut s[2], "a", &a);
    let (b, c) = (sent(&s[1]), sent(&s[2]));
    deliver(&mut s[0], "b", &b);
    deliver(&mut s[0], "c", &c);
    RNG_DOWN.with(|d| d.set(false));
    assert_eq!(stage_of(&s[0]), Some(RunStage::Aborted));
    assert!(!sent(&s[0]).iter().any(|f| matches!(f, WalletFrame::Round1(_))), "no round 1 from a zero seed");
}

/// Design §6: at the commit a view-only seat drops the other runs' records.
#[test]
fn a_view_only_seat_drops_the_other_runs_records_at_the_commit() {
    let mut b = with_init();
    let (purse, _) = purse_run(&b, 12, 3000, Network::Mainnet);
    let (_, other) = purse_run(&b, 13, 3000, Network::Mainnet);
    commit(&mut b, 6, created_value(&purse));
    let (mut stored, _tmp, dir) = crate::tests::support::stored_chain_signer(&b, "b", &ABC);
    let mut st = genesis_seat("b", &b, b.blocks.clone());
    st.active = stored.active.take();
    let active = st.active.as_ref().expect("active");
    assert!(active.handle.persist_wallet_keys_blocking(other[1].encode()));
    st.wallet_on_open(Ok(vec![other[1].encode()]));
    assert!(st.purse.run.watch_only);
    assert!(st.purse.run.records.is_empty());
    st.active.take().expect("active").handle.close(None);
    let (ws, _) = molt_storage::open_workspace(&dir).expect("reopen");
    assert!(ws.read_wallet_keys().expect("keys").is_empty(), "the dead run's share is gone");
}

/// Plan §10.38a on the receiving side: a nonce once seen is never joined again.
#[test]
fn a_seen_nonce_is_never_joined_again() {
    let (b, mut s) = three(true);
    s[0].wallet_start(Some([9; 32])).expect("start");
    let out = sent(&s[0]);
    deliver(&mut s[1], "a", &out);
    s[1].cmd_wallet_consent(false).expect("decline");
    s[2].wallet_start(Some([8; 32])).expect("start");
    let out = sent(&s[2]);
    deliver(&mut s[1], "c", &out);
    assert_eq!(s[1].purse.run.run.as_ref().map(|r| r.nonce), Some([8; 32]));
    let mut reopened = run_seat(&b, "a", true);
    reopened.wallet_start(Some([9; 32])).expect("a fresh seat starts it");
    deliver(&mut s[1], "a", &sent(&reopened));
    assert_eq!(s[1].purse.run.run.as_ref().map(|r| r.nonce), Some([8; 32]), "the reused nonce is not joined");
}

/// Plan §11: the starter's daemon reaches the others as a suggestion.
#[test]
fn the_starters_daemon_is_suggested() {
    let (_, mut s) = three(true);
    s[0].wallet_start(Some([7; 32])).expect("start");
    let out = sent(&s[0]);
    deliver(&mut s[1], "a", &out);
    assert_eq!(s[1].wallet_run_view().map(|v| v.daemon_hint), Some("http://a.onion".to_string()));
}

/// Each seat's round-1 frame to every other seat.
fn swap_round1(s: &mut [crate::State], r1: &[WalletFrame]) {
    for (i, f) in r1.iter().enumerate() {
        for j in (0..s.len()).filter(|j| *j != i) {
            deliver(&mut s[j], ABC[i], std::slice::from_ref(f));
        }
    }
}

/// I4: a seat whose record does not reach the disk never attests.
#[test]
fn an_unpersisted_record_is_never_attested() {
    let (_, mut s) = three(true);
    let r1 = all_in_round1(&mut s, [7; 32]);
    swap_round1(&mut s, &r1);
    let frames = pump(&mut s);
    for st in &s {
        assert_eq!(reason_of(st).as_deref(), Some("storage"));
        assert!(st.purse.run.records.is_empty());
    }
    assert!(frames.iter().any(|f| matches!(f, WalletFrame::Round2(_))), "the rounds ran");
    assert!(!frames.iter().any(|f| matches!(f, WalletFrame::Attest(_))), "no attestation left");
}

/// Design §3.4: a second, differing round-1 frame from one sender aborts.
#[test]
fn a_second_round1_frame_is_equivocation() {
    let (_, mut s) = three(true);
    let r1 = all_in_round1(&mut s, [7; 32]);
    let WalletFrame::Round1(mut forged) = r1[1].clone() else {
        panic!("round 1");
    };
    forged.msg.0.replace_range(..2, if forged.msg.0.starts_with("00") { "01" } else { "00" });
    deliver(&mut s[0], "b", &r1[1..2]);
    deliver(&mut s[0], "b", &[WalletFrame::Round1(forged)]);
    assert_eq!(reason_of(&s[0]).as_deref(), Some("equivocation"));
}

/// Design §3.4: a round-2 frame without exactly one share per other seat aborts.
#[test]
fn a_round2_frame_with_a_wrong_share_set_is_invalid() {
    let (_, mut s) = three(true);
    all_in_round1(&mut s, [7; 32]);
    let f = WalletFrame::Round2(WalletRound2Frame {
        v: WALLET_V,
        init: INIT,
        run: hex::encode([7u8; 32]),
        shares: [(3, molt_core::vault::SecretHex("00".repeat(128)))].into_iter().collect(),
        transcript: hex::encode([1u8; 32]),
    });
    deliver(&mut s[0], "b", &[f]);
    assert_eq!(reason_of(&s[0]).as_deref(), Some("invalid"));
}

/// Design §3.4: a seat that sees another transcript than its own aborts.
#[test]
fn a_differing_transcript_aborts_the_run() {
    let (_, mut s) = three(true);
    let r1 = all_in_round1(&mut s, [7; 32]);
    swap_round1(&mut s, &r1);
    assert_eq!(stage_of(&s[0]), Some(RunStage::Round2));
    let WalletFrame::Round2(mut f) = sent(&s[1]).into_iter().find(|f| matches!(f, WalletFrame::Round2(_))).expect("b's round 2")
    else {
        panic!("round 2");
    };
    f.transcript = hex::encode([0xee; 32]);
    deliver(&mut s[0], "b", &[WalletFrame::Round2(f)]);
    assert_eq!(reason_of(&s[0]).as_deref(), Some("transcript"));
}
