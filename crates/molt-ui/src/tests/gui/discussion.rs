// SPDX-License-Identifier: GPL-3.0-or-later
//! A vote's discussion made visible: the count on the Chat row, the
//! marker and the preview on the vote card.

use super::*;

/// A republic ON DISK in which, since this seat last read the group,
/// another seat opened a vote, posted in its discussion and said
/// something in the group.
fn workspace_with_a_discussed_vote(root: &std::path::Path) {
    let (mut ws, now) = workspace_on_disk(root, 2, &["walter", "mara"], "test the discussion");
    let vote = ChannelRef::Patch { id: ProposalId(1) };
    let read = MessageId([6; 16]);
    let events = [
        ("walter", molt_core::WorkspaceEvent::Chat(ChatMessage::text(read, "walter", "zero", now))),
        (
            "mara",
            molt_core::WorkspaceEvent::Proposed {
                id: ProposalId(1),
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_name", "value": "NewName" }),
            },
        ),
        (
            "mara",
            molt_core::WorkspaceEvent::Chat(
                ChatMessage::text(MessageId([7; 16]), "mara", "one", now).with_channel(vote),
            ),
        ),
        (
            "mara",
            molt_core::WorkspaceEvent::Chat(ChatMessage::text(MessageId([8; 16]), "mara", "two", now)),
        ),
    ];
    for (seq, (by, body)) in (2u64..).zip(events) {
        ws.append(&molt_core::EventEnvelope { prev_seq: seq - 1, seq, ts: now, by: by.to_string(), body })
            .expect("append");
    }
    ws.sync().expect("sync");
    // a seat without cursors opens with everything read
    let mut prefs = ws.prefs.clone();
    prefs.read_cursors.insert(ChannelRef::Group.storage_key(), read.to_string());
    ws.set_prefs(prefs).expect("prefs");
}

/// The stored workspace, opened with Organization on screen and mirrored.
struct Opened {
    rt: tokio::runtime::Runtime,
    w: WalletHandle,
    ui: AppWindow,
    chat_ui: Arc<Mutex<ChatUiState>>,
    last: Arc<Mutex<Option<SessionSettings>>>,
    _tmp: tempfile::TempDir,
}

impl Opened {
    fn new(prepare: impl FnOnce(&std::path::Path)) -> Self {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        prepare(&root);
        let rt = rt();
        let (w, ui) = {
            let _guard = rt.enter();
            (node_with_chat(&root).0, AppWindow::new().expect("headless window"))
        };
        let me = Self {
            rt,
            w,
            ui,
            chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
            last: Arc::new(Mutex::new(None)),
            _tmp: tmp,
        };
        let stored = molt_storage::scan_workspaces(&root)
            .first()
            .map(|e| e.info().id)
            .expect("the workspace is on disk");
        me.run(Command::OpenWorkspace { id: stored });
        me.run(Command::SelectSurface { surface: Surface::Organization });
        me.mirror();
        me
    }

    fn run(&self, command: Command) {
        self.rt.block_on(self.w.execute(command)).expect("the command runs");
    }

    fn mirror(&self) {
        self.rt.block_on(mirror(&self.w, &self.ui, &self.last, &self.chat_ui));
    }
}

/// **A post in a vote's discussion is counted on the top-level Chat row
/// and marked on the vote's card, with the post itself.** The count is
/// the chat tab's own field: the pending-proposals total stays what it
/// was.
#[test]
fn a_post_in_a_votes_discussion_reaches_the_chat_row_and_the_card() {
    let o = Opened::new(workspace_with_a_discussed_vote);
    let chat = surface_tab(&o.ui, "chat").expect("chat tab");
    assert_eq!(chat.unread_count, 2, "the group post plus the discussion post");
    assert_eq!(chat.pending_count, 0, "chat messages are not pending proposals");
    assert_eq!(build_ui_snapshot(&o.ui).pending_count, 1, "the one open vote");

    let org = surface_tab(&o.ui, "organization").expect("org tab");
    assert_eq!(org.unread_count, 0);
    let card = org.pending.row_data(0).expect("the open vote");
    assert_eq!((card.my_vote, card.unread), (0, 1));
    let talk: Vec<(String, String)> =
        card.talk.iter().map(|t| (t.who.to_string(), t.text.to_string())).collect();
    assert_eq!(talk, vec![("mara".to_string(), "one".to_string())]);
}

/// **A channel left selected behind another surface still counts.** Being
/// the chat filter is not being on screen.
#[test]
fn a_channel_selected_but_not_on_screen_keeps_its_unread_count() {
    let o = Opened::new(workspace_with_a_discussed_vote);
    assert_eq!(o.ui.get_group_unread(), 1, "Organization is on screen, not the group chat");
    o.run(Command::SelectSurface { surface: Surface::Chat });
    o.mirror();
    o.mirror();
    assert_eq!(o.ui.get_group_unread(), 0, "on screen is read");
}

/// The chat tab with `unread` messages and an Organization tab holding
/// one open vote with `posts` discussion posts, `unread` of them unread.
#[cfg(feature = "live-preview")]
fn votes_window(unread: i32, posts: usize) -> (AppWindow, Shown) {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("organization".into());
    ui.set_selected_view("pending".into());
    let talk: Vec<TalkLine> = (0..posts)
        .map(|i| TalkLine { who: "mara".into(), text: format!("post {i}").into(), when: "just now".into() })
        .collect();
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![
        SurfaceTab {
            key: "organization".into(),
            gated: true,
            pending_count: 1,
            pending: ModelRc::new(VecModel::from(vec![ProposalRow {
                id: 3,
                text: "a".into(),
                threshold: 2,
                unread,
                talk: ModelRc::new(VecModel::from(talk)),
                ..ProposalRow::default()
            }])),
            ..SurfaceTab::default()
        },
        SurfaceTab { key: "chat".into(), unread_count: unread, ..SurfaceTab::default() },
    ])));
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    (ui, shown)
}

/// How many elements with `id` render with a height.
#[cfg(feature = "live-preview")]
fn rendered(ui: &AppWindow, id: &str) -> usize {
    i_slint_backend_testing::ElementHandle::find_by_element_id(ui, id)
        .filter(|e| e.size().height > 0.0)
        .count()
}

/// **The top-level Chat row carries the unread count, also as the dot of
/// the collapsed rail; the open vote's card carries the 💬 pill and its
/// latest posts.** No unread, no posts: none of it.
#[cfg(feature = "live-preview")]
#[test]
fn unread_discussion_posts_show_on_the_chat_row_and_the_vote_card() {
    i_slint_backend_testing::init_no_event_loop();
    {
        let (ui, _shown) = votes_window(2, 2);
        assert_eq!(rendered(&ui, "SurfaceRow::sr-badge"), 2, "Organization's open vote and Chat's unread");
        assert_eq!(rendered(&ui, "ProposalCard::pc-unread"), 1);
        assert_eq!(rendered(&ui, "ProposalCard::pc-talk"), 1);
        for text in ["post 0", "post 1"] {
            assert_eq!(
                i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, text).count(),
                1,
                "{text} is on the card"
            );
        }
        let toggle = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::nav-toggle")
            .next()
            .expect("the rail toggle");
        click(&ui, &toggle);
        assert_eq!(rendered(&ui, "SurfaceRow::sr-dot"), 2, "the collapsed rail keeps both hints");
    }
    let (ui, _shown) = votes_window(0, 0);
    assert_eq!(rendered(&ui, "SurfaceRow::sr-badge"), 1, "only Organization's open vote");
    assert_eq!(rendered(&ui, "ProposalCard::pc-unread"), 0);
    assert_eq!(rendered(&ui, "ProposalCard::pc-talk"), 0);
}

/// **The compact card heading the open discussion repeats neither.**
#[cfg(feature = "live-preview")]
#[test]
fn the_compact_card_shows_no_pill_and_no_preview() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = votes_window(2, 2);
    let card = surface_tab(&ui, "organization").and_then(|s| s.pending.row_data(0)).expect("card");
    ui.set_selected_surface("chat".into());
    ui.set_selected_view("today".into());
    ui.set_selected_channel("patch:3".into());
    ui.set_selected_channel_votable(true);
    ui.set_selected_channel_org(true);
    ui.set_selected_decision(card);
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ProposalCard").count() >= 1,
        "the compact card renders"
    );
    assert_eq!(rendered(&ui, "ProposalCard::pc-unread"), 0);
    assert_eq!(rendered(&ui, "ProposalCard::pc-talk"), 0);
}
