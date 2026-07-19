// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The 2026-07-19 restart incident, reproduced over a real SMP server.**
//!
//! A live 3-node republic was restarted (two clean closes, one hard kill);
//! afterwards only ONE of the six chat directions still delivered, while
//! every surface said healthy. The stderr evidence pinned the cause:
//! `SKEY rejected: ERR AUTH` — the reopened node re-secured peer queues
//! with a FRESH sender key because its persisted credentials lacked the
//! old one (the mesh-up creds export happens before the first send ever
//! mints a sender key; only a clean close after traffic captured them).
//!
//! This test founds a real 2-of-3 republic over SMP between three engine
//! instances, proves the full 6-direction chat matrix, restarts all three
//! the way the incident did (A and B close cleanly, C is hard-killed),
//! reopens them as fresh engines through the REAL reopen path
//! (`reopen_transport` + `import_creds` — no loopback seam), and asserts
//! the full matrix again. Before the sender-seed fix the C→A and C→B legs
//! die exactly like the incident (fresh SKEY → ERR AUTH → endless backoff).
//!
//! `#[ignore]` (real network, throwaway queues on the public server):
//! `cargo test -p molt-engine --test mesh_restart_over_smp -- --ignored --nocapture`

mod common;

use std::path::Path;
use std::time::Duration;

use common::read_chat;
use molt_core::{Command, Reply, SessionSettings, SessionView};
use molt_engine::{FoundingInvite, WalletHandle};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

fn session_for(root: &Path) -> SessionView {
    SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            smp_server: "custom".to_string(),
            smp_url: KONKIN.to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    }
}

async fn chat(w: &WalletHandle, body: &str) {
    w.execute(Command::Chat {
        body: body.to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("chat send");
}

/// Poll every node until it holds every OTHER sender's body — the full
/// send/receive matrix. On timeout, panic with a per-direction report
/// (the incident's evidence shape: exactly which legs are dead).
async fn assert_matrix(nodes: &[(&str, &WalletHandle)], sent: &[(&str, &str)], secs: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let mut missing: Vec<String> = Vec::new();
        for (name, w) in nodes {
            let chat = read_chat(w).await;
            for (sender, body) in sent {
                if sender == name {
                    continue; // own message is local
                }
                if !chat.iter().any(|m| m["body"] == serde_json::json!(*body)) {
                    missing.push(format!("{sender} → {name}"));
                }
            }
        }
        if missing.is_empty() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("dead directions after {secs}s: {missing:?}");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Wait until the workspace LOCK under `root` is free again (the dropped
/// engine's writer thread released it).
async fn await_lock_release(root: &Path, id: &str) {
    let dir = molt_storage::find_workspace_dir(root, id).expect("workspace dir");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match molt_storage::open_workspace(&dir) {
            Ok(_) => break, // lock free; the probe guard drops right here
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the killed engine never released the workspace lock"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Drive the real engine `JoinStart` lifecycle from the pasted link until the
/// joiner has entered its own workspace. Returns the joiner's workspace id.
async fn engine_join(w: &WalletHandle, link: &str, member: &str) -> String {
    w.execute(Command::JoinStart {
        invite: link.to_string(),
        member: member.to_string(),
    })
    .await
    .expect("join start");
    // ratify the deliberated charter once the founder proposed it
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let s = read_session(w).await;
        if s.join.awaiting_ratify {
            break;
        }
        assert_ne!(s.join.run.outcome, 2, "{member} join failed: {:?}", s.join.run.log);
        assert!(
            tokio::time::Instant::now() < deadline,
            "{member} never reached ratification: {:?}",
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    w.execute(Command::JoinConfirmCharter).await.expect("ratify");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let s = read_session(w).await;
        if !s.active_workspace.is_empty() {
            return s.active_workspace.clone();
        }
        assert_ne!(s.join.run.outcome, 2, "{member} join failed: {:?}", s.join.run.log);
        assert!(
            tokio::time::Instant::now() < deadline,
            "{member} join did not complete: {:?}",
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "founds a 3-node republic over the real smp.konkin.io and restarts it"]
async fn restart_matrix_all_six_directions_deliver_after_mixed_restarts() {
    // surface the engine's/net's tracing (WARNs carry the failure reasons of
    // off-actor tasks — without this they vanish); RUST_LOG overrides
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "molt_engine=debug,molt_net=debug".into()),
        )
        .with_test_writer()
        .try_init();
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let root_b = tmp.path().join("member-b");
    let root_c = tmp.path().join("member-c");

    // --- found the 2-of-3 republic over real SMP -------------------------
    // production parity: founder AND joiners run the post-founding mesh
    // bootstrap (the incident's nodes ran spawn_with_config, bootstrap on)
    let (a, _rx) = molt_engine::__spawn_manual_founding_over_smp_bootstrap(
        molt_core::GroupConfig::demo(),
        session_for(&root_a),
    );
    a.execute(Command::CreateStart {
        name: "Restart Trio".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 3,
    })
    .await
    .expect("create start");
    // both real invite links
    let links = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let s = read_session(&a).await;
            let ready: Vec<String> = s
                .create
                .seats
                .iter()
                .filter(|seat| FoundingInvite::parse(&seat.link).is_some())
                .map(|seat| seat.link.clone())
                .collect();
            if ready.len() >= 2 {
                break ready;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "A never published two real invite links"
            );
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    };

    let b = molt_engine::__spawn_with_storage_bootstrap(molt_core::GroupConfig::demo(), session_for(&root_b));
    let c = molt_engine::__spawn_with_storage_bootstrap(molt_core::GroupConfig::demo(), session_for(&root_c));
    let b_join = engine_join(&b, &links[0], "member-b");
    let c_join = engine_join(&c, &links[1], "member-c");
    // joins run concurrently; the founder proposes once both seats are filled
    let propose = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            if read_session(&a).await.create.can_propose {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the members never joined in time"
            );
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        a.execute(Command::CreatePropose {
            name: "Restart Trio".to_string(),
            agenda: "survive the restart".to_string(),
        })
        .await
        .expect("propose charter");
    };
    let (b_id, c_id, ()) = tokio::join!(b_join, c_join, propose);

    // the founder's direct mesh comes up, then it enters the republic
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let s = read_session(&a).await;
        assert_ne!(s.create.run.outcome, 2, "ritual failed: {:?}", s.create.run.log);
        if s.create.run.outcome == 1
            && s.create.run.log.iter().any(|l| l.contains("direct mesh established"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder never bootstrapped its mesh; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    a.execute(Command::CreateFinish).await.expect("enter");
    let a_id = read_session(&a).await.active_workspace.clone();

    // --- baseline: the 6-direction matrix works before any restart -------
    chat(&a, "pre-a").await;
    chat(&b, "pre-b").await;
    chat(&c, "pre-c").await;
    assert_matrix(
        &[("founder-a", &a), ("member-b", &b), ("member-c", &c)],
        &[("founder-a", "pre-a"), ("member-b", "pre-b"), ("member-c", "pre-c")],
        60,
    )
    .await;
    println!("baseline matrix OK — restarting (A/B clean, C hard-killed)");

    // --- restart: A and B close cleanly, C is HARD-killed ----------------
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
    drop(a);
    drop(b);
    drop(c); // hard kill: no CloseWorkspace, no clean-close merge
    await_lock_release(&root_a, &a_id).await;
    await_lock_release(&root_b, &b_id).await;
    await_lock_release(&root_c, &c_id).await;

    // --- reopen all three as fresh engines through the REAL reopen path --
    let a2 = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session_for(&root_a));
    let b2 = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session_for(&root_b));
    let c2 = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session_for(&root_c));
    a2.execute(Command::OpenWorkspace { id: a_id.clone() })
        .await
        .expect("reopen a");
    b2.execute(Command::OpenWorkspace { id: b_id.clone() })
        .await
        .expect("reopen b");
    c2.execute(Command::OpenWorkspace { id: c_id.clone() })
        .await
        .expect("reopen c");
    for (name, w) in [("a2", &a2), ("b2", &b2), ("c2", &c2)] {
        let s = read_session(w).await;
        assert_eq!(s.notice, "", "{name} must not reopen detached");
    }

    // --- sends immediately after reopen (the incident's timing) ----------
    // The clean closers (A, B) persisted their advanced MLS ratchet on
    // close, so their first post-restart message is guaranteed. The
    // hard-killed C resumed from the last-persisted (mesh-up) ratchet: its
    // FIRST message may re-use an already-consumed sender generation and be
    // replay-rejected at the peers — the documented hard-kill window (the
    // per-drain MLS persist is the known-open hardening). The CONTRACT this
    // test pins is the incident's actual failure mode: the leg must HEAL —
    // C's next message must deliver everywhere (before the sender-seed fix
    // the leg stayed dead FOREVER: fresh SKEY → ERR AUTH → endless backoff).
    chat(&a2, "post-a").await;
    chat(&b2, "post-b").await;
    chat(&c2, "post-c").await; // may fall into the replay window — not asserted
    chat(&c2, "post-c-2").await;
    assert_matrix(
        &[("founder-a", &a2), ("member-b", &b2), ("member-c", &c2)],
        &[("founder-a", "post-a"), ("member-b", "post-b"), ("member-c", "post-c-2")],
        120,
    )
    .await;
    println!("OK: all six directions deliver after two clean closes + one hard kill");
}
