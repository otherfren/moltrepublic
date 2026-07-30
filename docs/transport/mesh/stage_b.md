# Stage B — Selbstheilende Leitungen + ehrliche Transport-Health

> **Historical (2026-07-30):** the SMP transport this document describes was
> removed in etappe N-demo of the Nostr transport replacement
> (`docs/transport/nostr_transport_marmot.md`), and with it the SKEY fix and
> the N-queue redundancy machinery. Kept as the record of why SMP was left.
> The per-peer resubscribe watchdog itself SURVIVES, reduced to a
> single-queue redial loop in the supervisor (`recv_watchdog_task`).

Status: **UMGESETZT** (2026-07-19, zusammen mit dem SKEY-Restart-Fix).
Dieses Dokument ist der Design-Record; die Historie des Vorfalls und der
Diagnose steht im Abschlussteil.

## 1 Problem

Zwei verwandte Defekte machten Leitungen still tot:

1. **Sender-Keys überlebten den Neustart nicht** (der 2026-07-19-Vorfall):
   `SmpTransport::send` sicherte eine Peer-Queue beim ersten Send per
   `SKEY` mit einem frisch zufälligen Ed25519-Key. Der Mesh-Up-Persist
   exportierte die Creds aber VOR dem ersten Send (send_keys leer), und nur
   ein Clean Close nach Traffic exportierte sie nach. Ein hart gekillter
   Sitz mintete nach dem Reopen frische Keys → der Server lehnt ein SKEY
   mit anderem Key auf einer gesicherten Queue mit `ERR AUTH` ab → die
   Outbox retryte endlos im Backoff. Eine WARN-Zeile, sonst still;
   `net_health` blieb Ok.
2. **Gestorbene Subscriptions blieben tot**: Subscribes feuerten genau
   einmal beim Supervisor-Spawn; endete der Delivery-Stream (SMP-recv-Loop
   stirbt bei jedem Fehler), war der Sitz für die Session taub. Dazu kam
   ein Zombie-Modus: ein server-seitiges `END` wurde verschluckt — die
   Verbindung wartete für immer auf eine Subscription, die der Server
   längst beendet hatte (null Log-Zeilen).

## 2 Fix Teil 1 — deterministische Sender-Keys (molt-net)

- `SmpState.sender_seed: Option<[u8; 32]>`, gemintet per `getrandom` in
  `SmpTransport::with_dialer`. Bei RNG-Fehler bleibt er `None` und der
  erste Send schlägt ehrlich fehl (`NetError::Crypto`) — NIE ein
  konstanter Fallback (vorhersagbare Keys wären vor-SKEYbar).
- Sender-Key pro Queue = `Ed25519(HMAC-SHA256(seed,
  "molt-smp-sender-v1" ‖ sender_id))` (`derive_sender_key`). Ein
  reopener Transport leitet damit DENSELBEN Key wieder her — die
  Persist-Reihenfolge ist egal, weil der Seed ab Transport-Geburt existiert
  und schon der Mesh-Up-Persist ihn trägt.
- `secure_as_sender(sender_id, sk)` nimmt den Key jetzt als Parameter.
- Send-Flow: Cache-Hit → signieren; sonst Key ableiten, `SKEY` versuchen;
  wird das SKEY abgelehnt, wird **trotzdem der signierte SEND versucht** —
  das Send-Urteil des Servers ist autoritativ (deckt Server ab, die ein
  Same-Key-Re-SKEY ablehnen; ein echter Fremd-Key-Fall scheitert dann
  ehrlich als Send-Fehler → Backoff → Degraded). Gecacht wird bei
  SKEY-Ok sofort, im Fallback-Fall NUR nach Send-Erfolg.
- Creds additiv **V2**: `PersistedCreds.sender_seed` am Ende; ein V1-Blob
  (Prä-Fix) fällt im Import auf das alte Layout zurück (recv+send_keys
  adoptiert, eigener frischer Seed bleibt; ein einmal adoptierter Seed wird
  von einem späteren V1-Import nie verworfen). Ein V2-Export bleibt für
  einen V1-Leser lesbar (bincode v1 toleriert Trailing-Bytes — im
  Unit-Test gepinnt).
- Live-Server-Pin: `skey_rederivation_after_reopen_keeps_sending`
  (molt-net, `--ignored`): Export VOR jedem Send, zwei frische
  Incarnationen senden nacheinander mit denselben Creds — beide kommen an.

**Grenze:** Queues, die eine Prä-Fix-Session mit einem verlorenen
Zufalls-Key gesichert hat, bleiben in Senderichtung unrettbar (der Server
kennt nur den verlorenen Key). Solche Legs brauchen Queue-Rotation /
Recovery (offen, §6); Stage B macht sie wenigstens sichtbar.

## 3 Fix Teil 2 — Resubscribe-Watchdog (molt-net/supervisor)

Pro Peer ersetzt `recv_watchdog_task` das einmalige subscribe+`recv_task`:

```
loop {
    rx = erstes Prebuild-Subscribe ODER transport.subscribe(...)
         (Fehler → link_down(reason), capped jittered Backoff, retry)
    link_up(peer); attempt = 0
    recv_task(rx) →
        EngineGone   → return          (Aktor weg)
        StreamEnded  → link_down("subscription ended — resubscribing")
    Backoff, weiter
}
```

- Backoff: `backoff_ms` mit `retry_base_ms`→`retry_cap_ms` aus `NetConfig`
  (prod 1 s → 2 min, Tests 20 → 100 ms).
- Ein SMP-`subscribe` wählt selbst eine frische Verbindung → der Watchdog
  re-dialt implizit. Reassembler/Reorder/Epoch-Puffer sind pro Inkarnation
  frisch; Cursor liegen im geteilten `state`-Arc; Unacktes redelivert der
  Server an den nächsten Subscriber.
- Das Prebuild (Semaphore 4) bleibt für die ERSTEN Subscribes; ein
  fehlgeschlagenes erstes Subscribe ist nicht mehr fatal.
- `END` vom Server beendet jetzt den recv-Stream (vorher Zombie) — der
  Watchdog übernimmt.
- Send-Pfad: nichts zu bauen — der Pool re-dialt bei kaputter Verbindung,
  `send_one` backofft gecappt; neu sind nur die Signale (§4).

## 4 Ehrliche Health (Sink → Engine → `net_health`)

- `EngineSink` (additiv, Default-No-op): `link_up(member)`,
  `link_down(member, reason)`, `send_ok(member)`. `send_one` feuert
  `send_ok` beim Backoff-Ausstieg; `send_failed` weiter beim ersten
  Fehlversuch.
- Drei neue engine-interne Commands (INTERNAL-Liste, generation-geprüft):
  `NetLinkUp`, `NetLinkDown { reason }`, `NetSendOk`.
- Engine-State: `net_link_down: BTreeMap<Member, Grund>` (Inbound-Leg) und
  `net_send_stuck: BTreeMap<Member, Grund>` (Outbound-Leg;
  `cmd_net_send_failed` setzt ihn — der ewige SKEY-Backoff ist damit
  sichtbar). `recompute_net_health()`:
  - `Down` (Open-/Config-Verdikt: fail-closed Dialer, detached Reopen)
    wird NIE überschrieben.
  - beide Maps leer → `Ok`; sonst `Degraded { "link to <m>: <r>;
    sends to <m>: <r>; …" }`. Emit nur bei Änderung.
  - Workspace-Wechsel/Close (`reset_workspace_state`) leert beide Maps.
- **Open ist ehrlich**: `build_real_net_shared` markiert nach dem
  Supervisor-Spawn jedes Leg `"connecting"` → die Pill ist kurz amber, bis
  jeder Watchdog sein Subscribe bestätigt hat. `Ok` heißt seitdem wirklich
  „jeder Mesh-Link lebt" (mesh_resume-Test pollt entsprechend).
- GUI/MCP: keine Änderung nötig — `NetHealth::Degraded` + Amber-Pill +
  Tooltip existierten bereits end-to-end.
- Stille Drops sind laut: nicht dekodierbare Inbound-MLS-Nachrichten
  (Replay/Proposal/Garbage) WARNen mit laufendem Zähler; lokale
  Send-Skips zählen mit; der Mesh-Bootstrap loggt verworfene
  Announcements mit Grund.

## 5 Tests (alle auf master grün)

- molt-net Unit: Seed-Ableitung deterministisch+queue-gebunden; V2-Creds
  round-trip; V1-Fallback; V2-bleibt-V1-lesbar; V1 verwirft nie einen
  adoptierten Seed.
- molt-net `--ignored` (Live-SMP): `skey_rederivation_after_reopen_keeps_sending`.
- molt-net supervisor (loopback, `NetConfig::fast` +
  `LoopbackHub::sever_subscriptions()`-Seam):
  `a_severed_subscription_resubscribes_and_resumes_delivery`,
  `a_healed_send_after_backoff_fires_send_ok`.
- molt-engine Unit: Ok→Degraded→Ok (Gründe nennen Peers), Down-Guard,
  Reset-Clears.
- molt-engine `--ignored` (Live-SMP, der Vorfalls-Beweis):
  `mesh_restart_over_smp` — echte 2-of-3-Gründung über smp.konkin.io,
  6-Richtungen-Baseline, Neustart (A/B clean, C hart), volle Matrix danach
  (C über die Heal-Garantie, §6 Punkt 2).

## 6 Bewusst offen

1. **Queue-Rotation/Heilung endgültig toter Sender-Legs** (Prä-Fix-Queues
   mit verlorenem Zufalls-Key): braucht Re-Provisioning der betroffenen
   Queue-Paare (Mesh-Extension-artig) oder das Recovery-Ritual. Die
   Live-Republik des Users hat solche Legs; sie erscheinen jetzt als
   Degraded statt still Ok.
2. **Per-Drain-MLS-Persist** (volle Crash-Safety des Ratchets): ein hart
   gekillter Sitz resumt vom letzten Persist-Stand; Nachrichten im
   regressierten Fenster re-nutzen Sender-Generationen und werden vom Peer
   replay-verworfen (jetzt LAUT, mit Zähler). Die Leitung heilt ab der
   nächsten Nachricht (im Matrix-Test gepinnt); die Fenster-Nachrichten
   selbst sind verloren, bis der Persist pro Drain landet.
3. **Transiente Ein-Richtung-Ausfälle über Live-SMP**: In Einzelläufen
   starb VOR dem Neustart sporadisch genau eine Founder-Outbound-Richtung
   ohne jedes Signal (Sends server-bestätigt, Subscription live, kein
   Discard). Verdächtige: Server-seitiges Verhalten (END-Zombie ist seit
   diesem Change ausgeschlossen; Queue-Tag-Debug-Logging liegt bereit,
   um den nächsten Fall einem Hop zuzuordnen). Da unsere SMP-Acks lazy
   beim NÄCHSTEN recv feuern, redelivert der Server eine app-seitig
   verlorene Nachricht nicht an dieselbe Verbindung — Ack-after-fsync
   (T3-Notiz) bleibt der zugehörige offene Punkt.
4. `net_health` ist session-global (ein offener Workspace) — unverändert.

## 7 Vorfalls-Historie (Kurzfassung)

3-Knoten-Republik, alle drei neu gestartet (2× clean, 1× hard kill);
danach lieferte 1 von 6 Richtungen. Stderr des gekillten Knotens:
`SKEY rejected: ERR AUTH` im Endlos-Backoff. Diagnose + Fix-Design in
fixme.md (Arbeitsdokument dieser Session, im Landing aufgegangen in dieses
Dokument). Der Repro-Test `mesh_restart_over_smp.rs` fiel vor dem Fix mit
exakt `dead directions: ["member-c → founder-a", "member-c → member-b"]`
und wurde mit dem Fix grün.
