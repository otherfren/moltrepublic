# Plan: one-click relay adoption - bulk, cleanup, republic relays

Status: **EXECUTED (2026-11-08).** Q1-Q4 decided the same day (§4). Follows
`988c5cde` (link panel under the link field, `relay_pool.md` §6).

## 0. Where we stand

- A join/recovery link carries the founder's relays
  (`InviteHandoverV2.relays`: the founding pick, else the dialable pool,
  capped at `MAX_PAYLOAD_RELAYS`, `founding.rs:472`).
- The link panel (`app.slint:5174`) lists them with this node's verdict
  (`diagnose_invite_relays`) and adopts ONE blocked relay per click:
  onion = `RelayAdd` + `RelayConfirm`, clearnet/local = `RelayAdd` + the
  exposure modal -> `RelayConfirm { accept_clearnet: true }`.
- Nothing writes the republic's relays into a member's local pool
  (`settings.relays`) on its own. Join only keeps what was clicked;
  `materialize_workspace` stores them as the workspace's ratified pool
  only.
- The 445 runtime dials `effective_relays ∩ local dialable`
  (`dialable_group_relays`, `net/mod.rs:660`). A ratified pool change
  (`adopt_pool_change`, `projection.rs:293`) that adds relay Z therefore
  dials Z on no member who lacks it locally, and nothing offers Z to them.
  The only trace is the members-list split marker and "No shared relay".

Gaps:

1. n clearnet relays in a link = n clicks + n modals.
2. A second, older adopt path: `jw_adopt_relays` on join step 0
   (`app.slint:4813`, `on_adopt_invite_relays`, `relays.rs:123`). It adds
   everything, confirms only onion, and ends in a "confirm in Settings"
   toast - the exact chore the panel removed.
3. Ratified relays the member lacks are never offered.

## 1. Bulk adopt in the link panel

Counterargument first: `relays.rs:127` argues that the convenient path
must not be the less private one. One acknowledgement for several
clearnet relays weakens per-relay deliberation. It holds anyway because
(a) `clearnet_enabled` is ONE node-wide switch, so the exposure decision
is already global; (b) the modal lists every URL it confirms and its
acknowledgement starts unchecked; (c) onion relays need no decision.
Decided by Q1.

Design (GUI only; MCP already has `relay_add` / `relay_confirm`, so
co-equality needs no new `Command`):

- Button `Add all (n)` above the rows when >= 2 rows are blocked.
- Onion rows: `RelayAdd` + `RelayConfirm { accept_clearnet: false }`.
- Clearnet/local rows: ONE exposure modal listing all their URLs; on
  confirm `RelayAdd` (an "already in the pool" refusal counts as success,
  as in `on_rw_relay_adopt`) + `RelayConfirm { accept_clearnet: true }`
  per URL. Mixed clearnet+local: the local body is appended (Q2).
- The modal's single URL (`relay-clearnet-url`) becomes a list; the
  single-row adopt passes a list of one, so there is one modal, not two.
- Adopting a relay does not re-pick: new rows come back usable and picked
  (current `link_rows` behaviour), so Continue arms without another
  click.

Tests: `an_adopt_plan_confirms_onion_and_gathers_the_rest_for_one_warning`
(`tests/surfaces.rs`), `adopt_all_goes_through_one_warning_for_every_exposed_relay`
(`tests/gui/link_relays.rs`).

## 2. Remove the old join-step-0 adopt path

Since the welcome rework (`docs_archive/ui/welcome_rework.md`) the join
input form (`jw-step == 0`) has no GUI entry: the link panel on the
restore screen starts every join. It is still reachable through
`Navigate { screen: Join }` (MCP `navigate`) with no join run.

- Engine: `cmd_navigate(Join)` with no join run (`join.run.step == 0`)
  lands on `Screen::Restore`, the one link entry. A running or finished
  join keeps `Join`.
- Red: `navigating_to_join_without_a_join_lands_on_the_link_entry`
  (`join_tests.rs`).
- GUI: delete the step-0 block and its frame branches,
  `adopt-invite-relays` / `on_adopt_invite_relays`, `parse-invite` /
  `InvitePreview`, and the strings only that block used.

## 3. The republic's ratified relays reach the member's pool

Decided (Q3): a relay a ratified pool change ADDS is written into the
local pool automatically. Bounds that keep §0 of `relay_pool.md` and the
join pick intact:

- Only on a LIVE append (`append_committed_block` -> `project_one`: own
  seal, peer commit, sync extend), diffed once per batch: the pool before
  the batch's first pool edit against the pool it ends on, at the batch's
  `persist_chain_now`. A relay a later edit of the same catch-up dropped
  never lands. A full rebuild (open, reorg, restore) adopts nothing: the
  persisted workspace copy of the pool is never rewritten, so a replay
  would re-add a relay the operator removed on every start.
- The genesis pool, and so a relay deselected at join, is not touched.
- An onion relay is added confirmed; a clearnet or local one is added
  unconfirmed (it needs the exposure warning, §0) and shows up in the
  offer below. An entry already in the pool keeps its state.
- Run log per relay: `relay=… confirmed=… relay adopted from the ratified pool`.

Everything the automatic path does not cover (a rebuild, an unconfirmed
or removed entry, clearnet off) is offered:

- Engine: `StatusView.missing_relays` = the group pool's relays this node
  cannot dial, with the `InviteRelayVerdict` reason
  (`missing_group_relays`, the classifier the join gate uses). MCP reads
  it on `status`; adoption stays `relay_add` + `relay_confirm`.
- GUI: Settings › Nostr relays lists them above the pool, per-row adopt
  and `Add all (n)`, through §1's `AdoptPlan` / `adopt_relays`. The
  "no relay channel - check this republic's relays in Settings" notice
  already points there.
- Q4: the ledger (`member_relays`) moves only by a `Membership` block, so
  a split this node already dials across reads `re-join to declare
  <relay>` on its own row.

Tests: `a_ratified_relay_lands_in_every_members_pool` (`org_effective.rs`,
two nodes over real relays, incl. the reopen guard),
`a_catch_up_adopts_only_the_relays_its_last_pool_keeps`,
`a_split_this_node_already_bridges_asks_for_the_re_join`,
`the_relays_tab_offers_the_republics_missing_relays`.

## 4. Decisions (user, 2026-11-08)

- **Q1** One exposure modal for all clearnet relays of a bulk adopt.
- **Q2** Mixed clearnet + local: one modal, both bodies.
- **Q3** Ratified relays are adopted automatically (§3 bounds).
- **Q4** Ledger staleness accepted; the split marker says when the
  local pool already dials the bridge relay: re-join to declare it.

## 5. Not done

- The split marker is an engine string and was never localized; the new
  `re-join to declare` form follows it (both English in the German GUI).
