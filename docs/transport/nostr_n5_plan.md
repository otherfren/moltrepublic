# N5 — the group runtime, the guarantee, presence

**Status: PLANNED.** Anchors verified 2026-08-02 against HEAD. Spec:
`nostr_transport_marmot.md` §4.1 (runtime shape), §4.3 (cursors), §4.4
(rotation grace), §6 (guarantee), §6.5 (presence), §11 (the etappe entry).

N5 is what unblocks **N4b step 6**: the rejoiner's chain catch-up needs a
transport that carries `WorkspaceEvent`s over 445, and nothing more — the
catch-up protocol itself already exists and is transport-agnostic
(`nostr_n4_plan.md` §8.8 step 10, resolved 2026-08-02).

## 1. What already exists (do not rebuild)

| asset | where | note |
|---|---|---|
| `AcceptedWindow`, `OutboundCursor` | `molt-core` | pure serde, no I/O — crosses untouched, byte fixtures included |
| the four floor functions | `molt-net/src/supervisor.rs:949-1018` | `rewind_unacked`, `own_ackable`, `advance_acked_floor`, `record_acked` — pure over engine data; a sibling module calls them by widening `fn` → `pub(crate) fn` |
| `OutboxLog`, `StateStore` | `supervisor.rs:262-319` | name no transport concept; reusable verbatim |
| `MESH_ACK_TAG` | `molt-net/src/lib.rs:95` | transport-neutral, two non-test sites; a 445 carries the same MLS frame, so no version bump |
| h-tag rotation | `envelope.rs:115-159` | `h_tag`, `h_tags_for_catchup` (written, tested, **zero src callers**) |
| publish/subscribe primitives | `ritual_net.rs:393-465` | `publish_frame` seals with the exporter secret and returns the carrier stamp; the N4a ritual already exercises them |
| EOSE / "history complete" | `relay_runtime.rs:346-392` | `synced()`, `sync_state()`, surfaced as `GroupSub::live_state` |
| chain catch-up | `molt-engine/src/chain.rs:1716-1800` | `receive_block`/`request_catchup`/`drain_buffered_blocks`, incremental verify; `ChainRequest`+`Committed` already in `crosses_wire` |

**The 445 filter carries no `since`/`until`/`limit`** (`ritual_net.rs:449`), so
every fresh placement is already a full history query under its tags. Nothing
needs inventing at the filter layer.

## 2. The four traps (each cost a session if met at runtime)

1. **`subscribe()` names only the CURRENT window's tag** (`ritual_net.rs:434`,
   plus one adjacent inside the 1 h skew). h tags are
   `SHA256(seed ‖ le64(unix/86400))`, so weeks-old frames sit under tags we
   never ask for. Old 445s are unreachable **because we do not ask**, not
   because relays pruned them.
2. **The window-roll detector REPLACES the tag set.** `GroupSub::recv`
   recomputes `current = window_tags(seed, now)` every iteration and, when not
   covered, re-places with exactly `current`, overwriting `self.tags`
   (`ritual_net.rs:519-533`). A catch-up placed inside an hour of a UTC
   boundary is discarded immediately. **A catch-up cannot be a `GroupSub`** —
   give it its own type, or make the roll UNION into `self.tags`.
3. **The per-relay cursor is a MAX.** `read_session` advances it on every
   delivered event (`relay_runtime.rs:742`), and `place_req` then applies
   `since = cursor - 172_800` on reconnect (`:566`). One fresh event mid-replay
   amputates the history. A catch-up must run with cursors disabled, and treat
   a reconnect during replay as "restart the range", never "resume".
4. **Catch-up is bounded by the EXPORTER RING, not by relay retention**
   (marmot §6). After a commit the epoch-N key schedule is gone, so a laggard
   cannot even strip the OUTER layer of a 445 published before it. Relay
   history does not rescue a member who slept through an epoch change — the
   ring does, and beyond the ring the answer must be a LOUD G4 report, never
   silence.

## 3. The one real re-design: the ACK frame

Today the ACK is pairwise: `AckPayload` describes one peer's acceptance, and
the recv loop pins `from == peer.member` (`supervisor.rs:1369`, and again for
envelopes at `:1465`). On a broadcast channel there is no leg peer, so:

- `AckPayload` is re-specified as **credential-keyed** — it must name whose
  window it reports, because one 445 reaches everyone.
- The anti-spoof pin becomes **"trust the MLS credential only"**. `from`
  already comes out of `MlsIncoming::Application { from, .. }`
  (`supervisor.rs:148`), which is credential-authenticated; the pairwise
  comparison adds nothing on a broadcast and would reject every legitimate ACK.

This is security-relevant: dropping the pin without keeping the credential
check would accept a forged sender. Keep the credential, drop the leg.

## 4. Steps (one commit each, red test first)

### N5.1 — the catch-up subscription
`GroupChannel::subscribe_since(since_secs, max_windows) -> CatchupSub`, feeding
`h_tags_for_catchup` into the already-private `subscribe_tags`. Its own type,
with no window-roll (trap 2) and no cursors (trap 3).
- **Red:** publish a 445 under a PAST window's tag, then a live `subscribe()`
  must NOT see it and `subscribe_since` MUST — over a `MockRelay`. That pair is
  the whole point: it fails today for the reason trap 1 names.

### N5.0 — a frame discriminator on the 445 plaintext *(do before N5.2)*

**Verified 2026-08-02.** Two producers already put different things inside a
445's MLS plaintext, and neither tags what it is:

- the ritual sends `serde_json::to_vec(RitualMsg)` — bare JSON
  (`nostr_ritual.rs:303`);
- the supervisor sends `serde_json::to_vec(EventEnvelope)` — also bare JSON —
  or a `MESH_ACK_TAG`-prefixed control frame (`supervisor.rs:155`).

Today they never meet: the ritual owns the channel before the runtime exists.
N5.2 is exactly the change that makes them meet, and then a receiver tells a
`RitualMsg` from an `EventEnvelope` by *trying to parse one and seeing if it
works*.

**✅ DONE, and smaller than it first looked.** The first version of this entry
called that ambiguous and prescribed a byte tag. Checking before building
showed the shapes are in fact robustly disjoint: `RitualMsg` is internally
tagged (`#[serde(tag = "kind")]`) so it demands a `kind` field, `EventEnvelope`
demands `seq`/`ts`/`by`/`body`, and the break that would collapse the
difference — giving `EventEnvelope` struct-level serde defaults — does not even
compile, because `WorkspaceEvent` has no `Default`.

So a wire-breaking tag would have bought a property we already have. What was
actually missing is that **nothing pinned it**: the disjointness is a
consequence of two unrelated serde choices, and it holds until someone adds an
explicit `#[serde(default = "…")]` or a `kind` field. That is now a test
(`molt-net/tests/frame_disjointness.rs`) asserting both directions fail to
cross-decode AND that each still round-trips as itself — so it cannot pass by
both shapes having become undecodable.

`MESH_ACK_TAG` stays the model for anything genuinely new on this channel: it
leads with NUL, which no JSON document may, so it is disjoint by construction
rather than by accident. Pinned in the same test.

### N5.2 — the group runtime ✅ DONE 2026-08-02
`spawn_group(...)` beside `supervisor::spawn` — a NEW function, not a
parameterization: `spawn` is bound to `T: Transport` and `Vec<PeerLink>`
(`supervisor.rs:510`), and marmot §4.1 already says the runtime is not behind
the queue trait. Two tasks: outbox (read log from cursor → MLS → 445 publish,
cursor advances on ≥1 relay-OK) and inbox (445 → MLS → `EngineSink`).
- **Red:** two engines over one `MockRelay` converge a chat message with no
  mesh and no queues.

### N5.3 — the guarantee over broadcast ✅ ACK LAYER DONE 2026-08-02
Re-home the four floor functions, re-specify `AckPayload` (§3), replace
`flush_due_acks`'s per-peer loop (`molt-engine/src/net.rs:2294-2380`, ~90 lines,
the most concentrated fork point) with ONE 445 carrying the group ack state.
- ✅ The ack layer: `group_ack.rs` (own tag, versioned, owner-keyed sheet),
  `group_control_frame` (one-lock framer for control plaintext), the ack task
  (a THIRD task, so a sheet never queues behind `publish_with_backoff`),
  `flush_group_ack` engine-side, and `apply_group_ack` on receipt. Keystone:
  `nostr_runtime.rs::a_broadcast_ack_moves_the_senders_proven_floor`.
- ✅ **The resend timer (N5.3b), 2026-08-02.** Stall clock mirroring the mesh:
  resend only when `ack_seen && own-ackable tail`, an anchored backoff
  (10 s → 600 s), an hourly budget PERSISTED with the cursor, loud after 8
  fruitless rounds. Keystone: `nostr_delivery_guarantee.rs`, over a relay that
  is PRUNED at the moment of the gap.
- ~~STILL OPEN — the resend timer.~~ Acks now move the proven floor and
  `rewind_group` acts on it, but nothing yet RE-publishes a stalled tail on a
  timer. Until that lands the guarantee proves delivery without repairing a
  loss. **Red for it:** the Nostr twin of `delivery_guarantee.rs` over a
  FORGETFUL relay (`LocalRelay` + `MemoryDatabase::new()` — it accepts, OKs,
  live-broadcasts and serves zero history). Do NOT reach for `MockRelay`: it
  stores ~75k events and every reopened subscription is a full history query,
  so the keystone would pass with the entire guarantee absent.

### N5.3c — the group runtime has no epoch handling *(found 2026-08-02)*
`ingest_one` (`group_runtime.rs:600`) matches `Deliver`/`Ack`/`GroupAck` and
routes everything else to `_ => Ingest::Nothing` — including the two arms
whose own doc comments say what to do:

- `EpochAdvanced` — "ack it and retry the epoch buffer";
- `FutureEpoch` — "hold it (acks unfired) and retry after the next commit
  merges". The group runtime holds nothing, so such a frame is **dropped**.

The mesh supervisor implements both (`epoch_watch`, the held-message retry).
Latent today only because nothing produces a commit on 445 yet — and N4b's
recovery re-key is exactly what starts. **Prerequisite for N4b step 6c**
(`nostr_n4b_step6_design.md`), not for N5's own keystones.

### N5.4 — epoch-ring honesty (G4)
A frame older than the exporter ring is undecryptable **by construction**.
Report it loudly rather than dropping it quietly.
- **Red:** a laggard past the ring gets a named failure, not silence.

### N5.5 — presence + net_health
Traffic-derived presence (§6.5); `net_health` becomes relay status. This is
where the "N5 pending" `Down` state finally goes green honestly.
- **Red:** an idle republic does not report its members offline.

### N5.6 — N4b step 6 falls out
The rejoiner gets a chain **ANCHOR** in the Welcome and catches up over the
live runtime. Deletes the last `NO_TRANSPORT_YET` (`lifecycles.rs:1441`,
`lib.rs:88`).

**Not the HEAD** — that reading was wrong and is corrected in
`nostr_n4_plan.md` §8.8 step 10: a headless node cannot even ask for blocks
(`request_catchup` returns immediately), its `Committed`/`ChainRequest`
ingest is gated on having a head, and a head lifted out of a Welcome is an
unverified claim rather than a head, because `verify_chain` is
all-or-nothing from the anchor. The anchor is `chain[0]` — the genesis, or
for a compacted republic the WP4b checkpoint blob with its anchor block —
and it is a valid chain of length 1 the rejoiner verifies for itself.

## 5. Ordering conflict to resolve

§4.4's **rotation grace is about RELAY-LIST changes**, not the h tag (the h tag
rotates deterministically with no grace — an absent member re-derives every
window it missed). A relay-list change needs `ChainChange::TransportPolicy`,
which §11 assigns to **N6**. So either the grace slips to N6, or
`TransportPolicy` moves forward into N5.

Recommendation: **slip the grace to N6**, and note it in §11 — N5 is already
wide, and the grace has no consumer until a relay list can actually change.
This also lines up with `relay_topology_plan.md` R6, which needs the same
governed change.
