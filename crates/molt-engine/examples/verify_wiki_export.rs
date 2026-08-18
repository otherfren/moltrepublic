// SPDX-License-Identifier: GPL-3.0-or-later

//! **Verify a wiki export** (`docs_archive/memory/wiki_export_plan.md`):
//!
//! ```text
//! cargo run -p molt-engine --example verify_wiki_export -- <export directory>
//! ```
//!
//! Checks that `<dir>/wiki` is exactly the fold of the patches in
//! `<dir>/proof/bundle.json`, and that each of those patches carries the
//! threshold signatures of the republic's sealed roster. Needs no republic
//! membership and no key. Exits non-zero on any failure.
//!
//! This binary is I/O only: reading the directory and printing the verdict.
//! The check itself is [`molt_engine::verify_wiki_export`], which runs the
//! republic's own byte layouts — the reference implementation
//! `<dir>/proof/README.md` specifies.

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: verify_wiki_export <export directory>");
        return ExitCode::FAILURE;
    };
    let dir = std::path::PathBuf::from(dir);
    let report = match molt_engine::read_wiki_export(&dir)
        .and_then(|(bundle, tree)| molt_engine::verify_wiki_export(&bundle, &tree))
    {
        Ok(report) => report,
        Err(e) => {
            eprintln!("FAILED  {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "genesis  ok  republic={} name={:?} m={} n={}",
        report.republic_id, report.name, report.rule_m, report.rule_n
    );
    println!(
        "roster   ok  members={} membership_blocks={}",
        report.members.join(","),
        report.membership_blocks
    );
    println!("patches  ok  verified={}", report.patches);
    println!("tree     ok  files={}", report.files);
    println!("note     authenticity and m-of-n approval, not completeness");
    ExitCode::SUCCESS
}
