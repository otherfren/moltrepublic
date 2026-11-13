// SPDX-License-Identifier: GPL-3.0-or-later

//! Temporary threads (`docs/chat_threads_lifecycle.md`): close, reopen and
//! forget a topic thread, and its end as a whole after inactivity.

use super::support::*;
use crate::*;
use molt_core::{ChannelInfo, ChannelRef, ChatKind, ChatMessage, EventEnvelope, ThreadOp, WorkspaceEvent};
use serde_json::json;

fn topic(name: &str) -> ChannelRef {
    ChannelRef::Topic { name: name.into() }
}

fn channel_of<'a>(infos: &'a [ChannelInfo], name: &str) -> Option<&'a ChannelInfo> {
    infos.iter().find(|i| i.channel == topic(name))
}

/// A peer's message at `ts` in `channel`, applied as the log would.
fn land(st: &mut State, seq: u64, ts: u64, channel: ChannelRef, op: Option<ThreadOp>) {
    let id = molt_core::MessageId([u8::try_from(seq).expect("small seq"); 16]);
    let mut msg = ChatMessage::text(id, "peer-1", "x", ts).with_channel(channel);
    if let Some(op) = op {
        msg = msg.with_kind(ChatKind::System).with_thread_op(op);
    }
    st.apply(&EventEnvelope {
        prev_seq: 0,
        seq,
        ts,
        by: "peer-1".to_string(),
        body: WorkspaceEvent::Chat(msg),
    });
}

/// Applied `set_thread_retention` of `days` days.
fn thread_window(st: &mut State, seq: u64, days: u64) {
    let now = now_secs();
    st.apply(&EventEnvelope {
        prev_seq: 0,
        seq,
        ts: now,
        by: "me".to_string(),
        body: WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(seq),
            surface: Surface::Organization,
            payload: json!({"op": "set_thread_retention", "title": "t", "value": format!("{days} days")}),
        },
    });
    st.apply(&EventEnvelope {
        prev_seq: 0,
        seq: seq + 1,
        ts: now,
        by: "me".to_string(),
        body: WorkspaceEvent::Applied { id: molt_core::ProposalId(seq) },
    });
}

/// A thread ends as a WHOLE once its window of silence passed - every
/// message at once, the enumeration row with it - while a General message
/// of the same age stays: the general chat ages on its own window.
#[test]
fn a_thread_ends_as_a_whole_after_its_window_of_silence() {
    let mut st = plain_state();
    let now = now_secs();
    thread_window(&mut st, 100, 2);
    land(&mut st, 1, now - 3 * 86_400 - 60, topic("old"), None);
    land(&mut st, 2, now - 3 * 86_400, topic("old"), None);
    land(&mut st, 3, now - 3 * 86_400, ChannelRef::Group, None);
    land(&mut st, 4, now - 36 * 3_600, topic("live"), None);
    land(&mut st, 5, now - 3_600, topic("live"), None);
    // a pause longer than the window started this one over
    land(&mut st, 6, now - 3 * 86_400, topic("restarted"), None);
    land(&mut st, 7, now - 3_600, topic("restarted"), None);

    let snap = st.snapshot(Surface::Chat, None, None);
    assert!(channel_of(&snap.channels, "old").is_none(), "{:?}", snap.channels);
    let live = channel_of(&snap.channels, "live").expect("live thread");
    assert_eq!(live.count, 2, "an active thread keeps its older messages");
    assert_eq!(live.expires_at, now - 3_600 + 2 * 86_400);
    assert!(!live.closed);
    let restarted = channel_of(&snap.channels, "restarted").expect("restarted thread");
    assert_eq!(restarted.count, 1, "the message before the pause belongs to the ended thread");
    assert_eq!(snap.applied.len(), 4, "group + live (2) + restarted (1): {:?}", snap.applied);
    assert_eq!(st.status().thread_retention_days, 2);
}

/// A closed thread stays readable for a day after its close, then ends.
#[test]
fn a_closed_thread_ends_a_day_after_its_close() {
    let mut st = plain_state();
    let now = now_secs();
    land(&mut st, 1, now - 26 * 3_600, topic("a"), None);
    land(&mut st, 2, now - 25 * 3_600, topic("a"), Some(ThreadOp::Close));
    land(&mut st, 3, now - 3_600, topic("b"), None);
    land(&mut st, 4, now - 1_800, topic("b"), Some(ThreadOp::Close));
    let snap = st.snapshot(Surface::Chat, None, None);
    assert!(channel_of(&snap.channels, "a").is_none(), "{:?}", snap.channels);
    let b = channel_of(&snap.channels, "b").expect("b");
    assert!(b.closed);
    assert_eq!(b.closed_by, "peer-1");
    assert_eq!(b.expires_at, now - 1_800 + molt_core::THREAD_CLOSED_GRACE_SECS);
}

/// Close refuses new speech on this seat, reopen takes it again; both are
/// lines everyone sees, and both are idempotent.
#[test]
fn closing_refuses_speech_and_reopening_takes_it() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let say = |body: &str| Command::Chat {
            body: body.into(),
            quote: None,
            channel: topic("a"),
        };
        let unknown = w.execute(Command::ThreadClose { name: "a".into() }).await;
        assert!(matches!(unknown, Err(MoltError::UnknownThread(_))), "{unknown:?}");

        w.execute(say("hi")).await.expect("speech");
        w.execute(Command::ThreadClose { name: "a".into() }).await.expect("close");
        let again = w.execute(Command::ThreadClose { name: "a".into() }).await;
        assert!(matches!(again, Ok(Reply::Ack)), "closing twice is a no-op: {again:?}");
        let refused = w.execute(say("more")).await;
        assert!(matches!(refused, Err(MoltError::ThreadClosed(_))), "{refused:?}");

        let snap = read_surface(&w, Surface::Chat).await;
        let a = channel_of(&snap.channels, "a").expect("a");
        assert!(a.closed && a.closed_by == "me", "{a:?}");
        assert_eq!(a.count, 2, "the speech + the close line");
        let line = snap.applied.last().expect("close line");
        assert_eq!(line["kind"], "system");
        assert_eq!(line["thread_op"], "close");

        w.execute(Command::ThreadReopen { name: "a".into() }).await.expect("reopen");
        w.execute(say("more")).await.expect("speech after reopen");
        let snap = read_surface(&w, Surface::Chat).await;
        assert!(!channel_of(&snap.channels, "a").expect("a").closed);
    });
}

/// Forget hides the thread on THIS seat only, persists, and a newer
/// message brings the thread back with just that message.
#[test]
fn forget_hides_the_thread_here_until_a_newer_message() {
    let mut st = plain_state();
    let now = now_secs();
    land(&mut st, 1, now - 60, topic("a"), None);
    land(&mut st, 2, now - 30, topic("a"), None);
    st.cmd_thread_forget("a".into()).expect("forget");
    let snap = st.snapshot(Surface::Chat, None, None);
    assert!(channel_of(&snap.channels, "a").is_none(), "{:?}", snap.channels);
    assert!(snap.applied.is_empty());
    assert!(matches!(st.cmd_thread_forget("a".into()), Err(MoltError::UnknownThread(_))));

    land(&mut st, 3, now + 5, topic("a"), None);
    let snap = st.snapshot(Surface::Chat, None, None);
    assert_eq!(channel_of(&snap.channels, "a").expect("back").count, 1);
}

/// The compactor drops thread content physically: a forgotten thread at
/// once, an ended one a further window later; the general chat keeps its
/// own rhythm (`docs/chat_threads_lifecycle.md` §7).
#[test]
fn compaction_drops_forgotten_and_long_ended_threads() {
    let mut st = plain_state();
    let now = now_secs();
    let day = 86_400;
    land(&mut st, 1, now - 3 * day, ChannelRef::Group, None);
    land(&mut st, 2, now - 60, topic("forgotten"), None);
    land(&mut st, 3, now - 8 * day, topic("ended-recently"), None);
    land(&mut st, 4, now - 15 * day, topic("ended-long-ago"), None);
    land(&mut st, 5, now - 60, topic("live"), None);
    st.cmd_thread_forget("forgotten".into()).expect("forget");
    // a cutoff far in the past: only the thread rules can drop anything
    let (dump, dropped) = st.compact_chat_at(1, now).expect("threads drop");
    assert_eq!(dropped, 2, "{:?}", dump.chat);
    let left: Vec<&ChannelRef> = st.chat.iter().map(|m| &m.channel).collect();
    assert_eq!(
        left,
        vec![&ChannelRef::Group, &topic("ended-recently"), &topic("live")],
        "the forgotten and the long-ended thread are gone"
    );
}

fn share_in(st: &mut State, seq: u64, ts: u64, from: &str, channel: ChannelRef) -> molt_core::MessageId {
    let id = molt_core::MessageId([u8::try_from(seq).expect("small seq"); 16]);
    let mut msg = ChatMessage::text(id, from, "file", ts).with_channel(channel);
    msg.file = Some(molt_core::FileMeta {
        name: "a.txt".to_string(),
        size: 3,
        kind: "TXT".to_string(),
        modified: 1,
        available: true,
        checksum: "ab".repeat(32),
        key_b64: String::new(),
        pieces: 0,
        root: String::new(),
    });
    st.apply(&EventEnvelope { prev_seq: 0, seq, ts, by: from.to_string(), body: WorkspaceEvent::Chat(msg) });
    id
}

/// Forgetting is this seat's view: its OWN share in the forgotten thread
/// stays in the log, servable to the members who kept the thread.
#[test]
fn compaction_keeps_my_own_share_in_a_forgotten_thread() {
    let mut st = plain_state();
    let now = now_secs();
    let me = st.member();
    let mine = share_in(&mut st, 1, now - 60, &me, topic("a"));
    land(&mut st, 2, now - 30, topic("a"), None);
    st.cmd_thread_forget("a".into()).expect("forget");
    st.compact_chat_at(1, now).expect("the foreign message drops");
    assert!(st.chat_by_id(&mine).is_ok(), "my share survives the forget");
    assert!(!st.share_expired(&mine));
}

/// A share in an ENDED thread ends with it, sooner than the chat window.
#[test]
fn a_share_in_an_ended_thread_is_expired() {
    let mut st = plain_state();
    let now = now_secs();
    thread_window(&mut st, 100, 1);
    let old = share_in(&mut st, 1, now - 2 * 86_400, "peer-1", topic("a"));
    let live = share_in(&mut st, 2, now - 60, "peer-1", topic("b"));
    assert!(st.share_expired(&old), "the thread ended a day ago");
    assert!(!st.share_expired(&live));
}
