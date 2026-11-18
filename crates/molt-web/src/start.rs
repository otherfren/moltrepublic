// SPDX-License-Identifier: GPL-3.0-or-later
//! What the page calls (`docs/web_gui.md` §4, §5.3): `start` once it
//! holds the seat key, `reconnect` with a key after a connection ended,
//! `page_text` for its own labels. The key is asked on every load and
//! lives in this module's memory only.

use std::cell::RefCell;

use futures_channel::mpsc;
use futures_util::StreamExt;
use molt_ui_kit::client::{remote, Ended, Pump, Signal};
use wasm_bindgen::prelude::*;

use crate::platform::WebPlatform;
use crate::rules;
use crate::socket::CloseCode;

thread_local! {
    /// Where [`reconnect`] hands the next key to the connection task.
    static NEXT_KEY: RefCell<Option<mpsc::UnboundedSender<String>>> = const { RefCell::new(None) };
}

/// Start the window on the canvas with id `canvas`, against the door this
/// page was served from.
///
/// `notify` is called with one word as the connection changes:
/// `"ready"` each time the server welcomed this bundle (again after a
/// rotated key moved the connection), `"lag"` when the server dropped
/// events (the window re-reads, nothing to do), and when a connection
/// ended for good one of `"refused"` (the key), `"protocol"`, `"closed"`
/// or `"version"` (the server is another build). After `"version"` the
/// page must reload; after the other three it may call [`reconnect`].
///
/// Call it ONCE per page load. It does not return normally: the event
/// loop takes over by throwing, which the caller must expect. An `Err` is
/// a start that did not happen.
///
/// # Errors
/// A second call, a page not served over HTTP, or a window Slint could
/// not open.
#[wasm_bindgen]
pub fn start(token: String, notify: js_sys::Function) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let url = rules::socket_url(&location.protocol()?, &location.host()?).ok_or("not served over http")?;
    // before any window code: the window reads its clocks through it
    if !molt_ui_kit::platform::install(Box::new(WebPlatform)) {
        return Err("already started".into());
    }
    // a panic would otherwise end the tab's window without a word
    std::panic::set_hook(Box::new(|info| web_sys::console::error_1(&info.to_string().into())));
    let (engine, pump) = remote();
    let (next_key, keys) = mpsc::unbounded();
    NEXT_KEY.with(|slot| *slot.borrow_mut() = Some(next_key));
    // commands wait in the handle until the server's welcome
    wasm_bindgen_futures::spawn_local(connections(url, token, pump, notify, keys));
    let lang = rules::lang_of(&window.navigator().language().unwrap_or_default());
    molt_ui::run_remote(engine, molt_ui::Spawner, lang, follow_viewport).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Connect again with `token`, after a connection ended with `"refused"`,
/// `"protocol"` or `"closed"`. The window is the same one: it re-reads
/// everything once the server welcomed the key. `false`: nothing was
/// started, or the page must reload (`"version"`).
#[wasm_bindgen]
pub fn reconnect(token: String) -> bool {
    NEXT_KEY.with(|slot| slot.borrow().as_ref().is_some_and(|next| next.unbounded_send(token).is_ok()))
}

/// The page's own labels in the browser's `language`, as an object of
/// name to text (`molt_ui::page_text`; `help` holds one line per limit).
#[wasm_bindgen]
#[must_use]
pub fn page_text(language: &str) -> js_sys::Object {
    let table = js_sys::Object::new();
    for (name, text) in molt_ui::page_text(rules::lang_of(language)) {
        // a plain object takes any string key
        drop(js_sys::Reflect::set(&table, &name.into(), &text.into()));
    }
    table
}

/// Serve the engine handle, one connection after another, for as long as
/// the page hands keys: each end is told to the page, which asks again.
async fn connections(
    url: String,
    mut token: String,
    pump: Pump,
    notify: js_sys::Function,
    mut keys: mpsc::UnboundedReceiver<String>,
) {
    // the server names its build first: another one than this bundle's
    // ends the connection inside the pump, before a command crosses (§5.1)
    let mut pump = pump.only_build(env!("CARGO_PKG_VERSION"));
    let closed = CloseCode::default();
    loop {
        let dial = || {
            let socket = crate::socket::open(&url, &closed);
            async move { socket.map(|s| (s.incoming, s.outgoing)) }
        };
        let heard = |signal| match signal {
            Signal::Welcomed(_) => tell(&notify, rules::READY),
            Signal::Lagged => tell(&notify, rules::LAG),
        };
        // a rotated key is followed inside: the pump redials with it
        let why = match pump.serve(&token, dial, heard).await {
            Ended::Version => rules::VERSION,
            ended => rules::why_ended(closed.get(), matches!(ended, Ended::Broken(_))),
        };
        tell(&notify, why);
        if why == rules::VERSION {
            // this bundle cannot serve that node: only a reload helps
            NEXT_KEY.with(|slot| slot.borrow_mut().take());
            return;
        }
        match keys.next().await {
            Some(next) => token = next,
            None => return,
        }
    }
}

/// Size the window to the viewport, now and whenever that changes. This
/// is Slint's own sizing (it sets the canvas's inline size); a CSS rule
/// stretching the canvas fights it (`docs/web_gui.md` §8.4).
fn follow_viewport(sizer: molt_ui::Sizer) {
    let fit = move || {
        let Some(window) = web_sys::window() else { return };
        let side = |read: Result<JsValue, JsValue>| read.ok().and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
        if let Some((width, height)) = rules::window_size(side(window.inner_width()), side(window.inner_height())) {
            sizer.set(width, height);
        }
    };
    fit();
    let Some(window) = web_sys::window() else { return };
    let on_resize = Closure::<dyn Fn()>::new(fit);
    // once more when the event loop runs: sized before it is shown, the
    // window kept its preferred width (seen in the first browser run)
    drop(window.set_timeout_with_callback_and_timeout_and_arguments_0(on_resize.as_ref().unchecked_ref(), 0));
    if window.add_event_listener_with_callback("resize", on_resize.as_ref().unchecked_ref()).is_ok() {
        // the listener lives as long as the page
        on_resize.forget();
    }
}

/// Hand the page one word. A page callback that throws is the page's.
fn tell(notify: &js_sys::Function, word: &str) {
    drop(notify.call1(&JsValue::NULL, &JsValue::from_str(word)));
}
