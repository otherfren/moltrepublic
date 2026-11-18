// SPDX-License-Identifier: GPL-3.0-or-later
//! **Browser only** (and the tests): the twin of `native.rs` for a target
//! with one thread and no operating system (`docs/web_gui.md` §6.2). The
//! window's futures run on Slint's own event loop and "off-thread" work
//! runs where it is asked for. The browser's platform - clipboard, alert,
//! clocks, files - is not here: `molt-web` installs it before the window
//! starts (`molt_ui_kit::platform::install`).

use std::future::Future;

/// Where the window's futures run: the event loop of the one thread.
// natively only type-checked: the headless test backend has no event loop
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[derive(Clone, Debug, Default)]
pub struct Spawner;

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
impl Spawner {
    /// Run `task` to completion, detached. Called on the UI thread, once
    /// the window exists.
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        // dropping the join handle detaches; an error means no event loop
        if let Err(e) = slint::spawn_local(task) {
            tracing::error!(error = %e, "spawn failed");
        }
    }
}

/// Run `work` here: there is no second thread. Always answers.
#[allow(clippy::unused_async)] // the native twin awaits a worker
pub(crate) async fn off_thread<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> Option<R> {
    Some(work())
}
