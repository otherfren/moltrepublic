// SPDX-License-Identifier: GPL-3.0-or-later

//! Reading the node's log file back ([`crate::Command::ReadLog`]): the pure
//! half. The file is `tracing_subscriber`'s full format without colour,
//! `2026-11-12T19:33:37.556130Z  WARN target: message key=value`.

use crate::{LogEntry, LogFilter};

/// Bytes of one entry kept; the rest is cut.
pub const ENTRY_MAX_BYTES: usize = 4096;

/// The level of a line that starts an entry; `None` for any other line.
#[must_use]
pub fn level_of(line: &str) -> Option<&'static str> {
    let (stamp, rest) = line.split_once(' ')?;
    let b = stamp.as_bytes();
    let stamped = b.len() >= 20 && b[0].is_ascii_digit() && b[10] == b'T' && b[b.len() - 1] == b'Z';
    if !stamped {
        return None;
    }
    let level = rest.trim_start().split(' ').next()?;
    ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"].into_iter().find(|l| *l == level)
}

/// Whether `name` is one of the daily files, `<prefix>.YYYY-MM-DD.<suffix>`.
/// Such names sort by age.
#[must_use]
pub fn is_daily_file(name: &str, prefix: &str, suffix: &str) -> bool {
    let date = name
        .strip_prefix(prefix)
        .and_then(|n| n.strip_prefix('.'))
        .and_then(|n| n.strip_suffix(suffix))
        .and_then(|n| n.strip_suffix('.'));
    date.is_some_and(|d| {
        d.len() == 10
            && d.bytes().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == b'-' } else { c.is_ascii_digit() })
    })
}

/// The entries in `bytes`, oldest first. `cut` = the bytes start somewhere
/// inside the file, so whatever precedes the first entry start is dropped.
#[must_use]
pub fn entries(bytes: &[u8], cut: bool) -> Vec<LogEntry> {
    let text = String::from_utf8_lossy(bytes);
    let mut text: &str = &text;
    if cut {
        text = text.split_once('\n').map_or("", |(_, rest)| rest);
    }
    let mut out: Vec<LogEntry> = Vec::new();
    // an entry whose start a cut removed: its remaining lines belong nowhere
    let mut headless = cut;
    for line in text.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.trim().is_empty()) {
        match (level_of(line), out.last_mut()) {
            (Some(level), _) => {
                headless = false;
                out.push(LogEntry { level: level.to_string(), text: line.to_string() });
            }
            (None, _) if headless => {}
            (None, Some(last)) => {
                last.text.push('\n');
                last.text.push_str(line);
            }
            (None, None) => out.push(LogEntry { level: String::new(), text: line.to_string() }),
        }
    }
    for e in &mut out {
        if e.text.len() > ENTRY_MAX_BYTES {
            let mut end = ENTRY_MAX_BYTES;
            while !e.text.is_char_boundary(end) {
                end -= 1;
            }
            e.text.truncate(end);
            e.text.push_str(" ...");
        }
    }
    out
}

/// The newest `cap` entries passing `filter`, oldest first. `files` yields
/// each file's entries, NEWEST file first, and is only pulled while short.
pub fn tail(
    files: impl IntoIterator<Item = Vec<LogEntry>>,
    filter: LogFilter,
    cap: usize,
) -> Vec<LogEntry> {
    let keep = |e: &LogEntry| filter == LogFilter::All || e.level == "WARN" || e.level == "ERROR";
    let mut newest_first: Vec<LogEntry> = Vec::new();
    let mut files = files.into_iter();
    while newest_first.len() < cap {
        let Some(file) = files.next() else { break };
        let room = cap - newest_first.len();
        newest_first.extend(file.into_iter().rev().filter(keep).take(room));
    }
    newest_first.reverse();
    newest_first
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "2026-11-12T19:33:37.556130Z  INFO moltd: loaded config path=/a/b.toml";
    const WARN: &str = "2026-11-12T19:33:38.000001Z  WARN molt_net::relay: dial failed relay=ws://x error=refused";
    const ERROR: &str = "2026-11-12T19:33:39.123456Z ERROR molt_engine: boom";

    fn texts(v: &[LogEntry]) -> Vec<&str> {
        v.iter().map(|e| e.text.as_str()).collect()
    }

    #[test]
    fn the_level_sits_behind_the_timestamp() {
        assert_eq!(level_of(INFO), Some("INFO"));
        assert_eq!(level_of(WARN), Some("WARN"));
        assert_eq!(level_of(ERROR), Some("ERROR"));
        assert_eq!(level_of("2026-11-12T19:33:39.123456Z DEBUG a: b"), Some("DEBUG"));
        assert_eq!(level_of("2026-11-12T19:33:39.123456Z TRACE a: b"), Some("TRACE"));
        assert_eq!(level_of("   at src/lib.rs:12 WARN"), None);
        assert_eq!(level_of("WARN no timestamp"), None);
        assert_eq!(level_of("2026-11-12T19:33:39.123456Z  NOTE a: b"), None);
        assert_eq!(level_of(""), None);
    }

    #[test]
    fn only_dated_names_of_this_prefix_are_daily_files() {
        let is = |n: &str| is_daily_file(n, "moltd", "log");
        assert!(is("moltd.2026-11-12.log"));
        assert!(!is("moltd1.2026-11-12.log"));
        assert!(!is("moltd.2026-11-12.log.bak"));
        assert!(!is("moltd.latest.log"));
        assert!(!is("moltd.2026-11-1x.log"));
        assert!(!is("moltd.log"));
    }

    #[test]
    fn a_multi_line_message_is_one_entry_at_its_first_lines_level() {
        let text = format!("{INFO}\n{ERROR}\n  caused by: x\n  WARN in the detail\n{WARN}\n");
        let got = entries(text.as_bytes(), false);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].level, "ERROR");
        assert_eq!(got[1].text, format!("{ERROR}\n  caused by: x\n  WARN in the detail"));
        assert_eq!(got[2].level, "WARN");
    }

    #[test]
    fn a_cut_read_drops_everything_before_its_first_entry() {
        let text = format!("9:33:36.1Z  WARN half a line\n  its continuation\n{INFO}\n{WARN}\n");
        assert_eq!(texts(&entries(text.as_bytes(), true)), [INFO, WARN]);
        // the cut may fall exactly on a line start: that line is not trusted either
        let text = format!("{ERROR}\n{INFO}\n");
        assert_eq!(texts(&entries(text.as_bytes(), true)), [INFO]);
        assert!(entries(b"no newline at all", true).is_empty());
    }

    #[test]
    fn an_uncut_file_keeps_lines_without_a_header() {
        let got = entries(format!("stray\n{INFO}\n").as_bytes(), false);
        assert_eq!((got[0].level.as_str(), got[0].text.as_str()), ("", "stray"));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn empty_input_and_blank_lines_give_nothing() {
        assert!(entries(b"", false).is_empty());
        assert!(entries(b"\n\r\n\n", false).is_empty());
        assert_eq!(texts(&entries(format!("{INFO}\r\n\r\n").as_bytes(), false)), [INFO]);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_refused() {
        let mut bytes = format!("{WARN} name=").into_bytes();
        bytes.extend_from_slice(&[0xff, 0xfe, b'\n']);
        let got = entries(&bytes, false);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].level, "WARN");
        assert!(got[0].text.ends_with("name=\u{fffd}\u{fffd}"), "{}", got[0].text);
    }

    #[test]
    fn an_oversized_entry_is_cut_on_a_char_boundary() {
        let text = format!("{WARN} {}\n", "ä".repeat(ENTRY_MAX_BYTES));
        let got = entries(text.as_bytes(), false);
        assert!(got[0].text.len() <= ENTRY_MAX_BYTES + 4, "{}", got[0].text.len());
        assert!(got[0].text.ends_with(" ..."));
    }

    fn file(lines: &[&str]) -> Vec<LogEntry> {
        entries((lines.join("\n") + "\n").as_bytes(), false)
    }

    #[test]
    fn the_filter_keeps_warnings_and_errors() {
        let got = tail([file(&["stray", INFO, WARN, ERROR, INFO])], LogFilter::Warn, 10);
        assert_eq!(texts(&got), [WARN, ERROR]);
        let got = tail([file(&[INFO, WARN, ERROR])], LogFilter::All, 10);
        assert_eq!(texts(&got), [INFO, WARN, ERROR]);
    }

    #[test]
    fn the_cap_keeps_the_newest() {
        assert_eq!(texts(&tail([file(&[INFO, WARN, ERROR])], LogFilter::All, 2)), [WARN, ERROR]);
        assert!(tail([file(&[INFO])], LogFilter::All, 0).is_empty());
    }

    #[test]
    fn a_short_newest_file_continues_into_the_one_before() {
        let today = file(&[ERROR]);
        let yesterday = file(&[INFO, WARN]);
        let got = tail([today.clone(), yesterday.clone()], LogFilter::All, 2);
        assert_eq!(texts(&got), [WARN, ERROR]);
        let got = tail([today, yesterday], LogFilter::Warn, 5);
        assert_eq!(texts(&got), [WARN, ERROR]);
    }

    #[test]
    fn an_older_file_is_not_read_once_the_cap_is_met() {
        let mut pulled = 0;
        let files = [file(&[WARN, ERROR]), file(&[WARN])].into_iter().inspect(|_| pulled += 1);
        assert_eq!(tail(files, LogFilter::Warn, 2).len(), 2);
        assert_eq!(pulled, 1);
    }
}
