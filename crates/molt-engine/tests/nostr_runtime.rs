// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **N5.2 keystone** — the kind-445 group runtime carries a WorkspaceEvent
//! between two real engines over one relay, with no mesh and no queues.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nsession: notice={:?} create.log={:?} join.log={:?}",
            s.notice,
            s.create.run.log,
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

/// ADR-0004: nothing is pre-configured — each node adds and confirms the
/// relay itself, then unlocks the clearnet/local session.
async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() })
        .await
        .expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// Found a real 2-of-2 "Chess Club" over one in-process relay, exactly as
/// `nostr_founding.rs`'s capstone does, and hand back both live engines.
async fn found_two_of_two(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("a production founding starts over the confirmed relay");

    let s = wait_for(&a, "the seat link to become a joinable v2 link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&root.join("joiner"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");

    wait_for(&a, "the founder to accept petra's join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess, decide together".to_string(),
    })
    .await
    .expect("charter proposed");

    wait_for(&b, "petra to see the proposed charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");

    wait_for(&a, "the founding to seal on the founder", |s| {
        s.create.run.outcome == 1 && s.screen == molt_core::Screen::Main
    })
    .await;
    wait_for(&b, "the join to seal on petra", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// Read the chat surface's applied rows.
async fn read_chat(w: &WalletHandle) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface: molt_core::Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

/// **N5.2 KEYSTONE** — two engines converge a chat message over one relay,
/// with no mesh and no queues.
///
/// The REOPEN is the anti-inert core, not ceremony. `maybe_finalize` takes the
/// ritual and `NostrRitual::drop` aborts every inbound task it owns, so after
/// closing both workspaces no ritual subscription and no ritual channel exist
/// anywhere in the process. Anything that arrives after that arrived over a
/// runtime rebuilt from `transport.state` alone — which is the thing under
/// test. Without the reopen this could pass while a leftover ritual
/// subscription did the work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_converge_a_chat_message_over_one_relay() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let (a, b) = found_two_of_two(tmp.path(), &url).await;
    let ws_a = read_session(&a).await.active_workspace.clone();
    let ws_b = read_session(&b).await.active_workspace.clone();

    // every ritual task dies here
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
    a.execute(Command::OpenWorkspace { id: ws_a })
        .await
        .expect("reopen a");
    b.execute(Command::OpenWorkspace { id: ws_b })
        .await
        .expect("reopen b");

    a.execute(Command::Chat {
        body: "over the relay".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("walter speaks");

    // …and it reaches petra, authored by walter — the author matters: a row
    // that arrived with the wrong `from` would mean the envelope was
    // re-authored somewhere, not carried
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let rows = read_chat(&b).await;
        if rows.iter().any(|r| {
            r.get("body").and_then(|v| v.as_str()) == Some("over the relay")
                && r.get("from").and_then(|v| v.as_str()) == Some("walter")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat message never crossed the relay; petra sees {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // …and again after a SECOND reopen, which is what proves the ratchet was
    // persisted. The runtime advances the MLS sender ratchet on every publish;
    // if the close did not merge it back into `transport.state`, the reopen
    // restores the founding blob and this second message REUSES a sender
    // generation — which every peer replay-rejects and silently drops. The
    // first round cannot catch that, because it starts from the founding blob
    // either way.
    let ws_a = read_session(&a).await.active_workspace.clone();
    let ws_b = read_session(&b).await.active_workspace.clone();
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
    a.execute(Command::OpenWorkspace { id: ws_a }).await.expect("reopen a");
    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");

    a.execute(Command::Chat {
        body: "and again".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("walter speaks again");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let rows = read_chat(&b).await;
        if rows
            .iter()
            .any(|r| r.get("body").and_then(|v| v.as_str()) == Some("and again"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second message never crossed — the ratchet regressed on reopen; petra sees {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
