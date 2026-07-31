# N2 execution plan — NostrTransport core (relay runtime)

Status: **IN BUILD (started 2026-07-31).** Executes the N2 etappe of
`nostr_transport_marmot.md` §11 under the decided stack (§10.12/ADR-0005:
own WS client, no rust-nostr pool; ring stays out). Every design input is
already ratified — this document is the execution map, not a discussion.

## 0. Scope

IN: the relay-side runtime core in `molt-net` — connections, publish,
subscriptions, cursors, dedup, size budget, AUTH, health — plus its test
seam (the in-process relay). NOT in N2: the 444/445 envelope layer (N3),
the engine wiring/`TransportKind` fork (N4/N5), presence/health GUI copy
(N5/N6). N2's consumer contract is a `NostrRelayRuntime` handle that N3
builds the envelope layer on.

## 1. Stack (decided; search-first verdicts)

- **WS client:** `tokio-tungstenite` driven directly (ADR-0005), TLS via
  the rustls-rustcrypto provider already used by the S3 client, TCP/SOCKS
  through `Dialer::dial_host` (T4, fail-closed). `tokio-tungstenite`'s
  `client_async` over an arbitrary `AsyncRead+AsyncWrite` stream fits the
  `DialStream` shape — no second dial path.
- **NIP-01 message framing:** `nostr::ClientMessage`/`nostr::RelayMessage`
  (the base crate we already ship, ADR-0002) — serde to/from the wire JSON.
  We do NOT hand-roll the message JSON (CLAUDE.md search-first rule).
- **NIP-11:** one HTTP/1.1 GET (`Accept: application/nostr+json`) over the
  SAME `DialStream` path (never a second HTTP client dependency — reqwest
  et al. would drag their own TLS). Response head parsed with `httparse`
  (already in the tree via tungstenite); body parsed with
  `nostr::nips::nip11::RelayInformationDocument`.
- **NIP-42:** challenge/response via `nostr` nip42 event building, signed
  with the per-republic transport anchor (`transport.state.nostr_sk`).
  **Two connections per relay** (mdk_evaluation §5): SUBSCRIBE may
  authenticate; PUBLISH stays unauthenticated so ephemeral-key events are
  not linkable to the member — if a relay refuses unauthenticated publish,
  publishing moves to the authenticated connection and the §7.5 linkage
  warning is surfaced (never silent).
- **Policy source:** the runtime consumes `molt_core::relay::dialable(...)`
  ONLY (ADR-0004) — empty pool = connect to nothing, silently. Local
  (`RelayKind::Local`) hosts dial DIRECT (never over Tor); `localhost`
  names are pinned to loopback by the dialer side, per relay_pool.md §3.

## 2. Module layout (all in `crates/molt-net/src/`)

- `relay_ws.rs` — ONE relay connection: dial (via `Dialer`), WS upgrade,
  read/write split, typed `ClientMessage`/`RelayMessage` I/O, NIP-42
  challenge capture. No policy, no retry — a dumb pipe with a typed edge.
- `relay_runtime.rs` — the pool runtime over `relay_ws`:
  - connection supervision per dialable relay (connect, exponential
    backoff 1s..60s with jitter, health `Up|Connecting|Down{reason}`),
  - `publish(event) -> ≥1-OK` semantics: success iff at least one relay
    OKs (NIP-01 `duplicate:` on `OK:false` COUNTS AS SUCCESS — MDK port
    #3); per-relay outcomes reported, never a silent partial,
  - NIP-11 size budget: probe `max_message_length` per relay at connect;
    refuse (loud, typed error) any publish exceeding the SMALLEST
    configured relay's cap,
  - one pooled subscription per h-tag filter; inbound fan-in with
    **event-id dedup** (bounded ring),
  - **per-relay cursor**: advance on delivered events, clamp `since` to
    `now + skew` (the +24h keystone), re-subscribe with the 172_800 s
    overlap widening (MDK port #2),
  - **EOSE gate**: "synced" only when EVERY connected relay sent EOSE
    (MDK port #6),
  - canonical endpoint identity = the normalized pool URL (MDK port #5 —
    already guaranteed by `normalize_relay_url`),
  - per-connection decrypt-failure circuit breaker seam: the consumer
    reports undecryptable events; past a threshold the connection is
    dropped and backed off (the relay is feeding garbage).
- Test seam: `nostr-relay-builder` in-process relay (dev-dep since N0) —
  `ws://127.0.0.1:PORT`, which the §10.14 Local policy admits.

## 3. TDD order (each step: red first, then green; keystones marked)

1. `relay_ws`: connect + publish EVENT → OK, REQ → stored EVENT + EOSE,
   against the in-process relay.
2. **KEYSTONE publish ≥1-OK:** two relays, one dead → publish succeeds
   with per-relay outcomes; all dead → typed failure. `duplicate:` OK:false
   counts as success (pin with a pre-seeded event).
3. **KEYSTONE dedup:** the same event delivered by two relays reaches the
   consumer once; the dedup ring is bounded.
4. **KEYSTONE cursor clamp (+24h):** a peer publishing `created_at` +24h
   does not blind the receiver after reopen — cursor clamps to local now +
   skew; resubscribe applies the 172_800 s overlap.
5. EOSE gate: sync completes only when every live relay EOSE'd; a dead
   relay does not wedge the gate (it is not "every configured", it is
   "every CONNECTED").
6. **KEYSTONE size budget:** an event over the smallest relay cap is
   refused loudly BEFORE any relay sees it (the oversized-
   `CheckpointServed` case); NIP-11 probe failure ⇒ conservative default
   (128 KiB, the measured nos.lol floor).
7. Reconnect/backoff/health: kill the in-process relay → Down + backoff;
   restart → Up; cursor overlap covers the gap.
8. **KEYSTONE WS no-leak twin:** Tor-required config → the WS dial reaches
   the SOCKS proxy, never the relay host directly (blackhole harness
   pattern); empty/unconfirmed pool → zero connections (`relay::dialable`
   is the only source).
9. NIP-42: challenge → signed auth on the subscribe connection; publish
   connection stays unauthenticated; the forced-auth-publish warning path.

## 4. Carried obligations

- The ring guard (`ring_free_guard.rs`) must stay green — tungstenite is
  pulled WITHOUT its `rustls-tls-*` features (we bring our own TLS).
- `cargo tree -p molt-net -e no-dev -i ring` empty; no `nostr-relay-pool`
  outside dev-deps.
- Real-relay `#[ignore]` twin extends `nostr_relay_poc.rs` once the
  runtime exists (soak = observational retention measurement, §11 N0).
