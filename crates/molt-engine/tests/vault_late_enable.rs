// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The vault, prepared at every founding and enabled by vote**
//! (`docs/vault/vault_late_enable.md`, E2-E5): a founding without the
//! vault is still roster-v6 with every seed persisted but shows no vault;
//! a `set_features` vote switches it on, across a cut and a recovery.

use molt_core::vault::{SecretText, VaultEnable};
use molt_core::{Command, MoltError, Reply, WorkspaceEvent};
use molt_engine::WalletHandle;
use nostr_relay_builder::MockRelay;

mod vault_support;
use vault_support::{
    close_and_open, cut_all, deposited, enable_vault, engine, found_n_at, granted, read_session, read_text,
    vault_view, wait_for, wait_vault,
};

async fn vault_enable(w: &WalletHandle) -> VaultEnable {
    match w.execute(Command::Status).await.expect("status") {
        Reply::Status(s) => s.vault_enable,
        other => panic!("unexpected: {other:?}"),
    }
}

/// E5: every vault command answers `vault: not enabled`.
async fn assert_hidden(w: &WalletHandle) {
    assert!(vault_view(w).await.is_none(), "read_state has no vault section");
    let r = w
        .execute(Command::VaultSeal { name: "n".into(), kind: "text".into(), text: SecretText("one".into()) })
        .await;
    match r {
        Err(e @ MoltError::FeatureDisabled("vault")) => assert_eq!(e.to_string(), "vault: not enabled"),
        other => panic!("expected `vault: not enabled`, got {other:?}"),
    }
}

/// E2 + E5: a founding without the vault is prepared - roster-v6, every
/// seat keyed, every seed persisted and deriving its key - and invisible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_founding_without_the_vault_is_prepared_but_hidden() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["memory"]).await;
    for w in &all {
        assert_hidden(w).await;
        assert_eq!(vault_enable(w).await, VaultEnable::Offer);
    }
    for (name, ws) in close_and_open(tmp.path(), &all).await {
        let log = ws.read_log_from(1).expect("log");
        let WorkspaceEvent::Founded { rule_m, rule_n, identities, republic_id, agenda, relays, features, .. } =
            &log[0].body
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
        assert!(bytes.starts_with(b"molt-roster-v6\0"), "{name}: prepared means roster-v6");
        assert_eq!(features.as_deref(), Some(&["memory".to_string()][..]));
        let (_, chain) = ws.read_chain().expect("chain");
        molt_engine::verify_chain(&chain).expect("the prepared genesis verifies");
        let seat = identities.iter().find(|i| i.member == name).expect("own seat");
        let seed = ws.read_transport_state().vault_seed.expect("the vault seed is persisted");
        let seed: [u8; 32] = seed.0.as_slice().try_into().expect("32 bytes");
        assert_eq!(molt_vault::vault_keypair(&seed).1, seat.vault_pk, "{name}: the seed derives the roster key");
    }
}

/// E5: a cut before enabling folds an empty vault base; the vote then
/// switches the vault on, and seal, grant and read work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cut_then_enable_then_seal_grant_read() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["memory"]).await;
    cut_all(&all).await;
    assert_hidden(&all[2]).await;

    enable_vault(&all).await;
    assert_eq!(vault_enable(&all[3]).await, VaultEnable::On);
    let sid = deposited(&all, "quiet-lantern-17").await;
    granted(&all[1], &all[3], &all[2], &sid, "c").await;
    assert_eq!(read_text(&all[2], &sid).await, "quiet-lantern-17");
}

/// E5: enable, deposit, cut; `d` loses its device and recovers with its
/// phrase; a grant to it reads - the enabled vault survives the cut and
/// the recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enable_then_cut_then_recover_and_read() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let mut all = found_n_at(tmp.path(), &url, 4, 2, &["memory"]).await;
    enable_vault(&all).await;
    let sid = deposited(&all, "amber-key-4").await;
    cut_all(&all).await;

    let id = read_session(&all[3]).await.active_workspace.clone();
    let phrase = match all[3].execute(Command::RevealSeed { id }).await.expect("reveal") {
        Reply::Seed { seed } => seed,
        other => panic!("unexpected: {other:?}"),
    };
    let d = all.pop().expect("d");
    d.execute(Command::CloseWorkspace).await.expect("d goes away");
    drop(d);
    all[0].execute(Command::RecoverInviteStart { member: "d".to_string() }).await.expect("mint");
    let s = wait_for(&all[0], "the recovery link", |s| {
        s.notice.starts_with("recovery-link:") || s.notice.starts_with("recovery-link-failed:")
    })
    .await;
    let link = s
        .notice
        .strip_prefix("recovery-link:")
        .unwrap_or_else(|| panic!("the mint must succeed: {:?}", s.notice))
        .to_string();
    let d = engine(&tmp.path().join("d-new"));
    vault_support::adopt_relay(&d, &url).await;
    d.execute(Command::RecoverStart { link, phrase }).await.expect("recover start");
    let s = wait_for(&d, "the recovery to open", |s| {
        (s.screen == molt_core::Screen::Main && s.notice.starts_with("recovered:"))
            || s.notice.starts_with("recover-failed:")
    })
    .await;
    assert!(!s.notice.starts_with("recover-failed:"), "recovery: {:?}", s.notice);
    wait_vault(&d, "the base and the payload", |v| {
        v.real && v.base_pending.is_none() && v.deposits.iter().any(|x| x.secret_id == sid && x.held)
    })
    .await;

    granted(&all[1], &all[2], &d, &sid, "d").await;
    assert_eq!(read_text(&d, &sid).await, "amber-key-4");
}
