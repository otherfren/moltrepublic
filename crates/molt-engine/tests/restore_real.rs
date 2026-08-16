// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Story 13 (design §8.4): the restore is REAL — file and S3 way. A
//! republic founded via the sim seam exports its blob; a SECOND engine on
//! a fresh root restores it: the engine hard-verifies the threshold-signed
//! chain before anything materializes, the workspace opens DETACHED with
//! the honest §4.4 notice, and the restored chat/chain state equals the
//! source. Failure paths never leave residue and never fake progress —
//! the mock ticker's invented log lines are pinned gone.

use molt_core::{Command, GroupConfig, Reply, SessionSettings, SessionView};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn session(w: &molt_engine::WalletHandle) -> SessionView {
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else {
        panic!("read session failed");
    };
    *sv
}

async fn poll_session(
    w: &molt_engine::WalletHandle,
    what: &str,
    pred: impl Fn(&SessionView) -> bool,
) -> SessionView {
    for _ in 0..600 {
        let sv = session(w).await;
        if pred(&sv) {
            return sv;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    panic!("{what} did not happen in time");
}

/// Found "Roundtrip Republic" (petra, 2-of-3) on a persisted sim engine
/// rooted at `root`, post one chat message, and return `(handle, id,
/// phrase)`.
async fn founded_source(
    root: &std::path::Path,
    settings: SessionSettings,
) -> (molt_engine::WalletHandle, String, String) {
    let sv = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..settings
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(GroupConfig::demo(), sv, true);
    w.execute(Command::CreateStart {
        name: "Roundtrip Republic".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 3,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    // ❻½: the founder's phrase-backup confirmation (n-of-n gate)
    {
        let seed_ = session(&w).await.create.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    let sv = poll_session(&w, "founding", |sv| sv.create.run.outcome != 0).await;
    assert_eq!(sv.create.run.outcome, 1, "founding sealed: {:?}", sv.create.run.log);
    w.execute(Command::CreateFinish).await.expect("create finish");
    let sv = session(&w).await;
    let id = sv.active_workspace.clone();
    let phrase = sv
        .workspaces
        .iter()
        .find(|ws| ws.id == id)
        .expect("entry")
        .seed
        .clone();
    w.execute(Command::Chat {
        body: "history to restore".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("chat");
    (w, id, phrase)
}

/// Manual export of `id` to `dest` with `pass`; waits for the honest ok.
async fn export_blob(
    w: &molt_engine::WalletHandle,
    id: &str,
    dest: &std::path::Path,
    pass: &str,
) {
    w.execute(Command::ExportWorkspace {
        id: id.to_string(),
        dest: dest.display().to_string(),
        passphrase: pass.to_string(),
    })
    .await
    .expect("export kickoff");
    let sv = poll_session(w, "export", |sv| {
        !sv.export.running && !sv.export.result.is_empty()
    })
    .await;
    assert_eq!(sv.export.result, "ok", "export must succeed");
}

/// A fresh restore-side engine on its own root.
fn restore_engine(root: &std::path::Path, settings: SessionSettings) -> molt_engine::WalletHandle {
    let sv = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..settings
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), sv)
}

/// Drive one restore to its terminal outcome; returns the settled session.
async fn run_restore(
    w: &molt_engine::WalletHandle,
    way: &str,
    target: &str,
    secret: &str,
    replace: bool,
) -> SessionView {
    w.execute(Command::RestoreStart {
        way: way.to_string(),
        target: target.to_string(),
        secret: secret.to_string(),
        replace,
    })
    .await
    .expect("restore start");
    poll_session(w, "restore outcome", |sv| sv.restore.run.outcome != 0).await
}

/// The fake-progress removal pin (§8.4.4): none of the mock ticker's
/// invented vocabulary may ever appear in a restore log again.
fn assert_no_invented_lines(log: &[String]) {
    for line in log {
        for fake in [
            "manifest.enc",
            "merkle",
            "aes-256-gcm",
            "sha256 ok",
            "chunk-",
            "rtt ",
        ] {
            assert!(
                !line.contains(fake),
                "invented mock-log vocabulary resurfaced: {line:?}"
            );
        }
    }
}

/// No `.import-*` staging residue under a root.
fn assert_no_staging_residue(root: &std::path::Path) {
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with(".import-"),
                "staging residue left behind: {name}"
            );
        }
    }
}

/// §8.1.1 / §8.4.3 keystone: export → file-restore into a fresh root →
/// verified chain, equal content, DETACHED open with the honest notice.
#[tokio::test]
async fn file_restore_round_trips_verified_and_opens_detached() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (src, id, _phrase) = founded_source(&tmp.path().join("src"), SessionSettings::default()).await;
    let blob = tmp.path().join("roundtrip.molt.enc");
    let pass = "correct horse battery";
    export_blob(&src, &id, &blob, pass).await;

    let dest_root = tmp.path().join("dst");
    let w2 = restore_engine(&dest_root, SessionSettings::default());
    // secret is mandatory — an empty one refuses before anything runs
    let err = w2
        .execute(Command::RestoreStart {
            way: "file".to_string(),
            target: blob.display().to_string(),
            secret: "  ".to_string(),
            replace: false,
        })
        .await
        .expect_err("no secret, no restore");
    assert!(err.to_string().contains("secret"), "{err}");

    let sv = run_restore(&w2, "file", &blob.display().to_string(), pass, false).await;
    assert_eq!(sv.restore.run.outcome, 1, "restore verified: {:?}", sv.restore.run.log);
    assert_eq!(sv.restore.run.progress_pct, 100);
    assert!(
        sv.restore.run.log.iter().any(|l| l.contains("chain verified")),
        "the log names the real verification: {:?}",
        sv.restore.run.log
    );
    assert_no_invented_lines(&sv.restore.run.log);
    // the entry carries the SOURCE workspace id (same identity, not a
    // freshly invented "Restored Republic")
    let entry = sv
        .workspaces
        .iter()
        .find(|ws| ws.id == id)
        .expect("restored entry has the source id");
    assert_eq!(entry.name, "Roundtrip Republic");
    assert_eq!(entry.detail, "2-of-3");
    assert!(!entry.seed.is_empty(), "the re-sealed seed backs the entry");

    // finish opens DETACHED: main screen, honest notice, readable history
    w2.execute(Command::RestoreFinish).await.expect("finish");
    let sv = session(&w2).await;
    assert_eq!(sv.active_workspace, id);
    assert_eq!(sv.notice, "detached", "the §4.4 notice is set");
    match w2
        .execute(Command::ReadState {
            surface: molt_core::Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => {
            assert!(
                s.applied
                    .iter()
                    .any(|v| v.to_string().contains("history to restore")),
                "the restored history is readable"
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
    match w2.execute(Command::Status).await.expect("status") {
        Reply::Status(st) => {
            assert_eq!(st.member, "petra");
            assert_eq!(st.threshold, 2);
            assert_eq!(st.members.len(), 3, "the verified roster is back");
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_no_staging_residue(&dest_root);

    // §8.1.9 collision: importing the same id again refuses…
    w2.execute(Command::CloseWorkspace).await.expect("close");
    let sv = run_restore(&w2, "file", &blob.display().to_string(), pass, false).await;
    assert_eq!(sv.restore.run.outcome, 2, "same-id import refused");
    assert!(
        sv.restore.run.log.iter().any(|l| l.contains("already exists")),
        "honest collision reason: {:?}",
        sv.restore.run.log
    );
    // …and an explicit replace trashes the existing dir first
    let sv = run_restore(&w2, "file", &blob.display().to_string(), pass, true).await;
    assert_eq!(sv.restore.run.outcome, 1, "replace commits: {:?}", sv.restore.run.log);
    assert!(
        dest_root.join(".trash").read_dir().expect("trash").count() > 0,
        "the replaced dir is recoverable"
    );
}

/// A stateful S3 stub: PUT stores the object, GET with `list-type=2` lists
/// the stored keys, a plain GET serves the stored body. Enough wire truth
/// for backup→wipe→restore-from-S3 end to end.
async fn bucket_stub() -> String {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    let store: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::new(Mutex::new(BTreeMap::new()));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let store = store.clone();
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
                let raw_path = parts.next().unwrap_or_default().to_string();
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
                let (path, query) = match raw_path.split_once('?') {
                    Some((p, q)) => (p.to_string(), q.to_string()),
                    None => (raw_path.clone(), String::new()),
                };
                let key = path.trim_start_matches("/molt-bucket/").to_string();
                let response: Vec<u8> = match method.as_str() {
                    "PUT" => {
                        store.lock().expect("store").insert(key, body);
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec()
                    }
                    "GET" if query.contains("list-type=2") => {
                        let contents: String = store
                            .lock()
                            .expect("store")
                            .iter()
                            .map(|(k, v)| {
                                format!(
                                    "<Contents><Key>{k}</Key>\
                                     <LastModified>2026-01-01T00:00:00Z</LastModified>\
                                     <Size>{}</Size></Contents>",
                                    v.len()
                                )
                            })
                            .collect();
                        let xml = format!(
                            "<ListBucketResult><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>"
                        );
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{xml}",
                            xml.len()
                        )
                        .into_bytes()
                    }
                    "GET" => match store.lock().expect("store").get(&key).cloned() {
                        Some(blob) => {
                            let mut resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                blob.len()
                            )
                            .into_bytes();
                            resp.extend_from_slice(&blob);
                            resp
                        }
                        None => b"HTTP/1.1 404 NF\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    },
                    _ => b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
                };
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn s3_settings(endpoint: &str) -> SessionSettings {
    SessionSettings {
        s3_endpoint: endpoint.to_string(),
        s3_access_key: "AKIAEXAMPLE".to_string(),
        s3_secret_key: "secret-example".to_string(),
        s3_bucket: "molt-bucket".to_string(),
        ..SessionSettings::default()
    }
}

/// §8.4.1: backup to the (stub) bucket, then restore FROM the bucket on a
/// fresh root with only the recovery phrase + the workspace id — newest
/// object picked, real download progress, verified chain, detached open.
#[tokio::test]
async fn s3_restore_round_trips_from_the_bucket_with_the_phrase() {
    let tmp = tempfile::tempdir().expect("tmp");
    let endpoint = bucket_stub().await;
    let (src, id, phrase) =
        founded_source(&tmp.path().join("src"), s3_settings(&endpoint)).await;
    // a real backup lands in the stub bucket (the manual trigger — same
    // task as the ticker)
    src.execute(Command::BackupNow { id: id.clone() }).await.expect("backup now");
    poll_session(&src, "confirmed upload", |sv| {
        sv.workspaces
            .iter()
            .any(|w| w.id == id && w.last_backup_min != molt_core::WorkspaceInfo::NEVER)
    })
    .await;

    // "total device loss": a fresh engine, fresh root — only phrase + id
    let dest_root = tmp.path().join("dst");
    let w2 = restore_engine(&dest_root, s3_settings(&endpoint));
    // an unusable target string is refused synchronously, honestly
    let err = w2
        .execute(Command::RestoreStart {
            way: "s3".to_string(),
            target: "https://not-an-id".to_string(),
            secret: phrase.clone(),
            replace: false,
        })
        .await
        .expect_err("endpoint-looking targets are not backups");
    assert!(err.to_string().contains("workspace id"), "{err}");

    let sv = run_restore(&w2, "s3", &id, &phrase, false).await;
    assert_eq!(sv.restore.run.outcome, 1, "restore verified: {:?}", sv.restore.run.log);
    assert!(
        sv.restore.run.log.iter().any(|l| l.contains("s3: GET molt/")),
        "the log names the real object: {:?}",
        sv.restore.run.log
    );
    assert!(
        sv.restore.run.log.iter().any(|l| l.contains("chain verified")),
        "verification happened: {:?}",
        sv.restore.run.log
    );
    assert_no_invented_lines(&sv.restore.run.log);
    w2.execute(Command::RestoreFinish).await.expect("finish");
    let sv = session(&w2).await;
    assert_eq!(sv.active_workspace, id);
    assert_eq!(sv.notice, "detached");
    match w2
        .execute(Command::ReadState {
            surface: molt_core::Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => assert!(
            s.applied
                .iter()
                .any(|v| v.to_string().contains("history to restore")),
            "the bucket round-trip preserved the history"
        ),
        other => panic!("unexpected: {other:?}"),
    }
    assert_no_staging_residue(&dest_root);
}

/// S7 (2026-08-08, `backup_restore_design.md` §10): "Restore from backup"
/// lands SEALED. `BackupFetch` downloads the newest object VERBATIM into a
/// stub entry the Open list shows as a restored backup — no secret asked,
/// nothing decrypted. `DecryptWorkspace` with the recovery phrase then
/// drives the verified restore pipeline; a wrong phrase refuses (sync on a
/// malformed one, async on a well-formed-but-wrong one) and keeps the
/// artifact either way.
#[tokio::test]
async fn backup_fetch_lands_sealed_and_the_phrase_opens_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let endpoint = bucket_stub().await;
    let (src, id, phrase) =
        founded_source(&tmp.path().join("src"), s3_settings(&endpoint)).await;
    src.execute(Command::BackupNow { id: id.clone() }).await.expect("backup now");
    poll_session(&src, "confirmed upload", |sv| {
        sv.workspaces
            .iter()
            .any(|w| w.id == id && w.last_backup_min != molt_core::WorkspaceInfo::NEVER)
    })
    .await;

    // total device loss: fresh engine + root — only the id from the backup
    // table, NO phrase at fetch time
    let dest_root = tmp.path().join("dst");
    let w2 = restore_engine(&dest_root, s3_settings(&endpoint));
    w2.execute(Command::BackupFetch { id: id.clone() })
        .await
        .expect("fetch acked");
    let sv = poll_session(&w2, "the sealed artifact to land", |sv| {
        sv.workspaces
            .iter()
            .any(|w| w.id == id && w.restored)
    })
    .await;
    let entry = sv.workspaces.iter().find(|w| w.id == id).expect("entry");
    assert!(
        entry.seed.is_empty(),
        "a fetched artifact must carry no phrase material"
    );

    // a malformed phrase refuses synchronously and changes nothing
    let err = w2
        .execute(Command::DecryptWorkspace {
            id: id.clone(),
            phrase: "not a bip39 phrase".to_string(),
        })
        .await
        .expect_err("a malformed phrase must refuse");
    assert!(!err.to_string().is_empty());
    // a WELL-FORMED but wrong phrase fails the restore run and keeps the
    // artifact (the decrypt happens off-actor)
    let wrong = molt_storage::generate_seed_phrase().expect("wrong phrase");
    w2.execute(Command::DecryptWorkspace { id: id.clone(), phrase: wrong })
        .await
        .expect("the well-formed wrong phrase starts the run");
    let sv = poll_session(&w2, "the wrong-phrase run to fail", |sv| {
        sv.restore.run.outcome == 2
    })
    .await;
    assert!(
        sv.workspaces
            .iter()
            .any(|w| w.id == id && w.restored),
        "the artifact survives a wrong phrase"
    );

    // the RIGHT phrase drives the verified pipeline: chain-verify, then a
    // real local workspace replaces the artifact
    w2.execute(Command::DecryptWorkspace { id: id.clone(), phrase: phrase.clone() })
        .await
        .expect("decrypt starts");
    let sv = poll_session(&w2, "the restore to verify + materialize", |sv| {
        sv.workspaces
            .iter()
            .any(|w| w.id == id && !w.restored)
    })
    .await;
    assert!(
        sv.restore.run.log.iter().any(|l| l.contains("chain verified")),
        "the open ran the verified pipeline: {:?}",
        sv.restore.run.log
    );
    w2.execute(Command::OpenWorkspace { id: id.clone() }).await.expect("open");
    match w2
        .execute(Command::ReadState {
            surface: molt_core::Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => assert!(
            s.applied
                .iter()
                .any(|v| v.to_string().contains("history to restore")),
            "the fetched + opened workspace carries the history"
        ),
        other => panic!("unexpected: {other:?}"),
    }
    assert_no_staging_residue(&dest_root);
}

/// §8.1.3/§8.1.5 at the engine level: a flipped byte rejects at the
/// decrypt layer; a FORGED CHAIN BLOCK rejects at the verify phase — and
/// neither leaves a directory or staging residue.
#[tokio::test]
async fn tampered_blob_and_forged_chain_restore_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let src_root = tmp.path().join("src");
    let (src, id, _phrase) = founded_source(&src_root, SessionSettings::default()).await;
    let blob_path = tmp.path().join("good.molt.enc");
    let pass = "correct horse battery";
    export_blob(&src, &id, &blob_path, pass).await;

    // 1) flipped byte → decrypt-layer reject
    let mut tampered = std::fs::read(&blob_path).expect("blob");
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0x01;
    let tampered_path = tmp.path().join("tampered.molt.enc");
    std::fs::write(&tampered_path, &tampered).expect("write tampered");
    let dest_root = tmp.path().join("dst");
    let w2 = restore_engine(&dest_root, SessionSettings::default());
    let sv = run_restore(&w2, "file", &tampered_path.display().to_string(), pass, false).await;
    assert_eq!(sv.restore.run.outcome, 2, "tampered blob must fail");
    assert!(
        sv.restore.run.log.iter().any(|l| l.contains("restore failed")),
        "honest failure line: {:?}",
        sv.restore.run.log
    );
    assert!(
        molt_storage::find_workspace_dir(&dest_root, &id).is_none(),
        "nothing materialized"
    );
    assert_no_staging_residue(&dest_root);

    // 2) forged chain: doctor one signature inside the source's chain.state
    // and re-export — decrypts fine, must die at the VERIFY phase
    src.execute(Command::CloseWorkspace).await.expect("close src");
    let src_dir = molt_storage::find_workspace_dir(&src_root, &id).expect("src dir");
    {
        let (ws, _loaded) = molt_storage::open_workspace(&src_dir).expect("open for forgery");
        let (blob, mut chain) = ws.read_chain().expect("chain readable");
        assert!(blob.is_none() && !chain.is_empty(), "a real genesis chain");
        let sig = &mut chain[0].sigs[0].sig;
        let flipped = if sig.ends_with('0') { "1" } else { "0" };
        sig.replace_range(sig.len() - 1.., flipped);
        ws.write_chain(None, &chain).expect("write forged chain");
    }
    let forged_path = tmp.path().join("forged.molt.enc");
    {
        let mut out = std::fs::File::create(&forged_path).expect("create");
        molt_storage::export::export_dir(
            &src_root,
            &src_dir,
            &molt_storage::export::ExportKey::passphrase(pass),
            &mut out,
        )
        .expect("export the forged dir");
    }
    let sv = run_restore(&w2, "file", &forged_path.display().to_string(), pass, false).await;
    assert_eq!(sv.restore.run.outcome, 2, "a forged block must fail the restore");
    assert!(
        sv.restore.run.log.iter().any(|l| l.contains("chain verification failed")),
        "the verify phase names itself: {:?}",
        sv.restore.run.log
    );
    assert!(
        molt_storage::find_workspace_dir(&dest_root, &id).is_none(),
        "a forged chain materializes NOTHING"
    );
    assert_no_staging_residue(&dest_root);
}

/// Review finding (HIGH): a restore-REPLACE aimed at the currently OPEN
/// workspace must be refused. The collision/replace path (design §4.3) is
/// for CLOSED dirs; replacing an OPEN one would `fs::rename` the live
/// directory into `.trash` from under its own running writer — ENOENT
/// writes, a dangling `self.active`, a duplicate id dir. It must never
/// touch the live dir.
#[tokio::test]
async fn restore_replace_onto_the_open_workspace_is_refused_and_never_trashes_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let src_root = tmp.path().join("src");
    let (src, id, _phrase) = founded_source(&src_root, SessionSettings::default()).await;
    let blob = tmp.path().join("self.molt.enc");
    let pass = "correct horse battery";
    export_blob(&src, &id, &blob, pass).await;

    // the workspace is still OPEN on this same engine — replace=true of its
    // own id must be refused, not trash the live dir
    let sv = run_restore(&src, "file", &blob.display().to_string(), pass, true).await;
    assert_eq!(
        sv.restore.run.outcome, 2,
        "restore-replace onto the open workspace is refused: {:?}",
        sv.restore.run.log
    );
    assert!(
        sv.restore
            .run
            .log
            .iter()
            .any(|l| l.to_lowercase().contains("close")),
        "the refusal tells the user to close it first: {:?}",
        sv.restore.run.log
    );
    // the live directory survived — never moved to .trash
    assert!(
        molt_storage::find_workspace_dir(&src_root, &id).is_some(),
        "the open workspace directory is intact"
    );
    let trash = src_root.join(".trash");
    assert!(
        !trash.exists() || trash.read_dir().expect("trash").count() == 0,
        "the live dir was NOT trashed"
    );
    assert_no_staging_residue(&src_root);
    // still open and usable
    let sv = session(&src).await;
    assert_eq!(sv.active_workspace, id, "the workspace stays open");
}

/// §8.4.2 failure honesty: a download that dies mid-stream fails the
/// restore with the true reason — no dir, no staging residue.
#[tokio::test]
async fn an_aborted_download_fails_honestly_with_no_residue() {
    // a stub that declares more bytes than it sends, then closes
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\npartial")
                    .await;
                let _ = sock.shutdown().await;
            });
        }
    });
    let endpoint = format!("http://127.0.0.1:{}", addr.port());
    let tmp = tempfile::tempdir().expect("tmp");
    let dest_root = tmp.path().join("dst");
    let w = restore_engine(&dest_root, s3_settings(&endpoint));
    let object = molt_core::backup_key(&"ab".repeat(32), 1);
    let sv = run_restore(&w, "s3", &object, "some secret words here", false).await;
    assert_eq!(sv.restore.run.outcome, 2, "mid-stream death must fail");
    assert!(
        sv.restore.run.log.iter().any(|l| l.contains("download failed")),
        "the true failure line: {:?}",
        sv.restore.run.log
    );
    assert_no_invented_lines(&sv.restore.run.log);
    assert_no_staging_residue(&dest_root);
    // cancel from the failed state returns to choice, still no residue
    w.execute(Command::RestoreCancel).await.expect("cancel");
    assert_no_staging_residue(&dest_root);
}
