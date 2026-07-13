// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
// Slint's generated code (via `include_modules!`) is injected into this crate
// and uses `as` casts, `unwrap`s, float layout math and a `todo!()` embed-stub
// that our (money-crate-oriented) workspace lints flag. The allows are scoped
// to this generated-code-only crate; the handwritten GUI logic lives in
// `molt-ui` and keeps its own, narrower posture.
#![allow(
    clippy::as_conversions,
    clippy::unwrap_used,
    clippy::float_arithmetic,
    clippy::todo
)]

//! `molt-ui-window`: the Slint-compiled window, and nothing else.
//!
//! This crate exists purely as a **compile-time firewall**: `slint-build`
//! turns `ui/app.slint` into a ~400k-line Rust module whose single rustc
//! peaks at several GiB. Keeping it in its own crate means that cost is paid
//! only when a `.slint` file actually changes — editing the GUI *logic*
//! (`molt-ui`) recompiles in seconds. Everything Slint generates (the
//! `AppWindow` component, the `Strings`/`Theme` globals, the row structs) is
//! re-exported at this crate's root; `molt-ui` glob-imports it.

slint::include_modules!();
