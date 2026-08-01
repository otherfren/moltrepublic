# Buzz (`block/buzz`) — comparison and adoption candidates

Date: 2026-08-01. Subject: <https://github.com/block/buzz>, Apache-2.0, Block Inc.

## 1. Why this document

Buzz is the closest public sibling to MoltRepublic: a Rust workspace that builds
a collaborative workspace on top of Nostr, with signed events as the single
substrate, cryptographic identity for every participant, and an explicit
agents-are-members stance. It is also the *inverse* of our product on the one
axis we care most about — privacy.

That combination makes it valuable. Where a design of theirs is orthogonal to
the trust model, they have usually already solved a problem we still have, and
we can adopt the solution outright. Where a design only works because a server
reads plaintext, it is unusable for us regardless of how polished it looks.

This document sorts their work into those two buckets and records, per item,
what we would have to build. It is input for a planning session, not a plan:
each candidate ends with the open questions that must be decided before it can
become a work package.

**Sourcing caveat.** Everything below is drawn from their published documents —
`README.md`, `ARCHITECTURE.md`, `VISION.md`, `VISION_MODERATION.md`, the custom
NIP specs under `docs/nips/`, and the crate listing. Their source was not read
line by line. Concrete constants quoted here (frame sizes, caps, kind numbers)
are good enough to reason about and to use as an audit yardstick, but must be
re-verified against their code before anything of ours depends on an exact
value.

## 2. Licensing and provenance

Buzz is Apache-2.0, "Copyright 2026 Block, Inc.", with no `NOTICE` file in the
repository. MoltRepublic is **GPL-3.0-or-later** — `COPYING` is the plain
GPLv3, and the workspace manifest carries `license = "GPL-3.0-or-later"`. Not
Affero.

Apache-2.0 is **one-way compatible** with GPLv3: Apache-2.0 code may be
incorporated into a GPLv3 work, and the combined work is then GPLv3. The
reverse is not permitted. The direction we care about is the open one.

What this document proposes, though, is almost never a code copy. It is reading
published specifications and reimplementing them against our own trust model —
usually with different primitives, and in S1's case with a deliberately
different binding. That raises no licensing question at all: a protocol design,
an architecture, an ordering of validation steps are not copyrightable; only
their expression is. Quoting a passage with attribution, as this document does
throughout, is ordinary citation.

Two obligations are worth writing down before anyone reaches for one of their
files:

- **If we ever vendor actual Apache-2.0 source**, §4 of that licence applies:
  keep the copyright, patent, trademark and attribution notices, ship a copy of
  the licence, and mark modified files as changed. There is no `NOTICE` file
  today, so §4(d) is moot — verify that at the time rather than trusting this
  sentence.
- **If we lift substantial spec text** into our own documents instead of
  paraphrasing, that is expression: attribute it and name the licence.

One counterintuitive point to carry into the planning session. Apache-2.0 §3
grants an express patent licence, and that grant travels **with the code**.
Reimplementing an idea from the spec without taking any of their source means
we never receive it. Block holds a substantial patent portfolio, so for a
component where we would otherwise clean-room a design of theirs, taking the
Apache-2.0 code can be the *safer* choice rather than the riskier one — the
opposite of the usual instinct. Weigh it per component; it argues for vendoring
in some places and changes nothing in most.

Separately, and not a Buzz question at all: under GPLv3 someone who runs a
modified MoltRepublic as a hosted service for others is not obliged to publish
their changes, whereas AGPL §13 would require it. Whether that gap matters is a
product decision, not a licensing accident to be corrected silently.

None of this is legal advice. It is here so the question is answered once, in
writing, rather than re-litigated per work package.

## 3. What Buzz is

A self-hosted workspace that merges chat, project management, code review, CI/CD
and agent orchestration into one event log. Rust monorepo, 27 crates, Nostr
NIP-01 wire format, Axum relay, Postgres for storage and full-text search, Redis
for pub/sub and presence, MinIO/S3 with the Blossom media protocol. Clients are
Tauri + React on desktop, Flutter on mobile, plus an agent-first JSON-in/JSON-out
CLI.

The relay is the architectural centre: *"The relay is the single source of
truth."* There is no peer-to-peer exchange and no gossip. One relay process per
community; in hosted multi-tenant deployments the community is derived from the
connection host, before any handler sees data, and unknown hosts fail closed.

Their layering discipline matches ours closely enough to be worth noting:
`buzz-core` is a zero-I/O crate (no tokio, sqlx, redis or axum) holding types,
signature verification, filter matching and the kind registry, with every other
crate above it and cross-subsystem coordination happening only through the
relay. That is the same shape as `molt-core` and the single-owner engine.

Their positioning line for agents is good and we should be aware of it, since it
is the same ground we stand on: *"agents are members, not bots"* — individual
keys, audit trails and scoped permissions identical to human teammates.

## 4. The dividing axiom

Buzz has deliberately not built end-to-end encryption, and says why:

> "No end-to-end encryption (yet): Current model uses TLS in transit and
> delegated at-rest encryption; E2E for DMs remains a future consideration."

> "Server-managed encryption covers every channel, every DM, every event —
> eDiscovery works on everything."

This is a coherent choice for a corporate workspace and an incoherent one for
ours. It cascades into their whole design:

- **Access control is channel membership, enforced by the relay.** Every read
  and write is gated by a Postgres membership check. Ours is the MLS group plus
  the sealed roster; no server is trusted with the decision.
- **Search is server-side Postgres FTS** over a generated `tsvector` column.
  Possible only because the relay holds plaintext.
- **The audit chain is a server-written hash chain.** Ours is threshold-signed
  and position-bound, written by the members.

So the rule for reading their repo: a feature that touches *who may see what* is
theirs and not transferable. A feature that touches *how a protocol extension is
shaped, specified, bounded or synchronised* is very likely transferable, because
those problems are the same on both sides of the encryption line.

## 5. Adoption candidates

Ranked by value as the analysis stood, before the planning session. Each item
opens with the verdict it was given on 2026-08-01; the ranking below it is left
as written, because two of the items were reordered or removed by decisions
rather than by argument, and that is worth being able to see. §8 summarises.

### S1 — Agent access without a roster seat (their NIP-AA / NIP-OA)

**Decided (2026-08-01): dropped.** There are no agents, only seats. From a
member's point of view it is irrelevant whether a human types or that member's
agent acts in their name: a member either operates the app or instructs an
agent to operate it for them, under the same seat and the same keys. Safety
comes from the m-of-n threshold — other seats must approve — not from roles,
so the delegate rung Buzz needs has no job here. The chain-height binding falls
with it; it was the repair of an expiry that no longer exists. The agent runs
on the member's own machine and drives the app through MCP, which co-equality
already guarantees. No work package.

**Their design.** NIP-OA defines an optional `auth` tag carried on an ordinary
event: `["auth", "<owner-pubkey-hex>", "<conditions>", "<sig-hex>"]`. The owner
signs a preimage over the agent's pubkey and a condition string; conditions
restrict event kind and a creation-timestamp window, e.g.
`kind=1&created_at<1713957000`. NIP-AA then carries that credential inside the
NIP-42 AUTH event (kind 22242), so an agent authenticates against the relay
using its owner's membership and receives "virtual membership" — connection-level
access derived from ownership, **with no persistent agent record created**.

Two properties carry the whole idea:

> "Clients MUST treat the agent key in `event.pubkey` as the only author key for
> the event."

Authorship is *not* delegated. The agent signs its own events with its own key;
the `auth` tag is provenance and authorization evidence, never an identity
override. Relays need not validate it and must never rewrite authorship based on
it. And compromise of an agent key does not compromise the owner key.

**Why this matters to us.** We have no answer for "I want to attach my coding
agent to this republic". Under the current model an agent would need a
three-anchor roster seat — `(name, identity_pk, nostr_pk)` — which means an
`m`-of-`n` membership change, a re-sealed table and a chain block. That is the
correct weight for a *member* and absurd weight for a tool one person runs for a
week. NIP-OA is exactly the missing middle rung: a scoped, expiring, revocable
credential minted unilaterally by a single seat holder, under which the agent
acts *as itself*, attributable to the seat that vouched for it.

It also composes with our co-equality rule rather than fighting it. An agent
holding an owner attestation is not a new privilege tier; it is a principal
whose authority is bounded by, and never exceeds, one member's authority.

**What we would build.** An Ed25519 attestation over a versioned canonical
preimage — `molt-agent-auth-v1 ‖ agent_pk ‖ conditions` — signed by a member's
roster `identity_pk`, so every other member can verify it against the sealed
roster with no new trust anchor. Delivered in-band and checked at ingest.

**Where we can beat them.** They concede the flaw themselves: an agent can
backdate `created_at`, so a `created_at<` clause is only advisory and any party
wanting wall-clock expiry must enforce it independently. We do not have to live
with that. **Bind conditions to chain height, not to wall-clock time.** Height is
objective, monotone and threshold-signed; an attestation valid `until height H`
cannot be extended by lying about a clock, and revocation has an obvious home —
a chain block that names the credential. This is the single clearest place where
our state model is strictly stronger than theirs, and it should be in the design
from the first test.

**Effort.** Medium. New canonical byte layout (versioned, byte-pinned), ingest
validation, a `Command` pair for mint/revoke that must land on both surfaces per
co-equality, and UI to show which seat vouched for which agent.

**Open questions at the time** — closed by the decision above.
- Does an agent get an MLS credential of its own, or does it ride the owner's
  MLS client? The former means a real group member and a commit; the latter
  means the owner's device must be online to relay. This is the fork that
  decides the whole work package.
- What is the condition grammar? Their `kind=…&created_at<…` is minimal; ours
  would want at least channel scope and command scope.
- Does a revoked or expired agent's past output stay valid history, or is it
  retroactively marked? (Our answer should probably be: stays valid, because it
  was authored by the agent and vouched for at the time.)
- Is an agent allowed to sign chain approvals at all? Strong default: **no** —
  governance stays with human seats.

### S2 — Cross-device read state (their NIP-RS)

**Decided: adopt, reshaped.** One machine per seat makes the cross-device sync
moot, so almost none of NIP-RS survives. What does survive came out of the
inspection: the unread ledger lives in `crates/molt-ui/src/lib.rs`
(`UnreadLedger`, in-memory, its own comment calls persistence "the B5 stretch
package"), and the word *unread* appears nowhere in `molt-engine` or
`molt-mcp`. So it dies on restart, and an agent driving the same seat cannot
ask what is new — the question every agent session opens with. Move it
engine-side, address by `MessageId` (it keys by position today, which
compaction shifts), and persist it. The GUI derives its counter from the
cursor; the agent gets the messages after it.

**Their design.** Read positions sync across a user's devices through encrypted
`kind:30078` addressable events. Each client has a stable `client_id` and
publishes to its own coordinate under a random slot identifier, so devices never
overwrite each other. The merge rule is grow-only:

> "The effective read timestamp for each context is the maximum timestamp across
> all blobs."

Content is encrypted with NIP-44 encrypt-to-self, so the relay sees only a
`read-state:` prefix and never learns which contexts a user reads or when.
Context identifiers are opaque by default — no interoperability burden. Clock
skew is handled by incrementing beyond the fetched maximum rather than trusting
a local clock.

The subtle part is the **manual mark-unread layer**, which cannot be a
timestamp. It is a CRDT: three counters per manually-marked context (set, clear,
baseline), merged by componentwise maximum. A context is unread only if the set
counter exceeds the clear counter *and* the effective read frontier has not
advanced past the recorded baseline. Tombstone floors preserve counter ceilings
so a stale snapshot cannot resurrect a register that was already cleared.

**Why this matters to us.** This is verbatim our deferred B5 (unread
persistence), already thought through by someone who hit the failure modes. We
already have `WorkspaceEvent::ChatRead` and batched read recording in
`molt-engine/src/chat.rs`, so the frontier half is largely there. What we do not
have is persistence across restarts and the manual-unread problem.

And the manual-unread problem is exactly the class of bug that has bitten this
codebase before: WP4a's id/position traps and the legacy-id determinism
keystones are the same shape — a naive "last write wins" silently resurrects
state that a peer already retired. Their counter-plus-tombstone-floor
construction is the known-good answer; taking it is cheaper than rediscovering
it.

**What we would build.** Persist the read frontier per context; add the
set/clear/baseline counter triple for manual marks with componentwise-max merge
and tombstone floors. Ours is easier than theirs in one respect — inside an MLS
group we do not need encrypt-to-self blob gymnastics to keep it private — and
harder in another: our multi-device story does not exist yet (see S9), so the
per-`client_id` slot design is only fully exercised once it does.

**Effort.** Small-to-medium, and mostly test work: the merge rules want a
property test that throws arbitrary interleavings at them.

**Open questions at the time** — closed by the decision above.
- Is read state per-member-private or shared with the group? Note our read
  receipts already publish per-message read dots on own sent messages, so some
  of this is *deliberately* shared. Where is the line?
- Does read state belong in the ephemeral log or does it need its own store? It
  is emphatically not chain material.

### S3 — Push notifications without metadata leakage (their NIP-PL)

**Decided: parked, with one rule fixed now.** No mobile client, so there is
nothing to build; a desktop node either runs and notifies natively or catches
up on reconnect under the delivery guarantee. The rule that must survive to the
day mobile arrives: a notification carries exactly one meaning, *reconnect* —
never content, never a sender name. That is the constraint someone relaxes two
years later because "just the sender" seems harmless, and that is how the
social graph ends up at Apple.

**Their design.** A "push lease" is a signed, expiring authorization telling a
relay to watch events on a client's behalf while its socket is closed, and to
wake it through the platform push channel. It is `kind:30350`, addressable,
encrypted to the executor's pubkey, carrying a random per-origin installation
ID, a mandatory expiry, and encrypted subscription filters.

Three safeguards define it:

1. Event content never transits Apple or Google servers — only a fixed reconnect
   signal.
2. Leases cannot amplify into firehoses: narrowed filters, bounded quotas,
   exact-match-only selectors.
3. Installations are sovereign and independent across devices.

> "This NIP defines exactly one notification meaning: reconnect to locally
> configured relays."

No rich previews, no relay-supplied notification text, no read receipts, no
durable delivery. The push transport is explicitly lossy and best-effort; the
relay remains the authoritative event source. This is one of the two specs they
considered worth a formal model (`docs/formal/nip-pl/`).

**Why this matters to us.** We have no push at all, and for a privacy-first
product this is the *only* correct shape: the push payload carries exactly one
bit of meaning, so a compromised or curious push provider learns "this
installation has something waiting" and nothing else — not who wrote, not in
which republic, not what about. Any design that puts a sender name or a message
preview in the notification would leak the social graph to Apple and Google
permanently, which for our threat model is disqualifying.

Worth noting they arrived at the restrictive design for product reasons (mobile
sockets die in seconds) and it happens to be privacy-optimal. We should take the
constraint as a hard rule, not as a v1 simplification to be relaxed later.

**Effort.** Large, and gated on having a mobile client at all. Lower priority
than S1/S2 in sequencing, higher in "decide the shape before anyone builds
something worse."

**Open questions at the time** — closed by the decision above.
- Does a lease go to a relay we do not trust? The lease encrypts its filters to
  the executor, but the executor still learns activity timing per installation.
  Is that acceptable, or does it want a dedicated gateway?
- Interaction with Tor: our transport posture assumes onion routing; a push
  gateway is a clearnet dependency and needs the same acknowledgement gate as
  clearnet relays.

### S4 — Kind-space discipline

**Decided: adopt, small.** We use 443, 444, 445 and 1059; exactly one of them
is a named constant (`KIND_GROUP` in `crates/molt-net/src/ritual_net.rs:45`,
module-private) and the rest are bare numbers in code and comments. A
`kinds.rs` with named constants and a uniqueness test, landed before N5 adds
further frame types.

**Their design.** The kind integer is the sole routing key, and adding a feature
means adding a kind — *"a zero-migration extensibility model."* They partition
the space explicitly: 0–9999 standard, 20000–29999 ephemeral (never stored,
never audited), 30000–39999 parameterized replaceable, 40000–49999 Buzz custom.
`buzz-core` exports `pub const ALL_KINDS: &[u32]` with 81 entries, and kind
22242 (AUTH) is structurally forbidden — never stored, never audited, never
counted in `ALL_KINDS`.

**Why this matters to us.** We currently use kind 445 as a solitary constant for
ritual frames, with no registry. As N5 lands more frame types this is exactly
where a quiet collision or an accidentally-persisted ephemeral frame becomes a
bug. A `kinds.rs` in `molt-net` with a pinned constant set, banded ranges and a
test asserting the set is complete and disjoint is a cheap, boring investment
that pays the first time two work packages allocate in parallel.

The structural-forbid idea is the part worth copying carefully: not "we do not
store AUTH" as a convention, but as a rejection that a test proves.

**Effort.** Small. Good candidate for a warm-up task at the start of the plan.

### S5 — Specs with fixtures as a standing rule

**Decided: adopt as a standing rule, not as a project.** No retrofit of the
existing layouts. Every *new* canonical byte layout gets prose plus
accept/reject fixtures from the start.

**Their design.** Fifteen custom NIP specs live in `docs/nips/`, and at least one
ships machine-readable conformance data alongside the prose: `NIP-MP.fixtures.json`
covers 31 relay ingest cases (11 accepted, 20 rejected — the 64-member cap,
malformed coordinates, duplicate detection), and `NIP-MP.fold-fixtures.json`
covers 12 client-side rendering cases pinning a deterministic output. Their
justification is the best sentence in the repository:

> "a divergence between them is a test failure rather than a production
> surprise."

Two of the specs additionally carry formal models (`docs/formal/nip-pl/`,
`docs/formal/nip-rs-unread/`) — notably the two whose merge/expiry semantics are
hardest to reason about informally.

**Why this matters to us.** We already have the instinct: byte-pin tests on
`molt-roster-v3`, `molt-republic-id-v2`, `molt-chain-checkpoint-v2` and the chat
wire fixtures exist precisely so a layout change goes red. What we do not have is
the *prose spec beside the fixture*, and that is what makes a layout reviewable
by a human and implementable by a second party. Our canonical layouts are
currently documented in CLAUDE.md warnings and in code comments; they deserve
`docs/spec/` entries with their accept/reject fixture sets.

The determinism requirement they state for the fold — *"same heads in, same
collection out, independent of arrival order or query shape"* — is the same
property our chain and chat projections need and mostly have. Writing it down as
a testable contract is the gap.

**Effort.** Small per layout, ongoing as a habit. Best adopted as a rule that
applies to every *new* canonical layout (starting with S1's agent attestation)
rather than as a retrofit project.

### S6 — Their server limits, mirrored as our client defences

**Decided: adopt, narrowed — the IP checklist is dropped.** Verified:
`IpAddr`, `Ipv4`, `Ipv6`, `to_socket_addrs` and `lookup_host` appear nowhere in
`molt-net`. Hostnames are handed to the SOCKS proxy and resolved inside the Tor
circuit — that *is* the no-leak property, and resolving them ourselves would be
the leak. The only literal IPs in the crate are our own proxy endpoints. Buzz
needs SSRF guards because their workflow engine fetches arbitrary URLs; we have
no webhooks. What remains, and remains fully: the hostile-relay bounds — frame
size, event flood against a single REQ, subscription count, and a connection
that only dribbles.

Residual, noted rather than planned: an invite ticket carries attacker-supplied
relay URLs (`invite.rs`), and in `Direct` (clearnet) mode a `ws://localhost:…`
entry would be dialled by the OS resolver. Under Tor it is moot, since Tor
refuses private ranges. The defence at that layer is the WHATWG parser gate,
which is already in place after the two CRITICAL host-parser bugs.

**Their design.** As documented: `MAX_FRAME_BYTES = 65_536`, `MAX_SUBSCRIPTIONS =
1024` per connection, `MAX_HISTORICAL_LIMIT = 500` per filter, a handler
semaphore of 1024 concurrent EVENT/REQ, a connection semaphore, and a slow-client
grace counter that cancels a connection after 3 consecutive full send buffers.
Plus input validation: 64-char hex validation on event ids to prevent path
injection, alphanumeric-and-underscore-only workflow step ids to prevent
`evalexpr` injection, and a table-name allowlist with strict date validators for
partition DDL.

Their `is_private_ip()` SSRF guard covers IPv4 unspecified, loopback, private,
link-local, CGNAT, benchmarking and broadcast ranges; IPv6 loopback, ULA,
link-local, multicast and documentation ranges; and **recursively checks the
embedded IPv4 of IPv4-mapped IPv6 addresses**.

**Why this matters to us.** They are defending a server against hostile clients.
We are the client against hostile *relays*, which is the mirror image of the same
problem and one we have not systematically audited. A malicious relay can send us
an oversized frame, an unbounded EVENT flood against one REQ, or a slow-loris
dribble that pins a connection forever. `relay_ws.rs` should be reviewed against
this list and given explicit bounds with named constants.

The SSRF range list is a free checklist, and given this project's history — two
CRITICAL bugs in a hand-rolled URL host parser where a backslash and a
`userinfo@` component defeated the onion/clearnet gate — the IPv4-mapped-IPv6
recursion case is precisely the kind of edge we should confirm we handle.

**Effort.** Small-to-medium; mostly an audit with tests, no new architecture.

### S7 — A relay conformance probe

**Decided: adopt as a product feature.** Mandatory at add-time, so an unusable
relay never enters the list rather than being diagnosed later. The result is a
verdict plus the single reason it failed, not a full protocol log. A `Command`
on both surfaces, so an agent can vet a relay too.

**Their design.** A dedicated `buzz-conformance` crate plus
`docs/multi-tenant-conformance.md`: a suite that can be run against a relay to
verify it behaves to spec, including tenant isolation.

**Why this matters to us.** For them this is testing. For us it is a *product
feature*. Relay selection is currently blind: a user adds a relay URL and finds
out later, through failures, whether it accepts our kind, honours NIP-42, retains
events long enough for a join ritual, or silently drops large frames. A probe
that answers those questions *before* the relay is added — and reports in one
scannable line per check — turns a class of confusing runtime failures into an
upfront, actionable result.

This also lines up with the diagnostics work already on master (`88dd854`,
`f4d698d`: a refused join names the relay that blocked it; a failed connection
says which relay, which route and why). A conformance probe is the proactive
half of the same effort.

**Effort.** Medium, and unusually high value per unit of work because it reuses
the dialer and the WS client we already have.

**Open question at the time** — closed by the decision above: it is a `Command`
on both surfaces, and it runs as a mandatory step of adding a relay rather than
as an optional check.

### S8 — The moderation model

**Decided: rejected — we already hold the two principles that matter, and hold
them harder.** Their model rests on an Owner/Admin/Member/Guest/Bot hierarchy
we do not have and do not want. Enforcement at the identity layer is already
ours: removing a member is a `ChainChange::Membership`, decided m-of-n rather
than by admin fiat. Honest tombstones are already ours: a delete leaves a
tombstone that keeps `from`, carries neither reactions nor read receipts, and
still counts toward ordinals. One UI decision taken here: the tombstone stays
visible and names who deleted.

Their principle binds harder for us than for them. Every member holds the group
key and runs their own client, so a render-time filter is theatre — swap the
client and it is gone. Only what the group cryptographically refuses is
actually enforced.

**Their design** (`VISION_MODERATION.md`) splits community moderation
(subjective, per-community, owner/admin authority, never extends beyond the
community boundary) from platform safety (illegal content, network abuse, legal
obligations; escalation only). Load-bearing details:

- Members report content with categories and optional context. **Reports are
  private** — never broadcast, never public events, visible only to those with
  authority to act.
- *"Reports are signals, never triggers."* No automatic removal; human judgement
  gates every enforcement action.
- Enforcement executes **at the identity layer**: bans are checked during
  authentication, timeouts are write-blocks with expiry timers. Explicitly not a
  render-time filter, because a filter is trivially bypassed by a custom client.
- Removed content shows an honest tombstone ("removed by a community moderator")
  with a sanitized reason, rather than vanishing.
- The restricted user is told what happened and for how long; the reporter is
  told the outcome, closing the loop.
- Bans, timeouts, dismissals and escalations all create durable audit records,
  separating the decision from the enforcement mechanism.

**Why this matters to us.** We are building a governance product and have no
moderation surface at all. Three of their principles translate directly and are
worth adopting as stated: reports private, reports never auto-trigger, and
enforcement at the identity layer rather than the client. That last one is
sharper for us than for them — in a system where every member runs their own
client and holds the group key, a client-side filter is *not moderation at all*.
The only enforcement that means anything is one that changes what the group
cryptographically accepts, which for us means a chain block and, at the limit, an
MLS commit removing a member.

The honest-tombstone rule is also a good fit for a chain-backed log, where
deletion cannot be retroactive anyway.

**Effort.** Large, and genuinely a product-design conversation before it is an
engineering one. Their split between "subjective community rules" and "platform
safety" may not map at all onto a self-hosted republic with no platform operator
— that is a real question, not a detail.

### S9 — Smaller items worth logging

**Decided: mesh transfer stays, Blossom is not adopted.** No third party sees
even encrypted blocks, no new dependency, and the delivery guarantee already
covers the path. Device pairing is moot under the S1 decision — kept on record
for the day mobile or a server-resident agent arrives. Git-over-Nostr, huddles
and the YAML workflow engine stay out of scope; the workflow engine especially,
because with agents as full seats the automation *is* the agent, and a
declarative layer beside it would rebuild the same capability more weakly.

- **Device pairing.** They ship `buzz-pair-relay` and `buzz-pairing-cli` as
  dedicated components. We are single-device by construction: the workspace key
  is sealed under `~/.moltrepublic/device.key`. Multi-device is a real gap and
  substantially harder for us than for them, because a second device is a second
  MLS client (or a shared ratchet, which is worse). Worth scoping, not worth
  starting blind.
- **Blossom media.** SHA-256-addressed blobs, `PUT /media/upload`, `GET
  /media/{sha256_ext}`, 50 MB cap. As a relay-agnostic offload for large files —
  with our own encryption applied before upload — this is more scalable than
  pushing everything through chunked mesh transfer, and it is an existing
  standard rather than a hand-rolled one.
- **Git over Nostr.** NIP-34 git events, `git-sign-nostr`, `git-credential-nostr`,
  git smart-HTTP on the relay, and `docs/git-on-object-storage.md`. Far outside
  current scope, but it is the clearest example of their "one workspace, one
  identity" thesis and worth knowing exists.
- **Huddle audio frame format.** Opus over WebSocket with an 8-byte header
  (sequence `u16`, 48 kHz timestamp `u32`, level dBov `i8`, flags `u8`), soft cap
  25 peers and hard cap 255 from the `u8` peer index, per-peer bounded channels
  that drop on full, and invalid levels clamped rather than dropping the frame.
  If voice inside an MLS group is ever on the table, this is a free starting
  design.
- **Workflow engine shape.** YAML-as-code with 4 trigger types (`message_posted`,
  `reaction_added`, `schedule`, `webhook`) and 7 actions, template variables with
  single-pass resolution and no recursion, `evalexpr` conditions with a 100 ms
  timeout, a 100-permit semaphore that returns `CapacityExceeded` immediately
  rather than queueing, and approval tokens that are CSPRNG UUIDs stored
  SHA-256-hashed and enforced single-use via `AND status = 'pending'` in the
  UPDATE. Take the *shape*, not the substance — see §7. Our governance chain is
  already the approval engine; what is missing is a declarative surface over it.
- **Product language.** *"Zero is the default. You opt in to noise, not out."*
  and *"one workspace, one URL, one identity system"*. Both are lines we could
  have written and have not.

## 6. What does not transfer

- **Relay as source of truth; membership as access control.** Their entire
  authorization model is server trust. Ours is MLS plus the sealed roster.
- **The Postgres / Redis / S3 operational stack.** Correct for a hosted
  multi-tenant service, wrong for a self-hosted republic.
- **Server-side full-text search.** Their `search_tsv` generated column with
  `CASE WHEN kind IN (1059, 30300, 30622) THEN NULL` to exclude privacy-sensitive
  kinds is a patch over a design we do not have and must not acquire. If we want
  search it is a local index over decrypted material, and that is a different
  project with different problems (index-at-rest sealing, in particular).
- **Delegated at-rest encryption for eDiscovery.** Directly contrary to the
  product.
- **Their audit hash chain.** SHA-256 over `(seq, timestamp, event_id, kind,
  actor, action, channel_id, canonical metadata, prev_hash)`, single-writer via
  `pg_advisory_lock`, genesis of 64 zeros. Competent, and strictly weaker than a
  threshold-signed, position-bound chain written by the members themselves.
  Their 10-action taxonomy (`EventCreated`, `EventDeleted`, `ChannelCreated`,
  `ChannelUpdated`, `ChannelDeleted`, `MemberAdded`, `MemberRemoved`,
  `AuthSuccess`, `AuthFailure`, `RateLimitExceeded`) is a useful checklist for
  what a governance log should be able to express, and that is all.
- **Their NIP-42 scope handling.** A successful AUTH grants `Scope::all_known()`
  — all 14 scopes at once. That is precisely the flattening that NIP-OA's
  condition strings get right, and we should not reproduce it: authority should
  be per-credential and bounded, everywhere.

## 7. Reality check before adopting anything

Their README shows ✅ where their own limitations table shows 🚧. From that
table:

| Area | Reality |
|---|---|
| Rate limiting | Not enforced. The `RateLimiter` trait exists; the only implementation is `AlwaysAllowRateLimiter`, behind a test/dev feature. The 4-tier `RateLimitConfig` is a design target. |
| Workflow approval gates | Not wired end-to-end. The executor returns `StepResult::Suspended` and runs that hit a gate are marked `Failed` (their WF-08). |
| Workflow actions | `send_dm` and `set_channel_topic` return `NotImplemented` (WF-07). |
| Huddle recording / per-track publishing | Kinds reserved, no producer. |
| sqlx | Runtime `sqlx::query()` only; no offline compile-time verification. |

So: read their specs as specs and their code as a work in progress. The specs
are where the value is — several are more finished than the implementations
behind them, which is the opposite of the usual failure mode and rather to their
credit.

## 8. Decisions

Walked through candidate by candidate on 2026-08-01; each verdict is recorded
above, at the item it belongs to. Summary:

| | Verdict |
|---|---|
| S1 agent access | **Dropped** — no agents, only seats; the agent drives its own seat locally over MCP, which exists |
| S2 read state | **Adopt, reshaped** — engine-side, `MessageId`-addressed, persisted; GUI derives the counter, the agent gets the messages |
| S3 push | **Parked** — no mobile client; the notification-carries-no-content rule is fixed now |
| S4 kind registry | **Adopt** — named constants plus a uniqueness test |
| S5 spec with fixtures | **Adopt as a rule** — every new byte layout, no retrofit |
| S6 client bounds | **Adopt, narrowed** — hostile-relay limits only; the IP checklist does not apply to us |
| S7 relay probe | **Adopt** — mandatory at add-time, verdict plus reason, both surfaces |
| S8 moderation | **Rejected** — both principles already held, and held harder |
| S9 remainder | **Mesh stays**, Blossom declined; pairing, git, voice and YAML recorded only |

Four work packages fall out — S2, S4, S6, S7 — plus S5 as a standing convention
and two small rules (S3's payload constraint, S8's visible tombstone). They are
planned in `buzz_followups.md`.

Two of the decisions are worth remembering for their shape rather than their
content. S1, which this document ranked first, was removed outright by a
product decision that made Buzz's cleverest construction unnecessary — their
delegate credential exists to tell a relay who is knocking, and no relay
decides anything here. And S2, ranked second for what Buzz had built, turned
out to matter for something else entirely: the inspection it prompted found
that our unread state is GUI-only and invisible to the MCP surface, which the
agent decision had just turned into a blocker. The comparison earned its keep
by what it made us look at, not by what it let us take.
