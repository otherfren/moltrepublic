// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The ring guard as a test, not prose (`mdk_evaluation.md` §7.6): the
//! DEFAULT (no-dev) build graph must stay free of the `ring` C library —
//! ring-free since etappe N-demo deleted the SMP cert-pin. The standing
//! trapdoor: rust-nostr's relay pool (`nostr-relay-pool` → `async-wsocket`)
//! hard-pins a ring-flavored `tokio-rustls`, and adopting it was REJECTED in
//! ADR-0005 (the N2 runtime drives tokio-tungstenite over rustls-rustcrypto
//! instead). This test turns CLAUDE.md's "`cargo tree … -i ring` must stay
//! empty" into a red build.

use std::process::Command;

/// `cargo tree --locked -e no-dev --target all -i <pkg>`: stdout is the
/// inverted dependency tree — EMPTY when nothing in the default (no-dev)
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

/// KEYSTONE — the default build graph is ring-free, from the transport crate
/// up to the shipped binary. If this goes red, a dependency re-adopted
/// `ring`; that is an explicit product decision (ADR-0005 already said no),
/// never a side effect of adding a crate.
#[test]
fn the_default_build_graph_is_ring_free() {
    // the mechanism must be able to see a package that IS in the graph —
    // otherwise the emptiness below would be a blind pass (e.g. after a
    // cargo-tree output change or a --locked failure)
    assert!(
        !inverse_no_dev_deps("molt-net", "tokio").trim().is_empty(),
        "cargo tree stopped reporting inverse deps — the ring guard is blind"
    );
    for root in ["molt-net", "molt-app"] {
        let tree = inverse_no_dev_deps(root, "ring");
        assert!(
            tree.trim().is_empty(),
            "`ring` re-entered the default build graph of {root}:\n{tree}"
        );
    }
}
