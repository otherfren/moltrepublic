// SPDX-License-Identifier: GPL-3.0-or-later
//! The (re)join relay panel under the link field.

use super::*;
use slint::{ModelRc, VecModel};

/// The panel sits under the pasted link and lists the link's relays: a
/// usable one toggles its pick, a blocked one is adopted (a clearnet one
/// through the exposure warning), and nothing picked disarms Continue.
#[test]
fn the_link_panel_lists_the_links_relays_and_acts_per_row() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Restore);
    apply_strings(&ui, 0);
    ui.on_classify_link(|_| LinkPreview {
        kind: 2,
        republic: "R".into(),
        who: "w".into(),
    });
    let row = |url: &str, picked, block, kind| LinkRelay { url: url.into(), picked, block, kind };
    ui.set_rw_relays(ModelRc::new(VecModel::from(vec![
        row("wss://a.example", true, 0, 1),
        row("wss://b.example", false, 1, 1),
    ])));
    ui.set_rv_link("molt://recover/x".into());
    let toggled = std::rc::Rc::new(std::cell::Cell::new(-1));
    let t = toggled.clone();
    ui.on_rw_relay_toggle(move |i| t.set(i));
    let adopted = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let a = adopted.clone();
    ui.on_rw_relay_adopt(move |u| *a.borrow_mut() = u.to_string());
    let _shown = show_headless(&ui);

    let shown = |label: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label)
            .next()
            .is_some()
    };
    assert!(shown("wss://a.example") && shown("wss://b.example"));
    assert!(shown(&ui.global::<Strings>().get_rw_rl_not_in_pool()));

    let checks: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::rw-relay-check")
            .collect();
    assert_eq!(checks.len(), 2);
    click(&ui, &checks[0]);
    assert_eq!(toggled.get(), 0);
    click(&ui, &checks[1]);
    assert_eq!(adopted.borrow().as_str(), "wss://b.example");
}

/// Two or more blocked relays get one adopt-all button, and the exposure
/// warning names every clearnet and local relay it confirms at once.
#[test]
fn adopt_all_goes_through_one_warning_for_every_exposed_relay() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Restore);
    apply_strings(&ui, 0);
    ui.on_classify_link(|_| LinkPreview { kind: 1, republic: "R".into(), who: "w".into() });
    let row = |url: &str, block, kind| LinkRelay { url: url.into(), picked: block == 0, block, kind };
    ui.set_rw_relays(ModelRc::new(VecModel::from(vec![
        row("wss://b.example", 1, 1),
        row("ws://192.168.1.2:7777", 1, 2),
    ])));
    ui.set_rw_relay_blocked(2);
    ui.set_rv_link("molt://invite/x".into());
    let all = std::rc::Rc::new(std::cell::Cell::new(false));
    let a = all.clone();
    ui.on_rw_relay_adopt_all(move || a.set(true));
    let confirmed = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let c = confirmed.clone();
    ui.on_relay_confirm_clearnet(move |urls| {
        *c.borrow_mut() = urls.iter().map(|u| u.to_string()).collect();
    });
    let _shown = show_headless(&ui);
    let by_label = |label: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label).next()
    };
    let strings = ui.global::<Strings>();

    let button = by_label(&format!("{} (2)", strings.get_rw_rl_adopt_all())).expect("adopt-all button");
    click(&ui, &button);
    assert!(all.get());

    let urls: Vec<slint::SharedString> = vec!["wss://b.example".into(), "ws://192.168.1.2:7777".into()];
    ui.invoke_open_clearnet_warnings(ModelRc::new(VecModel::from(urls)), true, true);
    // once in the panel, once in the warning
    let twice = |label: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label).count() >= 2
    };
    assert!(twice("wss://b.example") && twice("ws://192.168.1.2:7777"));
    click(&ui, &by_label(&strings.get_rp_cn_ack_many()).expect("ack"));
    click(&ui, &by_label(&strings.get_rp_cn_confirm_many()).expect("confirm"));
    assert_eq!(*confirmed.borrow(), vec!["wss://b.example", "ws://192.168.1.2:7777"]);
}

/// Settings › Nostr relays lists the republic's relays this node cannot
/// dial, with a per-row adopt and an adopt-all.
#[test]
fn the_relays_tab_offers_the_republics_missing_relays() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Settings);
    ui.set_set_tab(5);
    apply_strings(&ui, 0);
    let row = |url: &str, block, kind| LinkRelay { url: url.into(), picked: false, block, kind };
    ui.set_rp_missing(ModelRc::new(VecModel::from(vec![
        row("wss://b.example", 1, 1),
        row("ws://a.onion", 2, 0),
    ])));
    let adopted = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let a = adopted.clone();
    ui.on_rp_missing_adopt(move |u| *a.borrow_mut() = u.to_string());
    let all = std::rc::Rc::new(std::cell::Cell::new(false));
    let l = all.clone();
    ui.on_rp_missing_adopt_all(move || l.set(true));
    let _shown = show_headless(&ui);
    let strings = ui.global::<Strings>();
    let by_label = |label: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label).next()
    };

    assert!(by_label(&strings.get_rp_missing_title()).is_some());
    assert!(by_label(&strings.get_rw_rl_unconfirmed()).is_some());
    click(&ui, &by_label(&format!("{} (2)", strings.get_rw_rl_adopt_all())).expect("adopt all"));
    assert!(all.get());
    let rows: Vec<_> =
        i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::rp-missing-adopt")
            .collect();
    assert_eq!(rows.len(), 2);
    click(&ui, &rows[1]);
    assert_eq!(adopted.borrow().as_str(), "ws://a.onion");
}

/// The row button's hint sits in the clipped relay list, so it goes through
/// the window-topmost overlay, right of the button, and leaves with the
/// pointer.
#[test]
fn the_relay_row_hint_is_not_clipped_by_the_list() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    ui.set_screen(AppScreen::Restore);
    apply_strings(&ui, 0);
    ui.on_classify_link(|_| LinkPreview { kind: 1, republic: "R".into(), who: "w".into() });
    ui.set_rw_relays(ModelRc::new(VecModel::from(vec![LinkRelay {
        url: "wss://b.example".into(),
        picked: false,
        block: 1,
        kind: 1,
    }])));
    ui.set_rv_link("molt://invite/x".into());
    let _shown = show_headless(&ui);
    let button = i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::rw-relay-check")
        .next()
        .expect("row button");
    let at = |x: f32, y: f32| {
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: slint::LogicalPosition::new(x, y) });
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    };
    let (p, s) = (button.absolute_position(), button.size());
    at(p.x + s.width / 2.0, p.y + s.height / 2.0);
    let tip = ui.global::<HintTip>();
    assert_eq!(tip.get_label(), ui.global::<Strings>().get_rw_rl_adopt());
    assert!(tip.get_ax() >= p.x + s.width, "the hint starts right of the button");
    at(1.0, 1.0);
    assert_eq!(tip.get_label(), "");
}
