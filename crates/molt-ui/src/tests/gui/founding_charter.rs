// SPDX-License-Identifier: GPL-3.0-or-later
//! The founder writes the charter beside the republic's name at setup; once
//! every seat joined, the charter step only shows it for one propose click.

use super::*;

use std::cell::Cell;
use std::rc::Rc;

type Handle = i_slint_backend_testing::ElementHandle;

fn setup_form() -> (AppWindow, Shown) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 2000));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_step(0);
    ui.set_cw_name("a".into());
    ui.set_cw_member("b".into());
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    (ui, shown)
}

fn button(ui: &AppWindow, label: &str) -> Handle {
    Handle::find_by_element_type_name(ui, "AppButton")
        .find(|b| {
            let want = label.to_string();
            b.query_descendants()
                .match_predicate(move |d| d.accessible_label().is_some_and(|l| l.as_str() == want))
                .find_first()
                .is_some()
        })
        .unwrap_or_else(|| panic!("a {label:?} button must render"))
}

#[test]
fn the_setup_form_carries_the_agenda_and_the_features() {
    let (ui, _shown) = setup_form();
    assert_eq!(Handle::find_by_element_type_name(&ui, "AppArea").count(), 1, "the agenda field");
    assert_eq!(Handle::find_by_element_type_name(&ui, "FeatureTable").count(), 1);
}

#[test]
fn the_ritual_begins_only_with_an_agenda() {
    let (ui, _shown) = setup_form();
    let started = Rc::new(Cell::new(0));
    let s = started.clone();
    ui.on_create_start(move |_, _, _, _| s.set(s.get() + 1));
    let found = ui.global::<Strings>().get_cw_found().to_string();

    click(&ui, &button(&ui, &found));
    assert_eq!(started.get(), 0, "no agenda, no ritual");

    ui.set_cw_agenda("x".into());
    click(&ui, &button(&ui, &found));
    assert_eq!(started.get(), 1);
}

#[test]
fn the_charter_step_shows_the_draft_and_proposes_it_in_one_click() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 1800));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_name("a".into());
    ui.set_cw_member("b".into());
    ui.set_cw_agenda("x y".into());
    ui.set_cw_step(1);
    ui.set_cw_total(2);
    ui.set_cw_can_propose(true);
    apply_strings(&ui, 0);
    let _shown = show_headless(&ui);
    let proposed: Rc<Cell<Option<String>>> = Rc::new(Cell::new(None));
    let p = proposed.clone();
    ui.on_create_propose(move |_, agenda| p.set(Some(agenda.to_string())));

    assert_eq!(Handle::find_by_element_type_name(&ui, "AppArea").count(), 0, "nothing to write here");
    assert!(Handle::find_by_element_type_name(&ui, "CharterView").next().is_some());
    assert!(Handle::find_by_accessible_label(&ui, "x y").next().is_some(), "the draft is shown");

    let propose = ui.global::<Strings>().get_cw_propose().to_string();
    click(&ui, &button(&ui, &propose));
    assert_eq!(proposed.take().as_deref(), Some("x y"));
}
