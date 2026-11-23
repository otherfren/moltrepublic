# Issue claims against the code (#10 comment, #11, #12, #13)

**Status: ANALYSIS for the v0.1.3 plans; no code.** Each claim in the
public issues #11-#13 and the new comment on #10 is checked against
local master. The issues are static reads of v0.1.2 plus a toy model,
"not verified in running code". Line numbers below are master's; the
issues cite an older commit. Plans affected: `block_identity_plan.md`
(BI), `chain_rescue_plan.md` (CR), `chain_lock_wiring_plan.md` §12-§13
(LK).

## A. Claims

| id | claim | verdict | evidence (master) | consequence |
|---|---|---|---|---|
| 10c-1 | the link bytes hash the sorted `(member, sig)` list | CONFIRMED | `molt-core/src/chain.rs:822-838` | BI |
| 10c-2 | the verifier only needs ≥ m valid signers | CONFIRMED | `chain/verify.rs:477-490` (`block_signers`, threshold at :485) | BI: any m-subset is valid |
| 10c-3 | the sealer takes the m lowest-named valid signers IT holds | CONFIRMED | `chain/governance.rs:695-747` (sort :707, truncate :747) | BI |
| 10c-4 | seats with different signature views seal one decision under different ids | CONFIRMED | two seats can both be first in their OWN sealing line (`governance.rs:786-803`), or a fallback seals after the grace (`:71-75`, `:810-850`) | BI |
| 10c-5 | toy model: 3.7 % id forks with signers in the id, 0 % without; grows with n | NOT CHECKABLE STATICALLY | §B | BI: measure with the keystone |
| 10c-6 | checkpoints, anchors or exports may embed the sig-inclusive hash | PARTLY | `CheckpointState` holds no hash or signature (`chain.rs:517-585`); `applied_sigs` is display only (`projection.rs:371`). Only LINKS carry it: a cut's own `prev`, the anchor's identity (`verify.rs:1319`), test chains in `recovery.rs:1594/1641`. `RecoverAnchor` ships events, not hashes (`molt-net/src/invite.rs:657`) | BI §3: the boundary rule covers it |
| 10c-7 | `existing == &block` is struct equality and must become identity equality | CONFIRMED, and wider | `sync.rs:986`; `tie_break` also hashes `block_link_bytes` directly (`:998-999`), bypassing `block_hash` | BI §3a: every site |
| 10c-8 | activation needs an all-seats-capable gate so old builds stop | CONFIRMED | `caps` in the mirror declaration (`net/mirror.rs:722`); the same precondition as the lock (`chain_lock_wiring_plan.md` §11) | BI §4 |
| 11-1 | `approval_bytes` covers `republic_id ‖ height ‖ change`, not `prev` | CONFIRMED | `chain.rs:215-340` | all |
| 11-2 | a signature is valid on every branch at that height | CONFIRMED | that is what `relink_onto_variant` uses (`sync.rs:805-835`) | CR Q8 |
| 11-3 | `rebase_pending_approvals` re-signs EVERY open approval at each head+1 | PARTLY | it re-signs this seat's STANDING approvals only (`own_approvals`), once per (card, height): `rebase_pending_approvals` `governance.rs:1365-1410` -> `resign_standing_approvals` (`:340`). v0.1.2 added `resend_standing_approvals` (`:357`) after a heal, and `return_displaced_card` (`sync.rs:963`) re-enters a displaced card's stance. A card the seat never approved is never signed | none: same effect for approved cards |
| 11-4 | one seat can contribute to m-quorums for two different changes at one height | CONFIRMED | schedule: B approved p1 and p2, so it signs both at h (`chain_sign_and_gossip_approval`, `governance.rs:402`). A holds {A, B} on p1 and seals p1; C holds {B, C} on p2 and has not seen p1 sealable, so it seals p2. Nothing stops it: `sealed_a_smaller_id` (`:904`) only sees signatures held locally. Field: findings §2, the shared seat signed two blocks at one height | LK, CR |
| 11-5 | so quorum intersection gives no protection | CONFIRMED | follows from 11-4: the intersecting seat signed both | LK: one vote per slot needs the lock or E0 |
| 11-6 | k open proposals give k sealable candidates per height | CONFIRMED | for every card the seat approved | |
| 11-7 | "smallest sealable id first" works only when seats hold the same signatures | CONFIRMED | `governance.rs:904-930` filters `pending_sigs` held here | |
| 11-8 | toy table: 0.4 % / 0 % / 0 % value forks; 0.1 % / 19 % / 3.6 % stalls | NOT CHECKABLE STATICALLY | §B | |
| 11-9 | bind `prev` in a v3 layout; re-signing already happens after every re-base | PARTLY | re-bases re-sign (`try_reorg`, `rebase_pending_approvals`); a RELINK does not, it relies on `prev` being unsigned (`sync.rs:805-835`). With prev bound, the 11-4 schedule still forks: both blocks sit on the SAME `prev` | CR Q8 |
| 12-1 | the fork choice is "smaller hash wins" | CONFIRMED | `try_reorg` `sync.rs:720-724`; tip rule `:998-1051` | BI, CR |
| 12-2 | a late lower-hash block displaces everything down to the n-of-n anchor | CONFIRMED, bounded | floor = this holder's `chain[0]` (`sync.rs:663-670`). Every seat prunes to a sealed cut (`governance.rs:1309`). Cuts are proposed automatically from 32 blocks (`checkpoint.rs:19, 46`) but need every seat (`governance.rs:739-743`). Depth = blocks since the last cut; unbounded to the genesis while a seat is silent | CR Q7 |
| 12-3 | displaced `Applied` proposals go back to the vote | PARTLY | back to `Proposed` with the seat's stance only when the card has a proposer here; a materialized record is removed (`sync.rs:963-977`) | CR §4 |
| 12-4 | vault effects are undone via `vault_displaced` | PARTLY | `vault_displaced` (`vault/deposit.rs:384`) only logs a displaced grant (`vault/grant.rs:456-476`, WARN `vault: grant displaced`, kept for audit). The share was SENT to the reader when the grant block applied (`grant.rs:418-453`, `vault_answer`), so the disclosure is irreversible at depth 0 | CR Q7 |
| 12-5 | treasury decisions can be undone | PARTLY | Stage 1 cannot spend (`wallet.rs:282`). A displaced purse keeps every key record until its block is FINAL, i.e. below a cut (`wallet_run_tests.rs:595-630`, `wallet_prune_final`). Irreversible: the off-chain DKG key material, and money sent to a purse address | CR §4, Q7 |
| 12-6 | the winner depends on signature bytes | CONFIRMED today | via 10c-1. After BI: on (height, prev, approval bytes), i.e. on the decision, still not on who sealed first | BI |
| 12-7 | nothing is final | CONFIRMED above the last cut | the wallet already treats "below a cut" as final | CR Q7 |
| 12-8 | the reorg doc's R2 (cited under `docs/chain/`) | CONFIRMED | the doc lives at `docs_archive/chain/chain_reorg.md` (R2: a reorg never crosses a cut) | |
| 13-1 | the lock is Flexible Paxos with Q2 = m, Q1 = n - m + 1 | CONFIRMED | `chain/lock.rs:53-56`; a restore's need `:63-70` | LK |
| 13-2 | a leader change needs max(m, n - m + 1) reachable seats | CONFIRMED | n - m + 1 promises, then m accepts; `lock.rs:416` uses the same max for the amnesia floor | LK |
| 13-3 | table rows (2, 6, 8, 51, 91) | CONFIRMED | computed | |
| 13-4 | m should be at least a majority | CONFIRMED, decided | the majority rule, `chain_rescue_plan.md` | CR |
| 13-5 | warn when a roster change would violate it | WRONG premise | seats and m are fixed at founding (`verify.rs` `joined` refused; `rule_m: head.rule_m`) | none |
| 13-6 | a rotating sealer gives a hand-over per height and no safety | CONFIRMED | `chain_divergence_plan.md:195`, `:342` | |
| 13-7 | FROST/BLS add no agreement and need a fixed signer subset | NOT CHECKABLE STATICALLY (general) | no such code | none |
| 13-8 | BFT (> 2/3) halts 2-of-3 with one seat offline | CONFIRMED | arithmetic | |
| 13-9 | leader gossip is O(n) per block | NOT CHECKABLE STATICALLY | measured 17-22 vs 50 frames per block at 2-of-3 (`chain_lock_wiring_plan.md` §10) | |
| 13-10 | activation by vote needs every seat's capability; old builds stop | CONFIRMED | `chain_lock_wiring_plan.md` §11 preconditions | LK, BI |
| 13-11 | the repo has nothing above 3-4 seats | PARTLY | tests found up to 5 seats (2-of-4 vault, 3-of-5 MCP recipe); none above | |

Counts: CONFIRMED 24, PARTLY 7, WRONG 1, NOT CHECKABLE STATICALLY 4.

## B. The toy model

| models | leaves out |
|---|---|
| seats, m, open proposals, frame loss, a "presence disagreement" rate, the signer set in or out of the id | the sealing line with its grace (`sealer_wait`), so a second sealer waits instead of sealing at once |
| | B6's smallest-id rule only partly (#11 has it as a policy; #10's does not say) |
| | relink of variants (`relink_onto_variant`): in the code a variant is re-linked, not decided |
| | standing votes, re-send on heal, park of approvals one height ahead |
| | the real presence model (heard within the activity window), catch-up, the beacon |

Read the percentages as orders of magnitude at most: "a few percent of
heights get an id variant in 2-of-3 under loss, and it grows with n" is
plausible. The exact values are not, because the grace periods and the
relink both reduce what reaches the operator. The v0.1.2
`fork_decision` line (both hashes, both proposal ids) measures it in the
field; the `chain_convergence.rs` keystone measures it on loopback.

## C. What each issue changes

| issue | change | where |
|---|---|---|
| #10 comment | every identity comparison enumerated; attestation merge rule | `block_identity_plan.md` §3a |
| #11 | prev binding as an open question, with the finding that it does not stop the 11-4 schedule | `chain_rescue_plan.md` Q8 |
| #12 | Q7 refined with what is irreversible today and when it fires | `chain_rescue_plan.md` Q7 |
| #13 | none new: majority rule decided; the capability gate already in BI §4 | |

Order for v0.1.3:
1. Block identity v2.
2. Measure: the keystone and field `fork_decision` lines, variants against real forks.
3. Branch choice + majority rule.
4. Then decide prev binding (Q8), finality (Q7) and the lock (Q6), from the data.

## D. Contradictions

| where | finding |
|---|---|
| #11 suggestion 1 vs the issue's own schedule | binding `prev` does not stop two changes on the same predecessor (11-4, 11-9). Only one vote per slot does (the lock or E0) |
| #12 "treasury spend" | there is no spend in Stage 1 (12-5) |
| #12 "vault effects undone" | the disclosure is not undone; the code only keeps an audit line (12-4) |
| #13 "roster change" | no roster changes exist (13-5) |
| our `chain_rescue_plan.md` §4 | "wallet init / DKG: the purse record is gone" is too strong: the records stay until the block is final (below a cut). Correction: the chosen branch does not know the purse; the key records stay on disk; funds sent to its address are outside the chosen chain's view |
| our `chain_divergence_plan.md` C1 | "re-signs EVERY open approval": every open approval THIS seat gave (11-3) |
| our `block_identity_plan.md` §1 | holds: `tip_swapped` needs a different change (`sync.rs:1019-1025`, `:1051`) |
