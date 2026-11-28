# Finishing the rejoin

**Status: PLAN, STARTED; release v0.1.4, BEFORE Robo-Scrum (user
decision 2026-11-25).** Inventory taken on tag v0.1.3 (`3ad6790d`);
re-baselined 2026-11-27 on master `4e4256a1`, after the user decided
every question in §F. Fork prevention is REJECTED and not part of this
release: forks below the threshold stay possible, are healed and made
visible, and are counted in the field. Built on master: 5a, 5b, 5c
(engine side). Merged 2026-11-28: the base trickle and the close drain
(5d, 20; `cc9febf1`), the evidence exchange (§C.8; `d2fb9442`). Built
on side branches, not merged (§D2): the accepted window's cap deadlock
(25), the durable pending re-key (C.1), the governance gossip flood
(26), the branch choice's capability gate, `amnesia_ack`, pause rule and
keystones (step 7). Parked by recommendation after its review: the lag
guard (§C.9). Questions Q11
and Q12 decided 2026-11-28: a displaced restore is proposed again
(§C.10). Field evidence from a second seat's dump of the
same day: §A3 (gaps 25 to 29, questions Q11 and Q12).
"Rejoin" means three ways a seat comes back:
- R: restored on a new device by link;
- O: reopened after downtime;
- S: back from a dropped branch.

## A. Definition of done (operator's view)

| # | statement (each a test) |
|---|---|
| R1 | A seat restored by link ends with the republic's head, a working group key, the wiki and vault the republic holds, and can propose, vote, seal and co-sign the next cut |
| R2 | R1 holds through every disturbance below WITHOUT a second link, within a bound set only by relay round trips and the operators' own acts: no fixed give-up. (Field, §A2: no seat was ever lost; a NEW link always brought it level in 6-8 s. What is unfinished is that it does not come back by itself.) |
| R3 | When it cannot finish, both the coordinator and the device say WHY and WHAT to do, in one line, in status and over MCP |
| O1 | A seat reopened after any downtime receives every frame the republic still holds for it, without an operator's resync |
| O2 | A reopened seat whose window or epoch is behind says so (`behind`, `mls_behind`) instead of going quiet; a restored device counts no frame from before its own incarnation as a fault |
| S1 | A seat left on a dropped branch is told, stops writing, and returns by link to the chosen branch with R1's result |
| S2 | A device the chain no longer anchors does not stay in the group silently, and the republic never re-keys to a device that is not alive |
| F1 | A file the republic shows as persisted is held by at least one seat besides its uploader's device, or says that it is not |

Disturbances R2 must survive:

| disturbance | today |
|---|---|
| the coordinator restarts or crashes at any point | field: killed at `recover_ready` NOT level (rejoiner `link outdated`, survivors keep `restore_unfinished`); killed after seal/re-key/Welcome level; between persist and publish NOT RUN (B.1) |
| either side's relay consent toggles | field: coordinator's consent off after `ready` NOT level even after it returns (B.2); the rejoiner's side NOT RUN |
| a fork displaces the restore block | device stays in the group (B.3) |
| the old device is still running | field: mint refused 150 s (`seat still active`), the old device shows nothing (B.6); once restored, evicted with `seat-evicted` (built) |
| the rejoiner restarts mid-way | field: after its request or the anchor NOT level and the republic re-keys to the dead device (B.21); after the Welcome or in catch-up level 0.3-2.9 s after restart |
| two seats restore at once | field: level; same coordinator 34 s, two coordinators 102 s (a 60 s fallback-sealer wait) (B.17) |
| a cut before or after the restore | field (fresh): a cut sealed after a restore, the next auto-proposed; real history: two devices at 0 wiki documents after 10 min (B.5d) |
| mixed versions (v0.1.3 peers) | per step below |
| a second restore of the same seat | field: level 7.3 s; the coordinator-crash race of `docs/transport/pending_rekey_plan.md` §1 (B.4) NOT RUN |

Not part of "done":
- chat history (ephemeral by design, `docs_archive/ritual/recovery_ritual.md`);
- the own uploads' bytes, which are served only while the device knows the path (`files_state.rs:301-304`);
- mirrored pieces;
- the wallet key part (phrase recovery is watch-only, `docs_archive/chain/wallet_treasury_design.md`).

The Settings and README say so in one line each.

## A2. Field matrix on v0.1.3 (2026-11-25)

The released v0.1.3 binary, on fresh 2-of-3 and 3-of-5 republics (about
40 blocks, 3 wiki pages, one persisted share); some cases on a copy of
a real history. Counts and shapes only.

| case | result | time to level | operator sees |
|---|---|---|---|
| plain restore | level (both shapes, real history) | 5-8 s; 12.5 s real | mint refused `seat still active` 116-145 s after the old device stopped; the new device's health says `N new messages unreadable` (9-9,260) forever |
| restored seat as the next coordinator | level | 7-8 s | as above |
| old device still running | mint refused 150 s | - | coordinator `seat still active`; the old device shows nothing |
| coordinator killed at `recover_ready` | NOT level | never (900 s watched) | rejoiner `link outdated`; every survivor `restore_unfinished`. 2-of-3: `Restored` already sealed; 3-of-5: not |
| coordinator killed after seal / re-key / Welcome | level | 6-8 s | - |
| rejoiner killed after its request or the anchor | NOT level, worst case | never | sealed and re-keyed to the dead device (epoch +1); survivors hold that seat's stream; restarted device has no workspace; same link refused |
| rejoiner killed after the Welcome or in catch-up | level | 0.3-2.9 s after restart | - |
| coordinator's relay consent off after `ready` | NOT level, also after consent returns | never | `recover-interrupted`; rejoiner `link outdated` |
| relay frozen 60 s | level | the delay | - |
| two seats at once | level | 34 s (one coordinator), 102 s (two; a 60 s fallback sealer) | - |
| same seat twice | level | 7.3 s | - |
| restore during a 60-card burst | level, no fork | 288 s | new device `2002 new messages unreadable` |
| cut after a restore (fresh) | sealed; the next auto-proposed | - | - |
| content after 10 min (fresh) | wiki complete, the persisted file downloads in 1 s, a files vote approved by the restored seat; BUT a persisted share whose uploader was restored: `no longer available` | - | - |
| content after 10 min (real history) | two devices at 0 wiki documents, the third 500+ (one not restored in that run) | - | Memory 0 pages |
| reopen after a clean stop (5 / 50 blocks) | level | 0.3 / 7.3 s | - |
| kill -9 mid-burst | level | 10.7 s after restart | `behind` never set |
| offline while another seat was restored | level, the restored seat's chat arrives | 1.4 s | - |
| partition, 2 of 5 cut off 4.5 min | level, no fork | 29.5 s after healing | minority `no relay accepted the frame` |

- Every NOT-level case ended level with a NEW link in 6-8 s; no seat was unrecoverable.
- With m > n/2 a partition cannot seal two branches, so the branch-choice path was not exercised.
- NOT run:
  - the coordinator killed between persisting and publishing its re-key (needs a seam);
  - consent off on the rejoiner's side;
  - mixed v0.1.3/v0.1.2 seats;
  - 300 blocks of downtime;
  - a seat offline across the block-format vote;
  - the kill cases on the real history.

Ranking after the field matrix (the five that matter most):

| rank | gap | why |
|---|---|---|
| 1 | 21 re-key to a dead rejoiner | the worst field case; the republic finishes a restore nobody can use, and the only exit is a new link |
| 2 | 1/C.1 coordinator crash around `ready`/publish | NOT level in the field; the persist-publish seam is still unmeasured |
| 3 | 5d base pace on real histories | 0 wiki documents after 10 min on a real copy; blocks cuts and the content |
| 4 | 2/C.2 consent off during a restore | NOT level even after consent returns |
| 5 | 22 forever-degraded health on every restored device | every restore ends looking broken; it hides real faults |

Severity terms: "does not finish by itself" = a new link heals it; no
case in the field lost a seat. 5f stays the severest by construction
(a permanent split), but it was not reproduced in the field.

## A3. Field evidence from the second seat's dump (2026-11-27)

A 2-of-3 republic on v0.1.3, logs of 11-25 to 11-27 (3.9 M lines), read
against the first seat's dump. Counts and shapes only; the analysis
rests outside the tree.

| observation | counts and times (UTC) | gap |
|---|---|---|
| A seat stuck at head 439 for 2.9 h: 148 catch-up requests; the serving seat's log shows 15 serves; none reached the stuck seat until a bundle was imported by hand (level in 4 min; a second import level in 3 min). Not a rule refusal, not an MLS epoch, not a missing request: the serve envelopes did not arrive on the sender's stalled outbox path while other frames from the same sender did | 11-26 13:29 to 16:27 | 10, 11, C.5, C.8, 25 |
| The same class at a third seat: 5 serves of 209 blocks each in 30 min without an adoption | 11-26 17:26 to 17:56 | 16, 25 |
| Forks: 29 heights with two blocks in two days, 27 with different proposals; the longest unresolved 19.4 h, then a rebase that dropped 36 decisions; a 5.2 h race at heights 458 to 468, after block-v2 was active from 442 | 11-25, 11-26 | 14 (C.6), C.9 |
| A `Restored` block lost against an ordinary card at one height; the re-key into the group had already run; the device stayed in the group for 17 h, signed nothing from the next height on, and so blocked every cut | 11-26 17:26 to 18:12 | 3, 21; Q11, Q12 |
| No cut for 224 blocks; the anchor stayed at 233 | three days | 5e, C.4 |
| The holder of a base served no piece for 20 min: 96 waits behind the outbox | 11-26 18:02 to 18:12 | 5d |
| All three seats polled for a block nobody held for 13.3 h: 253 `beyond_head` answers in one seat's last log | 11-26 19:51 to 11-27 09:06 | 16, 28 |
| `withdraw_received ... outcome=Ignored`: 31,511 lines in one day, 99.93 % ignored (the first seat's dump: 92,538) | 11-26 | 26 |
| MCP sessions initialized: 388,221 and 203,897 lines on two days | 11-26, 11-27 | 27 |
| No clean stop at the second seat either: SIGTERM, no `workspace closed` for 90 s | 11-26 18:22 | 20 |
| A decision line from the other branch is logged `forged`; 208 blocks carry one `applied_at` | 11-26 | 29 |

Traced the same day, from both seats' logs and the v0.1.3 code: the stuck
seat received every envelope the sender re-offered (3,917 deliveries,
1,782 of the sender's sequence numbers in the window) and held all of
them beyond its accepted window's cap (gap 25); the gossip flood (gap 26)
had moved the sender's log 1,659 entries forward between two owed
envelopes. The sender's outbox behaved as specified.

## B. Inventory

Size in agent-hours. "Off/hint" means built but switched off, or only a hint.

| # | gap | where | operator sees today | severity | off/hint | fix | h | decision |
|---|---|---|---|---|---|---|---|---|
| 1 | coordinator dies after the persist, before the Welcome leaves: the Welcome is never re-sent; the delivery task is memory-only | `nostr_ritual.rs:505, 530-586`; `membership.rs:1575-1580`; `delivery_guarantee.md` §11 (:779); `pending_rekey_plan.md` §1-§3 (shelf reverted: cut off v0.1.0-v0.1.2 seats at upgrade) | device: `link outdated`; coordinator: `rekey_commit=unsent` | does not finish by itself (a new link: 6-8 s) | plan only | C.1: durable pending re-key | 20 | no |
| 2 | restore sealed while the coordinator cannot reach the group (consent off, no relay, no route): the request and KeyPackage are consumed | `membership.rs:1331, 1339-1351, 1484-1492`; `net/recovery.rs:885-901` | toast "Restore of {seat} interrupted - create a new link", status `restore_unfinished` | does not finish by itself (field R6; a new link: 6 s) | hint | C.2: keep the request, re-key when the group is reachable again | 10 | yes, by itself (Q3) |
| 3 | a displaced restore reopens, then fails: the new device stays in the MLS group, the chain keeps the old anchor | `sync.rs:989-1026` `return_displaced_restore`; `docs_archive/chain/persistent_chain.md` §7.3 (second §7.3, :339-350) | `restore=displaced` (log) | degrades (a reader the chain does not anchor) | planned, not built | C.3: status flag + eviction re-key; field 2026-11-27 (§A3): a `Restored` block lost against a card at one height, the device stayed in the group 17 h signing nothing and blocked every cut | 12 | yes, on command (Q2); Q11 and Q12 decided 2026-11-28 (§C.10) |
| 4 | an own commit that never left races a later one and can undo a second restore | `delivery_guarantee.md` §11 (:796-805); `pending_rekey_plan.md` §1 (B1) | nothing until the device is deaf | blocks | plan only | with C.1 | (in 1) | no |
| 5a | KNOWN (`crates/molt-engine/tests/rejoin_cut.rs`, branch commit `abc53f80`, three green tests at small scale): a restored seat DOES sign the next cut once its base is in. Covered: restored after a folded cut; restored far above its anchor (42 pages back 46 s after the restore); restored with no cut at all | `rejoin_cut.rs:176, 208, 233` | - | - | built | keep the tests | 0 | no |
| 5b | DONE on master (`9659d9fd`), except its GUI strings: a cut refused while the base is pending waits and is judged again when the base arrives (wiki and vault); a head that moved meanwhile drops it; an arriving base proposes no cut. Pinned by `a_cut_refused_while_the_base_is_pending_is_signed_once_it_arrives` (`chain/checkpoint_tests.rs:1318`, green) | `chain/checkpoint.rs`, `net/wiki_base.rs`, `net/vault_base.rs` | - | - | built | GUI strings only | 1 | no |
| 5c | DONE on master (`9659d9fd`), except its GUI strings: `status.chain_sync.cut_wait: [{seat, reason}]` with reason `base loading` / `offline` / `unknown` (`molt-core/src/lib.rs:9130, 9149`); the refusing seat tells the proposer once per cut via an additive `reason` on `VoteRefused`; the base fetch logs once per state change at INFO | `molt-core/src/lib.rs:9130-9160` | `cut_wait` in status | - | built | GUI strings only | 1 | no |
| 5d | [INFERRED; field: on a real-history copy two devices still listed 0 wiki documents after 10 min while the third listed 500+, one of them never restored in that run, so it is the base/open on that history] why the base may never arrive in the field: only holders serve pieces; the trickle publishes one 44 KB piece per 15 s by default, against the daily budget it shares with mirroring, so a base of a few MB needs 20+ minutes at best; the trickle yields to the group outbox, which a rejoiner catching up hundreds of blocks keeps busy; no holder online means no base | `net/wiki_base.rs:24, 56-151, 160-179`; `net/vault_base.rs:12-119`; `knowledge_base_scale.md` §4.9.9 | Memory: 0 pages, "still arriving"; fetch failures at debug | blocks on real histories (raised) | retry built, silent; the pace merged 2026-11-28 (`cc9febf1`) | pace and priority for a base answer to a pending seat (not the mirror budget); WARN once per episode; `status.bases` with `since` (built on the lane 2026-11-27); `holders_heard` is not buildable without a seat id in the piece framing (a wire change): dropped from this release; the daily byte budget still gates every base piece; a scaled field run | 8 | no |
| 5e | why the only cut sat hundreds of blocks below the head: `maybe_auto_checkpoint` fires only on the alphabetically LOWEST seat, only after its own seal, and never while ANY card is Proposed. An offline or base-pending lowest seat, or one long-lived open card, stops compaction indefinitely | `checkpoint.rs:62-84`; `governance.rs:1113` | nothing | degrades (anchor stays low, rejoiners start far back, reorgs deep) | - | the investigator's design: staggered by rank among HEARD seats, the seat at rank r may propose once the chain is 32 + 8·r blocks past the last cut; a cut may proceed while cards are open as long as this seat has no signatures pending for head+1 | 6 | no |
| 5f | CONFIRMED, SEVERE, NOT being fixed now: a cut and an ordinary card sealed at the SAME height give two chains that cannot heal (the seat that applied the cut prunes to it; the others ignore the higher cut; the next block makes it permanent; `chain_unsettled` does not cover it). Red test `a_cut_and_a_card_at_one_height_end_on_one_chain` stays `#[ignore]`d on master (`chain/checkpoint_tests.rs:1451-1453`, "red until the user decides the deterministic cut-versus-card rule at one height"). A fix without activation (keep the dropped period in memory, reorg across the cut) was built, reviewed and REJECTED (§C.4b) | `governance.rs:1359` (prune at the cut); `sync.rs` `tie_break`, `try_reorg` | two heads, forever | blocks (rare: needs a cut and a card at the very same height) | - | a consensus rule with its OWN chain-recorded activation (§C.4c) | not estimated | yes (Q9) |
| 6 | `seat still active`: a link is refused while the old device was heard within 120 s, plus a 120 s catch-up grace after opening (up to about 4 min) | `sync.rs:110`; `membership.rs:238, 572-596`; `net/recovery.rs:296-307` | `seat still active` | degrades (field: 116-145 s, 150 s with the old device running) | - | an explicit override by the minting operator | 4 | yes (Q4) |
| 7 | the first device of a stranded restore keeps nothing before its Welcome and says nothing after a restart; the coordinator's `restore-abandoned` needs a `RecoverFailed` a killed process never sends | `lifecycles.rs:1715-1770`; `nostr_ritual.rs:1523-1525, 1639`; `recovery_ritual.md` F2 (:235-241) | device: fresh start; coordinator: waits | degrades | - | decided against as a persistence (Q10: not B); folded into C.7: the same link is the resume, the coordinator names the superseded attempt | with 21 | decided |
| 8 | `cmd_recover_start` has no idle guard: a second start replaces the running one | `recovery_ritual.md:12-17` | silent replace | cosmetic | open question there | refuse while running (`a restore is running`) | 1 | no |
| 9 | the Welcome travels on the coordinator's dialable group relays; the device listens on link relays ∩ its pool; disjoint sets never meet; a mismatch is only logged | `nostr_ritual.rs:397, 1039-1041, 1501, 1622-1630`; `membership.rs:1484-1506`; `lifecycles.rs:1799-1816` | device waits | degrades | - | publish the Welcome on the union of the link's relays and the group's | 3 | no |
| 10 | reopen after a burst misses the tail beyond the window until `resync_seat`; the open probe is built and OFF; held frames are memory-only and fetched twice after a restart; field: kill -9 mid-burst levelled 10.7 s after restart but `behind` was never set | `net/delivery.rs:56-62, 211, 739-744`; `delivery_guarantee.md` §13 (:1026-1077) | `behind`, then "Fetch history again" | degrades | off | C.5 | 12 | yes, the sheet field (Q5) |
| 11 | a legacy `resync_seat` re-acts old catch-up requests (one per 5 min of history, served at most once per 30 s); `served_at` kept and `clear_divergence` bound to `peer_on_branch` not built | `delivery_guarantee.md` §11 (:864-869) | slow catch-up after a resync | degrades | - | as the note says | 4 | no |
| 12 | a seat behind its OWN commit shows only "N new messages unreadable" | `delivery_guarantee.md` §11 (:792-795); `pending_rekey_plan.md` §5 `mls_behind` | an unreadable count | degrades | plan only | `mls_behind` in status, with C.1 | 3 | no |
| 13 | a restored seat's own open cards: only Files votes are marked `moot`; other surfaces' cards it cannot re-sign stay open unexplained | `files_state.rs:507`; `proposals.rs:2101` | "moot here - decline or withdraw" (files only) | cosmetic | partial | extend `moot` to every card whose stance this device lost | 4 | no |
| 14 | branch choice: built and DARK. Open: GUI, `molt-branch-choice-v1` capability gate, `amnesia_ack` not in the signed bytes, a multi-member restore commit, a relay keystone with an MLS-split group and three branches, no vault test on the choice path; governance on the kept branch does not pause, so a seal makes an open card stale | `branch.rs:16-69`; `branch_run.rs:315-320, 691, 726, 942`; `chain_rescue_plan.md` status, §6, §7; `tests/branch_rescue.rs:222, 339` | refused while dark | blocks S | off | C.6, with C.8 | 30 | yes, live; pause rule yes (Q6, Q7) |
| 15 | seats on a dropped branch keep READING the group until restored: `stopped_when_abandoned` blocks governance and restore commands only [INFERRED: no ingest gate found] | `branch.rs:39-52` | abandoned banner (MCP) | degrades | - | with C.3: the choice's coordinator evicts after the rejoin, or on command | (in 3) | with Q2 |
| 16 | catch-up from far below the head: one `Committed` envelope per block, no batching, 30 s serve debounce; the only figure is 367 blocks in about 1 s on a local relay | `sync.rs:886-921`; `net/ingest.rs:17`; `rejoin_divergence_findings.md` §6 | minutes over Tor [unmeasured] | degrades | - | measure first (Tor, 1,000 blocks); batch only if the publish rate dominates | 8 | no |
| 17 | two seats restoring at once: no design, no test | - | level (field: 34 s / 102 s) | degrades (the 60 s fallback wait) | - | keystone; fix what it finds | 6 | no |
| 18 | a v1 restore consent still counts (an m = 2 hostile `Restored`) | `docs/reviews/known_debt.md:45` | - | security | - | require v2 once every seat announced it (the cap exists) | 3 | yes (Q8) |
| 19 | the block-v2 wall: an OLD-build rejoiner after activation is not specified; the refusal rule is named `threshold` in `persistent_chain.md` §5.1 but `link` in `docs_archive/chain/block_identity_plan.md` | `persistent_chain.md:170-173` | an old binary refuses a v5 workspace | cosmetic | - | test + one doc line; fix the name | 2 | no |
| 20 | a clean close does not drain a withdrawal, a plain drop or `shutdown()` | `delivery_guarantee.md` §12 (:998-1004) | sent at the next start | cosmetic | - | built and merged 2026-11-28 (`cc9febf1`) | 0 | no |
| 21 | the rejoiner dies after its request or after the anchor: the restore is still SEALED and the group RE-KEYED TO THE DEAD DEVICE (epoch +1); the restarted device has no workspace (the ritual is ephemeral, no disk write before the final seal); the same link is refused. The republic completes a restore nobody can use | field R5; `nostr_ritual.rs:1523-1525, 1639`; `net/recovery.rs:128-143` (attempt spent); `chain/membership.rs` auto-approval | survivors: a held stream for that seat (`from_start`); device: `link already used by an earlier attempt - mint a new one` | does not finish by itself (a new link: 6.6-8.3 s) | - | C.7 | 10 | yes, A + C (Q10) |
| 22 | a restored device shows `N new messages unreadable` (9 to 9,260 in the field) in `net_health` FOREVER: the pre-restore traffic it can never decrypt is counted as a fault | field R1, R10; `net/presence.rs` health | net_health degraded, never clears | degrades (misleads every restored seat) | - | count frames from epochs before this device's join as `before this device`, not as unreadable; health stays ok | 4 | no |
| 23 | stale `restore_unfinished`: one survivor kept it for a seat after later successful restores | field; `chain/sync.rs:1576`; `net/recovery.rs:865-875` (the clear) | `restore_unfinished` after the seat is level | cosmetic | - | clear it on any sealed and merged restore of that seat; a test with two restores and the flag on a third seat | 3 | no |
| 24 | a persisted share whose uploader was restored (old device off) reported `shared file ... is no longer available` while every seat lists it as persisted [INFERRED by the tester: with no mirror the bytes lived only on the old device] | field A2; `files_state.rs:301-304`; mirroring worker | `persisted` in every list, download fails | degrades ("persistent" promises a second holder it does not have) | - | status per share `held by N seats`; a persisted share with N = 1 shows `only on <seat>`; a restore warns when the seat is the only holder | 5 | no |
| 25 | traced 2026-11-27: a deadlock on the receiving side of the accepted window. A sender's log grew by 1,659 entries between two owed envelopes (the gossip flood of gap 26); the next owed one lay above `frontier + ACCEPT_WINDOW_BITS` (1024) and was held although its `prev_seq` was accepted. The ack sheet froze, the sender's acknowledged floor stuck at its first unaccepted envelope, every re-offer arrived and was held (15 serves of one block among them), and the listing that heals it was suppressed for 4 h by the `replaying` gate | `crates/molt-engine/src/net/delivery.rs` (`cap()`, the hold at the cap, the ask path's replay gate, `inbound_held`) | one INFO line `inbound_held`, then silence; `re-offering the tail` at the sender | does not finish by itself (a bundle by hand: 3 to 4 min) | - | (1) an envelope chain-linked to an accepted `prev_seq` is accepted beyond the cap and moves the frontier; (2) a listing ask also while a replay runs, bounded; (3) a repeated WARN while envelopes are held | 10 | no |
| 26 | a gossip flood, traced 2026-11-27: every catch-up beacon (300 s) and every heal retry of any seat triggers `serve_open_governance`, which re-serves every open card and every own withdrawn card inside the retention as fresh envelopes, as a broadcast (quadratic with three seats). Ignored frames are acknowledged; the transport is not the cause | `net/ingest.rs` (`serve_open_governance`), `chain/governance.rs` (the served set), `chain/sync.rs` (beacon and heal) | log noise, publish budget; a volume source behind 25 | degrades | - | serve open governance only to a peer that is behind or (re)joining; own tombstones once per peer; the repeated lines at DEBUG | 8 | no |
| 27 | every call of one client opens an MCP session and logs three INFO lines | `molt-mcp` default log filter | 388,221 lines in one day | cosmetic (the log) | - | `rmcp` at WARN in the default filter; tell the client's author | 1 | no |
| 28 | a stale "ahead" claim keeps every seat polling for a block nobody holds | `chain/sync.rs` (`request_catchup`, `beyond_head`), the peer heights | 13.3 h of polls | degrades (noise, budget) | - | a `beyond_head` answer retires that peer's claimed height; the status says nobody holds it | 3 | no |
| 29 | texts: a decision line from another branch is logged `forged`; a replayed range carries one `applied_at` | `net/ingest.rs`, the block stamps | misleading | cosmetic | - | `from another branch`; the stamp of the local apply | 2 | no |
| 30 | the `answers` flag of a hold latches at the first listing or gap answer and is never reset; from then on repeats of a re-offer arm no evidence and the §10(c) frontier jump is skipped, so a cap-held tail waits for a listing: the second half of the field stall (review of gap 25, 2026-11-28) | `crates/molt-engine/src/net/delivery.rs` (`SenderHold::answers`, `passed`, `inbound_gap_tick`) | nothing until a listing raises the frontier | does not finish by itself | - | built 2026-11-28: an answer ages out after 3 unanswered listing asks per episode, never inside the sender's 60 s refusal window | 6 | no |

## C. Designs

**C.1 Durable pending re-key** (gaps 1, 4, 12). `pending_rekey_plan.md` §3 is the design.

Its revert taught three rules:
- shelve only a commit THIS build marked (`commit_pending`), never a v0.1.0-v0.1.2 seat's last own merge;
- no day boundary: the shelf is kept by content until the commit is seen merged by a peer, or the device's admission is seen;
- no snapshot version bump: additive fields, so rollback and downgrade stay possible.

At the open, the commit is re-published, the Welcome re-sent and `mls_behind` reported.

Tests, red first:
- a keystone over MockRelay with a kill seam after the persist: the device joins after the coordinator restarts;
- the upgrade test the revert lacked: a v0.1.2 seat whose newest merge is its own re-key opens the new build and stays readable;
- the race: a second restore after an unsent first ends with one group.

Field run, on copies of a real republic: kill the coordinator between seal and publish, restart, and the device joins.

**C.2 Re-key when the group is reachable again** (gap 2). Do not consume the request: keep `pending` with its KeyPackage, sealed (the same key as the snapshot). When the group is reachable again, run `coordinator_rekey`. An already-approved block is completed, not a new decision, but the product's rule asks: Q3.

Tests:
- consent off at the seal, then on: the device joins;
- the request expires only when the device withdraws or a new link supersedes it.

**C.3 A device the chain does not anchor** (gaps 3, 15). Status flag `device_unanchored {seat}` and a seat-scope command `evict_device {seat}`: a remove commit for the leaf the chain does not name. Automatic or on command: Q2.

Tests:
- a displaced restore that fails: the flag shows, the command evicts, the old leaf decrypts nothing;
- a correct restore is never flagged.

**C.4 Cuts after a restore** (gaps 5a-5f).

Independent of the state-model choice, and built in any case:
- re-judge a refused cut when the base arrives (5b);
- show every seat who is missing and why (5c);
- make the base fetch fast enough: its own pace and priority, not the mirror budget, not behind the outbox (5d);
- let any seat propose a cut when the lowest stays silent (5e);
- the cut/card race (5f) is a consensus rule with its own activation (§C.4b, §C.4c; Q9 decided 2026-11-27).

The state-model question for the user (Q1), as the investigator laid it out:

| option | what | for | against |
|---|---|---|---|
| (a) | the restore ritual completes only after the base is fetched and verified | no state-model change; "every seat recomputes a cut" holds; 5a shows it works once the base is in | the restore waits on a holder being online and on the fetch pace (5d) |
| (b) | sign a cut from commitments without holding the content | a pending seat can sign | a new conditional checkpoint layout; a seat attests a wiki it never saw |
| (c) | cuts at m-of-n | `verify_next` already accepts m; only the sealer demands n; matches the no-split plan's Q3 for mode chains | the dropped-history argument returns: a seat that never reproduced the state loses the history below the cut |
| (d) | explicit abstention above a bound | cuts continue without the pending seat, visibly | a new wire event, a verification rule, a downgrade wall |

Decided 2026-11-27: (a), with the independent fixes. It closes the gap
without a format change; 5a shows the seat signs once the base is in.
(c) is not built: there are no mode chains.

Tests:
- 5b's test is green on master; 5f's stays ignored until the rule of §C.4c is active;
- a holder stays offline: every seat's status names the missing signer;
- a base of a few MB arrives within one catch-up at the new pace;
- the lowest seat is silent: another seat proposes;
- a scaled field run on copies (hundreds of pages, Tor).

**C.4b The cut-versus-card race (5f): a consensus rule.**

The rejected fix: keep the dropped period in memory and reorg across the
cut. It was built, reviewed and NOT merged:
- whether a seat rolled back across a cut depended on runtime state (did it restart since the cut, which build);
- the window stayed open until the next n-of-n cut, which a stranded seat prevents;
- it could halt a 3-of-4 republic two against two, and override a ratified `BranchChosen`;
- it broke R2 ("a reorg never crosses a checkpoint cut", `docs_archive/chain/chain_reorg.md`) and the wallet's I16 ("final = below a cut").

The rule to decide (the investigator's analysis, not built):

| item | content |
|---|---|
| rule | at one height a checkpoint cut carrying n distinct roster signatures outranks a non-cut block; otherwise the lower block identity wins as today. rank = (is_cut ? 0 : 1, identity) |
| where | `tie_break` at the tip; `tie_break` below the tip when the existing block is this seat's ANCHOR cut (the incoming non-cut loses, no divergence mark, serve our branch); `try_reorg` at the fork height |
| outcome [INFERRED] | every arrival order ends on the cut; a seat never crosses its anchor, so R2 and I16 stay true; a displaced card returns to the vote and re-seals above the cut |
| not covered | a fork BELOW the cut's height: stays with the rejoin path |
| against `BranchChosen` | to decide; recommended cut over chosen: every chooser also signed the cut, and the choice re-seals at H+1 |
| mixed versions | with v0.1.3 seats, a card with the lower identity and an old seat that took it first: the old seat keeps the card, new seats take the cut. That is a version-dependent split that does not exist today. Today the cut-first seats strand; the total stranding is not worse, but WHO strands changes |
| consequence | a consensus rule: it needs a chain-recorded activation height, not capability sightings. Decided 2026-11-27 (Q9): yes, and cut over `BranchChosen`; it gets its OWN activation (§C.4c), one vote, one downgrade wall |

Alternatives at the source:

| alternative | closes | price |
|---|---|---|
| signing lock: a seat that signed a cut at H signs no card at H until the cut seals or is withdrawn, and the reverse | the race for unmodified seats, with no activation (local signing policy) | liveness: deadlocks need an explicit withdraw (n = 3, m = 2: one seat signed the card, two the cut); every card waits behind an open cut that needs ALL seats, so one offline or base-loading seat freezes cards; mixed versions only narrow the race |
| sealer-side check: do not seal a cut while holding m signatures for a card at that height | narrows it only | none worth it alone |

Until the rule of §C.4c is active, the race stays open and rare: it
needs a cut and a card sealed at the very same height. The operator's way
out is to restore the stranded seat by link from a seat on the other chain.

**C.4c The rule and its activation (Q9, decided 2026-11-27).**

| item | content |
|---|---|
| rule | below the activation height nothing changes. From it: at one height a checkpoint cut carrying n distinct roster signatures outranks every other block, `BranchChosen` included; otherwise the lower block identity wins as today. rank = (is_cut ? 0 : 1, identity) |
| where | `tie_break` at the tip; below the tip when the existing block is this seat's ANCHOR cut (the incoming non-cut loses, no divergence mark, serve our branch); `try_reorg` at the fork height |
| activation | a chain-recorded height every seat computes the same way, built like `block-v2` (`docs_archive/chain/block_identity_plan.md`): a feature key set by one vote; gates: every seat announced the capability, `block-v2` active, no cut open. Existing republics: H = c + 1, c the first cut above the enabling block (every seat is on the enabling block's lineage when c seals); new foundings: with their first cut |
| not covered | a fork BELOW the cut's height: the rejoin path |
| mixed versions | a v0.1.3 seat that took the card first keeps it while new seats take the cut: a version-dependent split that does not exist today (today the cut-first seats strand). Who strands changes, not how many |
| design review | before any code: the feature key's name; a `BranchChosen` with m signatures against a cut with n at the same height when every chooser also signed the cut (§C.4b says cut over chosen); the relink of a displaced card above the cut |
| tests | red first: every arrival order of a cut and a lower-identity card at one height ends on the cut; a seat never crosses its anchor cut; a displaced card returns to the vote and re-seals above the cut; the ignored `a_cut_and_a_card_at_one_height_end_on_one_chain` turns green; the rank applies from H and not below; no wire layout changes |

**C.5 The reopen tail** (gap 10). The probe needs a bound tied to the sender being heard (§13 v0.1.4 list). The same doc names the simpler step: the ack sheet carries the sender's own log end. A receiver below it asks the existing gap want, with no listing protocol and no exemption. Decided 2026-11-27 (Q5): the sheet field; the probe stays off.

Tests:
- a seat killed in a burst of 2,000 frames reopens and holds them all without `resync_seat`;
- a v0.1.3 sender (no field) changes nothing.

**C.6 Branch choice goes live** (gap 14). Decided 2026-11-27 (Q6, Q7): live in v0.1.4, after the keystones and the field run, with the pause rule. Between split groups its transport is the evidence exchange (§C.8).
- Close the capability gate, put `amnesia_ack` into the signed bytes (pin), build the GUI rows.
- Keystones: MLS-split with three branches over a relay; 3-of-5 with forks at two heights; a vault grant on a dropped branch.
- Pause rule (Q6): seats that approved an open choice seal nothing else until it seals or fails, so a stale-card race cannot void it.
- The exit (decided 2026-11-28): an approver withdraws its own approval, on command, no clock; its pause ends. The signature it already sent cannot be recalled, so the card may still seal; once the withdrawer seals something else the card is `stale` and a new card is needed. No decline vote and no expiry in this release.
- Field run: copies of a split republic chose a branch, rejoined by link, one head, a vote seals.

**C.7 A restore nobody can use** (gap 21). Options for the user (Q10):

| option | what | for | against |
|---|---|---|---|
| A | seal and re-key only after the rejoiner shows it is alive at the last step: a fresh signed "still here" over the attempt, after the anchor, before the seal | no ritual persistence; the dead-device re-key cannot happen before that step | a device dying after its "still here" and before the Welcome remains (narrow) |
| B | the rejoiner persists its request state (ticket, KeyPackage secret) sealed under the phrase key, so a restart resumes the same attempt | the same link heals a restart | breaks "no disk write before the final seal" (`docs_archive/ritual/recovery_ritual.md`); a key on disk before admission |
| C | the link stays valid until the device ACKNOWLEDGES its Welcome: a new run on the same link supersedes the dead attempt and re-keys again | no persistence; the operator just restarts and uses the link again | the coordinator re-keys twice; the first, dead leaf must be removed (with C.3's eviction) |

Decided 2026-11-27 (Q10): A + C. A closes the common window, C covers the
rest without touching the ritual's ephemerality. Gap 7 folds in: the
restarted device uses the same link again, nothing is persisted on it,
and the coordinator names the superseded attempt.

Tests:
- kill the rejoiner after its request: no seal, no re-key, and the restarted device joins on the same link;
- kill after the Welcome: unchanged (level after restart).

**C.10 A displaced restore is proposed again (Q11 and Q12, decided
2026-11-28).** The re-key into the group keeps running before the
`Restored` block is final. When a fork displaces that block:
- the displaced `Restored` returns as an open card by the standing rule
  for displaced cards (`return_displaced_card`), and the device is named
  in `status` as waiting for it (C.3's flag);
- the survivors' standing approvals apply, and the restored device, which
  is in the group, signs its consent again at the new height;
- the sealer seals it like any m-1-plus-consent restore; no new link.
Tests, red first, in the field's shape: a `Restored` block and an
ordinary card at one height, the card takes the tip; the restore is then
proposed again, consented and sealed within the usual bounds, the device
is anchored again, and a cut can seal afterwards. Whatever kept the
field's restore from being re-sealed (the consent's re-sign, a
`restore_gate` refusal, a card that did not return) is found on the way
and pinned by a red test.

**C.8 The evidence exchange (decided 2026-11-27, after field feedback).**
Seats exchange the evidence of a second branch themselves, without a
passphrase; the seat table shows it per seat; adopting needs one
confirmation. Design: `docs/chain/chain_rescue_plan.md` §5a. Built on a
side branch on 2026-11-27 and merged 2026-11-28 (`d2fb9442`) after four
review passes. Open with it: the per-seat column in Organization > Status
> Seats and its confirmation control (step 7).

Field answer (2026-11-27, §A3): it was none of the three. The stuck
seat asked 148 times and the sender's log shows 15 serves; every serve
reached the stuck seat and was held there by its accepted window (gap 25,
traced on 2026-11-28). The hand imports made it level in 4 and 3 minutes
because the bundle travels outside that window. The automatic offer's
trigger, "served repeatedly without progress", is this field case; the
four review passes of 2026-11-27 and 2026-11-28 found and fixed eight
defects that would have broken it there (an unbounded re-judge, a budget
spent by a refused bundle, a latch set before anything was sent, a
size bound that trusted the worst case, a hold that ignored a corrected
claim).

**C.9 The lag guard (a plan item since 2026-11-27; merge only on the
user's word).** A seat behind a heard peer neither signs at its head nor
seals, and a signature for a height its signer left does not seal;
`status.chain_sync.signing_held` names the peer and the distance. It
narrows the fork schedules T1 and T2 of the rejected no-split plan (§1.1)
inside today's model and prevents none of them: two seats that consider
each other silent still seal. Built on a side branch (3 commits and a
review round P1b), not merged. Before any merge: its own plan document
(the rule, liveness under a false or stale "ahead" claim, mixed versions,
the tests), then an independent review.

Review of 2026-11-28: fix first. PARKED for this release by the user's
decision of 2026-11-28; the branch stays, nothing of it merges.
- HIGH: rule R5 filters the valid signatures of a proposal by locally
  inferred peer claims before the seal truncates them, so two seats can
  seal different signature sets for one proposal at one height. Where
  block identity v2 is active that is another attestation of one block;
  below its activation it is a competitor fork.
- MEDIUM: the replay gate suppresses the recording of claims, so a seal
  waits for the signer's next beacon; the hold also stops a cut's
  co-signature, and compaction stands still for the hold.
- The benefit in the field is narrow: it closes the re-seal loop after
  tip swaps and the returning seat's competitor seal, and changes nothing
  in the two large field shapes (the stuck catch-up is gap 25, the
  thirteen-hour poll is gap 28).
- Kept from it: rule R10 (a `beyond_head` answer retires the claimant's
  claimed height) belongs to gap 28.

## C2. What the rejected no-split plan needed from the rejoin

The no-split plan is rejected (2026-11-27). Of this section only N3
stays, as a rejoin item in its own right, and N1 as display; the table
"under the mode" below is void. Kept for the record: fork prevention was
planned in `docs_archive/chain/no_split_plan.md`; its prerequisites from
this plan were C.1, N3, C.4 and gap 18; it objected to N1 and N2.
Checked against the code:

| # | the no-split plan's objection | code check | position here |
|---|---|---|---|
| N1 "the Welcome carries the seat's signed state" | under its mode an acceptance travels only to the ballot's leader, so the coordinator's Welcome holds only its own slice; adopting the old device's state is safe only with answers from all n - 1 seats. Instead a device change is a NEW acceptor born by a sealed `Restored` block, and a device without its voting record never votes (§4.2) | true by its design. Today (B6) approvals are broadcast to all seats, but that changes under the mode. The Welcome carries no signature state today (`membership.rs:1501-1506`) | DROPPED as a safety requirement. Kept only as display: a restored device shows its seat's standing stances (`own_approvals` as the peers re-serve them), marked "earlier device", never counted as votes (3 h, step 7) |
| N2 "incarnation = `incarnation_height`" | computed from `chain.blocks`, which a prune cuts back, so a pruned verifier gets 0; the working anchor already survives a cut; build the incarnation inside `molt-chain-change-v4` (one vote, one wall) | `incarnation_height` filters `self.chain.blocks` (`chain/branch_run.rs:228-238`); the prune `self.chain.blocks.retain(\|b\| b.height >= anchor_height)` (`governance.rs:1359`). `CheckpointState.anchors` keeps the working anchors across a cut (`molt-core/src/chain.rs:637-643`; `walk_working_anchor`, `chain/verify.rs:534-544`). The anchor register keeps anchors unique per restore (`net/recovery.rs:128-143`) | the objection holds. N2 was to move to the no-split plan's S2 (incarnation = the working anchor, inside v4); with that plan rejected on 2026-11-27 it is not built, and the objection stands |
| N3 the old device stops on its own restore | (taken as a prerequisite) | today it stops only on MERGING the re-key commit that removes its leaf: `seat_evicted` (`net/presence.rs:231-247`), then `handle` refuses group commands with `SeatEvicted` (`lib.rs:2290-2292`). A `Restored` block for its own seat that arrives by catch-up without that merge is applied like any other; no own-seat check found in `settle_block` (`chain/governance.rs`) | STAYS here: a sealed `Restored` naming this seat with a working anchor that is not this device's → the evicted state, notice `seat-restored-elsewhere`, sign and publish nothing (6 h) |
| N4 cuts seal after a restore | (its activation needs a cut) | gap 5 | C.4 stays here |

The mode is not built; these are the definition-of-done statements it
would have changed (not designed here;
`docs_archive/chain/no_split_plan.md` §4.2-§4.3, §7):

| statement | under the mode |
|---|---|
| R1/R2 "restored by link" | a restore needs s(n, m) = max(m - 1, n - m + 1, floor((n - 1)/2) + 1) survivors plus the consent: in 2-of-3 BOTH other members online (today one + consent) |
| R2 "two seats lost" | in 2-of-3 halted for good (refound); today the last seat restores both alone, which made the field's split |
| R1 for an import (export or S3 backup) | passive until restored by link: reads, catches up, never votes |
| R3 | new lines: `restore needs 2 members online - C offline`, `voting record missing - restore this seat with a link` |
| gap 17 (two restores at once) | answered by the mode: one `Restored` per height, the other consent goes stale |

## D. Staging (v0.1.4)

Three lanes beside the branches already built (the evidence exchange, the
base trickle and close drain, the lag guard), each step behind its review
gate. "After" names what must be merged first.

| step | gaps | owns | after | parallel with |
|---|---|---|---|---|
| 1 C.1 durable re-key, with gap 9 | 1, 4, 12, 9 | `nostr_ritual.rs` re-key delivery, `chain/membership.rs` re-key, `molt-net/src/mls.rs` state, `net/presence.rs` | - | cut lane, delivery lane, small lane |
| 2 N3 old device stops | - | `chain/governance.rs` settle, `net/presence.rs` | 1 | cut lane, small lane |
| 3 C.7 and the rejoiner, then C.10 | 21, 6, 7, 8, 13, C.10 | `lifecycles.rs`, `nostr_ritual.rs` rejoiner half, `net/recovery.rs` attempts, `files_state.rs`/`proposals.rs` moot, `chain/sync.rs` (`return_displaced_card`) | 2 | cut lane |
| 4 v2 consent only | 18 | `chain/verify.rs` consent rule, `chain/membership.rs` mint gate | 2 | cut lane |
| 5 C.2 re-key when reachable | 2 | `chain/membership.rs`, `net/recovery.rs` | 3, 4 | cut lane |
| 6 C.3 and N1 as display | 3, 15, N1 | `molt-net/src/mls.rs`, `chain/membership.rs`, `chain/sync.rs` (`return_displaced_restore`), status | 5; its status flag and the line naming a seat that stopped signing start at once (field 2026-11-27) | cut lane |
| 7 C.6 branch choice live, with the evidence column | 14, evidence | `chain/branch*.rs`, `molt-ui` | 3, 6; its capability gate, `amnesia_ack`, pause rule and keystones start at once | - |
| 8 cut lane: 5d (merge), 5e, the 5b/5c strings, 17, then 5f (§C.4c) | 5b-5f, 17 | `chain/checkpoint.rs`, `net/wiki_base.rs`, `net/vault_base.rs`, `chain/sync.rs` `tie_break` / `try_reorg`, `molt-ui` strings | 5f after its design review | serial lane |
| 9 delivery lane: 25 at once (traced), 26 (its own lane), then C.5 and 11, then 28 | 25, 26, 10, 11, 28 | `net/delivery.rs` (the accepted window, the ack sheet), `net/ingest.rs` and `chain/governance.rs` (26), `molt-net` outbox | 10 and 11 after 25 | serial lane |
| 10 catch-up | 16 | a field run over Tor measures first; batching only if the publish rate dominates | 6 | - |
| 11 smaller | 19, 22, 23, 24, 27, 29 | docs, test; `net/presence.rs` health; `net/recovery.rs` flag; `files_state.rs`, mirroring worker, status; the log filter and two texts | 22 and 23 after 1 | any |
| 12 lag guard | - | its own plan and review; merge on the user's word | - | - |
| 13 acceptance | all | `tests/` keystones, field script, fork count from the seat dumps | 1-11 | - |

- Serial lane: 1 -> 2 -> 3 -> 4 -> 5 -> 6 -> 7 (re-key, ritual, MLS, membership). Step 3 carries C.7, the worst field case.
- Cut lane: 8. Delivery lane: 9, beside everything.
- The evidence exchange (§C.8) merges after its review; its GUI column is step 7. Step 11 runs beside any lane.
- Review gate: every step in recovery, MLS, chain or delivery gets an independent review before merge, against this plan's statements; a fix round adds the test of each finding.
- Acceptance script: R1-R3 under each disturbance, O1-O2, S1-S2, as MockRelay keystones with kill/partition/consent seams; then field runs on copies of a real republic over Tor (one per C-design). Forks in the field are counted from the seat dumps.

| | agent-h |
|---|---|
| gaps 1-24 as in §B (5b/5c's 2 h are their GUI strings; 7 folded into 21; 5f not estimated) | 168 |
| N3 | 6 |
| N1 as display | 3 |
| acceptance keystones + field runs | 24 |
| reviews and fix rounds | about 38 |
| total | about 240, without the evidence exchange and the lag guard |

Calendar: about 10 to 14 working days with three lanes; the serial lane
(about 100 h with reviews) sets the pace.

## D2. State of the lanes at the cut (2026-11-28, 12:15 CET)

The session that built the lanes above was cut here. Master is
`f6e96612`, pushed. Every lane sits in its own worktree under
`.claude/worktrees/`, one branch each, its tree clean; none of the five
below is merged. Each is merged only after an independent review and a
green gate on master.

| lane (gap) | branch | commits on top of master | state | next |
|---|---|---|---|---|
| accepted window's cap deadlock (25) | `worktree-agent-a9f0851128a72e486` | `743bd999` | built and green; reviewed 2026-11-28: merge, on condition that the full suite is green (it was still running at the cut) | rerun the gate in that worktree (`net::delivery`, the nine guarantee tests, the engine library, `molt-net`, clippy, the doc checker), then merge. The review's findings: MEDIUM, the `answers` flag latches for a hold's lifetime and silences the §10(c) self-heal, the second half of the field case (gap 30); LOW, the exception rests on the sender's honesty, so the spec must name that premise; the argument `high <= frontier + ACCEPT_WINDOW_BITS` is not an invariant, keep it out of the spec. `prev_seq` was confirmed to name the sender's previous own ackable event |
| durable pending re-key, C.1 (1, 4, 12, 9) | `worktree-agent-a0607100893122088` | `f3cea226`, `d34f9273`, `ad6e8e8b`, `ee563ea1`, `43661cda` | built; `molt-net` 292, the engine library 1320, `nostr_recovery` 43 green | not run: the 16 remaining integration suites, `molt-mcp`, clippy; then an independent review, then merge. Open in it: no engine keystone with disjoint link and group relays (gap 9); the runtime detector of `mls_behind` has no red test of its own; the GUI line for `status.mls_behind` |
| governance gossip flood (26) | `worktree-agent-aab3c91af45368f0c` | `ff2aef6b`, `703a09d9`, `0d690be1` (both wip) | rules 1, 2, 3 and 5 built and green; the keystone of rule 4 showed the serve reaching a reopened seat that is behind; its last failure was the test's own race, fixed by polling (not rerun since) | rerun `--test governance_serve_keystone` once; settle whether the new rule ("only a peer behind or joining") leaves a seat without its open cards that was offline while cards were proposed and is level afterwards (the keystone covers only a seat that is behind; on master the 300 s beacon served everyone): red test first, then the rule or the keystone; then review, then merge |
| branch choice gate, `amnesia_ack`, pause rule, keystones (14, step 7) | `worktree-agent-a405fe84b36ff8783` | `9d7dcb6d` to `8703e249` | built and green; reviewed 2026-11-28: fix first | (1) the keystone fixtures' literal page names fail `scripts/check-doc-refs.py`; (2) the pause does not recall signatures an approver made before it approved the card, so a card can still go stale: pin it and correct the claim in the rescue plan's status line; (3) a branch card has no withdraw, decline or expiry, so the plan's "or fails" needs an exit: decided 2026-11-28, an approver withdraws its own approval (§C.6); (4) one `chain_lock` test failed in the loaded batch and must be shown flaky or old; (5) the capability gate with a seat that truly did not announce. Then merge dark |
| lag guard (§C.9) | `worktree-agent-a9076529ed30a58c4` | `faa970b9` to `fd07abf0` | reviewed 2026-11-28: fix first | parked for v0.1.4 (user, 2026-11-28) |

**Second cut, 2026-11-28 (the machine restarts with more CPUs).** Master
merged into three lanes; every run below is after that merge, nothing
red, every tree clean. The table's "next" column still holds except
where this list says otherwise.

| lane | tip | green | still to run |
|---|---|---|---|
| 25 | `e959bc8c` | `molt-net` 472, the engine library 1354, the five delivery suites 13, the doc checker | clippy for `molt-net` and `molt-engine`; then merge |
| C.1 | `3cefe7e9` (one conflict, this plan's gap table) | `molt-net`, the engine library 1351, `backup_ticker`, `branch_rescue`, `c_free_guard`, `chain_convergence`, `chain_lock`, `chat_channels`, the doc checker | the other integration suites from `checkpoint_under_load` on (`nostr_recovery` too), `molt-core`, `molt-storage`, `molt-mcp --lib`, clippy for five crates; then its review |
| 26 | `0b551c84` | `molt-net` 472, `governance_serve_keystone` 2, `chain_convergence`, `three_nodes`, `majority_rule`, clippy 0, the doc checker | the engine library, `vote_at_a_moving_head`, `vote_survives_restart`, `proposal_listing`, `rejoin_cut`, `delivery_guarantee`, `nostr_delivery_guarantee`; then its review |

Lane 26's question is answered without a rule change: a seat that was
offline and returns level receives its open cards by the delivery
guarantee (`a_level_seat_back_from_an_absence_gets_its_open_cards`), and
master's beacon never served a level seat either (its ask from head+1
is refused `beyond_head`). For the review: the new keystone never ran
red, so prove it by mutation; confirm the claim about master in the
code. Unbuilt and the same on master: an absence longer than the
compaction grace leaves a level seat without its open cards.

Decided by the user on 2026-11-28: the lag guard is parked; a branch
card's exit is the approver's withdrawal (§C.6), built with step 7's
fix items after C.1 merges (the two lanes conflict).

Still to start from §D: N3 (step 2) after C.1; C.7 and the rejoiner with
C.10 (step 3); the evidence column of the seat table and the choice's
GUI (step 7); C.5 with gap 11 (step 9); gap 28 with the lag guard's rule
R10; the small lane (step 11). `docs_archive/transport/delivery_guarantee.md`
is still German: translate it once gaps 25 and C.1 are merged, since
both change it.

## E. Out of scope

| item | why |
|---|---|
| fork prevention | REJECTED by the user on 2026-11-27 (the restore cost); the design rests in `docs_archive/chain/no_split_plan.md`. Forks below the threshold stay possible: this release heals them, makes them visible and counts them |
| a restore attempt persisted on the device (gap 7's first fix, Q10's option B) | decided against: it breaks the ritual's ephemerality; the same link is the resume |
| finality (rescue Q7) | not about coming back |
| wiki search (`docs/BM25.md`), Robo-Scrum | after this release |
| chat history, own upload bytes, mirrored pieces, wallet key part for a restored device | by design (§A) |
| diagnostics on request (`docs/diagnostics_request_plan.md`) | design first |
| draining a clean close (gap 20) | built and merged 2026-11-28 (`cc9febf1`) |

## F. Open questions

| # | question | recommendation |
|---|---|---|
| Q1 | cuts after a restore: (a) the ritual completes only after the base is in, (b) sign from commitments, (c) m-of-n cuts, (d) explicit abstention (C.4) | (a), plus the independent fixes 5b-5f; (c) only on mode chains |
| Q2 | a device the chain no longer anchors: evicted automatically by a re-key, or on an operator's command | on command, with the status flag and a wake; automatic only for the choice's own coordinator after a rejoin |
| Q3 | re-key when the group is reachable again: by itself, or by the operator | by itself: it completes a block the threshold already approved; a notice says it ran |
| Q4 | `seat still active`: keep 120 s, shorten, or an override | keep, plus an override by the minting operator: `the running device will be removed` |
| Q5 | the open probe in v0.1.4, or the ack-sheet log-end field | the sheet field; the probe stays off |
| Q6 | the pause rule: approvers of an open branch choice seal nothing else until it ends | yes |
| Q7 | branch choice live in v0.1.4, or "back from a dropped branch" by link only | live, after C.6's keystones and field run |
| Q8 | v2 restore consent required once every seat announced it | yes (also a no-split prerequisite) |
| Q9 | the cut-versus-card rule at one height (§C.4b): "at one height a checkpoint cut with n distinct roster signatures outranks a non-cut block, otherwise the lower identity wins; activated with the no-split plan's activation height, not as a plain fix"; and does a cut outrank a `BranchChosen` at the same height? | yes, and cut over chosen. Decided 2026-11-27: with its own chain-recorded activation (§C.4c), one vote, one downgrade wall. Until it is active the race stays open and rare; the way out is a restore by link. Not the signing lock: it freezes cards behind any cut a single seat cannot sign |
| Q10 | a rejoiner that dies before its Welcome (C.7): A seal only after a fresh "still here", B persist the request state on the device, C the link stays valid until the Welcome is acknowledged | A + C; not B (it breaks the ritual's ephemerality) |
| Q11 | DECIDED 2026-11-28, with the user: re-propose and re-seal. A `Restored` block that a fork displaces returns as an open card; the survivors' standing approvals apply, the device in the group signs its consent again at the new height, no new link (§C.10). Field 2026-11-27: the card won at one height and the device sat in the group unanchored | eviction (C.3) stays for what that fails to heal |
| Q12 | DECIDED 2026-11-28, with the user: the re-key into the group keeps running before the `Restored` block is final (field: re-key at 17:26, the block displaced at 18:12) | repair by Q11 and C.3 |

Decided 2026-11-27: the user approved every recommendation above. One
change followed from the rejected no-split plan: Q9's rule gets its own
activation (§C.4c). Forks stay possible below the threshold by that
decision: this release heals them, makes them visible and counts them.

### Decisions that were to be taken together with the no-split plan

The plan is rejected, so the table below is history. What it settles for
this release: completing an approved block may run by itself (C.1, C.2;
Q3); removing a device needs an operator (Q2), choosing a value needs the
threshold (the branch choice); cuts stay n-of-n everywhere (Q1 (a)); v2
consent only (Q8); the cut-versus-card rule ships with its own activation
(Q9, §C.4c).

| here | there | interaction | to decide together |
|---|---|---|---|
| Q6 pause rule for an open branch choice | its halting rule: a height waits for its decision, nothing else seals (§7) | under the mode no fork happens among unmodified builds, so the branch choice serves only republics without the mode and splits from before activation. The pause rule is then the mode's rule in small | if the mode becomes the default (its Q4), is the branch choice still worth going live (Q7 here)? Recommendation: yes for existing republics before their activation cut |
| Q2 eviction on command, Q3 automatic re-key | its Q5 "no exit" for a restore with fewer than s survivors, and its Q7 timer-driven takeover | both plans refuse a silent decision between values but allow a retry that completes an approved one. Q3 here is such a retry; Q2's eviction is a decision | one rule for both: completing an approved block may run by itself; removing a device or choosing a value needs an operator |
| Q1 cuts after a restore (n-of-n today) | its Q3: a cut under the mode needs m (own state hash only) | with m, a base-pending seat no longer blocks cuts, which makes C.4's option (c) the mode's rule for mode chains. Option (a) and 5b-5f are still needed before activation, because the activation cut is n-of-n; 5f's fix must agree with its "a cut outranks other blocks at its height" | (a) now, everywhere; m for cuts only on mode chains |
| Q8 v2 consent only | its §5.1: v1 consent refused under the mode | the same rule, earlier | yes, in this release |
| Q9 cut over card (5f) | its §6: "a cut outranks every other block at its height" below H | the same rule; it needs n signatures on the cut and a chain-recorded activation | decide once, ship with its activation |

