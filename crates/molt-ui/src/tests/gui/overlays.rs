// SPDX-License-Identifier: GPL-3.0-or-later
//! Overlay captions fit their dialogs: wrapped, never cut, at UI scales
//! 1.0 and 1.5, in English and German (field report on v0.1.3).

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

/// `(x, y, width, height)`.
type Rect = (f32, f32, f32, f32);

fn rect(e: &Handle) -> Rect {
    (e.absolute_position().x, e.absolute_position().y, e.size().width, e.size().height)
}

fn one(ui: &AppWindow, id: &str) -> Rect {
    Handle::find_by_element_id(ui, id)
        .map(|e| rect(&e))
        .find(|r| r.2 > 0.0)
        .unwrap_or_else(|| panic!("{id} renders"))
}

fn labelled(ui: &AppWindow, label: &str) -> Rect {
    Handle::find_by_accessible_label(ui, label)
        .map(|e| rect(&e))
        .find(|r| r.2 > 0.0)
        .unwrap_or_else(|| panic!("{label:?} renders"))
}

/// The element carrying `label` that lies within `outer`.
fn labelled_in(ui: &AppWindow, label: &str, outer: Rect) -> Rect {
    Handle::find_by_accessible_label(ui, label)
        .map(|e| rect(&e))
        .find(|r| r.2 > 0.0 && r.1 >= outer.1 - 0.5 && r.1 < outer.1 + outer.3)
        .unwrap_or_else(|| panic!("{label:?} renders in {outer:?}"))
}

fn inside(what: &str, inner: Rect, outer: Rect) {
    assert!(
        inner.0 >= outer.0 - 0.5
            && inner.1 >= outer.1 - 0.5
            && inner.0 + inner.2 <= outer.0 + outer.2 + 0.5
            && inner.1 + inner.3 <= outer.1 + outer.3 + 0.5,
        "{what}: {inner:?} outside {outer:?}"
    );
}

fn window(width: u32, font: f32, lang: i32) -> (AppWindow, Shown) {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(width, 900));
    ui.set_screen(AppScreen::Main);
    ui.global::<Theme>().set_fs_app(font);
    apply_strings(&ui, lang);
    let shown = show_headless(&ui);
    rail_settled();
    (ui, shown)
}

const CONFIGS: [(u32, f32, i32); 8] = [
    (900, 14.0, 0),
    (900, 14.0, 1),
    (900, 21.0, 0),
    (900, 21.0, 1),
    (1400, 14.0, 0),
    (1400, 14.0, 1),
    (1400, 21.0, 0),
    (1400, 21.0, 1),
];

/// The closing overlay's sentence wraps inside its card instead of being
/// cut: it takes more than one line and stays within the card.
#[test]
fn the_closing_overlay_wraps_its_caption() {
    i_slint_backend_testing::init_no_event_loop();
    for (width, font, lang) in CONFIGS {
        let (ui, _shown) = window(width, font, lang);
        ui.set_closing(true);
        let caption = Lexicon::of(lang).close_busy;
        let text = labelled(&ui, caption);
        let card = one(&ui, "BusyModal::bm-card");
        let line = ui.global::<Theme>().get_fs_h2();
        eprintln!("closing {width}px fs {font} lang {lang}: text {text:?} card {card:?} fs-h2 {line}");
        inside(&format!("{width}px fs {font} lang {lang}"), text, card);
        // where the sentence cannot fit on one line it must wrap, not cut
        if font < 15.0 || lang == 1 {
            assert!(text.3 > 1.5 * line, "{width}px fs {font} lang {lang}: one cut line, not wrapped: {text:?}");
        }
        inside("card", card, (0.0, 0.0, width as f32, 900.0));
    }
}

/// The sweep: the other busy dialog, confirm dialogs with long captions
/// and a long toast, at scale 1.5 in German at the narrow width.
#[test]
fn long_captions_fit_their_dialogs_at_scale_one_and_a_half_in_german() {
    i_slint_backend_testing::init_no_event_loop();
    let long = "Die Allmende-Genossenschaft der Gemüsegärtnerinnen und Imker vom unteren Flusstal";
    for (width, font) in [(900, 21.0), (1400, 21.0)] {
        // the join's busy dialog, titled with the republic's name
        let (ui, _shown) = window(width, font, 1);
        ui.set_screen(AppScreen::Join);
        ui.set_jw_step(1);
        ui.set_jw_republic(long.into());
        let card = one(&ui, "BusyModal::bm-card");
        inside("join busy title", labelled_in(&ui, long, card), card);
        inside("join busy card", card, (0.0, 0.0, width as f32, 900.0));
        drop(_shown);

        // a confirm dialog with a long title (the block format switch)
        let (ui, _shown) = window(width, font, 1);
        ui.set_org_bv2_modal_open(true);
        let card = one(&ui, "ConfirmModal::cm-card");
        inside("switch title", labelled(&ui, Lexicon::de().os_bv2_switch), card);
        inside("switch price", labelled(&ui, Lexicon::de().os_bv2_price), card);
        inside("switch card", card, (0.0, 0.0, width as f32, 900.0));
        drop(_shown);

        // the resync confirm names a long seat
        let (ui, _shown) = window(width, font, 1);
        ui.set_os_resync_member("eine-sehr-lange-sitzbezeichnung-ohne-leerzeichen".into());
        let card = one(&ui, "ConfirmModal::cm-card");
        inside("resync card", card, (0.0, 0.0, width as f32, 900.0));
        drop(_shown);

        // a long toast wraps inside its bubble
        let (ui, _shown) = window(width, font, 1);
        let msg = format!("{long}: {long}");
        ui.invoke_show_toast(msg.as_str().into());
        let bubble = one(&ui, "AppWindow::toast-box");
        let label = labelled(&ui, &msg);
        inside("toast text", label, bubble);
        assert!(label.3 > 1.5 * ui.global::<Theme>().get_fs_body(), "the toast wraps: {label:?}");
    }
}
