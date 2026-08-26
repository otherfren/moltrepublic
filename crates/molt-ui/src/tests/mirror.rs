// SPDX-License-Identifier: GPL-3.0-or-later
//! The session mirror's row builders and the recovery notice reading.

use super::*;

/// The recovery flow rides the transient session notice (the engine's
/// contract: `recovery-link-pending:` / `recovery-link:` /
/// `recovery-link-failed:` / `recover-started:` / `recover-failed:` /
/// `recovered:`); the parser must split each prefix off verbatim and
/// treat everything else — including the existing notices — as none.
#[test]
fn recover_notices_parse_into_their_ui_effects() {
    assert_eq!(
        parse_recover_notice("recovery-link:molt://recover/abc"),
        RecoverNotice::Link("molt://recover/abc".to_string())
    );
    assert_eq!(
        parse_recover_notice("recover-started:ashi"),
        RecoverNotice::Started("ashi".to_string())
    );
    assert_eq!(
        parse_recover_notice("recover-failed:the survivors declined"),
        RecoverNotice::Failed("the survivors declined".to_string())
    );
    assert_eq!(
        parse_recover_notice("recovered:ashi"),
        RecoverNotice::Done("ashi".to_string())
    );
    // the coordinator's mint lifecycle: pending on the attempt, then the
    // outcome — a calm failed state (the flip side of Link) whose payload
    // is a reason the dialog maps onto localized text
    assert_eq!(
        parse_recover_notice("recovery-link-pending:ashi"),
        RecoverNotice::LinkPending("ashi".to_string())
    );
    assert_eq!(
        parse_recover_notice("recovery-link-failed:mesh-not-running"),
        RecoverNotice::LinkFailed("mesh-not-running".to_string())
    );
    // `recovery-link-failed:` must not be swallowed by the shorter
    // `recovery-link:` prefix — order in the parser matters
    assert_eq!(
        parse_recover_notice("recovery-link-failed:transport: queue gone"),
        RecoverNotice::LinkFailed("transport: queue gone".to_string())
    );
    // the non-recovery notices stay untouched by this path
    assert_eq!(parse_recover_notice("saved"), RecoverNotice::None);
    assert_eq!(parse_recover_notice("save-failed: disk"), RecoverNotice::None);
    assert_eq!(parse_recover_notice(""), RecoverNotice::None);
    // an error that itself contains a colon survives whole
    assert_eq!(
        parse_recover_notice("recover-failed:transport: queue gone"),
        RecoverNotice::Failed("transport: queue gone".to_string())
    );
}

fn ws(name: &str, minutes: i32) -> WorkspaceItem {
    WorkspaceItem {
        id: molt_core::demo_workspace_id(name).into(),
        name: name.into(),
        detail: "".into(),
        status: "".into(),
        synced: true,
        state: 0,
        last_sync_min: minutes,
        s3: false,
        backup: "".into(),
        encrypted: false,
        seed: "".into(),
        net: "".into(),
        members: ModelRc::new(VecModel::from(Vec::new())),
    }
}

/// A session with bucket-only entries, as a real listing would produce
/// them: one true orphan (id only, no name) and one foreign key. The
/// production DEFAULT has none — molt-core pins that.
fn sv_with_orphans() -> SessionView {
    SessionView {
        backup_orphans: vec![
            molt_core::BackupOrphan {
                id: "ab".repeat(32),
                name: String::new(),
                size_kib: 480,
                last_backup_min: 129_600,
            },
            molt_core::BackupOrphan {
                id: String::new(),
                name: "molt/leftover.bin".to_string(),
                size_kib: 75,
                last_backup_min: 43_200,
            },
        ],
        // the demo republics are a fixture, not the default (review K6)
        workspaces: molt_core::WorkspaceInfo::demo_set(),
        ..SessionView::default()
    }
}

#[test]
fn sort_bk_rows_by_size_and_names_with_empties_last() {
    let sv = sv_with_orphans();
    let mut rows = backup_rows(&sv);
    sort_bk_rows(&mut rows, "size", false);
    let sizes: Vec<i32> = rows.iter().map(|r| r.size_kib).collect();
    assert!(sizes.windows(2).all(|w| w[0] <= w[1]), "{sizes:?}");
    sort_bk_rows(&mut rows, "local", false);
    assert!(
        rows.last().expect("rows").local.is_empty(),
        "orphans sort last on the local column"
    );
    sort_bk_rows(&mut rows, "last", false);
    assert_eq!(
        rows.last().expect("rows").last.as_str(),
        "never",
        "never-backed-up rows sort last"
    );
}

#[test]
fn backup_rows_map_locals_then_orphans() {
    let sv = sv_with_orphans();
    let rows = backup_rows(&sv);
    assert_eq!(rows.len(), sv.workspaces.len() + sv.backup_orphans.len());
    // locals first: name on the left, bucket side only when auto is on
    for (row, w) in rows.iter().zip(&sv.workspaces) {
        assert!(row.has_local);
        assert_eq!(row.local.as_str(), w.name);
        assert_eq!(row.auto, w.s3);
        // the bucket cell claims nothing the bucket didn't confirm: a
        // real backup error, else really listed copies, else empty —
        // never derived from the auto toggle alone (story 12 honesty)
        if w.backup_error.is_empty() && w.backup_copies == 0 {
            assert!(row.remote.is_empty());
        } else {
            assert!(!row.remote.is_empty());
        }
    }
    // orphans last: bucket side only, no toggle. A true orphan shows
    // its shortened workspace-id pseudonym (no name exists in the
    // bucket — never invent one); a foreign key shows its raw key.
    let orphans = &rows[sv.workspaces.len()..];
    for row in orphans {
        assert!(!row.has_local);
        assert_eq!(row.local.as_str(), "");
        assert!(!row.auto);
    }
    assert_eq!(orphans[0].remote.as_str(), "abababababab…");
    // the row keeps the FULL pseudonym (restore starts from it)
    assert_eq!(orphans[0].id.as_str(), "ab".repeat(32));
    assert_eq!(orphans[1].remote.as_str(), "molt/leftover.bin");
    assert_eq!(orphans[1].id.as_str(), "", "a foreign key has no workspace id");
}

/// The production default renders a table with ONLY the local rows —
/// no invented bucket entries (story 8's regression fence, UI side).
#[test]
fn backup_rows_default_has_no_bucket_only_rows() {
    let sv = SessionView::default();
    let rows = backup_rows(&sv);
    assert_eq!(rows.len(), sv.workspaces.len());
    assert!(rows.iter().all(|r| r.has_local));
}

#[test]
fn sort_ws_items_by_name_and_recency() {
    let mut items = vec![ws("beta", 60), ws("Alpha", 5), ws("gamma", 0)];
    sort_ws_items(&mut items, "name", false);
    let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
    assert_eq!(names, ["Alpha", "beta", "gamma"], "case-insensitive");
    sort_ws_items(&mut items, "sync", false);
    let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
    assert_eq!(names, ["gamma", "Alpha", "beta"], "most recent first");
    sort_ws_items(&mut items, "sync", true);
    let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
    assert_eq!(names, ["beta", "Alpha", "gamma"]);
}

/// The relay panel renders the ENGINE's verdict, never its own: every
/// `blocked` reason becomes exactly one row state, and the position /
/// end-of-list flags follow the pool order (which IS the priority).
#[test]
fn relay_rows_mirror_the_engine_verdict_and_the_priority_order() {
    let status = |url: &str, kind, confirmed, blocked| RelayStatus {
        url: url.to_string(),
        kind,
        confirmed,
        blocked,
    };
    let rows = relay_rows(&[
        // in use: a confirmed onion relay dials by itself
        status("wss://aaa.onion", RelayKind::Onion, true, None),
        // in the pool, but the user has not confirmed it
        status(
            "wss://relay.example.org",
            RelayKind::Clearnet,
            false,
            Some(RelayBlock::Unconfirmed),
        ),
        // confirmed local (LAN self-host), but this session has not
        // activated it — same gate as clearnet, own badge (kind 2)
        status(
            "ws://192.168.1.5:7777",
            RelayKind::Local,
            true,
            Some(RelayBlock::ClearnetSessionLocked),
        ),
    ]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].pos, 1);
    assert_eq!(rows[0].kind, 0, "onion badge");
    assert!(rows[0].confirmed);
    assert_eq!(rows[0].blocked, 0, "no block = in use right now");
    assert!(rows[0].first, "position 0 cannot move up");
    assert!(!rows[0].last);
    assert_eq!(rows[1].pos, 2);
    assert_eq!(rows[1].kind, 1, "clearnet badge");
    assert_eq!(rows[1].blocked, 1, "unconfirmed");
    assert!(!rows[1].first && !rows[1].last, "the middle row moves both ways");
    assert_eq!(rows[2].pos, 3);
    assert_eq!(rows[2].kind, 2, "local badge - never presented as clearnet");
    assert_eq!(rows[2].blocked, 2, "outside Tor, not activated this session");
    assert!(rows[2].confirmed, "…yet confirmed: the two are independent");
    assert!(rows[2].last, "the bottom row cannot move down");
    // a single relay is BOTH ends — neither arrow may promise a move
    let one = relay_rows(&[status("wss://aaa.onion", RelayKind::Onion, false, Some(RelayBlock::Unconfirmed))]);
    assert!(one[0].first && one[0].last);
    assert!(relay_rows(&[]).is_empty(), "a fresh install shows no rows");
}
