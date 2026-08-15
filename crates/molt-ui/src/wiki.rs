//! The Multisig-Wiki design mock's client state machine.
//!
//! UX iteration in front of the real build (`docs/ui/mock_todo.md` story 14):
//! the pane's state — doc tree, open tabs, marked row, edit buffers and the
//! base-vs-working CHANGE SET — lives here instead of fixed `.slint` slots,
//! so rename / drag-move / delete-as-pending-change / per-block diffs are
//! expressible and unit-tested. Still a badged mock: no engine commands, no
//! I/O, nothing leaves the process. The change set is the visual ground for
//! the later "bundle all changes into one voted patch" step.
//!
//! Slint sees only flat row models built from [`Wiki`] (bridge in `lib.rs`);
//! this module stays slint-free so the whole state machine tests headless.

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use similar::{capture_diff_slices, Algorithm, DiffOp, TextDiff};

pub type DocId = u32;

/// A document's pending-change state vs the ratified base snapshot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Unchanged,
    Added,
    Modified,
    Deleted,
}

/// One pseudo-rendered markdown block (the pane's render primitive):
/// 0 = h1, 1 = h2+, 2 = paragraph, 3 = bullet, 4 = code.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Block {
    pub kind: u8,
    pub text: String,
    /// The same content as inline runs: plain text, or a `.md`-link run
    /// (target kept) — what makes preview links clickable. Concatenated
    /// span text equals `text`.
    pub spans: Vec<Span>,
}

/// One inline run of a block: `link` is empty for plain text, else the
/// `.md` target the run points at.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Span {
    pub text: String,
    pub link: String,
}

impl Block {
    fn new(kind: u8) -> Self {
        Block {
            kind,
            text: String::new(),
            spans: Vec::new(),
        }
    }

    /// Append a run, merging into the previous span when the link target
    /// matches (keeps plain prose as ONE span).
    fn push_run(&mut self, text: &str, link: &str) {
        if text.is_empty() {
            return;
        }
        self.text.push_str(text);
        match self.spans.last_mut() {
            Some(s) if s.link == link => s.text.push_str(text),
            _ => self.spans.push(Span {
                text: text.to_string(),
                link: link.to_string(),
            }),
        }
    }
}

/// A block's diff verdict in the preview.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockStatus {
    Same,
    Added,
    Changed,
    /// A base block the working copy dropped — rendered as a struck ghost.
    Removed,
}

/// The ratified counterpart of a working doc (mock sample data stands in
/// for the chain-backed base until story 14 lands).
#[derive(Clone, Debug)]
struct BaseDoc {
    path: String,
    raw: String,
}

#[derive(Clone, Debug)]
pub struct Doc {
    pub id: DocId,
    /// Current path, `folder/file.md` or `file.md` (single-level tree).
    pub path: String,
    /// Working markdown source (the edit buffer — persists across switches).
    pub raw: String,
    pub author: String,
    pub ver: String,
    pub when: String,
    base: Option<BaseDoc>,
    deleted: bool,
}

impl Doc {
    pub fn status(&self) -> Status {
        if self.deleted {
            return Status::Deleted;
        }
        match &self.base {
            None => Status::Added,
            Some(b) if b.raw != self.raw || b.path != self.path => Status::Modified,
            Some(_) => Status::Unchanged,
        }
    }

    fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    fn folder(&self) -> Option<&str> {
        self.path.rsplit_once('/').map(|(f, _)| f)
    }
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub name: String,
    pub open: bool,
    /// Created locally (pending change) vs part of the base tree.
    pub added: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Folder,
    File,
}

/// One flat navigator row, in display order.
#[derive(Clone, Debug)]
pub struct NavRow {
    pub kind: RowKind,
    /// File rows only; 0 for folders.
    pub id: DocId,
    /// The folder's name (folder rows), or the file's display name.
    pub label: String,
    pub nested: bool,
    pub open: bool,
    pub marked: bool,
    pub status: Status,
    pub renaming: bool,
}

/// One tab-strip row, in OPEN order (close-right/left and ⬅️/➡️ walk this).
#[derive(Clone, Debug)]
pub struct TabRow {
    pub id: DocId,
    pub label: String,
    pub active: bool,
    pub status: Status,
}

/// One recorded user action — the changeset stack's unit. Carries the
/// before-state its LIFO inverse needs and a display label frozen at
/// record time (an action log narrates what happened, not what is).
#[derive(Clone, Debug)]
enum Change {
    Created { id: DocId, label: String },
    CreatedFolder { name: String },
    Renamed { id: DocId, from: String, label: String },
    Moved { id: DocId, from: String, label: String },
    /// A base-backed doc's pending deletion (the `deleted` flag).
    DeletedDoc { id: DocId, label: String },
    /// A locally added doc removed entirely — the snapshot is the undo.
    DeletedAdded { doc: Doc, label: String },
    /// One coalesced run of keystrokes on a doc.
    Edited { id: DocId, before: String, label: String },
}

/// The kind tag of a [`StackRow`] (drives the panel row's glyph + tone).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Created,
    CreatedFolder,
    Renamed,
    Moved,
    Deleted,
    Edited,
}

/// One changeset-panel stack row, oldest first.
#[derive(Clone, Debug)]
pub struct StackRow {
    pub kind: ChangeKind,
    pub label: String,
}

/// The NET pending change set, recomputed from base vs working state (the
/// stack narrates actions; these numbers are what a vote would seal).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ChangesetCounts {
    /// Files that exist only in the working set.
    pub added: usize,
    /// Base files pending deletion.
    pub deleted: usize,
    /// Base files whose path changed (rename or move).
    pub moved: usize,
    /// Touched content lines across modified base files.
    pub lines: usize,
}

pub struct Wiki {
    docs: Vec<Doc>,
    folders: Vec<Folder>,
    /// Open tabs in the order they were opened.
    tabs: Vec<DocId>,
    active: Option<DocId>,
    marked: Option<DocId>,
    pub editing: bool,
    renaming: Option<DocId>,
    next_id: DocId,
    /// The changeset stack: every user action, oldest first.
    stack: Vec<Change>,
    /// The engine base's revision (count of applied patches) — rides the
    /// vote payload as the display-only staleness hint
    /// (`shared_memory_real.md` §9.1).
    pub base_rev: u64,
}

impl Wiki {
    // ---- construction -----------------------------------------------------

    #[cfg(test)]
    fn from_base(base: &[(&str, &str, &str, &str, &str)]) -> Self {
        let mut docs = Vec::new();
        let mut folders: Vec<Folder> = Vec::new();
        for (i, (path, author, ver, when, raw)) in base.iter().enumerate() {
            docs.push(Doc {
                id: u32::try_from(i).expect("base fits u32") + 1,
                path: (*path).to_string(),
                raw: (*raw).to_string(),
                author: (*author).to_string(),
                ver: (*ver).to_string(),
                when: (*when).to_string(),
                base: Some(BaseDoc {
                    path: (*path).to_string(),
                    raw: (*raw).to_string(),
                }),
                deleted: false,
            });
            if let Some((folder, _)) = path.rsplit_once('/') {
                if !folders.iter().any(|f| f.name == folder) {
                    folders.push(Folder {
                        name: folder.to_string(),
                        open: true,
                        added: false,
                    });
                }
            }
        }
        let next_id = u32::try_from(docs.len()).expect("base fits u32") + 1;
        let first = docs.first().map(|d| d.id);
        Wiki {
            docs,
            folders,
            tabs: first.into_iter().collect(),
            active: first,
            marked: first,
            editing: false,
            renaming: None,
            next_id,
            stack: Vec::new(),
            base_rev: 0,
        }
    }

    /// The production start: NOTHING — the real base arrives from the
    /// engine read ([`Wiki::set_base`]); until then the honest empty
    /// state shows (`shared_memory_real.md`: the founding baseline is the
    /// empty tree).
    pub fn empty() -> Self {
        Wiki {
            docs: Vec::new(),
            folders: Vec::new(),
            tabs: Vec::new(),
            active: None,
            marked: None,
            editing: false,
            renaming: None,
            next_id: 1,
            stack: Vec::new(),
            base_rev: 0,
        }
    }

    /// The folded engine base lands (`shared_memory_real.md` WP-C): every
    /// base doc updates or appears, docs whose base vanished leave —
    /// UNLESS local work sits on them. Local edits are the member's and
    /// are never dropped: an edited doc keeps its raw (recolored against
    /// the new base), an edited doc whose base vanished turns Added. Ids
    /// stay stable (nav marks, tabs and the undo stack hang off them).
    pub fn set_base(&mut self, base: &[(String, String)], rev: u64) {
        self.base_rev = rev;
        for (path, content) in base {
            if let Some(d) = self
                .docs
                .iter_mut()
                .find(|d| d.base.as_ref().is_some_and(|b| &b.path == path))
            {
                let unedited = d.status() == Status::Unchanged;
                d.base = Some(BaseDoc {
                    path: path.clone(),
                    raw: content.clone(),
                });
                if unedited {
                    // no local work — follow the base (path and content)
                    d.path = path.clone();
                    d.raw = content.clone();
                    d.deleted = false;
                }
                continue;
            }
            // hands off a LOCAL draft occupying the same path: the draft
            // stays visible as Added next to nothing — the member decides
            // (rescue/rework), the base doc appears once the paths differ.
            // Deliberately NOT a silent overwrite of local work.
            if self.docs.iter().any(|d| d.path == *path && d.base.is_none()) {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            self.docs.push(Doc {
                id,
                path: path.clone(),
                raw: content.clone(),
                author: String::new(),
                ver: String::new(),
                when: String::new(),
                base: Some(BaseDoc {
                    path: path.clone(),
                    raw: content.clone(),
                }),
                deleted: false,
            });
        }
        // docs whose base vanished: unedited ones leave (tabs closed),
        // edited ones stay as ADDED — their content is new against this
        // base and must not be lost
        let gone: Vec<(DocId, bool)> = self
            .docs
            .iter()
            .filter(|d| {
                d.base
                    .as_ref()
                    .is_some_and(|b| !base.iter().any(|(p, _)| p == &b.path))
            })
            .map(|d| (d.id, d.status() == Status::Unchanged || d.deleted))
            .collect();
        for (id, remove) in gone {
            if remove {
                self.close_tab(id);
                if self.marked == Some(id) {
                    self.marked = None;
                }
                self.docs.retain(|d| d.id != id);
            } else if let Some(d) = self.doc_mut(id) {
                d.base = None; // edited content survives as Added
            }
        }
        // folders follow the surviving docs
        for (path, _) in base {
            if let Some((folder, _)) = path.rsplit_once('/') {
                if !self.folders.iter().any(|f| f.name == folder) {
                    self.folders.push(Folder {
                        name: folder.to_string(),
                        open: true,
                        added: false,
                    });
                }
            }
        }
        self.folders.retain(|f| {
            f.added || self.docs.iter().any(|d| d.path.starts_with(&format!("{}/", f.name)))
        });
    }

    /// The sample base — a TEST FIXTURE only since the real fold landed
    /// (production starts [`Wiki::empty`] and rebases on the engine read).
    #[cfg(test)]
    pub fn sample() -> Self {
        Wiki::from_base(&[
            ("charter.md", "petra", "v12", "3 d",
             "# Charter\n\nWhat we agreed to at the founding - the purpose, the rule, and the habits that keep four people one republic.\n\n## Purpose\n\nPool funds, share knowledge and act as one signer where a single person would be a single point of failure.\n\n## Consensus\n\n- Every gated change needs 3 of 4 signatures - no exceptions, not even for the founder.\n- Proposals live in chat until sealed; only sealed changes are canon.\n\n## Habits\n\n- Quarterly recovery drill (see [runbooks](runbooks/node-recovery.md)).\n- Decisions get a dated file under decisions/ - if it is not written down, it was not decided.\n\nSee also [treasury](decisions/2026-07-02-treasury.md) and [glossary](glossary.md)."),
            ("decisions/2026-06-14-relay.md", "walter", "v4", "1 m",
             "# Decision: paid SMP relay\n\nFree relays kept dropping our queues under load. We now rent a dedicated relay and pin its certificate.\n\n## Options weighed\n\n- Stay on public relays - free, but no uptime promise.\n- Self-host - full control, but one member becomes ops.\n- Rent + cert-pin - chosen: predictable, and no single admin.\n\n```\nrelay = \"smp://u2Fq…@relay.example:5223\"  # pinned, see vault\n```\n\nRevisit when the second-relay quest lands (see [recovery](runbooks/node-recovery.md))."),
            ("decisions/2026-07-02-treasury.md", "mara", "v3", "2 w",
             "# Decision: treasury policy\n\nRules for spending from the shared wallet, sealed 3-of-4 on 2026-07-02 (see [charter](charter.md)).\n\n- Transfers above 5 XMR need a written purpose in the proposal text.\n- Recurring costs (relay, domain) are pre-approved up to 3 XMR per quarter.\n- Quest rewards are paid out only after the claim is marked done."),
            ("quests/onboarding-guide.md", "petra", "v9", "5 d",
             "# Onboarding guide\n\nThe path from invite link to first signed proposal, for the next member.\n\n- Read [charter.md](charter.md) first - it is short on purpose.\n- Work through the MLS & SMP reading list, messaging layer first.\n- Run the recovery drill once with a buddy before you hold real shares (see [glossary](glossary.md), [recovery](runbooks/node-recovery.md))."),
            ("runbooks/node-recovery.md", "walter", "v6", "1 w",
             "# Node recovery\n\nTotal-loss restore, tested at every quarterly drill. Budget an hour.\n\n- Restore from your seed phrase - it derives every key you had.\n- Re-adopt the queue credentials from the last clean-close backup.\n- Verify the chain before your first send; a red check means stop (see [charter](charter.md)).\n\n```\ndrill log 2026-06: petra 41 min · jonas 58 min (stuck at re-adopt)\n```"),
            ("glossary.md", "mara", "v17", "1 d",
             "# Glossary\n\nThe words we keep using, pinned down once (see [charter](charter.md)).\n\n- Ritual - the founding ceremony that seals the roster; nothing touches disk before the seal.\n- Seal - the moment a proposal reaches the threshold and becomes canon.\n- Threshold - our 3-of-4 rule: three signatures make a change real.\n- Patch - one gated change on the chain, like a commit."),
        ])
    }

    // ---- lookups ----------------------------------------------------------

    fn doc(&self, id: DocId) -> Option<&Doc> {
        self.docs.iter().find(|d| d.id == id)
    }

    fn doc_mut(&mut self, id: DocId) -> Option<&mut Doc> {
        self.docs.iter_mut().find(|d| d.id == id)
    }

    pub fn active(&self) -> Option<&Doc> {
        self.active.and_then(|id| self.doc(id))
    }

    pub fn marked(&self) -> Option<DocId> {
        self.marked
    }

    pub fn active_id(&self) -> Option<DocId> {
        self.active
    }

    /// (added, modified, deleted) doc counts — the pending change set
    /// (test seam; the UI reads [`Wiki::changeset_counts`]).
    #[cfg(test)]
    pub fn change_counts(&self) -> (usize, usize, usize) {
        let mut c = (0, 0, 0);
        for d in &self.docs {
            match d.status() {
                Status::Added => c.0 += 1,
                Status::Modified => c.1 += 1,
                Status::Deleted => c.2 += 1,
                Status::Unchanged => {}
            }
        }
        c
    }

    // ---- navigator rows ---------------------------------------------------

    /// Display order: folders alphabetically, each with its files
    /// alphabetically (when open), then root files alphabetically. Deleted
    /// files stay listed (struck) — they are pending changes, not gone.
    pub fn nav_rows(&self) -> Vec<NavRow> {
        let mut rows = Vec::new();
        let mut folders: Vec<&Folder> = self.folders.iter().collect();
        folders.sort_by(|a, b| a.name.cmp(&b.name));
        for f in folders {
            rows.push(NavRow {
                kind: RowKind::Folder,
                id: 0,
                label: f.name.clone(),
                nested: false,
                open: f.open,
                marked: false,
                status: if f.added { Status::Added } else { Status::Unchanged },
                renaming: false,
            });
            if f.open {
                for d in self.files_in(Some(&f.name)) {
                    rows.push(self.file_row(d, true));
                }
            }
        }
        for d in self.files_in(None) {
            rows.push(self.file_row(d, false));
        }
        rows
    }

    fn files_in(&self, folder: Option<&str>) -> Vec<&Doc> {
        let mut files: Vec<&Doc> = self
            .docs
            .iter()
            .filter(|d| d.folder() == folder)
            .collect();
        files.sort_by(|a, b| a.name().cmp(b.name()));
        files
    }

    fn file_row(&self, d: &Doc, nested: bool) -> NavRow {
        NavRow {
            kind: RowKind::File,
            id: d.id,
            label: d.name().to_string(),
            nested,
            open: false,
            marked: self.marked == Some(d.id),
            status: d.status(),
            renaming: self.renaming == Some(d.id),
        }
    }

    // ---- tabs -------------------------------------------------------------

    pub fn tab_rows(&self) -> Vec<TabRow> {
        self.tabs
            .iter()
            .filter_map(|id| self.doc(*id))
            .map(|d| TabRow {
                id: d.id,
                label: d.name().to_string(),
                active: self.active == Some(d.id),
                status: d.status(),
            })
            .collect()
    }

    /// Focus an open tab (tab-strip click). No tab is opened or marked.
    pub fn focus(&mut self, id: DocId) {
        if self.tabs.contains(&id) {
            self.active = Some(id);
            self.editing = false;
            self.renaming = None;
        }
    }

    /// Close one tab. Closing the active one focuses its RIGHT neighbor in
    /// the strip, else the left one, else the empty state.
    pub fn close_tab(&mut self, id: DocId) {
        let Some(pos) = self.tabs.iter().position(|t| *t == id) else {
            return;
        };
        self.tabs.remove(pos);
        if self.active == Some(id) {
            self.active = self
                .tabs
                .get(pos)
                .or_else(|| self.tabs.get(pos.wrapping_sub(1)))
                .copied();
            self.editing = false;
        }
    }

    /// Ctrl+W.
    pub fn close_active(&mut self) {
        if let Some(id) = self.active {
            self.close_tab(id);
        }
    }

    pub fn close_all(&mut self) {
        self.tabs.clear();
        self.active = None;
        self.editing = false;
    }

    /// Close every tab RIGHT of `id` in the strip. The active tab moves to
    /// `id` if it was among the closed.
    pub fn close_right_of(&mut self, id: DocId) {
        let Some(pos) = self.tabs.iter().position(|t| *t == id) else {
            return;
        };
        let closed: Vec<DocId> = self.tabs.split_off(pos + 1);
        if self.active.is_some_and(|a| closed.contains(&a)) {
            self.active = Some(id);
            self.editing = false;
        }
    }

    /// Close every tab LEFT of `id` in the strip.
    pub fn close_left_of(&mut self, id: DocId) {
        let Some(pos) = self.tabs.iter().position(|t| *t == id) else {
            return;
        };
        let closed: Vec<DocId> = self.tabs.drain(..pos).collect();
        if self.active.is_some_and(|a| closed.contains(&a)) {
            self.active = Some(id);
            self.editing = false;
        }
    }

    /// ⬅️/➡️ — walk the OPEN TABS in strip order, wrapping. Leaves the
    /// navigator mark behind (that is what arms 🎯).
    pub fn step_tab(&mut self, delta: i32) {
        let Some(a) = self.active else { return };
        let Some(pos) = self.tabs.iter().position(|t| *t == a) else {
            return;
        };
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        let next = (pos as i64 + i64::from(delta)).rem_euclid(n as i64) as usize;
        self.active = Some(self.tabs[next]);
        self.editing = false;
    }

    // ---- navigator actions ------------------------------------------------

    /// Single click: MARK only — nothing opens.
    pub fn mark(&mut self, id: DocId) {
        if self.doc(id).is_some() {
            self.marked = Some(id);
            self.renaming = None;
        }
    }

    /// Open route (double-click / Enter / menu-Open): mark + open as tab +
    /// focus. A deleted doc no longer opens.
    pub fn open(&mut self, id: DocId) {
        let Some(d) = self.doc(id) else { return };
        if d.deleted {
            return;
        }
        self.marked = Some(id);
        if !self.tabs.contains(&id) {
            self.tabs.push(id);
        }
        self.active = Some(id);
        self.editing = false;
        self.renaming = None;
    }

    /// RESCUE a declined/superseded proposal's patch into the local
    /// working copy (`shared_memory_real.md` §4): best-effort PER FILE —
    /// strict within each file (local work is never clobbered; a file
    /// whose base diverged is skipped), honest about the rest. Records
    /// real stack entries via the normal verbs, so the changeset panel
    /// shows and the next vote carries the rescued work. Returns
    /// `(applied, skipped)` file counts.
    pub fn rescue_patch(&mut self, patch: &str) -> (usize, usize) {
        let files = molt_core::wiki_fold::parse_patch(patch);
        let mut applied = 0usize;
        let mut skipped = 0usize;
        for f in &files {
            if self.rescue_file(f) {
                applied += 1;
            } else {
                skipped += 1;
            }
        }
        (applied, skipped)
    }

    fn rescue_file(&mut self, f: &molt_core::wiki_fold::PatchFile) -> bool {
        use molt_core::wiki_fold::apply_hunks;
        if f.added {
            // a fresh file: only onto a free path
            if self.docs.iter().any(|d| d.path == f.new_path && !d.deleted) {
                return false;
            }
            let Ok(content) = apply_hunks("", &f.hunks) else {
                return false;
            };
            let id = self.next_id;
            self.next_id += 1;
            let label = f.new_path.rsplit('/').next().unwrap_or(&f.new_path).to_string();
            if let Some((folder, _)) = f.new_path.rsplit_once('/') {
                if !self.folders.iter().any(|fl| fl.name == folder) {
                    self.folders.push(Folder {
                        name: folder.to_string(),
                        open: true,
                        added: true,
                    });
                }
            }
            self.docs.push(Doc {
                id,
                path: f.new_path.clone(),
                raw: content,
                author: "you".to_string(),
                ver: "draft".to_string(),
                when: "now".to_string(),
                base: None,
                deleted: false,
            });
            self.stack.push(Change::Created { id, label });
            return true;
        }
        let Some(d) = self
            .docs
            .iter()
            .find(|d| d.path == f.old_path && !d.deleted)
        else {
            return false;
        };
        let id = d.id;
        if f.deleted {
            // delete only what matches the patch exactly — a diverged or
            // edited file is the member's, not the patch's, to remove
            let matches = apply_hunks(&d.raw, &f.hunks).is_ok_and(|left| left.is_empty());
            if !matches {
                return false;
            }
            self.delete(id);
            return true;
        }
        let Ok(content) = apply_hunks(&d.raw, &f.hunks) else {
            return false;
        };
        if content != d.raw {
            self.set_raw(id, &content);
        }
        if f.renamed && f.new_path != f.old_path {
            // renames go through the normal verb (same-folder only — the
            // wiki's rename edits the basename; a cross-folder move that
            // no longer fits is the member's call to redo by hand)
            let same_folder = f.old_path.rsplit_once('/').map(|(a, _)| a)
                == f.new_path.rsplit_once('/').map(|(a, _)| a);
            let free = !self.docs.iter().any(|d| d.path == f.new_path && !d.deleted);
            if same_folder && free {
                let name = f
                    .new_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&f.new_path)
                    .trim_end_matches(".md");
                let _ = self.rename_commit(id, name);
            }
        }
        true
    }

    /// Open the doc a preview link names: exact path first, then the
    /// unique basename (hand-written links often skip the folder).
    /// Deleted docs and unknown or ambiguous targets stay put; `false`
    /// reports the miss.
    pub fn open_link(&mut self, target: &str) -> bool {
        let t = target.trim_start_matches("./");
        let id = match self.docs.iter().find(|d| !d.deleted && d.path == t) {
            Some(d) => Some(d.id),
            None => {
                let mut hits = self.docs.iter().filter(|d| !d.deleted && d.name() == t);
                match (hits.next(), hits.next()) {
                    (Some(d), None) => Some(d.id),
                    _ => None,
                }
            }
        };
        let Some(id) = id else { return false };
        self.open(id);
        true
    }

    /// 🎯 — re-align the navigator with the active tab: mark it and unfold
    /// its folder.
    pub fn reveal(&mut self) {
        let Some(a) = self.active else { return };
        let folder = self.doc(a).and_then(|d| d.folder().map(String::from));
        if let Some(f) = folder {
            self.set_folder_open(&f, true);
        }
        self.marked = Some(a);
    }

    pub fn toggle_folder(&mut self, name: &str) {
        if let Some(f) = self.folders.iter_mut().find(|f| f.name == name) {
            f.open = !f.open;
        }
    }

    fn set_folder_open(&mut self, name: &str, open: bool) {
        if let Some(f) = self.folders.iter_mut().find(|f| f.name == name) {
            f.open = open;
        }
    }

    pub fn set_all_folders(&mut self, open: bool) {
        for f in &mut self.folders {
            f.open = open;
        }
    }

    /// New file: `untitled-N.md` (first free N), created next to the marked
    /// file's folder (root otherwise), opened and put into rename mode.
    pub fn new_file(&mut self) -> DocId {
        let folder = self
            .marked
            .and_then(|m| self.doc(m))
            .and_then(|d| d.folder().map(String::from));
        let mut n = 1;
        let name = loop {
            let candidate = format!("untitled-{n}.md");
            let path = match &folder {
                Some(f) => format!("{f}/{candidate}"),
                None => candidate.clone(),
            };
            if !self.docs.iter().any(|d| d.path == path) {
                break path;
            }
            n += 1;
        };
        let id = self.next_id;
        self.next_id += 1;
        let title = name
            .rsplit('/')
            .next()
            .unwrap_or(&name)
            .trim_end_matches(".md")
            .to_string();
        let label = name.rsplit('/').next().unwrap_or(&name).to_string();
        self.docs.push(Doc {
            id,
            path: name,
            raw: format!("# {title}\n\nA fresh page. Use Edit to write it."),
            author: "you".to_string(),
            ver: "draft".to_string(),
            when: "now".to_string(),
            base: None,
            deleted: false,
        });
        self.stack.push(Change::Created { id, label });
        self.open(id);
        self.renaming = Some(id);
        id
    }

    /// New folder: `new-folder-N`, a local pending change.
    pub fn new_folder(&mut self) -> String {
        let mut n = 1;
        let name = loop {
            let candidate = format!("new-folder-{n}");
            if !self.folders.iter().any(|f| f.name == candidate) {
                break candidate;
            }
            n += 1;
        };
        self.folders.push(Folder {
            name: name.clone(),
            open: true,
            added: true,
        });
        self.stack.push(Change::CreatedFolder { name: name.clone() });
        name
    }

    pub fn renaming(&self) -> Option<DocId> {
        self.renaming
    }

    pub fn rename_start(&mut self, id: DocId) {
        if self.doc(id).is_some_and(|d| !d.deleted) {
            self.renaming = Some(id);
            self.marked = Some(id);
        }
    }

    pub fn rename_cancel(&mut self) {
        self.renaming = None;
    }

    /// Commit a rename of the file's NAME (folder stays). Empty names and
    /// path collisions are refused; `.md` is appended when missing.
    pub fn rename_commit(&mut self, id: DocId, new_name: &str) -> Result<(), String> {
        self.renaming = None;
        let name = new_name.trim().trim_matches('/');
        if name.is_empty() {
            return Err("empty name".to_string());
        }
        let name = if name.ends_with(".md") {
            name.to_string()
        } else {
            format!("{name}.md")
        };
        let Some(d) = self.doc(id) else {
            return Err("unknown file".to_string());
        };
        let path = match d.folder() {
            Some(f) => format!("{f}/{name}"),
            None => name,
        };
        if self.docs.iter().any(|o| o.id != id && o.path == path) {
            return Err("name already taken".to_string());
        }
        if let Some(d) = self.doc_mut(id) {
            if d.path != path {
                let from = d.path.clone();
                let from_name = from.rsplit('/').next().unwrap_or(&from).to_string();
                let to_name = path.rsplit('/').next().unwrap_or(&path).to_string();
                d.path = path;
                self.stack.push(Change::Renamed {
                    id,
                    from,
                    label: format!("{from_name} → {to_name}"),
                });
            }
        }
        Ok(())
    }

    /// Drag-move a file into `folder` (`None` = root). Same-name collisions
    /// are refused; deleted files do not move.
    pub fn move_to(&mut self, id: DocId, folder: Option<&str>) -> Result<(), String> {
        let Some(d) = self.doc(id) else {
            return Err("unknown file".to_string());
        };
        if d.deleted {
            return Err("deleted".to_string());
        }
        if let Some(f) = folder {
            if !self.folders.iter().any(|x| x.name == f) {
                return Err("unknown folder".to_string());
            }
        }
        let name = d.name().to_string();
        let path = match folder {
            Some(f) => format!("{f}/{name}"),
            None => name.clone(),
        };
        if self.docs.iter().any(|o| o.id != id && o.path == path) {
            return Err("name already taken".to_string());
        }
        let target = match folder {
            Some(f) => format!("{f}/"),
            None => "/".to_string(),
        };
        if let Some(d) = self.doc_mut(id) {
            if d.path != path {
                let from = d.path.clone();
                d.path = path;
                self.stack.push(Change::Moved {
                    id,
                    from,
                    label: format!("{name} → {target}"),
                });
            }
        }
        Ok(())
    }

    /// Drop a dragged file onto navigator row `row_idx` (the release
    /// target): a folder row moves the file INTO that folder, a file row
    /// moves it NEXT TO that file, past the list end means root. The
    /// pane's drag only measures rows; the semantics live here.
    pub fn drop_on_row(&mut self, id: DocId, row_idx: usize) -> Result<(), String> {
        let rows = self.nav_rows();
        let folder = match rows.get(row_idx) {
            Some(r) if r.kind == RowKind::Folder => Some(r.label.clone()),
            Some(r) => self
                .doc(r.id)
                .and_then(|d| d.folder().map(String::from)),
            None => None,
        };
        self.move_to(id, folder.as_deref())
    }

    /// Delete (toolbar 🗑️ / Del / menu): a base-backed doc becomes a RED
    /// pending deletion (visible, struck, tab closed); a locally added doc
    /// vanishes entirely — there is nothing to vote on.
    pub fn delete(&mut self, id: DocId) {
        let Some(d) = self.doc(id) else { return };
        if d.deleted {
            return; // already a pending deletion
        }
        let is_added = d.base.is_none();
        let label = d.name().to_string();
        let snapshot = d.clone();
        self.close_tab(id);
        if is_added {
            self.docs.retain(|d| d.id != id);
            if self.marked == Some(id) {
                self.marked = None;
            }
            self.stack.push(Change::DeletedAdded { doc: snapshot, label });
        } else {
            if let Some(d) = self.doc_mut(id) {
                d.deleted = true;
            }
            self.stack.push(Change::DeletedDoc { id, label });
        }
        if self.renaming == Some(id) {
            self.renaming = None;
        }
    }

    // ---- editing + diff ---------------------------------------------------

    /// The raw editor's buffer flows here on every edit; reverting to the
    /// base bytes turns the doc Unchanged again. One RUN of keystrokes on a
    /// doc coalesces into a single stack entry (its `before` is the state
    /// the run started from); switching docs starts a new run.
    pub fn set_raw(&mut self, id: DocId, raw: &str) {
        let Some(d) = self.doc(id) else { return };
        if d.deleted || d.raw == raw {
            return;
        }
        let coalesce =
            matches!(self.stack.last(), Some(Change::Edited { id: last, .. }) if *last == id);
        if !coalesce {
            self.stack.push(Change::Edited {
                id,
                before: d.raw.clone(),
                label: d.name().to_string(),
            });
        }
        if let Some(d) = self.doc_mut(id) {
            d.raw = raw.to_string();
        }
    }

    /// The active doc's preview: working blocks with their diff verdicts,
    /// plus struck ghosts for base blocks the working copy dropped.
    pub fn preview(&self, id: DocId) -> Vec<(Block, BlockStatus)> {
        let Some(d) = self.doc(id) else {
            return Vec::new();
        };
        let work = parse_blocks(&d.raw);
        let Some(base) = &d.base else {
            return work.into_iter().map(|b| (b, BlockStatus::Added)).collect();
        };
        let old = parse_blocks(&base.raw);
        let mut out = Vec::new();
        for op in capture_diff_slices(Algorithm::Myers, &old, &work) {
            match op {
                DiffOp::Equal { new_index, len, .. } => {
                    for b in &work[new_index..new_index + len] {
                        out.push((b.clone(), BlockStatus::Same));
                    }
                }
                DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    for b in &work[new_index..new_index + new_len] {
                        out.push((b.clone(), BlockStatus::Added));
                    }
                }
                DiffOp::Delete {
                    old_index, old_len, ..
                } => {
                    for b in &old[old_index..old_index + old_len] {
                        out.push((b.clone(), BlockStatus::Removed));
                    }
                }
                DiffOp::Replace {
                    old_index,
                    old_len,
                    new_index,
                    new_len,
                } => {
                    // pair old/new positionally: pairs render as Changed,
                    // excess old blocks stay visible as Removed ghosts,
                    // excess new ones are Added — a lopsided replace must
                    // not swallow a deletion
                    for i in 0..new_len.max(old_len) {
                        if i < new_len && i < old_len {
                            out.push((work[new_index + i].clone(), BlockStatus::Changed));
                        } else if i < new_len {
                            out.push((work[new_index + i].clone(), BlockStatus::Added));
                        } else {
                            out.push((old[old_index + i].clone(), BlockStatus::Removed));
                        }
                    }
                }
            }
        }
        out
    }

    /// The active doc's cross-links: `.md` link targets in order of first
    /// appearance.
    pub fn links(&self, id: DocId) -> Vec<String> {
        self.doc(id).map(|d| parse_links(&d.raw)).unwrap_or_default()
    }

    // ---- the changeset stack ----------------------------------------------

    /// Recorded actions on the stack (test seam; the panel's visibility
    /// reads the synced `cs-rows` model length).
    #[cfg(test)]
    pub fn stack_len(&self) -> usize {
        self.stack.len()
    }

    /// The panel's action list, oldest first.
    pub fn stack_rows(&self) -> Vec<StackRow> {
        self.stack
            .iter()
            .map(|c| match c {
                Change::Created { label, .. } => StackRow {
                    kind: ChangeKind::Created,
                    label: label.clone(),
                },
                Change::CreatedFolder { name } => StackRow {
                    kind: ChangeKind::CreatedFolder,
                    label: format!("{name}/"),
                },
                Change::Renamed { label, .. } => StackRow {
                    kind: ChangeKind::Renamed,
                    label: label.clone(),
                },
                Change::Moved { label, .. } => StackRow {
                    kind: ChangeKind::Moved,
                    label: label.clone(),
                },
                Change::DeletedDoc { label, .. } | Change::DeletedAdded { label, .. } => StackRow {
                    kind: ChangeKind::Deleted,
                    label: label.clone(),
                },
                Change::Edited { label, .. } => StackRow {
                    kind: ChangeKind::Edited,
                    label: label.clone(),
                },
            })
            .collect()
    }

    /// Undo the latest action (LIFO). Refuses when the inverse would
    /// collide with the current tree — possible only after a per-file
    /// revert re-occupied a path mid-stack; the entry then stays put.
    pub fn undo(&mut self) -> Result<(), String> {
        let Some(change) = self.stack.last().cloned() else {
            return Err("nothing to undo".to_string());
        };
        match change {
            Change::Created { id, .. } => {
                self.close_tab(id);
                self.docs.retain(|d| d.id != id);
                if self.marked == Some(id) {
                    self.marked = None;
                }
                if self.renaming == Some(id) {
                    self.renaming = None;
                }
            }
            Change::CreatedFolder { ref name } => {
                if self.docs.iter().any(|d| d.folder() == Some(name.as_str())) {
                    return Err("folder not empty".to_string());
                }
                self.folders.retain(|f| !(f.added && f.name == *name));
            }
            Change::Renamed { id, ref from, .. } | Change::Moved { id, ref from, .. } => {
                if self.docs.iter().any(|o| o.id != id && o.path == *from) {
                    return Err("name already taken".to_string());
                }
                if let Some(d) = self.doc_mut(id) {
                    d.path = from.clone();
                }
            }
            Change::DeletedDoc { id, .. } => {
                if let Some(d) = self.doc_mut(id) {
                    d.deleted = false;
                }
            }
            Change::DeletedAdded { ref doc, .. } => {
                if self.docs.iter().any(|o| o.path == doc.path) {
                    return Err("name already taken".to_string());
                }
                self.docs.push(doc.clone());
            }
            Change::Edited { id, ref before, .. } => {
                if let Some(d) = self.doc_mut(id) {
                    d.raw = before.clone();
                }
            }
        }
        self.stack.pop();
        Ok(())
    }

    /// Drop the whole changeset: every doc back to base, added docs and
    /// folders gone, stack cleared.
    pub fn revert_all(&mut self) {
        let added: Vec<DocId> = self
            .docs
            .iter()
            .filter(|d| d.base.is_none())
            .map(|d| d.id)
            .collect();
        for id in added {
            self.close_tab(id);
            if self.marked == Some(id) {
                self.marked = None;
            }
            if self.renaming == Some(id) {
                self.renaming = None;
            }
        }
        self.docs.retain(|d| d.base.is_some());
        for d in &mut self.docs {
            if let Some(b) = &d.base {
                d.path = b.path.clone();
                d.raw = b.raw.clone();
            }
            d.deleted = false;
        }
        self.folders.retain(|f| !f.added);
        self.stack.clear();
    }

    /// Revert ONE base-backed doc to its base state (content, path,
    /// deletion) and drop its stack entries; the rest of the stack stays.
    pub fn revert_doc(&mut self, id: DocId) -> Result<(), String> {
        let Some(d) = self.doc(id) else {
            return Err("unknown file".to_string());
        };
        let Some(base) = d.base.clone() else {
            return Err("a draft has no base".to_string());
        };
        if self.docs.iter().any(|o| o.id != id && o.path == base.path) {
            return Err("name already taken".to_string());
        }
        if let Some(d) = self.doc_mut(id) {
            d.path = base.path;
            d.raw = base.raw;
            d.deleted = false;
        }
        self.stack.retain(|c| {
            !matches!(c,
                Change::Renamed { id: cid, .. }
                | Change::Moved { id: cid, .. }
                | Change::DeletedDoc { id: cid, .. }
                | Change::Edited { id: cid, .. } if *cid == id)
        });
        Ok(())
    }

    /// Folder names in display order (test seam).
    #[cfg(test)]
    pub fn folders_named(&self) -> Vec<String> {
        self.folders.iter().map(|f| f.name.clone()).collect()
    }

    /// The NET change summary the panel shows.
    pub fn changeset_counts(&self) -> ChangesetCounts {
        let mut c = ChangesetCounts::default();
        for d in &self.docs {
            match (&d.base, d.deleted) {
                (None, _) => c.added += 1,
                (Some(_), true) => c.deleted += 1,
                (Some(b), false) => {
                    if b.path != d.path {
                        c.moved += 1;
                    }
                    if b.raw != d.raw {
                        c.lines += touched_lines(&b.raw, &d.raw);
                    }
                }
            }
        }
        c
    }

    /// The net changeset as ONE git-format patch (unified diffs + git
    /// rename/new/deleted headers), files sorted by display path.
    /// `None` when nothing net-changed.
    pub fn build_patch(&self) -> Option<String> {
        let mut changed: Vec<&Doc> = self
            .docs
            .iter()
            .filter(|d| d.status() != Status::Unchanged)
            .collect();
        if changed.is_empty() {
            return None;
        }
        changed.sort_by_key(|d| match (&d.base, d.deleted) {
            (Some(b), true) => b.path.clone(),
            _ => d.path.clone(),
        });
        let mut out = String::new();
        for d in changed {
            match (&d.base, d.deleted) {
                (None, _) => {
                    out.push_str(&format!(
                        "diff --git a/{p} b/{p}\nnew file mode 100644\n",
                        p = d.path
                    ));
                    out.push_str(&unified("", &d.raw, "/dev/null", &format!("b/{}", d.path)));
                }
                (Some(b), true) => {
                    out.push_str(&format!(
                        "diff --git a/{p} b/{p}\ndeleted file mode 100644\n",
                        p = b.path
                    ));
                    out.push_str(&unified(&b.raw, "", &format!("a/{}", b.path), "/dev/null"));
                }
                (Some(b), false) => {
                    let renamed = b.path != d.path;
                    let edited = b.raw != d.raw;
                    if renamed {
                        out.push_str(&format!("diff --git a/{} b/{}\n", b.path, d.path));
                        if !edited {
                            // pure rename — git's exact no-content idiom
                            out.push_str("similarity index 100%\n");
                        }
                        out.push_str(&format!(
                            "rename from {}\nrename to {}\n",
                            b.path, d.path
                        ));
                    } else {
                        out.push_str(&format!("diff --git a/{p} b/{p}\n", p = d.path));
                    }
                    if edited {
                        out.push_str(&unified(
                            &b.raw,
                            &d.raw,
                            &format!("a/{}", b.path),
                            &format!("b/{}", d.path),
                        ));
                    }
                }
            }
        }
        Some(out)
    }

    /// Markdown link markup for a doc: `[name](path)` (copy-link).
    pub fn link_markup(&self, id: DocId) -> Option<String> {
        self.doc(id).map(|d| {
            let stem = d.name().trim_end_matches(".md").to_string();
            format!("[{stem}]({})", d.path)
        })
    }
}

/// Touched content lines between two texts: inserted + removed, a replaced
/// run counted once at its wider side.
fn touched_lines(old: &str, new: &str) -> usize {
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

/// One file's unified diff with `---`/`+++` names, git hunk format.
fn unified(old: &str, new: &str, a: &str, b: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(a, b)
        .to_string()
}

// ---- markdown → blocks ----------------------------------------------------

/// Pseudo-render markdown into the pane's block primitives. H1 → 0, deeper
/// headings → 1, paragraphs → 2, list items → 3, code blocks → 4; inline
/// markup flattens to its text.
pub fn parse_blocks(raw: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut cur: Option<Block> = None;
    let mut in_item = false;
    // the `.md` target while inside a link — non-.md links stay plain runs
    let mut link = String::new();
    for ev in Parser::new(raw) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                cur = Some(Block::new(u8::from(level != HeadingLevel::H1)));
            }
            Event::Start(Tag::Item) => {
                in_item = true;
                cur = Some(Block::new(3));
            }
            // a loose list item wraps its text in a paragraph — the bullet
            // block in progress keeps collecting, so no new block there
            Event::Start(Tag::Paragraph) if !in_item => {
                cur = Some(Block::new(2));
            }
            Event::Start(Tag::CodeBlock(_)) => {
                cur = Some(Block::new(4));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let dest = dest_url.to_string();
                if dest.ends_with(".md") {
                    link = dest;
                }
            }
            Event::End(TagEnd::Link) => {
                link.clear();
            }
            Event::End(TagEnd::Heading(_) | TagEnd::CodeBlock | TagEnd::Item)
            | Event::End(TagEnd::Paragraph) => {
                if matches!(ev, Event::End(TagEnd::Item)) {
                    in_item = false;
                } else if in_item {
                    // paragraph end inside a loose item: keep the bullet
                    continue;
                }
                if let Some(mut b) = cur.take() {
                    if b.kind == 4 {
                        b.text = b.text.trim_end_matches('\n').to_string();
                        b.spans = if b.text.is_empty() {
                            Vec::new()
                        } else {
                            vec![Span {
                                text: b.text.clone(),
                                link: String::new(),
                            }]
                        };
                    }
                    out.push(b);
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(b) = &mut cur {
                    b.push_run(&t, &link);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(b) = &mut cur {
                    b.push_run(" ", &link);
                }
            }
            _ => {}
        }
    }
    out
}

/// `.md` link targets in order of first appearance, deduped.
pub fn parse_links(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ev in Parser::new(raw) {
        if let Event::Start(Tag::Link { dest_url, .. }) = ev {
            let dest = dest_url.to_string();
            if dest.ends_with(".md") && !out.contains(&dest) {
                out.push(dest);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn id_of(w: &Wiki, name: &str) -> DocId {
        w.docs
            .iter()
            .find(|d| d.name() == name)
            .map(|d| d.id)
            .expect("doc exists")
    }

    // ---- baseline ---------------------------------------------------------

    #[test]
    fn the_sample_opens_with_the_charter_as_the_only_tab() {
        let w = Wiki::sample();
        let tabs = w.tab_rows();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].label, "charter.md");
        assert!(tabs[0].active);
        assert_eq!(w.change_counts(), (0, 0, 0));
    }

    #[test]
    fn nav_rows_list_folders_alphabetically_then_root_files() {
        let w = Wiki::sample();
        let labels: Vec<String> = w.nav_rows().iter().map(|r| r.label.clone()).collect();
        assert_eq!(
            labels,
            vec![
                "decisions",
                "2026-06-14-relay.md",
                "2026-07-02-treasury.md",
                "quests",
                "onboarding-guide.md",
                "runbooks",
                "node-recovery.md",
                "charter.md",
                "glossary.md",
            ]
        );
    }

    #[test]
    fn a_closed_folder_hides_its_files() {
        let mut w = Wiki::sample();
        w.toggle_folder("decisions");
        let labels: Vec<String> = w.nav_rows().iter().map(|r| r.label.clone()).collect();
        assert!(!labels.contains(&"2026-06-14-relay.md".to_string()));
        assert!(labels.contains(&"onboarding-guide.md".to_string()));
    }

    // ---- mark vs open (items 8) -------------------------------------------

    #[test]
    fn a_single_click_marks_but_never_opens() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        w.mark(glossary);
        assert_eq!(w.marked(), Some(glossary));
        assert_eq!(w.tab_rows().len(), 1, "no tab may open on mark");
        assert_ne!(w.active_id(), Some(glossary));
    }

    #[test]
    fn the_open_route_marks_opens_and_focuses() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        w.open(glossary);
        assert_eq!(w.marked(), Some(glossary));
        assert_eq!(w.active_id(), Some(glossary));
        assert_eq!(w.tab_rows().len(), 2);
        // re-opening refocuses without duplicating the tab
        w.open(glossary);
        assert_eq!(w.tab_rows().len(), 2);
    }

    // ---- tabs (items 2, 4, 5) ---------------------------------------------

    fn three_tabs(w: &mut Wiki) -> (DocId, DocId, DocId) {
        let a = id_of(w, "charter.md");
        let b = id_of(w, "glossary.md");
        let c = id_of(w, "node-recovery.md");
        w.open(b);
        w.open(c);
        (a, b, c)
    }

    #[test]
    fn tabs_list_in_open_order() {
        let mut w = Wiki::sample();
        let (_, b, c) = three_tabs(&mut w);
        let ids: Vec<DocId> = w.tab_rows().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![id_of(&w, "charter.md"), b, c]);
    }

    #[test]
    fn prev_and_next_walk_the_open_tabs_not_the_tree() {
        let mut w = Wiki::sample();
        let (a, b, c) = three_tabs(&mut w);
        assert_eq!(w.active_id(), Some(c));
        w.step_tab(1); // wraps to the first tab
        assert_eq!(w.active_id(), Some(a));
        w.step_tab(-1); // and back
        assert_eq!(w.active_id(), Some(c));
        w.step_tab(-1);
        assert_eq!(w.active_id(), Some(b));
        // browsing keeps the navigator mark where it was (arms 🎯)
        assert_eq!(w.marked(), Some(c));
    }

    #[test]
    fn closing_the_active_tab_focuses_right_then_left() {
        let mut w = Wiki::sample();
        let (a, b, c) = three_tabs(&mut w);
        w.focus(b);
        w.close_tab(b); // right neighbor wins
        assert_eq!(w.active_id(), Some(c));
        w.close_tab(c); // no right neighbor left
        assert_eq!(w.active_id(), Some(a));
        w.close_active(); // Ctrl+W on the last tab
        assert_eq!(w.active_id(), None);
        assert!(w.tab_rows().is_empty());
    }

    #[test]
    fn close_right_and_left_of_a_tab() {
        let mut w = Wiki::sample();
        let (a, b, _c) = three_tabs(&mut w);
        w.close_right_of(b); // closes c; active was c → moves to b
        let ids: Vec<DocId> = w.tab_rows().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![a, b]);
        assert_eq!(w.active_id(), Some(b));
        w.close_left_of(b); // closes a; active b untouched
        let ids: Vec<DocId> = w.tab_rows().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![b]);
        assert_eq!(w.active_id(), Some(b));
    }

    #[test]
    fn close_all_leaves_the_honest_empty_state() {
        let mut w = Wiki::sample();
        let _ = three_tabs(&mut w);
        w.close_all();
        assert!(w.tab_rows().is_empty());
        assert_eq!(w.active_id(), None);
    }

    // ---- create / rename / move / delete (items 6, 7, 9, 10) --------------

    #[test]
    fn a_new_file_is_added_open_and_renaming() {
        let mut w = Wiki::sample();
        let id = w.new_file();
        assert_eq!(w.active_id(), Some(id));
        assert_eq!(w.renaming(), Some(id));
        let row = w
            .nav_rows()
            .into_iter()
            .find(|r| r.id == id)
            .expect("new file listed");
        assert_eq!(row.label, "untitled-1.md");
        assert_eq!(row.status, Status::Added);
        // a second one picks the next free name
        let id2 = w.new_file();
        assert_eq!(
            w.nav_rows()
                .into_iter()
                .find(|r| r.id == id2)
                .expect("second file listed")
                .label,
            "untitled-2.md"
        );
        assert_eq!(w.change_counts(), (2, 0, 0));
    }

    #[test]
    fn a_new_file_lands_in_the_marked_files_folder() {
        let mut w = Wiki::sample();
        let relay = id_of(&w, "2026-06-14-relay.md");
        w.mark(relay);
        let id = w.new_file();
        let d = w.doc(id).expect("created");
        assert_eq!(d.path, "decisions/untitled-1.md");
    }

    #[test]
    fn rename_appends_md_and_refuses_collisions() {
        let mut w = Wiki::sample();
        let relay = id_of(&w, "2026-06-14-relay.md");
        w.rename_commit(relay, "relay-decision")
            .expect("rename works");
        let d = w.doc(relay).expect("doc");
        assert_eq!(d.path, "decisions/relay-decision.md");
        assert_eq!(d.status(), Status::Modified);
        let treasury = id_of(&w, "2026-07-02-treasury.md");
        assert!(w.rename_commit(treasury, "relay-decision.md").is_err());
        assert!(w.rename_commit(relay, "  ").is_err());
    }

    #[test]
    fn a_rename_back_to_the_base_path_is_unchanged_again() {
        let mut w = Wiki::sample();
        let relay = id_of(&w, "2026-06-14-relay.md");
        w.rename_commit(relay, "x").expect("rename");
        w.rename_commit(relay, "2026-06-14-relay.md")
            .expect("rename back");
        assert_eq!(w.doc(relay).expect("doc").status(), Status::Unchanged);
    }

    #[test]
    fn move_rewrites_the_folder_and_refuses_collisions() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        w.move_to(glossary, Some("decisions")).expect("move works");
        assert_eq!(w.doc(glossary).expect("doc").path, "decisions/glossary.md");
        assert_eq!(w.doc(glossary).expect("doc").status(), Status::Modified);
        w.move_to(glossary, None).expect("move back to root");
        assert_eq!(w.doc(glossary).expect("doc").status(), Status::Unchanged);
        // collision: a glossary.md already sits in decisions/ — the move is
        // refused and the path stays untouched
        let id = w.new_file();
        w.rename_commit(id, "glossary-2").expect("rename");
        w.move_to(id, Some("decisions")).expect("move");
        w.rename_commit(id, "glossary").expect("rename in folder");
        assert!(w.move_to(glossary, Some("decisions")).is_err());
        assert_eq!(w.doc(glossary).expect("doc").path, "glossary.md");
    }

    #[test]
    fn deleting_a_base_doc_is_a_pending_change_not_a_disappearance() {
        let mut w = Wiki::sample();
        let charter = id_of(&w, "charter.md");
        w.delete(charter);
        // the tab closed, the row stays — struck red
        assert!(w.tab_rows().is_empty());
        let row = w
            .nav_rows()
            .into_iter()
            .find(|r| r.id == charter)
            .expect("deleted doc stays listed");
        assert_eq!(row.status, Status::Deleted);
        assert_eq!(w.change_counts(), (0, 0, 1));
        // and it no longer opens
        w.open(charter);
        assert_eq!(w.active_id(), None);
    }

    #[test]
    fn deleting_an_added_doc_removes_it_entirely() {
        let mut w = Wiki::sample();
        let id = w.new_file();
        w.delete(id);
        assert!(w.nav_rows().iter().all(|r| r.id != id));
        assert_eq!(w.change_counts(), (0, 0, 0));
    }

    // ---- edits + diff (item 10) -------------------------------------------

    #[test]
    fn an_edit_marks_modified_and_a_revert_clears_it() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        let base = w.doc(glossary).expect("doc").raw.clone();
        w.set_raw(glossary, &format!("{base}\n\nA new paragraph."));
        assert_eq!(w.doc(glossary).expect("doc").status(), Status::Modified);
        w.set_raw(glossary, &base);
        assert_eq!(w.doc(glossary).expect("doc").status(), Status::Unchanged);
    }

    #[test]
    fn the_preview_diff_marks_added_changed_and_removed_blocks() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        let base = w.doc(glossary).expect("doc").raw.clone();
        // three edits far apart, so each diff op stays pure: change one
        // bullet, insert another mid-list, drop the last one
        let edited = base
            .replace(
                "- Seal - the moment a proposal reaches the threshold and becomes canon.",
                "- Seal - the m-of-n moment a change becomes canon.",
            )
            .replace(
                "- Threshold - our 3-of-4 rule",
                "- Consensus - agreement made countable.\n- Threshold - our 3-of-4 rule",
            )
            .replace(
                "\n- Patch - one gated change on the chain, like a commit.",
                "",
            );
        w.set_raw(glossary, &edited);
        let pv = w.preview(glossary);
        assert!(
            pv.iter().any(|(b, s)| *s == BlockStatus::Changed
                && b.text.contains("m-of-n")),
            "the edited bullet is Changed: {pv:?}"
        );
        assert!(
            pv.iter().any(|(b, s)| *s == BlockStatus::Removed
                && b.text.contains("Patch")),
            "the dropped bullet stays visible as a Removed ghost: {pv:?}"
        );
        assert!(
            pv.iter().any(|(b, s)| *s == BlockStatus::Added
                && b.text.contains("Consensus")),
            "the inserted bullet is Added: {pv:?}"
        );
        assert!(
            pv.iter().any(|(_, s)| *s == BlockStatus::Same),
            "untouched blocks stay Same"
        );
    }

    #[test]
    fn an_added_docs_preview_is_all_added() {
        let mut w = Wiki::sample();
        let id = w.new_file();
        assert!(w.preview(id).iter().all(|(_, s)| *s == BlockStatus::Added));
    }

    // ---- markdown pseudo-render -------------------------------------------

    #[test]
    fn parse_blocks_maps_the_five_kinds() {
        let blocks = parse_blocks(
            "# Title\n\nA paragraph.\n\n## Section\n\n- bullet one\n- bullet two\n\n```\ncode line\n```",
        );
        let kinds: Vec<u8> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(kinds, vec![0, 2, 1, 3, 3, 4]);
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(blocks[3].text, "bullet one");
        assert_eq!(blocks[5].text, "code line");
    }

    #[test]
    fn inline_markup_flattens_and_links_are_collected() {
        let blocks = parse_blocks("Read [charter](charter.md) and `code` now.");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "Read charter and code now.");
        assert_eq!(
            parse_links("See [a](charter.md), [b](glossary.md), [a again](charter.md), [web](https://x.example)"),
            vec!["charter.md".to_string(), "glossary.md".to_string()]
        );
    }

    #[test]
    fn the_sample_docs_render_their_links() {
        let w = Wiki::sample();
        let charter = id_of(&w, "charter.md");
        let links = w.links(charter);
        assert!(links.contains(&"decisions/2026-07-02-treasury.md".to_string()));
        assert!(links.contains(&"glossary.md".to_string()));
    }

    // ---- the engine base lands (shared_memory_real.md WP-C) ----------------

    /// The folded base replaces the sample: an empty wiki fills from the
    /// engine read, a moved base recolors local work instead of dropping
    /// it, and ids stay stable across refreshes (the nav/tab state hangs
    /// off them).
    #[test]
    fn set_base_fills_updates_and_keeps_local_work() {
        let mut w = Wiki::empty();
        assert!(w.nav_rows().is_empty(), "an empty wiki starts with nothing");
        let base = vec![
            ("a.md".to_string(), "alpha\n".to_string()),
            ("dir/b.md".to_string(), "beta\n".to_string()),
        ];
        w.set_base(&base, 1);
        assert_eq!(w.base_rev, 1);
        let a = w.docs.iter().find(|d| d.path == "a.md").expect("a.md");
        assert_eq!(a.raw, "alpha\n");
        assert_eq!(a.status(), Status::Unchanged);
        let a_id = a.id;

        // local edit survives a base refresh of the OTHER file
        w.set_raw(a_id, "alpha local");
        let base2 = vec![
            ("a.md".to_string(), "alpha\n".to_string()),
            ("dir/b.md".to_string(), "beta v2\n".to_string()),
        ];
        w.set_base(&base2, 2);
        let a = w.docs.iter().find(|d| d.path == "a.md").expect("a.md");
        assert_eq!(a.id, a_id, "ids are stable across refreshes");
        assert_eq!(a.raw, "alpha local", "local work is kept");
        assert_eq!(a.status(), Status::Modified);
        let b = w.docs.iter().find(|d| d.path == "dir/b.md").expect("b");
        assert_eq!(b.raw, "beta v2\n", "an unedited doc follows the base");

        // the base moves UNDER the local edit: still kept, still Modified
        let base3 = vec![
            ("a.md".to_string(), "alpha v3\n".to_string()),
            ("dir/b.md".to_string(), "beta v2\n".to_string()),
        ];
        w.set_base(&base3, 3);
        let a = w.docs.iter().find(|d| d.path == "a.md").expect("a.md");
        assert_eq!(a.raw, "alpha local");
        assert_eq!(a.status(), Status::Modified);

        // a doc leaving the base: unedited vanishes (tab closed too),
        // edited stays as ADDED (your content is new against this base)
        w.open(a.id);
        let base4 = vec![("dir/b.md".to_string(), "beta v2\n".to_string())];
        w.set_base(&base4, 4);
        let a = w.docs.iter().find(|d| d.path == "a.md").expect("edited stays");
        assert_eq!(a.status(), Status::Added);
        assert!(w.docs.iter().all(|d| d.path != "x.md"));
        let base5: Vec<(String, String)> = Vec::new();
        let b_id = w.docs.iter().find(|d| d.path == "dir/b.md").expect("b").id;
        w.open(b_id);
        w.set_base(&base5, 5);
        assert!(
            w.docs.iter().all(|d| d.path != "dir/b.md"),
            "an unedited doc leaves with its base"
        );
        assert!(
            w.tab_rows().iter().all(|t| t.label != "dir/b.md"),
            "…and its tab closes"
        );
    }

    // ---- the rescue (shared_memory_real.md §4: declined/superseded) --------

    /// A declined or superseded proposal's patch comes back as LOCAL
    /// drafts: best-effort per file against the CURRENT working copy,
    /// with real stack entries (the changeset panel must show and the
    /// next vote must carry the rescued work); what no longer fits is
    /// skipped and counted honestly.
    #[test]
    fn rescue_restores_what_fits_and_reports_what_does_not() {
        let mut w = Wiki::empty();
        w.set_base(
            &[("a.md".to_string(), "hello\nworld\n".to_string())],
            1,
        );
        // the patch: edits a.md AND adds new.md AND edits a file that is
        // gone from this base
        let patch = "diff --git a/a.md b/a.md\n--- a/a.md\n+++ b/a.md\n@@ -1,2 +1,2 @@\n hello\n-world\n+welt\n\
diff --git a/new.md b/new.md\nnew file mode 100644\n--- /dev/null\n+++ b/new.md\n@@ -0,0 +1,1 @@\n+fresh\n\
diff --git a/gone.md b/gone.md\n--- a/gone.md\n+++ b/gone.md\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        let (applied, skipped) = w.rescue_patch(patch);
        assert_eq!((applied, skipped), (2, 1));
        let a = w.docs.iter().find(|d| d.path == "a.md").expect("a.md");
        assert_eq!(a.raw, "hello\nwelt\n");
        assert_eq!(a.status(), Status::Modified);
        let n = w.docs.iter().find(|d| d.path == "new.md").expect("new.md");
        assert_eq!(n.raw, "fresh\n");
        assert_eq!(n.status(), Status::Added);
        assert!(
            !w.stack_rows().is_empty(),
            "the rescue records stack entries - the panel must show"
        );
        // …and the rescued changeset votes: the built patch folds cleanly
        let built = w.build_patch().expect("patch");
        let mut tree = std::collections::BTreeMap::from([(
            "a.md".to_string(),
            "hello\nworld\n".to_string(),
        )]);
        molt_core::wiki_fold::apply_patch(
            &mut tree,
            &molt_core::wiki_fold::parse_patch(&built),
        )
        .expect("the rescued changeset applies");
        // a rescue onto DIRTY local work never overwrites it
        let a_id = w.docs.iter().find(|d| d.path == "a.md").expect("a").id;
        w.set_raw(a_id, "my own words");
        let (applied2, skipped2) = w.rescue_patch(patch);
        assert_eq!(applied2, 0, "nothing fits a diverged working copy");
        assert_eq!(skipped2, 3);
        let a = w.docs.iter().find(|d| d.path == "a.md").expect("a.md");
        assert_eq!(a.raw, "my own words", "local work is never clobbered");
    }

    // ---- the fold round-trip (shared_memory_real.md WP-A keystone) ---------

    /// THE contract between the proposer and every folder: the net patch
    /// `build_patch` emits, applied strictly by `molt_core::wiki_fold`
    /// onto the BASE tree, reproduces the proposer's working docs
    /// byte-exactly — add, edit, rename and delete included. If this
    /// breaks, members would ratify one content and fold another.
    #[test]
    fn a_built_patch_folds_byte_exactly_onto_the_base_tree() {
        let mut w = Wiki::sample();
        let base_tree: std::collections::BTreeMap<String, String> = w
            .docs
            .iter()
            .filter_map(|d| d.base.as_ref().map(|b| (b.path.clone(), b.raw.clone())))
            .collect();
        let id_of = |w: &Wiki, name: &str| {
            w.docs
                .iter()
                .find(|d| d.name() == name)
                .map(|d| d.id)
                .expect("doc")
        };
        // one of each kind: edit, rename+edit, delete, new file
        let glossary = id_of(&w, "glossary.md");
        w.set_raw(glossary, "# Words\n\nnew and short.");
        let relay = id_of(&w, "2026-06-14-relay.md");
        w.rename_commit(relay, "relay-decision").expect("rename");
        w.set_raw(relay, "# Relay\n\nmoved and edited");
        let charter = id_of(&w, "charter.md");
        w.delete(charter);
        let draft = w.new_file();
        w.rename_commit(draft, "todo").expect("rename");
        w.set_raw(draft, "# Todo\n\n- first\n- second");

        let patch = w.build_patch().expect("patch");
        let mut tree = base_tree;
        molt_core::wiki_fold::apply_patch(&mut tree, &molt_core::wiki_fold::parse_patch(&patch))
            .expect("the built patch applies strictly");

        let want: std::collections::BTreeMap<String, String> = w
            .docs
            .iter()
            .filter(|d| d.status() != Status::Deleted)
            .map(|d| (d.path.clone(), d.raw.clone()))
            .collect();
        assert_eq!(tree, want, "fold(base, build_patch) == working docs");
    }

    // ---- in-preview links --------------------------------------------------

    #[test]
    fn blocks_carry_link_spans_for_md_targets_only() {
        let blocks = parse_blocks("Read [charter](charter.md) and [web](https://x.example) now.");
        assert_eq!(blocks.len(), 1);
        // flat text stays what it was — the diff layer keys on it
        assert_eq!(blocks[0].text, "Read charter and web now.");
        let spans: Vec<(&str, &str)> = blocks[0]
            .spans
            .iter()
            .map(|s| (s.text.as_str(), s.link.as_str()))
            .collect();
        assert_eq!(
            spans,
            vec![
                ("Read ", ""),
                ("charter", "charter.md"),
                (" and web now.", ""),
            ]
        );
    }

    #[test]
    fn a_plain_block_is_one_plain_span() {
        let blocks = parse_blocks("No links here, `just code`.");
        assert_eq!(blocks.len(), 1);
        let spans = &blocks[0].spans;
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, blocks[0].text);
        assert_eq!(spans[0].link, "");
    }

    #[test]
    fn open_link_resolves_exact_path_then_unique_basename() {
        let mut w = Wiki::sample();
        assert!(w.open_link("glossary.md"));
        assert_eq!(w.active().expect("open").path, "glossary.md");
        assert!(w.open_link("runbooks/node-recovery.md"));
        assert_eq!(w.active().expect("open").path, "runbooks/node-recovery.md");
        // a hand-written link often skips the folder — the unique basename
        assert!(w.open_link("node-recovery.md"));
        assert_eq!(w.active().expect("open").path, "runbooks/node-recovery.md");
        // a miss changes nothing
        let before = w.active_id();
        assert!(!w.open_link("missing.md"));
        assert_eq!(w.active_id(), before);
    }

    #[test]
    fn open_link_skips_deleted_docs() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        w.delete(glossary);
        assert!(!w.open_link("glossary.md"));
    }

    // ---- drag-move (item 7) -----------------------------------------------

    #[test]
    fn a_drop_resolves_folder_file_and_past_end_targets() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        // row 0 is the "decisions" folder → into it
        w.drop_on_row(glossary, 0).expect("drop on folder");
        assert_eq!(w.doc(glossary).expect("doc").path, "decisions/glossary.md");
        // a file row in quests → next to that file
        let quests_row = w
            .nav_rows()
            .iter()
            .position(|r| r.label == "onboarding-guide.md")
            .expect("row");
        w.drop_on_row(glossary, quests_row).expect("drop on file");
        assert_eq!(w.doc(glossary).expect("doc").path, "quests/glossary.md");
        // past the list end → root
        w.drop_on_row(glossary, 999).expect("drop past end");
        assert_eq!(w.doc(glossary).expect("doc").path, "glossary.md");
    }

    // ---- the changeset stack (panel, undo, reverts) ------------------------

    #[test]
    fn keystrokes_coalesce_into_one_stack_entry_per_edit_run() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        let charter = id_of(&w, "charter.md");
        let base = w.doc(glossary).expect("doc").raw.clone();
        // echoing the current buffer back is not a change
        w.set_raw(glossary, &base);
        assert_eq!(w.stack_len(), 0);
        w.set_raw(glossary, &format!("{base}x"));
        w.set_raw(glossary, &format!("{base}xy"));
        assert_eq!(w.stack_len(), 1, "one run of keystrokes, one entry");
        w.set_raw(charter, "# Charter\n\nshorter.");
        assert_eq!(w.stack_len(), 2);
        // returning to the same doc starts a NEW run
        w.set_raw(glossary, &format!("{base}xyz"));
        assert_eq!(w.stack_len(), 3);
        // undo of that run lands on the state BEFORE it, not on base
        w.undo().expect("undo");
        assert_eq!(w.doc(glossary).expect("doc").raw, format!("{base}xy"));
    }

    #[test]
    fn undo_walks_the_stack_in_lifo_order() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        let relay = id_of(&w, "2026-06-14-relay.md");
        let charter = id_of(&w, "charter.md");
        let base = w.doc(glossary).expect("doc").raw.clone();
        w.set_raw(glossary, &format!("{base}\n\nMore."));
        w.rename_commit(relay, "relay-decision").expect("rename");
        w.delete(charter);
        assert_eq!(w.stack_len(), 3);
        w.undo().expect("undo delete");
        assert_eq!(w.doc(charter).expect("doc").status(), Status::Unchanged);
        w.undo().expect("undo rename");
        assert_eq!(w.doc(relay).expect("doc").path, "decisions/2026-06-14-relay.md");
        w.undo().expect("undo edit");
        assert_eq!(w.doc(glossary).expect("doc").status(), Status::Unchanged);
        assert_eq!(w.stack_len(), 0);
    }

    #[test]
    fn undo_brings_a_deleted_draft_back_and_a_second_undo_removes_it() {
        let mut w = Wiki::sample();
        let id = w.new_file();
        assert_eq!(w.stack_len(), 1);
        w.delete(id); // an added doc vanishes from the tree …
        assert!(w.nav_rows().iter().all(|r| r.id != id));
        assert_eq!(w.stack_len(), 2, "… but the action is on the stack");
        w.undo().expect("undo the delete");
        let row = w
            .nav_rows()
            .into_iter()
            .find(|r| r.id == id)
            .expect("draft is back");
        assert_eq!(row.status, Status::Added);
        w.undo().expect("undo the create");
        assert!(w.nav_rows().iter().all(|r| r.id != id));
        assert_eq!(w.stack_len(), 0);
        assert!(w.tab_rows().iter().all(|t| t.id != id));
    }

    #[test]
    fn revert_all_restores_the_base_snapshot_and_empties_the_stack() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        let charter = id_of(&w, "charter.md");
        w.set_raw(glossary, "# Glossary\n\ngutted.");
        w.delete(charter);
        let draft = w.new_file();
        w.new_folder();
        w.move_to(glossary, Some("decisions")).expect("move");
        assert!(w.stack_len() > 0);
        w.revert_all();
        assert_eq!(w.stack_len(), 0);
        assert_eq!(w.change_counts(), (0, 0, 0));
        assert_eq!(w.changeset_counts(), ChangesetCounts::default());
        assert!(w.nav_rows().iter().all(|r| r.id != draft));
        assert!(w.folders_named().iter().all(|f| f != "new-folder-1"));
        assert_eq!(w.doc(glossary).expect("doc").path, "glossary.md");
        assert_eq!(w.doc(charter).expect("doc").status(), Status::Unchanged);
    }

    #[test]
    fn revert_doc_restores_one_file_and_drops_only_its_entries() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        let charter = id_of(&w, "charter.md");
        w.set_raw(glossary, "# Glossary\n\ngutted.");
        w.rename_commit(glossary, "words").expect("rename");
        w.set_raw(charter, "# Charter\n\nshorter.");
        assert_eq!(w.stack_len(), 3);
        w.revert_doc(glossary).expect("revert");
        assert_eq!(w.doc(glossary).expect("doc").status(), Status::Unchanged);
        assert_eq!(w.doc(glossary).expect("doc").path, "glossary.md");
        assert_eq!(w.stack_len(), 1, "the charter edit survives");
        // a pending deletion is revertable the same way
        w.delete(charter);
        w.revert_doc(charter).expect("revert deletion");
        assert_eq!(w.doc(charter).expect("doc").status(), Status::Unchanged);
        assert_eq!(w.stack_len(), 0);
    }

    #[test]
    fn revert_doc_refuses_when_the_base_path_is_taken() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        w.rename_commit(glossary, "words").expect("rename away");
        let draft = w.new_file();
        w.rename_commit(draft, "glossary").expect("squat the base name");
        assert!(w.revert_doc(glossary).is_err());
        assert_eq!(w.doc(glossary).expect("doc").path, "words.md");
        assert_eq!(w.stack_len(), 3, "nothing was dropped");
    }

    #[test]
    fn undo_refuses_when_the_inverse_path_is_taken() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        w.rename_commit(glossary, "words").expect("rename away");
        let draft = w.new_file();
        w.rename_commit(draft, "glossary").expect("squat the base name");
        w.delete(draft); // draft (at glossary.md) leaves, snapshot on stack
        w.revert_doc(glossary).expect("glossary back at its base path");
        // undoing the draft's deletion would reinsert it at glossary.md,
        // which glossary holds again — refused, stack intact
        let before = w.stack_len();
        assert!(w.undo().is_err());
        assert_eq!(w.stack_len(), before);
    }

    #[test]
    fn stack_rows_narrate_the_actions_with_record_time_labels() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        w.set_raw(glossary, "# Glossary\n\ngutted.");
        let id = w.new_file();
        w.rename_commit(id, "notes").expect("rename");
        w.move_to(glossary, Some("decisions")).expect("move");
        w.new_folder();
        let treasury = id_of(&w, "2026-07-02-treasury.md");
        w.delete(treasury);
        let rows = w.stack_rows();
        let got: Vec<(ChangeKind, String)> =
            rows.into_iter().map(|r| (r.kind, r.label)).collect();
        assert_eq!(
            got,
            vec![
                (ChangeKind::Edited, "glossary.md".to_string()),
                (ChangeKind::Created, "untitled-1.md".to_string()),
                (ChangeKind::Renamed, "untitled-1.md → notes.md".to_string()),
                (ChangeKind::Moved, "glossary.md → decisions/".to_string()),
                (ChangeKind::CreatedFolder, "new-folder-1/".to_string()),
                (ChangeKind::Deleted, "2026-07-02-treasury.md".to_string()),
            ]
        );
    }

    // ---- the changeset summary + the git patch -----------------------------

    #[test]
    fn changeset_counts_split_new_deleted_moved_and_touched_lines() {
        let mut w = Wiki::sample();
        let treasury = id_of(&w, "2026-07-02-treasury.md");
        let glossary = id_of(&w, "glossary.md");
        let onboarding = id_of(&w, "onboarding-guide.md");
        w.new_file();
        w.delete(treasury);
        w.move_to(glossary, Some("decisions")).expect("move");
        let base = w.doc(onboarding).expect("doc").raw.clone();
        w.set_raw(
            onboarding,
            &base.replace(
                "- Work through the MLS & SMP reading list, messaging layer first.",
                "- Work through the reading list.",
            ),
        );
        assert_eq!(
            w.changeset_counts(),
            ChangesetCounts { added: 1, deleted: 1, moved: 1, lines: 1 }
        );
    }

    #[test]
    fn the_patch_covers_add_edit_rename_and_delete_sorted_by_path() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        let relay = id_of(&w, "2026-06-14-relay.md");
        let charter = id_of(&w, "charter.md");
        let base = w.doc(glossary).expect("doc").raw.clone();
        w.set_raw(
            glossary,
            &base.replace(
                "- Seal - the moment a proposal reaches the threshold and becomes canon.",
                "- Seal - the m-of-n moment.",
            ),
        );
        w.rename_commit(relay, "relay-decision").expect("pure rename");
        w.delete(charter);
        let draft = w.new_file();
        w.rename_commit(draft, "todo").expect("rename");
        w.set_raw(draft, "# Todo\n\n- first");
        let patch = w.build_patch().expect("patch");
        // edited file: plain hunks
        assert!(patch.contains("diff --git a/glossary.md b/glossary.md"));
        assert!(patch.contains("+- Seal - the m-of-n moment."));
        // pure rename: headers only, no hunks against the old path
        assert!(patch.contains(
            "diff --git a/decisions/2026-06-14-relay.md b/decisions/relay-decision.md"
        ));
        assert!(patch.contains("similarity index 100%"));
        assert!(patch.contains("rename from decisions/2026-06-14-relay.md"));
        assert!(patch.contains("rename to decisions/relay-decision.md"));
        assert!(!patch.contains("--- a/decisions/2026-06-14-relay.md"));
        // deletion
        assert!(patch.contains("deleted file mode 100644"));
        assert!(patch.contains("--- a/charter.md"));
        assert!(patch.contains("+++ /dev/null"));
        assert!(patch.contains("-# Charter"));
        // addition
        assert!(patch.contains("new file mode 100644"));
        assert!(patch.contains("--- /dev/null"));
        assert!(patch.contains("+++ b/todo.md"));
        assert!(patch.contains("@@ -0,0 +1,3 @@"));
        assert!(patch.contains("+# Todo"));
        // deterministic order: sorted by display path
        let order: Vec<usize> = [
            "diff --git a/charter.md",
            "diff --git a/decisions/2026-06-14-relay.md",
            "diff --git a/glossary.md",
            "diff --git a/todo.md",
        ]
        .iter()
        .map(|n| patch.find(n).expect("present"))
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "sorted: {order:?}");
    }

    #[test]
    fn a_rename_with_edits_carries_its_hunks_under_the_rename_header() {
        let mut w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        w.rename_commit(glossary, "words").expect("rename");
        w.set_raw(glossary, "# Words\n\nshort now.");
        let patch = w.build_patch().expect("patch");
        assert!(patch.contains("diff --git a/glossary.md b/words.md"));
        assert!(patch.contains("rename from glossary.md"));
        assert!(patch.contains("rename to words.md"));
        assert!(patch.contains("--- a/glossary.md"));
        assert!(patch.contains("+++ b/words.md"));
        assert!(!patch.contains("similarity index"));
    }

    #[test]
    fn a_changeset_that_nets_out_builds_no_patch() {
        let mut w = Wiki::sample();
        assert!(w.build_patch().is_none(), "clean tree, no patch");
        let glossary = id_of(&w, "glossary.md");
        let base = w.doc(glossary).expect("doc").raw.clone();
        w.set_raw(glossary, &format!("{base}\n\nMore."));
        w.set_raw(glossary, &base);
        assert!(w.stack_len() > 0, "the actions happened");
        assert!(w.build_patch().is_none(), "… but nothing net-changed");
        // an added empty folder is not patch content either
        w.new_folder();
        assert!(w.build_patch().is_none());
    }

    // ---- link markup (copy link) -------------------------------------------

    #[test]
    fn link_markup_names_the_file_and_keeps_the_full_path() {
        let w = Wiki::sample();
        let glossary = id_of(&w, "glossary.md");
        let recovery = id_of(&w, "node-recovery.md");
        assert_eq!(
            w.link_markup(glossary).expect("markup"),
            "[glossary](glossary.md)"
        );
        assert_eq!(
            w.link_markup(recovery).expect("markup"),
            "[node-recovery](runbooks/node-recovery.md)"
        );
        assert!(w.link_markup(9999).is_none());
    }

    // ---- reveal (🎯) -------------------------------------------------------

    #[test]
    fn reveal_marks_the_active_tab_and_unfolds_its_folder() {
        let mut w = Wiki::sample();
        let recovery = id_of(&w, "node-recovery.md");
        w.open(recovery);
        w.set_all_folders(false);
        let charter = id_of(&w, "charter.md");
        w.mark(charter); // mark diverges from active
        w.reveal();
        assert_eq!(w.marked(), Some(recovery));
        assert!(
            w.nav_rows().iter().any(|r| r.id == recovery),
            "the folder unfolded so the row is visible"
        );
    }
}
