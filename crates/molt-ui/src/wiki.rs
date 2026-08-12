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
use similar::{capture_diff_slices, Algorithm, DiffOp};

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
}

impl Wiki {
    // ---- construction -----------------------------------------------------

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
        }
    }

    /// The design-mock base snapshot (the .slint sample data, single source:
    /// raw markdown — blocks derive from it).
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

    /// (added, modified, deleted) doc counts — the pending change set.
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
            d.path = path;
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
            None => name,
        };
        if self.docs.iter().any(|o| o.id != id && o.path == path) {
            return Err("name already taken".to_string());
        }
        if let Some(d) = self.doc_mut(id) {
            d.path = path;
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
        let Some(is_added) = self.doc(id).map(|d| d.base.is_none()) else {
            return;
        };
        self.close_tab(id);
        if is_added {
            self.docs.retain(|d| d.id != id);
            if self.marked == Some(id) {
                self.marked = None;
            }
        } else if let Some(d) = self.doc_mut(id) {
            d.deleted = true;
        }
        if self.renaming == Some(id) {
            self.renaming = None;
        }
    }

    // ---- editing + diff ---------------------------------------------------

    /// The raw editor's buffer flows here on every edit; reverting to the
    /// base bytes turns the doc Unchanged again.
    pub fn set_raw(&mut self, id: DocId, raw: &str) {
        if let Some(d) = self.doc_mut(id) {
            if !d.deleted {
                d.raw = raw.to_string();
            }
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
}

// ---- markdown → blocks ----------------------------------------------------

/// Pseudo-render markdown into the pane's block primitives. H1 → 0, deeper
/// headings → 1, paragraphs → 2, list items → 3, code blocks → 4; inline
/// markup flattens to its text.
pub fn parse_blocks(raw: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut cur: Option<Block> = None;
    let mut in_item = false;
    for ev in Parser::new(raw) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => {
                cur = Some(Block {
                    kind: u8::from(level != HeadingLevel::H1),
                    text: String::new(),
                });
            }
            Event::Start(Tag::Item) => {
                in_item = true;
                cur = Some(Block {
                    kind: 3,
                    text: String::new(),
                });
            }
            // a loose list item wraps its text in a paragraph — the bullet
            // block in progress keeps collecting, so no new block there
            Event::Start(Tag::Paragraph) if !in_item => {
                cur = Some(Block {
                    kind: 2,
                    text: String::new(),
                });
            }
            Event::Start(Tag::CodeBlock(_)) => {
                cur = Some(Block {
                    kind: 4,
                    text: String::new(),
                });
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
                    }
                    out.push(b);
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(b) = &mut cur {
                    b.text.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(b) = &mut cur {
                    b.text.push(' ');
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
