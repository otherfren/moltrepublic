# The chain lock: fork-free sealing (B7)

**Status: BUILT on a branch (B7a pure state machine, B7b wiring,
2026-11-22): the lock runs where the chain enabled it - every founding of
this build, or an older republic after a `set_features` vote (§8);
deviations in §14; plan `docs/chain/chain_lock_wiring_plan.md`.** Modules:
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
- **Quorums are m-of-n per value** (m; n for a cut; m - 1 live for a
  consented restore), not majorities.
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
| `Membership{Restored}` with a consent valid at the height | max(m - 1, 1) |

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
a height window and Z's previous anchor, v2), not a live acceptor. It
counts as Z's acceptance of the restore R at every ballot, so `need(R) =
m - 1` live survivors. Safe iff Z's lost device never acts as an
acceptor (it is gone by definition of a restore, and B2/B3 gate the
restore of a live seat): then the acceptor universe is N \ {Z}, size n -
1, and Q1 + (m - 1) = n > n - 1 still intersects - also for any value
Z's old device accepted before it died (its acceptances are at heights
up to its last head + 1; a quorum minus Z still meets P).

- The consent rules stay as they are (`restore_consent_at` at the slot's
  height, single use, window). A consent invalid at the height means the
  change is not a value there; acceptors never accept it.
- The block carries m - 1 survivor signatures plus the consent, as
  today; with m survivors accepting, the m lowest-named survivors sign
  and the consent still rides (`try_commit`'s rule).
- Guard: a leader does not count a promise from a seat it knows is the
  subject of a registered consented restore (fewer promisers is always
  safe).
- **Changed liveness, on purpose:** phase 1 needs n - m + 1 seats, so in
  2-of-3 BOTH survivors must be reachable to restore the third (2-of-2:
  the one survivor suffices). On one lineage this removes N1's shape (a
  survivor restoring a seat alone); two survivors on two diverged
  branches have two slots, which stays B3's `chain diverged` gate.
- Cannot guarantee: if Z's old device is in fact alive and promises (the
  N1 shape), P may miss R's acceptor and a different value can seal at
  that slot. B3 is the defence; the lock degrades to R1 healing there.

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
answers no lock frame and gives no free signature at heights <= floor,
floor = max(own head at open, highest height a peer reported after the
first catch-up answer) + 1. Blocks it receives still apply.

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
| founding with the key | refuses to ratify (`verify_canonical_features`: unknown feature) - the founding fails honestly, no mixed republic is born |
| reading a chain whose genesis or a block carries it | verifies and folds it unchanged (the genesis check does not validate the vocabulary; `fold_one` does not validate org payloads; the union fold keeps the unknown key) - same bytes, same hashes, no fork |
| a checkpoint blob carrying it | hashes it as one more string, identical |
| proposing it | refused (`validate_org_payload`: unknown feature) |
| after activation | keeps signing freely and drops lock frames (§8 boundary below) |

This is milder than the "new surface strands an older seat" precedent:
nothing stops, nothing hashes differently; an older seat merely does not
take part in the lock and is expected to update.

**Guard on enabling.** A seat neither proposes nor signs the enabling
vote unless every other roster seat announced the capability
(`lock_enable_blocker`, reason `a seat runs a build without the lock`):
the approver check keeps the threshold from being reached past an
older seat; the proposer check keeps a card that nobody may sign off the
board. An older build cannot propose the key at all. The capability is
otherwise information only.

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
  free signatures exist at that height.
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
4. **Amnesia floor** uses peer-reported heights. Default as §6; if no
   peer answers, the seat stays passive (blocks still apply).
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
  (T1) `|F| >= n - m + a`: a chosen value needs `need >= m` acceptors at
  one ballot (a consented restore m - 1 among the seats other than its
  subject Z, Z not in F), none in F, and r's own only if it holds one, so
  at most `n - |F| - (1 - a) < m` (restore: `< m - 1`) - nothing can ever
  be chosen. (T2) `|F| >= m`: order the releases in time. Before the
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
  off nothing is written and nothing is passive. An export made before
  the lock ran carries no marker and looks like a fresh upgrade - but an
  imported workspace opens detached and comes back only through the
  recovery ritual, whose restored seat is §4.2's Z with a fresh acceptor,
  so no forgotten promise of the old device is ever asked for. Residual: two live
  copies of one workspace (a copied directory run twice) - B2/B3's
  territory, like a live old device.
- **The amnesia floor** is fixed by the first peer height this seat hears
  after the open (a block, catch-up samples, a cut proposal, a `Nack`'s
  head), not only a catch-up answer; past the floor the file is written
  and the seat is ordinary again.
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
