// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Cross-package chat-bus integration** (implementation plan §5.2): a
//! chain-governed two-instance workspace where the founder proposes through
//! real threshold governance, both sides chat in the proposal's
//! `Patch(id)` channel AND the all-hands `Group`, the engine-side channel
//! filter equals the client-side filter of the full read on BOTH engines, a
//! member reaction in the patch channel converges to the founder over the
//! mesh. One test, because the stages build on one another and the founding
//! is the expensive part. (The MCP-tool-built read equality of §5.2 lives in
//! `molt-mcp/tests/tool_reads.rs` — mcp → engine is the legal dependency
//! direction; the engine never depends on a surface crate, not even dev-only.)

mod common;

use std::time::Duration;

use molt_core::{
    ChannelRef, ChatMessage, Command, EventEnvelope, Reply, SessionSettings, SessionView,
    Surface, SurfaceSnapshot, WorkspaceEvent,
};
use molt_engine::WalletHandle;
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{MlsChannel, MlsMember, PeerLink};
use tokio::sync::watch;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// One `ReadState` on the chat surface, optionally channel-filtered.
async fn read_chat_snap(w: &WalletHandle, channel: Option<ChannelRef>) -> SurfaceSnapshot {
    match w
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// The channel a serialized chat message files under: `Group` messages omit
/// the field entirely (`skip_serializing_if`), every other ref is the
/// internally-tagged object.
fn channel_json(c: &ChannelRef) -> Option<serde_json::Value> {
    match c {
        ChannelRef::Group => None,
        other => Some(serde_json::to_value(other).expect("channel serializes")),
    }
}

/// The client-side filter of a full read — what the engine filter must equal.
fn client_filter(full: &[serde_json::Value], c: &ChannelRef) -> Vec<serde_json::Value> {
    let want = channel_json(c);
    full.iter()
        .filter(|m| m.get("channel") == want.as_ref())
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn channels_govern_chat_and_filter_coequally_across_instances() {
    // ---- the chain-governed two-instance workspace (founding ritual) ----
    let tmp = tempfile::tempdir().expect("tmp");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("founder").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding_bootstrap(molt_core::GroupConfig::demo(), session_a);
    a.execute(Command::CreateStart {
        name: "Guild".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
        net: "tor".to_string(),
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
    let b_phrase_for_b = b_phrase.clone();
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, true, None, None)
            .await
            .expect("B completes the member side + bootstrap")
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "member-b never joined");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Guild".to_string(),
        agenda: "deliberate per channel".to_string(),
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
    let sealed = b_outcome.sealed.expect("member collected the sealed roster");
    a.execute(Command::CreateFinish).await.expect("enter");

    // ---- the SECOND full engine: member-b's own instance --------------
    // Its workspace materializes from the SAME sealed roster the ritual
    // ratified (one genesis builder, `SealedRoster::into_genesis`), so both
    // engines project the same republic. Its runtime supervisor rides the
    // still-alive founding hub and feeds the engine through `net_sink` —
    // the same delivery path a production mesh drives.
    let root_b = tmp.path().join("member");
    let b_entropy = molt_storage::seed_entropy(&b_phrase_for_b).expect("b entropy");
    let genesis_b = sealed.into_genesis("member-b", 1_751_000_000);
    let ws_b = molt_storage::create_workspace(&root_b, &b_entropy, &genesis_b).expect("create b");
    let id_b = ws_b.manifest.workspace.id.clone();
    drop(ws_b); // release the LOCK for the engine
    let session_b = SessionView {
        workspaces: molt_storage::scan_workspaces(&root_b)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root_b.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let b = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session_b);
    b.execute(Command::OpenWorkspace { id: id_b })
        .await
        .expect("open member workspace");

    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let member_group = MlsMember::restore(&member_mls).expect("restore member MLS");
    let member_feed = MemLog::new();
    let (member_wake, member_wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 9),
        member_feed.clone(),
        MemStateStore::new(),
        b.net_sink(),
        member_wake_rx,
        Some(MlsChannel::new(member_group)),
    );

    // ---- (a) real threshold governance: propose, member co-signs ------
    let payload = serde_json::json!({"op": "add_note", "title": "minutes"});
    let pid = match a
        .execute(Command::Propose {
            surface: Surface::Memory,
            payload: payload.clone(),
        })
        .await
        .expect("propose")
    {
        Reply::Proposed { id } => id,
        other => panic!("unexpected: {other:?}"),
    };
    assert!(
        common::read_applied(&a, Surface::Memory).await.is_empty(),
        "the founder's own signature alone must not commit a 2-of-2 change"
    );
    // the member co-signs the SAME change with its own identity key (the
    // ritual salts the identity with the member's workspace-id string)
    let b_ws = molt_storage::derive_workspace_id(&b_entropy, "member");
    let (b_sk, _b_pk) = molt_storage::derive_identity_key(&b_entropy, &b_ws);
    let change = molt_core::ChainChange::Applied {
        proposal_id: pid.0,
        surface: Surface::Memory,
        payload: payload.clone(),
    };
    let bytes = molt_core::approval_bytes(&sealed.republic_id, 1, &change);
    let b_sig = molt_storage::identity_sign(&b_sk, &bytes);
    member_feed.push(EventEnvelope {
        seq: 2,
        ts: 1_751_000_200,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Approved {
            id: pid,
            by: "member-b".to_string(),
            height: 1,
            sig: b_sig,
        },
    });
    let _ = member_wake.send(2);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let applied = common::read_applied(&a, Surface::Memory).await;
        if applied.iter().any(|v| v["title"] == serde_json::json!("minutes")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the mesh-approved change never committed; applied: {applied:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ---- (b) both sides chat in Patch(pid) AND Group -------------------
    let patch = ChannelRef::Patch { id: pid };
    a.execute(Command::Chat {
        body: "kickoff".to_string(),
        quote: None,
        channel: ChannelRef::Group,
    })
    .await
    .expect("founder group chat");
    a.execute(Command::Chat {
        body: "patch talk from the founder".to_string(),
        quote: None,
        channel: patch.clone(),
    })
    .await
    .expect("founder patch chat");
    // the member chats through its OWN engine (which mints the ids) …
    b.execute(Command::Chat {
        body: "hello all".to_string(),
        quote: None,
        channel: ChannelRef::Group,
    })
    .await
    .expect("member group chat");
    b.execute(Command::Chat {
        body: "patch talk from the member".to_string(),
        quote: None,
        channel: patch.clone(),
    })
    .await
    .expect("member patch chat");
    // … and its supervisor outbox carries the SAME messages (same ids) to
    // the founder — this test wires the member engine's two chats onto the
    // wire by hand, exactly what the engine's own outbox does over SMP
    let b_chat = read_chat_snap(&b, None).await.applied;
    assert_eq!(b_chat.len(), 2, "the member's own two messages: {b_chat:?}");
    for (i, m) in b_chat.iter().enumerate() {
        let msg: ChatMessage = serde_json::from_value(m.clone()).expect("chat message decodes");
        member_feed.push(EventEnvelope {
            seq: 3 + u64::try_from(i).expect("tiny"),
            ts: msg.ts,
            by: "member-b".to_string(),
            body: WorkspaceEvent::Chat(msg),
        });
    }
    let _ = member_wake.send(4);

    // both engines converge on four messages (2 own + 2 from the peer)
    common::await_chat_len(&a, 4, 15).await;
    common::await_chat_len(&b, 4, 15).await;

    // ---- (c) the engine filter == the client-side filter, on BOTH ------
    for (w, who) in [(&a, "founder"), (&b, "member")] {
        let full = read_chat_snap(w, None).await;
        assert_eq!(full.applied.len(), 4, "{who} full log");
        for ch in [ChannelRef::Group, patch.clone()] {
            let filtered = read_chat_snap(w, Some(ch.clone())).await;
            // the concept's Phase-3 acceptance property, cross-package: the
            // engine-side filter returns EXACTLY the client-side filter of
            // the full read (proven red first against `full.applied`)
            assert_eq!(
                filtered.applied,
                client_filter(&full.applied, &ch),
                "{who}: filtered {ch:?} equals the client-side filter"
            );
            assert_eq!(filtered.applied.len(), 2, "{who}: two messages per channel");
            // the enumeration is a whole-log concern and ignores the filter
            assert_eq!(
                filtered.channels, full.channels,
                "{who}: filtering must not change the channel enumeration"
            );
        }
        let keys: Vec<Option<serde_json::Value>> = full
            .channels
            .iter()
            .map(|i| channel_json(&i.channel))
            .collect();
        assert!(keys.contains(&None), "{who}: Group is always enumerated");
        assert!(
            keys.contains(&channel_json(&patch)),
            "{who}: the patch channel is enumerated"
        );
    }

    // ---- (d) a member reaction in the patch channel converges ----------
    let a_full = read_chat_snap(&a, Some(patch.clone())).await.applied;
    let founder_patch_id: molt_core::MessageId = a_full
        .iter()
        .find(|m| m["body"] == serde_json::json!("patch talk from the founder"))
        .and_then(|m| m["id"].as_str())
        .expect("the founder's patch message id")
        .parse()
        .expect("valid id");
    member_feed.push(EventEnvelope {
        seq: 5,
        ts: 1_751_000_300,
        by: "member-b".to_string(),
        body: WorkspaceEvent::ChatReacted {
            index: 0, // the member's sender-local idea — must not matter
            id: Some(founder_patch_id),
            emoji: "👍".to_string(),
            by: "member-b".to_string(),
            op: Some(molt_core::ReactOp::Add),
        },
    });
    let _ = member_wake.send(5);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let patch_msgs = read_chat_snap(&a, Some(patch.clone())).await.applied;
        if patch_msgs.iter().any(|m| {
            m["id"] == serde_json::json!(founder_patch_id.to_string())
                && m["reactions"]["👍"] == serde_json::json!(["member-b"])
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member's patch-channel reaction never reached the founder: {patch_msgs:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ---- (e) a file share crosses the wire WITH its channel (Q8) -------
    // A share IS a chat message, so the member's offer into the patch
    // channel must arrive at the founder filed under Patch(pid) — not
    // flattened into Group.
    let share_src = tempfile::tempdir().expect("share tmp");
    let share_path = share_src.path().join("minutes.pdf");
    std::fs::write(&share_path, b"the patch minutes").expect("write share source");
    b.execute(Command::ShareFile {
        path: share_path.display().to_string(),
        channel: patch.clone(),
    })
    .await
    .expect("member shares into the patch channel");
    // the share posts async once the off-actor hash completes — poll
    let b_share = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let hit = read_chat_snap(&b, Some(patch.clone()))
                .await
                .applied
                .into_iter()
                .find(|m| m["file"]["name"] == serde_json::json!("minutes.pdf"));
            if let Some(hit) = hit {
                break hit;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the member's own patch view never held the offer"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let share_msg: ChatMessage = serde_json::from_value(b_share).expect("share decodes");
    assert_eq!(share_msg.channel, patch, "the share is tagged before the wire");
    member_feed.push(EventEnvelope {
        seq: 6,
        ts: share_msg.ts,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Chat(share_msg),
    });
    let _ = member_wake.send(6);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let patch_msgs = read_chat_snap(&a, Some(patch.clone())).await.applied;
        if patch_msgs
            .iter()
            .any(|m| m["file"]["name"] == serde_json::json!("minutes.pdf"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member's patch-channel file offer never reached the founder: {patch_msgs:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // … and the founder's Group view does NOT hold the offer
    assert!(
        read_chat_snap(&a, Some(ChannelRef::Group))
            .await
            .applied
            .iter()
            .all(|m| m.get("file").is_none() || m["file"].is_null()),
        "no file offer leaks into the founder's group view"
    );

    // (f) of §5.2 — the MCP-tool-built read equality — lives in
    // molt-mcp/tests/tool_reads.rs, on the legal side of the crate layering.
}
