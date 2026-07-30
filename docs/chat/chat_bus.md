# Concept: the chat bus — one ephemeral broadcast, channels as filters

Status: **IMPLEMENTED 2026-07-11** — all four phases are real (stable
`MessageId`s + id addressing everywhere, `ChannelRef` on the wire,
engine-side filter + channel enumeration on `ReadState`, UI channel
sidebar with unread counts and interleaved governance system lines;
reactions/deletes/file-removes now cross the wire, converge, and park
when they outrun their target). Executed per
`chat_bus_implementation.md` (stages A, B1–B4, C); B5 (persisted unread)
remains the stretch package. The decision log below is unchanged history.

## The idea

Group chat has two views:

- **UI level:** several "channels" (the all-hands group chat, a discussion
  thread per git patch, …), each its own chat window.
- **Technical level:** there is exactly **one broadcast channel** per republic.
  Every message carries a **channel tag**; a chat window is nothing but a
  *filter* over the one stream.

Examples: all-hands messages are tagged `@groupchat`; discussion about a patch
is tagged `@patchdiscussion` plus the patch/proposal id. Everything ephemeral
goes onto the bus; the UI only filters.

## Status quo: the bus already exists

This is the load-bearing observation — the proposal is *not* an architecture
change, it is the honest continuation of what the transport already does:

- There is **one MLS group per republic**. Every event that crosses the wire is
  the same serialized `EventEnvelope`, MLS-encrypted **once per log seq** and
  fanned out over the per-pair queue mesh (`molt-net/src/supervisor.rs`,
  `MlsChannel::ciphertext_for`). The per-pair queues exist for unlinkability,
  not for separating message kinds.
- Message kinds are **not** multiplexed by queue or topic. The discriminator is
  the serde `type` tag of the `WorkspaceEvent` inside the envelope. Chat,
  `Proposed`, `Approved`, `Committed`, `ChainRequest`, `MembershipProposed`,
  `MlsCommit` all ride the same stream (`crosses_wire`,
  `molt-engine/src/net.rs`).
- The UI already renders chat as **one flat list** filtered client-side
  (sub-views "today"/"archive"); there are no rooms/threads today.

So "one bus + tags + filters" matches reality one level down. The proposal is
one level up: **make human chat itself a tagged space** instead of introducing
structural channels (separate queues, separate MLS groups, separate logs). That
alternative — per-channel infrastructure — would fight the encrypt-once
fan-out, multiply mesh state, and re-open ordering questions. Verdict: **the
bus+filter model is the right shape.** The real work is not the bus; it is
message identity, the tag discipline, and where filtering lives in the read
contract — all decided below.

## What tags are — and are explicitly not

1. **Tags are views, never boundaries.** Every member receives every message —
   one MLS group, one ciphertext. A tag hides nothing from anyone. This is part
   of the contract and the UI copy, so nobody later builds "private channels"
   on top of tags. Private sub-groups would be a *different* feature (a second
   MLS group), out of scope here.
2. **Tags are chat-level, not protocol-level.** `Proposed`/`Approved`/
   `Committed` stay typed `WorkspaceEvent` variants with their verifiers and
   chain semantics. Tags never carry governance meaning; a tag can *reference*
   a proposal id, but approving happens only through `cmd_approve`. The bus is
   the transport truth; tags are a human namespace on the `Chat` variant only.
3. **Tags are claims, not facts.** Any member can tag anything (they are inside
   the group already, so this is a misfiling problem, not a security boundary).
   The UI treats tags as routing hints; nothing engine-side trusts them.

## The questions, and how they were decided

### Q1 — Message identity: indices break under filters → **stable unique ids**

`ChatMessage` had no id; reactions, deletes and quotes address messages by
0-based position in the local chat log. That is already why `ChatReacted` /
`ChatDeleted` do **not** cross the wire today (sender-local indices don't
transfer; delivery is in-order per sender only, so two members' logs can
differ). Filtered views make index addressing untenable: an index in a channel
view is not an index in the global log.

**Decided:** stable per-message ids are **Phase 1** of this plan, and they must
be **globally unique — a random 128-bit id (UUIDv4 or equivalent)** minted by
the sender per message. Explicitly *not* `(author, per-author seq)`: a
device-loss rejoin could restart the counter and collide. Requirements:

- The receiver treats an already-seen id as a duplicate and ignores it
  (replay defense, and a buggy/malicious sender reusing an id cannot overwrite
  an existing message — authorship stays bound to the MLS link identity, as
  today `cmd_net_delivered` overwrites `msg.from`).
- Old log entries without an id get a **deterministic synthetic id on read**,
  so existing logs stay addressable without rewriting them.
- `quote`, `ReactChat`, `DeleteChat` (and file addressing) migrate from indices
  to ids. Side effect shipped with this: reactions/deletes can finally cross
  the wire and converge across members.

### Q2 — Tag model → **structured `ChannelRef`, exactly one per message**

Free-form string tags (`tags: Vec<String>`) bring typo channels,
case/unicode normalization, tag spam, and an unbounded channel list derived by
scanning history.

**Decided:** a structured enum, **exactly one channel per message**:

```
ChannelRef = Group | Patch(ProposalId) | Topic(String)
```

- `Group` is the all-hands chat and the serde default — every legacy message
  files there.
- `Patch(ProposalId)` carries a real id (no typos; the UI resolves the title
  from proposal/chain state, lazily).
- `Topic(String)` is the escape valve for free human-named topic channels.
- New *system* channel kinds (e.g. a later `Membership(...)`) are new enum
  variants — deliberate design decisions, additively introduced.
- No cross-posting: a message has exactly one home, keeping filters, unread
  counts and quote context unambiguous. To surface a message in another
  channel, quote it there (cheap once ids exist).

### Q3 — Old readers → **degradation accepted**

The channel field is additive (`#[serde(default)]`); serde drops unknown
fields, so an *older* app version shows all tagged traffic unfiled in the one
flat list — patch chatter appears in the main chat. Nothing is lost, only
unfiled, and only while versions are mixed.

**Decided:** acceptable — the product is not in production yet; no compat
marker mechanism. Recorded here as a conscious degradation.

### Q4 — Ephemerality → **the gap is final; backfill is a non-goal**

Catch-up sync serves **only the chain** (`ChainRequest` → blocks); there is no
chat equivalent. Chat persists in the local encrypted log and survives
close/reopen on the *same* device, but a total-loss rejoiner (recovery ritual)
never receives chat history: they see the patch (via the chain) but an
**empty** `@patchdiscussion` channel.

**Decided:** this is **permanent, by design**. Ephemerality is a product
feature: fleeting deliberation is not re-distributed, not reconstructed, not
best-effort backfilled — a later "chat backfill" concept is explicitly **not
wanted**, so none is sketched here. (Writing discussions to the chain was never
on the table — it would break the "chain persistent changes only" invariant.)

UI consequences that must be handled regardless:

- A tagged message can arrive **before** the `Proposed` it references
  (ordering is per-sender only) — channels render before their referent is
  known, titles resolve lazily.
- A channel ref may stay unresolvable forever (proposal declined before this
  member joined, never re-served) — channels never error on unknown ids.

### Q5 — Where the filter lives → **engine-side**

Today `read_state(Surface::Chat)` returns the whole log and the GUI filters
client-side. Client-side filtering with channels would force every MCP agent
to re-implement `ChannelRef` semantics — duplicated logic, divergence risk,
and de-facto undermining of co-equality.

**Decided:** engine-side. `ReadState` gains an optional channel filter plus a
channel enumeration (for the sidebar and for agents orienting themselves); the
MCP `chat_send` / `read_state` tools gain the same parameter. It stays **one**
command — `Command::Chat` grows the channel field, no per-channel commands —
so the co-equality test keeps passing by construction.

### Q6 — Head-of-line and volume (noted, no action)

One stream means a burst in a busy patch discussion sits in front of `Group`
delivery (per-sender in-order pipeline). At republic scale (small groups) this
is a non-issue; recorded as a known property. Revisit only if real usage shows
it.

### Q7 — System lines in channel views → **first iteration**

**Decided:** a `Patch(id)` channel window interleaves the *protocol* events
for that id — `Proposed`, each `Approved` (with running count), `Committed` —
as system lines between the human messages, from the **first UI iteration**.
This is a pure read-side merge (two existing sources, one filter key), no wire
change — and it is the payoff of the whole concept: the channel becomes the
complete narrative of a patch (deliberation *and* governance trail in one
window), not just a filtered chat.

### Q8 — Interaction with existing chat features

- **Quotes**: migrate to message ids (Q1); quoting across channels is allowed —
  it is one log, and it is the sanctioned cross-posting mechanism (Q2).
- **Reactions / deletes**: move to id addressing and cross the wire (ships
  with Phase 1).
- **Files** (`FileMeta` in chat): a file share is a chat message, so it is
  tagged like any other — file offers scoped to a channel come for free.
- **"today"/"archive" sub-views**: orthogonal time dimension; kept as a second
  filter axis, not conflated with channels. Since 2026-07-16 this axis is real
  read semantics: `ReadState` takes an optional `view` ("today"/"archive")
  next to `channel`, and the two filters compose (see the retention note in
  the limitations below).

## The phased plan

Order matters: identity first, then the tag, then the read contract, then UI.
Test-first throughout (failing test → implementation), per house rules.

- **Phase 1 — stable message ids.** Random 128-bit per-message id (UUIDv4 or
  equivalent) on `ChatMessage` (`#[serde(default)]`, additive); deterministic
  synthetic ids on read for legacy entries; receiver-side duplicate-id
  rejection; migrate `quote` / `ReactChat` / `DeleteChat` (and file
  addressing) from indices to ids; let reactions/deletes cross the wire.
  Tests to pin first: id uniqueness and stability across
  serialize/replay/close/reopen; duplicate-id rejection; reaction convergence
  across two loopback instances with cross-sender reordering.
- **Phase 2 — the tag on the wire.** `ChatMessage.channel: ChannelRef`
  (`#[serde(default)]` → `Group`); `Command::Chat` gains the field;
  `crosses_wire` unchanged (chat already crosses). Receiving side: channel is
  data, never trusted for anything but display routing. Tests: envelope
  round-trip, old-reader tolerance (missing field → `Group`), two-instance
  loopback with mixed channels.
- **Phase 3 — read contract.** Engine-side channel filter + channel
  enumeration on `ReadState`; MCP `chat_send` / `read_state` grow the
  parameter; co-equality test updated alongside. Tests: filtered snapshot
  equals client-side filter of the full snapshot (property); channel
  enumeration matches distinct refs in the log; MCP tool schema.
- **Phase 4 — UI channels.** Sidebar of derived channels (`Group` always
  present; `Patch(id)` channels appear when referenced, titles resolved lazily
  and tolerant of unknown ids per Q4; `Topic` channels as they occur); chat
  window = filter; per-channel unread counts (purely local UI state);
  governance system lines interleaved in patch channels (Q7). Validation:
  clean `cargo build -p molt-ui` + engine-level tests (no GUI on
  `DISPLAY=:0`).

Non-goals (permanent): no per-channel queues or MLS groups; no persistence
change (chat stays off-chain; the local encrypted log keeps working as today);
no tag-based access control; no governance semantics in tags; **no chat
history backfill for rejoiners** (Q4 — ephemerality is the product).

## Decision log

All decided 2026-07-09 in discussion (chat-bus concept session):

| # | Question | Decision |
|---|----------|----------|
| Q1 | Message identity | Stable ids as Phase 1; globally unique random 128-bit (UUIDv4 or equivalent), duplicate-id rejection, synthetic ids for legacy entries |
| Q2 | Tag model | Structured `ChannelRef` enum (`Group \| Patch(ProposalId) \| Topic(String)`); exactly one channel per message; cross-posting via quotes |
| Q3 | Old readers | Degradation accepted (unfiled messages in the flat list); no compat marker — not in production yet |
| Q4 | Rejoiner gap | Final, by design: ephemerality is a product feature; chat backfill is a permanent non-goal |
| Q5 | Filter placement | Engine-side: `ReadState` channel filter + enumeration, same parameter on MCP tools; one command set |
| Q7 | System lines | In the first UI iteration: patch channels interleave `Proposed`/`Approved`/`Committed` as system lines |

## Known v1 limitations

Accepted in the 2026-07-10 review — documented, not fixed:

- **Id squatting (first-wins dedup).** Ids are random but not sender-bound: a
  malicious AUTHENTICATED roster member that learns a `MessageId` before some
  peer has the genuine message (from a quote, a parked reaction, a file ref)
  can race a forged `Chat` carrying that id. First-writer-wins dedup drops the
  loser; the winner holds the id — divergence is limited to that one message.
  The receive path (`net.rs`, `Chat` arm) WARN-logs the collision with BOTH
  identities (`from` and the stored author), which is the audit trail.
  Accepted because members are rostered, MLS-authenticated humans in small
  republics and chat is ephemeral; the real fix — sender-bound (signed) ids —
  is a protocol decision deferred deliberately.
- **Mixed-version reactions degrade to the legacy toggle.**
  `WorkspaceEvent::ChatReacted` now carries an additive idempotent `op`
  (`Add`/`Remove`, `#[serde(default)]`); a peer that does not send it falls
  back to the old toggle semantics — the accepted Q3 degradation posture.
- **Chat retention is a read filter, not physical deletion (2026-07-16).**
  "Delete chat after N days" is a real, threshold-gated engine setting
  (`set_chat_retention` on the Organization surface) and it is enforced at
  the READ contract: `ReadState` hides chat messages and declined vetoes
  older than the effective window, identically for GUI and MCP. The bytes
  still sit in the encrypted local log until the workspace is deleted.
  Physical pruning is a separate follow-up: log compaction must respect the
  legacy positional index scheme (synthetic-id derivation walks positions),
  the replay floor and the outbox cursors — a rewrite of the segment story,
  deliberately not smuggled into the retention feature.
  The window additionally splits at its half for the chat sub-views
  (2026-07-16): `ReadState { view }` serves "today" (the General view — age
  ≤ 50 % of the window, boundary inclusive, plus legacy ts 0) and "archive"
  (50 % < age ≤ 100 %); no `view` is the whole window, so older readers are
  unchanged. The boundary is one pure function (`chat_view_admits`,
  explicit `now`), the channel enumeration stays unfiltered, and the MCP
  `read_state` tool takes the same parameter — engine-side for GUI and
  agents alike, per Q5.
- **Uploads are ephemeral on the same rhythm (2026-07-16).** A file share
  IS a chat message, so it ages out with the same window, cutoff and clock
  (`State::chat_visible` / `aged_out_at`; ts 0 = unknown age, kept) — one
  knob, no separate link TTL (`UploadView.expires_ts` is now the real
  retention deadline, `ts` + window). Past the deadline the share leaves
  the uploads table, the member upload counts and the chat read together,
  and it stops being downloadable: `download_file` refuses cleanly with
  `FileExpired`, and the sharer refuses to serve an expired share over the
  wire (an honest `Refused`, covering a requester whose local check was
  skipped or lags near the boundary). Same pruning posture as chat: the
  share's log entry rides the log-compaction follow-up above, and the
  SHARER's local source bytes plus its `shared_files` path entry in the
  prefs sidecar likewise stay until that follow-up forgets expired shares
  (a downloader's saved copy is the user's file — never the engine's to
  delete).
- **Applied entries carry their proposal id (2026-07-17).** The read
  contract grew a parallel id track: `SurfaceSnapshot.applied_ids` runs
  positionally next to `applied` and names the proposal each applied entry
  came from (`None` = origin unknown: chat rows, pre-id dumps). The applied
  payloads themselves stay byte-identical — payload-comparing readers (the
  UI fate probe, MCP) are untouched. This is what lets an ACCEPTED vote's
  row (gated applied logs, Organization → Accepted) reopen its patch-channel
  discussion via 💬, completing the decided-votes story: declined cards
  already kept the link, now applied rows do too — read-only either way,
  the engine refuses new writes into a decided discussion.
