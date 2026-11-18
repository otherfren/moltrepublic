// SPDX-License-Identifier: GPL-3.0-or-later
//! `molt-web`: the browser build of the GUI (`docs/web_gui.md` §4, W5).
//!
//! The same window as the native one, compiled for
//! `wasm32-unknown-unknown`, against the engine behind `moltd`'s web door.
//! Nothing of the engine runs here: the crate holds the entry the page
//! calls ([`start`](fn@crate::start::start) on wasm32), the browser's
//! WebSocket under `molt-ui-kit`'s protocol pump, and the browser's half
//! of the platform seam (clipboard, alert, clocks, random token, files).
//!
//! Natively the crate is [`rules`] alone, with no dependency: what the
//! page decides without a browser, so it is tested like any other code.

pub mod rules;

#[cfg(target_arch = "wasm32")]
mod platform;
#[cfg(target_arch = "wasm32")]
mod socket;
#[cfg(target_arch = "wasm32")]
pub mod start;
