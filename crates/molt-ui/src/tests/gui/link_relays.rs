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
    assert!(shown("Use a clearnet relay?"), "a clearnet relay goes through the warning");
}
