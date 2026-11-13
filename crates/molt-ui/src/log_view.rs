// SPDX-License-Identifier: GPL-3.0-or-later
//! Settings › Error log: the pure half of the viewer. Filters and pages
//! the entries a `ReadLog` returned, cuts each line into tinted segments
//! (stamp, level, target, `key=` of a field) and wraps the rows to the
//! columns the pane fits. The same rows render twice - tinted underneath,
//! as one selectable text on top - so both must break identically.

use molt_core::LogEntry;

/// Entries per page.
pub const PAGE: usize = 200;
/// A continuation row's indent.
const INDENT: &str = "  ";

/// A segment's tint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Text = 0,
    Stamp = 1,
    Error = 2,
    Warn = 3,
    Info = 4,
    Debug = 5,
    Target = 6,
    Key = 7,
}

/// Which entries show: 0 errors · 1 warnings and errors · 2 info and up ·
/// 3 everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Level(pub i32);

impl Level {
    fn admits(self, level: &str) -> bool {
        let rank = match level {
            "ERROR" => 0,
            "WARN" => 1,
            "INFO" => 2,
            _ => 3,
        };
        rank <= self.0.clamp(0, 3)
    }
}

/// One display row: its segments, never empty.
pub type Row = Vec<(String, Tone)>;

/// The viewer's state.
#[derive(Default)]
pub struct LogView {
    pub entries: Vec<LogEntry>,
    pub level: Level,
    pub query: String,
    /// 0 = the newest page.
    pub page: usize,
    /// Columns a row may take; 0 = no wrap.
    pub cols: usize,
}

/// What one page shows.
#[derive(Debug, Default, PartialEq)]
pub struct Page {
    pub rows: Vec<Row>,
    /// The rows as one text, one line per row (the selectable layer).
    pub plain: String,
    /// The page's entries, unwrapped (Copy).
    pub raw: String,
    /// 1-based range of the page among the matches, newest last.
    pub first: usize,
    pub last: usize,
    pub total: usize,
    pub older: bool,
    pub newer: bool,
}

impl LogView {
    fn matches(&self) -> Vec<&LogEntry> {
        let q = self.query.trim().to_lowercase();
        self.entries
            .iter()
            .filter(|e| self.level.admits(&e.level))
            .filter(|e| q.is_empty() || e.text.to_lowercase().contains(&q))
            .collect()
    }

    /// Pages among the current matches (at least 1).
    pub fn pages(&self) -> usize {
        self.matches().len().div_ceil(PAGE).max(1)
    }

    /// The current page, oldest entry first.
    pub fn page(&self) -> Page {
        let all = self.matches();
        let total = all.len();
        let page = self.page.min(self.pages() - 1);
        let end = total.saturating_sub(page * PAGE);
        let start = end.saturating_sub(PAGE);
        let shown = &all[start..end];
        let rows: Vec<Row> = shown.iter().flat_map(|e| wrap(segments(&e.text), self.cols)).collect();
        let plain = rows
            .iter()
            .map(|r| r.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        Page {
            rows,
            plain,
            raw: shown.iter().map(|e| e.text.as_str()).collect::<Vec<_>>().join("\n"),
            first: if total == 0 { 0 } else { start + 1 },
            last: end,
            total,
            older: start > 0,
            newer: page > 0,
        }
    }
}

fn level_tone(level: &str) -> Tone {
    match level {
        "ERROR" => Tone::Error,
        "WARN" => Tone::Warn,
        "INFO" => Tone::Info,
        _ => Tone::Debug,
    }
}

/// One entry as tinted lines: the head line `stamp level target: message
/// key=value…`, every further line of the entry plain.
pub fn segments(text: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        out.push(if i == 0 { head(line) } else { fields(line) });
    }
    out
}

fn head(line: &str) -> Row {
    let Some(level) = molt_core::log_tail::level_of(line) else {
        return fields(line);
    };
    let (stamp, rest) = line.split_once(' ').unwrap_or((line, ""));
    let mut row: Row = vec![(stamp.to_string(), Tone::Stamp)];
    let pad = rest.len() - rest.trim_start().len();
    let rest = &rest[pad..];
    row.push((format!("{}{level}", " ".repeat(pad + 1)), level_tone(level)));
    let rest = &rest[level.len()..];
    // `target: ` - a module path, no spaces
    let body = rest.trim_start();
    let gap = &rest[..rest.len() - body.len()];
    match body.split_once(": ") {
        Some((target, msg)) if !target.is_empty() && !target.contains(' ') => {
            row.push((format!("{gap}{target}:"), Tone::Target));
            row.extend(fields(&format!(" {msg}")));
        }
        _ => row.extend(fields(rest)),
    }
    row
}

/// Plain text with each `key=` of a structured field tinted.
fn fields(text: &str) -> Row {
    let mut row: Row = Vec::new();
    let mut push = |t: String, tone: Tone| match row.last_mut() {
        Some((last, lt)) if *lt == tone => last.push_str(&t),
        _ => row.push((t, tone)),
    };
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            push(" ".to_string(), Tone::Text);
        }
        match word.split_once('=') {
            Some((key, value))
                if !key.is_empty()
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') =>
            {
                push(format!("{key}="), Tone::Key);
                if !value.is_empty() {
                    push(value.to_string(), Tone::Text);
                }
            }
            _ if !word.is_empty() => push(word.to_string(), Tone::Text),
            _ => {}
        }
    }
    if row.is_empty() {
        row.push((" ".to_string(), Tone::Text));
    }
    row
}

/// Rows at most `cols` characters wide; a continuation starts with
/// [`INDENT`]. `cols` 0 = as they are.
pub fn wrap(rows: Vec<Row>, cols: usize) -> Vec<Row> {
    let indent = INDENT.chars().count();
    if cols <= indent {
        return rows;
    }
    let mut out = Vec::new();
    for row in rows {
        if row.iter().map(|(t, _)| t.chars().count()).sum::<usize>() <= cols {
            out.push(row);
            continue;
        }
        let mut cur: Row = Vec::new();
        let mut room = cols;
        for (text, tone) in row {
            let mut chars = text.chars().peekable();
            while chars.peek().is_some() {
                if room == 0 {
                    out.push(std::mem::take(&mut cur));
                    cur.push((INDENT.to_string(), Tone::Text));
                    room = cols - indent;
                }
                let piece: String = chars.by_ref().take(room).collect();
                room -= piece.chars().count();
                cur.push((piece, tone));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, msg: &str) -> LogEntry {
        LogEntry {
            level: level.to_string(),
            text: format!("2026-11-12T10:00:00.000000Z {level:>5} molt_net::relay: {msg}"),
        }
    }

    fn joined(r: &Row) -> String {
        r.iter().map(|(t, _)| t.as_str()).collect()
    }

    #[test]
    fn a_head_line_tints_stamp_level_target_and_field_keys_and_keeps_its_text() {
        let e = entry("WARN", "relay connect failed relay=wss://a error=x y");
        let rows = segments(&e.text);
        assert_eq!(rows.len(), 1);
        assert_eq!(joined(&rows[0]), e.text, "the text survives the cut unchanged");
        let tone_of = |s: &str| rows[0].iter().find(|(t, _)| t.trim() == s).map(|(_, tone)| *tone);
        assert_eq!(tone_of("2026-11-12T10:00:00.000000Z"), Some(Tone::Stamp));
        assert_eq!(tone_of("WARN"), Some(Tone::Warn));
        assert_eq!(tone_of("molt_net::relay:"), Some(Tone::Target));
        assert_eq!(tone_of("relay="), Some(Tone::Key));
        assert_eq!(tone_of("error="), Some(Tone::Key));
        for (lvl, tone) in [("ERROR", Tone::Error), ("INFO", Tone::Info), ("DEBUG", Tone::Debug)] {
            let r = segments(&entry(lvl, "m").text);
            assert!(r[0].iter().any(|(t, to)| t.trim() == lvl && *to == tone), "{lvl}");
        }
        // a continuation line and a line without a stamp stay readable
        let multi = segments("2026-11-12T10:00:00.000000Z ERROR moltd: panic\n  at x.rs:1");
        assert_eq!(multi.len(), 2);
        assert_eq!(joined(&multi[1]), "  at x.rs:1");
        assert_eq!(joined(&segments("")[0]), " ", "a row is never empty");
    }

    #[test]
    fn wrapping_cuts_at_the_columns_with_an_indent_and_keeps_tones() {
        let row: Row = vec![("abcdef".into(), Tone::Stamp), ("ghij".into(), Tone::Text)];
        let out = wrap(vec![row.clone()], 4);
        assert_eq!(out.iter().map(joined).collect::<Vec<_>>(), ["abcd", "  ef", "  gh", "  ij"]);
        assert_eq!(out[1][1].1, Tone::Stamp);
        assert_eq!(out[2][1].1, Tone::Text);
        assert_eq!(wrap(vec![row.clone()], 0).len(), 1);
        assert_eq!(wrap(vec![row], 80).len(), 1);
    }

    #[test]
    fn the_view_filters_searches_and_pages_from_the_newest() {
        let mut v = LogView {
            entries: (0..450).map(|i| entry(if i % 3 == 0 { "WARN" } else { "INFO" }, &format!("n{i}"))).collect(),
            level: Level(3),
            ..LogView::default()
        };
        v.entries.push(entry("ERROR", "boom"));
        let p = v.page();
        assert_eq!((p.first, p.last, p.total, p.older, p.newer), (252, 451, 451, true, false));
        assert!(p.raw.ends_with("boom"), "the newest page ends on the newest entry");
        assert_eq!(p.plain.lines().count(), p.rows.len());
        v.page = 2;
        let p = v.page();
        assert_eq!((p.first, p.last, p.older, p.newer), (1, 51, false, true));
        v.page = 9;
        assert_eq!(v.page().first, 1, "past the end = the oldest page");

        v.page = 0;
        v.level = Level(0);
        assert_eq!(v.page().total, 1);
        v.level = Level(1);
        assert_eq!(v.page().total, 151);
        v.query = "N44".into();
        let p = v.page();
        assert!(p.total > 0 && p.raw.lines().all(|l| l.contains("n44")), "case-insensitive search");
        v.query = "nothing".into();
        let p = v.page();
        assert_eq!((p.first, p.last, p.total, p.rows.len()), (0, 0, 0, 0));
        assert_eq!(v.pages(), 1);
    }
}
