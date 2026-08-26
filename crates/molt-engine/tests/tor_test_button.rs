// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The anonymity panel's "Test Tor" button drives
//! [`molt_core::Command::NetTestTor`]: the engine resolves the fail-closed
//! dialer from the request (falling back to the saved settings for empty
//! fields), picks the probe target from the operator's OWN relay pool, runs
//! the two-rung probe **off the actor**, and feeds the honest verdict back as
//! `NetTestTorResult` into `session.tor_test`. The GUI button and the
//! `net_test_tor` MCP tool are thin wrappers over exactly this.
//!
//! No real network and no real Tor: every rung runs against loopback
//! listeners — a dead port, a blackhole listener, and a fake SOCKS5 proxy.

use std::time::Duration;

use molt_core::{Command, GroupConfig, Reply, SessionView, TorTestState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A clearnet relay whose host resolves nowhere — under SOCKS5h the name
/// never leaves the proxy, which is the point.
const RELAY: &str = "wss://relay.example.test";

/// A port nothing listens on.
async fn dead_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    l.local_addr().expect("addr").port()
}

/// Accept-and-close listener: something answers TCP, nothing speaks SOCKS.
async fn blackhole_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            drop(s);
        }
    });
    port
}

/// A fake SOCKS5 proxy completing RFC 1928 + RFC 1929 and answering CONNECT
/// with success.
async fn fake_socks5() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind socks");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = [0u8; 2];
                if s.read_exact(&mut head).await.is_err() {
                    return;
                }
                let mut methods = vec![0u8; usize::from(head[1])];
                if s.read_exact(&mut methods).await.is_err() {
                    return;
                }
                if s.write_all(&[0x05, 0x02]).await.is_err() {
                    return;
                }
                let mut ah = [0u8; 2];
                if s.read_exact(&mut ah).await.is_err() {
                    return;
                }
                let mut user = vec![0u8; usize::from(ah[1])];
                if s.read_exact(&mut user).await.is_err() {
                    return;
                }
                let mut pl = [0u8; 1];
                if s.read_exact(&mut pl).await.is_err() {
                    return;
                }
                let mut pass = vec![0u8; usize::from(pl[0])];
                if s.read_exact(&mut pass).await.is_err() {
                    return;
                }
                if s.write_all(&[0x01, 0x00]).await.is_err() {
                    return;
                }
                let mut ch = [0u8; 4];
                if s.read_exact(&mut ch).await.is_err() {
                    return;
                }
                let mut hl = [0u8; 1];
                if s.read_exact(&mut hl).await.is_err() {
                    return;
                }
                let mut host_port = vec![0u8; usize::from(hl[0]) + 2];
                if s.read_exact(&mut host_port).await.is_err() {
                    return;
                }
                let _ = s
                    .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
        }
    });
    port
}

/// Issue one NetTestTor and poll `session.tor_test` until it settles.
async fn run_test(w: &molt_engine::WalletHandle, cmd: Command) -> molt_core::TorTest {
    w.execute(cmd).await.expect("NetTestTor accepted");
    for _ in 0..200 {
        let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
            panic!("read session failed");
        };
        if !matches!(
            sv.tor_test.state,
            TorTestState::Idle | TorTestState::Testing
        ) {
            return sv.tor_test;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("tor_test never settled");
}

fn engine() -> molt_engine::WalletHandle {
    molt_engine::spawn(GroupConfig::demo(), SessionView::default())
}

/// A draft test of `network = tor` at `mode`/`port` (what the settings panel
/// sends before the operator has saved).
fn draft(mode: &str, port: u16) -> Command {
    Command::NetTestTor {
        network: "tor".to_string(),
        mode: mode.to_string(),
        port,
    }
}

/// KEYSTONE — with Tor off there is nothing to test, and the engine says so
/// instead of running a probe. `Off` is a refusal, never a verdict about Tor.

/// Save a test's settings the way the GUI does: the host posture and the
/// secrets through their own door (`SetNodePosture`), the rest wholesale —
/// a wholesale save keeps the stored posture (review 2026-08-26).
async fn save_all(w: &molt_engine::WalletHandle, settings: molt_core::SessionSettings) -> Result<Reply, molt_core::MoltError> {
    w.execute(Command::SetNodePosture { posture: molt_core::NodePosture::of(&settings) })
        .await?;
    w.execute(Command::SaveSettings { settings }).await
}

#[tokio::test]
async fn tor_off_is_a_refusal_not_a_test() {
    let w = engine();
    // default settings: anonymity = "none"
    let v = run_test(
        &w,
        Command::NetTestTor {
            network: String::new(),
            mode: String::new(),
            port: 0,
        },
    )
    .await;
    assert_eq!(v.state, TorTestState::Off, "{v:?}");
    assert!(v.proxy.is_empty() && v.target.is_empty(), "{v:?}");
    assert!(!v.detail.is_empty(), "the refusal names its reason: {v:?}");
}

/// A Tor setting the fail-closed dialer refuses to resolve is a CONFIG
/// failure — reported as such, with no dial attempted.
#[tokio::test]
async fn an_unresolvable_tor_setting_is_misconfigured() {
    let w = engine();
    let v = run_test(&w, draft("nonsense", 9050)).await;
    assert_eq!(v.state, TorTestState::Misconfigured, "{v:?}");
    assert!(v.detail.contains("nonsense"), "{v:?}");
}

/// The verdict names the network the operator ACTUALLY configured — the
/// effective label folds `nym` into `none`, and reporting "your network is
/// none" to someone who set `nym` describes a setting they never made. The
/// echoed value is bounded and control-character-free: a draft arrives
/// straight from an MCP argument and must not smuggle escapes into a line
/// the GUI, the MCP answer and the log all render.
#[tokio::test]
async fn the_refusal_names_the_configured_network_safely() {
    let w = engine();
    let v = run_test(
        &w,
        Command::NetTestTor {
            network: "nym".to_string(),
            mode: String::new(),
            port: 0,
        },
    )
    .await;
    assert_eq!(v.state, TorTestState::Off, "{v:?}");
    assert!(v.detail.contains("nym"), "names what was configured: {v:?}");

    let hostile = format!("none\r\nfake: tor works{}", "x".repeat(200));
    let v = run_test(
        &w,
        Command::NetTestTor {
            network: hostile,
            mode: String::new(),
            port: 0,
        },
    )
    .await;
    assert_eq!(v.state, TorTestState::Off, "{v:?}");
    assert!(
        !v.detail.contains('\n') && !v.detail.contains('\r'),
        "control characters must never reach the detail line: {v:?}"
    );
    assert!(v.detail.len() < 200, "the echo is bounded: {v:?}");
}

/// Nothing listening at the SOCKS address → there is no Tor daemon there.
#[tokio::test]
async fn a_dead_socks_port_reports_no_daemon() {
    let w = engine();
    let v = run_test(&w, draft("local", dead_port().await)).await;
    assert_eq!(v.state, TorTestState::NoProxy, "{v:?}");
}

/// KEYSTONE — the honest partial rung. A socket answers at the SOCKS
/// address, but the operator has no relay to route through, so NO circuit
/// was proven and the probe never invents a host to dial (ADR-0004).
#[tokio::test]
async fn a_listening_proxy_without_a_relay_is_the_partial_rung() {
    let w = engine();
    let port = blackhole_port().await;
    let v = run_test(&w, draft("local", port)).await;
    assert_eq!(v.state, TorTestState::ProxyOnly, "{v:?}");
    assert_eq!(v.proxy, format!("127.0.0.1:{port}"));
    assert!(v.target.is_empty(), "nothing was dialed: {v:?}");
}

/// The wiring of the full rung: a relay from the operator's OWN confirmed
/// pool is picked as the target and really dialed through the proxy.
///
/// It ends at `CircuitFailed`, and that is the POINT (review finding
/// 2026-07-31): the fake proxy answers "connected" while connecting to
/// nothing, and the circuit rung now completes a real relay handshake, so a
/// lying proxy can no longer earn the end-to-end claim. The rung's success
/// path is proven in `molt-net/tests/tor_probe.rs`, against a proxy that
/// actually forwards to a relay.
#[tokio::test]
async fn a_confirmed_relay_is_the_target_and_a_lying_proxy_earns_no_circuit() {
    let w = engine();
    w.execute(Command::RelayAdd {
        url: RELAY.to_string(),
    })
    .await
    .expect("relay added");
    w.execute(Command::RelayConfirm {
        url: RELAY.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirmed");
    // B4: the confirmation lands on the PROBE's verdict (this fictional
    // relay is unreachable -> unverified, and the consent stands)
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let s = match w.execute(Command::ReadSession).await.expect("read") {
            molt_core::Reply::Session(s) => s,
            other => panic!("unexpected: {other:?}"),
        };
        if s.settings.relays.iter().any(|r| r.confirmed) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "probe verdict never landed");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("clearnet unlocked");
    let v = run_test(&w, draft("local", fake_socks5().await)).await;
    assert_eq!(v.target, RELAY, "the operator's own relay was the target: {v:?}");
    assert_ne!(
        v.state,
        TorTestState::Circuit,
        "a proxy that connects to nothing must not read as a working Tor: {v:?}"
    );
    assert_eq!(v.state, TorTestState::CircuitFailed, "{v:?}");
}

/// A relay whose clearnet dialing is switched OFF is not dialable, so the
/// probe has no target — the partial rung, not a fabricated circuit. (Since
/// the ADR-0004 amendment an acknowledged confirmation ENABLES dialing, so
/// the blocked state is now reached by switching it back off — the operator
/// going dark deliberately.)
#[tokio::test]
async fn a_relay_with_clearnet_switched_off_is_not_a_probe_target() {
    let w = engine();
    w.execute(Command::RelayAdd {
        url: RELAY.to_string(),
    })
    .await
    .expect("relay added");
    w.execute(Command::RelayConfirm {
        url: RELAY.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirmed");
    w.execute(Command::RelayClearnetSession { unlock: false })
        .await
        .expect("go dark");
    let v = run_test(&w, draft("local", blackhole_port().await)).await;
    assert_eq!(v.state, TorTestState::ProxyOnly, "{v:?}");
    assert!(v.target.is_empty(), "{v:?}");
    // TARGETGAP: the verdict names the ACTUAL cause instead of the old
    // fixed hedge. In this fixture the confirm probe never lands (no live
    // relay), so the truthful cause is the unconfirmed pool; the
    // switched-off and empty-pool causes are pinned pairwise-distinct in
    // molt-net's the_verdict_names_the_actual_no_target_cause.
    assert!(
        v.detail.contains("confirmed") && !v.detail.contains("relay settings"),
        "the verdict must name the actual cause, not the hedge: {v:?}"
    );
}

/// A verdict describes ONE anonymity configuration. Changing the network,
/// mode or port makes it stale — it must not survive to claim the new,
/// unprobed setting works (the `s3_test` rule, applied here).
#[tokio::test]
async fn changing_the_anonymity_settings_clears_the_stale_verdict() {
    let w = engine();
    let v = run_test(&w, draft("local", dead_port().await)).await;
    assert_eq!(v.state, TorTestState::NoProxy);

    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    let mut settings = sv.settings.clone();
    settings.tor_port = settings.tor_port.wrapping_add(1);
    save_all(&w, settings)
        .await
        .expect("settings saved");
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    assert_eq!(
        sv.tor_test.state,
        TorTestState::Idle,
        "a changed anonymity setting drops the stale verdict"
    );
}

/// The saved settings drive the probe when the request carries no draft —
/// the MCP posture (an agent may simply ask "is Tor working?").
#[tokio::test]
async fn an_empty_request_tests_the_saved_settings() {
    let w = engine();
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    let mut settings = sv.settings.clone();
    settings.anonymity = "tor".to_string();
    settings.tor_mode = "local".to_string();
    settings.tor_port = dead_port().await;
    save_all(&w, settings)
        .await
        .expect("settings saved");
    let v = run_test(
        &w,
        Command::NetTestTor {
            network: String::new(),
            mode: String::new(),
            port: 0,
        },
    )
    .await;
    assert_eq!(v.state, TorTestState::NoProxy, "{v:?}");
}
