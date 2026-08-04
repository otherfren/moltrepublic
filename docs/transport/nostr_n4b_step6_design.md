# N4b step 6 — the Nostr rejoiner, and where its trust root comes from

**Status: DESIGN, ratified by measurement 2026-08-02.** Read
`nostr_n4_plan.md` §8.8 (steps 5–7, 10), `docs/chain/persistent_chain.md`
§6.1, `docs/chain/log_compaction.md` §B.5.

Step 6 is the `RecoverStart` twin of N4a's `spawn_member_join`. Everything
about the ritual leg is mechanical. The one real design question is the one
§8.8 step 10 ended on:

> What is the smallest prefix a rejoiner can be handed such that it has a
> verified head?

## 1. Why the obvious answers are all wrong

**Not the whole chain.** Measured (`welcome_chain_budget.rs`): one
`set_image` block with a 25 KiB logo costs 69318 B against the 65408 B
gift-wrap cap. Chain length is not a property a republic could stay under —
it is one proposal away from false, forever.

**Not the HEAD.** A head lifted out of a Welcome is an unverified claim:
`verify_chain` is all-or-nothing from the anchor. And a headless node cannot
use one anyway — `request_catchup` returns immediately when
`chain_head.is_none()`, and `is_chain_governed()` **is** `chain_head.is_some()`,
which gates the `Committed`/`ChainRequest` ingest.

**Not the ANCHOR in the Welcome either.** This is the new finding.
A full holder's anchor is the genesis: kilobytes, fine. A **pruned** holder
has no genesis — its root is the WP4b checkpoint blob, and the blob carries
`applied`, every payload below the cut, images included, because that is what
keeps pre-cut entries readable after the drop. Measured: a blob holding one
25 KiB logo costs **69628 B against the same 65408 B cap**.

And pruned is the steady state, not an edge case: `AUTO_CHECKPOINT_MIN_LEN`
is 32, so a republic compacts after 32 blocks — after which **no** survivor
holds a genesis to serve instead.

## 2. The bootstrap circularity, at its source

`WorkspaceEvent::CheckpointServed` already crosses the wire
(`net.rs:405`) and `receive_checkpoint_blob` (`chain.rs:2062`) explicitly
handles `chain_head: None` — `behind = true`. `receive_block`'s headless
branch (`chain.rs:1889`) is documented for exactly this case: "a headless
rejoiner bootstraps its chain from the genesis a survivor serves".

But the ingest gate at `net.rs:1379` is `if self.is_chain_governed()`. So a
headless rejoiner **drops the very frame that would give it a head**. The
headless branch is a dead branch: written for a case the caller cannot
produce.

## 3. Decision: the root travels over 445, assembled by the TASK

Not by the engine's ingest. Two reasons, and the second is the load-bearing
one:

1. Ungating the engine's chain ingest means a node with no workspace, no
   `replica` and no `republic_id()` accepting chain material — a new trust
   path, for one moment in one flow.
2. The rejoiner **task** already holds the anchor that path would need: the
   recovery link's `republic_id`. It is off-actor, exactly like
   `spawn_member_join`. And `cmd_net_recover_sealed` already takes the whole
   `ServedChainWire` as a string and **re-verifies everything** before
   materializing (defence in depth, symmetric with `cmd_net_join_sealed`).

So the task assembles the root and hands the actor the same shape loopback
hands it today. The actor's contract does not change at all.

### 3.1 The coordinator serves the ANCHOR, not the chain

New: `serve_chain_anchor()` beside `serve_chain_from`. It emits

- `CheckpointServed { blob }` — only when this holder is pruned, and
- exactly one `Committed(chain[0])` — the genesis, or the checkpoint anchor
  block.

That is the smallest prefix that verifies standalone:

| holder | what verifies it | resulting head |
|---|---|---|
| full | `verify_chain(&[genesis])` | height 0 |
| pruned | `verify_suffix_chain(blob, &[anchor], rid)` | `anchor.height` |

Everything above the anchor arrives over the **ordinary** catch-up, because
by then the rejoiner has materialized, has a head, and `is_chain_governed()`
is true. No second rail, no bespoke fetch — the thing §8.8 step 10
explicitly said would be the wrong move.

The coordinator pushes it right after the Welcome, because the rejoiner
cannot `record()` a `ChainRequest` before it has a workspace. **After the
`MlsCommit`**, and that ordering is load-bearing: the anchor has to be
encrypted at an epoch the returning member can read, which is the one the
re-key just created for it.

### 3.1a MOSTLY resolved (2026-08-04) — one race still loses the catch-up

**Correction to an earlier claim in this section.** Five defects were found
and fixed and the catch-up now runs — but it is not reliable: measured
2026-08-04, roughly **one run in four** loses it, and the capstone therefore
does NOT assert it (a flaky keystone is worse than a named gap).

**The race, diagnosed from instrumented logs.** The rejoiner subscribes to
445 only after its Welcome. Envelopes the coordinator published BEFORE that
moment are gone for it — the relay does not replay them into the new
subscription's window. Its first arriving envelope becomes the
fresh-incarnation ORDERING BASELINE (the rule §9 of
`delivery_guarantee.md` describes), and that baseline is whatever happened
to arrive first — in the losing runs the coordinator's seq 9, not its
current seq 12. The served catch-up blocks then arrive as seqs 13/14/15
with `prev_seq` 12/13/14, so 13 parks on a predecessor that can never
arrive, and 14/15 park behind it. The park's pathology valve is 900 s.

The delivery guarantee's own repair — the rejoiner ACKs, the coordinator
rewinds its broadcast cursor to the proven floor and republishes the span —
is the mechanism that SHOULD close this, and it left no trace within the
30 s window. That is the next thing to instrument: whether the fresh
incarnation's ack sheet reaches the coordinator at all, and whether
`group_floor` counts it.

**Consequence today:** a recovered seat usually catches up within a second,
and sometimes stays at its anchor until the park's valve releases. Honestly
behind, never silently wrong — but not yet the guarantee the rest of the
transport holds to.

### What the five fixes were

**Verified 2026-08-03 by the capstone, with a republic whose head is three
blocks above its anchor.** §3.1 above says "everything above the anchor
arrives over the ORDINARY catch-up". A rejoiner materialized correctly from
the anchor and stayed there. FIVE defects stacked on this one path; the
capstone now asserts the renames above the anchor arrive
(`a_lost_seat_rejoins_the_republic_over_relays`).

Three blockers were found and FIXED on the way, each real on its own:

1. **The recovered seat had no group runtime.** `cmd_net_recover_sealed`
   stood up the queue mesh and never `build_group_net` — so a recovered Nostr
   seat came back deaf: no 445 subscription, no outbox. It looked recovered
   and was frozen. (`net_health` now asserts `Ok` in the capstone.)
2. **Nothing issued the request.** The two triggers are a gap-block arriving
   and a workspace OPEN, and a recovery hits neither. It cannot hit the
   first: the coordinator's own head block was published at the epoch BEFORE
   the re-key, and a rejoiner that joined at the new one can never decrypt it
   (an exporter ring reaches backward only). `cmd_net_recover_sealed` asks
   explicitly now, after the runtime is up.
3. **The coordinator swallowed the rejoiner's envelopes as duplicates.** The
   returning seat is a new incarnation whose log seq space restarts at 1, and
   `reset_peer_accept_window` was wired only into the MESH recovery-announce,
   which a Nostr republic has no equivalent of. It now runs at the re-key,
   which is a stronger authenticated point (a threshold-committed `Restored`
   block for exactly this seat).

Instrumentation (2026-08-04) then found the remaining two — the request went
out, was served, and the ANSWER was discarded:

4. **The rejoiner held every served block as out-of-order, forever.** G7's
   in-order hold parks an envelope whose stamped `prev_seq` is not in the
   receiver's accept window — and a fresh incarnation holds NO history with
   the coordinator, whose predecessors were published at epochs the
   rejoiner's exporter ring can never open. The park waited on frames that
   cannot exist for this member. Fix: the fresh-incarnation rule — an empty
   `AcceptedWindow` delivers its first envelope unordered and seeds the
   window as the ordering baseline (`delivery_guarantee.md` §9, pinned by
   `a_first_contact_envelope_delivers_without_a_history_to_order_against`).
5. **The recovery arm built a SECOND group runtime** next to the one
   `materialize_workspace` had already brought up, and replacing the first
   lost its stop: the stop rode `Notify::notify_waiters`, which does not
   latch, so a handle dropped before its tasks first polled signalled into
   the void — an orphaned outbox published every frame of the rejoiner
   twice. Fix: one build (materialize's), and the stop is a `watch` channel
   (pinned by `a_stop_sent_before_the_tasks_first_poll_still_stops_them`).

### 3.2 The residual limit, stated rather than hidden

A blob larger than the 445 publish budget (`DEFAULT_SIZE_BUDGET`, 128 KiB,
or the smallest relay's NIP-11 cap) still cannot be served. That limit is
**pre-existing** — it is the same one WP4b already has for serving any
lagging peer, not something this design introduces — but recovery makes it
reachable by a user, so it must fail with a named reason, never silently.

Recorded as the open item it was: bounding `applied` payload bytes (the
`set_image` embedding) is the real fix and belonged with the image cap, not
here. **Both halves landed 2026-08-03**: the derived payload cap
(`6c8e4ce`) and the checkpoint summary rule (`1367b99`, checkpoint-v4), so
the blob no longer accumulates every logo a republic ever had.

### 3.3 The coordinator could not re-key at all — found 2026-08-02, FIXED 2026-08-03

`coordinator_rekey` was **mesh-only**. It reached the group through
`restore_member_on_group`, which reads `NetRuntime::real_crypto` — and a
Nostr workspace has **no `NetRuntime`**. Its group MLS lives on `GroupNet`.
So on a Nostr republic the whole function fell into its `None` arm and logged
"no runtime MLS group to re-key (state-only)", which was not even true: there
IS a group, in another field.

This was bigger than §8.8 step 6 records, and it is the reason step 6b landed
`serve_chain_anchor` without a caller: wiring it into the `else` of the queue
branch would have been an **unreachable branch dressed as a feature** — the
inert-code trap this project keeps meeting. That caller landed with the Nostr
re-key (6c below), so neither is inert now.

## 3.4 Traps this will be walked into (multi-agent recon, 2026-08-02)

Recorded because each one produces a test that passes for the wrong reason,
or a bug that only shows under load.

- **Do not copy join's possession check.** `lifecycles.rs:1338` compares
  `nostr_pk_for_sk(sk)` against `our_seat.nostr_pk` — for a rejoiner that is
  the **dead founding anchor**. It either always fails, or gets "fixed" by
  deletion, which silently accepts a mismatched key. Re-derive
  `nostr_identity(seed_entropy(phrase), inv.ticket)` on the actor and require
  equality with the delivered secret's pk.
- **Do not copy join's relay-honesty gate.** `nostr_ritual.rs:839` compares
  the payload's relays to the invite's, which is founding-only ("the two sets
  are the same by construction"). Since R3 the pool is chain-governed: check
  against the **verified anchor's** `sealed.relays`, and treat the link's
  list as only what this node may dial.
- **Do not bump `WELCOME_PAYLOAD_VERSION`.** `welcome.rs:127` hard-rejects a
  mismatch, so a bump makes every FOUNDING Welcome unreadable to older
  builds — for a field founding does not use. This design adds nothing to the
  Welcome anyway.
- **`catchup_from` is a latch.** Cleared only by `apply_next_block` and the
  workspace reset. A lost serve never re-arms the request, so a dropped
  anchor is a permanent silent stall, not a retry.
- **Epoch ordering cuts both ways.** The rejoiner joins at epoch N+1. A
  survivor that has not yet merged the `MlsCommit` cannot decrypt the
  rejoiner's first `ChainRequest` and drops it. Assert on eventual
  convergence, never on one request landing.
- **A capstone whose lost member never spoke proves nothing about step 9.**
  The survivor's `AcceptedWindow` for that member would be empty, so the
  dedup never fires and the test goes green with the window reset absent.
- **`MockRelay` replays full history**, so "the catch-up worked" can be
  satisfied by the RELAY re-serving old 445s. Use the forgetful
  `LocalRelay` + `MemoryDatabase`, as `nostr_delivery_guarantee.rs` does.

**Separate pre-existing bug, found here — FIXED 2026-08-03 (`6c8e4ce`):**
the outbox holds its cursor when a publish is refused, and
`RelayRuntime::publish` refuses anything over the smallest relay's NIP-11
cap. So **one** oversized `Applied{set_image}` block wedged that node's
entire outbox, not merely that block. The propose-time cap is now derived
from the publish budget (`proposals.rs::payload_fits`), so an over-budget
payload never enters the chain, and a locally refused publish is reported as
permanent instead of being retried on a futile backoff.

## 4. Steps (one commit each, red test first)

### 6a — the pool survives the reconstruction ✅ DONE (`a0b864a`)
`sealed_roster_from_genesis`/`_from_blob` dropped the ratified relay pool.
Found while tracing this design; fixed first because step 6 stands on it.

### 6b — `serve_chain_anchor`
- **Red:** a pruned survivor asked for the anchor emits `CheckpointServed`
  plus exactly ONE `Committed`, and the pair verifies standalone via
  `verify_served`; a full survivor emits the genesis alone and it verifies.
  The test must assert the emitted COUNT — an implementation that serves the
  whole chain would otherwise pass while reintroducing the size cliff.

### 6c — the coordinator can re-key on Nostr

**Three prerequisites, all verified 2026-08-02, none of them in §8.8.** A
recovery is the first thing in this product that produces an MLS *commit* on
the group channel, and the Nostr group runtime was built without one.

**(i) There is no Nostr re-key.** `coordinator_rekey` reaches the group via
`restore_member_on_group` → `NetRuntime::real_crypto`. A Nostr workspace has
no `NetRuntime`; its group MLS is `GroupNet.mls`. Needs a `GroupNet` arm.

**(ii) ✅ DONE — the carrier stamp is not optional here, it is a divergence.**
`molt-net/CLAUDE.md`: `CommitKey(created_at, sha256(commit))`, lowest wins,
and *"the stamp must come from the SAME source on both sides."* On 445 the
receive side already uses the real `created_at`
(`group_runtime.rs:605`, deliberately). So a sender passing
`NO_CARRIER_STAMP` would key its own commit at 0 while every receiver keys it
at the wire time — the two ends pick different winners of a same-epoch race
and **diverge permanently under one epoch number, silently**. That is the
exact failure the rule exists to prevent, mirrored.

The sender must therefore pin the stamp BEFORE committing, and
`publish_frame` (`ritual_net.rs:406`) generates `now` internally and only
returns it afterwards. It needs a caller-supplied variant — and the supplied
value must drive the **h tag too**, for the reason already in that function:
deriving them separately can straddle a window boundary and publish a stamp
its own tag disowns. So this is N4b step 8, and it lands with 6c rather than
after it.

**(iii) ✅ DONE (`9900f36`) — the group runtime had no epoch handling.** `ingest_one`
(`group_runtime.rs:600`) matches `Deliver`/`Ack`/`GroupAck` and sends
everything else to `_ => Ingest::Nothing` — including `EpochAdvanced`
("ack it and retry the epoch buffer") and `FutureEpoch` ("hold it and retry
after the next commit merges"). The mesh supervisor implements both; the
group runtime implements neither, so a frame that arrives ahead of its commit
is **dropped rather than held**. Latent because nothing commits on 445 yet —
and recovery is precisely what starts.

Building it found two more, both real and both now fixed: a commit was
sealed under the exporter of the epoch the sender had just moved TO (opaque
to exactly its recipients, since a receiver's ring reaches backward only),
and on 445 a future-epoch frame fails at the OUTER layer as `Opaque`, so a
hold keyed on `MlsDecode::FutureEpoch` alone would never have fired. See
`nostr_n5_plan.md` N5.3c.

Then: re-key on `GroupNet.mls`, publish the commit at the pinned stamp,
gift-wrap the 444 to the rejoiner's **NEW** anchor (`working_nostr_pk`
already returns it — `project_one` folds the `Restored` block before
`after_block_applied` runs), then `serve_chain_anchor()`.
- **Red:** on a Nostr republic a committed `Restored` block produces a
  Welcome and a chain-anchor offer; today it produces a log line and nothing
  else. Plus: both ends compute the same `CommitKey` over the production
  entry points (the N3 lesson — a keystone driving an API the product does
  not call pins nothing).

#### 6c ✅ DONE 2026-08-03

`chain.rs::nostr_rekey` (the MLS half, a free function so its rules are
testable without a live runtime) + `State::coordinator_rekey_nostr` (the
wiring) + `nostr_ritual::spawn_rekey_delivery` (the off-actor publish).
`coordinator_rekey` routes on `group_net.is_some()`; the mesh `None` arm no
longer has to lie about a group it cannot see. `serve_chain_anchor` has its
caller, so 6b is no longer inert.

Two decisions worth keeping:

- **The commit does NOT ride the log as an `MlsCommit` envelope** (the mesh
  arm records one). It is published directly, because the outbox picks its
  own publish time and the whole point of the pinned stamp is that the
  coordinator chooses it. Recording it too would publish the same commit
  twice, at two different stamps — the divergence, self-inflicted.
- **The Welcome is sent only if the commit landed.** The two failures are
  not symmetric: a Welcome without its commit puts the rejoiner at an epoch
  no survivor reaches (a split, with nothing to heal it), while a commit
  without its Welcome leaves the seat unable to return — which the re-mint
  failover already covers. So the recoverable failure is the one to prefer.
  Relays store the commit, so one accepting relay makes it durable for a
  survivor that was offline.

The tests pin the rules — the commit seals under the epoch its recipients
are still at (verified red without the fix), the stamp it is keyed with is
the stamp it carries, the Welcome really readmits the seat.

**The composition is pinned too, since 6e**: the step-6 capstone
(`nostr_recovery.rs::a_lost_seat_rejoins_the_republic_over_relays`) drives a
live republic through a whole recovery, and disabling this arm makes it time
out. The 2-of-3 harness turned out to be unnecessary — **1-of-2 is the right
shape**, and for a reason worth keeping: at m=2 the LOST seat's own signature
would be needed to re-admit it, so a 2-of-2 republic can never recover a
member at all. That is a real product limit, and the capstone is where it
would otherwise have been discovered.

### 6d — `NetRecoverSealed` carries the Nostr shape
Mirror `NetJoinSealed`: `nostr_sk`, `relays`, `rotation_seed`, `kind`.
`cmd_net_recover_sealed` stops passing `TransportShape::default()`.
- **Red:** a recovered workspace's `transport.state` holds a `nostr_sk` that
  pairs with the anchor in its own `Restored` block, and the ratified pool.

#### 6d ✅ DONE 2026-08-03

`NetRecoverSealed` gained `nostr_sk` + `rotation_seed`, and
`cmd_net_recover_sealed` stopped materializing `TransportShape::default()`.
It does **not** simply mirror `NetJoinSealed`, because two of the three
pieces have a better source than the task:

| piece | from | why not the task |
|---|---|---|
| relay pool | the VERIFIED chain (`sealed.relays`) | the pool is chain-governed since roster-v4, so the chain is the authority; the Welcome's copy is only what the task checked against. Sealing the task's list would let a coordinator narrow a recovering seat's view of its own republic. |
| rotation seed | the task | only the Welcome carries it; the chain has no record of the h-tag seed |
| transport secret | re-derived on the actor from `(phrase, ticket)` | see below |

The secret is re-derived rather than compared against the roster, which is
the §3.4 trap made concrete: a rejoiner's roster entry is its **dead founding
anchor**, so a join-style comparison either always fails or gets "fixed" by
deleting it, which then accepts any key at all. The delivered value is kept
as a cross-check (it proves the task signed its request with the key the
chain anchored), and the chain must really carry that key as the seat's
working anchor.

**Which is not `head.identities`** — `apply_membership` keeps a seat's
anchored *identity* key across a `Restored` block on purpose (a different one
there would let m-of-n survivors hijack the seat), so the re-anchored
transport key is a projection ALONGSIDE the roster. That fold now lives in
one place, `chain::working_anchors`.

#### VERIFIED DEFECT, found here, still open: a compaction loses the working anchor

`CheckpointState.roster` is built by the same `apply_membership`, so it
carries the **founding** anchor. The `Restored` block that re-anchored a seat
is dropped with the history at a cut, and `chain_anchors` is folded from the
surviving suffix only. So after a compaction:

> every member that had recovered becomes addressable only at the key it no
> longer holds — silently, which is exactly what `State::chain_anchors`
> documents itself as existing to prevent.

Reachable in the ordinary course: `AUTO_CHECKPOINT_MIN_LEN` is 32. Today one
send site reads it (`coordinator_rekey_nostr`'s Welcome address), so the
blast radius is "a member that recovers twice, after a compaction, is sent
its Welcome at a dead key" — but N5 adds send sites, and the projection's own
doc comment warns about precisely this.

The fix is to let the checkpoint carry the working anchor. The cheap form —
re-anchoring `nostr_pk` inside `apply_membership` so the blob's roster holds
it — needs a `checkpoint-v4 → v5` bump by the same argument v4 itself needed,
and needs the ripple checked: whether any recompute site reads a live
roster's `nostr_pk` expecting the founding value.

### 6e — the rejoiner task ✅ DONE 2026-08-03
`spawn_recovery_rejoiner`, the `spawn_member_join` twin: ephemeral key from
the RECOVERY ticket → 1059 inbox → readable-gate → gift-wrapped
`RecoverRequest` (new anchor + seat proof v2) → wait the 15-min absolute
`RECOVERY_WELCOME_TIMEOUT` → peel the 444 → `join_from_welcome` → subscribe
445 under the Welcome's `rotation_seed` → assemble until `verify_served`
succeeds → `NetRecoverSealed`. Deletes the last `NO_TRANSPORT_YET`
(`lifecycles.rs:1497`, const at `lib.rs:88`).
- **Red:** the honest failure (`recover-failed:` with the not-built-yet
  text) flips to a materialized workspace on a real relay. ✅ — and verified
  red twice over, once with the rejoiner spawn disabled and once with the
  Nostr re-key arm disabled.

`NO_TRANSPORT_YET` is gone; what remains is `LEGACY_RECOVERY_LINK`, for a
link carrying no v2 handover. That is not the same statement: recovery IS
built now, and a queue-shaped link names an SMP server this build no longer
speaks to.

**One check had to be weakened, and the capstone is what found it.** 6d
required the served chain to carry this seat's new anchor. It does not: a
Nostr coordinator serves the chain ANCHOR, and the seat's own `Restored`
block is at the HEAD, arriving later over the ordinary catch-up (§3.1).
Demanding it refused every real recovery. The check now reads "if the served
chain speaks about our anchor, it must agree" — which still catches a
coordinator re-admitting the seat under a different key, the only thing an
anchor-sized prefix can get wrong about it.

#### Follow-up — BUILT 2026-08-04: the rejoiner is quiet while it waits

The join task surfaces a deaf relay live (`NetJoinNote`, cluster F's widening
ladder). The rejoiner had no equivalent channel, so a deaf group channel was
invisible until the absolute deadline expired. **Built:** `NetRecoverNote`
(INTERNAL, the `NetJoinNote` twin) — "request sent", a once-a-minute widening
ladder across the coordinator's human-approval wait, "welcomed back -
fetching the chain anchor"; notice-borne (`recover-note:`), shown live in the
recover pane, generation-gated so a restarted recovery's stale task cannot
talk over the live one
(`a_recover_note_speaks_only_for_the_live_incarnation`).

### 6f — the PoP end-to-end pin (step 5's owed test) ✅ DONE 2026-08-03
Step 5 could not test the wrap-author gate: a forged request fails the SEAT
PROOF first, so the test would have passed for the wrong reason. Once 6e can
produce a **correctly signed** request with a mismatched wrap author, the
gate is pinnable.
- **Red:** that exact request is refused with the ticket UNSPENT. ✅ —
  verified red with the gate disabled, and the failure is the real one: the
  impostor's request re-admits the seat (a second block appears on the
  coordinator's chain).

Both halves are asserted, because either alone is satisfiable by a bug. The
refusal: the coordinator's chain stays at the genesis for a bounded window.
The ticket: the honest recovery goes on to succeed over the very same link —
"nothing happened" would otherwise also be satisfied by a coordinator that
silently burned the ticket, which strands the seat just as thoroughly as
accepting the impostor would.

`founding::member_identity` is public now so the test derives the seat's
identity the way the product does; forking the salt convention into a test
is exactly what its own doc comment warns against.
