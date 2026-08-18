// SPDX-License-Identifier: GPL-3.0-or-later

//! **The wiki export** (`docs/memory/wiki_export_plan.md`): the Shared-Memory
//! tree written to a user-picked directory as plain files, optionally with the
//! threshold signatures that make it verifiable by an outsider — no moltd, no
//! workspace key, no trust in the exporter.
//!
//! What leaves the workspace is exactly two things: the folded wiki tree, and
//! the blocks that AUTHENTICATE it (the genesis, every `Membership` block, and
//! every applied `wiki_patch`). No other block kind is exported, so no other
//! surface's content rides along.
//!
//! The verifier is [`crate::verify_wiki_export`], beside `verify_chain` — it
//! reuses the real byte layouts, so writer and verifier cannot drift.

use molt_core::{ChainBlock, ChainChange, Surface};
use serde::{Deserialize, Serialize};

/// The bundle's format tag. A verifier that meets an unknown one stops.
pub const WIKI_EXPORT_FORMAT: &str = "molt-wiki-export-v1";

/// `<dest>/proof/bundle.json`: the genesis plus the blocks a reviewer needs to
/// check every exported patch. Additive-only, like every wire shape here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiExportBundle {
    /// [`WIKI_EXPORT_FORMAT`].
    pub format: String,
    /// Block 0: the sealed founding constitution (n-of-n).
    pub genesis: ChainBlock,
    /// Every `Membership` block and every applied `wiki_patch`, ascending by
    /// height. Membership blocks ride along because they are the identity
    /// history: a later patch signed by a seat that joined after the founding
    /// verifies only against the roster those blocks establish.
    #[serde(default)]
    pub blocks: Vec<ChainBlock>,
}

/// Whether a change is an applied wiki patch (Memory surface, `wiki_patch`
/// op) — the ONE predicate the writer's selection and the verifier's
/// admission check share, so a block kind cannot be exported that the
/// verifier would refuse (or the reverse).
pub(crate) fn is_wiki_patch(change: &ChainChange) -> bool {
    matches!(
        change,
        ChainChange::Applied { surface: Surface::Memory, payload, .. }
            if payload.get("op").and_then(serde_json::Value::as_str) == Some("wiki_patch")
    )
}

/// Select the proof bundle out of a verified chain. `None` when the chain does
/// not start at its genesis (an empty chain, or a holder pruned to a
/// checkpoint anchor) — there is then nothing to anchor the signatures in, and
/// a bundle without that anchor would prove nothing.
pub(crate) fn bundle_from_chain(chain: &[ChainBlock]) -> Option<WikiExportBundle> {
    let genesis = chain.first()?;
    if !matches!(genesis.change, ChainChange::Genesis { .. }) {
        return None;
    }
    let blocks = chain
        .iter()
        .skip(1)
        .filter(|b| matches!(b.change, ChainChange::Membership { .. }) || is_wiki_patch(&b.change))
        .cloned()
        .collect();
    Some(WikiExportBundle {
        format: WIKI_EXPORT_FORMAT.to_string(),
        genesis: genesis.clone(),
        blocks,
    })
}
