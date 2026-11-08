# Plan: one-click relay adoption - bulk, cleanup, republic relays

Status: **PLAN (2026-11-08), Q1-Q4 decided the same day (§4); in build.** Follows
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

Tests (red first):

- `tests/gui/link_relays.rs`: `the_bulk_adopt_confirms_onion_outright_and_clearnet_through_one_warning`
  - two clearnet + one onion blocked row: one click fires the onion adopt,
  opens ONE modal naming both clearnet URLs, confirm fires both.
- same file: `no_bulk_button_below_two_blocked_rows`.
- `tests/surfaces.rs`: the action layer issues the per-URL command
  sequence and treats "already in the relay pool" as success.

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

- Only on the LIVE append of a `set_relays` block
  (`append_committed_block` -> `project_one`: own seal, peer commit,
  sync extend). A full rebuild (open, reorg, restore) adopts nothing:
  the persisted workspace copy of the pool is never rewritten, so a
  replay would re-add a relay the operator removed on every start.
- Only the relays the block adds (`effective after \ effective
  before`); the genesis pool, and so a relay deselected at join, is
  not touched.
- An onion relay is added confirmed; a clearnet or local one is added
  unconfirmed (it needs the exposure warning, §0) and shows up in the
  offer below. An entry already in the pool keeps its state.
- Run log per relay: `relay=… confirmed=… adopted from ratified pool`.

Everything the automatic path does not cover (a rebuild, an unconfirmed
or removed entry, clearnet off) is offered:

Engine (`molt-engine`, read surface beside `org_stats.relays`):

- `missing_relays: Vec<InviteRelayVerdict>` =
  `diagnose_invite_relays(effective_relays, settings.relays,
  clearnet_session)` filtered to `blocked.is_some()`. It reuses the same
  classifier, so GUI, MCP and run log agree.
- Recomputed on `adopt_pool_change`, open, and every `Relay*` command
  (they already `emit_session`).
- Run log, once per (workspace, relay): `relay=… block=… missing from
  local pool`.

GUI:

- Settings › Nostr relays: a section "Relays of this republic you don't
  use", rows + per-row adopt + `Add all (n)`, the same component as §1
  (extract it from the link panel rather than copying it).
- The "No shared relay" status and the members-list split marker link to
  that section.

MCP: the field rides the existing read (`read_session` / the org stats
surface); adoption stays `relay_add` + `relay_confirm`.

Ledger limit (stated, not fixed here): adopting Z locally makes Z
dialable at once (`RelayConfirm` calls `dialable_relays_changed`), but
the seat's ledger entry (`member_relays`) changes only through a
`Membership` block (join/restore). Split detection can therefore keep
reporting a split that the adoption already healed. See Q4.

Tests (red first):

- engine: `a_ratified_relay_the_seat_lacks_is_reported_missing` - two
  seats, pool vote adds Z, the seat without Z gets `missing_relays = [Z:
  NotInPool]`; after `RelayAdd` + `RelayConfirm`, `[]` and Z is dialed.
- engine: clearnet Z with `clearnet_enabled` off reports `ClearnetOff`.
- GUI: the settings section lists the field and adopts through §1's path.

## 4. Decisions (user, 2026-11-08)

- **Q1** One exposure modal for all clearnet relays of a bulk adopt.
- **Q2** Mixed clearnet + local: one modal, both bodies.
- **Q3** Ratified relays are adopted automatically (§3 bounds).
- **Q4** Ledger staleness accepted; the split marker says when the
  local pool already dials the bridge relay: re-join to declare it.

## 5. Order

§2 first (it removes the path §1 would otherwise have to keep in step),
then §1, then §3 (it reuses §1's component). Each step is its own commit:
live-preview checks (`scripts/dev-ui.sh build`, the molt-ui live-preview
suite), per-crate clippy, review, push. No full window build (user
decision 2026-09-08).

Archive this doc with the last step's commit; update `relay_pool.md` §6.
