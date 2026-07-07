# Concept: communication — SimpleX (SMP) over Tor

Status: **T1 + founding ritual + T2 (MLS, incl. the live runtime mesh) + T3
(real SMP)** (2026-07-08). MLS (OpenMLS 0.8, pure-Rust) is integrated into the
founding ritual AND the **running post-founding traffic is now real MLS
ciphertext over a live direct mesh**: after founding, members bootstrap a
per-pair-queue full mesh in-band over MLS and chat peer-to-peer, encrypt-once
fanned out; a clean close persists the advancing ratchet + the SMP queue
credentials so a reopen **resumes** the mesh (concept §6, clean-close variant;
per-drain write-ahead is the remaining crash-safety hardening). Fan-out privacy
jitter is on. **Still open in T2: recovery rejoin + the recovery-invite/approval
UI.** **T4 (Tor) is entirely open — SMP currently dials clearnet TCP directly,
not through Tor.** T5–T6 open (queue rotation, arti/whonix, multi-server,
ack-after-fsync, fuzz/CI tiers). See the milestone notes below for exact
real-vs-open.
What exists: `molt-net` (Transport trait, uniform-block framing with
named-constant budget math, mandatory per-queue wrapping,
chunker/reassembler with `(msg id, chunk idx)` dedup, `LoopbackTransport`
+ seeded chaos harness, per-node supervisor with log-backed outbox /
delivery cursors / fan-out jitter / backoff and per-sender in-order
inbound; **invite module**: single-use tickets, `HMAC(KDF(ticket),
name‖pk)` join MAC, `RitualMsg` wire); the engine is wired (record →
publish + coalescing wakeup, internal `NetDelivered` / `NetPeerSeen` /
`NetSendFailed` on the INTERNAL list, passive presence on the member
pills); `transport.state` is real (encrypted sub-key file, atomic
rewrite, cursors survive restarts); the reply simulator is retired — demo
members are loopback peer nodes with their own engine instance and
transport endpoint.

**The founding ritual is real** (§3.3): the workspace is created only
when the republic is fully constituted AND sealed. `CreateStart` derives
the founder's Ed25519 identity from their recovery phrase, mints a
single-use invite per seat, and opens a transport pair per seat. Members
— simulated loopback nodes today, the identical member-side code path
once T3 lands — derive their **own** identity from their **own** recovery
phrase, activate the link (`JoinRequest`, MAC-bound to the ticket, name
delivered), then sign the final canonical roster table (`SealSigned`).
Only when every seat is sealed does the engine write the `Founded`
genesis, carrying the complete `identities` table AND all n
`attestations` — the member list is signed by everyone from birth (no
"constituted but not sealed" state). No open seats; the fake founding
animation is gone; every ritual leg is a real event in the wizard's live
log. Approval is automatic during founding (ticket+MAC), manual for
recovery. Founding invites are ephemeral — cancel/crash before sealing
voids the links and leaves the disk untouched.

Honest deltas, to be closed where noted:

* **Founding members are simulated over loopback** (`prefs.simulated_members`),
  not yet real remote nodes; the crypto (ticket MAC, per-phrase identity
  derivation, seal signatures over the canonical table) is real, only the
  transport is in-process until T3. The visible invite link shows the
  human preview fields; the full queue/wrap-key handover payload is passed
  in-process and gets encoded into the link at T3.

* **Convergence is per-sender.** Delivered event *sets* and per-sender
  order converge across nodes; identical cross-sender ordering (and with
  it index-safe cross-node quotes/reactions) needs stable message ids and
  the reconciliation rule — lands with T2/R-plan. Until then only `Chat`
  events cross the wire; index-referencing events stay node-local and a
  transferred message's `quote` is stripped on receipt.
* **Ack-after-apply, not ack-after-fsync.** The loopback receiver acks
  once the engine accepted the event (applied + queued to the writer);
  tightening to the §3.4 fsync rule is T3 work (the writer already exposes
  the hook point).
* **The supervisor now carries MLS ciphertext** (2026-07-07): when a node has a
  group, the outbox encrypts a workspace event **once** per log seq (cached and
  reused for the n−1 fan-out copies, so the ratchet advances a single time) and
  the recv side decrypts to the authenticated sender + envelope (MLS itself
  rejects replays, so the per-link reorder/dedup path is bypassed). Proven
  end-to-end over loopback (`tests/mls_supervisor.rs`: one encrypt fans out to a
  3-member group, all decrypt). The demo mesh (sim peers, no group) keeps the
  plaintext WireFrame path unchanged.
  **Done (2026-07-08):** the **runtime full-mesh is live in the product**. After
  founding, each node opens per-pair inbound queues and announces the
  address+wrap-key handovers to the group in-band over MLS (post-founding MLS
  bootstrap + per-pair queues), assembles its `PeerLink`s, and stands a real
  supervisor up whose outbox is the encrypted workspace log — members chat
  peer-to-peer over MLS with no founding star and no demo peers
  (`two_instances::founding_chats_over_the_direct_mesh`). A **clean close**
  snapshots the advanced ratchet + serializes the SMP queue credentials into
  `transport.state` (a blocking storage merge that preserves the delivery cursors
  and seals the state so a wind-down save cannot clobber it); on reopen the node
  re-adopts the creds into a fresh transport and resumes the mesh.
  **Open:** the ratchet persist is **clean-close only** — the §6 *write-ahead*
  rule (fsync the ratchet before its ciphertext leaves) for full crash-safety is
  the remaining hardening; a hard crash resumes from the last-persisted ratchet
  (at most replay-rejection, never nonce reuse — MLS `reuse_guard`).

The rest of this document is the design as specified; sections below are
unchanged targets. Today the run lifecycles still fake handshakes and
`transport.*` config remains unwired (T4/T5). This document specifies the
real transport:
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
joiner sends `JoinRequest{ my display name, my identity pk (KeyPackage
once MLS lands), mac }` with `mac = HMAC(KDF(ticket), name ‖ pk)`. The
inviter's node verifies the MAC against the unspent ticket — a bare leaked
queue address is not enough to knock, and a replayed or reused ticket is
rejected outright (the invite queue is otherwise an open spam surface).

**Approval: automatic during founding, manual for recovery** (decision
2026-07-05). During the founding ritual a valid ticket+MAC turns the seat
green without a founder click — the founder just distributed those links
himself and is watching the list live; the single-use ticket carries the
trust. A **recovery** join stays approved-not-automatic: a valid request
surfaces on the minting member's node as an approval prompt (name +
key fingerprint), and only an explicit accept proceeds — whoever holds a
leaked recovery link still does not get in.

The three phases the join-run mock already displays ("contacting inviter /
receiving MLS welcome / syncing surfaces") become the *actual* state
machine states; joiner-side the UI does not change. Phase 1 honestly covers
the whole first leg: request queued on the invite queue
(store-and-forward — the **inviter may be offline**; the joiner sees
"contacting inviter" with elapsed time, not a fake spinner), then awaiting
the inviter's manual approval. Only the inviter's accept advances to
phase 2.

**Invite lifecycle, persistence & UI.** Membership is fixed at founding —
n never changes afterwards, and **there are no open seats**: a workspace
only ever exists with its complete, sealed member list (see the founding
ritual below). There are exactly two kinds of invite:

* **Founding invite** — minted by the founder inside the (pre-creation)
  founding ritual, one per future member; activating it delivers the
  member's name and identity key and, once everyone signed, the member is
  in the genesis. No approval click (decision above), no seat proof — no
  key is anchored yet: the single-use ticket MAC carries the trust, and
  the ritual is exactly the moment every identity key becomes anchored.
  Founding invites are **ephemeral**: they belong to one ritual attempt
  and die with the wizard (decision 2026-07-05) — cancel or crash before
  completion voids every distributed link and leaves the disk untouched;
  a new attempt mints fresh links. Nothing invite-related persists
  anywhere, because there is no workspace yet to persist into.
* **Recovery invite** — minted by *any* member, for an already-filled seat
  whose holder lost their workspace but still holds their recovery phrase.
  The minting member is the approver. MLS: Remove(old leaf) + Add(new
  KeyPackage) in a single commit; the shared log records the additive
  event variant `MemberRestored { member }`, and the rejoined node
  re-syncs the full workspace. Division of labor: the **phrase**
  re-derives the member's identity key, which answers the seat proof
  (below); the **recovery link** contributes only the transport path and
  the ticket (= the challenge); the human check stays where it is — the
  manual approval.

**Member identity & seat proof.** Every member — founder and joiners
alike — holds their *own* secret recovery phrase; from it the node derives
a per-workspace **identity keypair** (Ed25519, via the storage concept's
HKDF hierarchy, §5 there; per-workspace derivation keeps the
fresh-per-group rule — nothing cross-group linkable). The private key is
cached in `transport.state` for day-to-day signing; the phrase re-derives
it after total loss — "all keys derive from this phrase", the restore
screen's standing rule, now holds for *every* member, not only the
founder (today's "the joiner keeps no recovery phrase" is hereby a mock
artifact, like the once-only link display). The *public* keys are anchored
in the genesis event's identity table (below); from T2 on the same key is
the identity inside the member's MLS KeyPackage credential — one
identity, two anchors, and the verifying node checks that they match.

**The founding ritual precedes the workspace** (decision 2026-07-05,
supersedes the earlier genesis-first staging). Nothing touches the disk
until the republic is fully constituted AND sealed; the wizard hosts the
whole ritual:

1. **Configure** — name, the founder's handle, m-of-n.
2. **Mint** — the founder's recovery phrase is generated (shown once);
   the workspace id and the founder's identity key derive from it. Per
   future member: a high-entropy single-use ticket, an invite queue with
   a fresh wrapping key, and the `molt://invite/…` link carrying
   `{ ws id, m-of-n, queue address, wrapping key, ticket }`.
3. **Distribute, off-band** — the wizard shows the member list: the
   founder (green) plus one row per invite (link + copy). Links travel
   over private channels.
4. **Collect keys** — each member's node generates its *own* recovery
   phrase (shown once to them), derives its per-workspace identity key
   from it, and activates the link: `JoinRequest{ name, identity pk,
   reply queue, mac }` on the invite queue. Valid ticket+MAC turns the
   row live automatically and spends the ticket; the display name is the
   member's own choice, delivered with the activation.
5. **Seal** — when the last key is in, the founder sends the final
   canonical table (ws id, rule, ordered name → pubkey) to every member
   over their reply queue; each returns
   `sig = Sign(identity_sk, canonical table)`. A row is **green** only
   once its signature verified. The founder signs the same bytes locally.
6. **Genesis, only now** — with all n signatures in hand the workspace
   directory is created; the `Founded` event (seq 1) carries the rule,
   the final roster, the identity table AND all n attestations. The
   member list is immutable and signed by everyone **from birth** — there
   is no "constituted, not yet sealed" intermediate state, ever.
7. **Enter** — "Enter republic" unlocks only when every row is green
   (equivalently: once genesis exists).

Every ritual step lands in the wizard's live log (real events: link
activated, name received, key received, signature verified, workspace
created — the fake founding animation is retired). After sealing,
membership never changes: recovery replaces an MLS leaf, never a name or
a pubkey. Until the real network exists (T3), the activating members are
simulated loopback nodes driving the identical member-side code path:
own phrase, real key derivation, real JoinRequest/seal signature over
real queues.

Recovery is then a **challenge–response against the anchored key** — the
fresh single-use ticket in the recovery link *is* the challenge. The
rejoining node re-derives its identity key from its phrase and sends
`JoinRequest{ KeyPackage, mac, seat_sig }` with
`seat_sig = Sign(identity_sk, ticket ‖ KeyPackage ‖ workspace id)`.
Non-interactive on purpose: the proof survives the store-and-forward,
approver-may-be-offline first leg, and replay is dead because the ticket
is spent on first use. The approver's node verifies `seat_sig` against the
pubkey the log anchors for that seat and only then surfaces the approval
dialog; a request with a missing or invalid seat proof never reaches the
human.

**Lost phrase = lost seat (v1, deliberate).** A member who lost workspace
*and* phrase cannot rejoin. There is deliberately no override — founder
fiat or a quorum reseat would be a second, weaker rejoin path that
devalues the proof. A governance-gated reseat (m-of-n proposal) is a
later, separate concept point; until then the seat stays visibly dead.

Ticket lifecycle (recovery invites): **minted → shared → spent |
re-minted**. Re-minting voids the predecessor ticket; at most one valid
ticket exists per seat at any time. Unspent *recovery* invite material
lives node-locally in the **minter's** `transport.state` (storage concept
§3.5); losing that file loses links, never seats. Founding invites have
no persistence story at all — they are ephemeral to the ritual (above).

Where the UI shows invites (target state):

1. **Founding wizard** — the ritual lobby: member list (founder green,
   one row per invite with link + copy, rows turning green through
   key + signature) above the live ritual log, plus the "share each once,
   over a private channel" hint. Links exist nowhere else, ever.
2. **Filled seats, any node** — context action **Issue recovery link**:
   mints a recovery invite, copies it, toasts; hint "share only with the
   affected member, over a private channel".
3. **Approval surface** (recovery joins only): header notice plus dialog
   on the approver's node — display name, key fingerprint, target seat,
   the **seat-proof verdict** (signature verified against the seat's
   anchored identity key; requests failing the proof are dropped before
   any UI). Accept / Reject.
4. **Joiner side** — the join wizard gains one step: it reveals the
   joiner's *own* recovery phrase (generated locally, shown once — the
   same contract the founding wizard already has for the founder).
   Recovery runs through the existing restore screen's "Social
   peer-restore" path: recovery phrase + recovery link (placeholder
   `molt://invite/…`), then the same three-phase run.
5. **MCP co-equality** — the recovery verbs exist as operator commands:
   `mint_recovery_invite(member)`, `approve_join` / `reject_join`; the
   ritual state is readable via `read_session`. The approval verbs are
   ordinary operator commands, **not** on the INTERNAL list — approving
   is a human decision and must be reachable from both surfaces.

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
* Member identity keys (§3.3): per-workspace Ed25519 pairs derived from
  each member's own recovery phrase (storage concept §5) — so
  fresh-per-group holds for them too. The private key is cached in
  `transport.state`; only public keys ever enter the shared log
  (`MemberKey`, `RosterAttested`). The phrase is the sole recovery path —
  no escrow, no reset (§3.3, "lost phrase = lost seat").
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
  end over loopback, including the inviter-offline wait, rejection of
  reused or forged tickets, and rejection of recovery requests with a
  missing or invalid seat proof; the founding ritual seals (all
  `RosterAttested` present) once the last loopback peer joins. The existing reply simulator is retired by
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
   not scripted repliers.)* — **done 2026-07-05**, deltas in the status
   note at the top.
2. **T2** MLS integration (OpenMLS 0.8, pure-Rust `openmls_rust_crypto`).
   **Done (2026-07-07):** the MLS group is **born in the founding ritual** —
   the joiner's `KeyPackage` rides its `JoinRequest`, the founder builds the
   group at sealing and ships the one `Welcome` with the genesis distribution,
   and every node persists its own group state (`MlsMember` snapshot) into
   `transport.state.mls`; the derived Ed25519 identity is the MLS credential
   (one identity, two anchors). Proven interoperable across two independent
   engine instances (loopback + real SMP). The **deliberation step** landed
   with it (§3.3): after every seat joins, the founder proposes the final DAO
   name + a free-text charter (agenda), which is bound into the canonical
   bytes so every member's seal signature ratifies it, and the joiner's node
   gates its signature on an explicit human confirm before the workspace
   opens; the charter is immutable in the `Founded` genesis.
   **Done (2026-07-08):** the running post-founding traffic is encrypted with the
   group over a **live direct mesh** (bootstrap over the founding star → per-pair
   queues → real supervisor over the workspace log), and a clean close persists
   the ratchet + queue creds so a reopen resumes it (see the delta note above).
   **Open:** recovery rejoin (Remove+Add, `MemberRestored`) and the
   recovery-invite UI (mint / copy / re-issue) with the manual approval surface;
   the §6 write-ahead outbox (full crash-safety, currently clean-close only).
3. **T3** `SmpTransport` over TCP+TLS with fingerprint pinning; docker
   integration tests.
4. **T4** Tor `local` mode via SOCKS5h + stream isolation; the no-leak
   egress test.
5. **T5** `embedded` (arti) and `whonix` modes; header pills wired to real
   transport health.
6. **T6** queue rotation (incl. drain grace), passive-presence polish,
   fan-out jitter tuning, soak tests.
