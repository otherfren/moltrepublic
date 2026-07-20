// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Mesh self-heal Stage 3: self-initiated re-announce** (`documents/mesh_selfheal.md`).
//!
//! The #1 missing piece the design calls out: a node that, on its own, mints a
//! fresh inbound queue and broadcasts the new address over its still-working
//! legs (today only the recovery coordinator could produce a live-mesh
//! `MeshAnnounced`). Pinned here:
//!
//! - `NetMeshRotate { peer }` on a live mesh broadcasts a SELF-authored
//!   `WorkspaceEvent::MeshAnnounced` carrying a relay nonce, which reaches the
//!   peer over the working leg.
//!
//! The reciprocal fold-in (the peer adopts via the reused
//! `spawn_mesh_extension` → replies → `cmd_net_mesh_extended`) needs a second
//! *running* engine to reply and is exercised by the recovery/dynamic-mesh
//! suites that share that exact adopter path; here the peer is a bare
//! supervisor, so only the re-announce broadcast is asserted.

mod common;

use std::time::Duration;

use common::{found_with_mesh, read_session, CaptureSink};
use molt_core::{Command, NetHealth, WorkspaceEvent};
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{MlsChannel, MlsMember, PeerLink};
use tokio::sync::watch;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_self_initiated_rotate_broadcasts_a_nonced_reannounce() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (a, hub, member_mesh, member_mls, _id) = found_with_mesh(&root_a).await;
    a.execute(Command::CreateFinish).await.expect("enter the workspace");

    // member-b's runtime supervisor with a capturing sink
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let member_group = MlsMember::restore(&member_mls).expect("restore member MLS");
    let member_sink = CaptureSink::default();
    let (_wake, wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 11),
        MemLog::new(),
        MemStateStore::new(),
        member_sink.clone(),
        wake_rx,
        Some(MlsChannel::new(member_group)),
    );

    // The founder's mesh is up (found_with_mesh waited for "direct mesh
    // established" = net.is_real()). Verify-at-open leaves the leg amber
    // "verifying" until a frame is heard from the stub member — a raw supervisor
    // that never warms back — so net_health does NOT reach Ok here, and the
    // rotate does not need it to (it gates on the mesh being real, not on
    // health). Just confirm the open is honest, not Down.
    assert!(
        !matches!(read_session(&a).await.net_health, NetHealth::Down { .. }),
        "the founder's mesh must be up, not Down"
    );

    // trigger a self-initiated rotate toward member-b
    a.execute(Command::NetMeshRotate {
        peer: "member-b".to_string(),
        generation: None,
    })
    .await
    .expect("rotate");

    // member-b receives a SELF-authored, nonce-carrying re-announce over the
    // working leg — the self-heal broadcast the recovery coordinator used to be
    // the only producer of
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let got = member_sink.messages();
        if got.iter().any(|(from, env)| {
            from == "founder-a"
                && matches!(&env.body, WorkspaceEvent::MeshAnnounced { nonce: Some(_), .. })
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the rotate never broadcast a nonce-carrying re-announce; got {:?}",
            got.iter()
                .map(|(f, e)| (f.clone(), format!("{:?}", e.body)))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
