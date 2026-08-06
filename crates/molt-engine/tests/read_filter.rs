// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Chat-bus B2: the channel-filtered read and the channel enumeration
//! (design pin P7 — the filter and the list ride `Command::ReadState`, no
//! new command). The filter is exact [`ChannelRef`] equality (Topic names
//! compare by exact string, P3), the enumeration always lists every
//! channel in the log (`Group` even when empty), and `Status` counts are
//! computed over the full log regardless of any filtered read.

use molt_core::{
    ChannelInfo, ChannelRef, ChatMessage, Command, GroupConfig, MessageId, ProposalId, Reply,
    SessionView, Surface, SurfaceSnapshot,
};
use molt_engine::WalletHandle;
use serde_json::Value;

/// A single-member group: no demo peers, so no brain ever injects a
/// message and every count below is exact.
fn solo() -> GroupConfig {
    GroupConfig {
        member: "me".to_string(),
        members: vec!["me".to_string()],
        threshold: 1,
        self_cosign: false,
    }
}

fn spawn_solo() -> WalletHandle {
    molt_engine::spawn(solo(), SessionView::default())
}

async fn chat(w: &WalletHandle, body: &str, channel: ChannelRef) {
    w.execute(Command::Chat {
        body: body.to_string(),
        quote: None,
        channel,
    })
    .await
    .expect("chat");
}

async fn read_chat_snapshot(w: &WalletHandle, channel: Option<ChannelRef>) -> SurfaceSnapshot {
    match w
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel,
            view: None,
        })
        .await
        .expect("read state")
    {
        Reply::State(s) => s,
        other => panic!("unexpected reply: {other:?}"),
    }
}

/// The chat is ONE window: the General ("today") view and an unfiltered
/// read return the same messages, the channel filter composes with it, the
/// agent-facing "unread" slice is still accepted, and an unknown view key
/// is an error rather than a silently wrong window.
///
/// It used to be a time AXIS — General held the young half of the retention
/// window, Archive the old half — which is why a conversation older than
/// 3.5 days looked deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_filter_rides_read_state() {
    let w = spawn_solo();
    chat(&w, "fresh", ChannelRef::Group).await;
    let read = |channel: Option<ChannelRef>, view: &str| {
        let w = w.clone();
        let view = view.to_string();
        async move {
            match w
                .execute(Command::ReadState {
                    surface: Surface::Chat,
                    channel,
                    view: Some(view),
                })
                .await
                .expect("read state")
            {
                Reply::State(s) => s,
                other => panic!("unexpected reply: {other:?}"),
            }
        }
    };
    assert_eq!(
        read(None, "today").await.applied.len(),
        1,
        "a just-sent message is in the General view"
    );
    assert_eq!(
        read(None, "today").await.applied,
        read_chat_snapshot(&w, None).await.applied,
        "…and the General view IS the unfiltered window"
    );
    // channel and view compose
    assert_eq!(read(Some(ChannelRef::Group), "today").await.applied.len(), 1);
    // the enumeration stays unfiltered either way
    assert_eq!(
        read(None, "today").await.channels,
        read_chat_snapshot(&w, None).await.channels
    );
    // nothing is filed away any more, so no archive is ever flagged
    assert!(!read(None, "today").await.has_archive);
    assert!(!read_chat_snapshot(&w, None).await.has_archive);
    // the agent slice is a READ axis, not a nav view — still accepted
    w.execute(Command::ReadState {
        surface: Surface::Chat,
        channel: None,
        view: Some("unread".to_string()),
    })
    .await
    .expect("the unread slice is a valid read");
    // …but the retired Archive view is not a key any more
    for bad in ["archive", "yesterday"] {
        let err = w
            .execute(Command::ReadState {
                surface: Surface::Chat,
                channel: None,
                view: Some(bad.to_string()),
            })
            .await
            .expect_err("an unknown view key must be refused");
        assert!(
            format!("{err:?}").contains(bad),
            "the error names the bad key: {err:?}"
        );
    }
}

/// Parse one applied-log value back into the typed message (also proves
/// the wire value round-trips — filtered rows keep their embedded ids).
fn as_message(v: &Value) -> ChatMessage {
    serde_json::from_value(v.clone()).expect("an applied chat value decodes as ChatMessage")
}

fn info_for<'a>(channels: &'a [ChannelInfo], c: &ChannelRef) -> &'a ChannelInfo {
    channels
        .iter()
        .find(|i| &i.channel == c)
        .unwrap_or_else(|| panic!("channel {c:?} missing from the enumeration"))
}

/// P7 acceptance: for every channel present in a mixed log (plus one that
/// is absent), the engine-side filtered read returns exactly what a client
/// would get by filtering the full snapshot itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filtered_read_equals_client_side_filter_of_full_read() {
    let w = spawn_solo();
    let patch7 = ChannelRef::Patch { id: ProposalId(7) };
    let patch9 = ChannelRef::Patch { id: ProposalId(9) };
    let budget = ChannelRef::Topic {
        name: "budget".to_string(),
    };
    // Topic equality is exact-string (P3): "Budget" is a different channel.
    let budget_upper = ChannelRef::Topic {
        name: "Budget".to_string(),
    };

    chat(&w, "hello all", ChannelRef::Group).await;
    chat(&w, "on the patch", patch7.clone()).await;
    chat(&w, "numbers", budget.clone()).await;
    chat(&w, "hello again", ChannelRef::Group).await;
    chat(&w, "other patch", patch9.clone()).await;
    chat(&w, "case matters", budget_upper.clone()).await;
    chat(&w, "more numbers", budget.clone()).await;

    let full = read_chat_snapshot(&w, None).await;
    assert_eq!(full.applied.len(), 7, "the unfiltered read is the whole log");

    let present = [
        ChannelRef::Group,
        patch7,
        patch9,
        budget,
        budget_upper,
    ];
    let absent = ChannelRef::Topic {
        name: "nobody-here".to_string(),
    };
    for c in present.iter().chain(std::iter::once(&absent)) {
        let filtered = read_chat_snapshot(&w, Some(c.clone())).await;
        let expected: Vec<Value> = full
            .applied
            .iter()
            .filter(|v| &as_message(v).channel == c)
            .cloned()
            .collect();
        assert_eq!(
            filtered.applied, expected,
            "filtered read for {c:?} must equal the client-side filter"
        );
        // index-into-applied is dead: every filtered row keeps its own id
        for v in &filtered.applied {
            assert_ne!(
                as_message(v).id,
                MessageId::NIL,
                "a filtered row keeps its embedded id"
            );
        }
        // the enumeration is never filtered
        assert_eq!(
            filtered.channels, full.channels,
            "a filtered read still lists every channel"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channels_enumerates_distinct_refs_with_counts_and_group_is_always_present() {
    let w = spawn_solo();

    // an empty log still lists Group (and only Group)
    let empty = read_chat_snapshot(&w, None).await;
    assert_eq!(
        empty.channels,
        vec![ChannelInfo {
            channel: ChannelRef::Group,
            count: 0,
            last_ts: 0,
            state: None,
            unread: 0,
        }],
        "Group is present (and empty) before any message"
    );

    let alpha = ChannelRef::Topic {
        name: "alpha".to_string(),
    };
    let patch3 = ChannelRef::Patch { id: ProposalId(3) };

    // first message files under a topic — Group stays listed, empty
    chat(&w, "topic first", alpha.clone()).await;
    let snap = read_chat_snapshot(&w, None).await;
    assert_eq!(info_for(&snap.channels, &ChannelRef::Group).count, 0);
    assert_eq!(info_for(&snap.channels, &alpha).count, 1);

    chat(&w, "group one", ChannelRef::Group).await;
    chat(&w, "group two", ChannelRef::Group).await;
    chat(&w, "alpha again", alpha.clone()).await;
    chat(&w, "patch talk", patch3.clone()).await;

    let snap = read_chat_snapshot(&w, None).await;
    // deterministic order: Group first, then by first appearance in the log
    let order: Vec<ChannelRef> = snap.channels.iter().map(|i| i.channel.clone()).collect();
    assert_eq!(
        order,
        vec![ChannelRef::Group, alpha.clone(), patch3.clone()],
        "Group first, then first-appearance order"
    );

    // counts and last_ts agree with the log itself
    let msgs: Vec<ChatMessage> = snap.applied.iter().map(as_message).collect();
    for info in &snap.channels {
        let in_channel: Vec<&ChatMessage> =
            msgs.iter().filter(|m| m.channel == info.channel).collect();
        assert_eq!(info.count, in_channel.len(), "count for {:?}", info.channel);
        let want_ts = in_channel.iter().map(|m| m.ts).max().unwrap_or(0);
        assert_eq!(info.last_ts, want_ts, "last_ts for {:?}", info.channel);
    }

    // a deleted (tombstoned) message still counts for its channel — it is
    // still a row in the log
    let group_msg_id = msgs
        .iter()
        .find(|m| m.channel == ChannelRef::Group)
        .map(|m| m.id)
        .expect("a group message exists");
    w.execute(Command::DeleteChat { id: group_msg_id })
        .await
        .expect("delete");
    let after = read_chat_snapshot(&w, None).await;
    assert_eq!(
        info_for(&after.channels, &ChannelRef::Group).count,
        2,
        "a tombstoned message still counts for its channel"
    );
}

/// Concept Q8: a file share IS a chat message, so it files under the
/// channel it was posted into like any other — the offer appears in that
/// channel's filtered read, not in the group view, and the enumeration
/// counts it for its channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_share_files_into_its_channel() {
    let w = spawn_solo();
    let papers = ChannelRef::Topic {
        name: "papers".to_string(),
    };
    chat(&w, "group hello", ChannelRef::Group).await;
    let tmp = tempfile::tempdir().expect("tmp");
    let share_path = tmp.path().join("charter.pdf");
    std::fs::write(&share_path, b"the charter").expect("write share source");
    w.execute(Command::ShareFile {
        path: share_path.display().to_string(),
        channel: papers.clone(),
    })
    .await
    .expect("share");
    // the share posts async once the off-actor hash completes — poll
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if read_chat_snapshot(&w, None).await.applied.len() >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the share message never posted"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // the share message carries the real channel, not a hardcoded Group
    let full = read_chat_snapshot(&w, None).await;
    let share = as_message(&full.applied[1]);
    assert_eq!(share.channel, papers, "the share is tagged with its channel");
    assert!(share.file.is_some(), "…and it is a file offer");

    // the topic's filtered read contains the offer …
    let filtered = read_chat_snapshot(&w, Some(papers.clone())).await;
    assert_eq!(filtered.applied.len(), 1, "the topic view holds the share");
    let offer = as_message(&filtered.applied[0]);
    assert_eq!(
        offer.file.as_ref().map(|f| f.name.as_str()),
        Some("charter.pdf"),
        "the filtered row is the file offer"
    );

    // … and the group view does NOT
    let group = read_chat_snapshot(&w, Some(ChannelRef::Group)).await;
    assert_eq!(group.applied.len(), 1, "the group view holds only the text");
    assert!(
        as_message(&group.applied[0]).file.is_none(),
        "no file offer leaks into the group view"
    );

    // the enumeration counts the share for its channel
    assert_eq!(info_for(&full.channels, &papers).count, 1);
    assert_eq!(info_for(&full.channels, &ChannelRef::Group).count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_by_unknown_patch_id_returns_empty_not_error() {
    let w = spawn_solo();
    chat(&w, "one", ChannelRef::Group).await;
    chat(&w, "two", ChannelRef::Group).await;

    let snap = read_chat_snapshot(
        &w,
        Some(ChannelRef::Patch {
            id: ProposalId(999),
        }),
    )
    .await;
    assert!(
        snap.applied.is_empty(),
        "an unknown patch channel filters to an empty log, not an error"
    );
    // the enumeration still lists what actually exists
    assert_eq!(info_for(&snap.channels, &ChannelRef::Group).count, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_counts_are_unchanged_by_filtering() {
    let w = spawn_solo();
    let topic = ChannelRef::Topic {
        name: "side".to_string(),
    };
    chat(&w, "main one", ChannelRef::Group).await;
    chat(&w, "main two", ChannelRef::Group).await;
    chat(&w, "aside", topic.clone()).await;
    // a pending proposal on a gated surface (threshold 1, no self-cosign —
    // it stays pending)
    w.execute(Command::Propose {
        surface: Surface::Memory,
        payload: serde_json::json!({ "note": "keep" }),
    })
    .await
    .expect("propose");

    let status = |r: Reply| match r {
        Reply::Status(s) => serde_json::to_value(&s.surfaces).expect("status serializes"),
        other => panic!("unexpected reply: {other:?}"),
    };

    let before = status(w.execute(Command::Status).await.expect("status"));

    // a filtered read returns the narrowed log …
    let filtered = read_chat_snapshot(&w, Some(topic)).await;
    assert_eq!(filtered.applied.len(), 1);

    // … but status keeps counting the full log, before and after
    let after = status(w.execute(Command::Status).await.expect("status"));
    assert_eq!(before, after, "filtering a read never changes status");
    let chat_stat = after
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["surface"] == serde_json::json!("chat"))
        .expect("chat stat");
    assert_eq!(
        chat_stat["applied"],
        serde_json::json!(3),
        "status counts the whole chat log, not a filter"
    );
    let memory_stat = after
        .as_array()
        .expect("array")
        .iter()
        .find(|s| s["surface"] == serde_json::json!("memory"))
        .expect("memory stat");
    assert_eq!(
        memory_stat["pending"],
        serde_json::json!(1),
        "the pending proposal is unaffected by chat reads"
    );
}

/// **The reported sequence, engine-side** (2026-08-05): open a workspace,
/// speak in the group, open a topic, speak in it — and then read the way the
/// GUI reads. Every one of those reads must still hold its messages.
///
/// Written because a user saw the chat pane go blank after exactly this, and
/// the first job is to say whether the engine or the window lost them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topic_opened_mid_conversation_loses_nothing() {
    let w = spawn_solo();
    chat(&w, "in the group", ChannelRef::Group).await;
    let topic = ChannelRef::Topic { name: "budget".to_string() };
    chat(&w, "in the topic", topic.clone()).await;

    let body_of = |s: &SurfaceSnapshot| {
        s.applied
            .iter()
            .map(|v| v["body"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
    };
    // …the way the GUI reads it: the whole window, then each filter
    assert_eq!(
        body_of(&read_chat_snapshot(&w, None).await),
        ["in the group", "in the topic"],
        "the unfiltered read holds both"
    );
    assert_eq!(
        body_of(&read_chat_snapshot(&w, Some(ChannelRef::Group)).await),
        ["in the group"],
        "the group keeps its own message when a topic exists beside it"
    );
    assert_eq!(
        body_of(&read_chat_snapshot(&w, Some(topic.clone())).await),
        ["in the topic"],
        "…and the topic holds what was written into it"
    );
    // the enumeration must OFFER the topic - a channel a client cannot see
    // is a channel the user cannot get back to
    let channels = read_chat_snapshot(&w, None).await.channels;
    assert!(
        channels.iter().any(|c| c.channel == topic),
        "the topic must appear in the channel enumeration: {channels:?}"
    );
    assert!(channels.iter().any(|c| c.channel == ChannelRef::Group));
}

/// **A seat's OWN message is not unread to itself** (B2).
///
/// Two things went wrong while it was: the channel badge counted the
/// message the operator had just written, and the agent-facing `"unread"`
/// slice handed an agent its own output back as "what is new". It also kept
/// the GUI's read-marking permanently armed - every render of a channel the
/// seat had spoken in issued a `MarkChannelRead`, whose engine event started
/// another render.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seats_own_message_is_not_unread_to_itself() {
    let w = spawn_solo();
    chat(&w, "mine", ChannelRef::Group).await;
    let topic = ChannelRef::Topic { name: "budget".to_string() };
    chat(&w, "mine too", topic.clone()).await;

    let snap = read_chat_snapshot(&w, None).await;
    for info in &snap.channels {
        assert_eq!(
            info.unread, 0,
            "a seat cannot have unread its own words: {:?}",
            info.channel
        );
    }
    // …and the agent slice says the same: nothing new here
    let unread = match w
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel: None,
            view: Some("unread".to_string()),
        })
        .await
        .expect("read state")
    {
        Reply::State(s) => s,
        other => panic!("unexpected reply: {other:?}"),
    };
    assert!(
        unread.applied.is_empty(),
        "the unread slice must not hand an agent its own output back: {:?}",
        unread.applied
    );
}
