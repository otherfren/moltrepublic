# N0.5 — Engine-side refactor map for the second transport (Nostr/NIP-EE)

> **Update (2026-07-30):** etappe N-demo has executed — the DELETE/FORK
> verdicts for the SMP machinery below are now applied (SMP is gone; the
> loopback mesh bootstrap survives until N4/N5). The doc stays as the N1+
> build map; re-verify anchors against the post-demolition tree.

Status: **DONE 2026-07-29** (the N0.5 etappe of `nostr_transport_marmot.md`).
Read-only inventory of how `molt-engine` must fork to carry a group-broadcast
Nostr runtime beside the SMP per-pair-queue mesh. Verdicts: **FORK** (kind-
specific branch/twin), **REUSE** (transport-agnostic), **NEW** (net-new seam).
Anchors are `file:line` at the time of writing — re-verify before acting.

## Headline

The concept's "second runtime path, same engine interface" framing is accurate
**only** at the three thin `molt-net` supervisor traits (`EngineSink`,
`OutboxLog`, `StateStore` — `supervisor.rs:266-339`, whose default-no-op
Stage-B methods prove they were built to extend). Everything the engine wraps
around them is member/mesh-shaped and forks. This is **a real engine refactor,
not a bolt-on** — consistent with the revised "~6 weeks" sizing. Roughly a
third of the ~49 `Net*` surface goes dead, a third needs relay twins, a third
reuses. The correctness/health backbone (delivery-guarantee tick,
`AcceptedWindow`, MLS layer, chain, co-equality) reuses cleanly; the
transport-shaped surface (enum, health model, resume/persist dispatch,
presence) is where the fork is large.

## 1. `RitualTransport` enum — FORK, large

`founding.rs:143` — closed `enum { Loopback, Smp }`, `impl molt_net::Transport`
at `:151-204` (7 queue-shaped methods). The trait is **queue-shaped**
(`create_queue`/`send(addr)`/`subscribe(RcvQueue)`/`delete_queue`) — exactly
what §4.1 says Nostr must NOT be forced behind. A `Nostr` arm cannot satisfy
`create_queue -> QueuePair`; the group runtime bypasses the trait, so dispatch
sites need a **kind check before** the enum, not just a new arm.

Fork sites: `build_smp_transport` (`founding.rs:49-70`) needs a
`build_nostr_transport` twin (relay list, not SMP servers); founding build
(`:462` + `spawn_smp_provisioning :469`); `reopen_transport` (`:88-133`,
`mesh_servers` extraction `:94-114`); join build (`:1076`); test pins (`:2347`,
`:2368`). Runtime crypto: `RealCrypto = (RitualTransport, Arc<Mutex<MlsMember>>)`
(`net.rs:557`); `runtime_transport()` mints a recovery **queue** (`net.rs:629`,
SMP-only — recovery over Nostr mints a link); `crypto_for_close`→`export_creds`
(`net.rs:618`, no queue creds on Nostr — persist relay list + h-tag + exporter
ring). Recovery: `recovery.rs:830-848,892,933`. Session resume/offline gate:
`session.rs:613-648` (keys on `smp_queues`+`mesh`) and `:663-707` ("detached"
copy names queues/mesh). State handles typed on the enum: `lib.rs:361,742,761,
767,800`. File data plane: `transfer.rs:289,295,414,755,831,853` — queue-native,
**FORK or OFF** (§10.7 = OFF in V1). Mesh probe `probe.rs:71-323` — dead on
relays. `decrypt_group_message`/`group_arc`/`restore_member` (`net.rs:637-669`)
— REUSE (MLS identical).

## 2. `Net*` commands / tickers — a third dead, a third twin, a third reuse

49 `Net*` variants (`molt-core/src/lib.rs:2635-3665`); dispatch `lib.rs:1099-1335`.

- **DEAD on Nostr:** `NetMeshExtended/Warm/Verify/Rotate/ReAnnounce/Announced/
  Ready`, `NetMeshKeepaliveTick` — mesh bootstrap/announce/rotate(Track C)/
  Stage-B/keepalive (`net.rs:1966,2061,2279,2572,2868`).
- **Relay twins (FORK):** `NetLinkUp/LinkDown/SendFailed/SendOk/RawInbound`
  (all `MemberId`-keyed; need relay-keyed twins). `NetPeerSeen` REUSES
  (traffic-derived presence, §6.5).
- **REUSE:** `NetDelivered` (`lib.rs:1099`), `NetDeliveryTick` (`net.rs:2778` —
  the delivery-guarantee beat, transport-agnostic), `NetPresenceTick` minus the
  two rotate calls, all `NetExport*/Backup*/Restore*/Test*/ListBackups*` (S3/
  config).
- **Ritual cluster (re-implemented, FORK):** `NetJoinRequested/SealSigned/
  RitualLinkReady/RitualFailed/Join*/Recover*` — the flow survives, the
  envelopes fork to 444/445 (§4.2, N4).

Tickers (`lib.rs:375-386`): `NetPresenceTick` PARTIAL FORK (rotate calls die),
`NetDeliveryTick` REUSE, `NetMeshKeepaliveTick` DEAD, `BackupTick` REUSE.

## 3. Health model — FORK (sink extensible, consumer forks)

`EngineSink` (`supervisor.rs:285-339`) is `MemberId`-keyed on every method. The
Stage-B methods already have default no-op bodies (`:303-338`) — a Nostr sink
can no-op the peer methods and add `relay_up/relay_down`. But the **consumer**
forks: `recompute_net_health` (`net.rs:2693-2745`) derives health purely from
`net_link_down`/`net_send_stuck`/`mesh_up` (`MemberId` maps) and `deaf_legs()`
(`net.rs:2631-2650`, a per-peer live-but-deaf concept with no relay analog).
GUI copy is literally `"link to {member}"`, `"reconnecting to {member}"`
(`net.rs:2733-2735`). Relay model needs `relay_status: Map<RelayUrl, Up|Down|
Slow>` + group-level "≥1 relay OK", and the copy becomes "reconnecting to
relays" (§6.5). **Sink = additive; health consumer + GUI copy = fork.**

## 4. `TransportState` — additive fields + one new discriminator; readers fork

`molt-core/src/lib.rs:1587-1636`. REUSE: `outbound` cursors (`:1594`, per-member
floors stay), `mls` (`:1605`), `accepted` windows (`:1635`), `identity_sk`
(`:1629`, gains the secp256k1 anchor). SMP-only: `mesh` (`:1611`), `smp_queues`
(`:1619`). Semantic split: `inbound` wire-seq (`:1598`) vs. a Nostr event-id
ring. **There is no transport-kind field today** — the implicit shape (has
`mesh`+`smp_queues`) is what `session.rs:613` resume-matches and `:665` offline-
gates on, which a Nostr workspace (empty mesh/queues) trips as "detached". Need:
new `kind: TransportKind { Smp, Nostr }` + relay list + h-tag + per-relay cursor
+ exporter ring (all `#[serde(default, skip_serializing_if)]` additive). Struct
= net-additive; resume/offline readers = FORK.

## 5. `ChainOracle` seam (§5) — NEW, small, clean

Commits decrypt in molt-net: `MlsChannel::decode` (`supervisor.rs:143-197`) →
`MlsMember::decrypt` (`mls.rs:361-401`), merging immediately at
`mls.rs:387-391` (`merge_staged_commit`); recovery merges at build time
(`mls.rs:298-300`). The chain lives in engine `State` (`chain`/`chain_head`/
`chain_applied`, `lib.rs:511,513,529`); `ChainHead.hash` is a `String`
(`chain.rs:64-83`). Layering forbids net reading engine state. Seam (mirrors
`EngineSink` injection):

```rust
// defined in molt-net, implemented by molt-engine, handed into the runtime.
pub trait ChainOracle: Send + Sync + Clone + 'static {
    /// Does a threshold-decided chain block with this hash authorize a
    /// group-data (TransportPolicy) commit? Pure over the applied chain.
    fn authorizes(&self, block_hash: &str) -> bool;
    /// Current verified head (height, hash) — for staleness / the AAD binding.
    fn head(&self) -> Option<(u64, String)>;
}
```

The runtime resolves the block-hash bound into the commit's
GroupContextExtensions/AAD via `oracle.authorizes()` **before**
`merge_staged_commit` — drop-before-merge, never merge-then-reject (avoids the
permanent epoch split §5 warns of). `ChainChange::TransportPolicy` is an
additive variant (`molt-core/src/chain.rs:69`). No new types needed.

## 6. ACK path — frame REUSE, anti-spoof pin + fan-out FORK

Decode strips `MESH_ACK_TAG` → `MlsDecode::Ack(from, window)`
(`supervisor.rs:167-172`). The **pairwise pin** `if from != peer.member { drop }`
(`supervisor.rs:1538-1545`) has no meaning on a broadcast channel → replace with
"trust the MLS credential only". `flush_due_acks` (`net.rs:2792-2848`) sends
**per-peer** via `send_ping` to each `PeerLink` (`:2833,2841`) → forks to **one**
kind-445 broadcast at the min floor (§6/finding 8). `advance_acked_floor`/
`record_acked` (per-sender floor) REUSE. Note: every member now learns every
other's acceptance state (in-group metadata change — state it).

## 7. Co-equality — REUSE (additive)

`molt-mcp/src/lib.rs:1190-1300`, test `co_equality_every_command_is_a_tool_or_
documented_internal`; `INTERNAL: [&str; 51]` (`:1237`). New net-task internals
(relay-health twin, Nostr ritual report-backs) just extend the array + bump the
length literal + add a justification comment. No violation.

## Etappe implication

N1+ builds against this map. The load-bearing order: introduce the
`TransportKind` discriminator + the enum-bypass FIRST (so resume/offline gates
read kind, not shape), then the relay runtime, then the health/ACK/presence
forks, with the `ChainOracle` seam defined in N3 (its contract + a hard-reject
test) so N6 only fills in the `TransportPolicy` block type.
