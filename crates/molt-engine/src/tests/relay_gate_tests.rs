// SPDX-License-Identifier: GPL-3.0-or-later

//! `relay_pool.md` §3 rule 3 at the engine: what the engine hands its relay
//! tasks stops at a withdrawal, and a relay probe racing one confirms
//! nothing. Every "no dial" is read off a proxy's accept counter.

use super::support::*;
use crate::*;

/// A loopback proxy in front of a relay that holds each connection's
/// forwarding until released, counting accepts.
struct HeldProxy {
    url: String,
    accepts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    held: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<tokio::sync::oneshot::Sender<()>>>,
}

impl HeldProxy {
    async fn run(target: String, hold: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let accepts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (held_tx, held) = tokio::sync::mpsc::channel(16);
        let count = accepts.clone();
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (target, held_tx) = (target.clone(), held_tx.clone());
                tokio::spawn(async move {
                    if hold {
                        let (release, released) = tokio::sync::oneshot::channel();
                        if held_tx.send(release).await.is_err() || released.await.is_err() {
                            return;
                        }
                    }
                    if let Ok(mut out) = tokio::net::TcpStream::connect(&target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                    }
                });
            }
        });
        Self { url: format!("ws://127.0.0.1:{port}"), accepts, held: tokio::sync::Mutex::new(held) }
    }

    fn accepts(&self) -> usize {
        self.accepts.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn next_held(&self) -> tokio::sync::oneshot::Sender<()> {
        tokio::time::timeout(std::time::Duration::from_secs(15), self.held.lock().await.recv())
            .await
            .expect("a connection in time")
            .expect("proxy alive")
    }
}

async fn relay() -> (nostr_relay_builder::MockRelay, String) {
    let relay = nostr_relay_builder::MockRelay::run().await.expect("relay");
    let target = relay.url().await.to_string();
    let target = target.trim_start_matches("ws://").trim_end_matches('/').to_string();
    (relay, target)
}

fn wire(st: &mut State) -> (mpsc::Sender<Envelope>, mpsc::Receiver<Envelope>) {
    let (tx, rx) = mpsc::channel(8);
    st.cmd_tx = tx.downgrade();
    (tx, rx)
}

async fn probe_verdict(st: &mut State, rx: &mut mpsc::Receiver<Envelope>) {
    let env = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
        .await
        .expect("a verdict in time")
        .expect("channel open");
    match env.cmd {
        molt_core::Command::NetRelayProbed { url, error, unreachable, confirm } => {
            st.cmd_net_relay_probed(url, error, unreachable, confirm).expect("ack");
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn confirmed(st: &State, url: &str) -> bool {
    st.session.settings.relays.iter().any(|r| r.url == url && r.confirmed)
}

/// H1: a revoke, or the non-onion switch going off, while the confirmation
/// probe runs stops the probe at once and its verdict confirms nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_withdrawal_during_the_confirm_probe_stops_it_and_confirms_nothing() {
    let (_relay, target) = relay().await;
    for switch_off in [false, true] {
        let proxy = HeldProxy::run(target.clone(), true).await;
        let mut st = plain_state();
        let (_tx, mut rx) = wire(&mut st);
        st.cmd_relay_add(proxy.url.clone()).expect("add");
        st.cmd_relay_confirm(proxy.url.clone(), true).expect("confirm starts the probe");
        let release = proxy.next_held().await;
        if switch_off {
            st.cmd_relay_clearnet_session(false).expect("switch off");
        } else {
            st.cmd_relay_revoke(proxy.url.clone()).expect("revoke");
        }
        let _ = release.send(());
        probe_verdict(&mut st, &mut rx).await;
        assert!(!confirmed(&st, &proxy.url), "switch_off={switch_off}: withdrawn, yet confirmed");
        assert!(st.pending_relay_confirms.is_empty(), "switch_off={switch_off}: still pending");
        assert_eq!(proxy.accepts(), 1, "switch_off={switch_off}: the probe dialed on");
    }
}

/// The counter-case: left alone, the same probe confirms its relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_undisturbed_confirm_probe_confirms() {
    let (_relay, target) = relay().await;
    let proxy = HeldProxy::run(target, false).await;
    let mut st = plain_state();
    let (_tx, mut rx) = wire(&mut st);
    st.cmd_relay_add(proxy.url.clone()).expect("add");
    st.cmd_relay_confirm(proxy.url.clone(), true).expect("confirm");
    probe_verdict(&mut st, &mut rx).await;
    assert!(confirmed(&st, &proxy.url));
}

/// L3: the channels the engine hands its relay tasks - downloads, vault and
/// wiki base fetches, mirrors (`nostr_file_channel`), recovery frames and
/// the seat inbox (`recover_net`) - taken BEFORE a revoke dial nothing
/// after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_engines_relay_channels_dial_nothing_after_a_revoke() {
    let (_relay, target) = relay().await;
    let proxy = HeldProxy::run(target, false).await;
    let mut st = plain_state();
    st.transport_kind = Some(molt_core::TransportKind::Nostr);
    st.nostr = Some(crate::NostrTransport {
        sk: zeroize::Zeroizing::new(vec![1u8; 32]),
        relays: vec![proxy.url.clone()],
        rotation_seed: [7u8; 32],
    });
    st.session.settings.relays =
        vec![molt_core::relay::RelayEntry { url: proxy.url.clone(), confirmed: true }];
    st.clearnet_session = true;
    let files = st.nostr_file_channel().expect("file channel");
    let ritual = st.recover_net().expect("ritual endpoint");

    st.cmd_relay_revoke(proxy.url.clone()).expect("revoke");
    let withdrawn = |r: Result<(), molt_net::NetError>, what: &str| {
        assert!(matches!(r, Err(molt_net::NetError::Withdrawn)), "{what}: {r:?}");
    };
    withdrawn(files.publish_file_content_at("cGllY2U", 1).await.map(|_| ()), "series piece");
    withdrawn(files.subscribe_files_at(1).await.map(|_| ()), "download / base fetch");
    let msg = molt_net::invite::RitualMsg::Aborted { reason: "a".into() };
    withdrawn(ritual.send_ritual(&ritual.pk_hex(), &msg).await.map(|_| ()), "one-shot publish");
    withdrawn(ritual.inbox().await.map(|_| ()), "seat inbox");
    assert_eq!(proxy.accepts(), 0, "revoked, yet the relay was dialed");
}

/// Every dialer the engine builds comes from `dialer_for` (gated) - except
/// the probe of a relay being decided on, which carries its own keeper.
/// A new ungated construction in engine code is a finding.
#[test]
fn every_engine_dialer_comes_from_dialer_for() {
    const PATTERNS: [&str; 6] =
        ["Dialer::resolve(", "Dialer::direct(", "Dialer::default(", "Dialer::on(", ".ungated()", "Dialer {"];
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("src dir").flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if path.is_dir() {
                if name != "tests" {
                    walk(&path, out);
                }
                continue;
            }
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read");
            let code = text.split("#[cfg(test)]\nmod tests").next().unwrap_or("");
            for line in code.lines().filter(|l| !l.trim_start().starts_with("//")) {
                for p in PATTERNS {
                    if line.contains(p) {
                        out.push(format!("{name}: {p}"));
                    }
                }
            }
        }
    }
    let mut found = Vec::new();
    walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
    found.sort();
    assert_eq!(
        found,
        vec![
            "session.rs: .ungated()".to_string(),
            "session.rs: Dialer::resolve(".to_string(),
            "session.rs: Dialer::resolve(".to_string(),
        ],
        "an engine dialer bypasses dialer_for"
    );
}
