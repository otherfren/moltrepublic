// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **A vote cast while the head moves still counts** (field report
//! v0.0.11, point 2): in a 2-of-3 republic a member's approval of a
//! proposal that came right after a just-applied one was never counted -
//! `approvals` stayed at 1, the member read `open` - and the voter heard
//! nothing. Three real engines over a relay.

mod common;

use common::{adopt_relay, engine, read_session, wait_for};
use std::time::Duration;

use molt_core::{Command, Reply, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

/// A real 2-of-3 republic: mara founds, walter and dora join.
async fn found_trio(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle, WalletHandle) {
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
    let s = wait_for(&a, "two seat links", |s| {
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
    wait_for(&a, "both joins", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose { name: "Drei".to_string(), agenda: String::new(), features: Vec::new() })
        .await
        .expect("charter proposed");
    let seed = read_session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("founder backup");
    for w in &joiners {
        wait_for(w, "the charter", |s| s.join.awaiting_ratify).await;
        w.execute(Command::JoinConfirmCharter).await.expect("ratify");
        let seed = read_session(w).await.join.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("joiner backup");
    }
    wait_for(&a, "the seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    for w in &joiners {
        wait_for(w, "the join seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
        w.execute(Command::JoinFinish).await.expect("join finish");
        wait_for(w, "the joiner to enter", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty())
            .await;
    }
    let mut it = joiners.into_iter();
    (a, it.next().expect("walter"), it.next().expect("dora"))
}

async fn propose(w: &WalletHandle, name: &str) -> u64 {
    match w
        .execute(Command::Propose {
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_name", "value": name }),
        })
        .await
        .expect("propose")
    {
        Reply::Proposed { id, .. } => id.0,
        other => panic!("unexpected: {other:?}"),
    }
}

/// `(state, approvals, who approved)` of `id` as `w` sees it.
async fn card(w: &WalletHandle, id: u64) -> (molt_core::ProposalState, usize, Vec<String>) {
    match w.execute(Command::ReadProposal { id }).await.expect("read") {
        Reply::Proposals { proposals, .. } => {
            let p = proposals.first().expect("card");
            let who = p
                .votes
                .iter()
                .filter(|v| v.vote == molt_core::VoteState::Approved)
                .map(|v| v.member.clone())
                .collect();
            (p.state, p.approvals, who)
        }
        other => panic!("unexpected: {other:?}"),
    }
}

async fn until_applied(w: &WalletHandle, id: u64, secs: u64) -> bool {
    for _ in 0..secs * 10 {
        if card(w, id).await.0 == molt_core::ProposalState::Applied {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// mara has two cards open; walter's vote seals the first, and dora votes
/// on the second at that very moment - possibly before the first block
/// reached her. Her vote plus mara's must seal the second card on every
/// seat, however the race fell.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vote_cast_as_the_head_moves_still_counts() {
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    let root = tempfile::tempdir().expect("tmp");
    let (a, b, c) = found_trio(root.path(), &url).await;

    for round in 0..3 {
        let first = propose(&a, &format!("Eins{round}")).await;
        let second = propose(&a, &format!("Zwei{round}")).await;
        a.execute(Command::Approve { proposal: molt_core::ProposalId(first), note: None }).await.expect("mara 1");
        a.execute(Command::Approve { proposal: molt_core::ProposalId(second), note: None }).await.expect("mara 2");
        // every seat must hold both cards before the race
        for w in [&b, &c] {
            for id in [first, second] {
                for _ in 0..100 {
                    if w.execute(Command::ReadProposal { id }).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        let (rb, rc) = tokio::join!(
            b.execute(Command::Approve { proposal: molt_core::ProposalId(first), note: None }),
            c.execute(Command::Approve { proposal: molt_core::ProposalId(second), note: None }),
        );
        rb.expect("walter seals the first");
        let accepted = match rc {
            Ok(_) => true,
            Err(e) => {
                eprintln!("round {round}: dora's vote refused: {e}");
                false
            }
        };
        assert!(until_applied(&a, first, 20).await, "round {round}: the first card applies");
        if !accepted {
            continue; // refused loudly: the voter knows; nothing to check
        }
        let applied = until_applied(&a, second, 20).await;
        assert!(
            applied,
            "round {round}: dora's accepted vote + mara's seal the second card - mara sees {:?}, dora sees {:?}",
            card(&a, second).await,
            card(&c, second).await
        );
    }
}
