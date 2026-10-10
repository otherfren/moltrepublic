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

/// Is any element's label on screen one `pred` accepts?
fn any_text(ui: &AppWindow, pred: impl Fn(&str) -> bool + 'static) -> bool {
    i_slint_backend_testing::ElementQuery::from_root(ui)
        .match_descendants()
        .match_predicate(move |d| d.accessible_label().is_some_and(|l| pred(l.as_str())))
        .find_first()
        .is_some()
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
    crate::wallet::apply_wallet(&ui, 0, "w", Some(v));
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
    let pending = ui.global::<Strings>().get_wl_pending().to_string();
    assert!(!any_text(&ui, move |l| l.ends_with(&pending)), "nothing pending: no line");
    let (ui, _shown) = wallet_screen("balance", &WalletView { pending: 500_000_000_000, ..ready() });
    assert!(shown(&ui, "0.5 XMR pending"));
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
    crate::wallet::apply_wallet(&ui, 0, "w", Some(v));
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
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&done));
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
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&aborted));
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&view));
    settle();
    assert!(Handle::find_by_element_type_name(&ui, "ConfirmModal").next().is_some(), "a new run reopens it");

    crate::wallet::apply_wallet(&ui, 0, "w", Some(&aborted));
    settle();
    assert!(shown(&ui, "b declined"));
    assert!(shown(&ui, &s.get_wl_try_again()));

    // the purse committed: the panel stays with the address until closed
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&view));
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&ready()));
    settle();
    let panel = Handle::find_by_element_type_name(&ui, "ConfirmModal").next().expect("still up");
    assert!(has_text(&panel, ADDRESS) && has_text(&panel, &s.get_wl_warning()));
    ui.global::<Purse>().set_panel_dismissed(true);
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&ready()));
    settle();
    assert!(Handle::find_by_element_type_name(&ui, "ConfirmModal").next().is_none(), "closed for good");
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
        crate::wallet::apply_wallet(&ui, 0, "w", Some(&WalletView { phase, ..WalletView::default() }));
        ui.global::<Purse>().set_node("http://abc.onion:18081".into());
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

/// W9: the joiner's wizard ends with the same purse stage after its seal.
#[test]
fn the_joiners_wizard_ends_with_the_purse_stage() {
    backend();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 1800));
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Join);
    ui.set_jw_step(1);
    ui.set_jw_republic("a".into());
    ui.set_jw_feat_wallet(true);
    ui.set_jw_sealed(true);
    let founding = WalletView {
        phase: WalletPhase::Init,
        founding: true,
        run: Some(readiness(false)),
        ..WalletView::default()
    };
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&founding));
    let _shown = show_headless(&ui);
    settle();
    let s = ui.global::<Strings>();
    assert!(shown(&ui, &s.get_wl_stage_title()), "the purse stage");
    assert!(shown(&ui, &s.get_wl_choose_node()), "no node yet: choose one");
    assert!(shown(&ui, &s.get_enter_republic()), "Enter republic stays open");
    ui.global::<Purse>().set_node("http://abc.onion:18081".into());
    settle();
    assert!(shown(&ui, "Waiting for members 1/3"));
}

/// A seat asked for its consent can answer it before it has a node:
/// a decline needs none (W6).
#[test]
fn the_consent_is_asked_before_a_node_is_set() {
    let view = WalletView {
        phase: WalletPhase::Init,
        run: Some(readiness(true)),
        ..WalletView::default()
    };
    let (ui, _shown) = wallet_screen("history", &view);
    let said: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = said.clone();
    ui.global::<Purse>().on_consent(move |a| sink.borrow_mut().push(a));
    settle();
    let s = ui.global::<Strings>();
    let panel = Handle::find_by_element_type_name(&ui, "ConfirmModal").next().expect("the panel is up");
    assert!(has_text(&panel, &s.get_wl_choose_node()), "the node is asked");
    assert!(has_text(&panel, &s.get_wl_note_lost()), "what the consent covers");
    let decline = s.get_wl_decline().to_string();
    let button = panel
        .query_descendants()
        .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == decline))
        .find_first()
        .expect("Decline");
    click(&ui, &button);
    assert_eq!(*said.borrow(), vec![false]);
}

/// The organization dialog's purse box: ticked, it shows what it consents
/// to, and asks for the node first where none is set (U4).
#[test]
fn the_org_modal_asks_the_node_and_shows_the_consent() {
    backend();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1100, 900));
    ui.set_org_feat_memory(true);
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&WalletView { phase: WalletPhase::NoPurse, ..WalletView::default() }));
    let set_up = Rc::new(std::cell::Cell::new(0));
    let count = set_up.clone();
    ui.global::<Purse>().on_set_up(move || count.set(count.get() + 1));
    let _shown = show_headless(&ui);
    ui.set_org_features_modal_open(true);
    settle();
    let s = ui.global::<Strings>();
    let dlg = Handle::find_by_element_type_name(&ui, "ConfirmModal").next().expect("the dialog");
    assert!(!has_text(&dlg, &s.get_wl_note_sees()), "unticked: no lines");
    let label = s.get_feat_wallet().to_string();
    let purse_box = dlg
        .query_descendants()
        .match_type_name("AppCheck")
        .find_all()
        .into_iter()
        .find(|c| has_text(c, &label))
        .expect("the purse box");
    click(&ui, &purse_box);
    settle();
    let dlg = Handle::find_by_element_type_name(&ui, "ConfirmModal").next().expect("the dialog");
    for line in [s.get_wl_note_sees(), s.get_wl_note_lost(), s.get_wl_note_spend()] {
        assert!(has_text(&dlg, &line), "{line}");
    }
    assert!(has_text(&dlg, &s.get_wl_choose_node()), "no node: asked first");
    let propose = s.get_oc_propose().to_string();
    let find_propose = |dlg: &Handle| {
        let propose = propose.clone();
        dlg.query_descendants()
            .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == propose))
            .find_first()
            .expect("the propose button")
    };
    click(&ui, &find_propose(&dlg));
    settle();
    assert_eq!(set_up.get(), 0, "no node: nothing proposed");
    ui.global::<Purse>().set_node("http://abc.onion:18081".into());
    settle();
    let dlg = Handle::find_by_element_type_name(&ui, "ConfirmModal").next().expect("the dialog");
    assert!(!has_text(&dlg, &s.get_wl_choose_node()));
    click(&ui, &find_propose(&dlg));
    settle();
    assert_eq!(set_up.get(), 1, "the set-up vote");
}

/// A clearnet node's acknowledgement gates its confirmation, and the
/// confirmation stores the very URL it showed; it sits above the
/// organization dialog that asked for it.
#[test]
fn a_clearnet_node_needs_the_acknowledgement() {
    backend();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1100, 900));
    crate::wallet::apply_wallet(&ui, 0, "w", Some(&WalletView { phase: WalletPhase::NoPurse, ..WalletView::default() }));
    let got: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = got.clone();
    ui.global::<Purse>().on_node_confirmed(move |u| sink.borrow_mut().push(u.to_string()));
    let _shown = show_headless(&ui);
    ui.set_org_features_modal_open(true);
    ui.global::<Purse>().set_confirm_url("https://n.example.org:18089".into());
    ui.global::<Purse>().set_confirm_kind(1);
    settle();
    let s = ui.global::<Strings>();
    let modals = Handle::find_by_element_type_name(&ui, "ConfirmModal").collect::<Vec<_>>();
    let top = modals
        .iter()
        .find(|m| has_text(m, &s.get_wl_cn_title()))
        .expect("the node warning");
    assert!(
        std::ptr::eq(top, modals.last().expect("modals")),
        "the warning draws above the dialog that asked"
    );
    let confirm = s.get_wl_cn_confirm().to_string();
    let button = top
        .query_descendants()
        .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == confirm))
        .find_first()
        .expect("Confirm node");
    click(&ui, &button);
    settle();
    assert!(got.borrow().is_empty(), "not without the acknowledgement");
    let ack = s.get_wl_cn_ack().to_string();
    let check = top
        .query_descendants()
        .match_type_name("AppCheck")
        .find_all()
        .into_iter()
        .find(|c| has_text(c, &ack))
        .expect("the acknowledgement");
    click(&ui, &check);
    settle();
    click(&ui, &button);
    settle();
    assert_eq!(*got.borrow(), vec!["https://n.example.org:18089".to_string()]);
    assert_eq!(ui.global::<Purse>().get_confirm_kind(), 0, "closed");
}

/// The node flow against a real engine: an onion node is stored confirmed
/// and leaves non-onion dialing off; a clearnet node is stored only once
/// acknowledged, and then switches non-onion dialing on.
#[test]
fn the_node_flow_reaches_the_engine() {
    backend();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, _) = node_with_chat(tmp.path());
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    let cx = Ctx {
        rt: rt.handle().clone(),
        wallet: w.clone(),
        weak: ui.as_weak(),
        last_settings: Arc::new(Mutex::new(None)),
        chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
    };
    crate::actions::wallet::wire(&ui, &cx);
    let session = |w: &WalletHandle| match rt.block_on(w.execute(Command::ReadSession)) {
        Ok(Reply::Session(s)) => s,
        other => panic!("a session: {other:?}"),
    };
    let wait = |pred: &dyn Fn(&SessionView) -> bool| {
        for _ in 0..200 {
            if pred(&session(&w)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    };
    let p = ui.global::<Purse>();
    let onion = "http://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion:18081";
    p.invoke_use_node(onion.into());
    assert!(
        wait(&|s| s.settings.wallet_daemon_url == onion && s.settings.wallet_daemon_confirmed),
        "the onion node, confirmed"
    );
    assert!(!session(&w).clearnet_session, "an onion node opens no clearnet dialing");
    assert_eq!(p.get_confirm_kind(), 0, "no acknowledgement for an onion");

    let clear = "https://n.example.org:18089";
    p.invoke_use_node(clear.into());
    assert_eq!(p.get_confirm_kind(), 1, "the acknowledgement first");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(session(&w).settings.wallet_daemon_url, onion, "nothing stored yet");
    p.invoke_node_confirmed(clear.into());
    assert!(
        wait(&|s| s.settings.wallet_daemon_url == clear && s.settings.wallet_daemon_confirmed && s.clearnet_session),
        "the clearnet node, confirmed, dialing on"
    );
}
