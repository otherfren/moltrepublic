// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **F3 keystone — file sharing over relays**
//! (`docs_archive/transport/file_transfer_nostr.md`): the share is metadata-only,
//! the bytes publish LAZILY on the first download request (kind-447 chunk
//! series), and the downloader ends with the verified file on disk — over
//! a real 2-of-2 republic on an in-process relay.

mod common;
mod file_support;

use file_support::found_pair;
use std::time::Duration;

use molt_core::{Command, Reply};
use nostr_relay_builder::MockRelay;

/// The whole story: petra shares (metadata only — nothing on the relay
/// yet), walter downloads; the request triggers petra's lazy chunk-series
/// publish, walter fetches, verifies and lands the file in his download
/// dir with the shared bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_file_downloads_over_the_relay() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("workspaces");
    let (a, b) = found_pair(&root, &url).await;

    // petra's file: 100 KiB pattern
    let bytes: Vec<u8> = (0..100 * 1024)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    let src = tmp.path().join("bericht.bin");
    std::fs::write(&src, &bytes).expect("write source");

    a.execute(Command::ShareFile {
        path: src.display().to_string(),
        channel: molt_core::ChannelRef::Group,
    })
    .await
    .expect("the share is admitted over relays");

    // the share message reaches walter's chat (metadata only)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let share_id = loop {
        if let Reply::State(snap) = b
            .execute(Command::ReadState {
                surface: molt_core::Surface::Chat,
                channel: None,
                view: None,
            })
            .await
            .expect("read chat")
        {
            let found = snap.applied.iter().find_map(|v| {
                let msg = serde_json::from_value::<molt_core::ChatMessage>(v.clone()).ok()?;
                msg.file.is_some().then_some(msg.id)
            });
            if let Some(id) = found {
                break id;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the share message never reached the member"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // walter downloads — lazy publish + fetch under the hood
    b.execute(Command::DownloadFile { id: share_id, dest: None })
        .await
        .expect("download starts");

    let dl_dir = root.join("dl-b");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let landed = std::fs::read_dir(&dl_dir)
            .ok()
            .and_then(|mut it| {
                it.find_map(|e| {
                    let p = e.ok()?.path();
                    (p.file_name()?.to_string_lossy() == "bericht.bin").then_some(p)
                })
            })
            .and_then(|p| std::fs::read(&p).ok());
        if let Some(got) = landed {
            assert_eq!(got, bytes, "the downloaded bytes are the shared bytes");
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            let phase = match b.execute(Command::ReadUploads).await.expect("uploads") {
                Reply::Uploads { uploads } => format!("{uploads:?}"),
                other => format!("{other:?}"),
            };
            panic!("the download never landed; uploads: {phase}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
