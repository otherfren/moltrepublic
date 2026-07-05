// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The founding ritual end to end (transport concept §3.3): found a
//! persisted republic through the engine, then read the genesis straight
//! off disk and prove the sealed roster — every member's identity key is
//! anchored and every attestation verifies against it over the one
//! canonical table. This is the "sealed from birth" guarantee, checked
//! with real Ed25519 signatures, not a mock.

use std::time::Duration;

use molt_core::{Command, Reply, SessionSettings, SessionView, WorkspaceEvent};
use molt_engine::WalletHandle;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn await_founding(w: &WalletHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(w).await;
        match s.create.run.outcome {
            1 => return,
            2 => panic!("founding failed: {:?}", s.create.run.log),
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "founding did not seal in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn founding_seals_a_verifiable_roster_on_disk() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session);

    // found a 2-of-4 republic: founder + 3 simulated members
    w.execute(Command::CreateStart {
        name: "Sealed Club".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 4,
        net: "tor".to_string(),
    })
    .await
    .expect("create start");
    await_founding(&w).await;

    // every seat is green and named
    let s = read_session(&w).await;
    assert_eq!(s.create.seats.len(), 3);
    assert!(s.create.seats.iter().all(|seat| seat.state == 2));
    assert!(s.create.seats.iter().all(|seat| !seat.member.is_empty()));

    w.execute(Command::CreateFinish).await.expect("enter");
    let id = read_session(&w).await.active_workspace.clone();
    assert_eq!(id.len(), 64);

    // close and read the genesis straight off the log — the closing
    // snapshot puts it under the replay floor, but the frame is the truth
    // and carries the attestations the snapshot dump does not
    w.execute(Command::CloseWorkspace).await.expect("close");
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("dir");
    let (ws, _loaded) = molt_storage::open_workspace(&dir).expect("open");
    let log = ws.read_log_from(1).expect("read genesis");
    let genesis = log.first().expect("genesis frame");

    let WorkspaceEvent::Founded {
        rule_m,
        rule_n,
        roster,
        identities,
        attestations,
        ..
    } = &genesis.body
    else {
        panic!("first event is not Founded");
    };
    assert_eq!(*rule_m, 2);
    assert_eq!(*rule_n, 4);
    assert_eq!(roster.len(), 4, "founder + 3 members");
    assert_eq!(identities.len(), 4, "one identity key per member");
    assert_eq!(attestations.len(), 4, "sealed by everyone");
    assert_eq!(identities[0].member, "petra", "founder leads the table");

    // the roster order matches the identity table
    let names: Vec<&str> = identities.iter().map(|i| i.member.as_str()).collect();
    let roster_names: Vec<&str> = roster.iter().map(String::as_str).collect();
    assert_eq!(names, roster_names);

    // THE guarantee: every attestation verifies against the anchored key
    // over the one canonical table
    let table = molt_core::roster_canonical_bytes(&id, *rule_m, *rule_n, identities);
    for att in attestations {
        let identity = identities
            .iter()
            .find(|i| i.member == att.member)
            .expect("attestation names a member in the table");
        assert!(
            molt_storage::identity_verify(&identity.identity_pk, &table, &att.sig),
            "attestation for {} does not verify",
            att.member
        );
    }

    // a tampered table breaks every signature (sanity: the check has teeth)
    let bad = molt_core::roster_canonical_bytes(&id, 3, *rule_n, identities);
    assert!(!molt_storage::identity_verify(
        &identities[0].identity_pk,
        &bad,
        &attestations[0].sig
    ));
}
