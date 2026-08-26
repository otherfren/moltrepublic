// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendered prose: ages, dates, sizes, columns, icons.

use super::*;

#[test]
fn charter_splits_into_balanced_columns_at_word_boundaries() {
    // a short charter stays single-column
    assert_eq!(
        charter_columns("kurz und knapp", 3),
        vec!["kurz und knapp".to_string()]
    );
    // empty → no columns (the UI shows its no-agenda line)
    assert!(charter_columns("   ", 3).is_empty());
    // ~450 chars → 2 columns; nothing lost, split at word boundaries
    let mid = "wort ".repeat(90);
    let cols = charter_columns(&mid, 3);
    assert_eq!(cols.len(), 2);
    assert!(
        cols.join(" ")
            .split_whitespace()
            .eq(mid.split_whitespace()),
        "columns are a display split - every word survives"
    );
    // a long charter caps at the column maximum
    let long = "wort ".repeat(300);
    assert_eq!(charter_columns(&long, 3).len(), 3);
    // umlauts near the cut never split a character
    let umlaut = "ä".repeat(400);
    let cols = charter_columns(&umlaut, 3);
    assert_eq!(cols.concat(), umlaut);
}

#[test]
fn expires_labels_render_the_retention_deadline() {
    assert_eq!(expires_label(0, 100, 100 + 13 * 86_400, true), "in 13 days");
    assert_eq!(expires_label(0, 100, 100 + 86_400, true), "in 1 day");
    assert_eq!(expires_label(0, 100, 100 + 7_200, true), "in 2 h");
    assert_eq!(expires_label(0, 100, 100 + 120, true), "in 2 min");
    assert_eq!(expires_label(0, 500, 100, true), "expired");
    assert_eq!(
        expires_label(0, 100, 0, true),
        "-",
        "0 = unknown share age, no deadline (the engine keeps it forever)"
    );
    assert_eq!(
        expires_label(0, 100, 100 + 86_400, false),
        "-",
        "an unavailable share has nothing left to expire"
    );
    // the cell renders in the active language, like the tables around it
    assert_eq!(expires_label(1, 100, 100 + 13 * 86_400, true), "in 13 Tagen");
    assert_eq!(expires_label(1, 100, 100 + 86_400, true), "in 1 Tag");
    assert_eq!(expires_label(1, 500, 100, true), "abgelaufen");
}

#[test]
fn when_label_relative_part() {
    let ts = 1_750_000_000_u64;
    let at = |offset: i64| when_label_at(0, ts, 1_750_000_000 + offset);
    assert!(at(5).ends_with("(just now)"), "{}", at(5));
    assert!(at(60).ends_with("(~1 minute ago)"), "{}", at(60));
    assert!(at(20 * 60).ends_with("(~20 minutes ago)"), "{}", at(1200));
    assert!(at(2 * 3600).ends_with("(~2 hours ago)"), "{}", at(7200));
    assert!(at(3 * 86_400).ends_with("(~3 days ago)"), "{}", at(259_200));
}

/// The presence cell reads a REAL stamp: fresh sightings stay relative,
/// and past a week the DATE takes over - "34 d ago" is arithmetic the
/// reader should not have to do. Only a seat this install has never had
/// any evidence for says so.
#[test]
fn the_last_seen_cell_goes_from_relative_to_a_plain_date() {
    let now = 1_787_000_000_u64;
    assert_eq!(seen_label(0, now, molt_core::MemberInfo::NEVER, "never seen"), "never seen");
    assert_eq!(seen_label(0, now, now, ""), "just now");
    assert_eq!(seen_label(0, now, now - 3 * 3600, ""), "3 h ago");
    assert_eq!(seen_label(1, now, now - 2 * 86_400, ""), "vor 2 Tagen");
    // the week boundary: one side relative, the other the date itself
    assert_eq!(seen_label(0, now, now - 6 * 86_400, ""), "6 d ago");
    let old = now - 30 * 86_400;
    assert_eq!(seen_label(0, now, old, ""), date_label(0, old));
    assert_eq!(seen_label(1, now, old, ""), date_label(1, old));
    // the two spellings, pinned against the same instant
    let iso = date_label(0, old);
    let de = date_label(1, old);
    assert_eq!(iso.len(), 10, "ISO date: {iso}");
    assert_eq!(de.len(), 10, "German date: {de}");
    assert_eq!(
        de,
        format!("{}.{}.{}", &iso[8..10], &iso[5..7], &iso[0..4]),
        "the German date is the same day, written the German way"
    );
}

#[test]
fn sync_status_label_matches_the_demo_prose() {
    assert_eq!(sync_status_label(0, 0, 0, 0), "Synced · just now");
    assert_eq!(sync_status_label(0, 0, 2, 0), "Synced · 2 min ago");
    assert_eq!(sync_status_label(0, 0, 60, 0), "Synced · 1 h ago");
    assert_eq!(sync_status_label(0, 1, 0, 80), "Syncing… 80 items left");
    assert_eq!(sync_status_label(0, 2, 4320, 0), "Offline · last sync 3 d ago");
}

#[test]
fn size_and_backup_labels() {
    assert_eq!(size_label(920), "920 KiB");
    assert_eq!(size_label(1840), "1.8 MiB");
    assert_eq!(backup_when_label(0, molt_core::WorkspaceInfo::NEVER), "never");
    assert_eq!(backup_when_label(0, 0), "just now");
    assert_eq!(backup_when_label(0, 30), "30 min ago");
    assert_eq!(backup_when_label(0, 129_600), "90 d ago");
    assert_eq!(backup_when_label(1, molt_core::WorkspaceInfo::NEVER), "nie");
    assert_eq!(backup_when_label(1, 0), "gerade eben");
    assert_eq!(backup_when_label(1, 30), "vor 30 Min.");
    assert_eq!(backup_when_label(1, 129_600), "vor 90 Tagen");
}

/// Guard: every nav sub-view of every surface has a real icon — the
/// "▪️" fallback showing up in the sidebar means someone added a view
/// without extending `view_icon`.
#[test]
fn every_view_has_an_icon() {
    for surface in Surface::ALL {
        for (key, _) in surface.views() {
            assert_ne!(view_icon(key), "▪️", "view `{key}` has no icon");
        }
    }
}
