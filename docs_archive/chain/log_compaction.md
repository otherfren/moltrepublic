# Compaction: WP4a ephemeral pruning · WP4b chain checkpoint

> **STATUS: BUILT.** WP4a complete 2026-07-25; WP4b stages landed through
> 2026-08-03 (checkpoint layout now at `molt-chain-checkpoint-v8`,
> conditional, see CLAUDE.md). The remaining items named in §B.8 are declared v1 limits,
> not open work. Original decision record below.
>
> **DECIDED (2026-07-17, discussion with the user).**
> F1–F4 are settled (§A.2); the doc is split into two parts:
> **WP4a** = physical pruning of the ephemeral log (local, policy-enforcing),
> **WP4b** = threshold-signed chain checkpoint (m-of-n confirm the
> correctness of the compaction — result of the checkpoint discussion).
> Routine WP4a runs are deliberately NOT voted: an "I have
> compacted" signature would be unprovable, a block over ephemeral
> device state would violate the ephemeral boundary, and offline devices
> would change nothing about the result. The threshold authority lies in the POLICY
> (`set_chat_retention`, gated vote) and in the checkpoint (WP4b).

---

# Part A — WP4a: Physical pruning of the ephemeral log

## A.1 Mandate, constraints

As in the original draft: after **expiry + grace**, the content and
metadata of expired chats/shares really no longer exist locally.
Constraints C1–C5 unchanged:

- **C1** `seq` is NEVER renumbered (legacy-id keystones; entries drop
  out, gaps remain).
- **C2** Peer cursor < compaction floor ⇒ redirect to the
  WP2 catch-up track (`ChainRequest` → `serve_chain_from` +
  `serve_open_governance`), never log replay on deleted terrain.
- **C3** Chain blocks, genesis, roster: never touched by WP4a
  (WP4b has its OWN, voted mechanism for them).
- **C4** User files are off limits; only `prefs.shared_files` entries drop.
- **C5** Local hygiene, nothing crosses the wire.

Core idea unchanged: **compaction = snapshot pull-forward** (a pruned
`EngineStateDump` as the new base) **+ drop of whole segments** below the
floor. No byte rewrite of individual entries, no rewrite of partially affected
segments. Floor rules:

- **R1 Snapshot coverage:** floor ≤ `at_seq` of a valid snapshot.
- **R2 Outbox coverage:** min over peer delivery cursors; a dead peer
  holds the floor only until the peer grace (F4), then redirect (C2).
- **R3 Retention + grace:** only
  `ts + window + grace < now` is pruned; `ts 0` (unknown age) never.

## A.2 Decisions (2026-07-17, user)

- **F1 — Rotation: size-driven only** (8 MiB as today, no
  forced rotation). Accepted consequence: the open segment is compacted
  only after rotation — residual exposure is capped at ~8 MiB per workspace;
  quiet republics keep this residue longer.
- **F2 — Content grace = 1× retention window** (physically gone after 2×
  the window).
- **F3 — Segment keys IMMEDIATELY** (no unlink-only v1): real
  crypto-erasure from the first build stage. Design §A.3.
- **F4 — Peer grace = 2× retention window** (twice as long as the
  content grace), then cursor redirect to catch-up.
- **F4b — Ack-progress grace (2026-10-11, user; audit #32).** F4 measures
  only CONTACT; a peer that is present but never acks held the floor
  at 0 forever. Now a peer holds only as long as its hold value (acked
  floor, or the send cursor for a never-acker on the legacy path, 0 without a
  cursor) has moved within **2× the retention window**. Per
  peer, `transport.state` persists `ack_progress` (`floor`, `since`,
  `checked`); written only by the compactor (`compact_once`). The clock
  starts at the first run that sees the peer as holding, and restarts when
  the value rises or reaches the snapshot. After that the delivery
  guarantee ends for the part below the floor (C2). Clarifications
  (conservative, not part of the decision): (a) measurement happens per
  compactor run, i.e. daily — the release comes up to a day
  later. Daily means: once chat has been pruned (`chat_pruned`),
  the run happens on EVERY daily beat, even if nothing new expires —
  otherwise the clock would stand still in a quiet republic and the lurker would hold the
  floor forever. Before the first prune no run happens (nothing to delete);
  (b) a gap between two runs > 1 window, but at least
  > 2 days, is OUR absence (the peer could not ack to anyone who
  was not running) and is not counted against it — the daily beat itself
  (1 day + tick lag) must not count as absence at 1-day retention;
  the stall grace is accordingly at least 2 days; (c) a peer that drops off the holding list
  loses its entry and starts fresh on return.

Defaults from the open F6–F9 round (built this way unless there is
an objection):

- **F6 — Migration:** legacy segments (under the workspace key) are rewritten
  to segment keys once, on the FIRST compactor run
  (copy-then-swap) — after that, key erasure everywhere, no two
  deletion classes.
- **F7 — Backups:** no active reach into S3 copies; old copies
  age out via `s3_keep_copies`. The grace until then is named honestly in the doc +
  in the settings help.
- **F8 — Trigger:** 1×/day per open workspace (ticker) + once on
  clean close (snapshot and cursor are fresh there).
- **F9 — Proof:** the byte-fixture proof "pruned snapshot +
  tail replay holds the legacy-id keystones" is the FIRST commit of the
  WP4a implementation (red = design stop), before any compactor code.

## A.3 Segment keys (F3 design)

- One random data key (DEK) per segment; the frames of a segment
  are encrypted under its DEK instead of directly under the workspace key.
- DEKs live, wrapped with the workspace key, in a **key table**
  (`log/keys.state`) that is rewritten in full on EVERY change
  (tempfile + fsync + rename — the same atomic pattern as
  `write_snapshot`).
- **Deleting = unlink the segment file AND remove the DEK from the table.**
  Without the DEK, forensically recovered segment bytes are
  worthless.
- **Honest limits (documented residual risks):** (1) on flash/
  journaling FS, OLD versions of the small key table can survive
  forensically — only TRIM/hardware encryption gives a hard guarantee;
  (2) old S3 backup copies contain deleted content until
  `s3_keep_copies` displaces them (F7).
- Format version in the table header; an older reader without
  table knowledge must not open the workspace for writing
  (additive-only rule applied analogously at the storage level).

## A.4 Crash safety

Order per run, each step idempotently repeatable: (1) write the pruned
snapshot as a tempfile + rename, (2) advance `compaction_floor` in the
manifest, (3) only then unlink segments + rewrite the key table.
Crash between steps: orphaned segments below the floor are
deleted by the next run; the opposite direction (floor above still-needed
terrain) can never arise.

## A.5 Stages WP4a

1. ✅ this doc (decided 2026-07-17).
2. ✅ **Keystone proof (F9) + molt-storage primitives** (2026-07-25,
   `c3fe291` + `141eb6d`).
3. ✅ **Engine compactor** (ticker + clean close, F8) incl. floor/cursor
   redirect (C2/F4) — `a386d87`.
4. ✅ **Share forgetting** (`shared_files` drops on pruning, also in the
   persistent sidecar).
5. ✅ this section.

### What was built (state as of 2026-07-25)

**The F9 proof came first and found two real traps** that would otherwise
have shipped silently:

- **Per-sender ordinals shift.** The id of a legacy message is
  `sha256(TAG ‖ le64(sender_ordinal) ‖ from ‖ ts ‖ body)`; the ordinal is counted
  over the log. Whoever discards old messages counts anew — the next
  legacy message of the same sender would get a DIFFERENT id here than at the
  peers, and an id-addressed protocol diverges. Solved by
  `EngineStateDump::chat_pruned_counts`: the dropped count per sender is
  carried along and added to the ordinal (a sum, not a position — also covers
  out-of-order arrivals).
- **Legacy POSITION references address wrongly.** An id-less
  reaction/delete/file-removed (index-addressed) and the numeric `quote`
  would, after a prune, point at the message that moved up —
  i.e. react to/delete an innocent SURVIVING message. Solved by
  the sticky `chat_pruned` flag (persisted in the dump): afterwards
  `chat_target` refuses the index fallback and the quote stays unresolved.
  Id-addressed ops are unaffected.

**Flow of a round** (`compaction.rs` + `compact_once` in the storage writer):
content cutoff = `now − (1 + F2 grace) × window`; live state and snapshot
are pruned with the SAME call (they cannot drift apart);
then write snapshot + fsync (R1), floor = snapshot position, held back by the
delivery cursor of each peer WITHIN its grace (R2/F4) and its
ack-progress grace (F4b), then F6 migration,
then drop (delete keys first, then unlink). The first real drop raises the
manifest version — older binaries politely refuse the workspace instead of reading it
as damaged.

**Deviations from the default (deliberate, small):**
- The daily trigger rides on the presence tick instead of its own
  internal `Command` — the same solution as the track-C rotation, saves a
  command variant and thereby co-equality churn. The clean-close trigger runs
  SYNCHRONOUSLY, otherwise it loses the race against the writer's `Close`.
- The floor lives in the key table, not in the manifest: it belongs to exactly
  the data the table describes, is written atomically with it, and
  does not leak in the plaintext manifest. The manifest carries only the
  version stop.

**Gap found along the way (fixed):** `log/keys.state` was missing from the
export include table and the import allowlist — a blob of a compacted
workspace would have transported segments without their keys, i.e. an
undecryptable log on exactly the path that exists for recovery.

**Decided cards (2026-11-15):** A rejected, withdrawn or
superseded card drops in the same round and at the same cut as its
discussion (window + grace, default 14 days; configured via
`set_chat_retention`). A card without a stamp (superseded, legacy) starts
its clock at the first run. The ID stays as a tombstone in the snapshot
(`retired_proposals`): a late re-serve, a vote or a withdraw for
it is accepted and discarded, never parked and never revived as an open card.
Accepted cards stay (chain blocks).

**Open / deliberately not built:** F7 stays as decided (old S3 copies
age out via `s3_keep_copies`, no active reach) — the grace is
named in §A.3 as a residual risk and still belongs in the settings help.

---

# Part B — WP4b: Chain checkpoint (m-of-n-signed compaction)

## B.1 Idea and contract

The state projected from the chain is byte-identical on all nodes
(hard sequencing: `height` counter + `prev` hash link +
position-bound signatures). So the republic can
**compact it the way it governs**: a checkpoint freezes the
projected state at height `upto`; m-of-n sign the
**checksum of this state** — m participants confirm the *correctness
of the compaction*, not its execution. After that, every node may drop the
blocks ≤ `upto` locally; newcomers/rejoiners bootstrap from
**checkpoint + suffix** instead of from block 0.

What this achieves: bounded chain growth, cheaper recovery (today the
whole chain travels in the Welcome), and the one place where compaction becomes
"official" — chain-anchored, auditable, position-bound.

## B.2 New chain vocabulary (additive)

```rust
// molt-core ChainChange, additive variant (unknown ⇒ reader stops, as before)
Checkpoint {
    /// The last folded-in block: the checkpoint attests the
    /// state AFTER block `upto` is applied.
    upto: u64,
    /// sha256 (lowercase hex) over checkpoint_canonical_bytes(state@upto).
    state_hash: String,
}
```

The checkpoint block sits at height H with **`upto` = H − 1, strictly enforced**
(review finding 2026-07-18: a smaller `upto` would let blocks arise in
(`upto`, H) that are carried by neither blob nor suffix — their applied
ids would escape the double-apply guard, their membership changes would escape the
roster; a re-based checkpoint proposal must therefore cut ANEW:
recompute state + hash at the new head height, not merely change the
signature height). Signing uses
normal position-bound signatures over
`republic_id ‖ H ‖ change` (`approval_bytes` gets only the new
variant serialization — **no second signing path**, genesis rule
applied analogously).

## B.3 Canonical state serialization (`molt-chain-checkpoint-v6`)

Deterministic byte sequence, length-prefixed fields (the same framing
as `roster_canonical_bytes` — sibling layout, its own
version tag). Content in fixed order:

1. Tag `molt-chain-checkpoint-v2\0` (v2 since N1: both identity tables
   additionally carry each member's `nostr_pk` transport anchor — under
   v1 the roster anchor of a served blob could have been swapped
   without a hash change).
2. `republic_id`.
3. **Founding table**: name, `rule_m`, `rule_n`, GENESIS identities in
   founding order, agenda — so that every verifier can RECOMPUTE `republic_id`
   from the content (forgery protection like the
   genesis check, §B.5).
4. **Current roster** after all membership blocks ≤ `upto`:
   `(member, identity_pk, nostr_pk)` in chain order
   (deterministic, because the block sequence is totally ordered).
5. **Applied projection** per surface, surfaces in
   `Surface::ALL` order; per surface the list
   `(proposal_id, payload_canonical_json)` in block order.
   `payload_canonical_json` = serde_json with sorted map keys
   (default BTreeMap; a test PINS that `preserve_order` is active
   nowhere in the workspace). **Since v4 SUMMARIZED, not archived**
   (§B.6a): a last-write-wins slot keeps only its last entry,
   accumulating entries all survive — the declaration is
   `molt_core::applied_lww_slot`, applied in `fold_one`.
6. **Consumed proposal ids**: sorted list of all ids applied in blocks
   ≤ `upto` — seed for the double-apply guard of a
   suffix verifier (§B.5). **EVERY** id, including that of a
   summarized (dropped) payload.
7. **Working transport anchors** (v5): `(member, nostr_pk)` per seat that a
   `Restored` block re-anchored, sorted by member. Carried for the same
   reason as the relay pool — the blocks that set the anchor
   drop out at the cut, and the roster by construction keeps the
   FOUNDING anchor (a `Restored` must not move a seat's identity key,
   so the anchor travels BESIDE the roster). See §B.6b.
8. **Relay ledger** (v6, R3b): `(member, relays)` per seat whose
   `Membership` block carried a relay declaration, sorted by member —
   the split-detection input (R4) survives the cut for the same reason
   as the anchors in 7.
9. `upto`.

Same chain ⇒ same bytes ⇒ same hash, on every node. The block contains
ONLY the hash; the **state blob travels outside the chain**
(persisted locally next to `chain.state`, delivered with catch-up/recovery
and verified against `state_hash`) — blocks stay small.

## B.4 Flow (sign-what-you-see, the membership pattern)

1. **Propose:** human verb `propose_checkpoint` (parameterless —
   `upto` is ALWAYS the current head height, B-F1). Stage 3 delivers the
   MCP tool; the GUI affordance + visibility (pending display,
   seal/stale event) is stage 5. The proposer
   computes `state_hash` from its own chain and announces
   `WorkspaceEvent::CheckpointProposed { id, upto, state_hash }`
   (additive; the receive arm is INTERNAL, like `MembershipProposed`).
2. **Verify before sign:** every receiver recomputes the canonical
   bytes from its OWN chain@`upto` and compares. Unequal ⇒ do not
   sign (+ WARN) — nobody signs a foreign blob. Equal ⇒
   `chain_sign_and_gossip_approval`, position-bound to target height H.
3. **Seal:** m distinct signatures ⇒ deterministic block
   (`try_commit` path; `proposal_changes` carries the checkpoint change as
   for membership).
4. **After the commit:** every node persists the (verified) blob
   and drops blocks ≤ `upto` locally (default B-F2: automatic — the
   vote confirmed correctness). `chain.state` becomes
   `[blob] + [checkpoint block @H] + suffix`.
5. **Open proposals across the cut:** unchanged — a checkpoint
   is a normal block; pending signatures re-base via the
   existing mechanism onto the new head height.

## B.5 Verification (`verify_chain` extension)

- **Full-history holder** (chain begins with genesis): as today,
  PLUS: a checkpoint block is checked for content (own
  projection@`upto` ⇒ hash equality), otherwise hard-reject —
  all-or-nothing like any other violation.
- **Suffix holder** (chain begins with a checkpoint block): new
  trust anchor, mandatory checks all-or-nothing:
  1. Blob hash == `state_hash` in the block.
  2. `republic_id` recomputation from the founding table in the blob ==
     expected id (from the invite/recovery link) — forging the founding changes
     the id, exactly the genesis protection. Plus the structure checks of the
     genesis rule: `rule_m ∈ [1, rule_n]` and
     `founding_identities.len() == rule_n` (since N1 — a larger
     table would smuggle attacker keys into the signer set).
  3. **No circular trust** (review finding 2026-07-18): the
     blob roster is bound only by the hash that the anchor signatures
     themselves attest — therefore the anchor signatures verify against
     the **founding identities** (rid-bound), and every roster entry
     must appear verbatim in the founding table — all THREE anchors,
     `nostr_pk` included (seats are fixed from founding on; Restored
     keeps the anchored key). Forgery therefore requires
     m REAL founding keys — the honest-majority assumption, nothing less.
     ≥ m distinct, m from the founding table.
  4. Suffix from H normal (prev links, heights, signatures); double-apply
     guard seeded with the id list from the blob.
- **Explicit trust model (documented change):** a
  suffix bootstrapper no longer checks the roster evolution below `upto`
  block by block — it trusts m-of-n of the current roster that
  the blob summarizes this evolution correctly. That is the same
  honest-majority assumption on which threshold governance stands anyway.
  Whoever does not want that keeps the full history — dropping is
  permission, never obligation (C5 spirit).

## B.6 Catch-up, recovery, persistence

- `serve_chain_from(from)`: **decided + built (4b):** its own
  additive event `WorkspaceEvent::CheckpointServed { blob }`, served
  BEFORE the Committed re-serves, only if the requester is truly below the
  anchor (`from < anchor`). Receiver: first-stash-wins +
  invalidation; adopt only after full suffix verification. Known
  v1 limit: ONE malicious seat can delay the re-anchoring of a
  latecomer via a stash race (liveness, never safety) — per-peer stashes
  would be the fuller fix. A full holder behind the cut is pruned on
  re-anchor (the mesh no longer holds the gap blocks) —
  under B-F2 (auto-drop everywhere) this is consistent: archive holders
  deliberately do not exist in this design, recovery no longer depends on them
  since 4c.
- Recovery: the Welcome carries checkpoint blob + suffix instead of the full
  chain; the rejoiner verifies per the suffix rules (§B.5).
  **Stage 4c BUILT:** the Welcome carries the pruned wire form
  (`ServedChainWire`, untagged — full coordinators keep the bare
  array); the rejoiner verifies per the suffix rules and materializes
  from the blob's rid-bound founding table with EMPTY
  attestations (the genesis signatures are gone with block 0; authority
  is the verified blob+suffix, the local Founded record is
  bootstrap metadata).
- Persistence: `chain.state` additively extended by the blob (untagged:
  array = full, object = pruned). **Honest old-reader reality
  (review 2026-07-18):** an OLD binary reads a pruned
  `chain.state` as "decode failed" and keeps running CHAINLESS — the
  log-side `unknown_events` stop applies only as long as checkpoint
  events sit in the tail (before the next snapshot floor). An old
  binary on a pruned workspace can therefore walk into the legacy
  counting-path trap; the clean stop (manifest format bump on the first
  prune) is pinned as a stage 5 item.

## B.6a A checkpoint SUMMARIZES — it does not archive

**Product decision, 2026-08-03 (user). BUILT 2026-08-03 (v4).** A
checkpoint's state carries what the republic **is**, never the path that
produced it. Superseded values and dead intermediates are dropped at the cut.
That is what checkpoints are for.

**It used to be the opposite**, and that was a defect rather than a design:
§B.3 item 5 serialized the applied projection as `(proposal_id, payload)`
**in block order, complete**. So every logo the republic ever had, every
name it ever carried, stayed in the blob forever.

That is not a size nicety. The blob is the **trust root a rejoiner must be
handed** once the genesis is gone (`nostr_n4b_step6_design.md`), and it is
already over budget: measured, a blob holding ONE 25 KiB logo costs 69628 B
against a 65408 B gift-wrap cap and a 128 KiB relay message budget
(`welcome_chain_budget.rs`). Keeping the history means the blob grows for
the life of the republic until recovery into an old republic simply stops
working — and it stops working silently.

### What "summarize" means precisely

The two kinds of applied entry are not the same and must not be treated the
same:

- **Last-write-wins slots** — Organization's `set_name`, `set_charter`,
  `set_chat_retention`, `set_image` / `remove_image`. Only the LAST applied
  entry per slot survives the cut. This is not a new judgement about what
  matters: `org_effective` (`proposals.rs`) already folds exactly this way,
  so the summary is nothing but that fold's own answer, kept instead of
  recomputed from a history nobody reads.
- **Accumulating items** — an entry that is a distinct object rather than a
  superseded state (Memory's notes). These are KEPT. A checkpoint is a
  summary, not a delete.

A surface must therefore declare which of the two each of its ops is. An op
whose kind is undeclared is treated as accumulating — the conservative
direction, since dropping something that was not superseded loses data.

### Invariants this must not weaken

- **`consumed_ids` keeps EVERY consumed id**, including those whose payload
  was dropped. It is the double-apply guard, and a summarized-away payload
  must never become a re-appliable proposal. It is already a field of its
  own (§B.3 item 6), so this is a constraint to respect, not a change.
- **Deterministic and versioned.** Every node folds independently and must
  reach byte-identical `checkpoint_canonical_bytes`, or a cut can never
  gather its m signatures — the failure would be a republic that can no
  longer compact, with no error pointing at why. Changing what goes into
  item 5 is a **`molt-chain-checkpoint-v3` → `-v4` bump** with every
  byte-pin test moved together (the CLAUDE.md versioned-layout rule).
- **Verify-before-sign is unchanged in shape**: a signer recomputes the same
  summary from its own chain and compares `state_hash`. The rule only moves
  what "the same summary" means, and it moves it identically on every node.

### What a pruned holder loses, stated rather than hidden

The history view below the cut stops showing superseded values — an old
logo, a former republic name. That is intended and is the point of the
decision. The chain keeps the complete record until a cut; after it, the
republic remembers what it is, not every step of how it got there.

### The other half — DONE 2026-08-03

Even a perfect summary does not bound the CURRENT image, and that was the
other half of the same problem. **Measured 2026-08-03**
(`molt-net/tests/group_frame_budget.rs`, through the real pipeline —
payload base64, block JSON, MLS ciphertext, `seal_outer` base64, event
framing, counted exactly where `RelayRuntime::publish` counts it):

| | |
|---|---|
| wire expansion of an image | **x1.80** (base64 twice) |
| largest publishable image | **68 KiB** (125266 B event) |
| first size over budget | 72 KiB (132546 B) |
| `DEFAULT_SIZE_BUDGET` | 131072 B |
| `ORG_IMAGE_MAX_BYTES` (was) | **262144 B** |

The propose-time cap sat at nearly **4x** what the transport can carry.

**Built.** `ORG_IMAGE_MAX_BYTES` is gone; there is no second, independently
chosen number. `proposals.rs` now derives the ceiling from the budget:

- `molt_net::envelope::frame_cost` / `max_plaintext_for` — the kind-445 cost
  model, an upper bound proven against the real pipeline (MLS ciphertext,
  `seal_outer` base64, event framing) in
  `molt-net/tests/group_frame_budget.rs`.
- `proposals.rs::payload_fits` judges the **serialized payload**, not
  images: a long charter blows the same budget by the same mechanism, and
  images are merely where it bites first. It is roster-aware — every seat's
  attestation rides the block — and it is the SAME verdict on the wire
  ingest path, so peers agree on what to drop.
- The refusal names the figure that would fit, so "too large" comes with the
  number to aim at.
- Measured headroom for a 3-seat roster: **70 KiB** of image.

`DEFAULT_SIZE_BUDGET` and not a probed pool value on purpose: every member
must reach the same verdict on the same proposal. When pool probing lands
(`with_size_budget` still has no production caller), a SMALLER probed budget
belongs there as a second, node-local refusal — never as a replacement for
the shared one.

The wedge itself is also closed at the other end: a locally refused publish
is no longer retried on a backoff that cannot change a deterministic verdict
(`group_runtime.rs::PublishStall::Permanent`). The cursor still HOLDS —
nothing recovers a skipped envelope — but the operator is told the real
reason instead of "no relay accepted the frame".

The two together are what make a pruned republic recoverable, and neither
suffices alone: without the summary the blob grows without bound, and
without the cap a single current logo can already be unpublishable.

## B.6b A cut must not strand a restored seat

**Verified defect, found 2026-08-03 while building N4b 6d, fixed in
v5.** `CheckpointState.roster` arises from `apply_membership`, and that
deliberately leaves a seat's `nostr_pk` untouched: a `Restored` block
must not move the anchored identity key (otherwise m-of-n
survivors could hijack a seat). The new transport anchor is therefore a
projection BESIDE the roster (`State::chain_anchors`).

The block that sets it, however, dropped out at the cut, and `chain_anchors` was
folded only from the surviving suffix. Consequence:

> After a compaction, every member that was ever restored
> was addressable only under the key it no longer
> owns — silently.

Reachable in normal operation: `AUTO_CHECKPOINT_MIN_LEN` is 32. The
summary now carries the anchors itself (item 7), and a pruned
holder SEEDS `chain_anchors` from it before folding the suffix on top.
Keystone: `a_compaction_keeps_the_working_anchor_of_a_recovered_seat`.

## B.7 Limits (deliberate)

- The ephemeral log (chat/shares) NEVER gets a shared checksum:
  ephemeral logs are not identical between nodes (Q4 rejoiner without
  backfill, arrival orders, local clocks). WP4a remains
  responsible for that — local, policy-enforcing.
- ~~Checkpoint triggering is a governance matter (human verb); no
  automation in v1.~~ **Revised (product decision 2026-07-18): triggering
  is AUTOMATIC** (`maybe_auto_checkpoint`, molt-engine
  chain.rs) — the GUI button is removed, `propose_checkpoint` remains
  as a co-equal MCP verb (manual override). The signing side was
  always mechanical (verify-before-sign); only the trigger was
  automated, collision-free and deterministic:
  - Only the **alphabetically smallest-named
    roster member** triggers (one proposer — proposal ids are node-local;
    two simultaneous auto-proposers would collide). If it is
    offline, nothing happens — as with an absent manual
    proposer; the MCP verb remains the way out.
  - Only when this node **itself sealed at the live head**
    (`adopt_committed_block`, after the re-base): all co-signers are then
    at the same head — exactly what `receive_checkpoint_proposal` needs
    to recompute. Passively applied blocks (`apply_next_block`:
    catch-up serve, broadcast of another sealer) NEVER trigger — a
    catching-up node would cut at the stale intermediate head, and
    a quorum catching up in lockstep could even
    co-sign this cut and fork a holder AFTER the history drop
    (review finding 2026-07-18; pinned by test). No trigger on open
    into a cold mesh (without a re-serve the cut would only be lost).
  - Never **during open votes** (open surface proposals,
    ongoing signature collections, or an already pending cut) —
    a block sealing in between would only make the cut stale.
    The commit that resolves the last open vote fires the
    check again.
  - **Stale needs no timer/backoff**: the block that makes the cut
    stale runs through the same hook and re-proposes at the new
    head — at most one auto-propose per committed block. The
    stale toast in the GUI is removed (the event remains for MCP);
    only "Checkpoint besiegelt #X" remains visible.
  - Threshold: `AUTO_CHECKPOINT_MIN_LEN = 32` locally held blocks —
    a constant, not a setting (compaction is hygiene, not policy).

## B.8 Stages WP4b (each individually mergeable, TDD)

1. `checkpoint_canonical_bytes` + hash in molt-core (red tests:
   determinism across two independently built chains, version tag,
   republic_id recomputation, sorted-JSON pin).
2. `verify_chain` extension: checkpoint check (full holder) +
   suffix anchor (red tests: forged roster, forged founding table,
   hash mismatch, double-apply across the cut, sub-threshold m).
3. Engine flow: `propose_checkpoint` (command + MCP tool, co-equal),
   `CheckpointProposed` wire arm (INTERNAL list), verify-before-sign, seal.
4. Drop + serve: local drop ≤ `upto`, catch-up with blob, recovery
   via checkpoint (loopback E2E).
5. ✅ Docs + hardening (2026-07-18): persistent_chain.md section,
   manifest version bump on the first prune (old binaries refuse the
   workspace instead of running chainless), checkpoint lifecycle visible
   (Event::CheckpointSealed/-Stale + toasts), GUI verb in the
   Organization status (co-equal to the MCP tool). Deliberately left open:
   no ListProposals entry for pending cuts (collides with the
   id-collision guard from stage 3; the events give the proposer
   closure), latecomer buffer, per-peer blob stashes.

6. ✅ **DONE 2026-08-03 — the summary rule (§B.6a).** Item 5 carried the
   complete applied history; it now carries the CURRENT state.
   `molt_core::applied_lww_slot` is the declaration (Organization's four
   slots, `set_image`/`remove_image` sharing one; everything else and every
   undeclared op accumulates), applied inside `fold_one` so a suffix holder
   folding onto a summarized blob lands exactly where a full holder folding
   from the genesis does. Bumped to `molt-chain-checkpoint-v4`.

   **`fold_state` now delegates to `fold_one`.** The two folds had the same
   match written twice — the batch fold a proposer computes a cut with, and
   the incremental walk every verifier re-checks it with. A rule reaching
   one and not the other would have left the republic unable to sign ANY
   cut, with nothing pointing at why; the duplication was the trap, so it is
   gone rather than kept in sync by hand.

   Red tests, in this order (all landed):
   - a chain with three `set_image` blocks yields a blob carrying ONE image,
     and it is the last one (asserted by content, not by count — a summary
     that kept the FIRST would also pass a count check);
   - distinct slots stay independent, and a `remove_image` supersedes the
     `set_image` it removes;
   - a note surface's entries all survive, so the rule cannot be read as
     "keep only the last entry" globally; an undeclared op accumulates;
   - `consumed_ids` still lists every dropped payload's proposal id, and a
     block re-applying one of them is refused (the guard is the thing most
     likely to be lost by accident here);
   - the walk and the fold summarize identically, and a suffix holder
     summarizes onto the blob the same way;
   - two independently built chains fold to byte-identical
     `checkpoint_canonical_bytes` — without this a republic silently loses
     the ability to compact at all;
   - the `-v4` byte-pin fixture, recomputed independently.

Open from the stage 2 review for stage 3/4 (pinned, not forgotten):
- `after_block_applied` needs a checkpoint arm (emit event,
  clear `proposal_changes`/`pending_sigs` of the checkpoint proposal).
- Stage 4 must route ALL `verify_chain` callers (adopt_chain,
  append_committed_block, apply_next_block, tie_break, open/recovery) to
  suffix chains — today `adopt_chain` would delete a dropped chain
  on reopen.
-   **4d BUILT:** both walkers fold incrementally (`fold_one` +
  `hash_walk_state`) — O(n) instead of O(n·checkpoints).
- The proposal-id namespace is minted node-locally; stage 3 defuses
  the checkpoint side (occupied ids are never auto-signed), the
  general collision of two simultaneous proposals remains open.
- Stage 3 review, open for stage 5: (a) the checkpoint lifecycle is
  unobservable (no ListProposals entry, no event on
  seal/stale drop — the operator must read status/chain); (b) liveness:
  a latecomer discards the cut and never gets it re-served (no
  buffer, no re-gossip) — with < m current nodes nothing seals, the
  proposer must re-propose. Both deliberately v1, honestly documented.

## B.9 Defaults (open to objection)

- **B-F1:** `upto` at propose time = current head height (no freely
  selectable cut in v1).
- **B-F2:** local drop ≤ `upto` automatically after the commit (the
  vote confirmed correctness); no separate verb.

---

## Security audit WP4a + mesh follow-ups (2026-07-25)

Adversarial audit over the change set `7b7542a..HEAD`. Attacker model: a
**malicious member** of the republic (can send arbitrary announces MLS-authentically),
a **malicious server**, and an attacker with **access to the
medium** (for the erasure claim). No critical gap; five findings,
all fixed (commit `15f0f2f` — contrary to its commit message, it also contains
finding #5).

**#1 — Silent data loss: peer without a cursor (WP4a).** The compactor was given the
list of peers to IGNORE; a peer with no cursor entry at all (never
delivered to, or `transport.state` lost — elsewhere in the design explicitly only
a reason for resends) thus held nothing back. The log could be pulled out from under
exactly those who still needed it. Inverted: the
engine now supplies who HOLDS the log (`peers_holding_the_log`), and a
holding peer without a cursor counts as "has received nothing" and stops the round.

**#2 — DoS by a member: dynamic server table (mesh).** The table
of pinned foreign servers never evicted. A member that announces
new hosts on every rotation fills all 64 slots permanently; after that every
HONEST peer that later changes server lands on our primary — where its
queue does not exist. Delivery thus breaks for the honest, not for the
attacker. Now the least recently used entry is evicted.

**#3 — Erasure only on disk, not in memory (WP4a).** The compaction
promises that the bytes of a dropped segment are worthless because the
key is gone — but the DEK stayed in freed heap (entry
dropped without zeroize; every plaintext copy of the table during writing and reading
likewise). `SegmentKey` now zeroizes on drop, both plaintext buffers are
`Zeroizing`. The filesystem residual risk (§A.3) remains as documented.

**#4 — Availability: fsync on the actor (self-inflicted).** The new
pre-backup flush waited on the actor for an fsync — against our own rule
that handlers never await I/O; a slow disk would have stalled
command processing. Both copying paths now flush in their blocking task, the
ordering guarantee remains.

**#5 — Functional regression: `peek_genesis` (WP4a).** The open screen reads the
genesis as the FIRST log frame and decrypted it with the workspace key. After
the F6 migration, segment 1 lies under its DEK, after the first drop it is
gone entirely — the workspace list would have lost roster and charter for exactly the long-lived
republics. `peek_genesis` now reads segment-keyed
and then falls back to the snapshot, which carries all genesis facts anyway
(attestations stay empty — display metadata, never consensus input, as with
checkpoint recovery).

**Checked and judged acceptable (not changed):**
- A member can announce foreign or even OUR OWN queue in `queues_extra`.
  The fan-out is capped at `MESH_REDUNDANCY_CAP`, the content
  stays MLS-encrypted (the announcer is a member anyway), and a
  self-announce fails on the wrap-key mismatch. Cost: some bandwidth.
- The bounded SSRF-lite of the pinned dial (see `mesh_redundancy_stage2.md`).
- A manipulated `EngineStateDump` (via restore blob) could set `chat_pruned`
  or the ordinal counters — but that requires the recovery phrase,
  i.e. the same trust level as the whole workspace.
- Peers beyond the grace lose ephemeral gossip events below the floor.
  That is exactly C2 (catch-up via the chain); the durable truth lies in
  `chain.state`, which WP4a never touches (C3).

**Open (test finding, not a security gap) — PRE-EXISTING, measured:**
`recovery_distributes_the_rekey_commit_to_a_live_survivor` (two_instances)
waits for the chat note of the new MLS epoch at the survivor and is flaky.
Controlled A/B test with both test binaries, alternating on the same
machine, 10 runs each: **pre-session (7b7542a) 2/10 failures, HEAD 3/10** —
so pre-existing, not introduced by this change set. (Individual
6/6-green series on the old state had initially made it look
different; only the interleaved comparison is meaningful.) Additionally
ruled out: the compaction never fires in this test (verified via log),
and `ae92afd` (mesh follow-ups) ran 6/6 green. The likely
candidate remains a race in the supervisor rebuild of the mesh extension against the
in-flight chat note — a separate investigation item, not an audit finding.
