// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! A string literal must not carry its own source indentation.
//!
//! Rust's `\` line continuation strips the newline AND the next line's leading
//! whitespace. Lose the backslash and the indentation becomes part of the
//! message — which has now shipped TWICE in this repo, once in a security
//! refusal an operator reads when a join is turned away as a possible
//! impersonation, and once in a founding-failure toast.
//!
//! Both times the fix was a grep, and both times the claim "the sweep comes
//! back empty" was made in a commit message and then quietly stopped being
//! true. A claim that has to be re-verified by hand is not a guarantee, so
//! this is the sweep, enforced.

use std::path::Path;

/// Runs of this many spaces inside a literal are almost certainly source
/// indentation. Real prose never needs them; deliberately aligned blocks
/// (TOML/ASCII templates) are exempted by name below.
const SUSPICIOUS_RUN: usize = 6;

/// Files whose literals legitimately contain aligned columns.
const EXEMPT: &[&str] = &[
    // the config template renders an aligned TOML comment block
    "molt-config/src/lib.rs",
];

fn crates_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ is the parent of molt-core")
}

/// Every `"…"` literal on a line, crudely but adequately: we only care about
/// long runs of spaces, and a false split cannot manufacture one.
fn literal_spans(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut escaped = false;
    for ch in line.chars() {
        match &mut cur {
            None => {
                if ch == '"' {
                    cur = Some(String::new());
                }
            }
            Some(buf) => {
                if escaped {
                    escaped = false;
                    buf.push(ch);
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    out.push(std::mem::take(buf));
                    cur = None;
                } else {
                    buf.push(ch);
                }
            }
        }
    }
    out
}

#[test]
fn no_string_literal_carries_its_own_source_indentation() {
    let needle = " ".repeat(SUSPICIOUS_RUN);
    let mut offenders: Vec<String> = Vec::new();

    let mut stack = vec![crates_dir().to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // only shipping code: tests legitimately embed fixtures
                if path.file_name().is_some_and(|n| n == "target" || n == "tests") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let rel = path.strip_prefix(crates_dir()).unwrap_or(&path).display().to_string();
            if EXEMPT.iter().any(|x| rel.replace('\\', "/").ends_with(x)) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (n, line) in text.lines().enumerate() {
                for lit in literal_spans(line) {
                    // an indentation run always sits BETWEEN words; a literal
                    // that is only padding (column alignment) is not prose
                    if lit.contains(&needle) && lit.trim().len() > SUSPICIOUS_RUN {
                        offenders.push(format!("{rel}:{}: {}", n + 1, lit.trim()));
                    }
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "string literal(s) carry their own source indentation — a lost `\\` \
         line continuation bakes the indent into the message:\n  {}",
        offenders.join("\n  ")
    );
}
