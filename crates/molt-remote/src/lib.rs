// SPDX-License-Identifier: GPL-3.0-or-later
//! The web door (`docs/web_gui.md`): what a browser GUI may send to the
//! engine, and who may open the socket. A second remote door onto the
//! seat beside MCP - same seat key, same command set, fail-closed.

pub mod gate;
pub mod origin;
mod server;
mod session;

pub use server::{serve, serve_loopback, WebDoor, BUNDLE_FILES};
