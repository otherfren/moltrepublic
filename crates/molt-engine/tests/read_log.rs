// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! `Command::ReadLog` against a log directory in a tempdir.

use std::path::Path;

use molt_core::{Command, GroupConfig, LogEntry, LogFilter, Reply, SessionView};

fn line(day: u8, sec: u8, level: &str, msg: &str) -> String {
    format!("2026-11-{day:02}T10:00:{sec:02}.000000Z {level:>5} moltd: {msg}\n")
}

fn write(dir: &Path, day: u8, text: &str) {
    std::fs::write(dir.join(format!("moltd.2026-11-{day:02}.log")), text).expect("write");
}

fn engine(dir: &Path) -> molt_engine::WalletHandle {
    let session = SessionView { log_dir: dir.display().to_string(), ..SessionView::default() };
    molt_engine::spawn(GroupConfig::demo(), session)
}

async fn read(w: &molt_engine::WalletHandle, lines: usize, level: LogFilter) -> (String, Vec<LogEntry>) {
    match w.execute(Command::ReadLog { lines, level }).await {
        Ok(Reply::Log { path, lines }) => (path, lines),
        other => panic!("expected a log, got {other:?}"),
    }
}

fn msgs(lines: &[LogEntry]) -> Vec<&str> {
    lines.iter().map(|l| l.text.rsplit(": ").next().expect("a message")).collect()
}

#[tokio::test]
async fn the_newest_entries_come_back_across_the_daily_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(dir.path(), 11, &(line(11, 1, "WARN", "old warn") + &line(11, 2, "INFO", "old info")));
    write(dir.path(), 12, &(line(12, 1, "INFO", "new info") + &line(12, 2, "ERROR", "new error")));
    // neither another node's file nor a stray one is read
    std::fs::write(dir.path().join("other.2026-11-13.log"), line(13, 1, "ERROR", "foreign")).expect("write");
    let w = engine(dir.path());

    let (path, all) = read(&w, 3, LogFilter::All).await;
    assert_eq!(path, dir.path().join("moltd.2026-11-12.log").display().to_string());
    assert_eq!(msgs(&all), ["old info", "new info", "new error"]);
    assert_eq!(all[2].level, "ERROR");

    let (_, warn) = read(&w, 50, LogFilter::Warn).await;
    assert_eq!(msgs(&warn), ["old warn", "new error"]);
}

#[tokio::test]
async fn the_line_count_is_capped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text: String = (0..molt_core::READ_LOG_MAX_LINES + 20).map(|_| line(12, 1, "WARN", "w")).collect();
    write(dir.path(), 12, &text);
    let (_, lines) = read(&engine(dir.path()), usize::MAX, LogFilter::Warn).await;
    assert_eq!(lines.len(), molt_core::READ_LOG_MAX_LINES);
}

#[tokio::test]
async fn no_log_file_is_an_empty_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    for d in [dir.path().join("missing"), dir.path().to_path_buf()] {
        assert_eq!(read(&engine(&d), 10, LogFilter::All).await, (String::new(), Vec::new()));
    }
    let off = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    assert_eq!(read(&off, 10, LogFilter::All).await, (String::new(), Vec::new()));
}

#[test]
fn only_the_end_of_a_big_file_is_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text: String = (0..60).map(|s| line(12, s, "WARN", &format!("m{s}"))).collect();
    write(dir.path(), 12, &text);
    write(dir.path(), 11, &line(11, 1, "WARN", "yesterday"));
    let one = u64::try_from(line(12, 1, "WARN", "m1").len()).expect("fits");
    // three and a half lines of budget: the half line is dropped, and the
    // spent budget keeps yesterday's file closed
    let (_, got) = molt_engine::log_read::read(dir.path(), LogFilter::Warn, 500, one * 3 + one / 2);
    assert_eq!(msgs(&got), ["m57", "m58", "m59"]);
}

#[tokio::test]
async fn a_changed_log_file_switch_waits_for_the_restart() {
    let w = molt_engine::spawn(GroupConfig::demo(), SessionView::default());
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else { panic!("read") };
    let mut settings = sv.settings.clone();
    settings.log_file = false;
    w.execute(Command::SaveSettings { settings }).await.expect("saved");
    let Ok(Reply::Session(sv)) = w.execute(Command::ReadSession).await else { panic!("read") };
    assert_eq!(sv.restart_required, ["node.log_file"]);
}
