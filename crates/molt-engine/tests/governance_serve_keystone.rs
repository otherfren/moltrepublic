// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Keystone for the governance serve (gap 26)**: whatever rule quiets the
//! re-serve between level seats, a seat behind that asks ONCE still
//! gets everything - every open card, the standing approvals on it, and
//! the tombstones it needs to judge withdrawn cards. A real 2-of-3
//! republic over a real relay: dora closes, the other two run on (one card
//! applied, one left open with mara's standing approval, one withdrawn),
//! dora reopens and must end able to vote.

mod common;

use common::{adopt_relay, engine, read_session, wait_for_secs};
use std::time::Duration;

use molt_core::{Command, ProposalId, ProposalState, ProposalView, Reply};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

fn add_page(path: &str, text: &str) -> serde_json::Value {
    let patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{text}\n"
    );
    serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "+1" })
}

async fn workspace_id(w: &WalletHandle) -> String {
    read_session(w)
        .await
        .workspaces
        .first()
        .expect("the workspace")
        .id
        .clone()
}

async fn proposals(w: &WalletHandle) -> Vec<ProposalView> {
    match w.execute(Command::LIST_ALL_PROPOSALS).await.expect("list") {
        Reply::Proposals { proposals, .. } => proposals,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn chain_height(w: &WalletHandle) -> u64 {
    match w.execute(Command::ReadChain).await.expect("read chain") {
        // the chain lists NEWEST first - the head is the first row
        Reply::Chain { blocks, .. } => blocks.first().map(|b| b.height).unwrap_or(0),
        other => panic!("unexpected: {other:?}"),
    }
}

/// Poll the proposal listing until `pred` holds, at most `secs`.
async fn wait_proposals(
    w: &WalletHandle,
    what: &str,
    secs: u64,
    pred: impl Fn(&[ProposalView]) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let ps = proposals(w).await;
        if pred(&ps) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\n{ps:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Poll `chain_height(w)` until it reaches `want`, at most `secs`.
async fn wait_height(w: &WalletHandle, what: &str, secs: u64, want: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let got = chain_height(w).await;
        if got == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what} (got {got}, want {want})"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The id of the open card whose patch names `path`.
async fn open_card_for(w: &WalletHandle, path: &str) -> ProposalId {
    proposals(w)
        .await
        .iter()
        .find(|p| {
            p.state == ProposalState::Proposed
                && p.payload.get("value").and_then(serde_json::Value::as_str)
                    .is_some_and(|v| v.contains(path))
        })
        .map(|p| p.id)
        .unwrap_or_else(|| panic!("no open card for {path}"))
}

/// A real 2-of-3 republic over `url`: mara founds, walter and dora join.
/// The `chain_convergence.rs` twin, kept separate so this keystone stays
/// readable on its own.
async fn found_trio(root: &std::path::Path, url: &str) -> [WalletHandle; 3] {
    let a = engine(&root.join("mara"));
    // B6 alone: a republic founded before the lock existed (the lock's
    // fallback grace would stretch every seal past a comfortable wait)
    a.__found_locked(false);
    a.__lock_available(false);
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Drei".to_string(),
        member: "mara".to_string(),
        members: 3,
        threshold: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for_secs(&a, "two seat links", 180, |s| {
        s.create.seats.len() >= 2
            && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();
    let mut joiners = Vec::new();
    for (name, link) in ["walter", "dora"].iter().zip(links) {
        let w = engine(&root.join(name));
        w.__lock_available(false);
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: (*name).to_string(), relays: Vec::new() })
            .await
            .expect("join start");
        joiners.push(w);
    }
    wait_for_secs(&a, "both joins", 180, |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Drei".to_string(),
        agenda: String::new(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    for w in &joiners {
        wait_for_secs(w, "the charter", 180, |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    }
    wait_for_secs(&a, "the seal", 180, |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    for w in &joiners {
        wait_for_secs(w, "the join seal", 180, |s| {
            s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
        })
        .await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for_secs(w, "the joiner to enter", 180, |s| {
            s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
        })
        .await;
    }
    let mut it = joiners.into_iter();
    [a, it.next().expect("walter"), it.next().expect("dora")]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_behind_gets_every_card_approval_and_tombstone() {
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
    let [mara, walter, dora] = found_trio(tmp.path(), &url).await;

    // dora goes away; the republic runs on
    let dora_ws = workspace_id(&dora).await;
    dora.execute(Command::CloseWorkspace).await.expect("dora closes");

    // three cards of mara's: one sealed by both, one left OPEN with only
    // mara's standing approval, one pulled back by its proposer
    for path in ["applied.md", "open.md", "pulled.md"] {
        mara
            .execute(Command::Propose {
                surface: molt_core::Surface::Memory,
                payload: add_page(path, path),
            })
            .await
            .expect("mara proposes");
    }
    wait_proposals(&walter, "walter sees all three cards", 60, |ps| {
        ps.iter().filter(|p| p.state == ProposalState::Proposed).count() == 3
    })
    .await;
    let applied = open_card_for(&walter, "applied.md").await;
    mara.execute(Command::Approve { proposal: applied, note: None }).await.expect("mara signs");
    walter.execute(Command::Approve { proposal: applied, note: None }).await.expect("walter co-signs");
    // the seal pace holds a card at m for one head round (A2.2), so this
    // wait must cover the grace, not just the gossip
    wait_proposals(&walter, "the page seals", 180, |ps| {
        ps.iter().filter(|p| p.state == ProposalState::Proposed).count() == 2
    })
    .await;
    let sealed_head = chain_height(&walter).await;
    let walter_views = proposals(&walter).await;
    assert!(
        sealed_head >= 1,
        "the page sealed a block: head={sealed_head} listing={walter_views:?}"
    );
    let pulled = open_card_for(&walter, "pulled.md").await;
    mara
        .execute(Command::Withdraw { proposal: pulled, note: None })
        .await
        .expect("mara pulls it back");
    wait_proposals(&walter, "walter sees the withdrawal", 60, |ps| {
        ps.iter().all(|p| p.id != pulled || p.state != ProposalState::Proposed)
    })
    .await;

    // dora is back: it asks ONCE and gets everything
    let served = |ws: [&WalletHandle; 3]| ws.iter().map(|w| w.__governance_served()).sum::<u64>();
    let before = served([&mara, &walter, &dora]);
    dora.execute(Command::OpenWorkspace { id: dora_ws }).await.expect("dora reopens");
    let walter_height = chain_height(&walter).await;
    wait_height(&dora, "dora catches up to walter's head", 180, walter_height).await;
    let walter_after = chain_height(&walter).await;
    assert_eq!(
        chain_height(&dora).await, walter_after,
        "dora is at walter's head (walter was {walter_height}, now {walter_after})"
    );

    // the tombstone judged: the pulled-back card is NOT open on dora, and
    // the governance serve's frames have landed (they are in flight while
    // the head moves - poll, never read once)
    wait_proposals(&dora, "dora's listing settles", 60, |ps| {
        ps.iter().any(|p| p.state == ProposalState::Applied)
            && ps.iter().any(|p| {
                p.state == ProposalState::Proposed
                    && p.approvals == 1
                    && p.payload.get("value").and_then(serde_json::Value::as_str)
                        .is_some_and(|v| v.contains("open.md"))
            })
    })
    .await;
    assert!(served([&mara, &walter, &dora]) > before, "the seat behind was served the open governance");
    let dora_views = proposals(&dora).await;
    assert!(
        dora_views
            .iter()
            .all(|p| !(p.id == pulled && p.state == ProposalState::Proposed)),
        "the withdrawn card never reopens on dora: {dora_views:?}"
    );
    // the open card stands, with mara's standing approval counted
    let dora_head = chain_height(&dora).await;
    let open = dora_views
        .iter()
        .find(|p| {
            p.state == ProposalState::Proposed
                && p.payload.get("value").and_then(serde_json::Value::as_str)
                    .is_some_and(|v| v.contains("open.md"))
        })
        .unwrap_or_else(|| panic!("dora holds the open card: head={dora_head} views={dora_views:?}"));
    assert_eq!(open.approvals, 1, "mara's standing approval counts: {open:?}");

    // dora's own vote lands: the card seals everywhere
    let sealed_id = open.id;
    dora.execute(Command::Approve { proposal: sealed_id, note: None }).await.expect("dora votes");
    for (w, who) in [(&mara, "mara"), (&walter, "walter"), (&dora, "dora")] {
        wait_proposals(w, &format!("{who} sees the card sealed"), 180, |ps| {
            ps.iter().all(|p| !(p.id == sealed_id && p.state == ProposalState::Proposed))
        })
        .await;
    }
    let heights = (
        chain_height(&mara).await,
        chain_height(&walter).await,
        chain_height(&dora).await,
    );
    assert_eq!(heights.0, heights.1, "mara and walter on one head");
    assert_eq!(heights.0, heights.2, "dora ended on the same head");
}

/// Gap 26, the level returner: dora is closed while mara proposes and
/// approves a card, no block seals, so dora is LEVEL when it reopens - not
/// behind, not joining. Its open card must still reach it within a few
/// beacons, with mara's standing approval, and dora's vote must seal it.
/// Here the card arrives by the relay's stored frames; the unit tests in
/// `chain/sync_tests.rs` pin the serve rule itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_level_seat_back_from_an_absence_gets_its_open_cards() {
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
    let [mara, walter, dora] = found_trio(tmp.path(), &url).await;

    let dora_ws = workspace_id(&dora).await;
    dora.execute(Command::CloseWorkspace).await.expect("dora closes");
    let head = chain_height(&walter).await;

    mara
        .execute(Command::Propose { surface: molt_core::Surface::Memory, payload: add_page("away.md", "away") })
        .await
        .expect("mara proposes");
    wait_proposals(&walter, "walter sees the card", 60, |ps| {
        ps.iter().any(|p| p.state == ProposalState::Proposed)
    })
    .await;
    let card = open_card_for(&walter, "away.md").await;
    mara.execute(Command::Approve { proposal: card, note: None }).await.expect("mara signs");
    wait_proposals(&walter, "walter counts mara's approval", 60, |ps| {
        ps.iter().any(|p| p.id == card && p.approvals == 1)
    })
    .await;
    assert_eq!(chain_height(&mara).await, head, "nothing sealed while dora was away");

    dora.execute(Command::OpenWorkspace { id: dora_ws }).await.expect("dora reopens");
    assert_eq!(chain_height(&dora).await, head, "dora is level, not behind");

    // a bounded number of beacons, each with a short window to land
    let mut seen = false;
    for _ in 0..3 {
        dora.__beacon_now();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while tokio::time::Instant::now() < deadline {
            if proposals(&dora).await.iter().any(|p| {
                p.id == card && p.state == ProposalState::Proposed && p.approvals == 1
            }) {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if seen {
            break;
        }
    }
    assert!(seen, "dora holds the open card with mara's approval: {:?}", proposals(&dora).await);

    // a seat's first request since a start or a return may be served once;
    // after that, level beacons of every seat re-serve nothing
    let served = |ws: [&WalletHandle; 3]| ws.iter().map(|w| w.__governance_served()).sum::<u64>();
    for w in [&mara, &walter, &dora] {
        w.__beacon_now();
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before = served([&mara, &walter, &dora]);
    for _ in 0..3 {
        for w in [&mara, &walter, &dora] {
            w.__beacon_now();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert_eq!(served([&mara, &walter, &dora]), before, "no governance re-serve between level seats");

    dora.execute(Command::Approve { proposal: card, note: None }).await.expect("dora votes");
    for (w, who) in [(&mara, "mara"), (&walter, "walter"), (&dora, "dora")] {
        wait_proposals(w, &format!("{who} sees the card sealed"), 180, |ps| {
            ps.iter().all(|p| !(p.id == card && p.state == ProposalState::Proposed))
        })
        .await;
    }
}
