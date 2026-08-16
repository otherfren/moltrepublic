// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **L8 guard** — every whole-file read in molt-storage goes through the
//! capped helper. A direct `fs::read` slurps the entire file before any
//! validation, so bitrot, a sparse file or a hostile staging dir becomes
//! an OOM instead of a typed refusal. This scan keeps the discipline from
//! rotting (the molt-core `no_baked_indentation` shape).

use std::path::Path;

/// Non-test source of a file: everything before its first `#[cfg(test)]`.
fn production_source(path: &Path) -> String {
    let text = std::fs::read_to_string(path).expect("source readable");
    match text.find("#[cfg(test)]") {
        Some(cut) => text[..cut].to_string(),
        None => text,
    }
}

#[test]
fn every_whole_file_read_is_capped() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&src).expect("src dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = production_source(&path);
        for (no, line) in text.lines().enumerate() {
            let bare = line.trim_start();
            if bare.starts_with("//") {
                continue;
            }
            // the two capped helpers are the only sanctioned callers
            if bare.contains("READ_CAPPED_HELPER") {
                continue;
            }
            if bare.contains("fs::read(") || bare.contains("fs::read_to_string(") {
                offenders.push(format!(
                    "{}:{} {}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    no + 1,
                    bare
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "every whole-file read must go through read_capped/read_string_capped \
         (metadata-checked BEFORE the allocation) — direct reads found:\n{}",
        offenders.join("\n")
    );
}

// ---- L7: corrupt is never "no chain" --------------------------------------

use molt_storage::{create_workspace, generate_seed_phrase, open_workspace, seed_entropy};

fn founded() -> molt_core::EventEnvelope {
    molt_core::EventEnvelope {
        seq: 1,
        ts: 42,
        by: "petra".to_string(),
        prev_seq: 0,
        body: molt_core::WorkspaceEvent::Founded {
            name: "R".to_string(),
            rule_m: 1,
            rule_n: 1,
            member: "petra".to_string(),
            roster: vec!["petra".to_string()],
            identities: Vec::new(),
            attestations: Vec::new(),
            republic_id: "rid".to_string(),
            agenda: String::new(),
            relays: Vec::new(),
            features: None,
        },
    }
}

/// L7: a PRESENT-but-unreadable chain.state is a typed refusal — read as
/// "no chain" it ran a chain republic chainless (legacy counted path) and
/// the next governance write overwrote the damaged bytes. Absent stays
/// the quiet pre-chain answer.
#[test]
fn a_damaged_chain_state_is_not_reported_as_no_chain() {
    let tmp = tempfile::tempdir().expect("tmp");
    let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
    let ws = create_workspace(tmp.path(), &seed, &founded()).expect("create");
    let dir = ws.dir().to_path_buf();
    ws.write_chain(None, &[]).expect("a chain file lands");
    drop(ws);

    // damage shape 1: tag flip — framing intact, authentication fails
    let path = dir.join("chain.state");
    let mut bytes = std::fs::read(&path).expect("read");
    let n = bytes.len();
    bytes[n - 1] ^= 0xff;
    std::fs::write(&path, &bytes).expect("damage");
    let (ws, _state) = open_workspace(&dir).expect("the manifest gate is not the chain gate");
    assert!(
        ws.read_chain().is_err(),
        "an unauthenticated chain.state must be a typed verdict, not silence"
    );
    drop(ws);

    // damage shape 2: torn framing
    std::fs::write(&path, &bytes[..10]).expect("truncate");
    let (ws, _state) = open_workspace(&dir).expect("open");
    assert!(ws.read_chain().is_err(), "torn framing is a typed verdict");
    drop(ws);

    // absent: the quiet pre-chain answer
    std::fs::remove_file(&path).expect("rm");
    let (ws, _state) = open_workspace(&dir).expect("open");
    let (blob, chain) = ws.read_chain().expect("absent is not an error");
    assert!(blob.is_none() && chain.is_empty());
}

/// L8 pins: the cap refuses on METADATA — a sparse multi-GiB file never
/// reaches the allocator.
#[test]
fn an_oversized_file_is_refused_before_it_is_read() {
    let tmp = tempfile::tempdir().expect("tmp");
    let seed = seed_entropy(&generate_seed_phrase().expect("gen")).expect("entropy");
    let ws = create_workspace(tmp.path(), &seed, &founded()).expect("create");
    let dir = ws.dir().to_path_buf();
    drop(ws);

    // a sparse 8 GiB manifest.toml: zero disk cost, lethal to fs::read
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("manifest.toml"))
        .expect("open manifest");
    f.set_len(8 * 1024 * 1024 * 1024).expect("sparse");
    drop(f);
    let started = std::time::Instant::now();
    let err = match open_workspace(&dir) {
        Err(e) => e,
        Ok(_) => panic!("an 8 GiB manifest must refuse"),
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the refusal comes from metadata, never from an 8 GiB read"
    );
    assert!(err.to_string().contains("cap"), "{err}");
}
