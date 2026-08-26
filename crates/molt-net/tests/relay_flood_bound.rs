// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **buzz B3 step 2** — a relay may not answer one REQ with an unbounded
//! stream of stored events.
//!
//! We are the client against relays we do not control, which is buzz's problem
//! mirrored. A relay that replays forever costs us a signature verification and
//! a dedup-ring slot per event, and pushes real deliveries out of the ring.
//!
//! The bound is counted **pre-EOSE only**, and that is the whole subtlety: a
//! legitimate `subscribe_since` catch-up (N5.1) places ONE REQ across several
//! past h-windows and may legitimately replay a lot. A bound that also counted
//! live traffic, or that sat at buzz's chat-shaped 500, would silently truncate
//! a catch-up — and a member quietly missing history it believes it has is a
//! worse outcome than the flood, which at least announces itself.

use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::envelope::h_tag;
use molt_net::relay_runtime::RelayRuntime;
use molt_net::ritual_net::GroupChannel;
use nostr::{Filter, Kind};
use nostr_relay_builder::MockRelay;

fn dialer() -> Dialer {
    Dialer::resolve("none", "local", 0).expect("direct dialer")
}

/// Stuff `n` real kind-445 frames into the relay's store, through the very
/// path a member would use — a flood of WELL-FORMED events is the case that
/// matters, since a malformed one is already dropped by the tag gate.
async fn flood(url: &str, seed: &[u8; 32], n: usize) {
    let chan = GroupChannel::new(dialer(), vec![url.to_string()], *seed);
    for i in 0..n {
        chan.publish_frame(&[9u8; 32], format!("junk {i}").as_bytes())
            .await
            .expect("publish");
    }
}

/// A relay replaying more stored events than the bound is cut off, and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_may_not_replay_more_stored_events_than_the_bound() {
    let relay = MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();
    let seed = [7u8; 32];
    let tag = h_tag(&seed, nostr::Timestamp::now().as_secs());

    // …well past a small test bound, nowhere near the production one
    flood(&url, &seed, 40).await;

    let filter = Filter::new()
        .kind(Kind::Custom(molt_net::kinds::KIND_GROUP))
        .custom_tags(
            nostr::SingleLetterTag::lowercase(nostr::Alphabet::H),
            [tag.clone()],
        );
    let rt = RelayRuntime::new(dialer(), vec![url.clone()]).with_history_bound(10);
    let mut sub = rt.clone().subscribe(filter).await.expect("subscribe");

    // drain what the relay is allowed to give us
    let mut got = 0usize;
    while sub.recv(Duration::from_millis(400)).await.is_some() {
        got += 1;
        assert!(got <= 40, "runaway: the bound did nothing");
    }
    assert!(
        got <= 10,
        "the pre-EOSE bound is 10; the relay got {got} events through"
    );
    assert!(got > 0, "…and the bound must not swallow the whole stream");

    // the cut-off is not silent — the relay's own diagnostics carry it
    // (structured, one line: relay=… bound=… got=…) — and the health map
    // says the relay is DOWN: no supervisor serves it any more, so `deaf()`
    // must not count it as live (review 2026-08-25 T4)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let health = rt.health().await;
        if health.get(&url) == Some(&molt_net::relay_runtime::RelayHealth::Down) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the dropped relay stays Up: {health:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
