---
status: partially superseded
---

# Nostr relay policy: any relay allowed, curated onion default, self-hosted recommended

> **PARTIALLY SUPERSEDED (2026-07-31) by
> [ADR-0004](0004-no-preconfigured-relays-user-confirmed-pool.md).** The
> "curated set of onion relays as the default pool" below is VOID: the app now
> ships with an EMPTY pool and connects to nothing until the operator adds and
> confirms a relay (a shipped list would be a shipped surveillance point).
> The rest of this ADR — any relay allowed, self-hosting recommended, NIP-42
> AUTH in scope, honest badges for non-self-hosted/clearnet relays — stands.

**Context.** ADR-0001 settled *reachability* (onion by default, clearnet only
with a warning). This ADR settles *which relays a workspace may use at all*.
A shared relay sees a group's `h`-tag correlation, member count, and — unless
onion — subscription IPs. The tension is privacy vs. onboarding friction: a
strict self-host-only rule is safest but forces every founder to run a relay.

**Decision.** Relays are a **fully open user choice** — any relay (foreign
public, self-hosted, onion, or clearnet) may be added to a Nostr workspace.
But the product **steers hard** toward the private option:

- the founding wizard's relay list **defaults to a curated set of onion
  relays**;
- the UI **recommends self-hosted relays** and states plainly that **only a
  self-hosted relay avoids `h`-tag correlation** — any third-party relay (even
  a curated onion one) still sees which subscribers share a group;
- clearnet relays keep the stronger ADR-0001 "insecure" warning + health badge.

So: informed choice with a strong, private default — not a prohibition.

**Why.** A self-host-only rule would strand founders who cannot run
infrastructure and push them off the product entirely; an unguided "any relay"
free-for-all would normalize the metadata leak SMP was chosen to avoid. The
curated-onion default plus an honest recommendation gives the safe path by
default while keeping the door open, and it is consistent with the honest-status
posture used elsewhere (name the tradeoff, let the operator decide).

## Considered options

- **Self-host-only (rejected):** strongest privacy, worst onboarding; excludes
  non-technical founders.
- **Unrestricted public relays, no steering (rejected):** easiest, but the
  default path leaks `h`-tag + subscription metadata to third parties.

## Consequences

- **N2 must support arbitrary relays**, including foreign public ones — so
  **NIP-42 AUTH handling is in scope** (a relay may require it); on a foreign
  relay AUTH re-identifies the member, which the UI must warn about, while on
  the own onion relay it is harmless.
- The metadata comparison (`docs/transport/nostr_transport_marmot.md` §7) and
  §10.2 are resolved by this ADR + ADR-0001; §7.5 gains the curated-onion-list
  default + the "only self-hosted avoids h-tag correlation" recommendation.
- The health surface shows the relay set and flags any non-self-hosted /
  clearnet relay (badge), so the tradeoff stays visible, not accepted once.

**Conditional status:** takes effect only if the Nostr transport is built
(gated on the §0 go/no-go, which resolved GO on 2026-07-29).
