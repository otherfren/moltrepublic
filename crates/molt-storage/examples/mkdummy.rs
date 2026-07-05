// SPDX-License-Identifier: GPL-3.0-or-later

//! Materialize a small demo workspace on this machine:
//!
//! ```sh
//! cargo run -p molt-storage --example mkdummy [ROOT]
//! ```
//!
//! ROOT defaults to `~/.moltrepublic/workspaces`. The workspace ("Dummy
//! Republic", 2-of-3, member `me`) is created through the exact same
//! `molt-storage` path the founding run uses — real seed, sealed key,
//! `Founded` genesis, a bit of chat history — so the node's Open screen
//! lists it and opening replays it. Prints the recovery phrase and the id.

use std::collections::BTreeMap;

use molt_core::{ChatMessage, EventEnvelope, WorkspaceEvent};

fn chat_env(seq: u64, ts: u64, from: &str, body: &str, quote: Option<u64>) -> EventEnvelope {
    EventEnvelope {
        seq,
        ts,
        by: from.to_string(),
        body: WorkspaceEvent::Chat(ChatMessage {
            from: from.to_string(),
            body: body.to_string(),
            ts,
            quote,
            reactions: BTreeMap::new(),
            deleted_by: None,
        }),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args().nth(1).map(std::path::PathBuf::from).unwrap_or_else(|| {
        molt_storage::expand_tilde("~/.moltrepublic/workspaces")
    });

    let phrase = molt_storage::generate_seed_phrase()?;
    let seed = molt_storage::seed_entropy(&phrase)?;

    let now = molt_storage::now_secs();
    let member = "me";
    let roster = vec!["me".to_string(), "peer-1".to_string(), "peer-2".to_string()];

    let genesis = EventEnvelope {
        seq: 1,
        ts: now - 3600,
        by: member.to_string(),
        body: WorkspaceEvent::Founded {
            name: "Dummy Republic".to_string(),
            rule_m: 2,
            rule_n: 3,
            member: member.to_string(),
            roster,
        },
    };
    let mut ws = molt_storage::create_workspace(&root, &seed, &genesis)?;

    // a little pre-existing history, so opening shows a lived-in chat
    let t = now - 3500;
    ws.append(&chat_env(2, t, "me", "welcome to the dummy republic 🎉", None))?;
    ws.append(&chat_env(3, t + 60, "peer-1", "good to be here", Some(0)))?;
    ws.append(&chat_env(4, t + 120, "peer-2", "everything in this workspace survives a restart", None))?;
    ws.append(&EventEnvelope {
        seq: 5,
        ts: t + 180,
        by: "peer-1".to_string(),
        body: WorkspaceEvent::ChatReacted {
            index: 2,
            emoji: "👍".to_string(),
            by: "peer-1".to_string(),
        },
    })?;
    ws.append(&chat_env(6, t + 240, "me", "try sending a message, then restart moltd", None))?;
    ws.sync()?;

    println!("workspace dir : {}", ws.dir().display());
    println!("workspace id  : {}", ws.manifest.workspace.id);
    println!("recovery seed : {phrase}");
    Ok(())
}
