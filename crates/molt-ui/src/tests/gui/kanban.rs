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
    quests_node_with(root, rt, &["quests"])
}

fn quests_node_with(root: &std::path::Path, rt: &tokio::runtime::Runtime, features: &[&str]) -> Node {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    with(|st| *st = crate::actions::kanban::KanbanUi::default());
    drop(chain_workspace_with(root, 1, &["a", "b"], false, features, false).0);
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
    let (payload, sent) = with(|st| st.begin_propose(String::new(), &k.get_basket_auto()))
        .expect("a basket to propose");
    let reply = rt
        .block_on(node.w.execute(Command::Propose { surface: Surface::Quests, payload }))
        .expect("the engine takes the changeset");
    assert!(matches!(reply, Reply::Proposed { .. }), "{reply:?}");
    with(|st| st.settle_propose(Some(&sent)));
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
    // a fresh window state adopts it from the next push
    with(|st| *st = crate::actions::kanban::KanbanUi::default());
    node.mirror(&rt);
    assert_eq!(k.get_basket_count(), 1, "the push carried the stored basket");
    k.invoke_form_new();
    k.set_f_title("more".into());
    k.invoke_form_submit();
    node.mirror(&rt);
    assert_eq!(k.get_basket_count(), 2, "adopted once, never over staged work");
    k.invoke_basket_discard();
    assert_eq!(k.get_basket_count(), 0);
}

/// One Propose at a time; acts staged meanwhile stay in the basket.
#[test]
fn a_propose_in_flight_keeps_what_was_staged_meanwhile() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    let k = node.ui.global::<Kanban>();
    form_new(&node.ui);
    k.set_f_title("first".into());
    assert!(form_submit(&node.ui));
    let (payload, sent) = with(|st| st.begin_propose("mine".into(), "auto")).expect("begins");
    assert_eq!(payload["summary"], serde_json::json!("mine"));
    let rev = with(|st| st.feed.as_ref().map(|f| f.folded.rev));
    assert_eq!(payload["base_rev"].as_u64(), rev, "the fold the review checked");
    assert!(with(|st| st.begin_propose(String::new(), "auto")).is_none(), "in flight");
    form_new(&node.ui);
    k.set_f_title("second".into());
    assert!(form_submit(&node.ui));
    with(|st| st.settle_propose(Some(&sent)));
    let left = with(|st| st.basket.to_draft());
    assert!(left.contains("second") && !left.contains("first"), "{left}");
    assert!(!left.contains("mine"), "the summary went with the vote");
    assert!(with(|st| st.begin_propose(String::new(), "auto")).is_some(), "settled");
}

/// Toasts follow the APPLIED `wake_on` from the session push.
#[test]
fn notices_follow_the_applied_wake_on() {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    with(|st| *st = crate::actions::kanban::KanbanUi::default());
    let ui = AppWindow::new().expect("headless window");
    let chat_ui = Arc::new(Mutex::new(ChatUiState::default()));
    let mut sv = SessionView::default();
    sv.settings.wake_on = vec!["vote_pending".to_string()];
    apply_session(&ui, &sv, true, &chat_ui);
    ui.set_node_member("a".into());
    let applied = molt_core::Event::Applied { id: molt_core::ProposalId(1), surface: Surface::Quests };
    crate::actions::kanban::notice_event(&ui, &applied);
    let take = || with(|st| st.notices.take(&crate::i18n::Lexicon::en(), "poked"));
    assert_eq!(take(), None, "kanban is switched off");
    let proposed = molt_core::Event::Proposed { id: molt_core::ProposalId(2), surface: Surface::Quests, by: "b".into() };
    crate::actions::kanban::notice_event(&ui, &proposed);
    assert_eq!(take().as_deref(), Some("Vote waiting"));
    let all = molt_core::kanban_wake::WAKE_REASONS.iter().map(|r| (*r).to_string()).collect();
    sv.settings.wake_on = all;
    apply_session(&ui, &sv, true, &chat_ui);
}

/// The draft saves reach the engine in the order they were made.
#[test]
fn draft_saves_land_in_order() {
    let rt = rt();
    let saver = std::sync::Arc::new(crate::actions::kanban::DraftSaver::default());
    let written = std::sync::Arc::new(Mutex::new(Vec::new()));
    let first = saver.next();
    let second = saver.next();
    let w = written.clone();
    rt.block_on(saver.save(second, || async move { w.lock().expect("lock").push(2) }));
    let w = written.clone();
    rt.block_on(saver.save(first, || async move { w.lock().expect("lock").push(1) }));
    assert_eq!(*written.lock().expect("lock"), [2], "an older save never lands after a newer one");
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

/// §9 S5: a wiki page's `quest:` link opens the task, and the task's
/// drill-in names the pages that link it.
#[test]
fn a_wiki_quest_link_and_its_backlink_lead_both_ways() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node_with(tmp.path(), &rt, &["memory", "quests"]);
    let k = node.ui.global::<Kanban>();
    form_new(&node.ui);
    k.set_f_title("drill".into());
    assert!(form_submit(&node.ui));
    propose_basket(&node, &rt);
    let id = cards(&k.get_todo())[0].id.to_string();
    let page = format!(
        "diff --git a/plan.md b/plan.md\nnew file mode 100644\n--- /dev/null\n+++ b/plan.md\n@@ -0,0 +1,1 @@\n+Do [the drill](quest:{}) first.\n",
        &id[..8]
    );
    let payload = serde_json::json!({"op": "wiki_patch", "value": page, "summary": "plan"});
    rt.block_on(node.w.execute(Command::Propose { surface: Surface::Memory, payload }))
        .expect("the page is proposed");

    k.set_sel(id.as_str().into());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let cited = loop {
        node.mirror(&rt);
        let cited: Vec<String> = k.get_cited_by().iter().map(|l| l.key.to_string()).collect();
        if !cited.is_empty() || std::time::Instant::now() > deadline {
            break cited;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(cited, ["plan.md"]);

    let _wiki = crate::wiki_bridge::wire_wiki(&node.ui);
    k.set_sel("".into());
    node.ui
        .global::<crate::WikiState>()
        .invoke_open_link(format!("quest:{}", &id[..8]).into());
    assert_eq!(k.get_sel().as_str(), id, "the prefix opens the task");
    k.set_sel("".into());
    node.ui.global::<crate::WikiState>().invoke_open_link("quest:ffffffff".into());
    assert_eq!(k.get_sel().as_str(), "", "a dead link stays put");
    node.ui
        .global::<crate::WikiState>()
        .invoke_open_link(format!("quest:{}", &id[..4]).into());
    assert_eq!(k.get_sel().as_str(), "", "a prefix under 8 hex is no link");
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
    quests_screen(ui, "board")
}

#[cfg(feature = "live-preview")]
fn quests_screen(ui: &AppWindow, view: &str) -> Shown {
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("quests".into());
    ui.set_selected_view(view.into());
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

/// §8: the New task button and the basket bar on every kanban view, the
/// proposals list included.
#[cfg(feature = "live-preview")]
#[test]
fn the_proposals_view_keeps_new_task_and_the_basket() {
    type H = i_slint_backend_testing::ElementHandle;
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    crate::actions::kanban::wire(&node.ui, &node.ctx(&rt));
    let k = node.ui.global::<Kanban>();
    form_new(&node.ui);
    k.set_f_title("staged".into());
    assert!(form_submit(&node.ui));
    let _shown = quests_screen(&node.ui, "proposals");
    assert!(H::find_by_accessible_label(&node.ui, "New task").next().is_some(), "New task");
    assert!(
        H::find_by_accessible_label(&node.ui, "🧺 1 changes").next().is_some(),
        "the basket bar"
    );
}

/// Two tasks through the vote: one floating with a deadline, one timed
/// today (UTC, the engine's clock).
fn planned_node(root: &std::path::Path, rt: &tokio::runtime::Runtime) -> (Node, String) {
    let node = quests_node(root, rt);
    let k = node.ui.global::<Kanban>();
    let today = chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string();
    form_new(&node.ui);
    k.set_f_title("ship".into());
    k.set_f_due("2099-01-01".into());
    assert!(form_submit(&node.ui));
    form_new(&node.ui);
    k.set_f_title("meet".into());
    k.set_f_mode(1);
    k.set_f_start(today.as_str().into());
    k.set_f_end(today.as_str().into());
    assert!(form_submit(&node.ui));
    propose_basket(&node, rt);
    (node, today)
}

/// §8: plan, dependencies and calendar render the engine's board.
#[test]
fn the_views_render_the_board() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (node, today) = planned_node(tmp.path(), &rt);
    let k = node.ui.global::<Kanban>();
    let titles = |rows: Vec<String>| rows.into_iter().collect::<std::collections::BTreeSet<_>>();
    let plan: Vec<String> = k.get_plan_rows().iter().map(|r| r.title.to_string()).collect();
    assert_eq!(titles(plan), ["meet".to_string(), "ship".to_string()].into());
    assert_eq!(k.get_plan_bars().row_count(), 2, "a marker and a block");
    let deps: Vec<String> = k.get_deps().iter().map(|r| r.title.to_string()).collect();
    assert_eq!(titles(deps), ["meet".to_string(), "ship".to_string()].into());
    let blocks: Vec<_> = k.get_cal_blocks().iter().collect();
    assert_eq!(blocks.len(), 1, "only the timed task");
    let b = &blocks[0];
    assert_eq!(b.title.as_str(), "meet");
    let day = k.get_cal_days().row_data(usize::try_from(b.day).expect("day")).expect("a day");
    assert_eq!(day.date.as_str(), today, "where the form put it");
}

/// No feed: nothing of the previous board stays in any view.
#[test]
fn a_dropped_feed_clears_every_view() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (node, _) = planned_node(tmp.path(), &rt);
    let k = node.ui.global::<Kanban>();
    assert!(k.get_plan_legend().row_count() + k.get_plan_ticks().row_count() > 0);
    with(|st| st.feed = None);
    render(&node.ui);
    let counts = [
        k.get_plan_rows().row_count(),
        k.get_plan_bars().row_count(),
        k.get_plan_arrows().row_count(),
        k.get_plan_ticks().row_count(),
        k.get_plan_legend().row_count(),
        k.get_deps().row_count(),
        k.get_deps_legend().row_count(),
        k.get_cal_days().row_count(),
        k.get_cal_blocks().row_count(),
        k.get_cal_legend().row_count(),
        k.get_cal_weekdays().row_count(),
    ];
    assert_eq!(counts, [0; 11]);
    assert_eq!(k.get_plan_nodate(), -1);
    assert!(k.get_plan_today() < 0.0);
    assert_eq!(k.get_cal_title().as_str(), "");
}

/// §8: dragging a calendar block stages a `set`; a free slot opens the
/// form at that slot.
#[test]
fn the_calendar_stages_a_move_and_opens_a_free_slot() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (node, _) = planned_node(tmp.path(), &rt);
    let k = node.ui.global::<Kanban>();
    let b = k.get_cal_blocks().row_data(0).expect("meet");
    let target = if b.day > 0 { b.day - 1 } else { b.day + 1 };
    let src = usize::try_from(b.src).expect("draggable");
    assert!(crate::actions::kanban::cal_drop(&node.ui, src, usize::try_from(target).expect("day"), -1));
    assert_eq!(k.get_basket_count(), 1);
    let shadows: Vec<i32> = k.get_cal_blocks().iter().filter(|b| b.shadow).map(|b| b.day).collect();
    assert_eq!(shadows, [target], "the shadow at the target, the block stays");
    let row = k.get_basket_rows().row_data(0).expect("the staged move").text.to_string();
    assert!(row.contains(&short_id(&b.id)) && row.contains("when"), "{row}");

    let day = k.get_cal_days().row_data(3).expect("a day").date.to_string();
    crate::actions::kanban::cal_new(&node.ui, 3, 9 * 60, 3, 9 * 60);
    assert!(k.get_form_open());
    assert_eq!(k.get_f_mode(), 1);
    assert_eq!(k.get_f_start().as_str(), format!("{day}T09:00"));
    assert_eq!(k.get_f_end().as_str(), format!("{day}T10:00"));
}

/// §2.3: Organization › Status carries the UTC clock.
#[test]
fn the_status_shows_the_utc_clock() {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    let ui = AppWindow::new().expect("headless window");
    crate::actions::kanban::start_clock(&ui);
    let now = ui.get_org_utc_now().to_string();
    assert!(now.ends_with(" UTC") && now.len() == "2026-10-09 14:32 UTC".len(), "{now}");
}

/// A real pointer drag of a month block onto the neighbouring day.
#[cfg(feature = "live-preview")]
#[test]
fn a_real_drag_in_the_month_stages_a_move() {
    type H = i_slint_backend_testing::ElementHandle;
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (node, _) = planned_node(tmp.path(), &rt);
    crate::actions::kanban::wire(&node.ui, &node.ctx(&rt));
    let k = node.ui.global::<Kanban>();
    let b = k.get_cal_blocks().row_data(0).expect("meet");
    let _shown = quests_screen(&node.ui, "calendar");
    let chip = H::find_by_accessible_label(&node.ui, "meet")
        .find(|h| h.size().height < 40.0)
        .expect("the block renders");
    let cell = H::find_by_accessible_label(&node.ui, &k.get_cal_days().row_data(0).expect("day").date)
        .next()
        .expect("a day cell");
    let step = if b.day % 7 == 6 { -cell.size().width } else { cell.size().width };
    let pos = chip.absolute_position();
    let from = slint::LogicalPosition::new(pos.x + 10.0, pos.y + chip.size().height / 2.0);
    let to = slint::LogicalPosition::new(from.x + step, from.y);
    let win = node.ui.window();
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: from });
    win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: from,
        button: slint::platform::PointerEventButton::Left,
    });
    for n in 1..=10 {
        let x = from.x + (to.x - from.x) * n as f32 / 10.0;
        win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: slint::LogicalPosition::new(x, from.y) });
    }
    win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: to,
        button: slint::platform::PointerEventButton::Left,
    });
    assert_eq!(k.get_basket_count(), 1, "the drop staged one move");
    let want = if b.day % 7 == 6 { b.day - 1 } else { b.day + 1 };
    let shadows: Vec<i32> = k.get_cal_blocks().iter().filter(|b| b.shadow).map(|b| b.day).collect();
    assert_eq!(shadows, [want]);
    assert_eq!(k.get_sel().as_str(), "", "a drag is not a click");
}

/// The plan and dependency views render their rows; a fold opens a task.
#[cfg(feature = "live-preview")]
#[test]
fn plan_and_dependencies_render_and_fold() {
    type H = i_slint_backend_testing::ElementHandle;
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let node = quests_node(tmp.path(), &rt);
    crate::actions::kanban::wire(&node.ui, &node.ctx(&rt));
    let k = node.ui.global::<Kanban>();
    form_new(&node.ui);
    k.set_f_title("base".into());
    assert!(form_submit(&node.ui));
    propose_basket(&node, &rt);
    let base = k.get_deps().row_data(0).expect("base").id.to_string();
    form_new(&node.ui);
    k.set_f_title("top".into());
    k.set_f_due("2099-01-01".into());
    with(|st| st.form.blocked = vec![base]);
    assert!(form_submit(&node.ui));
    propose_basket(&node, &rt);

    let _shown = quests_screen(&node.ui, "plan");
    assert!(H::find_by_accessible_label(&node.ui, "top").next().is_some(), "the plan row");
    assert!(H::find_by_accessible_label(&node.ui, "base").next().is_some(), "open by default");
    assert_eq!(k.get_plan_arrows().row_count(), 1);
    drop(_shown);

    let _shown = quests_screen(&node.ui, "dependencies");
    assert_eq!(k.get_deps().row_count(), 1, "only the top, collapsed");
    let fold = H::find_by_element_type_name(&node.ui, "KFold").next().expect("the fold");
    click(&node.ui, &fold);
    assert_eq!(k.get_deps().row_count(), 2, "opened");
    assert_eq!(k.get_deps().row_data(1).expect("base").title.as_str(), "base");
}

/// §8: a real drag over free month cells opens the form with those days.
#[cfg(feature = "live-preview")]
#[test]
fn a_real_drag_over_free_days_opens_the_form() {
    type H = i_slint_backend_testing::ElementHandle;
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (node, _) = planned_node(tmp.path(), &rt);
    crate::actions::kanban::wire(&node.ui, &node.ctx(&rt));
    let k = node.ui.global::<Kanban>();
    let _shown = quests_screen(&node.ui, "calendar");
    let date = |i: usize| k.get_cal_days().row_data(i).expect("a day").date.to_string();
    let spot = |i: usize| {
        let cell = H::find_by_accessible_label(&node.ui, &date(i)).next().expect("a day cell");
        let p = cell.absolute_position();
        slint::LogicalPosition::new(p.x + cell.size().width / 2.0, p.y + cell.size().height - 6.0)
    };
    let (from, to) = (spot(2), spot(0));
    let win = node.ui.window();
    win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: from });
    win.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position: from,
        button: slint::platform::PointerEventButton::Left,
    });
    for n in 1..=10 {
        let x = from.x + (to.x - from.x) * n as f32 / 10.0;
        win.dispatch_event(slint::platform::WindowEvent::PointerMoved { position: slint::LogicalPosition::new(x, from.y) });
    }
    win.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position: to,
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(k.get_form_open());
    assert_eq!(k.get_f_mode(), 1);
    assert_eq!((k.get_f_start().to_string(), k.get_f_end().to_string()), (date(0), date(2)));
}
