// SPDX-License-Identifier: GPL-3.0-or-later
//! The chat pane against a real engine, headless.

use super::*;

/// **THE reported sequence: a cold start, then OPEN a workspace that is
/// already on disk.**
///
/// "beim ersten öffnen eines workspaces wird ein leerer chat angezeigt,
/// ich muss auf organization klicken und wieder zurück" — the switch is
/// what the second push stands for, and the assertion is BEFORE it.
#[test]
fn a_cold_open_of_a_stored_workspace_fills_the_chat_pane() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().to_path_buf();
    let rt = rt();
    let _guard = rt.enter();

    // --- a workspace ON DISK, the way a previous run left one behind
    let (mut ws, now) = workspace_on_disk(&root, 1, &["walter"], "test the chat");
    // …with a message in it
    ws.append(&molt_core::EventEnvelope {
        prev_seq: 1,
        seq: 2,
        ts: now,
        by: "walter".to_string(),
        body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            molt_core::MessageId([7u8; 16]),
            "walter",
            "hello group",
            now,
        )),
    })
    .expect("append");
    ws.sync().expect("sync");
    drop(ws);

    // --- second run: a COLD app, the way the user starts it
    let (w, _) = node_with_chat(&root);
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    rt.block_on(async {
        // the app comes up on the Choice screen and mirrors once
        mirror(&w, &ui, &last, &chat_ui).await;
        assert_eq!(
            chat_rows(&ui),
            0,
            "nothing is open yet - if this is not empty the test proves nothing"
        );

        // …and then the user opens the workspace
        let stored = molt_storage::scan_workspaces(&root)
            .first()
            .map(|e| e.info().id)
            .expect("the workspace is on disk");
        let open_id = stored;
        w.execute(Command::OpenWorkspace { id: open_id })
            .await
            .expect("the stored workspace opens");
        // the engine's own answer first: if IT is empty, the fault is
        // not in this layer and the assertion below would blame the
        // wrong one
        let engine_rows = match w
            .execute(Command::ReadState {
                surface: Surface::Chat,
                channel: Some(molt_core::ChannelRef::Group),
                view: None,
            })
            .await
        {
            Ok(Reply::State(snap)) => snap.applied.len(),
            _ => 0,
        };
        assert_eq!(engine_rows, 1, "the engine holds the stored message");

        mirror(&w, &ui, &last, &chat_ui).await;
    });

    assert!(
        chat_rows(&ui) > 0,
        "opening a stored workspace must fill the chat pane - having to \
         visit another surface and come back IS the bug"
    );
}

/// **The reported bug: opening a workspace must fill the chat pane.**
///
/// "beim ersten öffnen eines workspaces wird ein leerer chat angezeigt,
/// ich muss auf organization klicken und wieder zurück" — so the test
/// asserts the pane after the OPEN, before any surface switch.
#[test]
fn opening_a_workspace_fills_the_chat_pane() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter(); // the engine spawns tasks at construction
    let (w, _) = node_with_chat(tmp.path());
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));

    rt.block_on(async {
        // found a session-only workspace and say something in it
        w.execute(Command::CreateStart {
            name: "DevTest".to_string(),
            member: "walter".to_string(),
            threshold: 1,
            members: 1,
            relays: Vec::new(),
        })
        .await
        .ok();
        w.execute(Command::Chat {
            body: "hello group".to_string(),
            quote: None,
            channel: ChannelRef::Group,
        })
        .await
        .ok();

        mirror(&w, &ui, &last, &chat_ui).await;
    });

    assert!(
        ui.get_surfaces().row_count() > 0,
        "the bundle must have landed at all (else this test proves nothing)"
    );
    assert!(
        chat_rows(&ui) > 0,
        "the chat pane must hold the message the engine has - it took a \
         surface switch to appear, which is the reported bug"
    );
}

/// **The reported bug: a member wrote into a fresh topic and the two
/// RECEIVING clients stopped reacting — "klick auf linke navbar Chat
/// zeigt keine Funktion".**
///
/// Receiver perspective, headless: the workspace holds a group message
/// and a FOREIGN member's message in a topic channel (arrived over the
/// wire, so it is unread here). The mirror must survive that state, the
/// nav must list the topic row, and the Chat click must keep working.
#[test]
fn a_foreign_topic_message_keeps_the_chat_usable() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().to_path_buf();
    let rt = rt();
    let _guard = rt.enter();

    let (mut ws, now) = workspace_on_disk(&root, 2, &["walter", "ingrid"], "test the chat");
    ws.append(&molt_core::EventEnvelope {
        prev_seq: 1,
        seq: 2,
        ts: now,
        by: "walter".to_string(),
        body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            molt_core::MessageId([7u8; 16]),
            "walter",
            "hello group",
            now,
        )),
    })
    .expect("append group message");
    // the foreign topic message, the way the wire landed it
    ws.append(&molt_core::EventEnvelope {
        prev_seq: 2,
        seq: 3,
        ts: now,
        by: "ingrid".to_string(),
        body: molt_core::WorkspaceEvent::Chat(
            molt_core::ChatMessage::text(
                molt_core::MessageId([9u8; 16]),
                "ingrid",
                "topic talk",
                now,
            )
            .with_channel(ChannelRef::Topic {
                name: "asdf".to_string(),
            }),
        ),
    })
    .expect("append topic message");
    ws.sync().expect("sync");
    drop(ws);

    let (w, _) = node_with_chat(&root);
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    rt.block_on(async {
        mirror(&w, &ui, &last, &chat_ui).await;

        let stored = molt_storage::scan_workspaces(&root)
            .first()
            .map(|e| e.info().id)
            .expect("the workspace is on disk");
        w.execute(Command::OpenWorkspace { id: stored })
            .await
            .expect("the stored workspace opens");
        // the mirror push that follows the delivery — the receivers
        // froze HERE if this layer chokes on the topic state
        mirror(&w, &ui, &last, &chat_ui).await;
        assert!(
            chat_rows(&ui) > 0,
            "the group log must still show after a topic message arrived"
        );
        assert!(
            ui.get_chat_channels().iter().any(|c| c.key == "topic:asdf"),
            "the nav must list the foreign topic's row"
        );

        // …and the user's Chat click still navigates
        w.execute(Command::SelectSurface {
            surface: Surface::Chat,
        })
        .await
        .expect("the chat click reaches the engine");
        mirror(&w, &ui, &last, &chat_ui).await;
    });

    assert!(
        chat_rows(&ui) > 0,
        "after clicking Chat the pane must keep its rows - a dead pane \
         IS the reported bug"
    );
}

/// **The set_relays vote card shows the CHANGES** (relay story,
/// 2026-08-09): a pending pool edit reaches the window as a relay-op
/// card carrying the diff rows — kept, added, removed — instead of the
/// generic Ist/Soll text pair.
#[test]
fn a_pool_edit_proposal_carries_the_diff_rows() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().to_path_buf();
    let rt = rt();
    let _guard = rt.enter();

    // one seat cannot drive an edit to applied at m=2 (the proposer
    // already counts as approver), so the effective pool stays empty
    // here and every proposed relay renders as ADDED — the
    // kept/removed semantics are pinned by `relay_pool_diff`'s unit
    // test against a non-empty Ist-Stand
    let (ws, _now) = workspace_on_disk(&root, 2, &["walter", "ingrid"], "test the pool");
    drop(ws);

    let (w, _) = node_with_chat(&root);
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    rt.block_on(async {
        let stored = molt_storage::scan_workspaces(&root)
            .first()
            .map(|e| e.info().id)
            .expect("the workspace is on disk");
        w.execute(Command::OpenWorkspace { id: stored })
            .await
            .expect("the stored workspace opens");
        // the pool edit stays pending at m=2 — the vote card under test
        w.execute(Command::Propose {
            surface: Surface::Organization,
            payload: serde_json::json!({
                "op": "set_relays",
                "value": "wss://kept.example wss://new.example",
            }),
        })
        .await
        .expect("the pool edit proposes");

        mirror(&w, &ui, &last, &chat_ui).await;
    });

    let org = ui
        .get_surfaces()
        .iter()
        .find(|s| s.key == "organization")
        .expect("org surface present");
    assert_eq!(org.pending.row_count(), 1, "the pool edit is pending");
    let card = org.pending.row_data(0).expect("card row");
    assert!(card.relay_op, "the card knows it is a pool edit");
    let rows: Vec<(i32, String)> = card
        .relay_changes
        .iter()
        .map(|c| (c.sign, c.url.to_string()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (RELAY_ROW_ADDED, "wss://kept.example".to_string()),
            (RELAY_ROW_ADDED, "wss://new.example".to_string()),
        ],
        "the card carries the pool diff (empty Ist-Stand: all added)"
    );
}

/// **The reported bug (2026-08-09): after an approval elsewhere applied
/// the vote, clicking Chat showed "ein kaputtes Panel mit leerem
/// 'Proposal:', das die Hälfte der Seite einnimmt".**
///
/// A decided vote's discussion stays a selectable read-only view, but
/// the decision header's lookup chained only pending + declined — an
/// APPLIED proposal is in neither list, so the card above the chat
/// rendered from `ProposalRow::default()`: the empty wreck. The header
/// must carry the decided card.
#[test]
fn a_decided_votes_discussion_keeps_its_decision_card() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().to_path_buf();
    let rt = rt();
    let _guard = rt.enter();
    let (ws, _now) = workspace_on_disk(&root, 1, &["walter"], "test the header");
    drop(ws);

    let (w, _) = node_with_chat(&root);
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
    rt.block_on(async {
        let stored = molt_storage::scan_workspaces(&root)
            .first()
            .map(|e| e.info().id)
            .expect("the workspace is on disk");
        w.execute(Command::OpenWorkspace { id: stored })
            .await
            .expect("the stored workspace opens");
        mirror(&w, &ui, &last, &chat_ui).await;
        // the vote APPLIES instantly at m=1 — the state right after
        // the approval sound on the reporting client
        w.execute(Command::Propose {
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_name", "value": "NewName" }),
        })
        .await
        .expect("the vote proposes and applies");
        // …and the user opens the decision's discussion
        chat_ui
            .lock()
            .expect("ui state")
            .select(ChannelRef::Patch {
                id: molt_core::ProposalId(1),
            });
        mirror(&w, &ui, &last, &chat_ui).await;
    });

    let card = ui.get_selected_decision();
    assert!(
        card.id == 1 && !card.text.is_empty(),
        "a decided vote's discussion must head with ITS card, never an \
         empty one (id={}, text={:?})",
        card.id,
        card.text
    );
}
