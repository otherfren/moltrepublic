# Field report v0.0.2: MCP payload inconsistencies (2026-09-07)

Status: EXECUTED 2026-09-08 - every verdict below is on master. Source: an LLM agent driving a
recovered production seat (2-of-3) headless over MCP; five payload
inconsistencies, none blocking.

| # | Reported | Verdict |
|---|---|---|
| N1 | `status.founded_ts` is the moment THIS node materialized the workspace; every copy reports a different founding date | Real. Blocks carry no time (deterministic chain), so the founding date cannot come from the genesis block. Fix: the founder stamps `founded_ts` when it mints the seal proposal; the sealed roster carries it (`#[serde(default)]`, unsigned display metadata, clamped to "now" on receipt), every joiner writes its genesis envelope with that stamp, and the recovery Welcome carries the coordinator's value the same way. Manifest `created` follows automatically. Rejected: binding it into the canonical bytes (a roster-v6 bump for a display field). |
| N2 | `read_state.applied_ids` is an array of nulls | The read was the CHAT surface (the ids shown are 32-hex message ids); chat rows have no proposal origin, so the track is all-null by design. Fix: chat emits an empty track and the key is omitted when empty; the tool description says so. |
| N3 | Ordinary chat messages have `kind: null` | The wire omits the default (`user`) to keep pre-kind messages byte-identical; the client rendered the absent key as null. Fix: the MCP presentation fills `kind: "user"` on chat rows, so `kind` is exhaustively matchable. |
| N4 | `status.surfaces` lists `quests`/`vault` as if they existed | Real. Fix: `Surface::is_implemented` and `implemented` on every surface stat (organization, chat, memory, files = true; quests, vault, wallet = false). The rows stay listed (the GUI navigation keys on the list); the `create_start` feature text is corrected (wallet has no surface either). |
| N5 | `workspaces[].members` stale right after a recovery, `read_members` not | Both read the same session entry; the recovery seal pushed it with never-seen stamps and the first refresh ran only on the next tick. Fix: the seal refreshes the entry at once, so the two views agree from the first poll. With N1 the non-mesh seats show the founding date, not the recovery moment. |

Keystones: `join_sealed_validates_the_persisted_nostr_secret` (the joiner's
genesis and manifest carry the founder's stamp),
`recovery_materializes_the_workspace_from_the_full_verified_chain` (the
carried stamp, and the entry/members agreement at the seal),
`recovery_completes_end_to_end_and_the_rejoiner_materializes` (an unknown
stamp raises nothing) and `founding_gates_on_the_joiners_charter_ratification`
(molt-engine); `a_wire_without_the_founding_date_decodes_to_unknown`
(molt-net); `a_chat_row_reads_its_default_kind_explicitly` (molt-mcp);
`only_the_built_surfaces_read_as_implemented` (molt-core).
