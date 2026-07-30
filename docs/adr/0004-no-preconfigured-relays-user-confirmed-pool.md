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
- The per-session clearnet activation is runtime-only state
  (`State::clearnet_session`) and must never be persisted.
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
  the pool is always visible in the settings, that clearnet still needs the
  per-session act no file can grant, and that unusable URLs are dropped at
  ingest (`relay::sanitize_pool`).
