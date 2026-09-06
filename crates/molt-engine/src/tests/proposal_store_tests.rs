// SPDX-License-Identifier: GPL-3.0-or-later

//! Restart durability of the proposal store (round-3 fix plan, part B):
//! who proposed, who voted, and whether a dead patch stays dead across a
//! reopen.

use super::support::*;
use crate::chain::test_support::{wire, Builder};
use crate::*;
use molt_core::{ProposalId, ProposalState, Surface, VoteState, WorkspaceEvent};
use serde_json::json;

/// A peer's real signature for a surface proposal at `height`, as the
/// wire carries it (the change a card resolves to is the gated
/// `Applied` transition).
fn peer_sig(
    b: &Builder,
    member: &str,
    id: u64,
    surface: Surface,
    payload: serde_json::Value,
    height: u64,
) -> String {
    let change = molt_core::ChainChange::Applied {
        proposal_id: id,
        surface,
        payload,
    };
    molt_storage::identity_sign(
        b.key(member),
        &molt_core::chain::approval_bytes(&b.republic_id, height, &change),
    )
}

fn card(st: &State, id: u64) -> molt_core::ProposalView {
    let p = st.proposals.get(&id).expect("the card survives the reopen");
    st.view(id, p)
}

fn vote_of(v: &molt_core::ProposalView, member: &str) -> VoteState {
    v.votes
        .iter()
        .find(|x| x.member == member)
        .map(|x| x.vote)
        .expect("a roster row per member")
}

/// R12/R17: a reopen must keep WHO proposed and WHAT was voted.
///
/// Both halves were lost for the same reason - the wire arm mutated RAM
/// and wrote nothing: a foreign card came back only through the WP2
/// re-serve (an envelope re-authored under THIS node, so every peer's
/// proposal read `by: <me>, mine: true`), and the collected signatures
/// live in the ephemeral `pending_sigs`, so every vote read `open` and
/// the proposer's own signature stopped counting toward m.
#[test]
fn a_reopen_keeps_the_proposer_and_every_vote() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let (mut walter, _tmp, dir) = stored_chain_signer(&b, "walter", &["petra", "walter", "dora"]);

    // a FOREIGN card, plus the proposer's own signature for it
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(7),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Petra's" }),
        },
    );
    let sig = peer_sig(
        &b,
        "petra",
        7,
        Surface::Organization,
        json!({ "op": "set_name", "value": "Petra's" }),
        1,
    );
    wire(
        &mut walter,
        "petra",
        2,
        WorkspaceEvent::Approved {
            id: ProposalId(7),
            by: "petra".to_string(),
            height: 1,
            sig,
        },
    );
    // …and an OWN card this seat co-signed on the spot
    let own = match walter
        .cmd_propose(Surface::Organization, json!({ "op": "set_name", "value": "Walter's" }))
        .expect("propose")
    {
        Reply::Proposed { id, .. } => id.0,
        other => panic!("unexpected: {other:?}"),
    };
    // the catch-up re-serve: it re-authors BOTH cards under this node
    walter.serve_open_governance();

    let live_foreign = card(&walter, 7);
    assert_eq!(live_foreign.approvals, 1, "the live card counts petra's signature");

    let back = reopen_chain_signer(walter, &dir, &b, "walter", false);

    let foreign = card(&back, 7);
    assert_eq!(foreign.by, "petra", "the foreign card keeps its proposer");
    assert!(!foreign.mine, "…and is not read as our own");
    assert_eq!(foreign.state, ProposalState::Proposed, "still open");
    assert_eq!(foreign.approvals, 1, "petra's collected signature survives");
    assert_eq!(vote_of(&foreign, "petra"), VoteState::Approved);
    assert_eq!(vote_of(&foreign, "walter"), VoteState::Open);

    let mine = card(&back, own);
    assert_eq!(mine.by, "walter", "the own card keeps its proposer");
    assert!(mine.mine);
    assert_eq!(
        mine.approvals, 1,
        "the proposer's own signature keeps counting toward m"
    );
    assert_eq!(vote_of(&mine, "walter"), VoteState::Approved);
    assert!(mine.approved_by_me, "…and reads as this seat's own stance");
}

/// The same durability for a DECLINE: a foreign voice against a card is
/// a vote like any other and must not evaporate with the RAM that held
/// it (Left's node read every vote `open` while the patch channels
/// carried "declined by Mr. Right").
#[test]
fn a_reopen_keeps_a_foreign_decline() {
    let b = Builder::new(&["petra", "walter", "dora"], 2);
    let (mut walter, _tmp, dir) = stored_chain_signer(&b, "walter", &["petra", "walter", "dora"]);

    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(7),
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": "Petra's" }),
        },
    );
    wire(
        &mut walter,
        "dora",
        1,
        WorkspaceEvent::Declined {
            id: ProposalId(7),
            by: "dora".to_string(),
            hash: String::new(),
        },
    );
    let back = reopen_chain_signer(walter, &dir, &b, "walter", false);
    let v = card(&back, 7);
    assert_eq!(vote_of(&v, "dora"), VoteState::Declined, "the voice survives");
    assert_eq!(v.state, ProposalState::Proposed, "one voice in veto room 1 does not tip");
}

/// B2 (R2): a pure `create` patch onto a path the base ALREADY carries
/// can never apply - the supersede walk must retire it, at reload and at
/// receive alike. Four of round 2's proposals stayed `proposed` for a
/// day because nothing re-checked a card whose paths no later patch
/// touched.
#[test]
fn a_create_onto_an_occupied_path_is_superseded() {
    let mut b = Builder::new(&["petra", "walter", "dora"], 2);
    b.commit_wiki(3, "a.md", "hello", &["petra", "walter"]);
    let (mut walter, _tmp, dir) = stored_chain_signer(&b, "walter", &["petra", "walter", "dora"]);

    let create = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+mine\n";
    wire(
        &mut walter,
        "petra",
        1,
        WorkspaceEvent::Proposed {
            id: ProposalId(7),
            surface: Surface::Memory,
            payload: json!({ "op": "wiki_patch", "value": create }),
        },
    );
    let live = card(&walter, 7);
    assert!(live.superseded, "at RECEIVE: the path is already in the base");
    assert_eq!(live.state, ProposalState::Rejected);

    let back = reopen_chain_signer(walter, &dir, &b, "walter", false);
    let after = card(&back, 7);
    assert!(after.superseded, "…and it stays dead across the reopen");
    assert_eq!(after.state, ProposalState::Rejected);
}
