# N4 execution plan — the ritual over Nostr

Status: **N4a BUILT (2026-07-31), N4b OPEN.** Executes the N4 etappe of
`nostr_transport_marmot.md` §11 on top of the N2 relay runtime
(`nostr_n2_plan.md`) and the N3 wire edge (`nostr_n3_plan.md`). Every design
input is ratified (§4.2, §10.11–10.14, ADR-0004/0005/0006); this is the
execution map. The one genuinely open product question lives in §8.3 (N4b
re-anchoring) and is flagged for the user before N4b is built — nothing in
N4a depends on its answer.

Split: **N4a = founding + join** (this pass), **N4b = recovery** (next pass,
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
3. **Re-anchor question (THE open product point — ask before building):**
   the roster's `nostr_pk` anchor is genesis-signed and immutable, but the
   recovered device cannot re-derive its secret (ticket died). Recommended:
   persist the recovery-ticket-salted key as the seat's NEW working
   transport key (`transport.state.nostr_sk`, `NetRecoverSealed` gains an
   additive `nostr_sk` field), document that the roster anchor is the
   FOUNDING anchor (historical identity binding) while the working key may
   rotate through recoveries. Consequence to state honestly: post-recovery,
   gift-wraps addressed by roster anchor would miss — every N4/N5 flow that
   gift-wraps must address the WORKING key learned from live traffic
   (recovery's own flow already does; founding never re-wraps after
   genesis). Alternative (rejected unless the user overrides): no re-anchor,
   recovered seats permanently lose gift-wrap reachability and NIP-42 keys.
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
