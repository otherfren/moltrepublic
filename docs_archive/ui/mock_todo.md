# Final report — mock removal, stories 1–13 (session 2026-07-18/19)

**All 13 stories of this document are implemented and on master** (tested green,
clippy 0, Slint window build clean). Approach: test-driven, each story on its
own branch, merged to master in document order, each with its own
code review before the merge.

**What became real (1–13):** real `rfd` folder dialog (1); honest network display
when founding instead of a cosmetic dropdown (2); real workspace size from the
directory (3); demo mesh/peers removed from the production binary, solo boot,
test seam (4); real S3 connection test — new pure-Rust S3 client (SigV4, HTTP,
fail-closed/Tor), basis for 8/12/13 (5); real presence timestamps + aging ticker,
activity trio from real stamps (6); governance counting simulation removed, without
a chain only the node's own approval remains, honest `AlreadyApproved` (7); real
backup orphans from a signed `ListObjectsV2` with hardened XML parsing (8); real
encrypted single-file export `.molt.enc` (Argon2id, XChaCha20-Poly1305; MLS ratchets
and queue creds are never exported) (9); real at-rest sealing (derive-and-verify,
key leaves the disk, phrase really checked, survives restart) (10); plugin vocabulary
removed (11); real S3 auto-backup (ticker, honest stamps only after a confirmed
upload, retention) (12); real two-phase restore from file/S3 — staging →
chain verify hard-reject before materialization → commit, opening "detached", the way
back into the living republic remains the recovery ritual (13). Design in
`docs_archive/storage/backup_restore_design.md`. Story 14 (Memory/Quests/Vault/Wallet as real
products) deliberately stays open.

**UI work (user requests):** the four shelved surfaces (Memory/Quests/Vault/
Wallet) reactivated as clearly badged "design mock" panes; the Memory "Brain" pane as
file navigator + Markdown content with a movable divider; Settings→Network made wider
and the SMP server as a selection with a collapsible custom field.

**Wiki UX round (2026-08-12, user punch list):** the Brain pane state now lives
as a unit-tested Rust state machine (`crates/molt-ui/src/wiki.rs`,
`WikiState` global) instead of fixed .slint slots — tabs in opening order
(⬅️/➡️, Ctrl+W, context menu "close all/right/left"), navigator with
select≠open (double click/Enter/menu opens, Del deletes, right click
Open/Rename/Delete, drag moves), and EVERY local change is colour-tracked
(nav/tabs/preview block diff; delete = red struck-through pending row) — the
visual precursor to the later "changes as ONE patch for a vote" step. Plus
app-wide adjustable font sizes (app/navigator/editor, gear in the wiki
toolstrip → Settings→General, `set_fonts` command + `[ui]` keys in config.toml,
old configs keep defaults). Story 14 (real backend + patch vote) stays open.

**Changeset round (2026-08-14, user punch list):** the patch-vote step is
built. Every wiki action (edit coalesces per file, new, folder, rename,
move, delete) lands as an entry on a change stack
(`crates/molt-ui/src/wiki.rs`); the changeset panel above navigator+editor
shows stack + net count (new/deleted/moved/lines) with undo
(LIFO, collision-honest), discard (confirmed) and "start vote".
The vote reprocesses the stack into the NET git patch (`build_patch`:
unified diff + rename/new/deleted headers via `similar`; if everything cancels
out → panel cleared + toast) and runs as a REAL gated proposal
`{op:"wiki_patch", summary, value}` on the Memory surface — the same
threshold governance as everywhere, reachable co-equally via MCP `propose`.
The proposal card shows the raw patch only behind a
👁️ button (read-only modal + copy, charter idiom); the decision discussion
of a wiki_patch carries the DIFF VIEWER between card and chat
(`crates/molt-ui/src/patchview.rs` + `PatchView` global): the patch's files on
the left (`<moved>`/`<deleted>` marked), the character-exact diff on the right
(green added, red removed, paired lines char-diffed).
Also: context menu "discard changes" (Modified/Deleted) and "copy
link" (Markdown markup to the clipboard), editor context menu
cut/copy/paste, a click on the preview opens the editor,
the editor header shows a "changed" hint instead of "~1" + file revert.
After a successful vote start the working copy is reset to the base
(the proposal carries the changes). BUILT (2026-08-15,
`docs_archive/memory/shared_memory_real.md`): patch APPLY at threshold is the
deterministic FOLD (molt-core `wiki_fold`, strict, all-or-nothing),
the base comes from the engine read, the supersede walk terminally clears outdated
pending patches, draft rescue (decline AND superseded) brings
the patch best-effort into the working set, local drafts persist
sealed per workspace (`wiki_draft.json`, never in the export). With that
the MEMORY half of story 14 is real; Quests/Vault/Wallet stay open.

**Final review + security analysis:** adversarial multi-agent review over the
entire session diff (`86b72db..`, ~15.7k lines, 10 dimensions, every finding verified by
3 perspective-diverse skeptics). Result: **no critical bugs**;
found and fixed were 3 HIGH (cross-blob splice without per-blob salt in the export;
PUT upload ran into the 30-s overall timeout and would have made real backups over Tor
fail; non-canonical backup key widths broke the retention order), 1
HIGH interaction gap (restore-replace could destroy the open workspace), 8 MEDIUM
and ~19 LOW (among them zeroize of the secret buffers, honest S3 error classes, sealed-tmp
cleanup, phrase leak in the boot scan of a half-sealed directory, outdated
"mock" modal texts). All confirmed findings are fixed on master, test-driven,
green again.

---

# Mock inventory — what is not yet real

As of: 2026-07-18

This document is an exhaustive inventory of all features that (a) are explicitly
marked as mock/demo/simulation in the code or (b) have a GUI/MCP surface
with no real function behind it (no-op, fake data, local UI state only). Every
finding was verified against the actual Rust wiring (command → engine handler →
actual behaviour), because many comments are outdated — the project is further
along than its comments claim.
**Purpose: basis for decisions. NOTHING is implemented here yet.**

At the end there is a separate section "Outdated mock comments": places that are
labelled as mock but have long had a real function behind them — valuable
for cleanup, but no implementation needed.

## Overview

| No. | Title | Complexity |
|----:|-------|:-----------:|
| 1 | ✅ DONE — Workspace folder selection without a real file dialog | S |
| 2 | ✅ DONE — Create screen: "network" selection is cosmetic (documented, P8) | S |
| 3 | ✅ DONE (size; sync fields → no. 6/12) — Static status fields of the workspace list | S |
| 4 | ✅ DONE (removed) — Demo boot group + demo mesh with answering "brains" | S |
| 5 | ✅ DONE — S3 "test connection" always shows success (pure UI toast) | M |
| 6 | ✅ DONE — Presence without real timestamps: no aging, mock activity trio | M |
| 7 | ✅ DONE (removed) — Legacy counting simulation of governance | M |
| 8 | ✅ DONE — Backup orphans: static demo data in the backup table | M |
| 9 | ✅ DONE — Manual workspace export ("backup as blob") is a UI no-op | L |
| 10 | ✅ DONE — At-rest encryption: flag flip, phrase unchecked, not persistent (S6) | L |
| 11 | ✅ DONE (vocabulary removed) — Plugin governance without plugin state | L |
| 12 | ✅ DONE — Auto-backup to S3: settings complete, backend missing (S5) | XL |
| 13 | ✅ DONE — Restore from S3/file: fully simulated run with fake log (S4/S5) | XL |
| 14 | Four surfaces without implementation: Memory, Quests, Vault, Wallet | XL |

---

## 1. Workspace folder selection without a real file dialog — **S**

> **✅ DONE (2026-07-18).** "Browse…" now opens a real
> `rfd::AsyncFileDialog::pick_folder()` (start directory = validated draft,
> `~` expansion as in the engine path); the text field stays as a manual fallback.
> Mock wording removed. The location line numbers below are historical.

**Locations:**
- `crates/molt-ui-window/ui/app.slint:652` ("mock folder picker for the settings workspace tab"), modal at `:6116`
- `crates/molt-ui/src/lib.rs:4617` (`set_ws_dir_body`: "Mock — no real file dialog")

**What happens today:** The "choose folder" button in the settings workspace tab
opens a modal with a free text path field. The setting itself is
**real** (validated, persisted to `config.toml`, restart hint) — only the
picker is not one.

**Real implementation:** Wire up `rfd::AsyncFileDialog::pick_folder()` — `rfd`
is already a dependency and is used for file share (`on_share_pick`,
`crates/molt-ui/src/lib.rs:913`) and logo selection (`:1147`). Only
molt-ui + one callback.

**Complexity: S** — the same pattern exists twice in the same file.

## 2. Create screen: "network" selection is cosmetic (documented, P8) — **S**

> **✅ DONE (2026-07-18): variant (a) implemented.** `CreateStart` no longer has a
> `net` field (command + MCP tool param removed); the create screen shows
> the effective global setting read-only (with a pointer to Settings →
> Network); `WorkspaceInfo.net`/`CreateState.net` are derived engine-side from the
> global settings (`molt_core::effective_net_label` — also for
> join/recovery/restore and for the startup scan, which previously hard-coded "tor").
> Details: `tor_transport_implementation.md` §P8. The location line numbers
> below are historical.

**Locations:**
- `crates/molt-ui-window/ui/app.slint:1570–1624` (dropdown tor/none, Tor mode, port; `:1624` only passes the string `"tor"`/`"none"`)
- `crates/molt-engine/src/lifecycles.rs:179,198` (`cmd_create_start` stores `net` only as a label), `:645`, `:906` (join hard-codes `"tor"`)
- `crates/molt-engine/src/founding.rs:365–374`: the ritual transport comes from `resolve_dialer()` = **global** settings (`crates/molt-engine/src/session.rs:256–280`)
- Documented decision: `docs_archive/transport/tor_transport_implementation.md` §P8 ("`CreateStart.net` stays cosmetic")

**What happens today:** The network choice when founding (incl. Tor mode and
port) does not influence the transport; the global anonymity settings are always
authoritative. The value only ends up as a display label in
`WorkspaceInfo.net` ("network: tor" in the open detail).

**Real implementation:** Either (a) remove the dropdown/replace it with a
display of the global setting (S, more honest), or (b) build real
per-workspace transport (L: thread a per-workspace dialer through founding, join,
reopen and recovery). P8 has in effect already decided (a) — only the cleanup
step is open.

**Complexity: S** (for the honest variant; per-workspace transport would be L).

## 3. Static status fields of the workspace list — **S**

> **✅ DONE for the size (2026-07-18).** `size_kib` is real: recursive
> KiB walk in molt-storage (symlink-safe, race-tolerant), stamped at
> boot scan, materialization, open (after replay) and clean close; session-only
> demo entries honestly report 0. The sync fields (`synced`/`state`/
> `last_sync_min`/`sync_queue`) belong to no. 6, `last_backup_min` to no. 12.

**Locations:**
- `crates/molt-engine/src/lifecycles.rs:186–196` (`push_workspace_entry`: `synced: true`, `state: 0`, `last_sync_min: 0`, `sync_queue: 0`, `size_kib: 16`, `last_backup_min` = 0/NEVER)
- `crates/molt-core/src/lib.rs:386–402` (field definitions, `size_kib` "(mock)")
- Display: open-screen table + detail panel (`crates/molt-ui/src/lib.rs:1440ff`, sorting `sort_ws_items`)

**What happens today:** Every real workspace constantly shows "synced", "16 KiB",
"last sync 0 min", queue 0. The values are never updated; only the
demo data set (`demo_set`) carries varied fake values.

**Real implementation:** Compute `size_kib` from the directory size at scan/close
(molt-storage, trivial); feed `last_sync_min`/`sync_queue`/`state` from the
real outbox/delivery state of the mesh (molt-engine `net.rs`, ties in
with finding 6); `last_backup_min` belongs to finding 12.

**Complexity: S** for the size; the sync fields are the M part of finding 6.

## 4. Demo boot group + demo mesh with answering "brains" — **S**

**Locations:**
- `crates/molt-app/src/main.rs:177` (`GroupConfig::demo()` as the boot group of the real moltd)
- `crates/molt-core/src/lib.rs:3133–3141` (`GroupConfig::demo`: me/peer-1/peer-2, 2-of-3), `:451–466` (`demo_workspace_id`), `:487ff` (`demo_set` — only in sessions without persistence; moltd replaces the list with the disk scan, `main.rs:165`)
- `crates/molt-engine/src/net.rs:19–56, 523–640, 1589–1706` (demo mesh, `spawn_demo_peer`, canned `LINES`, answer probability/delay via `mockrand`)
- `crates/molt-core/src/lib.rs:1746–1763` (`mockrand` — expressly a non-crypto PRNG for the simulations)
- `crates/molt-engine/src/net.rs:555–563`: runs for the context without an open workspace AND for persisted workspaces with `prefs.simulated_members` (legacy stock founded before T3)

**What happens today:** Without an open workspace an in-process loopback mesh
runs with two simulated peers that answer roughly every fifth message with a canonical
demo line. Deliberate, cleanly delimited demo machinery (real
workspaces never get fake peers; a legacy workspace only via an explicit
prefs flag) — but it sits in the production binary and is reachable via MCP
(`chat_send` without an open workspace).

**Real implementation:** Product decision: keep it as an onboarding playground
(then label it clearly) or remove it (boot context without chat). Removing
would be small; the test usage (`tests/demo_mesh.rs`) would have to move to a test seam.

**Complexity: S** (decision + remove/label; no rebuild).

**Status (2026-07-18): REMOVED** (product decision). moltd boots with
`GroupConfig::solo()` (only the operator, no invented peers); the
demo mesh sits behind the default-OFF test seam `State::demo_mesh`
(spawner `__spawn_demo_mesh`, analogous to `ritual_sim`), and
`prefs.simulated_members` is still parsed but no longer spawns any peers
(inert). Chat without an open workspace is an honest local notes log -
nobody answers. Negative tests pin both in `tests/demo_mesh.rs`;
`GroupConfig::demo()` / `demo_workspace_id` live on as pure test fixtures.

## 5. S3 "test connection" always shows success — **M**

> **✅ DONE (2026-07-18).** Real `Command::NetTestS3` modelled on the
> SMP test: pure-Rust S3 building block in molt-net (SigV4 against the official
> AWS test vectors, minimal HTTP/1.1, rustls+webpki-roots) over the
> fail-closed, Tor-capable dialer; honest error classes (endpoint/connect/
> TLS/403 creds/404 bucket/301 region); MCP tool `net_test_s3`; fake toast
> removed. Verified live against AWS. The client is the basis for no. 8/12/13.

**Locations:**
- `crates/molt-ui-window/ui/app.slint:5083–5093`: `clicked => { root.show-toast(Strings.toast-s3-ok); }` — pure UI toast, no command
- `crates/molt-ui/src/lib.rs:4623` (`set_s3_test`), toast string `toast_s3_ok`

**What happens today:** The test button in the backup settings tab reports
success unconditionally as soon as endpoint + access key are non-empty. No
network access takes place. This collides directly with the project's own rule "the UI
must not fake success" (`crates/molt-ui/src/lib.rs:2414`) — the SMP test button
next to it shows how: real command (`NetTestServer`), result streaming,
`#[ignore]` live test (`crates/molt-engine/tests/smp_test_button.rs`).

**Real implementation:** A `Command::NetTestS3` modelled on the
SMP test: engine-side a minimal S3 request (e.g. signed
HEAD/ListBucket, pure-Rust signing crate) over the configured dialer
(Tor-capable, fail-closed), result into the session. Layers: molt-core
(command), molt-engine, molt-net/HTTP, molt-ui, molt-mcp (tool, co-equality).

**Complexity: M** — small in the UI, but it needs the first real
S3 client building block (which then feeds findings 8/12/13), incl. Tor routing.

## 6. Presence without real timestamps; mock activity trio — **M**

> **✅ DONE (2026-07-18).** `MemberInfo.last_seen` is a real
> Unix stamp (0 = never seen); a 30-s ticker ages pills (online ≤5 min,
> stale ≤30 min, otherwise offline; send backoff pins unreachable until the next
> sighting); the 1h/24h/7d trio and the upload availability compute from real
> stamps; the UI renders the relative time client-side.

**Locations:**
- `crates/molt-engine/src/net.rs:1537–1587`: `cmd_net_peer_seen` / `cmd_net_send_failed` / `update_member_pill` — incl. the honest comment "Honest limitation (T5 closes it)": the label "just now" cannot age
- `crates/molt-engine/src/proposals.rs:740, 758–776` (read_members: presence = session label), `:826–843` (activity trio 1h/24h/7d = projection of the mock presence), `:815` (upload "available" depends on this presence)
- `crates/molt-core/src/lib.rs:2979–2993, 3069–3079` (field doc "mock presence"), `:3043`
- `crates/molt-ui/src/lib.rs:1646, 2150, 2182, 3022` (GUI side), MCP description `crates/molt-mcp/src/lib.rs:648`

**What happens today:** Since commit b5a0055 presence is **half real**: the
engine stamps pills event-driven (peer seen → "just now"/synced;
send backoff → "unreachable"/offline). But there is no numeric
last-seen timestamp: a label never ages, members start with
placeholders, the status trio "active 1h/24h/7d" is a projection of the
pill states (7d = always the whole roster), and the upload availability ("sharer
online?") depends on the same pills.

**Real implementation:** Replace `MemberInfo.last` with a numeric
`last_seen` Unix stamp (rendering UI-side as with the
`last_sync_min` pattern), engine ticker ages states; compute the trio from real
stamps. Layers: molt-core (field, additive), molt-engine
(stamp/ticker), molt-ui (rendering), MCP description.

**Complexity: M** — the event sources already exist; it is model and
threading work across 4 crates plus test adjustments.

## 7. Legacy counting simulation of governance — **M**

> **✅ DONE (2026-07-18): removed** (product decision). Without
> chain governance a node records at most its OWN approval
> (repeat = `AlreadyApproved`); nobody counts up invented peers any more.
> The solo boot group runs as real 1-of-1 governance. `simulated_members`
> is an inert legacy label. Recovery can no longer mint fresh applied entries from
> old simulated counter states (pinned).

**Locations:**
- `crates/molt-engine/src/lib.rs:13` ("faithful but *simulated* stand-in for the real FROST threshold machine")
- `crates/molt-engine/src/proposals.rs:213, 352–366, 718–721` (a local operator counts up for the peers; anonymous counter; every pending proposal counts as open for peers)
- Delimitation: `crates/molt-engine/src/chain.rs:615–616, 723` — chain workspaces run **real** threshold governance over the mesh; the simulation is OFF there via `is_chain_governed`

**What happens today:** Only for the demo boot group and legacy workspaces with
`simulated_members` does the counted simulation still apply: approvals are local
counters without signatures, repeated approve simulates peers. For all
workspaces founded through the real ritual (chain-governed) governance is real
(signed, position-bound blocks, m-of-n).

**Real implementation:** No rebuild needed — the real path exists. The open
question is a product one: whether legacy/demo workspaces migrate to chain governance
or the simulation stays documented as deliberate demo behaviour.
Affected: molt-engine (`proposals.rs`), possibly a migration path in `chain.rs`.

**Complexity: M** — migration design for legacy stock; the machine itself is there.

## 8. Backup orphans: static demo data — **M**

> **✅ DONE (2026-07-18).** `demo_set()` is deleted; the backup table
> is fed from a real, signed `ListObjectsV2` over the fail-closed
> dialer (`Command::NetListBackups` + MCP tool, pagination, hardened
> XML parsing, generation counter against stale results); refresh when opening
> the backup tab + explicit button; errors are shown honestly, never
> invented rows.

**Locations:**
- `crates/molt-core/src/lib.rs:656–676` (`BackupOrphan`, "(mock). Shows up in the settings backup table", `demo_set()`)
- `crates/molt-app/src/main.rs:167`: moltd takes over `..SessionView::default()` — the demo orphans end up in the **real** production session state
- `crates/molt-ui/src/lib.rs:1499–1525` (backup table: local workspaces + orphans), display `app.slint:5195ff`

**What happens today:** The backup table in the settings always shows two
invented bucket entries ("remote only") — even in the real moltd, without any
S3 access ever having taken place.

**Real implementation:** Feed orphans from a real bucket listing
(needs the S3 client from finding 5/12) and until then remove the demo data from the
production path (leave empty). Layers: molt-core (empty the default),
molt-engine (listing command), molt-app.

**Complexity: M** — the immediate measure (demo data out) is S; the real listing
depends on the S3 client.

## 9. Manual workspace export ("backup as blob") is a UI no-op — **L**

> **✅ DONE (2026-07-18).** Real S4 export: `molt-export-v1` blob
> (Argon2id 64 MiB/t=3/p=1, XChaCha20-Poly1305 in 4-MiB chunks, header authenticated
> via key binding), atomic write with fsync; MLS ratchets and
> SMP queue creds are NEVER exported; `Command::ExportWorkspace` + MCP tool;
> design: `docs_archive/storage/backup_restore_design.md`. Import is no. 13.

**Locations:**
- `crates/molt-ui-window/ui/app.slint:5766–5795`: on "confirm" the backup modal only sets `root.export-note = "Exported (mock): <path>"` — no command, no write
- `crates/molt-ui/src/lib.rs:4548` (`bk_body`: "Mock — nothing is written"), `:4537` (`ow_export_note`)
- No `Command::Export*` in `crates/molt-core/src/lib.rs`, no MCP tool

**What happens today:** "Create backup now" in the open detail takes a
target path and afterwards claims an export in the panel — nothing is
read or written. (The modal text at least says so honestly.)

**Real implementation:** The blob format is milestone S4 from
`docs_archive/storage/concept-workspace-storage.md` (status header: "S4 export/import …
remain open"): an encrypted single-file export (`.molt.enc`) of the
workspace directory incl. key story, plus the import counterpart
(finding 13). Layers: molt-storage (format), molt-core (command),
molt-engine, molt-ui, molt-mcp.

**Complexity: L** — format design with a crypto part; half the foundation of
finding 13.

## 10. At-rest encryption: flag flip, phrase unchecked, not persistent — **L**

> **✅ DONE (2026-07-18).** S6 derive-and-verify: sealing removes
> key material from the disk (genesis frame tag = phrase check; wrong
> phrase = hard error, disk untouched), state comes from the
> directory marker (survives restart), `STORAGE_VERSION_SEALED` gate,
> legacy directories without migration; sealed directories refuse
> export/open in a typed way. Storage-less nodes refuse honestly.

**Locations:**
- `crates/molt-engine/src/session.rs:576–611`: `cmd_encrypt_workspace` only sets `ws.encrypted = true` (session field); `cmd_decrypt_workspace` demands a non-empty phrase but does not check it
- `crates/molt-core/src/lib.rs:398–405, 2264–2277` (command doc "(mock)"), `crates/molt-ui-window/ui/app.slint:354, 2180–2196`, `crates/molt-ui/src/lib.rs:4525` (`dw_body`)
- MCP tools `encrypt_workspace`/`decrypt_workspace` (`crates/molt-mcp/src/lib.rs:834–861`, description honestly declared as mock)
- Contrast: the event log itself IS encrypted at rest (device-sealed key, `crates/molt-storage/src/lib.rs:8–39`); `scan_workspaces` always returns `encrypted: false` (`:1433`)

**What happens today:** The switch blocks opening (open refuses
encrypted entries) — but it is a pure session flag: nothing changes on the
disk, any phrase "decrypts", and a restart forgets the
state completely (scan sets `false` again).

**Real implementation:** Milestone S6 "passphrase sealing"
(`docs_archive/storage/concept-workspace-storage.md`): additionally seal the
device-sealed log key under the recovery phrase, remove `seed.sealed`,
really verify the phrase on decrypt, derive state from the directory.
Layers: molt-storage (core), molt-engine, molt-ui/mcp (texts only).

**Complexity: L** — security-critical key hierarchy, migration path
for existing workspaces, design doc mandatory per CLAUDE.md.

## 11. Plugin governance without plugin state — **L**

> **✅ DONE (2026-07-18): vocabulary removed** (product decision). The
> five plugin mentions were pure prose; no persisted/wire-visible
> type ever carried a plugin variant. The replay tolerance for unknown org ops
> (load-bearing for additive-only) is now pinned by a test.

**Locations:**
- `crates/molt-engine/src/proposals.rs:133` ("no plugin state exists yet (mock) — nothing to show")
- `crates/molt-core/src/lib.rs:48` (Organization: "charter, name, image, plugins"), `crates/molt-ui-window/ui/app.slint:4467` (pending cards "plugin changes in voting")

**What happens today:** "Plugins" exist only as vocabulary: an
organization proposal with a plugin op would run through the real governance,
but there is no effective plugin state, nothing is activated,
the actual/target display stays empty. The GUI offers no plugin editor
(only charter/name/logo/retention are wired up — those are real).

**Real implementation:** First a product definition (what is a plugin?), then:
extend the effective org state by plugins (`org_effective`), apply semantics,
UI list. Layers: molt-core, molt-engine, molt-ui.

**Complexity: L** — not because of the technology but because the product definition
is missing; without it, removing the vocabulary (S) would also be honest.

## 12. Auto-backup to S3: settings complete, backend missing (S5) — **XL**

> **✅ DONE (2026-07-19).** Real backup ticker (60 s, actor pattern):
> crash-consistent `molt-export-v1` blob in workspace-key mode, signed
> PUT over the fail-closed dialer, `last_backup` advances ONLY on a confirmed
> upload, retention deletes only after confirmation and only own keys,
> sealed workspaces are honestly skipped; `backup_now` tool;
> fake stamp on enabling removed.

**Locations:**
- `crates/molt-engine/src/session.rs:613–633`: `cmd_set_workspace_backup` — the pref is persisted **for real** (`prefs.toml`), but the comment says it plainly: "the uploader itself is milestone S5; the stamp keeps list and prefs consistent" — `last_backup_min = 0` is stamped without any backup running
- Settings tab "Backup": interval, copy retention, endpoint/keys/bucket (`app.slint:5013–5133`) — everything is persisted for real to `config.toml` (`configstore.rs:336–342`), but consumed by nobody; no S3 code exists in the workspace (grep over molt-net/molt-storage: no hits)
- MCP tool `set_workspace_backup` (`molt-mcp/src/lib.rs:817`)
- Cross-reference: `docs_archive/reviews/total_review.md` (medium finding: "the S3 backend itself remains the known deferred work"), `docs_archive/storage/concept-workspace-storage.md` (S5 open)

**What happens today:** The auto-backup switch, interval and retention are
fully operable and survive restarts — but there is no uploader, no
ticker, no bucket interaction. "Last backup: just now" is a
claim produced by the stamp on enabling.

**Real implementation:** Milestone S5: S3 client (pure-Rust, over the
Tor-capable dialer), periodic backup ticker in the engine, blob format from
finding 9, retention (keep-copies), orphan listing (finding 8), honest
last-backup stamps. Layers: molt-net/HTTP client, molt-storage,
molt-engine, molt-core, molt-ui, molt-mcp.

**Complexity: XL** — new network client + background lifecycle +
format dependency on finding 9; security- and privacy-relevant (creds,
Tor routing).

## 13. Restore from S3/file: fully simulated run with fake log — **XL**

> **✅ DONE (2026-07-19).** The fake run (invented log lines, ~45 %
> failure rule, "Restored Republic") is deleted. Real two-phase import:
> staging (allowlist — `keys/`/`transport.state` can never come in via a blob)
> → **chain verify hard-reject before any materialization**
> (`verify_served` shared with recovery adoption) → commit (re-seal
> under the local device key, fresh minimal identity, never ratchets/creds).
> S3 path via streaming GET with caps; workspace honestly opens "detached" -
> the way back into the living republic remains the recovery ritual.

**Locations:**
- `crates/molt-engine/src/lifecycles.rs:207–248` (`cmd_restore_start/tick/cancel/finish`), `:312–366` (`restore_tick`: invented log lines — "GET …/manifest.enc · 200 OK", "chunk 17/23 fetched · sha256 ok", "aes-256-gcm: chunk decrypted · merkle node ok" — and the fake failure rule "implausible target fails at ~45 %")
- `:238–309` (`cmd_restore_finish`): materializes a **freshly founded** workspace "Restored Republic" without chain, MLS, mesh or identities ("restore rebuilds … at S4/S5")
- GUI: restore screen S3/file paths `app.slint:2664, 2821–2860` (shows "<endpoint> · OK" in green as soon as an endpoint is configured — unchecked), `:2924` (file), modal `rw_file_body` ("Mock — nothing is read")
- MCP tools `restore_start/cancel/finish` (`molt-mcp/src/lib.rs:864–893`, description honest: "(mock) restore")
- Core of `molt_core::RestoreState` as "The (mock) restore lifecycle" (`molt-core/src/lib.rs:1894, 1974`)

**What happens today:** The two restore paths "from S3" and "from file" are
a pure progress show: not a byte is read, the result is an
empty, freshly generated workspace with a new seed. **To be distinguished:** the
third path on the same screen — rejoining via recovery link
(`recover_invite_start`/`recover_start`) — is **real** (recovery ritual over
SMP, chain adoption, verified; `crates/molt-engine/src/recovery.rs`,
`lifecycles.rs:1044ff`) and must not be filed as a mock.

**Real implementation:** S4 (import of the encrypted blob: read,
decrypt, verify, restore directory + identities + chain + MLS snapshot)
and S5 (the same path from the bucket). Replaces the fake ticker
with a real task with real progress events. Layers: molt-storage,
molt-engine, molt-net (S3), molt-core, molt-ui, molt-mcp.

**Complexity: XL** — counterpart to 9+12 plus verification/recovery semantics
(what is a valid backup? Chain verify on import is mandatory).

## 14. Four surfaces without implementation: Memory, Quests, Vault, Wallet — **XL**

**Locations:**
- `crates/molt-core/src/lib.rs:41–130` (surface enum incl. sub-views for all six; Memory "shared brain", Quests "quest board", Vault "sealed secrets", Wallet "Monero multisig in production")
- `crates/molt-ui-window/ui/app.slint:1105`: navigation greyed out — "staged out for now: only Organization and Chat are live in the GUI"; there is no content pane for the four (`:3014ff` renders only chat/organization)
- `crates/molt-ui/src/lib.rs:4094–4104` (`default_op`: add_note/add_quest/seal_secret/transfer), `:4670` (`mv_later` — meanwhile an unused string)
- MCP: `select_surface`/`select_view`/`propose` accept the four surfaces (`session.rs:44–60` validates only against `Surface::ALL`)

**What happens today:** The governance mechanics for the four surfaces are real
(a proposal to "memory" runs through threshold voting and lands in the
chain's applied log) — but there is **no surface state**: no note,
quest, vault or wallet model, nothing is changed by an apply, the
GUI shows nothing (navigation locked; an MCP `select_surface` to
"vault" leads to an empty area in the GUI).

**Real implementation:** Four standalone products on the existing
gated governance: data model + apply semantics in molt-core/molt-engine,
views in molt-ui, tools in molt-mcp. Wallet is the largest (Monero multisig =
its own crypto/network project), Vault needs threshold-release crypto,
Memory/Quests are "only" CRDT-like state models over the chain.

**Complexity: XL** — per surface rather its own L–XL each; carried here as one
collective item.

---

## Deliberate test seams (no action needed, for completeness)

- **Simulated founding members** (`ritual_sim`): activatable only via test spawner
  (`crates/molt-engine/src/lib.rs:263, 601` — default `false`;
  `founding.rs:1499–1526`). The GUI then honestly shows the SIMULATION badge
  (`cw_sim_badge`). In-app founding runs for real over SMP.
- **Manual seam** (`spawn_manual_ritual`, `#[doc(hidden)]`): the
  two-instance loopback test path (`two_instances.rs`).
- **LoopbackTransport** (`crates/molt-net/src/loopback.rs`): deliberately permissive
  test double; the CLAUDE.md warning about it is current.
- `crates/molt-storage/examples/mkdummy.rs`: dev tool.

## Outdated mock comments (real, only the docs are wrong)

Behind these markers there has long been a real function — worthwhile cleanup,
otherwise they mislead the next session:

1. **`crates/molt-ui-window/ui/app.slint:14`** — "Everything is a mock: nothing
   is written to disk." Wrong: storage (S0–S3), settings persistence, founding,
   chain, chat are real.
2. **`crates/molt-ui/src/lib.rs:21`** — crate header: "workspace lifecycles are
   still a **simulation** — no workspace is created on disk yet." Wrong, see above.
3. **`crates/molt-ui-window/ui/theme.slint:11–14`** — AppScreen comments:
   `open` "(mock list)" (list comes from the real disk scan,
   `molt-app/main.rs:104`), `join` "(mock)" (real join ritual over SMP),
   `settings` "(mock — no disk write)" (persists format-preserving to
   `config.toml`). `restore` "(mock)" is only still true for the S3/file paths
   (finding 13); the recovery path is real.
4. **`crates/molt-ui-window/ui/app.slint:4806`** — "settings (in-memory config
   editor — MOCK, no disk write)" — same mistake.
5. **`crates/molt-ui-window/ui/theme.slint:147` + `molt-core/src/lib.rs:398`** —
   `seed` "(mock) recovery seed": the seed is real entropy
   (`generate_seed_phrase`), device-sealed on disk (`seed.sealed`), basis of the
   key derivation. Only the *verification on decrypt* is missing (finding 10).
6. **`crates/molt-ui/src/lib.rs:4512` (`ou_note`) and `app.slint:663, 4168`** —
   "(Transfer and expiry are mocks.)" Wrong: file transfer is real
   (manifest/pieces over MLS, `crates/molt-engine/src/chat.rs:216ff`,
   `transfer` module, reorder bugfix 2ef58b0), and `expires_ts` is the real
   retention deadline (`proposals.rs:787, 814`).
7. **`crates/molt-ui/src/lib.rs:4685` (`dm_body`)** — delete message:
   "(Mock — nothing on disk.)" Wrong: deletes are id-addressed, go
   over the wire and live in the persisted event log.
8. **`crates/molt-ui/src/lib.rs:4689` (`close_ws_body`)** — "This is a mock,
   nothing is written to disk." Wrong: the clean close
   persists the MLS snapshot + transport credentials (`cmd_close_workspace`,
   `session.rs:563–574`; `persist_crypto_blocking`).
9. **`crates/molt-ui/src/lib.rs:4545` (`del_ws_body`)** — "Mock — nothing on
   disk is touched." Wrong: `cmd_delete_workspace` really moves the
   directory to the trash (30-day purge; `session.rs:691–724`,
   `molt-storage:1520`).
10. **`crates/molt-ui-window/ui/app.slint:683`** — chat retention "(mock
    current value; a change is a proposal)": the value comes from the real
    effective org state (`set_chat_retention` proposals, filtered
    engine-side).
11. **`crates/molt-ui-window/ui/theme.slint:435`** — "the mock logo/charter
    edit modals": logo/charter/name changes are real
    organization proposals incl. sign-what-you-see image bytes.
12. **`crates/molt-app/src/main.rs:131–138`** — boot log: "loopback transport
    active; Tor/SMP wiring is milestone T3–T5" — T3/T4 have long been on master.
13. **`crates/molt-ui/src/lib.rs:4439` (`cw_ritual_hint_sim`)** — "Real
    members over SMP come with T3." The text appears only in the test seam,
    but claims a lack of T3 that no longer exists.
14. **`crates/molt-mcp/src/lib.rs:648`** — `read_members` description "mock
    last-seen/presence": since b5a0055 only half true (engine-stamped
    pills; finding 6 describes the open remainder).
15. **`crates/molt-ui-window/ui/app.slint:669, 674`** — "(mock) download" of the
    proposal image: the preview decodes the real, voted-on
    payload bytes locally — no download needed, no mock.
16. **Dead strings:** `mv_later` (`molt-ui:4670`, referenced nowhere any more)
    and the property name `choice-mock-note` (`theme.slint:340` — the text
    itself is honest by now).
17. **`crates/molt-ui-window/ui/app.slint:741, 863, 1889`** — "mock local
    workspace list" / "engine mock-run" / "open (mock empty list)": list and
    create/join runs are real; only the restore run (finding 13) still ticks
    simulated.
