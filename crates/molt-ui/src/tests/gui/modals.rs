// SPDX-License-Identifier: GPL-3.0-or-later
//! **Every modal dialog opens ready to type.** One table over the
//! dialogs that carry an input: the first input has the focus, Tab walks
//! on, and Enter goes through the confirm button's gate - never past it.
//!
//! The dialogs are raised through their own `…-open` flag, which is what
//! the button behind them sets; that is why those flags are `in-out`.

use super::*;

/// A window with nothing on screen but the dialog under test: the
/// welcome screen carries no input of its own, so every `TextInput` the
/// query finds belongs to the modal.
#[cfg(feature = "live-preview")]
fn dialog_window() -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    apply_strings(&ui, 0);
    ui.window().set_size(slint::PhysicalSize::new(1100, 800));
    ui.show().expect("show headless");
    ui
}

/// One frame, so a freshly created dialog runs its `init`.
#[cfg(feature = "live-preview")]
fn frame() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
}

/// The open dialog's inputs, in visual (tree) order.
#[cfg(feature = "live-preview")]
fn inputs(ui: &AppWindow) -> Vec<i_slint_backend_testing::ElementHandle> {
    let modal = i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "ConfirmModal")
        .next()
        .expect("the dialog is up");
    modal.query_descendants().match_type_name("TextInput").find_all()
}

/// What the input at `i` holds (`TextInput` publishes its text as the
/// accessible value).
#[cfg(feature = "live-preview")]
fn held(ui: &AppWindow, i: usize) -> String {
    inputs(ui)
        .get(i)
        .and_then(i_slint_backend_testing::ElementHandle::accessible_value)
        .map(|v| v.to_string())
        .unwrap_or_default()
}

#[cfg(feature = "live-preview")]
fn type_text(ui: &AppWindow, text: &str) {
    for c in text.chars() {
        type_char(ui, &c.to_string());
    }
}

/// Ctrl+Return: the modifier is a key event of its own, which is how
/// Slint's window learns the modifier state.
#[cfg(feature = "live-preview")]
fn ctrl_return(ui: &AppWindow) {
    let ctrl = slint::SharedString::from(slint::platform::Key::Control);
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: ctrl.clone() });
    press(ui, slint::platform::Key::Return);
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::KeyReleased { text: ctrl });
}

/// One dialog under test.
#[cfg(feature = "live-preview")]
struct Dialog {
    what: &'static str,
    /// how many inputs it carries, in visual order
    inputs: usize,
    /// raise it exactly as its own button does
    open: Box<dyn Fn(&AppWindow)>,
    /// is it still up?
    up: Box<dyn Fn(&AppWindow) -> bool>,
    /// what to type into the field it opens on
    first: &'static str,
    /// the field that still gates the confirm: Tab steps away, and what
    /// to type there
    also: Option<(usize, &'static str)>,
    /// a dialog whose confirming field is multi-line keeps Return for
    /// the newline and confirms on Ctrl+Return
    ctrl: bool,
}

#[cfg(feature = "live-preview")]
fn table() -> Vec<Dialog> {
    vec![
        Dialog {
            what: "delete workspace",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| {
                ui.set_delete_name("DevTest".into());
                ui.set_confirm_delete_open(true);
            }),
            up: Box::new(AppWindow::get_confirm_delete_open),
            first: "DevTest",
            also: None,
            ctrl: false,
        },
        Dialog {
            what: "new chat topic",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_nt_open(true)),
            up: Box::new(AppWindow::get_nt_open),
            first: "plans",
            also: None,
            ctrl: false,
        },
        Dialog {
            what: "manual backup",
            inputs: 2,
            open: Box::new(|ui: &AppWindow| ui.set_backup_modal_open(true)),
            up: Box::new(AppWindow::get_backup_modal_open),
            first: "~/b.enc",
            // the export passphrase has a ten-character floor
            also: Some((1, "0123456789")),
            ctrl: false,
        },
        Dialog {
            what: "restore from bucket",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_bk_restore_open(true)),
            up: Box::new(AppWindow::get_bk_restore_open),
            first: "one two three",
            also: None,
            ctrl: false,
        },
        Dialog {
            what: "republic image",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_org_logo_modal_open(true)),
            up: Box::new(AppWindow::get_org_logo_modal_open),
            first: "~/logo.png",
            also: None,
            ctrl: false,
        },
        Dialog {
            what: "member image",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_mp_img_open(true)),
            up: Box::new(AppWindow::get_mp_img_open),
            first: "~/me.png",
            also: None,
            ctrl: false,
        },
        Dialog {
            what: "seal a secret",
            inputs: 3,
            open: Box::new(|ui: &AppWindow| ui.set_vt_seal_open(true)),
            up: Box::new(AppWindow::get_vt_seal_open),
            first: "keys",
            // title, description, and the secret itself two Tabs on
            also: Some((2, "s3cret")),
            ctrl: true,
        },
        Dialog {
            what: "member description",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_mp_desc_open(true)),
            up: Box::new(AppWindow::get_mp_desc_open),
            first: "builder",
            also: None,
            ctrl: true,
        },
        Dialog {
            what: "seal / unseal at rest",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_decrypt_modal_open(true)),
            up: Box::new(AppWindow::get_decrypt_modal_open),
            first: "one two three",
            also: None,
            ctrl: false,
        },
        Dialog {
            what: "rename the republic",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_org_name_modal_open(true)),
            up: Box::new(AppWindow::get_org_name_modal_open),
            first: "Nova",
            also: None,
            ctrl: false,
        },
        Dialog {
            what: "edit the charter",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_org_charter_modal_open(true)),
            up: Box::new(AppWindow::get_org_charter_modal_open),
            first: "what we agreed",
            also: None,
            ctrl: true,
        },
        Dialog {
            what: "workspace folder",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_ws_dir_modal_open(true)),
            up: Box::new(AppWindow::get_ws_dir_modal_open),
            first: "~/molt",
            also: None,
            ctrl: false,
        },
        Dialog {
            what: "wiki export",
            inputs: 1,
            open: Box::new(|ui: &AppWindow| ui.set_mem_export_open(true)),
            up: Box::new(AppWindow::get_mem_export_open),
            first: "~/out",
            also: None,
            ctrl: false,
        },
    ]
}

/// **The user's requirement, over every dialog that has a field.** It
/// opens with that field in edit focus, Tab reaches the next one, and
/// Enter fires the confirm only once the confirm is enabled.
#[cfg(feature = "live-preview")]
#[test]
fn every_dialog_opens_on_its_first_input_and_enter_confirms() {
    let ui = dialog_window();
    for d in table() {
        // a confirm may navigate (the bucket restore does): every dialog
        // starts from the input-free welcome screen
        ui.set_screen(AppScreen::Choice);
        (d.open)(&ui);
        frame();
        assert!((d.up)(&ui), "{}: it is up", d.what);
        assert_eq!(inputs(&ui).len(), d.inputs, "{}: its inputs render", d.what);

        // nothing typed yet, so the confirm is disabled - the key must
        // not fire it
        if d.ctrl {
            ctrl_return(&ui);
        } else {
            press(&ui, slint::platform::Key::Return);
        }
        assert!((d.up)(&ui), "{}: a disabled confirm does not fire", d.what);

        type_text(&ui, d.first);
        assert_eq!(held(&ui, 0), d.first, "{}: the first input has the focus", d.what);

        if let Some((steps, text)) = d.also {
            for _ in 0..steps {
                press(&ui, slint::platform::Key::Tab);
            }
            type_text(&ui, text);
            assert_eq!(held(&ui, steps), text, "{}: Tab walks to input {steps}", d.what);
            assert_eq!(held(&ui, 0), d.first, "{}: …and leaves the first alone", d.what);
        }

        if d.ctrl {
            // Return belongs to the text here
            press(&ui, slint::platform::Key::Return);
            assert!((d.up)(&ui), "{}: Return writes a newline, it does not confirm", d.what);
            ctrl_return(&ui);
        } else {
            press(&ui, slint::platform::Key::Return);
        }
        assert!(!(d.up)(&ui), "{}: Enter confirms once it is enabled", d.what);
        frame();
    }
}

/// Escape cancels every one of them, from inside the field.
#[cfg(feature = "live-preview")]
#[test]
fn escape_cancels_every_dialog_from_inside_its_field() {
    let ui = dialog_window();
    for d in table() {
        ui.set_screen(AppScreen::Choice);
        (d.open)(&ui);
        frame();
        type_text(&ui, "x");
        press(&ui, slint::platform::Key::Escape);
        assert!(!(d.up)(&ui), "{}: Escape cancels", d.what);
        frame();
    }
}

/// The relay-pool editor is the one dialog whose field Enter means
/// something else: it ADDS the typed relay, because a URL still sitting
/// in the field blocks the proposal anyway.
#[cfg(feature = "live-preview")]
#[test]
fn the_relay_field_adds_on_enter_rather_than_confirming() {
    let ui = dialog_window();
    let added: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let a = added.clone();
        ui.on_relays_draft_add(move |url| a.borrow_mut().push(url.to_string()));
    }
    ui.set_org_relays_modal_open(true);
    frame();
    type_text(&ui, "wss://r.example");
    assert_eq!(held(&ui, 0), "wss://r.example", "the add field has the focus");
    press(&ui, slint::platform::Key::Return);
    assert_eq!(
        added.borrow().as_slice(),
        ["wss://r.example".to_string()],
        "Enter adds the row"
    );
    assert!(ui.get_org_relays_modal_open(), "…and leaves the dialog up");
}

/// A dialog with NO input keeps the zero-sized key scope it always had:
/// Enter presses confirm, Escape cancels.
#[cfg(feature = "live-preview")]
#[test]
fn a_dialog_without_inputs_still_confirms_and_cancels_on_the_keys() {
    let ui = dialog_window();
    ui.set_confirm_quit_open(true);
    frame();
    assert_eq!(inputs(&ui).len(), 0, "it has no field at all");
    press(&ui, slint::platform::Key::Return);
    assert!(!ui.get_confirm_quit_open(), "Enter confirms");

    frame();
    ui.set_confirm_quit_open(true);
    frame();
    press(&ui, slint::platform::Key::Escape);
    assert!(!ui.get_confirm_quit_open(), "Escape cancels");
}

/// Is `text` the PLACEHOLDER of one of the dialog's fields - one label
/// carrying it, and that label inside a field's own box? A caption row
/// would be a second one, and would sit outside every field.
#[cfg(feature = "live-preview")]
fn placeholder_of_a_field(ui: &AppWindow, text: &str) -> bool {
    let modal = i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "ConfirmModal")
        .next()
        .expect("the dialog is up");
    let wanted = text.to_string();
    let hits = modal
        .query_descendants()
        .match_predicate(move |e| e.accessible_label().is_some_and(|l| l == wanted.as_str()))
        .find_all();
    if hits.len() != 1 {
        return false;
    }
    let at = hits[0].absolute_position();
    modal
        .query_descendants()
        .match_type_name("AppField")
        .find_all()
        .iter()
        .any(|f| {
            let p = f.absolute_position();
            let s = f.size();
            at.x >= p.x && at.x <= p.x + s.width && at.y >= p.y && at.y <= p.y + s.height
        })
}

/// The captions a placeholder already carries are gone from the dialogs
/// that stacked them: one row less each, at every app font.
#[cfg(feature = "live-preview")]
#[test]
fn the_dialogs_drop_the_captions_their_placeholders_carry() {
    let ui = dialog_window();
    let seen = |label: &str| {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label).count() > 0
    };
    ui.set_backup_modal_open(true);
    frame();
    assert!(!seen("Target file"), "an example path needs no caption");
    assert!(
        placeholder_of_a_field(&ui, "Export passphrase (min. 10 characters)"),
        "the passphrase's caption became its placeholder"
    );
    ui.set_backup_modal_open(false);
    frame();

    ui.set_bk_restore_open(true);
    frame();
    assert!(
        placeholder_of_a_field(&ui, "Recovery phrase"),
        "the phrase's caption became its placeholder"
    );
    assert!(!seen("word1 word2 word3 …"), "…and the example went with it");
    ui.set_bk_restore_open(false);
    frame();

    ui.set_vt_seal_open(true);
    frame();
    assert!(!seen("Secret"), "the secret's caption is its placeholder");
    assert!(
        seen("Title") && !placeholder_of_a_field(&ui, "Title"),
        "two look-alike fields keep their captions"
    );
}
