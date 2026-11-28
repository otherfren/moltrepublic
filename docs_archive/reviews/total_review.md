# Whole-Project Review (2026-07-18)

Seven parallel auditor agents across all crates: `molt-core`, `molt-config`,
`molt-storage`, `molt-net`, `molt-engine` (2x), `molt-mcp`/`molt-app`,
`molt-ui`+Slint. This document lists the **negative findings first, sorted by
severity**. Each entry is marked **✅ FIXED** (in this review),
**📋 DOCUMENTED** (deliberately deferred — effort/design/risk, recorded here for
later) or **ℹ️ ACCEPTED** (not a real defect / covered by a
higher layer).

**Addendum 2026-08-16 — the LOW remainder is worked off as well.** L1 (SVG
is rejected, user decision), L2 (the display counts only verified
signatures), L3 (wire buffers capped; the finding was more severe than LOW —
an `id + 1` remote abort in receive_checkpoint_proposal, fixed),
L7 (corrupt ≠ absent, open refuses), L8 (read_capped across all
~22 read sites + guard), L10 (retention as a machine value), L12
(mockrand doc(hidden), lcg deleted, CSPRNG jitter), L14 (guard test
Surface::ALL↔tag), L15 (canonical writers panic instead of framing empty,
incl. molt-storage republic_id). The ONLY item still open is the second half
of H3 (broadcast does not wait for confirmed persistence) — extracted
to `known_debt.md`; this document is therefore archive material.

**Addendum 2026-08-07 — the deferred list is worked off.** Both
HIGH (H3, H5), all open MEDIUM (M5, M6, M7, M8, M9, M11, M12, M13) and
the three security-adjacent LOW (L5, L6, L11) are fixed or shown to be
moot; each entry below carries the result. Two
things deliberately stayed open and are named there: the second half of
H3 (the governance broadcast does not yet wait for confirmed
persistence — a change to the state model, not to the storage API) and
the remaining LOW items L1-L3, L7, L8, L10, L12-L15.

Two findings were DELIBERATELY solved differently than proposed here, because
the proposed direction does harm: M11 (normalizing "nym"→"none" would be
a silent downgrade to clearnet — the value is retired instead)
and H5 (the merge base cannot live in the tool builder — there are now
two verbs). The reasoning is given at each entry.

State after the fixes of this review: all tests green (Core/Net/Storage/Engine/
MCP: 305; molt-ui: 37), Clippy 0, Slint build clean. New security tests:
`a_membership_proposal_cannot_hijack_a_colliding_surface_id`,
`malicious_checkpoint_heights_are_refused_not_panics`,
`serde_json_object_serializes_with_sorted_keys`.

---

## CRITICAL

### C1 — Membership-proposal id collision hijacks approvals (governance bypass) ✅ FIXED
`crates/molt-engine/src/chain.rs` `receive_membership_proposal`.
`entry(id).or_insert(...)` without a collision check: a malicious roster member
gossips `MembershipProposed{op:Joined, member:mallory, id:X}`, where `X` is a
local surface proposal id that all honest nodes are about to approve.
Since `proposal_change(id)` resolves `proposal_changes` first, every
honest approve of that surface proposal signs the **membership** bytes — at *m*
signatures a `Membership{Joined}` block seals and `mallory` (with the
attacker's key) lands in the roster, without a human ever having
confirmed a membership. Exactly the same forge the checkpoint arm was hardened
against in WP4b stage 3 — never applied to the older membership arm.
**Fix:** a shared guard `id_free_for(id, change)` (an occupied id ⇒ only the
identical change is allowed) on `receive_membership_proposal`, `receive_proposed`
(symmetrically: a surface proposal must not shadow a chain change) and
`receive_checkpoint_proposal`. Pinned by test.

---

## HIGH

### H1 — Wire `Approved` with a forged `height` freezes proposals permanently ✅ FIXED
`crates/molt-engine/src/chain.rs` `receive_approval`/`collect_sig`. A member
gossips `Approved{height:u64::MAX, sig:arbitrary}`; `collect_sig` adopts the
higher height, **deletes all genuine signatures** and from then on blocks every
legitimate approval (`height < entry.height` ⇒ return). `try_commit` requires
`pending.height == head+1`, so it never seals; `rebase_pending_approvals` only clears
heights *below* the target — never a MAX height. A single member can thereby freeze every
vote permanently (liveness DoS until restart).
**Fix:** `receive_approval` discards a height > `head+1` (the only one for which
a genuine approval can exist) before `collect_sig`.

### H2 — Height arithmetic underflow/overflow on attacker data → process abort ✅ FIXED
`crates/molt-engine/src/chain.rs` `verify_suffix_chain` (`anchor.height - 1`),
`verify_next` checkpoint arm (`block.height - 1`), `try_adopt_from_blob`
(`blob.upto + 1`, `h += 1`). `overflow-checks = true` also applies to the
release build ⇒ every underflow/overflow is an **abort**, not a wrap. A
malicious coordinator (recovery) or insider (catch-up `CheckpointServed{
blob.upto = u64::MAX}`) crashes the receiver.
**Fix:** `checked_sub`/`checked_add`, height-0 anchors/checkpoints explicitly
rejected, `h += 1` aborts on overflow. Pinned by test.

### H3 — Storage acks "durable" even after a failed write ✅ FIXED (2026-08-07, with one named remaining half)
`crates/molt-storage/src/lib.rs` `PersistChain`/`MergeCrypto` arms (~l.1916).
On a write error only the `failed` flag is set, but `ack.send(())`
fires anyway. `persist_chain_blocking` promises "blocks until durable" —
so the engine broadcasts a threshold-signed block (or closes
cleanly believing the MLS ratchet snapshot is persisted) while nothing is on
disk.
**Fixed** as described: the ack channels (`PersistChain`, `MergeCrypto`,
`Flush`) now carry `bool`, and `persist_chain_blocking` /
`persist_crypto_blocking` / `persist_mesh_crypto_blocking` /
`flush_blocking` return whether it is really durable. `#[must_use]`
forces an answer at EVERY call site — there is none left that silently swallows
the failure; a dead writer now reports
`false` instead of "no-op = all good". Pinned:
`a_gone_writer_reports_failure_instead_of_durability`.

**The second half remains open**, deliberately: the governance broadcast
still runs independently of the persist result. Rewiring that means touching the
order Append → Persist → Broadcast in `chain.rs`, and
that is a change to the state model, not to the storage API — it
belongs in a session of its own with the chain design alongside. The call site
now writes the failure loudly to the log, and the writer's `failed` flag
turns it into the "storage-failed" note on the next `record`.

### H4 — Manifest version bump came BEHIND the pruned chain write ✅ FIXED
`crates/molt-storage/src/lib.rs` `PersistChain` arm. The WP4b bump ran after the
`write_chain(Pruned)`: a crash in between (or a bump failure) left a
pruned `chain.state` under the OLD manifest version — an old binary
(e.g. a second device on a synced folder) passes the gate,
does not decode the object form and runs **chainless** on a
governance-governed workspace (fork).
**Fix:** bump BEFORE the pruned write. This direction is strictly safe — a crash
afterwards only *over*-protects (an old binary refuses a still-complete
workspace: loss of availability, no fork).

### H5 — MCP `save_settings` overwrites omitted fields with defaults ✅ FIXED (2026-08-07)
`crates/molt-mcp/src/lib.rs` `settings_arg`/`cmd_save_settings`. A
partial call `{"headless":true}` fills the rest from `SessionSettings::default()`:
`mcp_token=""` (TCP auth off), `anonymity="none"` (a Tor node silently falls back to
clearnet — deanonymization), `mcp_allow` reset, S3 creds/`smp_url`
deleted — and `persist_settings` makes it **permanent** in `config.toml`.
**Fixed, with TWO verbs instead of one merge base.** The proposed
direction does not go where it was proposed: the tool builder is
`fn(&Value) -> Result<Command, String>` and does not hold the running session at all
— it cannot tell "omitted" from "set to the default". So:

- `save_settings` is an honest FULL replace — every field is
  `required`, and a partial call is refused by name ("`anonymity` is
  required — to change one setting use patch_settings"). A silent reset
  is no longer possible.
- `patch_settings` → `Command::PatchSettings { patch }` is the partial path; the
  merge happens in the ENGINE against the running settings, where
  the actual values live. Unknown keys are refused (a silently
  ignored `anonimity` reads like "the setting did not
  take"), and `relays`/`clearnet_relays_enabled` keep their own
  door.

Pinned: `a_partial_settings_payload_is_refused_not_defaulted` (MCP),
`a_patch_leaves_every_field_it_does_not_name` +
`a_patch_naming_nothing_or_naming_junk_is_refused` (engine — the
interesting assertion is that Tor and token are NOT touched).

### H6 — GUI sound alerts: zombie processes + spawn storm + UI-thread blocking ✅ FIXED
`crates/molt-ui/src/lib.rs` `play_alert`. One `spawn()` per alert without `wait()`
(zombies until process exit); the engine emits `Event::Chat` per
mesh message and `Event::Proposed` also for re-gossiped duplicates — a
reconnect catch-up of hundreds of messages spawns hundreds of `pw-play` (cacophony
+ PID exhaustion). In addition, the first WAV synthesis + the spawn ran on the
Slint UI thread (chat arm) or a Tokio worker (vote arm), respectively.
**Fix:** 400 ms debounce (a burst plays once), all work on a
detached thread that reaps the player with `wait()`; a per-process `pid` path instead of a
worldwide-shareable `/tmp` name.

---

## MEDIUM

### M1 — Reassembler pre-allocates from the 16-bit `count` of the first chunk ✅ FIXED
`crates/molt-net/src/chunk.rs`. `vec![None; count]` with `count` up to 65535 (~1.5 MB
per partial) — an authenticated peer sends 64 one-chunk messages with
`count=65535`: ~1 MiB of wire pins ~100 MiB of RAM.
**Fix:** `chunks` is now a sparse `BTreeMap<u16, Vec<u8>>` — memory
grows with chunks actually received, never with the claimed `count`.

### M2 — serde_json `preserve_order` determinism was NOT pinned by a test ✅ FIXED
`crates/molt-core/src/chain.rs`. The canonical-bytes comments promised
"pinned by test"; no such test existed anywhere. If any
crate in the build graph enables `serde_json/preserve_order` (feature unification),
`Value::Object` becomes insertion-ordered — two nodes serialize the same payload
differently ⇒ `approval_bytes`/`checkpoint_canonical_bytes` diverge, all
signatures/checkpoints break silently.
**Fix:** `serde_json_object_serializes_with_sorted_keys` pins sorted keys;
the false comment now points at this test.

### M3 — GUI "test S3 connection" fakes success ✅ FIXED
`crates/molt-ui-window/ui/app.slint` (~l.5071). The button unconditionally shows
"S3 connection OK (mock)" without any test — violates "never fake behavior".
**Fix:** honest toast "S3 backup is not wired up yet — nothing was
tested." (the S3 backend itself remains the known deferred work).

### M4 — SMP acks eagerly while the supervisor builds on redelivery of held acks ⬜ MOOT (2026-08-07)
`crates/molt-net/src/smp/conn.rs`/`transport.rs`. `recv_next` acks message N to the
server as soon as N+1 is fetched — before the supervisor decides to buffer.
A message held in the epoch/reorder buffer (re-key storm → shed) or a
block between server ack and engine fsync is **permanently lost** on a crash
(the server does not redeliver what was acked). Loopback does not catch it (only unacked
redelivery). The "loopback can't catch it" class from CLAUDE.md.
**Moot:** the SMP transport was removed without replacement in stage N-demo (2026-07-30)
— `crates/molt-net/src/smp/` no longer exists. The
ack discipline of the Nostr delivery guarantee is a different construction
(`docs_archive/transport/delivery_guarantee.md`).

### M5 — Old transport.state/log are silently clobbered by newer versions ✅ FIXED (2026-08-07)
`crates/molt-storage/src/lib.rs` `read_transport_state` (l.930): version >
`TRANSPORT_STATE_VERSION` ⇒ `default()`, and the `SaveTransport` RMW writes the
newer file back with the old version — MLS snapshot + SMP queue creds gone,
mesh resume permanently broken (SMP forbids subscribing to foreign-created
queues). Violates the additive-only rule "an older reader must not write
on meeting something unknown".
**Fixed** exactly that way: `ensure_transport_state_not_newer` in `open_workspace`,
the counterpart of `openable_gate` for the manifest. ONLY the newer case
refuses — missing/unreadable/non-authentic/undecodable stay
"start fresh", which for a ratchet is the honest answer and would otherwise turn a
repairable workspace into an unopenable one. Pinned:
`a_newer_transport_state_refuses_the_open` (also checks that the file is left
UNCHANGED on disk after the refusal).

### M6 — Torn-tail recovery truncates at the FIRST broken frame ⇒ data loss ✅ FIXED (2026-08-07)
`crates/molt-storage/src/lib.rs` (~l.1257). A single bit flip early in the last
segment (up to 8 MiB) deletes all valid, already-acked frames behind it — in the
extreme case the whole log incl. genesis, and `open_workspace` "succeeds" with an
empty history. Middle segments get the conservative hard error, the last
never.
**Fixed** exactly that way — `has_valid_frame_after` before the truncation. The
distinguishing criterion is clean, not heuristic: the writer ONLY APPENDS, so
nothing can follow a torn-off append (the file
ends there). A valid frame behind the damage therefore means: the file was
complete and was corrupted in place — the same case the
middle segments have long refused. Pinned:
`bitrot_with_good_frames_behind_it_is_refused_not_truncated` (also checks
that the file keeps its full length after the refusal);
`truncate_anywhere_recovers_the_maximal_valid_prefix` continues to prove
that every REAL torn tail is repaired at every cut point.

### M7 — MCP `serve_conn`: unbounded `read_line` before auth (pre-auth DoS) ✅ FIXED (2026-08-07)
`crates/molt-mcp/src/lib.rs` (~l.95). On a `0.0.0.0` allow node, a
client that only passes the IP filter streams GBs without a newline; `read_line` grows
without bound → OOM.
**Fixed:** `MAX_RPC_LINE` (1 MiB) via `take()` BEFORE the parse; an
over-long line ends the connection with a JSON-RPC error response
instead of being skipped — the rest would otherwise
parse as a request of its own. Pinned:
`a_giant_pre_auth_line_ends_the_connection_instead_of_growing`.

### M8 — `random_token()` silently returns `""` on RNG failure/non-Unix ✅ FIXED (2026-08-07)
`crates/molt-config/src/lib.rs` (~l.341). `--generate-config` in a sandbox
without `/dev/urandom` (or non-Linux) writes `token = ""` and reports success —
the node then accepts every MCP connection without a token.
**Fixed:** `getrandom` instead of opening a path, and the signature is now
`Result<String, TokenError>` — the type forces both callers to answer:
`--generate-config` aborts (instead of reporting a config with an empty token as a success),
the GUI rotation leaves the OLD token in place and says so.
Pinned: `a_minted_token_is_full_length_hex_and_never_repeats`.

### M9 — MCP token is not reloaded at runtime ✅ FIXED (2026-08-07)
`crates/molt-mcp/src/lib.rs` (~l.67). `serve_tcp` captures the token at startup;
a rotation takes effect only after restart — contrary to mcp-security.md "takes effect
immediately". A leaked token stays valid, the new one does not.
**Fixed** without a second handle: the accept loop reads the token per
connection from the RUNNING session (`live_token`) — the one source of
truth, in which a rotation is immediately visible across both surfaces. A
failed read falls back to the boot token, never to
"none". Pinned: `the_accept_loop_reads_the_token_that_is_current_now`.

### M10 — Chat archive pager: clamp uses `applied.len()`, slice uses `log.len()` ⬜ MOOT (2026-08-07)
`crates/molt-ui/src/lib.rs` (~l.2678). The clamp bounds the stored page
with the raw applied count, but the pager/slice paginates over the projected
log length (incl. merged system rows in patch channels). In a
patch-channel archive with system rows the last page is unreachable
("›" jumps back).
**Moot:** the chat archive view no longer exists — the chat
is ONE retention window (product decision 2026-08-04), and with the
view its pager went away.

### M11 — Legacy `anonymity="nym"` produces a phantom "unsaved" in the settings screen ✅ FIXED (2026-08-07)
`crates/molt-ui/src/lib.rs` (~l.4166). After the nym removal, an
old config `anonymity="nym"` loads as dropdown index "none", `read_settings_draft`
returns "none", but the actual value stays "nym" ⇒ draft ≠ stored forever:
the unsaved modal appears on every leave, external settings changes
are no longer mirrored, a save silently writes "nym"→"none".
**Fixed, but the OTHER WAY ROUND from what was proposed here.** The normalization
"nym"→"none" would be the one outcome that is worse than the finding: the
dialer fails closed on "nym", while "none" chooses clearnet. Turning "anonymize
me" silently into "don't" is exactly the downgrade that
H5 warns about. Instead the value is RETIRED: `AnonymityNetwork::Nym` is
gone, `validate_settings` no longer accepts it, and `load_config` refuses
to start, naming it (`selects_retired_nym`), instead of silently reinterpreting —
a node in this state used to start happily and then fail
on every dial. Pinned:
`the_retired_nym_is_named_and_refused_not_normalized`.

### M12 — Optimistic success toasts before the engine effect ✅ FIXED for the command sites (2026-08-07)
`crates/molt-ui-window/ui/app.slint` (org-propose sites, file-remove, copy).
Several buttons toast "Proposed"/"Deleted"/"Copied" on click, before
the command runs — the `set_image` path can still fail afterwards on decode/cap,
and the user sees success, then an error, for a proposal that never
existed.
**Fixed for the sites with a command behind them** — where the
toast CAN lie: all six `org-propose` calls and the removal of a
file now confirm through `issue_then_toast`, i.e. only once the engine
has agreed (and otherwise report the error instead of success-then-error). The
`set_image` path has its own branch, because after the WP3 pre-decode it
can still fail on the engine's payload cap.

**Deliberately NOT converted:** the `toast-copied` sites. Copying is
a local operation with no engine round trip — a toast on click describes
exactly what happened there, and a callback rework would only add
ceremony.

### M13 — `roster_canonical_bytes` writes `ws_id` unframed (non-injective) ✅ DONE (verified 2026-08-07)
`crates/molt-core/src/lib.rs` (~l.1309). Unlike `approval_bytes` (which
length-prefixes `republic_id`), `roster_canonical_bytes` writes the `ws_id`
without a length prefix — two different roster tables can yield identical signed bytes by shifting
the `ws_id`/`rule_m` boundary.
**ℹ️ Safe today** thanks to the higher invariant (verifiers recompute the
fixed-length content-derived `republic_id`), so no active hole — but the
function does not deliver the injectivity its contract claims.
**Done in passing**, verified 2026-08-07: the `molt-roster-v4` bump
(relay pool + member count) framed the `ws_id` too — it is now
`u32`-length-prefixed like every other field. Pinned by the
independently computed byte fixture
`roster_canonical_bytes_v4_binds_anchor_pool_and_id_byte_exactly`.

---

## LOW (documented, not fixed)

- **L1 — SVG "billion laughs" bypasses the dimension check.**
  `proposals.rs` `image_decodable`: an `<svg`/`<?xml` prefix immediately yields `Ok`,
  without a structure/dimension check — only the 256 KiB cap applies. A hostile
  256 KiB "SVG" (nested `use`, huge `viewBox`) reaches every
  GUI renderer. Direction: bound SVG structurally (size/nesting)
  or reject SVG proposals.
- **L2 — The displayed approval count counts unverified signatures.**
  `proposals.rs` `chain_approval_count`/vote row read `pending_sigs` raw (only
  `try_commit` verifies). A peer can forge the *displayed* approval
  (display integrity; sealing remains safe).
- **L3 — Unbounded `pending_blocks`/`pending_sigs`/`proposals` from the wire.**
  `chain.rs` buffers future-height blocks and id-addressed sigs/proposals without
  a cap — an insider can slowly OOM it. Direction: capped buffers with eviction
  (caution: liveness). The underflow/freeze vectors (C1/H1/H2) are fixed; the
  pure memory bound remains open.
- **L4 — Ed448 verify not strictly canonical (S-malleability).** ⬜
  MOOT (2026-08-07): `crates/molt-net/src/smp/` was deleted with the
  SMP transport in stage N-demo.
- **L5 — `parse_mcp_allow` binds `0.0.0.0` on an unparsable allow string.** ✅
  FIXED (2026-08-07): a list that names no usable address binds
  loopback and says so on stderr — a typo is not consent to expose the
  node. The deliberately wide binds (`0.0.0.0`, a real
  LAN IP) are unchanged. Pinned:
  `an_unreadable_allowlist_binds_loopback`.
- **L6 — `dir_size` follows symlinks without cycle/depth protection.** ✅ DONE
  (verified + extended 2026-08-07): the symlink part was already fixed —
  `entry.file_type()` (unlike `metadata()`) does not follow symlinks, so a
  cycle cannot recurse. What was added is the missing
  DEPTH limit (`DIR_SIZE_MAX_DEPTH`) against the same class.
- **L7 — `read_chain` confuses damaged with absent.**
  Both ⇒ `(None, vec![])`; a caller cannot tell "unreadable history" from
  "no chain" (additive-only guard not enforceable).
- **L8 — Whole files are `fs::read` without a cap.** `open_workspace` and others
  slurp the whole file before any validation — a multi-GiB corrupt
  segment/state file OOMs the open path.
- **L9 — Various UI success toasts without a success check** — the
  command-backed part is fixed with M12; the pure clipboard sites
  deliberately stay as they are (local operation, no engine round trip).
- **L10 — The `set_chat_retention` value bakes English "N days" into the payload.**
  The actual/target row shows "7 days → 30 days" in DE. The title is
  language-neutral (op placeholder), the value is not. Direction: carry the value as a plain
  number, localize the unit at render time.
- **L11 — Poisoned-mutex `.expect()` in the live-mirror task** ✅ FIXED
  (2026-08-07): both sites (`last_settings`, `chat_ui`) now take
  `into_inner()` like the others. A panic in any callback must not
  kill the task that draws anything at all.
- **L12 — `pub mod mockrand` in the RNG-free `molt-core`** — today purely
  deterministic demo helpers, but the placement invites misuse
  (history: `mock_ticket` once derived tickets from it).
- **L13 — `mls.rs` `store_pending_proposal` stores rejected bare proposals**
  — a compromised member can let MLS provider state grow slowly.
- **L14 — `checkpoint_canonical_bytes` without a group count / `Surface::ALL` not
  in the version tag** — a future 7th surface changes the bytes without a tag bump ⇒
  mixed-version divergence. Direction: count the applied section, bump the tag on every
  surface change.
- **L15 — `put_bytes`/`to_vec` map overflow/error to length 0** instead of
  panicking — a >4 GiB field or a failed serialization would lead to
  ambiguous framing in the signed bytes. Unreachable under SMP limits,
  but a silent-wrong-bytes path.

---

## ℹ️ Checked and fine (no action)

- XChaCha20 nonces are fresh-random per frame (24 bytes) — the
  in-place rewrites of `chain.state`/`transport.state`/snapshots under the fixed AAD
  `(SEGMENT, 0)` are **not** nonce reuse.
- `ChainStateFile` untagged: array ↔ `Full`, object ↔ `Pruned`, `[]` ↔
  `Full(vec![])`; no ambiguity in the serde form itself.
- The incremental 4d walk (`fold_one`/`hash_walk_state`) and the proposer side
  (`own_checkpoint_state`/`fold_state`) fold identically — no
  consensus divergence constructible.
- The restored re-key SECURITY fix (foreign-key rejection in `apply_membership`)
  and the seat-proof/coordinator authorization hold.
- Slint `id != ""` guards (no id-requiring actions on id-less rows) are
  enforced consistently on both sides; `page_slice` is empty-safe; the
  WAV casts are saturating.

---

## Summary

**Fixed in this review (9):** C1, H1, H2, H4, H6, M1, M2, M3, plus the
`id + 1` overflow in the Proposed arm (part of the H2/C1 hardening).

**Documented for later (by severity):** H3, H5 (both "need a
signature/semantics change with ripple"), M4–M13, L1–L15.

The fixed items are the security-critical ones (governance bypass,
remote abort, governance freeze, determinism keystone) and the clearly broken
cheap ones (memory amplification, zombie processes, fake success). The
deferred ones are either design changes with effort (ack durability,
MCP merge semantics, "refuse newer version") or narrow edge cases whose
fragile partial fix would bring more risk than benefit — each with a fix direction
noted.
