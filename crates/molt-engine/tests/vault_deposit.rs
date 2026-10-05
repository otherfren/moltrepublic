// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Vault deposits** (vault build plan S3b, spec §7, §14.3 a and b): a
//! deposit commits only once an approver holds the payload, and a forged
//! depositor is refused by every approver and every verifier.

use std::time::Duration;

use molt_core::vault::{SecretText, VaultDepositState, VaultRefusal, VaultView};
use molt_core::{ChainChange, Command, MoltError, ProposalId, Reply, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{close_and_open, found_n_at};

async fn vault_view(w: &WalletHandle) -> VaultView {
    match w
        .execute(Command::ReadState { surface: Surface::Vault, channel: None, view: None })
        .await
        .expect("read vault")
    {
        Reply::State(s) => s.vault.expect("the vault view"),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_vault(w: &WalletHandle, what: &str, pred: impl Fn(&VaultView) -> bool) -> VaultView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let v = vault_view(w).await;
        if pred(&v) {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}\n{v:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn seal(w: &WalletHandle, name: &str, text: &str) -> ProposalId {
    let cmd = Command::VaultSeal { name: name.into(), kind: "text".into(), text: SecretText(text.into()) };
    match w.execute(cmd).await.expect("sealed") {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    }
}

fn has(v: &VaultView, name: &str, state: VaultDepositState) -> bool {
    v.deposits.iter().any(|d| d.name == name && d.state == state)
}

async fn approvals(w: &WalletHandle, id: ProposalId) -> usize {
    match w.execute(Command::ReadProposal { id: id.0 }).await.expect("read proposal") {
        Reply::Proposals { proposals, .. } => proposals.first().map_or(0, |p| p.approvals),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Approve once the payload is here (the gate the keystone below pins).
async fn approve_when_held(w: &WalletHandle, id: ProposalId) {
    wait_vault(w, "the payload to arrive", |v| {
        v.deposits.iter().any(|d| d.proposal == Some(id.0) && d.held)
    })
    .await;
    w.execute(Command::Approve { proposal: id, note: None }).await.expect("approved");
}

/// KEYSTONE §14.3 a.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deposit_commits_only_once_the_approver_holds_the_payload() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    for w in &all[1..] {
        w.__vault_hold_payload_fetch(true);
    }
    let text = "gold-copper-nine-17";
    let id = seal(&all[0], "n", text).await;

    let v = wait_vault(&all[1], "the pending deposit", |v| {
        v.deposits.iter().any(|d| d.proposal == Some(id.0))
    })
    .await;
    assert!(v.deposits.iter().all(|d| !d.held));
    match all[1].execute(Command::Approve { proposal: id, note: None }).await {
        Err(MoltError::Vault(VaultRefusal::PayloadNotHeld)) => {}
        other => panic!("expected `payload not held`, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(has(&vault_view(&all[0]).await, "n", VaultDepositState::Pending), "nothing committed");

    for w in &all[1..] {
        w.__vault_hold_payload_fetch(false);
    }
    approve_when_held(&all[1], id).await;
    for w in &all {
        wait_vault(w, "the deposit to commit", |v| has(v, "n", VaultDepositState::Committed)).await;
    }

    // the text never reaches the log, the transport state or the chain
    for (name, ws) in close_and_open(tmp.path(), &all).await {
        let log = serde_json::to_string(&ws.read_log_from(1).expect("log")).expect("json");
        let transport = serde_json::to_string(&ws.read_transport_state()).expect("json");
        let (_, chain) = ws.read_chain().expect("chain");
        assert!(chain.iter().any(|b| matches!(&b.change, ChainChange::Applied { surface: Surface::Vault, .. })));
        let chain = serde_json::to_string(&chain).expect("json");
        for (what, s) in [("log", &log), ("transport", &transport), ("chain", &chain)] {
            assert!(!s.contains(text), "{name}: the text is in its {what}");
        }
    }
}

/// KEYSTONE §14.3 b: c seals a record naming a as its depositor; d runs
/// modified code and signs it. The honest approver never sees the card,
/// and the sealing walk of every node refuses the block, so it lands
/// nowhere while an honest deposit commits beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_depositor_is_refused_by_approver_and_verifier() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    let (a, b, c, d) = (&all[0], &all[1], &all[2], &all[3]);
    c.__vault_skip_checks(true);
    d.__vault_skip_checks(true);
    c.__vault_seal_as("a", "forged", "text", "one");

    let v = wait_vault(d, "the forged card", |v| has(v, "forged", VaultDepositState::Pending)).await;
    let forged = ProposalId(
        v.deposits.iter().find(|x| x.name == "forged").and_then(|x| x.proposal).expect("a proposal"),
    );
    d.execute(Command::Approve { proposal: forged, note: None }).await.expect("the colluder signs");
    // both colluders hold m signatures; past the sealing pace each one's
    // own walk refuses the block it seals
    for w in [c, d] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while approvals(w, forged).await < 2 {
            assert!(tokio::time::Instant::now() < deadline, "the colluders never reached m");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(has(&vault_view(c).await, "forged", VaultDepositState::Pending), "the sealer refused it");

    // the honest approver refuses it: it never even registered the card
    match b.execute(Command::Approve { proposal: forged, note: None }).await {
        Err(MoltError::UnknownProposal(_)) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }

    // liveness: an honest deposit commits meanwhile
    let id = seal(a, "honest", "two").await;
    approve_when_held(b, id).await;
    for w in &all {
        let v = wait_vault(w, "the honest deposit", |v| has(v, "honest", VaultDepositState::Committed)).await;
        assert!(!has(&v, "forged", VaultDepositState::Committed), "the forged deposit committed");
    }
    for w in [a, b] {
        assert!(!vault_view(w).await.deposits.iter().any(|x| x.name == "forged"));
    }

    for (name, ws) in close_and_open(tmp.path(), &all).await {
        let (_, chain) = ws.read_chain().expect("chain");
        molt_engine::verify_chain(&chain).expect("every seat holds a verifying chain");
        let named_forged = chain.iter().any(|blk| match &blk.change {
            ChainChange::Applied { surface: Surface::Vault, payload, .. } => {
                payload.get("name").and_then(serde_json::Value::as_str) == Some("forged")
            }
            _ => false,
        });
        assert!(!named_forged, "{name} holds the forged block");
    }
}
