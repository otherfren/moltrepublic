// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **F00 keystone - concurrent writers end on ONE chain**
//! (`docs/chain/chain_reorg.md`, `docs/reviews/mcp_agent_friction_fixes.md`
//! A2): two seats of a real 2-of-2 republic propose in bursts over a real
//! relay and approve each other's cards as they appear. Whatever order the
//! seals happen in on either side, both seats end with identical blocks,
//! identical wikis and no divergence flag - the pacing keeps forks rare and
//! the deep tie-break heals the ones that happen.

use std::time::Duration;

use molt_core::{Command, GroupConfig, ProposalState, Reply, SessionSettings, SessionView};
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
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
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

async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() }).await.expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    wait_for(w, "the relay probe", |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// A real 2-of-2 republic over the relay: petra founds, walter joins.
async fn found_pair(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Konvergenz".to_string(),
        member: "petra".to_string(),
        members: 2,
        threshold: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "the seat link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();
    let b = engine(&root.join("member"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "walter".to_string(),
    })
    .await
    .expect("join start");
    wait_for(&a, "the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Konvergenz".to_string(),
        agenda: String::new(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    wait_for(&b, "the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    {
        let seed_ = read_session(&b).await.join.seed.clone();
        b.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("joiner backup confirm");
    }
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

fn add_page(path: &str, text: &str) -> serde_json::Value {
    let patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{text}\n"
    );
    serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "+1" })
}

/// Approve every open foreign card this seat sees; how many it approved.
async fn approve_open(w: &WalletHandle) -> usize {
    let Reply::Proposals { proposals } = w.execute(Command::ListProposals).await.expect("list") else {
        panic!("list_proposals answers Reply::Proposals");
    };
    let mut n = 0;
    for p in proposals {
        if p.state == ProposalState::Proposed
            && !p.approved_by_me
            && w.execute(Command::Approve { proposal: p.id }).await.is_ok()
        {
            n += 1;
        }
    }
    n
}

async fn chain(w: &WalletHandle) -> Vec<(u64, u64, Vec<String>)> {
    let Reply::Chain { blocks, .. } = w.execute(Command::ReadChain).await.expect("read chain") else {
        panic!("read_chain answers Reply::Chain");
    };
    blocks
        .iter()
        .map(|b| (b.height, b.proposal_id, b.signers.clone()))
        .collect()
}

async fn diverged(w: &WalletHandle) -> usize {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => s.chain_diverged.len(),
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wiki_docs(w: &WalletHandle) -> u64 {
    match w
        .execute(Command::ReadState {
            surface: molt_core::Surface::Memory,
            channel: None,
            view: None,
        })
        .await
        .expect("read memory")
    {
        Reply::State(s) => s.wiki_docs,
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_bursts_end_on_one_chain() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().to_path_buf();
    let (a, b) = found_pair(&root, &url).await;

    // the burst: both seats propose four pages each, interleaved, at once
    const PAGES: usize = 4;
    for i in 0..PAGES {
        a.execute(Command::Propose {
            surface: molt_core::Surface::Memory,
            payload: add_page(&format!("petra/p{i}.md"), "von petra"),
        })
        .await
        .expect("petra proposes");
        b.execute(Command::Propose {
            surface: molt_core::Surface::Memory,
            payload: add_page(&format!("walter/w{i}.md"), "von walter"),
        })
        .await
        .expect("walter proposes");
    }

    // both approve whatever foreign card appears, until every page is on
    // BOTH chains and the chains are the same blocks in the same order
    let want = 2 * PAGES;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        approve_open(&a).await;
        approve_open(&b).await;
        let (ca, cb) = (chain(&a).await, chain(&b).await);
        let applied = |c: &Vec<(u64, u64, Vec<String>)>| c.iter().filter(|(_, pid, _)| *pid != 0).count();
        if applied(&ca) == want && ca == cb {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chains never converged:\npetra  {ca:?}\nwalter {cb:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(chain(&a).await, chain(&b).await, "one chain on both seats");
    assert_eq!((diverged(&a).await, diverged(&b).await), (0, 0), "no divergence flag survives");
    let want_docs = u64::try_from(want).expect("small");
    assert_eq!((wiki_docs(&a).await, wiki_docs(&b).await), (want_docs, want_docs), "the wikis agree");
}
