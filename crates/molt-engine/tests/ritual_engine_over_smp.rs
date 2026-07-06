// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The founding ritual across two independent instances **driven by the
//! engine actor, over a real SMP server** — the productionised end of
//! `two_instances.rs` (which runs the same flow over the loopback hub).
//!
//! Instance A is a real storage-backed founder engine running the actual
//! `CreateStart` ritual in manual-**over-SMP** mode: it provisions its
//! invite queue on the configured SMP server, verifies the activation MAC,
//! collects the key, seals the roster, and writes the genesis to disk.
//!
//! Instance B is a genuinely separate participant with its **own**
//! `SmpTransport` and its **own** recovery phrase, running the real member
//! side (`run_ritual_member`). It reads only the invite address / wrap /
//! ticket that A handed out — nothing else is shared but the SMP wire.
//!
//! `#[ignore]` (real network):
//! `cargo test -p molt-engine --test ritual_engine_over_smp -- --ignored --nocapture`

use std::time::Duration;

use molt_core::{Command, Reply, SessionSettings, SessionView, WorkspaceEvent};
use molt_engine::{InviteMaterial, RitualTransport, WalletHandle};
use molt_net::smp::{SmpServer, SmpTransport};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "founds over the real smp.konkin.io between two engine instances"]
async fn engine_founds_over_smp_across_two_instances() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");

    // --- Instance A: a real founder engine, founding over the configured
    // SMP server (custom url) in manual mode (no simulated members)
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            smp_server: "custom".to_string(),
            smp_url: KONKIN.to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding_over_smp(molt_core::GroupConfig::demo(), session_a);

    a.execute(Command::CreateStart {
        name: "SMP Duet".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
        net: "tor".to_string(),
    })
    .await
    .expect("create start");

    // A provisions its invite queue on the SMP server off the actor, then
    // hands out the seat material — this now takes a network round-trip
    let materials = tokio::task::spawn_blocking(move || {
        material_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("A provisions + hands out the invite material over SMP")
    })
    .await
    .expect("join blocking");
    assert_eq!(materials.len(), 1, "one seat for the one member");
    let seat = materials.into_iter().next().expect("seat material");

    // --- Instance B: its OWN SmpTransport and recovery phrase. It reuses
    // only A's invite address / wrap / ticket; the transport is entirely
    // B's own connection to the server.
    let b_server = SmpServer::parse(KONKIN).expect("parse");
    let b_material = InviteMaterial {
        seat: seat.seat,
        transport: RitualTransport::Smp(SmpTransport::new(b_server)),
        invite_snd: seat.invite_snd.clone(),
        invite_wrap: seat.invite_wrap.clone(),
        ticket: seat.ticket.clone(),
    };
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(b_material, "member-b".to_string(), b_phrase, None)
            .await
            .expect("B completes the member side over its own SMP transport")
    });
    let b_pk = b_task.await.expect("B task");

    // --- A seals and the workspace comes into being
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let id = loop {
        let s = read_session(&a).await;
        if s.create.run.outcome == 1 {
            break s.active_workspace.clone();
        }
        assert_eq!(s.create.run.outcome, 0, "ritual must not fail: {:?}", s.create.run.log);
        assert!(
            tokio::time::Instant::now() < deadline,
            "the ritual did not seal over SMP in time; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let s = read_session(&a).await;
    assert_eq!(s.create.seats[0].member, "member-b");
    assert_eq!(s.create.seats[0].state, 2, "sealed");
    a.execute(Command::CreateFinish).await.expect("enter");

    // --- A's on-disk genesis anchors B's real key with a verifying
    // attestation — the two independent instances founded a real republic
    // over the real SMP server
    a.execute(Command::CloseWorkspace).await.expect("close");
    let dir = molt_storage::find_workspace_dir(&root_a, &id).expect("dir");
    let (ws, _loaded) = molt_storage::open_workspace(&dir).expect("open");
    let log = ws.read_log_from(1).expect("genesis");
    let WorkspaceEvent::Founded { rule_m, rule_n, identities, attestations, .. } = &log[0].body
    else {
        panic!("first event is not Founded");
    };
    assert_eq!((*rule_m, *rule_n), (2, 2));
    assert_eq!(identities.len(), 2, "founder + member-b");
    assert_eq!(attestations.len(), 2, "both signed");

    let b_entry = identities.iter().find(|i| i.member == "member-b").expect("member-b anchored");
    assert_eq!(b_entry.identity_pk, b_pk, "B's own derived key is anchored");

    let table = molt_core::roster_canonical_bytes(&id, *rule_m, *rule_n, identities);
    for att in attestations {
        let identity =
            identities.iter().find(|i| i.member == att.member).expect("attestation names a member");
        assert!(
            molt_storage::identity_verify(&identity.identity_pk, &table, &att.sig),
            "attestation for {} does not verify",
            att.member
        );
    }
    println!("OK: two engine instances founded a republic over real SMP — genesis verifies");
}
