// SPDX-License-Identifier: GPL-3.0-or-later

//! The engine's unit tests over the actor (`State`) and the public
//! `WalletHandle` surface - moved out of `lib.rs` unchanged (review E8).

use super::*;
use molt_core::{demo_workspace_id, Screen, SessionSettings};
use serde_json::json;

mod chat_tests;
mod support;
pub(crate) use support::{plain_state, tiny_bmp_header};
use support::*;

/// **One export at a time** - and the rule is decidable without any I/O.
/// The integration twin proved this by parking the writer on a FIFO, i.e.
/// on the very wedge `check_writable_target` now refuses, and it rode a
/// 300 ms sleep besides. The guard itself needs neither.
#[test]
fn a_second_wiki_export_while_one_runs_is_refused() {
    let mut st = plain_state();
    st.session.wiki_export.running = true;
    let err = st
        .cmd_wiki_export("/tmp/anywhere".to_string(), false)
        .expect_err("one export at a time");
    assert_eq!(err.to_string(), "wiki export: an export is already running");
}

/// seed_backup_confirmation.md keystone (❻½): an all-ratified founding
/// does NOT finalize — nothing lands on disk — until the founder's own
/// recovery-phrase backup is confirmed (sim members auto-confirm
/// theirs). A wrong re-typed phrase is refused; the right one seals.
#[test]
fn founding_waits_for_the_backup_confirmation_before_writing() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let ws_dir = tmp.path().join("workspaces");
        let session = SessionView {
            workspaces: Vec::new(),
            settings: SessionSettings {
                workspace_dir: ws_dir.display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = __spawn_sim_founding(GroupConfig::demo(), session, true);
        w.execute(Command::CreateStart {
            name: "Backup".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
        // every sim seat ratifies AND auto-confirms its backup…
        let mut seen_all = false;
        for _ in 0..600 {
            let s = read_session(&w).await;
            assert_ne!(s.create.run.outcome, 2, "founding failed: {:?}", s.create.run.log);
            if s.create.seats.iter().all(|x| x.state >= 2) && !s.create.seats.is_empty() {
                seen_all = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(seen_all, "sim seats never ratified");
        // …but WITHOUT the founder's own confirmation nothing seals and
        // nothing is written (grace window, then re-check)
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let s = read_session(&w).await;
        assert_eq!(
            s.create.run.outcome, 0,
            "the founding sealed without the founder's backup confirmation: {:?}",
            s.create.run.log
        );
        let disk_empty = !ws_dir.exists()
            || std::fs::read_dir(&ws_dir).map(|d| d.count() == 0).unwrap_or(true);
        assert!(disk_empty, "the ritual wrote to disk before the last confirmation");
        // a wrong re-typed phrase is refused, the ritual keeps waiting
        let wrong = w
            .execute(Command::ConfirmSeedBackup { phrase: "wrong words entirely".to_string() })
            .await;
        assert!(wrong.is_err(), "a wrong phrase must be refused");
        assert_eq!(read_session(&w).await.create.run.outcome, 0);
        // the right phrase confirms, the founding finalizes, disk exists
        let seed = read_session(&w).await.create.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed })
            .await
            .expect("confirm backup");
        await_founding(&w).await;
        let s = read_session(&w).await;
        assert_eq!(s.active_workspace.len(), 64);
    });
}

/// The "it survives a restart" keystone: found a republic on a storage
/// engine, write chat + a threshold-applied proposal, close, reopen —
/// the replayed state equals the live state exactly.
#[test]
fn workspace_state_survives_close_and_reopen() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let session = SessionView {
            workspaces: Vec::new(),
            settings: SessionSettings {
                workspace_dir: tmp.path().join("workspaces").display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        // offline sim seam, storage-backed (this test reopens from disk)
        let w = __spawn_sim_founding(GroupConfig::demo(), session, true);

        // found a 2-of-3 republic
        w.execute(Command::CreateStart {
            name: "Keystone".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish");
        let s = read_session(&w).await;
        let id = s.active_workspace.clone();
        assert_eq!(id.len(), 64, "a real derived workspace id");
        let ws = s.workspaces.iter().find(|x| x.id == id).expect("entry");
        assert_eq!(ws.name, "Keystone");
        // the recovery phrase stays in the entry (decision 2026-07-15:
        // stored device-sealed, shown by the Open screen's details
        // panel while the workspace is at-rest-unencrypted)
        assert_eq!(ws.seed.split(' ').count(), 24, "the real phrase: {}", ws.seed);

        // write history: chat, reaction, delete, proposal to threshold
        // (all chat verbs address by stable id since the chat bus)
        w.execute(Command::Chat {
            body: "first".to_string(),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat 1");
        let first_id = msg_id(&read_surface(&w, Surface::Chat).await.applied[0]);
        w.execute(Command::Chat {
            body: "second".to_string(),
            quote: Some(first_id),
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat 2");
        let second_id = msg_id(&read_surface(&w, Surface::Chat).await.applied[1]);
        w.execute(Command::ReactChat {
            id: first_id,
            emoji: "👍".to_string(),
        })
        .await
        .expect("react");
        let pid = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"persisted"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::Approve { proposal: pid })
            .await
            .expect("approve");
        w.execute(Command::DeleteChat { id: second_id })
            .await
            .expect("delete");
        // two file shares (real temp files): one stays available, one
        // is removed — both states must survive the reopen
        let src_dir = tmp.path().join("sources");
        std::fs::create_dir_all(&src_dir).expect("src dir");
        let kept_share_id =
            share_temp_file(&w, &src_dir, "charter.pdf", b"the sealed charter").await;
        let removed_share_id =
            share_temp_file(&w, &src_dir, "draft.md", b"a draft to remove").await;
        w.execute(Command::RemoveFile {
            id: removed_share_id,
        })
        .await
        .expect("remove");

        let chat_before = read_surface(&w, Surface::Chat).await;
        let memory_before = read_surface(&w, Surface::Memory).await;

        // close (flush + closing snapshot + LOCK release), then reopen
        w.execute(Command::CloseWorkspace).await.expect("close");
        assert_eq!(read_session(&w).await.active_workspace, "");
        w.execute(Command::OpenWorkspace { id: id.clone() })
            .await
            .expect("reopen");

        let s = read_session(&w).await;
        assert_eq!(s.active_workspace, id);
        assert_eq!(s.screen, Screen::Main);
        let chat_after = read_surface(&w, Surface::Chat).await;
        let memory_after = read_surface(&w, Surface::Memory).await;
        assert_eq!(chat_after.applied, chat_before.applied);
        assert_eq!(memory_after.applied, memory_before.applied);
        assert_eq!(memory_after.pending.len(), memory_before.pending.len());

        // the file shares replay with their availability intact — and
        // stay addressable by the SAME ids after the reopen. The kept
        // share's source path came back via prefs (this node keeps
        // serving/copying across restarts): downloading the own share
        // is an honest local copy into the destination
        let dest_dir = tmp.path().join("downloads");
        std::fs::create_dir_all(&dest_dir).expect("dest dir");
        w.execute(Command::DownloadFile {
            id: kept_share_id,
            dest: Some(dest_dir.display().to_string()),
        })
        .await
        .expect("kept file downloads after reopen");
        await_file(&dest_dir.join("charter.pdf"), b"the sealed charter").await;
        assert!(matches!(
            w.execute(Command::DownloadFile {
                id: removed_share_id,
                dest: None,
            })
            .await,
            Err(MoltError::FileUnavailable(i)) if i == removed_share_id
        ));

        // the roster, rule, and founding date replayed from the genesis
        match w.execute(Command::Status).await.expect("status") {
            Reply::Status(st) => {
                assert_eq!(st.member, "petra");
                assert_eq!(st.threshold, 2);
                assert_eq!(st.members.len(), 3);
                assert!(
                    st.founded_ts > 0,
                    "the genesis envelope's timestamp is the founding date"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }

        // a second engine cannot open it while we hold the LOCK
        w.execute(Command::CloseWorkspace).await.expect("close 2");
        w.execute(Command::OpenWorkspace { id: id.clone() })
            .await
            .expect("open 3");
        let session2 = SessionView {
            workspaces: read_session(&w).await.workspaces.clone(),
            settings: SessionSettings {
                workspace_dir: tmp.path().join("workspaces").display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w2 = spawn_with_storage(GroupConfig::demo(), session2);
        assert!(matches!(
            w2.execute(Command::OpenWorkspace { id: id.clone() }).await,
            Err(MoltError::WorkspaceBusy(_))
        ));

        // deleting moves the directory to .trash and closes it
        w.execute(Command::DeleteWorkspace { id: id.clone() })
            .await
            .expect("delete ws");
        let root = tmp.path().join("workspaces");
        assert!(molt_storage::find_workspace_dir(&root, &id).is_none());
        assert!(root.join(".trash").read_dir().expect("trash").count() > 0);
    });
}

/// **A founded republic never reads as "never seen".** Those seats
/// founded it WITH this node - the genesis carries the date they all
/// signed - so an install that forgot every stamp is lying, not being
/// careful. Presence knowledge is LOCAL (never the chain): it lives in
/// the workspace's `prefs.toml`, starts at the founding date, advances
/// on every real sighting, and survives close/reopen. A republic
/// founded before this memory existed reads its founding date too.
#[test]
fn the_founding_dates_every_seat_and_the_stamps_survive_a_reopen() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let root = tmp.path().join("workspaces");
        let session = SessionView {
            workspaces: Vec::new(),
            settings: SessionSettings {
                workspace_dir: root.display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = __spawn_sim_founding(GroupConfig::demo(), session, true);
        w.execute(Command::CreateStart {
            name: "Seen".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish");

        let id = read_session(&w).await.active_workspace.clone();
        let founded = match w.execute(Command::Status).await.expect("status") {
            Reply::Status(st) => st.founded_ts,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(founded > 0, "the genesis carries the founding date");

        async fn seats(w: &WalletHandle, id: &str) -> Vec<(String, u64)> {
            read_session(w)
                .await
                .workspaces
                .iter()
                .find(|x| x.id == id)
                .expect("entry")
                .members
                .iter()
                .map(|m| (m.name.clone(), m.last_seen))
                .collect()
        }

        let founding = seats(&w, &id).await;
        assert_eq!(founding.len(), 3, "a 2-of-3 roster");
        for (name, last) in &founding {
            assert_ne!(
                *last,
                molt_core::MemberInfo::NEVER,
                "{name} reads as never seen right after the founding"
            );
            assert!(*last >= founded, "{name} is dated before its own founding");
        }

        // a real sighting advances one seat AND reaches the disk: the
        // memory is worthless if it only lives in this process
        let seat = founding
            .iter()
            .map(|(n, _)| n.clone())
            .find(|n| n != "petra")
            .expect("another seat");
        w.execute(Command::NetPeerSeen {
            member: seat.clone(),
            generation: None,
        })
        .await
        .expect("sighting");
        w.execute(Command::CloseWorkspace).await.expect("close");
        let dir = molt_storage::find_workspace_dir(&root, &id).expect("workspace dir");
        let prefs = molt_storage::read_prefs(&dir);
        assert!(
            prefs.last_seen.get(&seat).copied().unwrap_or(0) >= founded,
            "the sighting never reached prefs.toml: {:?}",
            prefs.last_seen
        );

        // a stamp NEWER than the founding is what a reopen must read back
        // (the founding date is only the floor under it)
        let later = founded + 4_242;
        let mut bumped = molt_storage::read_prefs(&dir);
        bumped.last_seen.insert(seat.clone(), later);
        molt_storage::write_prefs(&dir, &bumped).expect("bump the stamp");

        // reopening keeps every date - this is where the operator met
        // "never seen" on a republic he founded himself
        w.execute(Command::OpenWorkspace { id: id.clone() })
            .await
            .expect("reopen");
        for (name, last) in seats(&w, &id).await {
            assert_ne!(
                last,
                molt_core::MemberInfo::NEVER,
                "{name} lost its date across the reopen"
            );
            if name == seat {
                assert_eq!(last, later, "the remembered stamp did not come back");
            }
        }

        // a republic founded BEFORE this memory existed carries no
        // stamps at all - it still reads its founding date
        w.execute(Command::CloseWorkspace).await.expect("close 2");
        let mut legacy = molt_storage::read_prefs(&dir);
        legacy.last_seen.clear();
        molt_storage::write_prefs(&dir, &legacy).expect("wipe the stamps");
        w.execute(Command::OpenWorkspace { id: id.clone() })
            .await
            .expect("reopen 2");
        for (name, last) in seats(&w, &id).await {
            assert!(
                last >= founded,
                "{name} fell back to never-seen on a stamp-less workspace"
            );
        }
    });
}

/// Story 9: the manual export drives a REAL `molt-export-v1` blob onto
/// disk (decryptable at the storage layer), enforces the passphrase
/// policy synchronously, and reports an unwritable path as an honest
/// error — never a fake success.
#[test]
fn manual_export_writes_a_real_blob_and_fails_honestly() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let session = SessionView {
            workspaces: Vec::new(),
            settings: SessionSettings {
                workspace_dir: tmp.path().join("workspaces").display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = __spawn_sim_founding(GroupConfig::demo(), session, true);
        w.execute(Command::CreateStart {
            name: "Blob Republic".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish");
        let id = read_session(&w).await.active_workspace.clone();
        w.execute(Command::Chat {
            body: "history to back up".to_string(),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat");

        let pass = "correct horse battery".to_string();
        // passphrase policy: engine-enforced, synchronous, honest
        let err = w
            .execute(Command::ExportWorkspace {
                id: id.clone(),
                dest: tmp.path().join("x.molt.enc").display().to_string(),
                passphrase: "neunchars".to_string(),
            })
            .await
            .expect_err("9 chars must be refused");
        assert!(err.to_string().contains("at least 10"), "{err}");
        // unknown workspace is refused before anything runs
        assert!(w
            .execute(Command::ExportWorkspace {
                id: "77".repeat(32),
                dest: tmp.path().join("x.molt.enc").display().to_string(),
                passphrase: pass.clone(),
            })
            .await
            .is_err());

        // the real export, into a directory that does not exist yet
        let dest = tmp.path().join("backups").join("blob.molt.enc");
        w.execute(Command::ExportWorkspace {
            id: id.clone(),
            dest: dest.display().to_string(),
            passphrase: pass.clone(),
        })
        .await
        .expect("export kickoff");
        let sv = read_session(&w).await;
        assert_eq!(sv.export.workspace, id);
        let outcome = await_export(&w).await;
        assert_eq!(outcome.result, "ok", "export must succeed: {outcome:?}");
        assert!(outcome.bytes > 0);
        let blob = std::fs::read(&dest).expect("blob on disk");
        assert_eq!(outcome.bytes, u64::try_from(blob.len()).expect("len"));
        // the blob decrypts and verifies at the storage layer
        let a = molt_storage::export::read_export(
            &mut blob.as_slice(),
            &molt_storage::export::ExportSecret::passphrase(pass.clone()),
        )
        .expect("blob decrypts");
        assert_eq!(a.header.workspace_id, id);
        assert!(a.entries.iter().any(|e| e.path == "manifest.toml"));
        assert!(a.entries.iter().any(|e| e.path == "log/000001.mlog"));
        assert!(
            a.entries.iter().all(|e| e.path != "transport.state"),
            "live transport state must never be exported"
        );
        // no stray .part file remains
        assert!(std::fs::read_dir(dest.parent().expect("parent"))
            .expect("dir")
            .all(|e| !e
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".part")));

        // honest failure: the destination's parent is a FILE — the task
        // must report the real error, not a fake success
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"in the way").expect("blocker");
        w.execute(Command::ExportWorkspace {
            id: id.clone(),
            dest: blocker.join("nope.molt.enc").display().to_string(),
            passphrase: pass,
        })
        .await
        .expect("kickoff acks; the failure arrives async");
        let outcome = await_export(&w).await;
        assert!(
            outcome.result.starts_with("error: "),
            "unwritable path must fail honestly, got: {outcome:?}"
        );
    });
}


/// **The story-10 keystone:** at-rest sealing is real, phrase-verified
/// and derived from the directory (design §8.2, engine level). Found a
/// republic on a storage engine, close it, seal it with the real phrase
/// — the key material is gone from disk, a fresh scan (≈ restart)
/// reports the sealed state, open refuses honestly, a wrong phrase
/// changes nothing, and the right phrase brings everything back.
#[test]
fn at_rest_sealing_is_real_verified_and_survives_a_restart() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let root = tmp.path().join("workspaces");
        let session = SessionView {
            workspaces: Vec::new(),
            settings: SessionSettings {
                workspace_dir: root.display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = __spawn_sim_founding(GroupConfig::demo(), session, true);
        w.execute(Command::CreateStart {
            name: "Vaulted".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("create start");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish");
        let s = read_session(&w).await;
        let id = s.active_workspace.clone();
        let phrase = s.workspaces.iter().find(|x| x.id == id).expect("entry").seed.clone();
        assert_eq!(phrase.split(' ').count(), 24, "the real phrase");
        let dir = molt_storage::find_workspace_dir(&root, &id).expect("dir");

        // the ACTIVE workspace cannot be sealed from under itself
        assert!(matches!(
            w.execute(Command::EncryptWorkspace {
                id: id.clone(),
                phrase: phrase.clone(),
            })
            .await,
            Err(MoltError::WorkspaceBusy(_))
        ));
        w.execute(Command::CloseWorkspace).await.expect("close");

        // encrypt requires phrase PROOF: a foreign (valid) phrase and an
        // empty one are refused, and nothing is deleted
        let foreign = molt_storage::generate_seed_phrase().expect("gen");
        assert!(w
            .execute(Command::EncryptWorkspace {
                id: id.clone(),
                phrase: foreign.clone(),
            })
            .await
            .is_err());
        assert!(w
            .execute(Command::EncryptWorkspace {
                id: id.clone(),
                phrase: String::new(),
            })
            .await
            .is_err());
        assert!(dir.join("keys/workspace.key").exists(), "nothing deleted");
        assert!(dir.join("keys/seed.sealed").exists());

        // the real phrase seals: key material gone, session honest
        w.execute(Command::EncryptWorkspace {
            id: id.clone(),
            phrase: phrase.clone(),
        })
        .await
        .expect("encrypt");
        assert!(!dir.join("keys/workspace.key").exists(), "key removed");
        assert!(!dir.join("keys/seed.sealed").exists(), "seed removed");
        {
            let s = read_session(&w).await;
            let ws = s.workspaces.iter().find(|x| x.id == id).expect("entry");
            assert!(ws.encrypted);
            assert!(ws.seed.is_empty(), "no phrase to show while sealed");
            assert!(ws.members.is_empty(), "no roster to show while sealed");
        }
        assert!(matches!(
            w.execute(Command::OpenWorkspace { id: id.clone() }).await,
            Err(MoltError::WorkspaceEncrypted(_))
        ));

        // restart persistence: a FRESH scan of the directory (what boot
        // does) derives the sealed state — no session memory involved
        let entries = molt_storage::scan_workspaces(&root);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].info().encrypted, "a restart still sees it sealed");
        // …and a second engine booted from that scan refuses the open
        let session2 = SessionView {
            workspaces: entries.iter().map(|e| e.info()).collect(),
            settings: SessionSettings {
                workspace_dir: root.display().to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w2 = spawn_with_storage(GroupConfig::demo(), session2);
        assert!(matches!(
            w2.execute(Command::OpenWorkspace { id: id.clone() }).await,
            Err(MoltError::WorkspaceEncrypted(_))
        ));

        // wrong phrase on decrypt: hard error, still sealed on disk
        assert!(w
            .execute(Command::DecryptWorkspace {
                id: id.clone(),
                phrase: foreign,
            })
            .await
            .is_err());
        assert!(!dir.join("keys/workspace.key").exists(), "still sealed");
        assert!(
            molt_storage::scan_workspaces(&root)[0].info().encrypted,
            "still sealed after the failed attempt"
        );

        // the right phrase unseals; the entry gets its details back and
        // the workspace opens and replays
        w.execute(Command::DecryptWorkspace {
            id: id.clone(),
            phrase: phrase.clone(),
        })
        .await
        .expect("decrypt");
        {
            let s = read_session(&w).await;
            let ws = s.workspaces.iter().find(|x| x.id == id).expect("entry");
            assert!(!ws.encrypted);
            assert_eq!(ws.seed, phrase, "the stored phrase is shown again");
            assert!(!ws.members.is_empty(), "the roster is back");
        }
        w.execute(Command::OpenWorkspace { id: id.clone() })
            .await
            .expect("open after decrypt");
        match w.execute(Command::Status).await.expect("status") {
            Reply::Status(st) => {
                assert_eq!(st.member, "petra");
                assert_eq!(st.members.len(), 3, "the history replayed");
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

#[test]
fn workspace_backup_toggles_and_stamps() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), demo_session());
        // "Savings-DAO" ships without auto-backup
        let before = match w.execute(Command::ReadSession).await.expect("read0") {
            Reply::Session(s) => s
                .workspaces
                .iter()
                .find(|ws| ws.name == "Savings-DAO")
                .expect("workspace")
                .last_backup_min,
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::SetWorkspaceBackup {
            id: demo_workspace_id("Savings-DAO"),
            enabled: true,
        })
        .await
        .expect("enable");
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                let ws = s
                    .workspaces
                    .iter()
                    .find(|ws| ws.name == "Savings-DAO")
                    .expect("workspace");
                assert!(ws.s3);
                // honest stamps (story 12): enabling persists the pref and
                // NOTHING else — the stamp moves only on a confirmed
                // upload (NetBackupDone), never on the toggle
                assert_eq!(
                    ws.last_backup_min, before,
                    "enabling must never invent a backup stamp"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        let err = w
            .execute(Command::SetWorkspaceBackup {
                id: demo_workspace_id("No Such"),
                enabled: true,
            })
            .await;
        assert!(matches!(err, Err(MoltError::UnknownWorkspace(_))));
    });
}

#[test]
fn propose_then_threshold_applies() {
    rt().block_on(async {
        // 1-of-3, no self-cosign: the proposal genuinely waits for a
        // vote, and this node's OWN single approval honestly meets the
        // threshold — no peer is ever counted for.
        let cfg = GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(
            read_surface(&w, Surface::Memory).await.pending.len(),
            1,
            "no self-cosign: the proposal waits for this node's vote"
        );
        w.execute(Command::Approve { proposal: id })
            .await
            .expect("approve");
        match w
            .execute(Command::ReadState {
                surface: Surface::Memory,
                channel: None,
                view: None,
            })
            .await
            .expect("read")
        {
            Reply::State(s) => {
                assert_eq!(s.applied.len(), 1, "note should be applied at threshold");
                assert!(s.pending.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// Without chain governance this node records at most its OWN approval.
/// The pre-chain counting simulation (a repeated `Approve` counted as
/// the next member's co-signature) is gone from the production path: a
/// repeat is refused with an honest error, the counter never moves, and
/// no proposal applies on invented peer approvals.
#[test]
fn approve_never_counts_invented_peer_approvals() {
    rt().block_on(async {
        // self_cosign: proposing already recorded my one real approval
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        for _ in 0..2 {
            let err = w
                .execute(Command::Approve { proposal: id })
                .await
                .expect_err("a second local approval cannot stand in for a peer");
            assert!(
                matches!(err, MoltError::AlreadyApproved(got) if got == id),
                "unexpected: {err:?}"
            );
        }
        let snap = read_surface(&w, Surface::Memory).await;
        assert!(snap.applied.is_empty(), "2-of-3 never applies on one member");
        assert_eq!(snap.pending.len(), 1);
        assert_eq!(
            snap.pending[0].approvals, 1,
            "exactly this node's own approval, nothing invented"
        );
        assert!(snap.pending[0].approved_by_me);
    });
}

/// The explicit-vote twin: without self-cosign the FIRST `Approve` is
/// this node's real vote and is recorded; the second is the refused
/// simulation. The votes row attributes only what is known — me.
#[test]
fn second_local_approval_is_refused_without_chain_governance() {
    rt().block_on(async {
        let cfg = GroupConfig {
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::Approve { proposal: id })
            .await
            .expect("my own first approval is real");
        let err = w
            .execute(Command::Approve { proposal: id })
            .await
            .expect_err("no second local approval");
        assert!(
            matches!(err, MoltError::AlreadyApproved(got) if got == id),
            "unexpected: {err:?}"
        );
        let snap = read_surface(&w, Surface::Memory).await;
        assert!(snap.applied.is_empty());
        assert_eq!(snap.pending[0].approvals, 1);
        // honest attribution: my vote is mine, the peers stay open
        for v in &snap.pending[0].votes {
            let expect = if v.member == "me" {
                molt_core::VoteState::Approved
            } else {
                molt_core::VoteState::Open
            };
            assert_eq!(v.vote, expect, "stance of {}", v.member);
        }
    });
}

/// The open-time crash recovery must not resurrect the simulation: a
/// legacy log whose counter reached a threshold > 1 did so on invented
/// peer approvals, and minting a fresh `Applied` from that count would
/// fake a threshold decision no member made. Such proposals stay
/// pending (decline is the only exit).
#[test]
fn recovery_never_applies_from_simulated_counts() {
    let mut st = plain_state(); // 2-of-3 demo config
    let e = |seq: u64, by: &str, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
        seq,
        ts: 100 + seq,
        by: by.to_string(),
        body,
    };
    st.apply(&e(
        1,
        "me",
        molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(1),
            surface: Surface::Memory,
            payload: json!({"op":"add_note","title":"t"}),
        },
    ));
    // a legacy pre-chain log: two counted approvals (the second was the
    // simulation), crash before the Applied frame
    for seq in [2, 3] {
        st.apply(&e(
            seq,
            "me",
            molt_core::WorkspaceEvent::Approved {
                id: molt_core::ProposalId(1),
                by: "me".to_string(),
                height: 0,
                sig: String::new(),
            },
        ));
    }
    st.recover_pending_applies();
    let snap = st.snapshot(Surface::Memory, None, None);
    assert!(snap.applied.is_empty(), "no apply on invented peer counts");
    assert_eq!(snap.pending.len(), 1, "the legacy proposal stays pending");
}

/// The honest twin: at threshold 1 the one recorded vote is the local
/// operator's real decision, so a crash between the `Approved` frame
/// and its `Applied` frame recovers into the applied state at open.
#[test]
fn recovery_completes_a_real_single_operator_decision() {
    let mut st = plain_state();
    st.config.threshold = 1;
    let e = |seq: u64, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
        seq,
        ts: 100 + seq,
        by: "me".to_string(),
        body,
    };
    st.apply(&e(
        1,
        molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(1),
            surface: Surface::Memory,
            payload: json!({"op":"add_note","title":"t"}),
        },
    ));
    st.apply(&e(
        2,
        molt_core::WorkspaceEvent::Approved {
            id: molt_core::ProposalId(1),
            by: "me".to_string(),
            height: 0,
            sig: String::new(),
        },
    ));
    st.recover_pending_applies();
    let snap = st.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.applied.len(), 1, "my one real vote recovers to applied");
    assert!(snap.pending.is_empty());
}

/// The solo boot group (1-of-1) is REAL governance, not a simulation:
/// the only member's own self-cosigned approval meets the threshold,
/// so a proposal applies through the same honest single-operator path.
#[test]
fn solo_boot_group_runs_real_one_of_one_governance() {
    rt().block_on(async {
        let w = spawn(GroupConfig::solo(), SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"solo"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        let snap = read_surface(&w, Surface::Memory).await;
        assert_eq!(
            snap.applied.len(),
            1,
            "the sole member's own approval meets threshold 1"
        );
        assert!(snap.pending.is_empty());
        // a late vote on the decided proposal names the terminal state
        let err = w
            .execute(Command::Approve { proposal: id })
            .await
            .expect_err("the vote is decided");
        assert!(
            matches!(err, MoltError::AlreadyTerminal(got, _) if got == id),
            "unexpected: {err:?}"
        );
    });
}

/// At-rest sealing on a SESSION-ONLY node (no storage — unit tests,
/// ephemeral nodes): there are no on-disk bytes to seal and no genesis
/// to verify a phrase against, so BOTH commands refuse honestly instead
/// of faking a flag flip (the pre-story-10 mock accepted any phrase
/// here while the tool texts promised real verification). The real,
/// phrase-verified path is pinned by
/// [`at_rest_sealing_is_real_verified_and_survives_a_restart`]; the
/// session's `encrypted` flag still gates open (it is scan-derived on
/// storage nodes), pinned there too.
#[test]
fn a_storageless_node_refuses_to_fake_at_rest_sealing() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), demo_session());
        let id = demo_workspace_id("Family Office");
        // an empty phrase is rejected before anything else
        assert!(
            w.execute(Command::EncryptWorkspace {
                id: id.clone(),
                phrase: String::new(),
            })
            .await
            .is_err(),
            "encrypting needs a phrase"
        );
        // …and WITH a phrase the storage-less node still refuses: it
        // cannot verify or seal anything, and must not pretend to
        assert!(matches!(
            w.execute(Command::EncryptWorkspace {
                id: id.clone(),
                phrase: "word1 word2 word3".into(),
            })
            .await,
            Err(MoltError::Storage(_))
        ));
        let entry = |s: &SessionView| {
            s.workspaces
                .iter()
                .find(|ws| ws.id == id)
                .map(|ws| ws.encrypted)
                .expect("entry")
        };
        assert!(!entry(&*read_session(&w).await), "nothing was faked");
        assert!(matches!(
            w.execute(Command::DecryptWorkspace {
                id: id.clone(),
                phrase: "word1 word2 word3".into(),
            })
            .await,
            Err(MoltError::Storage(_))
        ));
        // an unknown id reports UnknownWorkspace, not a phrase error
        assert!(matches!(
            w.execute(Command::EncryptWorkspace {
                id: "no-such".into(),
                phrase: String::new(),
            })
            .await,
            Err(MoltError::UnknownWorkspace(_))
        ));
    });
}

/// The status summary carries the founding date (the genesis envelope's
/// timestamp — real on replayed workspaces, 0 on the sessionless demo)
/// and the REAL activity trio: nobody in the demo boot group has ever
/// been seen on the wire, so only the local member counts anywhere.
#[test]
fn status_carries_founding_date_and_activity() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        match w.execute(Command::Status).await.expect("status") {
            Reply::Status(st) => {
                assert_eq!(st.founded_ts, 0, "the demo group has no genesis event");
                assert_eq!(
                    st.active_7d, 1,
                    "honest presence: never-seen peers count nowhere - only the local member"
                );
                assert!(st.active_1h <= st.active_24h && st.active_24h <= st.active_7d);
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// The pending cards' "Ist-Stand / Soll-Stand" pair: an Organization
/// edit proposal exposes what the state is now (from the genesis
/// replica) and what the change would make it (the payload's `value`).
/// Display data, never consensus input — empty when unknown.
#[test]
fn org_pending_cards_carry_current_and_proposed_state() {
    let eff = |image: &str| proposals::OrgEffective {
        name: "Guild".into(),
        agenda: "alte Satzung".into(),
        retention_days: 7,
        image: image.to_string(),
        relays: String::new(),
        features: String::new(),
    };
    let rec = |surface: Surface, op: &str, value: &str| molt_core::ProposalRecord {
        surface,
        payload: json!({"op": op, "title": "t", "value": value}),
        approvals: 0,
        state: molt_core::ProposalState::Proposed,
        declined_at: 0,
        declined_by: String::new(),
        decliners: Vec::new(),
            voted: Vec::new(),
        by: String::new(),
        superseded: false,
        withdrawn: false,
    };
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "set_charter", "neue Satzung")
        ),
        ("alte Satzung".to_string(), "neue Satzung".to_string())
    );
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "set_name", "New Guild")
        ),
        ("Guild".to_string(), "New Guild".to_string())
    );
    // the image ops carry the current image reference as their Ist-Stand
    // ("" while none is set → the UI hides the empty line)
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "set_image", "~/logo.png")
        ),
        (String::new(), "~/logo.png".to_string())
    );
    assert_eq!(
        proposals::change_summary(
            &eff("/tmp/old.png"),
            &rec(Surface::Organization, "set_image", "~/logo.png")
        ),
        ("/tmp/old.png".to_string(), "~/logo.png".to_string())
    );
    assert_eq!(
        proposals::change_summary(
            &eff("/tmp/old.png"),
            &rec(Surface::Organization, "remove_image", "")
        ),
        ("/tmp/old.png".to_string(), String::new())
    );
    // a non-organization proposal exposes no pair beyond its value
    assert_eq!(
        proposals::change_summary(&eff(""), &rec(Surface::Memory, "add_note", "")),
        (String::new(), String::new())
    );
    // the chat-retention Ist-Stand is a MACHINE value (L10): the unit
    // renders in the frontends, per language; a legacy "14 days"
    // payload rides through untouched (the parser eats it)
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "set_chat_retention", "14 days")
        ),
        ("7".to_string(), "14 days".to_string())
    );
    // ops are free-form wire strings, so an older log may carry one this
    // build doesn't know (e.g. the retired plugin vocabulary): tolerated,
    // the Ist-Stand simply stays empty — never a rejection
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "enable_plugin", "calendar")
        ),
        (String::new(), "calendar".to_string())
    );
}

/// The republic's effective display identity is a fold of the applied
/// Organization log over the genesis: an applied `set_name` /
/// `set_charter` / `set_chat_retention` actually changes what every
/// reader sees (`StatusView.name/agenda/chat_retention_days`), and the
/// pending cards carry the EFFECTIVE state as their Ist-Stand. The
/// genesis itself stays immutable — it is only the fold's floor.
#[test]
fn effective_identity_follows_the_applied_org_ops() {
    rt().block_on(async {
        // 1-of-3, no self-cosign: this node's own single approval
        // honestly applies each change (no peer is counted for)
        let cfg = GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let status = |w: &WalletHandle| {
            let w = w.clone();
            async move {
                match w.execute(Command::Status).await.expect("status") {
                    Reply::Status(st) => st,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        let propose = |op: &'static str, value: &'static str| {
            let w = w.clone();
            async move {
                let payload = json!({"op": op, "title": "t", "value": value});
                match w
                    .execute(Command::Propose {
                        surface: Surface::Organization,
                        payload,
                    })
                    .await
                    .expect("propose")
                {
                    Reply::Proposed { id } => id,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        let st = status(&w).await;
        assert_eq!(st.name, "", "a demo workspace has no genesis name");
        assert_eq!(st.agenda, "");
        assert_eq!(st.chat_retention_days, 7, "the default window is 7 days");
        for (op, value) in [
            ("set_name", "Neue Gilde"),
            ("set_charter", "wir bauen echte dinge"),
            ("set_chat_retention", "14 days"),
        ] {
            let id = propose(op, value).await;
            w.execute(Command::Approve { proposal: id }).await.expect("approve");
        }
        let st = status(&w).await;
        assert_eq!(st.name, "Neue Gilde");
        assert_eq!(st.agenda, "wir bauen echte dinge");
        assert_eq!(st.chat_retention_days, 14);
        // a follow-up proposal shows the EFFECTIVE state as Ist-Stand
        let _next = propose("set_name", "Dritte Gilde").await;
        let pending = read_surface(&w, Surface::Organization).await.pending;
        assert_eq!(pending[0].current, "Neue Gilde");
        assert_eq!(pending[0].proposed, "Dritte Gilde");
        // a bare number parses as days too
        let id = propose("set_chat_retention", "21").await;
        w.execute(Command::Approve { proposal: id }).await.expect("approve");
        assert_eq!(status(&w).await.chat_retention_days, 21);
        // nonsense is refused at propose time — an unparseable window
        // must never reach the applied log
        for bad in ["bald", "", "0 days", "9999 days"] {
            let err = w
                .execute(Command::Propose {
                    surface: Surface::Organization,
                    payload: json!({"op": "set_chat_retention", "title": "t", "value": bad}),
                })
                .await
                .expect_err("an unparseable retention window is refused");
            assert!(
                matches!(err, MoltError::BadPayload(_)),
                "unexpected error for {bad:?}: {err:?}"
            );
        }
        // an empty name is refused too (the fold must never go blank)
        let err = w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op": "set_name", "title": "t", "value": "  "}),
            })
            .await
            .expect_err("an empty name is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
    });
}

/// WP1 (governance follow-ups): the read contract carries a parallel id
/// track — `SurfaceSnapshot.applied_ids` is positionally parallel to
/// `applied` and names the proposal each entry came from. `None` =
/// origin unknown (chat rows, legacy dumps). The payloads themselves
/// stay byte-identical — the UI fate probe and MCP readers compare them.
#[test]
fn applied_entries_carry_their_proposal_id() {
    let mut st = plain_state();
    let e = |seq: u64, by: &str, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
        seq,
        ts: 100 + seq,
        by: by.to_string(),
        body,
    };
    let payload = json!({"op": "add_note", "title": "minutes"});
    st.apply(&e(
        1,
        "petra",
        molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(4),
            surface: Surface::Memory,
            payload: payload.clone(),
        },
    ));
    st.apply(&e(
        2,
        "walter",
        molt_core::WorkspaceEvent::Applied {
            id: molt_core::ProposalId(4),
        },
    ));
    let snap = st.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.applied, vec![payload.clone()], "payload untouched");
    assert_eq!(
        snap.applied_ids,
        vec![Some(4)],
        "the applied entry knows the proposal it came from"
    );
    // chat rows have no proposal origin: same length, all None
    st.apply(&e(
        3,
        "petra",
        // ts 0 = unknown age: always inside the retention read window
        molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            molt_core::MessageId([7u8; 16]),
            "petra",
            "gm",
            0,
        )),
    ));
    let chat = st.snapshot(Surface::Chat, None, None);
    assert_eq!(chat.applied.len(), 1);
    assert_eq!(chat.applied_ids, vec![None]);
    // a NEW dump round-trips the id track…
    let dump = st.snapshot_now().state;
    let mut st2 = plain_state();
    st2.restore_dump(dump.clone());
    assert_eq!(
        st2.snapshot(Surface::Memory, None, None).applied_ids,
        vec![Some(4)]
    );
    // …a LEGACY dump (a pre-id writer: the field is absent) restores the
    // payloads unchanged with unknown origin
    let mut v = serde_json::to_value(&dump).expect("dump serializes");
    v.as_object_mut().expect("a JSON object").remove("applied_ids");
    let legacy: molt_core::EngineStateDump =
        serde_json::from_value(v).expect("legacy dump deserializes");
    let mut st3 = plain_state();
    st3.restore_dump(legacy);
    let restored = st3.snapshot(Surface::Memory, None, None);
    assert_eq!(restored.applied, vec![payload], "payloads survive untouched");
    assert_eq!(restored.applied_ids, vec![None], "unknown origin stays honest");
}

/// The republic's current image is derived from the applied
/// Organization log: the last applied `set_image` wins, an applied
/// `remove_image` clears it — and the pending image cards carry it as
/// their Ist-Stand. A `set_image` now CARRIES the bytes (base64 in the
/// payload — sign-what-you-see: members vote on the actual image); on
/// a session-only workspace (no storage dir to materialize a logo
/// file into) the reference falls back to the proposed display value.
#[test]
fn current_image_follows_the_applied_org_ops() {
    use base64::Engine as _;
    rt().block_on(async {
        // 1-of-3, no self-cosign: this node's own single approval
        // honestly applies each change (no peer is counted for)
        let cfg = GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let status = |w: &WalletHandle| {
            let w = w.clone();
            async move {
                match w.execute(Command::Status).await.expect("status") {
                    Reply::Status(st) => st,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        // a real 2x2 PNG — since WP3 the bytes must decode as a picture
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==".to_string();
        let propose = |op: &'static str, value: &'static str, with_bytes: bool| {
            let w = w.clone();
            let b64 = b64.clone();
            async move {
                let mut payload = json!({"op": op, "title": "t", "value": value});
                if with_bytes {
                    payload["bytes_b64"] = json!(b64);
                }
                match w
                    .execute(Command::Propose {
                        surface: Surface::Organization,
                        payload,
                    })
                    .await
                    .expect("propose")
                {
                    Reply::Proposed { id } => id,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        assert_eq!(status(&w).await.image, "", "no image before any change");
        // 1-of-3: this node's own approval applies the change
        let id = propose("set_image", "team.png", true).await;
        w.execute(Command::Approve { proposal: id }).await.expect("approve");
        assert_eq!(status(&w).await.image, "team.png");
        // a follow-up image proposal shows the applied state as Ist-Stand
        let next = propose("set_image", "new.png", true).await;
        let pending = read_surface(&w, Surface::Organization).await.pending;
        assert_eq!(pending[0].current, "team.png");
        assert_eq!(pending[0].proposed, "new.png");
        w.execute(Command::Approve { proposal: next }).await.expect("approve");
        assert_eq!(status(&w).await.image, "new.png", "last applied wins");
        // an applied remove_image clears the state again
        let rm = propose("remove_image", "", false).await;
        w.execute(Command::Approve { proposal: rm }).await.expect("approve");
        assert_eq!(status(&w).await.image, "");
        // a set_image without the actual bytes is refused — the mock
        // path-reference era is over (nothing real could be applied)
        let err = w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op": "set_image", "title": "t", "value": "x.png"}),
            })
            .await
            .expect_err("a set_image without bytes is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
        // bytes beyond what the transport can carry are refused with a
        // clear error — the ceiling is DERIVED from the publish budget
        // (`proposals::size_gate_tests` pins the derivation), and 256 KiB
        // is comfortably past it
        let big = base64::engine::general_purpose::STANDARD
            .encode(vec![0u8; 256 * 1024]);
        let err = w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op": "set_image", "title": "t", "value": "big.png", "bytes_b64": big}),
            })
            .await
            .expect_err("an oversized image is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
    });
}

/// WP3: a `set_image` proposal must carry DECODABLE bytes — a member
/// asked to sign-what-they-see must be able to see it. The engine
/// sniffs format + header dimensions (never a full decode — decode
/// bombs); real 2×2 fixtures of every picker format pass, garbage and
/// a dimension bomb are refused with an honest error.
#[test]
fn an_undecodable_set_image_proposal_is_refused() {
    use base64::Engine as _;
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let propose = |b64: String| {
            let w = w.clone();
            async move {
                w.execute(Command::Propose {
                    surface: Surface::Organization,
                    payload: json!({
                        "op": "set_image", "value": "x.png", "bytes_b64": b64,
                    }),
                })
                .await
            }
        };
        // garbage bytes: refused with a clear error
        let garbage =
            base64::engine::general_purpose::STANDARD.encode(b"definitely not an image");
        let err = propose(garbage).await.expect_err("garbage is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
        // a dimension bomb: a valid BMP HEADER declaring 20000x20000 —
        // the sniff reads only the header and refuses before any decode
        let bomb = base64::engine::general_purpose::STANDARD
            .encode(tiny_bmp_header(20_000, 20_000));
        let err = propose(bomb).await.expect_err("a dimension bomb is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
        // real minimal raster files (2x2, PIL-generated — the molt-ui
        // preview fixtures) pass for every remaining picker format
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
        let webp = "UklGRjoAAABXRUJQVlA4IC4AAACwAQCdASoCAAIAAUAmJaACdLoABDAAAP7x3I/4DdfFtMv/vYL/3YL/3YL/WwAA";
        for (fmt, b64) in [("png", png.to_string()), ("webp", webp.to_string())] {
            propose(b64).await.unwrap_or_else(|e| panic!("{fmt} must pass: {e:?}"));
        }
        // L1 (decided 2026-08-16): SVG is refused with its OWN reason —
        // the prefix sniff accepted any <svg/<?xml text unvetted
        // (billion-laughs class), and a structural vetting would be a
        // hand-rolled parser gate (the URL-parser lesson). Applied
        // legacy SVG logos keep rendering; this is propose/wire-only.
        let svg = base64::engine::general_purpose::STANDARD.encode(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#f00"/></svg>"##,
        );
        let err = propose(svg).await.expect_err("svg is refused");
        assert!(
            format!("{err:?}").contains("svg is not accepted"),
            "the refusal names the reason: {err:?}"
        );
        let bomb = base64::engine::general_purpose::STANDARD.encode(
            r#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol">]><svg>&lol;</svg>"#,
        );
        let err = propose(bomb).await.expect_err("an xml entity bomb is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
    });
}

/// Organization is a gated surface like the others: charter / name /
/// logo / retention changes go through propose → threshold → applied — and
/// because the MCP `propose` tool derives its surface list from
/// `is_gated`, the GUI edit modals and an MCP agent drive the SAME path.
#[test]
fn organization_changes_are_gated_proposals() {
    rt().block_on(async {
        // 1-of-3, no self-cosign: propose leaves the vote genuinely
        // open, this node's own approval honestly applies it
        let cfg = GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op":"set_charter","title":"Charter ändern","value":"neue Satzung"}),
            })
            .await
            .expect("propose on organization")
        {
            Reply::Proposed { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        // the pending view carries the Soll-Stand (the payload's value);
        // the Ist-Stand stays empty on a demo workspace (no genesis)
        let pending = read_surface(&w, Surface::Organization).await.pending;
        assert_eq!(pending[0].proposed, "neue Satzung");
        assert_eq!(pending[0].current, "");
        // threshold 1: this node's own approval applies the change
        w.execute(Command::Approve { proposal: id })
            .await
            .expect("approve");
        let snap = read_surface(&w, Surface::Organization).await;
        assert!(snap.gated, "organization is threshold-gated");
        assert_eq!(snap.applied.len(), 1, "applied at threshold");
        assert!(snap.pending.is_empty());
        // an op this build doesn't know still proposes: ops are free-form
        // wire strings (an MCP agent or an older/newer build may mint
        // one), so the validator only vets the ops it understands
        w.execute(Command::Propose {
            surface: Surface::Organization,
            payload: json!({"op":"enable_plugin","title":"t","value":"calendar"}),
        })
        .await
        .expect("an unknown org op is tolerated, not rejected");
    });
}

/// The pending cards render a voting row: per-member stance in roster
/// order. On the single-operator path the only attributable vote is
/// this node's own — my approval flips exactly my pill, every peer
/// honestly stays open.
#[test]
fn pending_views_carry_per_member_votes() {
    rt().block_on(async {
        let cfg = GroupConfig {
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let roster = cfg.members.clone();
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"minutes"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        // fresh proposal, no self-cosign: the whole roster is open
        let votes = &read_surface(&w, Surface::Memory).await.pending[0].votes;
        assert_eq!(
            votes.iter().map(|v| v.member.clone()).collect::<Vec<_>>(),
            roster,
            "one entry per roster member, in roster order"
        );
        assert!(votes.iter().all(|v| v.vote == molt_core::VoteState::Open));
        // my approval flips exactly my entry (the demo member is "me")
        w.execute(Command::Approve { proposal: id })
            .await
            .expect("approve");
        let votes = &read_surface(&w, Surface::Memory).await.pending[0].votes;
        for v in votes {
            let expect = if v.member == "me" {
                molt_core::VoteState::Approved
            } else {
                molt_core::VoteState::Open
            };
            assert_eq!(v.vote, expect, "stance of {}", v.member);
        }
    });
}

/// The read contract splits a surface's open governance by the reader:
/// a pending proposal says whether THIS node already approved it
/// (`approved_by_me`), and declined proposals count into `denied` —
/// the Organization → Status approvals table renders exactly these.
#[test]
fn pending_views_split_by_my_vote_and_count_denied() {
    rt().block_on(async {
        // no self-cosign: a fresh proposal starts with zero approvals,
        // so it genuinely waits on this node's vote
        let cfg = GroupConfig {
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let propose = |title: &str| {
            let w = &w;
            let payload = json!({"op":"add_note","title":title});
            async move {
                match w
                    .execute(Command::Propose {
                        surface: Surface::Memory,
                        payload,
                    })
                    .await
                    .expect("propose")
                {
                    Reply::Proposed { id } => id,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        let waiting_on_me = propose("waiting").await;
        let voted = propose("voted").await;
        let declined = propose("declined").await;
        // one approval of two: still pending, but no longer waiting on me
        w.execute(Command::Approve { proposal: voted })
            .await
            .expect("approve");
        w.execute(Command::Decline { proposal: declined })
            .await
            .expect("decline");
        let snap = read_surface(&w, Surface::Memory).await;
        assert_eq!(snap.pending.len(), 2);
        let by_id = |id| {
            snap.pending
                .iter()
                .find(|p| p.id == id)
                .expect("pending view")
        };
        assert!(
            !by_id(waiting_on_me).approved_by_me,
            "an untouched proposal waits on this node's vote"
        );
        assert!(
            by_id(voted).approved_by_me,
            "the own approval must reflect in the pending view"
        );
        assert_eq!(snap.denied, 1, "the declined proposal counts as denied");
    });
}

/// A declined proposal leaves `pending` and surfaces in the snapshot's
/// `declined` list — with who declined and when (the envelope ts the
/// GUI's retention window filters on), and the decliner's stance marked
/// in the votes row. The Organization → Declined view renders exactly
/// this projection.
#[test]
fn declined_proposals_surface_with_decliner_and_timestamp() {
    rt().block_on(async {
        let cfg = GroupConfig {
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"nope"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::Decline { proposal: id })
            .await
            .expect("decline");
        let snap = read_surface(&w, Surface::Memory).await;
        assert!(snap.pending.is_empty(), "a decline leaves pending");
        assert_eq!(snap.denied, 1, "the count stays for the status strip");
        assert_eq!(snap.declined.len(), 1, "the declined view is exposed");
        let v = &snap.declined[0];
        assert_eq!(v.id, id);
        assert_eq!(v.state, molt_core::ProposalState::Rejected);
        assert_eq!(v.declined_by, "me", "the decliner is named");
        assert!(v.declined_at > 0, "the decline carries its envelope ts");
        let mine = v
            .votes
            .iter()
            .find(|x| x.member == "me")
            .expect("my roster row");
        assert_eq!(
            mine.vote,
            molt_core::VoteState::Declined,
            "the votes row marks the decliner"
        );
    });
}

#[test]
fn workspaces_and_restore_lifecycle_are_shared() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), demo_session());

        // open by id moves to main and records the active workspace
        w.execute(Command::OpenWorkspace {
            id: demo_workspace_id("Family Office"),
        })
        .await
        .expect("open");
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Main);
                assert_eq!(s.active_workspace, demo_workspace_id("Family Office"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        // deleting an unknown workspace is an error
        assert!(matches!(
            w.execute(Command::DeleteWorkspace {
                id: demo_workspace_id("Nope"),
            })
            .await,
            Err(MoltError::UnknownWorkspace(_))
        ));

        // the fake-progress restore is GONE: a storage-less engine has
        // nowhere to restore into and refuses honestly instead of
        // running a progress show (story 13 — the real pipeline is
        // exercised end-to-end in tests/restore_real.rs)
        let err = w
            .execute(Command::RestoreStart {
                way: "s3".to_string(),
                target: "ab".repeat(32),
                secret: "some secret".to_string(),
                replace: false,
            })
            .await
            .expect_err("no storage → no restore");
        assert!(err.to_string().contains("storage"), "{err}");
        // finishing without a successful restore stays refused
        assert!(w.execute(Command::RestoreFinish).await.is_err());
    });
}

/// N4a: a PRODUCTION founding (no test seam) runs over Nostr — and on a
/// fresh node with an EMPTY relay pool (ADR-0004: nothing pre-configured)
/// it fails honestly, naming the missing prerequisite, before a single
/// ticket leaves the engine.
#[test]
fn create_start_without_a_confirmed_relay_fails_honestly() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let err = w
            .execute(Command::CreateStart {
                name: "Gap".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
                relays: Vec::new(),
            })
            .await
            .expect_err("no confirmed relay → no founding");
        assert!(
            err.to_string().contains("no relay configured"),
            "the honest prerequisite error surfaces: {err}"
        );
    });
}

/// …and the SAME refusal must not misdiagnose the pool it is looking at.
/// A confirmed clearnet relay with non-onion dialing switched off (the
/// hand-written `confirmed = true` without `clearnet_enabled = true`) was
/// told to "add and confirm one" — the one thing the operator had already
/// done, while the switch that was actually off went unmentioned.
#[test]
fn create_start_names_the_clearnet_switch_when_that_is_what_blocks_it() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::RelayAdd { url: "wss://relay.example.org".to_string() })
            .await
            .expect("add");
        w.execute(Command::RelayConfirm {
            url: "wss://relay.example.org".to_string(),
            accept_clearnet: true,
        })
        .await
        .expect("confirm");
        // B4: the confirmation lands on the PROBE's verdict, and this
        // relay does not exist — inject the verdict directly; the test
        // is about the clearnet SWITCH, not about reachability
        let url = match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => s.settings.relays[0].url.clone(),
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::NetRelayProbed {
            url,
            error: String::new(),
            unreachable: false,
            confirm: true,
        })
        .await
        .expect("verdict");
        // the operator (or their config file) leaves non-onion dialing off
        w.execute(Command::RelayClearnetSession { unlock: false })
            .await
            .expect("dark");
        let err = w
            .execute(Command::CreateStart {
                name: "Gap".to_string(),
                member: "petra".to_string(),
                threshold: 2,
                members: 3,
                relays: Vec::new(),
            })
            .await
            .expect_err("nothing dialable → no founding");
        let err = err.to_string();
        assert!(
            err.contains("clearnet_enabled") && !err.contains("no relay configured"),
            "the refusal names the switch, not a confirmation that exists: {err}"
        );
    });
}

#[test]
fn create_lifecycle_founds_a_republic() {
    rt().block_on(async {
        // the offline sim seam (session-only): simulated members seal the
        // ritual so the founder-side lifecycle can be tested without a
        // network — a production founding fails honestly until N4
        let w = __spawn_sim_founding(GroupConfig::demo(), SessionView::default(), false);

        // invalid configurations are rejected up front
        assert!(matches!(
            w.execute(Command::CreateStart {
                name: "X".to_string(),
                member: "me".to_string(),
                threshold: 4,
                members: 3,
                relays: Vec::new(),
            })
            .await,
            Err(MoltError::Create(_))
        ));
        for bad_n in [1_u8, 14] {
            assert!(matches!(
                w.execute(Command::CreateStart {
                    name: "X".to_string(),
                    member: "me".to_string(),
                    threshold: 1,
                    members: bad_n,
                    relays: Vec::new(),
                })
                .await,
                Err(MoltError::Create(_))
            ));
        }
        // threshold 1 is refused since 2026-08-08 (product decision) —
        // the engine gate, so MCP meets it too, not only the GUI stepper
        assert!(matches!(
            w.execute(Command::CreateStart {
                name: "X".to_string(),
                member: "me".to_string(),
                threshold: 1,
                members: 2,
                relays: Vec::new(),
            })
            .await,
            Err(MoltError::Create(_))
        ));

        // a valid founding runs the ritual: two seats, each activated
        // and sealed by a simulated member, then the workspace is born
        w.execute(Command::CreateStart {
            name: "Chess Club".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("start");
        // "Enter republic" is refused until every seat is sealed
        assert!(matches!(
            w.execute(Command::CreateFinish).await,
            Err(MoltError::Create(_))
        ));
        await_founding(&w).await;
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_eq!(s.create.run.outcome, 1);
                // sealed ≠ entered (2026-08-08): the founder backs its
                // phrase up on the wizard's last step first — the exact
                // twin of the joiner's JoinFinish gate
                assert_ne!(s.screen, Screen::Main, "sealing must not auto-enter");
                assert_eq!(s.create.seed.split(' ').count(), 24);
                assert_eq!(s.create.seats.len(), 2);
                for seat in &s.create.seats {
                    // 3 = sealed AND backup-confirmed (❻½): a finalized
                    // founding implies every seat attested its backup
                    assert_eq!(seat.state, 4, "every seat sealed + backup-confirmed");
                    assert!(!seat.member.is_empty(), "the member named itself");
                    // a SIMULATED founding mints preview-only links (the
                    // path shape, no handover) — the preview parser is
                    // the right reader; real links are neutral segments
                    let info =
                        molt_core::InviteInfo::parse(&seat.link).expect("invite parses");
                    assert_eq!(info.republic, "Chess Club");
                    assert_eq!(info.inviter, "petra");
                }
                // the log carries the real ritual events, not a fake anim
                assert!(s.create.run.log.iter().any(|l| l.contains("activated invite")));
                assert!(s.create.run.log.iter().any(|l| l.contains("signed the roster")));
                assert!(s.create.run.log.iter().any(|l| l.contains("workspace created")));
            }
            other => panic!("unexpected: {other:?}"),
        }
        w.execute(Command::CreateFinish).await.expect("finish");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Main);
                assert_eq!(s.active_workspace, demo_workspace_id("Chess Club"));
                let ws = s
                    .workspaces
                    .iter()
                    .find(|w| w.name == "Chess Club")
                    .expect("workspace added");
                assert_eq!(ws.detail, "2-of-3");
                assert_eq!(ws.members.len(), 3);
                assert_eq!(s.create, molt_core::CreateState::default());
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// The persisted `WorkspaceInfo.net` label mirrors the EFFECTIVE global
/// anonymity setting — the ritual transport always comes from the global
/// settings (`resolve_dialer`), so the label must reflect those and never
/// a client-supplied string (tor_transport_implementation.md §P8).
#[test]
fn workspace_net_label_mirrors_the_global_anonymity_setting() {
    rt().block_on(async {
        // default settings (anonymity = "none") → the label says "none"
        let w = __spawn_sim_founding(GroupConfig::demo(), SessionView::default(), false);
        w.execute(Command::CreateStart {
            name: "Plain".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("start");
        // the run header shows the effective network while the ritual runs
        assert_eq!(read_session(&w).await.create.net, "none");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish");
        let s = read_session(&w).await;
        let ws = s.workspaces.iter().find(|x| x.name == "Plain").expect("entry");
        assert_eq!(ws.net, "none", "label = the effective global setting");

        // tor configured globally → the label says "tor"
        let session = SessionView {
            settings: SessionSettings {
                anonymity: "tor".to_string(),
                ..SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = __spawn_sim_founding(GroupConfig::demo(), session, false);
        w.execute(Command::CreateStart {
            name: "Onioned".to_string(),
            member: "petra".to_string(),
            threshold: 2,
            members: 3,
            relays: Vec::new(),
        })
        .await
        .expect("start tor");
        assert_eq!(read_session(&w).await.create.net, "tor");
        await_founding(&w).await;
        w.execute(Command::CreateFinish).await.expect("finish tor");
        let s = read_session(&w).await;
        let ws = s.workspaces.iter().find(|x| x.name == "Onioned").expect("entry");
        assert_eq!(ws.net, "tor", "label = the effective global setting");
    });
}

#[test]
fn join_requires_a_joinable_link() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());

        // empty, plain text, and a bare preview link (no transport
        // handover) are all rejected — a real join needs a link that
        // carries the transport handover
        for bad in [
            "  ",
            "not-an-invite",
            "molt://invite/Chess-Club/2of3/walter/k9x2m4q7aa",
        ] {
            assert!(
                matches!(
                    w.execute(Command::JoinStart {
                        invite: bad.to_string(),
                        member: "petra".to_string(),
                    })
                    .await,
                    Err(MoltError::Join(_))
                ),
                "should reject `{bad}`"
            );
        }
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => assert_eq!(s.join, molt_core::JoinState::default()),
            other => panic!("unexpected: {other:?}"),
        }

        // a real founding link (with the transport handover) arms the
        // wizard — and then fails HONESTLY: this build has no network
        // relay-gate refusal: this node shares NO relay with the invite
        // (its pool is empty), so the run says exactly that — naming both
        // sides — instead of dialing somewhere the operator never approved
        let link = crate::FoundingInvite {
            info: molt_core::InviteInfo {
                republic: "Chess Club".to_string(),
                threshold: 2,
                members: 2,
                inviter: "walter".to_string(),
                ticket: "ab".repeat(32),
            },
            handover: molt_net::invite::InviteHandoverV2 {
                seat: 0,
                ticket: "ab".repeat(32),
                npub: molt_net::nostr_identity(b"test-founder-entropy", "self-ticket").1,
                relays: vec!["wss://no-such-relay.invalid".to_string()],
            },
        }
        .render()
        .expect("a well-formed handover renders");
        w.execute(Command::JoinStart {
            invite: link,
            member: "petra".to_string(),
        })
        .await
        .expect("a joinable link arms the wizard");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Join);
                assert_eq!(s.join.republic, "Chess Club");
                assert_eq!((s.join.rule_m, s.join.rule_n), (2, 2));
                assert!(!s.join.seed.is_empty(), "the joiner's recovery phrase is shown");
                assert_eq!(s.join.run.outcome, 2, "no shared relay → the run fails honestly");
                assert!(
                    s.join.run.log.iter().any(|l| l.contains("no relay in common")),
                    "the honest relay-gate error is in the run log: {:?}",
                    s.join.run.log
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        // cancel still clears the failed run
        w.execute(Command::JoinCancel).await.expect("cancel");
    });
}

/// A joinable link with an unreachable relay — parseable, so
/// `cmd_join_start` arms the wizard before it fails honestly.
fn joinable_link() -> String {
    crate::FoundingInvite {
        info: molt_core::InviteInfo {
            republic: "R".to_string(),
            threshold: 2,
            members: 2,
            inviter: "walter".to_string(),
            ticket: "ab".repeat(32),
        },
        handover: molt_net::invite::InviteHandoverV2 {
            seat: 0,
            ticket: "ab".repeat(32),
            npub: molt_net::nostr_identity(b"test-founder-entropy", "self-ticket").1,
            relays: vec!["wss://no-such-relay.invalid".to_string()],
        },
    }
    .render()
    .expect("a well-formed handover renders")
}

/// Petra's nostr identity for the join fixtures — a REAL derived pair,
/// so the sealed handler's sk↔anchored-pk cross-check has a genuine
/// secret to validate (the anchors must be real canonical curve points
/// anyway: `verify_sealed_roster` rejects anything else).
fn petra_nostr() -> ([u8; 32], String) {
    molt_net::nostr_identity(b"petra-entropy", "ticket-petra")
}

fn valid_sealed_roster() -> molt_core::SealedRoster {
    use molt_core::{MemberIdentity, RosterAttestation};
    let (sk_a, pk_a) = molt_storage::derive_identity_key(&[1u8; 32], "a");
    let (sk_b, pk_b) = molt_storage::derive_identity_key(&[2u8; 32], "b");
    let identities = vec![
        MemberIdentity {
            member: "founder".to_string(),
            identity_pk: pk_a,
            nostr_pk: molt_net::nostr_identity(b"founder-entropy", "ticket-f").1,
        },
        MemberIdentity {
            member: "petra".to_string(),
            identity_pk: pk_b,
            nostr_pk: petra_nostr().1,
        },
    ];
    let republic_id = molt_storage::republic_id("R", 2, 2, &identities);
    let table = molt_core::roster_canonical_bytes(&republic_id, 2, 2, &identities, "", &[], None);
    let attestations = vec![
        RosterAttestation { member: "founder".to_string(), sig: molt_storage::identity_sign(&sk_a, &table) },
        RosterAttestation { member: "petra".to_string(), sig: molt_storage::identity_sign(&sk_b, &table) },
    ];
    molt_core::SealedRoster {
        name: "R".to_string(),
        republic_id,
        rule_m: 2,
        rule_n: 2,
        roster: vec!["founder".to_string(), "petra".to_string()],
        identities,
        attestations,
        agenda: String::new(),
        relays: Vec::new(),
        features: None,
    }
}

/// An honest join failure (here: the ADR-0004 relay gate) rides the join
/// run's EXISTING failure surface (`cmd_net_join_failed`), and that
/// surface keeps its gates: a report after the run already failed is
/// dropped, not double-appended.
#[test]
fn join_fails_honestly_and_late_reports_stay_dropped() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::JoinStart { invite: joinable_link(), member: "petra".to_string() })
            .await
            .expect("start");
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_eq!(s.join.run.outcome, 2, "no shared relay → honest failure");
                assert!(s.join.run.log.iter().any(|l| l.contains("no relay in common")));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // a late failure report (any generation) is dropped — the run is
        // already settled, its log must not grow a second failure line
        w.execute(Command::NetJoinFailed { error: "boom".to_string(), generation: Some(1) })
            .await
            .expect("late");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert!(
                    !s.join.run.log.iter().any(|l| l.contains("boom")),
                    "a settled run drops late failure reports: {:?}",
                    s.join.run.log
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// The GENERATION clause of the join gates (`cmd_join_cancel` bumps the
/// generation to invalidate in-flight tasks): a report from a superseded
/// generation is dropped even while a run is LIVE, and the sealed handler
/// shares the clause — a stale-generation seal materializes nothing.
#[test]
fn join_reports_from_a_stale_generation_are_dropped_while_live() {
    let mut st = plain_state();
    st.join_generation = 2;
    assert_eq!(st.session.join.run.outcome, 0, "run starts live");
    st.cmd_net_join_failed("boom".to_string(), Some(1)).expect("stale gen");
    assert_eq!(st.session.join.run.outcome, 0, "stale-generation report ignored");
    st.cmd_net_join_failed("boom".to_string(), None).expect("no gen");
    assert_eq!(st.session.join.run.outcome, 0, "generation-less report ignored");
    st.cmd_net_join_failed("boom".to_string(), Some(2)).expect("current gen");
    assert_eq!(st.session.join.run.outcome, 2, "matching generation lands");
    assert!(st.session.join.run.log.iter().any(|l| l.contains("boom")));

    let mut st2 = plain_state();
    st2.join_generation = 2;
    let before = st2.session.workspaces.len();
    let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
    st2.cmd_net_join_sealed(sealed, String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
        .expect("stale seal");
    assert_eq!(
        st2.session.workspaces.len(),
        before,
        "a stale-generation seal materializes nothing"
    );
}

/// `NetJoinSealed` stays on the surface (dormant — N4's Nostr join task
/// re-emits it), so its materialization is pinned by arming the join
/// context DIRECTLY: `cmd_join_start` fails honestly without a transport
/// (its run settles at outcome 2, which gates the sealed handler off).
#[test]
fn join_seals_into_the_republic_from_a_valid_roster() {
    let rt = rt();
    let _guard = rt.enter();
    // a verified sealed roster materializes the republic
    let mut st = plain_state();
    st.join_generation = 1;
    st.session.join = molt_core::JoinState {
        member: "petra".to_string(),
        seed: "wombat lattice orbit".to_string(),
        ..molt_core::JoinState::default()
    };
    let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
    st.cmd_net_join_sealed(sealed, String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
        .expect("sealed");
    // sealed ≠ entered (2026-08-08): entry waits for the phrase-backup
    // confirmation — JoinFinish is the joiner's CreateFinish
    assert_ne!(st.session.screen, Screen::Main, "sealing must not auto-enter");
    assert_eq!(st.session.join.run.outcome, 1, "the run reports sealed");
    assert!(!st.session.join.sealed_id.is_empty(), "the sealed id is exposed");
    st.cmd_join_finish().expect("finish enters");
    assert_eq!(st.session.screen, Screen::Main, "entered the republic");
    assert_eq!(st.session.join, molt_core::JoinState::default(), "join reset");
    let ws = st.session.workspaces.iter().find(|ws| ws.name == "R").expect("workspace added");
    // the net label mirrors the joiner's own global anonymity setting
    // ("none" by default) — never a hardcoded "tor"
    assert_eq!(ws.net, "none", "label = the effective global setting");

    // a garbage roster fails the join rather than materialising anything
    let mut st2 = plain_state();
    st2.join_generation = 1;
    st2.session.join = molt_core::JoinState {
        member: "x".to_string(),
        ..molt_core::JoinState::default()
    };
    let before = st2.session.workspaces.len();
    st2.cmd_net_join_sealed("{".to_string(), String::new(), Vec::new(), String::new(), Vec::new(), String::new(), Some(1))
        .expect("bad");
    assert_eq!(st2.session.join.run.outcome, 2, "garbage roster fails");
    assert_eq!(st2.session.workspaces.len(), before, "nothing materialized");
}

/// N1 PIN — the secret that pairs with the FOREVER-anchored third anchor
/// is validated before it is persisted: `cmd_net_join_sealed` must
/// refuse a nostr_sk that is not 32 bytes of hex, or whose x-only public
/// key is not OUR seat's anchored `nostr_pk` — in both directions the
/// join FAILS (like the corrupt-MLS arm), because sealing a genesis
/// whose transport secret the node does not actually hold surfaces only
/// when N4's transport first uses the key, with the salting ticket long
/// dead and no re-derivation path. The matching secret persists into
/// `transport.state.nostr_sk` byte-exactly.
#[test]
fn join_sealed_validates_the_persisted_nostr_secret() {
    let rt = rt();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().expect("tmp");
    let persist_state = || {
        let (ev_tx, _keep) = broadcast::channel::<Event>(8);
        let (cmd_tx, _cmd_rx) = mpsc::channel::<Envelope>(8);
        let mut st = State::new(
            GroupConfig::demo(),
            SessionView {
                settings: molt_core::SessionSettings {
                    workspace_dir: tmp.path().display().to_string(),
                    ..molt_core::SessionSettings::default()
                },
                ..SessionView::default()
            },
            ev_tx,
            cmd_tx,
            None,
            true, // persist — the secret-lifecycle path under test
            None,
        );
        st.join_generation = 1;
        st.session.join = molt_core::JoinState {
            member: "petra".to_string(),
            seed: molt_storage::generate_seed_phrase().expect("seed"),
            ..molt_core::JoinState::default()
        };
        st
    };
    let sealed = serde_json::to_string(&valid_sealed_roster()).expect("json");
    let (petra_sk, petra_npk) = petra_nostr();

    // absent, truncated, and odd-length secrets all FAIL the join
    for bad in ["", "abcd", "ab", &hex::encode(&petra_sk[..16])] {
        let mut st = persist_state();
        st.cmd_net_join_sealed(sealed.clone(), String::new(), Vec::new(), bad.to_string(), Vec::new(), String::new(), Some(1))
            .expect("handler never errors");
        assert_eq!(
            st.session.join.run.outcome, 2,
            "a malformed nostr secret {bad:?} must fail the join"
        );
        assert!(st.active.is_none(), "nothing materialized for {bad:?}");
    }
    // a well-formed scalar that is NOT the private half of petra's
    // anchored nostr_pk fails too (the wrong-seat/wrong-derivation case)
    let (foreign_sk, _) = molt_net::nostr_identity(b"someone-else", "ticket-x");
    let mut st = persist_state();
    st.cmd_net_join_sealed(
        sealed.clone(),
        String::new(),
        Vec::new(),
        hex::encode(foreign_sk),
        Vec::new(),
        String::new(),
        Some(1),
    )
    .expect("handler never errors");
    assert_eq!(st.session.join.run.outcome, 2, "a mismatched secret must fail the join");
    assert!(st.active.is_none(), "nothing materialized for the mismatch");

    // the matching secret seals the join and persists byte-exactly
    let mut st = persist_state();
    st.cmd_net_join_sealed(
        sealed.clone(),
        String::new(),
        Vec::new(),
        hex::encode(petra_sk),
        Vec::new(),
        String::new(),
        Some(1),
    )
    .expect("handler never errors");
    // sealed (entering waits for the phrase-backup step; the secret's
    // validation + persistence is what THIS test pins)
    assert_eq!(st.session.join.run.outcome, 1, "the matching secret seals the join");
    let dir = st.active.as_ref().expect("materialized").dir.clone();
    drop(st); // release the writer + flock before reopening
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let ws = loop {
        match molt_storage::open_workspace(&dir) {
            Ok((ws, _)) => break ws,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("reopening the joined workspace: {e}"),
        }
    };
    let ts = ws.read_transport_state();
    assert_eq!(
        ts.nostr_sk.as_deref(),
        Some(&petra_sk[..]),
        "the validated secret is sealed into transport.state"
    );
    assert_eq!(
        molt_net::nostr_pk_for_sk(&petra_sk).expect("pk"),
        petra_npk,
        "…and it IS the private half of the anchored third anchor"
    );
}

/// An actionable recovery link with a bogus host — parseable, so
/// `cmd_recover_start` arms the context (generation + link + phrase)
/// before its honest no-transport failure; the injected
/// `NetRecoverSealed` then materializes against that context (the same
/// seam the two-instance tests drive).
/// R2's recover leg: the refusal must NAME the relays it diagnosed —
/// the join leg extends its run log with one line per invite relay,
/// but the recover leg dropped `refusal.detail` and served only the
/// headline. Rule 3 makes re-join the routine path for a relay change,
/// so this is the leg where the naming matters most (rule 5).
#[test]
fn a_recover_refusal_names_the_relays_it_diagnosed() {
    let rt = rt();
    let _guard = rt.enter();
    let tmp = tempfile::tempdir().expect("tmp");
    let (ev_tx, _keep) = broadcast::channel::<Event>(8);
    let (cmd_tx, _cmd_rx) = mpsc::channel::<Envelope>(8);
    let mut st = State::new(
        GroupConfig::demo(),
        SessionView {
            settings: molt_core::SessionSettings {
                workspace_dir: tmp.path().display().to_string(),
                ..molt_core::SessionSettings::default()
            },
            ..SessionView::default()
        },
        ev_tx,
        cmd_tx,
        None,
        true, // persist — the recover path needs storage to recover into
        None,
    );
    let link = crate::recovery::RecoveryInvite {
        republic: "Guild".to_string(),
        member: "bob".to_string(),
        ticket: "ab".repeat(32),
        server: String::new(),
        queue_id: String::new(),
        wrap: String::new(),
        republic_id: "f00d".to_string(),
        handover: Some(molt_net::invite::RecoveryHandoverV2 {
            ticket: "ab".repeat(32),
            npub: "12".repeat(32),
            relays: vec!["wss://coordinator.example".to_string()],
            republic_id: "f00d".to_string(),
            identity_pk: String::new(),
        }),
    }
    .render();
    st.cmd_recover_start(link, "brave mountain".to_string()).expect("acked");
    let notice = st.session.notice.clone();
    assert!(notice.starts_with("recover-failed:"), "{notice}");
    assert!(
        notice.contains("coordinator.example"),
        "the refusal names the relay the operator must add: {notice}"
    );
}

/// `gui_over_mcp.md` steps 1+4, the engine half: the window's publish
/// is readable back verbatim, an action without a window is REFUSED
/// (nothing could perform it — a silent ack would read as "clicked"),
/// and with a window it is announced on the event stream the live
/// mirror consumes.
#[test]
fn the_ui_snapshot_roundtrips_and_actions_are_announced() {
    let mut st = plain_state();
    let mut ev = st.subscribe_events();
    // no window yet: read answers None, an action refuses honestly
    assert!(matches!(
        st.handle(Command::ReadUiState),
        Ok(Reply::UiState { snapshot: None })
    ));
    assert!(st
        .cmd_ui_action(molt_core::UiAction {
            verb: "select_view".to_string(),
            args: serde_json::json!({ "surface": "chat", "view": "today" }),
        })
        .is_err());
    // the window publishes; the claim reads back verbatim
    let snap = molt_core::UiSnapshot {
        screen: "main".to_string(),
        surface: "chat".to_string(),
        view: "today".to_string(),
        chat_rows: 3,
        chat_in_view: true,
        generation: 7,
        ..molt_core::UiSnapshot::default()
    };
    st.handle(Command::UiPublish { snapshot: snap.clone() })
        .expect("publish acks");
    match st.handle(Command::ReadUiState) {
        Ok(Reply::UiState { snapshot: Some(got) }) => assert_eq!(got, snap),
        other => panic!("unexpected: {other:?}"),
    }
    // …and the action is announced for the mirror
    st.cmd_ui_action(molt_core::UiAction {
        verb: "chat_send".to_string(),
        args: serde_json::json!({ "body": "hi" }),
    })
    .expect("a live window performs it");
    let mut seen = false;
    while let Ok(e) = ev.try_recv() {
        if let Event::UiActionRequested { action } = e {
            assert_eq!(action.verb, "chat_send");
            seen = true;
        }
    }
    assert!(seen, "the mirror's event carries the verb");
}

#[test]
fn recover_start_guards_the_link_and_the_storage() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        // a bare preview link carries no transport handover — not actionable
        let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
        let err = w
            .execute(Command::RecoverStart {
                link: "molt://recover/Guild/bob/abcdef".to_string(),
                phrase: "some phrase".to_string(),
            })
            .await
            .expect_err("a preview link cannot start a recovery");
        assert!(matches!(err, MoltError::Recover(_)), "unexpected: {err:?}");

        // a storage-less node has nowhere to materialize the recovery
        let w2 = spawn(GroupConfig::demo(), SessionView::default());
        let err = w2
            .execute(Command::RecoverStart {
                link: recover_link("bob", "f00d"),
                phrase: "some phrase".to_string(),
            })
            .await
            .expect_err("a storage-less node cannot recover");
        assert!(matches!(err, MoltError::Recover(_)), "unexpected: {err:?}");
    });
}

/// **The A2 keystone:** a completed rejoin materializes the recovered
/// workspace from the verified chain — adopting the FULL chain (block 1's
/// gated `Applied` projects into the surface state), anchoring the seat's
/// phrase-derived identity, and entering the republic.
#[test]
fn recovery_materializes_the_workspace_from_the_full_verified_chain() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");
        let (chain, republic_id) = recovered_chain(&phrase);
        w.execute(Command::RecoverStart {
            link: recover_link("bob", &republic_id),
            phrase: phrase.clone(),
        })
        .await
        .expect("recover start");
        // the production path fails honestly (no transport in this build)
        // on the recovery notice channel — the armed context survives it
        let s = read_session(&w).await;
        assert_eq!(
            s.notice,
            format!("recover-failed:{}", crate::LEGACY_RECOVERY_LINK),
            "the honest N-demo gap error rides the recovery notice"
        );

        // a stale-generation result is dropped without a trace
        let chain_json = serde_json::to_string(&chain).expect("chain json");
        w.execute(Command::NetRecoverSealed {
            member: "bob".to_string(),
            chain: chain_json.clone(),
            mls: String::new(),
            mesh: Vec::new(),
            nostr_sk: String::new(),
            rotation_seed: String::new(),
            generation: Some(999),
        })
        .await
        .expect("stale sealed");
        let s = read_session(&w).await;
        assert!(s.workspaces.is_empty(), "a stale result must not materialize");

        // the current-generation result materializes the workspace
        w.execute(Command::NetRecoverSealed {
            member: "bob".to_string(),
            chain: chain_json,
            mls: String::new(),
            mesh: Vec::new(),
            nostr_sk: String::new(),
            rotation_seed: String::new(),
            generation: Some(1),
        })
        .await
        .expect("sealed");
        let s = read_session(&w).await;
        assert_eq!(s.screen, Screen::Main, "entered the recovered republic");
        let ws = s
            .workspaces
            .iter()
            .find(|x| x.name == "Guild")
            .expect("the recovered workspace is listed");
        assert_eq!(s.active_workspace, ws.id);
        assert_eq!(ws.agenda, "survive total loss");
        // the FULL chain was adopted, not just the genesis: block 1's
        // gated Applied projects into the surface state
        let mem = read_surface(&w, Surface::Memory).await;
        assert_eq!(mem.applied.len(), 1, "block 1 projected");
        assert_eq!(mem.applied[0]["title"], "survived the loss");
    });
}

/// **N4b step 6d: a recovered workspace comes back as a NOSTR workspace.**
///
/// Recovery used to materialize `TransportShape::default()` and a `None`
/// transport secret — the legacy queue shape — so a seat that recovered
/// into a Nostr republic came back unable to speak to it at all.
///
/// The three things it must hold, and where each honestly comes from:
///
/// - the **new** transport anchor's private half, re-derived on the actor
///   from `(phrase, recovery ticket)`. Not checked against the seat's
///   roster entry, which is the DEAD founding anchor — the returning seat
///   is re-anchored by its own `Restored` block, and comparing against
///   the roster would either always fail or, once "fixed" by deleting the
///   comparison, accept any key at all.
/// - the **chain-ratified** relay pool, from the verified chain rather
///   than from whatever the rejoin task reported. The pool is governed
///   (roster-v4), so the chain is the authority and the Welcome's copy is
///   only a hint.
/// - the group's rotation seed, which only the Welcome can supply.
#[test]
fn recovery_materializes_the_nostr_shape_and_the_new_anchor() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let ticket = "ab".repeat(8);
    // the anchor the rejoiner proves it holds: ticket-salted with THIS
    // recovery's ticket, exactly as the request signed it
    let entropy = molt_storage::seed_entropy(&phrase).expect("entropy");
    let (new_sk, new_pk) = molt_net::nostr_identity(&entropy, &ticket);
    let pool = vec!["wss://relay.one.example/".to_string()];
    let (chain, republic_id) = recovered_chain_with(&phrase, pool.clone(), Some(new_pk.clone()));

    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        hex::encode(new_sk),
        "5a".repeat(32),
        Some(1),
    )
    .expect("the handler never errors");
    assert_eq!(
        st.session.screen,
        Screen::Main,
        "the recovery must enter the republic; notice = {:?}",
        st.session.notice
    );
    let dir = st.active.as_ref().expect("materialized").dir.clone();
    drop(st); // release the writer + flock before reopening

    let ts = reopen(&dir).read_transport_state();
    assert_eq!(
        ts.kind,
        Some(molt_core::TransportKind::Nostr),
        "a recovered Nostr seat must not come back as a queue workspace"
    );
    assert_eq!(ts.relays, pool, "the CHAIN-ratified pool, not the task's copy");
    assert_eq!(
        ts.rotation_seed.as_deref(),
        Some(&[0x5au8; 32][..]),
        "without the h-tag seed the seat can neither publish nor subscribe"
    );
    let sk = ts.nostr_sk.as_ref().expect("the new transport secret is sealed");
    assert_eq!(
        molt_net::nostr_pk_for_sk(sk).expect("valid scalar"),
        new_pk,
        "the sealed secret must be the private half of the anchor the Restored \
         block put in the chain"
    );
}

/// B2 step 1 — the read cursor is the seat's own and PERSISTED: mark a
/// channel read, close the workspace, reopen it from the same storage —
/// the cursor is still where it was, and only what arrived after it
/// counts unread. (In-memory ledgers presented the whole history as
/// unread on every restart; an MCP agent could not see "what is new"
/// at all.)
#[test]
fn a_read_cursor_survives_a_restart() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let (chain, republic_id) = recovered_chain_with(&phrase, Vec::new(), None);
    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        String::new(),
        String::new(),
        Some(1),
    )
    .expect("materialize");
    let id = st.session.active_workspace.clone();
    // from a PEER, deliberately: unread means "what somebody else said
    // while I was away". This seat's own words are read by definition
    // (`chat_msg_unread`), so posting as "bob" here would prove nothing
    // about the cursor - both would be read whatever it pointed at.
    let a = st
        .post_message("alice".to_string(), "first".to_string(), None, molt_core::ChannelRef::Group)
        .expect("post a");
    st.post_message("alice".to_string(), "second".to_string(), None, molt_core::ChannelRef::Group)
        .expect("post b");
    st.cmd_mark_channel_read(molt_core::ChannelRef::Group, hex::encode(a.0))
        .expect("mark a read");
    drop(st); // close: the writer flushes prefs + log and releases LOCK

    // a FRESH engine over the same storage
    let (ev_tx, _keep) = broadcast::channel::<Event>(8);
    let (cmd_tx, _cmd_rx) = mpsc::channel::<Envelope>(8);
    let mut st2 = State::new(
        GroupConfig::demo(),
        SessionView {
            workspaces: molt_storage::scan_workspaces(tmp.path())
                .iter()
                .map(molt_storage::ScanEntry::info)
                .collect(),
            settings: molt_core::SessionSettings {
                workspace_dir: tmp.path().display().to_string(),
                ..molt_core::SessionSettings::default()
            },
            ..SessionView::default()
        },
        ev_tx,
        cmd_tx,
        None,
        true,
        None,
    );
    // the reopen may race the closing writer's LOCK release
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match st2.cmd_open_workspace(id.clone()) {
            Ok(_) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("reopening: {e:?}"),
        }
    }
    assert!(
        !st2.read_cursors.is_empty(),
        "the cursor came back from prefs.toml, not from RAM"
    );
    let unread: Vec<String> =
        st2.chat_visible_in(Some("unread")).map(|m| m.body.clone()).collect();
    assert_eq!(
        unread,
        vec!["second"],
        "the restart kept 'read through first' - not an unread wall, \
         and not silently all-read either"
    );
}

/// The rejoiner's status line (`NetRecoverNote`): a live incarnation's
/// note reaches the notice channel; a stale one says nothing (a
/// restarted recovery must not have the old task talking over it).
#[test]
fn a_recover_note_speaks_only_for_the_live_incarnation() {
    let mut st = tests::plain_state();
    st.recover_generation = 3;
    st.cmd_net_recover_note("waiting".to_string(), Some(2)).expect("ack");
    assert!(
        !st.session.notice.starts_with("recover-note:"),
        "a stale incarnation says nothing: {:?}",
        st.session.notice
    );
    st.cmd_net_recover_note(
        "waiting for the coordinator's Welcome (2 min)".to_string(),
        Some(3),
    )
    .expect("ack");
    assert_eq!(
        st.session.notice,
        "recover-note:waiting for the coordinator's Welcome (2 min)"
    );
}

/// The stuck-epoch self-heal is CAPPED and SPACED (`detached_reattach.md`
/// §2.4): a failed spawn still stamps the clock (no per-health-frame
/// retry), the session cap survives resets, and a running recovery task
/// is never stacked onto.
#[test]
fn the_self_heal_reattach_is_capped_and_spaced() {
    let mut st = tests::plain_state();
    // no chain, no seed — the spawn cannot start, but the clock stamps
    st.maybe_self_heal_reattach();
    assert_eq!(st.reattach_attempts, 0, "a spawn that cannot start counts no attempt");
    let first = st.last_reattach.expect("the try is stamped");
    // immediately again: inside the spacing window nothing happens
    st.maybe_self_heal_reattach();
    assert_eq!(st.last_reattach, Some(first), "spaced - no re-stamp inside the window");
    // at the session cap nothing ever fires again, even past the window
    st.reattach_attempts = 3;
    st.last_reattach = None;
    st.maybe_self_heal_reattach();
    assert_eq!(st.last_reattach, None, "the session cap is final (anti ping-pong)");
}

/// The rejoiner's checklist (`NetRecoverProgress`): a live incarnation's
/// report becomes the session's `RecoverState` with per-seat approval
/// flags; a stale incarnation's report is dropped like a stale note.
#[test]
fn a_recover_progress_builds_the_checklist_for_the_live_incarnation() {
    let mut st = tests::plain_state();
    st.recover_generation = 3;
    let roster = vec!["petra".to_string(), "vera".to_string(), "walter".to_string()];
    let approved = vec!["petra".to_string(), "walter".to_string()];
    st.cmd_net_recover_progress("petra".to_string(), 3, roster.clone(), approved.clone(), Some(2))
        .expect("ack");
    assert_eq!(
        st.session.recover,
        molt_core::RecoverState::default(),
        "a stale incarnation says nothing"
    );
    st.cmd_net_recover_progress("petra".to_string(), 3, roster, approved, Some(3))
        .expect("ack");
    let r = &st.session.recover;
    assert_eq!((r.member.as_str(), r.need), ("petra", 3));
    let flags: Vec<(&str, bool)> =
        r.seats.iter().map(|s| (s.member.as_str(), s.approved)).collect();
    assert_eq!(
        flags,
        vec![("petra", true), ("vera", false), ("walter", true)],
        "roster order, per-seat approval"
    );
}

/// The counter-case, and the reason the check cannot simply be deleted: a
/// rejoin task delivering a secret that is NOT this recovery's derived key
/// fails the recovery instead of sealing a workspace whose transport key
/// nobody addresses.
#[test]
fn recovery_refuses_a_transport_secret_that_is_not_the_seats_own() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let entropy = molt_storage::seed_entropy(&phrase).expect("entropy");
    let (_, new_pk) = molt_net::nostr_identity(&entropy, &"ab".repeat(8));
    let (chain, republic_id) = recovered_chain_with(
        &phrase,
        vec!["wss://relay.one.example/".to_string()],
        Some(new_pk),
    );
    let (foreign_sk, _) = molt_net::nostr_identity(b"someone-else-entirely", "other");

    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        hex::encode(foreign_sk),
        "5a".repeat(32),
        Some(1),
    )
    .expect("the handler never errors");
    assert!(st.active.is_none(), "a foreign transport secret must not materialize");
    assert!(
        st.session.notice.starts_with("recover-failed:"),
        "the failure surfaces to the operator; notice = {:?}",
        st.session.notice
    );
}

/// **A context switch abandons an in-flight recovery.**
///
/// `cmd_recover_start` has always invalidated the join; the reverse
/// direction was missing and cost nothing while a recovery spawned
/// nothing. Since 6e it holds a 1059 inbox and a 445 subscription for up
/// to fifteen minutes, so a forgotten one sits on relay sockets long
/// after the human moved on — and its late result would materialize a
/// republic into whatever context replaced it.
///
/// The generation bump is the observable half (the socket release rides
/// with it): a result from the abandoned incarnation must land nowhere.
#[test]
fn a_context_switch_abandons_an_in_flight_recovery() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let (chain, republic_id) = recovered_chain(&phrase);
    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    assert!(st.recover_ctx.is_some(), "precondition: the context is armed");

    // starting a founding is a context switch like any other. (A join
    // start that is REFUSED — an unparseable link — deliberately is not
    // one: nothing replaced the recovery, so nothing should abandon it.)
    let _ = st.cmd_create_start(
        "Other Republic".to_string(),
        "bob".to_string(),
        2,
        2,
        Vec::new(),
    );
    assert!(st.recover_ctx.is_none(), "the abandoned recovery kept its context");

    // …and the abandoned incarnation's result lands nowhere
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        String::new(),
        String::new(),
        Some(1),
    )
    .expect("the handler never errors");
    assert!(
        st.active.is_none(),
        "a superseded recovery must not materialize a republic into the new context"
    );
}

/// A coordinator that re-admits the seat under SOMEBODY ELSE'S transport
/// anchor is refused — the one thing the served chain can still say wrong
/// about our own key.
///
/// (It usually says nothing: a Nostr coordinator serves the chain ANCHOR,
/// and this seat's `Restored` block is at the head, arriving later over
/// catch-up. So the check is "if it speaks, it must agree" — demanding a
/// re-anchor here would refuse every real recovery, which is exactly what
/// the step-6 capstone caught.)
#[test]
fn recovery_refuses_a_chain_that_re_anchors_the_seat_elsewhere() {
    let tmp = tempfile::tempdir().expect("tmp");
    let phrase = molt_storage::generate_seed_phrase().expect("phrase");
    let entropy = molt_storage::seed_entropy(&phrase).expect("entropy");
    let (ours, _) = molt_net::nostr_identity(&entropy, &"ab".repeat(8));
    // the chain re-anchors bob to a key that is NOT this recovery's
    let (_, hostile_pk) = molt_net::nostr_identity(b"a-key-we-do-not-hold", "elsewhere");
    let (chain, republic_id) = recovered_chain_with(
        &phrase,
        vec!["wss://relay.one.example/".to_string()],
        Some(hostile_pk),
    );

    let mut st = recovering_state(&tmp, "bob", &republic_id, &phrase);
    st.cmd_net_recover_sealed(
        "bob".to_string(),
        serde_json::to_string(&chain).expect("chain json"),
        String::new(),
        Vec::new(),
        hex::encode(ours),
        "5a".repeat(32),
        Some(1),
    )
    .expect("the handler never errors");
    assert!(st.active.is_none(), "a seat re-anchored elsewhere must not materialize");
    assert!(
        st.session.notice.contains("different transport key"),
        "the refusal must name what is wrong; notice = {:?}",
        st.session.notice
    );
}

/// Defence in depth on the actor: a chain whose roster does not anchor the
/// identity derived from THIS recovery's phrase is hard-rejected — a forged
/// internal command (or a coordinator serving someone else's chain) must
/// not materialize a workspace the seat cannot sign for.
#[test]
fn recovery_hard_rejects_a_chain_that_does_not_anchor_the_phrase() {
    let tmp = tempfile::tempdir().expect("tmp");
    rt().block_on(async {
        let w = spawn_with_storage(GroupConfig::demo(), storage_session(&tmp));
        // the chain anchors an identity derived from a DIFFERENT phrase
        let other = molt_storage::generate_seed_phrase().expect("other phrase");
        let (chain, republic_id) = recovered_chain(&other);
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");
        w.execute(Command::RecoverStart {
            link: recover_link("bob", &republic_id),
            phrase,
        })
        .await
        .expect("recover start");
        w.execute(Command::NetRecoverSealed {
            member: "bob".to_string(),
            chain: serde_json::to_string(&chain).expect("chain json"),
            mls: String::new(),
            mesh: Vec::new(),
            nostr_sk: String::new(),
            rotation_seed: String::new(),
            generation: Some(1),
        })
        .await
        .expect("sealed");
        let s = read_session(&w).await;
        assert!(s.workspaces.is_empty(), "an unanchored chain must not materialize");
        assert_ne!(s.screen, Screen::Main);
        assert!(
            s.notice.starts_with("recover-failed:"),
            "the failure surfaces to the operator; notice = {:?}",
            s.notice
        );
    });
}

#[test]
fn leaving_the_create_screen_abandons_an_in_flight_founding() {
    rt().block_on(async {
        // manual seam: the ritual opens but no member joins, so it stays
        // open (it cannot seal and hijack the session behind our back)
        let (w, _material_rx) =
            __spawn_manual_founding(GroupConfig::demo(), SessionView::default());
        w.execute(Command::CreateStart {
            name: "Duet".to_string(),
            member: "founder".to_string(),
            threshold: 2,
            members: 2,
            relays: Vec::new(),
        })
        .await
        .expect("start");
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_ne!(s.create, molt_core::CreateState::default(), "founding is open")
            }
            other => panic!("unexpected: {other:?}"),
        }
        // navigating away abandons it (the session is in-memory)
        w.execute(Command::Navigate { screen: Screen::Choice }).await.expect("nav");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Choice);
                assert_eq!(s.create, molt_core::CreateState::default(), "founding abandoned");
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

#[test]
fn select_view_is_validated_shared_state() {
    rt().block_on(async {
        // Memory: enabled by the legacy feature baseline, so navigation
        // reaches the view validation (a disabled surface is refused a
        // step earlier — pinned in the D7 gate test)
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::SelectView {
            surface: Surface::Memory,
            // "archive" left the memory vocabulary with the design
            // mock (shared_memory_real.md WP-E) — denied is real
            view: "denied".to_string(),
        })
        .await
        .expect("select");
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => {
                assert_eq!(s.surface, Surface::Memory);
                assert_eq!(s.view, "denied");
            }
            other => panic!("unexpected: {other:?}"),
        }
        // a view that belongs to another surface is rejected
        assert!(matches!(
            w.execute(Command::SelectView {
                surface: Surface::Chat,
                view: "balance".to_string(),
            })
            .await,
            Err(MoltError::UnknownView(..))
        ));
        // a plain surface select falls back to that surface's default view
        w.execute(Command::SelectSurface {
            surface: Surface::Memory,
        })
        .await
        .expect("select2");
        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => assert_eq!(s.view, "brain"),
            other => panic!("unexpected: {other:?}"),
        }
    });
}

#[test]
fn session_navigate_and_save_are_co_equal_state() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let mut ev = w.subscribe();

        // Initial session is the choice screen.
        match w.execute(Command::ReadSession).await.expect("read") {
            Reply::Session(s) => assert_eq!(s.screen, Screen::Choice),
            other => panic!("unexpected: {other:?}"),
        }

        // Navigating emits SessionChanged and moves the shared screen.
        w.execute(Command::Navigate {
            screen: Screen::Settings,
        })
        .await
        .expect("navigate");
        assert!(matches!(
            ev.recv().await,
            Ok(Event::SessionChanged {
                scope: SessionScope::Full
            })
        ));

        // A mock save records the values and raises the "saved" notice.
        let settings = SessionSettings {
            anonymity: "tor".to_string(),
            ..SessionSettings::default()
        };
        w.execute(Command::SetNodePosture {
            posture: molt_core::NodePosture::of(&settings),
        })
        .await
        .expect("posture");
        w.execute(Command::SaveSettings {
            settings: settings.clone(),
        })
        .await
        .expect("save");

        match w.execute(Command::ReadSession).await.expect("read2") {
            Reply::Session(s) => {
                assert_eq!(s.screen, Screen::Settings);
                assert_eq!(s.settings.anonymity, "tor");
                assert_eq!(s.notice, "saved");
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}
