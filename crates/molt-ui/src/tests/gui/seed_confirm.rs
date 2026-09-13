// SPDX-License-Identifier: GPL-3.0-or-later
//! The wizards' phrase-backup proof takes a 24-word phrase TYPED, not
//! only pasted: the field is an area several lines tall, like the rejoin
//! phrase (field report 2026-09-13: a one-line field could not be typed
//! into).

use super::*;

#[cfg(feature = "live-preview")]
type Handle = i_slint_backend_testing::ElementHandle;

/// The founder at the backup proof: proposed, everyone ratified, the
/// phrase not yet confirmed.
#[cfg(feature = "live-preview")]
fn founder_at_backup_proof() -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_name("aurora".into());
    ui.set_cw_member("walter".into());
    ui.set_cw_step(1);
    ui.set_cw_seats(ModelRc::new(VecModel::from(vec![RitualSeat {
        member: "seat 0".into(),
        detail: "molt://invite/x".into(),
        state: 1,
    }])));
    ui.set_cw_total(1);
    ui.set_cw_proposed(true);
    ui.set_cw_all_ratified(true);
    ui.set_cw_backup_confirmed(false);
    ui.set_cw_seed("abandon ability able about above absent absorb abstract absurd abuse access accident".into());
    apply_strings(&ui, 0);
    ui.show().expect("show headless");
    ui
}

#[cfg(feature = "live-preview")]
fn inside(outer: &Handle, inner: &Handle) -> bool {
    let (o, i) = (outer.absolute_position(), inner.absolute_position());
    let s = outer.size();
    i.x >= o.x && i.y >= o.y && i.x < o.x + s.width && i.y < o.y + s.height
}

#[cfg(feature = "live-preview")]
#[test]
fn the_backup_proof_is_typed_into_an_area_not_a_line() {
    let ui = founder_at_backup_proof();
    let step = Handle::find_by_element_type_name(&ui, "SeedConfirmStep")
        .next()
        .expect("the backup proof renders");
    let area = Handle::find_by_element_type_name(&ui, "AppArea")
        .find(|a| inside(&step, a))
        .expect("the proof is typed into an AppArea");
    assert!(
        area.size().height >= 60.0,
        "several lines tall, not one: {}px",
        area.size().height
    );
    assert!(
        !Handle::find_by_element_type_name(&ui, "AppField").any(|f| inside(&step, &f)),
        "no one-line field is left in the step"
    );
}
