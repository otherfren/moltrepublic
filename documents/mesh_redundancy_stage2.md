# Design: Track B Stage 2 — N=2 redundant inbound queues per leg

Status: **EXECUTION-READY DESIGN 2026-07-23.** Builds on Stage 0 (`rcv_server`)
and Stage 1 (multi-server `SmpTransport` routing), both landed + audited clean.
Read `documents/mesh_reliability.md` (Track B) and the CLAUDE.md transport
section first. N=2 chosen (the SimpleX sweet spot).

## 0. Goal & the one correctness detail

Today every directed peer-pair leg is **one inbound queue on one server**. One
server hiccup ⇒ a dead leg (healed only by a rotate). SimpleX-level redundancy:
each recipient creates **N inbound queues on N different servers**; every sender
sends the SAME ciphertext to all N; the receiver subscribes all N and **dedups**.
Losing one server/queue leaves the leg alive with zero heal latency.

**The load-bearing correctness detail:** the N receive paths of ONE peer MUST
share **one `Reassembler` and one delivery cursor** (the single writer of
`inbound[peer]`). If each of the N queues got its own reassembler/cursor, the
`(msg id, chunk index)` dedup would not cross them and the two cursors for one
peer would collide — duplicate delivery and cursor thrash. So: **N subscriptions
→ one merged channel → one `recv_task`.**

## 1. Data model — wrap keys stay scalar, only ADDRESSES vectorize

The wrap key is a **pairwise, per-direction** symmetric key. The N redundant
queues are N *delivery paths* for the SAME encrypted stream, so they share the
one direction key. Only the queue *addresses* become plural. This keeps the
change small and the crypto unchanged.

### 1.1 `PeerLink` (runtime, `supervisor.rs`) — free to change (never persisted directly)

```rust
pub struct PeerLink {
    pub member: MemberId,
    pub snds: Vec<SndQueueAddr>,   // the peer's N inbound queues — send the SAME ciphertext to all
    pub wrap_out: WrapKey,         // ONE key, me→peer  (unchanged)
    pub rcvs: Vec<RcvQueue>,       // my N inbound queues — subscribe all
    pub wrap_in: WrapKey,          // ONE key, peer→me  (unchanged)
}
```

Invariant: `snds`/`rcvs` are non-empty (≥1). N=1 == today's behaviour exactly.

### 1.2 `MeshLink` (persisted, `molt-core`) — ADDITIVE primary+extra, no version bump

`MeshLink` is transport bookkeeping — **not** chained or signed (audit-confirmed:
`roster_canonical_bytes` hashes none of it), so no signature breaks. Keep the
existing scalar fields as the **primary (index 0)** queue and ADD the extra
queues as `#[serde(default)]` vectors:

```rust
// existing scalars = queue[0] (unchanged): snd_server, snd_queue, snd_wrap,
//                                           rcv_queue, rcv_wrap, rcv_server
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub snd_extra: Vec<QueueRef>,   // queues[1..] I send to
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub rcv_extra: Vec<QueueRef>,   // queues[1..] I receive on
// QueueRef { server: String, queue: String }  — wrap is shared (snd_wrap/rcv_wrap)
```

- Old `transport.state` (no extra) → 1 queue → single-server resume, **exactly as
  before**. A downgraded binary ignores the unknown extra fields → 1 queue →
  functional (no redundancy). Fully additive; `TRANSPORT_STATE_VERSION` unchanged.
- `PeerLink::to_mesh`: primary = `snds[0]`/`rcvs[0]`, extra = the rest.
- `PeerLink::from_mesh`: `snds = [primary] ++ snd_extra`, `rcvs = [primary] ++ rcv_extra`.

### 1.3 `MeshAnnounce` / `QueueHandover` (in-band, `mesh.rs`) — ADDITIVE

`MeshAnnounce.queues: BTreeMap<MemberId, QueueHandover>` stays (the primary queue
per peer). ADD:

```rust
#[serde(default)]
pub queues_extra: BTreeMap<MemberId, Vec<QueueHandover>>,  // peer → queues[1..]
```

An old announcer sends only `queues` (1 each) → an updated receiver assembles a
1-queue leg toward it (no redundancy that direction, still works). An old
receiver ignores `queues_extra` → 1-queue leg. Additive both ways.
`QueueHandover` already carries `{server, queue, wrap}` — here each announced
queue carries its own wrap (they *are* the same per-direction key, so all N of a
peer's announced queues repeat the one wrap; the receiver uses `wrap_out` from
`queues[me]` and treats extras as address-only, ignoring their repeated wrap).

## 2. Concurrency — the supervisor (`supervisor.rs`)

### 2.1 Receive: N forwarders → one merged channel → one `recv_task`

Today: `recv_watchdog_task` per peer runs `recv_task` over ONE subscription;
`recv_task` owns the `Reassembler`/reorder/epoch buffers and is the sole cursor
writer; Stage-B resubscribe lives in the watchdog.

New per peer:
- Spawn **N forwarder tasks** (one per `rcvs[k]`). A forwarder owns the
  subscribe + Stage-B resubscribe/backoff lifecycle for its one queue and pumps
  every `Delivery` into a shared `mpsc` (`merged_tx`). It carries the AckToken
  through untouched (the `recv_task` still owns ack discipline).
- Spawn **one `recv_task`** reading `merged_rx` → the single shared
  `Reassembler` + cursor + reorder + epoch buffers, unchanged internally. The
  reassembler dedups the N-fold duplication by `(id, index)` for free.

Per-leg health with redundancy (a leg is UP if ≥1 of its N queues is live):
- Shared `live: Arc<AtomicUsize>` per peer. A forwarder that subscribes OK does
  `fetch_add(1)`; on the 0→1 transition it calls `sink.link_up(peer)`. On stream
  end it `fetch_sub(1)`; on the 1→0 transition it calls
  `sink.link_down(peer, reason)`, then backs off and resubscribes.
- The `recv_task` no longer resubscribes (that moved to the forwarders); it runs
  until `merged_rx` closes (all forwarders gone = engine gone). Its
  `link_up`/`link_down` calls move OUT to the forwarders.
- Track D `raw_inbound` stays in the `recv_task` (fires after a successful
  `unwrap_block` — one shared throttle across the N sources, honest: any queue
  delivering = leg receiving).

Result: a queue dying (server hiccup) drops `live` N→N-1; while ≥1 stays up the
leg never reports down and messages keep flowing on the surviving queue(s). The
dead queue's forwarder resubscribes in the background (Stage B). Zero heal
latency for a single-server outage — the redundancy IS the heal.

### 2.2 Send: fan the ONE ciphertext to all N (`outbox`)

Today: `transport.send(&peer.snd, block.clone())`. New: for the block of wire
seq S, send to **every** `peer.snds` target, reusing the one `block`
(ciphertext) — NEVER re-encrypt (that would burn N sender generations / risk
nonce reuse; the ciphertext is one logical message, the peer dedups the copies).

- Success rule: seq S is "sent" if **≥1** target accepts (redundancy — one
  server down, another delivers). Advance `wire_seq` once.
- Retry: if **all** N fail, retry the whole seq on the existing capped backoff.
  (Per-target retry refinement is a later optimisation; all-or-≥1 is correct and
  simple.) The peer's reassembler dedups any target that later also succeeds.

### 2.3 Prebuild

`prebuild_circuits` warms cold (Tor) circuits at open. Extend it to prebuild all
Σ(N over peers) subscriptions, still bounded by `PREBUILD_PARALLELISM`. Each
forwarder is seeded with its prebuilt first subscription where available.

## 3. Minting N — bootstrap / rotate / extend / recovery

Every site that today creates ONE inbound queue per peer creates **N** (config
`redundancy`, default 2 when a server list is present, else 1):

- **Bootstrap** (`mesh.rs` bootstrap / the founding + join paths): mint N inbound
  queues per peer with the ONE fresh per-direction wrap key; announce all N in
  `queues` + `queues_extra`. `create_queue`'s Stage-1 round-robin spreads the N
  across the transport's servers automatically.
- **Rotate / mesh-extension / recovery** (`net.rs` `cmd_net_mesh_*`,
  `lifecycles.rs`): mint N; the rotate machinery (Track C reuses this) replaces
  one queue at a time.
- **Loopback `full_mesh`**: mint N per pair (all on the loopback hub — server
  string empty — so redundancy LOGIC is exercised over loopback even though the
  "servers" collapse to one hub; the dedup/merge/fan paths are what the tests
  cover).

## 4. Config — a server LIST (`molt-config`)

`[transport.smp]` gains an optional `servers` list (or repeatable `smp_url`);
`redundancy` (default = min(N_configured, 2)). One server configured ⇒ N=1 ⇒
behaviour-neutral. `reopen_transport` and the ritual transport builder construct
a **multi-server** `SmpTransport` (`SmpTransport::new_multi`) from the list — the
per-server construction deferred from Stage 1 lands here.

## 5. Staging (each independently landable, green, pushed)

- **Stage 2a ✅ (commit 375fd57) — Vec plumbing, N=1 (behaviour-neutral).**
  `PeerLink.snds/rcvs`, `MeshLink` additive primary+extra, `MeshAnnounce.
  queues_extra`, `assemble_mesh`, the send-fan — all wired; every mint site mints
  1 (Vec len 1). NO `TRANSPORT_STATE_VERSION` bump (additive). Every existing
  test unchanged; whole workspace incl. molt-ui-window compiles.
- **Stage 2b — activate N=2 (landed in pieces):**
  - ✅ **recv restructure (commit c4a7805):** N forwarder tasks → one merged
    channel → one `recv_consumer_task` (single Reassembler + sole-writer cursor);
    shared `live` AtomicUsize aggregates per-peer link_up/link_down (leg up iff
    ≥1 queue live). Behaviour-neutral at N=1.
  - ✅ **redundancy TDD (commit 5d92939):** `LoopbackHub::full_mesh_n` +
    `a_two_queue_leg_survives_one_expired_queue` (one queue idle-expired, the
    message still arrives once via the other) + `redundant_copies_dedup_to_one_
    delivery` (N copies → one delivery). Proves the load-bearing dedup detail.
  - ✅ **transport-driven activation (commit 9cff628):** `Transport::redundancy()`
    (default 1), `SmpTransport::redundancy() = servers.len().clamp(1,
    MESH_REDUNDANCY_CAP=2)`; `bootstrap_mesh` mints that many. Redundancy turns on
    automatically once the transport is built with 2 servers. Still N=1 in the
    current build (single-server config).
  - ✅ **security audit** of the Stage 2 change-set — three findings, all fixed
    (2292c32 #1 fan-out amplification cap + #3 empty-servers guard, 88ca87c #2
    link_up/down ordering under a per-peer live lock). See §7.
  - ✅ **activation via a config server list** (the user's chosen route, e7314dd):
    `[transport.smp].urls` (additive) → `SessionSettings::smp_server_list()` →
    `build_smp_transport`/`with_dialer_multi`. The founder runtime transport, the
    joiner (`ritual_join_over_smp` with the invite server prepended), and
    `reopen_transport` (gathers all mesh servers) all build multi-server. N=2
    turns on when ≥2 servers are configured; N=1 (single-server) is unchanged.
    **Constraint:** N=2 requires members to SHARE a server set (`route()` matches
    a queue's server against the local list, falls back to the primary otherwise;
    the ≥1-success send rule keeps it delivering, just non-redundant on a leg to
    an unshared server). Dialing an arbitrary pinned announced server (to lift the
    constraint) is a follow-up.
  - ✅ **GUI server-list editor** — a "one server per line" multi-line editor in
    the SMP settings group (`SmpServerGroup.smp-urls` ↔ `cfg-smp-urls`),
    round-tripped through `read_settings_draft`/push; the interim engine
    preserve-hack removed (the GUI now round-trips the list, so full-replace is
    the consistent model). Richer per-row add/remove/Test-each is a follow-up.
  - ⬜ **REMAINING follow-ups (not blocking):**
    1. **mesh-extension / recovery mint N** (currently N=1): a member who
       joins-later/recovers gets single-queue legs until a rotation. Correct, just
       asymmetric redundancy.
    2. **Dial an arbitrary pinned announced server** so N=2 works across members
       with DIFFERENT server sets (lift the shared-set constraint) — a `route()`
       change (parse+dial the queue's own fingerprint-pinned server) with its own
       small audit.

## 6. TDD plan (loopback — the redundancy LOGIC is server-agnostic)

Loopback collapses all "servers" to one hub, but the N-queue LOGIC (mint N,
announce N, subscribe N → merge → dedup, fan send → N) is fully exercised. The
`LoopbackHub::expire_queue` seam models a dead queue: a 2-queue leg with one
expired must still deliver (the core redundancy assertion). Multi-*server*
routing itself is unit-tested in Stage 1 (`route_dispatches_...`); real
multi-server delivery is an `#[ignore]` SMP test.

Red-first tests to write (Stage 2b):
1. `a_two_queue_leg_survives_one_expired_queue` — mint 2, expire 1, a message
   still arrives exactly once.
2. `n_copies_dedup_to_one_delivery` — the same ciphertext sent to 2 queues is
   delivered ONCE to the engine (shared reassembler).
3. `a_leg_reports_down_only_when_all_queues_die` — `live` aggregation.
4. `send_fans_to_all_targets_and_one_success_advances` — outbox fan-out.

## 7. Security notes (carry the audit's Stage 0/1 clearance forward)

- The ONE ciphertext reused across N sends preserves MLS nonce discipline
  (no re-encrypt). The peer dedups copies; MLS `reuse_guard` already guards the
  per-message path. **(Audit-confirmed: `ciphertext_for` memoizes per seq;
  `send_one` wraps once per chunk and clones to each target.)**
- Wrap keys unchanged (per-direction). No key is exposed N times beyond the
  handover it already rode in.
- `net_health` stays honest: a leg is up iff ≥1 queue live; all-N-down is a real
  `link_down`. Redundancy makes the honest state *better*, not louder.
  **(Audit finding #2: the naive AtomicUsize aggregation could reorder the
  up/down notifications at the engine and get STUCK alarming a live leg — FIXED
  by binding each transition to its notification under a per-peer `tokio::Mutex`,
  commit 88ca87c.)**

### 7.1 CORRECTION — the "only harms its own reachability" claim was WRONG

An earlier draft claimed "a malicious member announcing N attacker-server queues
only harms its own reachability." The Stage-2 security audit proved this **false**
for the send/extra path: `MeshAnnounce.queues_extra[victim]` names queues the
VICTIM will send to, so an oversized/attacker-chosen list makes the *victim* fan
every outbound block to attacker endpoints (egress amplification), and it was
uncapped on the ingest side even at N=1. **FIXED (commit 2292c32):**
`assemble_mesh` and `PeerLink::from_mesh` cap the fan-out at
`MESH_REDUNDANCY_CAP`. An empty `servers` list also panicked
(`with_dialer_multi`); fixed with a fail-closed placeholder. All three Stage-2
findings (#1 amplification, #2 aggregation honesty, #3 empty-servers) are closed;
the rest of the change-set audited clean (no MLS nonce reuse, dedup sound,
shutdown clean, additive schema, no signature break, routing sound).

## 8. Open forks (confirm with the user)

- **N× traffic / server load** accepted for N=2 (the user chose N=2 explicitly).
- **Config surface**: `servers` list vs repeatable `smp_url` — pick the smaller
  diff against the current `molt-config` shape.
- **Per-target send retry** deferred (all-or-≥1 is the Stage-2b contract).
