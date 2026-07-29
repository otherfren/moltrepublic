# Chat bus — implementation plan (multi-agent, maximally parallel)

Status: **EXECUTED 2026-07-11** (stages A, B1–B4, C — B5 unread-persistence
remains the stretch package). Deltas from the plan as written: the anchors
were re-verified first (the tree had moved 19 commits; notably `crosses_wire`
at net.rs:208, `cmd_net_delivered` at :636, INTERNAL at 24); P6 parking
landed in Stage C rather than B1 (it needs a `State` field in `lib.rs`,
which B1 did not own — B1 stopped and escalated per the brief); inbound
`channel` tags are normalized at the wire (coerce-to-Group for unnormalizable
topic claims — tags are claims, a mangled tag must not suppress content);
§5.2's MCP-equality test lives in `molt-mcp/tests/tool_reads.rs` (the legal
dependency direction — the engine never dev-depends on a surface crate); the
real-SMP `--ignored` suite was run green. Historical plan text below.

Everything here is grounded in a full code inventory (file:line references are
to the state at commit `224032b` / master `2bf6566`); if the tree has moved,
re-verify the anchors before starting.

---

## 0. Preconditions (do these BEFORE branching any implementation work)

1. **Land the recovery WIP first.** The uncommitted recovery work on master
   touches `crates/molt-engine/src/chain.rs`, `recovery.rs`,
   `tests/two_instances.rs`, `crates/molt-net/src/invite.rs`, and master's MCP
   `INTERNAL` list is `[&str; 20]` vs this branch's `[&str; 19]`. Branching
   chat-bus implementation off a master that still has that WIP in flight
   guarantees conflicts in `two_instances.rs` (1174 vs 1678 lines) and
   `molt-mcp`. Sequence: recovery merges → chat-bus implementation branches
   from the updated master.
2. **One integration branch** (e.g. `chat-bus-impl`) owned by the
   orchestrator. Stage-B agents branch from the Stage-A commit on it and merge
   back in a fixed order (§6). Nobody touches master.
3. Gates that apply to every package: `cargo clippy --all-targets` at 0
   (mind `unwrap_used`, `as_conversions`, `float_arithmetic`, `missing_docs`
   — all `warn`, kept at zero; `.expect("…")` in tests, never `.unwrap()`);
   `cargo test` green; test-first (the failing tests listed per package are
   written and seen red before the implementation).

---

## 1. Design pins (decided here so parallel agents don't diverge)

These are the judgment calls the inventory surfaced. They are **fixed**; an
agent that disagrees stops and escalates rather than improvising.

### P1 — `MessageId`: 128-bit random, hex-string on the wire, core stays RNG-free

- `molt-core` gets `pub struct MessageId(pub [u8; 16])` — `Serialize`/
  `Deserialize` as a 32-char lowercase hex string, `Display`/`FromStr`,
  `Eq/Ord/Hash`, `MessageId::NIL` (all zero) + `is_nil()`. No `uuid` crate:
  the workspace's blessed CSPRNG is `getrandom 0.2` (workspace Cargo.toml:89),
  already a direct dep of engine/net/storage, and 16 random bytes is the whole
  requirement. **`molt-core` does NOT grow an RNG dependency** (core is the
  no-I/O contract crate; OS entropy is I/O): ids are **minted by the engine**
  (`getrandom` into `[u8;16]`, same idiom as `crates/molt-net/src/invite.rs:32`)
  and passed into the constructor. `ChatMessage::text` gains an `id` parameter.
- `mockrand` (`molt-core/src/lib.rs:1342`) is explicitly NOT used for ids.

### P2 — persisted structures evolve additively; non-persisted contracts switch cleanly

The split that keeps the additive-only event rule intact without compat hacks:

- **Persisted** (lands in the encrypted log / snapshots — `WorkspaceEvent`,
  `ChatMessage`, `EngineStateDump`): additive only. `ChatMessage` gains
  `#[serde(default)] id: MessageId`, `#[serde(default, skip_serializing_if =
  "ChannelRef::is_group")] channel: ChannelRef`, and `#[serde(default,
  skip_serializing_if = "Option::is_none")] quote_id: Option<MessageId>`. The
  legacy `quote: Option<u64>` field **stays** (readable), is no longer written
  by new code, and is resolved to `quote_id` at ingest (P4).
  `WorkspaceEvent::ChatReacted/ChatDeleted/FileRemoved` each gain
  `#[serde(default)] id: Option<MessageId>` next to the legacy `index` —
  apply prefers `id`, falls back to `index` for legacy replay. Old logs and
  old snapshots keep replaying byte-identically (keystone tests
  `events.rs:414/440` and `molt-storage/src/lib.rs:1907` must stay green).
- **Not persisted** (`Command`, `Event`, `Reply`, MCP schemas — commands are
  never written to disk): clean swap, no legacy fields.
  `Command::ReactChat/DeleteChat/DownloadFile/RemoveFile` take
  `id: MessageId` instead of `index: u64`; `Command::Chat` becomes
  `{ body, quote: Option<MessageId>, channel: ChannelRef }`;
  `Command::ReadState` becomes `{ surface, #[serde(default)] channel:
  Option<ChannelRef> }`. `Event::Reacted/Deleted/FileRemoved` carry
  `id: MessageId`; `Event::Chat` gains `id` + `channel` (the UI's unread
  logic wants them; Event is a live mirror, not storage).
  `MoltError::UnknownMessage(u64)` & friends become `UnknownMessage(MessageId)`
  (update the message strings and every test matching them).

### P3 — `ChannelRef` serde shape (internal-tagging gotcha)

Repo convention is internally-tagged enums (`Command` `tag="cmd"`,
`WorkspaceEvent` `tag="type"`), and serde's internal tagging **cannot encode
newtype variants with scalar payloads**. Therefore struct variants:

```rust
#[derive(..., Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelRef {
    #[default] Group,
    Patch { id: ProposalId },     // ProposalId is a u64 newtype (lib.rs:1574)
    Topic { name: String },
}
```

plus `is_group()` for `skip_serializing_if` — a plain `Group` message
serializes to the exact same JSON as today (the `ChatMessage` doc comment at
`lib.rs:674-676` promising a stable wire shape must be updated to say
"additive fields since v…"). `Topic.name` is normalized on send (trim,
non-empty, length-capped, case-preserving; equality is exact-string — no
unicode folding in v1, note it in the rustdoc).

### P4 — deterministic synthetic ids for legacy messages

Legacy `ChatMessage`s (no `id` on disk) get an id synthesized **at the two
engine ingest choke points** — `apply`'s `Chat` arm
(`molt-engine/src/events.rs:113`) and `restore_dump` (`events.rs:263`) — as
`sha256("molt-chat-legacy-id\0" ‖ le64(position) ‖ from ‖ le64(ts) ‖ body)[..16]`,
where `position` is the message's index in `self.chat` at insertion. Both
ingest paths see the same positions, so full-replay and snapshot+tail produce
identical ids and the determinism keystone holds. Legacy `quote` indices are
resolved to `quote_id` at the same moment (the index is well-defined at apply
time — it is exactly what today's code resolves). New code never writes
positional data.

### P5 — receive-side enforcement matrix (the `cmd_net_delivered` twin)

`cmd_net_delivered` (`molt-engine/src/net.rs:563-627`) grows arms for the
newly wire-crossing events, with the same defense-in-depth posture as chat
(`msg.from = from` at `net.rs:587`):

| Incoming event        | Honored iff                                            |
|-----------------------|--------------------------------------------------------|
| `Chat`                | as today; **keep** `channel` and `quote_id` (they are global refs, unlike the old sender-local `quote` index which `net.rs:588` drops); force `msg.from = from`; **drop the message if its `id` is already known** (duplicate/replay) or `id.is_nil()` |
| `ChatReacted{id,…}`   | `by` forced to link identity; target id known (else **park**, P6) |
| `ChatDeleted{id,…}`   | link identity **== the target message's author** (no moderation concept); target known else park |
| `FileRemoved{id,…}`   | link identity == the sharer (`msg.from`); target known else park |

Duplicate detection uses a `HashMap<MessageId, usize>` maintained next to
`self.chat` (id → position) — it doubles as the O(1) lookup for every id-based
apply/handler, replacing `check_chat_index` (`chat.rs:185`). It is part of
engine state rebuilt on ingest, NOT persisted (derivable).

### P6 — park out-of-order references (convergence is impossible without it)

Cross-sender ordering is not guaranteed: at peer C, B's reaction to A's
message can arrive **before** A's message (per-sender in-order only; the MLS
path even bypasses the wire reorder buffer, `supervisor.rs:620` region). A
reaction/delete/file-remove whose target id is unknown is **parked** in a
bounded buffer (`BTreeMap<MessageId, Vec<PendingRef>>`, cap ~256 entries,
FIFO eviction) and drained when the target message arrives. Without this the
reaction-convergence test flakes by design. Parked state is engine-runtime
only (not persisted; a restart loses parked entries — acceptable, ephemerality).

### P7 — channel filter & enumeration ride the existing read reply (no new Command)

No `ListChannels` command (a new Command means MCP-tool/INTERNAL churn).
Instead `SurfaceSnapshot` (`molt-core/src/lib.rs:2119`) gains
`#[serde(default)] channels: Vec<ChannelInfo>` — populated for the chat
surface only: every distinct `ChannelRef` in the log, with
`ChannelInfo { channel: ChannelRef, count: usize, last_ts: u64 }`. The filter
is `Command::ReadState { surface, channel: Some(…) }` → `applied` contains
only matching messages (and `channels` still lists all). `Group` is always
present in `channels` even when empty. Filtered or not, `applied` values keep
their embedded `id` — index-into-applied is dead as an addressing scheme.

### P8 — patch-channel system lines are a UI-side merge

The UI synthesizes system `LogLine`s for a `Patch(id)` channel from
`SurfaceSnapshot.pending` / proposal state (`ProposalView`: id, approvals,
threshold, state) and chain events already re-read on every engine `Event`.
No engine/wire change. Titles resolve lazily via the existing `summarize()`
key-scan (`molt-ui/src/lib.rs:1662`); unknown/unresolvable proposal ids render
as `#<id>` and never error (concept Q4).

### P9 — unread counts are in-memory for the first iteration

Per-channel unread lives in the UI process (computed in `surface_data` by
diffing per-channel counts against a `last_seen` map; reset when the channel
is selected). Persisting it into `WorkspacePrefs` (`molt-core/src/lib.rs:796`,
the documented home for node-local state) is a **stretch package (B5)**, not
first-iteration scope — it needs new core+storage+engine plumbing that would
widen the conflict surface.

---

## 2. The dependency truth, and where the parallelism actually is

- Everything depends on the `molt-core` contract (`ChatMessage`, `Command`,
  `Event`, `SurfaceSnapshot`, `ChannelRef`, `MessageId`).
- Adding fields breaks **every** struct literal and every index-based call
  site across 6 crates at once — the workspace is red until all ~30
  inventoried sites are touched. Splitting "core" from "engine compile-fixes"
  across agents just serializes them with extra merge pain.
- `molt-ui` compiles only when `molt-engine` compiles (dependency chain), so
  UI work cannot validate before the engine is green.

**Therefore:** one serial Stage A that lands the contract *and* the mechanical
propagation, leaving the whole workspace green (ids exist and are addressable,
channels exist as data, everything else behaves exactly as today). After that,
four genuinely independent packages run in parallel with disjoint file
ownership, plus a serial integration stage. The critical path is
A → B1 → C; B2/B3/B4 run inside B1's shadow.

```
A (contract + mechanical propagation, 1 agent, serial)
├── B1 wire semantics & legacy ingest   (engine: net.rs, events.rs + tests)
├── B2 read filter & channel enumeration (engine: proposals.rs + tests)
├── B3 MCP surface                       (molt-mcp only)
├── B4 UI channels                       (molt-ui only)
└── B5 (stretch) unread persistence      (core prefs + storage + ui)
C (integration, merge order B1→B2→B3→B4, full gates, 1 agent, serial)
```

### File-ownership matrix (Stage B — no file appears twice)

| File | A | B1 | B2 | B3 | B4 |
|------|---|----|----|----|----|
| `molt-core/src/lib.rs` | ✍ full | — | — | — | — |
| `molt-engine/src/chat.rs` | ✍ handlers→id | — | — | — | — |
| `molt-engine/src/events.rs` | ✍ mechanical | ✍ ingest/synthesis/apply-by-id + tests | — | — | — |
| `molt-engine/src/net.rs` | — | ✍ crosses_wire, cmd_net_delivered, parking | — | — | — |
| `molt-engine/src/proposals.rs` | — | — | ✍ filter+channels | — | — |
| `molt-engine/src/lib.rs` (dispatch + mod tests) | ✍ | — | — | — | — |
| `molt-engine/tests/two_instances.rs` | — | ✍ new scenarios | — | — | — |
| `molt-engine/tests/{persisted_mesh,demo_mesh,common}` | ✍ mechanical | ✍ legacy-log fixture | — | — | — |
| `molt-mcp/src/lib.rs` | ✍ mechanical (id params compile) | — | — | ✍ schemas/tests | — |
| `molt-ui/src/lib.rs` + `ui/*.slint` | ✍ mechanical (row→id) | — | — | — | ✍ full |
| `molt-storage` (tests, mkdummy) | ✍ mechanical | — | — | — | — |
| `molt-net` (tests chat_env) | ✍ mechanical | — | — | — | — |

B2's only cross-file need — the `ChannelInfo` type and the `ReadState.channel`
field — is frozen in A, so B2 never touches core. B3 edits only the `ToolDef`
schema/build closures A already made compile. If B1 and A's `events.rs` split
feels risky, fold A's mechanical `events.rs` edits forward into B1's brief —
the matrix's point is that no two *Stage-B* agents share a file.

---

## 3. Stage A — contract freeze + mechanical propagation (serial, 1 agent)

**Goal:** the full new contract exists; the workspace builds, clippy is 0, all
existing tests pass (mechanically migrated); behavior is unchanged except that
messages now carry minted ids/channels and local addressing is by id.

**Failing tests to write first** (in `molt-core` mod tests + engine mod tests):
1. `message_id_round_trips_as_hex_and_rejects_bad_input`
2. `channel_ref_serdes_by_kind_tag_and_group_serializes_to_nothing` —
   a `Group` message's JSON is byte-identical to a pre-change fixture string.
3. `legacy_chat_json_without_id_or_channel_still_decodes` — fixture JSON from
   today's wire shape (incl. numeric `quote`) decodes; `channel == Group`.
4. `chat_commands_address_by_id` — engine test: send, react by id, delete by
   id, unknown id → `UnknownMessage`, quote by id survives in the log.
5. `every_new_message_gets_a_unique_nonnil_id` (N sends, all distinct).

**Work list** (anchors from the inventory):
- `molt-core/src/lib.rs`: `MessageId` (+`NIL`), `ChannelRef` (P3),
  `ChatMessage` fields (P2) + constructor signature `text(id, from, body, ts)`
  (`:661`); `ChannelInfo` + `SurfaceSnapshot.channels` (`:2119`, P7);
  `Command` swaps (`:1585-1703`, P2); `Event` swaps (`:2188-2217`);
  `WorkspaceEvent` additive `id` on `ChatReacted/ChatDeleted/FileRemoved`
  (`:1096-1118`); `MoltError` id-typed (`:2302-2311`); update the wire-shape
  doc comments (`:674-676`, `:686-688`).
- `molt-engine`: mint ids in `post_message`/`cmd_share_file`
  (`chat.rs:36,69`, getrandom); handlers `cmd_react_chat`/`cmd_delete_chat`/
  `cmd_download_file`/`cmd_remove_file` switch to id lookup via the id→pos map
  (P5) replacing `check_chat_index` (`chat.rs:100-191`); dispatch arms
  (`lib.rs:539-549`); quote validation becomes "id known" (`chat.rs:35`);
  `apply` writes the map on `Chat` push (`events.rs:112`), and
  `ChatReacted/ChatDeleted/FileRemoved` prefer `id` with `index` fallback
  (`events.rs:115-152`); emit id-carrying `Event`s (`chat.rs:137,161,181`).
  Demo brain (`net.rs:836`) sends `channel: Group`.
- Mechanical propagation, every site from the inventory: `ChatMessage` full
  literals (`chat.rs:36,69`, `events.rs:315`, `molt-storage/src/lib.rs:1756`,
  `molt-net/tests/mls_supervisor.rs:54`, `mesh_bootstrap.rs:51`) and `::text`
  callers (`two_instances.rs:714`, `persisted_mesh.rs:67`,
  `molt-net/tests/supervisor.rs:131`, `mkdummy.rs:18,77`); `Command::Chat`
  literals (`molt-mcp:341`, `molt-ui:598`, `net.rs:836`, engine tests
  `lib.rs:807-1142`, `two_instances.rs:824`, `demo_mesh.rs:23,66,99`); MCP
  build closures compile against id params (schemas stay for B3 to finalize);
  UI: `LogLineData`/`LogLine` gain `id: string`, callbacks map row→id
  (`molt-ui/src/lib.rs:592-694`, `app.slint` quote/delete/react/file plumbing
  `:2493-2527`, `mv-quote-index` becomes an id + kept row for scroll), engine
  unit tests in `lib.rs:771-1206` and `events.rs:307-477` migrated to
  id addressing (keeping their assertions' meaning), `mkdummy` hand-computed
  indices (`:67,98`) replaced with ids.

**Gate:** `cargo clippy --all-targets` 0; `cargo test` green;
`cargo build -p molt-ui` green. Commit to the integration branch; tag the
commit for Stage-B branch-off.

---

## 4. Stage B — four parallel packages (one agent each, own worktree)

### B1 — wire semantics & legacy ingest (the critical-path package)

**Goal:** reactions/deletes/file-removes cross the wire and converge;
duplicates rejected; legacy logs replay identically; quotes survive the wire.

Failing tests first:
1. `legacy_log_replay_synthesizes_stable_ids` — fixture log written in the
   OLD format (pre-id JSON, numeric quotes): replay twice → identical dumps;
   snapshot@k+tail == full replay (extends `events.rs:440`); legacy `quote`
   resolved to the right `quote_id`.
2. `reactions_and_deletes_converge_across_two_instances` — in
   `two_instances.rs`, after `founding_chats_over_the_direct_mesh`'s pattern:
   founder reacts → member's `RecordSink` sees `ChatReacted{id}` and member
   state converges; member deletes own message → founder shows tombstone.
3. `a_reaction_arriving_before_its_message_is_parked_and_applied` — loopback
   `ChaosPolicy { delay_ms: (0,30), duplicate_pct: 20, drop_pct: 20 }` (the
   `supervisor.rs:172` pattern, seeds [3,17,40961]): inject reaction before
   message; final reaction sets equal on all nodes.
4. `a_duplicate_message_id_is_ignored_and_a_foreign_delete_is_rejected` —
   replayed `Chat` with a seen id doesn't duplicate; `ChatDeleted` from a
   non-author link is dropped; `FileRemoved` from a non-sharer is dropped.
5. `a_wire_quote_resolves_on_the_receiver` — quoted message renders (quote_id
   kept, `net.rs:588` behavior replaced).

Work: `crosses_wire` += `ChatReacted|ChatDeleted|FileRemoved` (`net.rs:203`);
`cmd_net_delivered` arms per P5 (`net.rs:585-624`); parking buffer per P6;
ingest synthesis per P4 (`events.rs:113,263`); the `wants` path is untouched
(`net.rs:271`). Files owned: `net.rs`, `events.rs` (+ its mod tests),
`two_instances.rs`, `persisted_mesh.rs` fixture.

### B2 — read filter & channel enumeration

Failing tests first:
1. `filtered_read_equals_client_side_filter_of_full_read` (property over a
   mixed-channel log — the concept's Phase-3 acceptance test).
2. `channels_enumerates_distinct_refs_with_counts_and_group_is_always_present`
3. `filter_by_unknown_patch_id_returns_empty_not_error`
4. `status_counts_are_unchanged_by_filtering` (`proposals.rs:228` semantics).

Work: `applied_values`/`snapshot` grow the channel parameter
(`proposals.rs:195-226`), `channels` built in one pass; dispatch threads
`ReadState.channel` (`molt-engine/src/lib.rs:555`). Tests live in a new
`molt-engine/tests/read_filter.rs` (do NOT edit `lib.rs` mod tests — A owns
that file's test block, B2 must not conflict). Files owned: `proposals.rs`,
new test file.

### B3 — MCP surface

Failing tests first (mod tests in `molt-mcp`):
1. `chat_send_accepts_channel_and_quote_id` — schema exposes
   `channel: {kind, id?, name?}` + `quote` (hex id string); build maps to
   `Command::Chat`.
2. `read_state_accepts_a_channel_filter`
3. `react_delete_download_remove_take_hex_ids` (+ malformed id → clean error,
   not panic).
4. `co_equality_every_command_is_a_tool_or_documented_internal` stays green
   **unchanged** — field additions touch no command names (mcp `:821-870`;
   INTERNAL array length untouched).

Work: `tools()` `ToolDef` schema/build closures for `chat_send` (`:329`),
`react_chat` (`:347`), `download_file`/`remove_file`/`delete_chat`
(`:384-422`), `read_state` (`:466`); a `channel_arg` helper mirroring
`surface_arg` (`:244`); tool descriptions document the id addressing. File
owned: `molt-mcp/src/lib.rs` only.

### B4 — UI channels (the biggest package)

Failing/compile-gated checks first: `cargo build -p molt-ui` is the Slint
compile gate (no GUI on `DISPLAY=:0`); Rust-side unit tests for the pure
functions:
1. `derive_channels_lists_group_first_then_patches_then_topics_with_unread`
2. `annotate_chat_log_resolves_quotes_by_id` (rewrite of `:2062-2092` tests)
3. `system_lines_interleave_by_time_and_tolerate_unknown_proposals` (P8)
4. `unread_counts_reset_on_channel_selection` (P9)

Work, all in `molt-ui`:
- Channel model: derive from `SurfaceSnapshot.channels` (B2's shape, frozen in
  A/P7) + selected-channel state. Selection is **UI-local** for v1 (like
  `nav-collapsed`, `app.slint:204`) — NOT `SessionView` (avoids a core/session
  change; co-equality is preserved because filtering itself is engine-side and
  MCP agents pass their own filter). Note this in the code.
- Sidebar: reuse the `ViewRow` idiom (`parts.slint:179-229`) under the chat
  surface accordion (`app.slint:630-659`): Group, then patch channels
  (lazy titles via `summarize`, fallback `#id`), then topics; `PendingBadge`
  (`parts.slint:67`) as unread pill. New-topic affordance = a small compose
  control (send-to-new-topic), not a "create channel" ritual (a channel exists
  because a message exists — concept Q2).
- Chat pane = filter: pass the selected channel into the `ReadState` call in
  `push_surfaces` (`lib.rs:1317-1338`) or filter in `surface_data` off the
  full log — **use the engine filter** (that's the point); re-read on channel
  switch. Compose sends `Command::Chat { channel: selected }`; quote state
  switches to id (`mv-quote-index` plumbing `app.slint:211-214, 2510-2516,
  2549-2640`).
- System lines (P8): synthesize `LogLine`s (empty `lead`, styled) for
  `Patch(id)` channels from pending/applied proposal data + `Approved`
  progress; merge-sort by timestamp with the chat lines (proposals lack a ts —
  use first-seen time recorded UI-side; document the approximation).
- Unread (P9): per-channel counts in `surface_data`; badge on sidebar rows;
  reset on select. `Event::Chat{id, channel}` (from A) lets the handler know
  a foreign-channel message arrived without a full diff.
- Strings: every new label via `lexicon!` EN/DE pairs (`lib.rs:1789-2035`) +
  `Strings` global (`theme.slint:248+`).

Files owned: `molt-ui/src/lib.rs`, `ui/app.slint`, `ui/parts.slint`,
`ui/theme.slint`, `ui/components.slint`.

### B5 (stretch, only if B1–B4 land clean) — unread persistence

`WorkspacePrefs` gains `#[serde(default)] channel_reads: BTreeMap<String, u64>`
(channel key → last-read ts) (`molt-core/src/lib.rs:796`,
`molt-storage` `read_prefs`/`set_prefs` `:568,577,729`); UI loads/saves on
select/close. Separate because it crosses core+storage+engine+ui — run it
serial after C, not inside Stage B.

---

## 5. Stage C — integration (serial, 1 agent)

1. Merge order: **B1 → B2 → B3 → B4** (each merge: rebase, run
   `cargo clippy --all-targets` + `cargo test`; fix drift at the seam, don't
   re-architect).
2. Cross-package integration test (new, in `two_instances.rs` or a new
   `chat_channels.rs`): founder proposes (chain-governed workspace) → both
   sides chat in `Patch(id)` and `Group` → filtered reads match on both
   instances → member reacts in the patch channel → converges → a filtered
   `ReadState` over MCP-built commands returns the same as the UI path.
3. Optional but recommended: one run of the real-SMP suite
   (`cargo test -p molt-engine --test ritual_engine_over_smp -- --ignored`)
   since MLS-path ordering differs from loopback (reorder-buffer bypass).
4. `cargo build -p molt-ui` final; full `cargo build`.
5. Docs: update `chat_bus.md` status (phases → implemented); extend the
   CLAUDE.md conventions block if a new invariant emerged (e.g. "chat
   addressing is by MessageId; never reintroduce indices"); update
   `concept-workspace-storage.md` only if snapshots changed shape (they
   shouldn't — additive).
6. `/code-review` on the full diff; fix findings; PR against master.

---

## 6. Orchestration mechanics

- **Stages are separate runs, human between them.** A → review → fan out
  B1–B4 → C. The Stage-A diff is the contract everyone builds on; it deserves
  a human look before four agents amplify it.
- Stage B agents: one worktree each (isolated branches off the Stage-A
  commit), each with its package brief = this document §4 + the file-ownership
  row + the design pins §1. Brief them to: write the listed failing tests
  first, watch them fail, implement, keep clippy at 0, commit at checkpoints,
  and **stop and report** (not improvise) if they need a file another package
  owns or a pin seems wrong.
- Concurrency: B1–B4 genuinely concurrent (disjoint files). B2/B3 are small —
  expect them to finish first; they wait for C, no rolling merges into each
  other.
- Every agent runs `cargo clippy --all-targets` and the relevant
  `cargo test -p …` before declaring done; C runs the full suite.

## 7. Risks & watch items

- **Recovery-WIP collision** (§0) — the one sequencing hazard that produces
  real merge conflicts. Do not skip.
- **Slow first build per worktree** — each fresh worktree pays the full
  workspace build (Slint + OpenMLS). Consider a shared `CARGO_TARGET_DIR` or
  warm the caches before fanning out; otherwise B-agents spend their first
  20+ minutes compiling.
- **Determinism keystones** (`events.rs:414,440`, `molt-storage:1907`): any
  P4 mistake shows up here — treat a red keystone as a design stop, not a
  test to adjust.
- **Old-log fixtures**: B1's legacy fixture must be captured from *today's*
  serialization (commit the JSON as a test asset) — not regenerated with new
  code, or the test proves nothing.
- **MLS vs loopback ordering**: loopback chaos reorders differently than the
  MLS path (which drops rather than buffers out-of-order) — hence the parking
  buffer (P6) and the optional live-SMP run in C.
- **`Event::Chat` consumers**: adding fields to `Event` variants is a
  non-persisted change, but the UI event loop treats `Ok(_)` uniformly
  (`molt-ui/src/lib.rs:770`) — B4 must not accidentally start depending on
  event payloads for correctness (re-read stays the source of truth).
- **clippy traps** in new code: no `as` casts (`as_conversions`), no
  `.unwrap()` anywhere including tests, `missing_docs` on new public items
  (`MessageId`, `ChannelRef`, `ChannelInfo`).
