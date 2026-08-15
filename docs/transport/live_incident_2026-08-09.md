# Live incident 2026-08-09 — three-node Nostr test, post-recovery divergence

Status: **all defects addressed on master** — five fixed on 2026-08-09
(proposal-card resurrect, chat-nav pin deadlock, pool edit lost on reopen,
declines never converged, applied cards lost their voters), §2 and §3
closed on 2026-08-15 (see their sections); field verification of the
2026-08-15 fixes on the live three-node setup is pending. Source: the user's real three-node setup (Albert =
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

## 2. ADDRESSED (field verification pending) — the rejoiner was inbound-deaf after a double recovery

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

**2026-08-15 progress.** The field storm recurred (generations 28..36 —
same signature, same never-healed state). The repro landed:
`nostr_recovery.rs::a_double_recovery_of_the_same_seat_still_converges_both_ways`
drives found → recover → device dies again → recover again → chat BOTH
ways, all through the public surface. Verdict: the clean double recovery
REPRODUCES the SecretReuseError signature and the "re-offering the tail"
round, but CONVERGES — so the storm's benign half is relay RE-DELIVERY of
already-consumed frames (the envelope seq sits inside the ciphertext, so
the seq-dedup can only run after a decrypt that must fail). Shipped
mitigation: `group_runtime::SeenCiphertexts` — a bounded ring of consumed
ciphertext hashes turns exact re-deliveries around BEFORE the ratchet is
asked (held FutureEpoch/Opaque frames never enter the ring and stay
retryable). What the clean repro does NOT show is the field's permanent
deafness — the remaining suspect is the resend BUDGET holding the healing
tail (field: "budget spent — holding the tail floor=51" 18 s after start)
plus whatever the two Restored blocks did to the sender ratchets.

**Budget audit (2026-08-15, done).** The "spent 18 s after start" line is
the budget's PERSISTENCE working as designed (a crash loop must not buy a
fresh allowance per start) — the previous churny hour had burned all 12
rounds against the dead peer. The real defect: the budget also gated the
EVIDENCE-driven heal. When the recovered incarnation's first claim sheets
arrived (proof: a listening, still-lagging peer — its floor cannot advance
past what the old incarnation proved, so they land in `apply_group_ack`'s
no-progress arm), the outbox held the tail for up to the rest of the hour,
which the user reads as permanent deafness. Fix on master: a claim sheet
from a peer that still TRAILS the publish cursor latches
`GroupCursor.heal_evidence` (a caught-up peer's sheet proves nothing to
heal — review finding); a spent budget then still grants ONE
`consume_heal_round` per hour window (bounded — a blind stall loop buys
nothing, a normal round clears the latch because it just served it).
Keystones: `group_runtime::tests::a_claim_sheet_grants_one_heal_round_past_a_spent_budget`,
`only_a_lagging_peers_sheet_latches_heal_evidence`,
`a_normal_resend_round_clears_the_heal_evidence`.

## 3. FIXED — survivors' outboxes churned; chat between HEALTHY nodes stalled

**Symptom:** a fresh chat message between the two healthy nodes did not
arrive within tens of seconds; the user experiences "chat kaputt" on nodes
that are not themselves damaged.

**Evidence:** 44 (Veronica) / 76 (Albert) `relay runtime` constructions in
minutes — every group-frame publish builds a FRESH `RelayRuntime` (Tor
circuit + WS + TLS each, ~2s), and the resend rounds against Eduard's dead
acks saturate that path; new sends queue behind the resend cursor. Healthy
Eduard-side count in the same window: 1.

**Fix (master, 2026-08-15 — direction (a), user decision):**
`relay_runtime::PublishPool` — one persistent, deliberately UNAUTHENTICATED
connection per relay (the §7.5 rule stands: an authed publish channel would
link every ephemeral-key event to the member), shared by every clone of the
`GroupChannel`, so outbox, ack task and file plane ride kept sockets. A
broken/idle-closed socket redials exactly once on the next publish; a
relay's verdict (refusal, auth demand) never redials — it is a live answer.
≥1-OK semantics, the size gate and the report shape are the shared
`finish_publish_report` reduction, byte-compatible with the per-dial path
(which remains for ritual sends and probes). Keystones:
`tests/publish_pool.rs` (three publishes = ONE connection, red-verified at
3; a cut connection redials exactly once), plus both delivery suites and
the recovery capstone over the pooled path.

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

## 6. FIXED — a decline never crossed the wire, so a declined vote never died

Found in the evening continuation of the same three-node run (fresh
republic "My Company 2", 2-of-3, seats `links`/`mitte`/`rechts`).

**Symptom:** two proposals (`set_name`, `set_image`) proposed by `links`
were declined by BOTH other members — globally dead (max 1 of 2 possible
approvals) — yet stayed pending on every node forever; no click ends them.
MCP evidence: each node's card listed only its OWN decline (`mitte`'s node:
mitte=declined, rechts=open; `rechts`'s node: the mirror image), 1/2
approvals everywhere.

**Root cause:** `crosses_wire` sends `Declined` (the comment even states
votes must converge), but `deliver_gated` had NO receive arm for it — the
envelope was accepted, ACKed and dropped ("event over the wire not acted on
here"). Every node judged the vote winnable from its local view, so it
never turned Rejected anywhere. Once dropped, the at-least-once guarantee
is spent: the decline never comes back on its own.

**Fix (master):** one decline choke point (`register_decline`: dedup per
member, Rejected when declines > n − m) fed by the log applier, the new
wire arm and a bounded park for declines whose proposal is not known yet
(G7 orders per sender only; an own-log decline also replays before the WP2
re-serve returns its card). The wire arm counts a decline ONLY for the
link identity — it carries no signature, so a body claiming another member
is dropped. The WP2 re-serve now also carries the node's OWN declines
(open cards, rejected cards, parked voices), so the terminal verdict
reaches nodes that were closed while the vote died — that plus the
existing reopen catch-up probe heals the stuck live pair after a restart
of all three nodes (one member may need to decline once more if its node
never held the card). Keystones:
`chain.rs::wire_declines_converge_and_reject_at_the_veto_threshold`,
`chain.rs::a_decline_ahead_of_its_proposal_parks_and_registers`,
`chain.rs::open_governance_reserves_the_own_decline`,
`org_effective.rs::a_declined_vote_dies_on_every_node`,
`org_effective.rs::a_rejected_verdict_reaches_a_reopened_node` — all
red-verified without the receive arm.

## 7. FIXED — an applied card showed 0 approvals and no voters

**Symptom:** every decided (applied) vote in the history read "0/2
approvals, every pill open" — who voted was gone, on all three nodes, even
though `read_chain` plainly listed the two signers per block.

**Root cause:** the proposal view built its voting pills from the
ephemeral signature collection (`pending_sigs`), which is cleared the
moment a block seals — and on a reopen the settle-against-chain path
reconstructs applied cards with no vote data at all.

**Fix (master):** for an Applied chain proposal the view resolves the
sealed block by proposal id and reports its signers (count, pills,
`approved_by_me`); a pruned block (WP4) honestly leaves the pills open.
Keystone: `chain.rs::an_applied_card_reports_the_block_signers`.

## Repro assets

- Workspace snapshots from the live incident (pre-fix, all three nodes):
  job-local `evidence/ws{1,2,3}` copies taken 15:47 — re-copy from the real
  roots if a fresh snapshot is needed; the moment is otherwise gone.
- Headless three-node drive: `moltd` with a per-node config (real
  `workspace_dir`, own MCP port), `open_workspace`, then
  `relay_clearnet_session {url, unlock:true}` per session — newline JSON-RPC,
  `initialize {token}` first.
