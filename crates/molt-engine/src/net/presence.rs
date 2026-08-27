// SPDX-License-Identifier: GPL-3.0-or-later

//! Passive presence and net health: the supervisor's peer-seen / rekeyed /
//! send-failed / link signals, the last-seen stamps and pill aging, the
//! `NetHealth` verdict folded from the link maps and the group runtime's
//! report, and the presence tick.

use super::*;

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
            self.recompute_net_health();
        }
        Ok(Reply::Ack)
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
        if self.net_generation_current(generation) {
            tracing::info!(%member, "re-key commit merged - forgetting the seat's old accept window");
            self.reset_peer_accept_window(&member);
        }
        Ok(Reply::Ack)
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
    pub(crate) fn member_last_seen(&self, member: &MemberId) -> u64 {
        self.session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace)
            .and_then(|w| w.members.iter().find(|m| &m.name == member))
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
            if h.opaque_frames > 0 {
                // G4 (N5.4): older than the exporter ring is unreadable BY
                // CONSTRUCTION — a permanent, named loss, never silence
                parts.push(format!("{} frames past the key ring", h.opaque_frames));
            }
            // a stuck broadcast outbox names no peer — the channel is the
            // trouble, so its reason joins the channel verdict
            parts.extend(self.delivery.send_stuck.values().cloned());
            // SELF-HEAL (detached_reattach.md §2.4): the deaf-node signature
            // — the OWN outbox stalls (nobody acks) while frames arrive that
            // no held key opens. A healthy rejoiner counting a laggard's
            // stale frames never stalls, so it never triggers.
            if !self.delivery.send_stuck.is_empty() && h.opaque_frames > 0 {
                self.maybe_self_heal_reattach();
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
