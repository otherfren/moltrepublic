// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The pure-Rust guard for the wiki INDEX dependencies
//! (`docs/memory/knowledge_base_scale.md` §4.6), the sibling of
//! `molt-net/tests/ring_free_guard.rs`.
//!
//! `tantivy`'s DEFAULT feature set pulls C `zstd` through
//! `columnar-zstd-compression`; the workspace therefore pins
//! `default-features = false`. The trapdoor is silent - a feature unified
//! in by some other crate re-adopts the C toolchain without a word - so
//! the posture is a test, not a comment.

use std::process::Command;

/// `cargo tree --locked -e no-dev --target all -i <pkg>`: stdout is the
/// inverted dependency tree - EMPTY when nothing in the default (no-dev)
/// graph of `root`, on any target, depends on the package.
fn inverse_no_dev_deps(root: &str, pkg: &str) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "tree", "--locked", "-p", root, "-e", "no-dev", "--target", "all", "-i", pkg,
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .expect("cargo tree runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// KEYSTONE - the wiki index brought no C library into the default build
/// graph. A red here is an explicit dependency decision to make, never a
/// side effect of adding a crate.
#[test]
fn the_wiki_index_pulls_no_c_toolchain() {
    // the mechanism must be able to SEE a package that is in the graph,
    // or the emptiness below would be a blind pass
    assert!(
        !inverse_no_dev_deps("molt-engine", "tantivy").trim().is_empty(),
        "cargo tree stopped reporting inverse deps - this guard is blind"
    );
    for pkg in ["zstd-sys", "zstd", "libsqlite3-sys", "ring", "openssl-sys"] {
        for root in ["molt-engine", "molt-app"] {
            let tree = inverse_no_dev_deps(root, pkg);
            assert!(
                tree.trim().is_empty(),
                "`{pkg}` entered the default build graph of {root}:\n{tree}"
            );
        }
    }
}
