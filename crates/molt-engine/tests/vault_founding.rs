// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Vault founding** (vault build plan S2): a 2-of-4 founding with the
//! vault seals a roster-v6 genesis whose every seat carries its vault key,
//! and every seat persists the seed that derives it; without the vault the
//! founding stays v5 byte for byte.

use molt_core::WorkspaceEvent;
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{close_and_open, found_n_at};

/// The genesis as one seat persisted it: `(identities, canonical bytes)`.
fn genesis_of(ws: &molt_storage::OpenedWorkspace) -> (Vec<molt_core::MemberIdentity>, Vec<u8>) {
    let log = ws.read_log_from(1).expect("log");
    let WorkspaceEvent::Founded {
        rule_m,
        rule_n,
        identities,
        republic_id,
        agenda,
        relays,
        features,
        ..
    } = &log[0].body
    else {
        panic!("first event is not Founded");
    };
    let bytes = molt_core::roster_canonical_bytes(
        republic_id,
        *rule_m,
        *rule_n,
        identities,
        agenda,
        relays,
        features.as_deref(),
    );
    (identities.clone(), bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vault_founding_round_trips_roster_v6() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    let opened = close_and_open(tmp.path(), &all).await;

    let (founding, bytes) = genesis_of(&opened[0].1);
    assert!(bytes.starts_with(b"molt-roster-v6\0"), "the vault founding is roster-v6");
    assert_eq!(founding.len(), 4);
    for (name, ws) in &opened {
        let (ids, b) = genesis_of(ws);
        assert_eq!(ids, founding, "{name}: one founding table");
        assert_eq!(b, bytes, "{name}: one signed table");
        let (_, chain) = ws.read_chain().expect("chain");
        molt_engine::verify_chain(&chain).expect("the v6 genesis verifies");

        let seat = ids.iter().find(|i| &i.member == name).expect("own seat");
        assert!(!seat.vault_pk.is_empty(), "{name} is keyed");
        let seed = ws.read_transport_state().vault_seed.expect("the vault seed is persisted");
        let seed: [u8; 32] = seed.0.as_slice().try_into().expect("32 bytes");
        assert_eq!(molt_vault::vault_keypair(&seed).1, seat.vault_pk, "{name}: the seed derives the roster key");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vaultless_founding_stays_byte_identical() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["memory"]).await;
    let opened = close_and_open(tmp.path(), &all).await;

    for (name, ws) in &opened {
        let (ids, bytes) = genesis_of(ws);
        assert!(bytes.starts_with(b"molt-roster-v5\0"), "{name}: still v5");
        assert!(ids.iter().all(|i| i.vault_pk.is_empty()), "{name}: no vault key in any identity");
        assert!(ws.read_transport_state().vault_seed.is_none(), "{name}: no vault seed");
    }
}
