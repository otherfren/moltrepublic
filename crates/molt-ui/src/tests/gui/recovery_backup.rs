// SPDX-License-Identifier: GPL-3.0-or-later
//! The recovery checklist and the backup/restore modals, headless.

use super::*;

/// The rejoiner's checklist (recovery_auto_approval.md §5): the session's
/// `RecoverState` becomes per-seat rows plus the have/need counters, and
/// an empty state clears the rows again.
#[test]
fn the_recover_checklist_maps_seats_and_counts() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let sv = SessionView {
        recover: molt_core::RecoverState {
            member: "petra".to_string(),
            need: 3,
            seats: vec![
                molt_core::RecoverSeat { member: "walter".to_string(), approved: true },
                molt_core::RecoverSeat { member: "petra".to_string(), approved: true },
                molt_core::RecoverSeat { member: "vera".to_string(), approved: false },
            ],
        },
        ..SessionView::default()
    };
    apply_session(&ui, &sv, true, &chat_ui);
    assert_eq!((ui.get_rv_have(), ui.get_rv_need()), (2, 3));
    let rows = ui.get_rv_seats();
    let got: Vec<(String, bool)> = (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .map(|r| (r.member.to_string(), r.approved))
        .collect();
    assert_eq!(
        got,
        vec![
            ("walter".to_string(), true),
            ("petra".to_string(), true),
            ("vera".to_string(), false)
        ],
        "roster order, per-seat approval"
    );

    // a fresh recovery clears the list (RecoverStart resets the state)
    apply_session(&ui, &SessionView::default(), true, &chat_ui);
    assert_eq!(ui.get_rv_seats().row_count(), 0);
    assert_eq!((ui.get_rv_have(), ui.get_rv_need()), (0, 0));
}

/// Restore-from-backup (recovery_auto_approval.md §7): the Settings ›
/// Backup modal's state machine — a confirm without a phrase starts
/// nothing; with one it hands ("s3", the orphan's id, the phrase) to the
/// real restore pipeline, leads to the Restore screen, and drops the
/// phrase. Runs on the dev-ui chain (`ElementHandle` needs the
/// interpreter's debug info).
#[cfg(feature = "live-preview")]
#[test]
fn the_backup_restore_modal_drives_the_s3_pipeline() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    let calls: Rc<RefCell<Vec<(String, String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let c = calls.clone();
        ui.on_restore_start(move |way, target, secret| {
            c.borrow_mut().push((way.to_string(), target.to_string(), secret.to_string()));
        });
    }
    let navs: Rc<RefCell<Vec<AppScreen>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let n = navs.clone();
        ui.on_navigate(move |s| n.borrow_mut().push(s));
    }
    let label = ui.global::<Strings>().get_bk_restore().to_string();
    assert!(!label.is_empty(), "the label must be applied before searching");
    ui.show().expect("show headless");
    // control BEFORE the modal: nothing wears the label on this screen
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label.as_str())
            .next()
            .is_none(),
        "no restore affordance before the modal opens"
    );
    // an orphan row opened the modal
    ui.set_bk_restore_id("cafe01".into());
    ui.set_bk_restore_open(true);
    let click = |ui: &AppWindow| {
        let h = i_slint_backend_testing::ElementHandle::find_by_accessible_label(
            ui,
            label.as_str(),
        )
        .next()
        .expect("the modal's confirm button renders");
        let at = slint::LogicalPosition::new(
            h.absolute_position().x + h.size().width / 2.0,
            h.absolute_position().y + h.size().height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position: at,
            button: slint::platform::PointerEventButton::Left,
        });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position: at,
            button: slint::platform::PointerEventButton::Left,
        });
    };
    // no phrase yet: the confirm is disarmed
    click(&ui);
    assert!(calls.borrow().is_empty(), "no phrase, no pipeline");
    assert!(ui.get_bk_restore_open(), "the modal stays up");
    // with the phrase: the REAL pipeline is asked, the modal closes,
    // the phrase is dropped, and the run view is next
    ui.set_bk_restore_seed("brave mole over the hills".into());
    click(&ui);
    assert_eq!(
        calls.borrow().as_slice(),
        &[(
            "s3".to_string(),
            "cafe01".to_string(),
            "brave mole over the hills".to_string()
        )],
        "confirm hands way/target/phrase to restore-start"
    );
    assert!(!ui.get_bk_restore_open(), "confirm closes the modal");
    assert_eq!(ui.get_bk_restore_seed().as_str(), "", "every way out drops the phrase");
    assert_eq!(navs.borrow().last(), Some(&AppScreen::Restore), "the run view is next");
}

/// The orphan row's restore affordance lives IN the local column (user
/// decision 2026-08-24) and must FIT the row — the old trailing button
/// sat beyond the table's column budget and was clipped invisible on
/// every build (the "kein Knopf zu sehen" field report), worse under
/// ui-scale. Measured at a scaled app font, dev-ui chain.
#[cfg(feature = "live-preview")]
#[test]
fn the_orphan_restore_button_sits_in_the_local_column_and_fits() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.global::<Theme>().set_fs_app(20.0); // ui-scale ≈ 1.43, the field setup
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let (sv, _id) = sv_backup_orphan();
    apply_session(&ui, &sv, true, &chat_ui);
    ui.set_screen(AppScreen::Settings);
    ui.set_set_tab(2);
    ui.show().expect("show headless");
    let btn = i_slint_backend_testing::ElementHandle::find_by_element_id(
        &ui,
        "AppWindow::bkr-btn",
    )
    .next()
    .expect("the orphan row renders its restore button");
    let rows: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_id(
        &ui,
        "AppWindow::bk-row",
    )
    .collect();
    assert_eq!(rows.len(), 2, "orphan + foreign row render");
    let row = &rows[0];
    let btn_right = btn.absolute_position().x + btn.size().width;
    let row_right = row.absolute_position().x + row.size().width;
    assert!(
        btn.size().width > 0.0 && btn_right <= row_right + 0.5,
        "the restore button must fit inside its row: button right {btn_right} vs row right {row_right}"
    );
    // …and the last COLUMN stays inside too (the pre-fix budget ignored
    // the ui-scale of the fixed columns, clipping the row's tail)
    for r in &rows {
        assert!(
            r.absolute_position().x + r.size().width <= 1200.0,
            "a row never overflows the window"
        );
    }
}

/// A double-click anywhere on an orphan row arms the same restore modal
/// as the button (user decision 2026-08-24); a foreign-key row (no
/// workspace id) stays inert.
#[cfg(feature = "live-preview")]
#[test]
fn a_double_click_on_an_orphan_row_arms_the_restore_modal() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let (sv, id) = sv_backup_orphan();
    apply_session(&ui, &sv, true, &chat_ui);
    ui.set_screen(AppScreen::Settings);
    ui.set_set_tab(2);
    ui.show().expect("show headless");
    let rows: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_id(
        &ui,
        "AppWindow::bk-row",
    )
    .collect();
    let dclick = |row: &i_slint_backend_testing::ElementHandle| {
        let at = slint::LogicalPosition::new(
            // between the columns, not on the button
            row.absolute_position().x + row.size().width * 0.6,
            row.absolute_position().y + row.size().height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        for _ in 0..2 {
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
        }
    };
    // the foreign-key row (sorted last) stays inert
    dclick(&rows[1]);
    assert!(!ui.get_bk_restore_open(), "a foreign key has nothing to restore");
    // the orphan row arms the modal with ITS id
    dclick(&rows[0]);
    assert!(ui.get_bk_restore_open(), "double-click arms the restore modal");
    assert_eq!(ui.get_bk_restore_id().as_str(), id, "the row's own id");
}
