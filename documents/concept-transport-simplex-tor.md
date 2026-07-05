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
  authenticity, membership and forward secrecy. **MLS ciphertext is the SMP
  payload**; SMP adds hop privacy and store-and-forward. One thing MLS does
  *not* give us: a group message is **one** ciphertext fanned out to n−1
  queues, so all copies are byte-identical — a server hosting two members'
  queues would link them into a group at a glance. Every copy is therefore
  wrapped per queue before padding (§3.2). That wrapping takes the place of
  SMP's own E2E layer — which is thus not "redundant" but repurposed: its
  job here is *unlinkability of copies*, not confidentiality.
* **Tor** hides network location from the SMP servers and observers. The
  config already models it (`tor_mode = local | embedded | whonix`,
  `tor_port`).

## 2. Crate and process architecture

New crate **`molt-net`** (layering: beside the engine, never above it):

```
┌────────────┐  wake + NetCmd(ctl) ┌─────────────────────────────────────┐
│ engine     │ ─────────────────▶  │ molt-net supervisor task            │
│ actor      │ ◀─────────────────  │  ├─ SocksDialer (tor)               │
└────────────┘   Command::Net*     │  ├─ per-queue RecvTask (SUB loop)   │
   (internal, like ticks;          │  ├─ OutboxTask (send, retry)        │
    outbound data rides the        │  └─ MlsTask (group state machine)   │
    workspace log, not a channel)  └─────────────────────────────────────┘
```

* The engine stays the single state owner. The transport talks to it only
  through **internal commands** (`NetDelivered`, `NetPeerSeen`,
  `NetSendFailed`, …), exactly the pattern the tickers and `ChatFrom` use —
  they join the documented INTERNAL list of the co-equality test (a network
  peer must not be impersonatable through the MCP surface).
* Outbound: the engine appends the event to the workspace log **first**
  (the log is the outbox source of truth — storage concept §3.5), then only
  *nudges* `molt-net` with a coalescing `Notify`-style wakeup. The
  OutboxTask reads pending envelopes straight from storage: per member,
  everything with `seq > cursor(member)`; the **delivery cursors** live in
  the node-local encrypted `transport.state` (§6, storage concept §3.5).
  Two consequences: the actor **never awaits the transport** (the wakeup
  carries no data, the log does — same never-block discipline as the
  storage writer), and after a crash the outbox reconstructs itself from
  cursors vs. log, with no separate send queue to recover. The `NetCmd`
  mpsc remains for *control only* (create/delete queue, subscribe,
  shutdown) — rare, bounded, never on the message hot path.
* The core abstraction is a trait, so the entire engine/UI/E2E test stack
  runs without a network:

```rust
/// molt-net
#[async_trait]
pub trait Transport: Send + Sync {
    async fn create_queue(&self) -> Result<QueuePair, NetError>;      // recv side
    async fn send(&self, addr: &SndQueueAddr, block: PaddedBlock) -> Result<(), NetError>;
    fn subscribe(&self, q: &RcvQueue) -> BoxStream<'static, Delivery>;
    async fn delete_queue(&self, q: &RcvQueue) -> Result<(), NetError>;
}
```

`PaddedBlock` is exactly one SMP transport block (16 KiB). The *usable*
payload budget is smaller: block size minus SMP framing, per-queue wrapping
AEAD overhead (§3.2) and the chunk header. The chunker computes that budget
from named constants — nothing in the code may assume "payload == 16 KiB".

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
* **Per-queue wrapping (mandatory)**: the queue creator generates a fresh
  symmetric wrapping key per queue and hands it to the sender together with
  the queue address (in-band over MLS; for the very first pair, inside the
  invite payload). Every chunk is wrapped
  `XChaCha20-Poly1305(key_q, random nonce, chunk)` — *then* padded. The
  purpose is not confidentiality (MLS owns that) but **copy
  unlinkability**: the n−1 fan-out copies of one group message must be
  pairwise byte-distinct, otherwise any server hosting two members' queues
  links them trivially. The wrapping key rotates with its queue and lives
  in `transport.state` (§6).
* **Queue rotation**: long-lived queues are linkability surface. Rotation
  policy: per queue after ~1 000 messages or 7 days — create successor,
  announce in-band (MLS application message), then keep the old queue in
  drain until its sender demonstrably switched (first delivery on the
  successor) **or** a 14-day grace expires, whichever comes first — then
  `DEL`. A member offline past the grace re-syncs via normal catch-up on
  the successor queue, never via the dead one.

### 3.3 Mapping the republic onto queues

MLS gives one logical group; SMP gives pairwise pipes. v1 uses
**full-mesh fan-out**: a group message is MLS-encrypted once per epoch key
and sent to each member's inbound queue (n−1 sends). At republic scale
(n ≤ 13 by our own create rule) this is entirely adequate and has the best
metadata properties (no super-node). A relay/fan-out member (“post office”)
is a later optimization slot, not v1.

**Invite flow — making today's mock real.** The `molt://invite/…` link
grows the real payload: `{ invite-queue SndQueueAddr, invite-queue wrapping
key, inviter MLS KeyPackage, workspace id, m-of-n, ticket }` (the current
human fields stay for the preview). The **ticket is a high-entropy,
single-use secret** and is cryptographically bound to the request: the
joiner sends `JoinRequest{ my KeyPackage, mac }` with
`mac = HMAC(KDF(ticket), KeyPackage)`. The inviter's node verifies the MAC
against the unspent ticket — a bare leaked queue address is not enough to
knock, and a replayed or reused ticket is rejected outright (the invite
queue is otherwise an open spam surface).

**Join is approved, not automatic.** A valid request surfaces on the
inviter's node as an approval prompt (joiner's display name + KeyPackage
fingerprint); only an explicit accept runs the MLS Add/Commit and sends
Welcome + per-member queue addresses back on a fresh pair. The approver is
whoever minted the invite — the founder for seat invites, the minting
member for recovery invites (see the lifecycle below). This is a small new
inviter-side surface, in keeping with the deliberate m-of-n ethos —
whoever holds a leaked link still does not get in.

The three phases the join-run mock already displays ("contacting inviter /
receiving MLS welcome / syncing surfaces") become the *actual* state
machine states; joiner-side the UI does not change. Phase 1 honestly covers
the whole first leg: request queued on the invite queue
(store-and-forward — the **inviter may be offline**; the joiner sees
"contacting inviter" with elapsed time, not a fake spinner), then awaiting
the inviter's manual approval. Only the inviter's accept advances to
phase 2.

**Invite lifecycle, persistence & UI.** Seats are fixed at founding — n
never changes afterwards (matching today's roster derivation: founder plus
one named seat per invite). There are exactly two kinds of invite:

* **Seat invite** — minted by the founder, at founding, one per open seat;
  a completed join fills that seat (`MemberJoined`). The founder is the
  approver.
* **Recovery invite** — minted by *any* member, for an already-filled seat
  whose holder lost their workspace but still holds their recovery phrase.
  The minting member is the approver. MLS: Remove(old leaf) + Add(new
  KeyPackage) in a single commit; the shared log records the additive
  event variant `MemberRestored { member }`, and the rejoined node
  re-syncs the full workspace. Division of labor: the **seed** re-derives
  the workspace identity and keys ("all keys derive from this phrase" —
  the restore screen's standing rule); the **recovery link** contributes
  only the transport path and the ticket; the human check stays where it
  is — the manual approval.

Ticket lifecycle: **minted → shared → spent | re-minted**. Re-minting
voids the predecessor ticket; at most one valid ticket exists per seat at
any time.

Persistence: unspent invite material (ticket secret, invite-queue address,
its wrapping key) lives node-locally in the **minter's** `transport.state`
(storage concept §3.5). Losing that file loses links, never seats — seats
live in the shared log; the founder simply re-mints. Today's behavior —
links shown once in the founding wizard, then gone forever — is hereby
marked a mock artifact, not a design.

Where the UI shows invites (target state; spec for T2):

1. **Founding wizard, step 3** — unchanged: first display of the n−1
   links, per-link copy, and the existing "share each once, over a
   private channel" hint.
2. **Workspace detail, members grid** — open seats become *actionable on
   the founder's node*: clicking the chip reveals the link (elided) with
   **Copy** and **Re-issue** (confirmation required; the old ticket is
   voided on confirm). Every other node keeps rendering open seats as
   today's passive chips — it holds no ticket material, there is nothing
   it could show.
3. **Filled seats, any node** — context action **Issue recovery link**:
   mints a recovery invite, copies it, toasts; hint "share only with the
   affected member, over a private channel".
4. **Approval surface** (concretizing the manual approval above): header
   notice plus dialog on the approver's node — joiner display name,
   KeyPackage fingerprint, target seat; for recovery joins additionally a
   workspace-id-match indicator. Accept / Reject.
5. **Joiner side** — the join wizard is unchanged. Recovery runs through
   the existing restore screen's "Social peer-restore" path: recovery
   phrase + recovery link (placeholder `molt://invite/…`), then the same
   three-phase run.
6. **MCP co-equality** — the same verbs exist as operator commands:
   `remint_invite(seat)`, `mint_recovery_invite(member)`,
   `approve_join` / `reject_join`; open invites are readable in the
   status. The approval verbs are ordinary operator commands, **not** on
   the INTERNAL list — approving is a human decision and must be
   reachable from both surfaces.

### 3.4 Delivery semantics

* **At-least-once** from SMP, dedup in **two layers** because retries
  redeliver individual chunks, not whole messages. Transport layer: the
  reassembler dedups by `(message id, chunk index)`; chunks are `ACK`ed
  only after the fully reassembled message's event is durably appended
  (fsync) — a crash mid-reassembly means server redelivery, never loss.
  MLS layer: a per-sender `(epoch, generation)` window — the engine sees
  each event exactly once. The dedup window is node-local bookkeeping and
  lives in `transport.state` (§6), **not** in the shared log (two nodes'
  windows legitimately differ; the log stays replayable shared history).
* **Ordering**: MLS enforces sender-order; cross-sender order is resolved by
  the engine's existing rule (arrival order at the actor — the single-owner
  design makes this deterministic locally; the eventual CRDT-ish
  reconciliation for gated surfaces is the R-plan's concern, unchanged).
* **Offline**: SMP servers store-and-forward (bounded). Our own outbox
  (workspace log + delivery cursors, §2) retries sends with jittered
  exponential backoff (1 s → 2 min cap) until acked.
* **Presence is passive.** `last_seen(member)` derives solely from
  authenticated inbound traffic that happens anyway (application messages,
  MLS commits, rotation acks) — **no beacons, ever**: a periodic presence
  ping would be exactly the per-member traffic pattern this design spends
  Tor circuits to avoid. Presence is runtime state (optionally a timestamp
  map in `transport.state` so it survives restarts), never a broadcast
  event. The members pills show real liveness with honest staleness — a
  silently reading member appears stale, and that is correct.

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
* **OutboxTask**: single drainer of the log-backed outbox (§2); fan-out
  sends per message run **concurrently**, but each of the n−1 sends starts
  after an independent uniform **jitter from [0, 2 s]** (configurable) —
  a server hosting several member queues must not be able to correlate a
  group message by simultaneous arrival; Tor's per-circuit variance comes
  on top. Ack/bookkeeping is serialized in the task; per-member ordering is
  preserved via per-member sub-queues (jitter delays dispatch, it never
  reorders within one member).
* **Circuit prebuilding**: dial SOCKS/build circuits for all member hosts at
  workspace-open in parallel (bounded by a semaphore of 4) so first send
  latency is one round-trip, not one circuit build.
* **MlsTask** serializes group-state mutations (MLS state is as
  order-sensitive as the engine's), lives beside — not inside — the engine
  actor so crypto work (Welcome processing ~ms) never blocks command
  handling.
* Backpressure end-to-end: engine→net has **no data channel to
  backpressure** — the log is the buffer and the wakeup coalesces (§2), so
  the actor never awaits the transport (the same never-block rule the
  storage concept enforces with deferred replies). net→engine stays a
  bounded channel (engine is fast; 1 024 with lag warning); the `NetCmd`
  control channel is bounded and rare. Explicit degradation, never silent
  drops — same stance as the storage concept.

## 6. Security notes (delta to the obvious)

* MLS credentials: per-group fresh identities (docs' rule “fresh-per-group”)
  — the KeyPackage carries no cross-group linkable identifier.
* SMP servers learn: queue ids, sizes (uniform), timing, IP of a Tor exit
  or their own onion service. They never learn membership, group size, or
  content — and specifically they cannot link two queues they host into one
  group: the mesh copies are pairwise byte-distinct (per-queue wrapping,
  §3.2) **and** de-correlated in time (fan-out jitter, §5). Both defenses
  are load-bearing; neither is optional.
* Local state is **split by nature, not lumped into the log**. The shared
  history (events) is the append-only workspace log, as before. The
  crypto/transport state — MLS ratchets and secret tree, per-queue wrapping
  keys, delivery cursors, dedup windows, optional last-seen map — lives in
  **`transport.state`**: a separate encrypted file (sub-key derived from
  the workspace key), rewritten atomically, old content discarded. It must
  **not** live in the append-only log: MLS deletes key material *on
  purpose* — that deletion *is* forward secrecy — and a log that remembers
  every ratchet state would hand an attacker with the workspace key exactly
  the history MLS just burned. Still no second plaintext database.
* **Write-ahead rule** for `transport.state`: the ratchet state that
  produced a ciphertext is fsynced *before* that ciphertext leaves the
  node, and the state after processing an inbound commit is fsynced
  *before* its plaintext reaches the engine. A crash then costs at most a
  resend (dedup absorbs it) — never a nonce or ratchet reuse.
* Honesty note: atomic-rewrite deletion is logical; CoW filesystems and SSD
  wear-leveling may retain stale blocks. Full-disk encryption is the
  documented baseline assumption for forward secrecy against physical
  seizure — same posture as the storage concept.
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
  partitions heal; the invite → approval → join state machine runs end to
  end over loopback, including the inviter-offline wait and rejection of
  reused or forged tickets. The existing reply simulator is retired by
  making the simulated members real loopback peers — the demo becomes a
  3-node network in one process.
* **Protocol tier**: SMP framing/padding round-trip property tests (every
  block exactly 16 KiB); property: the n−1 fan-out copies of any one group
  message are pairwise byte-distinct (per-queue wrapping); the reassembler
  converges under chunk duplication and reordering; fuzz targets on the SMP
  response parser and the chunk reassembler (server input is untrusted).
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
   harness; engine wired (log-backed outbox, delivery cursors, wakeup);
   reply simulator replaced by loopback peers. *(No sockets yet, but the
   whole app now runs on real transport paths. Deliberately the **largest**
   milestone, not the smallest: the simulated members become full
   in-process peer nodes — own engine instance and transport endpoint —
   not scripted repliers.)*
2. **T2** MLS integration (OpenMLS) behind `MlsTask`, incl. the
   `transport.state` write-ahead discipline; invite/join state machine real
   over loopback: ticket MAC, single-use enforcement, and the inviter
   approval surface; invite persistence (`transport.state` invite table)
   with the members-grid invite UI (show / copy / re-issue), and the
   recovery rejoin through the Social-peer-restore path.
3. **T3** `SmpTransport` over TCP+TLS with fingerprint pinning; docker
   integration tests.
4. **T4** Tor `local` mode via SOCKS5h + stream isolation; the no-leak
   egress test.
5. **T5** `embedded` (arti) and `whonix` modes; header pills wired to real
   transport health.
6. **T6** queue rotation (incl. drain grace), passive-presence polish,
   fan-out jitter tuning, soak tests.
