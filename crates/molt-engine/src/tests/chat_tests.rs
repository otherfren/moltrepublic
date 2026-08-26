// SPDX-License-Identifier: GPL-3.0-or-later

//! Chat, reactions, deletes, file shares and the retention window over
//! the `WalletHandle` surface.

use super::support::*;
use crate::*;
use serde_json::json;

#[test]
fn chat_is_ungated_and_propose_rejects_chat() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        assert!(matches!(
            w.execute(Command::Chat {
                body: "hi".into(),
                quote: None,
                channel: molt_core::ChannelRef::default(),
            })
            .await,
            Ok(Reply::Ack)
        ));
        let err = w
            .execute(Command::Propose {
                surface: Surface::Chat,
                payload: json!({"op":"x"}),
            })
            .await;
        assert!(matches!(err, Err(MoltError::ChatNotGated)));
    });
}

#[test]
fn chat_reactions_toggle_and_switch() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::Chat {
            body: "gm".into(),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat");
        let id = msg_id(&read_surface(&w, Surface::Chat).await.applied[0]);

        let read = |w: WalletHandle| async move {
            match w
                .execute(Command::ReadState {
                    surface: Surface::Chat,
                    channel: None,
                    view: None,
                })
                .await
                .expect("read")
            {
                Reply::State(s) => s.applied[0].clone(),
                other => panic!("unexpected: {other:?}"),
            }
        };

        // react 👍 — my name lands under that emoji
        w.execute(Command::ReactChat {
            id,
            emoji: "👍".into(),
        })
        .await
        .expect("react");
        let msg = read(w.clone()).await;
        assert_eq!(msg["reactions"]["👍"], json!(["me"]));

        // switching to 🔥 removes 👍 (one reaction per member)
        w.execute(Command::ReactChat {
            id,
            emoji: "🔥".into(),
        })
        .await
        .expect("switch");
        let msg = read(w.clone()).await;
        assert!(msg["reactions"].get("👍").is_none());
        assert_eq!(msg["reactions"]["🔥"], json!(["me"]));

        // reacting with the same emoji again un-reacts; the empty map
        // disappears from the wire entirely
        w.execute(Command::ReactChat {
            id,
            emoji: "🔥".into(),
        })
        .await
        .expect("unreact");
        let msg = read(w.clone()).await;
        assert!(msg.get("reactions").is_none());

        // an unknown message id
        let unknown = MessageId([9u8; 16]);
        assert!(matches!(
            w.execute(Command::ReactChat {
                id: unknown,
                emoji: "👍".into(),
            })
            .await,
            Err(MoltError::UnknownMessage(i)) if i == unknown
        ));
    });
}

#[test]
fn file_share_lifecycle_download_until_removed() {
    rt().block_on(async {
        use sha2::Digest as _;
        let tmp = tempfile::tempdir().expect("tmp");
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let content: &[u8] = b"the sealed charter, for real this time";
        let share_id = share_temp_file(&w, tmp.path(), "charter.pdf", content).await;

        // the chat log carries the REAL metadata the engine derived —
        // including the streamed sha256 (the download anchor)
        let snap = read_surface(&w, Surface::Chat).await;
        let f = &snap.applied[0]["file"];
        assert_eq!(f["name"], json!("charter.pdf"));
        assert_eq!(f["size"], json!(content.len()));
        assert_eq!(f["kind"], json!("PDF"));
        assert!(f["modified"].as_u64().is_some_and(|m| m > 0));
        assert_eq!(f["available"], json!(true));
        let want_sha = hex::encode(sha2::Sha256::digest(content));
        assert_eq!(f["checksum"], json!(want_sha), "the real sha256 is log-anchored");

        // downloading the OWN share is an honest local copy — and a
        // name collision resolves as "name (1).ext", never overwrites
        let dest = tmp.path().join("dl");
        std::fs::create_dir_all(&dest).expect("dest");
        w.execute(Command::DownloadFile {
            id: share_id,
            dest: Some(dest.display().to_string()),
        })
        .await
        .expect("download works while available");
        await_file(&dest.join("charter.pdf"), content).await;
        w.execute(Command::DownloadFile {
            id: share_id,
            dest: Some(dest.display().to_string()),
        })
        .await
        .expect("second download");
        await_file(&dest.join("charter (1).pdf"), content).await;

        // … the sharer removes it locally → permanently unavailable
        w.execute(Command::RemoveFile { id: share_id })
            .await
            .expect("remove own share");
        assert!(matches!(
            w.execute(Command::DownloadFile { id: share_id, dest: None }).await,
            Err(MoltError::FileUnavailable(i)) if i == share_id
        ));
        assert!(matches!(
            w.execute(Command::RemoveFile { id: share_id }).await,
            Err(MoltError::FileUnavailable(i)) if i == share_id
        ));

        // plain messages have nothing to download
        w.execute(Command::Chat {
            body: "hi".into(),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat");
        let plain_id = msg_id(
            read_surface(&w, Surface::Chat)
                .await
                .applied
                .iter()
                .find(|m| m["body"] == json!("hi"))
                .expect("plain message"),
        );
        assert!(matches!(
            w.execute(Command::DownloadFile { id: plain_id, dest: None }).await,
            Err(MoltError::NoFile(i)) if i == plain_id
        ));
        // deleting a share message drops the share entirely
        let share2_id = share_temp_file(&w, tmp.path(), "notes.md", b"notes").await;
        w.execute(Command::DeleteChat { id: share2_id })
            .await
            .expect("delete");
        assert!(matches!(
            w.execute(Command::DownloadFile { id: share2_id, dest: None }).await,
            Err(MoltError::NoFile(i)) if i == share2_id
        ));
        let unknown = MessageId([9u8; 16]);
        assert!(matches!(
            w.execute(Command::DownloadFile { id: unknown, dest: None }).await,
            Err(MoltError::UnknownMessage(i)) if i == unknown
        ));
        // sharing an unreadable path fails honestly (no share message,
        // an honest notice instead)
        w.execute(Command::ShareFile {
            path: tmp.path().join("missing.bin").display().to_string(),
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("kickoff succeeds; the failure surfaces async");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let s = read_session(&w).await;
            if s.notice.starts_with("share-failed:missing.bin") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the share failure never surfaced: {:?}",
                s.notice
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    });
}

#[test]
fn chat_delete_leaves_a_tombstone() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::Chat {
            body: "secret".into(),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat");
        let id = msg_id(&read_surface(&w, Surface::Chat).await.applied[0]);
        w.execute(Command::ReactChat {
            id,
            emoji: "🔥".into(),
        })
        .await
        .expect("react");
        w.execute(Command::DeleteChat { id })
            .await
            .expect("delete");
        match w
            .execute(Command::ReadState {
                surface: Surface::Chat,
                channel: None,
                view: None,
            })
            .await
            .expect("read")
        {
            Reply::State(s) => {
                let msg = &s.applied[0];
                assert_eq!(msg["body"], json!(""));
                assert_eq!(msg["deleted_by"], json!("me"));
                assert!(msg.get("reactions").is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
        let unknown = MessageId([9u8; 16]);
        assert!(matches!(
            w.execute(Command::DeleteChat { id: unknown }).await,
            Err(MoltError::UnknownMessage(i)) if i == unknown
        ));
    });
}

/// Chat bus Stage A pin: every chat verb addresses messages by their
/// stable id — send, react, delete, and a quote all work by id; an
/// unknown id is `UnknownMessage`.
#[test]
fn chat_commands_address_by_id() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        w.execute(Command::Chat {
            body: "root".into(),
            quote: None,
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat");
        // the demo peers may chat back mid-test — address rows by body
        let row = |snap: &molt_core::SurfaceSnapshot, body: &str| {
            snap.applied
                .iter()
                .find(|m| m["body"] == json!(body))
                .cloned()
                .unwrap_or_else(|| panic!("no chat row with body {body:?}"))
        };
        let snap = read_surface(&w, Surface::Chat).await;
        let root_id = msg_id(&row(&snap, "root"));
        assert!(!root_id.is_nil(), "a new message carries a minted id");

        // react by id
        w.execute(Command::ReactChat {
            id: root_id,
            emoji: "👍".into(),
        })
        .await
        .expect("react by id");
        let snap = read_surface(&w, Surface::Chat).await;
        assert_eq!(row(&snap, "root")["reactions"]["👍"], json!(["me"]));

        // quote by id survives in the log (as quote_id; the legacy
        // numeric quote is never written by new code)
        w.execute(Command::Chat {
            body: "reply".into(),
            quote: Some(root_id),
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("quoted reply");
        let snap = read_surface(&w, Surface::Chat).await;
        let reply = row(&snap, "reply");
        assert_eq!(
            reply["quote_id"],
            json!(root_id.to_string()),
            "the quote rides as a stable id"
        );
        assert!(
            reply.get("quote").is_none(),
            "new code never writes the legacy index quote"
        );
        let reply_id = msg_id(&reply);

        // delete by id
        w.execute(Command::DeleteChat { id: reply_id })
            .await
            .expect("delete by id");
        let snap = read_surface(&w, Surface::Chat).await;
        let tombstone = snap
            .applied
            .iter()
            .find(|m| m["id"] == json!(reply_id.to_string()))
            .expect("the deleted row remains as a tombstone");
        assert_eq!(tombstone["deleted_by"], json!("me"));
        assert_eq!(tombstone["body"], json!(""));

        // an unknown id is rejected with the id in the error
        let unknown = MessageId([7u8; 16]);
        assert!(matches!(
            w.execute(Command::ReactChat {
                id: unknown,
                emoji: "👍".into(),
            })
            .await,
            Err(MoltError::UnknownMessage(i)) if i == unknown
        ));
        assert!(matches!(
            w.execute(Command::DeleteChat { id: unknown }).await,
            Err(MoltError::UnknownMessage(i)) if i == unknown
        ));

        // a quote pointing at an unknown id is dropped, not kept dangling
        w.execute(Command::Chat {
            body: "dangling".into(),
            quote: Some(unknown),
            channel: molt_core::ChannelRef::default(),
        })
        .await
        .expect("chat with dangling quote");
        let snap = read_surface(&w, Surface::Chat).await;
        assert!(row(&snap, "dangling").get("quote_id").is_none());
    });
}

/// Chat bus Stage A pin: ids are minted per message — non-nil and
/// pairwise distinct.
#[test]
fn every_new_message_gets_a_unique_nonnil_id() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        const N: usize = 20;
        for i in 0..N {
            w.execute(Command::Chat {
                body: format!("msg {i}"),
                quote: None,
                channel: molt_core::ChannelRef::default(),
            })
            .await
            .expect("chat");
        }
        let snap = read_surface(&w, Surface::Chat).await;
        // the demo peers may have chatted back — pick out OUR messages
        let ids: Vec<MessageId> = snap
            .applied
            .iter()
            .filter(|m| m["from"] == json!("me"))
            .map(msg_id)
            .collect();
        assert_eq!(ids.len(), N);
        assert!(ids.iter().all(|id| !id.is_nil()), "no nil ids");
        let distinct: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(distinct.len(), N, "all ids are pairwise distinct");
    });
}

/// The Organization read projections behind the Members and Uploads
/// tables: every roster member with its identity anchor + governance +
/// upload counters, and every file shared into the chat with its
/// retention deadline. Read-only commands — MCP tools like every read,
/// so an agent can auto-test the same tables the GUI renders.
#[test]
fn the_share_card_carries_its_availability_word() {
    // §5.5 (file_transfer_nostr.md): ONE status word, derived — a live
    // series stamp means the relays hold it (no live sharer needed), no
    // stamp means the first download wakes the sharer, a withdrawn
    // share without a stamp is gone.
    let mut st = plain_state();
    let id = molt_core::MessageId([3u8; 16]);
    let mut msg = molt_core::ChatMessage::text(id, "me", "a share", now_secs());
    msg.file = Some(molt_core::FileMeta {
        name: "plan.pdf".to_string(),
        size: 9,
        kind: "PDF".to_string(),
        modified: 100,
        available: true,
        checksum: String::new(),
    });
    let env = st.make_env("me".to_string(), molt_core::WorkspaceEvent::Chat(msg));
    st.apply(&env);

    let word = |st: &State| st.uploads_view()[0].availability.clone();
    assert_eq!(word(&st), "sharer-only", "no stamp: the sharer must serve");
    st.file_series.insert(id, 7);
    assert_eq!(word(&st), "relay-held", "a live stamp: the relays serve");
    st.file_series.remove(&id);
    let rm = st.make_env(
        "me".to_string(),
        molt_core::WorkspaceEvent::FileRemoved { index: 0, id: Some(id), by: "me".to_string() },
    );
    st.apply(&rm);
    assert_eq!(word(&st), "gone", "withdrawn and no stamp");
}

#[test]
fn members_and_uploads_projections_serve_the_org_tables() {
    rt().block_on(async {
        let tmp = tempfile::tempdir().expect("tmp");
        let w = spawn(GroupConfig::demo(), SessionView::default());
        share_temp_file(&w, tmp.path(), "charter.pdf", b"real shared bytes").await;
        // a self-cosigned pending proposal: no longer waiting on me,
        // still waiting on both peers
        w.execute(Command::Propose {
            surface: Surface::Memory,
            payload: json!({"op":"add_note","title":"t"}),
        })
        .await
        .expect("propose");
        match w.execute(Command::ReadMembers).await.expect("members") {
            Reply::Members { members: rows } => {
                assert_eq!(rows.len(), 3, "one row per roster member");
                let me = rows.iter().find(|m| m.member == "me").expect("me");
                assert_eq!(me.uploads, 1, "the share counts as my upload");
                assert_eq!(me.open_proposals, 0, "self-cosign → not waiting on me");
                assert!(
                    me.identity_pk.is_empty() && me.id.is_empty(),
                    "a demo workspace anchors no identities"
                );
                let peer = rows.iter().find(|m| m.member == "peer-1").expect("peer");
                assert_eq!(peer.uploads, 0);
                assert_eq!(peer.open_proposals, 1, "the proposal waits on the peer");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match w.execute(Command::ReadUploads).await.expect("uploads") {
            Reply::Uploads { uploads: rows } => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].member, "me");
                assert_eq!(rows[0].name, "charter.pdf");
                assert_eq!(rows[0].kind, "PDF");
                assert!(rows[0].available);
                assert!(!rows[0].id.is_nil(), "addressable for download_file");
                assert_eq!(
                    rows[0].expires_ts,
                    rows[0].ts + 7 * 86_400,
                    "the share expires with the chat retention window (default 7 days)"
                );
                assert_eq!(
                    rows[0].checksum,
                    {
                        use sha2::Digest as _;
                        hex::encode(sha2::Sha256::digest(b"real shared bytes"))
                    },
                    "the REAL sha256 of the shared bytes, log-anchored"
                );
                assert!(
                    rows[0].online,
                    "the sharer is this node itself - always online"
                );
                assert!(
                    rows[0].download.is_none(),
                    "no download of this share is running"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// "Delete chat after N days" is engine semantics, enforced at the read
/// contract (co-equality: GUI and MCP see the same filtered snapshot):
/// chat messages older than the effective window and declined proposals
/// whose veto aged out disappear from `ReadState`; a legacy ts of 0
/// stays visible (unknown age must not silently vanish), and the
/// channel enumeration keeps covering the full log.
#[test]
fn chat_retention_filters_the_read_contract() {
    let mut st = plain_state();
    let now = now_secs();
    let stale = now - 10 * 86_400;
    let fresh = now - 3_600;
    let msg = |seq: u64, ts: u64, body: &str| molt_core::EventEnvelope { prev_seq: 0,
        seq,
        ts: if ts == 0 { now } else { ts },
        by: "peer-1".to_string(),
        body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            molt_core::MessageId([u8::try_from(seq).expect("small test seq"); 16]),
            "peer-1",
            body,
            ts,
        )),
    };
    st.apply(&msg(1, stale, "stale"));
    st.apply(&msg(2, fresh, "fresh"));
    st.apply(&msg(3, 0, "legacy"));
    let snap = st.snapshot(Surface::Chat, None, None);
    assert_eq!(
        snap.applied.len(),
        2,
        "the 10-day-old message ages out of the 7-day default window"
    );
    assert_eq!(
        snap.channels[0].count, 2,
        "channel counts agree with the retention-filtered read (the stale \
         message ages out of the count too, ts 0 stays)"
    );
    // widening the window to 30 days via an applied org change brings
    // the stale message back — the setting is REAL state
    st.apply(&molt_core::EventEnvelope { prev_seq: 0,
        seq: 4,
        ts: now,
        by: "me".to_string(),
        body: molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(1),
            surface: Surface::Organization,
            payload: json!({"op": "set_chat_retention", "title": "t", "value": "30 days"}),
        },
    });
    st.apply(&molt_core::EventEnvelope { prev_seq: 0,
        seq: 5,
        ts: now,
        by: "me".to_string(),
        body: molt_core::WorkspaceEvent::Applied { id: molt_core::ProposalId(1) },
    });
    assert_eq!(st.snapshot(Surface::Chat, None, None).applied.len(), 3);
    // declined proposals age out on the same rhythm (their veto stamp)
    st.apply(&molt_core::EventEnvelope { prev_seq: 0,
        seq: 6,
        ts: stale,
        by: "me".to_string(),
        body: molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(2),
            surface: Surface::Organization,
            payload: json!({"op": "set_name", "title": "t", "value": "abgelehnt"}),
        },
    });
    st.apply(&molt_core::EventEnvelope { prev_seq: 0,
        seq: 7,
        ts: now - 40 * 86_400,
        by: "peer-1".to_string(),
        body: molt_core::WorkspaceEvent::Declined {
            id: molt_core::ProposalId(2),
            by: "peer-1".to_string(),
            hash: String::new(),
        },
    });
    let org = st.snapshot(Surface::Organization, None, None);
    assert!(
        org.declined.is_empty(),
        "a veto older than the retention window is hidden: {:?}",
        org.declined
    );
    assert_eq!(org.denied, 0, "the denied count follows the filtered view");
}

/// Uploads are ephemeral exactly like chat: a file share is a chat
/// message, so it ages out of EVERY read surface on the same
/// `retention_days` rhythm (one knob — no separate link TTL). The
/// uploads table hides an expired share, its `expires_ts` is the real
/// retention deadline (`ts` + window; 0 = unknown age, kept forever),
/// and a download attempt of an expired share fails cleanly with
/// [`MoltError::FileExpired`] — a widened window brings both back.
#[test]
fn uploads_age_out_with_the_chat_retention_window() {
    let mut st = plain_state();
    let now = now_secs();
    let stale_ts = now - 10 * 86_400;
    let fresh_ts = now - 3_600;
    let share = |seq: u64, ts: u64, name: &str| {
        let mut m = molt_core::ChatMessage::text(
            molt_core::MessageId([u8::try_from(seq).expect("small test seq"); 16]),
            "peer-1",
            "",
            ts,
        );
        m.file = Some(molt_core::FileMeta {
            name: name.to_string(),
            size: 3,
            kind: "PDF".to_string(),
            modified: 1,
            available: true,
            checksum: String::new(),
        });
        molt_core::EventEnvelope { prev_seq: 0,
            seq,
            ts: if ts == 0 { now } else { ts },
            by: "peer-1".to_string(),
            body: molt_core::WorkspaceEvent::Chat(m),
        }
    };
    let stale_id = molt_core::MessageId([1u8; 16]);
    let legacy_id = molt_core::MessageId([3u8; 16]);
    st.apply(&share(1, stale_ts, "stale.pdf"));
    st.apply(&share(2, fresh_ts, "fresh.pdf"));
    st.apply(&share(3, 0, "legacy.pdf"));

    // the uploads table follows the chat window (default 7 days)
    let rows = st.uploads_view();
    assert_eq!(
        rows.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(),
        vec!["fresh.pdf", "legacy.pdf"],
        "the 10-day-old share ages out of the 7-day default window, ts 0 stays"
    );
    assert_eq!(
        rows[0].expires_ts,
        fresh_ts + 7 * 86_400,
        "the share expires on the retention deadline - the org window, not a mock TTL"
    );
    assert_eq!(
        rows[1].expires_ts, 0,
        "unknown age (ts 0) never ages out - 0 = no deadline"
    );

    // downloading the expired share fails cleanly, the others pass the gate
    let err = st
        .cmd_download_file(stale_id, None)
        .expect_err("an expired share must not be downloadable");
    assert!(
        matches!(err, MoltError::FileExpired(id) if id == stale_id),
        "unexpected: {err:?}"
    );
    let err = st
        .cmd_download_file(legacy_id, None)
        .expect_err("plain_state has no live engine to spawn the fetch");
    assert!(
        !matches!(err, MoltError::FileExpired(_)),
        "ts 0 passes the retention gate: {err:?}"
    );

    // widening the window to 30 days via an applied org change brings
    // the stale share back — same knob as chat, REAL state
    st.apply(&molt_core::EventEnvelope { prev_seq: 0,
        seq: 4,
        ts: now,
        by: "me".to_string(),
        body: molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(1),
            surface: Surface::Organization,
            payload: json!({"op": "set_chat_retention", "title": "t", "value": "30 days"}),
        },
    });
    st.apply(&molt_core::EventEnvelope { prev_seq: 0,
        seq: 5,
        ts: now,
        by: "me".to_string(),
        body: molt_core::WorkspaceEvent::Applied { id: molt_core::ProposalId(1) },
    });
    let rows = st.uploads_view();
    assert_eq!(rows.len(), 3, "the widened window re-exposes the stale share");
    assert_eq!(
        rows[0].expires_ts,
        stale_ts + 30 * 86_400,
        "the deadline follows the effective window"
    );
    let err = st
        .cmd_download_file(stale_id, None)
        .expect_err("plain_state has no live engine to spawn the fetch");
    assert!(
        !matches!(err, MoltError::FileExpired(_)),
        "inside the widened window the share is downloadable again: {err:?}"
    );
}

/// The retention boundary is a pure function of the message timestamp,
/// "now" and the window (explicit `now`, like the `*_label_at`
/// helpers): ONE window, no sub-split. It used to halve the window into
/// a General and an Archive view, so a conversation older than half a
/// window - 3.5 days at the default retention - silently left the chat
/// the user was looking at. A legacy ts of 0 (unknown age) never
/// vanishes.
#[test]
fn chat_view_boundary_is_the_whole_retention_window() {
    use crate::proposals::chat_view_admits;
    let now = 1_700_000_000;
    let days = 10; // window: 864 000 s
    let at = |pct: u64| now - 864_000 * pct / 100;
    // the old half-window cliff sat here: 60 % of the window old is
    // ordinary chat now, not something filed away
    for pct in [10u64, 50, 60, 99] {
        assert!(
            chat_view_admits(at(pct), now, days),
            "{pct} % of the window old must stay in the chat"
        );
    }
    // exactly 100 %: the window's oldest visible instant
    assert!(chat_view_admits(at(100), now, days));
    // past it: gone, which is what "delete chat after N days" means
    assert!(!chat_view_admits(at(110), now, days));
    // ts 0 = unknown age: always kept
    assert!(chat_view_admits(0, now, days));
}

/// `ReadState { view }` on chat is ONE window: the General view and an
/// unfiltered read return the same messages, and the only thing a view
/// key still narrows is the agent-facing `"unread"` slice. The window
/// used to be halved (General = young half, Archive = old half), which
/// made a conversation vanish from the chat at 3.5 days by default.
#[test]
fn the_chat_view_is_the_whole_retention_window() {
    let mut st = plain_state();
    let now = now_secs();
    let window = 7 * 86_400; // the default 7-day retention window
    let young = now - window * 10 / 100;
    let old = now - window * 60 / 100;
    let gone = now - window * 110 / 100;
    let msg = |seq: u64, ts: u64, body: &str| molt_core::EventEnvelope { prev_seq: 0,
        seq,
        ts: if ts == 0 { now } else { ts },
        by: "peer-1".to_string(),
        body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            molt_core::MessageId([u8::try_from(seq).expect("small test seq"); 16]),
            "peer-1",
            body,
            ts,
        )),
    };
    st.apply(&msg(1, young, "young"));
    st.apply(&msg(2, old, "old"));
    st.apply(&msg(3, gone, "gone"));
    let body_of = |v: &serde_json::Value| v["body"].as_str().expect("body").to_string();
    let today = st.snapshot(Surface::Chat, None, Some("today"));
    assert_eq!(
        today.applied.iter().map(body_of).collect::<Vec<_>>(),
        vec!["young", "old"],
        "the General view holds everything inside the window - the 60 % \
         message is ordinary chat, not something filed away"
    );
    let all = st.snapshot(Surface::Chat, None, None);
    assert_eq!(
        all.applied.iter().map(body_of).collect::<Vec<_>>(),
        vec!["young", "old"],
        "…and an unfiltered read is the same window"
    );
    assert!(!today.has_archive, "there is no second view to offer");
    // the enumeration is a whole-window concern, like with `channel`
    assert_eq!(today.channels, all.channels);
    // a legacy ts of 0 (unknown age) never vanishes
    st.apply(&msg(4, 0, "legacy"));
    assert_eq!(
        st.snapshot(Surface::Chat, None, Some("today")).applied.len(),
        3,
        "unknown age stays visible"
    );
}
