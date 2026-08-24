//! The wiki-patch diff viewer's data side: parse the git-format patch a
//! `wiki_patch` proposal carries (the exact shape `wiki::build_patch`
//! emits) and pseudo-render each file's changes as rows of colored
//! segments — added characters green, removed characters red, paired
//! change lines char-diffed via `similar`. Read-only by design: the
//! viewer browses a vote's content, it never edits.
//!
//! Slint sees only flat models built from here (bridge in `lib.rs`);
//! this module stays slint-free so it tests headless.

use similar::{ChangeTag, TextDiff};

/// Longest line pair that still gets a char diff; longer pairs render whole.
const CHAR_DIFF_MAX_LINE: usize = 2048;
/// Time budget for one line pair's char diff.
const CHAR_DIFF_DEADLINE_MS: u64 = 50;

// The parser and the file/hunk types moved DOWN to molt-core
// (shared_memory_real.md WP-A): the strict fold applies exactly what this
// viewer renders, so both must read one parse. This module keeps the
// RENDERING half (rows of colored segments).
pub use molt_core::wiki_fold::{parse_patch, PatchFile};
#[cfg(test)]
use molt_core::wiki_fold::{Hunk, HunkLine};

/// A rendered segment's tone: 0 plain · 1 added (green) · 2 removed
/// (red) · 3 meta (hunk separator).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegTone {
    Plain,
    Added,
    Removed,
    Meta,
}

#[derive(Clone, Debug)]
pub struct Seg {
    pub text: String,
    pub tone: SegTone,
}

/// One display row of the details pane.
#[derive(Clone, Debug)]
pub struct Row {
    pub segs: Vec<Seg>,
}

/// A file's details rows: context stays plain, paired -/+ runs render as
/// ONE merged row char-diffed green/{red}, unpaired lines color whole,
/// hunk boundaries separate with a meta "⋯" row.
pub fn file_rows(f: &PatchFile) -> Vec<Row> {
    let mut out = Vec::new();
    for (i, h) in f.hunks.iter().enumerate() {
        if i > 0 {
            out.push(Row {
                segs: vec![Seg {
                    text: "⋯".to_string(),
                    tone: SegTone::Meta,
                }],
            });
        }
        let lines = &h.lines;
        let mut i = 0;
        while i < lines.len() {
            match lines[i].op {
                ' ' => {
                    out.push(plain_row(&lines[i].text, SegTone::Plain));
                    i += 1;
                }
                '+' => {
                    out.push(plain_row(&lines[i].text, SegTone::Added));
                    i += 1;
                }
                '-' => {
                    // the whole removal run, then the addition run that
                    // follows — pairs char-diff, excess colors whole lines
                    let start = i;
                    while i < lines.len() && lines[i].op == '-' {
                        i += 1;
                    }
                    let removed: Vec<&str> =
                        lines[start..i].iter().map(|l| l.text.as_str()).collect();
                    let astart = i;
                    while i < lines.len() && lines[i].op == '+' {
                        i += 1;
                    }
                    let added: Vec<&str> =
                        lines[astart..i].iter().map(|l| l.text.as_str()).collect();
                    let pairs = removed.len().min(added.len());
                    for k in 0..removed.len().max(added.len()) {
                        if k < pairs {
                            out.push(char_diff_row(removed[k], added[k]));
                        } else if k < removed.len() {
                            out.push(plain_row(removed[k], SegTone::Removed));
                        } else {
                            out.push(plain_row(added[k], SegTone::Added));
                        }
                    }
                }
                _ => i += 1,
            }
        }
    }
    out
}

fn plain_row(text: &str, tone: SegTone) -> Row {
    Row {
        segs: vec![Seg {
            text: text.to_string(),
            tone,
        }],
    }
}

/// One merged char-diff row for a changed line pair: equal runs plain,
/// removed characters red, added characters green — in order.
fn char_diff_row(old: &str, new: &str) -> Row {
    // a pending patch from ANY member reaches every window unasked, and
    // Myers on two long, different lines is quadratic — bound the work so
    // one hostile 30 KB line cannot freeze every member's UI thread
    // (review 2026-08-25): past the length cap the pair renders whole, and
    // the char diff itself carries a deadline (past it `similar` falls
    // back to a coarse result)
    if old.len() > CHAR_DIFF_MAX_LINE || new.len() > CHAR_DIFF_MAX_LINE {
        return Row {
            segs: vec![
                Seg { text: old.to_string(), tone: SegTone::Removed },
                Seg { text: new.to_string(), tone: SegTone::Added },
            ],
        };
    }
    let diff = TextDiff::configure()
        .deadline(std::time::Instant::now() + std::time::Duration::from_millis(CHAR_DIFF_DEADLINE_MS))
        .diff_chars(old, new);
    let mut segs: Vec<Seg> = Vec::new();
    for change in diff.iter_all_changes() {
        let tone = match change.tag() {
            ChangeTag::Equal => SegTone::Plain,
            ChangeTag::Delete => SegTone::Removed,
            ChangeTag::Insert => SegTone::Added,
        };
        let text = change.value();
        match segs.last_mut() {
            Some(last) if last.tone == tone => last.text.push_str(text),
            _ => segs.push(Seg {
                text: text.to_string(),
                tone,
            }),
        }
    }
    Row { segs }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::Wiki;

    /// Past the length cap a changed pair renders whole (one removed, one
    /// added segment) instead of a quadratic char diff.
    #[test]
    fn an_overlong_line_pair_renders_whole_instead_of_char_diffed() {
        let old = "a".repeat(CHAR_DIFF_MAX_LINE + 1);
        let new = "b".repeat(CHAR_DIFF_MAX_LINE + 1);
        let row = char_diff_row(&old, &new);
        let tones: Vec<SegTone> = row.segs.iter().map(|s| s.tone).collect();
        assert_eq!(tones, vec![SegTone::Removed, SegTone::Added]);
        // and a short pair still gets the fine-grained diff
        let fine = char_diff_row("hello", "hallo");
        assert!(fine.segs.len() > 2, "a short pair is char-diffed: {fine:?}");
    }

    fn hl(op: char, text: &str) -> HunkLine {
        HunkLine {
            op,
            text: text.to_string(),
            newline: true,
        }
    }

    /// The real thing end to end: a changeset built in the wiki model,
    /// emitted as a patch, parsed back — the viewer must see exactly the
    /// four files with their kinds.
    #[test]
    fn a_built_patch_parses_back_into_its_files() {
        let mut w = Wiki::sample();
        let ids: Vec<(u32, String)> = w
            .nav_rows()
            .into_iter()
            .filter(|r| r.id != 0)
            .map(|r| (r.id, r.label))
            .collect();
        let id_of = |name: &str| {
            ids.iter()
                .find(|(_, l)| l == name)
                .map(|(i, _)| *i)
                .expect("doc")
        };
        let glossary = id_of("glossary.md");
        let base = "# Words\n\nshort.";
        w.set_raw(glossary, base);
        w.rename_commit(id_of("2026-06-14-relay.md"), "relay-decision")
            .expect("rename");
        w.delete(id_of("charter.md"));
        let draft = w.new_file();
        w.rename_commit(draft, "todo").expect("rename");
        w.set_raw(draft, "# Todo\n\n- first");
        let patch = w.build_patch().expect("patch");

        let files = parse_patch(&patch);
        assert_eq!(files.len(), 4);
        let by_path = |p: &str| {
            files
                .iter()
                .find(|f| f.display_path() == p)
                .expect("file present")
        };
        let deleted = by_path("charter.md");
        assert!(deleted.deleted);
        assert_eq!(deleted.marker(), "<deleted>");
        assert_eq!(deleted.header_marker(), "<deleted>");
        assert_eq!(deleted.status(), 3);
        assert!(!deleted.hunks.is_empty(), "the deletion carries its lines");
        let renamed = by_path("decisions/relay-decision.md");
        assert!(renamed.renamed && !renamed.deleted);
        assert_eq!(renamed.marker(), "<moved>");
        // the details-pane header names the whole move
        assert_eq!(
            renamed.header_marker(),
            "<moved> decisions/2026-06-14-relay.md → decisions/relay-decision.md"
        );
        // moves carry their own tone (4), distinct from edits
        assert_eq!(renamed.status(), 4);
        assert_eq!(renamed.old_path, "decisions/2026-06-14-relay.md");
        assert!(renamed.hunks.is_empty(), "a pure rename has no hunks");
        let added = by_path("todo.md");
        assert!(added.added);
        assert_eq!(added.marker(), "");
        assert_eq!(added.header_marker(), "");
        assert_eq!(added.status(), 1);
        let modified = by_path("glossary.md");
        assert_eq!(modified.status(), 2);
        assert!(!modified.hunks.is_empty());
    }

    #[test]
    fn a_changed_line_pair_renders_char_level_green_and_red() {
        let f = PatchFile {
            hunks: vec![Hunk {
                lines: vec![
                    hl(' ', "context"),
                    hl('-', "the old word"),
                    hl('+', "the new word"),
                ],
                ..Hunk::default()
            }],
            ..PatchFile::default()
        };
        let rows = file_rows(&f);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].segs.len(), 1);
        assert_eq!(rows[0].segs[0].tone, SegTone::Plain);
        // the merged pair row: "the " plain, "old"→red, "new"→green,
        // " word" plain — order preserved, adjacent same-tone runs merged
        let segs = &rows[1].segs;
        assert!(segs.iter().any(|s| s.tone == SegTone::Removed && s.text.contains("old")));
        assert!(segs.iter().any(|s| s.tone == SegTone::Added && s.text.contains("new")));
        assert!(segs.iter().any(|s| s.tone == SegTone::Plain && s.text.contains("the")));
        assert!(
            segs.iter().all(|s| !s.text.is_empty()),
            "no empty segments: {segs:?}"
        );
    }

    #[test]
    fn unpaired_lines_color_whole_and_hunks_separate() {
        let f = PatchFile {
            hunks: vec![
                Hunk {
                    lines: vec![
                        hl('-', "gone entirely"),
                        hl('-', "also gone"),
                        hl('+', "replacement"),
                    ],
                    ..Hunk::default()
                },
                Hunk {
                    lines: vec![hl('+', "fresh line")],
                    ..Hunk::default()
                },
            ],
            ..PatchFile::default()
        };
        let rows = file_rows(&f);
        // pair(0), excess removed(1), then ⋯, then the added line
        assert_eq!(rows.len(), 4);
        assert!(rows[1].segs.iter().all(|s| s.tone == SegTone::Removed));
        assert_eq!(rows[2].segs[0].tone, SegTone::Meta);
        assert!(rows[3].segs.iter().all(|s| s.tone == SegTone::Added));
    }
}
