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

/// One file of a parsed patch.
#[derive(Clone, Debug, Default)]
pub struct PatchFile {
    pub old_path: String,
    pub new_path: String,
    pub added: bool,
    pub deleted: bool,
    pub renamed: bool,
    pub hunks: Vec<Hunk>,
}

impl PatchFile {
    /// The navigator label: the file as the patch would leave it (the
    /// old path for deletions).
    pub fn display_path(&self) -> &str {
        if self.deleted {
            &self.old_path
        } else {
            &self.new_path
        }
    }

    /// The navigator's extra marker: moved / deleted files say so.
    pub fn marker(&self) -> &'static str {
        if self.deleted {
            "<deleted>"
        } else if self.renamed {
            "<moved>"
        } else {
            ""
        }
    }

    /// Status code in the wiki tone vocabulary: 1 added · 2 modified ·
    /// 3 deleted.
    pub fn status(&self) -> u8 {
        if self.deleted {
            3
        } else if self.added {
            1
        } else {
            2
        }
    }
}

/// One `@@` hunk: its content lines with their op char.
#[derive(Clone, Debug, Default)]
pub struct Hunk {
    /// `' '` context · `'+'` added · `'-'` removed.
    pub lines: Vec<(char, String)>,
}

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

/// Parse a git-format patch into files. Tolerant reader for the shape
/// `wiki::build_patch` emits (`diff --git`, `new/deleted file mode`,
/// `similarity index`, `rename from/to`, `---`/`+++`, `@@`, content
/// lines, `\ No newline` hints); anything unrecognized is skipped.
pub fn parse_patch(patch: &str) -> Vec<PatchFile> {
    let mut files: Vec<PatchFile> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            let mut f = PatchFile::default();
            // paths may contain spaces — split at the LAST " b/", the
            // only separator both sides agree on
            if let Some(pos) = rest.rfind(" b/") {
                f.old_path = rest[..pos].to_string();
                f.new_path = rest[pos + 3..].to_string();
            }
            files.push(f);
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        if line.starts_with("new file mode") {
            f.added = true;
        } else if line.starts_with("deleted file mode") {
            f.deleted = true;
        } else if let Some(p) = line.strip_prefix("rename from ") {
            f.renamed = true;
            f.old_path = p.to_string();
        } else if let Some(p) = line.strip_prefix("rename to ") {
            f.renamed = true;
            f.new_path = p.to_string();
        } else if line.starts_with("@@") {
            f.hunks.push(Hunk::default());
        } else if line.starts_with("similarity index")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with('\\')
        {
            // header noise / no-newline hint — nothing to keep
        } else if let Some(h) = f.hunks.last_mut() {
            let mut chars = line.chars();
            if let Some(op @ ('+' | '-' | ' ')) = chars.next() {
                h.lines.push((op, chars.collect()));
            }
        }
    }
    files
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
            match lines[i].0 {
                ' ' => {
                    out.push(plain_row(&lines[i].1, SegTone::Plain));
                    i += 1;
                }
                '+' => {
                    out.push(plain_row(&lines[i].1, SegTone::Added));
                    i += 1;
                }
                '-' => {
                    // the whole removal run, then the addition run that
                    // follows — pairs char-diff, excess colors whole lines
                    let start = i;
                    while i < lines.len() && lines[i].0 == '-' {
                        i += 1;
                    }
                    let removed: Vec<&str> =
                        lines[start..i].iter().map(|(_, t)| t.as_str()).collect();
                    let astart = i;
                    while i < lines.len() && lines[i].0 == '+' {
                        i += 1;
                    }
                    let added: Vec<&str> =
                        lines[astart..i].iter().map(|(_, t)| t.as_str()).collect();
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
    let diff = TextDiff::from_chars(old, new);
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
        assert_eq!(deleted.status(), 3);
        assert!(!deleted.hunks.is_empty(), "the deletion carries its lines");
        let renamed = by_path("decisions/relay-decision.md");
        assert!(renamed.renamed && !renamed.deleted);
        assert_eq!(renamed.marker(), "<moved>");
        assert_eq!(renamed.old_path, "decisions/2026-06-14-relay.md");
        assert!(renamed.hunks.is_empty(), "a pure rename has no hunks");
        let added = by_path("todo.md");
        assert!(added.added);
        assert_eq!(added.marker(), "");
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
                    (' ', "context".to_string()),
                    ('-', "the old word".to_string()),
                    ('+', "the new word".to_string()),
                ],
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
                        ('-', "gone entirely".to_string()),
                        ('-', "also gone".to_string()),
                        ('+', "replacement".to_string()),
                    ],
                },
                Hunk {
                    lines: vec![('+', "fresh line".to_string())],
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
