// SPDX-License-Identifier: GPL-3.0-or-later
//! Discussion groups in the nav: four or more open votes of one kind fold
//! under one `<Kind> (N)` header, collapsed until the user expands it.

use super::discussion::Opened;
use super::*;

/// A 2-of-2 republic on disk: every vote this seat files stays open.
fn republic(root: &std::path::Path) {
    drop(workspace_on_disk(root, 2, &["walter", "mara"], "test the groups"));
}

/// [`republic`], plus four wiki patches mara filed, each with one post of
/// hers in its discussion (unread here).
fn republic_with_four_patches_and_a_post(root: &std::path::Path) {
    let (mut ws, now) = workspace_on_disk(root, 2, &["walter", "mara"], "test the groups");
    let read = MessageId([6; 16]);
    let mut events = vec![molt_core::WorkspaceEvent::Chat(ChatMessage::text(read, "walter", "zero", now))];
    events.extend((1..=4).map(|n| molt_core::WorkspaceEvent::Proposed {
        id: ProposalId(n),
        surface: Surface::Memory,
        payload: wiki_patch(n),
    }));
    events.extend((1..=4u8).map(|n| {
        molt_core::WorkspaceEvent::Chat(
            ChatMessage::text(MessageId([10 + n; 16]), "mara", "look", now)
                .with_channel(ChannelRef::Patch { id: ProposalId(u64::from(n)) }),
        )
    }));
    for (seq, body) in (2u64..).zip(events) {
        let by = if seq == 2 { "walter" } else { "mara" };
        ws.append(&molt_core::EventEnvelope { prev_seq: seq - 1, seq, ts: now, by: by.to_string(), body })
            .expect("append");
    }
    ws.sync().expect("sync");
    let mut prefs = ws.prefs.clone();
    prefs.read_cursors.insert(ChannelRef::Group.storage_key(), read.to_string());
    ws.set_prefs(prefs).expect("prefs");
}

fn wiki_patch(n: u64) -> serde_json::Value {
    serde_json::json!({
        "op": "wiki_patch",
        "summary": "+1",
        "value": format!("diff --git a/p{n}.md b/p{n}.md\nnew file mode 100644\n--- /dev/null\n+++ b/p{n}.md\n@@ -0,0 +1,1 @@\n+{n}\n"),
    })
}

impl Opened {
    /// File a vote and post in its discussion: a channel exists once a
    /// message does.
    fn propose(&self, surface: Surface, payload: serde_json::Value) -> String {
        let id = match self.rt.block_on(self.w.execute(Command::Propose { surface, payload })) {
            Ok(Reply::Proposed { id, .. }) => id,
            other => panic!("unexpected: {other:?}"),
        };
        self.run(Command::Chat {
            body: "why".to_string(),
            quote: None,
            channel: ChannelRef::Patch { id },
        });
        format!("patch:{}", id.0)
    }

    fn propose_wiki(&self, n: u64) -> String {
        self.propose(Surface::Memory, wiki_patch(n))
    }

    fn propose_rename(&self, n: u64) -> String {
        self.propose(
            Surface::Organization,
            serde_json::json!({ "op": "set_name", "value": format!("Name{n}") }),
        )
    }

    fn propose_four_wiki(&self) -> Vec<String> {
        (1..=4).map(|n| self.propose_wiki(n)).collect()
    }

    fn on_chat(&self) {
        self.run(Command::SelectSurface { surface: Surface::Chat });
        self.mirror();
    }

    fn groups_open(&self) -> Vec<String> {
        match self.rt.block_on(self.w.execute(Command::ReadSession)) {
            Ok(Reply::Session(s)) => s.settings.discussion_groups_open,
            other => panic!("unexpected: {other:?}"),
        }
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            rt: self.rt.handle().clone(),
            wallet: self.w.clone(),
            weak: self.ui.as_weak(),
            last_settings: self.last.clone(),
            chat_ui: self.chat_ui.clone(),
        }
    }

    /// Wait for the engine to store `want`, then mirror.
    fn await_groups_open(&self, want: &[&str]) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self.groups_open() != want {
            assert!(std::time::Instant::now() < deadline, "stored {:?}, want {want:?}", self.groups_open());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.mirror();
    }
}

/// The nav's discussion rows: key, plus `+`/`-` for an open/collapsed header.
fn nav(ui: &AppWindow) -> Vec<String> {
    ui.get_chat_channels()
        .iter()
        .map(|c| match (c.header, c.open) {
            (true, true) => format!("+{}", c.label),
            (true, false) => format!("-{}", c.label),
            _ => c.key.to_string(),
        })
        .collect()
}

#[test]
fn the_fourth_wiki_patch_folds_the_discussions_into_a_collapsed_group() {
    let o = Opened::new(republic);
    let mut keys: Vec<String> = (1..=3).map(|n| o.propose_wiki(n)).collect();
    o.on_chat();
    assert_eq!(nav(&o.ui), keys, "three stay plain rows");
    keys.push(o.propose_wiki(4));
    o.mirror();
    assert_eq!(nav(&o.ui), ["-Wiki patches (4)"]);
    let snap = build_ui_snapshot(&o.ui);
    assert_eq!(
        snap.discussions,
        keys.iter().map(|k| format!("{k} group=memory collapsed")).collect::<Vec<_>>(),
        "the walk still sees every discussion"
    );
}

#[test]
fn an_expanded_group_survives_new_activity_a_fifth_entry_and_a_restart() {
    let o = Opened::new(republic);
    let keys = o.propose_four_wiki();
    o.on_chat();
    crate::actions::chat::wire(&o.ui, &o.ctx());
    o.ui.invoke_toggle_discussion_group("memory".into(), false);
    o.await_groups_open(&["memory"]);
    assert_eq!(nav(&o.ui)[0], "+Wiki patches (4)");
    assert_eq!(nav(&o.ui)[1..], keys);
    o.run(Command::Chat {
        body: "more".to_string(),
        quote: None,
        channel: parse_channel_key(&keys[2]).expect("a channel"),
    });
    o.propose_wiki(5);
    o.mirror();
    assert_eq!(nav(&o.ui)[0], "+Wiki patches (5)");
    assert_eq!(nav(&o.ui).len(), 6);

    let settings = SessionSettings { discussion_groups_open: o.groups_open(), ..SessionSettings::default() };
    let again = Opened::with_settings(republic, settings);
    again.propose_four_wiki();
    again.on_chat();
    assert_eq!(nav(&again.ui)[0], "+Wiki patches (4)", "the stored choice holds after a restart");

    again.run(Command::SetDiscussionGroup { kind: "memory".to_string(), open: false });
    again.mirror();
    assert_eq!(nav(&again.ui), ["-Wiki patches (4)"]);
}

#[test]
fn a_collapsed_header_carries_its_members_unread_posts() {
    let o = Opened::new(republic_with_four_patches_and_a_post);
    o.run(Command::SelectSurface { surface: Surface::Organization });
    o.mirror();
    let header = o.ui.get_chat_channels().row_data(0).expect("the header");
    assert!(header.header && !header.open);
    assert_eq!(header.unread, 4, "mara's posts");
    assert_eq!(surface_tab(&o.ui, "chat").expect("chat tab").unread_count, 4, "counted once");
}

#[test]
fn the_selected_discussion_shows_its_group_expanded_without_storing_it() {
    let o = Opened::new(republic);
    let keys = o.propose_four_wiki();
    o.run(Command::SelectSurface { surface: Surface::Chat });
    o.chat_ui.lock().expect("ui state").select(parse_channel_key(&keys[1]).expect("a channel"));
    o.mirror();
    assert_eq!(nav(&o.ui)[0], "+Wiki patches (4)");
    assert_eq!(nav(&o.ui)[1..], keys);
    assert!(o.ui.get_selected_channel_listed());
    assert!(o.groups_open().is_empty(), "the preference is untouched");

    o.chat_ui.lock().expect("ui state").select(ChannelRef::Group);
    o.mirror();
    assert_eq!(nav(&o.ui), ["-Wiki patches (4)"], "collapsed again once the selection moves");
}

#[test]
fn dropping_below_four_dissolves_the_group() {
    let o = Opened::new(republic);
    let mut keys = o.propose_four_wiki();
    o.on_chat();
    assert_eq!(nav(&o.ui), ["-Wiki patches (4)"]);
    let gone = keys.remove(1);
    let Some(ChannelRef::Patch { id }) = parse_channel_key(&gone) else { panic!("a vote") };
    o.run(Command::Withdraw { proposal: id, note: None });
    o.mirror();
    assert_eq!(nav(&o.ui), keys);
}

#[test]
fn each_kind_gets_its_own_group_and_topics_stay_plain() {
    let o = Opened::new(republic);
    for n in 1..=4 {
        o.propose_wiki(n);
    }
    for n in 1..=4 {
        o.propose_rename(n);
    }
    o.run(Command::Chat {
        body: "hi".to_string(),
        quote: None,
        channel: ChannelRef::Topic { name: "plan".to_string() },
    });
    o.on_chat();
    assert_eq!(nav(&o.ui), ["topic:plan", "-Wiki patches (4)", "-Organization (4)"]);
}

/// The header is one nav row high, at both scales.
#[cfg(feature = "live-preview")]
#[test]
fn a_group_header_is_one_row_high_and_a_click_toggles_it() {
    let o = Opened::new(republic);
    for n in 1..=4 {
        o.propose_wiki(n);
    }
    o.run(Command::Chat {
        body: "hi".to_string(),
        quote: None,
        channel: ChannelRef::Topic { name: "plan".to_string() },
    });
    o.on_chat();
    apply_strings(&o.ui, 0);
    o.ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    o.ui.set_screen(AppScreen::Main);
    let _shown = show_headless(&o.ui);
    rail_settled();
    let height = |ty: &str| {
        i_slint_backend_testing::ElementHandle::find_by_element_type_name(&o.ui, ty)
            .filter(|e| e.size().width > 0.0)
            .map(|e| e.size().height)
            .next()
            .unwrap_or_else(|| panic!("a {ty} renders"))
    };
    for (font, scale) in [(14.0_f32, 1.0_f32), (21.0, 1.5)] {
        o.ui.global::<Theme>().set_fs_app(font);
        rail_settled();
        assert!((height("GroupRow") - 30.0 * scale).abs() < 0.5, "scale {scale}: {}", height("GroupRow"));
        assert!((height("GroupRow") - height("ViewRow")).abs() < 0.5, "scale {scale}");
    }

    crate::actions::chat::wire(&o.ui, &o.ctx());
    let header = i_slint_backend_testing::ElementHandle::find_by_accessible_label(&o.ui, "Wiki patches (4)")
        .find(|e| e.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Button))
        .expect("the header is a button named by its group");
    click(&o.ui, &header);
    o.await_groups_open(&["memory"]);
    assert_eq!(nav(&o.ui).len(), 6, "topic, header, four members");
}

impl Opened {
    /// File a wiki-patch vote nobody posts on; its channel key.
    fn propose_silent(&self, n: u64) -> String {
        match self.rt.block_on(self.w.execute(Command::Propose { surface: Surface::Memory, payload: wiki_patch(n) })) {
            Ok(Reply::Proposed { id, .. }) => format!("patch:{}", id.0),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The open votes' discussions as MCP `read_state` lists them.
    fn read_state_open(&self) -> Vec<String> {
        let snap = match self.rt.block_on(self.w.execute(Command::ReadState {
            surface: Surface::Chat,
            channel: None,
            view: None,
        })) {
            Ok(Reply::State(snap)) => snap,
            other => panic!("unexpected: {other:?}"),
        };
        let mut keys: Vec<String> = snap
            .channels
            .iter()
            .filter(|i| i.state == Some(molt_core::ProposalState::Proposed))
            .map(|i| crate::channels::channel_key(&i.channel))
            .collect();
        keys.sort();
        keys
    }
}

/// The snapshot's discussion keys, sorted.
fn walk_keys(ui: &AppWindow) -> Vec<String> {
    let mut keys: Vec<String> = build_ui_snapshot(ui)
        .discussions
        .iter()
        .map(|d| d.split_whitespace().next().unwrap_or_default().to_string())
        .collect();
    keys.sort();
    keys
}

/// An open vote nobody posted on has its row, adds nothing to the Chat
/// badge, and leaves with its vote.
#[test]
fn an_open_vote_nobody_posted_on_has_a_row_until_it_is_decided() {
    let o = Opened::new(republic);
    let key = o.propose_silent(1);
    o.on_chat();
    assert_eq!(nav(&o.ui), std::slice::from_ref(&key));
    assert_eq!(surface_tab(&o.ui, "chat").expect("chat tab").unread_count, 0, "an empty row counts nothing");
    assert_eq!(walk_keys(&o.ui), std::slice::from_ref(&key), "the walk sees it");
    let Some(ChannelRef::Patch { id }) = parse_channel_key(&key) else { panic!("a vote") };
    o.run(Command::Withdraw { proposal: id, note: None });
    o.mirror();
    assert!(nav(&o.ui).is_empty(), "gone with its vote");
}

/// Four silent wiki patches fold like any four; the window and MCP
/// `read_state` list the same open discussions.
#[test]
fn silent_wiki_patches_fold_and_the_window_lists_what_read_state_lists() {
    let o = Opened::new(republic);
    for n in 1..=4 {
        o.propose_silent(n);
    }
    o.on_chat();
    assert_eq!(nav(&o.ui), ["-Wiki patches (4)"]);
    assert_eq!(walk_keys(&o.ui), o.read_state_open());
    assert_eq!(o.read_state_open().len(), 4);
}

/// `n` silent wiki patches mara filed, on disk (past one proposer's cap).
fn republic_with_silent_patches(n: u64) -> impl FnOnce(&std::path::Path) {
    move |root| {
        let (mut ws, now) = workspace_on_disk(root, 2, &["walter", "mara"], "test the groups");
        for (seq, id) in (2u64..).zip(1..=n) {
            let body = molt_core::WorkspaceEvent::Proposed { id: ProposalId(id), surface: Surface::Memory, payload: wiki_patch(id) };
            ws.append(&molt_core::EventEnvelope { prev_seq: seq - 1, seq, ts: now, by: "mara".to_string(), body })
                .expect("append");
        }
        ws.sync().expect("sync");
    }
}

/// Sixty silent patches are one header row; expanded, all sixty list.
#[test]
fn sixty_silent_patches_are_one_header_row() {
    let o = Opened::new(republic_with_silent_patches(60));
    o.on_chat();
    assert_eq!(nav(&o.ui), ["-Wiki patches (60)"]);
    o.run(Command::SetDiscussionGroup { kind: "memory".to_string(), open: true });
    o.mirror();
    assert_eq!(nav(&o.ui).len(), 61);
}

/// 200 open cards refresh by patching the row model in place: the model
/// survives, the selected row stays listed, the expanded group stays open.
#[test]
fn a_refresh_with_200_open_cards_keeps_the_model_and_its_state() {
    let o = Opened::new(republic_with_silent_patches(200));
    o.run(Command::SelectSurface { surface: Surface::Chat });
    o.run(Command::SetDiscussionGroup { kind: "memory".to_string(), open: true });
    o.chat_ui.lock().expect("ui state").select(ChannelRef::Patch { id: ProposalId(7) });
    o.mirror();
    let model = o.ui.get_chat_channels();
    assert_eq!(model.row_count(), 201);
    o.run(Command::Chat { body: "news".to_string(), quote: None, channel: ChannelRef::Patch { id: ProposalId(9) } });
    let started = std::time::Instant::now();
    o.mirror();
    let took = started.elapsed();
    eprintln!("refresh with 200 open cards: {took:?}");
    assert!(o.ui.get_chat_channels() == model, "patched in place, never swapped");
    assert_eq!(nav(&o.ui)[0], "+Wiki patches (200)");
    assert_eq!(o.ui.get_selected_channel(), "patch:7");
    assert!(o.ui.get_selected_channel_listed());
}
