# Konzept: Zustellgarantie für Mesh-Nachrichten (at-least-once, Ende-zu-Ende)

> **Scope note (2026-07-30, etappe N-demo):** the SMP transport was removed in
> the Nostr transport replacement (`docs/transport/nostr_transport_marmot.md`).
> The delivery-guarantee CORE this document designs — `AcceptedWindow` dedup,
> `MESH_ACK_TAG` frames, the log-position acked floor, rewind-resend with
> fresh encryptions, the live-ratchet persist, G7 in-order delivery (§3,
> §4.1–4.8, §9) — stays load-bearing over the loopback transport and will
> carry the Nostr runtime in N5. The §4.9 V1/V8 fixes ALSO survive: they live
> in the mesh-extension path (`net.rs`), which stays live for recovery until
> N4/N5, and the hard-kill keystone (§5 item 8) still runs over the loopback
> `expire_queue` seam. Historical are only the SMP-specific triggers: the
> Stage-B/rotate rebuild machinery and the server-queue mechanics behind the
> V1/V2-rotation/V8 loss paths in §2, replaced by the relay equivalents in N5.

Status: **GEBAUT + REVIEWED 2026-07-28, E1–E7 auf master** (E7: zwei
unabhängige adversariale Review-Läufe, 7 Findings gefixt — §8). Umsetzungs-Verfeinerungen gegenüber
dem Erstentwurf sind inline markiert („BUILT, verfeinert"); die wichtigsten:
ACK-Konsumption im Supervisor statt per Command (§4.3), Dup-getriggerte
Re-ACKs statt Keepalive-Huckepack (§4.3), und der Live-Ratchet-Persist
(MLS_PERSIST_SECS=10) hängt an `record()` UND am Presence-Tick — der Tick
allein ist ein 30-s-Beat, ein Hard-Kill dazwischen hätte den Ratchet um einen
ganzen Burst regressiert. E2E-Keystones:
`crates/molt-engine/tests/delivery_guarantee.rs` (Clean-Close- und
Hard-Kill-Variante, beide als echte Pins verifiziert: rot ohne
Rewind/Persist, grün mit).

Ursprünglich: BESCHLOSSEN 2026-07-28 (User-Auftrag im 3-Node-Livetest, Nodes
`classic`/`dark`/`brutal`): *„Ich will eine Garantie, dass alle Nachrichten an
alle verschickt werden — Chatnachrichten sollen gequeued werden und dann an den
jeweiligen Client geschickt, sobald er wieder erreichbar ist. Das gilt für alle
Arten von Nachrichten, auch Vote-Proposals, Votes etc. — generell immer für
alle Nachrichten."* Ausführungsreif, test-first. Vorher lesen:
`docs_archive/transport/mesh/stage_b.md`, `docs_archive/transport/mesh/mesh_selfheal.md`,
`docs_archive/transport/mesh/mesh_rotation_trackc.md`, `docs/chain/log_compaction.md` und den
Transport-Abschnitt in `CLAUDE.md` — dieses Konzept baut direkt auf deren
Maschinerie und ändert **nichts** an Ritual, Chain oder Roster.

## 0. Auftrag, Produktentscheidung, Abgrenzung

**Garantie (neu):** Jedes Wire-Event, das ein Mitglied authored (Chat,
Reaktionen, Deletes, ChatRead, FileRemoved, Proposed/Approved/Declined,
Committed, Checkpoint-Gossip — alles in `crosses_wire`,
`molt-engine/src/net.rs:495`), erreicht **jeden aktuellen Sitz** der Republik
mindestens einmal (at-least-once, Empfänger dedupliziert), solange der
Empfänger innerhalb des **Garantie-Horizonts** zurückkehrt. Der Horizont ist
die bestehende WP4a-Peer-Karenz (2 × Retention-Window, Default 14 Tage,
`compaction.rs:66-75`) — dieselbe Grenze, die schon heute bestimmt, wie lange
ein abwesender Peer den Log hält. Jenseits davon gilt weiterhin die ehrliche
Lücke (bounded storage schlägt unbounded Queue).

**Was das NICHT umkehrt:** `chat_bus.md` Q4 („kein History-Backfill") bleibt
für **Total-Verlust-Rejoiner** (Recovery-Ritual, neues Gerät) bestehen: die
Garantie ist *Sender-seitig* — jeder Sender liefert seine **eigenen**
unbestätigten Events nach. Niemand rekonstruiert fremde Historie, nichts wird
über Dritte relayed, ACKs landen nie im Log. Ein Recovery-Rejoiner bekommt wie
bisher Chain, aber keine fremde Chat-Historie; was Peers ihm aber noch
**schulden** (deren unbestätigte eigene Events innerhalb des Horizonts),
bekommt er nach dem Re-Key nachgeliefert, soweit die neuen Epochen-Schlüssel
das Entschlüsseln erlauben (Resends werden ohnehin frisch verschlüsselt, §4.5).

## 1. Ist-Zustand: was schon trägt, mit Ankern

- **Die Outbox ist bereits eine persistente Per-Peer-Queue.** Der
  verschlüsselte Workspace-Log IST das Sendejournal (`StorageLog`,
  `molt-engine/src/net.rs:441-457`); pro Peer läuft ein `outbox_task`
  (`molt-net/src/supervisor.rs:658-713`), der ab persistiertem Cursor
  (`TransportState.outbound`, `molt-core/src/lib.rs:1466`) liest und **eigene**
  Events (`env.by == me`, supervisor.rs:694) sendet. Transportfehler werden
  endlos mit Jitter-Backoff retried (`send_one`, supervisor.rs:786-820) —
  auf dieser Ebene geht nichts verloren.
- **SMP-Queues sind serverseitig store-and-forward**: ein Peer, der offline
  ist, dessen Queue aber lebt, bekommt alles beim Resubscribe (Stage B).
- **Der Cursor rückt bei Server-Accept vor** (`advance_outbound`,
  supervisor.rs:704, 837-852) — „der Server hat's" ist NICHT „der Peer hat's".
  Das ist die Wurzel der strukturellen Verluste (§2 V2/V3).
- **Empfangsseitig gibt es auf dem MLS-Pfad keine per-Sender-Buchführung**:
  `env.seq` des Senders wird nie gelesen, `TransportState.inbound` nie
  fortgeschrieben (supervisor.rs:1222-1232, der `continue` bei :1303);
  Ordnung ist „MLS's job". Dedup existiert nur pro Event-Art (Chat per
  `MessageId`, net.rs:1007-1029; MeshAnnounce per Nonce, net.rs:1190-1201) —
  Reaktionen/Votes verlassen sich auf idempotente Applier.
- **MLS-Fenster sind unkonfigurierte OpenMLS-Defaults**: `out_of_order_tolerance
  = 5`, `maximum_forward_distance = 1000`, `max_past_epochs = 0`
  (`molt-net/src/mls.rs:179-191`, :306; openmls 0.8.1 Defaults).
- **Ciphertext-Cache**: ein MLS-Ciphertext pro Log-Seq, geteilt über alle
  Peers, nie evicted, Lebensdauer = eine Supervisor-Inkarnation
  (supervisor.rs:57, 97-116; `MlsChannel::from_shared` beginnt leer).
- **msg_id ist auf `(me, peer, log_seq)` gekeyt** (supervisor.rs:745) — ein
  Resend trägt dieselbe id wie das Original.
- **Teardown = Abort**: `teardown_net` droppt das `JoinSet` (supervisor.rs:598),
  Cursor-Fortschritt des laufenden Batches geht verloren → nächste Inkarnation
  resendet ab Cursor (Dups, kein Verlust — der Empfänger-Dedup muss das aber
  wirklich tragen, §2 V4).

## 2. Verlustpfade (alle verifiziert, mit Ankern)

- **V1 — Cooldown-Burn beim Rotate-Broadcast** *(der Live-Log vom 2026-07-27)*:
  `spawn_mesh_extension` stempelt den 60-s-Cooldown (`mesh_extension_at.insert`,
  net.rs:1572) BEVOR `send_targets` prüft, ob der Announce überhaupt eine Queue
  für uns trägt (net.rs:1578). Ein Stage-3-Rotate-Announce wird an ALLE
  gebroadcastet (net.rs:1182-1216), trägt aber nur Queues für den einen
  Ziel-Peer. Folge: brutals Announce-für-dark verbrennt bei classic den Slot
  („carries no usable queue"), brutals Announce-für-classic 300 ms später wird
  „inside the cooldown" verworfen, classic adoptiert nie, brutals Rotate läuft
  in den 20-s-Timeout, die Leg bleibt pro Zyklus ≥ 60 s länger taub — und in
  diesem Fenster füttert classic brutals tote Queue.
- **V2 — Accepted ≠ Delivered + Rotation verwaist Queues**: Eine Queue, die
  serverseitig lebt, deren Subscriber aber taub ist, frisst SENDs (Cursor
  rückt vor). Rotiert der Peer danach auf frische Queues, wird die alte Leg
  nur aus dem Mesh-Vec entfernt (net.rs:1719-1721), ihr Inhalt ist weg — der
  Sender hält alles für zugestellt. (Die alten Queues werden außerdem nie
  serverseitig gelöscht — Leak, V8.)
- **V3 — Empfänger-Discard wird geackt**: Jeder MLS-Decrypt-Fehler (Replay,
  > 5 hinter dem Ratchet, > 1000 voraus, falsche/ältere Epoche wegen
  `max_past_epochs = 0`) wird klassifiziert als `Discard` → `warn` +
  Prozess-Zähler → **`ack_all`** (supervisor.rs:1293-1301, 941-944). Für den
  Transport ist die Nachricht damit zugestellt; sie ist endgültig weg.
  Konkret verlustträchtig: App-Nachricht der Epoche N trifft ein, nachdem der
  Commit auf N+1 (über eine andere Leg) schon gemerged ist.
- **V4 — msg_id-Dedup verschluckt Resends**: Resend nach Rebuild = frische
  Verschlüsselung, aber DIESELBE msg_id (seq-gekeyt). Steht die id noch im
  256er-`completed`-Ring des Reassemblers (`chunk.rs:165-167, 38`), wird der
  Resend **ungelesen weggeackt** — auch wenn der Erst-Decrypt fehlschlug (V3).
  Resend-Heilung und V3 blockieren sich also gegenseitig.
- **V5 — Forward-Fenster bricht die Leg dauerhaft**: > 1000 eigene Nachrichten
  während eine Leg taub ist (der Sender-Ratchet läuft ja weiter, ein Advance
  pro Log-Seq) → nach der Heilung liegt der Sprung über
  `maximum_forward_distance` → jeder weitere Frame wird discarded (und per V3
  geackt) → die Leg konvergiert nie wieder.
- **V6 — Pruning-Horizont schreibt still ab**: Fällt ein Peer aus der
  2×-Karenz, ignoriert `compact_once` seinen Cursor (`molt-storage/src/
  lib.rs:2381-2391`), Segmente droppen, `read_log_from` überspringt den
  Bereich **stumm** (lib.rs:1383-1423), der Cursor springt auf den ersten
  überlebenden Seq. Bereits dokumentiert-akzeptiert (log_compaction.md:471) —
  wird zum expliziten Garantie-Horizont (§0), aber das Gate muss künftig auf
  dem ACK-Stand stehen, nicht auf dem Sende-Cursor (§4.9).
- **V7 — Lokale Skips advancen trotzdem**: schlägt Encrypt/Chunk/Wrap lokal
  fehl, wird der Seq übersprungen, der Log-Cursor rückt am Batch-Ende trotzdem
  vor (supervisor.rs:689-704) — für diesen Peer dauerhaft weg. Pathologisch
  (kaputter Ratchet), aber die Resend-Schleife (§4.4) heilt ihn jetzt gratis:
  unbestätigt bleibt unbestätigt, der nächste Durchlauf versucht es erneut.
- **V8 — Queue-Leak bei Rotation/Extension**: ersetzte Legs hinterlassen ihre
  N Queues auf dem Server bis zum Idle-Expiry; `delete_queue` wird nur auf
  Fehlerpfaden frisch geminteter Queues gerufen (net.rs:1610-1618, 1816-1827,
  1898). Kein Nachrichtenverlust (die ACK-Mechanik deckt den Inhalt), aber
  Hygiene + Server-Müll.

## 3. Zielinvarianten (testbar, nummeriert)

- **G1 (at-least-once):** Jedes eigene Wire-Event erreicht jeden aktuellen
  Sitz, der innerhalb des Horizonts wieder erreichbar wird — über Taubheit,
  Rotation, Reopen, Hard-Kill des Senders, Epochen-Wechsel hinweg.
- **G2 (exactly-once-Wirkung):** Resends/Fan-out-Kopien verändern den
  Workspace-Zustand nie doppelt: Envelope-Dedup per `(Sender, env.seq)` im
  Empfangsfenster, zusätzlich zu den bestehenden Art-Dedups.
- **G3 (zugestellt = Engine hat akzeptiert):** Der ACK-Stand eines Peers
  bewegt sich nur, wenn dessen **Engine** das Envelope nach Generation- und
  Roster-Gate angenommen hat — nie durch Server-Accept, nie durch
  Transport-Ack, nie durch einen Discard.
- **G4 (kein stiller Verlust):** Jeder Pfad, der eine Nachricht endgültig
  aufgibt (Horizont, Attempt-Cap, Roster-Rauswurf), tut das **laut** (WARN +
  Session-Notice), nie durch stilles Cursor-Vorrücken.
- **G5 (Additivität):** Alt-Knoten bleiben funktionsfähig: neue
  `TransportState`-Felder serde-additiv, der ACK-Frame ist ein neuer
  Control-Tag, den Alt-Knoten als Discard warnen-und-droppen; kein
  Datenpfad-Wire-Format ändert sich. Die Garantie greift erst, wenn beide
  Seiten das Update haben — vorher exakt heutiges Verhalten.
- **G7 (in-order, nachgerüstet 2026-07-28 aus der Live-Evaluation):** Die
  Events EINES Senders werden bei jedem Empfänger in Sende-Reihenfolge
  sichtbar — ein nachgelieferter Vorgänger erscheint nie NACH seinem
  Nachfolger. Reihenfolge über Sender hinweg bleibt wie bisher unversprochen.
- **G6 (bounded):** Alle neuen Zustände sind beschränkt: Empfangsfenster W
  Bits/Peer, Resend-Backoff gedeckelt, Cache-Eviction unter dem Min-ACK,
  Horizont = Karenz. Keine unbounded Queues, keine unbounded Maps.

## 4. Design

### 4.1 Überblick

Drei Cursor-Ebenen pro Richtung einer Leg:

```
Sender:    log ──► sent-Cursor (heute: outbound.log_seq, Server-Accept)
                └► acked-Floor (NEU: alles ≤ floor ist Engine-bestätigt)
Empfänger: accepted-Window pro Sender (NEU: high + W-Bitmap über Log-Seqs)
                └── dient ZUGLEICH als Envelope-Dedup (G2) und ACK-Quelle
```

Der Empfänger meldet sein Fenster periodisch als MLS-Control-Frame zurück
(§4.3). Der Sender rechnet daraus seinen `acked`-Stand, rewindet bei jeder
Leg-Neuetablierung auf den Floor und resendet unbestätigte eigene Seqs mit
**frischer Verschlüsselung und frischer msg_id** (§4.4/4.5). Discards bleiben
unbestätigt und werden dadurch geheilt statt beerdigt.

### 4.2 Accept-Punkt & Empfangsfenster (Empfänger)

**Accept-Punkt** ist `cmd_net_delivered` NACH dem Generation-Gate
(net.rs:969-972) und dem Roster-/Impersonations-Gate (net.rs:973-984), VOR dem
Art-spezifischen Arm: dort gilt „die Engine hat das Envelope authentifiziert
angenommen" — auch für Arten, die der Arm danach bewusst ignoriert
(Fall-through net.rs:1239) oder art-dedupliziert: Ignorieren ist Semantik,
nicht Transportverlust. Ein Generation-Drop (alte Mesh-Inkarnation) markiert
NICHT — das Envelope kommt per Resend wieder. Ein Roster-Drop markiert NICHT
(der Sender läuft in den Attempt-Cap, G4 — ein entfernte Sitz ist kein
Zustellziel mehr, §4.4).

**Fensterzustand** pro Sender S (im Engine-State, persistiert §4.7):

```rust
/// molt-core: additiv in TransportState
pub struct AcceptedWindow {
    pub high: u64,          // höchster akzeptierter env.seq von S
    pub bits: Vec<u64>,     // W=1024 Bits: seq high-1 … high-W (1 = akzeptiert)
}
```

Update bei Accept von seq s: s > high → Fenster nach oben schieben (Bits
rutschen, high = s); s ≤ high ∧ Bit gesetzt → **Duplikat, Envelope droppen**
(G2 — vor jedem Art-Arm); s ≤ high-W → unterhalb des Fensters: als Duplikat
behandeln (konservativ; ein legitimer Erst-Empfang so tief unterm High ist
mit Resend-Kadenz + Near-in-order praktisch ausgeschlossen, und die Art-Dedups
fangen Chat/Announce zusätzlich). Wichtig: die Bits sind über den
**Log-Seq-Raum des Senders** (der eigene Seqs nur sparse enthält — fremde
Events, Non-Wire-Events dazwischen); Nullbits sind darum NICHT automatisch
Verluste — nur der Sender kann sie gegen seine Sent-Menge diffen (§4.4).

### 4.3 Der ACK-Frame

Neuer NUL-präfixierter Control-Tag neben Keepalive/Probe
(`molt-net/src/lib.rs:77`):

```rust
pub const MESH_ACK_TAG: &[u8] = b"\x00molt-mesh-ack-v1";
// Frame-Plaintext: MESH_ACK_TAG ‖ serde_json(AckPayload)
pub struct AckPayload { pub high: u64, pub bits: Vec<u64> }  // = AcceptedWindow
```

- MLS-App-Message auf der bestehenden Leg, gesendet wie ein Keepalive
  (`send_ping`-Muster, net.rs:2555) — pro Paar, nie geloggt, nie relayed.
- Der Payload beschreibt IMMER „was ich von DIR (dem Leg-Peer) akzeptiert
  habe" — pairwise, kein Map-over-Members nötig.
- **Kadenz (BUILT, verfeinert):** (a) debounced nach JEDER Zustellung —
  frisch ODER Duplikat (`ACK_DEBOUNCE_SECS = 3`, ein Frame pro Burst): ein
  Dup heißt „mein voriges ACK ist verloren/spät" — das Re-ACK ist es, was
  die Resend-Schleife des Senders beendet, darum braucht es KEINEN
  periodischen ACK; (b) sofort-fällig bei Leg-up (Flush auf dem nächsten
  Presence-Tick), damit ein rewindender Peer seinen Resend-Umfang trimmt,
  bevor die Resends fließen. Das ursprünglich skizzierte Keepalive-Huckepack
  ist GESTRICHEN: es hätte im Mixed-Mesh das Queue-Warming der Alt-Knoten
  ersetzt (die den ACK-Tag discarden) und so falsche Deaf-Alarme erzeugt.
- **Konsumption (BUILT, verfeinert): im SUPERVISOR, nicht in der Engine.**
  Decode-Arm `strip_prefix(MESH_ACK_TAG)` → `MlsDecode::Ack(from, window)`;
  der Recv-Loop pinnt `from == leg-Peer`, stempelt Presence, liest den
  eigenen Log ab dem alten Floor (`OutboxLog::read_from` — der Supervisor
  besitzt Log UND Cursor) und advanct `outbound[peer].acked_floor`
  monoton (`advance_acked_floor`/`record_acked`, supervisor.rs). KEIN neues
  `Command`, Co-Equality unberührt; ein regressiertes ACK kann den Floor
  nie zurückziehen (kein Resend-Sturm bestätigter Historie).
- Alt-Knoten: unbekannter Plaintext → `Discard`-Warnung, geackt — harmlos
  (G5); der Alt-Sender resendet nichts, verhält sich exakt wie heute.

### 4.4 Sender: Floor, Rewind, Resend

**Zustand** (additiv an `OutboundCursor`, molt-core/src/lib.rs:1388):

```rust
pub struct OutboundCursor {
    pub log_seq: u64,                     // wie heute: sent (Server-Accept)
    pub wire_seq: u64,                    // wie heute (Plaintext-Pfad)
    #[serde(default)] pub acked_floor: u64,   // alles Eigene ≤ floor bestätigt
    #[serde(default)] pub resend_epoch: u32,  // msg_id-Salz, ++ pro Rewind
}
```

Bei `NetAckReceived`: Sent-Menge des Bereichs `(acked_floor, sent]` aus dem
Log lesen (eigene Seqs, `MlsCommit`-Bodies ausgenommen — §4.5), gegen
`high`+`bits` diffen, `acked_floor` = größtes f, sodass jede eigene gesendete
Seq ≤ f bestätigt ist. Seqs unterhalb `high−W` ohne Bit gelten als bestätigt
(Fenster-Aging, G6 — W ist groß gegen die Resend-Kadenz).

**Rewind-Regel (der Kern):** Bei JEDEM Supervisor-Build für Peer P (Reopen,
Rotate-Adopt `cmd_net_mesh_extended`, Extension, Recovery-Fold) wird
`outbound[P].log_seq` auf `acked_floor` zurückgesetzt und `resend_epoch`
inkrementiert — die Outbox liest dann von selbst ab Floor+1 und sendet den
unbestätigten Schwanz erneut (dieselbe Schleife wie heute, supervisor.rs:684).
Das macht jede Heilung selbst-nachliefernd, ohne zweiten Sendepfad.

**Periodischer Resend** (heilt V3-Discards bei GESUNDER Leg): ein Tick auf dem
bestehenden Presence-Tick (wie `rotate_deaf_legs`, net.rs:2454): wenn für P
`sent > acked_floor` länger als `RESEND_AFTER_SECS = 30` ohne ACK-Fortschritt
UND die Leg up ist → Rewind wie oben. Backoff pro Peer verdoppelt sich ohne
Fortschritt (Cap `RESEND_MAX_BACKOFF_SECS = 600`), reset bei Floor-Fortschritt.
Leg down/Peer offline → KEIN Resend-Tick (die Outbox hängt ohnehin im
send_one-Backoff; der Rebuild-Rewind deckt die Heilung).

**Attempt-Cap (G4):** bleibt eine konkrete Seq über `RESEND_GIVEUP_EPOCHS = 8`
Rewinds unbestätigt, WARN mit Seq+Peer und Session-Notice
(`zustellung-haengt:<peer>`); weiter versuchen im Max-Backoff (die Garantie
gibt nie still auf — endgültig beendet nur der Horizont §0 oder ein
Membership-Rauswurf des Peers: entfernte Sitze werden aus `outbound`/
Resend-Betrachtung gestrichen, wenn die Chain die Membership-Änderung
appliziert).

### 4.5 msg_id, Ciphertext-Cache, MlsCommit

- **msg_id** wird `resend_epoch`-abhängig: `msg_id(me, peer, seq, epoch)`
  (epoch 0 ⇒ byte-identisch zu heute — Kompat G5; Implementierung: bestehende
  Ableitung, bei epoch > 0 zusätzliches Salz). Fan-out-Kopien EINER Runde
  teilen die id (Ring-Dedup fängt Kopien), ein Rewind bekommt frische ids
  (Ring-Dedup kann Resends nicht mehr verschlucken — V4 tot). Echte
  inhaltliche Dups fängt das Empfangsfenster (G2).
- **Cache**: bei Rewind für P werden die Cache-Einträge des Bereichs
  `(floor, sent_P]` evicted → Resends sind IMMER frische Verschlüsselungen an
  der aktuellen Ratchet-Position/Epoche (heilt V3-Epoche-Fälle und macht
  MLS-Replay-Rejects für Resends unmöglich). Zusätzlich globale Eviction:
  Einträge ≤ min(acked_floor über alle Peers) fliegen (endlich beschränkter
  Cache — heutiges „grows with in-flight" ist damit auch behoben).
- **MlsCommit ist ack-exempt**: sein Frame ist die rohen Commit-Bytes
  (supervisor.rs:106-110), der Empfänger konsumiert ihn im Supervisor
  (EpochAdvanced), ein Envelope erreicht die Engine nie → kann nie bestätigt
  werden. Der Sender überspringt Commit-Seqs beim Floor-Diff und beim Resend.
  Verlust-Modell für Commits bleibt MLS/Recovery (ein Peer, der einen Commit
  verpasst, ist Epochen-detached und läuft in die bestehende
  Recovery-Maschinerie — nicht Aufgabe dieser Garantie).

### 4.6 MLS-Fensterkonfiguration (V5)

`SenderRatchetConfiguration` EXPLIZIT setzen (Create- UND Join-Config,
mls.rs:179-191/:306): `out_of_order_tolerance = 5` (wie Default; Fan-out ist
per-Runde, Skew klein), `maximum_forward_distance = 100_000` (Forward-Skip
kostet nur Kettenableitung, keine Speicherung; 1000 ist gegen einen
14-Tage-Horizont grob zu klein). `max_past_epochs` bleibt 0 (Forward Secrecy;
Epochen-Discards heilt jetzt der Resend, nicht ein Schlüssel-Archiv).

### 4.7 Persistenz & Additivität

- `TransportState` (molt-core/src/lib.rs:1460): NEU
  `#[serde(default)] accepted: BTreeMap<MemberId, AcceptedWindow>`;
  `OutboundCursor` wie §4.4. Alte Dateien lesen sich mit Defaults (floor 0,
  epoch 0, leeres accepted) — Verhalten wie heute, bis ACKs fließen.
- Schreibwege wie gehabt: `SaveTransport` merged künftig `outbound`, `inbound`
  UND `accepted` (molt-storage/src/lib.rs:2479-2499); `MergeCrypto`
  unverändert. Fire-and-forget-Semantik bleibt: eine verlorene
  Fenster-Persistierung regressiert `accepted` → der Peer resendet → das
  Fenster dedupliziert erneut aufgebautes; nicht-idempotente Art-Applier sind
  im Regressionsfenster theoretisch doppelt anwendbar — akzeptiertes
  Restrisiko (klein: Debounce-Sekunden), dokumentiert hier.
- Der ACK-Stand des SENDERS (`acked_floor`) regressiert bei verlorener
  Persistierung nur nach unten → mehr Resend, nie Verlust.

### 4.8 Kompatibilität Alt ↔ Neu

| Konstellation | Verhalten |
|---|---|
| neu → alt | Alt discardet ACK-Frames (warn); Neu-Sender bekommt nie ACKs, `acked_floor` bleibt 0 → Rewinds resenden ab 0? NEIN: ohne jemals ein ACK von P gesehen zu haben (`accepted`-Gegenstelle unbekannt), bleibt der Rewind auf dem heutigen Verhalten (`log_seq` unangetastet). Erst das ERSTE ACK von P aktiviert Floor-Semantik für P. |
| alt → neu | Neu ackt brav, Alt ignoriert (kennt den Tag nicht → Discard-warn) — heutiges Verhalten. |
| neu → neu | volle Garantie. |

Das „erst ab erstem ACK"-Gate (`ack_seen: bool` transient pro Peer, abgeleitet
aus `accepted_floor > 0 ∨ ACK empfangen diese Session`) verhindert, dass ein
Mixed-Mesh Alt-Peers mit Resend-Stürmen ab Seq 0 flutet.

### 4.9 Flankierende Fixes

- **V1-Fix:** `mesh_extension_at.insert` erst NACH erfolgreichem
  `send_targets` (net.rs:1572→hinter :1584); der „carries no usable
  queue"-Fall eines genonceten Relay-Announces wird `debug!` statt `warn!`
  (erwarteter Fall: Broadcast trifft Nicht-Adressaten). Der Cooldown schützt
  weiterhin das Teure (Queue-Mint + Rebuild) — genau das passiert im
  No-Queue-Fall ja nie.
- **V8-Fix:** in `cmd_net_mesh_extended` nach erfolgreichem Rebuild die
  rcv-Queues der ERSETZTEN Leg best-effort `delete_queue`n (spawn, Fehler nur
  loggen — es sind unsere eigenen Queues; Inhalt ist durch Rewind gedeckt).
- **Pruning-Gate auf ACK-Stand (V6):** `compact_once` nimmt für haltende
  Peers künftig `min(outbound[peer].acked_floor.max(bekanntes log_seq-Minimum))`
  — konkret: der Floor-Kandidat pro Peer ist `acked_floor` falls je ein ACK
  gesehen (sonst wie heute `log_seq`); unbestätigter Schwanz bleibt dadurch
  segmentweise erhalten, bis der Peer bestätigt oder aus der Karenz fällt.
  Der Horizont selbst (2×Retention) bleibt unverändert.

### 4.10 Bewusste Nicht-Ziele

- Kein Relay über Dritte, kein Gossip-Repair — die Sender-Outbox ist die
  einzige Wahrheit über eigene Events.
- ACKs/Fenster erscheinen NIE im Event-Log (kein Log-Wachstum, keine
  Chain-Berührung, `crosses_wire` unverändert).
- Kein Ordering-Versprechen über Sender hinweg (wie bisher: konvergente
  Anwendung, ids statt Indizes).
- Keine Änderung an Ritual, Chain-Verify, Roster-Bytes, Recovery.
- Plaintext-/Demo-Pfad (WireFrame + wire_seq) bleibt unangetastet — die
  Garantie gilt dem realen MLS-Mesh.

## 5. Teststrategie (TDD — rot vor Code)

Reihenfolge = Etappenreihenfolge §6. Loopback bekommt zwei Test-Hebel im Hub:
`kill_queue(id)` (simuliert Server-Expiry: SENDs schlagen fehl) und
`mute_queue(id)` (Queue akzeptiert, liefert aber nicht mehr — simuliert die
taube Subscription). Beide nur unter `#[cfg(any(test, feature = "test-hooks"))]`.

1. **V1 (unit, engine):** Rotate-Announce ohne eigene Queue verbrennt den
   Cooldown nicht: erst Announce-für-anderen, dann Announce-für-uns im selben
   Fenster → Extension läuft an (heute rot).
2. **Fenster-Mechanik (unit, core/engine):** AcceptedWindow shift/dup/aging;
   Envelope-Dup wird vor dem Art-Arm gedroppt; Accept nach Roster-Gate zählt,
   Generation-Drop zählt nicht.
3. **ACK-Roundtrip (unit, supervisor):** Ack-Frame decode → Sink; Alt-Pfad:
   unbekannter Tag discardet wie bisher (Kompat-Pin).
4. **Floor-Diff (unit):** sparse eigene Seqs + Commit-Exempt + Fenster-Aging →
   korrekter `acked_floor`; kein Floor-Fortschritt ohne ACK (`ack_seen`-Gate).
5. **Rewind + frische msg_id (unit, supervisor):** Rewind evicted Cache-Bereich,
   resend trägt neue id (epoch > 0), Erstsendung epoch 0 = heutige id-Bytes
   (Byte-Pin wie die Chat-Fixtures).
6. **E2E mute→rotate (loopback, 3 Nodes):** dark sendet 5 Chats, brutals
   Inbound wird ge`mute`t, brutal rotiert, Leg heilt → **alle 5 bei brutal**
   (heute rot: verloren). Dito mit `kill_queue` (heute: hängt nur, grün durch
   Retry — Pin gegen Regression).
7. **E2E Epochen-Discard (loopback):** Nachricht der alten Epoche trifft nach
   Commit ein → Discard → periodischer Resend liefert nach (heute rot).
8. **E2E Reopen/Hard-Kill (loopback):** Sender hart getötet nach Server-Accept
   in gemutete Queue, Reopen → Rewind liefert nach; Empfänger-Fenster
   dedupliziert die Dups (Zustands-Assertion: exactly-once-Wirkung).
9. **E2E Votes/Proposals:** dasselbe Muster mit Proposed/Approved statt Chat —
   ein während der Taubheit abgegebenes Votum erreicht den Sammler, der Block
   sealt (heute rot bei mute+rotate).
10. **Pruning-Gate (storage-unit):** unbestätigter Schwanz hält den Floor,
    bestätigter gibt frei; Karenz-Ablauf verhält sich wie heute.
11. **Backlog > 1000 (unit, mls):** expliziter SenderRatchet-Config-Pin +
    Decrypt nach 5-stelligem Forward-Sprung.

Bestehende Keystones (Chat-Byte-Fixtures, co_equality, Determinismus) bleiben
grün — nichts an Log-/Chain-Formaten ändert sich.

## 6. Etappen (jede endet grün + gepusht auf master)

- **E1 ✅ (82c27e7) — V1-Cooldown-Fix** (net.rs, Test 1).
- **E2 ✅ (5f7198e) — Empfangsfenster + Envelope-Dedup** (molt-core
  AcceptedWindow, TransportState additiv; engine Accept-Punkt; SaveAccepted-
  Merge-Arm in molt-storage).
- **E3 ✅ (6723292) — ACK-Frame** (MESH_ACK_TAG; Konsumption im SUPERVISOR —
  kein neues Command, §4.3 verfeinert; Dup-getriggerte Re-ACKs; Leg-up-ACK).
- **E4 ✅ (636b71e) — Sender-Floor, Build-Rewind, periodischer Resend mit
  Backoff+Give-up-Loudness, msg_id-Epoche (Byte-Pin für Epoche 0),
  Cache-Eviction (Rewind + Min-Floor).**
- **E5 ✅ (0d8fc3e) — E2E-Keystone** (delivery_guarantee.rs Clean-Close;
  Loopback revive_queue + hub()-Seam; Red-Check verifiziert).
- **E6 ✅ (1c5aa73) — Live-Ratchet-Persist (record-gekoppelt) + Hard-Kill-
  E2E-Pin + SenderRatchetConfiguration(5, 100k) + Pruning-Gate auf
  Acked-Floor + V8-Queue-Delete.**
- **E7 ✅ — Review-Runde über den Gesamt-Diff** (2 unabhängige adversariale
  Läufe, 7 Findings gefixt inkl. 3× HIGH — §8), Doku-Closeout (dieses
  Dokument, CLAUDE.md-Transportabschnitt, MEMORY.md).

## 7. Risiken & Grenzfälle (entschieden)

- **Resend-Duplikate bei Fenster-Regression** (§4.7): akzeptiert, klein,
  dokumentiert; Chat/Announce doppelt gedeckt durch Art-Dedup.
- **W=1024 zu klein bei extremem Log-Tempo:** Aging erklärt Unbestätigtes
  unterhalb high−W für bestätigt — bei 3–9-Personen-Republiken und
  Sekunden-Debounce praktisch unerreichbar; Konstante zentral, notfalls
  hochdrehen.
- **Poisoned message** (Roster-Drop, ewig unbestätigt): Attempt-Cap → laut +
  Max-Backoff; endgültig bereinigt durch Horizont oder Membership-Block.
- **Mixed-Version-Mesh:** `ack_seen`-Gate hält Alt-Peers auf heutigem
  Verhalten; ACK-Discard-Warnungen beim Alt-Knoten sind kosmetisch und enden
  mit dessen Update.
- **ACK-Fälschung:** ausgeschlossen — der Frame ist MLS-authentifiziert
  (Credential = Sender), und `NetAckReceived` ist INTERNAL (kein MCP-Zugriff).
  Ein bösartiges Mitglied kann höchstens SEINE EIGENEN Acks lügen (Selbstschaden:
  es verzichtet auf Nachlieferung bzw. erzwingt Resends an sich selbst —
  gedeckelt durch Backoff-Cap).

## 8. E7-Review (2026-07-28): 2 unabhängige adversariale Läufe, Findings + Auflösung

Beide Reviewer bestätigten Kernmechanik, Bit-Mathematik, ACK-Authentisierung,
msg_id-Epochen, Mixed-Version-Pfad und die Writer-Merge-Trennung als korrekt.
Gefixt (alle mit Test):

1. **Recovery-Inkarnation vs. Accept-Fenster (HIGH):** Ein Recovery-Rejoin
   startet den Seq-Raum neu; das alte Fenster der Survivor hätte JEDEN neuen
   Envelope als Duplikat geschluckt und falsch geackt. Fix:
   `reset_peer_accept_window` am authentifizierten One-Shot-Punkt des
   Recovery-Announce. Die Restore-Variante des Findings ist NACHGEPRÜFT
   GESCHLOSSEN (2026-07-28, Branch delivery-followups): ein Backup-Blob darf
   per §3.3-Allowlist NIE ein `transport.state` tragen (import.rs hard-
   reject; gepinnt in `stage_and_commit_round_trip_into_a_fresh_root` —
   „no queue credentials are ever imported"), also öffnet JEDER Produkt-
   Restore ohne Mesh-Creds → offline → Re-Attach nur übers Recovery-Ritual
   → genau dort feuert dieser Reset. Der ursprünglich skizzierte
   next_seq-Fast-Forward hätte zudem den WP4a-Integritätscheck (Key-Table
   first_seq == Positionszählung, open-scan hard-reject) aufweichen müssen —
   verworfen. Rest-Risiko: nur manuelles Verzeichnis-Kopieren am Produkt
   vorbei (kein Produktpfad).
2. **`seq 0`-Panik (HIGH):** u64-Underflow im Fenster (`overflow-checks`
   gilt auch im Release) — ein craftetes Envelope eines Roster-Mitglieds
   tötete den Actor, Redelivery = Crash-Loop. Fix: seq 0 ist nie gültig,
   Reject im `accept`.
3. **Seq-Raum-Verwechslung Floor↔Cursor (HIGH, beide Reviewer):** Der Floor
   lebte im „eigene Events"-Raum, der Outbox-Cursor ist Ganz-Log-Position →
   Dauer-Rewind-Loop + falsches Degraded für stille Zuhörer, und das
   WP4a-Gate klemmte bei Lurker-Nodes wochenalt fest. Fix: der Floor IST
   jetzt Log-Position (`advance_acked_floor` läuft über fremde/Commit-Seqs
   frei hinweg; nur eigene unbestätigte ackbare Events stoppen ihn), und die
   Outbox self-advanct über rein-fremde Spannen ohne ACK (ein gecachter
   Log-Check pro (floor, head)-Spanne).
4. **ACK-Kadenz hing am 30-s-Presence-Tick (MEDIUM):** Die „3 s"-Debounce
   war real 3–33 s und verlor das Rennen gegen den 30-s-Resend-Timer. Fix:
   eigener 1-s-`NetDeliveryTick` (INTERNAL) für ACK-Flush + die debounced
   Persists; der Leg-up-Sofort-ACK wirkt damit wirklich sofort.
5. **ACK-Spam-Amplification (MEDIUM):** Voll-Verarbeitung (Log-Read auf dem
   Writer-Thread + Snapshot-Save) pro Frame. Fix: 500-ms-Drossel pro Peer,
   Persist nur bei tatsächlichem Floor-/ack_seen-Fortschritt.
6. **`merge_mls_async` blockte den Actor (LOW):** `send` auf vollem
   Writer-Queue im `record()`-Hot-Path. Fix: `try_send` + Drop-Warn; der
   Debounce-Stempel rückt nur bei wirklich enqueuetem Merge vor.
7. **Hard-Kill-E2E war Jitter-grün (Test-Robustheit):** Der Persist-Beweis
   hing an RNG-Timing. Fix: deterministisch — nach Ablauf der Debounce
   erzwingt ein dritter Chat den record-gekoppelten Persist NACHWEISLICH vor
   dem Kill; der ACK geht dreifach raus statt auf ein 700-ms-Fenster zu
   hoffen; beide Nachlieferungen („zwei" + „drei") werden asserted.

Bekannte offene Kleinigkeiten (bewusst zurückgestellt, LOW):
- Ein per Membership entferntes Mitglied hinterlässt bis zum Reopen einen
  `ack_seen`-Cursor, der die Min-Floor-Cache-Eviction pinnt (nur RAM).
- Der E2E heilt per `revive_queue` (gleiche Queue); der Rotate-Adopt-Pfad
  mit Resend auf FRISCHE Queues ist nur unit-gepinnt (`rewind_unacked`) —
  ein voller Rotate-E2E ist ein Follow-up.

## 9. G7 — In-Order-Zustellung (Nachtrag aus der Live-Evaluation, 2026-07-28)

Die Live-Validierung zeigte: at-least-once allein reicht nicht — ein per
Resend geheiltes A erschien ~60 s NACH B. Der User-Auftrag dazu: *"Nachrichten
dürfen nicht in der falschen Reihenfolge ankommen! Wenn A nicht an Client3
geschickt werden kann, dann darf auch B nicht geschickt werden solange A nicht
verschickt wurde."* Umsetzung (Branch delivery-followups):

- **`prev_seq`-Kette (additiv):** Jedes selbst-authored Envelope trägt die
  Seq des vorherigen EIGENEN ackbaren Events (`make_env` stempelt; `apply`
  leitet die Kette her — auch beim Replay; `MlsCommit`s sind ausgenommen,
  ein Empfänger könnte sie nie akzeptieren). `0` = Kettenstart/Alt-Writer →
  ungeordnet wie bisher; `skip_serializing_if` hält alle Alt-Bytes
  (Fixtures, Log-Frames, Hashes) identisch. Re-recorded Peer-Events tragen
  keine Kette (sie werden nie gefannt).
- **Empfänger-Hold:** `cmd_net_delivered` parkt ein Envelope, dessen
  `prev_seq` nicht im Accept-Fenster steht — UNMARKIERT: der Sender hält es
  für unbestätigt, Floor stallt darunter, der Resend liefert den fehlenden
  Vorgänger; ein Crash des Empfängers kostet nur den RAM-Park, den die
  Resend-Maschine von selbst wieder füllt. Landet der Vorgänger, drainiert
  der Park in Seq-Reihenfolge durch denselben `deliver_gated`-Pfad.
  Bounded (512/Sender, Überlauf shedded aufs Resend), Dup-tolerant, vom
  Recovery-Fenster-Reset mitgeräumt.
- **Pathologie-Ventil:** Ein Park-Eintrag, dessen Vorgänger nie kommt
  (verlogene/kaputte Kette), wird nach 900 s LAUT ungeordnet freigegeben —
  ehrliche Ketten erreichen das nie (Resend-Backoff-Cap 600 s), aber ein
  Fehler darf einen Sender nicht für immer wedgen.
- **Latenz-Flanken:** `out_of_order_tolerance` 5 → 5000 — die im Server
  gespeicherten ORIGINALE des tauben Fensters kommen nach dem Heal mit
  längst überholten Generationen an und wurden alle
  `TooDistantInThePast`-verworfen (Live-Log: „292 < 415"); jetzt decrypten
  sie sofort, der Resend ist nur noch Backstop. `RESEND_AFTER_SECS` 30 → 10
  (die ACK-Latenz ist seit dem 1-s-Tick ≤ ~4 s), damit ein fehlender
  Vorgänger seine Nachfolger nur Sekunden statt eine Minute verdeckt.
- **Pins:** `a_successor_waits_for_its_predecessor_and_lands_in_order`
  (rot-ohne-Hold verifiziert), `make_env_chains_own_ackable_events_and_
  skips_commits`, `the_park_clears_on_reset_and_releases_stale_entries`,
  `prev_seq_is_byte_invisible_at_zero_and_roundtrips_otherwise`,
  `frames_far_behind_the_ratchet_still_decrypt_after_the_heal`.
- **Evidence vs. silence in a claim sheet (2026-08-04):** `apply_group_ack`
  latches `ack_seen` whenever the sheet SPEAKS about us
  (`window_for(me).is_some()`) — the implied floor may be 0. The rule the
  guard exists for is "a sheet that says nothing ABOUT US proves nothing",
  and requiring a non-zero floor on top of it stranded a REJOINER, whose
  honest floor is zero: `group_floor` stayed `None`, so `rewind_group`
  never republished the span its catch-up was parked on. Pinned from both
  sides (`a_fresh_incarnations_sheet_counts_as_evidence_at_floor_zero`,
  `a_sheet_that_is_silent_about_us_still_proves_nothing`).
- **Fresh-incarnation rule (2026-08-04, N4b §3.1a):** a sender this node
  holds NO accepted history with (empty `AcceptedWindow`) delivers its first
  envelope UNORDERED and seeds the window as the ordering baseline; G7 is
  fully in force from there on. A rejoiner or late joiner enters the
  broadcast mid-stream: the stamped predecessors were published at epochs
  its exporter ring can never open, so parking held the recovery catch-up
  hostage to frames that cannot exist for it — the rejoiner verified its
  anchor and then never saw a block again. Pin:
  `a_first_contact_envelope_delivers_without_a_history_to_order_against`.
