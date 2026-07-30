---
status: accepted
---

# N2 WebSocket stack: own client over rustls-rustcrypto, not the rust-nostr relay pool

**Context.** The N2 relay runtime needs a WebSocket client. Two candidates
existed. (a) The ready-made `nostr-relay-pool` (via `nostr-sdk`): pool,
reconnect, subscription plumbing for free — but its transport crate
`async-wsocket` hard-pins `tokio-rustls = { features = ["ring", "tls12"] }`
and `tokio-tungstenite` with `rustls-tls-webpki-roots`; the `ring` rustls
provider is non-optional in that stack (audited N0, 2026-07-30 — no
rustcrypto path, no feature to swap it). Adopting it would return `ring` to
the default build graph, which has been ring-free since N-demo and is guarded
by a standing rule (CLAUDE.md; CI gate is follow-up §7.6 of
`mdk_evaluation.md`). It also brings its own dialing, so the T4 fail-closed
onion-preferred dialer (ADR-0001) cannot front it without fighting the pool.
(b) Drive `tokio-tungstenite` directly over our existing rustls-rustcrypto
client config and the T4 dialer (`crates/molt-net/src/dial.rs`), writing
connect/backoff/health ourselves.

**Decision.** Build the own client (b). `tokio-tungstenite` is driven
directly: TCP/SOCKS5 through `relay::dialable(...)` and the T4 fail-closed
dialer, TLS through the rustls-rustcrypto provider we already ship, WS on
top. The rust-nostr pool and `nostr-relay-builder` remain **dev-dependencies
only** (the in-process test relay). The default no-dev graph stays ring-free.

**Why.**

- **The ring guard holds.** Re-adopting `ring` was explicitly reserved as an
  N2 decision, never a side effect; taking the pool would have been exactly
  that side effect. The pure-Rust posture survives at the cost of code we
  largely had to write anyway.
- **The T4 dialer is the privacy load-bearing piece** (ADR-0001/0004:
  onion-preferred, fail-closed, warned clearnet, per-session gate). An own
  client threads every connection through it naturally; the pool would dial
  on its own terms.
- **The pool's real value is portable without the pool.** Connect/backoff/
  health were already budgeted in N2, and the hard-won relay-client
  behaviours (durable outbound fanout, `duplicate:` = success, since-widening,
  EOSE gating, canonical endpoint matching, multi-route backfill) are being
  ported as the six MDK adapter behaviours (`mdk_evaluation.md` §2.2)
  regardless of the WS layer underneath.

**Consequences.** We own reconnect/backoff/health/cursor logic and its tests
(N2 keystones cover them). `nostr` (the base crate, C secp256k1 per ADR-0002)
stays the only rust-nostr runtime dependency; a future upstream that unpins
ring could reopen the pool question, but nothing in our layering depends on
that. Decided 2026-07-31 with the user; recorded in the concept §10.12.
