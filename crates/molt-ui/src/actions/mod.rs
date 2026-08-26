// SPDX-License-Identifier: GPL-3.0-or-later
//! The window's callback wiring, grouped by screen/surface. Every handler
//! turns a click into a [`molt_core::Command`] on the shared engine (or a
//! UI-local change followed by a surfaces re-read) - the GUI drives the
//! same command set an MCP agent does, co-equal.

pub(crate) mod relays;
pub(crate) mod settings;
pub(crate) mod workspace;
