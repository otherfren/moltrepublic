// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs, dead_code)]

//! Shared helpers of the engine integration tests.

use std::time::Duration;

use molt_core::{Command, Reply, Surface};
use molt_engine::WalletHandle;

/// Read a surface's applied log.
pub async fn read_applied(w: &WalletHandle, surface: Surface) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState { surface })
        .await
        .expect("read state")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Read the chat surface's applied log.
pub async fn read_chat(w: &WalletHandle) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface: Surface::Chat,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Poll until the chat log holds at least `want` messages (or panic after
/// `secs`).
pub async fn await_chat_len(w: &WalletHandle, want: usize, secs: u64) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let chat = read_chat(w).await;
        if chat.len() >= want {
            return chat;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {want} chat messages, have {}",
            chat.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
