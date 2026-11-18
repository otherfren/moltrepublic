// SPDX-License-Identifier: GPL-3.0-or-later
//! The paste buttons (`docs/web_gui.md` §6.3). A paste is asynchronous:
//! the window names the field, Rust fills exactly that one when the
//! clipboard answers. Two of the four fields hold a recovery phrase, so a
//! pasted text must never reach a field that was not asked for.

use super::*;
use crate::actions::clipboard::land_paste;

const TARGETS: [PasteTarget; 4] =
    [PasteTarget::RwSeed, PasteTarget::RvLink, PasteTarget::BkRestoreSeed, PasteTarget::DecryptPhrase];

/// The four fields a paste can fill, in [`TARGETS`] order.
fn fields(ui: &AppWindow) -> [String; 4] {
    [
        ui.get_rw_seed().to_string(),
        ui.get_rv_link().to_string(),
        ui.get_bk_restore_seed().to_string(),
        ui.get_decrypt_phrase().to_string(),
    ]
}

/// A window with `target`'s field on show - the two wizard fields share
/// one screen and never show together - and the two dialogs open.
fn on_show(target: PasteTarget) -> AppWindow {
    let ui = AppWindow::new().expect("headless window");
    ui.set_screen(AppScreen::Restore);
    ui.set_rw_phase(if target == PasteTarget::RwSeed { 1 } else { 0 });
    ui.set_bk_restore_open(true);
    ui.set_decrypt_modal_open(true);
    frame();
    ui
}

/// One event-loop turn: the window's change handlers run.
fn frame() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
}

/// Take `target`'s field off show.
fn leave(ui: &AppWindow, target: PasteTarget) {
    match target {
        PasteTarget::RwSeed | PasteTarget::RvLink => ui.set_screen(AppScreen::Choice),
        PasteTarget::BkRestoreSeed => ui.set_bk_restore_open(false),
        PasteTarget::DecryptPhrase => ui.set_decrypt_modal_open(false),
    }
    frame();
}

/// Put `target`'s field back on show.
fn reenter(ui: &AppWindow, target: PasteTarget) {
    match target {
        PasteTarget::RwSeed | PasteTarget::RvLink => ui.set_screen(AppScreen::Restore),
        PasteTarget::BkRestoreSeed => ui.set_bk_restore_open(true),
        PasteTarget::DecryptPhrase => ui.set_decrypt_modal_open(true),
    }
    frame();
}

/// A browser's clipboard answers late: a text for a field that is no
/// longer on show - a closed phrase dialog, a left wizard - is dropped.
#[test]
fn a_paste_for_a_field_that_left_the_screen_is_dropped() {
    i_slint_backend_testing::init_no_event_loop();
    for (at, target) in TARGETS.into_iter().enumerate() {
        let ui = on_show(target);
        let asked = ui.get_paste_opening();
        leave(&ui, target);
        land_paste(&ui, target, asked, "brave mole over the hills".to_string());
        assert_eq!(fields(&ui)[at], "", "{target:?}");
    }
}

/// A dialog serves every workspace and every backup row, so the same
/// field on show again is not the field that was asked for: a text
/// asked from one opening never lands in the next.
#[test]
fn a_paste_asked_from_an_earlier_opening_is_dropped() {
    i_slint_backend_testing::init_no_event_loop();
    for (at, target) in TARGETS.into_iter().enumerate() {
        let ui = on_show(target);
        let asked = ui.get_paste_opening();
        leave(&ui, target);
        reenter(&ui, target);
        land_paste(&ui, target, asked, "brave mole over the hills".to_string());
        assert_eq!(fields(&ui)[at], "", "{target:?}");
    }
}

/// The wizard's two fields sit on two steps of one screen: a text asked
/// on one step does not land once the wizard moved to the other, nor
/// after it came back.
#[test]
fn a_paste_asked_on_another_wizard_step_is_dropped() {
    i_slint_backend_testing::init_no_event_loop();
    for (at, target, own, other) in [(0, PasteTarget::RwSeed, 1, 0), (1, PasteTarget::RvLink, 0, 1)] {
        let ui = on_show(target);
        let asked = ui.get_paste_opening();
        ui.set_rw_phase(other);
        frame();
        land_paste(&ui, target, asked, "brave mole over the hills".to_string());
        assert_eq!(fields(&ui)[at], "", "{target:?} under the other step");
        // the step change is seen at once, before any change handler ran
        ui.set_rw_phase(own);
        ui.set_rw_phase(other);
        land_paste(&ui, target, ui.get_paste_opening(), "brave mole over the hills".to_string());
        assert_eq!(fields(&ui)[at], "", "{target:?} hidden, same turn");
        ui.set_rw_phase(own);
        frame();
        land_paste(&ui, target, asked, "brave mole over the hills".to_string());
        assert_eq!(fields(&ui)[at], "", "{target:?} back on its step");
        // a run view covers both steps
        ui.set_rw_step(1);
        frame();
        land_paste(&ui, target, ui.get_paste_opening(), "brave mole over the hills".to_string());
        assert_eq!(fields(&ui)[at], "", "{target:?} under the run view");
    }
}

/// The pasted text lands in the named field and in no other.
#[test]
fn a_paste_fills_the_named_field_and_no_other() {
    i_slint_backend_testing::init_no_event_loop();
    for (at, target) in TARGETS.into_iter().enumerate() {
        let ui = on_show(target);
        ui.set_rw_seed("kept one".into());
        ui.set_rv_link("kept two".into());
        ui.set_bk_restore_seed("kept three".into());
        ui.set_decrypt_phrase("kept four".into());
        let mut want = fields(&ui);

        land_paste(&ui, target, ui.get_paste_opening(), "brave mole over the hills".to_string());

        want[at] = "brave mole over the hills".to_string();
        assert_eq!(fields(&ui), want, "{target:?}");
    }
}

/// A fake clipboard: what `copy` took is what `paste` answers.
#[derive(Default)]
struct Board(Mutex<Option<String>>);

impl molt_ui_kit::platform::Platform for Board {
    fn copy(&self, text: String) {
        *self.0.lock().expect("the board") = Some(text);
    }
    fn paste(&self) -> molt_ui_kit::platform::Task<Option<String>> {
        let text = self.0.lock().expect("the board").clone();
        Box::pin(async move { text })
    }
    fn play_alert(&self, _kind: &str, _wav: fn(&str) -> Vec<u8>) {}
    fn unix_now(&self) -> u64 {
        0
    }
    fn monotonic(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    fn sleep(&self, _time: std::time::Duration) -> molt_ui_kit::platform::Task<()> {
        Box::pin(async {})
    }
    fn random_token(&self) -> Option<String> {
        None
    }
    fn pick_folder(&self, _start: String) -> molt_ui_kit::platform::Task<Option<String>> {
        Box::pin(async { None })
    }
    fn pick_file(&self, _pick: molt_ui_kit::platform::FilePick) -> molt_ui_kit::platform::Task<Option<String>> {
        Box::pin(async { None })
    }
    fn pick_save(&self, _name: String) -> molt_ui_kit::platform::Task<Option<String>> {
        Box::pin(async { None })
    }
    fn read_file(&self, _file: String) -> molt_ui_kit::platform::Task<std::io::Result<Vec<u8>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn save_file(&self, _save: molt_ui_kit::platform::Save) -> molt_ui_kit::platform::Task<std::io::Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }
}

/// The paste callback asks the platform it was wired with and hands the
/// answer on with the target the window named and the opening it was
/// asked from; an unreadable clipboard
/// leaves every field alone.
#[test]
fn the_paste_callback_reads_the_platform_and_keeps_its_target() {
    i_slint_backend_testing::init_no_event_loop();
    let rt = rt();
    let ui = AppWindow::new().expect("headless window");
    static BOARD: std::sync::OnceLock<Board> = std::sync::OnceLock::new();
    let board = BOARD.get_or_init(Board::default);
    let (landed, mut got) = futures_channel::mpsc::unbounded::<(PasteTarget, i32, String)>();
    crate::actions::clipboard::wire_with(&ui, rt.handle().clone().into(), board, move |target, opening, text| {
        let _ = landed.unbounded_send((target, opening, text));
    });
    ui.set_decrypt_modal_open(true);
    frame();
    let opening = ui.get_paste_opening();

    // nothing to paste: nothing is handed on
    ui.invoke_paste_clipboard(PasteTarget::RwSeed);
    ui.invoke_copy_text("one two three".into());
    ui.invoke_paste_clipboard(PasteTarget::DecryptPhrase);
    let first = rt.block_on(async {
        use futures_util::StreamExt as _;
        tokio::time::timeout(std::time::Duration::from_secs(10), got.next()).await
    });
    assert_eq!(first.expect("in time"), Some((PasteTarget::DecryptPhrase, opening, "one two three".to_string())));
}

/// Every paste button names its OWN field: the four sites are told apart
/// by the argument alone.
#[cfg(feature = "live-preview")]
#[test]
fn each_paste_button_names_its_own_field() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1400, 900));
    apply_strings(&ui, 0);
    let asked = Rc::new(RefCell::new(Vec::<PasteTarget>::new()));
    let a = asked.clone();
    ui.on_paste_clipboard(move |target| a.borrow_mut().push(target));
    let _shown = show_headless(&ui);
    let press = |label: slint::SharedString| {
        let mut found = i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label.as_str());
        let button = found.next().expect("the button renders");
        assert!(found.next().is_none(), "one {label} button at a time");
        click(&ui, &button);
    };
    let paste = ui.global::<Strings>().get_rw_paste();

    // the link field of the way in
    ui.set_screen(AppScreen::Restore);
    press(paste.clone());
    // its phrase step, reached with a recovery link
    ui.set_rv_link("molt://recover/x".into());
    ui.set_rv_verdict(LinkPreview { link: "molt://recover/x".into(), kind: 2, republic: "R".into(), who: "w".into() });
    press(ui.global::<Strings>().get_rw_continue());
    press(paste.clone());
    // the two phrase dialogs, over a screen with no paste button
    ui.set_screen(AppScreen::Choice);
    ui.set_bk_restore_open(true);
    press(paste.clone());
    ui.set_bk_restore_open(false);
    ui.set_decrypt_modal_open(true);
    press(paste);

    assert_eq!(
        asked.borrow().as_slice(),
        &[PasteTarget::RvLink, PasteTarget::RwSeed, PasteTarget::BkRestoreSeed, PasteTarget::DecryptPhrase]
    );
}
