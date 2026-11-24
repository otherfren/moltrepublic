// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The branch choice over a real relay** (`docs/chain/chain_rescue_plan.md`
//! §10): seats cut off from each other seal two branches at one height
//! (the shared seat signs both while it hears neither seal), the split is
//! repaired by a vote carried in bundle files, the left-behind seat stops
//! and rejoins by link, and a vote seals afterwards on one head.

mod common;

use std::time::Duration;

use common::{adopt_relay, read_session, wait_for};
use molt_core::{Command, ProposalState, Reply, StatusView};
use molt_engine::WalletHandle;

const PASS: &str = "correct horse battery staple";

fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
}

async fn relay() -> (nostr_relay_builder::LocalRelay, String) {
    use nostr_relay_builder::builder::{RateLimit, RelayBuilder};
    use nostr_relay_builder::prelude::{MemoryDatabase, MemoryDatabaseOptions};
    let db = std::sync::Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions { events: true, max_events: None }));
    let relay = nostr_relay_builder::LocalRelay::new(
        RelayBuilder::default().database(db).rate_limit(RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 }),
    );
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

fn page(path: &str, text: &str) -> serde_json::Value {
    let patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,1 @@\n+{text}\n"
    );
    serde_json::json!({ "op": "wiki_patch", "value": patch, "summary": "+1" })
}

/// A real republic of `names` (the first founds) at threshold `m`;
/// returns the seats in order and every joiner's phrase.
async fn found(root: &std::path::Path, url: &str, names: &[&str], m: u8) -> (Vec<WalletHandle>, Vec<String>) {
    let a = common::engine(&root.join(names[0]));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Club".to_string(),
        member: names[0].to_string(),
        members: u8::try_from(names.len()).expect("few"),
        threshold: m,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "seat links", |s| {
        s.create.seats.len() >= names.len() - 1
            && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();
    let mut joiners = Vec::new();
    for (name, link) in names[1..].iter().zip(links) {
        let w = common::engine(&root.join(name));
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: (*name).to_string(), relays: Vec::new() })
            .await
            .expect("join start");
        joiners.push(w);
    }
    let mut phrases = Vec::new();
    for w in &joiners {
        phrases.push(wait_for(w, "a phrase", |s| !s.join.seed.is_empty()).await.join.seed.clone());
    }
    wait_for(&a, "every join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose { name: "Club".to_string(), agenda: String::new(), features: vec!["memory".to_string()] })
        .await
        .expect("charter");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    for w in &joiners {
        wait_for(w, "the charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("backup");
    }
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("finish");
    for w in &joiners {
        wait_for(w, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "in", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()).await;
    }
    let mut seats = vec![a];
    seats.extend(joiners);
    (seats, phrases)
}

async fn status(w: &WalletHandle) -> StatusView {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn branches(w: &WalletHandle) -> molt_core::branch::BranchStatusView {
    match w.execute(Command::BranchStatus).await.expect("branch status") {
        Reply::Branches(v) => *v,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn kinds(w: &WalletHandle) -> Vec<(u64, String)> {
    let Reply::Chain { blocks, .. } = w.execute(Command::ReadChain).await.expect("chain") else {
        panic!("read_chain");
    };
    let mut out: Vec<(u64, String)> = blocks.iter().map(|b| (b.height, b.kind.clone())).collect();
    out.sort();
    out
}

async fn eventually<F, Fut>(what: &str, secs: u64, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !check().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn propose(w: &WalletHandle, path: &str) -> u64 {
    match w.execute(Command::Propose { surface: molt_core::Surface::Memory, payload: page(path, path) }).await.expect("propose") {
        Reply::Proposed { id, .. } => id.0,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn open_cards(w: &WalletHandle) -> Vec<u64> {
    let Reply::Proposals { proposals, .. } = w.execute(Command::LIST_ALL_PROPOSALS).await.expect("list") else {
        panic!("list");
    };
    proposals.into_iter().filter(|p| p.state == ProposalState::Proposed).map(|p| p.id.0).collect()
}

async fn head(w: &WalletHandle) -> String {
    status(w).await.chain_sync.head_hash
}

async fn export(w: &WalletHandle, dir: &std::path::Path, name: &str) -> String {
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    w.execute(Command::BranchExport { dest: path.display().to_string(), passphrase: PASS.to_string() }).await.expect("export");
    eventually("the bundle file", 60, || {
        let p = path.clone();
        async move { p.exists() }
    })
    .await;
    path.display().to_string()
}

async fn import(w: &WalletHandle, path: &str) {
    w.execute(Command::BranchImport { path: path.to_string(), passphrase: PASS.to_string() }).await.expect("import");
}

/// Split `left` and `right` at one height through `shared`: both sides
/// propose, the shared seats see both cards, then hear nobody while they
/// sign both; each side seals its own block. Returns the two card ids.
async fn split(left: &[&WalletHandle], right: &[&WalletHandle], shared: &[&WalletHandle], names: (&[&str], &[&str], &[&str]), tag: &str) -> (u64, u64) {
    for w in left {
        w.__cut_off(names.1);
    }
    for w in right {
        w.__cut_off(names.0);
    }
    let p1 = propose(left[0], &format!("{tag}-left.md")).await;
    let p2 = propose(right[0], &format!("{tag}-right.md")).await;
    for w in shared {
        eventually("both cards at the shared seat", 60, || async { let o = open_cards(w).await; o.contains(&p1) && o.contains(&p2) }).await;
    }
    for w in left.iter().skip(1) {
        eventually("the left card", 60, || async { open_cards(w).await.contains(&p1) }).await;
    }
    for w in right.iter().skip(1) {
        eventually("the right card", 60, || async { open_cards(w).await.contains(&p2) }).await;
    }
    let all: Vec<&str> = names.0.iter().chain(names.1).chain(names.2).copied().collect();
    for w in shared {
        w.__cut_off(&all);
    }
    for w in left.iter().skip(1) {
        w.execute(Command::Approve { proposal: molt_core::ProposalId(p1), note: None }).await.expect("left approves");
    }
    for w in right.iter().skip(1) {
        w.execute(Command::Approve { proposal: molt_core::ProposalId(p2), note: None }).await.expect("right approves");
    }
    for w in shared {
        w.execute(Command::Approve { proposal: molt_core::ProposalId(p1), note: None }).await.expect("shared: left");
        w.execute(Command::Approve { proposal: molt_core::ProposalId(p2), note: None }).await.expect("shared: right");
    }
    let applied = |w: &WalletHandle, id: u64| {
        let w = w.clone();
        async move { !open_cards(&w).await.contains(&id) }
    };
    eventually("the left seals", 240, || applied(left[0], p1)).await;
    eventually("the right seals", 240, || applied(right[0], p2)).await;
    (p1, p2)
}

/// The keystone, 2-of-3: mara and dora cut off from each other, walter
/// signs both; mara and walter keep mara's branch, dora stops and rejoins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_two_of_three_split_is_repaired_by_vote_and_link() {
    trace();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (seats, phrases) = found(tmp.path(), &url, &["mara", "walter", "dora"], 2).await;
    let [mara, walter, dora]: [WalletHandle; 3] = seats.try_into().unwrap_or_else(|_| panic!("three seats"));
    mara.__branch_choice_available(true);
    let (_, p2) = split(&[&mara], &[&dora], &[&walter], (&["mara"], &["dora"], &["walter"]), "s1").await;
    assert_ne!(head(&mara).await, head(&dora).await, "two branches");
    // walter comes back to mara only and follows her branch
    walter.__cut_off(&["dora"]);
    walter.__beacon_now();
    mara.__beacon_now();
    eventually("walter on mara's head", 180, || async { head(&walter).await == head(&mara).await }).await;
    // the evidence moves by file
    let from_dora = export(&dora, tmp.path(), "dora.bundle").await;
    import(&mara, &from_dora).await;
    eventually("mara proves the fork", 60, || async { !branches(&mara).await.forks.is_empty() }).await;
    let fork = branches(&mara).await.forks.remove(0);
    let tip = fork.branches.iter().find(|b| b.own).expect("own branch").head_hash.clone();
    let Reply::BranchProposed { id, digest, .. } = mara
        .execute(Command::ChooseBranch { base_height: fork.base_height, tip_hash: tip, amnesia_ack: false })
        .await
        .expect("choose")
    else {
        panic!("choose_branch answers BranchProposed");
    };
    mara.execute(Command::ApproveBranch { id, dropped_digest: digest.clone(), amnesia_ack: false }).await.expect("mara signs");
    let from_mara = export(&mara, tmp.path(), "mara.bundle").await;
    import(&walter, &from_mara).await;
    eventually("walter sees the card", 60, || async { branches(&walter).await.cards.iter().any(|c| c.id == id.0) }).await;
    eventually("walter holds the dropped branch", 60, || async {
        branches(&walter).await.cards.iter().any(|c| c.id == id.0 && !c.digest.is_empty())
    })
    .await;
    let seen = branches(&walter).await.cards.into_iter().find(|c| c.id == id.0).expect("card").digest;
    assert_eq!(seen, digest, "walter computes the same dropped list");
    walter.execute(Command::ApproveBranch { id, dropped_digest: seen, amnesia_ack: false }).await.expect("walter signs");
    for w in [&mara, &walter] {
        eventually("the choice sealed", 180, || async { kinds(w).await.last().is_some_and(|(_, k)| k == "branch") }).await;
    }
    assert_eq!(head(&mara).await, head(&walter).await);
    // dora takes the sealed choice in by file and stops
    let after = export(&mara, tmp.path(), "mara-after.bundle").await;
    import(&dora, &after).await;
    eventually("dora stops", 60, || async { status(&dora).await.chain_sync.abandoned.is_some() }).await;
    // dora rejoins with a link on a fresh device
    drop(dora);
    mara.__cut_off(&[]);
    walter.__cut_off(&[]);
    mara.__seat_active_secs(0);
    walter.__seat_active_secs(0);
    mara.execute(Command::RecoverInviteStart { member: "dora".to_string() }).await.expect("mint");
    let s = wait_for(&mara, "the link", |s| s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")).await;
    let link = s.notice.strip_prefix("recovery-link:").unwrap_or_else(|| panic!("mint: {:?}", s.notice)).to_string();
    let dora = common::engine(&tmp.path().join("dora-new"));
    adopt_relay(&dora, &url).await;
    dora.execute(Command::RecoverStart { link, phrase: phrases[1].clone(), relays: Vec::new() }).await.expect("recover");
    let s = common::wait_for_secs(&dora, "dora is back", 300, |s| {
        (s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()) || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "{:?}", s.notice);
    eventually("one head on all three", 240, || async {
        let h = head(&mara).await;
        head(&walter).await == h && head(&dora).await == h
    })
    .await;
    // a vote seals afterwards, on one head
    let p3 = propose(&mara, "after.md").await;
    eventually("walter sees the next card", 120, || async { open_cards(&walter).await.contains(&p3) }).await;
    walter.execute(Command::Approve { proposal: molt_core::ProposalId(p3), note: None }).await.expect("approve");
    eventually("the next vote seals everywhere", 240, || async {
        let h = head(&mara).await;
        !open_cards(&mara).await.contains(&p3) && head(&walter).await == h && head(&dora).await == h
    })
    .await;
    for w in [&mara, &walter, &dora] {
        assert!(status(w).await.chain_diverged.is_empty(), "nobody diverged");
    }
    // dora's dropped decision is a vote again where it was known
    assert!(open_cards(&walter).await.contains(&p2), "the dropped card is open on walter");
}

/// A seat left behind rejoins with a link minted by `by` on a fresh device.
async fn rejoin(by: &WalletHandle, voters: &[&WalletHandle], member: &str, phrase: &str, url: &str, dir: &std::path::Path) -> WalletHandle {
    by.execute(Command::RecoverInviteStart { member: member.to_string() }).await.expect("mint");
    let s = wait_for(by, "the link", |s| s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")).await;
    let link = s.notice.strip_prefix("recovery-link:").unwrap_or_else(|| panic!("mint: {:?}", s.notice)).to_string();
    let w = common::engine(dir);
    adopt_relay(&w, url).await;
    w.execute(Command::RecoverStart { link, phrase: phrase.to_string(), relays: Vec::new() }).await.expect("recover");
    // the restore card is a human vote: the other survivors approve it
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let s = read_session(&w).await;
        assert!(!s.notice.starts_with("recover-failed:"), "{:?}", s.notice);
        if s.screen == molt_core::Screen::Main && !s.workspaces.is_empty() {
            break;
        }
        for v in voters {
            if let Ok(Reply::Proposals { proposals, .. }) = v.execute(Command::LIST_ALL_PROPOSALS).await {
                let restore = |p: &molt_core::ProposalView| p.payload.get("op").and_then(serde_json::Value::as_str) == Some("restore_member");
                for p in proposals.into_iter().filter(|p| p.state == ProposalState::Proposed && !p.approved_by_me && restore(p)) {
                    let _ = v.execute(Command::Approve { proposal: p.id, note: None }).await;
                }
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "{member} never came back");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    w
}

/// 3-of-5: a and b against d and e, c signs both. a, b and c keep a's
/// branch; d and e stop and rejoin one after the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_three_of_five_split_is_repaired_by_vote_and_link() {
    trace();
    let (_relay, url) = relay().await;
    let tmp = tempfile::tempdir().expect("tmp");
    let (seats, phrases) = found(tmp.path(), &url, &["a", "b", "c", "d", "e"], 3).await;
    let [a, b, c, d, e]: [WalletHandle; 5] = seats.try_into().unwrap_or_else(|_| panic!("five seats"));
    a.__branch_choice_available(true);
    let (_, _) = split(&[&a, &b], &[&d, &e], &[&c], (&["a", "b"], &["d", "e"], &["c"]), "s1").await;
    assert_ne!(head(&a).await, head(&d).await, "two branches");
    c.__cut_off(&["d", "e"]);
    c.__beacon_now();
    a.__beacon_now();
    eventually("c on a's head", 180, || async { head(&c).await == head(&a).await }).await;
    let from_d = export(&d, tmp.path(), "d.bundle").await;
    import(&a, &from_d).await;
    eventually("a proves the fork", 60, || async { !branches(&a).await.forks.is_empty() }).await;
    let fork = branches(&a).await.forks.remove(0);
    let tip = fork.branches.iter().find(|br| br.own).expect("own").head_hash.clone();
    let Reply::BranchProposed { id, digest, .. } =
        a.execute(Command::ChooseBranch { base_height: fork.base_height, tip_hash: tip, amnesia_ack: false }).await.expect("choose")
    else {
        panic!("choose_branch answers BranchProposed");
    };
    a.execute(Command::ApproveBranch { id, dropped_digest: digest.clone(), amnesia_ack: false }).await.expect("a signs");
    let from_a = export(&a, tmp.path(), "a.bundle").await;
    for w in [&b, &c] {
        import(w, &from_a).await;
        eventually("the card with its dropped list", 60, || async {
            branches(w).await.cards.iter().any(|card| card.id == id.0 && card.digest == digest)
        })
        .await;
        w.execute(Command::ApproveBranch { id, dropped_digest: digest.clone(), amnesia_ack: false }).await.expect("signs");
    }
    for w in [&a, &b, &c] {
        eventually("the choice sealed", 180, || async { kinds(w).await.last().is_some_and(|(_, k)| k == "branch") }).await;
    }
    let after = export(&a, tmp.path(), "a-after.bundle").await;
    for w in [&d, &e] {
        import(w, &after).await;
        eventually("the right side stops", 60, || async { status(w).await.chain_sync.abandoned.is_some() }).await;
    }
    drop(d);
    drop(e);
    for w in [&a, &b, &c] {
        w.__cut_off(&[]);
        w.__seat_active_secs(0);
    }
    let d = rejoin(&a, &[&b, &c], "d", &phrases[2], &url, &tmp.path().join("d-new")).await;
    let e = rejoin(&a, &[&b, &c, &d], "e", &phrases[3], &url, &tmp.path().join("e-new")).await;
    let all = [&a, &b, &c, &d, &e];
    eventually("one head on all five", 300, || async {
        let h = head(&a).await;
        let mut same = true;
        for w in all {
            same &= head(w).await == h;
        }
        same
    })
    .await;
    let p3 = propose(&a, "after.md").await;
    for w in [&b, &d] {
        eventually("the next card", 120, || async { open_cards(w).await.contains(&p3) }).await;
        w.execute(Command::Approve { proposal: molt_core::ProposalId(p3), note: None }).await.expect("approve");
    }
    eventually("the next vote seals everywhere", 300, || async {
        let h = head(&a).await;
        let mut same = !open_cards(&a).await.contains(&p3);
        for w in all {
            same &= head(w).await == h;
        }
        same
    })
    .await;
}
