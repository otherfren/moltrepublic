# Mock-Inventur — was noch nicht echt ist

Stand: 2026-07-18

Dieses Dokument ist eine erschöpfende Inventur aller Features, die (a) im Code
explizit als Mock/Demo/Simulation markiert sind oder (b) eine GUI-/MCP-Oberfläche
haben, hinter der keine echte Funktion liegt (No-op, Fake-Daten, nur lokaler
UI-State). Jede Fundstelle wurde gegen die tatsächliche Rust-Verdrahtung
verifiziert (Command → Engine-Handler → tatsächliches Verhalten), weil viele
Kommentare veraltet sind — das Projekt ist weiter, als seine Kommentare behaupten.
**Zweck: Entscheidungsgrundlage. Hier wird noch NICHTS umgesetzt.**

Am Ende steht ein eigener Abschnitt „Veraltete Mock-Kommentare“: Stellen, die als
Mock beschriftet sind, hinter denen aber längst echte Funktion liegt — wertvoll
zum Aufräumen, aber kein Implementierungsbedarf.

## Übersicht

| Nr. | Titel | Komplexität |
|----:|-------|:-----------:|
| 1 | ✅ ERLEDIGT — Workspace-Ordner-Auswahl ohne echten Dateidialog | S |
| 2 | ✅ ERLEDIGT — Create-Screen: „Netzwerk“-Auswahl ist kosmetisch (dokumentiert, P8) | S |
| 3 | ✅ ERLEDIGT (Größe; Sync-Felder → Nr. 6/12) — Statische Statusfelder der Workspace-Liste | S |
| 4 | ✅ ERLEDIGT (entfernt) — Demo-Boot-Gruppe + Demo-Mesh mit Antwort-„Brains“ | S |
| 5 | ✅ ERLEDIGT — S3 „Verbindung testen“ zeigt immer Erfolg (reiner UI-Toast) | M |
| 6 | ✅ ERLEDIGT — Präsenz ohne echte Zeitstempel: keine Alterung, Mock-Aktivitäts-Trio | M |
| 7 | ✅ ERLEDIGT (entfernt) — Legacy-Zähl-Simulation der Governance | M |
| 8 | ✅ ERLEDIGT — Backup-Orphans: statische Demo-Daten in der Backup-Tabelle | M |
| 9 | ✅ ERLEDIGT — Manueller Workspace-Export („Backup als Blob“) ist ein UI-No-op | L |
| 10 | ✅ ERLEDIGT — At-rest-Verschlüsselung: Flag-Flip, Phrase ungeprüft, nicht persistent (S6) | L |
| 11 | ✅ ERLEDIGT (Vokabel entfernt) — Plugin-Governance ohne Plugin-Zustand | L |
| 12 | ✅ ERLEDIGT — Auto-Backup nach S3: Einstellungen komplett, Backend fehlt (S5) | XL |
| 13 | ✅ ERLEDIGT — Restore aus S3/Datei: vollständig simulierter Lauf mit Fake-Log (S4/S5) | XL |
| 14 | Vier Surfaces ohne Implementierung: Memory, Quests, Vault, Wallet | XL |

---

## 1. Workspace-Ordner-Auswahl ohne echten Dateidialog — **S**

> **✅ ERLEDIGT (2026-07-18).** „Durchsuchen…“ öffnet jetzt einen echten
> `rfd::AsyncFileDialog::pick_folder()` (Startverzeichnis = validierter Draft,
> `~`-Expansion wie im Engine-Pfad); das Textfeld bleibt als manueller Fallback.
> Mock-Wortlaut entfernt. Die Fundort-Zeilennummern unten sind historisch.

**Fundorte:**
- `crates/molt-ui-window/ui/app.slint:652` („mock folder picker for the settings workspace tab“), Modal bei `:6116`
- `crates/molt-ui/src/lib.rs:4617` (`set_ws_dir_body`: „Mock — kein echter Dateidialog“)

**Was heute passiert:** Der „Ordner wählen“-Knopf im Settings-Workspace-Tab
öffnet ein Modal mit einem freien Text-Pfadfeld. Die Einstellung selbst ist
**echt** (validiert, nach `config.toml` persistiert, Restart-Hinweis) — nur der
Picker ist keiner.

**Echte Implementierung:** `rfd::AsyncFileDialog::pick_folder()` anbinden — `rfd`
ist bereits Dependency und wird für Datei-Share (`on_share_pick`,
`crates/molt-ui/src/lib.rs:913`) und Logo-Auswahl (`:1147`) genutzt. Nur
molt-ui + ein Callback.

**Komplexität: S** — dasselbe Muster existiert zweimal im selben File.

## 2. Create-Screen: „Netzwerk“-Auswahl ist kosmetisch (dokumentiert, P8) — **S**

> **✅ ERLEDIGT (2026-07-18): Variante (a) umgesetzt.** `CreateStart` hat kein
> `net`-Feld mehr (Command + MCP-Tool-Param entfernt); der Create-Screen zeigt
> die effektive globale Einstellung read-only (mit Hinweis auf Einstellungen →
> Netzwerk); `WorkspaceInfo.net`/`CreateState.net` werden engine-seitig aus den
> globalen Settings abgeleitet (`molt_core::effective_net_label` — auch beim
> Join/Recovery/Restore und beim Startup-Scan, die vorher „tor“ hartkodierten).
> Details: `tor_transport_implementation.md` §P8. Die Fundort-Zeilennummern
> unten sind historisch.

**Fundorte:**
- `crates/molt-ui-window/ui/app.slint:1570–1624` (Dropdown tor/none, Tor-Modus, Port; `:1624` übergibt nur den String `"tor"`/`"none"`)
- `crates/molt-engine/src/lifecycles.rs:179,198` (`cmd_create_start` speichert `net` nur als Label), `:645`, `:906` (Join hartkodiert `"tor"`)
- `crates/molt-engine/src/founding.rs:365–374`: der Ritual-Transport kommt aus `resolve_dialer()` = **globale** Settings (`crates/molt-engine/src/session.rs:256–280`)
- Dokumentierte Entscheidung: `documents/tor_transport_implementation.md` §P8 („`CreateStart.net` stays cosmetic“)

**Was heute passiert:** Die Netzwerk-Wahl beim Gründen (inkl. Tor-Modus und
Port) beeinflusst den Transport nicht; maßgeblich sind immer die globalen
Anonymitäts-Settings. Der Wert landet nur als Anzeige-Label in
`WorkspaceInfo.net` („Netzwerk: tor“ im Open-Detail).

**Echte Implementierung:** Entweder (a) das Dropdown entfernen/durch eine
Anzeige der globalen Einstellung ersetzen (S, ehrlicher), oder (b) echten
per-Workspace-Transport bauen (L: per-Workspace-Dialer durch Founding, Join,
Reopen und Recovery fädeln). P8 hat (a) sinngemäß schon entschieden — offen ist
nur der Aufräum-Schritt.

**Komplexität: S** (für die ehrliche Variante; per-Workspace-Transport wäre L).

## 3. Statische Statusfelder der Workspace-Liste — **S**

> **✅ ERLEDIGT für die Größe (2026-07-18).** `size_kib` ist echt: rekursiver
> KiB-Walk in molt-storage (symlink-sicher, race-tolerant), gestempelt bei
> Boot-Scan, Materialisierung, Open (nach Replay) und Clean-Close; Session-only
> Demo-Einträge melden ehrlich 0. Die Sync-Felder (`synced`/`state`/
> `last_sync_min`/`sync_queue`) gehören zu Nr. 6, `last_backup_min` zu Nr. 12.

**Fundorte:**
- `crates/molt-engine/src/lifecycles.rs:186–196` (`push_workspace_entry`: `synced: true`, `state: 0`, `last_sync_min: 0`, `sync_queue: 0`, `size_kib: 16`, `last_backup_min` = 0/NEVER)
- `crates/molt-core/src/lib.rs:386–402` (Felddefinitionen, `size_kib` „(mock)“)
- Anzeige: Open-Screen-Tabelle + Detailpanel (`crates/molt-ui/src/lib.rs:1440ff`, Sortierung `sort_ws_items`)

**Was heute passiert:** Jeder echte Workspace zeigt konstant „synced“, „16 KiB“,
„last sync 0 min“, Queue 0. Die Werte werden nie aktualisiert; nur der
Demo-Datensatz (`demo_set`) trägt abwechslungsreiche Fake-Werte.

**Echte Implementierung:** `size_kib` aus der Verzeichnisgröße beim Scan/Close
berechnen (molt-storage, trivial); `last_sync_min`/`sync_queue`/`state` aus dem
echten Outbox-/Delivery-Zustand des Mesh speisen (molt-engine `net.rs`, hängt
mit Finding 6 zusammen); `last_backup_min` gehört zu Finding 12.

**Komplexität: S** für die Größe; die Sync-Felder sind der M-Teil von Finding 6.

## 4. Demo-Boot-Gruppe + Demo-Mesh mit Antwort-„Brains“ — **S**

**Fundorte:**
- `crates/molt-app/src/main.rs:177` (`GroupConfig::demo()` als Boot-Gruppe des echten moltd)
- `crates/molt-core/src/lib.rs:3133–3141` (`GroupConfig::demo`: me/peer-1/peer-2, 2-of-3), `:451–466` (`demo_workspace_id`), `:487ff` (`demo_set` — nur in Sessions ohne Persistenz; moltd ersetzt die Liste durch den Disk-Scan, `main.rs:165`)
- `crates/molt-engine/src/net.rs:19–56, 523–640, 1589–1706` (Demo-Mesh, `spawn_demo_peer`, canned `LINES`, Antwort-Wahrscheinlichkeit/Delay per `mockrand`)
- `crates/molt-core/src/lib.rs:1746–1763` (`mockrand` — ausdrücklich Nicht-Krypto-PRNG für die Simulationen)
- `crates/molt-engine/src/net.rs:555–563`: läuft für den Kontext ohne offenen Workspace UND für persistierte Workspaces mit `prefs.simulated_members` (vor T3 gegründete Alt-Bestände)

**Was heute passiert:** Ohne offenen Workspace läuft ein In-Prozess-Loopback-Mesh
mit zwei simulierten Peers, die auf ~jede fünfte Nachricht eine kanonische
Demo-Zeile antworten. Absichtliche, sauber abgegrenzte Demo-Maschinerie (echte
Workspaces bekommen nie Fake-Peers; ein Alt-Workspace nur per explizitem
Prefs-Flag) — aber sie steckt im Produktions-Binary und ist über MCP
(`chat_send` ohne offenen Workspace) erreichbar.

**Echte Implementierung:** Produktentscheidung: als Onboarding-Spielwiese
behalten (dann klar labeln) oder entfernen (Boot-Kontext ohne Chat). Entfernen
wäre klein; die Test-Nutzung (`tests/demo_mesh.rs`) müsste auf einen Test-Seam
umziehen.

**Komplexität: S** (Entscheidung + Entfernen/Labeln; kein Neubau).

**Status (2026-07-18): ENTFERNT** (Produktentscheidung). moltd bootet mit
`GroupConfig::solo()` (nur der Operator, keine erfundenen Peers); das
Demo-Mesh liegt hinter dem default-OFF Test-Seam `State::demo_mesh`
(Spawner `__spawn_demo_mesh`, analog `ritual_sim`), und
`prefs.simulated_members` wird weiter geparst, spawnt aber keine Peers mehr
(inert). Chat ohne offenen Workspace ist ein ehrliches lokales Notizlog —
niemand antwortet. Negativ-Tests pinnen beides in `tests/demo_mesh.rs`;
`GroupConfig::demo()` / `demo_workspace_id` leben als reine Test-Fixtures
weiter.

## 5. S3 „Verbindung testen“ zeigt immer Erfolg — **M**

> **✅ ERLEDIGT (2026-07-18).** Echter `Command::NetTestS3` nach dem Muster des
> SMP-Tests: purer Rust-S3-Baustein in molt-net (SigV4 gegen offizielle
> AWS-Testvektoren, minimaler HTTP/1.1, rustls+webpki-roots) über den
> fail-closed, Tor-fähigen Dialer; ehrliche Fehlerklassen (Endpoint/Connect/
> TLS/403 Creds/404 Bucket/301 Region); MCP-Tool `net_test_s3`; Fake-Toast
> entfernt. Live gegen AWS verifiziert. Der Client ist die Basis für Nr. 8/12/13.

**Fundorte:**
- `crates/molt-ui-window/ui/app.slint:5083–5093`: `clicked => { root.show-toast(Strings.toast-s3-ok); }` — reiner UI-Toast, kein Command
- `crates/molt-ui/src/lib.rs:4623` (`set_s3_test`), Toast-String `toast_s3_ok`

**Was heute passiert:** Der Test-Knopf im Backup-Settings-Tab meldet
bedingungslos Erfolg, sobald Endpoint + Access-Key nicht leer sind. Es findet
kein Netz-Zugriff statt. Das kollidiert direkt mit der eigenen Regel „the UI
must not fake success“ (`crates/molt-ui/src/lib.rs:2414`) — der SMP-Test-Knopf
daneben macht es vor: echter Command (`NetTestServer`), Ergebnis-Streaming,
`#[ignore]`-Livetest (`crates/molt-engine/tests/smp_test_button.rs`).

**Echte Implementierung:** Ein `Command::NetTestS3` nach dem Muster des
SMP-Tests: Engine-seitig ein minimaler S3-Request (z. B. signierter
HEAD/ListBucket, pure-Rust-Signatur-Crate) über den konfigurierten Dialer
(Tor-fähig, fail-closed), Ergebnis in die Session. Schichten: molt-core
(Command), molt-engine, molt-net/HTTP, molt-ui, molt-mcp (Tool, Co-Equality).

**Komplexität: M** — klein im UI, aber es braucht den ersten echten
S3-Client-Baustein (der dann Finding 8/12/13 zuarbeitet) inkl. Tor-Routing.

## 6. Präsenz ohne echte Zeitstempel; Mock-Aktivitäts-Trio — **M**

> **✅ ERLEDIGT (2026-07-18).** `MemberInfo.last_seen` ist ein echter
> Unix-Stempel (0 = nie gesehen); ein 30-s-Ticker altert Pills (online ≤5 min,
> stale ≤30 min, sonst offline; Send-Backoff pinnt unreachable bis zur nächsten
> Sichtung); das 1h/24h/7d-Trio und die Upload-Verfügbarkeit rechnen aus echten
> Stempeln; UI rendert die relative Zeit clientseitig.

**Fundorte:**
- `crates/molt-engine/src/net.rs:1537–1587`: `cmd_net_peer_seen` / `cmd_net_send_failed` / `update_member_pill` — inkl. ehrlichem Kommentar „Honest limitation (T5 closes it)“: das Label „just now“ kann nicht altern
- `crates/molt-engine/src/proposals.rs:740, 758–776` (read_members: Präsenz = Session-Label), `:826–843` (Aktivitäts-Trio 1h/24h/7d = Projektion der Mock-Präsenz), `:815` (Upload-„verfügbar“ hängt an dieser Präsenz)
- `crates/molt-core/src/lib.rs:2979–2993, 3069–3079` (Felddoku „mock presence“), `:3043`
- `crates/molt-ui/src/lib.rs:1646, 2150, 2182, 3022` (GUI-Seite), MCP-Beschreibung `crates/molt-mcp/src/lib.rs:648`

**Was heute passiert:** Seit Commit b5a0055 ist Präsenz **halb echt**: der
Engine stempelt Pills ereignisgetrieben (Peer gesehen → „just now“/synced;
Sende-Backoff → „unreachable“/offline). Aber es gibt keinen numerischen
last-seen-Zeitstempel: ein Label altert nie, Mitglieder starten mit
Platzhaltern, das Status-Trio „aktiv 1h/24h/7d“ ist eine Projektion der
Pill-Zustände (7d = immer ganze Roster), und die Upload-Verfügbarkeit („Sharer
online?“) hängt an denselben Pills.

**Echte Implementierung:** `MemberInfo.last` durch einen numerischen
`last_seen`-Unix-Stempel ersetzen (Rendering UI-seitig wie beim
`last_sync_min`-Muster), Engine-Ticker altert Zustände; Trio aus echten
Stempeln rechnen. Schichten: molt-core (Feld, additiv), molt-engine
(Stempel/Ticker), molt-ui (Rendering), MCP-Beschreibung.

**Komplexität: M** — Ereignisquellen existieren schon; es ist Modell- und
Durchfädel-Arbeit über 4 Crates plus Testanpassung.

## 7. Legacy-Zähl-Simulation der Governance — **M**

> **✅ ERLEDIGT (2026-07-18): entfernt** (Produktentscheidung). Ohne
> Chain-Governance zeichnet ein Knoten höchstens seine EIGENE Zustimmung auf
> (Wiederholung = `AlreadyApproved`); niemand zählt mehr erfundene Peers hoch.
> Die Solo-Boot-Gruppe läuft als echte 1-of-1-Governance. `simulated_members`
> ist ein inertes Legacy-Label. Recovery kann aus alten simulierten
> Zählerständen keine frischen Applied mehr münzen (gepinnt).

**Fundorte:**
- `crates/molt-engine/src/lib.rs:13` („faithful but *simulated* stand-in for the real FROST threshold machine“)
- `crates/molt-engine/src/proposals.rs:213, 352–366, 718–721` (ein lokaler Operator zählt für die Peers hoch; anonymer Zähler; jede pending Proposal gilt für Peers als offen)
- Abgrenzung: `crates/molt-engine/src/chain.rs:615–616, 723` — Chain-Workspaces laufen **echte** Threshold-Governance über das Mesh; die Simulation ist dort per `is_chain_governed` AUS

**Was heute passiert:** Nur für die Demo-Boot-Gruppe und Alt-Workspaces mit
`simulated_members` gilt noch die gezählte Simulation: Approvals sind lokale
Zähler ohne Signaturen, wiederholtes Approve simuliert Peers. Für alle über
das echte Ritual gegründeten (chain-governed) Workspaces ist Governance real
(signierte, positions-gebundene Blocks, m-of-n).

**Echte Implementierung:** Kein Neubau nötig — der echte Pfad existiert. Offen
ist die Produktfrage, ob Alt-/Demo-Workspaces auf Chain-Governance migriert
oder die Simulation als bewusstes Demo-Verhalten dokumentiert bleibt.
Betroffen: molt-engine (`proposals.rs`), ggf. Migrationspfad in `chain.rs`.

**Komplexität: M** — Migrationsdesign für Altbestände; die Maschine selbst ist da.

## 8. Backup-Orphans: statische Demo-Daten — **M**

> **✅ ERLEDIGT (2026-07-18).** `demo_set()` ist gelöscht; die Backup-Tabelle
> speist sich aus einem echten, signierten `ListObjectsV2` über den fail-closed
> Dialer (`Command::NetListBackups` + MCP-Tool, Pagination, gehärtetes
> XML-Parsing, Generation-Counter gegen stale Ergebnisse); Refresh beim Öffnen
> des Backup-Tabs + expliziter Knopf; Fehler werden ehrlich angezeigt, nie
> erfundene Zeilen.

**Fundorte:**
- `crates/molt-core/src/lib.rs:656–676` (`BackupOrphan`, „(mock). Shows up in the settings backup table“, `demo_set()`)
- `crates/molt-app/src/main.rs:167`: moltd übernimmt `..SessionView::default()` — die Demo-Orphans landen im **echten** Produktions-Session-State
- `crates/molt-ui/src/lib.rs:1499–1525` (Backup-Tabelle: lokale Workspaces + Orphans), Anzeige `app.slint:5195ff`

**Was heute passiert:** Die Backup-Tabelle in den Settings zeigt immer zwei
erfundene Bucket-Einträge („nur remote“) — auch im echten moltd, ohne dass je
ein S3-Zugriff stattfand.

**Echte Implementierung:** Orphans aus einem echten Bucket-Listing speisen
(braucht den S3-Client aus Finding 5/12) und bis dahin die Demo-Daten aus dem
Produktionspfad entfernen (leer lassen). Schichten: molt-core (Default leeren),
molt-engine (Listing-Command), molt-app.

**Komplexität: M** — Sofortmaßnahme (Demo-Daten raus) ist S; echtes Listing
hängt am S3-Client.

## 9. Manueller Workspace-Export („Backup als Blob“) ist ein UI-No-op — **L**

> **✅ ERLEDIGT (2026-07-18).** Echter S4-Export: `molt-export-v1`-Blob
> (Argon2id 64 MiB/t=3/p=1, XChaCha20-Poly1305 in 4-MiB-Chunks, Header über
> Key-Bindung authentifiziert), atomarer Write mit fsync; MLS-Ratchets und
> SMP-Queue-Creds werden NIE exportiert; `Command::ExportWorkspace` + MCP-Tool;
> Design: `documents/backup_restore_design.md`. Import ist Nr. 13.

**Fundorte:**
- `crates/molt-ui-window/ui/app.slint:5766–5795`: das Backup-Modal setzt bei „Bestätigen“ nur `root.export-note = "Exportiert (Mock): <Pfad>"` — kein Command, kein Write
- `crates/molt-ui/src/lib.rs:4548` (`bk_body`: „Mock — es wird nichts geschrieben“), `:4537` (`ow_export_note`)
- Kein `Command::Export*` in `crates/molt-core/src/lib.rs`, kein MCP-Tool

**Was heute passiert:** „Backup jetzt erstellen“ im Open-Detail nimmt einen
Zielpfad entgegen und behauptet danach im Panel einen Export — es wird nichts
gelesen oder geschrieben. (Der Modal-Text sagt das immerhin ehrlich dazu.)

**Echte Implementierung:** Das Blob-Format ist Milestone S4 aus
`documents/concept-workspace-storage.md` (Status-Kopf: „S4 export/import …
remain open“): ein verschlüsselter Ein-Datei-Export (`.molt.enc`) des
Workspace-Verzeichnisses inkl. Schlüssel-Story, plus Import-Gegenstück
(Finding 13). Schichten: molt-storage (Format), molt-core (Command),
molt-engine, molt-ui, molt-mcp.

**Komplexität: L** — Formatdesign mit Krypto-Anteil; halbes Fundament von
Finding 13.

## 10. At-rest-Verschlüsselung: Flag-Flip, Phrase ungeprüft, nicht persistent — **L**

> **✅ ERLEDIGT (2026-07-18).** S6 derive-and-verify: Versiegeln entfernt
> Schlüsselmaterial von der Platte (Genesis-Frame-Tag = Phrase-Prüfung; falsche
> Phrase = harter Fehler, Platte unangetastet), Zustand kommt aus dem
> Verzeichnis-Marker (überlebt Neustart), `STORAGE_VERSION_SEALED`-Gate,
> Alt-Verzeichnisse ohne Migration; versiegelte Verzeichnisse verweigern
> Export/Open typisiert. Storage-lose Knoten verweigern ehrlich.

**Fundorte:**
- `crates/molt-engine/src/session.rs:576–611`: `cmd_encrypt_workspace` setzt nur `ws.encrypted = true` (Session-Feld); `cmd_decrypt_workspace` verlangt eine nicht-leere Phrase, prüft sie aber nicht
- `crates/molt-core/src/lib.rs:398–405, 2264–2277` (Command-Doku „(mock)“), `crates/molt-ui-window/ui/app.slint:354, 2180–2196`, `crates/molt-ui/src/lib.rs:4525` (`dw_body`)
- MCP-Tools `encrypt_workspace`/`decrypt_workspace` (`crates/molt-mcp/src/lib.rs:834–861`, Beschreibung ehrlich als Mock deklariert)
- Kontrast: der Event-Log selbst IST at-rest verschlüsselt (device-sealed key, `crates/molt-storage/src/lib.rs:8–39`); `scan_workspaces` liefert immer `encrypted: false` (`:1433`)

**Was heute passiert:** Der Schalter sperrt das Öffnen (Open verweigert
verschlüsselte Einträge) — aber es ist ein reines Session-Flag: auf der Platte
ändert sich nichts, jede Phrase „entschlüsselt“, und ein Neustart vergisst den
Zustand komplett (Scan setzt wieder `false`).

**Echte Implementierung:** Milestone S6 „passphrase sealing“
(`documents/concept-workspace-storage.md`): den device-sealed Log-Key
zusätzlich unter der Recovery-Phrase versiegeln, `seed.sealed` entfernen,
Phrase beim Decrypt echt verifizieren, Zustand aus dem Verzeichnis ableiten.
Schichten: molt-storage (Kern), molt-engine, molt-ui/mcp (nur Texte).

**Komplexität: L** — sicherheitskritische Schlüssel-Hierarchie, Migrationspfad
für Bestands-Workspaces, Design-Doc-Pflicht laut CLAUDE.md.

## 11. Plugin-Governance ohne Plugin-Zustand — **L**

> **✅ ERLEDIGT (2026-07-18): Vokabel entfernt** (Produktentscheidung). Die
> fünf Plugin-Erwähnungen waren reine Prosa; kein persistierter/wire-sichtbarer
> Typ trug je eine Plugin-Variante. Die Replay-Toleranz für unbekannte Org-Ops
> (tragend für Additiv-Only) ist jetzt per Test gepinnt.

**Fundorte:**
- `crates/molt-engine/src/proposals.rs:133` („no plugin state exists yet (mock) — nothing to show“)
- `crates/molt-core/src/lib.rs:48` (Organization: „charter, name, image, plugins“), `crates/molt-ui-window/ui/app.slint:4467` (Pending-Karten „plugin changes in voting“)

**Was heute passiert:** „Plugins“ existieren nur als Vokabel: eine
Organization-Proposal mit einem Plugin-Op würde durch die echte Governance
laufen, aber es gibt keinen effektiven Plugin-Zustand, nichts wird aktiviert,
die Ist/Soll-Anzeige bleibt leer. GUI bietet keinen Plugin-Editor an
(nur Charter/Name/Logo/Retention sind angebunden — die sind echt).

**Echte Implementierung:** Erst Produktdefinition (was ist ein Plugin?), dann:
effektiver Org-Zustand um Plugins erweitern (`org_effective`), Apply-Semantik,
UI-Liste. Schichten: molt-core, molt-engine, molt-ui.

**Komplexität: L** — nicht wegen Technik, sondern weil die Produktdefinition
fehlt; ohne sie wäre auch Entfernen der Vokabel (S) ehrlich.

## 12. Auto-Backup nach S3: Einstellungen komplett, Backend fehlt (S5) — **XL**

> **✅ ERLEDIGT (2026-07-19).** Echter Backup-Ticker (60 s, Actor-Muster):
> crash-konsistenter `molt-export-v1`-Blob im Workspace-Key-Modus, signierter
> PUT über den fail-closed Dialer, `last_backup` wandert NUR bei bestätigtem
> Upload, Retention löscht erst nach Bestätigung und nur eigene Keys,
> versiegelte Workspaces werden ehrlich übersprungen; `backup_now`-Tool;
> Fake-Stempel beim Einschalten entfernt.

**Fundorte:**
- `crates/molt-engine/src/session.rs:613–633`: `cmd_set_workspace_backup` — die Pref wird **echt** persistiert (`prefs.toml`), aber der Kommentar sagt es klar: „the uploader itself is milestone S5; the stamp keeps list and prefs consistent“ — `last_backup_min = 0` wird gestempelt, ohne dass ein Backup läuft
- Settings-Tab „Backup“: Intervall, Kopien-Retention, Endpoint/Keys/Bucket (`app.slint:5013–5133`) — alles wird real nach `config.toml` persistiert (`configstore.rs:336–342`), aber von niemandem konsumiert; kein S3-Code existiert im Workspace (grep über molt-net/molt-storage: keine Treffer)
- MCP-Tool `set_workspace_backup` (`molt-mcp/src/lib.rs:817`)
- Querverweis: `documents/total_review.md` (Medium-Finding: „das S3-Backend selbst bleibt die bekannte deferred-Arbeit“), `documents/concept-workspace-storage.md` (S5 offen)

**Was heute passiert:** Der Auto-Backup-Schalter, Intervall und Retention sind
voll bedienbar und überleben Neustarts — aber es gibt keinen Uploader, keinen
Ticker, keine Bucket-Interaktion. „Letztes Backup: gerade eben“ ist eine
Behauptung, die der Stempel beim Einschalten erzeugt.

**Echte Implementierung:** Milestone S5: S3-Client (pure-Rust, über den
Tor-fähigen Dialer), periodischer Backup-Ticker in der Engine, Blob-Format aus
Finding 9, Retention (keep-copies), Orphan-Listing (Finding 8), ehrliche
last-backup-Stempel. Schichten: molt-net/HTTP-Client, molt-storage,
molt-engine, molt-core, molt-ui, molt-mcp.

**Komplexität: XL** — neuer Netz-Client + Hintergrund-Lebenszyklus +
Format-Abhängigkeit zu Finding 9; sicherheits- und privacy-relevant (Creds,
Tor-Routing).

## 13. Restore aus S3/Datei: vollständig simulierter Lauf mit Fake-Log — **XL**

> **✅ ERLEDIGT (2026-07-19).** Der Fake-Lauf (erfundene Log-Zeilen, ~45 %-
> Fehlregel, „Restored Republic“) ist gelöscht. Echter zweiphasiger Import:
> Staging (Allowlist — `keys/`/`transport.state` können nie über einen Blob
> einwandern) → **Chain-Verify hard-reject vor jeder Materialisierung**
> (geteiltes `verify_served` mit der Recovery-Adoption) → Commit (Re-Seal
> unter lokalem Device-Key, frische Minimal-Identität, nie Ratchets/Creds).
> S3-Weg per Streaming-GET mit Caps; Workspace öffnet ehrlich „detached“ —
> der Weg zurück in die lebende Republik bleibt das Recovery-Ritual.

**Fundorte:**
- `crates/molt-engine/src/lifecycles.rs:207–248` (`cmd_restore_start/tick/cancel/finish`), `:312–366` (`restore_tick`: erfundene Log-Zeilen — „GET …/manifest.enc · 200 OK“, „chunk 17/23 fetched · sha256 ok“, „aes-256-gcm: chunk decrypted · merkle node ok“ — und die Fake-Fehlregel „unplausibles Ziel scheitert bei ~45 %“)
- `:238–309` (`cmd_restore_finish`): materialisiert einen **frisch gegründeten** Workspace „Restored Republic“ ohne Chain, MLS, Mesh oder Identitäten („restore rebuilds … at S4/S5“)
- GUI: Restore-Screen S3-/Datei-Pfade `app.slint:2664, 2821–2860` (zeigt „<endpoint> · OK“ in Grün, sobald ein Endpoint konfiguriert ist — ungeprüft), `:2924` (Datei), Modal `rw_file_body` („Mock — es wird nichts gelesen“)
- MCP-Tools `restore_start/cancel/finish` (`molt-mcp/src/lib.rs:864–893`, Beschreibung ehrlich: „(mock) restore“)
- Kern von `molt_core::RestoreState` als „The (mock) restore lifecycle“ (`molt-core/src/lib.rs:1894, 1974`)

**Was heute passiert:** Die beiden Restore-Wege „aus S3“ und „aus Datei“ sind
eine reine Fortschritts-Show: kein Byte wird gelesen, das Ergebnis ist ein
leerer, frisch generierter Workspace mit neuem Seed. **Abzugrenzen:** der
dritte Weg auf demselben Screen — Wiederbeitritt über Recovery-Link
(`recover_invite_start`/`recover_start`) — ist **real** (Recovery-Ritual über
SMP, Chain-Übernahme, verifiziert; `crates/molt-engine/src/recovery.rs`,
`lifecycles.rs:1044ff`) und darf nicht als Mock einsortiert werden.

**Echte Implementierung:** S4 (Import des verschlüsselten Blobs: lesen,
entschlüsseln, verifizieren, Verzeichnis + Identitäten + Chain + MLS-Snapshot
wiederherstellen) und S5 (derselbe Pfad aus dem Bucket). Ersetzt Fake-Ticker
durch echten Task mit echten Fortschrittsereignissen. Schichten: molt-storage,
molt-engine, molt-net (S3), molt-core, molt-ui, molt-mcp.

**Komplexität: XL** — Gegenstück zu 9+12 plus Verifikations-/Recovery-Semantik
(was ist ein gültiges Backup? Chain-Verify beim Import ist Pflicht).

## 14. Vier Surfaces ohne Implementierung: Memory, Quests, Vault, Wallet — **XL**

**Fundorte:**
- `crates/molt-core/src/lib.rs:41–130` (Surface-Enum inkl. Sub-Views für alle sechs; Memory „shared brain“, Quests „quest board“, Vault „sealed secrets“, Wallet „Monero multisig in production“)
- `crates/molt-ui-window/ui/app.slint:1105`: Navigation ausgegraut — „staged out for now: only Organization and Chat are live in the GUI“; für die vier gibt es keinen Content-Pane (`:3014ff` rendert nur chat/organization)
- `crates/molt-ui/src/lib.rs:4094–4104` (`default_op`: add_note/add_quest/seal_secret/transfer), `:4670` (`mv_later` — inzwischen ungenutzter String)
- MCP: `select_surface`/`select_view`/`propose` akzeptieren die vier Surfaces (`session.rs:44–60` validiert nur gegen `Surface::ALL`)

**Was heute passiert:** Die Governance-Mechanik für die vier Surfaces ist echt
(eine Proposal an „memory“ durchläuft Threshold-Abstimmung und landet im
Applied-Log der Chain) — aber es gibt **keinen Surface-Zustand**: kein Notiz-,
Quest-, Vault- oder Wallet-Modell, nichts wird durch ein Apply verändert, die
GUI zeigt nichts an (Navigation gesperrt; ein MCP-`select_surface` auf
„vault“ führt in der GUI zu einer leeren Fläche).

**Echte Implementierung:** Vier eigenständige Produkte auf der vorhandenen
Gated-Governance: Datenmodell + Apply-Semantik in molt-core/molt-engine,
Views in molt-ui, Tools in molt-mcp. Wallet ist das größte (Monero-Multisig =
eigenes Krypto-/Netz-Projekt), Vault braucht Threshold-Release-Krypto,
Memory/Quests sind „nur“ CRDT-artige Zustandsmodelle über der Chain.

**Komplexität: XL** — pro Surface eher je ein eigenes L–XL; hier als ein
Sammelposten geführt.

---

## Absichtliche Test-Seams (kein Handlungsbedarf, der Vollständigkeit halber)

- **Simulierte Founding-Mitglieder** (`ritual_sim`): nur über Test-Spawner
  aktivierbar (`crates/molt-engine/src/lib.rs:263, 601` — Default `false`;
  `founding.rs:1499–1526`). Die GUI zeigt dann ehrlich den SIMULATION-Badge
  (`cw_sim_badge`). Das In-App-Gründen läuft real über SMP.
- **Manual-Seam** (`spawn_manual_ritual`, `#[doc(hidden)]`): der
  Zwei-Instanzen-Loopback-Testpfad (`two_instances.rs`).
- **LoopbackTransport** (`crates/molt-net/src/loopback.rs`): bewusst permissives
  Test-Doppel; die CLAUDE.md-Warnung dazu ist aktuell.
- `crates/molt-storage/examples/mkdummy.rs`: Dev-Werkzeug.

## Veraltete Mock-Kommentare (real, nur Doku falsch)

Hinter diesen Markierungen liegt längst echte Funktion — lohnendes Aufräumen,
sonst führen sie die nächste Session in die Irre:

1. **`crates/molt-ui-window/ui/app.slint:14`** — „Everything is a mock: nothing
   is written to disk.“ Falsch: Storage (S0–S3), Settings-Persistenz, Founding,
   Chain, Chat sind real.
2. **`crates/molt-ui/src/lib.rs:21`** — Crate-Header: „workspace lifecycles are
   still a **simulation** — no workspace is created on disk yet.“ Falsch, s. o.
3. **`crates/molt-ui-window/ui/theme.slint:11–14`** — AppScreen-Kommentare:
   `open` „(mock list)“ (Liste kommt aus dem echten Disk-Scan,
   `molt-app/main.rs:104`), `join` „(mock)“ (echtes Join-Ritual über SMP),
   `settings` „(mock — no disk write)“ (persistiert format-erhaltend nach
   `config.toml`). `restore` „(mock)“ stimmt nur noch für die S3-/Datei-Wege
   (Finding 13); der Recovery-Weg ist real.
4. **`crates/molt-ui-window/ui/app.slint:4806`** — „settings (in-memory config
   editor — MOCK, no disk write)“ — gleicher Irrtum.
5. **`crates/molt-ui-window/ui/theme.slint:147` + `molt-core/src/lib.rs:398`** —
   `seed` „(mock) recovery seed“: der Seed ist echte Entropie
   (`generate_seed_phrase`), device-sealed auf Platte (`seed.sealed`), Basis der
   Schlüsselableitung. Nur die *Verifikation beim Decrypt* fehlt (Finding 10).
6. **`crates/molt-ui/src/lib.rs:4512` (`ou_note`) und `app.slint:663, 4168`** —
   „(Übertragung und Ablauf sind Mocks.)“ Falsch: Dateitransfer ist real
   (Manifest/Pieces über MLS, `crates/molt-engine/src/chat.rs:216ff`,
   `transfer`-Modul, Reorder-Bugfix 2ef58b0), und `expires_ts` ist die echte
   Retention-Deadline (`proposals.rs:787, 814`).
7. **`crates/molt-ui/src/lib.rs:4685` (`dm_body`)** — Nachricht löschen:
   „(Mock — nichts auf der Platte.)“ Falsch: Deletes sind id-adressiert, gehen
   über die Leitung und liegen im persistierten Event-Log.
8. **`crates/molt-ui/src/lib.rs:4689` (`close_ws_body`)** — „Dies ist ein Mock,
   es wird nichts auf die Platte geschrieben.“ Falsch: der Clean-Close
   persistiert MLS-Snapshot + Transport-Credentials (`cmd_close_workspace`,
   `session.rs:563–574`; `persist_crypto_blocking`).
9. **`crates/molt-ui/src/lib.rs:4545` (`del_ws_body`)** — „Mock — auf der
   Platte wird nichts angefasst.“ Falsch: `cmd_delete_workspace` verschiebt das
   Verzeichnis real in den Trash (30-Tage-Purge; `session.rs:691–724`,
   `molt-storage:1520`).
10. **`crates/molt-ui-window/ui/app.slint:683`** — Chat-Retention „(mock
    current value; a change is a proposal)“: der Wert kommt aus dem echten
    effektiven Org-Zustand (`set_chat_retention`-Proposals, engine-seitig
    gefiltert).
11. **`crates/molt-ui-window/ui/theme.slint:435`** — „the mock logo/charter
    edit modals“: Logo/Charter/Name-Änderungen sind echte
    Organization-Proposals inkl. sign-what-you-see-Bildbytes.
12. **`crates/molt-app/src/main.rs:131–138`** — Boot-Log: „loopback transport
    active; Tor/SMP wiring is milestone T3–T5“ — T3/T4 sind längst auf master.
13. **`crates/molt-ui/src/lib.rs:4439` (`cw_ritual_hint_sim`)** — „Echte
    Mitglieder über SMP kommen mit T3.“ Der Text erscheint nur im Test-Seam,
    behauptet aber ein nicht mehr existierendes Fehlen von T3.
14. **`crates/molt-mcp/src/lib.rs:648`** — `read_members`-Beschreibung „mock
    last-seen/presence“: seit b5a0055 nur noch halb wahr (engine-gestempelte
    Pills; Finding 6 beschreibt den offenen Rest).
15. **`crates/molt-ui-window/ui/app.slint:669, 674`** — „(mock) download“ des
    Proposal-Bilds: die Vorschau dekodiert die echten, mitgestimmten
    Payload-Bytes lokal — kein Download nötig, kein Mock.
16. **Tote Strings:** `mv_later` (`molt-ui:4670`, nirgends mehr referenziert)
    und der Property-Name `choice-mock-note` (`theme.slint:340` — der Text
    selbst ist inzwischen ehrlich).
17. **`crates/molt-ui-window/ui/app.slint:741, 863, 1889`** — „mock local
    workspace list“ / „engine mock-run“ / „open (mock empty list)“: Liste und
    Create-/Join-Läufe sind real; nur der Restore-Lauf (Finding 13) tickt noch
    simuliert.
