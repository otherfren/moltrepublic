// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The relay pool through the REAL command surface — the same path an MCP
//! agent drives (`docs_archive/transport/relay_pool.md`). These pin the promises a
//! user relies on: nothing is pre-trusted, adding never connects, and a
//! clearnet relay needs an explicit acknowledgement of its exposure before a
//! single packet leaves — an acknowledgement that is then REMEMBERED
//! (ADR-0004 amendment, 2026-07-31).

use molt_core::relay::{RelayBlock, RelayKind};
use molt_core::{Command, GroupConfig, Reply, SessionView};
use molt_engine::spawn;

const ONION: &str = "wss://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion";
const CLEARNET: &str = "wss://relay.example.org";

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

async fn session(w: &molt_engine::WalletHandle) -> SessionView {
    match w.execute(Command::ReadSession).await.expect("read") {
        Reply::Session(s) => *s,
        other => panic!("unexpected reply: {other:?}"),
    }
}

/// B4: the confirmation lands on the PROBE's verdict, off-actor — these
/// fictional relays are unreachable, so the verdict is "unverified" and
/// the operator's acknowledged consent stands. Wait for it.
async fn wait_confirmed(w: &molt_engine::WalletHandle, url: &str) -> SessionView {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let s = session(w).await;
        if s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
        {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the probe verdict never confirmed {url}: notice={:?}",
            s.notice
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// KEYSTONE — a fresh node ships with no relay at all, and adding one does
/// NOT connect: it lands unconfirmed.
#[test]
fn a_fresh_node_has_no_relays_and_adding_one_does_not_connect() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let s = session(&w).await;
        assert!(s.settings.relays.is_empty(), "nothing is pre-trusted");
        assert!(s.relays.is_empty(), "and nothing is dialable");

        w.execute(Command::RelayAdd { url: ONION.to_string() })
            .await
            .expect("add");
        let s = session(&w).await;
        assert_eq!(s.relays.len(), 1);
        assert_eq!(s.relays[0].url, ONION);
        assert_eq!(s.relays[0].kind, RelayKind::Onion);
        assert!(!s.relays[0].confirmed, "adding never confirms");
        assert_eq!(
            s.relays[0].blocked,
            Some(RelayBlock::Unconfirmed),
            "an added-but-unconfirmed relay is not dialed"
        );
    });
}

/// KEYSTONE — the clearnet gate: confirming a clearnet relay without the
/// explicit acknowledgement is REFUSED (the same refusal an MCP agent hits).
///
/// **Changed 2026-07-31 (user decision, ADR-0004 amendment):** the
/// acknowledged confirmation is now DURABLE and SUFFICIENT — it activates
/// clearnet dialing and is persisted, so it survives a restart. The old
/// design demanded a SECOND, per-session unlock that reset on every start;
/// with a hand-edited config and repeated restarts that turned "confirm
/// once" into "confirm, restart, unlock, restart, unlock, …" and made the
/// node unusable for its actual purpose. The consent moment stays exactly
/// where it was (`accept_clearnet` on the confirm); what changed is that
/// consent is now REMEMBERED. The session toggle survives as a deliberate
/// OFF switch, and turning it off is remembered too.
#[test]
fn an_acknowledged_clearnet_confirmation_is_durable_and_sufficient() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::RelayAdd { url: CLEARNET.to_string() })
            .await
            .expect("add");

        let refused = w
            .execute(Command::RelayConfirm {
                url: CLEARNET.to_string(),
                accept_clearnet: false,
            })
            .await;
        assert!(refused.is_err(), "no silent clearnet confirmation");
        let s = session(&w).await;
        assert!(!s.relays[0].confirmed, "the refusal changed nothing");
        assert!(
            !s.settings.clearnet_relays_enabled,
            "a refused confirmation must not enable anything"
        );

        w.execute(Command::RelayConfirm {
            url: CLEARNET.to_string(),
            accept_clearnet: true,
        })
        .await
        .expect("acknowledged");
        let s = wait_confirmed(&w, CLEARNET).await;
        assert!(s.relays[0].confirmed);
        assert_eq!(
            s.relays[0].blocked, None,
            "the acknowledged confirmation is enough — no second ritual"
        );
        assert!(
            s.settings.clearnet_relays_enabled,
            "…and it is stored in the settings, so it survives a restart"
        );
        assert!(s.clearnet_session, "the live flag follows the stored decision");

        // the OFF switch still exists — and switching off is remembered too,
        // so "go dark" is not undone by the next restart
        w.execute(Command::RelayClearnetSession { unlock: false })
            .await
            .expect("go dark");
        let s = session(&w).await;
        assert!(s.relays[0].confirmed, "going dark keeps the confirmation");
        assert_eq!(s.relays[0].blocked, Some(RelayBlock::ClearnetSessionLocked));
        assert!(
            !s.settings.clearnet_relays_enabled,
            "the OFF decision is persisted like the ON decision"
        );
    });
}

/// KEYSTONE — the stored decision is what a FRESH process starts from: a
/// node whose `config.toml` says clearnet is enabled dials its confirmed
/// clearnet relay immediately, with no further human action. This is the
/// whole point of the amendment — the previous design could not express
/// "yes, I mean it, stop asking".
#[test]
fn a_started_node_adopts_the_stored_clearnet_decision() {
    rt().block_on(async {
        let stored = SessionView {
            settings: molt_core::SessionSettings {
                relays: vec![molt_core::relay::RelayEntry {
                    url: CLEARNET.to_string(),
                    confirmed: true,
                }],
                clearnet_relays_enabled: true,
                ..molt_core::SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = spawn(GroupConfig::demo(), stored);
        let s = session(&w).await;
        assert!(
            s.clearnet_session,
            "the stored decision is live from the first moment"
        );
        assert_eq!(
            s.relays[0].blocked, None,
            "the confirmed clearnet relay is dialable without a fresh unlock"
        );
    });
}

/// KEYSTONE — §10.14 (decided 2026-07-31): a LOCAL relay (loopback, RFC1918,
/// `localhost`) is a legitimate self-host target, but it is reached outside
/// Tor — so it faces exactly the clearnet gate: an explicit acknowledgement
/// before it is ever dialed, never a silent connection. Since the ADR-0004
/// amendment that acknowledgement is durable (see the clearnet keystone) —
/// which matters most HERE, because a self-hosted LAN relay is the setup an
/// operator restarts constantly while getting it working.
#[test]
fn a_local_relay_needs_the_same_acknowledgement_as_clearnet() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        const LOCAL: &str = "ws://192.168.1.5:7777";
        w.execute(Command::RelayAdd { url: LOCAL.to_string() })
            .await
            .expect("a LAN self-host relay may enter the pool");
        let s = session(&w).await;
        assert_eq!(s.relays[0].kind, RelayKind::Local);
        assert_eq!(s.relays[0].blocked, Some(RelayBlock::Unconfirmed));

        let refused = w
            .execute(Command::RelayConfirm {
                url: LOCAL.to_string(),
                accept_clearnet: false,
            })
            .await;
        assert!(refused.is_err(), "no silent confirmation of a non-Tor relay");

        w.execute(Command::RelayConfirm {
            url: LOCAL.to_string(),
            accept_clearnet: true,
        })
        .await
        .expect("acknowledged");
        let s = wait_confirmed(&w, LOCAL).await;
        assert_eq!(
            s.relays[0].blocked, None,
            "the acknowledged confirmation is enough, and it is remembered"
        );
        assert!(
            s.settings.clearnet_relays_enabled,
            "the decision is persisted — a LAN self-host node is not re-asked \
             after every restart"
        );
    });
}

/// An onion relay needs no acknowledgement and is dialable as soon as it is
/// confirmed — that is the whole point of preferring onion.
#[test]
fn a_confirmed_onion_relay_is_dialable_without_any_session_unlock() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::RelayAdd { url: ONION.to_string() })
            .await
            .expect("add");
        w.execute(Command::RelayConfirm {
            url: ONION.to_string(),
            accept_clearnet: false,
        })
        .await
        .expect("onion needs no clearnet ack");
        let s = session(&w).await;
        assert!(s.relays[0].confirmed);
        assert_eq!(s.relays[0].blocked, None);
        assert!(!s.clearnet_session, "and no clearnet was unlocked on the way");
    });
}

/// Priority is the pool order, editable one step at a time; revoking takes a
/// relay out of use without forgetting it.
#[test]
fn priority_moves_and_revocation_work_through_the_surface() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        for url in [ONION, CLEARNET] {
            w.execute(Command::RelayAdd { url: url.to_string() })
                .await
                .expect("add");
        }
        let s = session(&w).await;
        assert_eq!(s.relays[0].url, ONION, "added in order");

        w.execute(Command::RelayMove { url: CLEARNET.to_string(), up: true })
            .await
            .expect("move up");
        let s = session(&w).await;
        assert_eq!(s.relays[0].url, CLEARNET, "clearnet is now first priority");
        assert_eq!(s.relays[1].url, ONION);
        // moving past the edge is a no-op, not an error
        w.execute(Command::RelayMove { url: CLEARNET.to_string(), up: true })
            .await
            .expect("edge move");
        assert_eq!(session(&w).await.relays[0].url, CLEARNET);

        w.execute(Command::RelayConfirm { url: ONION.to_string(), accept_clearnet: false })
            .await
            .expect("confirm");
        w.execute(Command::RelayRevoke { url: ONION.to_string() })
            .await
            .expect("revoke");
        let s = session(&w).await;
        assert!(!s.relays[1].confirmed, "revoked, but still in the pool");
        assert_eq!(s.relays[1].blocked, Some(RelayBlock::Unconfirmed));

        w.execute(Command::RelayRemove { url: ONION.to_string() })
            .await
            .expect("remove");
        let s = session(&w).await;
        assert_eq!(s.relays.len(), 1);
        assert_eq!(s.relays[0].url, CLEARNET);
    });
}

/// KEYSTONE — the relay pool has ONE way in: the `Relay*` commands, with
/// their URL validation and their clearnet gate. `save_settings` replaces the
/// settings wholesale, so it must neither inject a relay (a pre-confirmed
/// clearnet entry would walk straight past the acknowledgement) nor wipe the
/// pool as a side effect of changing an unrelated field.
#[test]
fn save_settings_can_neither_inject_nor_wipe_relays() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::RelayAdd { url: ONION.to_string() })
            .await
            .expect("add");
        w.execute(Command::RelayConfirm { url: ONION.to_string(), accept_clearnet: false })
            .await
            .expect("confirm");

        // a settings payload that tries to smuggle in a confirmed clearnet
        // relay AND drop the real one, while changing something innocuous
        let mut settings = session(&w).await.settings;
        settings.mcp_port = 4141;
        settings.relays = vec![molt_core::relay::RelayEntry {
            url: "wss://attacker.example.org".to_string(),
            confirmed: true,
        }];
        w.execute(Command::SaveSettings { settings }).await.expect("save");

        let s = session(&w).await;
        assert_eq!(s.settings.mcp_port, 4141, "the honest field was saved");
        assert_eq!(s.relays.len(), 1, "no relay was injected");
        assert_eq!(s.relays[0].url, ONION, "and the real pool survived");
        assert!(s.relays[0].confirmed);
    });
}

/// Malformed and unsafe URLs never enter the pool, and one relay cannot be
/// added twice under two spellings.
#[test]
fn bad_urls_are_refused_and_duplicates_collapse() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        for bad in [
            "https://relay.example.org",  // wrong scheme
            "relay.example.org",          // no scheme
            "wss://",                     // no host
            "ws://relay.example.org",     // plaintext clearnet
            "wss://relay example.org",    // junk
        ] {
            assert!(
                w.execute(Command::RelayAdd { url: bad.to_string() }).await.is_err(),
                "must refuse {bad:?}"
            );
        }
        assert!(session(&w).await.relays.is_empty(), "nothing entered the pool");

        w.execute(Command::RelayAdd { url: CLEARNET.to_string() })
            .await
            .expect("add");
        assert!(
            w.execute(Command::RelayAdd { url: "WSS://Relay.Example.ORG/".to_string() })
                .await
                .is_err(),
            "the same relay in another spelling is a duplicate"
        );
        assert_eq!(session(&w).await.relays.len(), 1);
    });
}

/// KEYSTONE (2026-08-16) — a founding must not overtake a relay confirmation
/// still being verified: the confirm lands async on the probe's verdict, and
/// a `create_start` issued in the same breath minted invites from a pool
/// MISSING the relay the operator had just consented to (observed in the
/// 5-node dev test: the "invites went stale" note fired right after the
/// ritual opened). Fail closed: found only once the pool has settled.
#[test]
fn founding_refuses_while_a_confirmation_is_still_verifying() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        // a private, unroutable target: ws:// is allowed for it, and its
        // probe cannot come back fast enough to close the race window
        let slow = "ws://10.255.255.1:9";
        w.execute(Command::RelayAdd { url: slow.to_string() }).await.expect("add");
        w.execute(Command::RelayConfirm { url: slow.to_string(), accept_clearnet: true })
            .await
            .expect("confirm accepted");
        let refused = w
            .execute(Command::CreateStart {
                name: "r".to_string(),
                member: "m".to_string(),
                threshold: 2,
                members: 2,
                relays: Vec::new(),
            })
            .await;
        let err = format!("{:?}", refused.expect_err("the race must be refused"));
        assert!(
            err.contains("still verifying"),
            "the refusal names the pending confirmation: {err}"
        );

        // a STANDALONE probe's verdict for the same URL (confirm = false)
        // must NOT open the gate — only the confirm probe's own verdict
        // settles the pending confirmation (review 2026-08-16). The url is
        // read back from the pool so it matches the normalized pending key.
        let stored = session(&w).await.settings.relays[0].url.clone();
        w.execute(Command::NetRelayProbed {
            url: stored,
            error: String::new(),
            unreachable: false,
            confirm: false,
        })
        .await
        .expect("standalone verdict lands");
        let still = w
            .execute(Command::CreateStart {
                name: "r".to_string(),
                member: "m".to_string(),
                threshold: 2,
                members: 2,
                relays: Vec::new(),
            })
            .await;
        let err = format!("{:?}", still.expect_err("the gate must stay closed"));
        assert!(
            err.contains("still verifying"),
            "a standalone verdict must not clear the confirm gate: {err}"
        );
    });
}

/// …and the gate CLEARS with the verdict: once the probe came back the same
/// founding passes it (it may still fail for transport reasons — just never
/// with the pending-confirmation refusal).
#[test]
fn founding_passes_once_the_confirmation_verdict_landed() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::RelayAdd { url: ONION.to_string() }).await.expect("add");
        w.execute(Command::RelayConfirm { url: ONION.to_string(), accept_clearnet: false })
            .await
            .expect("confirm accepted");
        wait_confirmed(&w, ONION).await;
        let again = w
            .execute(Command::CreateStart {
                name: "r".to_string(),
                member: "m".to_string(),
                threshold: 2,
                members: 2,
                relays: Vec::new(),
            })
            .await;
        if let Err(e) = again {
            let msg = format!("{e:?}");
            assert!(
                !msg.contains("still verifying"),
                "the gate must clear with the verdict: {msg}"
            );
        }
    });
}

/// The joiner's twin: `adopt relays` confirms async too, and a join in the
/// same breath would race the verdict exactly like the founding.
#[test]
fn joining_refuses_while_a_confirmation_is_still_verifying() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let slow = "ws://10.255.255.1:9";
        w.execute(Command::RelayAdd { url: slow.to_string() }).await.expect("add");
        w.execute(Command::RelayConfirm { url: slow.to_string(), accept_clearnet: true })
            .await
            .expect("confirm accepted");
        let refused = w
            .execute(Command::JoinStart {
                invite: "molt://invite/x".to_string(),
                member: "m".to_string(),
            })
            .await;
        let err = format!("{:?}", refused.expect_err("the race must be refused"));
        assert!(
            err.contains("still verifying"),
            "the refusal names the pending confirmation: {err}"
        );
    });
}
