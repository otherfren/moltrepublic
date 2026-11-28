# Stage B — Self-healing links + honest transport health

> **Historical (2026-07-30):** the SMP transport this document describes was
> removed in etappe N-demo of the Nostr transport replacement
> (`docs_archive/transport/nostr_transport_marmot.md`), and with it the SKEY fix and
> the N-queue redundancy machinery. Kept as the record of why SMP was left.
> The per-peer resubscribe watchdog itself SURVIVES, reduced to a
> single-queue redial loop in the supervisor (`recv_watchdog_task`).

Status: **IMPLEMENTED** (2026-07-19, together with the SKEY restart fix).
This document is the design record; the history of the incident and of the
diagnosis is in the closing section.

## 1 Problem

Two related defects silently killed links:

1. **Sender keys did not survive a restart** (the 2026-07-19 incident):
   `SmpTransport::send` secured a peer queue on the first send via
   `SKEY` with a freshly random Ed25519 key. The mesh-up persist, however,
   exported the creds BEFORE the first send (send_keys empty), and only
   a clean close after traffic exported them again. A hard-killed
   seat minted fresh keys after the reopen → the server rejects a SKEY
   with a different key on a secured queue with `ERR AUTH` → the
   outbox retried endlessly in backoff. One WARN line, otherwise silent;
   `net_health` stayed Ok.
2. **Dead subscriptions stayed dead**: subscribes fired exactly
   once at supervisor spawn; if the delivery stream ended (the SMP recv loop
   dies on any error), the seat was deaf for the session. On top of that
   came a zombie mode: a server-side `END` was swallowed — the
   connection waited forever for a subscription that the server had
   long since ended (zero log lines).

## 2 Fix part 1 — deterministic sender keys (molt-net)

- `SmpState.sender_seed: Option<[u8; 32]>`, minted via `getrandom` in
  `SmpTransport::with_dialer`. On an RNG error it stays `None` and the
  first send fails honestly (`NetError::Crypto`) — NEVER a
  constant fallback (predictable keys could be pre-SKEYed).
- Sender key per queue = `Ed25519(HMAC-SHA256(seed,
  "molt-smp-sender-v1" ‖ sender_id))` (`derive_sender_key`). A
  reopened transport thus re-derives the SAME key — the
  persist order does not matter, because the seed exists from transport birth
  and even the mesh-up persist carries it.
- `secure_as_sender(sender_id, sk)` now takes the key as a parameter.
- Send flow: cache hit → sign; otherwise derive the key, try `SKEY`;
  if the SKEY is rejected, **the signed SEND is attempted anyway** —
  the server's send verdict is authoritative (covers servers that reject a
  same-key re-SKEY; a genuine foreign-key case then fails
  honestly as a send error → backoff → Degraded). The key is cached
  immediately on SKEY ok, in the fallback case ONLY after a send success.
- Creds additively **V2**: `PersistedCreds.sender_seed` at the end; a V1 blob
  (pre-fix) falls back to the old layout on import (recv+send_keys
  adopted, own fresh seed stays; a seed once adopted is never
  discarded by a later V1 import). A V2 export stays readable by
  a V1 reader (bincode v1 tolerates trailing bytes — pinned in the
  unit test).
- Live-server pin: `skey_rederivation_after_reopen_keeps_sending`
  (molt-net, `--ignored`): export BEFORE any send, two fresh
  incarnations send one after the other with the same creds — both arrive.

**Limit:** queues that a pre-fix session secured with a lost
random key stay unrecoverable in the send direction (the server
knows only the lost key). Such legs need queue rotation /
recovery (open, §6); Stage B at least makes them visible.

## 3 Fix part 2 — resubscribe watchdog (molt-net/supervisor)

Per peer, `recv_watchdog_task` replaces the one-shot subscribe+`recv_task`:

```
loop {
    rx = first prebuilt subscribe OR transport.subscribe(...)
         (error → link_down(reason), capped jittered backoff, retry)
    link_up(peer); attempt = 0
    recv_task(rx) →
        EngineGone   → return          (actor gone)
        StreamEnded  → link_down("subscription ended — resubscribing")
    backoff, continue
}
```

- Backoff: `backoff_ms` with `retry_base_ms`→`retry_cap_ms` from `NetConfig`
  (prod 1 s → 2 min, tests 20 → 100 ms).
- An SMP `subscribe` picks a fresh connection itself → the watchdog
  re-dials implicitly. Reassembler/reorder/epoch buffers are fresh per
  incarnation; cursors live in the shared `state` Arc; unacked messages are redelivered by the
  server to the next subscriber.
- The prebuild (semaphore 4) stays for the FIRST subscribes; a
  failed first subscribe is no longer fatal.
- `END` from the server now ends the recv stream (previously a zombie) — the
  watchdog takes over.
- Send path: nothing to build — the pool re-dials on a broken connection,
  `send_one` backs off with a cap; only the signals are new (§4).

## 4 Honest health (sink → engine → `net_health`)

- `EngineSink` (additive, default no-op): `link_up(member)`,
  `link_down(member, reason)`, `send_ok(member)`. `send_one` fires
  `send_ok` when it leaves backoff; `send_failed` still fires on the first
  failed attempt.
- Three new engine-internal commands (INTERNAL list, generation-checked):
  `NetLinkUp`, `NetLinkDown { reason }`, `NetSendOk`.
- Engine state: `net_link_down: BTreeMap<Member, reason>` (inbound leg) and
  `net_send_stuck: BTreeMap<Member, reason>` (outbound leg;
  `cmd_net_send_failed` sets it — the endless SKEY backoff is thereby
  visible). `recompute_net_health()`:
  - `Down` (open/config verdict: fail-closed dialer, detached reopen)
    is NEVER overwritten.
  - both maps empty → `Ok`; otherwise `Degraded { "link to <m>: <r>;
    sends to <m>: <r>; …" }`. Emit only on change.
  - Workspace switch/close (`reset_workspace_state`) clears both maps.
- **Open is honest**: after the supervisor spawn, `build_real_net_shared` marks
  every leg `"connecting"` → the pill is briefly amber until
  every watchdog has confirmed its subscribe. `Ok` has since meant truly
  "every mesh link is alive" (the mesh_resume test polls accordingly).
- GUI/MCP: no change needed — `NetHealth::Degraded` + amber pill +
  tooltip already existed end to end.
- Silent drops are loud: undecodable inbound MLS messages
  (replay/proposal/garbage) WARN with a running counter; local
  send skips count too; the mesh bootstrap logs discarded
  announcements with the reason.

## 5 Tests (all green on master)

- molt-net unit: seed derivation deterministic + queue-bound; V2 creds
  round-trip; V1 fallback; V2-stays-V1-readable; V1 never discards an
  adopted seed.
- molt-net `--ignored` (live SMP): `skey_rederivation_after_reopen_keeps_sending`.
- molt-net supervisor (loopback, `NetConfig::fast` +
  `LoopbackHub::sever_subscriptions()` seam):
  `a_severed_subscription_resubscribes_and_resumes_delivery`,
  `a_healed_send_after_backoff_fires_send_ok`.
- molt-engine unit: Ok→Degraded→Ok (reasons name peers), Down guard,
  reset clears.
- molt-engine `--ignored` (live SMP, the incident proof):
  `mesh_restart_over_smp` — real 2-of-3 founding over smp.konkin.io,
  6-direction baseline, restart (A/B clean, C hard), full matrix afterwards
  (C via the heal guarantee, §6 point 2).

## 6 Deliberately open

1. **Queue rotation/healing of permanently dead sender legs** (pre-fix queues
   with a lost random key): needs re-provisioning of the affected
   queue pairs (mesh-extension-like) or the recovery ritual. The
   user's live republic has such legs; they now show up as
   Degraded instead of silently Ok.
2. **Per-drain MLS persist** (full crash safety of the ratchet): a hard-
   killed seat resumes from the last persist state; messages in the
   regressed window reuse sender generations and are replay-rejected by the peer
   (now LOUD, with a counter). The link heals from the
   next message on (pinned in the matrix test); the window messages
   themselves are lost until the persist lands per drain.
3. **Transient one-direction outages over live SMP**: in single runs,
   exactly one founder outbound direction sporadically died BEFORE the restart
   without any signal (sends server-confirmed, subscription live, no
   discard). Suspects: server-side behavior (the END zombie is excluded since
   this change; queue-tag debug logging is ready
   to attribute the next case to a hop). Since our SMP acks fire lazily
   on the NEXT recv, the server does not redeliver a message lost on the app side
   to the same connection — ack-after-fsync
   (T3 note) remains the related open item.
4. `net_health` is session-global (one open workspace) — unchanged.

## 7 Incident history (short version)

3-node republic, all three restarted (2× clean, 1× hard kill);
afterwards 1 of 6 directions delivered. Stderr of the killed node:
`SKEY rejected: ERR AUTH` in endless backoff. Diagnosis + fix design in
fixme.md (working document of this session, merged into this
document at landing). The repro test `mesh_restart_over_smp.rs` failed before the fix with
exactly `dead directions: ["member-c → founder-a", "member-c → member-b"]`
and went green with the fix.
