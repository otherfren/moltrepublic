---
status: proposed
---

# Nostr relay reachability: onion by default, clearnet only with a warning

**Context.** The proposed Nostr/NIP-EE transport (`docs/transport/nostr_transport_marmot.md`)
replaces the per-pair SMP queue mesh with publish/subscribe to a shared relay
set. Unlike per-pair queues, a shared relay sees the group's `h`-tag, its
member count, and — decisively — the subscription IPs that tie those members
to real-world identities. This undercuts the privacy-first posture that
motivated SMP in the first place, so *how the relay is reached* is a load-
bearing decision, not a deployment detail.

**Decision.** A Nostr workspace's relay(s) default to **Tor onion services
reached over Tor** (both ends hidden, relay location-hidden), dialed through
the existing T4 onion-preferred, fail-closed path. The settings UI nudges this
default and steers toward a self-hosted onion relay. A **clearnet relay stays
selectable but is gated behind an explicit "insecure" warning** and a
persistent health-surface badge; Tor-off with any relay selected fails closed,
never a silent clearnet dial.

**Why.** Onion reachability is the only posture where the relay cannot see
member IPs and is not itself a findable/seizable/censorable clearnet target —
the same reason SimpleX runs its servers over Tor, and it reuses infrastructure
we already have (T4). A clearnet relay is asymmetric: dialing it over Tor still
hides the *client* IP (useful for members who do not trust the relay operator),
but leaves the *relay* exposed — hidden leaves, trunk in the clearing. The
degenerate case — tunnelling to your **own** clearnet relay to hide **your
own** IP from **your own** server — is close to pointless, so clearnet's value
is only for non-operator members and must be surfaced as a warned tradeoff, not
a silent default.

## Considered options

- **Clearnet relay as the default (rejected):** simpler ops, lower latency, but
  exposes the relay and normalizes the weaker posture — the exact metadata leak
  the transport was supposed to improve on.
- **Foreign curated public relays (deferred, see the concept's §10.2):** best
  availability, worst privacy (a third party sees `h`-tag + subscription IPs);
  NIP-42 AUTH on such a relay would re-identify the member via a persistent key
  and undo the ephemeral-key hiding. Left open pending the self-host-only vs.
  public decision.

## Consequences

- **Server-side onion service and client-side Tor are distinct** and both are
  required; "embedded relay behind Tor" alone is underspecified.
- **Residual correlation remains even on the onion default:** the relay still
  sees the `h`-tag and which (now IP-less) subscribers share a group —
  acceptable on the group's *own* relay (the operator knows the roster), a leak
  only on a foreign one, which is why the default steers to self-hosting.
- **Availability rendezvous:** a single onion relay is a choke point (onion
  services add latency); the default is **two or more** onion relays (native
  Nostr redundancy), not one.
- **N2 scope:** the WebSocket dialer must ride the T4 fail-closed path with a
  WS twin of the no-leak harness; the N6 wizard/settings gain the relay-list
  editor with the onion default and the clearnet warning + badge.

**Conditional status:** this decision only takes effect if the Nostr transport
is built at all, which is itself gated on the go/no-go self-host-SMP experiment
in the concept's §0. Recorded now so the reachability posture is fixed before,
not during, implementation. Full reasoning and cross-references:
`docs/transport/nostr_transport_marmot.md` §7.5.
