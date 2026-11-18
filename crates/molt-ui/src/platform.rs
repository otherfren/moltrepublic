// SPDX-License-Identifier: GPL-3.0-or-later
//! The window's door to the machine it runs on (`docs/web_gui.md` §6.3):
//! clipboard, alert, clocks, sleep, random token and files all go through
//! [`platform`]. Shared code names no OS facility itself.

use molt_ui_kit::platform::Platform;

/// The process's platform: the installed one, else this target's own.
pub(crate) fn platform() -> &'static dyn Platform {
    molt_ui_kit::platform::get_or_install(crate::native::platform)
}

/// The wall clock as a UTC date-time.
pub(crate) fn utc_now() -> chrono::DateTime<chrono::Utc> {
    i64::try_from(platform().unix_now())
        .ok()
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .unwrap_or_default()
}
