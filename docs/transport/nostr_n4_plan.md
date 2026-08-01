# N4 execution plan — the ritual over Nostr

Status: **N4a BUILT (2026-07-31), N4b OPEN.** Executes the N4 etappe of
`nostr_transport_marmot.md` §11 on top of the N2 relay runtime
(`nostr_n2_plan.md`) and the N3 wire edge (`nostr_n3_plan.md`). Every design
input is ratified (§4.2, §10.11–10.14, ADR-0004/0005/0006); this is the
execution map. §8.3's re-anchor question — the last genuinely open product
point — was **decided by the user on 2026-07-31**: a recovered seat's new
working transport key rides the threshold-signed `Restored` chain block.
Nothing in N4a depended on that answer, and **N4b now has no open question.**

Split: **N4a = founding + join** (built), **N4b = recovery** (next pass,
planned in §8). The N0.5 inventory, re-verified against the current tree
(2026-07-31), is the seam map; stale anchors from that doc are superseded by
the ones here.

## 0. Scope

IN (N4a):

- Invite **link v2**: founder npub + invite-relay list + the FULL ticket
  (today's joinable link carries only `ticket[..10]` and cannot authenticate
  a join at all — a discovered gap, see §3).
- The gift-wrap leg: JoinRequest and the founder→joiner pre-group replies as
  NIP-59 kind-1059 wraps (rumor kind 446, §2), the founder's inbox
  subscription, and **nostr-key proof-of-possession as a side effect** (§2.1).
- The **flow restructure** §4.2 ratified: the MLS group is born at
  all-joined, the Welcome (kind-444, payload v2 with `rotation_seed` +
  relays) goes out BEFORE deliberation, and Seal/Signed/Declined/Genesis run
  as kind-445 group events (§1).
- `TransportState` v4: `kind` discriminator, `relays`, `rotation_seed`,
  `relay_cursors` (§6); exporter-ring persistence folded into the MLS
  snapshot (the N3 §5.5 debt, decided here: build it now, §6.1).
- The member-side join task (does not exist today — the engine has never
  spawned one) ending in `NetJoinSealed`, which lights the known-dark actor
  path (dispatch + persist branch) end-to-end.
- The capstone keystone: **two real engines** founding+joining over an
  in-process `MockRelay` — the first engine-level test to drive the relay
  runtime at all.
- Resume/offline honesty for Nostr workspaces (§7.5).

IN (N4b, §8): recovery link v2, gift-wrapped RecoverRequest over an
ephemeral rejoin key, the 444 recovery Welcome (payload v2 + served chain),
the carrier `created_at` to BOTH ends of `restore_member`/`decrypt_at`, the
replay-safe accept-window reset re-triggered from the recovery chain block.

NOT in N4: the running group runtime (N5 — `NostrGroupRuntime`, delivery
guarantee over 445, presence, net_health relay copy), `TransportPolicy`
blocks (N6), file transfer on Nostr (OFF, §10.7), GUI copy changes beyond
what the existing surfaces carry automatically (N6).

**Honest intermediate state, stated up front:** after N4a a founded/joined
Nostr republic materializes, persists, and reopens — but has **no live
traffic runtime until N5**. Chat written meanwhile lands in the persistent
outbox log (at-least-once semantics deliver it when N5 stands the runtime
up), `net_health` reports Down with a "relay runtime lands with N5" notice,
and read-receipt dots stay honest (undelivered). This replaces today's
`NO_TRANSPORT_YET` refusal for create/join; recovery keeps failing honestly
until N4b.

## 1. The flow restructure (the one real design change)

Today (loopback): join* → charter propose → `Seal` per queue → `Signed` per
queue → founder builds MLS group + Welcome at FINALIZE → `Genesis{sealed,
welcome}` per queue.

N4 (ratified §4.2 — deliberation/ratification "run as kind-445 in the
freshly born group", which requires the group to exist first):

1. `CreateStart` → founder derives identity + self-ticket nostr anchor
   (unchanged), mints tickets, spawns the **inbox task** (subscription
   kind-1059 `#p` founder-npub over the invite relays). Links go out via the
   dormant `NetRitualLinkReady` seam once the subscription is live
   (subscribe-before-advertise, the existing rule).
2. Joiner (`JoinStart`) → member task: derive identity + ticket-salted
   nostr anchor, subscribe its OWN 1059 inbox, then publish
   `RitualMsg::Join` gift-wrapped to the founder npub. MAC v2 + 3-anchor
   validation ladder on the founder is **unchanged** (`cmd_net_join_requested`).
   `JoinAccepted` / `LinkSpent` come back as gift-wraps to the joiner's
   anchored npub — the reply-queue handover (`ReplyHandover`) has no Nostr
   analog and is not carried (`reply: None`); **the MAC-bound `nostr_pk` IS
   the reply address.**
3. **All seats joined → the group is born now** (`build_founder_mls`
   moves from finalize to all-joined): founder creates the group, commits
   `add_members` over all KeyPackages, mints the **`rotation_seed`** (32
   random bytes), and fans out ONE kind-444 Welcome per seat (payload v2,
   §4), gift-wrapped. Both sides then subscribe the group 445 channel
   (`#h = h_tag(rotation_seed, now)`).
4. Deliberation: `CreatePropose` → `Seal{proposal}` as a 445 (founder MLS-
   encrypts the same JSON, seals under the exporter secret). Member:
   `verify_seal_proposal` + human ratification exactly as today; `Signed` /
   `Declined` go back as 445s. Sender authenticity of `Signed` gains MLS
   credential binding on top of the existing signature check.
5. Finalize: founder collects attestations, seals, publishes
   `Genesis{sealed}` as a 445 (**no welcome inside** — it already went out
   in step 3). Member runs the genesis-time byte comparison unchanged and
   reports `NetJoinSealed`.

Decline still aborts the founding; the member is already in the ephemeral
MLS group, which dies with the ritual (no disk trace — unchanged). What IS
new honesty: a cancelled/declined ritual leaves opaque ciphertext events on
the invite relays (gift-wraps to fresh npubs, 445s under a fresh h-tag).
Nothing links them to anything; state it, don't hide it.

**Idempotency is a hard requirement** (N2 §3.5: at-least-once, dedup ring
4096): every actor handler already tolerates redelivery (spent-seat idempotent
ack, `sealed` spend-once, generation guards); the NEW member state machine
must too — duplicate `JoinAccepted` ignored after first, duplicate `Welcome`
ignored after joined, duplicate `Seal`/`Genesis` re-verified idempotently.

**Ritual-time h-tag windows:** publish always under the CURRENT window's
tag. The group subscription filter carries the current tag plus the adjacent
window's tag when within Δ = 1h of a UTC boundary (one filter, two `#h`
values — this implements §4.4's documented-but-unbuilt skew margin for the
ritual path), and the ritual channel re-subscribes when the window rolls
(live filters are immutable — N2 limit). Human deliberation is unbounded, so
a ritual CAN cross midnight UTC; the keystone pins it.

## 2. Wire mapping

| Leg | Today (queue) | N4 carrier |
|---|---|---|
| `Join(JoinRequest)` | invite queue | 1059 gift-wrap → founder npub, rumor kind 446 |
| `JoinAccepted` / `LinkSpent` | reply queue | 1059 gift-wrap → joiner's anchored npub, rumor kind 446 |
| MLS Welcome (+ seed, relays) | inside `Genesis` | kind-444 rumor (payload v2) in 1059 → joiner npub, at all-joined |
| `Seal{proposal}` | reply queues | kind-445 group event |
| `Signed` / `Declined` | invite queue | kind-445 group event |
| `Genesis{sealed}` | reply queues | kind-445 group event |
| `MeshAnnounce` / mesh bootstrap | founding star | **gone** — no per-pair mesh on Nostr; the 445 channel is the runtime (N5) |

No kind 443, no kind 10051, ever (ratified: no public discovery events).
Rumor kind **446** is ours (private use, no interop per §10.3): a kind-446
rumor's content is the `RitualMsg` JSON verbatim — the whole tagged-serde
vocabulary of `invite.rs` is reused unchanged. 444 keeps its N3 meaning
(Welcome), payload versioned in §4.

### 2.1 Proof-of-possession lands for free — pin it

NIP-59's seal (kind 13) is SIGNED by the rumor author; `UnwrappedGift`
verifies it on peel. N4 requires **rumor author == the `nostr_pk` claimed in
the `JoinRequest`** (after `canonical_nostr_pk` normalization) — a
JoinRequest whose wrap was not built by the holder of the claimed nostr
secret is rejected before the ticket is spent. This upgrades the N1 "chosen
and bound, never proven possessed" limit (founding_ritual §8): on Nostr the
third anchor gets PoP at join time. Keystone required (a wrap sealed by a
different key than the claimed anchor is refused), and verify during
implementation that rust-nostr's peel enforces seal.pubkey == rumor.pubkey —
if it does not, enforce it ourselves in the peel helper.

## 3. Link v2

`FoundingInvite` (engine) is re-cut; the display preview (`InviteInfo`,
core) is untouched:

```
molt://invite/<republic>/<m>of<n>/<inviter>/<ticket-prefix>/<hex(v2-handover)>
v2-handover = JSON { v: 2, ticket, npub, relays: [url, …] }
```

- **The FULL ticket moves into the handover blob** (the preview segment
  stays the 10-char display prefix). Today's joinable link cannot compute
  `join_mac` at all — production join was never reachable, so there is no
  compat to keep; the parser REJECTS a v1 handover (`server\nqueue_id\n…`)
  with an honest "link from an older build".
- `npub` is the founder's anchor in bech32 (link/UI form per §3 of the
  concept); parsed back to the canonical hex form immediately.
- `relays`: the invite-relay list, each entry `normalize_relay_url`-clean.
  Length-capped (MAX_URL_LEN each, ≤8 relays) — a link is untrusted input.
- **ADR-0004 applies to link-carried relays on the JOINER**: `JoinStart`
  normalizes each, then requires the joiner's own pool to have the relay
  added+confirmed (and clearnet/local session-unlocked). Missing → honest
  failure naming the relay ("add and confirm it first"). No silent dial of
  a URL somebody pasted into a link. (GUI affordance to one-click-add from
  the join wizard is N6.)
- `RecoveryInvite` v2 is the same shape (§8) with `republic_id` kept.

## 4. The 444 Welcome payload v2

Rumor content becomes versioned JSON (was: bare hex of the MLS Welcome):

```
{ "v": 2, "welcome": <hex MLS Welcome>, "rotation_seed": <hex 32B>, "relays": [url, …] }
```

- Satisfies §4.2 finding 9 verbatim: relay list and h-tag material are
  "delivered only inside the authenticated Welcome, never before". The same
  payload shape serves N4b recovery.
- Authentication chain: the wrap is NIP-44 to the joiner; the peeled rumor
  author must equal the founder npub from the link; the MLS Welcome itself
  only admits the KeyPackage the member minted (`join_from_welcome`). A
  wrong `rotation_seed`/relay list from a malicious founder is transport
  denial, not a governance/read break — same trust class as today's
  founder-supplied queue handovers.
- **Decision recorded (search-first rule):** Marmot/MDK carries group
  relays + group id in an MLS GroupContext extension (`NostrGroupData`,
  `mdk_evaluation.md` §2). We deliberately do NOT adopt it in N4: no
  interop goal (§10.3), it drags the openmls unknown-extension API surface
  into scope, and recovery would still need a side channel for the served
  chain. Trade-off accepted: the seed/relays are per-delivery payload, not
  group state — each Welcome sender must supply them (founder at founding,
  coordinator at recovery, both of whom hold them). Revisit only if N6's
  TransportPolicy work wants the relay list inside MLS group state.
- Size honesty: NIP-44 caps plaintext at 65408 (rust-nostr send-side, N0
  canary). The MLS Welcome carries the ratchet tree in-band and grows with
  n; the wrap helper measures and refuses LOUDLY over the cap (a too-big
  republic fails at founding with a real error, not a relay mystery).
  Keystone with a fat synthetic payload.

## 5. The `RitualChannel` seam

`run_ritual_member<T: Transport>` holds the member state machine the tests
pin (order: accept → seal-verify → ratify → sign → genesis byte-check). N4
must not fork that logic. Extraction:

- A message-shaped seam, NOT the queue trait (§4.1's rule applied to the
  ritual): `RitualChannel { async send(RitualMsg), async recv() ->
  RitualMsg }` — implemented by (a) the existing queue framing
  (wrap/chunk/reassemble; behavior-identical refactor, loopback tests stay
  green) and (b) the Nostr channel.
- The Nostr member channel owns **two `RelayRuntime`s** (N2 limit: one
  shared cursor map per runtime — a shared 1059-inbox + 445-group runtime
  breaks the 48h NIP-59 overlap; verdict from the re-verified map): the
  inbox runtime (1059 `#p` us) and, from Welcome on, the group runtime
  (445 `#h`). `recv()` merges both: peel 1059→446 rumors; open 445s via
  exporter + `group.decrypt_at(wire, event.created_at)` — **the carrier
  stamp is threaded from day one** on every 445 ingest, closing the
  N3 `NO_CARRIER_STAMP` receive side for ritual traffic.
- The founder side gets the mirror: `SeatRuntime`'s reply legs and the
  fan-out sends (`maybe_seal`, `distribute_genesis`, acks) go through a
  founder channel enum (Loopback queues | Nostr {inbox runtime, group
  runtime, per-seat npubs}). The founder recv loop maps peeled messages
  onto the SAME internal Commands as today (`spawn_founder_recv`'s ladder).
- `RitualTransport` (the queue enum) gains **no Nostr arm** — the Nostr
  ritual path bypasses the queue trait entirely. `delivery_guarantee.rs`'s
  irrefutable `Loopback` destructuring stays valid; nothing SMP-shaped
  returns.

## 6. TransportState v4 + TransportKind

Additive (`#[serde(default)]`), version 3→4:

- `kind: Option<TransportKind>` — new core enum, one variant `Nostr` for
  now; `None` = legacy queue-shaped state (loopback tests, old files).
  N0.5's load-bearing order honored: the discriminator lands FIRST so
  resume/offline gates read kind, not shape.
- `relays: Vec<String>` — the workspace's group relay list (normalized).
  The GLOBAL pool (`SessionSettings.relays`) stays the operator's dial
  policy; the workspace list records what the group agreed on (until N6's
  TransportPolicy governs changes). Dialing still goes through
  `relay::dialable`-style gating — a workspace relay the operator has not
  confirmed is not dialed silently.
- `rotation_seed: Option<Vec<u8>>` — 32 bytes, secret-class like
  `nostr_sk` (transport.state is at-rest sealed).
- `relay_cursors: BTreeMap<String, u64>` — per-relay `created_at` floors
  (`RelayRuntime::cursors()/with_cursors` shape). Unused until N5's runtime
  but persisted from the start so the format doesn't churn twice.
- `mesh` stays `[]` and `smp_queues` stays `None` for Nostr workspaces.

Byte/compat pins: a v3 file (no new fields) loads with `kind: None`; a v4
file round-trips; the resume gate classifies all three shapes (legacy
queue, Nostr, import-detached) correctly.

### 6.1 Exporter-ring persistence (N3 §5.5 debt — decided: build now)

The ring is runtime-only today (`snapshot()` serializes the provider map
only; restore re-fills empty — re-verified in `mls.rs`). N4 makes the
snapshot a versioned wrapper `{ provider_map, exporter_ring }` with a
fallback: a legacy blob (bare bincode map) restores with an empty ring,
byte-for-byte as today. Rationale: N4b/N5 recovery + catch-up lean on the
ring across restarts, and the snapshot format is already being touched by
nothing else — one versioned change now beats a second format bump in N5.
Keystone: snapshot→restore preserves the ring; a legacy blob still loads.

## 7. Engine wiring (N4a)

### 7.1 Founder path

`cmd_create_start` (production branch, replacing the `NO_TRANSPORT_YET`
return): resolve the fail-closed dialer (`dialer_for()`), take
`relay::dialable(pool, clearnet_session)` — empty → honest create failure
("no confirmed dialable relay"). Then as today minus queues: tickets +
preview links synchronously; spawn the **founder inbox task** (own
`RelayRuntime`, 1059 `#p` founder-npub); when its subscription is live it
reports the v2 links through `NetRitualLinkReady` (first real emitter;
provisioning failure → `NetRitualFailed`, also first real emitter). The
inbox loop peels wraps → the existing Command ladder (`NetJoinRequested`
etc.). PoP check (§2.1) happens in the peel helper before the Command is
sent.

All-joined: the actor (synchronous, no I/O) builds the MLS group + seed and
hands welcome fan-out + group-subscription startup to the spawned channel
task. Seal/Genesis legs publish 445s through it. `cmd_net_seal_signed`,
`maybe_finalize`, `finalize_founding` are logic-unchanged; materialize
writes TransportState v4 (`kind: Nostr`, relays, rotation_seed, mesh `[]`),
and `spawn_founder_bootstrap`/mesh machinery is simply not entered for
Nostr rituals (no `NetMeshReady`; the create run completes at genesis
distribution + materialize).

### 7.2 Member path

`cmd_join_start` (replacing the `NO_TRANSPORT_YET` tail): parse link v2,
gate the link relays (§3), then spawn the member task = the extracted state
machine over the Nostr `RitualChannel`. Progress/deliberation surfaces ride
the SAME dormant commands (`NetJoinAccepted`, `NetJoinCharterProposed`,
`join_confirm` gate, `NetJoinFailed`) — first real emitters. Success emits
`NetJoinSealed{sealed, mls, mesh: [], nostr_sk, relays, rotation_seed,
generation}` — the two NEW fields are additive; `cmd_net_join_sealed`'s
validation ladder (defence-in-depth roster verify, nostr_sk↔anchor match,
zeroize-on-mismatch) is unchanged and its persist branch passes the new
fields into `materialize_workspace`. The supervisor stand-up tail stays
gated on `!mesh.is_empty()` → correctly skipped for Nostr (runtime is N5).

`materialize_workspace` grows the three additive parameters (kind, relays,
rotation_seed) — founder, joiner (and later rejoiner) call sites updated
together.

### 7.3 Bookkeeping

- Co-equality: no new Command *names* are strictly required (the dormant
  surface suffices) except the two added `NetJoinSealed` fields; if any new
  internal variant does appear, it goes on `INTERNAL` (currently
  `[&str; 45]`) with a rationale.
- Stale "SMP" doc-comments on the ritual commands get corrected in passing
  (core:3483, core:3535, core:2404).
- clippy 0 including tests; `.expect` not `.unwrap` in tests.
- molt-engine gains dev-deps `nostr-relay-builder` + `nostr` (workspace
  versions) for the capstone.

### 7.4 What N4a explicitly does NOT touch

`build_real_net` / supervisor / delivery-guarantee internals (N5),
`NetMesh*` commands (die with N5's cleanup), presence model (N5), GUI copy
(N6), `ChainOracle` wiring (N6).

### 7.5 Resume/offline honesty

`cmd_open_workspace`'s two shape-matches read `kind` first: a Nostr
workspace (kind `Some(Nostr)`, `mls` present) is NOT "detached" — it opens
with `net_health: Down` and the notice "relay runtime lands with N5"; the
legacy triple keeps its exact behavior for `kind: None`. Keystone: reopen a
founded Nostr workspace → honest classification, no supervisor, no crash.

## 8. N4b — recovery over Nostr (planned; built after N4a review lands)

1. **Recovery link v2**: `molt://recover/<republic>/<member>/<ticket-prefix>/
   <hex({v:2, ticket, npub, relays, republic_id})>` — coordinator npub +
   relays; minting requires no queue, so `spawn_recovery_provisioning`
   reduces to inbox-subscription-then-`NetRecoverLinkReady`.
2. **RecoverRequest** rides a 1059 to the coordinator npub. The total-loss
   rejoiner has NO anchored nostr key, so it derives an **ephemeral rejoin
   key from the recovery ticket** (`nostr_identity(entropy, ticket)` —
   same primitive, recovery-ticket-salted) as wrap author + reply inbox.
   The seat proof (`molt-seat-proof-v1`) grows the rejoin npub into its
   signed bytes (versioned bump v2) so the coordinator's Welcome cannot be
   redirected — and the wrap-author PoP (§2.1) applies to the rejoin key.
3. **Re-anchor — DECIDED 2026-07-31 (user-ratified): the new working key
   rides the `Restored` CHAIN BLOCK.** The roster's `nostr_pk` stays the
   genesis-signed **founding/historical** anchor, immutable forever; the
   seat's **working transport key** is re-established through the
   threshold-signed block recovery already produces:

   - The rejoiner derives the new key from the RECOVERY ticket
     (`nostr_identity(entropy, recovery_ticket)`) and carries its public
     half in the `RecoverRequest`.
   - **Seat proof v2** (`molt-seat-proof-v2`) signs
     `ticket ‖ key_package ‖ republic_id ‖ new_nostr_pk` with the
     re-derived IDENTITY key — so the new transport anchor is attested by
     the one key the phrase reconstructs, and a relay-level attacker
     cannot substitute its own. (Bump the tag; the v1 layout must not
     verify against a v2 seat — the invite-MAC-v2 precedent.)
   - `ChainChange::Membership { op: Restored, .. }` gains an **additive**
     `nostr_pk: Option<String>` (`#[serde(default)]`, canonicalized via
     `canonical_nostr_pk` at ingest, cross-seat-unique like every other
     anchor). The coordinator puts the attested key there; survivors
     threshold-approve the block; **every member learns the new anchor by
     APPLYING the block** — authenticated, converged, no inference from
     traffic.
   - Engine state gains a working-anchor projection (roster anchor unless
     a later `Restored` block overrode it); every gift-wrap send resolves
     the recipient through THAT, never the raw roster field. Pin it with a
     test that a post-recovery gift-wrap addresses the new key.

   Rejected alternatives, recorded: (a) local-only working key with other
   members inferring it from live traffic — the member→key mapping becomes
   an implicit rule each node re-derives, and any sender that reaches for
   the obvious roster anchor silently misses; (b) no re-anchor at all —
   makes recovery one-shot per seat (no future Welcome, no second
   recovery, no NIP-42), which contradicts recovery being the answer to a
   lost device.
4. **Welcome + chain**: the coordinator's 444 payload v2 carries
   `rotation_seed` + relays + the served chain (`ServedChainWire`), sizes
   checked against the 65408 cap — a big chain doesn't fit a gift wrap;
   if over budget, the Welcome carries the chain HEAD + the rejoiner
   fetches blocks over 445 catch-up (N5's machinery) — decide by
   measurement in N4b, keystone either way.
5. **Carrier stamp, both ends**: the coordinator picks the commit 445's
   `created_at` BEFORE `restore_member(member, kp, created_at)` and reuses
   it verbatim when publishing; receivers already feed `decrypt_at` from
   the event (§5). This retires `NO_CARRIER_STAMP` from the production
   path and restores the timestamp-first commit order.
6. **Replay-safe window reset** (ratified §4.2): the survivors' accept-
   window reset moves off the mesh announce onto the **Restored chain
   block**, guarded one-shot per (member, block height/id) — a re-served
   block during catch-up must NOT wipe a live window
   (`recovery_mesh_window`'s spend-once pattern generalizes; the guard set
   persists with the chain projection, not in memory).
7. The rejoin state machine reuses the §5 channel seam; `run_rejoin`'s
   verification ladder (served-chain verify, head-anchor check, phrase
   re-derivation) is transport-agnostic and unchanged.

### 8.8 N4b TDD order (execution-ready; anchors re-verified 2026-07-31)

Each step is one commit, red test first, green on master before the next.

1. **Seat proof v2** (`molt-net`/`molt-engine::founding` — `seat_proof_bytes`
   / `make_seat_proof` / `verify_seat_proof` around `founding.rs:750-794`).
   Tag `molt-seat-proof-v2`, layout
   `ticket ‖ key_package ‖ republic_id ‖ new_nostr_pk`, length-prefixed per
   the `hash-length-prefix-not-separators` rule. **Red:** a v1 proof must NOT
   verify against a v2 seat; a proof whose `new_nostr_pk` was swapped must
   fail. No back-compat path — nothing shipped a v1 recovery over Nostr.
2. **`ChainChange::Membership` gains `nostr_pk: Option<String>`**
   (`molt-core::chain`, additive `#[serde(default)]`). `approval_bytes` binds
   it — so **bump `molt-chain-change-v1` → `-v2`** and update `verify_chain`
   together (the CLAUDE.md versioned-layout rule; an unbumped change breaks
   signatures silently). **Red:** byte-pin the new `approval_bytes` layout
   (fixture computed independently), and a block whose `nostr_pk` was
   tampered after signing must fail `verify_chain`.
3. **Working-anchor projection** (`molt-engine::State`): `working_nostr_pk(
   member) -> &str` = the roster anchor unless a later applied `Restored`
   block overrode it. Built in `apply`/`after_block_applied` from the chain,
   never persisted separately (it is a projection, like `chain_applied`).
   **Red:** after a `Restored` block the projection returns the NEW key while
   `identities[i].nostr_pk` still returns the founding anchor. Then make
   EVERY gift-wrap send site resolve through it (grep `send_ritual` /
   `send_welcome` callers) — a send that reaches for the raw roster field is
   the bug this projection exists to prevent.
4. **Recovery link v2** (`recovery.rs:35-111 RecoveryInvite`): reuse
   `InviteHandoverV2`'s shape plus `republic_id`
   (`{v:2, ticket, npub, relays, republic_id}`). **Red:** round-trip, v1
   rejection with the honest "older build" message, the same relay caps.
5. ✅ **LANDED 2026-08-02.** `State` adopts the Nostr material at both bring-up
   paths (5a); `mint_recovery_link_over_relays` + `spawn_recovery_inbox`
   replace the queue mint on a Nostr republic (5b); `sender_npub` reaches the
   ingest and gates deliverability there (5c). Keystones in
   `crates/molt-engine/tests/nostr_recovery.rs`. **Still owed:** the PoP gate
   has no end-to-end test — a forged request fails the SEAT PROOF first, so a
   test written today would pass for the wrong reason (the inert-keystone
   trap). It is pinned when step 6 can produce a correctly-signed request with
   a mismatched wrap author. Also deferred: the relay set advertised in the
   link is this node's dialable pool ∩ the group's relays, which is the
   conservative reading of §10.15 but does NOT resolve it.

   **Coordinator mint over relays**: `cmd_recover_invite_start`
   (`net.rs:1526`, precondition at `:1561-1567`) currently REQUIRES
   `runtime_transport()` to mint a queue — on Nostr it needs only the dialer
   + the workspace relay list, so the mesh precondition becomes a kind check.
   `spawn_recovery_provisioning` (`recovery.rs:205-285`, link render `:248`,
   `NetRecoverLinkReady` `:260`) reduces to inbox-subscribe →
   `NetRecoverLinkReady` (its `NetRecoverLinkFailed` ticket-unregistration
   path stays).

   **5a — PREREQUISITE, verified 2026-08-01: the survivor has no Nostr
   material to mint with.** `open_stored_workspace` (`session.rs:1017`) reads
   the whole `TransportState` but adopts only `identity_sk` (`:1043`);
   `kind` becomes a local `nostr_kind` bool (`:877`) and is dropped. `State`
   holds no `nostr_sk`, no group `relays`, no transport kind — so a reopened
   coordinator cannot build a `RitualNet` at all. Step 5 therefore starts by
   adopting them into `State`, in `open_stored_workspace` AND in
   `materialize_workspace` (`lifecycles.rs`), next to `identity_sk`. Without
   this the rest of step 5 cannot be written, let alone tested.

   **5b — the inbox task.** `spawn_recovery_inbox`, modelled on
   `spawn_founder_inbox` (`nostr_ritual.rs:74`): `RitualNet::inbox()` →
   `live_state(LIVE_WAIT).any()` readable-gate (subscribe-before-advertise —
   a link advertised over an inbox nothing answers on is the N4a defect
   `a275f6e` fixed for founding) → THEN render the v2 link → loop feeding
   `RitualMsg::Recover` into `Command::NetRecoverRequested`. The production
   mint still renders `handover: None` (`recovery.rs:256`); this is where
   `RecoveryHandoverV2` (step 4) gets its first production caller.

   **5c — PoP for the rejoin key.** `NetRecoverRequested` (`core:3389`) has
   no `sender_npub`, while its founding twin `NetJoinRequested` (`core:3353`)
   carries one, set from the peeled wrap's proven sender
   (`nostr_ritual.rs:173`) and checked before the ticket is spent
   (`founding.rs:2378-2394`). §8.2 requires the wrap-author PoP to apply to
   the rejoin key, so the field is added here — the inbox task is what knows
   the sealer — and gated in `cmd_net_recover_requested` BEFORE the ticket is
   spent. An empty `sender_npub` keeps the loopback path unchanged.

   **Red (the bar this step must clear, stated because §8.8 originally gave
   step 5 none):** on a founded-over-relays republic, `RecoverInviteStart`
   on a survivor currently reports `recovery-link-failed:mesh-not-running`.
   The red test asserts that literal outcome first, then flips to asserting a
   parseable `molt://recover/…` link whose handover decodes as v2 and names
   the coordinator's anchor + the group relays. Second red: a `Recover`
   request whose `new_nostr_pk` is not the wrap's proven sealer is refused
   with the ticket UNSPENT.

   **Copy that step 5 makes false** (all keyed to the `mesh-not-running`
   reason at `net.rs:1564`): the `recover_invite_start` MCP tool description
   (`molt-mcp/src/lib.rs:1154` — also prose, against the compact-text rule),
   the GUI branch at `app.slint:7374` / `molt-ui/src/lib.rs:2032`, and the
   test pinning it (`two_instances.rs:4620`). Keep `mesh-not-running` for the
   legacy kind; give the Nostr path its own short reasons from the existing
   relay vocabulary (`relay_msg::pool_gap_reason`).
6. **Rejoiner task**: the `RecoverStart` twin of N4a's `spawn_member_join` —
   derive the ephemeral key from the RECOVERY ticket, subscribe the 1059
   inbox, gift-wrap the `RecoverRequest` (with `new_nostr_pk` + seat proof
   v2), wait the 15-min `RECOVERY_WELCOME_TIMEOUT` (`recovery.rs:25`,
   absolute deadline — keep it), peel the 444, `join_from_welcome`, verify
   the served chain, report `NetRecoverSealed`. Deletes the last
   `NO_TRANSPORT_YET` raise (`lifecycles.rs:1441`, the const at `lib.rs:88`).

   **ORDERING PROBLEM, found 2026-08-01 — decide before step 6 starts.** Step
   6 says "peel the 444 … verify the served chain", but `WelcomePayload`
   (`molt-net/src/welcome.rs:82`, `WELCOME_PAYLOAD_VERSION = 2`) carries only
   `welcome`, `rotation_seed`, `relays` — **no chain slot**. Loopback gets
   away with it by bundling the chain beside the Welcome on the reply queue
   (`recovery.rs:294`, fed from `chain.rs:1365`); over Nostr there is no
   second channel. So step 6 cannot be written without the payload decision
   §8.8 defers to **step 10** (carry the chain vs. carry the HEAD and fetch
   over 445 — the 65408-cap measurement). Either pull step 10's measurement
   into step 6, or reorder 10 before 6. Also additive here:
   `NetRecoverSealed` (`core:3722`) has no `nostr_sk`/`relays`/
   `rotation_seed`, so `cmd_net_recover_sealed` still materializes the legacy
   shape (`lifecycles.rs:1534`) — mirror `NetJoinSealed`.
7. **Ingest + block** — **mostly landed already** by steps 1–3:
   `cmd_net_recover_requested` (`net.rs:1443-1513`) already runs
   `canonical_nostr_pk` on the wire `new_nostr_pk` (`:1470-1481`), already
   enforces cross-seat uniqueness (`:1483-1494`), and already spends the
   ticket only after verification (`:1505`). What REMAINS for step 7 is the
   PoP gate (moved into 5c) and the doc comment at `net.rs:1432-1440`, which
   still claims the opposite of the code ("recovery ingests NO wire-supplied
   `nostr_pk`") and must be corrected in whichever commit touches the file
   first. Originally it was to run — ticket spent only on a VERIFIED request — passes it into
   `verify_and_propose_restore` (`chain.rs:910-941`) so the proposed
   `Restored` block carries it.
8. **Carrier stamp, both ends**: `restore_member_on_group`
   (`net.rs:530-544`) stops passing `NO_CARRIER_STAMP` — `coordinator_rekey`
   (`chain.rs:1273-1336`) picks the commit 445's `created_at` FIRST and
   reuses it verbatim when publishing. **Red:** a keystone over the
   PRODUCTION entry points (the N3 lesson — a keystone driving an API the
   product does not call pins nothing) showing both ends compute the same
   `CommitKey`.
9. **Replay-safe window reset**: move the survivors' reset off the mesh
   announce (`net.rs:1616-1664`) onto the applied `Restored` block, guarded
   one-shot per `(member, block height)` in a set that lives with the chain
   projection. **Red:** re-serving the same `Restored` block during catch-up
   must NOT wipe a live accept window (the failure this guard exists for is
   "everything is a duplicate" — the exact bug the reset was invented to
   prevent).
10. ✅ **MEASURED 2026-08-02 — the Welcome CANNOT carry the chain.** Keystone:
    `crates/molt-engine/tests/welcome_chain_budget.rs`.

    | case | cost | cap |
    |---|---|---|
    | ordinary governance block | 1061 B → ~61 blocks fit | 65408 B |
    | **one** `set_image` with a 25 KiB logo | **69318 B for that one block** | 65408 B |

    A `set_image` proposal EMBEDS the picture (`payload.bytes_b64`,
    `proposals.rs::image_bytes`) and the payload rides the applied chain block —
    that is how every device materializes the logo. Images are capped by
    DIMENSION (8192×8192), never by bytes, so one ordinary logo exceeds the
    whole gift-wrap budget by itself: base64 ×1.33, payload hex ×2.

    So "the chain fits in a Welcome" is not a property of chain LENGTH a
    republic could stay under — it is **one proposal away from false, forever**.
    Even without images the ceiling is ~61 blocks.

    **Decision: the Welcome carries the chain HEAD; the rejoiner fetches blocks
    over 445 catch-up.** Which is N5 machinery — so, as this step itself
    demands, N4b says so rather than half-building it:

    > **N4b step 6 cannot deliver a working recovery before the 445 catch-up
    > exists.** Either N5's catch-up moves ahead of step 6, or N4b builds a
    > minimal fetch and N5 generalizes it. This is a sequencing decision, not a
    > coding one — it is recorded here because the old ordering (6 before 10)
    > silently assumed the opposite answer.

11. ~~**Welcome size**~~: measure a real served chain in the 444 payload against
    the 65408 cap. Under → carry it; over → carry the HEAD and fetch blocks
    over 445 catch-up. **Decide by measurement, keystone either way** — and
    if the fetch path is needed, it is N5 machinery and N4b must say so
    rather than half-build it.
11. **Capstone**: the recovery twin of `tests/nostr_founding.rs` — found a
    2-of-3 over a MockRelay, hard-kill one member (the
    `delivery_guarantee.rs:33 hard_kill` pattern: drop the handle, wait for
    the LOCK), mint a recovery link on a survivor, rejoin on a FRESH engine
    over the same relay, and assert: the rejoiner materializes with the
    verified chain, its `transport.state.nostr_sk` pairs with the NEW
    anchor in the `Restored` block, survivors' `working_nostr_pk` returns
    that same new key, and a post-recovery gift-wrap reaches the recovered
    seat. Negatives: a wrong phrase, a doctored link id, and a re-served
    `Restored` block leaving the accept window intact.
12. **Doc closeout** + the adversarial multi-lens review over the N4b diff.

## 8b. N4a — what actually landed (2026-07-31)

Built and green on master across four commits (steps 1–3 + the core):

- **v4 `TransportState`** (`TransportKind::Nostr`, `relays`,
  `rotation_seed`, `relay_cursors`) + the kind-first resume gate (a Nostr
  workspace opens honestly pending its N5 runtime, never "detached"); the
  MLS snapshot is v2 and carries the exporter ring (a v1 blob restores with
  an empty ring). The exporter-ring persistence debt (N3 §5.5) is **closed.**
- **kind-446 `ritual_wrap`** (proven-sealer peel = PoP at join) and the
  **444 Welcome payload v2** (seed + relays inside the authenticated
  Welcome, oversize-refused at the NIP-44 cap).
- **Invite link v2** (`InviteHandoverV2`): full ticket + founder npub
  (bech32 on the wire, canonical hex in memory) + gated relay list; the
  pre-N4 queue link is refused with an honest message.
- **`molt-net::ritual_net`** — the engine-facing facade: `RitualNet`
  (1059 inbox + gift-wrap sends), `GroupChannel` (445 publish/subscribe with
  the §4.4 skew-margin window tags and the window-roll resubscribe). Built
  by a delegated agent against a fixed contract; two minor deviations
  (`live(&mut self)`, `window_tags` pub).
- **`molt-engine::nostr_ritual`** — the founder inbox loop, the founder 445
  recv, the shared 445 publish leg, and the whole **member join task** that
  emits the once-dark `NetJoin*` lane. The group is born at all-joined; the
  founder's genesis frame is **encrypted before the snapshot** (ratchet
  coherence — the capstone caught a `SecretReuseError` here).
- **Capstone** `tests/nostr_founding.rs`: two real engines over one
  `MockRelay` found+join+deliberate+seal+persist+reopen; negatives for the
  spent link and the declined charter. First engine-level test to drive the
  relay runtime.

Deviations from the plan as written, all deliberate:

- The genesis is one **445** (not a per-seat send) AND is encrypted at
  finalize time, not inside `distribute_genesis` — the ratchet-coherence
  fix moved it. `distribute_genesis` stays the loopback-only path.
- No new `Net*` command *names*; `NetJoinSealed` gained two additive fields
  (`relays`, `rotation_seed`). Co-equality unaffected (INTERNAL unchanged).
- `NO_TRANSPORT_YET` is retargeted to **recovery only** — founding and join
  no longer use it.

## 9. TDD order (one commit per step, each green on master)

1. **TransportState v4 + TransportKind + resume gates** (§6, §7.5) — red:
   serde-compat + resume-classification tests; plus the exporter-ring
   snapshot wrapper (§6.1) with its legacy-fallback pin.
2. **molt-net ritual wire**: kind-446 `wrap_ritual`/`peel_ritual`
   (generalizing welcome.rs's chain), 444 payload v2, PoP enforcement
   (§2.1) — red: roundtrips, fail-closed negatives (wrong recipient/kind/
   payload/author), oversize refusal, byte fixtures.
3. **Link v2** render/parse (+ recovery twin shape, parse only) — red:
   fixtures, v1 rejection, full-ticket MAC closure, relay-list caps.
4. **`RitualChannel` extraction** (behavior-identical refactor) — red bar
   is the EXISTING suites: `two_instances`, `three_nodes`, `founding`,
   `join_timeout` stay green; no new behavior.
5. **Nostr channel + founder path** (§7.1) — red: founder-side keystones
   over `MockRelay` with a scripted member (link-ready via inbox-live,
   join-ingest through the real ladder incl. PoP + LinkSpent, welcome
   fan-out at all-joined, Seal/Genesis as 445s, window-roll resubscribe).
6. **Member task + `NetJoinSealed`** (§7.2) — red: member-side keystones
   over `MockRelay` with a scripted founder (accept timeout, ratify gate,
   decline, genesis byte-check, duplicate-delivery idempotency).
7. **Capstone**: `crates/molt-engine/tests/nostr_founding.rs` — two REAL
   engines, one MockRelay: full CreateStart→JoinStart→CreatePropose→
   ratify→seal; asserts genesis-on-disk (3 anchors, attestations verify),
   both `transport.state`s (kind/relays/rotation_seed/nostr_sk), MLS
   interop from both persisted snapshots, the `NetJoinSealed` persist
   branch runs FOR REAL (no injection), both reopen honestly (§7.5).
   Negatives: second link activation → LinkSpent; declined charter aborts
   both sides; forged-MAC join rejected without spending the ticket.
8. **Doc closeout**: concept §11 N4a status, this plan's status, CLAUDE.md
   transport section touch-up, memory update.

Steps 1–3 are independent of each other; 4 precedes 5/6; 7 needs 5+6.
After step 8: the adversarial multi-lens review pass over the whole N4a
diff (the method that caught the N1 CRITICAL and both N3 inert-keystone
bugs), findings fixed before the etappe is called done.

## 10. Known limits carried out of N4a (recorded, not hidden)

- No live runtime until N5 (§0 honesty block).
- The outer envelope still binds no AAD context (N3 §5.5) — unchanged
  here; N5 decides the AAD shape together with the runtime.
- NIP-42 on ritual inboxes uses the anchor keys (correlation handle noted
  in N2 §3.5); ephemeral-per-relay auth keys remain a follow-up.
- The founder's inbox subscription lives only as long as the ritual — a
  joiner activating a link after `CreateCancel` gets silence, exactly like
  today's dead queues.
- Relay-side ciphertext residue of cancelled rituals (§1).
- `relay_cursors` persisted but unread until N5.
