// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **A relay without NIP-40 changes nothing**
//! (`docs_archive/chat/chat_threads_lifecycle.md` §8.2). The group's frames
//! and a temporary share's pieces carry an `expiration` tag; a relay that
//! does not implement NIP-40 stores and serves them like any other event,
//! expired or not, and this client must take them exactly as before - the
//! tag is a request to the relay, never a filter on our side.
//!
//! The suite's `MockRelay` DOES implement NIP-40 (nostr-database rejects an
//! expired event), so this file brings its own relay that ignores the tag.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use molt_net::dial::Dialer;
use molt_net::ritual_net::{GroupChannel, GroupRecv};
use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const SEED: [u8; 32] = [7u8; 32];

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// A relay with no NIP-40: every EVENT is stored and acknowledged, every
/// REQ is answered with everything stored, then EOSE. Tags are not read.
async fn relay_without_nip40() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let stored: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let stored = stored.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    let Message::Text(t) = msg else { continue };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    match v.get(0).and_then(|s| s.as_str()) {
                        Some("EVENT") => {
                            let event = v.get(1).cloned().unwrap_or_default();
                            let id = event.get("id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                            stored.lock().expect("lock").push(event);
                            let ok = serde_json::json!(["OK", id, true, ""]);
                            let _ = ws.send(Message::text(ok.to_string())).await;
                        }
                        Some("REQ") => {
                            let sub = v.get(1).and_then(|s| s.as_str()).unwrap_or("").to_string();
                            let events = stored.lock().expect("lock").clone();
                            for e in events {
                                let frame = serde_json::json!(["EVENT", sub, e]);
                                let _ = ws.send(Message::text(frame.to_string())).await;
                            }
                            let _ = ws.send(Message::text(serde_json::json!(["EOSE", sub]).to_string())).await;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    format!("ws://{addr}")
}

/// Both shapes reach the reader through a relay that ignores NIP-40: an
/// event whose deadline already passed (the relay never dropped it), and
/// one our running group published with its three-day deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_nip40_every_frame_still_arrives() {
    let url = relay_without_nip40().await;
    let dialer = Dialer::resolve("none", "local", 0).expect("direct dialer");
    let reader = GroupChannel::new(dialer.clone(), vec![url.clone()], SEED);

    // a frame whose deadline passed an hour ago - a NIP-40 relay would have
    // refused or dropped it, this one keeps serving it
    let t = now();
    let h = molt_net::envelope::h_tag(&SEED, t);
    let expired = EventBuilder::new(Kind::Custom(molt_net::kinds::KIND_GROUP), "expired-frame")
        .tag(Tag::parse(["h", h.as_str()]).expect("h"))
        .tag(Tag::expiration(Timestamp::from_secs(t - 3_600)))
        .custom_created_at(Timestamp::from_secs(t))
        .sign_with_keys(&Keys::generate())
        .expect("sign");
    let rt = molt_net::relay_runtime::RelayRuntime::new(dialer.clone(), vec![url.clone()]);
    let report = rt.publish(&expired).await.expect("the relay takes it");
    assert_eq!(report.accepted.len(), 1);

    // the running group's own publish: deadline three days on, accepted
    let running = GroupChannel::new(dialer, vec![url], SEED).with_group_expiry();
    let (_, report) = running.publish_frame(&[1u8; 32], b"hello").await.expect("published");
    assert_eq!(report.accepted.len(), 1, "a relay without NIP-40 accepts the tagged frame");

    let mut sub = reader.subscribe().await.expect("subscribes");
    let mut contents = Vec::new();
    for _ in 0..10 {
        match sub.recv(Duration::from_millis(500)).await {
            GroupRecv::Frame { content, .. } => contents.push(content),
            GroupRecv::Idle => {
                if contents.len() == 2 {
                    break;
                }
            }
            GroupRecv::Deaf(e) => panic!("deaf: {e}"),
        }
    }
    assert_eq!(contents.len(), 2, "both frames arrive: {contents:?}");
    assert!(contents.iter().any(|c| c == "expired-frame"), "an expired frame is not filtered here");
}
