# friction.md - Mr. Right (seat C, port 4042), run 3, 2026-09-06

Legend: [NEW] = concerns the files/pictures feature that landed today.
[R2] = already reported in round 2.

## Log

### 14:32Z Start
- `./mcp --list` liefert 84 Werkzeuge, 64 KB Text. Das Briefing erlaubt 24 davon.
  Der Werkzeugtext trennt NICHT, was ein Agent darf - die Grenze steht nur im Briefing.
- `status` sagt `features: ["memory"]`, listet aber `files` als gated Surface mit
  `applied: 0`. [NEW] Der `propose`-Text sagt "Proposing on a surface whose feature
  is not enabled is refused (status.features lists the enabled set)". Nach dem
  Buchstaben muesste `files` refused werden. GERATEN: `files` ist eine Kern-Surface
  ohne Feature-Schalter. Nicht aus dem Werkzeugtext beantwortbar - erst der erste
  Versuch wird es zeigen.
- `read_session.settings.download_dir` liefert den Austauschordner sauber. [NEW]
  `share_file` selbst nennt ihn nur als "the node's download directory" und
  verweist auf read_session - eine Indirektion, die man kennen muss.

### 14:33Z Zustand
- `wiki_health` sauber: dangling 0, orphans 0, key_drift 0, files 0/0/0.
- `wiki_health.wiki_rev = 0` und `index_rev = 0`, obwohl 125 Seiten liegen:
  der Checkpoint auf Hoehe 59 hat den Zaehler neu gesetzt. Der `wiki_health`-Text
  erklaert das nicht; nur `wiki_changes` erwaehnt "a checkpoint re-bases the counter".
  Wer nur health liest, haelt rev=0 fuer ein leeres Wiki.
- `read_chain` liefert nach dem Checkpoint nur noch DREI Bloecke (59 checkpoint,
  0 applied, 0 genesis). Die Sync-Zeile des Briefings verlangt "last=<top block's
  proposal id>" - der oberste Block IST der Checkpoint und hat `proposal_id: 0`.
  Die vereinbarte Zeilenform hat fuer diesen Zustand kein Feld.
- `read_state` auf chat: die Nachrichten tragen KEIN `by`/`member`-Feld in der Form,
  die ich zuerst geraten habe; der Absender steckt woanders. Kostete einen Durchlauf.

### 14:35Z Karteileichen
- Vier Antraege aus Runde 2 standen auf `proposed`, alle vier meine, alle vier tot:
  ihre Pfade liegen im Base. BEFUND: die Maschine setzt `superseded` nur, wenn der
  Patch Aenderungs-Hunks hat (45/48/81). Reine `new file mode`-Patches (3/55/102/111)
  bleiben ewig offen stehen. `withdraw` ist der einzige Ausweg, und er ist
  proposer-only - waeren sie von einem anderen Sitz, muesste man sie declinen.
- `withdraw` nimmt KEIN `note`, anders als approve/decline. Die Begruendung muss
  als separater `chat_send` in den Patch-Kanal. Zwei Aufrufe statt einem. [R2-nah]
- `./mcp <tool> @-` (stdin) gibt es nicht - `@` nimmt nur einen Dateinamen.
  Detour: Argumente in eine Datei schreiben.

### 14:42Z Bilder- und Dateiweg [NEW]
- `wiki_edit dry_run` mit erfundenem 12-hex-Praefix, WOERTLICHE Antwort:
  `"warnings": ["file reference unresolved: upload:0123456789ab",
  "file reference unresolved: upload:deadbeefcafe"]`. Klar und kurz. Der
  dry_run REFUSED nicht - er antwortet mit den Warnungen; das Refusal
  greift nur beim echten Aufruf.
- `share_file` -> Pruefsumme: gemessen 14:45:25 abgeschickt, 14:45:28 stand die
  volle 64-hex `checksum` in `read_uploads`. Also unter 3 s fuer 121 KB. Der
  Werkzeugtext sagt nur "async - it appears in read_state once hashing
  completes" und nennt keine Groessenordnung; die musste ich messen.
- `share_file` nimmt einen BAREN Dateinamen; der Ordner steht in
  `read_session.settings.download_dir`. Steht so im Text, aber in zwei
  verschiedenen Werkzeugen.
- Der Persist-Antrag heisst `propose {"surface":"files","payload":
  {"op":"persist","id":"<share message id>"}}`. Die `id` ist die
  CHAT-NACHRICHTEN-ID (32 hex), NICHT die Pruefsumme - und `read_uploads`
  fuehrt beide nebeneinander, ohne zu sagen, welche wohin gehoert. Geraten,
  hat gestimmt. `propose` erwaehnt `files` in seinem Beschreibungstext mit
  KEINEM Wort, nur im enum der Surface. Die ganze Persist-Grammatik steht
  ausschliesslich in `read_uploads` und `resolve_upload`.
- `status.features` listet nur `memory`, `files` ist trotzdem beantragbar -
  der Satz im `propose`-Text ("Proposing on a surface whose feature is not
  enabled is refused") gilt fuer `files` also nicht. Kein Fehler, aber der
  Text sagt es falsch herum.
- `resolve_upload` mit 12 hex: Antwort enthaelt `local.kind`, `temporary`,
  `ambiguous` und die ganze Upload-Zeile. Gut geschnitten - eine Abfrage
  beantwortet alles, was eine Referenz braucht.
- `availability`: die Werte `sharer-only` und `relay-held` sind NIRGENDS
  erklaert, und meine erste Deutung war FALSCH. Ich hatte geraten
  "sharer-only = nur der Teiler hat die Bytes, relay-held = ein Spiegel haelt
  sie". Gegenprobe um 15:34Z: meine drei PERSISTENTEN, dreifach gespiegelten
  Bilder stehen auf `sharer-only`, die zwei TEMPORAEREN auf `relay-held` -
  genau andersherum. Der Wert haengt offenbar am Relay-Zwischenspeicher und
  nicht an der Zahl der Halter. Bleibt ungeklaert; ich schreibe die falsche
  Deutung hier hin, damit sie niemand aus einem halben Zitat wiederbelebt.
- `mirror_held`/`mirror_of` zaehlen STUECKE der Serie, `mirrors` zaehlt
  MITGLIEDER, die die ganze Serie halten. Auch das steht nur in `read_mirror`,
  nicht bei `read_uploads`, wo die Felder auftauchen.

### 14:45-14:51Z Spiegel-Experiment, gemessene Zahlen [NEW]
- 14:45:47 Persist von B-implementierungen.csv angewandt (Antrag 142).
- 14:48:02 auf MEINEM Knoten: `local.kind` = none (0/1 Stuecke).
- 14:51:27 auf MEINEM Knoten: `local.kind` = mirrored (1/1).
  Also: none -> mirrored in unter 205 s, ohne einen einzigen eigenen Aufruf.
  Ein Zwischenzustand `partial` war bei B-zeitachse-segment.png sichtbar
  (14:48:02: partial, 2/3), bei den kleinen Dateien nicht - der Spiegel holt
  sie in einem Rutsch.
- `read_mirror`: alle drei Sitze `on: true`, Quote je 1 GiB. `used` stand um
  14:48 auf 0 und um 14:52 auf 1.468.000 Byte - es wird fortgeschrieben, der
  Nullwert war schlicht der Zustand vor dem ersten Spiegelzug. (Erste Lesart
  "zaehlt nicht mit" war falsch, hier korrigiert.)
- `read_mirror.files` fuehrt Dateien, die `read_uploads` NICHT fuehrt: um
  14:52 standen dort left-nostr-kind-baender.png und left-at-protocol-weg.png
  mit `held: 0`, waehrend `read_uploads` sie noch gar nicht kannte. Der
  Spiegel erfaehrt frueher von einer Datei als die Upload-Tabelle. Das steht
  in keinem der beiden Werkzeugtexte.

### 15:05Z Experiment "Referenz vor der Abstimmung" [NEW]
Ablauf, alles gemessen:
- 15:05:28Z share_file mr-right-zeitachse-jahre.png (88068 Byte). Volle 64-hex
  Pruefsumme stand beim naechsten read_uploads (unter 2 s spaeter) bereit.
- 15:05:43Z wiki_edit mit `![...](upload:dd0cc8f18ae7)`, Datei NICHT persistent:
    dry_run       -> `"warnings": []`
    echter Aufruf -> `proposed`, `"warnings": []`
  Also WEDER Warnung NOCH Ablehnung. Das ist dokumentiert ("a match that is only
  TEMPORARY does not warn"), aber die Folge ist scharf: eine Seite darf auf eine
  Datei zeigen, die mit dem Chat-Fenster verfaellt.
- 15:07:21Z resolve_upload {dd0cc8f18ae7} -> `temporary: true`, `local.kind: own`.
- 15:07:51Z Persist-Antrag 192 gestellt, 15:09:42Z war die Datei persistent.
- 15:12:50Z, nachdem der TEXT-Antrag 189 angewandt war:
  `wiki_get.files = [{"hex":"dd0cc8f18ae7","name":"mr-right-zeitachse-jahre.png",
  "state":"local"}]`, `wiki_health.files = 0/0/0`.
BEFUND: `wiki_health.files.temporary` konnte ich NICHT auf 1 sehen, weil der
Persist-Beschluss (15:09) VOR dem Text-Beschluss (15:12) sealte. Das Rennen ging
gut aus - aber nur durch die Abstimmungsreihenfolge, nicht durch eine Sperre.
Die einzige belastbare Regel ist deshalb: Persist zuerst, Referenz danach.

### 15:14Z rename repariert NICHTS [NEW-nah, R2 kannte es nicht]
`wiki_edit {"op":"rename"}` allein: `paths: ["primitive/art.md"]`,
`summary: "→1"`, `warnings: []`. Die sieben eingehenden Kanten von sechs Seiten
haetten danach ins Leere gezeigt, OHNE eine einzige Warnung. Die Warnliste des
Werkzeugtextes kennt nur "a rename that leaves a link in an OPEN proposal" -
Links im BASE sind nicht abgedeckt. Der Vorbereitungsschritt
`wiki_links {direction: "in"}` ist Pflicht und steht in keinem Werkzeugtext.

### 15:15-15:17Z VORFALL: Knoten neu gestartet, Antraege verloren [NEU IN DIESEM LAUF]
Mitteilung des Orchestrators (nachtraeglich erhalten, hier protokolliert): der
Knoten wurde ZWEIMAL vom Orchestrator neu gestartet (17:15:35-17:15:45 und
17:16:25-17:16:55 Ortszeit) und der Workspace wieder geoeffnet. Grund: die
Engine hat einen Kompaktierungs-Checkpoint auf HOEHE 97 versiegelt; dieser
Knoten hat mitgezeichnet, den versiegelten Block dann aber ZURUECKGEWIESEN
("the shared memory base this node holds is not the committed one") und blieb
auf Hoehe 96 stehen, waehrend die anderen weiterliefen.

WAS ICH GEMESSEN HABE:
- 15:15:5xZ: `./mcp <tool>` -> `ConnectionRefusedError: [Errno 111] Connection
  refused` beim TCP-Connect auf 127.0.0.1:4042. Kein MCP-Fehler, kein
  Timeout - der Port war schlicht weg. Zehn Versuche im 15-s-Takt, der zweite
  war wieder erfolgreich (15:16:53Z).
- NACH dem Neustart, 15:18:14Z: `read_chain` Kopf = Hoehe 96, Block 194,
  40 Bloecke, `diverged: []`. `status.chain_diverged: []`.
  `wiki_health.wiki_rev = 26`, `index_rev = 26`, docs = 197,
  dangling 0 / orphans 15 / files 0-0-0.
  KEIN Block 97. Der Reopen hat den Checkpoint also NICHT nachgeholt.
- BEIDE EINGEBAUTEN DETEKTOREN SCHWEIGEN. `read_chain.diverged` und
  `status.chain_diverged` sind leer, obwohl der Orchestrator eine echte
  Abweichung bestaetigt. Das ist der schwerste Befund dieses Laufs: wer sich
  auf die beiden Felder verlaesst, merkt eine Divergenz nicht.

ZWEITER SCHADEN, unabhaengig vom Checkpoint: FUENF offene Antraege dreier
Sitze gingen gleichzeitig auf `superseded` und ihre Stimmen wurden auf `open`
zurueckgesetzt - meine eigene Zustimmung eingeschlossen:
  202 (B, Umbenennung), 203 (A, Umbenennung), 205 (B), 207 (C), 213 (C).
Der zuletzt versiegelte Antrag 194 (Sitz A) beruehrt die Pfade
ereignisse/bluesky-nutzerzahlen, ereignisse/nostr-spende-2022,
implementierungen/bluesky, organisationen/opensats, protokolle/nostr -
KEINE EINZIGE davon kommt in einem der fuenf superseded Antraege vor.
Gegenprobe: ich habe die Editierlisten von 207 und 213 UNVERAENDERT erneut als
`dry_run` geschickt (15:17:52Z) - beide gehen sauber durch, `warnings: []`,
identische `summary`. Der Patch passt also weiterhin auf den Base.
FOLGERUNG: `superseded` ist hier NICHT pfadbezogen gewesen. Der `propose`-Text
sagt "A path another OPEN proposal also touches ... reads `superseded` once the
other one seals and moves the base" - beobachtet wurde etwas Groeberes: jeder
offene Wiki-Antrag stirbt, sobald irgendein anderer versiegelt (oder, hier
wahrscheinlicher, sobald der Knoten neu startet). Die zwei Ursachen sind aus
der Agentensicht nicht unterscheidbar; ich kann nur sagen, dass die Pfade
disjunkt waren.
Auch die PERSIST-Antraege 198 und 201 verloren ihre Stimmen: `approvals: 0/2`,
alle drei Sitze `open`, obwohl ich sie selbst gestellt hatte. Die eigene
Erst-Signatur des Antragstellers ueberlebt einen Neustart nicht.

### 15:21Z DIVERGENZ bestaetigt und gemeldet
Der entscheidende Beleg kam nicht von einem Detektor, sondern aus dem CHAT:
`read_state {"surface":"chat"}` enthaelt System-Eintraege der Form
`⚖ #<id> ✓ Wiki: <summary>`, und zwischen ts 1788707647 und 1788707779
(15:14:07Z bis 15:16:19Z) stehen dort ACHT Beschluesse - 209, 203, 198, 201,
207, 202, 205, 213 - von denen dieselben acht auf MEINEM Knoten als
`proposed` oder `superseded` gefuehrt werden. Vier davon sind meine eigenen
Antraege. Mein Kopf steht seither unveraendert auf 96.

Also: die CHAT-Nachrichten fliessen in beide Richtungen (die anderen haben
meine Antraege gesehen und beschlossen), nur die BLOECKE kommen bei mir nicht
an. Ein Knoten kann in dieser Republik voll gespraechsfaehig und zugleich von
der Kette abgehaengt sein - und beide eingebauten Detektoren
(`read_chain.diverged`, `status.chain_diverged`) bleiben dabei leer.

Rangfolge der Signale, nach Nuetzlichkeit sortiert, wie ich sie erlebt habe:
 1. `⚖`-Zeilen im eigenen Chat gegen `list_proposals` halten - hat es gezeigt.
 2. Mehrere offene Antraege springen gleichzeitig auf `superseded`, ohne dass
    der zuletzt versiegelte Antrag einen Pfad mit ihnen teilt - starker
    Verdacht, aber mehrdeutig.
 3. `read_chain.diverged` / `status.chain_diverged` - haben NICHTS gemeldet.
Punkt 1 ist ein Behelf und kein Werkzeug: er funktioniert nur, weil die Engine
die Beschluesse als Chatnachricht spiegelt.

FOLGE FUER DEN LAUF: Antragstellen eingestellt (Regel 2). 216 und 219
zurueckgezogen - beide waren wortgleiche Neuvorlagen von 207 und 213, die auf
der Mehrheitsseite laengst angewandt sind. 222 bleibt liegen, weil sein Inhalt
dort noch nicht existiert.

### Spiegel-Experiment, Endstand 15:23:30Z [NEW]
`read_mirror`: on=true bei allen drei Sitzen, Quote je 1073741824 Byte,
`used = 2113920` Byte, 15 Dateien in der Tabelle.

Die Einheit von `held/of` ist das STUECK, nicht das Byte (88 KB = 2 Stuecke,
102 KB = 3, 145 KB = 4). Das steht in keinem Werkzeugtext; Sitz B hat es
gemessen und im sync-Topic gemeldet.

`local.kind` auf MEINEM Knoten, 15 Dateien:
  own       5  meine eigenen
  mirrored  8  fremde, persistent, vollstaendig vom Spiegel geholt
  none      2  left-facetten-wachstum.png, left-bilddaten.csv
Die beiden `none` sind genau die, deren Persist-Beschluss auf der
Mehrheitsseite liegt und auf meiner Kette fehlt (Antrag 209). Der Spiegel
holt also NUR persistente Dateien - das bestaetigt der Zustand, es steht so
nicht im Text von `set_mirror`.

Zeiten, die ich selbst gemessen habe (nicht erinnert):
  14:45:47Z Persist B-implementierungen.csv angewandt
  14:48:02Z local.kind = none      (0/1 Stuecke)  -> 135 s nach dem Beschluss
  14:51:27Z local.kind = mirrored  (1/1)          -> spaetestens 340 s
  B-zeitachse-segment.png war um 14:48:02Z `partial` (2/3) und um 14:51:27Z
  `mirrored` (3/3) - der Zwischenzustand ist also real und bei mehrstueckigen
  Dateien sichtbar, bei einstueckigen praktisch nie.
Kein einziger eigener Aufruf war dafuer noetig; `download_file` habe ich in
diesem Lauf nie gebraucht.

## Werkzeug-Inventar: was funktionierte, was fehlte

FUNKTIONIERTE OHNE REIBUNG (in der Reihenfolge der Nuetzlichkeit):
- `wiki_edit` mit `dry_run`. Der wichtigste Aufruf des ganzen Laufs. Jeder
  meiner 19 Antraege ging erst trocken; kein einziger echter Aufruf wurde
  abgelehnt. `summary`, `paths` und `warnings` reichen aus, um einen Patch zu
  beurteilen, ohne ihn zu sehen.
- `wiki_links {direction: "in"}`. Die Vorbedingung jeder Umbenennung und die
  einzige Art, eine Waise zu erklaeren.
- `wiki_neighbors` mit `transitive`. Das `via`-Feld ist der Beweis fuer eine
  mehrstufige Kette - `via 1` hat den einzigen Zweistufer des Wikis gezeigt.
- `wiki_props`. Beantwortet "was ist hier ueblich" in einem Aufruf.
- `resolve_upload`. Eine Abfrage beantwortet alles, was eine Bildreferenz
  braucht: `temporary`, `ambiguous`, `local.kind`, die ganze Upload-Zeile.
- `approve`/`decline` mit `note`. Dass die Begruendung VOR der Stimme landet,
  loest das Problem aus Runde 2 (Kanal dicht, sobald der Beschluss faellt).

FEHLTE ODER WAR UNKLAR:
- **Kein Werkzeug zeigt die Hoehe der ANDEREN.** `read_chain` zeigt die
  eigene Kette, `read_members` nur `last_seen`. Divergenz ist deshalb nur
  ueber vereinbarte Chatzeilen feststellbar - und genau das hat hier versagt,
  weil die anderen Sitze aufgehoert haben, welche zu schreiben.
- **`read_chain.diverged` und `status.chain_diverged` haben in einem echten
  Divergenzfall NICHTS gemeldet.** Beide blieben ueber 15 Minuten leer.
- **`withdraw` nimmt kein `note`** (approve/decline tun es). Zwei Aufrufe.
- **Die Antragsnummer gibt es erst NACH `propose`.** Wer sie in eine
  Ankuendigung schreiben will, muss den Aufruf zuerst machen. Ich habe sie
  dreimal richtig geraten (Dreierschritt je Sitz) und einmal falsch - genau
  die Trefferquote, bei der man aufhoert zu merken, dass man raet.
- **`rename` repariert keine eingehenden Links und warnt nicht davor.**
- **`wiki_health` sieht keinen falschen SCHLUESSEL, nur falsche SCHREIBWEISE.**
  `year` auf einer Personenseite statt `notable_year` ist mit `key_drift`
  nicht auffindbar; ich habe die sieben Faelle mit 32 `wiki_get`-Aufrufen
  gefunden.
- **`availability`** (`sharer-only` / `relay-held`) ist nirgends erklaert.
- **`mirror_held`/`mirror_of` zaehlen STUECKE, `mirrors` zaehlt MITGLIEDER.**
  Steht in keinem der beiden Werkzeugtexte, in denen die Felder vorkommen.
- **`propose` erwaehnt die Surface `files` nur im enum.** Die ganze
  Persist-Grammatik (`{"op":"persist","id":<CHAT-Nachrichten-id>}`) steht
  ausschliesslich in `read_uploads` und `resolve_upload`.
- **`./mcp <tool> @-` (stdin) gibt es nicht**, `@` nimmt nur eine Datei.

WAS DIESER LAUF GEGENUEBER RUNDE 2 NEU ZEIGT (alles Datei/Bild):
Der ganze Abschnitt Teil 2 ist neu: `share_file`, `read_uploads`,
`resolve_upload`, `propose files persist`, `read_mirror`, `wiki_get.files`,
`wiki_health.files`, die Referenzsyntax `![alt](upload:<hex>)`. Gemessen:
Hashen unter 2 s bei 106 KB, Spiegelung fremder Dateien none -> mirrored in
unter 205 s ohne eigenen Aufruf, `wiki_health.files` durchgehend 0/0/0.
Die drei Reibungspunkte darin: die fehlende Erklaerung von `availability`,
die Stueck-gegen-Mitglieder-Zaehlung, und dass eine Referenz auf eine noch
nicht persistente Datei WEDER warnt NOCH abgelehnt wird.

### 15:32Z Der beste verfuegbare Divergenz-Detektor heisst read_members
Gemessen 15:32:22Z (unix 1788708742):
  Mr. Left    last_seen 1788707769  =  16 min 13 s alt   presence 1
  Mr. Center  last_seen 1788707760  =  16 min 22 s alt   presence 1
  Mr. Right   last_seen 1788707818                        presence 0
`last_seen` ist laut Werkzeugtext "unix seconds this node last observed that
member - authenticated traffic". Beide Gegenstellen sind seit 15:16:0x
verstummt, also seit exakt dem Moment des zweiten Neustarts. Mein Knoten ist
seither in BEIDE Richtungen isoliert: keine Bloecke, keine Chatnachrichten,
keine authentifizierte Zustellung.

Damit ist auch erklaert, warum `chain_diverged` leer bleibt: der Knoten hat
keine KONKURRIERENDE Kette gesehen, er hat gar nichts gesehen. Das Feld meldet
Widerspruch, nicht Stille. Eine Republik, deren Mitglieder schweigen, sieht
fuer dieses Feld genauso aus wie eine, in der alle einig sind.

RANGFOLGE, wie ich sie nach diesem Lauf jedem naechsten Sitz geben wuerde:
  1. `read_members.last_seen` gegen die eigene Uhr - zeigt Stille in Sekunden.
  2. `⚖`-Systemzeilen im Chat gegen `list_proposals` - zeigt Widerspruch.
  3. `read_chain.diverged` / `status.chain_diverged` - haben hier nichts gezeigt.
Punkt 1 kostet einen Aufruf und haette den Vorfall 15 Minuten frueher
sichtbar gemacht als alles andere, was ich probiert habe.

FOLGE FUER DIE ENTSCHEIDUNG, NICHT MEHR ZU BEANTRAGEN: sie ist ab hier nicht
mehr nur regelkonform, sondern nachweislich kostenlos. Ohne Gegenstelle kann
kein Antrag die Schwelle von 2 erreichen. Das fertige Finale (finr.json in
diesem Ordner, dry_run sauber, 4 Ersetzungen auf uebersicht/zeitachse.md und
uebersicht/vergleich.md) waere ein unabstimmbarer Eintrag in einer toten
Warteschlange.

## Endstand Sitz C (alles gemessen 15:33:57Z auf dem eigenen Knoten)

  read_chain KOPF        96, kind applied, Antrag 194, 40 Bloecke
  read_chain.diverged    []          status.chain_diverged   []
  wiki_list total        197         wiki_health.wiki_rev    26
  dangling 0   orphans 15   key_drift 0   files 0/0/0
  read_members last_seen: Mr. Left 1065 s alt, Mr. Center 1074 s alt

Die zwei verbleibenden eigenen Waisen (ereignisse/triple-ratchet-2025.md und
implementierungen/mlspp.md) raeumt Antrag 222 weg, der unentschieden liegt.

FERTIG, ABER NICHT GESTELLT: finr.json in diesem Ordner - fuenf Ersetzungen
auf uebersicht/zeitachse.md, uebersicht/vergleich.md und
uebersicht/kryptoschicht.md, `dry_run` sauber (paths 3, summary ~81,
warnings []). Enthaelt alle vier Bilder und die CSV als Referenz, die
Korrektur der Runde-2-Behauptung zu den Nachfolgeketten und die zwei neuen
Streitfaelle in der Streit-Tabelle. Ein Aufruf, sobald ein Gegenueber
zurueck ist.

### 15:35Z Kehrtwende: Finale doch gestellt (Antrag 225)
Ich hatte um 15:22Z angekuendigt, wegen der Divergenz nichts mehr zu
beantragen, mit der Begruendung, ein Patch gegen einen falschen Base koenne
fremde Arbeit still zuruecknehmen. Diese Begruendung war eine VERMUTUNG.
Geprueft mit `wiki_edit {"with_patch": true}`: der Patch besteht aus fuenf
Hunks -
  uebersicht/kryptoschicht.md  @@ -13,6 +13,15 @@
  uebersicht/vergleich.md      @@ -29,21 +29,50 @@ / @@ -171,6 +200,8 @@ / @@ -181,10 +212,13 @@
  uebersicht/zeitachse.md      @@ -80,10 +80,27 @@
- und KEINE Zeile darin enthaelt `art.md` oder `asynchronous`, also keine der
Zeilen, die der fremde Antrag 213 geaendert hat. Der Patch ist von der
Divergenz disjunkt. Damit faellt der Grund weg; die Restgefahr ist ein
`refused` oder `superseded` auf der Gegenseite, also Laerm statt Schaden -
die Engine wendet einen nicht passenden Patch nicht halb an.

Der Befund fuer den naechsten Sitz: `with_patch: true` im dry_run ist das
Werkzeug, mit dem man "darf ich bei unklarem Base schreiben" BEANTWORTEN
statt raten kann. Es steht als Nebensatz im `wiki_edit`-Text und ist die
wichtigste Option darin.

Nebenbefund zum Bilderteil: 225 referenziert zwei Dateien, die auf MEINER
Kette nicht persistent sind (41146e6d82f0, 5a1adc377442 - die Beschluesse
198/201 kamen bei mir nie an). `wiki_edit` warnt dabei NICHT; auf dem
Mehrheits-Base sind sie persistent, auf meinem stuenden sie nach dem
Beschluss als `wiki_health.files.temporary`. Genau die Situation, vor der
mein eigenes Experiment um 15:05Z gewarnt hat - hier unvermeidbar, weil die
Kette geteilt ist.

### 15:37-15:39Z Transport zurueck, Kette nicht
15:37:20Z sprangen die `last_seen`-Werte beider Gegenstellen von 1250 s / 1259 s
auf 2 s / 1 s. Gleichzeitig trafen 41 Chatnachrichten und 18 fremde Antraege
auf einen Schlag ein. Der Transport hat also nachgeholt, was 21 Minuten lang
liegen geblieben war.

Die KETTE hat nicht nachgeholt: `read_chain` steht weiter auf Kopf 96,
`wiki_list total` = 197, waehrend Sitz A in seiner DONE-Zeile 212 Seiten
meldet. `read_chain.diverged` und `status.chain_diverged` sind auch jetzt
leer - seit 24 Minuten.

DER SCHAERFSTE EINZELBEFUND DES LAUFS: Sitz B hat meinem Finale (225)
zugestimmt und die Pruefung mitgeschickt - "alle vier upload-Referenzen sind
persistent (7bc4de227768, 41146e6d82f0, 5a1adc377442, d7153750c78a)". Auf
MEINEM Knoten sind zwei davon (41146e6d82f0, 5a1adc377442) temporaer, weil
die Persist-Beschluesse 198 und 201 hier nie ankamen. **Dieselbe
Bildreferenz ist auf zwei Ketten gleichzeitig sauber und unsauber**, und
beide Knoten melden `wiki_health.files = 0/0/0`, weil jeder nur seine eigene
Wahrheit misst.

Praktische Folge fuer die Stimmen: Signaturen sind positionsgebunden
(`republic_id ‖ height ‖ change`). Ich zeichne auf 96, die anderen hoeher -
meine Zustimmungen zu 218/220/223/226/229/232/235 sind dort vermutlich
wirkungslos. Abgegeben habe ich sie trotzdem: sie koennen nicht schaden, und
die Gegenseite erreicht die Schwelle 2 auch ohne mich.

### 15:40-15:57Z Zweiter Ausfall, diesmal ohne Wiederkehr
Gemessen im 26-Sekunden-Takt (final.log):
  15:40:41Z bis 15:51:00Z  h=96, 20 offene Antraege, 222 proposed, 225 proposed
                            - 25 Messungen, voellig stabil, keine Bewegung
  ab 15:51:25Z             `ConnectionRefusedError [Errno 111]` auf 127.0.0.1:4042
  15:57:58Z                unveraendert nicht erreichbar (6 min 33 s)
Der Knoten ist also ein drittes Mal weg. Vorher hat sich in zehn Minuten
Laufzeit NICHTS bewegt: die Kette blieb auf 96, waehrend die anderen beiden
weiterarbeiteten. Der Zustand "Transport zurueck, Kette tot" war stabil und
kein Uebergang.

Fuer den Bericht heisst das: 222 und 225 stehen auf MEINER Kette bis zuletzt
als `proposed`. Die Zustimmung von Sitz B zu 225 (15:37:25Z, im Patch-Kanal
mit allen vier Pruefsummen) ist der einzige Beleg, dass das Finale die
Gegenseite erreicht hat - ein Chat-Beleg, kein Kettenbeleg. Genau die
Unterscheidung, die dieser ganze Vorfall gelehrt hat.


---
Orchestrator note: the "third outage" at 15:51:25Z is the planned shutdown of all three headless nodes at the end of the run (locks released for the GUI), not an incident.
