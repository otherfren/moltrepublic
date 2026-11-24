// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The majority rule** (`chain_rescue_plan.md`): a new founding needs
//! 2m > n at every door; a republic founded before the rule keeps
//! running and verifying, and carries the notice.

use molt_core::{Command, MoltError, Reply, StatusView};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{adopt_relay, close_and_open, enable_vault, engine, found_n_at, read_session, wait_for, NAMES};

async fn status(w: &WalletHandle) -> StatusView {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => st,
        other => panic!("unexpected: {other:?}"),
    }
}

/// The founder (seam per `founder_seam`) and n-1 joiners (no seam) up to
/// `can_propose`; the engines founder first.
async fn joined(root: &std::path::Path, url: &str, m: u8, n: u8, founder_seam: bool) -> Vec<WalletHandle> {
    let a = engine(&root.join(NAMES[0]));
    adopt_relay(&a, url).await;
    a.__allow_minority_threshold(founder_seam);
    a.execute(Command::CreateStart {
        name: "R".to_string(),
        member: NAMES[0].to_string(),
        threshold: m,
        members: n,
        relays: Vec::new(),
    })
    .await
    .expect("founding starts");
    let s = wait_for(&a, "joinable seat links", |s| {
        s.create.seats.len() == usize::from(n) - 1
            && s.create.seats.iter().all(|seat| molt_engine::FoundingInvite::parse(&seat.link).is_ok())
    })
    .await;
    let links: Vec<String> = s.create.seats.iter().map(|seat| seat.link.clone()).collect();
    let mut all = vec![a];
    for (i, link) in links.into_iter().enumerate() {
        let w = engine(&root.join(NAMES[i + 1]));
        adopt_relay(&w, url).await;
        w.execute(Command::JoinStart { invite: link, member: NAMES[i + 1].to_string(), relays: Vec::new() })
            .await
            .expect("join starts");
        all.push(w);
    }
    wait_for(&all[0], "every join accepted", |s| s.create.can_propose).await;
    all
}

/// An older founder's 2-of-4 table: every member refuses to ratify it,
/// naming the smallest allowed m.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_refuse_to_ratify_a_threshold_that_is_no_majority() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = joined(tmp.path(), &url, 2, 4, true).await;
    all[0]
        .execute(Command::CreatePropose { name: "R".to_string(), agenda: String::new(), features: Vec::new() })
        .await
        .expect("the seam founder proposes");
    for w in &all[1..] {
        let s = wait_for(w, "the refusal", |s| s.join.run.outcome == 2).await;
        assert!(
            s.join.run.log.iter().any(|l| l.contains("needs a majority: at least 3 of 4")),
            "{:?}",
            s.join.run.log
        );
        assert!(!s.join.awaiting_ratify, "never asked to sign");
    }
}

/// A republic founded below majority (the seam builds the pre-rule
/// shape): it shows the notice, seals a block (a 2-of-4 vault enable,
/// which it keeps), and every seat's chain verifies after a reopen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_republic_below_majority_keeps_running_and_shows_the_notice() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["memory"]).await;
    for w in &all {
        let st = status(w).await;
        assert!(st.chain_governed && st.below_majority);
    }
    enable_vault(&all).await;
    let id = read_session(&all[0]).await.active_workspace.clone();
    for (name, ws) in close_and_open(tmp.path(), &all).await {
        let (_, chain) = ws.read_chain().expect("chain");
        assert!(chain.len() >= 2, "{name}: the vote sealed a block");
        molt_engine::verify_chain(&chain).expect("a 2-of-4 chain verifies");
    }
    all[0].execute(Command::OpenWorkspace { id }).await.expect("reopens");
    let st = status(&all[0]).await;
    assert!(st.below_majority && st.features.iter().any(|f| f == "vault"));
}

/// 3-of-4 with the vault cannot be founded and says why; 3-of-5 with the
/// vault founds, a majority without the notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_vault_needs_five_members_under_the_majority_rule() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let four = joined(&tmp.path().join("four"), &url, 3, 4, false).await;
    let err = four[0]
        .execute(Command::CreatePropose { name: "R".to_string(), agenda: String::new(), features: vec!["vault".into()] })
        .await
        .expect_err("no vault at n = 4");
    assert!(matches!(err, MoltError::Vault(molt_core::vault::VaultRefusal::FewMembers)), "{err:?}");
    assert_eq!(err.to_string(), "vault: needs 5 or more members");
    four[0].execute(Command::CreateCancel).await.expect("cancel");

    let five = found_n_at(&tmp.path().join("five"), &url, 5, 3, &["vault"]).await;
    for w in &five {
        let st = status(w).await;
        assert!(!st.below_majority);
        assert!(st.features.iter().any(|f| f == "vault"), "{:?}", st.features);
    }
}
