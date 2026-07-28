// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Hard-kill resilience of the runtime mesh** (the 2026-07-19 incident).
//!
//! Queue receive-credentials used to be persisted ONLY by the clean-close
//! path, so a hard kill (Ctrl+C with the workspace open) left a
//! `transport.state` without `smp_queues` — and the next open silently fell
//! through to no transport at all while `net_health` still said Ok.
//!
//! Pinned here:
//! 1. The moment the real mesh comes up (founder's `NetMeshReady`), the
//!    on-disk `transport.state` already carries the queue credentials —
//!    WITHOUT any close having happened.
//! 2. A hard-killed engine (dropped, never `CloseWorkspace`d) reopens into a
//!    WORKING mesh: a fresh engine on the same directory resumes and chats
//!    both directions with the surviving peer.
//! 3. A workspace whose `transport.state` has MLS state but no usable queue
//!    credentials opens HONESTLY offline: `net_health` is Down with a
//!    reason and the persistent `"detached"` notice is set — never a silent
//!    ok — while local (offline-first) chat keeps working.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{await_chat_len, read_chat};
use molt_core::{
    Command, EventEnvelope, MemberId, MeshLink, NetHealth, Reply, SessionSettings, SessionView,
    WorkspaceEvent,
};
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

/// Records what the member-side supervisor delivers.
#[derive(Clone, Default)]
struct RecordSink {
    got: std::sync::Arc<std::sync::Mutex<Vec<(MemberId, EventEnvelope)>>>,
}
impl RecordSink {
    fn messages(&self) -> Vec<(MemberId, EventEnvelope)> {
        self.got.lock().expect("lock").clone()
    }
}
impl EngineSink for RecordSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.got.lock().expect("lock").push((from.clone(), env));
        Ok(())
    }
    async fn peer_seen(&self, _m: &MemberId) {}
    async fn send_failed(&self, _m: &MemberId, _r: &str) {}
}

/// Run a real 2-of-2 founding + mesh bootstrap over the loopback hub:
/// founder engine on `root_a`, genuine member via `run_ritual_member`.
/// Returns the founder handle, the shared hub transport, the member's
/// assembled mesh + post-bootstrap MLS snapshot, and the workspace id.
async fn found_with_mesh(
    root_a: &Path,
) -> (
    WalletHandle,
    molt_engine::RitualTransport,
    Vec<MeshLink>,
    Vec<u8>,
    String,
) {
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding_bootstrap(molt_core::GroupConfig::demo(), session_a);
    a.execute(Command::CreateStart {
        name: "Phoenix".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
    })
    .await
    .expect("create start");
    let materials = tokio::task::spawn_blocking(move || {
        material_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A hands out the invite material")
    })
    .await
    .expect("join blocking");
    let seat = materials.into_iter().next().expect("seat material");
    let hub = seat.transport.clone();

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(
            seat,
            "member-b".to_string(),
            b_phrase,
            true,
            true,
            None,
            None,
        )
        .await
        .expect("B completes the member side + bootstrap")
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "member-b never joined");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Phoenix".to_string(),
        agenda: "survive a hard kill".to_string(),
    })
    .await
    .expect("founder proposes the charter");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(&a).await;
        assert_ne!(s.create.run.outcome, 2, "ritual must not fail: {:?}", s.create.run.log);
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
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let b_outcome = b_task.await.expect("B task");
    let member_mesh = b_outcome.mesh.expect("B assembled its direct mesh");
    let member_mls = b_outcome.mls_snapshot.expect("member post-bootstrap snapshot");
    let id = read_session(&a).await.active_workspace.clone();
    assert!(!id.is_empty(), "the founded workspace is active");
    (a, hub, member_mesh, member_mls, id)
}

/// Hard-kill the engine (drop, never `CloseWorkspace`) and wait until its
/// writer thread released the workspace LOCK. Returns the workspace dir.
async fn hard_kill(a: WalletHandle, root: &Path, id: &str) -> PathBuf {
    drop(a);
    let dir = molt_storage::find_workspace_dir(root, id).expect("workspace dir");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match molt_storage::open_workspace(&dir) {
            Ok(_) => break, // lock free again; the guard drops right here
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the hard-killed engine never released the workspace lock"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    dir
}

/// (1) The mesh-up persist: once the direct mesh is established, the queue
/// credentials are ALREADY on disk — no clean close ever happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mesh_up_persists_queue_creds_without_any_close() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (a, _hub, _mesh, _mls, id) = found_with_mesh(&root_a).await;

    // HARD kill — no CloseWorkspace, no clean-close merge
    let dir = hard_kill(a, &root_a, &id).await;

    let (ws, _loaded) = molt_storage::open_workspace(&dir).expect("open raw");
    let ts = ws.read_transport_state();
    assert!(ts.mls.is_some(), "the post-bootstrap MLS snapshot is on disk");
    assert_eq!(ts.mesh.len(), 1, "the assembled mesh link is on disk");
    assert!(
        ts.smp_queues.as_ref().is_some_and(|c| !c.is_empty()),
        "the queue credentials are on disk WITHOUT a clean close — a hard \
         kill after mesh-up must be survivable"
    );
}

/// (2) The keystone: hard-killed founder, fresh engine on the same dir,
/// reopen resumes a WORKING mesh — chat crosses both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hard_killed_founder_resumes_the_mesh_on_reopen() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (a, hub, member_mesh, member_mls, id) = found_with_mesh(&root_a).await;
    a.execute(Command::CreateFinish).await.expect("enter");

    // Settle one debounced live-ratchet beat (MLS_PERSIST_SECS = 10, riding
    // the 1 s delivery tick): the founding fan-out ENCRYPTS after mesh-up,
    // and a kill before the next persist regressed the resumed ratchet by
    // those generations — the member (ack-less here, the mixed-version pin)
    // then replay-rejected the post-resume chat under load. That
    // seconds-after-mesh-up kill edge is covered WITH the production heal
    // (acks + rewind-resend) by delivery_guarantee.rs's hard-kill keystone;
    // this test pins the resume itself, deterministically.
    tokio::time::sleep(Duration::from_secs(11)).await;

    // HARD kill the founder (the loopback hub — standing in for the SMP
    // server — survives in `hub`, exactly like a real server would)
    hard_kill(a, &root_a, &id).await;

    // a fresh engine "process" on the same directory; the reopen seam hands
    // it a transport on the still-running hub (what a fresh SmpTransport +
    // import_creds does against a real server)
    let session_a2 = SessionView {
        workspaces: molt_storage::scan_workspaces(&root_a)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let a2 = molt_engine::__spawn_with_reopen_transport(
        molt_core::GroupConfig::demo(),
        session_a2,
        hub.clone(),
    );
    a2.execute(Command::OpenWorkspace { id: id.clone() })
        .await
        .expect("reopen after hard kill");
    // Stage B honest open: the reopen must be honest (not Down, not "detached"),
    // but verify-at-open means the inbound leg cannot read the honest Ok YET — no
    // real frame has been heard over it (the member's supervisor is not even up),
    // so it stays amber "verifying". Ok is asserted below, once the member's reply
    // has actually crossed the resumed leg and confirmed it.
    {
        let sv = read_session(&a2).await;
        assert_eq!(sv.notice, "", "no detached notice — the mesh resumed");
        assert!(
            !matches!(sv.net_health, NetHealth::Down { .. }),
            "the resumed mesh must not be Down: {:?}",
            sv.net_health
        );
    }

    // the surviving member's runtime supervisor (kept alive across A's kill)
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let member_group = MlsMember::restore(&member_mls).expect("restore member MLS");
    let member_feed = MemLog::new();
    let member_sink = RecordSink::default();
    let (member_wake, member_wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 11),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        member_wake_rx,
        Some(MlsChannel::new(member_group)),
    );

    // founder → member across the resumed mesh
    a2.execute(Command::Chat {
        body: "back from the dead".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("chat after resume");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let got = member_sink.messages();
        if got.iter().any(|(from, env)| {
            from == "founder-a"
                && matches!(&env.body, WorkspaceEvent::Chat(m) if m.body == "back from the dead")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resumed founder's chat never reached the member; got {:?}",
            got.iter().map(|(f, _)| f).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // member → founder across the resumed mesh
    member_feed.push(common::chat_env(2, "member-b", "good to have you back"));
    let _ = member_wake.send(2);
    let chat = await_chat_len(&a2, 2, 15).await;
    assert!(
        chat.iter().any(|m| m["body"] == serde_json::json!("good to have you back")
            && m["from"] == serde_json::json!("member-b")),
        "the member's chat reached the resumed founder: {chat:?}"
    );

    // verify-at-open honesty: a real frame HAS now been heard over the resumed
    // inbound leg (the member's reply), so net_health clears "verifying" to the
    // honest Ok — the leg is confirmed by actual delivery, not just a live SUB.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let sv = read_session(&a2).await;
        if sv.net_health == NetHealth::Ok {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "net_health never reached the honest Ok after a frame was heard: {:?}",
            sv.net_health
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn genesis(member: &str) -> EventEnvelope {
    EventEnvelope {
        seq: 1,
        ts: 1_751_000_000,
        by: member.to_string(),
        body: WorkspaceEvent::Founded {
            name: "Orphaned".to_string(),
            rule_m: 2,
            rule_n: 2,
            member: member.to_string(),
            roster: vec!["ada".to_string(), "ben".to_string()],
            identities: Vec::new(),
            attestations: Vec::new(),
            republic_id: String::new(),
            agenda: String::new(),
        },
    }
}

/// (3) The honest offline state: MLS + mesh persisted, queue creds MISSING
/// (the incident's on-disk shape after a hard kill on a pre-fix build).
/// The open must say Down + "detached", not a silent healthy-looking no-op —
/// while local chat still appends (offline-first).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_with_mls_but_no_creds_is_honestly_offline() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("node-b");
    let seed = molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("gen"))
        .expect("entropy");
    let ws = molt_storage::create_workspace(&root, &seed, &genesis("ben")).expect("create");
    let id = ws.manifest.workspace.id.clone();
    // exactly what the founding-time write left behind: group + mesh, NO creds
    ws.write_transport_state(&molt_core::TransportState {
        mls: Some(b"opaque-mls-snapshot".to_vec()),
        mesh: vec![MeshLink {
            member: "ada".to_string(),
            snd_server: "smp://AAAA@host.example".to_string(),
            snd_queue: "aa".to_string(),
            snd_wrap: "bb".to_string(),
            rcv_queue: "cc".to_string(),
            rcv_wrap: "dd".to_string(),
            rcv_server: String::new(),
            snd_extra: Vec::new(),
            rcv_extra: Vec::new(),
        }],
        ..molt_core::TransportState::default()
    })
    .expect("write transport.state");
    drop(ws); // release the LOCK

    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(&root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session);
    w.execute(Command::OpenWorkspace { id }).await.expect("open");

    let sv = read_session(&w).await;
    assert!(
        matches!(&sv.net_health, NetHealth::Down { reason } if !reason.is_empty()),
        "opening with MLS but no queue creds must be an HONEST Down, got {:?}",
        sv.net_health
    );
    assert_eq!(
        sv.notice, "detached",
        "the persistent detached/offline notice is set"
    );

    // offline-first: the local log still accepts writes
    w.execute(Command::Chat {
        body: "written while offline".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("offline chat appends locally");
    let chat = read_chat(&w).await;
    assert!(
        chat.iter().any(|m| m["body"] == serde_json::json!("written while offline")),
        "offline-first local chat: {chat:?}"
    );
}
