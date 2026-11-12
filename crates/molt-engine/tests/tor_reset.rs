// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! `Command::NetResetTor` refuses wherever the embedded client is not the
//! configured route - and those are the only cases driven here: the reset
//! itself removes this user's Tor state, so it is tested against a tempdir
//! in `molt_net::tor_embedded`.

use molt_core::{Command, GroupConfig, MoltError, Reply, SessionView};

fn engine() -> molt_engine::WalletHandle {
    molt_engine::spawn(GroupConfig::demo(), SessionView::default())
}

async fn refusal(w: &molt_engine::WalletHandle) -> String {
    match w.execute(Command::NetResetTor).await {
        Err(MoltError::Settings(why)) => why,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn without_tor_there_is_nothing_to_reset() {
    assert_eq!(refusal(&engine()).await, "embedded Tor not in use");
}

#[tokio::test]
async fn a_socks_proxy_is_not_the_embedded_client() {
    let w = engine();
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    let mut settings = sv.settings.clone();
    settings.anonymity = "tor".to_string();
    settings.tor_mode = "local".to_string();
    w.execute(Command::SetNodePosture { posture: molt_core::NodePosture::of(&settings) })
        .await
        .expect("posture");
    w.execute(Command::SaveSettings { settings }).await.expect("saved");
    assert_eq!(refusal(&w).await, "embedded Tor not in use");
}
