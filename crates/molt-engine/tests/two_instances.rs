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
    ChatMessage, Command, Event, EventEnvelope, MemberId, ProposalId, Reply, SessionSettings,
    SessionView, Surface, WorkspaceEvent,
};
use molt_engine::WalletHandle;
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{
    invite, msg_id, EngineSink, LoopbackHub, MlsChannel, MlsIncoming, MlsMember, NetError, PeerLink,
    QueueId, Reassembler, SndQueueAddr, Transport, WrapKey,
};
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
        relays: Vec::new(),
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
    let b_out = b_task.await.expect("B task");
    let b_pk = b_out.pk.clone();

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
    // a REAL founding must not mark its workspace as simulated — the flag
    // is the sim seam's marker, and a republic of real members carrying it
    // would (on the demo-mesh seam) grow fake peers over a real log
    assert!(
        !molt_storage::read_prefs(&dir).simulated_members,
        "a real founding persisted simulated_members = true"
    );
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
        molt_core::roster_canonical_bytes(republic_id, *rule_m, *rule_n, identities, agenda, &[]);
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

    // --- N1: the real ritual anchors a NON-EMPTY, canonical third anchor
    // for BOTH seats (the founder's is verified by nobody else — only this
    // end-to-end pin catches a broken founder-side derivation), the two
    // anchors differ (ticket-salted, no shared handle), and each side ends
    // the ritual HOLDING the private half of exactly its anchored key.
    let f_entry = identities
        .iter()
        .find(|i| i.member == "founder-a")
        .expect("founder anchored");
    for entry in [f_entry, b_entry] {
        assert_eq!(
            molt_net::canonical_nostr_pk(&entry.nostr_pk)
                .expect("a real, valid third anchor"),
            entry.nostr_pk,
            "{} anchors the one canonical byte form",
            entry.member
        );
    }
    assert_ne!(
        f_entry.nostr_pk, b_entry.nostr_pk,
        "the ticket-salted derivations yield distinct transport anchors"
    );
    // B's ritual outcome carries the secret its workspace would persist —
    // it must be the private half of B's anchored third anchor
    assert_eq!(
        molt_net::nostr_pk_for_sk(&b_out.nostr_sk).expect("a valid scalar"),
        b_entry.nostr_pk,
        "B's join-carried nostr secret pairs with B's anchored anchor"
    );
    // the founder's secret is persisted at the seal (transport.state, beside
    // identity_sk) — a non-re-derivable key, so a silent drop is permanent
    let ts = ws.read_transport_state();
    let f_sk = ts
        .nostr_sk
        .expect("the founder's nostr_sk is persisted with the seal");
    assert_eq!(
        molt_net::nostr_pk_for_sk(&f_sk).expect("a valid scalar"),
        f_entry.nostr_pk,
        "the persisted founder secret pairs with the founder's anchored anchor"
    );
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
        relays: Vec::new(),
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
        relays: Vec::new(),
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
            // a declined seat can never seal, so the run is over: the engine
            // marks it FAILED (outcome 2) — the GUI's abort button re-arms and
            // the lobby shows the terminal state instead of waiting forever
            assert_eq!(
                s.create.run.outcome, 2,
                "a decline ends the founding as failed"
            );
            assert!(
                s.create.run.log.iter().any(|l| l.contains("founded anew")),
                "the log states the ritual is over for good: {:?}",
                s.create.run.log
            );
            break;
        }
        assert_ne!(s.create.run.outcome, 1, "nothing seals when a member declines");
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
        relays: Vec::new(),
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
        relays: Vec::new(),
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

/// Part B — after founding, the founder engine stands a **real runtime
/// supervisor** up from its persisted mesh + MLS group (no founding star, no
/// demo mesh) and chats peer-to-peer over MLS with the joined member, both
/// directions. The still-alive loopback hub stands in for the members' relay
/// (its queues can't be rebuilt from state, so the runtime reuses it).
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
        relays: Vec::new(),
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
        channel: molt_core::ChannelRef::default(),
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
    member_feed.push(common::chat_env(2, "member-b", "aye, received"));
    let _ = member_wake.send(2);
    let chat = common::await_chat_len(&a, 2, 15).await;
    assert!(
        chat.iter().any(|m| m["body"] == serde_json::json!("aye, received")
            && m["from"] == serde_json::json!("member-b")),
        "the member's chat reached the founder engine over the direct mesh: {chat:?}"
    );
}

/// **Chat-bus wire semantics over the direct mesh** (B1): the founder's
/// reaction to the member's message crosses the wire carrying the stable
/// message id, and the member's deletion of its own message is honored on
/// the founder — the tombstone shows. Rides the exact
/// [`founding_chats_over_the_direct_mesh`] setup.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reactions_and_deletes_converge_across_two_instances() {
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
        relays: Vec::new(),
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
        agenda: "react over the mesh".to_string(),
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

    a.execute(Command::CreateFinish).await.expect("enter");

    // --- the member's runtime supervisor on the shared hub ---
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
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

    // founder chats, member answers — both logs converge on two messages
    a.execute(Command::Chat {
        body: "first light".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("founder chat");
    member_feed.push(common::chat_env(2, "member-b", "aye"));
    let _ = member_wake.send(2);
    common::await_chat_len(&a, 2, 15).await;

    // --- the founder reacts to the MEMBER's message; the reaction crosses
    // the wire addressed by the stable id ---
    a.execute(Command::ReactChat {
        id: common::test_msg_id(2),
        emoji: "👍".to_string(),
    })
    .await
    .expect("founder reacts");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let got = member_sink.messages();
        if got.iter().any(|(from, env)| {
            from == "founder-a"
                && matches!(
                    &env.body,
                    WorkspaceEvent::ChatReacted { id: Some(id), by, emoji, .. }
                        if *id == common::test_msg_id(2) && by == "founder-a" && emoji == "👍"
                )
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder's reaction never crossed the mesh; got {:?}",
            got.iter().map(|(_, e)| &e.body).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // and the founder's own state carries it (the local apply path)
    let chat = common::read_chat(&a).await;
    assert_eq!(
        chat[1]["reactions"]["👍"],
        serde_json::json!(["founder-a"]),
        "the founder's reaction is on the member's message: {chat:?}"
    );

    // --- the founder marks the MEMBER's message read; the read receipt
    // crosses the wire (crosses_wire + the outbox feed gate) addressed by the
    // stable id, and the founder's own state shows the green-dot member ---
    a.execute(Command::MarkRead {
        ids: vec![common::test_msg_id(2)],
    })
    .await
    .expect("founder marks read");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let got = member_sink.messages();
        if got.iter().any(|(from, env)| {
            from == "founder-a"
                && matches!(
                    &env.body,
                    WorkspaceEvent::ChatRead { ids, by }
                        if ids.contains(&common::test_msg_id(2)) && by == "founder-a"
                )
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder's read receipt never crossed the mesh; got {:?}",
            got.iter().map(|(_, e)| &e.body).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let chat = common::read_chat(&a).await;
    assert_eq!(
        chat[1]["read_by"],
        serde_json::json!(["founder-a"]),
        "the founder's read receipt is on the member's message: {chat:?}"
    );

    // --- the member deletes its OWN message; the founder honors it and
    // shows the tombstone (dropping reactions AND read receipts with it) ---
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 3,
        ts: 1_751_000_003,
        by: "member-b".to_string(),
        body: WorkspaceEvent::ChatDeleted {
            index: 0, // the member's sender-local idea of the position
            id: Some(common::test_msg_id(2)),
            by: "member-b".to_string(),
        },
    });
    let _ = member_wake.send(3);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let chat = common::read_chat(&a).await;
        if chat
            .get(1)
            .is_some_and(|m| m["deleted_by"] == serde_json::json!("member-b"))
        {
            assert_eq!(chat[1]["body"], serde_json::json!(""), "the body is wiped");
            assert!(
                chat[1].get("reactions").is_none(),
                "reactions drop with the message: {:?}",
                chat[1]
            );
            assert!(
                chat[1].get("read_by").is_none(),
                "read receipts drop with the message: {:?}",
                chat[1]
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member's delete never reached the founder: {chat:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// **P6 parking under chaos: a reaction that arrives BEFORE its message is
/// parked and applied when the message lands.** Cross-sender ordering is
/// not guaranteed (per-sender in-order only), so at the receiving engine
/// chi's reaction to ada's message can outrun ada's message itself. Two raw
/// sender supervisors feed one full engine (`net_sink`) over a loopback hub
/// with the chaos policy of the molt-net convergence suite (delay,
/// duplicate, drop + redelivery; the same seeds). The reaction is injected
/// FIRST and the target message only after a settle pause — without the
/// parking buffer the engine drops the early reaction and the final
/// reaction set never converges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reaction_arriving_before_its_message_is_parked_and_applied() {
    for seed in [3u64, 17, 40_961] {
        let tmp = tempfile::tempdir().expect("tmp");
        let root_b = tmp.path().join("node-ben");

        // the receiving engine: member "ben" on a persisted workspace whose
        // genesis roster contains both raw senders
        let seed_b =
            molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("gen"))
                .expect("entropy");
        let genesis = EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 1_751_000_000,
            by: "ben".to_string(),
            body: WorkspaceEvent::Founded {
                name: "Park Club".to_string(),
                rule_m: 2,
                rule_n: 3,
                member: "ben".to_string(),
                roster: vec!["ada".to_string(), "ben".to_string(), "chi".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: String::new(),
                relays: Vec::new(),
            },
        };
        let ws_b =
            molt_storage::create_workspace(&root_b, &seed_b, &genesis).expect("create ben");
        let id_b = ws_b.manifest.workspace.id.clone();
        drop(ws_b); // release the LOCK for the engine
        let session = SessionView {
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
        let w_ben = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session);
        w_ben
            .execute(Command::OpenWorkspace { id: id_b })
            .await
            .expect("open ben");

        // the chaotic mesh (supervisor.rs convergence-suite policy)
        let hub = LoopbackHub::new(molt_net::ChaosPolicy {
            seed,
            delay_ms: (0, 30),
            drop_pct: 20,
            duplicate_pct: 20,
            redeliver_after_ms: 60,
        });
        let members: Vec<MemberId> =
            vec!["ada".to_string(), "ben".to_string(), "chi".to_string()];
        let mut mesh = hub.full_mesh(&members).expect("mesh wiring");
        let links_ada = mesh.remove("ada").expect("ada links");
        let links_ben = mesh.remove("ben").expect("ben links");
        let links_chi = mesh.remove("chi").expect("chi links");

        let (_wake_ben, wake_ben_rx) = watch::channel(0u64);
        let _sup_ben = supervisor::spawn(
            hub.transport(),
            NetConfig::fast("ben".to_string(), links_ben, seed),
            MemLog::new(),
            MemStateStore::new(),
            w_ben.net_sink(),
            wake_ben_rx,
            None,
        );
        let ada_feed = MemLog::new();
        let (wake_ada, wake_ada_rx) = watch::channel(0u64);
        let _sup_ada = supervisor::spawn(
            hub.transport(),
            NetConfig::fast("ada".to_string(), links_ada, seed.wrapping_add(1)),
            ada_feed.clone(),
            MemStateStore::new(),
            RecordSink::default(),
            wake_ada_rx,
            None,
        );
        let chi_feed = MemLog::new();
        let (wake_chi, wake_chi_rx) = watch::channel(0u64);
        let _sup_chi = supervisor::spawn(
            hub.transport(),
            NetConfig::fast("chi".to_string(), links_chi, seed.wrapping_add(2)),
            chi_feed.clone(),
            MemStateStore::new(),
            RecordSink::default(),
            wake_chi_rx,
            None,
        );

        // chi's reaction to ada's (not yet sent!) message goes out FIRST
        let target = common::test_msg_id(42);
        chi_feed.push(EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 1_751_000_001,
            by: "chi".to_string(),
            body: WorkspaceEvent::ChatReacted {
                index: 999, // a bogus sender-local index: must not matter
                id: Some(target),
                emoji: "🎉".to_string(),
                by: "chi".to_string(),
                op: Some(molt_core::ReactOp::Add),
            },
        });
        let _ = wake_chi.send(1);
        // let the reaction (and its chaos redeliveries) land well before the
        // message exists anywhere — the out-of-order case by construction
        tokio::time::sleep(Duration::from_millis(400)).await;

        // stamped "now": an ancient ts would age the message straight out
        // of the retention read window and the assertion below reads chat
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        ada_feed.push(EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: now,
            by: "ada".to_string(),
            body: WorkspaceEvent::Chat(ChatMessage::text(
                target,
                "ada",
                "the parked target",
                now,
            )),
        });
        let _ = wake_ada.send(1);

        // convergence: ben's log holds ada's message WITH chi's reaction
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let chat = common::read_chat(&w_ben).await;
            if chat
                .first()
                .is_some_and(|m| m["reactions"]["🎉"] == serde_json::json!(["chi"]))
            {
                assert_eq!(chat.len(), 1, "exactly one message (seed {seed}): {chat:?}");
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the early reaction never converged (seed {seed}): {chat:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
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
        relays: Vec::new(),
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
    // the pending view reflects the reader's own signature: the founder
    // co-signed at propose (self_cosign), so it is not waiting on this node
    match a
        .execute(Command::ReadState {
            surface: Surface::Memory,
            channel: None,
            view: None,
        })
        .await
        .expect("read pending")
    {
        molt_core::Reply::State(s) => {
            assert_eq!(s.pending.len(), 1);
            assert!(
                s.pending[0].approved_by_me,
                "the founder's chain co-signature must reflect in approved_by_me"
            );
            // the voting row knows exactly who signed: the founder's collected
            // co-signature marks it approved, the member is still open
            let votes = &s.pending[0].votes;
            assert_eq!(votes.len(), 2, "one stance per roster member: {votes:?}");
            for v in votes {
                let expect = if v.member == "founder-a" {
                    molt_core::VoteState::Approved
                } else {
                    molt_core::VoteState::Open
                };
                assert_eq!(v.vote, expect, "stance of {}", v.member);
            }
        }
        other => panic!("unexpected: {other:?}"),
    }

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
    let approval = EventEnvelope { prev_seq: 0,
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

/// **WP2: a reopened member recovers the open governance state from the
/// mesh.** A member that closes and reopens lost the ephemeral
/// Proposed/Approved gossip with its RAM; at open it already broadcasts a
/// `ChainRequest` (chain catch-up). This test drives the ANSWER side over
/// the real mesh: the founder engine holds an open 1-of-2 proposal, the
/// member sends the exact `ChainRequest` a reopened engine records — and
/// receives the proposal AND the founder's collected co-signature back,
/// MLS-encrypted over the wire. The member then co-signs the recovered
/// change and the block seals at 2-of-2, proving the re-served state is
/// fully usable. (A literal close/reopen of a second full engine needs a
/// resumable transport — SMP, not the loopback founding hub, whose queues
/// die with the ritual transport; the requester side is exactly the open
/// path `request_catchup` already pins.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_member_recovers_open_proposals_from_the_mesh() {
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
        relays: Vec::new(),
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
        agenda: "recover open votes".to_string(),
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

    // --- the founder proposes; his self-cosign makes it 1-of-2, pending ---
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
        "1-of-2 stays pending"
    );

    // --- the member asks for catch-up: the EXACT frame a reopened engine
    // records at open (request_catchup(head+1) — genesis head, so from 1) ---
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 2,
        ts: 1_751_000_100,
        by: "member-b".to_string(),
        body: WorkspaceEvent::ChainRequest { from_height: 1 },
    });
    let _ = member_wake.send(2);

    // --- the founder's answer re-serves the open governance state: the
    // proposal and his collected co-signature arrive over the mesh ---
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let founder_sig = loop {
        let got = member_sink.messages();
        let proposal_back = got.iter().any(|(from, env)| {
            from == "founder-a"
                && matches!(&env.body,
                    WorkspaceEvent::Proposed { id, surface, payload: p }
                        if *id == pid && *surface == Surface::Memory && p == &payload)
        });
        let sig_back = got.iter().find_map(|(from, env)| match &env.body {
            WorkspaceEvent::Approved { id, by, height, sig }
                if from == "founder-a" && *id == pid && by == "founder-a" && *height == 1 =>
            {
                Some(sig.clone())
            }
            _ => None,
        });
        if let (true, Some(sig)) = (proposal_back, sig_back) {
            break sig;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the catch-up answer never re-served the open proposal; got {:?}",
            got.iter().map(|(f, e)| (f.clone(), e.body.clone())).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(!founder_sig.is_empty(), "a real position-bound signature rode back");

    // --- the recovered state is USABLE: the member co-signs the same
    // change and the founder seals the 2-of-2 block ---
    let b_entropy = molt_storage::seed_entropy(&b_phrase_for_sig).expect("b entropy");
    let b_ws = molt_storage::derive_workspace_id(&b_entropy, "member");
    let (b_sk, _b_pk) = molt_storage::derive_identity_key(&b_entropy, &b_ws);
    let change = molt_core::ChainChange::Applied {
        proposal_id: pid.0,
        surface: Surface::Memory,
        payload: payload.clone(),
    };
    let bytes = molt_core::approval_bytes(&sealed.republic_id, 1, &change);
    let b_sig = molt_storage::identity_sign(&b_sk, &bytes);
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 3,
        ts: 1_751_000_200,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Approved {
            id: pid,
            by: "member-b".to_string(),
            height: 1,
            sig: b_sig,
        },
    });
    let _ = member_wake.send(3);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let applied = common::read_applied(&a, Surface::Memory).await;
        if applied.iter().any(|v| v["title"] == serde_json::json!("minutes")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the recovered-then-approved change never committed; applied: {applied:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}


/// A real, decodable ~48 KiB BMP of non-repeating pixels (WP3: set_image
/// bytes must decode — random bytes would be dropped by the sniff). BMP is
/// uncompressed, so the size is exact and the content still exercises the
/// chunker across many 16 KiB transport blocks. `seed` varies the pixels.
///
/// Sized to stay under the DERIVED propose cap (`proposals.rs`: what one
/// kind-445 frame can carry), not under a number of its own — it used to be
/// ~150 KiB against a 256 KiB cap that the transport could never honour.
fn big_bmp(seed: u32) -> Vec<u8> {
    let (w, h) = (128u32, 128u32); // 128*128*3 = 48 KiB of pixel rows
    let row = (w * 3).div_ceil(4) * 4;
    let size = 54 + row * h;
    let mut b = Vec::with_capacity(usize::try_from(size).expect("small image"));
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&size.to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&54u32.to_le_bytes());
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&i32::try_from(w).expect("small dims").to_le_bytes());
    b.extend_from_slice(&i32::try_from(h).expect("small dims").to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    b.extend_from_slice(&[0; 24]);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x).wrapping_add(seed);
            b.extend_from_slice(&[
                u8::try_from((i.wrapping_mul(2_654_435_761) >> 13) & 0xff).unwrap_or(0),
                u8::try_from((i.wrapping_mul(2_246_822_519) >> 11) & 0xff).unwrap_or(0),
                u8::try_from(i & 0xff).unwrap_or(0),
            ]);
        }
        b.resize(b.len() + usize::try_from(row - w * 3).expect("small pad"), 0);
    }
    b
}

/// **A `set_image` proposal's bytes survive the mesh, both directions.**
/// The image rides the `Proposed` gossip itself (sign-what-you-see: every
/// member votes on the actual bytes), so a realistic ~48 KiB logo is the
/// first governance frame that must chunk across many transport blocks.
/// Founder → member: the member's supervisor delivers the founder's
/// proposal payload byte-identical. Member → founder: the founder engine
/// records the peer proposal and its pending read serves the identical,
/// base64-decodable bytes — exactly what the GUI's "click to view" decodes.
/// Pins every layer against truncation/re-encoding: outbox, MLS, chunker,
/// wrap, reassembly, and the `cmd_net_delivered` set_image guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_image_proposal_carries_its_bytes_across_the_mesh() {
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
        relays: Vec::new(),
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
        agenda: "carry the image over the mesh".to_string(),
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

    a.execute(Command::CreateFinish).await.expect("enter");

    // --- the member's runtime supervisor on the shared hub ---
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let member_group = MlsMember::restore(&member_mls).expect("restore member MLS");
    let member_feed = MemLog::new();
    let member_sink = RecordSink::default();
    let (member_wake, member_wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 13),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        member_wake_rx,
        Some(MlsChannel::new(member_group)),
    );

    // a realistic logo: a real ~48 KiB BMP of non-repeating pixels (three
    // transport blocks, under the derived propose cap, and decodable — WP3
    // refuses random bytes), base64 like the GUI
    use base64::Engine as _;
    let founder_bytes: Vec<u8> = big_bmp(0);
    let founder_b64 = base64::engine::general_purpose::STANDARD.encode(&founder_bytes);
    let payload = serde_json::json!({
        "op": "set_image",
        "title": "Set image to crest.png",
        "value": "crest.png",
        "bytes_b64": founder_b64,
    });
    let pid = match a
        .execute(Command::Propose {
            surface: Surface::Organization,
            payload: payload.clone(),
        })
        .await
        .expect("propose")
    {
        Reply::Proposed { id } => id,
        other => panic!("unexpected: {other:?}"),
    };

    // --- founder → member: the delivered gossip carries the identical,
    // decodable bytes ---
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    'founder_to_member: loop {
        for (from, env) in member_sink.messages() {
            if let WorkspaceEvent::Proposed { id, surface, payload } = &env.body {
                if *id == pid {
                    assert_eq!(from, "founder-a", "the gossip is link-authenticated");
                    assert_eq!(*surface, Surface::Organization);
                    let got = payload["bytes_b64"].as_str().expect("bytes_b64 is a string");
                    assert_eq!(
                        got, founder_b64,
                        "the member must receive the founder's image bytes verbatim"
                    );
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(got)
                        .expect("the delivered payload base64-decodes");
                    assert_eq!(decoded, founder_bytes);
                    break 'founder_to_member;
                }
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder's set_image proposal never reached the member; got {:?}",
            member_sink
                .messages()
                .iter()
                .map(|(_, e)| std::mem::discriminant(&e.body))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // --- member → founder: the peer proposal lands in the founder's
    // pending read with the identical bytes (what the GUI preview decodes) ---
    let member_bytes: Vec<u8> = big_bmp(7);
    let member_b64 = base64::engine::general_purpose::STANDARD.encode(&member_bytes);
    let mut a_ev = a.subscribe();
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 2,
        ts: 1_751_000_300,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Proposed {
            id: ProposalId(7),
            surface: Surface::Organization,
            payload: serde_json::json!({
                "op": "set_image",
                "title": "Set image to seal.png",
                "value": "seal.png",
                "bytes_b64": member_b64,
            }),
        },
    });
    let _ = member_wake.send(2);
    // the streaming event names the WIRE proposer — the GUI's alert-sound
    // gate (only somebody else's vote rings)
    let ev_by = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(Event::Proposed { id, by, .. }) = a_ev.recv().await {
                if id == ProposalId(7) {
                    return by;
                }
            }
        }
    })
    .await
    .expect("the founder streams the member's proposal");
    assert_eq!(ev_by, "member-b", "a wire proposal is attributed to its sender");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let pending = match a
            .execute(Command::ReadState {
                surface: Surface::Organization,
                channel: None,
                view: None,
            })
            .await
            .expect("read pending")
        {
            Reply::State(s) => s.pending,
            other => panic!("unexpected: {other:?}"),
        };
        if let Some(p) = pending.iter().find(|p| p.id == ProposalId(7)) {
            let got = p.payload["bytes_b64"].as_str().expect("bytes_b64 is a string");
            assert_eq!(
                got, member_b64,
                "the founder's pending read must serve the member's image bytes verbatim"
            );
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(got)
                .expect("the pending payload base64-decodes");
            assert_eq!(decoded, member_bytes);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the member's set_image proposal never reached the founder; pending ids: {:?}",
            pending.iter().map(|p| p.id).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // --- a re-served DUPLICATE must not re-announce: WP2 catch-up re-wraps
    // open proposals under the SERVING peer's name, so a second Proposed
    // for a known id would re-ring every GUI's vote alert after each
    // rejoin. Only a genuinely new insert may emit; the next streamed
    // Proposed must therefore be the fresh id 8, never id 7 again. ---
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 3,
        ts: 1_751_000_400,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Proposed {
            id: ProposalId(7),
            surface: Surface::Organization,
            payload: serde_json::json!({
                "op": "set_image",
                "title": "Set image to seal.png",
                "value": "seal.png",
                "bytes_b64": member_b64,
            }),
        },
    });
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 4,
        ts: 1_751_000_500,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Proposed {
            id: ProposalId(8),
            surface: Surface::Organization,
            payload: serde_json::json!({
                "op": "set_name",
                "title": "Namen ändern",
                "value": "Nach dem Duplikat",
            }),
        },
    });
    let _ = member_wake.send(3);
    let next = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(Event::Proposed { id, .. }) = a_ev.recv().await {
                return id;
            }
        }
    })
    .await
    .expect("the fresh proposal streams");
    assert_eq!(
        next,
        ProposalId(8),
        "a deduplicated re-serve must not re-emit Proposed"
    );
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
        relays: Vec::new(),
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
        molt_engine::make_seat_proof(&b_sk, &material.ticket, kp_hex, &material.republic_id, "");
    let request = invite::RitualMsg::Recover(invite::RecoverRequest {
        new_nostr_pk: String::new(),
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

/// **The recovery capstone: the whole ritual end-to-end, across instances.**
/// A republic is founded 1-of-2 (threshold 1, so the lone surviving
/// coordinator's self-cosign commits the re-admission — the lightest vehicle
/// that exercises the REAL committed-block path). Member-b loses its device.
/// The coordinator mints a recovery link; the rejoiner drives the real
/// `run_rejoin` against it — seat proof, threshold `Restored` block, MLS
/// re-key, Welcome + the full chain served back. Then a THIRD engine (the
/// rejoiner's fresh device) materializes the recovered workspace through the
/// engine lifecycle (`RecoverStart` → `NetRecoverSealed`). This is the
/// orchestration test `coordinator_rekey`'s pieces were waiting for: the
/// commit → re-key → welcome → catch-up → materialize chain fires on a real
/// committed block, not on injected fixtures.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_completes_end_to_end_and_the_rejoiner_materializes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("coordinator").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx, recovery_rx) =
        molt_engine::__spawn_manual_founding_bootstrap_recoverable(
            molt_core::GroupConfig::demo(),
            session_a,
        );
    // 1-of-2: the coordinator alone reaches the threshold (self-cosign), so
    // the Restored block COMMITS with member-b's device gone
    a.execute(Command::CreateStart {
        name: "Guild".to_string(),
        member: "founder-a".to_string(),
        threshold: 1,
        members: 2,
        relays: Vec::new(),
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

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_join = b_phrase.clone();
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_join, true, true, None, None)
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
        agenda: "survive total loss".to_string(),
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
    // member-b's device completes the founding — and is then LOST (its
    // workspace, mesh and MLS state are simply never used again)
    let _lost_device = b_task.await.expect("B task");
    a.execute(Command::CreateFinish).await.expect("enter");

    // ❶ the surviving coordinator mints the recovery link
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
    let inv = molt_engine::RecoveryInvite::parse(&material.link).expect("actionable link");

    // ❷–❻ the rejoiner drives the REAL recovery: seat proof over the minted
    // ticket, coordinator verifies + proposes, the 1-of-2 self-cosign COMMITS
    // the Restored block, coordinator_rekey fires (restore_member → commit
    // broadcast + welcome + chain served), run_rejoin re-enters the group,
    // verifies the served chain from its genesis, and — dynamic mesh
    // membership — re-establishes its per-pair link to the coordinator
    let rejoin_transport = material.transport.clone();
    let rejoin_phrase = b_phrase.clone();
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::spawn(async move {
            molt_engine::run_rejoin(rejoin_transport, inv, &rejoin_phrase, true).await
        }),
    )
    .await
    .expect("the rejoin finishes in time")
    .expect("rejoin task")
    .expect("the rejoin succeeds");

    // the served chain is the coordinator's REAL committed chain: the genesis
    // plus the threshold-committed Restored block
    assert_eq!(outcome.member, "member-b");
    assert_eq!(outcome.chain.len(), 2, "genesis + the committed Restored block");
    assert!(
        matches!(
            &outcome.chain[1].change,
            molt_core::ChainChange::Membership {
                op: molt_core::MembershipOp::Restored,
                member,
                ..
            } if member == "member-b"
        ),
        "block 1 is the re-admission: {:?}",
        outcome.chain[1].change
    );
    assert!(!outcome.mls_snapshot.is_empty(), "the rejoiner is back in the group");

    // the coordinator announced the rejoin in the group chat (recorded
    // synchronously when the block committed, before the welcome went out)
    let chat = common::read_chat(&a).await;
    let notice = chat
        .iter()
        .find(|m| {
            m["body"]
                .as_str()
                .is_some_and(|b| b.contains("member-b rejoined the republic after recovery"))
        })
        .unwrap_or_else(|| panic!("the rejoin notice is in the coordinator's chat: {chat:?}"));
    // ... and it carries the system kind through the read surface, so every
    // frontend renders it as a quiet system line, never as member speech
    assert_eq!(
        notice["kind"].as_str(),
        Some("system"),
        "the rejoin notice is a ChatKind::System row: {notice:?}"
    );

    // DYNAMIC MESH: the rejoiner assembled a fresh per-pair link to the
    // coordinator, and the coordinator folded its own fresh link to the
    // rejoiner into its RUNNING supervisor (rebuild, replacing the stale
    // lost-device link) — proven by LIVE bidirectional chat over the new queues
    assert_eq!(outcome.mesh.len(), 1, "one re-established link, to the coordinator");
    assert_eq!(outcome.mesh[0].member, "founder-a");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if read_session(&a).await.notice == "mesh-extended:member-b" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the coordinator never folded the rejoiner into its mesh"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let links: Vec<PeerLink> = outcome.mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let r_group = MlsMember::restore(&outcome.mls_snapshot).expect("restore rejoiner MLS");
    let r_feed = MemLog::new();
    let r_sink = RecordSink::default();
    let (r_wake, r_wake_rx) = watch::channel(0u64);
    let _r_sup = supervisor::spawn(
        material.transport.clone(),
        NetConfig::fast("member-b".to_string(), links, 13),
        r_feed.clone(),
        MemStateStore::new(),
        r_sink.clone(),
        r_wake_rx,
        Some(MlsChannel::new(r_group)),
    );
    a.execute(Command::Chat {
        body: "welcome back to the mesh".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("coordinator chats");
    wait_for(
        &r_sink,
        |(_, env)| {
            matches!(&env.body, WorkspaceEvent::Chat(m) if m.body == "welcome back to the mesh")
        },
        "the coordinator's chat to reach the rejoiner over the NEW link",
    )
    .await;
    r_feed.push(common::chat_env(1, "member-b", "alive on fresh queues"));
    let _ = r_wake.send(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let chat = common::read_chat(&a).await;
        if chat.iter().any(|m| m["body"].as_str() == Some("alive on fresh queues")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the rejoiner's chat never reached the coordinator over the new link"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ❼–❽ the rejoiner's FRESH DEVICE materializes the recovered workspace
    // through the engine lifecycle. RecoverStart arms the context (link +
    // phrase); its background SMP task cannot run over the loopback hub, so
    // the real outcome above is fed back as the internal command — exactly
    // what the off-actor task reports over SMP.
    let session_c = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("rejoiner").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let c = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session_c);
    c.execute(Command::RecoverStart {
        link: material.link.clone(),
        phrase: b_phrase.clone(),
    })
    .await
    .expect("recover start");
    c.execute(Command::NetRecoverSealed {
        member: outcome.member.clone(),
        chain: serde_json::to_string(&outcome.chain).expect("chain json"),
        mls: hex::encode(&outcome.mls_snapshot),
        // the REAL re-established mesh: engine C seals it into the recovered
        // transport.state (its own rejoin transport slot is empty on this
        // injection seam, so no live supervisor stands up here — over real
        // SMP, RecoverStart's own task fills the slot and the net comes up)
        mesh: outcome.mesh.clone(),
        nostr_sk: String::new(),
        rotation_seed: String::new(),
        generation: Some(1),
    })
    .await
    .expect("recover sealed");
    let s = read_session(&c).await;
    assert_eq!(s.screen, molt_core::Screen::Main, "the rejoiner entered the republic");
    let ws = s
        .workspaces
        .iter()
        .find(|w| w.name == "Guild")
        .expect("the recovered workspace is listed");
    assert_eq!(s.active_workspace, ws.id);
    assert_eq!(ws.agenda, "survive total loss");
    assert_eq!(ws.members.len(), 2, "the full roster came back from the chain");

    // PRESENCE HONESTY: recovery is NOT a full-roster live seal. The peers the
    // re-established mesh actually reached (here the coordinator, over the fresh
    // per-pair link) and the returning seat itself show a real sighting.
    let pill = |ws: &molt_core::WorkspaceInfo, name: &str| -> molt_core::MemberInfo {
        ws.members.iter().find(|m| m.name == name).expect("member pill").clone()
    };
    let founder = pill(ws, "founder-a");
    assert_ne!(
        founder.last_seen,
        molt_core::MemberInfo::NEVER,
        "the coordinator re-meshed with us — a real sighting stamps it"
    );
    assert_eq!(founder.state, 0, "the re-meshed coordinator is online");
    assert_eq!(pill(ws, "member-b").state, 0, "the returning seat itself is online");

    // ...and the negative: a recovery that re-establishes NO live mesh (option
    // A — state restored, no live links) has nobody to stamp but the returning
    // seat. Every survivor comes back honestly NEVER-seen, never a fabricated
    // "seen just now". A fresh device recovers the SAME seat from the SAME
    // verified outcome but with an empty mesh.
    let session_d = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("rejoiner-d").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let d = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session_d);
    d.execute(Command::RecoverStart {
        link: material.link.clone(),
        phrase: b_phrase.clone(),
    })
    .await
    .expect("recover start (option A)");
    d.execute(Command::NetRecoverSealed {
        member: outcome.member.clone(),
        chain: serde_json::to_string(&outcome.chain).expect("chain json"),
        mls: hex::encode(&outcome.mls_snapshot),
        mesh: Vec::new(), // option A: no live links re-established
        nostr_sk: String::new(),
        rotation_seed: String::new(),
        generation: Some(1),
    })
    .await
    .expect("recover sealed (option A)");
    let sd = read_session(&d).await;
    let wd = sd.workspaces.iter().find(|w| w.name == "Guild").expect("recovered ws (D)");
    let founder_d = pill(wd, "founder-a");
    assert_eq!(
        founder_d.last_seen,
        molt_core::MemberInfo::NEVER,
        "no mesh re-established → the survivor is honestly never-seen, not 'just now'"
    );
    assert_eq!(founder_d.state, 2, "a never-seen survivor's pill is offline");
    assert_eq!(pill(wd, "member-b").state, 0, "the returning seat itself is still online");
}

/// **The re-mint failover (decision A1, 2026-07-11): a COMPLETE second
/// recovery round after a first attempt died.** When the recovery coordinator
/// dies — before or after the `Restored` block committed — the sanctioned
/// failover is RE-MINT: any survivor mints a NEW recovery link and a full
/// second round runs, producing a SECOND `Restored` block for the same seat
/// (same anchored identity; only the MLS leaf re-keys again).
///
/// Founded 1-of-2 like the capstone test. ROUND 1 is the attempt that dies
/// from the REJOINER's perspective: a hand-driven `RecoverRequest` with a REAL
/// fresh KeyPackage whose reply queue is never read — the coordinator
/// verifies, proposes, self-cosigns, COMMITS Restored #1 and re-keys, and the
/// Welcome lands unread (the rejoiner "died" waiting). ROUND 2 mints a second
/// link (a second single-use ticket coexists with the first) and drives the
/// REAL `run_rejoin` against it with the same phrase: the coordinator's
/// `restore_member` must remove the MLS leaf added in round 1 and the chain
/// ends genesis + TWO `Membership{Restored}` blocks for the seat. A fresh
/// engine then materializes from that chain, exactly like the capstone tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_recovery_round_after_a_dead_first_attempt_succeeds() {
    let tmp = tempfile::tempdir().expect("tmp");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("coordinator").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx, recovery_rx) =
        molt_engine::__spawn_manual_founding_bootstrap_recoverable(
            molt_core::GroupConfig::demo(),
            session_a,
        );
    // 1-of-2: the coordinator alone reaches the threshold (self-cosign), so a
    // Restored block COMMITS with member-b's device gone
    a.execute(Command::CreateStart {
        name: "Guild".to_string(),
        member: "founder-a".to_string(),
        threshold: 1,
        members: 2,
        relays: Vec::new(),
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

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_join = b_phrase.clone();
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_join, true, true, None, None)
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
        agenda: "survive a dead recovery round".to_string(),
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
    // member-b's device completes the founding — and is then LOST
    let _lost_device = b_task.await.expect("B task");
    a.execute(Command::CreateFinish).await.expect("enter");

    // ── ROUND 1: the recovery attempt that DIES from the rejoiner's side ──
    a.execute(Command::RecoverInviteStart {
        member: "member-b".to_string(),
    })
    .await
    .expect("mint the first recovery link");
    let (material1, recovery_rx) = tokio::task::spawn_blocking(move || {
        let m = recovery_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A mints the first recovery link");
        (m, recovery_rx)
    })
    .await
    .expect("first recovery mint blocking");

    // the doomed rejoiner's half, hand-driven: the SAME re-derived identity a
    // real `run_rejoin` would use, a REAL fresh KeyPackage from it — so the
    // coordinator's `restore_member` adds a genuine leaf round 2 must evict —
    // and a reply queue that is NEVER read (the rejoiner dies waiting)
    let (b_sk, b_pk) = member_identity(&b_phrase);
    let ghost = MlsMember::new(&b_sk, "member-b").expect("round-1 mls member");
    let kp_hex = hex::encode(ghost.key_package().expect("round-1 fresh key package"));
    let dead_reply_q = material1
        .transport
        .create_queue()
        .await
        .expect("round-1 reply queue (never read)");
    let dead_reply_wrap = WrapKey::fresh().expect("round-1 reply wrap");
    let seat_proof =
        molt_engine::make_seat_proof(&b_sk, &material1.ticket, &kp_hex, &material1.republic_id, "");
    let request = invite::RitualMsg::Recover(invite::RecoverRequest {
        new_nostr_pk: String::new(),
        member: "member-b".to_string(),
        identity_pk: b_pk,
        key_package: kp_hex,
        ticket: material1.ticket.clone(),
        seat_proof,
        reply: Some(invite::ReplyHandover {
            server: dead_reply_q.snd.server.clone(),
            queue_id: hex::encode(&dead_reply_q.snd.id.0),
            wrap: hex::encode(dead_reply_wrap.to_bytes()),
        }),
    });
    supervisor::send_framed(
        &material1.transport,
        &material1.recover_snd,
        &material1.recover_wrap,
        msg_id("member-b", "coordinator", 1),
        &serde_json::to_vec(&request).expect("encode round-1 request"),
    )
    .await
    .expect("send the round-1 recovery request");

    // the coordinator verified, proposed, self-cosigned, COMMITTED Restored #1
    // and re-keyed — its post-commit chat notice is the commit-happened signal
    // (the Welcome went to the unread queue; round 1 is now dead)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let chat = common::read_chat(&a).await;
        if chat.iter().any(|m| m["body"]
            .as_str()
            .is_some_and(|b| b.contains("member-b rejoined the republic after recovery")))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "round 1 never committed (no rejoin notice): {chat:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ── ROUND 2: the re-mint failover — a full second round, for real ──
    // a second mint for the same seat works while round 1's queue still
    // listens, and a second single-use ticket coexists with the first
    a.execute(Command::RecoverInviteStart {
        member: "member-b".to_string(),
    })
    .await
    .expect("mint the second recovery link");
    let material2 = tokio::task::spawn_blocking(move || {
        recovery_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A mints the second recovery link")
    })
    .await
    .expect("second recovery mint blocking");
    assert_ne!(material2.ticket, material1.ticket, "a fresh single-use ticket");
    let inv2 = molt_engine::RecoveryInvite::parse(&material2.link).expect("actionable link");

    // the REAL rejoiner drives the whole ritual against the second link with
    // the SAME phrase; the coordinator's restore_member must evict the leaf
    // round 1 added (removal is by credential handle) and re-key again
    let rejoin_transport = material2.transport.clone();
    let rejoin_phrase = b_phrase.clone();
    let outcome = tokio::time::timeout(
        Duration::from_secs(40),
        tokio::spawn(async move {
            molt_engine::run_rejoin(rejoin_transport, inv2, &rejoin_phrase, true).await
        }),
    )
    .await
    .expect("the second rejoin finishes in time")
    .expect("rejoin task")
    .expect("the second recovery round succeeds");

    // the served chain records BOTH rounds: genesis + two Restored blocks for
    // the same seat (the chain-level twin pins verify_chain's acceptance)
    assert_eq!(outcome.member, "member-b");
    assert_eq!(
        outcome.chain.len(),
        3,
        "genesis + TWO committed Restored blocks: {:?}",
        outcome.chain.iter().map(|b| &b.change).collect::<Vec<_>>()
    );
    for h in [1usize, 2] {
        assert!(
            matches!(
                &outcome.chain[h].change,
                molt_core::ChainChange::Membership {
                    op: molt_core::MembershipOp::Restored,
                    member,
                    ..
                } if member == "member-b"
            ),
            "block {h} is a re-admission of member-b: {:?}",
            outcome.chain[h].change
        );
    }
    assert!(!outcome.mls_snapshot.is_empty(), "the rejoiner is back in the group");
    assert_eq!(outcome.mesh.len(), 1, "one re-established link, to the coordinator");

    // ── the rejoiner's fresh device materializes from the round-2 outcome,
    // exactly like the capstone test's tail ──
    let session_c = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("rejoiner").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let c = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session_c);
    c.execute(Command::RecoverStart {
        link: material2.link.clone(),
        phrase: b_phrase.clone(),
    })
    .await
    .expect("recover start");
    c.execute(Command::NetRecoverSealed {
        member: outcome.member.clone(),
        chain: serde_json::to_string(&outcome.chain).expect("chain json"),
        mls: hex::encode(&outcome.mls_snapshot),
        mesh: outcome.mesh.clone(),
        nostr_sk: String::new(),
        rotation_seed: String::new(),
        generation: Some(1),
    })
    .await
    .expect("recover sealed");
    let s = read_session(&c).await;
    assert_eq!(s.screen, molt_core::Screen::Main, "the rejoiner entered the republic");
    let ws = s
        .workspaces
        .iter()
        .find(|w| w.name == "Guild")
        .expect("the recovered workspace is listed");
    assert_eq!(s.active_workspace, ws.id);
    assert_eq!(ws.members.len(), 2, "the full roster came back from the chain");
}

/// **Recovery with a LIVE survivor: the re-key commit reaches the mesh.** A
/// republic of three (1-of-3): the coordinator, member-c (a survivor whose
/// runtime supervisor keeps running), and member-b (device lost). The
/// coordinator re-admits b through the full ritual; the survivor must live
/// through the re-key — the engine's `coordinator_rekey` broadcasts the raw
/// `MlsCommit` over the runtime mesh, c's receive path merges it, and c then
/// decrypts the post-re-key chat notice (an epoch-N+1 message c could only
/// read if it applied the commit). This is the ENGINE-path twin of
/// `a_rekey_commit_broadcast_over_the_mesh_keeps_survivors_in_epoch` (which
/// proved the mechanism on raw supervisors): here the broadcast is driven by
/// a real committed `Restored` block inside the coordinator's engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_distributes_the_rekey_commit_to_a_live_survivor() {
    let tmp = tempfile::tempdir().expect("tmp");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("coordinator").display().to_string(),
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
        threshold: 1,
        members: 3,
        relays: Vec::new(),
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
    let mut seats = materials.into_iter();
    let seat_b = seats.next().expect("seat for member-b");
    let seat_c = seats.next().expect("seat for member-c");
    let c_hub = seat_c.transport.clone();

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let c_phrase = molt_storage::generate_seed_phrase().expect("c phrase");
    let b_join = b_phrase.clone();
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat_b, "member-b".to_string(), b_join, true, true, None, None)
            .await
            .expect("B completes the member side + bootstrap")
    });
    let c_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat_c, "member-c".to_string(), c_phrase, true, true, None, None)
            .await
            .expect("C completes the member side + bootstrap")
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the members never joined");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Guild".to_string(),
        agenda: "survive together".to_string(),
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
    let _lost_device = b_task.await.expect("B task");
    let c_outcome = c_task.await.expect("C task");
    a.execute(Command::CreateFinish).await.expect("enter");

    // the SURVIVOR stands its runtime supervisor up from its founded mesh +
    // MLS group and stays online through the whole recovery. Its group is
    // shared with the test (from_shared): member-c is a raw supervisor, so the
    // test code below plays its engine half of the mesh re-join.
    let c_mesh = c_outcome.mesh.expect("C assembled its direct mesh");
    let c_mls = c_outcome.mls_snapshot.expect("C post-bootstrap snapshot");
    let links: Vec<PeerLink> = c_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let c_group = std::sync::Arc::new(std::sync::Mutex::new(
        MlsMember::restore(&c_mls).expect("restore C's MLS"),
    ));
    let c_sink = RecordSink::default();
    let (_c_wake, c_wake_rx) = watch::channel(0u64);
    let _c_sup = supervisor::spawn(
        c_hub.clone(),
        NetConfig::fast("member-c".to_string(), links, 11),
        MemLog::new(),
        MemStateStore::new(),
        c_sink.clone(),
        c_wake_rx,
        Some(MlsChannel::from_shared(c_group.clone())),
    );

    // the coordinator mints; the rejoiner drives the real recovery
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
    let inv = molt_engine::RecoveryInvite::parse(&material.link).expect("actionable link");
    // member-c's engine half of the mesh re-join, played by the test (c is a
    // raw supervisor): when the coordinator's relayed MeshAnnounced lands in
    // c's sink, decrypt it (authenticating bob), create the own fresh queue,
    // and reply MLS-encrypted directly onto the queue bob announced for c —
    // exactly what a survivor ENGINE does (pinned separately in
    // a_survivor_folds_a_relayed_mesh_announce_into_its_running_mesh)
    let c_reply_sink = c_sink.clone();
    let c_reply_group = c_group.clone();
    let c_reply_hub = c_hub.clone();
    let c_reply = tokio::spawn(async move {
        use molt_net::mesh;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let ct = loop {
            let got = c_reply_sink.messages().into_iter().find_map(|(_, env)| {
                if let WorkspaceEvent::MeshAnnounced { ct, .. } = env.body {
                    hex::decode(&ct).ok()
                } else {
                    None
                }
            });
            if let Some(ct) = got {
                break ct;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the relayed mesh announce never reached the survivor"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let molt_net::MlsIncoming::Application { from, plaintext } = c_reply_group
            .lock()
            .expect("c group")
            .decrypt(&ct)
            .expect("decrypt the relayed announce")
        else {
            panic!("the relayed announce is an application message");
        };
        assert_eq!(from, "member-b", "the announce authenticates as the rejoiner");
        let a: mesh::MeshAnnounce = serde_json::from_slice(&plaintext).expect("announce");
        let target = a.queues.get("member-c").expect("a queue for member-c");
        let own_q = c_reply_hub.create_queue().await.expect("c's queue for bob");
        let own_wrap = WrapKey::fresh().expect("wrap");
        let mut queues = std::collections::BTreeMap::new();
        queues.insert(
            "member-b".to_string(),
            mesh::QueueHandover::of(&own_q.snd, &own_wrap),
        );
        let reply = mesh::MeshAnnounce { queues };
        let ct = c_reply_group
            .lock()
            .expect("c group")
            .encrypt(&serde_json::to_vec(&reply).expect("encode"))
            .expect("encrypt reply");
        let msg = invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
        let payload = serde_json::to_vec(&msg).expect("payload");
        supervisor::send_framed(
            &c_reply_hub,
            &target.addr().expect("addr"),
            &target.wrap_key().expect("wrap"),
            msg_id("member-c", "member-b", 1),
            &payload,
        )
        .await
        .expect("c's reply reaches bob's announced queue");
    });
    let rejoin_transport = material.transport.clone();
    let rejoin_phrase = b_phrase.clone();
    let outcome = tokio::time::timeout(
        Duration::from_secs(40),
        tokio::spawn(async move {
            molt_engine::run_rejoin(rejoin_transport, inv, &rejoin_phrase, true).await
        }),
    )
    .await
    .expect("the rejoin finishes in time")
    .expect("rejoin task")
    .expect("the rejoin succeeds");
    c_reply.await.expect("c's reply half");
    assert_eq!(outcome.chain.len(), 2, "genesis + the committed Restored block");
    // the rejoiner re-established links to BOTH survivors — the coordinator
    // (its engine replied) and member-c (the relayed announce reached it)
    let mut mesh_members: Vec<&str> =
        outcome.mesh.iter().map(|l| l.member.as_str()).collect();
    mesh_members.sort_unstable();
    assert_eq!(mesh_members, vec!["founder-a", "member-c"]);

    // the survivor receives the committed Restored block over the mesh …
    wait_for(
        &c_sink,
        |(_, env)| {
            matches!(&env.body,
                WorkspaceEvent::Committed(b)
                    if matches!(&b.change,
                        molt_core::ChainChange::Membership {
                            op: molt_core::MembershipOp::Restored,
                            member,
                            ..
                        } if member == "member-b"))
        },
        "the Restored block to reach the survivor",
    )
    .await;
    // … and — having merged the broadcast raw MlsCommit — decrypts the chat
    // notice the coordinator posted AT THE NEW EPOCH: the proof the re-key
    // commit was distributed live and applied
    wait_for(
        &c_sink,
        |(_, env)| {
            matches!(&env.body,
                WorkspaceEvent::Chat(m)
                    if m.body.contains("member-b rejoined the republic after recovery"))
        },
        "the post-re-key chat notice to decrypt at the survivor",
    )
    .await;
}

/// **Dynamic mesh membership, survivor side.** A relayed
/// `WorkspaceEvent::MeshAnnounced` arrives at a survivor ENGINE over the
/// runtime mesh; the engine authenticates the announcer by MLS decryption (the
/// event author is only the relay), creates a fresh per-pair queue, replies
/// with its own announce directly onto the announced queue, and REBUILDS its
/// running supervisor with the new link — replacing the stale one. Proven by
/// live bidirectional chat over the rotated queues. (Here the announcer is an
/// existing peer re-keying its own link — the same code path a recovery
/// rejoiner's relayed announce takes, and the §4 queue-rotation shape.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_survivor_folds_a_relayed_mesh_announce_into_its_running_mesh() {
    use molt_net::mesh;
    use std::sync::{Arc, Mutex};

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
        relays: Vec::new(),
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
        agenda: "rotate the queues".to_string(),
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
    a.execute(Command::CreateFinish).await.expect("enter");

    // member-b's runtime supervisor over the founded mesh — its group is
    // SHARED with the test (from_shared) so the test can author the announce
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let b_group = Arc::new(Mutex::new(
        MlsMember::restore(&member_mls).expect("restore member MLS"),
    ));
    let member_feed = MemLog::new();
    let member_sink = RecordSink::default();
    let (member_wake, member_wake_rx) = watch::channel(0u64);
    let member_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), links, 7),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        member_wake_rx,
        Some(MlsChannel::from_shared(b_group.clone())),
    );

    // member-b re-keys its own link: a fresh inbound queue for the founder,
    // announced as a MeshAnnounced event over the RUNNING mesh (in a recovery
    // this ciphertext would be the coordinator's verbatim relay of a rejoiner)
    let new_q = hub.create_queue().await.expect("b's fresh queue");
    let new_wrap = WrapKey::fresh().expect("fresh wrap");
    let mut queues = std::collections::BTreeMap::new();
    queues.insert(
        "founder-a".to_string(),
        mesh::QueueHandover::of(&new_q.snd, &new_wrap),
    );
    let announce = mesh::MeshAnnounce { queues };
    let ct = b_group
        .lock()
        .expect("b group")
        .encrypt(&serde_json::to_vec(&announce).expect("encode"))
        .expect("encrypt announce");
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 2,
        ts: 1_751_000_002,
        by: "member-b".to_string(),
        body: WorkspaceEvent::MeshAnnounced { ct: hex::encode(&ct), nonce: None },
    });
    let _ = member_wake.send(2);

    // the survivor engine folds the announce in: fresh queue, direct reply,
    // supervisor rebuild — surfaced as the mesh-extended notice
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.notice == "mesh-extended:member-b" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the survivor never folded the announce into its mesh"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // the survivor's reply announce is the FIRST frame on the announced queue
    let mut rx = hub.subscribe(&new_q.rcv).await.expect("subscribe b's fresh queue");
    let mut reasm = Reassembler::new();
    let reply_ct = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let d = rx.recv().await.expect("queue open");
            let Ok(plain) = molt_net::wrap::unwrap_block(&new_wrap, &d.block) else {
                d.ack.ack();
                continue;
            };
            let out = reasm.push(&plain);
            d.ack.ack();
            if let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = out {
                if let Ok(invite::RitualMsg::MeshAnnounce { ct }) =
                    serde_json::from_slice::<invite::RitualMsg>(&bytes)
                {
                    break hex::decode(&ct).expect("reply hex");
                }
            }
        }
    })
    .await
    .expect("the survivor's reply reaches the announced queue");
    // tear b's OLD supervisor down BEFORE touching the shared ratchet again
    member_sup.shutdown();
    let molt_net::MlsIncoming::Application { from, plaintext } = b_group
        .lock()
        .expect("b group")
        .decrypt(&reply_ct)
        .expect("decrypt the survivor's reply")
    else {
        panic!("the reply is an application message");
    };
    assert_eq!(from, "founder-a", "the reply is MLS-authenticated");
    let reply: mesh::MeshAnnounce = serde_json::from_slice(&plaintext).expect("reply announce");
    let target = reply.queues.get("member-b").expect("a queue for member-b");

    // member-b's ROTATED link: send to the survivor's fresh queue, receive on
    // the queue it announced — a second supervisor runs over it
    let rotated = PeerLink {
        member: "founder-a".to_string(),
        snds: vec![target.addr().expect("addr")],
        wrap_out: target.wrap_key().expect("wrap"),
        rcvs: vec![new_q.rcv.clone()],
        wrap_in: new_wrap.clone(),
    };
    let feed2 = MemLog::new();
    let sink2 = RecordSink::default();
    let (wake2, wake2_rx) = watch::channel(0u64);
    let _sup2 = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), vec![rotated], 9),
        feed2.clone(),
        MemStateStore::new(),
        sink2.clone(),
        wake2_rx,
        Some(MlsChannel::from_shared(b_group.clone())),
    );

    // live chat BOTH ways over the rotated queues
    a.execute(Command::Chat {
        body: "over the rotated link".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("founder chats");
    wait_for(
        &sink2,
        |(_, env)| {
            matches!(&env.body, WorkspaceEvent::Chat(m) if m.body == "over the rotated link")
        },
        "the survivor's chat to arrive over the ROTATED link",
    )
    .await;
    feed2.push(common::chat_env(1, "member-b", "rotation complete"));
    let _ = wake2.send(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let chat = common::read_chat(&a).await;
        if chat.iter().any(|m| m["body"].as_str() == Some("rotation complete")) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "member-b's chat never reached the survivor over the rotated link"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // RATE LIMIT: an immediate SECOND announce from the same member must not
    // trigger another teardown+rebuild+fsync round (a member could otherwise
    // churn every peer's supervisor at will) — within the cooldown it is
    // ignored: no reply lands on the newly announced queue
    let third_q = hub.create_queue().await.expect("b's third queue");
    let third_wrap = WrapKey::fresh().expect("third wrap");
    let mut queues = std::collections::BTreeMap::new();
    queues.insert(
        "founder-a".to_string(),
        mesh::QueueHandover::of(&third_q.snd, &third_wrap),
    );
    let announce = mesh::MeshAnnounce { queues };
    let ct = b_group
        .lock()
        .expect("b group")
        .encrypt(&serde_json::to_vec(&announce).expect("encode"))
        .expect("encrypt second announce");
    feed2.push(EventEnvelope { prev_seq: 0,
        seq: 2,
        ts: 1_751_000_003,
        by: "member-b".to_string(),
        body: WorkspaceEvent::MeshAnnounced { ct: hex::encode(&ct), nonce: None },
    });
    let _ = wake2.send(2);
    let mut rx3 = hub.subscribe(&third_q.rcv).await.expect("subscribe third queue");
    let got_reply = tokio::time::timeout(Duration::from_secs(3), rx3.recv()).await.is_ok();
    assert!(
        !got_reply,
        "a second announce within the cooldown must be ignored (no reply, no rebuild)"
    );
}

/// The `member_identity` derivation (`entropy → workspace-id "member" → key`),
/// reproduced here because the engine keeps it `pub(crate)`. The rejoiner
/// (`run_rejoin`) derives its key this way internally; the test derives the same
/// key to check the seat proof, exactly as a coordinator reads the anchored key.
fn member_identity(phrase: &str) -> (molt_storage::SigningKey, String) {
    let entropy = molt_storage::seed_entropy(phrase).expect("entropy");
    let ws = molt_storage::derive_workspace_id(&entropy, "member");
    molt_storage::derive_identity_key(&entropy, &ws)
}

/// **Recovery step ❶–❻: the rejoiner re-enters the encrypted group.** A member
/// that lost its device drives `run_rejoin` against a coordinator over the
/// loopback hub: it re-derives its identity, builds a fresh KeyPackage, and sends
/// a `RecoverRequest` on the coordinator's recovery queue. The coordinator
/// verifies the seat proof against the anchored key (a genuine authentication —
/// only the phrase-holder can produce it), re-keys the seat with a REAL
/// `restore_member`, and sends the resulting Welcome back. The rejoiner processes
/// the Welcome and is back inside the group — proven by decrypting a message the
/// coordinator encrypts AFTER the re-key (real epoch consistency, not a stub).
///
/// This pins the crypto/ritual core of the rejoiner side. The full production
/// path (the coordinator's real threshold commit driving the Welcome, plus the
/// rejoiner's follow-on mesh catch-up + materialize) is the next increment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejoiner_re_enters_the_mls_group_from_the_coordinators_welcome() {
    // a real 2-member MLS group: the coordinator + bob, born like a founding
    let coord_phrase = molt_storage::generate_seed_phrase().expect("coord phrase");
    let bob_phrase = molt_storage::generate_seed_phrase().expect("bob phrase");
    let (coord_sk, _coord_pk) = member_identity(&coord_phrase);
    let (_bob_sk, bob_pk) = member_identity(&bob_phrase);

    let mut coord = MlsMember::new(&coord_sk, "coordinator").expect("coord mls");
    coord.create_group().expect("coord creates the group");
    {
        // bob's original leaf joins the group, then bob "loses its device"
        let (bob_orig_sk, _) = member_identity(&bob_phrase);
        let mut bob_orig = MlsMember::new(&bob_orig_sk, "bob").expect("bob mls");
        let bob_kp = bob_orig.key_package().expect("bob kp");
        let welcome0 = coord
            .add_members(&[bob_kp])
            .expect("add bob")
            .expect("a welcome for bob");
        bob_orig.join_from_welcome(&welcome0).expect("bob joins");
        // bob_orig is dropped here — the device is gone
    }

    // the coordinator mints a recovery queue + link (the mint plumbing is proven
    // elsewhere; here we drive `run_rejoin` against a real re-key)
    let hub = LoopbackHub::calm();
    let t = hub.transport();
    let recover_q = hub.create_queue_blocking().expect("recovery queue");
    let recover_wrap = WrapKey::fresh().expect("recovery wrap");
    let republic_id = "content-derived-republic-id";
    let inv = molt_engine::RecoveryInvite {
        republic: "Guild".to_string(),
        member: "bob".to_string(),
        ticket: molt_net::invite::mint_ticket().expect("ticket"),
        server: "loopback".to_string(),
        queue_id: hex::encode(&recover_q.snd.id.0),
        wrap: hex::encode(recover_wrap.to_bytes()),
        republic_id: republic_id.to_string(),
        handover: None,
    };
    // exercise the link render→parse roundtrip on the way in
    let link = inv.render();
    let parsed = molt_engine::RecoveryInvite::parse(&link).expect("actionable link");

    // the rejoiner runs the real driver over its own transport clone
    let rejoiner = tokio::spawn(async move {
        molt_engine::run_rejoin(hub.transport(), parsed, &bob_phrase, false).await
    });

    // --- the coordinator side of the handshake (played by the test) ---
    // receive the RecoverRequest on the recovery queue
    let mut rx = t.subscribe(&recover_q.rcv).await.expect("subscribe recovery queue");
    let mut reasm = Reassembler::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let req = loop {
        let delivery = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("recover request in time")
            .expect("recovery queue open");
        let Ok(plain) = molt_net::wrap::unwrap_block(&recover_wrap, &delivery.block) else {
            delivery.ack.ack();
            continue;
        };
        let outcome = reasm.push(&plain);
        delivery.ack.ack();
        if let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome {
            if let Ok(invite::RitualMsg::Recover(r)) =
                serde_json::from_slice::<invite::RitualMsg>(&bytes)
            {
                break r;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "no recover request arrived");
    };

    // authenticate: the seat proof must verify against bob's ANCHORED key, and it
    // must bind this exact ticket + fresh KeyPackage + republic id — the security
    // core of the ritual (a leaked link without the phrase cannot forge it)
    assert_eq!(req.member, "bob");
    assert_eq!(req.identity_pk, bob_pk, "the rejoiner re-derived its anchored key");
    assert!(
        molt_engine::verify_seat_proof(
            &bob_pk,
            &req.ticket,
            &req.key_package,
            republic_id,
            &req.new_nostr_pk,
            &req.seat_proof,
        ),
        "the seat proof verifies against the anchored key"
    );
    assert_eq!(req.ticket, inv.ticket, "the seat proof is bound to the minted ticket");

    // re-key the seat with a REAL MLS commit and send the Welcome back
    let new_kp = hex::decode(&req.key_package).expect("kp hex");
    let (_commit, welcome) = coord.restore_member("bob", &new_kp, molt_net::mls::NO_CARRIER_STAMP).expect("re-key bob");
    let reply = req.reply.expect("the request advertises a reply queue");
    let reply_snd = SndQueueAddr {
        server: reply.server.clone(),
        id: QueueId::from_bytes(hex::decode(&reply.queue_id).expect("reply queue hex")),
    };
    let reply_wrap_bytes: [u8; 32] = hex::decode(&reply.wrap)
        .expect("reply wrap hex")
        .try_into()
        .expect("reply wrap length");
    let reply_wrap = WrapKey::from_bytes(reply_wrap_bytes);
    let welcome_msg = invite::RitualMsg::Welcome {
        welcome: hex::encode(&welcome),
        chain: String::new(), // this MLS-only test serves no chain
    };
    supervisor::send_framed(
        &t,
        &reply_snd,
        &reply_wrap,
        msg_id("coordinator", "bob", 1),
        &serde_json::to_vec(&welcome_msg).expect("encode welcome"),
    )
    .await
    .expect("send the welcome");

    // the rejoiner finished: back inside the group with its re-derived identity
    let outcome = tokio::time::timeout(Duration::from_secs(10), rejoiner)
        .await
        .expect("rejoin completes in time")
        .expect("rejoin task")
        .expect("rejoin succeeds");
    assert_eq!(outcome.member, "bob");
    assert_eq!(outcome.pk, bob_pk);
    assert_eq!(outcome.republic_id, republic_id);

    // PROVE the re-key was real and consistent: restore bob's group from the
    // returned snapshot; a message the coordinator encrypts AFTER the re-key
    // decrypts cleanly on bob's side (same epoch, authenticated sender). A stubbed
    // or stale Welcome would fail here.
    let mut bob_back = MlsMember::restore(&outcome.mls_snapshot).expect("restore bob's group");
    let ct = coord.encrypt(b"welcome back, bob").expect("coord encrypts post re-key");
    match bob_back.decrypt(&ct).expect("bob decrypts") {
        MlsIncoming::Application { from, plaintext } => {
            assert_eq!(from, "coordinator");
            assert_eq!(plaintext, b"welcome back, bob");
        }
        other => panic!("expected an application message, got {other:?}"),
    }
    // and the reverse direction, so bob is a full participant again
    let ct2 = bob_back.encrypt(b"thanks, i'm back").expect("bob encrypts");
    match coord.decrypt(&ct2).expect("coord decrypts") {
        MlsIncoming::Application { from, plaintext } => {
            assert_eq!(from, "bob");
            assert_eq!(plaintext, b"thanks, i'm back");
        }
        other => panic!("expected an application message, got {other:?}"),
    }
}

/// Build a recovery invite with a fixed republic + loopback server, for tests.
fn recovery_invite(
    member: &str,
    ticket: String,
    queue_id: String,
    wrap: String,
    republic_id: &str,
) -> molt_engine::RecoveryInvite {
    molt_engine::RecoveryInvite {
        republic: "Guild".to_string(),
        member: member.to_string(),
        ticket,
        server: "loopback".to_string(),
        queue_id,
        wrap,
        republic_id: republic_id.to_string(),
        handover: None,
    }
}

/// Drive `run_rejoin` over a fresh loopback hub, capture the single
/// `RecoverRequest` it emits on the coordinator's recovery queue, then abort the
/// driver (these authentication tests never send a Welcome, so it would otherwise
/// wait forever). The invite is built here so its queue id matches the
/// hub-created recovery queue.
async fn capture_recover_request(
    member: &str,
    ticket: String,
    republic_id: &str,
    phrase: String,
) -> invite::RecoverRequest {
    let hub = LoopbackHub::calm();
    let q = hub.create_queue_blocking().expect("recovery queue");
    let recover_wrap = WrapKey::fresh().expect("recovery wrap");
    let inv = recovery_invite(
        member,
        ticket,
        hex::encode(&q.snd.id.0),
        hex::encode(recover_wrap.to_bytes()),
        republic_id,
    );
    let t = hub.transport();
    let mut rx = t.subscribe(&q.rcv).await.expect("subscribe recovery queue");
    let rejoiner = tokio::spawn(async move {
        let _ = molt_engine::run_rejoin(hub.transport(), inv, &phrase, false).await;
    });

    let mut reasm = Reassembler::new();
    let req = loop {
        let delivery = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a recover request in time")
            .expect("recovery queue open");
        let Ok(plain) = molt_net::wrap::unwrap_block(&recover_wrap, &delivery.block) else {
            delivery.ack.ack();
            continue;
        };
        let outcome = reasm.push(&plain);
        delivery.ack.ack();
        if let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome {
            if let Ok(invite::RitualMsg::Recover(r)) =
                serde_json::from_slice::<invite::RitualMsg>(&bytes)
            {
                break r;
            }
        }
    };
    rejoiner.abort(); // no Welcome will come; end the driver
    req
}

/// **Security: a leaked link without the phrase cannot rejoin.** Only the seat's
/// phrase re-derives its identity key. A rejoiner running the real driver with
/// the WRONG phrase produces a request whose identity does not match the anchored
/// key and whose seat proof fails to verify against it — the coordinator drops it
/// (recovery_ritual.md §4 ❸, §6). This is the seat-ownership guarantee.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejoiner_with_the_wrong_phrase_is_rejected() {
    let real_phrase = molt_storage::generate_seed_phrase().expect("real phrase");
    let wrong_phrase = molt_storage::generate_seed_phrase().expect("wrong phrase");
    let (_, anchored_pk) = member_identity(&real_phrase); // what the roster anchors
    let republic_id = "content-derived-republic-id";
    let ticket = molt_net::invite::mint_ticket().expect("ticket");

    let req = capture_recover_request("bob", ticket, republic_id, wrong_phrase).await;

    // the wrong-phrase rejoiner presents a DIFFERENT identity than the anchored
    // one, and its seat proof does not verify against the anchored key
    assert_ne!(req.identity_pk, anchored_pk, "wrong phrase = wrong identity");
    assert!(
        !molt_engine::verify_seat_proof(
            &anchored_pk,
            &req.ticket,
            &req.key_package,
            republic_id,
            &req.new_nostr_pk,
            &req.seat_proof,
        ),
        "a wrong-phrase seat proof must NOT verify against the anchored key"
    );
}

/// **Security: a doctored link is caught.** The recovery link carries the
/// republic id (a total-loss rejoiner cannot derive it), and the seat proof binds
/// it. A tampered link with a wrong id makes the rejoiner sign over that wrong id;
/// the coordinator verifies against its OWN (real) id, so the proof fails —
/// exactly the guard recovery_ritual.md §3 relies on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_doctored_recovery_link_id_is_rejected() {
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let (_, anchored_pk) = member_identity(&phrase);
    let real_republic_id = "the-real-content-derived-id";
    let doctored_republic_id = "an-attackers-substituted-id";
    let ticket = molt_net::invite::mint_ticket().expect("ticket");

    // the link the rejoiner receives carries the DOCTORED id, so its seat proof
    // is signed over that id
    let req = capture_recover_request("bob", ticket, doctored_republic_id, phrase).await;

    assert_eq!(req.identity_pk, anchored_pk, "the phrase is correct here");
    assert!(
        !molt_engine::verify_seat_proof(
            &anchored_pk,
            &req.ticket,
            &req.key_package,
            real_republic_id,
            &req.new_nostr_pk,
            &req.seat_proof,
        ),
        "a proof signed over a doctored republic id must NOT verify against the real id"
    );
    // sanity: it DOES verify against the doctored id it was actually signed over,
    // so the failure above is the id-binding at work, not an unrelated mismatch
    assert!(molt_engine::verify_seat_proof(
        &anchored_pk,
        &req.ticket,
        &req.key_package,
        doctored_republic_id,
        &req.new_nostr_pk,
        &req.seat_proof,
    ));
}

fn ev_mls_commit(by: &str, seq: u64, commit_hex: &str) -> EventEnvelope {
    EventEnvelope { prev_seq: 0,
        seq,
        ts: 1_751_000_000 + seq,
        by: by.to_string(),
        body: WorkspaceEvent::MlsCommit {
            commit: commit_hex.to_string(),
        },
    }
}

async fn wait_for(
    sink: &RecordSink,
    pred: impl Fn(&(MemberId, EventEnvelope)) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if sink.messages().iter().any(&pred) {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// **Recovery step ❺, distribution: the re-key commit rides the mesh in-order
/// with chat.** Two survivors (a, b) share a live MLS group that also holds a
/// third seat (zoe). `a` re-keys zoe's seat (`restore_member`) — advancing its
/// own epoch — and broadcasts the resulting commit over the runtime mesh as a
/// `WorkspaceEvent::MlsCommit` (sent RAW, not application-encrypted: the receiver
/// needs it to REACH the new epoch). `b` applies it via its normal receive path.
/// The proof it worked: `b` can decrypt a chat `a` sends AFTER the re-key — an
/// epoch-N+1 message `b` could only read if it applied the commit. The reverse
/// direction confirms the group stayed coherent, not just one-way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rekey_commit_broadcast_over_the_mesh_keeps_survivors_in_epoch() {
    use molt_net::{LoopbackHub, MlsChannel};
    use std::sync::{Arc, Mutex};

    // a live MLS group: a + b are the meshed survivors; zoe is a third seat that
    // gets re-keyed (added, but its device is "lost" — it never joins the mesh)
    let (a_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("a phrase"));
    let (b_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("b phrase"));
    let zoe_phrase = molt_storage::generate_seed_phrase().expect("zoe phrase");
    let (zoe_sk, _) = member_identity(&zoe_phrase);

    let mut a_member = MlsMember::new(&a_sk, "a").expect("a mls");
    let mut b_member = MlsMember::new(&b_sk, "b").expect("b mls");
    let zoe_member = MlsMember::new(&zoe_sk, "zoe").expect("zoe mls");
    a_member.create_group().expect("a creates the group");
    let welcome = a_member
        .add_members(&[
            b_member.key_package().expect("b kp"),
            zoe_member.key_package().expect("zoe kp"),
        ])
        .expect("add b + zoe")
        .expect("a welcome");
    b_member.join_from_welcome(&welcome).expect("b joins");

    let a_mls = Arc::new(Mutex::new(a_member));
    let b_mls = Arc::new(Mutex::new(b_member));

    // wire the 2-node runtime mesh over one loopback hub
    let hub = LoopbackHub::calm();
    let mut links = hub
        .full_mesh(&["a".to_string(), "b".to_string()])
        .expect("mesh wiring");
    let a_links = links.remove("a").expect("a links");
    let b_links = links.remove("b").expect("b links");

    let a_log = MemLog::new();
    let b_log = MemLog::new();
    let a_sink = RecordSink::default();
    let b_sink = RecordSink::default();
    let (a_wake, a_wake_rx) = watch::channel(0u64);
    let (b_wake, b_wake_rx) = watch::channel(0u64);

    let _a_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("a".to_string(), a_links, 1),
        a_log.clone(),
        MemStateStore::new(),
        a_sink.clone(),
        a_wake_rx,
        Some(MlsChannel::from_shared(a_mls.clone())),
    );
    let _b_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("b".to_string(), b_links, 2),
        b_log.clone(),
        MemStateStore::new(),
        b_sink.clone(),
        b_wake_rx,
        Some(MlsChannel::from_shared(b_mls.clone())),
    );

    // 1) the base mesh works at the current epoch — a → b — and we WAIT for it,
    // so this pre-re-key message is encrypted (and read) before the epoch moves
    a_log.push(common::chat_env(1, "a", "before the re-key"));
    let _ = a_wake.send(1);
    wait_for(&b_sink, |(from, e)| from == "a" && e.seq == 1, "b gets the pre-re-key chat").await;

    // 2) a re-keys zoe's seat → a advances to epoch N+1
    let zoe2 = MlsMember::new(&zoe_sk, "zoe").expect("zoe2 mls");
    let zoe2_kp = zoe2.key_package().expect("zoe2 kp");
    let (commit, _welcome) = a_mls
        .lock()
        .expect("a mls lock")
        .restore_member("zoe", &zoe2_kp, molt_net::mls::NO_CARRIER_STAMP)
        .expect("re-key zoe");

    // 3) broadcast the raw commit over the mesh. It is at-least-once (the log
    // outbox) and applies at b's CURRENT epoch, so it lands reliably. (A chat
    // racing AHEAD of it is held by the cross-epoch retry — pinned separately
    // in a_chat_racing_ahead_of_the_rekey_commit_is_buffered_not_lost.) We let
    // the commit settle before the post-re-key chat so this test pins the
    // re-key application deterministically, not the epoch-boundary timing.
    a_log.push(ev_mls_commit("a", 2, &hex::encode(&commit)));
    let _ = a_wake.send(2);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // 4) a post-re-key chat (encrypted at epoch N+1)
    a_log.push(common::chat_env(3, "a", "after the re-key"));
    let _ = a_wake.send(3);

    // b can only decrypt+deliver the epoch-N+1 chat if it applied the commit —
    // the proof the mesh-borne re-key worked
    wait_for(&b_sink, |(from, e)| from == "a" && e.seq == 3, "b gets the post-re-key chat").await;
    // the commit itself is applied at the MLS layer, never delivered as an event
    assert!(
        !b_sink.messages().iter().any(|(_, e)| matches!(e.body, WorkspaceEvent::MlsCommit { .. })),
        "the raw commit is merged into the ratchet, not surfaced as an event"
    );

    // 5) the group stayed coherent both ways: b → a still decrypts at N+1
    b_log.push(common::chat_env(1, "b", "b is still here"));
    let _ = b_wake.send(1);
    wait_for(&a_sink, |(from, e)| from == "b" && e.seq == 1, "a gets b's post-re-key chat").await;
}

/// **Cross-epoch retry: a chat racing AHEAD of the re-key commit is buffered,
/// not lost.** The same harness as the broadcast test above, but the race is
/// forced: after re-keying zoe's seat, `a` sends a chat encrypted at epoch
/// N+1 BEFORE broadcasting the commit — exactly the wire order the lazily
/// encrypting outbox produces in the lone-coordinator burst. `b`, still at
/// epoch N, classifies the ciphertext as future-epoch and holds it (acks
/// unfired); when the commit merges, the held chat decrypts and delivers.
/// Before this hardening the chat was acked away and silently lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_racing_ahead_of_the_rekey_commit_is_buffered_not_lost() {
    use molt_net::{LoopbackHub, MlsChannel};
    use std::sync::{Arc, Mutex};

    let (a_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("a phrase"));
    let (b_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("b phrase"));
    let zoe_phrase = molt_storage::generate_seed_phrase().expect("zoe phrase");
    let (zoe_sk, _) = member_identity(&zoe_phrase);

    let mut a_member = MlsMember::new(&a_sk, "a").expect("a mls");
    let mut b_member = MlsMember::new(&b_sk, "b").expect("b mls");
    let zoe_member = MlsMember::new(&zoe_sk, "zoe").expect("zoe mls");
    a_member.create_group().expect("a creates the group");
    let welcome = a_member
        .add_members(&[
            b_member.key_package().expect("b kp"),
            zoe_member.key_package().expect("zoe kp"),
        ])
        .expect("add b + zoe")
        .expect("a welcome");
    b_member.join_from_welcome(&welcome).expect("b joins");
    let a_mls = Arc::new(Mutex::new(a_member));
    let b_mls = Arc::new(Mutex::new(b_member));

    let hub = LoopbackHub::calm();
    let mut links = hub
        .full_mesh(&["a".to_string(), "b".to_string()])
        .expect("mesh wiring");
    let a_links = links.remove("a").expect("a links");
    let b_links = links.remove("b").expect("b links");
    let a_log = MemLog::new();
    let b_sink = RecordSink::default();
    let (a_wake, a_wake_rx) = watch::channel(0u64);
    let (_b_wake, b_wake_rx) = watch::channel(0u64);
    let _a_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("a".to_string(), a_links, 1),
        a_log.clone(),
        MemStateStore::new(),
        RecordSink::default(),
        a_wake_rx,
        Some(MlsChannel::from_shared(a_mls.clone())),
    );
    let _b_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("b".to_string(), b_links, 2),
        MemLog::new(),
        MemStateStore::new(),
        b_sink.clone(),
        b_wake_rx,
        Some(MlsChannel::from_shared(b_mls.clone())),
    );

    // the base mesh works at the current epoch, settled before the race
    a_log.push(common::chat_env(1, "a", "before the re-key"));
    let _ = a_wake.send(1);
    wait_for(&b_sink, |(from, e)| from == "a" && e.seq == 1, "b gets the pre-re-key chat").await;

    // a re-keys zoe's seat → a is at epoch N+1, b still at N
    let zoe2 = MlsMember::new(&zoe_sk, "zoe").expect("zoe2 mls");
    let (commit, _welcome) = a_mls
        .lock()
        .expect("a mls lock")
        .restore_member(
            "zoe",
            &zoe2.key_package().expect("zoe2 kp"),
            molt_net::mls::NO_CARRIER_STAMP,
        )
        .expect("re-key zoe");

    // THE RACE: the post-re-key chat goes out FIRST …
    a_log.push(common::chat_env(2, "a", "raced ahead of the commit"));
    let _ = a_wake.send(2);
    // … and reaches b while b is still at epoch N — held, not delivered
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !b_sink.messages().iter().any(|(_, e)| e.seq == 2),
        "the future-epoch chat must not deliver before the commit"
    );
    // … only then does the commit follow on the same ordered link
    a_log.push(ev_mls_commit("a", 3, &hex::encode(&commit)));
    let _ = a_wake.send(3);

    // once the commit merges, the held chat decrypts and delivers
    wait_for(
        &b_sink,
        |(from, e)| {
            from == "a"
                && matches!(&e.body,
                    WorkspaceEvent::Chat(m) if m.body == "raced ahead of the commit")
        },
        "the raced-ahead chat to deliver after the commit",
    )
    .await;
}

/// **A malformed announce must not burn the recovery-mesh window.** The
/// one-shot window is the rejoiner's ONLY chance to re-mesh over the recovery
/// channel; spending it on an announce that authenticates (MLS) but fails to
/// parse would kill the mesh phase on a mere version skew or client bug. The
/// window may only be consumed by an announce that actually parses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_announce_does_not_burn_the_recovery_window() {
    use molt_net::mesh;

    let tmp = tempfile::tempdir().expect("tmp");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("coordinator").display().to_string(),
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
        threshold: 1,
        members: 2,
        relays: Vec::new(),
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
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_join = b_phrase.clone();
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_join, true, true, None, None)
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
        agenda: "resilient windows".to_string(),
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
    let _lost_device = b_task.await.expect("B task");
    a.execute(Command::CreateFinish).await.expect("enter");

    // mint + full rejoin WITHOUT the built-in mesh phase — the test drives the
    // announces itself so it can inject a malformed one first
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
    let inv = molt_engine::RecoveryInvite::parse(&material.link).expect("actionable link");
    let rejoin_transport = material.transport.clone();
    let rejoin_phrase = b_phrase.clone();
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::spawn(async move {
            molt_engine::run_rejoin(rejoin_transport, inv, &rejoin_phrase, false).await
        }),
    )
    .await
    .expect("the rejoin finishes in time")
    .expect("rejoin task")
    .expect("the rejoin succeeds");

    // the rejoiner is back in the group; the coordinator's window is armed.
    // ❶ a malformed (but MLS-authentic) announce: valid ciphertext, junk JSON
    let mut b_group = MlsMember::restore(&outcome.mls_snapshot).expect("restore b group");
    let junk_ct = b_group.encrypt(b"this is not a MeshAnnounce").expect("enc junk");
    let msg = invite::RitualMsg::MeshAnnounce { ct: hex::encode(&junk_ct) };
    let payload = serde_json::to_vec(&msg).expect("payload");
    supervisor::send_framed(
        &material.transport,
        &material.recover_snd,
        &material.recover_wrap,
        msg_id("member-b", "mesh-junk", 1),
        &payload,
    )
    .await
    .expect("junk announce sent");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ❷ the real announce follows — it must still be honored
    let new_q = material.transport.create_queue().await.expect("b's fresh queue");
    let new_wrap = WrapKey::fresh().expect("fresh wrap");
    let mut queues = std::collections::BTreeMap::new();
    queues.insert(
        "founder-a".to_string(),
        mesh::QueueHandover::of(&new_q.snd, &new_wrap),
    );
    let announce = mesh::MeshAnnounce { queues };
    let ct = b_group
        .encrypt(&serde_json::to_vec(&announce).expect("encode"))
        .expect("encrypt announce");
    let msg = invite::RitualMsg::MeshAnnounce { ct: hex::encode(&ct) };
    let payload = serde_json::to_vec(&msg).expect("payload");
    supervisor::send_framed(
        &material.transport,
        &material.recover_snd,
        &material.recover_wrap,
        msg_id("member-b", "mesh-real", 1),
        &payload,
    )
    .await
    .expect("real announce sent");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.notice == "mesh-extended:member-b" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the real announce must still extend the mesh after a malformed one"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// **A mesh rebuild must not kill an outstanding recovery.** The recovery
/// queue's recv loop is spawned once at link-mint time; a later mesh
/// EXTENSION rebuilds the supervisor (bumping the mesh incarnation). The
/// recovery request arriving afterwards must still be processed — recovery
/// lifetimes are scoped to the OPEN WORKSPACE, not to a mesh incarnation.
/// Here: the coordinator mints a recovery link for lost member-c, THEN folds
/// member-b's re-announced link in (a rebuild), and only then does c's
/// seat-proofed request arrive on the minted queue: the coordinator must
/// still propose the threshold re-admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mesh_rebuild_does_not_kill_an_outstanding_recovery() {
    use molt_net::mesh;
    use std::sync::{Arc, Mutex};

    let tmp = tempfile::tempdir().expect("tmp");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("coordinator").display().to_string(),
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
        members: 3,
        relays: Vec::new(),
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
    let mut seats = materials.into_iter();
    let seat_b = seats.next().expect("seat for member-b");
    let seat_c = seats.next().expect("seat for member-c");
    let hub = seat_b.transport.clone();

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let c_phrase = molt_storage::generate_seed_phrase().expect("c phrase");
    let c_join = c_phrase.clone();
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat_b, "member-b".to_string(), b_phrase, true, true, None, None)
            .await
            .expect("B completes the member side + bootstrap")
    });
    let c_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat_c, "member-c".to_string(), c_join, true, true, None, None)
            .await
            .expect("C completes the member side + bootstrap")
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the members never joined");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Guild".to_string(),
        agenda: "outlive the rebuild".to_string(),
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
    let _lost_c = c_task.await.expect("C task"); // member-c's device is lost
    a.execute(Command::CreateFinish).await.expect("enter");

    // member-b stays online: its supervisor over the founded mesh, group
    // shared with the test so it can re-announce (and see the gossip)
    let b_mesh = b_outcome.mesh.expect("B assembled its direct mesh");
    let b_mls = b_outcome.mls_snapshot.expect("B post-bootstrap snapshot");
    let sealed = b_outcome.sealed.expect("B collected the sealed roster");
    let links: Vec<PeerLink> = b_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let b_group = Arc::new(Mutex::new(MlsMember::restore(&b_mls).expect("restore B MLS")));
    let b_feed = MemLog::new();
    let b_sink = RecordSink::default();
    let (b_wake, b_wake_rx) = watch::channel(0u64);
    let b_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), links, 7),
        b_feed.clone(),
        MemStateStore::new(),
        b_sink.clone(),
        b_wake_rx,
        Some(MlsChannel::from_shared(b_group.clone())),
    );

    // ❶ the coordinator mints the recovery link for lost member-c — its recv
    // loop starts NOW, before any rebuild
    a.execute(Command::RecoverInviteStart {
        member: "member-c".to_string(),
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

    // ❷ member-b rotates its link: a MeshAnnounced over the running mesh makes
    // the coordinator REBUILD its supervisor (the mesh incarnation bumps)
    let new_q = hub.create_queue().await.expect("b's fresh queue");
    let new_wrap = WrapKey::fresh().expect("fresh wrap");
    let mut queues = std::collections::BTreeMap::new();
    queues.insert(
        "founder-a".to_string(),
        mesh::QueueHandover::of(&new_q.snd, &new_wrap),
    );
    let announce = mesh::MeshAnnounce { queues };
    let ct = b_group
        .lock()
        .expect("b group")
        .encrypt(&serde_json::to_vec(&announce).expect("encode"))
        .expect("encrypt announce");
    b_feed.push(EventEnvelope { prev_seq: 0,
        seq: 2,
        ts: 1_751_000_002,
        by: "member-b".to_string(),
        body: WorkspaceEvent::MeshAnnounced { ct: hex::encode(&ct), nonce: None },
    });
    let _ = b_wake.send(2);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.notice == "mesh-extended:member-b" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the coordinator never folded b's rotated link in"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // finish b's side of the rotation: read the coordinator's reply announce
    // off the announced queue and stand b's supervisor up over the ROTATED
    // link (the coordinator now sends on the new queues)
    let mut rx = hub.subscribe(&new_q.rcv).await.expect("subscribe b's fresh queue");
    let mut reasm = Reassembler::new();
    let reply_ct = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let d = rx.recv().await.expect("queue open");
            let Ok(plain) = molt_net::wrap::unwrap_block(&new_wrap, &d.block) else {
                d.ack.ack();
                continue;
            };
            let out = reasm.push(&plain);
            d.ack.ack();
            if let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = out {
                if let Ok(invite::RitualMsg::MeshAnnounce { ct }) =
                    serde_json::from_slice::<invite::RitualMsg>(&bytes)
                {
                    break hex::decode(&ct).expect("reply hex");
                }
            }
        }
    })
    .await
    .expect("the coordinator's reply reaches the announced queue");
    b_sup.shutdown();
    let molt_net::MlsIncoming::Application { plaintext, .. } = b_group
        .lock()
        .expect("b group")
        .decrypt(&reply_ct)
        .expect("decrypt the reply")
    else {
        panic!("the reply is an application message");
    };
    let reply: mesh::MeshAnnounce = serde_json::from_slice(&plaintext).expect("reply announce");
    let target = reply.queues.get("member-b").expect("a queue for member-b");
    let rotated = PeerLink {
        member: "founder-a".to_string(),
        snds: vec![target.addr().expect("addr")],
        wrap_out: target.wrap_key().expect("wrap"),
        rcvs: vec![new_q.rcv.clone()],
        wrap_in: new_wrap.clone(),
    };
    let b_sink = RecordSink::default();
    let (_b_wake2, b_wake2_rx) = watch::channel(0u64);
    let _b_sup2 = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), vec![rotated], 9),
        MemLog::new(),
        MemStateStore::new(),
        b_sink.clone(),
        b_wake2_rx,
        Some(MlsChannel::from_shared(b_group.clone())),
    );

    // ❸ only NOW does member-c's seat-proofed request arrive on the queue
    // minted BEFORE the rebuild — it must still drive the re-admission
    let c_pk = sealed
        .identities
        .iter()
        .find(|i| i.member == "member-c")
        .expect("member-c anchored")
        .identity_pk
        .clone();
    let (c_sk, _) = member_identity(&c_phrase);
    let kp_hex = "abcd"; // an opaque fresh key package for this test
    let seat_proof =
        molt_engine::make_seat_proof(&c_sk, &material.ticket, kp_hex, &material.republic_id, "");
    let request = invite::RitualMsg::Recover(invite::RecoverRequest {
        new_nostr_pk: String::new(),
        member: "member-c".to_string(),
        identity_pk: c_pk,
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
        msg_id("member-c", "coordinator", 1),
        &payload,
    )
    .await
    .expect("send the recovery request on the minted queue");

    // the coordinator must verify + propose despite the interleaved rebuild:
    // the MembershipProposed{Restored} reaches member-b over the (rotated) mesh
    wait_for(
        &b_sink,
        |(_, env)| {
            matches!(&env.body,
                WorkspaceEvent::MembershipProposed { member, op, .. }
                    if member == "member-c" && *op == molt_core::MembershipOp::Restored)
        },
        "the re-admission proposal to reach member-b after the rebuild",
    )
    .await;
}

/// **Cross-epoch retry across LINKS: the commit and the held message may
/// arrive on different peers' links.** The MLS group is node-global but each
/// per-peer recv loop holds its own future-epoch buffer — a commit that merges
/// via peer A's link must also release messages held on peer B's link, or they
/// sit there for the whole session (B may never send another commit). Here: a
/// re-keys zoe's seat; b merges the commit FAST (out of band) and chats at the
/// new epoch — that chat reaches c on the b→c link BEFORE a's commit arrives
/// on the a→c link. Once the commit lands via a, the chat held on the b link
/// must deliver.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_on_one_link_releases_messages_held_on_another() {
    use molt_net::{LoopbackHub, MlsChannel};
    use std::sync::{Arc, Mutex};

    let (a_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("a phrase"));
    let (b_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("b phrase"));
    let (c_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("c phrase"));
    let zoe_phrase = molt_storage::generate_seed_phrase().expect("zoe phrase");
    let (zoe_sk, _) = member_identity(&zoe_phrase);
    let mut a_member = MlsMember::new(&a_sk, "a").expect("a mls");
    let mut b_member = MlsMember::new(&b_sk, "b").expect("b mls");
    let mut c_member = MlsMember::new(&c_sk, "c").expect("c mls");
    let zoe_member = MlsMember::new(&zoe_sk, "zoe").expect("zoe mls");
    a_member.create_group().expect("a creates the group");
    let welcome = a_member
        .add_members(&[
            b_member.key_package().expect("b kp"),
            c_member.key_package().expect("c kp"),
            zoe_member.key_package().expect("zoe kp"),
        ])
        .expect("add b + c + zoe")
        .expect("a welcome");
    b_member.join_from_welcome(&welcome).expect("b joins");
    c_member.join_from_welcome(&welcome).expect("c joins");
    let a_mls = Arc::new(Mutex::new(a_member));
    let b_mls = Arc::new(Mutex::new(b_member));
    let c_mls = Arc::new(Mutex::new(c_member));

    let hub = LoopbackHub::calm();
    let mut links = hub
        .full_mesh(&["a".to_string(), "b".to_string(), "c".to_string()])
        .expect("mesh wiring");
    let a_links = links.remove("a").expect("a links");
    let b_links = links.remove("b").expect("b links");
    let c_links = links.remove("c").expect("c links");
    let a_log = MemLog::new();
    let b_log = MemLog::new();
    let c_sink = RecordSink::default();
    let (a_wake, a_wake_rx) = watch::channel(0u64);
    let (b_wake, b_wake_rx) = watch::channel(0u64);
    let (_c_wake, c_wake_rx) = watch::channel(0u64);
    let _a_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("a".to_string(), a_links, 1),
        a_log.clone(),
        MemStateStore::new(),
        RecordSink::default(),
        a_wake_rx,
        Some(MlsChannel::from_shared(a_mls.clone())),
    );
    let _b_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("b".to_string(), b_links, 2),
        b_log.clone(),
        MemStateStore::new(),
        RecordSink::default(),
        b_wake_rx,
        Some(MlsChannel::from_shared(b_mls.clone())),
    );
    let _c_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("c".to_string(), c_links, 3),
        MemLog::new(),
        MemStateStore::new(),
        c_sink.clone(),
        c_wake_rx,
        Some(MlsChannel::from_shared(c_mls.clone())),
    );

    // a re-keys zoe's seat → a at N+1; b merges the commit FAST, out of band
    // (in production: b's a-link simply delivered the commit before c's did)
    let zoe2 = MlsMember::new(&zoe_sk, "zoe").expect("zoe2 mls");
    let (commit, _welcome) = a_mls
        .lock()
        .expect("a mls lock")
        .restore_member(
            "zoe",
            &zoe2.key_package().expect("zoe2 kp"),
            molt_net::mls::NO_CARRIER_STAMP,
        )
        .expect("re-key zoe");
    match b_mls.lock().expect("b mls").decrypt(&commit).expect("b merges") {
        molt_net::MlsIncoming::Commit => {}
        other => panic!("expected a commit, got {other:?}"),
    }

    // b chats at the NEW epoch — it reaches c on the b→c link while c is
    // still at N (a's commit has not been sent yet): held on the b link
    b_log.push(common::chat_env(1, "b", "crossed the epoch on another link"));
    let _ = b_wake.send(1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !c_sink.messages().iter().any(|(from, _)| from == "b"),
        "the future-epoch chat must be held while c is behind"
    );

    // only now does a's commit reach c — on the a→c link
    a_log.push(ev_mls_commit("a", 1, &hex::encode(&commit)));
    let _ = a_wake.send(1);

    // the commit merged via the a link must release the chat held on the b link
    wait_for(
        &c_sink,
        |(from, e)| {
            from == "b"
                && matches!(&e.body,
                    WorkspaceEvent::Chat(m) if m.body == "crossed the epoch on another link")
        },
        "the chat held on the b link to deliver after the commit from a",
    )
    .await;
}

/// **Cross-epoch retry under buffer pressure: a SHED message is not lost.**
/// The future-epoch hold is bounded (64 per link); the 65th racing message is
/// itself shed onto transport redelivery (newest, so the buffer stays in
/// sender-ratchet generation order). The shed message's acks are unfired on
/// purpose — but the reassembler must also FORGET its message id, or the
/// redelivered copy classifies as a duplicate of an "accepted" message and is
/// acked away: the only durable copy erased, silent permanent loss. Here `a`
/// races 65 chats ahead of the re-key commit; after the commit lands, ALL 65
/// must reach `b` — the shed one via redelivery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shed_future_epoch_message_survives_via_redelivery() {
    use molt_net::{LoopbackHub, MlsChannel};
    use std::sync::{Arc, Mutex};

    let (a_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("a phrase"));
    let (b_sk, _) = member_identity(&molt_storage::generate_seed_phrase().expect("b phrase"));
    let zoe_phrase = molt_storage::generate_seed_phrase().expect("zoe phrase");
    let (zoe_sk, _) = member_identity(&zoe_phrase);
    let mut a_member = MlsMember::new(&a_sk, "a").expect("a mls");
    let mut b_member = MlsMember::new(&b_sk, "b").expect("b mls");
    let zoe_member = MlsMember::new(&zoe_sk, "zoe").expect("zoe mls");
    a_member.create_group().expect("a creates the group");
    let welcome = a_member
        .add_members(&[
            b_member.key_package().expect("b kp"),
            zoe_member.key_package().expect("zoe kp"),
        ])
        .expect("add b + zoe")
        .expect("a welcome");
    b_member.join_from_welcome(&welcome).expect("b joins");
    let a_mls = Arc::new(Mutex::new(a_member));
    let b_mls = Arc::new(Mutex::new(b_member));

    let hub = LoopbackHub::calm();
    let mut links = hub
        .full_mesh(&["a".to_string(), "b".to_string()])
        .expect("mesh wiring");
    let a_links = links.remove("a").expect("a links");
    let b_links = links.remove("b").expect("b links");
    let a_log = MemLog::new();
    let b_sink = RecordSink::default();
    let (a_wake, a_wake_rx) = watch::channel(0u64);
    let (_b_wake, b_wake_rx) = watch::channel(0u64);
    let _a_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("a".to_string(), a_links, 1),
        a_log.clone(),
        MemStateStore::new(),
        RecordSink::default(),
        a_wake_rx,
        Some(MlsChannel::from_shared(a_mls.clone())),
    );
    let _b_sup = supervisor::spawn(
        hub.transport(),
        NetConfig::fast("b".to_string(), b_links, 2),
        MemLog::new(),
        MemStateStore::new(),
        b_sink.clone(),
        b_wake_rx,
        Some(MlsChannel::from_shared(b_mls.clone())),
    );

    a_log.push(common::chat_env(1, "a", "before the re-key"));
    let _ = a_wake.send(1);
    wait_for(&b_sink, |(from, e)| from == "a" && e.seq == 1, "b gets the pre-re-key chat").await;

    // a re-keys zoe's seat → a is at epoch N+1, b still at N
    let zoe2 = MlsMember::new(&zoe_sk, "zoe").expect("zoe2 mls");
    let (commit, _welcome) = a_mls
        .lock()
        .expect("a mls lock")
        .restore_member(
            "zoe",
            &zoe2.key_package().expect("zoe2 kp"),
            molt_net::mls::NO_CARRIER_STAMP,
        )
        .expect("re-key zoe");

    // 65 chats race ahead of the commit — one more than the hold buffer, so
    // the LAST one is shed onto transport redelivery while b is still at N
    for i in 0..65u64 {
        a_log.push(common::chat_env(2 + i, "a", &format!("racing #{i}")));
    }
    let _ = a_wake.send(66);
    // let the burst arrive and the shed happen (b holds 64, sheds #64)
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !b_sink.messages().iter().any(|(_, e)| e.seq >= 2),
        "nothing may deliver before the commit"
    );
    // only then does the commit follow
    a_log.push(ev_mls_commit("a", 67, &hex::encode(&commit)));
    let _ = a_wake.send(67);

    // EVERY racing chat delivers — the held 64 on the commit, the shed one via
    // redelivery (its acks were never fired, and its id must be forgotten so
    // the redelivered copy re-completes instead of being acked as a duplicate)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let got: std::collections::HashSet<u64> = b_sink
            .messages()
            .iter()
            .filter(|(from, e)| from == "a" && e.seq >= 2)
            .map(|(_, e)| e.seq)
            .collect();
        if (2..=66).all(|s| got.contains(&s)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "all 65 racing chats must deliver; missing: {:?}",
            (2..=66).filter(|s| !got.contains(s)).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// **Minting a recovery link never needs the RETURNING member online** — the
/// link exists precisely because that member is unreachable (device lost). The
/// only live dependency is the coordinator's OWN mesh runtime. When that mesh
/// is not running (here: the workspace was closed and reopened over the
/// loopback hub, whose queues cannot outlive their transport — the same state
/// as a reopen after a crash), the mint must NOT surface as a raw command
/// error: the engine acks the human's decision and reports the operational
/// outcome on the same session-notice channel the minted link itself rides
/// (`recovery-link-failed:` beside `recovery-link:`), so every operator (GUI,
/// MCP) reads a calm, retryable state instead of a failure toast. The reopened
/// republic stays chain-governed — recovery exists here, and the status read
/// says so (the GUI offers the action only where it exists).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_mint_without_a_running_mesh_reports_calmly_instead_of_erroring() {
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
    let (a, material_rx) = molt_engine::__spawn_manual_founding_bootstrap(
        molt_core::GroupConfig::demo(),
        session_a,
    );
    a.execute(Command::CreateStart {
        name: "Guild".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
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
        agenda: "mint links for the absent".to_string(),
    })
    .await
    .expect("founder proposes the charter");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(&a).await;
        assert_ne!(s.create.run.outcome, 2, "ritual must not fail: {:?}", s.create.run.log);
        if s.create.run.outcome == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founding never sealed; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    b_task.await.expect("B task");
    a.execute(Command::CreateFinish).await.expect("enter");
    let id = read_session(&a).await.active_workspace.clone();
    assert!(!id.is_empty(), "the founded republic is open");

    // close cleanly, reopen: the loopback mesh cannot be resumed (its queues
    // died with the ritual transport), so the reopened workspace runs WITHOUT
    // a real mesh — the exact state the GUI's Members table acts from after
    // an app restart that could not resume the transport
    a.execute(Command::CloseWorkspace).await.expect("close");
    a.execute(Command::OpenWorkspace { id }).await.expect("reopen");

    // recovery still exists here (chain-governed republic) — and the status
    // read carries that fact for the frontends
    match a.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => {
            assert!(st.chain_governed, "a reopened republic is chain-governed");
        }
        other => panic!("unexpected: {other:?}"),
    }

    // the human's decision is acked, not errored; the outcome is the calm
    // notice — the returning member's presence never entered the picture
    a.execute(Command::RecoverInviteStart {
        member: "member-b".to_string(),
    })
    .await
    .expect("a mint without a running mesh is not a command error");
    let s = read_session(&a).await;
    assert_eq!(
        s.notice, "recovery-link-failed:mesh-not-running",
        "the operational outcome rides the recovery notice channel"
    );
}
