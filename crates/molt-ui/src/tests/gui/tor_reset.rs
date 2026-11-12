// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings, Anonymity network: the embedded client's reset asks once and
//! names its cost.

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

fn labelled(ui: &AppWindow, wanted: &str) -> Vec<Handle> {
    Handle::find_by_element_type_name(ui, "Text")
        .filter(|t| t.size().height > 0.0 && t.accessible_label().is_some_and(|l| l == wanted))
        .collect()
}

fn anonymity_tab() -> (AppWindow, Shown, Rc<std::cell::Cell<u32>>) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Settings);
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1600, 900));
    let shown = show_headless(&ui);
    ui.set_set_tab(4);
    ui.set_cfg_network_index(0);
    let resets = Rc::new(std::cell::Cell::new(0));
    let seen = resets.clone();
    ui.on_reset_tor(move || seen.set(seen.get() + 1));
    (ui, shown, resets)
}

#[test]
fn only_the_embedded_mode_offers_the_reset() {
    let (ui, _shown, _) = anonymity_tab();
    for (mode, shown) in [(0, 0), (1, 1), (2, 0)] {
        ui.set_cfg_tor_mode_index(mode);
        assert_eq!(labelled(&ui, "Reset embedded Tor").len(), shown, "tor mode {mode}");
    }
    ui.set_cfg_network_index(1);
    ui.set_cfg_tor_mode_index(1);
    assert!(labelled(&ui, "Reset embedded Tor").is_empty(), "no Tor, no reset");
}

#[test]
fn the_reset_asks_once_and_names_its_cost() {
    let (ui, _shown, resets) = anonymity_tab();
    ui.set_cfg_tor_mode_index(1);
    click(&ui, &labelled(&ui, "Reset embedded Tor")[0]);
    assert!(ui.get_tor_reset_confirm_open());
    assert_eq!(resets.get(), 0, "nothing happens before the confirm");
    assert_eq!(labelled(&ui, "Picks new entry guards and reloads the directory.").len(), 1);

    press(&ui, slint::platform::Key::Escape);
    assert!(!ui.get_tor_reset_confirm_open());
    assert_eq!(resets.get(), 0, "Escape resets nothing");

    click(&ui, &labelled(&ui, "Reset embedded Tor")[0]);
    click(&ui, &labelled(&ui, "Reset")[0]);
    assert!(!ui.get_tor_reset_confirm_open());
    assert_eq!(resets.get(), 1);
}
