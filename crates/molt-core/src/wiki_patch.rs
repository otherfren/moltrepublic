// SPDX-License-Identifier: GPL-3.0-or-later

//! The unified-diff EMITTER — the exact inverse of
//! [`crate::wiki_fold::apply_patch`], which is why it lives beside it
//! (`docs/memory/wiki_semantic_gaps.md` §7 step 5). One emitter for both
//! writers: the GUI's changeset vote and the engine's structured
//! `wiki_edit` produce the same bytes, so members see one diff shape.
//!
//! The strict applier is the arbiter: every patch this emits must apply
//! byte-for-byte at the position it names (`wiki_fold`'s
//! `strictness_position_and_context_must_match_exactly`).
//!
//! Pure — no I/O; `similar` is pure Rust.

use std::collections::{BTreeMap, BTreeSet};

use similar::{DiffOp, TextDiff};

/// The NET change counts a changeset vote carries in its `summary`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatchCounts {
    /// Documents that exist only on the new side.
    pub added: usize,
    /// Documents the patch removes.
    pub deleted: usize,
    /// Documents whose path changed.
    pub moved: usize,
    /// Touched content lines across edited documents.
    pub lines: usize,
}

impl PatchCounts {
    /// The language-neutral summary a proposal card shows, `"+2 -1 →1 ~34"`.
    /// Zero counts are left out; nothing changed reads as the empty string.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for (n, sign) in [
            (self.added, '+'),
            (self.deleted, '-'),
            (self.moved, '→'),
            (self.lines, '~'),
        ] {
            if n > 0 {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push(sign);
                out.push_str(&n.to_string());
            }
        }
        out
    }
}

/// Touched content lines between two texts: inserted + removed, a replaced
/// run counted once at its wider side.
pub fn touched_lines(old: &str, new: &str) -> usize {
    TextDiff::from_lines(old, new)
        .ops()
        .iter()
        .map(|op| match op {
            DiffOp::Insert { new_len, .. } => *new_len,
            DiffOp::Delete { old_len, .. } => *old_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => (*old_len).max(*new_len),
            DiffOp::Equal { .. } => 0,
        })
        .sum()
}

/// The net changeset from `base` to `after` as ONE git-format patch,
/// files sorted by the path they leave behind. `renames` maps a NEW path
/// to the base path it came from — a rename is never guessed from content.
/// `None` when nothing net-changed.
pub fn build_patch(
    base: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    renames: &BTreeMap<String, String>,
) -> Option<String> {
    let changes = changes(base, after, renames);
    if changes.is_empty() {
        return None;
    }
    let mut out = String::new();
    for c in changes {
        match c {
            Change::Added { path, content } => {
                out.push_str(&format!(
                    "diff --git a/{path} b/{path}\nnew file mode 100644\n"
                ));
                out.push_str(&unified("", content, "/dev/null", &format!("b/{path}")));
            }
            Change::Deleted { path, content } => {
                out.push_str(&format!(
                    "diff --git a/{path} b/{path}\ndeleted file mode 100644\n"
                ));
                out.push_str(&unified(content, "", &format!("a/{path}"), "/dev/null"));
            }
            Change::Edited {
                old_path,
                new_path,
                before,
                content,
            } => {
                out.push_str(&format!("diff --git a/{old_path} b/{new_path}\n"));
                if old_path != new_path {
                    if before == content {
                        // git's exact no-content idiom for a pure move
                        out.push_str("similarity index 100%\n");
                    }
                    out.push_str(&format!("rename from {old_path}\nrename to {new_path}\n"));
                }
                if before != content {
                    out.push_str(&unified(
                        before,
                        content,
                        &format!("a/{old_path}"),
                        &format!("b/{new_path}"),
                    ));
                }
            }
        }
    }
    Some(out)
}

/// What [`build_patch`] would emit, counted.
pub fn count_changes(
    base: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    renames: &BTreeMap<String, String>,
) -> PatchCounts {
    let mut c = PatchCounts::default();
    for change in changes(base, after, renames) {
        match change {
            Change::Added { .. } => c.added += 1,
            Change::Deleted { .. } => c.deleted += 1,
            Change::Edited {
                old_path,
                new_path,
                before,
                content,
            } => {
                if old_path != new_path {
                    c.moved += 1;
                }
                if before != content {
                    c.lines += touched_lines(before, content);
                }
            }
        }
    }
    c
}

/// One file of the net changeset.
enum Change<'a> {
    Added { path: &'a str, content: &'a str },
    Deleted { path: &'a str, content: &'a str },
    Edited {
        old_path: &'a str,
        new_path: &'a str,
        before: &'a str,
        content: &'a str,
    },
}

impl Change<'_> {
    /// The path the patch LEAVES behind — the file order, so a deletion
    /// sorts under its old name and everything else under its new one.
    fn key(&self) -> &str {
        match self {
            Change::Added { path, .. } | Change::Deleted { path, .. } => path,
            Change::Edited { new_path, .. } => new_path,
        }
    }
}

/// The net changeset, file-ordered. A base path a rename consumed and the
/// new side occupies again reads as a CREATION (the rename freed it) — the
/// same bytes the GUI emitted before this moved down here. Such a patch
/// names one path twice and the strict applier voids it, which is why the
/// structured write path refuses the combination before it gets here.
fn changes<'a>(
    base: &'a BTreeMap<String, String>,
    after: &'a BTreeMap<String, String>,
    renames: &'a BTreeMap<String, String>,
) -> Vec<Change<'a>> {
    let mut consumed: BTreeSet<&str> = BTreeSet::new();
    for (new_path, old_path) in renames {
        if after.contains_key(new_path) && base.contains_key(old_path) {
            consumed.insert(old_path.as_str());
        }
    }
    let mut out: Vec<Change<'a>> = Vec::new();
    for (path, content) in after {
        let source = renames
            .get(path)
            .and_then(|old| base.get_key_value(old))
            .map(|(old, before)| (old.as_str(), before.as_str()));
        if let Some((old_path, before)) = source {
            if old_path != path || before != content {
                out.push(Change::Edited {
                    old_path,
                    new_path: path,
                    before,
                    content,
                });
            }
            continue;
        }
        match base.get(path) {
            // a base path a rename freed: the new side CREATES it
            Some(_) if consumed.contains(path.as_str()) => {
                out.push(Change::Added { path, content });
            }
            Some(before) if before != content => out.push(Change::Edited {
                old_path: path,
                new_path: path,
                before,
                content,
            }),
            Some(_) => {}
            None => out.push(Change::Added { path, content }),
        }
    }
    for (path, content) in base {
        if after.contains_key(path) || consumed.contains(path.as_str()) {
            continue;
        }
        out.push(Change::Deleted { path, content });
    }
    out.sort_by(|a, b| a.key().cmp(b.key()));
    out
}

/// One file's unified diff with `---`/`+++` names, git hunk format.
fn unified(old: &str, new: &str, a: &str, b: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(a, b)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki_fold;

    fn tree(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(p, c)| ((*p).to_string(), (*c).to_string()))
            .collect()
    }

    fn renames(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        tree(entries)
    }

    /// THE keystone of the emitter: whatever it writes, the STRICT applier
    /// reproduces exactly — every edit kind, the unterminated last line
    /// included. Members ratify the diff; the fold must reach the tree the
    /// writer meant.
    #[test]
    fn every_edit_kind_round_trips_through_the_strict_applier() {
        let base = tree(&[
            ("edit.md", "# One\n\nhello\nworld\n"),
            ("gone.md", "# Gone\n"),
            ("move.md", "# Move\n"),
            ("move-edit.md", "# Move\n\nbody\n"),
            ("tail.md", "# Tail\nno newline"),
        ]);
        let after = tree(&[
            ("edit.md", "# One\n\nhello\nwelt\n"),
            ("moved.md", "# Move\n"),
            ("moved-edit.md", "# Move\n\nbody, edited\n"),
            ("tail.md", "# Tail\nno newline either"),
            ("new.md", "# New\n\nfresh\n"),
            ("empty.md", ""),
        ]);
        let renames = renames(&[("moved.md", "move.md"), ("moved-edit.md", "move-edit.md")]);
        let patch = build_patch(&base, &after, &renames).expect("something changed");
        let mut work = base.clone();
        wiki_fold::apply_patch(&mut work, &wiki_fold::parse_patch(&patch))
            .expect("the emitted patch applies strictly");
        assert_eq!(work, after, "apply_patch(build_patch(x)) == x");
    }

    /// A pure rename carries git's no-content idiom and no hunk at all.
    #[test]
    fn a_pure_rename_emits_the_similarity_idiom() {
        let base = tree(&[("a.md", "# A\n")]);
        let after = tree(&[("b.md", "# A\n")]);
        let patch = build_patch(&base, &after, &renames(&[("b.md", "a.md")])).expect("a rename");
        assert_eq!(
            patch,
            "diff --git a/a.md b/b.md\nsimilarity index 100%\nrename from a.md\nrename to b.md\n"
        );
    }

    /// The file order is the path the patch LEAVES behind, so a deletion
    /// sorts under its old path and everything else under its new one.
    #[test]
    fn files_are_ordered_by_the_path_they_leave_behind() {
        let base = tree(&[("b.md", "# B\n"), ("z.md", "# Z\n")]);
        let after = tree(&[("a.md", "# A\n"), ("z.md", "# Z\n")]);
        let patch = build_patch(&base, &after, &BTreeMap::new()).expect("a change");
        let order: Vec<&str> = patch
            .lines()
            .filter(|l| l.starts_with("diff --git"))
            .collect();
        assert_eq!(
            order,
            ["diff --git a/a.md b/a.md", "diff --git a/b.md b/b.md"]
        );
    }

    /// Nothing net-changed is not an empty patch: there is nothing to vote on.
    #[test]
    fn an_unchanged_tree_has_no_patch() {
        let t = tree(&[("a.md", "# A\n")]);
        assert_eq!(build_patch(&t, &t, &BTreeMap::new()), None);
        // …and a rename back onto itself is not a change either
        assert_eq!(
            build_patch(&t, &t, &renames(&[("a.md", "a.md")])),
            None
        );
    }

    /// The summary the proposal card renders: created, deleted, renamed,
    /// touched lines — a moved-and-edited document counts in both.
    #[test]
    fn the_summary_counts_created_deleted_renamed_and_touched_lines() {
        let base = tree(&[
            ("gone.md", "# Gone\n"),
            ("move.md", "# Move\n\none\n"),
            ("edit.md", "a\nb\n"),
        ]);
        let after = tree(&[
            ("moved.md", "# Move\n\ntwo\n"),
            ("edit.md", "a\nc\n"),
            ("new.md", "# New\n"),
        ]);
        let counts = count_changes(&base, &after, &renames(&[("moved.md", "move.md")]));
        assert_eq!(
            counts,
            PatchCounts {
                added: 1,
                deleted: 1,
                moved: 1,
                lines: 2,
            }
        );
        assert_eq!(counts.summary(), "+1 -1 →1 ~2");
        assert_eq!(PatchCounts::default().summary(), "");
    }
}
