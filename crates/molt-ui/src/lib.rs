// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
// The handwritten GUI logic casts small ints to Slint's `i32`, does float
// label math, and drives Slint APIs that return `Option`s we unwrap; the
// allows are scoped to this UI crate only, so the rest of the workspace
// keeps the strict posture. (Slint's GENERATED code lives in molt-ui-window
// with its own allow header.)
#![allow(
    clippy::as_conversions,
    clippy::unwrap_used,
    clippy::float_arithmetic,
    clippy::todo
)]

//! `molt-ui`: the GUI operator.
//!
//! This crate hosts the multi-stage front of the node — a first-run wizard
//! (create / open / join / restore), a shared completion screen, the main
//! surfaces view, and a settings panel. The settings are real (they persist
//! to the node's `config.toml` and mirror external edits of it); the
//! workspace lifecycles are real — create/open/join/close write to disk.
//!
//! The GUI is a **live-mirror of the engine's shared session**, not a holder of
//! its own state. Every action (navigate, switch language, save settings, finish
//! a wizard) is turned into a [`molt_core::Command`] on the shared
//! [`Engine`]; a background task re-reads the session on each
//! [`molt_core::Event::SessionChanged`] and pushes it back into the Slint
//! properties. An MCP agent issuing the *same* commands drives this *same* state,
//! so the GUI and the MCP operator are co-equal — exactly as for the surfaces.
//!
//! Module map (review 2026-08-25 F11):
//! - `app` — [`run_app`]: build the window, wire, mirror, run; `Ctx` is what
//!   every callback captures.
//! - `actions/{settings,workspace,relays,ritual,chat,org}` — the callback
//!   wiring, one `wire(ui, ctx)` per screen/surface.
//! - `mirror` — the live mirror: session/runs/relays/surfaces pushed into the
//!   window, the UI snapshot publish, the mirror task.
//! - `surfaces` — the `Send` bundle a mirror pass gathers, the UI-local
//!   chat-bus state, the proposal/chain/table row projections.
//! - `chat_log`, `channels` — the chat pane's rows and the chat bus's
//!   channels + proposal cache.
//! - `settings` — the settings draft (read/apply/dirty/save).
//! - `native` — the one module that names tokio and the operating
//!   system: the spawner behind `Ctx::spawn`, off-thread work, and the
//!   native half of the platform seam.
//! - `platform` — the door to that seam: clipboard, alert, clocks, sleep,
//!   random token and files (`docs/web_gui.md` §6.3).
//! - `bytes` — file content to and from the engine: by path for the
//!   engine in this process, through the byte doors for one behind the
//!   web door (`docs/web_gui.md` §7).
//! - `models` — the one in-place `VecModel` patch (`sync_model`).
//! - `images`, `labels`, `i18n`, `alerts`, `net_tor` — pictures, rendered
//!   prose, localization, alert sounds, the anonymity panel.
//! - `wiki`, `patchview`, `wiki_bridge` — the Shared-Memory wiki's state
//!   machine, its diff viewer, and their Slint bridge.
//! - `tests/` — the unit tests per module and the headless GUI tests.

// The Slint-generated window (AppWindow, the Strings/Theme globals, every
// row struct) lives in its own crate as a compile-time firewall — see
// molt-ui-window's crate docs. The glob keeps this crate's code reading as
// if the module were still injected here.
pub use molt_ui_window::*;

pub use app::run_app;
pub use engine::Engine;
pub use native::Spawner;

mod actions;
mod alerts;
mod app;
mod bytes;
mod channels;
mod chat_log;
mod download_status;
mod engine;
mod i18n;
mod images;
mod kanban;
mod kanban_views;
mod labels;
mod log_view;
mod mirror;
mod models;
// the one module the OS and runtime calls of `clippy.toml` are allowed in
// (`docs/web_gui.md` §8.3)
#[allow(clippy::disallowed_methods)]
mod native;
mod net_tor;
mod patchview;
mod platform;
mod settings;
mod surfaces;
mod wallet;
mod wiki;
mod wiki_bridge;

// tests set up files, runtimes and waits of their own; they never ship
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests;
