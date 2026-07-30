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
| `url` | `wss://…` (or `ws://…` for `.onion` only — see §2) |
| `confirmed` | the user's persisted "yes, use this relay" |

**`kind` is never stored.** Whether a relay is onion or clearnet is *derived*
from its URL every time it is needed. Storing it would create a second source
of truth that a hand-edited config could contradict — a clearnet relay
labelled `onion = true` would silently bypass the confirmation gate.

## 2. URL validation — an allow-list, because a parser disagreement IS the bug

- scheme must be `wss://` or `ws://`; there must be a host
- **the authority is validated by allow-list**: letters, digits, `-` and `.`
  in the host, plus an optional numeric port. Everything a real URL parser
  treats specially is refused — `@` (userinfo), `\`, `%`, `[` `]`, non-ASCII.
  A trailing dot and empty labels are refused too, so one host has exactly one
  spelling
- **`Onion` requires a real v3 address**: 56 base32 characters (`a-z2-7`)
  before `.onion`. Merely *ending* in `.onion` does not earn the auto-dial
  privilege; a host that claims `.onion` without the right shape is refused at
  ingest rather than quietly demoted to clearnet (a clearnet badge and an
  IP-exposure warning would both be lies about an address that cannot resolve)
- `ws://` (plaintext) is allowed **only** for such an onion host, where the Tor
  circuit already provides encryption and authentication. Plaintext clearnet is
  refused outright — it would publish every subscription in the clear
- no whitespace or control characters; no duplicate URL in one pool

**Why an allow-list and not a delimiter blacklist** (found by the 2026-07-31
adversarial review, two CRITICAL findings): the classifier decides whether
something is dialed *with no user interaction*, so it must never disagree with
the parser that actually dials. The first implementation ended the authority at
`/ ? #` only and treated everything before the last `:` as the host. Both
`wss://evil.example.org\x.onion` and `wss://abcd.onion:1234@attacker.example.org`
therefore classified as **Onion** here while WHATWG — what every WebSocket and
Nostr client uses — resolves them to `evil.example.org` and
`attacker.example.org`. Result: a green onion badge, no acknowledgement, no
session lock, and an automatic clearnet connection on every start. Any future
change to this parsing must keep the allow-list posture and re-run
`onion_classification_cannot_be_spoofed`.

**`relay_kind` is independently strict.** It does not assume its input passed
ingest, because `config.toml` is hand-editable and reaches the pool without it.
Anything it cannot parse as a well-formed onion authority counts as clearnet —
the side of the gate that asks first.

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
on screen rather than hidden; clearnet still needs the per-session activation,
which no file can grant; and invalid URLs never reach a dial path.

## 3. The gate — three rules, one pure function

`molt_core::relay::dialable(pool, clearnet_session)` returns the relays the
runtime may dial, in priority order. It is pure, lives in `molt-core`, and is
the ONLY place the policy exists — the N2 relay runtime must consume it rather
than read the pool directly.

1. **Unconfirmed is never dialed.** No confirmation, no connection — this is
   what makes a fresh install offline.
2. **Onion relays may be dialed automatically**, including at startup and in
   background reconnects.
3. **A clearnet relay is never dialed automatically.** On top of its persisted
   `confirmed` flag it needs an *in-session* activation (`clearnet_session`),
   which resets to off on every start. So "always a warning and an explicit
   confirmation before a clearnet connection" holds literally: after a
   restart, no clearnet packet leaves the machine until the user acts again.

Tor does not waive rule 3. Routing a clearnet relay over Tor hides the node's
IP from the relay operator, but it is still a clearnet endpoint with a
clearnet operator; the user decides, and the warning names which of the two
situations they are in.

## 4. Command surface (co-equal on GUI and MCP)

Adding, ordering, confirming and activating relays are **human decisions**, so
they are tools on both surfaces (co-equality rule) — never engine-internal:

| command | effect |
|---|---|
| `RelayAdd { url }` | validate + append, **unconfirmed** |
| `RelayRemove { url }` | drop the entry |
| `RelayMove { url, up }` | change priority by one position |
| `RelayConfirm { url, accept_clearnet }` | persist the confirmation; the engine **refuses** a clearnet URL unless `accept_clearnet` is true |
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
