// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Story 12 (design §8.3): the engine backup ticker is REAL and honest.
//! A republic founded via the offline sim seam gets its auto-backup pref
//! enabled — the stamp moves ONLY on a confirmed upload to an in-process
//! S3 stub (the `s3_list_backups.rs` posture), failures keep the stamp
//! untouched and surface verbatim, retention prunes exactly beyond
//! keep-copies, sealed-at-rest workspaces are skipped with an honest
//! status, and neither an unelapsed interval nor an in-flight upload
//! spawns a second task.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView, WorkspaceInfo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One recorded request.
#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// `(status, body, delay_ms)` a route answers with.
type RouteResp = (u16, String, u64);
type Router = Arc<dyn Fn(&str, &str) -> RouteResp + Send + Sync>;

/// A multi-request S3 stub: accepts connections forever, records every
/// request (method, path, body), answers per the router.
async fn stub_server(router: Router) -> (String, Arc<Mutex<Vec<Req>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let log: Arc<Mutex<Vec<Req>>> = Arc::new(Mutex::new(Vec::new()));
    let log_task = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let router = router.clone();
            let log = log_task.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head_end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|i| i + 4)
                    .expect("head end");
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or_default().to_string();
                let mut parts = request_line.split(' ');
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let content_length: usize = lines
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = buf[head_end..].to_vec();
                while body.len() < content_length {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let (status, resp_body, delay_ms) = router(&method, &path);
                log.lock().expect("log lock").push(Req { method, path, body });
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), log)
}

/// An empty (complete) ListObjectsV2 body.
fn empty_listing() -> String {
    "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".to_string()
}

fn listing_of(keys: &[String]) -> String {
    let contents: String = keys
        .iter()
        .map(|k| {
            format!(
                "<Contents><Key>{k}</Key><LastModified>2013-05-24T00:00:00Z</LastModified>\
                 <Size>1024</Size></Contents>"
            )
        })
        .collect();
    format!("<ListBucketResult><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>")
}

async fn session(w: &molt_engine::WalletHandle) -> SessionView {
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    *sv
}

/// Spawn a persisted engine over the sim founding seam, found a republic,
/// and return `(handle, workspace id, root)`.
async fn founded_engine(
    tmp: &std::path::Path,
    endpoint: &str,
    keep_copies: u16,
) -> (molt_engine::WalletHandle, String, std::path::PathBuf) {
    let root = tmp.join("workspaces");
    let sv = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            // the GLOBAL auto-backup master switch: on for the harness —
            // the per-workspace pref only takes effect underneath it
            s3_backup: true,
            s3_endpoint: endpoint.to_string(),
            s3_access_key: "AKIAEXAMPLE".to_string(),
            s3_secret_key: "secret-example".to_string(),
            s3_bucket: "molt-bucket".to_string(),
            s3_keep_copies: keep_copies,
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(GroupConfig::demo(), sv, true);
    w.execute(Command::CreateStart {
        name: "Backup Republic".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    for _ in 0..600 {
        let s = session(&w).await;
        match s.create.run.outcome {
            1 => break,
            2 => panic!("founding failed: {:?}", s.create.run.log),
            _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    w.execute(Command::CreateFinish).await.expect("create finish");
    let id = session(&w).await.active_workspace.clone();
    assert!(!id.is_empty(), "founding produced a workspace");
    (w, id, root)
}

fn entry(sv: &SessionView, id: &str) -> WorkspaceInfo {
    sv.workspaces
        .iter()
        .find(|w| w.id == id)
        .cloned()
        .expect("entry exists")
}

/// Poll until the predicate holds on the session.
async fn poll_session(
    w: &molt_engine::WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> SessionView {
    for _ in 0..300 {
        let sv = session(w).await;
        if pred(&sv) {
            return sv;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    panic!("{what} did not happen in time");
}

/// The settings' GLOBAL "Automatic S3 backup" checkbox is a MASTER gate:
/// with it off, the ticker spawns nothing — even for a workspace whose
/// per-workspace pref is on. Unchecking the box must actually stop the
/// automation (2026-07-19 report); the per-workspace pref chooses WHICH
/// republics back up while the global switch is on.
#[tokio::test]
async fn global_toggle_off_stops_the_ticker_despite_the_workspace_pref() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (endpoint, log) = stub_server(Arc::new(|method, _path| match method {
        "PUT" => (200, String::new(), 0),
        _ => (200, empty_listing(), 0),
    }))
    .await;
    let (w, id, _root) = founded_engine(tmp.path(), &endpoint, 5).await;
    w.execute(Command::SetWorkspaceBackup { id: id.clone(), enabled: true })
        .await
        .expect("enable per-workspace pref");

    // flip the GLOBAL switch off (what the settings checkbox saves)
    let mut settings = session(&w).await.settings.clone();
    settings.s3_backup = false;
    w.execute(Command::SaveSettings { settings }).await.expect("save");

    // ticks decide NOTHING now: no upload request reaches the server and
    // no stamp appears, despite valid config + per-workspace pref
    w.execute(Command::BackupTick).await.expect("tick 1");
    w.execute(Command::BackupTick).await.expect("tick 2");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        log.lock().expect("log").is_empty(),
        "the global switch off must stop every automatic upload"
    );
    let sv = session(&w).await;
    assert_eq!(
        entry(&sv, &id).last_backup_min,
        WorkspaceInfo::NEVER,
        "no stamp without an upload"
    );
}

/// §8.3.1 + §8.3.2: enabling NEVER stamps; a tick runs a real upload whose
/// blob is the genuine workspace-key-mode export, and only the confirmed
/// upload moves the stamp (prefs + entry).
#[tokio::test]
async fn stamp_moves_only_on_a_confirmed_upload_and_the_blob_is_real() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (endpoint, log) = stub_server(Arc::new(|method, _path| match method {
        "PUT" => (200, String::new(), 0),
        _ => (200, empty_listing(), 0),
    }))
    .await;
    let (w, id, root) = founded_engine(tmp.path(), &endpoint, 5).await;

    // enable: pref persists, NOTHING is stamped, nothing is uploaded
    w.execute(Command::SetWorkspaceBackup { id: id.clone(), enabled: true })
        .await
        .expect("enable");
    let sv = session(&w).await;
    let e = entry(&sv, &id);
    assert!(e.s3, "pref mirrored");
    assert_eq!(
        e.last_backup_min,
        WorkspaceInfo::NEVER,
        "enabling must never invent a backup stamp"
    );
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("dir");
    assert_eq!(
        molt_storage::read_prefs(&dir).last_backup,
        None,
        "no stamp in prefs either"
    );
    assert!(log.lock().expect("log").is_empty(), "no upload ran yet");

    // one ticker pass: the real upload runs and confirms
    w.execute(Command::BackupTick).await.expect("tick");
    let sv = poll_session(&w, "confirmed upload", |sv| {
        entry(sv, &id).last_backup_min != WorkspaceInfo::NEVER
    })
    .await;
    assert_eq!(entry(&sv, &id).backup_error, "", "no failure on success");
    // prefs carry the durable stamp (survives restarts via the boot scan)
    // — the writer thread may still be flushing; poll briefly
    let mut stamped = None;
    for _ in 0..100 {
        stamped = molt_storage::read_prefs(&dir).last_backup;
        if stamped.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let stamped = stamped.expect("prefs.last_backup persisted");
    // the wire saw exactly one PUT with the canonical object key, then the
    // retention listing (empty → nothing to delete)
    let reqs = log.lock().expect("log").clone();
    let puts: Vec<&Req> = reqs.iter().filter(|r| r.method == "PUT").collect();
    assert_eq!(puts.len(), 1, "exactly one upload: {reqs:?}");
    let object_key = puts[0]
        .path
        .strip_prefix("/molt-bucket/")
        .expect("path-style bucket prefix")
        .split('?')
        .next()
        .expect("path")
        .to_string();
    let (parsed_id, ts) = molt_core::parse_backup_key(&object_key)
        .expect("object key follows the §6.2 naming scheme");
    assert_eq!(parsed_id, id, "the key names the workspace pseudonym");
    assert_eq!(ts, stamped, "the stamp IS the uploaded object's timestamp");
    assert!(reqs.iter().any(|r| r.method == "GET"), "retention listed after upload");
    assert!(!reqs.iter().any(|r| r.method == "DELETE"), "nothing beyond keep-copies");

    // the uploaded body is the real blob: workspace key mode, decryptable
    // from the recovery phrase (phrase → key → read_export)
    let phrase = entry(&sv, &id).seed.clone();
    assert!(!phrase.is_empty(), "the entry carries the phrase");
    let seed = molt_storage::seed_entropy(&phrase).expect("phrase parses");
    let key = molt_storage::derive_workspace_key(&seed, &id);
    let archive = molt_storage::export::read_export(
        &mut puts[0].body.as_slice(),
        &molt_storage::export::ExportSecret::WorkspaceKey(key),
    )
    .expect("the uploaded blob decrypts with the phrase-derived key");
    assert_eq!(archive.header.workspace_id, id);
    assert_eq!(archive.header.key_mode, "workspace");
    assert!(
        archive.entries.iter().all(|e| e.path != "transport.state"),
        "live transport state never travels"
    );

    // §8.3.4 interval: a second tick inside the interval spawns nothing
    w.execute(Command::BackupTick).await.expect("tick 2");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let puts_after = log
        .lock()
        .expect("log")
        .iter()
        .filter(|r| r.method == "PUT")
        .count();
    assert_eq!(puts_after, 1, "the interval was not elapsed — no second upload");
}

/// §8.3.1: a failing bucket keeps the stamp untouched and surfaces the
/// real error — no fake success anywhere.
#[tokio::test]
async fn a_failed_upload_never_stamps_and_surfaces_the_reason() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (endpoint, log) = stub_server(Arc::new(|_m, _p| (503, String::new(), 0))).await;
    let (w, id, root) = founded_engine(tmp.path(), &endpoint, 5).await;
    w.execute(Command::SetWorkspaceBackup { id: id.clone(), enabled: true })
        .await
        .expect("enable");
    w.execute(Command::BackupTick).await.expect("tick");
    let sv = poll_session(&w, "honest failure", |sv| {
        !entry(sv, &id).backup_error.is_empty()
    })
    .await;
    let e = entry(&sv, &id);
    assert_eq!(e.last_backup_min, WorkspaceInfo::NEVER, "stamp untouched");
    assert!(e.backup_error.contains("503"), "verbatim reason: {}", e.backup_error);
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("dir");
    assert_eq!(molt_storage::read_prefs(&dir).last_backup, None);
    assert!(log.lock().expect("log").iter().any(|r| r.method == "PUT"));
}

/// §8.3.3 retention: with keep_copies = 3 and five listed generations, the
/// two OLDEST are deleted — exactly those, nothing else.
#[tokio::test]
async fn retention_prunes_exactly_beyond_keep_copies() {
    let tmp = tempfile::tempdir().expect("tmp");
    // the listing must name THIS workspace's id, which is only known after
    // founding — route through a mutable slot
    let listing: Arc<Mutex<String>> = Arc::new(Mutex::new(empty_listing()));
    let route_listing = listing.clone();
    let (endpoint, log) = stub_server(Arc::new(move |method, _path| match method {
        "PUT" => (200, String::new(), 0),
        "DELETE" => (204, String::new(), 0),
        _ => (200, route_listing.lock().expect("listing").clone(), 0),
    }))
    .await;
    let (w, id, _root) = founded_engine(tmp.path(), &endpoint, 3).await;
    let old: Vec<String> = (1..=5u64).map(|ts| molt_core::backup_key(&id, ts)).collect();
    *listing.lock().expect("listing") = listing_of(&old);
    w.execute(Command::SetWorkspaceBackup { id: id.clone(), enabled: true })
        .await
        .expect("enable");
    w.execute(Command::BackupTick).await.expect("tick");
    poll_session(&w, "confirmed upload", |sv| {
        entry(sv, &id).last_backup_min != WorkspaceInfo::NEVER
    })
    .await;
    // deletes settle right after the stamp; poll the request log
    for _ in 0..100 {
        if log.lock().expect("log").iter().filter(|r| r.method == "DELETE").count() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let reqs = log.lock().expect("log").clone();
    let deleted: Vec<String> = reqs
        .iter()
        .filter(|r| r.method == "DELETE")
        .map(|r| r.path.trim_start_matches("/molt-bucket/").to_string())
        .collect();
    assert_eq!(
        deleted,
        vec![old[0].clone(), old[1].clone()],
        "exactly the two oldest go: {reqs:?}"
    );
}

/// §8.3.4 (P6): a sealed-at-rest workspace is skipped with an honest
/// status — no key is accessible, nothing dials the bucket.
#[tokio::test]
async fn a_sealed_workspace_is_skipped_with_an_honest_status() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (endpoint, log) = stub_server(Arc::new(|_m, _p| (200, empty_listing(), 0))).await;
    let (w, id, _root) = founded_engine(tmp.path(), &endpoint, 5).await;
    let phrase = entry(&session(&w).await, &id).seed.clone();
    w.execute(Command::SetWorkspaceBackup { id: id.clone(), enabled: true })
        .await
        .expect("enable");
    w.execute(Command::CloseWorkspace).await.expect("close");
    w.execute(Command::EncryptWorkspace { id: id.clone(), phrase })
        .await
        .expect("seal at rest");
    w.execute(Command::BackupTick).await.expect("tick");
    let sv = poll_session(&w, "honest sealed skip", |sv| {
        entry(sv, &id).backup_error.contains("sealed")
    })
    .await;
    assert_eq!(entry(&sv, &id).last_backup_min, WorkspaceInfo::NEVER);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        log.lock().expect("log").is_empty(),
        "a sealed workspace must not dial the bucket"
    );
    // the manual trigger refuses loudly instead of skipping silently
    let err = w
        .execute(Command::BackupNow { id: id.clone() })
        .await
        .expect_err("backup_now on a sealed workspace is refused");
    assert!(err.to_string().contains("encrypted"), "{err}");
}

/// An in-flight upload blocks a second spawn for the same workspace: two
/// immediate ticks against a slow bucket produce exactly one PUT.
#[tokio::test]
async fn an_inflight_upload_is_never_doubled() {
    let tmp = tempfile::tempdir().expect("tmp");
    let puts = Arc::new(AtomicUsize::new(0));
    let count = puts.clone();
    let (endpoint, _log) = stub_server(Arc::new(move |method, _p| match method {
        "PUT" => {
            count.fetch_add(1, Ordering::SeqCst);
            (200, String::new(), 400) // slow store: the upload stays in flight
        }
        _ => (200, empty_listing(), 0),
    }))
    .await;
    let (w, id, _root) = founded_engine(tmp.path(), &endpoint, 5).await;
    w.execute(Command::SetWorkspaceBackup { id: id.clone(), enabled: true })
        .await
        .expect("enable");
    w.execute(Command::BackupTick).await.expect("tick 1");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    w.execute(Command::BackupTick).await.expect("tick 2");
    poll_session(&w, "confirmed upload", |sv| {
        entry(sv, &id).last_backup_min != WorkspaceInfo::NEVER
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(puts.load(Ordering::SeqCst), 1, "one workspace, one in-flight upload");
}

/// Review finding (MEDIUM): sealing must not race an in-flight backup. A
/// backup reads the very key material `EncryptWorkspace` deletes; if it
/// commits mid-upload the bucket keeps a confirmed-but-unrestorable blob
/// and retention prunes the good copies. Encrypt is refused with
/// `WorkspaceBusy` while a backup of the same workspace is out.
#[tokio::test]
async fn encrypt_is_refused_while_a_backup_is_in_flight() {
    let tmp = tempfile::tempdir().expect("tmp");
    // a slow store keeps the upload in flight across the encrypt attempt
    let (endpoint, _log) = stub_server(Arc::new(|method, _p| match method {
        "PUT" => (200, String::new(), 800),
        _ => (200, empty_listing(), 0),
    }))
    .await;
    let (w, id, _root) = founded_engine(tmp.path(), &endpoint, 5).await;
    let phrase = entry(&session(&w).await, &id).seed.clone();
    // kick off a backup while the workspace is open (inflight set now)
    w.execute(Command::BackupNow { id: id.clone() }).await.expect("backup now");
    // close so the active-workspace guard is out of the way — the inflight
    // guard is what must catch the seal
    w.execute(Command::CloseWorkspace).await.expect("close");
    let err = w
        .execute(Command::EncryptWorkspace { id: id.clone(), phrase })
        .await
        .expect_err("sealing during an in-flight backup is refused");
    assert!(
        err.to_string().to_lowercase().contains("backup"),
        "the refusal names the in-flight backup: {err}"
    );
    // once the upload settles the seal goes through
    poll_session(&w, "backup settled", |sv| {
        entry(sv, &id).last_backup_min != WorkspaceInfo::NEVER
            || !entry(sv, &id).backup_error.is_empty()
    })
    .await;
}

/// Review finding (MEDIUM, gap): a chainless (legacy, pre-chain) dir would
/// export a blob that restore ALWAYS refuses ("no verifiable chain") — a
/// doom discovered only at disaster time. The backup side refuses it up
/// front instead of shipping a useless blob.
#[tokio::test]
async fn a_chainless_dir_is_refused_up_front_and_never_uploaded() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (endpoint, log) = stub_server(Arc::new(|_m, _p| (200, empty_listing(), 0))).await;
    let (w, id, root) = founded_engine(tmp.path(), &endpoint, 5).await;
    w.execute(Command::CloseWorkspace).await.expect("close");
    // simulate a legacy/chainless workspace: drop chain.state
    let dir = molt_storage::find_workspace_dir(&root, &id).expect("dir");
    std::fs::remove_file(dir.join("chain.state")).expect("remove chain.state");
    let err = w
        .execute(Command::BackupNow { id: id.clone() })
        .await
        .expect_err("a chainless dir cannot be backed up");
    assert!(
        err.to_string().to_lowercase().contains("chain"),
        "the refusal names the missing chain: {err}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        log.lock().expect("log").iter().all(|r| r.method != "PUT"),
        "no doomed blob was uploaded"
    );
    // the ticker skips it with an honest status too, no dial
    w.execute(Command::SetWorkspaceBackup { id: id.clone(), enabled: true })
        .await
        .expect("enable");
    w.execute(Command::BackupTick).await.expect("tick");
    let sv = poll_session(&w, "chainless skip status", |sv| {
        entry(sv, &id).backup_error.to_lowercase().contains("chain")
    })
    .await;
    assert_eq!(entry(&sv, &id).last_backup_min, WorkspaceInfo::NEVER);
    assert!(
        log.lock().expect("log").iter().all(|r| r.method != "PUT"),
        "the ticker did not upload a doomed blob either"
    );
}

/// Review finding (LOW): the honest "sealed — skipped" backup status must
/// not outlive the sealed state. Decrypting clears it, so the backup table
/// stops claiming a now-openable workspace is un-backup-able.
#[tokio::test]
async fn decrypting_clears_the_sealed_skip_backup_status() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (endpoint, _log) = stub_server(Arc::new(|_m, _p| (200, empty_listing(), 0))).await;
    let (w, id, _root) = founded_engine(tmp.path(), &endpoint, 5).await;
    let phrase = entry(&session(&w).await, &id).seed.clone();
    w.execute(Command::SetWorkspaceBackup { id: id.clone(), enabled: true })
        .await
        .expect("enable");
    w.execute(Command::CloseWorkspace).await.expect("close");
    w.execute(Command::EncryptWorkspace { id: id.clone(), phrase: phrase.clone() })
        .await
        .expect("seal");
    w.execute(Command::BackupTick).await.expect("tick");
    poll_session(&w, "sealed skip status", |sv| {
        entry(sv, &id).backup_error.contains("sealed")
    })
    .await;
    // decrypt: the sealed-skip note no longer describes anything
    w.execute(Command::DecryptWorkspace { id: id.clone(), phrase })
        .await
        .expect("decrypt");
    let sv = session(&w).await;
    assert_eq!(
        entry(&sv, &id).backup_error,
        "",
        "the sealed-skip status is cleared on decrypt"
    );
    assert!(!entry(&sv, &id).encrypted, "decrypted");
}
