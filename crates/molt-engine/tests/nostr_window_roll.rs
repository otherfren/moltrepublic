// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Cluster E, engine level — the deafness must reach the WIZARDS.
//!
//! This is the test that keeps the fix from being an inert refactor: the
//! compiler forces every caller to handle the new `GroupRecv::Deaf`, but it
//! cannot stop them writing `Deaf(_) => continue`, which would restore the
//! exact silence the cluster is about.
//!
//! OWN TEST BINARY: `shift_window_clock_for_tests` is process-global.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use molt_engine::WalletHandle;
use molt_net::envelope::H_WINDOW;
use molt_net::ritual_net::shift_window_clock_for_tests;
use nostr_relay_builder::MockRelay;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn wait_for(
    w: &WalletHandle,
    what: &str,
    secs: u64,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\ncreate.log={:?}\njoin.log={:?}",
            s.create.run.log,
            s.join.run.log
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() }).await.expect("relay add");
    w.execute(Command::RelayConfirm { url: url.to_string(), accept_clearnet: true })
        .await
        .expect("relay confirm");
    // B4: the confirmation lands on the PROBE's verdict, off-actor — an
    // unusable relay never becomes a confirmed one
    wait_for(w, "the relay probe to confirm the relay", 30, |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true }).await.expect("unlock");
}

struct Cuttable {
    port: u16,
    enabled: Arc<AtomicBool>,
    forwards: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Cuttable {
    async fn run(target: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let port = listener.local_addr().expect("addr").port();
        let enabled = Arc::new(AtomicBool::new(true));
        let forwards: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let on = enabled.clone();
        let fw = forwards.clone();
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                if !on.load(Ordering::SeqCst) {
                    drop(inbound);
                    continue;
                }
                let target = target.clone();
                fw.lock().await.push(tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(&target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                }));
            }
        });
        Self { port, enabled, forwards }
    }

    async fn cut(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        for f in self.forwards.lock().await.drain(..) {
            f.abort();
        }
    }

    fn restore(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }
}

/// KEYSTONE — a deaf group channel is SURFACED to both wizards, kills
/// neither run, and heals.
///
/// The failure never left molt-net (a `tracing::debug!`), no Command carried
/// it, and both callers treated it as idle — so both nodes spun in silence
/// while their run logs said nothing at all.
///
/// It must also stay NON-FATAL: `CreatePropose` is one-shot, so a founding
/// aborted on a transient relay blip would lose every collected signature and
/// force a re-mint. Loud forever, never terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deaf_group_channel_is_surfaced_to_both_wizards_and_heals() {
    let relay = MockRelay::run().await.expect("relay");
    let direct = relay.url().await.to_string();
    let proxy = Cuttable::run(direct.trim_start_matches("ws://").to_string()).await;
    let url = format!("ws://127.0.0.1:{}", proxy.port);
    let tmp = tempfile::tempdir().expect("tmp");

    let a = engine(&tmp.path().join("founder"));
    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Rolling".to_string(),
        member: "walter".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create starts");
    let s = wait_for(&a, "a joinable link", 30, |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;

    let b = engine(&tmp.path().join("joiner"));
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart {
        invite: s.create.seats[0].link.clone(),
        member: "petra".to_string(),
    })
    .await
    .expect("join starts");
    // all-joined ⇒ the group is born ⇒ BOTH sides hold a live GroupSub
    wait_for(&a, "the group to be born", 30, |s| s.create.can_propose).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // the relay vanishes AND the window rolls: the re-placement cannot succeed
    proxy.cut().await;
    shift_window_clock_for_tests(i64::try_from(H_WINDOW).expect("window fits"));

    let sb = wait_for(&b, "the joiner to report the deafness", 40, |s| {
        s.join.run.log.iter().any(|l| l.contains("cannot hear the group channel"))
    })
    .await;
    assert_eq!(sb.join.run.outcome, 0, "a relay blip must NOT fail the join");
    let sa = wait_for(&a, "the founder to report the deafness", 40, |s| {
        s.create.run.log.iter().any(|l| l.contains("cannot hear the group channel"))
    })
    .await;
    assert_eq!(sa.create.run.outcome, 0, "…nor the one-shot founding");
    // the repeating note must not stack: it is deduped against the last line
    let repeats = sb
        .join
        .run
        .log
        .iter()
        .filter(|l| l.contains("cannot hear the group channel"))
        .count();
    assert!(repeats <= 2, "the deaf note must not stack per poll, saw {repeats}");

    // …and the deafness was survivable: heal, then finish the founding
    proxy.restore();
    shift_window_clock_for_tests(0);

    // Once frames flow again the pair must SETTLE, not alternate. `deaf` used
    // to be cleared only by a successful re-placement, so a delivered frame
    // left it set: every caller printed "the group channel is back" on the
    // frame and "cannot hear the group channel" on the next budget expiry,
    // both false when written, two lines every RECV_SLICE forever. The
    // last-line dedup collapses repeats but not an alternating pair.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let settled = read_session(&b).await;
    let flapping = settled
        .join
        .run
        .log
        .windows(2)
        .filter(|w| {
            w[0].contains("group channel is back") && w[1].contains("cannot hear the group channel")
        })
        .count();
    assert_eq!(
        flapping, 0,
        "the deaf/back pair must not alternate once frames flow: {:?}",
        settled.join.run.log
    );
    a.execute(Command::CreatePropose {
        name: "Rolling".to_string(),
        agenda: "survive a window roll".to_string(),
    })
    .await
    .expect("proposed");
    wait_for(&b, "petra to see the charter", 60, |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    wait_for(&a, "the founding to seal", 60, |s| s.create.run.outcome == 1).await;
}
