// SPDX-License-Identifier: GPL-3.0-or-later

//! Wire ingest: the id-addressed verbs on the wire, the read-receipt park
//! cap, the sharer-only `FileServed` rule and the P6 parking buffer.

use super::test_support::*;
use crate::chat::{ParkedRefs, PendingRef, PARKED_TARGET_CAP};
use molt_core::{ChatMessage, EventEnvelope, MessageId, WorkspaceEvent};

/// A wire reaction whose known target is already a tombstone is skipped
/// ENTIRELY — no event recorded (the log gets no dead entry), nothing
/// parked, no reaction on the tombstone. The commuting twin of the
/// applier-side guard: react/delete converge independent of order.
#[test]
fn a_wire_reaction_on_a_tombstone_records_no_event() {
    let mut st = crate::tests::plain_state();
    let id = MessageId([0x2au8; 16]);
    st.apply(&EventEnvelope { prev_seq: 0,
        seq: 1,
        ts: 101,
        by: "peer-1".to_string(),
        body: WorkspaceEvent::Chat(ChatMessage::text(id, "peer-1", "soon gone", 101)),
    });
    st.apply(&EventEnvelope { prev_seq: 0,
        seq: 2,
        ts: 102,
        by: "peer-1".to_string(),
        body: WorkspaceEvent::ChatDeleted {
            index: 0,
            id: Some(id),
            by: "peer-1".to_string(),
        },
    });
    let seq_before = st.next_seq;
    st.wire_react(
        id,
        "peer-2".to_string(),
        "🔥".to_string(),
        Some(molt_core::ReactOp::Add),
    );
    assert_eq!(st.next_seq, seq_before, "no event was recorded");
    assert!(st.chat[0].reactions.is_empty(), "no reaction on the tombstone");
    assert!(!st.parked.holds(&id), "a KNOWN tombstoned target parks nothing");
}

/// A wire chat message into a DECIDED vote's discussion still lands in
/// the log: closed discussions are enforced on the local send paths
/// only (`cmd_chat` / `cmd_share_file`) — the receive path stays
/// permissive so every member's log converges even when a peer's
/// message was in flight while the vote decided (convergence over
/// enforcement, same posture as the channel-claim coercion above).
#[test]
fn a_wire_chat_into_a_closed_discussion_still_lands() {
    // a runtime context: the delivery path may publish to a transport
    // feed / bump watch channels (spawned tasks)
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut st = crate::tests::plain_state();
    // a proposal that is proposed, then declined — replayed as events,
    // exactly like the log rebuild does it
    st.apply(&EventEnvelope { prev_seq: 0,
        seq: 1,
        ts: 100,
        by: "me".to_string(),
        body: WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(1),
            surface: molt_core::Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "title": "t" }),
        },
    });
    st.apply(&EventEnvelope { prev_seq: 0,
        seq: 2,
        ts: 101,
        by: "peer-2".to_string(),
        body: WorkspaceEvent::Declined {
            id: molt_core::ProposalId(1),
            by: "peer-2".to_string(),
            hash: String::new(),
        },
    });
    // the local send path refuses…
    assert!(matches!(
        st.cmd_chat(
            "too late".to_string(),
            None,
            molt_core::ChannelRef::Patch {
                id: molt_core::ProposalId(1)
            },
        ),
        Err(molt_core::MoltError::DiscussionClosed(
            molt_core::ProposalId(1),
            molt_core::ProposalState::Rejected,
        ))
    ));
    // …but the same message arriving over the wire lands in the log
    let msg = ChatMessage::text(id(7), "peer-1", "was in flight", 102).with_channel(
        molt_core::ChannelRef::Patch {
            id: molt_core::ProposalId(1),
        },
    );
    st.cmd_net_delivered(
        "peer-1".to_string(),
        EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 102,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::Chat(msg),
        },
        None,
    )
    .expect("a wire delivery never errors");
    assert_eq!(st.chat.len(), 1, "the wire message landed");
    assert_eq!(st.chat[0].body, "was in flight");
}

/// **A hostile stamp on a wire chat never panics a read.** `ts` is the
/// peer's claim; `uploads_view` added the retention to it, and with
/// release overflow checks on, `u64::MAX` took the whole actor down —
/// persisted, so every reopen died on the Uploads tab (review
/// 2026-08-25). The wire clamps the stamp to a plausible window and
/// the read saturates.
#[test]
fn a_wire_chat_with_a_hostile_stamp_never_panics_the_uploads_view() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut st = crate::tests::plain_state();
    let mut msg = ChatMessage::text(id(8), "peer-1", "share", u64::MAX);
    msg.file = Some(molt_core::FileMeta {
        name: "a.bin".to_string(),
        size: 1,
        kind: "bin".to_string(),
        modified: u64::MAX,
        available: true,
        checksum: String::new(),
    });
    st.cmd_net_delivered(
        "peer-1".to_string(),
        EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: u64::MAX,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::Chat(msg),
        },
        None,
    )
    .expect("a wire delivery never errors");
    assert_eq!(st.chat.len(), 1);
    assert!(
        st.chat[0].ts <= crate::now_secs().saturating_add(900),
        "the stamp is clamped to the FileServed plausibility window"
    );
    let uploads = st.uploads_view();
    assert_eq!(uploads.len(), 1, "the share is listed, the read did not panic");
}

/// A wire chat is a FRESH message: the log original carries no
/// reactions, receipts or tombstone — those travel as their own
/// link-authenticated events. Carrying them inside the body attributed
/// forged stances to OTHER members (review 2026-08-25).
#[test]
fn a_wire_chat_carries_no_foreign_reactions_receipts_or_tombstone() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut st = crate::tests::plain_state();
    let mut msg = ChatMessage::text(id(9), "peer-1", "forged", 0);
    msg.reactions
        .insert("👍".to_string(), vec!["peer-2".to_string()]);
    msg.read_by.insert("peer-2".to_string());
    msg.deleted_by = Some("peer-2".to_string());
    st.cmd_net_delivered(
        "peer-1".to_string(),
        EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 0,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::Chat(msg),
        },
        None,
    )
    .expect("a wire delivery never errors");
    let m = &st.chat[0];
    assert!(m.reactions.is_empty(), "no forged reactions");
    assert!(m.read_by.is_empty(), "no forged receipts");
    assert_eq!(m.deleted_by, None, "no forged tombstone");
    assert_ne!(m.ts, 0, "an unknown age is the arrival time, not 'forever'");
}

/// E6: one `ChatRead` of random ids parks a bounded number of
/// targets — never the whole P6 buffer.
#[test]
fn a_read_receipt_frame_parks_a_bounded_number_of_targets() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut st = crate::tests::plain_state();
    let ids: Vec<MessageId> = (100..160).map(id).collect();
    st.cmd_net_delivered(
        "peer-1".to_string(),
        EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 100,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::ChatRead { ids: ids.clone(), by: "peer-1".to_string() },
        },
        None,
    )
    .expect("a wire delivery never errors");
    let parked = ids.iter().filter(|i| st.parked.holds(i)).count();
    assert_eq!(parked, super::PARKED_READS_PER_FRAME, "the per-frame cap holds");
}

/// RELAY file plane trust gates (review 2026-08-10): a `FileServed`
/// counts only from the SHARER's own mouth, only with a plausible
/// stamp, and an old redelivery never regresses a newer one — one
/// crafted frame from any member must not poison the group's stamp
/// cache (a future stamp names an h-window that holds nothing, so a
/// poisoned cache bricks the share's downloads for good).
#[test]
fn a_file_served_counts_only_from_the_sharer() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = rt.enter();
    let mut st = crate::tests::plain_state();
    st.nostr = Some(crate::NostrTransport {
        sk: zeroize::Zeroizing::new(vec![7u8; 32]),
        relays: vec!["ws://relay.example".to_string()],
        rotation_seed: [0u8; 32],
    });
    // peer-1's share message lands over the wire
    let sid = id(9);
    let mut msg = ChatMessage::text(sid, "peer-1", "der bericht", 100);
    msg.file = Some(molt_core::FileMeta {
        name: "bericht.bin".to_string(),
        size: 4,
        kind: "File".to_string(),
        modified: 100,
        available: true,
        checksum: "aa".repeat(32),
    });
    st.cmd_net_delivered(
        "peer-1".to_string(),
        EventEnvelope {
            prev_seq: 0,
            seq: 1,
            ts: 100,
            by: "peer-1".to_string(),
            body: WorkspaceEvent::Chat(msg),
        },
        None,
    )
    .expect("share lands");
    let served = |from: &str, seq: u64, at: u64| EventEnvelope {
        prev_seq: 0,
        seq,
        ts: 100 + seq,
        by: from.to_string(),
        body: WorkspaceEvent::FileServed { id: sid, at },
    };
    // another member announcing the sharer's series: dropped
    st.cmd_net_delivered("peer-2".to_string(), served("peer-2", 1, 1_000), None)
        .expect("ack");
    assert!(st.files.series.is_empty(), "a non-sharer's announcement must not count");
    // the sharer with an absurd future stamp: dropped
    let future = crate::now_secs() + 1_000_000;
    st.cmd_net_delivered("peer-1".to_string(), served("peer-1", 2, future), None)
        .expect("ack");
    assert!(st.files.series.is_empty(), "a far-future stamp must not count");
    // the sharer's plausible stamp lands…
    st.cmd_net_delivered("peer-1".to_string(), served("peer-1", 3, 5_000), None)
        .expect("ack");
    assert_eq!(st.files.series.get(&sid), Some(&5_000));
    // …and an older redelivery does not regress it
    st.cmd_net_delivered("peer-1".to_string(), served("peer-1", 4, 4_000), None)
        .expect("ack");
    assert_eq!(st.files.series.get(&sid), Some(&5_000), "at-least-once must not rewind");
}

fn react(by: &str) -> PendingRef {
    PendingRef::React {
        by: by.to_string(),
        emoji: "🎉".to_string(),
        op: Some(molt_core::ReactOp::Add),
    }
}

/// Cap overflow evicts the OLDEST parked target (FIFO), and a drained
/// target frees its slot so the next new target evicts nothing.
#[test]
fn park_eviction_is_fifo_and_a_drain_frees_the_slot() {
    let mut p = ParkedRefs::new();
    for n in 0..PARKED_TARGET_CAP {
        p.park(id(n), react("ada"));
    }
    assert_eq!(p.targets(), PARKED_TARGET_CAP);
    assert!(p.holds(&id(0)));

    // one over the cap: the oldest target (0) goes, the newest stays
    p.park(id(PARKED_TARGET_CAP), react("ben"));
    assert_eq!(p.targets(), PARKED_TARGET_CAP);
    assert!(!p.holds(&id(0)), "the OLDEST target is evicted first");
    assert!(p.holds(&id(1)));
    assert!(p.holds(&id(PARKED_TARGET_CAP)));

    // draining a target frees its slot: the next new target fits
    // without evicting anything
    assert_eq!(p.drain(&id(5)), vec![react("ada")]);
    assert!(!p.holds(&id(5)), "drained refs are gone");
    assert!(p.drain(&id(5)).is_empty(), "a second drain finds nothing");
    p.park(id(PARKED_TARGET_CAP + 1), react("chi"));
    assert_eq!(p.targets(), PARKED_TARGET_CAP);
    assert!(p.holds(&id(1)), "no eviction after a drain freed a slot");

    // several refs under one target keep their arrival order
    let mut q = ParkedRefs::new();
    q.park(id(7), react("ada"));
    q.park(id(7), PendingRef::Delete { by: "ben".to_string() });
    assert_eq!(
        q.drain(&id(7)),
        vec![react("ada"), PendingRef::Delete { by: "ben".to_string() }]
    );
}
