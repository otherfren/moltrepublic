# The supersede verdict survives the apply: 320 applied patches read `superseded: "conflict"`

Status: **OPEN (2026-09-13)** - fix plan, execution-ready; the liveness
twin in §2.4 is a hypothesis until its test runs. Found on a live seat of a
production republic (dev build of `1356e034`, the node reopened
the same night). Siblings: `list_proposals_unbounded.md`,
`wiki_changes_below_the_cut.md`. Leaves for `docs_archive/reviews/` with
the change that closes §4.

## 1. Symptom

`list_proposals` / `read_proposal` on that seat: 320 of 328 applied
memory proposals (from every day since the founding) carry
`state: "applied"` AND `superseded: "conflict"`. The tool text defines
`conflict` as "dead, `state: superseded`". The eight clean ones are
proposal 0 (the base) and seven small patches; the two sampled are
add-only (one new page) while a sampled conflict one
edited two existing pages. `votes` on the conflict cards read
approved/approved/open - decided cards, not corpses.

## 2. Cause (verified by reading; the numbered path is the reopen)

1. `open_stored_workspace` (`crates/molt-engine/src/session.rs:1330`):
   `restore_dump` → replay of the event TAIL (`self.apply(env)` per
   event, `:1388`) → `adopt_chain` (`:1424`) → `adopt_wiki_base`
   (`:1432`). Snapshots are cut every 1 000 events
   (`crates/molt-engine/src/events.rs:23`), so the whole 09-08..09-12
   history was still in the tail.
2. A `WorkspaceEvent::Proposed` in the tail registers the record as
   `Proposed` and runs `supersede_stale_wiki(None)`
   (`crates/molt-engine/src/events.rs:314-337`). At that moment the chain
   is not adopted: `chain.head` is `None` (`is_chain_governed` false,
   `crates/molt-engine/src/chain/governance.rs:48`), `chain.applied` is
   empty, and the legacy `applied` map of a chain republic holds no wiki
   patches - so `wiki_base()` folds an EMPTY tree from nothing and
   answers `Ok`. The K6 base-pending guard (`proposals.rs:3631`) does not
   fire because no base commitment is visible yet.
3. The walk (`crates/molt-engine/src/proposals.rs:3586-3672`) re-checks
   every `Proposed` wiki patch against that empty tree: a patch that
   modifies, renames or deletes a page fails `wiki_patch_applies` →
   `state = Rejected`, `superseded = true`, `superseded_kind = Conflict`
   (`:3665-3668`). A pure-add patch applies and stays clean - the seven
   exceptions.
4. `adopt_chain` → `settle_cards_against_chain`
   (`crates/molt-engine/src/chain/projection.rs:428-492`) turns every
   chain-consumed card `Applied` (`:487`) and, like the four other
   Applied transitions (`governance.rs:638`, `:778`; `events.rs:405`;
   `membership.rs:196`), leaves `superseded` / `superseded_kind`
   standing. The next snapshot persists the verdict; nothing ever clears
   it. Deterministic: every reopen of every full holder does this.
5. molt-mcp `withdrawn_is_a_state` (`crates/molt-mcp/src/lib.rs:544-577`)
   renders a `superseded: true` without a kind as `"conflict"` whatever
   the state, and the present test pins it ("an applied card … no kind,
   but the bool was set → conflict", `:2919`, `:2928`).

The R20 note of round 3 ("a proposal can be `superseded: true` and still
seal", `docs_archive/reviews/mcp_agent_friction_2026-09-06_r3.md:118-122`)
was this mechanism seen from the other side.

### 2.4 The liveness twin (hypothesis - the first test decides)

An OPEN modify/rename/delete patch sitting in the tail at a reopen dies
the same way (`Rejected` + `Conflict`) and nothing resurrects it: the
settle touches only chain-consumed ids, the WP2 re-serve `or_insert`s
without clobbering (`events.rs:314`), and the R2 walk in
`adopt_wiki_base` (`crates/molt-engine/src/chain/checkpoint.rs:268`)
re-checks `Proposed` cards only. The existing reopen tests ratify
add-only patches (`add(path, body)` in
`crates/molt-engine/tests/checkpoint_under_load.rs`), which is exactly
the shape the empty-tree walk lets through. If the test in §4.1(b) goes
red, a restart kills every open wiki edit on the restarting seat - a
data-loss bug, and the reason this issue is fixed before the other two.

## 3. Design

- **The walk does not judge against a tree that is not the republic's
  base.** `State.chain_pending: bool`, set in `open_stored_workspace`
  between the point of no return and `adopt_chain` whenever the storage
  carries a chain (`!chain.is_empty() || checkpoint_blob.is_some()`),
  cleared by `adopt_chain`. `supersede_stale_wiki` returns early while it
  is set - the same posture as base-pending (§4.9.6). The tail's
  `Proposed` cards get their registration check from the walk that
  already runs after the settle (`projection.rs:577`, "reach the same
  terminal states a live node reached") and, where a cut exists, from the
  R2 walk once the base is adopted. Reordering the reopen (adopt before
  replay) was considered and rejected: the tail carries the collected
  signatures the adopt re-verifies (R12) and the consumed-id gate depends
  on the order (review E1 residual).
- **The block outranks the walk.** One helper,
  `ProposalRecord::settle_applied(&mut self)` in molt-core: state
  `Applied`, `superseded = false`, `superseded_kind = None`. All five
  transition sites call it. A record that carried a local verdict and
  then sealed on another seat (R20) reads applied and clean.
- **Stored verdicts on applied cards are healed on load.** In
  `restore_dump` (and the snapshot-less path via `settle`): an `Applied`
  record with a verdict is normalized; one structured `warn!(count)`
  per open. This is what repairs the affected store on its next reopen; no
  manual step.
- **The MCP flag reads a kind only while it matters.** `superseded` is
  `"rebase"` or `null` on `proposed`/`sealing`, `"conflict"` on state
  `superseded`, `null` on every other state. Tool texts of
  `list_proposals`/`read_proposal`: "`null` on every decided card".
- GUI: `crates/molt-ui/src/surfaces.rs:1434`, `:1608` copy
  `p.superseded` into the Denied label; an applied card never reaches
  that view, and after the heal the bool is false anyway. No GUI change
  expected; the GUI test shards confirm.

## 4. Work, red first

1. Unit, `crates/molt-engine/src/chain/governance_tests.rs` beside
   `a_reopen_keeps_the_apply_stamp` (the `stored_chain_signer` /
   `reopen_chain_signer` harness):
   (a) a chain-governed seat with an applied add (page `a`) and an applied
   modify of page `a`; log-only reopen → both cards `Applied`,
   `superseded_kind == None`, `superseded == false`.
   (b) the liveness twin: an OPEN modify of page `a` at close → after the
   reopen it reads `Proposed` and an `approve` still seals it.
   (c) a stored `Applied` record carrying `Conflict` (hand-built dump)
   reads clean after `restore_dump`.
2. Integration, new `crates/molt-engine/tests/reopen_supersede.rs` over
   `MockRelay` (the `found_three`/`ratify` helpers of
   `checkpoint_under_load.rs`): ratify add then modify, close and reopen
   one seat, list → clean; with a cut in between → still clean.
3. molt-mcp present tests: the applied + `superseded: true` fixture
   reads `null`; `proposed` + `rebase` stays; state `superseded` reads
   `conflict`; a `withdrawn` card reads `null`.
4. Core: `settle_applied`; engine: the flag, the five call sites, the
   heal in `restore_dump`; MCP: `withdrawn_is_a_state`, tool texts.
5. Docs: one sentence in `docs_archive/memory/shared_memory_real.md` §4
   ("the registration walk waits for the adopted chain; a sealed block
   outranks a local verdict"); this file's status.
6. Verify on the live seat after the next reopen:
   `list_proposals {state: "decided"}` (after
   `list_proposals_unbounded.md`) shows `superseded: null` throughout.

## 5. Decided here (object if wrong)

- Clearing the verdict on apply is right also for a TRUE conflict that
  sealed elsewhere: the chain is the truth, the fold's void verdict is
  display (§4 of `shared_memory_real.md`: "the chain honestly records that
  the vote passed").
- The heal logs a count, not a list: 320 warn lines would be the log
  noise D10/D11 removed.
