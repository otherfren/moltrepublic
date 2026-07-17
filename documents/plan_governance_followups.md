# Plan: Governance-Follow-ups (Stand 2026-07-17, master `413cf33`)

Vom User beauftragte Reihenfolge: **WP1 → WP2 → WP3 → WP4**. Jedes WP ist
einzeln lieferbar (eigener Merge auf master); WP4 ist ein eigenes Projekt in
Etappen. Dieser Plan ist selbsttragend: eine frische Session kann ohne
weiteres Vorwissen bei „WP1, Schritt 1" anfangen.

Zeilenangaben gelten für `413cf33`; wo Code seither wandert, gilt das
danebenstehende grep-Muster.

---

## Arbeitsweise (gilt für alle WPs)

- **Workflow:** EnterWorktree → TDD → mergen auf master → push → Worktree +
  Branch löschen. Endzustand nach jedem WP: nur `master`, kein Side-Branch
  (CLAUDE.md-Regel; der User verlässt sich darauf).
- **TDD:** Red-Test zuerst, für den richtigen Grund fehlend sehen, dann grün.
- **Build-/RAM-Regeln:** GUI-Iteration nur über `scripts/dev-ui.sh build` und
  `SLINT_LIVE_PREVIEW=1 cargo test -p molt-ui --features live-preview`;
  der eine autoritative `cargo build -p molt-ui-window -p molt-ui` läuft
  einmal pro Change-Set vor dem Push, nie zwei Window-Builds parallel.
- **Cache-Aliasing-Falle:** Ein über Worktrees geteiltes `CARGO_TARGET_DIR`
  vermischt `molt-*`-Artefakte (Memory
  `worktree-shared-target-cache-aliasing`). Bei Geister-Compilefehlern
  („no field X" obwohl vorhanden): `cargo clean -p` der Workspace-Crates.
  Autoritative Läufe im Worktree-eigenen Target oder nach dem Merge im
  Haupt-Checkout.
- **Clippy 0** (`--all-targets`, `.expect` statt `.unwrap` in Tests);
  Co-Equality-Test in `crates/molt-mcp/src/lib.rs` bei jedem neuen
  `Command` (Netz/Ritual → INTERNAL-Liste, menschliche Verben → Tool).
- **Additive-only:** Neue `WorkspaceEvent`-/Snapshot-Felder mit
  `#[serde(default)]`; alte Leser dürfen bei Unbekanntem nie schreiben.

---

## WP1 — Applied-Einträge tragen ihre Proposal-Id; 💬 auch für akzeptierte Votes

**✅ ERLEDIGT (2026-07-17, master `f930801`).** Umgesetzt wie geplant
(Tupel-Vec, additive Dump-/Snapshot-Felder); zusätzlich entstand die neue
Org-Unteransicht „Accepted" (`("accepted", "Accepted")` in
`Surface::views`, hide-while-empty wie pending/declined), weil die
Organization vorher gar keine Applied-Liste in der GUI hatte; der
🗳-Rücksprung eines applied Org-Votes landet jetzt dort.

**User-Anforderung:** „Wenn ein Vote akzeptiert oder abgelehnt wird, soll die
Diskussion weiterhin verlinkt sein" — für *abgelehnte* Votes fertig
(Declined-Karten behalten 💬, Channel öffnet read-only); für *akzeptierte*
fehlt der Klickpunkt, weil Applied-Log-Einträge nackte Payloads ohne
Proposal-Id sind.

**Entschiedenes Design (additiv, kein Payload-Umbau):**
Die Applied-Payloads selbst bleiben byte-identisch (der UI-Fate-Probe-Cache
und MCP-Leser vergleichen Payloads!). Stattdessen bekommt der Read-Contract
eine **parallele Id-Spur**: `SurfaceSnapshot` erhält
`applied_ids: Vec<Option<u64>>` (`#[serde(default)]`), positionsgleich zu
`applied`. `None` = Herkunft unbekannt (Legacy-Dumps, Chat).

**Anker:**
- Engine-Projektionen: `crates/molt-engine/src/proposals.rs:551`
  `applied_values()` (legacy `self.applied` + `self.chain_applied` concat)
  und `:260` `try_apply()` (legacy-Pfad, kennt die Id beim Anwenden);
  Chain-Pfad: `crates/molt-engine/src/chain.rs:297–313` (Projektion in
  `chain_applied`) und `:578` `adopt_committed_block(block, proposal_id)` —
  die Id liegt dort bereits vor.
- Speicherform: `self.applied` / `self.chain_applied` sind
  `HashMap<Surface, Vec<Value>>` → auf `Vec<(Option<u64>, Value)>` erweitern
  ODER parallele `applied_ids`-Maps führen. **Empfehlung:** Tupel-Vec (eine
  Quelle, kein Drift); die bestehenden Reader (`applied_org_entries()`
  `proposals.rs:452`, `org_effective()`-Fold, Zähler `:799`) mappen auf
  `.1`.
- Persistenz: prüfen, ob `applied`/`chain_applied` in den State-Dump
  wandern (`events.rs:352 restore_dump` + `EngineStateDump`) — wenn ja,
  Dump-Feld additiv erweitern; alte Dumps ⇒ `None`-Ids.
- Snapshot: `SurfaceSnapshot` in `crates/molt-core/src/lib.rs` (grep
  `pub struct SurfaceSnapshot`), Befüllung in `snapshot()`
  (grep `fn snapshot` in molt-engine).
- UI: Gated-Surface-Logzeilen entstehen in `crates/molt-ui/src/lib.rs`
  (grep `LogLine` / `system:` Befüllung aus `s.log`); `LogLineData` bekommt
  `proposal_id: Option<u64>` → Slint `LogLine` ein `patch-id: int` (-1 =
  keins) + 💬-Affordance analog `ProposalCard.discuss` (`select-view("chat",
  "today")` + `select-channel("patch:<id>")`). Read-only-Zustand kommt
  danach automatisch (Engine-Annotation `ChannelInfo.state`, WP der
  Vorsession).
- MCP: `read_state`-Beschreibung um `applied_ids` ergänzen
  (`crates/molt-mcp/src/lib.rs:596ff`), kein Schema-/Command-Change.

**TDD (Namen als Vorgabe):**
1. `applied_entries_carry_their_proposal_id` (molt-engine, neben
   `archive_view_holds_...` in `lib.rs`-Tests): chain-governed Workspace,
   Proposal sealen → `snapshot().applied_ids[i] == Some(id)`,
   positionsgleich; Chat-Snapshot ⇒ alle `None`; Legacy-Dump-Restore ⇒
   `None`, Payloads unverändert.
2. `legacy_counted_applies_also_carry_ids` (falls der counted-Pfad noch
   testbar erreichbar ist — sonst im Test dokumentiert weglassen).
3. molt-ui-Unit-Test: Logzeile mit `proposal_id` ⇒ `patch-id` gesetzt,
   ohne ⇒ -1 (neben `vote_jump_targets_the_hosting_surface_and_fate_view`).
4. Bestehende Tests, die sich NICHT ändern dürfen: alle Payload-Gleichheits-
   Probes (`update_known_proposals`-Tests) — rot hier = Design verletzt.

**Risiken:** `WorkspaceEvent::Founded`/Dump-Ripple klein halten (kein neues
Founded-Feld nötig); `Vec<(Option<u64>, Value)>` ändert bincode-Dumps ⇒
Dump-Kompatibilität prüfen (Serde-Shape des Dumps additiv halten, sonst
Migration im `restore_dump`).

**Definition of Done:** Ein akzeptierter Org- UND Gated-Surface-Vote ist aus
seiner Applied-Zeile per 💬 wieder öffenbar (read-only, 🗳-Rücksprung im
Banner funktioniert); Testsuiten + Clippy 0 + autoritativer Window-Build;
`documents/chat_bus.md` um einen Satz zur Id-Spur ergänzt.

---

## WP2 — Pending-Proposals überleben den Neustart (Option b: Catch-up, keine Persistenz)

**✅ ERLEDIGT (2026-07-17, master `b82aa37`).** `serve_open_governance`
(chain.rs) beantwortet jeden `ChainRequest` zusätzlich mit den offenen
Proposals + gesammelten Signaturen (verbatim, positionsgebunden);
Idempotenz-Pins + E2E-Test über die echte Mesh
(`a_reopened_member_recovers_open_proposals_from_the_mesh`).
Membership-Proposals werden bewusst nicht re-gossipt.

**Symptom:** Ein über die Wire empfangenes `Proposed` lebt nur im Speicher
(`crates/molt-engine/src/net.rs:886–900`, `receive_proposed` ohne
`record`-Schritt). Reopen ⇒ Pending-Karte weg bis zum nächsten Gossip.

**Entschiedenes Design (Option b, vom User gewählt):** Kein Persistieren des
flüchtigen Gossips (Grenze „ephemeral bis zum Block" bleibt). Stattdessen
holt ein Reopen den offenen Zustand vom Mesh — auf der Schiene, die es
schon gibt:

- Beim Open ruft `crates/molt-engine/src/session.rs:426` bereits
  `request_catchup(height + 1)` (Chain-Blöcke). Der Serve-Pfad ist
  `WorkspaceEvent::ChainRequest { from_height }` → `serve_chain_from`
  (`net.rs:912`, `chain.rs:782ff`).
- **Erweiterung:** Wer einen `ChainRequest` beantwortet, sendet zusätzlich
  seinen offenen Governance-Zustand: pro offenem Proposal ein reguläres
  `Proposed`-Event erneut plus die schon gesammelten `Approved`-Events
  (aus `pending_sigs` — grep `pending_sigs` in `chain.rs`). Kein neues
  Event-Vokabular nötig: Re-Gossip identischer Events ist idempotent —
  `receive_proposed` / `receive_approval` müssen Duplikate bereits
  tolerieren (Test pinnt das). Falls Dedup fehlt: dort nachrüsten, nicht
  beim Sender.
- Signatur-Semantik unangetastet: `Approved` trägt `(id, by, height, sig)`
  positionsgebunden — Re-Gossip ändert nichts an
  `approval_bytes`/`molt-chain-change-v1`.
- Alternativ-Falle (NICHT tun): Pending in den Log/Dump schreiben — bricht
  die dokumentierte Ephemeral-Grenze (`documents/persistent_chain.md`).

**TDD:**
1. `a_reopened_member_recovers_open_proposals_from_the_mesh`
   (`crates/molt-engine/tests/two_instances.rs`, Muster
   `a_link_mint_without_a_running_mesh_...`): A proposet (kein Seal), B
   sieht pending → B close/reopen → B sieht das Proposal UND den
   Approval-Stand wieder; B kann approven, Block sealt bei m.
2. `regossiped_proposals_and_approvals_are_idempotent` (Unit, `chain.rs`):
   doppeltes `receive_proposed`/`receive_approval` ⇒ ein Eintrag, eine
   Signatur pro Member.
3. Randfall: Reopener ist der PROPOSER (sein eigenes Proposal war nur im
   RAM) — Catch-up der Peers bringt es zurück; Test-Assert ergänzen.

**Risiken:** `serve_chain_from` läuft bei jedem Catch-up — Re-Gossip-Volumen
begrenzen (nur offene Proposals; set_image-Payloads können ~256 KiB sein —
akzeptiert, es ist derselbe Chunker wie beim Erst-Gossip, per Test
`a_set_image_proposal_carries_its_bytes_across_the_mesh` gedeckt).
Mehrere Antworter ⇒ mehrfaches Re-Gossip ⇒ Idempotenz-Test ist der Anker.

**Definition of Done:** Reopen zeigt offene Votes samt Zählerstand ohne
User-Zutun; keine neue Persistenz; `persistent_chain.md` um einen
Catch-up-Absatz ergänzt (Stil der bestehenden Phase-3-Notizen).

---

## WP3 — set_image-Proposals validieren die Decodierbarkeit beim Proposen

**✅ ERLEDIGT (2026-07-17).** `image_decodable` in molt-engine
(Format-Sniff + Header-Dimensionen ≤ 8192², SVG-Prefix; nie Voll-Decode),
eingehängt in `validate_org_payload` + Wire-Guard; GUI-Pre-Check mit dem
echten Preview-Decoder (lokalisierter Toast); MCP-`propose`-Beschreibung
ergänzt. Der Mesh-Bytes-Test nutzt jetzt ein echtes 147-KiB-BMP.

**Symptom:** Eine defekte/exotische Bilddatei wird anstandslos proposet;
jeder Voter bekommt nur den `pc_img_missing`-Toast. Sign-what-you-see läuft
leer.

**Entschiedenes Design (co-equal = Engine validiert, GUI meldet schön):**
- **Engine** (`crates/molt-engine/src/proposals.rs:122`-Umgebung,
  `validate_org_payload`, plus Wire-Guard `net.rs:886–897`): zusätzlich zur
  Byte-Cap ein **billiger Format-Sniff ohne Voll-Decode** (Decode-Bomben!):
  `image::guess_format(bytes)` + `image::io::Reader::into_dimensions()`
  (liest nur Header) mit demselben 8192²-Cap wie die Preview; SVG-Zweig:
  Prefix-Sniff (`<svg`/`<?xml`). Fehlschlag ⇒ ehrlicher `MoltError`
  („the image cannot be decoded (png/jpeg/webp/gif/bmp/svg)") lokal, Drop
  mit `tracing::warn` auf der Wire (Konvergenz vor Enforcement, wie der
  Cap-Guard).
- Neue Dependency `image` in molt-engine: gleiche Version/Features wie
  molt-ui (`0.25`, `default-features = false`, png/jpeg/webp/gif/bmp —
  pure-Rust-Decoder, Posture bleibt).
- **GUI** (`crates/molt-ui/src/lib.rs:904–956`, `on_org_propose`
  set_image-Zweig): denselben Check VOR dem `Command::Propose` ausführen
  (sofortiger, lokalisierter Toast statt Engine-Fehlerlauf); Helfer neben
  `image_from_bytes` (`lib.rs:1288`) als `fn image_decodable(bytes) -> bool`
  herausziehen und von beiden Seiten… NEIN — Engine und UI sind getrennte
  Crates: der Engine-Check lebt in molt-engine (`proposals.rs`), die UI
  nutzt ihren bestehenden Decoder; Doppelung ist hier gewollt (UI rendert
  wirklich, Engine sniffed nur). Kommentar an beiden Stellen verweist auf
  die jeweils andere.
- MCP: kein Schema-Change; `propose`-Tool-Beschreibung um den
  Decodierbarkeits-Satz ergänzen.

**TDD:**
1. `an_undecodable_set_image_proposal_is_refused` (molt-engine, neben dem
   bestehenden Cap-Test `lib.rs:2232`-Umgebung): Garbage-Bytes ⇒ Fehler;
   2×2-PNG/WebP/SVG-Fixtures (aus dem Preview-Fix übernehmen, grep
   `a_proposed_image_decodes_from_the_payload_for_every_picker_format` in
   molt-ui) ⇒ akzeptiert.
2. `an_undecodable_peer_set_image_is_dropped_not_recorded` (Wire-Guard,
   Muster des Cap-Drop-Tests).
3. Dimension-Bombe: 20000×20000-PNG-Header ⇒ refused (Header-Fixture
   generieren, kein echtes Riesenbild).

**Definition of Done:** Defektes Bild ⇒ sofortiger verständlicher Fehler
beim Proposer (beide Frontends), nie ein Pending-Proposal; Peers droppen
konsistent; Tests + Clippy + Window-Build grün.

---

## WP4 — Physisches Pruning (Log-Kompaktierung, „weg ist weg") — zum Schluss, in Etappen

**Auftrag:** Abgelaufenes verschwindet heute nur aus den Reads
(`chat_view_admits`/`aged_out_at`); Log-Einträge und `shared_files`-Pfade
bleiben auf der Platte. Ziel: nach Ablauf + Karenz ist der Inhalt lokal
wirklich gelöscht. Referenz: `documents/chat_bus.md:266–292` (Compaction-
Bullet + Uploads-Bullet — die dort genannten Constraints sind der Vertrag).

**Harte Constraints (nicht verhandelbar):**
- **Synthetische Legacy-Ids** hängen an der Ableitung
  `sha256("molt-chat-legacy-id\0" ‖ …)` über Positionen/`seq`
  (`crates/molt-engine/src/events.rs:30`, `:768ff`, beide Choke-Points
  `apply`-Chat-Arm + `restore_dump`): Kompaktierung darf `seq` NIE
  renumerieren — Einträge fallen weg, Lücken bleiben.
- **Replay-Floor + Outbox-Cursor** (`net.rs:267ff` log-backed outbox):
  ein Peer-Cursor darf nie auf gelöschtes Terrain zeigen; Cursor <
  Kompaktierungs-Floor ⇒ der Peer wird auf Chain-Catch-up (WP2-Schiene)
  statt Log-Replay umgeleitet.
- **Chain-Blöcke sind heilig:** Kompaktiert wird nur der flüchtige
  Event-Log (Chat/Shares); Blöcke, Genesis, Roster niemals.
- **User-Dateien sind tabu:** Pruning eines Shares löscht den
  `prefs.shared_files`-Eintrag (`chat.rs:327ff`, `session.rs:439`) — nie
  die Quelldatei des Teilenden, nie heruntergeladene Kopien.
- **Deterministisch:** Kompaktierung ist eine lokale Hygiene-Operation,
  KEIN konvergenzrelevantes Ereignis — nichts davon kreuzt die Wire.

**Etappe 1 begonnen (2026-07-17):** `documents/log_compaction.md` liegt als
ENTWURF ZUR DISKUSSION auf master (Kernidee: Kompaktierung =
Snapshot-Vorziehen + Segment-Drop, kein Byte-Rewrite; offene Fragen F1–F5
in §8). **Halt: erst nach der Diskussion mit dem User beginnt Etappe 2.**

**Etappen (jede einzeln mergebar):**
1. **Analyse-Etappe (Design-Doc, discuss-before-push!):**
   `documents/log_compaction.md` — Segment-/Floor-Modell, Cursor-Umleitung,
   Ablauf+Karenz (Vorschlag: Karenz = 1× Retention-Fenster, damit
   Boundary-Races der Reads nie sichtbar werden), Crash-Sicherheit
   (Kompaktierung als Copy-then-Swap auf Storage-Ebene, molt-storage).
   **Halt: Dieses Doc dem User zur Diskussion geben, bevor Code entsteht**
   (Memory `feedback-concept-docs-discuss-before-push`).
2. **Storage-Primitiv:** `molt-storage` bekommt atomare
   Segment-Rewrite-Operation (Tempfile + rename), Test: Crash zwischen
   Copy und Swap ⇒ alter Stand intakt.
3. **Engine-Kompaktor:** Ticker-getrieben (Muster der bestehenden Ticker,
   Engine-Actor: sync Handler, Off-Actor-Task → internes Command, INTERNAL-
   Liste!): markiert abgelaufene Chat-/Share-Einträge unterhalb des Floors,
   schreibt das Segment neu (seq-stabil), hebt den Replay-Floor, leitet
   veraltete Cursor um. Tests: Ids stabil (Byte-Fixtures der Chat-Surface
   in molt-core MÜSSEN grün bleiben — rot = Design-Stopp), Reads identisch
   vor/nach Kompaktierung, Peer mit altem Cursor konvergiert via Catch-up.
4. **Share-Vergessen:** abgelaufene `shared_files`-Einträge fallen beim
   Kompaktieren mit; Download-Anfrage danach ⇒ vorhandenes ehrliches
   `Refused` (Test existiert sinngemäß, erweitern).
5. **Doku-Abschluss:** `chat_bus.md`-Follow-up-Bullets als erledigt
   umschreiben, `plan_governance_followups.md` (dieses Doc) löschen oder
   als erledigt markieren.

**Definition of Done (WP4 gesamt):** Nach Ablauf + Karenz existieren Inhalt
und Metadaten abgelaufener Chats/Shares lokal nicht mehr; alle
Determinismus-Keystones (Legacy-Id-Fixtures, verify_chain, Konvergenz-
Tests) grün; kein Wire-Verhalten geändert.

---

## Abschluss-Checkliste (nach jedem WP)

1. `cargo test -p molt-core -p molt-engine -p molt-mcp` grün (Worktree).
2. GUI berührt? `scripts/dev-ui.sh build` + molt-ui-Tests/Clippy
   (live-preview) grün.
3. `cargo clippy -p molt-core -p molt-config -p molt-storage -p molt-net
   -p molt-engine -p molt-mcp --all-targets` = 0.
4. Merge auf master im Haupt-Checkout → dort `cargo build -p molt-ui-window
   -p molt-ui` (autoritativ) + `cargo build` + `cargo test -p molt-engine
   --lib` → push.
5. Worktree + Branch löschen; `git branch -a` zeigt nur master.
6. Memory-Index aktuell halten (dieses Doc ist dort verlinkt).
