// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The gate of `block_identity_plan.md` §3b, measured**: a 2-of-3
//! republic over a local relay, one seat's inbound delayed by about one
//! block, 50 proposals; propose-to-sealed per card with the `prev` binding
//! (`molt-chain-change-v3`). The run without it (2026-11-24, before the
//! binding shipped) is in `persistent_chain.md` §5.1. Run explicitly:
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

/// One card of a run.
struct Card {
    secs: f64,
    resigns: u64,
    /// Its place in its burst (0 = first proposed).
    slot: usize,
    /// Cards of its burst sealed before it.
    sealed_before: usize,
}

/// One run of `cards` cards: per card, and the fork decisions.
async fn run(unbound: bool, cards: usize) -> (Vec<Card>, u64) {
    run_with(unbound, cards, [0, 0, 6_000]).await
}

/// [`run`] with each seat's inbound delay in ms (mara, walter, dora).
async fn run_with(unbound: bool, cards: usize, delays: [u64; 3]) -> (Vec<Card>, u64) {
    molt_engine::__prev_unbound(unbound);
    let relay = nostr_relay_builder::MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let seats = found_trio(tmp.path(), &url).await;
    assert!(status(&seats[0]).await.chain_sync.block_v2_since > 0, "a new founding ratifies block-v2");
    // dora lags one block: the seal pace is 5 s
    for (w, d) in seats.iter().zip(delays) {
        w.__delay_inbound_ms(d);
    }
    let resigns_before = molt_engine::__resign_counts();
    const BURST: usize = 5;
    let mut opened: Vec<(u64, Instant, usize)> = Vec::new();
    let mut sealed: std::collections::BTreeMap<u64, (Duration, usize)> = std::collections::BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(7200);
    let mut next = 0usize;
    let mut burst_sealed = 0usize;
    while sealed.len() < cards {
        if opened.len() == sealed.len() && next < cards {
            burst_sealed = 0;
            for slot in 0..BURST.min(cards - next) {
                let reply = seats[next % 2]
                    .execute(Command::Propose {
                        surface: molt_core::Surface::Memory,
                        payload: add_page(&format!("p{next}.md"), "x"),
                    })
                    .await
                    .expect("proposes");
                let Reply::Proposed { id, .. } = reply else { panic!("{reply:?}") };
                opened.push((id.0, Instant::now(), slot));
                next += 1;
            }
        }
        for w in &seats {
            approve_open(w).await;
        }
        let views = proposals(&seats[0]).await;
        for (id, at, _) in &opened {
            if !sealed.contains_key(id) && views.iter().any(|p| p.id.0 == *id && p.state == ProposalState::Applied) {
                sealed.insert(*id, (at.elapsed(), burst_sealed));
                burst_sealed += 1;
            }
        }
        assert!(Instant::now() < deadline, "sealed {} of {cards}", sealed.len());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut forks = 0;
    for w in &seats {
        forks += status(w).await.chain_sync.fork_decisions;
    }
    for w in &seats {
        w.__delay_inbound_ms(0);
    }
    let after = molt_engine::__resign_counts();
    let out = opened
        .iter()
        .map(|(id, _, slot)| {
            let (d, sealed_before) = sealed[id];
            let resigns = after.get(id).copied().unwrap_or(0) - resigns_before.get(id).copied().unwrap_or(0);
            Card { secs: d.as_secs_f64(), resigns, slot: *slot, sealed_before }
        })
        .collect();
    (out, forks)
}

fn pct(sorted: &[f64], p: usize) -> f64 {
    let i = (sorted.len() * p).div_ceil(100).saturating_sub(1).min(sorted.len() - 1);
    sorted[i]
}

/// n >= 200 per run, three runs per mode: the tail and its re-signs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: about two hours"]
async fn prev_binding_adds_no_wait_with_a_lagging_seat() {
    let _ = molt_engine::__resign_counts();
    for round in 0..3 {
        for unbound in [true, false] {
            let (cards, forks) = run(unbound, 200).await;
            let mut secs: Vec<f64> = cards.iter().map(|c| c.secs).collect();
            secs.sort_by(f64::total_cmp);
            let mut hist = std::collections::BTreeMap::<u64, usize>::new();
            for c in &cards {
                *hist.entry(c.resigns).or_default() += 1;
            }
            let out_of_order = cards.iter().filter(|c| c.slot != c.sealed_before).count();
            let mut slow: Vec<&Card> = cards.iter().collect();
            slow.sort_by(|a, b| b.secs.total_cmp(&a.secs));
            let tail: Vec<String> = slow.iter().take(5).map(|c| format!("{:.0}s/slot{}/resigns{}", c.secs, c.slot, c.resigns)).collect();
            eprintln!(
                "measure round={round} bound={} n={} median={:.2} p90={:.2} p95={:.2} p99={:.2} max={:.2} forks={forks} resigns_per_card={hist:?} out_of_slot_order={out_of_order} slowest={tail:?}",
                !unbound,
                secs.len(),
                pct(&secs, 50),
                pct(&secs, 90),
                pct(&secs, 95),
                pct(&secs, 99),
                pct(&secs, 100)
            );
        }
    }
    molt_engine::__prev_unbound(false);
}

/// The review's symmetric case: the two sealing seats (mara and walter
/// propose) each see the other 6 s late; dora is prompt. n = 100.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: about an hour"]
async fn prev_binding_with_two_seats_behind_each_other() {
    let _ = molt_engine::__resign_counts();
    for unbound in [false, true] {
        let (cards, forks) = run_with(unbound, 100, [6_000, 6_000, 0]).await;
        let mut secs: Vec<f64> = cards.iter().map(|c| c.secs).collect();
        secs.sort_by(f64::total_cmp);
        let mut hist = std::collections::BTreeMap::<u64, usize>::new();
        for c in &cards {
            *hist.entry(c.resigns).or_default() += 1;
        }
        eprintln!(
            "measure symmetric bound={} n={} median={:.2} p95={:.2} max={:.2} forks={forks} resigns_per_card={hist:?}",
            !unbound,
            secs.len(),
            pct(&secs, 50),
            pct(&secs, 95),
            pct(&secs, 100)
        );
    }
    molt_engine::__prev_unbound(false);
}
