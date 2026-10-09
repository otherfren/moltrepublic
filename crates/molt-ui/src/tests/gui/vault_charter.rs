// SPDX-License-Identifier: GPL-3.0-or-later
//! The vault (vault spec §3, D11): the wizard box is off by default and
//! obeys `2 <= m <= n-2`, the charter echo shows the signed selection,
//! and the Organization modal offers it only where a vote can enable it.

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

/// Does `e` carry a `Text` labelled `label` among its descendants?
fn has_text(e: &Handle, label: &str) -> bool {
    let want = label.to_string();
    e.query_descendants()
        .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == want))
        .find_first()
        .is_some()
}

/// The `AppCheck` rows labelled `label` inside `scope`.
fn checks_in(scope: &Handle, label: &str) -> Vec<Handle> {
    scope
        .query_descendants()
        .match_type_name("AppCheck")
        .find_all()
        .into_iter()
        .filter(|c| has_text(c, label))
        .collect()
}

fn window_root(ui: &AppWindow) -> Handle {
    i_slint_backend_testing::ElementRoot::root_element(ui)
}

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(300));
}

/// The founder's charter step of an `m`-of-`n` founding, every seat in.
/// The caller initializes the backend (once per test thread).
fn charter_step(m: i32, n: i32) -> (AppWindow, Shown) {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 1800));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_name("a".into());
    ui.set_cw_member("a".into());
    ui.set_cw_threshold(m);
    ui.set_cw_members(n);
    ui.set_cw_step(1);
    let seats: Vec<RitualSeat> = (0..n)
        .map(|i| RitualSeat {
            member: format!("seat {i}").into(),
            detail: String::new().into(),
            state: 1,
        })
        .collect();
    ui.set_cw_seats(ModelRc::new(VecModel::from(seats)));
    ui.set_cw_total(n);
    ui.set_cw_can_propose(true);
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    settle();
    (ui, shown)
}

fn wizard_vault_box(ui: &AppWindow) -> Handle {
    let label = ui.global::<Strings>().get_feat_vault().to_string();
    let mut found = checks_in(&window_root(ui), &label);
    assert_eq!(found.len(), 1, "one vault box on the charter step");
    found.remove(0)
}

#[test]
fn the_vault_box_is_disabled_outside_2_to_n_minus_2() {
    i_slint_backend_testing::init_no_event_loop();
    for (m, n, allowed) in [(2, 3, false), (2, 4, true), (3, 4, false), (1, 4, false), (3, 5, true)] {
        let (ui, _shown) = charter_step(m, n);
        let s = ui.global::<Strings>();
        let low = s.get_feat_vault_low().to_string();
        let high = s.get_feat_vault_high().to_string();
        click(&ui, &wizard_vault_box(&ui));
        assert_eq!(ui.get_cw_feat_vault(), allowed, "{m}-of-{n}: a click ticks the box iff allowed");
        let shown = |t: &str| Handle::find_by_accessible_label(&ui, t).next().is_some();
        assert_eq!(shown(&low), !allowed && m < 2, "{m}-of-{n}: the low line");
        assert_eq!(shown(&high), !allowed && m >= 2, "{m}-of-{n}: the high line");
    }
}

/// The reason sits on the vault box's own line, not below the grid.
#[test]
fn the_vault_reason_sits_beside_the_box() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = charter_step(3, 4);
    let high = ui.global::<Strings>().get_feat_vault_high().to_string();
    let reason = Handle::find_by_accessible_label(&ui, &high).next().expect("reason shown");
    let check = wizard_vault_box(&ui);
    let (cy, ch) = (check.absolute_position().y, check.size().height);
    let (ry, rh) = (reason.absolute_position().y, reason.size().height);
    let mid = ry + rh / 2.0;
    assert!(mid > cy && mid < cy + ch, "reason centre {mid} within the box row {cy}..{}", cy + ch);
    assert!(reason.absolute_position().x > check.absolute_position().x, "right of the box");
}

/// E1: every founding prepares the vault; ticking it is opt-in.
#[test]
fn the_vault_box_is_off_by_default() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = charter_step(2, 4);
    assert!(!ui.get_cw_feat_vault(), "unticked");
    assert_eq!(crate::actions::ritual::charter_features(&ui), vec!["memory".to_string()]);
}

#[test]
fn a_ticked_vault_reaches_create_propose() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = charter_step(2, 4);
    assert_eq!(crate::actions::ritual::charter_features(&ui), vec!["memory".to_string()]);
    click(&ui, &wizard_vault_box(&ui));
    assert_eq!(
        crate::actions::ritual::charter_features(&ui),
        vec!["memory".to_string(), "vault".to_string()]
    );
}

#[test]
fn the_charter_echo_shows_the_vault() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 1800));
    ui.set_screen(AppScreen::Create);
    apply_strings(&ui, 0);
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let sv = SessionView {
        screen: molt_core::Screen::Create,
        create: molt_core::CreateState {
            run: molt_core::RunCore { step: 1, ..molt_core::RunCore::default() },
            name: "a".to_string(),
            features: vec!["memory".to_string(), "vault".to_string()],
            can_propose: true,
            ..molt_core::CreateState::default()
        },
        join: molt_core::JoinState {
            proposed_features: Some(vec!["vault".to_string()]),
            ..molt_core::JoinState::default()
        },
        ..SessionView::default()
    };
    apply_session(&ui, &sv, true, &chat_ui);
    assert!(ui.get_jw_feat_vault(), "the joiner echo reads the proposed list");
    // the local box stays unticked: the founder echo must read the
    // proposed list, not the form
    assert!(!ui.get_cw_feat_vault());
    let _shown = show_headless(&ui);
    ui.set_cw_proposed(true);
    settle();
    ui.set_cw_fold(2);
    settle();
    let view = Handle::find_by_element_type_name(&ui, "CharterView")
        .next()
        .expect("the charter fold renders its CharterView");
    let label = ui.global::<Strings>().get_feat_vault().to_string();
    let vault = checks_in(&view, &label);
    assert_eq!(vault.len(), 1, "the echo has one vault row");
    assert!(has_text(&vault[0], "✓"), "the founder echo shows the proposed vault");
}

type Proposed = Rc<RefCell<Vec<(String, String)>>>;

thread_local! {
    static BACKEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `StatusView.vault_enable` as the window carries it.
const ON: i32 = 0;
const OFFER: i32 = 1;
const NEWER: i32 = 2;
const BOUNDS: i32 = 3;

/// The Organization features modal over a republic whose vault switch
/// reads `enable` (on: the vault is in the effective selection).
fn org_modal(enable: i32) -> (AppWindow, Shown, Proposed) {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1100, 800));
    ui.set_org_feat_vault(enable == ON);
    ui.set_org_vault_enable(enable);
    let got: Proposed = Rc::new(RefCell::new(Vec::new()));
    let sink = got.clone();
    ui.on_org_propose(move |op, value| {
        sink.borrow_mut().push((op.to_string(), value.to_string()));
    });
    let shown = show_headless(&ui);
    ui.set_org_features_modal_open(true);
    settle();
    (ui, shown, got)
}

fn modal(ui: &AppWindow) -> Handle {
    Handle::find_by_element_type_name(ui, "ConfirmModal")
        .next()
        .expect("the features dialog is up")
}

/// Click the dialog's propose button (its label, inside the button).
fn confirm(ui: &AppWindow) {
    let label = ui.global::<Strings>().get_oc_propose().to_string();
    let button = modal(ui)
        .query_descendants()
        .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == label))
        .find_first()
        .expect("the propose button");
    click(ui, &button);
    settle();
}

/// E4: where the vault cannot be voted in, the row names why - and
/// offers no box.
#[test]
fn the_org_modal_names_why_the_vault_is_locked() {
    for enable in [NEWER, BOUNDS] {
        let (ui, _shown, got) = org_modal(enable);
        let s = ui.global::<Strings>();
        let dlg = modal(&ui);
        assert!(
            checks_in(&dlg, &s.get_feat_vault()).is_empty()
                && checks_in(&dlg, &format!("{}{}", s.get_feat_vault(), s.get_feat_mock())).is_empty(),
            "no vault checkbox in the dialog"
        );
        let why = if enable == NEWER { s.get_vault_newer_republic() } else { s.get_feat_vault_bounds() };
        assert!(has_text(&dlg, &why), "the vault row says {why}");
        confirm(&ui);
        assert!(got.borrow().is_empty(), "nothing to propose");
    }
}

/// E4: a prepared republic within the bounds votes the vault in from the
/// modal.
#[test]
fn the_org_modal_offers_the_vault_when_it_can_be_enabled() {
    let (ui, _shown, got) = org_modal(OFFER);
    let vault = ui.global::<Strings>().get_feat_vault().to_string();
    let mut boxes = checks_in(&modal(&ui), &vault);
    assert_eq!(boxes.len(), 1, "one vault box");
    click(&ui, &boxes.remove(0));
    confirm(&ui);
    let got = got.borrow();
    assert_eq!(got.len(), 1, "one proposal");
    assert_eq!(got[0].0, "set_features");
    assert!(got[0].1.split_whitespace().any(|k| k == "vault"), "{}", got[0].1);
}

#[test]
fn an_org_proposal_in_a_vault_republic_carries_no_vault() {
    let (ui, _shown, got) = org_modal(ON);
    let memory = ui.global::<Strings>().get_feat_memory().to_string();
    let mut boxes = checks_in(&modal(&ui), &memory);
    assert_eq!(boxes.len(), 1, "one memory box");
    click(&ui, &boxes.remove(0));
    confirm(&ui);
    let got = got.borrow();
    assert_eq!(got.len(), 1, "one proposal");
    assert_eq!(got[0].0, "set_features");
    let keys: Vec<&str> = got[0].1.split_whitespace().collect();
    assert_eq!(keys, vec!["memory"], "the vault never rides a set_features value");
}

/// S4: the Kanban is real - the wizard box is opt-in and reaches
/// `CreatePropose`.
#[test]
fn a_ticked_kanban_reaches_create_propose() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = charter_step(2, 4);
    let label = ui.global::<Strings>().get_feat_quests().to_string();
    let mut found = checks_in(&window_root(&ui), &label);
    assert_eq!(found.len(), 1, "one kanban box on the charter step");
    assert!(!ui.get_cw_feat_quests(), "off by default");
    click(&ui, &found.remove(0));
    assert_eq!(
        crate::actions::ritual::charter_features(&ui),
        vec!["memory".to_string(), "quests".to_string()]
    );
}

/// S4: the Organization modal votes the Kanban in.
#[test]
fn the_org_modal_votes_the_kanban_in() {
    let (ui, _shown, got) = org_modal(NEWER);
    let quests = ui.global::<Strings>().get_feat_quests().to_string();
    let mut boxes = checks_in(&modal(&ui), &quests);
    assert_eq!(boxes.len(), 1, "one kanban box");
    click(&ui, &boxes.remove(0));
    confirm(&ui);
    let got = got.borrow();
    assert_eq!(got.len(), 1, "one proposal");
    let keys: Vec<&str> = got[0].1.split_whitespace().collect();
    assert_eq!(keys, vec!["quests"]);
}
