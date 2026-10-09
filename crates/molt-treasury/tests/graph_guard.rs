// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The purse stack's dependency posture as a test (plan §6): pure Rust, no
//! `ring`, and one `multiexp` (the `schnorr-signatures` pin).

use std::collections::BTreeSet;
use std::process::Command;

/// `(name, version)` of every package in the default (no-dev) graph, any target.
fn no_dev_graph() -> BTreeSet<(String, String)> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "tree",
            "--locked",
            "-p",
            "molt-treasury",
            "-e",
            "no-dev",
            "--target",
            "all",
            "--prefix",
            "none",
            // chrono's per-OS time zone lookup (haiku `cc`, macOS/wasm `-sys`)
            "--prune",
            "iana-time-zone",
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .expect("cargo tree runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((
                it.next()?.to_owned(),
                it.next()?.trim_start_matches('v').to_owned(),
            ))
        })
        .collect()
}

#[test]
fn the_purse_stack_is_pure_rust_with_one_multiexp() {
    let graph = no_dev_graph();
    assert!(
        graph.iter().any(|(n, _)| n == "dkg-pedpop"),
        "the guard does not see the graph"
    );
    let banned: Vec<_> = graph
        .iter()
        .filter(|(n, _)| n == "ring" || n == "cc" || n.ends_with("-sys"))
        .collect();
    assert!(banned.is_empty(), "C entered the purse stack: {banned:?}");

    let multiexp: Vec<_> = graph.iter().filter(|(n, _)| n == "multiexp").collect();
    assert_eq!(
        multiexp.len(),
        1,
        "two multiexp versions break dkg-pedpop: {multiexp:?}"
    );
    assert!(graph.contains(&("schnorr-signatures".into(), "0.5.2".into())));
}
