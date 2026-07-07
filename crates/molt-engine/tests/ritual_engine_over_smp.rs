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

use std::path::Path;

use molt_core::{
    Command, MemberIdentity, Reply, Screen, SessionSettings, SessionView, WorkspaceEvent,
};
use molt_core::RosterAttestation;
use molt_engine::{FoundingInvite, WalletHandle};

const KONKIN: &str = "smp://f4nx4eK5dHAw8sO9_wl-UOfLQOGzxl8mVOA3Nj3wrQ0=@smp.konkin.io";

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// The sealed-roster fields of a workspace's on-disk genesis.
struct FoundedView {
    name: String,
    rule_m: u8,
    rule_n: u8,
    identities: Vec<MemberIdentity>,
    attestations: Vec<RosterAttestation>,
    republic_id: String,
    agenda: String,
}

fn read_founded(root: &Path, id: &str) -> FoundedView {
    let dir = molt_storage::find_workspace_dir(root, id).expect("dir");
    let (ws, _loaded) = molt_storage::open_workspace(&dir).expect("open");
    let log = ws.read_log_from(1).expect("genesis");
    let WorkspaceEvent::Founded {
        name,
        rule_m,
        rule_n,
        identities,
        attestations,
        republic_id,
        agenda,
        ..
    } = log[0].body.clone()
    else {
        panic!("first event is not Founded");
    };
    FoundedView { name, rule_m, rule_n, identities, attestations, republic_id, agenda }
}

/// The invite link carries the transport handover *and* still shows a
/// preview — no network needed.
#[test]
fn founding_link_round_trips_and_stays_previewable() {
    let inv = FoundingInvite {
        info: molt_core::InviteInfo {
            republic: "SMP Duet".into(),
            threshold: 2,
            members: 2,
            inviter: "founder-a".into(),
            ticket: "ab".repeat(32),
        },
        server: KONKIN.into(),
        queue_id: "cd".repeat(12),
        wrap: "ef".repeat(32),
        seat: 0,
    };
    let link = inv.render();
    // the preview still parses (the GUI shows republic / m-of-n / inviter)
    let preview = molt_core::InviteInfo::parse(&link).expect("preview parses");
    assert_eq!(preview.republic, "SMP Duet");
    assert_eq!((preview.threshold, preview.members), (2, 2));
    assert_eq!(preview.inviter, "founder-a");
    // and the full handover round-trips
    let back = FoundingInvite::parse(&link).expect("full link parses");
    assert_eq!(back.server, KONKIN);
    assert_eq!(back.queue_id, "cd".repeat(12));
    assert_eq!(back.wrap, "ef".repeat(32));
    assert_eq!(back.seat, 0);
    assert_eq!(back.info.ticket, "ab".repeat(32));
    // a bare preview link (no handover) is not a founding invite
    assert!(FoundingInvite::parse(&inv.info.render()).is_none());
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
    // keep the material sink alive (unused: B joins from the link instead)
    let (a, _material_rx) =
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
    // publishes the REAL joinable link into its session — exactly what its
    // GUI would show. Poll for it (a network round-trip).
    let link = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let s = read_session(&a).await;
            if let Some(seat0) = s.create.seats.first() {
                if FoundingInvite::parse(&seat0.link).is_some() {
                    break seat0.link.clone();
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "A never published a real invite link"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };

    // --- Instance B: a genuinely separate node that has ONLY the link (as if
    // it were pasted from an off-band message). It builds its own SmpTransport
    // from the link's handover and joins over SMP with its own recovery phrase.
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let link_for_b = link.clone();
    let root_b = tmp.path().join("member-b");
    let root_b_arg = root_b.clone();
    let b_task = tokio::spawn(async move {
        // the standalone join auto-ratifies the charter (no human gate)
        molt_engine::join_founding_over_smp(
            &link_for_b,
            "member-b".to_string(),
            b_phrase,
            &root_b_arg,
        )
        .await
        .expect("B joins from the link over SMP and writes its own workspace")
    });

    // once B has joined, the deliberation step unlocks: the founder proposes
    // the final name + charter, and only then does the roster seal. (Do this
    // BEFORE awaiting B — B's join returns only after the seal.)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(&a).await;
        if s.create.can_propose {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "member-b never joined in time; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    a.execute(Command::CreatePropose {
        name: "SMP Duet".to_string(),
        agenda: "keep the commons in good repair".to_string(),
    })
    .await
    .expect("founder proposes the charter");

    // B's join returns only after the founder distributed the sealed roster,
    // so by here A has finalized
    let b_ws_id = b_task.await.expect("B task");

    // --- A's workspace comes into being
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let a_id = loop {
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
    a.execute(Command::CreateFinish).await.expect("enter");
    a.execute(Command::CloseWorkspace).await.expect("close");

    // --- both instances hold the SAME sealed constitution, each on its OWN
    // disk under its OWN seed-derived id
    let a_founded = read_founded(&root_a, &a_id);
    let b_founded = read_founded(&root_b, &b_ws_id);

    // distinct LOCAL ids (each from its own seed) …
    assert_ne!(a_id, b_ws_id, "each member's local workspace id is its own");
    // … but one shared republic + roster + attestations
    assert!(!a_founded.republic_id.is_empty(), "the republic id is set");
    assert_eq!(a_founded.republic_id, b_founded.republic_id, "same republic id");
    assert_eq!(a_founded.identities, b_founded.identities, "same identity roster");
    assert_eq!(a_founded.attestations, b_founded.attestations, "same attestations");
    assert_eq!(a_founded.identities.len(), 2, "founder + member-b");
    assert_eq!(a_founded.attestations.len(), 2, "both signed");

    // the republic id is the neutral, content-derived value (no member's seed)
    assert_eq!(
        a_founded.republic_id,
        molt_storage::republic_id(
            &a_founded.name,
            a_founded.rule_m,
            a_founded.rule_n,
            &a_founded.identities
        ),
        "republic id is the content-derived value"
    );

    // every attestation verifies against the republic-id table (NOT a local id)
    let table = molt_core::roster_canonical_bytes(
        &a_founded.republic_id,
        a_founded.rule_m,
        a_founded.rule_n,
        &a_founded.identities,
        &a_founded.agenda,
    );
    for att in &a_founded.attestations {
        let id = a_founded
            .identities
            .iter()
            .find(|i| i.member == att.member)
            .expect("attestation names a member");
        assert!(
            molt_storage::identity_verify(&id.identity_pk, &table, &att.sig),
            "attestation for {} does not verify",
            att.member
        );
    }
    println!(
        "OK: two engine instances founded over real SMP — both hold the same \
         sealed roster on their own disks (a={a_id:.8}, b={b_ws_id:.8}), all attestations verify"
    );
}

/// The joiner drives the ACTUAL engine `JoinStart` lifecycle (not the
/// standalone helper): paste the link, and the engine runs the real SMP join
/// off the actor, materialises the joiner's own workspace, and enters it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "engine join over the real smp.konkin.io"]
async fn engine_join_lifecycle_over_smp_enters_the_republic() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let root_b = tmp.path().join("joiner");

    // A: founder engine over SMP, publishes the real link
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
    let (a, _rx) =
        molt_engine::__spawn_manual_founding_over_smp(molt_core::GroupConfig::demo(), session_a);
    a.execute(Command::CreateStart {
        name: "Join Duet".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
        net: "tor".to_string(),
    })
    .await
    .expect("create start");
    let link = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let s = read_session(&a).await;
            if let Some(seat0) = s.create.seats.first() {
                if FoundingInvite::parse(&seat0.link).is_some() {
                    break seat0.link.clone();
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "A never published a link");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };

    // B: a normal storage-backed engine drives the real JoinStart — the link
    // carries the server, so B needs no SMP config of its own
    let session_b = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_b.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let b = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session_b);
    b.execute(Command::JoinStart { invite: link, member: "member-b".to_string() })
        .await
        .expect("join start");

    // the joiner's own recovery phrase is shown while it waits
    let joining = read_session(&b).await;
    assert_eq!(joining.screen, Screen::Join);
    assert!(!joining.join.seed.is_empty(), "the joiner's phrase is shown");

    // once B has joined, the founder proposes the deliberated charter
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "member-b never reached the founder");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Join Duet".to_string(),
        agenda: "share the load".to_string(),
    })
    .await
    .expect("founder proposes the charter");

    // B's wizard surfaces the charter for ratification; B confirms it, which
    // releases its seal signature (the human gate the GUI join enforces)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(&b).await;
        if s.join.awaiting_ratify {
            assert_eq!(s.join.proposed_name, "Join Duet", "the joiner sees the final name");
            assert_eq!(s.join.proposed_agenda, "share the load", "…and the agenda");
            break;
        }
        assert_ne!(s.join.run.outcome, 2, "join must not fail: {:?}", s.join.run.log);
        assert!(
            tokio::time::Instant::now() < deadline,
            "B never reached the ratification step: {:?}",
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    b.execute(Command::JoinConfirmCharter)
        .await
        .expect("B ratifies the charter");

    // B's join completes → it enters the republic with its own workspace
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let b_id = loop {
        let s = read_session(&b).await;
        if !s.active_workspace.is_empty() {
            assert_eq!(s.screen, Screen::Main, "joiner entered the republic");
            break s.active_workspace.clone();
        }
        assert_ne!(s.join.run.outcome, 2, "join must not fail: {:?}", s.join.run.log);
        assert!(
            tokio::time::Instant::now() < deadline,
            "the engine join did not complete: {:?}",
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let a_id = read_session(&a).await.active_workspace.clone();
    assert!(!a_id.is_empty(), "A sealed its own workspace");

    // both hold the same sealed roster on their own disks
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
    let a_founded = read_founded(&root_a, &a_id);
    let b_founded = read_founded(&root_b, &b_id);
    assert_ne!(a_id, b_id, "each member's local workspace id is its own");
    assert!(!a_founded.republic_id.is_empty());
    assert_eq!(a_founded.republic_id, b_founded.republic_id, "same republic id");
    assert_eq!(a_founded.identities, b_founded.identities, "same roster");
    assert_eq!(a_founded.attestations, b_founded.attestations, "same attestations");
    assert_eq!(a_founded.attestations.len(), 2, "both signed");

    let table = molt_core::roster_canonical_bytes(
        &b_founded.republic_id,
        b_founded.rule_m,
        b_founded.rule_n,
        &b_founded.identities,
        &b_founded.agenda,
    );
    for att in &b_founded.attestations {
        let id = b_founded
            .identities
            .iter()
            .find(|i| i.member == att.member)
            .expect("attestation names a member");
        assert!(
            molt_storage::identity_verify(&id.identity_pk, &table, &att.sig),
            "attestation for {} does not verify",
            att.member
        );
    }
    println!(
        "OK: joiner drove the engine JoinStart over real SMP and entered the \
         republic with its own workspace (b={b_id:.8})"
    );
}
