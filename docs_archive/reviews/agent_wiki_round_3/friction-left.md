# friction.md - Mr. Left (seat A, port 4040), run 3, 2026-09-06

Legend: [NEW] = first reported in run 3 (files/pictures feature), [R2] = already
reported in round 2, [OK] = worked as documented.

## Log

- 16:31 `./mcp --list` returns 63.3 KB in ONE blob. [R2] There is no way to ask
  for a single tool's text; the wrapper has no `--list <tool>`. Cost: the whole
  text has to be paged through a file.
- 16:34 [NEW, schwerwiegend] Nach dem Neustart des Knotens ist die
  PROPOSAL-ZUSCHREIBUNG kaputt: `list_proposals` und `read_proposal` melden
  fuer ALLE 22 Vorlagen `by: "Mr. Left"` und `mine: true` - auch fuer #96,
  das nachweislich Mr. Right eingebracht hat (Patch-Kanal 96, erste
  Nachricht von Mr. Right; ich selbst habe dort DECLINE geschrieben).
  Gegenprobe: Mr. Right meldet im Gruppenchat "vier Antraege ... alle vier
  meine", mein Knoten sieht nur zwei offene und haelt beide fuer meine.
  Ebenfalls verloren: `approvals` ist ueberall 0 und `votes` ueberall
  "open", obwohl die Systemzeilen in den Patch-Kanaelen "declined by
  Mr. Right" / "declined by Mr. Center" tragen. Wer die Urheberschaft
  braucht, muss den Patch-Kanal lesen - das Feld luegt.

## Teil 2 - Dateien und Bilder (alles [NEW] in dieser Runde)

- 16:41:38 `share_file {"name":"left-nostr-verschluesselungslinie.png"}` ->
  `{"reply":"ack"}` [OK]. Die Zeile stand mit 64-hex `checksum` um 16:41:47
  in `read_uploads`, also **9 Sekunden** fuer 87814 Byte. Der Werkzeugtext
  sagt "async - poll read_uploads", nennt aber keine Groessenordnung; 9 s
  ist der gemessene Wert.
- [NEW] `propose {"surface":"files",...}` funktioniert, OBWOHL
  `status.features` nur `["memory"]` fuehrt. Der Werkzeugtext von `propose`
  sagt "Proposing on a surface whose feature is not enabled is refused
  (status.features lists the enabled set)" - `files` ist also kein
  Charter-Feature, sondern immer da. Das steht nirgends und ich habe den
  Aufruf in der Erwartung einer Absage abgeschickt.
- [NEW] Die Nutzlast einer persist-Vorlage traegt `by` KORREKT
  (`payload.by = "Mr. Center"`), waehrend das Feld `by` derselben Vorlage
  auf oberster Ebene falsch "Mr. Left" sagt. Wer die Urheberschaft einer
  Datei-Vorlage braucht, liest `payload.by`.
- [NEW] Die persist-Nutzlast enthaelt `key_b64`, `root` und `pieces`. Der
  Schluessel der Datei steht damit in der Abstimmung - innerhalb der
  Republik unbedenklich, aber es steht in keinem Werkzeugtext, dass ein
  Zustimmen zu einer persist-Vorlage bedeutet, den Dateischluessel zu
  sehen.
- [NEW] EXPERIMENT "download vor dem Spiegel", vollstaendig gemessen
  (B-protokollstapel.png, Mr. Center, 102817 Byte, 3 pieces):
    16:42:34.2  vor approve      local=none  temp=true  avail=sharer-only mirrors=1 held=0/3
    16:42:34.3  approve 130      -> applied, approvals 2/2, head=60, sofort
    16:42:34.5  nach approve     local=none  temp=FALSE avail=sharer-only  (temp kippt in <0.2 s)
    16:42:34.5  download_file    -> {"reply":"ack"}
    +10.2 s     local=partial, avail=relay-held, download.percent=0 phase=requested
    +72.2 s     held 1/3, percent 42, phase=transferring
    +88.4 s     held 2/3, percent 85
    +104.7 s    local=DOWNLOADED, mirrors=3, held 3/3, percent 100, phase=done,
                path=<exchange-left>/B-protokollstapel.png
  Drei Befunde daraus:
  1. `download.percent` bleibt 70 Sekunden lang auf 0 und `phase` auf
     "requested", WAEHREND `local.kind` schon `partial` meldet und
     `availability` von `sharer-only` auf `relay-held` gesprungen ist. Die
     Fortschrittsanzeige des Downloads hinkt dem tatsaechlichen
     Stueckerwerb um mehr als eine Minute hinterher.
  2. Der Auftrag wollte "none -> partial -> mirrored" gegen den Download
     abgrenzen. Das geht NICHT: beide fuellen denselben Stueckspeicher,
     und weil ich explizit `download_file` gerufen habe, endet der Zustand
     als `downloaded`, nie als `mirrored`. Die Frage "war es der Spiegel
     oder mein Download" ist mit diesen Werkzeugen nicht beantwortbar.
  3. `mirrors` springt erst am Ende von 1 auf 3 - der Zaehler zaehlt nur
     Mitglieder mit der VOLLSTAENDIGEN Serie, Teilbestand erscheint darin
     gar nicht. `availability` kennt mindestens die Woerter `sharer-only`
     und `relay-held`; was sie genau bedeuten, sagt kein Werkzeugtext.

## Werkzeugbefunde Teil 1 (Wiki)

- [NEW] `wiki_edit` prueft die Namenswarnung gegen die BASIS, nicht gegen
  die Arbeitskopie. Vorlage 176 entfernt in Edit 1 per `set_props` die
  Aliase "Merkle Search Tree" und "MST" von
  `standards/atproto-repository.md` und legt in Edit 3 die Seite
  `primitive/merkle-search-tree.md` mit genau diesen Namen an. Der
  Werkzeugtext sagt "edits apply IN ORDER to a working copy of the current
  base" - die Warnung
  `title "Merkle Search Tree" already names standards/atproto-repository.md
  - both pages lose it` erscheint trotzdem, dreimal, und zwingt zu
  `allow_warnings: true`. Damit ist eine echte Warnung von einer veralteten
  im selben Aufruf nicht mehr unterscheidbar: `allow_warnings` schaltet
  ALLE ab, es gibt kein "diese eine kenne ich".
- [NEW] Referenzen `upload:<hex>` sind formfrei. Alle vier Faelle - 11
  Stellen, 12 Stellen, 65 Stellen und `zzzzzzzzzzzz` (kein Hex) - liefern
  dieselbe Warnung `file reference unresolved: upload:<was auch immer>`.
  Der dokumentierte Bereich ist "12..=64 hex digits", aber ein Tippfehler
  in der Laenge oder im Alphabet ist von einer gueltigen, nur unbekannten
  Pruefsumme nicht zu unterscheiden.
- [NEW] Eine Referenz auf eine noch TEMPORAERE Datei warnt nicht, weder im
  `dry_run` noch im echten Aufruf (gemessen 16:51:34, Vorlage 164 auf
  `left-at-protocol-weg.png`, `resolve_upload` sagte im selben Moment
  `temporary: true`). Das ist so dokumentiert und es ist die richtige
  Entscheidung - aber es heisst, dass die einzige Warnung vor einer Seite,
  die nach dem Chat-Fenster ins Leere zeigt, `wiki_health.files.temporary`
  ist, und die liest niemand, der gerade schreibt.
- [NEW] EXPERIMENT "Spiegel ohne Download" (Kontrollgruppe, 6 fremde
  Dateien), Stand 16:58:20:
    B-ap-zustellweg.png            local=mirrored   held 2/2
    B-zeitachse-segment.png        local=mirrored   held 3/3
    B-implementierungen-balken.png local=mirrored   held 2/2
    B-implementierungen.csv        local=mirrored   held 1/1
    mr-right-abloeseketten.png     local=mirrored   held 3/3
    mr-right-kryptoschicht-stapel  local=mirrored   held 3/3
    B-protokollstapel.png          local=DOWNLOADED held 3/3   (der eine, den ich gezogen habe)
  DER UNTERSCHIED, den kein Werkzeugtext nennt und der praktisch der
  wichtigste ist: gespiegelte Bytes liegen in
  `~/.moltrepublic/mirror/<workspace-id>/` (read_mirror.dir), NICHT im
  Austauschordner. Im Austauschordner liegt von sechs fremden Dateien
  genau EINE - die heruntergeladene. Ein Agent, der eine fremde Datei
  ANSEHEN will, muss `download_file` rufen, auch wenn `local.kind` schon
  `mirrored` sagt; "der Knoten hat die Bytes" und "ich komme an die Bytes"
  sind zwei verschiedene Aussagen.
- [NEW] `wiki_edit` mit `op: rename` repariert IN-LINKS NICHT. Gemessen
  2026-09-06: ein reiner Rename von `standards/lexicon.md` nach
  `standards/atproto-lexicon.md` erzeugt mit `with_patch: true` genau
  diesen Patch -
    `similarity index 100% / rename from ... / rename to ...`
  - und `paths` nennt nur die eine Datei. Die fuenf Seiten, die den Pfad
  ausgeschrieben tragen, muss der Aufrufer selbst nachziehen, sonst haengen
  fuenf Kanten. `wiki_links {"direction":"in"}` ist der einzige Weg, sie zu
  finden.
- [NEW] `wiki_list {"prefix": ...}` filtert nach ORDNER, nicht nach
  Zeichenkette. Gemessen: `standards` -> 56, `standards/` -> 56,
  `standards/atproto-` -> **0**. Der Werkzeugtext sagt "only paths under
  this folder" und ist damit korrekt - der Parametername `prefix` legt
  trotzdem das Gegenteil nahe, und ich habe eine Umbenennung mit einer
  Begruendung geplant, die daran gescheitert ist.

## Governance und Kette

- [NEW] Eine 2-von-3-Republik kann einen Sitz ABHAENGEN, ohne dass
  irgendein Knoten Divergenz meldet. Gemessen 17:33 auf meinem Knoten:
  von 28 Bloecken ueber dem Genesis tragen **27** die Signierer
  (Mr. Center, Mr. Left) und **einer** (Hoehe 97, der Checkpoint) die
  Signierer (Mr. Center, Mr. Right). Der hoechste Block mit Mr. Rights
  Unterschrift ist 97; er selbst meldet seine Kette auf 96 stehend, also
  hat er den Schnitt SIGNIERT und nicht ANGEWANDT. Waehrenddessen:
  `read_chain.diverged` = [] und `status.chain_diverged` = [] in JEDER
  Lesung dieser Runde. Das ist kein Fehler des Detektors - es gibt keine
  Gabelung, Center und ich sind ein gueltiges Quorum. Aber wer `diverged`
  als "alles in Ordnung" liest, sieht diesen Fall nie. Es fehlt ein
  Signal der Art "Mitglied X hat seit N Bloecken nicht mitgezeichnet".
- [NEW] Gegenprobe, die die naheliegende Erklaerung ausschliesst: es sind
  nicht Bloecke, die versiegelt wurden, bevor seine Stimme ankam.
  `read_proposal` auf 209, 198, 201, 222 und 260 zeigt bei ALLEN
  `Mr. Right: open` - auch bei **222, seinem EIGENEN Antrag**, dessen
  Antragstellersignatur laut Werkzeugtext die erste von m ist. Seine
  Governance-Nachrichten erreichen meinen Knoten seit 15:15:35Z nicht,
  waehrend sein CHAT ankommt (DONE-Meldung 17:31:22, `read_members`
  presence 0 um 17:33:02). Chat und Governance reiten auf demselben
  Broadcast-Strom, und nur eines von beiden kommt an.
- [R2, bestaetigt] `approvals` zaehlt nur die Signaturen DIESES Knotens.
  Nach `approve` steht dort oft 1 und der Block versiegelt Sekunden
  spaeter mit 2 - das ist kein Fehler, aber es macht die Zahl als
  Fortschrittsanzeige unbrauchbar.
- [NEW] Der CHECKPOINT setzt `wiki_rev` auf 0 zurueck und wechselt die
  `base` von `wiki_changes` (`truncated: true`). Zwei HEAD-Zeilen ueber
  einen Checkpoint hinweg sind damit NICHT vergleichbar - nach der
  Sync-Regel des Auftrags sieht das aus wie eine Divergenz und ist keine.
  Der Checkpoint auf Hoehe 97 kam ohne meine Stimme zustande; ich wurde
  nicht gefragt und habe ihn erst an der zurueckgesetzten rev bemerkt.

## Was gut funktioniert hat [OK]

- `wiki_edit` mit `dry_run` vor jedem Stapel: 16 Vorlagen, keine einzige
  wegen eines Formfehlers zurueckgewiesen. Die Warnung "alias X already
  names Y - both pages lose it" hat zweimal einen echten Namenskonflikt
  gefunden, bevor er im Wiki stand.
- `supersedes` in `wiki_edit`: 212 -> 242 in einem Aufruf, ohne dass eine
  abgelehnte Fassung offen liegen blieb.
- `approve` mit `note`: der Grund landet VOR der Stimme im Patch-Kanal.
  Bei 2 von 3 ist das Fenster zwischen erster Stimme und Entscheidung eine
  einzige fremde Handlung breit - ohne diesen Parameter waeren mir in
  dieser Runde mindestens fuenf Begruendungen in einen bereits
  geschlossenen Kanal gefallen.
- `share_file` -> `read_uploads` -> `propose files persist` ->
  `resolve_upload`: die Kette hat fuenfmal ohne Ueberraschung
  funktioniert, Hashdauer 9 bis 14 Sekunden fuer 6 KB bis 145 KB.
- `wiki_links {"direction":"in"}` ist das einzige Werkzeug, mit dem sich
  eine Umbenennung ueberhaupt sauber nachziehen laesst.

## Was gefehlt hat

- Ein Weg, die Urheberschaft einer Vorlage zuverlaessig zu lesen. `by` und
  `mine` sind nach einem Neustart falsch, `payload.by` gibt es nur auf der
  Flaeche `files`, und fuer Wiki-Vorlagen bleibt nur: den Patch-Kanal
  lesen und schauen, wer die erste Nachricht geschrieben hat.
- Eine Suche nach einer Pruefsumme. `wiki_search "b8ecea4ac949"` findet
  nichts, obwohl die Zeichenkette als `upload:`-Referenz im Text stehen
  koennte. Vor einer `unpersist` muss man `wiki_get.files` auf JEDER Seite
  lesen, die ueberhaupt eine Datei traegt - es gibt keine Rueckwaertsfrage
  "welche Seiten nennen diese Datei".
- Ein Signal fuer einen zurueckgebliebenen Sitz (siehe oben).
- [NEW, praeziser] Die Diagnose der Divergenz laesst sich auf DREI Kanaele
  aufteilen, alle am 2026-09-06 gemessen:
    Chat            kommt an   (DONE-Meldung Mr. Right 17:31:22,
                                read_members presence 0 um 17:33:02)
    Antragstexte    kommen an  (sein Antrag 225 um 17:40 bei mir lesbar,
                                mit Patch, auszaehlbar, zeichenbar)
    Signaturen      kommen NICHT an (0 von 29 Bloecken seit 15:15:35Z,
                                einschliesslich seiner EIGENEN Antraege
                                222 und 225, deren Antragstellersignatur
                                laut Werkzeugtext "the first of m" ist)
  Nur EINER der drei Wege ist ausgefallen, und es ist der, den kein
  Werkzeug beobachtet. `read_members` meldet ihn als online, `read_chain`
  meldet `diverged: []`, `list_proposals` zeigt seine Antraege - alles
  gruen, und trotzdem hat er seit 29 Bloecken nicht mitgezeichnet.

## Endstand 17:47

- Kette: h=126, last=225, docs=212, rev=19, diverged=[],
  status.chain_diverged=[]
- wiki_health: dangling 0, orphans 0, key_drift 0,
  files {dangling 0, temporary 0, ambiguous 0}
- Austauschordner: 5 eigene Dateien (4 PNG, 1 CSV) plus
  B-protokollstapel.png, die einzige FREMDE Datei auf der Platte - und die
  einzige, die ich per download_file geholt habe. Die anderen sechs
  fremden liegen gespiegelt in ~/.moltrepublic/mirror/<workspace-id>/,
  nicht hier.
- Alle offenen Antraege sind entschieden, vier Messungen im Abstand von
  30 s: open 0.
