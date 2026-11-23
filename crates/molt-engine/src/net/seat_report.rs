// SPDX-License-Identifier: GPL-3.0-or-later

//! What each seat says about itself: its build (from its mirror
//! declaration) and the head it last reported (from its catch-up request
//! or beacon). Runtime only: the one who-ask at open makes every peer
//! declare again. Group members only - both ride the MLS group.

use std::collections::HashMap;

use molt_core::MemberId;

use crate::State;

/// This build, as a declaration names it. No commit id: the build embeds none.
pub(crate) const BUILD: &str = env!("CARGO_PKG_VERSION");

/// Hex digits of a head hash in `status.seats[]`.
const HEAD_HASH_HEX: usize = 12;

/// The seats' own reports, as last heard.
#[derive(Debug, Default)]
pub(crate) struct SeatReports {
    /// member -> its build ("" = it declared without one: an older build)
    builds: HashMap<MemberId, String>,
    /// member -> (height, full hash) of the head it last reported
    heads: HashMap<MemberId, (u64, String)>,
}

impl SeatReports {
    pub(crate) fn clear(&mut self) {
        self.builds.clear();
        self.heads.clear();
    }
}

impl State {
    /// A declaration's build; called only when its capabilities are taken
    /// (the same order: the sender's clock).
    pub(crate) fn note_seat_build(&mut self, from: &MemberId, build: &str) {
        self.seat_reports.builds.insert(from.clone(), build.to_string());
    }

    /// The head a peer's own catch-up request names; only the newest
    /// request reaches here (`fresh_chain_request`).
    pub(crate) fn note_seat_head(&mut self, from: &MemberId, top: &molt_core::HeightHash) {
        if *from == self.member() {
            return;
        }
        self.seat_reports.heads.insert(from.clone(), (top.height, top.hash.clone()));
    }

    /// Fill the reported fields of `status.seats[]`.
    pub(crate) fn fill_seat_reports(&self, seats: &mut [molt_core::SeatSync]) {
        let me = self.member();
        let short = |h: &str| h.chars().take(HEAD_HASH_HEX).collect::<String>();
        for seat in seats {
            if seat.member == me {
                seat.build = Some(BUILD.to_string());
                if let Some(head) = self.chain.head.as_ref() {
                    seat.head_hash = Some(short(&head.hash));
                    seat.head_height = Some(head.height);
                }
                continue;
            }
            seat.build = self.seat_reports.builds.get(&seat.member).filter(|b| !b.is_empty()).cloned();
            if let Some((height, hash)) = self.seat_reports.heads.get(&seat.member) {
                seat.head_hash = Some(short(hash));
                seat.head_height = Some(*height);
            }
            seat.diverged = self.chain.diverged.contains_key(&seat.member);
        }
    }
}
