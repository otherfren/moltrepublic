// SPDX-License-Identifier: GPL-3.0-or-later
//! What the GUI needs on every target (`docs/web_gui.md` §4). Wasm-clean:
//! depends on `molt-core` only, and nothing below `molt-ui` depends on it.

pub mod client;
pub mod protocol;
