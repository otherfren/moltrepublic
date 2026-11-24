// SPDX-License-Identifier: GPL-3.0-or-later

//! The activation gate of block identity v2 (`block_identity_plan.md` §4):
//! a `set_features` vote naming [`molt_core::chain::BLOCK_V2_FEATURE`] is
//! proposed and signed only once every seat announced
//! [`molt_core::chain::BLOCK_V2_CAP`].

use super::*;

impl State {
    /// The first seat that keeps `block-v2` off, and why; `None` when every
    /// other seat announced the capability.
    pub(crate) fn block_v2_not_capable(&self) -> Option<String> {
        #[cfg(test)]
        if let Some(all) = self.chain.block_v2_caps_override {
            return (!all).then(|| "a seat runs an older build".to_string());
        }
        let me = self.member();
        let cap = molt_core::chain::BLOCK_V2_CAP;
        let known = self.active.as_ref().map(|a| &a.prefs.seat_caps);
        self.roster().into_iter().filter(|m| *m != me).find_map(|m| match known.and_then(|k| k.get(&m)) {
            None => Some(format!("{m} has not announced")),
            Some(c) if !c.iter().any(|c| c == cap) => Some(format!("{m} runs an older build")),
            Some(_) => None,
        })
    }

    /// Why this seat neither proposes nor signs `change` now: it enables
    /// `block-v2` while a seat cannot follow.
    pub(crate) fn block_v2_enable_blocker(&self, change: &ChainChange) -> Option<String> {
        let key = molt_core::chain::BLOCK_V2_FEATURE;
        if !sets_feature(change, key) || self.block_v2_from().is_some() {
            return None;
        }
        self.block_v2_not_capable()
    }

    /// `status.chain_sync`: the activation, and what holds it back.
    pub(crate) fn block_v2_status(&self) -> (u64, String) {
        match self.block_v2_from() {
            Some(h) => (h, String::new()),
            None if self.is_chain_governed() => (0, self.block_v2_not_capable().unwrap_or_default()),
            None => (0, String::new()),
        }
    }
}
