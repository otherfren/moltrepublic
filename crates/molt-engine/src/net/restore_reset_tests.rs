// SPDX-License-Identifier: GPL-3.0-or-later

//! A restored seat's inbound state is forgotten once per restore, on every
//! seat (`delivery_guarantee.md` §4.9).

use crate::chain::test_support::{chain_peer, wire, Builder};
use molt_core::{AcceptedWindow, ChainBlock, MessageId, WorkspaceEvent, RESTORE_REKEYED};

const SEATS: [&str; 3] = ["petra", "walter", "dora"];

fn anchor() -> String {
    "ab".repeat(32)
}

fn chat(id: u8, body: &str) -> WorkspaceEvent {
    WorkspaceEvent::Chat(molt_core::ChatMessage::text(MessageId([id; 16]), "dora", body, 1_751_000_000))
}

/// Walter, a bystander, holding dora's OLD incarnation window (seqs
/// 1..=40), and the `Restored` block for dora not yet applied.
fn bystander() -> (crate::State, ChainBlock) {
    let mut b = Builder::new(&SEATS, 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    for seq in 1..=40u64 {
        wire(&mut walter, "dora", seq, chat(100 + u8::try_from(seq).expect("small"), "old"));
    }
    b.commit_restored("dora", &anchor(), &["petra", "walter"]);
    let block = b.blocks.last().expect("the restore").clone();
    (walter, block)
}

fn bodies(st: &crate::State, body: &str) -> usize {
    st.chat.iter().filter(|m| m.body == body).count()
}

fn window(st: &crate::State) -> AcceptedWindow {
    st.delivery.accepted.get("dora").cloned().unwrap_or_default()
}

/// The field shape: a bystander whose live re-key reset was lost still
/// forgets the old incarnation when the `Restored` block is applied.
#[test]
fn applying_the_restored_block_resets_the_seats_window_on_a_bystander() {
    let (mut walter, block) = bystander();
    wire(&mut walter, "petra", 1, WorkspaceEvent::Committed(block));
    assert_eq!(walter.chain.anchors.get("dora"), Some(&anchor()), "the block applied");
    wire(&mut walter, "dora", 5, chat(1, "comeback"));
    assert_eq!(bodies(&walter, "comeback"), 1, "the new incarnation's seq 5 is not a duplicate");
    assert_eq!(window(&walter).restore, anchor(), "the reset is keyed to the restore");
}

/// Merge first, block second: the block must not throw away what the new
/// device delivered after the merge.
#[test]
fn a_block_after_the_rekey_merge_does_not_reset_again() {
    let (mut walter, block) = bystander();
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    assert_eq!(window(&walter).restore, RESTORE_REKEYED);
    for seq in 1..=3u64 {
        wire(&mut walter, "dora", seq, chat(u8::try_from(seq).expect("small"), "new"));
    }
    wire(&mut walter, "petra", 1, WorkspaceEvent::Committed(block));
    let w = window(&walter);
    assert!(w.is_accepted(3), "the block reset the window a second time: {w:?}");
    assert_eq!(w.restore, anchor(), "the block adopts the merged window");
    // a resend of seq 2 is a duplicate, not a second apply
    wire(&mut walter, "dora", 2, chat(9, "new again"));
    assert_eq!(bodies(&walter, "new again"), 0);
    assert_eq!(bodies(&walter, "new"), 3);
}

/// Block first, merge second, then the same restore found again (a
/// re-application, the reconcile at open): one reset for the block, the
/// merge's own, none after.
#[test]
fn a_restore_seen_again_after_new_traffic_resets_nothing() {
    let (mut walter, block) = bystander();
    wire(&mut walter, "petra", 1, WorkspaceEvent::Committed(block));
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    for seq in 1..=3u64 {
        wire(&mut walter, "dora", seq, chat(u8::try_from(seq).expect("small"), "new"));
    }
    walter.reset_for_restore("dora", &anchor());
    assert!(window(&walter).is_accepted(3), "a re-applied block reset the window");
    assert_eq!(window(&walter).restore, anchor());
    walter.reconcile_restore_windows();
    walter.reset_for_restore("dora", &anchor());
    assert!(window(&walter).is_accepted(3), "a replay at open reset the window");
}

/// The window a v0.1.1 build persisted for a restored seat carries no key:
/// the open resets it once, and only once.
#[test]
fn an_unkeyed_window_of_a_restored_seat_is_reset_once_at_open() {
    let (mut walter, block) = bystander();
    wire(&mut walter, "petra", 1, WorkspaceEvent::Committed(block));
    let old: AcceptedWindow = serde_json::from_str(r#"{"high":40,"frontier":40}"#).expect("a v0.1.1 window");
    assert!(old.restore.is_empty());
    walter.delivery.accepted.insert("dora".to_string(), old);
    walter.delivery.inbound.clear();
    walter.reconcile_restore_windows();
    assert_eq!(window(&walter).restore, anchor());
    assert!(walter.delivery.accepted_dirty, "the reset reaches the next save");
    wire(&mut walter, "dora", 5, chat(1, "comeback"));
    assert_eq!(bodies(&walter, "comeback"), 1);
    walter.reconcile_restore_windows();
    assert!(window(&walter).is_accepted(5), "a second open reset again");
}

/// A seat never restored keeps its window at open, and a window created
/// for a restored seat carries the key from the start.
#[test]
fn the_open_leaves_unrestored_seats_and_fresh_windows_alone() {
    let (mut walter, block) = bystander();
    wire(&mut walter, "petra", 1, WorkspaceEvent::Committed(block));
    wire(&mut walter, "petra", 2, chat(7, "petra"));
    walter.delivery.accepted.remove("dora");
    wire(&mut walter, "dora", 1, chat(1, "first"));
    assert_eq!(window(&walter).restore, anchor(), "a fresh window counts under the current restore");
    walter.reconcile_restore_windows();
    assert!(window(&walter).is_accepted(1));
    assert!(walter.delivery.accepted["petra"].is_accepted(2), "petra was never restored");
}

fn deliver_at(st: &mut crate::State, seq: u64, ts: u64, body: WorkspaceEvent) {
    let env = molt_core::EventEnvelope { prev_seq: 0, seq, ts, by: "dora".to_string(), body };
    st.cmd_net_delivered("dora".to_string(), env, None).expect("a wire delivery acks");
}

#[derive(Clone, Default)]
struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn logged_during(f: impl FnOnce()) -> String {
    let buf = LogBuf::default();
    let subscriber = tracing_subscriber::fmt().with_writer(buf.clone()).with_ansi(false).without_time().finish();
    tracing::subscriber::with_default(subscriber, f);
    let text = String::from_utf8_lossy(&buf.0.lock().expect("log buffer")).to_string();
    text
}

fn streak(st: &crate::State) -> u32 {
    st.seat_syncs(&std::collections::HashSet::new())
        .into_iter()
        .find(|s| s.member == "dora")
        .map_or(0, |s| s.dup_streak)
}

/// A window that predates the seat's restore drops frames authored after
/// everything it accepted, at rising seqs: one WARN per episode, and the
/// seat's status row says so.
#[test]
fn dropping_newer_frames_as_seen_warns_once_and_shows_in_status() {
    let (mut walter, _block) = bystander();
    let logs = logged_during(|| {
        for seq in 1..=40u64 {
            deliver_at(&mut walter, seq, 1_751_000_500 + seq, chat(200 + u8::try_from(seq).expect("small"), "lost"));
        }
    });
    assert_eq!(bodies(&walter, "lost"), 0, "the stale window drops them");
    assert_eq!(logs.matches("dup_streak").count(), 1, "one WARN per episode: {logs}");
    assert!(logs.contains("first_seq=1 last_seq=32 count=32"), "{logs}");
    assert_eq!(streak(&walter), 40);
    // the reset ends the episode
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    assert_eq!(streak(&walter), 0);
}

/// A resend round re-offers what was accepted: older stamps, so no episode.
#[test]
fn a_resend_round_of_accepted_frames_is_no_episode() {
    let (mut walter, _block) = bystander();
    let logs = logged_during(|| {
        for _round in 0..3 {
            for seq in 1..=40u64 {
                deliver_at(&mut walter, seq, 1_751_000_000 + seq, chat(100 + u8::try_from(seq).expect("small"), "old"));
            }
        }
    });
    assert!(!logs.contains("dup_streak"), "{logs}");
    assert_eq!(streak(&walter), 0);
    assert_eq!(bodies(&walter, "old"), 40);
}

/// Frames re-read after a restart (a ratchet persisted late) are accepted
/// ones: the reference stamp is persisted with the window, so they are no
/// episode either.
#[test]
fn frames_re_read_after_a_restart_are_no_episode() {
    let (mut walter, _block) = bystander();
    let persisted = walter.delivery.accepted.clone();
    walter.delivery = crate::DeliveryState::default();
    walter.delivery.accepted = persisted;
    let logs = logged_during(|| {
        for seq in 1..=40u64 {
            deliver_at(&mut walter, seq, 1_751_000_000 + seq, chat(100 + u8::try_from(seq).expect("small"), "old"));
        }
    });
    assert!(!logs.contains("dup_streak"), "{logs}");
    assert_eq!(streak(&walter), 0);
}
