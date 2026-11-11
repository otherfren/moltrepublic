// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **N5.1 — the catch-up subscription** (`nostr_n5_plan.md` §4).
//!
//! OWN TEST BINARY: `shift_window_clock_for_tests` is a process-global seam,
//! so it would leak into every other test sharing a binary.
//!
//! The pair below is the whole point. A live `subscribe()` names only the
//! CURRENT UTC-day window's `h` tag (plus one adjacent inside the skew
//! margin), and an `h` tag is `SHA256(seed ‖ le64(unix/86400))` — so a frame
//! published three days ago sits under a tag the live subscription never
//! asks for. It is invisible **because we do not ask**, not because the relay
//! pruned it. A returning member that only ever calls `subscribe()` is
//! therefore permanently blind to everything it slept through, however
//! faithfully the relay stored it.

use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::envelope::H_WINDOW;
use molt_net::ritual_net::{shift_window_clock_for_tests, GroupChannel, GroupRecv};
use nostr_relay_builder::MockRelay;

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

// the window clock is process-global: one test at a time
static CLOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SEED: [u8; 32] = [7u8; 32];
const EXPORTER: [u8; 32] = [9u8; 32];

/// A frame from three windows ago is invisible to a live subscription and
/// visible to a catch-up one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_catch_up_subscription_sees_what_the_live_one_never_asks_for() {
    let _clock = CLOCK.lock().await;
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let chan = GroupChannel::new(dialer(), vec![url], SEED);

    // …three days ago: publish under THAT window's h tag
    shift_window_clock_for_tests(-3 * i64::try_from(H_WINDOW).expect("window fits"));
    chan.publish_frame(&EXPORTER, b"three days ago")
        .await
        .expect("the old frame publishes");

    // …back to now
    shift_window_clock_for_tests(0);

    // the LIVE subscription asks only for today's tag — it must not see it
    let mut live = chan.subscribe().await.expect("live subscribe");
    live.live(Duration::from_secs(3)).await;
    assert!(
        matches!(live.recv(Duration::from_millis(600)).await, GroupRecv::Idle),
        "a live subscription must not see a frame from three windows ago — \
         if it does, this test proves nothing about the catch-up"
    );
    drop(live);

    // …and the CATCH-UP subscription, naming the windows it slept through, must
    let mut back = chan
        .subscribe_since(3 * H_WINDOW, 8)
        .await
        .expect("catch-up subscribe");
    back.live(Duration::from_secs(3)).await;
    let got = back.recv(Duration::from_secs(3)).await;
    assert!(
        matches!(got, GroupRecv::Frame { .. }),
        "the catch-up must replay the frame from three windows ago, got {got:?}"
    );

    // …and it does not lose its windows at a UTC boundary. `GroupSub::recv`
    // re-places under exactly the current window's tags whenever they are not
    // covered, overwriting the set it was opened with; a catch-up doing that
    // would throw away the very range it exists for. Roll a whole window
    // forward mid-replay and the range must still deliver.
    shift_window_clock_for_tests(i64::try_from(H_WINDOW).expect("fits"));
    let after_roll = back.recv(Duration::from_secs(3)).await;
    shift_window_clock_for_tests(0);
    assert!(
        !matches!(after_roll, GroupRecv::Deaf(_)),
        "a boundary roll must not make the catch-up deaf, got {after_roll:?}"
    );
}


/// A ritual subscription keeps the windows since its start across a roll:
/// a frame published before midnight is still delivered after it, and a new
/// one under the fresh window arrives too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ritual_subscription_keeps_earlier_windows_across_a_roll() {
    let _clock = CLOCK.lock().await;
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let chan = GroupChannel::new(dialer(), vec![url], [8u8; 32]);
    let start = molt_net::ritual_net::unix_now();

    // published on the ritual's first day, while nobody listened
    chan.publish_frame(&EXPORTER, b"day one").await.expect("day-one frame");

    // two days later a member finally subscribes
    shift_window_clock_for_tests(2 * i64::try_from(H_WINDOW).expect("fits"));
    let mut sub = chan.subscribe_from(start).await.expect("ritual subscribe");
    sub.live(Duration::from_secs(3)).await;
    let got = sub.recv(Duration::from_secs(3)).await;
    assert!(matches!(got, GroupRecv::Frame { .. }), "day one is replayed, got {got:?}");

    // the next roll re-places the subscription: the old range must survive
    shift_window_clock_for_tests(3 * i64::try_from(H_WINDOW).expect("fits"));
    chan.publish_frame(&EXPORTER, b"day four").await.expect("day-four frame");
    let mut frames = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while frames < 2 && tokio::time::Instant::now() < deadline {
        if let GroupRecv::Frame { .. } = sub.recv(Duration::from_secs(2)).await {
            frames += 1;
        }
    }
    shift_window_clock_for_tests(0);
    assert_eq!(frames, 2, "the re-placed subscription replays day one and hears day four");
}
