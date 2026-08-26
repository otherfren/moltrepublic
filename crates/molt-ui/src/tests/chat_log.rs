// SPDX-License-Identifier: GPL-3.0-or-later
//! The chat log's projection: lines, quotes, receipts, system lines.

use super::*;

fn line(lead: &str, text: &str) -> LogLineData {
    LogLineData {
        id: String::new(),
        lead: lead.to_string(),
        text: text.to_string(),
        when: String::new(),
        quote: -1,
        quote_id: String::new(),
        system: false,
        quote_label: String::new(),
        quote_indent: 0,
        deleted_by: String::new(),
        first: true,
        own: false,
        alt: false,
        mine_emoji: String::new(),
        reactions: Vec::new(),
        receipts: Vec::new(),
        has_file: false,
        file_name: String::new(),
        file_meta: String::new(),
        file_available: false,
        proposal_id: None,
    }
}

/// A deterministic 32-char hex id for tests.
fn hex_id(b: u8) -> String {
    MessageId([b; 16]).to_string()
}

fn qsrc(lead: &str, text: &str, deleted: bool) -> QuoteSrc {
    QuoteSrc {
        lead: lead.to_string(),
        text: text.to_string(),
        deleted,
    }
}

/// An engine-authored System-kind message maps onto the same per-line
/// `system` flag the governance rows use — one quiet rendering path,
/// never a second style; a User message stays a normal card.
#[test]
fn a_system_kind_message_maps_onto_the_quiet_line_flag() {
    let user = ChatMessage::text(MessageId([1; 16]), "petra", "gm", 100);
    assert!(!chat_line(0, &user, "me", &[]).system);
    let notice = ChatMessage::text(MessageId([2; 16]), "petra", "🔑 back", 101)
        .with_kind(molt_core::ChatKind::System);
    assert!(chat_line(0, &notice, "me", &[]).system);
}

/// Read receipts show ONLY on the local member's own messages (the sender
/// wants delivery confirmation) — one dot per OTHER member, green once in
/// read_by; an incoming message carries no receipt row at all.
#[test]
fn read_receipts_render_only_on_own_messages() {
    let roster = vec!["me".to_string(), "ada".to_string(), "bo".to_string()];

    // my own message: a dot per OTHER member, ada green (read), bo yellow
    let mut mine = ChatMessage::text(MessageId([3; 16]), "me", "hi", 100);
    mine.read_by.insert("ada".to_string());
    let r = chat_line(0, &mine, "me", &roster).receipts;
    assert_eq!(r.len(), 2, "one dot per other member");
    assert_eq!(r.iter().find(|x| x.name == "ada").map(|x| x.read), Some(true));
    assert_eq!(r.iter().find(|x| x.name == "bo").map(|x| x.read), Some(false));
    assert!(r.iter().all(|x| x.name != "me"), "the author gets no self-dot");

    // an incoming message (not mine): NO receipt row
    let mut theirs = ChatMessage::text(MessageId([4; 16]), "ada", "yo", 101);
    theirs.read_by.insert("me".to_string());
    assert!(
        chat_line(0, &theirs, "me", &roster).receipts.is_empty(),
        "a received message shows no receipts"
    );
}

/// Rewrite of the pre-chat-bus author-block/teaser tests, meaning
/// preserved: header once per block, zebra flips on author change,
/// quotes tease "author: body", dangling quotes are dropped — but the
/// quotes are now id-addressed, resolve their teaser through the
/// full-log map (so a cross-channel quote teases without a jump row)
/// and deleted targets tease with an ellipsis.
#[test]
fn annotate_chat_log_resolves_quotes_by_id() {
    let mut log = vec![
        line("me", "first"),
        line("me", "second"),
        line("ashi", "answer"),
        line("me", "back"),
    ];
    for (i, l) in log.iter_mut().enumerate() {
        l.id = hex_id(u8::try_from(i).expect("tiny") + 1);
    }
    log[2].quote_id = hex_id(1); // in view → teaser + jump row
    log[3].quote_id = hex_id(99); // dangling id → dropped
    let quotes = HashMap::from([(hex_id(1), qsrc("me", "first", false))]);
    annotate_chat_log(&mut log, &quotes);
    // the header shows once per author block …
    assert_eq!(
        log.iter().map(|l| l.first).collect::<Vec<_>>(),
        [true, false, true, true]
    );
    // … and the zebra flips exactly on author changes
    assert_eq!(
        log.iter().map(|l| l.alt).collect::<Vec<_>>(),
        [false, false, true, false]
    );
    assert_eq!(log[2].quote_label, "me: first");
    assert_eq!(log[2].quote, 0, "the jump target is the quoted row");
    assert_eq!(log[3].quote, -1, "dangling quotes are dropped");
    assert_eq!(log[3].quote_label, "");

    // a deleted target teases with an ellipsis; a target OUTSIDE the
    // displayed log (cross-channel quote — the sanctioned cross-post)
    // teases from the full-log map but offers no jump row
    let mut log = vec![line("ashi", "reply")];
    log[0].id = hex_id(2);
    log[0].quote_id = hex_id(1);
    let quotes = HashMap::from([(hex_id(1), qsrc("me", "", true))]);
    annotate_chat_log(&mut log, &quotes);
    assert_eq!(log[0].quote_label, "me: …");
    assert_eq!(log[0].quote, -1, "not in view: teaser without a jump");

    // legacy numeric quotes (pre-chat-bus rows) still resolve by row
    let mut log = vec![line("me", "first"), line("ashi", "answer"), line("me", "back")];
    log[1].quote = 0;
    log[2].quote = 99; // out of range
    annotate_chat_log(&mut log, &HashMap::new());
    assert_eq!(log[1].quote_label, "me: first");
    assert_eq!(log[2].quote, -1, "out-of-range legacy quotes are dropped");
}

#[test]
fn system_lines_interleave_by_time_and_tolerate_unknown_proposals() {
    let pv = ProposalView {
        id: ProposalId(4),
        surface: Surface::Memory,
        payload: serde_json::json!({ "op": "add_note", "title": "budget" }),
        approvals: 2,
        threshold: 3,
        state: ProposalState::Proposed,
        approved_by_me: false,
        declined_by_me: false,
        current: String::new(),
        proposed: String::new(),
        votes: Vec::new(),
        declined_at: 0,
        declined_by: String::new(),
        by: String::new(),
        mine: false,
        superseded: false,
        withdrawn: false,
    };
    let first_seen = HashMap::from([(4u64, 150u64)]);
    let sys = patch_system_lines(0, 4, &[pv], &HashMap::new(), &first_seen);
    assert_eq!(sys.len(), 1);
    assert_eq!(sys[0].0, 150, "stamped with the UI-side first-seen time");
    assert!(sys[0].1.system, "system lines carry the quiet-style flag");
    assert!(sys[0].1.lead.is_empty(), "system lines have no author");
    assert!(sys[0].1.id.is_empty(), "no id → no id-requiring actions");
    let text = &sys[0].1.text;
    assert!(
        text.contains("#4") && text.contains("budget") && text.contains("2/3"),
        "{text}"
    );

    // an unknown/already-materialized proposal renders as a bare
    // handle, never an error (concept Q4)
    let sys_unknown = patch_system_lines(0, 9, &[], &HashMap::new(), &first_seen);
    assert!(sys_unknown[0].1.text.contains("#9"), "{}", sys_unknown[0].1.text);
    assert_eq!(sys_unknown[0].0, 0, "never seen → sorts to the top");

    // merged by time into the chat lines; the chat order itself is
    // never disturbed and a tie puts the system line first
    let chat = vec![
        (100u64, line("me", "a")),
        (200, line("me", "b")),
        (300, line("me", "c")),
    ];
    let system = vec![
        (200u64, system_line_data("s2".into())),
        (150, system_line_data("s1".into())),
    ];
    let merged = merge_by_time(chat, system);
    assert_eq!(
        merged.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
        ["a", "s1", "s2", "b", "c"]
    );
}

#[test]
fn quote_indent_groups_by_target_and_alternates_between_neighbors() {
    let mut log = vec![
        line("a", "question 1"),
        line("b", "reply 1"),
        line("c", "reply 2"),
        line("d", "reply to something else"),
        line("e", "plain"),
        line("f", "late reply"),
    ];
    log[1].quote_id = hex_id(1);
    log[2].quote_id = hex_id(1);
    log[3].quote_id = hex_id(2);
    log[5].quote_id = hex_id(3);
    let quotes = HashMap::from([
        (hex_id(1), qsrc("a", "question 1", false)),
        (hex_id(2), qsrc("x", "question 2", false)),
        (hex_id(3), qsrc("y", "question 3", false)),
    ]);
    annotate_chat_log(&mut log, &quotes);
    assert_eq!(log[0].quote_indent, 0, "no quote, no indent");
    assert_eq!(log[1].quote_indent, 1, "a fresh reply group starts at depth 1");
    assert_eq!(log[2].quote_indent, 1, "same target keeps the depth");
    assert_eq!(log[3].quote_indent, 2, "a neighboring different target alternates");
    assert_eq!(log[4].quote_indent, 0, "plain rows sit flush and end the run");
    assert_eq!(log[5].quote_indent, 1, "after a break the next group restarts at 1");
}
