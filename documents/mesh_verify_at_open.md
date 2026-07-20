# Design: mesh verify-at-open — proactive leg verification (self-heal Fix A)

Status: **DISCUSSION DRAFT 2026-07-21** — design proposed, open questions in §7
to settle before any code. Builds directly on `documents/mesh_selfheal.md` (all
four phases landed on master, 43ab06d..ff8923c). Read that + the CLAUDE.md
transport section first; this reuses its machinery and only front-loads it.

## 0. The incident (grounded, 2026-07-21)

Three nodes (c1/c2/c3) over the public SMP server. **All clients closed, idle a
few minutes, reopened together.** Symptom: **c3 did not receive from c2** while
c1 kept receiving (c1 = hub). It *did* heal — but only after a visibly deaf
window (order of a minute), and the connection banner **flapped green during the
deafness**. Not a full partition (a hub survived); the reactive self-heal worked,
but **too slowly and dishonestly at open**. User verdict: "super unzuverlässig."

### Why the existing self-heal underperforms *at open*

The Stage-1..3 machinery (`mesh_selfheal.md`) is tuned for **mid-session** death
(a leg that ran, then idle-expired): generous 300 s deaf window, keepalive-warmed,
30 s detection beat. **Open (cold resume / founding) is a different regime** — a
leg can be **born dead**: its SMP inbound queue idle-expired while the node was
offline, and `SUB`/`SEND` both still return `Ok`. Today, at open:

- **`net_health` reads a false `Ok` for the first `MESH_DEAF_NEW_SECS = 45 s`**
  (`net.rs:70`). `cmd_net_link_up` (`net.rs:2037`) stamps `mesh_up` and calls the
  one-way `warm_leg`, but `deaf_legs` (`net.rs:2101`) only flags the leg once the
  45 s born-dead window is crossed — so `recompute_net_health` (`net.rs:2139`)
  says `Ok` while the node is already deaf. **This is the green flap.**
- **Detection runs only on the 30 s presence tick** (`PRESENCE_TICK_MS`,
  `lib.rs:74` → `cmd_net_presence_tick` → `rotate_deaf_legs`, `net.rs:1894`). So a
  born-dead leg is first *noticed* at **45–75 s**, then `cmd_net_mesh_rotate`
  (`net.rs:1681`) fires with up to `MESH_ROTATE_REPLY_TIMEOUT = 20 s`
  (`net.rs:98`) for the adopter's reply ⇒ **~1–1.5 min visibly deaf**.
- **`warm_leg` (`net.rs:2296`) is fire-and-forget** (one-way keepalive): a healthy
  leg proves itself only when the *peer's independent* `warm_leg` reciprocates; a
  dead leg is inferred purely from silence, never actively probed.

The keepalive (Stage 2) prevents *mid-session* idle-expiry but **cannot cover the
offline gap** — while all clients are closed, nothing keeps the queues warm. So a
cold reopen is precisely where born-dead legs cluster, and the whole worst case
lands **at the moment the user opens the app**. That is what reads as unreliable.

## 1. Goal & non-goals

**Goal:** at open (resume) **and** at founding mesh-up, *actively verify every
leg round-trips within seconds* and *proactively re-establish any that does not*
— before the user relies on the mesh — with **`net_health` honest from t=0** (no
green flap; amber until a leg is actually heard).

**Non-goals (unchanged, out of scope here):**

- **Mid-session death** — already covered by keepalive + the 300 s window. This
  design only front-loads the *open* regime; it does not touch the mid-session
  cadence.
- **The full-partition hard limit** — all queues dead, no peer reachable over any
  leg → still the Stage-4 recovery / m-of-n path (`mesh_selfheal.md` §3.4). Not
  wished away here; only made faster/honest when a hub survives (the incident's
  actual shape).
- **Multi-server redundancy** — the latent `rcv_server` bug (`mesh_selfheal.md`
  §7) is separate.

## 2. Invariants preserved

Same load-bearing list as `mesh_selfheal.md` §1 — none weakened:

- **Ratchet continuity** — every rebuild shares the live group `Arc`
  (`build_real_net_shared`, `net.rs:812`); the rotate path already does. No
  snapshot→restore.
- **Reuse the one mesh-mutation path** — verification triggers the *existing*
  `cmd_net_mesh_rotate` → `NetMeshReAnnounce` → adopt → `NetMeshExtended`. No
  second mechanism.
- **Probes are MLS-authenticated** — encrypted on the shared group, so
  unforgeable; same trust envelope as keepalive/rotate. A node can only re-point
  **its own** inbound.
- **Chain untouched, ephemeral** — probes are transport-level frames, never
  `WorkspaceEvent`s, never blocks.
- **New:** **open is never blocked.** The workspace opens immediately; verify runs
  underneath with an honest amber banner. We do *not* add a "wait until meshed"
  gate to the ritual or to open (that risks a hang on one stubborn leg).

## 3. What already exists (the 95 %) and the 5 % to add

| Building block | Status | Where |
|---|---|---|
| One-way warm on link-up | EXISTS (fire-and-forget) | `warm_leg` `net.rs:2296`; `spawn_keepalive` `net.rs:2252` |
| Mint fresh queue + re-announce + relay + fold-in | EXISTS | `cmd_net_mesh_rotate` `net.rs:1681`; `cmd_net_re_announce` `net.rs:1867`; adopt `cmd_net_mesh_extended` `net.rs:1615` |
| Live-but-deaf cross-check | EXISTS (45 s / 300 s two-tier) | `deaf_legs` `net.rs:2101` |
| Honest `net_health` (deaf + reconnecting) | EXISTS | `recompute_net_health` `net.rs:2139` |
| Per-leg mesh-up stamp | EXISTS | `cmd_net_link_up` `net.rs:2037` |
| Keepalive frame + decode | EXISTS | `MESH_KEEPALIVE_TAG` `molt-net/src/lib.rs:77`; `MlsDecode::Keepalive` `supervisor.rs:133` |
| Link-up sink (per-leg, both build paths) | EXISTS | trait `supervisor.rs:231`; fired in the resubscribe watchdog `supervisor.rs:844` |
| Dead-queue test seam (SUB/SEND Ok, 0 delivery) | EXISTS | `LoopbackHub::expire_queue` (ce5baa6, `loopback.rs`) |

**MISSING (the 5 %):**

1. **An honest "verified" gate.** A leg is *verified* only once it has delivered
   ≥ 1 authenticated inbound frame **this incarnation** (`member_last_seen(m) ≥
   mesh_up(m)`). `net_health` must read `Degraded` for a never-heard leg **from
   t=0**, not `Ok`-for-45 s. No new field — derived from the existing
   `last_seen`/`mesh_up` stamps.
2. **A fast, targeted open-verify beat** so detection latency at open is ~seconds,
   not 30 s-granular: a per-leg one-shot armed at `link_up`, not the 30 s presence
   tick.
3. *(Optional, see §7 Q3)* **A solicited round-trip probe** — a keepalive variant
   that asks the receiver to warm back immediately, so a single node can
   *deterministically* confirm a leg round-trips without depending on the peer's
   independent warm timing.

## 4. Mechanism

### 4.1 Honest-from-t=0 `net_health` (Phase 1 — the flap killer)

`recompute_net_health` today only escalates a never-heard leg to `Degraded` once
it crosses the 45 s born-dead window. Change: a leg that is **linked-up but
unverified** (`last_seen < mesh_up`) reads `Degraded { "verifying {peer}" }`
**immediately**. So a leg's honest lifecycle at open becomes:

```
link_up ──► amber "verifying {peer}"  ──(heard)──► green Ok
                    │
                    └─(T_verify elapsed, still unheard)─► amber "reconnecting {peer}" + rotate
```

No green until the leg is *actually heard*. This one change removes the flap even
before any faster healing lands — which is why it is Phase 1 and independently
valuable.

### 4.2 Verify-on-link_up + fast open-rotate (Phase 3 — the speed)

On **every** `link_up` (so it covers cold-open, founding mesh-up, *and* Stage-B
mid-session resubscribes — strictly better than only "at open"):

1. **Probe the leg** — send a **solicited probe** (§4.4, decided) so the peer
   warms back at once; retried once mid-window (t=0 and t≈`T_verify`/2).
2. **Arm a one-shot `NetMeshVerify { peer, generation }`** to fire at `T_verify`
   (a few seconds). Modeled on the rotate's `tokio::spawn` + `cmd_tx.send`
   pattern (`net.rs:1716`): sleep `T_verify`, then send the command to the actor.
3. **Handler:** if the leg is still unverified (`member_last_seen(peer) <
   mesh_up(peer)`) → `cmd_net_mesh_rotate(peer)` (mint fresh queue + re-announce
   over the surviving hub legs + adopt + fold-in — the existing path). If it *is*
   verified, no-op. The 60 s rotate cooldown (`net.rs:1691`) still bounds churn;
   at open `rotate_at` is empty so the first rotate is never blocked.

This replaces the **45–75 s** born-dead detection latency at open with **~T_verify
(seconds)**, without touching the mid-session 300 s window (that stays governed by
`deaf_legs`, unchanged). A false positive (a merely-slow-server leg) is harmless —
the rotate's off-actor task GCs its unused fresh queue on reply-timeout
(`net.rs:1788`).

### 4.3 Founding parity (Phase 4 — resolves ff8923c's "next step")

`cmd_net_mesh_ready` (`founding.rs:1750`) stands the supervisor up via
`build_real_net`; every leg that subscribes fires the **same** `link_up` sink
(`supervisor.rs:844`) as resume's `build_real_net_shared`. So if the verify hook
lives on `link_up`, **founding gets it for free** — a leg born dead at founding
(the Test10 case ff8923c named) is probed and, if unheard within `T_verify`,
rotated in seconds, instead of sitting yellow for the full deaf window. **No
separate blocking founding handshake is needed** — which is exactly the
non-goal-blocking posture (§1). *(Impl check: confirm `build_real_net` wires the
same `link_up` sink as `build_real_net_shared`.)*

### 4.4 The solicited probe frame (Phase 2 — decided, §7 Q3)

Reserve a control tag `MESH_PROBE_TAG =
b"\x00molt-mesh-probe-v1"` in the existing `\x00molt-mesh-*` reserved namespace
(additive, mirrors `MESH_KEEPALIVE_TAG`), decoding to `MlsDecode::Probe`
(`supervisor.rs:162`). On receipt the supervisor (a) stamps the sender's
`last_seen` via the existing `peer_seen` path (proves *its* leg to me is alive),
**and** (b) fires **one** warm-back keepalive to that peer (so the prober hears a
frame → verifies its leg). The warm-back is a plain keepalive, **never another
probe** — no echo storm. An old peer (pre-probe) receiving an unknown `\x00`-tag
must **drop it as a no-op**, never mis-parse it as an application frame *(impl
check on the decode: the `\x00`-leading control namespace is distinct from a
Reassembler chunk, so this is a guard, not a parse ambiguity)*.

### 4.5 Timing (decided, §7 Q1)

| Knob | Value | Rationale |
|---|---|---|
| `T_verify` (probe deadline / one-shot) | **10 s** | 1 RTT over SMP is sub-second; 10 s is conservative — tolerates a slow round-trip and minimises needless rotates on a sluggish server, still heals a dead leg in ~10 s vs ~45–75 s today |
| probe retries before rotate | **1** (t=0 + t≈5 s) | one mid-window retry absorbs a single lost frame without delaying the rotate materially |
| rotate reply timeout | **20 s (unchanged)** | `MESH_ROTATE_REPLY_TIMEOUT` |
| rotate cooldown | **60 s (unchanged)** | `MESH_ROTATE_COOLDOWN_SECS` |

A new engine constant `MESH_VERIFY_SECS = 10` (`net.rs`) drives the one-shot; the
mid-window probe retry derives from it (`MESH_VERIFY_SECS / 2`).

## 5. New wire / state / commands (all additive)

- **`NetMeshVerify { peer, generation }`** — new engine-internal command
  (INTERNAL: the node's own transport speaking, like `NetMeshRotate`; **not** an
  MCP tool). Update the co-equality `INTERNAL` list + count in
  `molt-mcp/src/lib.rs` and its test.
- **`recompute_net_health`** grows the "unverified ⇒ Degraded from t=0" branch
  (§4.1) — subsumes/tightens the open-time born-dead reporting.
- *(Optional, Q3)* **`MESH_PROBE_TAG` + `MlsDecode::Probe`** + warm-back-once.
- **No new persisted/runtime field** — "verified" is derived from `last_seen ≥
  mesh_up`. No chain change, no `roster_canonical_bytes` / `molt-chain-*` bump
  (none of this is chained).

## 6. Implementation plan (phased, test-first)

Each phase lands green on master with its own tests before the next.

- **Phase 1 — Honest-from-t=0.** `recompute_net_health`: a linked-up-never-heard
  leg reads `Degraded("verifying")`. **Red first:** a just-`link_up`-never-heard
  leg reads `Ok` today (see the existing `a_leg_heard_from_since_mesh_up_stays_ok`
  test neighbourhood, `net.rs:2966`) — must go `Degraded`. Kills the flap alone.
- **Phase 2 — Solicited probe frame.** `MESH_PROBE_TAG` + `MlsDecode::Probe` +
  warm-back-once. Unit-test the frame decode and the **no-echo** property (a probe
  provokes exactly one keepalive back, never another probe), and that an unknown
  `\x00`-control tag decodes to a dropped no-op. Must land before Phase 3 (which
  uses it).
- **Phase 3 — Verify-on-link_up + fast rotate.** `NetMeshVerify` one-shot armed in
  `cmd_net_link_up` (sends the probe + schedules the `T_verify` check); handler
  rotates an unverified leg. **Loopback test using `expire_queue`:** a born-dead
  leg at open rotates within ~`T_verify` and delivery resumes; a healthy leg is
  verified by the probe round-trip and never rotates.
- **Phase 4 — Founding parity + real-SMP.** Confirm the `link_up` hook fires at
  `cmd_net_mesh_ready` (impl check §4.3); extend `mesh_restart_over_smp.rs` with an
  idle-then-cold-reopen case; the `#[ignore]`d real-SMP verify proof.
- **Phase 5 — Land.** `/code-review` each phase's diff; `cargo clippy
  --all-targets` = 0; green on master.

## 7. Questions — settled 2026-07-21

- **Q1 — `T_verify` + retries.** ✅ **10 s, 1 retry** (conservative; §4.5).
- **Q2 — Every `link_up` or only the first at open?** ✅ **Every** `link_up`
  (covers Stage-B resubscribes too, generalizes cleanly, strictly better).
- **Q3 — Solicited probe or reuse mutual `warm_leg`?** ✅ **Solicited probe from
  day one** (`MESH_PROBE_TAG`, §4.4): deterministic single-node verification,
  independent of the peer's warm timing. Robustness over minimal wire surface.
- **Q4 — Non-blocking open.** ✅ **Yes** — open immediately, amber
  "verifying/reconnecting", heal underneath; no blocking mesh gate in the ritual.
- **Q5 — Scope.** ✅ Full-partition stays Stage-4 recovery; mid-session death stays
  keepalive + 300 s. This design is *only* the open/founding regime.

## 8. Decision log

- 2026-07-21 — incident: cold reopen of 3 nodes, one leg (c2→c3) born dead behind
  a surviving hub; healed but ~1 min slow and `net_health` flapped green. Root:
  the self-heal is mid-session-tuned; at open a born-dead leg reads false-`Ok` for
  45 s and is detected only on the 30 s tick. Keepalive cannot cover the offline
  gap, so the worst case concentrates at open.
- 2026-07-21 — user chose **Fix A (verify-at-open) first, design-doc + discussion
  first** (over Fix B, three-state receipts).
- 2026-07-21 — forks settled: **solicited probe frame from day one** (Q3) and
  **`T_verify = 10 s` conservative** (Q1); Q2/Q4/Q5 confirmed to the recommended
  defaults. Build order: honest health → probe frame → verify-on-link_up → founding
  parity → land.
