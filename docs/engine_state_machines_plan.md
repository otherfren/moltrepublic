# Pure engine state machines and a deterministic simulator

**Status: PLAN, not started; follows v0.1.1.** Decision (user,
2026-11-22): speed up development and catch protocol bugs earlier by
moving decision logic into pure, I/O-free, clock-free state machines
that a deterministic multi-seat simulator drives, the way
`crates/molt-engine/src/chain/lock.rs` + `lock_tests.rs` already do. No
crate cut first; a separate crate comes last, only if still wanted.
Measured 2026-11-22 on master `a431fb69`.

## 1. Goal and non-goals

Goal: protocol defects at the seams between chain, recovery and delivery
show up as a failing seed in seconds, not as a field incident or a
load-flaky 60 s integration wait.

Non-goals:

- no crate cut before the boundary is stable (stage S9 decides);
- no behaviour change: every move is a refactor pinned by tests first;
- no signed layout, wire or persisted-format change (`approval_bytes`,
  block link, roster, checkpoint, consent bytes keep their tags and pins);
- the actor stays: one owner, synchronous handlers, one command set
  executed in one place; pure modules are called BY the actor;
- no generic simulation framework, no effect traits beyond one enum per
  module.

## 2. What exists (measured)

Lines are production files; tests live in sibling `*_tests.rs` files.
"Reach" = `self.<field>` accesses into engine state outside the module's
own struct (counted with grep, approximate).

| module | lines | reach into `State` | clock / RNG / I/O | tests today |
|---|---|---|---|---|
| `chain/governance.rs` (seal line, `try_commit`, tally, parks, held approvals) | 1 879 | `chain` 107 (22 distinct fields), `proposals` 16, clock 6, `record`/`make_env` 5, vault 6, `nostr` 1 | `presence_now()`; pacing keyed on `self.nostr.is_some()` | 70 unit (`governance_tests.rs`) over whole `State`s |
| `chain/membership.rs` (restore gates, settle, consent, withdrawal, re-key) | 1 514 | `recovery` 47 (13 fields), `chain` 40, `proposals` 8, `seat_active` seam, `delivery.evicted`, `group_net` 2 | clock 2, `now_secs` 1, `SystemTime` 4; settle keyed on `nostr` | 56 unit |
| `chain/sync.rs` (tie-break, reorg, relink, catch-up, heal tick, beacon, re-push) | 1 160 | `chain` 142 (27 fields), `recovery` 12, clock 9, `session` 6, `persist_chain_now` 5 | clock; 10 timer/backoff consts | 41 unit |
| `chain/checkpoint.rs` | 448 | `chain` 34 (8 fields), wiki/vault/wallet adoption | none | 26 unit |
| `chain/projection.rs` | 715 | `chain` 36, `nostr` 3, relays, replica | none | 34 unit |
| `chain/verify.rs` | 1 338 | none (own walker struct) | none | 46 unit |
| `chain/lock.rs` | 294 | none (pure) | none | 19 unit + seeded sim: 9 configs x 1 500 seeds |
| `net/recovery.rs` (coordinator attempts) | 1 501 | `recovery` 57, `session` 14, net scopes, `cmd_tx` | clock, 3 `tokio::time`, 9 spawn/await | 11 unit + `tests/nostr_recovery.rs` |
| `nostr_ritual.rs` rejoiner (`await_admission`, `recovery_rejoin`, `seal_first_anchor`) | ~490 of 2 770 | async task, talks to the engine by `Command` | 42 `tokio::time` in file | integration only (`nostr_recovery.rs`, MockRelay) |
| `net/delivery.rs` (accept point, G7 hold by need, gap want, acks) | 685 | `delivery` 44, `group_net` 5, clock 5 | clock; 1 RNG | 8 unit + `nostr_inbound_gap.rs`, `delivery_guarantee.rs` |
| `net/presence.rs` (ticks, health, genesis stop) | 554 | `delivery` 20, `session` 14, `group_net` 9 | `tokio::time` 1 | 21 unit |
| `molt-net/src/supervisor.rs` + `group_runtime.rs` (outbox, floor, rewind, resend, gap answer) | 2 489 + 3 757 | n/a (async tasks) | timers, backoff RNG, sockets | 8 + 35 inline, `crates/molt-net/tests/` |
| already pure in molt-net / molt-core | `gap.rs` 200, `group_ack.rs` 211, `epoch_hold.rs` 170, `molt_core::AcceptedWindow` | none | none | unit |

Shared state: `State` has ~127 fields; `ChainProjection` alone ~50,
mixing the persisted chain, derived caches and timers (`ready_at`,
`heal_next_at`, `beacon_at`, `repush_check_at`, ...). `chain.pending_sigs`
is touched from 8 production files, `recovery.settle/held_approvals/
withdrawn/held_seals` from 3.

Tests (molt-engine): ~1 015 unit tests in `src`, 320 async integration
tests in 70 binaries (227 multi-threaded; 158 of them relay-backed over
`nostr_relay_builder::MockRelay`). `cargo test` runs the 70 binaries one
after another. The full engine suite takes ~20-30 min on 4 cores, the GUI
suite ~48 min (user figures; S0 re-measures per test).

Findings that shape the plan:

1. **The pacing rules are off in every loopback test.** `seal_pace_secs()`
   (`SEAL_PACE_SECS`, `SEALER_GRACE_SECS`, the sealing line's wait) and
   `restore_settling` (`RESTORE_SETTLE_SECS`) return "no wait" unless
   `self.nostr.is_some()`; unit tests plant a fake `NostrTransport` to turn
   them on. B6 and F1 are exercised only by MockRelay tests and those unit
   tests.
2. **"Sealable" is judged twice.** `try_commit` counts signatures it
   verifies live; `still_sealable` counts `pending.verified`. The C2 fix
   (stale "sealable since") holds only while both agree.
3. **A synchronous multi-seat harness is close.** Unit tests already build
   whole `State`s (`tests::plain_state`, `chain_peer_3`), drive them through
   the synchronous `State::handle` and `cmd_net_delivered`, set the clock
   through `presence.clock_override`, and `record()` already has a
   `cfg(test)` tap (`RECORDED_CHAIN`). The real code can be simulated
   before anything moves.
4. **Merge friction is measurable.** Re-running the 16 merges since
   2026-11-08 with `git merge-tree`: 5 conflicted, in `chain/mod.rs` (2),
   `lib.rs` (2), `net/presence.rs` + `presence_tests.rs` (2),
   `chain/governance.rs` (1). Most commits since then: `lib.rs` 38,
   `events.rs` 24, `proposals.rs` 22, `chain/sync.rs` 17.

## 3. Library verdict

Checked 2026-11-22 with `cargo search` / `cargo info` (versions and
licences verified; maintenance from crate metadata and my knowledge, not
from commit history, which was not checked).

| candidate | what it is | fit here | verdict |
|---|---|---|---|
| `turmoil` 0.7.2, MIT (tokio-rs) | simulated hosts + network under one tokio runtime; faults on links | simulates sockets; our decisions sit above the transport, and the relay path runs over our own dialer/WebSocket stack | leave for now; revisit for a relay-level test of `group_runtime` |
| `madsim` 0.2.34, Apache-2.0 | deterministic runtime replacing tokio/time/rand via `--cfg madsim` | whole dependency graph rebuilt under a cfg; cost on a box where the window build already has no headroom | leave |
| `shuttle` 0.9.10, Apache-2.0 (awslabs) | randomized scheduler for threads/async tasks in one process | our bugs are cross-seat, not cross-thread; the actor is single-owner | leave |
| `loom` 0.7.3, MIT | exhaustive memory-model permutations of atomics | not our class of bug | leave |
| `stateright` 0.31.0, MIT | explicit-state model checker with an actor model (`Actor`: `on_start/on_msg/on_timeout`, `Storage` for crash-restart, lossy/duplicating networks) | exact fit for SMALL pure cores; state must be `Hash + Clone`, so N whole engines explode | take later, optional (S9): exhaustive check of the lock and the sealing line |
| `proptest` 1.12.0 + `proptest-state-machine` 0.9.0, MIT OR Apache-2.0 | property tests with shrinking; reference-model vs system-under-test op sequences | shrinking a failing schedule to a short trace; differential test old-vs-moved code | take as dev-dependency (S1): schedules are a choice sequence it can shrink |
| `quickcheck` 1.1.0, Unlicense OR MIT | older property testing, weaker shrinking | proptest covers it | leave |
| `arbtest` 0.3.2, MIT OR Apache-2.0 | minimal property harness over `arbitrary` (already in `Cargo.lock`) | smaller alternative to proptest | fallback if proptest's dependency weight is refused |
| sans-IO style (pattern: `quinn-proto`, `rustls`; ours: `lock.rs`, `AcceptedWindow`) | state machine takes inputs incl. time, returns outputs | the target shape | take (the rule in §4) |
| own seeded harness | `lock_tests.rs` splitmix64 + chaos loop | proven, no dependency | take as the simulator core, generalized |

A consensus crate is not the answer (evaluated twice, `docs/chain/chain_lock_design.md` §1):
an acceptance here is a signed vote in an existing byte format that a seat
may withhold, and quorums are m-of-n with n-of-n cuts and consented
restores; every Raft/Paxos/BFT library owns its own log and accepts any
value at a fresh ballot.

## 4. The target shape

Rule for a pure module:

- owns its state in its own struct; never sees `State`;
- inputs carry everything: `now` (seconds), ids, the facts it needs as
  plain values (seat sets, verified-signer sets, the open set at a height);
- outputs are a decision or a `Vec<Effect>`, plus the state change; it
  performs nothing;
- deterministic: `BTreeMap`/`BTreeSet` only, no RNG (randomness is an
  input), no `HashMap` iteration in a decision;
- crypto stays outside: inputs say "verified", signing is an effect;
- persisted state is serde with `#[serde(default)]` and a byte-pinned
  shape test (as `lock_tests::the_persisted_shape_is_pinned`);
- `pub(crate)`, a module doc listing the API, tests in `<module>_tests.rs`.

The engine side is a thin adapter method on `State`: gather inputs, call
the core, apply effects in order. Persist effects come first; a failed
write skips the frames behind it (the lock wiring's rule).

| effect | engine call today |
|---|---|
| `Broadcast(WorkspaceEvent)` | `self.record(self.make_env(..))` (outbox fans it out) |
| `Ask { height }` | `ask_for_height` / `send_chain_request` |
| `Seal(block)` | `adopt_committed_block` |
| `Persist(what)` | `persist_chain_now`, `snapshot_to_disk`, the lock file write |
| `Notice(..)` | `emit_session`, `tracing::warn!(key=value)` |
| `Task(..)` | spawned re-key / Welcome / anchor push (stays in the engine) |

Worked example, the sealing line (`chain/governance.rs`). Before: one
method reads the clock, roster, last-seen times, `chain.diverged`,
`chain.ready_at`, `self.nostr`, re-judges sealability itself, writes
`ready_at`/`fallback_asked`, and sends.

```rust
impl State {
    fn sealer_turn(&mut self, id: u64, target: u64, change: &ChainChange) -> bool;
    fn prune_ready_at(&mut self, target: u64);          // calls still_sealable
    pub(crate) fn sealing_line(&self, _height: u64) -> Vec<String>;
}
```

After: the core decides; the caller supplies the set of ids sealable at
`target` (one counting rule, from S4), so a stale "sealable since" cannot
exist by construction.

```rust
// chain/seal_line.rs - no State, no clock, no I/O
pub(crate) struct SealLine { ready_at: BTreeMap<u64, (u64, u64)>, asked: Option<u64> }
pub(crate) struct Seats<'a> { pub me: &'a str, pub roster: &'a [String],
    pub unseen: &'a BTreeSet<String>, pub diverged: &'a BTreeSet<String> }
pub(crate) enum Turn { Seal, Wait, Ask { height: u64 } }
impl SealLine {
    pub(crate) fn line(seats: &Seats) -> Vec<String>;
    pub(crate) fn turn(&mut self, now: u64, target: u64, id: u64,
        sealable: &BTreeSet<u64>, seats: &Seats, excluded: Option<&str>,
        timing: &Timing) -> (Turn, bool /* hand signatures over */);
}
// governance.rs adapter
fn sealer_turn(&mut self, id: u64, target: u64, change: &ChainChange) -> bool {
    let (turn, hand_over) = self.chain.line.turn(self.presence_now(), target, id,
        &self.sealable_at(target), &self.seats(), restored_member(change), &self.timing);
    if hand_over { self.reserve_signatures(id); }
    match turn { Turn::Seal => true, Turn::Wait => false,
        Turn::Ask { height } => { self.ask_for_height(height); false } }
}
```

## 5. The simulator

One harness, `crates/molt-engine/src/sim/` (`cfg(test)`, crate-private
access), grown in three fidelity levels:

| level | seats are | simulated | stubbed |
|---|---|---|---|
| Sim-A (S1) | N whole `State`s (`chain_peer_3`-style fixtures, real Ed25519) | governance, membership, sync, verify, delivery accept point and G7 hold, the virtual clock (`clock_override`), ticks (`NetDeliveryTick`, heal, presence) | MLS (frames in clear; a group split = a drop matrix), relay (direct delivery with delay), the supervisor's ack/resend (lost stays lost until the schedule's reliable phase), spawned tasks (re-key, Welcome, anchor push complete as harness intents); crash = rebuild from the seat's chain blocks only |
| Sim-B (after S3-S5) | pure cores + adapters' input builders | as Sim-A plus explicit `Persist` effects: crash between a state change and its persist; version skew per core (rule off = older build) | as Sim-A |
| Sim-C (S7-S8) | pure cores + delivery and rejoin machines | a relay that stores and replays newest-first with paging and a bounded subscriber buffer; outbox floor, rewind, resend backoff; `AcceptedWindow` aging; eviction by re-key, two re-keys at one epoch; the rejoiner machine | sockets, Tor, real MLS crypto (kept in the integration keystones) |

Network faults per schedule: loss, delay, duplication, reordering,
partitions, permanent loss (relay paged out), an MLS-group split, crash
and restart (with amnesia at Sim-A, with a torn persist at Sim-B), a
downgraded seat (B6 or lock off), withdraw/decline/gate/settle by any
seat. A schedule is a choice sequence from a seed (splitmix64 as in
`lock_tests.rs`); proptest shrinks it.

Invariants (checked after every step unless marked):

| id | invariant |
|---|---|
| I1 | never two different changes sealed at one height on one lineage (god's-eye over all seats' blocks) |
| I2 | every seat's chain passes `verify_chain` |
| I3 | after faults stop (reliable phase), all live seats reach one head within a bound, and every open height decides (end of run) |
| I4 | nothing owed is lost: every own ackable envelope of a live seat is applied by every live peer (end of run) |
| I5 | no `Restored(X)` seals while X's node is live in the group (god's-eye) |
| I6 | a withdrawal delivered to a sealer before it seals keeps the restore unsealed; one sealed elsewhere is reported, never re-keyed |
| I7 | while no ack arrives, frames published per seat stay under a budget per virtual hour (Sim-C) |
| I8 | a held envelope is applied before it ages below the accept window (Sim-C, partly Sim-A) |
| I9 | a current decliner's signature never counts toward m |

Failure output: the invariant, `SIM_SEED=<n> SIM_CONFIG=<name>`, and the
shrunk trace (one `key=value` line per delivered event, last 200). Replay
with `SIM_SEED=<n> cargo test -p molt-engine sim_replay`. Default runs a
small seed count inside the normal suite (< 30 s); a long run is
`#[ignore]` with `SIM_SEEDS=<n>`.

Which of this round's defects each level would have caught (honest
estimate; nothing here was run):

| defect | caught at | why / why not |
|---|---|---|
| per-proposal sealing line forks (pre-B6) | S1 | real sealing code, 3 seats, bursts, loss: I1 |
| stale "sealable since" after a withdrawal (C2) | S1 if withdrawals are in the op mix; impossible by construction after S3 | I1 via a fallback sealing beside the turn-holder |
| rejoiner deadlock on a replay above its buffer (R11) | S8 only, and only with the bounded subscriber buffer modeled | the bug was async plumbing (awaiting end-of-replay without reading); `nostr_recovery.rs`'s 12 000-event rejoin stays |
| resend flood to silent peers (R12) | S7 | supervisor resend is stubbed before; needs eviction + I7 |
| the 900 s ordered park | S1 partly (permanent loss + I3 bound flags a deaf seat); S7 fully | the sender's gap answer is stubbed at Sim-A |
| accept-window jump that loses envelopes | S1 likely, with a flood schedule reaching the window; S7 certain | `cmd_net_delivered` runs the real window; I8/I4 |
| withdrawn restore sealed by a bystander | S1 with consented restore cards (`consent_for` fixtures) and the paced seam on; by construction after S5 | I6 |
| the lock's stuck forced value after a withdrawal | S2 | today's lock sim has no withholding acceptor; I3 |
| the downgrade fork | S1, approximated | a "downgraded" seat is a rule switched off, not the older binary; the mixed-version E2E stays |

## 6. Stages

Each stage: red characterisation tests first (they pin today's behaviour
through the adapter, green before and after the move), then a move-only
commit, then the simulation added. Clippy 0 per crate; no window build.

| stage | what moves | size / risk |
|---|---|---|
| S0 | test-time hygiene, nothing moves | S / low |
| S1 | Sim-A over whole `State`s | M / low |
| S2 | lock simulation gaps, lock into Sim-A | S-M / low |
| S3 | sealing line -> `chain/seal_line.rs` | S / low |
| S4 | approval tally, parks, held approvals, cut need -> `chain/tally.rs` | M / medium |
| S5 | restore decision -> `chain/restore_rules.rs` | M / medium |
| S6 | fork rule and catch-up -> `chain/fork_rule.rs`, `chain/catchup.rs` | L / high |
| S7 | delivery rules (inbound frontier, outbox reach) | M-L / medium |
| S8 | recovery attempt machines (coordinator, rejoiner) | L / high |
| S9 | optional: stateright check, crate cut decision | S each |

S0 and S1 come first: neither moves code, both pay at once, and S1 is the
net every later move is checked against. Among the moves S3 is the
cheapest with the highest value: ~300 lines, already half pure
(`sealer_wait`), the hook the lock calls, and the site of B6, C2 and the
fork history.

**S0. Test-time hygiene.**
- `cargo nextest` as the runner (a dev tool, not a dependency): one process
  per test, binaries in parallel, per-test timings, a test group capping
  relay-backed tests, `slow-timeout` reporting; `.config/nextest.toml`.
- A `Timing` struct on `State` (an `Arc` seam like `seat_active`) holding
  today's constants: `PRESENCE_TICK_MS` 30 000, `STATUS_RESEND_SECS` 30,
  `SEAL_PACE_SECS` 5, `SEALER_GRACE_SECS` 60, `SILENT_AFTER_SECS` 120,
  `RESTORE_SETTLE_SECS` 5, the gap and ack debounces. Production uses the
  defaults; tests pick a fast profile. Unit tests pin the production values.
- Replace the transport check behind pacing with an explicit `paced` flag
  (finding 1), set by the Nostr transport and by tests.
- Condition waits instead of sleeps: `every_seat_announced`
  (`nostr_recovery.rs`, 30 s deadline; `vault_support`, 60 s) waits on the
  30 s status tick and goes sub-second with the fast profile; audit the
  68 `sleep(` calls in `two_instances.rs` and 22 in `nostr_recovery.rs`.
  Tokio's paused time does not help: the engine clock is `SystemTime`
  unless `clock_override` is set.
- Flaky list: 5 full runs under load with nextest retries; the tests that
  needed a retry are named in the measurement table (§9), not guessed.
- Shrinks: nothing yet; it makes the suite cheaper as is.

**S1. Sim-A.**
- Interface: `World::new(cfg, seed)`, `step()`, `run_until(t)`, `check()`;
  seats via `State::handle(Command)` and `cmd_net_delivered`; outgoing
  envelopes from a widened `cfg(test)` tap in `record()` (the whole
  envelope, not only `RECORDED_CHAIN`'s kind and height).
- Pins first: none needed (no move); the existing 1 000 unit tests stay.
- Adds: configs 2-of-2, 2-of-3, 3-of-5 with bursts, loss, withdrawals,
  declines, consented restores, a downgraded seat; I1-I6, I9.
- Can shrink: `tests/chain_convergence.rs`, `checkpoint_under_load.rs`,
  `vote_at_a_moving_head.rs` to one keystone each.
- Conflict surface removed: none (adds files only).

**S2. Lock gaps.** In `lock_tests.rs`: acceptors that withhold (withdraw,
decline, gate, settle), a head move or fork below the slot (lineage
change), amnesia, a downgraded seat; liveness under those. Then the
lock's adapter runs inside Sim-A. Depends on the lock wiring being
merged.

**S3. Sealing line.** Interface as in §4. Pins first: the line's order,
the k-th seat's wait, ask-then-seal, the restored seat excluded, the
smallest-id rule, a withdrawn id dropping its start (existing
`governance_tests` cover most; add the missing ones). Then Sim-B for the
line alone at high seed rates. Removes: `ready_at`, `fallback_asked`,
`sealing_smaller` from `ChainProjection`; the B6/lock merge hotspot in
`governance.rs`.

**S4. Tally.** `Tally { height, sigs, verified, decliners, consent }`
with `sealable(need) -> Option<Vec<Attestation>>`, the park
(`ParkedApproval`: verified rank, per-sender cap) and held approvals
(`sign_blocker` reasons as an enum). One counting rule for `try_commit`
and the line (finding 2). Cut need = n and the checkpoint co-sign guards
(`chain_unsettled`) fold in here; `checkpoint.rs` gets no stage of its
own. Pins: D2 (current decliner), consent counts once, re-base clears,
head+2 park. Shrinks: approval-order integration cases in
`two_instances.rs` that only count signatures.

**S5. Restore decision.** `RestoreRules` over `restore_gate`, settle,
`consent_never_seals`, sticky withdrawal, sealed-after-withdrawal report,
`readmission_in_flight`, and the coordinator's attempt book (cap 4,
spent/seen rings). Inputs: heard-at times, evicted flag, unsettled set,
open card ids. Pins: every refusal string, settle window, withdrawal
order. Shrinks: the gate and withdrawal cases of `nostr_recovery.rs`
(36 tests). Stays E2E: anchor-first rejoin, backlog rejoin (B1), both
mixed-version pairings, evicted node goes quiet, double recovery.

**S6. Fork rule and catch-up.** Pure decisions over block summaries
(height, prev, hash, change id): `tie_break` outcome, `try_reorg`'s
choice of fork point and suffix, relink plan, candidate TTL, catch-up
request window, heal backoff, beacon and re-push timers. Block storage,
`verify_chain` calls and persistence stay in the engine. Pins: the
`sync_tests.rs` set plus one test per timer. Removes ~15 timer/backoff
fields from `ChainProjection` and most of the `sync.rs` merge surface.
Highest risk: the reorg re-signs and returns displaced cards; keep the
move in two commits (decision, then timers).

**S7. Delivery rules.** Being reworked on another branch now
(per-sender frontier, held buffer, range listing): plan against the
intent of `docs_archive/transport/delivery_guarantee.md` §4 and §10 and
re-measure files after v0.1.1. Shape: an `InboundFrontier` beside
`AcceptedWindow` (hold by need, gap want/answer release rules, cap,
aging) and an `OutboxReach` in molt-net (floor, rewind epochs, backoff
ladder, give-up, eviction stop), both pure, driven by the existing async
tasks. Sim-C. Stays E2E: both hard-kill keystones
(`delivery_guarantee.rs`, `nostr_delivery_guarantee.rs`),
`nostr_inbound_gap.rs`, one `nostr_outbox_quiet.rs`.

**S8. Recovery attempt machines.** Coordinator (`net/recovery.rs`) and
rejoiner (`await_admission`, `recovery_rejoin`, `seal_first_anchor`) as
sans-IO machines; the async shells keep only reading, writing and timers.
Inputs: frames received, relay "answered/end of replay", time; outputs:
frames to send, step lines, outcome. Sim-C runs whole rejoins against the
relay model. Stays E2E: the B1 backlog test, anchor-first over MockRelay.

**S9. Optional.** stateright over `lock` + `seal_line` (exhaustive at
n <= 3, heights <= 2). Then decide the crate cut: only if the modules of
S3-S6 have not changed their API for one release.

Dropped: `chain/projection.rs` (a fold, not a decision under faults;
`verify`/fold tests cover it) and a separate checkpoint stage (in S4).

## 7. Order relative to other open work

| open work | rule |
|---|---|
| lock wiring, v0.1.1 (`docs/chain/chain_lock_wiring_plan.md`, branch `wip/chain-lock-wiring`) | nothing here starts before it is merged and released; S2-S4 touch the same code |
| delivery rework (other branch) | S0's `Timing` seam must not touch `net/delivery.rs` until it is merged; S7 starts after |
| rescue (`docs/chain/chain_rescue_plan.md`) | its design can start any time; its build must not run beside S5 or S6 (same files). Recommended: S0-S3, then the rescue built and tested in Sim-A, then S4-S8 |
| Robo-Scrum (`docs/robo_scrum_plan.md`) | touches `proposals.rs` and `lib.rs`; may run beside S0-S3, never beside S1's seams in `lib.rs` in the same week |

## 8. Risks

| risk | containment |
|---|---|
| behaviour drift during a move | characterisation tests first; move-only commits; for one stage the old method and the core run side by side under Sim-A on the same seeds (differential, proptest-state-machine), then the old one goes |
| simulator drifts from the engine | Sim-A runs the real `State`; Sim-B results are cross-checked against Sim-A seeds; adapters stay thin (gather inputs, apply effects) and are covered by the existing unit tests; E2E keystones stay |
| false confidence from a model that omits the real seams | every level lists what is stubbed (§5); the defect table states what each level cannot catch; findings from field runs become a fault class before they are fixed |
| over-abstraction | one effect enum per module, no traits; a module moves only if a simulator invariant needs it; stages that do not pay are dropped (projection, checkpoint) |
| a fast timing profile hides a production-timing bug | production constants pinned by unit tests on the pure decisions; one relay-backed keystone per timer family runs at production values |

## 9. Measures of success

Recorded before S0 and after each stage, in this doc:

| measure | how | baseline |
|---|---|---|
| engine suite wall time | `cargo nextest run -p molt-engine` from a warm build, idle box | user figure 20-30 min with `cargo test`; S0 measures |
| tests waiting on real time | nextest per-test timing, count >= 5 s | S0 measures |
| fixed sleeps in tests | `grep -c 'sleep('` over `crates/molt-engine/tests` | S0 counts |
| relay-backed integration tests | count over `tests/` | 158 |
| flaky tests | failures in 5 runs under load | S0 measures |
| seeds per second | per sim level | lock sim: 13 500 runs in < 1 min |
| merge conflicts | `git merge-tree --write-tree` over each merge commit of a release | 5 of 16 merges since 2026-11-08 |
| `State` reach of `chain/` | grep counts of §2 | `chain` 359, `recovery` 61, `proposals` 33 |
| defects found by a seed before the field | count per release | 0 |

## 10. Open questions (defaults in bold)

1. Where do the pure modules live? **Inside `molt-engine` beside their
   adapters (`chain/seal_line.rs`, ...);** only wire-shared types move to
   `molt-core`. A crate is decided at S9.
2. Simulation dependency? **`proptest` (+ `proptest-state-machine`) as a
   dev-dependency for shrinking and differential tests; the harness
   itself stays our own seeded loop; `stateright` only at S9.** Fallback:
   `arbtest`.
3. `cargo nextest` as the standard runner (a dev tool installed with
   `cargo install`)? **Yes**, `cargo test` keeps working.
4. Rescue before or after the extraction? **After S3, built against
   Sim-A; S4-S8 follow it.**
