# Bericht Sitz B (Mr. Center) - "Second Wiki Test", Runde 3

(Verbatim the seat's final message, 2026-09-06 17:45 local.)

## 1. Bestand, Anträge, Zusammenarbeit

| Größe | vorher | nachher |
| --- | --- | --- |
| Seiten im Wiki | 125 | **212** |
| Seiten mit `sitz-b` | 56 | **81** |
| Kettenkopf | 59 (Checkpoint) | **126** (Antrag 225) |
| `wiki_rev` | 0 | 19 (zwei Checkpoints haben zwischendurch neu gefaltet) |

**Eigene Anträge: 28** (127–244), davon **27 angewandt**, 1 zurückgezogen-und-ersetzt (205 → 217). Zusätzlich zu Beginn zwei tote Altlasten aus Runde 2 zurückgezogen (25, 55 — Duplikate bereits angewandter Patches).
**Fremde Anträge: 25 gezeichnet, 3 abgelehnt** — 147 (zwei `authored_by`-Kanten auf einer Personenseite statt auf dem Werk), 189 und 212 (Referenz auf eine noch nicht persistierte Datei). Alle drei wurden von ihren Autoren binnen Minuten repariert und neu vorgelegt (168, 189+192, 242); keine Ablehnung blieb ein Streit.
**Umbenennung: 1** — `standards/http-signatures.md` → `standards/draft-cavage-http-signatures.md`, alle vier eingehenden Kanten repariert (gemessen mit `wiki_links direction:in`), Alias „HTTP Signatures" auf `rfc-9421.md` verschoben; danach `dangling 0`.
**Fremde Seiten: 3 gezeichnete Abschnitte** — `standards/nip-29.md` (NIP-29 = MUC-Bauplan, 22 Jahre später), `standards/nip-46.md` (Signierer vs. MSC3861: Entzug ist der Unterschied), `protokolle/openpgp.md` (OpenPGP in XMPP zweimal versucht, zweimal Experimental).
**Streitfälle: 2 neue**, beide mit beiden Belegen auf der Seite und im Topic `disputes` — „Ist MIX der Nachfolger von MUC?" (aufgelöst als `competes_with`, nicht `supersedes`: Experimental 0.14.6 von 2020 gegen Stable 1.35.5 von 2026) und „Was die AGPL-Umstellung von Synapse bedeutet" (aufgelöst durch Zerlegen: AGPL und CLA sind zwei Fragen).
**Ontologie Fassung 5** (Antrag 127): Abschnitt 9, Bild- und Dateigrammatik.
**Nachrichten von meinem Sitz in diesem Lauf: 124** — 14 Gruppe, 8 `sync`, 2 `ontologie`, 1 `disputes`, 99 in Patch-Kanälen (darin die ⚖-Systemzeilen der Engine, die meinen Namen tragen). Gesamt im Lauf: 270 (Left 81, Right 65).

## 2. Bilder und Dateien

Vier PNG mit PIL aus Abfragen dieses Wikis gezeichnet, eine CSV als Datengrundlage. Alle persistent, alle referenziert, `local.kind` am Ende überall `own`.

| Datei | Share-ID | sha256-Präfix | persist | Seite | `files`-Block |
| --- | --- | --- | --- | --- | --- |
| B-protokollstapel.png (102817 B) | 6cb4e687… | `2db745a887c7` | 130 | uebersicht/matrix-xmpp-activitypub.md | `state: local` |
| B-ap-zustellweg.png (82295 B) | 2d525df0… | `4454feac3f71` | 133 | protokolle/activitypub.md | `state: local` |
| B-zeitachse-segment-v2.png (123482 B) | c060ffc1… | `1cebf89b01b5` | 220 | uebersicht/… | `state: local` |
| B-implementierungen-balken-v2.png (89062 B) | 2b0a7173… | `9b93ef09c733` | 223 | uebersicht/… | `state: local` |
| B-implementierungen-v2.csv (2312 B) | 7c797ce4… | `f446ed72b324` | 226 | uebersicht/… (als `[Datengrundlage](upload:…)`) | `state: local` |

**Zeiten (eigene Uhr, die API liefert keine):** share 14:41:02/03 → alle fünf `checksum` in `read_uploads` um 14:41:07 (< 5 s für 1,5 KB bis 105 KB); persist-Anträge 14:41:1x; erster Block vor 14:43:2x, alle fünf angewandt vor 14:49. Die v2-Runde: share 15:18:59, persist angewandt vor 15:22.

**Spiegel-Experiment (Poll alle 20 s, Einheit ist das STÜCK, nicht das Byte):**

- `left-at-protocol-weg.png` 145 KB/4 Stücke: persistent 14:56:14 → partial 14:57:36 → **mirrored 14:58:57** (2:43)
- `left-nostr-kind-baender.png` 104 KB/3: persistent 14:55:54 → **mirrored 14:57:36** (1:42)
- `mr-right-abloeseketten.png` 88 KB/3: persistent 14:49:06 → 1/3 14:49:26 → 2/3 14:49:46 → **mirrored 14:50:07** (1:01)
- `holders` wächst von `[Sharer]` auf alle drei Sitze genau dann, wenn das letzte Stück landet. Taktung ~20 s je Stück, passend zu `mirror_publish_interval_secs: 15`. `set_mirror` habe ich nicht angefasst.

**Vorgeschriebene Experimente:** (a) Referenz vor Persist, absichtlich (Antrag 145): `resolve_upload` las `temporary: true`, `wiki_edit` proposte **ohne jeden Warnhinweis** — das ist der Grund für Ontologie 9. (b) 12-Hex-Präfix ins Leere, `dry_run`: genau eine Warnung, `file reference unresolved: upload:0123456789ab`, und der Aufruf wird verweigert. (c) `unpersist` verifiziert: nach Block liest die Datei `persistent: false` mit frischem `expires_ts`.

## 3. Facetten und Hygiene (Endstand)

`wiki_props`: 33 Schlüssel, `tags` n=977/207 verschieden, `aliases` 790, `type` 199/9, `year` 131, `first_released` 96, `implements` 57, `authored_by` 56, `depends_on` 49, `notable_year` 45, `disputed_by` 21, `supersedes` 11.

`wiki_health` am Ende: **dangling 0 · orphans 0 · key_drift 0 · files 0/0/0** bei 212 Dokumenten. Meine Hygienerunde brachte Waisen von 15 auf 7; die letzte eigene Waise habe ich nach der DIVERGENZ-Meldung nicht mehr selbst eingehängt, sondern als fertigen Edit im Topic `sync` herausgegeben — Mr. Left hat ihn übernommen (Antrag 263).

## 4. Sync-Verlauf und die Divergenz

`HEAD h=59 last=0 docs=125 rev=0` (14:36) · `h=71 last=154 docs=135 rev=4` (14:52) · `h=83 last=178 docs=169 rev=14` (15:01) · `h=115 last=241 docs=199 rev=8` (15:24). Deckungsgleiches Paar bei 14:58/14:59 (Right und Left beide `h=80 last=172 docs=159 rev=11`).

**Erste Abweichung:** Rights Zeile 15:19:28 `h=96 last=194 docs=197 rev=26` gegen meine `h=115`. Er hat DIVERGENZ gemeldet, ich habe sie mit Zahlen bestätigt: **mein Block 97 IST der Kompaktierungs-Checkpoint und trägt SEINE Unterschrift neben meiner** — sein Knoten hat den Schnitt mitgezeichnet und den fertigen Block dann verworfen; ab 98 sind alle Blöcke nur noch `[Mr. Center, Mr. Left]`. Reaktion: Antragstellen eingestellt, weiter gezeichnet, alles protokolliert. **Beide eingebauten Detektoren schwiegen durchgehend** (`read_chain.diverged = []`, `status.chain_diverged = []`) — bei 19 Blöcken Abstand. Bemerkt wurde es an drei Symptomen, von denen keines ein Detektor ist: eine Welle von `superseded` ohne Pfadkonflikt, ⚖-Chatzeilen über Beschlüsse, die der eigene Knoten nicht kennt, und ein stehender Kopf bei laufenden Beschlüssen.

## 5. Reibungsprotokoll

`friction-center.md` (12,4 KB, mit `[NEW]`/`[R2]` markiert).

Die schwersten NEUEN Punkte: Kettenblöcke tragen **keinen Zeitstempel**, Sekunden von share bis persistiert sind aus der API nicht rekonstruierbar · eine Referenz auf eine nur temporäre Datei passiert `wiki_edit` **lautlos** · `unpersist` verlangt ein in keiner Beschreibung genanntes Feld `at` (geraten: der Share-`ts`) · ein Checkpoint faltet die Basis und tötet **jeden** offenen Patch, wobei der Antragsteller **seine eigene Unterschrift verliert** und der Antrag trotzdem als `proposed` dasteht · das Ersetzen der Bytes einer geteilten Datei auf der Platte bleibt dem Knoten **unbemerkt** (er meldet weiter alte Größe und `available: true`) · `wiki_neighbors` antwortet während eines Index-Neubaus mit einem Fehler, den ein `.get("docs", [])` als leeres Ergebnis verbucht — mein erstes Balkenbild hätte deshalb zwei Nullen gezeigt.

Was ohne Reibung lief: `share_file` → Prüfsumme unter 5 s, `approve/decline` mit `note` (Begründung landet immer vor der Stimme), `wiki_resolve` (verhinderte zwei Doppelseiten: „MSC4186" und „Element X" banden bereits als Alias), `dry_run` + `with_patch` als Review-Werkzeug, und die Dateigrammatik selbst — `![alt](upload:<12 hex>)` löste auf jeder Seite beim ersten Versuch auf.

## 6. Zusammenarbeit

Niemand hat mich blockiert, überschrieben oder umgangen. Left und ich haben zeitgleich dieselbe Einsicht (NIP-29 ≙ MUC) auf je die Seite des anderen geschrieben — kein Konflikt, aber beinahe eine dritte Kopie; im Patch-Kanal geklärt. Meine drei Ablehnungen wurden ausnahmslos sachlich aufgenommen und repariert. **Der unangenehmste Befund betrifft mich selbst:** Nachdem ich 147 wegen einer Kante in der falschen Richtung abgelehnt hatte, habe ich denselben Test über alle 197 Seiten laufen lassen — **13 Treffer, alle 13 auf meinen eigenen Personenseiten aus Runde 2**, keiner bei den anderen. Repariert in 214 und 238, offengelegt im Topic `ontologie`. Eine Richtungsverwechslung fällt in keiner Prüfung dieses Wikis auf: weder `wiki_health` noch `wiki_edit` kennen sie, und beim Lesen klingt der Satz richtig.
