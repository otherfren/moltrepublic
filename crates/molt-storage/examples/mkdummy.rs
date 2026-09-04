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

use molt_core::{ChatMessage, EventEnvelope, FileMeta, MessageId, WorkspaceEvent};

/// A fixed, non-nil message id for the canned history (a live engine mints
/// random ids; a dummy log just needs stable, distinct ones).
fn dummy_id(n: u8) -> MessageId {
    let mut b = [0xd0u8; 16];
    b[15] = n;
    MessageId(b)
}

fn chat_env(
    seq: u64,
    ts: u64,
    id: MessageId,
    from: &str,
    body: &str,
    quote_id: Option<MessageId>,
) -> EventEnvelope {
    let mut msg = ChatMessage::text(id, from, body, ts);
    msg.quote_id = quote_id;
    EventEnvelope { prev_seq: 0,
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

    let genesis = EventEnvelope { prev_seq: 0,
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
            agenda: String::new(),
            relays: Vec::new(),
            features: None,
        },
    };
    let mut ws = molt_storage::create_workspace(&root, &seed, &genesis)?;

    // a little pre-existing history, so opening shows a lived-in chat —
    // every message carries its stable id; reactions/removals/quotes
    // address by those ids (the numeric `index` on the events is only the
    // legacy slot older readers fall back to)
    let id_welcome = dummy_id(1);
    let id_here = dummy_id(2);
    let id_survives = dummy_id(3);
    let id_try = dummy_id(4);
    let id_charter = dummy_id(5);
    let id_scan = dummy_id(6);
    let t = now - 3500;
    ws.append(&chat_env(2, t, id_welcome, "me", "welcome to the dummy republic 🎉", None))?;
    ws.append(&chat_env(3, t + 60, id_here, "peer-1", "good to be here", Some(id_welcome)))?;
    ws.append(&chat_env(4, t + 120, id_survives, "peer-2", "everything in this workspace survives a restart", None))?;
    ws.append(&EventEnvelope { prev_seq: 0,
        seq: 5,
        ts: t + 180,
        by: "peer-1".to_string(),
        body: WorkspaceEvent::ChatReacted {
            // the legacy slot of the id_survives message (positions count
            // messages, not envelopes — this reaction occupies no slot)
            index: 2,
            id: Some(id_survives),
            emoji: "👍".to_string(),
            by: "peer-1".to_string(),
            op: Some(molt_core::ReactOp::Add),
        },
    })?;
    ws.append(&chat_env(6, t + 240, id_try, "me", "try sending a message, then restart moltd", None))?;
    // two file shares: one still on "peer-1's disk", one already removed —
    // the chat shows both card states out of the box
    let share = |seq: u64, ts: u64, id: MessageId, from: &str, name: &str, size: u64, kind: &str| {
        let mut msg = ChatMessage::text(id, from, "", ts);
        msg.file = Some(FileMeta {
            name: name.to_string(),
            size,
            kind: kind.to_string(),
            modified: ts - 86_400,
            available: true,
            // a dummy share has no real bytes — honestly unknown
            checksum: String::new(),
            key_b64: String::new(),
            pieces: 0,
            root: String::new(),
        });
        EventEnvelope { prev_seq: 0,
            seq,
            ts,
            by: from.to_string(),
            body: WorkspaceEvent::Chat(msg),
        }
    };
    ws.append(&share(7, t + 300, id_charter, "peer-1", "dummy-charter.pdf", 148_480, "PDF"))?;
    ws.append(&share(8, t + 360, id_scan, "peer-2", "old-scan.jpg", 2_411_724, "Image"))?;
    ws.append(&EventEnvelope { prev_seq: 0,
        seq: 9,
        ts: t + 420,
        by: "peer-2".to_string(),
        body: WorkspaceEvent::FileRemoved {
            // the id addresses the seq-8 share; index 5 is its legacy slot
            // (the sixth chat message) for pre-chat-bus readers
            index: 5,
            id: Some(id_scan),
            by: "peer-2".to_string(),
        },
    })?;
    ws.sync()?;

    println!("workspace dir : {}", ws.dir().display());
    println!("workspace id  : {}", ws.manifest.workspace.id);
    println!("recovery seed : {phrase}");
    Ok(())
}
