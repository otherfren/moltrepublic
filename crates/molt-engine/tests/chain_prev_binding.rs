// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The gate of `block_identity_plan.md` §3b, measured**: a 2-of-3
//! republic over a local relay, one seat's inbound delayed by about one
//! block, 50 proposals. Propose-to-sealed per card with and without the
//! `prev` binding (`molt-chain-change-v3`). Run explicitly:
//! `cargo test -p molt-engine --test chain_prev_binding -- --ignored --nocapture`.

mod common;

use common::{adopt_relay, engine, read_session, wait_for_secs};
use std::time::{Duration, Instant};

use molt_core::{Command, ProposalState, Reply};
use molt_engine::WalletHandle;

fn add_page(path: &str, text: &str) -> serde_json::Value {
    let patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{text}\n"
    );
    serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "+1" })
}

async fn proposals(w: &WalletHandle) -> Vec<molt_core::ProposalView> {
    let Reply::Proposals { proposals, .. } = w.execute(Command::LIST_ALL_PROPOSALS).await.expect("list") else {
        panic!("list_proposals answers Reply::Proposals");
    };
    proposals
}

async fn approve_open(w: &WalletHandle) {
    for p in proposals(w).await {
        if p.state == ProposalState::Proposed && !p.approved_by_me {
            let _ = w.execute(Command::Approve { proposal: p.id, note: None }).await;
        }
    }
}

async fn wait(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&molt_core::SessionView) -> bool,
) -> Box<molt_core::SessionView> {
    wait_for_secs(w, what, 180, pred).await
}

async fn found_trio(root: &std::path::Path, url: &str) -> [WalletHandle; 3] {
    let a = engine(&root.join("mara"));
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
    let s = wait(&a, "two seat links", |s: &molt_core::SessionView| {
        s.create.seats.len() >= 2
            && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();
    let mut joiners = Vec::new();
    for (name, link) in ["walter", "dora"].iter().zip(links) {
        let w = engine(&root.join(name));
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: (*name).to_string(), relays: Vec::new() })
            .await
            .expect("join start");
        joiners.push(w);
    }
    wait(&a, "both joins", |s: &molt_core::SessionView| s.create.can_propose).await;
    a.execute(Command::CreatePropose { name: "Drei".to_string(), agenda: String::new(), features: vec!["memory".to_string()] })
        .await
        .expect("charter proposed");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    for w in &joiners {
        wait(w, "the charter", |s: &molt_core::SessionView| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    }
    wait(&a, "the seal", |s: &molt_core::SessionView| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    for w in &joiners {
        wait(w, "the join seal", |s: &molt_core::SessionView| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait(w, "the joiner to enter", |s: &molt_core::SessionView| {
            s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
        })
        .await;
    }
    let mut it = joiners.into_iter();
    [a, it.next().expect("walter"), it.next().expect("dora")]
}

async fn status(w: &WalletHandle) -> molt_core::StatusView {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// One run: `(per-card propose-to-sealed, re-sign rounds, fork decisions)`.
async fn run(bound: bool) -> (Vec<Duration>, u64, u64) {
    molt_engine::__prev_binding(bound);
    let relay = nostr_relay_builder::MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let seats = found_trio(tmp.path(), &url).await;
    assert!(status(&seats[0]).await.chain_sync.block_v2_since > 0, "a new founding ratifies block-v2");
    // dora lags one block: the seal pace is 5 s
    seats[2].__delay_inbound_ms(6_000);
    let resigns_before = molt_engine::__resign_count();
    const CARDS: usize = 50;
    const BURST: usize = 5;
    let mut opened: Vec<(u64, Instant)> = Vec::new();
    let mut sealed: std::collections::BTreeMap<u64, Duration> = std::collections::BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(1800);
    let mut next = 0usize;
    while sealed.len() < CARDS {
        let open = opened.len() - sealed.len();
        if open == 0 && next < CARDS {
            for _ in 0..BURST.min(CARDS - next) {
                let proposer = &seats[next % 2];
                let reply = proposer
                    .execute(Command::Propose {
                        surface: molt_core::Surface::Memory,
                        payload: add_page(&format!("p{next}.md"), "x"),
                    })
                    .await
                    .expect("proposes");
                let Reply::Proposed { id, .. } = reply else { panic!("{reply:?}") };
                opened.push((id.0, Instant::now()));
                next += 1;
            }
        }
        for w in &seats {
            approve_open(w).await;
        }
        let views = proposals(&seats[0]).await;
        for (id, at) in &opened {
            if !sealed.contains_key(id) && views.iter().any(|p| p.id.0 == *id && p.state == ProposalState::Applied) {
                sealed.insert(*id, at.elapsed());
            }
        }
        assert!(Instant::now() < deadline, "sealed {} of {CARDS}", sealed.len());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut forks = 0;
    for w in &seats {
        forks += status(w).await.chain_sync.fork_decisions;
    }
    seats[2].__delay_inbound_ms(0);
    (sealed.into_values().collect(), molt_engine::__resign_count() - resigns_before, forks)
}

fn pct(sorted: &[Duration], p: usize) -> f64 {
    let i = (sorted.len() * p).div_ceil(100).saturating_sub(1).min(sorted.len() - 1);
    sorted[i].as_secs_f64()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: about 30 min"]
async fn prev_binding_adds_no_wait_with_a_lagging_seat() {
    let mut medians = [Vec::new(), Vec::new()];
    for round in 0..3 {
        for (i, bound) in [false, true].into_iter().enumerate() {
            let (mut times, resigns, forks) = run(bound).await;
            times.sort();
            eprintln!(
                "measure round={round} bound={bound} median={:.2} p95={:.2} max={:.2} resigns={resigns} forks={forks}",
                pct(&times, 50),
                pct(&times, 95),
                pct(&times, 100)
            );
            medians[i].push(pct(&times, 50));
        }
    }
    eprintln!("measure medians unbound={:?} bound={:?}", medians[0], medians[1]);
}
