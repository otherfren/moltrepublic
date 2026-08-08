// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **N5.2 keystone** — the kind-445 group runtime carries a WorkspaceEvent
//! between two real engines over one relay, with no mesh and no queues.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

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

/// ADR-0004: nothing is pre-configured — each node adds and confirms the
/// relay itself, then unlocks the clearnet/local session.
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
    // B4: the confirmation lands on the PROBE's verdict, off-actor — an
    // unusable relay never becomes a confirmed one
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

/// Found a real 2-of-2 "Chess Club" over one in-process relay, exactly as
/// `nostr_founding.rs`'s capstone does, and hand back both live engines.
async fn found_two_of_two(root: &std::path::Path, url: &str) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    adopt_relay(&a, url).await;
    a.execute(Command::CreateStart {
        name: "Chess Club".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("a production founding starts over the confirmed relay");

    let s = wait_for(&a, "the seat link to become a joinable v2 link", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&root.join("joiner"));
    adopt_relay(&b, url).await;
    b.execute(Command::JoinStart {
        invite: link,
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");

    wait_for(&a, "the founder to accept petra's join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Chess Club".to_string(),
        agenda: "play chess, decide together".to_string(),
    })
    .await
    .expect("charter proposed");

    wait_for(&b, "petra to see the proposed charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");

    wait_for(&a, "the founding to seal on the founder", |s| {
        s.create.run.outcome == 1 && s.screen == molt_core::Screen::Main
    })
    .await;
    wait_for(&b, "the join to seal on petra", |s| {
        s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()
    })
    .await;
    // entering is gated on the phrase-backup step now (2026-08-08)
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "petra to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// Read the chat surface's applied rows.
async fn read_chat(w: &WalletHandle) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface: molt_core::Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

/// **N5.2 KEYSTONE** — two engines converge a chat message over one relay,
/// with no mesh and no queues.
///
/// The REOPEN is the anti-inert core, not ceremony. `maybe_finalize` takes the
/// ritual and `NostrRitual::drop` aborts every inbound task it owns, so after
/// closing both workspaces no ritual subscription and no ritual channel exist
/// anywhere in the process. Anything that arrives after that arrived over a
/// runtime rebuilt from `transport.state` alone — which is the thing under
/// test. Without the reopen this could pass while a leftover ritual
/// subscription did the work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_converge_a_chat_message_over_one_relay() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let (a, b) = found_two_of_two(tmp.path(), &url).await;
    let ws_a = read_session(&a).await.active_workspace.clone();
    let ws_b = read_session(&b).await.active_workspace.clone();

    // every ritual task dies here
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
    a.execute(Command::OpenWorkspace { id: ws_a })
        .await
        .expect("reopen a");
    b.execute(Command::OpenWorkspace { id: ws_b })
        .await
        .expect("reopen b");

    a.execute(Command::Chat {
        body: "over the relay".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("walter speaks");

    // …and it reaches petra, authored by walter — the author matters: a row
    // that arrived with the wrong `from` would mean the envelope was
    // re-authored somewhere, not carried
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let rows = read_chat(&b).await;
        if rows.iter().any(|r| {
            r.get("body").and_then(|v| v.as_str()) == Some("over the relay")
                && r.get("from").and_then(|v| v.as_str()) == Some("walter")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat message never crossed the relay; petra sees {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // …and again after a SECOND reopen, which is what proves the ratchet was
    // persisted. The runtime advances the MLS sender ratchet on every publish;
    // if the close did not merge it back into `transport.state`, the reopen
    // restores the founding blob and this second message REUSES a sender
    // generation — which every peer replay-rejects and silently drops. The
    // first round cannot catch that, because it starts from the founding blob
    // either way.
    let ws_a = read_session(&a).await.active_workspace.clone();
    let ws_b = read_session(&b).await.active_workspace.clone();
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
    a.execute(Command::OpenWorkspace { id: ws_a }).await.expect("reopen a");
    b.execute(Command::OpenWorkspace { id: ws_b }).await.expect("reopen b");

    a.execute(Command::Chat {
        body: "and again".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("walter speaks again");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let rows = read_chat(&b).await;
        if rows
            .iter()
            .any(|r| r.get("body").and_then(|v| v.as_str()) == Some("and again"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second message never crossed — the ratchet regressed on reopen; petra sees {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}

/// **N5.3** — a broadcast ACK moves the sender's proven floor.
///
/// This is the mechanism the whole guarantee rests on, and on a broadcast it
/// could not work by accident: log seqs are node-private (every node stamps
/// every envelope from its own `next_seq`), so petra's sheet has to name
/// WHOSE acceptance each window describes, and walter has to look up his own
/// name in it. Get that inversion backwards — ship the window describing
/// petra instead of the one describing walter — and it still compiles, still
/// parses, and silently advances the wrong floor.
///
/// The proof is on DISK, in walter's `transport.state`: `ack_seen` latched and
/// a floor above zero for petra.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broadcast_ack_moves_the_senders_proven_floor() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");

    let (a, b) = found_two_of_two(tmp.path(), &url).await;
    let ws_a = read_session(&a).await.active_workspace.clone();

    a.execute(Command::Chat {
        body: "prove it".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("walter speaks");

    // petra receives it, accepts it, and acks — the ack rides the same 445
    // channel as everything else
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let rows = read_chat(&b).await;
        if rows
            .iter()
            .any(|r| r.get("body").and_then(|v| v.as_str()) == Some("prove it"))
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the message never arrived");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // …and walter's own bookkeeping records it. The ack is debounced
    // (ACK_DEBOUNCE_SECS) and flushed on the 1 s delivery beat, so give the
    // round trip room, then close ONCE — closing repeatedly would mostly keep
    // walter shut while petra's sheet was arriving.
    tokio::time::sleep(Duration::from_secs(8)).await;
    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");

    let root_a = tmp.path().join("founder");
    let dir = molt_storage::find_workspace_dir(&root_a, &ws_a).expect("walter's dir");
    let (ws, _) = molt_storage::open_workspace(&dir).expect("open walter");
    let ts = ws.read_transport_state();
    let cursor = ts
        .outbound
        .get("petra")
        .unwrap_or_else(|| panic!("no floor for petra; outbound = {:?}", ts.outbound));
    assert!(
        cursor.ack_seen,
        "petra's sheet must latch ack_seen — without it group_floor stays None and nothing is ever proven"
    );
    assert!(
        cursor.acked_floor > 0,
        "…and lift the floor above zero, got {}",
        cursor.acked_floor
    );
}

/// B4 at the engine seam: an UNREACHABLE relay (down right now — or onion
/// while Tor is off) cannot be judged, so the operator's consent stands:
/// the entry confirms, and the verdict says so honestly
/// (`relay-unverified:`). Without this middle class a relay's downtime
/// would veto the operator, and no onion relay could ever be confirmed
/// before Tor is up. The add itself stays dial-free and safe (ADR-0004).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_relay_confirms_unverified_by_name() {
    let tmp = tempfile::tempdir().expect("tmp");
    let w = engine(tmp.path());
    // nothing listens here: bind a port, learn it, drop the listener
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = l.local_addr().expect("addr").port();
        drop(l);
        format!("ws://127.0.0.1:{port}")
    };
    w.execute(Command::RelayAdd { url: dead.clone() })
        .await
        .expect("adding is dial-free and always safe");
    w.execute(Command::RelayConfirm { url: dead.clone(), accept_clearnet: true })
        .await
        .expect("the confirm is acked; the PROBE's verdict decides");
    let s = wait_for(&w, "the probe verdict to land as unverified", |s| {
        s.notice.starts_with("relay-unverified:")
    })
    .await;
    assert!(
        s.settings.relays.iter().any(|r| r.confirmed),
        "the operator consented and the relay could not be judged - the \
         confirmation stands, honestly marked: {:?}",
        s.settings.relays
    );
}

/// B4's hard half: a relay that ANSWERED and disqualified itself (no kind
/// 445, no retention, tiny cap) is NEVER confirmed — the verdict names the
/// refusal and the entry stays exactly as unconfirmed (= inert) as it was.
/// The verdict is injected here; its honest production is pinned in
/// molt-net's probe tests against scripted relay doubles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unusable_relay_is_never_confirmed() {
    let tmp = tempfile::tempdir().expect("tmp");
    let w = engine(tmp.path());
    let relay = MockRelay::run().await.expect("relay");
    let url = relay.url().await.to_string();
    w.execute(Command::RelayAdd { url: url.clone() }).await.expect("add");
    let stored = read_session(&w).await.settings.relays[0].url.clone();
    w.execute(Command::NetRelayProbed {
        url: stored,
        error: "does not accept kind 445: blocked".to_string(),
        unreachable: false,
        confirm: true,
    })
    .await
    .expect("verdict");
    let s = wait_for(&w, "the refusal to land", |s| s.notice.starts_with("relay-refused:")).await;
    assert!(
        s.settings.relays.iter().all(|r| !r.confirmed),
        "an unusable relay must never become a confirmed one: {:?}",
        s.settings.relays
    );
    assert!(s.notice.contains("kind 445"), "the one reason travels: {:?}", s.notice);
}
