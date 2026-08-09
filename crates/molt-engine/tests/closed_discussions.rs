// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Discussions of decided votes are read-only: once a proposal leaves
//! `Proposed` (declined or applied), its `Patch` discussion channel
//! refuses new local writes — chat and file shares alike — with
//! [`MoltError::DiscussionClosed`]. The channel stays readable (it is a
//! view over the one log, chat_bus.md), UNKNOWN patch ids stay writable
//! (chat-bus Q4: a ref may arrive before — or forever without — its
//! referent), and the wire receive path stays permissive (convergence
//! over enforcement — a slower peer's in-flight message must still land
//! identically everywhere).

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
        Reply::Proposed { id } => id,
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

/// The error carries the id and the terminal state — both surfaces (GUI
/// toast, MCP error string) name exactly what closed the discussion.
fn assert_closed(result: Result<Reply, MoltError>, id: ProposalId, state: ProposalState) {
    match result {
        Err(MoltError::DiscussionClosed(got_id, got_state)) => {
            assert_eq!(got_id, id, "the error names the proposal");
            assert_eq!(got_state, state, "the error names the decided state");
        }
        other => panic!("expected DiscussionClosed({id:?}, {state:?}), got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_into_a_declined_discussion_is_refused() {
    let w = spawn_solo();
    let id = propose(&w, "veto me").await;
    chat(&w, "still open", patch(id)).await.expect("a Proposed discussion is writable");
    w.execute(Command::Decline { proposal: id }).await.expect("decline");
    assert_closed(
        chat(&w, "too late", patch(id)).await,
        id,
        ProposalState::Rejected,
    );
    // …and the earlier message is still readable: the channel closed for
    // writes, not for reads — plus the decliner's decision summary, the
    // engine-authored System line every decided vote appends (2026-08-09)
    let snap = read_chat(&w).await;
    assert_eq!(
        snap.applied.len(),
        2,
        "the pre-decline message stays in the log, plus the summary"
    );
    assert!(
        snap.applied.last().and_then(|m| m.get("body")).and_then(|b| b.as_str())
            .is_some_and(|b| b.contains('⊘')),
        "the last line is the decline summary"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_into_an_applied_discussion_is_refused() {
    let w = spawn_solo();
    let id = propose(&w, "seal me").await;
    w.execute(Command::Approve { proposal: id }).await.expect("approve to threshold");
    assert_closed(
        chat(&w, "after the seal", patch(id)).await,
        id,
        ProposalState::Applied,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn share_file_into_a_closed_discussion_is_refused() {
    let w = spawn_solo();
    let id = propose(&w, "no more attachments").await;
    w.execute(Command::Decline { proposal: id }).await.expect("decline");
    let res = w
        .execute(Command::ShareFile {
            path: "/tmp/anything.txt".to_string(),
            channel: patch(id),
        })
        .await;
    assert_closed(res, id, ProposalState::Rejected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proposed_discussion_stays_writable() {
    let w = spawn_solo();
    let id = propose(&w, "still deliberating").await;
    chat(&w, "opinions?", patch(id)).await.expect("an open vote's discussion accepts chat");
}

/// Chat-bus Q4: a patch ref whose proposal this node never saw must keep
/// working — channels never error on unknown ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_patch_discussion_stays_writable() {
    let w = spawn_solo();
    chat(&w, "referent unknown here", patch(ProposalId(999)))
        .await
        .expect("an unknown patch id stays writable (Q4)");
}

/// The read side of co-equality: `ChannelInfo.state` annotates each patch
/// channel with its vote's lifecycle, so ANY frontend (GUI, MCP agent)
/// renders closed-ness from the same engine-side data. Group/Topic — and
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
    w.execute(Command::Decline { proposal: declined }).await.expect("decline");
    w.execute(Command::Approve { proposal: applied }).await.expect("approve");

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

/// The guard reads `State.proposals`, which the log replay rebuilds — so a
/// decided vote's discussion must stay closed across close/reopen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enforcement_survives_close_and_reopen() {
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
    await_founding(&w).await;
    w.execute(Command::CreateFinish).await.expect("finish");
    let ws = read_session(&w).await.active_workspace.clone();

    let id = propose(&w, "decided before the restart").await;
    // file a message while the vote is open, so the channel exists in the
    // enumeration after the replay
    chat(&w, "deliberated", patch(id)).await.expect("chat while open");
    w.execute(Command::Decline { proposal: id }).await.expect("decline");
    assert_closed(
        chat(&w, "refused live", patch(id)).await,
        id,
        ProposalState::Rejected,
    );

    w.execute(Command::CloseWorkspace).await.expect("close");
    w.execute(Command::OpenWorkspace { id: ws }).await.expect("reopen");

    assert_closed(
        chat(&w, "refused after replay", patch(id)).await,
        id,
        ProposalState::Rejected,
    );
    // the annotation replays too
    let snap = read_chat(&w).await;
    let info = snap
        .channels
        .iter()
        .find(|i| i.channel == patch(id))
        .expect("the discussion channel replays");
    assert_eq!(info.state, Some(ProposalState::Rejected));
}
