// SPDX-License-Identifier: GPL-3.0-or-later
//! The Kanban pane against a real engine (`kanban_workflows.md` §8): the
//! form stages into the basket, the basket proposes one changeset, a drop
//! stages a move, the basket survives in `kanban_draft.json`.

use molt_core::kanban_fold::short_id;

use super::*;
use crate::actions::kanban::{drop_card, form_new, form_submit, render, with};
use crate::{Kanban, KbCard};

thread_local! {
    static BACKEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct Node {
    w: WalletHandle,
    ui: AppWindow,
    last: Arc<Mutex<Option<SessionSettings>>>,
    chat_ui: Arc<Mutex<ChatUiState>>,
}

impl Node {
    fn mirror(&self, rt: &tokio::runtime::Runtime) {
        rt.block_on(mirror(&self.w, &self.ui, &self.last, &self.chat_ui));
    }

    fn ctx(&self, rt: &tokio::runtime::Runtime) -> Ctx {
        Ctx {
            rt: rt.handle().clone(),
            wallet: self.w.clone(),
            weak: self.ui.as_weak(),
            last_settings: self.last.clone(),
            chat_ui: self.chat_ui.clone(),
        }
    }
}

/// A 1-of-2 quests republic as seat `a`: one approval applies.
fn quests_node(root: &std::path::Path, rt: &tokio::runtime::Runtime) -> Node {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    with(|st| *st = crate::actions::kanban::KanbanUi::default());
    drop(chain_workspace_with(root, 1, &["a", "b"], false, &["quests"], false).0);
    let (w, _) = node_with_chat(root);
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let node = Node {
        w,
        ui,
        last: Arc::new(Mutex::new(None)),
        chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
    };
    rt.block_on(async {
        let id = molt_storage::scan_workspaces(root)
            .first()
            .map(|e| e.info().id)
            .expect("the workspace is on disk");
        node.w.execute(Command::OpenWorkspace { id }).await.expect("opens");
    });
    node.mirror(rt);
    node
}

fn cards(m: &ModelRc<KbCard>) -> Vec<KbCard> {
    m.iter().collect()
}

/// What `propose` sends, run to completion.
fn propose_basket(node: &Node, rt: &tokio::runtime::Runtime) {
    let k = node.ui.global::<Kanban>();
    let payload = with(|st| {
        let rev = st
            .feed
            .as_ref()
            .and_then(|f| f.snap.board.as_ref())
            .and_then(|b| b.get("rev"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        st.basket.payload(rev, &k.get_basket_auto())
    });
    let reply = rt
        .block_on(node.w.execute(Command::Propose { surface: Surface::Quests, payload }))
        .expect("the engine takes the changeset");
    assert!(matches!(reply, Reply::Proposed { .. }), "{reply:?}");
    with(|st| st.basket = crate::kanban::Basket::default());
    node.mirror(rt);
}

#[test]
fn a_new_task_goes_through_the_basket_into_one_vote() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    let k = node.ui.global::<Kanban>();
    assert!(cards(&k.get_todo()).is_empty());

    form_new(&node.ui);
    assert!(k.get_form_open());
    let seats: Vec<(String, bool)> =
        k.get_f_assignees().iter().map(|p| (p.key.to_string(), p.on)).collect();
    assert_eq!(seats, [("a".to_string(), true), ("b".to_string(), false)], "the own seat preset");
    k.set_f_title("write docs".into());
    k.set_f_due("2026-8-1".into());
    assert!(!form_submit(&node.ui), "a bad date stays on the form");
    assert!(!k.get_f_error().is_empty());
    k.set_f_due("2099-01-01".into());
    assert!(form_submit(&node.ui));
    assert!(!k.get_form_open());
    assert_eq!(k.get_basket_count(), 1);
    assert!(k.get_basket_tip().contains("no acceptance criteria"), "{}", k.get_basket_tip());

    propose_basket(&node, &rt);
    let todo = cards(&k.get_todo());
    assert_eq!(todo.len(), 1, "1-of-2: the own approval applied it");
    assert_eq!(todo[0].title.as_str(), "write docs");
    assert_eq!(todo[0].badge.as_str(), "open");
}

/// §8: a drop stages; the card stays, a shadow waits in the target column,
/// and the move becomes real only through the vote.
#[test]
fn a_dropped_card_stages_and_the_vote_moves_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    let k = node.ui.global::<Kanban>();
    form_new(&node.ui);
    k.set_f_title("api".into());
    assert!(form_submit(&node.ui));
    propose_basket(&node, &rt);
    let id = cards(&k.get_todo())[0].id.to_string();

    assert!(drop_card(&node.ui, &id, 1));
    assert_eq!(k.get_basket_count(), 1);
    let todo = cards(&k.get_todo());
    assert_eq!(todo.len(), 1, "the card stays");
    assert!(todo[0].staged.contains("in basket"));
    let wip = cards(&k.get_wip());
    assert!(wip.len() == 1 && wip[0].shadow, "the shadow in In progress");
    assert!(!drop_card(&node.ui, &id, 2), "floating work cannot close from to do");

    propose_basket(&node, &rt);
    assert!(cards(&k.get_todo()).is_empty());
    let wip = cards(&k.get_wip());
    assert!(wip.len() == 1 && !wip[0].shadow, "the vote moved it");
    assert_eq!(wip[0].short.as_str(), short_id(&id));
}

/// §8: the basket is kept beside the wiki draft and comes back on open.
#[test]
fn the_basket_survives_in_the_draft() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    crate::actions::kanban::wire(&node.ui, &node.ctx(&rt));
    let k = node.ui.global::<Kanban>();
    k.invoke_form_new();
    k.set_f_title("kept".into());
    k.invoke_form_submit();
    let saved = (0..200).find_map(|_| {
        let draft = match rt.block_on(node.w.execute(Command::KanbanDraftLoad)) {
            Ok(Reply::KanbanDraft { draft }) => draft,
            _ => String::new(),
        };
        if draft.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(10));
            None
        } else {
            Some(draft)
        }
    });
    let saved = saved.expect("the basket was saved");
    assert!(saved.contains("kept"), "{saved}");
    // a fresh window state adopts it on the next open of this workspace
    with(|st| *st = crate::actions::kanban::KanbanUi::default());
    node.mirror(&rt);
    assert_eq!(k.get_basket_count(), 0, "the load is asynchronous");
    let ws = with(|st| st.workspace.clone());
    crate::actions::kanban::adopt_draft(&node.ui, &ws, &saved);
    assert_eq!(k.get_basket_count(), 1);
    crate::actions::kanban::adopt_draft(&node.ui, &ws, "{}");
    assert_eq!(k.get_basket_count(), 1, "never over staged work");
    k.invoke_basket_discard();
    assert_eq!(k.get_basket_count(), 0);
}

/// The drill-in names both link directions and offers only legal moves.
#[test]
fn the_drill_in_offers_the_legal_moves() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    let k = node.ui.global::<Kanban>();
    form_new(&node.ui);
    k.set_f_title("base".into());
    assert!(form_submit(&node.ui));
    propose_basket(&node, &rt);
    let base = cards(&k.get_todo())[0].id.to_string();
    form_new(&node.ui);
    k.set_f_title("top".into());
    with(|st| st.form.blocked = vec![base.clone()]);
    assert!(form_submit(&node.ui));
    propose_basket(&node, &rt);

    k.set_sel(base.as_str().into());
    render(&node.ui);
    assert_eq!(k.get_prereq_for().row_count(), 1);
    let moves: Vec<String> = k.get_moves().iter().map(|m| m.key.to_string()).collect();
    assert_eq!(moves, ["wip", "cancelled"]);
    assert_eq!(k.get_detail().creator.as_str(), "a");
}

#[test]
fn the_wake_group_reads_into_the_settings_draft() {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    let ui = AppWindow::new().expect("headless window");
    let stored = SessionSettings::default();
    apply_settings_fields(&ui, &stored);
    assert!(ui.get_cfg_wake_on_kanban());
    assert_eq!(ui.get_cfg_wake_rest().as_str(), "300");
    ui.set_cfg_wake_on_kanban(false);
    ui.set_cfg_wake_rest("90".into());
    ui.set_cfg_wake_lead("999".into());
    let d = read_settings_draft(&ui, &stored);
    assert_eq!(d.wake_on, ["poked", "vote_pending", "task_start"]);
    assert_eq!(d.wake_min_interval_secs, 90);
    assert_eq!(d.task_wake_lead_min, molt_core::TASK_WAKE_LEAD_MAX, "clamped");
    ui.set_cfg_wake_rest("x".into());
    assert_eq!(read_settings_draft(&ui, &stored).wake_min_interval_secs, 300, "not a number keeps it");
}

/// The skill modal shows the one MCP text (§6.2).
#[test]
fn the_skill_modal_shows_the_mcp_skill() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    crate::actions::kanban::wire(&node.ui, &node.ctx(&rt));
    assert_eq!(node.ui.get_skill_text().as_str(), molt_mcp::WAKE_SKILL);
}

#[cfg(feature = "live-preview")]
fn board_screen(ui: &AppWindow) -> Shown {
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("quests".into());
    ui.set_selected_view("board".into());
    let shown = show_headless(ui);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    shown
}

/// A real pointer drag from To do onto In progress stages the start.
#[cfg(feature = "live-preview")]
#[test]
fn a_real_drag_onto_in_progress_stages_a_start() {
    type H = i_slint_backend_testing::ElementHandle;
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    crate::actions::kanban::wire(&node.ui, &node.ctx(&rt));
    let k = node.ui.global::<Kanban>();
    form_new(&node.ui);
    k.set_f_title("drag me".into());
    assert!(form_submit(&node.ui));
    propose_basket(&node, &rt);
    let _shown = board_screen(&node.ui);
    let card = H::find_by_accessible_label(&node.ui, "drag me").next().expect("the card renders");
    let pos = card.absolute_position();
    let size = card.size();
    let from = slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
    let to = slint::LogicalPosition::new(from.x + 1400.0 / 3.0, from.y);
    let win = node.ui.window();
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: from });
    win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: from,
        button: slint::platform::PointerEventButton::Left,
    });
    for step in 1..=10 {
        let x = from.x + (to.x - from.x) * step as f32 / 10.0;
        win.dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, from.y),
        });
    }
    win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: to,
        button: slint::platform::PointerEventButton::Left,
    });
    assert_eq!(k.get_basket_count(), 1, "the drop staged one act");
    assert!(cards(&k.get_wip()).iter().any(|c| c.shadow));
    assert_eq!(k.get_sel().as_str(), "", "a drag is not a click");
}
