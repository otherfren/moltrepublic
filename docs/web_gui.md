# Web GUI: the Slint window in a browser (WASM + WebGL)

**Status: BUILT on branch `web-ui-slint`, first headless Chromium run
done, the browser checks of §12 mostly open (2026-11-18).** Stages W1-W6 are built and tested natively; the browser
side is type-checked for wasm32 against the stub window, and the real
bundle was built through the release script twice at one commit with the
same hash (§8.4). Only headless Chromium has loaded that bundle (§12 A.0),
the branch is not on master, and the native window build has not run with these changes.
**Everything still open is in §12, and only there**; a section's own
"Open" notes give the detail behind an entry.

Decisions D1-D4 are the user's (2026-11-17). The plan was reviewed
against the code the same day (four lenses, 29 findings, all folded in).
What each stage built (W0 (a)-(b) 2026-11-17, the rest 2026-11-18):

- W0 (a)-(c): the wasm32 compile and a first browser load of the bare
  window module, measured (§8.4). (d) is open.
- W1: the wire and the web door (§5), ADR-0008.
- W2: the client and spawner seams, the pure helpers in core, link
  classification as an engine read (§6.1, §6.2, §6.4, §6.6).
- W3: the platform seam with asynchronous paste, typed server paths, the
  `clippy.toml` lint (§6.3, §7, §8.3). What ties `molt-ui` to native is
  two cfg-gated modules (§6.7).
- W4: the upload store, chunked upload frames, reads by share id,
  picture key and last export, and the window on them (§5.1, §7). A
  republic was founded, joined and chatted in with the founder driven
  through the door; a file went up from bytes and came down byte for
  byte.
- W5: `molt-web` - socket adapter, browser platform, entry, the page
  with its key prompt and reconnect, the window sized from the viewport,
  the web form's keys (§4, §6.3, §6.5, §9).
- W6: profile, script with a check-only mode and its test, toolchain
  target, build doc, service file, README (§8.1, §8.2); the script's
  names, its bind step and its tar line are held to the crate and to the
  door's list by
  `the_release_script_packs_exactly_what_the_door_serves`
  (`crates/molt-remote/tests/door.rs`).
- After W6, from a real node on a slow link (2026-11-18): stall
  timeouts in place of totals, the bundle served compressed, large
  frames packed (§5.6). Not yet run in a browser.

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
- **`molt-web`** (built 2026-11-18, `cdylib` + `rlib`): every dependency
  sits under the wasm32 target table, so a native workspace build sees a
  crate with no dependencies - a plain member, not kept out by
  `default-members`: resolver 2 ignores another target's dependencies,
  so no native feature set or unit hash moves, and a test pins that its
  native graph is itself alone. Its wasm32 modules: the entry
  (`start.rs`), the socket adapter over the kit's pump (`socket.rs`) and
  the browser platform (`platform.rs`). One module compiles everywhere
  and is tested natively, `rules.rs`: what the page decides without a
  browser (why a connection ended, the socket URL, the start language,
  how a picked file is named, the size check, the key's shape). The
  `rlib` is there so those tests and the graph test build.
  - **Entry**: `start(token, notify)`, a `#[wasm_bindgen]` export the
    page calls once it holds the key - not `#[wasm_bindgen(start)]`,
    because the key comes from the page's prompt (§5.3). It installs the
    platform and a panic hook (the panic goes to the console), dials
    `ws(s)://<the page's host>/ws`, and runs the window
    (`molt_ui::run_remote`), which does not return: winit leaves by
    throwing, and the page must expect that. `notify` hears one word:
    `ready` on every welcome, `lag` when the server dropped events (a
    passing note), and when a connection ended for good one of `version`
    (the server's build is not the
    bundle's - decided inside the pump at the welcome,
    `Pump::only_build`, so no queued command crosses; pinned by
    `another_build_ends_the_connection_before_a_command_crosses`), `refused` (close
    4401), `protocol` (close 4400 or a frame the client cannot read;
    both codes are `molt_core::wire` constants the door closes with,
    and a test holds the page rules' copies to them),
    `closed`. A second call is refused.
  - **Reconnect inside one page load** (built 2026-11-18): the pump
    outlives its connection (`Pump::serve` in `molt-ui-kit`). After
    `refused`, `protocol` or `closed` the page asks for the key again
    and calls `reconnect(token)`; the same window goes on, and the
    first welcome of a later connection reads as lag, so the mirror
    re-reads everything. A command open or queued when a connection
    ends is answered with an error, never kept for the next one. Only
    `version` needs a reload: the page reloads once by itself (a fixed
    `#reloaded` marker in the address is the "once") and shows the
    mismatch if it stands.
  - **Sizing** (built 2026-11-18, not yet seen in a browser): the page
    sets no size on the canvas. `run_remote` hands the entry a `Sizer`;
    `start` sizes the window to `innerWidth` x `innerHeight` before the
    loop starts and on every `resize`, through Slint's own
    `Window::set_size` - which is what writes the canvas's inline size
    (winit's `request_inner_size`). The window's 990 px minimum still
    holds, so a narrower viewport scrolls.
  - **Socket**: `gloo-net`'s `WebSocket` (the rustwasm wrapper: a
    futures `Stream` + `Sink` over the browser socket, `websocket`
    feature only) instead of hand-written event closures; the adapter
    maps its messages onto the pump's `Wire` and keeps the close code.
    New in the lockfile: `gloo-net`, `gloo-utils` (wasm32 only).
  - **The page** (built 2026-11-18, `crates/molt-web/page/`):
    `index.html`, `page.js`, `page.css` - three files because the
    door's content policy admits nothing inline. The key prompt (a
    password field, cleared the moment it is read; no storage, cookie
    or address ever holds the key), the reason a connection ended, the
    prompt again to reconnect, a passing line on `lag`, and the limits
    of §9 as a fold-out. Its labels come from `molt-ui`'s `i18n.rs`
    (`page_text`, English and German by the browser's language) through
    the `page_text` export, so the page carries no text of its own. It
    also styles `rfd`'s file overlay, whose own inline style sheet the
    policy drops (§6.3). `crates/molt-web/tests/page.rs` reads the three
    files as text and holds those rules; the door's list is held to the
    directory by `the_door_serves_exactly_what_the_page_ships`
    (`crates/molt-remote/tests/door.rs`). No browser has loaded it.

Workspace rules for the new members:

- every wasm32 invocation that compiles the real window names
  `-p molt-web` - never `-p molt-ui` or `-p molt-ui-window` alone (web-sys
  feature unification would rebuild the window module, the trap CLAUDE.md
  records for the solo native window build);
- no crate adds a slint feature on any target;
- `wasm-bindgen = "=0.2.126"` is pinned in `[workspace.dependencies]`
  (done 2026-11-18), so a Slint bump that moves it fails loudly and the
  CLI pin moves with it; a test holds the pin exact and the lockfile at
  that one version;
- a graph test modelled on `ring_free_guard.rs`
  (`crates/molt-web/tests/graph.rs`, built 2026-11-18): the package set
  of `cargo tree -p molt-web --target wasm32-unknown-unknown -e no-dev`
  holds none of `molt-engine`, `molt-net`, `molt-storage`, `molt-mcp`,
  `molt-remote`, `molt-config`, `molt-vault`, `molt-treasury`,
  `molt-app`, `tokio`, `arboard`, `secp256k1`, `secp256k1-sys`,
  `libsqlite3-sys`, `ring` - and does hold the window crates, so a
  listing that went blind fails too. It reads the whole forward graph
  instead of `-i <pkg>`: an inverse query for a name with two versions
  in the lockfile errors with empty output and would pass blind.

The engine actor is unchanged. `molt-remote` holds a `WalletHandle` like
MCP does and never touches engine state directly.

## 5. The wire

### 5.1 Frames

One WebSocket, JSON text frames for control, binary frames for bulk bytes.

```
client -> server   {"hello": {"v": 1, "token": "..."}}                first frame only
                   {"hello": {"v": 1, "token": "...", "packed": true}}  a client that unpacks (§5.6)
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
- A server frame of 1 KiB or more goes to a client that said `packed`
  as binary chunk frames holding its deflated text (§5.6); to any other
  client every frame is the text frame shown here.
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

The window's quit is not a `Command`, and a tab cannot reach it: the
quit dialog opens only on the window manager's close request, which a
browser never raises (2026-11-18; no control to hide).

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
- Static files: a fixed file list (`index.html`, `page.js`, `page.css`,
  `molt_web.js`, `molt_web_bg.wasm`; `molt_remote::BUNDLE_FILES`)
  matched on the raw request target, no directory walking; the bytes are read ONCE at start (§5.4), so nothing
  on disk is followed per request. A `Sec-Fetch-Site: cross-site` request
  is refused and files carry `Cross-Origin-Resource-Policy: same-origin`,
  so another site cannot probe for a running node. A file goes out in
  the smallest content coding the request offers (gzip or brotli, made
  once at start) with `Vary: Accept-Encoding`; a connection that takes
  nothing of its answer for 30 s is dropped, one that reads slowly is
  not (§5.6). Headers: CSP `default-src 'self'; script-src
  'self' 'wasm-unsafe-eval'; frame-ancestors 'none'`,
  `X-Content-Type-Options: nosniff`. The page is `Cache-Control:
  no-store`; its files are `no-cache` with an `ETag` (§5.6).
- The page keeps the token in memory only and asks on every load.
- The `ring_free_guard` must stay green.

### 5.4 The bundle is trusted like the binary

The seat token is typed into whatever `[web] dir` serves. `moltd` refuses
to open the web listener (one log line, the node stays up) if the
directory or a listed file is a symlink, group- or world-writable, or
owned by anyone but `moltd`'s user or root (built: `load_bundle`). The default `[web] dir` is `web/` beside the
config file, where the web tarball is unpacked - never a shared location. The
directory is made with `mkdir -m 755 web` (§8.2), not a bare `mkdir`.

The door's rules are in `docs_archive/security/mcp-security.md` ("The web
door") and its decision in
`docs_archive/adr/0008-web-door-second-remote-seat-door.md`.

### 5.5 `UiPublish`

The engine has one snapshot slot, read by `read_ui_state` (test
instrumentation for `gui_walk.py`). The browser does not publish: a
window on a remote engine skips the publish at its source
(`mirror.rs::publish_snapshot`, pinned by test), so the door's refusal of
`ui_publish` is never exercised by its own page. Before 2026-11-18 the
remote window sent it after every mirror pass and the node logged one
refusal per pass. Driving the web GUI over MCP would be its own change.

### 5.6 A slow link (2026-11-18)

Measured on a real node behind an SSH tunnel over a lossy path (about
35 KB/s server to browser, RTT 150 ms): the 62 MB module was cut off by
the 60 s total on an HTTP exchange, a 16.8 MB `read_state` reply could
never pass the 30 s total on one frame, and nothing was compressed. The
window never filled. Three changes, none of which moves a door rule (the
bind, Origin/Host, the gate and the bundle trust are untouched).

**A. A timeout measures a stall, never a total.** The accepted TCP
socket is wrapped once in `tokio_io_timeout::TimeoutWriter`
(`WebDoor::stall`, 30 s in `moltd`): its timer starts when a write finds
the send buffer full and restarts when the kernel takes more. hyper's
upgrade hands the same socket on, so one wrapper covers a static file's
body and every WebSocket frame. A client that keeps reading is not cut
off; one that takes nothing for `stall` loses its connection, its
session slot and whatever was buffered for it.

The timer's grain is the send buffer, not a byte: Linux reports a full
socket writable again only once about a third of its buffer is free,
and a kernel-tuned buffer on loopback is megabytes (`tcp_wmem` max
4 MiB here), a third of which the 35 KB/s tunnel does not take in
30 s. So every accepted socket gets `SO_SNDBUF` = 32 KiB
(`server.rs::SEND_BUFFER`, set through `socket2`, tokio's own socket
layer; a socket that refuses it is not served). Loopback has no round
trip a large buffer would cover. Measured with the 500 ms test timeout
against a reader with a 16 KiB receive buffer: 50 KiB per stall is
served, 25 KiB is not, at 8, 16 and 32 KiB alike (a 64 KiB buffer fails
at 50 and passes at 150; untuned, 150 KiB per stall is cut after
2.4 of 6 MB). At 30 s that is a floor between 0.8 and 1.7 KB/s; the
tunnel takes about 1 MB per stall. Below the floor a steady reader is
dropped like a stopped one. The buffer also caps what an unread
connection pins in the kernel (256 slots at 64 KiB accounted, where
autotuning allowed 4 MiB each). The two
totals (`HTTP_EXCHANGE`, `WRITE_TIMEOUT`) are gone. What they also
bounded and what bounds it now: a request that never arrives and an idle
keep-alive connection end at hyper's header read timeout (pinned by
test); the close frame at a session's end keeps a total of `stall`.
Accepted: a local process that reads just above that floor holds a
connection slot for as long as it likes (256 slots; a page of another
site gets a 403 without a body and cannot).

- Crate: `tokio-io-timeout` 1.2 (pin-project-lite and tokio only; its
  write timer is exactly this one, read and checked in its source). The
  alternative was a hand-written `AsyncWrite` wrapper with the same
  timer; nothing already in the graph offered it. Checked by mutation:
  with the timer off, both stopped-reader tests fail.
- A large reply still crosses as bounded frames only when it is packed
  (C). Unpacked it stays the one text frame it was: a client that did
  not ask for packed frames can reassemble nothing else, and the stall
  timer makes one large frame deliverable.
- While one reply is being written the session reads nothing and
  forwards nothing; on a slow link the event queue overflows meanwhile
  and the client is told `lag` (§12 C).

**B. The bundle is served compressed.** Each file is encoded ONCE at
start, beside its identity bytes, and a request's `Accept-Encoding`
picks: the smallest coding it offers (for the module that is brotli;
for a short text it can be gzip), else identity. Every bundle answer
carries `Vary: Accept-Encoding`; `Content-Length` is the served body's.
A coding that is not smaller than the file is not kept (so not served).
`q=0` refuses a coding; the weights do not rank; `*` is not read as an
offer. The header is split by hand (comma list, optional `;q=`): no
crate in the graph parses it, and `tower-http`, the usual answer,
compresses per request or serves precompressed files from a directory
it walks, against the read-once rule. A misread costs a larger
download, never a door rule.

Measured on the release module (65,176,616 bytes, this box, one thread,
release build of the two encoders):

| coding | bytes | encode | extra RSS | at 35 KB/s |
|---|---|---|---|---|
| identity | 65,176,616 | - | - | 31 min |
| gzip 1 | 15,143,058 | 0.16 s | 15 MB | 7.2 min |
| gzip 6 | 12,804,091 | 1.26 s | 14 MB | 6.1 min |
| gzip 9 | 12,772,449 | 2.24 s | 13 MB | 6.1 min |
| brotli 4, window 22 | 11,723,523 | 0.39 s | 49 MB | 5.6 min |
| brotli 5, window 22 | 10,390,141 | 0.82 s | 54 MB | 4.9 min |
| brotli 9, window 22 | 10,053,098 | 2.84 s | 87 MB | 4.8 min |
| brotli 10, window 22 | 9,182,243 | 30.2 s | 131 MB | 4.4 min |
| brotli 11, window 22 | 8,760,849 | 73.3 s | 126 MB | 4.2 min |
| brotli 11, window 24 | 8,643,264 | 78.3 s | 258 MB | 4.1 min |
| zstd -19 (C, reference) | 9,458,996 | 6.9 s | - | 4.5 min |

Decided: **both, gzip 6 and brotli 5 (window 22).** Brotli 5 is smaller
AND faster than gzip 6 here, but browsers offer `br` only where the
transport is opaque: Chromium on https and on localhost, Firefox on
https (from their documented behaviour, not checked against the door
yet: §12 A.1). gzip is what every browser offers on plain http, so it
is the floor and brotli the gain where offered. Above quality 5 the curve is
flat until 10, which costs half a minute of start on this box (more on
the 4-core server) for 12 %. zstd is left: the pure-Rust encoder
(`ruzstd`) has only its fast levels, and the C library is against the
pure-Rust posture.

- Crates: `flate2` on `miniz_oxide` (pure Rust, both already in the
  graph through `png`) and `brotli` 9 (pure Rust, the encoder Dropbox
  maintains; native only, not in the wasm graph).
- Cost at start: the bundle is read and encoded on a blocking thread
  before the door serves, gzip and brotli of a file side by side. From
  the encoders' own times above that is about 1.3 s wall on this box;
  `moltd` itself was not timed in a release build. An unoptimized build
  takes 5.9 s and 9.6 s for the same two. The node's start does not
  wait: the door is its own task. Held for the process's life: 65.2 +
  12.8 + 10.4 = 88 MB instead of 65 MB.
- Kept by the browser, asked for every time (2026-11-18): the page's
  files carry `Cache-Control: no-cache` and an `ETag` (a hash of the
  file, one per coding); a matching `If-None-Match` is answered 304
  without a body. `index.html` stays `no-store`. The files hold no key
  and no republic data, and a kept one is revalidated before each use.

**C. Large frames cross packed.** tokio-tungstenite 0.26.2 (the version
in `Cargo.lock`) has no permessage-deflate: tungstenite treats a set
RSV1 bit as a protocol error and negotiates no extension. So it is done
one layer up, in the frames the client already reassembles.

- Negotiated in the hello: `{"hello": {"v": 1, "token": "...",
  "packed": true}}`. The field is additive (`#[serde(default)]`, left
  out when false, so the legacy hello keeps its bytes) and
  `WIRE_VERSION` stays 1. **Old client, new server:** no `packed`, the
  server never packs; every frame is what it was. **New client, old
  server:** the old hello type ignores the unknown field (pinned by a
  test against the old shape), the client asked for something that
  never comes and reads text frames as before. Neither pairing needs
  the `version` path; that one still ends a different build at its
  welcome (§5.1).
- A server frame (reply, event, `lag`) whose JSON is at least
  `PACK_MIN` (1 KiB) and at most `MAX_UNPACKED` (64 MiB) is deflated
  (raw deflate, level 3) and, if that is smaller, sent as binary chunk
  frames with flag bit 2 set: `le64 seq ‖ le64 offset ‖ flags ‖ bytes`,
  256 KiB a chunk, `seq` the connection's count of packed frames. All
  chunks of one packed frame are sent back to back. From the second
  chunk of any transfer on (packed frame or byte read) the key is
  re-checked before the chunk is written: a reply that takes minutes
  to cross stops with close 4401 when its key is rotated. One unpacked
  text frame cannot be stopped midway; it is finished or stalls out. Anything else is
  the text frame it always was. Commands are not packed (they are
  small; an upload's bytes are binary already).
- The client (`molt-ui-kit`'s `Protocol::on_binary`) collects a packed
  frame through the same chunk checks as a byte read (in order, at most
  64 MiB on the wire), inflates it and reads the result as the text
  frame it stands for. A strict `Chunk::decode` (the server's, and an
  old client's) refuses flag bit 2.
- **The bomb bound:** inflating stops at `MAX_UNPACKED` = 64 MiB of
  text; one byte more ends the connection as a broken protocol
  (`packed frame over the ceiling`), pinned by test with 65 MiB of
  zeros, under 128 KiB on the wire. Deflate tops out near 1000:1, so without it
  64 MiB of wire could ask for 64 GiB. The largest reply measured is
  16.8 MB. A reply over the bound is sent unpacked, as before.
- Crate: `flate2` again, decoder side in `molt-ui-kit`: `miniz_oxide`
  is already in the wasm graph (through `png`), so the bundle grows by
  no new crate. Brotli on the socket was left: on JSON it is no better
  (below), its encoder wants 30 to 50 MB a reply, and its decoder would
  be new in the wasm graph.
- **No shared context, by design.** Each packed frame is its own
  deflate stream (`pack::deflate` builds a fresh encoder; pinned by
  `session.rs::every_packed_frame_is_its_own_stream`), so a frame's
  length says nothing about another frame's content. Keep it so: one
  stream per connection would turn a member's chat text into a length
  probe (CRIME/BREACH) against whatever shares the stream, for anyone
  reading sizes on a tunnel. For the same reason a reply carrying a
  local secret must not share a frame with peer-written content.
  Today none does: the token only travels in the hello, which is never
  packed, the stored secrets are write-only, and the bundle's codings
  cover public files.

Measured on synthetic chat-surface JSON (invented words, seven invented
names, ids and timestamps as the real shape has them), this box, one
thread:

| coding | 6.0 MB reply | encode | 16.8 MB reply | encode | inflate |
|---|---|---|---|---|---|
| deflate 1 | 1.89 MB (3.17) | 0.02 s | 5.30 MB | 0.05 s | 0.03 s |
| deflate 3 | 1.45 MB (4.13) | 0.06 s | 4.07 MB | 0.16 s | 0.03 s |
| deflate 6 | 1.33 MB (4.51) | 0.20 s | 3.73 MB | 0.52 s | 0.02 s |
| deflate 9 | 1.32 MB (4.54) | 0.26 s | 3.70 MB | 0.69 s | 0.02 s |
| brotli 4 | 1.39 MB (4.31) | 0.06 s | 3.98 MB | 0.16 s | 0.02 s |
| brotli 5 | 1.37 MB (4.39) | 0.12 s | 3.84 MB | 0.35 s | 0.02 s |

Level 3: the door cannot tell a tunnel from a local browser, and a
local one pays the CPU for nothing; level 6 costs three times the CPU
for 9 % fewer bytes. At 35 KB/s the 16.8 MB reply takes about 2 min
packed instead of 8, by these ratios; the real replies were not
re-measured (6 MB took 41 s through `ssh -C`, which is zlib 6).
Encoding runs on a blocking thread from 64 KiB up.

Tests: `crates/molt-remote/tests/slow_link.rs` (a real engine, real
loopback sockets, a 500 ms stall timeout: a throttled reader gets a
12 MB reply and a 12 MB file whole, one at 150 KiB per stall a 6 MB
file, a stopped one is dropped, an idle
HTTP connection is closed, each coding, old and new hello, a rotation
mid-reply), `molt-ui-kit` (`protocol.rs`, `client.rs`: packed frames
through the shipped client, the bomb bound), `molt-core` (`wire.rs`:
the hello both ways against the old frame type).

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

**Built (2026-11-18), browser side.** `molt-ui/src/web.rs` is the twin:
`Spawner` is a unit type over `slint::spawn_local` (the join handle is
dropped, which detaches), `off_thread` runs the work inline and always
answers. `native.rs` and the `Native` arm of `Engine` are
`cfg(not(target_arch = "wasm32"))`; shared code takes `Spawner` and
`off_thread` from `crate::platform`, which re-exports this target's.
The tests compile `web.rs` natively; its `spawn` is only type-checked
there (the headless backend has no event loop) and has not run in a
browser. Image fit and decode therefore block the one browser thread;
whether large pictures need chunking is unmeasured. The direct `std::fs` / `std::thread` lines
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

**Built (2026-11-18), browser side** - `molt-web/src/platform.rs`,
type-checked for wasm32, NOT yet run in a browser:

- every call runs its JS future on the page's thread and answers
  through a channel, so the trait's futures stay `Send`; the audio
  context and the picked files are thread-locals;
- clipboard: `navigator.clipboard.writeText` / `readText`, the read
  started inside the click; a refusal reads as "nothing to paste", and
  `copy` answers whether the write got there (the key dialog of §6.5
  waits for it; every other Copy button drops the answer);
- alert: the shared WAV decoded from memory through WebAudio
  (`decodeAudioData`), so no `blob:` URL the page's CSP would have to
  allow; no sound is silent, not an error;
- clocks: `web_time`; sleep: `gloo-timers`; the key: 24 bytes from
  `crypto.getRandomValues` (getrandom's `js` feature) as hex, the shape
  `moltd` mints (pinned against `molt_config::random_token`);
- files: `rfd`'s wasm backend. A pick is kept by number (eight at most)
  and named `<n>/<name>`; `read_file` takes it once, MEASURES it first
  and answers `molt_ui_kit::platform::too_large()` over 64 MiB - which
  the shared code shows as `too large for the browser`, not as an
  unreadable file, for a share and for both picture reads (one helper,
  `bytes::read_picked`; `a_file_the_platform_refuses_unread_is_too_large`),
  and a file that does not read is named without its pick number -
  then reads through `Blob.arrayBuffer()`. `save_file` is rfd's download
  overlay: a link the user clicks, so it holds when the bytes arrive
  long after the click that asked. `pick_folder` and `pick_save` are
  `None`. `rfd` needs its `xdg-portal` and `async-std` features on
  wasm32 too (its build script demands a backend there; both are inert);
- chrono has `wasmbind` and `similar` has `wasm32_web_time` on wasm32
  (`molt-ui`'s wasm32 dependency table).

Open:

- the first browser run. To check there: `readText()` from a Slint
  click (else the web hides the paste buttons and relies on Ctrl+V);
  the file input's `click()` from a spawned task; **rfd's overlay
  against the door's CSP** - it styles itself with an inline `<style>`,
  which `default-src 'self'` blocks, so the overlay would show unstyled
  until the CSP carries that style's hash or the page styles
  `#rfd-overlay` itself; a cancelled rfd dialog never answers (its
  promise is dropped unresolved), so the task behind it stays parked;
- built (2026-11-18, §7): behind the web door the chat share, the
  download and the export hand bytes to and from the platform, so a
  `pick_file` answer need not be a server path and `pick_save` may be
  `None`. The browser's `read_file` refuses a file over the 64 MiB
  ceiling before it reads it (above). `save_file` is called outside a
  click - phase two of a download arrives on an engine event, an export
  after polling - which a browser must be checked to allow (§12).

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
  OPEN: nothing re-sends the read when a door connection returns. The
  client reconnects since 2026-11-18 (§4) and the mirror re-reads, but
  the link field's verdict is not part of that pass, so the user edits
  the field.
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
  real web door.
- The web door refuses an empty `mcp_token`, so a client bug cannot close
  its own listener (`the_door_cannot_empty_its_own_seat_key`,
  `crates/molt-remote/tests/door.rs`).

**Built (2026-11-18): the web form.** One window property, `web`, set
for every window behind the door (`open_remote` before the first frame,
`apply_remote_startup` with the first session); a native window never
has it and behaves as before.

- **No stored key is shown.** The two wells, the peek and the copy
  buttons are gone; a line says so (`set_keys_hidden`). The read key's
  button reads `Issue or rotate` and Revoke is always live: the form
  cannot know whether one is stored.
- **Clearing is an action.** An empty field equals what the form was
  shown, so it cannot mean "remove". Revoke writes
  `patch_settings {mcp_read_token: ""}` by itself; the S3 tab gets
  `Remove stored secret`, which writes `{s3_secret_key: ""}`. Each
  touches that one value.
- **Rotate shows the key first.** The key is minted through the platform
  seam and goes on show in a dialog (`key-new`); nothing is written.
  Cancel, Escape and the scrim discard the key. Apply takes it off the
  window and writes `patch_settings` with that one key. The native
  window keeps its one step (mint, into the draft, save).
- **Apply waits for a copy that worked** (review fix, 2026-11-18).
  `Platform::copy` answers whether the text reached the clipboard; the
  Copy button (`copy-new-key`) arms Apply only on `true`
  (`land_key_copy`, for the key still on show). A refused clipboard
  says so (`key_copy_failed`) and leaves Apply dead. The key is drawn
  by a read-only input (`KeyWell`), so it can be selected by hand -
  Slint's web backend serves the browser's own `copy` event from the
  focused input's selection, which needs no clipboard permission - and
  the whole key selected arms Apply too. Neither gate proves the key
  was kept; both are a deliberate step.
- **The connection follows the seat key.** The write goes out as
  `RemoteHandle::rotate` (`molt-ui-kit`): it notes the new key, and when
  the server closes the session (§5.3) `Pump::serve` dials again with
  it. A refused write forgets the key.
- **The next welcome is the answer** (review fix, 2026-11-18). The door
  never answers the write that kills its own session's key: it checks
  the session's key before every reply and closes with 4401 (§5.3). So
  `rotate` resolves from the reconnect: the new key welcomed is
  `set_key_applied`; the new key refused and the old one welcomed is
  `set_key_kept` (the write never landed); neither welcomed, or no
  connection to be had, is `set_key_unsure` (`NotRotated`,
  `land_new_key`). Other tabs end as `refused` and ask.

Pins: `crates/molt-ui-kit/src/client.rs`
(`a_rotated_key_moves_the_connection_onto_the_new_key`,
`an_answered_rotation_is_applied_and_followed`,
`a_refused_rotation_keeps_the_old_key`,
`a_rotation_the_server_never_took_falls_back_to_the_old_key`,
`a_rotation_no_key_is_welcomed_after_stays_open`,
`a_rotation_with_no_way_back_stays_open`,
`a_rotation_is_answered_when_its_pump_is_gone`,
`a_pump_serves_again_after_its_connection_ended`; the fake server hangs
up without answering the write, as the door does) and, through a real
door, `crates/molt-ui/src/tests/gui/remote.rs`
(`a_rotation_behind_the_web_door_shows_the_key_first_and_reconnects_with_it`,
`the_read_key_is_issued_behind_the_web_door_without_touching_the_seat_key`,
`the_web_form_revokes_the_read_key_and_removes_the_s3_secret`,
`a_native_window_rotates_in_one_step`,
`the_web_form_states_that_stored_keys_are_not_shown`,
`a_rotation_through_the_web_door_answers_applied`,
`a_rotation_s_outcome_is_told_as_it_ended`,
`the_new_key_dialog_arms_apply_only_on_a_copy_that_worked`,
`a_copy_answer_arms_only_the_key_it_copied`). NOT covered: the
`invoke_from_event_loop` hop between an answer and its landing
(`land_new_key`, `land_key_copy` are called directly), and the real
clipboard route (a test must not touch the user's clipboard).

Open:

- to check in the first browser run: that `writeText` from a canvas
  click is allowed, and that Ctrl+C on the selected key reaches the
  browser's `copy` event.
- with the key input focused, Enter does not press Apply (the input
  holds the keys); Apply is clicked.
- the form still cannot say whether a read key or an S3 secret is
  stored; a flag in the session view would be its own change.

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

**Built (2026-11-18): the web entry.** `molt_ui::run_remote(engine,
spawner, lang, opened)` is `run_app`'s twin and takes neither argument: the
config location and the embedded row arrive with the first session, as
above. It labels the window in `lang` at once (the browser's language,
until the session names the node's), clears `folder-browse` and sets
`web` (§6.5) before the first frame, and hands `opened` the window's
`Sizer` (§4); `run_app` itself is native only. Both share
`wire_window`. Pinned by
`the_web_entry_opens_a_window_on_the_remote_engine`
(`tests/gui/remote.rs`: labelled, no folder dialog, a callback reaches
the engine behind a real door). Until the first session lands the
footer is empty and the embedded row shows the window's default
(greyed).

### 6.7 What ties `molt-ui` to native (2026-11-18)

Nothing a browser build compiles. Two places name what a browser cannot
have, both `cfg(not(target_arch = "wasm32"))`: `native.rs` (the tokio
spawner, off-thread work, `NativePlatform`, `absolute`) and the `Native`
arm of `engine.rs` with `run_app`. `web.rs` is the browser's twin of the
first (§6.2). Pinned by
`the_higher_crate_uses_outside_native_are_the_listed_ones` and
`the_os_sites_outside_native_are_the_listed_ones`
(`tests/gui/spawner.rs`) and by the `clippy.toml` lint (§8.3), which
allows the listed calls in `native.rs` alone.

Not in `molt-ui` at all: `molt-net` (dev-dependency), `molt-mcp`
(dev-dependency since 2026-11-18), no audio crate (the alert runs a
system player natively, WebAudio in `molt-web`).

Measured 2026-11-18: `SLINT_LIVE_PREVIEW=1 cargo check -p molt-web
--features live-preview --target wasm32-unknown-unknown` passes with no
error and no warning, and so does clippy in the same form (18 errors
before the cfg, 22 before the platform seam, 27 before §6.4). That is a
type check against the stub window; what compiles and fails at run time
is the lint's and the graph test's business, and a real run is W5's
open half.

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
# strip: "none" is inherited; the script strips the bundle's copy with
#        wasm-bindgen --remove-name-section --remove-producers-section
#        (decided 2026-11-18: the pinned CLI does it, the unstripped
#        module stays in the target dir)
[profile.release-web.package.molt-ui-window]
opt-level = "s"               # measured, §8.4
codegen-units = 16            # measured: 26 GiB instead of 32, 11 min instead of 27
```

Cargo cannot set `lto` per package, so "no LTO for the window crate" is
not a lever. Without the name section browser traces show function
indices only; an unstripped `.wasm` is kept locally, as the native build
keeps its debug info. Artefact:
`target/wasm32-unknown-unknown/release-web/molt_web.wasm`.

Built 2026-11-18 in the root `Cargo.toml`. The §8.4 figures were taken
with `strip = true`; with the wasm-bindgen strip the
module is 65.2 MB (§8.4, measured 2026-11-18).

### 8.2 Script

Built (2026-11-18). `scripts/build-release-web.sh`, its own entry point:
the native script is the third-party verification recipe and must not
start needing wasm tools or a second window-scale compile. The releaser
runs native, then web.

`--check-only` validates rustc against `rust-toolchain.toml`, the wasm32
target, the CLI version, GNU tar, the profile, the page and licences and
reports memory and a busy target dir, without compiling. The target dir
is remapped as well, so a `CARGO_TARGET_DIR` outside the tree gives the
same bytes; the registry and git remaps follow the effective cargo home
(`${CARGO_HOME:-$HOME/.cargo}`), not a fixed `$HOME/.cargo`. The bundle
is staged in `<target>/wasm32-unknown-unknown/release-web/web-bundle`.
`scripts/test-build-release-web.sh` tests the script against stub tools.
The busy check covers this target dir only (cargo, rustc, rustdoc,
clippy-driver by command line, cwd or `CARGO_TARGET_DIR`); a
window-scale build on another target dir is not seen. Low memory is a
warning, not a refusal.

Install, beside the config file:
`mkdir -m 755 web && tar --zstd -xf moltrepublic-web.tar.zst -C web` -
never a bare `mkdir web`: the door refuses a group- or world-writable
bundle directory (§5.4) and a umask of 002 would produce one.

- `rust-toolchain.toml` gains `targets = ["wasm32-unknown-unknown"]`.
- Same `SOURCE_DATE_EPOCH`, path remaps, `LC_ALL`, `TZ`, `--locked`.
- Refuses to start while a cargo/rustc is live on the target dir (two
  scripts lose the serial order one script gave; this is the documented
  OOM class).
- `cargo build -j 1 -p molt-web --target wasm32-unknown-unknown --profile
  release-web`, then `wasm-bindgen --target web --no-typescript`, the CLI
  version checked against `Cargo.lock` (mismatch fails and prints the
  `cargo install --locked wasm-bindgen-cli --version =X` line).
- Deterministic tar of the page (`index.html`, `page.js`, `page.css`
  from `crates/molt-web/page/`), the glue `.js`, the `.wasm`, `COPYING`
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
  cache dir (`target/dev-ui-wasm`) is a `dev-ui.sh` mode (built
  2026-11-18: `scripts/dev-ui.sh wasm`; about 20 s after an edit, 2 s
  warm) and the per-change compile of all wasm-cfg code. `molt-web`
  carries a dev-only `live-preview` passthrough like `molt-app`'s; it is
  check-only, the stubs cannot run in a browser. The same invocation
  with `clippy` in place of `check` is the only lint the wasm-cfg code
  gets.

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
    builds held on this box (2026-11-18, below); a second machine or
    checkout path is still open.
  - **the real crate through the script** (2026-11-18,
    `scripts/build-release-web.sh`, `molt-web` in `release-web`, `-j 1`,
    `wasm-bindgen-cli 0.2.126`): 17m40s cold, 16m24s with the host side
    cached; the window rustc peaked at **26.1 GiB** (RSS + swap) both
    times. Bound `.wasm` 65.2 MB (9.5 MB at `zstd -19`), unstripped
    105.7 MB, glue 111 KB, tarball 9.5 MB. Two builds of one commit,
    the wasm profile dir deleted in between: the same SHA-256 for the
    tarball and for the unstripped module. No host path in the module.
    The glue exports what `page.js` imports. Not loaded in a browser.
  - the canvas took the window's preferred 1060x720, not the viewport;
    W5 sizes it from the window, not from CSS (§4, built 2026-11-18,
    still to be seen in a browser).
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

Stated in the page's help (built 2026-11-18, `page_text` in
`molt-ui/src/i18n.rs`) and the README (W6):

- no quit (close the tab; nothing in the window offers one, §5.2);
- tabs and the native window show the same screen (one shared session);
- stored API keys cannot be shown, only replaced;
- paste needs the browser's paste gesture or a clipboard permission
  (the paste buttons stay: the read starts inside the click, §6.3;
  whether a browser honours that from a Slint click is unchecked);
- no folder pickers; server directories are typed;
- no wiki export download (server path only);
- files over the web ceiling (64 MiB per file through the browser) go
  through the server-path flow; uploads need a workspace on disk;
- no screen reader support (Slint renders its own text);
- IME and mobile keyboards are weak; the layout needs 990 px width;
- `Ctrl+W` and `Tab` are the browser's: behind the door the wiki closes
  a tab on `F4` and flips edit and preview on `F2` (built 2026-11-18;
  the two tooltips name `F2` there;
  `the_wiki_keys_have_browser_safe_twins_behind_the_door`). The native
  keys stay bound, so a browser that does pass `Tab` to the canvas
  still works;
- proportional text renders in Inter unless a proportional font is
  bundled; the 990 px minimum was tuned against DejaVu Sans Bold and is
  re-checked in W0/W5;
- right-click menus, colour emoji (Twemoji COLR under FemtoVG/WebGL) and
  popups are unverified until W0 loads the real window.

## 10. Stages

Each stage is test-first and reviewed. They sit on the branch
`web-ui-slint`, one commit series; landing on master is open (§12). What
a stage left open is in §12, not here.

- **W0 Measure and load.**
  (a) done: target, pinned CLI, minimal window end to end (§8.4).
  (b) done: the live-preview stubs type-check on wasm32 (§8.4).
  (c) done: the real window module compiles for wasm32 and loads in a
  browser (§8.4). The spike lives in `target/w0-spike` (ignored).
  (d) open (§12): the same spike with hand-set strings and models (main screen,
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
  `crates/molt-ui/src/tests/gui/remote.rs`); the web adapter of the
  client handle (`molt-web/src/socket.rs`, type-checked, not yet run in
  a browser). Red tests first: the gate test;
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
  (2026-11-18), the settings save with its secrets since W2; the rest
  of the secret draft - clearing and rotating behind the door - came
  with W5 (2026-11-18, §6.5). Proof: the GUI
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
  on (§8.3). The web implementation is written and type-checked
  (2026-11-18, §6.3); its browser run is W5's open half.
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
  (`crates/molt-ui/src/tests/gui/remote.rs`).
- **W5 `molt-web`.** Built (2026-11-18), NOT yet run in a browser:
  everything is type-checked for wasm32 with the stub window and tested
  natively where a native test can reach it. Part 1: the crate (`cdylib` +
  `rlib`, a plain workspace member with wasm32-only dependencies, §4);
  the entry `start(token, notify)`; the socket adapter on `gloo-net`
  under the kit's pump; the browser platform (§6.3); the web `Spawner`
  and `off_thread` in `molt-ui/src/web.rs` with `native.rs` and
  `Engine::Native` gated (§6.2, §6.7); `molt_ui::run_remote` (§6.6);
  chrono `wasmbind` and similar `wasm32_web_time`; the `live-preview`
  passthrough and `scripts/dev-ui.sh wasm` (§8.3); the graph test and
  the exact `wasm-bindgen` pin (§4). Part 2: the page (`index.html`,
  `page.js`, `page.css`, on the door's list) with the key prompt, the
  close reasons, reconnect inside one page load and the limits of §9;
  the window sized from the viewport through Slint's own sizing; a
  panic hook; and the web-only behaviour behind the window's `web`
  property - no stored key shown, a key shown before it is written,
  revoke and remove as actions, the connection following a rotated seat
  key, `F2` and `F4` for the wiki (§4, §6.5, §9).
- **W6 Pipeline.** Done (2026-11-18): `[profile.release-web]`, the
  wasm32 target in `rust-toolchain.toml`, `scripts/build-release-web.sh`
  with `--check-only` and its test, the web section of
  `docs_archive/build/reproducible-builds.md`, the `[web]` lines in
  `scripts/moltd.service` and the README; the first real build of
  `molt-web` through the script, its measured size and the two-build
  reproducibility check on one host (§8.4). Verified against the real
  crate (2026-11-18): package, lib and artefact names, the page
  directory, the five bundle files against `molt_remote::BUNDLE_FILES`,
  the CLI check against `Cargo.lock`, the `live-preview` passthrough
  (`scripts/dev-ui.sh wasm`), `bash -n` on both scripts and
  `--check-only` (exit 0); a test now holds the script's names, bind
  step and tar line to the crate and the door
  (`crates/molt-remote/tests/door.rs`). The landing
  page lists no downloads and was left as is.

This doc moves to `docs_archive/ui/` once all four items of §12 A are
done AND every entry of §12 B, C and D is closed or moved to its own doc
under `docs/`. Finishing the browser run and W0 (d) alone does not
archive it.

## 11. Risks

- **The wasm window compile needs about 26 GiB** of RAM + swap (§8.4);
  it fits this box only with swap, and the module grows with the window.
  Levers: `opt-level = "s"` for the window crate, LTO off for the whole
  web profile, more swap; shrinking the module is the weak lever. The
  bare module loaded in a browser (W0); the real bundle has not (§12).
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

## 12. What is still open (2026-11-18)

The one list. A section's own "Open" notes are the detail behind an
entry here; nothing is open that is not named here.

### A. Before this doc can move to the archive

0. Done 2026-11-18, headless Chromium (SwiftShader) against a scratch
   node serving the release bundle: the key prompt, a refused key, the
   connect, the window labelled and mirroring, clicks reaching the
   engine, the canvas following the viewport at 1.0 and 1.1 device
   scale, nothing in `localStorage`, `sessionStorage`, cookies or the
   URL. Found and fixed there: the first fit ran before the window was
   shown and kept the preferred width. The page requests one file the
   door answers with 404 (the favicon, by the look of it). Everything
   else in item 1 is still unchecked, Firefox included.
1. **The first real browser run** of the bundle
   (`dist/moltrepublic-web.tar.zst`, unpacked per §8.2, a node with
   `[web] port`). Nothing of W5 has run in a browser. To check, in
   Chromium and in Firefox:
   - the page: key prompt, `ready`, a refused key, a closed connection
     and the reconnect, the one reload on `version`, a password manager
     offering to save the key field (§4);
   - the canvas following the viewport, also at a fractional zoom in
     Firefox (§4, §8.4);
   - clipboard: `readText()` and `writeText()` from a Slint click, and
     Ctrl+C on the selected key in the new-key dialog (§6.3, §6.5);
   - files: the file input's `click()` from a spawned task, `save_file`
     outside a click (phase two of a download, an export), `rfd`'s
     overlay with the page's CSS under the door's content policy, a
     cancelled dialog (§6.3);
   - the alert through WebAudio; `F2` and `F4` reaching the canvas from
     inside the wiki editor (§9);
   - right-click menus, colour emoji, popups, a real GPU (§9);
   - the web `Spawner` (`slint::spawn_local`, detached) under a real
     event loop (§6.2);
   - the slow link (§5.6): which `Accept-Encoding` each browser sends
     to `http://localhost` and that it decodes what comes back, the
     module streaming-compiled from a coded answer, the window filling
     from packed frames, and the same through the SSH tunnel the
     numbers came from. Nothing of §5.6 has run in a browser.
2. **W0 (d) measurements**: the populated window (main screen, about 500
   chat rows with colour emoji, a popup, a context menu) for frame time,
   tab memory and fonts against the 990 px layout; thin LTO and
   `opt-level` 3 if the memory allows (§8.4, §10). With them: whether
   inline image fit and decode need chunking (§6.2) and what the
   Organization snapshot's picture bytes weigh over a socket (§7).
3. **Reproducibility of the web build beyond one host**: the same hash
   from a second machine, a second checkout path, a `CARGO_TARGET_DIR`
   outside the tree, and with the host-side build scripts rebuilt
   (§8.4, `docs_archive/build/reproducible-builds.md`). No release
   carries a web tarball, so no published hash exists.
4. **Landing.** The work is on `web-ui-slint`, not on master. The
   compiled native window (the non-live-preview profile of `molt-ui`,
   `molt-app`, `scripts/build-release.sh`, `scripts/gui_walk.py`) has
   not been built or run with these changes: every GUI check ran on the
   live-preview stubs. Full test runs of `molt-engine`'s integration
   tests, `molt-net`, `molt-storage`, `molt-config` and `molt-vault`
   were not repeated after the last stages.

### B. Decisions not taken

- brotli 10 or 11 for the module, made off the start path or shipped
  precompressed in the tarball: 8.8 MB instead of 10.4 MB (§5.6).
- The font: Inter as it falls out, or a bundled proportional font (§9).
- Whether the paste buttons stay behind the door (§6.3, §9); decided
  after A.1.
- Whether stored uploads travel in an export or a backup (§7).
- A republic that is not stored has no upload store: behind the door it
  can neither share a file nor set a picture (§7).
- What removes phase one's file and a bare-name export's blob on the
  server, and whether the window gets a download-dir control (§7).
- A session flag saying whether a read key or an S3 secret is stored, so
  the web form can stop guessing (§6.5).
- The page links no source and shows no licence: the tarball carries
  `COPYING` and the font licences, but the door serves only its five
  files (§2 "Licence", §5.3). Needed before a bundle is offered to
  anyone but its builder.
- Whether the release script refuses on low memory instead of warning
  (§8.2).
- CLAUDE.md: the crate order names neither `molt-ui-kit`, `molt-remote`
  nor `molt-web`; its `INTERNAL` sentence does not mention the byte
  doors; "Build, test, run" has no line for
  `scripts/build-release-web.sh` (about 26 GiB, never beside another
  window-scale build). Left to the user.

### C. Known gaps in what is built

- **The window re-reads a whole surface on every change** (measured
  2026-11-18 on a real node: one `read_state` reply of 6.0 MB for one
  surface, 16.8 MB for another). At 35 KB/s that is 3 and 8 minutes a
  reply as plain text and, by the ratios of §5.6, still about 40 s and
  2 min packed - for every chat line. While such a reply is written the
  session forwards no events; the queue overflows, the client is told
  `lag` and re-reads everything again. §5.6 makes the window fill; it
  does not make it usable on that link. An incremental read (a cursor
  or a delta per surface) is its own design.
- The first page load after an install fetches the module: 10.4 MB as
  brotli, 12.8 MB as gzip, 5 to 6 minutes at 35 KB/s. Later loads
  revalidate it (§5.6); not yet seen in a browser.
- A local process that reads a few KB/s holds a connection slot for as
  long as it likes; the totals that ended it are gone (§5.6).
- The stall floor (about 1 to 2 KB/s at 30 s) is measured on loopback
  with a 500 ms timeout, not through a real tunnel (§5.6).
- `moltd`'s own start with the real bundle was not timed in a release
  build, and nothing of §5.6 ran against the real node (§5.6).

- The link read is not re-sent when a door connection returns; the user
  edits the field (§6.4).
- A cancelled `rfd` dialog in the browser never answers, so the task
  behind it stays parked (§6.3).
- A picture proposal the engine refuses after its upload went up leaves
  that upload until the 15 minute sweep or the session's end; eight of
  them fill the open-upload quota (§7).
- A typed mirror folder in another spelling of the one in use keeps
  Apply lit (§7). A relative `mirror_dir` already stored is neither
  migrated nor refused on load.
- In the new-key dialog Enter does not press Apply while the key input
  holds the focus (§6.5).
- No console sink for `tracing` lines in the tab; a panic is logged
  (§4).
- A command issued between a connection's end and the next welcome
  waits in the handle and goes out after the welcome; the page covers
  the window meanwhile, but the mirror's own reads can queue (§4).
- Until the first session lands, a web window shows an empty footer and
  the default embedded-Tor row (§6.6).
- The download note (`to_user`) is per window and workspace: after a
  switch or a reconnect the next click hands the file over (§7).
- Not covered by any test: every `invoke_from_event_loop` hop (the
  headless backend never drains it), the real clipboard and file
  dialogs, and `start.rs`, `socket.rs`, `platform.rs` beyond the wasm
  type check.
- Seen beside this work, not caused by it, to look at: an own share
  whose message left the log may not be re-adopted after a restart
  (`adopt_share_paths`); two tests failed once under a full parallel
  run and passed alone (`mirror_gossip.rs`,
  `chat_tests::file_share_lifecycle_download_until_removed`).

### D. Pipeline, with the first release that ships the tarball

- Download lines in the README and on the landing page (§8.2).
- `wasm-bindgen-cli` at the `Cargo.lock` version on the release box
  (0.2.126, installed here 2026-11-18; again after every lock bump).
- `shellcheck` over the two scripts (not installed here; `bash -n`
  only).
- The busy check sees only its own target dir; a window-scale build
  elsewhere is the releaser's to avoid (§8.2).

### Out of v1 by decision (not open)

A non-loopback bind or TLS (D2, its own ADR), per-connection view state
(§3), a web door for the read key (§5.3), driving the web GUI over MCP
(§5.5; the browser window sends no `ui_publish`, which the door refuses
by design - until 2026-11-18 it did, one WARN per mirror pass in the
node's log), a browser download of the wiki export (§7), `wasm-opt`
(§8.2).
