// SPDX-License-Identifier: GPL-3.0-or-later

//! `dev_relay` without its rate limit and replay cap, for replaying a
//! stored republic against it
//! (`docs_archive/chain/rejoin_divergence_analysis_plan.md`, Phase L).
//!
//! ```text
//! cargo run -p molt-net --example sandbox_relay -- [port]
//! ```

use nostr_relay_builder::builder::RateLimit;
use nostr_relay_builder::{LocalRelay, RelayBuilder};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = RelayBuilder::default()
        .rate_limit(RateLimit { max_reqs: 100_000, notes_per_minute: u32::MAX })
        .default_filter_limit(1_000_000);
    if let Some(port) = std::env::args().nth(1) {
        builder = builder.port(port.parse()?);
    }
    let relay = LocalRelay::new(builder);
    relay.run().await?;
    println!("{}", relay.url().await);
    std::future::pending::<()>().await;
    Ok(())
}
