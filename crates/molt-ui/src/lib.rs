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
//! [`WalletHandle`]; a background task re-reads the session on each
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

/// The Restore wizard's one link field: which flow a pasted link arms.
pub use actions::ritual::{link_kind, LinkKind};
pub use app::run_app;

mod actions;
mod alerts;
mod app;
mod channels;
mod chat_log;
mod i18n;
mod images;
mod labels;
mod mirror;
mod models;
mod net_tor;
mod patchview;
mod settings;
mod surfaces;
mod wiki;
mod wiki_bridge;

#[cfg(test)]
mod tests;
