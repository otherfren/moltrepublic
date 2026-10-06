# Vault: prepared at every founding, enabled by vote

Status: EXECUTED 2026-10-06 (a record, not instructions - the spec
`vault_threshold_disclosure.md` rev 5 is the authority). Amended D11 and
D14 of the spec in the change that landed the behaviour (`5df3d343`).
Ground truth was master `2fbc8d84`.

Deviations from the plan as written: none in behaviour. The refusal
text is the existing disabled-feature form `vault: not enabled`. The
"enabled" predicate reads the founding features from the chain (genesis
or anchor), not the node's replica. The holder check after an enabling
block reuses the idempotent receipt sweep. The integration, MCP and
nav tests were written after the engine change they pin; their unit
twins were seen red first.

## 1. Decisions (user, 2026-10-06; ratified)

- **E1 Wizard.** A Vault checkbox, OFF by default, tickable iff
  `2 <= m <= n-2`, greyed with `needs 2 <= m <= n-2` otherwise.
  (Already the shipped wizard; pinned by a test.)
- **E2 Every new founding prepares the vault.** Every joiner sends
  `vault_pk`, the sealed roster is always roster-v6 with every seat
  keyed, every seat persists `vault_seed`, ticked or not, bounds or not.
- **E3 Older builds join no new founding** (D14 extended): the founder
  refuses a joiner without `vault_pk` (`needs a newer version`), vault
  ticked or not.
- **E4 Enable later by `set_features`** (enable-only, like every
  feature) iff the genesis is roster-v6 and `2 <= m <= n-2`. Refused at
  the NEW-proposal doors only (propose, approve, wire `Proposed`):
  v5 genesis -> `needs a newer republic`; bounds -> `needs 2 <= m <= n-2`.
  Historic `set_features vault` blocks keep verifying; `vault` in a v5
  feature set stays the mock forever.
- **E5 Prepared but invisible until enabled.** In a v6 republic whose
  effective features (genesis features plus applied `set_features`, as
  walked on the chain) lack `vault`: no Vault nav/pane, `read_state` has
  no `vault` object, every vault command refuses (the memory/poke
  precedent: `MoltError::FeatureDisabled`, text `vault: not enabled`),
  no vault frames are sent. `verify_chain`, approve and block ingest
  hard-reject a Vault deposit/grant block at a height where the walked
  chain has not enabled the vault (judged from the chain under
  verification, plan rule 1.3.8). Cuts stay `CheckpointVault`/v10 with an
  empty base before enabling; cut, recovery and the vault keep working
  after enabling.

Obvious defaults this plan takes:
- **Enabled** = prepared (genesis/anchor roster-v6) AND bounds hold AND
  `vault` in the effective features. The bounds are part of the
  predicate, so a hand-crafted out-of-bounds enable block that commits
  anyway (m colluding seats) enables nothing - deterministic, from the
  chain, and history is never rejected.
- The refusal is the existing `FeatureDisabled("vault")`
  (`vault: not enabled`, DE `vault: nicht aktiviert`), not a new
  vault-specific error: one rule for every disabled feature.
- A Vault proposal arriving on the wire before the enabling block lands
  in the pool like any disabled-surface card (charter_features D7) and
  is not approvable; after the enabling block the holder check runs for
  every pending deposit card.
- `VaultRefusal::FoundingOnly` is replaced by `NeedsNewerRepublic`
  (`needs a newer republic`); the error is never serialized.
- The member side (`verify_seal_proposal`) keeps accepting a keyless
  table: E3 is about joiners; an older FOUNDER still founds a v5
  republic (it can never enable the vault there).
- New: `StatusView.vault_enable` (`on` / `offer` / `needs_newer_republic`
  / `bounds`, `#[serde(default)]` = `needs_newer_republic`) drives the
  Organization modal; co-equal for MCP (`status`).

## 2. Code sites

molt-core
- `vault.rs`: `VaultRefusal::FoundingOnly` -> `NeedsNewerRepublic`;
  new `VaultEnable` enum; byte-pin/text tests.
- `lib.rs` `StatusView`: `vault_enable: VaultEnable` (serde default).

molt-engine
- `vault/mod.rs`: `check_roster_keys` - keys need every seat keyed,
  canonical, unique; bounds and the `vault` feature only when the
  feature is selected (keys without `vault` are the prepared vault).
  `check_new_founding_keys` unchanged in shape (`vault` without keys
  refused). New `walk_enabled(state, ctx)`, `State::vault_founding_ctx`
  (= old `vault_ctx`, prepared), `is_vault_prepared`, `vault_ctx` /
  `is_vault_republic` become ENABLED-only, `require_vault()`,
  `vault_enable()`.
- `founding.rs`: `full_identities` never drops keys; join door refuses an
  empty `vault_pk` (`needs a newer version`); `cmd_create_propose`
  refuses a keyless seat always (NeedsNewerVersion), bounds only when
  `vault` is ticked.
- `chain/verify.rs` `fold_one`: in a prepared chain a Vault `Applied`
  block needs `walk_enabled` (`block N: vault not enabled`).
- `chain/governance.rs`: `cut_kind` and `receive_proposed` use prepared
  / the new refusal.
- `proposals.rs`: `adds_vault_feature` -> `vault_enable_refusal`
  (propose, approve); generic propose on Vault in a prepared republic
  refuses via `require_vault` first, then `use vault_seal`; snapshot
  `vault` is `None` while prepared-not-enabled; `status().vault_enable`.
- `net/ingest.rs`: wire drop uses `vault_enable_refusal`.
- `vault/fold.rs`, `session.rs` (seed re-derive): prepared.
- `vault/deposit.rs`, `grant.rs`, `read.rs`, `receipts.rs` commands:
  `require_vault()` first; `vault_wire_check` judges shape with the
  prepared context; `receipts::tick` and `net/vault_payload.rs` run only
  when enabled (no frames while disabled).
- Org apply hook: when an applied `set_features` turns the vault on, run
  the holder check for pending deposit cards.

molt-mcp: `propose` + `create_propose` descriptions and the server
instructions (`founding only` -> `prepared at founding, enabled by a
set_features vote; needs 2 <= m <= n-2`).

molt-ui / molt-ui-window: `org-vault-enable` (int) + `of-vault-draft`;
the modal offers a Vault box only on `offer`, shows the locked reason
otherwise (`needs a newer republic` / `needs 2 <= m <= n-2`), a checked
locked box when on; `vault_founding_only` string -> `vault_newer_republic`;
the Organization charter list drops `(ui mock)` on a real vault.

scripts/vault_lab.py: `join --no-vault` (headless founder proposes
`memory` only), `enable [--by seat]` (a `set_features` vote adding
`vault`); recipe in spec-adjacent doc and this plan.

Docs: spec rev 5 (D11 rewritten, D14 extended, §3, §6, §12),
`charter_features.md` amendment line, `known_debt.md` line, CLAUDE.md
roster-v6 line.

## 3. Red tests (written first, each seen failing)

molt-engine unit
- `founding::create_propose_keys_the_table_without_the_vault` - a
  memory-only charter keeps every seat's key (E2).
- `founding::create_propose_prepares_outside_the_bounds` - 2-of-3
  memory-only proposes a keyed table (E2, bounds only gate the tick).
- `founding::create_propose_refuses_an_older_joiner_without_the_vault`
  - NeedsNewerVersion with `memory` (E3).
- `founding::a_join_without_a_vault_key_is_refused` (was the tail of
  `a_join_validates_the_vault_key`) - empty `vault_pk` is not anchored,
  the log names `needs a newer version` (E3).
- `founding::verify_seal_proposal_accepts_vault_keys_without_the_vault_feature`
  (flips `..._rejects_...`), at 2-of-4 and 2-of-3 (E2).
- `vault::tests::set_features_enables_the_vault_in_a_prepared_republic`
  - propose, approve and the wire take it (E4).
- `vault::tests::set_features_vault_needs_a_newer_republic_on_v5` (E4).
- `vault::tests::set_features_vault_refuses_outside_the_bounds` (E4).
- `vault::tests::a_historic_set_features_vault_block_still_verifies` -
  v5 chain with the block verifies (E4).
- `vault::tests::the_vault_is_hidden_until_enabled` - snapshot without
  `vault`, seal/reseal/grant/read refuse `vault: not enabled`, approve
  of a vault card refused; after the enabling block: real view (E5).
- `deposit_tests::verify_chain_rejects_a_vault_block_before_the_enabling_block`
  - deposit and grant blocks rejected, the same deposit after the enable
  block verifies; also through `extend_own` (block ingest) (E5).
- `deposit_tests::an_out_of_bounds_enable_block_enables_nothing` - a
  committed 2-of-3 enable verifies, a deposit after it is rejected (E5).

molt-engine integration (`tests/vault_late_enable.rs`, real relay)
- `a_founding_without_the_vault_is_prepared_but_hidden` - roster-v6,
  every seed persisted and deriving its key, `read_state` no vault,
  `vault_seal` refused `vault: not enabled` (replaces
  `vault_founding.rs::a_vaultless_founding_stays_byte_identical`; the
  legacy keyless v5 byte identity stays pinned by the core byte-pin
  `roster_bytes_are_unchanged_by_the_vault_pk_field` and
  `verify_seal_proposal_accepts_a_keyless_table_from_an_older_founder`).
- `cut_then_enable_then_seal_grant_read` - cut before enabling (empty
  base), enable vote, deposit, grant, read (E5).
- `enable_then_cut_then_recover_and_read` - enable, deposit, cut, `d`
  recovers, grant to `d`, it reads (E5).

molt-mcp (`tests/vault_tools.rs`)
- `vault_tools_refuse_until_enabled` - every vault tool refuses
  `vault: not enabled`, `read_state(vault)` carries no `vault`, `status`
  reports `vault_enable = offer`.

molt-ui (live-preview headless)
- `vault_charter::the_vault_box_is_off_by_default`.
- `vault_charter::the_org_modal_offers_the_vault_when_it_can_be_enabled`
  - the box ticks and the proposal carries `vault`.
- `vault_charter::the_org_modal_names_why_the_vault_is_locked` - both
  reasons, no box.
- `vault::the_vault_nav_hides_until_the_enabling_block` - real engine,
  on-disk v6 genesis without `vault`: no tab; with a committed enable
  block: tab and real pane.

Updated, not weakened: `vault_founding.rs` byte-identity test (see
above), `founding::create_propose_keys_the_table_only_for_a_vault_charter`,
`..._rejects_vault_keys_without_the_vault_feature`, the `founding only`
tests in `vault/mod.rs` and the UI, `deposit_tests::a_deposit_is_refused_outside_a_vault_republic`,
the adversarial join tests (now send a key), i18n tests.

## 4. Verification

```
cargo test -j 2 -p molt-core
cargo test -j 2 -p molt-vault
cargo test -j 2 -p molt-storage
cargo test -j 2 -p molt-net
cargo test -j 2 -p molt-engine
cargo test -j 2 -p molt-mcp
CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo test -j 2 -p molt-ui --lib --features molt-ui/live-preview
cargo clippy -j 2 -p <crate> --all-targets            # each crate above; engine also --features vault-lab
CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo clippy -j 2 -p molt-ui --all-targets --features molt-ui/live-preview
scripts/dev-ui.sh build
CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo check -j 2 -p molt-app --tests --features live-preview,vault-lab
CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo build -j 2 -p molt-app --features live-preview,vault-lab
# headless lab smoke, late enable:
cargo run -p molt-net --example dev_relay &
python3 scripts/vault_lab.py up --relay <url> --founder-headless
python3 scripts/vault_lab.py join --no-vault
python3 scripts/vault_lab.py seal f a one     # refused: vault: not enabled
python3 scripts/vault_lab.py enable && python3 scripts/vault_lab.py approve
python3 scripts/vault_lab.py seal f a one && python3 scripts/vault_lab.py approve
python3 scripts/vault_lab.py grant a s1 && python3 scripts/vault_lab.py approve
python3 scripts/vault_lab.py read s1 a        # prints: one
python3 scripts/vault_lab.py down
python3 scripts/check-doc-refs.py
```

Never the full window build, never `cargo test --workspace`, `-j 2`.

## 5. Manual recipe

`docs_archive/vault/vault_build_plan.md` §5, amended: steps 4 and 8
changed, steps 11-13 and the headless line added (found without the
vault, enable by the Organization features vote, then seal, grant,
read). The founding-with-vault path (steps 4-7) stays.
