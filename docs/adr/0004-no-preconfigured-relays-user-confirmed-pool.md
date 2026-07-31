---
status: accepted
---

# No pre-configured relays: an empty, user-confirmed pool with a clearnet gate

**Supersedes** the "curated onion default list" part of
[ADR-0003](0003-nostr-relay-policy-open-choice-onion-default.md). Everything
else in ADR-0003 (any relay allowed, self-hosting recommended, NIP-42 AUTH in
scope, honest badges) stands. Full design:
`docs/transport/relay_pool.md`.

**Context.** ADR-0003 decided the product would ship a *curated set of onion
relays* as the default pool, to spare founders the onboarding friction. The
user rejected that on 2026-07-31: a shipped relay list is a shipped
surveillance point, and it makes every MoltRepublic node identifiable by its
first outbound packet — the curated operators would see the whole install base
appear, and a compromised or coerced entry in that list would be a
single-point metadata leak for every republic that never touched its settings.

**Decision.** The app ships with **zero relays** and connects to nothing until
its operator has added a relay **and confirmed it**. On top of that:

- **Automatic/background connections go to `.onion` relays only.** A confirmed
  onion relay is dialed on start and on reconnect without asking again.
- **A clearnet relay is never dialed automatically.** Confirming one requires
  an explicit acknowledgement of the exposure (enforced in the engine, so an
  MCP agent faces the same gate as a human), and dialing it additionally
  requires an **in-session activation that does not survive a restart** — so
  "always a warning and an explicit confirmation before a clearnet connection"
  holds literally, not just the first time.
- The pool is **ordered, and the order is the dial priority**.
- The onion/clearnet kind is **derived from the URL, never stored**, so a
  hand-edited config cannot mislabel a clearnet relay past the gate.
- Plaintext `ws://` is accepted for `.onion` hosts only.
- **Only a real v3 onion address earns the automatic dial** (56 base32
  characters before `.onion`), and the authority is validated by allow-list.
  Added 2026-07-31 after the adversarial review proved two spoofs — a
  backslash (`wss://evil.example.org\x.onion`) and a userinfo component
  (`wss://abcd.onion:1234@attacker.example.org`) — that our host parser read
  as onion while every real client dials the clearnet host. A classifier that
  grants "connect without asking" must never disagree with the parser that
  connects; see `relay_pool.md` §2.

**Why.** A default that leaks is not a default anyone chose. Onboarding
friction is real but recoverable (the operator pastes one URL); a default
surveillance point is neither visible nor recoverable. Making the onion path
the *frictionless* one — add, confirm, done, forever — while the clearnet path
costs an acknowledgement plus a per-session act encodes the privacy preference
in the product's ergonomics instead of in a recommendation nobody reads.

## Consequences

- The founding wizard cannot offer a working relay out of the box; a fresh
  install is deliberately offline until configured. The empty state must
  therefore be inviting and instructive, not an error.
- N2's relay runtime MUST consume `molt_core::relay::dialable(...)` rather than
  reading the pool directly — that pure function is the single place the policy
  lives, and it returns the empty set for an unconfigured node.
- ~~The per-session clearnet activation is runtime-only state
  (`State::clearnet_session`) and must never be persisted.~~
  **AMENDED 2026-08-01 — see the amendment below: the decision IS persisted.**
- `SaveSettings` deliberately ignores any relay pool in its payload: the
  `Relay*` commands are the only way in, so a settings write cannot inject a
  pre-confirmed clearnet relay past the acknowledgement.
- ADR-0003's "curated onion default" bullet is void; its recommendation that
  only a self-hosted relay avoids `h`-tag correlation is unchanged and belongs
  in the UI copy.
- A `.onion` host that is not a valid v3 address is **refused at ingest**
  rather than accepted as a clearnet relay — it cannot resolve anywhere, so
  both a clearnet badge and an IP-exposure warning would be lies. Legacy v2
  addresses (16 chars, removed from Tor) therefore cannot be added; if a
  test/alternative onion format ever needs to be supported, it must be an
  explicit decision, not a loosened parser.
- The `config.toml` pool is **trusted but sanitized**: an operator (or
  anything with write access to that file) may add *and* confirm a relay, and
  a confirmed onion relay then dials automatically. That is deliberate — the
  file is the operator's own authority — and the honest mitigations are that
  the pool is always visible in the settings, that a non-onion relay still
  needs the operator's acknowledged confirmation, and that unusable URLs are
  dropped at ingest (`relay::sanitize_pool`).

## Amendment 2026-08-01 — the clearnet decision is REMEMBERED

**What changed.** The acknowledged confirmation of a non-onion (clearnet /
LAN / loopback) relay now also *activates* non-onion dialing, and that
decision is persisted (`[transport.nostr] clearnet_enabled`). The separate
per-session unlock, which reset on every start, is gone as a *requirement*;
the switch survives as the deliberate OFF control, and switching off is
persisted too.

**Why.** The original design demanded two acts for one decision: an explicit
acknowledgement per relay (durable) plus a global activation (session-only).
In use — reported from the operator's own two-node setup — that second act
made the node unusable for its purpose: every config edit and every restart
silently revoked it, so a founding or join failed until the operator
re-performed a consent they had already given, with an error message that
did not say so. Repetition is not consent; it is habituation, which is the
failure mode informed-consent design exists to avoid. A control the operator
must re-perform forever gets clicked without reading.

**What is NOT weakened.** The consent moment is unchanged and still explicit:
a non-onion relay cannot be confirmed without `accept_clearnet`, the refusal
still names the exposure, and an unconfirmed relay is still never dialed. A
fresh install still dials nothing. `SaveSettings` still cannot set the flag
(one door: the `Relay*` commands), so neither a settings payload nor an MCP
agent can grant itself non-onion dialing.

**What IS given up, honestly.** The property "after a restart no clearnet
packet leaves this machine until a human acts again" no longer holds for a
node whose operator enabled it. That property protected against a stolen or
seized machine being restarted and immediately phoning a clearnet relay —
a real but narrow case, and one the operator can still choose by switching
clearnet off before shutdown (now remembered). Weighed against a gate that
made the product unusable and trained its operator to click past warnings,
remembering the decision is the better trade.
