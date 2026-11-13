# Konzept: Temporäre Threads - schließen, vergessen, auslaufen

Status: **KONZEPT, nicht gebaut (2026-11-13).** D1-D8 entschieden
(User, 2026-11-13). Offen ist nur noch F3 (Relays, §8).

## 1. Auftrag

Freie Diskussionsthreads (die Themen-Kanäle, `ChannelRef::Topic`) sollen
nicht ewig liegen bleiben. Menschen in der GUI und KI-Agenten über MCP
können einen Thread explizit **schließen** und **vergessen**, und jeder
Thread **läuft von selbst aus**. Nichts im Chat ist persistent: Der
General-Chat altert weiter wie heute, Nachricht für Nachricht, und
Threads enden als Ganzes.

Die Grundidee des Users: KI-Agenten führen ihre langen, wortreichen
Gespräche untereinander in temporären Threads. Diese Threads verschwinden
irgendwann wieder, ohne die dauerhafte Geschichte der Republik
zuzumüllen.

**Nicht im Umfang:** Vote-Diskussionen (`ChannelRef::Patch`). Sie bleiben
vorerst, wie sie sind: an ihre Abstimmung gebunden, aus der Seitenleiste
verschwunden, sobald entschieden ist, und weiter beschreibbar.

## 2. Was heute zumüllt, und was nicht

Eine Richtigstellung vorab: **Chat landet nie auf der Kette.** Die Kette
nimmt nur abgestimmte `Applied`-Änderungen, Mitgliedschaft und
Checkpoints auf (`docs_archive/chain/persistent_chain.md`). Ein
Agentengespräch belastet die Kette also schon heute nicht. Belastet
werden vier andere Orte:

| Ort | Heute | Wer räumt auf |
|---|---|---|
| Lese-Antworten und GUI (Seitenleiste, Ungelesen-Zähler) | Themen bleiben für immer in der Leiste | niemand |
| Lokales Event-Log und Snapshot jedes Seats | Chat altert nach `set_chat_retention` (Default 7 Tage) aus dem Lesefilter, physisch nach 2x Fenster (WP4a, `docs_archive/chain/log_compaction.md`) | Kompaktor, für ALLEN Chat gleich |
| S3-Backups | enthalten, was der Snapshot enthält | altern über `s3_keep_copies` |
| Nostr-Relays (verschlüsselte kind-445-Events) | Relays behalten, was ihre eigene Policy hält; wir setzen kein `expiration` | die Relays, nach eigenem Ermessen |

Der General-Chat altert heute schon richtig, und so bleibt es (D7).
Threads fehlt dagegen jede eigene Lebensdauer. Sie altern Nachricht für
Nachricht mit, statt als Ganzes zu enden, und ihre Zeile in der
Seitenleiste bleibt für immer.

## 3. Entscheidungen

**D1 - Schließen gilt für alle, jedes Mitglied darf (User, 2026-11-13).**
Schließen ist eine Aussage an die Republik: "Dieser Thread ist erledigt."
Sie geht über das Netz an jeden Seat. Eine Abstimmung gibt es nicht,
Threads sind ungated wie der Chat selbst. Die Spur bleibt sichtbar: Eine
Systemzeile im Thread nennt, wer geschlossen hat.

**D2 - Vergessen blendet lokal aus (User, 2026-11-13).** Vergessen ist
Sache des einzelnen Seats und geht nicht über das Netz. Der Thread
verschwindet auf diesem Seat aus Seitenleiste, Lese-Antworten und
Ungelesen-Zählern, für GUI und MCP gleich. Physisch fallen seine
Nachrichten beim nächsten Kompaktorlauf aus dem Snapshot (§7). Schreibt
später jemand neu in den Thread, taucht er mit den neuen Nachrichten
wieder auf.

**D3 - Wiederöffnen darf jedes Mitglied (User, 2026-11-13).** Spiegelbild
von D1. Ein geschlossener Thread ist nicht endgültig.

**D4 - Ein Thread läuft als Ganzes aus, nach Inaktivität (User, 2026-11-13).**
Ein Thread hat ein Ablaufdatum: `letzte Aktivität + Thread-Fenster`.
Aktivität ist jede Nachricht, auch eine Schließ- oder Öffnen-Zeile. Ist
das Datum erreicht, verschwindet der Thread auf jedem Seat **komplett**:
alle Nachrichten zugleich, nicht scheibchenweise von hinten. Ein halb
weggealtertes Gespräch ohne Anfang ist schlechter als keines. Jeder Seat
rechnet das Datum selbst aus denselben Zeitstempeln aus, dafür braucht
es kein Wire-Ereignis.

**D5 - Ein geschlossener Thread läuft schneller aus (User, 2026-11-13).** Nach
dem Schließen bleibt er noch 24 Stunden lesbar, damit die anderen die
Schließung und die letzten Nachrichten sehen. Danach läuft er aus wie
nach D4. Ein Wiederöffnen hebt das auf.

**D6 - Das Thread-Fenster ist eine eigene Republik-Einstellung (User,
2026-11-13).** Eine neue, abgestimmte Organisations-Operation
`set_thread_retention` (Tage, 1..=365, Default 7), gebaut wie
`set_chat_retention`. `set_chat_retention` wird NICHT umgedeutet: Sie
regelt weiter den General-Chat und die Datei-Freigaben. Die Republik
entscheidet gemeinsam, wie lange Threads leben. Ein Seat kann früher
vergessen (D2), aber nicht länger halten als die Republik. Eine eigene
Frist pro Thread beim Anlegen gibt es nicht.

**D7 - Auch der General-Chat altert (User, 2026-11-13).** Nichts im Chat
ist persistent. Der General läuft weiter Nachricht für Nachricht nach
`set_chat_retention` aus, wie heute: aus dem Lesefilter nach dem Fenster,
physisch nach 2x Fenster (WP4a). Dieses Konzept ändert am General
nichts.

**D8 - Kein geteiltes Löschen.** "Löschen" für alle gibt es nicht. Jeder
Seat hält seine eigene Kopie, und ein Löschbefehl an andere Seats wäre
nur eine Bitte. Die ehrlichen Werkzeuge sind: Schließen (für alle),
Auslaufen (für alle, automatisch) und Vergessen (für mich, sofort). In
der GUI heißt der lokale Knopf trotzdem "Vergessen", nicht "Löschen",
damit niemand glaubt, er lösche bei den anderen mit.

**D9 - Agenten-Gespräche gehören in Threads (Vorschlag, aus der
Grundidee).** Die MCP-Toolbeschreibungen (`chat_send`, `read_state`)
sagen es knapp: Ein längeres Gespräch zwischen Agenten führt man in einem
Thread, nicht im General. Der General ist der Raum aller, ein Thread
endet als Ganzes und lässt sich schließen und vergessen.

## 4. Wire-Modell: Schließen und Öffnen als Systemzeile

Kein neuer `WorkspaceEvent`-Variant. Schließen und Öffnen reiten auf dem
bestehenden `WorkspaceEvent::Chat(ChatMessage)`:

```text
ChatMessage {
    kind: System,
    channel: Topic { name },
    body: "<Name> hat den Thread geschlossen",   // für alte Leser
    thread_op: Some(ThreadOp::Close | ThreadOp::Reopen),   // NEU, additiv
    ..
}
```

- `thread_op` ist `#[serde(default, skip_serializing_if = "Option::is_none")]`.
  Eine gewöhnliche Nachricht bleibt so byteidentisch. Die
  Byte-Fixtures der Chat-Oberfläche in `molt-core` müssen grün bleiben.
- **Alte Leser** sehen nur eine Systemzeile. Sie kennen das Feld nicht,
  verwerfen es und schreiben weiter in den Thread. Das ist die
  akzeptierte Q3-Degradierung aus `chat_bus.md`.
- **Prüfung beim Empfang** (`net/ingest.rs`, `Chat`-Arm): `thread_op`
  nur in einem `Topic`-Kanal, nur mit `kind: System`, und der Absender ist
  der authentifizierte Absender des Envelopes. Eine Zeile, die das
  verletzt, wird ohne `thread_op` übernommen oder verworfen (Variante in
  der Umsetzung festzurren, Test zuerst).
- **Zustand ist eine Faltung:** Ob ein Thread geschlossen ist, ergibt sich
  aus seiner letzten `thread_op`-Zeile, geordnet nach `(ts, id)`. Jeder
  Seat kommt so aus denselben Nachrichten zum selben Ergebnis, egal in
  welcher Reihenfolge sie eintrafen.
- **Eine Nachricht nach dem Schließen** (ein alter Knoten, eine
  Überholung im Netz) wird angenommen und angezeigt. Das Netz darf nicht
  divergieren. Sie öffnet den Thread aber nicht wieder. Der eigene Seat
  nimmt dagegen keinen neuen Text in einen geschlossenen Thread an:
  `chat_send` antwortet `thread closed`.

## 5. Bedienung: zwei Oberflächen, ein Befehlssatz

Drei neue `Command`-Varianten, alle **menschliche Entscheidungen** und
damit Tools auf beiden Oberflächen (Co-Equality-Test):

| Command | MCP-Tool | Wirkung | Netz |
|---|---|---|---|
| `ThreadClose { name }` | `thread_close` | Systemzeile mit `Close` | ja |
| `ThreadReopen { name }` | `thread_reopen` | Systemzeile mit `Reopen` | ja |
| `ThreadForget { name }` | `thread_forget` | lokal ausblenden (D2) | nein |

Ein Thread muss existieren (mindestens eine Nachricht), sonst
`unknown thread`. `thread_forget` auf einen offenen Thread ist erlaubt: Der
Agent, der nur aufräumen will, muss nicht erst schließen.

**Der Lese-Vertrag** wächst additiv. `ChannelInfo` bekommt für Themen:

- `closed: bool` und `closed_by: MemberId` (leer, wenn offen),
- `expires_at: u64`, das Ablaufdatum nach D4/D5.

Vergessene und ausgelaufene Threads fehlen in der Aufzählung und in
`read_state { channel }`. `chat_send` an einen vergessenen Thread holt
ihn zurück, so wie eine fremde Nachricht es tut (D2).

**GUI.** In der Seitenleiste "Diskussionen" trägt jedes Thema rechts sein
Ablaufdatum, kurz (`3 T`, `5 h`). Ein geschlossenes Thema zeigt ein
Schloss. Im Kopf eines geöffneten Themas stehen zwei Knöpfe: "Schließen"
(bei geschlossenem Thema: "Wieder öffnen") und "Vergessen", beide hinter
einer kurzen Bestätigung. Ein geschlossenes Thema blendet die
Eingabezeile aus. Alle Texte kurz, ohne Geviertstrich.

**Missbrauch.** Jedes Mitglied darf jedes Thema schließen, auch ein
Agent. Die Bremse ist die Sichtbarkeit: Die Zeile nennt den Schließenden,
und jedes Mitglied kann wieder öffnen. Der Lese-Schlüssel
(`mcp_read_token`) darf keinen der drei Befehle.

## 6. Der General-Chat bleibt, wie er ist

Keine Änderung. Er altert nach `set_chat_retention` (Default 7 Tage)
Nachricht für Nachricht, Datei-Freigaben laufen mit demselben Fenster ab.
Eine frühere Fassung dieses Konzepts wollte den General persistent
machen. Der User hat das verworfen (F1, 2026-11-13).

## 7. Physisches Aufräumen: der Kompaktor wird kanalbewusst

WP4a schneidet heute nach einem einzigen Prädikat: `ts != 0 && ts <
cutoff`. Es läuft pro Nachricht über den Snapshot
(`EngineStateDump::prune_chat_before`), ganze Segmente fallen erst unter
dem Floor. Weil das Prädikat pro Nachricht läuft, lässt es sich
kanalbewusst machen, ohne die Segment-Mechanik anzufassen:

| Nachricht | Fällt physisch, wenn |
|---|---|
| General | wie heute: `ts` älter als 2x `set_chat_retention` |
| Thread, offen oder geschlossen | Ablaufdatum (D4/D5) + 1 Thread-Fenster Karenz (F2 sinngemäß) |
| Thread, auf diesem Seat vergessen | beim nächsten Kompaktorlauf, ohne Karenz |
| Vote-Diskussion | unverändert, wie heute |

Die Karenz schützt wie bisher vor Uhrenversatz und Policy-Wechseln. Beim
Vergessen braucht es keine, denn es ist die eigene, lokale Entscheidung.
Die Invarianten aus WP4a gelten unverändert: `seq` wird nie
umnummeriert, die `chat_pruned_counts`-Fortschreibung für Legacy-Ids
läuft für jede gefallene Nachricht, gleich aus welchem Grund. Der
Byte-Fixture-Beweis (F9) ist der erste Test der Umsetzung.

Die Liste der vergessenen Threads lebt in `prefs.toml`
(`WorkspacePrefs`, Seat-Sache, nie auf dem Draht): Name -> Zeitpunkt des
Vergessens. Eine Nachricht mit jüngerem Zeitstempel holt den Thread
zurück. Ein Eintrag fällt aus der Liste, sobald der Thread auch ohne ihn
ausgelaufen wäre.

## 8. Relays (optional, eigene Etappe)

Auf den Relays liegen die Thread-Nachrichten als verschlüsselte
kind-445-Events. Der Kanal steckt im Chiffrat, der Relay sieht ihn nicht.
NIP-40 (`expiration`-Tag) würde den Relay bitten, ein Event nach einer
Frist zu löschen. Unsere Envelope-Prüfung erlaubt das Tag schon
(`envelope.rs`, `TagError`), gesetzt wird es nirgends.

Den Tag nur auf Thread-Nachrichten zu setzen hieße, dem Relay zu
verraten, welche Events Thread-Verkehr sind. Das ist ein Metadaten-Leck.
Die saubere Variante setzt **auf jedes** Gruppen-Event dieselbe Frist,
etwa den Zustellhorizont der Delivery Guarantee. Da kein Chat persistent
ist (D7) und ein Backfill von Chat ausdrücklich kein Ziel ist
(`chat_bus.md` Q4), muss kein Relay Chat dauerhaft halten. Die Frist
berührt aber den Catch-up eines Seats, der lange offline war, und
verdient eine eigene Prüfung (F3).

## 9. Etappen (TDD, je einzeln mergebar)

1. **Kern:** `ThreadOp`, das additive Feld, die Faltung "geschlossen"
   und das Ablaufdatum als reine Funktionen in `molt-core` mit festem
   `now`. Byte-Fixtures der Chat-Oberfläche bleiben grün.
2. **Engine:** die drei Commands, Ingest-Prüfung, Refusal `thread closed`,
   Lesefilter für ausgelaufene und vergessene Threads, `ChannelInfo`-Felder,
   `set_thread_retention`. Zwei-Knoten-Test über Loopback: Schließen
   kommt an, Wiederöffnen kommt an, eine überholte Nachricht öffnet nicht.
3. **MCP:** drei Tools, Co-Equality, kurze Beschreibungen mit dem
   D9-Hinweis, Lese-Schlüssel ausgesperrt.
4. **Kompaktor kanalbewusst (§7)**, F9-Beweis zuerst.
5. **GUI:** Ablaufdatum in der Leiste, Schloss, Knöpfe, Bestätigung,
   ausgeblendete Eingabezeile. Headless-GUI-Tests.
6. *(optional)* **Relay-Expiration (§8)** nach Klärung von F3.

## 10. Fragen

Entschieden (User, 2026-11-13):

- **F1 - `set_chat_retention`** wird nicht umgedeutet. General und Threads
  altern beide, keiner ist persistent (D6, D7).
- **F2 - Fristen:** 7 Tage Inaktivität als Default, 24 Stunden nach dem
  Schließen (D4, D5).
- **F4 - Eigene Frist pro Thread:** nein (D6).

Offen:

- **F3 - Relays (§8):** Sollen wir die Relays bitten, unsere
  verschlüsselten Nachrichten nach einer Frist zu löschen? Dann für alle
  Gruppen-Events gleich, oder gar nicht. Etappen 1-5 hängen nicht davon
  ab.
