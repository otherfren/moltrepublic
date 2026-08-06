// SPDX-License-Identifier: GPL-3.0-or-later

//! A throwaway Nostr relay for driving a real `moltd` by hand.
//!
//! The test suite gets its relay from `MockRelay::run()` inside the test
//! process; a developer poking at the daemon over MCP has no such thing, and
//! founding refuses without a confirmed relay ("cannot found: no relay
//! configured"). This prints a `ws://127.0.0.1:<port>` URL and stays up until
//! killed — add and confirm it through `relay_add` / `relay_confirm`, or the
//! GUI's relay settings.
//!
//! ```text
//! cargo run -p molt-net --example dev_relay
//! ```
//!
//! It keeps everything in memory: stop it and the republic's history is gone
//! with it. That is the point — it is for driving flows, never for anything
//! whose loss would matter.

use nostr_relay_builder::MockRelay;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let relay = MockRelay::run().await?;
    // stdout, one line, so a script can read it: the caller's next step is
    // pasting this into a node's relay pool
    println!("{}", relay.url().await);
    // …and then simply exist. Ctrl-C ends it.
    std::future::pending::<()>().await;
    Ok(())
}
