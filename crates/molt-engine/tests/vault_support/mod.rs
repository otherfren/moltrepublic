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
