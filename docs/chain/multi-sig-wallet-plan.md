# Multisig-Wallet-Surface: Implementierungsplan Etappe 1

Stand: **Revision 3, 2026-10-06**, Anker gegen master `0f0cb30d` geprüft.
Ersetzt Revision 2 (gleicher Tag); §17 listet die Änderungen.
Design-Autorität: `docs/chain/wallet_treasury_design.md` (rev 3). Dieses
Dokument ist der Bauplan: Dateien, Symbole, Tests, Reihenfolge.

**Scope: nur Etappe 1** — Kasse einrichten, empfangen, beobachten.
Ausgeben (Etappe 2) wartet auf SA+L-Threshold-Signing in monero-wallet
(W2, Design §8). Kein Code für Etappe 2 anlegen.

**Für die Implementierung gilt zwingend:**
- Zeilennummern driften — **immer per Symbolname greppen.**
- Keine Upstream-API erfinden. Der Dep-Spike (§6) hat die Kern-APIs gegen
  den Compiler gelockt; alles Weitere vor Gebrauch in
  `~/.cargo/registry/src/*/<crate>-<ver>/` nachlesen.
- Repo-Regeln aus `CLAUDE.md` gelten vollständig (§3).

---

## 1. Mission und Entscheidungen

Eine Republik erhält eine gemeinsame Monero-Kasse. Alle n Mitglieder halten
einen Schlüsselteil aus einem DKG; ausgegeben wird später mit derselben
Schwelle `rule_m`, mit der die Republik regiert.

| | Entscheidung (User, 2026-10-06) |
|---|---|
| W1 | Kasse nur bei `2 ≤ m ≤ n − 1` |
| W2 | Etappe 2 wartet auf SA+L; kein CLSAG-Spend |
| W3 | Nur Monero |
| W4 | Nur ein ausdrücklicher Wallet-Akt (`wallet_init`-Vote) erzeugt eine Kasse; kein Marker, `set_features` kann `wallet` nicht hinzufügen |
| W5 | Schlüsseldatei defekt: Export bricht ab; Import ohne sie, Sitz watch-only |
| W6 | Ein Decline beendet den Lauf |
| W7 | DKG-Teilnehmer = Position in der Genesis-Gründungstabelle (Vault-Ordnung) |
| W8 | `wallet_created` trägt n Identitäts-Attestationen; die Projektion prüft sie aus dem Payload (übersteht jeden Schnitt) |
| W9 | Kasse standardmäßig an; die Kassen-Stufe mit Fortschrittsbalken ist der automatische letzte Wizard-Schritt |
| W10 | Späteres Einschalten zeigt dieselbe Stufe; alle n gleichzeitig online |
| W11 | Mainnet erlaubt; neben der Adresse groß: „Spending does not work yet. Money sent here is gone.“ |

Weiter gültig: Republik = Wallet, ein Threshold-Begriff, nur XMR,
Charter-Feature, nie abschaltbar.

## 2. Der Stack (per Spike verifiziert 2026-10-06)

Zwei Spikes (nicht im Repo): `treasury-spike` (DKG, Adresse, View-Key,
FROST-Signatur) und `stage1-min` (Etappe 1 ohne `multisig` und ohne
`modular-frost`: DKG-Test grün, kein `ring`, kein C). Nachgeprüft am
selben Tag gegen die Crate-Quellen.

**Etappe 1 braucht:**

| Krate | Version | Rolle |
|---|---|---|
| `dkg` | 0.6.1 | `ThresholdParams`, `ThresholdKeys`, `Participant` |
| `dkg-pedpop` | **=0.6.0** (Pin) | DKG, 2 Runden, `BlameMachine`; **auf Serais `next` gelöscht**; reviewed, not vendored (Design §12) |
| `dalek-ff-group` | 0.5 | `Ed25519`-Ciphersuite |
| `ciphersuite` | 0.4 | `Ciphersuite`-Trait |
| `monero-wallet` | 0.2.0, **ohne** `multisig` | `ViewPair`, `Scanner`, Adressen |
| `monero-daemon-rpc` | 0.2.0 | RPC-Client über eigenen `HttpTransport` |
| `schnorr-signatures` | **=0.5.2** (Pin) | 0.5.3 weitet die `multiexp`-Spanne → `multiexp` 0.4 + 0.5 nebeneinander → `dkg-pedpop` bricht (zwei `BatchVerifier`) |

`modular-frost` und das `multisig`-Feature (außerhalb SemVer) kommen erst
mit Etappe 2.

Verifizierte API-Fakten:
- DKG: `KeyGenMachine::new(params, context: [u8; 32])` →
  `generate_coefficients(rng)` → `generate_secret_shares(rng, map)` →
  `calculate_share(rng, map)` → `BlameMachine::complete()` / `.blame(..)`.
- Alle n müssen teilnehmen (`validate_map`). pedpop erkennt **keine**
  Equivocation — „responsibility lies with the caller“; Abschluss mit
  allen bestätigen (Doku). → Transkript-Hash (§7.4).
- Kontext soll pro Multisig eindeutig sein. → Run-Nonce im Kontext.
- `*::read` brauchen die eigenen `ThresholdParams`.
- Zwischenstände sind nicht serialisierbar.
- `ciphersuite::read_G` prüft nur Gültigkeit/Kanonizität, **nicht** den
  Identitätspunkt → eigene Prüfung, für jeden Punkt beider Frames
  (Design §12).
- **Kein View-Key aus dem DKG.** Adresse = `ViewPair::new(spend, view)
  .legacy_address(network)`; `ViewPair::new` nimmt monero-oxides eigenen
  `ed25519::Point` → Konvertierung aus dalek-ff-group nötig; lehnt Torsion ab.
- **Nicht `GuaranteedViewPair`/`GuaranteedScanner`.** Standard-`Scanner`;
  gesehene Output-Keys MÜSSEN geprüft und gespeichert werden.
- `Scanner::scan` lehnt Blöcke mit `hardfork_version > 16` ab
  (`UnsupportedProtocol`); unbekannte Output-Typen scheitern beim Dekodieren.
- `monero-daemon-rpc`: `trait HttpTransport { post(route, body,
  response_size_limit) }`; Auth liegt beim Implementierer.
- Größen (n=13): Runde 1 ≈ m·32 + 96 B, Runde 2 ≈ 128 B je Empfänger,
  `wallet_created` ≈ n·64 B Signaturen + Felder.
- `monero-simple-request-rpc`: zieht `ring` + `cc`. **Nicht verwenden.**

## 3. Repo-Regeln, die hier besonders greifen

1. **TDD.** Jeder Schritt beginnt mit roten Tests (§10).
2. **clippy = 0, auch Tests, pro Crate.** `.expect("…")`, nie `.unwrap()`.
3. **Co-Equality.** Neue `Command`-Varianten: MCP-Tool (mit `scope`) oder
   `INTERNAL` (Liste in der Testfunktion
   `co_equality_every_command_is_a_tool_or_documented_internal`).
4. **Events additiv**; keine neue Regel in der Chain-Verifikation (§7.7).
5. **Kein I/O in molt-core.** Handler synchron; Blockierendes off-actor.
6. **GUI:** `scripts/dev-ui.sh build` + Live-Preview-Tests. Kein voller
   Fenster-Build pro Change-Set. Nie `DISPLAY=:0`. Kein Em-Dash in UI-Text.
7. **Direkt auf master;** Review über den Diff vor dem Landen.
8. **Secrets nie in getrackten Artefakten benennen.**
9. **Status-Zeilen pflegen;** nach Doc-Moves `scripts/check-doc-refs.py`.
10. **Gründungsritual unangetastet** (I15): kein Byte, kein Schritt von
    `founding.rs`/`nostr_ritual.rs` ändert sich; die Kassen-Stufe beginnt
    nach dem Siegel.

## 4. Anker im Repo

| Anker | Ort | Rolle |
|---|---|---|
| `Surface::Wallet`, Views | `molt-core/src/lib.rs` (`Surface::views`) | bleibt; `send` bleibt Stub |
| `Surface::is_implemented` | `molt-core/src/lib.rs` (Test ~Z. 8380) | Wallet aufnehmen |
| `effective_features` / `feature_on` / `require_feature` | `molt-engine/src/chain/projection.rs` | `wallet` effektiv auch durch angewandten `wallet_init` |
| `propose_payload`, Enable-only-Gate („already enabled“, „cannot be disabled“) | `molt-engine/src/proposals.rs` | `set_features` darf `wallet` nicht neu hinzufügen; `wallet_init` ohne Feature erlaubt |
| `cmd_propose` / `cmd_approve` (ruft `require_feature`) | `proposals.rs` | Wallet-Arm, Approve-Checks; `wallet_init` vom Feature-Gate ausnehmen |
| `register_decline` (`veto_room`) | `proposals.rs` | `veto_room = 0` für `wallet_init` |
| `chain_sign_and_gossip_approval` | `chain/governance.rs` | dieselben Checks auch hier |
| `try_commit` | `chain/governance.rs` | **unverändert** (Siegel bei m) |
| `proposal_change` / `id_free_for` | `chain/governance.rs` | eigene ID für `wallet_created` |
| `rebase_pending_approvals`, `forget_votes_for` | `chain/governance.rs`, `events.rs` | Re-Sign; Geschwister-Karten |
| `own_approvals` (Feld des Chain-Zustands) | `molt-engine/src/lib.rs` | „eigene Zustimmung steht“ |
| `after_block_applied` | `molt-engine/src/chain/` | **ein** Hook für den Auto-Start; Pfade ohne ihn (Checkpoint-Kandidat `checkpoint.rs`, Recovery-Adoption) starten nichts |
| Create/Join-Abschluss (nach `materialize_workspace` in `lifecycles.rs`, `chain_blocks` = `None`) | `molt-engine/src/lifecycles.rs` | Auto-Init nur hier; nie Recovery, nie Reopen; Ritual-Code selbst unverändert |
| `own_signature_stands`, Auto-Co-Sign des Checkpoints | `chain/governance.rs`, `chain/checkpoint.rs` | Vorbild Auto-Co-Sign |
| `vault_arm`, `vault_approve_check` | `molt-engine/src/vault/mod.rs` | Vorbild geschlossener Op-Satz + Prüfung in jedem Signierpfad |
| `supersede_stale_vault_cards` | `vault/deposit.rs` | Vorbild Geschwister-Karten schließen |
| `State::vault_founding_table()` | `molt-engine/src/vault/fold.rs` | **die** Sitzordnung (Genesis/Anker, jeder Roster); Wallet nutzt sie direkt |
| `bounds_ok` | `vault/mod.rs` | Vorbild W1 |
| `CheckpointState.rule_m` / `.rule_n` / `.founding_identities` | `molt-core/src/chain.rs` | Quelle für W1 und die Attestations-Prüfung; **nicht** `State::threshold()` (Config-Fallback) |
| `net_scope_current`, `reset_workspace_state` | `net/mod.rs`, `events.rs` | Lebensdauer Scanner/Lauf |
| `cmd_net_presence_tick`, `vault::receipts::tick` | `net/presence.rs`, `vault/receipts.rs` | Resend, Deadlines |
| `CONTROL_FRAMES`, `ControlParser` | `molt-net/src/supervisor.rs` | neue Tags; Frames sind nur MLS-authentifiziert |
| `vault_frames.rs`, `mirror_gossip.rs` | `molt-net/src/` | Vorbild Frames |
| `GroupHandle::publish_control` | `molt-net/src/group_runtime.rs` | best-effort (Queue 64), daher Resend |
| `TransportState` (`mirror`, `vault_status`) | `molt-core/src/lib.rs` | Shareholder-Status, Laufzustand |
| Segmente `−1 … −8` (`−7`/`−8` Kanban), `RESERVED_SEGMENT_FLOOR` (per Test gepinnt) | `molt-storage/src/lib.rs` | neu `−9` Schlüssel, `−10` Scan; Floor + Test mitziehen |
| `chain_state_key`/`hkdf32`, `encode_frame`, `write_atomic` | `molt-storage/src/lib.rs` | Muster für beide Dateien |
| `persist_chain_blocking`, `WriterMsg` | `molt-storage/src/lib.rs` | blockierender Writer |
| Export-Funktion mit Closure `checked_kind` | `molt-storage/src/export.rs` | Export-Arme |
| `allowed_entry`, Verify-Loop, `staging.dropped` | `molt-storage/src/import.rs` | Import-Arme |
| Backup-Ticker | `molt-engine/src/backup.rs` | Backoff bei Schlüssel-Fehler |
| `tools()`, `Scope` | `molt-mcp/src/lib.rs` | neue Tools (Seat) |
| `VaultView` / `SurfaceSnapshot.vault` | `molt-core/src/vault.rs` | Vorbild `WalletView` / `.wallet` |
| `classified` / `relay_kind` / `RelayKind` | `molt-core/src/relay.rs` | Host-Regel extrahieren; nimmt heute nur `ws`/`wss` |
| `relay_block` / `clearnet_enabled` | `relay.rs`, `molt-config/src/lib.rs` | dasselbe Nicht-Onion-Gate für den Daemon |
| `s3::http` (freie Funktionen über einen Stream; `MAX_RESPONSE` Modulkonstante, `S3Error`) | `molt-net/src/s3/http.rs` | Grenze als Parameter, Fehlertyp generisch — Refactor |
| `Dialer` (Enum) | `molt-net/src/dial.rs` | Transport |
| `Config` (`deny_unknown_fields`, `render`, `salvage`) | `molt-config/src/` | Sektion `[wallet]` |
| Wizard Feature-Schritt: `cw-feat-*`, Wallet `checked: false; enabled: false` | `molt-ui-window/ui/app.slint` (~Z. 3966) | Default an, Sperre nur bei W1 |
| `SealedPanel` (Gründer `cw-outcome == 1`, Joiner `jw-sealed`) | `app.slint`, `parts.slint` | danach automatisch die Kassen-Stufe |
| Organization-Modal `org-feat-wallet` (gesperrt; Ride-along `" wallet"`) | `app.slint` (~Z. 6353, 9805) | Checkbox → `wallet_init` statt `set_features` |
| `WalletPane` (Mock) | `molt-ui-window/ui/surfaces.slint`; Route `app.slint` | wird echt |
| `default_op` (`"transfer"`), `wl_*` | `molt-ui/src/labels.rs`, `i18n.rs` | aufräumen |
| E2E-Harness | `molt-engine/tests/vault_support/mod.rs` | Vorlage |
| **Namensfalle** `WalletHandle` | `molt-engine/src/lib.rs` | Engine-Aktor-Handle, nicht die Kasse |

## 5. Architektur

### 5.1 Crate `crates/molt-treasury`

Vorbild `molt-vault`: reine Funktionen, kein I/O, hängt nur an `molt-core`
und den Krates aus §2. Workspace-Header ergänzen.

```
crates/molt-treasury/src/
  lib.rs      // Fassade, TreasuryError
  dkg.rs      // Runden: Kontext (mit Run-Nonce), Identity-Check, Transkript
  attest.rs   // Attestations-Bytes, Signieren, Prüfen aller n
  keys.rs     // View-Key aus Beiträgen, Standardadresse, (De)Serialisierung,
              // zeroize; Byte-Layouts mit Tags + Byte-Pin-Tests
  scan.rs     // Scanner-Kern hinter eigenem Trait: ViewPair, gesehene Keys,
              // Block-Hashes, UnsupportedProtocol -> Pause
```

- **Daemon-URL:** in `molt-core/src/relay.rs` die Host-Klassifizierung aus
  `classified` als gemeinsame Funktion herauslösen (WHATWG-Parse, ASCII,
  kein Backslash, kein Userinfo, kanonische Schreibweise); Eintritt
  `daemon_kind(url)` für `http`/`https`, `relay_kind` unverändert für
  `ws`/`wss`. Kein zweiter Parser.
- **RPC-Transport** in molt-net (`monero_rpc.rs`): `HttpTransport` über die
  `s3::http`-Funktionen und den `Dialer`. Onion → Tor; Local/Clearnet →
  nur bei `clearnet_enabled` und bestätigtem Daemon (wie ein Relay),
  sonst fail-closed mit Grund. Login: `http-auth` (digest-scheme), Urteil
  in §6. Antwortgrenze pro Aufruf (`get_blocks.bin` groß).
- **Sitzordnung:** `State::vault_founding_table()` — kein neuer Helfer.

### 5.2 Kontrakt in molt-core (`molt-core/src/wallet.rs`, neu)

```rust
pub struct WalletView {
    pub address: String,             // "" bis zum Terminal-Block
    pub network: String,             // mainnet | stagenet | testnet
    pub balance: u64,                // >= 20 Bestätigungen, Piconero
    pub pending: u64,
    pub scan_height: u64,
    pub daemon_height: u64,
    pub connected: bool,
    pub scan_paused: Option<String>, // "update needed" (Fork) o. ä.
    pub threshold: u32,
    pub participants: u32,
    pub phase: WalletPhase,          // Off | Bounds | NoPurse | Init | Ready
    pub run: Option<WalletRunView>,  // Fortschritt (§7.5)
    pub shareholders: Vec<(String, ShareStatus)>, // Held | WatchOnly | Unknown
    pub history: Vec<WalletTxView>,
    pub can_spend: bool,             // Etappe 1: immer false
}

pub struct WalletRunView {
    pub stage: RunStage,             // Ready | Round1 | Round2 | Attest | Sealing | Done | Aborted
    pub done: u32,                   // k von n
    pub of: u32,
    pub missing: Vec<String>,        // wer fehlt (Readiness)
    pub reason: Option<String>,      // Abbruchgrund, ein Wort/eine Zeile
    pub needs_consent: bool,         // dieser Sitz muss noch zustimmen
}
```

## 6. Dependency-Lock (Schritt 2)

```toml
dkg = { version = "0.6", default-features = false, features = ["std"] }
dkg-pedpop = "=0.6.0"
dalek-ff-group = { version = "0.5", default-features = false, features = ["std"] }
ciphersuite = { version = "0.4", default-features = false, features = ["std"] }
monero-wallet = { version = "0.2", default-features = false, features = ["std", "compile-time-generators"] }
monero-daemon-rpc = { version = "0.2", default-features = false, features = ["std"] }
schnorr-signatures = { version = "=0.5.2", default-features = false }
```

1. In `[workspace.dependencies]`, Minor exakt halten. Den Pin mit einer
   Zeile begründen (multiexp-Spaltung), damit ihn niemand „aufräumt“.
2. Bare `molt-treasury` mit den Spike-Funktionen. Compilieren = gelockt.
3. Prüfprotokoll: `cargo tree -d`, `-i ring`, `-i cc` leer, keine `*-sys`.
4. **`dkg-pedpop` reviewen** (Transcript, Kontext, Blame) und entscheiden:
   crates.io-Pin oder vendoren (es ist auf Serais `next` gelöscht,
   `dkg-evrf` unveröffentlicht). Urteil ins Design §12.
5. Digest-Auth-Crate suchen und bewerten (§5.1).
6. QR-Crate suchen und bewerten (§11).

**Done 2026-10-09** (verdicts with their arguments in Design §12):

- Locked in the workspace manifest; `molt-treasury` carries the spike
  functions (`dkg::{params, round1, round2, complete}`,
  `keys::standard_address`) and `tests/stage1_lock.rs` (3-seat DKG agrees,
  `ThresholdKeys` round-trip, standard main address; a round-1 frame under
  another context is refused; the `HttpTransport` shape compiles).
  `monero-daemon-rpc` is a dev-dependency there until molt-net's transport
  (§5.1) takes it; `molt-core` joins when first used (step 3).
- Audit: `cargo tree -p molt-treasury -d` shows only `thiserror` 1 + 2;
  `-i ring` and `-i cc` match nothing; no `*-sys`.
  `tests/graph_guard.rs` keeps that true and fails on a second `multiexp`
  or a moved `schnorr-signatures`. The pin only bites on a fresh resolve:
  an existing lock keeps `multiexp` 0.4 even at 0.5.3, the guard then
  goes red on the version.
- `dkg-pedpop`: crates.io pin `=0.6.0`, not vendored. Step 3 rejects the
  identity in every point of both frames, not only the commitments, and
  never calls `blame` with wire indexes.
- Digest auth: `http-auth` 0.1 with `default-features = false,
  features = ["digest-scheme"]`. Left: `digest_auth`.
- QR: `qrcode` 0.14 with `default-features = false` (no deps), module grid
  via `to_colors()`/`width()`. Left: `qrcodegen`, `fast_qr`.

## 7. Engine

### 7.1 Die Tür (W1, W4)

- `Command::WalletInit` (Tool, Seat): Gates W1 (aus Genesis/Anker),
  chain-governed, kein angewandter oder offener `wallet_init`;
  Daemon-Probe off-actor; Proposal `{op: wallet_init, birthday_height,
  network}` auf `Surface::Wallet`, Birthday = Daemon-Höhe − Marge.
  `require_feature(Wallet)` gilt für diese Op nicht — bei Propose,
  `cmd_approve` **und** im Re-Sign-Pfad.
- Navigation: Wallet sichtbar, sobald ein Init offen oder angewandt ist.
- `effective_features` enthält `wallet`, sobald ein `wallet_init`
  angewandt ist (aus der Wallet-Liste des Fold, schnittfest).
- `set_features`: `wallet` nur als Ride-along eines schon effektiven
  Features; Neu-Hinzufügen → `BadPayload("wallet: set up the purse")`.
- Projektion: erster `wallet_init` zählt, jeder weitere wird ignoriert.

### 7.2 Init-Vote und Zustimmung (W6)

- **Approve-Checks** in `cmd_approve` **und**
  `chain_sign_and_gossip_approval` (Vorbild `vault_approve_check`):
  geschlossener Op-Satz; Birthday ≤ eigene Daemon-Höhe + Schlupf und
  nicht mehr als ein festes Fenster darunter; Netz = eigenes; Gates aus
  §7.1. **Ohne Daemon: Enthaltung** (weder Approve noch Decline);
  Auto-Approve sobald einer gesetzt ist.
- **Decline tötet die Karte** dort, wo er ankommt: `veto_room = 0` für
  `wallet_init`. Vorher gesammelte m Approvals siegeln trotzdem — die
  Garantie von W6 ist die Zustimmung im Lauf.
- Siegel bei **m** (normaler `try_commit`).
- Zustimmung eines Sitzes = eigene Approve-Signatur, solange das eigene
  Log sie trägt (`own_approvals` wird aus dem Log gebaut), sonst
  `WalletConsent` im Lauf.

### 7.3 Auto-Init und Wizard (W9)

- Nach dem Create/Join-Abschluss (nie Recovery, nie Reopen): enthält die
  Charter `wallet` und hält W1, schlägt die niedrigste Gründungsposition
  **mit Daemon** den Init vor; die nächste nach einer Verzögerung, falls
  kein Init sichtbar.
- Ein Sitz, der `wallet` **in dieser Gründungssitzung** ratifiziert hat
  (Ritual-Zustand, nur im Speicher), approved automatisch nach dem
  Birthday-Check.
- Der Wizard (Gründer und Joiner) geht nach dem Siegel automatisch in die
  Kassen-Stufe: zuerst Daemon (falls keiner), dann `WalletView.run`.
  „Enter republic“ ist ab Beginn der Stufe da; Verlassen schiebt den
  Balken ins Panel. Ohne `wallet` in der Charter endet der Wizard am
  Siegel wie heute.

### 7.4 Lauf und Runden (Control-Frames)

Tags in `CONTROL_FRAMES`, je mit `init_id` und Run-Nonce `r`; Resend am
Presence-Tick bis Commit oder Abbruch; nie im Workspace-Log:

| Tag | Inhalt |
|---|---|
| `\x00molt-wstart-v1` | Start eines Laufs: `init_id`, `r`, Starter-Position |
| `\x00molt-wdhint-v1` | Daemon-URL als Vorschlag, solange eine Kassen-Stufe offen ist |
| `\x00molt-wrdy-v1` | Build + Daemon + Zustimmung, oder Decline |
| `\x00molt-wr1-v1` | Commitments + PoP, View-Beitrag `c_i` |
| `\x00molt-wr2-v1` | Shares (je Empfänger) + Transkript-Hash `T` |
| `\x00molt-watt-v1` | Attestation `A_i` |
| `\x00molt-wabrt-v1` | Abbruch mit Grund (kein Name) |

- **Starter:** Gründungsposition 1, Fallback nach Position; startet
  automatisch **einmal pro Init**, für den ersten Init der Projektion, im
  Hook `after_block_applied` mit einer In-Memory-Menge gestarteter Inits
  (nie beim Restore beim Öffnen); danach nur per `WalletRetry` (Tool,
  Seat).
- **Rennen:** bekannte Nonce → ablehnen; zwei Starts in der Readiness →
  jeder nimmt den mit niedrigster Starter-Position, dann niedrigster Nonce.
- **Sitz mit Keys-Record eines älteren Laufs** tritt neuen Läufen bei und
  behält den Record; vollendet sich der alte doch, gewinnt er.
- **Kontext:** `H("molt-wallet-dkg-v1" ‖ republic_id ‖ init_id ‖ r ‖ t ‖ n)`.
- **Transkript:** `T = H("molt-wallet-transcript-v1" ‖ r ‖ alle Runde-1-
  Nachrichten in Teilnehmer-Reihenfolge)`. Abweichendes `T` → Abbruch.
- **Equivocation / Blame:** Abbruch mit Grund; Sender und Library-Blame
  nur im lokalen Log (`wallet_abort init=… run=… reason=… from=…`).
- Identity-Punkte (jeder Punkt in Runde 1 und 2, Gruppenschlüssel) →
  Abbruch vor dem Library-Aufruf.
- Ingest idempotent; Frames für einen fremden/alten Lauf ignorieren.
- `State.wallet_run` im Speicher. Deadline im Presence-Tick. Reopen ohne
  Laufzustand **und** ohne Keys-Record → Abbruch-Frame; mit Keys-Record →
  nie abbrechen, erneut attestieren.
- Encodings (`i` u16 LE, `init_id` u64 LE, `republic_id` roh 32 B, `r`
  32 B) mit Byte-Pin-Tests.

### 7.5 Fortschritt (W9, W10)

- `WalletRunView` aus dem Laufzustand: `Ready k/n` (mit `missing`) →
  `Round1 k/n` → `Round2 k/n` → `Attest k/n` → `Sealing` → `Done`;
  `Aborted` mit `reason`.
- Frontend-`Event::WalletRunProgress` bei jeder Änderung, damit Wizard,
  Panel und MCP denselben Stand sehen.
- Später eingeschaltet: jeder Sitz bekommt beim Start das Panel über dem
  Hauptfenster (§11).

### 7.6 Abschluss, Attestation, Terminal-Block (W8)

1. Lokal: `ThresholdKeys`, View-Key, Adresse.
2. `A_i` signieren (Identitätsschlüssel, Tag `molt-wallet-attest-v1`,
   Felder Design §3.5).
3. Keys-Record (Share, View-Key, Init, `r`, `T`, Adresse, Netz, m, n,
   Birthday, `A_i`) über den blockierenden Writer schreiben (I4), **dann**
   `A_i` senden.
4. Wer n gültige Attestationen hält, schlägt `wallet_created` vor
   (Position 1 zuerst, dann nach Verzögerung die nächste); eigene
   Proposal-ID; `attestations` in Teilnehmer-Reihenfolge.
5. Approver co-signieren nur bei exaktem Match **und** n gültigen
   Attestationen. Siegel bei m.
6. **Projektion:** Kasse = erster `wallet_created` in der Wallet-Liste,
   der auf den angewandten Init zeigt, W1 hält, Birthday und Netz des
   Inits trägt, `threshold == rule_m`,
   `participants == n` (Genesis/Anker) und n Attestationen trägt, die je
   unter dem `identity_pk` der Gründungsposition `i`
   (`vault_founding_table`) prüfen. Nur Payload + `CheckpointState`.
7. Beim Commit Geschwister-Karten schließen (Vorbild
   `supersede_stale_vault_cards`) und die Keys-Records aller anderen Läufe
   löschen.
8. **Restart:** Keys-Records vorhanden, keine Kasse → jede Attestation
   erneut senden, passenden offenen `wallet_created` co-signieren.

### 7.7 Keine neue Verifikationsregel

- Keine Ablehnung in `verify_chain` / `verify_next` / `fold_one`;
  Checkpoint-Layouts unverändert.
- Op-Satz bei Propose, Approve, Wire-Ingest und in jedem Signierpfad.
- Die Projektion ignoriert deterministisch: andere Wallet-Ops, jeden
  `wallet_init` außer dem ersten, jeden `wallet_created` außer dem ersten
  gültigen.

### 7.8 Recovery und Status

- **Status-Frame** `\x00molt-wstat-v1` (Held/WatchOnly), je Mitglied in
  `TransportState`, Resend am Presence-Tick.
- **Frage/Antwort** `\x00molt-wvask-v1` / `\x00molt-wvresp-v1`: Sitz ohne
  View-Key fragt nach dem Rejoin; Prüfung `view·G` gegen die Adresse.

### 7.9 Scanner

- Off-actor-Task pro offenem Workspace mit `net_scope`, Abbruch in
  `reset_workspace_state`.
- Ab `max(birthday, cursor)`; Rückmeldung per `Net*`-Command.
- Gesehene Output-Keys; letzte Block-Hashes je Höhe für Reorg-Erkennung.
- 20 Bestätigungen; Reorg → zurückspulen oder Rescan ab Birthday.
- `UnsupportedProtocol` → `scan_paused = "update needed"`; Dekodierfehler →
  Daemon-Fehler.

## 8. Kontrakt-Erweiterungen

- `Command`: Tools (Seat) `WalletInit`, `WalletConsent { accept }`,
  `WalletRetry`, `WalletAcknowledgeLoss`; INTERNAL `NetWalletProbe`,
  `NetWalletFrame`, `NetWalletScan`, `NetWalletStatus`,
  `NetWalletViewAnswer` (Namen beim Bau final; Liste in der Testfunktion).
- Keine neuen `WorkspaceEvent`-Varianten (Läufe sind Control-Frames).
- `Event` (Frontend): `WalletRunProgress`, `WalletCreated { address }`,
  `WalletScanPaused`.
- `SurfaceSnapshot.wallet: Option<WalletView>`.
- MCP: obige Tools mit `Scope::Seat`. `read_state(wallet)` bleibt Seat; der
  Read-Key sieht die Kasse **nicht**.
- `[wallet]`: `daemon_url`, `daemon_confirmed`, `daemon_login`,
  `network` (Default `mainnet`, W11). In `Config`, `Settings`, `salvage`, `render`. Kein
  Default-Daemon. Nur abweichend vom Default geschrieben: ältere Binaries
  lehnen nur eine Config mit gesetztem Daemon ab — Release-Notes.

## 9. Persistenz

| Datei | Segment | Inhalt | Export | Import |
|---|---|---|---|---|
| `wallet_keys.state` | `u64::MAX − 9` | Keys-Records (je attestiertem Lauf, nach Commit nur der der Kasse) | prüfen; defekt → **Abbruch** | prüfen; defekt → verwerfen, watch-only; Record ≠ Kasse → watch-only |
| `wallet_scan.state` | `u64::MAX − 10` | Cursor, Block-Hashes, gesehene Keys, Outputs | prüfen; defekt → skip + benennen | prüfen; defekt → verwerfen, Rescan |

- Sub-Keys `hkdf32(ws_key, "molt-wallet-keys", id)` /
  `"molt-wallet-scan"`; `encode_frame` + `write_atomic`; Cap
  `READ_CAP_STATE`. `RESERVED_SEGMENT_FLOOR` auf `−10`, Pin-Test mit.
- Schlüsseldatei: `WriterMsg::PersistWalletKeys` +
  `persist_wallet_keys_blocking`. Records werden angehängt, nie ersetzt;
  erst der Commit der Kasse räumt die anderen ab (I16).
- Beim Öffnen und Import: Record gegen die projizierte Kasse (Init, Lauf,
  Adresse); Abweichung → watch-only.
- **Ausweg (W5):** `WalletAcknowledgeLoss` legt eine defekte
  Schlüsseldatei als Dot-Datei beiseite (Export ignoriert Dot-Dateien);
  Sitz watch-only; Exporte laufen wieder (hebt `backup_hold` auf, Test).
- **Backup-Ticker:** Backoff bei Schlüssel-Fehler; eine Meldung.
- Import mit Ersetzen: die Records des ersetzten Ordners kommen zu denen
  des Blobs (I12); eine defekte Datei dort reist mit, wenn der Blob keine
  bringt.
- `docs_archive/storage/backup_restore_design.md` §3.2 + Allowlist
  nachziehen. Release-Notes: ältere Builds exportieren ohne Schlüsseldatei
  und lehnen Blobs mit ihr ab.

## 10. TDD-Fahrplan (rote Tests zuerst)

**molt-treasury:**
1. `a_three_seat_dkg_agrees_on_key_view_and_address` (Byte-Roundtrip).
2. `the_context_binds_republic_init_and_run`.
3. `view_key_depends_on_every_contribution`.
4. `a_differing_transcript_aborts`.
5. `two_round_one_frames_from_one_sender_abort`.
6. `an_identity_point_in_either_frame_aborts` (jeder Punkt beider
   Frames; der Gruppenschlüssel: `an_identity_group_key_has_no_address`).
7. `the_address_is_a_standard_main_address`.
8. `attestations_verify_only_for_their_seat_and_fields`.
9. `an_attestation_from_another_run_or_init_fails`.
10. `wallet_keys_round_trip_and_zeroize`.
11. `a_repeated_output_key_is_ignored`.
12. `unsupported_protocol_pauses_and_a_decode_error_does_not`.
13. Byte-Pin-Tests aller Layouts (Kontext, View, Transkript, Attestation);
    dazu `attest_tag_cannot_collide_with_openmls_sign_content`.

**molt-core:**
14. `daemon_kind_classifies_onion_local_clearnet` (inkl. Userinfo,
    Backslash, `ws://` abgelehnt; `relay_kind` unverändert).

**molt-storage:**
15. `wallet_keys_and_scan_round_trip_under_their_segments`.
16. `a_damaged_keys_file_fails_the_export`.
17. `a_damaged_keys_file_restores_watch_only`.
18. `a_damaged_scan_file_is_skipped_and_named`.
19. `an_acknowledged_loss_lets_the_export_resume`.

**molt-engine (E2E über MockRelay, Harness `vault_support`):**
20. `wallet_needs_two_to_n_minus_one`.
21. `set_features_cannot_add_wallet`.
22. `a_memory_vote_in_a_legacy_wallet_republic_creates_no_purse`.
23. `wallet_init_turns_the_feature_on`.
24. `one_decline_kills_the_init_card`.
25. `a_future_birthday_is_declined_but_a_lagging_daemon_is_not`.
25a. `a_seat_without_a_daemon_abstains`.
25b. `the_init_card_is_approvable_while_wallet_is_off`.
26. `a_seat_without_a_daemon_aborts_at_readiness_and_is_named`.
27. `a_declined_consent_aborts_the_run_and_retry_starts_a_new_nonce`.
28. `three_seats_found_one_purse` (gleiche Adresse, Datei vor Attestation).
29. `a_founding_with_wallet_runs_the_purse_stage` (Auto-Init, Auto-Approve).
30. `an_aborted_founding_stage_leaves_the_republic_founded`.
31. `a_late_enable_runs_the_stage_on_every_seat`.
32. `wallet_created_without_n_attestations_is_ignored`.
33. `m_seats_cannot_seal_a_purse_of_their_own`.
34. `an_anchor_rejoiner_computes_the_same_purse` (Schnitt zwischen Init und
    `wallet_created` und danach).
35. `racing_wallet_created_cards_leave_one_purse`.
36. `a_mock_transfer_block_does_not_break_the_chain` (Alt-Republik).
37. `reopening_does_not_restart_a_run`.
38. `a_withheld_attestation_cannot_seal_a_run_without_shares`
    (Records bleiben; der alte Lauf gewinnt mit allen Teilen).
38a. `racing_starts_converge_on_one_run` und `a_reused_nonce_is_refused`.
38b. `a_recovery_never_auto_proposes_an_init`.
38c. `a_restored_record_of_another_run_is_view_only`.
38d. `a_purse_with_a_foreign_birthday_or_network_is_ignored`.
39. `a_restart_after_persist_re_attests_and_cosigns`.
40. `a_phrase_only_recovery_is_watch_only_and_gets_the_view_key`.
41. `a_backup_restore_keeps_the_share`.
42. `the_read_key_does_not_see_the_wallet`.
43. Co-Equality grün; `is_implemented`-Test angepasst.

**Manuell, `#[ignore]`:** regtest-monerod hinter `MOLT_TEST_MONEROD`
(`network = testnet` bzw. regtest): Mining sichtbar; Close/Reopen hält
Stand.

## 11. UI und Usability (Etappe 1)

**Maßstab: der offizielle Monero-GUI-Client im Simple Mode.** Wer ihn
bedienen kann, bedient die Kasse ohne Erklärung. Die Mehrparteien-Technik
bleibt unsichtbar; sichtbar sind nur Kontostand, Empfangen, Verlauf.

**Regeln (Review-Kriterium wie kompakte Texte):**

- **U1 Kein Krypto-Vokabular.** Nie „DKG“, „Runde“, „Transkript“,
  „Attestation“, „Share“, „View-Key“, „Init“, „Nonce“ in der UI. Wörter:
  *purse*, *set up*, *key part*, *view only*.
- **U2 Ein Balken, vier Klartext-Stufen** (Abbildung von `RunStage`):
  `Ready` → „Waiting for members 3/5“ (+ wer fehlt); `Round1`/`Round2` →
  „Creating the purse“; `Attest` → „Confirming 4/5“; `Sealing` →
  „Sealing“; `Done` → Adresse + Warnung. `Aborted` → ein Grund
  („Bob is offline“, „Bob declined“) und „Try again“.
- **U3 Beträge in XMR**, nachlaufende Nullen weg, nie Piconero; Gruppierung
  nach Locale.
- **U4 Keine Eingaben, wo keine nötig sind.** Gründung: die Checkbox ist die
  Zustimmung; die Kassen-Stufe fragt nur nach dem Daemon, wenn keiner
  gesetzt ist. Später: Zustimmen/Ablehnen, sonst nichts.
- **U5 Fehler in einer Zeile**, der Ausweg einmal.
- **U6 Technik nur unter „Details“** (eingeklappt): Daemon, Höhe,
  Netzwerk, „Key parts held 4/5“, je Mitglied *key part* / *view only*.

**Ansichten** (bestehende Views `balance`, `receive`, `history`,
`status`):

| Ansicht | Inhalt |
|---|---|
| Balance | große Zahl „1.25 XMR“; darunter „0.5 XMR pending“ nur wenn > 0; ohne Kasse ein Knopf „Set up the purse“ |
| Receive | Warnung (W11) groß direkt über der Adresse; Adresse monospace; „Copy“; QR-Code als `monero:<address>`-URI |
| History | je Zeile: Richtung, Betrag, Datum, „3/20“ bis bestätigt, dann ein Haken; txid nur im Kontextmenü „Copy txid“ |
| Details (`status`) | U6 |
| Send | gesperrt, eine Zeile „Spending is not available yet.“ |

**Daemon wie der „Remote node“ des GUI:** ein Feld, ein Knopf. Der Start-
Sitz mit Daemon kündigt seine URL als **Vorschlag** an (`wdhint`,
flüchtig, nie auf der Chain), solange eine Kassen-Stufe offen ist; die
Stufe zeigt ihn vorausgefüllt mit „Use“. Nicht-Onion → derselbe Bestätigungsdialog wie bei Relays, einmal.
Kein mitgelieferter Daemon. Wer einen eigenen setzt, stärkt die
Gegenprüfung (Design §7); der Vorschlag tauscht etwas davon gegen einen
Klick.

**Wizard:**
- Feature-Schritt: Wallet-Checkbox **standardmäßig an**, wenn W1 hält;
  sonst gesperrt mit einer Zeile Grund. Darunter die drei Zeilen aus
  Design §3.3 (U1-konform formuliert).
- **Letzter Schritt (Gründer und Joiner):** nach `SealedPanel`
  automatisch die Kassen-Stufe (U2, U4): erst „Choose a node“ falls
  keiner gesetzt, dann der Balken; „Enter republic“ immer da, der Balken
  läuft im Panel weiter.

**Späteres Einschalten:** dieselbe Komponente als Modal über dem
Hauptfenster auf jedem Sitz; Zustimmen/Ablehnen bei `needs_consent`; eine
Zeile „Everyone must be online.“; „Try again“ nach Abbruch.
Organization-Modal: Wallet-Checkbox entsperrt (W1), erzeugt `wallet_init`.

**Sonst:** Verlust-Dialog für die Schlüsseldatei (§9); `default_op`
(`transfer`) und tote `wl_*` entfernen; kein Em-Dash. Tests nach
`tests/gui/vault.rs`, u. a. `the_warning_sits_beside_the_address`,
`the_wizard_ends_with_the_purse_stage`, `no_crypto_words_in_wallet_strings`
(Wortliste U1 gegen alle `wl_*`/Wallet-Strings), `amounts_render_in_xmr`.

**QR-Code:** keine eigene Kodierung: `qrcode` 0.14 ohne Default-Features
(Urteil §6).

## 12. Fork-Vorsorge (FCMP++ + Carrot)

- Scanner hinter eigenem Trait; Carrot-Update = Implementierung tauschen.
- `UnsupportedProtocol` → Pause, Bilanz bleibt.
- Am Fork prüfen: Carrot-fähiges monero-wallet-Release; Legacy-Scan mit
  geteiltem View-Key unverändert?
- `ThresholdKeys` und Adresse bleiben; nichts migrieren.

## 13. Etappe 2 — Bedingungen, kein Bau

Erst, wenn alle drei gelten (Design §8): `SalLegacyAlgorithm` über
`ThresholdKeys<Ed25519>` in veröffentlichter monero-wallet (nicht
`SalAlgorithm`, das läuft über `Ed25519T`); Beweis oder Audit;
Fork-Höhen fest. Bis dahin `can_spend = false`, kein `sign.rs`.

## 14. Ausführungsreihenfolge

1. [x] Design rev 3 + Plan rev 3 — zur Ratifizierung.
2. [x] §6 Dependency-Lock + bare `molt-treasury` + pedpop-Review +
   Digest-Auth-Urteil (2026-10-09).
3. [x] §10.1–13 molt-treasury; §10.14 Daemon-Klassifizierung (2026-10-09).
   Runde-1-Nachricht = `c_i ‖ pedpop-Bytes`, `T` deckt beide; `m`/`n` in
   Attestation und Keys-Record u16 LE, Netz ein Byte (0 main, 1 test,
   2 stage). `http://` nur zu Onion/Local (wie `ws://`). Der Graph-Guard
   lässt molt-cores Teilbaum aus (chronos Plattform-Krates). Die
   Scan-Datei-Bytes kommen mit Schritt 4.
4. [x] §9 + §10.15–19 Storage, Backup-Doku (2026-10-09). Storage keeps
   the records opaque (no molt-treasury dependency, as with the vault):
   plaintext `molt-wallet-keys-file-v1` ‖ count ‖ length-prefixed records,
   byte-pinned. Append skips a byte-equal record and refuses a damaged
   file; `prune_wallet_keys(keep)` keeps the byte-equal record and refuses
   when it is not held. Scan bytes: `ScanState::encode/decode`
   (`molt-wallet-scan-v1`, seen keys = output keys). Set aside as
   `.wallet_keys.state.lost<n>`, never an intact file. The ticker's hold
   rides `NetBackupFailed.hold`: 1 h, doubling to 24 h, one notice. The
   record-vs-purse check stays with step 7 (`read_wallet_keys`,
   `ImportStaging::wallet_keys_dropped`).
5. [x] §8 Kontrakt, MCP, Config; Co-Equality grün (2026-10-09).
   `molt-core/src/wallet.rs` (view, `WalletRefusal`, `bounds_ok`);
   `SurfaceSnapshot.wallet` is boxed (clippy: `Reply` size). INTERNAL:
   `NetWalletProbe`, `NetWalletFrame`, `NetWalletScan`, `NetWalletStatus`,
   `NetWalletViewAnswer`. Session keys `wallet_daemon_url`,
   `wallet_daemon_confirmed`, `wallet_daemon_login` (write-only, stripped
   for the read key with any `wallet` snapshot), `wallet_network`. The
   daemon's one door is `patch_settings`: a wholesale save keeps it, a new
   URL starts unconfirmed. `[wallet]` is written only off its defaults, so
   older builds keep opening an untouched file; a URL or login the loader
   refuses never reaches the settings, and a save leaves its line alone.
   `WalletAcknowledgeLoss` acts on the open workspace, refuses while a
   backup is in flight, and lifts its backup hold. The phase is
   `Off`/`Bounds`/`NoPurse` from the genesis or anchor rule; the other
   doors answer `purse: not available yet` (`TODO(step N)` in
   `molt-engine/src/wallet.rs`).
6. [x] §7.1–7.2 + §7.7; §10.20–25, 36 (2026-10-10). The daemon
   transport came forward from step 8 (`molt-net/src/monero_rpc.rs`,
   `s3::http` with a per-call cap and its own `HttpError`; the
   `stub-daemon` feature is the in-process monerod of the tests).
   `WalletInit` probes off the actor and the probe's
   `NetWalletProbe` answers the caller with the proposal. Birthday =
   height − 10; an approver takes it up to 30 above and 1440 below its
   own daemon (a height older than 120 s is asked again). The
   projection counts the first *well-formed* applied init. A raw
   `propose` on Wallet answers `use wallet_init` (as the vault's). A seat
   judges each init card when it lands: another network or a birthday
   out of its window declines at once (the card dies, W6), also after an
   own approve whose re-sign aged out; a seat without a daemon, or whose
   daemon is still syncing (`get_info`), abstains on any network. An
   `approve` it cannot check yet is kept in memory, answers `... -
   approval held` and signs once the daemon answers. A malformed init is
   dropped at the wire; a close or a daemon change cancels a waiting
   `WalletInit` (`set-up cancelled`). `wallet_created` is refused at every
   signing path until step 7. `wallet` is no keep requirement of
   `set_features` (the union keeps it, like the vault), so the
   organization dialog no longer rides it along. §10.20–22 and 36 are unit tests on built
   chains (`molt-engine/src/wallet_tests.rs`); 23–25a also run end to
   end (`tests/wallet_init.rs`).
7. [ ] §7.3–7.6 + §7.8; §10.26–35, 37–41.
8. [ ] §7.9 Scanner; manueller regtest-Lauf (the RPC transport landed
   with step 6).
9. [ ] §11 UI.
10. [ ] clippy pro Crate = 0; Suiten grün; Review über den Gesamt-Diff;
    master.

## 15. Bekannte Fallen

- Zeilennummern driften — Symbole greppen.
- Den `schnorr-signatures`-Pin nicht „aufräumen“ (multiexp-Spaltung).
- **Keine neue Regel in der Chain-Verifikation**, Checkpoint-Layouts
  unverändert (§7.7).
- Die Projektion liest **nur** Payload + `CheckpointState`: keine
  Signaturanzahl, keine Höhe, keine Reihenfolge zwischen Surfaces — die
  überleben keinen Schnitt.
- `rule_m`/`rule_n` aus Genesis/Anker, nie `State::threshold()`.
- Marker nie aus dem Feature-Wert ableiten; `set_features` fügt `wallet`
  nicht hinzu.
- `wallet_created` braucht eine eigene ID; Geschwister-Karten schließen.
- Decline- und Birthday-Check in **jedem** Signierpfad.
- Auto-Start nur im Hook `after_block_applied`, einmal pro Init, nie beim
  Restore; Auto-Init nur bei Create/Join, nie Recovery; danach nur
  `WalletRetry`.
- Keys-Records nie ersetzen vor dem Commit (I16); Reopen mit Record bricht
  nie ab.
- `require_feature` sitzt auch in `cmd_approve` — `wallet_init` ausnehmen.
- Gründungsritual nicht anfassen; die Stufe beginnt nach dem Siegel.
- `relay_kind` kennt nur `ws`/`wss` — Daemon über `daemon_kind`.
- Kind-445-Events werden nicht gechunkt; Control-Frames sind klein.
- Control-Frames sind nicht an Dritte beweisbar — keine öffentliche Schuld.
- `WeakSender` beendet keine Workspace-Tasks; `net_scope`.
- `MemberSeen` ist ein No-op.
- Nicht `monero-simple-request-rpc`, nicht `GuaranteedScanner`, nicht
  `multisig`-Feature in Etappe 1.
- Read-Key sieht die Kasse nicht.

## 16. Definition of Done (Etappe 1)

- Tests aus §10 grün; regtest-Lauf einmal manuell, Protokoll im Commit.
- clippy pro Crate = 0; `dev-ui.sh build` sauber.
- Manuell über drei Instanzen: Gründung mit Kassen-Stufe als letztem
  Wizard-Schritt, gleiche Adresse, Warnung sichtbar, Mining sichtbar,
  Close/Reopen, Export/Import behält den Teil, Phrase-Recovery zeigt
  watch-only; zweite Republik ohne Kasse → späteres Einschalten mit Panel.
- Review über den Gesamt-Diff, Findings gefixt, grün auf master.
- Doc-Status aktualisiert, `check-doc-refs.py` sauber.

## 17. Änderungen gegenüber Revision 2

- W8: n Identitäts-Attestationen im `wallet_created`; Projektion prüft aus
  dem Payload; Siegel bei m statt n, `try_commit` unverändert.
- W4 neu: `wallet_init` ist die einzige Tür; kein `wallet_rev`-Marker;
  `set_features` fügt `wallet` nicht hinzu.
- Init (einmal, Chain) und Lauf (flüchtig, Run-Nonce, Retry ohne Block)
  getrennt; Starter/Proposer mit Positions-Fallback.
- W9/W10: Kasse standardmäßig an; Kassen-Stufe als automatischer letzter
  Wizard-Schritt; Panel beim späteren Einschalten.
- W11: Mainnet erlaubt, Warnung neben der Adresse.
- Usability-Regeln U1–U6 nach dem offiziellen GUI (Simple Mode); QR-Code;
  Daemon-Vorschlag per `wdhint`.
- Review-Runde rev 3: Keys-Record je Lauf (nie ersetzt), Daemon zuerst +
  Enthaltung, Birthday-Marge, Feature-Gate auch bei Approve, Hook
  `after_block_applied`, Auto-Init nur Create/Join, Lauf-Rennen,
  „Enter republic“ immer offen, Kasse prüft Birthday/Netz des Inits,
  Mainnet als Default.
- Keine öffentliche Schuldzuweisung.
- Daemon: `daemon_kind` aus der Relay-Host-Regel, Nicht-Onion-Gate, kein
  Default-Daemon.
- Anker korrigiert: `vault_founding_table` statt neuem Helfer;
  `own_approvals` ist ein Feld; `checked_kind` eine Closure; Floor per
  Test gepinnt; `s3::http` sind freie Funktionen; `rule_m` aus Genesis.
- Upstream korrigiert: Pin-Ursache, pedpop auf `next` gelöscht,
  `SalLegacyAlgorithm`, Scanner gehört zu monero-wallet.
