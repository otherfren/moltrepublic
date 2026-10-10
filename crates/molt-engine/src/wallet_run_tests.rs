// SPDX-License-Identifier: GPL-3.0-or-later

//! The purse record (plan §7.6, design §3.5) on built chains: the
//! projection proves all n from the payload alone, survives a cut, first
//! valid wins, and a seat co-signs only its own result.

use super::*;
use crate::chain::test_support::{chain_signer, genesis_seat, Builder};
use crate::wallet::WALLET_INIT;
use crate::wallet_seat::{ANSWER_SECS, ASK_SECS, STATUS_RESEND_SECS};
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
pub(crate) fn purse_run(b: &Builder, tag: u8, birthday: u64, network: Network) -> (Created, Vec<KeysRecord>) {
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

pub(crate) fn with_init() -> Builder {
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

/// Design §6: a view-only seat drops the other runs' records once the
/// purse is final.
#[test]
fn a_view_only_seat_drops_the_other_runs_records_once_final() {
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
    assert_eq!(st.purse.run.records.len(), 1, "kept while a reorg may displace the purse");
    let blob = cut_at(&mut b, 2);
    adopt_cut(&mut st, &b, blob, 2);
    st.wallet_run_tick(T0);
    assert!(st.purse.run.records.is_empty());
    assert!(records_on_disk(&mut st, &dir).is_empty(), "the dead run's share is gone");
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

/// Push a checkpoint over `b`'s blocks up to `upto`; its state blob.
fn cut_at(b: &mut Builder, upto: u64) -> molt_core::CheckpointState {
    let blob = crate::chain::checkpoint_state(&b.blocks, upto).expect("state");
    let hash = crate::chain::checkpoint_state_hash(&blob);
    let anchor = b.seal(upto + 1, ChainChange::Checkpoint { upto, state_hash: hash }, &ABC);
    b.push(anchor);
    blob
}

/// Adopt `b`'s chain from the cut at `upto` (its anchor at `upto + 1`).
fn adopt_cut(st: &mut crate::State, b: &Builder, blob: molt_core::CheckpointState, upto: u64) {
    st.set_checkpoint_blob(Some(blob));
    st.adopt_chain(b.blocks[usize::try_from(upto + 1).expect("index")..].to_vec());
}

/// The keys records on `dir`'s disk, after closing `st`'s writer.
fn records_on_disk(st: &mut crate::State, dir: &std::path::Path) -> Vec<Vec<u8>> {
    st.active.take().expect("active").handle.close(None);
    let (ws, _) = molt_storage::open_workspace(dir).expect("reopen");
    ws.read_wallet_keys().expect("keys").iter().map(|r| r.to_vec()).collect()
}

/// I16 against a reorg: the other runs' records stay until the purse
/// block is final (below a cut), so a re-base onto another run's purse
/// still finds this seat's part.
#[test]
fn a_displaced_purse_keeps_every_record_until_it_is_final() {
    let base = with_init();
    let (one, r1) = purse_run(&base, 15, 3000, Network::Mainnet);
    let (two, r2) = purse_run(&base, 16, 3000, Network::Mainnet);
    let mut first = base.clone();
    commit(&mut first, 6, created_value(&one));
    let mut second = base.clone();
    commit(&mut second, 7, created_value(&two));
    let (mut stored, _tmp, dir) = crate::tests::support::stored_chain_signer(&first, "b", &ABC);
    let mut st = genesis_seat("b", &first, first.blocks.clone());
    st.active = stored.active.take();
    let active = st.active.as_ref().expect("active");
    assert!(active.handle.persist_wallet_keys_blocking(r1[1].encode()));
    assert!(active.handle.persist_wallet_keys_blocking(r2[1].encode()));
    st.wallet_on_open(Ok(vec![r1[1].encode(), r2[1].encode()]));
    assert_eq!(st.wallet_purse().map(|p| p.created.run), Some(one.run));
    assert_eq!(st.purse.run.records.len(), 2, "not final: the other record stays");

    st.adopt_chain(base.blocks.clone());
    st.wallet_run_tick(T0);
    assert_eq!(st.wallet_purse(), None, "displaced");
    st.adopt_chain(second.blocks.clone());
    st.wallet_on_commit();
    assert_eq!(st.wallet_purse().map(|p| p.created.run), Some(two.run), "re-based onto the other purse");
    assert!(!st.purse.run.watch_only, "this seat still holds its part");

    let blob = cut_at(&mut second, 2);
    adopt_cut(&mut st, &second, blob, 2);
    st.wallet_run_tick(T0 + RESEND_SECS);
    assert_eq!(records_on_disk(&mut st, &dir), [r2[1].encode().to_vec()], "final: only the purse's record");
}

/// W6: a decline withdraws this seat's consent, an approval of the init
/// included; a later run asks again.
#[test]
fn a_decline_withdraws_consent_for_later_runs() {
    let (_, mut s) = three(true);
    s[2].chain.own_approvals.insert(INIT);
    s[0].wallet_start(Some([7; 32])).expect("start");
    let a = sent(&s[0]);
    deliver(&mut s[2], "a", &a);
    s[2].cmd_wallet_consent(false).expect("decline");
    let _ = sent(&s[2]);
    s[1].wallet_start(Some([8; 32])).expect("start");
    let b = sent(&s[1]);
    deliver(&mut s[2], "b", &b);
    assert_eq!(s[2].purse.run.run.as_ref().map(|r| r.nonce), Some([8; 32]));
    assert!(!sent(&s[2]).iter().any(|f| matches!(f, WalletFrame::Ready(r) if r.ok)), "not readied unasked");
    assert_eq!(s[2].wallet_run_view().map(|v| v.needs_consent), Some(true));
}

/// Design §3.4: an abort names its reason; only absence names seats.
#[test]
fn a_remote_abort_names_no_seat() {
    let (_, mut s) = three(true);
    let r1 = all_in_round1(&mut s, [7; 32]);
    deliver(&mut s[0], "b", &r1[1..2]);
    let abort = WalletFrame::Abort(WalletAbortFrame {
        v: WALLET_V,
        init: INIT,
        run: hex::encode([7u8; 32]),
        reason: "equivocation".to_string(),
    });
    deliver(&mut s[0], "b", &[abort]);
    assert_eq!(reason_of(&s[0]).as_deref(), Some("equivocation"));
    assert_eq!(s[0].wallet_run_view().map(|v| v.missing), Some(Vec::new()));
}

/// Design §3.5: a reopen with a record re-attests and co-signs a matching
/// open card at once, not only on the resend beat.
#[test]
fn a_reopen_with_a_record_re_attests_and_cosigns_at_once() {
    let b = with_init();
    let (c, records) = purse_run(&b, 17, 3000, Network::Mainnet);
    let mut st = genesis_seat("c", &b, b.blocks.clone());
    crate::chain::test_support::wire(
        &mut st,
        "a",
        1,
        molt_core::WorkspaceEvent::Proposed { id: molt_core::ProposalId(9), surface: Surface::Wallet, payload: created_value(&c) },
    );
    let _ = sent(&st);
    st.wallet_on_open(Ok(vec![records[2].encode()]));
    let sig = hex::encode(records[2].attestation);
    assert!(sent(&st).iter().any(|f| matches!(f, WalletFrame::Attest(a) if a.sig == sig)), "re-attested");
    assert!(st.chain.own_approvals.contains(&9), "co-signed");
}

/// Design §3.3: position k starts after (k-1) steps of silence; a start
/// that arrives first is joined instead.
#[test]
fn a_silent_starter_is_replaced_by_the_next_position() {
    let (_, mut s) = three(true);
    let init = init_payload(3000, "mainnet");
    s[1].after_wallet_applied(INIT, &init);
    s[2].after_wallet_applied(INIT, &init);
    assert!(stage_of(&s[1]).is_none(), "position 2 waits");
    s[1].wallet_fallback_start(T0 + STEP_SECS - 1);
    assert!(stage_of(&s[1]).is_none());
    s[1].wallet_fallback_start(T0 + STEP_SECS);
    assert_eq!(s[1].purse.run.run.as_ref().map(|r| r.starter), Some(2), "position 2 starts");
    s[2].wallet_fallback_start(T0 + STEP_SECS);
    assert!(stage_of(&s[2]).is_none(), "position 3 waits two steps");
    let start = sent(&s[1]);
    deliver(&mut s[2], "b", &start);
    s[2].wallet_fallback_start(T0 + 2 * STEP_SECS);
    assert_eq!(s[2].purse.run.run.as_ref().map(|r| r.starter), Some(2), "joined, not a second start");
}

/// Design §3.5: position k proposes the purse only after (k-1) steps.
#[test]
fn a_later_position_proposes_only_after_its_wait() {
    let b = with_init();
    let (c, records) = purse_run(&b, 18, 3000, Network::Mainnet);
    let mut st = genesis_seat("b", &b, b.blocks.clone());
    st.presence.clock_override = Some(T0);
    st.purse.run.records.push(records[1].clone());
    for (pos, sig) in (1u16..).zip(&c.sigs) {
        st.wallet_on_attest(pos, c.run, *sig);
    }
    let valid = created_value(&c);
    let proposed = |st: &crate::State| st.proposals.values().any(|p| p.state == ProposalState::Proposed && p.payload == valid);
    assert!(!proposed(&st), "position 2 waits");
    st.presence.clock_override = Some(T0 + STEP_SECS - 1);
    st.wallet_maybe_propose(c.run);
    assert!(!proposed(&st));
    st.presence.clock_override = Some(T0 + STEP_SECS);
    st.wallet_maybe_propose(c.run);
    assert!(proposed(&st), "after one step");
}

/// A retry or a start never supersedes a live run; after the purse a
/// retry names it.
#[test]
fn a_retry_or_start_never_supersedes_a_live_run() {
    let refused = |r: Result<Reply, MoltError>, want: WalletRefusal| matches!(r, Err(MoltError::Wallet(w)) if w == want);
    let (_, mut s) = three(true);
    s[0].wallet_start(Some([7; 32])).expect("start");
    assert!(refused(s[0].cmd_wallet_retry(), WalletRefusal::RunActive), "readiness");
    let (_, mut s) = three(true);
    all_in_round1(&mut s, [7; 32]);
    assert!(refused(s[0].cmd_wallet_retry(), WalletRefusal::RunActive), "the rounds");
    assert_eq!(s[1].wallet_start(Some([8; 32])), Err(WalletRefusal::RunActive), "a start in the rounds");

    let mut b = with_init();
    let (c, _) = purse_run(&b, 19, 3000, Network::Mainnet);
    commit(&mut b, 6, created_value(&c));
    let mut st = genesis_seat("a", &b, b.blocks.clone());
    assert!(refused(st.cmd_wallet_retry(), WalletRefusal::InitExists), "the purse exists");
}

/// Plan §7.4: a seat that left a run in readiness for a racing start
/// rejoins it on that run's round frame, and answers no abort.
#[test]
fn a_run_left_in_readiness_is_rejoined_on_its_round_frame() {
    let (b, mut s) = three(true);
    s[1].wallet_start(Some([3; 32])).expect("start");
    let start = sent(&s[1]);
    deliver(&mut s[0], "b", &start);
    deliver(&mut s[2], "b", &start);
    let (ra, rc) = (sent(&s[0]), sent(&s[2]));
    deliver(&mut s[1], "a", &ra);
    deliver(&mut s[1], "c", &rc);
    assert_eq!(stage_of(&s[1]), Some(RunStage::Round1));
    let r1 = sent(&s[1]).into_iter().find(|f| matches!(f, WalletFrame::Round1(_))).expect("b's round 1");
    let mut racer = run_seat(&b, "a", true);
    racer.wallet_start(Some([5; 32])).expect("start");
    deliver(&mut s[2], "a", &sent(&racer));
    assert_eq!(s[2].purse.run.run.as_ref().map(|r| r.nonce), Some([5; 32]), "the lower position");
    let _ = sent(&s[2]);
    deliver(&mut s[2], "b", &[r1]);
    assert_eq!(s[2].purse.run.run.as_ref().map(|r| r.nonce), Some([3; 32]), "rejoined");
    assert!(!sent(&s[2]).iter().any(|f| matches!(f, WalletFrame::Abort(_))), "no abort");
}

/// Plan §7.4: a reopened seat rejoins a run still in readiness; the
/// frames that came before the start are replayed.
#[test]
fn a_reopened_seat_rejoins_a_run_in_readiness() {
    let (b, mut s) = three(true);
    s[0].wallet_start(Some([7; 32])).expect("start");
    let start = sent(&s[0]);
    deliver(&mut s[1], "a", &start);
    let rb = sent(&s[1]);
    let mut c = run_seat(&b, "c", false);
    deliver(&mut c, "b", &rb);
    assert!(stage_of(&c).is_none(), "kept for the start");
    deliver(&mut c, "a", &start);
    let ready = c.purse.run.run.as_ref().map(|r| r.ready.clone());
    assert_eq!(ready, Some([1, 2].into_iter().collect()), "both early readies replayed");
}

/// The purse committed on a seat that holds `records` (none: view only).
pub(crate) fn seat_with_purse(member: &str, records: &[KeysRecord]) -> (crate::State, Created) {
    let mut b = with_init();
    let (c, _) = purse_run(&b, 11, 3000, Network::Mainnet);
    commit(&mut b, 6, created_value(&c));
    let mut st = genesis_seat(member, &b, b.blocks.clone());
    st.wallet_on_open(Ok(records.iter().map(KeysRecord::encode).collect()));
    (st, c)
}

/// Plan §7.8 (design §5): a seat without a key part asks; a holder
/// answers; the answer counts only when it opens the address, and only
/// from a seat.
#[test]
fn a_view_answer_must_open_the_address() {
    let b = with_init();
    let (_, records) = purse_run(&b, 11, 3000, Network::Mainnet);
    let (mut c, _) = seat_with_purse("c", &[]);
    assert!(!c.wallet_view().can_watch);
    c.wallet_seat_tick(1000);
    assert!(sent(&c).iter().any(|f| matches!(f, WalletFrame::ViewAsk(a) if a.init == INIT)), "it asks");

    let (mut a, _) = seat_with_purse("a", &records[..1]);
    a.wallet_on_view_ask(&"c".to_string(), INIT);
    let answer = sent(&a)
        .into_iter()
        .find_map(|f| match f {
            WalletFrame::ViewResp(r) => Some(hex::decode(&r.view.0).expect("hex")),
            _ => None,
        })
        .expect("a holder answers");
    assert_eq!(answer, records[0].view.to_vec());

    let mut wrong = answer.clone();
    wrong[0] ^= 1;
    c.cmd_net_wallet_view_answer(&"a".to_string(), &wrong).expect("ack");
    assert!(!c.wallet_view().can_watch, "a key that does not open the address");
    c.cmd_net_wallet_view_answer(&"z".to_string(), &answer).expect("ack");
    assert!(!c.wallet_view().can_watch, "not a seat");
    c.cmd_net_wallet_view_answer(&"a".to_string(), &answer).expect("ack");
    let v = c.wallet_view();
    assert!(v.can_watch);
    assert!(v.shareholders.contains(&("c".to_string(), ShareStatus::WatchOnly)), "still no key part");
}

/// Plan §7.8: each seat's status frame fills its row; this seat speaks
/// for itself, a stranger, an unknown status and another init change nothing.
#[test]
fn status_frames_fill_the_shareholders() {
    let b = with_init();
    let (_, records) = purse_run(&b, 11, 3000, Network::Mainnet);
    let (mut a, _) = seat_with_purse("a", &records[..1]);
    let rows = |st: &crate::State| st.wallet_view().shareholders;
    assert_eq!(
        rows(&a),
        vec![
            ("a".to_string(), ShareStatus::Held),
            ("b".to_string(), ShareStatus::Unknown),
            ("c".to_string(), ShareStatus::Unknown)
        ]
    );
    a.cmd_net_wallet_status(&"b".to_string(), ShareStatus::Held, INIT).expect("ack");
    a.cmd_net_wallet_status(&"c".to_string(), ShareStatus::WatchOnly, INIT).expect("ack");
    a.cmd_net_wallet_status(&"z".to_string(), ShareStatus::Held, INIT).expect("ack");
    a.cmd_net_wallet_status(&"a".to_string(), ShareStatus::WatchOnly, INIT).expect("ack");
    a.cmd_net_wallet_status(&"b".to_string(), ShareStatus::Unknown, INIT)
        .expect("ack");
    a.cmd_net_wallet_status(&"c".to_string(), ShareStatus::Held, INIT + 1).expect("ack");
    assert_eq!(
        rows(&a),
        vec![
            ("a".to_string(), ShareStatus::Held),
            ("b".to_string(), ShareStatus::Held),
            ("c".to_string(), ShareStatus::WatchOnly)
        ]
    );
    sent(&a);
    a.wallet_seat_tick(1000);
    let out = sent(&a);
    assert!(out.iter().any(|f| matches!(f, WalletFrame::Status(s) if s.held && s.init == INIT)));
    assert!(!out.iter().any(|f| matches!(f, WalletFrame::ViewAsk(_))), "a holder never asks");
}

/// Plan §7.3 (W1, I15): the founding stage is armed only by `wallet` in
/// the charter within the bounds, and the ratification counts as consent.
#[test]
fn the_founding_stage_needs_wallet_within_bounds() {
    let b = with_init();
    let mut st = genesis_seat("b", &b, b.blocks.clone());
    st.wallet_arm_founding(Some(&["memory".to_string()]));
    assert!(!st.wallet_view().founding);
    assert!(!st.wallet_consents(INIT));
    st.wallet_arm_founding(Some(&["memory".to_string(), "wallet".to_string()]));
    assert!(st.wallet_view().founding);
    assert!(st.wallet_consents(INIT), "ratified at the founding");

    let all = Builder::new(&ABC, 3);
    let mut st = genesis_seat("b", &all, all.blocks.clone());
    st.wallet_arm_founding(Some(&["wallet".to_string()]));
    assert!(!st.wallet_view().founding, "3-of-3 has no purse");
}

/// An armed founding seat with a daemon at 4000 (no init yet).
fn armed_seat(name: &str) -> crate::State {
    let b = Builder::new(&ABC, 2);
    let mut st = run_seat(&b, name, false);
    st.wallet_arm_founding(Some(&["wallet".to_string()]));
    st
}

/// Plan §7.3: position k tries the founding's init `k-1` steps after arming.
#[test]
fn a_founding_position_proposes_only_after_its_wait() {
    let tried = |name: &str, at: u64| {
        let mut st = armed_seat(name);
        st.wallet_seat_tick(at);
        st.purse.seat.tried_at == at
    };
    assert!(tried("a", T0), "position 1 at once");
    assert!(!tried("b", T0 + STEP_SECS - 1));
    assert!(tried("b", T0 + STEP_SECS), "position 2 after one step");
    assert!(!tried("c", T0 + 2 * STEP_SECS - 1));
    assert!(tried("c", T0 + 2 * STEP_SECS), "position 3 after two");
}

fn land_card(st: &mut crate::State, seq: u64, id: u64, birthday: u64) {
    crate::chain::test_support::wire(
        st,
        "a",
        seq,
        molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(id),
            surface: Surface::Wallet,
            payload: init_payload(birthday, "mainnet"),
        },
    );
}

/// W6 (design §3.1): a seat that declined the founding's init card, by
/// its birthday check here, is asked in the run like any other.
#[test]
fn a_declined_founding_card_is_no_consent() {
    let mut c = armed_seat("c");
    land_card(&mut c, 1, INIT, 100);
    assert!(c.proposals.get(&INIT).is_some_and(|p| p.decliners.contains(&"c".to_string())), "out of the window");
    assert!(!c.wallet_consents(INIT), "declined: asked in the run");
}

/// Design §3.2/W10: the ratification consents to the founding's init
/// only; a later card is asked for.
#[test]
fn founding_consent_covers_only_the_founding_init() {
    let mut c = armed_seat("c");
    land_card(&mut c, 1, INIT, 3000);
    assert!(c.chain.own_approvals.contains(&INIT), "the founding's card is approved");
    land_card(&mut c, 2, 9, 3000);
    assert!(!c.chain.own_approvals.contains(&9), "a later card is not");
    assert!(c.wallet_consents(INIT));
    assert!(!c.wallet_consents(9));
}

/// Plan §7.8: own status every 30 s and on change, never on every beat.
#[test]
fn status_goes_out_every_30_s_and_on_change() {
    let statuses = |st: &crate::State| sent(st).into_iter().filter(|f| matches!(f, WalletFrame::Status(_))).count();
    let (mut a, _) = seat_with_purse("a", &[]);
    a.wallet_seat_tick(1000);
    assert_eq!(statuses(&a), 1);
    a.wallet_seat_tick(1000 + STATUS_RESEND_SECS - 1);
    assert_eq!(statuses(&a), 0, "not before 30 s");
    a.wallet_seat_tick(1000 + STATUS_RESEND_SECS);
    assert_eq!(statuses(&a), 1);
    a.purse.seat.status_sent = Some((ShareStatus::Held, 1000 + STATUS_RESEND_SECS));
    a.wallet_seat_tick(1000 + STATUS_RESEND_SECS + 1);
    assert_eq!(statuses(&a), 1, "on change");
}

/// Plan §7.8: a seat without the view key asks every 15 s.
#[test]
fn the_view_ask_goes_out_every_15_s() {
    let asks = |st: &crate::State| sent(st).into_iter().filter(|f| matches!(f, WalletFrame::ViewAsk(_))).count();
    let (mut c, _) = seat_with_purse("c", &[]);
    c.wallet_seat_tick(1000);
    assert_eq!(asks(&c), 1);
    c.wallet_seat_tick(1000 + ASK_SECS - 1);
    assert_eq!(asks(&c), 0, "not before 15 s");
    c.wallet_seat_tick(1000 + ASK_SECS);
    assert_eq!(asks(&c), 1);
}

/// Plan §7.8: at most one answer per asker in 10 s; another asker is
/// answered at once.
#[test]
fn a_holder_answers_each_asker_once_per_10_s() {
    let b = with_init();
    let (_, records) = purse_run(&b, 11, 3000, Network::Mainnet);
    let (mut a, _) = seat_with_purse("a", &records[..1]);
    let answers = |st: &crate::State| sent(st).into_iter().filter(|f| matches!(f, WalletFrame::ViewResp(_))).count();
    a.presence.clock_override = Some(T0);
    a.wallet_on_view_ask(&"c".to_string(), INIT);
    assert_eq!(answers(&a), 1);
    a.presence.clock_override = Some(T0 + ANSWER_SECS - 1);
    a.wallet_on_view_ask(&"c".to_string(), INIT);
    assert_eq!(answers(&a), 0, "not twice in 10 s");
    a.wallet_on_view_ask(&"b".to_string(), INIT);
    assert_eq!(answers(&a), 1, "another asker");
    a.presence.clock_override = Some(T0 + ANSWER_SECS);
    a.wallet_on_view_ask(&"c".to_string(), INIT);
    assert_eq!(answers(&a), 1);
    a.wallet_on_view_ask(&"c".to_string(), INIT + 1);
    assert_eq!(answers(&a), 0, "another init");
}

/// W5: a keys file damaged after open is set aside, and the part still
/// held in memory is written back - the seat keeps it.
#[test]
fn an_acknowledged_loss_writes_back_the_part_held_in_memory() {
    let mut b = with_init();
    let (purse, records) = purse_run(&b, 12, 3000, Network::Mainnet);
    commit(&mut b, 6, created_value(&purse));
    let (mut st, _tmp, dir) = crate::tests::support::stored_chain_signer(&b, "b", &ABC);
    let mine = records[1].encode();
    assert!(st.active.as_ref().expect("open").handle.persist_wallet_keys_blocking(mine.clone()));
    st.wallet_on_open(Ok(vec![mine.clone()]));
    let keys = dir.join(molt_storage::WALLET_KEYS_FILE);
    let mut rotten = std::fs::read(&keys).expect("keys");
    let last = rotten.len() - 1;
    rotten[last] ^= 1;
    std::fs::write(&keys, &rotten).expect("rot");

    st.cmd_wallet_acknowledge_loss().expect("acknowledged");
    assert!(dir.join(".wallet_keys.state.lost0").exists(), "the damaged file is kept aside");
    assert_eq!(st.wallet_own_status(), ShareStatus::Held);
    assert_eq!(records_on_disk(&mut st, &dir), vec![mine.to_vec()], "the part is on disk again");
}
