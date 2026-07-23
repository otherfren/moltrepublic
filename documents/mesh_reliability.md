# Design: SimpleX-level mesh reliability — honest status, redundancy, rotation

Status: **DISCUSSION DRAFT 2026-07-21.** Grounded in a live measurement against
the real Test12 on `smp8.simplex.im` (3 nodes classic/dark/brutal). Read
`documents/mesh_probe.md`, `documents/mesh_selfheal.md`,
`documents/mesh_verify_at_open.md`, and the CLAUDE.md transport section first.

## 0. What the measurement established (so we fix the RIGHT thing)

The user reopened Test12 (C1+C2, C3 offline) and saw both nodes spinning
"Verbinde erneut…" — apparent total deafness. Measured live with the
`MOLT_MESH_PROBE` diagnostic + `molt_net=debug` recv-path instrumentation:

- **Queues are ALIVE.** Every leg on every node: `SUB → OK`. NO queue expired.
  The "server idle-expired our queues" diagnosis (`mesh_selfheal.md`) is **wrong**
  — matches the SMP docs: idle queues aren't deleted, only messages expire (21 d).
- **The receive path WORKS.** Frames are received → unwrapped → reassembled
  (`COMPLETE`) → MLS-decoded → **DELIVERED as real application messages**, in every
  configuration tested (all-3 AND C1+C2-only). No Discard, no FutureEpoch, no
  wrap-mismatch, no reassembly stall. There is **no receive bug**.
- So the deafness was **not** the server and **not** a lost/broken transport. The
  most consistent explanation for "beide finden nix" is two things compounding:
  1. **The banner over-alarms.** `net_health` is `Degraded` if *any* leg is deaf —
     including a leg to a member who is simply **offline** (C3). So C1 and C2 show
     "Verbinde erneut…" **because of the dead leg to the offline C3**, even though
     C1↔C2 itself delivers. The user reads a global "reconnecting" and concludes
     the whole mesh is dead. (SimpleX shows *per-contact* status, never a global
     alarm for one absent contact.)
  2. **Cold-reopen re-establishment latency + self-heal churn.** On reopen the
     alive queues deliver within seconds, but `verify-at-open` may rotate a leg at
     t=10 s before it has *decoded* a frame, and the rotate/re-announce handshake
     (especially with a member offline) takes time to converge — a visibly slow,
     churny warm-up, honestly shown as amber the whole time.

The user asked for **SimpleX-level reliability incl. redundancy and regular queue
rotation.** SimpleX's own robustness (per its protocol docs) is: generous server
retention (queues persist), **redundancy across multiple servers/queues**, keeping
connections warm, and **queue rotation** on a slow schedule — plus **honest
per-connection status**. This doc plans those four tracks.

## Track A — Honest per-peer status ✅ BUILT (2026-07-23, commit 9b27e67)

**Done.** `recompute_net_health` now alarms ("reconnecting to {m}") only for a leg
to a peer with *recent contact* (`peer_present` = `presence_state != offline`)
that is live-but-deaf or rotated-toward-and-unheard; a never-/long-unheard peer is
gentle OFFLINE (its presence pill), never a banner alarm. `link_down`/`send_stuck`
still alarm. Supersedes verify-at-open's Phase-1 "verifying" amber for a
never-heard leg (green banner + offline pill = honest "connected, peer offline";
green now means MY connection health, per-peer reachability is presence). The
deaf-leg HEAL is unchanged (`deaf_legs` still drives the rotate). Tests reworked;
full suite green, clippy clean. The boundary the user chose: **never-heard leg =
offline (gentle).** Original design below.

**Problem:** one offline member drags the whole banner to "reconnecting", so a
working mesh looks dead.

**Design:** separate "a peer is offline" (expected, gentle) from "I cannot reach
anyone / a leg to an *online* peer is down" (alarming).

- Keep the existing per-peer `last_seen`/presence. A leg whose peer is simply
  quiet/offline (never heard, no live evidence) reads as **presence = offline**,
  not `net_health = Degraded("reconnecting {peer}")`.
- `net_health` becomes **Ok** when *every reachable* peer delivers, and carries a
  soft, separate signal for absent members ("N of M members online") rather than a
  red/amber "reconnecting" alarm. "Reconnecting/Down" is reserved for the honest
  hard case: a leg to a peer we have *evidence is online* (recent traffic) that
  then goes silent, or we can reach **no one**.
- The banner then reads like SimpleX: green "connected" with a quiet "1 member
  offline", not a scary "Verbinde erneut…". A genuine full outage still goes red.

**Fork (needs your call):** what exactly flips a leg from "peer offline" (gentle)
to "reconnecting" (alarming)? Proposal: a leg is "reconnecting" only if the peer
was heard **this incarnation** (proving it's online) and then went silent past the
deaf window; a never-heard leg at open is "peer offline", gentle. This is a
`net_health`/presence semantics change — the load-bearing honesty rule ("never
look healthier than you are") must still hold: we are NOT hiding a real outage,
only not screaming about an absent friend.

## Track B — Redundancy across servers and queues (N=2, staged)

**Staging (from the change-surface survey; each stage independently landable):**
- **Stage 0 ✅ BUILT (commit 30ce5a6):** `RcvQueue.server` + `MeshLink.rcv_server`
  (additive, `#[serde(default)]`, no version bump); `subscribe` honors the queue's
  own server (`server_of`). Fixes the latent resume bug; behaviour-neutral
  (loopback ignores the server, single-server SMP has rcv_server == self.server).
  Prerequisite for the rest.
- **Stage 1 ✅ BUILT (commit e349f21):** `SmpTransport` is internally
  multi-server — `server: SmpServer` → `servers: Vec<SmpServer>`, `pool` →
  `pools` (one pool per server), plus a round-robin `next` cursor.
  `send`/`subscribe`/`delete_queue` route by the queue's OWN server via
  `route(&str)` (match by rendered address, fall back to the first for
  empty/unconfigured); `create_queue` round-robins placement across the servers.
  `with_dialer_multi`/`new_multi` build the N-server form; `new`/`with_dialer`
  delegate with a single-element list, so single-server config is byte-for-byte
  the former behaviour. Creds stay server-agnostic (the server rides on the
  RcvQueue/SndQueueAddr, persisted in MeshLink since Stage 0). TDD:
  `route_dispatches_by_the_queue_s_own_server`. All 23 smp transport tests +
  full molt-net green, clippy clean. `reopen_transport` still builds a
  single-server transport (config is single-server) — its per-server
  construction lands with Stage 2 when the config gains a server list.
- **Stage 2 (the feature):** N=2 redundant inbound queues per leg. `bootstrap_mesh`
  mints N per peer; `MeshAnnounce.queues` → `Vec<QueueHandover>` per peer (additive);
  `PeerLink` carries N send targets + N recv queues; the supervisor fans send to N
  (reusing the ONE ciphertext per seq — never re-encrypt) and subscribes N (the N
  recv loops of one peer MUST share one Reassembler + the single-writer cursor, or
  the `(id,index)` dedup won't cross them — the key correctness detail); rotate /
  extend / recovery mint N; config gains a server list. Bump
  `TRANSPORT_STATE_VERSION` only if the MeshLink shape changes non-additively.

---
### Original design

**Problem (latent, `mesh_selfheal.md` §7):** each per-pair leg is a **single
queue on a single server**. `MeshLink` persists `snd_server` but **no
`rcv_server`** — `reopen_transport` collapses every leg to `mesh[0].snd_server`
(`founding.rs:49`), so a genuinely multi-server mesh mis-subscribes on resume. One
server hiccup ⇒ a dead leg.

**Design (SimpleX-style redundancy):**
- **Per-queue server.** Add `rcv_server` to `MeshLink`/`RcvQueue`; thread a
  per-queue server through `subscribe`/`send` (today they ignore it, CLAUDE.md
  "Transport gotchas"). This is the prerequisite for everything else and fixes the
  latent resume bug on its own.
- **N-of-M redundant inbound queues per leg.** A recipient creates its inbound
  queue on **2+ servers**; senders send to all; the receiver dedups (the
  reassembler already dedups by `(msg id, chunk index)` — `chunk.rs`). Losing one
  server/queue leaves the leg alive. The handover (`QueueHandover`,
  `mesh.rs`) grows from one address to a small set (additive).
- **Server set** comes from config (a list, not the single `[transport.smp]`), so
  a node can spread queues across e.g. smp8 + a self-hosted server.

**Cost/forks:** more queues = more server load + more traffic (each message sent
N×). Choose N (2 is the usual sweet spot). Schema ripples: `MeshLink`,
`QueueHandover`, `roster`/founding queue creation, `reopen_transport`, the
persisted creds. Big but mechanical; the dedup already exists.

## Track C — Regular queue rotation

**Problem:** a long-lived queue is a long-lived correlation handle (unlinkability)
and a single point of staleness. SimpleX rotates queues on a slow schedule.

**Design:** on a slow cadence (hours), a node **mints a fresh inbound queue,
`MeshAnnounced`s it over the live mesh, and retires the old** — reusing the
*exact* Stage-3 rotate machinery already built (`cmd_net_mesh_rotate` →
`NetMeshReAnnounce` → adopt → `cmd_net_mesh_extended`, `mesh_verify_at_open.md`).
Rotation is that path on a timer instead of on a deaf-leg trigger.

- **Overlap, don't cut over.** Keep the old queue subscribed for a grace window
  after announcing the new one, so in-flight messages aren't dropped (the
  reassembler dedups any overlap). Retire the old only after the peer is heard on
  the new.
- **Combine with redundancy (B):** rotate one of the N queues at a time, so a leg
  is never fully down during a rotation.
- **Cadence** well under any server expiry window and slow enough to stay
  beaconless-ish (`concept-transport-simplex-tor.md` §3.4) — order of hours.

**Fork:** rotation churns queue addresses; with a member offline, the adopt
handshake can't complete (same limit as verify-at-open). Rotation must be **safe
under partial availability** — rotate only legs to *reachable* peers, defer the
rest. And it must not fight verify-at-open's rotate (share the cooldown/seen-set).

## Track D — Cold-reopen smoothing ✅ BUILT (2026-07-23, commit 9f69256)

**Done.** The supervisor recv loop signals `EngineSink::raw_inbound` (throttled 2 s/
leg) whenever a frame unwraps at the transport — decoded or not — stamping
`last_raw_inbound` via the new INTERNAL `Command::NetRawInbound`. `leg_receiving`
(activity within `MESH_RECEIVING_SECS` = 30 s) gates the rotate: `cmd_net_verify` /
`rotate_deaf_legs` rotate only `leg_unverified && !leg_receiving` (resp.
`deaf && !receiving`). So a demonstrably-alive leg (draining redelivery / holding
future-epoch frames) is not churned to a fresh queue; a truly silent/born-dead leg
(nothing arriving) still rotates. Raw activity is NEVER presence (it may be old
redelivery — never advances `last_seen`). TDD:
`a_receiving_leg_is_alive_and_gates_off_the_verify_rotate`. Original design below.

## Track D — Cold-reopen smoothing (trust the alive queues)

The measurement shows the alive queues deliver within seconds on reopen. So the
reopen should feel instant, not churny.

- **Don't rotate a leg that is receiving.** `verify-at-open` rotates on
  `leg_unverified` (no *decoded* frame by t=10 s). But if the transport is
  **receiving frames** on that queue (they exist — the probe proved it), the queue
  is obviously alive and a rotate is counterproductive. Surface a per-leg
  "transport received something" signal (below the decode layer) and suppress the
  rotate when it's set — let the frames drain instead of churning to a fresh queue.
- **Give decode a beat before declaring deaf.** Redelivered/held frames may take a
  moment; `T_verify` should not trip while frames are actively arriving.

This makes a cold reopen resume on the existing (alive) queues — the SimpleX
behaviour: your queue was there the whole time, you just re-subscribe and drain.

## Suggested order

1. **Track A** (honest status) — smallest, highest perceived-reliability win; it
   is most of what made Test12 *look* dead. (Design fork on the offline↔reconnect
   boundary — your call.)
2. **Track B** `rcv_server` (the latent resume bug) — prerequisite for real
   multi-server; valuable alone.
3. **Track D** (reopen smoothing) — stop the churn, trust alive queues.
4. **Track B** redundant queues, then **Track C** rotation — the full
   SimpleX-level posture.

Each track lands green on master, reviewed, test-first, per the house rules. B and
C are schema-rippling (`MeshLink`/`QueueHandover`/founding/reopen) and
security-adjacent — design + discussion before code.

## Open questions

- **A:** the exact offline→reconnecting boundary (proposal above) — confirm the
  honesty rule stays intact.
- **B:** N (redundancy factor); the server-set source (config list); acceptance of
  N× send traffic + N× server load.
- **C:** rotation cadence; behaviour under partial availability.
- **All:** these mutate the load-bearing mesh/roster schema — confirm the
  additive-only + versioned-bytes discipline (CLAUDE.md) for each.
