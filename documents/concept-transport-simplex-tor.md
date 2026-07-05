# Concept: communication — SimpleX (SMP) over Tor

Status: **design**. Today there is no network: the reply simulator fakes
other members, the run lifecycles fake handshakes, and `transport.*` config
is parsed but unwired. This document specifies the real transport:
**MLS-protected group messages carried as opaque payloads over SimpleX
Messaging Protocol (SMP) queues, reached exclusively through Tor**
(per the docs' architecture: MLS ▸ SMP ▸ Tor/Nym; Nym is a later backend
behind the same trait).

## 1. Why this stack (one paragraph each)

* **SMP** is a minimal relay protocol: unidirectional message queues on
  untrusted servers, created by the *recipient*, addressed by random ids —
  no accounts, no user identifiers, sender and recipient of one queue are
  never linkable to other queues. That is exactly the “dumb, deniable pipe”
  a republic needs; we do not use SimpleX Chat, only the queue layer.
* **MLS** (already the plan of record) provides group confidentiality,
  authenticity, membership and forward secrecy. SMP's own E2E agreement is
  redundant for us: **MLS ciphertext is the SMP payload**; SMP adds hop
  privacy and store-and-forward.
* **Tor** hides network location from the SMP servers and observers. The
  config already models it (`tor_mode = local | embedded | whonix`,
  `tor_port`).

## 2. Crate and process architecture

New crate **`molt-net`** (layering: beside the engine, never above it):

```
┌────────────┐   NetCmd (mpsc)    ┌─────────────────────────────────────┐
│ engine     │ ─────────────────▶ │ molt-net supervisor task            │
│ actor      │ ◀───────────────── │  ├─ SocksDialer (tor)               │
└────────────┘   Command::Net*    │  ├─ per-queue RecvTask (SUB loop)   │
   (internal, like ticks)         │  ├─ OutboxTask (send, retry)        │
                                  │  └─ MlsTask (group state machine)   │
                                  └─────────────────────────────────────┘
```

* The engine stays the single state owner. The transport talks to it only
  through **internal commands** (`NetDelivered`, `NetPeerSeen`,
  `NetSendFailed`, …), exactly the pattern the tickers and `ChatFrom` use —
  they join the documented INTERNAL list of the co-equality test (a network
  peer must not be impersonatable through the MCP surface).
* Outbound: the engine appends the event to the workspace log **first**
  (storage concept §6 — the log is the outbox source of truth), then hands
  the envelope to `molt-net`.
* The core abstraction is a trait, so the entire engine/UI/E2E test stack
  runs without a network:

```rust
/// molt-net
#[async_trait]
pub trait Transport: Send + Sync {
    async fn create_queue(&self) -> Result<QueuePair, NetError>;      // recv side
    async fn send(&self, addr: &SndQueueAddr, blob: Blob16K) -> Result<(), NetError>;
    fn subscribe(&self, q: &RcvQueue) -> BoxStream<'static, Delivery>;
    async fn delete_queue(&self, q: &RcvQueue) -> Result<(), NetError>;
}
```

Implementations: `LoopbackTransport` (in-process; **replaces the current
reply simulator** — simulated members become loopback peers driving real
code paths), `SmpTransport` (the real thing), later `NymTransport`.

## 3. SMP specifics

### 3.1 Servers and addressing

* Server address format follows SimpleX: `smp://<fingerprint>@<host>[:5223]`
  — `fingerprint` pins the server's offline certificate; TLS (1.3, ALPN
  `smp/1`) is verified **against the pinned fingerprint only**, no WebPKI
  (CAs are irrelevant and a metadata leak via OCSP). `.onion` hosts
  preferred; clearnet hosts allowed but always dialed through Tor anyway.
* Each member configures ≥2 servers (their own choice — server choice is
  per-*queue recipient*, one more unlinkability degree). Defaults ship in
  config; the restore-“Social peer-restore” UI already speaks `smp://`.

### 3.2 Queues

* A **connection** between two members = two unidirectional queues (one per
  direction), each living on the *recipient's* chosen server.
* Queue lifecycle commands used: `NEW` (create; returns recipient id +
  sender id + per-queue keys), `KEY` (secure), `SUB` (long-lived subscribe),
  `SEND`, `ACK`, `OFF`/`DEL` (retire). We keep the standard SMP command
  set — no forks — so any public SMP relay works.
* **Uniform blocks**: SMP transports fixed-size blocks (16 KiB, padded).
  Our framing MUST always fill to block size (padding is not optional);
  larger MLS messages are chunked with a tiny reassembly header inside the
  encrypted payload.
* **Queue rotation**: long-lived queues are linkability surface. Rotation
  policy: per queue after ~1 000 messages or 7 days — create successor,
  announce in-band (MLS application message), drain, `DEL`.

### 3.3 Mapping the republic onto queues

MLS gives one logical group; SMP gives pairwise pipes. v1 uses
**full-mesh fan-out**: a group message is MLS-encrypted once per epoch key
and sent to each member's inbound queue (n−1 sends). At republic scale
(n ≤ 13 by our own create rule) this is entirely adequate and has the best
metadata properties (no super-node). A relay/fan-out member (“post office”)
is a later optimization slot, not v1.

**Invite flow — making today's mock real.** The `molt://invite/…` link
grows the real payload: `{ invite-queue SndQueueAddr, inviter MLS
KeyPackage, workspace id, m-of-n }` (the current human fields stay for the
preview). Join = send `JoinRequest{my KeyPackage}` to the invite queue →
inviter's node runs the MLS Add/Commit → sends Welcome + per-member queue
addresses back on a fresh pair. The three phases the join-run mock already
displays (“contacting inviter / receiving MLS welcome / syncing surfaces”)
become the *actual* state machine states — the UI does not change.

### 3.4 Delivery semantics

* **At-least-once** from SMP (`ACK` after local durable append), dedup at
  the MLS layer (epoch, sender, generation) — the engine sees each event
  exactly once; the dedup set is persisted in the workspace log envelope
  metadata.
* **Ordering**: MLS enforces sender-order; cross-sender order is resolved by
  the engine's existing rule (arrival order at the actor — the single-owner
  design makes this deterministic locally; the eventual CRDT-ish
  reconciliation for gated surfaces is the R-plan's concern, unchanged).
* **Offline**: SMP servers store-and-forward (bounded). Our own outbox
  (workspace log) retries sends with jittered exponential backoff
  (1 s → 2 min cap) until acked; `MemberSeen` checkpoints feed the presence
  strip — the members pills finally show real liveness.

## 4. Tor integration

| `tor_mode` | Mechanism |
|---|---|
| `local` | SOCKS5 to `127.0.0.1:<tor_port>` (system tor / Tor Browser). Health check at startup (SOCKS handshake + descriptor of a known onion); clear notice when absent. |
| `embedded` | **arti** (`arti-client`, pure Rust — keeps the no-C-toolchain posture) bootstrapped in-process; state dir under `~/.moltrepublic/arti`. |
| `whonix` | SOCKS5 to the gateway (`10.152.152.10:9050` default, host overridable); DNS categorically never local. |

* **Stream isolation is mandatory**: every SMP server connection uses its
  own SOCKS auth pair (`user=molt-<random>` per queue-host) ⇒ Tor puts them
  on separate circuits ⇒ two queues of ours never share an exit/timing
  fingerprint. With arti: `IsolationToken` per queue-host.
* No DNS anywhere: onion addresses resolve in-circuit; clearnet SMP hosts
  resolve via SOCKS5h (proxy-side resolution).
* Failure honesty: “tor unreachable” is a first-class state — the header's
  `chat` pill goes amber/red with the reason (the pills already exist).

## 5. Concurrency & parallelism

* **Supervisor task** owns the transport instance and restarts children
  with backoff; children are pure tokio tasks communicating by channels —
  no shared mutable state, same discipline as engine/store.
* **Per-queue RecvTask**: one long-lived SUB per inbound queue (n−1 tasks +
  invite queue). Cheap: they are parked awaiting frames.
* **OutboxTask**: single consumer of the send queue; fan-out sends per
  message run **concurrently** (`join_all` over members) but ack/bookkeeping
  is serialized in the task — per-member ordering preserved via per-member
  sub-queues.
* **Circuit prebuilding**: dial SOCKS/build circuits for all member hosts at
  workspace-open in parallel (bounded by a semaphore of 4) so first send
  latency is one round-trip, not one circuit build.
* **MlsTask** serializes group-state mutations (MLS state is as
  order-sensitive as the engine's), lives beside — not inside — the engine
  actor so crypto work (Welcome processing ~ms) never blocks command
  handling.
* Backpressure end-to-end: bounded channels engine→net (drop-oldest never;
  sender awaits), net→engine (engine is fast; bounded 1 024 with lag
  warning) — the same explicit-degradation stance as the storage concept.

## 6. Security notes (delta to the obvious)

* MLS credentials: per-group fresh identities (docs' rule “fresh-per-group”)
  — the KeyPackage carries no cross-group linkable identifier.
* SMP servers learn: queue ids, sizes (uniform), timing, IP of a Tor exit
  or their own onion service. They never learn membership, group size (mesh
  sends arrive on unlinkable queues), or content.
* Local threat: all transport state (queue keys, dedup sets, MLS state)
  persists **inside the encrypted workspace log** — no second plaintext
  database appears.
* Panic on downgrade: if a configured server stops offering the pinned
  fingerprint, the queue is marked poisoned and traffic stops — no TOFU
  re-pin without explicit user action.

## 7. Testing

The trait boundary is the strategy: everything above `Transport` tests
without sockets, everything below tests without the engine.

* **Loopback tier (default `cargo test`)**: `LoopbackTransport` with an
  injectable chaos policy — delay distribution, reorder, duplicate, drop,
  partition. Property tests: for any chaos seed, all members converge to
  identical engine state (the deterministic actor makes “converged” a
  strict equality); dedup holds under duplication; outbox drains after
  partitions heal. The existing reply simulator is retired by making the
  simulated members real loopback peers — the demo becomes a 3-node network
  in one process.
* **Protocol tier**: SMP framing/padding round-trip property tests (every
  block exactly 16 KiB); fuzz targets on the SMP response parser and the
  chunk reassembler (server input is untrusted).
* **Integration tier (feature-gated `net-tests`, CI job)**: dockerized
  reference `smp-server`; run NEW/KEY/SUB/SEND/ACK against it over plain
  TCP+TLS: queue lifecycle, rotation, server-restart recovery, bounded
  offline storage behavior.
* **Tor tier (nightly CI, allowed-to-be-slow)**: the same suite through a
  local tor (`chutney` test network or a single local tor with an onion
  smp-server); asserts: no direct-dial ever happens (egress firewall in the
  test container fails the build on any non-SOCKS connection — the
  strongest possible “we never leak” test), stream isolation (distinct
  SOCKS auth observed per queue-host).
* **E2E**: two `moltd` processes over loopback-tor (or plain loopback
  transport in CI): found on node A, invite, join from node B over MCP,
  chat both ways, kill B mid-chat, restart, catch-up completes — all
  through the existing MCP harness, screenshots optional.

## 8. Milestones

1. **T1** `molt-net` crate: `Transport` trait + `LoopbackTransport` + chaos
   harness; engine wired; reply simulator replaced by loopback peers.
   *(no sockets yet, but the whole app now runs on real transport paths)*
2. **T2** MLS integration (OpenMLS) behind `MlsTask`; invite/join state
   machine real over loopback.
3. **T3** `SmpTransport` over TCP+TLS with fingerprint pinning; docker
   integration tests.
4. **T4** Tor `local` mode via SOCKS5h + stream isolation; the no-leak
   egress test.
5. **T5** `embedded` (arti) and `whonix` modes; header pills wired to real
   transport health.
6. **T6** queue rotation, presence checkpoints, backpressure polish, soak
   tests.
