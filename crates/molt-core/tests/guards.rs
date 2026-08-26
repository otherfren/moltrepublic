// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Workspace-wide source guards (L12/L15): invariants a green suite must
//! keep from rotting — the `no_baked_indentation` shape.

use std::path::{Path, PathBuf};

fn workspace_crates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .to_path_buf()
}

/// Non-test source of a file: everything before its first `#[cfg(test)]`.
fn production_source(path: &Path) -> String {
    let text = std::fs::read_to_string(path).expect("source readable");
    match text.find("#[cfg(test)]") {
        Some(cut) => text[..cut].to_string(),
        None => text,
    }
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) != Some("target") {
                rust_sources(&path, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// L12: `mockrand` is a demo/timing PRNG, NOT cryptography. A new call
/// site means someone reached for it — if that is a key, nonce, ticket
/// or secret, use `getrandom`.
#[test]
fn mockrand_callers_stay_on_the_allowlist() {
    let mut sources = Vec::new();
    rust_sources(&workspace_crates(), &mut sources);
    let mut callers: Vec<String> = sources
        .iter()
        .filter(|p| {
            std::fs::read_to_string(p)
                .map(|t| {
                    // comment-only mentions (e.g. "never mockrand") are fine
                    t.lines()
                        .filter(|l| !l.trim_start().starts_with("//"))
                        .any(|l| l.contains("mockrand"))
                })
                .unwrap_or(false)
        })
        .map(|p| {
            let s = p.to_string_lossy().replace('\\', "/");
            s.split("/crates/").nth(1).unwrap_or(&s).to_string()
        })
        .collect();
    callers.sort();
    assert_eq!(
        callers,
        [
            "molt-core/src/lib.rs",
            "molt-core/tests/guards.rs",
            "molt-engine/src/net/demo_mesh.rs",
            "molt-net/src/loopback.rs",
            "molt-net/src/supervisor.rs",
        ],
        "a new mockrand caller appeared — it is a demo/timing PRNG, NOT \
         crypto; keys, nonces, tickets and secrets use getrandom"
    );
}

/// L15: a canonical signed-bytes writer must PANIC on an unframeable
/// field — `unwrap_or(0)` wrote "empty field" and then appended the
/// bytes anyway, a non-injective preimage two different states could
/// share; `unwrap_or_default()` let m members sign "this proposal had no
/// payload".
#[test]
fn canonical_writers_never_frame_a_failure_as_empty() {
    let crates = workspace_crates();
    let files = [
        crates.join("molt-core/src/chain.rs"),
        crates.join("molt-core/src/lib.rs"),
        crates.join("molt-storage/src/lib.rs"),
    ];
    let mut offenders = Vec::new();
    for file in files {
        let text = production_source(&file);
        for (no, line) in text.lines().enumerate() {
            let silent = line.contains("unwrap_or(0)") || line.contains("unwrap_or_default()");
            let framing = line.contains("to_le_bytes") || line.contains("to_vec");
            if silent && framing {
                offenders.push(format!(
                    "{}:{} {}",
                    file.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    no + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a canonical writer frames a failure as an empty field — panic \
         instead (ambiguous signed bytes are never acceptable):\n{}",
        offenders.join("\n")
    );
}
