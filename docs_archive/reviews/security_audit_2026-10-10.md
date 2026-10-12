# Security audit 2026-10-10

Status: EXECUTED - audit log, 2026-10-10. Fixes landed on master
`3d79122c`..`5541059b` (2026-10-10/11, audit base `fdd8c48b`), the
decided follow-ups 2026-10-11 at the end. What is
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
| 5 | MED | recovery, chain | Restored relays not covered by the consent; a coordinator can swap them, survivors auto-sign | v2 (§7e), v1 residual open |
| 6 | MED | recovery | A restore consent has no position or freshness; replayable into any block | v2 (§7e), v1 residual open |
| 7 | MED | recovery | A junk MembershipProposed blocks a seat's recovery for good (R3 gate) | `810ec2a4` |
| 8 | MED | recovery | Coordinator merges a re-key KeyPackage not bound to the seat, strands itself | `810ec2a4` |
| 9 | MED | mls | Concurrent-commit tie-break on unauthenticated `created_at`; replay forces rewinds | `e42a267a`, digest-only order 2026-10-11 |
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
| 32 | LOW | gov-chat | A silent-but-present member pins the compaction floor at 0; expired chat never erased | fixed 2026-10-11 (ack-progress grace, `log_compaction.md` F4b) |
| 33 | LOW | gov-chat | Withdraw authorized by first-sighting `by`, not the minting seat | `6995b339` |
| 34 | LOW | gov-chat | Predictable decision-line id lets an insider pre-post a forged line | `70ab3175` |
| 35 | LOW | configstore | HTTP credentials snapshotted per connection; revoked keys work on keep-alive | `6da54fde` |
| 36 | LOW | configstore | Reloaded `clearnet_enabled = false` never reaches the live dial gate | `d1b2c26f` |
| 37 | LOW | configstore | A save silently reverts a hand edit the poller has not seen | `9298017d` |

## Open decisions

- **Restore consent v2** (#5, #6) - BUILT 2026-10-11
  (`recovery_auto_approval.md` §7e; v1 residual in known debt): `molt-restore-consent-v1` signs
  neither the relays nor a single-use value. Proposed: a conditional v2
  over relays plus the KeyPackage hash or ticket, and an anchor-freshness
  verifier rule for v2 blocks only. Signed layout and consensus rule -
  user decision.
- **Deliberation-time leaf forgery** (#1 residual): a joiner enters the
  group from the Welcome before it knows the identity table, so a
  founder-planted leaf can forge ephemeral deliberation frames until the
  seal. ACCEPTED 2026-10-11 (below).
- **Recovery rejoiner tree check** (#1 residual): a coordinator's Welcome
  tree is only warned about and the extra leaf muted, not refused.
  ACCEPTED 2026-10-11 (below).
- **Homoglyph handles** (#20 residual): closing them needs a UTS #39
  skeleton uniqueness check - product policy and a dependency.
  Decided 2026-10-11: a new handle is ASCII + ä ö ü ß, anchored rosters
  are never re-validated (`founding_ritual.md` §4 ❸).
- **Compaction floor** (#32): decided 2026-10-11 - ack-progress grace of
  two retention windows (`log_compaction.md` F4b), built.

## Final gate

Every crate's tests and per-crate clippy green; molt-ui on the
live-preview suite. The gate found one pre-existing red:
`molt-vault/tests/pure_rust_guard.rs` saw `cc` via chrono's `clock`
feature (kanban S1). `clock` now rides only molt-ui and molt-engine's
dev graph.

## Follow-ups 2026-10-11

The user decided the open questions on 2026-10-11. Built test-first,
reviewed, landed on master:

| Decision | Commits |
|---|---|
| Restore consent v2 (#5, #6): signs the block's relays, the consent anchor and the replaced working anchor; single use, 1024-block window; v1 verifies but is never auto-signed | `3a373694`, `f730d758`, `694a87aa` |
| MLS concurrent commits (#9): order by `sha256(commit)` only, no carrier stamp; an evicted committer's commit loses in every arrival order; a bare removal is refused | `48831155`, `95d9f30d`, `7b2e0778` |
| MCP auth (#11, #12, #35): no `[mcp].token` starts no TCP endpoint; an emptied seat key admits nobody; a line session re-checks its key per request and closes once revoked | `ecdce46e`, `ef3241fd` |
| MCP live TCP: a seat key set at runtime binds the TCP endpoint(s) without a restart; cleared, it closes the port and every connection on it; a runtime bind failure retries, a bind failure at start still ends the endpoint | `c261c52e`, `94db83ef`, `e52312fa`, `375ee911` |
| Compaction floor (#32): a peer in contact but never acking stops holding the floor after two retention windows (`log_compaction.md` F4b) | `94b446e7`, `02b61698`, `d43f8c33` |
| Homoglyph handles (#20): a new handle is ASCII plus `ä ö ü Ä Ö Ü ß`; anchored names are never re-checked; a verified join outside the set is refused visibly | `3a885ae1`, `f4188027` |
| Plaintext `http://` to a clearnet host: a purse node needs a confirmation and shows a warning; an S3 endpoint needs `s3_plaintext_confirmed` | `d2a08d7f`, `5a47dfaf` |
| Purse dust: outputs below 0.0001 XMR are counted (count and total), never held, outside `MAX_OUTPUTS` and the balance | `618c9e0e`, `f6cb32ad` |
| Restore consent v2 emission: minted only once every seat announced the `molt-restore-consent-v2` capability (mirror declaration `caps`, kept in `prefs.toml`) | `d37d68e2`, `b1a28c6c` |

Accepted residuals (no work planned):

- **Recovery Welcome foreign leaf** (#1): warned and muted, not refused.
- **Device-sealed `prefs.toml`** (#13): stays plaintext; only a
  phrase-sealed workspace seals it.
- **No purse emergency exit** (wallet design §5): not built.
- **Deliberation leaf** (#1): ephemeral deliberation frames carry no
  authority before the seal.
- **Block-link grind** (#3): a seat can still grind an attestation list
  to win a slot; the re-link keeps every decision.
- **Wallet start rate** (#23): a seat's own run starts are not
  rate-limited; only lost-run answers are.
- **No idle cap on authenticated line sessions** (#11): the pre-auth
  timeout bounds only unauthenticated peers.

Still open, in `docs/reviews/known_debt.md`: the v1 restore-consent
residual and ASCII look-alike handles (#20).

Final gate: every crate's tests and per-crate clippy, molt-ui on the
live-preview suite, `ring` absent from molt-net's graph, doc refs clean.
It found one red: molt-treasury's graph guard pruned `iana-time-zone`,
which left the graph with `c40e1452`; fixed in `0fc25826`.
The gate over the second batch (2026-10-12, master `375ee911`) found
no red: 2056 crate tests, molt-ui live-preview 507, clippy clean per
crate, `ring` absent, doc refs clean. Package residuals in known debt:
the S3 plaintext prompt skipped by "Save & continue", the compaction
grace charging a clean close to the peer.
