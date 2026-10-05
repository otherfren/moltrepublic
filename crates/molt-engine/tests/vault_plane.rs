// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The vault payload plane** (vault build plan S3a, spec §9.2): a
//! payload one seat holds reaches every other seat over the file plane,
//! with the mirror switched off everywhere - holding is mandatory, not
//! mirror consent.

use std::time::Duration;

use molt_core::Command;
use nostr_relay_builder::MockRelay;
use sha2::Digest as _;

mod vault_support;
use vault_support::{close_and_open, found_n_at, read_session, NAMES};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_payload_reaches_every_seat_without_mirror_consent() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let all = found_n_at(tmp.path(), &url, 4, 2, &["vault"]).await;
    for w in &all {
        w.execute(Command::SetMirror { on: false, quota_bytes: 0 }).await.expect("mirror off");
    }

    let payload: Vec<u8> = (0..3_000u32).map(|i| u8::try_from(i % 241).unwrap_or(0)).collect();
    let hash = hex::encode(sha2::Sha256::digest(&payload));
    let size = u64::try_from(payload.len()).expect("size");
    let secret_id = "ab".repeat(32);
    // the depositor holds and publishes it; every other seat is told a
    // deposit names it, as a pending or committed deposit would
    all[0].__vault_name_payload(&secret_id, &hash, size, Some(payload.clone()));
    for w in &all[1..] {
        w.__vault_name_payload(&secret_id, &hash, size, None);
    }

    let mut dirs = Vec::new();
    for (i, w) in all.iter().enumerate() {
        let id = read_session(w).await.active_workspace.clone();
        dirs.push(molt_storage::find_workspace_dir(&tmp.path().join(NAMES[i]), &id).expect("dir"));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let held: Vec<bool> = dirs
            .iter()
            .map(|d| molt_storage::list_vault_payloads(d).contains(&secret_id))
            .collect();
        if held.iter().all(|h| *h) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "not every seat holds the payload: {held:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    for (name, ws) in close_and_open(tmp.path(), &all).await {
        assert_eq!(
            ws.read_vault_payload(&secret_id).expect("readable").as_deref(),
            Some(payload.as_slice()),
            "{name} holds the exact bytes"
        );
    }
}
