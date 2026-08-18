// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Member profiles are REAL vote-gated state (`member_profiles_plan.md`):
//! a member proposes its OWN picture and description, the republic votes,
//! and the applied change materializes an avatar file inside every member's
//! workspace directory — the reference `read_members` serves is that local
//! file. A `remove_member_image` deletes it again, and a reopen rebuilds
//! every avatar deterministically from the replayed log.
//!
//! Runs on a REAL 2-of-2 pair over an in-process relay, like `org_effective`:
//! the applied EFFECT under the real threshold machinery.

use std::time::Duration;

use molt_core::{Command, GroupConfig, MemberView, Reply, SessionSettings, SessionView, Surface};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nsession: notice={:?} create.log={:?} join.log={:?}",
            s.notice,
            s.create.run.log,
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() })
        .await
        .expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    wait_for(w, "the relay probe to confirm the relay", |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// Found a real 2-of-2 republic over the relay; both engines end up entered.
async fn found_pair(
    root: &std::path::Path,
    urls: &[&str],
    name: &str,
) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    for url in urls {
        adopt_relay(&a, url).await;
    }
    a.execute(Command::CreateStart {
        name: name.to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "the seat link to become joinable", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&root.join("member"));
    for url in urls {
        adopt_relay(&b, url).await;
    }
    b.execute(Command::JoinStart {
        invite: link,
        member: "walter".to_string(),
    })
    .await
    .expect("join starts");
    wait_for(&a, "the founder to accept the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: name.to_string(),
        agenda: String::new(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    wait_for(&b, "walter to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    {
        let seed_ = read_session(&b).await.join.seed.clone();
        b.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("joiner backup confirm");
    }
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join to seal", |s| {
        s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
    })
    .await;
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// The second voice: wait until `w` sees the open proposal with this `op`,
/// then approve it through the public command surface.
async fn approve_op(w: &WalletHandle, op: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Reply::Proposals { proposals } =
            w.execute(Command::ListProposals).await.expect("list proposals")
        {
            if let Some(p) = proposals.iter().find(|p| {
                p.state == molt_core::ProposalState::Proposed
                    && p.payload.get("op").and_then(|v| v.as_str()) == Some(op)
            }) {
                w.execute(Command::Approve { proposal: p.id })
                    .await
                    .expect("approve");
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the {op} proposal never reached the second voice"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn members(w: &WalletHandle) -> Vec<MemberView> {
    match w.execute(Command::ReadMembers).await.expect("read members") {
        Reply::Members { members } => members,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Wait until `w`'s members table satisfies `pred` for `member`.
async fn wait_member(
    w: &WalletHandle,
    what: &str,
    member: &str,
    pred: impl Fn(&MemberView) -> bool,
) -> MemberView {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(row) = members(w).await.into_iter().find(|m| m.member == member) {
            if pred(&row) {
                return row;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A square, decodable image of exactly `len` bytes: a 2x2 BMP header
/// padded out. The engine's sniff reads only the header, so the padding
/// rides free — which is what makes an arbitrary SIZE testable.
fn padded_square_bmp(len: usize) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&54u32.to_le_bytes()); // "file size" (header only)
    b.extend_from_slice(&[0; 4]); // reserved
    b.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
    b.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
    b.extend_from_slice(&2i32.to_le_bytes()); // width
    b.extend_from_slice(&2i32.to_le_bytes()); // height
    b.extend_from_slice(&1u16.to_le_bytes()); // planes
    b.extend_from_slice(&24u16.to_le_bytes()); // bits per pixel
    b.extend_from_slice(&[0; 24]); // compression/size/ppm/palette zeros
    b.resize(len.max(b.len()), 0x00);
    b
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn image_budget(w: &WalletHandle) -> u64 {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => st.image_budget,
        other => panic!("unexpected: {other:?}"),
    }
}

/// **The keystone**: a member's own picture and description are gated
/// changes, and applying them is real on BOTH devices — the picture as a
/// file, the description as table state. A reopen rebuilds the file, a
/// removal deletes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_applied_profile_materializes_the_avatar_on_every_device() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");

    let (a, b) = found_pair(&root, &[&url], "Bee Club").await;
    let id = read_session(&a).await.active_workspace.clone();
    let dir_a = molt_storage::find_workspace_dir(&root.join("founder"), &id).expect("dir a");
    // the storage id is per install — the joiner names its own copy
    let id_b = read_session(&b).await.active_workspace.clone();
    let dir_b = molt_storage::find_workspace_dir(&root.join("member"), &id_b).expect("dir b");

    let picture = padded_square_bmp(64);
    a.execute(Command::Propose {
        surface: Surface::Organization,
        payload: serde_json::json!({
            "op": "set_member_image",
            "member": "petra",
            "value": "face.bmp",
            "bytes_b64": b64(&picture),
        }),
    })
    .await
    .expect("propose set_member_image");
    approve_op(&b, "set_member_image").await;

    // the applied picture is a FILE on both devices, with the proposed bytes
    for (w, dir) in [(&a, &dir_a), (&b, &dir_b)] {
        let row = wait_member(w, "the avatar reference", "petra", |m| !m.image.is_empty()).await;
        let path = std::path::PathBuf::from(&row.image);
        assert_eq!(path.parent(), Some(dir.as_path()), "the avatar lives in the workspace dir");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !std::fs::read(&path).is_ok_and(|have| have == picture) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the avatar file never materialized with the proposed bytes: {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // …and no other seat picked it up
        let walter = members(w).await.into_iter().find(|m| m.member == "walter").expect("walter");
        assert_eq!(walter.image, "", "one seat's picture must not leak onto another");
    }

    // a reopen rebuilds the file deterministically from the replayed log
    let avatar = std::path::PathBuf::from(
        wait_member(&a, "the avatar reference", "petra", |m| !m.image.is_empty())
            .await
            .image,
    );
    std::fs::remove_file(&avatar).expect("drop the materialized file");
    a.execute(Command::CloseWorkspace).await.expect("close");
    a.execute(Command::OpenWorkspace { id: id.clone() }).await.expect("reopen");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !std::fs::read(&avatar).is_ok_and(|have| have == picture) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reopen did not rebuild the avatar file"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // the description is table state on both devices
    a.execute(Command::Propose {
        surface: Surface::Organization,
        payload: serde_json::json!({
            "op": "set_member_desc",
            "member": "petra",
            "value": "keeps the bees",
        }),
    })
    .await
    .expect("propose set_member_desc");
    approve_op(&b, "set_member_desc").await;
    for w in [&a, &b] {
        wait_member(w, "the applied description", "petra", |m| {
            m.description == "keeps the bees"
        })
        .await;
    }

    // an applied removal deletes the file and clears the reference
    a.execute(Command::Propose {
        surface: Surface::Organization,
        payload: serde_json::json!({ "op": "remove_member_image", "member": "petra" }),
    })
    .await
    .expect("propose remove_member_image");
    approve_op(&b, "remove_member_image").await;
    wait_member(&a, "the cleared avatar reference", "petra", |m| m.image.is_empty()).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while avatar.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the applied removal did not delete the avatar file"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // the description survives its own vote's slot
    let row = members(&a).await.into_iter().find(|m| m.member == "petra").expect("petra");
    assert_eq!(row.description, "keeps the bees");

    // the served budget is a PROMISE, not decoration: an image fitted to it
    // is accepted, and a clearly oversized one is refused naming the image
    let budget = usize::try_from(image_budget(&a).await).expect("budget fits");
    assert!(budget >= 32 * 1024, "the served image budget is unusably small: {budget} B");
    let propose = |bytes: Vec<u8>| {
        a.execute(Command::Propose {
            surface: Surface::Organization,
            payload: serde_json::json!({
                "op": "set_member_image",
                "member": "petra",
                "value": "face.bmp",
                "bytes_b64": b64(&bytes),
            }),
        })
    };
    let err = propose(padded_square_bmp(budget * 2))
        .await
        .expect_err("an image over the served budget is refused");
    assert!(
        format!("{err}").contains("image"),
        "the refusal must name the image: {err}"
    );
    propose(padded_square_bmp(budget))
        .await
        .expect("an image at the served budget is accepted");
}
