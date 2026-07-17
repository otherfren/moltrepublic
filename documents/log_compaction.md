# Kompaktierung: WP4a ephemeres Pruning · WP4b Chain-Checkpoint

> **STATUS: ENTSCHIEDEN (2026-07-17, Diskussion mit dem User).**
> F1–F4 sind beschlossen (§A.2); das Doc ist in zwei Teile gespalten:
> **WP4a** = physisches Pruning des flüchtigen Logs (lokal, Policy-vollziehend),
> **WP4b** = threshold-signierter Chain-Checkpoint (m-of-n bestätigen die
> Korrektheit der Kompaktierung — Ergebnis der Checkpoint-Diskussion).
> Routinemäßige WP4a-Läufe sind bewusst NICHT votiert: eine „ich habe
> kompaktiert"-Signatur wäre unbeweisbar, ein Block über flüchtigen
> Gerätezustand verletzte die Ephemeral-Grenze, und Offline-Geräte
> änderten am Ergebnis nichts. Die Threshold-Hoheit liegt in der POLICY
> (`set_chat_retention`, gated Vote) und im Checkpoint (WP4b).

---

# Teil A — WP4a: Physisches Pruning des flüchtigen Logs

## A.1 Auftrag, Constraints

Wie im Ursprungs-Entwurf: Nach **Ablauf + Karenz** existieren Inhalt und
Metadaten abgelaufener Chats/Shares lokal wirklich nicht mehr.
Constraints C1–C5 unverändert:

- **C1** `seq` wird NIE renummeriert (Legacy-Id-Keystones; Einträge fallen
  weg, Lücken bleiben).
- **C2** Peer-Cursor < Kompaktierungs-Floor ⇒ Umleitung auf die
  WP2-Catch-up-Schiene (`ChainRequest` → `serve_chain_from` +
  `serve_open_governance`), nie Log-Replay auf gelöschtem Terrain.
- **C3** Chain-Blöcke, Genesis, Roster: niemals von WP4a angefasst
  (WP4b hat dafür einen EIGENEN, votierten Mechanismus).
- **C4** User-Dateien tabu; nur `prefs.shared_files`-Einträge fallen.
- **C5** Lokale Hygiene, nichts kreuzt die Wire.

Kernidee unverändert: **Kompaktierung = Snapshot-Vorziehen** (gestutzter
`EngineStateDump` als neue Basis) **+ Drop ganzer Segmente** unter dem
Floor. Kein Byte-Rewrite einzelner Einträge, kein Rewrite teilbetroffener
Segmente. Floor-Regeln:

- **R1 Snapshot-Deckung:** Floor ≤ `at_seq` eines gültigen Snapshots.
- **R2 Outbox-Deckung:** min über Peer-Delivery-Cursor; ein toter Peer
  hält den Floor nur bis zur Peer-Karenz (F4), danach Umleitung (C2).
- **R3 Retention + Karenz:** gestutzt wird nur
  `ts + Fenster + Karenz < now`; `ts 0` (unbekanntes Alter) nie.

## A.2 Entscheidungen (2026-07-17, User)

- **F1 — Rotation: nur größengesteuert** (8 MiB wie heute, keine
  Zwangsrotation). Akzeptierte Konsequenz: das offene Segment kompaktiert
  erst nach Rotation — Rest-Exposition ist auf ~8 MiB pro Workspace
  begrenzt; stille Republiken behalten diesen Bodensatz länger.
- **F2 — Inhalts-Karenz = 1× Retention-Fenster** (physisch weg nach 2×
  Fenster).
- **F3 — Segment-Schlüssel SOFORT** (kein Unlink-only-v1): echte
  Krypto-Erasure ab der ersten Ausbaustufe. Design §A.3.
- **F4 — Peer-Karenz = 2× Retention-Fenster** (doppelt so lang wie die
  Inhalts-Karenz), danach Cursor-Umleitung auf Catch-up.

Vorgaben aus der offenen F6–F9-Runde (werden so gebaut, solange kein
Widerspruch kommt):

- **F6 — Migration:** Alt-Segmente (unter dem Workspace-Key) werden beim
  ERSTEN Kompaktorlauf einmalig auf Segment-Keys umgeschrieben
  (Copy-then-Swap) — danach überall Key-Erasure, keine zwei
  Lösch-Klassen.
- **F7 — Backups:** Kein aktives Durchgreifen auf S3-Kopien; alte Kopien
  altern über `s3_keep_copies` aus. Die Karenz bis dahin wird im Doc +
  in der Settings-Hilfe ehrlich benannt.
- **F8 — Trigger:** 1×/Tag pro offenem Workspace (Ticker) + einmal beim
  Clean-Close (Snapshot und Cursor sind dort frisch).
- **F9 — Beweisführung:** Der Byte-Fixture-Beweis „gestutzter Snapshot +
  Tail-Replay hält die Legacy-Id-Keystones" ist der ERSTE Commit der
  WP4a-Implementierung (rot = Design-Stopp), vor jedem Kompaktor-Code.

## A.3 Segment-Schlüssel (F3-Design)

- Pro Segment ein zufälliger Data-Key (DEK); die Frames eines Segments
  verschlüsseln unter seinem DEK statt direkt unter dem Workspace-Key.
- DEKs liegen, mit dem Workspace-Key gewrappt, in einer **Key-Tabelle**
  (`log/keys.state`), die bei JEDER Änderung komplett neu geschrieben
  wird (Tempfile + fsync + rename — dasselbe atomare Muster wie
  `write_snapshot`).
- **Löschen = Segment-Datei unlinken UND den DEK aus der Tabelle
  entfernen.** Ohne DEK sind forensisch wiederhergestellte Segment-Bytes
  wertlos.
- **Ehrliche Grenzen (dokumentierte Restrisiken):** (1) auf Flash/
  Journaling-FS können ALTE Versionen der kleinen Key-Tabelle forensisch
  überleben — harte Garantie liefert erst TRIM/Hardware-Verschlüsselung;
  (2) alte S3-Backup-Kopien enthalten gelöschte Inhalte, bis
  `s3_keep_copies` sie verdrängt (F7).
- Format-Version im Tabellen-Header; ein älterer Reader ohne
  Tabellen-Wissen darf das Workspace nicht schreibend öffnen
  (additive-only-Regel sinngemäß auf Storage-Ebene).

## A.4 Crash-Sicherheit

Reihenfolge je Lauf, jede Stufe idempotent wiederholbar: (1) gestutzten
Snapshot als Tempfile schreiben + rename, (2) `compaction_floor` im
Manifest nachziehen, (3) erst dann Segmente unlinken + Key-Tabelle neu
schreiben. Crash zwischen Stufen: verwaiste Segmente unter dem Floor
löscht der nächste Lauf; die Gegenrichtung (Floor über noch nötigem
Terrain) kann nie entstehen.

## A.5 Etappen WP4a

1. ✅ dieses Doc (entschieden 2026-07-17).
2. Keystone-Beweis (F9) + molt-storage-Primitive: Segment-Keys +
   Key-Tabelle (F3/F6), atomarer Snapshot-Swap, Segment-Drop, Floor im
   Manifest.
3. Engine-Kompaktor (Ticker + Clean-Close, F8; sync Handler entscheidet,
   Off-Actor-Task arbeitet, internes Command auf der INTERNAL-Liste),
   Cursor-Umleitung (C2/F4).
4. Share-Vergessen (`shared_files` fällt beim Stutzen; Download danach ⇒
   vorhandenes ehrliches `Refused`/`FileExpired`).
5. Doku-Abschluss (chat_bus.md-Follow-ups, Plan-Doc).

---

# Teil B — WP4b: Chain-Checkpoint (m-of-n-signierte Kompaktierung)

## B.1 Idee und Vertrag

Der aus der Chain projizierte Zustand ist bei allen Nodes byte-identisch
(harte Sequenzierung: `height`-Counter + `prev`-Hash-Link +
positionsgebundene Signaturen). Also kann die Republik ihn
**kompaktieren, wie sie regiert**: Ein Checkpoint friert den
projizierten Zustand bei Höhe `upto` ein; m-of-n signieren die
**Checksum dieses Zustands** — m Teilnehmer bestätigen die *Korrektheit
der Kompaktierung*, nicht ihren Vollzug. Danach darf jeder Node die
Blöcke ≤ `upto` lokal droppen; Newcomer/Rejoiner bootstrappen aus
**Checkpoint + Suffix** statt ab Block 0.

Erreicht: begrenztes Chain-Wachstum, billigere Recovery (heute reist die
ganze Chain im Welcome), und die eine Stelle, an der Kompaktierung
„offiziell" wird — chain-verankert, auditierbar, positionsgebunden.

## B.2 Neues Chain-Vokabular (additiv)

```rust
// molt-core ChainChange, additive Variante (unknown ⇒ Reader stoppt, wie gehabt)
Checkpoint {
    /// Der letzte eingefaltete Block: der Checkpoint bezeugt den
    /// Zustand NACH Anwendung von Block `upto`.
    upto: u64,
    /// sha256 (lowercase hex) über checkpoint_canonical_bytes(state@upto).
    state_hash: String,
}
```

Der Checkpoint-Block sitzt an Höhe H mit **`upto` = H − 1, hart erzwungen**
(Review-Finding 2026-07-18: ein kleineres `upto` ließe Blöcke in
(`upto`, H) entstehen, die weder Blob noch Suffix tragen — ihre applied
Ids entkämen dem Double-Apply-Guard, ihre Membership-Änderungen dem
Roster; ein re-based Checkpoint-Proposal muss deshalb NEU schneiden:
Zustand + Hash an der neuen Head-Höhe rekomputieren, nicht nur die
Signaturhöhe wechseln). Signiert wird mit
normalen positionsgebundenen Signaturen über
`republic_id ‖ H ‖ change` (`approval_bytes` bekommt nur die neue
Varianten-Serialisierung — **kein zweiter Signierpfad**, Genesis-Regel
sinngemäß).

## B.3 Kanonische Zustands-Serialisierung (`molt-chain-checkpoint-v1`)

Deterministische Byte-Folge, längenpräfixierte Felder (dasselbe Framing
wie `roster_canonical_bytes` — Geschwister-Layout, eigener
Versions-Tag). Inhalt in fester Reihenfolge:

1. Tag `molt-chain-checkpoint-v1\0`.
2. `republic_id`.
3. **Founding-Tabelle**: Name, `rule_m`, `rule_n`, GENESIS-Identities in
   Gründungsreihenfolge, Agenda — damit jeder Verifier `republic_id`
   aus dem Inhalt REKOMPUTIEREN kann (Fälschungsschutz wie beim
   Genesis-Check, §B.5).
4. **Aktueller Roster** nach allen Membership-Blöcken ≤ `upto`:
   `(member, identity_pk)` in Chain-Reihenfolge (deterministisch, weil
   die Blockfolge total geordnet ist).
5. **Applied-Projektion** pro Surface, Surfaces in
   `Surface::ALL`-Reihenfolge; je Surface die Liste
   `(proposal_id, payload_canonical_json)` in Block-Reihenfolge.
   `payload_canonical_json` = serde_json mit sortierten Map-Keys
   (Default-BTreeMap; ein Test PINNT, dass `preserve_order` im Workspace
   nirgends aktiv ist).
6. **Verbrauchte Proposal-Ids**: sortierte Liste aller in Blöcken
   ≤ `upto` applied Ids — Seed für den Double-Apply-Guard eines
   Suffix-Verifiers (§B.5).
7. `upto`.

Gleiche Chain ⇒ gleiche Bytes ⇒ gleicher Hash, auf jedem Node. Im Block
steht NUR der Hash; der **Zustands-Blob reist außerhalb der Chain**
(lokal neben `chain.state` persistiert, bei Catch-up/Recovery
mitgeliefert und gegen `state_hash` verifiziert) — Blöcke bleiben klein.

## B.4 Ablauf (Sign-what-you-see, das Membership-Muster)

1. **Propose:** menschliches Verb `propose_checkpoint { upto }` — MCP-Tool
   UND GUI, co-equal; Default-`upto` = aktuelle Head-Höhe. Der Proposer
   berechnet `state_hash` aus der eigenen Chain und announced
   `WorkspaceEvent::CheckpointProposed { id, upto, state_hash }`
   (additiv; der Empfangs-Arm ist INTERNAL, wie `MembershipProposed`).
2. **Verify vor Sign:** Jeder Empfänger rekomputiert die kanonischen
   Bytes aus der EIGENEN Chain@`upto` und vergleicht. Ungleich ⇒ nicht
   signieren (+ WARN) — niemand signiert einen fremden Blob. Gleich ⇒
   `chain_sign_and_gossip_approval`, positionsgebunden auf Zielhöhe H.
3. **Seal:** m distinct Signaturen ⇒ deterministischer Block
   (`try_commit`-Pfad; `proposal_changes` trägt die Checkpoint-Change wie
   bei Membership).
4. **Nach dem Commit:** Jeder Node persistiert den (verifizierten) Blob
   und droppt Blöcke ≤ `upto` lokal (Vorgabe B-F2: automatisch — der
   Vote hat die Korrektheit bestätigt). `chain.state` wird zu
   `[Blob] + [Checkpoint-Block @H] + Suffix`.
5. **Offene Proposals über den Schnitt:** unverändert — ein Checkpoint
   ist ein normaler Block; pending Signaturen re-basen über die
   bestehende Mechanik auf die neue Head-Höhe.

## B.5 Verifikation (`verify_chain`-Erweiterung)

- **Voll-Historien-Halter** (Chain beginnt mit Genesis): wie heute,
  PLUS: ein Checkpoint-Block wird inhaltlich geprüft (eigene
  Projektion@`upto` ⇒ Hash-Gleichheit), sonst hard-reject —
  alles-oder-nichts wie jede andere Verletzung.
- **Suffix-Halter** (Chain beginnt mit einem Checkpoint-Block): neuer
  Vertrauensanker, Pflicht-Checks alles-oder-nichts:
  1. Blob-Hash == `state_hash` im Block.
  2. `republic_id`-Rekomputation aus der Founding-Tabelle im Blob ==
     erwartete Id (aus Invite/Recovery-Link) — Founding fälschen ändert
     die Id, exakt der Genesis-Schutz.
  3. **Kein zirkuläres Vertrauen** (Review-Finding 2026-07-18): Der
     Blob-Roster ist nur durch den Hash gebunden, den die Anker-Signaturen
     selbst attestieren — deshalb verifizieren die Anker-Signaturen gegen
     die **Founding-Identities** (rid-gebunden), und jeder Roster-Eintrag
     muss wörtlich in der Founding-Tabelle stehen (Sitze sind ab Gründung
     fix; Restored behält den verankerten Key). Fälschung erfordert damit
     m ECHTE Gründungs-Keys — die Honest-Majority-Annahme, nicht weniger.
     ≥ m distinct, m aus der Founding-Tabelle.
  4. Suffix ab H normal (prev-Links, Höhen, Signaturen); Double-Apply-
     Guard geseedet mit der Id-Liste aus dem Blob.
- **Explizites Vertrauensmodell (dokumentierte Änderung):** Ein
  Suffix-Bootstrapper prüft die Roster-Evolution unterhalb `upto` nicht
  mehr Block für Block — er vertraut m-of-n des aktuellen Rosters, dass
  der Blob diese Evolution korrekt zusammenfasst. Das ist dieselbe
  Honest-Majority-Annahme, auf der die Threshold-Governance ohnehin
  steht. Wer das nicht will, behält die volle Historie — Droppen ist
  Erlaubnis, nie Pflicht (C5-Geist).

## B.6 Catch-up, Recovery, Persistenz

- `serve_chain_from(from)`: hält der Server nur noch den Suffix und ist
  `from ≤ upto`, antwortet er mit `[Blob + Checkpoint-Block + Suffix]`
  (additives Wire-Detail: eigenes Event `CheckpointServed { blob }` oder
  Blob-Feld am re-served Committed — Entscheid in der Implementierung,
  beides additiv).
- Recovery: das Welcome trägt Checkpoint-Blob + Suffix statt der vollen
  Chain; der Rejoiner verifiziert nach den Suffix-Regeln (§B.5).
- Persistenz: `chain.state` additiv um den Blob erweitert; ältere Leser
  treffen die unbekannte `ChainChange`-Variante und stoppen — gewollt
  (additive-only-Regel).

## B.7 Grenzen (bewusst)

- Der flüchtige Log (Chat/Shares) bekommt NIE eine gemeinsame Checksum:
  ephemere Logs sind zwischen Nodes nicht identisch (Q4-Rejoiner ohne
  Backfill, Ankunftsreihenfolgen, lokale Uhren). Dafür bleibt WP4a
  zuständig — lokal, Policy-vollziehend.
- Checkpoint-Auslösung ist Governance-Sache (menschliches Verb); keine
  Automatik in v1.

## B.8 Etappen WP4b (je einzeln mergebar, TDD)

1. `checkpoint_canonical_bytes` + Hash in molt-core (rote Tests:
   Determinismus über zwei unabhängig gebaute Chains, Versions-Tag,
   republic_id-Rekomputation, sorted-JSON-Pin).
2. `verify_chain`-Erweiterung: Checkpoint-Prüfung (Voll-Halter) +
   Suffix-Anker (rote Tests: geforgter Roster, geforgte Founding-Tabelle,
   Hash-Mismatch, Double-Apply über den Schnitt, unterschwelliger m).
3. Engine-Fluss: `propose_checkpoint` (Command + MCP-Tool, co-equal),
   `CheckpointProposed`-Wire-Arm (INTERNAL-Liste), Verify-vor-Sign, Seal.
4. Drop + Serve: lokales Droppen ≤ `upto`, Catch-up mit Blob, Recovery
   über Checkpoint (Loopback-E2E).
5. Doku (persistent_chain.md-Abschnitt; „chain compaction" von der
   Deferred-Liste nehmen).

Aus dem Etappe-2-Review offen für Etappe 3/4 (gepinnt, nicht vergessen):
- `after_block_applied` braucht einen Checkpoint-Arm (Event emittieren,
  `proposal_changes`/`pending_sigs` des Checkpoint-Proposals räumen).
- Etappe 4 muss ALLE `verify_chain`-Aufrufer (adopt_chain,
  append_committed_block, apply_next_block, tie_break, Open/Recovery) auf
  Suffix-Chains routen — heute würde `adopt_chain` eine gedroppte Chain
  beim Reopen löschen.
- Verify ist heute O(n²) bei vielen Checkpoints (Recompute ab
  Genesis/Blob pro Checkpoint pro Lauf); mit automatischem Droppen
  bleiben Chains kurz, aber der inkrementelle Walk (ein gemeinsamer
  Walker für Voll/Suffix, Zustand läuft mit) ist die richtige Form —
  spätestens in Etappe 4 umbauen.
- Proposal-Id-Namespace ist knoten-lokal gemintet (Kollision zweier
  gleichzeitiger Proposals verschiedener Nodes ist heute schon möglich,
  auch ohne Checkpoints) — bei Etappe 3 dokumentieren/entschärfen.

## B.9 Vorgaben (einspruchsfähig)

- **B-F1:** `upto` beim Proposen = aktuelle Head-Höhe (kein frei
  wählbarer Schnitt in v1).
- **B-F2:** Lokales Droppen ≤ `upto` automatisch nach dem Commit (der
  Vote hat die Korrektheit bestätigt); kein separates Verb.
