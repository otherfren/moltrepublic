// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! A **2-of-3 republic**: one founder engine + two genuinely separate
//! members found over the loopback transport (the same code path SMP runs),
//! then real threshold governance proves m-of-n with m < n — the founder's
//! self-cosign plus ONE member signature seals an Organization block while
//! the third seat never votes. The applied `set_name` is REAL: the status
//! view, the session entry and the plaintext manifest all take the new name.

mod common;

use std::time::Duration;

use molt_core::{Command, EventEnvelope, Reply, SessionSettings, SessionView, Surface, WorkspaceEvent};
use molt_engine::WalletHandle;
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{EngineSink, MlsChannel, MlsMember, NetError, PeerLink};
use tokio::sync::watch;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Sink for a member supervisor whose deliveries the test only observes.
#[derive(Clone, Default)]
struct NullSink;
impl EngineSink for NullSink {
    async fn deliver(&self, _f: &molt_core::MemberId, _e: EventEnvelope) -> Result<(), NetError> {
        Ok(())
    }
    async fn peer_seen(&self, _m: &molt_core::MemberId) {}
    async fn send_failed(&self, _m: &molt_core::MemberId, _r: &str) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_2_of_3_republic_founds_and_applies_a_rename_at_threshold() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("founder");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) = molt_engine::__spawn_manual_founding_bootstrap(
        molt_core::GroupConfig::demo(),
        session_a,
    );
    a.execute(Command::CreateStart {
        name: "Drei Gilden".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let materials = tokio::task::spawn_blocking(move || {
        material_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A hands out two seats of invite material")
    })
    .await
    .expect("join blocking");
    let mut seats = materials.into_iter();
    let seat_b = seats.next().expect("seat b");
    let seat_c = seats.next().expect("seat c");
    // every clone shares the ONE in-process loopback hub — the transport
    // both member runtimes ride after the seal
    let hub = seat_b.transport.clone();

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_phrase_for_sig = b_phrase.clone();
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat_b, "member-b".to_string(), b_phrase, true, true, None, None)
            .await
            .expect("B completes the member side")
    });
    let c_phrase = molt_storage::generate_seed_phrase().expect("c phrase");
    let c_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat_c, "member-c".to_string(), c_phrase, true, true, None, None)
            .await
            .expect("C completes the member side")
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the members never joined in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Drei Gilden".to_string(),
        agenda: "zu dritt regieren".to_string(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("propose charter");
    // ❻½: the founder's phrase-backup confirmation (n-of-n gate)
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
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
            "the founder never sealed + bootstrapped; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let b_outcome = b_task.await.expect("B task");
    let c_outcome = c_task.await.expect("C task");
    a.execute(Command::CreateFinish).await.expect("enter");

    // a REAL 2-of-3: the status view carries the sealed rule and roster
    match a.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => {
            assert_eq!(st.threshold, 2);
            assert_eq!(st.members.len(), 3);
            assert_eq!(st.name, "Drei Gilden", "the founding name is effective");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // both members' runtimes come up on the shared transport (C stays a
    // silent bystander — 2-of-3 must commit WITHOUT its vote)
    let sealed = b_outcome.sealed.expect("B holds the sealed roster");
    let b_links: Vec<PeerLink> = b_outcome
        .mesh
        .expect("B mesh")
        .iter()
        .filter_map(PeerLink::from_mesh)
        .collect();
    let b_group = MlsMember::restore(&b_outcome.mls_snapshot.expect("B mls")).expect("restore B");
    let b_feed = MemLog::new();
    let (b_wake, b_wake_rx) = watch::channel(0u64);
    let _b_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), b_links, 7),
        b_feed.clone(),
        MemStateStore::new(),
        NullSink,
        b_wake_rx,
        Some(MlsChannel::new(b_group)),
    );
    let c_links: Vec<PeerLink> = c_outcome
        .mesh
        .expect("C mesh")
        .iter()
        .filter_map(PeerLink::from_mesh)
        .collect();
    let c_group = MlsMember::restore(&c_outcome.mls_snapshot.expect("C mls")).expect("restore C");
    let (_c_wake, c_wake_rx) = watch::channel(0u64);
    let _c_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-c".to_string(), c_links, 8),
        MemLog::new(),
        MemStateStore::new(),
        NullSink,
        c_wake_rx,
        Some(MlsChannel::new(c_group)),
    );

    // --- the founder proposes the rename; its self-cosign is 1 of 2 ---
    let payload = serde_json::json!({
        "op": "set_name",
        "title": "Namen ändern",
        "value": "Umbenannte Gilden",
    });
    let pid = match a
        .execute(Command::Propose {
            surface: Surface::Organization,
            payload: payload.clone(),
        })
        .await
        .expect("propose rename")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        common::read_applied(&a, Surface::Organization).await.is_empty(),
        "one signature must not commit a 2-of-3 change"
    );

    // --- member-b co-signs the SAME change with its own anchored key ---
    let b_entropy = molt_storage::seed_entropy(&b_phrase_for_sig).expect("b entropy");
    let b_ws = molt_storage::derive_workspace_id(&b_entropy, "member");
    let (b_sk, _b_pk) = molt_storage::derive_identity_key(&b_entropy, &b_ws);
    let change = molt_core::ChainChange::Applied {
        proposal_id: pid.0,
        surface: Surface::Organization,
        payload: payload.clone(),
    };
    let bytes = molt_core::approval_bytes(&sealed.republic_id, 1, &change);
    let b_sig = molt_storage::identity_sign(&b_sk, &bytes);
    b_feed.push(EventEnvelope { prev_seq: 0,
        seq: 1,
        ts: now(),
        by: "member-b".to_string(),
        body: WorkspaceEvent::Approved {
            id: pid,
            by: "member-b".to_string(),
            height: 1,
            sig: b_sig,
        },
    });
    let _ = b_wake.send(1);

    // --- 2 of 3 commits: the rename becomes REAL on the founder ---
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match a.execute(Command::Status).await.expect("status") {
            Reply::Status(st) => {
                if st.name == "Umbenannte Gilden" {
                    break;
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the 2-of-3 rename never applied"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // …in the session entry (header + Open list)…
    let s = read_session(&a).await;
    let id = s.active_workspace.clone();
    let entry = s.workspaces.iter().find(|w| w.id == id).expect("entry");
    assert_eq!(entry.name, "Umbenannte Gilden");
    // …and on disk, in the plaintext manifest (async writer → poll)
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("dir");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if molt_storage::read_manifest(&dir)
            .is_ok_and(|m| m.workspace.name == "Umbenannte Gilden")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "manifest.toml never took the applied name"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
