# Härtung: Queue-Creds-Persist beim Mesh-Up + ehrlicher Offline-Zustand

> **Historical (2026-07-30):** the SMP transport this plan targets was removed
> in etappe N-demo of the Nostr transport replacement
> (`docs/transport/nostr_transport_marmot.md`). The CORE it designed — the
> live queue-creds/MLS persist at every mesh-up and the honest "detached"
> offline state — survived and was carried into the delivery guarantee
> (`docs/transport/delivery_guarantee.md`); the SMP-specific parts
> (`SmpTransport`, `reopen_transport`, SKEY sender keys) are historical.

> **STATUS: AUSFÜHRUNGSREIF (2026-07-19).** Analyse und Design sind fertig und
> gegen den Quellstand **master `1d4f273`** verifiziert; die roten Tests (TDD)
> liegen fertig committet auf Branch **`worktree-mesh-cred-persist`**
> (Commit `d18e1e5`, Worktree `.claude/worktrees/mesh-cred-persist`) und sind
> zusätzlich vollständig im Anhang dieses Dokuments. Dieser Plan ist so
> geschrieben, dass er ohne weitere Recherche abgearbeitet werden kann.
> CLAUDE.md gilt uneingeschränkt (TDD, clippy 0, Review vor Merge, Ende =
> getestet-grün auf master).

---

## 0 Auftrag in einem Satz

Nach einem **harten Kill** (Ctrl+C bei offenem Workspace) muss ein Sitz beim
nächsten Öffnen sein echtes Mesh wieder aufnehmen können (Teil 1), und wenn er
es nicht kann, muss der Zustand **unübersehbar ehrlich** sein — niemals ein
stilles „sieht gesund aus, sendet aber nichts" (Teil 2).

**Out of scope (bewusst):** der Reconnect-/Resubscribe-Supervisor („Stage B")
— im Abschlussbericht als offen vermerken, NICHT anfangen.

---

## 1 Der Vorfall und die verifizierte Ursachenkette

Eine 2-of-3-Republik über drei moltd-Instanzen starb nach einem Neustart; zwei
Instanzen waren zuvor hart gekillt worden. Kette (alle Fundstellen @`1d4f273`):

1. **SMP-Queue-Empfangs-Credentials (`smp_queues` in `TransportState`) werden
   NUR vom Clean-Close-Pfad persistiert** (`persist_net_crypto_on_close` →
   `persist_crypto_blocking`). Die Founding-/Join-Writes schreiben
   `TransportState { mls, mesh, identity_sk, ..Default::default() }` — also
   `smp_queues: None`:
   - `crates/molt-engine/src/lifecycles.rs` ~110–118 (`materialize_workspace`)
   - Founder-Merge ohne Creds: `crates/molt-engine/src/founding.rs` ~1713–1723
     (`cmd_net_mesh_ready` ruft `persist_mesh_crypto_blocking(Some(mls), None,
     mesh)` — das zweite Argument ist heute hart `None`).
   - Der periodische Cursor-Save des Storage-Writers schreibt nur Cursors und
     fügt nie Creds hinzu (`crates/molt-storage/src/lib.rs` ~1975–1994).
2. **Beim Reopen resümiert das echte Mesh nur, wenn `mls` UND `smp_queues` UND
   nicht-leeres `mesh` UND ein aufgelöster Dialer vorliegen**
   (`crates/molt-engine/src/session.rs` ~594–601); jedes Fehlen fällt still auf
   `ensure_demo_net()` durch (Produktions-No-Op) → Workspace öffnet **ganz ohne
   Transport**.
3. **Der Zustand ist unsichtbar:** die „detached"-Notice feuert nur, wenn ALLES
   fehlt (mit MLS wird sie geleert, session.rs ~614–623); `net_health` bleibt
   `Ok`; `chat_send` ackt lokal und sendet nichts. Ein hart gekillter Sitz ist
   außer über das Recovery-Ritual unrettbar, weil die Empfangs-Creds nur im
   Prozess-Speicher existierten.

---

## 2 Verifizierter Ist-Stand (erspart erneutes Suchen)

- **Alle Mesh-Up-Stellen im Engine-Code** (wo ein echtes Netz entsteht):
  1. Founder: `cmd_net_mesh_ready` (`founding.rs` ~1693–1744) — persistiert
     heute `mls + mesh` per `persist_mesh_crypto_blocking`, **Creds = None**.
  2. Joiner: `cmd_net_join_sealed` (`lifecycles.rs` ~1174–1191) — nimmt den
     Ritual-Transport aus `self.join_transport`, baut das Netz, **persistiert
     keine Creds** (materialize hat vorher mls+mesh geschrieben).
  3. Recovery: `cmd_net_recover_sealed` (`lifecycles.rs` ~1433–1450) — Zwilling
     des Joiners mit `self.recover_transport`, **persistiert keine Creds**.
  4. Mesh-Extension: `cmd_net_mesh_extended` (`net.rs` ~1422–1477) —
     **persistiert bereits korrekt** `mls + creds + mesh` über
     `crypto_for_close()` → hier ist NICHTS zu tun (Referenzmuster!).
  5. Reopen: `cmd_open_workspace` (`session.rs` ~551–626) mit
     `reopen_transport` (`founding.rs` ~44–53).
- **Der Storage-Writer ist schon korrekt:** `WriterMsg::SaveTransport` macht
  Read-Modify-Write und übernimmt NUR die Cursor-Maps; `MergeCrypto`
  (read-modify-write, fsync, optional `seal`) erhält Cursors. Beides ist durch
  bestehende Tests gepinnt: `merge_crypto_preserves_delivery_cursors` und
  `a_cursor_save_never_clobbers_a_live_crypto_merge` (molt-storage,
  `lib.rs`-Modultests). **In molt-storage ist KEINE Änderung nötig.**
- **`persist_mesh_crypto_blocking(mls, creds, mesh)`** ist der richtige
  Choke-Point für alle neuen Persists: LIVE-Merge (kein `seal`!), blockierend
  bis fsync, vom Aktor aus aufrufbar (gleiches Muster wie Mesh-Extension und
  Clean-Close). **Niemals `persist_crypto_blocking` für den Mesh-Up nehmen** —
  das setzt `seal=true` und würde alle späteren Cursor-Saves der laufenden
  Session verwerfen.
- **`LoopbackTransport::export_creds()` liefert heute `None`** (Trait-Default,
  `crates/molt-net/src/lib.rs` ~223–235), und `reopen_transport` liefert für
  ein Loopback-Mesh `None` (Servername „loopback" parst nicht als SMP-URL).
  Darum braucht Test (2) den Reopen-Seam (§4 S2).
- **GUI braucht KEINE Änderung:** `molt-ui/src/lib.rs:2212` matcht exakt
  `sv.notice == "detached"` → Toast; `:2233–2235` rendert `net_health` als
  Pill (rot bei `Down`, Reason als Tooltip). MCP liest `SessionView.notice` +
  `SessionView.net_health` unverändert mit. Kein neues `Command` → keine
  MCP-Co-Equality-Änderung.
- **`WalletHandle`-Drop modelliert den harten Kill korrekt:** alle
  Engine-internen Halter des Command-Kanals sind `WeakSender`
  (State/Sinks/Ticker/Brains); fällt der letzte starke Sender (das Handle),
  endet die Aktor-Schleife, der Writer-Thread endet und gibt das flock-LOCK
  frei — OHNE Clean-Close-Persist. Genau darauf bauen die Tests.
- `Transport`-Trait ist in `founding.rs` in Scope; in `lifecycles.rs` und
  `session.rs` die neuen Trait-Aufrufe **voll qualifiziert** schreiben
  (`molt_net::Transport::export_creds(&t)` / `…::import_creds(&t, creds)`),
  dann keine Import-Änderung nötig.

---

## 3 Design-Entscheidungen (festgezurrt)

- **D1 — Persist-Zeitpunkt:** „in dem Moment, in dem das echte Mesh steht" =
  die drei Aktor-Handler `cmd_net_mesh_ready` / `cmd_net_join_sealed` /
  `cmd_net_recover_sealed`. Früher (bei `materialize_workspace`) gibt es beim
  Founder noch keine Mesh-Queues; später wäre wieder ein Fenster offen.
- **D2 — Merge-Semantik:** immer der LIVE-Merge (`seal=false`). Ordnung ist
  sicher: alle transport.state-Schreiber laufen FIFO über den einen
  Writer-Thread; ein späterer Clean-Close überschreibt mls/creds mit neueren
  Werten (RMW), nie umgekehrt.
- **D3 — Loopback-Creds sind echt, kein Fake:** `LoopbackTransport` bekommt
  ein über Klone geteiltes Set der von ihm erzeugten Queue-Ids (das
  Loopback-Analogon zu SMPs Empfangs-Credentials; `SmpTransport` teilt seinen
  Zustand genauso über eine Arc). Export = Serialisierung dieser Ids; nur
  gültig, solange der Hub lebt (ein neuer PROZESS kann ein Loopback-Mesh nie
  resümieren — ein frischer Engine auf demselben Hub im Test schon). Founder-
  Export enthält auch die (toten) Stern-Queues des Rituals — unschädlich, der
  Supervisor subscribed nur die Mesh-Queues aus den Links.
- **D4 — Reopen-Seam:** `#[doc(hidden)] __spawn_with_reopen_transport(config,
  session, transport)` — ein Test-Seam exakt in der Tradition von
  `__spawn_manual_founding*`/`__spawn_demo_mesh`. Das Produkt installiert ihn
  nie; der echte Reopen baut weiterhin `reopen_transport` (SmpTransport +
  `import_creds`).
- **D5 — Ehrlicher Offline-Zustand:** Notice-Token bleibt exakt `"detached"`
  (GUI-Exact-Match, null GUI-Aufwand); die präzise Ursache trägt
  `net_health = Down { reason }` (Pill-Tooltip + MCP). Dialer-Fehlschläge
  behalten ihren eigenen fail-closed `Down`-Reason aus `resolve_dialer` und
  bekommen KEINE detached-Notice (Settings fixen + neu öffnen resümiert —
  der Workspace ist nicht detached).
- **D6 — Import-Detached bleibt unverändert:** der bestehende §4.4-Fall
  (Chain da, keinerlei Transport-Evidenz) behält sein Verhalten (Notice
  `"detached"`, net_health unberührt) — `restore_real.rs` pinnt ihn. Ihn auch
  auf `Down` zu heben ist ein optionales Follow-up, nicht Teil dieser Härtung.
- **D7 — Offline-first bleibt:** `chat_send` u. a. schreiben lokal weiter;
  nichts blockiert.

---

## 4 Implementierung Schritt für Schritt

**Arbeitsweise:** eigener Worktree (es EXISTIERT schon
`worktree-mesh-cred-persist` mit den roten Tests — dort weiterarbeiten oder
den Commit `d18e1e5` in einen frischen Worktree cherry-picken). NIE im
Haupt-Checkout committen; am Ende normaler Merge nach master (§7).

**Schritt 0 — Rot verifizieren.**
```
cargo test -p molt-engine --test mesh_resume --no-run
```
Erwartung @`d18e1e5`: genau EIN Fehler — `__spawn_with_reopen_transport` fehlt
(E0425). Nach S2 kompiliert alles; dann laufen lassen: Test (1) und (2) rot
(keine Creds auf Platte / kein Resume), Test (3) rot (net_health Ok statt
Down). Erst dann implementieren.

### S1 — molt-net: echte Loopback-Creds

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

- `LoopbackHub::transport()` initialisiert `created: Arc::new(Mutex::new(BTreeSet::new()))`.
- `create_queue` (Trait-Impl) registriert: nach `create_queue_blocking()`
  `self.created.lock()` → `insert(pair.rcv.id.0.clone())`.
- Trait-Impl ergänzen:

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

- Import `std::collections::BTreeSet` ergänzen; `hex`/`serde_json` sind in
  molt-net schon Dependencies.
- Vorher greppen: `LoopbackTransport {` wird NUR in `LoopbackHub::transport()`
  konstruiert — falls doch weitere Literale existieren, alle anpassen.
- Doc-Kommentar am Trait (`crates/molt-net/src/lib.rs`, `export_creds`)
  aktualisieren: der Halbsatz „`None` for … the loopback hub …" stimmt danach
  nicht mehr — Loopback exportiert jetzt seine Queue-Ids (prozess-lokal gültig).

### S2 — Engine-Seam: `__spawn_with_reopen_transport`

`crates/molt-engine/src/lib.rs`:

1. `State` bekommt ein Feld (bei den anderen Seam-Feldern, ~Z. 560):

```rust
/// Reopen **test seam only** ([`__spawn_with_reopen_transport`]): resume a
/// persisted mesh over THIS transport instead of building a fresh
/// `SmpTransport` from the mesh links. Lets the loopback tests drive a
/// literal hard-kill + reopen of a full engine (their hub survives in the
/// test, like a real SMP server would). The product never sets it.
pub(crate) reopen_seam: Option<founding::RitualTransport>,
```

   In `State::new` mit `reopen_seam: None` initialisieren.
2. `spawn_actor` bekommt einen 14. Parameter
   `reopen_seam: Option<founding::RitualTransport>` und setzt ihn wie die
   anderen Seams (`state.reopen_seam = reopen_seam;`). ALLE bestehenden
   Aufrufer (8 Stellen: `spawn_inner`, `__spawn_manual_founding`,
   `…_bootstrap`, `…_bootstrap_recoverable`, `__spawn_demo_mesh`,
   `__spawn_sim_founding`, `__spawn_manual_founding_over_smp`,
   `spawn_with_config`) übergeben `None`.
3. Neuer Spawner (bei den anderen `__spawn_*`):

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

   (Argument-Reihenfolge an die tatsächliche Signatur anpassen!)

### S3 — `cmd_open_workspace`: Seam nutzen + ehrlicher Offline-Zustand

`crates/molt-engine/src/session.rs`, den Block von `let dialer = …` (~Z. 594)
bis einschließlich der Notice-Zuweisung (~Z. 623) ERSETZEN durch:

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

(Der bestehende §4.4-Kommentarblock über der alten Notice-Zuweisung kann in
den neuen Code wandern; die beiden Bedingungen sind disjunkt — Evidenz
vs. keinerlei Evidenz.)

### S4 — Founder: Creds im Mesh-Ready-Merge

`crates/molt-engine/src/founding.rs`, `cmd_net_mesh_ready` (~Z. 1711–1732),
den Persist-/Aufbau-Teil ERSETZEN durch:

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

### S5 — Joiner: Creds beim Join-Sealed

`crates/molt-engine/src/lifecycles.rs`, `cmd_net_join_sealed` (~Z. 1179–1188),
den Netz-Aufbau-Block ERSETZEN durch:

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

### S6 — Recovery: Creds beim Recover-Sealed

Gleiche Änderung im Zwilling `cmd_net_recover_sealed` (~Z. 1437–1446), mit
`self.recover_transport` statt `join_transport` (der umgebende Code ist dort
bereits identisch strukturiert — nur den inneren `if let`-Block wie in S5 um
den Creds-Persist erweitern).

### S7 — Doku ehrlich halten

In **CLAUDE.md**, Abschnitt „Transport gotchas", den Satz „Only a CLEAN close
persists — …" anpassen: seit dieser Härtung persistieren Founder-Mesh-Ready,
Join-Sealed, Recover-Sealed und Mesh-Extension die Queue-Creds + MLS-Snapshot
**live beim Mesh-Up**; der Clean-Close-Merge schreibt weiterhin den
NEUESTEN Ratchet-Stand (ein harter Kill resümiert vom letzten persistierten
Ratchet — ein paar In-Flight-Nachrichten werden replay-rejected, akzeptiert).

---

## 5 Erwartete Test-Landschaft (Audit-Anleitung)

Nach S1–S6 zuerst `cargo test -p molt-engine --test mesh_resume` (grün),
dann die volle Suite. Mögliche Folge-Effekte, **einzeln prüfen, nicht
pauschal wegfixen**:

- Loopback-Workspaces, die in Tests **sauber geschlossen und wieder geöffnet**
  werden, haben jetzt Creds auf Platte; ohne Seam schlägt `reopen_transport`
  fehl („loopback" ist keine SMP-URL) → neu: `notice == "detached"` +
  `net_health Down` statt vorher stillem Nichts. Das ist der EHRLICHE Zustand
  — Assertions solcher Tests ggf. anpassen (oder den Seam installieren, wenn
  der Test ein echtes Resume meint). Kandidaten: `two_instances.rs`,
  `three_nodes.rs`, `founding.rs`, `chat_channels.rs`, `demo_mesh.rs`.
- `restore_real.rs` (Import-Detached, Zeilen 218/434) MUSS unverändert grün
  bleiben (D6). `persisted_mesh.rs` ist unbetroffen (dessen Engine-Workspace
  hat keinerlei Transport-Evidenz).
- Sim-Founding (`__spawn_sim_founding`, ohne Bootstrap) schreibt mls, aber
  weder Mesh noch Creds → Reopen zeigt jetzt Down+detached. Auch das ist
  ehrlich (Sitz ohne Links); betroffene Assertions anpassen.
- Die `#[ignore]`-Netztests (`ritual_engine_over_smp.rs`, `tor_e2e.rs`)
  NICHT lokal laufen lassen (echter SMP-Server; außerdem laufen beim User
  gerade drei moltd-Instanzen). Nichts an ihnen ändern.

**Clippy-Fallen:** kein `.unwrap()` (auch in Tests → `.expect("…")`);
`--all-targets` muss 0 Warnungen zeigen.

---

## 6 Validierung (Pflicht, in dieser Reihenfolge)

```
cargo test -p molt-engine --test mesh_resume        # neue Tests grün
cargo test -p molt-engine -p molt-storage -p molt-net
cargo clippy --all-targets                          # 0 Warnungen
```

- **KEIN** `molt-ui-window`-Build (keine GUI-/Strings-Änderung nötig; falls
  doch etwas in molt-ui angefasst wird: genau EIN `cargo build -p
  molt-ui-window -p molt-ui`, vorher prüfen, dass kein anderer Build läuft,
  bei knappem RAM `-j 1`).
- Worktree-Falle (Memory): geteilte `CARGO_TARGET_DIR` über Worktrees mischt
  molt-*-Artefakte — autoritative Läufe im Worktree-eigenen `target/`; bei
  Geister-Fehlern `cargo clean -p molt-…`.
- **Tabu:** die laufenden moltd-Instanzen des Users, `~/.moltrepublic*`,
  `config.toml`/`config2.toml`/`config3.toml` (untracked im Repo-Dir) —
  weder lesen-schreiben noch killen. Tests nur gegen Tempdirs.

---

## 7 Review + Landing

1. **Code-Review über den vollen Diff** (CLAUDE.md-Pflicht), Findings fixen.
   Review-Schwerpunkte: (a) kein neuer Persist benutzt versehentlich den
   sealenden `persist_crypto_blocking`; (b) `cmd_net_mesh_ready` persistiert
   auch dann unverändert (Creds `None`), wenn `runtime_transport` schon weg
   ist; (c) der Offline-Block setzt `Down` NIE bei `resumed_real`,
   `!persist` oder Dialer-Fehler; (d) Import-Pfad (`import.rs`) exportiert
   weiterhin nie Creds (bestehender Test Zeile ~616).
2. Frisches lokales master mergen (master kann sich bewegen — andere Session
   aktiv!), Suite + clippy erneut, dann im Haupt-Checkout
   `git -C /home/user/projects/moltrepublic merge <branch>` (normaler Merge,
   NIE push, NIE force, NIE History umschreiben). Bei nicht sicher lösbaren
   Konflikten: STOPPEN und berichten.
3. Worktree + Branch löschen (`git worktree remove …`, `git branch -d …`).
   Die übrigen `worktree-agent-*`-Worktrees gehören anderen Sessions — nicht
   anfassen. (Ein evtl. noch existierender Branch
   `worktree-settings-network-fixes` gehört ebenfalls einer anderen Session.)
4. Abschlussbericht: was wann wohin persistiert wird; Testnamen + was jeder
   pinnt; wie der Offline-Zustand erscheint (`notice="detached"`,
   `net_health=Down{reason}`); Review-Findings; master-Hash; Tests+clippy
   grün; **Stage B (Reconnect-Supervisor) bleibt offen**.

---

## 8 Bewusst offen (nach dieser Härtung)

- **Stage B:** Reconnect-/Resubscribe-Supervisor (laufende Verbindung
  überwachen, re-subscriben, Health live nachführen) — explizit NICHT Teil
  dieses Auftrags.
- Recovery-Link-Mint-Fenster: die dedizierte Recovery-Queue des Koordinators
  entsteht off-actor; ein harter Kill zwischen Mint und Mesh-Extension
  verliert sie (Link tot, neu minten). Klein, separat härtenswert.
- Assertion „Creds nach Engine-Join auf Platte" im `#[ignore]`-SMP-Test
  nachziehen (nur mit echtem Server ausführbar).
- Import-Detached zusätzlich auf `net_health Down` heben (D6-Follow-up).
- Per-Drain-MLS-Persist (volle Crash-Safety des Ratchets) — bekanntes
  offenes Hardening aus CLAUDE.md.

---

## Anhang A — die roten Tests (vollständig)

Liegt committet als `crates/molt-engine/tests/mesh_resume.rs` auf Branch
`worktree-mesh-cred-persist` (`d18e1e5`); falls Branch/Worktree verloren
gehen, die Datei 1:1 aus diesem Anhang wiederherstellen.

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
