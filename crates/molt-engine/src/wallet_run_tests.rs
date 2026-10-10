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
