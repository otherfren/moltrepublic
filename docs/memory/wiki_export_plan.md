# Wiki export to disk, with externally verifiable patch signatures

**Status: PLAN (decided 2026-08-18) — not built; all forks resolved with the user, ready to execute**

## Goal

When the wiki (Shared Memory / Multisig-Wiki) is non-empty, the GUI offers a
button that saves the ENTIRE wiki to a user-chosen directory on disk. The
export can carry a **proof bundle**: the threshold signatures of every applied
wiki patch plus the genesis they anchor to, so an **external reviewer** — no
moltd, no workspace key — can verify that the exported tree is exactly the
fold of patches a real m-of-n of the sealed roster approved. Nothing beyond
the wiki and the minimum verification anchors leaves the workspace; secrets
stay secret.

## What exists (verified 2026-08-18, file:line)

- Wiki state = **fold over applied `wiki_patch` payloads**, pure and
  deterministic: `molt_core::wiki_fold::wiki_fold` (`crates/molt-core/src/wiki_fold.rs:375`),
  tree = `BTreeMap<path, content>`, engine recomputes on read
  (`State::wiki_tree`, `crates/molt-engine/src/proposals.rs:1433`).
  `applied_values` concats the legacy projection and the chain projection
  (`proposals.rs:1494` — one of the two is always empty per workspace).
- A chain-governed workspace already keeps **the sealing signatures per
  applied proposal**: `chain_applied_sigs: proposal_id → Vec<RosterAttestation>`
  (`crates/molt-engine/src/lib.rs:593`, filled at `chain.rs:1350-1372`).
- **A member signature covers `republic_id ‖ height ‖ change`**
  (`molt_core::chain::approval_bytes`, tag `molt-chain-change-v2\0`,
  `crates/molt-core/src/chain.rs:192-292`); `prev` and the sig set are
  structural (`block_link_bytes`, `molt-chain-block-v1\0`, `:536`). So **one
  block verifies on its own** against the roster valid at its height — no
  neighbors needed.
- Roster evolution happens ONLY via `ChainChange::Genesis` and
  `ChainChange::Membership` (`chain.rs:69-160`); a recovery replaces a
  member's `identity_pk`. Genesis verifies n-of-n over
  `roster_canonical_bytes` (v4/v5 conditional, `molt-core/src/lib.rs:2038`)
  and `republic_id` re-derives via `molt-republic-id-v2`
  (`molt-storage/src/lib.rs:286`).
- Export machinery to copy: `Command::ExportWorkspace` — sync validation,
  `session.export` state, `tokio::spawn` + `spawn_blocking` write, outcome as
  internal `NetExportDone/Failed` (`crates/molt-engine/src/session.rs:1692-1826`;
  test `manual_export_writes_a_real_blob_and_fails_honestly`,
  `molt-engine/src/lib.rs:1995`).
- Folder picker precedent: `on_ws_dir_pick` (`crates/molt-ui/src/lib.rs:1724`,
  rfd `pick_folder` in `spawn_blocking`).
- Wiki GUI: non-empty = `WikiState.base-docs.length > 0`; toolbar homes:
  navigator `ToolStrip` (5 buttons, `surfaces.slint:789-834` — its 180px
  min-width comment names "the five-button toolbar's floor") or the editor
  strip (`:1364`).
- Local drafts live OUTSIDE the fold (GUI `Wiki.stack` + `wiki_draft.json`,
  export-excluded by decision — `docs_archive/memory/shared_memory_real.md:328`).
- The wiki surface holds **only member-authored markdown** (no blobs, no
  vault linkage; vault is an unbuilt draft). A `wiki_patch` block carries no
  author identity beyond the m signer handles in `sigs`.

## Design

### Export layout (a user-picked directory)

```
<dest>/
  wiki/<path>          one plain file per folded WikiDoc path (paths are
                       already validated: relative, ≤8 segments, no dot-dirs —
                       re-checked on write, never escaping <dest>)
  proof/bundle.json    only with the proof option: see below
  proof/README.md      the verification algorithm + the disclosure notice,
                       written for the external reviewer
```

Plain files, no archive: "save to disk" means browsable markdown. The proof
bundle is optional at the dialog (see open question 1).

### The proof bundle (`proof/bundle.json`)

```json
{
  "format": "molt-wiki-export-v1",
  "genesis": <ChainBlock 0, serde JSON>,
  "blocks":  [<ChainBlock>, ...]
}
```

`blocks` = every `Membership` block and every `Applied { surface: memory,
payload.op == "wiki_patch" }` block, ascending by height. Membership blocks
MUST ride along: they are the identity history — after a recovery, later
patch signatures verify only against the replaced key. All other block kinds
(org changes, checkpoints, future vault/wallet) stay OUT; their content never
touches the export.

### What the external reviewer can verify (and what not)

With `bundle.json` alone (plus the `wiki/` tree):

1. **Genesis seal**: recompute `roster_canonical_bytes` (v4/v5) from the
   Genesis fields, verify the n-of-n attestations against the anchored
   `identity_pk`s, re-derive `republic_id` (`molt-republic-id-v2`) and match.
2. **Roster walk**: apply Membership blocks in height order (verifying each
   with ≥ rule_m distinct signatures against the roster valid before it,
   restore-consent counting as one signer, exactly like `verify_next`).
3. **Every wiki patch**: ≥ rule_m distinct valid signatures over
   `approval_bytes(republic_id, height, change)` against the roster valid at
   that height; no duplicate heights; ascending order.
4. **Fold correctness**: `wiki_fold` over the payloads in height order equals
   the shipped `wiki/` tree byte for byte.

Honest limitation, stated in `proof/README.md`: the bundle proves
**authenticity and threshold provenance**, not **completeness** — an exporter
could omit trailing patches and present an older (still genuinely approved)
state. Detecting omission would require the full hash-linked chain including
every non-wiki block, which contradicts "export nothing but the wiki"
(open question 2).

### Disclosure (this is the price of verifiability)

The bundle necessarily reveals: republic name, charter/agenda, feature set,
member names, `identity_pk`s, **`nostr_pk` transport anchors** (inside both
the signed roster bytes and `republic_id` — not redactable without breaking
the seal), relay declarations inside Membership blocks, and which m members
signed each patch. `proof/README.md` and the export dialog say this in one
compact line each. Exporting WITHOUT the proof bundle writes only `wiki/`.

### Engine surface

- `Command::WikiExport { dest: String, proof: bool }` — a human decision:
  MCP tool **`wiki_export`** + GUI button (co-equality).
- Internal outcomes `NetWikiExportDone { dest, files, bytes }` /
  `NetWikiExportFailed { error }` → INTERNAL list in `molt-mcp::tools()`.
- Handler (copy `cmd_export_workspace`, `session.rs:1692`): sync validation —
  workspace open, wiki tree non-empty, dest non-empty, no export running —
  then `tokio::spawn` + `spawn_blocking`: write files, then bundle. State in
  a new `session.wiki_export: ExportState` (serde default; reusing the
  struct, not the backup-export slot, so the two cannot collide). Outcome
  emits `SessionChanged` (toast via notice, like backup export).
- Non-chain (legacy counted) workspace: `chain_applied_sigs` is empty — a
  `proof: true` export is REFUSED with `wiki export: proof needs chain
  governance` (files-only export stays available). No fake proofs.
- Drafts are NOT exported: the export is the approved fold, matching the
  backup-export decision. If the local changeset stack is non-empty the GUI
  dialog shows one line: "n local unapplied changes stay local".

### The verifier

New public pure function in molt-engine (`crates/molt-engine/src/chain.rs`):

```rust
pub fn verify_wiki_export(bundle_json: &str, tree: &BTreeMap<String, String>)
    -> Result<WikiExportReport, String>
```

implementing steps 1-4 above by reusing the REAL primitives
(`approval_bytes`, `roster_canonical_bytes`, `molt_storage::identity_verify`,
`molt_storage::republic_id`, `wiki_fold`) — no byte layout is reimplemented,
so the verifier can never drift from the writer. A thin new example binary
`crates/molt-engine/examples/verify_wiki_export.rs` (first example in the
crate) does the I/O: `cargo run -p molt-engine --example verify_wiki_export
-- <dir>` prints per-step verdicts and exits non-zero on any failure.
`verify_sealed_roster`-adjacent helpers stay `pub(crate)`; the new function
lives beside `verify_chain`, which is already public.

`proof/README.md` documents the byte layouts precisely enough that an
independent implementation is possible without our code (tag strings, le32
framing, sort orders) — but the shipped verifier is the reference.

## TDD keystones (write red first)

1. `molt-engine` chain.rs mod tests, on the existing crafted-chain fixtures:
   `verify_wiki_export` green on a 2-member chain with two wiki patches and
   one Membership (recovery) between them; red on (a) a tampered file in the
   tree, (b) a tampered patch payload, (c) a forged/removed signature,
   (d) the Membership block omitted (post-recovery sig must fail),
   (e) reordered/duplicate heights.
2. Engine cmd test (pattern `manual_export_writes_a_real_blob…`): export
   writes `wiki/` + `proof/bundle.json`; empty wiki → refused; `proof: true`
   on a non-chain workspace → refused with the compact fault; a second
   export while one runs → refused.
3. Round trip: run the export, then `verify_wiki_export` over what was
   written — green; flip one byte in a wiki file — red.
4. Co-equality test forces the `wiki_export` tool + INTERNAL entries.
5. GUI (stub suite): button visible/enabled only when `base-docs.length > 0`;
   dialog wiring issues the command; i18n lexicon entries (EN + DE) present.

## GUI

- 💾 `ToolBtn` in the wiki navigator `ToolStrip` (6th button; bump the 180px
  floor comment at `surfaces.slint:754`), enabled iff the base tree is
  non-empty.
- Click → `ConfirmModal`: folder picker row (rfd `pick_folder`, the
  `on_ws_dir_pick` pattern), an `AppCheck` "Include verification bundle"
  (see open question 1 for the default), one compact disclosure line, one
  drafts line when the local stack is non-empty. Confirm →
  `Command::WikiExport`.
- Outcome toast from the notice: `wiki exported: <n> files` /
  `wiki export failed: <reason>`. All strings compact, EN+DE lexicon, no em
  dash.

## Verification (how this lands)

`cargo test -p molt-engine` (new keystones red→green), fast GUI iteration via
`scripts/dev-ui.sh build` + stub test suite, ONE
`cargo build -j 1 -p molt-ui-window -p molt-ui` per change-set,
`cargo clippy --all-targets` = 0, then master.

## Decisions (user, 2026-08-18)

1. **Proof default ON**: the export dialog's "Include verification bundle"
   checkbox defaults to checked, with the one-line disclosure (names, keys,
   nostr anchors, relays, charter, per-patch signer sets are revealed).
2. **Completeness bound accepted**: the proof shows authenticity + m-of-n
   provenance, not latest-ness; `proof/README.md` states it. A reviewer who
   needs freshness compares two exports or obtains one from another member.
3. **Verifier = in-repo example**: `cargo run -p molt-engine --example
   verify_wiki_export -- <dir>`, reusing the real byte-layout functions; no
   standalone reimplementation. The README's layout spec keeps independent
   implementations possible.
4. **Drafts stay local** (default taken, unobjected): only the approved fold
   is exported; the dialog says so in one line when the local stack is
   non-empty.
5. **Button home** (default taken, unobjected): the wiki navigator toolbar,
   as the 6th ToolBtn.
