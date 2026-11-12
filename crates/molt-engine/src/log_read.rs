// SPDX-License-Identifier: GPL-3.0-or-later

//! The file half of [`molt_core::Command::ReadLog`]: a bounded read from
//! the end of the daily log files. Blocking - never call it on the actor.

use std::path::Path;

use molt_core::{LogEntry, LogFilter};

/// Bytes one read scans at most, across all files.
pub const SCAN_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The newest file's path and the newest `lines` entries passing `filter`
/// (oldest first), scanning at most `scan_max` bytes. No file = `("", [])`.
#[must_use]
pub fn read(dir: &Path, filter: LogFilter, lines: usize, scan_max: u64) -> (String, Vec<LogEntry>) {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| {
            molt_core::log_tail::is_daily_file(n, molt_config::LOG_FILE_PREFIX, molt_config::LOG_FILE_SUFFIX)
        })
        .collect();
    names.sort_unstable_by(|a, b| b.cmp(a));
    let Some(newest) = names.first() else {
        return (String::new(), Vec::new());
    };
    let path = dir.join(newest).display().to_string();
    let mut budget = scan_max;
    let files = names.iter().filter_map(|name| {
        let (bytes, cut) = read_end(&dir.join(name), budget)?;
        budget = budget.saturating_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        Some(molt_core::log_tail::entries(&bytes, cut))
    });
    let entries = molt_core::log_tail::tail(files, filter, lines.min(molt_core::READ_LOG_MAX_LINES));
    (path, entries)
}

/// The last `max` bytes of `path`, and whether that left a part unread.
fn read_end(path: &Path, max: u64) -> Option<(Vec<u8>, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    if max == 0 {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some((bytes, start > 0))
}
