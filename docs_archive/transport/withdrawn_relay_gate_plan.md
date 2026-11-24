# A withdrawn relay is not dialed: one gate at the dial

Status: EXECUTED (v0.1.3). The behaviour is specified in
`docs_archive/transport/relay_pool.md` §3 rule 3; this file keeps the
reasoning.

## The gap

After consent for a relay is withdrawn (the non-onion switch off, a
revoke), the group runtime stops (v0.1.2). Other tasks hold their own
channel and still dial it. All of them predate v0.1.0.

| path | where | after the withdrawal |
|---|---|---|
| one-shot publish (seat inbox, ritual) | `relay_runtime.rs` `RelayRuntime::publish` | detached, one attempt |
| series publish (a shared file) | `net/files.rs` -> `transfer::spawn_series_publish` | detached, no handle, minutes |
| downloads | `net/files.rs` `spawn_nostr_fetch`, `spawn_fetch_job` | aborted only at close |
| vault payload, wiki base, vault base fetch | `vault_payload.rs`, `wiki_base.rs`, `vault_base.rs` | not aborted |
| re-key delivery | `chain/membership.rs`, `nostr_ritual.rs` | own channel, retries with backoff |
| a dial begun before the withdrawal | `relay_ws.rs` `RelayWs::connect` | completes its handshake, sends nothing |

## Why not abort the tasks

Aborting by task is per relay set, not per relay: a re-key delivery
aborted because ONE relay was withdrawn never reaches the rejoiner over
the remaining ones. The check belongs where the relay is known: the dial.

## The gate

Every relay socket of a running node opens in `RelayWs::connect`; the
NIP-11 probe goes through `dial_maybe_tls`.

- `Dialer` becomes `{ route, gate: Option<RelayGate> }` (today an enum,
  `Direct` a unit variant built or matched at about 70 sites).
- `RelayGate`: an `Arc` holding the engine's current dialable set and an
  epoch. `dialer_for()` attaches it; `dialable_relays_changed` updates it.
- `RelayWs::connect` refuses a relay outside the set before any socket
  opens (`NetError::Withdrawn`) and checks again between the TCP connect
  and the WebSocket handshake. A retry loop ends on that error.
- Probe and adoption of a relay run with `gate: None`.
- Per engine, never per process: several engines share a test process,
  and a workspace switch must not inherit a set.

## Tests

One per path class above: after the withdrawal returned, the proxy sees
no TCP accept for that relay; switching back on dials again. The engine
test `a_withdrawn_relay_is_not_redialed_by_the_running_republic` drops
its grace for in-flight dials.

## Decided while building

- The set is the pool's consent (`relay::dialable(pool, clearnet)`), not
  the group's ratified set: a founding or join dials pool relays before
  any group exists, and a pool vote is no withdrawal of consent.
- The keeper is a `watch` channel: `dialer_for` re-publishes the set on
  every dialer it hands out, so a path that edits the pool without
  `dialable_relays_changed` cannot leave the gate stale.
- An established socket ends at once, not at its owner's next send: a
  waiting `RelayWs::recv` selects on the gate and returns
  `Dead(Withdrawn)`, a send or ping refuses, `close` sends no close frame.
- Owed work waits for consent instead of ending: a subscription
  supervisor, the re-key delivery, recovery frames and a seat Welcome park
  on the gate (no timer, no dial) and resume when a relay is allowed
  again; they end with the engine. Optional work ends: a piece publish
  (`transient_publish_error`), a refusal retry, the publish pool's breaker
  is not tripped (consent given again dials at once).
- The Tor test button carries the gate: its target is a dialable relay,
  so the gate only stops a test racing that relay's withdrawal.
- The NIP-11 probe and the relay probe of a relay being decided on run
  `ungated()`; S3 and the wallet daemon never consult the gate (they use
  `dial_host` / `dial_maybe_tls`, not `RelayWs::connect`).
- The ritual inboxes (founder, join, recovery) wait for consent too.
  `spawn_publish_frame` keeps its timers (its first outcome must be
  reported) and the recovery member's asks keep theirs; both log a
  withdrawal once (`OncePerWithdrawal`).
- Review round (v0.1.3): a dial in flight is dropped at the withdrawal in
  any stage (`select!` on `RelayGate::withdrawn`); an idle pooled publish
  socket is closed by a watcher; the relay probe runs under its own
  one-URL keeper; the NIP-11 probe asks the gate.
- Known limit (L1, left to the re-key delivery work): a re-key delivery
  waits on the relays captured at re-key
  time. If all are withdrawn and a different relay is allowed, its
  Welcome waits until one of the captured ones returns or the engine
  ends; the commit still reaches the group over the outbox.
