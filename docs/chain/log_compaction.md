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
2. ✅ **Keystone-Beweis (F9) + molt-storage-Primitive** (2026-07-25,
   `c3fe291` + `141eb6d`).
3. ✅ **Engine-Kompaktor** (Ticker + Clean-Close, F8) inkl. Floor/Cursor-
   Umleitung (C2/F4) — `a386d87`.
4. ✅ **Share-Vergessen** (`shared_files` fällt beim Stutzen, auch im
   persistenten Sidecar).
5. ✅ dieser Abschnitt.

### Was gebaut wurde (Ist-Stand 2026-07-25)

**Der F9-Beweis kam zuerst und hat zwei echte Fallen gefunden**, die sonst
still ausgeliefert worden wären:

- **Per-Sender-Ordinals verschieben sich.** Die Id einer Legacy-Nachricht ist
  `sha256(TAG ‖ le64(sender_ordinal) ‖ from ‖ ts ‖ body)`; das Ordinal wird
  über den Log gezählt. Wer alte Nachrichten wegwirft, zählt neu — die nächste
  Legacy-Nachricht desselben Senders bekäme hier eine ANDERE Id als bei den
  Peers, und ein id-adressiertes Protokoll divergiert. Gelöst durch
  `EngineStateDump::chat_pruned_counts`: die gedroppte Anzahl pro Sender wird
  mitgeführt und zum Ordinal addiert (eine Summe, keine Position — deckt auch
  Out-of-Order-Ankünfte).
- **Legacy-POSITIONS-Referenzen adressieren falsch.** Ein id-loses
  Reaction/Delete/File-Removed (index-adressiert) und das numerische `quote`
  würden nach einem Prune auf die Nachricht zeigen, die nachgerückt ist —
  also eine unschuldige ÜBERLEBENDE Nachricht bereaktion/löschen. Gelöst durch
  das klebrige `chat_pruned`-Flag (im Dump persistiert): danach verweigert
  `chat_target` den Index-Fallback und das Quote bleibt unaufgelöst.
  Id-adressierte Ops sind unberührt.

**Ablauf einer Runde** (`compaction.rs` + `compact_once` im Storage-Writer):
Inhalts-Cutoff = `now − (1 + F2-Karenz) × Fenster`; Live-State und Snapshot
werden mit DEMSELBEN Aufruf gestutzt (sie können nicht auseinanderlaufen);
dann Snapshot schreiben + fsync (R1), Floor = Snapshot-Position, gebremst vom
Delivery-Cursor jedes Peers INNERHALB seiner Karenz (R2/F4), dann F6-Migration,
dann Drop (Keys zuerst löschen, dann unlinken). Erster echter Drop hebt die
Manifest-Version — ältere Binaries verweigern das Workspace höflich, statt es
als beschädigt zu lesen.

**Abweichungen von der Vorgabe (bewusst, klein):**
- Der Tages-Trigger reitet auf dem Presence-Tick statt auf einem eigenen
  internen `Command` — dieselbe Lösung wie die Track-C-Rotation, spart eine
  Command-Variante und damit Co-Equality-Churn. Der Clean-Close-Trigger läuft
  SYNCHRON, sonst verliert er das Rennen gegen den `Close` des Writers.
- Der Floor liegt in der Key-Tabelle, nicht im Manifest: er gehört zu genau
  den Daten, die die Tabelle beschreibt, wird mit ihr atomar geschrieben und
  leakt nicht im Klartext-Manifest. Das Manifest trägt nur den
  Versions-Stop.

**Dabei gefundene Lücke (gefixt):** `log/keys.state` fehlte in der
Export-Include-Tabelle und der Import-Allowlist — ein Blob eines kompaktierten
Workspaces hätte Segmente ohne ihre Schlüssel transportiert, also ein
unentschlüsselbares Log auf genau dem Pfad, der zur Wiederherstellung da ist.

**Offen / bewusst nicht gebaut:** F7 bleibt wie entschieden (alte S3-Kopien
altern über `s3_keep_copies` aus, kein aktives Durchgreifen) — die Karenz ist
in §A.3 als Restrisiko benannt und gehört noch in die Settings-Hilfe.

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

## B.3 Kanonische Zustands-Serialisierung (`molt-chain-checkpoint-v2`)

Deterministische Byte-Folge, längenpräfixierte Felder (dasselbe Framing
wie `roster_canonical_bytes` — Geschwister-Layout, eigener
Versions-Tag). Inhalt in fester Reihenfolge:

1. Tag `molt-chain-checkpoint-v2\0` (v2 seit N1: beide Identity-Tabellen
   tragen zusätzlich den `nostr_pk`-Transport-Anker jedes Mitglieds — unter
   v1 wäre der Roster-Anker eines servierten Blobs ohne Hash-Änderung
   austauschbar gewesen).
2. `republic_id`.
3. **Founding-Tabelle**: Name, `rule_m`, `rule_n`, GENESIS-Identities in
   Gründungsreihenfolge, Agenda — damit jeder Verifier `republic_id`
   aus dem Inhalt REKOMPUTIEREN kann (Fälschungsschutz wie beim
   Genesis-Check, §B.5).
4. **Aktueller Roster** nach allen Membership-Blöcken ≤ `upto`:
   `(member, identity_pk, nostr_pk)` in Chain-Reihenfolge
   (deterministisch, weil die Blockfolge total geordnet ist).
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

1. **Propose:** menschliches Verb `propose_checkpoint` (parameterlos —
   `upto` ist IMMER die aktuelle Head-Höhe, B-F1). Etappe 3 liefert das
   MCP-Tool; die GUI-Affordance + Sichtbarkeit (pending-Anzeige,
   Seal-/Stale-Ereignis) ist Etappe 5. Der Proposer
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
     die Id, exakt der Genesis-Schutz. Dazu die Struktur-Checks der
     Genesis-Regel: `rule_m ∈ [1, rule_n]` und
     `founding_identities.len() == rule_n` (seit N1 — eine größere
     Tabelle würde Angreifer-Keys in die Signer-Menge schmuggeln).
  3. **Kein zirkuläres Vertrauen** (Review-Finding 2026-07-18): Der
     Blob-Roster ist nur durch den Hash gebunden, den die Anker-Signaturen
     selbst attestieren — deshalb verifizieren die Anker-Signaturen gegen
     die **Founding-Identities** (rid-gebunden), und jeder Roster-Eintrag
     muss wörtlich in der Founding-Tabelle stehen — alle DREI Anker,
     `nostr_pk` eingeschlossen (Sitze sind ab Gründung fix; Restored
     behält den verankerten Key). Fälschung erfordert damit
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

- `serve_chain_from(from)`: **entschieden + gebaut (4b):** eigenes
  additives Event `WorkspaceEvent::CheckpointServed { blob }`, serviert
  VOR den Committed-Re-Serves, nur wenn der Requester echt unter dem
  Anker liegt (`from < anchor`). Empfänger: first-stash-wins +
  Invalidierung; Adopt nur nach voller Suffix-Verifikation. Bekannte
  v1-Grenze: EIN böswilliger Sitz kann per Stash-Race das Re-Anchoring
  eines Nachzüglers verzögern (Liveness, nie Safety) — per-Peer-Stashes
  wären der vollere Fix. Ein Voll-Halter hinter dem Schnitt wird beim
  Re-Anchor gepruned (die Mesh hält die Lückenblöcke nicht mehr) —
  unter B-F2 (Auto-Drop überall) ist das konsistent: Archiv-Halter
  existieren in diesem Design bewusst nicht, Recovery hängt seit 4c
  nicht mehr an ihnen.
- Recovery: das Welcome trägt Checkpoint-Blob + Suffix statt der vollen
  Chain; der Rejoiner verifiziert nach den Suffix-Regeln (§B.5).
  **Etappe 4c GEBAUT:** Das Welcome trägt die Pruned-Wire-Form
  (`ServedChainWire`, untagged — Voll-Koordinatoren behalten das nackte
  Array); der Rejoiner verifiziert per Suffix-Regeln und materialisiert
  aus der rid-gebundenen Founding-Tabelle des Blobs mit LEEREN
  Attestations (die Genesis-Signaturen sind mit Block 0 weg; Autorität
  ist der verifizierte Blob+Suffix, der lokale Founded-Record ist
  Bootstrap-Metadatum).
- Persistenz: `chain.state` additiv um den Blob erweitert (untagged:
  Array = voll, Objekt = pruned). **Ehrliche Old-Reader-Realität
  (Review 2026-07-18):** ein ALTES Binary liest ein pruned
  `chain.state` als „decode failed" und läuft CHAINLOS weiter — der
  Log-seitige `unknown_events`-Stopp greift nur, solange Checkpoint-
  Events im Tail liegen (vor dem nächsten Snapshot-Floor). Ein altes
  Binary auf einem pruned Workspace kann also in die Legacy-Zählpfad-
  Falle laufen; der saubere Stopp (Manifest-Format-Bump beim ersten
  Prune) ist als Etappe-5-Punkt gepinnt.

## B.6a A checkpoint SUMMARIZES — it does not archive

**Product decision, 2026-08-03 (user).** A checkpoint's state carries what
the republic **is**, never the path that produced it. Superseded values and
dead intermediates are dropped at the cut. That is what checkpoints are for.

**Today it is the opposite**, and that is a defect rather than a design:
§B.3 item 5 serializes the applied projection as `(proposal_id, payload)`
**in block order, complete**. So every logo the republic ever had, every
name it ever carried, stays in the blob forever.

That is not a size nicety. The blob is the **trust root a rejoiner must be
handed** once the genesis is gone (`nostr_n4b_step6_design.md`), and it is
already over budget: measured, a blob holding ONE 25 KiB logo costs 69628 B
against a 65408 B gift-wrap cap and a 128 KiB relay message budget
(`welcome_chain_budget.rs`). Keeping the history means the blob grows for
the life of the republic until recovery into an old republic simply stops
working — and it stops working silently.

### What "summarize" means precisely

The two kinds of applied entry are not the same and must not be treated the
same:

- **Last-write-wins slots** — Organization's `set_name`, `set_charter`,
  `set_chat_retention`, `set_image` / `remove_image`. Only the LAST applied
  entry per slot survives the cut. This is not a new judgement about what
  matters: `org_effective` (`proposals.rs`) already folds exactly this way,
  so the summary is nothing but that fold's own answer, kept instead of
  recomputed from a history nobody reads.
- **Accumulating items** — an entry that is a distinct object rather than a
  superseded state (Memory's notes). These are KEPT. A checkpoint is a
  summary, not a delete.

A surface must therefore declare which of the two each of its ops is. An op
whose kind is undeclared is treated as accumulating — the conservative
direction, since dropping something that was not superseded loses data.

### Invariants this must not weaken

- **`consumed_ids` keeps EVERY consumed id**, including those whose payload
  was dropped. It is the double-apply guard, and a summarized-away payload
  must never become a re-appliable proposal. It is already a field of its
  own (§B.3 item 6), so this is a constraint to respect, not a change.
- **Deterministic and versioned.** Every node folds independently and must
  reach byte-identical `checkpoint_canonical_bytes`, or a cut can never
  gather its m signatures — the failure would be a republic that can no
  longer compact, with no error pointing at why. Changing what goes into
  item 5 is a **`molt-chain-checkpoint-v3` → `-v4` bump** with every
  byte-pin test moved together (the CLAUDE.md versioned-layout rule).
- **Verify-before-sign is unchanged in shape**: a signer recomputes the same
  summary from its own chain and compares `state_hash`. The rule only moves
  what "the same summary" means, and it moves it identically on every node.

### What a pruned holder loses, stated rather than hidden

The history view below the cut stops showing superseded values — an old
logo, a former republic name. That is intended and is the point of the
decision. The chain keeps the complete record until a cut; after it, the
republic remembers what it is, not every step of how it got there.

### Open, and separate

Even a perfect summary does not bound the CURRENT image. That is the
transport-cap question (`ORG_IMAGE_MAX_BYTES` is 256 KiB while a 445 is
budgeted at 128 KiB for the whole websocket message) and it is decided in
its own place — the two together are what make a pruned republic
recoverable, and neither suffices alone.

## B.7 Grenzen (bewusst)

- Der flüchtige Log (Chat/Shares) bekommt NIE eine gemeinsame Checksum:
  ephemere Logs sind zwischen Nodes nicht identisch (Q4-Rejoiner ohne
  Backfill, Ankunftsreihenfolgen, lokale Uhren). Dafür bleibt WP4a
  zuständig — lokal, Policy-vollziehend.
- ~~Checkpoint-Auslösung ist Governance-Sache (menschliches Verb); keine
  Automatik in v1.~~ **Revidiert (Produktentscheidung 2026-07-18): die
  Auslösung ist AUTOMATIK** (`maybe_auto_checkpoint`, molt-engine
  chain.rs) — der GUI-Knopf ist entfernt, `propose_checkpoint` bleibt
  als co-equales MCP-Verb (manueller Override). Die Signier-Seite war
  immer schon mechanisch (verify-before-sign); automatisiert wurde nur
  der Trigger, kollisionsfrei und deterministisch:
  - Es triggert ausschließlich der **alphabetisch namenskleinste
    Roster-Member** (ein Proposer — Proposal-Ids sind knoten-lokal;
    zwei gleichzeitige Auto-Proposer würden kollidieren). Ist er
    offline, passiert nichts — wie bei einem abwesenden manuellen
    Proposer; das MCP-Verb bleibt der Ausweg.
  - Nur wenn dieser Knoten **selbst am Live-Head versiegelt hat**
    (`adopt_committed_block`, nach dem Re-Base): alle Co-Signer stehen
    dann am selben Head — genau was `receive_checkpoint_proposal` zum
    Nachrechnen braucht. Passiv angewendete Blöcke (`apply_next_block`:
    Catch-up-Serve, Broadcast eines anderen Sealers) triggern NIE — ein
    aufholender Knoten würde am veralteten Zwischen-Head schneiden, und
    ein im Gleichschritt aufholendes Quorum könnte diesen Cut sogar
    co-signieren und einen Holder NACH dem History-Drop forken
    (Review-Fund 2026-07-18; per Test gepinnt). Kein Trigger beim Open
    in ein kaltes Mesh (ohne Re-Serve wäre der Cut nur verloren).
  - Nie **während offener Abstimmungen** (offene Surface-Proposals,
    laufende Signatur-Sammlungen, oder ein bereits schwebender Cut) —
    ein dazwischen siegelnder Block würde den Cut nur stale machen.
    Der Commit, der die letzte offene Abstimmung auflöst, feuert den
    Check erneut.
  - **Stale braucht keinen Timer/Backoff**: der Block, der den Cut
    stale macht, durchläuft denselben Hook und re-proposed am neuen
    Head — maximal ein Auto-Propose pro committetem Block. Der
    Stale-Toast in der GUI ist entfernt (das Event bleibt für MCP);
    sichtbar bleibt nur „Checkpoint besiegelt #X".
  - Schwelle: `AUTO_CHECKPOINT_MIN_LEN = 32` lokal gehaltene Blöcke —
    Konstante, keine Einstellung (Kompaktion ist Hygiene, nicht Policy).

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
5. ✅ Doku + Härtung (2026-07-18): persistent_chain.md-Abschnitt,
   Manifest-Version-Bump beim ersten Prune (alte Binaries verweigern den
   Workspace statt chainlos zu laufen), Checkpoint-Lebenszyklus sichtbar
   (Event::CheckpointSealed/-Stale + Toasts), GUI-Verb im
   Organization-Status (co-equal zum MCP-Tool). Bewusst offen geblieben:
   kein ListProposals-Eintrag für pending Cuts (kollidiert mit dem
   Id-Kollisions-Guard aus Etappe 3; die Events geben dem Proposer
   Closure), Nachzügler-Buffer, per-Peer-Blob-Stashes.

6. **OPEN — the summary rule (§B.6a, decided 2026-08-03).** Item 5 of the
   canonical serialization keeps the complete applied history; it must keep
   the CURRENT state instead. Red tests, in this order:
   - a chain with three `set_image` blocks yields a blob carrying ONE image,
     and it is the last one (asserted by content, not by count — a summary
     that kept the FIRST would also pass a count check);
   - a note surface's entries all survive, so the rule cannot be read as
     "keep only the last entry" globally;
   - `consumed_ids` still lists every dropped payload's proposal id, and a
     block re-applying one of them is refused (the guard is the thing most
     likely to be lost by accident here);
   - two independently built chains fold to byte-identical
     `checkpoint_canonical_bytes` — without this a republic silently loses
     the ability to compact at all;
   - the `-v4` byte-pin fixture, recomputed independently.

Aus dem Etappe-2-Review offen für Etappe 3/4 (gepinnt, nicht vergessen):
- `after_block_applied` braucht einen Checkpoint-Arm (Event emittieren,
  `proposal_changes`/`pending_sigs` des Checkpoint-Proposals räumen).
- Etappe 4 muss ALLE `verify_chain`-Aufrufer (adopt_chain,
  append_committed_block, apply_next_block, tie_break, Open/Recovery) auf
  Suffix-Chains routen — heute würde `adopt_chain` eine gedroppte Chain
  beim Reopen löschen.
-   **4d GEBAUT:** beide Walker falten inkrementell (`fold_one` +
  `hash_walk_state`) — O(n) statt O(n·Checkpoints).
- Proposal-Id-Namespace ist knoten-lokal gemintet; Etappe 3 entschärft
  die Checkpoint-Seite (belegte Ids werden nie auto-signiert), die
  generelle Kollision zweier gleichzeitiger Proposals bleibt offen.
- Etappe-3-Review, offen für Etappe 5: (a) Checkpoint-Lebenszyklus ist
  unbeobachtbar (kein ListProposals-Eintrag, kein Event bei
  Seal/Stale-Drop — Operator muss Status/Chain lesen); (b) Liveness:
  ein Nachzügler verwirft den Cut und bekommt ihn nie re-served (kein
  Buffer, kein Re-Gossip) — bei < m aktuellen Nodes sealt nichts, der
  Proposer muss neu proposen. Beides bewusst v1, ehrlich dokumentiert.

## B.9 Vorgaben (einspruchsfähig)

- **B-F1:** `upto` beim Proposen = aktuelle Head-Höhe (kein frei
  wählbarer Schnitt in v1).
- **B-F2:** Lokales Droppen ≤ `upto` automatisch nach dem Commit (der
  Vote hat die Korrektheit bestätigt); kein separates Verb.

---

## Sicherheits-Audit WP4a + Mesh-Follow-ups (2026-07-25)

Adversariales Audit über den Change-Set `7b7542a..HEAD`. Angreifermodell: ein
**bösartiges Mitglied** der Republik (kann MLS-authentisch beliebige Announces
schicken), ein **bösartiger Server**, und ein Angreifer mit **Zugriff auf das
Medium** (für die Erasure-Behauptung). Keine kritische Lücke; fünf Befunde,
alle gefixt (Commit `15f0f2f` — er enthält entgegen seiner Commit-Message auch
Befund #5).

**#1 — Stiller Datenverlust: Peer ohne Cursor (WP4a).** Der Kompaktor bekam die
Liste der zu IGNORIERENDEN Peers; ein Peer ganz ohne Cursor-Eintrag (nie
beliefert, oder `transport.state` verloren — anderswo im Design ausdrücklich nur
ein Grund für Resends) hielt damit gar nichts zurück. Der Log konnte genau
denen unter den Füßen weggezogen werden, die ihn noch brauchten. Umgedreht: die
Engine liefert jetzt, wer den Log HÄLT (`peers_holding_the_log`), und ein
haltender Peer ohne Cursor zählt als „hat nichts bekommen" und stoppt die Runde.

**#2 — DoS durch ein Mitglied: dynamische Server-Tabelle (Mesh).** Die Tabelle
der gepinnten Fremdserver evictete nie. Ein Mitglied, das bei jeder Rotation
neue Hosts announced, füllt alle 64 Plätze dauerhaft; danach landet jeder
EHRLICHE Peer, der später den Server wechselt, auf unserer Primary — wo seine
Queue nicht existiert. Die Zustellung bricht also bei den Ehrlichen, nicht beim
Angreifer. Jetzt wird der am längsten ungenutzte Eintrag verdrängt.

**#3 — Erasure nur auf Platte, nicht im Speicher (WP4a).** Die Kompaktierung
verspricht, dass die Bytes eines gedroppten Segments wertlos sind, weil der
Schlüssel weg ist — der DEK blieb aber im freigegebenen Heap liegen (Eintrag
ohne Zeroize gedroppt; jede Klartext-Kopie der Tabelle beim Schreiben und Lesen
ebenfalls). `SegmentKey` zeroisiert jetzt beim Drop, beide Klartext-Puffer sind
`Zeroizing`. Das Dateisystem-Restrisiko (§A.3) bleibt wie dokumentiert bestehen.

**#4 — Verfügbarkeit: fsync auf dem Actor (selbst eingebaut).** Der neue
Pre-Backup-Flush wartete auf dem Actor auf einen fsync — gegen die eigene Regel,
dass Handler nie I/O awaiten; eine langsame Platte hätte die Kommandobearbeitung
angehalten. Beide kopierenden Pfade flushen jetzt in ihrem Blocking-Task, die
Reihenfolge-Garantie bleibt.

**#5 — Funktionsregression: `peek_genesis` (WP4a).** Der Open-Screen liest die
Genesis als ERSTEN Log-Frame und entschlüsselte ihn mit dem Workspace-Key. Nach
der F6-Migration liegt Segment 1 unter seinem DEK, nach dem ersten Drop ist es
ganz weg — der Workspace-Liste wären für genau die langlebigen Republiken
Roster und Charter abhandengekommen. `peek_genesis` liest jetzt segment-keyed
und fällt danach auf den Snapshot zurück, der alle Genesis-Fakten ohnehin trägt
(Attestations bleiben leer — Anzeige-Metadaten, nie Konsens-Input, wie beim
Checkpoint-Recovery).

**Geprüft und als akzeptabel bewertet (nicht geändert):**
- Ein Mitglied kann in `queues_extra` fremde oder sogar UNSERE eigene Queue
  announcen. Der Fan-out ist auf `MESH_REDUNDANCY_CAP` gedeckelt, der Inhalt
  bleibt MLS-verschlüsselt (der Announcer ist ohnehin Mitglied), und eine
  Selbst-Announce scheitert am Wrap-Key-Mismatch. Kosten: etwas Bandbreite.
- Der gebundene SSRF-lite des Pinned-Dial (siehe `mesh_redundancy_stage2.md`).
- Ein manipulierter `EngineStateDump` (via Restore-Blob) könnte `chat_pruned`
  oder die Ordinal-Zähler setzen — das setzt aber die Recovery-Phrase voraus,
  also dieselbe Vertrauensstufe wie das ganze Workspace.
- Peers jenseits der Karenz verlieren flüchtige Gossip-Events unter dem Floor.
  Das ist genau C2 (Catch-up über die Chain); die durable Wahrheit liegt in
  `chain.state`, das WP4a nie anfasst (C3).

**Offen (Testbefund, keine Sicherheitslücke) — VORBESTEHEND, gemessen:**
`recovery_distributes_the_rekey_commit_to_a_live_survivor` (two_instances)
wartet auf die Chat-Notiz der neuen MLS-Epoche beim Überlebenden und ist flaky.
Kontrollierter A/B-Test mit beiden Testbinaries, abwechselnd auf derselben
Maschine, je 10 Läufe: **Vor-Session (7b7542a) 2/10 Fehlschläge, HEAD 3/10** —
also vorbestehend, nicht von diesem Change-Set eingeführt. (Einzelne
6/6-grüne Serien auf dem alten Stand hatten das zunächst anders aussehen
lassen; nur der interleavte Vergleich ist aussagekräftig.) Zusätzlich
ausgeschlossen: die Kompaktierung feuert in diesem Test nie (per Log
verifiziert), und `ae92afd` (Mesh-Follow-ups) lief 6/6 grün. Der wahrscheinliche
Kandidat bleibt ein Rennen im Supervisor-Rebuild der Mesh-Extension gegen die
in-flight Chat-Notiz — ein eigener Untersuchungspunkt, kein Audit-Befund.
