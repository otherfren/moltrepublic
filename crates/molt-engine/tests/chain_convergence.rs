// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **F00 keystone - concurrent writers end on ONE chain**
//! (`docs_archive/chain/chain_reorg.md`, `docs_archive/reviews/mcp_agent_friction_fixes.md`
//! A2): two seats of a real 2-of-2 republic propose in bursts over a real
//! relay and approve each other's cards as they appear. Whatever order the
//! seals happen in on either side, both seats end with identical blocks,
//! identical wikis and no divergence flag - the pacing keeps forks rare and
//! the deep tie-break heals the ones that happen.

mod common;

use common::{adopt_relay, engine, found_pair, read_session, wait_for_secs, wiki_docs};
use std::time::Duration;

use molt_core::{Command, ProposalState, Reply};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

fn add_page(path: &str, text: &str) -> serde_json::Value {
    let patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{text}\n"
    );
    serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "+1" })
}

/// Approve every open foreign card this seat sees; how many it approved.
async fn approve_open(w: &WalletHandle) -> usize {
    let Reply::Proposals { proposals, .. } = w.execute(Command::LIST_ALL_PROPOSALS).await.expect("list") else {
        panic!("list_proposals answers Reply::Proposals");
    };
    let mut n = 0;
    for p in proposals {
        if p.state == ProposalState::Proposed
            && !p.approved_by_me
            && w.execute(Command::Approve { proposal: p.id, note: None }).await.is_ok()
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
    let (a, b) = found_pair(&root, &[&url], "Konvergenz").await;

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

/// The engine runs on wall-clock time (presence, sealing grace), so a
/// paused tokio clock cannot drive it: every wait is a condition with a
/// ceiling sized for a loaded machine.
async fn wait_long(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&molt_core::SessionView) -> bool,
) -> Box<molt_core::SessionView> {
    wait_for_secs(w, what, 180, pred).await
}

/// A real 2-of-3 republic over `url`: mara founds, walter and dora join.
async fn found_trio(root: &std::path::Path, url: &str, lock: bool) -> [WalletHandle; 3] {
    let a = engine(&root.join("mara"));
    a.__lock_cap_off(!lock);
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
    let s = wait_long(&a, "two seat links", |s| {
        s.create.seats.len() >= 2
            && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();
    let mut joiners = Vec::new();
    for (name, link) in ["walter", "dora"].iter().zip(links) {
        let w = engine(&root.join(name));
        w.__lock_cap_off(!lock);
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: (*name).to_string(), relays: Vec::new() })
            .await
            .expect("join start");
        joiners.push(w);
    }
    wait_long(&a, "both joins", |s| s.create.can_propose).await;
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
        wait_long(w, "the charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    }
    wait_long(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    for w in &joiners {
        wait_long(w, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_long(w, "the joiner to enter", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            .await;
    }
    let mut it = joiners.into_iter();
    [a, it.next().expect("walter"), it.next().expect("dora")]
}

/// `(head hash, fork decisions)` as `w` reports them.
async fn sync_of(w: &WalletHandle) -> (String, u64) {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => (s.chain_sync.head_hash.clone(), s.chain_sync.fork_decisions),
        other => panic!("unexpected: {other:?}"),
    }
}

/// B6 (`docs/chain/rejoin_divergence_build_plan.md`): three seats of a
/// 2-of-3 republic propose in bursts and approve whatever they see, while
/// every tenth inbound frame is lost on first arrival. All three end on one
/// head hash with no restart, and
/// the per-height sealing line keeps fork decisions rare.
///
/// Fork decisions (sum over the three seats), measured 2026-11-21, five
/// runs each: per-proposal line (before B6) 19, 19, 13, 11, 15; per-height
/// line 0, 0, 0, 0, 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_seats_with_bursts_and_lost_frames_end_on_one_head() {
    let m = lossy_trio(false).await;
    assert!(m.forks <= FORKS_MAX, "fork decisions {} above {FORKS_MAX}", m.forks);
}

/// The same run with every seat on the lock (B7): no fork decision at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_seats_under_the_lock_with_bursts_and_lost_frames_never_fork() {
    let m = lossy_trio(true).await;
    assert_eq!(m.forks, 0, "no fork under the lock");
}

struct Measured {
    forks: u64,
}

/// Found a trio, run two bursts under 10 % inbound loss until one head;
/// prints fork decisions, duration and relay frames per sealed block.
async fn lossy_trio(lock: bool) -> Measured {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
    // a storing relay without the notes-per-minute limit: a burst of three
    // seats is not what this test measures
    use nostr_relay_builder::builder::{RateLimit, RelayBuilder};
    use nostr_relay_builder::prelude::{Filter, MemoryDatabase, MemoryDatabaseOptions, NostrDatabase};
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions { events: true, max_events: None }));
    let relay = nostr_relay_builder::LocalRelay::new(
        RelayBuilder::default()
            .database(db.clone())
            .rate_limit(RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 }),
    );
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let seats = found_trio(tmp.path(), &url, lock).await;
    for w in &seats {
        w.__drop_inbound_every(10);
    }
    let frames_before = db.count(Filter::new()).await.expect("count");
    let height_before = chain(&seats[0]).await.len();
    const BURSTS: usize = 2;
    const PAGES: usize = 3;
    let names = ["mara", "walter", "dora"];
    // a ceiling, not a pace: the loop leaves as soon as the heads agree
    let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
    let started = tokio::time::Instant::now();
    for burst in 0..BURSTS {
        for i in 0..PAGES {
            for (w, name) in seats.iter().zip(names) {
                w.execute(Command::Propose {
                    surface: molt_core::Surface::Memory,
                    payload: add_page(&format!("{name}/b{burst}p{i}.md"), name),
                })
                .await
                .expect("proposes");
            }
        }
        let want = (burst + 1) * PAGES * seats.len();
        loop {
            for w in &seats {
                approve_open(w).await;
            }
            let chains = [chain(&seats[0]).await, chain(&seats[1]).await, chain(&seats[2]).await];
            let applied = |c: &Vec<(u64, u64, Vec<String>)>| c.iter().filter(|(_, pid, _)| *pid != 0).count();
            let heads = [sync_of(&seats[0]).await.0, sync_of(&seats[1]).await.0, sync_of(&seats[2]).await.0];
            if chains.iter().all(|c| applied(c) == want) && heads[0] == heads[1] && heads[1] == heads[2] {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no common head:\nmara   {:?}\nwalter {:?}\ndora   {:?}",
                chains[0],
                chains[1],
                chains[2]
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let mut forks = 0;
    for w in &seats {
        forks += sync_of(w).await.1;
    }
    let dropped: u64 = seats.iter().map(WalletHandle::__dropped_inbound).sum();
    let frames = db.count(Filter::new()).await.expect("count") - frames_before;
    let blocks = chain(&seats[0]).await.len() - height_before;
    eprintln!(
        "lock={lock} fork_decisions={forks} dropped={dropped} secs={} frames={frames} blocks={blocks} frames_per_block={}.{}",
        started.elapsed().as_secs(),
        frames / blocks.max(1),
        frames * 10 / blocks.max(1) % 10
    );
    assert!(dropped > 0, "the network was lossy");
    Measured { forks }
}

/// Fork decisions the per-height line may take: a takeover after a lost
/// block can still fork, but far below the 11 the old line never undercut.
const FORKS_MAX: u64 = 3;
