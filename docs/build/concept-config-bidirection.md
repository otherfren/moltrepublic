# Concept: bi-directional config — the app edits `config.toml` at runtime

Status: **implemented** through C1–C3 plus the restart-required surfacing of
C4 (`molt-engine/src/configstore.rs`, `molt-config::update`). `SaveSettings`
persists for real; the watcher mirrors external edits; the settings UI warns
persistently about restart-required changes and guards leaving with unsaved
draft edits. Deliberate implementation deviations from the text below:

* One internal notice command `ConfigNotice { notice }` instead of
  `ConfigPersisted { ok, detail }` — it also carries `config-conflict`.
* Echo suppression compares the **full last-written bytes** instead of a
  digest (the file is tiny; a byte compare is strictly stronger), which also
  replaces the `(mtime, len, hash)` poll tuple: the poll reads and compares.
* Cross-instance safety uses a `<config>.lock` PID file (fail fast naming the
  holder, stale locks swept via `/proc/<pid>`) instead of `flock` — no added
  dependency, and the error can name the PID portably.
* Still open from C4: applying `mcp.token` / `mcp.allow` rotation to the
  *live* MCP acceptor (until then they are honestly listed as
  restart-required) and per-field "needs restart" hints next to each widget
  (today: one persistent warning line naming the changed keys).

## 1. Goal and non-goals

**Goal.** `config.toml` and the running node stay in sync in *both*
directions:

* **App → file**: a `SaveSettings` command (GUI Save button, `save_settings`
  MCP tool — co-equal as always) persists the session settings to the same
  `config.toml` the node was started from, atomically, without destroying
  what the user hand-wrote into the file (comments, ordering, unknown-to-us
  whitespace).
* **File → app**: an external edit (user in an editor, a provisioning
  script, `--repair-config` from a second shell) is picked up by the running
  node, validated, and mirrored into the shared session — so the GUI *and*
  MCP agents see the new values without a restart.

**Non-goals.** Multi-node config sync, config schema migration between
released versions (covered by `salvage`), secrets management beyond what the
file already carries.

## 2. Where this hooks into the existing code

| Existing piece | Role in this design |
|---|---|
| `molt_config::Settings` | the flat, validated value struct (already the `Config` ⇄ session bridge) |
| `molt_config::{parse, render, salvage, is_well_formed, read_settings, backup_path}` | reused unchanged for validation and repair paths |
| `molt_core::SessionSettings` + `Command::SaveSettings` | stays the *only* mutation path (engine actor = single owner) |
| `molt-app` `resolve_config_path` / `load_config` | the resolved path becomes part of the engine's construction, not just a display string |
| GUI `set-path-label` (“Would write to …”) | becomes “Writes to …” — the mock disclaimer dies |

## 3. Design

### 3.1 Ownership: one writer, and it is the engine's agent

The engine actor stays the single owner of the *values*. File I/O must not
run inside the actor (a blocked disk stalls every operator), so a dedicated
**ConfigStore task** owns the *file*:

```text
 GUI ──SaveSettings──▶ engine actor ──StoreRequest──▶ ConfigStore task ──▶ config.toml
 MCP ──save_settings─▶      │  ▲                            │
                            │  └──── ReloadSettings ◀───────┘ (watcher)
                            └──Event::SessionChanged──▶ mirrors
```

* `ConfigStore::spawn(path, initial_bytes) -> ConfigStoreHandle` — one tokio
  task per node, owning an `mpsc::Receiver<StoreRequest>`.
* `StoreRequest::Persist(Settings)` — write the settings to disk.
* `StoreRequest::Shutdown(oneshot)` — flush and stop (clean quit path).
* The watcher half (3.4) lives in the same task: one owner for the file, no
  cross-task file races by construction.

The engine keeps a `ConfigStoreHandle` next to `cmd_tx`. `SaveSettings`
becomes:

1. validate (existing rules; on failure return `MoltError` — the GUI shows
   the error toast, MCP gets the message),
2. mutate `session.settings`, emit `SessionChanged { scope: Full }`,
3. `store.persist(settings.clone())` — fire-and-forget into the queue.

The reply to the operator does **not** wait for the disk (UI latency), but a
follow-up notice reports the outcome: on write success the store sends the
engine an internal `ConfigPersisted { ok, detail }` command which sets
`session.notice = "saved"` (exactly today's notice) or `"save-failed: …"` —
so a failed disk write is *visible*, not silent. `notice` semantics stay as
they are (cleared on navigate).

### 3.2 Writing: format-preserving, atomic, coalesced

**Format preservation.** `render()` produces our canonical file — good for
`--generate-config`, wrong for touching a user-maintained file (it would
delete their comments). The store therefore edits with **`toml_edit`**
(pure Rust, the toml-rs project's format-preserving DOM):

1. parse the last-known-good file bytes into a `toml_edit::DocumentMut`,
2. set exactly the keys that map from `Settings` (one `fn apply(settings,
   &mut DocumentMut)` — the inverse of `Config → Settings`, property-tested
   against it, see §6),
3. serialize. Comments, key order and unknown formatting survive.

Fallback: if the on-disk file is unparseable at write time (user is mid-edit
with a broken file), do **not** guess — write nothing, report
`save-failed: config.toml on disk is invalid; fix it or run --repair-config`.
The session keeps the values; the user's broken file is not clobbered.

**Atomicity.** Standard temp-and-rename in the *same directory*:

```text
write  config.toml.tmp-<pid>   → fsync(file)
rename config.toml.tmp-<pid> → config.toml  → fsync(dir)
```

Crash windows leave either the old file or the new file, never a torn one.
Permissions are copied from the original (the file carries the MCP token —
it must stay `0600`; creation sets `0600` explicitly).

**Coalescing.** The store drains its queue before writing and keeps only the
newest `Persist` — a burst of saves (e.g. an agent scripting settings)
becomes one write. A 250 ms debounce timer after the first request bounds
write frequency without adding user-visible latency.

### 3.3 Echo suppression

Every write the store makes is recorded as `(len, blake3-of-bytes)` (or
SHA-256 — anything cheap; std-only fallback: length + `DefaultHasher` is NOT
enough, use a real digest). When the watcher fires, the store hashes the
file; if it matches the last self-write, the event is *our echo* and is
dropped. This is the load-bearing detail of bi-direction — without it every
save loops back as a reload and, worse, a slow editor + fast agent can
ping-pong.

### 3.4 Watching: mtime polling, not inotify

Use **polling** (2 s interval; `(mtime, len, hash-on-change)` tuple), not
`notify`/inotify:

* deterministic and testable (inject the clock / force a poll),
* immune to editor strategies (vim renames, VS Code truncate-writes, sed
  `-i` creates a new inode — inotify watches need re-arming per editor
  quirk; polling by path does not care),
* one config file: polling cost is unmeasurable; no new native dependency.

Poll loop, on external change (hash differs from both last-read and last
self-write):

1. read bytes; if `!is_well_formed` → keep last good, set
   `session.notice = "config-conflict"`, log a warning, and *keep polling* —
   the user is probably mid-edit; when the file becomes valid the next poll
   applies it. No auto-`salvage` of a file the user is editing.
2. `parse` + validate → diff against current session settings,
3. send the engine an internal `Command::ReloadSettings { settings }` (like
   the tick commands: in the enum, **not** an MCP tool — agents that want a
   reload edit via `save_settings`; add it to the co-equality test's
   INTERNAL list with that reasoning),
4. the engine applies it exactly like `SaveSettings` minus the persist step,
   emits `SessionChanged` — GUI form fields refresh via the existing
   `settings_changed` draft-protection logic (a reload while the user has
   unsaved draft edits keeps the draft; the notice says the file changed
   under them — the user decides by pressing Save or reopening settings).

### 3.5 Hot vs. restart-required keys

Not every key can take effect live. The classification is part of the
concept and is surfaced honestly in the UI (a per-field “needs restart”
hint) and in the reload notice:

| Key | Liveness | Mechanism |
|---|---|---|
| `ui.lang`, `ui.theme` | hot | already session state |
| `storage.workspace_dir` | hot | Open screen re-scans on next visit |
| `storage.s3_*` | hot | read per backup run |
| `mcp.token` | **hot** | listener checks token per `initialize`; move the token into an `ArcSwap<String>` (or `RwLock`) shared with the acceptor so rotation applies to *new* connections immediately; existing authed connections stay (documented) |
| `mcp.allow` | hot (new connections) | same shared-state pattern as token |
| `mcp.port` | restart | rebinding a live listener is possible but not worth the states; mark restart-required |
| `node.headless` | restart | mode is chosen at boot |
| `transport.*` | restart until `molt-net` lands; hot rotation is a transport-concept concern |

Restart-required changes still persist and still mirror into the session
(so the settings screen shows the truth) — they just carry the hint.

## 4. Concurrency & parallelism summary

* **Engine actor**: values only; never blocks on disk.
* **ConfigStore task**: owns file writes *and* the poll loop → the file has
  exactly one process-internal owner; sequencing is a free consequence.
* Blocking file I/O inside the store uses `tokio::task::spawn_blocking` (or
  `tokio::fs`); writes are serialized by the task, coalesced by the queue.
* Cross-instance safety: on startup the store takes an advisory lock
  (`flock`) on `<config>.lock`; a second `moltd` on the same config refuses
  to start read-write (clear error naming the PID). This also protects the
  echo-suppression assumption (only *we* and humans write).

## 5. Failure matrix

| Failure | Behavior |
|---|---|
| disk full / EACCES on save | session keeps values; notice `save-failed: …`; retry on next save (no auto-retry loop) |
| file deleted externally | watcher treats as external change to “nothing”; recreate from session on next save; notice |
| invalid TOML appears externally | keep last good, `config-conflict` notice, keep polling |
| valid TOML but invalid values (threshold rules etc.) | same as invalid TOML, with the validation message |
| crash between temp write and rename | stale temp file; startup sweeps `config.toml.tmp-*` older than the boot |
| two nodes, one config | second node fails fast on the flock |

## 6. Testing

All tests run against a `tempfile::TempDir`; the poll clock is injected
(`fn poll_now(&mut self)` on the store, called by tests instead of waiting).

* **Round-trip unit tests**: `Settings → apply(toml_edit) → parse →
  Settings` equality; comments/ordering of a fixture file survive a save
  (golden-file comparison).
* **Property tests** (proptest): arbitrary valid `Settings` round-trip
  through `apply`+`parse`; `salvage` is idempotent; `apply` after `salvage`
  never produces an unparseable file.
* **Echo suppression**: save → force poll → assert no `ReloadSettings` was
  issued (spy on the engine channel).
* **External edit**: rewrite the file with new valid values → force poll →
  session mirrors them; with *invalid* values → session unchanged +
  `config-conflict` notice; file later fixed → applied.
* **Atomicity (fault injection)**: a store test double that panics between
  temp-write and rename; assert the original file is byte-identical after
  recovery.
* **Coalescing**: 100 `Persist` requests in a burst → exactly one write
  (count via an injected write-counter), content equals the last request.
* **Draft protection**: engine-level test — reload while `settings_changed`
  cache differs keeps the GUI draft path intact (extends the existing
  session tests).
* **Co-equality**: `ReloadSettings` joins the INTERNAL list in the existing
  `molt-mcp` guard test — the test fails if someone adds it as a tool
  without deciding that on purpose.
* **E2E (existing harness)**: over MCP, `save_settings` then read the file;
  edit the file externally, `read_session` shows the change.

## 7. Milestones

1. **C1** — ConfigStore task + atomic canonical writes (`render`), notice
   plumbing, flock. `SaveSettings` persists for real. *(smallest shippable)*
2. **C2** — `toml_edit` format-preserving writes + round-trip property
   tests.
3. **C3** — poll watcher + echo suppression + `ReloadSettings` (internal).
4. **C4** — hot-key classification surfaced in the settings UI; token/allow
   rotation applied to the live MCP acceptor via shared state.

Each milestone keeps every existing test green; C1 flips the GUI string
from “Would write to” to “Writes to”.
