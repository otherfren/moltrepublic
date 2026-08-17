// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
fn main() {
    // All controls are hand-rolled (no std-widgets), so no widget style is
    // needed — the look comes entirely from our own `Theme` palettes.
    //
    // Build-cost spike (2026-08-17): slint-build 1.17's experimental
    // `as_library()`/`rust_module()` CANNOT split this crate's ~400k-line
    // generated module — in the compiler, `from_library` is set only by
    // collect_globals.rs, so only GLOBALS cross module boundaries; every
    // used sub-component is still generated (and inlined) at the consumer.
    // Re-evaluate when upstream marks components as library-external.
    slint_build::compile("ui/app.slint").expect("compile ui/app.slint");
}
