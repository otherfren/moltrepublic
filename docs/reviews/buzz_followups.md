# Buzz follow-ups — the four work packages

Decided 2026-08-01 in the planning session over
`docs_archive/reviews/buzz_comparison.md`. That
document holds the analysis and the reasoning per candidate; this one holds only
what gets built, in the order it gets built.

Nine candidates were walked. Five produced no work: S1 dropped (there are no
agents, only seats), S3 parked (no mobile client), S8 rejected (we already hold
both principles, and hold them harder), S9 declined (mesh transfer stays,
Blossom not adopted), and S5 is a convention rather than a package. Four remain.

House rules apply throughout and are not repeated per step: test first and watch
it fail for the right reason; `cargo clippy --all-targets` at zero before a
commit, `.expect("…")` and never `.unwrap()`, including in tests; every new
`Command` is either an MCP tool or on the documented INTERNAL list, or
`co_equality_every_command_is_a_tool_or_documented_internal` goes red.

## Order

| # | Package | Area | Size |
|---|---|---|---|
| B1 | Kind registry (S4) | `molt-net` | ~30 min |
| B2 | Read state engine-side (S2) | `molt-core`, `molt-engine`, `molt-mcp`, `molt-ui` | large |
| B3 | Hostile-relay bounds (S6) | `molt-net` | medium |
| B4 | Relay probe (S7) | `molt-net`, `molt-core`, `molt-engine`, `molt-mcp`, `molt-ui` | medium |

B1 first because it is half an hour and clears the deck for anything that
touches frames. B2 next because it carries the most product value: with agents
driving seats through MCP, "what is new?" is the question every agent session
opens with, and today it cannot be asked. B3 before B4 because the probe reuses
the bounds it establishes.

## B1 — Kind registry (S4)

**Why.** We use 443, 444, 445 and 1059. Exactly one is a named constant —
`KIND_GROUP` in `crates/molt-net/src/ritual_net.rs:45`, module-private — and the
rest live as bare numbers in code and comments. N5 will add frame types, and two
work packages allocating in parallel is precisely how a collision arrives.

**Steps.**

1. Red: a test in the new `crates/molt-net/src/kinds.rs` asserting that the
   exported set of kinds contains no duplicates and that each documented kind is
   present. It fails because the module does not exist.
2. Create `kinds.rs`: one named `pub const` per kind with a one-line comment
   saying what it carries (443 KeyPackage, 444 Welcome, 445 group message, 1059
   gift wrap), plus a slice exporting the full set.
3. Replace every literal use, starting with `ritual_net.rs:45`. Grep for the
   bare numbers afterwards; the ones left should only be in prose comments.
4. Green, clippy clean.

**Not in scope.** Buzz's banded ranges (ephemeral / replaceable / custom). We
have four kinds, three of them fixed by the Marmot spec — bands would be
ceremony without a payload.

## B2 — Read state engine-side (S2) — ✅ BUILT 2026-08-04

Landed as specified: one cursor per channel, `MessageId`-addressed, held by
the engine and persisted in `prefs.toml` (`WorkspacePrefs.read_cursors`,
local-only). `Command::MarkChannelRead` (a tool on both surfaces —
`MarkRead` stayed the shared read-RECEIPTS verb) advances it, only ever
forward; `ReadState`'s channel enumeration carries per-channel `unread`,
and the chat surface gained the `view: "unread"` slice — exactly the
messages after the cursor, in order, by id. Seeding survived the move
engine-side: the first observation of a cursor-less workspace marks
everything read. The GUI derives its badges from the engine counts, marks
the on-screen channel read, and `UnreadLedger` is deleted. Step 7
(mark-unread) deliberately not built, as the step itself argues. Pins:
`a_read_cursor_survives_a_restart`,
`marking_a_channel_read_counts_and_slices_by_id`,
`a_read_cursor_survives_compaction_by_id` (the id-versus-index decision).

**Why.** `UnreadLedger` lives in `crates/molt-ui/src/lib.rs` and is in-memory;
its own comment names persistence as "the B5 stretch package". The word *unread*
appears nowhere in `molt-engine` or `molt-mcp`. Two consequences: every restart
presents the whole history as unread, and an agent driving the same seat through
MCP cannot see what is new. The second one is the blocker — it contradicts
co-equality, which we otherwise enforce strictly (channel filtering is
engine-side for exactly this reason).

**Design.** One cursor per channel, addressed by `MessageId`, held by the engine
and persisted. The GUI derives its counter from the cursor; the agent asks for
the messages after it. Read state is the seat's own and private — distinct from
the read *receipts* we already share with the group, which stay as they are.

**Watch out.** The ledger keys by position today (`last_seen: HashMap<String,
usize>`). Positions shift under WP4a compaction. In memory and reset per session
that may be harmless; persisted it certainly is not. This is the same class of
trap the WP4a keystone caught — id addressing is the standing rule, and the
migration must not carry the index model across.

**Steps.**

1. Red: engine test — mark a channel read at message *m*, restart the engine
   from the same storage, assert the cursor is still at *m*.
2. Red: engine test — after the cursor, ask for new messages and get exactly
   those after *m*, in order, by id.
3. Red: compaction test — prune below the cursor and assert the cursor still
   resolves to the same logical position. This is the one that pins the
   index-versus-id decision.
4. Implement: cursor state in the engine, persisted through the existing prefs
   path; `MessageId`-addressed throughout.
5. Extend the read contract (`Command::ReadState` and its surface) so the cursor
   and the after-cursor slice are available to both surfaces. Check whether this
   needs a new `Command` or fits the existing one — if new, the co-equality test
   decides where it must be registered.
6. Move the GUI to derive its counter from the engine cursor; delete
   `UnreadLedger`. The first-observation seeding behaviour (opening a workspace
   must not present its whole history as one unread wall) has to survive the
   move — keep it, engine-side.
7. Manual mark-unread: only if it exists today. If it does not, do not build it
   here. Buzz needs CRDT counters for it because they merge across devices; with
   one machine per seat a flag suffices, and the counters would be machinery for
   a device count of one.

## B3 — Hostile-relay bounds (S6)

**Why.** Buzz bounds what a malicious client can do to their server. We are the
client against relays we do not control, which is the same problem mirrored: an
oversized frame, an event flood answering a single REQ, or a connection that
dribbles a byte at a time and pins us forever.

**Their yardstick**, to be adapted rather than copied: frames capped at 64 KiB,
1024 subscriptions per connection, 500 historical events per filter, and a
slow-peer counter that drops the connection after three consecutive full
buffers. Re-verify these against their source before adopting a number as ours.

**Explicitly not in scope.** The IP/SSRF checklist. Verified during planning:
`IpAddr`, `Ipv4`, `Ipv6`, `to_socket_addrs` and `lookup_host` appear nowhere in
`molt-net`. Hostnames go to the SOCKS proxy and resolve inside the Tor circuit —
resolving them ourselves would be the leak we are avoiding. Buzz needs SSRF
guards because their workflow engine fetches arbitrary URLs; we have no
webhooks.

**Re-verified against HEAD, 2026-08-02 — two of the four bounds already exist.**

- The frame cap is there: `MAX_WS_MESSAGE = 1 MiB` on both `max_message_size`
  and `max_frame_size` (`relay_ws.rs:29,175`), enforced by tungstenite.
- The dribbler is covered: every read is
  `ws.recv(KEEPALIVE.min(SUB_IDLE_TIMEOUT))` with a keepalive ping
  (`relay_runtime.rs:726`), so a connection that goes quiet dies on the idle
  bound instead of pinning us.
- Events are signature-verified before anything trusts them
  (`relay_runtime.rs:729`), so a relay cannot forge ids to suppress an honest
  copy through the dedup ring.

**What is genuinely missing is the per-REQ event bound** — and it needs more
care than buzz's number suggests:

> **It interacts with the catch-up subscription (N5.1).** A naive "500
> historical events per filter" would silently TRUNCATE a legitimate
> `subscribe_since` over several windows, which is worse than the flood it
> prevents: the flood is noisy and self-announcing, a truncated catch-up is a
> member quietly missing history it believes it has.

So the bound must be (a) on the HISTORICAL phase only — counted until EOSE,
with live traffic left unbounded, which is what buzz's own number means; (b)
generous enough that a real multi-window catch-up never reaches it, an order of
magnitude above buzz's chat-shaped 500; and (c) LOUD when hit, because at that
size it is evidence, not tuning.

**Steps.**

1. ~~Red: a relay double sends a frame past the cap~~ — already bounded; write
   the test to PIN it rather than to add it.
2. ✅ **DONE 2026-08-02.** `MAX_STORED_EVENTS_PER_REQ = 5_000`, counted
   pre-EOSE only, with `RelayRuntime::with_history_bound` so a test does not
   have to publish five thousand events to reach it. Keystone:
   `molt-net/tests/relay_flood_bound.rs` — the flood goes in through the real
   `publish_frame` path (a flood of WELL-FORMED events is the case that
   matters; a malformed one is already dropped by the tag gate), and the whole
   molt-net suite including the N5.1 catch-up stays green, which is the
   interaction this bound was specified around. No hostile relay double was
   needed after all: a real relay holding hostile events IS the double.
3. Red: a relay double accepts the connection then dribbles; assert we time out
   instead of pinning the connection.
4. Implement the bounds in `relay_ws.rs` as named constants with a comment on
   where each number came from.
5. Diagnostics stay structured and one line — `relay=… bound=… got=…` — matching
   the strings already on master.

**Residual, noted not planned.** An invite ticket carries attacker-supplied
relay URLs (`invite.rs`), and in `Direct` (clearnet) mode a `ws://localhost:…`
entry would be dialled by the OS resolver. Under Tor it is moot, since Tor
refuses private ranges, and clearnet requires the explicit acknowledgement. The
defence at that layer is the WHATWG parser gate, already in place after the two
CRITICAL host-parser bugs. Worth a look when someone is next in `invite.rs`; not
worth a package.

## B4 — Relay probe (S7) — ✅ BUILT 2026-08-04

**Deviation from "mandatory at add-time", deliberate:** the probe gates the
**confirmation**, not the add. `relay_add`'s contract (ADR-0004) is "adding
is safe — nothing is dialed", and a probe dials; the confirm is the moment
dialing is consented to, so that is where the probe runs. The spec's goal
survives intact: an unconfirmed entry is inert, so an unusable relay never
becomes an ACTIVE one. A second deviation, found by the tests: the verdict
is three-valued. **Unusable** (the relay ANSWERED and disqualified itself —
no kind 445, no retention, tiny frame cap) never confirms; **Unreachable**
(down right now, or onion while Tor is off) cannot be JUDGED, so the
operator's consent stands — the entry confirms and the verdict says so
honestly (`relay-unverified:`). Without the middle class a relay's downtime
would veto the operator, and no onion relay could ever be confirmed before
Tor is up. Landed: `molt_net::relay_runtime::probe_relay` (bounded phases,
one reason — reachability, frame cap via NIP-11 best-effort, kind-445
acceptance under an ephemeral key, NIP-42 READ auth satisfied with the same
key, retention via fetch-back), the `relay_probe` tool on both surfaces,
`NetRelayProbed` INTERNAL, and the verdict on the notice channel
(`relay-ok:`/`relay-unverified:`/`relay-refused:`), toasted in the GUI.
Pins: `the_probe_passes_a_well_behaved_relay`, `…_a_read_auth_relay`,
`…_refuses_a_relay_that_blocks_kind_445`, `…_that_does_not_retain`,
`…_calls_a_dead_relay_unreachable_within_its_bound`,
`an_unusable_relay_is_never_confirmed`,
`an_unreachable_relay_confirms_unverified_by_name`.

**Why.** Today a relay is added blind and its unsuitability surfaces later as a
failure. Four questions are answerable in seconds with the dialer and WS client
we already have: does it accept kind 445, does it demand authentication, does it
retain events long enough for a join ritual to complete, does it survive our
frame sizes. This is the preventive half of the diagnostics already on master,
which name the relay and the reason after a failure.

**Shape, as decided.** Mandatory at add-time — an unusable relay never enters
the list. The result is a verdict plus the single reason it failed, not a
protocol log. A `Command`, so both surfaces can vet a relay.

**Steps.**

1. Red: probe a relay double that refuses kind 445; assert verdict *unusable*
   and that the reason names the kind.
2. Red: probe a double that demands auth we can satisfy; assert *usable*.
3. Red: probe a double that drops events immediately; assert *unusable* with
   retention as the reason.
4. Implement the probe over the existing dialer and WS client, reusing B3's
   bounds. Every check gets a timeout — a probe that hangs is worse than one that
   fails.
5. Add the `Command` and register it on both surfaces (it is a human decision
   aid, so it is a tool, not INTERNAL).
6. Wire it into the add-relay path so the verdict gates the insertion, and
   surface the reason in one line.

**Deliberately not built.** A full conformance suite in Buzz's sense. We need a
gate on four questions, not a specification test bench.

## Standing rules adopted alongside

- **New canonical byte layouts get prose plus fixtures** (S5). No retrofit of
  `molt-roster-v3`, `molt-republic-id-v2` or `molt-chain-checkpoint-v2`; from
  here on, a new layout arrives with a short spec and an accept/reject fixture
  set beside its byte-pin test.
- **A push notification carries exactly one meaning, "reconnect"** (S3). No
  content, no sender name, ever — recorded now so it survives to whenever a
  mobile client appears.
- **A deleted message keeps a visible tombstone naming who deleted it** (S8).
  The data model already does this; the rule fixes the display side.
