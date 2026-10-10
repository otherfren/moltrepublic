// SPDX-License-Identifier: GPL-3.0-or-later
//! The purse on screen (wallet plan §11): the warning beside the address,
//! the founding wizard's last step, the wizard checkbox (W9) and the
//! panel of a later enable (W10). Element lookups: live-preview only.

use molt_core::wallet::{RunStage, WalletPhase, WalletRunView, WalletView};

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

thread_local! {
    static BACKEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn backend() {
    if !BACKEND.with(|b| b.replace(true)) {
        i_slint_backend_testing::init_no_event_loop();
    }
}

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(300));
}

/// Does `e` carry a `Text` labelled `label` among its descendants?
fn has_text(e: &Handle, label: &str) -> bool {
    let want = label.to_string();
    e.query_descendants()
        .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == want))
        .find_first()
        .is_some()
}

fn shown(ui: &AppWindow, label: &str) -> bool {
    Handle::find_by_accessible_label(ui, label).next().is_some()
}

fn one(ui: &AppWindow, label: &str) -> Handle {
    Handle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("{label:?} on screen"))
}

const ADDRESS: &str = "44AFFq5kSiGBoZ4NMDwYtN18obc8AemS33DBLWs3H7otXft3XjrpDtQGv7SqSsaBYBb98uNbr2VBBEt7f2wfn3RVGQBEP3A";

fn ready() -> WalletView {
    WalletView {
        address: ADDRESS.to_string(),
        network: "mainnet".to_string(),
        balance: 1_250_000_000_000,
        threshold: 2,
        participants: 3,
        phase: WalletPhase::Ready,
        can_watch: true,
        connected: true,
        ..WalletView::default()
    }
}

fn readiness(needs_consent: bool) -> WalletRunView {
    WalletRunView {
        stage: RunStage::Ready,
        done: 1,
        of: 3,
        missing: vec!["b".to_string(), "c".to_string()],
        needs_consent,
        ..WalletRunView::default()
    }
}

/// The main window on Wallet › `view`, `v` applied.
fn wallet_screen(view: &str, v: &WalletView) -> (AppWindow, Shown) {
    backend();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "wallet".into(),
        gated: true,
        ..SurfaceTab::default()
    }])));
    ui.set_selected_surface("wallet".into());
    ui.set_selected_view(view.into());
    crate::wallet::apply_wallet(&ui, 0, Some(v));
    let shown = show_headless(&ui);
    settle();
    (ui, shown)
}

/// W11: large, directly above the address, not dismissable.
#[test]
fn the_warning_sits_beside_the_address() {
    let (ui, _shown) = wallet_screen("receive", &ready());
    let warning = one(&ui, &ui.global::<Strings>().get_wl_warning());
    let address = one(&ui, ADDRESS);
    let (w, a) = (warning.absolute_position(), address.absolute_position());
    let w_bottom = w.y + warning.size().height;
    assert!(w_bottom <= a.y + 0.5, "the warning {w_bottom} above the address {}", a.y);
    assert!(a.y - w_bottom < 40.0, "directly above: gap {}", a.y - w_bottom);
    assert!((w.x - a.x).abs() < 20.0, "one column: {} vs {}", w.x, a.x);
    let s = ui.global::<Strings>();
    assert!(!shown(&ui, &s.get_set_close()), "nothing closes it");
}

/// U1-U2: without a purse the balance offers the set-up, with one the
/// figure; pending only when > 0.
#[test]
fn the_balance_shows_the_figure_or_the_set_up() {
    let (ui, _shown) = wallet_screen("balance", &ready());
    assert!(shown(&ui, "1.25 XMR"));
    assert!(!shown(&ui, &ui.global::<Purse>().get_pending()) || ui.global::<Purse>().get_pending().is_empty());
    let none = WalletView {
        phase: WalletPhase::NoPurse,
        address: String::new(),
        ..ready()
    };
    let (ui, _shown) = wallet_screen("balance", &none);
    let set_up = ui.global::<Strings>().get_wl_set_up().to_string();
    assert!(shown(&ui, &set_up), "the set-up button");
    assert!(!shown(&ui, "0 XMR"));
}

/// Send is locked with one line.
#[test]
fn send_is_one_locked_line() {
    let (ui, _shown) = wallet_screen("send", &ready());
    assert!(shown(&ui, &ui.global::<Strings>().get_wl_send_locked()));
}

/// The founder's sealed wizard of a founding with the purse.
fn sealed_wizard(v: &WalletView) -> (AppWindow, Shown) {
    backend();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 1800));
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Create);
    ui.set_cw_name("a".into());
    ui.set_cw_step(1);
    ui.set_cw_outcome(1);
    crate::wallet::apply_wallet(&ui, 0, Some(v));
    let shown = show_headless(&ui);
    settle();
    (ui, shown)
}

/// W9: after the seal the wizard moves on to the purse stage by itself:
/// the node first where none is set, then the bar; Enter stays open.
#[test]
fn the_wizard_ends_with_the_purse_stage() {
    let founding = WalletView {
        phase: WalletPhase::Init,
        founding: true,
        run: Some(WalletRunView {
            daemon_hint: "http://abc.onion:18081".to_string(),
            ..readiness(false)
        }),
        ..WalletView::default()
    };
    let (ui, _shown) = sealed_wizard(&founding);
    let s = ui.global::<Strings>();
    assert!(shown(&ui, &s.get_wl_stage_title()), "the purse stage");
    assert!(shown(&ui, &s.get_wl_choose_node()), "no node yet: choose one");
    assert_eq!(ui.global::<Purse>().get_node_draft(), "http://abc.onion:18081", "prefilled");
    assert!(shown(&ui, &s.get_enter_republic()), "Enter republic stays open");

    ui.global::<Purse>().set_node("http://abc.onion:18081".into());
    settle();
    assert!(!shown(&ui, &s.get_wl_choose_node()));
    assert!(shown(&ui, "Waiting for members 1/3"));
    assert!(shown(&ui, "Missing: b, c"));

    let done = WalletView { founding: true, ..ready() };
    crate::wallet::apply_wallet(&ui, 0, Some(&done));
    settle();
    assert!(shown(&ui, ADDRESS) && shown(&ui, &s.get_wl_warning()), "the address and its warning end it");

    let plain = WalletView { phase: WalletPhase::NoPurse, ..WalletView::default() };
    let (ui, _shown) = sealed_wizard(&plain);
    assert!(!shown(&ui, &ui.global::<Strings>().get_wl_stage_title()), "no purse in the charter: ends at the seal");
}

/// W10: a later enable shows the stage as a panel over the main window,
/// asks the consent, says everyone must be online, and Try again after
/// an abort.
#[test]
fn a_late_enable_shows_the_panel() {
    let view = WalletView {
        phase: WalletPhase::Init,
        run: Some(readiness(true)),
        threshold: 2,
        participants: 3,
        ..WalletView::default()
    };
    let (ui, _shown) = wallet_screen("history", &view);
    ui.global::<Purse>().set_node("http://abc.onion:18081".into());
    let said: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = said.clone();
    ui.global::<Purse>().on_consent(move |a| sink.borrow_mut().push(a));
    settle();
    let s = ui.global::<Strings>();
    let panel = Handle::find_by_element_type_name(&ui, "ConfirmModal")
        .next()
        .expect("the panel is up");
    assert!(has_text(&panel, &s.get_wl_online()), "everyone must be online");
    assert!(has_text(&panel, &s.get_wl_note_sees()), "what the consent covers");
    let agree = s.get_wl_agree().to_string();
    let button = panel
        .query_descendants()
        .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == agree))
        .find_first()
        .expect("Agree");
    click(&ui, &button);
    assert_eq!(*said.borrow(), vec![true]);

    // closed, it stays closed for this run, and comes back for the next
    ui.global::<Purse>().set_panel_dismissed(true);
    settle();
    assert!(Handle::find_by_element_type_name(&ui, "ConfirmModal").next().is_none());
    let mut aborted = view.clone();
    aborted.run = Some(WalletRunView {
        stage: RunStage::Aborted,
        reason: Some("declined".to_string()),
        missing: vec!["b".to_string()],
        of: 3,
        ..WalletRunView::default()
    });
    crate::wallet::apply_wallet(&ui, 0, Some(&aborted));
    crate::wallet::apply_wallet(&ui, 0, Some(&view));
    settle();
    assert!(Handle::find_by_element_type_name(&ui, "ConfirmModal").next().is_some(), "a new run reopens it");

    crate::wallet::apply_wallet(&ui, 0, Some(&aborted));
    settle();
    assert!(shown(&ui, "b declined"));
    assert!(shown(&ui, &s.get_wl_try_again()));
}

fn charter_step(m: i32, n: i32) -> (AppWindow, Shown) {
    backend();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 1800));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_name("a".into());
    ui.set_cw_member("a".into());
    ui.set_cw_threshold(m);
    ui.set_cw_members(n);
    ui.set_cw_step(1);
    ui.set_cw_total(n);
    ui.set_cw_can_propose(true);
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    settle();
    (ui, shown)
}

/// W9: checked by default where 2 <= m <= n-1, else locked off with one
/// line of reason; ticked, the three consent lines sit below.
#[test]
fn the_wallet_checkbox_defaults_on_within_bounds() {
    for (m, n, allowed) in [(2, 3, true), (3, 4, true), (1, 3, false), (3, 3, false)] {
        let (ui, _shown) = charter_step(m, n);
        let s = ui.global::<Strings>();
        let wallet = crate::actions::ritual::charter_features(&ui).contains(&"wallet".to_string());
        assert_eq!(wallet, allowed, "{m}-of-{n}: wallet in the charter");
        assert_eq!(shown(&ui, &s.get_wl_note_lost()), allowed, "{m}-of-{n}: the consent lines");
        assert_eq!(shown(&ui, &s.get_wl_bounds_low()), !allowed && m < 2, "{m}-of-{n}: low");
        assert_eq!(shown(&ui, &s.get_wl_bounds_high()), !allowed && m >= 2, "{m}-of-{n}: high");
    }
}

/// The organization dialog's purse box proposes the set-up vote (W4),
/// never a set_features value; outside the bounds it names why.
#[test]
fn the_org_modal_proposes_the_set_up() {
    for phase in [WalletPhase::NoPurse, WalletPhase::Bounds] {
        backend();
        let ui = AppWindow::new().expect("headless window");
        apply_strings(&ui, 0);
        ui.window().set_size(slint::PhysicalSize::new(1100, 800));
        ui.set_org_feat_memory(true);
        crate::wallet::apply_wallet(&ui, 0, Some(&WalletView { phase, ..WalletView::default() }));
        let proposed: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = proposed.clone();
        ui.on_org_propose(move |op, _| sink.borrow_mut().push(op.to_string()));
        let set_up = Rc::new(std::cell::Cell::new(0));
        let count = set_up.clone();
        ui.global::<Purse>().on_set_up(move || count.set(count.get() + 1));
        let _shown = show_headless(&ui);
        ui.set_org_features_modal_open(true);
        settle();
        let s = ui.global::<Strings>();
        let dlg = Handle::find_by_element_type_name(&ui, "ConfirmModal").next().expect("the dialog");
        let label = s.get_feat_wallet().to_string();
        let boxes: Vec<Handle> = dlg
            .query_descendants()
            .match_type_name("AppCheck")
            .find_all()
            .into_iter()
            .filter(|c| has_text(c, &label))
            .collect();
        if phase == WalletPhase::Bounds {
            assert!(boxes.is_empty(), "no purse box outside the bounds");
            assert!(has_text(&dlg, &s.get_wl_bounds()), "the reason");
            continue;
        }
        assert_eq!(boxes.len(), 1, "one purse box");
        click(&ui, &boxes[0]);
        let propose = s.get_oc_propose().to_string();
        let button = dlg
            .query_descendants()
            .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == propose))
            .find_first()
            .expect("the propose button");
        click(&ui, &button);
        settle();
        assert_eq!(set_up.get(), 1, "the set-up vote");
        assert!(proposed.borrow().is_empty(), "no set_features: {:?}", proposed.borrow());
    }
}
