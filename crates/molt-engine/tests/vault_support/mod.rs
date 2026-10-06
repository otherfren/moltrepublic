// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(dead_code)]

//! Shared harness of the vault integration tests: a real n-seat founding
//! over one in-process relay, driven only through the public commands.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;

/// Seat names: the founder first.
pub const NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];

pub async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

pub async fn wait_for(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nnotice={:?} create.log={:?} join.log={:?}",
            s.notice,
            s.create.run.log,
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            // the file trickle at one piece a second, not fifteen
            mirror_publish_interval_secs: 1,
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

/// ADR-0004: each node adds and confirms the relay itself.
pub async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() }).await.expect("relay add");
    w.execute(Command::RelayConfirm { url: url.to_string(), accept_clearnet: true })
        .await
        .expect("relay confirm");
    wait_for(w, "the relay probe to confirm the relay", |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true }).await.expect("session unlock");
}

/// Found an `m`-of-`n` republic with `features` over the relay at `url`;
/// the engines come back founder first, every seat entered.
pub async fn found_n_at(
    root: &std::path::Path,
    url: &str,
    n: u8,
    m: u8,
    features: &[&str],
) -> Vec<WalletHandle> {
    let a = engine(&root.join(NAMES[0]));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "R".to_string(),
        member: NAMES[0].to_string(),
        threshold: m,
        members: n,
        relays: Vec::new(),
    })
    .await
    .expect("founding starts");
    let seats = usize::from(n) - 1;
    let s = wait_for(&a, "joinable seat links", |s| {
        s.create.seats.len() == seats
            && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();

    let mut all = vec![a];
    for (i, link) in links.into_iter().enumerate() {
        let w = engine(&root.join(NAMES[i + 1]));
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: NAMES[i + 1].to_string() })
            .await
            .expect("join starts");
        all.push(w);
    }
    let founder = &all[0];
    wait_for(founder, "every join accepted", |s| s.create.can_propose).await;
    founder
        .execute(Command::CreatePropose {
            name: "R".to_string(),
            agenda: "one".to_string(),
            features: features.iter().map(|f| (*f).to_string()).collect(),
        })
        .await
        .expect("charter proposed");
    let seed = read_session(founder).await.create.seed.clone();
    founder.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    for w in &all[1..] {
        wait_for(w, "the proposed charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    }
    wait_for(founder, "the founding to seal", |s| s.create.run.outcome == 1).await;
    founder.execute(Command::CreateFinish).await.expect("create finish");
    for w in &all[1..] {
        wait_for(w, "the join to seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty())
            .await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "the member to enter", |s| {
            s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
        })
        .await;
    }
    all
}

/// Close every engine's workspace and open each one from disk (the lock
/// is free once closed): `(seat name, opened workspace)`, founder first.
pub async fn close_and_open(
    root: &std::path::Path,
    all: &[WalletHandle],
) -> Vec<(String, molt_storage::OpenedWorkspace)> {
    let mut out = Vec::new();
    for (i, w) in all.iter().enumerate() {
        let id = read_session(w).await.active_workspace.clone();
        w.execute(Command::CloseWorkspace).await.expect("close");
        let dir = molt_storage::find_workspace_dir(&root.join(NAMES[i]), &id).expect("dir");
        let (ws, _) = molt_storage::open_workspace(&dir).expect("open");
        out.push((NAMES[i].to_string(), ws));
    }
    out
}

/// The vault section of `read_state(vault)`; `None` while hidden (E5).
pub async fn vault_view(w: &WalletHandle) -> Option<molt_core::vault::VaultView> {
    match w
        .execute(Command::ReadState { surface: molt_core::Surface::Vault, channel: None, view: None })
        .await
        .expect("read vault")
    {
        Reply::State(s) => s.vault,
        other => panic!("unexpected: {other:?}"),
    }
}

pub async fn wait_vault(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&molt_core::vault::VaultView) -> bool,
) -> molt_core::vault::VaultView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(v) = vault_view(w).await {
            if pred(&v) {
                return v;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub fn proposed(r: Reply) -> molt_core::ProposalId {
    match r {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Wait until `w` holds a pending proposal `id` on `surface`, then approve.
pub async fn approve_card(w: &WalletHandle, surface: molt_core::Surface, id: molt_core::ProposalId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let snap = match w
            .execute(Command::ReadState { surface, channel: None, view: None })
            .await
            .expect("read state")
        {
            Reply::State(s) => s,
            other => panic!("unexpected: {other:?}"),
        };
        if snap.pending.iter().any(|p| p.id == id) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "card {} never arrived", id.0);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    w.execute(Command::Approve { proposal: id, note: None }).await.expect("approved");
}

/// `all[0]` proposes the vault, `all[1]` makes it m; every seat sees it on.
pub async fn enable_vault(all: &[WalletHandle]) {
    let payload = serde_json::json!({ "op": "set_features", "value": "memory vault" });
    let id = proposed(
        all[0]
            .execute(Command::Propose { surface: molt_core::Surface::Organization, payload })
            .await
            .expect("the enable vote"),
    );
    approve_card(&all[1], molt_core::Surface::Organization, id).await;
    for w in all {
        wait_vault(w, "the vault to switch on", |v| v.real).await;
    }
}

/// `a` deposits `text` under `n`, `b` approves once it holds the payload;
/// every seat in `all` holds the committed version. Its `secret_id`.
pub async fn deposited(all: &[WalletHandle], text: &str) -> String {
    use molt_core::vault::{SecretText, VaultDepositState};
    let cmd = Command::VaultSeal { name: "n".into(), kind: "text".into(), text: SecretText(text.into()) };
    let id = proposed(all[0].execute(cmd).await.expect("sealed"));
    wait_vault(&all[1], "the payload to arrive", |v| {
        v.deposits.iter().any(|d| d.proposal == Some(id.0) && d.held)
    })
    .await;
    all[1].execute(Command::Approve { proposal: id, note: None }).await.expect("approved");
    let mut sid = String::new();
    for w in all {
        let v = wait_vault(w, "the deposit to commit and arrive", |v| {
            v.deposits.iter().any(|d| d.name == "n" && d.state != VaultDepositState::Pending && d.held)
        })
        .await;
        sid = v.deposits.iter().find(|d| d.name == "n").expect("card").secret_id.clone();
    }
    sid
}

async fn has_cut(w: &WalletHandle) -> bool {
    match w.execute(Command::ReadChain).await.expect("read chain") {
        Reply::Chain { blocks, .. } => blocks.iter().any(|v| v.kind == "checkpoint"),
        other => panic!("unexpected: {other:?}"),
    }
}

/// `all[0]` proposes a cut; every seat co-signs and applies it.
pub async fn cut_all(all: &[WalletHandle]) {
    all[0].execute(Command::ProposeCheckpoint).await.expect("cut proposed");
    for (i, w) in all.iter().enumerate() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        while !has_cut(w).await {
            assert!(tokio::time::Instant::now() < deadline, "{} never applied the cut", NAMES[i]);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// `proposer` names `reader` for `sid`, `approver` makes it m; waits until
/// `reader_w` sees it committed.
pub async fn granted(
    proposer: &WalletHandle,
    approver: &WalletHandle,
    reader_w: &WalletHandle,
    sid: &str,
    reader: &str,
) {
    let id = proposed(
        proposer
            .execute(Command::VaultGrant { secret_id: sid.to_string(), reader: reader.to_string() })
            .await
            .expect("grant proposed"),
    );
    let v = wait_vault(approver, "the grant card", |v| v.grants.iter().any(|g| g.proposal == Some(id.0))).await;
    let gid = v.grants.iter().find(|g| g.proposal == Some(id.0)).expect("card").grant_id.clone();
    approver.execute(Command::Approve { proposal: id, note: None }).await.expect("grant approved");
    wait_vault(reader_w, "the grant to commit", |v| {
        v.grants
            .iter()
            .any(|g| g.grant_id == gid && g.state == molt_core::vault::VaultGrantState::Committed)
    })
    .await;
}

/// Read until the text arrives; a pending answer or a base still arriving
/// asks again.
pub async fn read_text(w: &WalletHandle, sid: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        match w.execute(Command::VaultRead { secret_id: sid.to_string() }).await {
            Ok(Reply::VaultText { text, .. }) => return text.0,
            Ok(Reply::VaultPending { have, need, .. }) => {
                assert!(tokio::time::Instant::now() < deadline, "still {have} of {need}");
            }
            Err(e) => assert!(tokio::time::Instant::now() < deadline, "read: {e}"),
            Ok(other) => panic!("unexpected: {other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
