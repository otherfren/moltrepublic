// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The relay pool through the REAL command surface — the same path an MCP
//! agent drives (`docs/transport/relay_pool.md`). These pin the promises a
//! user relies on: nothing is pre-trusted, adding never connects, and a
//! clearnet relay needs an explicit acknowledgement plus a per-session
//! activation before a single packet leaves.

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
/// explicit acknowledgement is REFUSED (the same refusal an MCP agent hits),
/// and even once confirmed it stays blocked until the session is unlocked.
#[test]
fn clearnet_needs_the_acknowledgement_and_then_a_session_unlock() {
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

        w.execute(Command::RelayConfirm {
            url: CLEARNET.to_string(),
            accept_clearnet: true,
        })
        .await
        .expect("acknowledged");
        let s = session(&w).await;
        assert!(s.relays[0].confirmed);
        assert_eq!(
            s.relays[0].blocked,
            Some(RelayBlock::ClearnetSessionLocked),
            "confirmed clearnet is still not dialed automatically"
        );
        assert!(!s.clearnet_session, "a session never starts unlocked");

        w.execute(Command::RelayClearnetSession { unlock: true })
            .await
            .expect("unlock");
        let s = session(&w).await;
        assert!(s.clearnet_session);
        assert_eq!(s.relays[0].blocked, None, "now it may be dialed");

        // and it can be locked again without touching the confirmation
        w.execute(Command::RelayClearnetSession { unlock: false })
            .await
            .expect("relock");
        let s = session(&w).await;
        assert!(s.relays[0].confirmed);
        assert_eq!(s.relays[0].blocked, Some(RelayBlock::ClearnetSessionLocked));
    });
}

/// KEYSTONE — §10.14 (decided 2026-07-31): a LOCAL relay (loopback, RFC1918,
/// `localhost`) is a legitimate self-host target, but it is reached outside
/// Tor — so it faces exactly the clearnet gate: explicit acknowledgement to
/// confirm, per-session activation to dial, never a silent connection.
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
        let s = session(&w).await;
        assert_eq!(
            s.relays[0].blocked,
            Some(RelayBlock::ClearnetSessionLocked),
            "confirmed, but still waiting for the session activation"
        );

        w.execute(Command::RelayClearnetSession { unlock: true })
            .await
            .expect("unlock");
        let s = session(&w).await;
        assert_eq!(s.relays[0].blocked, None, "now it may be dialed");
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
