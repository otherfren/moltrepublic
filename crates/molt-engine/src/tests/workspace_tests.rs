// SPDX-License-Identifier: GPL-3.0-or-later

//! Workspace storage over the `WalletHandle` surface: close/reopen,
//! export, at-rest sealing, backup flags, the restore lifecycle and the
//! per-seat stamps and read cursors that survive a restart.

use super::support::*;
use crate::*;
use molt_core::{demo_workspace_id, Screen, SessionSettings};
use serde_json::json;

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
