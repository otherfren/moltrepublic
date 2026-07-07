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
        // the joiner's own workspace; ratify = None: sign as soon as verified
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, false, None, None)
            .await
            .expect("B completes the member side")
            .pk
    });

    // once B has joined, the founder proposes the charter (deliberation step);
    // only then does the roster seal — propose BEFORE awaiting B (B returns
    // after signing, which follows the seal)
    let propose_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(
            tokio::time::Instant::now() < propose_deadline,
            "member-b never joined in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Duet".to_string(),
        agenda: "hold the line".to_string(),
    })
    .await
    .expect("founder proposes the charter");
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
        agenda,
        ..
    } = &log[0].body
    else {
        panic!("first event is not Founded");
    };
    assert_eq!((*rule_m, *rule_n), (2, 2));
    assert_eq!(agenda, "hold the line", "the ratified charter is in the genesis");
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
    let table =
        molt_core::roster_canonical_bytes(republic_id, *rule_m, *rule_n, identities, agenda);
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

/// The deliberation step **gates on the joiner's ratification** (concept
/// §3.3): the founder proposes a charter, the joiner sees it and must confirm
/// before its seal signature is released — until then nothing seals. Drives the
/// real member side with a [`molt_engine::Ratifier`], acting as the human.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn founding_gates_on_the_joiners_charter_ratification() {
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
        name: "Pact".to_string(),
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

    // B runs the real member side behind a ratification gate; the test is the
    // human on the other end of the channels
    let (prop_tx, mut prop_rx) = tokio::sync::mpsc::channel::<(String, String)>(1);
    let (conf_tx, conf_rx) = tokio::sync::mpsc::channel::<bool>(1);
    let ratifier = molt_engine::Ratifier {
        proposal: prop_tx,
        confirm: conf_rx,
    };
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, Some(ratifier), None)
            .await
            .expect("B completes the member side")
    });

    // the founder proposes once B has joined
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "member-b never joined");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Pact".to_string(),
        agenda: "the pact: tend the commons, share the harvest".to_string(),
    })
    .await
    .expect("founder proposes the charter");

    // re-proposing after the charter is out is refused — a second, different
    // charter would silently invalidate signatures already being collected
    assert!(
        matches!(
            a.execute(Command::CreatePropose {
                name: "Pact".to_string(),
                agenda: "a sneaky replacement".to_string(),
            })
            .await,
            Err(molt_core::MoltError::Create(_))
        ),
        "re-proposing the charter must be refused"
    );

    // B surfaces the proposed charter for the human to review
    let (name, agenda) = prop_rx.recv().await.expect("charter surfaced to the joiner");
    assert_eq!(name, "Pact");
    assert_eq!(agenda, "the pact: tend the commons, share the harvest");

    // the gate holds: with B not yet confirmed, the founding has not sealed
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        read_session(&a).await.create.run.outcome,
        0,
        "nothing seals until the joiner ratifies"
    );

    // the human confirms → B signs → the ritual seals
    conf_tx.send(true).await.expect("confirm");
    let b_out = b_task.await.expect("B task");
    let sealed = b_out.sealed.expect("B received the sealed roster");
    assert_eq!(sealed.agenda, "the pact: tend the commons, share the harvest");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let s = read_session(&a).await;
        if s.create.run.outcome == 1 {
            break;
        }
        assert_eq!(s.create.run.outcome, 0, "ritual must not fail: {:?}", s.create.run.log);
        assert!(tokio::time::Instant::now() < deadline, "ritual did not seal after ratification");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A **declined** charter aborts the member side and nothing seals — the other
/// half of the ratification gate (the confirm path is covered above).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declined_charter_aborts_the_member_without_sealing() {
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
        name: "Nope".to_string(),
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
            .expect("material")
    })
    .await
    .expect("join blocking");
    let seat = materials.into_iter().next().expect("seat material");

    let (prop_tx, mut prop_rx) = tokio::sync::mpsc::channel::<(String, String)>(1);
    let (conf_tx, conf_rx) = tokio::sync::mpsc::channel::<bool>(1);
    let ratifier = molt_engine::Ratifier {
        proposal: prop_tx,
        confirm: conf_rx,
    };
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    // return the raw Result — we expect an Err here
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, Some(ratifier), None).await
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
        name: "Nope".to_string(),
        agenda: "you will not agree to this".to_string(),
    })
    .await
    .expect("propose");

    // B surfaces the charter, the human DECLINES
    let _ = prop_rx.recv().await.expect("charter surfaced");
    conf_tx.send(false).await.expect("decline");

    // the member side aborts with an error, and the founding never seals
    let b_res = b_task.await.expect("b task joins");
    assert!(b_res.is_err(), "a declined charter aborts the member side");

    // and the founder is TOLD: the seat turns declined (state 3) with a log line,
    // so a silent member can no longer wedge the founding invisibly
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = read_session(&a).await;
        if s.create.seats.first().is_some_and(|seat| seat.state == 3) {
            assert!(
                s.create.run.log.iter().any(|l| l.contains("declined")),
                "the founder's log records the decline: {:?}",
                s.create.run.log
            );
            break;
        }
        assert_eq!(s.create.run.outcome, 0, "nothing seals when a member declines");
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder was never told about the decline; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
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
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, None, None)
            .await
            .expect("B completes the member side")
    });

    // once B has joined, the founder proposes the charter so the roster seals
    let propose_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(
            tokio::time::Instant::now() < propose_deadline,
            "member-b never joined in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Guild".to_string(),
        agenda: "keep the roads clear".to_string(),
    })
    .await
    .expect("founder proposes the charter");

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
