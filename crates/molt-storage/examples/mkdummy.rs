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

use molt_core::{ChatMessage, EventEnvelope, FileMeta, WorkspaceEvent};

fn chat_env(seq: u64, ts: u64, from: &str, body: &str, quote: Option<u64>) -> EventEnvelope {
    let mut msg = ChatMessage::text(from, body, ts);
    msg.quote = quote;
    EventEnvelope {
        seq,
        ts,
        by: from.to_string(),
        body: WorkspaceEvent::Chat(msg),
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
            identities: Vec::new(),
            attestations: Vec::new(),
            republic_id: String::new(),
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
    // two file shares: one still on "peer-1's disk", one already removed —
    // the chat shows both card states out of the box
    let share = |seq: u64, ts: u64, from: &str, name: &str, size: u64, kind: &str| {
        let mut msg = ChatMessage::text(from, "", ts);
        msg.file = Some(FileMeta {
            name: name.to_string(),
            size,
            kind: kind.to_string(),
            modified: ts - 86_400,
            available: true,
        });
        EventEnvelope {
            seq,
            ts,
            by: from.to_string(),
            body: WorkspaceEvent::Chat(msg),
        }
    };
    ws.append(&share(7, t + 300, "peer-1", "dummy-charter.pdf", 148_480, "PDF"))?;
    ws.append(&share(8, t + 360, "peer-2", "old-scan.jpg", 2_411_724, "Image"))?;
    ws.append(&EventEnvelope {
        seq: 9,
        ts: t + 420,
        by: "peer-2".to_string(),
        body: WorkspaceEvent::FileRemoved {
            // chat position of the seq-8 share: the sixth chat message
            // (indices count messages, not envelopes — the reaction at
            // seq 5 occupies no chat slot)
            index: 5,
            by: "peer-2".to_string(),
        },
    })?;
    ws.sync()?;

    println!("workspace dir : {}", ws.dir().display());
    println!("workspace id  : {}", ws.manifest.workspace.id);
    println!("recovery seed : {phrase}");
    Ok(())
}
