// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Cluster H — the security checks nothing observed** (`docs/transport/
//! nostr_n4a_followup_plans.md` §H).
//!
//! Every check in here EXISTS in the shipping code and no test would have
//! noticed losing it. What they all need is the same thing the founding tests
//! cannot give: a peer that MISBEHAVES. `nostr_founding.rs` drives two honest
//! engines, so the impersonation branch, the founder-identity guards and the
//! genesis byte comparison are all reached only on paths an honest node never
//! takes.
//!
//! So the counterparty here is hand-written from public API — a `RitualNet`
//! under a key of the test's choosing, and (for the joiner-side cases) a whole
//! hostile founder: `MlsMember` + `GroupChannel` over one `MockRelay`. The
//! engine under test is always REAL, driven through the public Command
//! surface, so what these tests pin is the production ladder and not a
//! reimplementation of it.
//!
//! Prove-red instructions are on each test, because a coverage pin whose
//! deletion experiment is undocumented rots into a green test that proves
//! nothing (which is exactly how cluster H came to exist).

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;
use molt_net::invite::{self, JoinRequest, RitualMsg};
use molt_net::ritual_net::RitualNet;
use molt_net::MlsMember;
use nostr_relay_builder::MockRelay;

// ---------------------------------------------------------------------------
// the same engine harness the honest founding tests use
// ---------------------------------------------------------------------------

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for(
    w: &WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nsession: notice={:?} create.log={:?} join.log={:?}",
            s.notice,
            s.create.run.log,
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() })
        .await
        .expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    wait_for(w, "the relay probe to confirm the relay", |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

fn dialer() -> molt_net::dial::Dialer {
    molt_net::dial::Dialer::resolve("none", "local", 0).expect("direct dialer")
}

/// A founder engine mid-founding, with its link parsed: everything the
/// hostile side needs to address it (ticket, anchor, relays).
async fn founder_awaiting_a_join(
    root: &std::path::Path,
    url: &str,
) -> (WalletHandle, molt_engine::FoundingInvite) {
    let a = engine(root);
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("founding starts");
    let s = wait_for(&a, "the seat link to become joinable", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let invite =
        molt_engine::FoundingInvite::parse(&s.create.seats[0].link).expect("joinable link");
    (a, invite)
}

/// One joiner's material, all derived exactly as the production ritual does.
struct Joiner {
    identity_pk: String,
    key_package: String,
    /// The ticket-salted transport secret, for a `RitualNet` under this seat.
    nostr_sk: [u8; 32],
    /// …and its canonical x-only anchor.
    nostr_pk: String,
}

fn joiner(name: &str, seed: u8, ticket: &str) -> Joiner {
    let (sk, identity_pk) = molt_storage::derive_identity_key(&[seed; 32], name);
    let key_package = hex::encode(
        MlsMember::new(&sk, name)
            .expect("mls identity")
            .key_package()
            .expect("key package"),
    );
    let (nostr_sk, nostr_pk) = molt_net::nostr_identity(&[seed; 32], ticket);
    Joiner {
        identity_pk,
        key_package,
        nostr_sk,
        nostr_pk,
    }
}

// ---------------------------------------------------------------------------
// H1 — the proof-of-possession gate on the founder's ingest
// ---------------------------------------------------------------------------

/// **A join request claiming a transport key it did not sign with is
/// refused** (`founding.rs::cmd_net_join_requested`, the `is_nostr` arm).
///
/// The invite MAC proves the sender holds the TICKET, and nothing else: the
/// ticket is printed in the link, so anyone holding the link can mint a valid
/// v2 MAC over ANY `nostr_pk` — including a victim's. What upgrades the third
/// anchor from "chosen" to "possessed" is the gift wrap: NIP-59 verified its
/// seal, so the sender demonstrably holds THAT key, and the claim must equal
/// it.
///
/// Without the gate a link-holder anchors somebody else's transport key into
/// the roster and the republic id — forever-bytes, sealed by everyone's
/// signature.
///
/// **Prove red** (verified 2026-08-04): delete the `if is_nostr { … }` block
/// in `cmd_net_join_requested` — the impersonating request then anchors the
/// seat ("mallory activated invite 1 · key received") and `can_propose` flips
/// true.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_claiming_a_transport_key_it_did_not_sign_with_is_refused() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, inv) = founder_awaiting_a_join(&tmp.path().join("founder"), &url).await;
    let h = inv.handover;

    let mallory = joiner("mallory", 7, &h.ticket);
    let victim = joiner("petra", 9, &h.ticket);
    assert_ne!(
        mallory.nostr_pk, victim.nostr_pk,
        "the whole point is that the claimed key is not the sealing key"
    );

    // the attacker seals under ITS key and claims the victim's anchor. The
    // MAC is genuine — the ticket is in the link, so it proves nothing about
    // possession, which is exactly why the gate cannot be the MAC.
    let net = RitualNet::new(dialer(), vec![url.clone()], &mallory.nostr_sk)
        .expect("attacker transport");
    net.send_ritual(
        &h.npub,
        &RitualMsg::Join(JoinRequest {
            seat: h.seat,
            name: "mallory".to_string(),
            identity_pk: mallory.identity_pk.clone(),
            nostr_pk: victim.nostr_pk.clone(),
            mac: invite::join_mac(
                &h.ticket,
                "mallory",
                &mallory.identity_pk,
                &victim.nostr_pk,
            ),
            reply: None,
            key_package: mallory.key_package.clone(),
            relays: Vec::new(),
        }),
    )
    .await
    .expect("the wrap publishes — the refusal is the founder's, not the relay's");

    let s = wait_for(&a, "the founder to log its verdict on the request", |s| {
        s.create.run.log.iter().any(|l| l.contains("mallory"))
    })
    .await;
    assert!(
        s.create.run.log
            .iter()
            .any(|l| l.contains("transport key it did not sign with")),
        "the refusal must name the impersonation: {:?}",
        s.create.run.log
    );
    assert!(
        s.create.seats[0].member.is_empty(),
        "an impersonating request must not anchor the seat"
    );
    assert!(!s.create.can_propose, "…nor unlock deliberation");

    // CONTROL: the same attacker, now claiming the key it actually holds,
    // is accepted — the refusal did not spend the single-use ticket.
    net.send_ritual(
        &h.npub,
        &RitualMsg::Join(JoinRequest {
            seat: h.seat,
            name: "mallory".to_string(),
            identity_pk: mallory.identity_pk.clone(),
            nostr_pk: mallory.nostr_pk.clone(),
            mac: invite::join_mac(
                &h.ticket,
                "mallory",
                &mallory.identity_pk,
                &mallory.nostr_pk,
            ),
            reply: None,
            key_package: mallory.key_package.clone(),
            relays: Vec::new(),
        }),
    )
    .await
    .expect("the honest request publishes");

    let s = wait_for(&a, "the honest request to anchor the seat", |s| {
        s.create.can_propose
    })
    .await;
    assert_eq!(
        s.create.seats[0].member, "mallory",
        "a request that signs with the key it claims is the normal path"
    );
}

// ---------------------------------------------------------------------------
// H3 — the founder-identity guards on the joiner's 1059 inbox
// ---------------------------------------------------------------------------

/// **A 1059 frame from anybody but the link's founder cannot kill a join**
/// (`nostr_ritual.rs::member_join`, the `sender == h.npub` guards).
///
/// A joiner's anchor is not a secret: it rides its own JoinRequest, and on a
/// real relay the Welcome's `#p` tag publishes it. Any observer can therefore
/// gift-wrap a frame to it. Two of them END a join outright — `LinkSpent`
/// ("this link was already used") and a `WelcomePayload` with garbage MLS
/// bytes ("mls welcome: …") — so without the guards an unauthenticated
/// bystander can DoS every join in the republic.
///
/// The imposter here sends both, at the two moments they would land: before
/// the genuine acceptance, and after it while the joiner waits for the
/// Welcome. The join must reach the charter as if neither had happened.
///
/// **Prove red** (both verified 2026-08-04): drop the `if sender == h.npub`
/// guard from the `LinkSpent` arms and the join dies with "already used by
/// someone else"; drop it from the `Welcome` arms and it dies at
/// "mls welcome: mls wire: parsing welcome: …".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_1059_frame_from_anyone_but_the_link_founder_cannot_kill_a_join() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, inv) = founder_awaiting_a_join(&tmp.path().join("founder"), &url).await;
    let h = inv.handover;
    let link = molt_engine::FoundingInvite {
        info: inv.info,
        handover: h.clone(),
    }
    .render()
    .expect("re-render the link the joiner was given");

    // the imposter: a key with no relation to this founding at all
    let (imposter_sk, _imposter_pk) = molt_net::nostr_identity(&[42u8; 32], "not-this-ticket");
    let imposter =
        RitualNet::new(dialer(), vec![url.clone()], &imposter_sk).expect("imposter transport");

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");

    // the joiner's anchor is NOT a secret: it rides its own JoinRequest and
    // the Welcome's `#p` tag. Here it is derived from the phrase the join
    // surfaces, by the same two public functions the joiner itself uses.
    let s = wait_for(&b, "the join to surface its recovery phrase", |s| {
        !s.join.seed.is_empty()
    })
    .await;
    let entropy = molt_storage::seed_entropy(&s.join.seed).expect("entropy");
    let (_, victim_anchor) = molt_net::nostr_identity(&entropy, &h.ticket);

    // Both shots, from BEFORE the founder has accepted anything and for the
    // whole run: `LinkSpent` (ends the join in either wait loop) and a
    // `WelcomePayload` with the invite's exact relay list — so the
    // relay-honesty check is not what refuses it — and unusable MLS bytes.
    //
    // The timing is the test. Shooting only after the genuine acceptance
    // leaves the Welcome guards UNPINNED: the honest Welcome has already been
    // consumed by then, so the garbage never gets a chance to win. Starting
    // before it is what makes both guard pairs load-bearing.
    let spent = RitualMsg::LinkSpent {
        seat: h.seat,
        reason: String::new(),
    };
    let garbage = molt_net::welcome::WelcomePayload {
        welcome: b"garbage".to_vec(),
        rotation_seed: [9u8; 32],
        relays: h.relays.clone(),
    };
    let anchor = victim_anchor.clone();
    let shooter = {
        let imposter = imposter.clone();
        tokio::spawn(async move {
            loop {
                let _ = imposter.send_welcome(&anchor, &garbage).await;
                let _ = imposter.send_ritual(&anchor, &spent).await;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
    };

    // …the founder accepts the GENUINE request and proposes the charter
    wait_for(&a, "the founder to accept petra's join", |s| {
        s.create.can_propose
    })
    .await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess, decide together".to_string(),
    })
    .await
    .expect("charter proposed");

    let s = wait_for(&b, "petra to reach the charter despite the imposter", |s| {
        s.join.awaiting_ratify || s.join.run.outcome == 2
    })
    .await;
    shooter.abort();
    assert!(
        s.join.awaiting_ratify,
        "an unauthenticated bystander must not be able to end a join: {:?}",
        s.join.run.log
    );
    assert_eq!(s.join.proposed_agenda, "play chess, decide together");
}
