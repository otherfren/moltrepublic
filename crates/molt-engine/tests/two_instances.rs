// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The founding ritual across **two independent, parallel instances**.
//!
//! Instance A is the founder: a real storage-backed engine running the
//! actual `CreateStart` ritual — mint tickets, open per-seat queues,
//! verify each activation's MAC, collect keys, seal the roster, write the
//! genesis. It runs in *manual* mode, so it does NOT simulate its members;
//! instead it hands out each seat's transport material.
//!
//! Instance B is a genuinely separate participant: it derives its **own**
//! identity from its **own** recovery phrase and runs the real member side
//! (`run_ritual_member`) against A's transport — exactly the code path a
//! remote node runs, only over the loopback hub instead of SMP (that swap
//! is T3). Nothing about the two sides shares state beyond the wire.
//!
//! The test proves: two independent instances complete the ritual over the
//! transport, and A's on-disk genesis anchors B's real key with an
//! attestation that verifies.

use std::time::Duration;

use molt_core::{Command, Reply, SessionSettings, SessionView, WorkspaceEvent};
use molt_engine::WalletHandle;
use molt_net::{MlsIncoming, MlsMember};

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn founding_ritual_completes_across_two_instances() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");

    // --- Instance A: the founder engine, manual (no simulated members)
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding(molt_core::GroupConfig::demo(), session_a);

    // A founds a 2-of-2 republic: the founder plus exactly one member (B)
    a.execute(Command::CreateStart {
        name: "Duet".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
        net: "tor".to_string(),
    })
    .await
    .expect("create start");

    // A hands out the seat material; B picks it up (blocking recv on a
    // std channel — the material is produced synchronously inside the
    // ritual's start on A's actor thread)
    let materials = tokio::task::spawn_blocking(move || {
        material_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A hands out the invite material")
    })
    .await
    .expect("join blocking");
    assert_eq!(materials.len(), 1, "one seat for the one member");
    let seat = materials.into_iter().next().expect("seat material");

    // While the ritual is open, A refuses "Enter republic"
    assert!(matches!(
        a.execute(Command::CreateFinish).await,
        Err(molt_core::MoltError::Create(_))
    ));

    // --- Instance B: a genuinely separate participant runs the member
    // side with its own freshly generated recovery phrase and identity
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        // collect_genesis = false: this test checks the founder's genesis, not
        // the joiner's own workspace
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, false, None)
            .await
            .expect("B completes the member side")
            .pk
    });
    let b_pk = b_task.await.expect("B task");

    // --- A seals and the workspace comes into being
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let id = loop {
        let s = read_session(&a).await;
        if s.create.run.outcome == 1 {
            break s.active_workspace.clone();
        }
        assert_eq!(s.create.run.outcome, 0, "ritual must not fail");
        assert!(
            tokio::time::Instant::now() < deadline,
            "the ritual did not seal across the two instances in time; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // the seat is green and named for B
    let s = read_session(&a).await;
    assert_eq!(s.create.seats.len(), 1);
    assert_eq!(s.create.seats[0].member, "member-b");
    assert_eq!(s.create.seats[0].state, 2, "sealed");

    a.execute(Command::CreateFinish).await.expect("enter");

    // --- A's genesis anchors B's real key with a verifying attestation
    a.execute(Command::CloseWorkspace).await.expect("close");
    let dir = molt_storage::find_workspace_dir(&root_a, &id).expect("dir");
    let (ws, _loaded) = molt_storage::open_workspace(&dir).expect("open");
    let log = ws.read_log_from(1).expect("genesis");
    let WorkspaceEvent::Founded {
        rule_m,
        rule_n,
        identities,
        attestations,
        republic_id,
        ..
    } = &log[0].body
    else {
        panic!("first event is not Founded");
    };
    assert_eq!((*rule_m, *rule_n), (2, 2));
    assert_eq!(identities.len(), 2, "founder + member-b");
    assert_eq!(attestations.len(), 2, "both signed");

    // B's key, as derived on the independent instance, is the one anchored
    let b_entry = identities
        .iter()
        .find(|i| i.member == "member-b")
        .expect("member-b anchored");
    assert_eq!(b_entry.identity_pk, b_pk, "B's own derived key is anchored");

    // the roster is salted by the neutral republic id, not any local ws id
    assert_eq!(
        *republic_id,
        molt_storage::republic_id("Duet", *rule_m, *rule_n, identities),
        "the republic id is the content-derived value"
    );
    // every attestation verifies against the anchored key over the table
    let table = molt_core::roster_canonical_bytes(republic_id, *rule_m, *rule_n, identities);
    for att in attestations {
        let identity = identities
            .iter()
            .find(|i| i.member == att.member)
            .expect("attestation names a member");
        assert!(
            molt_storage::identity_verify(&identity.identity_pk, &table, &att.sig),
            "attestation for {} does not verify",
            att.member
        );
    }
}

/// The founding ritual **establishes a real MLS group** across the two
/// instances (T2): B's KeyPackage rides its JoinRequest, A builds the group at
/// sealing and ships the Welcome with the genesis, B joins from it, and — after
/// both persist their own group state — they exchange authenticated MLS
/// application messages in both directions. The confidentiality layer whose
/// ciphertext is the SMP payload is born with the republic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn founding_establishes_a_real_mls_group_across_two_instances() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding(molt_core::GroupConfig::demo(), session_a);
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

    // B runs the real member side with collect_genesis = true, so it waits for
    // the founder's Welcome and returns its own MLS snapshot
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, None)
            .await
            .expect("B completes the member side")
    });

    // A seals; the workspace comes into being and A distributes the Welcome
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let id = loop {
        let s = read_session(&a).await;
        if s.create.run.outcome == 1 {
            break s.active_workspace.clone();
        }
        assert_eq!(s.create.run.outcome, 0, "ritual must not fail: {:?}", s.create.run.log);
        assert!(tokio::time::Instant::now() < deadline, "ritual did not seal in time");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let b_outcome = b_task.await.expect("B task");

    // --- restore the founder's MLS group from its persisted transport.state
    a.execute(Command::CreateFinish).await.expect("enter");
    a.execute(Command::CloseWorkspace).await.expect("close");
    let dir = molt_storage::find_workspace_dir(&root_a, &id).expect("dir");
    let (ws, _loaded) = molt_storage::open_workspace(&dir).expect("open");
    let a_blob = ws
        .read_transport_state()
        .mls
        .expect("the founder's MLS group is sealed into transport.state");
    let mut a_mls = MlsMember::restore(&a_blob).expect("restore founder MLS");

    // --- restore B's MLS group from the snapshot it returned
    let b_blob = b_outcome
        .mls_snapshot
        .expect("the joiner processed the Welcome and produced a snapshot");
    let mut b_mls = MlsMember::restore(&b_blob).expect("restore member MLS");

    // --- the two groups interoperate: real MLS ciphertext, both directions
    let ct = a_mls.encrypt(b"the charter is ratified").expect("A encrypts");
    match b_mls.decrypt(&ct).expect("B decrypts") {
        MlsIncoming::Application { from, plaintext } => {
            assert_eq!(from, "founder-a", "authenticated sender");
            assert_eq!(plaintext, b"the charter is ratified");
        }
        other => panic!("expected an application message, got {other:?}"),
    }
    let ct = b_mls.encrypt(b"aye, seconded").expect("B encrypts");
    match a_mls.decrypt(&ct).expect("A decrypts") {
        MlsIncoming::Application { from, plaintext } => {
            assert_eq!(from, "member-b", "authenticated sender");
            assert_eq!(plaintext, b"aye, seconded");
        }
        other => panic!("expected an application message, got {other:?}"),
    }
}
