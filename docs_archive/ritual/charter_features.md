# Charter features — the founder picks the republic's surfaces, the threshold grows them

Status: **BUILT (2026-08-12)** — S1..S7 landed on master (S1: roster-v5 +
checkpoint-v7, conditional; S2: the selection travels the ritual; S3:
`set_features` governance with union fold; S4: engine gates + legacy
baseline; S5/S6: wizard checkboxes, ratify card, nav gating, Organization
list + enable modal, vote diff card; S7: docs). One deviation from §3 D8 as
planned: the vote card reuses the existing relay-diff rows verbatim
(feature keys rendered as display labels) instead of renaming the row
struct — same generalization, no window-API churn.

Read first: `founding_ritual.md` (the deliberation step this extends),
`docs_archive/chain/persistent_chain.md`, `docs_archive/transport/relay_topology_plan.md`
(R3/R6 — the two mechanisms this mirrors exactly).

## 1. The story (user, 2026-08-11)

- In the wizard's charter step (step 3) the founder picks, next to name and
  agenda, which **features** the republic activates: Chat (always on, shown
  disabled), Shared Memory, Kanban board, Vault (all off by default), Wallet
  (Monero multisig — ON by default).
- The feature selection is **ratified by every member together with the
  charter** (sign-what-you-see).
- The selection controls which items the left nav **shows**. The surfaces all
  exist in the code; an unselected feature's nav row is *not rendered* (today
  the staged-out rows render greyed).
- Under Organization › charter, active and inactive features are listed and
  editable; every change is an **m-of-n vote**, exactly like a relay-pool
  change (R6).
- **Features can never be switched off again** — enable-only, monotone.
- UI with minimal explanation; the design speaks for itself.

## 2. What existed BEFORE the build (survey snapshot, 2026-08-11)

| piece | where | state |
|---|---|---|
| all five surfaces + nav rows | `molt_core::Surface` (`ALL`: organization, chat, memory, quests, vault, wallet) | real; quests/vault/wallet hard-greyed in `app.slint` (`enabled: s.key != "quests" && …`), memory opens its DESIGN-MOCK pane |
| charter step in the create wizard | `app.slint` charter dot (`cw-agenda`, `create-propose`) → `Command::CreatePropose { name, agenda }` | real |
| member ratification card | `app.slint` (`jw-proposed-name/-agenda`, `join-confirm-charter`) ← `NetJoinCharterProposed` | real |
| charter bound into signed bytes | `roster_canonical_bytes` tag **`molt-roster-v4`** (CLAUDE.md's "v3" is stale — R3 bumped it) | real |
| gated org edit under threshold | `Propose { surface: Organization, payload: {op, value} }`, ops `set_relays`, `set_charter`, `set_name`, … | real (R6) |
| vote card with diff rows | `ProposalCard` `relay-op`/`relay-changes` (`parts.slint`), `relay_pool_diff` (molt-ui) | real |
| checkbox component | `components.slint::AppCheck` (no `enabled` prop yet) | real |
| feature gating | — | **does not exist** |

## 3. Design decisions

### D1 — Feature keys are the optional surface keys

`memory`, `quests`, `vault`, `wallet` — strings equal to `Surface` keys, so no
second vocabulary exists. Chat and Organization are core, never in the set:
the wizard's Chat checkbox is display only (checked, disabled). The stored
set is **sorted + deduped** (canonicalized at every ingest, like the relay
tokens in `cmd_propose`).

### D2 — The ratified selection lives in the signed roster bytes: `molt-roster-v5`, conditional

The feature set is charter content, so it goes where the agenda and the relay
pool went: into `roster_canonical_bytes`, as a new final field (le32 count +
per-key le32 length prefix — `hash-length-prefix-not-separators`), tag bumped
to `molt-roster-v5`.

**Conditional on presence, because live v4 republics exist.** The signature
paths carry `features: Option<Vec<String>>`:

- `None` → the emitted bytes are **byte-identical to v4** (tag included).
  Every existing republic's genesis keeps verifying unchanged.
- `Some(set)` → v5 layout with the feature field, `Some([])` ≠ `None`.

This is the `ChainChange::Membership` precedent (conditional fields keep
pre-existing preimages and their recorded signatures intact), lifted to the
tag level. New foundings always send `Some` (the wizard always has a
selection, possibly empty). The one-shot `CreatePropose` gains
`features: Vec<String>` (serde default; MCP schema: optional array).

Ripple (one commit, or signatures break silently): the 5 production
recompute sites (`chain.rs::approval_bytes` genesis arm,
`founding.rs::RitualRuntime::canonical`, `verify_sealed_roster`,
`verify_seal_proposal`, `lifecycles.rs::finalize_founding`), the structs
(`SealedRoster`, `into_genesis`, `WorkspaceEvent::Founded`,
`ChainChange::Genesis`, `EngineStateDump`, `ReplicaState`, the snapshot→
genesis rebuild in `molt-storage`, `sealed_roster_from_genesis/_from_blob`),
and every recompute harness/byte-pin test (§6 S1).

`verify_seal_proposal` additionally rejects a non-canonical set (unsorted or
duplicated keys) — one set, one byte encoding — **and any key this build
cannot render** (review 2026-08-12): the ratify card shows exactly the known
vocabulary, so a foreign key would be signed sight-unseen. A newer-build
founder against an older member fails honestly at ratification, like the
m-of-n mismatch gate. (Unknown keys arriving via a newer build's applied
BLOCK are still kept by the union fold — read-tolerance stays additive.)

### D3 — The republic id does NOT change

The agenda precedent: charter *content* is bound via the roster bytes, not
via the id preimage. `molt-republic-id-v2` stays. (Bumping it would re-key
every workspace for zero security gain — the n attestations over v5 bytes
already pin the feature set.)

### D4 — The checkpoint carries the founding set: `molt-chain-checkpoint-v7`, conditional

A cut drops the genesis block, so the founding feature set must ride
`CheckpointState` (exactly why v3 added the ratified relay pool) — also the
suffix walk recomputes the founding roster bytes from the blob and needs the
field to rebuild v5 bytes. Same conditional rule: `None` emits v6 bytes
(legacy blobs and blobs cut on legacy republics stay verifiable and
byte-stable), `Some` emits v7.

### D5 — Post-founding enable is an ordinary gated Organization op: `set_features`

R6's exact shape, no new `ChainChange` variant, no new `Command`, zero MCP
surface work (`propose`/`approve` already carry it, co-equality test stays
green untouched):

- payload `{"op":"set_features","value":"<space-separated keys>"}` — the
  **full target set**, canonicalized (sort+dedup) in `cmd_propose` like the
  relay tokens.
- `validate_org_payload` arm: every token one of the known four.
- Propose-time gates (courtesy, per the `fold_pool_edit` lesson): the target
  set must keep every effective feature this build KNOWS and add at least
  one new key — compact refusals (`already enabled` / `<key>: cannot be
  disabled`). Unknown effective keys are exempt from the keep-check (review
  2026-08-12): this build can neither name them (validate refuses) nor lose
  them (the fold is a union), and demanding them would brick feature
  governance on every older build in a mixed-version republic.
- **The deterministic rule is the fold, and the fold is a UNION:**
  `effective = baseline(genesis) ∪ ⋃ applied set_features values`. A block
  that tried to drop a feature folds as pure addition on every holder —
  enable-only is a construction property, not a gate.
- **Accumulating, NOT an `applied_lww_slot`:** two racing enable votes can
  both commit; an LWW summary would keep only the later value and a cut
  would silently lose the other's addition. Accumulating applied entries
  survive the checkpoint summary by design (`log_compaction.md`), so the
  union stays correct across cuts. (This is the one deliberate divergence
  from `organization.relays`.)
- `project_one` hook on `set_features` → refresh the session (nav updates
  ride the existing `after_org_applied` full-session emit).
- Display: `change_summary` (current = effective set, proposed = value),
  `decision_summary` label `Features`, `org_effective` fold arm.

### D6 — Legacy baseline: a republic without the field has `{memory}`

`features: None` (every republic founded before this change) must not lose
surface: memory is selectable today (real governance views under a mock
pane), so the legacy baseline is `{memory}` — the greyed dead rows
(quests/vault/wallet) disappear until voted in, memory stays. New foundings
use exactly the ratified `Some(set)` as baseline. *(User-confirmable default
— see §5.)*

### D7 — Gating is engine-side (co-equality)

`StatusView.features` exposes the effective set. The engine refuses, with a
compact error (`not enabled`):

- `SelectSurface` onto a disabled optional surface,
- `Propose` onto a disabled gated surface (Organization always allowed —
  `set_features` itself rides it).

The GUI *hides* the rows instead of greying them; an MCP agent hits the same
gate the nav enforces visually. An **enabled** feature's row is shown and
selectable — its pane is the existing DESIGN-MOCK-badged pane until the real
surface lands (the established stepwise-UI modus; never fake, always badged).

### D8 — UI

- **Wizard charter step:** under the agenda editor, five checkbox rows
  (`AppCheck` + new `enabled` prop for the locked Chat row). Defaults:
  chat ✓ locked, memory ☐, quests ☐, vault ☐, wallet ✓. Labels are the nav
  surface labels (one vocabulary). No explanatory prose.
- **Ratification card:** between agenda and the confirm/decline buttons, the
  five rows with their on/off state — the member sees the whole selection it
  signs, not only the enabled part. Read-only.
- **Organization › charter:** beneath the charter text, the feature rows
  (active normal, inactive greyed) with the section's edit pencil → modal
  listing the checkboxes; already-enabled and chat are locked-checked;
  arming gate = strict superset. Confirm → `org-propose("set_features", …)`.
- **Vote card:** diff rows via the existing relay-diff mechanism,
  generalized (`DiffRow`: `+ added` green / kept unmarked; `−` cannot occur).
- **Nav:** `SurfaceTab` rows for disabled features are not built; the
  hardcoded staged-out `enabled:` condition in `app.slint` is deleted.

## 4. What this does NOT do

- No per-feature settings, roles or permissions (agents-are-seats decision:
  security comes from the threshold alone).
- No disable path, no "hide again" — monotone by fold.
- No real Quests/Vault/Wallet implementations — their panes stay
  DESIGN-MOCK-badged; this plan only governs visibility.
- No invite-preview change (the invitee sees the selection at ratification,
  where it signs it).

## 5. Open points — DECIDED by the user 2026-08-11

1. **Labels:** the `quests` surface is labelled **"Kanban"** everywhere (nav
   and wizard; wire key stays `quests`). The wallet stays **"Wallet"**.
2. **Legacy baseline `{memory}`** (D6) — confirmed.
3. **Section term:** **"Features"**, one vocabulary everywhere.

## 6. Steps (one commit each, red first)

### S1 — core: `molt-roster-v5` + `molt-chain-checkpoint-v7`, conditional
The lockstep byte commit. `roster_canonical_bytes` gains
`features: Option<&[String]>`; `checkpoint_canonical_bytes` /
`CheckpointState` gain the founding set; all structs of D2 gain the field
(`#[serde(default)]`, `skip_serializing_if` on the chain/event side); all 5
production recompute sites and every fixture updated together.
- **Red:** v5 pin (features bound, `Some([])` ≠ `None`, independent sha
  recompute); the existing v4 pin values stay green through the `None` path
  (byte-for-byte legacy compat proven, not assumed); checkpoint v7 pin + v6
  `None`-path pin; `verify_sealed_roster` rejects a tampered feature list;
  `verify_seal_proposal` rejects a non-canonical set.

### S2 — ritual: the selection travels and is ratified
`CreatePropose.features` (+ MCP schema) → validation (known keys,
canonicalize) in `cmd_create_propose` → `RitualRuntime` → `maybe_seal`
proposal → member verify → `Ratifier` / `NetJoinCharterProposed.features` →
genesis on both legs (loopback + Nostr).
- **Red:** e2e twin of `the_relay_pool_is_bound_into_what_every_member_signs`
  — the feature set reaches every member's genesis and a founder cannot seal
  a different set than ratified; `a_sealed_roster_differing_from_the_ratified_
  proposal_is_rejected` extended to a feature-only difference.

### S3 — governance: `set_features`
D5 complete: validate arm, canonicalization, superset gate, union fold +
`effective_features()`, accumulate (no LWW), `project_one` hook, StatusView,
`change_summary`/`decision_summary`, `org_effective`.
- **Red:** a `set_features` commits under threshold and moves the effective
  set (non-founder proposer); a proposal dropping an enabled key is refused;
  a hand-built block that drops a key **folds as a no-op-removal** (union —
  the deterministic twin); unknown key refused; two racing enables both
  survive a checkpoint cut.

### S4 — engine gates + legacy baseline
D6 + D7: `effective_features()` baseline `{memory}` on `None`;
`SelectSurface`/`Propose` refusals.
- **Red:** legacy state reports exactly `{memory}`; selecting/proposing a
  disabled surface is refused compactly; enabled surface passes.

### S5 — UI: wizard + ratification card
D8 first half. `AppCheck.enabled`, `cw-feat-*` properties read by the
`create-propose` handler, `jw-feat-*` display rows.
- Validated by `cargo build -p molt-ui-window -p molt-ui` (+ dev-ui.sh for
  iteration); engine-level behavior already pinned by S2.

### S6 — UI: nav gating + Organization list + vote diff
D8 second half. Delete the staged-out condition; filter `SurfaceTab` rows on
`StatusView.features`; charter-section rows + modal; `DiffRow`
generalization of the relay diff card.

### Known debt (review 2026-08-12, deliberately deferred)

- **UI fan-out**: the four feature keys are hand-enumerated across the
  slint booleans, three checkbox grids, the reset block, the modal and the
  MCP schema enum. A model-driven `[FeatureRow]` (the nav/relay-rows shape)
  would collapse it; do this when the fifth optional surface appears.
- **Fold walk count**: `org_effective`/`status` fold the (small) applied
  org log a handful of times per read; a single-pass fold is cheaper if it
  ever shows up in a profile.
- **Version-skew diagnosis**: a pre-v5 build meeting a v5 genesis fails
  with the generic tamper error (it silently drops the unknown field before
  recomputing) — inherent to any layout bump; nothing in-tree can teach an
  already-shipped build a better message.
- **Legacy history visibility**: a pre-v5 republic that (via MCP) had
  applied entries on quests/vault/wallet shows them again only after the
  m-of-n enable vote; until then they are reachable read-only over MCP.

### S7 — docs + stale references
`founding_ritual.md` (the Seal/Genesis tables + guarantee list gain the
feature set), `relay_topology_plan.md` cross-note, CLAUDE.md's stale
"molt-roster-v3" mentions → v5-with-history, stale v3 comments in
`founding.rs`. `python3 scripts/check-doc-refs.py` clean.
