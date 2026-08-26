// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared fixtures of the engine tests: the bare actor state, the
//! `WalletHandle` drivers (session/surface reads, the founding and file
//! awaits) and the recovery fixtures (phrase-anchored chains, a recovering
//! state, a reopen) every sibling test file uses.

use super::*;

pub(super) fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// The demo republics as the session's workspace list — what the
/// fixture-driven tests below address by name (`SessionView::default()`
/// lists nothing since review K6).
pub(super) fn demo_session() -> SessionView {
    SessionView {
        workspaces: molt_core::WorkspaceInfo::demo_set(),
        ..SessionView::default()
    }
}

/// A bare actor state for unit tests of the event applier (no runtime,
/// no storage, no config store).
pub(crate) fn plain_state() -> State {
    let (ev_tx, _keep) = broadcast::channel::<Event>(8);
    let (cmd_tx, _cmd_rx) = mpsc::channel::<Envelope>(8);
    State::new(
        GroupConfig::demo(),
        SessionView::default(),
        ev_tx,
        cmd_tx,
        None,
        false,
        None,
    )
}

pub(super) async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Drive a founding to completion: the ritual runs its simulated
/// members asynchronously (activate → key → seal), so we poll the
/// session until the workspace is sealed (`create.run.outcome == 1`).
pub(super) async fn await_founding(w: &WalletHandle) {
    // the ❻½ gate: the sim members attest automatically, but the
    // FOUNDER's own phrase backup must be confirmed before anything
    // seals — the helper plays that human once the seed is visible
    let mut confirmed = false;
    for _ in 0..600 {
        let s = read_session(w).await;
        if s.create.run.outcome == 1 {
            return;
        }
        if s.create.run.outcome == 2 {
            panic!("founding failed: {:?}", s.create.run.log);
        }
        if !confirmed && !s.create.seed.is_empty() {
            w.execute(Command::ConfirmSeedBackup { phrase: s.create.seed.clone() })
                .await
                .expect("confirm backup");
            confirmed = true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("founding did not seal in time");
}

pub(super) async fn read_surface(w: &WalletHandle, surface: Surface) -> molt_core::SurfaceSnapshot {
    match w
        .execute(Command::ReadState {
            surface,
            channel: None,
            view: None,
        })
        .await
        .expect("read state")
    {
        Reply::State(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// The stable id a chat snapshot row carries, parsed back into the type.
pub(super) fn msg_id(v: &serde_json::Value) -> MessageId {
    v["id"]
        .as_str()
        .expect("message id on the wire")
        .parse()
        .expect("valid message id")
}

/// Write `content` to `dir/name` and share it — awaiting the share
/// message (posting is async: it appears once the off-actor hash
/// completes). Returns the share's stable id.
pub(super) async fn share_temp_file(
    w: &WalletHandle,
    dir: &std::path::Path,
    name: &str,
    content: &[u8],
) -> MessageId {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write share source");
    w.execute(Command::ShareFile {
        path: path.display().to_string(),
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("share");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let snap = read_surface(w, Surface::Chat).await;
        if let Some(row) = snap
            .applied
            .iter()
            .find(|m| m["file"]["name"] == serde_json::json!(name))
        {
            return msg_id(row);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the share message for {name} never posted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Poll until `path` exists with exactly `content`.
pub(super) async fn await_file(path: &std::path::Path, content: &[u8]) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::fs::read(path).is_ok_and(|b| b == content) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} never landed with the expected bytes",
            path.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Poll the session until the in-flight export settles (~Argon2-bounded).
pub(super) async fn await_export(w: &WalletHandle) -> molt_core::ExportState {
    for _ in 0..600 {
        let sv = read_session(w).await;
        if !sv.export.running && !sv.export.result.is_empty() {
            return sv.export;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("export did not settle in time");
}

/// A BMP whose HEADER declares the given dimensions; carries no pixel
/// data (dimension sniffs read only the header, so none is needed).
pub(crate) fn tiny_bmp_header(w: u32, h: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&54u32.to_le_bytes()); // "file size" (header only)
    b.extend_from_slice(&[0; 4]); // reserved
    b.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
    b.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
    b.extend_from_slice(&i32::try_from(w).expect("small dims").to_le_bytes());
    b.extend_from_slice(&i32::try_from(h).expect("small dims").to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // planes
    b.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    b.extend_from_slice(&[0; 24]); // compression/size/ppm/palette zeros
    b
}

/// A real, threshold-signed **two-block chain** for the recovery tests: a
/// genesis anchoring a coordinator and "bob" — whose identity derives from
/// `phrase` exactly as the ritual derives it — plus one gated `Applied`
/// block (m=1, signed by the coordinator). Recovering must adopt the FULL
/// chain: the genesis alone would not project block 1's surface state.
pub(super) fn recovered_chain(phrase: &str) -> (Vec<molt_core::ChainBlock>, String) {
    recovered_chain_with(phrase, Vec::new(), None)
}

/// [`recovered_chain`] with a ratified relay pool and, when `re_anchor`
/// is given, a third block: the `Restored` membership change that puts
/// the seat back with a NEW transport anchor — the shape a real Nostr
/// recovery produces before the rejoiner materializes.
pub(super) fn recovered_chain_with(
    phrase: &str,
    relays: Vec<String>,
    re_anchor: Option<String>,
) -> (Vec<molt_core::ChainBlock>, String) {
    use molt_core::{ChainBlock, ChainChange, MemberIdentity, RosterAttestation, GENESIS_PREV};
    let (coord_sk, coord_pk) = molt_storage::derive_identity_key(&[7u8; 32], "coordinator");
    let (bob_sk, bob_pk) =
        crate::founding::member_identity(phrase).expect("bob's ritual identity");
    let identities = vec![
        MemberIdentity {
            member: "coordinator".to_string(),
            identity_pk: coord_pk,
            nostr_pk: "cc".repeat(32),
        },
        MemberIdentity {
            member: "bob".to_string(),
            identity_pk: bob_pk.clone(),
            nostr_pk: "dd".repeat(32),
        },
    ];
    let republic_id = molt_storage::republic_id("Guild", 1, 2, &identities);
    let change = ChainChange::Genesis {
        name: "Guild".to_string(),
        republic_id: republic_id.clone(),
        rule_m: 1,
        rule_n: 2,
        identities,
        agenda: "survive total loss".to_string(),
        relays,
        features: None,
    };
    let bytes = molt_core::approval_bytes(&republic_id, 0, &change);
    let genesis = ChainBlock {
        height: 0,
        prev: GENESIS_PREV.to_string(),
        sigs: vec![
            RosterAttestation {
                member: "coordinator".to_string(),
                sig: molt_storage::identity_sign(&coord_sk, &bytes),
            },
            RosterAttestation {
                member: "bob".to_string(),
                sig: molt_storage::identity_sign(&bob_sk, &bytes),
            },
        ],
        change,
    };
    let change1 = ChainChange::Applied {
        proposal_id: 1,
        surface: Surface::Memory,
        payload: json!({"op":"add_note","title":"survived the loss"}),
    };
    let bytes1 = molt_core::approval_bytes(&republic_id, 1, &change1);
    let block1 = ChainBlock {
        height: 1,
        prev: molt_storage::content_hash(&molt_core::block_link_bytes(&republic_id, &genesis)),
        sigs: vec![RosterAttestation {
            member: "coordinator".to_string(),
            sig: molt_storage::identity_sign(&coord_sk, &bytes1),
        }],
        change: change1,
    };
    let Some(new_anchor) = re_anchor else {
        return (vec![genesis, block1], republic_id);
    };
    let change2 = ChainChange::Membership {
        op: molt_core::MembershipOp::Restored,
        member: "bob".to_string(),
        identity_pk: bob_pk,
        nostr_pk: Some(new_anchor),
        relays: Vec::new(),
        consent: None,
    };
    let bytes2 = molt_core::approval_bytes(&republic_id, 2, &change2);
    let block2 = ChainBlock {
        height: 2,
        prev: molt_storage::content_hash(&molt_core::block_link_bytes(&republic_id, &block1)),
        sigs: vec![RosterAttestation {
            member: "coordinator".to_string(),
            sig: molt_storage::identity_sign(&coord_sk, &bytes2),
        }],
        change: change2,
    };
    (vec![genesis, block1, block2], republic_id)
}

pub(super) fn recover_link(member: &str, republic_id: &str) -> String {
    crate::recovery::RecoveryInvite {
        republic: "Guild".to_string(),
        member: member.to_string(),
        ticket: "ab".repeat(8),
        server: "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@no-such-host.invalid"
            .to_string(),
        queue_id: "cd".repeat(12),
        wrap: "ef".repeat(32),
        republic_id: republic_id.to_string(),
        handover: None,
    }
    .render()
}

pub(super) fn storage_session(tmp: &tempfile::TempDir) -> SessionView {
    SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("workspaces").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    }
}

/// A persisting `State` rooted at `tmp` with a recovery context armed for
/// `member`. The direct-actor seam the join twin uses, so the workspace
/// flock is released by dropping the state rather than by racing a task.
pub(super) fn recovering_state(
    tmp: &tempfile::TempDir,
    member: &str,
    republic_id: &str,
    phrase: &str,
) -> State {
    let (ev_tx, _keep) = broadcast::channel::<Event>(8);
    let (cmd_tx, _cmd_rx) = mpsc::channel::<Envelope>(8);
    let mut st = State::new(
        GroupConfig::demo(),
        SessionView {
            settings: molt_core::SessionSettings {
                workspace_dir: tmp.path().display().to_string(),
                ..molt_core::SessionSettings::default()
            },
            ..SessionView::default()
        },
        ev_tx,
        cmd_tx,
        None,
        true, // persist — transport.state is what these tests read
        None,
    );
    // the production path: it arms the context, then fails honestly
    // because this build has no rejoin transport
    st.cmd_recover_start(recover_link(member, republic_id), phrase.to_string())
        .expect("recover start arms the context");
    st
}

/// Reopen a materialized workspace directory once its writer is gone.
pub(super) fn reopen(dir: &std::path::Path) -> molt_storage::OpenedWorkspace {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match molt_storage::open_workspace(dir) {
            Ok((ws, _)) => return ws,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("reopening the recovered workspace: {e}"),
        }
    }
}
