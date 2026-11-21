// SPDX-License-Identifier: GPL-3.0-or-later

//! **Dump a workspace for offline chain forensics**
//! (`docs/chain/rejoin_divergence_analysis_plan.md`, Phase O):
//!
//! ```text
//! cargo run -p molt-engine --example chain_forensics -- <workspace dir> <out dir>
//! ```
//!
//! Run it on a COPY laid out as `<x>/device.key` + `<x>/workspaces/<dir>`:
//! opening a workspace writes to it. No engine, no network. Writes
//! `chain.jsonl` (one block per line, with its hash), `checkpoint.json`,
//! `log.jsonl` (every event envelope) and `snapshot.json`.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(ws), Some(out)) = (args.next(), args.next()) else {
        eprintln!("usage: chain_forensics <workspace dir> <out dir>");
        return ExitCode::FAILURE;
    };
    match run(Path::new(&ws), Path::new(&out)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("FAILED  {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(ws: &Path, out: &Path) -> Result<(), String> {
    // a missing device key would be CREATED by the open, hiding the real fault
    let device_key = ws
        .parent()
        .and_then(Path::parent)
        .map(|x| x.join("device.key"))
        .ok_or("workspace dir has no grandparent")?;
    if !device_key.is_file() {
        return Err(format!("no device key at {}", device_key.display()));
    }
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let (opened, loaded) = molt_storage::open_workspace(ws).map_err(|e| e.to_string())?;

    let (checkpoint, blocks) = opened.read_chain().map_err(|e| e.to_string())?;
    let rid = match (&checkpoint, blocks.first().map(|b| &b.change)) {
        (Some(c), _) => c.republic_id.clone(),
        (None, Some(molt_core::ChainChange::Genesis { republic_id, .. })) => republic_id.clone(),
        _ => return Err("no republic id: neither a checkpoint nor a genesis block".into()),
    };
    let mut f = create(out, "chain.jsonl")?;
    for b in &blocks {
        let hash = molt_storage::content_hash(&molt_core::chain::block_link_bytes(&rid, b));
        let line = serde_json::json!({ "height": b.height, "hash": hash, "block": b });
        writeln!(f, "{line}").map_err(|e| e.to_string())?;
    }
    write_json(out, "checkpoint.json", &checkpoint)?;
    write_json(out, "snapshot.json", &loaded.snapshot)?;

    let log = opened.read_log_from(0).map_err(|e| e.to_string())?;
    let mut f = create(out, "log.jsonl")?;
    for env in &log {
        let line = serde_json::to_string(env).map_err(|e| e.to_string())?;
        writeln!(f, "{line}").map_err(|e| e.to_string())?;
    }
    println!(
        "blocks={} anchor={} head={} checkpoint={} log_envelopes={} compaction_floor={} unknown_events={}",
        blocks.len(),
        blocks.first().map_or(0, |b| b.height),
        blocks.last().map_or(0, |b| b.height),
        checkpoint.is_some(),
        log.len(),
        loaded.compaction_floor,
        loaded.unknown_events,
    );
    Ok(())
}

fn create(out: &Path, name: &str) -> Result<std::fs::File, String> {
    std::fs::File::create(out.join(name)).map_err(|e| e.to_string())
}

fn write_json<T: serde::Serialize>(out: &Path, name: &str, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    std::fs::write(out.join(name), bytes).map_err(|e| e.to_string())
}
