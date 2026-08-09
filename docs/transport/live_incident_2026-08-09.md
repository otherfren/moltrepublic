# Live incident 2026-08-09 — three-node Nostr test, post-recovery divergence

Status: **three defects fixed on master** (proposal-card resurrect, chat-nav
pin deadlock, pool edit lost on reopen), **two open** (each with evidence
below). Source: the user's real three-node setup (Albert =
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

## 4. FIXED — the "GUI freeze" was a chat-nav pin deadlock

**Symptom:** "UI bleibt hängen und bleibt unbenutzbar" (5+ occurrences),
reliably after an approval arrived and was accepted; headless via MCP kept
working. Alert sounds still played — which proved the Slint event loop AND
the engine-event mirror task alive, so this was never a frozen window.

**Root cause:** accepting an approval selects the decision's discussion, an
Organization patch channel. `nav-expanded` (app.slint) then deliberately
pins the Organization section open while the surface is "chat" — but
clicking the "Chat" nav row only re-selects the already-selected chat
surface, so nothing changes and the chat section can never be expanded
again. A navigation deadlock, not a freeze; every thread idled normally in
`ep_poll` the whole time.

**Fix (master):** the "Chat" surface-row click resets the channel filter to
the group channel when an Organization decision discussion is the active
filter — the click IS the way out of the pin. Slint-side handler; not
reachable from a Rust test (inline .slint), validated by the compile plus
the live setup.

**Open footnote:** the accepting node's GUI process grows a second, idle
4-worker tokio runtime at approval time (0 CPU forever, seen twice on
`config.toml`'s node, absent on the others). Harmless so far but
unexplained — worth identifying the spawner when next in that code path.

## 5. FIXED — a sealed pool edit did not survive the reopen

**Symptom:** the 2→1 `set_relays` vote sealed (h=2, Albert+Veronica; one
decline does not block 2-of-3) and applied live — but after closing and
reopening the workspaces every node dialed and showed the original
two-relay pool again: "the vote had no effect".

**Root cause:** the reopen adopts the chain, then overwrites `nostr.relays`
from the persisted `transport.state` copy and builds the group runtime from
that; `adopt_pool_change` (R6) ran only on the live append path, and the
Status view serves `nostr.relays` — so both the dialed pool and the UI
regressed on every restart.

**Fix (master):** `cmd_open_workspace` runs `adopt_pool_change` before
building the group runtime — the chain-ratified pool outranks the persisted
copy on every open (no runtime is up yet, so it only adopts the list).
Keystone: `org_effective.rs::a_sealed_pool_edit_survives_the_reopen`, red at
the reopen assert before the fix.

## Repro assets

- Workspace snapshots from the live incident (pre-fix, all three nodes):
  job-local `evidence/ws{1,2,3}` copies taken 15:47 — re-copy from the real
  roots if a fresh snapshot is needed; the moment is otherwise gone.
- Headless three-node drive: `moltd` with a per-node config (real
  `workspace_dir`, own MCP port), `open_workspace`, then
  `relay_clearnet_session {url, unlock:true}` per session — newline JSON-RPC,
  `initialize {token}` first.
