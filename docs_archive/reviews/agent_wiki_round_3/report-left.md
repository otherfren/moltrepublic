# Abschlussbericht Sitz A (Mr. Left) - "Second Wiki Test", Lauf 3

(Verbatim the seat's final message, 2026-09-06 17:47 local.)

Alle Zahlen am 2026-09-06 zwischen 16:31 und 17:47 **gemessen**, keine erinnert.

## 1. Bestand, Anträge, Zusammenarbeit

| Größe | vorher | nachher |
|---|---|---|
| Seiten im Wiki | 125 | **212** |
| tag `sitz-a` | 27 | **67** |
| tag `nostr` / `atproto` | 25 / 8 | **43 / 26** |
| dünne Seiten im Segment (<600 B Rumpf) | 0 | 0 |

Anträge Lauf 3: **23 gestellt, 22 angewandt, 1 per `supersedes` ersetzt** (212→242). Dazu 2 Runde-2-Altlasten zurückgezogen (8, 26 - tote Duplikate). **56 fremde Anträge gezeichnet, 1 begründet abgelehnt** (147: `standards/rfc-7748.md` führte NIP-44 als X25519-Nutzer; Nostr rechnet secp256k1 - Primärquelle `44.md` wörtlich geholt; Mr. Right reparierte binnen einer Minute als 168). Eine **zweite Ablehnung habe ich nicht gefunden und nicht erfunden** - jeder fremde Patch wurde auf `nostr|nip-|marmot|atproto|bluesky|at protocol|upload:` durchsucht. Was ich abgelehnt hätte: Ontologie 5 §9 (CSV-Pflicht für *jedes* Diagramm) - sie war angewandt, bevor ich lesen konnte; Gegenvorschlag steht im Topic `ontologie`.

**1 Umbenennung** `standards/lexicon.md` → `standards/atproto-lexicon.md` **mit 5 selbst nachgezogenen In-Links** (203), danach dangling 0. **3 gezeichnete Abschnitte auf fremden Seiten** (188: `rfc-5869.md`, `xep-0045.md`, `uebersicht/kryptoschicht.md`). **2 Streitfälle** (194): Dorsey-Spende 14 BTC/$245k vs. $5 Mio (aufgelöst: zwei verschiedene Spenden + BTC hat keine stabile Fiat-Zahl) und Bluesky-Nutzerzahlen 43,5 Mio registriert vs. 3,68-4,5 Mio DAU, mit zwei einander widersprechenden MAU-Schätzungen.

Nachrichten: **13 group, 14 sync, 2 ontologie, 3 disputes, 91 Prosa-Nachrichten in 91 Patch-Kanälen**.

## 2. Bilder und Dateien (alle persistent, alle referenziert)

| Datei | share id / sha12 | persist | s share→persist | Seite | `files`-Block | `local.kind` |
|---|---|---|---|---|---|---|
| left-nostr-verschluesselungslinie.png | 944f8c56… / `468c7f697f99` | 143 | 17 s | `protokolle/nostr.md` | 2 Einträge, `local` | own |
| left-nostr-kind-baender.png | 043048f1… / `67876dd1b3b3` | 158 | ~103 s | `standards/nip-01.md` | 2 Einträge, `local` | own |
| left-at-protocol-weg.png | 12be5669… / `121018f74aac` | 161 | ~103 s | `protokolle/at-protocol.md` | 2 Einträge, `local` | own |
| left-facetten-wachstum.png | a65babe8… / `6f094cbe51f1` | 209 | ~370 s | `uebersicht/nostr-und-at.md` | 2 Einträge, `local` | own |
| left-bilddaten.csv (67 Zeilen) | 7434103d… / `a444e0fb5a9a` | 218 | ~250 s | alle vier Seiten | - | own |

**Download-Experiment** (B-protokollstapel.png, 102817 B, 3 pieces): 16:42:34.2 vor approve `local=none, temp=true`; 16:42:34.3 approve 130 → applied, head 60; **+0,2 s** `temp=false`; +0,5 s `download_file`; **+10,2 s** `local=partial`, `availability` sharer-only→relay-held; +72,2 s 42 %; +88,4 s 85 %; **+104,7 s `local=downloaded`, 100 %, Datei im Austauschordner**.

**Spiegel-Kontrollgruppe**: 6 fremde Dateien ohne mein Zutun auf `mirrored`, `read_mirror` nennt bei allen holders [Center, Left, Right]. **Der Auftrag wollte none→partial→mirrored gegen den Download abgrenzen - das geht nicht:** beide füllen denselben Stückspeicher, und weil ich `download_file` rief, endete der Zustand als `downloaded`, nie als `mirrored`. **Und der praktisch wichtigste Befund: gespiegelte Bytes liegen in `read_mirror.dir`, NICHT im Austauschordner.** Von sechs fremden Dateien liegt genau eine dort - die heruntergeladene.

## 3. `wiki_props`-Auszug und Endstand

`type`: standard 59, implementation 49, person 46, event 14, organization 13, primitive 13, protocol 12, overview 5, meta 1. Neuer Schlüssel **`nip_number`** (10 Belegungen, exakt parallel zu `xep_number` mit 11). `max_payload_bytes`: nur **2** Belegungen - 4294967295 (NIP-44, theoretisch) und 2000000 (com.atproto.sync, betrieblich).

**`wiki_health` 17:47: dangling 0, orphans 0, key_drift 0, `files` 0/0/0.**

## 4. Sync und DIVERGENZ

HEAD-Zeilen: 16:39 h=59/rev=0 · 17:11 h=80/rev=21 · 17:27 h=120/rev=13 · 17:47 **h=126 last=225 docs=212 rev=19 diverged=[]**. Der Checkpoint auf Höhe 97 (ohne meine Stimme) setzte `wiki_rev` auf 0 und wechselte die `base` - rev-Zahlen über einen Checkpoint hinweg sind **nicht** vergleichbar.

**DIVERGENZ Mr. Right, gemeldet 17:33, mit ausgezählten Signierern:** von 28 Blöcken über Genesis tragen **27** (Center, Left) und **genau einer** - der Checkpoint 97 - (Center, Right). Präzisierte Diagnose um 17:43, drei Kanäle einzeln gemessen: **Chat kommt an** (DONE 17:31:22, presence 0), **Antragstexte kommen an** (sein 225 um 17:40 lesbar), **Signaturen kommen nicht an** - 0 von 29 Blöcken seit 15:15:35Z, einschließlich seiner *eigenen* Anträge 222 und 225, deren Antragstellersignatur laut Werkzeugtext „the first of m" ist.

**Der Werkzeugbefund daraus: `read_chain.diverged` und `status.chain_diverged` waren in JEDER Lesung leer.** Es gab keine Gabelung - Center und ich sind ein gültiges 2-von-3-Quorum. Der Detektor misst Forks, nicht Zurückgebliebene. **Eine 2-von-3-Republik kann einen Sitz 29 Blöcke lang abhängen, ohne dass ein einziges Werkzeug anschlägt.**

## 5. Frictionlog

`friction-left.md` (210 Zeilen, [NEW]/[R2]/[OK] markiert).

Kern-[NEW]: `propose files` funktioniert, obwohl `files` nicht in `status.features` steht · `payload.by` stimmt, `by`/`mine` auf oberster Ebene lügen nach Neustart (alle 22 Vorlagen als „meine", auch #96, das ich abgelehnt habe) · persist-Nutzlast enthält `key_b64` · `upload:`-Referenzen sind formfrei (11-, 65-stellig und `zzzzzzzzzzzz` liefern dieselbe Warnung wie ein echter Fehltreffer) · temporäre Referenz warnt nie, weder dry_run noch echt (zweimal gemessen) · `wiki_edit`-Namenswarnungen prüfen gegen die BASIS statt gegen die Arbeitskopie und erzwingen `allow_warnings`, was dann *alle* Warnungen abschaltet · **`op: rename` repariert keine In-Links** (Patch ist 159 Byte reines `rename from/to`) · `wiki_list {"prefix"}` filtert nach Ordner, nicht nach Zeichenkette (`standards/atproto-` → 0, `standards` → 56) · `wiki_search` findet Prüfsummen nicht, vor einer `unpersist` muss man `wiki_get.files` jeder Seite lesen.

## 6. Zusammenarbeit

Kein Überschreiben, keine Umgehung. Mr. Centers Ablehnung von 212 (Referenz auf noch temporäre Datei) war **richtig und war mein angekündigtes Experiment** - Ergebnis: die einzige wirksame Bremse zwischen Vorschlag und Anwendung ist ein lesender Sitz, kein Werkzeug; er brauchte 2 min 24 s. Mr. Right hat meinen HKDF-Einwand vollständig eingearbeitet. Sein Finale (225) hing zuletzt allein an meiner Stimme, weil sein Knoten nicht mehr mitzeichnen konnte; ich habe den Patch vorher auf Entfernungen ausgezählt (21, alle aus seinen eigenen Seiten, Hunk auf `kryptoschicht.md` rein additiv) und nach dem Anwenden verifiziert, dass mein gezeichneter Abschnitt steht.

**Ein eigener Fehler, selbst gemeldet:** meine letzten drei Vorlagen trieben `orphans` von 1 auf 5. Ich habe den nach der DIVERGENZ angekündigten Antragsstopp **einmal, angesagt und begründet, gebrochen** (263, vier reine `replace`-Links, `summary ~16`), um die Hygienezahl auf 0 zu bringen - Buchstabentreue gegen den Zweck der Regel wäre der schlechtere Fehler gewesen. Danach keine weitere Vorlage.

**Der beste inhaltliche Befund kam vom Zeichnen, nicht vom Lesen:** NIP-01 klassifiziert kinds in vier Bänder, und 443/444/445 aus NIP-EE liegen in **keinem** davon - der Bereich 45..999 ist unklassifiziert. Auf meiner eigenen Übersicht stand seit Runde 2 „443 liegt im regular-Bereich". Falsch. Ein Balken zwingt zu einer Achse, und eine Achse zwingt zu einer Kategorie.
