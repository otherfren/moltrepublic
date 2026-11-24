// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **The open probe over a slow sender** (`delivery_guarantee.md` §13):
//! every listing answer comes 3 s late and every re-published frame takes
//! 400 ms, so a page fills slower than the 60 s order bound and no answer
//! lands on the tick after its ask. The process-wide seams make this file
//! hold only this test.

mod common;

use common::{fetches_the_tail_and_says_so, found_pair, hold_tail_and_reopen, read_chat};
use std::time::Duration;

use molt_core::Command;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_sender_still_brings_the_whole_tail_and_behind_shows_until_it_has() {
    molt_net::group_runtime::__set_resend_backoff(Duration::from_secs(600), Duration::from_secs(600));
    let db = std::sync::Arc::new(nostr_relay_builder::prelude::MemoryDatabase::with_opts(
        nostr_relay_builder::prelude::MemoryDatabaseOptions { events: true, max_events: None },
    ));
    let relay = nostr_relay_builder::LocalRelay::new(nostr_relay_builder::RelayBuilder::default().database(db).rate_limit(
        nostr_relay_builder::builder::RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 },
    ));
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    let tmp = tempfile::tempdir().expect("tmp");
    let (a, b) = found_pair(tmp.path(), &[&url], "Gap").await;
    a.execute(Command::Chat { body: "warm".to_string(), quote: None, channel: molt_core::ChannelRef::Group })
        .await
        .expect("chat");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !read_chat(&b).await.iter().any(|r| r.get("body").and_then(|v| v.as_str()) == Some("warm")) {
        assert!(std::time::Instant::now() < deadline, "warm never arrived");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(5)).await;

    const SPAN: usize = 300;
    molt_net::group_runtime::__set_gap_delays(Duration::from_secs(3), Duration::from_millis(400));
    let reopened = hold_tail_and_reopen(&a, &[&b], SPAN, false).await;
    let took = fetches_the_tail_and_says_so(&b, "petra", SPAN, reopened, Duration::from_secs(30), Duration::from_secs(560)).await;
    eprintln!("tail_after={took:?}");

    a.execute(Command::CloseWorkspace).await.expect("close a");
    b.execute(Command::CloseWorkspace).await.expect("close b");
}
