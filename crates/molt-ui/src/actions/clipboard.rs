// SPDX-License-Identifier: GPL-3.0-or-later
//! The clipboard callbacks: copy a text, copy a run log, and the paste
//! buttons. A paste is asynchronous on every target (`docs/web_gui.md`
//! §6.3): the window names the field, [`land_paste`] fills exactly that
//! one when the clipboard answers.

use molt_ui_kit::platform::Platform;
use slint::{ComponentHandle, Model};

use crate::app::Ctx;
use crate::native::Spawner;
use crate::{AppWindow, PasteTarget};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    let weak = ctx.weak.clone();
    wire_with(ui, ctx.spawner.clone(), crate::platform::platform(), move |target, opening, text| {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                land_paste(&ui, target, opening, text);
            }
        });
    });
}

/// [`wire`] with its parts named: the clipboard is `platform`'s, and a
/// pasted text is handed to `land` with the field the window asked for
/// and the opening (`paste-opening`) it was asked from.
pub(crate) fn wire_with(
    ui: &AppWindow,
    spawner: Spawner,
    platform: &'static dyn Platform,
    land: impl Fn(PasteTarget, i32, String) + Clone + Send + 'static,
) {
    ui.on_copy_text(move |text| platform.copy(text.to_string()));
    let weak = ui.as_weak();
    ui.on_paste_clipboard(move |target| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let opening = ui.get_paste_opening();
        // asked here, on the UI thread, where a native clipboard lives
        let read = platform.paste();
        let land = land.clone();
        spawner.spawn(async move {
            if let Some(text) = read.await {
                land(target, opening, text);
            }
        });
    });
    // Copy one of the (session-mirrored) run logs as one text block.
    let weak = ui.as_weak();
    ui.on_copy_log(move |which| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let log = match which.as_str() {
            "create" => ui.get_cw_log(),
            "join" => ui.get_jw_log(),
            _ => ui.get_rw_log(),
        };
        let text = log.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        platform.copy(text);
    });
}

/// Put a pasted `text` into the field `target` names - that one and no
/// other: two of the four hold a recovery phrase. The answer may be late
/// (a browser asks first), so it lands only in the opening it was asked
/// from: the field is on show now, and `opening` is still the window's
/// `paste-opening`. A dialog reopened meanwhile serves another workspace
/// or backup, a wizard another step - a phrase must reach neither.
pub(crate) fn land_paste(ui: &AppWindow, target: PasteTarget, opening: i32, text: String) {
    let on_show = match target {
        PasteTarget::RwSeed => ui.get_rw_seed_on_show(),
        PasteTarget::RvLink => ui.get_rv_link_on_show(),
        PasteTarget::BkRestoreSeed => ui.get_bk_restore_open(),
        PasteTarget::DecryptPhrase => ui.get_decrypt_modal_open(),
    };
    if !on_show || opening != ui.get_paste_opening() {
        return;
    }
    let text = slint::SharedString::from(text);
    match target {
        PasteTarget::RwSeed => ui.set_rw_seed(text),
        PasteTarget::RvLink => ui.set_rv_link(text),
        PasteTarget::BkRestoreSeed => ui.set_bk_restore_seed(text),
        PasteTarget::DecryptPhrase => ui.set_decrypt_phrase(text),
    }
}
