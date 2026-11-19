// SPDX-License-Identifier: GPL-3.0-or-later
//! The window's door to the machine it runs on (`docs/web_gui.md` §6.3):
//! clipboard, alert, clocks, sleep, random token and files all go through
//! [`platform`]. Shared code names no OS facility itself.

use molt_ui_kit::platform::Platform;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use crate::native::{off_thread, Spawner};
#[cfg(target_arch = "wasm32")]
pub(crate) use crate::web::{off_thread, Spawner};

/// The process's platform: the installed one, else this target's own.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn platform() -> &'static dyn Platform {
    molt_ui_kit::platform::get_or_install(crate::native::platform)
}

/// The browser's platform. `molt-web` installs it before it opens the
/// window, so a missing one is a broken entry point, not a state to run
/// in: every clock would read zero and every file dialog stay shut.
#[cfg(target_arch = "wasm32")]
pub(crate) fn platform() -> &'static dyn Platform {
    molt_ui_kit::platform::installed().expect("the platform is installed before the window opens")
}

/// The wall clock as a UTC date-time.
pub(crate) fn utc_now() -> chrono::DateTime<chrono::Utc> {
    i64::try_from(platform().unix_now())
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .unwrap_or_default()
}
