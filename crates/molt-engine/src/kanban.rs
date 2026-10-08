// SPDX-License-Identifier: GPL-3.0-or-later

//! The kanban board in the engine (`docs/kanban/kanban_workflows.md` §4,
//! §5.4, §9 S2): canonicalization and the precheck at propose, the fold
//! cache over the applied Quests log (the wiki fold-cache precedent), the
//! derived dates cached per `(rev, today)`, and what a card shows.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use molt_core::kanban_dates::{derive, Derived};
use molt_core::kanban_fold::{
    kanban_canonicalize, kanban_fold_one, kanban_precheck, validate_kanban_wire, BoardState,
    FoldStep,
};
use molt_core::kanban_review::{advisories, board_view};
use molt_core::{MoltError, Surface};
use serde_json::Value;

use crate::State;

/// The folded board, kept across reads. A pure derivation of the applied
/// Quests logs: dropping it costs a refold, never a different board.
#[derive(Clone)]
pub(crate) struct KanbanCache {
    pub(crate) board: BoardState,
    /// Entries folded from the legacy / the chain log (concat order).
    legacy: usize,
    chain: usize,
    epoch: u64,
    seats: BTreeSet<String>,
    /// The proposal behind each fold revision, where known.
    rev_proposal: BTreeMap<u64, u64>,
    /// Applied changesets the fold voided, by proposal: the reason.
    void: BTreeMap<u64, String>,
    /// `derive(board, today)` for the revision it was taken at.
    derived: Option<(u64, NaiveDate, Derived)>,
}

impl KanbanCache {
    fn empty(legacy: usize, chain: usize, epoch: u64, seats: BTreeSet<String>) -> KanbanCache {
        KanbanCache {
            board: BoardState::default(),
            legacy,
            chain,
            epoch,
            seats,
            rev_proposal: BTreeMap::new(),
            void: BTreeMap::new(),
            derived: None,
        }
    }

    /// THE fold step, for the full fold and the extension alike.
    fn step(&mut self, id: Option<u64>, payload: &Value) {
        let step = kanban_fold_one(&mut self.board, payload, &self.seats);
        if step == FoldStep::Skipped {
            return;
        }
        if let Some(id) = id {
            self.rev_proposal.insert(self.board.rev, id);
            if let FoldStep::Void(reason) = step {
                self.void.insert(id, reason.0);
            }
        }
    }

    fn derived_for(&self, today: NaiveDate) -> Option<&Derived> {
        self.derived
            .as_ref()
            .filter(|(rev, day, _)| *rev == self.board.rev && *day == today)
            .map(|(_, _, d)| d)
    }
}

impl State {
    fn quests_lens(&self) -> (usize, usize) {
        (
            self.applied.get(&Surface::Quests).map_or(0, Vec::len),
            self.chain.applied.get(&Surface::Quests).map_or(0, Vec::len),
        )
    }

    fn kanban_seats(&self) -> BTreeSet<String> {
        self.roster().into_iter().collect()
    }

    /// Today in UTC (§2.3), on the presence clock so tests can pin it.
    pub(crate) fn kanban_today(&self) -> NaiveDate {
        let secs = i64::try_from(self.presence_now()).unwrap_or(i64::MAX);
        chrono::DateTime::from_timestamp(secs, 0)
            .map_or(NaiveDate::MIN, |t| t.date_naive())
    }

    fn kanban_cache_current(&self, c: &KanbanCache) -> bool {
        c.epoch == self.applied_epoch
            && (c.legacy, c.chain) == self.quests_lens()
            && c.seats == self.kanban_seats()
    }

    fn fold_kanban(&self) -> KanbanCache {
        let (legacy, chain) = self.quests_lens();
        let mut cache = KanbanCache::empty(legacy, chain, self.applied_epoch, self.kanban_seats());
        for (id, payload) in self.applied_entries(Surface::Quests) {
            cache.step(id, payload);
        }
        cache
    }

    /// [`Self::refresh_kanban_fold`], then derive for today unless that is
    /// cached already.
    pub(crate) fn refresh_kanban_cache(&mut self) {
        self.refresh_kanban_fold();
        let today = self.kanban_today();
        if let Some(cache) = self.kanban_cache.as_mut() {
            if cache.derived_for(today).is_none() {
                cache.derived = Some((cache.board.rev, today, derive(&cache.board, today)));
            }
        }
    }

    /// Bring the fold up to the current logs: extend it by the appended
    /// entries while the other half stood still, else refold.
    pub(crate) fn refresh_kanban_fold(&mut self) {
        let (legacy, chain) = self.quests_lens();
        let seats = self.kanban_seats();
        let epoch = self.applied_epoch;
        let mut cache = self.kanban_cache.take().filter(|c| {
            c.epoch == epoch
                && c.seats == seats
                && ((c.chain == 0 && chain == 0 && c.legacy <= legacy)
                    || (c.legacy == legacy && c.chain <= chain))
        });
        if let Some(c) = cache.as_mut() {
            let tail = self
                .applied
                .get(&Surface::Quests)
                .into_iter()
                .flatten()
                .skip(c.legacy)
                .chain(
                    self.chain
                        .applied
                        .get(&Surface::Quests)
                        .into_iter()
                        .flatten()
                        .skip(c.chain),
                )
                .map(|(id, v)| (*id, v));
            for (id, payload) in tail {
                c.step(id, payload);
            }
            c.legacy = legacy;
            c.chain = chain;
        }
        self.kanban_cache = Some(cache.unwrap_or_else(|| self.fold_kanban()));
    }

    /// The cache while it describes the current logs, else a fresh fold.
    pub(crate) fn kanban_board(&self) -> Cow<'_, KanbanCache> {
        match self.kanban_cache.as_ref() {
            Some(c) if self.kanban_cache_current(c) => Cow::Borrowed(c),
            _ => Cow::Owned(self.fold_kanban()),
        }
    }

    fn kanban_derived<'a>(&self, cache: &'a KanbanCache, today: NaiveDate) -> Cow<'a, Derived> {
        cache
            .derived_for(today)
            .map_or_else(|| Cow::Owned(derive(&cache.board, today)), Cow::Borrowed)
    }

    /// The propose door for a `kanban_ops` changeset (§4.2-§4.4):
    /// canonicalize, then refuse what would void now with the first
    /// reason. Returns the minted ids and the card's advisory lines.
    pub(crate) fn prepare_kanban_proposal(
        &mut self,
        payload: &mut Value,
    ) -> Result<(Vec<String>, Vec<String>), MoltError> {
        let canon = kanban_canonicalize(payload, &self.member(), &mut || {
            crate::chat::mint_message_id()
                .map(|m| hex::encode(m.0))
                .map_err(|e| e.to_string())
        })
        .map_err(MoltError::BadPayload)?;
        self.refresh_kanban_cache();
        let board = self.kanban_board();
        kanban_precheck(&board.board, &canon.payload, &board.seats)
            .map_err(|f| MoltError::BadPayload(f.reason.0))?;
        let warnings = self.kanban_advisories(&canon.payload);
        *payload = canon.payload;
        Ok((canon.minted, warnings))
    }

    /// The wire door (§4.3): the canonical shape, and every `creator` a
    /// roster seat - both node-independent, so peers drop alike.
    pub(crate) fn kanban_wire_check(&self, payload: &Value) -> Result<(), String> {
        validate_kanban_wire(payload)?;
        let seats = self.kanban_seats();
        let creators = payload
            .get("ops")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|op| op.get("creator").and_then(Value::as_str));
        for c in creators {
            if !seats.contains(c) {
                return Err(format!("creator {c} is not a seat"));
            }
        }
        Ok(())
    }

    /// The advisory lines a pending kanban card shows (§4.5).
    pub(crate) fn kanban_advisories(&self, payload: &Value) -> Vec<String> {
        let cache = self.kanban_board();
        let today = self.kanban_today();
        let before = self.kanban_derived(&cache, today);
        advisories(
            &cache.board,
            &before,
            payload,
            &cache.seats,
            today,
            &|rev| cache.rev_proposal.get(&rev).copied(),
        )
    }

    /// Why the fold voided this applied changeset, if it did.
    pub(crate) fn kanban_void(&self, id: u64) -> Option<String> {
        self.kanban_board().void.get(&id).cloned()
    }

    /// The board with its derived status, as the Quests read serves it.
    pub(crate) fn kanban_board_view(&self) -> Value {
        let cache = self.kanban_board();
        let today = self.kanban_today();
        let derived = self.kanban_derived(&cache, today);
        board_view(&cache.board, &derived, today)
    }
}

#[cfg(test)]
mod tests;
