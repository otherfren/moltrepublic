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

mod common;

use std::time::Duration;

use molt_core::{
    ChatMessage, Command, EventEnvelope, MemberId, Reply, SessionSettings, SessionView, Surface,
    WorkspaceEvent,
};
use molt_engine::WalletHandle;
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{invite, msg_id, EngineSink, MlsChannel, MlsIncoming, MlsMember, NetError, PeerLink};
use tokio::sync::watch;

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
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, false, false, None, None)
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
    // the founder AUTO-ENTERS on seal (no manual "Enter republic") — aligned
    // with the joiner, which auto-enters on its own seal
    assert_eq!(
        s.screen,
        molt_core::Screen::Main,
        "the founder auto-enters the workspace when the ritual seals"
    );
    assert_eq!(s.active_workspace, id, "the founded workspace is active");

    // CreateFinish is now idempotent (already entered) — still accepted
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
    let (acc_tx, mut acc_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (prop_tx, mut prop_rx) = tokio::sync::mpsc::channel::<(String, String)>(1);
    let (conf_tx, conf_rx) = tokio::sync::mpsc::channel::<bool>(1);
    let ratifier = molt_engine::Ratifier {
        accepted: acc_tx,
        proposal: prop_tx,
        confirm: conf_rx,
    };
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, false, Some(ratifier), None)
            .await
            .expect("B completes the member side")
    });

    // BEFORE any charter: the founder acks the join, and the joiner sees it land
    // (early feedback instead of a silent wait)
    tokio::time::timeout(Duration::from_secs(15), acc_rx.recv())
        .await
        .expect("the founder's join-accepted ack reaches the joiner")
        .expect("the ack channel stayed open");

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

    // the ratified charter is exposed on the now-active workspace — this is what
    // the Constitution surface renders
    let s = read_session(&a).await;
    let ws = s
        .workspaces
        .iter()
        .find(|w| w.id == s.active_workspace)
        .expect("the founded workspace is in the list");
    assert_eq!(
        ws.agenda, "the pact: tend the commons, share the harvest",
        "the workspace surfaces the ratified charter"
    );
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

    let (acc_tx, _acc_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (prop_tx, mut prop_rx) = tokio::sync::mpsc::channel::<(String, String)>(1);
    let (conf_tx, conf_rx) = tokio::sync::mpsc::channel::<bool>(1);
    let ratifier = molt_engine::Ratifier {
        accepted: acc_tx,
        proposal: prop_tx,
        confirm: conf_rx,
    };
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    // return the raw Result — we expect an Err here
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, false, Some(ratifier), None).await
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
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, false, None, None)
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

/// The founding ritual **bootstraps a live direct mesh** across the two
/// instances (T2, the 2c-engine wiring). After sealing, the member
/// (`bootstrap = true`) and the founder (`ritual_bootstrap`) exchange
/// `MeshAnnounce`s over the founding star — the member announcing on the invite
/// queue, the founder announcing + relaying on the reply queues — open their
/// per-pair queues, and each assembles + persists its full-mesh handovers. This
/// proves both post-founding sides: the two meshes MIRROR each other (each node
/// sends to the very queue the other receives on) and the two MLS groups still
/// interoperate *after* the announcement round advanced their ratchets — i.e.
/// the bootstrap left the confidentiality layer in sync, ready for live chat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn founding_bootstraps_a_direct_mesh_across_two_instances() {
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

    // B runs the member side WITH the post-founding mesh bootstrap
    // (collect_genesis = true, bootstrap = true): it joins the group, announces
    // its per-pair queues, assembles its direct mesh, and returns it alongside
    // its post-bootstrap MLS snapshot.
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, true, None, None)
            .await
            .expect("B completes the member side + bootstrap")
    });

    // once B has joined, the founder proposes the charter so the roster seals
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
        agenda: "hold the mesh".to_string(),
    })
    .await
    .expect("founder proposes the charter");

    // the founder seals, then its off-actor bootstrap runs and logs the direct
    // mesh — wait for that line (still on the founding log until CreateFinish)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let id = loop {
        let s = read_session(&a).await;
        assert_ne!(s.create.run.outcome, 2, "ritual must not fail: {:?}", s.create.run.log);
        if s.create.run.outcome == 1
            && s.create.run.log.iter().any(|l| l.contains("direct mesh established"))
        {
            break s.active_workspace.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder never bootstrapped its mesh; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let b_outcome = b_task.await.expect("B task");
    let member_mesh = b_outcome.mesh.expect("B assembled its direct mesh");
    assert_eq!(member_mesh.len(), 1, "one link, to the founder");
    let m2f = &member_mesh[0];
    assert_eq!(m2f.member, "founder-a");

    // --- read the founder's persisted mesh + post-bootstrap group from disk ---
    a.execute(Command::CreateFinish).await.expect("enter");
    a.execute(Command::CloseWorkspace).await.expect("close");
    let dir = molt_storage::find_workspace_dir(&root_a, &id).expect("dir");
    let (ws, _loaded) = molt_storage::open_workspace(&dir).expect("open");
    let ts = ws.read_transport_state();
    assert_eq!(ts.mesh.len(), 1, "the founder persisted its direct mesh");
    let f2m = &ts.mesh[0];
    assert_eq!(f2m.member, "member-b");

    // the two meshes MIRROR each other: each node sends to the queue the other
    // receives on, and the wrap keys pair up the same way
    assert_eq!(f2m.snd_queue, m2f.rcv_queue, "founder sends where member receives");
    assert_eq!(f2m.rcv_queue, m2f.snd_queue, "founder receives where member sends");
    assert_eq!(f2m.snd_wrap, m2f.rcv_wrap);
    assert_eq!(f2m.rcv_wrap, m2f.snd_wrap);

    // --- the post-bootstrap MLS groups still interoperate: the announcement
    // round advanced both ratchets in lockstep, so a chat still decrypts -------
    let a_blob = ts.mls.expect("the founder sealed its post-bootstrap group");
    let mut a_mls = MlsMember::restore(&a_blob).expect("restore founder MLS");
    let b_blob = b_outcome.mls_snapshot.expect("member post-bootstrap snapshot");
    let mut b_mls = MlsMember::restore(&b_blob).expect("restore member MLS");

    let ct = a_mls.encrypt(b"the republic stands").expect("A encrypts");
    match b_mls.decrypt(&ct).expect("B decrypts") {
        MlsIncoming::Application { from, plaintext } => {
            assert_eq!(from, "founder-a");
            assert_eq!(plaintext, b"the republic stands");
        }
        other => panic!("expected an application message, got {other:?}"),
    }
    let ct = b_mls.encrypt(b"and answers").expect("B encrypts");
    match a_mls.decrypt(&ct).expect("A decrypts") {
        MlsIncoming::Application { from, plaintext } => {
            assert_eq!(from, "member-b");
            assert_eq!(plaintext, b"and answers");
        }
        other => panic!("expected an application message, got {other:?}"),
    }
}

/// A test-only sink that records what a manually-built member supervisor
/// delivers (a persisted-log outbox has no wire-scope gate — the real receiver
/// filters — so this may include the genesis alongside the chat).
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

fn member_chat(seq: u64, body: &str) -> EventEnvelope {
    EventEnvelope {
        seq,
        ts: 1_751_000_000 + seq,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Chat(ChatMessage::text("member-b", body, 1_751_000_000 + seq)),
    }
}

/// Part B — after founding, the founder engine stands a **real runtime
/// supervisor** up from its persisted mesh + MLS group (no founding star, no
/// demo mesh) and chats peer-to-peer over MLS with the joined member, both
/// directions. The still-alive loopback hub stands in for the members' SMP
/// server (its queues can't be rebuilt from state, so the runtime reuses it —
/// exactly what a fresh SmpTransport does over a real server).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn founding_chats_over_the_direct_mesh() {
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
    // the shared founding hub — the member's runtime supervisor rides it too
    let hub = seat.transport.clone();

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
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
        agenda: "chat over the mesh".to_string(),
    })
    .await
    .expect("founder proposes the charter");

    // wait until the founder's real supervisor is up (the "direct mesh
    // established" line is logged right after it is built)
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

    // enter the republic — the real net keeps running across CreateFinish
    a.execute(Command::CreateFinish).await.expect("enter");

    // --- build the MEMBER's runtime supervisor on the shared hub ---
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    assert_eq!(links.len(), 1, "one link, to the founder");
    let member_group = MlsMember::restore(&member_mls).expect("restore member MLS");
    let member_feed = MemLog::new();
    let member_sink = RecordSink::default();
    let (member_wake, member_wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 7),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        member_wake_rx,
        Some(MlsChannel::new(member_group)),
    );

    // --- founder → member: the founder engine chats; it reaches the member,
    // MLS-decrypted, over the direct mesh (no star, no demo peers) ---
    a.execute(Command::Chat {
        body: "the mesh carries us".to_string(),
        quote: None,
    })
    .await
    .expect("founder chat");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let got = member_sink.messages();
        if got.iter().any(|(from, env)| {
            from == "founder-a"
                && matches!(&env.body, WorkspaceEvent::Chat(m) if m.body == "the mesh carries us")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder's chat never reached the member over the direct mesh; got {:?}",
            got.iter().map(|(f, _)| f).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // --- member → founder: the member chats; the founder engine records it
    // into its real chat log through the same direct mesh ---
    member_feed.push(member_chat(2, "aye, received"));
    let _ = member_wake.send(2);
    let chat = common::await_chat_len(&a, 2, 15).await;
    assert!(
        chat.iter().any(|m| m["body"] == serde_json::json!("aye, received")
            && m["from"] == serde_json::json!("member-b")),
        "the member's chat reached the founder engine over the direct mesh: {chat:?}"
    );
}

/// **Real threshold governance over the direct mesh.** The founder engine
/// (chain-governed from its founding) proposes a gated change and co-signs it —
/// one of two, so it stays pending. The member then co-signs the exact change
/// with its **own** identity key and sends that signature over the MLS mesh;
/// the founder collects the second signature, seals a commit block, and the
/// change materializes on the Memory surface. This is the whole 2b/2c path:
/// sign → gossip → collect → commit, driven end-to-end over the transport.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn founding_governs_over_the_direct_mesh() {
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
    let b_phrase_for_sig = b_phrase.clone();
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
        agenda: "govern over the mesh".to_string(),
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

    // --- build the MEMBER's runtime supervisor on the shared hub ---
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let member_group = MlsMember::restore(&member_mls).expect("restore member MLS");
    let member_feed = MemLog::new();
    let member_sink = RecordSink::default();
    let (member_wake, member_wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 9),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        member_wake_rx,
        Some(MlsChannel::new(member_group)),
    );

    // --- the founder proposes a gated Memory change and co-signs it (1 of 2) ---
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
    // one signature is not enough — a 2-of-2 change stays pending
    assert!(
        common::read_applied(&a, Surface::Memory).await.is_empty(),
        "the founder's own signature alone must not commit a 2-of-2 change"
    );

    // --- the member co-signs the SAME change with its own key, over the mesh ---
    // derive member-b's identity EXACTLY as run_ritual_member did (its own
    // workspace id salts the identity), so the signature verifies against the
    // key the founder anchored in the roster
    let b_entropy = molt_storage::seed_entropy(&b_phrase_for_sig).expect("b entropy");
    let b_ws = molt_storage::derive_workspace_id(&b_entropy, "member");
    let (b_sk, _b_pk) = molt_storage::derive_identity_key(&b_entropy, &b_ws);
    let change = molt_core::ChainChange::Applied {
        proposal_id: pid.0,
        surface: Surface::Memory,
        payload: payload.clone(),
    };
    // the first post-genesis block is height 1
    let bytes = molt_core::approval_bytes(&sealed.republic_id, 1, &change);
    let b_sig = molt_storage::identity_sign(&b_sk, &bytes);
    let approval = EventEnvelope {
        seq: 2,
        ts: 1_751_000_200,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Approved {
            id: pid,
            by: "member-b".to_string(),
            height: 1,
            sig: b_sig,
        },
    };
    member_feed.push(approval);
    let _ = member_wake.send(2);

    // --- the founder collects the second signature, seals a block, and the
    // change materializes on the Memory surface ---
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
}

/// **Recovery link-mint + request, end to end over the minted queue.** The
/// surviving coordinator mints a recovery link (`RecoverInviteStart`) — a
/// dedicated recovery queue on its running mesh transport, a single-use ticket,
/// a rendered `molt://recover/…` link. A returning member (device lost) proves
/// its seat with a seat proof signed by its RE-DERIVED identity key and sends a
/// `RecoverRequest` on that minted queue. The coordinator's recovery recv loop
/// turns it into the internal command, verifies the proof against the anchored
/// roster key, spends the ticket, and proposes the threshold
/// `Membership{Restored}` re-admission — gossiped over the mesh, where the
/// member sees it. Supersedes the earlier injected-request test: the request now
/// flows over a real coordinator-minted link, not an engine-side injection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_flows_over_a_coordinator_minted_link() {
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
    let (a, material_rx, recovery_rx) =
        molt_engine::__spawn_manual_founding_bootstrap_recoverable(
            molt_core::GroupConfig::demo(),
            session_a,
        );
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
    let b_phrase_for_sig = b_phrase.clone();
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
        agenda: "recover over the mesh".to_string(),
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

    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let member_group = MlsMember::restore(&member_mls).expect("restore member MLS");
    let member_feed = MemLog::new();
    let member_sink = RecordSink::default();
    let (_member_wake, member_wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 11),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        member_wake_rx,
        Some(MlsChannel::new(member_group)),
    );

    // member-b lost its device; it proves its seat with its RE-DERIVED key
    let b_pk = sealed
        .identities
        .iter()
        .find(|i| i.member == "member-b")
        .expect("member-b anchored")
        .identity_pk
        .clone();
    let b_entropy = molt_storage::seed_entropy(&b_phrase_for_sig).expect("b entropy");
    let b_ws = molt_storage::derive_workspace_id(&b_entropy, "member");
    let (b_sk, _) = molt_storage::derive_identity_key(&b_entropy, &b_ws);
    let kp_hex = "abcd"; // an opaque fresh key package for this test

    // the surviving coordinator mints a recovery link for member-b — a real
    // dedicated queue on its running mesh transport, listening for the request
    a.execute(Command::RecoverInviteStart {
        member: "member-b".to_string(),
    })
    .await
    .expect("mint recovery link");
    let material = tokio::task::spawn_blocking(move || {
        recovery_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A mints the recovery link + hands the queue material out")
    })
    .await
    .expect("recovery mint blocking");
    // the minted link is a well-formed, actionable recovery link bound to the
    // republic id the coordinator carries in it
    let parsed = molt_engine::RecoveryInvite::parse(&material.link).expect("actionable link");
    assert_eq!(parsed.member, "member-b");
    assert_eq!(parsed.republic_id, sealed.republic_id);
    assert_eq!(parsed.ticket, material.ticket);

    // member-b re-derives, signs the seat proof over the MINTED ticket + the
    // republic id carried in the link, and sends the RecoverRequest on the
    // coordinator's minted queue — the recovery-ritual transport in action
    let seat_proof =
        molt_engine::make_seat_proof(&b_sk, &material.ticket, kp_hex, &material.republic_id);
    let request = invite::RitualMsg::Recover(invite::RecoverRequest {
        member: "member-b".to_string(),
        identity_pk: b_pk,
        key_package: kp_hex.to_string(),
        ticket: material.ticket.clone(),
        seat_proof,
        reply: None,
    });
    let payload = serde_json::to_vec(&request).expect("encode recover request");
    supervisor::send_framed(
        &material.transport,
        &material.recover_snd,
        &material.recover_wrap,
        msg_id("member-b", "coordinator", 1),
        &payload,
    )
    .await
    .expect("send the recovery request on the minted queue");

    // the coordinator verified the seat proof and proposed re-admission — the
    // MembershipProposed{Restored} reaches the member over the direct mesh
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let got = member_sink.messages();
        if got.iter().any(|(_, env)| {
            matches!(&env.body,
                WorkspaceEvent::MembershipProposed { member, op, .. }
                    if member == "member-b" && *op == molt_core::MembershipOp::Restored)
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the coordinator never proposed re-admission over the mesh; saw {:?}",
            got.iter().map(|(_, e)| std::mem::discriminant(&e.body)).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
