// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The browser build's dependency graph as a test (`docs/web_gui.md` §4):
//! nothing of the engine, no runtime, no C library reaches the wasm. The
//! guard against drift is this graph, not the wasm `cargo check` - a crate
//! can compile for wasm32 and still be the wrong thing to ship to a tab.

use std::collections::BTreeSet;
use std::process::Command;

/// Every package in molt-web's wasm32 graph without dev-dependencies, by
/// name: `cargo tree --locked -p molt-web --target wasm32-unknown-unknown
/// -e no-dev`.
fn wasm_graph() -> BTreeSet<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "tree", "--locked", "-p", "molt-web", "--target", "wasm32-unknown-unknown", "-e", "no-dev",
            "--prefix", "none", "--format", "{p}",
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .expect("cargo tree runs");
    assert!(out.status.success(), "cargo tree failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// KEYSTONE - the tab gets the window and the wire, never the node.
#[test]
fn the_browser_graph_holds_no_engine_no_runtime_and_no_c_library() {
    let graph = wasm_graph();
    // the listing must be able to see what IS there, or the absences below
    // would be a blind pass
    for present in ["molt-web", "molt-ui", "molt-ui-kit", "molt-ui-window", "molt-core", "slint", "wasm-bindgen"] {
        assert!(graph.contains(present), "cargo tree no longer lists {present}: the guard is blind");
    }
    let forbidden = [
        // the node
        "molt-engine", "molt-net", "molt-storage", "molt-mcp", "molt-remote", "molt-config", "molt-vault",
        "molt-treasury", "molt-app",
        // a runtime and an OS clipboard the browser does not have
        "tokio", "arboard",
        // C libraries (ADR-0002 keeps secp256k1 at the transport edge)
        "secp256k1", "secp256k1-sys", "libsqlite3-sys", "ring",
    ];
    let found: Vec<&str> = forbidden.into_iter().filter(|pkg| graph.contains(*pkg)).collect();
    assert!(found.is_empty(), "in the browser build's graph: {found:?}");
}

/// Natively the crate depends on nothing: a workspace build sees an empty
/// crate, and no native graph changes because molt-web exists.
#[test]
fn the_native_graph_of_the_crate_is_the_crate_alone() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["tree", "--locked", "-p", "molt-web", "-e", "no-dev", "--prefix", "none", "--format", "{p}"])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .expect("cargo tree runs");
    assert!(out.status.success(), "cargo tree failed: {}", String::from_utf8_lossy(&out.stderr));
    let listed = String::from_utf8_lossy(&out.stdout);
    let names: Vec<&str> = listed.lines().filter_map(|line| line.split_whitespace().next()).collect();
    assert_eq!(names, ["molt-web"], "a native dependency of molt-web");
}

/// The wasm-bindgen CLI must equal the crate to the patch version, so the
/// pin is exact and the lockfile agrees with it.
#[test]
fn wasm_bindgen_is_pinned_to_one_exact_version() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let manifest = std::fs::read_to_string(format!("{root}/Cargo.toml")).expect("the workspace manifest");
    let pin = manifest
        .lines()
        .find_map(|line| line.strip_prefix("wasm-bindgen "))
        .and_then(|rest| rest.split('"').nth(1))
        .expect("a wasm-bindgen entry in [workspace.dependencies]");
    let version = pin.strip_prefix('=').expect("an exact pin (=x.y.z)");
    let lock = std::fs::read_to_string(format!("{root}/Cargo.lock")).expect("the lockfile");
    let locked: Vec<&str> = lock
        .split("[[package]]")
        .filter(|entry| entry.contains("\nname = \"wasm-bindgen\"\n"))
        .filter_map(|entry| entry.lines().find_map(|line| line.strip_prefix("version = ")))
        .collect();
    assert_eq!(locked, [format!("\"{version}\"")], "one wasm-bindgen in the lockfile, the pinned one");
}
