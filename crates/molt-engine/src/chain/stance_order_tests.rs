// SPDX-License-Identifier: GPL-3.0-or-later

//! A member's stance re-delivered after a reset or rewind never overrides
//! a newer stance of the same member.

use super::test_support::{chain_peer, Builder};
use molt_core::{ProposalId, ProposalState, Surface, WorkspaceEvent};
use serde_json::json;

const T: u64 = 1_751_000_000;

/// Walter in a 2-of-3 (veto room 1) holding petra's open card 7.
fn seat() -> crate::State {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let mut walter = chain_peer("walter", &b, b.blocks.clone());
    walter.identity_sk = Some(b.key("walter").clone());
    walter.receive_proposed(7, Surface::Memory, json!({ "op": "add_note", "id": 7 }), "petra");
    walter
}

fn from(st: &mut crate::State, who: &str, seq: u64, ts: u64, body: WorkspaceEvent) {
    let env = molt_core::EventEnvelope { prev_seq: 0, seq, ts, by: who.to_string(), body };
    st.cmd_net_delivered(who.to_string(), env, None).expect("a wire delivery acks");
}

fn decline(by: &str) -> WorkspaceEvent {
    WorkspaceEvent::Declined { id: ProposalId(7), by: by.to_string(), hash: String::new() }
}

fn approve(by: &str) -> WorkspaceEvent {
    WorkspaceEvent::Approved { id: ProposalId(7), by: by.to_string(), height: 1, sig: String::new() }
}

fn card(st: &crate::State) -> &molt_core::ProposalRecord {
    st.proposals.get(&7).expect("card 7")
}

/// Dora declined, then approved; petra declines. Dora's old decline,
/// delivered again, must not join petra's into a rejection.
#[test]
fn a_re_delivered_old_decline_does_not_reject_an_open_card() {
    let mut walter = seat();
    from(&mut walter, "dora", 10, T + 10, decline("dora"));
    from(&mut walter, "dora", 11, T + 11, approve("dora"));
    from(&mut walter, "petra", 20, T + 20, decline("petra"));
    assert_eq!(card(&walter).state, ProposalState::Proposed);
    walter.reset_peer_accept_window(&"dora".to_string());
    from(&mut walter, "dora", 10, T + 10, decline("dora"));
    assert_eq!(card(&walter).state, ProposalState::Proposed, "a stale decline rejected the card");
    assert_eq!(card(&walter).decliners, vec!["petra".to_string()]);
}

/// A re-delivered old approval does not clear a newer decline.
#[test]
fn a_re_delivered_old_approval_keeps_a_newer_decline() {
    let mut walter = seat();
    from(&mut walter, "dora", 11, T + 11, approve("dora"));
    from(&mut walter, "dora", 12, T + 12, decline("dora"));
    assert_eq!(card(&walter).decliners, vec!["dora".to_string()]);
    walter.reset_peer_accept_window(&"dora".to_string());
    from(&mut walter, "dora", 11, T + 11, approve("dora"));
    assert_eq!(card(&walter).decliners, vec!["dora".to_string()], "a stale approval cleared the decline");
}

/// The newest stance still wins when it is the one arriving.
#[test]
fn a_newer_stance_still_overrides() {
    let mut walter = seat();
    from(&mut walter, "dora", 12, T + 12, decline("dora"));
    from(&mut walter, "dora", 13, T + 13, approve("dora"));
    assert!(card(&walter).decliners.is_empty());
}

/// A re-delivered withdrawal leaves the withdrawn card as it is.
#[test]
fn a_re_delivered_withdrawal_changes_nothing() {
    let mut walter = seat();
    let withdraw = || WorkspaceEvent::Withdrawn { id: ProposalId(7), by: "petra".to_string() };
    from(&mut walter, "petra", 30, T + 30, withdraw());
    let after = card(&walter).clone();
    walter.reset_peer_accept_window(&"petra".to_string());
    from(&mut walter, "petra", 30, T + 30, withdraw());
    assert_eq!(*card(&walter), after);
}

/// The card records the newest stance per member as (stamp, wire seq).
#[test]
fn the_newest_stance_is_recorded_on_the_card() {
    let mut walter = seat();
    from(&mut walter, "dora", 10, T + 10, decline("dora"));
    from(&mut walter, "dora", 11, T + 11, approve("dora"));
    assert_eq!(card(&walter).stances.get("dora"), Some(&(T + 11, 11)));
}

/// (F5) Two stances of dora in one second - decline (seq 10), approve
/// (seq 11) - and petra's decline, met in three delivery orders with dora's
/// window re-filled after a reset: one card state everywhere, and the
/// replay of the kept events rebuilds it.
#[test]
fn stances_of_one_second_converge_in_every_order() {
    let d10 = || ("dora", 10, decline("dora"));
    let d11 = || ("dora", 11, approve("dora"));
    let p20 = || ("petra", 20, decline("petra"));
    let orders: [Vec<(&str, u64, WorkspaceEvent)>; 3] = [
        vec![d10(), d11(), p20(), ("reset", 0, decline("dora")), d10()],
        vec![d11(), d10(), p20(), ("reset", 0, decline("dora")), d11(), d10()],
        vec![d10(), d11(), ("reset", 0, decline("dora")), d10(), d11(), p20()],
    ];
    let mut cards = Vec::new();
    for order in orders {
        let mut walter = seat();
        for (who, seq, body) in order {
            if who == "reset" {
                walter.reset_peer_accept_window(&"dora".to_string());
                continue;
            }
            let ts = if who == "dora" { T + 11 } else { T + 20 };
            from(&mut walter, who, seq, ts, body);
        }
        let c = card(&walter);
        assert_eq!((c.state, c.decliners.clone()), (ProposalState::Proposed, vec!["petra".to_string()]));
        cards.push((c.state, c.decliners.clone(), c.stances.clone()));
    }
    assert!(cards.windows(2).all(|w| w[0] == w[1]), "{cards:?}");
    // the replay of the kept events (stamp, and the wire seq in prev_seq)
    let mut replayed = seat();
    for (who, seq, ts, body) in [("dora", 10, T + 11, decline("dora")), ("dora", 11, T + 11, approve("dora")), ("petra", 20, T + 20, decline("petra"))] {
        let env = replayed.make_env_at(who.to_string(), ts, body);
        let env = molt_core::EventEnvelope { prev_seq: seq, ..env };
        replayed.apply(&env);
    }
    let c = card(&replayed);
    assert_eq!((c.state, c.decliners.clone(), c.stances.clone()), cards[0].clone());
}
