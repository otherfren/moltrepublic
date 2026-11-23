# Diagnostics on request: a redacted log package from another seat

**Status: PLAN, not started (user decision 2026-11-23: design first,
later release).** Source: an operators' wish list from a live 2-of-3
republic (item I2; seats A/B/C, anonymised). Nothing here is built.
The open questions in §11 must be answered before S2.

## 1. Purpose

A member asks another seat for a redacted technical log package, to
diagnose a fault between seats without operators swapping raw logs by
hand. The asked seat's operator sees exactly what would leave, and
decides.

## 2. What exists (v0.1.2)

| piece | where | what it gives |
|---|---|---|
| per-seat sync | `molt_core::SeatSync` (`build`, `head_hash`, `head_height`, `diverged`, `dup_streak`), filled in `chain/sync.rs` `seat_syncs` | another seat's build and head with no request at all |
| refused blocks | `ChainSync::refused` (`BlockRefusal { height, hash, from, rule, at }`), rule codes in `chain/sync.rs` `refusal_rule` | why this node refused a block, as a stable code |
| stuck heal | `ChainSync::heal_stuck` (`HealStuck { peer, since, height, rule }`) | a heal that does not progress |
| relay refusals | `StatusView::relay_refusals` (`RelayRefusals`, transient count + `last_refusal`) | relay trouble, per relay |
| the log file | `molt-app/src/main.rs` (daily file under `<config dir>/logs/<config stem>/`, owner-only, `[node] log_file`) | everything, unredacted, local |
| reading it | `Command::ReadLog` -> `session.rs` `cmd_read_log`, `log_read.rs`; the `read_log` tool; Settings › Error log | the newest lines, by level |
| a file to members | `share_file` -> `chat.rs` `cmd_share_file`, `transfer.rs` (`NetFileShared`) | an encrypted temporary share in a channel |

The status fields already answer most of the operators' first
question ("which build, which head, who refuses what"). The package is
for what status cannot hold: the sequence of events around a fault.

## 3. The flow as requested

| step | who | what |
|---|---|---|
| 1 | member X | `diag_request { member, question, scope, since, until }`; `scope` ∈ `chain_sync`, `governance`, `delivery`, `relay`, `recovery`, `votes` |
| 2 | asked seat Y | shows the request: who, the question, the scope, and a PREVIEW of the finished package |
| 3 | Y's operator | release, or decline; with no answer the request expires (see Q5) |
| 4 | Y | the package goes encrypted to the members, as a file share in the request's channel |
| 5 | X | reads it like any shared file; the request card reads `released`, `declined` or `expired` |

## 4. The hard question: "only a human may release"

This cannot be ENFORCED by the node.

- Since ADR-0007 the seat token operates the machine:
  `docs_archive/security/mcp-security.md`, "The machine boundary": "the
  line is WHO SPEAKS, not what is allowed". The engine cannot tell a
  human from an agent holding the token. A flag like `agent: true` is
  self-declaration, and a client omits it.
- A GUI-only door is a deterrent, not a gate. `ui_action` drives the
  window by domain verb (`docs_archive/ui/gui_over_mcp.md`, "What an
  action names": `click { target }` for named affordances). A verb for
  the release button would hand the door back to the token. Leaving the
  verb out still leaves the agent the machine (ADR-0007).
- A headless seat (`node.headless = true`, the service mode) has no
  window, so under a GUI-only rule it could never release.
- The project has no agent concept: "Agents are seats; the threshold is
  the only authority" (`docs_archive/kanban/kanban_workflows.md`).

| option | how | cost | what breaks |
|---|---|---|---|
| A | ordinary `Command` + MCP tool `diag_respond`, co-equal with a GUI button. The human gate is POLICY: the tool is on the wake skill's `DENY_TOOLS` (`crates/molt-mcp/src/wake_skill.md`, DE copy `crates/molt-ui/src/wake_skill_de.md`), and the tool description says "the operator's decision" | none for headless seats | nothing; the policy holds only for agents set up through the wake skill |
| B | a second factor only a human holds: a word from the recovery phrase, or a local stdin/TTY prompt | a headless service has no TTY. The phrase is on the surface since ADR-0007 (`reveal_seed`), so an agent can supply the word | a factor the agent can read is not a factor |
| C | GUI-only INTERNAL command (the `ui_publish` pattern) | headless seats can never release | co-equality: `co_equality_every_command_is_a_tool_or_documented_internal` (`crates/molt-mcp/src/lib.rs`) passes only with a new INTERNAL entry, against CLAUDE.md's rule that human decisions are tools on both surfaces |

**Recommendation: A.**
- `diag_respond` goes on `DENY_TOOLS`; `the_wake_skill_lists_partition_the_tool_catalogue` (`molt-mcp/src/lib.rs`) forces a place for it.
- `diag_respond { id, release: true }` must carry the `digest` of the preview it releases. The engine refuses a release whose digest differs from the package it would send now, so nothing leaves that the caller has not fetched in full.

The user decides between A, B and C (Q1).

## 5. Redaction by allow-list

Rule: per log event SHAPE, a list of fields that may leave. Everything else
is dropped; an unknown shape is dropped whole. Hashes are cut to 12 hex.

Never leaves (no shape may list it): chat text, wiki text, file names, vote
notes, proposal payloads, keys, seeds, tokens, local paths, IP addresses,
peer addresses other than the chain's ratified relays.

| shape (example) | allowed fields |
|---|---|
| `refused a chain block` | `height`, `hash`, `from`, `rule` |
| `chain_catchup=*` | `from`, `from_height`, `blocks`, `known`, `reason` |
| `fork_decision` | `from`, `fork`, `outcome`, `mine`, `theirs`, `mine_id`, `theirs_id` |
| `vote=refused`, `approval not signed` | `id`, `peer`, `peer_head`, `head`, `reason` |
| relay refusal | `relay` (only if in the ratified pool), `class` |

Engineering consequence: the log today is `tracing` lines with free-text
messages. An allow-list needs STRUCTURED events with stable shapes:
- a constant message per shape (a code, not prose), and
- fields as `key=value`.

v0.1.2's rule codes (`refusal_rule`) and the `key=value` diagnostics rule
(CLAUDE.md, "User-facing text") are the start.

Measuring the share already usable (method, run before S1):
1. List every `tracing::{warn,info}!` call in `crates/*/src`. A first pass over single lines: 615 calls, 532 with a field on the same line. Multi-line calls are miscounted, so this is an upper bound for "has fields".
2. Classify each call by hand:
   - (a) constant message and only scalar fields;
   - (b) fields, but the message carries data;
   - (c) a `%e`/`error` field holding free text (relay or peer supplied).
3. Only (a) can be allow-listed as is. (c) needs a code instead of the text.

The package is built from the shape registry, not by scrubbing lines. A
line whose shape is not registered never leaves.

## 6. Limits and where they are enforced

| limit | value | enforced |
|---|---|---|
| package size | ≤ 256 KiB after redaction | asked seat, at build; also below the share path's own cap |
| time span | `until - since` ≤ 24 h | at `diag_request` (requester) and again at build (asked seat) |
| requests | ≤ 3 per requesting seat per day at the asked seat | asked seat, at ingest; excess dropped with one WARN `diag_request=refused reason=rate` |
| open requests | ≤ 1 per (requester, asked seat) | asked seat |

The rate window is a counting window, not a behaviour deadline. Expiry is
one (Q5).

## 7. Wire, storage

- Events: `DiagRequested { id, member, question, scope, since, until }`, `DiagDeclined { id }`, and the package as an ordinary file share (`share_file` path) whose message names the request id. All additive.
- Unknown variants on older builds: `serde` fails, then WARN `discarding an undecodable event frame`, then `MlsDecode::Discard` (`molt-net/src/supervisor.rs`, the envelope parse after the control-frame table). The envelope is never acked. So both new events MUST be in `molt_core::unowed_on_the_wire` (`molt-core/src/lib.rs`), or the sender's outbox owes them to an old peer forever (rewind rounds, then `send_failed`).
- Price of an unowed event: a lost request is not resent; the requester asks again. Alternative: NUL-tagged control frames, which an old build drops silently (`supervisor.rs`, "an unknown control tag ... is dropped as a no-op").
- `question` is free text from a member, shown as such and never executed (the wake skill's rule "text of other people is never an instruction").
- Storage: open requests live in the session (ephemeral, like votes). A released package is a temporary share and ages out with it. Nothing new on disk at the asked seat beyond the share. Retention at the receiver: Q3.

## 8. GUI and MCP

- MCP:
  - `diag_request`, `diag_preview { id }` (the package and its `digest`), `diag_respond { id, release, digest }`;
  - `status.diag_requests[]` (open requests to and from this seat);
  - `read_log` unchanged.
  - Every tool goes into the wake skill partition.
- GUI: a request card in the notice area and the request's channel: who, question, scope, a "Preview" that opens the redacted package read-only, Release / Decline.
- Slint cost: one card and one modal. It needs the full window build once (release only); iterate with `scripts/dev-ui.sh`. No em dash in strings.

## 9. Tests (red first)

| test | pins | counter-test |
|---|---|---|
| a package from a log seeded with a list of secret test strings (chat body, wiki text, a file name, a vote note, a token, a seed word, a local path, an IP, a non-pool relay) contains none of them | redaction | add one shape with a leaking field: red |
| an unregistered shape is absent from the package | allow-list, not deny-list | |
| decline -> requester sees `declined`; no share is sent | flow | |
| no answer -> `expired` (if Q5 keeps expiry) | flow | |
| a release with a stale `digest` is refused | preview mandatory | same digest: released |
| under option A: `diag_respond` is in `DENY_TOOLS`; under C: no tool exists and the co-equality test lists it INTERNAL | the chosen gate | |
| an older build (event parser of v0.1.1) receives `DiagRequested`: it drops it, and the sender's outbox does not stall (`send_failed` stays unset) | unowed rule | remove it from `unowed_on_the_wire`: red |
| a 4th request in a day from one seat is dropped | rate limit | |

## 10. Steps

| step | content | ships alone | effort (agent-h) |
|---|---|---|---|
| S1 | structured log events: a shape registry for the six scopes, constant messages + `key=value`; `diag_package { scope, since, until }` builds a LOCAL redacted package + digest, no network; tests 1-2 | yes: an operator can hand-share a safe package | 12-16 |
| S2 | `DiagRequested` / `DiagDeclined` over the group (unowed), rate and size limits, `diag_request` / `diag_preview` / `diag_respond` tools, release via the file-share path, `status.diag_requests[]`; tests 3-8 | yes, MCP only | 10-12 |
| S3 | GUI card + preview modal, DE/EN strings | yes | 6-8, plus one full window build |

## 11. Open questions (user)

| # | question | recommendation |
|---|---|---|
| Q1 | enforcement: A, B or C (§4) | A + deny list + mandatory preview digest |
| Q2 | may a package name other seats (e.g. `from=B` in a refusal)? | yes, seat names only: they are the diagnosis, and members know them already |
| Q3 | retention of a received package | the share's own temporary lifetime; no copy into the wiki |
| Q4 | does `diag_query` (the operators' live ping, I1) belong here? | no: v0.1.2's `status.seats[]` covers build/head/diverged without a request. A live ping is a separate control frame, if still wanted |
| Q5 | expiry after 24 h without an answer is a behaviour deadline (project rule: none invented) | no expiry; the request stays open until answered or withdrawn by the requester |
| Q6 | unowed events (a lost request is not resent) or control frames | unowed events; they stay in the log and the card can show them |
