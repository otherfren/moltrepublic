// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! T4 Tor — live end-to-end over a real Tor daemon (plan §4 B4, concept §7).
//!
//! The productionised founding ritual of `ritual_engine_over_smp.rs`, but with
//! every SMP connection routed over Tor: the founder engine resolves its dialer
//! from `anonymity = tor, tor_mode = local, tor_port = 9050`, and the separate
//! joiner is handed the same Tor `Dialer`. The whole multi-round-trip ritual —
//! invite-queue provisioning, activation-MAC verification, MLS key collection,
//! the roster seal and its distribution — happens through SOCKS5h circuits, and
//! the test asserts both instances end up holding the *same* sealed
//! constitution. It is the live twin of the deterministic no-leak harness in
//! `crates/molt-net/tests/tor_no_leak.rs`.
//!
//! `#[ignore]` (needs live infrastructure):
//!
//! 1. **A real Tor daemon listening on `127.0.0.1:9050`** (system tor / Tor
//!    Browser). Without it every dial fails closed (`TorUnavailable`) and the
//!    ritual never seals — that is the correct fail-closed behaviour, not a
//!    test bug.
//! 2. **A reachable SMP server** (this test uses the public `smp.konkin.io`,
//!    dialed clearnet-through-a-Tor-exit via SOCKS5h). A cold circuit to it can
//!    be slow, so the deadlines below are generous.
//!
//! Run:
//! ```text
//! cargo test -p molt-engine --test tor_e2e -- --ignored --nocapture
//! ```
//!
//! ## Manual CI tier — the OS-level egress no-leak proof (concept §7)
//!
//! The strongest T4 guarantee ("a Tor-configured node makes ZERO direct dials")
//! is proven by running THIS ignored test with all non-loopback direct egress
//! blocked at the OS and confirming it *still* completes over Tor. The
//! deterministic `tor_no_leak.rs` is the automatable proxy for this manual
//! runner tier; this is the real-firewall version.
//!
//! Example (Linux, iptables) — block the *test user's* direct egress while
//! leaving loopback (so the app can still reach Tor's SOCKS listener on
//! `127.0.0.1:9050`) and Tor's own exit traffic (tor runs as a different user,
//! so `--uid-owner` does not match it):
//! ```text
//!   sudo iptables -A OUTPUT -o lo -j ACCEPT
//!   sudo iptables -A OUTPUT -m owner --uid-owner "$(id -u)" -p tcp \
//!        ! -d 127.0.0.0/8 --syn -j REJECT
//!   # now run the ritual as this user — it must STILL seal, entirely over Tor:
//!   cargo test -p molt-engine --test tor_e2e -- --ignored --nocapture
//!   # tear down afterwards:
//!   sudo iptables -D OUTPUT -m owner --uid-owner "$(id -u)" -p tcp \
//!        ! -d 127.0.0.0/8 --syn -j REJECT
//!   sudo iptables -D OUTPUT -o lo -j ACCEPT
//! ```
//! A `nftables` equivalent (`ct state new tcp ... meta skuid <uid> ip daddr !=
//! 127.0.0.0/8 reject`) works identically. If the ritual seals under that rule,
//! there was no direct egress — every byte went through Tor.

use std::path::Path;
use std::time::Duration;

use molt_core::RosterAttestation;
use molt_core::{
    Command, MemberIdentity, Reply, SessionSettings, SessionView, WorkspaceEvent,
};
use molt_engine::{FoundingInvite, WalletHandle};
use molt_net::smp::tls::Dialer;

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

/// The full founding ritual between two independent engine instances, **every
/// SMP round-trip carried over Tor** (founder resolves tor+local from its
/// settings; the joiner is handed the same Tor dialer). Proves the ritual seals
/// over Tor and both instances hold the same constitution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live tor: needs a Tor daemon on 127.0.0.1:9050 + a reachable SMP server"]
async fn engine_founds_over_tor_across_two_instances() {
    // the same Tor routing the app resolves from settings — used verbatim for
    // the joiner and mirrored in the founder's settings below.
    let tor_dialer = Dialer::resolve("tor", "local", 9050).expect("resolve tor+local dialer");
    assert!(
        !matches!(tor_dialer, Dialer::Direct),
        "the Tor dialer must never be Direct — fail-closed"
    );

    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");

    // --- Instance A: a real founder engine, founding over the configured SMP
    // server through Tor (anonymity=tor → the SOCKS5h dialer, resolved by the
    // engine's own config→dialer bridge).
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            smp_server: "custom".to_string(),
            smp_url: KONKIN.to_string(),
            anonymity: "tor".to_string(),
            tor_mode: "local".to_string(),
            tor_port: 9050,
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    // keep the material sink alive (unused: B joins from the link instead)
    let (a, _material_rx) =
        molt_engine::__spawn_manual_founding_over_smp(molt_core::GroupConfig::demo(), session_a);

    a.execute(Command::CreateStart {
        name: "Tor Duet".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
    })
    .await
    .expect("create start");

    // A provisions its invite queue on the SMP server over Tor, then publishes
    // the real joinable link into its session. Poll for it (Tor round-trips are
    // slow, so allow generously).
    let link = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let s = read_session(&a).await;
            if let Some(seat0) = s.create.seats.first() {
                if FoundingInvite::parse(&seat0.link).is_some() {
                    break seat0.link.clone();
                }
            }
            // a fail-closed dialer error surfaces on the health pill — report it
            assert!(
                !matches!(s.net_health, molt_core::NetHealth::Down { .. }),
                "the founder's transport went Down (is Tor running on 9050?): {:?}",
                s.net_health
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "A never published a real invite link over Tor (is Tor running on 9050?)"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };

    // --- Instance B: a genuinely separate node with ONLY the link. It builds
    // its own SmpTransport from the handover and joins over SMP **through Tor**
    // (the same resolved dialer), with its own recovery phrase.
    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let link_for_b = link.clone();
    let root_b = tmp.path().join("member-b");
    let root_b_arg = root_b.clone();
    let b_dialer = tor_dialer.clone();
    let b_task = tokio::spawn(async move {
        // the standalone join auto-ratifies the charter (no human gate); every
        // dial routes over Tor via the handed-in dialer.
        molt_engine::join_founding_over_smp(
            &link_for_b,
            "member-b".to_string(),
            b_phrase,
            &root_b_arg,
            b_dialer,
        )
        .await
        .expect("B joins from the link over Tor and writes its own workspace")
    });

    // once B has joined, the deliberation step unlocks: the founder proposes
    // the final name + charter, and only then does the roster seal. (Do this
    // BEFORE awaiting B — B's join returns only after the seal.)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let s = read_session(&a).await;
        if s.create.can_propose {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "member-b never joined over Tor in time; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Tor Duet".to_string(),
        agenda: "route the commons over Tor".to_string(),
    })
    .await
    .expect("founder proposes the charter");

    // B's join returns only after the founder distributed the sealed roster,
    // so by here A has finalized.
    let b_ws_id = b_task.await.expect("B task");

    // --- A's workspace comes into being
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let a_id = loop {
        let s = read_session(&a).await;
        if s.create.run.outcome == 1 {
            break s.active_workspace.clone();
        }
        assert_eq!(s.create.run.outcome, 0, "ritual must not fail: {:?}", s.create.run.log);
        assert!(
            tokio::time::Instant::now() < deadline,
            "the ritual did not seal over Tor in time; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    a.execute(Command::CreateFinish).await.expect("enter");
    a.execute(Command::CloseWorkspace).await.expect("close");

    // --- both instances hold the SAME sealed constitution, each on its OWN
    // disk under its OWN seed-derived id — proving the ritual completed over Tor
    let a_founded = read_founded(&root_a, &a_id);
    let b_founded = read_founded(&root_b, &b_ws_id);

    assert_ne!(a_id, b_ws_id, "each member's local workspace id is its own");
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

    // every attestation verifies against the republic-id table
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
        "OK: two engine instances founded over real Tor — both hold the same \
         sealed roster on their own disks (a={a_id:.8}, b={b_ws_id:.8})"
    );
}
