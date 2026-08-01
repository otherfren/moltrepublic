# Plan: per-message read receipts ("Lesebestätigung")

Status: **BUILT** (2026-07-19, `c618a8c`; dots restricted to own sent messages
in `35260b5`). ARCHIVED — the plan was executed. Was: PLANNED, execution-ready,
test-first. Read `docs/chat/chat_bus.md` first (this rides the chat/reaction
machinery); read `docs/chain/persistent_chain.md` to confirm the ephemerality
boundary this respects.

## 0. What the feature is

Every chat message shows, in its **bottom-right corner**, a small per-member
row: **one icon per other member — yellow while that member has not yet read the
message, turning green when their read confirmation arrives.**

Nothing like this exists today. Presence ("Präsenz") is deliberately
**beaconless / runtime-only / never on the wire** (`concept-transport-simplex-tor.md:415`),
so it cannot back a per-message "who read this". A read receipt is therefore a
**new wire-crossing, converging event modeled exactly on reactions** — the
proven pattern: id-addressed by `MessageId`, admitted by `crosses_wire`,
out-of-order-tolerant via the "parking" buffer, persisted to the local encrypted
log, and **never chained**.

## 1. Decisions (agreed 2026-07-19)

| # | Fork | Decision |
|---|------|----------|
| D1 | Wire model | **Per-message receipts** — `WorkspaceEvent::ChatRead { ids, by }`, batched; `ChatMessage.read_by: BTreeSet<MemberId>`; converges like reactions with parking. (Rejected: per-member read-cursor/watermark — fragile under the bus's per-sender-only ordering.) |
| D2 | "Read" trigger | **Channel opened/focused** — activating a channel while the window is focused marks every currently-loaded, not-mine, not-yet-read-by-me message in it as read, in one batched call. (Rejected: per-row viewport tracking — more precise, more complex.) |
| D3 | Privacy | **On by default + local off switch** — a per-node preference `read_receipts_enabled` (same layer as the S3 auto-backup master switch); **symmetric**: disabling sending also hides others' receipts from you. |
| D4 | UI placement | Receipt row lives in the **bottom-right corner** of the message bubble, as the last right-aligned row of the content flow (below the reaction row) — so it adds to the card height and never overlaps short messages. |

## 2. The three stores — where a receipt does and does not live

Conflating these is the trap. A read receipt is ephemeral like chat/reactions,
and the toggle is a plain local preference.

| Store | Chat / Reactions | Presence | **ChatRead receipt** | Toggle |
|---|---|---|---|---|
| Persistent chain (blocks) | NO | NO | **NO** — ephemeral (`persistent_chain.md:46-59`); structurally impossible (the chain folds only `ChainChange::Applied`, `chain.rs`) | NO |
| Wire (`crosses_wire`, `net.rs:356`) | YES | NO | **YES** — add the variant to the `matches!` | NO |
| Local encrypted log (`record()`→append, `events.rs:73`) | YES | NO | **YES** — via `record()`, exactly like a reaction; replayed on restart; ages out with chat retention | NO |
| Local `config.toml` (`SessionSettings`) | — | — | — | **`read_receipts_enabled: bool`** (default true) |

## 3. Core model changes (`molt-core`)

### 3.1 `ChatMessage.read_by` — mirror `reactions`
`crates/molt-core/src/lib.rs:1106-1145`:
```rust
/// Members (never the author) who have confirmed reading this message.
/// Monotonic (insert-only; there is no "un-read"). Bounded by roster size.
#[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
pub read_by: BTreeSet<MemberId>,
```
- `skip_serializing_if` keeps the **chat byte-identity fixtures green**: an empty
  `read_by` emits no bytes, so the legacy wire shape is byte-identical. CLAUDE.md
  flags those fixtures as a *design stop* — a new fixture is added for the
  populated case; the empty case must stay unchanged.
- Update the `ChatMessage::text` constructor (`:1065`) to default `read_by`
  empty; `with_channel`/`with_kind` builders unaffected.

### 3.2 New event `WorkspaceEvent::ChatRead` (additive-only)
`crates/molt-core/src/lib.rs:1563` enum:
```rust
/// A member confirmed reading these messages. Batched: one event carries a
/// whole channel-open's worth of ids. `by` is bound to the authenticated link
/// identity on the wire (like `ChatReacted.by`). No `op` — reads don't toggle.
ChatRead { ids: Vec<MessageId>, by: MemberId },
```

### 3.3 New commands (`Command` enum, `crates/molt-core/src/lib.rs:2287`)
```rust
MarkRead { ids: Vec<MessageId> },   // co-equal TOOL -> "mark_read"
SetReadReceipts { enabled: bool },  // co-equal TOOL -> "set_read_receipts"
```

### 3.4 `SessionSettings.read_receipts_enabled: bool` (default **true**)
Node-level `config.toml` layer (same as `s3_backup`). Sites (all pinned):
- `SessionSettings` struct `molt-core/src/lib.rs:219`, default `:296`.
- `SessionView` already exposes the nested `settings` (`:2224`) — no extra
  top-level field needed; the UI reads `settings.read_receipts_enabled`.
- `molt-config`: `Settings` (`:352`, default `:400`), `StorageConfig`
  (`:58` with `#[serde(default)]`, default `:103`), `From<&Config>` (`:451`),
  `render` (`:501`/arg `:560`), `salvage` (`:602`), `apply` (`:784`).
- `molt-engine/src/configstore.rs`: `file_settings` (`:332`), `session_settings`
  (`:361`).
- **Four default sites must read absence as `true`**: `SessionSettings::default`,
  `Settings::default`, `StorageConfig::default`, and the field's
  `#[serde(default = "…")]`.
- **Do not** add it to `mark_restart_required` (`session.rs:478`) — it is a hot
  pref, no restart.

### 3.5 Operator event (optional) `Event::Read { ids, by }`
Next to `Event::Reacted` (`:3752`), so the operator log / MCP event stream can
observe receipts. Emit only for the newly-recorded ids.

## 4. Engine — send path (`molt-engine`)

### 4.1 `cmd_mark_read` — new, in `chat.rs`, mirror `cmd_react_chat` (`chat.rs:381`)
```
if !self.session.settings.read_receipts_enabled { return Ok(Reply::Ack); } // D3: send nothing
let me = self.member();
let fresh: Vec<MessageId> = ids.into_iter().filter(|id| match self.chat_by_id(id) {
    Ok((_, m)) => m.deleted_by.is_none() && m.kind == ChatKind::User
                  && m.from != me && !m.read_by.contains(&me),
    Err(_)     => false,          // unknown id: ignore (GUI won't pass one; MCP might)
}).collect();
if fresh.is_empty() { return Ok(Reply::Ack); } // idempotent: repeat channel-open is a no-op
self.record_read(fresh.clone(), me.clone());   // apply + append + broadcast (self-authored => wire)
self.emit(Event::Read { ids: fresh, by: me });
```

### 4.2 `apply` arm for `ChatRead` — `events.rs`, next to the `ChatReacted` arm (`:170`)
```
WorkspaceEvent::ChatRead { ids, by } => {
    for id in ids {
        let Some(m) = self.chat_target(Some(id), 0) else { continue };  // unknown id ignored
        if m.deleted_by.is_some() { continue }   // no receipts on a tombstone (commute)
        if m.from == *by         { continue }    // author never receipts own message
        m.read_by.insert(by.clone());            // idempotent set
    }
}
```

### 4.3 `ChatDeleted` apply (`events.rs:208`)
Extend the existing "wipe body / clear reactions / drop file" step to **also
clear `read_by`** — a tombstone carries no receipts (keeps the commute with 4.2).

### 4.4 `cmd_set_read_receipts` — copy `cmd_set_theme` (`session.rs:82`)
```
self.session.settings.read_receipts_enabled = enabled;
self.persist_settings(false);          // silent persist to config.toml
self.emit_session(SessionScope::Full);
Ok(Reply::Ack)
```

### 4.5 Shared helper `record_read(ids, by)`
Builds `WorkspaceEvent::ChatRead { ids, by }` and calls `record()` (apply +
local append + net publish). Used by both `cmd_mark_read` (by = me → crosses the
wire) and the inbound wire path (by = peer → appended locally, **not**
re-broadcast: the outbox `wants` gate is `env.by == self.context.0`, `net.rs:441`).

## 5. Engine — wire path (`molt-engine/net.rs`)

### 5.1 `crosses_wire`
Add `WorkspaceEvent::ChatRead { .. }` to the `matches!` (`net.rs:356`).

### 5.2 Receive arm in `cmd_net_delivered` (next to the `ChatReacted` arm, `net.rs:914`)
**Force `by = from`** (the authenticated link identity — a forged `by` cannot
inject another member's receipt):
```
WorkspaceEvent::ChatRead { ids, .. } => {
    let mut known = vec![];
    for id in ids {
        match self.chat_by_id(&id) {
            Ok((_, m)) if m.deleted_by.is_none() && m.from != from => known.push(id),
            Ok(_)  => {}                                        // tombstone / own: skip (commute)
            Err(_) => self.parked.park(id, PendingRef::Read { by: from.clone() }),
        }
    }
    if !known.is_empty() { self.record_read(known, from); }
}
```

### 5.3 `PendingRef::Read { by: MemberId }` (`net.rs:95`)
New parking variant, same bounded-FIFO caps as `React`/`Delete`/`FileRemove`
(`PARKED_TARGET_CAP` / `PARKED_REFS_PER_TARGET`).

### 5.4 Drain — extend the parked-ref loop in the `Chat` arm (`net.rs:898`)
```
PendingRef::Read { by } => self.wire_read(id, by),
```
`wire_read(id, by)` re-checks tombstone/self and calls `record_read(vec![id], by)`.

## 6. Co-equality + MCP (`molt-mcp`)

- `mark_read` `ToolDef` (schema `{ "ids": { "type": "array", "items": {"type":"string"} } }`,
  required `["ids"]`) → `Command::MarkRead` — clone the `set_workspace_backup`
  shape (`mcp/src/lib.rs:858`).
- `set_read_receipts` `ToolDef` (`{ "enabled": {"type":"boolean"} }`, required
  `["enabled"]`) → `Command::SetReadReceipts` — clone the `set_theme` shape.
- **No `INTERNAL` edits**: inbound peer receipts ride the existing `NetDelivered`
  (already INTERNAL). Adding two tools auto-satisfies
  `co_equality_every_command_is_a_tool_or_documented_internal` (`mcp/src/lib.rs:1128`).
- `read_state` already serializes each `ChatMessage` whole into `applied`, so
  **`read_by` reaches MCP agents for free**; the roster comes from `read_members`.
  Reading via `read_state` is side-effect-free — marking read is the explicit
  `mark_read` call, no hidden coupling.

## 7. UI (`molt-ui` + `molt-ui-window`)

### 7.1 New Slint types
`crates/molt-ui-window/ui/theme.slint` (next to `ReactionItem` `:20`):
```
struct ReceiptItem { name: string, read: bool }
// LogLine (theme.slint:28) gains:  receipts: [ReceiptItem]
```
`crates/molt-ui-window/ui/app.slint` config-draft block (`:856`) gains:
```
in-out property <bool> cfg-read-receipts: true;
```

### 7.2 Rust ⇄ Slint bridge (`crates/molt-ui/src/lib.rs`)
- In `chat_line` (`:3634`, where `reactions` are built `:3638`) compute
  `receipts` from `msg.read_by` × the roster: for each member `X != msg.from`,
  push `{ name: X, read: msg.read_by.contains(X) }`. The local member appears
  green (it was marked on view). **Symmetric hide (D3):** if the local
  `read_receipts_enabled` is false, emit `receipts = []`.
  (`chat_line` needs the roster name list — thread it in from the same source
  that feeds `active-members`/`ReadMembers`; the local `me` is already threaded.)
- Map `LogLineData.receipts → Vec<ReceiptItem>` in the `LogLine` construction
  (`:3157-3194`, mirror the reactions map at `:3160-3168`).
- `cfg-read-receipts` follows the `s3_backup` trail: `set_cfg_read_receipts` in
  `apply_settings_fields` (`:2260`) reading `settings.read_receipts_enabled`; a
  dedicated toggle callback → `Command::SetReadReceipts` (like `on_set_language`
  `:224`), or via `read_settings_draft` (`:1947`) + `save_settings`.

### 7.3 The receipt row in `ChatRow` (bottom-right corner, D4)
`crates/molt-ui-window/ui/parts.slint`, inside `crow` (`:418-614`) as the **last**
child (after the reaction row `:570-613`), right-aligned:
```
if root.line.receipts.length > 0 && !root.line.system && root.line.id != "" && root.receipts-on:
HorizontalLayout {
    alignment: end;            // push to the right edge -> bottom-right corner
    spacing: 3px;
    for r in root.line.receipts: Rectangle {
        width: 8px; height: 8px; border-radius: 4px;
        background: r.read ? Theme.good : Theme.warn;   // green / amber (theme.slint:331-332)
        ta := TouchArea {}     // hover -> HintTip global showing r.name
    }
}
```
- Being the last row **inside `crow`**, it grows `card.preferred-height` and sits
  in the true bottom-right corner — **no overlap** (the top-right action buttons
  are absolutely positioned and do overlap short messages; the receipt row
  deliberately does not, per D4).
- Per-dot member-name tooltip via the **HintTip global + app-root overlay** —
  **never a `PopupWindow`** (the `has-hover` pointer-grab strobe trap is a
  recorded bug: `slint-popup-hover-strobe-and-drag-traps`).
- Colors: `Theme.good` = green, `Theme.warn` = amber/yellow — the exact
  presence-dot idiom (`parts.slint:784`, `app.slint:4432`).
- Add a `receipts-on: bool` in-property on `ChatRow` wired from the window's
  session flag (the local toggle), so D3's symmetric hide is a single gate.

### 7.4 The D2 trigger (`crates/molt-ui/src/lib.rs`)
A `mark_channel_read(channel)` helper collects the ids of currently-loaded
messages in the **active** channel where `!own && id != "" && !system`, and
issues **one** `Command::MarkRead { ids }`. Fire it on:
1. a channel becomes active,
2. the window regains focus while a channel is active,
3. a new message arrives while the channel is active + focused.

`cmd_mark_read`'s freshness filter (4.1) makes over-firing a no-op, so no
debounce is strictly required.

### 7.5 Settings UI
An `AppCheck` "Send read receipts" bound to `cfg-read-receipts` — clone the
`s3_backup` checkbox (`app.slint:5531`, `components.slint:338`).

### 7.6 Validation
`cargo build -p molt-ui-window -p molt-ui` (Slint compiler + logic against the
generated API). **No GUI on `DISPLAY=:0`.** Iterate with `scripts/dev-ui.sh`.

## 8. Test plan (TDD — write the red test first, then implement)

1. **Byte-identity** — empty `read_by` serializes to nothing; the legacy
   `ChatMessage` byte-identity fixtures stay green; a new fixture round-trips
   `read_by = {A, B}`.
2. **`cmd_mark_read`** — idempotent (second call emits nothing); own messages
   never receipted; `enabled = false` emits nothing; system lines skipped.
3. **Convergence over loopback** — two instances
   (`crates/molt-engine/tests/two_instances.rs` style): B opens the channel →
   `B ∈ read_by` of A's message on both nodes.
4. **Parking / reorder** — a `ChatRead` delivered *before* its `Chat` parks,
   then drains on `Chat` arrival and converges (mirror the reaction reorder
   test; `--test-threads=32` is the stress lever).
5. **Auth** — an inbound `ChatRead` with a spoofed `by` is overridden to the
   link identity; no cross-member receipt injection.
6. **Tombstone commute** — a receipt targeting a deleted message is dropped;
   deleting a message clears its `read_by`.
7. **Retention** — a receipt on an aged-out message is not surfaced (rides the
   existing chat-retention read filter; no separate handling).
8. **Legacy synthetic-id** — a `ChatRead` addressing a pre-id (synthetic-id)
   message converges (synthetic ids are cross-node stable).
9. **Parking caps** — `PendingRef::Read` respects the bounded FIFO (no unbounded
   growth).
10. **Co-equality** — `co_equality_every_command_is_a_tool_or_documented_internal`
    stays green with the two new tools.

## 9. Phased execution

- **P1 core** (§3) — model / event / commands / settings field; test 1 red→green.
- **P2 engine send** (§4) — `cmd_mark_read`, `apply` arm, tombstone clear, toggle
  handler; tests 2, 6, 7.
- **P3 engine wire** (§5) — `crosses_wire`, receive arm, `PendingRef::Read`,
  drain; tests 3, 4, 8, 9.
- **P4 MCP** (§6) — two tools; test 10.
- **P5 UI** (§7) — Slint types, bridge, receipt row (bottom-right), trigger,
  settings toggle; `cargo build -p molt-ui-window -p molt-ui`.
- **P6 land** — `/code-review` the diff, fix findings, green on master (house
  rule). `cargo clippy --all-targets` clean (`.expect(...)` in tests, never
  `.unwrap()`).

## 10. Invariants respected · non-goals · known limits

- **Ephemeral, never chained** — `ChatRead` is a plain `WorkspaceEvent`; the
  chain folds only `ChainChange::Applied`. No `roster_canonical_bytes` /
  `molt-chain-*` byte-layout bump (this touches neither).
- **Additive-only** — `read_by` and `ChatRead` are `#[serde(default)]` / a new
  variant; an old reader dropping the field just shows no receipts (the accepted
  Q3-style degradation).
- **No un-read** — monotonic yellow→green; a read never reverts.
- **Metadata cost is real** — receipts reveal who-read-what-when, a departure
  from the beaconless posture; the **local off switch (D3)** is the mitigation.
  Volume is O(msgs × members) but batched per channel-open (a known property,
  like `chat_bus.md` Q6 head-of-line — revisit only if real usage shows strain).
- **Id-squatting** inherited from chat (first-wins dedup) — receipts key by the
  **authenticated** identity, so no forgery; same posture as reactions
  (`chat_bus.md` "Known v1 limitations").
- **Coarse "read" (D2)** — channel-open marks messages read even if the member
  never scrolled to them; a documented overstatement.
- **Late joiners** — a member who joined after a message was posted (never
  received it; chat is ephemeral) shows a perpetual yellow dot for it. Accepted
  and documented; not worth per-message roster snapshots.
- **Total-loss rejoiner** — no receipt backfill (chat itself is not backfilled;
  `chat_bus.md` Q4).

## 11. File-touch checklist

- `crates/molt-core/src/lib.rs` — `ChatMessage.read_by`, `WorkspaceEvent::ChatRead`,
  `Command::{MarkRead, SetReadReceipts}`, `SessionSettings.read_receipts_enabled`,
  `Event::Read`.
- `crates/molt-config/src/lib.rs` — `Settings` / `StorageConfig` / `From` /
  `render` / `salvage` / `apply` + three `Default` sites.
- `crates/molt-engine/src/chat.rs` — `cmd_mark_read`, `record_read`.
- `crates/molt-engine/src/events.rs` — `ChatRead` apply arm; `ChatDeleted` clears
  `read_by`.
- `crates/molt-engine/src/net.rs` — `crosses_wire`, receive arm, `PendingRef::Read`,
  `wire_read`, drain.
- `crates/molt-engine/src/session.rs` — `cmd_set_read_receipts`.
- `crates/molt-engine/src/configstore.rs` — `file_settings` / `session_settings`.
- `crates/molt-engine/src/lib.rs` — command dispatch for the two new commands.
- `crates/molt-mcp/src/lib.rs` — `mark_read` + `set_read_receipts` tools;
  co-equality test stays green.
- `crates/molt-ui/src/lib.rs` — `chat_line` receipts, `LogLine` map, settings
  bridge, `mark_channel_read` trigger.
- `crates/molt-ui-window/ui/{theme.slint, parts.slint, app.slint, components.slint}`
  — `ReceiptItem`, `LogLine.receipts`, `ChatRow` receipt row, `cfg-read-receipts`
  + settings checkbox.
- Tests — `molt-core` mod tests (byte-identity, fixtures), a new
  `crates/molt-engine/tests/*` for two-instance convergence + reorder/parking +
  auth.
