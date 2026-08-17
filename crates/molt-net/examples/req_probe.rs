// SPDX-License-Identifier: GPL-3.0-or-later
//! Dev probe: place one kind-445 `#h` subscription on a relay through the
//! REAL runtime (supervisors, EOSE gate) and print the sync verdict.
//! Usage: `PROBE_NET=tor cargo run -p molt-net --example req_probe -- wss://nos.lol <h-tag>…`

use std::time::Duration;

use molt_net::dial::Dialer;
use molt_net::relay_runtime::RelayRuntime;
use nostr::{Alphabet, Filter, Keys, Kind, SingleLetterTag};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("molt_net=debug")
        .init();
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("usage: req_probe <relay-url> [h-tag…]");
    let tags: Vec<String> = args.collect();
    let net = std::env::var("PROBE_NET").unwrap_or_else(|_| "none".into());
    let dialer = Dialer::resolve(&net, "local", 9050).expect("dialer");
    let mut filter = Filter::new().kind(Kind::Custom(445));
    if !tags.is_empty() {
        filter = filter.custom_tags(SingleLetterTag::lowercase(Alphabet::H), tags.iter().cloned());
    } else {
        filter = filter.limit(3);
    }
    // occupy N connections first (same dialer = same pooled circuit), the
    // way a recovering moltd already holds inbox/publish/anchor connections
    let held: usize = std::env::var("PROBE_HELD").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut parked = Vec::new();
    for i in 0..held {
        let rt = RelayRuntime::new(dialer.clone(), vec![url.clone()])
            .with_auth_keys(Some(Keys::generate()));
        let sub = rt
            .subscribe(Filter::new().kind(Kind::Custom(445)).limit(1))
            .await
            .expect("held subscribe");
        println!("held connection {i} up");
        parked.push((rt, sub));
    }
    let rt = RelayRuntime::new(dialer, vec![url]).with_auth_keys(Some(Keys::generate()));
    let mut sub = rt.subscribe(filter).await.expect("subscribe");
    let st = sub.sync_state(Duration::from_secs(10)).await;
    println!("sync_state: {st:?}  any={}", st.any());
}
