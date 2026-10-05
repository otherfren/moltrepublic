// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The vault's dependencies stay pure Rust: no `ring`, no C build step
//! (`cc`). Sibling of `molt-engine/tests/c_free_guard.rs`.

use std::process::Command;

/// `cargo tree --locked -e no-dev --target all -i <pkg>` for molt-vault:
/// EMPTY when nothing in its default graph depends on the package.
fn inverse_no_dev_deps(pkg: &str) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "tree",
            "--locked",
            "-p",
            "molt-vault",
            "-e",
            "no-dev",
            "--target",
            "all",
            "-i",
            pkg,
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .expect("cargo tree runs");
    // a package outside the graph exits non-zero with an empty stdout -
    // the same shape as "clean"; any other failure must not pass
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() || err.contains("did not match any packages"),
        "cargo tree failed for -i {pkg}: {err}"
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_vault_pulls_no_ring_and_no_c_toolchain() {
    assert!(
        !inverse_no_dev_deps("vsss-rs").trim().is_empty(),
        "cargo tree stopped reporting inverse deps - this guard is blind"
    );
    for pkg in ["ring", "cc"] {
        let tree = inverse_no_dev_deps(pkg);
        assert!(
            tree.trim().is_empty(),
            "`{pkg}` entered molt-vault's build graph:\n{tree}"
        );
    }
}
