# Concept: Nostr/Marmot transport (NIP-EE) as a second transport backend

Status: **DRAFT 2026-07-29, DISCUSSION DOCUMENT — not an execution plan.**
Written after the delivery-guarantee live validation (2026-07-27..29) and
hardened by two adversarial review passes (§13, two ledgers). Read first:
`documents/delivery_guarantee.md` (the layer this offloads),
`documents/mesh_selfheal.md` + `mesh_reliability.md` (the machinery this
removes — and the earlier measurement that partly contradicts §0),
`documents/recovery_ritual.md`, `documents/backup_restore_design.md`,
`documents/founding_ritual.md` (invariants this must not weaken), and the
CLAUDE.md transport section.

**Read this first — the honest state of the concept (both review passes):**

1. **This is a go/no-go proposal, not a green-lit build.** The two concrete
   SMP grievances (`ERR QUOTA`, silent queue deafness) are, respectively, a
   server config knob and operator policy — both fixable by **self-hosting one
   smp-server**, keeping ONE transport, zero roster changes, and the five
   already-green compensation layers. The self-host experiment (§10.8) costs a
   day. **Run it first.** Commit to N0–N6 only if self-hosted SMP still
   misbehaves — see the go/no-go gate at the end of §0.
2. **The diagnosis has flipped once.** Three weeks earlier, against the same
   public server, `mesh_reliability.md` §0 concluded "queues are ALIVE, no
   queue expired, there is no receive bug." A substrate diagnosis that already
   reversed once is a shaky basis for a **permanent third anchor in the roster
   bytes** — the one place mistakes are forever. Re-confirm the substrate is
   really the problem before touching roster-v3.
3. **The §4.1 "same engine interface" claim is the concept's weakest point.**
   It is true only of the thin `molt-net` supervisor traits. The real coupling
   is `molt-engine/src/net.rs` (~4,300 lines, member/mesh-shaped end to end),
   the `RitualTransport`-typed runtime crypto, and the reopen/recovery/persist
   dispatch — none of it budgeted. §11 now carries an explicit engine-inventory
   etappe (N0.5) and a ~6-week (not 3-week) sizing.
4. **§9 "re-found + restore" is a data-loss trap, not a migration** (finding
   II-4): `restore` is bound to the SAME workspace id by a hard content check,
   so a Nostr re-founding yields a new empty republic + a detached read-only
   archive — no live chat/governance/file history. Fixed honestly in §9.
5. **The product story is unresolved.** Per-workspace transport choice, files
   possibly OFF on Nostr, coarser presence, and relay operators seeing your
   subscription IP + member count — nobody has written the one paragraph that
   tells a founder *why* to pick Nostr. If it can't be written crisply, the
   answer to §10.2 is "self-host-only, Tor mandatory."

Everything below §0 is the DESIGN, execution-ready in its mapping details but
NOT in its scope until the engine inventory (N0.5) and the §10 design-input
decisions land. Treat §10 splits (design-input vs. product-taste) as gates.

## 0. Why at all

The live validation showed our stack loses nothing and orders correctly —
but it spends its life fighting the substrate. On the public SMP server idle
queues die silently (SUB/SEND keep returning OK), each queue has a
128-message quota (`ERR QUOTA` in the 2026-07-28 live log), and our self-heal
protocol needs at least one working leg to rotate — so when several legs are
deaf at once (the 15-minute loop to "Madame Brutal") no path carries the
rotate announce and healing spins for minutes to hours. This is not a single
bug: the per-pair-queue model makes **every leg its own single point of
deafness**, and we have stacked five compensation layers on top (keepalive,
verify-at-open, Stage-B watchdog, rotate, ACK/resend).

Marmot/NIP-EE proves our crypto model (MLS groups) runs on a gentler
substrate: dumb, redundant Nostr relays with store-and-forward. White Noise
(Rust, OpenMLS, in production since 2025/26) is the reference; NIP-EE is
adopted into the official Nostr NIPs.

**Goal:** Nostr relays as a SECOND transport backend beside SMP — chosen per
workspace at founding. **Non-goal:** replacing SMP, or live-migrating an
existing republic (§9).

**Honest headline from the review (§13):** the naive pitch — "relay history
IS the catch-up, the self-heal zoo just disappears" — does not survive
contact with our own pinned MLS posture and with how relays actually behave.
The real win is narrower and still large: **N² fragile per-pair legs collapse
to a small redundant relay set**, and the guarantee layer we already built
(ACK/rewind/ordering) remains the actual correctness backbone rather than the
subscription. Read §6 and §13 before believing any "it just works" claim.

**Go/no-go gate (decide before N0.5):**

- **Step A — self-host experiment (§10.8), one day.** Stand up one
  `smp-server` with gentle policy (`expire_messages_days` high,
  `restore_messages` on, quota raised, inactive-client TTL generous). Point
  all three test instances at it (custom-server config, fixed 2026-07-29 in
  `10e4ee0`), found a fresh republic, use it for a few hours.
- **Step B — verdict.** If the deafness/quota symptoms vanish → **the
  substrate was operator policy, not the model**: stop here, ship a self-host
  recommendation, do NOT build Nostr. If they persist against a well-configured
  own server → the per-pair-queue model is genuinely the problem → proceed,
  but frame Nostr as a **replacement candidate with intent to deprecate SMP
  for new foundings** (finding II-7), not as a permanent second transport we
  maintain forever.
- N0 (spike/audit) may run in parallel with Step A; N0.5 and everything after
  are gated on the Step B verdict.

## 1. NIP-EE/Marmot primer (verified against EE.md, state 2026-07)

Three event kinds carry MLS over relays:

- **Kind 443 — KeyPackage:** hex-serialized MLS KeyPackage, signed with the
  Nostr identity key; tags `mls_protocol_version`, `ciphersuite`,
  `extensions`, `relays`. Marmot recommends "last resort" KeyPackages; after a
  successful join: delete + rotate the signing key. **Kind 10051** publishes
  which relays hold a user's KeyPackages (the public "mailbox address" for
  invites). **We drop both 443 and 10051 — see §4.2 and §13 finding 5.**
- **Kind 444 — Welcome:** the MLS Welcome, NIP-59 **gift-wrapped** to the
  invitee (recipient inbox, not group relays). Deliberately UNSIGNED: a
  leaked 444 is not publishable.
- **Kind 445 — Group Event:** ALL group messages (MLS application AND
  commits/proposals). Double-wrapped: the MLS ciphertext is additionally
  NIP-44-encrypted with a keypair derived from the MLS `exporter_secret`
  (label `nostr`, 32 bytes, **rotated every epoch**) — so relays cannot even
  see the MLS frames. Published **per event with a fresh ephemeral Nostr
  keypair** (membership/size hidden); the only visible group metadatum is the
  `h`-tag group id, which can change over the group's lifetime.
- **Group metadata** (the group's relay list, group id, `admin_pubkeys`) live
  in the MLS extension `nostr_group_data` and change only via MLS commits;
  NIP-EE gates those changes to the listed admins.
- **Commit races:** a sender waits for a relay ACK before applying its own
  commit locally; concurrent commits of the same epoch are broken
  deterministically (lowest `created_at`, then lowest event id); clients keep
  the prior state briefly to heal forks.
- **Security properties:** compromise of the Nostr identity key gives NO
  access to group messages (MLS keys are independent); forward secrecy as in
  MLS ("keys deleted as soon as used").

## 2. What stays, what goes, what is new

**Unchanged** (the republic layers): molt-core (Command/Event/Chain
contract), the founding ritual as a *flow* (sign-what-you-see, deliberation,
one-shot seal), the threshold chain, the engine actor, MCP co-equality, GUI,
storage, and the delivery-guarantee semantics (AcceptedWindow, prev_seq
ordering G7) as an end-to-end net UNDER the new transport (§6).

**Removed** (for Nostr workspaces only): the per-pair-queue mesh and all its
upkeep — mesh bootstrap/announce/assemble, rotation (Track C), deaf-leg
detection, Stage-B resubscribe watchdog, queue hygiene, `ERR QUOTA`. The
runtime shrinks to: publish own events to N relays, subscribe to the group's
events from N relays, dedup.

**New:** a `NostrTransport` (relay pool, NIP-01 subscription, NIP-44/NIP-59
crypto), the NIP-EE event mapping, a second identity anchor (§3), the
governance bridge for `nostr_group_data` (§5), an **exporter-secret ring**
(§6, forced by review finding 1), an **h-tag/relay rotation grace window**
(§4.4, forced by finding 6), and a wire-size budget against relay caps (§7,
forced by finding 7).

## 3. Identity design: one seed, three anchors

Nostr signs with secp256k1 Schnorr (BIP-340); our roster/chain identity is
Ed25519. Both stay, both derived from the SAME recovery phrase (BIP-340 key
via pure-Rust `k256`; derivation analogous to `founding::member_identity`):

- **Ed25519** stays the roster identity + MLS credential key ("one identity,
  two anchors" untouched — the MLS binding still checks Ed25519).
- **secp256k1** becomes the transport anchor: receives gift-wraps, signs the
  ritual gift-wrap envelopes. **Not** a public discovery key (no 443/10051).

The roster gains the Nostr pubkey as a **third anchor**: a new field on
`MemberIdentity` (`nostr_pk`, 32 raw bytes, lowercase hex in signed bytes,
bech32 only in link/UI). This touches `roster_canonical_bytes` → **tag bump
to `molt-roster-v3`** and ALL recompute sites (founder canonical,
`verify_sealed_roster`, `verify_seal_proposal`, test harnesses) in one pass —
the known ~15-site ripple, as its own etappe (N1) with byte-pin tests.

**Binding (review finding 3 — this is the security core, and the naive
version does not bind):**

- **Salt the nostr derivation with the seat's `ticket`**, not a global
  constant — the ticket is the only secret both parties share at join time
  (`republic_id` is content-derived and does not exist until every seat is
  filled). A global salt would make one person present the same npub in every
  republic (a cross-republic correlation handle; finding 5).
- **Invite MAC v2** covers the nostr key: `HMAC(KDF(ticket), version ‖ name ‖
  0 ‖ identity_pk ‖ 0 ‖ nostr_pk)` with an explicit version byte so a v1 link
  cannot be replayed into a v2 seat. Today's MAC covers only
  `name ‖ 0 ‖ identity_pk` (`molt-net/src/invite.rs`), so without this the
  third anchor is unbound.
- **Sign-what-you-see extends to the new anchor:** `verify_seal_proposal` /
  `verify_sealed_roster` must self-check `nostr_pk` (own seat's value matches
  what the member derived), or a malicious founder could anchor an
  attacker-controlled `nostr_pk` for member B — not a group-read break (MLS
  still binds Ed25519) but a silent hijack of B's future gift-wrapped material
  (Welcomes, recovery), i.e. denial-of-recovery plus a shadow transport
  identity the relay sees as B.
- **`republic_id`:** today it hashes only `identity_pk` (`molt-storage`, tag
  `molt-republic-id-v1`). **Decision required (§10.6):** bump to v2 including
  `nostr_pk` (recommended — otherwise the id no longer commits to the full
  roster content, weakening founding_ritual §8.2). x-only BIP-340 keys share
  an x for `d` and `n−d`; normalize to the even-y representation at ingest so
  one key has one signed-byte form.

## 4. Transport mapping

### 4.1 Strategy: NOT behind the queue trait

The existing `Transport` trait is queue-shaped (create_queue/send/subscribe
per address). Forcing Nostr into it (queue ≈ tag filter) would keep the
per-pair model and thus the WHOLE mesh upkeep — the main win would evaporate.
Instead: a **second runtime path** beside the SMP supervisor, same engine
interface (`EngineSink`, `OutboxLog`, `StateStore` reused), but group-shaped:

```
NostrGroupRuntime
├─ outbox_task: reads the log from the cursor, MLS-encrypt (same MlsChannel
│    semantics as today) → exporter-NIP-44 → kind-445 with an ephemeral key
│    → publish to N relays; cursor advances on ≥1 relay-OK.
│    NOTE (finding 8): resends publish ONCE at the min acked_floor across
│    members and let each receiver's AcceptedWindow dedup — NOT one publish
│    per peer. Per-member floors stay (§6); the resend backoff is
│    relay-cost-aware (capped rounds/hour, not only per-peer backoff).
├─ recv_task: ONE subscription (`h` tag; see 4.4 for the rotation grace)
│    over the relay pool; event-id dedup ring; per-connection decrypt-failure
│    budget with a circuit breaker (finding 10); NIP-44-unwrap →
│    MlsChannel::decode → EngineSink::deliver — Epoch-Buffer/FutureEpoch
│    logic AS TODAY.
└─ relay_pool: N WebSocket connections, reconnect with backoff, per-relay sub
     state; "leg status" = relay status (net_health shows relays, not peers).
```

Per-peer state shrinks to the delivery-guarantee cursors (§6); the
`PeerLink`/mesh persistence is gone for Nostr workspaces. `transport.state`
stores instead: the group relay list, the current `h` tag, a **per-relay**
subscription cursor (§4.3), and the exporter-secret ring (§6) — additive
fields.

### 4.2 Founding/Join/Recovery over NIP-EE — no public discovery events

The ritual FLOW stays; only the envelopes change. **We publish neither kind
443 nor 10051** (finding 5: they leak a member's handle + Ed25519/nostr key
to any relay operator, correlatable across every republic the member joins).
Discovery is the only thing they buy and our flow never needs it:

- **Invite:** the link carries the founder's `npub` + invite-relay list + the
  MAC secret (a versioned extension of today's invite link; the founder's
  npub is the recipient of the joiner's gift-wrap).
- **JoinRequest:** the joiner gift-wraps (NIP-59) its KeyPackage **inside**
  the existing MAC-bound `RitualMsg` to the founder's npub over the invite
  relays — no standalone 443. The founder checks the MAC v2 + the 3-anchor
  binding.
- **Welcome/Deliberation/Seal:** kind-444 (gift-wrapped) for the MLS Welcome;
  the deliberation/ratification messages run as kind-445 in the freshly born
  group — sign-what-you-see unchanged (the member still recomputes the
  canonical table itself).
- **Recovery (finding 9 — circular otherwise):** a total-loss rejoiner has no
  disk, so it knows neither the relay list nor the `h` tag (both live inside
  the MLS group). The coordinator therefore mints a **recovery link v2**
  (npub + invite-relays; the `h` tag is delivered only inside the
  authenticated Welcome, never before), the twin of today's recovery-queue
  handover. The MLS re-key commit + Welcome come from the MLS layer as today,
  in 444/445 envelopes; the queue-re-pointing machinery is gone. The
  survivors' accept-window reset (E7 finding 1) still fires, but its trigger
  moves to the recovery **chain block** — and because a block is re-served
  during catch-up, the reset MUST be made idempotent-and-one-shot against
  replay (guard on the block height/id, not "a recovery block arrived"), or a
  re-delivered block wipes a live accept window and re-opens the
  "everything-is-a-duplicate" failure the reset exists to prevent.

### 4.3 The subscription cursor is an optimization, never the truth (finding 4)

`created_at` is publisher-chosen and, with ephemeral per-event keys, wholly
unaccountable; NIP-59 also randomizes gift-wrap `created_at` up to two days
into the past. So one peer with a +1-day clock skew (or a spammer) could push
every receiver's cursor a day into the future, and after a restart the
receiver would subscribe with `since ≈ now+23h` and silently receive nothing
until wall-clock catches up. Rules:

- Never advance the cursor past local `now`; clamp far-future `created_at` on
  ingest and flag it.
- Keep the cursor **per relay** (relays differ in retention and in what they
  served).
- Treat it strictly as an optimization: correctness rests on the ACK/rewind
  layer (§6), with a wide fixed overlap (hours) plus the event-id ring across
  the overlap. **Keystone (N2):** "a peer publishing +24h `created_at` does
  not blind the receiver after reopen."

### 4.4 h-tag / relay rotation is a grace-window overlap, not a switch (finding 6)

§0's whole argument is that per-pair queues make every leg a single point of
deafness. Nostr replaces N² legs with ONE rendezvous: the `h` tag + relay
list. Both change only via MLS commits — and the commit that says "move here"
travels only over the channel being rotated away from. A member offline
across the rotation wakes subscribed to the old `(h, relays)`, sees nothing,
and (with no 443/queue fallback) is stranded.

Therefore rotation is a **grace overlap**: publish the rotation commit and a
following window of traffic to BOTH old and new `(h, relays)`, keep the old
subscription alive for the whole grace, and tie the grace to the existing
WP4a peer horizon so it matches the delivery guarantee's horizon. **Keystone
(N5):** "a node offline across a rotation still converges when it returns
inside the grace, and reports loudly (G4) when it returns outside it." A node
outside the grace falls back to the recovery ritual (§4.2) — the same honest
last resort as a total-loss rejoiner.

## 5. Governance collision: `admin_pubkeys` vs. threshold (finding 2)

NIP-EE gates `nostr_group_data` changes on admin pubkeys — MoltRepublic knows
no admins, only m-of-n. And under ephemeral per-event publish keys the event
signature maps to no `admin_pubkeys` entry at all, so `admin_pubkeys` carries
**no enforcement weight** — it is decorative interop. Resolution:

- **MLS layer:** all members are in `admin_pubkeys` (interoperable — every
  member CAN build a group-data commit).
- **Engine layer (the real gate), fork-proof by construction:** a group-data
  commit is only built/applied if a **threshold-decided chain block**
  authorizes it (new additive `ChainChange::TransportPolicy { relays, h_tag,
  .. }`). The authorizing block's hash is bound into the commit
  (GroupContextExtensions payload / AAD), so every honest member's validation
  is a pure function: a commit whose block does not resolve is dropped
  **before** `merge_staged_commit`, not "rejected after". This is essential —
  a bare "reject" leaves honest members at epoch N while the proposer is at
  N+1, every subsequent proposer message `FutureEpoch`-buffers forever, and
  one member has permanently partitioned the group (our recovery is built for
  a lost device, not an epoch split).
- **Commit lifecycle (real change to the MLS wrapper, N3 not N6):** today
  `decrypt()` merges a staged commit immediately, and `restore_member` merges
  the pending commit at BUILD time — before publish (`molt-net/src/mls.rs`).
  For a relay world we need an explicit stage → publish → await relay-OK →
  merge state, with the prior state retained across the tiebreak window. The
  snapshot contract ("always current, atomically overwritten") must grow a
  bounded prior-state slot for fork healing.
- **The layering knot (finding II-2):** the component that decrypts commits
  lives in `molt-net` (`MlsChannel::decode`), but the authorizing chain block
  lives in engine `State` (`chain_applied`). Strict layering (core→…→net→
  engine) forbids net reading engine state, and bouncing every staged commit
  up to the actor and back opens the very `FutureEpoch` gap §5 exists to
  close. **Seam:** a narrow `ChainOracle` trait defined in `molt-net`
  (`fn authorizes(block_hash) -> bool`, plus the current head), implemented by
  the engine and handed into the runtime exactly like `EngineSink`. The
  runtime resolves the block-hash bound into the commit synchronously against
  the oracle before `merge_staged_commit`; no round-trip, no gap. This seam's
  contract + a stubbed hard-reject test belong in **N3**, so N3's keystone
  tests the actual security property instead of only the tiebreak — N6 then
  only fills in the `ChainChange::TransportPolicy` block type. (Without this,
  N3 can go green while the fork-proofness is untestable until N6 — a
  meaningless-etappe smell.)
- NIP-EE's `created_at` tiebreak stays confined to the MLS epoch mechanics;
  republic STATE is still decided solely by our chain (signatures, positions,
  hard-reject).

## 6. Delivery-guarantee interplay — and the forward-secrecy tension (finding 1, HIGH)

The seductive claim was "relays are store-and-forward with history, so
subscription catch-up closes most gaps." **This is false across every epoch
change**, and epochs change on every membership change and every recovery
re-key. The 445 outer NIP-44 key derives from the epoch's `exporter_secret`,
and we pin `max_past_epochs = 0` twice in `mls.rs` (defended by
`the_evicted_leaf_cannot_speak_after_the_rekey` and
`an_old_epoch_message_is_rejected_after_a_rekey`). After a commit, the
epoch-N key schedule is gone, so a laggard cannot even STRIP THE OUTER LAYER
of any 445 published before that commit — it is an opaque blob,
indistinguishable from relay spam. NIP-EE structurally assumes you keep recent
exporter secrets to decrypt across epochs, which is the opposite of what we
pinned.

Resolution — split the two secrets explicitly:

- Keep a **bounded ring of the last K exporter secrets** for the OUTER
  (NIP-44) layer only. The exporter secret authenticates nothing and grants no
  MLS read capability, so retaining it does NOT re-open the eviction hole the
  `max_past_epochs = 0` tests pin — the INNER MLS layer still rejects an
  evicted leaf's old-epoch message. Add a test asserting exactly that
  asymmetry (outer strips, inner rejects).
- **Catch-up is bounded by the exporter ring, not by relay retention.** Beyond
  the ring, old-epoch events are epoch-opaque and must be reported loudly (G4)
  rather than silently skipped. The ACK/rewind layer — not the subscription —
  remains the guarantee: a laggard rejoining across a commit gets everything
  it is still owed via fresh resends at the current epoch.

Everything else from the 2026-07-28 work stays and mostly gets simpler:

- **AcceptedWindow + ACK frames:** ACKs are kind-445 messages like everything
  else (they are MLS frames already). But the frame is no longer pairwise: on
  a broadcast channel the `from == leg-peer` anti-spoof pin is replaced by
  "trust the MLS credential only", `AckPayload` is re-specified as "what I
  accepted from sender S" keyed by the credential, and every member now learns
  every other's acceptance state (an in-group metadata change — state it).
- **Rewind-resend:** unchanged at `acked_floor`; resend = re-publish (fresh
  encryption, new event id — the V4 msg-id-swallow problem does not exist on
  Nostr). Amplification is real and must be pinned (finding 8): N receivers ×
  R relays × rounds; publish once at the min floor, cap rounds/hour.
- **G7 `prev_seq` ordering:** unchanged (`created_at` is untrusted; our chain
  stays the truth).
- **Dedup:** event-id ring in recv_task (relay copies) + AcceptedWindow
  (engine). The reassembler ring is SMP-only.
- **The MLS ratchet windows** (guarantee §4.6: tolerance 5000 / forward 100k)
  stay — relay redelivery + catch-up overlap produce the same late frames.

## 6.5 Presence over relays (finding II-5 — the doc had no story)

Today presence pills, deaf detection, and the "offline vs. reconnecting"
honesty model (Track A) ride pairwise MLS keepalives every 120 s
(`MESH_KEEPALIVE_SECS`) that stamp `peer_seen`. On Nostr there are no queues
to warm, and both naive options hurt: broadcast keepalive 445s per member per
interval are public-relay spam AND a fixed-cadence beacon whose *traffic
pattern* re-leaks the member count that ephemeral publish keys hide (§7);
no keepalives at all regresses to traffic-only presence, so an idle republic
shows everyone offline — exactly the GUI-honesty regression the mesh-resume
saga fought.

**Decision (recommended): traffic-derived presence + honest coarseness.**
Presence advances only on real received 445s (application, ACK, commit); the
GUI states plainly that presence is *coarse* on a relay transport (a member
is "last seen" at its last message, not pinged live). No beacons → no
count-leaking cadence, no relay spam. Deaf detection is replaced entirely by
**relay** health (the recv circuit breaker, §4.1) — there are no per-peer
legs to be deaf. `net_health` reports relays, not members; the GUI copy
"reconnecting to {member}" has no relay analog and becomes "reconnecting to
relays". This is a real GUI-semantics change, budgeted in N5/N6, not a free
reuse of the mesh presence model.

## 7. Metadata comparison + wire-size reality (honest)

| Dimension | SMP (per-pair) | Nostr/NIP-EE |
|---|---|---|
| Who sees group existence | no server (queues unlinked) | relays see the `h`-tag id (rotatable; nothing else) |
| Sender identity at server | queue credential (pseudonymous) | ephemeral key PER EVENT (stronger) |
| Member count, publish side | derivable per queue-pair | hidden (ephemeral keys) |
| Member count, subscribe side | server sees delivery queue | **exposed** — distinct subscriptions on `{#h}` reveal count + IPs |
| IP protection | Tor dialer (T4) | same Tor dialer in front of the WebSockets; **onion relay by default** (§7.5) |
| Outer-layer confidentiality | n/a | group-shared exporter key — hides frames from OUTSIDERS, not from a leaked group secret |
| Invite metadata | queue in the link | `npub`+relays in the link; NIP-59 p-tags the recipient publicly |

Net: group unlinkability is redistributed, not strictly improved. Relays see
a rotatable `h` tag and — decisively — the **subscription IPs and count** of
members; SimpleX servers see unlinked queues and sender credentials. With a
self-hosted relay + Tor, Nostr is at least comparable; on foreign public
relays the `h`-tag correlation and subscription-count exposure are the price
of the robustness. §7.5 turns this into a concrete default posture rather than
leaving it as an open product decision.

**Wire-size cliff (finding 7):** §4.1's "445 carries the whole event" is only
safe for small events. Relays advertise `max_message_length` in NIP-11
(commonly 64–256 KiB), and our largest wire events are not small:
`CheckpointServed { blob: CheckpointState }` and `ChainRequest` are in
`crosses_wire` and can be large; MLS framing + NIP-44 + hex roughly doubles
the payload. A checkpoint serve can exceed a public relay's cap and be
rejected while the cursor advances on a relay that DID accept it. N2 must
probe each relay's NIP-11 cap, refuse to publish over it (loud, G4), and pin
the worst-case `CheckpointServed`/`ChainRequest` size against the smallest
configured relay's cap.

**File transfer has no Nostr path yet (finding 7):** the file data plane is
not the mesh — it uses a dedicated queue pair with 256 KiB pieces and a
4-piece flow window (`molt-net/src/transfer.rs`). A Nostr workspace has no
queues, so file sharing simply does not exist there unless designed. §10.7
decides: V1 ships file sharing OFF on Nostr (surfaced honestly in the GUI), or
we design a 445-level chunked data plane.

## 7.5 Relay reachability: onion by default, clearnet only with a warning

*Recorded as [ADR-0001](../docs/adr/0001-nostr-relay-reachability-onion-by-default.md)
(status: proposed, conditional on the §0 go/no-go).*

The `h`-tag/subscription exposure of §7 is not fought at the group layer (a
group needs *a* rendezvous), but at the **reachability** layer: who can see
the relay, and whether the relay can see member IPs. Two postures exist; the
default is decided here, and the settings UI enforces it.

**Default (recommended, nudged in settings): the group's relay(s) are Tor
onion services, reached over Tor — both ends hidden.** The relay is published
as a `.onion` address; every member dials it through the existing T4
onion-preferred dialer (the same fail-closed dialer that fronts SMP today —
§8, "same Tor dialer in front of the WebSockets"). Properties:

- The relay never sees a member's real IP (it sees the connection arriving
  from the Tor network at its rendezvous point) → the `{#h}` subscription set
  can no longer be tied to real-world identities.
- No network observer sees "IP X talks to relay Y" — the traffic is inside
  Tor end to end.
- The relay is **location-hidden**: no clearnet address to enumerate, block,
  seize, or censor. This is the same posture SimpleX uses for servers over Tor,
  and the reason §7 can claim "at least comparable".

This reuses infrastructure we already have (T4: fail-closed, onion-preferred,
no-leak harness); it is not new transport work beyond pointing the WebSocket
dialer at the onion address (N2).

**Two distinct pieces — do not conflate them.** The strong posture needs
BOTH: (a) *server-side* — the relay exposes an onion service (a Tor/arti
process beside `rnostr`); and (b) *client-side* — the member dials over Tor
(the external daemon, or our opt-in `embedded-arti` mode). "Embedded relay
behind Tor" is underspecified on its own: it must also say the clients arrive
over Tor, or the onion address buys nothing.

**Optional (selectable, gated behind a clear "insecure" warning): a clearnet
relay.** A member may point a workspace at a clearnet Nostr relay
(`wss://…`). The settings UI presents this as the non-default choice and shows
an explicit warning, because the posture is asymmetric:

- Dialing a clearnet relay *over Tor* still hides the **client's** IP from the
  relay and from observers — genuinely useful for members who do not want to
  trust the relay operator (even a fellow member) with their IP. So it is not
  worthless.
- But the **relay itself stays exposed**: a public clearnet address that a
  network observer can find, fingerprint, block, seize, and whose activity
  pattern (and thus the group's) is visible. You have hidden the leaves and
  left the trunk standing in the clearing.
- And the degenerate case the UI warning must name: tunnelling to your **own**
  clearnet relay over Tor to hide **your own** IP from **your own** server is
  close to pointless — the value of a clearnet relay is only for the
  *non-operator* members, and even then only against everything except the
  relay's own exposure.

**Residual honesty (true even for the onion default):** an onion relay still
sees the `h`-tag correlation and the subscription set — i.e. *which* (now
IP-less) subscribers share a group. On the group's **own** relay this is
acceptable (the operator already knows the roster); it is only a leak on a
*foreign* relay, which is exactly why the default steers to a self-hosted
onion relay. And the shared rendezvous re-raises the availability point of §6:
if the single onion relay is down or slow (onion services carry extra
latency), the group goes dark — so the default is **two or more** onion
relays (the native Nostr redundancy, §4.1), not one.

**Settings model (drives N2 + the N6 wizard):**

- The relay-list editor defaults to an onion-relay entry (with a short
  self-host recommendation and a link to a one-command `rnostr`-as-onion
  recipe). New Nostr foundings inherit this default.
- Adding a `wss://` clearnet relay is allowed but flagged: an inline
  "insecure — the relay is a visible, seizable clearnet target; only your
  client IP is protected, and only if Tor is on" warning, and the workspace's
  health surface carries a persistent "clearnet relay" badge so the tradeoff
  stays visible, not just accepted once at founding.
- If Tor is *off* while any relay is selected, fail closed exactly as the SMP
  path does today (T4 posture) — never dial a relay in the clear silently.

This resolves the reachability half of the §10.2 product question: the posture
is **onion-by-default, clearnet-opt-in-with-warning**, not "decide later". The
metadata-vs-usability half of §10.2 (whether to also permit *foreign* curated
public relays at all, and whether NIP-42 AUTH is required) still stands, and
NIP-42 AUTH interacts with this: AUTH re-identifies the member to the relay
via a persistent key, so on the *own onion* relay it is fine (the operator
knows the roster anyway), while on a foreign relay it would undo the ephemeral-
key hiding — another reason the default is a self-hosted onion relay.

## 8. Dependency / pure-Rust audit

- **Crypto:** secp256k1/Schnorr via `k256` (pure Rust, RustCrypto); NIP-44 v2
  = secp256k1-ECDH + ChaCha20 + HMAC-SHA256 (all RustCrypto); NIP-59 is a
  layer above. No C. k256 scalar derivation must reject/retry out-of-range
  hashes, never clamp (N1 note). k256 is younger than libsecp256k1 and only
  best-effort constant-time — but it signs only transport envelopes, never
  roster/chain, so the side-channel exposure is bounded (note in N0).
- **`rust-nostr`** (v0.44, near beta): offers Event/NIP-44/NIP-59/relay-pool
  ready-made — BUT its default backend can pull the `secp256k1` C binding;
  pin the `k256` feature or take only the protocol crates. Decide in N0 via a
  `cargo tree` audit. Fallback: a small in-house client (NIP-01 frames are
  trivial; NIP-44/59 are the real work and exist as standalone crates).
- **MDK / White Noise:** MIT, used as a REFERENCE (event layout, race
  handling to test against), not a dependency — their account/storage/runtime
  overlaps our engine entirely.
- **WebSocket:** `tokio-tungstenite` + rustls — check compatibility with our
  rustls-rustcrypto posture in N0 (avoid re-introducing `ring`).

## 9. Migration & coexistence

- **Per-workspace choice at founding** (the invite link carries the transport
  kind; config sets the default). Existing SMP republics run unchanged.
- **No live migration in V1 — and "re-found + restore" is NOT a migration**
  (finding II-4). `restore`/import is bound to the SAME workspace identity by
  a hard content check (`derive_workspace_id(seed, member) == id_hex`,
  `molt-storage/import.rs`; axiom "import restores knowledge, recovery restores
  membership", `backup_restore_design.md`). A republic re-founded on Nostr has
  a NEW `republic_id` (more so under the §3 v2 bump), a new genesis, a new
  chain — so `restore` cannot graft the old history into it. The actual outcome
  is: the old SMP workspace survives as a **permanently detached read-only
  archive**, plus a **brand-new empty** Nostr republic — no live chat history,
  no governance history, no read receipts, no files. Two similarly-named
  picker entries. This strands exactly the frustrated existing-SMP users §0
  targets. **This is a product decision, not a footnote (§10.9):** either V1
  accepts "archive + fresh start" and says so in the GUI, or a chat-history
  *export-as-log graft* into the new workspace is V1 scope, or the V2 in-place
  `TransportPolicy` migration (both transports run a WP4a-horizon grace in
  parallel) is promoted from footnote to plan.
- **GUI:** the founding wizard gains the transport choice (default from
  config); settings show each workspace's transport read-only, plus the
  relay-list editor (onion default, clearnet-with-warning per §7.5).

## 10. Open questions — split into design inputs vs. product taste (finding II-6)

Two classes. **Design inputs** MUST be answered before their named etappe —
they change what gets built, not just how it feels; the "everything below is
execution-ready" claim does not hold until they land. **Product taste** can be
decided any time before ship.

**Design inputs (gate the named etappe):**

- **§10.2 → N2 — Metadata posture (reachability half DECIDED in §7.5).** The
  reachability posture is settled: **onion relay by default, clearnet opt-in
  with a warning**, both over the T4 dialer (§7.5). What remains for §10.2:
  (i) whether *foreign* curated public relays are permitted at all or the
  product is self-host-only; (ii) NIP-42 AUTH — on the own onion relay it is
  fine, on a foreign relay it undoes the ephemeral-key hiding (§7.5), so its
  necessity follows from (i). N2 must build the WS twin of the T4 Tor no-leak
  harness plus the onion-dialing path regardless; AUTH is conditional on (i).
- **§10.3 → N3 — Interop.** Marmot compatibility vs. NIP-EE mechanics only
  decides the N3 byte layouts you are told to pin with fixtures — you cannot
  write the fixture before answering. (Recommendation: mechanics yes, interop
  no — our inner events are `EventEnvelope`s, not Marmot `app_data`.)
- **§10.6 → N1 — `republic_id` v2** (§3): include `nostr_pk` (recommended) or
  keep v1 and document the narrowed guarantee.
- **§10.10 → N1 — roster-v3 universality (NEW, finding II-3):** is
  `molt-roster-v3` **universal** (every future SMP founding also mints a dead
  `nostr_pk`, forcing ticket-salted derivation on all republics) or
  **per-transport-kind** (two canonical-byte layouts + two verify paths in
  `verify_sealed_roster` forever)? Either answer ripples through all ~15
  recompute sites; not choosing = choosing by accident. (Recommendation:
  universal — one layout, `nostr_pk` optional-but-present, cheaper than two
  verify paths.)
- **§10.7 → N6 — File transfer on Nostr** (§7): OFF in V1 with an honest GUI
  notice, or a 445-level chunked data plane. Interacts with §9/finding II-4:
  restored history contains `FileShared` messages whose data plane doesn't
  exist on the Nostr side.
- **§10.9 → N6 — Migration outcome (NEW, finding II-4):** accept "archive +
  fresh start", or graft chat-history-as-log into the new workspace, or
  promote V2 in-place migration to plan.

**Product taste (decide any time):**

- **§10.1 — Relay policy:** default relays for a new republic (recommendation:
  2 curated public + optional self-hosted, editable in the wizard) — but see
  §10.2: if the posture is self-host-only, the default changes.
- **§10.4 — Exporter-ring depth K** (§6): catch-up reach vs. leaked-secret
  window (recommendation: small, 3–5).
- **§10.5 — h-tag rotation grace length** (§4.4): tie to the WP4a horizon so
  guarantee-horizon and transport-horizon are one number. NOTE (finding II-8):
  double-publishing identical traffic to old+new `h` for 14 days makes the
  rotation trivially linkable — so `h` rotation is a relay-MIGRATION tool, not
  a privacy tool; §7's table should say so, and the grace could be shorter if
  we accept a harder cutover.
- **§10.8 — Priority:** the self-host experiment runs FIRST and gates the
  build (see §0's go/no-go). N0 audit may run in parallel.

## 11. Etappen (each green on master, TDD)

- **N0 — Spike & audit (small):** `cargo tree` audit of rust-nostr/k256/
  tungstenite; a PoC publishing + subscribing against rnostr (an `#[ignore]`
  test like `ritual_over_smp`); pin NIP-44/59 against the reference vectors;
  measure real public-relay retention + NIP-11 caps. (May run parallel to the
  §0 self-host experiment; everything after N0 is gated on the go/no-go.)
- **N0.5 — Engine-side inventory & seams (NEW, finding II-1, ESSENTIAL):**
  before any runtime code, inventory `molt-engine/src/net.rs` and adjacent:
  which of the ~30 `Net*` internals/tickers apply per transport kind; the
  relay-shaped `EngineSink` extension (relay health, not `MemberId` legs); the
  `TransportState` per-workspace transport discriminator; the `RitualTransport`
  enum split (`Loopback|Smp|Nostr`) and every dispatch site (reopen,
  recovery's `runtime_transport`, close-persist `export_creds`, the open-path
  "detached" mesh-evidence check); the `ChainOracle` seam (§5); the
  credential-keyed broadcast ACK vs. today's pairwise ACK. Output: a concrete
  refactor map + the seam trait signatures. **Deliverable is a design, not a
  feature** — but N5's sizing is fiction until it exists.
- **N1 — Identity:** k256 derivation from the phrase (ticket-salted);
  `MemberIdentity.nostr_pk` + `molt-roster-v3` bump (the ~15-site ripple, byte
  pins); MAC v2; the 3-anchor sign-what-you-see self-check; the `republic_id`
  decision. **Keystone:** roster fixture v2→v3, MAC-v2 binding, a
  split-anchor attempt rejected.
- **N2 — NostrTransport core:** relay pool (connect/backoff/health), publish
  with ≥1-OK semantics + NIP-11 size budget, per-relay cursor with clamp +
  overlap, event-id dedup, per-connection decrypt-failure circuit breaker.
  The WebSocket dialer rides the T4 onion-preferred, fail-closed path (§7.5) —
  onion `.onion` relays over Tor by default, clearnet only via the warned
  opt-in, never a silent clearnet dial. In-process relay (like LoopbackHub)
  for fast tests. **Keystones:** publish/subscribe/dedup across 2 relays with
  one dying; the +24h-cursor test; the oversized-`CheckpointServed` refusal;
  a WS twin of the T4 no-leak harness (no clearnet dial when Tor is required).
- **N3 — NIP-EE mapping + commit lifecycle:** 443-free 444/445 build+parse,
  exporter-NIP-44 with the ring, ephemeral keys, gift-wrap; the explicit
  stage→publish→await→merge commit state machine + prior-state slot.
  **Keystones:** roundtrip vectors; exporter rotation with the ring
  (outer-strips/inner-rejects asymmetry); a concurrent-commit tiebreak heals.
- **N4 — Ritual over Nostr (bigger than it looks, finding II-3):** invite-link
  v2 + recovery-link v2; join/welcome/deliberation/seal end-to-end over the
  in-process relay; recovery with the replay-safe window reset. This
  re-implements the queue-based founder-seat machine, reply handover and
  recovery handover (founding.rs + lifecycles.rs + recovery.rs are
  ~6k engine lines, pinned by ~4.5k test lines) — "only the envelopes change"
  is true of the flow, not the implementation. Consider splitting into
  N4a (founding+join) and N4b (recovery). **Keystone:** the Nostr twin of the
  `two_instances` founding + the recovery suite.
- **N5 — Runtime + guarantee + presence:** NostrGroupRuntime on
  EngineSink/OutboxLog; AcceptedWindow/ACK/G7 over it with min-floor
  single-publish resend and amplification counting; traffic-derived presence
  (§6.5); net_health = relay status; the rotation grace. **Keystones:** the
  Nostr twin of `delivery_guarantee.rs` (a relay dies/prunes → rewind-resend
  delivers, ordering holds); the offline-across-rotation convergence test;
  an idle-republic presence-honesty test.
- **N6 — Governance bridge + GUI:** `ChainChange::TransportPolicy` filling in
  the N3 block-hash-bound commit gate; wizard transport choice + the migration
  outcome (§10.9); file-transfer GUI gating (§10.7); relay-shaped health copy
  (§6.5); doc closeout (CLAUDE.md, this document to BUILT).

Rough sizing, revised after BOTH review passes (findings I-1/2/3/7 and
II-1/2/3/5 are design changes, not mappings; N0.5 and presence and the ritual
re-implementation were unbudgeted): N0 1 d; **N0.5 2 d**; N1 2 d; N2 2–3 d
(+ NIP-42/Tor-WS if §10.2 says self-host-only); N3 3–4 d; **N4 4–5 d**
(the real ritual surface); N5 3–4 d (+ presence); N6 2–3 d — **order of six
weeks beside daily work, not three.** The three-week figure in the first draft
was off by ~2×; treat any sizing before N0.5 lands as provisional.

## 12. Risks

- **Spec churn:** NIP-EE is young; we pin the event layouts with byte fixtures
  (like the chat fixtures) and follow changes deliberately. Without an interop
  goal (§10.3) we depend only on the MECHANICS, not on the Marmot ecosystem's
  cadence.
- **Relay quality:** public relays prune/limit differently — the E2E guarantee
  absorbs it; N0 measures real retention and caps.
- **A second transport doubles the test surface:** the runtime keystones
  double (the SMP twin stays mandatory); accept the CI cost.
- **The exporter ring is a security knob** (§6/§10.4): too deep widens the
  leaked-secret window; too shallow shortens catch-up. Bound it and test both
  edges.
- **Availability floods:** the outer key is group-shared, so any member (incl.
  a seized post-eviction device, for its last epoch) can mint outer-valid
  `h`-tagged spam. Require NIP-42 AUTH on self-hosted relays; treat public
  relays as best-effort mirrors with the recv circuit breaker.

## 13. Adversarial review (two grill rounds, 2026-07-29)

Two independent passes: a protocol/security lens (Ledger I) and an
architecture/product lens (Ledger II). All findings folded into the sections
above. The architecture pass's verdict: **fit as a discussion document, NOT as
an execution plan** — the §4.1 "same engine interface" claim hides an
unbudgeted engine refactor, sizing is ~2× off, and the §9 migration is
contradicted by the repo's own import path. That verdict is why the top-of-doc
framing and the §0 go/no-go gate now exist.

### Ledger I — protocol / security, most severe first

1. **HIGH — exporter_secret vs. our pinned `max_past_epochs = 0`:** naive
   "subscription catch-up" is unreadable across every epoch change. Fixed by
   the bounded exporter ring for the outer layer only, with catch-up bounded
   by the ring and the ACK/rewind layer as the real guarantee (§6, §10.4).
2. **HIGH — single-member fork:** a valid MLS "admin" can push an
   unauthorized group-data commit that honest engines "reject" → permanent
   epoch split. Fixed by binding the authorizing chain-block hash into the
   commit (drop-before-merge, fork-proof) + an explicit commit lifecycle with
   a retained prior state; `admin_pubkeys` stated as non-enforcing under
   ephemeral keys (§5).
3. **HIGH — the 3-anchor binding did not bind:** MAC didn't cover the nostr
   key, sign-what-you-see didn't self-check it, `republic_id` didn't commit to
   it, and hex/bech32/x-only gave multiple signed forms. Fixed by MAC v2, the
   extended self-check, the republic_id decision, and ingest normalization
   (§3).
4. **HIGH — cursor poisoning:** untrusted `created_at` (+ NIP-59 randomization)
   can blind a node after restart. Fixed by clamp-to-now, per-relay cursors,
   and "cursor is an optimization; correctness is the ACK layer" (§4.3).
5. **HIGH — 443/10051 privacy regression:** publishing KeyPackages leaks
   handle + keys, correlatable across all of a member's republics; NIP-59
   p-tags the recipient. Fixed by dropping both and gift-wrapping the
   KeyPackage inside the MAC-bound ritual message; ticket-salted derivation
   (§4.2, §3).
6. **HIGH — the `h` tag is the old single-point-of-deafness, relocated:** a
   node offline across an h/relay rotation is stranded. Fixed by the
   grace-window overlap tied to the WP4a horizon, recovery as the outside-grace
   fallback (§4.4).
7. **MEDIUM-HIGH — file transfer + oversized events have no Nostr path:** the
   data plane is queue-based, and `CheckpointServed`/`ChainRequest` can exceed
   relay caps. Surfaced as §10.7 + the N2 wire-size budget (§7).
8. **MEDIUM — single-outbox economics:** one cursor forces min-floor rewinds
   that amplify to N×R×rounds; the pairwise ACK shape breaks on broadcast.
   Fixed by publish-once-at-min-floor with receiver-side dedup, cost-aware
   backoff, and a re-specified credential-keyed ACK (§6, §4.1).
9. **MEDIUM-HIGH — recovery is circular + the window-reset is replayable:** a
   total-loss rejoiner can't know the relays/h-tag, and a chain-block-triggered
   reset re-fires on catch-up. Fixed by recovery-link v2 and an
   idempotent/one-shot reset guarded on block id/height (§4.2).
10. **MEDIUM — unauthenticated `h`-tag inbound + a wrong metadata claim:**
    anyone can flood outer-valid `h`-tagged events (one ECDH per garbage
    event), and §7's "member count hidden" was only true of the publish side.
    Fixed by the recv circuit breaker + NIP-42 AUTH on self-hosted relays, and
    the corrected §7 table (subscribe-side exposure).

### Ledger II — architecture / product, most severe first

1. **HIGH — the real coupling is engine-side, unbudgeted.** §4.1's trait reuse
   is true of the thin supervisor traits; `molt-engine/src/net.rs` (~4,300
   lines) is member/mesh-shaped, `EngineSink` is peer-keyed, `RitualTransport`
   is a closed `Loopback|Smp` enum, and reopen/recovery/persist dispatch on
   mesh evidence. Fixed by the NEW N0.5 engine-inventory etappe + the relay-
   shaped sink / `TransportState` discriminator / enum split (§11, §0 point 3).
2. **HIGH — chain-gated commit needs engine data below the engine, and N3 is
   ordered before its N6 gate.** Fixed by the `ChainOracle` seam in `molt-net`
   (synchronous, no round-trip gap) with its contract + hard-reject test moved
   into N3 (§5).
3. **HIGH — "ritual flow stays, only envelopes change" underplays a ~6k-line
   re-implementation; roster-v3 universality is an undecided signature fork.**
   Fixed by re-sizing N4 (split N4a/N4b) and adding §10.10 (universal vs.
   per-transport roster bytes) as an N1 gate (§11, §10).
4. **HIGH — §9 "re-found + restore" is a data-loss trap.** `restore` is bound
   to the same workspace id by a hard check → a new empty republic + a detached
   archive, no live history. Fixed by the honest §9 rewrite + §10.9 (§9, §0
   point 4).
5. **MEDIUM-HIGH — presence had no Nostr story.** Fixed by the new §6.5
   (traffic-derived presence, honest coarseness, no count-leaking beacons,
   relay-shaped health).
6. **MEDIUM-HIGH — three §10 "questions" are load-bearing design inputs.**
   Fixed by splitting §10 into design-inputs (gate an etappe: metadata/AUTH→N2,
   interop→N3, republic_id→N1, roster-universality→N1, files→N6, migration→N6)
   vs. product-taste (§10).
7. **MEDIUM — strongest case AGAINST the dual path:** the diagnosis flipped
   once (mesh_reliability.md §0), self-hosting is a one-day fix for both
   grievances, and dual dooms the non-driven transport to bitrot (SMP needed
   six live-hardening campaigns to reach today's shape). Fixed by making the
   §0 go/no-go gate binding and reframing Nostr as a replacement candidate, not
   a permanent second transport (§0).
8. **MEDIUM — the per-workspace fork is an incoherent product surface, and the
   rotation grace defeats its own privacy purpose.** Fixed by demanding the
   "who chooses Nostr and why" paragraph (§9/§10.2) and noting that
   double-publishing to old+new `h` for 14 days makes `h` rotation a
   migration-only tool (§10.5, §7).
