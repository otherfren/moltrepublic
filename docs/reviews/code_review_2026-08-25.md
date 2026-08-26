# Code review 2026-08-25 (security, correctness, style, refactoring)

Status: **OPEN WORK** for the items marked OPEN below; every item marked
FIXED landed on master the same night (`2714d814` core patch parser,
`584125ec` outbox timer, `7ccdbd15` governance + wire ingest + MLS
authority, `919ad6bc` roster verifiers, `83d72c39` Tor lanes + dedup ring,
`7e350909` storage seal, `ecf9b640` MCP schema / config mode / diff,
`5a421487` the follow-ups a second review pass over that diff raised). The
review covered every crate: eight independent read-only passes (one per
area), each finding verified against the code before it was accepted, the
CRITICAL/HIGH ones reproduced by a red test before the fix. Items already
listed in `docs_archive/reviews/total_review.md` (L1-L15) or
`docs/reviews/known_debt.md` are not repeated.

Severity: CRITICAL = threshold/authorization bypass · HIGH = a single
insider or planted file breaks a security promise or availability · MEDIUM
= correctness/robustness defect with a concrete trigger · LOW = bounded
nuisance or latent · STYLE / REFACTOR = the repo's own rules.

An OPEN item leaves this document in the change that closes it; when the
last one is gone the document moves to `docs_archive/reviews/`.

---

## 1. Governance chain (`molt-engine/src/chain.rs`)

### C1 [CRITICAL] A forged wire `Approved{by: victim}` became the victim's real signature at the next re-base - FIXED
`rebase_pending_approvals` inferred "this node approved X" from the
wire-collected `pending_sigs`, which any member fills with junk under any
roster name (`collect_sig` never verified). After any block sealed, every
node re-signed X with its genuine key: a threshold bypass by one insider,
no human decision, `approved_by_me` showing `false` meanwhile.
Fix: the decision register `State::own_approvals`, written only by the own
signing path, cleared by the own D2 decline retraction and by a withdraw,
rebuilt from an own `Approved` in the own log at replay (a restart); the
re-base reads it and nothing else; `collect_sig` never holds an
unverified signature under the own name.
Tests: `a_forged_own_approval_is_not_re_signed_at_the_rebase`,
`a_declined_own_approval_is_not_re_signed_at_the_rebase`,
`the_own_log_rebuilds_the_decision_register`.

### C2 [HIGH] Unverified "latest wins" evicted verified signatures - FIXED
A later junk signature under a roster name replaced an earlier VERIFIED
one; one insider could stall every vote forever (and evict a member's own
genuine signature on its own node). Fix: verify-before-replace in
`collect_sig` (a held verified signature is only replaced by one that
verifies). Test: `junk_does_not_evict_a_verified_signature`.

### C3 [MEDIUM] `ChainRequest` is an unbounded amplifier - OPEN
`chain.rs` `serve_chain_from` / `serve_open_governance`, `net.rs` arm:
every roster member makes every other member record the whole chain + blob
+ open cards per frame, into the durable log. Fix direction: per-requester
debounce keyed `(from, from_height)`, cap `from_height` to the anchor
unless the blob path applies, skip when `from_height > head`; consider a
non-logged direct serve.

### C4 [MEDIUM] Byte quota prunes OTHER nodes' backups in a shared bucket - OPEN
`backup.rs` `enforce_quota` / `quota_candidates` count every parseable key
bucket-wide; the doc and GUI say "this node's backups". Fix direction:
restrict candidates to this node's workspace ids (pass the set into the
task) or namespace keys per node; or correct the doc/GUI text if
bucket-wide is intended (product call).

### C5 [LOW] `tie_break` re-walks the chain before verifying the incoming block - OPEN
`chain.rs` `tie_break`: a ground low-hash block at tip height costs a full
walk per frame. Fix direction: `verify_next` the incoming block alone
first.

### C6 [LOW] Dead headless-genesis adoption adopts ANY valid genesis - OPEN
`receive_block` with `chain_head == None` → `adopt_chain` without a
republic-id pin; unreachable from the wire today. Fix direction: delete
the branch + its test, or pin `adopt_chain` to `self.republic_id()`.

### C7 [LOW] `MembershipOp::Joined` + a checkpoint bricks pruned holders - OPEN
`walk_suffix_chain` requires every roster entry in the founding table; a
`Joined` seat is not. No production producer (product decision: no seat
adding). Fix direction: `verify_next` hard-rejects `Joined`, or the
variant is documented reserved and its producers deleted.

### C8 [LOW] Restore anchor-uniqueness gates check founding anchors only - OPEN
`auto_approve_restore` and the coordinator ingest compare against
`identities` only; `anchor_seen_in_chain` already exists. Fix direction:
use it in both gates.

### C9 [STYLE] Em dashes / prose in strings reaching the UI - OPEN
`backup.rs:30` (`SEALED_SKIP`), `backup.rs` export-cap and chainless
messages, `chain.rs` consent-twice and no-previous-exporter errors; prose
log lines `chain.rs` (seal held back, tie-break, checkpoint arms). Plain
`-`, one clause, structured fields.

### C10 [REFACTOR] `chain.rs` (10k lines) split + duplicates - OPEN
Proposed modules: `chain/verify.rs` (pure verification, 1-1238),
`chain/projection.rs`, `chain/governance.rs`, `chain/membership.rs`,
`chain/checkpoint.rs`, `chain/sync.rs`, tests per module with a shared
`test_support`. Duplicates to fold: the replay guard
(`auto_approve_restore` vs `receive_checkpoint_proposal`), the
roster-adopt block (`project_one` vs `apply_chain_to_state`), the
bookkeeping-removal loop, "settle consumed ids", the double
`id_free_for` check; a checkpoint seal writes the chain three times.
Stale docs: `working_nostr_pk` / `serve_chain_anchor` "no production
caller yet" (both called), misplaced doc comments at the top of the file.

## 2. Engine wire ingest and state (`net.rs`, `events.rs`, `proposals.rs`, `session.rs`, `chat.rs`, `transfer.rs`, `configstore.rs`)

### E1 [HIGH] Wire `MembershipProposed` was recorded and applied BEFORE its gates - FIXED
One `id = u64::MAX - 1` set `next_id = u64::MAX` on every node (every
further proposal silently vanished) and every frame persisted a phantom
card. Fix: `admits_membership_proposal` (plausible id, pending cap,
`id_free_for`) runs before `record`; the applier bumps `next_id` only
inside the wire id window. Test:
`a_membership_proposal_with_an_implausible_id_is_not_recorded`.
Residual (LOW, OPEN): a blob-seeded rejoiner that logged a wire
membership id far above its snapshot's `next_id` and crashed before the
next snapshot replays the tail with the gate closed, while `adopt_chain`
raises `next_id` only afterwards - its next local mint can collide. Fix
direction: adopt the chain (or `bump_next_id_past_chain`) BEFORE the tail
replay, or widen the replay gate to `max(next_id, chain top) + window`.

### E2 [HIGH] Wire `Chat.ts` passthrough + `ts + retention` overflow panicked the actor - FIXED
`uploads_view` added the retention to a peer-chosen stamp; with release
overflow checks a `u64::MAX` stamp took the actor down, persisted, on
every reopen. Fix: the wire arm clamps the stamp to
`now + WIRE_STAMP_SKEW_SECS` (0 → now); `expires_ts` saturates. Test:
`a_wire_chat_with_a_hostile_stamp_never_panics_the_uploads_view`.

### E3 [MEDIUM] Wire `Chat` carried `reactions` / `read_by` / `deleted_by` - FIXED
Forged stances attributed to OTHER members inside a fresh message body.
Fix: the wire arm clears all three (they travel as their own
link-authenticated events). Test:
`a_wire_chat_carries_no_foreign_reactions_receipts_or_tombstone`.
Residual (LOW, OPEN): `kind: System` legitimately crosses the wire (the
recovery rejoin notice), so an insider can still dress a message as a
system line - cosmetic. Fix direction: a wire system line must carry the
deterministic id its content derives, verifiable at ingest.

### E4 [MEDIUM] `configstore::acquire_lock` decides liveness by `/proc/<pid>` - OPEN
PID reuse after a reboot refuses startup with a false "another moltd
runs". Fix direction: `rustix::fs::flock` (already used for the workspace
LOCK), PID as diagnostic only.

### E5 [LOW] `reset_workspace_state` misses five chain projections - OPEN
`chain_applied_sigs`, `chain_anchors`, `chain_member_relays`,
`split_noted`, `last_group_ack` bleed into a chainless workspace opened
next. Fix direction: clear them, or (R2) move the chain projection into
one struct reset with `Default`.

### E6 [LOW] `ChatRead` parks every unknown id unbounded per frame - OPEN
One frame with 300 random ids sweeps the whole P6 parking buffer. Fix
direction: per-frame cap, or a separate small ring for read refs.

### E7 [STYLE] Prose / em dashes in engine strings the GUI renders - OPEN
`session.rs` (`NOSTR_RUNTIME_PENDING`, relay-confirm refusals, offline
health reasons), `lib.rs` (`LEGACY_RECOVERY_LINK`), `proposals.rs`,
`chat.rs`, `transfer.rs`, `net.rs:2027`.

### E8 [REFACTOR] `lib.rs` / `net.rs` structure - OPEN
`lib.rs`: 4,150 of 5,835 lines are tests → `src/tests/*`; `State` (90
fields) → `DeliveryState` / `PresenceState` / `FilePlane` /
`ChainProjection` / `RecoveryState` sub-structs (closes E5 structurally);
`spawn_actor`'s 13 positional args → a `SpawnSeams` struct. `net.rs`: demo
mesh, `ParkedRefs` + `wire_*` (→ `chat.rs`), delivery ticks, ingest, file
plane (→ `transfer.rs`), recovery (→ `recovery.rs`), presence (`presence_of`
vs `refresh_member_pills` duplicate); ~1,800 lines of loopback-only mesh
code behind a feature/module. `transfer.rs` landing sequence three times.

## 3. Founding ritual and recovery (`founding.rs`, `nostr_ritual.rs`, `recovery.rs`, `lifecycles.rs`, `relay_msg.rs`)

### R1 [MEDIUM] Member-side roster verifiers weaker than `verify_genesis` - PARTLY FIXED
`verify_sealed_roster` / `verify_seal_proposal` now enforce distinct
attestation signers, `identities.len() == rule_n` and `1 <= m <= n`
(`check_rule_shape`). Test:
`verify_sealed_roster_refuses_duplicate_signers_and_a_lying_n`.
OPEN half: `materialize_workspace` still rewrites `rule_n` from
`roster.len()` and writes the workspace before `adopt_chain` verifies the
genesis. Fix direction: use `sealed.rule_n`, run `verify_own` on the built
genesis chain BEFORE `create_workspace`, fail the run on error.

### R2 [MEDIUM] Founder accepts `Signed` / `BackupConfirmed` before the table is frozen - OPEN
`cmd_net_seal_signed` / `apply_backup_attestation` have no
`charter_proposed` gate; `finalize_founding` never runs
`verify_sealed_roster`. Fix direction: gate both on `charter_proposed`
(+ `seal_published` on Nostr), re-anchoring gate `!charter_proposed`,
`verify_sealed_roster` in `finalize_founding` with outcome 2 on error.

### R3 [MEDIUM] Ticketed recovery lane has no one-re-admission-at-a-time gate - OPEN
A same-coordinator re-mint while the first `Restored` is pending strands
the seat (re-key with KP #2, Welcome to anchor #1). Fix direction: apply
the pending-`Restored` gate to the ticketed lane (`RecoverRefused`), or
supersede deterministically.

### R4 [LOW] A single survivor can capture a reattaching seat into a private group - OPEN (design)
The Welcome's group and rotation seed are not bound to the chain. Fix
direction: require a threshold-sealed `Restored` block naming our new
anchor before flipping to `recovered:`; document the threat model in
`detached_reattach.md`.

### R5 [LOW] Far-end text chooses the joiner's headline - OPEN
`relay_msg.rs` `ritual_headline` matches "did not publish" before the
own-phrase arms. Fix direction: `starts_with` on the anchored own phrases.

### R6 [LOW] Unauthenticated `JoinRequest`s write attacker text into the founder's run log; the handle is never validated - OPEN
Fix direction: log after `verify_join_mac`; validate the handle
(non-empty, <= 64 chars, no control chars) at ingest and in
`cmd_join_start` / `cmd_create_start`.

### R7 [LOW] Rejoiner buffers `Committed` blocks unbounded, re-verifies per frame - OPEN
Fix direction: cap 256, height window from the first served frame,
verify once per new consecutive run.

### R8 [LOW] A recovery ticket is not bound to the member it was minted for - OPEN
Fix direction: register `(ticket, member)`, refuse a mismatch.

### R9 [LOW] Phrase / `nostr_sk` transit as plain `String` / `Vec` - OPEN
Fix direction: `Zeroizing` in `JoinCtx` / `RecoverCtx` and the two
commands, a redacting `Debug` on `Command`.

### R10 [STYLE] ~100 em dashes in run-log producers and the E5 shape list - OPEN
Must change together with `known_log_shapes()` (the GUI localizes by
shape). Stale docs: `lifecycles.rs:3-7`, `recovery.rs:11`, the misplaced
`cfg_attr` on `recover_command`.

### R11 [REFACTOR] The member state machine exists twice and has diverged - OPEN
`run_ritual_member` vs `member_join`; a `RitualLeg` trait with one
ladder. `cmd_net_join_requested` 450 lines with eight refusal tails
(`refuse_join`); single-variant `RitualTransport`; loopback-only mesh
bootstrap; four copies of the framed-message reader.

## 4. MLS and delivery guarantee (`molt-net/src/mls.rs`, `supervisor.rs`, `group_runtime.rs`, `file_plane.rs`)

### M1 [HIGH] Any current leaf could re-key any seat - FIXED
`decrypt_at` merged every well-formed commit; `restore_member` never
checked the new KeyPackage against the roster. Fix: `MlsMember::set_roster_keys`
(the chain's identity table, armed by the engine at every runtime build)
- a leaf added for a member under any other signature key is refused
before the merge, epoch untouched. Test:
`an_added_leaf_must_carry_the_anchored_identity_key`.
Residual (OPEN): a legacy workspace without a chain arms no authority
(unchecked, as before). The `ChainOracle` seam (remove+add backed by a
threshold `Restored` block) is still unwired.

### M2 [HIGH] An evicted device undid its eviction with a back-dated same-epoch commit - PARTLY FIXED
The prior slot now records which leaves the merged commit removed; a
rewind onto a commit from one of them is refused (`rewind_forbidden`).
Test: `an_evicted_leaf_cannot_undo_its_eviction_with_a_back_dated_commit`.
OPEN half: the REVERSE order - the evicted device's low-key commit
arriving before the re-key makes the re-key `CommitSuperseded`; the
recovery has to retry and the device can repeat. Fix direction: the
tiebreak ranks a chain-backed commit (one an armed oracle authorizes)
above an unbacked one before comparing `CommitKey`s.

### M3 [MEDIUM] The prior slot is not persisted - OPEN
Two nodes decide a late same-epoch commit differently depending on a
restart in between - a silent fork among survivors. Fix direction:
persist `(prior_epoch, snapshot, CommitKey, removed)` in the MLS snapshot
(v4, additive).

### M4 [MEDIUM] Stall clock spun once the hourly budget was spent - FIXED
The spent branch never re-anchored `stalled_since`. Fix: re-anchor like a
granted round.

### M5 [MEDIUM] `retry_epoch_hold` counts frames lost that the same pass would still open - OPEN
Fix direction: keep `Opaque` frames in `still` while the pass made
progress; count lost only in the terminating no-progress pass (the
counter is the self-heal trigger).

### M6 [MEDIUM] The held-frame timer retried a PERMANENT refusal forever - FIXED
Separate `held_permanent` flag (no timer, and no stall clock either: a
permanent hold above a proven floor would otherwise be rewound and
re-failed every budgeted round), own `held_backoff_secs` (the stall clock
keeps its own), no sleep after the last publish attempt. Test:
`a_permanent_refusal_does_not_arm_the_held_frame_timer` (the floor-less
case; the floor case rides the same flag).

### M7 [MEDIUM] `TransportState` load-modify-save races between the group tasks - OPEN
Inbox, outbox and file plane each `load → mutate → save(whole)`; a claim
sheet or a consumed budget round can be lost. Fix direction: one shared
`Arc<Mutex<TransportState>>` as the mesh supervisor has, store as
write-behind.

### M8 [MEDIUM] §7 deep-laggard commit resends are not wired on Nostr - OPEN
The Nostr re-key arm publishes once and records nothing, so the outbox
never re-offers a commit; `detached_reattach.md` §7 overclaims. Fix
direction: record the commit with its pinned stamp and publish
`MlsCommit`s via `publish_frame_at(stamp)`; or downgrade the doc.

### M9 [LOW] `Stopped` mid-batch discards cursor progress - OPEN
Persist `published_through` before returning on `Stopped`.

### M10 [LOW] Shape-malformed 445s are held as if they might open later - OPEN
Match `EnvelopeError::Shape` → drop; hold only `EpochOpaque`.

### M11 [STYLE] Em dashes reaching the health surface; prose logs - OPEN
`group_runtime.rs` "not acknowledging deliveries — still resending",
`supervisor.rs:966, 1374`, `file_plane.rs:286`; prose warns
`supervisor.rs:1286, 1632, 300-303`.

### M12 [REFACTOR] - OPEN
`retry_epoch_hold` vs `drain_epoch_buffer` (one generic
progress loop); `outbox_loop` (270 lines, four concerns) → publish pass +
stall decision + a pure `Backoff`; `decode_at` tag table; dead in
production: `ChainOracle`, `MlsDecode::Ack` on 445, the plaintext
`WireFrame` path, `MlsChannel.cache` / `evict_*`.

## 5. Relay transport (`relay_runtime.rs`, `relay_ws.rs`, `dial.rs`, `nostr.rs`, `envelope.rs`, `ritual_net.rs`, `s3/`)

### T1 [HIGH] Tor stream isolation was per host only - FIXED
The anchor-authenticated inbox, the throwaway-key 445 subscription, the
unauthenticated publish channel and every other republic shared one
circuit per relay; an onion relay operator links them by circuit id.
Fix: `Dialer::isolated(lane)` (SOCKS credential / arti token per lane and
host); lanes: `anchor:<pk>` (RitualNet), `group:<seed>` (subscriptions),
`publish:<random>` per `GroupChannel`, `s3`, `probe`. Test:
`two_lanes_to_one_host_yield_distinct_socks_credentials`.

### T2 [MEDIUM] `PublishPool`: one stalled relay caps the node at ~1 event / 20 s - OPEN
`publish()` waits for the slowest relay; nothing remembers a timeout. Fix
direction: per-relay `Down{until}` backoff in the pool, report
`failed("backing off")` immediately, resolve when the live relays
answered.

### T3 [MEDIUM] `DEDUP_CAP` (4096) < history bound (5000) - FIXED
The fan-in channel's size argument assumed the ring covers a whole
replay. Fix: the ring is sized `max(history_bound + 64, DEDUP_CAP)`.
Test: `the_dedup_ring_covers_the_history_bound`.

### T4 [LOW] A relay dropped for over-replaying stays `Up` in `health()` - OPEN
`deaf()` then reports a live relay with no supervisor. Fix direction: set
`Down` before the bound `Break`, distinct log.

### T5 [LOW] `place_req` narrows a multi-window catch-up on reconnect - OPEN (latent)
`since = cursor - 48 h` overrides the caller's window; `subscribe_since`
has no production caller. Fix direction: a `resume` flag on `subscribe`.

### T6 [LOW] Relay-supplied strings unbounded into `NetError` / WARN - OPEN
Fix direction: one `relay_reason()` helper (cap ~200 chars, strip
control chars) at the four sites.

### T7 [LOW] S3 endpoint host not lowercased / ASCII-checked - OPEN
`.ONION` bypasses the Direct-dial onion refusal; a long non-ASCII host
can panic `truncate(255)`. Fix direction: parse with `url::Url` (or
lowercase + ASCII refusal); `eq_ignore_ascii_case` in the dialer; refuse
plaintext `http://` to clearnet like the relay policy.

### T8 [LOW] `S3Config` derives `Debug` with the secret - OPEN
Manual `Debug` eliding `secret_key`.

### T9 [STYLE] Em dashes in `TorTest.detail`, `S3Error` hints, `NetError::Framing` - OPEN
`tor_probe.rs`, `s3/mod.rs`, `s3/http.rs`, `dial.rs:303`,
`relay_runtime.rs` publish gate; `molt-ui` rewrites them per string.

### T10 [REFACTOR] - OPEN
`publish_one` duplicates `send_and_await_ok`'s verdict; URL→(host, port)
three times (the NIP-11 copy lacks the IPv6 bracket trim);
`with_cursors` / `cursors()` / `subscribe_since` have only test callers
and a stale doc.

## 6. Storage (`molt-storage`)

### S1 [HIGH] `manifest.crypto.key_file` and symlinked key/logo files steered the seal's zero-fill + unlink outside the workspace - FIXED
Fix: `read_manifest` refuses any `key_file` other than
`molt_core::DEFAULT_KEY_FILE` (an imported blob's cover sheet inherits the
gate); `secure_remove` removes a symlink AS a link and opens with
`O_NOFOLLOW`; a symlinked `keys` directory refuses the seal outright
(`O_NOFOLLOW` guards the last component only). Tests:
`a_manifest_naming_a_foreign_key_file_is_refused_untouched`,
`sealing_never_follows_a_planted_symlink`,
`sealing_refuses_a_symlinked_keys_dir`.
Residual (LOW, OPEN): other ancestors are not checked; the complete
answer is `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on a dirfd
of the workspace (rustix is in the tree).

### S2 [MEDIUM] `migrate_to_segment_keys` resurrects a key-erased segment - OPEN
A dropped segment whose file reappears gets a fresh DEK and a bogus
`first_seq`; the next open fails AEAD. Fix direction: skip (and unlink)
keyless segments below the lowest keyed one / with last seq <= floor; a
compaction error is not the fatal flag.

### S3 [MEDIUM] `has_valid_frame_after` is quadratic in CRC work - OPEN
A planted tail of `0x02` turns open into hours under the LOCK. Fix
direction: budget the scan (e.g. 4x `data.len()`), classify as `Corrupt`
past it.

### S4 [MEDIUM] The PRUNED version gate is lowered by `unseal_at_rest` and never raised by the F6 migration - OPEN
Fix direction: `version_floor` returns PRUNED when `log/keys.state`
exists; bump right after `migrate_to_segment_keys`.

### S5 [MEDIUM] Segment / snapshot numbers from file names are unbounded - OPEN
`18446744073709551615.msnap` panics `open_workspace` (`at_seq + 1`); a
`u64::MAX` `.mlog` becomes the active segment under the snapshot AAD
marker. Fix direction: reject numbers >= `KEYS_SEGMENT` in the callers
of `list_sorted`, `saturating_add`, refuse an empty last segment that is
not the only one.

### S6 [LOW] The export carries `prefs.toml` `shared_files` absolute paths - OPEN
Strip in `commit_inner` next to `last_backup`.

### S7 [LOW] Key hygiene inconsistent with `segkeys.rs`'s promise - OPEN
`SegmentKey` derives `Debug`; `dek()` hands out unzeroized copies;
`OpenedWorkspace.key` plain. `Zeroizing` + manual `Debug`.

### S8 [LOW] `write_atomic` reuses a pre-existing tmp file; import stages state files at 0644 - OPEN
`create_new` after unlinking the tmp path; `write_atomic(.., true)` in
the staging loop.

### S9 [LOW] `PersistChain` acks durable on a swallowed dir fsync - OPEN
Propagate the dir fsync error in `write_atomic` (at least for
`mode_600`).

### S10 [STYLE] Em dashes in `MoltError::Storage` strings; the transport-loss log - OPEN
`lib.rs`, `import.rs`, `export.rs` sites; the four-line `lost` sentence.

### S11 [REFACTOR] - OPEN
Three copies of "decrypt chain.state" and of "decrypt the genesis frame";
the seal/unseal quadruplet; transport.state decrypted per cursor save;
dead `at_rest = "phrase"` import arm; `open_workspace` 250 lines.

## 7. Core (`molt-core`)

### K1 [MEDIUM] `parse_patch` dropped in-hunk lines that look like `---` / `+++` headers - FIXED
A removed line starting with `-- ` vanished from the viewer AND voided
the ratified change at apply. Fix: header noise only before the first
hunk. Test: `hunk_lines_that_look_like_file_headers_are_content`.

### K2 [LOW] `apply_patch`'s "one voice per path" fires for renames only - OPEN
Dedupe `{old_path, new_path}` per file, then void unconditionally.

### K3 [LOW] `*.localhost` classified `Local` (direct, never Tor) - OPEN
Resolver-dependent. Grant `Local` to bare `localhost` and IP literals
only.

### K4 [HIGH] `ReadSession` carried every unsealed workspace's recovery phrase - FIXED (2026-08-26)
`WorkspaceInfo.seed` (a demo-era display field of the first scaffold,
`8b7c54ff`) and the two wizard phrases rode the most frequent read, over
cleartext TCP; `mcp-security.md` even documented it as "the agent holds
the same bytes". The operator's rule (2026-08-26): a phrase is private and
never shared. Fix: the three fields are `#[serde(skip_serializing)]` -
no wire form of the session carries them, on any surface; the GUI reads
them in-process. Test: `no_recovery_phrase_ever_serializes`.
OPEN (product call): `settings.s3_secret_key` and `mcp_token` still ride
`read_session` (`save_settings` is wholesale and requires them back).
OPEN: a headless node has no way to show its phrase to the operator except
the device files - a local CLI (`moltd --reveal-seed <id>`) would close
that without touching the network surface.

### K5 [STYLE] Em dashes / prose in `Display` strings; stale contract docs - OPEN
`ChatNotGated`, `AlreadyApproved`, `DiscussionClosed`,
`WorkspaceEncrypted`, `RelayUrlError` (`TooLong` hardcodes 512).
Docs: `clearnet_session` "does not survive a restart" (it does),
`ReadState.view` archive half (refused), `has_archive` hard-false,
`CreateStart.threshold` "1..=members" (m >= 2).

### K6 [REFACTOR] - OPEN
`SessionView::default()` ships six demo republics (fence test
`session_default_lists_no_workspaces`, `demo_set` behind test support);
eight inline length-prefix copies vs one `put_bytes`; dead
`InviteInfo::render`, `TorTestState::running`, `has_archive`.

## 8. Frontends (`molt-mcp`, `molt-app`, `molt-config`, `molt-ui`)

### F1 [HIGH] `save_settings` could never succeed as advertised - FIXED
The builder required `file_cap_bytes` (absent from the schema) and
defaulted `download_dir` (the H5 class). Fix: both in `properties` +
`required`, `download_dir` required. Test:
`save_settings_builds_from_exactly_its_schemas_required_list`.

### F2 [MEDIUM] `ui_action` advertises verbs the GUI does not implement - OPEN
`set_draft` / `press` / `click` are warn no-ops that still bump the
snapshot generation. Fix direction: trim the description, refuse unknown
verbs, verb inventory in `molt_core` next to `UiAction`.

### F3 [MEDIUM] "Save & continue" and "Rotate token" drop an edited wake command - OPEN
Only `on_save_settings` issues `SetWakeCommand`. Fix direction: one
`save_draft` helper issuing `SetWakeCommand` then `SaveSettings` in ONE
task, used by all three sites.

### F4 [MEDIUM] CLI-generated `config.toml` was world-readable and the runtime writer preserved it - FIXED
`--generate-config` / `--repair-config` write 0600 (`write_private`,
the repair's backup copy included - `fs::copy` kept the old mode);
`configstore::atomic_write` keeps an original's mode only within
`0o600`. Test: `a_save_narrows_a_world_readable_config_to_owner_only`.
OPEN: `molt_config::write` has no callers and a stale doc - delete.

### F5 [MEDIUM] Wiki-patch diff viewer ran an unbounded char diff on the UI thread - FIXED
Pairs over 2 KiB render whole; the char diff carries a 50 ms deadline.
Test: `an_overlong_line_pair_renders_whole_instead_of_char_diffed`.

### F6 [LOW] MCP token comparison is not constant-time - OPEN
`subtle::ConstantTimeEq` is already in the lock.

### F7 [LOW] Accept loop dies on any accept error; unauthenticated connects cost an engine round-trip, uncapped - OPEN
Log-and-continue (sleep on `EMFILE`), token fetched on `initialize`,
semaphore on pre-auth connections.

### F8 [LOW] Alert WAV in a world-writable temp dir is trusted if it exists - OPEN
`$XDG_RUNTIME_DIR` or `O_EXCL` + random suffix, or pipe the bytes.

### F9 [LOW] The co-equality test never checks that a tool builds the command its label names - OPEN
Build every tool from minimal args, compare the serde `cmd` tag.

### F10 [STYLE] Stale operator text; em dashes - PARTLY FIXED
Fixed: the `threshold` schema text (`2..=members`). OPEN: `main.rs:159`
"the Nostr transport lands with N4" at every boot; the `SIMULATION`
shape; em dashes in `LOG_SHAPES_DE` (change with the engine shapes), the
net-health / S3 / Tor detail strings, `main.rs` clap text and stdout,
`molt-config` rendered template comments. Slint files and the landing
page are clean.

### F11 [REFACTOR] `molt-ui/src/lib.rs` (14.4k lines) - OPEN
`run_app` is one 2,150-line function; tests are 4,950 lines in the same
file; five copies of the in-place `VecModel` diff; six copies of the
"toggle then push_surfaces" tail. Proposed split: `app.rs` (+ a `Ctx`),
`actions/{settings,workspace,relays,ritual,chat,org}.rs`, `mirror.rs`,
`models.rs` (one generic `sync_model`), `images.rs`, `labels.rs`,
`i18n.rs`, `surfaces.rs`, `channels.rs`, `alerts.rs`, `net_tor.rs`,
`wiki_bridge.rs`, `src/tests/*`. Mechanical moves first, then the
`Ctx` / `sync_model` collapses.

## 9. MCP tool privileges (audit 2026-08-26)

A dedicated pass over every MCP tool (70) against the lenses secret
disclosure, self-escalation / lockout, destructive actions, third-party
impact and the INTERNAL boundary. The rule applied: "agents are seats"
licenses republic actions, not the operator's machine, identity or
secrets — the same line `SetWakeCommand` already drew. The doc is
`docs_archive/security/mcp-security.md` ("The host boundary").

### P1 [HIGH] `export_workspace` exfiltrated the recovery seed - FIXED
Blob + passphrase to an agent-chosen path carried the seed ("full seat
capability"). Fix: the MCP tool is `export_workspace_archive` — no seed,
marked phrase-sealed (an import commits it sealed; the phrase opens it),
written into the exchange folder. `ExportWorkspace` is INTERNAL (GUI).
Test: `an_archive_export_carries_no_seed_and_imports_sealed`.

### P2 [HIGH] `download_file dest` was an arbitrary-file-write primitive - FIXED
Peer-chosen bytes to any writable path (a dotfile = persistence). Fix:
`dest` is a bare name inside `download_dir`, refused at the builder.
Test: `file_tools_take_bare_exchange_names_only`.

### P3 [HIGH] `share_file` read and served any file on the host - FIXED
Fix: the MCP tool is `share_file_from_exchange` (a bare name in
`download_dir`); `ShareFile` (any path) is the GUI's file dialog, INTERNAL.

### P4 [HIGH] `read_session` handed every client the S3 secret and the MCP token - FIXED
Fix: both fields never serialize (`#[serde(skip_serializing)]`); the S3
secret is settable write-only through `patch_settings`; the token's door
is `SetNodePosture`. Test: `no_recovery_phrase_ever_serializes` (extended).

### P5 [MEDIUM] `patch_settings` / `save_settings` could deanonymize the human and widen exposure - FIXED
`anonymity`, `tor_mode`, `tor_port`, `mcp_allow`, `mcp_port`,
`mcp_token`, `headless`, `workspace_dir`, `download_dir` (+ the wake
command) are the HOST POSTURE: `Command::SetNodePosture` (INTERNAL, GUI /
config) is their one door; `patch_settings` refuses them, `save_settings`
re-merges the stored values. The GUI's three save paths go through one
`save_draft` (wake → posture → wholesale, one task — closes F3). Tests:
`the_settings_tools_carry_no_host_posture_or_secret`,
`the_accept_loop_reads_the_token_that_is_current_now`.

### P6 [MEDIUM] Clearnet consent had an MCP door - FIXED
`relay_confirm` refuses `accept_clearnet: true`, `relay_clearnet_session`
refuses `unlock: true` over MCP (switching off stays). `relay_probe`
dials through the live dialer (Tor when on) — kept. Test:
`clearnet_consent_is_not_given_over_mcp`.

### P7 [MEDIUM] Export / wiki export wrote to agent-chosen paths - FIXED
Both write into the exchange folder over MCP (`wiki_export_archive`);
`WikiExport` is INTERNAL.

### P8 [MEDIUM] Ritual-abandon side effects of `create_start` / `join_start` / `recover_start` / `open_workspace` - OPEN (product)
A single call while the human is mid-founding tears the ritual down for
everyone. Fix direction: refuse the context switch while a ritual is in
flight unless an explicit `force` argument is set (both surfaces).

### P9 [LOW] Tool descriptions claimed `read_session` shows the seed - FIXED
`create_start` / `create_finish` / `join_finish` say where the phrase is
shown and that a founding/join completes on a GUI node. OPEN: a local
`--reveal-seed` for headless nodes (K4).

### P10 [LOW] No send-side rate limits on chat / poke / propose / share - OPEN
Bounded only by the hourly relay budget and the receive-side poke
cooldown. Fix direction: per-tool send caps.

---

## Checked and in order (no finding)

Canonical byte layouts (roster v4/v5, checkpoint v6/v7, block/change
bytes) are length-prefixed and counted; `republic_id` v2 injective; the
relay classifier (single WHATWG parse, authority round-trip, onion
length, IPv6 masks); `AcceptedWindow` arithmetic on hostile input;
additive event evolution; wiki-fold determinism. Every network await is
deadline-bounded; frame/response bounds hold; event verification precedes
the dedup ring; NIP-42 signs on subscribe connections only; the publish
path never authenticates; `canonical_nostr_pk` at every ingest; invite
MAC v2 constant-time. Chunk reassembly bounds; wrap nonces; drain-don't-abort.
Sign-what-you-see closes on both transports; the three anchors at the
founder ingest; ritual ephemerality; recovery seat proof v2 and consent.
Verification core of the chain; deterministic sealing; catch-up buffering;
compaction gates; backup never logs the secret. MCP pre-auth bounds, the
INTERNAL list, the wake hook, the sound player, image decoding, export
paths, i18n pairs.
