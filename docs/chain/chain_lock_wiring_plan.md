# The chain lock: wiring plan (release v0.1.1)

**Status: BUILT, dark: not available in release builds, under review;
the switch is `CHAIN_LOCK_AVAILABLE` (design, "The switch").** Design:
`docs/chain/chain_lock_design.md` (§8 activation, §14 "As built"). With
the switch on the lock runs where the chain enabled it: after a
`set_features` vote naming `chain-lock` (from the height after that
block). A founding starts without it (§11); only a test seam
(`__found_locked`) founds locked from height 1. Activation by capability is gone.

## 1. What the lock is for

B6 gives one sealer per height and took the measured fork decisions in a
lossy three-seat test from 11-19 to 0, but a fallback sealer can still
seal a different proposal after a lost block. The lock makes two
different blocks at one height impossible among honest seats: a seat
signs a position only for the value its leader announced, and a new
leader must first learn what was accepted. Signed bytes do not change.

## 2. What exists

- Wire: five additive events (`LockPrepare`, `LockPromise`,
  `LockAccept`, `LockAccepted`, `LockNack`), not owed by the delivery
  guarantee, byte-pinned; a pin that an older build drops them; the
  capability `molt-chain-lock-v1`; `chain_sync` status fields.
- Storage: a sealed `chain_lock.state` with its own key, written
  blocking and atomically, excluded from exports.
- Engine (`chain/lock_wire.rs`): one gate for free signing, one for lock
  acceptance; every acceptor change is persisted before its frame and
  rolled back when the write fails; intents instead of position
  signatures at locked heights; leader, takeover, forced value, seal;
  lineage with a cache; the corrected release rule.
- Position signing has exactly two production call sites (§3).

## 3. Position-signing paths (re-derived on the merged tree)

Production `identity_sign` over `approval_bytes`: `governance.rs`
`chain_sign_and_gossip_approval` (behind `lock_sign_freely`) and
`lock_wire.rs` `lock_acceptor_accept` (behind `lock_step`). Everything
else (founding roster, recovery test fixtures, vault deposits) signs
other bytes or is test code. Callers of the first door:

| path | entry | at a locked height |
|---|---|---|
| approve | `proposals.rs` `cmd_approve` | intent |
| proposer's own approval | `proposals.rs` `propose_payload` | intent |
| membership propose / restore auto-approve | `membership.rs` `propose_membership_with`, `auto_approve_restore` | intent; the accept re-checks consent, `restore_gate`, `sign_blocker` |
| cut propose / co-sign | `checkpoint.rs` `cmd_propose_checkpoint`, `receive_checkpoint_proposal` | intent; the accept re-checks the hash |
| re-base after a head move | `governance.rs` `rebase_pending_approvals` | nothing re-signed (intents stand) |
| B5 held approvals after a heal | `governance.rs` `sign_held_approvals` | intent |
| reorg re-sign | `sync.rs` `try_reorg` | intent |
| wallet co-sign | `wallet.rs` `wallet_review`, `wallet_run.rs` `wallet_cosign_review` | intent; the accept re-checks |
| release re-sign | `lock_wire.rs` `lock_try_release` | free (the mark is written by the release) |

`sign_blocker` (B5 relay wait, evicted seat, chain behind) runs before
the mode gate, so a held approval consumes no free mark. The verified
park (`park_approval`) and relay approvals (`receive_approval_via`) only
receive; lock signatures never enter `pending_sigs`, so no re-serve can
turn one into an `Approved`.

## 4. Steps

1. Merge the WIP onto master, then master 0.1.0 and its acceptance
   stages (settle, open-card cap, gap hold): done; all conflicts additive.
2. Re-derive the signing paths: done, re-checked after each merge (§3).
3. Review the release rule: counterexample found and corrected (§5).
4. Activation as chain state (user decision 2026-11-22): done (§8).
5. Suites with the lock on by default for new foundings: §9.
6. Lossy trio and lock integration tests, five runs each: §10.
7. Two code reviews (S1-S6, W1-W10): fixed except S6, design §14.
8. Re-check the gap-hold pins when the frontier rework lands on master.
9. Traffic measurement over Tor against a real relay: open.
10. Acceptance on field copies (a v0.1.0 republic enabling it by vote, a
    fresh founding): open.

## 5. The release rule (review result)

The rule as first wired (promise-only state, `|F| >= n - m`) is safe but
not live: 2-of-2 and 3-of-3 stall for good when the leader already
accepted, 2-of-5 when two seats signed freely (phase 1 can never
complete, the rule waits for three). Pinned at seeds 2, 1, 0. Corrected:
release all lock state once `|F| >= min(m, n - m + a)`; proof in design
§14; 16 shapes x 800 seeds with a liveness phase pass.

Blocking the default: a card whose voices straddle a free seat and lock
seats (3-of-5: one free, two lock) waits for more lock voices where B6
would seal (design §14, pinned). Not rare: after a founding the first
approvals are free (no capability known yet), the next ones intents;
the vault suite (2-of-4) stalls at height 1 in every test that waits
for a commit. Proposed correction: replace the race by one agreed
activation height - the founding fixes it for a new republic (every
founder runs the build), a chain block names it for an existing one -
so no honest seat signs freely at a height another locks. The release
rule then only serves a seat restored onto an older build.

## 6. Suite with the lock on (the default flipped locally)

`molt-engine`: 1 316 passed, 32 failed before the fixes below.

| failures | cause | fix |
|---|---|---|
| 16 `nostr_recovery` | the restore's subject led the height (B6's line), the survivor waited a full grace | the lock line leaves it out; held restore seals retried |
| 1 of them, `a_bystander_that_slept_through_a_recovery_still_hears_the_rejoiner` | one survivor restores while the other sleeps: impossible under the lock by decision | runs on the capability-off seam |
| 13 vault (`vault_complaints` 6, `vault_deposit` 1, `vault_grant` 4, `vault_late_enable` 1, `vault_recovery` 1) | the straddle at height 1 (§5) | none: blocks the default |
| `a_reorg_keeps_the_approvals_it_releases_from_the_park` | intents displayed at a free height | intents count only at locked heights |
| `a_consented_restore_under_the_lock_needs_both_survivors` | the wait text names the missing seat now | expectation updated |

After the fixes: `nostr_recovery` 33/33; the 13 vault failures remain.
`checkpoint_under_load` (red in the first attempt) passes with the lock
on.

Also open: under the active lock a restore needs promises from
n - m + 1 seats, so in 2-of-3 BOTH survivors must be reachable to bring
the third back (the status shows `promises 1/2 missing <seat>`, the log
`lock=wait reason=restore_needs_promises`).

## 7. Measured (2026-11-22, loopback relay, 4-core box shared with a release build)

`chain_convergence.rs`, 2-of-3, two bursts of nine pages, 10 % inbound
loss, five runs each. Frames = every event the relay stored during the
bursts (votes, lock frames, blocks, acks, presence), per sealed block.

| | fork decisions | seconds | frames per block |
|---|---|---|---|
| lock | 0, 0, 0, 0, 0 | 93, 93, 78, 74, 83 | 24.0, 24.2, 23.3, 22.8, 23.7 |
| B6 | 0, 0, 0, 0, 0 | 111, 112, 110, 114, 123 | 55.5, 52.3, 50.8, 50.5, 50.4 |

The first attempt's estimate (about twice the frames) does not hold on
this relay: the lock sends fewer than half, because B6's re-signed
approvals and re-serves after every head move disappear. Not yet
measured over Tor against a real relay.

`chain_lock.rs` (7 tests), five runs: 7/7 in four; in one run, under
load, `a_capability_that_flips_mid_run_neither_stalls_nor_forks` found no
common head within 300 s. Cause: approvals cast as unsigned intents while
the lock ran were never signed when the height turned free (a B6 re-base
re-signs collected signatures only). Fixed: the tick signs standing
intents freely once per height that turns free; the flip test then
passed 12 of 12 under the same load (1 of 8 failed before).
The keystone (bursts, loss) had 0 fork decisions in all five runs,
63-85 s.

## 8. Activation (decided 2026-11-22, built)

The user chose both shapes of the proposal that stood here: a republic
founded with this build ratifies `chain-lock` with its charter; an older
one enables it by a `set_features` vote. The research verdict, what an
older build does in each case, the boundary argument and what was
deleted are in the design's §8. The 13 vault tests that stalled on the
activation race found fresh republics, so they now run locked from
height 1 with no race.

## 9. Suite with the lock on by default (chain activation, 2026-11-22)

(Superseded by §11: a founding starts without the lock.) Every founding
ran under the lock then. First full `molt-engine` run:
1 348 passed, 12 failed; each traced:

| test | cause | resolution |
|---|---|---|
| `the_feature_set_is_bound_into_what_every_member_signs`, `founding_ritual_completes_across_two_instances`, `vault_late_enable` charter check | the charter carries `chain-lock` | expectation updated (the decision) |
| `a_poke_crosses_the_mesh_and_wakes_the_opted_in_target` | counted the leader's `LockPrepare` as an application envelope | lock frames left out of that count |
| `a_reopened_member_recovers_open_proposals_from_the_mesh` | pins the B6 re-serve of a collected signature | founds pre-lock |
| `recovery_distributes_the_rekey_commit_to_a_live_survivor` | its "live survivor" is a stub that never promises | founds pre-lock |
| `a_restore_sealed_after_its_withdrawal_is_reported_and_a_new_link_heals_it` | a B6 race: sealed with a pre-collected signature | founds pre-lock |
| `a_bystander_that_slept_through_a_recovery_still_hears_the_rejoiner` | one survivor restores alone | founds pre-lock |
| `a_forged_depositor_is_refused_by_approver_and_verifier` | colluders reaching m pin the slot (design §14) | founds pre-lock; residual documented |
| `a_withdrawn_relay_is_not_redialed_by_the_running_republic`, `wallet_stage`/`wallet_scan` `the_fork_pauses...`, `a_backup_restore_keeps_the_share`, `enable_then_cut_then_recover_and_read` | load (two suites and a GUI run on four cores) | green alone, repeatedly |
| `a_payment_confirms_after_20_blocks_and_a_reorg_rewinds_it` | two founding wallet inits race; the lock's leader can pick the card a seat never claimed (design §14) | open: 2 of 7 locked runs, 0 of 8 B6 runs; a consent change for decision |

After the master merge (restore settle, open-card cap, gap hold): the lock
accepts a `Restored` value only after the settle; intents wait behind a
gap as stances, lock frames never wait. `a_consented_restore_under_the_lock_needs_both_survivors`
minted its link before every seat announced and got a v1 consent (a
human vote); it now waits for the announcements, as the recovery tests do.

## 10. Measured on the chain-activation build (2026-11-22)

`chain_convergence.rs`, 2-of-3, two bursts, 10 % inbound loss, five runs
(lock: a republic founded with this build; B6: founded pre-lock):

| | fork decisions | seconds | frames per block |
|---|---|---|---|
| lock | 0, 0, 0, 0, 0 | 21, 94, 5, 36, 80 | 16.9, 21.6, 16.7, 17.8, 19.5 |
| B6 | 0, 0, 0, 0, 0 | 92, 90, 90, 90, 89 | 49.7, 48.0, 49.5, 53.0, 50.3 |

After the review round and the master merges (`7d1055ec`), five more runs:

| | fork decisions | seconds | frames per block |
|---|---|---|---|
| lock | 0, 0, 0, 0, 0 | 50, 47, 38, 35, 50 | 20.4, 22.5, 20.3, 19.2, 19.5 |
| B6 | 0, 0, 0, 0, 0 | 90, 90, 90, 89, 92 | 55.0, 56.0, 52.5, 57.4, 53.8 |

`chain_lock.rs` there: 8/8 in five of five runs, 40-48 s.

`chain_lock.rs` (8 tests, in parallel - each founds its own republic on
its own relay): 8/8 in five of five runs, 39-45 s per run (before: 512-600
s, two tests sitting out 120 s and 240 s deadlines). The keystone: 0 fork
decisions every run. Waits driven through seams: the fallback grace of
both sealing lines (`__lock_timing`, now also B6's sealer turn) - the
enabling-vote test went from 118 s to 32 s; the restore settle step has
`__restore_settle_secs` (unused by default).

`vault_late_enable` `enable_then_cut_then_recover_and_read`, three runs
each: lock 68, 54, 29 s; B6 39, 35, 36 s.

## 11. Activation by the members (decided 2026-11-22; the founding part built, the UI and MCP part not)

- Enabled EXPLICITLY in the UI under Organization (where the feature set
  lives), never silently. A republic founded on a build with the lock
  starts WITHOUT it unless the members choose it. This REPLACES "every
  founding of this build adds `chain-lock`" (built: `founding.rs` adds
  the key only through the test seam `__found_locked`). Open, to confirm when the
  UI is built: the same choice in the founding wizard's feature step,
  default off.
- Name: everything a human reads says "Strict Order Mode" (DE "Strenge
  Reihenfolge"), the action "Activate Strict Order Mode"; the key
  `chain-lock`, code, wire events, status and log fields keep their names.
- Choosing it opens a modal in plain words, no protocol vocabulary (no
  quorum, ballot, promise, fork, height); rows computed for this
  republic's n and m (example 2-of-3). Headline: "Strict Order Mode - one
  decision at a time. No two versions of your republic, ever."

  | | Today | With Strict Order Mode |
  |---|---|---|
  | Two conflicting decisions at once | rare, heals by itself | cannot happen among members running unmodified software |
  | Network traffic | a lot | about a quarter |
  | A member loses their device | one other member can bring it back | all remaining members must be online (here: 2 of 2) |
  | Members that may be missing | n-m | min(m-1, n-m) |
  | Republics where every member must approve | works | not offered |
  | Taking a proposal back | usually works | only until it was called up |
  | Switching it off | - | not possible |
  | Using an older version of the app | works | no longer possible |

  Below it one sentence: it pays off with many members or agents
  proposing at once and everyone reliably online; not with members often
  away for days, or a large republic with a small majority (2-of-5: 4 may
  be missing today, 1 with the mode). The modal names seats on a build
  without it, or not yet announced.
- Confirming the modal PROPOSES the vote (the existing `set_features`
  path, m of n); the lock is active from the height after that block.
- MCP is co-equal: a read returns the same rows as data for this republic
  (n, m, members needed to bring a device back, members that may be
  missing with and without, seats lacking the capability); the `propose` description,
  `status.chain_sync` and the refusal texts carry the same facts; no step
  only the window can do.
- Preconditions at propose and approve: every seat capable; m >= 2 (a
  legacy m = 1 chain must not enable it); m < n (where every member must
  approve, a member lost mid-decision would stop the republic for good);
  no restore open.

## 12. Not built, required before the mode leaves the dark

- The answer frame for a withheld acceptance (so a leader learns why).
- An approve control in the window for a withdrawn card the status names
  (item 3's exit). REQUIRED before the mode is offered: MCP `approve`
  resolves it today, and a state only MCP can resolve breaks co-equality.
- Status texts for every state where nothing is ready.
- The activation modal and its MCP data (§11).
- The rejoiner's new device and lineages in the simulation.
- A decision on two simultaneous founding wallet inits.
- A Tor traffic measurement.
- A field acceptance.

### Sixth review (a7c12a17): open design decisions

Findings, not decisions; F1-F5 are fixed (design §14, "Review 6").

- (a) The forced-value rule has had four versions (own intent / any
  promiser / one signer / two signers); the fourth still let a leader
  sign a made-up change (F1). Candidate simplification: a seat signs only
  a change whose DIGEST it approved, forced or not; a seat that did not
  approve it never completes it automatically - staged wait, then the
  operator reads the card and approves. No evidence frames at all. Price:
  the operator prompt whenever the approvers of a possibly-chosen value
  are gone.
- (b) #4: every import is amnesiac; in 2-of-3 two overlapping imports, or
  one import while another seat is lost and not yet restored, wait for
  ever, and the status `restore this seat with a new link` names an exit
  that cannot be decided in exactly that state (a restore needs m
  acceptors and n - m + 1 promisers among non-amnesiac non-subjects).
  `two_amnesiac_seats_of_three_say_restore` checks only the string.
- (c) #7: a device restored by the recovery ritual is NOT amnesiac,
  although the old device may have promised or accepted at r + 1 before
  the re-key reached everyone (the review's n = 4, m = 2 schedule). The
  cheap close (a ritual-restored device amnesiac like an import) deepens
  (b).
- (d) #6: completion cards can be flooded, read as proposed "by Y" from
  the unauthenticated id stripe with no `relayed_by`, and pre-empt Y's
  next ids.
- (e) #9, #10 (open as the review states them). Also: an amnesia answer's
  `touched` is taken as reported - one lying answer can hold the asker
  passive.
- (f) §11's row about a misbehaving member is not proven while F1-F4 were
  open and the forced-value rule is undecided: removed until proven.
  "Two conflicting decisions cannot happen" holds among members running
  unmodified software.
- Note for the explorer's author: the header of `chain/lock_explore_tests.rs`
  names functions that no longer exist (not edited here).
