// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs, dead_code)]

//! Shared helpers of the engine integration tests.

use std::time::Duration;

use molt_core::{ChatMessage, Command, EventEnvelope, Reply, Surface, WorkspaceEvent};
use molt_engine::WalletHandle;

/// A deterministic non-nil message id for hand-built test envelopes (the
/// engine mints real random ids; this stands in for a peer's minting).
pub fn test_msg_id(seq: u64) -> molt_core::MessageId {
    let mut b = [0xa5u8; 16];
    b[..8].copy_from_slice(&seq.to_le_bytes());
    molt_core::MessageId(b)
}

/// One hand-built peer chat envelope carrying `test_msg_id(seq)` — what a
/// sender's outbox would hold for a plain text message. Stamped "now" so
/// the messages sit inside the chat-retention read window.
pub fn chat_env(seq: u64, from: &str, body: &str) -> EventEnvelope {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + seq;
    EventEnvelope {
        seq,
        ts,
        by: from.to_string(),
        body: WorkspaceEvent::Chat(ChatMessage::text(test_msg_id(seq), from, body, ts)),
    }
}

/// Read a surface's applied log.
pub async fn read_applied(w: &WalletHandle, surface: Surface) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface,
            channel: None,
        })
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
            channel: None,
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
