# The supersede verdict survives the apply: 320 applied patches read `superseded: "conflict"`

Status: **EXECUTED 2026-09-13** - built the same day, red-first; the
liveness twin of §2.4 was CONFIRMED by its test (a hard-kill restart
killed every open edit). Found on a live seat of a production republic
(dev build of `1356e034`, the node reopened the same night).
Siblings: `list_proposals_unbounded.md`, `wiki_changes_below_the_cut.md`.

## 1. Symptom

`list_proposals` / `read_proposal` on that seat: 320 of 328 applied
memory proposals, from every day since the founding, carry `state:
"applied"` AND `superseded: "conflict"`. The tool text defines `conflict`
as "dead, `state: superseded`". The eight clean ones are the base entry
and seven small patches; a sampled clean one was add-only (one new
page), a sampled conflict one edited two existing pages. `votes` on the
conflict cards read approved/approved/open - decided cards, not corpses.

## 2. Cause (two paths, both verified by a red test)

**2.1 The live path - every seat, every sealed wiki patch.**
`append_committed_block` (`crates/molt-engine/src/chain/governance.rs:598`)
runs `project_one` BEFORE `after_block_applied`: the projection's Applied
arm (`crates/molt-engine/src/chain/projection.rs:335-352`) runs the
supersede walk over the moved paths while the consumed card is still
`Proposed`, and the patch just folded never applies to the tree that now
contains it (an add onto an existing page, an edit whose context is gone)
- so the card it belongs to reads `Rejected` + `Conflict`
(`proposals.rs:3665-3668`). `after_block_applied` then sets `Applied`
(`governance.rs:638`, A1 guard) and, like the four other Applied
transitions (`governance.rs:778`; `events.rs:405`; `projection.rs:487`;
`membership.rs:196`), leaves the verdict standing. The next snapshot
persists it. The seven clean cards were settled by a rebuild
(`settle_cards_against_chain` runs before its walk) rather than by a live
block.

**2.2 The reopen path - a hard kill.** `open_stored_workspace`
(`crates/molt-engine/src/session.rs:1330`): `restore_dump` → replay of
the event TAIL (`self.apply(env)` per event, `:1388`) → `adopt_chain`
(`:1424`) → `adopt_wiki_base` (`:1432`). A `WorkspaceEvent::Proposed` in
the tail registers the record as `Proposed` and runs
`supersede_stale_wiki(None)` (`crates/molt-engine/src/events.rs:314-337`).
At that moment the chain is not adopted: `chain.head` is `None`,
`chain.applied` is empty, and the legacy `applied` map of a chain republic
holds no wiki patches - so `wiki_base()` folds an EMPTY tree and answers
`Ok`; the K6 base-pending guard (`proposals.rs:3631`) does not fire
because no base commitment is visible yet. Every replayed patch that
modifies, renames or deletes a page fails `wiki_patch_applies` against
the empty tree. A clean close writes a snapshot and leaves no tail (the
clean-close test stays green without the fix); a hard kill (OOM, power,
SIGKILL) replays the tail.

**2.3 The presentation.** molt-mcp `withdrawn_is_a_state`
(`crates/molt-mcp/src/lib.rs:544-577`) rendered a `superseded: true`
without a kind as `"conflict"` whatever the state, and the present test
pinned it.

The R20 note of round 3 ("a proposal can be `superseded: true` and still
seal", `docs_archive/reviews/mcp_agent_friction_2026-09-06_r3.md:118-122`)
was 2.1 seen from the other side.

### 2.4 The liveness twin - CONFIRMED

An OPEN modify/rename/delete patch in the tail at a hard-kill reopen died
the same way (`Rejected` + `Conflict`) and nothing resurrected it: the
settle touches only chain-consumed ids, the WP2 re-serve `or_insert`s
without clobbering, and the R2 walk in `adopt_wiki_base` re-checks
`Proposed` cards only. Pinned red-then-green by
`a_reopen_keeps_applied_edits_clean_and_open_edits_open`
(`crates/molt-engine/src/chain/governance_tests.rs`, the log-only reopen)
and `a_hard_kill_reopen_keeps_a_ratified_edit_clean_and_an_open_edit_open`
(`crates/molt-engine/tests/checkpoint_under_load.rs`, a fresh engine on
the same directory with no closing snapshot).

## 3. What was built

- **The walk runs after the settle.** The moved-paths walk left
  `project_one` for `after_block_applied`'s Applied arm, right after the
  card settles (both callers of `append_committed_block` run it next;
  the rebuild path already settled first). The consumed card is `Applied`
  when the walk looks, so `state == Proposed` is the whole candidate
  rule - no id filter, no widened `seen`.
- **The walk does not judge against a tree that is not the republic's
  base.** `ChainProjection.adoption_pending`, set by `State::replay_loaded`
  (the ONE replay half of a reopen, used by `open_stored_workspace` and
  the restart-test harness alike) whenever the storage carries a chain,
  cleared by `adopt_chain` and by the workspace reset; the walk returns
  early while it is set. The tail's cards get their registration check
  from the walk after the settle (`projection.rs:577`) and, under a cut,
  from the R2 walk once the base is adopted. Residual: that walk carries
  no `moved` paths, so a `rebase` a live seat showed for an open patch is
  not reproduced after a hard kill (`docs/reviews/known_debt.md`). Reordering the reopen (adopt before replay) was rejected:
  the tail carries the collected signatures the adopt re-verifies (R12)
  and the consumed-id gate depends on the order (review E1 residual).
- **The block outranks the walk.** `ProposalRecord::settle_applied`
  (molt-core): state `Applied`, `superseded = false`, `superseded_kind =
  None`; all five transition sites call it.
- **Stored verdicts on applied cards are healed on load.** `restore_dump`
  clears them and logs one `warn!(healed = n)`. This repairs the affected
  store on its next reopen; no manual step.
- **The MCP flag reads a kind only while it matters.** `"rebase"` or
  `null` on `proposed`/`sealing`, `"conflict"` on state `superseded`,
  `null` on every other state; the tool texts say so.
- GUI: untouched - the Denied label reads `p.superseded`, which an applied
  card no longer carries.
- Spec: `docs_archive/memory/shared_memory_real.md` §4, "The block
  outranks the walk".

## 4. Tests

- `chain::governance_tests::a_reopen_keeps_applied_edits_clean_and_open_edits_open`
  - red on the live path first ("live add: no verdict"), then red on the
  reopened open edit (`Rejected`), then green; the open edit seals after
  the reopen.
- `chain::governance_tests::a_stored_verdict_on_an_applied_card_is_healed_on_load`.
- `checkpoint_under_load`: `a_clean_reopen_…`, `a_hard_kill_reopen_…`
  (red without the adoption gate, verified) and
  `a_hard_kill_reopen_under_a_cut_…`; each seals the open edit after the
  reopen.
- molt-mcp `proposals_present_as_headers_and_one_full_record`: an applied
  card with a stale bool reads `null`, a withdrawn one too.

Deferred: the live-seat verification (`list_proposals {state:
"decided"}` reads `null` throughout) waits for the live seat to run a
build with this fix and the filter of `list_proposals_unbounded.md`;
until then the heal is verified by the load test only.

## 5. Decided

- Clearing the verdict on apply is right also for a TRUE conflict that
  sealed elsewhere: the chain is the truth, the fold's void verdict is
  display (§4 of `shared_memory_real.md`: "the chain honestly records that
  the vote passed").
- The heal logs a count, not a list: 320 warn lines would be the log
  noise D10/D11 removed.
