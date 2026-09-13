// SPDX-License-Identifier: GPL-3.0-or-later
//! The settings tab bar lists the relays before the anonymity network
//! (user decision 2026-09-13).

use super::*;

#[cfg(feature = "live-preview")]
fn tab_x(ui: &AppWindow, label: &str) -> f32 {
    // a tab is an AppButton; the invisible measuring texts carry the same labels
    i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "AppButton::abtn-label")
        .find(|e| e.accessible_label().is_some_and(|l| l == label))
        .unwrap_or_else(|| panic!("a tab titled {label:?} must render"))
        .absolute_position()
        .x
}

#[cfg(feature = "live-preview")]
#[test]
fn the_relays_tab_comes_before_the_anonymity_tab() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Settings);
    ui.set_active_workspace("w".into());
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1600, 900));
    ui.show().expect("show headless");
    let s = ui.global::<Strings>();
    let relays = tab_x(&ui, s.get_set_tab_relays().as_ref());
    let anon = tab_x(&ui, s.get_set_tab_anon().as_ref());
    let s3 = tab_x(&ui, s.get_set_tab_s3().as_ref());
    assert!(s3 < relays && relays < anon, "order S3 {s3} < relays {relays} < anonymity {anon}");
}
