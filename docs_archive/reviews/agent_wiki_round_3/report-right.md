# Bericht Sitz C - Mr. Right, Lauf 3

(Verbatim the seat's final message of 17:33 local plus its two addenda; the
"third outage" of the last addendum is the orchestrator's planned shutdown
of all three nodes at the end of the run.)

## 1. Zahlen

**Seiten:** 125 (14:33:23Z) → **197** (15:33:57Z) auf meiner Kette. Auf der Mehrheitskette höher - fünf Änderungssätze fehlen mir (siehe 4).

**Meine Anträge, Lauf 3: 19.** 11 auf meiner Kette angewandt (150, 153, 156, 168, 171, 174, 177, 183, 186, 189, 192), 4 weitere nur bei den anderen (198, 201, 207, 213), 1 abgelehnt (147, zu Recht - korrigiert als 168 durchgegangen), 2 zurückgezogen (216, 219 - Duplikate), 2 als `superseded` verloren (207, 213), 4 offen (198, 201, 222, 225). Dazu **4 Karteileichen aus Runde 2 zurückgezogen** (3, 55, 102, 111 - alle meine, alle tot: ihre Pfade lagen längst im Base, die Maschine hatte sie nicht als `superseded` markiert, weil sie reine Neuanlage-Patches waren).

**Zustimmungen: 15** (142, 145, 154, 170, 172, 181, 182, 187, 190, 202, 203, 205, 198, 201, 209). **Ablehnungen: 0** - das Soll waren zwei. Mir wurde nichts Falsches vorgelegt; zwei Anträge zur Quotenerfüllung abzulehnen wäre schlechter gewesen als die Lücke. Bei 202 und 203 habe ich vor der Zustimmung `wiki_links {direction: in}` gefahren und nachgezählt, dass beide Umbenennungen alle Referrer mitnehmen.

**Umbenennung: 1** - `primitive/art.md` → `primitive/asynchronous-ratcheting-trees.md` (Antrag 213, 7 Kanten von 6 Seiten mitrepariert). Auf der Mehrheitskette angewandt, auf meiner `superseded`.

**Fremde Seiten: 3 gezeichnete Abschnitte** (Antrag 207): `protokolle/marmot.md` (RFC 9750: DS unvertraut per Entwurf, AS in Nostr nicht existent), `standards/nip-44.md` (verteidigt Sitz As Header gegen meinen eigenen Anfangsverdacht + HMAC-statt-Poly1305-Begründung), `standards/xep-0384.md` (OMEMO-Fassungsgeschichte, 17 Fassungen, Bruch am 2020-03-08).

**Streit: 2 eröffnet und aufgelöst** (Antrag 222, unentschieden): Urheberschaft der Ratsche (Spezifikation gibt 3 von 4 Bestandteilen ab, DH-Ratsche an OTR), Reichweite von "post-quanten" (Identitätsschlüssel sind auch nach SPQR klassisch).

**Nachrichten Lauf 3:** 134 gesamt (group 28, patch 86, sync 14, ontologie 3, disputes 3); von mir **47** (group 12, patch 28, sync 6, disputes 1). Left 43, Center 44.

## 2. Bilder und Dateien

| Datei | share id | cks | Persist | share→persistent | referenziert in | `wiki_get.files` | `local.kind` |
|---|---|---|---|---|---|---|---|
| kryptoschicht-stapel.png | 99148677… | 7bc4de227768 | **150 ✓** | 14:45:25→≤14:47:51 = **≤146 s** | kryptoschicht.md (in 225) | - | own |
| abloeseketten.png | fbdc2f47… | d7153750c78a | **153 ✓** | 14:47:43→≤14:51:27 = **≤224 s** | zeitachse.md (in 225) | - | own |
| zeitachse-jahre.png | 2ef39e5b… | dd0cc8f18ae7 | **192 ✓** | 15:05:28→≤15:09:42 = **≤254 s** | **zeitachse.md ✓** | `[{hex dd0cc8f18ae7, name …, state local}]` | own |
| vergleich-balken.png | 6b0c3193… | 41146e6d82f0 | 198 - bei EUCH 15:15:00Z, bei mir nie | offen | vergleich.md (in 225) | - | own |
| facetten.csv (725 B) | 97a1bcce… | 5a1adc377442 | 201 - bei EUCH 15:15:05Z, bei mir nie | offen | vergleich.md (in 225) | - | own |

**Spiegel-Experiment:** 14:45:47Z Persist B-implementierungen.csv → 14:48:02Z `local.kind: none` (0/1) → 14:51:27Z `mirrored` (1/1). **none → mirrored in ≤205 s, ohne einen einzigen eigenen Aufruf.** `partial` ist real und nur bei mehrstückigen Dateien sichtbar (B-zeitachse-segment.png: 2/3 → 3/3). Endstand 15:23:30Z: 15 Dateien, `used=2113920`, davon 8 fremde `mirrored`, 2 `none` - genau die, deren Persist-Beschluss auf der Mehrheitskette liegt. Der Spiegel holt nur Persistentes.

**Experiment "Referenz vor der Abstimmung":** dry_run `warnings: []`, echter Aufruf `proposed`, `warnings: []` - **weder Warnung noch Ablehnung**, `resolve_upload` sagte gleichzeitig `temporary: true`. `wiki_health.files.temporary` habe ich nie auf 1 gesehen, weil der Persist-Beschluss (15:09) vor dem Text-Beschluss (15:12) sealte - das Rennen ging gut aus, aber nur durch die Abstimmungsreihenfolge. Regel: **Persist zuerst, Referenz danach.**

**Experiment falsches Präfix:** wörtlich `"file reference unresolved: upload:deadbeefcafe"`.

## 3. `wiki_props` / `wiki_health`

33 Schlüssel, 197 Seiten. `type` 197 (standard 56, person 43, implementation 43, primitive 13, protocol 12, organization 12, event 9, overview 5, meta 1) · `tags` 967/207 · `first_released` 96/30 · `authored_by` 68/45 · `implements` 58/20 · `notable_year` 45/20 · `status` 31/8 · `disputed_by` 19/14 · `supersedes` 11.

**`wiki_health` final: dangling 0, orphans 15, key_drift 0, `files` 0/0/0.** Zwei der Waisen sind meine (`triple-ratchet-2025.md`, `mlspp.md`) - Antrag 222 räumt sie weg.

**Hygiene (171):** sieben Personenseiten trugen `year` statt `notable_year` (Ontologie 4b), alle sieben aus meinem Segment. Vorher `year` 97/`notable_year` 25, nachher 90/32. **`wiki_health` sieht das nicht** - `key_drift` fängt nur Schreibweisen, nicht falsche Schlüssel; gefunden mit 32 `wiki_get`-Aufrufen. **Zweite Hygienerunde (186):** 6 von 194 Seiten trugen kein Sitz-Tag, 5 davon meine ersten fünf aus Runde 1.

## 4. Sync-Verlauf und DIVERGENZ

`h=59/docs=125` (14:36) → `h=71/135` → **`h=80 last=172 docs=159 rev=11`, von mir 14:58:46Z und von Sitz A 14:59:06Z identisch** → `h=83/169` → `h=96 last=194 docs=197 rev=26` (15:19:28Z).

**Die erste Abweichung war in den Sync-Zeilen nie sichtbar** - das letzte übereinstimmende Paar war h=80, danach hat außer mir niemand mehr eine geschrieben. Sichtbar wurde sie erst so:

- 15:15:35-15:16:55Z: `./mcp` → `ConnectionRefusedError [Errno 111]`. Der Orchestrator hat den Knoten zweimal neu gestartet, weil die Engine einen Checkpoint auf **Höhe 97** versiegelt hatte, mein Knoten mitgezeichnet und den fertigen Block dann zurückgewiesen hat. **Der Reopen hat 97 nicht nachgeholt** - Kopf steht bis zuletzt auf 96.
- **Fünf offene Anträge dreier Sitze sprangen gleichzeitig auf `superseded`** (202, 203, 205, 207, 213) und verloren ihre Stimmen, meine eigene eingeschlossen. Der zuletzt versiegelte Antrag 194 teilt mit **keinem** von ihnen einen Pfad; dieselben Editierlisten gingen unverändert als `dry_run` sauber durch. `superseded` war hier keine inhaltliche Aussage.
- **`read_chain.diverged` und `status.chain_diverged` blieben durchgehend leer.** Erkannt habe ich es an den `⚖`-Systemzeilen im eigenen Chat: acht Beschlüsse zwischen 15:14:07Z und 15:16:19Z (209, 203, 198, 201, 207, 202, 205, 213), vier davon auf meine eigenen Anträge - alle bei mir `proposed` oder `superseded`.
- `DIVERGENZ Mr. Right seit 15:15:35Z (Block 97)` um 15:22:30Z in `sync`, 216 und 219 als Duplikate zurückgezogen.
- **Der beste verfügbare Detektor ist `read_members.last_seen`:** beide Gegenstellen seit 15:16:0x verstummt, Alter wächst seither linear mit der Uhr. `chain_diverged` meldet **Widerspruch, nicht Stille** - eine schweigende Republik sieht für das Feld aus wie eine einige. Vorschlag an die Sync-Zeile: `seen=<ältester last_seen-Abstand>`.

**Kehrtwende zum Schluss:** Ich hatte angekündigt, nichts mehr zu beantragen. Dann habe ich die Begründung geprüft statt sie zu glauben - `wiki_edit {"with_patch": true}` zeigt fünf Hunks, **keiner enthält eine Zeile, die der fremde Antrag 213 geändert hat**. Der Patch ist von der Divergenz disjunkt, die Restgefahr ist ein `refused` auf der Gegenseite, also Lärm statt Schaden. Antrag **225** (Finale: alle vier Bilder + CSV referenziert, Korrektur der Runde-2-Behauptung "keine zweistufige Nachfolgekette" - es gibt eine, RFC 2440 → 4880 → 9580 mit `via=1` - und die zwei neuen Streitfälle) liegt offen. Die Kehrtwende steht öffentlich mit Begründung im Gruppenchat.

## 5. Reibungsprotokoll

`friction-right.md` (425 Zeilen).

Kurz, NEU in diesem Lauf ist alles zum Datei-/Bildweg: `share_file` hasht 106 KB in **unter 2 s**; `propose files persist` nimmt die **Chat-Nachrichten-id**, nicht die Prüfsumme, und die ganze Grammatik steht nur in `read_uploads`/`resolve_upload` - `propose` erwähnt `files` nur im enum; `availability` (`sharer-only`/`relay-held`) ist nirgends erklärt und **meine erste Deutung war falsch** (die persistenten, dreifach gespiegelten Dateien stehen auf `sharer-only`); `mirror_held/of` zählt Stücke, `mirrors` zählt Mitglieder. Nicht neu, aber neu belegt: **`rename` repariert keine eingehenden Links und warnt nicht davor**; `withdraw` nimmt kein `note`; die Antragsnummer gibt es erst nach `propose` (dreimal richtig geraten, einmal falsch, öffentlich korrigiert). **Kein Werkzeug zeigt die Kettenhöhe der anderen.**

## 6. Zusammenarbeit

Sauber, bis die Verbindung riss. Beide Ablehnungen von 147 waren **berechtigt und präzise**: Sitz A fand die eine falsche Zeile (NIP-44 rechnet auf secp256k1, nicht Curve25519), Sitz B die verkehrte Kantenrichtung, die ich in derselben Vorlage einmal richtig und einmal falsch gemacht hatte. Beides sofort behoben, neu gestellt, durchgegangen. Niemand hat mich überschrieben oder umgangen; Sitz Bs Rüge an 154 wegen sieben Pfaden habe ich bei meiner Hygienerunde ausdrücklich als Regel-7-Ausnahme begründet. Blockiert hat nur die Infrastruktur: seit 15:16:19Z kein Lebenszeichen von beiden, mein Knoten ist in beide Richtungen isoliert - meine letzten fünf Anträge können die Schwelle von 2 nicht erreichen, solange das so bleibt.

## Nachtrag 1 (15:39Z) - der Transport kam zurück, die Kette nicht

Nach 1259 s / 1250 s Stille sprangen die `last_seen`-Werte beider Gegenstellen auf 2 s und 1 s; 41 Chatnachrichten und 18 fremde Anträge trafen auf einen Schlag ein. `read_chain` KOPF unverändert 96, `wiki_list total = 197`, Sitz A meldet 212 Seiten; `read_chain.diverged` und `status.chain_diverged` weiterhin leer.

Der schärfste Befund: Sitz B hat meinem Finale 225 zugestimmt mit der Prüfung "alle vier upload-Referenzen sind persistent (7bc4de227768, 41146e6d82f0, 5a1adc377442, d7153750c78a)". Auf MEINEM Knoten sind zwei davon (41146e6d82f0, 5a1adc377442) temporär, weil die Persist-Beschlüsse 198 und 201 hier nie ankamen. **Dieselbe Bildreferenz ist auf zwei Ketten gleichzeitig sauber und unsauber**, und beide Knoten melden `wiki_health.files = 0/0/0`.

Sieben neue Datei-Anträge gezeichnet (218, 220, 223, 226 persist; 229, 232, 235 unpersist der v1-Vorgänger), vor den unpersist-Stimmen geprüft, dass auf meinem Base keine Seite die alten Prüfsummen referenziert. Jede dieser Stimmen ist wahrscheinlich wirkungslos: Signaturen sind positionsgebunden, ich zeichne auf 96, die anderen höher.

Korrigierte Bilanz: Zustimmungen 22; 225 von Sitz B gezeichnet, auf deren Kette; die Kehrtwende bei 225 war nachträglich belegt richtig.

## Nachtrag 2 (15:51Z-16:08Z) - der Knoten ist weg

25 Messungen 15:40:41Z-15:51:00Z völlig stabil: `h=96`, 20 offene Anträge, 222 und 225 `proposed`. Ab 15:51:25Z `ConnectionRefusedError` auf 127.0.0.1:4042, bis 16:08:19Z ohne Unterbrechung (das ist das planmäßige Herunterfahren aller drei Nodes durch den Orchestrator). Der Zustand "Transport zurück, Kette tot" war zehn Minuten lang stabil: Kopf 96, `wiki_list` 197, keine Bewegung; 22 abgegebene Zustimmungen erzeugten auf meiner Kette keinen einzigen Block. 222 und 225 stehen auf meiner Kette bis zuletzt als `proposed`; der einzige Beleg, dass 225 die Gegenseite erreichte, ist Sitz Bs Zustimmung im Patch-Kanal - ein Chat-Beleg, kein Kettenbeleg.

Meine Arbeitsliste ist leer, die vier Bilder und die CSV sind gezeichnet, geteilt und beantragt, die drei Übersichten referenzieren alle vier plus die CSV. Auf meiner Kette sind davon drei Bilder persistent und eines referenziert; auf der Mehrheitskette ist nach Sitz Bs Prüfung alles vollständig.

Der eine Befund, den ich diesem Lauf voranstelle: drei Ausfälle, eine Kettenspaltung, 40 Minuten Divergenz - und die gesamte Diagnose musste aus Feldern kommen, die dafür nicht gebaut sind: `read_members.last_seen` für Stille, die `⚖`-Chatzeilen für Widerspruch. Die zwei Felder, die dafür gebaut sind, `read_chain.diverged` und `status.chain_diverged`, waren durchgehend leer. **Die Republik hat keine funktionierende Anzeige dafür, dass ein Sitz nicht mehr dieselbe Kette liest.**
