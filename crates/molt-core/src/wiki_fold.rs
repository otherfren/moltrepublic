//! The Shared-Memory base as a deterministic FOLD
//! (`docs/memory/shared_memory_real.md` WP-A): parse the git-format patch
//! a `wiki_patch` proposal carries (the exact shape the wiki's
//! `build_patch` emits) and apply it STRICTLY — exact position AND exact
//! context, whole patch or nothing — over the tree of applied patches in
//! chain order. Any mismatch voids the WHOLE patch, deterministically on
//! every node (same bytes + same predecessor tree → same verdict), which
//! is what lets live state, replay and a checkpoint cut converge on one
//! tree.
//!
//! Deliberately NOT a tolerant apply: `diffy`/GNU-patch-style offset
//! search could bind a hunk to the wrong duplicate region of repeated
//! markdown — members ratified a diff shown at a specific location (the
//! evaluation verdict lives in the plan doc §WP-A).
//!
//! Pure — no I/O, no crypto, std only (this crate's posture).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// One file of a parsed patch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PatchFile {
    /// Path on the old side (`a/…`; the source of a rename).
    pub old_path: String,
    /// Path on the new side (`b/…`; the target of a rename).
    pub new_path: String,
    /// `new file mode` — the file is created.
    pub added: bool,
    /// `deleted file mode` — the file is removed.
    pub deleted: bool,
    /// `rename from`/`rename to` — the file moves (possibly edited too).
    pub renamed: bool,
    /// The `@@` hunks, in patch order.
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

    /// The details-pane header's marker: a move also names where from
    /// and where to (the navigator column stays short).
    pub fn header_marker(&self) -> String {
        if !self.deleted && self.renamed {
            format!("<moved> {} → {}", self.old_path, self.new_path)
        } else {
            self.marker().to_string()
        }
    }

    /// Status code in the wiki tone vocabulary plus the viewer's own
    /// move tone: 1 added · 2 modified · 3 deleted · 4 moved (a renamed
    /// file colors as a move even when it also carries edits — the diff
    /// itself shows those).
    pub fn status(&self) -> u8 {
        if self.deleted {
            3
        } else if self.renamed {
            4
        } else if self.added {
            1
        } else {
            2
        }
    }
}

/// One content line of a hunk: its op, the text (without the newline),
/// and whether the line ends with a newline in its file (`false` only for
/// an unterminated last line — the `\ No newline at end of file` hint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HunkLine {
    /// `' '` context · `'+'` added · `'-'` removed.
    pub op: char,
    /// The line's text, without its newline.
    pub text: String,
    /// Whether the line ends with a newline in its file.
    pub newline: bool,
}

/// One `@@` hunk: the old-side position from the header, and the content
/// lines. `old_start` is 1-based; `old_count == 0` means "insert after
/// line `old_start`" (git's `-l,0` idiom; `-0,0` = the empty file).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based old-side start line (0 only with `old_count == 0`).
    pub old_start: usize,
    /// Old-side line count; 0 = pure insertion after `old_start`.
    pub old_count: usize,
    /// The content lines, in patch order.
    pub lines: Vec<HunkLine>,
}

/// Parse a git-format patch into files. Tolerant reader for the shape
/// the wiki's `build_patch` emits (`diff --git`, `new/deleted file mode`,
/// `similarity index`, `rename from/to`, `---`/`+++`, `@@` with
/// positions, content lines, `\ No newline` hints); anything
/// unrecognized is skipped. Strictness lives in [`apply_patch`], not
/// here — the DIFF VIEWER shares this parser and must render even odd
/// patches.
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
        } else if let Some(head) = line.strip_prefix("@@ ") {
            let (old_start, old_count) = parse_hunk_old(head);
            f.hunks.push(Hunk {
                old_start,
                old_count,
                lines: Vec::new(),
            });
        } else if line.starts_with('\\') {
            // `\ No newline at end of file` — marks the PRECEDING line
            // as the unterminated last line of its side
            if let Some(l) = f.hunks.last_mut().and_then(|h| h.lines.last_mut()) {
                l.newline = false;
            }
        } else if line.starts_with("similarity index")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            // header noise — nothing to keep
        } else if let Some(h) = f.hunks.last_mut() {
            let mut chars = line.chars();
            if let Some(op @ ('+' | '-' | ' ')) = chars.next() {
                h.lines.push(HunkLine {
                    op,
                    text: chars.collect(),
                    newline: true,
                });
            }
        }
    }
    files
}

/// The old side of `-l[,c] +…` (after "@@ "); a missing count means 1.
/// Unparseable headers read as (0, 0) — [`apply_patch`] then voids them.
fn parse_hunk_old(head: &str) -> (usize, usize) {
    let Some(old) = head.split(' ').next().and_then(|s| s.strip_prefix('-')) else {
        return (0, 0);
    };
    match old.split_once(',') {
        Some((s, c)) => (
            s.parse().unwrap_or(0),
            c.parse().unwrap_or(0),
        ),
        None => (old.parse().unwrap_or(0), 1),
    }
}

/// Why a patch does not apply — one honest reason, `Display`-ready.
/// Every variant is a DETERMINISTIC verdict (pure function of tree +
/// patch bytes), which is what the fold's convergence rests on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidReason(
    /// The honest one-line reason.
    pub String,
);

impl std::fmt::Display for VoidReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn void(msg: impl Into<String>) -> VoidReason {
    VoidReason(msg.into())
}

/// A path the tree accepts: non-empty, relative, at most ONE folder
/// level (the wiki's single-level tree), no empty / dot segments.
fn valid_path(p: &str) -> bool {
    if p.is_empty() || p.starts_with('/') || p.ends_with('/') {
        return false;
    }
    let segments: Vec<&str> = p.split('/').collect();
    if segments.len() > 2 {
        return false;
    }
    segments
        .iter()
        .all(|s| !s.is_empty() && *s != "." && *s != ".." && !s.contains('\\'))
}

/// Split into elements that each carry their newline (the last one may
/// not have one); the empty string has NO elements.
fn to_elements(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = s;
    while let Some(pos) = rest.find('\n') {
        out.push(rest[..=pos].to_string());
        rest = &rest[pos + 1..];
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

fn line_element(l: &HunkLine) -> String {
    if l.newline {
        format!("{}\n", l.text)
    } else {
        l.text.clone()
    }
}

/// Apply one file's hunks STRICTLY to `old` (exact position, exact
/// context/removal match — byte-for-byte including the newline shape).
fn apply_hunks(old: &str, hunks: &[Hunk]) -> Result<String, VoidReason> {
    let old: Vec<String> = to_elements(old);
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize; // 0-based into `old`
    for h in hunks {
        // 1-based start; a 0-count hunk INSERTS after line `old_start`
        let start0 = if h.old_count == 0 {
            h.old_start
        } else {
            let Some(s) = h.old_start.checked_sub(1) else {
                return Err(void("hunk position 0 with a non-zero count"));
            };
            s
        };
        if start0 < cursor {
            return Err(void("hunks overlap or run backwards"));
        }
        if start0 > old.len() {
            return Err(void("hunk position beyond the file"));
        }
        out.extend(old[cursor..start0].iter().cloned());
        cursor = start0;
        let mut consumed = 0usize;
        for l in &h.lines {
            let elem = line_element(l);
            match l.op {
                ' ' | '-' => {
                    if old.get(cursor) != Some(&elem) {
                        return Err(void("context does not match the base"));
                    }
                    if l.op == ' ' {
                        out.push(elem);
                    }
                    cursor += 1;
                    consumed += 1;
                }
                '+' => out.push(elem),
                _ => return Err(void("unknown hunk op")),
            }
        }
        // the header's own count must agree — a lying header is a lie
        // about what the members were shown
        if consumed != h.old_count {
            return Err(void("hunk header count disagrees with its lines"));
        }
    }
    out.extend(old[cursor..].iter().cloned());
    Ok(out.concat())
}

/// Apply a WHOLE parsed patch to `tree`, all-or-nothing: on ANY mismatch
/// the tree is untouched and the reason says why (the fold skips the
/// patch — VOID — identically on every node).
pub fn apply_patch(
    tree: &mut BTreeMap<String, String>,
    files: &[PatchFile],
) -> Result<(), VoidReason> {
    if files.is_empty() {
        return Err(void("empty patch"));
    }
    // one patch, one voice per path (the plan's collision rule)
    let mut touched: BTreeSet<&str> = BTreeSet::new();
    for f in files {
        for p in [f.old_path.as_str(), f.new_path.as_str()] {
            if !p.is_empty() && !touched.insert(p) && f.old_path != f.new_path {
                return Err(void(format!("path touched twice: {p}")));
            }
        }
    }
    let mut work = tree.clone();
    for f in files {
        if !valid_path(&f.new_path) && !f.deleted {
            return Err(void(format!("invalid path: {}", f.new_path)));
        }
        if !valid_path(&f.old_path) {
            return Err(void(format!("invalid path: {}", f.old_path)));
        }
        if f.added {
            if work.contains_key(&f.new_path) {
                return Err(void(format!("already exists: {}", f.new_path)));
            }
            let content = apply_hunks("", &f.hunks)?;
            work.insert(f.new_path.clone(), content);
            continue;
        }
        let Some(old) = work.remove(&f.old_path) else {
            return Err(void(format!("missing: {}", f.old_path)));
        };
        if f.deleted {
            let left = apply_hunks(&old, &f.hunks)?;
            if !left.is_empty() {
                return Err(void(format!("deletion leaves content: {}", f.old_path)));
            }
            continue;
        }
        if f.renamed && work.contains_key(&f.new_path) {
            return Err(void(format!("rename target exists: {}", f.new_path)));
        }
        let content = apply_hunks(&old, &f.hunks)?;
        work.insert(f.new_path.clone(), content);
    }
    *tree = work;
    Ok(())
}

/// One applied payload into the tree: `true` when the patch applied,
/// `false` when it was VOID (skipped) — either way deterministic.
pub fn fold_one(tree: &mut BTreeMap<String, String>, payload: &serde_json::Value) -> bool {
    let Some("wiki_patch") = payload.get("op").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(patch) = payload.get("value").and_then(serde_json::Value::as_str) else {
        return false;
    };
    apply_patch(tree, &parse_patch(patch)).is_ok()
}

/// THE base: fold every applied payload in chain order over the empty
/// founding tree. Non-wiki payloads and void patches skip silently — the
/// verdict derives only from chain-ordered bytes, so live state, replay
/// and snapshot+tail all reach the same tree.
pub fn wiki_fold(applied: &[serde_json::Value]) -> BTreeMap<String, String> {
    let mut tree = BTreeMap::new();
    for payload in applied {
        let _ = fold_one(&mut tree, payload);
    }
    tree
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patch_of(files: &str) -> Vec<PatchFile> {
        parse_patch(files)
    }

    const ADD_A: &str = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
    const EDIT_A: &str = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n hello\n-world\n+welt\n";
    const DELETE_A: &str = "diff --git a/a.md b/a.md\ndeleted file mode 100644\n--- a/a.md\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-hello\n-world\n";
    const RENAME_A: &str = "diff --git a/a.md b/b.md\nsimilarity index 100%\nrename from a.md\nrename to b.md\n";

    #[test]
    fn the_happy_chain_folds_add_edit_rename_delete() {
        let mut tree = BTreeMap::new();
        assert!(apply_patch(&mut tree, &patch_of(ADD_A)).is_ok());
        assert_eq!(tree.get("a.md").map(String::as_str), Some("hello\nworld\n"));
        assert!(apply_patch(&mut tree, &patch_of(EDIT_A)).is_ok());
        assert_eq!(tree.get("a.md").map(String::as_str), Some("hello\nwelt\n"));
        let rename_then_delete =
            "diff --git a/a.md b/b.md\nsimilarity index 100%\nrename from a.md\nrename to b.md\n";
        let mut t2 = tree.clone();
        assert!(apply_patch(&mut t2, &patch_of(rename_then_delete)).is_ok());
        assert!(t2.contains_key("b.md") && !t2.contains_key("a.md"));
    }

    #[test]
    fn strictness_position_and_context_must_match_exactly() {
        let mut tree = BTreeMap::from([("a.md".to_string(), "hello\nworld\n".to_string())]);
        // context mismatch → void, tree untouched
        let wrong_ctx = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n hallo\n-world\n+welt\n";
        let before = tree.clone();
        assert!(apply_patch(&mut tree, &patch_of(wrong_ctx)).is_err());
        assert_eq!(tree, before, "a void patch must not touch the tree");
        // RIGHT context at the WRONG position → void (no offset search —
        // the diffy disqualifier, plan §WP-A)
        let wrong_pos = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -2,1 +2,1 @@\n-hello\n+ahoy\n";
        assert!(apply_patch(&mut tree, &patch_of(wrong_pos)).is_err());
        assert_eq!(tree, before);
    }

    #[test]
    fn all_or_nothing_a_second_file_mismatch_voids_the_first_too() {
        let mut tree = BTreeMap::from([("a.md".to_string(), "hello\nworld\n".to_string())]);
        let two = format!("{EDIT_A}diff --git a/missing.md b/missing.md\n--- a/missing.md\n+++ b/missing.md\n@@ -1,1 +1,1 @@\n-x\n+y\n");
        let before = tree.clone();
        assert!(apply_patch(&mut tree, &patch_of(&two)).is_err());
        assert_eq!(tree, before, "file 1's clean apply must roll back");
    }

    #[test]
    fn file_level_guards_add_exists_delete_missing_rename_collision() {
        let mut tree = BTreeMap::from([
            ("a.md".to_string(), "hello\nworld\n".to_string()),
            ("b.md".to_string(), "x\n".to_string()),
        ]);
        let before = tree.clone();
        assert!(apply_patch(&mut tree, &patch_of(ADD_A)).is_err(), "add over existing");
        let del_missing = DELETE_A.replace("a.md", "nope.md");
        assert!(apply_patch(&mut tree, &patch_of(&del_missing)).is_err());
        assert!(apply_patch(&mut tree, &patch_of(RENAME_A)).is_err(), "rename onto b.md");
        assert_eq!(tree, before);
    }

    #[test]
    fn deletion_must_consume_the_whole_file() {
        let mut tree = BTreeMap::from([("a.md".to_string(), "hello\nworld\nrest\n".to_string())]);
        let before = tree.clone();
        assert!(apply_patch(&mut tree, &patch_of(DELETE_A)).is_err());
        assert_eq!(tree, before);
    }

    #[test]
    fn hostile_paths_and_lying_headers_are_void_never_a_panic() {
        let mut tree = BTreeMap::new();
        for bad in [
            ADD_A.replace("a.md", "../a.md"),
            ADD_A.replace("a.md", "x/y/z.md"),
            ADD_A.replace("a.md", "/abs.md"),
            // header claims 1 old line, body has none
            "diff --git a/a.md b/a.md\nnew file mode 100644\n@@ -1,1 +1,1 @@\n+x\n".to_string(),
        ] {
            assert!(apply_patch(&mut tree, &patch_of(&bad)).is_err(), "{bad}");
            assert!(tree.is_empty());
        }
    }

    #[test]
    fn no_newline_hints_round_trip_exactly() {
        // an unterminated file: the hint must survive parse + apply
        let add = "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+tail\n\\ No newline at end of file\n";
        let mut tree = BTreeMap::new();
        assert!(apply_patch(&mut tree, &patch_of(add)).is_ok());
        assert_eq!(tree.get("a.md").map(String::as_str), Some("tail"));
        // …and editing it requires matching that exact unterminated shape
        let edit = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,1 +1,1 @@\n-tail\n\\ No newline at end of file\n+tail\n";
        assert!(apply_patch(&mut tree, &patch_of(edit)).is_ok());
        assert_eq!(tree.get("a.md").map(String::as_str), Some("tail\n"));
    }

    #[test]
    fn the_fold_skips_void_and_foreign_payloads_deterministically() {
        let applied = vec![
            json!({"op": "wiki_patch", "summary": "+1", "value": ADD_A}),
            json!({"op": "set_charter", "value": "not a patch"}),
            json!({"op": "wiki_patch", "value": "garbage, not a patch"}),
            // stale twin of the first — already exists → void
            json!({"op": "wiki_patch", "value": ADD_A}),
            json!({"op": "wiki_patch", "value": EDIT_A}),
        ];
        let tree = wiki_fold(&applied);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get("a.md").map(String::as_str), Some("hello\nwelt\n"));
        // byte-pin: the fold of the same list is bit-identical
        assert_eq!(tree, wiki_fold(&applied));
    }
}
