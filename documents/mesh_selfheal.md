# Design: mesh self-heal — queue liveness, honest health, auto-heal, recovery fallback

Status: **PLANNED 2026-07-19** — design agreed in discussion; not yet built.
Execution-ready, test-first. Read `documents/dynamic_mesh.md`,
`documents/recovery_ritual.md`, `documents/stage_b.md`, and the CLAUDE.md
transport section first — this builds directly on their machinery.

## 0. The problem this fixes (grounded in a live incident)

**2026-07-19, workspace "Test7", 3 nodes over the public SMP server
`smp8.simplex.im`.** After each node was restarted and reopened the workspace,
**2 of 3 nodes went silently deaf**: they received no messages at all, while the
third received everything. Reproduced deterministically and root-caused live
(delivery matrix over MCP + per-queue `MESHDIAG` logging of both queue faces):

- **Single server** (all mesh links → `smp8.simplex.im`) — so it is *not* a
  multi-server split.
- **Pairing is perfect on every leg** — each sender sends to exactly the
  recipient-face the deaf node subscribes to. So it is *not* a pairing bug.
- **Sends succeed** (server `OK`; keys derived from `sender_seed`) and
  **subscriptions succeed** (`SUB` → `OK`). No errors anywhere.
- Yet only the most-active node still received.

The only state consistent with *single server + perfect pairing + `SUB`/`SEND`
both `OK` + no delivery* is: **the SMP server expired/dropped the idle inbound
queues of the two less-active nodes** (the active node's queues stayed warm).
The client **cannot detect this** — `SUB` and `SEND` both return `OK` on the
half-dead queues, the subscription never `END`s — so `net_health` stays `Ok`,
the Stage-B resubscribe watchdog never fires, and the node is **permanently,
silently deaf**.

### Why this is not the read-receipt feature and not a quick patch

Read receipts merely *revealed* the broken mesh (a deaf node's `read_by` only
ever holds itself). The fault is the transport's inability to (a) keep queues
alive, (b) notice a dead-but-`OK` inbound leg, or (c) re-establish it. A plain
resume faithfully re-adopts the dead queues.

### Reference: how SimpleX survives this

A SimpleX "group" is **not** a server construct — it is a set of **pairwise
connections** (each member ↔ every other member), exactly like our per-pair
mesh. Their robustness (per the SimpleX protocol docs) is unspectacular:
**generous server expiry windows, redundancy across multiple servers/queues,
and keeping connections warm with activity.** Automatic queue rotation is on
their *roadmap*, not shipped. And when a queue does expire while a client is
offline, **SimpleX connections break too** and must be re-established. So the
right shape is two-tier: **prevent** (keep queues warm / rotate) as the main
line, **heal + honest fallback** as the safety net.

## 1. Invariants this design must preserve

- **Ephemeral chat, no backfill** (`chat_bus.md` Q4). A reconnected member has a
  gap; the UI must say so, never pretend it caught up.
- **Serverless + threshold governance.** A partitioned member cannot re-admit
  itself; re-establishment needs **m-of-n** peers to sign a `Restored` block
  (`recovery_ritual.md`). No lone re-inject — a security property, and a UX
  constraint to communicate honestly.
- **Ratchet continuity.** Any mesh rebuild shares the **live group `Arc`**
  (`build_real_net_shared`, `net.rs:723`) — never snapshot→restore, or the MLS
  ratchet regresses.
- **Chain untouched.** Self-heal is a *transport-layer* concern. It never writes
  a chain block; keepalives are not even `WorkspaceEvent`s.
- **Reuse the one mesh-mutation path.** Do not fork a second mechanism —
  everything routes through the existing adopt path
  (`spawn_mesh_extension` → `cmd_net_mesh_extended`).
- **Beaconless posture is bent, consciously and minimally.** The transport
  deliberately avoids periodic per-member traffic (`concept-transport-simplex-tor.md`
  §3.4). Keepalive re-introduces *some* — but **idle-only, low-rate, MLS-encrypted
  mesh traffic** (not a per-member server beacon, not per-message). This is the
  explicit, bounded tradeoff that buys a mesh that does not silently die.

## 2. What already exists — the 90 %

The expensive machinery is **built and exercised** by the recovery / dynamic-mesh
path. Auto-heal is mostly *triggering* it from a new place.

| Building block | Status | Where |
|---|---|---|
| Queue handover wire type (per-peer) | EXISTS | `molt-net/src/mesh.rs:31-74` (`QueueHandover`, `MeshAnnounce`) |
| MLS-authenticated announce encode/decode | EXISTS | `bootstrap_over_mls` `mesh.rs:175`; `decrypt_group_message` `net.rs:480` |
| **Adopt a peer's announced queue over the LIVE mesh** | EXISTS | `WorkspaceEvent::MeshAnnounced` arm `net.rs:1039` → `spawn_mesh_extension` `net.rs:1390` |
| Create a fresh per-pair inbound queue | EXISTS | `SmpTransport::create_queue` `transport.rs:354`; used `net.rs:1431`, `recovery.rs:650` |
| Reply/announce directly onto a peer's queue | EXISTS | `send_framed` `net.rs:1460` |
| Replace a peer's link in the running mesh + rebuild | EXISTS | `cmd_net_mesh_extended` `net.rs:1499-1554` (replace-by-member, teardown, `build_real_net_shared`, persist `net.rs:1545`) |
| Persist grown/changed mesh (live, non-sealing) | EXISTS | `persist_mesh_crypto_blocking` `storage/src/lib.rs:1813` |
| Ratchet-continuous rebuild (share live group `Arc`) | EXISTS | `build_real_net_shared` `net.rs:723` |
| Full-partition heal (fresh queues from scratch) | EXISTS (needs out-of-band `molt://recover/…` + threshold `Restored`) | `cmd_recover_start` `lifecycles.rs:1215`; `rejoin_mesh` `recovery.rs:630`; `coordinator_rekey` `chain.rs:1259` |
| Per-peer inbound-liveness edge ("heard from X") | EXISTS **but health-blind** — feeds presence only | `peer_seen` `supervisor.rs:1044` → `cmd_net_peer_seen` `net.rs:1645` → `last_seen` `net.rs:1786` |
| `NetHealth` states | EXISTS | `Ok \| Degraded{reason} \| Down{reason}` `core/src/lib.rs:2199` |
| Recovery UI (both sides) | EXISTS | rejoiner + coordinator panels |

**MISSING (the 10 % to build):**

1. **A self-initiated announce trigger.** Today the *only* producer of a
   `MeshAnnounced` onto the live mesh is the recovery coordinator relay
   (`cmd_net_recover_announced` `net.rs:1378`), gated on `recovery_mesh_window`
   (armed only in `coordinator_rekey` `chain.rs:1316`). A node cannot, on its
   own, mint a fresh inbound queue and broadcast its new address over the still-
   live legs. (`dynamic_mesh.md:121` marks "queue rotation for existing members"
   out-of-scope.)
2. **A liveness signal beyond subscription-confirmed.** `recompute_net_health`
   (`net.rs:1729`) is built purely from `net_link_down` (subscribe failed/ended)
   and `net_send_stuck` (send backoff). The per-peer "heard from X since mesh-up"
   edge exists (`last_seen`) but is wired only to presence, never to health. A
   `SUB`-`OK`-but-never-delivers queue therefore reads as perfectly healthy.
3. **Keepalive** to keep idle queues warm on the server.

## 3. The mechanism, in four stages

Ordered so each stage is independently valuable and testable, and later stages
build on earlier ones. Detection first (ends the *silent* failure immediately),
then keepalive (prevents most failures and sharpens detection), then reactive
heal, then the UX fallback for the residue.

### Stage 1 — Detection: honest `net_health`

**Goal:** never be silently deaf. A live-but-non-delivering leg must surface.

- Track, per mesh peer, a **mesh-up timestamp** (when this peer's inbound
  subscription first went live this incarnation) — new runtime field alongside
  the existing `net_link_down` map. The watchdog already knows the moment
  (`link_up`, `supervisor.rs:825`).
- Extend `recompute_net_health` (`net.rs:1729`) to cross-check the **existing**
  `last_seen(peer)` (stamped by `peer_seen` on *any* authenticated inbound
  frame, `supervisor.rs:1044`): if a peer's subscription is live **and**
  `now - mesh_up(peer) > T_deaf` **and** we have received **nothing** from that
  peer since mesh-up (`last_seen(peer) < mesh_up(peer)`), report
  `Degraded { peer, "no inbound since mesh-up" }`.
- `T_deaf` is generous (order of minutes) to avoid flapping. This is honest even
  for a genuinely quiet/offline peer: "no contact with X" is *true* either way,
  and the Stage-3 heal attempt is safe regardless (see §3.3).
- The moment any authenticated frame arrives from the peer (chat, governance,
  a keepalive, or a re-announce), `last_seen` advances and the leg clears back
  to `Ok`.

**Why this is honest, not merely coarse:** presence already ages a silent peer
to "offline" on the pills; today `net_health` contradicts that by saying `Ok`.
Stage 1 removes the contradiction — health and presence agree.

**Tests (loopback):** a peer whose inbound frames are dropped (a test transport
hook, see §7) trips `Degraded{peer}` after `T_deaf`; a peer that keeps sending
stays `Ok`; the leg clears when a frame finally lands.

### Stage 2 — Prevention: idle queue keepalive (and optional rotation)

**Goal:** queues never idle-expire on the server, so the failure mostly stops
happening. This is the SimpleX-roadmap approach.

- A new low-rate ticker `Command::NetMeshKeepaliveTick` (engine-internal, like
  `NetPresenceTick` `lib.rs`), period `T_keepalive` chosen **well under the
  server's idle-expiry window** and **only fires per leg when that leg has been
  idle** (no real traffic) for close to `T_keepalive`. So an actively-chatting
  mesh sends **zero** extra frames; only idle legs get a ping.
- The keepalive is a **transport-level MLS frame**, *not* a `WorkspaceEvent`:
  it never enters the event log or chain (pure liveness, like presence is
  runtime-only). It rides the same per-pair queue + `MlsChannel` the mesh
  already uses. Because it is authenticated inbound traffic, the receiver's
  `peer_seen` fires automatically (`supervisor.rs:1044`) → `last_seen` advances
  → **it doubles as the Stage-1 liveness signal**: a quiet-but-alive peer now
  proves liveness, so "no inbound past `T_keepalive + margin`" becomes a
  *reliable* death/offline signal, not a false alarm.
- **Mutual keepalive keeps every queue warm:** my ping keeps the queue *I send
  to* (the peer's inbound) warm; the peer's ping keeps *my* inbound warm.
- **Optional rotation (unlinkability bonus, slower schedule):** on a much slower
  cadence a node may *rotate* rather than merely ping — mint a fresh inbound
  queue, `MeshAnnounced` it over the live mesh (Stage-3 producer), retire the
  old. Reuses the adopt path. Deferrable; keepalive alone fixes the delivery
  bug.

**The beacon tradeoff, stated plainly:** this adds periodic idle-only mesh
traffic — a bounded departure from strict beaconless (`concept-transport-simplex-tor.md`
§3.4). It is MLS-encrypted mesh traffic (not a per-member server beacon, not
per-message), fires only on idle legs, at a rate set by the expiry window (order
of hours, not seconds). The alternative is a mesh that silently dies — the
tradeoff is deliberate and recorded here.

**Tests:** an idle leg emits exactly one keepalive per `T_keepalive`; an active
leg emits none; the keepalive stamps the peer's `last_seen` on the receiver.

### Stage 3 — Reactive auto-heal: self-initiated re-announce (+ relay)

**Goal:** when a leg is already dead (Stage 1 flags it, or a keepalive round
never returns), re-establish it without a human re-invite where possible.

- New engine-internal command `Command::NetMeshRotate { peer }` (INTERNAL —
  the node's own transport speaking, like `NetMeshExtended`; **not** an MCP
  tool, so an agent cannot forge mesh churn). Triggered by Stage-1 detection
  (debounced) — or by the Stage-2 rotation schedule.
- Handler: mint a **fresh inbound queue** for that peer (`create_queue`
  `transport.rs:354`), then **record a self-authored `WorkspaceEvent::MeshAnnounced`**
  carrying the new sender-face — modeled on `cmd_net_recover_announced`
  (`net.rs:1378`) but gated by detection instead of `recovery_mesh_window`. The
  event `crosses_wire`, so it is broadcast over **every working outbound leg**.
- **Adopt is already built:** each peer that receives it runs the existing
  `net.rs:1039` → `spawn_mesh_extension` → `cmd_net_mesh_extended` — creates its
  own fresh queue, replaces the link, rebuilds ratchet-continuous, persists.
- **Single-node expiry (the common case) heals with no relay:** the deaf node's
  *outbound* to every peer still works (peers' inbounds are alive), so its
  re-announce reaches all peers directly → full heal.
- **Multi-node partition (the Test7 case) needs a relay.** When A and B are both
  deaf, A↔B is dead both directions; only the legs to a still-reachable hub work.
  Each deaf node first heals its leg to the hub (direct re-announce over the
  working outbound), then **the hub relays** re-announces between the nodes that
  cannot reach each other — the *same relay shape* the recovery coordinator
  already uses (`net.rs:1378` records a `MeshAnnounced` it received on behalf of
  another member). Generalize that: a node that adopts a peer's re-announce
  **re-broadcasts it** to roster members the announcer could not reach.
  **Loop prevention is mandatory:** each `MeshAnnounced` carries a random nonce
  (additive field, `#[serde(default)]`); a `seen`-set (runtime, bounded FIFO
  like `ParkedRefs`) drops a re-announce already relayed. Convergence requires
  only that the working-leg graph is connected (a hub suffices).
- **Security:** the announce is MLS-authenticated; the adopt path already
  enforces `announcer != me && roster.contains(announcer)` (`net.rs:1043`) and a
  per-member 60 s cooldown (`net.rs:1400`) bounds churn/DoS. A member can only
  re-point **its own** inbound address — it cannot redirect another member's
  traffic. Same trust envelope as recovery.

**Tests (loopback + real-SMP):** single-node dead leg self-heals (fresh queue +
re-announce + adopt, delivery resumes); a 3-node double-partition heals via the
hub relay; the relay nonce/seen-set prevents an announce storm; a re-announce to
a merely-offline peer is harmless and applies when it returns.

### Stage 4 — UX fallback: when auto-heal cannot fix it

Auto-heal cannot cross a *full* partition (no working leg to anyone) or a
too-few-peers-online recovery. Then the rule is **show it, offer one action.**

**The honesty ladder** — `net_health` → banner:

- **Green (`Ok`):** flowing; no chrome.
- **Yellow (`Degraded{peer, reason}`):** *"Reconnecting to {peer}…"* with a
  spinner while Stage-3 heal runs; clears silently on success.
- **Red (`Down{reason}`) / heal timed out:** *"Disconnected from {republic} —
  you're not sending or receiving."* + a **"Repair connection"** button.

Two things the user otherwise mis-reads:

- **Outbound is buffered, not lost:** `net_send_stuck` already holds unsent
  frames. Show *"N messages waiting — will send when reconnected."*
- **The ephemeral gap:** chat has no backfill. After reconnect, say *"messages
  sent while you were disconnected won't appear"* — never imply a silent catch-up.

**"Repair connection"** routes into the **existing recovery rejoin**:

- **Best case:** a peer is reachable over *some* path → the app re-establishes
  automatically (fresh queue + Stage-3 / recovery); the click is the only step.
- **Human step when no queue reaches anyone:** guide the user — *"Ask a member
  who's online to send you a reconnection link"* → that member generates a
  `molt://recover/…` link (the coordinator recovery-invite UI already exists) →
  the user pastes it → fresh queues, back in. Like re-adding a contact.

**The honest hard limit (the security model, not a bug):** re-admission needs
**m-of-n** members online to sign the `Restored` block. The UI must say so
instead of spinning forever: *"Waiting for {m} members to approve reconnection"*
and, if too few are reachable, *"Reconnection isn't possible until enough
members are online."*

**Validation:** engine-level `net_health` transitions drive the banner; the
repair button reuses the recovery flow; no GUI on `DISPLAY=:0` — Slint compiler
+ engine tests.

## 4. New wire / state / commands (all additive)

- **Keepalive frame** — a transport-level MLS ping, *not* a `WorkspaceEvent`
  (runtime liveness only; never logged/chained). A tiny authenticated frame over
  the existing per-pair queue; the receiver's `peer_seen` stamps `last_seen`.
- **`MeshAnnounced` gains an optional `nonce`** (`#[serde(default)]`, additive)
  for relay loop-prevention. Existing single-hop adopt ignores it.
- **Runtime state (session, never persisted/chained):** `mesh_up(peer)`
  timestamp; a bounded `seen` FIFO of relayed announce nonces (mirror
  `net::ParkedRefs`).
- **New engine-internal commands (INTERNAL list in `molt-mcp/src/lib.rs`,
  never MCP tools):** `NetMeshKeepaliveTick`, `NetMeshRotate { peer }`. Update
  the co-equality test's `INTERNAL` array + count. (Human "repair" is **not** a
  new command — it reuses the existing recovery commands, which are already
  tools/mapped.)
- **`recompute_net_health`** grows the `last_seen`/`mesh_up` cross-check.
- **No chain change, no `roster_canonical_bytes` / `molt-chain-*` bump** — none
  of this is chained.

## 5. Implementation plan (phased, test-first)

Each phase lands green on master with its own tests before the next. Real-SMP
tests are `#[ignore]`d (run `-- --ignored`); the loopback + `mesh_restart_over_smp.rs`
harness is the vehicle.

- **Phase 1 — Detection (honest health).** `mesh_up` per-peer timestamp;
  `recompute_net_health` cross-checks `last_seen`; `Degraded{peer}` on
  "subscription live + nothing since mesh-up > `T_deaf`". Tests: red first
  (a dropped-inbound leg stays `Ok` today → must go `Degraded`). Files:
  `molt-engine/src/net.rs` (`recompute_net_health`, watchdog `link_up` site),
  `molt-core` (`NetHealth` reason strings if needed).
- **Phase 2 — Keepalive.** `NetMeshKeepaliveTick` ticker + per-leg idle gate;
  transport-level MLS ping; receiver `peer_seen` path already stamps
  `last_seen`. Tests: idle leg pings once/period, active leg silent, ping stamps
  liveness, Phase-1 detection now distinguishes dead vs quiet. Files:
  `molt-engine/src/{lib.rs,net.rs}`, `molt-net/src/{supervisor.rs,smp/*}` (ping
  frame), spawn ticker.
- **Phase 3 — Reactive heal + relay.** `NetMeshRotate{peer}` (mint queue +
  self-`MeshAnnounced`); `MeshAnnounced.nonce` + relay re-broadcast with
  seen-set; wire detection → rotate (debounced). Reuses `spawn_mesh_extension` /
  `cmd_net_mesh_extended` unchanged. Tests: single-node self-heal; double-
  partition hub-relay heal; loop-prevention; harmless re-announce to offline
  peer. Files: `molt-engine/src/net.rs`, `molt-core` (`MeshAnnounced` field,
  `Command` variants), `molt-mcp` (INTERNAL list + co-equality test).
- **Phase 4 — UX fallback.** Banner from `net_health` (green/yellow/red),
  "Repair connection" → recovery rejoin, pending-count + ephemeral-gap +
  threshold copy. Files: `molt-ui/src/lib.rs`, `molt-ui-window/ui/*.slint`
  (banner + repair affordance + `Strings` en/de). Validation:
  `cargo build -p molt-ui-window -p molt-ui`.
- **Phase 5 — Land.** `/code-review` each phase's diff; `cargo clippy
  --all-targets` = 0; green on master; the `#[ignore]`d real-SMP heal test
  passes `-- --ignored`.

## 6. Test strategy — simulating a dead queue

Loopback is *permissive* (shared hub, no expiry), so it cannot reproduce the
server-expiry deafness on its own. Add a **test transport hook** that marks a
named queue "expired" — `SUB`/`SEND` still return `Ok` but deliveries to it are
silently dropped (exactly the observed server behavior). With that hook:

- Phase 1: expire a leg → `net_health` must go `Degraded{peer}`.
- Phase 3: expire a leg → auto-heal mints a fresh queue, re-announces, delivery
  resumes; assert convergence of a two-instance (and three-instance partition)
  loopback.

The real-SMP proof extends `crates/molt-engine/tests/mesh_restart_over_smp.rs`
(the 3-node restart matrix) with an idle-then-resume case once `T_keepalive` is
tunable low for tests.

## 7. Non-goals, limits, related follow-ups

- **Multi-server redundancy is a separate fix.** `MeshLink` persists `snd_server`
  but **no server for the node's own inbound queue** (`PeerLink.rcv` has no
  `server`; `reopen_transport` collapses to `mesh[0].snd_server`,
  `founding.rs:49`). A genuinely multi-server mesh would mis-subscribe on
  resume. Not the cause of the Test7 incident (single server) but a real latent
  bug — track separately (add `rcv_server` to `MeshLink`/`RcvQueue`; per-queue
  server in `subscribe`/`send`). Redundancy across servers (SimpleX-style) is a
  larger, later item.
- **No chat backfill** — permanent (`chat_bus.md` Q4). Reconnect ≠ catch-up.
- **Threshold recovery** — a partitioned member needs m-of-n peers online;
  unavoidable in a serverless threshold system; communicated, not "fixed".
- **The server idle-expiry window is unknown** for public servers — `T_keepalive`
  must be conservative; make it configurable, default well under known SimpleX
  windows.
- **Rotation for unlinkability** (Stage 2 optional) is deferred; keepalive alone
  closes the delivery bug.

## 8. Decision log

- 2026-07-19 — root cause established live (server idle-expiry of inbound
  queues; undetectable because `SUB`/`SEND` both `OK`). Not read receipts, not
  multi-server, not pairing.
- 2026-07-19 — approach agreed: two-tier (prevent via keepalive/rotation; heal +
  honest fallback), reusing the existing adopt path; the missing pieces are a
  self-initiated announce trigger, a liveness-fed `net_health`, and keepalive.
- 2026-07-19 — beaconless posture bent consciously to a bounded idle-only
  keepalive; recorded as the deliberate tradeoff.
