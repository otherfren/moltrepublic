# A withdrawn relay is not dialed: one gate at the dial

Status: PLAN, not started; candidate for v0.1.3. Spec it amends:
`docs_archive/transport/relay_pool.md` §3 (withdrawing consent reaches
the live transport at once).

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

## Open

- Does the Tor test button (`session.rs`) carry the gate?
- An established socket to a withdrawn relay: dropped by its owner at the
  next send, or closed by the gate's epoch change?
