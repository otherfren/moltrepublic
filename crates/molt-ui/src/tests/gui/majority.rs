// SPDX-License-Identifier: GPL-3.0-or-later
//! The majority rule (`chain_rescue_plan.md`): the wizard cannot select
//! 2m <= n and says why in one line; the vault row names the seats it
//! needs; Organization shows the notice of a republic founded before it.

use super::*;
use crate::actions::vault::wire_local;
use crate::surfaces::{OrgStats, SurfacesBundle};

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

fn shown(ui: &AppWindow, label: &str) -> bool {
    Handle::find_by_accessible_label(ui, label).any(|e| e.size().width > 0.0)
}

fn setup_form(m: i32, n: i32) -> (AppWindow, Shown) {
    backend();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 1800));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_name("a".into());
    ui.set_cw_member("a".into());
    ui.set_cw_members(n);
    ui.set_cw_threshold(m);
    ui.set_cw_step(0);
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    settle();
    (ui, shown)
}

/// `(threshold, members)` steppers, left to right.
fn steppers(ui: &AppWindow) -> (Handle, Handle) {
    let mut all: Vec<Handle> =
        Handle::find_by_element_type_name(ui, "AppStepper").filter(|s| s.size().width > 0.0).collect();
    assert_eq!(all.len(), 2, "two steppers on the setup form");
    all.sort_by(|a, b| a.absolute_position().x.total_cmp(&b.absolute_position().x));
    let members = all.pop().expect("members");
    (all.pop().expect("threshold"), members)
}

/// One click on the stepper's `−` (left) or `+` (right) end.
fn step(ui: &AppWindow, stepper: &Handle, up: bool) {
    let (p, s) = (stepper.absolute_position(), stepper.size());
    let x = if up { p.x + s.width - 10.0 } else { p.x + 10.0 };
    let at = slint::LogicalPosition::new(x, p.y + s.height / 2.0);
    ui.window().dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
    for pressed in [true, false] {
        let button = slint::platform::PointerEventButton::Left;
        ui.window().dispatch_event(if pressed {
            slint::platform::WindowEvent::PointerPressed { position: at, button }
        } else {
            slint::platform::WindowEvent::PointerReleased { position: at, button }
        });
    }
    settle();
}

#[test]
fn the_threshold_stepper_stops_at_a_majority() {
    let (ui, _shown) = setup_form(4, 5);
    let why = ui.global::<Strings>().get_cw_rule_majority().to_string();
    assert!(!shown(&ui, &why), "above the floor: no line");
    let (threshold, _) = steppers(&ui);
    step(&ui, &threshold, false);
    assert_eq!(ui.get_cw_threshold(), 3);
    assert!(shown(&ui, &why), "at the floor: the reason");
    step(&ui, &threshold, false);
    assert_eq!(ui.get_cw_threshold(), 3, "3-of-5 is the floor, 2-of-5 is not selectable");
}

#[test]
fn more_members_raise_the_floor() {
    let (ui, _shown) = setup_form(2, 3);
    let (threshold, members) = steppers(&ui);
    step(&ui, &members, true);
    assert_eq!((ui.get_cw_threshold(), ui.get_cw_members()), (3, 4));
    step(&ui, &threshold, false);
    assert_eq!(ui.get_cw_threshold(), 3, "2-of-4 is not selectable");
    step(&ui, &members, true);
    step(&ui, &members, true);
    assert_eq!((ui.get_cw_threshold(), ui.get_cw_members()), (4, 6));
    step(&ui, &threshold, false);
    assert_eq!(ui.get_cw_threshold(), 4, "3-of-6 is not selectable");
}

/// Under five seats the vault row says so; 3-of-5 offers it.
#[test]
fn the_vault_row_names_the_seats_it_needs() {
    for (n, allowed) in [(3, false), (4, false), (5, true)] {
        let (ui, _shown) = setup_form(n / 2 + 1, n);
        let few = ui.global::<Strings>().get_feat_vault_members().to_string();
        assert_eq!(shown(&ui, &few), !allowed, "n = {n}");
        assert_eq!(
            crate::actions::ritual::charter_features(&ui).contains(&"vault".to_string()),
            allowed,
            "n = {n}"
        );
    }
}

fn organization(below: bool, enable: molt_core::vault::VaultEnable, few: bool) -> (AppWindow, Shown) {
    backend();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    apply_strings(&ui, 0);
    wire_local(&ui);
    apply_surfaces(
        &ui,
        &SurfacesBundle {
            org_stats: OrgStats {
                below_majority: below,
                vault_enable: enable,
                vault_few_members: few,
                mirror_quota: 1,
                ..OrgStats::default()
            },
            ..SurfacesBundle::default()
        },
    );
    ui.set_selected_surface("organization".into());
    ui.set_selected_view("status".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "organization".into(),
        name: "Organization".into(),
        ..SurfaceTab::default()
    }])));
    let shown = show_headless(&ui);
    settle();
    (ui, shown)
}

#[test]
fn organization_shows_the_notice_only_below_majority() {
    use molt_core::vault::VaultEnable;
    for below in [true, false] {
        let (ui, _shown) = organization(below, VaultEnable::Offer, false);
        let line = format!("⚠ {}", ui.global::<Strings>().get_os_below_majority());
        assert_eq!(shown(&ui, &line), below);
    }
}

/// The Organization vault row's reason: seats where a majority republic
/// lacks them, the bounds otherwise.
#[test]
fn the_vault_reason_in_organization_follows_the_seat_count() {
    use molt_core::vault::VaultEnable;
    let (ui, _shown) = organization(false, VaultEnable::Bounds, true);
    assert_eq!(ui.get_org_vault_enable(), 4);
    let (ui, _shown) = organization(false, VaultEnable::Bounds, false);
    assert_eq!(ui.get_org_vault_enable(), 3);
}
