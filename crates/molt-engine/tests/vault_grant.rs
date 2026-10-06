// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Vault grants and reads** (vault build plan S4, spec §8, §14.4): the
//! reader decrypts with one seat dead, nobody else can, a reader restored
//! from a backup older than the grant reads by asking again, and a replace
//! racing a grant supersedes it.

use std::time::Duration;

use molt_core::vault::{SecretText, VaultDepositState, VaultGrantState, VaultRefusal, VaultView};
use molt_core::{ChainChange, Command, MoltError, ProposalId, Reply, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{close_and_open, found_n_at, read_session, wait_for, NAMES};

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

fn proposed(r: Reply) -> ProposalId {
    match r {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn seal(w: &WalletHandle, name: &str, text: &str) -> ProposalId {
    let cmd = Command::VaultSeal { name: name.into(), kind: "text".into(), text: SecretText(text.into()) };
    proposed(w.execute(cmd).await.expect("sealed"))
}

/// Approve once the payload is here.
async fn approve_when_held(w: &WalletHandle, id: ProposalId) {
    wait_vault(w, "the payload to arrive", |v| v.deposits.iter().any(|d| d.proposal == Some(id.0) && d.held)).await;
    w.execute(Command::Approve { proposal: id, note: None }).await.expect("approved");
}

fn committed(v: &VaultView, name: &str) -> Option<String> {
    v.deposits
        .iter()
        .find(|d| d.name == name && d.state != VaultDepositState::Pending)
        .map(|d| d.secret_id.clone())
}

/// `a` deposits `text` under `n`, `b` approves; every live seat in
/// `all` holds it. Returns its `secret_id`.
async fn deposited(all: &[&WalletHandle], text: &str) -> String {
    let id = seal(all[0], "n", text).await;
    approve_when_held(all[1], id).await;
    let mut sid = String::new();
    for w in all {
        let v = wait_vault(w, "the deposit to commit and arrive", |v| {
            v.deposits.iter().any(|d| d.name == "n" && d.state != VaultDepositState::Pending && d.held)
        })
        .await;
        sid = committed(&v, "n").expect("committed");
    }
    sid
}

/// `proposer` names `reader`; `approver` makes it m.
async fn grant(proposer: &WalletHandle, approver: &WalletHandle, sid: &str, reader: &str) -> String {
    let id = proposed(
        proposer
            .execute(Command::VaultGrant { secret_id: sid.to_string(), reader: reader.to_string() })
            .await
            .expect("grant proposed"),
    );
    let v = wait_vault(approver, "the grant card", |v| v.grants.iter().any(|g| g.proposal == Some(id.0))).await;
    let gid = v.grants.iter().find(|g| g.proposal == Some(id.0)).expect("card").grant_id.clone();
    approver.execute(Command::Approve { proposal: id, note: None }).await.expect("grant approved");
    gid
}

async fn wait_grant(w: &WalletHandle, gid: &str) {
    wait_vault(w, "the grant to commit", |v| {
        v.grants.iter().any(|g| g.grant_id == gid && g.state == VaultGrantState::Committed)
    })
    .await;
}

/// Read until the text arrives; every pending answer asks again.
async fn read_text(w: &WalletHandle, sid: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        match w.execute(Command::VaultRead { secret_id: sid.to_string() }).await.expect("read") {
            Reply::VaultText { text, .. } => return text.0,
            Reply::VaultPending { have, need, .. } => {
                assert!(tokio::time::Instant::now() < deadline, "still {have} of {need}");
            }
            other => panic!("unexpected: {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The copy on disk is checked again after an open.
async fn wait_held(w: &WalletHandle, sid: &str) {
    wait_vault(w, "the payload checked", |v| v.deposits.iter().any(|d| d.secret_id == sid && d.held)).await;
}

fn not_the_reader(r: Result<Reply, MoltError>) {
    match r {
        Err(MoltError::Vault(VaultRefusal::NotTheReader)) => {}
        other => panic!("expected `not the reader`, got {other:?}"),
    }
}

/// KEYSTONE §14.4: 2-of-4, `c` closed before the grant commits; `d`
/// combines its own share with `b`'s answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_reader_decrypts_with_one_seat_dead() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    let (a, b, c, d) = (&all[0], &all[1], &all[2], &all[3]);
    let text = "amber-falcon-31";
    let sid = deposited(&[a, b, c, d], text).await;

    c.execute(Command::CloseWorkspace).await.expect("c goes away");
    let gid = grant(b, a, &sid, "d").await;
    wait_grant(d, &gid).await;
    assert_eq!(read_text(d, &sid).await, text);
    let card = vault_view(d).await.grants.into_iter().find(|g| g.grant_id == gid).expect("card");
    assert!(card.mine && card.answers >= card.need && card.bad_answers.is_empty(), "{card:?}");

    // everyone else sees the audit entry, content locked
    for w in [a, b] {
        let v = wait_vault(w, "the audit entry", |v| v.grants.iter().any(|g| g.grant_id == gid)).await;
        let g = v.grants.iter().find(|g| g.grant_id == gid).expect("card");
        assert_eq!((g.reader.as_str(), g.name.as_str(), g.mine, g.state), ("d", "n", false, VaultGrantState::Committed));
    }
}

/// KEYSTONE §14.4: only the elected reader reads; the answers pass every
/// other seat without opening (sealed to `d`'s founding key, unit-pinned
/// in `grant_tests.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_reader_cannot_decrypt() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    let (a, b, c, d) = (&all[0], &all[1], &all[2], &all[3]);
    let text = "cobalt-meadow-58";
    let sid = deposited(&[a, b, c, d], text).await;
    let gid = grant(c, b, &sid, "d").await;
    for w in &all {
        wait_grant(w, &gid).await;
    }
    assert_eq!(read_text(d, &sid).await, text);
    for w in [a, b, c] {
        not_the_reader(w.execute(Command::VaultRead { secret_id: sid.clone() }).await);
        let g = vault_view(w).await.grants.into_iter().find(|g| g.grant_id == gid).expect("card");
        assert!(!g.mine && g.answers == 0 && g.bad_answers.is_empty(), "{g:?}");
    }

    // nothing anyone persisted carries the text, nor any holder's share
    let mut disks = Vec::new();
    let mut shares = Vec::new();
    for (name, ws) in close_and_open(tmp.path(), &all).await {
        let ts = ws.read_transport_state();
        let (_, chain) = ws.read_chain().expect("chain");
        shares.extend(own_share(&name, &ts, &chain));
        let log = serde_json::to_string(&ws.read_log_from(1).expect("log")).expect("json");
        let transport = serde_json::to_string(&ts).expect("json");
        let chain = serde_json::to_string(&chain).expect("json");
        disks.push((name, [("log", log), ("transport", transport), ("chain", chain)]));
    }
    assert_eq!(shares.len(), 3, "every seat but the depositor holds a share");
    for (name, files) in &disks {
        for (what, s) in files {
            assert!(!s.contains(text), "{name}: the text is in its {what}");
            for (holder, hex, json) in &shares {
                assert!(!s.contains(hex) && !s.contains(json), "{name}: {holder}'s share is in its {what}");
            }
        }
    }
}

/// `name`'s share of the deposit on `chain`, opened with the vault seed
/// its `transport.state` keeps: `(name, hex, json bytes)`.
fn own_share(
    name: &str,
    ts: &molt_core::TransportState,
    chain: &[molt_core::ChainBlock],
) -> Option<(String, String, String)> {
    let ChainChange::Genesis { republic_id, rule_m, identities, .. } = &chain.first()?.change else {
        return None;
    };
    let ctx = molt_core::vault::VaultCtx {
        m: *rule_m,
        holders_in_genesis_order: identities
            .iter()
            .map(|i| (i.member.clone(), i.identity_pk.clone(), i.vault_pk.clone()))
            .collect(),
    };
    let dep = chain.iter().find_map(|b| match &b.change {
        ChainChange::Applied { surface: Surface::Vault, payload, .. } => {
            match serde_json::from_value::<molt_core::vault::VaultOp>(payload.clone()) {
                Ok(molt_core::vault::VaultOp::Deposit(d)) => Some(d),
                _ => None,
            }
        }
        _ => None,
    })?;
    let seed = <[u8; 32]>::try_from(ts.vault_seed.as_ref()?.0.as_slice()).ok()?;
    let (sk, _) = molt_vault::vault_keypair(&seed);
    let share = molt_vault::check_my_share(&dep, republic_id, &ctx, name, &sk).ok()?;
    let bytes = share.as_bytes();
    Some((name.to_string(), hex::encode(bytes), serde_json::to_string(bytes.as_slice()).expect("json")))
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read dir") {
        let e = e.expect("entry");
        let dest = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).expect("copy");
        }
    }
}

/// KEYSTONE §14.4: `d` reads once, then comes back from a copy of its
/// workspace taken before the grant - no answer survives, the grant
/// arrives by catch-up, and asking again reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_restored_from_a_pre_grant_backup_reads_by_asking_again() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    let (a, b, c, d) = (&all[0], &all[1], &all[2], &all[3]);
    let text = "silver-harbor-64";
    let sid = deposited(&[a, b, c, d], text).await;

    let ws = read_session(d).await.active_workspace.clone();
    let dir = molt_storage::find_workspace_dir(&tmp.path().join(NAMES[3]), &ws).expect("dir");
    let backup = tmp.path().join("d-backup");
    d.execute(Command::CloseWorkspace).await.expect("close");
    copy_dir(&dir, &backup);
    d.execute(Command::OpenWorkspace { id: ws.clone() }).await.expect("reopen");
    wait_for(d, "d back in", |s| s.active_workspace == ws).await;
    wait_held(d, &sid).await;

    let gid = grant(a, b, &sid, "d").await;
    wait_grant(d, &gid).await;
    assert_eq!(read_text(d, &sid).await, text);

    d.execute(Command::CloseWorkspace).await.expect("close");
    std::fs::remove_dir_all(&dir).expect("rm");
    copy_dir(&backup, &dir);
    d.execute(Command::OpenWorkspace { id: ws.clone() }).await.expect("restore");
    wait_for(d, "d restored", |s| s.active_workspace == ws).await;
    let v = vault_view(d).await;
    assert!(!v.grants.iter().any(|g| g.grant_id == gid), "the backup predates the grant");
    wait_grant(d, &gid).await;
    wait_held(d, &sid).await;
    assert_eq!(read_text(d, &sid).await, text);
}

/// KEYSTONE §14.4: a replace commits while a grant on the old version is
/// still a vote - the grant is superseded everywhere and nothing answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replace_racing_a_grant_supersedes_it() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    let (a, b, c, d) = (&all[0], &all[1], &all[2], &all[3]);
    let old = deposited(&[a, b, c, d], "one").await;

    let g = proposed(
        b.execute(Command::VaultGrant { secret_id: old.clone(), reader: "d".into() }).await.expect("grant proposed"),
    );
    for w in &all {
        wait_vault(w, "the grant card", |v| v.grants.iter().any(|x| x.proposal == Some(g.0))).await;
    }
    let r = seal(a, "n", "two").await;
    approve_when_held(c, r).await;
    let mut new = String::new();
    for w in &all {
        let v = wait_vault(w, "the replace, the grant gone", |v| {
            committed(v, "n").is_some_and(|s| s != old) && v.grants.is_empty()
        })
        .await;
        new = committed(&v, "n").expect("current");
        assert_eq!(v.deposits.iter().find(|x| x.secret_id == new).and_then(|x| x.replaces.clone()), Some(old.clone()));
    }
    assert!(c.execute(Command::Approve { proposal: g, note: None }).await.is_err(), "a superseded grant takes no vote");
    not_the_reader(d.execute(Command::VaultRead { secret_id: old.clone() }).await);
    not_the_reader(d.execute(Command::VaultRead { secret_id: new }).await);

    tokio::time::sleep(Duration::from_secs(3)).await;
    for (name, ws) in close_and_open(tmp.path(), &all).await {
        let (_, chain) = ws.read_chain().expect("chain");
        let grants = chain
            .iter()
            .filter(|blk| match &blk.change {
                ChainChange::Applied { surface: Surface::Vault, payload, .. } => {
                    payload.get("op").and_then(serde_json::Value::as_str) == Some("grant")
                }
                _ => false,
            })
            .count();
        assert_eq!(grants, 0, "{name} committed the grant");
    }
}
