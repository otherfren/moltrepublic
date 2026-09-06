// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! A proposal's discussion outlives its decision (2026-09-06, G2/G6): a
//! `Patch` channel stays writable after the vote applied, was declined or
//! was pulled back — a review of a decided change is not dead chat, and
//! at 2-of-3 the tipping reviewer's reason would otherwise race the
//! decision. No patch channel is refused any more; an unknown referent
//! stays writable too (chat-bus Q4). A vote carries its reasoning with
//! it: `note` posts into the channel BEFORE the vote lands.

use std::time::Duration;

use molt_core::{
    ChannelRef, Command, GroupConfig, MoltError, ProposalId, ProposalState, Reply,
    SessionSettings, SessionView, Surface, SurfaceSnapshot,
};
use molt_engine::WalletHandle;

/// A single-member group (threshold 1, no self-cosign): a proposal stays
/// `Proposed` until this node votes, one `Approve` applies it, one
/// `Decline` rejects it — every lifecycle state is reachable directly.
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

async fn chat(w: &WalletHandle, body: &str, channel: ChannelRef) -> Result<Reply, MoltError> {
    w.execute(Command::Chat {
        body: body.to_string(),
        quote: None,
        channel,
    })
    .await
}

async fn propose(w: &WalletHandle, title: &str) -> ProposalId {
    match w
        .execute(Command::Propose {
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "title": title }),
        })
        .await
        .expect("propose")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected reply: {other:?}"),
    }
}

async fn read_chat(w: &WalletHandle) -> SurfaceSnapshot {
    match w
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read state")
    {
        Reply::State(s) => s,
        other => panic!("unexpected reply: {other:?}"),
    }
}

fn patch(id: ProposalId) -> ChannelRef {
    ChannelRef::Patch { id }
}

/// The post-mortem of a REJECTED proposal has a room (G6): the channel
/// takes the review remark, and the decision summary above it says what
/// happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_into_a_declined_discussion_is_accepted() {
    let w = spawn_solo();
    let id = propose(&w, "veto me").await;
    chat(&w, "still open", patch(id)).await.expect("a Proposed discussion is writable");
    w.execute(Command::Decline { proposal: id, note: None }).await.expect("decline");
    chat(&w, "why I declined", patch(id))
        .await
        .expect("a decided vote's discussion stays writable");
    let snap = read_chat(&w).await;
    assert_eq!(
        snap.applied.len(),
        3,
        "the deliberation, the decline summary and the post-mortem"
    );
    assert_eq!(
        snap.applied.last().and_then(|m| m.get("body")).and_then(|b| b.as_str()),
        Some("why I declined"),
        "the post-mortem is the last line"
    );
}

/// The tipping reviewer's reasoning lands after the seal (G2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_into_an_applied_discussion_is_accepted() {
    let w = spawn_solo();
    let id = propose(&w, "seal me").await;
    w.execute(Command::Approve { proposal: id, note: None }).await.expect("approve to threshold");
    chat(&w, "found an error after the seal", patch(id))
        .await
        .expect("an applied vote's discussion stays writable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn share_file_into_a_decided_discussion_is_accepted() {
    let w = spawn_solo();
    let id = propose(&w, "attachments after the vote").await;
    w.execute(Command::Decline { proposal: id, note: None }).await.expect("decline");
    w.execute(Command::ShareFile {
        path: "/tmp/anything.txt".to_string(),
        channel: patch(id),
    })
    .await
    .expect("a decided discussion still takes an attachment");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proposed_discussion_stays_writable() {
    let w = spawn_solo();
    let id = propose(&w, "still deliberating").await;
    chat(&w, "opinions?", patch(id)).await.expect("an open vote's discussion accepts chat");
}

/// Chat-bus Q4: a patch ref whose proposal this node never saw keeps
/// working — a tagged message may arrive before its referent, so channels
/// never error on unknown ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_patch_discussion_stays_writable() {
    let w = spawn_solo();
    chat(&w, "referent unknown here", patch(ProposalId(999)))
        .await
        .expect("an unknown patch id stays writable (Q4)");
    w.execute(Command::ShareFile {
        path: "/tmp/anything.txt".to_string(),
        channel: patch(ProposalId(999)),
    })
    .await
    .expect("and so does a share into it");
}

/// The read side of co-equality: `ChannelInfo.state` annotates each patch
/// channel with its vote's lifecycle, so ANY frontend (GUI, MCP agent)
/// renders the decision from the same engine-side data. Group/Topic — and
/// unknown patch refs — stay `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_enumeration_annotates_the_vote_state() {
    let w = spawn_solo();
    let open = propose(&w, "open").await;
    let declined = propose(&w, "declined").await;
    let applied = propose(&w, "applied").await;
    // file a message into every channel while all votes are still open
    chat(&w, "group", ChannelRef::Group).await.expect("chat");
    chat(&w, "topic", ChannelRef::Topic { name: "side".to_string() })
        .await
        .expect("chat");
    for id in [open, declined, applied] {
        chat(&w, "deliberation", patch(id)).await.expect("chat");
    }
    chat(&w, "referent unknown", patch(ProposalId(999)))
        .await
        .expect("chat");
    // now decide two of the three votes
    w.execute(Command::Decline { proposal: declined, note: None }).await.expect("decline");
    w.execute(Command::Approve { proposal: applied, note: None }).await.expect("approve");

    let snap = read_chat(&w).await;
    let state_of = |c: &ChannelRef| {
        snap.channels
            .iter()
            .find(|i| &i.channel == c)
            .unwrap_or_else(|| panic!("channel {c:?} missing from the enumeration"))
            .state
    };
    assert_eq!(state_of(&ChannelRef::Group), None, "Group carries no vote");
    assert_eq!(
        state_of(&ChannelRef::Topic { name: "side".to_string() }),
        None,
        "a topic carries no vote"
    );
    assert_eq!(state_of(&patch(open)), Some(ProposalState::Proposed));
    assert_eq!(state_of(&patch(declined)), Some(ProposalState::Rejected));
    assert_eq!(state_of(&patch(applied)), Some(ProposalState::Applied));
    assert_eq!(
        state_of(&patch(ProposalId(999))),
        None,
        "an unknown referent stays unannotated (Q4)"
    );
}

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected reply: {other:?}"),
    }
}

async fn await_founding(w: &WalletHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(w).await;
        match s.create.run.outcome {
            1 => return,
            2 => panic!("founding failed: {:?}", s.create.run.log),
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "founding did not seal in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The lifecycle annotation reads `State.proposals`, which the log replay
/// rebuilds — the decided discussion stays open across close/reopen and
/// keeps saying what it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decided_discussion_survives_close_and_reopen() {
    let tmp = tempfile::tempdir().expect("tmp");
    let session = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("workspaces").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::__spawn_sim_founding(GroupConfig::demo(), session, true);
    // 2-of-2: ONE decline makes the threshold unreachable, so this node's
    // own decline legitimately rejects (a decline is a voice, not a veto —
    // in 2-of-3 it would leave the vote pending for the third member)
    w.execute(Command::CreateStart {
        name: "Readonly".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    // ❻½: the founder's phrase-backup confirmation (n-of-n gate)
    {
        let seed_ = read_session(&w).await.create.seed.clone();
        w.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    await_founding(&w).await;
    w.execute(Command::CreateFinish).await.expect("finish");
    let ws = read_session(&w).await.active_workspace.clone();

    let id = propose(&w, "decided before the restart").await;
    // file a message while the vote is open, so the channel exists in the
    // enumeration after the replay
    chat(&w, "deliberated", patch(id)).await.expect("chat while open");
    w.execute(Command::Decline { proposal: id, note: None }).await.expect("decline");
    chat(&w, "post-mortem, live", patch(id)).await.expect("writable live");

    w.execute(Command::CloseWorkspace).await.expect("close");
    w.execute(Command::OpenWorkspace { id: ws }).await.expect("reopen");

    chat(&w, "post-mortem, after the replay", patch(id))
        .await
        .expect("writable after the replay");
    // the annotation replays too
    let snap = read_chat(&w).await;
    let info = snap
        .channels
        .iter()
        .find(|i| i.channel == patch(id))
        .expect("the discussion channel replays");
    assert_eq!(info.state, Some(ProposalState::Rejected));
}

// ---- A2: the reasoning rides with the vote ---------------------------
//
// At 2-of-3 the window between "one vote" and "decided" is one foreign
// action wide, so "comment first, then approve" loses the race for the
// second reviewer (G2). `note` closes it: one command, the note first.

async fn vote_with_note(
    w: &WalletHandle,
    approve: bool,
    id: ProposalId,
    note: &str,
) -> Result<Reply, MoltError> {
    let note = Some(note.to_string());
    w.execute(if approve {
        Command::Approve { proposal: id, note }
    } else {
        Command::Decline { proposal: id, note }
    })
    .await
}

async fn bodies(w: &WalletHandle, id: ProposalId) -> Vec<String> {
    match w
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel: Some(patch(id)),
            view: None,
        })
        .await
        .expect("read state")
    {
        Reply::State(s) => s
            .applied
            .iter()
            .map(|m| {
                m.get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect(),
        other => panic!("unexpected reply: {other:?}"),
    }
}

/// The note is posted BEFORE the vote lands, so it stands above the
/// decision marker the seal appends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approve_note_lands_before_the_decision() {
    let w = spawn_solo();
    let id = propose(&w, "with a reason").await;
    vote_with_note(&w, true, id, "checked the sources - correct").await.expect("approve");
    let b = bodies(&w, id).await;
    assert_eq!(b.len(), 2, "the note and the decision summary");
    assert_eq!(b[0], "checked the sources - correct");
    assert!(b[1].contains('✓'), "the decision marker follows the note: {b:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decline_note_lands_before_the_decision() {
    let w = spawn_solo();
    let id = propose(&w, "with a reason against").await;
    vote_with_note(&w, false, id, "the date is wrong").await.expect("decline");
    let b = bodies(&w, id).await;
    assert_eq!(b.len(), 2, "the note and the decision summary");
    assert_eq!(b[0], "the date is wrong");
    assert!(b[1].contains('⊘'), "the decision marker follows the note: {b:?}");
}

/// G6: the third reviewer's finding lands even when the vote is already
/// decided - a late approval is not an error, and neither is its note.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_note_lands_on_a_late_approval() {
    let w = spawn_solo();
    let id = propose(&w, "sealed before the review").await;
    w.execute(Command::Approve { proposal: id, note: None }).await.expect("seal");
    vote_with_note(&w, true, id, "reviewed after the seal - one error").await.expect("late");
    let b = bodies(&w, id).await;
    assert_eq!(
        b.last().map(String::as_str),
        Some("reviewed after the seal - one error"),
        "the late finding has a room: {b:?}"
    );
}

/// An absent or blank note posts nothing - the vote stays one action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blank_note_posts_nothing() {
    let w = spawn_solo();
    let id = propose(&w, "no reason given").await;
    vote_with_note(&w, true, id, "   ").await.expect("approve");
    let b = bodies(&w, id).await;
    assert_eq!(b.len(), 1, "only the decision summary: {b:?}");
}
