# Relay topology — the group shares its relays, and says so

**Status: R2b–R6 BUILT (R3/R3b/R4/R5/R6 landed 2026-08-02..04, the R6
make-before-break gate and the pool-edit UI 2026-08-09); R1, R2 and R2c still
open.** Rules ratified by the user 2026-08-02; recorded in
`nostr_transport_marmot.md` §10.15. This document is the execution plan.

Read first: `relay_pool.md` (relays do not federate; the confirmation gate),
`nostr_transport_marmot.md` §10.15, `nostr_n4_plan.md` §8.8 (N4b, in flight).

## 1. The rules

1. Everyone must be able to reach the same relay. Either all members join over
   one common relay, or — when a member runs their own — that relay must
   already be in every other member's pool **before** they join.
2. The founder has **no special standing**. Coordinating the opening ritual is
   the whole of it; afterwards every member is equal.
3. Relays are **exchangeable**. A member moving to a new relay **re-joins**;
   the others must have added it first, or the join fails.
4. The members **keep a ledger** of who knows which relay.
5. A **split is communicated** — run log and error messages, naming the
   missing relay.
6. The **Create-DAO dialog states rule 1** before invites are minted.

## 2. Why this is not just a UI change

The current gate proves the condition needed to JOIN, not the one needed to
OPERATE. That is the failure class this project keeps meeting: a condition
that holds when it is checked and quietly stops holding later. Two members can
both satisfy "share a relay with the founder" and still share none with each
other.

The founder's pool is also, today, the group's relay list: it is captured at
founding (`founding.rs`, the `dialable` ∩ cap-8 set), baked into every invite
and Welcome, and sealed into `TransportState.relays`. Nothing can change it
afterwards. That is rule 2 violated by construction — not by intent, but
because no other mechanism exists.

## 3. What exists today (verified 2026-08-02)

| piece | where | state |
|---|---|---|
| per-node relay pool + confirmation | `molt_core::relay`, `RelayEntry` | real |
| dialability verdicts per relay | `relay::diagnose_invite_relays` | real |
| whole-pool verdict | `relay::pool_gap` | real |
| join-time gate vs the INVITE's relays | `lifecycles.rs::cmd_join_start` | real |
| operator-facing refusal text | `relay_msg::join_relay_refusal` | real |
| group relay list, sealed | `TransportState.relays` | written at founding/join |
| …read by | the N4b recovery mint only | since step 5 |
| per-member relay knowledge | — | **does not exist** |
| relay change → re-join | — | **does not exist** |
| split detection | — | **does not exist** |

`State.nostr.relays` (N4b step 5a) is the group list in the live actor.

## 4. DECIDED 2026-08-02 (user-ratified) — the pool is chain state

**(a) Chain-borne.** Concretely:

- **The initial pool is the founder's**, written into the chain and **signed by
  everyone**. Seeding it is coordination, not authority.
- **Afterwards the founder holds no privilege over it.**
- **Every later change needs threshold consent**, like any other gated change.
- The ledger of who-knows-which-relay is therefore a **chain projection**, the
  same shape as `working_nostr_pk` — computed from applied blocks, never
  persisted separately, identical on every node.

Rejected: an ephemeral projection fed by each member announcing its own relay
set. It would make "who can reach whom" depend on unauthenticated gossip —
exactly the thing §10.15 was opened about — and the ledger would diverge per
node.

### 4.1 The consequence: `roster_canonical_bytes` must bump to v4

"Signed by everyone" and "in the chain" together mean the genesis. Block 0's
`approval_bytes` **is** `roster_canonical_bytes` (`persistent_chain.md`), and
that layout (`molt-roster-v3`) does not carry relays today. So the initial pool
has to be bound into those bytes → **`molt-roster-v4`**.

This is the ~15-site ripple `CLAUDE.md` warns about: founder canonical,
`verify_sealed_roster`, `verify_seal_proposal`, the byte-pin tests, and every
harness that recomputes the signed table — all in ONE change, or signatures
break silently. Length-prefix the relay list per the
`hash-length-prefix-not-separators` rule; a separator-joined list of
member-supplied URLs is forgeable.

It also lands the pool in **sign-what-you-see**: members already ratify the
name and agenda they were shown, and after this they ratify the relays too —
which is right, because the relay set decides who can reach whom.

**Alternative considered and rejected:** carry the initial pool as a separate
n-of-n block at height 1. It avoids the v4 bump, but it leaves a window where
the republic exists with no agreed relays, and it splits one constitutional
fact across two blocks.

## 5. Steps (one commit each, red test first)

### R1 — The Create dialog states the rule
The founder is the one person who can still choose the relay cheaply, and the
only one who sees this screen before invites exist.

- Copy, compact (`CLAUDE.md`): the group needs ONE relay everyone can reach; a
  self-hosted relay must be in every member's pool first.
- Shown in the create wizard next to the relay/threshold inputs, not as a
  modal.
- **Red:** a lexicon test pinning the string is short and names both branches.

### R2 — A refused join names the missing relay
Largely built (`join_relay_refusal`) — this is the audit that it says the right
thing under rule 1, plus the headline.

- The refusal must name what to ADD, not merely what is wrong.
- `headline_for` already maps this to "No shared relay".
- **Red:** the joiner's log names the republic's relay it does not have.

### R2b — The pool is visible where the other group settings are ✅ DONE
`StatusView.relays` carries the GROUP's pool (not this node's own settings
pool — a different list), and `OrgSettingsCard` shows it beside the retention
window. *Amended 2026-08-09 (user decision): the Status card shows only the
COUNT plus the edit pencil — the URL list itself lives in the R6 edit modal,
which lists the pool as editable rows.*

### R2c — The founder PICKS the group's relays in the create dialog
*(user idea, 2026-08-02 — and a prerequisite for R3)*

Today the invite's relay list is an accident: `cmd_create_start`
(`founding.rs:474`) takes `dialable(own pool)` and caps it at 8 **in pool
order**. Nobody chose it, and the founder cannot see or change what the invite
will carry.

That is fine as a default and wrong as a mechanism, for two reasons:

1. **R3 makes this set constitutional.** What every member signs into the
   genesis must be a deliberate set, not whatever one node's settings page
   happened to hold that afternoon.
2. **It weakens the joiner's error message.** The refusal can name the relays
   the invite carries, but not with authority — "the republic uses these" is
   only true if the founder meant them. With an explicit pick, the joiner's
   refusal states a fact about the REPUBLIC instead of a fact about the
   founder's laptop.

**Build:** a relay picker in the create wizard beside the rule text R1 already
shows — the node's confirmed relays, each toggleable, defaulting to the current
dialable set, capped at `MAX_PAYLOAD_RELAYS` with the existing
"using the first eight" note. The picked set becomes the invite's
`InviteHandoverV2.relays`, the Welcome's list, and (with R3) the signed
genesis pool.

- **Red:** a founder who deselects a relay mints an invite that does not name
  it; a joiner refused against that invite names the PICKED relays and nothing
  else.

**Note the cap interacts with rule 1.** If a founder picks more relays than the
payload can carry, the invite silently describes a smaller republic than the
genesis would. Either the pick is capped at `MAX_PAYLOAD_RELAYS` in the UI (so
the two can never disagree), or R3 must carry the full list some other way.
Cap in the UI — the simpler invariant.

### R3 — The initial pool is signed by everyone ✅ DONE 2026-08-02
The `molt-roster-v4` bump of §4.1, with the pool travelling from the founder's
create-wizard choice into the founding table every member ratifies.
- **Red:** a v3 table must not verify against a v4 seat; a sealed roster whose
  relay list was altered after ratification must fail `verify_sealed_roster`;
  the byte-pin fixture is recomputed independently.
- Then `State::group_relays()` reads the VERIFIED genesis, not
  `TransportState.relays`.

### R3b — The ledger ✅ DONE 2026-08-04
`State::member_relays(member)`, a projection over applied blocks like
`working_nostr_pk`: a `Membership` block can carry the relays a seat declares
(conditionally signed — empty appends nothing, pre-R3b preimages stay
byte-identical), the checkpoint summary carries the ledger across a cut
(`molt-chain-checkpoint-v6`), and a member without a declaration is covered
by the ratified genesis pool.
- **Red:** after a member joins over relay X, every OTHER member's ledger
  reports X for that member.
  (`the_ledger_reports_declared_relays_and_survives_a_cut`)

### R4 — Split detection ✅ DONE 2026-08-04
With the ledger, a split is computable: any pair of members with an empty
relay intersection (`State::relay_splits`).
- Surfaced as a named state, not a silence: a structured warn once per pair,
  and the members surface carries a per-member `split` marker naming the
  counterpart and the bridging relay.
- **Red:** two members with disjoint relay sets produce a split verdict naming
  both and the missing relay.
  (`disjoint_relay_sets_produce_a_split_verdict_naming_the_bridge`)

### R5 — Relay change → re-join ✅ DONE 2026-08-04
The rejoiner declares the relays it can actually dial (`RecoverRequest.relays`,
bound inside the seat proof — conditionally, pre-R5 proofs keep verifying);
the coordinator's gate refuses a declaration that shares no relay with some
member, and a passing declaration becomes the seat's ledger entry on the
`Restored` block.
- The refusal names the relay the others must add — that message is the whole
  feature (rule 5).
- **Red:** a re-join whose new relay is in nobody else's pool fails with that
  relay named; adding it on the others makes the same re-join succeed.
  (`a_rejoin_over_a_foreign_relay_is_refused_naming_it`,
  `a_seat_proof_binds_the_relay_declaration`)

### R6 — The pool is editable under threshold, from the details window ✅ DONE 2026-08-04
Built as an ordinary gated Organization op (`set_relays`, space-separated
URLs) rather than a new `ChainChange` variant: the retention edit next to it
already IS the exact shape (proposal → threshold → applied), so the pool edit
reuses the whole propose/approve/commit machinery, the checkpoint summary
carries it as a last-write-wins slot (`organization.relays`,
`applied_lww_slot`), and no new signing path exists. The effective pool =
latest applied edit, else the ratified founding pool
(`State::effective_relays`); a commit reaches the LIVE transport by
rebuilding the group runtime over the shared ratchet Arc (the accepted
whole-group blip, Track C option A — the §4.4 rotation grace is deliberately
NOT built). The propose-pencil sits in `OrgSettingsCard`; the modal proposes
via the generic `org-propose`.

This is what makes rule 2 real: the pool stops being "whatever the founder's
pool was at founding" and becomes group state any member can move and no
member can move alone.
- **Red:** a non-founder can carry a pool change to completion; a change below
  threshold does not alter the effective pool; removing the last relay a
  DECLARED member can reach is refused naming the member and its relay
  (undeclared members follow the governed pool and never gate).
  (`a_pool_edit_commits_under_threshold_and_moves_the_effective_pool`,
  `a_pool_edit_that_would_strand_a_member_is_refused`)

**Make-before-break (added 2026-08-09, found by a live E2E).** A pool edit
whose new pool shares NO relay with the effective pool is refused at propose
time. The committing block travels over the OLD pool, and every member that
applies it rebuilds its runtime onto the new pool only — with zero overlap
the members that have not yet applied keep listening where nobody publishes
anymore, and the republic tears permanently at exactly that commit (a
throwaway 2-of-2 split this way in the E2E; the per-member strand gate did
not fire because founding-era seats carry no ledger declaration). A full
migration is two votes: add the new relay, then drop the old.
(`a_pool_edit_sharing_no_relay_with_the_current_pool_is_refused`)

**UI (2026-08-09).** The edit modal lists the draft pool as rows (delete per
row, validated add field — molt-core's own URL parser, so the field message
and the engine gate can never disagree), and a pending `set_relays` renders
as a DIFF vote card: the union of current and proposed pool, one row per
relay, marked kept / + added / − removed
(`a_pool_edit_proposal_carries_the_diff_rows`, `relay_pool_diff`).

## 6. Ordering against N4b

N4b steps 6–12 are in flight and touch the same files (`net.rs`,
`nostr_ritual.rs`, `recovery.rs`). R1/R2 are copy-and-test only and can land
beside it. R3–R6 must wait for N4b to finish, or they will collide in
`ChainChange::Membership` (step 2's layout) and in the Welcome payload
(step 10's open size decision).
