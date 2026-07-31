# Design: the Nostr relay pool — user-owned, nothing pre-trusted

Status: **DESIGN + BUILD (2026-07-31)**, decided by the user on 2026-07-31.
Read with `nostr_transport_marmot.md` §7.5 (reachability) and
`docs/adr/0003-nostr-relay-policy-open-choice-onion-default.md`, whose
"curated onion default list" this document **supersedes**.

## 0. The decision

> No hard-coded relays. The app must not connect until the user has confirmed
> a relay in the config. Automatic background connections go to `.onion`
> relays only; connecting to a clearnet relay always requires a warning and an
> explicit user confirmation first.

The shipped configuration therefore contains **zero** relays. A fresh install
is deliberately offline: the republic cannot reach anyone until its operator
has named a relay and confirmed it. That is a feature, not a gap — a default
relay list is a default surveillance point, and it would make every
MoltRepublic node identifiable by its first outbound packet.

## 1. The pool

An **ordered** list of entries; the order IS the priority (position 0 is tried
first). Each entry is:

| field | meaning |
|---|---|
| `url` | `wss://…` (or `ws://…` for `.onion` and local addresses — see §2) |
| `confirmed` | the user's persisted "yes, use this relay" |

**`kind` is never stored.** Whether a relay is onion, clearnet or local is
*derived* from its URL every time it is needed. Storing it would create a
second source of truth that a hand-edited config could contradict — a clearnet
relay labelled `onion = true` would silently bypass the confirmation gate.

## 2. URL validation — the WHATWG parser, plus strictness on top

*(Rebuilt 2026-07-31 per `mdk_evaluation.md` §7.1/§7.2 — the hand-rolled
allow-list parser is gone; `url`, the parser every real WebSocket/Nostr
client dials with, is the authority for what a URL means.)*

- scheme must be `wss://` or `ws://`; there must be a host
- **the host comes from the parse** (`url::Url` — WHATWG), never from string
  slicing, so the classification can never disagree with what a client dials.
  On TOP of the parse, ingest is strict: ASCII only, no whitespace/control,
  no `\`, no credentials, no `#fragment`, ≤ 512 bytes (the MDK validator's
  bound), conservative domain labels (letters, digits, `-`; no empty label,
  no trailing dot)
- **one endpoint, one spelling**: anything the parser had to rewrite — a
  percent escape, an alternate IPv4 notation (`0x7f.1`, plain integer,
  octal, leading zeros), an odd port spelling — is refused as non-canonical
  rather than stored as a second reading of the same address. An explicit
  default port (`:443` on wss, `:80` on ws) is the one redundancy accepted,
  and it is dropped, so `wss://r:443` and `wss://r` are one pool entry. The
  path is stored canonical (dot-segments collapsed)
- **`Onion` requires a real v3 address**: 56 base32 characters (`a-z2-7`)
  before `.onion`. Merely *ending* in `.onion` does not earn the auto-dial
  privilege; a host that claims `.onion` without the right shape is refused at
  ingest rather than quietly demoted to clearnet (a clearnet badge and an
  IP-exposure warning would both be lies about an address that cannot resolve)
- **`Local` (§10.14, decided 2026-07-31)**: loopback, RFC1918-private,
  link-local and unique-local addresses, plus `localhost` names, classify as
  a third kind. A local relay is a legitimate self-host target but is reached
  DIRECTLY, never over Tor — so it rides exactly the clearnet gate: an
  explicit acknowledgement to confirm, which then also activates dialing and
  is remembered (ADR-0004 amendment 2026-08-01). Addresses
  nothing can listen on (unspecified, broadcast, multicast) are refused
- `ws://` (plaintext) is allowed for an onion host (the Tor circuit already
  encrypts and authenticates) and for a local one (no CA certifies private
  addresses; the exposure ends at the local path and sits behind the
  acknowledgement). Plaintext CLEARNET stays refused outright — it would
  publish every subscription in the clear
- no duplicate URL in one pool

**Why the real parser and not our own** (two CRITICAL findings, 2026-07-31
adversarial review): the classifier decides whether something is dialed *with
no user interaction*, so it must never disagree with the parser that actually
dials. The first implementation ended the authority at `/ ? #` only and
treated everything before the last `:` as the host. Both
`wss://evil.example.org\x.onion` and `wss://abcd.onion:1234@attacker.example.org`
therefore classified as **Onion** here while WHATWG — what every WebSocket and
Nostr client uses — resolves them to `evil.example.org` and
`attacker.example.org`. Result: a green onion badge, no acknowledgement, no
session lock, and an automatic clearnet connection on every start. The fix on
top of the fix (`mdk_evaluation.md` §6): stop hand-rolling — parse with `url`
and apply policy to the PARSED host. `onion_classification_cannot_be_spoofed`
remains the regression net for any future change here.

**`relay_kind` is one code path with ingest.** It runs the same single parse
as `normalize_relay_url`, so the classifier and ingest cannot disagree — and
it does not assume its input passed ingest, because `config.toml` is
hand-editable and reaches the pool without it. Anything the parse refuses
counts as clearnet — the side of the gate that asks first.

## 2.5 Pools that did not come through the command surface

`config.toml` is hand-editable, so its pool is run through
`relay::sanitize_pool` at BOTH ingest points (boot in `molt-app`, the watcher
reload in `molt-engine/src/configstore.rs`): URLs are normalized, unusable
entries are dropped instead of being displayed as relays, duplicates collapse,
and the file order (the dial priority) survives. Without it an unvalidated
string would also be unreachable by the `Relay*` commands, which address
entries by their normalized URL — the row could be seen but never confirmed,
moved or removed.

**The config file is a trusted input, and that is deliberate.** An operator
editing their own file may add *and* confirm a relay; the template invites
exactly that. So anything able to write `config.toml` can pre-confirm a relay,
and a confirmed onion relay is dialed automatically. What still holds: the pool
is always visible in the relay settings, so a relay the operator never added is
on screen rather than hidden; a non-onion relay still needs its acknowledged
confirmation; and invalid URLs never reach a dial path. Note the honest limit
of the 2026-08-01 amendment: since the clearnet decision is persisted, a file
that sets `clearnet_enabled = true` AND confirms a clearnet relay does grant
non-onion dialing — the file is the operator's own authority, and the pool
stays visible in the settings where such an entry can be revoked.

## 3. The gate — three rules, one pure function

`molt_core::relay::dialable(pool, clearnet_session)` returns the relays the
runtime may dial, in priority order. It is pure, lives in `molt-core`, and is
the ONLY place the policy exists — the N2 relay runtime must consume it rather
than read the pool directly.

1. **Unconfirmed is never dialed.** No confirmation, no connection — this is
   what makes a fresh install offline.
2. **Onion relays may be dialed automatically**, including at startup and in
   background reconnects.
3. **A non-onion relay is never dialed automatically** — clearnet and local
   alike (§10.14: both are reached outside Tor). On top of its persisted
   `confirmed` flag it needs an *in-session* activation (`clearnet_session`),
   which resets to off on every start. So "always a warning and an explicit
   confirmation before a connection outside Tor" holds literally: after a
   restart, no such packet leaves the machine until the user acts again.

Tor does not waive rule 3. Routing a clearnet relay over Tor hides the node's
IP from the relay operator, but it is still a clearnet endpoint with a
clearnet operator; the user decides, and the warning names which of the two
situations they are in. A local relay cannot be routed over Tor at all —
that is exactly why it sits behind the same gate.

**N2 dialer obligation (review 2026-07-31):** the plaintext allowance for
`localhost`/`*.localhost` rests on those names resolving to loopback
(RFC 6761 *SHOULD*). The N2 dialer must pin them to loopback itself —
resolve `localhost` names to `127.0.0.1`/`::1` without asking the system
resolver — before honoring the `ws://` allowance, so a non-conforming
resolver can never carry plaintext off the machine.

## 4. Command surface (co-equal on GUI and MCP)

Adding, ordering, confirming and activating relays are **human decisions**, so
they are tools on both surfaces (co-equality rule) — never engine-internal:

| command | effect |
|---|---|
| `RelayAdd { url }` | validate + append, **unconfirmed** |
| `RelayRemove { url }` | drop the entry |
| `RelayMove { url, up }` | change priority by one position |
| `RelayConfirm { url, accept_clearnet }` | persist the confirmation; the engine **refuses** a clearnet or local URL unless `accept_clearnet` is true |
| `RelayRevoke { url }` | withdraw the confirmation |
| `RelayClearnetSession { unlock }` | activate/deactivate clearnet dialing for THIS session only |

`accept_clearnet` is enforced in the engine, not in the GUI: an MCP agent
faces exactly the same gate as a human clicking through the warning dialog.
The GUI renders the warning and only then sends the command with the flag set.

**The read side is co-equal too** — `read_session` exposes the pool with its
*derived* per-entry state (`kind`, whether it would be dialed right now, and
why not), so the whole feature is drivable and inspectable head-less over
MCP: add a relay, see it unconfirmed, confirm it, watch the dial set change,
without touching the GUI. That is the developer-test path.

## 5. config.toml

```toml
[transport.nostr]
# Relays this node may use, in priority order. EMPTY BY DEFAULT — the app
# connects to nothing until you add a relay here (or in the GUI) and confirm
# it. Onion relays connect automatically; a clearnet relay additionally needs
# an explicit confirmation each session.
[[transport.nostr.relay]]
url = "wss://your-relay.onion"
confirmed = false
```

The template ships this as **shape documentation with a placeholder host**,
not a reachable address. The relay array is rewritten wholesale on save (the
entries are app-managed structured data); comments elsewhere in the file keep
their format-preserving guarantee.

## 6. What the user sees

- Settings → Nostr relays is a dedicated tab with the relay-pool editor: add, remove, reorder (the
  priority), a per-entry confirm toggle, and an onion/clearnet badge derived
  from the URL.
- Confirming a clearnet relay opens a warning that names the concrete
  exposure (the operator sees your subscriptions and, unless Tor is on, your
  IP address) and requires an explicit acknowledgement.
- With no confirmed relay the network state reads honestly as "no relay
  confirmed — not connected", never as an error or a spinner.
- Clearnet relays configured but not activated this session are shown as
  exactly that, with the activation action next to them.

## 7. Keystones

- an empty/unconfirmed pool yields an empty dial set
- an onion relay is dialed automatically; a clearnet relay is not, until the
  session is unlocked — and the unlock does not survive a restart
- `RelayConfirm` on a clearnet URL without `accept_clearnet` is refused
- `ws://` clearnet and malformed URLs are refused at ingest
- priority order survives the config round-trip
