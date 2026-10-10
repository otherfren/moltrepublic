// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Wallet plan §14 step 5: the purse's contract before its runs exist -
//! the view's phase from the founding rule, and the doors that refuse.
//! The init vote (step 6) lives in `wallet_init.rs`.

use molt_core::wallet::{ShareStatus, WalletPhase, WalletRefusal};
use molt_core::{Command, GroupConfig, MoltError, Reply, SessionSettings, SessionView, Surface};

async fn session(w: &molt_engine::WalletHandle) -> SessionView {
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    *sv
}

/// A republic of `members` seats at `threshold`, founded over the sim seam.
async fn founded(tmp: &std::path::Path, threshold: u8, members: u8) -> molt_engine::WalletHandle {
    let sv = SessionView {
        settings: SessionSettings {
            workspace_dir: tmp.join("workspaces").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(GroupConfig::demo(), sv, true);
    w.execute(Command::CreateStart {
        name: "Purse Republic".to_string(),
        member: "petra".to_string(),
        threshold,
        members,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let seed = session(&w).await.create.seed.clone();
    w.execute(Command::ConfirmSeedBackup { phrase: seed }).await.expect("backup confirm");
    for _ in 0..600 {
        let s = session(&w).await;
        match s.create.run.outcome {
            1 => break,
            2 => panic!("founding failed: {:?}", s.create.run.log),
            _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    w.execute(Command::CreateFinish).await.expect("create finish");
    w
}

async fn wallet_view(w: &molt_engine::WalletHandle) -> molt_core::wallet::WalletView {
    match w.execute(Command::ReadState { surface: Surface::Wallet, channel: None, view: None }).await {
        Ok(Reply::State(snap)) => *snap.wallet.expect("the wallet surface carries its view"),
        other => panic!("unexpected: {other:?}"),
    }
}

/// W1 from the genesis rule: inside the bounds there is no purse yet,
/// outside them the phase says why; no chain, no rule.
#[tokio::test]
async fn the_phase_follows_the_founding_rule() {
    let plain = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    assert_eq!(wallet_view(&plain).await.phase, WalletPhase::Off);

    let tmp = tempfile::tempdir().expect("tmp");
    let w = founded(tmp.path(), 2, 3).await;
    let v = wallet_view(&w).await;
    assert_eq!(v.phase, WalletPhase::NoPurse);
    assert_eq!((v.threshold, v.participants), (2, 3));
    assert_eq!(v.network, "mainnet");
    assert!(v.address.is_empty() && !v.can_spend);

    let tmp = tempfile::tempdir().expect("tmp");
    let w = founded(tmp.path(), 3, 3).await;
    assert_eq!(wallet_view(&w).await.phase, WalletPhase::Bounds, "m = n has no purse");
}

/// The doors step 7 builds refuse with one line until then; the init
/// needs a chain, and a probe nobody started changes nothing.
#[tokio::test]
async fn the_unbuilt_doors_refuse_compactly() {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    match w.execute(Command::WalletInit).await {
        Err(e @ MoltError::Wallet(WalletRefusal::NotChain)) => {
            assert_eq!(e.to_string(), "purse: not a chain republic");
        }
        other => panic!("unexpected: {other:?}"),
    }
    let probe = Command::NetWalletProbe { height: Some(1), error: String::new(), generation: None };
    assert!(matches!(w.execute(probe).await, Ok(Reply::Ack)));
    let peer = || "bjorn".to_string();
    let secret = || molt_core::vault::SecretBytes(vec![7; 32]);
    match w.execute(Command::WalletConsent { accept: true }).await {
        Err(e @ MoltError::Wallet(WalletRefusal::NoRun)) => assert_eq!(e.to_string(), "purse: no set-up running"),
        other => panic!("unexpected: {other:?}"),
    }
    match w.execute(Command::WalletRetry).await {
        Err(e @ MoltError::Wallet(WalletRefusal::NotChain)) => assert_eq!(e.to_string(), "purse: not a chain republic"),
        other => panic!("unexpected: {other:?}"),
    }
    let junk = Command::NetWalletFrame { from: peer(), body: secret(), generation: None };
    assert!(matches!(w.execute(junk).await, Ok(Reply::Ack)), "an unusable frame is dropped");
    for cmd in [
        Command::NetWalletScan {
            scan_height: 1,
            daemon_height: 2,
            paused: None,
            error: String::new(),
            generation: None,
        },
        Command::NetWalletStatus { from: peer(), status: ShareStatus::Held, generation: None },
        Command::NetWalletViewAnswer { from: peer(), view: secret(), generation: None },
    ] {
        match w.execute(cmd).await {
            Err(e @ MoltError::Wallet(WalletRefusal::NotYet)) => {
                assert_eq!(e.to_string(), "purse: not available yet");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
