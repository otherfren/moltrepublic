# Design: Track C — regular queue rotation (unlinkability)

Status: **OPTION A BUILT 2026-07-24.** The 4th SimpleX-reliability track (after A
honest-status ✅, D reopen-smoothing ✅, B redundancy ✅ complete). The user chose
**Option A** (§3) — ship the simple scheduled rotation now, accepting the brief
whole-mesh blip. Built + green + pushed; Option B (per-queue rotation, zero blip)
remains the follow-up. Read `documents/mesh_reliability.md` (Track C) and
`documents/mesh_verify_at_open.md` (the rotate machinery) first.

## BUILT (Option A) — summary

- `MESH_ROTATE_CADENCE_SECS` = 6 h; a per-leg `established_at` stamp (State,
  runtime-only) — reset ONLY for the leg that rotates (unlike `mesh_up`, which a
  whole-mesh rebuild refreshes for all legs, which would starve every leg but the
  lowest-named).
- `rotate_stale_legs` runs on the presence tick (after `rotate_deaf_legs`):
  birth-stamps new legs, then rotates the SINGLE oldest REACHABLE leg past the
  cadence via the existing `cmd_net_mesh_rotate` (sharing its 60 s `rotate_at`
  cooldown, so a leg is never double-rotated by the heal + schedule). One leg per
  tick bounds the whole-mesh re-subscribe churn. `established_at` is reset
  OPTIMISTICALLY at rotate-initiation, so a rotate that fails to complete cannot
  re-fire before a full cadence (the cooldown is the hard bound in between).
- Offline legs are deferred (their adopt handshake can't complete — the same
  partial-availability rule as verify-at-open).
- `stale_leg_to_rotate` is a pure selection fn (unit-tested without a live mesh):
  oldest-reachable-stale is chosen, offline is deferred, a fresh leg is
  birth-stamped not rotated. No new `Command` (piggybacks the presence tick), so
  co-equality is untouched. NOTE the residual: a rotate rebuilds the whole
  supervisor, so a scheduled rotate is a brief whole-mesh blip (Option A's
  accepted cost); the zero-loss overlap (keep the old queue as `rcv_extra` a
  grace window) and per-queue rotation are Option B follow-ups. With Stage 2
  redundancy ACTIVE, the other queue already carries traffic during a rotate.

### Post-build audit fix (commit 73108de)

The activation+Track C audit found that a rotate (deaf-heal, verify-at-open, AND
scheduled) minted ONE queue and replaced the whole leg, so within a cadence every
N=2 leg decayed to N=1 — silently nullifying Track B. FIXED: `cmd_net_mesh_rotate`
and `spawn_mesh_extension` now mint `transport.redundancy()` inbound queues and
build an N-rcv leg (announce all N; clean up all on failure); `RitualTransport`
now delegates `redundancy()` to its inner transport (it had used the trait default
1). So a rotate PRESERVES redundancy. Also: a proactive rotate of a healthy leg no
longer flashes "reconnecting" (the alarm now needs a pending rotate AND no contact
within the keepalive interval). N=1 stays byte-neutral.

Original design below.

---

Status: **DESIGN DRAFT 2026-07-23.** The 4th SimpleX-reliability track (after A
honest-status ✅, D reopen-smoothing ✅, B redundancy ✅ mechanism). Read
`documents/mesh_reliability.md` (Track C) and `documents/mesh_verify_at_open.md`
(the rotate machinery) first. **This doc surfaces a genuine architectural fork —
it is not execution-ready until that fork is decided.**

## 1. Goal

A long-lived inbound queue is a long-lived **correlation handle**: a passive
server (or network observer) that sees the same queue id receiving for weeks can
link a member's sessions over time. SimpleX rotates queues on a slow schedule so
no queue id is a durable identifier. Track C: on a slow cadence (order of hours),
a node **mints a fresh inbound queue for a leg, announces it over the live mesh,
and retires the old** — so queue ids churn and no id is long-lived.

## 2. The machinery already exists — and the tension it creates

Track C is "the Stage-3 rotate on a timer instead of a deaf trigger." The rotate
path (`cmd_net_mesh_rotate` → mint fresh queue + subscribe → `MeshAnnounce`
re-announce over the working legs → peer adopts + replies → `cmd_net_mesh_extended`
folds the reciprocal link back) is built and proven (`mesh_rotate.rs`,
`two_instances.rs`). `rotate_deaf_legs` already calls it from the presence tick.

**BUT — the load-bearing fact:** `cmd_net_mesh_extended` (net.rs:1652)
**tears down and rebuilds the WHOLE supervisor** (`teardown_net()` +
`build_real_net_shared`) — every leg's subscription drops and re-establishes, not
just the rotated one. It does this deliberately (sharing the live group Arc so no
ratchet rewind), and for a *deaf* leg being healed the blip is invisible (that
leg was already dead). **For a HEALTHY leg rotated on a schedule, a
whole-mesh blip is a reliability COST** — precisely what the user's
reliability goal wants to avoid. Rotating M legs on a timer = M whole-mesh blips.

So a naive "call the rotate on a timer" makes reliability *worse* to buy
unlinkability. That is the fork.

## 3. The fork (needs the user's call)

**Option A — Accept periodic whole-mesh blips (cheap, ships now).**
Rotate one leg at a time on a slow cadence (e.g. every few hours, one leg per
tick), reusing the exact existing machinery. Each rotate is a brief whole-mesh
re-subscribe (sub-second over SMP; the outbox/reassembler absorb the gap — the
same blip a deaf-leg heal already causes today, just now also on healthy legs).
Simple, minimal new code. Downside: a periodic, cross-whole-mesh re-subscribe
even when nothing is wrong; with a member offline the adopt handshake can't
complete for that leg (defer it — rotate only *reachable* peers).

**Option B — Per-queue rotation without a full rebuild (aligns with the
reliability + Stage 2 redundancy goal; more work).**
Build a *targeted* leg-rotate that swaps ONE queue in place — add the new inbound
queue's forwarder to the running supervisor and retire the old — WITHOUT tearing
down the other legs' subscriptions. Combined with Stage 2's N=2 redundancy:
rotate one of a leg's N queues at a time, so the leg is **never fully down during
a rotation** (the other queue carries traffic). This is the SimpleX posture:
overlap, never cut over. Downside: the supervisor's spawn/JoinSet owns the
forwarder set; adding/removing one live requires a control channel into the
running supervisor (it has none today — it's spawned once and only stopped
wholesale), so this is a real supervisor-lifecycle change.

**Recommendation:** ship **A** first (it delivers the unlinkability win with the
proven machinery and is honestly no worse than today's heal blip), and treat **B**
as a follow-up once Stage 2 redundancy is *activated* in production (B only pays
off when there are ≥2 queues per leg to rotate one-at-a-time; with N=1, rotating
"one of the N" still drops the leg, so B needs B-redundancy live first). Until
redundancy is activated, A is the whole story.

## 4. Design (Option A — the shippable path)

### 4.1 Cadence + selection
- A new slow ticker (`MESH_ROTATE_CADENCE_SECS`, order of hours — well under any
  server retention window, slow enough to stay beaconless-ish per
  `concept-transport-simplex-tor.md` §3.4). A `Command::NetMeshRotateTick`
  (INTERNAL — an MCP agent must not drive rotation; it's a co-equality
  requirement, like the other `Net*` ticks).
- On each tick: pick the **single oldest** leg (by an `established_at` stamp per
  leg) whose peer is **reachable** (`peer_present`), and rotate it. One leg per
  tick bounds churn and spreads the whole-mesh blips out. A leg to an offline
  peer is skipped (its rotate can't complete — same limit as verify-at-open) and
  retried next tick once the peer is back.
- **Share the cooldown/seen-set with the deaf-leg + verify-at-open rotates**
  (`rotate_at`, `MESH_ROTATE_COOLDOWN_SECS`): a leg just healed/verified must not
  also be scheduled-rotated, and vice-versa — one rotate per leg per window,
  whatever triggered it. `cmd_net_mesh_rotate` already debounces on `rotate_at`;
  the scheduled path goes through it, so this is free.

### 4.2 Overlap (don't drop in-flight frames)
The design-doc requirement ("keep the old queue during the switch"). The current
rotate mints+subscribes the new queue BEFORE the announce, and the whole-mesh
rebuild re-subscribes everything — but the OLD queue for the rotated leg stops
being subscribed after the rebuild, and is never explicitly `delete_queue`d, so
it lingers server-side (its un-acked frames redeliver only if re-subscribed).
For a *scheduled* rotate of a *healthy* leg we must not lose the last frames on
the old queue:
- Keep the old queue in the rebuilt mesh as an EXTRA (Stage 2 `rcv_extra`!) for a
  grace window, so the new supervisor subscribes BOTH; the reassembler dedups any
  overlap. Retire the old (drop it from the mesh + `delete_queue`) only after the
  peer is heard on the new queue, or after a grace timeout.
- This makes Track C a natural *consumer* of the Stage 2 Vec-of-queues shape: a
  rotation is "prepend a fresh queue, drop the stale one after the grace window,"
  i.e. the leg briefly runs N+1 queues. Elegant — and another reason B-redundancy
  and C-rotation share the same data model.

### 4.3 Establishment stamp
Add `established_at: HashMap<MemberId, u64>` (runtime-only, like `rotate_at`),
stamped when a leg's mesh is built/rebuilt (founding mesh-ready, join-sealed,
each `cmd_net_mesh_extended`). "Oldest leg" = min `established_at`. Not persisted
(a reopen re-establishes everything anyway).

## 5. Security notes (carry forward)
- The rotation ticker is INTERNAL (co-equality): an MCP agent cannot force queue
  churn / correlation-window resets on the node.
- A rotate re-mints only the node's OWN inbound queue for one leg + re-announces
  over the authenticated MLS mesh — it moves no one else's queue and forges no
  member (same surface the deaf-leg rotate already has, audit-cleared).
- Rotation strictly SHRINKS the correlation window — it never weakens
  confidentiality (MLS unchanged) or the sign-what-you-see/chain invariants
  (queues are transport bookkeeping, unsigned/unchained).
- Overlap dedup relies on the SAME single-reassembler/single-cursor guarantee
  Stage 2 established — an overlapping old+new queue is just two more redundant
  copies, deduped by `(id, index)` + cursor/MLS-replay.

## 6. TDD plan (loopback)
1. `a_scheduled_rotate_fires_on_cadence_for_the_oldest_reachable_leg` — drive the
   tick, assert the oldest leg's inbound queue id changed and the peer still
   delivers.
2. `a_scheduled_rotate_skips_an_offline_peer` — an unreachable peer's leg is not
   rotated (its queue id is unchanged), a reachable one is.
3. `rotation_does_not_drop_in_flight_frames` — a frame in flight on the old queue
   during a rotate is still delivered exactly once (the overlap window).
4. `a_just_healed_leg_is_not_also_scheduled_rotated` — shared cooldown.

## 7. Open questions (for the user)
- **The fork in §3:** ship A (accept periodic whole-mesh blips now) vs. wait for /
  build B (per-queue rotation, needs redundancy activated + a supervisor control
  channel). Recommendation: A now, B after redundancy is live.
- **Cadence:** the actual `MESH_ROTATE_CADENCE_SECS` (hours? which?). SimpleX's
  own default is on the order of many hours to a day.
- **Does rotation run at all before B-redundancy is ACTIVATED?** With N=1, even
  Option A's per-leg rotate is a real per-leg outage during the adopt handshake
  (bounded, but a healthy leg briefly churns). Arguably rotation should only turn
  on once redundancy is live (so there's always a surviving queue). This couples
  Track C's activation to Track B's activation (the 2-server-transport decision).
