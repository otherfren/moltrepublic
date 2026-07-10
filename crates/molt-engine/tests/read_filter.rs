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
        })
        .await
        .expect("read state")
    {
        Reply::State(s) => s,
        other => panic!("unexpected reply: {other:?}"),
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
