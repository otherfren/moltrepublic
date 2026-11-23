// SPDX-License-Identifier: GPL-3.0-or-later

//! `status.seats[]`: each seat's build, reported head and divergence.

use crate::chain::test_support::{chain_peer_3, wire, Builder};
use crate::net::seat_report::BUILD;
use molt_core::{HeightHash, SeatSync, WorkspaceEvent};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime")
}

fn seat<'a>(seats: &'a [SeatSync], m: &str) -> &'a SeatSync {
    seats.iter().find(|s| s.member == m).expect("a roster seat")
}

fn declare(st: &mut crate::State, from: &str, at: u64, build: &str) {
    st.cmd_net_mirror_decl(&from.to_string(), false, 0, 0, Vec::new(), at, build).expect("declaration");
}

/// Two seats name their build, the third declares the way an older
/// build does (no field): it reads unknown, never a guess.
#[test]
fn seats_show_their_build_and_an_older_one_reads_unknown() {
    let rt = rt();
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_peer_3("walter", &b);
    declare(&mut walter, "petra", 100, "0.1.2");
    declare(&mut walter, "dora", 100, "");
    let seats = walter.status().seats;
    assert_eq!(seat(&seats, "walter").build.as_deref(), Some(BUILD), "the own seat");
    assert_eq!(seat(&seats, "petra").build.as_deref(), Some("0.1.2"));
    assert_eq!(seat(&seats, "dora").build, None, "older than v0.1.2");
    let json = serde_json::to_value(seat(&seats, "dora")).expect("json");
    assert!(json.get("build").is_none(), "absent, not null: {json}");
}

/// A seat that restarts on a new build says so with its next declaration
/// (it declares at open); a reordered older one does not undo it. A new
/// device (incarnation) restarts the order.
#[test]
fn a_restarted_seat_is_read_on_its_next_declaration() {
    let rt = rt();
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_peer_3("walter", &b);
    walter.presence.clock_override = Some(1_000);
    let build = |st: &crate::State| seat(&st.status().seats, "petra").build.clone();
    declare(&mut walter, "petra", 100, "0.1.1");
    declare(&mut walter, "petra", 200, "0.1.2");
    assert_eq!(build(&walter).as_deref(), Some("0.1.2"));
    declare(&mut walter, "petra", 150, "0.1.1");
    assert_eq!(build(&walter).as_deref(), Some("0.1.2"), "an older stamp does not win");
    walter.reset_peer_accept_window(&"petra".to_string());
    declare(&mut walter, "petra", 50, "0.1.3");
    assert_eq!(build(&walter).as_deref(), Some("0.1.3"), "the new device's clock counts");
}

/// Our own declaration carries the build, and a who-ask makes this seat
/// declare again (the path a restarted peer's open triggers).
#[test]
fn the_own_declaration_carries_the_build_and_a_who_ask_owes_one() {
    let rt = rt();
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_peer_3("walter", &b);
    assert_eq!(walter.mirror_decl_frame(5).build, BUILD);
    walter.files.mirror_decl_due = false;
    walter.cmd_net_mirror_who(&"petra".to_string()).expect("who");
    assert!(walter.files.mirror_decl_due, "the asker is answered with a declaration");
}

/// The head a peer reports in its catch-up request is shown as it
/// reported it: 12 hex of the hash and the height. The own seat shows
/// its own head; a peer on another branch reads diverged.
#[test]
fn a_reported_head_and_divergence_reach_the_seat_row() {
    let rt = rt();
    let _guard = rt.enter();
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_applied(1, &["petra", "dora"]);
    let mut walter = chain_peer_3("walter", &b);
    let head = HeightHash { height: 1, hash: b.head_hash.clone() };
    wire(&mut walter, "petra", 1, WorkspaceEvent::ChainRequest { from_height: 2, known: vec![head] });
    walter.chain.diverged.insert(
        "dora".to_string(),
        molt_core::ChainDivergence { peer: "dora".to_string(), since_height: 1, seen_ts: 1 },
    );
    let seats = walter.status().seats;
    let petra = seat(&seats, "petra");
    assert_eq!(petra.head_hash.as_deref(), Some(&b.head_hash[..12]));
    assert_eq!((petra.head_height, petra.diverged), (Some(1), false));
    let own = seat(&seats, "walter");
    assert_eq!((own.head_hash.as_deref(), own.head_height), (Some(&b.head_hash[..12]), Some(1)));
    let dora = seat(&seats, "dora");
    assert!(dora.diverged);
    assert_eq!(dora.head_hash, None, "never reported");
}

/// A peer's self-description is shown only in a safe shape: a build of at
/// most 32 chars of `[0-9A-Za-z.+-]`, a head hash of hex. Anything else
/// reads unknown.
#[test]
fn a_malformed_build_or_head_hash_reads_unknown() {
    let rt = rt();
    let _guard = rt.enter();
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_peer_3("walter", &b);
    declare(&mut walter, "petra", 100, "0.1.2+abc-1");
    declare(&mut walter, "dora", 100, "0.1.2\nWARN forged");
    let seats = walter.status().seats;
    assert_eq!(seat(&seats, "petra").build.as_deref(), Some("0.1.2+abc-1"));
    assert_eq!(seat(&seats, "dora").build, None, "a control character");
    declare(&mut walter, "dora", 200, &"9".repeat(33));
    assert_eq!(seat(&walter.status().seats, "dora").build, None, "too long");
    let bad = HeightHash { height: 3, hash: "zz<script>".into() };
    wire(&mut walter, "petra", 1, WorkspaceEvent::ChainRequest { from_height: 4, known: vec![bad] });
    let petra = walter.status().seats.into_iter().find(|s| s.member == "petra").expect("petra");
    assert_eq!((petra.head_hash, petra.head_height), (None, None), "not hex");
}
