# Web GUI: the Slint window in a browser (WASM + WebGL)

**Status: PLAN, not built (2026-11-17).** Decisions D1-D4 are the user's
(2026-11-17). The plan was reviewed against the code the same day (four
lenses, 29 findings, all folded in). Stage W0's compile and first browser load are
measured (§8.4, 2026-11-18); its populated-content half is open.

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
     (NOT pure, §6.4), the phrase wordlist, `random_token`, the wake skill
     text, the handle charset refusal, the wallet-keys-damaged string.
     `expand_tilde` is used only to seed the native folder dialog and
     stays native-only;
  2. a multi-thread tokio `Handle` in `run_app`, ~45 `rt.spawn` /
     7 `spawn_blocking` sites, 47 `invoke_from_event_loop`, plus
     `tokio::sync::{oneshot, Mutex}` and one `tokio::time::sleep`;
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
  helpers `molt-ui` needs, with today's owners re-exporting so one source
  stays: the wiki front-matter and link helpers (core gains `yaml-rust2`,
  default features off, pure Rust, no I/O), `HANDLE_CHARSET_REFUSAL`, the
  wallet-keys-damaged message as a const `StorageError`'s Display uses,
  `WAKE_SKILL` / `WAKE_SKILL_FILE` / `fill_wake_skill`, and the hoisted
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
- **`molt-ui`**: engine/storage/config/mcp/tokio/arboard become
  `cfg(not(target_arch = "wasm32"))` dependencies used only by the native
  client, spawner and platform impls; shared code uses no `tokio::` path.
  The wordlist lookup is two `bip39` calls made directly. Native
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
- **Bytes, both directions, always chunked**: bounded binary frames
  `le64 id ‖ le64 offset ‖ flags ‖ bytes` (flag 1 = last chunk), so neither
  side holds a whole file in one frame. The engine has no default file
  cap (`file_cap_bytes: None`), so the web door carries its own absolute
  per-transfer ceiling (64 MiB) and chunk bound (256 KiB), with
  `file_cap_bytes` on top when set; a read's caller-supplied `cap` is
  clamped to the ceiling. A file over it shows as `too large for the
  browser`; the server-path flow remains for those. A dropped socket
  deletes its partial upload.
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
  `read_upload_bytes`, `set_wake_command`, and the byte-door and
  link-read commands of §6.4/§7 where they are not MCP tools.

Everything else is refused before execution. `as_operator` is refused;
should a wrapper command ever be allowed, the gate must recurse into its
inner command.

A test in the style of the co-equality test pins it: tools and `WEB_DOOR`
are disjoint; every `Command::variant_names()` entry is either allowed or
on `INTERNAL` minus `WEB_DOOR`; every command `molt-ui` issues is allowed.
A new `Command` breaks the test until it is classified.

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

`Ctx::spawn(future: Send)` replaces `rt.spawn`: tokio on native,
`slint::spawn_local` on the web. Shared code uses no tokio: the two
oneshots in `mirror.rs` move to `futures-channel` (a new workspace line),
the kanban `DraftSaver` mutex becomes a std mutex or a runtime-neutral
async lock, the one `sleep` goes through the platform seam.
`spawn_blocking` sites become platform calls (file reads) or inline work
(image decode - W5 measures whether large images need chunking on the one
browser thread).

### 6.3 Platform

One trait for what the OS did: clipboard copy and paste, play the alert,
wall clock, monotonic clock, sleep, random token, folder pick (native
only; fills a typed draft). Native impl is today's code; the web impl
uses web-sys (`navigator.clipboard`, WebAudio, `web_time`,
`crypto.getRandomValues`). chrono gets `wasmbind` on wasm32.

- **File pick and save: take `rfd`** on both targets through
  `AsyncFileDialog` (name + bytes in, bytes out as a download on the
  web). The blocking `rfd::FileDialog` site in `actions/kanban.rs` moves
  to the async form. Per-target features: `xdg-portal` + `async-std`
  native, none on wasm32. No hand-written `<input type=file>`.
- **Clock inside the diff**: `patchview.rs` arms its deadline with
  `Instant::now()`; it moves to `similar`'s own
  `TextDiff::configure().timeout(..)`, and the wasm build enables
  `similar`'s `wasm32_web_time` feature so the timeout is real.
- **Paste becomes asynchronous on both targets** - a `.slint` interface
  change: `paste-clipboard(target)` with no return value, `target` an
  enum over the four fields (`rw-seed`, `rv-link`, `bk-restore-seed`,
  `decrypt-phrase`); Rust sets exactly that property when the text
  arrives. Two of the four are recovery phrases, so a headless GUI test
  pins that the text lands in the named field and no other. If a browser
  refuses `readText()` from a Slint click (W0/W5 check), the web hides
  the paste buttons and relies on Ctrl+V into the focused input.

### 6.4 Synchronous callbacks and link classification

`seed-matches`, `seed-rows`, `vt-*` and `folder-preview` (a string format
over `molt_core::slugify`) are pure and stay local.

**`classify-link` cannot stay local.** `FoundingInvite::parse` /
`RecoveryInvite::parse` end in molt-net's handover decoders, which
validate the npub through `canonical_nostr_pk` on the C `secp256k1`
(ADR-0002). Moving that to core would drag the C library into the wasm
build; a second, weaker parser is what CLAUDE.md forbids. So
classification becomes a stateless engine read (a new command returning
kind, republic, who and relays; an MCP tool or classified) on BOTH
targets:

- `rv-kind` becomes a property Rust sets; the `pure callback
  classify-link` goes away;
- Rust resets `rv-kind` to kind 0 synchronously when the link changes,
  before sending the read, and applies a reply only if the link it echoes
  is still current - the join/recover buttons arm on `rv-kind`, so a
  stale answer must never arm them;
- the seat rows gain an additive `#[serde(default)]` `joinable` flag in
  `SessionView` instead of the re-parse in the mirror;
- the existing tests (a damaged handover arms nothing) stay the pin.

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

`embedded_tor_available` comes from `welcome.caps`; the config dir and
file come from `SessionView.config_path` (already absolute engine-side;
there is no `current_dir` in a browser). Native keeps passing both.

## 7. Files as bytes

New byte doors are `Command`s, classified in the gate test, chunked and
capped per §5.1.

| Feature | Today | Web |
|---|---|---|
| Share a chat file | `ShareFile{path}` | chunked upload into an engine-owned per-workspace store; the share points at that file |
| Download a chat file | `DownloadFile{dest}` | two phases: the existing `DownloadFile` into the server's download dir with its `FileTransfer` progress, then a chunked read by id and a browser download |
| Avatar and logo display | `std::fs::read(path)` in the mirror | bytes by the key the view already carries |
| Set avatar, logo, wiki image | read a picked local path | upload bytes |
| Wiki inline images | `ReadUploadBytes` | unchanged, chunked |
| Workspace export, wake skill export | write to a path | bytes to a browser download; the server-path form stays |
| Wiki export | `WikiExport{dest}` writes a directory tree | typed server path only (`wiki_export_plan.md` decided "plain files, no archive"); a browser download is out of v1 |
| Mirror dir, workspace dir, download dir | folder picker | typed server path; browse buttons hidden |

- The upload store is durable state, not temp handling: it lives for the
  share's life, is deleted on `RemoveFile`/expiry, and has a stated quota.
- An offline sharer makes phase one of a download wait without deadline,
  as today; the web shows the same progress row.
- The mirror folder is today a read-only label plus a picker-only button;
  it gains a typed draft and apply on both targets (W3, a `.slint`
  change, existing `SetMirrorDir`). Whether the download dir has any
  window control at all is checked in the same pass.
- The native window may keep its path flow or move to the same doors; W4
  decides per door by what removes the most duplicated code. No door
  accepts a server path from the browser that MCP would not accept.

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
- files over the web ceiling go through the server-path flow;
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
  `molt-ui-kit` (wasm-clean), tested against the real door. Open: the
  client handle and its adapters (W2 native, W5 web); upload chunks (with
  W4); the 90 ms tick measurement; found/join/chat over the socket. Red tests first: the gate test;
  foreign Origin, Origin with another port than Host, Origin `null`,
  Host `evil.example`, Host `127.0.0.1.evil.example`, tunnel port
  accepted; a silent subscribed session closed 4401 on rotation with no
  event after it; oversized hello; lag. Proof: found, join and chat
  against a real engine over the socket. Measures the 90 ms run tick.
- **W2 Client and spawner seams** in `molt-ui`, native. Done
  (2026-11-18): the client seam - `molt-ui` holds an `Engine` (the
  engine's own handle or `molt-ui-kit`'s `RemoteHandle`), and the window
  mirrors through a real web door like a native one
  (`crates/molt-ui/src/tests/gui/remote.rs`). Open: everything below. Helpers move to
  core. The secret draft (§6.5) and link read (§6.4). Proof: the GUI
  suite, plus a new named test set - a real `molt-remote` on loopback and
  a window whose `Ctx` holds the remote client with the mirror running
  (found, join, chat, settings save with secrets intact).
- **W3 Platform seam**: clipboard with async paste, rfd async everywhere,
  alert, clocks, sleep, token; the mirror-folder draft; the clippy lint.
- **W4 Byte doors** on engine, gate classification and native GUI.
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
- **The 90 ms run tick** drives ~11 `ReadSession` replies per second over
  the socket during a ritual. Measured in W1; if heavy, the server
  coalesces run-scoped events per connection.
- **Single-threaded browser**: image decode and diffs block the UI
  thread (W5 measures).
- **Drift**: std or tokio calls creeping back into shared `molt-ui` code.
  The clippy lint and the graph test are the guards (§8.3), not the wasm
  `cargo check`.
