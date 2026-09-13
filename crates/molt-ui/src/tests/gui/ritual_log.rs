// SPDX-License-Identifier: GPL-3.0-or-later
//! The wizard's log button carries the number of refusals and warnings in
//! the ritual log, so the operator knows when the modal is worth opening.

use super::*;

#[cfg(feature = "live-preview")]
fn founder_running(warnings: i32) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_step(1);
    ui.set_cw_log_warnings(warnings);
    apply_strings(&ui, 0);
    ui.show().expect("show headless");
    ui
}

#[cfg(feature = "live-preview")]
fn log_button(ui: &AppWindow, label: &str) -> bool {
    i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "AppButton::abtn-label")
        .any(|e| e.accessible_label().is_some_and(|l| l == label))
}

#[cfg(feature = "live-preview")]
#[test]
fn the_log_button_counts_refusals_and_warnings() {
    let ui = founder_running(0);
    let title = ui.global::<Strings>().get_cw_log_title().to_string();
    assert!(log_button(&ui, &title), "a clean log: the plain title");
    ui.set_cw_log_warnings(2);
    assert!(log_button(&ui, &format!("{title} · 2")), "two problems: the count on the button");
    assert!(!log_button(&ui, &title), "the plain title is gone");
}
