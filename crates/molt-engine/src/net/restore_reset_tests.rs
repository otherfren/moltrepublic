// SPDX-License-Identifier: GPL-3.0-or-later

//! A restored seat's inbound state is forgotten once per restore, on
//! evidence or proof, never blindly (`delivery_guarantee.md` §11).

use crate::chain::test_support::{chain_peer, wire, Builder};
use molt_core::{AcceptedWindow, ChainBlock, MessageId, RestoreProof, WorkspaceEvent};

const SEATS: [&str; 3] = ["petra", "walter", "dora"];
const T: u64 = 1_751_000_000;

fn anchor() -> String {
    "ab".repeat(32)
}

fn anchor2() -> String {
    "cd".repeat(32)
}

fn chat(id: u8, body: &str) -> WorkspaceEvent {
    WorkspaceEvent::Chat(molt_core::ChatMessage::text(MessageId([id; 16]), "dora", body, T))
}

fn small(seq: u64) -> u8 {
    u8::try_from(seq).expect("small")
}

/// One envelope of dora's, stamped `ts`.
fn dora_at(st: &mut crate::State, seq: u64, ts: u64, body: WorkspaceEvent) {
    let env = molt_core::EventEnvelope { prev_seq: 0, seq, ts, by: "dora".to_string(), body };
    st.cmd_net_delivered("dora".to_string(), env, None).expect("a wire delivery acks");
}

/// A block from petra (her wire seq `seq`), its envelope stamped `ts`.
fn block_at(st: &mut crate::State, seq: u64, ts: u64, block: ChainBlock) {
    let env =
        molt_core::EventEnvelope { prev_seq: 0, seq, ts, by: "petra".to_string(), body: WorkspaceEvent::Committed(block) };
    st.cmd_net_delivered("petra".to_string(), env, None).expect("a wire delivery acks");
}

/// Walter, a bystander, holding dora's OLD incarnation window (seqs 1..=40
/// stamped T+1..T+40); dora's restore (sealed at T+100) not yet applied.
fn bystander() -> (crate::State, ChainBlock) {
    let (walter, first, _) = bystander_twice();
    (walter, first)
}

/// [`bystander`] plus a second restore of dora, sealed after the first.
fn bystander_twice() -> (crate::State, ChainBlock, ChainBlock) {
    let mut b = Builder::new(&SEATS, 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    for seq in 1..=40u64 {
        wire(&mut walter, "dora", seq, chat(100 + small(seq), "old"));
    }
    b.commit_restored("dora", &anchor(), &["petra", "walter"]);
    let first = b.blocks.last().expect("restore one").clone();
    b.commit_restored("dora", &anchor2(), &["petra", "walter"]);
    let second = b.blocks.last().expect("restore two").clone();
    (walter, first, second)
}

fn bodies(st: &crate::State, body: &str) -> usize {
    st.chat.iter().filter(|m| m.body == body).count()
}

fn window(st: &crate::State) -> AcceptedWindow {
    st.delivery.accepted.get("dora").cloned().unwrap_or_default()
}

/// The field shape: a bystander that missed the live re-key reset. The
/// block only keys its window; the first dropped frame stamped after the
/// restore and after everything the window accepted proves the fault: the
/// window resets on it, and that frame lands.
#[test]
fn a_bystander_that_missed_the_merge_resets_on_the_first_newer_duplicate() {
    let (mut walter, block) = bystander();
    block_at(&mut walter, 1, T + 100, block);
    assert_eq!(walter.chain.anchors.get("dora"), Some(&anchor()), "the block applied");
    assert!(window(&walter).is_accepted(40), "the block alone resets nothing");
    dora_at(&mut walter, 5, T + 205, chat(1, "comeback"));
    assert_eq!(bodies(&walter, "comeback"), 1, "the evidence frame lands");
    let w = window(&walter);
    assert_eq!((w.restore.as_str(), w.proof), (anchor().as_str(), RestoreProof::Evidence));
    assert!(w.is_accepted(5) && !w.is_accepted(40));
}

/// Merge first, block second: the block must not throw away what the new
/// device delivered after the merge, and a resend of it is no evidence.
#[test]
fn a_block_after_the_rekey_merge_does_not_reset_again() {
    let (mut walter, block) = bystander();
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    assert_eq!(window(&walter).proof, RestoreProof::MergePending);
    for seq in 1..=3u64 {
        dora_at(&mut walter, seq, T + 200 + seq, chat(small(seq), "new"));
    }
    block_at(&mut walter, 1, T + 100, block);
    let w = window(&walter);
    assert!(w.is_accepted(3), "the block reset the window a second time: {w:?}");
    assert_eq!((w.restore.as_str(), w.proof), (anchor().as_str(), RestoreProof::Merge));
    dora_at(&mut walter, 2, T + 202, chat(9, "new again"));
    assert_eq!(bodies(&walter, "new again"), 0);
    assert_eq!(bodies(&walter, "new"), 3);
    assert!(window(&walter).is_accepted(3), "a resend is no evidence");
}

/// Block first, merge second, then the same restore found again (a
/// re-application, the open): nothing after the merge's reset.
#[test]
fn a_restore_seen_again_after_new_traffic_resets_nothing() {
    let (mut walter, block) = bystander();
    block_at(&mut walter, 1, T + 100, block);
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    assert_eq!(window(&walter).proof, RestoreProof::Merge);
    for seq in 1..=3u64 {
        dora_at(&mut walter, seq, T + 200 + seq, chat(small(seq), "new"));
    }
    walter.reset_for_restore("dora", &anchor());
    walter.reconcile_restore_windows();
    walter.reset_for_restore("dora", &anchor());
    assert!(window(&walter).is_accepted(3), "a replay reset the window");
}

/// Two restores of one seat, both block first: the second block resets the
/// first device's stream - a merge marker never outlives its restore - and
/// the reset is written at once, not on the save debounce.
#[test]
fn a_second_restore_resets_the_first_devices_stream() {
    let (mut walter, first, second) = bystander_twice();
    block_at(&mut walter, 1, T + 100, first);
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    for seq in 1..=3u64 {
        dora_at(&mut walter, seq, T + 200 + seq, chat(small(seq), "device one"));
    }
    let saves = walter.delivery.reset_saves;
    block_at(&mut walter, 2, T + 300, second);
    let w = window(&walter);
    assert_eq!((w.restore.as_str(), w.proof), (anchor2().as_str(), RestoreProof::Block));
    assert!(!w.is_accepted(3), "device one's marks survived the second restore");
    assert_eq!(walter.delivery.reset_saves, saves + 1, "the reset went to the writer at once");
}

/// The same, the second merge first: the block adopts the merged window.
#[test]
fn a_second_restore_merged_first_is_adopted() {
    let (mut walter, first, second) = bystander_twice();
    block_at(&mut walter, 1, T + 100, first);
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    assert_eq!(window(&walter).proof, RestoreProof::MergePending);
    dora_at(&mut walter, 1, T + 400, chat(1, "device two"));
    block_at(&mut walter, 2, T + 300, second);
    let w = window(&walter);
    assert_eq!((w.restore.as_str(), w.proof), (anchor2().as_str(), RestoreProof::Merge));
    assert!(w.is_accepted(1));
}

/// A healthy window a v0.1.1 build wrote (no key, no stamp) is keyed at open,
/// not reset; frames re-read after the restart are no evidence.
#[test]
fn a_healthy_unkeyed_window_is_keyed_at_open_not_reset() {
    let (mut walter, block) = bystander();
    block_at(&mut walter, 1, T + 100, block);
    let mut healthy = AcceptedWindow::default();
    for seq in 1..=40u64 {
        assert!(healthy.accept(seq));
    }
    walter.delivery.accepted.insert("dora".to_string(), healthy);
    walter.delivery.inbound.clear();
    walter.delivery.dups.clear();
    walter.presence.clock_override = Some(T + 1_000);
    walter.delivery.opened_at = T + 1_000;
    walter.reconcile_restore_windows();
    assert_eq!(window(&walter).restore, anchor());
    assert!(window(&walter).is_accepted(40), "a healthy window was reset at open");
    dora_at(&mut walter, 30, T + 990, chat(130, "re-read"));
    dora_at(&mut walter, 31, T + 1_000 + crate::net::ingest::WIRE_STAMP_SKEW_SECS, chat(131, "re-read"));
    assert!(window(&walter).is_accepted(40), "a re-read frame counted as evidence");
}

/// The same window shape holding the OLD incarnation: with no stamp known
/// the open time plus the skew bound is the reference, and the first frame
/// past it resets the window and lands.
#[test]
fn an_unkeyed_stale_window_resets_on_a_frame_past_the_open_bound() {
    let (mut walter, block) = bystander();
    block_at(&mut walter, 1, T + 100, block);
    let mut stale = window(&walter);
    stale.stamp = 0;
    stale.restore.clear();
    stale.proof = RestoreProof::Unknown;
    walter.delivery.accepted.insert("dora".to_string(), stale);
    walter.delivery.inbound.clear();
    walter.presence.clock_override = Some(T + 1_000);
    walter.delivery.opened_at = T + 1_000;
    walter.reconcile_restore_windows();
    let past = T + 1_001 + crate::net::ingest::WIRE_STAMP_SKEW_SECS;
    dora_at(&mut walter, 5, past, chat(1, "proof"));
    assert_eq!(bodies(&walter, "proof"), 1);
    assert_eq!(window(&walter).proof, RestoreProof::Evidence);
}

/// A restored device catching up hears another restored seat before its
/// catch-up applies that seat's older `Restored` block: the block adopts
/// the first-contact window, it does not replay the seat.
#[test]
fn a_first_contact_before_the_catch_up_block_is_not_reset() {
    let mut b = Builder::new(&SEATS, 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    b.commit_restored("dora", &anchor(), &["petra", "walter"]);
    let block = b.blocks.last().expect("the restore").clone();
    for seq in 1..=3u64 {
        dora_at(&mut walter, seq, T + 200 + seq, chat(small(seq), "new"));
    }
    assert_eq!(window(&walter).proof, RestoreProof::Pending);
    block_at(&mut walter, 1, T + 100, block);
    let w = window(&walter);
    assert!(w.is_accepted(3), "the catch-up block replayed the seat: {w:?}");
    assert_eq!((w.restore.as_str(), w.proof), (anchor().as_str(), RestoreProof::Adopted));
}

/// A seat never restored keeps its window at open.
#[test]
fn the_open_leaves_unrestored_seats_alone() {
    let (mut walter, block) = bystander();
    block_at(&mut walter, 1, T + 100, block);
    wire(&mut walter, "petra", 2, chat(7, "petra"));
    walter.reconcile_restore_windows();
    assert!(walter.delivery.accepted["petra"].is_accepted(2), "petra was never restored");
    assert_eq!(walter.delivery.accepted["petra"].restore, "");
}

/// After a reset the new incarnation is owed from its first seq: a later
/// envelope is ordered behind its missing predecessors instead of becoming
/// a baseline the sheet would ack everything below.
#[test]
fn a_reset_window_orders_the_new_incarnation_from_its_start() {
    let (mut walter, _block) = bystander();
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    let env = |seq: u64, prev: u64, id: u8, body: &str| molt_core::EventEnvelope {
        prev_seq: prev,
        seq,
        ts: T + 100 + seq,
        by: "dora".to_string(),
        body: chat(id, body),
    };
    walter.cmd_net_delivered("dora".to_string(), env(50, 40, 2, "later"), None).expect("ack");
    assert_eq!(bodies(&walter, "later"), 0, "held behind its predecessor");
    assert!(!window(&walter).is_accepted(50), "not acked before what it follows");
    walter.cmd_net_delivered("dora".to_string(), env(40, 0, 1, "first"), None).expect("ack");
    assert_eq!((bodies(&walter, "first"), bodies(&walter, "later")), (1, 1));
    let order: Vec<&str> =
        walter.chat.iter().filter(|m| m.body == "first" || m.body == "later").map(|m| m.body.as_str()).collect();
    assert_eq!(order, ["first", "later"]);
}

/// A sender that answers no gap want (v0.1.0) cannot fill a reset window
/// from its start: past the fallback the frontier goes to just below what
/// is held - loudly - instead of holding its stream for ever.
#[test]
fn a_reset_window_of_a_silent_sender_falls_back_to_a_baseline() {
    let (mut walter, _block) = bystander();
    let fallback = crate::net::delivery::GAP_FALLBACK_SECS;
    walter.presence.clock_override = Some(T + 1_000);
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    let env = molt_core::EventEnvelope { prev_seq: 2_990, seq: 3_000, ts: T + 1_000, by: "dora".to_string(), body: chat(1, "far") };
    walter.cmd_net_delivered("dora".to_string(), env, None).expect("ack");
    assert_eq!(bodies(&walter, "far"), 0, "held above the cap");
    walter.inbound_gap_tick(T + 1_000 + fallback - 1);
    assert_eq!(bodies(&walter, "far"), 0);
    walter.presence.clock_override = Some(T + 1_000 + fallback);
    walter.inbound_gap_tick(T + 1_000 + fallback);
    assert_eq!(bodies(&walter, "far"), 1, "the stream is released past the fallback");
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

/// The operator resync: a stale window that the new device has already
/// overtaken sees no newer duplicate, so only the operator can reset it. It
/// is refused for the own seat and a non-seat, and a second call while the
/// history is still owed changes nothing.
#[test]
fn an_operator_resync_resets_one_seat_once() {
    let (mut walter, block) = bystander();
    block_at(&mut walter, 1, T + 100, block);
    dora_at(&mut walter, 41, T + 300, chat(1, "past the old high"));
    assert_eq!(bodies(&walter, "past the old high"), 1);
    assert!(walter.cmd_resync_seat("walter".to_string()).is_err(), "the own seat");
    assert!(walter.cmd_resync_seat("zoe".to_string()).is_err(), "a non-seat");
    walter.cmd_resync_seat("dora".to_string()).expect("resync");
    let w = window(&walter);
    assert_eq!((w.proof, w.high), (RestoreProof::Operator, 0));
    let again = walter.cmd_resync_seat("dora".to_string()).expect_err("running");
    assert!(again.to_string().ends_with("resync of dora running"), "{again}");
    // the history arrives again, ordered from its start; what was applied
    // stays applied once
    dora_at(&mut walter, 1, T + 201, chat(2, "since the restore"));
    dora_at(&mut walter, 41, T + 300, chat(1, "past the old high"));
    assert_eq!((bodies(&walter, "since the restore"), bodies(&walter, "past the old high")), (1, 1));
}

/// An unchanged sheet is not republished although its stamp moved.
#[test]
fn an_unchanged_sheet_is_the_same_sheet() {
    let mut w = AcceptedWindow::default();
    assert!(w.accept(3));
    let claims = std::collections::BTreeMap::from([("dora".to_string(), w)]);
    let mut last = molt_net::group_ack::GroupAck::new("walter".to_string(), claims.clone());
    last.at = 1;
    let mut next = molt_net::group_ack::GroupAck::new("walter".to_string(), claims);
    next.at = 2;
    assert!(crate::net::delivery::same_sheet(Some(&last), &next));
    let mut moved = next.clone();
    if let Some(w) = moved.claims.get_mut("dora") {
        assert!(w.accept(4));
    }
    assert!(!crate::net::delivery::same_sheet(Some(&last), &moved));
}
