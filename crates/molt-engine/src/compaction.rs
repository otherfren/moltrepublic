// SPDX-License-Identifier: GPL-3.0-or-later

//! Log compaction, engine half — WP4a (`documents/log_compaction.md` Teil A).
//!
//! After **expiry + grace**, expired chat content must not merely leave the
//! read contract: it has to stop existing on this device. The storage half
//! ([`molt_storage`] segment keys, key erasure, segment drop) does the
//! deleting; this module decides *when* and *what*:
//!
//! * **when** — one round per day per open workspace, plus one on the clean
//!   close (F8), where snapshot and cursors are freshest;
//! * **what** — content older than the retention window PLUS one window of
//!   grace (F2), so nothing vanishes the moment it leaves the read filter;
//! * **whose cursor still counts** — a peer that has been out of contact
//!   longer than two windows (F4) no longer pins the log; it is redirected to
//!   the chain catch-up (§A.1 C2) like any node that fell too far behind.
//!
//! Routine rounds are deliberately NOT voted: an "I compacted" signature is
//! unprovable, a block over local device state would break the ephemeral
//! boundary, and offline devices would not change the outcome. The threshold
//! authority sits in the POLICY (`set_chat_retention`, a gated vote) and in
//! the chain checkpoint (WP4b).

use molt_core::{EngineStateDump, MemberId, WorkspaceSnapshot};

use crate::State;

/// One compaction round per day and per open workspace (F8). Compaction is
/// hygiene, not a deadline: a workspace that is open for an hour a day still
/// gets its round, and a missed one costs nothing but a day of retention.
pub(crate) const COMPACT_EVERY_SECS: u64 = 24 * 60 * 60;

/// Content grace (F2): expired chat is physically dropped one further
/// retention window after it left the read contract, so a policy change or a
/// clock skew cannot delete content that a peer still considers live.
/// Physically gone after 2× the window.
const CONTENT_GRACE_WINDOWS: u64 = 1;

/// Peer grace (F4): a peer whose last contact is older than this many
/// retention windows stops holding the compaction floor. Twice the content
/// grace — a peer gets strictly longer to come back than the content gets to
/// survive.
const PEER_GRACE_WINDOWS: u64 = 2;

impl State {
    /// The instant before which chat content is **physically** dropped:
    /// `now - (1 + grace) × retention window`. Strictly older than the read
    /// filter's cutoff ([`State::chat_retention_cutoff`]), which is what the
    /// grace means.
    pub(crate) fn compaction_cutoff(&self, now: u64) -> u64 {
        let window = self.org_effective().retention_days * 86_400;
        now.saturating_sub(window.saturating_mul(1 + CONTENT_GRACE_WINDOWS))
    }

    /// The peers whose delivery cursor still holds the log back (R2): every
    /// roster member except us that has been in contact within
    /// [`PEER_GRACE_WINDOWS`] retention windows. A peer past that grace is not
    /// forgotten — it catches up over the chain (§A.1 C2) instead of pinning
    /// every segment on every remaining node forever.
    ///
    /// This is deliberately a list of who HOLDS, not of who may be ignored:
    /// the compactor then treats a holding peer with no cursor entry (never
    /// delivered to, or a lost `transport.state`) as "has received nothing"
    /// and drops nothing. The inverse phrasing would silently drop the log
    /// out from under exactly those peers.
    pub(crate) fn peers_holding_the_log(&self, now: u64) -> Vec<MemberId> {
        let window = self.org_effective().retention_days * 86_400;
        let grace = window.saturating_mul(PEER_GRACE_WINDOWS);
        let me = self.member();
        self.roster()
            .into_iter()
            .filter(|m| *m != me)
            .filter(|m| now.saturating_sub(self.member_last_seen(m)) <= grace)
            .collect()
    }

    /// Physically drop expired chat from the LIVE state and return the
    /// trimmed dump to persist as the new snapshot, plus how many messages
    /// went. `None` = nothing to do (or a dump that must not be pruned yet —
    /// see [`EngineStateDump::prune_chat_before`]).
    ///
    /// The live state and the snapshot are trimmed by the SAME call, so the
    /// two can never disagree about what this node still holds.
    pub(crate) fn compact_chat(&mut self, cutoff: u64) -> Option<(EngineStateDump, usize)> {
        let mut dump = self.dump();
        let dropped = dump.prune_chat_before(cutoff);
        if dropped == 0 {
            return None;
        }
        self.chat = dump.chat.clone();
        self.chat_pruned = dump.chat_pruned;
        self.chat_pruned_counts = dump.chat_pruned_counts.clone();
        self.chat_pos = self
            .chat
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id, i))
            .collect();
        // §A.1 C4 (Etappe 4, "share forgetting"): a share whose message is
        // gone is gone with it — both the runtime map and the persistent
        // `prefs.shared_files` sidecar, so a restart cannot resurrect it. The
        // user's own FILE is never touched; what falls is our record that we
        // serve it, after which a download request meets the honest
        // `Refused`/`FileExpired` the read path already produces.
        self.share_paths.retain(|id, _| self.chat_pos.contains_key(id));
        if let Some(active) = self.active.as_mut() {
            let before = active.prefs.shared_files.len();
            let live: std::collections::HashSet<String> =
                dump.chat.iter().map(|m| m.id.to_string()).collect();
            active.prefs.shared_files.retain(|id, _| live.contains(id));
            if active.prefs.shared_files.len() != before {
                active.handle.set_prefs(active.prefs.clone());
            }
        }
        Some((dump, dropped))
    }

    /// Whether a compaction round is due (F8: daily). Split out so the gate
    /// is unit-testable without a workspace on disk.
    pub(crate) fn compaction_due(&self, now: u64) -> bool {
        self.active.is_some() && now.saturating_sub(self.compacted_at) >= COMPACT_EVERY_SECS
    }

    /// Run one compaction round if it is due (the daily beat rides the
    /// presence tick — no separate ticker, no new `Command`). Trimming the
    /// live state is cheap and happens on the actor; the disk work (snapshot,
    /// key erasure, segment drop) runs on the writer thread through a
    /// blocking call, so it is handed to the blocking pool and never stalls
    /// the actor.
    pub(crate) fn maybe_compact(&mut self, now: u64) {
        if !self.compaction_due(now) {
            return;
        }
        self.compacted_at = now;
        self.compact_now(now, false);
    }

    /// One compaction round, unconditionally (the daily beat and the clean
    /// close share it).
    ///
    /// `wait` decides who does the disk work: the daily beat hands it to the
    /// blocking pool so the actor keeps serving commands, while the CLOSE path
    /// must wait — the writer is about to be shut down, and an un-awaited
    /// round would lose the race against `Close` and silently never happen.
    pub(crate) fn compact_now(&mut self, now: u64, wait: bool) {
        let cutoff = self.compaction_cutoff(now);
        let Some((dump, dropped)) = self.compact_chat(cutoff) else {
            return;
        };
        let holding_peers = self.peers_holding_the_log(now);
        let snapshot = WorkspaceSnapshot {
            version: molt_core::STORAGE_VERSION,
            at_seq: self.next_seq.saturating_sub(1),
            state: dump,
        };
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let handle = active.handle.clone();
        tracing::info!(dropped, cutoff, "compacting the ephemeral log");
        let run = move || {
            let out = handle.compact_blocking(snapshot, holding_peers);
            if out.segments_dropped > 0 {
                tracing::info!(
                    floor = out.floor,
                    segments = out.segments_dropped,
                    "log compaction dropped segments"
                );
            }
        };
        if wait {
            run();
        } else {
            tokio::task::spawn_blocking(run);
        }
        // a dropped message must leave the readers' views now, not at the
        // next open — the session push is what makes them re-read
        self.emit_session(molt_core::SessionScope::Full);
    }
}

#[cfg(test)]
mod tests {
    use molt_core::ChatMessage;

    use crate::tests::plain_state;

    /// The grace is the whole point of F2: content leaves the READ contract at
    /// the retention window, and only one further window later does it stop
    /// existing. The compaction cutoff must therefore always sit strictly
    /// behind the read cutoff.
    #[test]
    fn the_compaction_cutoff_trails_the_read_filter_by_a_full_grace_window() {
        let st = plain_state();
        let now = 100 * 86_400;
        let window = st.org_effective().retention_days * 86_400;
        assert_eq!(st.compaction_cutoff(now), now - 2 * window);
        assert!(
            st.compaction_cutoff(now) < now - window,
            "physical deletion trails the read filter"
        );
        // and it can never wrap into the future on a young workspace
        assert_eq!(st.compaction_cutoff(10), 0);
    }

    /// A compaction round trims the LIVE state and the snapshot with the same
    /// call, so the two cannot disagree: the dropped messages are gone from
    /// `chat`, from `chat_pos`, and from the shares this node serves, while
    /// everything inside the window is untouched.
    #[test]
    fn compacting_trims_live_state_and_snapshot_together() {
        let mut st = plain_state();
        let old = ChatMessage::text(molt_core::MessageId([1u8; 16]), "petra", "ancient", 100);
        let new = ChatMessage::text(molt_core::MessageId([2u8; 16]), "petra", "recent", 5_000);
        for (seq, m) in [(1u64, old.clone()), (2, new.clone())] {
            st.apply(&molt_core::EventEnvelope { prev_seq: 0,
                seq,
                ts: m.ts,
                by: "petra".to_string(),
                body: molt_core::WorkspaceEvent::Chat(m),
            });
        }
        st.share_paths.insert(old.id, std::path::PathBuf::from("/tmp/ancient.pdf"));
        st.share_paths.insert(new.id, std::path::PathBuf::from("/tmp/recent.pdf"));

        let (dump, dropped) = st.compact_chat(1_000).expect("something ages out");
        assert_eq!(dropped, 1);
        assert_eq!(dump.chat.len(), 1, "the snapshot carries only the survivor");
        assert_eq!(st.chat.len(), 1, "and so does the live state");
        assert_eq!(st.chat[0].id, new.id);
        assert_eq!(st.chat_pos.get(&new.id), Some(&0), "chat_pos was rebuilt");
        assert!(!st.chat_pos.contains_key(&old.id));
        assert!(st.chat_pruned, "the node is marked pruned (positions are dead)");
        assert_eq!(st.chat_pruned_counts.get("petra").copied(), Some(1));
        assert!(
            !st.share_paths.contains_key(&old.id),
            "the dropped message's share is forgotten with it"
        );
        assert!(st.share_paths.contains_key(&new.id), "a live share stays");

        // a second round with nothing eligible is a no-op
        assert!(st.compact_chat(1_000).is_none());
    }

    /// Etappe 4 (§A.1 C4): the share record of a dropped message falls with
    /// it in the PERSISTENT sidecar too, not just in the runtime map — a
    /// restart must not resurrect a share whose message no longer exists. The
    /// user's own file is never touched; what goes is our record of serving
    /// it, after which a download meets the honest refusal the read path
    /// already produces.
    #[test]
    fn compaction_forgets_the_dropped_shares_in_the_prefs_sidecar() {
        let tmp = tempfile::tempdir().expect("tmp");
        let seed =
            molt_storage::seed_entropy(&molt_storage::generate_seed_phrase().expect("phrase"))
                .expect("entropy");
        let genesis = molt_core::EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 10,
            by: "me".to_string(),
            body: molt_core::WorkspaceEvent::Founded {
                name: "Compaction".to_string(),
                rule_m: 1,
                rule_n: 1,
                member: "me".to_string(),
                roster: vec!["me".to_string()],
                identities: Vec::new(),
                attestations: Vec::new(),
                republic_id: String::new(),
                agenda: String::new(),
            },
        };
        let ws = molt_storage::create_workspace(tmp.path(), &seed, &genesis).expect("create");
        let dir = ws.dir().to_path_buf();
        let mut st = plain_state();
        let old = ChatMessage::text(molt_core::MessageId([1u8; 16]), "me", "ancient", 100);
        let new = ChatMessage::text(molt_core::MessageId([2u8; 16]), "me", "recent", 5_000);
        st.apply(&molt_core::EventEnvelope { prev_seq: 0,
            seq: 1,
            ts: 10,
            by: "me".to_string(),
            body: genesis.body.clone(),
        });
        for (seq, m) in [(2u64, old.clone()), (3, new.clone())] {
            st.apply(&molt_core::EventEnvelope { prev_seq: 0,
                seq,
                ts: m.ts,
                by: "me".to_string(),
                body: molt_core::WorkspaceEvent::Chat(m),
            });
        }
        let mut prefs = molt_core::WorkspacePrefs::default();
        prefs.shared_files.insert(old.id.to_string(), "/tmp/ancient.pdf".to_string());
        prefs.shared_files.insert(new.id.to_string(), "/tmp/recent.pdf".to_string());
        st.active = Some(crate::ActiveStorage {
            id: "w-compact".to_string(),
            dir,
            prefs,
            handle: molt_storage::start_writer(ws),
        });

        st.compact_chat(1_000).expect("something ages out");
        let live = &st.active.as_ref().expect("active").prefs.shared_files;
        assert!(!live.contains_key(&old.id.to_string()), "the dropped share is forgotten");
        assert!(live.contains_key(&new.id.to_string()), "the live share stays");
        st.active.take().expect("active").handle.close(None);
    }

    /// F4: a peer that is merely quiet still pins the log; one that has been
    /// out of contact past the peer grace does not — it catches up over the
    /// chain instead of freezing every segment on every node forever.
    #[test]
    fn only_peers_past_the_grace_stop_holding_the_log() {
        let mut st = plain_state();
        let now = 100 * 86_400;
        let window = st.org_effective().retention_days * 86_400;
        let roster = vec!["me".to_string(), "quiet".to_string(), "gone".to_string()];
        st.replica = Some(crate::ReplicaState {
            member: "me".to_string(),
            roster: roster.clone(),
            rule_m: 2,
            ..Default::default()
        });
        let id = "w-compact".to_string();
        st.session.active_workspace = id.clone();
        let mut entry = molt_core::WorkspaceInfo {
            id,
            name: "Compaction".to_string(),
            detail: "2-of-3".to_string(),
            synced: true,
            state: 0,
            last_sync_min: 0,
            sync_queue: 0,
            s3: false,
            size_kib: 0,
            last_backup_min: molt_core::WorkspaceInfo::NEVER,
            backup_copies: 0,
            backup_error: String::new(),
            seed: String::new(),
            net: "none".to_string(),
            encrypted: false,
            members: molt_core::roster_members(&roster, now, |_| molt_core::MemberInfo::NEVER),
            agenda: String::new(),
        };
        for m in &mut entry.members {
            m.last_seen = match m.name.as_str() {
                "quiet" => now - window,     // inside the grace
                "gone" => now - 3 * window,  // past it
                _ => now,
            };
        }
        st.session.workspaces.push(entry);
        let holding = st.peers_holding_the_log(now);
        assert!(holding.contains(&"quiet".to_string()), "a merely quiet peer still holds");
        assert!(!holding.contains(&"gone".to_string()), "the long-gone peer is released");
        assert!(!holding.contains(&st.member()), "our own cursor never holds us back");
    }

    /// The daily gate (F8): without an open workspace nothing runs at all, and
    /// a round that just ran is not repeated until the day is up.
    #[test]
    fn the_daily_gate_holds() {
        let mut st = plain_state();
        assert!(!st.compaction_due(super::COMPACT_EVERY_SECS * 5), "no workspace, no round");
        st.compacted_at = 1_000;
        assert!(!st.compaction_due(1_000 + super::COMPACT_EVERY_SECS - 1));
    }
}
