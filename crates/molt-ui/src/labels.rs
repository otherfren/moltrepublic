// SPDX-License-Identifier: GPL-3.0-or-later
//! Rendered prose and the small name/index maps: the shared data carries
//! machine values (stamps, counts, keys), and every human label is composed
//! here in the active language (0 = English, 1 = German).

use molt_core::{Screen, Surface};

use crate::AppScreen;

/// A human "x ago" label from minutes (0 = English, 1 = German — these
/// labels are composed per row in Rust, so like [`seat_state_label`] they
/// take the language instead of going through the Slint `Strings` global).
fn ago_label(lang: i32, minutes: u32) -> String {
    if lang == 1 {
        match minutes {
            0 => "gerade eben".to_string(),
            m if m < 60 => format!("vor {m} Min."),
            m if m < 1440 => format!("vor {} Std.", m / 60),
            m if m < 2880 => "vor 1 Tag".to_string(),
            m => format!("vor {} Tagen", m / 1440),
        }
    } else {
        match minutes {
            0 => "just now".to_string(),
            m if m < 60 => format!("{m} min ago"),
            m if m < 1440 => format!("{} h ago", m / 60),
            m => format!("{} d ago", m / 1440),
        }
    }
}

/// Human "last seen" label from a member's REAL unix stamp — the
/// `last_sync_min` pattern: the shared data carries the number, prose is
/// rendered here. `never` is what a stamp-less member shows ("" keeps an
/// Open-list chip bare; the live surfaces say so explicitly).
pub(crate) fn seen_label(lang: i32, now: u64, last_seen: u64, never: &str) -> String {
    if last_seen == molt_core::MemberInfo::NEVER {
        return never.to_string();
    }
    let age = now.saturating_sub(last_seen);
    // past a week "34 d ago" is arithmetic the reader has to do; the DATE
    // is the fact the engine remembers, so show that instead
    if age >= 7 * 86_400 {
        return date_label(lang, last_seen);
    }
    let min = u32::try_from(age / 60).unwrap_or(u32::MAX);
    ago_label(lang, min)
}

/// A unix stamp as a plain local date - German `22.08.2026`, else ISO
/// `2026-08-22`. No time of day: the stamp is minute-coarse presence, and
/// a date is what the reader is actually asking for that far back.
pub(crate) fn date_label(lang: i32, ts: u64) -> String {
    let Some(dt) = chrono::DateTime::from_timestamp(i64::try_from(ts).unwrap_or(0), 0) else {
        return String::new();
    };
    let local = dt.with_timezone(&chrono::Local);
    if lang == 1 {
        local.format("%d.%m.%Y").to_string()
    } else {
        local.format("%Y-%m-%d").to_string()
    }
}

/// The honest "never seen" cell text.
pub(crate) fn never_seen_label(lang: i32) -> &'static str {
    if lang == 1 {
        "noch nie gesehen"
    } else {
        "never seen"
    }
}

/// Unix seconds now — the UI-side render clock for relative age labels.
pub(crate) fn unix_now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0)
}

/// Render the human sync-status line from the machine fields — prose is
/// presentation, so it lives here and not in the shared data.
pub(crate) fn sync_status_label(lang: i32, state: u8, last_sync_min: u32, sync_queue: u32) -> String {
    match (lang, state) {
        (1, 1) => format!("Synchronisiere… {sync_queue} ausstehend"),
        (1, 2) => format!("Offline · letzter Sync {}", ago_label(lang, last_sync_min)),
        (1, _) => format!("Synchronisiert · {}", ago_label(lang, last_sync_min)),
        (_, 1) => format!("Syncing… {sync_queue} items left"),
        (_, 2) => format!("Offline · last sync {}", ago_label(lang, last_sync_min)),
        (_, _) => format!("Synced · {}", ago_label(lang, last_sync_min)),
    }
}

/// Human size for a KiB count, e.g. `"920 KiB"` / `"1.8 MiB"`.
pub(crate) fn size_label(size_kib: u32) -> String {
    if size_kib >= 1024 {
        format!("{:.1} MiB", f64::from(size_kib) / 1024.0)
    } else {
        format!("{size_kib} KiB")
    }
}

/// Human "last backup" cell ([`molt_core::WorkspaceInfo::NEVER`] = never).
pub(crate) fn backup_when_label(lang: i32, minutes: u32) -> String {
    if minutes == molt_core::WorkspaceInfo::NEVER {
        if lang == 1 { "nie" } else { "never" }.to_string()
    } else {
        ago_label(lang, minutes)
    }
}

/// The bucket-side label of an orphan/unknown backup row: a foreign key
/// shows its raw object key; a real orphan is known only by its
/// workspace-id pseudonym (backup objects carry no display names —
/// `backup_restore_design.md` §6.2), shortened for the table.
pub(crate) fn orphan_remote_label(o: &molt_core::BackupOrphan) -> String {
    if !o.name.is_empty() {
        return o.name.clone();
    }
    short_hex_id(&o.id)
}

/// Shorten a 64-hex workspace-id pseudonym for a table cell. A real id is
/// 64 ASCII hex chars (`parse_backup_key` pins it), so byte slicing is
/// safe — same idiom as the checksum cell.
pub(crate) fn short_hex_id(id: &str) -> String {
    match id.get(..12) {
        Some(short) if id.len() > 12 => format!("{short}…"),
        _ => id.to_string(),
    }
}

/// "Founder" label per language.
pub(crate) fn strings_founder(lang: i32) -> &'static str {
    if lang == 1 {
        "Gründer · versiegelt"
    } else {
        "Founder · sealed"
    }
}

/// A ritual seat's status line once the member activated (state 1/2).
pub(crate) fn seat_state_label(lang: i32, state: u8) -> String {
    match (lang, state) {
        (1, 4) => "versiegelt · Schlüssel gesichert",
        (1, 3) => "hat die Satzung abgelehnt",
        (1, 2) => "versiegelt · sichert den Schlüssel…",
        (1, _) => "Schlüssel erhalten · signiert…",
        (_, 4) => "sealed · key secured",
        (_, 3) => "declined the charter",
        (_, 2) => "sealed · securing the key…",
        (_, _) => "key received · signing…",
    }
    .to_string()
}

/// The genesis frame reached no relay: the founding succeeded locally, but
/// nobody else can learn of it until it is republished. Said plainly, because
/// the operator's next action (check the relays, then re-found) depends on
/// knowing which half failed.
pub(crate) fn genesis_undelivered_copy(lang: i32) -> &'static str {
    if lang == 1 {
        "Die Republik wurde hier gegründet, aber der Genesis-Block erreichte kein Relay - die anderen Mitglieder wissen nichts davon."
    } else {
        "The republic was founded here, but the genesis reached no relay - the other members have not been told."
    }
}

/// Render a chat timestamp as `2026-06-02 13:37 (~20 minutes ago)` in the
/// local timezone. The relative part refreshes with every surfaces push.
pub(crate) fn when_label(lang: i32, ts: u64) -> String {
    when_label_at(lang, ts, chrono::Utc::now().timestamp())
}

/// [`when_label`] against an explicit "now" (testable). The relative part
/// renders in the ACTIVE language (a cached English "(~2 days ago)" was
/// leaking into the German UI — user report 2026-07-18).
pub(crate) fn when_label_at(lang: i32, ts: u64, now: i64) -> String {
    let Ok(secs) = i64::try_from(ts) else {
        return String::new();
    };
    let Some(utc) = chrono::DateTime::from_timestamp(secs, 0) else {
        return String::new();
    };
    let local = utc.with_timezone(&chrono::Local);
    let ago = (now - secs).max(0);
    let de = lang == 1;
    let rel = if ago < 60 {
        if de { "gerade eben".to_string() } else { "just now".to_string() }
    } else if ago < 3600 {
        let m = ago / 60;
        if de {
            format!("vor ~{m} Minute{}", if m == 1 { "" } else { "n" })
        } else {
            format!("~{m} minute{} ago", if m == 1 { "" } else { "s" })
        }
    } else if ago < 86_400 {
        let h = ago / 3600;
        if de {
            format!("vor ~{h} Stunde{}", if h == 1 { "" } else { "n" })
        } else {
            format!("~{h} hour{} ago", if h == 1 { "" } else { "s" })
        }
    } else {
        let d = ago / 86_400;
        if de {
            format!("vor ~{d} Tag{}", if d == 1 { "" } else { "en" })
        } else {
            format!("~{d} day{} ago", if d == 1 { "" } else { "s" })
        }
    };
    format!("{} ({rel})", local.format("%Y-%m-%d %H:%M"))
}

/// The colorful (Twemoji) nav icon for a sub-view key. Keys repeating across
/// surfaces (archive, proposals, status, …) deliberately share one glyph.
pub(crate) fn view_icon(key: &str) -> &'static str {
    match key {
        "charter" => "📜",
        "status" => "📡",
        "members" => "👥",
        "uploads" => "📎",
        "persistent" => "📌",
        "pending" => "🗳️",
        "declined" => "🚫",
        "today" => "💬",
        "archive" => "🗄️",
        "brain" => "🧠",
        "proposals" => "🗳️",
        "accepted" => "✅",
        "denied" => "❌",
        "board" => "📋",
        "plan" => "🗓️",
        "create" => "✨",
        "my-quests" => "🎯",
        "secrets" => "🔐",
        "requests" => "📤",
        "unsealed" => "🔓",
        "balance" => "💰",
        "history" => "📜",
        "send" => "📤",
        "receive" => "📥",
        "settings" => "⚙️",
        _ => "▪️",
    }
}

/// Tiny bilingual pick for labels that live in Rust-side projections.
pub(crate) fn strings_pick(de: bool, en: &str, de_s: &str) -> String {
    if de { de_s.to_string() } else { en.to_string() }
}

/// Human size for a byte count, e.g. `"680 B"` / `"48 KiB"` / `"1.2 MiB"`.
/// Bytes as a decimal-GB field value ("1", "1.5", "0.25"): integer math,
/// two decimals at most, trailing zeros dropped.
pub(crate) fn gb_label(bytes: u64) -> String {
    let whole = bytes / 1_000_000_000;
    let cents = (bytes % 1_000_000_000) / 10_000_000;
    if cents == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{cents:02}").trim_end_matches('0').to_string()
    }
}

/// The inverse of [`gb_label`]: a decimal-GB field value to bytes
/// (`None` = not a number; up to two decimals count).
pub(crate) fn gb_bytes(text: &str) -> Option<u64> {
    let text = text.trim().replace(',', ".");
    let (whole, frac) = text.split_once('.').unwrap_or((text.as_str(), ""));
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let frac: String = frac.chars().take(2).collect();
    if !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let cents: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<2}").parse().ok()? };
    whole
        .checked_mul(1_000_000_000)?
        .checked_add(cents.checked_mul(10_000_000)?)
}

pub(crate) fn file_size_label(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        size_label(u32::try_from(bytes / 1024).unwrap_or(u32::MAX))
    }
}

/// Split the charter into up to `max` visually balanced columns at word
/// boundaries (~320 chars per column) — a DISPLAY split only, the text
/// itself is untouched: short charters stay single-column, long ones use
/// the status panel's width. Empty input yields no columns.
pub(crate) fn charter_columns(text: &str, max: usize) -> Vec<String> {
    const PER_COL: usize = 320;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let cols = trimmed.len().div_ceil(PER_COL).clamp(1, max.max(1));
    let target = trimmed.len().div_ceil(cols);
    let mut out = Vec::new();
    let mut rest = trimmed;
    for _ in 0..cols - 1 {
        if rest.trim().is_empty() {
            break;
        }
        // cut at the first whitespace at/after the balance target
        // (char_indices keeps every cut on a character boundary); a
        // whitespace-free tail keeps the remainder in one column
        let cut = rest
            .char_indices()
            .find(|(i, c)| *i >= target && c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(cut);
        out.push(head.trim().to_string());
        rest = tail;
    }
    if !rest.trim().is_empty() {
        out.push(rest.trim().to_string());
    }
    out
}

/// The uploads table's "expires in" cell: uploads are ephemeral like chat,
/// so the share ages out of the read contract at `expires_ts` (share time +
/// the org's chat retention window). 0 = unknown age, no deadline; an
/// unavailable share has nothing left to expire (both "—").
pub(crate) fn expires_label(lang: i32, now: u64, expires_ts: u64, available: bool) -> String {
    let de = lang == 1;
    if !available || expires_ts == 0 {
        return "-".to_string();
    }
    if expires_ts <= now {
        return strings_pick(de, "expired", "abgelaufen");
    }
    let left = expires_ts - now;
    if left < 3600 {
        format!("in {} min", (left / 60).max(1))
    } else if left < 86_400 {
        format!("in {} h", left / 3600)
    } else {
        let d = left / 86_400;
        let unit = strings_pick(
            de,
            if d == 1 { "day" } else { "days" },
            if d == 1 { "Tag" } else { "Tagen" },
        );
        format!("in {d} {unit}")
    }
}

/// The file's own date as a local calendar day, e.g. `"2026-07-01"`.
pub(crate) fn file_date_label(ts: u64) -> String {
    let Ok(secs) = i64::try_from(ts) else {
        return String::new();
    };
    let Some(utc) = chrono::DateTime::from_timestamp(secs, 0) else {
        return String::new();
    };
    utc.with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string()
}

/// Localized surface label for the sidebar (0 = English, 1 = German) —
/// presentation, like [`seat_state_label`]; the machine key stays
/// [`Surface::as_str`].
pub(crate) fn surface_name(lang: i32, sf: Surface) -> &'static str {
    if lang == 1 {
        match sf {
            Surface::Organization => "Organisation",
            Surface::Chat => "Chat",
            Surface::Memory => "Shared Memory",
            // user-decided 2026-08-11: the board is named for what it shows
            Surface::Quests => "Kanban",
            // user-decided 2026-08-19: the surface is named "Vault" in
            // both languages - it is the product's word for it
            Surface::Vault => "Vault",
            Surface::Wallet => "Wallet",
            Surface::Files => "Shared Files",
        }
    } else {
        match sf {
            Surface::Organization => "Organization",
            Surface::Chat => "Chat",
            Surface::Memory => "Shared Memory",
            Surface::Quests => "Kanban",
            Surface::Vault => "Vault",
            Surface::Wallet => "Wallet",
            Surface::Files => "Shared Files",
        }
    }
}

/// Localized sub-view label for a nav row. The English display label comes
/// from the shared `molt-core` vocabulary ([`Surface::views`]); German maps
/// by the machine key here — keys repeating across surfaces (archive,
/// proposals, status, …) deliberately share one word.
pub(crate) fn view_label(lang: i32, key: &str, en: &str) -> String {
    if lang != 1 {
        return en.to_string();
    }
    match key {
        "members" => "Mitglieder",
        "uploads" => "Temporäre Uploads",
        "persistent" => "Dauerhafte Uploads",
        "pending" => "Ausstehend",
        "declined" => "Abgelehnt",
        "today" => "Allgemein",
        "archive" => "Archiv",
        "proposals" => "Vorschläge",
        "accepted" => "Angenommen",
        "denied" => "Abgelehnt",
        "create" => "Erstellen",
        "plan" => "Planung",
        "my-quests" => "Meine",
        "secrets" => "Geheimnisse",
        "requests" => "Anträge",
        "unsealed" => "Entsiegelt",
        "balance" => "Kassenstand",
        "history" => "Verlauf",
        "send" => "Senden",
        "receive" => "Empfangen",
        "settings" => "Einstellungen",
        // Status, Multisig-Wiki, Board — shared or product terms
        _ => en,
    }
    .to_string()
}

/// The default transition op the GUI uses when proposing on a surface.
pub(crate) fn default_op(sf: Surface) -> &'static str {
    match sf {
        Surface::Memory => "add_note",
        Surface::Quests => "add_quest",
        Surface::Vault => "seal_secret",
        Surface::Wallet => "transfer",
        // organization changes come from the dedicated edit modals
        // (org-propose carries the specific op); chat and files are ungated
        Surface::Chat | Surface::Files | Surface::Organization => "note",
    }
}

pub(crate) fn to_screen(s: AppScreen) -> Screen {
    match s {
        AppScreen::Choice => Screen::Choice,
        AppScreen::Create => Screen::Create,
        AppScreen::Open => Screen::Open,
        AppScreen::Join => Screen::Join,
        AppScreen::Restore => Screen::Restore,
        AppScreen::Settings => Screen::Settings,
        AppScreen::Main => Screen::Main,
    }
}

pub(crate) fn from_screen(s: Screen) -> AppScreen {
    match s {
        Screen::Choice => AppScreen::Choice,
        Screen::Create => AppScreen::Create,
        Screen::Open => AppScreen::Open,
        Screen::Join => AppScreen::Join,
        Screen::Restore => AppScreen::Restore,
        Screen::Settings => AppScreen::Settings,
        Screen::Main => AppScreen::Main,
    }
}

/// Map a theme name to the Theme global's index.
pub(crate) fn theme_index(s: &str) -> i32 {
    match s {
        "classic" => 0,
        "brutalism" => 2,
        _ => 1,
    }
}

/// Map a theme index back to its name.
pub(crate) fn theme_name(i: i32) -> String {
    match i {
        0 => "classic",
        2 => "brutalism",
        _ => "dark",
    }
    .to_string()
}

#[cfg(test)]
mod gb_tests {
    use super::*;

    /// The GB field round-trips through integer math, two decimals, no
    /// trailing zeros.
    #[test]
    fn the_gb_field_round_trips() {
        assert_eq!(gb_label(1_000_000_000), "1");
        assert_eq!(gb_label(1_073_741_824), "1.07");
        assert_eq!(gb_label(2_500_000_000), "2.5");
        assert_eq!(gb_label(250_000_000), "0.25");
        assert_eq!(gb_bytes("2.5"), Some(2_500_000_000));
        assert_eq!(gb_bytes("2,50"), Some(2_500_000_000));
        assert_eq!(gb_bytes(".25"), Some(250_000_000));
        assert_eq!(gb_bytes("3"), Some(3_000_000_000));
        assert_eq!(gb_bytes("x"), None);
    }
}
