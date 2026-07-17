# Log-Kompaktierung: physisches Pruning („weg ist weg")

> **STATUS: ENTWURF ZUR DISKUSSION (2026-07-17) — WP4 Etappe 1.**
> Kein Code, bevor die offenen Fragen (§8) mit dem User entschieden sind
> (`documents/plan_governance_followups.md`, WP4; Arbeitsregel
> „concept docs: discuss before code"). Die §§1–7 sind der Vorschlag.

## 1. Auftrag und Grenze

„Delete chat after N days" ist heute ein **Read-Filter**
(`chat_view_admits` / `aged_out_at`): Abgelaufenes verschwindet aus jedem
Read (Chat, Uploads, Zähler), aber die Bytes bleiben verschlüsselt im
lokalen Log liegen, und `prefs.shared_files`-Einträge abgelaufener Shares
bleiben in den Prefs. Ziel: Nach **Ablauf + Karenz** existieren Inhalt und
Metadaten abgelaufener Chats/Shares **lokal wirklich nicht mehr**.

Nicht verhandelbare Constraints (aus `documents/chat_bus.md` §Follow-ups
und dem WP4-Plan):

- **C1 — `seq` wird NIE renummeriert.** Die synthetischen Legacy-Ids
  hängen an Positionen/Ordinalen der Replay-Reihenfolge
  (`molt-chat-legacy-id`, beide Choke-Points `apply`-Chat-Arm +
  `restore_dump`). Kompaktierung lässt Einträge **wegfallen**; Lücken in
  `seq` sind normal und bleiben.
- **C2 — Replay-Floor + Outbox-Cursor.** Die log-backed Outbox liest
  `read_from(from_seq)`; Peer-Cursor (in `transport.state`) dürfen nie auf
  gelöschtes Terrain zeigen. Cursor < Kompaktierungs-Floor ⇒ der Peer wird
  auf Chain-Catch-up umgeleitet (die WP2-Schiene: `ChainRequest` →
  `serve_chain_from` + `serve_open_governance`), nie auf Log-Replay.
- **C3 — Chain-Blöcke sind heilig.** Kompaktiert wird nur der flüchtige
  Event-Log (Chat/Shares/Koordinationsframes). `chain.state`, Genesis,
  Roster, Snapshots der Chain: niemals.
- **C4 — User-Dateien sind tabu.** Pruning eines Shares löscht den
  `prefs.shared_files`-Eintrag; NIE die Quelldatei des Teilenden, nie
  heruntergeladene Kopien.
- **C5 — Lokal & deterministisch-unkritisch.** Kompaktierung ist lokale
  Hygiene, KEIN konvergenzrelevantes Ereignis; nichts davon kreuzt die
  Wire. Zwei Nodes dürfen zu verschiedenen Zeiten (oder nie) kompaktieren.

## 2. Ist-Zustand (Anker)

- Log = rotierende, verschlüsselte Segmente `log/000001.mlog`, Rotation
  bei 8 MiB (`SEGMENT_ROTATE_BYTES`); Frames tragen `(seg_no, seq)`.
- Snapshots alle 1000 Events (`SNAPSHOT_EVERY`, `WorkspaceSnapshot` =
  `EngineStateDump` bei `at_seq`); Öffnen = Snapshot + Tail-Replay.
  Snapshots sind Optimierung — löschen ist immer erlaubt.
- Delivery-Cursor pro Peer in `transport.state` (clean-close persistiert).
- Reads filtern bereits: `chat_view_admits` (Fenster + Hälften),
  Upload-Expiry (`FileExpired`, ehrliches `Refused` beim Serve).

## 3. Kernidee: Kompaktierung = Snapshot-Vorziehen, nicht Byte-Chirurgie

Der Log ist schon heute „Snapshot + Tail". Kompaktierung nutzt genau das:

1. **Floor bestimmen.** `floor_seq` = höchstes `seq`, dessen Event
   *sicher entbehrlich* ist (§4). Es gilt immer
   `floor_seq ≤ at_seq` eines existierenden Snapshots — der Snapshot IST
   der Ersatz für alles darunter.
2. **Snapshot als neue Basis.** Der aktuelle Snapshot (der den Zustand bis
   `at_seq` trägt) wird zum **Basis-Snapshot** erklärt. Sein
   `EngineStateDump.chat` wird dabei **gestutzt**: abgelaufene Nachrichten
   (Ablauf + Karenz, §4) fallen aus dem Dump; ihre `shared_files`-Einträge
   fallen aus den Prefs (C4). Der gestutzte Dump wird neu geschrieben.
3. **Segmente droppen.** Ganze Segmente, deren höchstes `seq ≤ floor_seq`
   ist, werden gelöscht (Datei weg = Bytes weg; Krypto-Erasure durch
   Dateisystem-Unlink + verschlüsselte Frames — ohne Segment-Key-Rotation,
   §8 F3). Teilbetroffene Segmente bleiben stehen, bis die Rotation sie
   unter den Floor schiebt — **kein** Rewrite einzelner Segmente nötig.
4. **Floor persistieren.** `compaction_floor = floor_seq` wandert als
   additives Feld in den Manifest-/Snapshot-Header. Ein Open, dessen
   Snapshot fehlt/älter als der Floor ist, bricht ehrlich ab (korrupter
   Zustand), statt mit Lücke zu replayen.

Warum kein Segment-Rewrite (Copy-then-Swap einzelner Einträge)? C1 erlaubt
Wegfallen, aber der Replay-Determinismus verlangt: Snapshot(at_seq) +
Tail(at_seq+1..) == voller Replay. Einträge MITTEN im Tail zu löschen
bricht genau das (der Chat-Arm zählt Ordinale über die gesehene
Reihenfolge). Vor dem Snapshot ist der Log ohnehin tot — also ist die
einzige sichere Löschgrenze „alles unter einem Snapshot", und die gibt es
segmentweise gratis. Der Preis: Granularität = Segment (8 MiB) bzw.
Snapshot-Kadenz; akzeptabel für „Hygiene", nicht Echtzeit (§8 F1).

## 4. Was ist „sicher entbehrlich"? (Floor-Regeln)

`floor_seq` = min über:

- **R1 — Snapshot-Deckung:** höchstes `at_seq` eines gültigen Snapshots
  (darunter braucht Replay den Log nicht mehr).
- **R2 — Outbox-Deckung:** min über alle Peer-Delivery-Cursor. Ein Peer,
  der noch nicht bestätigt hat, hält den Floor — bis zur Karenzgrenze
  (§5): danach wird sein Cursor auf „chain-catch-up nötig" markiert (C2)
  und hält den Floor nicht mehr. (Ein monatelang toter Peer darf die
  Hygiene nicht ewig blockieren; er bekommt Blöcke + offenen Zustand über
  WP2, verliert aber den ephemeren Chat — genau die dokumentierte
  Rejoiner-Semantik Q4.)
- **R3 — Retention + Karenz für den *Inhalt*:** Das Dump-Stutzen (§3.2)
  entfernt nur Nachrichten mit
  `ts + Retention-Fenster + Karenz < now`. Karenz-Vorschlag: **1×
  Retention-Fenster** (d. h. physisch weg nach 2× Fenster) — damit
  Boundary-Races der Reads (Archiv-Hälfte, `ts 0`-Sonderfälle,
  Clock-Skew zwischen Peers) nie sichtbar werden. `ts 0` (unbekanntes
  Alter) wird NIE gestutzt.

## 5. Cursor-Umleitung (C2, die eine Wire-sichtbare Folge)

Der Kompaktor selbst sendet nichts. Aber wenn ein Peer mit
`cursor < compaction_floor` wieder auftaucht, darf die Outbox nicht von
seinem Cursor lesen (Terrain gelöscht). Verhalten:

- Outbox-Start für diesen Peer springt auf `floor_seq + 1` (statt Fehler).
- Der Peer merkt selbst, dass ihm Blöcke fehlen (Chain-Gap) und zieht
  Chain + offenen Governance-Zustand über die WP2-Schiene. Ephemerer Chat
  dazwischen ist für ihn verloren — by design (Q4, „Ephemeralität ist das
  Produkt").
- Kein neues Wire-Vokabular; nur die lokale Lesegrenze ändert sich.

## 6. Crash-Sicherheit (Etappe 2, molt-storage-Primitiv)

Reihenfolge im Kompaktor-Schritt (jede Stufe idempotent wiederholbar):

1. Gestutzten Snapshot als Tempfile schreiben, fsync, **rename** über den
   alten (atomar; Copy-then-Swap).
2. `compaction_floor` im Manifest nachziehen (gleiches
   Tempfile+rename-Muster wie `write_snapshot` heute).
3. Erst DANACH Segmente unlinken (Crash dazwischen: verwaiste Segmente
   unter dem Floor werden beim nächsten Lauf/Open erneut gelöscht —
   harmlos; die Gegenrichtung — Floor zeigt über noch nötige Segmente —
   kann nie entstehen).

Test-Pflichten: Crash zwischen jedem Paar von Stufen ⇒ Open liefert
denselben Zustand; Byte-Fixtures der Chat-Surface (molt-core) bleiben
grün; Reads identisch vor/nach Kompaktierung; Peer mit altem Cursor
konvergiert via Catch-up (Loopback-E2E).

## 7. Engine-Einbettung (Etappe 3)

- Ticker-getrieben (Muster der bestehenden Ticker): sync Handler entscheidet
  „Kompaktierung fällig?" (Floor-Regeln sind reine Funktionen über
  Snapshot-Metadaten + Cursors + Uhr), Off-Actor-Task macht die
  Datei-Arbeit, Ergebnis kommt als engine-internes Command zurück
  (INTERNAL-Liste, Co-Equality-Test).
- Frequenz: 1×/Tag pro offenem Workspace reicht (Hygiene); zusätzlich
  einmal beim Clean-Close (dann ist der Snapshot frisch und die Cursors
  persistiert — der natürliche Kompaktierungsmoment).
- Share-Vergessen (Etappe 4): das Dump-Stutzen entfernt die
  `shared_files`-Einträge; ein Download danach läuft ins vorhandene
  ehrliche `Refused`/`FileExpired`.

## 8. Offene Fragen (Produktentscheidungen, vor Etappe 2 klären)

- **F1 — Granularität akzeptiert?** Physisch weg heißt: erst wenn ein
  Snapshot + Segmentgrenze das Terrain deckt (bei ruhigen Republiken kann
  ein 8-MiB-Segment lange offen bleiben). Alternative: Rotation zusätzlich
  zeitgesteuert erzwingen (z. B. 1×/Woche), damit stille Workspaces
  überhaupt kompaktierbar werden. Empfehlung: ja, zeitgesteuerte Rotation
  dazu.
- **F2 — Karenz = 1× Retention-Fenster?** (Physisch weg nach 2× Fenster.)
  Alternativen: fix (z. B. 7 Tage) oder 0 (sofort nach Fenster).
  Empfehlung: 1× Fenster, ein Knopf weniger.
- **F3 — Krypto-Erasure-Anspruch?** Unlink genügt gegen „normales"
  Auslesen, nicht gegen Forensik auf dem Datenträger. Echte Erasure
  bräuchte Segment-Schlüssel-Hierarchie (Key pro Segment, Löschen =
  Schlüssel vergessen) — deutlicher Umbau von molt-storage. Empfehlung:
  v1 = Unlink, Segment-Keys als dokumentiertes Später (eigenes Konzept).
- **F4 — Peer-Karenz (R2):** Wie lange hält ein toter Peer den Floor?
  Vorschlag: gleiche Karenz wie F2 (1× Fenster). Danach Umleitung auf
  Catch-up (er verliert nur Ephemeres — Q4-Posture).
- **F5 — Snapshot-Stutzung vs. `EngineStateDump`-Shape:** Das Stutzen
  ändert nur Inhalte, nie das Schema (additiv bleibt additiv). Aber: ein
  gestutzter Dump + Tail-Replay muss die Legacy-Id-Keystones halten —
  der Beweis gehört als Byte-Fixture-Test in Etappe 2, BEVOR der Kompaktor
  entsteht. Einverstanden?

## 9. Etappen (aus dem WP4-Plan, unverändert)

1. dieses Doc + Diskussion (**hier stehen wir**),
2. molt-storage-Primitiv (atomarer Snapshot-Swap + Segment-Drop + Floor),
3. Engine-Kompaktor (Ticker, INTERNAL-Command, Cursor-Umleitung),
4. Share-Vergessen,
5. Doku-Abschluss (chat_bus.md-Follow-ups, Plan-Doc erledigt markieren).
