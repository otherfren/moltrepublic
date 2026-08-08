// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **N5.0 — the two 445 plaintexts must stay tellable apart.**
//!
//! Two producers put different things inside a 445's MLS plaintext, and
//! neither tags what it is: the ritual sends `RitualMsg` JSON
//! (`nostr_ritual.rs::publish_frame_now`), the supervisor sends
//! `EventEnvelope` JSON (or a `MESH_ACK_TAG`-prefixed control frame). Today
//! they never meet, because the ritual owns the channel before any runtime
//! exists — N5.2 is the change that makes them meet.
//!
//! They are disjoint today, but **by accident, not by design**: `RitualMsg` is
//! internally tagged (`#[serde(tag = "kind")]`) so it demands a `kind` field,
//! and `EventEnvelope` demands `seq`/`ts`/`by`/`body`. Neither shape satisfies
//! the other's required fields, so a try-parse discriminator happens to work.
//!
//! Nothing enforced that. One `#[serde(default)]` on `EventEnvelope`, or a
//! `kind` field added to it, and a ritual message starts decoding as an event
//! — silently, on the wire, at the one moment the two share a channel. This
//! test is the enforcement, and it is cheaper than the byte tag it replaces:
//! a tag would break the ritual's wire format for a property we already have.

use molt_core::{EventEnvelope, WorkspaceEvent};
use molt_net::invite::{JoinRequest, RitualMsg};

fn ritual() -> RitualMsg {
    RitualMsg::Join(JoinRequest {
        seat: 1,
        name: "petra".to_string(),
        identity_pk: "aa".repeat(32),
        nostr_pk: "bb".repeat(32),
        mac: "cc".repeat(32),
        reply: None,
        key_package: "dd".repeat(64),
        relays: Vec::new(),
    })
}

fn envelope() -> EventEnvelope {
    EventEnvelope {
        seq: 7,
        ts: 1_700_000_000,
        by: "walter".to_string(),
        body: WorkspaceEvent::ChainRequest { from_height: 3 },
        prev_seq: 6,
    }
}

/// Neither wire form parses as the other — the property a try-parse
/// discriminator on the 445 plaintext silently depends on.
#[test]
fn a_ritual_message_and_an_event_envelope_never_decode_as_each_other() {
    let r = serde_json::to_vec(&ritual()).expect("ritual serializes");
    let e = serde_json::to_vec(&envelope()).expect("envelope serializes");

    assert!(
        serde_json::from_slice::<EventEnvelope>(&r).is_err(),
        "a RitualMsg decoded as an EventEnvelope — the 445 discriminator is \
         broken and the ritual's frames would be delivered as events"
    );
    assert!(
        serde_json::from_slice::<RitualMsg>(&e).is_err(),
        "an EventEnvelope decoded as a RitualMsg — the 445 discriminator is \
         broken and the runtime's events would be delivered as ritual frames"
    );

    // …and each still decodes as ITSELF, so the test cannot pass by both
    // shapes having become undecodable garbage
    assert_eq!(
        serde_json::from_slice::<RitualMsg>(&r).expect("ritual round-trips"),
        ritual()
    );
    assert_eq!(
        serde_json::from_slice::<EventEnvelope>(&e).expect("envelope round-trips"),
        envelope()
    );
}

/// The ack control frame is tagged, and its tag cannot be mistaken for JSON.
///
/// `MESH_ACK_TAG` is the precedent this whole question comes from: someone
/// already decided a control frame must announce itself rather than be
/// recognised by a hopeful parse. It starts with a NUL, which no JSON document
/// may, so the two are disjoint by construction rather than by accident.
#[test]
fn the_ack_tag_cannot_be_confused_with_a_json_frame() {
    let tag = molt_net::MESH_ACK_TAG;
    assert_eq!(tag.first(), Some(&0u8), "the ack tag leads with NUL");
    let e = serde_json::to_vec(&envelope()).expect("serializes");
    assert_ne!(e.first(), Some(&0u8), "a JSON frame never does");
    assert!(
        !e.starts_with(tag),
        "an event envelope must never look like an ack control frame"
    );
}
