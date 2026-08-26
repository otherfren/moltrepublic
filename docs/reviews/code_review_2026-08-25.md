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

### C3 [MEDIUM] `ChainRequest` is an unbounded amplifier - FIXED
One requester is served at most once per `CHAIN_SERVE_DEBOUNCE_SECS`
(30 s) and never for a height above the head. Test:
`a_catch_up_request_is_served_once_per_debounce`. OPEN: a non-logged
direct serve would spare every member's log the re-broadcast.

### C4 [MEDIUM] Byte quota prunes OTHER nodes' backups in a shared bucket - FIXED
The quota counts and prunes this node's workspaces only (the set rides
into the backup task), as the doc and the GUI hint say. Test:
`the_quota_sees_only_this_nodes_workspaces`.

### C5 [LOW] `tie_break` re-walks the chain before verifying the incoming block - FIXED
The contender's signatures are checked against the roster (threshold
included) before anything moves; only a contender that carries a valid
threshold triggers the walk.

### C6 [LOW] Dead headless-genesis adoption adopts ANY valid genesis - FIXED
A headless node adopts only a genesis carrying its replica's republic id.
Test: `a_headless_node_refuses_another_republics_genesis`.

### C7 [LOW] `MembershipOp::Joined` + a checkpoint bricks pruned holders - FIXED
`Joined` is hard-rejected by the verifier (seats are fixed at founding);
the variant stays reserved. Tests: `a_joined_block_is_refused_whole`,
`a_bundle_with_a_joined_block_is_refused`.

### C8 [LOW] Restore anchor-uniqueness gates check founding anchors only - FIXED
Both gates use `anchor_seen_in_chain` (founding anchors, every Restored
block's anchor, the blob's working anchors).

### C9 [STYLE] Em dashes / prose in strings reaching the UI - FIXED
Plain `-` and one clause in `backup.rs` / `chain.rs` strings (style sweep
2026-08-26).

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
Residual - FIXED: the chain is read before the point of no return and
its consumed ids clear the mint counter BEFORE the tail replays
(`max_applied_proposal_id`).

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

### E4 [MEDIUM] `configstore::acquire_lock` decides liveness by `/proc/<pid>` - FIXED
An flock held for the store's lifetime; the PID inside is a diagnostic.
Test: `a_stale_lock_naming_a_live_pid_is_not_a_holder`.

### E5 [LOW] `reset_workspace_state` misses five chain projections - FIXED
All five are cleared (the structural `ChainProjection` fold stays with
E8).

### E6 [LOW] `ChatRead` parks every unknown id unbounded per frame - FIXED
At most `PARKED_READS_PER_FRAME` (16) targets per frame. Test:
`a_read_receipt_frame_parks_a_bounded_number_of_targets`.

### E7 [STYLE] Prose / em dashes in engine strings the GUI renders - FIXED
`NOSTR_RUNTIME_PENDING`, the offline health reasons and the
encrypt/decrypt refusals cut to one clause; the GUI's German arms follow
(style sweep 2026-08-26).

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
Open half - FIXED: the genesis chain is verified BEFORE the first disk
write (`verify_chain` in `materialize_workspace`); `rule_n` equals the
ratified value by the verifiers' `n == seats` rule.

### R2 [MEDIUM] Founder accepts `Signed` / `BackupConfirmed` before the table is frozen - FIXED
Both are ignored before the charter proposal, a re-anchoring is refused
after it, and `finalize_founding` verifies the sealed roster before
anything is written (a failure ends the run with outcome 2).

### R3 [MEDIUM] Ticketed recovery lane has no one-re-admission-at-a-time gate - FIXED
The pending-`Restored` gate applies to both lanes (a second request is
dropped, loudly, while the first re-admission is pending).

### R4 [LOW] A single survivor can capture a reattaching seat into a private group - OPEN (design)
The Welcome's group and rotation seed are not bound to the chain. Fix
direction: require a threshold-sealed `Restored` block naming our new
anchor before flipping to `recovered:`; document the threat model in
`detached_reattach.md`.

### R5 [LOW] Far-end text chooses the joiner's headline - FIXED
The arms that embed far-end text come first; the hostile-reason test
covers "did not publish".

### R6 [LOW] Unauthenticated `JoinRequest`s write attacker text into the founder's run log; the handle is never validated - FIXED
`check_handle` (non-empty, <= 64 chars, one line) at the founder ingest
(silent drop) and in `cmd_join_start` / `cmd_create_start`; a logged
handle is thereby bounded to one line.

### R7 [LOW] Rejoiner buffers `Committed` blocks unbounded, re-verifies per frame - FIXED
Buffer capped at 256 within a 256-height window above the lowest seen;
the run is verified only when it grew.

### R8 [LOW] A recovery ticket is not bound to the member it was minted for - FIXED
`recovery_tickets` maps ticket → member; another seat's ticket takes the
self-service lane.

### R9 [LOW] Phrase / `nostr_sk` transit as plain `String` / `Vec` - PARTLY FIXED
The phrases in `JoinCtx` / `RecoverCtx` / `recover_ctx` are `Zeroizing`.
OPEN: `nostr_sk` in the two `Net*Sealed` commands (a redacting `Debug`
on `Command`, or a newtype).

### R10 [STYLE] ~100 em dashes in run-log producers and the E5 shape list - FIXED
Producers, `known_log_shapes()` and `LOG_SHAPES_DE` moved together; the
invite refusals lost their parenthetical lectures (style sweep 2026-08-26).
The stale doc comments (`lifecycles.rs:3-7`, `recovery.rs:11`, the
`cfg_attr`) stay with R11.

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
Residual (OPEN, design): a legacy workspace without a chain arms no
authority (unchecked, as before). The `ChainOracle` seam (remove+add
backed by a threshold `Restored` block) is still unwired. Design: the
engine arms a one-shot ALLOWANCE on the MLS member when a `Restored`
block for seat X seals (`allow_rekey_of(X)`); a commit removing+adding X
consumes it, one without it is HELD like a future-epoch frame and retried
when the allowance arrives (the block precedes the commit on the
coordinator, but relays reorder); the hold's eviction rules apply.

### M2 [HIGH] An evicted device undid its eviction with a back-dated same-epoch commit - PARTLY FIXED
The prior slot now records which leaves the merged commit removed; a
rewind onto a commit from one of them is refused (`rewind_forbidden`).
Test: `an_evicted_leaf_cannot_undo_its_eviction_with_a_back_dated_commit`.
OPEN half (design): the REVERSE order - the evicted device's low-key
commit arriving before the re-key makes the re-key `CommitSuperseded`;
the recovery has to retry and the device can repeat. Design: the tiebreak
ranks a commit that consumed an allowance (M1 design) above one that did
not, before comparing `CommitKey`s — a survivor that merged the evicted
device's commit first then REWINDS onto the chain-backed re-key, and
the device's commit can never win a slot the chain decided.

### M3 [MEDIUM] The prior slot is not persisted - OPEN
Two nodes decide a late same-epoch commit differently depending on a
restart in between - a silent fork among survivors. Fix direction:
persist `(prior_epoch, snapshot, CommitKey, removed)` in the MLS snapshot
(v4, additive).

### M4 [MEDIUM] Stall clock spun once the hourly budget was spent - FIXED
The spent branch never re-anchored `stalled_since`. Fix: re-anchor like a
granted round.

### M5 [MEDIUM] `retry_epoch_hold` counts frames lost that the same pass would still open - FIXED
`Opaque` frames stay held while the pass made progress; only the
terminating no-progress pass counts them lost (the counter is the
self-heal trigger). Landed for the 445 runtime in `29d7b92f`; the M12
dedupe moved the rule into the shared loop
(`epoch_hold::drain_until_no_progress`), so the mesh queue path has it
too. Test: `an_opaque_frame_survives_a_progressing_pass`.

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

### M8 [MEDIUM] §7 deep-laggard commit resends are not wired on Nostr - FIXED
The Nostr re-key arm records the commit in the log with its carrier
stamp (`MlsCommit.stamp`, additive); the outbox publishes a stamped commit
at exactly that stamp, so a resend keys like the original. Test:
`a_commits_frame_carries_its_recorded_stamp`.

### M9 [LOW] `Stopped` mid-batch discards cursor progress - OPEN
Persist `published_through` before returning on `Stopped`.

### M10 [LOW] Shape-malformed 445s are held as if they might open later - OPEN
Match `EnvelopeError::Shape` → drop; hold only `EpochOpaque`.

### M11 [STYLE] Em dashes reaching the health surface; prose logs - FIXED
The three `send_failed` / `link_down` reasons and the GUI arms that match
them use `-`; the named prose warns carry fields (style sweep 2026-08-26).

### M12 [REFACTOR] - FIXED
One progress loop (`epoch_hold::drain_until_no_progress` +
`HeldIngest`) behind `retry_epoch_hold` and `drain_epoch_buffer`;
`outbox_loop` = `publish_pass` → `PassOutcome`, `stall_decision` →
`Action`, and the pure `Backoff` / `HeldFrame` / `StallClock` (held-frame
and stall escalations stay separate, M6) with unit tests; `decode_at`
picks its control-frame parser from the `CONTROL_FRAMES` tag table (the
unknown-NUL-tag drop stays the one rule below it). `ChainOracle` /
`GroupDataRefused` / `authorize_group_data` are `cfg(test)` until N6
wires them. `MlsDecode::Ack` on 445, the plaintext `WireFrame` path and
`MlsChannel.cache` / `evict_*` are NOT dead: the loopback mesh (the test
transport, and the engine's demo mesh) runs them - each now says so in
its doc.

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

### T5 [LOW] `place_req` narrows a multi-window catch-up on reconnect - FIXED
The resume cursor applies to open-ended subscriptions only (a filter that
carries its own `since` keeps it).

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

### T9 [STYLE] Em dashes in `TorTest.detail`, `S3Error` hints, `NetError::Framing` - FIXED
Sources and the GUI's prefix arms use `-`; the two long Tor hedges are one
clause each (style sweep 2026-08-26).

### T10 [REFACTOR] - PARTLY FIXED
`publish_one` = connect → `send_and_await_ok` → close; one
`relay_dial_coords` for the WS connection, the NIP-11 probe and the Tor
probe. OPEN: `with_cursors` / `cursors()` / `subscribe_since` still have
only test callers (their doc claims a reopen reseeds them).

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

### S10 [STYLE] Em dashes in `MoltError::Storage` strings; the transport-loss log - FIXED
Error strings use `-`; the transport-loss log is one line with
`cause=` / `loses=` fields (style sweep 2026-08-26).

### S11 [REFACTOR] - FIXED
`decode_chain_state` / `read_chain_at` and `decrypt_genesis_frame` /
`genesis_frame_at` behind the three copies each (typed faults, each
caller keeps its wording); `seal_blob` / `unseal_blob` behind the
quadruplet; `read_transport_state_raw` with the loud-default read and the
open's newer-version gate as wrappers, the decoded state cached on
`OpenedWorkspace` (test: `a_cursor_save_reads_the_cached_transport_state`);
`open_workspace` in four named steps. The `at_rest = "phrase"` import arm
is NOT dead - `export_archive` produces such blobs since `2cace65b`
(pinned by `an_archive_export_carries_no_seed_and_imports_sealed`); it
stays, with a comment naming that test.

## 7. Core (`molt-core`)

### K1 [MEDIUM] `parse_patch` dropped in-hunk lines that look like `---` / `+++` headers - FIXED
A removed line starting with `-- ` vanished from the viewer AND voided
the ratified change at apply. Fix: header noise only before the first
hunk. Test: `hunk_lines_that_look_like_file_headers_are_content`.

### K2 [LOW] `apply_patch`'s "one voice per path" fires for renames only - FIXED
Test: `two_sections_on_one_path_are_void`.

### K3 [LOW] `*.localhost` classified `Local` (direct, never Tor) - FIXED
Only the bare `localhost` (and IP literals) are `Local`.

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

### K5 [STYLE] Em dashes / prose in `Display` strings; stale contract docs - FIXED
Docs corrected (`clearnet_session`, `RelayClearnetSession.unlock`,
`ReadState.view`, `CreateStart.threshold`); the `Display` strings use `-`
and `RelayUrlError::TooLong` formats `MAX_URL_LEN` (style sweep
2026-08-26). `has_archive` stays (a wire field; documented dead).

### K6 [REFACTOR] - FIXED
`SessionView::default()` lists no workspace (fence test
`session_default_lists_no_workspaces`); `TorTestState::running` deleted.
`InviteInfo::render` is NOT dead (the founder's link rendering uses it).
One `molt_core::put_bytes` / `put_count` behind the inline length-prefix
copies in `roster_canonical_bytes`, `chain.rs` and
`molt_storage::republic_id` - byte-identical (the v4/v5/v6/v7 pins and
the literal `republic_id` fixture prove it). `WorkspaceInfo::demo_set` is
`#[doc(hidden)]` and documented as a test fixture (it stays `pub`: the
engine, MCP and GUI suites use it).

## 8. Frontends (`molt-mcp`, `molt-app`, `molt-config`, `molt-ui`)

### F1 [HIGH] `save_settings` could never succeed as advertised - FIXED
The builder required `file_cap_bytes` (absent from the schema) and
defaulted `download_dir` (the H5 class). Fix: both in `properties` +
`required`, `download_dir` required. Test:
`save_settings_builds_from_exactly_its_schemas_required_list`.

### F2 [MEDIUM] `ui_action` advertises verbs the GUI does not implement - FIXED
The inventory is `molt_core::UI_ACTION_VERBS`; the engine refuses any
other verb; the tool description lists the five.

### F3 [MEDIUM] "Save & continue" and "Rotate token" drop an edited wake command - FIXED
One `save_draft` (wake → posture → wholesale, one task) behind all three
paths (landed with the MCP privilege batch, `2cace65b`).

### F4 [MEDIUM] CLI-generated `config.toml` was world-readable and the runtime writer preserved it - FIXED
`--generate-config` / `--repair-config` write 0600 (`write_private`,
the repair's backup copy included - `fs::copy` kept the old mode);
`configstore::atomic_write` keeps an original's mode only within
`0o600`. Test: `a_save_narrows_a_world_readable_config_to_owner_only`.
`molt_config::write` (no callers) is deleted.

### F5 [MEDIUM] Wiki-patch diff viewer ran an unbounded char diff on the UI thread - FIXED
Pairs over 2 KiB render whole; the char diff carries a 50 ms deadline.
Test: `an_overlong_line_pair_renders_whole_instead_of_char_diffed`.

### F6 [LOW] MCP token comparison is not constant-time - FIXED
`subtle::ConstantTimeEq`.

### F7 [LOW] Accept loop dies on any accept error; unauthenticated connects cost an engine round-trip, uncapped - FIXED
Accept errors are logged and retried; open connections are capped at 64.
OPEN: the token is still read per accepted connection (one session read
before auth).

### F8 [LOW] Alert WAV in a world-writable temp dir is trusted if it exists - FIXED
Under `$XDG_RUNTIME_DIR` when present, a random per-process-start tag,
created exclusively (never through an existing file).

### F9 [LOW] The co-equality test never checks that a tool builds the command its label names - FIXED
Every tool that builds from a schema-derived argument set is compared
against its label's serde tag.

### F10 [STYLE] Stale operator text; em dashes - FIXED
The `threshold` schema text (`2..=members`), the boot line and the
`SIMULATION` shape no longer mention N4; `LOG_SHAPES_DE`, the net-health /
S3 / Tor arms, the MCP tool descriptions, `main.rs` and the rendered
`config.toml` comments use `-` (style sweep 2026-08-26).

### F11 [REFACTOR] `molt-ui/src/lib.rs` (14.4k lines) - FIXED
Split by responsibility, `.slint`-free, no behaviour change (every
callback registration and every test carried over one-to-one, 179 tests
green after each move). `lib.rs` is the crate docs, the module list and
the public re-exports (`run_app`, `LinkKind`, `link_kind`, the window
glob). The map, with line counts:

- `app.rs` (267) - `run_app`: window, wiring, mirror, event loop; `Ctx`
  (`rt`, `wallet`, `weak`, `last_settings`, `chat_ui`) is what every
  callback captures, with `issue` / `issue_then_toast` / `issue_draft` /
  `refresh_surfaces` as its methods - the ~70 hand-cloned capture blocks
  and the six "toggle UI state, then spawn push_surfaces" tails are gone.
- `actions/{settings 328, workspace 172, relays 229, ritual 389, chat
  237, org 498}.rs` - the callback wiring, one `wire(ui, ctx)` each.
- `mirror.rs` (1525) - `push_session` / `apply_session` / `apply_runs` /
  `apply_relays`, `push_surfaces` / `apply_surfaces`, the UI snapshot
  publish, `spawn_mirror` (the event loop task).
- `surfaces.rs` (1557) - the `Send` bundle + `gather_surfaces`, the
  UI-local `ChatUiState`, proposal/chain/table rows, display titles.
- `chat_log.rs` (375), `channels.rs` (309) - chat rows and the chat bus.
- `settings.rs` (219) - the draft read/apply/dirty check and the
  three-door save.
- `models.rs` (77) - ONE `sync_model(rc, items, eq, set)` behind the
  former `sync_rows` / `sync_vec_model` / `sync_wiki_blocks` copies
  (`sync_rows` / `sync_strings` stay as wrappers); the in-place patch
  semantics are unchanged - a `ModelRc` is never replaced once it is a
  `VecModel`.
- `images.rs` (241), `labels.rs` (453), `i18n.rs` (1427), `alerts.rs`
  (180), `net_tor.rs` (178), `wiki_bridge.rs` (936).
- `tests/{i18n,images,chat_log,channels,surfaces,labels,mirror,ritual,
  net_tor,settings,relays}.rs` and `tests/gui/{snapshot,wiki,poke,layout,
  recovery_backup,chat}.rs` (helpers in `gui/mod.rs`), 5.1k lines, behind
  one `#[cfg(test)] mod tests;` with the prelude in `tests/mod.rs`.

`pub(crate)` is kept to what crosses a module boundary. Not done, by
choice: rustfmt over the moved code (it would bury the mechanical moves
in a reformat diff) and a further split of `i18n.rs` (a lexicon table)
or `mirror.rs` / `surfaces.rs` (each one responsibility).

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
