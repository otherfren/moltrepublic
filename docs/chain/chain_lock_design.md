# The chain lock: fork-free sealing (B7)

**Status: BUILT, dark: not available in release builds, under review
(B7a pure state machine, B7b wiring, 2026-11-22). Review rounds three
and four found a halt and a fork in 2-of-3 that are not fixed in the
release tree. With the switch on the lock runs where the chain enabled
it, always by a `set_features` vote: a founding starts without it
(wiring plan §11; tests found locked through a seam) (§8); deviations in §14; plan `docs/chain/chain_lock_wiring_plan.md`.**

**The switch.** `CHAIN_LOCK_AVAILABLE = false` in `chain/lock_wire.rs`; a
hidden engine seam (`__lock_available`) turns it on per engine, so the
lock suites run against the real code. Off: a founding ratifies no
`chain-lock` (the v0.1.0 charter, roster and genesis bytes), the
founding-join refusal does not fire, the capability is not announced
(mirror declaration, JoinRequest), a `set_features` naming the key is
refused at propose and approve (`Strict Order Mode is not available in
this build`), no lock frame is sent or answered, and nothing lock-related is
written - no file, no version 4, no marker
(`the_lock_is_dark_in_this_build`). A chain that nevertheless carries the
key (founded or switched with a dev build): this build signs nothing
there - neither the lock nor a free signature beside it - and says
`this republic uses Strict Order Mode - needs a newer build`; reading and
catching up stay possible (`sign_blocker`).

Modules:
`crates/molt-engine/src/chain/lock.rs` (simulation in `lock_tests.rs`),
the wiring `chain/lock_wire.rs` (`lock_wire_tests.rs`), over a relay
`crates/molt-engine/tests/chain_lock.rs`. Plan: `docs_archive/chain/rejoin_divergence_build_plan.md`
B7; research: `docs/chain/chain_divergence_plan.md` S6. Threat model:
honest seats, faulty network (loss, delay, duplication, reordering,
restarts). No signed layout changes.

## 1. What exists (take / leave)

Re-checked 2026-11-21 (`cargo search`, crate metadata, protocol knowledge).

| candidate | verdict |
|---|---|
| openraft 0.10, raft-rs 0.7 | leave: each owns the log, membership, storage and RPC (a second chain, roster and sync beside ours), elects by majority, and an entry carries no member signatures. Take the ideas: persisted vote, sticky leader, term = ballot. |
| omnipaxos 0.2 | leave: a Multi-Paxos replicated-log library with its own leader election (heartbeats) and storage; mapping the chain onto its log duplicates it. |
| paxakos 0.13 (GPL-3.0) | leave: Multi-Paxos with its own log/state-machine/communicator traits, same mismatch; licence would fit. |
| quepaxa 0.7 | leave: sans-IO and attractive, but a randomized majority-quorum log; no m-of-n quorums, no refusing acceptors. |
| paxos 0.0.5 (CASPaxos), paxos-rust, epaxos | leave: single register, toy, or unmaintained. |
| BFT (malachite, HotStuff) | leave (S6): > 2/3 quorums halt a 2-of-3 republic on one offline seat. |
| stateright 0.31 (model checker) | not now: a new dev-dependency; the seeded simulation covers B7a. Worth an exhaustive check later. |

Why no library fits, in one line each:

- **An acceptance here is a vote.** A seat signs only a change it
  approved; every Paxos/Raft library assumes an acceptor accepts any
  value at a fresh ballot.
- **The acceptance must be today's position signature** (`approval_bytes`,
  unchanged), because the block is what old builds verify.
- **Quorums are m-of-n per value** (m; n for a cut; for a consented
  restore m where m < n, n - 1 where m = n), not majorities.
- **The log already exists** (the chain, its sync, its verifier).

So we build only the per-slot Flexible Paxos core (about 250 lines,
pure), and the textbook algorithm is the reference: Lamport's Paxos,
Howard et al. Flexible Paxos (Q1 + Q2 > n).

## 2. The guarantee

Never two different changes sealed at one **slot** while every seat that
signs there runs the lock. A slot is `(height, base)`, `base` = the
lineage of changes below (§3.1); a fork can therefore only start at a
height where some seat signed freely (B6), and R1 still heals those.
Attestation variants (one change, two signer sets) remain possible and
are re-linked by R1 as today; nothing is re-voted.

## 3. Slots, values, quorums

### 3.1 Slot

`base(h)` = hex `L(h-1)`:

```
d(k)  = sha256(approval_bytes(rid, k, change_k))
L(a)  = sha256("molt-lock-lineage-v1\0" ‖ le64(a) ‖ d(a))   a = last cut block <= h-1, or 0
L(k)  = sha256(L(k-1) ‖ d(k))                              a < k <= h-1
```

Every holder holds the last cut block (a pruned holder's anchor is it),
so all compute the same base. Attestation variants share a base. Not a
signed layout: a mismatch only means two different slots. Kept
incrementally beside the head; recomputed on `adopt_chain`.

Why not the height alone: a free fork below the first locked height
would otherwise share the slot, and a value accepted on one branch can
be a double-apply on the other - a slot that can never seal.

### 3.2 Value

`Value { digest, need }`: `digest = d(height)` of the change - exactly the
bytes every signature covers, so two attestation variants are ONE value
and a signature is evidence of exactly that value. `need` = live
acceptances that seal it, a function of the change:

| change | need |
|---|---|
| `Applied`, `Membership{Joined}` | m |
| cut (`Checkpoint*`) | n (A2: a cut seals n-of-n) |
| `Membership{Restored}` with a consent valid at the height | m where m < n, n - 1 where m = n (`Quorum::restore_need`) |

The wire carries the proposal id beside the digest so a seat finds the
change; for a membership or cut change the full `ChainChange` rides too
(small, and membership is never re-served by `serve_open_governance`).

### 3.3 Quorums and safety

Q2(v) = `need`; Q1 = n - m + 1 promises, for every slot (the value is
unknown before phase 1, so Q1 must meet the SMALLEST phase-2 quorum).
For a cut this is NOT 1: the slot could still decide an m-value.

Safety (standard Flexible Paxos, per slot): a value is chosen when
`need` acceptors accepted it at the SAME ballot. Invariant I(c): if v is
chosen at b < c, ballot c proposes v. A leader at c holds a promise set
P, |P| >= n - m + 1; the chosen set A has |A| >= m, so |P| + |A| > n and
some a in P accepted (b, v) before promising c, reporting a ballot >= b.
The highest reported pair (b_max, v_max) has v_max = v (b_max = b: one
value per ballot; b_max > b: by I(b_max)).

**Forced-value rule with an escape** (`Leader::forced`): v_max is forced
unless `holding + unheard < v_max.need`, where `holding` = promisers
whose latest accepted value is v_max and `unheard` = n - |P|. Sound: if
v_max were chosen, every member of its quorum holds it as latest (later
ballots propose only v_max), so at least `need - unheard` promisers do.
If v_max is not chosen, nothing is (any chosen w would equal v_max), so
the leader is free. For m-values with |P| = Q1 the escape never fires;
it is what frees a cut that one seat refused.

**Sealing is per ballot.** Ed25519 is deterministic, so a seat's
signature for v at height h is the same bytes in every ballot; a seat
may accept v at b1 and w at b4. Pooling every signature ever given forks
(2-of-3: A:v@1, C:w@2, B:v@3 => v holds A+B; leader 4 hears A and C,
forces w@2, A+C accept => w too). So a block is sealed only from one
ballot's `LockAccepted` frames (`Tally`). The simulation pins this
(`signatures_of_different_ballots_must_not_be_pooled`).

Liveness: a slot decides once one leader holds Q1 promises and `need`
approving acceptors answer in the same ballot without a higher ballot
intervening; i.e. while max(Q1, need) approving seats are reachable and
takeovers back off (§5).

## 4. Special blocks

### 4.1 Cut (n-of-n)

Q2 = n, Q1 = n - m + 1. A cut can only seal when every seat accepts in
one ballot. Rules:

- The leader proposes a cut only when it holds promises from ALL n seats
  at its ballot (everyone reachable this round), and only under the
  existing gates (no diverged peer, bases loaded).
- A seat that does not reproduce the hash refuses (no `accept` call):
  the escape frees the slot once a later leader hears it.
- Residual: a seat that accepted the cut and then stays away keeps the
  slot forced to the cut (it may have been chosen) until it returns. A
  seat lost for good during that window needs the later rescue (F-X1).

### 4.2 Consented restore

The consent is a pre-signed statement of the returning seat Z (bound to
a height window and Z's previous anchor, v2), not a live acceptor. Z's
old device may come back at any time (a found laptop) and promise,
accept, even lead without knowing it is being restored - the design does
not rely on it being gone (fourth review, H2/A-F1).

- **need(R) = m where m < n** (`Quorum::restore_need`): R's acceptors
  come from N \ {Z}, and Q1 = n - m + 1 meets any m of them even when Z
  promises. Cost: a restore needs max(n - m + 1, m) live survivors (2-of-3
  and 3-of-5: unchanged in phase 1; 3-of-4: 2 -> 3; 4-of-5: 3 -> 4).
  Pinned: `a_restore_needs_m_where_m_is_below_n`.
- **m = n: need(R) = n - 1**, and the consent stands in for Z. Q1 = 1, so
  a promise from Z alone would be a phase 1: every acceptor refuses
  `LockPrepare`/`LockAccept` from a seat that is a restore's subject in
  its view (`an_acceptor_refuses_a_restore_subjects_prepare`); R's n - 1
  acceptors all know R, so a Z ballot reaches none of them.
- **Who is a subject, for which value** (`lock_subjects`, a pure
  `Subjects` in `lock.rs`): an OPEN consented restore card (a declined or
  withdrawn one takes no live seat out, C-HIGH-1:
  `a_declined_restore_card_makes_no_subject`; where m < n not one whose
  seat is heard - it is alive), and every restore value the lock holds at
  head + 1. A subject is never a promiser and never a leader (also not
  in its own view: `a_restore_subject_never_leads`); its acceptance of
  its own restore never counts; its intents and acceptances of every
  other value count as any seat's.
- Per value (T1): the subject is no unheard voice for its OWN restore
  (it never accepts it) and an ordinary unheard voice for every other
  value (`a_lost_seat_heard_for_every_value_forks_when_it_comes_back`).
  With need(R) = m the excusal is no longer needed for safety; it stays
  for liveness: in 2-of-3 a restore one survivor holds is otherwise
  forced while Z is silent.
- The consent rules stay as they are (`restore_consent_at` at the slot's
  height, single use, window). A consent invalid at the height means the
  change is not a value there; acceptors never accept it.
- The block carries the survivor signatures (m where m < n) plus the
  consent; `verify_chain` accepts m - 1 or more beside it.
- Simulation: `lock_sim.rs` (wire-faithful) found the 2-of-3 fork on the
  old rule (seed 42) and decides every slot without a fork with these
  rules.

## 5. Ballots and leaders

- `Ballot { round, leader }`, ordered by round then name; a leader uses
  only its own name, so ballots are unique.
- Round choice: `max(acceptor.next_round(slot), highest round seen for
  the slot + 1)`. The leader handles its own `Prepare` (persisted) before
  sending it, so a restarted leader never reuses a ballot.
- Default leader of a slot: the first seat of B6's per-height line
  (lowest-named seat seen, the restored seat of a `Restored` change left
  out). It starts phase 1 for head + 1 the moment its head moves, with
  no proposal pending, so the next value costs one round trip.
- Takeover: the k-th seat of the line starts its own ballot after k x
  `SEALER_GRACE_SECS` without a block at the slot, asking for a missed
  block first (`ask_for_height`), as B6. Any `Accepted` at a higher
  ballot seen meanwhile restarts the wait.
- Dueling: a leader that receives a `Nack` with a higher promise, or
  sees a higher ballot's frame, stops; its next attempt waits
  `grace x (attempt + 1)`, capped at 600 s, plus a jitter in
  `[0, grace/2)` from `sha256(name ‖ height ‖ attempt)`.
- Learning the ballot: every lock frame is group gossip; a seat keeps the
  highest round seen per open slot (shown in `chain_sync`).
- The value a free leader proposes: the B6 choice (smallest proposal id
  with `need` approving intents, D2's decliner rule applied).
- Who seals: the ballot's leader as soon as its `Tally` is sealable; any
  other seat from its own tally only after the leader's grace (safe,
  only adds attestation variants). Seal pacing (`SEAL_PACE_SECS`) and
  `sealer_turn` are off at locked slots.

## 6. Acceptor state and persistence

Per seat, `Acceptor`:

- per open slot: promised ballot, accepted (ballot, value);
- the heights it signed freely (B6), kept `FREE_MARK_KEEP` (4096) below
  the head, since a free signature is valid on every branch and a reorg
  can re-open a height.

GC: `settle(head)` after every head move (append, tie-break, reorg,
adopt) drops slots at or below the head. Slots of an abandoned lineage
above the head are harmless and go as the head passes them.

**Write before the frame leaves.** `prepare`, `accept` and
`sign_freely` return `persist`; when set, the engine writes the whole
acceptor (JSON, a few hundred bytes) and records the reply only after
the write acknowledged durable. If the write fails, the reply is not
sent.

Where: its own sealed file `chain_lock.state` in the workspace dir,
written by the storage writer atomically (tmp + fsync + rename) through a
new acked message `persist_lock_blocking(json) -> bool`, the
`persist_wallet_keys_blocking` precedent (wallet I4: the attestation
leaves only after this returns true). Loaded at open, before the
transport starts. Not the event log (group commit; the outbox may read
an entry before its fsync; compaction) and not the dump (written at
close). Never exported: an export restored later would carry stale
promises.

**Amnesia.** A chain without a lock file (import, restore from an
export, a deleted file) means the seat may have promised before. It
answers no lock frame and gives no free signature at heights <= floor;
the floor comes from the answers of max(m, n - m + 1) other seats (R-c,
§14 "Amnesia, the rule as built"). Blocks it receives still apply.

## 7. Wire

Five new `WorkspaceEvent` variants, sender = the MLS-authenticated
`env.by`; all in `crosses_wire`, all in `unowed_on_the_wire` (an older
build drops the unknown variant, and the lock retransmits on its own):

```
LockPrepare  { height, base, round }                         leader = env.by
LockPromise  { height, base, round, leader,
               accepted: Option<{ round, leader, digest, need, id,
                                  change: Option<ChainChange> }> }
LockAccept   { height, base, round, digest, need, id,
               change: Option<ChainChange> }                 leader = env.by
LockAccepted { height, base, round, leader, digest, sig }
LockNack     { height, base, round, leader,
               promised: Option<{ round, leader }>, head }
```

- `sig` in `LockAccepted` IS today's signature: `identity_sign(
  approval_bytes(rid, height, change))`. A seat gives it only for the
  announced value, only at ballot >= its promise, at most one value per
  ballot, and only for a change it approves (own intent, or the
  automatic co-sign of a matching cut / verified consented restore).
- **`Approved` becomes the intent at locked heights:** same variant,
  `sig: ""`, `height` = head + 1 for display only. Intents are
  height-free and are never re-sent on a head move. An older build
  ignores it (`receive_approval` returns early on an empty signature);
  the own-log replay already rebuilds `own_approvals` from it. At free
  heights `Approved` keeps its signature (B6), and that signature is
  `sign_freely(head + 1)` first.
- Signatures in `LockAccepted` never enter `pending_sigs`; free
  signatures in `Approved` never enter a `Tally`. The two pools do not
  mix.
- `Committed` is unchanged. Retransmission: the leader re-sends its
  `LockPrepare`/`LockAccept` every 15 s while it leads an open slot;
  acceptors answer duplicates idempotently (no write).

What an old (v0.0.20) node sees: unsigned `Approved` (ignored), lock
frames (dropped), `Committed` blocks (verified and adopted as today).

## 8. Activation per height (user decision 2026-11-22, as built)

How the members switch it on once it is available (explicitly, in the UI
under Organization, never by a founding default) is decided but not
built: `docs/chain/chain_lock_wiring_plan.md` §11.

The chain decides, not a belief: the lock runs at height h iff the
effective feature set at h - 1 holds the key `chain-lock`
(`molt_core::chain::CHAIN_LOCK_FEATURE`, `State::lock_enabled`).

- **A republic founded with this build** ratifies the key with its
  charter (`founding.rs`: added to every proposed feature set, not a
  wizard choice). It is in the genesis, so height 1 is the first locked
  height.
- **An older republic** enables it by an explicit `set_features` vote
  (value `chain-lock` plus the kept features); the lock runs from the
  height after that block.

Verdict on the vehicle (research 2026-11-22): the feature set fits.
- Typed as canonical strings: sorted, deduped, length-prefixed in
  roster-v5 and in the checkpoint's founding run.
- The key changes their content, never a layout or tag. A table
  without it keeps its bytes (pins `roster_canonical_bytes_v5_*`,
  `the_lock_is_a_feature_key_in_the_unchanged_v5_layout`).
- `set_features` is not a last-write-wins slot, so a cut keeps every
  entry and a pruned holder's union equals a full holder's. No start
  height needs carrying: the mode is only ever asked for head+1, and
  the head's effective set answers it (pinned on a full and a pruned
  holder).
- No new `ChainChange`, no new frame.
- The one cost: the first feature key that is not a surface key
  (`charter_features.md` D1).

What an older build (v0.1.0, v0.0.20) does:

| where | older build |
|---|---|
| joining a founding of this build | refused at the join, both sides told: the founder logs `<seat> runs a build without the lock - refused` and sends the activator `LinkSpent` with the reason (the joiner declares no lock capability in its `JoinRequest`); the ticket stays usable after an update. Without that gate the member would refuse to ratify silently (`verify_canonical_features`) and the ritual would hang |
| founding as the founder | founds without the key: a pre-lock republic, enabled later by vote |
| reading a chain whose genesis or a block carries it | verifies and folds it unchanged (the genesis check does not validate the vocabulary; `fold_one` does not validate org payloads; the union fold keeps the unknown key) - same bytes, same hashes, no fork |
| a checkpoint blob carrying it | hashes it as one more string, identical |
| proposing it | refused (`validate_org_payload`: unknown feature) |
| after activation | keeps signing freely and drops lock frames (§8 boundary below) |

This is milder than the "new surface strands an older seat" precedent:
nothing stops, nothing hashes differently; an older seat merely does not
take part in the lock and is expected to update.

**Guard on enabling.** A seat neither proposes nor signs the enabling
vote unless every other roster seat announced the capability
(`lock_enable_blocker`; the reason names the seat: `<seat> runs a build
without the lock`, or `<seat> has not announced`):
the approver check keeps the threshold from being reached past an
older seat; the proposer check keeps a card that nobody may sign off the
board. An older build cannot propose the key at all. The capability is
otherwise information only. The lock cannot be switched off: like the
vault, a `set_features` value need not name it and the union fold keeps
it (refusing such a card would break every caller that lists only the
surfaces, the GUI among them).

**Capabilities reach every seat** (R10). A seat declares at open, on a
change, and again whenever a peer asks (`MirrorWho`) or a seat is
restored or joins; a seat lacking a roster peer's declaration asks,
backing off from 10 s to an hour. A declaration without a capability is
kept as such (`declared without` differs from `unheard`). A
declaration carries the sender's clock (`at`, additive): a reordered
older one never overwrites newer capabilities; an unstamped one (an
older build) after a stamped one counts only once a repeat period
passed (T4). Without this
a restored seat learned the others' capabilities only at their 6 h
repeat and could not approve the enabling vote.

**The boundary.** The enabling block E at height h is sealed under the
old rules (free signatures, B6). A seat decides the mode for h + 1 only
once its head is E, and then it is locked; a seat whose head is still
h - 1 signs at h, not h + 1. So no height is half-run among honest seats
on one lineage. A B6 fork at h (one branch with E, one without) gives
two lineages and therefore two slots; R1 heals it, and a free signature
given at h + 1 on the losing branch is still covered by the per-height
free mark (exclusivity), so it can never join a lock quorum.

**Seats without the lock after activation** (an older build, or a seat
restored onto one): they sign freely and answer no lock frame.
- Safety: exclusivity, as before. A lock decision needs n - m + 1
  promises from seats that never sign freely there, so at most m - 1
  free signatures exist at that height - for an ordinary value. A
  consented restore seals freely with m - 1 survivor signatures plus the
  consent, so a free restore can stand beside a lock decision (A-F2):
  open, see §14 (a seat without the capability in a republic running the
  lock).
- Liveness: the lock decides while n - m + 1 capable seats promise and
  `need` of them approve. Where the free seats leave it no quorum, the
  release rule (§14) turns the height free and B6 decides.
- Pinned: `a_seat_without_the_lock_after_activation_neither_stalls_nor_forks`
  (one head, fork decisions no more than B6 on the same schedule) and
  `one_older_seat_after_activation_still_ends_on_one_head`.

**Deleted:** activation by capability (`lock_all_capable` deciding the
mode, and the "every seat announced it" belief), the default switch
`CHAIN_LOCK_DEFAULT_ON`. **Kept:** the free/lock exclusivity per height,
the release rule (now only a liveness bridge for older-build seats and a
fork across the boundary, no longer an activation mechanism), and the
re-signing of standing intents when a height turns free (a seat whose
build loses the lock).

## 9. Interplay with the existing machinery

- `rebase_pending_approvals` / `try_reorg`'s re-sign loop: at locked
  heights nothing is re-signed - intents stay, the leader re-proposes.
  At free heights unchanged (B6), each signature behind `sign_freely`.
- `try_commit`: at a locked slot it only answers "which proposal would I
  propose"; the seal is `Tally`-driven. `sealer_turn`, seal pacing and
  `seal_held` apply to free heights only.
- Fork rule, tie-break, deep reorg, relink: unchanged; still needed for
  free heights, mixed operation and attestation variants. A reorg moves
  the base, so the old lineage's slots stop mattering; `settle(head)`
  runs after it. Displaced decisions (R4) can only come from free
  heights.
- Catch-up and served blocks: a block from the wire is a decision; it is
  verified and adopted as today, then `settle`.
- Cuts: `maybe_auto_checkpoint` registers the cut; its co-sign becomes
  an intent plus the accept; the leader applies §4.1.
- `consent_anchor_for` / restore gates (B3): unchanged; `need(R)` uses
  `restore_consent_at` at the slot's height.
- Display: at locked heights the approval count counts intents
  (verified sender = envelope author), not signatures.

## 10. Failure cases

| case | outcome |
|---|---|
| frame lost | the leader re-sends after 15 s; replies are idempotent |
| duplicate / reordered | `prepare`/`accept` are idempotent; a stale lower ballot gets a `Nack` |
| acceptor crashes before the write | change and reply both lost; the state on disk is the old one |
| acceptor crashes after the write | the reply is re-sent on the leader's retry |
| leader dies after m - 1 accepts | the next leader's phase 1 forces the value if any promiser holds it; if none does (the accepts were all outside P) it may pick another - nothing was chosen, and the B6 choice (smallest ready id) usually picks the same change |
| leader sealed, block never arrived | acceptors hold (b, v); a learner with the tally seals, or phase 1 forces v and a new ballot re-seals it (a variant, re-linked by R1 when the old block shows up) |
| two simultaneous takeovers | the lower ballot is nacked; backoff by rank and attempt (§5) |
| seat restarts mid-height | it reloads its promise from `chain_lock.state` before the transport runs |
| seat opened from an export | amnesia floor (§6) |
| partition into minorities | no Q1 anywhere: the height waits; nothing seals on either side |

## 11. Module API (`chain/lock.rs`, as built)

```rust
struct Slot { height: u64, base: String }
struct Ballot { round: u64, leader: String }            // Ord: round, leader
struct Value { digest: String, need: usize }
struct Quorum { n: usize, m: usize }  fn promises(self) -> usize  // n - m + 1
enum Reply { Promise { slot, ballot, accepted: Option<(Ballot, Value)> },
             Accepted { slot, ballot, value },
             Nack { slot, ballot, promised: Option<Ballot> } }  // None = free here
struct Step { reply: Reply, persist: bool }

impl Acceptor {                                         // Serialize + Deserialize
  fn prepare(&mut self, &Slot, &Ballot) -> Step;
  fn accept(&mut self, &Slot, &Ballot, &Value) -> Step;
  fn may_sign_freely(&self, height: u64) -> bool;
  fn sign_freely(&mut self, height: u64) -> Option<bool>; // None refused, Some(persist)
  fn settle(&mut self, head: u64) -> bool;              // changed
  fn next_round(&self, &Slot) -> u64;
  fn promised(&self, &Slot) -> Option<&Ballot>;
  fn accepted(&self, &Slot) -> Option<&(Ballot, Value)>;
}
impl Leader {                                           // volatile
  fn new(Slot, Quorum, Ballot) -> Self;
  fn promise(&mut self, from: &str, &Ballot, Option<(Ballot, Value)>);
  fn ready(&self) -> bool;
  fn forced(&self) -> Option<Value>;
  fn propose(&mut self, own: Option<Value>) -> Option<Value>; // once per ballot
}
impl Tally { fn accepted(&mut self, from: &str, &Ballot, &Value) -> Option<Sealable>; }
struct Sealable { ballot: Ballot, value: Value, signers: Vec<String> } // need lowest names
```

## 12. Test plan for B7b

Red first, invented fixtures.

- Byte pins: the five lock variants; an unsigned `Approved`; the old
  deserializer drops each lock variant (the S3 test pattern).
- Engine units: persist-before-record (a failing lock write sends
  nothing); exclusivity both ways through `chain_sign_and_gossip_approval`
  and the lock ingest; `rebase_pending_approvals` re-signs nothing at a
  locked height; base equality across a relink and after a cut; amnesia
  floor.
- `MockRelay`, three seats, bursts and 10 % drops: zero fork decisions
  (`fork_decisions` stays 0), one head hash.
- A leader killed mid-height (after its accept left): the next leader
  seals the SAME change.
- A seat hard-killed after promising, restarted: its promise holds (a
  lower ballot is nacked).
- Mixed: one seat on an old-shaped build (no capability): B6 behaviour,
  every seat ends on one head, nothing stalls.
- Restore in 2-of-3 with both survivors up: seals with one survivor
  signature plus the consent; with one survivor down: no seal (pinned as
  the intended guard).
- A cut with one seat refusing the hash: the slot is freed and the next
  ordinary proposal seals.

## 13. Open questions (defaults the wiring follows)

1. **One-survivor restore in 2-of-3** no longer seals while the other
   survivor is unreachable. Default: keep (the one-lineage N1 guard); the
   rescue F-X1 covers a republic that lost two seats.
2. **A seat lost during a cut's accept window** pins the slot to the
   cut. Default: propose a cut only with all n promises in hand; the
   rest waits for F-X1.
3. **A live old device of a restored seat** can break §4.2. Default:
   rely on B2/B3, plus the leader's promise exclusion.
4. **Amnesia floor**: superseded by R-c (§14); if too few seats answer,
   the seat stays passive (blocks still apply).
5. **Grace and retry**: `SEALER_GRACE_SECS` (60) for takeover, 15 s
   retransmit. Default: these, re-set from measured latency like B6.
6. **Learner sealing** adds variants. Default: only after the leader's
   grace.
7. **Byzantine seats** are out of scope; ballots and `prev` stay
   unsigned (signing them would be a versioned layout, chain_reorg R6).

## 14. As built (B7b)

Where the wiring differs from §1-§13, and why.

- **The two doors.** Every position signature leaves through
  `State::lock_sign_freely` (the free mark durable first) or
  `State::lock_acceptor_accept` (the acceptance durable first). The only
  production `identity_sign` over `approval_bytes` outside the latter is
  in `chain_sign_and_gossip_approval`, behind the former; every other
  signing path (`approve`, the proposer's co-sign, `rebase_pending_approvals`,
  `try_reorg`'s re-sign loop, `sign_held_approvals`, the cut co-sign,
  `auto_approve_restore`, the wallet/vault paths) calls that function.
  `serve_open_governance` and `reserve_signatures` only re-send
  signatures that already exist.
- **A split height must still decide: the release rule (corrected
  2026-11-22).** Activation is not simultaneous: one seat already runs
  the lock at h (its own `Prepare` is a promise), another still believes
  h free and signs freely. Exclusivity keeps that safe, but nothing makes
  it live. Let F be the OTHER seats known to have signed freely at h (a
  verified free signature, or a free `Nack`; a registered restore's
  subject never counts). A seat r trades ALL its lock state at h durably
  for a free mark (`Acceptor::release_to_free`) once the lock is dead
  there (`Quorum::lock_dead`):

  `F non-empty and |F| >= min(m, n - m + a)`, a = 1 if r holds an
  acceptance at h, else 0.

  Proof. A free seat never promises nor accepts at h afterwards, and an
  acceptance is never followed by a free signature unless released.
  (T1) `|F| >= n - m + a`: a chosen value needs `need` acceptors at one
  ballot, none in F, and r's own only if it holds one, so at most
  `n - |F| - (1 - a) <= m - 1` acceptors exist. An ordinary value and a
  consented restore where m < n need m; a consented restore where m = n
  needs n - 1 among the seats other than its subject Z (Z not in F), of
  which at most `n - 1 - |F| - (1 - a) <= n - 2` exist - nothing can
  ever be chosen. (T2) `|F| >= m`: order the releases in time. Before the
  first one every free seat never had lock state, so every promise set
  lies in the `n - |F| < n - m + 1` non-free seats: no ballot ever
  completes phase 1, nothing is accepted, nothing chosen. Every later
  release happens where an earlier sound release already made the lock
  dead. Releasing an acceptance does not break lock-vs-lock either: it
  only happens where nothing can be chosen.
  Lock-vs-lock with free seats around stays Paxos: free seats are
  unheard (or heard as holding nothing, `Leader::heard_free`, sound
  because a released acceptance belongs to a dead slot).

  **The first rule was wrong (counterexamples, pinned).** As first wired
  it released promise-only state at `|F| >= n - m`. Safe, but a height
  could stall for good: 2-of-2 / 3-of-3 when the leader had already
  accepted its value (an acceptance was never released), and 2-of-5 with
  two free seats (three non-free seats can never give the four promises
  phase 1 needs, while the rule waited for three free). The seeded
  simulation finds them at seeds 2 (2-of-2), 1 (2-of-5), 0 (3-of-3)
  (`the_shipped_release_rule_stalls_and_the_corrected_one_decides`), the
  engine twin is `an_acceptance_is_released_once_no_quorum_can_complete_it`.
  The corrected rule passes 16 shapes x 800 seeds with a liveness phase
  (`simulation_with_free_signers_and_the_release_rule_never_forks_and_decides`:
  2-of-2 to 4-of-7, cuts, consented restores, old builds appearing
  mid-run, crashes between a release and its write, duplicated and
  reordered free signatures); releasing on the first free signer still
  forks (2-of-5, seed 137).

  **The straddle, and why activation became chain state.** With
  activation by capability, a card whose voices split between free and
  lock seats could have m voices and still not seal (3-of-5: one free,
  two lock; `a_value_split_between_a_free_and_two_lock_voices_waits` pins
  the mechanism with a forced belief). It was not rare: right after a
  founding no seat knew the others' capability, and the vault suite
  (2-of-4) stalled at height 1 in 13 tests. Since §8 is chain state no
  two honest seats of one lineage disagree on the mode, so the straddle
  needs a seat on an older build; the release rule then turns such a
  height free once the free seats leave the lock no quorum.
- **Amnesia needs a marker (§6).** "No lock file = amnesia" would make
  every seat of an upgraded republic amnesiac at head+1 at once, and the
  height never decides. `prefs.chain_lock` is set after the first
  successful lock write and travels with an export: file missing with the
  marker set = amnesia; neither = a seat that never ran the lock. On
  every open with the lock on, the replayed own signatures (always free
  ones) are marked free at their heights before anything else, so a
  build without the lock that ran in between leaves no gap. With the lock
  off nothing is written and nothing is passive. An import sets the
  marker where the imported chain runs the lock, whatever the export
  carried, so such a restored workspace opens amnesiac (S6 below); an
  open with the lock off and no file clears a marker (T3: "the marker"
  means "the lock wrote here" - a pre-lock import marked anyway would
  turn amnesiac at the enabling vote and halt an n-of-n republic). It also opens
  detached and comes back only through the recovery ritual. Residual: two live
  copies of one workspace (a copied directory run twice) - B2/B3's
  territory, like a live old device.
- **The amnesia floor** (superseded: the first peer height heard forked,
  explorer R-a) now comes from R-c's answers (§14); past the floor the
  file is written and the seat is ordinary again.
- **The lock file** is `{acceptor, values}`: `values` names the proposal id
  (and the change of a membership change or cut) behind each accepted
  digest, so a promise after a restart still tells the next leader what
  it must force.
- **A forced cut waits for all n promises** before the leader proposes
  (§4.1 refined): with an unheard seat the escape cannot fire, and the
  leader would re-propose a cut one present seat refuses. Waiting is
  always safe.
- **Retransmit** (§7): `Prepare` while phase 1 is short, `Accept` while a
  proposed value is unsealed; an idle leader that holds its promises
  sends nothing. A proposed but unsealed run restarts with a new ballot
  after grace x (attempt + 1) plus the jitter.
- **Takeover** (§5): the k-th seat starts its clock when it first holds a
  ready value (intents reach `need`), asks for the block at k x grace and
  opens its ballot `min(FALLBACK_ASK_SECS, grace)` later.
- **Intents** are every `Approved` whose envelope author is the voter
  (signed or not); an unsigned one is kept in the own log (R12), and the
  own intents are re-served with the open governance. At a locked height
  the display count is the larger of verified signatures and standing
  voices; at a free height it stays B6's verified count. A seat known to
  have signed freely at the slot's height does not count toward making a
  value ready (it can never accept).
- **Lineage**: extended by one block when the head moves by one non-cut
  block, recomputed otherwise (relink, tie-break, reorg, cut, adopt).
- **Status**: `chain_sync.lock_active`, `lock_ballot` (`round:leader`,
  highest seen at head+1), `lock_wait` (`promises k/q missing <seats>`,
  `acceptances k/need`, `leader <seat>`, `amnesia`). Logs: `lock=prepare|
  promised|propose|accepted|nack|yield|retry|takeover_ask|takeover|
  sealed|released|wait|amnesia_floor|amnesia_over|unwritten height=
  ballot=`; a restore waiting for promises logs `lock=wait
  reason=restore_needs_promises missing=` at INFO.
- **A withheld acceptance is retried on the tick** (a restore settling, an
  intent not cast yet): the leader's own acceptance has no resend, and
  waiting for the leader's 15 s resend made every settled restore late.
- **A restore settles before the lock accepts it** (`RESTORE_SETTLE_SECS`,
  as the B6 approval and seal do): a withdrawal right behind the card wins.
- **Inbound frontier** (delivery rework, master 2026-11-22): an intent is
  a stance and waits behind a missing seq like any `Approved`; the five
  lock frames are not order-sensitive and pass a gap at once. They do
  carry log seqs, so like every envelope a lock frame more than the
  accept window (1 024) above the frontier is buffered and applied bottom-up
  - stale and ignored by then, never a decision out of order.
- **The line leaves a restore's subject out** (§5 as designed; the
  first wiring used B6's line unchanged, so the lost seat, still "heard"
  seconds ago, led the height and the survivor waited a full grace:
  every recovery test timed out). A seal held by the restore gate is
  retried on the tick.
- **A build without the lock** (the test seam `__lock_cap_off(true)`)
  writes no lock file, keeps no free marks, counts no intents - B6
  exactly, which is what an older build does.
- **A capability change is announced at once** (the mirror declaration is
  re-sent when this seat's lock capability flips), not on the next
  repeat.

**Residual outside the threat model: colluders.** Seats running modified
code that reach `need` acceptances of a value at one ballot make it the
slot's chosen value, even when no honest seat registers or verifies it
(a forged vault deposit). Every later leader must force it, cannot, and
the height stalls; B6 let the honest card seal beside the refused block.
The vault keystone `a_forged_depositor_is_refused_by_approver_and_verifier`
therefore runs on a pre-lock republic. Closing it needs acceptors that
refuse what verification refuses and leaders that may skip a forced
value the walk would reject - a Byzantine extension, not built.

**Exposed elsewhere: two founding wallet inits.** When two seats both
auto-propose `wallet_init` at a founding, a seat consents only to the
init cards it saw at its first claim (`wallet_founding_claim`). The
lock's leader takes the first card with `need` intents, which can be the
one a third seat never claimed; the n-of-n DKG then aborts `not ready`.
`wallet_scan.rs` failed 2 of 7 runs this way, B6 0 of 8. Proposed fix
(a consent change, for decision): a seat's founding claim takes in every
init card it sees before one applies.

Not done: a 2-of-2
republic whose seat is amnesiac waits for that seat's floor to pass,
which only another block moves (§13 Q4). The rejoiner's checklist now
counts the survivors' intents.

### Review round (2026-11-22, two reviewers, findings S1-S6, W1-W10)

Both reviews found the safety machinery sound; the findings were
liveness and the downgrade boundary. Each fix has a red-first test.

- **S1/W1, an acceptance is a promise kept.** A value this seat already
  accepted at the slot is accepted again at a higher ballot without the
  approval gates (withdrawn, declined, a check turned false): it may be
  chosen with every `LockAccepted` lost. The lock file keeps the whole
  change of each accepted value, so a withdrawn card stays resolvable.
- **S2/W3, a downgrade cannot open a locked workspace.** The first lock
  write raises the manifest to version 4 (`STORAGE_VERSION_LOCK`); older
  builds (ceiling 3) refuse it at the gate, sealing keeps it. Nothing is
  written, marked or amnesia-gated before the chain enables the lock, so
  a pre-lock republic stays openable by older builds until its vote.
  Defence in depth: a seat seen ACCEPTING at a height is never counted
  free there; one seen only promising counts once the seats already
  known free kill the lock for a seat holding nothing - the release rule
  it applies to itself, so a sound cascade never stalls (review R4); a seat that finds its own free signature beside
  its lock state at open goes passive at that height
  (`lock=passive reason=conflict`). Residual: lock frames of a seat that
  were all lost, followed by a downgrade from a backup the marker never
  saw.
- **S3, a forced value waits for a fuller hearing for one retransmit
  interval** while seats are unheard (its holders keep a value nobody
  chose alive), then goes out anyway so the holders of a chosen value
  complete it.
- **S4/W4, one rule for phase 1.** A busy run (promises missing, a forced
  value waiting, a value out) re-sends at a doubling interval to 600 s
  (reset by a new promise) and goes stale after its backoff. An idle
  leader - promises in hand, nothing to propose - sends nothing (the
  first version of this rule did, and `nostr_outbox_quiet` caught it).
- **S5.** Seats known free are heard as free; promisers and free seats
  stay disjoint.
- **R6.** A free signature given on a lineage without the enabling
  block is marked free when the head moves onto one that runs the lock,
  before any lock reply there - what §8's per-height free mark claims.
- **R7.** A card withdrawn after the lock chose it seals anyway: the
  record reads applied and withdrawn, and its proposer gets
  `sealed-after-withdrawal:<id>`. Once the card is retired the leader
  cannot resolve a `LockAccepted` for it, so only a seat holding the
  value seals it - slower, not a stall.
- **H1, a forced value is completed by any promiser.** A lost seat
  counts as a voice that may hold any ordinary value at the slot, so a
  value fewer than `need` survivors hold can stay forced while the lost
  seat is silent. In Paxos an acceptor never refuses a value a leader
  was forced to propose: a seat accepts a `LockAccept { forced: true }`
  whether or not it approved the card, also when the card was withdrawn
  (an approval gates what a leader may FREELY pick, not what a seat must
  complete). The leader carries the whole change on a forced Accept, and
  a promise reports the whole change of the value it holds (C-MED-5), so
  a seat that never saw the card can complete it. What such a seat
  checks before signing (M4, `lock_forced_invalid`): the carried change
  hashes to the forced digest; the checks an incoming card meets
  (`receive_proposed`: no reserved op, the vault and wallet wire shapes,
  the feature-set refusals, no other change under the id); a restore's
  consent valid at the height; a cut of this height with this seat's
  state hash; any other membership change only as registered here. The
  withdrawal, the settle, the seat-still-active gate and the local
  vault/wallet checks do not hold. **A withdrawal under the lock means
  "do not choose this if it is not yet under way"**; a value that seals
  anyway is reported (`sealed-after-withdrawal`, for a restore
  `restore-sealed-after-withdrawal` with no re-key). Residual: a forced
  cut whose hash no longer matches on every seat stays forced (C-LOW-7,
  A-F6) - a cut with a lost seat halts. The local wallet/vault refusal
  is lost on the forced path (L5); no divergence follows, the change is
  the chain's. Pinned: `a_forced_withdrawn_value_completes_with_any_promiser`,
  `lock_sim` (H1 halt on the old rule at seed 32).
- **Item 3, a forced value carries evidence.** H1 alone let ONE bad
  seat lead a `LockAccept { forced: true }` with any change and collect
  m - 1 good signatures. Now a forced Accept carries `evidence`: real
  position signatures on exactly that change at that height by TWO
  DISTINCT seats, the sender allowed as one (`lock::evidence_holds`,
  called by the wire and the simulation alike). One signer other than
  the sender is not enough: a bad seat answers a good leader's Prepare
  with a made-up value it "accepted" (it signs what it likes), the good
  leader is forced to it and forwards that signature - modelled, 1911
  acceptances of made-up values over 900 seeds, 0 with two signers. A
  promise that reports a held value carries the holder's own signature;
  the leader sends the acceptances it collected (promises, `LockAccepted`
  frames, at most two) and adds its own only where it accepts the value
  itself (it holds it, approved it, or already has two). A reported
  value's `need` is recomputed from its change and a report whose change
  does not hash to its digest is dropped - a need-1 lie would otherwise
  force the value at every ballot. Without valid evidence a seat judges
  the value as it would a free one: only its own approval lets it accept.
  Induction (one bad seat B, signatures unforgeable): every value a good
  seat accepts was approved by a good seat. A free acceptance needs the
  acceptor's own approval; a forced one needs two distinct real
  acceptances, at least one by a good seat, which (earlier in time) rests
  on a good seat's approval; a good leader signs only what it accepts.
  So B cannot get a change no good seat approved accepted. (Review 6:
  this missed that a good leader vouched by card id, not by change - F1,
  fixed below; the rule itself is an open decision, wiring plan §12.) What B still
  can do: (a) replay acceptances good seats really gave - force a value
  good seats once accepted at that height (approved, possibly withdrawn
  since) at a higher ballot without a phase 1; Paxos is not
  Byzantine-tolerant, and a leader skipping phase 1 can make a second
  value chosen among values good seats approved, as it already could
  among freely approved ones. (b) Stall: its lie forces the made-up value
  only while 1 + unheard >= m (`Leader::forced` counts every unheard seat
  as holding it), so it stalls a height only while m - 1 other seats are
  unreachable, and the height decides once they are heard (S3 waits one
  interval for a fuller hearing; a stale ballot re-runs phase 1). Free
  signing stalls when fewer than m good seats are reachable: the mode is
  worse only where m - 1 <= n - 1 - m (2-of-5: the liar and one absent
  seat stall the mode, free signing still has three good seats; 2-of-3
  and 3-of-5 stall together). Modelled: `a_liar_stalls_only_while_m_minus_one_seats_are_away`
  (2-of-3, 2-of-5, 3-of-5, 4-of-7 with m - 1 away and back decide; 2-of-5
  with one away for good stalls in 150 of 150 runs).
  Liveness exit, staged: a value only one seat holds has no evidence. A
  seat withholds it (`lock=withheld reason=needs approval`); its status
  reads `decision <id>: waiting for <seats>` (the seats whose acceptance
  would vouch for it), the leader's resends recompute the evidence, and
  the wait ends by itself once one of them is heard. The approve is
  offered - `decision <id> may already be made - approve to complete it`
  - only after one retry interval (the leader's retransmit step) AND
  while every seat it waits for, other than the leader that sent it, is
  silent here beyond the still-active window (a seat heard recently will
  most likely answer; the sender's evidence already says what it vouches
  for - waiting on it deadlocked the model) AND the value is not already
  being sealed here; until then the status keeps the wait.
  Then an `approve` of that card is accepted although the card is
  withdrawn; before that, and for every other withdrawn card, approve
  stays refused (`AlreadyTerminal`). A seat that approved the card itself
  is never asked: it accepts on its own intent
  (`a_seat_that_approved_the_card_is_never_prompted`). A seat that never
  had the card registers the carried change as a withdrawn card marked
  `pending_completion` (proposer from the id's stripe, content readable;
  never open, never a free pick, gone once its height decides), so the
  operator reads what it completes
  (`a_forced_value_without_its_card_is_completed_by_an_approve`,
  `a_pending_completion_card_never_opens_and_goes_with_its_height`). MCP
  `approve` works; the window has no control for it yet (required before
  the mode is offered, wiring plan §12). How often, in the simulation
  (150 runs per shape, 1 % and 5 % frame loss): (i) a seat reaches the
  wait - 2-of-3 in 22, where a seat comes back 33-40; (ii) the approve is
  offered - 7-8 in 2-of-3 without a failure, 7-13 where a seat comes
  back; (iii) the height cannot decide without it (no human answers) - 0
  without a failure; where a seat in 2-of-3 dies for good 31-40, and
  there the offer appears in the same runs ((ii) = (iii)): the H1 shape,
  the exit by design. Without the silent-seats rule (ii) was 13-23. The
  remaining 5 % are offers a later approve of the card at the leader
  (its resend then vouches) makes unnecessary - not predictable here.
  3-of-5 and larger: (ii) 0.
  Size: under the mode the propose and wire gates also bound the largest
  lock frame (`proposals::lock_frame_plaintext_len`: a forced Accept with
  the change and two evidence signatures, a promise with the change and
  one); with two seats these outgrow the block by about 180 B, so the
  largest payload is 96 575 B instead of 96 754 B under a 97 508 B
  ceiling (`a_forced_accept_with_evidence_fits_the_transport`). A card
  registered just before the mode turned on and within that margin is
  refused at the lock stage - `the proposal is 1 KiB over what this
  republic's relays carry` at approve, never chosen - and stays
  withdrawable (`a_card_too_large_for_the_lock_frames_is_refused_and_withdrawable`). Additive
  wire: `LockAccept.evidence` and `LockAcceptedValue.sig` are skipped
  when empty (the old byte pins hold), filled pins are added, and an
  older reader ignores them.
  Pinned: `a_forced_value_without_evidence_waits_for_an_approve`,
  `forced_evidence_needs_two_real_acceptances`,
  `a_promise_cannot_lie_about_need_or_digest`,
  `an_approve_of_a_withdrawn_card_is_refused_unless_the_status_names_it`;
  modelled: `a_made_up_forced_value_finds_no_good_acceptor`,
  `how_often_the_exit_prompt_appears`.
- **M3, a chosen value seals.** `lock_seal` holds nothing: the
  seat-still-active gate decides only what a seat ACCEPTS, never the
  seal of a value the lock already chose (a gate at the seal pinned the
  height while the restored seat's old device ran). The coordinator's
  re-key then removes the old device's leaf (`seat-evicted`, B2).
  Pinned: `a_chosen_restore_seals_while_the_old_device_is_heard` (the
  gate at its default).
- **A-F2, the capability rule.** No seat enters a republic in Strict
  Order Mode on a build without it: the rejoiner's request declares its
  capabilities (`RecoverRequest.caps`, additive), and the coordinator
  refuses one without the lock (`this republic uses Strict Order Mode -
  needs a newer build`, told to the rejoiner;
  `a_rejoiner_on_a_build_without_the_mode_is_refused`). The release rule
  covers only a seat that was already there when the mode came on.
  Why it holds without a signed capability (item 7): every seat of a
  republic in the mode keeps workspace version 4, so no seat goes back to
  a build without it; the enabling vote needs every seat capable; a seat
  without it can only enter by a restore, and every coordinator is a
  capable seat. But not only at the request: schedule (2-of-3 A, B, Z,
  the mode off) - Z rejoins from a build without it; A proposes R(Z)
  (correctly, the mode is off); the enabling vote seals while R(Z) still
  waits; R(Z) then seals under the mode, and A's re-key would welcome a
  device that signs freely. So the coordinator keeps each request's
  declared capabilities and, at the seal, withholds the re-key and
  Welcome where the chain runs the mode and the device lacks it
  (`restore=unwelcomed`; the coordinator shows `<member> re-admitted on
  the chain, device not welcomed: needs a newer build`, the rejoiner
  `this republic uses Strict Order Mode - needs a newer build`). The
  seat stays restored on the chain with no device in the group. The
  updated device's retry of the same link is answered `link outdated`
  (its anchor is in the chain, review R2); a new link restores it again
  and welcomes it. The identity key is phrase-derived and unchanged by a
  restore: an old device keeps its MLS leaf and its signatures verify
  until a re-key removes the leaf. Second guard: the enabling vote is
  refused at propose and approve while a restore card is open (`Strict
  Order Mode waits for a pending restore`) - not load-bearing, the vote
  may be signed before the card appears. Pinned:
  `a_device_without_the_mode_is_not_welcomed_and_comes_back_on_a_new_link`
  (relays, the schedule end to end with the race staged by a seam, then
  the same link and a new one), `a_restore_under_the_mode_does_not_welcome_a_device_without_it`,
  `an_open_restore_holds_the_enabling_vote`.
- **Not offered where m = n.** Every value needs every seat there. A
  seat lost between the others' acceptance of a value and its own pins
  that height for good: the value is forced (n - 1 holders + the lost
  seat >= n) and needs the lost seat, which may have sealed it alone on
  its own device, so no restore may take the height. Free signing has no
  such halt (an unsealed value stays open; the restore seals with n - 1
  signatures and the consent). With amnesia's lack of a safe return
  there, the enabling vote is refused at propose and approve (`Strict
  Order Mode needs fewer approvals per decision than members`,
  `the_mode_is_not_offered_where_every_seat_must_approve`). Modelled:
  `where_m_equals_n_a_seat_lost_mid_height_halts` (a seat dies mid-run:
  3-of-3 halts in 126, 2-of-2 in 63 of 150 runs; the same shape with
  m < n decides). No safe escape: superseding the pinned value by a
  restore that all n - 1 survivors accept is what free signing does, and
  forks when the lost device returns with the value sealed.
- **A seat never accepts its own restore, on any path** (explorer 3):
  `lock_acceptor_accept` refuses it before the forced and the kept paths
  - the count and the seal filtered it before, but no filter stands in
  for a refusal (`a_seat_never_accepts_its_own_restore_even_forced`).
- **Amnesia, the rule as built (R-c).** A seat that lost its lock state
  (no file where one was kept, an unreadable or unwritten one, a file
  ahead of the chain, an import) asks the others (`LockAmnesiaAsk`, a
  fresh nonce per open, re-sent each retry interval); only seats that
  hold their own state answer (`LockAmnesiaReply`: `touched` = max(head +
  1, the highest height with any lock state there) and the highest round
  seen from the asker) - an amnesiac seat answers nothing. With answers
  from max(m, n - m + 1) distinct OTHER seats to this ask, it is passive
  up to the highest `touched` and its own ballots start above the
  highest round reported (load-bearing: an acceptor cannot refuse a
  reused ballot it only promised;
  `an_amnesiac_seat_leads_above_the_rounds_reported`). The arithmetic is
  pure (`lock::amnesia_need`, `amnesia_touched`, `amnesia_floor`), shared
  with the simulation and the explorer. Status: `amnesia - waiting for
  <k> of <need> seats to answer`; where too many other seats ask too (are
  amnesiac themselves) for an answer set to form, `amnesia - restore this
  seat with a new link` - two amnesiac seats of three halt the slot under
  every rule until one is replaced by the recovery ritual
  (`two_amnesiac_seats_of_three_say_restore`). Replaced: the old floor
  (1 + max(own head, the FIRST peer height heard)) forks - a peer one
  block behind answers first (explorer R-a). Answers from n - m + 1 and
  a floor from heads fork where n < 2m - 1 (explorer R-b, 4 seats m = 3:
  the forgotten promise is known only to the n - m others of that
  phase-1 set; `the_amnesia_control_r_b_forks_where_r_c_holds`). Pinned:
  `amnesia_waits_for_enough_answers`,
  `only_a_seat_with_its_own_state_answers_an_ask`; modelled:
  `an_amnesiac_seat_waits_for_the_others_and_never_forks`. m = n is not
  offered.
- **Review 6 (a7c12a17), fixes F1-F5.**
  F1: an approval is of the change this seat's own record holds under
  the id. `lock_forced_vouched` counts an own approval, an exit approve
  or an intent only where `proposal_change(id)` equals the carried
  change, and a leader adds its evidence signature only where it holds
  the value or would itself accept it with every check run
  (`lock_forced_invalid` included) - before, a deviating promiser's P'
  under an approved id got the leader's signature and a sealable block
  (`a_leader_signs_nothing_for_a_change_it_did_not_approve`). F2: a
  change whose proposal id the chain already applied is refused on every
  acceptance path and dropped from promise reports; an applied card's
  approval is cleared (`an_applied_card_is_never_accepted_again`). F3:
  `ROUND_MAX` = 2^40 - a peer round above it is refused in every lock
  frame and amnesia answer, and own rounds add with saturation: honest
  rounds start near 1 per slot and grow by one per attempt above the
  highest seen, so 2^40 is out of reach (`a_huge_round_neither_aborts_nor_pins`).
  F4: lock state is kept for head + 1 only - a frame for another height
  touches no map - and an amnesia answer reports this seat's own state
  (`a_far_height_poisons_nothing`). F5: a promise's signature is evidence
  only where it verifies over exactly the reported change
  (`a_junk_promise_signature_is_no_evidence`).

  Every position signature this build produces, and where the signed
  change comes from:

  | site | change signed | why it is this seat's own |
  |---|---|---|
  | `governance::chain_sign_and_gossip_approval` (free mode) | `proposal_change(id)` | its own card record, registered through `receive_proposed` / the membership register; `sign_blocker`, vault and wallet checks run |
  | `lock_acceptor_accept`, not forced | `proposal_change(id)` | its own record, `lock_value` equal to the value, `lock_withheld` (own intent, open, gates) |
  | `lock_acceptor_accept`, kept | the value this acceptor already holds | validated in full at its first acceptance |
  | `lock_acceptor_accept`, forced | the carried change | `lock_consumed`, then own approval of exactly that change (F1) or two real acceptances by distinct seats, then `lock_forced_invalid` in full |
  | `lock_on_prepare` (a promise's `sig`) | the change this acceptor stored when it accepted, else its own record | only a value this acceptor holds |
  | `lock_forced_evidence` (the leader's own) | the carried change or its own record | only where it holds the value, or `lock_forced_vouched` AND `lock_forced_invalid` pass (F1) |

  Other signatures are not position signatures: the founding roster
  table (sign-what-you-see, recomputed), the restore consent (bytes the
  seat builds itself), a vault deposit (its own deposit).

  Peer-supplied numbers in `lock.rs`/`lock_wire.rs`: rounds (every lock
  frame, a promise's reported round, a Nack's promised round, an amnesia
  answer) - bounded by `ROUND_MAX`; heights - only head + 1 is processed;
  a cut's `upto` - `checked_add`; `need` - `try_from` into usize, compared,
  never added; amnesia `touched` - compared and kept as a floor (a lying
  answer can hold the asker passive: open, §12). Own-only arithmetic
  (head + 1 of a verified chain, the clock plus own intervals, attempt
  counters, the file's own floor - saturating) is not peer-reachable. In
  the dark build every lock handler returns at `lock_listens()` before it
  reads a field (`lock_on_amnesia_reply` now as well); `note_peer_claim`
  in `lock_on_nack` runs after that check.
- **Item 2, an import's lock state is unknown.** Every import sets the
  workspace pref `lock_unknown` (whatever the chain runs: the mode may
  come on after the export, and the device the export came from may have
  promised since). A seat with it is amnesiac from the first locked
  height it reaches - at the open where the chain already runs the mode,
  or at the head move that turns it on (a catch-up included) - and the
  pref clears once the floor is passed. It raises no workspace version.
  Pinned: `an_import_turns_amnesiac_when_the_mode_comes_on`, the storage
  import test; modelled: `an_import_from_before_the_mode_is_amnesiac`
  (237 forks over 1 800 runs opened fresh, 0 under the rule).
- **Operability (C-MED-3, C-MED-6, B-F3, B-F6, B-F7, D-F3).** A card whose
  ballot went stale stays behind the other ready cards across heights
  until it is retired (a never-sealable card no longer costs a timeout
  at every height; `a_stale_card_stays_behind_at_the_next_height`). A
  withholding acceptor logs its reason at INFO once per height and card;
  the leader's status names the promisers whose acceptance is missing
  (`acceptances 1/2 missing walter`). Not built: an answer frame that
  carries the reason to the leader. The manifest version is raised in
  memory only once it is on disk, and an open heals a lock file whose
  raise never landed (`a_lock_file_raises_the_version_at_open`); the
  marker alone keeps version 4 through a seal round trip
  (`a_lock_marker_keeps_the_lock_version_through_sealing`). Lock
  deadlines run on a monotonic clock. An unowed lock frame never takes
  the link of the owed envelope that shares its `prev_seq` - it stalled
  the sender's frontier (`an_unowed_lock_frame_never_takes_an_owed_link`).
- **A-F3.** Lock state at a height on any lineage forbids a free
  signature there, also on a lineage without the enabling block - the
  signature is base-agnostic and would count free where the lock holds
  (`lock_state_on_another_lineage_forbids_a_free_signature`).
- **Fault tolerance (A-F4).** The lock decides while max(n - m + 1, m)
  seats answer; it halts beyond min(m - 1, n - m) lost seats (B6: n -
  m + 1, since a consented restore there needs m - 1 survivors). 2-of-3:
  1 (2); 2-of-5: 1 (4); 3-of-5: 2 (3); 3-of-7: 2 (5); 4-of-7: 3 (4). The enabling vote is refused where m < 2
  (`Strict Order Mode needs at least 2 approvals per decision`,
  `the_mode_needs_two_approvals_per_decision`).
- **B-F1/D-F1, a seat never forgets what it promised or accepted at a
  slot that can still be contended.** A head move forgets slots only up
  to `min(durable_head, max(last cut, head - SLOT_KEEP))`: never above
  the chain on disk (the lock file is written before the chain in every
  receive path, so a crash or a failing chain write would otherwise leave
  a promise forgotten that the reopened chain still needs), and never
  inside the reorg window (attestation variants let a seat leave a
  lineage and come back below its old head). The lock file records the
  floor it forgot up to; a file whose floor is above the chain on disk
  loads and is amnesiac up to floor + 1 (`lock=amnesia
  reason=ahead_of_chain`). Finality: the
  fork rule has no depth bound, so only a cut is final; `SLOT_KEEP` = 256
  heights (measured: about 96 KiB of lock file at one slot per height,
  plus the open slot's whole change - below the head the file keeps a
  digest's id, not its change, so the worst case is no longer 256
  payloads of up to 70 KiB each) is the
  residual - a reorg deeper than that onto a lineage this seat held state
  on. A learner's tally for a slot that is no longer head+1 gives way to
  the current one.
- **R8.** Intents that outrun their card survive a head move (up to
  1,024 unknown ids, oldest first out); a retired card's go.
- **W2/R5.** The leader skips a card the chain-wide gates refuse now
  (relay wait, a restore gate and consent, a cut's hash); a seat's own
  vault/wallet checks only stop its acceptance, since another seat may
  pass them. A card whose ballot went stale without reaching need (every
  acceptor withholding, e.g. a wallet daemon down) waits at that height
  while another card is ready; when all went stale they take turns, the
  one stale longest first (T2: a card blocked for good never takes every
  ballot from one that went stale once). A
  forced value is never skipped. Residual (L6): a free pick that a late
  promise turned into a forced value is not marked stale when its
  ballot lapses; it is the forced value then, and forced values are not
  rotated.
- **W5** voices before digests; **W6** marks unwritten at open mean
  amnesia; **W7** no amnesia where the lock is not enabled; **W8** the
  restore label counts intents; **W9** an `Accept` before its card is
  retried; **W10** activation reads the same on full, pruned, re-anchored,
  restored and reopened seats (pinned).
- **Simulation:** withholding acceptors (values withdrawn after an
  acceptance or a choice): every shape decides; without S1, or waiting for
  every seat forever, it stalls. Not modelled: the downgrade (the storage
  gate closes it), head moves and pruning (pinned in the engine tests).
- **S6, a residual outside the crash model.** A filesystem backup that
  restores the chain AND an older but readable `chain_lock.state` is not
  detected: the seat may re-promise a slot whose acceptance it lost. An
  operator who restores such a backup must DELETE `chain_lock.state`
  (never restore it): with the marker set and the file gone the seat is
  amnesiac - passive up to the floor R-c's answers fix (§14) - which is
  the safe state. The product's own paths cannot produce the case: an export
  (file or S3 backup) never carries the lock file, and an import sets the
  marker where the imported chain runs the lock, so such a restored
  workspace opens amnesiac
  (`a_restored_workspace_is_amnesiac_only_where_its_chain_runs_the_lock`;
  since item 2 every import is amnesiac once the mode runs). The earlier
  "first peer report" floor is superseded by R-c.
