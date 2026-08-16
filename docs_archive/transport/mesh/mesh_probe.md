# Mesh transport probe — measuring deaf-on-reopen (server expiry vs. our bug)

> **Historical (2026-07-30):** the SMP transport this document describes was
> removed in etappe N-demo of the Nostr transport replacement
> (`docs_archive/transport/nostr_transport_marmot.md`), and the probe machinery with
> it. Kept as the record of the deaf-on-reopen diagnosis that drove the
> mesh-reliability work — part of why SMP was left.

A workspace that reopens **deaf** (peers can't find each other, the banner spins
"reconnecting") has two very different causes, and the fix differs completely:

1. **Server-side expiry** — the SMP server deleted our inbound queue while we were
   offline. Then the fix is operational/redundancy (own server, multi-server,
   keep a node warm), because the address is genuinely gone.
2. **A moltrepublic resume/delivery bug** — the queue is *still alive* on the
   server (with our peers' messages waiting, up to the 21-day message retention),
   and we simply fail to *receive* from it on resume. Then the fix is in
   moltrepublic, and the waiting messages come back once resume is fixed.

**This distinction matters because the default SMP server does NOT delete idle
queues** — only *messages* expire (after `expire_messages_days: 21`), and
`[INACTIVE_CLIENTS] disconnect` is off by default (and only drops a TCP
connection, never a queue). See the SimpleX server docs. That makes (2) the more
likely culprit — but it must be **measured**, not assumed. The earlier
"idle-expiry" diagnosis (`mesh_selfheal.md`) is under re-examination precisely
because `SUB → OK` (which the deaf legs returned) is *inconsistent* with a
deleted queue (a deleted queue answers `Err`, not `OK`).

## What the probe does

`crates/molt-engine/src/probe.rs`, gated by the env var `MOLT_MESH_PROBE`. When
set, opening a workspace runs a raw per-leg SMP self-test **instead of** the real
mesh (SMP allows one subscription per queue, so it can't run alongside it — the
workspace is offline-for-diagnostics during the probe). For each mesh leg it:

1. **subscribes** to *this* node's inbound queue — `SUB → Ok` means the queue is
   alive on the server; `Err` means it is gone;
2. **sends** a marked raw frame (`MOLT-MESH-PROBE:<me>`) to the *peer's* inbound;
3. **listens** ~20 s. Receiving the peer's marker (run the peer in probe mode too)
   — or catching its real send-retries — proves the queue **delivers**.

It bypasses MLS/mesh entirely, so it isolates the pure transport.

## How to run it (on the deaf Test12, BOTH nodes)

On each node, start moltd (or the dev app) with the env var + logging, then open
the deaf workspace:

```
MOLT_MESH_PROBE=1 RUST_LOG=molt_mesh_probe=info <your moltd invocation>
# then open Test12; watch stderr for the `molt_mesh_probe` lines; close after ~30 s
```

Run it on **C1 and C2 at roughly the same time** so each hears the other's probe
frame. The banner will say *"mesh-probe: diagnostics only — the real mesh is NOT
running"* — that's expected; nothing is sent/received except the probe.

## Reading the per-leg VERDICT lines

| Verdict | Meaning | Implication |
|---|---|---|
| `SUB → ERR … queue is GONE` | The server rejected the subscription — the queue was deleted. | **Server expiry is real** for this server → operational fix (own/redundant server, keep-warm / always-on hub). |
| `queue ALIVE + DELIVERING` | `SUB → Ok` and the peer's probe (or real traffic) arrived. | **Not the server — a moltrepublic resume/delivery bug.** Fix resume; Test12's waiting messages should come back. |
| `SUB OK but NOTHING delivered` | The queue exists but nothing arrived in the window. | Either the peer wasn't sending (make sure the PEER also ran in probe mode), or a **queue-id split** — compare this leg's `snd_id` on one node against the *other* node's `rcv_id` for the same peer; they must be **equal**. A mismatch is a moltrepublic resume bug (subscribing to a different queue than peers send to). |

If both nodes show `queue ALIVE …` (delivering or silent-but-SUB-OK), the queue
is alive → **the fix is in moltrepublic's resume path**, and the next step is to
make reopen correctly re-subscribe to the existing queue (and to fix any split).
Only `SUB → ERR` on the deaf legs would confirm genuine server-side expiry.

## Notes

- Diagnostics-only: with the env var **unset** (normal operation) the open path is
  byte-for-byte unchanged. Smoke-tested over loopback: a live queue reads
  `AliveDelivering`; the `expire_queue` seam (SUB Ok, delivery dropped) reads
  `AliveButSilent`.
- The probe reuses the same wrap keys as the real mesh, so a marker sent by C1 to
  C2's inbound is unwrappable by C2's probe (the queue's wrap key is shared).
- This does not modify any state — it only subscribes/sends/listens. Safe to run
  repeatedly.
