// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! `list_proposals` answers a FILTER and a PAGE, not the history since the
//! founding: open cards by default, the decided ones on request, both
//! paged by id; `read_proposal` is the one full card.

use molt_core::{Command, GroupConfig, ProposalFilter, ProposalId, Reply, SessionView, Surface};
use molt_engine::WalletHandle;
use serde_json::json;

fn solo() -> GroupConfig {
    GroupConfig {
        member: "me".to_string(),
        members: vec!["me".to_string()],
        threshold: 1,
        self_cosign: false,
    }
}

async fn propose_name(w: &WalletHandle, value: &str) -> ProposalId {
    match w
        .execute(Command::Propose {
            surface: Surface::Organization,
            payload: json!({ "op": "set_name", "value": value }),
        })
        .await
        .expect("propose")
    {
        Reply::Proposed { id, .. } => id,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn list(
    w: &WalletHandle,
    filter: ProposalFilter,
    limit: u32,
    cursor: Option<u64>,
    with_texts: bool,
) -> (Vec<molt_core::ProposalView>, u64, Option<u64>) {
    match w
        .execute(Command::ListProposals {
            filter,
            limit,
            cursor,
            with_texts,
        })
        .await
        .expect("list")
    {
        Reply::Proposals {
            proposals,
            total,
            next_cursor,
        } => (proposals, total, next_cursor),
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn the_list_filters_and_pages_and_read_proposal_is_the_full_card() {
    let w = molt_engine::spawn(solo(), SessionView::default());
    let a = propose_name(&w, "A").await;
    let b = propose_name(&w, "B").await;
    let c = propose_name(&w, "C").await;
    for id in [a, b] {
        w.execute(Command::Approve {
            proposal: id,
            note: None,
        })
        .await
        .expect("approve seals at threshold 1");
    }

    let (open, total, next) = list(&w, ProposalFilter::Open, 0, None, false).await;
    assert_eq!(
        open.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![c],
        "open = the one card still on the table"
    );
    assert_eq!((total, next), (1, None));

    let (decided, total, next) = list(&w, ProposalFilter::Decided, 0, None, false).await;
    assert_eq!(decided.iter().map(|p| p.id).collect::<Vec<_>>(), vec![a, b]);
    assert_eq!((total, next), (2, None));
    assert!(
        decided.iter().all(|p| p.current.is_empty() && p.proposed.is_empty()),
        "the list carries no texts unless asked"
    );

    let (all, total, _) = list(&w, ProposalFilter::All, 0, None, false).await;
    assert_eq!(all.len(), 3);
    assert_eq!(total, 3);

    // one card per page, the cursor walks the ids
    let (page, total, next) = list(&w, ProposalFilter::All, 1, None, false).await;
    assert_eq!((page.len(), total, next), (1, 3, Some(a.0)));
    let (page, _, next) = list(&w, ProposalFilter::All, 1, next, false).await;
    assert_eq!((page[0].id, next), (b, Some(b.0)));
    let (page, _, next) = list(&w, ProposalFilter::All, 1, next, false).await;
    assert_eq!((page[0].id, next), (c, None), "the last page has no cursor");

    let (page, _, _) = list(&w, ProposalFilter::All, 0, None, true).await;
    assert_eq!(page[2].proposed, "C", "with_texts builds the texts");

    match w
        .execute(Command::ReadProposal { id: c.0 })
        .await
        .expect("read")
    {
        Reply::Proposals { proposals, total, .. } => {
            assert_eq!((proposals.len(), total), (1, 1));
            assert_eq!(proposals[0].proposed, "C", "the full card carries its texts");
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(
        w.execute(Command::ReadProposal { id: 999 }).await.is_err(),
        "an unknown id is an error, not an empty answer"
    );
}
