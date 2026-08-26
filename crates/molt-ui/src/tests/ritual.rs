// SPDX-License-Identifier: GPL-3.0-or-later
//! The Restore wizard's one link field (welcome_rework.md).

use super::*;

// ---- the Restore wizard's one link field (welcome_rework.md) -------

/// The two link shapes are rendered by the ENGINE's own `render()`,
/// never hand-written here: a hand-built string pins the test's idea of
/// the format, and the day the real one changes the test keeps passing
/// while the panel stops recognizing anything.
/// A real x-only anchor - the handover encoders validate the key, so a
/// made-up hex string cannot stand in for one.
fn anchor(seed: u8) -> String {
    molt_net::nostr_identity(&[seed; 32], "fixture").1
}

fn invite_link() -> String {
    molt_engine::FoundingInvite {
        info: molt_core::InviteInfo {
            republic: "Chess Club".to_string(),
            threshold: 2,
            members: 3,
            inviter: "walter".to_string(),
            ticket: "a".repeat(64),
        },
        handover: molt_net::invite::InviteHandoverV2 {
            seat: 1,
            ticket: "a".repeat(64),
            npub: anchor(1),
            relays: vec!["ws://127.0.0.1:7777".to_string()],
        },
    }
    .render()
    .expect("the engine renders its own link")
}

fn recovery_link() -> String {
    molt_engine::RecoveryInvite {
        republic: "Chess Club".to_string(),
        member: "petra".to_string(),
        ticket: "c".repeat(64),
        server: String::new(),
        queue_id: String::new(),
        wrap: String::new(),
        republic_id: "d".repeat(64),
        handover: Some(molt_net::invite::RecoveryHandoverV2 {
            identity_pk: String::new(),
            ticket: "c".repeat(64),
            npub: anchor(2),
            relays: vec!["ws://127.0.0.1:7777".to_string()],
            republic_id: "d".repeat(64),
        }),
    }
    .render()
}

/// One field, two flows: an invite link asks for a NAME and joins, a
/// recovery link brings its own seat and needs the PHRASE. Getting this
/// wrong sends someone through the founding ritual to recover a seat
/// they already hold, so it is pinned rather than eyeballed.
#[test]
fn one_link_field_tells_a_join_from_a_recovery() {
    assert_eq!(
        link_kind(&invite_link()),
        LinkKind::Invite {
            republic: "Chess Club".to_string(),
            inviter: "walter".to_string(),
        },
        "a founding invite routes to the join"
    );
    assert_eq!(
        link_kind(&recovery_link()),
        LinkKind::Recovery {
            republic: "Chess Club".to_string(),
            member: "petra".to_string(),
        },
        "a recovery link routes to the ritual, and names its own seat"
    );
    // whitespace is what a paste actually carries
    assert_eq!(link_kind(&format!("  {}\n", invite_link())), link_kind(&invite_link()));
}

/// Everything else arms nothing. A PREVIEW-only invite link is the
/// interesting case: it parses as a human-readable invite and carries no
/// transport handover at all, so a panel that armed on "looks like an
/// invite" would start a join that cannot reach anybody.
#[test]
fn a_link_that_cannot_act_arms_nothing() {
    let full = invite_link();
    let preview = full.rsplit_once('/').expect("the handover is the last segment").0;
    assert_eq!(
        link_kind(preview),
        LinkKind::Unrecognized,
        "a preview link has no transport handover - nothing can be done with it"
    );
    let damaged = format!("{}zz", recovery_link());
    assert_eq!(
        link_kind(&damaged),
        LinkKind::Unrecognized,
        "a damaged recovery handover is not an actionable link"
    );
    for junk in ["", "   ", "hello", "molt://", "molt://invite/", "https://example.com"] {
        assert_eq!(link_kind(junk), LinkKind::Unrecognized, "junk: {junk:?}");
    }
}
