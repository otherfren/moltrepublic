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

### H5 — MCP `save_settings` überschreibt weggelassene Felder mit Defaults ✅ GEFIXT (2026-08-07)
`crates/molt-mcp/src/lib.rs` `settings_arg`/`cmd_save_settings`. Ein
Teilaufruf `{"headless":true}` füllt den Rest aus `SessionSettings::default()`:
`mcp_token=""` (TCP-Auth aus), `anonymity="none"` (ein Tor-Node fällt still auf
Clearnet — Deanonymisierung), `mcp_allow` zurückgesetzt, S3-Creds/`smp_url`
gelöscht — und `persist_settings` macht es **dauerhaft** in `config.toml`.
**Gefixt, mit ZWEI Verben statt einer Merge-Basis.** Die vorgeschlagene
Richtung geht nicht dort, wo sie vorgeschlagen war: der Tool-Builder ist
`fn(&Value) -> Result<Command, String>` und hält die laufende Session gar
nicht — er kann „weggelassen" nicht von „auf den Default gesetzt"
unterscheiden. Also:

- `save_settings` ist ein ehrliches VOLLSTÄNDIGES Ersetzen — jedes Feld ist
  `required`, und ein Teilaufruf wird mit Namen abgelehnt („`anonymity` is
  required — to change one setting use patch_settings"). Ein stiller Reset
  ist nicht mehr möglich.
- `patch_settings` → `Command::PatchSettings { patch }` ist der Teil-Weg; die
  Zusammenführung passiert in der ENGINE gegen die laufenden Settings, dort
  wo die Ist-Werte liegen. Unbekannte Schlüssel werden abgelehnt (ein still
  ignoriertes `anonimity` liest sich wie „die Einstellung hat nicht
  gegriffen"), und `relays`/`clearnet_relays_enabled` behalten ihre eigene
  Tür.

Gepinnt: `a_partial_settings_payload_is_refused_not_defaulted` (MCP),
`a_patch_leaves_every_field_it_does_not_name` +
`a_patch_naming_nothing_or_naming_junk_is_refused` (Engine — die
interessante Zusicherung ist, dass Tor und Token NICHT angefasst werden).

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

### M4 — SMP ackt eager, während der Supervisor auf Redelivery gehaltener Acks baut ⬜ GEGENSTANDSLOS (2026-08-07)
`crates/molt-net/src/smp/conn.rs`/`transport.rs`. `recv_next` ackt Nachricht N zum
Server, sobald N+1 geholt wird — bevor der Supervisor entscheidet zu puffern.
Eine im Epoch-/Reorder-Puffer gehaltene Nachricht (Re-Key-Storm → shed) oder ein
Block zwischen Server-Ack und Engine-fsync ist bei Crash **permanent verloren**
(der Server liefert Geacktes nicht neu). Loopback fängt es nicht (nur unacked
Redelivery). Die „loopback kann's nicht fangen"-Klasse aus CLAUDE.md.
**Gegenstandslos:** der SMP-Transport wurde in Etappe N-demo (2026-07-30)
ersatzlos entfernt — `crates/molt-net/src/smp/` existiert nicht mehr. Die
Ack-Disziplin der Nostr-Zustellgarantie ist eine andere Konstruktion
(`docs/transport/delivery_guarantee.md`).

### M5 — Alte transport.state/log werden von neueren Versionen still geklobbert ✅ GEFIXT (2026-08-07)
`crates/molt-storage/src/lib.rs` `read_transport_state` (Z.930): Version >
`TRANSPORT_STATE_VERSION` ⇒ `default()`, und der `SaveTransport`-RMW schreibt die
neuere Datei mit der alten Version zurück — MLS-Snapshot + SMP-Queue-Creds weg,
Mesh-Resume dauerhaft kaputt (SMP verbietet Subscribe auf fremd-erzeugte
Queues). Verletzt die Additive-Only-Regel „ein älterer Leser darf bei Unbekanntem
nicht schreiben".
**Gefixt** genau so: `ensure_transport_state_not_newer` in `open_workspace`,
das Gegenstück zu `openable_gate` für das Manifest. NUR der Neuer-Fall
verweigert — fehlend/unlesbar/nicht-authentisch/undekodierbar bleiben
„starte frisch", was für eine Ratchet die ehrliche Antwort ist und aus einem
reparierbaren Workspace sonst einen unöffenbaren machen würde. Gepinnt:
`a_newer_transport_state_refuses_the_open` (prüft auch, dass die Datei nach
der Verweigerung UNVERÄNDERT auf der Platte liegt).

### M6 — Torn-Tail-Recovery truncatet beim ERSTEN kaputten Frame ⇒ Datenverlust 📋 DOKUMENTIERT
`crates/molt-storage/src/lib.rs` (~Z.1257). Ein einzelner Bitflip früh im letzten
(bis 8 MiB) Segment löscht alle validen, bereits geackten Frames dahinter — im
Extremfall den ganzen Log inkl. Genesis, und `open_workspace` „gelingt" mit
leerer Historie. Mittlere Segmente bekommen den konservativen Hard-Error, das
letzte nie.
**Zurückgestellt** — braucht eine „folgt ein valider Frame nach dem Schaden?"-
Prüfung (Torn-Tail vs. Bitrot unterscheiden) vor der destruktiven Truncation.

### M7 — MCP `serve_conn`: unbegrenztes `read_line` vor Auth (Pre-Auth-DoS) ✅ GEFIXT (2026-08-07)
`crates/molt-mcp/src/lib.rs` (~Z.95). Auf einem `0.0.0.0`-Allow-Node streamt ein
Client, der nur den IP-Filter passiert, GB ohne Newline; `read_line` wächst
unbegrenzt → OOM.
**Gefixt:** `MAX_RPC_LINE` (1 MiB) über `take()` VOR dem Parse; eine
überlange Zeile beendet die Verbindung mit einer JSON-RPC-Fehlerantwort,
statt sie zu überspringen — der Rest würde sonst als eigener Request
parsen. Gepinnt:
`a_giant_pre_auth_line_ends_the_connection_instead_of_growing`.

### M8 — `random_token()` liefert still `""` bei RNG-Fehler/Non-Unix ✅ GEFIXT (2026-08-07)
`crates/molt-config/src/lib.rs` (~Z.341). `--generate-config` in einer Sandbox
ohne `/dev/urandom` (oder Non-Linux) schreibt `token = ""` und meldet Erfolg —
der Node akzeptiert dann jede MCP-Verbindung ohne Token.
**Gefixt:** `getrandom` statt Pfad-Open, und die Signatur ist jetzt
`Result<String, TokenError>` — der Typ zwingt beide Aufrufer zur Antwort:
`--generate-config` bricht ab (statt eine Config mit leerem Token als Erfolg
zu melden), die GUI-Rotation lässt das ALTE Token stehen und sagt es.
Gepinnt: `a_minted_token_is_full_length_hex_and_never_repeats`.

### M9 — MCP-Token wird zur Laufzeit nicht neu geladen ✅ GEFIXT (2026-08-07)
`crates/molt-mcp/src/lib.rs` (~Z.67). `serve_tcp` fängt das Token beim Start ein;
eine Rotation wirkt erst nach Neustart — entgegen mcp-security.md „takes effect
immediately". Ein geleaktes Token bleibt gültig, das neue nicht.
**Gefixt** ohne zweites Handle: der Accept-Loop liest das Token pro
Verbindung aus der LAUFENDEN Session (`live_token`) — die eine Quelle der
Wahrheit, in der eine Rotation über beide Oberflächen sofort steht. Ein
fehlgeschlagener Lesevorgang fällt auf das Boot-Token zurück, nie auf
„keines". Gepinnt: `the_accept_loop_reads_the_token_that_is_current_now`.

### M10 — Chat-Archiv-Pager: Clamp nutzt `applied.len()`, Slice nutzt `log.len()` ⬜ GEGENSTANDSLOS (2026-08-07)
`crates/molt-ui/src/lib.rs` (~Z.2678). Der Clamp begrenzt die gespeicherte Seite
mit der rohen Applied-Zahl, der Pager/Slice paginiert aber über die projizierte
Log-Länge (inkl. gemergter System-Zeilen in Patch-Kanälen). In einem
Patch-Kanal-Archiv mit System-Zeilen ist die letzte Seite unerreichbar
(„›" springt zurück).
**Gegenstandslos:** die Chat-Archiv-Ansicht gibt es nicht mehr — der Chat
ist EIN Aufbewahrungsfenster (Produktentscheidung 2026-08-04), und mit der
Ansicht fiel ihr Pager weg.

### M11 — Legacy-`anonymity="nym"` erzeugt Phantom-„ungespeichert" im Settings-Screen ✅ GEFIXT (2026-08-07)
`crates/molt-ui/src/lib.rs` (~Z.4166). Nach dem nym-Entfernen lädt eine
Alt-Config `anonymity="nym"` als Dropdown-Index „none", `read_settings_draft`
gibt „none" zurück, der Ist-Wert bleibt aber „nym" ⇒ Draft ≠ Stored für immer:
die Unsaved-Modal erscheint bei jedem Verlassen, externe Settings-Änderungen
werden nicht mehr gespiegelt, ein Save schreibt still „nym"→„none".
**Gefixt, aber ANDERSHERUM als hier vorgeschlagen.** Die Normalisierung
„nym"→„none" wäre der eine Ausgang, der schlimmer ist als der Befund: der
Dialer schlägt bei „nym" fail-closed fehl, „none" wählt. Aus „anonymisiere
mich" still „tu es nicht" zu machen, ist genau die Deklassierung, vor der
H5 warnt. Stattdessen ist der Wert RETIRED: `AnonymityNetwork::Nym` ist
weg, `validate_settings` nimmt ihn nicht mehr an, und `load_config` weigert
sich beim Namen zu starten (`selects_retired_nym`) statt still umzudeuten —
ein Knoten in diesem Zustand startete bisher fröhlich und scheiterte dann
an jedem Dial. Gepinnt:
`the_retired_nym_is_named_and_refused_not_normalized`.

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
