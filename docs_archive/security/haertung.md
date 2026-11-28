# Hardening: queue-creds persist at mesh-up + honest offline state

> **Historical (2026-07-30):** the SMP transport this plan targets was removed
> in etappe N-demo of the Nostr transport replacement
> (`docs_archive/transport/nostr_transport_marmot.md`). The CORE it designed — the
> live queue-creds/MLS persist at every mesh-up and the honest "detached"
> offline state — survived and was carried into the delivery guarantee
> (`docs_archive/transport/delivery_guarantee.md`); the SMP-specific parts
> (`SmpTransport`, `reopen_transport`, SKEY sender keys) are historical.

> **STATUS: READY TO EXECUTE (2026-07-19).** Analysis and design are done and
> verified against the source state **master `1d4f273`**; the red tests (TDD)
> are committed and ready on branch **`worktree-mesh-cred-persist`**
> (commit `d18e1e5`, worktree `.claude/worktrees/mesh-cred-persist`) and are
> also reproduced in full in the appendix of this document. This plan is
> written so that it can be worked off without further research.
> CLAUDE.md applies without restriction (TDD, clippy 0, review before merge,
> end state = tested green on master).

---

## 0 The task in one sentence

After a **hard kill** (Ctrl+C with the workspace open), a seat must be able to
resume its real mesh on the next open (part 1), and if it
cannot, the state must be **unmistakably honest** — never a
silent "looks healthy, but sends nothing" (part 2).

**Out of scope (deliberately):** the reconnect/resubscribe supervisor ("Stage B")
— note it as open in the final report, do NOT start it.

---

## 1 The incident and the verified chain of causes

A 2-of-3 republic across three moltd instances died after a restart; two
instances had been hard-killed beforehand. Chain (all locations @`1d4f273`):

1. **SMP queue receive credentials (`smp_queues` in `TransportState`) are
   persisted ONLY by the clean-close path** (`persist_net_crypto_on_close` →
   `persist_crypto_blocking`). The founding/join writes write
   `TransportState { mls, mesh, identity_sk, ..Default::default() }` — i.e.
   `smp_queues: None`:
   - `crates/molt-engine/src/lifecycles.rs` ~110–118 (`materialize_workspace`)
   - Founder merge without creds: `crates/molt-engine/src/founding.rs` ~1713–1723
     (`cmd_net_mesh_ready` calls `persist_mesh_crypto_blocking(Some(mls), None,
     mesh)` — the second argument is currently hard-coded `None`).
   - The storage writer's periodic cursor save writes only cursors and
     never adds creds (`crates/molt-storage/src/lib.rs` ~1975–1994).
2. **On reopen the real mesh resumes only if `mls` AND `smp_queues` AND a
   non-empty `mesh` AND a resolved dialer are present**
   (`crates/molt-engine/src/session.rs` ~594–601); any absence silently falls through to
   `ensure_demo_net()` (a no-op in production) → the workspace opens **with no
   transport at all**.
3. **The state is invisible:** the "detached" notice fires only when EVERYTHING
   is missing (with MLS it is cleared, session.rs ~614–623); `net_health` stays
   `Ok`; `chat_send` acks locally and sends nothing. A hard-killed seat is
   unrecoverable except via the recovery ritual, because the receive creds existed
   only in process memory.

---

## 2 Verified current state (saves searching again)

- **All mesh-up sites in the engine code** (where a real network comes into being):
  1. Founder: `cmd_net_mesh_ready` (`founding.rs` ~1693–1744) — currently persists
     `mls + mesh` via `persist_mesh_crypto_blocking`, **creds = None**.
  2. Joiner: `cmd_net_join_sealed` (`lifecycles.rs` ~1174–1191) — takes the
     ritual transport from `self.join_transport`, builds the network, **persists
     no creds** (materialize wrote mls+mesh beforehand).
  3. Recovery: `cmd_net_recover_sealed` (`lifecycles.rs` ~1433–1450) — twin
     of the joiner using `self.recover_transport`, **persists no creds**.
  4. Mesh extension: `cmd_net_mesh_extended` (`net.rs` ~1422–1477) —
     **already persists correctly** `mls + creds + mesh` via
     `crypto_for_close()` → NOTHING to do here (reference pattern!).
  5. Reopen: `cmd_open_workspace` (`session.rs` ~551–626) with
     `reopen_transport` (`founding.rs` ~44–53).
- **The storage writer is already correct:** `WriterMsg::SaveTransport` does a
  read-modify-write and takes over ONLY the cursor maps; `MergeCrypto`
  (read-modify-write, fsync, optional `seal`) preserves cursors. Both are pinned by
  existing tests: `merge_crypto_preserves_delivery_cursors` and
  `a_cursor_save_never_clobbers_a_live_crypto_merge` (molt-storage,
  `lib.rs` module tests). **NO change is needed in molt-storage.**
- **`persist_mesh_crypto_blocking(mls, creds, mesh)`** is the right
  choke point for all new persists: LIVE merge (no `seal`!), blocking
  until fsync, callable from the actor (same pattern as mesh extension and
  clean close). **Never use `persist_crypto_blocking` for mesh-up** —
  it sets `seal=true` and would discard all later cursor saves of the running
  session.
- **`LoopbackTransport::export_creds()` currently returns `None`** (trait default,
  `crates/molt-net/src/lib.rs` ~223–235), and `reopen_transport` returns `None` for
  a loopback mesh (server name "loopback" does not parse as an SMP URL).
  That is why test (2) needs the reopen seam (§4 S2).
- **The GUI needs NO change:** `molt-ui/src/lib.rs:2212` matches exactly
  `sv.notice == "detached"` → toast; `:2233–2235` renders `net_health` as a
  pill (red on `Down`, reason as tooltip). MCP reads `SessionView.notice` +
  `SessionView.net_health` unchanged. No new `Command` → no
  MCP co-equality change.
- **Dropping `WalletHandle` models the hard kill correctly:** all
  engine-internal holders of the command channel are `WeakSender`
  (state/sinks/ticker/brains); when the last strong sender (the handle) falls,
  the actor loop ends, the writer thread ends and releases the flock LOCK
  — WITHOUT a clean-close persist. The tests build on exactly that.
- The `Transport` trait is in scope in `founding.rs`; in `lifecycles.rs` and
  `session.rs` write the new trait calls **fully qualified**
  (`molt_net::Transport::export_creds(&t)` / `…::import_creds(&t, creds)`),
  then no import change is needed.

---

## 3 Design decisions (pinned down)

- **D1 — Persist timing:** "the moment the real mesh stands" =
  the three actor handlers `cmd_net_mesh_ready` / `cmd_net_join_sealed` /
  `cmd_net_recover_sealed`. Earlier (at `materialize_workspace`) the
  founder has no mesh queues yet; later would leave a window open again.
- **D2 — Merge semantics:** always the LIVE merge (`seal=false`). Ordering is
  safe: all transport.state writers run FIFO through the one
  writer thread; a later clean close overwrites mls/creds with newer
  values (RMW), never the other way round.
- **D3 — Loopback creds are real, not a fake:** `LoopbackTransport` gets
  a set, shared across clones, of the queue ids it created (the
  loopback analogue of SMP's receive credentials; `SmpTransport` shares its
  state the same way via an Arc). Export = serialization of these ids; only
  valid while the hub lives (a new PROCESS can never resume a loopback mesh
  — a fresh engine on the same hub in a test can). The founder
  export also contains the (dead) star queues of the ritual — harmless, the
  supervisor subscribes only to the mesh queues from the links.
- **D4 — Reopen seam:** `#[doc(hidden)] __spawn_with_reopen_transport(config,
  session, transport)` — a test seam exactly in the tradition of
  `__spawn_manual_founding*`/`__spawn_demo_mesh`. The product never installs it;
  the real reopen still builds `reopen_transport` (SmpTransport +
  `import_creds`).
- **D5 — Honest offline state:** the notice token stays exactly `"detached"`
  (GUI exact match, zero GUI effort); the precise cause is carried by
  `net_health = Down { reason }` (pill tooltip + MCP). Dialer failures
  keep their own fail-closed `Down` reason from `resolve_dialer` and
  get NO detached notice (fix the settings + reopen resumes —
  the workspace is not detached).
- **D6 — Import-detached stays unchanged:** the existing §4.4 case
  (chain present, no transport evidence whatsoever) keeps its behavior (notice
  `"detached"`, net_health untouched) — `restore_real.rs` pins it. Raising it
  to `Down` as well is an optional follow-up, not part of this hardening.
- **D7 — Offline-first stays:** `chat_send` and others keep writing locally;
  nothing blocks.

---

## 4 Implementation step by step

**Way of working:** a worktree of its own (`worktree-mesh-cred-persist`
with the red tests ALREADY EXISTS — keep working there or
cherry-pick commit `d18e1e5` into a fresh worktree). NEVER commit in the
main checkout; at the end a normal merge to master (§7).

**Step 0 — verify red.**
```
cargo test -p molt-engine --test mesh_resume --no-run
```
Expectation @`d18e1e5`: exactly ONE error — `__spawn_with_reopen_transport` missing
(E0425). After S2 everything compiles; then run it: tests (1) and (2) red
(no creds on disk / no resume), test (3) red (net_health Ok instead of
Down). Only then implement.

### S1 — molt-net: real loopback creds

`crates/molt-net/src/loopback.rs`:

```rust
/// One node's endpoint on a [`LoopbackHub`].
#[derive(Clone)]
pub struct LoopbackTransport {
    hub: LoopbackHub,
    /// Queue ids this endpoint created (= receives on): the loopback
    /// analogue of SMP's receive credentials. Shared across clones (like
    /// `SmpTransport`'s state Arc) so ritual/runtime clones export ONE
    /// credential set. Only meaningful while the hub lives — a new PROCESS
    /// cannot resume a loopback mesh; a fresh engine on the same hub
    /// (the tests' reopen seam) can.
    created: Arc<Mutex<BTreeSet<Vec<u8>>>>,
}
```

- `LoopbackHub::transport()` initializes `created: Arc::new(Mutex::new(BTreeSet::new()))`.
- `create_queue` (trait impl) registers: after `create_queue_blocking()`
  `self.created.lock()` → `insert(pair.rcv.id.0.clone())`.
- Extend the trait impl:

```rust
fn export_creds(&self) -> Option<Vec<u8>> {
    let ids: Vec<String> = self.created.lock().ok()?.iter().map(hex::encode).collect();
    if ids.is_empty() {
        return None;
    }
    serde_json::to_vec(&ids).ok()
}

fn import_creds(&self, creds: &[u8]) {
    // bookkeeping only — the hub is permissive (any endpoint may subscribe),
    // so adopting the ids keeps a re-export after reopen faithful
    let Ok(ids) = serde_json::from_slice::<Vec<String>>(creds) else { return };
    if let Ok(mut c) = self.created.lock() {
        for id in ids {
            if let Ok(b) = hex::decode(id) {
                c.insert(b);
            }
        }
    }
}
```

- Add the import `std::collections::BTreeSet`; `hex`/`serde_json` are already
  dependencies of molt-net.
- Grep first: `LoopbackTransport {` is constructed ONLY in `LoopbackHub::transport()`
  — if further literals exist after all, adapt them all.
- Update the doc comment on the trait (`crates/molt-net/src/lib.rs`, `export_creds`):
  the half-sentence "`None` for … the loopback hub …" is no longer true afterwards
  — loopback now exports its queue ids (valid process-locally).

### S2 — Engine seam: `__spawn_with_reopen_transport`

`crates/molt-engine/src/lib.rs`:

1. `State` gets a field (next to the other seam fields, ~l. 560):

```rust
/// Reopen **test seam only** ([`__spawn_with_reopen_transport`]): resume a
/// persisted mesh over THIS transport instead of building a fresh
/// `SmpTransport` from the mesh links. Lets the loopback tests drive a
/// literal hard-kill + reopen of a full engine (their hub survives in the
/// test, like a real SMP server would). The product never sets it.
pub(crate) reopen_seam: Option<founding::RitualTransport>,
```

   Initialize it in `State::new` with `reopen_seam: None`.
2. `spawn_actor` gets a 14th parameter
   `reopen_seam: Option<founding::RitualTransport>` and sets it like the
   other seams (`state.reopen_seam = reopen_seam;`). ALL existing
   callers (8 sites: `spawn_inner`, `__spawn_manual_founding`,
   `…_bootstrap`, `…_bootstrap_recoverable`, `__spawn_demo_mesh`,
   `__spawn_sim_founding`, `__spawn_manual_founding_over_smp`,
   `spawn_with_config`) pass `None`.
3. New spawner (next to the other `__spawn_*`):

```rust
/// Storage-backed engine that resumes a persisted mesh over the GIVEN
/// transport instead of a fresh `SmpTransport` — the loopback reopen seam
/// for the hard-kill tests. The product never uses it: the real reopen
/// path builds `reopen_transport` from the persisted mesh + creds.
#[doc(hidden)]
pub fn __spawn_with_reopen_transport(
    config: GroupConfig,
    session: SessionView,
    transport: founding::RitualTransport,
) -> WalletHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Envelope>(CMD_QUEUE);
    spawn_actor(
        config, session, cmd_tx, cmd_rx, None, true, None, None, false, false,
        false, None, false, Some(transport),
    )
}
```

   (Adapt the argument order to the actual signature!)

### S3 — `cmd_open_workspace`: use the seam + honest offline state

`crates/molt-engine/src/session.rs`, REPLACE the block from `let dialer = …` (~l. 594)
up to and including the notice assignment (~l. 623) with:

```rust
let dialer = self.resolve_dialer().ok();
let resumed = match (&transport_state.mls, &transport_state.smp_queues, &dialer) {
    (Some(mls), Some(creds), Some(dialer)) if !transport_state.mesh.is_empty() => {
        // the reopen seam (tests): a transport on the still-running loopback
        // hub replaces the fresh-SmpTransport build — same import contract
        let transport = if let Some(seam) = self.reopen_seam.clone() {
            molt_net::Transport::import_creds(&seam, creds);
            Some(seam)
        } else {
            crate::founding::reopen_transport(&transport_state.mesh, creds, dialer.clone())
        };
        transport.and_then(|t| self.build_real_net(t, &transport_state.mesh, mls))
    }
    _ => None,
};
let resumed_real = resumed.is_some();
if let Some(net) = resumed {
    self.net = Some(net);
} else {
    self.ensure_demo_net();
}
self.session.screen = Screen::Main;
// the honest OFFLINE state (2026-07-19 incident): a workspace whose
// transport.state carries real-mesh evidence (MLS/creds/links) but whose
// mesh did NOT resume must never look healthy — net_health goes Down with
// the exact gap, and the persistent "detached" notice is set. A dialer
// failure keeps its own fail-closed Down reason from resolve_dialer (the
// workspace is not detached — fixing the setting + reopening resumes).
let offline = if resumed_real || !self.persist || dialer.is_none() {
    None
} else if transport_state.mls.is_some()
    || transport_state.smp_queues.is_some()
    || !transport_state.mesh.is_empty()
{
    Some(if transport_state.smp_queues.is_none() {
        "offline: no queue credentials on disk — the mesh cannot resume on \
         this seat (hard shutdown before the mesh came up, or a pre-fix \
         build); local reads/writes work, nothing reaches the peers; rejoin \
         via a recovery link"
    } else if transport_state.mls.is_none() {
        "offline: no MLS group snapshot on disk — the mesh cannot resume; \
         rejoin via a recovery link"
    } else if transport_state.mesh.is_empty() {
        "offline: no mesh links on disk — the mesh cannot resume; rejoin \
         via a recovery link"
    } else {
        "offline: resuming the persisted mesh failed — local reads/writes \
         work, nothing reaches the peers"
    })
} else {
    None
};
self.session.notice = if let Some(reason) = offline {
    self.session.net_health = molt_core::NetHealth::Down { reason: reason.to_string() };
    "detached".to_string()
} else if self.persist
    && self.chain_head.is_some()
    && transport_state.mls.is_none()
    && transport_state.mesh.is_empty()
    && transport_state.smp_queues.is_none()
{
    // backup_restore_design.md §4.4: imported knowledge, no live membership
    "detached".to_string()
} else {
    String::new()
};
```

(The existing §4.4 comment block above the old notice assignment can move into
the new code; the two conditions are disjoint — evidence
vs. no evidence at all.)

### S4 — Founder: creds in the mesh-ready merge

`crates/molt-engine/src/founding.rs`, `cmd_net_mesh_ready` (~l. 1711–1732),
REPLACE the persist/build part with:

```rust
self.founder_mesh_in = None;
let peers = mesh.len();
// reuse the ritual transport for the runtime supervisor AND export its
// queue credentials: the receive keys of the star+mesh queues live only in
// this transport's memory. Persisting them NOW — not only on clean close —
// is what makes a hard kill after this point survivable (2026-07-19).
let transport = self.runtime_transport.take();
if let Some(active) = &self.active {
    let creds = transport.as_ref().and_then(|t| t.export_creds());
    // merge the founder's post-bootstrap MLS + assembled mesh + queue creds
    // into transport.state (a LIVE merge: the writer owns the file, and
    // plain cursor saves carry only the cursor maps)
    active.handle.persist_mesh_crypto_blocking(
        Some(mls_snapshot.clone()),
        creds,
        mesh.clone(),
    );
}
// stand the runtime supervisor up over the direct mesh, reusing the
// ritual transport (the loopback hub / the founder's SMP server)
if let Some(transport) = transport {
    if let Some(net) = self.build_real_net(transport, &mesh, &mls_snapshot) {
        self.teardown_net();
        self.net = Some(net);
    }
}
```

### S5 — Joiner: creds at join-sealed

`crates/molt-engine/src/lifecycles.rs`, `cmd_net_join_sealed` (~l. 1179–1188),
REPLACE the network build block with:

```rust
let (mls_blob, mesh) = net_seed;
let reused = self.join_transport.lock().ok().and_then(|mut s| s.take());
if self.persist && !mesh.is_empty() {
    if let (Some(blob), Some(transport)) = (mls_blob, reused) {
        // hard-kill safety (2026-07-19): the bootstrap queues' receive
        // credentials exist only in this transport's memory — merge them
        // into transport.state NOW, not only on clean close (live merge;
        // materialize_workspace already wrote mls + mesh synchronously)
        if let (Some(active), Some(creds)) = (
            self.active.as_ref(),
            molt_net::Transport::export_creds(&transport),
        ) {
            active.handle.persist_mesh_crypto_blocking(None, Some(creds), mesh.clone());
        }
        if let Some(net) = self.build_real_net(transport, &mesh, &blob) {
            self.teardown_net();
            self.net = Some(net);
        }
    }
}
```

### S6 — Recovery: creds at recover-sealed

The same change in the twin `cmd_net_recover_sealed` (~l. 1437–1446), with
`self.recover_transport` instead of `join_transport` (the surrounding code there is
already structured identically — only extend the inner `if let` block as in S5
with the creds persist).

### S7 — Keep the docs honest

In **CLAUDE.md**, section "Transport gotchas", adjust the sentence "Only a CLEAN close
persists — …": since this hardening, founder mesh-ready,
join-sealed, recover-sealed and mesh extension persist the queue creds + MLS snapshot
**live at mesh-up**; the clean-close merge still writes the
NEWEST ratchet state (a hard kill resumes from the last persisted
ratchet — a few in-flight messages get replay-rejected, accepted).

---

## 5 Expected test landscape (audit guide)

After S1–S6 first run `cargo test -p molt-engine --test mesh_resume` (green),
then the full suite. Possible knock-on effects, **check individually, do not
blanket-fix**:

- Loopback workspaces that are **cleanly closed and reopened** in tests
  now have creds on disk; without the seam `reopen_transport` fails
  ("loopback" is not an SMP URL) → new: `notice == "detached"` +
  `net_health Down` instead of the former silent nothing. That is the HONEST state
  — adapt the assertions of such tests if needed (or install the seam if
  the test means a real resume). Candidates: `two_instances.rs`,
  `three_nodes.rs`, `founding.rs`, `chat_channels.rs`, `demo_mesh.rs`.
- `restore_real.rs` (import-detached, lines 218/434) MUST stay green
  unchanged (D6). `persisted_mesh.rs` is unaffected (its engine workspace
  has no transport evidence whatsoever).
- Sim founding (`__spawn_sim_founding`, without bootstrap) writes mls, but
  neither mesh nor creds → reopen now shows Down+detached. That too is
  honest (a seat without links); adapt the affected assertions.
- Do NOT run the `#[ignore]` network tests (`ritual_engine_over_smp.rs`, `tor_e2e.rs`)
  locally (real SMP server; besides, the user currently has three moltd
  instances running). Change nothing in them.

**Clippy traps:** no `.unwrap()` (also in tests → `.expect("…")`);
`--all-targets` must show 0 warnings.

---

## 6 Validation (mandatory, in this order)

```
cargo test -p molt-engine --test mesh_resume        # new tests green
cargo test -p molt-engine -p molt-storage -p molt-net
cargo clippy --all-targets                          # 0 warnings
```

- **NO** `molt-ui-window` build (no GUI/strings change needed; if
  something in molt-ui is touched after all: exactly ONE `cargo build -p
  molt-ui-window -p molt-ui`, first check that no other build is running,
  with tight RAM `-j 1`).
- Worktree trap (memory): a shared `CARGO_TARGET_DIR` across worktrees mixes
  molt-* artifacts — authoritative runs in the worktree's own `target/`; on
  ghost errors `cargo clean -p molt-…`.
- **Taboo:** the user's running moltd instances, `~/.moltrepublic*`,
  `config.toml`/`config2.toml`/`config3.toml` (untracked in the repo dir) —
  neither read-write nor kill. Tests only against temp dirs.

---

## 7 Review + landing

1. **Code review over the full diff** (CLAUDE.md duty), fix the findings.
   Review focus: (a) no new persist accidentally uses the sealing
   `persist_crypto_blocking`; (b) `cmd_net_mesh_ready` persists
   unchanged (creds `None`) even when `runtime_transport` is already gone;
   (c) the offline block NEVER sets `Down` on `resumed_real`,
   `!persist` or dialer failure; (d) the import path (`import.rs`) still
   never exports creds (existing test line ~616).
2. Merge a fresh local master (master may move — another session
   active!), suite + clippy again, then in the main checkout
   `git -C /home/user/projects/moltrepublic merge <branch>` (normal merge,
   NEVER push, NEVER force, NEVER rewrite history). On conflicts that cannot be resolved
   safely: STOP and report.
3. Delete worktree + branch (`git worktree remove …`, `git branch -d …`).
   The remaining `worktree-agent-*` worktrees belong to other sessions — do not
   touch them. (A branch `worktree-settings-network-fixes`, if it still exists,
   likewise belongs to another session.)
4. Final report: what is persisted when and where; test names + what each one
   pins; how the offline state appears (`notice="detached"`,
   `net_health=Down{reason}`); review findings; master hash; tests+clippy
   green; **Stage B (reconnect supervisor) stays open**.

---

## 8 Deliberately open (after this hardening)

- **Stage B:** reconnect/resubscribe supervisor (monitor the running connection,
  resubscribe, track health live) — explicitly NOT part of
  this task.
- Recovery-link mint window: the coordinator's dedicated recovery queue
  is created off-actor; a hard kill between mint and mesh extension
  loses it (link dead, mint again). Small, worth hardening separately.
- Add the assertion "creds on disk after engine join" to the `#[ignore]` SMP test
  (executable only with a real server).
- Also raise import-detached to `net_health Down` (D6 follow-up).
- Per-drain MLS persist (full crash safety of the ratchet) — known
  open hardening from CLAUDE.md.

---

## Appendix A — the red tests (complete)

Committed as `crates/molt-engine/tests/mesh_resume.rs` on branch
`worktree-mesh-cred-persist` (`d18e1e5`); if the branch/worktree are lost,
restore the file 1:1 from this appendix.

```rust
// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **Hard-kill resilience of the runtime mesh** (the 2026-07-19 incident).
//!
//! Queue receive-credentials used to be persisted ONLY by the clean-close
//! path, so a hard kill (Ctrl+C with the workspace open) left a
//! `transport.state` without `smp_queues` — and the next open silently fell
//! through to no transport at all while `net_health` still said Ok.
//!
//! Pinned here:
//! 1. The moment the real mesh comes up (founder's `NetMeshReady`), the
//!    on-disk `transport.state` already carries the queue credentials —
//!    WITHOUT any close having happened.
//! 2. A hard-killed engine (dropped, never `CloseWorkspace`d) reopens into a
//!    WORKING mesh: a fresh engine on the same directory resumes and chats
//!    both directions with the surviving peer.
//! 3. A workspace whose `transport.state` has MLS state but no usable queue
//!    credentials opens HONESTLY offline: `net_health` is Down with a
//!    reason and the persistent `"detached"` notice is set — never a silent
//!    ok — while local (offline-first) chat keeps working.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{await_chat_len, read_chat};
use molt_core::{
    Command, EventEnvelope, MemberId, MeshLink, NetHealth, Reply, SessionSettings, SessionView,
    WorkspaceEvent,
};
use molt_engine::WalletHandle;
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::{EngineSink, MlsChannel, MlsMember, NetError, PeerLink};
use tokio::sync::watch;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Records what the member-side supervisor delivers.
#[derive(Clone, Default)]
struct RecordSink {
    got: std::sync::Arc<std::sync::Mutex<Vec<(MemberId, EventEnvelope)>>>,
}
impl RecordSink {
    fn messages(&self) -> Vec<(MemberId, EventEnvelope)> {
        self.got.lock().expect("lock").clone()
    }
}
impl EngineSink for RecordSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.got.lock().expect("lock").push((from.clone(), env));
        Ok(())
    }
    async fn peer_seen(&self, _m: &MemberId) {}
    async fn send_failed(&self, _m: &MemberId, _r: &str) {}
}

/// Run a real 2-of-2 founding + mesh bootstrap over the loopback hub:
/// founder engine on `root_a`, genuine member via `run_ritual_member`.
/// Returns the founder handle, the shared hub transport, the member's
/// assembled mesh + post-bootstrap MLS snapshot, and the workspace id.
async fn found_with_mesh(
    root_a: &Path,
) -> (
    WalletHandle,
    molt_engine::RitualTransport,
    Vec<MeshLink>,
    Vec<u8>,
    String,
) {
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding_bootstrap(molt_core::GroupConfig::demo(), session_a);
    a.execute(Command::CreateStart {
        name: "Phoenix".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
    })
    .await
    .expect("create start");
    let materials = tokio::task::spawn_blocking(move || {
        material_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A hands out the invite material")
    })
    .await
    .expect("join blocking");
    let seat = materials.into_iter().next().expect("seat material");
    let hub = seat.transport.clone();

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(
            seat,
            "member-b".to_string(),
            b_phrase,
            true,
            true,
            None,
            None,
        )
        .await
        .expect("B completes the member side + bootstrap")
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "member-b never joined");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Phoenix".to_string(),
        agenda: "survive a hard kill".to_string(),
    })
    .await
    .expect("founder proposes the charter");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(&a).await;
        assert_ne!(s.create.run.outcome, 2, "ritual must not fail: {:?}", s.create.run.log);
        if s.create.run.outcome == 1
            && s.create.run.log.iter().any(|l| l.contains("direct mesh established"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder never bootstrapped its mesh; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let b_outcome = b_task.await.expect("B task");
    let member_mesh = b_outcome.mesh.expect("B assembled its direct mesh");
    let member_mls = b_outcome.mls_snapshot.expect("member post-bootstrap snapshot");
    let id = read_session(&a).await.active_workspace.clone();
    assert!(!id.is_empty(), "the founded workspace is active");
    (a, hub, member_mesh, member_mls, id)
}

/// Hard-kill the engine (drop, never `CloseWorkspace`) and wait until its
/// writer thread released the workspace LOCK. Returns the workspace dir.
async fn hard_kill(a: WalletHandle, root: &Path, id: &str) -> PathBuf {
    drop(a);
    let dir = molt_storage::find_workspace_dir(root, id).expect("workspace dir");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match molt_storage::open_workspace(&dir) {
            Ok(_) => break, // lock free again; the guard drops right here
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the hard-killed engine never released the workspace lock"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    dir
}

/// (1) The mesh-up persist: once the direct mesh is established, the queue
/// credentials are ALREADY on disk — no clean close ever happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mesh_up_persists_queue_creds_without_any_close() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (a, _hub, _mesh, _mls, id) = found_with_mesh(&root_a).await;

    // HARD kill — no CloseWorkspace, no clean-close merge
    let dir = hard_kill(a, &root_a, &id).await;

    let (ws, _loaded) = molt_storage::open_workspace(&dir).expect("open raw");
    let ts = ws.read_transport_state();
    assert!(ts.mls.is_some(), "the post-bootstrap MLS snapshot is on disk");
    assert_eq!(ts.mesh.len(), 1, "the assembled mesh link is on disk");
    assert!(
        ts.smp_queues.as_ref().is_some_and(|c| !c.is_empty()),
        "the queue credentials are on disk WITHOUT a clean close — a hard \
         kill after mesh-up must be survivable"
    );
}

/// (2) The keystone: hard-killed founder, fresh engine on the same dir,
/// reopen resumes a WORKING mesh — chat crosses both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hard_killed_founder_resumes_the_mesh_on_reopen() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root_a = tmp.path().join("founder");
    let (a, hub, member_mesh, member_mls, id) = found_with_mesh(&root_a).await;
    a.execute(Command::CreateFinish).await.expect("enter");

    // HARD kill the founder (the loopback hub — standing in for the SMP
    // server — survives in `hub`, exactly like a real server would)
    hard_kill(a, &root_a, &id).await;

    // a fresh engine "process" on the same directory; the reopen seam hands
    // it a transport on the still-running hub (what a fresh SmpTransport +
    // import_creds does against a real server)
    let session_a2 = SessionView {
        workspaces: molt_storage::scan_workspaces(&root_a)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let a2 = molt_engine::__spawn_with_reopen_transport(
        molt_core::GroupConfig::demo(),
        session_a2,
        hub.clone(),
    );
    a2.execute(Command::OpenWorkspace { id: id.clone() })
        .await
        .expect("reopen after hard kill");
    let sv = read_session(&a2).await;
    assert_eq!(
        sv.net_health,
        NetHealth::Ok,
        "the resumed mesh is healthy, not silently absent"
    );
    assert_eq!(sv.notice, "", "no detached notice — the mesh resumed");

    // the surviving member's runtime supervisor (kept alive across A's kill)
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    let member_group = MlsMember::restore(&member_mls).expect("restore member MLS");
    let member_feed = MemLog::new();
    let member_sink = RecordSink::default();
    let (member_wake, member_wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub,
        NetConfig::fast("member-b".to_string(), links, 11),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        member_wake_rx,
        Some(MlsChannel::new(member_group)),
    );

    // founder → member across the resumed mesh
    a2.execute(Command::Chat {
        body: "back from the dead".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("chat after resume");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let got = member_sink.messages();
        if got.iter().any(|(from, env)| {
            from == "founder-a"
                && matches!(&env.body, WorkspaceEvent::Chat(m) if m.body == "back from the dead")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resumed founder's chat never reached the member; got {:?}",
            got.iter().map(|(f, _)| f).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // member → founder across the resumed mesh
    member_feed.push(common::chat_env(2, "member-b", "good to have you back"));
    let _ = member_wake.send(2);
    let chat = await_chat_len(&a2, 2, 15).await;
    assert!(
        chat.iter().any(|m| m["body"] == serde_json::json!("good to have you back")
            && m["from"] == serde_json::json!("member-b")),
        "the member's chat reached the resumed founder: {chat:?}"
    );
}

fn genesis(member: &str) -> EventEnvelope {
    EventEnvelope {
        seq: 1,
        ts: 1_751_000_000,
        by: member.to_string(),
        body: WorkspaceEvent::Founded {
            name: "Orphaned".to_string(),
            rule_m: 2,
            rule_n: 2,
            member: member.to_string(),
            roster: vec!["ada".to_string(), "ben".to_string()],
            identities: Vec::new(),
            attestations: Vec::new(),
            republic_id: String::new(),
            agenda: String::new(),
        },
    }
}

/// (3) The honest offline state: MLS + mesh persisted, queue creds MISSING
/// (the incident's on-disk shape after a hard kill on a pre-fix build).
/// The open must say Down + "detached", not a silent healthy-looking no-op —
/// while local chat still appends (offline-first).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_with_mls_but_no_creds_is_honestly_offline() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("node-b");
    let seed = molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("gen"))
        .expect("entropy");
    let ws = molt_storage::create_workspace(&root, &seed, &genesis("ben")).expect("create");
    let id = ws.manifest.workspace.id.clone();
    // exactly what the founding-time write left behind: group + mesh, NO creds
    ws.write_transport_state(&molt_core::TransportState {
        mls: Some(b"opaque-mls-snapshot".to_vec()),
        mesh: vec![MeshLink {
            member: "ada".to_string(),
            snd_server: "smp://AAAA@host.example".to_string(),
            snd_queue: "aa".to_string(),
            snd_wrap: "bb".to_string(),
            rcv_queue: "cc".to_string(),
            rcv_wrap: "dd".to_string(),
        }],
        ..molt_core::TransportState::default()
    })
    .expect("write transport.state");
    drop(ws); // release the LOCK

    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(&root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let w = molt_engine::spawn_with_storage(molt_core::GroupConfig::demo(), session);
    w.execute(Command::OpenWorkspace { id }).await.expect("open");

    let sv = read_session(&w).await;
    assert!(
        matches!(&sv.net_health, NetHealth::Down { reason } if !reason.is_empty()),
        "opening with MLS but no queue creds must be an HONEST Down, got {:?}",
        sv.net_health
    );
    assert_eq!(
        sv.notice, "detached",
        "the persistent detached/offline notice is set"
    );

    // offline-first: the local log still accepts writes
    w.execute(Command::Chat {
        body: "written while offline".to_string(),
        quote: None,
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("offline chat appends locally");
    let chat = read_chat(&w).await;
    assert!(
        chat.iter().any(|m| m["body"] == serde_json::json!("written while offline")),
        "offline-first local chat: {chat:?}"
    );
}
```
