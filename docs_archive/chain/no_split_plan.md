# No splits: one decision per height

**Status: REJECTED, never started (design and staging written 2026-11-25
on master = tag v0.1.3). The user rejected the trade in §7 on 2026-11-27:
in 2-of-3 a restore would need both other members online, two lost seats
would mean refounding, an import could not vote until restored. Restores
stay possible with one surviving seat and consent; the existing model is
improved instead (`docs/rejoin_completion_plan.md`). Kept as the analysis
of how two blocks get sealed at one height today (§1.1) and of the design
space (§3, §4); do not build from it. One piece is reused: the activation
of §6 for the rule that a cut outranks a card at one height
(`docs/rejoin_completion_plan.md` §C.4c).**

Threat model: honest seats on unmodified builds; crashes, restarts, device
changes, imports; loss, delay, duplication, reordering and partitions of
any length. A modified build can always sign twice: that is detectable
(§5.4), not preventable, and out of scope. Seats are A, B, C.

## 1. The problem

### 1.1 How two blocks are sealed at one height today

| # | schedule (2-of-3 unless noted) | code that allows it |
|---|---|---|
| T1 | B approved p1 and p2, so it signs both at h. A holds {A,B} on p1, C holds {B,C} on p2. Each is first in its OWN sealing line (it considers the other silent: no frame heard for 120 s) and seals | `resign_standing_approvals` `governance.rs:371`; `sealing_line` `:835` with `unseen_seats` (`SILENT_AFTER_SECS = 120`, `sync.rs:110`); `sealed_a_smaller_id` `:953` sees only local `pending_sigs` |
| T2 | A seals p1 at h; the block to C is late beyond C's turn (60 s grace + 30 s ask, `sealer_wait` `:79`) and C's catch-up answer is late too; C seals p2 | `sealer_turn` `:859` |
| T3 | two survivors each hold a restore card for B (two links, or a split group); each seals `Restored(B)` with itself + B's consent: m-1 + consent = m | `try_commit` `:765-799` (the consent is one voice) |
| T4 | C + consent of B's new device seals `Restored(B)` at h; A + B's OLD device seal X at h | the same, plus nothing binds a signature to a device |
| T5 | every seat co-signed a cut at h and signs its standing card at h; one sealer seals the cut (n), another the card (m) | cut need `:788`; the cut co-sign is automatic |
| T6 | a re-sign after a tip swap at h-1 | not a new fork: since v0.1.3 the signature names `prev` (`molt-chain-change-v3`, `persistent_chain.md` §5.1), so it continues the fork at h-1 |

T1/T2 are the confirmed field mechanism (`chain_issue_claims_check.md`
11-4; height 928 in `docs_archive/chain/rejoin_divergence_findings.md` §2).
T3 is the restore shape of the same findings: from 933 on every branch
carried `Restored` blocks signed by ONE seat plus a consent.

Why the sandbox never forks (several hundred cards on v0.1.2/v0.1.3 over
a local relay, with bursts, SIGSTOP and restarts): T1 and T2 need a block
or a signature to arrive later than the seal pace (5 s) or the fallback
turn (90 s), or two seats to disagree on presence (120 s). A local relay
delivers in milliseconds, so every seat sees the same signatures and the
same presence. The live republic had minutes of Tor latency, relay replay
gaps (`chain_divergence_plan.md` C6/C7) and seats away for days: forks at
79 heights in one day before v0.0.18 (O1 there).

### 1.2 What "split" means, and who covers what

| mechanism | heals by itself | this plan | remains for |
|---|---|---|---|
| chain fork: two blocks at one height | yes, by the fork rule (`tie_break`, `try_reorg`), displacing applied decisions | prevented from the activation height H | heights below H; republics that never activate; modified builds (branch choice) |
| lasting split: seats never converge (an unreadable branch, different cuts, relay gaps) | no | prevented where it starts as a fork (T1-T5); a cut can no longer differ | catch-up/relay defects: `docs/rejoin_completion_plan.md` O1/O2 |
| MLS group split: branch-local restores re-key each other out (finding N1) | no | prevented: one `Restored` per height, and a restore needs a survivor quorum (§4.3), so a lone seat cannot restore | concurrent re-key commits, a lost Welcome, a stale leaf: the rejoin plan C.1-C.3 |

### 1.3 The goal as testable properties

- **SAFETY.** For every height h ≥ H, no two different blocks (block
  identity v2, `persistent_chain.md` §5.1) with m valid signatures exist,
  across any interleaving of crashes, restarts, device changes, imports,
  restores (also twice), and an old device that keeps running.
- **LIVENESS.** A height decides once m seats with their voting record are
  connected long enough for two round trips and at least m of them approve
  one open card; a restore of Z decides with s(n, m) survivors (§4.3). No
  step has a fixed give-up; retries back off and say what they wait for.
- **OPERATOR.** Every state that cannot progress names its cause and the
  missing seats in one line, in status and over MCP (§7).

## 2. Research: protocols and libraries

| candidate | system model | mapped to this product | verdict |
|---|---|---|---|
| Paxos / Multi-Paxos | crash faults, asynchronous safety, a leader for liveness; acceptors keep promise + acceptance durably | heights are slots; a block is a phase-2 quorum; the leader is a seat | TAKE as the design (already what the dark lock implements) |
| Flexible Paxos (Q1 + Q2 > n) | as Paxos | Q2 = m gives Q1 = n - m + 1; with 2m > n that is ≤ m, so max(Q1, Q2) = m: the flexibility buys nothing any more | take the proof, drop the second quorum size (§3.2) |
| Raft: `openraft` 0.10.1 (MIT OR Apache-2.0), `raft` 0.7.0 / raft-rs (Apache-2.0) | crash faults; majority quorums; a replicated log the library owns; randomized election timeouts with heartbeats | a second log beside the chain; followers accept any leader entry (an acceptance here is a human vote); majority ≠ m for 4-of-5; elections need ticking followers, not seats offline for days | LEAVE the crates. TAKE the doctrine: a member that lost its disk is removed and re-added as a new member, never re-admitted with an empty vote (§4) |
| Viewstamped Replication | as Raft; a recovery protocol where a replica that lost state asks f + 1 others | its recovery relies on the full state being replicated at a quorum; Paxos acceptances live only at the leader | leave; the recovery is the lock's R-c, which §4.2 replaces |
| `omnipaxos` 0.2.4 (Apache-2.0), `paxakos` 0.13 (GPL-3.0-only) | replicated-log libraries with their own storage, election and log | the same mismatch as Raft (`chain_lock_design.md` §1) | leave |
| PBFT, Tendermint, HotStuff: `informalsystems-malachitebft-core-consensus` 0.5.0, `hotstuff_rs` 0.4.0 (Apache-2.0); `tendermint` 0.41.0 is types/RPC, no engine | Byzantine faults, n ≥ 3f + 1, quorums of 2f + 1, partial synchrony with round timeouts | 2-of-3 halts with one seat offline; validators assumed online | leave (Byzantine seats are out of scope) |
| threshold signatures (FROST, BLS) | none for agreement; FROST needs a DKG and a fixed signer set per round | shrinks a block, orders nothing (`chain_issue_claims_check.md` 13-7) | leave |
| `stateright` 0.31.0 (MIT) | explicit-state model checker for actor systems: lossy/duplicating networks, crashes with `Storage` that survives them, BFS/DFS, simulation | can drive the SAME pure state machine the engine drives (§8). Pure Rust (checked in a scratch crate: ahash, dashmap, parking_lot, rand, tiny_http) | TAKE as a dev-dependency of `molt-engine` |
| `toxiproxy` (MIT, external binary), `turmoil` 0.7.2 (MIT) | network fault proxy / tokio network simulation | toxiproxy in front of `dev_relay` for the field protocol; turmoil would replace the loopback hub we already own | take toxiproxy as a tool; leave turmoil |

**Can a library CORE be used with our transport and signatures?** No
candidate's safety rules fit unchanged: every one lets an acceptor accept
any value at a fresh ballot, while here a seat signs only a change its
human or agent approved (`chain_lock_design.md` §1); quorums depend on the
change (m, a restore's survivors, a cut); the log already exists and is
verified offline. The single-decree core is about 250 lines and survived
six reviews (`lock.rs`); every defect those reviews found sat in the
wiring around it (activation mixing, amnesia, restore subjects, evidence).
A library would not remove one of them. So: we keep our own core, and the
checking is not hand-rolled: `stateright` drives that core (§8).

## 3. Design space

### 3.1 The options

- **(B) "One signature per seat per height", durable, plus a deterministic
  choice** (the lowest approved open id). Same views → same vote. Different
  views split: A signs its own card, B its own, C its own (agents that
  approve their own proposals do exactly that). Nothing reaches m and no
  seat may move: a permanent stall (issue #11's toy model: 19 % stalled
  heights). A seat may move from X to Y only when X can no longer be
  chosen, i.e. when it knows that the seats that could still sign X are
  fewer than m. Knowing that means asking m seats and making them promise
  not to sign X any more: that is Paxos phase 1. Moving also means signing
  a second value at h, which is safe only if a signature names its round,
  or the two values pool into two m-sets. So the smallest correct (B) is
  ballots in the signed bytes + phase 1 + the forced-value rule: (A').
- **(C) One sequencer per height**: safe only while the leader never
  changes. A leader change without phase 1 forks exactly like E0 (below);
  with phase 1 it is (A'). There is no failure detector without time (FLP):
  some timer must trigger the takeover. What this design guarantees instead:
  a timer only decides WHEN a seat tries, never WHAT is chosen once a value
  may be chosen (the forced-value rule), and every try is safe. Retries
  back off, never give up (user rule 2026-11-21: no fixed deadlines). No operator
  babysitting and no "skip" vote: a skip vote is a ballot under another
  name.
- **(D) E0** (`chain_lock_wiring_plan.md` §13): one signature per
  (height, round), no phase 1. Forks across rounds (the §13.3 schedule).
  E0 + phase 1 = (A').
- **(E) Finality by depth + branch choice**: forks still happen, applied
  decisions are still displaced, a lasting split still needs a vote. It
  makes forks less harmful; the user's requirement is that they do not
  occur. No depth is final either: the fork rule has no depth bound, only
  a cut is.
- **(A) Strict Order Mode as built**, finished: §12 lists eight unbuilt
  items and seven open decisions; three of them ((a) forced values, (b)/(c)
  amnesia) are the parts that kept breaking.

### 3.2 Comparison

| | safety argument | seats for a decision | seats for a restore (2-of-3) | signed layout | carried risk |
|---|---|---|---|---|---|
| today (B6) | none: a reducer | m | 1 + consent | v3 | forks, displacement, lasting splits |
| (A) as built | Flexible Paxos per slot, honest sealers pool one ballot | max(m, n-m+1) | 2 | unchanged | release rule, amnesia ask (R-c), evidence frames, forced completion by non-approvers |
| (B) strict | quorum intersection (2m > n) | m, but permanent stall on a split | 2 | v4 | stalls |
| (C) without phase 1 | none after a leader change | m | - | v4 | forks at takeovers |
| (D) E0 | intersection within one round only | m | - | v4 | forks across rounds |
| (E) | none | m | 1 + consent | v3 | forks remain, by definition |
| **(A') recommended** | Paxos per height, ballot in the signed bytes, majority quorums, device change = new acceptor | **m** | **2** | **v4 (with N2)** | the prompt in §3.3 step 6; restore cost §7 |

### 3.3 The recommendation (A'): Strict Order Mode, majority-only

One single-decree Paxos instance per height, at heights ≥ H only:

1. **Stance and vote are separate.** `approve` records a stance
   (`own_approvals`, durable, gossiped as an unsigned intent: the
   `Approved` shape with `sig: ""`, as the lock does). A signature exists
   only as an acceptance in a ballot.
2. **Ballot** = (round, leader). Phase 1 is never skipped; it is pre-paid:
   the default leader starts it for head + 1 the moment its head moves.
   Default leader = the lowest-named seat heard (presence: liveness only).
   Takeover by the k-th seat after k × grace with backoff and jitter, as
   built (`chain_lock_design.md` §5).
3. **Quorums.** Q1 = m promises for every value. Q2 = m for a card and a
   cut (Q3), s(n, m) survivors + consent for a restore (§4.3). The mode is
   offered only where 2m > n and m < n.
4. **Free choice.** With nothing forced, the leader proposes the smallest
   open id with ≥ need intents it holds (B6's choice, decliner rule D2).
5. **Acceptor rule.** A seat accepts at ballot b ≥ its promise only a
   change whose digest its OWN record approves, or the value it already
   holds (an acceptance is a promise kept, review S1). The durable record
   is written before the signature is computed (§4.1).
6. **Forced value** = the highest-ballot value reported by the promisers,
   unless `holding + unheard < need` (the escape, `Leader::forced`). A
   forced value no reachable approver can complete stays open, and the
   status offers `decision <id> may already be made - approve to complete
   it` to seats that did not approve it: a human vote, never an automatic
   signature. This is §12 (a)'s candidate; it deletes H1, item 3 and F1.
7. **Seal.** Any seat holding m acceptances of one (h, prev, ballot, value)
   seals; seals by several seats are one block identity.
8. **Signed bytes `molt-chain-change-v4`**, every field length-prefixed:
   `tag ‖ republic_id ‖ height ‖ prev ‖ round ‖ leader ‖ signer_incarnation
   ‖ change`. `ChainBlock.ballot` is a new `#[serde(default)]` field;
   identity v2 excludes ballot and signers. `verify_chain` from H: every
   signature verifies over v4 with the block's ballot and the signer's
   incarnation at h (§4.2), else hard-reject.

What it cuts from the dark lock:

| piece | fate | why |
|---|---|---|
| `Acceptor`, `Leader`, `Tally` (`lock.rs`) | keep, moved into the pure core (§8) | sound in six reviews |
| Q1 = n - m + 1, `Quorum::promises` | replace by m | 2m > n |
| release rule, free marks, `lock_dead`, the straddle | delete | one agreed H, no free signing at ≥ H (§6) |
| amnesia ask/reply (R-c), `lock_unknown` floor | delete | §4.2: a device without its record never votes |
| evidence frames, forced completion by any promiser (H1, item 3, F1, F5) | delete | step 6: only approvers sign |
| lineage hash L(h) | delete | `prev` is in the signed bytes |
| restore subjects, A-F1 refusals | keep the rule "a seat never accepts its own restore"; drop the rest | §4.3 |
| `lock_wire.rs`, `lock_sim.rs`, `lock_explore_tests.rs` | retire after S3 (Q6); wire variants stay decodable | rules mirrored outside the core are the failure class (§8) |

## 4. The promise across crash and device change

### 4.1 What is durable, and when

| step | order |
|---|---|
| promise or accept | update the record → write it blocking and atomically (tmp + fsync + rename, the `persist_lock_blocking` precedent) → only then compute the signature and send |
| write fails | nothing is sent; the seat reports `vote not recorded` |
| hard kill after the write, before the send | safe: the record is a superset of what left |
| hard kill before the write | nothing was signed |
| forgetting | a height's record goes only once the chain ON DISK holds a block at it (review B-F1); there are no reorgs above H, so no reorg window |
| marker | the chain snapshot names the incarnation whose record exists (`vote_record_of`); the record file is never exported |

### 4.2 A device without its record never votes: device change = new incarnation

Adopting the old device's promises from others does not work. The old
device's acceptance at h travels to the ballot's leader, and a promise only
to its leader; the other members of a quorum hold their own signatures, not
Z's. A new device is safe only after hearing from every seat that could
have led: all n - 1. The coordinator's Welcome holds only its own slice.
"Adopt what the reachable seats hold" is therefore safe only with n - 1
answers, which is no better than the restore quorum below (n - 1 ≥ s). If
the old device sent an acceptance nobody received, that acceptance can
never be part of a block, so nothing needs adopting, except a block that
never left the old device. Such a block is chosen, so phase 1 at its height
finds it forced (Q1 meets its m - 1 surviving acceptors).

The rule instead (the Raft doctrine, §2): **an acceptor is (seat,
incarnation); a new incarnation starts with an empty record and is born
only by a sealed `Restored` block.**

- Incarnation id = the seat's working transport anchor (`nostr_pk` of the
  device). It is unique per restore (the anchor register,
  `net/recovery.rs:130-142`), and it survives a cut (`CheckpointState.anchors`).
  The rejoin plan's N2 proposes `incarnation_height` instead
  (`chain/branch_run.rs:228`). That value is computed from `chain.blocks`,
  which a prune cuts back (`governance.rs:1359`), so a pruned verifier
  would compute 0. Using it needs `molt-chain-checkpoint-v11` (Q2).
- Z restored at r: at heights ≤ r the old incarnation is the valid one
  (an old device that still runs has its own record intact, so it is an
  ordinary honest acceptor). From r + 1 on only the new incarnation's v4
  signatures verify. Every acceptor at r + 1 holds block r (its `prev`),
  so it also drops the old device's frames. The re-key removes the leaf
  (rejoin N3 makes the old device stop at once).
- The restored device signs nothing at heights ≤ r, needs nothing from the
  Welcome about signatures, and has no amnesia.

| case | outcome |
|---|---|
| restart after a hard kill | the record is intact (§4.1) |
| restored twice (r1 < r2) | three incarnations, each valid on its own interval; a second link's v2 consent names the replaced anchor, so after r1 it is stale (`consent_stale`) |
| old device still running | ordinary acceptor up to r; invalid above r; evicted by the re-key |
| two restore links in flight | one `Restored(Z)` per height, the other card's consent goes stale |
| import of an export or S3 backup (same incarnation, no record) | passive: reads and catches up, never signs; status `voting record missing - restore this seat with a link` (Q1) |
| record file lost or unreadable, marker present | the same as an import |
| a filesystem backup that brings back an OLD record | not detectable (the lock's residual S6); the operator deletes the record. The product's own paths cannot produce it |

### 4.3 The restore quorum, derived

R(Z) at r has survivor set S (|S| = s) plus Z's consent. It must meet
(a) any value X at r whose m-quorum includes Z's old device: s + (m - 1) >
n - 1; (b) a second R'(Z): 2s > n - 1; (c) the phase-1 set: m + s > n. So
s(n, m) = max(m - 1, n - m + 1, floor((n - 1)/2) + 1). The consent stays
an authorization, never a voice for intersection.

| n, m | 3, 2 | 4, 3 | 5, 3 | 5, 4 | 7, 4 | 7, 5 |
|---|---|---|---|---|---|---|
| survivors today (m - 1 + consent) | 1 | 2 | 2 | 3 | 3 | 4 |
| s(n, m) under the mode | 2 | 2 | 3 | 3 | 4 | 4 |

The +1 hits exactly the minimal odd majorities, which are the common case.

## 5. The other block kinds, and what the rule retires

### 5.1 Per kind

| kind | under the mode |
|---|---|
| genesis | n-of-n, unique by the ritual; a new founding runs the mode from height 1 |
| `Applied` (incl. `set_features`, wallet, vault) | need m; local vault/wallet checks only stop this acceptor; finality is immediate, so issue #12 / rescue Q7 closes for these chains |
| `Restored` | s(n, m) survivors + a v2 consent valid at the height; the subject never accepts its own restore; v1 consent refused (rejoin gap 18) |
| cut | n today (decision A2, 2026-09-06). Inside a slot a need-n value can halt for good: a seat dies after the others accepted, the value is forced, and no restore can take the height. Q3 recommends m, each acceptor co-signing only its own state hash |
| `BranchChosen` | a value like any other; at ≥ H only for §5.4 |
| `Joined` | refused by the verifier, as today |

### 5.2 Fallback sealer and standing votes

The sealing line, the fallback turn and seal pacing are replaced by the
ballot takeover. Standing approvals are no longer re-signed per height:
a stance is a preference that never expires; a vote exists once per
(height, ballot).

### 5.3 Code that goes dark on a mode chain (kept for heights < H and unactivated republics)

`try_reorg`, `tie_break` (beyond duplicate detection), `relink_onto_variant`,
`return_displaced_card`, `return_displaced_restore`, `vault_displaced`,
`rebase_pending_approvals`, `resign_standing_approvals`,
`resend_standing_approvals`, `reserve_signatures`, `sealer_turn`,
`sealed_a_smaller_id`, `park_approval` (head + 2), the fork part of
`heal_stuck`. Each gets a guard `height >= H => unreachable on a mode
chain` with a test.

### 5.4 Detection instead of a fork rule at ≥ H

A second valid block at a height ≥ H means a modified build, an operator
override, or a bug. No tie-break: the seat stops extending, keeps both
blocks as evidence, reports `two decisions at height <h> - chain stopped`
(status, MCP, a wake), and the repair is the branch choice.

## 6. Activation and coexistence

| item | decision proposed |
|---|---|
| key | `strict-order` in the feature set (charter or `set_features`); `chain-lock` stays dark and is never offered; human name "Strict Order Mode" (user decision 2026-11-22) |
| gates at propose and approve | every seat announced the capability `molt-strict-order-v1` (mirror declaration); 2m > n; m < n; `block-v2` active; no restore card open |
| new foundings | on by default, H = 1 (Q4); the wizard shows the restore cost in one row |
| existing republics | H = c + 1, c = the first cut above the enabling block (issue #12's suggestion). A cut needs every seat, so every seat is on the enabling block's lineage when c seals |
| the window between enable and cut | the old rule, plus one change to the fork rule for heights < H: a cut outranks every other block at its height. The cut is then final on every seat: a seat that sealed a card at c re-bases onto the cut instead of splitting from seats already pruned to it |
| the cut-versus-card race | the same rule closes `docs/rejoin_completion_plan.md` gap 5f (a cut and a card sealed at one height); it outranks only with n distinct roster signatures on the cut. A fix without activation (reorg across the cut) was built and rejected: runtime-dependent rollback, a window open until the next n-of-n cut, R2 and wallet I16 broken (rejoin plan §C.4b, Q9) |
| both rules at once | never: below H B6, from H the mode, H a function of a final block |
| an older build | cannot be a seat at activation (the gate); the first record write raises the manifest version, so a downgrade refuses the workspace; a rejoiner without the capability gets no Welcome (A-F2 precedent); an old build that receives a v4 block refuses it and stops extending |
| `block-v2` | prerequisite; v4 replaces v3 from H |
| one layout bump | v4 carries ballot AND incarnation: the rejoin plan's N2 is built here, one vote, one wall |
| off-switch | none. Switching off needs a sealed block; the case that would want it (fewer than m reachable) cannot seal one. Turning it off would also bring back free signing at heights where records exist |

## 7. What the user must accept

| | 2-of-3 | 3-of-5 | 4-of-7 |
|---|---|---|---|
| a decision seals with | 2 reachable (today 2) | 3 (3) | 4 (4) |
| members that may be down | 1 (1) | 2 (2) | 3 (3) |
| bringing a lost device back needs | both others online (today 1) | 3 of 4 (today 2) | 4 of 6 (today 3) |
| two seats lost | halted for good: refound (today the last seat restores both, alone: the field's split) | the other 3 restore both | the other 5 restore both |
| more than n - m lost | halted (today m - 1 survivors can restore) | halted | halted |
| import after a disk loss | passive until restored, like a lost device (today: votes at once) | same | same |
| a seat dies mid-decision | rarely: `decision <id> may already be made - approve to complete it` to the others | same | same |
| the leader goes silent | one takeover, about a minute plus Tor; then the next seat leads | same | same |
| a fork | cannot happen among unmodified builds; a modified one stops the chain (§5.4) | same | same |
| traffic | less: 17-22 frames per block against about 50 (lock, loopback, §10 of the wiring plan) | not measured | not measured |

Today a fork usually heals within seconds to minutes, and each heal can
undo an applied decision (a wiki patch back to the vote; a vault
disclosure that cannot be undone). In the field it ended in three
branches. Under the mode nothing is undone, and the price is a
recognizable wait that names the missing seat.

Status lines (MCP fields carry the same facts):
`waiting for members - 1 of 2, C offline` ·
`restore needs 2 members online - C offline` ·
`voting record missing - restore this seat with a link` ·
`decision 17 may already be made - approve to complete it` ·
`two decisions at height 940 - chain stopped` ·
`needs a newer build: B`. The approve-to-complete control exists in the
window too (co-equality; wiring plan §12 made it a requirement).

## 8. Verification that would have caught the six rounds

### 8.1 What the dark lock's checks covered and missed

| check | covered | missed (its own headers) |
|---|---|---|
| `lock_explore_tests.rs` (exhaustive BFS, one slot) | real `Acceptor`/`Leader`; restores, subjects, forced values | the wire rules are MIRRORED per seat in the test; cuts, more than one slot, lineages, `Committed` reaching others, declines, persistence failures, takeover timing |
| `lock_sim.rs` (seeded, wire-faithful) | loss, intents, withdrawals, persist-before-frame | free signers, the release rule, the downgrade, head moves, pruning, the rejoiner's new device (§12) |
| engine tests | the wiring | only the schedules someone thought of |

Twice a rule existed only in the model. Every later defect sat where the
model did not reach: bringing a seat back, a seat without its state, an
older build, the restore gate.

### 8.2 The structure that prevents it

- **One pure core, two drivers.** `chain/seal_core.rs`: inputs = head move
  (height, prev, incarnations), stance change, frame, `Persisted(token)`,
  monotonic tick; effects = persist, send, seal, status. "Send only after
  persisted" is a core rule. The engine adapter translates and signs and
  decides nothing. A `stateright` `Actor` wraps the same core. Fits
  `docs/engine_state_machines_plan.md` (pure, clock-free modules).
- **Model scope**, red first: n = 3/4/5 with majority m, two heights, two
  cards, three rounds; actions: approve, decline, withdraw, crash (keeps
  `Storage`), device loss + restore (new incarnation), import (same
  incarnation, no record), the old device revived, an older-build seat
  signing v3 below H, the activation cut, loss/duplication/reordering, any
  takeover order. Properties: `always` safety; `always` every seal passes
  the real `verify_next`; liveness on bounded fair runs plus
  `spawn_simulation` seeds. Mutation check: each rule of §3.3/§4 disabled
  in turn must produce a counterexample.
- **Trace replay**: engine runs over the loopback hub record the core's
  inputs; replaying them into a bare core must yield the same effects
  (catches drift between adapter and core).
- **Pure arithmetic tests**: s(n, m), the need per kind and the escape,
  exhaustively for n ≤ 9.
- **Deterministic simulation over the loopback hub**: a seeded
  delay/drop/partition layer plus the existing kill and reopen seams.

### 8.3 Field protocol: reproduce real forks first

1. Baseline on v0.1.3: three `moltd` nodes (2-of-3) and five (3-of-5)
   against `dev_relay`, each seat behind its own toxiproxy: latency 2-30 s
   with jitter, a blackhole of 3-10 min per seat in turn (seats disagree
   on presence past 120 s), an asymmetric A↔C cut; agents propose in
   bursts of 15+ cards and approve each other's.
2. The baseline counts as valid only when it shows `fork_decision` lines
   with two different proposal ids. If it shows none after 1 000 cards,
   sharpen the schedule; without a red baseline the change proves nothing.
3. The same schedule on the mode: 0 forks, no `heal_stuck`, propose-to-seal
   p50/p95 recorded; then a restore, an import and a kill at every step.
4. Copies of a real republic (exported into scratch dirs outside the tree,
   counts only), then over Tor for the traffic figure.

## 9. Staging

Prerequisites from `docs/rejoin_completion_plan.md`, before S3: C.1
(durable re-key: a restore now needs more survivors and must not be lost
to a crash), N3 (the old device stops), C.4 (cuts seal after a restore: an
existing republic activates only through a cut), gap 18 (v2 consent). N2
is built here (S2). N1 is not a safety mechanism (§4.2); it may stay as a
display. Gap 17 (two restores at once) is answered by §4.2.

| step | content (red test first) | after | parallel with | agent-h |
|---|---|---|---|---|
| S0 | field harness: toxiproxy recipe, loopback delay layer, fork counter; red baseline on v0.1.3 | - | S1, S2 | 14 |
| S1 | `seal_core.rs` from `lock.rs` + `stateright` model, mutations, arithmetic tests | - | S0, S2 | 24 |
| S2 | `molt-chain-change-v4`, `ChainBlock.ballot`, incarnation from the anchors, `verify_chain` dual path, byte pins, manifest wall | - | S0, S1 | 16 |
| S3 | engine adapter: wire frames, durable record, intents, takeover, seal, trace replay | S1, S2, rejoin C.1/N3 | S0 | 28 |
| S4 | restore s(n, m), incarnation start, import passive, status lines | S3 | S5's UI part | 16 |
| S5 | activation: key, gates, cut-anchored H, the cut precedence below H, founding default, modal + MCP data, approve-to-complete control (one window build) | S4, rejoin C.4 | - | 22 |
| S6 | §5.3 guards, §5.4 stop with evidence | S3 | S4 | 8 |
| S7 | field acceptance (§8.3 steps 3-4), Tor traffic | S5, S6 | - | 12 |
| reviews | six independent gates (S1, S2, S3, S4, S5, a final adversarial pass on activation, restore, import and older builds); each finding gets a test | each step | - | 24 |

Total about 164 agent-hours. Serial lane S1 → S3 → S4 → S5 → S7 with
reviews: about 10-13 working days after the rejoin plan's prerequisites.
The chain core (S1-S4) is serial; S0, S2 and S6 run beside it. Every merge
in this area waits for its review gate; a reviewer gets the code and the
properties of §1.3, not this plan's conclusions.

## 10. Questions for the user

| # | question | recommendation | cost either way |
|---|---|---|---|
| Q1 | an imported device (export or S3 backup): passive until a restore vote, or the lock's R-c ask (answers from m seats set a floor) | passive + restore | passive: a 2-of-3 import needs both others online. R-c: the most-broken part of six reviews comes back |
| Q2 | the incarnation id in v4: the working anchor (this plan) or `incarnation_height` (rejoin N2) | the anchor | anchor: loopback tests must mint non-empty anchors. Height: `molt-chain-checkpoint-v11` and every recompute site |
| Q3 | a cut under the mode: m (own state hash only) or n (decision A2) | m | m: a silent seat no longer vetoes a cut and catches up by the blob. n: one seat lost mid-cut halts the republic for good |
| Q4 | new foundings: the mode on by default | on | on: 2-of-3 needs both others for a restore. Off: new republics keep forking |
| Q5 | a restore with fewer than s survivors: no exit, or an operator "the other devices are gone" acknowledgement | no exit in this release | none: two lost seats in 2-of-3 mean refounding. Acknowledgement: a false one recreates the field's three branches |
| Q6 | the dark lock code (`lock_wire.rs`, `lock_sim.rs`, explorer) | delete after S3 is green; wire variants stay decodable | keeping it: two engines to review |
| Q7 | a timer may trigger a leader takeover (retry with backoff, never a give-up, never a choice between values) | yes | no: an operator action per silent leader |
| Q8 | republics with m = n or 2m ≤ n: no mode; they keep the fork rule and the branch choice notice | yes | offering it there: a seat lost mid-height halts for good (m = n), or two disjoint quorums fork (2m ≤ n) |
