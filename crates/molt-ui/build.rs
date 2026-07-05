// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
fn main() {
    // All controls are hand-rolled (no std-widgets), so no widget style is
    // needed — the look comes entirely from our own `Theme` palettes.
    slint_build::compile("ui/app.slint").expect("compile ui/app.slint");
}
