# Multisig-Wallet-Surface: Implementierungsplan Etappe 1

Stand: **Revision 2, 2026-10-06**, alle Anker gegen master `9573c3a`
verifiziert, unabhängig reviewt. Ersetzt Revision 1 (2026-08-16); §17
listet die Änderungen. Design-Autorität: `docs/chain/wallet_treasury_design.md`
(rev 2). Dieses Dokument ist der Bauplan: Dateien, Symbole, Tests,
Reihenfolge.

**Scope: nur Etappe 1** — Kasse einrichten, empfangen, beobachten.
Ausgeben (Etappe 2) wartet auf SA+L-Threshold-Signing in monero-wallet
(W2, Design §8). Kein Code für Etappe 2 anlegen.

**Für die Implementierung gilt zwingend:**
- Zeilennummern sind Stand `9573c3a` — **immer per Symbolname greppen.**
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
| W1 | Einschalten nur bei `2 ≤ m ≤ n − 1` |
| W2 | Etappe 2 wartet auf SA+L; kein CLSAG-Spend |
| W3 | Nur Monero |
| W4 | Altes Einschalten zählt nicht; frischer `set_features`-Vote mit Marker |
| W5 | Schlüsseldatei defekt: Export bricht ab; Import ohne sie, Sitz watch-only |
| W6 | Ein Decline beendet den Init |
| W7 | DKG-Teilnehmer = Position in der Genesis-Gründungstabelle (Vault-Ordnung) |

Weiter gültig aus rev 1: Republik = Wallet, ein Threshold-Begriff, nur XMR,
Charter-Feature, nie abschaltbar.

## 2. Der Stack (per Spike verifiziert 2026-10-06)

Zwei Spikes dieser Session (nicht im Repo): `treasury-spike` (DKG,
Adresse, View-Key, FROST-Signatur) und `stage1-min` (Etappe 1 ohne
`multisig` und ohne `modular-frost`: DKG-Test grün, kein `ring`, kein C).

**Etappe 1 braucht:**

| Krate | Version | Rolle |
|---|---|---|
| `dkg` | 0.6.1 | `ThresholdParams`, `ThresholdKeys`, `Participant` |
| `dkg-pedpop` | 0.6.0 | DKG, 2 Runden, `BlameMachine`; **verwaist upstream** |
| `dalek-ff-group` | 0.5 | `Ed25519`-Ciphersuite |
| `ciphersuite` | 0.4 | `Ciphersuite`-Trait |
| `monero-wallet` | 0.2.0, **ohne** `multisig` | `ViewPair`, `Scanner`, Adressen |
| `monero-daemon-rpc` | 0.2.0 | RPC-Client über eigenen `HttpTransport` |
| `schnorr-signatures` | **=0.5.2** (Pin) | sonst baut `dkg-pedpop` nicht |

`modular-frost` und das `multisig`-Feature (außerhalb SemVer) kommen erst
mit Etappe 2.

Verifizierte API-Fakten:
- DKG: `KeyGenMachine::new(params, context: [u8; 32])` →
  `generate_coefficients(rng)` → `generate_secret_shares(rng, map)` →
  `calculate_share(rng, map)` → `BlameMachine::complete()` / `.blame(..)`.
- Alle n müssen teilnehmen (`validate_map`). pedpop erkennt **keine**
  Equivocation (unterschiedliche Commitments an verschiedene Empfänger) —
  „responsibility lies with the caller“; `complete()` erst nach bestätigtem
  Abschluss mit allen (Doku). → Transkript-Hash (§7.3).
- Kontext soll pro Multisig eindeutig sein (Doku). → §7.3.
- `*::read` brauchen die eigenen `ThresholdParams`.
- Zwischenstände sind nicht serialisierbar.
- Blame braucht den `EncryptionKeyProof` des Anklägers und kann auch den
  Empfänger treffen.
- **Kein View-Key aus dem DKG.** Adresse = `ViewPair::new(group_key, view)
  .legacy_address(network)`.
- **Nicht `GuaranteedViewPair`/`GuaranteedScanner`.** Standard-`Scanner`;
  gesehene Output-Keys MÜSSEN geprüft und gespeichert werden.
- `Scanner::scan` lehnt Blöcke mit `hardfork_version > 16` ab
  (`UnsupportedProtocol`); unbekannte Output-Typen scheitern schon beim
  Dekodieren.
- `dalek-ff-group::from_bytes` lehnt Torsion ab; `ViewPair::new` prüft.
- Größen (n=13): Runde 1 ≈ m·32 + 96 B, Runde 2 ≈ 128 B je Empfänger.
- `monero-simple-request-rpc`: zieht `ring` + `cc`, nur URL. **Nicht
  verwenden.**

## 3. Repo-Regeln, die hier besonders greifen

1. **TDD.** Jeder Schritt beginnt mit roten Tests (§10).
2. **clippy = 0, auch Tests, pro Crate.** `.expect("…")`, nie `.unwrap()`.
3. **Co-Equality.** Neue `Command`-Varianten: MCP-Tool (mit `scope`) oder
   `INTERNAL` (Liste in der Testfunktion
   `co_equality_every_command_is_a_tool_or_documented_internal`).
4. **Events additiv**; keine neue Regel in der Chain-Verifikation (§7.5).
5. **Kein I/O in molt-core.** Handler synchron; Blockierendes off-actor.
6. **GUI:** `scripts/dev-ui.sh build` + Live-Preview-Tests. Kein voller
   Fenster-Build pro Change-Set. Nie `DISPLAY=:0`.
7. **Direkt auf master;** Review über den Diff vor dem Landen.
8. **Secrets nie in getrackten Artefakten benennen.**
9. **Status-Zeilen pflegen;** nach Doc-Moves `scripts/check-doc-refs.py`.

## 4. Anker im Repo (Stand `9573c3a`)

| Anker | Ort | Rolle |
|---|---|---|
| `Surface::Wallet`, Views | `molt-core/src/lib.rs` (`Surface::views`) | bleibt; `send` bleibt Stub |
| `Surface::is_implemented` | `molt-core/src/lib.rs` | Wallet aufnehmen (Test pinnt) |
| `effective_features` / `feature_on` / `require_feature` | `molt-engine/src/chain/projection.rs` | Wallet-Sonderfall (§7.1) |
| `propose_payload`, Enable-only-Gate („already enabled“), `set_features`-Kanonisierung | `molt-engine/src/proposals.rs` | Marker `wallet_rev` |
| `cmd_propose` / `cmd_approve` | `proposals.rs` | Wallet-Arm, Approve-Checks |
| `register_decline` (`veto_room`) | `proposals.rs` | `veto_room = 0` für `wallet_init` |
| `chain_sign_and_gossip_approval` | `chain/governance.rs` | dieselben Checks auch hier |
| `try_commit` (`need`, `CutKind`) | `chain/governance.rs` | n-of-n für `wallet_init` und `wallet_created` |
| `proposal_change` / `id_free_for` | `chain/governance.rs` | Grund für die eigene ID des Terminal-Blocks |
| `rebase_pending_approvals`, `own_approvals`, `forget_votes_for` | `chain/governance.rs`, `events.rs` | Re-Sign; Geschwister-Karten |
| `maybe_auto_checkpoint` | `chain/checkpoint.rs` | Vorbild „nur live, nie beim Replay“ |
| `own_signature_stands`, `cmd_propose_checkpoint`, `receive_checkpoint_proposal` | `chain/governance.rs`, `chain/checkpoint.rs` | Vorbild Auto-Co-Sign |
| `vault_arm`, `vault_approve_check` | `molt-engine/src/vault/mod.rs` | Vorbild geschlossener Op-Satz + Prüfung in jedem Signierpfad |
| `supersede_stale_vault_cards` | `vault/deposit.rs` | Vorbild Geschwister-Karten schließen |
| `seat_x`, `ctx_from_founding` | `vault/grant.rs`, `vault/mod.rs` | Sitzordnung; Helfer aus den **Genesis-`founding_identities`** bauen (nicht aus `founding.rs::full_identities`, nicht nur roster-v6) |
| `bounds_ok`, `walk_enabled` | `vault/mod.rs` | Vorbild Einschaltgrenze im Fold |
| `State::threshold()`, `State.net_ritual` | `molt-engine/src/lib.rs` | `rule_m`; Vorbild Ritual-Zustand |
| `net_scope_current`, `reset_workspace_state` | `net/mod.rs`, `events.rs` | Lebensdauer Scanner |
| `cmd_net_presence_tick`, `vault::receipts::tick` | `net/presence.rs`, `vault/receipts.rs` | Resend-Cursor, Deadline-Prüfung |
| `CONTROL_FRAMES` | `molt-net/src/supervisor.rs` | neue Tags registrieren |
| `vault_frames.rs`, `mirror_gossip.rs` | `molt-net/src/` | Vorbild Frames (Runden, Status, Frage/Antwort) |
| `GroupHandle::publish_control` | `molt-net/src/group_runtime.rs` | best-effort, daher Resend |
| `TransportState` (`mirror`, `vault_status`) | `molt-core/src/lib.rs` | Ablage Shareholder-Status, Rundenzustand nach Runde 2 |
| Segmente `−1 … −6`, `RESERVED_SEGMENT_FLOOR` | `molt-storage/src/lib.rs` | neu: `−7` Schlüssel, `−8` Scan; Floor mitziehen |
| `chain_state_key`, `encode_frame`, `write_atomic` | `molt-storage/src/lib.rs` | Muster für beide Dateien |
| `persist_chain_blocking`, `WriterMsg` | `molt-storage/src/lib.rs` | blockierender Writer (Persist vor Attest) |
| `collect_entries`, `checked_kind` | `molt-storage/src/export.rs` | Export-Arme |
| `allowed_entry`, Verify-Loop, `staging.dropped` | `molt-storage/src/import.rs` | Import-Arme |
| `READ_CAP_STATE` | `molt-storage/src/lib.rs` | Cap |
| Backup-Ticker | `molt-engine/src/backup.rs` | Backoff bei Schlüssel-Fehler |
| `tools()`, `Scope` | `molt-mcp/src/lib.rs` | `wallet_init` (Seat); `read_state` ist Seat |
| `VaultView` / `SurfaceSnapshot.vault` | `molt-core/src/vault.rs` | Vorbild `WalletView` / `.wallet` |
| `WalletPane` (Mock) | `molt-ui-window/ui/surfaces.slint`; Route `app.slint` | wird echt |
| `default_op` (`"transfer"`), `wl_*` | `molt-ui/src/labels.rs`, `i18n.rs` | aufräumen |
| `relay_kind` / `RelayKind` | `molt-core/src/relay.rs` | „lokal“-Klassifizierung für den Daemon |
| `s3::http` (Client, `MAX_RESPONSE` 4 MiB) | `molt-net/src/s3/http.rs` | HTTP-Client wiederverwenden; Größengrenze für RPC anpassen |
| `Dialer` | `molt-net/src/dial.rs` | Transport |
| `Config` (`deny_unknown_fields`) | `molt-config/src/` | Sektion `[wallet]` |
| E2E-Harness | `molt-engine/tests/vault_support/mod.rs` | Vorlage |
| **Namensfalle** `WalletHandle` | `molt-engine/src/lib.rs` | Engine-Aktor-Handle, nicht die Kasse |

## 5. Architektur

### 5.1 Crate `crates/molt-treasury`

Vorbild `molt-vault`: reine Funktionen, kein I/O, hängt nur an `molt-core`
und den Krates aus §2. Workspace-Header ergänzen.

```
crates/molt-treasury/src/
  lib.rs      // Fassade, TreasuryError
  dkg.rs      // Runden: Kontext, Identity-Check, Transkript, Blame-Beweise
  keys.rs     // View-Key aus Beiträgen, Standardadresse, (De)Serialisierung,
              // zeroize; Byte-Layouts mit Tags + Byte-Pin-Tests
  scan.rs     // Scanner-Kern hinter eigenem Trait: ViewPair, gesehene Keys,
              // Block-Hashes, UnsupportedProtocol -> Pause
```

- **RPC-Transport** in molt-net (`monero_rpc.rs`): `HttpTransport` über den
  bestehenden `s3::http`-Client und den `Dialer`. Lokal = `relay_kind` ==
  `Local` (Loopback, privat, link-local), dann direkt; sonst Dialer,
  fail-closed. Digest-Login für `--rpc-login`; Antwortgrenze passend zu
  `get_blocks.bin`.
- **Sitz-Helfer:** ein gemeinsamer Ort (z. B. `molt-engine/src/seats.rs`),
  gebaut aus den Genesis-`founding_identities` (Genesis oder Anker); Vault
  und Wallet nutzen ihn.

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
    pub phase: WalletPhase,          // Off | Legacy | Enabled | Init(..) | Ready
    pub shareholders: Vec<(String, ShareStatus)>, // Held | WatchOnly | Unknown
    pub history: Vec<WalletTxView>,
    pub can_spend: bool,             // Etappe 1: immer false
}
```

## 6. Dependency-Lock (Schritt 2)

```toml
dkg = "0.6"
dkg-pedpop = "0.6"
dalek-ff-group = "0.5"
ciphersuite = "0.4"
monero-wallet = { version = "0.2", default-features = false, features = ["std", "compile-time-generators"] }
monero-daemon-rpc = { version = "0.2", default-features = false, features = ["std"] }
schnorr-signatures = { version = "=0.5.2", default-features = false }
```

1. In `[workspace.dependencies]`, Minor exakt halten.
2. Bare `molt-treasury` mit den Spike-Funktionen. Compilieren = gelockt.
3. Prüfprotokoll: `cargo tree -d`, `-i ring`, `-i cc` leer, keine `*-sys`.
4. **`dkg-pedpop` reviewen:** gepinnte Version lesen (Transcript,
   Kontext, Blame), Identity-Check vor dem Lesen der Commitments.
5. Ergebnis ins Design-Doc §12.

## 7. Engine

### 7.1 Einschalten (W1, W4)

- `set_features` mit `wallet` bekommt `"wallet_rev": 1` (beim Propose
  gesetzt; die Kanonisierung schreibt nur `value` um und behält das Feld).
- Enable-only-Gate: `wallet` gilt als neu, solange keine angewandte
  Einschaltung mit Marker existiert.
- `feature_on(Wallet)` = Marker **und** `2 ≤ m ≤ n − 1` **und**
  chain-governed, im Fold aus der verifizierten Chain (Vorbild
  `walk_enabled`).
- `wallet` ohne Marker: `WalletPhase::Legacy`, Commands verweigern mit
  eigener Begründung.
- Wizard: Wallet bleibt gesperrt. Organization-Modal bietet den Vote an,
  sobald W1 erfüllt ist.

### 7.2 Init und Konsens (W6)

- `Command::WalletInit` (Tool, `Scope::Seat`): Gates; Daemon-Probe
  off-actor; Proposal `{op: wallet_init, birthday_height}`.
- **Approve-Checks** in `cmd_approve` **und**
  `chain_sign_and_gossip_approval` (Vorbild `vault_approve_check`):
  geschlossener Op-Satz; Birthday ≤ eigene Daemon-Höhe und nicht mehr als
  ein festes Fenster darunter; kein laufender Init; Gates aus §7.1.
- **Ein Decline beendet:** `veto_room = 0` für `wallet_init` in
  `register_decline`.
- **Siegel bei n:** `need = n` für `wallet_init` neben dem `CutKind`-Fall in
  `try_commit`.
- **Start des Rituals** nur, wenn der Init-Block **live** angewandt wurde
  (Vorbild `maybe_auto_checkpoint`), **n** verschiedene gültige Signaturen
  trägt und die eigene Zustimmung noch steht (`own_approvals`). Ein von
  einem älteren Build bei m gesiegelter Init ist wirkungslos.
- Re-Sign nach Head-Move braucht jeden Sitz online — Hinweis im UI.

### 7.3 Runden (Control-Frames)

Tags in `CONTROL_FRAMES`, je mit `init_id`; Resend am Presence-Tick bis
Commit oder Abbruch; nie im Workspace-Log:

| Tag | Inhalt |
|---|---|
| `\x00molt-wrdy-v1` | Sitz hat Wallet-Build + Daemon (Runde 0) |
| `\x00molt-wr1-v1` | Commitments + PoP, View-Beitrag `c_i` |
| `\x00molt-wr2-v1` | Shares (je Empfänger, protokollverschlüsselt) + Transkript-Hash `T` |
| `\x00molt-wabrt-v1` | Abbruch; Name nur mit Beweis |

- **Kontext:** `H("molt-wallet-dkg-v1" ‖ republic_id ‖ init_id ‖ t ‖ n)`.
- **Transkript:** `T = H("molt-wallet-transcript-v1" ‖ alle Runde-1-
  Nachrichten in Teilnehmer-Reihenfolge)`. Abweichendes `T` → Abbruch.
- **Equivocation:** zwei verschiedene Runde-1-Frames eines Senders →
  Abbruch; Name nur mit beiden signierten Frames als Beweis.
- **Blame:** Name nur mit dem Library-Beweis (`EncryptionKeyProof`), sonst
  Abbruch ohne Namen.
- Identity-Punkte → Abbruch vor dem Library-Aufruf.
- Ingest idempotent.
- `State.wallet_ritual` im Speicher; **nach dem Senden von Runde 2** bleibt
  der Zustand bis Commit oder Supersede (lokaler Timeout allein verwirft
  nicht). Deadline-Prüfung im Presence-Tick.
- Encodings (`i` als u16 LE, `init_id` als u64 LE, `republic_id` roh 32 B)
  mit Byte-Pin-Tests.

### 7.4 Abschluss und Terminal-Block

1. Lokal: `ThresholdKeys`, View-Key, Adresse.
2. Schlüsseldatei über den blockierenden Writer schreiben.
3. **Proposer:** Sitz an Gründungsposition 1; fehlt nach der Deadline ein
   Vorschlag, Position 2 usw. Payload `{op: wallet_created, init,
   transcript, address, threshold, participants, birthday_height}`, eigene
   Proposal-ID.
4. Jeder andere Sitz co-signiert nur bei exaktem Match.
5. **Siegel bei n.**
6. **Erster gewinnt:** die Projektion nimmt den ersten committeten
   `wallet_created` je Init; beim Commit werden Geschwister-Karten desselben
   Inits geschlossen (Vorbild `supersede_stale_vault_cards`), damit kein
   Re-Sign sie wiederbelebt.
7. Projektion prüft: Init existiert mit n Signaturen, `threshold == rule_m`,
   `participants == n`.
8. Abweichende Rechnung: nie signieren; die Kasse entsteht nicht; der Init
   stirbt; neuer `WalletInit` erlaubt.
9. **Restart:** beim Öffnen mit Schlüsseldatei und ungesiegelter Kasse den
   passenden offenen `wallet_created` erneut co-signieren. Wer vor Runde 2
   schloss, sendet beim Öffnen einen Abbruch.

### 7.5 Keine neue Verifikationsregel

- Keine Ablehnung in `verify_chain` / `verify_next` / `fold_one`
  (ältere Builds, Mock-`transfer`-Blöcke in Alt-Republiken, all-or-nothing).
- Der Op-Satz wird bei Propose, Approve, Wire-Ingest und in jedem
  Signierpfad durchgesetzt.
- Die Projektion **ignoriert** deterministisch: andere Wallet-Ops, alles vor
  der markierten Einschaltung, Inits ohne n Signaturen, jeden
  `wallet_created` außer dem ersten je Init.

### 7.6 Recovery und Status

- **Status-Frame** `\x00molt-wstat-v1` (Held/WatchOnly), je Mitglied in
  `TransportState`, Resend-Cursor.
- **Frage/Antwort** `\x00molt-wvask-v1` / `\x00molt-wvresp-v1`: Sitz ohne
  View-Key fragt nach dem Rejoin; Prüfung `view·G` gegen die Adresse.

### 7.7 Scanner

- Off-actor-Task pro offenem Workspace mit `net_scope`, Abbruch in
  `reset_workspace_state`.
- Ab `max(birthday, cursor)`; Rückmeldung per `Net*`-Command.
- Gesehene Output-Keys; letzte Block-Hashes je Höhe für Reorg-Erkennung.
- 20 Bestätigungen; Reorg → zurückspulen oder Rescan ab Birthday.
- `UnsupportedProtocol` → `scan_paused = "update needed"`; Dekodierfehler →
  Daemon-Fehler (sonst könnte ein lügender Daemon die Pause erzwingen).

## 8. Kontrakt-Erweiterungen

- `Command`: `WalletInit` (Tool, Seat); INTERNAL: `NetWalletProbe`,
  `NetWalletFrame`, `NetWalletScan`, `NetWalletStatus`,
  `NetWalletViewAnswer` (Namen beim Bau final; Liste in der Testfunktion
  pflegen).
- Keine neuen `WorkspaceEvent`-Varianten (Runden sind Control-Frames).
- `Event` (Frontend): `WalletDkgProgress`, `WalletCreated { address }`,
  `WalletDkgFailed`, `WalletScanPaused`.
- `SurfaceSnapshot.wallet: Option<WalletView>`.
- MCP: `wallet_init` (`Scope::Seat`). `read_state(wallet)` bleibt Seat; der
  Read-Key sieht die Kasse **nicht** (Read = Wiki + Shared Files,
  Produktentscheidung).
- `[wallet]`: `daemon_url`, `network`. In `Config`, `Settings`, `salvage`,
  `render`. Ältere Binaries lehnen die Config ab — Release-Notes.

## 9. Persistenz

| Datei | Segment | Inhalt | Export | Import |
|---|---|---|---|---|
| `wallet_keys.state` | `u64::MAX − 7` | Share, View-Key, Birthday, Init-ID | prüfen; defekt → **Abbruch** | prüfen; defekt → verwerfen, watch-only |
| `wallet_scan.state` | `u64::MAX − 8` | Cursor, Block-Hashes, gesehene Keys, Outputs | prüfen; defekt → skip + benennen | prüfen; defekt → verwerfen, Rescan |

- Sub-Keys `hkdf32(ws_key, "molt-wallet-keys", id)` /
  `"molt-wallet-scan"`; `encode_frame` + `write_atomic`; Cap
  `READ_CAP_STATE`. `RESERVED_SEGMENT_FLOOR` auf `−8`, Const-Assert mit.
- Schlüsseldatei: einmal geschrieben, `WriterMsg::PersistWalletKeys` +
  `persist_wallet_keys_blocking`.
- **Ausweg (W5):** ein Command `WalletAcknowledgeLoss` (Tool, Seat) legt eine
  defekte Schlüsseldatei als Dot-Datei beiseite (der Export ignoriert
  Dot-Dateien); Sitz watch-only; Exporte laufen wieder.
- **Backup-Ticker:** Backoff bei Schlüssel-Fehler statt Minuten-Retry; eine
  Meldung, nicht eine pro Minute.
- `docs_archive/storage/backup_restore_design.md` §3.2 + Allowlist
  nachziehen. Release-Notes: ältere Builds exportieren ohne Schlüsseldatei
  und lehnen Blobs mit ihr ab.

## 10. TDD-Fahrplan (rote Tests zuerst)

**molt-treasury:**
1. `seats_follow_the_genesis_founding_table` (gleich Vault `seat_x`).
2. `a_three_seat_dkg_agrees_on_key_view_and_address` (Byte-Roundtrip).
3. `the_context_binds_republic_and_init`.
4. `view_key_depends_on_every_contribution`.
5. `a_differing_transcript_aborts`.
6. `two_round_one_frames_from_one_sender_abort`.
7. `an_identity_commitment_aborts`.
8. `a_bad_share_names_its_sender_only_with_proof`.
9. `the_address_is_a_standard_main_address`.
10. `wallet_keys_round_trip_and_zeroize`.
11. `a_repeated_output_key_is_ignored`.
12. `unsupported_protocol_pauses_and_a_decode_error_does_not`.
13. Byte-Pin-Tests aller Layouts.

**molt-storage:**
14. `wallet_keys_and_scan_round_trip_under_their_segments`.
15. `a_damaged_keys_file_fails_the_export`.
16. `a_damaged_keys_file_restores_watch_only`.
17. `a_damaged_scan_file_is_skipped_and_named`.
18. `an_acknowledged_loss_lets_the_export_resume`.

**molt-engine (E2E über MockRelay, Harness `vault_support`):**
19. `wallet_needs_two_to_n_minus_one`.
20. `a_legacy_wallet_enablement_needs_a_fresh_vote`.
21. `one_decline_kills_the_init`.
22. `a_future_birthday_is_declined`.
23. `an_init_sealed_at_m_is_inert`.
24. `a_seat_without_a_daemon_aborts_before_round_one`.
25. `three_seats_found_one_purse` (gleiche Adresse, Siegel bei n, Datei vor
    Signatur).
26. `racing_wallet_created_cards_leave_one_purse`.
27. `a_mock_transfer_block_does_not_break_the_chain` (Alt-Republik).
28. `a_diverging_seat_blocks_the_purse_and_allows_reinit`.
29. `a_restart_after_persist_re_cosigns`.
30. `a_phrase_only_recovery_is_watch_only_and_gets_the_view_key`.
31. `a_backup_restore_keeps_the_share`.
32. `the_purse_survives_a_cut`.
33. `the_read_key_does_not_see_the_wallet`.
34. Co-Equality grün; `is_implemented`-Test angepasst.

**Manuell, `#[ignore]`:** regtest-monerod hinter `MOLT_TEST_MONEROD`
(`network = testnet` bzw. regtest): Mining sichtbar; Close/Reopen hält
Stand.

## 11. UI (Etappe 1)

- `WalletPane` echt: `balance`, `history`, `receive`, `status` (Daemon,
  Höhe, Phase, Shareholder).
- `send`, `settings`: „Ausgeben folgt mit Etappe 2“.
- Einschalt-Dialog: drei Zeilen (Design §3.1).
- Init-Karte: „alle müssen zustimmen und online sein“; Fortschritt;
  Abbruchgrund.
- Verlust-Dialog für die Schlüsseldatei (§9).
- `default_op` (`transfer`) und tote `wl_*` entfernen.
- Kurze Texte, kein Em-Dash. Tests nach `tests/gui/vault.rs`.

## 12. Fork-Vorsorge (FCMP++ + Carrot)

- Scanner hinter eigenem Trait; Carrot-Update = Implementierung tauschen.
- `UnsupportedProtocol` → Pause, Bilanz bleibt.
- Am Fork prüfen: Carrot-fähiges monero-oxide-Release; Legacy-Scan mit
  geteiltem View-Key unverändert?
- `ThresholdKeys` und Adresse bleiben; nichts migrieren.

## 13. Etappe 2 — Bedingungen, kein Bau

Erst, wenn alle drei gelten (Design §8): SA+L-Signing über
`ThresholdKeys<Ed25519>` in veröffentlichter monero-wallet; Beweis oder
Audit; Fork-Höhen fest. Bis dahin `can_spend = false`, kein `sign.rs`.

## 14. Ausführungsreihenfolge

1. [x] Design rev 2 + Plan rev 2 — zur Ratifizierung.
2. [ ] §6 Dependency-Lock + bare `molt-treasury` + pedpop-Review.
3. [ ] §10.1–13 molt-treasury.
4. [ ] §9 + §10.14–18 Storage, Backup-Doku.
5. [ ] §8 Kontrakt, MCP, Config; Co-Equality grün.
6. [ ] §7.1–7.2 + §7.5; §10.19–23, 27.
7. [ ] §7.3–7.4 + §7.6; §10.24–26, 28–32.
8. [ ] §7.7 Scanner; manueller regtest-Lauf.
9. [ ] §11 UI.
10. [ ] clippy pro Crate = 0; Suiten grün; Review über den Gesamt-Diff;
    master.

## 15. Bekannte Fallen

- Zeilennummern driften — Symbole greppen.
- `dkg-pedpop` ohne `schnorr-signatures`-Pin baut nicht.
- **Keine neue Regel in der Chain-Verifikation** (§7.5).
- Init und Terminal-Block siegeln bei **n**; der Terminal-Block braucht eine
  eigene ID; Geschwister-Karten schließen.
- Ein-Decline- und Birthday-Check in **jedem** Signierpfad.
- Ritual nur bei live angewandtem Init starten, nie beim Replay.
- Kind-445-Events werden nicht gechunkt; Control-Frames sind klein.
- `WeakSender` beendet keine Workspace-Tasks; `net_scope`.
- `MemberSeen` ist ein No-op.
- Nicht `monero-simple-request-rpc`, nicht `GuaranteedScanner`, nicht
  `multisig`-Feature in Etappe 1.
- Read-Key sieht die Kasse nicht.
- Checkpoint-Layouts nicht anfassen; `wallet_created` akkumuliert (ein
  Eintrag).

## 16. Definition of Done (Etappe 1)

- Tests aus §10 grün; regtest-Lauf einmal manuell, Protokoll im Commit.
- clippy pro Crate = 0; `dev-ui.sh build` sauber.
- Manuell über drei Instanzen: Vote, Init, gleiche Adresse, Mining
  sichtbar, Close/Reopen, Export/Import behält den Teil,
  Phrase-Recovery zeigt watch-only.
- Review über den Gesamt-Diff, Findings gefixt, grün auf master.
- Doc-Status aktualisiert, `check-doc-refs.py` sauber.

## 17. Änderungen gegenüber Revision 1

- Scope Etappe 1; Etappe 2 gated (W2).
- Stack: ohne `multisig` und `modular-frost`; `monero-daemon-rpc`;
  `schnorr-signatures`-Pin.
- View-Key aus Beitragsrunde; kein gespeicherter Outgoing-Key.
- Init und Terminal-Block siegeln bei n; Transkript-Hash; DKG-Kontext;
  Equivocation-Erkennung; Rundenzustand nach Runde 2 behalten.
- Runden als Control-Frames; Readiness-Runde.
- Ein Proposer, erster gewinnt, Geschwister schließen; eigene ID.
- Keine neue Verifikationsregel; Projektion ignoriert Fremdes.
- Birthday-Prüfung durch jeden Approver.
- Einschaltgrenze `2 ≤ m ≤ n − 1`; Marker `wallet_rev`.
- Zwei Dateien (Segmente −7/−8); Abbruch + Ausweg für die
  Schlüsseldatei; Ticker-Backoff.
- Standard-Scanner, gesehene Keys, Block-Hashes, 20 Bestätigungen,
  `UnsupportedProtocol` = Pause.
- Recovery per Frage/Antwort, Prüfung gegen die Adresse; Status per
  Control-Frame.
- RPC über `s3::http` + `Dialer`; lokal per `relay_kind`.
- Read-Key sieht die Kasse nicht.
- Netzwerk konfigurierbar.
- Sitzordnung aus den Genesis-Gründungsidentitäten, geteilt mit dem Vault.
- Alle Anker auf `9573c3a`; Fork-Vorsorge neu.
