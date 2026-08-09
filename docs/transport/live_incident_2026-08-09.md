# Live incident 2026-08-09 — three-node Nostr test, post-recovery divergence

Status: **one defect fixed on master** (proposal-card resurrect), **three open**
(each with evidence below). Source: the user's real three-node setup (Albert =
`config.toml`, Eduard = `config2.toml`, Veronica = `config3.toml`; republic
"Our Software Company", 2-of-3, relays `wss://nos.lol` + one onion, via local
Tor). All findings were taken from the LIVE engines over MCP and from headless
reruns on copies of the real workspaces — not from code reading.

## Timeline (all 2026-08-09, local)

- 14:11 — Eduard's node last received foreign traffic (pre-restart era).
- 15:28-15:29 — all three nodes restarted (GUI mode).
- ~15:33 — first recovery of Eduard (`Membership/Restored`, block h=3).
- 15:37:04 — Eduard's node re-created via recovery link (second recovery,
  block h=4). His own chain never received h=3/h=4.
- 15:36-15:38 — survivors exchange messages Eduard never receives.
- 15:47+ — diagnosis over MCP; later headless reruns with the fixed build.

## 1. FIXED — a reopen resurrected decided proposal cards

**Symptom:** a long-decided vote ("proposal … müsste längst weg sein") shows
as an open card again; on Albert's engine `set_relays` (prop 2) read
`proposed` with contradictory votes while his OWN chain carried it applied at
h=2 with his own signature.

**Root cause:** `open_stored_workspace` replays the proposal cards from the
persisted gossip log first and adopts the chain second; nothing settled the
cards against the chain, so every restart resurrected every card whose seal
happened in an earlier session. Offline-reproduced on a copy of the real
workspace before the fix; verified settled after it.

**Fix (master):** `settle_cards_against_chain` runs at the end of
`apply_chain_to_state` (Applied blocks by `proposal_id`, blob `consumed_ids`
below a checkpoint cut, membership blocks by content), plus a
`receive_proposed` guard refusing ids the walk already consumed (the live
resend twin). Keystone: `chain::tests::adopting_a_chain_settles_replayed_proposal_cards`.

## 2. OPEN — the rejoiner is inbound-deaf after a double recovery

**Symptom:** Eduard's node receives nothing from the survivors since his
15:37 recovery (missing: 3 group messages, the 4th patch-2 message, his own
membership blocks h=3/h=4) while his OWN sends reach both survivors.

**Evidence:**
- Eduard headless log: `ERROR molt_net::group_runtime: MLS-framing a group
  frame failed — skipped seq=73` — his outbox cannot encrypt; the frame is
  skipped (lost by design, loudly).
- Eduard: `WARN the resend budget for this hour is spent — holding the tail
  floor=51` 18 seconds after process start — the budget was spent instantly
  (persisted across restart, or skipped frames are counted as spends: check
  `group_runtime` budget accounting).
- Albert inbound: `openmls SecretReuseError, ciphertext generation out of
  bounds 21..16` storms — replayed frames on consumed ratchet generations.
- Eduard's chain top is h=2: he never received the very blocks that restored
  him (they sealed around/after his handover).

**Suspected root:** MLS state divergence out of TWO recoveries of the same
seat in one hour (blocks h=3 and h=4, signers `[Albert]` each) — the N3 §1
concurrent-commit / rewind area, plus the handover racing the second rekey.
Next step: reproduce a double recovery of the same seat in
`nostr_recovery.rs` (recover, then recover again, then assert both directions
of chat converge) and audit the resend-budget accounting.

## 3. OPEN — survivors' outboxes churn; chat between HEALTHY nodes stalls

**Symptom:** a fresh chat message between the two healthy nodes did not
arrive within tens of seconds; the user experiences "chat kaputt" on nodes
that are not themselves damaged.

**Evidence:** 44 (Veronica) / 76 (Albert) `relay runtime` constructions in
minutes — every group-frame publish builds a FRESH `RelayRuntime` (Tor
circuit + WS + TLS each, ~2s), and the resend rounds against Eduard's dead
acks saturate that path; new sends queue behind the resend cursor. Healthy
Eduard-side count in the same window: 1.

**Direction:** (a) reuse one persistent publish channel for the outbox
instead of a fresh runtime per publish, or (b) at least de-prioritize resend
rounds behind fresh sends; plus the §2 budget fix so a deaf peer cannot
starve the group.

## 4. OPEN — the GUI freezes while the engine stays healthy (recurring)

**Symptom:** "UI bleibt hängen und bleibt unbenutzbar" (5+ occurrences);
headless via MCP keeps working. During the frozen state every thread of all
three GUI processes idled normally (main loop in `ep_poll`, no spinner, no
futex deadlock) — so the window still pumps events but stops reflecting
state.

**Lead:** both SURVIVOR GUI processes (and only they) grew a second, idle
4-worker tokio runtime at ~15:33 — the moment of the first recovery — which
the restarted Eduard GUI lacked. Whatever spawned it runs in the
recovery/notification path of a GUI process and is the best marker for where
the state-push pipeline died. ptrace is blocked on this host (Yama scope 1),
so the next freeze needs either `sudo gdb -p` or a debug build with the
push-loop instrumented (log every `upgrade_in_event_loop` failure instead of
discarding it).

## Repro assets

- Workspace snapshots from the live incident (pre-fix, all three nodes):
  job-local `evidence/ws{1,2,3}` copies taken 15:47 — re-copy from the real
  roots if a fresh snapshot is needed; the moment is otherwise gone.
- Headless three-node drive: `moltd` with a per-node config (real
  `workspace_dir`, own MCP port), `open_workspace`, then
  `relay_clearnet_session {url, unlock:true}` per session — newline JSON-RPC,
  `initialize {token}` first.
