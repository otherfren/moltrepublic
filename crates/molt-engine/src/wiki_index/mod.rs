// SPDX-License-Identifier: GPL-3.0-or-later

//! Derived INDEXES over the folded wiki base
//! (`docs/memory/knowledge_base_scale.md` §4.4-§4.6): front matter, the
//! link graph and full text. Every one of them is a cache over the fold —
//! rebuilt from it, never consensus input, never persisted.
//!
//! Pure functions only; the engine owns the state that holds their results.

pub(crate) mod front_matter;
pub(crate) mod graph;
pub(crate) mod search;
