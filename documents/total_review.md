# Gesamtprojekt-Review (2026-07-18)

Sieben parallele Auditor-Agenten über alle Crates: `molt-core`, `molt-config`,
`molt-storage`, `molt-net`, `molt-engine` (2×), `molt-mcp`/`molt-app`,
`molt-ui`+Slint. Dieses Dokument listet die **negativen Befunde zuerst, nach
Schweregrad sortiert**. Jeder Eintrag ist als **✅ GEFIXT** (in diesem Review),
**📋 DOKUMENTIERT** (bewusst zurückgestellt — Aufwand/Design/Risiko, hier für
später festgehalten) oder **ℹ️ AKZEPTIERT** (kein echter Defekt / durch eine
höhere Schicht abgedeckt) markiert.

Stand nach den Fixes dieses Reviews: alle Tests grün (Core/Net/Storage/Engine/
MCP: 305; molt-ui: 37), Clippy 0, Slint-Build sauber. Neue Security-Tests:
`a_membership_proposal_cannot_hijack_a_colliding_surface_id`,
`malicious_checkpoint_heights_are_refused_not_panics`,
`serde_json_object_serializes_with_sorted_keys`.

---

## CRITICAL

### C1 — Membership-Proposal-Id-Kollision kapert Approvals (Governance-Bypass) ✅ GEFIXT
`crates/molt-engine/src/chain.rs` `receive_membership_proposal`.
`entry(id).or_insert(...)` ohne Kollisionsprüfung: ein böswilliges Rostermitglied
gossipt `MembershipProposed{op:Joined, member:mallory, id:X}`, wobei `X` eine
lokale Surface-Proposal-Id ist, die alle ehrlichen Nodes gerade approven wollen.
Da `proposal_change(id)` zuerst `proposal_changes` auflöst, signiert jedes
ehrliche Approve dieser Surface-Proposal die **Membership**-Bytes — bei *m*
Signaturen sealt ein `Membership{Joined}`-Block und `mallory` (mit
Angreifer-Key) landet im Roster, ohne dass je ein Mensch eine Mitgliedschaft
bestätigt hat. Exakt derselbe Forge, gegen den der Checkpoint-Arm in WP4b-Etappe
3 gehärtet wurde — auf dem älteren Membership-Arm nie angewandt.
**Fix:** gemeinsamer Guard `id_free_for(id, change)` (belegte Id ⇒ nur die
identische Change erlaubt) auf `receive_membership_proposal`, `receive_proposed`
(symmetrisch: eine Surface-Proposal darf keine Chain-Change überschatten) und
`receive_checkpoint_proposal`. Gepinnt durch Test.

---

## HIGH

### H1 — Wire-`Approved` mit gefälschter `height` friert Proposals dauerhaft ein ✅ GEFIXT
`crates/molt-engine/src/chain.rs` `receive_approval`/`collect_sig`. Ein Mitglied
gossipt `Approved{height:u64::MAX, sig:beliebig}`; `collect_sig` übernimmt die
höhere Höhe, **löscht alle echten Signaturen** und blockt danach jede
legitime Approval (`height < entry.height` ⇒ return). `try_commit` verlangt
`pending.height == head+1`, sealt also nie; `rebase_pending_approvals` räumt nur
Höhen *unter* dem Target — eine MAX-Höhe nie. Ein Mitglied friert damit jede
Abstimmung permanent ein (Liveness-DoS bis Neustart).
**Fix:** `receive_approval` verwirft eine Höhe > `head+1` (die einzige, für die
eine echte Approval existieren kann) vor `collect_sig`.

### H2 — Height-Arithmetik-Underflow/Overflow auf Angreiferdaten → Prozess-Abort ✅ GEFIXT
`crates/molt-engine/src/chain.rs` `verify_suffix_chain` (`anchor.height - 1`),
`verify_next` Checkpoint-Arm (`block.height - 1`), `try_adopt_from_blob`
(`blob.upto + 1`, `h += 1`). `overflow-checks = true` gilt auch für den
Release-Build ⇒ jeder Underflow/Overflow ist ein **Abort**, nicht ein Wrap. Ein
böswilliger Koordinator (Recovery) oder Insider (Catch-up `CheckpointServed{
blob.upto = u64::MAX}`) crasht den Empfänger.
**Fix:** `checked_sub`/`checked_add`, height-0-Anker/-Checkpoints explizit
abgelehnt, `h += 1` bricht bei Overflow ab. Gepinnt durch Test.

### H3 — Storage ackt „durable" auch nach fehlgeschlagenem Write 📋 DOKUMENTIERT
`crates/molt-storage/src/lib.rs` `PersistChain`/`MergeCrypto`-Arme (~Z.1916).
Bei Schreibfehler wird nur das `failed`-Flag gesetzt, aber `ack.send(())`
trotzdem gefeuert. `persist_chain_blocking` verspricht „blockt bis durable" —
die Engine broadcastet also einen threshold-signierten Block (oder schließt
clean im Glauben, die MLS-Ratchet-Snapshot sei persistiert), während nichts auf
der Platte liegt.
**Zurückgestellt**, weil der saubere Fix die Ack-Kanal-Signatur (`SyncSender<()>`
→ Erfolg tragen) und alle Aufrufer betrifft — ein Ripple, den ich nicht im
Review-Rahmen risikofrei mache. **Richtung:** Ack ein `Result`/`bool` tragen
lassen, `persist_chain_blocking`/`merge_crypto_blocking` den Fehler
zurückgeben, der Governance-Broadcast erst nach bestätigter Persistenz.

### H4 — Manifest-Version-Bump lag HINTER dem pruned Chain-Write ✅ GEFIXT
`crates/molt-storage/src/lib.rs` `PersistChain`-Arm. Der WP4b-Bump lief nach dem
`write_chain(Pruned)`: ein Crash dazwischen (oder ein Bump-Fehlschlag) ließ ein
pruned `chain.state` unter der ALTEN Manifest-Version liegen — ein altes Binary
(z. B. ein zweites Gerät auf einem synchronisierten Ordner) passiert das Gate,
decodiert die Objekt-Form nicht und läuft **chainlos** auf einem
governance-regierten Workspace (Fork).
**Fix:** Bump VOR dem pruned Write. Diese Richtung ist strikt sicher — ein Crash
danach *über*beschreibt nur (altes Binary verweigert einen noch-vollständigen
Workspace: Verfügbarkeitsverlust, kein Fork).

### H5 — MCP `save_settings` überschreibt weggelassene Felder mit Defaults 📋 DOKUMENTIERT
`crates/molt-mcp/src/lib.rs` `settings_arg`/`cmd_save_settings`. Ein
Teilaufruf `{"headless":true}` füllt den Rest aus `SessionSettings::default()`:
`mcp_token=""` (TCP-Auth aus), `anonymity="none"` (ein Tor-Node fällt still auf
Clearnet — Deanonymisierung), `mcp_allow` zurückgesetzt, S3-Creds/`smp_url`
gelöscht — und `persist_settings` macht es **dauerhaft** in `config.toml`.
**Zurückgestellt**, weil der Fix den Handler die AKTUELLE Session als Merge-Basis
statt `default()` nehmen lassen muss (der Handler baut heute aus `args` mit
`d=default()`). Machbar, aber ein Verhaltenswechsel der MCP-Semantik, den ich mit
dem User abstimmen würde. **Richtung:** `settings_arg` bekommt die laufenden
`SessionSettings` als Default-Basis; ein weggelassenes Feld behält den
Ist-Wert.

### H6 — GUI-Sound-Alerts: Zombie-Prozesse + Spawn-Storm + UI-Thread-Blocking ✅ GEFIXT
`crates/molt-ui/src/lib.rs` `play_alert`. Pro Alert ein `spawn()` ohne `wait()`
(Zombies bis Prozessende); die Engine emittiert `Event::Chat` pro
Mesh-Nachricht und `Event::Proposed` auch für re-gossipte Duplikate — ein
Reconnect-Catch-up von hunderten Nachrichten spawnt hunderte `pw-play` (Kakofonie
+ PID-Erschöpfung). Zudem lief die erste WAV-Synthese + der Spawn auf dem
Slint-UI-Thread (Chat-Arm) bzw. einem Tokio-Worker (Vote-Arm).
**Fix:** 400-ms-Debounce (ein Burst spielt einmal), alle Arbeit auf einem
Detached-Thread, der den Player mit `wait()` reapt; per-Prozess-`pid`-Pfad statt
weltweit teilbarem `/tmp`-Namen.

---

## MEDIUM

### M1 — Reassembler prä-allokiert aus dem 16-bit `count` des ersten Chunks ✅ GEFIXT
`crates/molt-net/src/chunk.rs`. `vec![None; count]` mit `count` bis 65535 (~1,5 MB
pro Partial) — ein authentifizierter Peer sendet 64 Ein-Chunk-Nachrichten mit
`count=65535`: ~1 MiB Wire pinnt ~100 MiB RAM.
**Fix:** `chunks` ist jetzt eine sparse `BTreeMap<u16, Vec<u8>>` — der Speicher
wächst mit tatsächlich empfangenen Chunks, nie mit dem behaupteten `count`.

### M2 — serde_json-`preserve_order`-Determinismus war NICHT durch Test gepinnt ✅ GEFIXT
`crates/molt-core/src/chain.rs`. Die kanonischen Byte-Kommentare versprachen
„pinned by test", ein solcher Test existierte nirgends. Aktiviert irgendeine
Crate im Build-Graph `serde_json/preserve_order` (Feature-Unification), wird
`Value::Object` insertion-geordnet — zwei Nodes serialisieren dieselbe Payload
verschieden ⇒ `approval_bytes`/`checkpoint_canonical_bytes` divergieren, alle
Signaturen/Checkpoints brechen still.
**Fix:** `serde_json_object_serializes_with_sorted_keys` pinnt sortierte Keys;
der falsche Kommentar zeigt jetzt auf diesen Test.

### M3 — GUI „S3-Verbindung testen" täuscht Erfolg vor ✅ GEFIXT
`crates/molt-ui-window/ui/app.slint` (~Z.5071). Der Button zeigt bedingungslos
„S3-Verbindung OK (Mock)" ohne jeden Test — verletzt „never fake behavior".
**Fix:** ehrlicher Toast „S3-Backup ist noch nicht angebunden — es wurde nichts
getestet." (das S3-Backend selbst bleibt die bekannte deferred-Arbeit).

### M4 — SMP ackt eager, während der Supervisor auf Redelivery gehaltener Acks baut 📋 DOKUMENTIERT
`crates/molt-net/src/smp/conn.rs`/`transport.rs`. `recv_next` ackt Nachricht N zum
Server, sobald N+1 geholt wird — bevor der Supervisor entscheidet zu puffern.
Eine im Epoch-/Reorder-Puffer gehaltene Nachricht (Re-Key-Storm → shed) oder ein
Block zwischen Server-Ack und Engine-fsync ist bei Crash **permanent verloren**
(der Server liefert Geacktes nicht neu). Loopback fängt es nicht (nur unacked
Redelivery). Die „loopback kann's nicht fangen"-Klasse aus CLAUDE.md.
**Zurückgestellt** — tiefes Transport-Design (die Ack-Disziplin müsste erst beim
tatsächlichen Buffer/Accept greifen, nicht beim Prefetch). Braucht eigene
Session + einen SMP-Integrationstest.

### M5 — Alte transport.state/log werden von neueren Versionen still geklobbert 📋 DOKUMENTIERT
`crates/molt-storage/src/lib.rs` `read_transport_state` (Z.930): Version >
`TRANSPORT_STATE_VERSION` ⇒ `default()`, und der `SaveTransport`-RMW schreibt die
neuere Datei mit der alten Version zurück — MLS-Snapshot + SMP-Queue-Creds weg,
Mesh-Resume dauerhaft kaputt (SMP verbietet Subscribe auf fremd-erzeugte
Queues). Verletzt die Additive-Only-Regel „ein älterer Leser darf bei Unbekanntem
nicht schreiben".
**Zurückgestellt** — die saubere Lösung (bei neuerer Version das Öffnen
verweigern statt Default zu schreiben) ist dieselbe Klasse wie H4, aber für
transport.state; braucht dieselbe „refuse newer"-Semantik konsistent über alle
State-Dateien.

### M6 — Torn-Tail-Recovery truncatet beim ERSTEN kaputten Frame ⇒ Datenverlust 📋 DOKUMENTIERT
`crates/molt-storage/src/lib.rs` (~Z.1257). Ein einzelner Bitflip früh im letzten
(bis 8 MiB) Segment löscht alle validen, bereits geackten Frames dahinter — im
Extremfall den ganzen Log inkl. Genesis, und `open_workspace` „gelingt" mit
leerer Historie. Mittlere Segmente bekommen den konservativen Hard-Error, das
letzte nie.
**Zurückgestellt** — braucht eine „folgt ein valider Frame nach dem Schaden?"-
Prüfung (Torn-Tail vs. Bitrot unterscheiden) vor der destruktiven Truncation.

### M7 — MCP `serve_conn`: unbegrenztes `read_line` vor Auth (Pre-Auth-DoS) 📋 DOKUMENTIERT
`crates/molt-mcp/src/lib.rs` (~Z.95). Auf einem `0.0.0.0`-Allow-Node streamt ein
Client, der nur den IP-Filter passiert, GB ohne Newline; `read_line` wächst
unbegrenzt → OOM.
**Zurückgestellt** (billiger Fix möglich): begrenzter Reader (`take(MAX)` je
Zeile) vor dem Parse. Nicht im Security-kritischen Governance-Kern, daher hier
notiert.

### M8 — `random_token()` liefert still `""` bei RNG-Fehler/Non-Unix 📋 DOKUMENTIERT
`crates/molt-config/src/lib.rs` (~Z.341). `--generate-config` in einer Sandbox
ohne `/dev/urandom` (oder Non-Linux) schreibt `token = ""` und meldet Erfolg —
der Node akzeptiert dann jede MCP-Verbindung ohne Token.
**Zurückgestellt** (billiger Fix): `getrandom` statt Pfad-Open, harter Abbruch
statt leerem Token.

### M9 — MCP-Token wird zur Laufzeit nicht neu geladen 📋 DOKUMENTIERT
`crates/molt-mcp/src/lib.rs` (~Z.67). `serve_tcp` fängt das Token beim Start ein;
eine Rotation wirkt erst nach Neustart — entgegen mcp-security.md „takes effect
immediately". Ein geleaktes Token bleibt gültig, das neue nicht.
**Zurückgestellt** — braucht ein geteiltes, live-lesbares Token-Handle im
Accept-Loop.

### M10 — Chat-Archiv-Pager: Clamp nutzt `applied.len()`, Slice nutzt `log.len()` 📋 DOKUMENTIERT
`crates/molt-ui/src/lib.rs` (~Z.2678). Der Clamp begrenzt die gespeicherte Seite
mit der rohen Applied-Zahl, der Pager/Slice paginiert aber über die projizierte
Log-Länge (inkl. gemergter System-Zeilen in Patch-Kanälen). In einem
Patch-Kanal-Archiv mit System-Zeilen ist die letzte Seite unerreichbar
(„›" springt zurück).
**Zurückgestellt** — der saubere Fix verlangt die projizierte `log.len()` am
Clamp-Punkt, der aber VOR der Projektion in der State-Closure liegt (Closure/
apply_surfaces-Split). Enger Randfall; präzise notiert statt fragil geflickt.

### M11 — Legacy-`anonymity="nym"` erzeugt Phantom-„ungespeichert" im Settings-Screen 📋 DOKUMENTIERT
`crates/molt-ui/src/lib.rs` (~Z.4166). Nach dem nym-Entfernen lädt eine
Alt-Config `anonymity="nym"` als Dropdown-Index „none", `read_settings_draft`
gibt „none" zurück, der Ist-Wert bleibt aber „nym" ⇒ Draft ≠ Stored für immer:
die Unsaved-Modal erscheint bei jedem Verlassen, externe Settings-Änderungen
werden nicht mehr gespiegelt, ein Save schreibt still „nym"→„none".
**Zurückgestellt** — sauber wäre eine Normalisierung „nym"→„none" an EINER
Stelle (molt-config-Parse), damit „nym" nie in den State kommt; Cross-Layer,
daher notiert.

### M12 — Optimistische Erfolgs-Toasts vor dem Engine-Effekt 📋 DOKUMENTIERT
`crates/molt-ui-window/ui/app.slint` (org-propose-Sites, file-remove, copy).
Mehrere Buttons toasten „Vorgeschlagen"/„Gelöscht"/„Kopiert" beim Klick, bevor
das Command läuft — der `set_image`-Pfad kann danach noch am Decode/Cap
scheitern, der User sieht Erfolg dann Fehler für eine Proposal, die nie
existierte.
**Zurückgestellt** — ein breites UI-Muster (viele Sites); der saubere Umbau
(Toast erst im Erfolgs-Callback) ist ein eigener GUI-Sweep. Der schädlichste
Fall (`set_image`) hat bereits den Pre-Decode-Check aus WP3, der die meisten
Fehlschläge vor dem Toast abfängt.

### M13 — `roster_canonical_bytes` schreibt `ws_id` ungeframt (nicht-injektiv) 📋 DOKUMENTIERT
`crates/molt-core/src/lib.rs` (~Z.1309). Anders als `approval_bytes` (das
`republic_id` längenpräfixiert) schreibt `roster_canonical_bytes` die `ws_id`
ohne Längenpräfix — zwei verschiedene Roster-Tabellen können durch Verschieben
der `ws_id`/`rule_m`-Grenze identische signierte Bytes ergeben.
**ℹ️ Heute abgesichert** durch die höhere Invariante (Verifier rekomputieren die
fixe-Länge content-derived `republic_id`), also keine aktive Lücke — aber die
Funktion liefert die in ihrem Vertrag behauptete Injektivität nicht.
**Zurückgestellt** — der Fix (ws_id framen) bräuchte einen `molt-roster-v3`-Tag
an allen Recompute-Sites gleichzeitig.

---

## LOW (dokumentiert, nicht gefixt)

- **L1 — SVG „billion laughs" umgeht die Dimensions-Prüfung.**
  `proposals.rs` `image_decodable`: ein `<svg`/`<?xml`-Prefix gibt sofort `Ok`,
  ohne Struktur-/Dimensions-Check — nur der 256-KiB-Cap greift. Ein feindliches
  256-KiB-„SVG" (verschachtelte `use`, riesige `viewBox`) erreicht jeden
  GUI-Renderer. Richtung: SVG strukturell begrenzen (Größe/Verschachtelung)
  oder SVG-Proposals ablehnen.
- **L2 — Angezeigte Approval-Zahl zählt unverifizierte Signaturen.**
  `proposals.rs` `chain_approval_count`/Vote-Row lesen `pending_sigs` roh (erst
  `try_commit` verifiziert). Ein Peer kann die *angezeigte* Zustimmung fälschen
  (Display-Integrität; das Sealing bleibt sicher).
- **L3 — Unbegrenzte `pending_blocks`/`pending_sigs`/`proposals` aus der Wire.**
  `chain.rs` puffert future-height-Blöcke und id-adressierte Sigs/Proposals ohne
  Cap — ein Insider kann langsam OOMen. Richtung: gedeckelte Puffer mit Eviction
  (Vorsicht: Liveness). Die Underflow/Freeze-Vektoren (C1/H1/H2) sind gefixt; die
  reine Speichergrenze bleibt offen.
- **L4 — Ed448-Verify nicht strikt kanonisch (S-Malleability).**
  `smp/ed448.rs`: prüft nur `S`-Top-Byte, nicht `S < L`. Benign für den
  Cert-Pin-Zweck (Malleability gibt kein MITM), aber Abweichung von RFC 8032.
- **L5 — `parse_mcp_allow` bindet `0.0.0.0` bei unparsbarem Allow-String.**
  `molt-app/main.rs`: ein Tippfehler wie `allow="localhost"` öffnet den Socket
  auf allen Interfaces (Verbindungen werden fail-closed abgelehnt, aber der Port
  ist offen). Richtung: Fallback auf Loopback statt `0.0.0.0`.
- **L6 — `dir_size` folgt Symlinks ohne Zyklus-/Tiefen-Schutz.**
  `molt-storage`: eine Symlink-Schleife im Workspace-Dir crasht den Open-Scan
  per Stack-Overflow.
- **L7 — `read_chain` verwechselt beschädigt mit abwesend.**
  Beide ⇒ `(None, vec![])`; ein Aufrufer kann „unlesbare Historie" nicht von
  „keine Chain" unterscheiden (Additive-Only-Guard nicht durchsetzbar).
- **L8 — Ganze Dateien werden ungedeckelt `fs::read`.** `open_workspace` u. a.
  slurpen die ganze Datei vor jeder Validierung — eine multi-GiB-korrupte
  Segment-/State-Datei OOMt den Open-Pfad.
- **L9 — Diverse UI-Erfolgs-Toasts ohne Erfolgsprüfung** (copy_text ignoriert
  Clipboard-Fehler, file-remove/retention analog) — siehe M12.
- **L10 — `set_chat_retention`-Value bakt englisches „N days" in die Payload.**
  Die Ist/Soll-Zeile zeigt in DE „7 days → 30 days". Der Titel ist
  sprachneutral (op-Platzhalter), der Value nicht. Richtung: den Wert als reine
  Zahl tragen, die Einheit beim Rendern lokalisieren.
- **L11 — Poisoned-Mutex-`.expect()` im Live-Mirror-Task** kann nach einem
  Callback-Panic alle GUI-Updates still stoppen (andere Lock-Sites behandeln
  Poison bereits tolerant).
- **L12 — `pub mod mockrand` in der RNG-freien `molt-core`** — heute rein
  deterministische Demo-Helfer, aber die Platzierung lädt zu Fehlnutzung ein
  (Historie: `mock_ticket` leitete daraus mal Tickets ab).
- **L13 — `mls.rs` `store_pending_proposal` speichert verworfene Bare-Proposals**
  — ein kompromittiertes Mitglied kann MLS-Provider-State langsam wachsen
  lassen.
- **L14 — `checkpoint_canonical_bytes` ohne Gruppen-Count / `Surface::ALL` nicht
  im Versions-Tag** — ein künftiger 7. Surface ändert die Bytes ohne Tag-Bump ⇒
  Mixed-Version-Divergenz. Richtung: Applied-Sektion zählen, Tag bei jeder
  Surface-Änderung bumpen.
- **L15 — `put_bytes`/`to_vec` mappen Overflow/Fehler auf Länge 0** statt zu
  panicken — ein >4-GiB-Feld oder eine fehlgeschlagene Serialisierung würde in
  den signierten Bytes zu mehrdeutigem Framing. Über SMP-Limits unerreichbar,
  aber ein Silent-Wrong-Bytes-Pfad.

---

## ℹ️ Geprüft und in Ordnung (keine Aktion)

- XChaCha20-Nonces sind pro Frame frisch-zufällig (24 Byte) — die
  In-Place-Rewrites von `chain.state`/`transport.state`/Snapshots bei fixem AAD
  `(SEGMENT, 0)` sind **kein** Nonce-Reuse.
- `ChainStateFile` untagged: Array ↔ `Full`, Objekt ↔ `Pruned`, `[]` ↔
  `Full(vec![])`; keine Ambiguität in der Serde-Form selbst.
- Der inkrementelle 4d-Walk (`fold_one`/`hash_walk_state`) und die Proposer-Seite
  (`own_checkpoint_state`/`fold_state`) falten identisch — keine
  Konsens-Divergenz konstruierbar.
- Der Restored-Re-Key-SECURITY-Fix (Fremd-Key-Ablehnung in `apply_membership`)
  und die Seat-Proof-/Koordinator-Autorisierung halten.
- Slint-`id != ""`-Guards (keine id-fordernden Aktionen auf id-losen Zeilen) sind
  auf beiden Seiten konsistent durchgesetzt; `page_slice` ist leer-sicher; die
  WAV-Casts sind saturating.

---

## Zusammenfassung

**Gefixt in diesem Review (9):** C1, H1, H2, H4, H6, M1, M2, M3, plus der
`id + 1`-Overflow im Proposed-Arm (Teil von H2/C1-Härtung).

**Dokumentiert für später (nach Schwere):** H3, H5 (beide „braucht
Signatur-/Semantik-Änderung mit Ripple"), M4–M13, L1–L15.

Die gefixten Punkte sind die Security-kritischen (Governance-Bypass,
Remote-Abort, Governance-Freeze, Determinismus-Keystone) und die klar-kaputten
billigen (Speicher-Amplifikation, Zombie-Prozesse, Fake-Erfolg). Die
zurückgestellten sind entweder Design-Änderungen mit Aufwand (Ack-Durability,
MCP-Merge-Semantik, „refuse newer version") oder enge Randfälle, deren
fragiler Teilfix mehr Risiko als Nutzen brächte — jeweils mit Fix-Richtung
notiert.
