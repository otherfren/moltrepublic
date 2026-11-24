// SPDX-License-Identifier: GPL-3.0-or-later

//! Passive presence and net health: the supervisor's peer-seen / rekeyed /
//! send-failed / link signals, the last-seen stamps and pill aging, the
//! `NetHealth` verdict folded from the link maps and the group runtime's
//! report, and the presence tick.

use super::*;

/// The `net_health` reason of an evicted seat.
pub(crate) const SEAT_EVICTED: &str = "seat evicted";

/// B3: a group runtime's lock-out verdict.
#[derive(Debug, Default)]
pub(crate) struct LockOut {
    pub(crate) active: bool,
    /// `live_opaque` when the last lock-out cleared: only growth past it
    /// is new evidence.
    pub(crate) mark: u64,
}

/// How long the unreadable count must stand still before its episode ends:
/// past the 600 s resend ceiling, so an older peer re-offering a tail this
/// seat cannot read stays one episode.
pub(crate) const UNREADABLE_QUIET_SECS: u64 = 900;

/// B2: a run of growing unreadable live frames, one WARN and one INFO.
#[derive(Debug, Default)]
pub(crate) struct UnreadableEpisode {
    seen: u64,
    /// The count before the episode began; `None` = no episode.
    base: Option<u64>,
    grew_at: u64,
}

/// What one tick of [`UnreadableEpisode`] reports.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UnreadableNote {
    Started { count: u64 },
    Ended { total: u64 },
}

impl UnreadableEpisode {
    pub(crate) fn tick(&mut self, n: u64, now: u64) -> Option<UnreadableNote> {
        let before = std::mem::replace(&mut self.seen, n);
        if n > before {
            self.grew_at = now;
            if self.base.is_none() {
                self.base = Some(before);
                return Some(UnreadableNote::Started { count: n });
            }
            return None;
        }
        let base = self.base?;
        if now.saturating_sub(self.grew_at) < UNREADABLE_QUIET_SECS {
            return None;
        }
        self.base = None;
        Some(UnreadableNote::Ended { total: n.saturating_sub(base) })
    }
}

/// The 0/1/2 pill derivation of the ACTIVE workspace, shared by
/// [`State::presence_of`] (one member, on demand) and the tick's
/// `refresh_member_pills` (every pill): THIS node is always online (it is
/// the one running - it never hears itself on the wire, so its stamp would
/// otherwise age out); a send-failure pin forces offline; everyone else
/// ages from their real last-seen stamp. `coarse` is §6.5 (N5.5): presence
/// over relays is traffic-derived and COARSE - short silence is not absence
/// (no keepalives by design), so a stamped member ages to stale. The lift
/// ends at `COARSE_SECS`: past a week silence IS absence, and a seat
/// carrying only its founding stamp must not glow yellow for ever.
pub(crate) fn pill_state(
    me: &str,
    unreachable: &std::collections::HashSet<MemberId>,
    coarse: bool,
    member: &str,
    last_seen: u64,
    now: u64,
) -> u8 {
    if member == me {
        0
    } else if unreachable.contains(member) {
        2
    } else {
        let s = molt_core::presence_state(now, last_seen);
        if s == 2
            && coarse
            && last_seen != molt_core::MemberInfo::NEVER
            && now.saturating_sub(last_seen) <= molt_core::MemberInfo::COARSE_SECS
        {
            1
        } else {
            s
        }
    }
}

impl State {
    /// Passive presence: stamp the member with the engine clock's real
    /// unix time (authenticated inbound traffic is the ONLY thing that
    /// moves a stamp) and lift a send-failure pin.
    pub(crate) fn cmd_net_peer_seen(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.delivery.unreachable.remove(&member);
            let now = self.presence_now();
            self.stamp_member_pill(&member, now);
            // before the health fold, which would re-raise on the old mark
            let cleared = self.clear_lockout(&member);
            self.recompute_net_health();
            let was_silent = self.group_net.as_ref().is_some_and(|g| g.handle.reach_silent(&member));
            if cleared || was_silent {
                self.push_outbox_reach();
            }
            self.genesis_heard(&member);
        }
        Ok(Reply::Ack)
    }

    /// End the genesis re-publish once every other seat spoke in the
    /// running group: a seat there already holds the genesis.
    fn genesis_heard(&mut self, member: &str) {
        if self.genesis_stop.is_none() || !self.genesis_unheard.remove(member) || !self.genesis_unheard.is_empty() {
            return;
        }
        if let Some(stop) = self.genesis_stop.take() {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        tracing::info!("genesis_republish=done");
    }

    /// B3: reading a peer's frame ends this seat's lock-out; `true` when
    /// one was active.
    fn clear_lockout(&mut self, by: &MemberId) -> bool {
        let Some(g) = self.group_net.as_mut() else {
            return false;
        };
        if !g.lockout.active {
            return false;
        }
        g.lockout.active = false;
        g.lockout.mark = g.health.borrow().live_opaque;
        tracing::info!(%by, "seat_locked_out_cleared");
        true
    }

    /// This seat cannot reach its group: evicted, or locked out until it
    /// reads a peer again. New writes would only pile up unsent.
    pub(crate) fn cannot_reach_group(&self) -> bool {
        self.delivery.evicted || self.group_net.as_ref().is_some_and(|g| g.lockout.active)
    }

    /// Owed peers not heard within the activity window: the chain's own
    /// `silent` verdict, or the restore gates' test-seam window.
    fn silent_peers(&self) -> std::collections::BTreeSet<MemberId> {
        match self.seat_active.load(std::sync::atomic::Ordering::SeqCst) {
            u64::MAX => self.chain_lag().silent.into_iter().map(|s| s.member).collect(),
            window => {
                let (now, me) = (self.presence_now(), self.member());
                self.roster()
                    .into_iter()
                    .filter(|m| *m != me)
                    .filter(|m| {
                        let seen = self.member_last_seen(m);
                        seen == molt_core::MemberInfo::NEVER || now.saturating_sub(seen) >= window
                    })
                    .collect()
            }
        }
    }

    /// Hand the group outbox who it can reach (no-op when unchanged).
    pub(crate) fn push_outbox_reach(&mut self) {
        let Some(g) = self.group_net.as_ref() else {
            return;
        };
        let reach = molt_net::group_runtime::OutboxReach {
            silent: self.silent_peers(),
            locked_out: self.cannot_reach_group(),
        };
        g.handle.set_reach(reach);
    }

    /// A merged re-key commit re-admitted `member` as a new incarnation
    /// (fresh log seq space): forget its accept window. Arrives over the
    /// transport's ORDERED inbound path, so the reset lands before any of
    /// the new incarnation's envelopes — the race the announce-/block-side
    /// resets could lose on a bystander catching up from a backlog (live
    /// incident 2026-08-09 §2, field rerun 2026-08-17). A member never in
    /// the roster carries no window, so no roster check is needed; the own
    /// seat never rides an add-proposal this node merges about itself
    /// mid-session.
    pub(crate) fn cmd_net_peer_rekeyed(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        // the group's own list is the source; the message only wakes it
        if let Some(mls) = self.group_mls() {
            self.drain_rekeys(&mls);
        } else if self.net_generation_current(generation) {
            self.rekey_reset(&member);
        }
        Ok(Reply::Ack)
    }

    /// B2: a merged commit removed this seat's own leaf - its seat runs on
    /// another device. Honoured only when the live group really is
    /// inactive, so a stale runtime cannot silence the next workspace.
    pub(crate) fn cmd_net_evicted(
        &mut self,
        by: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        let inactive = self
            .group_net
            .as_ref()
            .is_some_and(|g| g.mls.lock().is_ok_and(|m| m.is_evicted()));
        if self.net_generation_current(generation) && inactive {
            self.enter_evicted(&by);
        }
        Ok(Reply::Ack)
    }

    /// Stop the group runtime for good (no resends, no publishes), keep the
    /// inactive group on disk so a restart stays quiet, and say so once.
    pub(crate) fn enter_evicted(&mut self, by: &str) {
        if let Some(group) = self.group_net.take() {
            let mls = group.mls.clone();
            tokio::spawn(async move { group.handle.shutdown().await });
            let snap = self.ratchet_snapshot(&mls);
            if let (Some(active), Some(snap)) = (self.active.as_ref(), snap) {
                let _ = active.handle.merge_mls_async(snap);
            }
            self.group_mls_parked = Some(mls);
        }
        if !self.delivery.evicted {
            self.delivery.evicted = true;
            tracing::warn!(by, "seat_evicted");
            self.session.notice = "seat-evicted".to_string();
        }
        self.session.net_health = self.group_down();
        self.emit_session(SessionScope::Full);
    }

    /// B3: this seat cannot take part and will not ask for its own restore
    /// (that re-keyed live seats in the field): one notice and one WARN per
    /// session; the operator rejoins with a link.
    pub(crate) fn note_locked_out(&mut self, reason: &str) {
        if self.delivery.locked_out_noted {
            return;
        }
        self.delivery.locked_out_noted = true;
        tracing::warn!(reason, "seat_locked_out");
        self.session.notice = "seat-locked-out".to_string();
        self.emit_session(SessionScope::Full);
    }

    /// B2: live frames no held key opens, one WARN when an episode starts
    /// and one INFO when it ends ([`UNREADABLE_QUIET_SECS`] without growth).
    pub(crate) fn note_unreadable_tick(&mut self) {
        let now = self.presence_now();
        let Some(g) = self.group_net.as_mut() else {
            return;
        };
        let n = g.health.borrow().live_opaque;
        match g.unreadable.tick(n, now) {
            Some(UnreadableNote::Started { count }) => tracing::warn!(count, "group_unreadable"),
            Some(UnreadableNote::Ended { total }) => tracing::info!(total, "group_unreadable_end"),
            None => {}
        }
    }

    /// The verdict of a Nostr workspace whose group runtime is not up.
    pub(crate) fn group_down(&self) -> molt_core::NetHealth {
        molt_core::NetHealth::Down {
            reason: if self.delivery.evicted {
                SEAT_EVICTED.to_string()
            } else {
                crate::session::NOSTR_RUNTIME_PENDING.to_string()
            },
        }
    }

    /// Transport trouble: pin the member's pill unreachable AND flag the
    /// outbound leg stuck (Stage B: the endless-backoff outbox — e.g. the
    /// 2026-07-19 `SKEY ERR AUTH` loop — becomes a visible `Degraded`, not
    /// one stderr line). The last-seen stamp stays untouched — it records
    /// real sightings only; the presence pin lifts on the next sighting,
    /// the stuck flag only on a successful send (`NetSendOk`).
    pub(crate) fn cmd_net_send_failed(
        &mut self,
        member: MemberId,
        reason: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if !self.net_generation_current(generation) {
            return Ok(Reply::Ack);
        }
        tracing::warn!(%member, %reason, "sends to a member keep failing - outbox is backing off");
        self.delivery.send_stuck.insert(member.clone(), reason);
        // the group runtime names the OWN seat for its broadcast outbox: a
        // presence pin on this node could never lift (nothing sights itself)
        if member != self.member() {
            self.delivery.unreachable.insert(member);
        }
        self.refresh_member_pills();
        self.recompute_net_health();
        Ok(Reply::Ack)
    }

    /// The watchdog confirmed a member's inbound leg (subscription live):
    /// clear its degraded state (Stage B).
    pub(crate) fn cmd_net_link_up(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.delivery.link_down.remove(&member);
            // delivery guarantee §4.3: a (re)established leg gets an ACK right
            // away (the next presence tick flushes it), so a peer resuming or
            // rewinding trims its resend range to what this node still misses
            if self.delivery.accepted.contains_key(&member) {
                self.delivery.ack_due.insert(member.clone(), self.presence_now());
            }
            self.recompute_net_health();
        }
        Ok(Reply::Ack)
    }

    /// A member's inbound leg died (subscription ended/failed); the
    /// watchdog is re-subscribing — surface it honestly (Stage B).
    pub(crate) fn cmd_net_link_down(
        &mut self,
        member: MemberId,
        reason: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.delivery.link_down.insert(member, reason);
            self.recompute_net_health();
        }
        Ok(Reply::Ack)
    }

    /// A previously backing-off send went through: clear the stuck flag
    /// (Stage B).
    pub(crate) fn cmd_net_send_ok(
        &mut self,
        member: MemberId,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if self.net_generation_current(generation) {
            self.delivery.send_stuck.remove(&member);
            self.recompute_net_health();
        }
        Ok(Reply::Ack)
    }

    /// This member's REAL last-seen stamp in the active workspace, or
    /// [`molt_core::MemberInfo::NEVER`] (= 0) if we've never heard from it
    /// (or it isn't in the active roster). Used by the self-heal liveness
    /// cross-check — a stamp older than a leg's mesh-up means nothing has
    /// been delivered on that leg since it came live.
    pub(crate) fn member_last_seen(&self, member: &str) -> u64 {
        self.session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace)
            .and_then(|w| w.members.iter().find(|m| m.name == member))
            .map_or(molt_core::MemberInfo::NEVER, |m| m.last_seen)
    }

    /// Re-derive `session.net_health` (Track A — honest per-peer status). `Down`
    /// is the open/config path's fail-closed verdict and is NEVER overridden
    /// here. Otherwise a `Degraded` names only the REAL troubles: an inbound leg
    /// the watchdog reported down, or an outbox whose sends keep failing. Only
    /// an honest all-clear is `Ok`. Emits only on an actual change.
    pub(crate) fn recompute_net_health(&mut self) {
        // a Nostr workspace: the group channel's verdict, nothing else —
        // link/send maps are per-peer mesh concepts it never feeds
        if let Some(h) = self.group_net.as_ref().map(|g| g.health.borrow().clone()) {
            self.apply_group_health(h);
            return;
        }
        if matches!(self.session.net_health, molt_core::NetHealth::Down { .. }) {
            return;
        }
        let health = if self.delivery.link_down.is_empty() && self.delivery.send_stuck.is_empty() {
            molt_core::NetHealth::Ok
        } else {
            let parts: Vec<String> = self
                .delivery.link_down
                .iter()
                .map(|(m, r)| format!("link to {m}: {r}"))
                .chain(self.delivery.send_stuck.iter().map(|(m, r)| format!("sends to {m}: {r}")))
                .collect();
            molt_core::NetHealth::Degraded {
                reason: parts.join("; "),
            }
        };
        if self.session.net_health != health {
            self.session.net_health = health;
            self.emit_session(SessionScope::Full);
        }
    }

    /// N5.4/N5.5: fold the GROUP CHANNEL's health into `session.net_health`
    /// — on a relay transport the verdict is about relays, not members
    /// (there are no per-peer legs to be deaf; §6.5).
    ///
    /// Unlike the mesh fold this RECOMPUTES fully, `Down` included: a live
    /// group runtime is itself the proof that the open path's fail-closed
    /// config verdict passed (a refused dialer never builds one), and the
    /// one `Down` this fold owns — a dead subscription — really is terminal
    /// (the inbox loop returned; nothing re-subscribes until reopen).
    pub(crate) fn apply_group_health(&mut self, h: molt_net::group_runtime::GroupHealth) {
        let health = if !h.subscribed {
            molt_core::NetHealth::Down {
                reason: h.deaf.unwrap_or_else(|| "no 445 subscription".to_string()),
            }
        } else {
            let mut parts: Vec<String> = Vec::new();
            if let Some(why) = h.deaf {
                parts.push(format!("relays: {why}"));
            }
            if h.catching_up {
                parts.push("catching up".to_string());
            }
            if h.live_opaque > 0 {
                // G4 (N5.4): LIVE traffic no held key opens is a named loss;
                // a rejoiner's replayed day window is history, not trouble
                parts.push(format!("{} new messages unreadable", h.live_opaque));
            }
            if !self.delivery.behind.is_empty() {
                parts.push(format!("behind: {}", self.delivery.behind.join(", ")));
            }
            if let Some((owed, _)) = &self.delivery.resend_held {
                parts.push(format!("resend budget spent, owed: {}", owed.join(", ")));
            }
            // a stuck broadcast outbox names no peer — the channel is the
            // trouble, so its reason joins the channel verdict
            parts.extend(self.delivery.send_stuck.values().cloned());
            // the deaf-node signature - the OWN outbox stalls (nobody acks)
            // while LIVE frames arrive that no held key opens. B3: reported,
            // never self-healed; the operator rejoins with a link.
            let mark = self.group_net.as_ref().map_or(0, |g| g.lockout.mark);
            if !self.delivery.send_stuck.is_empty() && h.live_opaque > mark {
                self.note_locked_out("key_behind");
                let raised = self.group_net.as_mut().is_some_and(|g| !std::mem::replace(&mut g.lockout.active, true));
                if raised {
                    self.push_outbox_reach();
                }
            }
            if parts.is_empty() {
                molt_core::NetHealth::Ok
            } else {
                molt_core::NetHealth::Degraded { reason: parts.join("; ") }
            }
        };
        if self.session.net_health != health {
            self.session.net_health = health;
            self.emit_session(SessionScope::Full);
        }
    }

    /// The presence ticker (spawned with the actor, period
    /// [`crate::PRESENCE_TICK_MS`]): re-age every pill from its stamp so
    /// a silent member drifts online → stale → offline. The stamps only
    /// ever move on real traffic; reads additionally re-derive live, so
    /// the tick exists for the PUSHED session pills. It also re-evaluates
    /// `net_health` on the same periodic beat.
    pub(crate) fn cmd_net_presence_tick(&mut self) -> Result<Reply, MoltError> {
        self.refresh_member_pills();
        self.recompute_net_health();
        self.note_unreadable_tick();
        self.retry_held_restore_approvals();
        self.recover_wait_tick(tokio::time::Instant::now());
        // the mirror gossip beat (mirroring §3.4) rides this tick as well
        self.mirror_gossip_tick(self.presence_now());
        crate::vault::receipts::tick(self, self.presence_now());
        self.wallet_tick();
        // WP4a: the DAILY compaction beat rides this tick (F8) — expired chat
        // stops existing on this device, it does not merely leave the read
        // filter. Gated to one round a day; the work itself is off-actor.
        self.maybe_compact(self.presence_now());
        Ok(Reply::Ack)
    }

    /// Record a real sighting on the active workspace entry's pill. The
    /// stamp is always advanced (aging + the activity trio read it), and a
    /// full session push fires when the pill STATE changes OR when the
    /// advanced stamp crosses a label-minute boundary. A peer already online
    /// re-stamping every second within the same displayed minute renders an
    /// identical "N min ago" label and is not re-broadcast, but once the
    /// label would change the fresh stamp IS pushed — otherwise the pushed
    /// stamp freezes and the displayed age drifts upward against a still-green
    /// pill (mirrors render the age from the pushed stamp against their own
    /// clock, the `last_sync_min` pattern).
    fn stamp_member_pill(&mut self, member: &MemberId, now: u64) {
        let active = self.session.active_workspace.clone();
        let Some(entry) = self.session.workspaces.iter_mut().find(|w| w.id == active) else {
            return;
        };
        let Some(m) = entry.members.iter_mut().find(|m| m.name == *member) else {
            return;
        };
        let state = molt_core::presence_state(now, now);
        let state_changed = m.state != state;
        // the "N min ago" label renders at minute granularity: a re-stamp that
        // lands in a new minute bucket is what a mirror would draw differently
        let label_advanced = m.last_seen / 60 != now / 60;
        m.state = state;
        m.last_seen = now;
        if state_changed || label_advanced {
            // the same gate keeps the DISK write down to one per displayed
            // minute per member: presence is local knowledge, and without
            // it on disk every restart claims it never saw anyone
            self.remember_seen(vec![(member.clone(), now)]);
            self.emit_session(SessionScope::Full);
        }
    }

    /// Re-derive every pill state — of EVERY known workspace entry, not just
    /// the active one — from each member's stamp, so a switched-away workspace
    /// ages instead of freezing its pills at whatever they were on close.
    /// Self-online and send-failure pins are scoped to the ACTIVE workspace
    /// (the node runs exactly one mesh); a non-active entry ages purely from
    /// stamps. Emits only when a state actually changed.
    fn refresh_member_pills(&mut self) {
        let now = self.presence_now();
        let me = self.member();
        let active = self.session.active_workspace.clone();
        let unreachable = &self.delivery.unreachable;
        // §6.5 (N5.5): the open workspace's transport decides how silence
        // ages — `pill_state`, the one derivation `presence_of` shares
        let coarse = self.nostr.is_some();
        let mut changed = false;
        for entry in &mut self.session.workspaces {
            let is_active = entry.id == active;
            for m in &mut entry.members {
                let state = if is_active {
                    pill_state(&me, unreachable, coarse, &m.name, m.last_seen, now)
                } else {
                    molt_core::presence_state(now, m.last_seen)
                };
                if m.state != state {
                    m.state = state;
                    changed = true;
                }
            }
        }
        if changed {
            self.emit_session(SessionScope::Full);
        }
    }
}
