# Security audit 2026-10-10

Status: EXECUTED - audit log, 2026-10-10. Fixes landed on master
`3d79122c`..`5541059b` (2026-10-10/11, audit base `fdd8c48b`). What is
still open is carried by `docs/reviews/known_debt.md`; nothing here is
open work on its own.

## Scope

Every crate, by threat area: the founding ritual, recovery, the chain
(verify, sync, governance), MLS, the wire (ingest, mirror, votes),
the MCP endpoint, storage and sealing at rest, the vault, files and
wiki search, hooks and dev harnesses, network privacy (clearnet consent,
onion handling), the GUI's chat rendering, the wallet run, governance
and chat, config store and reload, the file plane.

Out of scope by design (ADR-0007): the seat token operates the machine.
Checked instead: nobody without the seat token gets it, the read-only
key sees no secret, phrase or purse, and no MCP client speaks as another
member (`net_*` and `ui_publish` stay INTERNAL).

## Method

One finder pass per area, read-only, each finding with a concrete
failure scenario. Every finding then went to three independent
skeptics; a finding was kept on a majority and its severity is the
majority vote. One fixer per area fixed the kept findings test-first
(a red test reproducing the scenario, then the fix), reviewed, landed on
master. A final gate ran every crate's tests and clippy.

- Kept: 41 reports, 37 distinct (four were found by two areas).
- Rejected by the skeptics: 2.

Severity: HIGH = one insider or outsider breaks a security promise or
the shared state · MEDIUM = a concrete denial, leak or authority gap ·
LOW = bounded nuisance, latent or local-only.

## Kept findings

| # | Sev | Area | Finding | Fix |
|---|---|---|---|---|
| 1 | HIGH | ritual | Founding MLS tree never checked against the sealed roster; 445 traffic authenticated by credential name only | `3d79122c` |
| 2 | HIGH | mls | Update path / Update proposal can rename the sender's leaf to another member | `e42a267a` |
| 3 | HIGH | chain | Junk attestation grinds a block's hash; tie-break reorgs every holder and drops all blocks above | `8abe1ec2` |
| 4 | HIGH | files | A chat message reusing a persisted share's id becomes that share's identity | `ca7cd1ae` |
| 5 | MED | recovery, chain | Restored relays not covered by the consent; a coordinator can swap them, survivors auto-sign | open |
| 6 | MED | recovery | A restore consent has no position or freshness; replayable into any block | open |
| 7 | MED | recovery | A junk MembershipProposed blocks a seat's recovery for good (R3 gate) | `810ec2a4` |
| 8 | MED | recovery | Coordinator merges a re-key KeyPackage not bound to the seat, strands itself | `810ec2a4` |
| 9 | MED | mls | Concurrent-commit tie-break on unauthenticated `created_at`; replay forces rewinds | `e42a267a` |
| 10 | MED | wire | Unproven hold claims; a silent lowest-named seat starves every re-seed | `9b8eb420` |
| 11 | MED | mcp, configstore | 64 connection slots held forever by unauthenticated peers (no pre-auth timeout) | `052986b7` |
| 12 | MED | mcp, hooks | `--repair-config` writes an empty MCP token, i.e. authentication off | `9d787d22` |
| 13 | MED | storage | Phrase-sealed workspace keeps plaintext `prefs.toml` (handles, presence, paths) | `61c8ef1a` |
| 14 | MED | files | Unbounded `wiki_search` nesting overflows the actor's stack | `ea7450a1` |
| 15 | MED | files | Huge `wiki_search` cursor requests a tens-of-GB allocation | `ea7450a1` |
| 16 | MED | netpriv | Withdrawn clearnet consent or relay keeps being dialed until reopen | `26b7a377` |
| 17 | MED | gov-chat | Declined/withdrawn membership cards fill the pending cap; real recovery refused | `34b9b93c`, `810ec2a4` |
| 18 | MED | file-plane | One forged top record captures a rootless fetch (wiki base, vault base) for good | `5541059b` |
| 19 | LOW | ritual | Unauthenticated JoinRequests grow the uncapped run log | `3d79122c` |
| 20 | LOW | ritual | Handle validated trimmed but anchored untrimmed; invisible characters allowed | `3d79122c` (homoglyphs open) |
| 21 | LOW | chain | `wiki_base` op not reserved; a member can propose a bogus base commitment | `a998787c` |
| 22 | LOW | wire | One forged Approved makes every peer publish a VoteRefused | `67329c09` |
| 23 | LOW | wire, wallet | Fresh run nonces grow `seen`/`aborted` and trigger n-1 abort publishes | `c78d0f84` |
| 24 | LOW | mcp | Read-only key sees the local download path | `120e95b9` |
| 25 | LOW | storage | `.trash` keeps a device-sealed seed of a later phrase-sealed workspace | `61c8ef1a` |
| 26 | LOW | vault | Own-check cache keyed by `secret_id`, which does not cover the share | `a72dc581` |
| 27 | LOW | hooks | Lab harnesses use a fixed `/tmp` dir and a public MCP token | `c785fb2b` |
| 28 | LOW | hooks | No private umask; workspace dirs and plaintext files world-readable | `290acaf9` |
| 29 | LOW | netpriv | Trailing-dot onion S3 endpoint resolved via system DNS; hand-rolled parser | `4e89b00a` |
| 30 | LOW | ui | Peer chat with kind=System renders as an unattributed engine line | `70ab3175` |
| 31 | LOW | wallet | Any seat's `wdhint` overwrites the starter's daemon suggestion | `b8ce8889` |
| 32 | LOW | gov-chat | A silent-but-present member pins the compaction floor at 0; expired chat never erased | open |
| 33 | LOW | gov-chat | Withdraw authorized by first-sighting `by`, not the minting seat | `6995b339` |
| 34 | LOW | gov-chat | Predictable decision-line id lets an insider pre-post a forged line | `70ab3175` |
| 35 | LOW | configstore | HTTP credentials snapshotted per connection; revoked keys work on keep-alive | `6da54fde` |
| 36 | LOW | configstore | Reloaded `clearnet_enabled = false` never reaches the live dial gate | `d1b2c26f` |
| 37 | LOW | configstore | A save silently reverts a hand edit the poller has not seen | `9298017d` |

## Open decisions

- **Restore consent v2** (#5, #6): `molt-restore-consent-v1` signs
  neither the relays nor a single-use value. Proposed: a conditional v2
  over relays plus the KeyPackage hash or ticket, and an anchor-freshness
  verifier rule for v2 blocks only. Signed layout and consensus rule -
  user decision.
- **Deliberation-time leaf forgery** (#1 residual): a joiner enters the
  group from the Welcome before it knows the identity table, so a
  founder-planted leaf can forge ephemeral deliberation frames until the
  seal. Recommended: accept while deliberation frames carry no authority.
- **Recovery rejoiner tree check** (#1 residual): a coordinator's Welcome
  tree is only warned about and the extra leaf muted, not refused.
  Hard refusal could strand a live republic with a historically imperfect
  tree.
- **Homoglyph handles** (#20 residual): closing them needs a UTS #39
  skeleton uniqueness check - product policy and a dependency.
- **Compaction floor** (#32): needs a policy for a peer that is reachable
  but never acks (a grace after which it stops pinning the floor).

## Final gate

Every crate's tests and per-crate clippy green; molt-ui on the
live-preview suite. The gate found one pre-existing red:
`molt-vault/tests/pure_rust_guard.rs` saw `cc` via chrono's `clock`
feature (kanban S1). `clock` now rides only molt-ui and molt-engine's
dev graph.
