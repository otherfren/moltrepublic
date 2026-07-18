// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The settings backup table's refresh drives
//! [`molt_core::Command::NetListBackups`]: the engine builds the S3 config
//! from the SAVED settings, runs a SigV4-signed ListObjectsV2 under `molt/`
//! **off the actor** through the resolved dialer, and feeds the outcome
//! back as `NetListBackupsResult` — real orphans land in
//! `session.backup_orphans`, the honest status in `session.s3_list`. The
//! GUI refresh and the `net_list_backups` MCP tool are thin wrappers over
//! this.
//!
//! No real network: the listing runs against an in-process HTTP stub on
//! 127.0.0.1 (the `s3_test_button.rs` posture).

use molt_core::{Command, GroupConfig, Reply, SessionView};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A single-shot S3 stub answering `status` with `body`.
async fn stub_server(status: u16, body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut chunk).await.expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let resp = format!(
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.expect("write response");
        sock.shutdown().await.ok();
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// Spawn an engine, persist S3 settings pointing at `endpoint`, issue
/// NetListBackups and poll until `session.s3_list` settles.
async fn run_listing(endpoint: &str) -> SessionView {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    let mut settings = sv.settings.clone();
    settings.s3_endpoint = endpoint.to_string();
    settings.s3_access_key = "AKIAEXAMPLE".to_string();
    settings.s3_secret_key = "secret-example".to_string();
    settings.s3_bucket = "molt-bucket".to_string();
    w.execute(Command::SaveSettings { settings }).await.expect("settings saved");
    w.execute(Command::NetListBackups).await.expect("NetListBackups accepted");
    for _ in 0..150 {
        let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
            panic!("read session failed");
        };
        if !sv.s3_list.is_empty() && sv.s3_list != "listing" {
            return *sv;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("s3_list never settled");
}

fn contents(key: &str, size: u64) -> String {
    format!(
        "<Contents><Key>{key}</Key><LastModified>2013-05-24T00:00:00Z</LastModified>\
         <Size>{size}</Size></Contents>"
    )
}

#[tokio::test]
async fn a_real_listing_lands_real_orphans_in_the_session() {
    // one locally known workspace's backup, one true orphan prefix (two
    // generations), one foreign key
    let local = molt_core::demo_workspace_id("Family Office");
    let orphan = "ab".repeat(32);
    let body = format!(
        "<ListBucketResult><IsTruncated>false</IsTruncated>{}{}{}{}</ListBucketResult>",
        contents(&format!("molt/{local}/001700000000.molt.enc"), 4096),
        contents(&format!("molt/{orphan}/001700000000.molt.enc"), 1024),
        contents(&format!("molt/{orphan}/001700000600.molt.enc"), 1025),
        contents("molt/readme.txt", 512),
    );
    let endpoint = stub_server(200, body).await;
    let sv = run_listing(&endpoint).await;
    assert_eq!(sv.s3_list, "ok", "a parsed listing lands as ok");
    assert_eq!(sv.backup_orphans.len(), 2, "orphan + foreign: {:?}", sv.backup_orphans);
    let o = &sv.backup_orphans[0];
    assert_eq!(o.id, orphan, "the true orphan carries its workspace id");
    assert_eq!(o.name, "", "no display name is known for an orphan");
    assert_eq!(o.size_kib, 3, "generations aggregate");
    let f = &sv.backup_orphans[1];
    assert_eq!(f.id, "", "a foreign key has no workspace id");
    assert_eq!(f.name, "molt/readme.txt", "shown by its raw key");
}

#[tokio::test]
async fn a_403_reports_the_honest_credentials_class_and_no_orphans() {
    let endpoint = stub_server(403, String::new()).await;
    let sv = run_listing(&endpoint).await;
    assert!(
        sv.s3_list.starts_with("error:") && sv.s3_list.contains("403"),
        "403 → honest error, got {:?}",
        sv.s3_list
    );
    assert!(
        sv.s3_list.contains("access key"),
        "the 403 class names the credentials: {:?}",
        sv.s3_list
    );
    assert!(
        sv.backup_orphans.is_empty(),
        "a failed listing must never leave invented orphans"
    );
}

#[tokio::test]
async fn unconfigured_backup_target_fails_fast_with_an_honest_note() {
    // nothing configured: the validation fails in-actor, no dial happens
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    w.execute(Command::NetListBackups).await.expect("NetListBackups accepted");
    for _ in 0..150 {
        let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
            panic!("read session failed");
        };
        if !sv.s3_list.is_empty() && sv.s3_list != "listing" {
            assert!(
                sv.s3_list.starts_with("error:") && sv.s3_list.contains("endpoint"),
                "missing config → in-actor honest note, got {:?}",
                sv.s3_list
            );
            assert!(sv.backup_orphans.is_empty(), "empty table, never fake rows");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("s3_list never settled");
}
