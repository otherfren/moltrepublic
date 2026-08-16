# Multisig-Wallet-Surface: vollständiger Implementierungsplan

Stand 2026-07-18; **Revision 2026-08-16**: alle Code-Anker gegen master
verifiziert und die seit Juli gelandeten Umbauten eingearbeitet
(Charter-Features, SMP-Entfernung/Nostr-Transport, Zustellgarantie,
WP4a-Segmente, Storage-API-Umbenennungen). Dieses Dokument ist der
**ausführungsreife Bauplan** für
`Surface::Wallet` (die Monero-Multisig-Schatzkammer der Republik). Es ist so
geschrieben, dass die Implementierung ohne weiteren Kontext daraus erfolgen
kann: exakte Dateien, Symbole, Typen, Testnamen, Reihenfolge. Grundlage sind
die Analyse von eigenwallet/core (`~/projects/core`) und serai-dex/
monero-oxide sowie die am 2026-07-18 vom User ratifizierten
Produktentscheidungen.

**Für die Implementierung gilt zwingend:**
- Zeilennummern in diesem Doc sind Stand 2026-08-16 — **immer per Symbolname
  greppen**, nie blind an Zeilen editieren.
- **Keine upstream-API erfinden.** Die exakten Signaturen von `dkg-pedpop`,
  `modular-frost`, `monero-wallet` werden in Schritt 2 (Dep-Spike) gegen den
  Compiler gelockt. Alle API-Skizzen hier sind Richtungsangaben, deren
  Methodennamen im Spike zu verifizieren sind (CLAUDE.md: „Don't invent
  specs — fetch the real thing and lock it against the compiler").
- Repo-Regeln aus `CLAUDE.md` gelten vollständig; die für dieses Projekt
  kritischsten sind in §3 wiederholt.

---

## 1. Mission und ratifizierte Produktentscheidungen

Eine Republik erhält eine gemeinsame Monero-Kasse:

- **Krypto-Pfad: FROSTLASS** — das FROST-artige CLSAG-Threshold-Protokoll
  aus dem Serai-Ökosystem, heute gepflegt in
  `github.com/monero-oxide/monero-oxide` hinter dem default-off
  Cargo-Feature `multisig`. Sicherheitsbeweis: Cypher Stack, Paper
  „FROSTLASS" (IACR ePrint 2026/589); Implementierungs-Audit „Cypher Stack
  May 2025" liegt im `audits/`-Ordner von monero-oxide. Alles pure Rust,
  alle benutzten Krates MIT-lizenziert. **Kein wallet2-C++-FFI.**
- **Republik = Wallet:** alle n Mitglieder erhalten im DKG einen Key-Share.
  Das Spend-Threshold ist **dasselbe `rule_m`** wie in der Chain-Governance
  (Genesis-Feld; `State::threshold()` in `crates/molt-engine/src/lib.rs`).
  n und m sind ab Gründung fix (Produktentscheidung 2026-07-11: kein
  Seat-Adding, für immer). Es gibt EINEN Threshold-Begriff im System.
- **Scope: nur XMR.** (`bitcoin-serai` — FROST/Taproot über dasselbe
  `modular-frost` — bleibt dokumentierte spätere Option.)
- **Wallet ist ein Charter-Feature** (seit 2026-08-11,
  `docs_archive/ritual/charter_features.md`): `Surface::Wallet` ist optional —
  aktiviert bei der Gründung (Wizard Schritt 3) oder später per
  `set_features`-Vote, nie deaktivierbar. Die Legacy-Baseline
  (`features: None`) ist nur `["memory"]`. `require_feature`
  (`crates/molt-engine/src/chain.rs:1219`) weist Proposals auf einem
  deaktivierten Surface mit `MoltError::FeatureDisabled` ab; der Fold kann
  `["wallet"]` bereits (grüner Test `chain.rs:7493`). Für diesen Plan:
  `WalletInit` setzt das aktivierte Feature voraus (§7.1), und §11 baut
  auf der feature-getriebenen Nav-Sichtbarkeit auf statt auf einem
  hartkodierten Gate.
- **Gestufter Ausbau:**
  - **Etappe 1:** DKG-Ritual „Kasse einrichten" + gemeinsame Adresse +
    Shared-View-Key-Scanning → die Views `balance`, `history`, `receive`,
    `status` sind ECHT. `send`/`settings` sind ehrliche „Etappe 2"-Stubs.
  - **Etappe 2:** Spend-Flow (Propose → m-of-n-Approve → 2 FROST-Runden →
    Broadcast). Skizze in §13.
  - Jede Etappe endet getestet-grün auf master.

## 2. Der ausgeliehene Stack (Analyse-Ergebnis)

### 2.1 Echte Dependencies (serai / monero-oxide)

| Krate | Version/Quelle | Lizenz | Rolle |
|---|---|---|---|
| `dkg` | 0.6.1, crates.io | MIT | Kerntypen: `ThresholdParams`, `ThresholdKeys`, `Participant` (NonZero u16) |
| `dkg-pedpop` | 0.6.0, crates.io | MIT | Das DKG selbst: PedPoP (Pedersen-VSS + Proofs of Possession), 2 Runden, identifizierbare Aborts (`BlameMachine`) |
| `modular-frost` | 0.11.1 (2026-07-26), crates.io, Feature `ed25519` | MIT | Threshold-Signing: `AlgorithmMachine` → `.preprocess(rng)` → `.sign(HashMap<Participant, Preprocess>, msg)` → `.complete(HashMap<Participant, SignatureShare>)`; ungültige Shares werden dem Signierer attribuiert |
| `monero-wallet` | **0.2.0, crates.io** (koordiniertes monero-oxide-0.1.0-Release 2026-07-31), `features=["multisig"]` | MIT | `SignableTransaction::multisig(ThresholdKeys<Ed25519>)`, `Scanner`, `GuaranteedViewPair`, Adress-Typen; `multisig` aktiviert std/transcript/frost + `monero-clsag/multisig` |
| `monero-clsag` | 0.1.x, crates.io (kommt transitiv mit `multisig`) | MIT | FROSTLASS `ClsagMultisig`-Algorithmus |
| `monero-simple-request-rpc` (+ `monero-interface`) | 0.1.0, crates.io (gleiches Release) | MIT | monerod-RPC-Client |

Protokoll-Fakten (im Spike gegen den Compiler verifizieren):

- **DKG (dkg-pedpop), 2 Runden:** Runde 1 = Broadcast von `Commitments`
  (Polynom-Commitments + Proof of Possession) an alle; Runde 2 =
  per-Empfänger **verschlüsselte** `SecretShare`s (das Protokoll bringt
  eigene per-Message-Verschlüsselung mit; es verlangt einen
  **authentifizierten** Kanal — unser MLS-Mesh erfüllt das). Abschluss
  liefert `ThresholdKeys<Ed25519>`. Fehlerfall: `BlameMachine` attribuiert
  den Schuldigen beweisbar. **Alle n müssen teilnehmen.** Es gibt KEIN
  Resharing (passt zu „n+m fix für immer").
- **Signing (modular-frost), 2 Runden:** preprocess-Broadcast, dann
  share-Broadcast, dann complete. Alle Nachrichten implementieren
  `Writable` (Byte-Serialisierung) → passen direkt in unsere Wire-Events.
- **Monero-Tx-Ebene (`monero-wallet`, Feature `multisig`):** eine
  FROST-Maschine **pro Input**; alle Signierer brauchen die
  **byte-identische `SignableTransaction`** (inkl. Decoys); das
  `msg`-Argument der Sign-Runde MUSS leer sein; **Preprocess-Caching ist
  für die Tx-Maschine verboten**; Inputs nach Key-Image sortiert.
- **Scanning:** Shared-View-Scalar ist ein normaler Skalar, den jedes
  Mitglied hält; gescannt wird mit `GuaranteedViewPair`/`GuaranteedScanner`
  (schützt vor dem Burning-Bug). **Nur Main-Address** — Subaddress +
  Multisig ist upstream unverifiziert.
- **Vorbehalte (Upstream-Stand 2026-08-16):** die git-rev-Pflicht ist
  GEFALLEN — monero-oxide hat am 2026-07-31 sein erstes koordiniertes
  Release (0.1.0-Ökosystem) auf crates.io publiziert; alle benötigten
  Krates werden als crates.io-Versionen gepinnt (§6). Das
  `multisig`-Feature bleibt eingeschränkt: laut Wallet-README „not
  covered by SemVer, **except along minor versions**" → die
  Minor-Version exakt pinnen (`0.2.x` ok, nie stillschweigend auf 0.3
  floaten), Upgrade = bewusster Akt mit Diff-Read. CLSAG-only — und der
  **FCMP++-Horizont ist nah** (Details unten): Mainnet-Fork noch nicht
  aktiviert (Milestone 66 % per 2026-08-16, kein Datum fixiert, zweites
  Beta-Stressnet läuft seit 2026-05-06), aber die Implementierung lebt im
  aktiv gepflegten `fcmp++`-Branch von monero-oxide. Konsequenz für
  diesen Plan: Etappe 1 ist fork-robust (FCMP++ migriert keine
  Wallets/Adressen/Outputs — der DKG-Gruppenkey und die Adresse
  überleben; nur das Tx-Format-Parsing braucht am Fork ein
  Dependency-Upgrade zum Weiter-Scannen). Etappe 2 trifft die
  CLSAG-Ablösung: VOR dem Bau von §13 den Fork-Status prüfen und ggf.
  direkt das FCMP++-GSP-Multisig (2-Runden, FROST-inspiriert, gleicher
  Autor) statt FROSTLASS bauen — gleiche `ThresholdKeys`, anderer
  Signier-Algorithmus (im Etappe-2-Design-Pass gegen den Compiler
  verifizieren).

### 2.2 Muster aus eigenwallet/core (Referenz, KEINE Dependency)

- `monero-wallet-ng` (`~/projects/core/monero-wallet-ng/src/`): Scanner als
  Stream (Fetcher-Task + Scanner-Task → Subscription), Confirmation-Tracker,
  `verify_transfer`, Retry/Backoff — Vorlage für unseren `scan.rs`.
- `monero-rpc-pool`: Node-Pool/Health/Hedging/Tor-via-arti — spätere
  Härtung, Etappe 1 nutzt EINEN konfigurierten Daemon.
- `monero-harness`: regtest-monerod (Docker-Image
  `ghcr.io/sethforprivacy/simple-monerod`) — Referenz für den manuellen
  Integrationstest (§10.6).
- UX-Checkliste aus der Tauri-GUI (für unsere Panes, §11): Balance
  total/unlocked, Sync-Fortschritt mit Ziel-Höhe, Empfangsadresse mit
  Copy, History mit Confirmations-Badge, Send-Bestätigung mit
  Betrag/Fee/Ziel + Countdown (Etappe 2), Seed/Secret niemals ungefragt
  anzeigen.

### 2.3 Was nirgends existiert (bauen WIR)

Runden-Orchestrierung über ein Gruppen-Mesh, Proposer-/Ratifizierungs-Flow,
persistenter Wallet-Zustand (Cursor, Outputs, Backup), Recovery-Story für
Shares, Governance-Anbindung, UI. eigenwallet hat **kein** Multisig (nur
additive 2-of-2-Key-Teilung im Atomic-Swap, bei der der Spender den vollen
Key rekonstruiert — für eine dauerhafte Kasse unbrauchbar).

## 3. Repo-Regeln, die hier besonders greifen

1. **TDD:** jeder Schritt beginnt mit roten Tests (§10 nennt sie). Erst
   fehlschlagen sehen (aus dem richtigen Grund), dann implementieren.
2. **clippy = 0, auch Tests;** `.expect("…")` statt `.unwrap()` überall.
   `cargo clippy --all-targets` vor jedem Commit.
3. **Co-Equality-Test:** jede neue `Command`-Variante MUSS entweder
   MCP-Tool werden (`crates/molt-mcp/src/lib.rs::tools()`, ~Z.580) oder auf
   die dokumentierte `INTERNAL`-Liste (derzeit `[&str; 54]`, ebenda ~Z.1592) —
   sonst wird `co_equality_every_command_is_a_tool_or_documented_internal`
   rot. Netz-/Ritual-Feedback = INTERNAL; menschliche Verben = Tool.
4. **Events additiv:** neue `WorkspaceEvent`-Varianten sind erlaubt, neue
   Felder auf bestehenden nur mit `#[serde(default)]`. Ein älterer Leser,
   der eine unbekannte Variante trifft, darf nicht schreiben.
5. **Kein I/O in molt-core.** Engine-Command-Handler sind synchron und
   awaiten nie; alles Blockierende läuft als gespawnter Task, der per
   engine-internem `Net*`-Command zurückmeldet (Ticker-/Ritual-Muster).
6. **Keine Fake-Daten in der UI**, sobald ein Element live ist (die
   bisherige `WalletPane` ist ein Mock — wird ersetzt, §11). Piktogramme
   nur als Twemoji-Font / `AppButton.emoji`-Prop, nie Plain-Text-Emoji.
7. **Direkt auf master arbeiten**; Review über den Diff vor dem Landen;
   Endzustand getestet-grün auf master.
8. **Bauen:** GUI-Validierung = `cargo build -p molt-ui-window -p molt-ui`
   (einmal pro Change-Set); Iteration über `scripts/dev-ui.sh`. Nie zwei
   window-scale Builds parallel (OOM-Killer); bei knappem RAM `-j 1`.
   Nie ein GUI-Fenster auf `DISPLAY=:0` starten.
9. **Secrets nie in getrackten Artefakten benennen** (auch nicht in
   Commit-Messages zu `wallet.state`-Handling — generisch bleiben).

## 4. Verifizierte Anker im Repo (Symboltabelle)

| Anker | Ort (Stand 2026-08-16) | Rolle für dieses Projekt |
|---|---|---|
| `Surface::Wallet`, Views `balance/history/send/receive/status/settings` | `crates/molt-core/src/lib.rs:62`, `:127` (`Surface::views`) | existiert schon; Views nicht umbenennen |
| `Surface::is_charter_feature` / `canonical_features` / `LEGACY_FEATURES` | ebenda ~Z.104/195/110 | Wallet ist optionales Feature (§1, §7.1) |
| `SurfaceSnapshot` | ebenda ~Z.4852 | bekommt additives Feld `wallet` (§8.4) |
| `Command`-Enum + `Net*`-Muster | ebenda ~Z.3162ff | neue Varianten §8.1 |
| `WorkspaceEvent` | ebenda ~Z.2108ff | neue Varianten §8.2 |
| `crosses_wire(event)` | `crates/molt-engine/src/net.rs:387` | Wallet-Rundenevents hier aufnehmen |
| `cmd_net_delivered` | `crates/molt-engine/src/net.rs:1075` | Empfangs-Dispatch: neue Arme |
| `persist_crypto_blocking`-Callsites (Clean-Close) | `crates/molt-engine/src/net.rs:621/632` | daneben `wallet.state`-Final-Write |
| `is_chain_governed` / `chain_sign_and_gossip_approval` / `collect_sig` / `try_commit` | `crates/molt-engine/src/chain.rs:1562/1843/1871/1890` | unverändert wiederverwenden; `try_commit` sealt bei m niedrigst-benannten gültigen Signierern und trägt seit 2026-08-08 den Restored-Consent-Sonderfall — der `wallet_init`-Guard (§7.2) wird sein Nachbar |
| `require_feature` / `effective_features` | `crates/molt-engine/src/chain.rs:1219/1197` | Feature-Gate für `WalletInit` (§7.1) |
| Checkpoint-Muster (Engine rechnet Inhalt selbst nach, Auto-Co-Sign nur bei exaktem Match) | `crates/molt-engine/src/chain.rs::cmd_propose_checkpoint` (~Z.2661) / `receive_checkpoint_proposal` (~Z.2709) | Vorbild für den Terminal-Block §7.5 |
| `State.net_ritual: Option<founding::RitualRuntime>` | `crates/molt-engine/src/lib.rs:792` | Vorbild für `wallet_ritual` |
| `State::threshold()` | `crates/molt-engine/src/lib.rs:1115` | = `rule_m` |
| `cmd_propose` / `cmd_approve` | `crates/molt-engine/src/proposals.rs:373/550` | Konsens-Wiederverwendung; Sonderfall alle-n §7.2 |
| Ticker-Muster | `crates/molt-engine/src/lifecycles.rs::spawn_ticker_every` (~Z.2267; hieß früher `spawn_ticker`) | Vorbild Timeout-/Scanner-Task-Anbindung |
| `transport_key`/`chain_key` + `read/write_transport_state`, `read_chain`/`write_chain` (frühere Namen `read/write_chain_state`) | `crates/molt-storage/src/lib.rs` ~Z.1380–1500, `persist_chain_blocking` ~Z.2604 | 1:1-Vorbild für `wallet_state` (§9) |
| Segment-Konstanten `TRANSPORT_SEGMENT = u64::MAX-1`, `CHAIN_SEGMENT = u64::MAX-2`, `KEYS_SEGMENT = u64::MAX-3` (WP4a) | `crates/molt-storage/src/lib.rs:84–92` | neu: `WALLET_SEGMENT = u64::MAX - 4` (−3 ist seit WP4a BELEGT) |
| Export/Import-Module | `crates/molt-storage/src/export.rs` / `import.rs` | dort `wallet.state` in Include + Allowlist (§9) |
| Backup-Include-Tabelle + Import-Allowlist | `docs_archive/storage/backup_restore_design.md` §3.2 (Tabelle ~Z.105) und §-Import (~Z.332, Allowlist `manifest.toml, prefs.toml, chain.state, …`) | `wallet.state` ergänzen (Doc + Code) |
| E2E-Vorbilder | `crates/molt-engine/tests/two_instances.rs::founding_governs_over_the_direct_mesh` (~Z.1421), `tests/three_nodes.rs` | Stil für den DKG-E2E-Test (Loopback bleibt DER Test-Transport) |
| `#[ignore]`-Netztest-Präzedenz | `crates/molt-net/tests/nostr_relay_poc.rs` (das frühere Vorbild `ritual_engine_over_smp.rs` fiel mit dem SMP-Transport) | Stil für den monerod-Test |
| Mock-`WalletPane` | `crates/molt-ui-window/ui/surfaces.slint:2248`; eingebunden `app.slint:33`, Routing ~Z.6353 | vollständiger Design-Mock aller sechs Views; wird echt (§11) |
| Nav-Sichtbarkeit | feature-getrieben: molt-ui filtert auf `org_stats.features` (`crates/molt-ui/src/lib.rs:3991`); ein aktiviertes-aber-ungebautes Surface öffnet die gebadgte Mock-Pane | KEIN hartkodiertes Gate mehr (§11.1) |
| GUI-Logik | `crates/molt-ui/src/lib.rs` (`apply_surfaces` ~Z.4294, `issue` ~Z.2553, toter „transfer"-Mock in `default_op` ~Z.5942) | §11 |
| MCP `tools()` / `INTERNAL` | `crates/molt-mcp/src/lib.rs:580/1592` | §8.5 |
| **Namensfalle:** `WalletHandle` in molt-engine | `crates/molt-engine/src/lib.rs:103` | ist der ENGINE-AKTOR-Handle (konkinwallet-Erbe), NICHT die Schatzkammer. Nicht umbenennen, nicht verwechseln. |

## 5. Architektur

### 5.1 Neues Crate `crates/molt-treasury`

Im Workspace-`Cargo.toml`-Header bereits als zukünftiges Crate genannt.
Layering: Geschwister von `molt-net` **unterhalb** der Engine
(core → config → storage → net/treasury → engine → mcp → ui → app).
Dependencies: `molt-core`, die Monero-/FROST-Krates aus §2.1, `tokio`,
`zeroize`, `serde`, `hex`, `sha2`, `tracing`. **Nicht**: molt-net,
molt-engine, molt-storage (Storage spricht die Engine an, nicht treasury).

Module:

```
crates/molt-treasury/src/
  lib.rs      // pub-Fassade + Fehlertyp TreasuryError (thiserror)
  roster.rs   // deterministisches Mapping Rosternamen -> Participant
  dkg.rs      // DkgRitual: Wrapper um die pedpop-Rundenmaschinen
  keys.rs     // WalletKeys: ThresholdKeys-Serialisierung, View-Scalar,
              // Adress-Ableitung, view_key_commitment
  rpc.rs      // dünner monerod-Client über monero-simple-request-rpc
  scan.rs     // async Scanner-Task (von der Engine gespawnt)
  sign.rs     // ETAPPE 2 (in Etappe 1 nicht anlegen)
```

Warum eigenes Crate: isoliert die git-Deps (Kompilierzeit, Risiko),
unit-testbar ohne Engine, und der Header verspricht genau diese Form
(„plug into molt-engine behind the same Surface/Approvable contract").

### 5.2 Kontrakt in molt-core (`crates/molt-core/src/wallet.rs`, neu)

Nur I/O-freie, monero-oxide-freie Typen; Beträge als `u64` (Piconero),
Adressen/Hashes als `String`:

```rust
/// Read-model of the treasury for frontends (SurfaceSnapshot.wallet).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalletView {
    pub address: String,          // group main address ("" until created)
    pub balance: u64,             // piconero, confirmed
    pub pending: u64,             // piconero, < 10 confirmations
    pub scan_height: u64,
    pub daemon_height: u64,
    pub connected: bool,
    pub threshold: u32,           // = rule_m
    pub participants: u32,        // = n
    pub dkg: DkgPhase,
    /// member name -> holds a live key share (false = watch-only, §12)
    pub shareholders: Vec<(String, bool)>,
    pub history: Vec<WalletTxView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalletTxView {
    pub tx_hash: String,
    pub amount: u64,              // piconero received
    pub height: u64,
    pub confirmations: u64,
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DkgPhase { None, Proposed, Round1, Round2, Ready, Failed }

/// Payload-op strings (chain payload `{"op": ...}`).
pub const OP_WALLET_INIT: &str = "wallet_init";
pub const OP_WALLET_CREATED: &str = "wallet_created";
```

(Exakte Feldliste darf beim Implementieren wachsen, aber: serde-additiv
denken, Feldnamen snake_case, keine floats.)

## 6. Dependency-Pinning (Schritt 2, „Dep-Lock-Spike")

1. Seit dem koordinierten monero-oxide-Release (2026-07-31) sind ALLE
   Krates auf crates.io — kein git-Pinning mehr. In
   `[workspace.dependencies]` des Workspace-`Cargo.toml` eintragen
   (Minor exakt pinnen, §2.1-Semver-Vorbehalt des multisig-Features):

   ```toml
   monero-wallet = { version = "0.2", default-features = false, features = ["std", "multisig", "compile-time-generators"] }
   monero-simple-request-rpc = "0.1"
   modular-frost = { version = "0.11", default-features = false, features = ["ed25519"] }
   dkg = "0.6"
   dkg-pedpop = "0.6"
   ```

   (Feature-Namen im Spike gegen die realen Cargo.tomls der publizierten
   Versionen prüfen; monero-wallet 0.2.0 verlangt `modular-frost ^0.11`
   mit `ed25519` — die Matrix ist konsistent, `dkg-pedpop` steht bei
   0.6.0, `dkg` bei 0.6.1, `modular-frost` bei 0.11.1.)
2. Bare `molt-treasury` anlegen, das nur Typen berührt: eine Funktion, die
   `ThresholdParams` baut, eine, die einen `KeyGenMachine`-Rundenzyklus
   in-memory für n=3 durchläuft, eine, die aus `ThresholdKeys<Ed25519>`
   die Main-Address ableitet. **Compilieren = Spec gelockt.**
3. `cargo tree -d` prüfen: keine doppelten Versionen von
   `modular-frost`/`dkg`/`curve25519-dalek`/`ed25519-dalek`/`zeroize`
   im Workspace (sonst unifizieren `ThresholdKeys`-Typen nicht bzw.
   wachsen Binaries sinnlos).
4. TLS/HTTP: Etappe 1 spricht plain HTTP zu einem lokalen oder
   Onion-Daemon. Falls die RPC-Krate TLS-Features hat, die `ring`/
   `native-tls` ziehen: abschalten (Pure-Rust-Posture; `ring` ist eine
   sanktionierte Ausnahme, keine Einladung).
5. Die gepinnten Versionen + dieses Prüfprotokoll ins Design-Doc (§14)
   schreiben.

## 7. Das „Kasse einrichten"-Ritual (Etappe-1-Kern)

Vorbilder: Founding-Ritual (einstimmig, ephemer bis zum Seal, one-shot,
sign-what-you-see) und Checkpoint (jede Engine rechnet den Inhalt selbst
nach und co-signt nur bei exaktem Match). Ablauf:

### 7.1 Trigger

`Command::WalletInit` (neues menschliches Verb; MCP-Tool + GUI-Button).
Der Handler:
- lehnt ab, wenn das Charter-Feature „wallet" nicht aktiviert ist
  (`require_feature(Surface::Wallet)` → `MoltError::FeatureDisabled`;
  aktivieren per Gründungs-Wizard oder `set_features`-Vote — §1), wenn die
  Republik nicht chain-governed ist, ein Wallet bereits existiert (ein
  `wallet_created`-Applied in der Chain) oder ein Init in-flight ist
  (`state.wallet_ritual.is_some()` oder ein offenes `wallet_init`-Proposal)
  → `MoltError`-Varianten analog CreatePropose. Das Propose-Gate ist lokale
  Höflichkeit (Kommentar bei `require_feature`: ein Peer auf anderem Build
  umgeht es) — deshalb prüfen AUCH die Empfangs-Arme (§7.3) und der
  Ritual-Start das Feature, bevor sie ein DKG beginnen.
- spawnt einen Kurz-Task (Engine-Handler awaiten nie), der die
  Daemon-Höhe holt und `Command::NetWalletInitReady { height, generation }`
  zurücksendet (via `net::CmdSink`-Muster).
- `NetWalletInitReady`-Handler: mintet ein NORMALES Pending-Proposal auf
  `Surface::Wallet` mit Payload
  `{"op":"wallet_init","birthday_height":<height - 720>}` (720 Blöcke ≈ 1
  Tag Sicherheitsmarge) über den bestehenden `cmd_propose`-Pfad (inkl.
  `WorkspaceEvent::Proposed`-Gossip und Self-Co-Sign). Die Birthday-Höhe
  steht in der ratifizierten Absicht → deterministischer Scan-Start für
  alle.

### 7.2 Konsens: alle n, über bestehende Approve/Decline

Kein neues Voting. Sonderfall in `cmd_approve`/`try_commit`-Vorstufe:
ein Proposal mit `op == "wallet_init"` wird NICHT bei m gesealt, sondern
startet das DKG erst, wenn **alle n** Approvals (gültige Chain-Signaturen,
dedupliziert wie in `collect_sig`) vorliegen. Begründung: PedPoP braucht
ohnehin alle n; einen Key-Share zu halten ist eine explizite
per-Member-Einwilligung (sign-what-you-see). Ein einziges `Decline`
beendet den Vorgang (Proposal → declined, Ritualzustand verworfen).
Implementierung: in der Approval-Sammellogik einen Guard
`is_wallet_init(proposal)` einziehen, der das Sealing unterdrückt und
stattdessen bei n den Ritualstart auslöst. Der Terminal-Block (7.5) wird
später unter derselben `proposal_id` gesealt.

### 7.3 DKG-Runden über das Mesh

Der Rundenverkehr sind **`WorkspaceEvent`-Varianten** über den bestehenden
Gossip-Pfad (`crosses_wire()` → Workspace-Log als Outbox → beim Peer via
`Command::NetDelivered` → `cmd_net_delivered`-Dispatch). KEINE neuen
Net*-Commands für Wire-Verkehr.

- Bei n-of-n-Konsens baut jede Engine
  `wallet_ritual = Some(treasury::DkgRitual::new(params, my_participant,
  roster_mapping, init_id))` und broadcastet ihre Runde-1-Nachricht als
  `WorkspaceEvent::WalletRound1 { init_id, member, payload_hex }`.
- `cmd_net_delivered`-Arm für `WalletRound1`: an
  `wallet_ritual.receive_round1(member, bytes)` füttern (idempotent —
  Duplikate droppen; unbekannte `init_id` oder kein Ritual → defensiver
  Drop mit `tracing::warn`, wie beim Governance-Gossip). Idempotenz ist
  hier PFLICHT, nicht Defensive: die Zustellgarantie (2026-07-28,
  `docs_archive/transport/delivery_guarantee.md`) liefert jedes Wire-Event
  at-least-once — Duplikate kommen konstruktionsbedingt (Rewind-Resend),
  nicht nur im Fehlerfall. Sobald alle n−1
  Peer-Commitments da sind, erzeugt das Ritual die Runde-2-Nachricht:
  `WorkspaceEvent::WalletRound2 { init_id, member, shares_hex }`.
  Die per-Empfänger-Shares sind PedPoP-eigen verschlüsselt; MLS liefert
  die geforderte Sender-Authentizität — Broadcast über den Gruppenkanal
  ist damit sicher.
- `WalletRound2`-Arm analog → `receive_round2` → bei Vollständigkeit
  Abschluss: `DkgOutcome { threshold_keys, group_address, view_scalar,
  view_key_commitment }`.
- Fortschritt als Broadcast-`Event::WalletDkgProgress { phase, have, need }`
  emittieren (UI-Anzeige).
- Rundenverarbeitung ist reine Berechnung (kein I/O) → läuft on-actor im
  Handler; die Maschinen leben in `State.wallet_ritual` (Vorbild
  `net_ritual`).

### 7.4 Timeout, Abort, Blame

- `WalletInit` spawnt zusätzlich einen Deadline-Task (Ticker-Muster,
  z.B. 10 min), der `Command::NetWalletDkgTimeout { init_id, generation }`
  einspeist. Kommt er an, während das Ritual noch läuft: Abbruch.
- Ein Share, dessen Verifikation fehlschlägt, wird über die pedpop-
  `BlameMachine` attribuiert.
- Abbruchpfad (beide Fälle): `WorkspaceEvent::WalletAbort { init_id,
  blamed: Option<String>, reason }` broadcasten,
  `Event::WalletDkgFailed { reason, blamed }` emittieren,
  `wallet_ritual = None` (**ephemer: vor Abschluss wird NICHTS
  persistiert — Crash/Cancel hinterlässt keine Spur**), Proposal als
  declined markieren.
- Re-Run = frisches `WalletInit` mit neuer `proposal_id`
  (CreatePropose-Semantik: einmalig; Cancel + Re-Mint statt Reuse).

### 7.5 Terminal-Block (Checkpoint-Muster)

Nach lokalem DKG-Abschluss, in dieser Reihenfolge:

1. **Erst persistieren:** `wallet.state` schreiben (§9). (Crash zwischen
   Persist und Block: der Share ist sicher, der Block kommt per
   Chain-Catch-up nach.)
2. Dann Auto-Co-Sign der Change
   `ChainChange::Applied { proposal_id: init_id, surface: Wallet, payload }`
   mit Payload:

   ```json
   {"op":"wallet_created",
    "address":"<group main address>",
    "view_key_commitment":"<hex sha256(\"molt-wallet-view-v1\\0\" || scalar)>",
    "threshold": <rule_m>, "participants": <n>,
    "birthday_height": <aus der Absicht>}
   ```

   Jedes Feld ist deterministischer DKG-Output oder ratifizierte Absicht →
   byte-identisch bei allen (serde_json-Maps sind BTreeMap-kanonisch, die
   Chain nutzt das bereits). Bestehende `collect_sig`/`try_commit` sealen
   bei m und broadcasten `Committed`. **Keine neue `ChainChange`-Variante.**
3. `Event::WalletReady { address }` emittieren; Scanner starten (§8.6).

**Nie im Block:** der private View-Scalar oder irgendein Share-Material.
Jeder DKG-Teilnehmer berechnet den Scalar selbst. Ein per Recovery
wiederhergestelltes Mitglied bekommt ihn von einem Peer über den
MLS-Recovery-Kanal und verifiziert gegen das On-Chain-Commitment
(Chain = Authentizität, MLS = Vertraulichkeit — die bestehende Teilung).

### 7.6 Deterministisches Participant-Mapping (`roster.rs`)

`Participant` ist NonZero u16. Mapping: Rosternamen (die anchored names
aus dem Founding) **byteweise aufsteigend sortiert**, Index 1..=n.
Muss permutationsstabil sein und exakt dem Sortierkriterium der
„m niedrigst-benannten Signierer"-Regel in `try_commit` entsprechen
(gleiche Vergleichsfunktion verwenden!). Der DKG-Gruppenkey ist ein
NEUER Key — **nicht** die Roster-Ed25519-Identität (Lektion der
gesalzenen Ritual-Identitätskeys: nie aus dem Member-Handle re-deriven).

## 8. Exakte Kontrakt-Erweiterungen

### 8.1 `Command` (molt-core)

```rust
/// Human verb: start the one-shot treasury DKG ritual. MCP tool.
WalletInit,
/// Engine-internal: daemon height probe for WalletInit returned.
NetWalletInitReady { height: u64, generation: Option<u64> },
/// Engine-internal: scanner delivered new outputs / cursor.
NetWalletScanned { outputs: Vec<WalletTxView>, height: u64, generation: Option<u64> },
/// Engine-internal: scanner health/status probe.
NetWalletStatus { daemon_height: u64, connected: bool, detail: String, generation: Option<u64> },
/// Engine-internal: DKG deadline fired.
NetWalletDkgTimeout { init_id: u64, generation: Option<u64> },
```

(`generation` folgt dem bestehenden Mesh-Generation-Muster der anderen
Net*-Varianten — beim Implementieren an den Nachbarn orientieren.)

### 8.2 `WorkspaceEvent` (additiv; alle in `crosses_wire()` aufnehmen)

```rust
/// Treasury DKG round 1: PedPoP commitments + proof of possession.
WalletRound1 { init_id: u64, member: String, payload_hex: String },
/// Treasury DKG round 2: encrypted secret shares (protocol-encrypted).
WalletRound2 { init_id: u64, member: String, shares_hex: String },
/// Treasury DKG aborted (timeout or attributed bad share).
WalletAbort { init_id: u64, blamed: Option<String>, reason: String },
```

Empfangs-Arme in `cmd_net_delivered` (idempotent — at-least-once-Duplikate
sind der Normalfall, §7.3 — plus defensive Drops).
Ältere Leser: unbekannte Variante ⇒ nicht schreiben (Regel existiert).

### 8.3 Broadcast-`Event` (molt-core, Frontend-Stream)

```rust
WalletDkgProgress { phase: DkgPhase, have: u32, need: u32 },
WalletReady { address: String },
WalletDkgFailed { reason: String, blamed: Option<String> },
WalletUpdated { height: u64, balance: u64 },
```

### 8.4 `SurfaceSnapshot`

```rust
/// Wallet-only read model (None for other surfaces / no wallet yet).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub wallet: Option<WalletView>,
```

Befüllung in `State::snapshot` (proposals.rs), nur bei
`surface == Surface::Wallet`. `pending`/`applied` laufen generisch weiter
(die `wallet_init`-Abstimmung erscheint in Pending, der
`wallet_created`-Block im Applied-Log — kostenlos).

### 8.5 MCP (`crates/molt-mcp/src/lib.rs`)

- Neues Tool in `tools()`: `wallet_init` — Beschreibung: startet das
  einmalige Kassen-Gründungsritual (DKG); keine Argumente; baut
  `Command::WalletInit`. (Konsens läuft über die bestehenden
  `approve`/`decline`-Tools; `read_state` mit `surface=wallet` liefert die
  Views — keine weiteren Tools nötig.)
- `INTERNAL` erweitern (54 → 58): `"net_wallet_init_ready"`,
  `"net_wallet_scanned"`, `"net_wallet_status"`, `"net_wallet_dkg_timeout"`
  mit dem üblichen Begründungskommentar (Netz-Feedback; ein MCP-Agent darf
  keine Scanner-/Ritual-Ergebnisse fälschen).

### 8.6 Engine-Zustand und Scanner-Anbindung

```rust
// in State (crates/molt-engine/src/lib.rs), neben net_ritual:
pub(crate) wallet_ritual: Option<molt_treasury::DkgRitual>,
pub(crate) wallet: Option<WalletProjection>,   // Read-Model, engine-eigen
```

`WalletProjection` (engine-intern, `crates/molt-engine/src/treasury.rs`
neu — Modul-Datei im Engine-Crate für die cmd_*-Handler): Outputs
(tx_hash, amount, height, key_image-frei in Etappe 1), scan_height,
daemon_height, connected, dkg-Phase, Adresse. Balance = Summe der Outputs
(Etappe 1: nichts ausgebbar → alles unverbraucht; `pending` = Outputs mit
< 10 Confirmations — Monero-Unlock-Fenster).

Scanner: `molt_treasury::scan::run(rpc_url, view_pair, start_height,
poll_interval, sink)` wird bei `cmd_open_workspace` (und nach 7.5)
gespawnt, wenn die Chain ein `wallet_created` trägt und `wallet.state`
lesbar ist; er meldet über den `CmdSink` `NetWalletScanned`/
`NetWalletStatus` zurück. Task-Ende beim Workspace-Close über das
bestehende Abbruch-Muster der Ticker (WeakSender-Upgrade schlägt fehl →
Task endet). Config: neue Sektion in molt-config:

```toml
[wallet]
daemon_url = "http://127.0.0.1:18081"   # regtest/stagenet/mainnet-Daemon
network = "mainnet"                       # "mainnet" | "stagenet" | "regtest"
```

(Defaults; Settings-UI-Editing ist Etappe 2 — Etappe 1 liest nur die
Config-Datei. `ConfigNotice`/Reload-Muster existiert bereits.)

## 9. Persistenz: `wallet.state` (molt-storage)

Exakt das `chain.state`-Muster kopieren
(`crates/molt-storage/src/lib.rs`, `chain_key`/`read_chain`/`write_chain`
~Z.1371–1500 — die Funktionen hießen zur Planungszeit
`read/write_chain_state`):

- Sub-Key: `fn wallet_key(&self) -> [u8;32] {
  hkdf32(&self.key, "molt-wallet-state", &self.id) }`
- Segment: `const WALLET_SEGMENT: u64 = u64::MAX - 4;` — **NICHT −3**:
  den Slot hält seit WP4a `KEYS_SEGMENT` (`log/keys.state`, ~Z.92).
- `pub fn read_wallet_state(&self) -> Option<WalletState>` /
  `pub fn write_wallet_state(&self, st: &WalletState)` — atomisch via
  `write_atomic(&self.dir, "wallet.state", &frame, true)` (tmp + rename,
  Mode 0600), Framing/AAD wie chain.state, Versionsfeld
  `WALLET_STATE_VERSION: u32 = 1`; beschädigt/neuer ⇒ `None` + lauter
  Warn (der Caller behandelt „kein Share" = watch-only, §12 — NIE stumm
  Defaults erfinden).

```rust
#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct WalletState {
    pub version: u32,
    #[zeroize(skip)] pub address: String,
    pub threshold_keys: Vec<u8>,   // serialized ThresholdKeys (secret!)
    pub view_scalar: [u8; 32],     // shared private view key (secret!)
    #[zeroize(skip)] pub birthday_height: u64,
    #[zeroize(skip)] pub scan_height: u64,
    #[zeroize(skip)] pub outputs: Vec<PersistedOutput>,
}
```

Schreibpunkte: (a) einmal bei DKG-Abschluss (VOR dem Co-Sign, §7.5);
(b) gedrosselt beim Cursor-Fortschritt (alle 1000 gescannte Blöcke ODER
wenn neue Outputs gefunden wurden — read-modify-write); (c) beim
Clean-Close neben den `persist_crypto_blocking`-Callsites
(`net.rs:621/632`-Umgebung).

**Backup:** `wallet.state` in die Export-Include-Liste und die
Import-**Allowlist** aufnehmen — die Code-Stellen leben heute in
`crates/molt-storage/src/export.rs` und `import.rs` (per Grep nach
`"chain.state"` finden): überall, wo chain.state als „verbatim
ciphertext" exportiert/importiert/allowlisted wird, wallet.state daneben
stellen (portabel, weil der Sub-Key aus `workspace_key` abgeleitet ist). **Die Tabelle in
`docs_archive/storage/backup_restore_design.md` §3.2 mit aktualisieren** (Zeile
`wallet.state | yes, verbatim ciphertext | DKG-Share + View-Scalar —
ohne ihn ist das wiederhergestellte Mitglied watch-only`). NICHT in
`transport.state` legen — das ist vom Backup hart ausgeschlossen.

## 10. TDD-Fahrplan (rote Tests zuerst, in dieser Reihenfolge)

Jeder Punkt: Test schreiben → aus dem RICHTIGEN Grund rot sehen →
implementieren → grün → clippy 0 → Commit.

1. **`crates/molt-treasury/src/roster.rs` + tests** (in-Modul `#[cfg(test)]`):
   - `participant_mapping_is_deterministic_and_permutation_stable` —
     zwei Rosterreihenfolgen, gleiches Mapping; Indizes 1..=n; n=1 und
     n=max geprüft.
   - `participant_mapping_matches_chain_signer_order` — dieselbe
     Sortierung wie `try_commit`s m-niedrigst-benannte (Vergleichsfunktion
     exportieren statt duplizieren).
2. **`keys.rs` tests:**
   - `threshold_keys_roundtrip` — serialize → deserialize → gleiche
     Gruppen-Pubkeys/Adresse.
   - `view_commitment_is_stable` — Fixture: bekannter Scalar → bekanntes
     `view_key_commitment`-Hex (pinnt die Domain-Separation
     `"molt-wallet-view-v1\0"`).
3. **`dkg.rs` test:** `dkg_loopback_three_members_two_threshold` — drei
   `DkgRitual`-Instanzen in-memory durch beide Runden treiben; alle drei
   erhalten identischen Gruppenkey + identische Main-Address;
   anschließend `dkg_blame_attributes_corrupted_share` — ein manipulierter
   Runde-2-Payload attribuiert genau den Täter.
4. **molt-core:** `wallet_created_payload_byte_fixture` — festes Payload →
   festes `approval_bytes`-Hex (pinnt die Wire-Shape wie die
   Chat-Fixtures; ein Rotwerden ist ein Design-Stop).
5. **molt-storage:** `wallet_state_roundtrip`, `wallet_state_tamper_rejected`
   (Byte kippen → `None` + warn), `backup_includes_wallet_state`
   (Export→Import-Roundtrip trägt die Datei).
6. **Engine-E2E** (`crates/molt-engine/tests/wallet_dkg.rs`, Stil
   `founding_governs_over_the_direct_mesh`, Loopback, n=3/m=2; die
   Gründung muss das Feature „wallet" ratifizieren — Wizard-/Features-Pfad
   der Test-Harness):
   - `wallet_init_refused_without_feature` — Republik ohne „wallet" im
     Feature-Set: `WalletInit` → `FeatureDisabled`; nach einem
     `set_features`-Vote läuft es durch.
   - `wallet_init_seals_identical_block_on_all_nodes` — Init → 3 Approvals
     → Runden → alle sealen byte-identischen `wallet_created`-Block,
     gleiche Adresse in allen Snapshots, `wallet.state` auf allen Disks.
   - `wallet_init_times_out_and_can_be_reminted` — ein Knoten schweigt →
     `WalletDkgFailed` überall, kein `wallet.state`, zweites `WalletInit`
     läuft durch.
   - `wallet_init_is_one_shot` — zweites Init bei existierendem Wallet
     wird abgelehnt.
   - `wallet_decline_kills_ritual` — ein `Decline` beendet sauber.
7. **MCP:** der bestehende Co-Equality-Test wird durch 8.1 rot und durch
   Tool+INTERNAL wieder grün (das IST der Pinning-Test); dazu
   `wallet_init_tool_builds_command`.
8. **`#[ignore]`d Integration** (`crates/molt-engine/tests/wallet_scan.rs`):
   gegen echten regtest-monerod hinter env `MOLT_TEST_MONEROD`
   (Repo-Präzedenz `crates/molt-net/tests/nostr_relay_poc.rs`; Referenz-Image
   `ghcr.io/sethforprivacy/simple-monerod`): DKG über Loopback, Mining auf
   die Gruppenadresse, Scanner findet Outputs, Balance/History im
   Snapshot, Cursor überlebt Close/Reopen.

## 11. UI (Etappe 1)

Iteration über `scripts/dev-ui.sh`; finale Validierung einmal:
`cargo build -p molt-ui-window -p molt-ui`.

1. **Nav: nichts freischalten — sie ist schon feature-getrieben.** Das
   hartkodierte Gate von 2026-07 existiert nicht mehr: molt-ui filtert die
   Surface-Liste auf das effektive Feature-Set (`lib.rs:3991`), und ein
   aktiviertes-aber-ungebautes Surface öffnet die als Design-Mock
   gebadgte Pane. Reale Aufgabe hier: wenn die WalletPane echt wird, ihr
   Mock-Badging entfernen (Vorbild: wie das Memory-Surface real wurde).
2. **`WalletPane` echt machen** (`surfaces.slint:2248`): die Pane ist
   inzwischen ein VOLLSTÄNDIGER Design-Mock aller sechs Views (Balance,
   History, Send-Formular, Receive mit Pseudo-QR, Status, Settings) —
   die Aufgabe ist also „echte Daten in die bestehende Struktur
   verdrahten und die Sample-Properties (`transfers`, `pending-tx`, …)
   entfernen", nicht Pane-Design. Für die Etappe-1-Views gilt:
   - `balance`-View: Balance-Karte (bestätigt + pending, XMR-formatiert
     aus Piconero: 12 Nachkommastellen, führende Nullen trimmen),
     Scan-Fortschritt (scan_height/daemon_height), Verbindungs-Indikator.
   - `history`-View: Liste (Betrag, Kurz-Hash, Höhe, Confirmations-Badge).
   - `receive`-View: Gruppenadresse als kopierbare Monospace-Box
     (bestehende Copy-Muster der App nutzen).
   - `status`-View: DKG-Phase, m/n, Mitgliederliste mit
     share-held/watch-only, bei `DkgPhase::None` der
     „Kasse einrichten"-Button (→ `WalletInit`), bei `Proposed` der
     Abstimmungs-Hinweis (Voting läuft über die bestehende Pending-UI),
     bei `Failed` Grund + blamed + Re-Init-Button.
   - `send`/`settings`-Views: ehrlicher Stub-Text „Kommt in Etappe 2 —
     Ausgaben laufen dann als Threshold-Vorschlag" (KEINE toten
     Eingabefelder, kein Fake-Formular — CLAUDE.md-Regel).
   - Neue Structs (`WalletRow` etc.) nach `theme.slint` neben
     `ChainRow`/`ViewItem`; Strings in die `Strings`-Tabelle (de/en);
     Piktogramme als Twemoji.
3. **molt-ui-Logik** (`crates/molt-ui/src/lib.rs`): in `apply_surfaces`
   (~Z.4294) den `SurfaceSnapshot.wallet` in die Slint-Properties
   mappen; `WalletInit`-Button über `issue(rt, wallet, weak,
   Command::WalletInit)` (~Z.2553); auf `Event::WalletUpdated`/
   `WalletDkgProgress`/`WalletReady`/`WalletDkgFailed` einen
   Snapshot-Refresh triggern (wie bestehende Event-Behandlung); den toten
   „transfer"-Mock in `default_op` (~Z.5942) entfernen.

## 12. Recovery- und Fehler-Semantik (per Default entschieden, Veto möglich)

- **Recovery MIT Backup:** `wallet.state` reist als verbatim ciphertext im
  Export mit → Share + Scalar + Cursor kommen wieder. Voll funktionsfähig.
- **Recovery OHNE Backup (nur Phrase):** `dkg 0.6` kennt kein Resharing →
  der Share ist unwiederbringlich. Das wiederhergestellte Mitglied wird
  **watch-only**: es erhält den View-Scalar von einem Peer über den
  MLS-Recovery-Kanal (neben dem bestehenden Chain-Serve beim Recovery),
  verifiziert ihn gegen das On-Chain-`view_key_commitment` und kann
  Balance/History sehen und abstimmen — aber nicht mit-signieren. Die
  Republik bleibt spend-fähig, solange ≥ m lebende Shares existieren.
  Status-View zeigt das pro Mitglied (`shareholders`). Das Design-Doc
  dokumentiert diese Grenze prominent. (Wie ein Peer erfährt, ob ein
  Mitglied seinen Share noch hat: `NetWalletStatus`-analoges Flag im
  bestehenden Presence-/`MemberSeen`-Pfad — kleinster Mechanismus, der es
  ehrlich abbildet; bei Unklarheit „unknown" anzeigen, nicht raten.)
- **DKG-Gossip im Log:** die Runden-Events reiten wie ALLER Wire-Verkehr
  über den Workspace-Log (konsistent zur heutigen Architektur; ein
  log-freier Kanal ist bekanntes Future-Work der Chain-Doku). Die
  Payloads enthalten nur protokoll-verschlüsselte bzw. öffentliche
  DKG-Daten — niemals rohe Shares.
- **Regtest-Harness:** manueller monerod hinter `MOLT_TEST_MONEROD`
  (Repo-Präzedenz) statt testcontainers-Abhängigkeit.

## 13. Etappe 2 (Skizze — NICHT in Etappe 1 bauen)

**Vorab-Entscheidung am Etappe-2-Start (FCMP++-Horizont, §2.1):** Ist der
FCMP++-Mainnet-Fork aktiviert oder terminiert, wird der Spend-Flow gegen
das FCMP++-GSP-Multisig (2-Runden, FROST-inspiriert, in monero-oxides
`fcmp++`-Branch entstehend) gebaut statt gegen FROSTLASS/CLSAG — CLSAG-
Transaktionen sind nach dem Fork nicht mehr gültig. Der DKG-Gruppenkey
und die Shares aus Etappe 1 bleiben dieselben; es ändert sich der
Signier-Algorithmus und dessen Audit-Lage (im Design-Pass prüfen). Die
Skizze unten beschreibt den CLSAG-Pfad; Struktur (Propose = exakte
Tx-Bytes, m-of-n-Ratifizierung, deterministische Signierer, Runden über
Events) gilt für beide.

- `Propose { surface: Wallet, payload: {"op":"transfer","dest":…,
  "amount":…} }`: der Proposer baut die `SignableTransaction` off-actor
  (Decoys/Fee vom Daemon) und legt die **exakten serialisierten Bytes**
  mit in den Payload; jede Engine parst dest/amount AUS DIESEN BYTES nach
  und zeigt sie an (sign-what-you-see) — das m-of-n-Approval ratifiziert
  die byte-identische Transaktion.
- Nach dem Sealing führen die **m niedrigst-benannten online
  Share-Halter** (bestehende Determinismus-Regel) die zwei FROST-Runden
  über neue Events `WalletSignPreprocess`/`WalletSignShare` aus: eine
  Maschine pro Input, `msg` leer, **frisches Preprocess pro Versuch**
  (Caching für die Tx-Maschine ist upstream verboten), ungültige Shares →
  Blame-Event. `.complete()` → Daemon-Broadcast → Folge-Block trägt die
  txid; Outputs lokal als verbraucht markiert.
- Decoy-/Fee-Staleness (Tx zu alt geworden) scheitert ehrlich →
  Rebuild + Re-Propose. Settings-View bekommt Daemon-URL-Editing
  (bidirektionales Config-Muster existiert).
- Send-UI übernimmt die eigenwallet-Checkliste: Betrag mit
  Balance-Validierung, Bestätigungsscreen mit Betrag/Fee/Ziel,
  Fortschritt der Signier-Runden, Erfolgsscreen mit txid.

## 14. Schritt 1 vor allem Code: Design-Doc `docs/chain/wallet_treasury_design.md`

Sicherheitskritischer Flow ⇒ erst Design-Doc, erst diskutieren
(CLAUDE.md + Concept-Doc-Regel). Gliederung:

1. Prinzip „ein Threshold" — die Kasse spendet mit demselben m-of-n, mit
   dem die Republik regiert; alle n halten Shares.
2. Akteure & Schlüssel: Roster-Ed25519-Identität ≠ DKG-Gruppenkey ≠
   View-Scalar; Participant-Mapping.
3. Das Ritual Phase für Phase (Diagramm im Stil founding_ritual.md:
   Absicht → n-of-n-Konsens → Runde 1 → Runde 2 → lokaler Abschluss →
   Persist → Auto-Co-Sign → Block bei m).
4. Chain vs. Mitglieds-Geheimnis: Block-Payload-Spec; Scalar nur über
   MLS; Commitment-Verifikation.
5. Persistenz & Recovery: `wallet.state`, Backup-Inklusion,
   Watch-only-Grenze (§12).
6. Scanning: GuaranteedViewPair, nur Main-Address, Burning-Bug-Begründung,
   Cursor-Semantik, Reorg-Verhalten (Etappe 1: Scan ist idempotent ab
   Cursor; bei Verdacht Rescan ab birthday).
7. Etappe-2-Spec (aus §13 ausgearbeitet).
8. **Tragende Invarianten** (Stil founding_ritual.md — nicht schwächen):
   sign-what-your-own-DKG-computed (nie ein fremdes Ergebnis
   übernehmen); one-shot Init (Re-Run = Cancel + Re-Mint); ephemer bis
   zum Block; alle n nehmen am DKG teil; View-Scalar/Shares nie on-chain,
   Shares verlassen `wallet.state` nie; FROST-`msg` leer; kein
   Preprocess-Caching; Gruppenkey nie aus Identitäts-Keys abgeleitet.
9. Fehler & Abort: Timeout, Blame, Decline, Offline-Mitglieder,
   Crash-Punkte (vor/nach Persist).
10. Dependency-Pinning: rev, Prüfprotokoll (§6), Upgrade-Prozedur,
    Semver-Vorbehalt des multisig-Features.
11. Implementierungs-Landkarte (Verweis auf dieses Dokument).

## 15. Ausführungsreihenfolge (Checkliste)

1. [x] Design-Doc §14 geschrieben und diskutiert — **RATIFIZIERT
   2026-08-16** (inkl. der Revisionen dieses Datums).
2. [ ] Dep-Lock-Spike §6 (Commit: Workspace-Deps + bare molt-treasury).
   **← HIER geht es weiter.**
3. [ ] §10.1–10.3: molt-treasury roster/keys/dkg (Commit je grünem Block).
4. [ ] §10.4–10.5: core-Fixture + molt-storage `wallet.state` +
   Backup-Include + Doc-Tabelle.
5. [ ] §8: Kontrakt in molt-core; MCP-Tool + INTERNAL (Co-Equality grün).
6. [ ] §10.6: E2E rot → Engine-Verdrahtung §7 komplett (Init, Konsens-
   Guard, Runden-Arme, Timeout, Terminal-Block, `[wallet]`-Config).
7. [ ] §8.6: Scanner + Projektion + Snapshot-Füllung; §10.8-Test.
8. [ ] §11: UI. Validierung `cargo build -p molt-ui-window -p molt-ui`.
9. [ ] `cargo clippy --all-targets` = 0; volle Testsuite grün;
   `#[ignore]`d-Tests einmal manuell gegen regtest-monerod.
10. [ ] Code-Review über den Gesamt-Diff (/code-review), Findings fixen,
    grün auf master landen. `docs_archive/ui/mock_todo.md` Punkt 14:
    Wallet-Zeile auf „Etappe 1 done" aktualisieren.

## 16. Bekannte Fallen (für die Implementierung)

- **Zeilennummern driften** — Symbole greppen.
- **Nicht zwei window-scale Builds parallel** (OOM-Kill = SIGKILL beim
  molt-ui-window-rustc); RAM knapp → `-j 1`.
- Geteiltes `CARGO_TARGET_DIR` über Worktrees aliast Artefakte — bei
  Geister-Compile-Fehlern `cargo clean -p molt-*`.
- Der Engine-Aktor stoppt, wenn der letzte starke `cmd_tx` fällt
  (`WeakSender`) — Scanner-/Deadline-Tasks halten NUR Weak-Handles
  (Ticker-Muster kopieren), sonst lebt der Aktor ewig.
- `roster_canonical_bytes`/`approval_bytes` NICHT anfassen — der
  Terminal-Block ist ein normales `Applied`; kein neues Byte-Layout,
  keine Tag-Bumps nötig.
- Slint: `alignment: start` + x/y-verankerte Kinder ⇒ 0-Breite-Falle
  (siehe Memory „ProposalCard-Bild-Bug") — Panes wie die
  Organization-Vorbilder layouten.
- Beim `wallet_init`-Konsens-Guard aufpassen, dass der LEGACY-Pfad
  (nicht-chain-governed Workspaces) `WalletInit` sauber ablehnt statt im
  gezählten Simulations-Pfad zu landen (`is_chain_governed`-Guard).
- Hex-Payloads (`payload_hex`, `shares_hex`) haben Größenordnungen von
  wenigen KiB — der Nostr-Transport chunkt große Frames ohnehin
  (`CHUNK_PAYLOAD_BUDGET`, `crates/molt-net/src/chunk.rs`), also im
  normalen Log-Outbox-Pfad bleiben (kein Sonderweg nötig).

## 17. Verifikation (Definition of Done, Etappe 1)

- Alle Tests aus §10 grün; die zwei `#[ignore]`d-Tests einmal manuell
  gegen einen regtest-monerod gelaufen (Protokoll im PR-/Commit-Text).
- `cargo clippy --all-targets` = 0; `cargo build -p molt-ui-window -p
  molt-ui` sauber.
- Manuelle Probe über zwei Instanzen (Loopback-Demo-Peers): Kasse
  einrichten, Abstimmung in der UI, DKG läuft durch, beide zeigen
  dieselbe Adresse; regtest-Mining auf die Adresse erscheint in Balance +
  History; Close/Reopen behält Cursor und Balance.
- Code-Review über den Gesamt-Diff gelaufen, Findings gefixt, Endzustand
  grün auf master.
