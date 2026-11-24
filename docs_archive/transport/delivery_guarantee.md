# Konzept: Zustellgarantie für Mesh-Nachrichten (at-least-once, Ende-zu-Ende)

> **Scope note (2026-07-30, etappe N-demo):** the SMP transport was removed in
> the Nostr transport replacement (`docs_archive/transport/nostr_transport_marmot.md`).
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
`docs_archive/transport/mesh/mesh_rotation_trackc.md`, `docs_archive/chain/log_compaction.md` und den
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
`peers_holding_the_log` in `compaction.rs`) — dieselbe Grenze, die schon
heute bestimmt, wie lange ein abwesender Peer den Log hält. Seit 2026-10-11
(Audit #32, `log_compaction.md` F4b) gilt sie auch für einen Peer, der in
Kontakt ist, aber nicht ackt: bewegt sich sein acked floor 2 × Retention-Window
lang nicht, hält er den Log nicht mehr, und die Garantie endet für den Teil
unter dem Floor. Jenseits davon gilt weiterhin die ehrliche Lücke (bounded
storage schlägt unbounded Queue).

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
  Der Horizont selbst (2×Retention) bleibt unverändert; seit 2026-10-11
  zählt er auch ab dem letzten ACK-Fortschritt (F4b, §0).

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
  Dup-tolerant, vom Recovery-Fenster-Reset mitgeräumt. **Superseded by §10
  (v0.1.1):** only order-sensitive kinds wait, the gap is asked for, the
  900 s valve and the shed-newest cap are gone.
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
- **Re-offer only to owed peers that can hear (2026-11-21, broadcast
  outbox).** §4.4's "peer offline → no resend tick", restored on the 445
  channel: a stall round goes out only if some peer still owed an own
  envelope (`ack_seen`, floor below it) is HEARD - an authenticated frame
  decrypted within the engine's silence window (`chain_lag().silent`,
  120 s). It rewinds only to the lowest floor among those peers. The
  engine hands the verdict over `GroupHandle::set_reach` (1 s delivery
  tick, and at once on `peer_seen` of a silent peer); a quiet stall
  resumes on that event. Skipped rounds publish nothing and spend no
  budget, but keep the ladder and count toward the 8-round `send_failed`.
  Exceptions: the first 2 rounds after a runtime start go out regardless
  (old stamps; a quiet live listener proves itself only by acking), and a
  runtime start publishes one claim sheet, so a returning seat is heard
  even when the relay kept nothing. A seat that concluded
  `seat_locked_out` re-offers nothing until it reads a peer's frame, is
  reopened or rejoins. The tail itself stays owed and un-compacted
  exactly as before. Pins: `a_silent_peer_gets_no_re_offers_until_it_is_heard`,
  `a_heard_slow_peer_keeps_its_resends_beside_a_silent_one`,
  `a_locked_out_seat_re_offers_nothing`,
  `crates/molt-engine/tests/nostr_outbox_quiet.rs`.

## 10. G7 by need: a frontier per sender, a buffer, the gap asked for (v0.1.1)

Field 2026-11: after a restart one missing predecessor per sender held a
node deaf for the full 900 s valve - every later envelope of that sender
parked behind it, the park filled (512) and shed the newest, which opened
the next gap. The original of a missing envelope often can never arrive
(MLS: a generation spent before the restart or behind the 5 000
out-of-order window of a newest-first replay; relay: paged out, expired,
pruned), and the sender's stall round is on a 10..600 s ladder plus an
hourly budget, unmoved by a hold that changes no window. Worse, the acked
floor only moves forward (`apply_group_ack`, and the mesh's twin): an
accept window that regressed across a hard stop (saved every 5 s, the
ratchet every 10 s) leaves a seq the sender already counts as acked - no
stall round ever re-offers it, §4.7's "resends absorb a lost save" does
not hold for it.

The accept window itself is the second trap: a seq more than 1024 below
`high` reads as accepted (aged out), the sheet reports it so, and the
sender's floor passes it for good. Whatever lets `high` jump past an
unseen owed seq loses it silently.

- **Frontier.** Per sender, `frontier` C: every envelope the sender owes
  at or below C is accepted or proven unfillable. It is persisted with the
  window (`AcceptedWindow.frontier`, and `links` - the `(prev_seq, seq)`
  pairs accepted above it - both receiver-local, stripped from every ack
  sheet; `0` = legacy, read as `high`). **Nothing is accepted above C +
  1024**, so no owed seq ever ages out unseen and the sender's floor never
  passes one. C rises only on proof: (a) an accepted envelope whose
  `prev_seq` is at or below C (then on through the stored links); (b) the
  sender's listing of its own seqs above C, up to the first listed seq
  that is neither accepted nor held; (c) for a sender that never answers a
  want: its resend round passed - a second arrival, in this session, of a
  seq above C (MLS opens a consumed frame only once, so a relay re-delivery
  cannot fake it; a frame re-read after a restart is from an earlier
  session and does not count) - then 5 s later C jumps to just below the
  lowest seq that round re-offered (the round starts at the sender's floor
  and goes up; a hole below it is outside it), never past a held or a D7
  refused seq. Seqs passed over by (c), and listed seqs re-published 3
  times that never landed, are the only losses: `inbound_gap_lost
  member= count= first= via=` (WARN).
- **Buffer.** Decrypted envelopes that may not be applied yet are kept,
  unaccepted: anything above C + 1024, and an order-sensitive one behind a
  missing seq. They are applied bottom-up as C rises, so a newest-first
  replay catches up entirely from what the relay sent, without a resend.
  Bounded by 64 MiB across all senders (a card nears the 70 KiB payload
  budget: ~900 of the largest, tens of thousands of chat and votes); past
  it the highest seq of the largest buffer is shed, becomes missing again
  and stays owed. A restart loses the buffer; its frames are spent, the
  window never claimed them, C and the links come back with it, and what
  is still owed is listed or re-offered.
- **Order.** Order-free kinds at or below the cap are applied at once:
  `Proposed`, `Withdrawn` (terminal, parks on its card's id), `Committed`,
  `ChainRequest`, `Checkpoint*`, `MembershipProposed`, `VoteRefused`,
  `MeshAnnounced`, `File*`. Chat (`Chat`, `ChatReacted`, `ChatDeleted`,
  `ChatRead`, `FileRemoved`) and stances (`Approved`, `Declined`) wait while
  a seq of their sender below them is missing or held. For ORDER only that
  wait ends when the seq arrives, is answered `gone` or not listed, was
  `resent` and is absent 5 s later, a round passed it, or after 60 s;
  none of these moves C. A seq left unaccepted by the D7 decline park is
  not re-asked and is settled by nothing but the 60 s: re-publishing it
  changes nothing, and longer would hold that member's chat behind a local
  park.
- **Ask.** `GapWant` (`\x00molt-gapwant-v1`): named seqs (≤ 64, a seq
  missing for 2 s, re-asked every 10 s), or `range (after, upto]` (≤ 1024)
  whenever something is known above C. No want goes out while a stored
  replay is coming in (`GroupHealth.replaying`): a relay's replay is open
  from its REQ until its final EOSE (all pages and the tail), until it is
  given up as stuck (3 rounds), or until its session dies (a reconnect
  opens it again), and it counts as in until the consumer has read every
  frame queued up to that end; then 2 s. Asking earlier made the sender
  re-publish what the relay was about to deliver: an EOSE arrives while
  the last page is still queued unread. What a finished replay still
  lacks is a real hole - among them the rest of a second that held more
  stored frames than one page (`relay replay skipped one second`). The sender re-publishes named own
  envelopes as fresh frames and answers `GapAnswer` (`\x00molt-gapans-v1`)
  `resent`/`gone`; a range is answered with `listing {after, through,
  own}` (≤ 256 seqs, never past its publish cursor), nothing re-published.
  Bounds per asker: each seq re-published at most once per 60 s, 64 frames
  per 10 s, one listing per 5 s; 384 re-published frames per minute in
  all; askers served round-robin, each read bounded to the asked span.
- **Visibility.** `inbound_held member= missing_seq= held= frontier=
  since_s=` once per episode (after 2 s); `inbound_held_cleared member=
  since_s= via= unordered= shed= lost=`; `inbound_gap_skipped` per seq
  settled for order (WARN for the 60 s); `inbound_hold_shed` once per
  episode. `StatusView.inbound_held`: per sender `frontier`, `missing_seq`,
  `missing`, `held`, `bytes`, `since`; a row appears too while accepted
  links hang above the frontier. During a long catch-up the frontier
  rises page by page and `held` drains; a frontier that stands still while
  `held` grows is stuck. The restore
  settle (`recovery_auto_approval.md`) keeps waiting while the
  coordinator's stream has a missing or held seq here.
- **Mixed versions.** An older build drops both tags as unknown control
  frames: as a receiver it keeps its old park and ignores `frontier` and
  `links` (it reads `high` as before; what this build buffered is memory
  and gone, still owed); as a sender it never answers, so its holes close
  by its resend rounds (≤ 600 s, its hourly budget) via (c).

Pins: `net::delivery_tests` (`a_newest_first_replay_past_the_window_loses_nothing`,
`an_older_senders_hole_is_never_acked_and_closes_on_its_round`,
`a_listed_hole_is_resent_and_the_frontier_walks_up`,
`a_shed_envelope_stays_owed_and_is_recovered`,
`a_restart_mid_catch_up_loses_nothing_owed`,
`a_frame_re_read_after_a_restart_is_no_round`,
`a_cut_round_passes_only_what_lies_below_it`,
`a_gap_holds_only_order_sensitive_kinds`,
`a_gap_the_sender_disowns_releases_at_once`,
`an_unanswered_gap_falls_back_far_below_the_old_valve`,
`a_resend_round_passing_the_gap_settles_it`,
`a_flood_behind_a_gap_loses_nothing`,
`an_answering_senders_duplicates_are_no_evidence`,
`a_gap_want_is_ours_only_from_the_roster_for_our_stream`),
`group_runtime::tests::a_gap_want_is_answered_with_exactly_the_missing_frames`,
`chain::membership_tests::a_restore_keeps_settling_while_the_coordinators_stream_has_a_gap`,
`crates/molt-engine/tests/nostr_inbound_gap.rs`,
`crates/molt-engine/tests/nostr_long_replay.rs`,
`crates/molt-engine/tests/nostr_slow_replay.rs`.

## 11. A restored seat's new incarnation (v0.1.2)

A restored seat's new device numbers its log from the start (the
materialized genesis is seq 1). A window that still holds the old
incarnation's marks drops the new device's frames as duplicates
(everything 1024 below its old `high` reads as accepted), every duplicate
re-arms the ack, and the sheet's old `high` walks the new device's acked
floor past frames nobody applied. Three sources of truth decide what
happens; none of the new rules reads a wall clock (two older §10 rules
do, below).

**1. The re-key merge resets.** The group runtime decodes in one task and
hands a merged re-key (`NetPeerRekeyed`) to the engine over the same
ordered sink as the frames, and MLS keeps no past epoch (`max_past_epochs
= 0`): every frame of that seat delivered after the merge is the new
leaf's, none delivered before it is. So the merge - and the coordinator's
own commit - empties the window (`AcceptedWindow.proof` `merge`) and the
incarnation's request/claim gates with it. The `Restored` block resets
nothing. A window born by first contact in this build (`current`) is
trusted: every later restore of that seat reaches it as a merge.
- **Disk order.** Every merge - received, or this node's own commit -
  lists the re-keyed seat inside the MLS state; that list, not the
  `NetPeerRekeyed` message, is the source: every delivery beat, every
  ratchet persist and every runtime rebuild drains it and resets each seat
  taken, so a message lost to an aborted inbox loses nothing. No ratchet
  snapshot is taken while the list is non-empty (checked under the lock
  the merge takes) or while a reset is not yet queued; a refused queue
  keeps it dirty and retries every beat. The coordinator resets before it
  records the commit. The writer is FIFO: on disk the pair is both-old or
  both-new; a kill in between merges the re-key again at the reopen.
- **Paths that reach a new epoch without the hook.** A seat joining by
  Welcome (founding, a rejoiner's own bootstrap) holds no window yet. The
  epoch-hold retry calls the hook. An import or a restore from an export
  carries no `transport.state` (no window, no ratchet). The one path left
  is an older build that persisted the merged ratchet and lost the window
  save: that window is legacy (2). The test transport's `NetRuntime` path
  snapshots its ratchet without consulting the re-key list (test
  transport only).
- **The committer persists before it publishes.** No peer sends a seat its
  own commit back, so a committer whose merged epoch missed the disk stays
  an epoch behind for good (every frame `past epoch` or opaque). The
  coordinator therefore logs the commit and the ratchet through the gate
  above as ONE writer step (`append_with_mls_blocking`, synced, acked with
  the write's result; `rekey_persist=unconfirmed` when it failed). The
  outbox reads the log through the same writer, so no read returns the
  commit before its epoch is on disk; the delivery task starts after. A
  kill after that leaves it at most AHEAD of the group: the
  outbox publishes own log entries from its persisted cursor, so the
  logged commit goes out at the reopen, sealed under the exporter of the
  epoch it was made at (the ring is part of the MLS snapshot). The late
  merge resets the bystanders' window for the seat; what they sent at the
  old epoch meanwhile reaches the coordinator by re-send. The `Restored`
  block is applied in the same handler, before the re-key, and rides the
  same outbox ahead of it. Not re-sent: the Welcome - the reopened
  coordinator answers the device `link outdated`, and a restore by a new
  link re-keys on top. The delivery of commit and Welcome starts again on
  the relays this node may dial whenever that set moves, and ends with the
  workspace (`a_rekey_delivery_moves_to_a_relay_allowed_after_its_own_were_withdrawn`).
  That link mints only once the seat was silent for
  the window (120 s): before, the mint answers `seat still active`
  (`restore_gate`); the test sets the window to 0. Founding's `add_members` is written whole at the
  seal (the ritual's first disk write). Pins:
  `a_coordinator_killed_right_after_its_rekey_reopens_at_the_new_epoch`,
  `a_coordinator_killed_before_its_rekey_went_out_publishes_it_at_the_reopen`.
- **v0.1.3 note.** (a) A durable pending re-key (commit + Welcome, re-sent
  at the reopen until the Welcome landed) would close the Welcome gap.
  (b) A seat behind its own commit is detectable at the open (the last own
  `MlsCommit` was made at an epoch >= the ratchet's); today it shows only
  as `N new messages unreadable` - the frames' epoch sits inside the outer
  layer, so the inbound side cannot tell "newer epoch" from opaque.
  (c) An own commit that never left races a later one. Coordinator C makes
  K_C at epoch N, persists, dies before any relay took it; the device gets
  a new link from seat S, which makes K_S at N. C reopens and re-offers
  K_C: with the lower digest every seat rewinds onto it (the second
  restore undone, the device locked out again); after two or more commits
  during the outage the others ignore K_C and C stays split. v0.1.1 had
  the same (its log re-offered the commit too). v0.1.3 keeps the re-offer;
  a shelf built for it was reverted (it touched states older builds wrote
  and could not replay a re-key of an earlier day). The plan:
  `docs/transport/pending_rekey_plan.md`.
- Pins: `mls::tests::the_evicted_leaf_cannot_speak_after_the_rekey` and
  the exporter-ring test (an old-epoch frame is refused after the merge),
  `mls::tests::a_merged_rekey_stays_listed_until_taken`,
  `net::restore_reset_tests` (`the_merge_resets_once_and_the_block_resets_nothing`,
  `a_first_contact_window_is_trusted_at_any_time`,
  `the_committers_ratchet_never_reaches_the_disk_before_its_reset`,
  `a_lost_rekey_report_is_drained_and_the_ratchet_persists_again`,
  `an_own_commit_is_logged_with_its_ratchet_in_one_write`,
  `an_own_commit_without_its_ratchet_is_not_called_durable`).

**2. A legacy window is left alone.** A window an older build wrote
(`proof` absent, `unknown`) is neither reset nor proven stale: nothing
the sender sends can prove it - a listing reveals only the publish
cursor, which every stall round rewinds. It gives a hint instead:
duplicates it drops from one seat at rising seqs are counted while
nothing of that seat was accepted this session; at 32 `dup_streak from=
first_seq= last_seq= count=` WARN, once; the first fresh frame ends it
(`dup_streak_cleared`); `status.seats[].dup_streak` carries the count. A
seat whose frames are accepted is not behind a stale window - or that
window was overtaken by the new device's seqs, which nothing can prove:
the hint is off then and `resync_seat` stays available. Accepting frames
does not promote the window: it stays `unknown`, and a resync of it
forgets the request/claim gates too (3).

**3. The operator resyncs.** `resync_seat { member }` (a seat tool) empties
that seat's window (`proof` `operator`, `accept_window_reset
via=operator`). A `current`/`merge` window is the same incarnation: its
request/claim gates and its newest-seen seq stay. An `unknown` window may
describe another incarnation: they go too. It is the heal for a legacy
window that holds the old device, and the only one once the new device's
seqs passed the old `high`. Refused for the own seat and a non-seat; while
the history is owed a second call answers so.

**A reset window owes the stream from its start.** An empty reset window
is history at seq 0: the frontier starts at 0, a later envelope waits
behind its missing predecessors, and the gap asks fetch them - however
long that takes. `status.inbound_held` shows the seat with `from_start`
(waiting for its history). No new rule drops it on a timer; two §10 rules
from v0.1.1 apply to it as to any hold and DO decide a loss by a fixed
time: a sender's resend round that passed (5 s grace, then the frontier
goes below the round's lowest seq: `inbound_gap_lost via=resend_passed`),
and a listed seq re-published three times without landing, each try
given 5 s (`via=resent_absent`). So a v0.1.0 sender ends the wait with
its next resend round, losing what lies below it; one with no unacked
tail never does. `resync_seat { member, give_up: true }` ends it in any
state: below what is held, else at the highest seq accepted, else the next
frame is the baseline (`inbound_gap_lost via=operator count=`; `count` in
log positions).

**A re-delivered frame acts only if it is the newest known from that
seat.** The highest seq accepted or held from a sender this session -
never a dropped duplicate - is the source of "newest"; before any arrival
it is the stored window's `high`, except for a legacy window (it may
describe another device). A `ChainRequest` (round, serve, divergence
clear, claim, seat head) and a `VoteRefused` (notice) act only for a frame
at that seq; an older one - a gap-filler after a restart included - is
applied to state as history. A merge reset starts it anew, an operator
resync of a legacy window too; one of a `current` window keeps it. After
a legacy `resync_seat` the seat's re-fetched history arrives ascending
with nothing seen yet, so its old catch-up requests each act once as they
pass: about one per 5 minutes of that device's history, serving at most
once per 30 s, ending with the first frame at its top - a one-off cost of
the operator action (next release: keep `served_at` on an operator resync
and bind `clear_divergence` there to `peer_on_branch`). A
`Committed` block's divergence mark keeps its rule - excused only by a
newer frame that showed the sender on this branch (`peer_on_branch`) -
which an operator resync keeps too.

**Stances** are ordered per card and member by (stamp, wire seq): the
kept own-log event carries the wire seq in `prev_seq` (it is never
published), so the replay rebuilds the marks; an equal pair is the same
frame. A re-delivered old decline never joins a newer one into a
rejection, an old approval never clears a newer decline. Arrival order
between DIFFERENT members still decides a rejection (a decline tips the
card when it lands) - unchanged.

**The sender side.**
- **Ack-ahead guard** (`apply_group_ack`). A sheet whose `high` for us lies
  beyond our log (the tail read from that seq is empty - only the tail is
  read) proves nothing: once per episode the peer's floor falls to 0 with
  `ack_seen`, `ack_ahead from= window_high= own_high=` WARN, and the stall
  rounds re-offer the log. After 8 such rounds (`RESEND_GIVEUP_ROUNDS`,
  the `send_failed` count) the peer is no longer re-offered to; its tail
  stays owed. Only a sheet BELOW the high that opened the episode ends it
  (the peer reset its window): one at or above it may be that stale
  window, overtaken once this log grew past it - acking from it would mark
  frames the peer never had. Meanwhile that peer's floor stays at 0, which
  holds compaction for it until the F4b ack-progress grace releases it
  (`log_compaction.md`); what the peer lacks is then compacted and a later
  resync cannot bring it back. The honest case: a
  seat that imported an export older than its last run reuses seqs its
  peers hold - known limit; restore the seat with a recovery link instead.
- **Regression rewind.** Every sheet carries its writer's clock in
  milliseconds (`GroupAck.at`, additive; an older build writes none and
  never triggers a rewind). This orders ONE peer's sheets by that peer's
  own clock - not a comparison across clocks. The cursor keeps the `high`
  and the stamp of the sheet that last moved the floor (`ack_high`,
  `ack_at`); a sheet stamped later whose `high` is below that `high` no
  longer holds what was acked: the floor falls to what it proves
  (recomputed from `high - 1025`, the last aged-out seq), never below the
  log (`compacted=` counts dropped positions once), `ack_regressed from=
  reported= floor= compacted=` WARN once per episode. A peer whose clock
  once jumped far ahead pins `ack_at`; its rewinds stay off until real time
  passes it (the safe direction). A rewind publishes nothing itself: the
  rounds stay inside the hourly budget (12 plus one heal round).

**Receiver dedup of re-delivered frames**: chat by `MessageId`;
`ChatReacted` with an `op` as an idempotent set/unset; `ChatDeleted`,
`FileRemoved` terminal; `ChatRead` by id; `Proposed` by id; `Committed` by
height; `FileWanted`/`FileServed`/`FileRequested` soft state; stances as
above. A legacy reaction without `op` toggles; no current build sends one.
A chat message older than the retention that a full replay re-lands is
hidden by the read filter and dropped again by the next compaction.

**What heals what**, the restored seat's build × the build of a seat that
kept its old window:

| restored seat \ old-window seat | v0.1.0 / v0.1.1 | v0.1.2 |
|---|---|---|
| v0.1.0 | nothing; frames above the old `high` pass | `resync_seat`; it answers no want: its next resend round, or `give_up`, ends the wait (what it does not re-offer stays lost) |
| v0.1.1 | nothing; frames above the old `high` pass | `resync_seat`; the history comes by listings and named wants (about 6 frames/s) |
| v0.1.2 | refuses the stale acks, re-offers 8 rounds, then waits; frames above the old `high` pass | `resync_seat`; gap asks plus the sender's rewind on the lower sheet |

A window lost to a v0.1.2 merge cannot occur (1).

**Fixed times left in this path**: the accept-window save (5 s), the ack
debounce (3 s) and the live ratchet persist (10 s) are persistence
cadences; the gap asks (after 2 s, every 10 s) and the 60 s order bound
are §10's (order only); the resend ladder (10..600 s) and its hourly
budget are §4.4's. Two pre-existing §10 rules decide a loss by a fixed
time: the 5 s grace after a passed resend round and the 5 s per listed
re-publication (above). None decides a reset or an episode.

**Development nodes.** `RestoreProof` values and the `stances` shape
changed between untagged commits; decoding is tolerant - an unknown
`proof` reads as `unknown`, a `stances` of another shape as empty - so a
workspace never fails to open over them.

Pins: `net::restore_reset_tests`, `chain::stance_order_tests`,
`mls::tests::a_merged_rekey_stays_listed_until_taken`,
`tests::a_development_builds_window_and_card_still_decode` (molt-core),
`group_runtime::tests::{a_sheet_acking_above_the_own_log_is_refused_and_rewinds_the_peer,
a_frozen_stale_sheet_does_not_end_the_episode_once_the_log_passes_it,
an_ack_ahead_episode_reads_the_tail_only_and_stops_after_the_give_up_rounds,
a_newer_sheet_below_the_floor_rewinds_once_per_episode,
a_late_older_or_unstamped_sheet_does_not_rewind,
a_rewind_below_the_compacted_log_counts_what_is_gone,
a_rewind_reads_the_last_explicit_bit_as_unacked}`,
`group_ack::tests::the_stamp_is_additive_both_ways`,
`crates/molt-engine/tests/nostr_recovery.rs`
(`a_seat_that_kept_its_old_window_hears_the_restored_seat_after_a_resync`,
`a_coordinator_killed_right_after_its_rekey_reopens_at_the_new_epoch`,
`a_coordinator_killed_before_its_rekey_went_out_publishes_it_at_the_reopen`),
`crates/molt-engine/tests/nostr_delivery_guarantee.rs`
(`a_receiver_whose_window_regressed_gets_the_tail_again_and_applies_it_once`).

## 12. A clean close publishes what was recorded (v0.1.3)

Measured on v0.1.1 and v0.1.2: five `chat_send` answered `ok`, SIGTERM
under a millisecond later - the other seat held one or two until the
stopped seat's next start. The outbox was parked on its wakeup when the
stop came, and the stop won.

**Rule.** A clean close (`CloseWorkspace`, a workspace switch, founding
or joining another republic while one is open, the service's
SIGTERM/SIGINT - all run `persist_net_crypto_on_close`) closes
the group runtime with `GroupHandle::close`: the engine blocks in it, so
it accepts nothing meanwhile, and the outbox, after its stop, reads the
log once more from its cursor and offers every own envelope it finds,
each to every relay ONCE. The pass ends at the first envelope no relay
took (the cursor is persisted per published envelope, so a pass cut
short keeps what went out) or at the end of the log; the gap answers stop
at their next turn. Then the ratchet snapshot is taken, so it covers every frame the
pass sealed. `outbox=closed sent= [held_at= error=]`. The pass calls
nothing back into the engine, and every sink call of the outbox gives way
to the stop - a blocked engine answers nothing.

**No clock decides the drain.** What bounds it is the transport's
per-publish bounds (`[transport.nostr] connect_timeout_secs` 60,
`publish_timeout_secs` 30): a relay costs at most a dial plus one OK
round-trip, re-asked on a transient refusal only inside that publish
bound - about connect + 2 x publish (120 s by default) - and the relays
are asked concurrently. Every relay unreachable: the first envelope
fails within that bound and the pass ends. One relay accepting while
another hangs: each envelope waits the accepting relay plus the 2 s
grace until the hung one trips its breaker. After a stop by signal, a
second SIGTERM/SIGINT ends the process at once (`stop=forced`, exit 1, no
durable close); a run that ended otherwise (the window closed, stdio
closed) keeps its close. SIGKILL always ends it. The service unit's
`TimeoutStopSec` (180) lies above the farewells' 15 s plus one attempt.
The close shows no progress meanwhile, and MCP requests wait for it.

**Not drained.** A withdrawal (`withdraw()`, a relay left the dialable
set) closes the channel: the pass publishes nothing. A relay the dial gate
refuses (`Withdrawn`) counts as refused: the pass never waits for consent. A plain drop or
`shutdown()` (eviction, the parked-re-key seam, a context reset) stops
at once as before. A hard kill is unchanged - the at-least-once resend
covers it. A current-thread runtime (unit tests) cannot block in place
and drops the handle instead.

Pins: `group_runtime::tests::{a_clean_close_publishes_the_tail_the_outbox_never_read,
a_publish_in_flight_at_the_close_is_finished_and_the_pass_sends_the_rest,
a_plain_shutdown_takes_no_final_pass}`; bounds (they hold without the
pass too): `a_withdrawn_close_publishes_nothing`,
`a_gate_withdrawn_relay_ends_the_close_at_once`,
`an_unanswering_relay_holds_the_close_for_one_attempt_only`;
`crates/molt-engine/tests/nostr_clean_stop.rs`,
`crates/molt-app/tests/service.rs` (`a_second_sigterm_ends_a_waiting_close`).

## 13. A tail read before a restart (v0.1.3)

Field shape (a long history): a seat holds a sender's stream behind a gap
while more than a window (1024) of newer envelopes arrives. Those above
`frontier + 1024` are decrypted and held, never accepted - the ack sheet
reports only accepted seqs, so the sender keeps them owed. A restart loses
the hold; the relay copies no longer open (their MLS generations were
consumed), so nothing above the frontier is visible and the seat asks
nothing, while the sender's hourly budget for blind rounds may be spent.
Result: a seat many envelopes short with every surface green.

- **Open probe** (`OPEN_PROBE`, one switch; the visibility below does not
  depend on it). After the first replay of a session was read through, a
  seat asks every sender it holds history with for the listing of
  `(frontier, frontier + 1024]`, and asks again while a listing comes back
  full (a page of `GAP_LIST_MAX`, or the range's width). The probe ends on
  a short page or on a level answer: a sender with nothing above the
  asker's frontier answers an empty listing with `through == after` (new
  in v0.1.3; a v0.1.1/v0.1.2 receiver drops it as malformed, and never
  asks in that state anyway). A fresh frame does not end it - a frame
  re-published for another asker looks the same. Nothing ends it by time: an unanswered ask is judged only when the next one is due, and
  a sender known to answer listings (it answered one, or it reported a
  build - v0.1.2 and later) is asked again on 10 s doubling to 600 s until
  it answers. A sender not known to answer gets one ask, then silence until
  it is known. A runtime rebuild keeps the holds and the probe state.
- **Listed seqs are owed.** A listed seq is never settled by the 60 s order
  bound: the listing proves it exists, so it is asked for until it arrives,
  the sender answers `gone`, or three re-publications that did not land
  (`LISTED_TRIES`) give it up as lost, loudly. A listing wider than one
  window is refused, and a page never replaces a newer one held.
- **The gap path is not the hourly budget.** A want or a listing is
  answered under its own bound - 64 frames per asker per 10 s, 384 per
  minute over all askers, one listing per asker per 5 s, a re-published seq
  refused to the same asker for 60 s. Neither is the regression rewind of a
  heard peer's lower sheet. Over a slow relay or two askers a 256-seq page
  takes longer than a minute; nothing depends on it being faster.
- **Visible.** `status.behind` and `net_health` (`behind: <seat>`) name a
  sender this seat is short on: a listed own seq not here, or a full page
  not yet followed by a short one (at once); held content or an owed gap
  older than 10 s (a declined, parked seq is not owed). It clears only
  when the window has the listed tail and the last page was short. A sender
  whose spent budget holds a tail reports it in its own field:
  `status.resend_held` (the owed seats) and `resend_held_since`, and in
  `net_health` as `resend budget spent, owed: <seats>`; a granted round or
  a settled floor clears it. It is not a send failure and never counts
  toward the locked-out verdict.
- **v0.1.4 note.** A sender before v0.1.1 answers no listing: its tail
  still waits for its next frame or round. The held frames are memory-only,
  so a restart fetches them a second time. A sheet field carrying the
  sender's log end (additive) would show the gap without a probe, for every
  build that writes it.

Pins: `nostr_inbound_gap.rs`
(`a_restart_that_lost_a_far_ahead_hold_shows_it_is_behind_and_fetches_the_tail`,
`a_lost_probe_ask_is_asked_again`,
`two_seats_probing_one_sender_both_get_the_tail`), `nostr_open_probe.rs`
(`a_slow_sender_still_brings_the_whole_tail_and_behind_shows_until_it_has`),
`net::delivery::probe_tests`, `gap::tests::a_gap_want_is_bounded_and_never_names_seq_zero` (the level listing), `net::delivery_tests`
(`a_held_resend_is_its_own_note_and_never_a_lockout`,
`a_lying_or_stale_listing_changes_nothing`),
`group_runtime::tests::a_spent_budget_names_the_peers_owed_the_tail`.
