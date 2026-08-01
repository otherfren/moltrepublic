# N4a follow-ups — verified execution plans

Status: **WORKING DOCUMENT.** Companion to `nostr_n4a_review_followups.md`,
which says WHAT is broken and why. This one says HOW to fix it: per cluster,
the verified anchors, the red test to write first, the ordered steps, the
files touched and the risks.

## Provenance, and what to distrust

Seven independent agents each investigated ONE cluster against the tree at
`88dd854`, then an eighth adjudicated the collisions between their plans.
Every plan reports `still_holds: true` — each agent confirmed its defect
against real code rather than trusting the backlog.

Two warnings, because this document ages badly:

1. **The line numbers were verified at `88dd854` and the tree has moved
   since** (clusters B, D and I landed, plus the connection diagnostics).
   Treat every anchor as a hint, not an address — re-read the symbol before
   editing. The FILE and SYMBOL columns age far better than the line.
2. **The agents deviated from their instructions.** They were told read-only
   and no cargo; several wrote files into the shared checkout and built
   anyway. Their reasoning is still worth having, but it was not produced
   under the discipline it claims — verify before acting on any specific
   claim, especially "X has no callers".

Clusters D and I have since LANDED. Their plans are kept as the reasoning
behind those commits, not as work to do.

**One claim HAS been verified by hand** — cluster H's `SealedRoster.roster`
finding, see "Open decisions" below. It holds, with a narrower severity than
the investigator implied. The other plans have NOT been re-verified.

## Ownership (agreed 2026-08-01)

Two sessions work this backlog in parallel. The split exists because the same
work was built twice in one day (the startup posture; cluster I nearly). It is
chosen so the two tracks share NO file:

| Track | Clusters | Crate |
|---|---|---|
| molt-net | **E, G** | `ritual_net.rs`, `relay_runtime.rs` |
| molt-engine | **C, F** | `founding.rs`, `lifecycles.rs`, `session.rs` |
| either, LAST | **H** | tests only |

**H goes last on purpose:** it pins seams that C changes. A keystone written
against today's publish seam would pin the shape C is about to replace.

## Open decisions — read before starting a cluster

Five clusters carry a genuine fork. Each has a recommended default, so work
can start; but a wrong guess is expensive in exactly these five places.

- **C — the Genesis delivery story.** (A, recommended) bounded retry of the
  SAME pre-encrypted frame plus an honest notice when no relay ever accepts —
  cheap, no wire change, and justified because relays STORE the 445, so one
  accepted relay reaches any member subscribing inside the h window. (B) a
  real `GenesisAck` round — a new `RitualMsg` variant, and the founder must
  keep the ritual's group and 445 receiver alive past `maybe_finalize`'s
  `take()`. A makes "we published it" true; only B makes "everyone got it"
  true. **Decide before step 6 of C.**
- **E — does persistent deafness ever become terminal?** The plan is "loud
  forever, never fatal" (the delivery-guarantee precedent). The counter is
  cluster F's own complaint: a wizard that warns visibly but never ends. If a
  ceiling is wanted (fail after N minutes of continuous deafness), it changes
  the caller code and adds an assertion.
- **F — post-birth retry.** The Welcome is bound to the joiner's first
  KeyPackage, whose HPKE private half dies with the aborted task, so a retry
  after group birth cannot resume without a fresh Welcome. In a 2-member
  republic the group is born the moment the single seat anchors, so ANY
  hiccup after acceptance forces a full re-mint. Recommended: refuse with an
  actionable message now, file per-seat re-key as later work.
- **G — which identity authenticates the kind-445 GROUP subscription?**
  `nostr_n4_plan.md` §10 decided the anchor for the 1059 INBOXES only, and
  445 is the opposite privacy case (its filter is an anonymous h tag).
  (a, recommended) a fresh ephemeral key per subscription — no API ripple,
  works wherever AUTH is anti-spam, but is refused by a relay that whitelists
  known pubkeys, plausibly our own recommended self-hosted onion relay.
  (b) the member's roster anchor — works when whitelisted, but permanently
  links the subscription to the member.
- **H — `SealedRoster.roster` is an UNSIGNED constitutional field.**
  **VERIFIED 2026-08-01 against `fb7e5fe` — the finding is real, and the
  severity is narrower than the investigator implied.** What was checked:
  - `roster_canonical_bytes` (`molt-roster-v3`) hashes `ws_id`, `rule_m`,
    `rule_n`, each identity's `(member, identity_pk, nostr_pk)` and `agenda`.
    `roster` is absent. ✔ claim holds.
  - `republic_id` (`molt-republic-id-v2`) hashes name, m, n and the sorted
    `(identity_pk, nostr_pk)` pairs. `roster` is absent. ✔ claim holds.
  - `verify_sealed_roster` (`founding.rs`) never reads `s.roster` at all —
    it checks the republic id, the anchors, the attestation count and the
    signatures over `identities` + `agenda`. ✔ no cross-check exists.
  - `SealedRoster::into_genesis` copies `self.roster.clone()` straight into
    `WorkspaceEvent::Founded`, which becomes `replica.roster` and therefore
    `State::roster()`. ✔ the wire value is materialized unverified.
  - The N1 genesis-time self-check does NOT constrain it either: it compares
    CANONICAL BYTES, and `roster` is not in them.

  **What it is NOT.** It is not a chain-authorization hole: `verify_chain`
  authorizes signers from `identities`, not from `roster`, and MLS membership
  binds credentials independently. `net.rs`'s `roster().contains(&from)` is
  only the fallback when no mesh is up (the primary is `peer_names`).

  **What it IS.** A founder can seal a republic whose materialized membership
  differs from the table every member cryptographically ratified — members
  sign `identities` and store `roster`. It drives the displayed member list,
  `rule_detail` (`replica.roster.len()`), the proposal/compaction paths and
  that fallback membership check. A sign-what-you-see gap in a constitutional
  field, so: **HIGH, not CRITICAL.**

  **Recommended fix — option (a), cheap and additive:** cross-check `roster`
  against `identities` (same set, same order) inside `verify_sealed_roster`
  AND `verify_seal_proposal`. No byte-layout change, no `molt-roster-v4`, no
  ~15-site ripple. It also matches what the codebase already does elsewhere:
  `recovery.rs::sealed_roster_from_blob` DERIVES `roster` from `identities`
  rather than trusting a separate field. Options (b) drop-the-field and (c)
  bind-into-the-bytes remain open but cost far more for the same guarantee.

  **This is a finding, not coverage debt — it should be fixed on its own,
  ahead of the rest of H.**


## Status

| Cluster | Title | State |
|---|---|---|
| A | 445 sender binding (CRITICAL) | ✅ `63555dc` |
| B | Joiner's relay gate + diagnosis | ✅ `88dd854` |
| C | The inert publish-failure seam | ✅ (this session) |
| D | Join-task lifecycle | ✅ `9809f6f` |
| E | `GroupSub::recv` failure handling | open |
| F | Honesty gaps | ✅ (elapsed-wait deferred) |
| G | NIP-42 inert on ritual subscriptions | open |
| H | Unpinned security checks | roster finding ✅ `1defd69`; coverage open |
| I | Invite relay cap | ✅ `28456f7` |

## Cluster C — The inert publish-failure seam

**Verdict.** The core defect holds at HEAD (88dd854). `spawn_publish_frame_with`'s `fail` sink is still dead: the one and only caller is the `None` wrapper `spawn_publish_frame` (nostr_ritual.rs:187), used at exactly one site — the Seal leg (founding.rs:2374) — so a Seal that no relay accepts produces one `tracing::error!` and nothing else: `create.run.outcome` stays 0, the founder sits on "charter proposed" forever and every member waits for a frame that never existed. `PublishReport` is still thrown away at both choke points (`RitualNet::publish` `.map(|_report| ())` ritual_net.rs:179; `GroupChannel::publish_frame` returns only the stamp, ritual_net.rs:347-350), so 1-of-N landing is indistinguishable from full delivery. The Genesis leg is still a single fire-and-forget publish with a `tracing::error!` (lifecycles.rs:945-952) whose own comment repeats the false §8b claim ("the member's own open wait surfaces a relays-down condition") — the member's Genesis wait (nostr_ritual.rs:588-625) is an unbounded `loop` with no deadline and no elapsed reporting. TWO SUB-FINDINGS RETIRE: the Welcome fan-out is NOT inert — it already routes a publish failure into `NetRitualFailed` by hand (founding.rs:1850-1875) — and the member's own `Signed` publish already fails loudly (nostr_ritual.rs:582-584); both only lack the partial-landing report. One structural fact the fix must respect: `maybe_finalize` does `self.net_ritual.take()` (lifecycles.rs:773) BEFORE the genesis publish, so a genesis report gated on the ritual generation would be silently dropped by `ritual_generation_current` (founding.rs:560-567), and `cmd_net_ritual_failed` early-returns once `outcome != 0` (founding.rs:1633-1637) — the genesis leg therefore cannot reuse that sink.

### Anchors (verified at 88dd854 — RE-VERIFY, the tree has moved)

| file | line | symbol | why |
|---|---|---|---|
| `crates/molt-engine/src/nostr_ritual.rs` | 187 | `spawn_publish_frame` | the only caller of spawn_publish_frame_with, hardcoding `None` — this is what makes the fail sink dead code |
| `crates/molt-engine/src/nostr_ritual.rs` | 195 | `spawn_publish_frame_with (fail param)` | `fail: Option<(mpsc::WeakSender<Envelope>, u64)>` — never once passed Some; the NetRitualFailed send at line 200-209 is unreachable |
| `crates/molt-engine/src/nostr_ritual.rs` | 178 | `spawn_publish_frame doc comment` | carries the FALSE claim 'the member's own wait surfaces a relays-down condition' — must be corrected with the fix |
| `crates/molt-engine/src/founding.rs` | 2374 | `maybe_seal` | the single Seal publish call: `spawn_publish_frame(chan, group, msg, "seal")` — no failure sink, no report; the log line at 2358-2362 already claimed success |
| `crates/molt-engine/src/lifecycles.rs` | 948 | `finalize_founding (genesis leg)` | `chan.publish_frame(&exporter, &ct)` fire-and-forget; only tracing::error! on failure — no retry, no surface, no ack |
| `crates/molt-engine/src/lifecycles.rs` | 773 | `maybe_finalize` | `self.net_ritual.take()` — the ritual is GONE before the genesis publish runs, so `ritual_generation_current(Some(g))` is false for any genesis report |
| `crates/molt-engine/src/founding.rs` | 1633 | `cmd_net_ritual_failed` | early-returns when `outcome != 0` — the post-materialization genesis failure cannot use this sink; it needs its own surface |
| `crates/molt-engine/src/founding.rs` | 1859 | `nostr_group_birth (welcome fan-out)` | already reports a failed Welcome via NetRitualFailed (hand-rolled send_cmd) — this half of the finding is already wired; only the PublishReport is dropped |
| `crates/molt-net/src/ritual_net.rs` | 179 | `RitualNet::publish` | `.map(\|_report\| ())` — the per-relay outcomes die here for send_ritual (184) and send_welcome (195) |
| `crates/molt-net/src/ritual_net.rs` | 347 | `GroupChannel::publish_frame` | publishes and returns only `stamp`; the PublishReport from RelayRuntime::publish is discarded |
| `crates/molt-net/src/relay_runtime.rs` | 197 | `RelayRuntime::publish` | the report that already exists: accepted[] / failed[(url,reason)], Err only when accepted is empty (238-243) |
| `crates/molt-engine/src/nostr_ritual.rs` | 588 | `member_join (genesis wait)` | `let sealed_final = loop { sub.recv(RECV_SLICE) ... }` — unbounded, no deadline, no elapsed surfacing: the member really does hang forever |
| `crates/molt-net/src/mls.rs` | 520 | `MlsMember::decrypt_at` | created_at is only consulted for the concurrent-COMMIT tiebreak — republishing the same genesis ciphertext under a fresh stamp is safe for an application frame |
| `crates/molt-mcp/src/lib.rs` | 1319 | `INTERNAL const` | `const INTERNAL: [&str; 45]` — the array length must become 46 when the report command is added, or co-equality goes red |
| `crates/molt-engine/tests/nostr_founding.rs` | 88 | `a_republic_founds_and_a_member_joins_over_one_relay` | the existing two-real-engines-over-MockRelay harness (engine(), adopt_relay(), wait_for()) every new red test extends |

### Red tests first

- **`a_seal_that_no_relay_accepts_fails_the_founding_instead_of_hanging`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::CreateStart → JoinStart → CreatePropose on two real engines (no injection); the failure must arrive as the production NetRitualFailed and show in SessionView**.
  - asserts: after CreatePropose, wait_for(a, |s| s.create.run.outcome == 2) and the create.run.log contains a line naming the seal publish failure (e.g. contains("seal") && contains("did not publish")); no workspace materializes
  - red today because: the relay is a `LocalRelay::new(RelayBuilder::default().write_policy(RejectKind445))` (nostr-relay-builder 0.44.1 builder.rs:339 write_policy / PolicyResult::Reject → OK:false "blocked: …", which counts_as_published() rejects). The Seal publish therefore returns Err; at HEAD spawn_publish_frame passes fail=None (nostr_ritual.rs:187), so only tracing::error! fires, outcome stays 0 and wait_for panics at its 30 s deadline
- **`a_seal_that_lands_on_only_one_of_two_relays_says_so_in_the_founding_log`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **the full happy path over Commands, founder pool = [live MockRelay, one undialable relay] (reuse the .onion-with-no-Tor shape already proven in a_join_needs_only_one_relay_in_common_with_the_invite), joiner pool = [live]**.
  - asserts: the founding still seals (outcome == 1) AND create.run.log carries a ⚠ line naming the undialable relay and the count it landed on ("1 of 2")
  - red today because: the per-relay PublishReport is discarded at ritual_net.rs:179 and 347 and never leaves the spawned task, so no such line can exist; the assertion fails immediately
- **`a_genesis_the_relay_first_rejects_is_retried_until_the_member_seals`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **the full happy path over Commands; the relay's WritePolicy counts kind-445 events and rejects #3 and #4 (deterministic happy-path 445 order: #1 founder Seal, #2 member Signed, #3+ founder Genesis attempts), accepting from #5**.
  - asserts: the founder seals (outcome == 1) AND the joiner seals for real: wait_for(b, |s| s.screen == Main && !s.workspaces.is_empty()), genesis on disk
  - red today because: at HEAD the genesis is published exactly once (lifecycles.rs:948); the single attempt is rejected, nothing retries, the member's unbounded wait (nostr_ritual.rs:588) never ends and wait_for on b panics
- **`a_genesis_no_relay_ever_accepts_is_surfaced_to_the_founder`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **same harness; the WritePolicy accepts the first two 445s (Seal, Signed) and rejects every later 445 forever**.
  - asserts: the founder still materializes (outcome == 1, workspace present — it did found) AND within the retry budget the session surfaces it: s.notice starts with "genesis-undelivered" and create.run.log carries a ✗ line saying the members were not told
  - red today because: the failure produces one tracing::error! and nothing else at HEAD — notice stays empty and no log line exists; also proves the report is NOT swallowed by ritual_generation_current after maybe_finalize's take() (lifecycles.rs:773)
- **`publish_frame_reports_the_relay_that_refused`** — in `crates/molt-net/tests/nostr_ritual_net.rs`, driven through **GroupChannel::publish_frame — the real public API the engine's publish task calls**.
  - asserts: with pool = [live MockRelay, dead port], the call succeeds and the returned report has accepted == [live] and one failed entry naming the dead relay with its reason
  - red today because: publish_frame returns only `u64` today (ritual_net.rs:328-351) — the test does not compile against HEAD, which is the signature change this cluster needs

### Fix steps

1. STEP 0 (design, no code): record the genesis decision in docs/transport/nostr_n4a_review_followups.md §C — recommended: bounded retry + surfaced notice, NOT an ack round (rationale: relays STORE the 445, so one accepted relay is durable for any member subscribing inside the h window; an ack round would force the founder's ritual/group/445-recv to stay alive past maybe_finalize's take()). See open_questions — get the user's yes before step 6.
2. STEP 1 (molt-net, mechanical): stop discarding the report. `RitualNet::publish` returns `PublishReport` (drop the `.map(|_report| ())` at ritual_net.rs:179); `send_ritual` (184) and `send_welcome` (195) return `Result<PublishReport, NetError>`; `GroupChannel::publish_frame` (328) returns `Result<(u64, PublishReport), NetError>`. Fix the two molt-net test call sites (nostr_ritual_net.rs:170, 196) and add the red test `publish_frame_reports_the_relay_that_refused`.
3. STEP 2 (molt-core): add ONE engine-internal command `Command::NetRitualPublished { what: String, accepted: Vec<String>, failed: Vec<String>, #[serde(default)] generation: Option<u64> }` with a doc comment saying it is the publish task reporting its REAL per-relay outcome and is never a tool. Keep `failed` pre-formatted ("url: reason") so the actor owns the wording.
4. STEP 3 (molt-mcp): INTERNAL 45 → 46, add "net_ritual_published" plus a comment paragraph in the block above the array (an MCP agent must not be able to forge a relay outcome). This is what keeps co_equality_every_command_is_a_tool_or_documented_internal green.
5. STEP 4 (molt-engine/nostr_ritual.rs): replace `spawn_publish_frame` + `spawn_publish_frame_with` with ONE non-optional-sink helper: `spawn_publish_frame(chan, payload, what: &'static str, retry: RetryPolicy, tx: mpsc::WeakSender<Envelope>, generation: Option<u64>)` where `payload` is a small enum { Encrypt(Arc<Mutex<MlsMember>>, RitualMsg), Sealed { ct: Vec<u8>, exporter: [u8;32] } }. ENCRYPT ONCE, then retry only the publish (never re-encrypt — a re-encrypt burns ratchet generations past the snapshot). The task ALWAYS sends `NetRitualPublished` (success, partial and total) — deleting the Option removes the seam a future call site can forget. Delete the false doc comment at nostr_ritual.rs:176-180.
6. STEP 5 (molt-engine/founding.rs): `maybe_seal` (2372-2379) calls the new helper with `self.cmd_tx.upgrade()?.downgrade()` + `Some(ritual.generation)` and the pre-seal retry policy (3 attempts, ~2 s backoff — bounded so the test's 30 s wait covers it). Add a `seal_published: bool` once-guard on the ritual so the second `maybe_seal` path (founding.rs:2187, a redelivered JoinRequest) cannot publish a second Seal, double-report, or advance the ratchet after the snapshot.
7. STEP 6 (molt-engine/lifecycles.rs 938-956): route the genesis through the same helper with `FramePayload::Sealed`, the genesis retry policy (e.g. 4 attempts, 2 s → 8 s backoff), and `generation: None` — the ritual is already `take()`n at 773, so a generation-gated report would be dropped. Delete the false comment at 940-944.
8. STEP 7 (molt-engine/founding.rs): new handler `cmd_net_ritual_published(what, accepted, failed, generation)` + the dispatch arm in molt-engine/src/lib.rs (next to NetRitualFailed at 1266). Branches: (a) accepted empty && pre-seal leg → delegate to `cmd_net_ritual_failed(format!("{what} did not publish: …"), generation)` (existing outcome=2 + teardown); (b) accepted empty && what == "genesis" → `session.notice = format!("genesis-undelivered:{detail}")`, a ✗ create.run.log line saying the members were NOT told, `tracing::error!`; (c) accepted non-empty && !failed.is_empty() → a ⚠ create.run.log line "landed on k of n relays" naming each failed relay; (d) clean success → `tracing::debug!` only. Gate (a)/(c) on `ritual_generation_current`; (b) must not be so gated.
9. STEP 8 (molt-ui, no .slint edit): toast the new notice in the edge-triggered notice block (crates/molt-ui/src/lib.rs ~2360-2370, next to the backup-failed/detached toasts) using a Rust-side localized copy fn (the `tor_verdict_copy_for` precedent) so no Strings entry and no 6-GiB molt-ui-window rebuild is needed. Without this the notice is itself an inert seam — the exact sin this cluster is about.
10. STEP 9 (docs): docs/transport/nostr_n4_plan.md §8b — strike the false "the member's own wait surfaces it" claim and record the genesis story that was chosen; mark cluster C DONE in docs/transport/nostr_n4a_review_followups.md, noting that the Welcome fan-out and the member's Signed publish were already wired (finding partially retired).
11. STEP 10: `cargo clippy --all-targets` at 0 and the four new tests green; then the code review over the diff and land on master.

### Files edited

- `crates/molt-net/src/ritual_net.rs`
- `crates/molt-net/tests/nostr_ritual_net.rs`
- `crates/molt-core/src/lib.rs`
- `crates/molt-mcp/src/lib.rs`
- `crates/molt-engine/src/nostr_ritual.rs`
- `crates/molt-engine/src/founding.rs`
- `crates/molt-engine/src/lifecycles.rs`
- `crates/molt-engine/src/lib.rs`
- `crates/molt-engine/tests/nostr_founding.rs`
- `crates/molt-ui/src/lib.rs`
- `docs/transport/nostr_n4_plan.md`
- `docs/transport/nostr_n4a_review_followups.md`

### Risks

- Co-equality: adding `Command::NetRitualPublished` makes `Command::variant_names()` grow; `const INTERNAL: [&str; 45]` (molt-mcp/src/lib.rs:1319) must become 46 with the name AND a justification comment, or the co-equality test goes red.
- NO byte layouts are touched — roster_canonical_bytes / republic_id / checkpoint tags and their byte-pin tests are untouched by this cluster. Keep it that way: nothing here may reach into the signed tables.
- The genesis retry MUST republish the same ciphertext, never re-encrypt: `encrypt` advances the sender ratchet, and finalize deliberately snapshots AFTER the genesis encrypt (lifecycles.rs:850-887) — a re-encrypt regresses that invariant and yields SecretReuseError on every member after reopen.
- `maybe_seal` is reachable twice (founding.rs:2187 from a redelivered JoinRequest, 2255 from CreatePropose). Without the once-guard the new sink can fail a founding twice or (worse) re-encrypt the Seal after the snapshot.
- The genesis report must NOT be generation-gated: `maybe_finalize` already did `net_ritual.take()` (lifecycles.rs:773), so `ritual_generation_current(Some(g))` is false and the report would vanish — recreating the exact inertness being fixed.
- `cmd_net_ritual_failed` early-returns on `outcome != 0` (founding.rs:1633): routing the genesis failure there would be silently swallowed once the founding sealed.
- Weak-handle rule: the retry task must hold `mpsc::WeakSender` only (nostr_ritual.rs:62 send_cmd pattern) — a retry loop with a strong sender would keep a dropped engine's actor and its workspace flock alive for the whole backoff.
- clippy at 0 including tests: `.expect("…")` not `.unwrap()` in the new tests; the new Command variant and its fields need doc comments (molt-core denies missing_docs) and `#[serde(default)]` on the optional field.
- Test-order fragility: the WritePolicy tests assume the happy-path kind-445 order (Seal, Signed, Genesis…). Document that assumption in the test and filter the policy strictly on `Kind::Custom(445)` so the 1059 traffic never shifts the counter. Retry budgets must stay well under the 30 s wait_for deadline.
- Existing green tests that will now emit new log lines: `a_join_needs_only_one_relay_in_common_with_the_invite` founds over a pool containing an undialable .onion, so it will start producing the ⚠ partial-landing lines — check nothing asserts on log length/shape there.
- molt-ui edit must stay Rust-side; touching ui/*.slint pulls the ~4 min / ~6 GiB molt-ui-window rebuild and can OOM next to another build.

### Open questions

- The Genesis delivery story — the one genuine fork. (A) RECOMMENDED: bounded retry (≈4 attempts with backoff) publishing the SAME pre-encrypted frame, plus an honest surfaced notice when no relay ever accepts. Cheap, no wire change, no lifetime change; justified because relays STORE the 445, so one accepted relay reaches a member that subscribes inside the h window. It makes 'we published it' true, not 'everyone got it'. (B) A real `GenesisAck` round: a new RitualMsg variant, the member publishes an ack, and the founder keeps the ritual's group + 445 recv alive past `maybe_finalize`'s `net_ritual.take()` until every seat acks or a deadline expires — the only option that turns delivery into a fact, at the cost of a post-materialization ritual lifetime and a member-side change. Pick before step 6; retro-fitting (B) later means re-opening the finalize/teardown boundary.

## Cluster E — GroupSub::recv failure handling

**Verdict.** The defect still exists, with one correction to the backlog's wording. `GroupSub::recv` (crates/molt-net/src/ritual_net.rs:407) returns `Option<(String,u64)>`; on a failed window-roll resubscribe it logs at `debug` and `return None` (line 421-422), and all three production callers (crates/molt-engine/src/nostr_ritual.rs:345, 532, 589) read `None` as "idle slice" and `continue`. Two real harms follow: (1) the node is deaf — the live `Subscription` keeps the PREVIOUS window's `#h` filter, so frames published under the new tag are never delivered — and nothing anywhere says so (debug-level trace only, no engine command, no log line, no health change); (2) the resubscribe is retried on EVERY loop iteration with no backoff and the call returns instantly, so the caller's 30 s `RECV_SLICE` budget collapses into a tight loop that redials every relay thousands of times per second (a self-inflicted connect storm through the Tor dialer). Correction: "permanently deaf" is imprecise — because the retry happens on every call, the node heals by itself once a relay accepts a REQ again; the accurate claim is "silently deaf, and busy-spinning, for as long as the resubscribe keeps failing". The `None`-means-idle lie and the missing backoff are both real and unpinned by any test (the roll path has zero coverage — the same gap H lists as its 4th item).

### Anchors (verified at 88dd854 — RE-VERIFY, the tree has moved)

| file | line | symbol | why |
|---|---|---|---|
| `crates/molt-net/src/ritual_net.rs` | 407 | `GroupSub::recv` | the return type `Option<(String,u64)>` conflates 'nothing arrived' with 'I can no longer hear the group' — the lie every caller believes |
| `crates/molt-net/src/ritual_net.rs` | 413 | `GroupSub::recv → channel.subscribe_tags` | the roll resubscribe: attempted on EVERY loop iteration, ungated by any backoff or retry schedule |
| `crates/molt-net/src/ritual_net.rs` | 421 | `GroupSub::recv (Err arm)` | tracing::debug! + `return None` — the only report of deafness, at a level no operator sees, and it aborts the caller's timeout budget |
| `crates/molt-net/src/ritual_net.rs` | 411 | `GroupSub::recv → window_tags(Timestamp::now())` | the wall-clock read that decides the roll; with no seam a UTC-midnight roll is unreachable in a test |
| `crates/molt-net/src/ritual_net.rs` | 338 | `GroupChannel::publish_frame (h_tag + custom_created_at)` | the publish side reads the same wall clock — the seam must cover it or a shifted test publishes under a tag its own subscriber disowns |
| `crates/molt-net/src/ritual_net.rs` | 363 | `GroupChannel::subscribe_tags` | builds a FRESH RelayRuntime per attempt (dials every relay) — this is what the busy-spin hammers |
| `crates/molt-net/src/ritual_net.rs` | 377 | `struct GroupSub` | where the retry schedule / deaf state must live so it survives across recv calls |
| `crates/molt-net/src/ritual_net.rs` | 56 | `ROLL_POLL` | the 1 s inner slice that already paces the loop — the fix reuses it instead of adding a sleep |
| `crates/molt-net/src/relay_runtime.rs` | 367 | `RelayRuntime::subscribe` | returns Err(Unreachable) fast when no relay accepts the REQ — the fast failure that turns the retry into a spin |
| `crates/molt-engine/src/nostr_ritual.rs` | 345 | `spawn_founder_group_recv` | caller 1: `let Some(..) = sub.recv(RECV_SLICE).await else { continue }` — deafness read as an idle tick |
| `crates/molt-engine/src/nostr_ritual.rs` | 532 | `member_join (Seal wait)` | caller 2: same else-continue; a joiner waiting for the charter goes deaf and spins |
| `crates/molt-engine/src/nostr_ritual.rs` | 589 | `member_join (Genesis wait)` | caller 3: same else-continue on the sign-what-you-see genesis wait |
| `crates/molt-engine/src/nostr_ritual.rs` | 34 | `RECV_SLICE` | the 30 s budget the failure path silently discards |
| `crates/molt-engine/src/founding.rs` | 1628 | `State::cmd_net_ritual_failed` | the model (and generation gate `ritual_generation_current`) for the new non-fatal founder-side note handler |
| `crates/molt-engine/src/lifecycles.rs` | 1631 | `State::cmd_net_join_accepted` | the existing last-line dedup pattern (`if log.last() == Some(&line)`) the deaf note must reuse so it cannot stack |
| `crates/molt-engine/src/lib.rs` | 1266 | `State::execute dispatch (NetRitualFailed arm)` | where the two new internal command arms go |
| `crates/molt-mcp/src/lib.rs` | 1319 | `INTERNAL: [&str; 45]` | co-equality list — must grow to 47 with a documenting comment or the audit test fails |
| `crates/molt-net/tests/nostr_relay_runtime.rs` | 324 | `mod proxy::Cuttable` | the reusable cuttable TCP proxy — the only way to take a MockRelay down and bring it back on the same port |
| `crates/molt-engine/tests/nostr_founding.rs` | 86 | `a_republic_founds_and_a_member_joins_over_one_relay` | the production harness (CreateStart/JoinStart over a MockRelay, no seams) the engine-level red test clones |

### Red tests first

- **`a_failed_window_roll_is_reported_deaf_and_retried_on_a_backoff`** — in `crates/molt-net/tests/nostr_window_roll.rs (new file — own test binary, because the clock seam is process-global)`, driven through **GroupChannel::subscribe + GroupSub::recv — the exact public API the engine's three ritual loops call (nostr_ritual.rs:330/345, 523/532, 589)**.
  - asserts: Setup: MockRelay behind a Cuttable proxy (copy of tests/nostr_relay_runtime.rs mod proxy, extended with an AtomicUsize incremented on every accept); GroupChannel over the proxy URL; subscribe + live(). Then proxy.cut() and shift the window clock by +H_WINDOW. (a) the next recv(3s) returns GroupRecv::Deaf(reason) whose reason names the failed resubscribe — never Idle; (b) drive recv(1s) in a loop for ~6 s and assert the proxy saw <= 5 connection attempts (retry is backoff-gated, not per-iteration); (c) heal: proxy.restore(), a second GroupChannel sharing the rotation seed publishes a frame under the NEW window, and recv within 20 s yields GroupRecv::Frame with exactly that content.
  - red today because: (a) does not compile today (no GroupRecv); after the mechanical enum step (Frame/Idle only, failure still mapping to Idle) it fails on the Deaf assertion because ritual_net.rs:422 returns None. (b) fails hard today: with the proxy cut, subscribe_tags fails in ~1 ms and recv returns instantly, so the 6 s window produces hundreds-to-thousands of accepts instead of <= 5.
- **`a_deaf_group_channel_is_surfaced_to_both_wizards_and_heals`** — in `crates/molt-engine/tests/nostr_window_roll.rs (new file — own test binary)`, driven through **Command::CreateStart / Command::JoinStart / Command::CreatePropose / Command::JoinConfirmCharter / Command::ReadSession — the real founding+join over a MockRelay, exactly the nostr_founding.rs harness, no injected commands**.
  - asserts: Found on engine A and join on engine B over a relay behind the Cuttable proxy; wait until A reports create.can_propose (the founder has welcomed the seat, so BOTH sides now hold a live GroupSub), settle ~500 ms, then proxy.cut() + shift the window clock by +H_WINDOW. Within 30 s: (a) B's session.join.run.log gains a line naming that the group channel cannot be re-subscribed, and (b) A's session.create.run.log gains the same class of line; (c) neither run fails — join.run.outcome == 0 and create.run.outcome == 0 (a relay blip must not kill a one-shot founding); (d) heal: proxy.restore(), A CreatePropose, B reaches awaiting_ratify, B ratifies, both seal (create.run.outcome == 1, B on Screen::Main) — the deafness was survivable and nothing was lost.
  - red today because: (a) and (b) fail: the failure never leaves molt-net (tracing::debug! at ritual_net.rs:421), no Command carries it, and both callers `continue` as if idle — the two run logs stay silent forever while both nodes spin. (This is also the test that keeps the fix from being inert: without it, the callers could satisfy the compiler with `GroupRecv::Deaf(_) => continue` and the bug would survive the refactor.)

### Fix steps

1. 0. Baseline: `cargo test -p molt-net --test nostr_ritual_net` and `-p molt-engine --test nostr_founding` green before touching anything (another agent may be in the tree — re-check `git log --oneline -3` first).
2. 1. molt-net clock seam (no behavior change). In `ritual_net.rs` add a process-global `static WINDOW_CLOCK_SHIFT: AtomicI64` plus `#[doc(hidden)] pub fn shift_window_clock_for_tests(secs: i64)` and a private `fn now_secs() -> u64 { Timestamp::now().as_secs().saturating_add_signed(shift) }`, documented as a TEST SEAM that is zero in every shipping run (it is deliberately not feature-gated: molt-net's own integration tests cannot enable a feature on their own crate). Route the three wall-clock reads through it: line 338 (publish_frame's h_tag AND custom_created_at — one `now`, as today), line 357 (subscribe), line 411 (recv's roll check).
3. 2. Write RED test 1 (`crates/molt-net/tests/nostr_window_roll.rs`) exactly as specified. It will not compile — that is the first beat.
4. 3. Mechanical API change: `pub enum GroupRecv { Frame { content: String, created_at: u64 }, Idle, Deaf(String) }` returned by `GroupSub::recv`; map today's two `return None` sites to `Idle` and leave the Err arm returning `Idle` on purpose. Update the three engine call sites (nostr_ritual.rs:345/532/589) to `match` with `Idle | Deaf(_) => continue` for now. Run test 1: it is now behaviorally RED on the Deaf assertion and on the accept-count assertion — the right reasons.
5. 4. Implement the real recv. Add to `GroupSub`: `deaf: Option<String>`, `retry_at: tokio::time::Instant`, `retry_backoff: Duration` (initial 1 s, ×2, cap 30 s — mirror `RelayRuntime`'s backoff shape and constants style). In the loop: attempt `subscribe_tags` ONLY when the tags differ AND `Instant::now() >= retry_at`; on success reset backoff, clear `deaf`, swap sub+tags; on failure set `deaf = Some(e.to_string())`, set `retry_at = now + backoff`, double the backoff (capped), and return `GroupRecv::Deaf(reason)` immediately (returns are backoff-gated, so this cannot spin). While deaf but no attempt is due, KEEP READING the stale subscription for the ROLL_POLL slices (inside the skew margin the previous window's tag is still legitimate traffic) and, when the budget elapses with `deaf.is_some()`, return `Deaf(last reason)` — never `Idle`. Test 1 goes green including the heal assertion.
6. 5. Write RED test 2 (`crates/molt-engine/tests/nostr_window_roll.rs`) as specified. It fails: the two run logs stay silent.
7. 6. Add the two engine-internal commands in `molt-core`: `NetRitualNote { note: String, #[serde(default)] generation: Option<u64> }` (founder/create scope) and `NetJoinNote { note: String, #[serde(default)] generation: Option<u64> }` (join scope), each documented as 'the node's own ritual task reporting a non-fatal transport condition — never an MCP tool'.
8. 7. Dispatch + handlers: arms in `molt-engine/src/lib.rs` next to the `NetRitualFailed` arm (line 1266); `cmd_net_ritual_note` in `founding.rs` beside `cmd_net_ritual_failed` (gate on `ritual_generation_current(generation)` and `run.outcome == 0`, push into `session.create.run.log`, `emit_session(SessionScope::Create)`); `cmd_net_join_note` in `lifecycles.rs` beside `cmd_net_join_failed` (gate on `generation == Some(self.join_generation)` and `run.outcome == 0`, push into `session.join.run.log`, `emit_session(SessionScope::Full)`). BOTH must reuse the `cmd_net_join_accepted` dedup (`if log.last() == Some(&line) { return Ok(Reply::Ack) }`) so a 30 s-repeating deaf note cannot stack lines. Neither handler ever sets `outcome = 2`.
9. 8. Grow `INTERNAL` in `crates/molt-mcp/src/lib.rs:1319` from `[&str; 45]` to `[&str; 47]` with `net_ritual_note` / `net_join_note` and a comment in the block above explaining why they are internal (a task reporting a transport condition, not an operator decision).
10. 9. Wire the three call sites in `nostr_ritual.rs`: on `GroupRecv::Deaf(why)` send the scope's note ('⚠ cannot hear the group channel — {why} · still retrying' — founder: NetRitualNote, joiner: NetJoinNote) and `continue`; track a local `was_deaf` flag and, on the first `Frame` after a deaf spell, send '✓ the group channel is back'. Keep the existing frame handling byte-identical otherwise. Test 2 goes green.
11. 10. `cargo clippy --all-targets -p molt-net -p molt-core -p molt-engine -p molt-mcp` at zero (`.expect("…")` in the new tests, never `.unwrap()`), run `-p molt-net --test nostr_ritual_net --test nostr_window_roll --test nostr_relay_runtime` and `-p molt-engine --test nostr_founding --test nostr_window_roll`, then mark cluster E done in `docs/transport/nostr_n4a_review_followups.md` and add one line to `docs/transport/nostr_n4_plan.md` §4.4 recording that the roll now retries on a backoff and reports deafness. One commit, on master.

### Files edited

- `crates/molt-net/src/ritual_net.rs`
- `crates/molt-engine/src/nostr_ritual.rs`
- `crates/molt-core/src/lib.rs`
- `crates/molt-engine/src/lib.rs`
- `crates/molt-engine/src/founding.rs`
- `crates/molt-engine/src/lifecycles.rs`
- `crates/molt-mcp/src/lib.rs`
- `crates/molt-net/tests/nostr_window_roll.rs`
- `crates/molt-engine/tests/nostr_window_roll.rs`
- `docs/transport/nostr_n4a_review_followups.md`
- `docs/transport/nostr_n4_plan.md`

### Risks

- No byte-layout ripple: nothing here touches roster_canonical_bytes / republic_id / checkpoint tags / WorkspaceEvent. The `#h` tag derivation and the 445 wire shape are unchanged — only WHEN a subscription is re-placed changes.
- Co-equality test: two new `Command` variants make `co_equality_every_command_is_a_tool_or_documented_internal` go red until INTERNAL grows 45→47 (crates/molt-mcp/src/lib.rs:1319). They must be INTERNAL, not tools — an MCP agent must not be able to write lines into a founding log.
- The test clock seam is PROCESS-GLOBAL, so it is contagious across tests sharing a binary. Both new tests must live in their own files (own binaries); never call `shift_window_clock_for_tests` from `src/`, and never from `nostr_founding.rs`.
- `GroupRecv` is a breaking change to a molt-net public API; the compiler finds all three engine call sites, but the compiler CANNOT stop `Deaf(_) => continue` — that is exactly the inert-fix trap this cluster is about, which is why red test 2 (engine-level, run-log assertion) is not optional.
- Cluster C also wants a non-fatal warning line in the founding log ('landed on fewer relays than configured'). If C lands first it may introduce its own note command — reuse it instead of adding a second one; expect a merge conflict in molt-core's Command enum, the engine dispatch and the INTERNAL array. Coordinate with whoever holds C.
- The proxy double is currently a private `mod proxy` inside crates/molt-net/tests/nostr_relay_runtime.rs (test binaries do not share modules). Copy it into the new file(s) or promote it to crates/molt-net/tests/common/mod.rs (which does not exist yet); the engine test needs its own copy either way.
- Timing sensitivity in test 2: the joiner only holds a GroupSub after the Welcome. Gate the cut on A's `create.can_propose` plus a short settle, or the cut lands before the subscription exists and the test measures nothing. Keep the standard 30 s wait_for deadline.
- clippy at zero including tests: the atomic shift needs `saturating_add_signed`/explicit casts (no `as` truncation lints), and the new tests must use `.expect("…")`.
- Behavior change for the stale-subscription read: while deaf, recv keeps serving the OLD window's subscription. That is correct inside the ±1 h skew margin and harmless outside it (the h-tag gate at ritual_net.rs:437 still filters), but it means `Deaf` and a delivered `Frame` can interleave — callers must treat Deaf as advisory, never as a terminal state.

### Open questions

- Does a persistently deaf ritual EVER become terminal? The plan above is 'loud forever, never fatal' (the delivery-guarantee precedent: go loud, never silently give up; the operator cancels). The counter-argument is cluster F's unbounded-wait complaint: a founder or joiner can now sit in a visibly-warning-but-never-ending wizard indefinitely. If the product wants a ceiling (e.g. NetRitualFailed/NetJoinFailed after N minutes of continuous deafness), say so now — it changes the caller code and adds a second assertion to red test 2, and getting it wrong is expensive in the other direction because CreatePropose is one-shot: a founding aborted on a transient relay blip loses every collected signature and must be re-minted.

## Cluster F — Honesty gaps

**Verdict.** All three gaps still exist at HEAD, but F3's stated mechanism is wrong. F1: `net_health` is written in exactly three places (session.rs:768/872/901, net.rs:2075/2083) — none of them is the shared `materialize_workspace` (lifecycles.rs:70), so a Nostr founding/join ends with the default `NetHealth::Ok` and a green pill for the whole first session; only a REOPEN sets the honest Down. F2: `cmd_create_cancel` (lifecycles.rs:749) just calls `teardown_ritual()` — no frame is published, and there is no `Aborted` variant in `RitualMsg` (invite.rs:314-397) to publish; meanwhile the member task's Welcome (nostr_ritual.rs:492), Seal (528) and Genesis (586) waits are `loop { recv(RECV_SLICE) }` with no deadline and no progress surface, so a dead ritual is indistinguishable from a slow one, forever. F3: the backlog says "a retry re-derives the same identity" — FALSE. `cmd_join_start` mints a fresh seed phrase on every start (lifecycles.rs:1033-1034) and both anchors derive from it (`member_identity`, `nostr_identity(entropy, ticket)`), so a retry presents a genuinely DIFFERENT identity; `same` at founding.rs:1936 is correctly false and the `LinkSpent` arm fires. The comparison is not the bug — the bug is that the founder has no re-activation path at all, so a transport hiccup burns the seat to a dead identity and wedges the founding. The defect is real and worse than reported; the prescribed fix ("re-check the comparison") would fix nothing.

### Anchors (verified at 88dd854 — RE-VERIFY, the tree has moved)

| file | line | symbol | why |
|---|---|---|---|
| `crates/molt-engine/src/lifecycles.rs` | 70 | `State::materialize_workspace` | the ONE materialize every finish shares (founder 904, joiner 1259, recovery 1472); it already receives `shape: TransportShape` and never touches `session.net_health` — this is where F1's honest Down belongs |
| `crates/molt-engine/src/lifecycles.rs` | 904 | `finalize_founding -> self.materialize_workspace(` | the founder's Nostr path; passes ritual.transport_shape() (kind=Nostr) and leaves health Ok |
| `crates/molt-engine/src/lifecycles.rs` | 1259 | `cmd_net_join_sealed -> match self.materialize_workspace(` | the joiner's Nostr path; mesh is empty on Nostr so no net is built and health stays Ok |
| `crates/molt-engine/src/session.rs` | 872 | `cmd_open_workspace (nostr_kind branch)` | the ONLY place the honest 'runtime lands with N5' Down is set today — extract this literal into a shared const |
| `crates/molt-engine/src/net.rs` | 2070 | `recompute_net_health` | early-returns on Down, so a Down set at founding survives the 30s presence ticker; resolve_dialer (session.rs:765) resets it to Ok on the next open, so it is not sticky |
| `crates/molt-engine/src/lifecycles.rs` | 749 | `cmd_create_cancel` | tears the ritual down (753) and tells nobody — F2's publish site |
| `crates/molt-engine/src/founding.rs` | 1644 | `cmd_net_ritual_failed -> self.teardown_ritual()` | second silent-death site; the same helper must fire here |
| `crates/molt-engine/src/founding.rs` | 159 | `struct NostrRitual` | holds net/dialer/relays/group/chan — everything an abort publish needs; Drop aborts only INBOUND tasks, so fire-and-forget outbound abort tasks survive the teardown |
| `crates/molt-net/src/invite.rs` | 347 | `RitualMsg::LinkSpent` | the additive-variant precedent (an older joiner fails to parse and keeps its old wait) — `Aborted { reason }` goes next to it |
| `crates/molt-engine/src/nostr_ritual.rs` | 492 | `member_join — Welcome wait` | 'unbounded — the founder waits for every seat'; listens ONLY on the 1059 inbox, so the abort must also travel as a gift-wrap per anchored seat |
| `crates/molt-engine/src/nostr_ritual.rs` | 528 | `member_join — Seal wait loop` | unbounded; listens ONLY on the 445 group sub, so the post-birth abort must be a group frame — and must be gated on the MLS-authenticated `from` |
| `crates/molt-engine/src/nostr_ritual.rs` | 287 | `check_proposal_provenance` | `from != info.inviter => NotTheFounder` is the exact rule the abort arm must reuse; extract it so one helper serves both |
| `crates/molt-engine/src/founding.rs` | 1926 | `cmd_net_join_requested — the `spent` snapshot` | reads (member, identity_pk, ticket) off the anchored seat; needs the nostr anchor and the `sealed` flag too |
| `crates/molt-engine/src/founding.rs` | 1936 | `let same = anchored_member == member && anchored_pk == identity_pk` | the two-anchor idempotency test; correct as at-least-once dedup, but there is no re-activation branch after it — every retry falls into LinkSpent |
| `crates/molt-engine/src/lifecycles.rs` | 1034 | `cmd_join_start -> molt_storage::generate_seed_phrase()` | THE evidence that the backlog's F3 premise is wrong: every JoinStart mints a new phrase, so a retry cannot re-derive the same identity |
| `crates/molt-engine/src/founding.rs` | 1798 | `nostr_group_birth` | `nostr.group.is_some()` is the born flag that must gate re-anchoring; the Welcome bytes are moved into the fan-out tasks and NOT retained, which is why a post-birth retry cannot be resumed without a re-key |
| `crates/molt-core/src/lib.rs` | 2457 | `pub struct JoinState` | where `waiting_since: u64` goes (#[serde(default)]); constructed literally at lifecycles.rs:1038 |
| `crates/molt-engine/src/lib.rs` | 593 | `State::clock_override` | pub(crate) test seam — the elapsed-wait test must therefore be an in-crate mod test, not an integration test |
| `crates/molt-engine/tests/nostr_founding.rs` | 286 | `reopen honesty assert` | the ONLY existing pin of the N5 health string — it covers reopen, not the first session |
| `crates/molt-engine/tests/nostr_founding.rs` | 535 | `a_second_activation_of_the_same_link_fails_as_spent` | carol (a DIFFERENT handle) must stay refused — this test must remain green and is the guard on the F3 fix's blast radius |

### Red tests first

- **`the_first_session_of_a_nostr_republic_is_honest_about_the_missing_runtime (extend the existing keystone a_republic_founds_and_a_member_joins_over_one_relay)`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::CreateStart / JoinStart / CreatePropose / JoinConfirmCharter, then Command::ReadSession on BOTH engines — inserted right after both sides seal (~line 158, before the CloseWorkspace block)**.
  - asserts: both sessions report NetHealth::Down whose reason contains "N5", and notice != "detached" — the same verdict the reopen already asserts at line 286
  - red today because: materialize_workspace never writes net_health, so both sessions are NetHealth::Ok (the serde default) for the whole first session; the assert fires on Ok
- **`a_founder_cancel_reaches_the_members_inside_the_born_group`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::CreateStart → JoinStart → (wait for a.create.can_propose = all-joined = group born) → Command::CreateCancel on the founder engine**.
  - asserts: within the 30 s wait_for, petra's join.run.outcome == 2 and her log names the founder's abort ("the founder ended this founding"); nothing materialized on either side
  - red today because: cmd_create_cancel (lifecycles.rs:749) only calls teardown_ritual; no frame exists (RitualMsg has no Aborted variant) and the member sits in the unbounded Seal wait (nostr_ritual.rs:528) — wait_for times out
- **`an_abort_frame_from_a_co_member_is_ignored`** — in `crates/molt-engine/src/nostr_ritual.rs (mod tests)`, driven through **the extracted `frame_is_from_founder(from, &info)` helper that BOTH 445 arms (Seal at 531 and the new Aborted arm) call on the production path**.
  - asserts: true only for from == info.inviter; false for any other MLS credential — i.e. a welcomed seat cannot kill another seat's join with a forged abort
  - red today because: the helper does not exist and neither does the arm; without it the abort arm would honor any group member's frame — the exact impersonation class fixed as CRITICAL in 63555dc
- **`a_waiting_join_reports_how_long_it_has_been_waiting`** — in `crates/molt-engine/src/lifecycles.rs (mod tests, #[tokio::test])`, driven through **Command::RelayAdd/RelayConfirm/RelayClearnetSession then Command::JoinStart, then Command::NetPresenceTick — all through State::handle on a State built with a LIVE cmd_rx (new in-crate helper) and clock_override pinned**.
  - asserts: after JoinStart, session.join.waiting_since == T; after advancing clock_override to T+300 and one NetPresenceTick, the join log's LAST line reports the elapsed wait ("5 min") and a second tick does not stack a second line
  - red today because: JoinState has no waiting_since field and cmd_net_presence_tick (net.rs:2099) never looks at the join run — the wait is invisible; today's log ends at "waiting for the deliberation" forever
- **`a_retry_of_the_same_link_by_the_same_joiner_keeps_the_seat_and_the_founding_completes`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::CreateStart{members:3, threshold:2} → Command::JoinStart(link0,"petra") → (wait a.create.seats[0].member == "petra") → Command::JoinStart(link0,"petra") AGAIN on the same engine → third engine joins link1 → CreatePropose → both JoinConfirmCharter**.
  - asserts: petra's second run never fails as spent (outcome stays 0 through the retry), the founder's log carries no "activated a second time" line for it, seat 0 still names petra, and the founding seals with 3 seats
  - red today because: the retry derives a fresh phrase → fresh identity_pk (lifecycles.rs:1034), so `same` at founding.rs:1936 is false, the MAC still verifies, and the founder sends LinkSpent → petra's run fails with "already used by someone else" and the seat stays anchored to her dead first identity

### Fix steps

1. F1-a: extract the honest reason at session.rs:872 into one pub(crate) const (e.g. `NOSTR_RUNTIME_PENDING`) and use it there.
2. F1-b: in materialize_workspace (lifecycles.rs:70) capture `let nostr = shape.kind == Some(molt_core::TransportKind::Nostr);` BEFORE `shape` is consumed into the TransportState (line ~145); after `self.active = Some(...)` and before `Ok(id)`, set `self.session.net_health = NetHealth::Down { reason: NOSTR_RUNTIME_PENDING.to_string() }` when `nostr`. Both callers already emit_session Full afterwards (founding.rs:2479, lifecycles.rs:1332) — do not add an emit inside materialize.
3. F2-a: add `RitualMsg::Aborted { reason: String }` to crates/molt-net/src/invite.rs next to LinkSpent (347), with the same additive doc note (an older peer's `from_slice(..).ok()` yields None and skips it).
4. F2-b: add `State::abandon_ritual(&mut self, reason: &str)` in founding.rs: no-op unless `net_ritual.nostr` is Some; for every seat with an anchored identity spawn `net.send_ritual(&seat.nostr_pk, &RitualMsg::Aborted{..})` (covers members still in the unbounded Welcome wait), and if `nostr.group`+`nostr.chan` are set also `spawn_publish_frame(chan, group, Aborted{..}, "abort")` (covers the Seal/Genesis waits, where the member listens only on the group channel). Then call teardown_ritual(). Outbound tasks own their clones, so the NostrRitual Drop (which aborts only inbound tasks) does not kill them.
5. F2-c: call abandon_ritual instead of teardown_ritual at the five abandon sites, each with its own reason: cmd_create_cancel (lifecycles.rs:753), cmd_net_ritual_failed (founding.rs:1644), cmd_create_start (lifecycles.rs:695), cmd_join_start (lifecycles.rs:1030), cmd_select_screen (session.rs:44). It is a no-op on success (maybe_finalize takes net_ritual first) and on loopback.
6. F2-d: extract `fn frame_is_from_founder(from: &str, info: &InviteInfo) -> bool` from check_proposal_provenance (nostr_ritual.rs:287) and call it from BOTH the Seal arm and the new Aborted arms.
7. F2-e: member task (nostr_ritual.rs) — handle Aborted in the accept wait (456) and the Welcome wait (492) gated on `sender == h.npub`, and in the Seal (531) / Genesis (589) loops gated on `frame_is_from_founder(&from, &ctx.invite.info)`; return `Err("the founder ended this founding: {reason}")` so the existing NetJoinFailed path reports it.
8. F2-f: add `waiting_since: u64` (#[serde(default)]) to JoinState (molt-core:2457); set it from `self.presence_now()` in cmd_join_start (the JoinState literal at lifecycles.rs:1038); clear it in cmd_net_join_failed (1583), cmd_net_join_sealed (the JoinState::default() at 1288) and cmd_join_cancel (1599).
9. F2-g: add `State::refresh_join_wait(&mut self)` and call it from cmd_net_presence_tick (net.rs:2099): while `session.join.run.running() && waiting_since != 0`, REWRITE (never stack — mirror the idempotent last-line guard in cmd_net_join_accepted at lifecycles.rs:1633) a trailing `· still waiting … — N min` line and emit_session only on change. No .slint or molt-ui edit is needed: the wizard already renders join.run.log (molt-ui:2630).
10. F3-a: rework the spent arm (founding.rs:1926-1975). Snapshot (anchored_member, anchored_pk, anchored_nostr_pk, seat.sealed, ticket). Treat as the SAME seat claim only when all three anchors match (canonicalize the incoming nostr_pk first) → silent Ack, unchanged.
11. F3-b: else, if the MAC verifies AND `anchored_member == member` AND the seat is not sealed AND the group is not born (`nostr.group.is_none()`): RE-ANCHOR — clear seat.identity/key_package/reply_snd/reply_wrap and let control fall through into the existing verification ladder (PoP → MAC → canonical anchor → cross-seat uniqueness → KeyPackage binding) so the retry is re-checked in full, not fast-pathed; log `· invite N re-activated by {member} — the earlier attempt is replaced`; send LinkSpent to the DISPLACED anchor (not to the new one).
12. F3-c: else (different handle, or sealed, or group already born): keep LinkSpent to the new activator, but split the wording — a different handle keeps today's 'that link is spent, ask for your own'; a same-handle post-birth retry gets the true reason ('this founding has already formed its group around the first activation — the founder must cancel and re-mint'). Mirror the split in the joiner's error text (nostr_ritual.rs:474 and 500).
13. F3-d: log the currently-silent case too — a re-activation whose MAC does NOT verify appends a refusal line instead of returning Ack in silence.
14. Docs: add the `Aborted` row to the wire-mapping table in docs/transport/nostr_n4_plan.md §2 (line ~124); record the abort frame + the re-activation rule in docs/ritual/founding_ritual.md; in docs/transport/nostr_n4a_review_followups.md mark F done AND correct the F3 diagnosis in place (the retry does not re-derive the same identity — cmd_join_start mints a fresh phrase; the comparison was never the bug).

### Files edited

- `crates/molt-core/src/lib.rs`
- `crates/molt-net/src/invite.rs`
- `crates/molt-engine/src/lifecycles.rs`
- `crates/molt-engine/src/session.rs`
- `crates/molt-engine/src/founding.rs`
- `crates/molt-engine/src/nostr_ritual.rs`
- `crates/molt-engine/src/net.rs`
- `crates/molt-engine/src/lib.rs`
- `crates/molt-engine/tests/nostr_founding.rs`
- `docs/transport/nostr_n4_plan.md`
- `docs/transport/nostr_n4a_review_followups.md`
- `docs/ritual/founding_ritual.md`

### Risks

- No signed byte layout is touched: roster_canonical_bytes / republic_id / checkpoint / chain tags are untouched by all three fixes (RitualMsg is an unsigned wire type, JoinState is session surface). If a step starts editing those, the plan has drifted.
- No new Command variant is introduced, so co_equality_every_command_is_a_tool_or_documented_internal stays green. If the elapsed surfacing is instead built as a task→actor progress Command, molt-mcp's INTERNAL list MUST be updated in the same commit.
- RitualMsg::Aborted is a new wire variant: an older peer skips it (`from_slice(..).ok()`), which is the LinkSpent posture. The abort must never be authoritative for anything persisted — it only ends a run.
- SECURITY: the 445 abort arm must be gated on the MLS-authenticated `from` (frame_is_from_founder). Ungated, any welcomed seat kills every other seat's join with one frame — the exact class fixed as CRITICAL in 63555dc.
- The unit pin on frame_is_from_founder is one refactor away from being inert. It only secures anything while BOTH 445 arms call it; if a reviewer inlines the check back, the pin dies silently. Consider a follow-up 3-node integration test (cluster H territory).
- F3 loosens single-use in the pre-birth window. Restricting the re-anchor to `anchored_member == member` keeps a second PERSON out (they would have to guess the first joiner's handle), and the founder sees a re-activation log line either way. tests/nostr_founding.rs:535 (carol, a different handle) must stay GREEN — it is the guard on this.
- The re-anchor path must fall THROUGH the full ingest ladder, not bypass it. A shortcut that overwrites seat.identity without re-running PoP/MAC/canonical/uniqueness/KeyPackage-binding would reintroduce an unauthenticated anchor write.
- Setting Down inside materialize_workspace must be gated strictly on shape.kind == Some(Nostr); the loopback suites (two_instances.rs, three_nodes.rs, demo_mesh.rs) pass TransportShape::default() and would otherwise all go Down.
- Down is never lifted by recompute_net_health (net.rs:2071) — verify the founding-time Down is cleared on the next resolve_dialer/open (session.rs:765) so it cannot leak onto a later loopback workspace in the same session.
- abandon_ritual uses tokio::spawn; it is called from the actor task in production, but any in-crate mod test driving CreateCancel must run under a runtime (#[tokio::test] / rt().block_on).
- clippy at 0 including tests: use .expect("…") in the new tests and the new in-crate State helper; the helper must KEEP the cmd_rx alive (a dropped receiver makes cmd_tx.upgrade() return None and cmd_join_start fails as "engine stopped").
- The elapsed log line must be rewritten in place, never appended per tick — a stacking log is a UI regression and would also break the last-line idempotence guard in cmd_net_join_accepted.

### Open questions

- Post-birth retry: is 'refuse honestly + re-mint the whole founding' acceptable for N4a? The Welcome is bound to the joiner's first KeyPackage (whose HPKE private half died with the aborted task), so a retry after group birth cannot be resumed without the founder re-committing the retry's fresh KeyPackage and issuing a new Welcome. For a 2-member republic the group is born the instant the single seat anchors, so ANY transport hiccup after the founder accepted forces a full re-mint. Recommended default: refuse with an actionable message now (cheap, no MLS re-key), and file the per-seat re-key as N5 work. If the user judges that a shipping blocker, the re-key must be scoped before F3 is implemented.

## Cluster G — NIP-42 inert on ritual subscriptions

**Verdict.** Both halves still hold at HEAD (88dd854). `with_auth_keys` (crates/molt-net/src/relay_runtime.rs:158) has ZERO production callers — only two molt-net tests — so every ritual subscription is built by `RelayRuntime::new`, whose `auth_keys` is `None` (relay_runtime.rs:150); the supervisor then bails at `let Some(keys) = &shared.auth_keys else { continue }` (relay_runtime.rs:587) and an auth-required relay keeps a live, silent connection (the `auth-required:` CLOSED arm deliberately keeps the session alive), so the ritual times out with no error anywhere. The three `let _ = …live(LIVE_WAIT).await;` sites (nostr_ritual.rs:95, 431, 526) discard the gate, and the founder's own 445 recv (nostr_ritual.rs:330) never calls `live()` at all. One correction to the backlog's wording: a HARD subscribe failure is NOT ignored — `subscribe()` returns `Err` when no relay accepted the REQ and both call sites already report it (nostr_ritual.rs:83-93, 332-342). The real gap is the relay that ACCEPTS the connection and REQ but never becomes readable (auth-required, rate-limited, CLOSED-then-refused) — `live()` false, proceed blind.

### Anchors (verified at 88dd854 — RE-VERIFY, the tree has moved)

| file | line | symbol | why |
|---|---|---|---|
| `crates/molt-net/src/relay_runtime.rs` | 158 | `RelayRuntime::with_auth_keys` | the NIP-42 seam itself — grep proves its only callers are crates/molt-net/tests/nostr_relay_runtime.rs:493 and :631; no src/ caller anywhere in the workspace |
| `crates/molt-net/src/relay_runtime.rs` | 150 | `RelayRuntime::new` | every runtime is born with auth_keys: None, so the ritual facade's four construction sites are all inert w.r.t. NIP-42 |
| `crates/molt-net/src/relay_runtime.rs` | 587 | `read_session (RelayMessage::Auth arm)` | `let Some(keys) = &shared.auth_keys else { continue };` — without keys the challenge is dropped and the session stays connected-but-silent forever (the CLOSED `auth-required:` arm at :624 deliberately does not kill it) |
| `crates/molt-net/src/relay_runtime.rs` | 314 | `Subscription::synced` | the ALL-connected-relays-EOSE gate; the only readable sync signal, and it collapses partial/zero into one bool — the ≥1 rule needs a richer return here |
| `crates/molt-net/src/ritual_net.rs` | 214 | `RitualNet::inbox` | the 1059 inbox subscription — `RelayRuntime::new(self.dialer.clone(), self.relays.clone()).subscribe(filter)`, no auth keys, although `self.keys` (the ticket-salted anchor) is right there in the struct |
| `crates/molt-net/src/ritual_net.rs` | 367 | `GroupChannel::subscribe_tags` | the kind-445 group subscription (initial + every window-roll re-placement) — same unauthenticated runtime; GroupChannel holds no keys at all today |
| `crates/molt-net/src/ritual_net.rs` | 176 | `RitualNet::publish` | the publish runtime that must STAY unauthenticated (§7.5 / mdk_evaluation §5) — the fix must not fold the two into one authed runtime |
| `crates/molt-net/src/ritual_net.rs` | 240 | `RitualInbox::live` | bool-only replay gate; needs a counts-returning twin so the engine can apply a ≥1 rule and warn on partial |
| `crates/molt-net/src/ritual_net.rs` | 396 | `GroupSub::live` | the 445 twin of the same bool-only gate |
| `crates/molt-engine/src/nostr_ritual.rs` | 95 | `spawn_founder_inbox` | `let _ = inbox.live(LIVE_WAIT).await;` — the discarded result; lines 96-122 then render and publish every seat link unconditionally (subscribe-before-advertise degraded to advertise-blind) |
| `crates/molt-engine/src/nostr_ritual.rs` | 431 | `member_join` | `let _ = inbox.live(LIVE_WAIT).await;` — the joiner announces its JoinRequest (line 439) with an unproven inbox, so the founder's reply can land nowhere |
| `crates/molt-engine/src/nostr_ritual.rs` | 526 | `member_join (group sub)` | `let _ = sub.live(LIVE_WAIT).await;` then the UNBOUNDED Seal wait at 531-562 — a never-readable 445 sub means the join hangs forever with no message |
| `crates/molt-engine/src/nostr_ritual.rs` | 330 | `spawn_founder_group_recv` | `chan.subscribe()` with NO live() gate at all — a founder whose 445 sub never replays waits forever for Signed frames |
| `crates/molt-engine/src/founding.rs` | 1845 | `cmd_net_all_joined (group birth)` | `GroupChannel::new(dialer, relays, seed)` — the founder-side 445 channel construction; the second is nostr_ritual.rs:521 (member). Both would need a keys argument IF the anchor (not an ephemeral key) is chosen for 445 AUTH |
| `crates/molt-engine/src/founding.rs` | 1628 | `cmd_net_ritual_failed` | the existing failure sink: sets create.run.outcome = 2 and pushes `✗ founding failed: …` — the provisioning failure needs no new Command |
| `crates/molt-engine/src/lifecycles.rs` | 1583 | `cmd_net_join_failed` | the member-side twin: join.run.outcome = 2 plus `✗ join failed: …` — what an Err from member_join becomes |
| `crates/molt-engine/tests/nostr_founding.rs` | 88 | `a_republic_founds_and_a_member_joins_over_one_relay` | the existing Command-driven two-engine harness (engine(), adopt_relay(), wait_for()) the new keystones extend — swap MockRelay for a NIP-42 LocalRelay and the choreography is unchanged |
| `crates/molt-net/tests/nostr_relay_runtime.rs` | 493 | `nip42 auth test` | proves the test double works: `LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42{ mode: Read }))` allows unauthenticated WRITES, refuses reads until AUTH, and the mock accepts ANY well-formed AUTH event (registry inner.rs: only check_challenge) |

### Red tests first

- **`ritual_endpoints_sync_and_deliver_on_an_auth_required_relay`** — in `crates/molt-net/tests/nostr_ritual_net.rs`, driven through **molt_net::ritual_net::RitualNet::inbox() / send_ritual() and GroupChannel::subscribe() / publish_frame() — the exact public APIs the engine calls at nostr_ritual.rs:81, 427, 330, 522**.
  - asserts: Against a `LocalRelay::new(RelayBuilder::default().nip42(RelayBuilderNip42{ mode: RelayBuilderNip42Mode::Read }))`: (1) `founder.inbox()` then `inbox.live(RECV_TIMEOUT)` is TRUE; (2) the joiner's `send_ritual` delivery arrives with the proven sender; (3) a `GroupChannel` over the same relay: `subscribe()` then `live()` TRUE and `recv()` yields the published 445 frame. Read mode is required — Write/Both would refuse the publishes and the test would be red for the wrong reason.
  - red today because: ritual_net.rs:214 and :367 build their runtimes with `RelayRuntime::new(...)`, whose auth_keys is None (relay_runtime.rs:150). The relay answers the REQ with CLOSED `auth-required:` plus an AUTH challenge; read_session hits `let Some(keys) = &shared.auth_keys else { continue }` (relay_runtime.rs:587) and never authenticates, so no EOSE and no events: live() returns false and recv() returns None. This is the fast (seconds) twin to iterate against.
- **`a_founding_and_join_complete_over_an_auth_required_relay`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::RelayAdd/RelayConfirm/RelayClearnetSession, Command::CreateStart, Command::JoinStart, Command::CreatePropose, Command::JoinConfirmCharter on two real engines (the existing engine()/adopt_relay()/wait_for() harness)**.
  - asserts: Same choreography as the existing capstone but over a NIP-42 Read-mode LocalRelay instead of MockRelay: the seat link becomes a parseable v2 handover, the founder reaches can_propose, the joiner reaches awaiting_ratify, both seal (create.run.outcome == 1, joiner on Screen::Main with a workspace).
  - red today because: The joiner's JoinRequest publishes fine (Read mode allows unauthenticated writes), but the founder's 1059 inbox is an unauthenticated subscription that the relay never replays, so `NetJoinRequested` is never fed to the actor: `wait_for(&a, "the founder to accept petra's join", |s| s.create.can_propose)` panics at its 30 s deadline with an empty create log — the exact silent timeout the cluster describes (no NetRitualFailed, no notice, nothing).
- **`a_founding_refuses_when_its_inbox_subscription_never_becomes_readable`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::CreateStart (plus the relay-adoption commands) on one real engine**.
  - asserts: Against a `LocalRelay` built with a `QueryPolicy` that returns `PolicyResult::Reject("no reqs")` for every filter (writes still accepted; import QueryPolicy/PolicyResult/BoxedFuture from `nostr_relay_builder::prelude`): within the wait budget `s.create.run.outcome == 2`, the `✗ founding failed:` line names the unreadable inbox and the relay url, and NO seat link ever parses as a v2 handover (`molt_engine::FoundingInvite::parse(&seat.link).is_err()` for every seat) — the link must never be advertised over a subscription nothing accepted.
  - red today because: `spawn_founder_inbox` discards live()'s bool (nostr_ritual.rs:95) and unconditionally renders + sends `NetRitualLinkReady` for every seat (96-122). The relay CLOSEs each REQ with `error: no reqs`, so subscribe() itself still succeeds (connect_and_req only sends the REQ, relay_runtime.rs:448-457) and the existing Err path at nostr_ritual.rs:83 is never taken: today the link IS published and outcome stays 0 forever, so both assertions fail.
- **`a_join_refuses_when_the_group_channel_never_becomes_readable`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::CreateStart / JoinStart / CreatePropose on two real engines**.
  - asserts: Against a `LocalRelay` whose QueryPolicy rejects only filters whose `kinds` contain 445 (1059 REQs pass, so founding + join + Welcome all work): the joiner reaches `join.run.outcome == 2` with a `✗ join failed:` line naming the group channel, and the founder reaches `create.run.outcome == 2` from its own group-recv gate.
  - red today because: nostr_ritual.rs:526 discards live() and falls into the UNBOUNDED Seal wait (531-562): the join hangs forever with outcome 0 and an empty log. The founder side is worse — `spawn_founder_group_recv` (nostr_ritual.rs:330) never calls live() at all, so it too waits forever. This is the only test that pins the member-side and founder-side 445 gates; under the ≥1 rule a mixed pool cannot trigger them, so the double must refuse the 445 filter specifically.

### Fix steps

1. Step 0 (order): land AFTER cluster C and E if those are in flight — all three edit crates/molt-engine/src/nostr_ritual.rs, and E rewrites the same GroupSub::recv/live area of ritual_net.rs. Coordinate or rebase; do not fork the file.
2. Step 1 — crates/molt-net/src/relay_runtime.rs: add `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct SyncState { pub synced: usize, pub connected: usize }` with `pub fn any(&self) -> bool { self.synced > 0 }` and `pub fn full(&self) -> bool { self.synced >= self.connected }`. Add `pub async fn sync_state(&mut self, timeout: Duration) -> SyncState` — the existing Subscription::synced loop (relay_runtime.rs:314-324), but on timeout it returns the counts instead of `false`. Reimplement `synced()` as `self.sync_state(timeout).await.full()` so every existing assertion in nostr_relay_runtime.rs / nostr_ritual_net.rs stays byte-for-byte valid.
3. Step 2 — crates/molt-net/src/ritual_net.rs:214 (RitualNet::inbox): append `.with_auth_keys(Some(self.keys.clone()))` to the runtime. Comment the reason: the 1059 filter is `#p = our anchor`, so the relay ALREADY knows this anchor — AUTH with the same key adds no correlation the REQ did not disclose. This is the recorded decision in nostr_n4_plan.md §10.
4. Step 3 — crates/molt-net/src/ritual_net.rs:367 (GroupChannel::subscribe_tags): append `.with_auth_keys(Some(Keys::generate()))` — a FRESH ephemeral key per placement (and per window-roll re-placement). The 445 filter names only an h tag; authenticating it with the roster anchor would hand every relay operator the anchor→group-id link for the life of the republic (mdk_evaluation.md §5's gap, inverted). See open_questions — if the user picks the anchor instead, GroupChannel::new grows a `Keys` argument and BOTH call sites change (crates/molt-engine/src/founding.rs:1845, crates/molt-engine/src/nostr_ritual.rs:521), and the Debug impl at ritual_net.rs:307 must keep redacting it.
5. Step 4 — LEAVE ritual_net.rs:176 (RitualNet::publish) and :347 (GroupChannel::publish_frame) unauthenticated, and say so in a comment at each: an authed publish channel links every ephemeral-key event to the member (§7.5). `publish_one` already refuses an `auth-required:` OK loudly (relay_runtime.rs:736-740).
6. Step 5 — crates/molt-net/src/ritual_net.rs: add `pub async fn live_state(&mut self, timeout: Duration) -> SyncState` to RitualInbox (next to :240) and GroupSub (next to :396), delegating to Subscription::sync_state; keep `live()` as the bool wrapper.
7. Step 6 — crates/molt-engine/src/nostr_ritual.rs:95 (spawn_founder_inbox): replace the discard with `let st = inbox.live_state(LIVE_WAIT).await;` — if `!st.any()`, send `Command::NetRitualFailed { error: "the founding inbox is not readable on any relay — no relay replayed the subscription (auth required, rate limited, or refused)", generation }` and RETURN BEFORE rendering any link. If `st.any() && !st.full()`, `tracing::warn!(synced = st.synced, connected = st.connected, …)` and proceed.
8. Step 7 — crates/molt-engine/src/nostr_ritual.rs:431 (member_join): same gate, `return Err("the join inbox is not readable on any relay …")` (becomes NetJoinFailed) BEFORE `net.send_ritual(...)` at :439.
9. Step 8 — crates/molt-engine/src/nostr_ritual.rs:526: same gate with group wording (`"the group channel is not readable on any relay …"`) before the Seal wait.
10. Step 9 — crates/molt-engine/src/nostr_ritual.rs:330 (spawn_founder_group_recv): add the MISSING gate — after `chan.subscribe()` succeeds, `let st = sub.live_state(LIVE_WAIT).await;` and on `!st.any()` send NetRitualFailed naming the group channel, then return.
11. Step 10 — the ≥1 rule, stated once in the module doc: `synced == 0` is a provisioning failure (no relay is proven readable); `0 < synced < connected` is a warning, not a failure. This mirrors the pool's ≥1-OK publish and ≥1-accepted-REQ semantics; failing on ANY unsynced relay would let one lagging relay in a healthy pool kill every founding.
12. Step 11 — regression GUARD (green today, cheap, and the thing most likely to be broken by step 2): in crates/molt-net/tests/nostr_ritual_net.rs, against a `RelayBuilderNip42Mode::Both` relay, `RitualNet::send_ritual` must fail with an error containing "refused to link the publish key". Verify it is a real guard by temporarily adding `.with_auth_keys(...)` to ritual_net.rs:176 and watching it go red.
13. Step 12 — docs: mark §G done in docs/transport/nostr_n4a_review_followups.md, and update the NIP-42 bullet in docs/transport/nostr_n4_plan.md §10 to record what actually shipped (anchor AUTH on the 1059 inboxes, ephemeral-per-subscription on 445, and the remaining whitelist-relay limitation).
14. Step 13 — `cargo clippy --all-targets -p molt-net -p molt-engine` at zero (`.expect("…")` in tests, never `.unwrap()`), then the four tests green, then commit on master.

### Files edited

- `crates/molt-net/src/relay_runtime.rs`
- `crates/molt-net/src/ritual_net.rs`
- `crates/molt-engine/src/nostr_ritual.rs`
- `crates/molt-net/tests/nostr_ritual_net.rs`
- `crates/molt-engine/tests/nostr_founding.rs`
- `docs/transport/nostr_n4a_review_followups.md`
- `docs/transport/nostr_n4_plan.md`

### Risks

- NO byte-layout risk: roster_canonical_bytes / republic_id / checkpoint tags, SealedRoster and WorkspaceEvent are untouched. NO co-equality risk: no new Command variant (NetRitualFailed and NetJoinFailed already exist), so molt-mcp's INTERNAL[45] list is unchanged. The partial-sync warning must therefore go to tracing::warn!, NOT the founding log — there is no generic log-note Command, and inventing one would break the co-equality test.
- Privacy: step 2 makes the ritual anchor sign an AUTH event on every inbox connection AND every supervisor reconnect. That is the accepted N4 §10 tradeoff, but it MUST NOT leak into the publish runtimes — a refactor that stores one authed RelayRuntime on RitualNet and reuses it for publish would reproduce exactly the MDK §5 gap (every ephemeral-key 445 linkable to the member). Step 11's guard exists for that.
- Making live() fatal turns 'accepted the REQ but never sent EOSE' into a hard refusal. A relay that delivers live traffic without ever EOSEing (non-conformant, but possible) would now break foundings that previously limped. The ≥1 rule bounds the blast radius to pools where NO relay replays; if a real relay is found that behaves this way, the honest fix is a per-relay diagnosis, not weakening the gate back to a discard.
- Latency: a failing pool now costs a full LIVE_WAIT (10 s) before the refusal appears, twice on the member path (inbox, then group). The existing wait_for budget in nostr_founding.rs is 30 s per predicate — enough, but a test that chains both gates should not tighten it.
- Test-double traps: RelayBuilderNip42 defaults to Both mode, which refuses unauthenticated WRITES — the happy-path test must use Read or it goes red for the wrong reason. The QueryPolicy-reject double makes the supervisor reconnect-loop at 1 s backoff for the duration of the gate; assert on session outcome only, never on health()/connection counts, or cluster E's reconnect rework will flake it.
- File collision: clusters C (spawn_publish_frame_with fail sink) and E (GroupSub::recv resubscribe failure) edit the same two files. E in particular may restructure GroupSub::recv/live — land G after E or merge deliberately.
- The 445 ephemeral AUTH key is minted per subscribe_tags call, so a window-roll resubscribe (ritual_net.rs:413) presents a NEW pubkey to the relay mid-founding. On a relay that ties access to the authenticated key that is a fresh auth round, not a failure — but on a whitelist relay it fails the same way the first one did (see open question).

### Open questions

- Which identity authenticates the kind-445 GROUP subscription? nostr_n4_plan.md §10 decided the anchor for the 1059 INBOXES only, and 445 is the opposite privacy case: its filter is an anonymous h tag. (a) FRESH ephemeral key per subscription — recommended default, no API ripple (GroupChannel keeps its 3-arg constructor), works on any relay that uses AUTH as anti-spam, but is refused by a relay that whitelists known pubkeys (plausibly our own recommended self-hosted onion relay, once it has a whitelist). (b) The member's roster anchor — works wherever the operator is whitelisted, but permanently hands every relay operator the anchor→group-id link, and that link survives into the N5 runtime subscriptions. Guessing (b) is the expensive mistake: it is a silent, irreversible deanonymization of group membership, not a bug that surfaces as a failure.

## Cluster H — Unpinned security checks

**Verdict.** All four items still hold at HEAD (88dd854): each check exists in the shipping code and no test observes it — deleting any of the four leaves the suite green. Worse than reported on item 2: the "N1 loopback twin" the backlog credits (founding.rs:2931) swaps the member's own identity_pk, which trips `verify_seal_proposal`'s self-anchor check BEFORE the byte comparison is ever reached, so the existing keystone stays green with founding.rs:1472-1478 deleted too — the genesis byte comparison is pinned on NEITHER path. Two adjacent defects found while verifying: the PoP refusal string (founding.rs:2005) and the ticket-MAC refusal string (founding.rs:2015) each carry a ~25-space run from a forgotten `\` line continuation, and `SealedRoster.roster` is covered by neither `roster_canonical_bytes` nor `republic_id` yet drives `materialize_workspace` — so a tampered `roster` list survives both the genesis byte comparison and `verify_sealed_roster`.

### Anchors (verified at 88dd854 — RE-VERIFY, the tree has moved)

| file | line | symbol | why |
|---|---|---|---|
| `crates/molt-engine/src/founding.rs` | 2000 | `State::cmd_net_join_requested — `if is_nostr { … claimed != sender_npub … }`` | H1: THE proof-of-possession gate (2000-2011). Now on the actor, so it is state-testable; nothing in crates/ references its refusal today. |
| `crates/molt-engine/src/founding.rs` | 2005 | `the PoP refusal log line` | H1 bonus defect: the literal reads "…transport key it did not                          sign with…" (26 spaces, missing `\` continuation). Same bug at line 2015 for the ticket-MAC refusal. |
| `crates/molt-engine/src/nostr_ritual.rs` | 616 | `member_join — `if sealed_table != table`` | H2 (Nostr shipping path): the genesis sign-what-you-see byte comparison, 616-622. No test reaches it. |
| `crates/molt-engine/src/founding.rs` | 1472 | `run_ritual_member — `if sealed_table != table`` | H2 (loopback twin): the same check, 1472-1478. |
| `crates/molt-engine/src/founding.rs` | 2931 | `a_sealed_roster_differing_from_the_ratified_proposal_is_rejected` | H2: the supposed keystone. Its swap (evil identity_pk/nostr_pk at founding.rs:3055-3056) makes verify_seal_proposal fail with "does not anchor our own (name, key)" first — the test only asserts `outcome.is_err()`, so it stays GREEN if 1472-1478 is deleted. Semi-inert. |
| `crates/molt-engine/src/nostr_ritual.rs` | 475 | `member_join accept loop — `RitualMsg::LinkSpent { .. }, sender) if sender == h.npub`` | H3: founder-identity guard on the 1059 inbox; twins at 469 (JoinAccepted), 483 (Welcome, accept loop), 497/499 (Welcome + LinkSpent, welcome-wait loop). `h.npub` is canonical by construction (invite.rs:200-206 canonicalizes at decode), so the comparison is sound — just unpinned. |
| `crates/molt-net/src/ritual_net.rs` | 412 | `GroupSub::recv — `if current != self.tags { … subscribe_tags … }`` | H4: the §4.4 window-roll resubscribe, 410-425. Only the pure `window_tags` helper is pinned (nostr_ritual_net.rs:210); the roll itself is untested because `subscribe()` (line 356) reads `Timestamp::now()` with no seam. |
| `crates/molt-net/src/ritual_net.rs` | 356 | `GroupChannel::subscribe` | H4: the single wall-clock read that fixes a GroupSub's tags at birth — the one place a test seam is needed. |
| `crates/molt-engine/src/lifecycles.rs` | 1185 | `cmd_net_join_sealed — `verify_sealed_roster` defence-in-depth` | H2 caveat: any genesis differing in the SIGNED bytes also fails here (n-of-n attestations), so an E2E test must assert the refusal REASON to detect deletion of nostr_ritual.rs:616. |
| `crates/molt-engine/src/nostr_ritual.rs` | 147 | `spawn_founder_inbox — `sender_npub: sender`` | H1 wiring: the proven NIP-59 seal author the actor compares against. An actor-only test would not pin this feed; the E2E hostile-joiner test does. |
| `crates/molt-core/src/lib.rs` | 1790 | `roster_canonical_bytes (molt-roster-v3)` | Adjacent gap: `SealedRoster.roster` appears in neither these bytes nor `molt_storage::republic_id`, yet lifecycles.rs:1263 feeds `sealed.roster` to materialize_workspace. |

### Red tests first

- **`a_request_claiming_a_transport_key_it_did_not_sign_with_is_refused`** — in `crates/molt-engine/tests/nostr_ritual_adversarial.rs (new)`, driven through **Command::CreateStart on a real founder engine over a MockRelay, then a real gift-wrapped kind-446 JoinRequest on the wire (molt_net::ritual_net::RitualNet::send_ritual) → spawn_founder_inbox → Command::NetJoinRequested → cmd_net_join_requested**.
  - asserts: Attacker holds nostr key A, claims the VICTIM's anchor V in `nostr_pk` and mints a valid v2 MAC over it (the ticket is in the link, so the MAC proves nothing about possession). After the wrap lands: seats[0].member stays empty, create.can_propose stays false, and create.run.log carries a line containing "transport key it did not sign with". CONTROL: the same attacker re-sends with nostr_pk = A and a matching MAC → seats[0].member == "mallory" and can_propose flips true (the PoP refusal did not spend the ticket).
  - red today because: RED AT HEAD for a real defect: the asserted substring does not exist — founding.rs:2005 contains "did not                          sign with" (26 spaces from a missing `\` continuation). After that string is repaired the test is green, and RED AGAIN when founding.rs:2000-2011 is deleted (the impersonating request then anchors V and can_propose flips true). Unlike an actor-level Command injection this also pins the feed at nostr_ritual.rs:147.
- **`a_genesis_that_is_not_the_ratified_table_is_refused_over_nostr`** — in `crates/molt-engine/tests/nostr_ritual_adversarial.rs (new)`, driven through **Command::JoinStart + Command::JoinConfirmCharter on a real joiner engine against a hand-written hostile founder (RitualNet + MlsMember + GroupChannel over one MockRelay) → nostr_ritual::member_join**.
  - asserts: The hostile founder runs the ritual honestly (JoinAccepted, kind-444 Welcome with the invite's exact relay list, 445 Seal of proposal P with agenda "play chess"), collects the joiner's Signed, then publishes a 445 Genesis identical to P except agenda = "a charter nobody ratified" (republic_id does NOT cover the agenda, so the table stays self-consistent and passes verify_seal_proposal + check_proposal_provenance). Assert: join.run.outcome == 2, the run log contains "not the table we ratified", and workspaces stays empty.
  - red today because: Green at HEAD (this is a coverage pin): the red step is the deletion experiment — comment out nostr_ritual.rs:616-622 and the joiner accepts the swapped genesis, emits NetJoinSealed, and the refusal comes from lifecycles.rs:1185 with a different message ("attestation for petra does not verify"), so the log assertion fails. The reason-coupling is deliberate and must be commented in the test: the actor's defence-in-depth would otherwise mask the deletion.
- **`a_sealed_roster_differing_from_the_ratified_proposal_is_rejected (repair the existing loopback keystone)`** — in `crates/molt-engine/src/founding.rs (mod tests, line 2931)`, driven through **founding::run_ritual_member (the loopback member entry the ritual actually spawns)**.
  - asserts: Change the swap from an identity swap to an AGENDA swap (keep bob's three anchors intact, keep the republic id, self-sign the founder attestation) and assert the returned Err contains "not the table we ratified" instead of the bare `is_err()`.
  - red today because: RED AT HEAD in its current form once the assertion is tightened: today's evil-identity swap fails one gate EARLIER ("the proposed roster does not anchor our own (name, key)", founding.rs:960), so the existing test proves verify_seal_proposal, not the byte comparison — it stays green with founding.rs:1472-1478 deleted. After the rewrite the deletion makes run_ritual_member return Ok(JoinOutcome) (that task runs no verify_sealed_roster), so the failure is STRUCTURAL, not message-coupled.
- **`a_1059_frame_from_anyone_but_the_link_founder_cannot_kill_a_join`** — in `crates/molt-engine/tests/nostr_ritual_adversarial.rs (new)`, driven through **Command::JoinStart on a real joiner engine → nostr_ritual::member_join's 1059 accept/welcome loops (same hostile-founder harness, plus a second RitualNet under an unrelated key)**.
  - asserts: Before the genuine JoinAccepted, the imposter gift-wraps RitualMsg::LinkSpent{seat:0} to the joiner's anchor (learned from the JoinRequest — on a real relay it is public in the Welcome's #p tag); after the joiner logs "the founder accepted" (join.run.progress_pct == 45) the imposter gift-wraps a WelcomePayload with the CORRECT relay list but garbage MLS welcome bytes. Assert the join survives both and reaches join.awaiting_ratify with the genuine charter.
  - red today because: Green at HEAD; the red step is deleting either guard. Without nostr_ritual.rs:475/499 the join dies with "this invite link was already used by someone else"; without 483/497 it dies at "mls welcome: …". Both are structural (outcome 2 instead of awaiting_ratify) — an unauthenticated observer can DoS every join.
- **`a_group_subscription_placed_yesterday_rolls_onto_todays_tag`** — in `crates/molt-net/tests/nostr_ritual_net.rs (extend)`, driven through **GroupChannel::subscribe_at(now - H_WINDOW) then GroupSub::recv — the same recv body nostr_ritual.rs:345 and :532 call in production**.
  - asserts: A GroupSub whose live filter carries YESTERDAY's h tag (exactly the state of a founding that began before UTC midnight) receives a frame published by the production GroupChannel::publish_frame under today's tag: recv returns the content and open_outer opens it under the exporter. Publish from a spawned task ~200 ms in, so delivery is live traffic on the fresh REQ and the test does not depend on MockRelay backlog replay.
  - red today because: Does not compile at HEAD (no `subscribe_at`). With the seam added it is green, and RED when ritual_net.rs:410-425 is deleted: the filter stays on yesterday's tag, the relay never sends the frame, and the `self.tags.contains(&h)` gate at line 437 would drop it anyway — recv times out. window_tags(D-1) and window_tags(D) always differ in element 0, so the trigger is deterministic at any wall-clock time.

### Fix steps

1. STEP 0 (molt-net first — it builds and tests without touching the engine or the GUI). In crates/molt-net/src/ritual_net.rs add the §4.4 test seam: `pub async fn subscribe_at(&self, now_secs: u64) -> Result<GroupSub, NetError>` containing today's body of `subscribe()` (window_tags(seed, now_secs) → subscribe_tags → GroupSub{sub, tags, channel}), and reduce `subscribe()` to `self.subscribe_at(Timestamp::now().as_secs()).await`. Do NOT touch the `Timestamp::now()` read inside `recv` (line 411) or in `publish_frame` — production semantics must stay byte-identical. Doc-comment it as "the window-roll seam: production calls `subscribe()`; a test places a subscription in a past window to exercise the roll" (missing_docs is a workspace warn).
2. STEP 1 (H4 red). Add the keystone to crates/molt-net/tests/nostr_ritual_net.rs as Keystone 7, next to `window_tags_cover_the_skew_margin`. Shape: MockRelay + `GroupChannel::new(dialer(), vec![url], [4u8;32])`; `let mut sub = chan.subscribe_at(now - H_WINDOW).await` where `now = nostr::Timestamp::now().as_secs()` (add nothing to Cargo.toml — use `std::time::SystemTime` to avoid a direct `nostr` dev-dep if it is not already there); spawn a task that sleeps 200 ms then calls `chan.publish_frame(&[9u8;32], b"after the roll")`; assert `sub.recv(Duration::from_secs(10))` yields content that `open_outer(&[[9u8;32]], &content)` opens. Prove red by commenting out ritual_net.rs:410-425, run, restore. Commit molt-net alone (cargo test -p molt-net --test nostr_ritual_net).
3. STEP 2 (H2a red, cheapest engine-side win). In crates/molt-engine/src/founding.rs mod tests, rewrite the swap inside `a_sealed_roster_differing_from_the_ratified_proposal_is_rejected` (2931): keep `identities` exactly as ratified, build the forged SealedRoster with `agenda: "a charter nobody ratified"` (republic_id unchanged — it does not cover the agenda), attestations self-signed by the founder over the forged table, and assert the Err string contains "not the table we ratified". Drop the now-false `verify_sealed_roster(&sealed).is_ok()` assertion (an agenda swap invalidates bob's attestation) and replace it with a comment naming what the swap defeats. Prove red by deleting founding.rs:1472-1478 → the member task returns Ok.
4. STEP 3 (H1 string repair). Fix the two malformed operator strings in founding.rs: line 2005 ("…transport key it did not                          sign with…") and line 2015 ("…does not match — refused (wrong or                      edited link…") — insert the missing `\` line continuation so the rendered line has single spaces. These are the strings the GUI run-log shows.
5. STEP 4 (the adversarial harness). Create crates/molt-engine/tests/nostr_ritual_adversarial.rs with `#![allow(missing_docs)]`, reusing the helpers of tests/nostr_founding.rs (copy `engine`, `adopt_relay`, `read_session`, `wait_for` — or lift them into tests/common/mod.rs if you prefer one copy). Add a `HostileFounder` struct built ONLY from public API: `molt_net::nostr_identity(b"hostile-founder", "self")` for the transport keys, `molt_net::invite::mint_ticket()`, `molt_engine::FoundingInvite{info: InviteInfo{republic, threshold:2, members:2, inviter:"walter", ticket}, handover: InviteHandoverV2{seat:0, ticket, npub, relays: vec![url]}}.render()` for the link, `RitualNet::new(Dialer::resolve("none","local",0)?, vec![url], &nsk)` + `inbox()` + `live()` BEFORE the joiner starts, `molt_storage::derive_identity_key(&[7u8;32], "walter")` + `MlsMember::new(&sk, "walter")` (the credential MUST equal info.inviter — check_proposal_provenance compares `from` to it) + `create_group()` + `add_members(&[kp])`, `ritual_net::mint_rotation_seed()`, `send_welcome` with `relays` EXACTLY equal to the invite's list, and `GroupChannel::new(dialer, vec![url], seed)` + `subscribe()` before the first Seal. Frame I/O: publish with `mls.encrypt(&serde_json::to_vec(&RitualMsg)?)` + `exporter_secret()` + `publish_frame`; read with `envelope::open_outer(&[exporter], &content)` + `mls.decrypt_at(&wire, created_at)`, skipping errors (own echoes never decrypt).
6. STEP 5 (H1 test). In that file: founder = a REAL engine (CreateStart 2-of-2 over a MockRelay, wait for a parseable link). Attacker = `RitualNet` under key A sending `RitualMsg::Join(JoinRequest{seat:0, name:"mallory", identity_pk: m_pk, nostr_pk: VICTIM_npub, mac: invite::join_mac(&h.ticket, "mallory", &m_pk, &VICTIM_npub), reply: None, key_package: hex::encode(MlsMember::new(&m_sk,"mallory")?.key_package()?)})` to `h.npub`. Assert the refusal (seat unanchored, !can_propose, the repaired log line), then the control request with nostr_pk = A anchors the seat. Prove red by deleting founding.rs:2000-2011.
7. STEP 6 (H3 test). Same file, harness path: joiner engine executes JoinStart; the harness waits for the JoinRequest, then an `imposter` RitualNet (unrelated key) sends `LinkSpent{seat:0}` to `j.nostr_pk`; then the genuine JoinAccepted + Welcome; then, after `wait_for(join.run.progress_pct == 45)`, the imposter sends a WelcomePayload{welcome: b"garbage", rotation_seed: [9u8;32], relays: <the invite's list>}. Publish the Seal in a retry loop (every 300 ms until `join.awaiting_ratify`) so the test never depends on relay backlog replay. Assert the join reaches awaiting_ratify with the genuine charter. Prove red by deleting each guard pair (475/499, then 483/497).
8. STEP 7 (H2b test). Same file, same harness: after `JoinConfirmCharter`, read the joiner's `Signed` off the 445 channel, then publish `Genesis{sealed: <P with agenda swapped>, welcome: String::new()}` in a retry loop until `join.run.outcome != 0`. Assert outcome == 2, the log contains "not the table we ratified", and `workspaces.is_empty()`. Add a comment stating WHY the assertion names the message (lifecycles.rs:1185 would refuse the swap anyway — the message is what proves the joiner's own gate fired). Prove red by deleting nostr_ritual.rs:616-622.
9. STEP 8. `cargo clippy --all-targets -p molt-net -p molt-engine` at zero (`.expect("…")` everywhere, no `as` casts), then `cargo test -p molt-net --test nostr_ritual_net` and `cargo test -p molt-engine --test nostr_ritual_adversarial --test nostr_founding -- --test-threads=2` (per-crate, -j 1 if RAM is tight; never alongside a molt-ui-window build). One commit per item is fine; land green on master.
10. STEP 9. Update docs/transport/nostr_n4a_review_followups.md §H: mark the four items DONE with the test names, and record the two verification findings — (a) the loopback keystone was semi-inert and why, (b) the malformed refusal strings. Add the `SealedRoster.roster` gap as a NEW open item (see open_questions) rather than silently fixing it here.

### Files edited

- `crates/molt-net/src/ritual_net.rs`
- `crates/molt-net/tests/nostr_ritual_net.rs`
- `crates/molt-engine/src/founding.rs`
- `crates/molt-engine/tests/nostr_ritual_adversarial.rs`
- `docs/transport/nostr_n4a_review_followups.md`

### Risks

- No `Command` variant is added or changed → `co_equality_every_command_is_a_tool_or_documented_internal` is untouched. No signed byte layout is touched: the harness builds its rosters through the public `molt_storage::republic_id` + `molt_core::roster_canonical_bytes`, so a future molt-roster-v4 bump does not silently invalidate the tests.
- `GroupChannel::subscribe_at` is new PUBLIC molt-net API. Keep it a thin delegate with a doc comment naming it the §4.4 seam; production must keep calling `subscribe()`. Leaving the `Timestamp::now()` read inside `recv` untouched is what keeps the roll detector on the real clock in production.
- MockRelay backlog replay is NOT guaranteed for a REQ placed after a publish. Both the H4 test and the harness must publish AFTER the subscriber is live (H4: a 200 ms delayed publish task; harness: retry the Seal/Genesis publish until the joiner's session state moves). A test that relies on replay will be flaky under load.
- clippy at zero including tests: `.expect("…")` not `.unwrap()`, and `as_conversions` is a warn — use `u64::from`/`try_from` in the H4 time arithmetic. `missing_docs` warns on the new public fn.
- The H2b assertion is coupled to the refusal wording because `cmd_net_join_sealed`'s `verify_sealed_roster` (lifecycles.rs:1185) refuses a swapped genesis regardless. Anyone rewording nostr_ritual.rs:617-621 will make it red — that is intended; say so in the test comment so the next reader does not "fix" it by loosening the assertion.
- Engine tests here spawn two storage-backed engines plus a MockRelay each; run them per-crate and never next to a `molt-ui-window` build (OOM-killer rule). Use generous `wait_for` deadlines (30 s, as nostr_founding.rs does) — the founder's 90 s accept window is not the bottleneck, EOSE/live() waits are.
- FOUND WHILE VERIFYING, not fixed by this cluster: `SealedRoster.roster` (the Vec<String> membership list) is covered by neither `roster_canonical_bytes` nor `republic_id`, so a hostile founder can hand a joiner a genesis whose `roster` differs while both the byte comparison (nostr_ritual.rs:616) and `verify_sealed_roster` pass — and `lifecycles.rs:1263` feeds exactly that field to `materialize_workspace`. See open_questions.

### Open questions

- `SealedRoster.roster` is an UNSIGNED constitutional field (absent from `roster_canonical_bytes` molt-roster-v3 and from `republic_id` molt-republic-id-v2) that nevertheless becomes the materialized member list. Three ways to close it, with very different costs: (a) cheap and additive — cross-check `roster` against `identities` (same set, same order) inside `verify_sealed_roster` + `verify_seal_proposal`, no byte-layout change; (b) drop the field entirely and derive the roster from `identities` at every read (touches ~15 Founded/materialize sites); (c) bind it into the canonical bytes → molt-roster-v4, which ripples through every recompute site and every byte-pin test. Which one — and is it this cluster's business at all, or a new backlog entry? A wrong guess here is expensive in exactly the way CLAUDE.md warns about, so it should not be decided inside a coverage change-set.

## Cluster D — Join-task lifecycle — ✅ LANDED (`9809f6f`)

*Kept for the record: this plan was written before `9809f6f` landed. Read it as the reasoning behind that commit, not as work to do.*

**Verdict.** The defect is real at HEAD (88dd854), exactly as described. `cmd_create_start` (lifecycles.rs:665) calls `teardown_ritual()` — which only clears `net_ritual` / `founder_mesh_in` / `runtime_transport` (founding.rs:549-553) — and never touches `join_generation` or `join_task`; `cmd_recover_start` (lifecycles.rs:1348) bumps only `recover_generation` (line 1376). The one and only gate in `cmd_net_join_sealed` is `generation != Some(self.join_generation) || self.session.join.run.outcome != 0` (lifecycles.rs:1148) — neither clause moves when a founding/recovery starts, so a late `NetJoinSealed` from the abandoned Nostr member task (emitted at nostr_ritual.rs:634) still passes, materializes a foreign workspace, sets `active_workspace` and flips `session.screen = Screen::Main` (lifecycles.rs:1292-1334) out from under the founding wizard. The task itself is also never aborted, so it keeps its relay sockets and its charter-confirm channel alive. Two related entry points share the root cause and are also unguarded: `cmd_open_workspace` (session.rs:781) invalidates no join, and `cmd_recover_start` — unlike `cmd_join_start` (lifecycles.rs:1030) — does not `teardown_ritual()`, so an in-flight FOUNDING can seal into a recovery session via `maybe_finalize` (lifecycles.rs:765).

### Anchors (verified at 88dd854 — RE-VERIFY, the tree has moved)

| file | line | symbol | why |
|---|---|---|---|
| `crates/molt-engine/src/lifecycles.rs` | 695 | `cmd_create_start → self.teardown_ritual()` | the only context-invalidation the founding entry point does; the join is untouched here — this is where invalidate_join() goes |
| `crates/molt-engine/src/lifecycles.rs` | 665 | `State::cmd_create_start` | production handler for Command::CreateStart (dispatch at lib.rs:1150); validation + guard_idle run at 672-687, before the teardown |
| `crates/molt-engine/src/lifecycles.rs` | 1376 | `cmd_recover_start → self.recover_generation += 1` | the recovery arming point: bumps ONLY recover_generation; join_generation and join_task survive — second insertion point |
| `crates/molt-engine/src/lifecycles.rs` | 1363 | `cmd_recover_start → if !self.persist { return Err(..) }` | the last early-return before arming; invalidate_join() must sit AFTER it so an erroring RecoverStart changes nothing |
| `crates/molt-engine/src/lifecycles.rs` | 1148 | `cmd_net_join_sealed generation gate` | `generation != Some(self.join_generation) \|\| self.session.join.run.outcome != 0` — the ONLY guard; nothing else stops the hijack |
| `crates/molt-engine/src/lifecycles.rs` | 1599 | `State::cmd_join_cancel` | the correct template: 1604 bumps join_generation, 1605 clears join_confirm, 1606-1608 takes+aborts join_task, 1609 resets session.join — extract as invalidate_join() |
| `crates/molt-engine/src/lifecycles.rs` | 1292 | `cmd_net_join_sealed → self.session.join = JoinState::default() / push_workspace_entry / screen = Main` | the hijack payload: a late seal materializes a workspace, sets active_workspace and switches the screen |
| `crates/molt-engine/src/founding.rs` | 549 | `State::teardown_ritual` | proves teardown_ritual clears only net_ritual/founder_mesh_in/runtime_transport — it is NOT a join invalidation |
| `crates/molt-engine/src/lib.rs` | 706 | `State::join_task` | Option<JoinHandle<()>> — documented as aborted 'on cancel or on a restarted join'; founding/recovery are the missing cases |
| `crates/molt-engine/src/nostr_ritual.rs` | 634 | `spawn_member_join → Command::NetJoinSealed{..}` | the real emitter of the late seal the abandoned task sends; the red tests inject byte-identically this command |
| `crates/molt-engine/src/session.rs` | 781 | `State::cmd_open_workspace` | same-class third entry point (only callers: dispatch lib.rs:1099 and cmd_restore_finish lifecycles.rs:656) — also invalidates no join |
| `crates/molt-engine/tests/nostr_founding.rs` | 48 | `engine() / adopt_relay() / wait_for() over nostr_relay_builder::MockRelay` | the in-process-relay harness that makes an actor-level in-flight join reachable through Commands only — reuse for the keystones |

### Red tests first

- **`founding_invalidates_an_in_flight_join`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::CreateStart (engine a) → Command::JoinStart (engine b) → Command::CreateStart (engine b) → Command::NetJoinSealed{generation: Some(1)} (engine b) — all through WalletHandle::execute, no test-only API**.
  - asserts: Pre-check (before b's CreateStart): a.create.can_propose == true and b.join.run.outcome == 0 — the join is genuinely live, so a later green cannot be for the wrong reason. After injecting the abandoned join's seal: b's session lists NO workspace named "R", b.screen == Screen::Create, b.create.run.outcome == 0, b.active_workspace is unchanged. Build the injected payload in-test like the unit helper valid_sealed_roster(): two MemberIdentity seats ("founder", "petra"), petra's nostr_pk = molt_net::nostr_identity(b"petra-entropy", "ticket-petra").1, republic_id = molt_storage::republic_id("R",2,2,&ids), attestations = molt_storage::identity_sign over molt_core::roster_canonical_bytes(...), and pass hex(petra_sk) as nostr_sk so the persist path would really materialize.
  - red today because: cmd_create_start leaves join_generation == 1 and session.join.run.outcome == 0, so the gate at lifecycles.rs:1148 passes: cmd_net_join_sealed validates the roster + the matching nostr secret, calls materialize_workspace, pushes the "R" workspace and sets screen = Main — the founding wizard is hijacked mid-founding.
- **`recovery_invalidates_an_in_flight_join`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::CreateStart (a) → Command::JoinStart (b) → Command::RecoverStart (b, link = molt_engine::RecoveryInvite{..}.render(), any phrase) → Command::NetJoinSealed{generation: Some(1)} (b)**.
  - asserts: RecoverStart returns Ok (it fails honestly on session.notice == "recover-failed:…" but the context is armed); after the injected seal b lists no "R" workspace and b.screen != Screen::Main. Same pre-check that the join was live.
  - red today because: cmd_recover_start bumps only recover_generation (lifecycles.rs:1376) and never aborts join_task, so join_generation stays 1 and the injected seal materializes the workspace and switches the screen.
- **`founding_and_recovery_abort_the_join_task`** — in `crates/molt-engine/src/lib.rs (mod tests, next to join_reports_from_a_stale_generation_are_dropped_while_live at 4056)`, driven through **State::cmd_create_start and State::cmd_recover_start — the exact fns the CreateStart/RecoverStart dispatch arms call (lib.rs:1150 / lib.rs:1182)**.
  - asserts: Inside rt().block_on: build the State inline (State::new(GroupConfig::demo(), …)) and KEEP a strong cmd_tx clone alive — plain_state()'s WeakSender cannot upgrade, so start_ritual would return Err("engine stopped"); set st.ritual_material_sink = Some(tx) (manual mode) so start_ritual takes the loopback path with no relays. Install a sentinel: let (tx, rx) = oneshot::channel(); st.join_task = Some(tokio::spawn(async move { let _hold = tx; std::future::pending::<()>().await })). Then cmd_create_start(...) → assert st.join_task.is_none() AND tokio::time::timeout(5s, rx).await.expect("the abandoned join task must be aborted").is_err() (the sender drops only when the aborted future is dropped). Repeat the second half for cmd_recover_start on a persist State (tempfile::tempdir + the existing recover_link("bob", "f00d") helper at lib.rs:4301).
  - red today because: neither handler touches join_task, so the sentinel task keeps running: join_task stays Some and the timeout elapses.
- **`opening_a_workspace_invalidates_an_in_flight_join`** — in `crates/molt-engine/src/lib.rs (mod tests) — covers step 5`, driven through **State::cmd_open_workspace (dispatch arm lib.rs:1099) then State::cmd_net_join_sealed (dispatch arm lib.rs:1277)**.
  - asserts: plain_state() with one WorkspaceInfo injected into session.workspaces (persist = false, so no stored dir is needed); arm the join (join_generation = 1, session.join = JoinState{run: RunCore::started(), member: "petra", seed: <phrase>, ..default}); cmd_open_workspace(id) → cmd_net_join_sealed(valid_sealed_roster json, …, Some(1)) → session.workspaces.len() unchanged and session.active_workspace still the opened id.
  - red today because: cmd_open_workspace bumps net_generation/net_scope but never join_generation, so the late seal appends the "R" workspace and re-points active_workspace at it.
- **`recovery_start_tears_down_an_in_flight_founding`** — in `crates/molt-engine/src/lib.rs (mod tests) — covers step 6, the symmetric hole`, driven through **State::cmd_create_start → State::cmd_recover_start → State::maybe_finalize (called from every ritual seal handler)**.
  - asserts: persist State on a tempdir, manual-mode founding (2 members, 1 seat); mark the seat sealed (st.session.create.seats[0].state = 2); cmd_recover_start(recover_link("bob","f00d"), phrase); then maybe_finalize() → st.session.create.run.outcome == 0 and st.session.workspaces is unchanged (nothing was founded into the recovery session).
  - red today because: cmd_recover_start never calls teardown_ritual, so net_ritual is still installed; maybe_finalize takes it, finalizes the founding, sets outcome = 1, pushes the workspace and screen = Main during a recovery.

### Fix steps

1. 1. Write red tests 1+3 in crates/molt-engine/tests/nostr_founding.rs (reuse engine()/adopt_relay()/wait_for(); add a local `injected_seal()` helper building the SealedRoster + matching nostr secret). Run `cargo test -p molt-engine --test nostr_founding founding_invalidates recovery_invalidates` and watch both fail with the "R" workspace present / screen Main.
2. 2. Write red test 2 (task abort) in crates/molt-engine/src/lib.rs mod tests, beside join_reports_from_a_stale_generation_are_dropped_while_live (line 4056). Watch it fail on `join_task.is_none()` and on the 5s timeout.
3. 3. Add the shared helper in crates/molt-engine/src/lifecycles.rs immediately above cmd_join_cancel (line 1599): `pub(crate) fn invalidate_join(&mut self) { self.join_generation += 1; self.join_confirm = None; if let Some(task) = self.join_task.take() { task.abort(); } self.session.join = JoinState::default(); self.join_transport = std::sync::Arc::new(std::sync::Mutex::new(None)); }` with a doc comment naming the invariant ("every entry point that switches the session out of a join must invalidate it, or a late NetJoinSealed materializes a republic the user never asked for"). Refactor cmd_join_cancel to `self.invalidate_join(); self.session.screen = Screen::Choice; self.emit_session(SessionScope::Full); Ok(Reply::Ack)` — behavior-identical plus the transport-slot reset.
4. 4. Call `self.invalidate_join();` in cmd_create_start directly beside `self.teardown_ritual();` (lifecycles.rs:695) — i.e. after guard_idle + the name/handle/threshold validation, before start_ritual, so a founding that fails to start still invalidates (matching the already-unconditional teardown_ritual).
5. 5. Call `self.invalidate_join();` in cmd_recover_start immediately before `self.recover_generation += 1;` (lifecycles.rs:1376) — after the link parse, the !persist check and the empty-phrase check, so an erroring RecoverStart changes nothing.
6. 6. Re-run the three tests; they must go green. Then run the whole engine suite (`cargo test -p molt-engine`) — no existing test sequences JoinStart before CreateStart/RecoverStart, so nothing should move.
7. 7. EXTENSION (same root cause, verified at HEAD — drop only if scope must stay literal to the backlog text): add red test 4, then call `self.invalidate_join();` in crates/molt-engine/src/session.rs::cmd_open_workspace after the WorkspaceEncrypted check (line 788) and before the already-open no-op branch (line 792), so both the reopen no-op and the full open invalidate.
8. 8. EXTENSION (the symmetric hole): add red test 5, then make cmd_recover_start abandon an in-flight FOUNDING the way cmd_join_start does — `self.teardown_ritual(); self.ritual_attestations.clear();` next to the invalidate_join() call from step 5. teardown alone closes it (maybe_finalize returns early once net_ritual is None); resetting session.create is optional cosmetics — decide once, and if you reset it, do it in the same commit as its assertion.
9. 9. `cargo clippy --all-targets -p molt-engine` clean, then commit; the change-set adds no Command variant, so molt-mcp needs no update.

### Files edited

- `crates/molt-engine/src/lifecycles.rs`
- `crates/molt-engine/tests/nostr_founding.rs`
- `crates/molt-engine/src/lib.rs`
- `crates/molt-engine/src/session.rs`

### Risks

- No new Command variant and no signature change → the co-equality test (molt-mcp tools()/INTERNAL list) is untouched. Confirm by not adding a variant.
- No byte layout is touched: roster_canonical_bytes / republic_id / checkpoint tags and their byte-pin tests are out of scope. If a fix idea starts reaching into SealedRoster, it is the wrong fix.
- clippy-at-zero including tests: use .expect("…") everywhere in the new tests (never .unwrap()), and keep the `if let Some(task) = … { task.abort(); }` shape (matches cmd_join_cancel, no clippy lint).
- CLAUDE.md's "drain the outbound path, don't abort()" rule is NOT violated here: cmd_join_cancel already aborts this exact task deliberately. The cost is the same as cancel — the abandoned join's last outbound frame (e.g. its seal signature) may be lost, which can strand the OTHER republic's founder. That founder-side story is cluster F.2 (abort frames / legible waits), not D; do not try to solve it by keeping the task alive.
- invalidate_join() resets session.join, so the surface loses the abandoned join wizard's log. Verified no existing test asserts join state after a CreateStart/RecoverStart/OpenWorkspace, but re-check nostr_founding.rs (s.join.* assertions at lines 143-145, 358, 420-421, 503) after the change.
- Unit-test trap: plain_state() drops its strong cmd_tx (State::new downgrades at lib.rs:774), so start_ritual's upgrade fails and cmd_create_start returns Err("engine stopped"). Any state-level founding test must hold a Sender clone AND set ritual_material_sink (manual mode) or ritual_sim, or it will pass for the wrong reason.
- Green-for-the-wrong-reason trap in the actor tests: if the injected roster's petra nostr_sk does not match the anchored nostr_pk (lifecycles.rs:1239-1248), or the workspace dir is unwritable, the seal fails anyway and the test is green at HEAD = inert. Watch it fail red first, and keep the pre-check that join.run.outcome == 0.
- Adding invalidate_join() to cmd_open_workspace also fires on the cmd_restore_finish path (lifecycles.rs:656) — intended (a restore is also a context switch), but it means a restore now kills an in-flight join; state it in the commit message.

## Cluster I — Invite relay cap — ✅ LANDED (`28456f7`)

*Kept for the record: this plan was written before `28456f7` landed. Read it as the reasoning behind that commit, not as work to do.*

**Verdict.** The defect still exists at HEAD, and it is worse than the backlog describes. `founding.rs:441` hands the founder's ENTIRE dialable pool to `RitualNet`; `nostr_ritual.rs:103` copies `net.relays()` verbatim into the `InviteHandoverV2`, and `InviteHandoverV2::encode` → `check_relays` (`invite.rs:169`, `invite.rs:220`) refuses at >8 — which `nostr_ritual.rs:113` converts into a FATAL `NetRitualFailed`, so a founder with 9+ dialable relays never gets a link and the founding aborts at once. The same list also feeds `WelcomePayload{relays}` (`founding.rs:1855`), whose `encode` enforces the identical cap (`welcome.rs:109`), so a link-only fix would merely move the failure to group birth — and it would ALSO break the joiner's `payload.relays != h.relays` equality check (`nostr_ritual.rs:511`), which demands the invite set and the Welcome set be byte-identical. Nothing caps `SessionSettings.relays` on the way in (`cmd_relay_add`, `session.rs:102`), so a 9-relay pool is reachable purely through the public Command surface. The fix therefore belongs at the single source: cap `relays` in `start_ritual` to the first `MAX_PAYLOAD_RELAYS` in pool (= priority) order, so invite, Welcome, `GroupChannel`, and the persisted `TransportState.relays` all agree.

### Anchors (verified at 88dd854 — RE-VERIFY, the tree has moved)

| file | line | symbol | why |
|---|---|---|---|
| `crates/molt-net/src/welcome.rs` | 46 | `MAX_PAYLOAD_RELAYS` | the 8-relay cap constant; the value the founder's set must be truncated to (already public, engine may reference it) |
| `crates/molt-net/src/welcome.rs` | 109 | `WelcomePayload::encode` | the SECOND enforcement of the same cap — a link-only fix still fails here at group birth |
| `crates/molt-net/src/invite.rs` | 220 | `InviteHandoverV2::check_relays` | the build-side refusal (`{n} relays — more than the 8 an invite may carry`) reached from encode(); stays strict, the caller must stop handing 9 |
| `crates/molt-net/src/invite.rs` | 169 | `InviteHandoverV2::encode` | calls check_relays on the BUILD path, which is what makes the founder's own pool the thing being rejected |
| `crates/molt-engine/src/founding.rs` | 441 | `State::start_ritual (let relays = molt_core::relay::dialable(...))` | THE fix site: the single place the founder's ritual relay set is computed; everything downstream clones it |
| `crates/molt-engine/src/founding.rs` | 477 | `NostrRitual { relays, .. }` | the same list stored as the group relay list (= invite relays at founding) |
| `crates/molt-engine/src/founding.rs` | 315 | `RitualState::transport_shape` | persists `n.relays` as TransportState.relays — capping at the source keeps founder and joiner byte-identical |
| `crates/molt-engine/src/founding.rs` | 1855 | `cmd_net_all_joined welcome fan-out (WelcomePayload { relays: relays.clone() })` | second consumer of the same list; hits welcome.rs:109 at >8 and reports as a fatal 'welcome did not publish' |
| `crates/molt-engine/src/nostr_ritual.rs` | 103 | `spawn_founder_inbox (relays: net.relays().to_vec())` | the untruncated copy into the invite handover |
| `crates/molt-engine/src/nostr_ritual.rs` | 113 | `spawn_founder_inbox (Err(e) => Command::NetRitualFailed)` | turns the render refusal into a FATAL founding failure — why the operator gets no link at all |
| `crates/molt-engine/src/nostr_ritual.rs` | 511 | `member_join (if payload.relays != h.relays)` | the joiner requires invite set == Welcome set — proves the cap must be applied ONCE, upstream of both |
| `crates/molt-engine/src/lifecycles.rs` | 713 | `cmd_create_start (self.session.create = CreateState { .. })` | CreateState is REPLACED after start_ritual returns, so a run-log note pushed inside start_ritual is wiped — the note must be applied here |
| `crates/molt-engine/src/lifecycles.rs` | 698 | `cmd_create_start (self.start_ritual(...))` | the ONLY caller of start_ritual — a return-type change ripples nowhere else |
| `crates/molt-engine/src/session.rs` | 102 | `State::cmd_relay_add` | no pool-size cap on ingest: a 9-entry confirmed pool is reachable purely through the Command surface |
| `crates/molt-core/src/relay.rs` | 415 | `molt_core::relay::dialable` | returns the dialable relays IN POOL (= priority) ORDER, so `.take(8)` is exactly 'the first 8 in priority order' |
| `crates/molt-net/src/dial.rs` | 274 | `Dialer::dial_host (.onion under Direct)` | an .onion under the direct dialer fails INSTANTLY — this is what makes onion pad-relays a hermetic, fast way to build a 9-relay pool in the test |
| `crates/molt-engine/tests/nostr_founding.rs` | 306 | `a_join_needs_only_one_relay_in_common_with_the_invite` | the exact template for the new test: MockRelay + one unreachable v3 onion in the founder pool, full CreateStart→JoinStart→propose→ratify→seal choreography |

### Red tests first

- **`a_founder_pool_over_the_link_cap_still_founds_over_its_first_eight_relays`** — in `crates/molt-engine/tests/nostr_founding.rs`, driven through **Command::RelayAdd + RelayConfirm + RelayClearnetSession (the existing `adopt_relay` helper) → Command::CreateStart on engine A → Command::JoinStart on engine B → Command::CreatePropose → Command::JoinConfirmCharter → Command::ReadSession/CloseWorkspace. No injected engine-internal commands, no test seam.**.
  - asserts: Setup: one `MockRelay::run()` adopted FIRST, then 8 syntactically valid but unreachable v3 onion relays (56 chars of [a-z2-7], generated by varying the last char of the existing 56-char literal) — 9 dialable entries in priority order. (1) a seat link appears and `molt_engine::FoundingInvite::parse` succeeds; (2) `inv.handover.relays.len() == 8` and equals the first 8 adopted URLs IN ORDER, and does not contain the 9th; (3) `s.create.run.log` contains a line naming that the invite carries 8 of the 9 dialable relays (the honesty note); (4) the whole choreography completes: founder `can_propose` → CreatePropose → joiner `awaiting_ratify` → JoinConfirmCharter → `create.run.outcome == 1` and joiner on `Screen::Main` — this half is what pins the WELCOME leg and the joiner's invite-set == Welcome-set equality check; (5) after CloseWorkspace the founder's persisted `TransportState.relays` is those same 8. Use a first wait predicate of `joinable link OR create.run.outcome == 2` so the red run fails in seconds with the real error rather than after the 30 s wait_for timeout.
  - red today because: At HEAD `start_ritual` (founding.rs:441) hands all 9 relays to `RitualNet`; `spawn_founder_inbox` (nostr_ritual.rs:103) copies them into `InviteHandoverV2`; `encode`→`check_relays` (invite.rs:169/220) returns `Framing("9 relays — more than the 8 an invite may carry")`; nostr_ritual.rs:113 makes that a FATAL `NetRitualFailed`. No seat link is ever surfaced, `create.run.outcome` goes to 2, and assertions (1)–(5) all fail. Assertion (4) additionally stays red against a naive link-only fix (WelcomePayload::encode, welcome.rs:109, would then refuse at group birth; and a differently-capped Welcome would trip nostr_ritual.rs:511 on the joiner).

### Fix steps

1. RED: add the test above to crates/molt-engine/tests/nostr_founding.rs. Run ONLY that test (`cargo test -p molt-engine --test nostr_founding <name>`) and confirm it fails with `rendering invite link: 9 relays — more than the 8 an invite may carry` in `create.run.log` — red for exactly that reason, not a timeout.
2. GREEN step 1 — cap at the source. In crates/molt-engine/src/founding.rs::start_ritual, replace the `let relays = molt_core::relay::dialable(...)` binding (line 441) with: bind the full dialable list, compute `let dropped = dialable.len().saturating_sub(molt_net::welcome::MAX_PAYLOAD_RELAYS);`, then `let relays: Vec<String> = dialable.into_iter().take(molt_net::welcome::MAX_PAYLOAD_RELAYS).collect();`. Comment WHY: an invite link and a Welcome payload are untrusted input at the far end and both cap at MAX_PAYLOAD_RELAYS, so cap what goes IN — in the pool's own priority order — instead of refusing to render the link; and cap it ONCE here because the joiner (nostr_ritual.rs:511) requires the invite set and the Welcome set to be identical. Do NOT touch invite.rs/welcome.rs: their build-side refusal stays as the fail-loud backstop and the decode-side check is untrusted-input enforcement.
3. GREEN step 2 — carry the honesty note out of start_ritual. `self.session.create` is wholly replaced in cmd_create_start (lifecycles.rs:713), so a log line pushed inside start_ritual is discarded. Change `start_ritual`'s return from `Result<Vec<String>, String>` to a small pub(crate) struct `RitualStart { links: Vec<String>, notes: Vec<String> }` (declared next to `NostrRitual` in founding.rs). When `dropped > 0`, push one note: `"→ this node's pool has {n} dialable relays; the invite and the Welcome carry the first {MAX} (the pool order is the priority — reorder in Settings to change which)"` using the run log's `→ ` tone prefix.
4. GREEN step 3 — apply the notes. In crates/molt-engine/src/lifecycles.rs::cmd_create_start, destructure the new return (`let RitualStart { links, notes } = self.start_ritual(...)`), build `seats` from `links` unchanged, and AFTER the `self.session.create = CreateState { .. }` assignment (line 713) do `self.session.create.run.log.extend(notes);` — before the existing `emit_session`. Verify no other caller of `start_ritual` exists (grep: lifecycles.rs:698 is the only one).
5. Run the new test green, then the neighbours that share this path: `cargo test -p molt-engine --test nostr_founding` and `cargo test -p molt-engine --test two_instances` (the loopback seam path takes the `else` branch and must be untouched), plus `cargo test -p molt-net invite`.
6. clippy + docs: `cargo clippy --all-targets` at zero (`.expect("…")` only in the new test, no `.unwrap()`, no index panics when generating the 8 onion URLs). Then update docs/transport/nostr_n4_plan.md §3 (the `≤8 relays` bullet, line ~168) with one sentence: the founder takes the first MAX_PAYLOAD_RELAYS of `relay::dialable` in priority order, and the SAME capped list feeds invite, Welcome, group channel and TransportState. Tick cluster I in docs/transport/nostr_n4a_review_followups.md.
7. One commit on master, code-review the diff, land green.

### Files edited

- `crates/molt-engine/src/founding.rs`
- `crates/molt-engine/src/lifecycles.rs`
- `crates/molt-engine/tests/nostr_founding.rs`
- `docs/transport/nostr_n4_plan.md`
- `docs/transport/nostr_n4a_review_followups.md`

### Risks

- The joiner hard-requires `payload.relays == h.relays` (nostr_ritual.rs:511). Capping in two places (once for the link, once for the Welcome) would break EVERY join over a >8 pool. The cap must be applied exactly once, at founding.rs:441, upstream of both.
- The founder now also SUBSCRIBES/publishes on only the first 8. If an operator's top 8 are dead and the 9th is the live one, the founding fails — mitigated by the run-log note pointing at the pool order (relay_pool.md: the order IS the priority). This is a behavior change, not just a rendering change; state it in the commit message.
- `start_ritual`'s return type changes. Verified single call site (lifecycles.rs:698), but re-grep before editing — a missed caller is a compile error, not a silent break.
- Test-runtime trap: pad the founder pool with unreachable v3 ONION relays (instant fail-closed refusal, dial.rs:274), never with clearnet/`.invalid` hostnames — a slow resolver would stall every publish to PUBLISH_TIMEOUT and make the test flaky/slow.
- No byte-layout ripple: roster_canonical_bytes / republic_id / checkpoint tags are untouched; `TransportState.relays` is an existing field (its value simply becomes the capped list, identical on both ends).
- No new `Command` variant → the co-equality test is untouched. No `WorkspaceEvent` change → additive-only rule not engaged.
- run.log assertions across the suite are all `.iter().any(...)`, none index- or length-based (checked), so adding a line breaks nothing — keep it that way.
- N4b's recovery link (nostr_n4_plan.md §346) will carry relays too. When that lands it must reuse this capped set (or the same `.take(MAX_PAYLOAD_RELAYS)` rule) — if a second production site appears, extract a named helper next to MAX_PAYLOAD_RELAYS rather than duplicating the truncation.

---

# Collision analysis

Read the collision-critical regions at HEAD (88dd854). Verified: `INTERNAL: [&str; 45]`, the garbled refusal strings (founding.rs:2005/2015), the three-way crowding of `cmd_create_start`, and the two competing clock seams in `ritual_net.rs`.

## 1. File-conflict matrix

`R` = same function/region (real), `b` = same file, different region (benign), `t` = shared test file (append-only, benign).

| file | C | D | E | F | G | H | I |
|---|---|---|---|---|---|---|---|
| `molt-net/src/ritual_net.rs` | **R** `publish`179, `publish_frame`347 | – | **R** `GroupSub::recv/live`396-425 | – | **R** `publish`176, `inbox`214, `subscribe_tags`367, `live`240/396 | **R** `subscribe`356 | – |
| `molt-net/src/relay_runtime.rs` | – | – | – | – | R (sole owner) | – | – |
| `molt-net/src/invite.rs` | – | – | – | R `Aborted` variant | – | – | b (anchor only) |
| `molt-engine/src/nostr_ritual.rs` | **R** `spawn_publish_frame`176-209 | – | **R** recv sites 345/532/589 | **R** member_join 456/475/492/531/589 | **R** live sites 95/330/431/526 | b (mod tests) | b `spawn_founder_inbox`103 |
| `molt-engine/src/founding.rs` | **R** `maybe_seal`2374, new handler | – | R `cmd_net_ritual_note` next to 1628 | **R** `teardown_ritual`549, spent arm 1926-1975, 1644 | – | **R** strings 2005/2015, mod tests 2931 | **R** `start_ritual`441 |
| `molt-engine/src/lifecycles.rs` | **R** `finalize_founding`938-956 | **R** `cmd_create_start`695, `cmd_recover_start`1376, `cmd_join_cancel` | R `cmd_net_join_note` | **R** `materialize_workspace`70, `cmd_create_start`695, `cmd_create_cancel`749, `cmd_join_start`1038 | – | – | **R** `cmd_create_start`698/713 |
| `molt-core/src/lib.rs` | **R** Command enum | – | **R** Command enum | b `JoinState` | – | – | – |
| `molt-mcp/src/lib.rs` | **R** INTERNAL 45→46 | – | **R** INTERNAL 45→47 | – | – | – | – |
| `molt-engine/src/lib.rs` | **R** dispatch @1266 | b `join_task`, mod tests | **R** dispatch @1266 | b | – | – | – |
| `molt-engine/src/session.rs` | – | R `cmd_open_workspace`781 | – | R `cmd_open_workspace`872, `cmd_select_screen`44 | – | – | – |
| `molt-engine/src/net.rs` | – | – | – | R `cmd_net_presence_tick` | – | – | – |
| `tests/nostr_founding.rs` | t | t | – | t | t | b (helpers copied) | t |
| `molt-net/tests/nostr_ritual_net.rs` | **R** call sites 170/196 | – | – | – | **R** new tests | **R** new keystone | – |

**Hottest three points:** `lifecycles.rs::cmd_create_start` lines 694–713 (D + F + I, all within 20 lines); `nostr_ritual.rs::member_join` 456–620 (C + E + F + G all edit the same three wait loops); `molt-core Command` + `molt-mcp INTERNAL` (C + E, mutually exclusive array lengths).

## 2. Recommended implementation order

| # | cluster | why here |
|---|---|---|
| 1 | **I** | Smallest, zero open questions, and it's a total founding-abort at 9+ relays — fix `cmd_create_start`/`start_ritual` while that region is still clean. |
| 2 | **D** | Correctness (a late `NetJoinSealed` materializes a foreign workspace); engine-only, no open questions; takes `cmd_create_start` second while it's still only I's edit. |
| 3 | **H-strings only** (founding.rs:2005/2015 `\` continuation) | 2-line real defect in operator-visible text; must land *before* F rewrites the same arm and before any test asserts those substrings. |
| 4 | **C-1** (molt-net half: `RitualNet::publish`/`send_ritual`/`send_welcome`/`publish_frame` return the `PublishReport`) | Pure signature seam. Everything downstream (E, G, H tests) compiles against it once — moving it later means re-touching four test files. |
| 5 | **G** | Answers the AUTH-identity fork and turns "connected but never readable" into a real refusal; introduces `SyncState`/`live_state`, the seam E's `recv` rework must sit on. Correctness before E's coverage. |
| 6 | **E** | Rebuilds `GroupSub::recv` (+ backoff, `GroupRecv::Deaf`) on top of G's `live_state`, and lands **the** window-clock test seam that H4 also wants. |
| 7 | **C-2** (engine half: publish reporting, genesis retry, notice) | Needs the genesis product decision *and* must reuse E's note command rather than mint a second one. |
| 8 | **F** | Largest blast radius (7 src files); lands last onto now-stable `member_join` loops and the already-crowded `cmd_create_start`. F1 (`net_health` in `materialize_workspace`) is separable and can go at position 1b. |
| 9 | **H-rest** | Coverage by definition, and it pins seams that 4–8 all move. Rewrite H4 against E's clock seam; drop `subscribe_at`. |

## 3. Contradictions / redundancies

1. **E vs G — is an unreadable subscription fatal?** G step 6/9 makes `synced == 0` a hard `NetRitualFailed`/`NetJoinFailed`; E's whole design is "deaf is loud, never terminal" and its open question asks exactly this. Same condition, opposite verdicts, distinguished only by *when* it happens (at placement vs. at roll). Decide once: recommend G's rule at placement (nothing has started yet, cheap to refuse) + E's rule after placement (a one-shot founding must not die on a blip). Write that sentence into both plans.
2. **E vs H — two clock seams for one thing.** E adds a process-global `WINDOW_CLOCK_SHIFT` covering `publish_frame`/`subscribe`/`recv`; H adds `GroupChannel::subscribe_at`. Ship E's (production keeps calling `subscribe()`; H's seam is a public API production never calls — a mildly inert keystone). **H red-test 5 becomes redundant** — E's red test 1 already drives the roll through fail → backoff → heal on the real code path.
3. **C vs E — two internal note commands.** C adds `NetRitualPublished` (INTERNAL 45→46); E adds `NetRitualNote` + `NetJoinNote` (45→47). C's branch (c) ("landed on 1 of 2 relays" as a ⚠ log line) *is* E's `NetRitualNote`. Land E's pair; C reports through it and only adds `NetRitualPublished` if the structured `accepted`/`failed` fields are genuinely needed. Otherwise the INTERNAL array and the dispatch block conflict every rebase.
4. **F vs E — two mechanisms writing the join run log.** F2-g's `refresh_join_wait` (actor-side, no Command) and E's `NetJoinNote` (task-side Command) both append/rewrite a trailing line and both claim the `cmd_net_join_accepted` last-line dedup. Land E first and have F's elapsed line go through the same dedup helper, or they will stack lines against each other.
5. **C vs F on the same loop.** C's red tests 3/4 assume the member's genesis wait at `nostr_ritual.rs:589` stays an unbounded loop that eventually receives; F2-e adds an `Aborted` arm and F2-f a deadline surface to that same loop. Not contradictory, but they must not be written by two agents in parallel.
6. **Nobody covers the mirror of D.** D aborts the abandoned `join_task`; F2 gives the *founder* an abort frame. Neither tells the *other republic's founder* that its joiner walked away — it sits in the same unbounded wait D's own risk note flags. That's a real hole in the union of these seven plans, not in any one of them.

## 4. Genuinely concurrent-safe (isolated worktrees)

- **D ∥ G** — disjoint `src/` (D: lifecycles/session/lib; G: relay_runtime/ritual_net/nostr_ritual/relay_runtime). Only `tests/nostr_founding.rs` is shared, append-only at the file end.
- **I ∥ G** — same story; I's `founding.rs` edit is `start_ritual:441`, which G only reads.
- **D ∥ I is NOT safe** — both edit `cmd_create_start` lines 694–713.
- **C-1 (molt-net only) ∥ D** — safe; C-1's engine touch is deferred to C-2.
- Everything else in `{C-2, E, F, G, H}` shares `nostr_ritual.rs` and/or `ritual_net.rs` at function granularity — serialize them.

Max safe parallelism: **two lanes** — lane A `I → D → F`, lane B `C-1 → G → E → C-2`, converge, then H.

## 5. Weak plans

- **H (weakest as scheduled work).** 3 of 5 tests are green at HEAD; they're deletion-experiment pins, not TDD-red. H2b's assertion is coupled to a refusal *message* (because `lifecycles.rs:1185` refuses anyway) — one reword makes it red, one loosening makes it inert forever. H4 drives `subscribe_at`, an API production never calls. H's real value is its two audit findings, which belong elsewhere: the garbled strings (→ order position 3) and the **unsigned `SealedRoster.roster`** gap, which is a genuine security question and must not be decided inside a coverage change-set.
- **C.** Its central open question (genesis retry vs. `GenesisAck`) *blocks* fix step 6 and red tests 3/4 — it cannot start. Step 8 (a molt-ui toast) is scope creep into a different crate for a cluster about molt-net reporting. Test 2 asserts a literal `"1 of 2"` string — brittle. Also verify `nostr-relay-builder 0.44.1` actually exposes `write_policy`/`QueryPolicy` before committing three tests to it (G depends on the same double).
- **F.** Correctly demolishes the backlog's own F3 diagnosis (a retry mints a fresh phrase, so `same` is *supposed* to be false) — but its replacement loosens single-use in the pre-birth window on an **unresolved product fork** (post-birth retry = full re-mint?). F should be split: F1 (`net_health`, 5 lines, no questions, land at position 1b) / F2 (abort frame + legible waits) / F3 (re-anchor, blocked on the user). Shipping F as one unit makes a trivial honesty fix hostage to a governance decision.
- **G.** Technically the sharpest plan, but its open question is the only **irreversible** one in the set: authenticating the kind-445 subscription with the roster anchor permanently hands every relay operator the anchor→group-id link, and it survives into N5. Do not let this get guessed during implementation — it needs an explicit yes before step 3.
- **E.** Sound, but red test 2 (engine-level, proxy cut + clock shift + 30 s predicates) is the most flake-prone test in the whole set; keep it, but expect to stabilize it. Its open question is answered by G — resolve them together.
- **D, I.** No weaknesses found. Both have a single specific red test that fails for exactly the stated reason. Start here.