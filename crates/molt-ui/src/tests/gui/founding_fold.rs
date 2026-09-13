// SPDX-License-Identifier: GPL-3.0-or-later
//! The wizards' finished steps fold away (`docs_archive/ui/founding_stack_fold.md`):
//! the current step renders as before, every finished one is a
//! `FoldedStep` at the bottom of the column, at most one open at a time,
//! and a phase advance closes whatever was open.

use super::*;

#[cfg(feature = "live-preview")]
type Handle = i_slint_backend_testing::ElementHandle;

/// The founder's lobby with `n` seats, `joined` of them activated.
#[cfg(feature = "live-preview")]
fn founder_lobby(n: usize, joined: usize) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    ui.set_screen(AppScreen::Create);
    ui.set_cw_name("aurora".into());
    ui.set_cw_member("walter".into());
    ui.set_cw_step(1);
    let seats: Vec<RitualSeat> = (0..n)
        .map(|i| RitualSeat {
            member: format!("seat {i}").into(),
            detail: "molt://invite/x".into(),
            state: if i < joined { 1 } else { 0 },
        })
        .collect();
    ui.set_cw_seats(ModelRc::new(VecModel::from(seats)));
    ui.set_cw_total(i32::try_from(n).expect("small n"));
    ui.set_cw_can_propose(joined == n);
    apply_strings(&ui, 0);
    ui.show().expect("show headless");
    ui
}

/// Every `FoldedStep` on screen, top to bottom.
#[cfg(feature = "live-preview")]
fn folds(ui: &AppWindow) -> Vec<Handle> {
    let mut v: Vec<Handle> = Handle::find_by_element_type_name(ui, "FoldedStep").collect();
    v.sort_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y));
    v
}

/// The fold's clickable header, found by the step title it wears.
#[cfg(feature = "live-preview")]
fn fold_header(ui: &AppWindow, title: &str) -> Handle {
    Handle::find_by_accessible_label(ui, title)
        .find(|e| e.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Button))
        .unwrap_or_else(|| panic!("a fold header titled {title:?} must render"))
}

/// Let the fold animation and the phase's change handler run: both ride
/// the event loop the headless backend never drains.
#[cfg(feature = "live-preview")]
fn settle() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(300));
}

/// A closed fold is exactly its header tall.
#[cfg(feature = "live-preview")]
fn header_h(ui: &AppWindow, title: &str) -> f32 {
    fold_header(ui, title).size().height
}

#[cfg(feature = "live-preview")]
#[test]
fn while_links_are_handed_out_the_members_stay_unfolded_and_only_setup_folds() {
    let ui = founder_lobby(3, 1);
    let setup = ui.global::<Strings>().get_wiz_step_setup().to_string();
    let f = folds(&ui);
    assert_eq!(f.len(), 1, "P1: the setup is the only finished step");
    assert!((f[0].size().height - header_h(&ui, &setup)).abs() < 1.0, "closed = header tall");
    let seats = Handle::find_by_element_type_name(&ui, "FounderSeats")
        .next()
        .expect("the member list renders");
    assert!(
        seats.absolute_position().y < f[0].absolute_position().y,
        "the current step (members) sits above the folded pile"
    );
}

#[cfg(feature = "live-preview")]
#[test]
fn once_every_seat_joined_the_members_fold_beneath_the_charter_form() {
    let ui = founder_lobby(3, 3);
    let s = ui.global::<Strings>();
    let invites = s.get_wiz_step_invites().to_string();
    let setup = s.get_wiz_step_setup().to_string();
    let f = folds(&ui);
    assert_eq!(f.len(), 2, "P2: invites and setup are finished");
    // newest above oldest
    assert!(
        fold_header(&ui, &invites).absolute_position().y
            < fold_header(&ui, &setup).absolute_position().y
    );
    for e in &f {
        assert!((e.size().height - header_h(&ui, &setup)).abs() < 1.0, "every fold starts closed");
    }
    assert_eq!(ui.get_cw_fold(), -1);
}

#[cfg(feature = "live-preview")]
#[test]
fn one_folded_step_opens_at_a_time() {
    let ui = founder_lobby(3, 3);
    let s = ui.global::<Strings>();
    let invites = s.get_wiz_step_invites().to_string();
    let setup = s.get_wiz_step_setup().to_string();
    let closed = header_h(&ui, &setup);

    click(&ui, &fold_header(&ui, &invites));

    settle();
    assert_eq!(ui.get_cw_fold(), 1, "+ on invites opens invites");
    let f = folds(&ui);
    assert!(f[0].size().height > closed + 40.0, "the open fold shows its body");
    assert!((f[1].size().height - closed).abs() < 1.0, "setup stays closed");

    click(&ui, &fold_header(&ui, &setup));

    settle();
    assert_eq!(ui.get_cw_fold(), 0, "+ on setup opens setup ...");
    let f = folds(&ui);
    assert!((f[0].size().height - closed).abs() < 1.0, "... and closes invites");
    assert!(f[1].size().height > closed + 20.0);

    click(&ui, &fold_header(&ui, &setup));

    settle();
    assert_eq!(ui.get_cw_fold(), -1, "- on the open one closes it");
}

#[cfg(feature = "live-preview")]
#[test]
fn a_phase_advance_closes_the_open_fold_and_folds_the_charter() {
    let ui = founder_lobby(3, 3);
    let s = ui.global::<Strings>();
    let invites = s.get_wiz_step_invites().to_string();
    let charter = s.get_wiz_step_charter().to_string();
    click(&ui, &fold_header(&ui, &invites));
    settle();
    assert_eq!(ui.get_cw_fold(), 1);

    ui.set_cw_agenda("we meet on tuesdays".into());
    ui.set_cw_proposed(true);
    settle();
    assert_eq!(ui.get_cw_fold(), -1, "the phase moved on: nothing stays open");
    assert_eq!(folds(&ui).len(), 3, "charter, invites, setup");
    // the charter is the newest finished step, so it tops the pile
    let f = folds(&ui);
    assert!(
        (fold_header(&ui, &charter).absolute_position().y - f[0].absolute_position().y).abs()
            < 1.0
    );
}

#[cfg(feature = "live-preview")]
#[test]
fn the_folded_invites_header_counts_the_sealed_seats_live() {
    let ui = founder_lobby(4, 4);
    ui.set_cw_proposed(true);
    let word = ui.global::<Strings>().get_cw_sealed_word().to_string();
    ui.set_cw_sealed(1);
    assert!(Handle::find_by_accessible_label(&ui, format!("1 / 4 {word}").as_str()).next().is_some());
    ui.set_cw_sealed(3);
    assert!(
        Handle::find_by_accessible_label(&ui, format!("3 / 4 {word}").as_str()).next().is_some(),
        "the summary follows the seats while folded"
    );
}

#[cfg(feature = "live-preview")]
#[test]
fn the_joiner_folds_the_ratified_charter_and_the_join_facts() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 900));
    ui.set_screen(AppScreen::Join);
    ui.set_jw_step(1);
    ui.set_jw_republic("aurora".into());
    ui.set_jw_rule("2-of-3".into());
    ui.set_jw_proposed_name("aurora".into());
    ui.set_jw_proposed_agenda("we meet on tuesdays".into());
    ui.set_jw_awaiting_ratify(true);
    apply_strings(&ui, 0);
    ui.show().expect("show headless");
    let s = ui.global::<Strings>();
    let invite = s.get_wiz_step_invite().to_string();
    let charter = s.get_wiz_step_charter().to_string();

    assert_eq!(folds(&ui).len(), 1, "while ratifying, the charter is the current step");
    let _ = fold_header(&ui, &invite);
    assert!(
        Handle::find_by_element_type_name(&ui, "CharterView").next().is_some(),
        "the charter renders unfolded for the decision"
    );

    click(&ui, &fold_header(&ui, &invite));

    settle();
    assert_eq!(ui.get_jw_fold(), 0);
    ui.set_jw_awaiting_ratify(false);
    settle();
    assert_eq!(ui.get_jw_fold(), -1, "ratified: the phase moved, the fold closed");
    assert_eq!(folds(&ui).len(), 2, "the charter joined the pile");
    assert!(
        fold_header(&ui, &charter).absolute_position().y
            < fold_header(&ui, &invite).absolute_position().y,
        "newest above oldest"
    );
}
