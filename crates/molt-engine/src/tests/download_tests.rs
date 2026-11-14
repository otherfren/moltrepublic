// SPDX-License-Identifier: GPL-3.0-or-later
//! Download progress and states (`docs_archive/files/download_progress.md`): the
//! view keeps what earlier phases measured, cancel ends a download for
//! good, the rate and the wait derive from the clock, a serving seat's
//! report rides along, and a whole holder's stamp is trusted.

use super::support::*;
use crate::*;

fn peer(st: &State) -> MemberId {
    st.roster().into_iter().find(|m| *m != st.member()).expect("a peer in the roster")
}

/// A share `by` posted, as every seat sees it.
fn share_from(st: &mut State, by: &MemberId, n: u8) -> MessageId {
    let id = MessageId([n; 16]);
    let mut msg = molt_core::ChatMessage::text(id, by, "a share", now_secs());
    msg.file = Some(molt_core::FileMeta {
        name: "a.bin".to_string(),
        size: 440_000,
        kind: "BIN".to_string(),
        modified: 100,
        available: true,
        checksum: "ab".repeat(32),
        key_b64: String::new(),
        pieces: 10,
        root: String::new(),
    });
    let env = st.make_env(by.clone(), molt_core::WorkspaceEvent::Chat(msg));
    st.apply(&env);
    id
}

fn begin(st: &mut State, id: MessageId) {
    st.files.downloads.insert(
        id,
        molt_core::DownloadView {
            phase: "requested".to_string(),
            size: 440_000,
            since: now_secs(),
            ..molt_core::DownloadView::default()
        },
    );
}

#[test]
fn progress_fills_the_view_and_the_rate_needs_three_samples() {
    let mut st = plain_state();
    let me = st.member();
    let id = MessageId([1; 16]);
    begin(&mut st, id);
    st.set_download_phase(id, molt_core::TransferPhase::Waiting);
    assert_eq!(st.download_view(&id, &me).expect("view").phase, "waiting");
    st.note_download_progress(id, 44_000, 440_000, 1, 10);
    let v = st.download_view(&id, &me).expect("view");
    assert_eq!((v.phase.as_str(), v.percent, v.held, v.of, v.bytes), ("transferring", 10, 1, 10, 44_000));
    assert!(v.last_piece > 0 && v.since > 0);
    assert_eq!((v.rate, v.eta), (0, 0), "one sample is no rate");
    // three samples over 10 s: 88 000 B in 10 s
    let now = now_secs();
    st.files.dl_samples.insert(id, [(now - 10, 44_000), (now - 5, 88_000), (now, 132_000)].into_iter().collect());
    if let Some(d) = st.files.downloads.get_mut(&id) {
        d.bytes = 132_000;
    }
    let v = st.download_view(&id, &me).expect("view");
    assert_eq!(v.rate, 8_800);
    assert_eq!(v.eta, (440_000 - 132_000) / 8_800);
    st.set_download_phase(id, molt_core::TransferPhase::Verifying);
    let v = st.download_view(&id, &me).expect("view");
    assert_eq!((v.phase.as_str(), v.held, v.of), ("verifying", 1, 10), "a phase move keeps the counts");
    assert!(st.download_running(&id));
    st.set_download_phase(id, molt_core::TransferPhase::Failed { reason: "the landed bytes do not match the share checksum".into() });
    let v = st.download_view(&id, &me).expect("view");
    assert_eq!((v.phase.as_str(), v.code.as_str()), ("failed", "checksum"));
    assert!(!st.download_running(&id));
}

#[test]
fn cancel_ends_the_download_and_late_results_are_ignored() {
    let mut st = plain_state();
    let id = MessageId([2; 16]);
    assert!(st.cmd_cancel_download(id).is_err(), "nothing runs");
    begin(&mut st, id);
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    st.files.dl_cancel.insert(id, tx);
    st.note_download_progress(id, 44_000, 440_000, 1, 10);
    st.cmd_cancel_download(id).expect("cancel");
    assert!(*rx.borrow_and_update(), "the fetch task hears the cancel");
    let v = st.files.downloads.get(&id).expect("view");
    assert_eq!((v.phase.as_str(), v.held, v.bytes), ("cancelled", 0, 0));
    assert!(!st.download_running(&id));
    // a piece the task reported before it stopped changes nothing
    st.note_download_progress(id, 88_000, 440_000, 2, 10);
    assert_eq!(st.files.downloads.get(&id).expect("view").phase, "cancelled");
}

#[test]
fn the_wait_says_who_or_what_holds_the_download() {
    let mut st = plain_state();
    let me = st.member();
    let other = peer(&st);
    let id = MessageId([3; 16]);
    begin(&mut st, id);
    st.set_download_phase(id, molt_core::TransferPhase::Waiting);
    assert_eq!(st.download_view(&id, &me).expect("view").wait, "sharer");
    assert_eq!(st.download_view(&id, &other).expect("view").wait, "sharer_offline", "never seen = offline");
    st.set_download_phase(id, molt_core::TransferPhase::Progress { percent: 10, held: 1, of: 10 });
    if let Some(d) = st.files.downloads.get_mut(&id) {
        d.since = now_secs() - 600;
        d.last_piece = now_secs() - 300;
    }
    assert_eq!(st.download_view(&id, &me).expect("view").wait, "relays");
    if let Some(d) = st.files.downloads.get_mut(&id) {
        d.asked = now_secs() - 10;
    }
    assert_eq!(st.download_view(&id, &me).expect("view").wait, "pieces");
    if let Some(d) = st.files.downloads.get_mut(&id) {
        d.last_piece = now_secs();
    }
    assert_eq!(st.download_view(&id, &me).expect("view").wait, "", "a piece just came");
}

#[test]
fn a_serving_report_replaces_the_members_last_and_shows_while_running() {
    let mut st = plain_state();
    let me = st.member();
    let other = peer(&st);
    let (a, b) = (MessageId([4; 16]), MessageId([5; 16]));
    let view = |published| molt_core::ServeView { published, of: 10, gate: "daily".into(), at: 1 };
    st.note_remote_serving(&other, vec![(a, view(3)), (b, view(4))]);
    st.note_remote_serving(&me, vec![(a, view(9))]);
    assert_eq!(st.files.serve_remote.get(&a).map(|(_, v)| v.published), Some(3), "own frames are no report");
    st.note_remote_serving(&other, vec![(a, view(5))]);
    assert_eq!(st.files.serve_remote.get(&a).map(|(_, v)| v.published), Some(5));
    assert!(!st.files.serve_remote.contains_key(&b), "a series left out is done");
    begin(&mut st, a);
    assert!(st.download_view(&a, &other).expect("view").sharer.is_none(), "requested shows none");
    st.set_download_phase(a, molt_core::TransferPhase::Waiting);
    assert_eq!(st.download_view(&a, &other).expect("view").sharer.map(|s| s.published), Some(5));
}

#[test]
fn a_served_stamp_counts_from_the_sharer_or_a_whole_holder_only() {
    let mut st = plain_state();
    let roster = st.roster();
    let me = st.member();
    let others: Vec<MemberId> = roster.into_iter().filter(|m| *m != me).collect();
    assert!(others.len() >= 2, "the demo roster has two peers");
    let (sharer, holder) = (others[0].clone(), others[1].clone());
    let id = share_from(&mut st, &sharer, 6);
    assert!(st.served_stamp_trusted(&id, &sharer));
    assert!(!st.served_stamp_trusted(&id, &holder), "a member holding nothing");
    st.files.mirror.status.insert(holder.clone(), vec![molt_core::MirrorHold { id, held: 4, of: 10 }]);
    assert!(!st.served_stamp_trusted(&id, &holder), "a partial holder");
    st.files.mirror.status.insert(holder.clone(), vec![molt_core::MirrorHold { id, held: 10, of: 10 }]);
    assert!(st.served_stamp_trusted(&id, &holder), "a whole holder re-seeds");
}

#[test]
fn failure_reasons_map_to_stable_codes() {
    use crate::transfer::failure_code;
    assert_eq!(failure_code("the landed bytes do not match the share checksum"), "checksum");
    assert_eq!(failure_code("creating /x: permission denied"), "write");
    assert_eq!(failure_code("moving into place: denied"), "write");
    assert_eq!(failure_code("no dialable relay for the file plane"), "no_relay");
    assert_eq!(failure_code("the share expired"), "expired");
    assert_eq!(failure_code("the file changed since it was shared"), "changed");
    assert_eq!(failure_code("fetching the piece series: framing"), "transfer");
}
