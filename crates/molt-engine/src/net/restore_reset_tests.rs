// SPDX-License-Identifier: GPL-3.0-or-later

//! A restored seat's inbound state: the re-key merge resets it, the block
//! resets nothing, an older build's window is left alone, the operator
//! resyncs one seat - and re-delivered history never acts
//! (`delivery_guarantee.md` §11). No decision here reads a clock.

use crate::chain::test_support::{chain_peer, wire, Builder};
use molt_core::{AcceptedWindow, ChainBlock, MessageId, ProposalId, RestoreProof, WorkspaceEvent};

const SEATS: [&str; 3] = ["petra", "walter", "dora"];
const T: u64 = 1_751_000_000;
const DAY: u64 = 86_400;

fn anchor() -> String {
    "ab".repeat(32)
}

fn chat(id: u8, body: &str) -> WorkspaceEvent {
    WorkspaceEvent::Chat(molt_core::ChatMessage::text(MessageId([id; 16]), "dora", body, T))
}

fn small(seq: u64) -> u8 {
    u8::try_from(seq).expect("small")
}

fn dora_at(st: &mut crate::State, seq: u64, ts: u64, body: WorkspaceEvent) {
    let env = molt_core::EventEnvelope { prev_seq: 0, seq, ts, by: "dora".to_string(), body };
    st.cmd_net_delivered("dora".to_string(), env, None).expect("a wire delivery acks");
}

fn block_at(st: &mut crate::State, seq: u64, ts: u64, block: ChainBlock) {
    let env =
        molt_core::EventEnvelope { prev_seq: 0, seq, ts, by: "petra".to_string(), body: WorkspaceEvent::Committed(block) };
    st.cmd_net_delivered("petra".to_string(), env, None).expect("a wire delivery acks");
}

/// Walter holding dora's window over seqs 1..=40 (first contact in this
/// build), and dora's `Restored` block, not yet applied.
fn bystander() -> (crate::State, ChainBlock) {
    let mut b = Builder::new(&SEATS, 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    for seq in 1..=40u64 {
        wire(&mut walter, "dora", seq, chat(100 + small(seq), "old"));
    }
    b.commit_restored("dora", &anchor(), &["petra", "walter"]);
    let block = b.blocks.last().expect("the restore").clone();
    (walter, block)
}

/// [`bystander`] with dora's window as an older build wrote it.
fn legacy_bystander() -> (crate::State, ChainBlock) {
    let (mut walter, block) = bystander();
    let mut w = window(&walter);
    w.proof = RestoreProof::Unknown;
    walter.delivery.accepted.insert("dora".to_string(), w);
    walter.delivery.dups.clear();
    (walter, block)
}

fn bodies(st: &crate::State, body: &str) -> usize {
    st.chat.iter().filter(|m| m.body == body).count()
}

fn window(st: &crate::State) -> AcceptedWindow {
    st.delivery.accepted.get("dora").cloned().unwrap_or_default()
}

fn streak(st: &crate::State) -> u32 {
    st.seat_syncs(&std::collections::HashSet::new())
        .into_iter()
        .find(|s| s.member == "dora")
        .map_or(0, |s| s.dup_streak)
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

/// (1) A healthy window an older build wrote, met by a resend to the group
/// days later: never reset; the repeats only raise the hint.
#[test]
fn a_legacy_window_is_never_reset_by_repeats_however_late() {
    let (mut walter, block) = legacy_bystander();
    block_at(&mut walter, 1, T + 100, block);
    walter.presence.clock_override = Some(T + 30 * DAY);
    let logs = logged_during(|| {
        for seq in 1..=40u64 {
            dora_at(&mut walter, seq, T + 30 * DAY + seq, chat(100 + small(seq), "old"));
        }
    });
    let w = window(&walter);
    assert_eq!((w.proof, w.high), (RestoreProof::Unknown, 40), "a legacy window was reset");
    assert_eq!(bodies(&walter, "old"), 40, "nothing applied twice");
    assert_eq!(logs.matches("dup_streak").count(), 1, "one hint: {logs}");
    assert!(logs.contains("first_seq=1 last_seq=32 count=32"), "{logs}");
    assert_eq!(streak(&walter), 40);
}

/// The hint counts only a legacy window, and only while nothing of the seat
/// was accepted this session: a healthy legacy window whose seat speaks,
/// then re-offers a 40-frame tail, shows nothing; a fresh frame ends a
/// shown hint.
#[test]
fn the_repeat_hint_counts_only_a_legacy_window_nothing_was_accepted_into() {
    let (mut current, _) = bystander();
    for seq in 1..=40u64 {
        dora_at(&mut current, seq, T + seq, chat(100 + small(seq), "old"));
    }
    assert_eq!(streak(&current), 0, "a window of this build never counts");
    let (mut healthy, _) = legacy_bystander();
    dora_at(&mut healthy, 41, T + 41, chat(1, "fresh"));
    for seq in 1..=40u64 {
        dora_at(&mut healthy, seq, T + seq, chat(100 + small(seq), "old"));
    }
    assert_eq!(streak(&healthy), 0, "a resend round to a seat that speaks is no hint");
    assert_eq!(window(&healthy).proof, RestoreProof::Unknown, "accepting proves nothing about the restore");
    let (mut stale, _) = legacy_bystander();
    for seq in 1..=40u64 {
        dora_at(&mut stale, seq, T + seq, chat(100 + small(seq), "old"));
    }
    assert_eq!(streak(&stale), 40, "rising repeats with nothing accepted");
    dora_at(&mut stale, 41, T + 41, chat(1, "fresh"));
    assert_eq!(streak(&stale), 0, "a fresh frame ends the hint");
}

/// The merge is THE reset; the block - live, re-applied - resets nothing,
/// in either order.
#[test]
fn the_merge_resets_once_and_the_block_resets_nothing() {
    let (mut walter, block) = bystander();
    block_at(&mut walter, 1, T + 100, block.clone());
    assert!(window(&walter).is_accepted(40), "the block reset the window");
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    assert_eq!((window(&walter).proof, window(&walter).high), (RestoreProof::Merge, 0));
    for seq in 1..=3u64 {
        dora_at(&mut walter, seq, T + 200 + seq, chat(small(seq), "new"));
    }
    walter.adopt_chain(walter.chain.blocks.clone());
    assert!(window(&walter).is_accepted(3), "a re-applied chain reset the window");
    assert_eq!(bodies(&walter, "new"), 3);
}

/// A legacy window holding the old device, past the restore block: nothing
/// resets it (no clock decides), the hint shows, the operator heals it.
#[test]
fn a_stale_legacy_window_waits_for_the_operator() {
    let (mut walter, block) = legacy_bystander();
    block_at(&mut walter, 1, T + 100, block);
    for seq in 1..=40u64 {
        dora_at(&mut walter, seq, T + 300 + seq, chat(small(seq), "lost"));
    }
    assert_eq!(bodies(&walter, "lost"), 0, "dropped as already seen");
    assert_eq!(window(&walter).proof, RestoreProof::Unknown);
    assert!(streak(&walter) >= 32, "the hint shows");
    walter.cmd_resync_seat("dora".to_string(), false).expect("resync");
    for seq in 1..=40u64 {
        dora_at(&mut walter, seq, T + 300 + seq, chat(small(seq), "lost"));
    }
    assert_eq!(bodies(&walter, "lost"), 40, "re-delivered once each");
    assert_eq!(streak(&walter), 0);
}

/// (5) A recovered device's first-contact window, twenty minutes later and
/// after its catch-up applied the older restore: nothing is reset.
#[test]
fn a_first_contact_window_is_trusted_at_any_time() {
    let mut b = Builder::new(&SEATS, 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    b.commit_restored("dora", &anchor(), &["petra", "walter"]);
    let block = b.blocks.last().expect("the restore").clone();
    for seq in 1..=3u64 {
        dora_at(&mut walter, seq, T + seq, chat(small(seq), "new"));
    }
    walter.presence.clock_override = Some(T + 20 * 60);
    block_at(&mut walter, 1, T + 20 * 60, block);
    walter.adopt_chain(walter.chain.blocks.clone());
    let w = window(&walter);
    assert_eq!((w.proof, w.high), (RestoreProof::Current, 3));
}

/// After a reset the new incarnation is owed from its first seq: a later
/// envelope waits behind its missing predecessors instead of becoming a
/// baseline the sheet would ack everything below.
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
}

/// (6) A reset window whose sender is slow - behind a relay replay, or an
/// older build that answers nothing - keeps waiting, days if need be, says
/// so, and loses nothing; only the operator ends the wait.
#[test]
fn a_reset_window_waits_for_history_until_the_operator_gives_up() {
    let (mut walter, _block) = bystander();
    walter.presence.clock_override = Some(T);
    walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
    let far =
        molt_core::EventEnvelope { prev_seq: 2_990, seq: 3_000, ts: T, by: "dora".to_string(), body: chat(1, "far") };
    walter.cmd_net_delivered("dora".to_string(), far, None).expect("ack");
    for day in 1..=3 {
        walter.presence.clock_override = Some(T + day * DAY);
        walter.inbound_gap_tick(T + day * DAY);
    }
    assert_eq!(bodies(&walter, "far"), 0, "still waiting, nothing dropped");
    let row = walter.inbound_held_view().into_iter().find(|r| r.member == "dora").expect("a row");
    assert!(row.from_start, "the hold says it waits for the history");
    let again = walter.cmd_resync_seat("dora".to_string(), false).expect_err("running");
    assert!(again.to_string().contains("give_up"), "{again}");
    let logs = logged_during(|| {
        walter.cmd_resync_seat("dora".to_string(), true).expect("give up");
    });
    assert_eq!(bodies(&walter, "far"), 1, "released");
    assert!(logs.contains("inbound_gap_lost") && logs.contains("via=\"operator\""), "{logs}");
}

/// The operator resync: refused for the own seat and a non-seat, keeps the
/// request/claim gates (the same incarnation), and a second call while the
/// history is owed changes nothing.
#[test]
fn an_operator_resync_keeps_the_incarnation_gates() {
    let (mut walter, _) = bystander();
    walter.chain.request_seq.insert("dora".to_string(), 30);
    walter.chain.peer_claim_seq.insert("dora".to_string(), 30);
    assert!(walter.cmd_resync_seat("walter".to_string(), false).is_err(), "the own seat");
    assert!(walter.cmd_resync_seat("zoe".to_string(), false).is_err(), "a non-seat");
    assert!(walter.cmd_resync_seat("dora".to_string(), true).is_err(), "nothing to give up");
    walter.cmd_resync_seat("dora".to_string(), false).expect("resync");
    assert_eq!((window(&walter).proof, window(&walter).high), (RestoreProof::Operator, 0));
    assert_eq!(walter.chain.request_seq.get("dora"), Some(&30), "the request gate survived");
    assert_eq!(walter.chain.peer_claim_seq.get("dora"), Some(&30), "the claim gate survived");
    assert!(walter.cmd_resync_seat("dora".to_string(), false).is_err(), "running");
}

fn request(height: u64) -> WorkspaceEvent {
    WorkspaceEvent::ChainRequest { from_height: height, known: Vec::new() }
}

fn refusal(height: u64) -> WorkspaceEvent {
    WorkspaceEvent::VoteRefused { id: ProposalId(7), voter: "walter".to_string(), head: height, height }
}

/// (4) After a merge reset and after an operator resync, history delivered
/// again - an old `ChainRequest`, an old `VoteRefused` - takes no live
/// effect, while the newest frame still does.
#[test]
fn re_delivered_history_never_acts_after_a_reset() {
    for operator in [false, true] {
        let (mut walter, _) = bystander();
        if operator {
            walter.cmd_resync_seat("dora".to_string(), false).expect("resync");
        } else {
            walter.cmd_net_peer_rekeyed("dora".to_string(), None).expect("ack");
        }
        walter.chain.request_seq.remove("dora");
        walter.chain.peer_claim_seq.remove("dora");
        // the newest frame first (a newest-first replay), history after it
        dora_at(&mut walter, 60, T + 60, request(1));
        assert_eq!(walter.chain.request_seq.get("dora"), Some(&60), "the newest request acts");
        dora_at(&mut walter, 50, T + 50, request(1));
        assert_eq!(walter.chain.request_seq.get("dora"), Some(&60), "an old request acted");
        walter.session.notice.clear();
        dora_at(&mut walter, 55, T + 55, refusal(9));
        assert!(!walter.session.notice.starts_with("vote-refused"), "an old refusal raised a notice");
        dora_at(&mut walter, 61, T + 61, refusal(9));
        assert!(walter.session.notice.starts_with("vote-refused"), "the newest refusal still does");
    }
}

/// (M1, the field shape) A legacy window with an old high of 5000; the new
/// device speaks below it and is dropped; the operator resyncs. The seat's
/// `ChainRequest` then takes its live effects - served, its divergence mark
/// cleared, its head recorded - although its seq is far below the old high
/// and the test leaves every gate alone.
#[test]
fn after_a_resync_of_a_legacy_window_the_seats_requests_act() {
    let (mut walter, block) = bystander();
    let genesis = walter.chain.head.clone().expect("the genesis");
    block_at(&mut walter, 1, T + 100, block);
    let mut stale = AcceptedWindow::default();
    for seq in 4_990..=5_000u64 {
        assert!(stale.accept(seq));
    }
    walter.delivery.accepted.insert("dora".to_string(), stale);
    walter.delivery.inbound.clear();
    walter.delivery.dups.clear();
    walter.delivery.seen_high.clear();
    walter.chain.request_seq.insert("dora".to_string(), 5_000);
    walter.chain.peer_claim_seq.insert("dora".to_string(), 5_000);
    // the new device holds the genesis and asks for what follows
    let known = vec![molt_core::HeightHash { height: genesis.height, hash: genesis.hash.clone() }];
    let req = |known: Vec<molt_core::HeightHash>| WorkspaceEvent::ChainRequest { from_height: genesis.height + 1, known };
    for seq in 1..=10u64 {
        dora_at(&mut walter, seq, T + seq, chat(small(seq), "lost"));
    }
    dora_at(&mut walter, 11, T + 11, req(known.clone()));
    assert!(!walter.chain.served_at.contains_key("dora"), "dropped as already seen");
    walter.chain.diverged.insert(
        "dora".to_string(),
        molt_core::ChainDivergence { peer: "dora".to_string(), since_height: 0, seen_ts: T },
    );
    walter.cmd_resync_seat("dora".to_string(), false).expect("resync");
    for seq in 1..=10u64 {
        dora_at(&mut walter, seq, T + seq, chat(small(seq), "lost"));
    }
    dora_at(&mut walter, 11, T + 11, req(known));
    assert_eq!(bodies(&walter, "lost"), 10);
    assert!(walter.chain.served_at.contains_key("dora"), "the catch-up was served");
    assert!(!walter.chain.diverged.contains_key("dora"), "the mark was cleared");
    let mut seats = walter.seat_syncs(&std::collections::HashSet::new());
    walter.fill_seat_reports(&mut seats);
    let dora = seats.iter().find(|s| s.member == "dora").expect("dora's row");
    assert_eq!(dora.head_height, Some(genesis.height), "the seat head was recorded");
}

/// (M6) `give_up` ends the wait in every state it is accepted in - here with
/// nothing held at all: the row goes, the next frame is the baseline.
#[test]
fn give_up_with_nothing_held_ends_the_wait() {
    let (mut walter, _) = bystander();
    walter.cmd_resync_seat("dora".to_string(), false).expect("resync");
    assert!(walter.inbound_held_view().iter().any(|r| r.member == "dora" && r.from_start));
    walter.cmd_resync_seat("dora".to_string(), true).expect("give up");
    assert!(walter.inbound_held_view().iter().all(|r| r.member != "dora"), "the waiting row stays");
    let far =
        molt_core::EventEnvelope { prev_seq: 2_990, seq: 3_000, ts: T, by: "dora".to_string(), body: chat(1, "far") };
    walter.cmd_net_delivered("dora".to_string(), far, None).expect("ack");
    assert_eq!(bodies(&walter, "far"), 1, "the next frame is the baseline");
    assert!(walter.cmd_resync_seat("dora".to_string(), true).is_err(), "nothing waits any more");
}

fn mls_key(seed: u8) -> molt_storage::SigningKey {
    molt_storage::SigningKey::from_bytes(&[seed; 32])
}

/// walter's and petra's group states with dora in it, and dora's new
/// device's KeyPackage.
fn mls_group() -> (molt_net::MlsMember, molt_net::MlsMember, Vec<u8>) {
    let mut walter = molt_net::MlsMember::new(&mls_key(1), "walter").expect("walter");
    let mut petra = molt_net::MlsMember::new(&mls_key(2), "petra").expect("petra");
    let mut dora = molt_net::MlsMember::new(&mls_key(3), "dora").expect("dora");
    walter.create_group().expect("group");
    let welcome = walter
        .add_members(&[petra.key_package().expect("kp"), dora.key_package().expect("kp")])
        .expect("add")
        .expect("welcome");
    petra.join_from_welcome(&welcome).expect("petra joins");
    dora.join_from_welcome(&welcome).expect("dora joins");
    let dora2 = molt_net::MlsMember::new(&mls_key(4), "dora").expect("dora's new device");
    (walter, petra, dora2.key_package().expect("kp"))
}

/// The on-disk pair (dora's window, the ratchet) after `persist` ran with
/// window saves refused, then allowed: both old, then both new.
fn assert_disk_pair_moves_together(
    st: &mut crate::State,
    dir: &std::path::Path,
    mls: &std::sync::Arc<std::sync::Mutex<molt_net::MlsMember>>,
) {
    let disk = |st: &crate::State, dir: &std::path::Path| {
        assert!(st.active.as_ref().expect("stored").handle.flush_blocking(), "the writer drained");
        molt_storage::peek_transport_state(dir).unwrap_or_default()
    };
    let persist = |st: &mut crate::State| {
        if let Some(snap) = st.ratchet_snapshot(mls) {
            if let Some(active) = st.active.as_ref() {
                assert!(active.handle.merge_mls_async(snap));
            }
        }
    };
    st.drop_seam.refuse_saves.store(true, std::sync::atomic::Ordering::SeqCst);
    persist(st);
    let ts = disk(st, dir);
    assert!(ts.mls.is_none(), "the merged ratchet reached the disk before the reset");
    assert_eq!(ts.accepted.get("dora").map(|w| w.high), Some(40), "both old");
    st.drop_seam.refuse_saves.store(false, std::sync::atomic::Ordering::SeqCst);
    st.save_accepted_if_due(st.presence_now());
    persist(st);
    let ts = disk(st, dir);
    assert!(ts.mls.is_some(), "the ratchet persists once the reset is queued");
    let w = ts.accepted.get("dora").cloned().unwrap_or_default();
    assert_eq!((w.proof, w.high), (RestoreProof::Merge, 0), "both new");
}

/// A stored seat holding dora's window over 1..=40, saved.
fn stored_bystander(member: &str) -> (crate::State, tempfile::TempDir, std::path::PathBuf) {
    let b = Builder::new(&SEATS, 2);
    let (mut st, tmp, dir) = crate::tests::support::stored_chain_signer(&b, member, &SEATS);
    for seq in 1..=40u64 {
        wire(&mut st, "dora", seq, chat(100 + small(seq), "old"));
    }
    st.save_accepted_now();
    assert!(st.active.as_ref().expect("stored").handle.flush_blocking(), "the old window is on disk");
    (st, tmp, dir)
}

/// (M2) THE COMMITTER: its own re-key merges inside its group state; the
/// earliest ratchet persist after it finds the re-key listed, resets the
/// window first and holds the ratchet until that reset is queued - the
/// disk pair is both-old or both-new.
#[test]
fn the_committers_ratchet_never_reaches_the_disk_before_its_reset() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let (mut walter, _tmp, dir) = stored_bystander("walter");
    let (mut walter_mls, _petra_mls, dora2) = mls_group();
    walter_mls.restore_member("dora", &dora2).expect("the re-key");
    let mls = std::sync::Arc::new(std::sync::Mutex::new(walter_mls));
    assert_disk_pair_moves_together(&mut walter, &dir, &mls);
}

/// (M3) A BYSTANDER whose `NetPeerRekeyed` was lost (its inbox aborted
/// between the merge and the send): the group's own list still names the
/// seat; the next beat resets it and the ratchet persists again.
#[test]
fn a_lost_rekey_report_is_drained_and_the_ratchet_persists_again() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    let (mut petra, _tmp, dir) = stored_bystander("petra");
    let (mut walter_mls, mut petra_mls, dora2) = mls_group();
    let (commit, _w) = walter_mls.restore_member("dora", &dora2).expect("the re-key");
    petra_mls.decrypt(&commit).expect("petra merges");
    let mls = std::sync::Arc::new(std::sync::Mutex::new(petra_mls));
    petra.group_mls_parked = Some(mls.clone());
    assert_disk_pair_moves_together(&mut petra, &dir, &mls);
    assert!(!mls.lock().expect("mls").rekeys_pending(), "drained");
}

/// The list is drained without any report at all: by the delivery beat,
/// and by a runtime rebuild (a relay-pool change aborting the inbox).
#[test]
fn the_beat_and_a_rebuild_drain_a_listed_rekey() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _guard = rt.enter();
    for via_rebuild in [false, true] {
        let (mut petra, _tmp, _dir) = stored_bystander("petra");
        let (mut walter_mls, mut petra_mls, dora2) = mls_group();
        let (commit, _w) = walter_mls.restore_member("dora", &dora2).expect("the re-key");
        petra_mls.decrypt(&commit).expect("petra merges");
        let mls = std::sync::Arc::new(std::sync::Mutex::new(petra_mls));
        petra.group_mls_parked = Some(mls.clone());
        if via_rebuild {
            petra.rebuild_group_net(false);
        } else {
            petra.cmd_net_delivery_tick().expect("beat");
        }
        let w = window(&petra);
        assert_eq!((w.proof, w.high), (RestoreProof::Merge, 0), "rebuild={via_rebuild}");
        assert!(!mls.lock().expect("mls").rekeys_pending());
    }
}

/// (follow-up 1) After a RESTART the runtime-only newest-seen is empty: a
/// late gap-filler of a `current` window below its stored high must still
/// count as history - no serve, no cleared mark, no head.
#[test]
fn after_a_restart_a_gap_filler_below_the_high_is_history() {
    let mut b = Builder::new(&SEATS, 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    let genesis = walter.chain.head.clone().expect("the genesis");
    b.commit_restored("dora", &anchor(), &["petra", "walter"]);
    block_at(&mut walter, 1, T + 100, b.blocks.last().expect("a block").clone());
    for seq in (1..=40u64).filter(|s| *s != 25) {
        dora_at(&mut walter, seq, T + seq, chat(small(seq), "x"));
    }
    // the restart: everything runtime-only is gone
    walter.delivery.seen_high.clear();
    walter.chain.request_seq.clear();
    walter.chain.peer_claim_seq.clear();
    walter.chain.served_at.clear();
    walter.chain.diverged.insert(
        "dora".to_string(),
        molt_core::ChainDivergence { peer: "dora".to_string(), since_height: 0, seen_ts: T },
    );
    let known = vec![molt_core::HeightHash { height: genesis.height, hash: genesis.hash.clone() }];
    dora_at(&mut walter, 25, T + 25, WorkspaceEvent::ChainRequest { from_height: genesis.height + 1, known });
    assert!(window(&walter).is_accepted(25), "the gap filled");
    assert!(!walter.chain.served_at.contains_key("dora"), "an old request served");
    assert!(walter.chain.diverged.contains_key("dora"), "an old request cleared the mark");
    let mut seats = walter.seat_syncs(&std::collections::HashSet::new());
    walter.fill_seat_reports(&mut seats);
    assert_eq!(seats.iter().find(|s| s.member == "dora").and_then(|s| s.head_height), None, "an old head");
}

/// (follow-up 1) A legacy window: a dropped duplicate never counts as seen -
/// its old high would make a fresh request of the new device history.
#[test]
fn a_dropped_duplicate_never_counts_as_seen() {
    let (mut walter, block) = bystander();
    let genesis = walter.chain.head.clone().expect("the genesis");
    block_at(&mut walter, 1, T + 100, block);
    let mut stale = AcceptedWindow::default();
    assert!(stale.accept(5_000));
    walter.delivery.accepted.insert("dora".to_string(), stale);
    walter.delivery.inbound.clear();
    walter.delivery.seen_high.clear();
    walter.chain.request_seq.clear();
    dora_at(&mut walter, 5_000, T + 300, chat(1, "dup"));
    assert_eq!(bodies(&walter, "dup"), 0, "dropped");
    let known = vec![molt_core::HeightHash { height: genesis.height, hash: genesis.hash.clone() }];
    dora_at(&mut walter, 4_500, T + 301, WorkspaceEvent::ChainRequest { from_height: genesis.height + 1, known });
    assert!(walter.chain.served_at.contains_key("dora"), "the fresh request was taken for history");
}

/// (follow-up 2) `give_up` ends a wait held only by a missing seq - no frame
/// held, nothing accepted yet.
#[test]
fn give_up_ends_a_wait_held_only_by_a_missing_seq() {
    let (mut walter, _) = bystander();
    walter.cmd_resync_seat("dora".to_string(), false).expect("resync");
    walter.note_missing_for_test("dora", 7);
    assert!(walter.cmd_resync_seat("dora".to_string(), false).is_err(), "running");
    walter.cmd_resync_seat("dora".to_string(), true).expect("give up");
    assert!(walter.cmd_resync_seat("dora".to_string(), true).is_err(), "the wait ended");
    assert!(walter.inbound_held_view().iter().all(|r| r.member != "dora"));
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

