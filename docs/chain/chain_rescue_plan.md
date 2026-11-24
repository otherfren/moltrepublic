# Chain rescue: m members choose a branch

**Status: PLAN, not started; release v0.1.3 (user decision 2026-11-23).**
When m members notice a split that does not heal and agree to set the
chain back, they VOTE on which branch to keep. That vote is the next
block on the chosen branch. The other n - m seats rejoin explicitly, by
link. Every step is an operator action over MCP (and the co-equal GUI);
nothing switches by itself. Strict Order Mode stays postponed
(`docs/chain/chain_lock_wiring_plan.md` §12-§13).

Evidence of the failure this rescues: `docs_archive/chain/rejoin_divergence_findings.md`.
The first republic ended with three seats on three branches, because
every seat coordinated a restore alone (N1), and the MLS group split. Its
counts and shapes are the keystone below.

## The majority rule (decided 2026-11-23)

**Rule: a republic's threshold must satisfy m > floor(n/2), i.e. 2m > n.**
3-of-5 is allowed, 2-of-5 is not.

Why:
- With 2m ≤ n, two disjoint groups can each reach the threshold: at one height, or in one branch choice, with no software fault at all. No repair can then say which half is the republic.
- With 2m > n, any two deciding groups share at least one seat. That shared seat is what makes a branch choice unique (§2).

| applies to | what happens | code |
|---|---|---|
| (a) the branch choice | offered only where 2m > n. Refused at propose and approve with `needs a majority of the members: <m> of <n> is not one` | `choose_branch`, `approve` (§8) |
| (b) new foundings | refuse a threshold with 2m ≤ n (Q5: refuse vs warn). The wizard says it in one line: `the threshold must be more than half of the members` | engine: `lifecycles.rs` `cmd_create_start` (the `threshold < 2` gate, about line 805-809). Wizard: `crates/molt-ui-window/ui/app.slint`, the threshold `AppStepper` (`minimum: 2` -> `root.cw-rule-default`, already `floor(n/2)+1`) and the `changed` snap on the members stepper. MCP: the `create_start` schema and description (`molt-mcp/src/lib.rs`, "2..=members", "at least 2"). Optional: a joiner refuses a non-majority charter in `founding.rs` `verify_seal_proposal` (sign-what-you-see against an older founder) |
| (c) existing republics with 2m ≤ n | keep running unchanged; NO branch choice; a one-line notice in status and Organization: `this republic's threshold is not a majority: a split cannot be repaired by vote` | status: `chain_sync` notice field. NOT in `verify_chain`: the rule is a founding and feature gate. A live republic's chain must keep verifying, the same reason the m ≥ 2 gate sits at founding, not in the verifier (comment at the gate) |
| (d) m = n | allowed (it is a majority). Its own cost: any lost seat stalls every decision except its restore | unchanged |

- **Changing the threshold:** impossible today. `rule_m` comes from the genesis and is copied unchanged from block to block (`chain/verify.rs` `verify_next`, `rule_m: head.rule_m`), and seats are fixed at founding (`verify.rs`, `joined` hard-rejected). A 2m ≤ n republic's way out is to found again.
- **Consequence for the vault:** the vault needs m ≤ n - 2 (`molt-engine/src/vault/mod.rs`). Together with 2m > n it is only possible from n = 5 (3-of-5). A new republic of 3 or 4 seats can no longer enable the vault. The wallet's m < n stays possible from n = 3.

## 0. What changed against the earlier plan

| earlier plan (F-X1) | now | why |
|---|---|---|
| a rescue block sealed by one survivor plus the rejoiner's consent | a `BranchChosen` block sealed by m members like any vote | the threshold decides, not one survivor; the consent path is what split the group (N1) |
| always cut back to the last common block | choose ANY tip on one branch at or above the fork point (the common block itself included) | members may keep the work of the branch they trust |
| a fresh MLS group formed by the coordinator | the existing restore machinery: one coordinator's group stays, every other seat rejoins by link (§7) | reuse; a new group ritual is the riskiest part, avoided |
| a seat that missed the rescue may re-offer its old branch | a seat that sees a valid choice for another branch STOPS (§6) | no automatism, and no silent fork |
| kept | an additive `ChainChange` variant; never below a cut; displaced `Restored` blocks dropped; an explicit decision only | |

The existing automatic fork rule (`chain/sync.rs` `tie_break`,
`try_reorg`, `relink_onto_variant`; `persistent_chain.md` §7) keeps
running unchanged for forks that heal by themselves. This plan is for the
ones that do not: `status.chain_sync.heal_stuck` set, or seats that
cannot read each other at all.

## 1. The object: `ChainChange::BranchChosen`

```
BranchChosen {
    base_height: u64,          // the last block both branches share
    base_hash: String,         // its link hash
    tip_height: u64,           // the chosen tip; == block height - 1
    tip_hash: String,          // the chosen tip's link hash; == block.prev
    chosen_root: String,       // chosen branch's block at base_height + 1 ("" when tip == base)
    dropped: Vec<HeightHash>,  // per abandoned branch: its block at base_height + 1 and its head, as the proposer saw them
}
```

| question | answer | code |
|---|---|---|
| what is signed | `approval_bytes(republic_id, height = tip_height + 1, change)`, tag `molt-chain-change-v2`, new variant byte 6, every field length-prefixed | `molt-core/src/chain.rs` `approval_bytes` (tags 1-5 used) |
| why `tip_hash` is in the change | member signatures cover `(republic_id, height, change)`, not `prev` (CLAUDE.md, "Signatures are position-bound… `prev` is a structural hash-link"). Without `tip_hash`, a signature at height h would be valid on EVERY branch with h - 1 blocks. With it, the verifier's `block.prev == change.tip_hash` ties the signed bytes to exactly one branch | `chain/verify.rs` `verify_next`: a new arm beside the cut arm (`upto == height - 1` is the precedent) |
| verify | a normal m-of-n block (`block_signers`, threshold `rule_m`) plus three structural checks: `tip_height == height - 1`, `tip_hash == prev`, `base_height < tip_height` or `tip == base`, and `base_height ≥` this walk's anchor | `verify.rs` `verify_next`, `ChainWalk::step`, `fold_one` (no projection change) |
| an older build | the `Committed` envelope fails to parse, giving WARN `discarding an undecodable event frame` and `Discard` (`molt-net/src/supervisor.rs`, envelope parse). The seat never applies it and so never extends past it (additive rule, `chain.rs:63`). It shows the chosen branch's seats as ahead (`status.chain_sync.ahead_*`) and its own votes are refused as lagging. It cannot rejoin until updated: a rejoiner verifies the whole chain | |
| delivery cost on old peers | an old peer never acks the block, so the sender's tail owes it (rewind rounds, loud after 8); the same as any earlier new variant | `docs_archive/transport/delivery_guarantee.md` |
| projection | none: the block decides which blocks count, not what they change | `chain/projection.rs` untouched |

## 2. Uniqueness: one choice per seat and fork point

Two m-sets always share a seat only when 2m > n, which is the majority rule.

| (n, m) | 2m > n | two choices at one fork point |
|---|---|---|
| 2-of-3, 3-of-4, 3-of-5, 4-of-5 | yes | impossible while the seats in the intersection each sign once |
| 2-of-4, 2-of-5, 3-of-6 | no | refused: there, two disjoint groups could each choose, so a choice would decide nothing |

**The record.** `chain.branch_choices: BTreeMap<base_height, (tip_hash, chosen_root)>`,
part of the chain snapshot, written BLOCKING before the `Approved` frame
is recorded (the precedent is `chain_lock.state`, written before its
frame). A seat signs a `BranchChosen` at `base_height` only when there
is no record, or the record names the same `tip_hash`. Code:
`chain/governance.rs` (new `sign_branch_choice`), `events.rs` snapshot.

**The amnesia problem, smallest form.** A restored or imported device
holds the chain, not the old device's unsealed signatures. Proposed
rule: sign only if the own chain holds no `BranchChosen` at that
`base_height`. Not enough. Schedule, 2-of-3, fork at f:

| step | A | B (shared) | C |
|---|---|---|---|
| 1 | proposes "X" at f | signs X (record written on B's old device) | |
| 2 | collects A + B, seals X | device lost before X reaches it | |
| 3 | | restored by a link from C (C's branch, no X on it) | coordinates the restore |
| 4 | | new device: no X in its chain, no record | proposes "Y" at f |
| 5 | | signs Y | seals Y: two choices at f |

Handled without a protocol:

1. A device whose own incarnation began ABOVE f refuses a choice at f by default (code `amnesiac`). Its incarnation height is the height of its `Restored` block, or its import anchor.
2. The operator may override with `choose_branch`/`approve` argument `amnesia_ack: true`. The card then shows `B signs on a device restored above the fork point`.
3. The restore in step 3 is itself refused on master while C sees the divergence (`restore_gate` -> `chain diverged`, `chain/membership.rs`). The schedule therefore needs C to be blind to the fork, i.e. a split group.

This only narrows the risk; it is the hardest open point (§11). The
majority rule does not close it: the rule guarantees a shared seat, not
that the shared seat remembers.

## 3. How far back

| rule | decision | reason |
|---|---|---|
| base = the last block both branches share, as THIS seat verifies it | required: the proposer holds the other branch's blocks from `base_height + 1` (§5) with m valid signatures that link at `base_hash` | a choice needs evidence of a second branch |
| below a checkpoint cut | refused, rule `below_cut` | a cut seals n-of-n (`chain/governance.rs`, cut `need = identities.len()`), and a seat co-signs only a cut of its own projection, so no honest cut lies above a fork point. A seat whose anchor is above `base_height` cannot verify the base, so it refuses. If no m seats can: not rescuable by a vote; found again |
| tip below the chosen branch's head | allowed | the blocks above the tip are listed as dropped like the other branch's |
| reset to an earlier height on one's own branch with no second branch | NOT allowed (Q1) | that is the threshold rewriting agreed history; a mistaken decision is undone by a new decision |

## 4. What is dropped

The node computes the list from its own chain plus the evidence. The card
shows one row per dropped block: `height, surface/op, short id, effect,
irreversible?`. Irreversible rows are flagged and counted in the card
header.

| surface | dropping means | irreversible |
|---|---|---|
| wiki / memory | the patch is no longer applied. Its card returns as an open proposal on seats that hold it, with stances cleared (Q3); authors see `back to the vote: chain choice at f` | no |
| quests / kanban | the same as wiki | no (a wake already fired stays fired) |
| organization `set_relays` | the pool reverts to the chosen branch's | no (the relays may hold frames) |
| organization `set_features` | the feature is off again on the chosen branch, against "enable-only" | flagged: a vault or wallet enabled only there loses its base |
| organization name/agenda/images | revert | no |
| membership `Restored` (the MAIN case) | the restored device is out again; its working anchor reverts; it must rejoin by link (§6) | no, but its re-key stays in the MLS group of that branch |
| files `persist` / delete votes | a share returns to its previous state; bytes already mirrored stay on disk until the share expires | partly (copies made) |
| vault deposit | the deposit record is gone; seats keep their shares on disk | flagged |
| vault grant (a disclosure) | the reader already holds the secret | IRREVERSIBLE |
| wallet init / DKG | the purse leaves the chosen chain's view (its key records stay on each seat until that block falls below a cut); its address may hold funds | IRREVERSIBLE (funds outside the chosen chain's view) |
| wallet stage / recorded outputs | Stage 1 is receive and watch only: outputs exist on the Monero chain whatever this chain says; the chosen branch's purse rescans | flagged (the record, not the money) |
| checkpoint cut | cannot occur (§3) | |

Code: `chain/sync.rs` `return_displaced_card` and `vault_displaced` are the
re-base precedent (R4); the new path reuses them, with stances cleared.

## 5. The channel

In a pure chain fork the MLS group still works: the card, the evidence
(served blocks) and the votes travel like any proposal. In a split group
(each branch re-keyed the others out, findings N1/E1) nobody reads anybody.

| option | how | fits "no automatism" | weak point |
|---|---|---|---|
| (a) ritual channel | gift-wrap kind 1059, as the recovery ritual, addressed to the seats' transport anchors | partly | after mutual restores each branch knows only its own working anchors, so messages address keys of abandoned devices (findings §6, self-heal) |
| (b) bundle file | `branch_export { path, passphrase }` writes this seat's blocks from a height plus any card and signatures; `branch_import { path, passphrase }` reads one. Sealed like `export_workspace` (Argon2id + XChaCha20-Poly1305, `molt-storage/src/export.rs`). The operators move it over any side channel | fully: nothing is sent unless a human moves a file | manual; blocks carry republic content, hence the passphrase |
| (c) both | | | |

Recommendation: (b) first, and it is the first shippable step (S1). A
pure fork needs no file: the in-group path works. (a) later, only if the
field asks for it (Q2).

## 6. The other n - m seats

| case | what the seat does | code |
|---|---|---|
| sees a valid `BranchChosen` (m signatures over the roster's identity keys, which a `Restored` block never moves, so a standalone check works) whose `chosen_root` is not its own block at `base_height + 1` | enters `abandoned { base_height, by }`: no seal, no signature, no propose, no cut, no restore coordination; chat stays local | `sign_blocker` (new reason), `chain/sync.rs` `receive_block_at` (a contender carrying `BranchChosen` is checked before `stash_fork_candidate`), `cannot_reach_group`-like gate |
| status | `branch abandoned at <f> by <A, B> - rejoin with a link` (`status.chain_sync.abandoned`); one notice, one chat line, one wake | `chain/sync.rs` `refresh_sync_feedback` |
| refused commands | `approve`, `decline`, `propose`, `withdraw`, `propose_checkpoint`, `recover_invite_start` -> `branch abandoned` | `proposals.rs`, `membership.rs` |
| coming back | a member on the chosen branch mints a link (`recover_invite_start`); the abandoned seat runs `recover_start` on a fresh workspace; anchor-first, consent, a `Restored` block (m-1 survivors + consent), re-key, Welcome (`recovery_ritual.md` §4). Its fresh state is a new incarnation: the re-key merge resets every peer's accept window for it (`delivery_guarantee.md` §11), which also clears the stale windows a divergence leaves | existing; the `restore_gate` gates `seat still active` and `chain diverged` must pass for a seat the chain names abandoned |
| the old workspace | stays on disk, read-only, exportable | |
| refuses and continues | n - m < m under the majority rule, so it can seal nothing | |

## 7. The MLS group after the choice

- The choosers may sit in different groups. One chooser on the chosen branch whose group is live is the COORDINATOR, the proposer by default.
- Every other seat, choosers included, rejoins its group by link: a batch of ordinary restores. Each restore is `restore_member` (remove + add in one commit, `molt-net/src/mls.rs`): the coordinator's group still has a leaf for every seat, held by a device elsewhere, so no new group is needed.
- Missing pieces:
  1. Restore signatures from choosers who cannot read the coordinator's group yet. The `Restored` card needs m: in 2-of-3 one survivor plus consent is enough; in 3-of-5 a second chooser must sign it outside the group, via the same bundle (§5).
  2. A multi-member restore commit (optional; sequential restores work).
  3. The `restore_gate` exemption of §6.
- No fresh-group ritual: the restore path is reused.

## 8. The surface

| tool (MCP) | GUI | does |
|---|---|---|
| `chain_branches` | Chain › Branches table | per seat: last reported head + hash (`seats[].head_height/head_hash`), `diverged`, the fork point this node can PROVE, blocks only on each side, choices seen |
| `branch_export`, `branch_import` | Chain › Export/Import evidence | §5 (b) |
| `choose_branch { base_height, tip_hash, amnesia_ack? }` | "Keep this branch" on a row | proposes the card; the reply carries the dropped list and its digest; refused where 2m ≤ n (`needs a majority of the members: <m> of <n> is not one`), without evidence (`no second branch`), below a cut, `amnesiac`, or when a signer-to-be runs a build without the capability `molt-branch-choice-v1` |
| `approve { id, dropped_digest }`, `decline` | the card | `dropped_digest` must match the node's own computation: sign what you see |
| result | the card turns applied; status shows the new head | |

- Authorization: seat scope. Agents are seats (`docs_archive/kanban/kanban_workflows.md`), so the rule is POLICY: `choose_branch`, `branch_export` and `branch_import` go on the wake skill's `DENY_TOOLS` (`crates/molt-mcp/src/wake_skill.md` and the DE copy). The wake prompt's voting step skips branch cards (Q4).
- Co-equality: every `Command` above is a tool (`co_equality_every_command_is_a_tool_or_documented_internal`).
- Texts: one fact each, no em dash.
- The card's internal path: a new additive `WorkspaceEvent::BranchChoiceProposed { id, change }`, put on `unowed_on_the_wire` (older builds drop unknown variants with a WARN and never ack) and re-served like open cards.
- Signatures: `Approved` at the fixed height `tip_height + 1`, collected in `chain.branch_sigs` and exempt from the head+1 bounds of `receive_approval_via` and the re-base sweep (`chain/governance.rs`).
- Sealing: a seat holding the chosen branch up to the tip seals, truncating its own blocks above the tip.

## 9. Safety argument

- **An honest majority can:** keep one branch, drop the rest, and readmit the others by link. Everything dropped is listed before signing.
- **An honest majority cannot:**
  - rewrite history without a second branch (§3);
  - go below a cut;
  - forge a seat (identity keys are fixed; a rejoin needs the phrase);
  - undo a disclosure or a payment (flagged).
- **A single faulty seat:**
  - cannot choose: it needs m - 1 others;
  - can withhold its signature, but only blocks the choice when it is needed for m;
  - can sign two choices at one fork point only if it is dishonest. Under the majority rule that is the shared-seat failure the lock design also cannot exclude without honest software (`chain_lock_wiring_plan.md` §12 (f)).
- **What the majority rule buys:**

  | property | with 2m > n |
  |---|---|
  | uniqueness of the branch choice | two choices at one fork point need two m-sets, and they intersect in at least 2m - n ≥ 1 seats; if each of those signs at most once per fork point, only one choice can seal |
  | the shared seat | exactly the seats in the intersection carry the guarantee; each must sign at most once per fork point (the durable record, §2). A device restored during a split can forget, and the rule does not change that: the hardest open point stays |
  | Strict Order Mode and E0 (pointer only) | both get simpler under the same rule: one quorum size m for both phases, no reduced fault tolerance, E0 safe within a round. Their hard problem, durable promises across crashes and device changes, does not go away (`chain_lock_wiring_plan.md` §12-§13) |
- **Replay:** a `BranchChosen` cannot be replayed at another fork point (`base_height`, `base_hash` and the height are signed) or on another branch (`tip_hash` is signed and must equal `prev`). Replaying the same block on its own branch is a duplicate (`tie_break` identity check).
- **The automatic fork rule:**
  - On every seat whose chain holds the choice, it outranks the hash tie-break for every fork at or below its height: a contender lacking it loses whatever its hash (`try_reorg`, `tie_break`: new log `fork=chosen`).
  - On every seat whose branch it abandons, it is a STOP, never an automatic re-base: the seat rejoins by link.
- **Checkpoints:** none above a fork point (§3). A cut after the choice is ordinary.
- **Strict Order Mode:** dark (`CHAIN_LOCK_AVAILABLE = false`); no interaction. If it ever ships, a `BranchChosen` at a locked height is a value like any other.

## 10. Tests and staging

| keystone (red first) | expectation | counter-test |
|---|---|---|
| the first republic's shape: 2-of-3, fork at f, mutual restores, three branches, split group; A and B export/import evidence, choose A's branch, C rejoins by link | one head on all three, a vote seals afterwards, every seat's `diverged` empty | without the stop rule C keeps sealing restores: red |
| a pure fork that does not heal (no common readable path for the tie-break, group intact) | choice over the group, the loser stops, rejoins | |
| the choice where 2m ≤ n (a 2-of-5 chain) | `choose_branch` and `approve` refused: `needs a majority of the members: 2 of 5 is not one` | the same at 3-of-5: allowed |
| a founding with 2 of 5 | refused (engine and MCP); the wizard's stepper cannot go below 3 | 3 of 5 founds |
| an existing 2-of-5 republic founded by an older build | still verifies, shows the notice, refused the branch choice | |
| a dropped `Restored` block | the restored device is `abandoned`, rejoins, its window reset by the merge | |
| a choice below a cut, or with no second branch | refused `below_cut` / `no second branch` | |
| an older build | stops at the block (undecodable), no state change | |
| amnesia schedule §2 | the restored B refuses (`amnesiac`) | with `amnesia_ack` it signs |
| replay of a choice at another base or branch | `verify_chain` refuses | |

| step | content | ships alone | agent-h |
|---|---|---|---|
| S1 | the majority rule: founding gate (engine, MCP schema, wizard stepper and line) + the status/Organization notice; `chain_branches` read; `branch_export`/`branch_import` with evidence verification (the other branch links at the base with m signatures); no chain state change | yes: diagnosis + the gate | 17 |
| S2 | `BranchChosen` variant, verifier, byte pin, uniqueness record, card + dropped list + digest, signature collection, seal and adoption on the choosers, tie-break precedence | yes, MCP only | 24 |
| S3 | `abandoned` state and its gates, `restore_gate` exemption, rejoin path, GUI (Branches table, card, abandoned banner; one full window build), wake skill lists | yes | 18 |

## 11. Questions and decisions (user, 2026-11-24)

The user accepted every recommendation below; Q7 stays open until the
fork count after block identity v2 is known.

| # | question | recommendation |
|---|---|---|
| Q1 | reset to an earlier height without a second branch? | no: a decision is undone by a new decision |
| Q2 | the channel | (b) bundle file first (S1); (a) gift-wrap only if asked |
| Q3 | dropped wiki/quests cards: back as proposals? | yes, as open cards with stances cleared: each seat decides again |
| Q4 | may a wake agent vote on a branch choice? | no: `choose_branch` on the deny list, branch cards skipped by the wake prompt; approve needs the dropped-list digest, which also covers the irreversible rows (§4) |
| Q5 | a founding with 2m ≤ n: refuse or warn? | refuse: a warning leaves a republic whose split no vote can repair; note that it also ends the vault for n ≤ 4 |
| Q6 | retire Strict Order Mode formally? | not yet: keep it dark until this ships and the field shows whether forks still need prevention |
| Q7 | no finality (issue #12): a late block with a lower hash displaces applied decisions down to the last cut. Delay irreversible effects until k blocks sit on top, until the next cut, or not at all? See §11a | decide after `block_identity_plan.md` ships and the fork counter shows how many real forks remain; if any: delay the vault answer by depth, k a charter value, default 1; the wallet keeps "final = below a cut"; never a clock |
| Q8 | bind `prev` into the signed bytes (issue #11; `molt-chain-change-v3`)? See §11b | DECIDED 2026-11-24: yes, in the same activation as block identity v2 (`block_identity_plan.md` §3b), once the wait check there passes. A later, separate activation would cost a second vote of every seat and a second downgrade wall |

### 11a. Finality: what is irreversible today (`chain_issue_claims_check.md` 12-2..12-5)

| effect | fires | displaced by a reorg | irreversible |
|---|---|---|---|
| vault grant: each holder sends its share to the reader | when the grant block APPLIES (`vault/grant.rs` `on_commit`, `vault_answer`) | an audit line `vault: grant displaced` (`on_displaced`) | yes, at depth 0 |
| wallet init: the off-chain DKG starts | when the init block applies (`wallet_run.rs` `after_wallet_applied`) | key records kept until the block is final, i.e. below a cut (`wallet_prune_final`) | key material exists; money sent to a purse address |
| wallet spend | none in Stage 1 (`wallet.rs`: Stage 1 cannot spend) | | |
| kanban wake | when a quests block applies (`wake_trigger`) | | a fired wake |
| files persist | mirroring starts on apply | | copies on disk |

Reorg depth: down to this seat's anchor, the last sealed cut (every
seat prunes to it, `governance.rs`). Cuts need every seat, so a silent
seat leaves the depth unbounded.

| option | effect | cost |
|---|---|---|
| none | today | a reorg can follow a disclosure |
| delay by depth k (charter value) | an effect fires when its block has k blocks on top | a quiet republic waits for the next block of any kind |
| until the next cut | the wallet's rule today | cuts need every seat, so a silent seat stalls the effect for good |

### 11b. Binding `prev` into the signature (#11)

| | |
|---|---|
| buys, once block identity v2 exists | a signature is valid on one branch only: no cross-branch reuse |
| does NOT buy | protection against two changes on the SAME predecessor, the field shape (`chain_issue_claims_check.md` 11-4, 11-9). Only one vote per slot does that (Strict Order Mode or E0) |
| costs, the real one | signatures of seats on different predecessors no longer combine: nothing seals until m seats share one `prev`. Seconds while the fork rule settles; during a lasting split the vote stands still until the branch is chosen |
| costs, minor | every re-base re-signs (already so); a relink must re-sign instead of re-link (dead after v2 anyway); a signature format change: `molt-chain-change-v3`, activated like v2 and possibly at the same height |
| branch choice | none: `BranchChosen.tip_hash` already binds its branch |
| standing votes | re-signed per head move as today |
| Strict Order Mode | signs digests on a lineage, which is already prev-like; no conflict |

Hardest unresolved point: the shared seat's single choice across a
device restore during a split (§2, amnesia). The default refusal narrows
it, but only a protocol (or honest humans) closes it.
