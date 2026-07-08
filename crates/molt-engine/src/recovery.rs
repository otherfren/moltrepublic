// SPDX-License-Identifier: GPL-3.0-or-later

//! The **recovery ritual** transport (see `documents/recovery_ritual.md`): the
//! total-loss twin of the founding ritual. The coordinator/crypto half already
//! lives elsewhere (`Command::NetRecoverRequested`, `cmd_net_recover_requested`,
//! `verify_and_propose_restore`, `coordinator_rekey`); this module builds the
//! transport twin of the founding invite machinery — the recovery link, the
//! `RitualMsg::Recover` wire request, the coordinator recv loop, and the
//! rejoiner activation — mirroring `founding.rs`.
//!
//! Built stepwise, test-first. Today: the recovery **link** type.

/// A recovery link — `molt://recover/<republic>/<member>/<ticket>/<handover>` —
/// mirroring [`crate::FoundingInvite`], but for an *existing* seat. It carries a
/// transport handover (the coordinator's recovery queue) and a single-use ticket
/// the seat proof binds. The `<handover>` segment is
/// `hex(server ‖ '\n' ‖ queue_id ‖ '\n' ‖ wrap)` so the smp URL's `//@=` cannot
/// leak into the path. A link without a handover parses as a preview only and is
/// not actionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryInvite {
    /// The republic's display name (spaces travel as dashes).
    pub republic: String,
    /// The returning member's seat handle.
    pub member: String,
    /// The single-use recovery ticket (lowercase hex).
    pub ticket: String,
    /// The coordinator's recovery-queue server (`smp://fingerprint@host`).
    pub server: String,
    /// The coordinator's recovery-queue send-side id (lowercase hex).
    pub queue_id: String,
    /// The per-queue wrap key (lowercase hex, 32 bytes).
    pub wrap: String,
}

impl RecoveryInvite {
    /// Render the link (preview + hex transport handover).
    pub fn render(&self) -> String {
        let handover = format!("{}\n{}\n{}", self.server, self.queue_id, self.wrap);
        format!(
            "molt://recover/{}/{}/{}/{}",
            self.republic.replace(' ', "-"),
            self.member,
            self.ticket,
            hex::encode(handover),
        )
    }

    /// Parse a `molt://recover/…` link; `None` if it is not a well-formed,
    /// actionable recovery link (a missing/damaged handover is rejected).
    pub fn parse(link: &str) -> Option<RecoveryInvite> {
        let rest = link.trim().strip_prefix("molt://recover/")?;
        let mut parts = rest.split('/');
        let republic = parts.next()?.replace('-', " ");
        let member = parts.next()?.to_string();
        let ticket = parts.next()?.to_string();
        let handover_hex = parts.next()?;
        if parts.next().is_some() {
            return None; // trailing junk
        }
        if republic.trim().is_empty() || member.is_empty() || ticket.len() < 4 {
            return None;
        }
        let text = String::from_utf8(hex::decode(handover_hex).ok()?).ok()?;
        let mut fields = text.split('\n');
        let server = fields.next()?.to_string();
        let queue_id = fields.next()?.to_string();
        let wrap = fields.next()?.to_string();
        if fields.next().is_some() || server.is_empty() || queue_id.is_empty() || wrap.is_empty() {
            return None;
        }
        Some(RecoveryInvite {
            republic,
            member,
            ticket,
            server,
            queue_id,
            wrap,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RecoveryInvite {
        RecoveryInvite {
            republic: "Chess Club".to_string(),
            member: "walter".to_string(),
            ticket: "k9x2m4q7aa".to_string(),
            server: "smp://fingerprint@host".to_string(),
            queue_id: "deadbeef".to_string(),
            wrap: "00112233".to_string(),
        }
    }

    #[test]
    fn a_recovery_link_round_trips() {
        let inv = sample();
        let link = inv.render();
        assert!(link.starts_with("molt://recover/"), "the scheme names recovery");
        assert!(
            link.contains("Chess-Club"),
            "spaces in the republic travel as dashes"
        );
        assert_eq!(RecoveryInvite::parse(&link).as_ref(), Some(&inv));
    }

    #[test]
    fn a_link_without_a_handover_is_not_actionable() {
        // preview only — no hex handover segment
        assert!(RecoveryInvite::parse("molt://recover/Chess-Club/walter/k9x2m4q7aa").is_none());
    }

    #[test]
    fn a_malformed_link_is_rejected() {
        assert!(RecoveryInvite::parse("molt://invite/Chess-Club/2of3/walter/tick").is_none());
        assert!(RecoveryInvite::parse("not a link").is_none());
    }
}
