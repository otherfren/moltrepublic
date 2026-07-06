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
    Command, MemberIdentity, Reply, RosterAttestation, SessionSettings, SessionView, WorkspaceEvent,
};
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
        ..
    } = log[0].body.clone()
    else {
        panic!("first event is not Founded");
    };
    FoundedView { name, rule_m, rule_n, identities, attestations, republic_id }
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
        molt_engine::join_founding_over_smp(
            &link_for_b,
            "member-b".to_string(),
            b_phrase,
            &root_b_arg,
        )
        .await
        .expect("B joins from the link over SMP and writes its own workspace")
    });
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
