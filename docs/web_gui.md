# Web GUI: the Slint window in a browser (WASM + WebGL)

**Status: PLAN, not built (2026-11-17).** Decisions D1-D4 are the user's
(2026-11-17). The plan was reviewed against the code the same day (four
lenses, 29 findings, all folded in). Stage W0's compile and first browser load are
measured (§8.4, 2026-11-18); its populated-content half is open. W1 and
W2's client and spawner seams are built, its pure helpers sit in core,
link classification is an engine read (§6.4), and the platform seam is
built on the native side with paste asynchronous (§6.3); what still ties
`molt-ui` to native is two modules (2026-11-18, §6.7, §10). W4 is built
(2026-11-18, §5.1, §7): the upload store, chunked upload frames, reads
by share id, picture key and last export, and the window on them - a
republic was founded, joined and chatted in with the founder driven
through the door, a file went up from bytes and came down byte for byte,
and pictures show through `read_image_bytes` in both windows. Open
before a browser runs it: W0 (d), the rest of §6.5, W5 and W6.

## 1. Goal

The same Slint window, compiled to `wasm32-unknown-unknown` and rendered
through FemtoVG on WebGL2, running in a browser tab. The engine (relays,
Tor, MLS, storage, chain) stays in `moltd` on the server. The browser is a
third co-equal operator beside the native window and MCP: it drives the one
`molt_core::Command` set through the one engine. The bundle is produced by
the release pipeline next to the native tarball (§8).

Non-goals: a second GUI codebase, an HTML rewrite, rendering on the server,
running any engine crate in the browser.

## 2. What exists (research 2026-11-17)

- **The engine seam is two methods.** `molt-ui` talks to the engine only
  through `WalletHandle::execute(Command) -> Result<Reply, MoltError>` and
  `WalletHandle::subscribe() -> broadcast::Receiver<Event>`
  (`molt-engine/src/lib.rs`). One `subscribe()` exists
  (`molt-ui/src/mirror.rs`, `spawn_mirror`); events are invalidations, the
  mirror re-reads state through commands, and a lagged receiver re-reads
  everything. A lossy event push is therefore enough; no replay log.
- **The contract already serializes.** `Command` (`tag = "cmd"`), `Reply`
  (`tag = "reply"`), `Event` (`tag = "event"`), `SessionView`,
  `SurfaceSnapshot` derive `Serialize + Deserialize`. `MoltError` does not
  (`FeatureDisabled(&'static str)`), and the GUI localizes on its variants.
- **`molt-core` is wasm-clean**: `cargo check -p molt-core --target
  wasm32-unknown-unknown` passes (2026-11-17). `molt-storage`, `molt-net`,
  `molt-engine`, `molt-mcp` are not and never will be; `molt-config` and
  `molt-vault` are not needed in the browser.
- **`molt-ui` is not wasm-clean today**, for three reasons:
  1. hard deps on `molt-engine`, `molt-storage`, `molt-config`, `molt-mcp`,
     mostly for pure helpers: wiki front-matter and links, invite parsing
     (NOT pure; an engine read since 2026-11-18, §6.4), the phrase wordlist, `random_token`, the wake skill
     text, the handle charset refusal, the wallet-keys-damaged string.
     `expand_tilde` is used only to seed the native folder dialog and
     stays native-only (2026-11-18: the pure helpers moved to core and
     `molt-mcp` left the dependency list; what is left is in §6.7);
  2. a multi-thread tokio `Handle` in `run_app`, ~45 `rt.spawn` /
     7 `spawn_blocking` sites, 47 `invoke_from_event_loop`, plus
     `tokio::sync::{oneshot, Mutex}` and one `tokio::time::sleep`
     (2026-11-18: all of it behind `Ctx::spawn` and `native.rs`, §6.2);
     the OS use of point 3 went behind the platform seam the same day
     (§6.3);
  3. native OS use: `rfd` (7 sites, one of them the blocking form),
     `arboard` (3 callbacks, one a synchronous paste), `std::fs` (9 sites,
     among them avatar and logo reads from engine-side paths), the alert
     sound through `std::process::Command`, `Instant`/`SystemTime`
     (these COMPILE on wasm32 and panic at run time), chrono `clock`.
- **Two process inputs** reach `run_app` from `moltd`, not the engine: the
  config path (also in `SessionView`) and the compile-time
  `embedded_tor_available` flag (nowhere in core).
- **MCP is not the wire.** It is poll-only, its replies are
  `present()`-reshaped text rather than `Reply`, and three commands the GUI
  issues are not tools (`ui_publish`, `read_upload_bytes`,
  `set_wake_command`). Its `INTERNAL` list is a const inside the
  co-equality test body and `Credentials` is private - neither is API
  today.
- **Slint 1.17 on the web**: the workspace feature set (`backend-winit`,
  `renderer-femtovg`) is the documented web path (WebGL2, no WebGL1
  fallback) and builds for wasm32 unchanged (§8.4). One window, bound to
  the element with id `canvas`. No system fonts (Inter is embedded as
  fallback), no accessibility tree, no programmatic clipboard outside a
  browser gesture, emulated IME, no threads, `run()` does not return. The
  `live-preview` stubs cannot RUN in a browser, so the shipped web build
  is always the fully compiled window module; they do type-check on
  wasm32 (§8.4), which keeps everyday checks cheap.
  Upstream calls the web target suitable for tools and secondary
  platforms, not general web apps.
- **`rfd` 0.15 has a wasm backend**: async `pick_file(s)` returning name +
  bytes, and `save_file` + `FileHandle::write` as a download. No folder
  pick, no blocking `FileDialog`.
- **Licence**: the repo is GPL-3.0-or-later and uses Slint under its GPL
  option; serving the wasm conveys object code (GPLv3 §6), so the page
  links the exact source tag and carries the font notices.
- **Pipeline**: `scripts/build-release.sh` is the whole pipeline (no CI)
  and doubles as the third-party verification recipe for the native
  tarball. `wasm-bindgen 0.2.126` is in `Cargo.lock` through Slint.

## 3. Decisions

- **D1 Wire.** A new WebSocket endpoint in `moltd` carrying the serde
  contract directly, with event push. MCP is untouched.
- **D2 Reach.** v1 is loopback only: a separate `[web]` listener, off by
  default, bound to `127.0.0.1`; remote use goes through an SSH tunnel
  (any local port). A non-loopback bind or TLS needs its own ADR first.
- **D3 Files.** Content moves as bytes through the browser's own picker
  and downloads. Whatever names a server directory (`workspace_dir`,
  mirror dir, download dir, the wiki export folder) stays a typed server
  path with no picker.
- **D4 Measure first.** One wasm32 window compile is measured, and the
  real window loaded in a browser, before §8's profile is final (W0).

Defaults taken without a question (reversible, argue if wrong):

- The bundle is served by `moltd` from a directory on disk, not embedded
  with `include_bytes!` - embedding would make the native bytes depend on
  the wasm build.
- `molt-ui` itself becomes target-generic; no parallel "web slice" of the
  GUI logic that could drift.
- Several tabs may be open, but they are not independent: screen,
  surface, view, language, theme and the wizard state (a founding's
  phrase included) are the one node-wide `SessionView`, so every tab and
  the native window mirror each other - what MCP already does. Accepted
  for v1; per-connection view state would be its own change.
- Only the native window publishes `UiPublish` (§5.5).
- The web bundle has its own release entry point so the native
  verification recipe does not grow wasm tools (§8.2).

## 4. Architecture

```
native:  core -> config -> storage -> net -> engine -> mcp -> remote -> app
                                              ui-kit -> ui ----------^
wasm32:  core -> ui-kit -> ui -> web          (ui-window beside ui, slint only)
```

- **`molt-core`** gains the wire frames (§5.1, built) and the pure
  helpers `molt-ui` needs (built 2026-11-18), with the former owners
  re-exporting so one source stays: the wiki header parser and emitter
  (`wiki_front_matter`; core gains `yaml-rust2`, default features off,
  pure Rust, no I/O) and the body-link parser (`wiki_links`), both still
  reachable as `molt_engine::…`; `HANDLE_CHARSET_REFUSAL`;
  `WALLET_KEYS_DAMAGED`, the const `StorageError`'s Display uses;
  `kanban_wake::{WAKE_SKILL, WAKE_SKILL_FILE, fill_wake_skill}` with the
  skill text beside them, re-exported by `molt-mcp`; and the hoisted
  `INTERNAL` list beside `Command::variant_names()` (with its per-entry
  rationale comments; the co-equality test consumes it from there).
- **`molt-ui-kit`** (new, wasm-clean, depends on core only, nothing below
  `molt-ui` depends on it): the engine-client and platform seams (§6) and
  the remote client's protocol core, sans-IO - frame encode/decode, the
  id-to-waiter table, JSON+binary pairing, lag, the hello/welcome check -
  unit-tested natively.
- **`molt-remote`** (new, native): the WebSocket server and static bundle
  serving. Depends on engine (the handle) and on mcp for one new narrow
  pub function, `seat_token_matches(&WalletHandle, &str) -> bool` (live
  key, constant-time, false on an empty key, seat scope only) and for
  `tools()`. `molt-app` hands it `cfg!(feature = "embedded-tor")` at
  listener start, as it does `run_app`.
- **`molt-ui`**: engine/storage/config/tokio/arboard are
  `cfg(not(target_arch = "wasm32"))` dependencies (done 2026-11-18;
  `molt-mcp` is a dev-dependency only; `rfd` joined the gated set with
  §6.3), used only by `native.rs` and the native arm of `engine.rs`
  (§6.7); shared code uses no `tokio::` path and no OS call.
  The wordlist lookup is two `bip39` calls made directly (done). Native
  behaviour stays byte for byte except where §6 says otherwise (paste,
  link classification, the secret draft).
- **`molt-web`** (new, `cdylib`): `#![cfg(target_arch = "wasm32")]` with
  every dependency under the wasm32 target table, so a native workspace
  build sees an empty crate. Holds the `#[wasm_bindgen(start)]` entry, the
  web-sys socket adapter over the kit's protocol core, the browser
  platform impl and `index.html`.

Workspace rules for the new members:

- every wasm32 invocation that compiles the real window names
  `-p molt-web` - never `-p molt-ui` or `-p molt-ui-window` alone (web-sys
  feature unification would rebuild the window module, the trap CLAUDE.md
  records for the solo native window build);
- no crate adds a slint feature on any target;
- `wasm-bindgen = "=0.2.126"` is pinned in `[workspace.dependencies]`, so
  a Slint bump that moves it fails loudly and the CLI pin moves with it;
- a graph test modelled on `ring_free_guard.rs`: `cargo tree -p molt-web
  --target wasm32-unknown-unknown -e no-dev -i <pkg>` is empty for
  `molt-engine`, `molt-storage`, `molt-config`, `molt-net`, `molt-mcp`,
  `tokio`, `arboard`.

The engine actor is unchanged. `molt-remote` holds a `WalletHandle` like
MCP does and never touches engine state directly.

## 5. The wire

### 5.1 Frames

One WebSocket, JSON text frames for control, binary frames for bulk bytes.

```
client -> server   {"hello": {"v": 1, "token": "..."}}                first frame only
server -> client   {"welcome": {"v": 1, "build": "0.0.18",
                                "caps": {"embedded_tor": true}}}       or close 4401
client -> server   {"cmd": {"id": n, "cmd": { ...Command... }}}
server -> client   {"reply": {"id": n, "result": {"Ok": { ...Reply... }}}}
                   {"reply": {"id": n, "result": {"Err": { ...MoltError... }}}}
server -> client   {"event": { ...Event... }}
server -> client   "lag"                                              the client re-reads everything
```

Built: `molt_core::wire` (`ClientFrame`, `ServerFrame`, `Caps`, `Chunk`),
shapes pinned by test.

- Requests are answered out of order; `id` correlates.
- `lag` maps `broadcast::RecvError::Lagged` and a slow socket alike onto
  the mirror's existing full re-read. The server never buffers events
  without bound: a full per-connection queue drops events and sets `lag`.
- **Version**: `build` must equal the bundle's compiled-in version; on
  mismatch the page reloads once, then shows `version mismatch`. This
  catches a stale bundle; it is not an integrity check (§5.4). No
  cross-version compatibility is promised on this wire.
- **Errors cross as themselves.** `MoltError` derives serde (built): its
  three `&'static str` payloads became `Cow<'static, str>`, so there is
  no twin type to drift and `localize_error` is unchanged. A test walks
  every variant through a round trip.
- **Bytes, both directions, always chunked** (built 2026-11-18): bounded
  binary frames `le64 id ‖ le64 offset ‖ flags ‖ bytes` (flag 1 = last
  chunk), so neither side holds a whole file in one frame. Server to
  client: the bytes of a `Reply::UploadBytes` follow their reply as
  chunks under the reply's id. Client to server: each chunk is its own
  `upload_chunk` command whose JSON carries no bytes, followed by exactly
  one chunk frame with that command's id, offset and last flag; each is
  answered by `upload_progress`, which is the backpressure. A frame that
  is not exactly the announced one closes 4400. The bounds live in
  `molt_core::wire`: `MAX_TRANSFER` 64 MiB per transfer either way (the
  engine has no default file cap), `CHUNK` 256 KiB; a read's
  caller-supplied `cap` is clamped to the ceiling. `file_cap_bytes`
  applies at `share_upload`, as for `share_file`; `upload_begin` enforces
  only the 64 MiB ceiling and the store quota (a node with
  `file_cap_bytes = 0` accepts the upload and refuses the share). A file
  over the ceiling shows as `too large for the browser`; the server-path
  flow remains for those. An upload belongs to the session that began
  it; a session's end aborts what it did not share, including an upload
  whose `upload_begin` was still unanswered when the socket closed.
- **Frame bounds**: the socket is built with an explicit
  `WebSocketConfig` (never tungstenite's 64 MiB default; `relay_ws.rs` is
  the model): 1 MiB per frame (the MCP line bound), replies' bytes in
  256 KiB chunks. tokio-tungstenite cannot change the bound after the
  handshake, so before `welcome` the socket can buffer up to 1 MiB; a
  hello over 4 KiB is refused once read.

### 5.2 The command gate

`Command` deserializes every variant, including the ones that speak AS
someone else. The gate is an **allow-list that fails closed**: a command
executes only if its outer `cmd` tag is

- the command of a seat-scope tool in `molt_mcp::tools()`, or
- on `WEB_DOOR`, a production const in `molt-remote`:
  `read_upload_bytes`, `set_wake_command`, and the byte doors of §7
  (`upload_begin`, `upload_chunk`, `upload_abort`, `share_upload`,
  `read_file_bytes`, `read_image_bytes`, `read_export_bytes`), none of
  which is an MCP tool (the link read of §6.4 is a tool,
  `classify_link`, so it needs no entry).

Everything else is refused before execution. `as_operator` is refused;
should a wrapper command ever be allowed, the gate must recurse into its
inner command.

A test in the style of the co-equality test pins it: tools and `WEB_DOOR`
are disjoint; every `Command::variant_names()` entry is either allowed or
on `INTERNAL` minus `WEB_DOOR`; every command `molt-ui` issues is allowed.
A new `Command` breaks the test until it is classified. Until the window
used them, the seven byte doors sat on a pinned list in that test; it
is gone (2026-11-18): the scan covers `molt-ui` and `molt-ui-kit` (the
upload steps are constructed in the kit), and a web door the GUI never
uses fails the test again.

**Scope is ADR-0007: a tab holding the seat token operates the machine.**
Reachable by design: host posture through `patch_settings` (rotating or
clearing the seat key included), any-path file read and write,
`reveal_seed`, and the wake command (run through `sh -c`, fired by
`test_wake`). `WEB_DOOR` adds no reach over MCP - the wake value is
already on the settings surface and both doors go through the same
validation. The seat token is the boundary; the loopback bind and the
Origin/Host check only limit who can present it. The "web door" ADR
ratifies exactly this.

The window's quit is not a `Command`; the web build hides the control.

### 5.3 Auth and the listener

- `[web] port` (unset = no listener) and `[web] dir` are start-time config
  only: not in `Settings`, not reachable through `patch_settings`, changed
  by editing the file and restarting (the rule `mcp.port` follows). Bound
  to `127.0.0.1` only in v1; anything else is refused at start with one
  line.
- While the seat key is empty the door serves nothing: every request,
  the page included, answers 503, and a cleared key closes every session
  at its next check (built this way instead of unbinding the port; the
  exposure is the same and there is no rebind race).
- The token travels in the first frame, never in the URL, a cookie or a
  header; 10 s pre-auth deadline; checked through `seat_token_matches`.
  The read key gets no web door in v1.
- **Rotation closes actively.** The socket is a push channel, so MCP's
  next-request re-check is not enough: each session keeps the token it
  presented and re-validates it on every inbound frame and, in its event
  forwarder, on every `SessionChanged{scope: Full}` and on lag, BEFORE
  anything is written. A token that no longer matches closes the socket
  with 4401; the triggering event is not forwarded.
- **Origin and Host, compared as parsed values.** Both parse with
  `url::Url`, the parser browsers use (no hand-rolled parsing; rmcp's own
  check is private and not reusable). Host must be exactly `localhost`,
  `127.0.0.1` or `[::1]` in canonical spelling (`127.1`, `0x7f.0.0.1`, a
  percent-escape or a padded port are refused), any port (a tunnel on
  another local port works). On the upgrade, Origin is
  required, scheme `http`, host and port equal to Host's (missing port =
  80 on both sides); absent or `null` is refused. On static GETs Host is
  always checked and Origin only when present. WebSockets are not covered
  by CORS, so this is what stands between a hostile page in the same
  browser and the port.
- **Upgrade glue** (decided, built): hyper's upgrade plus tungstenite's
  `derive_accept_key` / `WebSocketStream::from_raw_socket`, both already
  in the graph. `hyper-tungstenite` was left: what it adds over those two
  is the four request-header checks of RFC 6455 §4.2.1, about ten lines
  here, against a new dependency on the seat's door. The protocol itself
  stays tungstenite's.
- **Connections**: WebSocket sessions have 64 slots and the 10 s pre-auth
  deadline. Known and accepted: a LOCAL process (it can forge Origin; a
  web page cannot) can hold those slots with silent sockets, 10 s at a
  time. Static requests are unauthenticated by design, short-lived
  (header read timeout, no long keep-alive) and cannot exhaust the
  session slots.
- Static files: a fixed file list matched on the raw request target, no
  directory walking; the bytes are read ONCE at start (§5.4), so nothing
  on disk is followed per request. A `Sec-Fetch-Site: cross-site` request
  is refused and files carry `Cross-Origin-Resource-Policy: same-origin`,
  so another site cannot probe for a running node. One HTTP exchange has
  60 s in total. Headers: CSP `default-src 'self'; script-src
  'self' 'wasm-unsafe-eval'; frame-ancestors 'none'`,
  `X-Content-Type-Options: nosniff`, `Cache-Control: no-store`.
- The page keeps the token in memory only and asks on every load.
- The `ring_free_guard` must stay green.

### 5.4 The bundle is trusted like the binary

The seat token is typed into whatever `[web] dir` serves. `moltd` refuses
to open the web listener (one log line, the node stays up) if the
directory or a listed file is a symlink, group- or world-writable, or
owned by anyone but `moltd`'s user or root (built: `load_bundle`). The default `[web] dir` is `web/` beside the
config file, where the web tarball is unpacked - never a shared location.

The door's rules are in `docs_archive/security/mcp-security.md` ("The web
door") and its decision in
`docs_archive/adr/0008-web-door-second-remote-seat-door.md`.

### 5.5 `UiPublish`

The engine has one snapshot slot, read by `read_ui_state` (test
instrumentation for `gui_walk.py`). The browser does not publish. Driving
the web GUI over MCP would be its own change.

## 6. Making `molt-ui` target-generic

All seams land on the native build first, each green before any wasm code
exists.

### 6.1 Engine client

A `Clone + Send + Sync` client on both targets: `execute(Command) ->
impl Future<Output = Result<Reply, MoltError>> + Send`, and an event
stream of `Event | Lag`. Native calls `WalletHandle` directly (no
forwarding task). The remote client is a channel handle; one pump task
owns the socket (on the web a `spawn_local` task holding the web-sys
socket, so nothing `!Send` leaks into the 34 functions that take a handle
today). No cfg-split bounds.

### 6.2 Spawner and shared primitives

**Built (2026-11-18), native side.** `Ctx::spawn(future: Send +
'static)` is the one place the window starts a future; it forwards to a
`Spawner` (`molt-ui/src/native.rs`: the tokio handle; on the web a unit
type over `slint::spawn_local`, same method, no call site changes).
`run_app` takes `impl Into<Spawner>`, so `moltd` still passes its handle.
Shared code names no tokio - a test scans the sources for it
(`tests/gui/spawner.rs`, which also pins that a spawned future runs to
completion):

- the two oneshots in `mirror.rs` are `futures-channel`, the kanban
  `DraftSaver` lock is `futures_util::lock::Mutex` (runtime-neutral; the
  generation check, not lock fairness, keeps an older save from landing
  late);
- `native.rs` holds what is left: `off_thread` (image fit and decode -
  inline on the one browser thread, W5 measures whether large images
  need chunking) and the in-process engine's event receiver; the file
  reads, the save-as write and `sleep` went on into the platform seam
  (§6.3);
- the wake-skill save dialog moved from the blocking `rfd::FileDialog`
  to the async form, like every other dialog.

Open: the web `Spawner` and the wasm twin of `native.rs` (W5); the
`cfg(not(target_arch = "wasm32"))` on the module and on the tokio
dependency lands with them. The direct `std::fs` / `std::thread` lines
that stayed in shared code went behind the platform seam (§6.3), and the
`clippy.toml` lint holds the rule by resolved path (§8.3).

### 6.3 Platform

**Built (2026-11-18), native side.** One trait for what the OS did,
`molt_ui_kit::platform::Platform`: clipboard copy and paste, play the
alert, wall clock (`unix_now`), monotonic clock, sleep, random token,
folder pick, and the files (below). It sits in `molt-ui-kit` because that
crate is wasm-clean and both `molt-ui` and the later `molt-web` depend on
it; `molt-web` installs its implementation with
`molt_ui_kit::platform::install` before the window starts. The native
implementation is `NativePlatform` in `molt-ui/src/native.rs` - the code
that was spread over `app.rs`, `alerts.rs`, `settings.rs` and the action
modules, moved - and is what `molt-ui`'s `platform()` falls back to when
nothing was installed, so `moltd` changes nothing.

- **Every future the trait returns is `Send`** (`Task<T>`), so the
  window's shared code keeps one bound on both targets. A browser
  implementation runs its `!Send` JS futures on its one thread and
  answers through a channel.
- **One slot per process**, not a field of `Ctx`: the clock is read in
  free functions far from any callback (`labels.rs`, `surfaces.rs`). The
  clipboard wiring takes its platform as an argument
  (`actions/clipboard.rs::wire_with`), so a test hands it a fake and
  never touches the user's clipboard.
- **Files** go through `rfd::AsyncFileDialog`, now named only in the
  native implementation: `pick_file` answers what NAMES the file (its
  path natively), `read_file` gives its bytes, `save_file` takes a name
  and bytes and answers where they went (a download on the web),
  `pick_save` and `pick_folder` answer a path for an engine on this
  machine and are `None` where there are no such paths. What follows the
  last `/` or `\` of a `pick_file` answer is the file's name (a browser's
  answer has no directory). `local_bytes` / `local_stamp` left the trait
  (2026-11-18): pictures are read through the engine on both targets
  (§7). The wake-skill save keeps its refusal to write through a link
  (`Save::refuse_link`). The mirror-folder dialog now seeds its start
  directory like the workspace one (tilde expanded, only when it exists).
- **Clocks**: no `Instant`, `SystemTime` or `Utc::now` in shared code.
  The alert debounce and the wiki draft's age are `Duration`s on the
  monotonic clock. `patchview.rs` uses `similar`'s own
  `TextDiff::configure().timeout(..)`.
- **Alert**: the debounce and the pure-Rust WAV (`alerts::alert_wav`)
  stay shared; the platform plays it (native: the cached file and the
  system player, as before).
- **Paste is asynchronous on both targets** - a `.slint` interface
  change: `callback paste-clipboard(PasteTarget)` with no return value,
  `PasteTarget` an enum over the four fields (`rw-seed`, `rv-link`,
  `bk-restore-seed`, `decrypt-phrase`). Rust (`land_paste`) sets exactly
  that property when the text arrives, and only in the opening the paste
  was asked from: a browser may answer late, and a phrase must not wait
  in a closed dialog nor land in the next one (the phrase dialogs serve
  every workspace and every backup row). Two checks: the field is on
  show now (`rw-seed-on-show` / `rv-link-on-show` - screen, run state and
  wizard step - or the dialog's open flag), and the window's
  `paste-opening` still reads what it read at the click; every open,
  close, screen change and wizard step moves that counter. `rw-seed` and
  `decrypt-phrase` became `in-out` for this; nothing mirrors them
  (`rw-phase` is `in-out` for the tests alone).
- A test pins that shared code names none of them
  (`the_os_sites_outside_native_are_the_listed_ones`,
  `tests/gui/spawner.rs`): the last line, `current_dir` in `run_app`,
  moved to `native::absolute` (2026-11-18). The `clippy.toml` lint
  (§8.3) holds the same rule by resolved path.

Pins: `crates/molt-ui/src/tests/gui/paste.rs` (the text lands in the
named field and in no other; a field that left the screen gets nothing,
nor does one reopened or on another wizard step since the click;
each of the four buttons names its own field, by real clicks; the
callback reads the platform it was wired with),
`crates/molt-ui/src/tests/gui/spawner.rs` (the native clocks, token and
file reads; the debounce; the WAV; the scan). NOT covered, as for every
window read: the `invoke_from_event_loop` hop in front of `land_paste`.

Open:

- the web implementation (W5: `navigator.clipboard`, WebAudio,
  `web_time`, `crypto.getRandomValues`, `rfd`'s wasm backend with no
  features); chrono gets `wasmbind` on wasm32 for `Local`; `similar`
  gets `wasm32_web_time` so the diff timeout is real there. If a browser
  refuses `readText()` from a Slint click (W0/W5 check), the web hides
  the paste buttons and relies on Ctrl+V into the focused input.
- built (2026-11-18, §7): behind the web door the chat share, the
  download and the export hand bytes to and from the platform, so a
  `pick_file` answer need not be a server path and `pick_save` may be
  `None`. For W5: `read_file` should refuse a file over the 64 MiB
  ceiling before it reads it into memory (the shared code checks after
  the read), and `save_file` is called outside a click - phase two of a
  download arrives on an engine event, an export after polling - which
  a browser must be checked to allow.

### 6.4 Synchronous callbacks and link classification

`seed-matches`, `seed-rows`, `vt-*` and `folder-preview` (a string format
over `molt_core::slugify`) are pure and stay local.

**Built (2026-11-18).** `classify-link` could not stay local.
`FoundingInvite::parse` / `RecoveryInvite::parse` end in molt-net's
handover decoders, which validate the npub through `canonical_nostr_pk`
on the C `secp256k1` (ADR-0002). Moving that to core would drag the C
library into the wasm build; a second, weaker parser is what CLAUDE.md
forbids. So classification is a stateless engine read on BOTH targets:

- `Command::ClassifyLink { link }` answers `Reply::Link(LinkView)`: the
  trimmed link echoed, `kind` (0 nothing usable, 1 invite, 2 recovery),
  `republic`, `who` and the link's `relays`. The handler
  (`cmd_classify_link`) calls the two parsers and nothing else. It is an
  MCP tool, `classify_link`, seat scope: an agent about to call
  `join_start` or `recover_start` gets the same pre-check, and the read
  tells a caller nothing it did not hand in. A tool is admitted by the
  gate as such, so it needs no `WEB_DOOR` entry. The wake script denies
  it with the other ritual tools.
- The window holds the answer in `rv-verdict`, a property Rust sets;
  the `pure callback classify-link` is gone. `LinkPreview` carries the
  field text it is for, and the window derives `rv-kind` from it: a
  verdict for any other text than the field holds reads as kind 0. That
  comparison is a binding, so it holds in the gap before Slint's
  deferred `changed rv-link` handler runs.
- Rust (`actions/ritual.rs`) clears the verdict and the link's relay
  rows in the change handler, before it sends the read, and
  `land_link` drops an answer whose echo is not the field's trimmed
  text. An unknown `kind` lands as 0. While a read is out the panel
  shows no verdict line at all (not "unrecognized"). Blank text is
  never read: the handler judges it itself, as no link, so the line
  shows at once.
- A failed read (`link_failed`) leaves the panel unjudged and toasts
  once per outage; a failure for a link the field has left is dropped.
  OPEN: nothing re-sends the read when a door connection returns - the
  client seam has no reconnect yet, so the user edits the field.
- Seat rows carry `joinable` (`RitualSeatView`, `#[serde(default)]`),
  set by the engine wherever it sets the link; the mirror shows a link
  to copy only then.
- `molt-ui` no longer names the invite parsers (§6.7).

Pins: `crates/molt-engine/src/tests/join_tests.rs` (`classify_link_*`,
the seat flag), `crates/molt-ui/src/tests/gui/link_read.rs` (join vs
recovery, a link that cannot act arms nothing - both through a real
engine - `a_stale_verdict_never_arms_the_panel`,
`a_blank_link_is_judged_without_a_read` and
`a_failed_read_toasts_once_and_arms_nothing`),
`the_link_read_answers_through_the_web_door`
(`crates/molt-ui/src/tests/gui/remote.rs`). NOT covered, as for every
window read: the `invoke_from_event_loop` hop between the reply and
`land_link`, which the headless backend never drains.

### 6.5 Write-only secrets in the settings form

`mcp_token`, `mcp_read_token`, `s3_secret_key` and `wallet_daemon_login`
never serialize, so a remote client's form holds them empty. Today's save
is three commands and the middle one, `SetNodePosture`, is built by
`NodePosture::of(&settings)` with all three posture secrets as
`Some(form value)`; the engine stores `Some("")` verbatim. **Unchanged,
one Save in the browser would empty the seat key - closing MCP and the
web door itself - revoke the read key and wipe the S3 secret.**
(`keep_stored_secrets` only covers the `SaveSettings` leg.)

- Built: `save_draft` takes what the form was filled from and sends a
  secret only when the draft differs from it; untouched goes as `None`
  on both targets (in the native window an untouched secret equals the
  stored one, so nothing changes there). Pinned by a GUI test through a
  real web door. Still open for W5: a web form cannot CLEAR a secret this
  way (empty equals what it was shown), so revoking the read key and
  removing the S3 secret need their own explicit actions there.
- The web door refuses an empty `mcp_token`, so a client bug cannot close
  its own listener.
- **Rotate** on the web: the page mints the key through the platform
  seam, shows it with a copy step BEFORE issuing the write, then
  reconnects with the new key after the server closes the session. A
  failed write keeps the old key.
- The web form cannot show a stored key (stated in §9).
- Pinned test-first in W2: Save, Save & continue and read-key Revoke
  through the remote client leave the stored secrets as they should be.

### 6.6 Startup inputs

**Built (2026-11-18).** `embedded_tor_available` comes from
`welcome.caps`; the config dir and file come from
`SessionView.config_path` (already absolute engine-side; there is no
`current_dir` in a browser). Native keeps passing both.

- `RemoteHandle::welcome()` holds the server's welcome for every clone of
  the handle; `Engine::remote_caps()` is `None` for the engine in this
  process.
- The mirror's session landing (`mirror::land_session`) applies both for
  a remote engine on every full session push: a reply is in hand by then,
  so the welcome has arrived. It is handed the ENGINE and reads the
  welcome itself, so a caller cannot leave it out. A native window is
  never touched by it.
- The SERVER's path is split by string on `/` or `\` (`config_location`),
  not `Path`: its separator is not the browser's. Native absolutizes
  with `current_dir` first, as before, and splits its own path with
  `Path` (`local_config_location`; on Unix a backslash is a file-name
  character).
- Pinned by `a_window_behind_the_web_door_takes_its_startup_inputs_from_the_server`
  (`tests/gui/remote.rs`), which calls `land_session` directly. NOT
  covered: the `invoke_from_event_loop` hop in `push_session` that calls
  it - the headless backend never drains that queue, the same limit every
  mirror test has.

Open (W5): what `run_app`'s web twin is handed in place of the two
arguments; until the first session lands the footer is empty and the
embedded row shows the window's default (greyed).

### 6.7 What still ties `molt-ui` to native (2026-11-18)

Two modules, outside `src/tests/`: `native.rs` (the spawner, off-thread
work and `NativePlatform`) and the `Native` arm of `engine.rs`. Pinned by
`the_higher_crate_uses_outside_native_are_the_listed_ones` and
`the_os_sites_outside_native_are_the_listed_ones`
(`tests/gui/spawner.rs`); the lists only shrink, and the second is
empty. `clippy.toml` (§8.3) allows the listed calls in `native.rs`
alone.

| What | Where | Purpose | Goes with |
|---|---|---|---|
| `molt-engine` | `engine.rs` | `WalletHandle`, the in-process engine | W5 (cfg on the `Native` arm) |
| `molt-storage`, `molt-config`, `tokio`, `arboard`, `rfd` | `native.rs` | `expand_tilde`, `random_token`, the runtime, the clipboard, the dialogs | W5 (cfg on the module, a wasm twin for the spawner and `off_thread`) |
| `std::fs`, `std::thread`, `std::process`, `std::env`, `Instant`, `SystemTime` | `native.rs` | file reads and writes, the alert player, the clocks, `absolute` for the native config path | W5 (same cfg) |
| `native::absolute` | `app.rs` (`run_app`) | absolutizes the native config path | W5 (native-only entry) |
| chrono `Local` | `labels.rs` | local time display | W5 (`wasmbind` on wasm32) |

Not in `molt-ui` at all: `molt-net` (dev-dependency), `molt-mcp`
(dev-dependency since 2026-11-18), no audio crate (the alert runs a
system player).

Measured 2026-11-18 (`cargo check -p molt-ui --target
wasm32-unknown-unknown --features molt-ui/live-preview`, type check
only): 18 errors, 17 of them inside `native.rs` (5 `tokio`, 5 `arboard`,
4 `rfd`, 2 `molt-config`, 1 `molt-storage`) and 1 `molt-engine` in
`engine.rs`. Everything else type-checks for wasm32 (22 before the
platform seam, 27 before §6.4; unchanged after the lint pass and after
the window's byte flows of §7, whose module type-checks for wasm32).
chrono's `Local` compiles and fails at run time, so the compiler does
not count it.

## 7. Files as bytes

The byte doors are `Command`s, classified in the gate test, chunked and
capped per §5.1. Engine, door, client and the window's use of them are
built (2026-11-18; rules in `docs_archive/security/mcp-security.md`,
"The byte doors").

**One flow per feature, one fork in it** (`molt-ui/src/bytes.rs`): an
engine in this process shares the window's disk, so content crosses as a
path; one behind the door gets bytes. The fork is `Engine::is_remote`,
not the target: a native window on a remote engine takes the byte side
too, which is what the tests run.

| Feature | Engine in this process | Engine behind the door | Decision |
|---|---|---|---|
| Share a chat file | `ShareFile{path}` | the platform reads the picked file, `upload_begin` / `upload_chunk` into `<workspace>/uploads/<id>/<name>`, then `share_upload` | native KEEPS the path: hashed in place, no copy into the store, no 64 MiB ceiling |
| Download a chat file | save dialog, then `DownloadFile{dest}` | two phases: `DownloadFile` into the server's download dir with its `FileTransfer` progress row, then `read_file_bytes` by the share's id and the platform's file save. A share the server already holds (own, downloaded, mirrored) skips phase one | native KEEPS the path: the engine writes where the user points, no ceiling, no second copy |
| Avatar and logo display | `read_image_bytes` | `read_image_bytes` | BOTH moved: the mirror's `std::fs::read` on the UI thread and the two platform methods behind it are gone, and a republic that is not stored no longer has a proposed display value opened as a path |
| Set avatar, logo | picked file read through the platform, checked and fitted in the window, `bytes_b64` in the proposal | the same, the fitted bytes as an upload and `upload` in the proposal | one flow, the CARRIER forks: inline works without a stored workspace and is what the engine always took; behind the door a text frame holds 1 MiB |
| Wiki image | a chat share referenced from the page | the same share flow as above | nothing of its own |
| Wiki inline images | `ReadUploadBytes` | unchanged, chunked | - |
| Workspace export | typed path, `ExportWorkspace` | a BARE NAME: `export_workspace`, the session's export state polled until it is not running, `read_export_bytes`, the platform's file save. A typed path stays a server path; "bare" is the engine's own reading (`molt_core::is_bare_name`, one predicate for both sides) | native KEEPS the path. Behind the door the dialog's default is the bare name |
| Wake skill export | the window holds the text and saves it through the platform's file save (since W3) | the same | no door, nothing forked |
| Wiki export | `WikiExport{dest}` writes a directory tree | typed server path only (`wiki_export_plan.md` decided "plain files, no archive"); a browser download is out of v1 | - |
| Mirror dir, workspace dir, download dir | folder picker beside the typed field | typed server path; browse buttons hidden | - |

- **Pictures on display.** The gather pass reads each picture through
  `read_image_bytes` once and keeps the bytes (`bytes::hold_pictures`);
  the apply decodes. A replaced picture keeps its file name, so the
  cache key is the `image` value plus a hash over the applied picture
  changes of the Organization log (`pictures_version`), which the
  snapshot already carries. A picture the engine REFUSED (a republic
  that is not stored names a display value, never a file) is remembered
  under that key and not asked for again until the pictures change
  (2026-11-18, review fix: it cost one round trip per picture per
  surfaces pass); a read that failed otherwise - a dropped connection, a
  file that is not there - is asked for again on the next pass. The engine reads
  the file behind its storage writer, so the read that follows an
  applied change sees the new file
  (`a_picture_read_waits_for_the_writer`).
- **Phase two of a download** hangs on the mirror's one event stream:
  the window notes the share it asked for (`ChatUiState::to_user`), and
  the `FileTransfer` done event for it starts the read and the save;
  the engine-path toast is replaced by the save's own. A failed or
  cancelled fetch forgets the note. The note is per workspace and per
  window: after a switch or a reconnect the fetch still finishes on the
  server, and the next click hands the file over in one step.
- **What stays on the server**: phase one's file in the download dir and
  a bare-name export's blob there. Nothing removes them; the export note
  names the path.
- **The download dir** has no window control (`cfg-download-dir` only
  round-trips the settings draft) and none was added: phase one uses the
  configured folder.
- **Refusals** show as `Too large for the browser (64 MiB at most).`
  (lexicon `toast_too_large_web`, checked in the window before an upload
  and before a read) and, for the store's own (`no stored workspace is
  open`, `8 uploads already open`, the store full), as the engine's line
  with a German rendering.
- The upload store is durable state, not temp handling: a shared upload
  lives for the share's life and is deleted on `RemoveFile`, a deleted
  message, a delete vote and the expiry; a failed share drops its upload
  at once; an upload idle for 15 minutes or left by a closed workspace
  or a crash is removed. Quota: 64 MiB per upload, 8 open uploads, 1 GiB
  per workspace. It needs a workspace on disk and is not part of an
  export or a backup.
- The store deletes only a directory it minted
  (`<store>/<minted id>/<name>`, compared by component), and
  `share_file` refuses a path under `<workspace>/uploads`.
- After an accepted `share_upload` the upload id is spent for the
  session. If the share then fails (session notice
  `share-failed:<name>:<reason>`), the upload is gone and the file must
  be shared again.
- An offline sharer makes phase one of a download wait without deadline,
  as today; the web shows the same progress row.
- **Built (2026-11-18): server paths are typed.** The mirror folder was
  a read-only label plus a picker-only button; it is a field with its
  own Apply on a second line of the mirror row (`org-mirror-dir-draft`,
  `set-mirror-dir(string)`, the existing `SetMirrorDir`; an empty path
  is the default folder, and the engine refuses one that is neither
  absolute nor `~`-led, on every surface -
  `mirror_worker.rs::a_relative_mirror_folder_is_refused`). The draft follows the stored folder only when
  that changes, like the quota. One window property, `folder-browse`
  (default true), takes every folder dialog button away - mirror row,
  workspace-folder dialog, wiki export dialog - and leaves the typed
  form; `apply_remote_startup` clears it for any window behind the web
  door, so `molt-web` needs no line for it past the first session (it
  may set it before). The export dialog reads the same property for its
  default target. Pins: `tests/gui/files.rs`
  (`the_mirror_folder_is_typed_and_applied`,
  `one_property_hides_every_folder_dialog_button`,
  `a_typed_mirror_folder_reaches_the_engine_and_comes_back`) and the
  startup test in `tests/gui/remote.rs`.
- A typed path that names the folder already in use in another spelling
  (`~/x` for its expansion) is stored, but the view does not change, so
  the draft keeps its spelling and Apply stays lit.
- No door accepts a server path from the browser that MCP would not
  accept.

Pins (`crates/molt-ui/src/tests/gui/remote.rs`, two real engines over an
in-process relay, each behind a real web door):
`a_file_shared_from_bytes_arrives_and_downloads_byte_identical` (300 KiB
up from bytes, both phases down, the own and the already fetched share
in one step, an unreadable pick), `a_picture_set_from_bytes_through_the_web_door_shows`
(the window's own callbacks; a replaced picture; a logo over 1 MiB; the
carrier fork), `an_export_comes_to_the_user_through_the_web_door`,
`the_engine_in_this_process_shares_and_downloads_by_path`; the key and
name rules in `tests/images.rs`. NOT covered, as for every window read:
the `invoke_from_event_loop` hops (the toasts, the mirror's
`FileTransfer` arm that calls phase two) and the share, download and
export click closures themselves, which only pick and call into
`bytes.rs`.

Open:

- stored uploads do not travel in an export or a backup (a product
  decision, not taken);
- a republic that is not stored has no upload store, so behind the door
  it can neither share a file nor set a picture;
- a picture proposal the engine refuses after its upload went up leaves
  that upload in the store until the idle sweep (15 minutes) or the
  session's end; eight such refusals in that time fill the open-upload
  quota;
- the Organization snapshot carries every applied picture's bytes on
  each surfaces read (it did before; over a socket it is weight W5
  should measure).

## 8. Build and pipeline

### 8.1 Profile

```
[profile.release-web]
inherits = "release"          # panic = "abort", codegen-units = 1, overflow-checks
lto = "off"                   # whole-profile only; measured with off, thin is unmeasured
# strip: "none" is inherited and the binutils post-step does not apply to
#        wasm; W0 picks `strip = true` or wasm-bindgen
#        --remove-name-section --remove-producers-section
[profile.release-web.package.molt-ui-window]
opt-level = "s"               # measured, §8.4
codegen-units = 16            # measured: 26 GiB instead of 32, 11 min instead of 27
```

Cargo cannot set `lto` per package, so "no LTO for the window crate" is
not a lever. Without the name section browser traces show function
indices only; an unstripped `.wasm` is kept locally, as the native build
keeps its debug info. Artefact:
`target/wasm32-unknown-unknown/release-web/molt_web.wasm`.

### 8.2 Script

`scripts/build-release-web.sh`, its own entry point: the native script is
the third-party verification recipe and must not start needing wasm
tools or a second window-scale compile. The releaser runs native, then
web.

- `rust-toolchain.toml` gains `targets = ["wasm32-unknown-unknown"]`.
- Same `SOURCE_DATE_EPOCH`, path remaps, `LC_ALL`, `TZ`, `--locked`.
- Refuses to start while a cargo/rustc is live on the target dir (two
  scripts lose the serial order one script gave; this is the documented
  OOM class).
- `cargo build -j 1 -p molt-web --target wasm32-unknown-unknown --profile
  release-web`, then `wasm-bindgen --target web --no-typescript`, the CLI
  version checked against `Cargo.lock` (mismatch fails and prints the
  `cargo install --locked wasm-bindgen-cli --version =X` line).
- Deterministic tar of `index.html`, the `.js`, the `.wasm`, `COPYING`
  and the font licences into `dist/moltrepublic-web.tar.zst`, then its
  `sha256sum`.
- No `wasm-opt` in v1: one more unpinned tool in the envelope. Revisit
  with the measured size.
- `docs_archive/build/reproducible-builds.md` gains a second, independent
  promise and envelope for the web tarball; the Linux promise text stays.
  `scripts/moltd.service` and the README gain the `[web]` lines.

### 8.3 Everyday checks

- `cargo check --target wasm32-unknown-unknown` for `molt-core` and
  `molt-ui-kit` is cheap and catches a dependency that does not compile.
  It does NOT catch `std::fs`, `Instant::now`, `std::thread::spawn` or
  `tokio::time`: those compile on wasm32 and die at run time.
- So `molt-ui` and `molt-ui-kit` carry a `clippy.toml` with
  `disallowed-methods` (explicit paths: `std::time::Instant::now`,
  `SystemTime::now`, `std::thread::spawn`, `std::process::Command::new`,
  the `std::fs` functions in use, `tokio::time::*`,
  `tokio::task::spawn_blocking`); the native platform module is the one
  `#[allow]`. It runs in the existing live-preview clippy.
  **Built (2026-11-18).** The list also names `std::thread::sleep`, the
  `std::env` reads, the `std::path::Path` stat methods, chrono's
  `Utc::now` / `Local::now`, the tokio spawn and runtime entry points,
  `arboard::Clipboard::new` and the `rfd` dialog constructors. The two
  files are identical (a test compares them); paths of crates one of the
  two does not depend on carry `allow-invalid`. In `molt-ui` the allow
  sits on `mod native;`, on `mod tests;` (test code only) and on the
  native `pickerprobe` example; `molt-ui-kit` needs none. A lint reads
  resolved calls, not types or imports: `use tokio::...` in shared code
  is still caught by the source scan (`tests/gui/spawner.rs`), which
  stays.
- The graph test of §4.
- A wasm check of `molt-ui` against the generated window module would
  cost ~11 GiB. The live-preview stubs type-check on wasm32 (§8.4), so
  they are the stub window: `SLINT_LIVE_PREVIEW=1 cargo check -p molt-web
  --features live-preview --target wasm32-unknown-unknown` in its own
  cache dir (`target/dev-ui-wasm`) becomes a `dev-ui.sh` mode and the
  per-change compile of all wasm-cfg code. `molt-web` carries a dev-only
  `live-preview` passthrough like `molt-app`'s; it is check-only, the
  stubs cannot run in a browser.

### 8.4 Measured (2026-11-17, this box)

- `molt-core` checks clean for wasm32.
- A minimal window with the workspace's exact slint feature set, pinned
  to the repo's `Cargo.lock`, `opt-level = "s"`, thin LTO,
  `panic = "abort"`: builds in 1m43s; `wasm-bindgen-cli 0.2.126` accepts
  it; `.wasm` 10.4 MB, glue `.js` 91 KB. That is the floor (renderers,
  winit, Inter) before the window module.
- Live-preview stubs on wasm32: `SLINT_LIVE_PREVIEW=1 cargo check -p
  molt-ui-window --features live-preview --target
  wasm32-unknown-unknown` passes, 1m26s cold.
- **The real window module** (2026-11-18, 31 GiB box + 32 GiB swap file,
  a throwaway cdylib on `molt-ui-window` + `slint` only, `-j 1`, LTO off,
  `opt-level = "s"` and one codegen unit for the window crate,
  `strip = true`). The generated module is 136 MB of Rust.
  - compile: 27m24s; the window rustc peaked at **31.6 GiB** (RSS + swap).
    A 25 GiB cgroup cap killed it twice.
  - where it goes (`-Ztime-passes`): macro expansion to 5 GiB, type check
    8.4, borrow check 14.4, monomorphization 19 GiB - all before codegen,
    and no profile setting touches that part. Codegen adds the rest.
  - `.wasm` after wasm-bindgen: 50.0 MB, 7.8 MB at `zstd -19`; glue 92 KB.
  - headless Chromium (WebGL2 on SwiftShader): the module instantiates,
    the welcome screen draws correctly, first frame ~0.5 s after
    navigation on localhost, 61 fps idle, renderer process 222 MiB.
  - **16 codegen units for the window crate** instead of one: 10m48s,
    peak **26.0 GiB**, `.wasm` 58.9 MB (8.2 MB at `zstd -19`). 5.6 GiB
    less memory and 2.5x faster for 18 % more bytes (5 % compressed) -
    the candidate for `release-web`; its byte reproducibility across two
    builds is checked in W6.
  - the canvas took the window's preferred 1060x720, not the viewport;
    W5 sizes it from CSS.
  - 2026-11-18, with the English string table set: the welcome screen
    renders with its labels in headless Chromium (100 % and 110 %
    zoom) and in desktop Firefox. **A CSS stretch on the canvas
    (`width:100vw; height:100vh`) breaks Firefox**: content about four
    times too large, blurred, no text - Chromium overrode the rule with
    Slint's inline size and was fine. Seen at about 110 % page zoom;
    whether zoom takes part is not separated. W5 must size the canvas
    the way Slint supports and check it in Firefox at a fractional zoom.
  - NOT yet measured: thin LTO, `opt-level` 3, populated models (chat
    rows, emoji, popup, context menu), fonts against the 990 px layout,
    `readText()` from a click, a real GPU, Firefox.

Cost: a web release is one more window-scale compile and needs ~26 GiB of
RAM + swap for the window rustc (16 codegen units; ~32 GiB with one). Run the spike under `systemd-run --user
--scope -p MemoryMax=..` so an overshoot kills only the compiler.

## 9. What the browser cannot do

Stated in the page's help and the README:

- no quit (close the tab);
- tabs and the native window show the same screen (one shared session);
- stored API keys cannot be shown, only replaced;
- paste needs the browser's paste gesture or a clipboard permission;
- no folder pickers; server directories are typed;
- no wiki export download (server path only);
- files over the web ceiling (64 MiB per file through the browser) go
  through the server-path flow; uploads need a workspace on disk;
- no screen reader support (Slint renders its own text);
- IME and mobile keyboards are weak; the layout needs 990 px width;
- `Ctrl+W` and `Tab` are the browser's: the wiki tab-close and
  edit/preview toggle get alternative bindings on the web;
- proportional text renders in Inter unless a proportional font is
  bundled; the 990 px minimum was tuned against DejaVu Sans Bold and is
  re-checked in W0/W5;
- right-click menus, colour emoji (Twemoji COLR under FemtoVG/WebGL) and
  popups are unverified until W0 loads the real window.

## 10. Stages

Each stage is test-first, reviewed, and lands green on master on its own.

- **W0 Measure and load.**
  (a) done: target, pinned CLI, minimal window end to end (§8.4).
  (b) done: the live-preview stubs type-check on wasm32 (§8.4).
  (c) done: the real window module compiles for wasm32 and loads in a
  browser (§8.4). The spike lives in `target/w0-spike` (ignored).
  (d) open: the same spike with hand-set strings and models (main screen,
  ~500 chat rows with colour emoji, a popup and a context menu reachable)
  for frame time, tab memory, fonts against the 990 px layout, context
  menu, emoji, popups and `readText()` from a click; thin LTO and
  `opt-level` 3 if the memory allows. Needs none of W1-W4.
- **W1 Wire and door.** Done (2026-11-18): frames, serializable
  `MoltError`, the hoisted `INTERNAL` list in core;
  `seat_token_matches`; `molt-remote` with the gate, auth, rotation
  close, Origin/Host, bounds, reply chunking, the static bundle list -
  proven against a real engine over loopback
  (`crates/molt-remote/tests/door.rs`); `[web]` config and the `moltd`
  wiring with the bundle-directory trust check
  (`crates/molt-app/tests/web_door.rs`); ADR-0008 and the
  `mcp-security.md` section; the client's sans-IO protocol core in
  `molt-ui-kit` (wasm-clean), tested against the real door. Also done
  (2026-11-18): binary upload chunk frames in `molt-remote` (server) and
  `molt-ui-kit` (protocol + client); found, join and chat between two
  engines with the founder driven through the door, and the run-event
  load measured (§11;
  `a_republic_is_founded_joined_and_chatted_in_through_the_web_door`,
  `crates/molt-ui/src/tests/gui/remote.rs`). Open: the web adapter of
  the client handle (W5). Red tests first: the gate test;
  foreign Origin, Origin with another port than Host, Origin `null`,
  Host `evil.example`, Host `127.0.0.1.evil.example`, tunnel port
  accepted; a silent subscribed session closed 4401 on rotation with no
  event after it; oversized hello; lag. Proof: found, join and chat
  against a real engine over the socket.
- **W2 Client and spawner seams** in `molt-ui`, native. Done
  (2026-11-18): the client seam - `molt-ui` holds an `Engine` (the
  engine's own handle or `molt-ui-kit`'s `RemoteHandle`), and the window
  mirrors through a real web door like a native one
  (`crates/molt-ui/src/tests/gui/remote.rs`); the spawner seam -
  `Ctx::spawn` over `native.rs`, no tokio in shared code (§6.2,
  `crates/molt-ui/src/tests/gui/spawner.rs`); the pure helpers in core,
  the wordlist straight from `bip39`, the startup inputs from the server
  (§6.6), and the native-only dependencies target-gated with the rest
  listed and pinned (§6.7); the link read - `classify_link`, a verdict
  Rust sets, a stale answer dropped, `joinable` on the seat rows (§6.4).
  Found, join and chat run through a real door since W4
  (2026-11-18), the settings save with its secrets since W2.
  Open: the rest of the secret draft (§6.5). Proof: the GUI
  suite, plus a new named test set - a real `molt-remote` on loopback and
  a window whose `Ctx` holds the remote client with the mirror running
  (found, join, chat, settings save with secrets intact).
- **W3 Platform seam.** Done (2026-11-18), native side: the trait in
  `molt-ui-kit` and `NativePlatform` behind it - clipboard with
  asynchronous paste (`paste-clipboard(PasteTarget)`), the alert, both
  clocks, sleep, the token, folder pick, and the files as names and
  bytes through `rfd`'s async dialogs; no OS call is left in shared code
  (§6.3, §6.7); the mirror folder as a typed draft with Apply and one
  property that hides every folder dialog button (§7); the
  `clippy.toml` lint in both crates, live-preview clippy at 0 with it
  on (§8.3). Open: the web implementation (W5).
- **W4 Byte doors.** Done (2026-11-18). Engine, door and client:
  the engine's upload store (`<workspace>/uploads/<id>/<name>`; 64 MiB
  per upload, 8 open, 1 GiB per workspace; idle 15 minutes; dropped on
  `remove_file`, delete, expiry, a failed share and close) and the
  seven byte-door commands - `upload_begin`, `upload_chunk`,
  `upload_abort`, `share_upload`, `read_file_bytes`, `read_image_bytes`,
  `read_export_bytes` - `INTERNAL` for MCP and on `WEB_DOOR`; `upload`
  as a picture-proposal source; upload chunk frames and session-owned
  uploads in `molt-remote`; the chunk frame and the `upload` helper in
  `molt-ui-kit`; red tests first for path traversal, quota, oversize,
  unknown and foreign id, out-of-order offsets and an abandoned upload
  (`crates/molt-engine/src/upload_store.rs`,
  `crates/molt-engine/src/tests/upload_store_tests.rs`,
  `crates/molt-remote/tests/byte_doors.rs`); spec in
  `docs_archive/security/mcp-security.md`, "The byte doors", and
  ADR-0008. The window (`molt-ui/src/bytes.rs`, §7 with the decision
  per door): share from bytes, the two-phase download, pictures shown
  through `read_image_bytes` on both targets (read behind the engine's
  storage writer), pictures set through an upload, the export as a
  download, the refusal strings; the gate test's pinned list is empty
  and gone. Proof: two real engines, each behind a real door
  (`crates/molt-ui/src/tests/gui/remote.rs`). Open: what §7 lists
  (uploads in an export, a republic that is not stored, snapshot
  weight) and the browser half of the platform calls (W5).
- **W5 `molt-web`**: the cdylib, the socket adapter, the web platform
  impl, `index.html` with the token prompt, key rebinding, font decision,
  the graph test; first full browser run.
- **W6 Pipeline**: profile, `build-release-web.sh`, reproducibility doc,
  service file, README, landing page link. This doc moves to
  `docs_archive/ui/` as the spec.

## 11. Risks

- **The wasm window compile may not fit this box, or the browser may not
  take the module** (W0). Levers: `opt-level = "s"` for the window crate,
  LTO off for the whole web profile, more swap; shrinking the module is
  the weak lever.
- **A second remote door**, and a tab holding the token is a shell on
  the host (§5.2). Mitigated by the shared live seat key, the
  fail-closed gate, loopback-only binding, Origin/Host checks, the
  trusted-bundle rule; it is the part that gets the security review.
- **The 90 ms run tick** no longer exists: the engine's simulated
  run progress went, and run-scoped session events are emitted on real
  ritual progress only. Measured 2026-11-18 over a real web door on
  loopback, a real 2-of-2 founding against an in-process relay, the
  founder's every command through the socket: the whole ritual pushed
  14 session events through the door in 0.4 s (4 run-scoped, 10 full;
  the busiest second held all of them), one `ReadSession` reply is
  2.9 KB and takes 1.4 ms round trip (debug build; the test prints the
  numbers and asserts none of them). No server-side
  coalescing was added; nothing here needs it. A full event still
  makes the mirror re-read the surfaces (about 15 commands); a flood of
  those is bounded by the per-connection queue and `lag` (§5.1).
- **Single-threaded browser**: image decode and diffs block the UI
  thread (W5 measures).
- **Drift**: std or tokio calls creeping back into shared `molt-ui` code.
  The clippy lint and the graph test are the guards (§8.3), not the wasm
  `cargo check`.
