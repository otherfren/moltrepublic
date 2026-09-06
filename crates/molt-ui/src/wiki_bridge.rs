// SPDX-License-Identifier: GPL-3.0-or-later
//! The Shared-Memory wiki's bridge between its Rust state machine
//! ([`crate::wiki`]) and the `WikiState` / `PatchView` globals: the face
//! sync, the callback wiring, the diff viewer and the export dialog.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use molt_core::{Command, LocalCopy, Reply, Surface, UploadView};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::i18n::{error_toast, localize_wiki_err, Lexicon};
use crate::labels::file_size_label;
use crate::models::{sync_model, wiki_block_eq};
use crate::settings::browse_start_dir;
use crate::app::Ctx;
use crate::{
    patchview,
    wiki,
    AppWindow,
    DiffRow,
    DiffSeg,
    PatchNavRow,
    PatchView,
    Strings,
    WikiBlock,
    WikiChangeRow,
    WikiBase,
    WikiHitRow,
    WikiNavRow,
    WikiProp,
    WikiRelation,
    WikiSpan,
    WikiState,
    WikiTabRow,
};

/// `WikiNavRow.status` / `WikiTabRow.status` code for a doc's
/// pending-change state (0 unchanged · 1 added · 2 modified · 3 deleted —
/// the tone mapping lives once in the pane's `tone()`).
fn wiki_status_code(s: wiki::Status) -> i32 {
    match s {
        wiki::Status::Unchanged => 0,
        wiki::Status::Added => 1,
        wiki::Status::Modified => 2,
        wiki::Status::Deleted => 3,
    }
}

fn wiki_doc_id(id: i32) -> wiki::DocId {
    u32::try_from(id).unwrap_or(0)
}

/// At most one draft hop per this much traffic (WP-D): a hard kill loses
/// at most that window.
const DRAFT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// What ONE wiring's face sync carries between calls: the editor-buffer
/// guard, plus the keys that say which parts of the face have to be
/// rebuilt at all (`docs_archive/ui/wiki_pane_performance.md` F4 - the sync used
/// to rebuild everything after every callback, 85 % of it a draft
/// serialization of a model nothing had touched).
pub(crate) struct FaceState {
    /// The (doc, edit mode) `raw` was written for. `None` forces the next
    /// sync to rewrite the editor buffer (a modal wrote under the caret);
    /// otherwise the keystroke echo never rewrites it.
    raw_for: Option<(wiki::DocId, bool)>,
    /// The draft last handed to the engine, when, and the model
    /// generation it was taken at.
    draft: String,
    draft_at: std::time::Instant,
    draft_gen: u64,
    /// A change is waiting for the 2 s window to close, and a timer is
    /// armed to save it there: without one, a member who types once and
    /// walks away has no next sync, and the window would never end.
    draft_pending: bool,
    flush_armed: bool,
    /// The generation `cs-patch` was built at.
    patch_gen: Option<u64>,
    /// The active document the block view, infobox and link list were
    /// built from ([`wiki::Wiki::face_key`]).
    doc_key: Option<(wiki::DocId, u64)>,
}

impl FaceState {
    fn new() -> Self {
        FaceState {
            raw_for: None,
            draft: String::new(),
            draft_at: std::time::Instant::now(),
            draft_gen: 0,
            draft_pending: false,
            flush_armed: false,
            patch_gen: None,
            doc_key: None,
        }
    }
}

// ---- file references (images + file links) ---------------------------------
// `wiki_files_and_images.md` §3.4/§3.5. One cache per window thread, keyed
// by the reference's hex: the engine's answer, the decoded picture and the
// in-flight set. An answer PATCHES the rows it touches - it never re-syncs
// the face (`docs_archive/ui/wiki_pane_performance.md` F4).

/// What `ReadUploadBytes` refuses above (§4).
const IMAGE_READ_CAP: u64 = 32 * 1024 * 1024;
/// One window's budget of decoded pictures.
const IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// `WikiBlock::file_state` / `WikiSpan::file_state`. State 3 with an
/// EMPTY name is the answer still in flight: the card shows the
/// reference and no verb.
pub(crate) mod file_state {
    pub(crate) const NONE: i32 = 0;
    pub(crate) const UNKNOWN: i32 = 1;
    pub(crate) const AMBIGUOUS: i32 = 2;
    pub(crate) const REMOTE: i32 = 3;
    pub(crate) const FETCHING: i32 = 4;
    pub(crate) const DECODING: i32 = 5;
    pub(crate) const READY: i32 = 6;
    pub(crate) const FAILED: i32 = 7;
    pub(crate) const TEMPORARY: i32 = 8;
}

/// The engine's answer to one reference ([`Reply::UploadResolved`]).
pub(crate) struct RefAnswer {
    pub(crate) upload: Option<UploadView>,
    pub(crate) ambiguous: bool,
    pub(crate) temporary: bool,
    pub(crate) local: LocalCopy,
}

/// One reference as the pane renders it.
#[derive(Clone, Default, PartialEq, Eq)]
struct RefFace {
    state: i32,
    name: String,
    size: String,
    member: String,
    /// The availability word, the progress, or the state word - composed
    /// once here, in the active language.
    avail: String,
    can_fetch: bool,
    /// The share the `Herunterladen` verb addresses.
    id: Option<molt_core::MessageId>,
    /// The FULL checksum: the jump's needle and the byte read's argument.
    checksum: String,
}

/// The decoded pictures of one window, oldest first, bounded by
/// [`IMAGE_CACHE_BYTES`] of decoded RGBA.
#[derive(Default)]
struct ImageCache {
    entries: VecDeque<(String, slint::Image, usize)>,
    held: usize,
}

impl ImageCache {
    fn get(&mut self, checksum: &str) -> Option<slint::Image> {
        let at = self.entries.iter().position(|(c, ..)| c == checksum)?;
        let hit = self.entries.remove(at)?;
        let img = hit.1.clone();
        self.entries.push_back(hit);
        Some(img)
    }

    /// Insert, then evict from the front until the budget holds. The
    /// entry just inserted is never evicted; the checksums that were are
    /// returned, so their references can go back to asking.
    fn put(&mut self, checksum: &str, img: slint::Image, bytes: usize) -> Vec<String> {
        self.entries.retain(|(c, _, b)| {
            let keep = c != checksum;
            if !keep {
                self.held -= b;
            }
            keep
        });
        self.entries.push_back((checksum.to_string(), img, bytes));
        self.held += bytes;
        let mut dropped = Vec::new();
        while self.held > IMAGE_CACHE_BYTES && self.entries.len() > 1 {
            if let Some((c, _, b)) = self.entries.pop_front() {
                self.held -= b;
                dropped.push(c);
            }
        }
        dropped
    }
}

/// The window's file-reference cache.
#[derive(Default)]
struct FileRefs {
    /// The wiki base revision the answers were taken at.
    rev: i32,
    /// The open page's references: hex → written as a PICTURE.
    wanted: HashMap<String, bool>,
    by_hex: HashMap<String, RefFace>,
    /// Asked, no answer yet - so a sync never asks twice.
    asking: HashSet<String>,
    images: ImageCache,
    /// The language the stored words were composed in: an answer carries
    /// its rendered word, so a language switch has to re-resolve.
    words: String,
}

thread_local! {
    /// One cache per window thread; every entry point below runs on the
    /// UI thread, so it needs no lock.
    static FILE_REFS: RefCell<FileRefs> = RefCell::new(FileRefs::default());
}

/// The hex a span's destination names, or `None` when the run is no file
/// reference at all - the ONE test for "is this a file reference".
fn ref_hex(dest: &str) -> Option<String> {
    molt_core::wiki_refs::checksum_of(dest)
}

/// The cache key an ARGUMENT names: the pane hands the `upload:`
/// destination, the bridge's own re-resolve triggers hand a bare hex.
fn ref_key(dest: &str) -> String {
    ref_hex(dest).unwrap_or_else(|| dest.trim().to_ascii_lowercase())
}

/// The open page's references, and the ask for what is missing. Called
/// from [`sync_doc_face`], which runs only when the document's bytes
/// moved - so a keystroke never re-parses the page.
fn want_file_refs(s: &WikiState<'_>, body: &str, rev: i32) {
    let refs = molt_core::wiki_refs::file_refs(body);
    FILE_REFS.with(|c| {
        let mut c = c.borrow_mut();
        if c.rev != rev {
            // another base revision may name other files
            c.rev = rev;
            c.by_hex.clear();
            c.asking.clear();
        }
        // EVERY reference, malformed ones included - the pane renders
        // those as `unknown` and must not skip its own row walk for them
        c.wanted.clear();
        for r in refs {
            let entry = c.wanted.entry(r.hex).or_insert(false);
            *entry |= r.image;
        }
    });
    ask_missing(s);
}

/// Ask the engine for every reference of the open page that has neither
/// an answer nor a question in flight.
fn ask_missing(s: &WikiState<'_>) {
    let ask: Vec<slint::SharedString> = FILE_REFS.with(|c| {
        let mut c = c.borrow_mut();
        let fresh: Vec<String> = c
            .wanted
            .keys()
            .filter(|h| molt_core::wiki_refs::valid_hex(h))
            .filter(|h| !c.by_hex.contains_key(*h) && !c.asking.contains(*h))
            .cloned()
            .collect();
        for h in &fresh {
            c.asking.insert(h.clone());
        }
        fresh.into_iter().map(Into::into).collect()
    });
    if !ask.is_empty() {
        s.invoke_file_refs_wanted(ModelRc::new(VecModel::from(ask)));
    }
}

/// The cached face of one destination. An unresolved reference is
/// `REMOTE` with an empty name (the card then shows no verb); a
/// malformed hex is `UNKNOWN` and never reaches the engine.
fn face_of(c: &FileRefs, dest: &str, lex: &WikiFileWords) -> RefFace {
    let Some(hex) = ref_hex(dest) else {
        return RefFace {
            state: file_state::NONE,
            ..RefFace::default()
        };
    };
    if !molt_core::wiki_refs::valid_hex(&hex) {
        return RefFace {
            state: file_state::UNKNOWN,
            avail: lex.unknown.clone(),
            ..RefFace::default()
        };
    }
    c.by_hex.get(&hex).cloned().unwrap_or(RefFace {
        state: file_state::REMOTE,
        ..RefFace::default()
    })
}

/// The state words this layer composes, read once per pass out of the
/// Slint `Strings` global (the model itself stays language-free).
struct WikiFileWords {
    unknown: String,
    ambiguous: String,
    temporary: String,
    failed: String,
    decoding: String,
    loading: String,
    mirroring: String,
    gone: String,
    offline: String,
}

impl WikiFileWords {
    fn of(ui: &AppWindow) -> Self {
        let s = ui.global::<Strings>();
        WikiFileWords {
            unknown: s.get_mem_file_unknown().to_string(),
            ambiguous: s.get_mem_file_ambiguous().to_string(),
            temporary: s.get_mem_file_temporary().to_string(),
            failed: s.get_mem_file_failed().to_string(),
            decoding: s.get_mem_file_decoding().to_string(),
            loading: s.get_mem_file_loading().to_string(),
            mirroring: s.get_mem_file_mirroring().to_string(),
            gone: s.get_ou_gone().to_string(),
            offline: s.get_ou_offline().to_string(),
        }
    }
}

/// Write a block's file face. Returns whether anything moved.
fn fill_block_face(c: &mut FileRefs, b: &mut WikiBlock, lex: &WikiFileWords) -> bool {
    if b.kind != 5 {
        return false;
    }
    let dest = b.spans.row_data(0).map(|sp| sp.link.to_string()).unwrap_or_default();
    let f = face_of(c, &dest, lex);
    let img = (f.state == file_state::READY)
        .then(|| c.images.get(&f.checksum))
        .flatten()
        .unwrap_or_default();
    let moved = b.file_state != f.state
        || b.file_name != f.name.as_str()
        || b.file_size != f.size.as_str()
        || b.file_member != f.member.as_str()
        || b.file_avail != f.avail.as_str()
        || b.file_can_fetch != f.can_fetch
        || b.image != img;
    b.file_state = f.state;
    b.file_name = f.name.into();
    b.file_size = f.size.into();
    b.file_member = f.member.into();
    b.file_avail = f.avail.into();
    b.file_can_fetch = f.can_fetch;
    b.image = img;
    moved
}

/// Patch a block's file-link RUNS in place - the span model instance
/// survives, so the runs on screen are not re-created.
fn fill_span_faces(c: &mut FileRefs, spans: &ModelRc<WikiSpan>, lex: &WikiFileWords) {
    for i in 0..spans.row_count() {
        let Some(sp) = spans.row_data(i) else { continue };
        if ref_hex(sp.link.as_str()).is_none() {
            continue;
        }
        let f = face_of(c, sp.link.as_str(), lex);
        if sp.file_state == f.state
            && sp.file_name == f.name.as_str()
            && sp.file_size == f.size.as_str()
        {
            continue;
        }
        spans.set_row_data(
            i,
            WikiSpan {
                file_state: f.state,
                file_name: f.name.into(),
                file_size: f.size.into(),
                ..sp
            },
        );
    }
}

/// Apply the cache to the blocks already on screen, then ask for what is
/// still missing. Never a model swap: a resolve must not rebuild the page.
pub(crate) fn patch_file_rows(ui: &AppWindow) {
    // the common page has no reference at all: every sync would otherwise
    // walk its spans and compose nine words for nothing
    if FILE_REFS.with(|c| c.borrow().wanted.is_empty()) {
        return;
    }
    let s = ui.global::<WikiState>();
    let lex = WikiFileWords::of(ui);
    let blocks = s.get_blocks();
    FILE_REFS.with(|c| {
        let mut c = c.borrow_mut();
        // a question in flight keeps its claim: its answer composes in
        // the NEW language by itself, and re-asking would only double it
        if c.words != lex.unknown {
            c.words = lex.unknown.clone();
            c.by_hex.clear();
        }
        for i in 0..blocks.row_count() {
            let Some(mut b) = blocks.row_data(i) else { continue };
            fill_span_faces(&mut c, &b.spans, &lex);
            if fill_block_face(&mut c, &mut b, &lex) {
                blocks.set_row_data(i, b);
            }
        }
    });
    ask_missing(&s);
}

/// One reference's answer. Returns the FULL checksum whose bytes the
/// picture now wants.
pub(crate) fn file_resolved(
    ui: &AppWindow,
    hex: &str,
    answer: Option<RefAnswer>,
) -> Option<String> {
    let lex = WikiFileWords::of(ui);
    let read = FILE_REFS.with(|c| {
        let mut c = c.borrow_mut();
        c.asking.remove(hex);
        let as_image = c.wanted.get(hex).copied().unwrap_or(false);
        let (face, read) = ref_face(&mut c, answer, as_image, &lex);
        c.by_hex.insert(hex.to_string(), face);
        read
    });
    patch_file_rows(ui);
    read
}

/// The engine's answer, turned into the card the member sees.
fn ref_face(
    c: &mut FileRefs,
    answer: Option<RefAnswer>,
    as_image: bool,
    lex: &WikiFileWords,
) -> (RefFace, Option<String>) {
    let bad = |state: i32, word: &str| RefFace {
        state,
        avail: word.to_string(),
        ..RefFace::default()
    };
    // the engine refused: the read itself is the failure the card names
    let Some(a) = answer else {
        return (bad(file_state::FAILED, &lex.failed), None);
    };
    if a.ambiguous {
        return (bad(file_state::AMBIGUOUS, &lex.ambiguous), None);
    }
    let Some(u) = a.upload else {
        return (bad(file_state::UNKNOWN, &lex.unknown), None);
    };
    let mut f = RefFace {
        name: u.name.clone(),
        size: file_size_label(u.size),
        member: u.member.clone(),
        id: Some(u.id),
        checksum: u.checksum.clone(),
        ..RefFace::default()
    };
    if a.temporary {
        f.state = file_state::TEMPORARY;
        f.avail = lex.temporary.clone();
        return (f, None);
    }
    match u.download.as_ref().map(|d| (d.phase.as_str(), d.percent)) {
        Some(("requested" | "transferring", pct)) => {
            f.state = file_state::FETCHING;
            f.avail = format!("{} {pct} %", lex.loading);
            return (f, None);
        }
        Some(("failed", _)) => {
            f.state = file_state::FAILED;
            f.avail = lex.failed.clone();
            return (f, None);
        }
        _ => {}
    }
    if let LocalCopy::Partial { held, of } = a.local {
        // the mirror brings it by itself: the progress, and no verb (Q4)
        f.state = file_state::FETCHING;
        f.avail = format!(
            "{} {} %",
            lex.mirroring,
            held.saturating_mul(100).checked_div(of).unwrap_or(0)
        );
        return (f, None);
    }
    // an SVG share is never decoded (§4): it renders as a file link
    let picture = as_image && !u.name.to_ascii_lowercase().ends_with(".svg");
    if a.local != LocalCopy::None {
        if !picture {
            f.state = file_state::READY;
            return (f, None);
        }
        if c.images.get(&f.checksum).is_some() {
            f.state = file_state::READY;
            return (f, None);
        }
        // a legacy share carries no checksum - there is nothing the bytes
        // could be verified against, so they are never read
        if u.checksum.len() == molt_core::wiki_refs::HEX_FULL {
            f.state = file_state::DECODING;
            f.avail = lex.decoding.clone();
            let want = f.checksum.clone();
            return (f, Some(want));
        }
    }
    f.state = file_state::REMOTE;
    let relay_held = u.availability == "relay-held";
    f.can_fetch = u.available && (u.online || relay_held);
    f.avail = if u.availability == "gone" {
        lex.gone.clone()
    } else if !u.online && !relay_held {
        lex.offline.clone()
    } else {
        String::new()
    };
    (f, None)
}

/// The decoded picture (or its failure) for a reference.
pub(crate) fn image_decoded(
    ui: &AppWindow,
    hex: &str,
    decoded: Option<crate::images::DecodedImage>,
) {
    let lex = WikiFileWords::of(ui);
    FILE_REFS.with(|c| {
        let mut c = c.borrow_mut();
        let Some(checksum) = c.by_hex.get(hex).map(|f| f.checksum.clone()) else {
            return;
        };
        match decoded {
            Some(d) => {
                let bytes = d.bytes();
                let dropped = c.images.put(&checksum, d.into_image(), bytes);
                // an evicted picture's reference goes back to asking
                c.by_hex.retain(|_, f| !dropped.contains(&f.checksum));
                if let Some(f) = c.by_hex.get_mut(hex) {
                    f.state = file_state::READY;
                    f.avail.clear();
                }
            }
            None => {
                if let Some(f) = c.by_hex.get_mut(hex) {
                    f.state = file_state::FAILED;
                    f.avail = lex.failed.clone();
                }
            }
        }
    });
    patch_file_rows(ui);
}

/// Forget one reference's answer and ask again (the `failed` card's verb,
/// and every re-resolve trigger below).
pub(crate) fn forget_file_ref(ui: &AppWindow, dest: &str) {
    let hex = ref_key(dest);
    FILE_REFS.with(|c| {
        let mut c = c.borrow_mut();
        c.by_hex.remove(&hex);
        c.asking.remove(&hex);
    });
    patch_file_rows(ui);
}

/// A transfer moved: the reference naming that share resolves again.
pub(crate) fn transfer_touched(ui: &AppWindow, id: molt_core::MessageId) {
    let hit = FILE_REFS.with(|c| {
        c.borrow()
            .by_hex
            .iter()
            .find(|(_, f)| f.id == Some(id))
            .map(|(h, _)| h.clone())
    });
    if let Some(hex) = hit {
        forget_file_ref(ui, &hex);
    }
}

/// A surfaces push: a mirror that completed brings a referenced file's
/// bytes without the wiki asking for them.
pub(crate) fn uploads_pushed(ui: &AppWindow, uploads: &[crate::surfaces::UploadRowData]) {
    let stale: Vec<String> = FILE_REFS.with(|c| {
        let c = c.borrow();
        uploads
            .iter()
            // a legacy share has no checksum, and "" would match every
            // reference whose answer has not landed yet
            .filter(|u| {
                !u.checksum_full.is_empty() && u.mirror_of > 0 && u.mirror_held == u.mirror_of
            })
            .filter_map(|u| {
                c.by_hex
                    .iter()
                    .find(|(_, f)| {
                        f.checksum == u.checksum_full
                            && matches!(f.state, file_state::REMOTE | file_state::FETCHING)
                    })
                    .map(|(h, _)| h.clone())
            })
            .collect()
    });
    for hex in stale {
        forget_file_ref(ui, &hex);
    }
}

/// The share a reference names, for the `Herunterladen` verb.
fn file_ref_share(dest: &str) -> Option<molt_core::MessageId> {
    let hex = ref_key(dest);
    FILE_REFS.with(|c| c.borrow().by_hex.get(&hex).and_then(|f| f.id))
}

/// The jump's needle and view (§3.5): the FULL checksum where the answer
/// carries one, and the table the row actually lives in.
fn file_ref_jump(dest: &str) -> (String, &'static str) {
    let hex = ref_key(dest);
    FILE_REFS.with(|c| {
        let c = c.borrow();
        match c.by_hex.get(&hex) {
            Some(f) if !f.checksum.is_empty() => (
                f.checksum.clone(),
                if f.state == file_state::TEMPORARY { "uploads" } else { "persistent" },
            ),
            _ => (hex, "persistent"),
        }
    })
}

#[cfg(test)]
pub(crate) fn reset_file_refs() {
    FILE_REFS.with(|c| *c.borrow_mut() = FileRefs::default());
}

/// The refusal a failed link commit left, in the ACTIVE language: the
/// model deals in codes so it stays language-free (`wiki::LINK_ERR_*`).
fn link_error_text(ui: &AppWindow, code: &str) -> String {
    let s = ui.global::<Strings>();
    match code {
        wiki::LINK_ERR_HEADER => s.get_mem_link_err_header().to_string(),
        wiki::LINK_ERR_QUALIFIED => s.get_mem_link_err_qual().to_string(),
        wiki::LINK_ERR_QUAL_SYNTAX => s.get_mem_link_err_qsyntax().to_string(),
        wiki::LINK_ERR_BODY => s.get_mem_link_err_body().to_string(),
        _ => String::new(),
    }
}

/// The navigator keeps ONE context menu per row KIND and builds it from
/// the MARKED row, so the row's facts travel as scalars
/// (`docs_archive/ui/wiki_pane_performance.md` F3).
fn sync_nav_menu(s: &WikiState<'_>, nav: &[WikiNavRow]) {
    let m = nav.iter().find(|r| r.marked);
    s.set_marked_is_folder(m.is_some_and(|r| r.is_folder));
    s.set_marked_id(m.map_or(0, |r| r.id));
    s.set_marked_path(m.map(|r| r.path.clone()).unwrap_or_default());
    s.set_marked_depth(m.map_or(0, |r| r.depth));
    s.set_marked_status(m.map_or(0, |r| r.status));
    // the windowed navigator holds no element for an off-screen row, so
    // the row that starts renaming has to be scrolled to
    s.set_nav_renaming_row(
        nav.iter()
            .position(|r| r.renaming)
            .and_then(|p| i32::try_from(p).ok())
            .unwrap_or(-1),
    );
}

/// The debounced draft persist (WP-D), the half of a sync that talks to
/// the engine. The WHOLE model as JSON (21 ms on a held base) is
/// serialized only when the model actually moved and the window is over:
/// the keystroke echo never pays for a comparison that is thrown away
/// 2 s out of 2 s (F4a). Returns whether a change is still WAITING for
/// the window to close - [`arm_draft_flush`] is what ends that wait.
/// `forced` is the flush timer speaking: the window is over by
/// construction there, and the mocked clock the GUI tests run on never
/// moves an `Instant`.
fn sync_draft(ui: &AppWindow, w: &wiki::Wiki, face: &mut FaceState, forced: bool) -> bool {
    let gen = w.generation();
    if gen == face.draft_gen {
        return false;
    }
    if !forced && face.draft_at.elapsed() < DRAFT_WINDOW {
        return true;
    }
    face.draft_gen = gen;
    let draft = w.to_draft();
    if draft != face.draft {
        face.draft = draft.clone();
        face.draft_at = std::time::Instant::now();
        ui.invoke_wiki_draft_save(draft.into());
    }
    false
}

/// Push the wiki model into the `WikiState` global after every mutation:
/// the small models patch in place, and the parts that are neither cheap
/// nor on screen are skipped on the keys `face` carries
/// (`docs_archive/ui/wiki_pane_performance.md` F4). The editor buffer is the
/// oldest of those keys: `raw` is rewritten only when the active doc or
/// the edit mode changes (`raw_for`), never on the keystroke echo — a
/// mid-typing rewrite fights the caret. A modal write clears it itself.
fn sync_wiki(ui: &AppWindow, w: &wiki::Wiki, face: &mut FaceState) {
    let gen = w.generation();
    face.draft_pending = sync_draft(ui, w, face, false);
    let s = ui.global::<WikiState>();
    let nav: Vec<WikiNavRow> = w
        .nav_rows()
        .into_iter()
        .map(|r| WikiNavRow {
            is_folder: r.kind == wiki::RowKind::Folder,
            id: i32::try_from(r.id).unwrap_or(0),
            label: r.label.into(),
            path: r.path.into(),
            depth: i32::try_from(r.depth).unwrap_or(0),
            open: r.open,
            marked: r.marked,
            status: wiki_status_code(r.status),
            renaming: r.renaming,
        })
        .collect();
    sync_nav_menu(&s, &nav);
    sync_model(&s.get_nav_rows(), nav, PartialEq::eq, |m| s.set_nav_rows(m));
    let tabs: Vec<WikiTabRow> = w
        .tab_rows()
        .into_iter()
        .map(|t| WikiTabRow {
            id: i32::try_from(t.id).unwrap_or(0),
            label: t.label.into(),
            active: t.active,
            status: wiki_status_code(t.status),
        })
        .collect();
    sync_model(&s.get_tabs(), tabs, PartialEq::eq, |m| s.set_tabs(m));
    s.set_has_marked(w.has_marked());
    s.set_editing(w.editing);
    // reveal also SCROLLS to the row, so it stays useful once the row is
    // marked but out of view
    s.set_can_reveal(w.active_id().is_some());
    // the changeset panel: NET counts + the action stack (visibility is
    // the stack's non-emptiness, .slint-side)
    let c = w.changeset_counts();
    s.set_cs_added(i32::try_from(c.added).unwrap_or(0));
    s.set_cs_deleted(i32::try_from(c.deleted).unwrap_or(0));
    s.set_cs_moved(i32::try_from(c.moved).unwrap_or(0));
    s.set_cs_lines(i32::try_from(c.lines).unwrap_or(0));
    let cs_rows: Vec<WikiChangeRow> = w
        .stack_rows()
        .into_iter()
        .map(|r| WikiChangeRow {
            kind: match r.kind {
                wiki::ChangeKind::Created => 0,
                wiki::ChangeKind::CreatedFolder => 1,
                wiki::ChangeKind::Renamed => 2,
                wiki::ChangeKind::Moved => 3,
                wiki::ChangeKind::Deleted => 4,
                wiki::ChangeKind::Edited => 5,
            },
            label: r.label.into(),
        })
        .collect();
    sync_model(&s.get_cs_rows(), cs_rows, PartialEq::eq, |m| s.set_cs_rows(m));
    // the two authoring modals' drafts
    let tag_rows: Vec<slint::SharedString> =
        w.tag_rows().iter().map(|t| t.as_str().into()).collect();
    if tag_rows.len() == s.get_tag_rows().row_count() {
        // same rows, possibly new text: patch in place, or the field the
        // member is typing in loses its focus on every keystroke
        sync_model(&s.get_tag_rows(), tag_rows, PartialEq::eq, |m| s.set_tag_rows(m));
    } else {
        // a row came or went: the surviving inputs must be REBUILT, or a
        // removed row leaves its neighbour showing the text it no longer
        // holds (a one-way binding an input has already written to is
        // dead)
        s.set_tag_rows(ModelRc::new(VecModel::from(tag_rows)));
    }
    s.set_tag_ready(w.tag_ready());
    s.set_link_name(w.link_name().into());
    s.set_link_target(w.link_target().into());
    s.set_link_filter(w.link_filter().into());
    s.set_link_custom(w.link_custom().into());
    s.set_link_ready(w.link_ready());
    s.set_link_header(w.link_header());
    s.set_link_qualifiers(w.link_qualifiers().into());
    s.set_link_error(link_error_text(ui, w.link_error()).into());
    s.set_can_write_header(w.can_write_header());
    s.set_can_write_body(w.can_write_body());
    let targets: Vec<slint::SharedString> =
        w.link_targets().into_iter().map(Into::into).collect();
    sync_model(&s.get_link_targets(), targets, PartialEq::eq, |m| s.set_link_targets(m));
    let rels: Vec<WikiRelation> = w
        .link_relations()
        .into_iter()
        .map(|(key, on)| WikiRelation { key: key.into(), on })
        .collect();
    sync_model(&s.get_link_relations(), rels, PartialEq::eq, |m| {
        s.set_link_relations(m);
    });
    // the details modal shows the WHOLE change — rebuilt when the model
    // moved, not once per sync into a modal nobody opened (F4)
    if face.patch_gen != Some(gen) {
        face.patch_gen = Some(gen);
        s.set_cs_patch(w.build_patch().unwrap_or_default().into());
    }
    s.set_cs_vote_queued(w.vote_pending());
    // …and the BYTES any changed document still lacks (§4.10), whether or
    // not a tab is open: a delete or move made in the navigator needs the
    // ratified text just as much, and asking only for the OPEN document
    // left those waiting forever. CLAIMED, not repeated: a path in flight
    // is skipped until it lands or the fetch gives up (F5).
    if let Some(want) = w.wants_content() {
        s.invoke_content_wanted(want.into());
    }
    if let Some(doc) = w.active() {
        let id = doc.id;
        let path_changed = s.get_doc_path().as_str() != doc.path;
        s.set_doc_open(true);
        s.set_doc_path(doc.path.clone().into());
        // the SUBJECT of an inline relation: the claim belongs to this
        // page, whatever the sentence's grammar suggests
        s.set_doc_title(w.title_of(&doc.path).into());
        // a ratified document has no author/version/age of its own: the
        // line would read " ·  · " and the pane hides it instead
        let meta = if doc.author.is_empty() && doc.ver.is_empty() && doc.when.is_empty() {
            String::new()
        } else {
            format!("{} · {} · {}", doc.author, doc.ver, doc.when)
        };
        s.set_doc_meta(meta.into());
        s.set_doc_status(wiki_status_code(doc.status()));
        // The block view, the infobox and the link list are ONE face over
        // the document's bytes, and surfaces.slint shows it only while
        // `doc-open && !editing`. So it is rebuilt when that key moved -
        // never per keystroke, where the markdown parse of working AND
        // base text plus a Myers diff went into a view that is off screen
        // (F4). Leaving the editor is a key change, so the blocks are
        // fresh the moment they are visible again.
        let doc_key = if w.editing { None } else { w.face_key() };
        if let Some(key) = doc_key {
            if face.doc_key != Some(key) {
                face.doc_key = Some(key);
                sync_doc_face(&s, w, id);
            }
        }
        // the in-edges come from the engine, so they are fetched only when
        // the open document actually changed
        if path_changed {
            s.set_backlinks(ModelRc::new(VecModel::from(Vec::<slint::SharedString>::new())));
            s.invoke_backlinks_wanted(doc.path.clone().into());
        }
        if face.raw_for != Some((id, w.editing)) {
            s.set_raw(doc.raw.clone().into());
            face.raw_for = Some((id, w.editing));
        }
        patch_file_rows(ui);
    } else {
        face.doc_key = None;
        s.set_doc_open(false);
        s.set_doc_path("".into());
        s.set_doc_title("".into());
        s.set_doc_meta("".into());
        s.set_doc_status(0);
        sync_model(&s.get_blocks(), Vec::new(), wiki_block_eq, |m| s.set_blocks(m));
        sync_model(&s.get_links(), Vec::new(), PartialEq::eq, |m| s.set_links(m));
        sync_model(&s.get_props(), Vec::new(), PartialEq::eq, |m| s.set_props(m));
        sync_model(&s.get_tag_pills(), Vec::new(), PartialEq::eq, |m| s.set_tag_pills(m));
        s.set_can_add_tags(false);
        sync_model(&s.get_backlinks(), Vec::new(), PartialEq::eq, |m| {
            s.set_backlinks(m);
        });
        face.raw_for = None;
    }
}

/// The open document's rendered face: the diffed blocks, its links and
/// its header as infobox rows. Split out of [`sync_wiki`] because it is
/// the expensive half and runs only when the document's bytes moved.
fn sync_doc_face(s: &WikiState<'_>, w: &wiki::Wiki, id: wiki::DocId) {
    let blocks: Vec<WikiBlock> = w
        .preview(id)
        .into_iter()
        .map(|(b, st)| WikiBlock {
            kind: i32::from(b.kind),
            text: b.text.into(),
            status: match st {
                wiki::BlockStatus::Same => 0,
                wiki::BlockStatus::Added => 1,
                wiki::BlockStatus::Changed => 2,
                wiki::BlockStatus::Removed => 3,
            },
            spans: ModelRc::new(VecModel::from(
                b.spans
                    .into_iter()
                    .map(|sp| WikiSpan {
                        text: sp.text.into(),
                        link: sp.link.into(),
                        rel: sp.rel.into(),
                        ..WikiSpan::default()
                    })
                    .collect::<Vec<_>>(),
            )),
            ..WikiBlock::default()
        })
        .collect();
    sync_model(&s.get_blocks(), blocks, wiki_block_eq, |m| s.set_blocks(m));
    // the page's `upload:` references, once per bytes change - the face
    // itself is patched by `patch_file_rows` on every sync
    if let Some(doc) = w.active() {
        want_file_refs(s, wiki::body_of(&doc.raw), s.get_base_rev());
    }
    let links: Vec<slint::SharedString> = w.links(id).into_iter().map(Into::into).collect();
    sync_model(&s.get_links(), links, PartialEq::eq, |m| s.set_links(m));
    let rows: Vec<WikiProp> = w
        .infobox(id)
        .into_iter()
        .map(|r| WikiProp {
            key: r.key.into(),
            value: r.value.into(),
            link: r.link.into(),
            status: i32::from(r.status),
            hue: i32::from(r.hue),
        })
        .collect();
    // tags read as pills, every other key as a labelled row - two
    // models, so each band knows whether it has anything to show
    let (tags, props): (Vec<WikiProp>, Vec<WikiProp>) = rows.into_iter().partition(|r| r.hue >= 0);
    sync_model(&s.get_props(), props, PartialEq::eq, |m| s.set_props(m));
    sync_model(&s.get_tag_pills(), tags, PartialEq::eq, |m| s.set_tag_pills(m));
    s.set_can_add_tags(w.can_add_tags());
}

/// A callback ran: the model moved, so bump its generation and push the
/// face. Every wiki verb is a mutation by construction, which makes this
/// the ONE place the sync's "has anything changed" key has to be kept
/// honest (`docs_archive/ui/wiki_pane_performance.md` F4).
fn sync_after(ui: &AppWindow, m: &Rc<RefCell<wiki::Wiki>>, face: &Rc<RefCell<FaceState>>) {
    m.borrow_mut().touch();
    sync_wiki(ui, &m.borrow(), &mut face.borrow_mut());
    arm_draft_flush(ui, m, face);
}

/// End the 2 s draft window even when nothing else happens: ONE timer per
/// window (`flush_armed`), so a burst of keystrokes arms one and not
/// twenty, and the change a member typed before walking away still lands.
/// The face itself needs no re-push - nothing mutated - so the timer runs
/// the draft half alone.
fn arm_draft_flush(ui: &AppWindow, m: &Rc<RefCell<wiki::Wiki>>, face: &Rc<RefCell<FaceState>>) {
    let left = {
        let mut f = face.borrow_mut();
        if !f.draft_pending || f.flush_armed {
            return;
        }
        f.flush_armed = true;
        DRAFT_WINDOW.saturating_sub(f.draft_at.elapsed())
    };
    #[cfg(test)]
    wiki::counters::bump_flush();
    let weak = ui.as_weak();
    let m = m.clone();
    let face = face.clone();
    slint::Timer::single_shot(left, move || {
        let Some(ui) = weak.upgrade() else { return };
        let w = m.borrow();
        let mut f = face.borrow_mut();
        f.flush_armed = false;
        f.draft_pending = sync_draft(&ui, &w, &mut f, true);
    });
}

/// Wire the Multisig-Wiki's Rust state machine ([`wiki`]) to the
/// `WikiState` global: every action callback mutates the model, then
/// re-syncs the face ([`sync_after`]). UI-local by design — of the wiki
/// verbs only the changeset VOTE talks to the engine, and that one is
/// wired separately ([`wire_wiki_vote`]) where the handles live. Returns
/// the shared model + face state for that wiring.
pub(crate) fn wire_wiki(ui: &AppWindow) -> (Rc<RefCell<wiki::Wiki>>, Rc<RefCell<FaceState>>) {
    // the REAL base arrives from the engine read (set_base) — until then
    // the honest empty state shows; the sample stays a test fixture
    let model = Rc::new(RefCell::new(wiki::Wiki::empty()));
    let last: Rc<RefCell<FaceState>> = Rc::new(RefCell::new(FaceState::new()));
    sync_wiki(ui, &model.borrow(), &mut last.borrow_mut());
    let g = ui.global::<WikiState>();

    // one shape for every handler: mutate under the borrow, then push the
    // new face (the toast-carrying handlers below spell it out instead)
    macro_rules! act {
        ($setter:ident, |$w:ident $(, $arg:ident : $ty:ty)*| $body:expr) => {{
            let m = model.clone();
            let la = last.clone();
            let weak = ui.as_weak();
            g.$setter(move |$($arg: $ty),*| {
                let Some(ui) = weak.upgrade() else { return };
                {
                    let mut $w = m.borrow_mut();
                    $body;
                }
                sync_after(&ui, &m, &la);
            });
        }};
    }

    act!(on_nav_mark, |w, id: i32| w.mark(wiki_doc_id(id)));
    act!(on_nav_open, |w, id: i32| w.open(wiki_doc_id(id)));
    act!(on_nav_toggle_folder, |w, name: slint::SharedString| w
        .toggle_folder(&name));
    act!(on_nav_rename_start, |w, id: i32| w.rename_start(wiki_doc_id(id)));
    // deferred (slint#6426 class): Escape fires from the rename row's own
    // FocusScope, and the cancel tears that row variant down
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_rename_cancel(move || {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                m.borrow_mut().rename_cancel();
                sync_after(&ui, &m, &la);
            });
        });
    }
    // deferred (slint#6426 class): fired from the row's own context menu,
    // and the delete restructures that row
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_delete(move |id| {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                m.borrow_mut().delete(wiki_doc_id(id));
                sync_after(&ui, &m, &la);
            });
        });
    }
    act!(on_tab_focus, |w, id: i32| w.focus(wiki_doc_id(id)));
    act!(on_tab_close, |w, id: i32| w.close_tab(wiki_doc_id(id)));
    act!(on_tab_close_all, |w| w.close_all());
    act!(on_tab_close_right, |w, id: i32| w
        .close_right_of(wiki_doc_id(id)));
    act!(on_tab_close_left, |w, id: i32| w.close_left_of(wiki_doc_id(id)));
    act!(on_tab_step, |w, delta: i32| w.step_tab(delta));
    act!(on_close_active, |w| w.close_active());
    act!(on_new_file, |w| {
        let _ = w.new_file();
    });
    act!(on_new_folder, |w| {
        let _ = w.new_folder();
    });
    act!(on_fold_all, |w, open: bool| w.set_all_folders(open));
    // hand-wired: the row index goes to the navigator AFTER the rows it
    // indexes are in the model
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_reveal(move || {
            let Some(ui) = weak.upgrade() else { return };
            let at = m.borrow_mut().reveal();
            sync_after(&ui, &m, &la);
            if let Some(at) = at {
                ui.global::<WikiState>()
                    .set_nav_scroll_to(i32::try_from(at).unwrap_or(-1));
            }
        });
    }
    act!(on_open_marked, |w| w.open_marked());
    act!(on_delete_marked, |w| w.delete_marked());
    act!(on_rename_marked, |w| w.rename_marked());
    act!(on_nav_mark_folder, |w, path: slint::SharedString| w
        .mark_folder(&path));
    act!(on_edit_toggle, |w| {
        if w.active_id().is_some() {
            w.editing = !w.editing;
        }
    });
    act!(on_edited, |w, text: slint::SharedString| {
        if let Some(id) = w.active_id() {
            w.set_raw(id, &text);
        }
    });
    // ---- the two authoring modals ------------------------------------------
    act!(on_tag_open, |w| w.tag_open());
    act!(on_tag_add, |w, value: slint::SharedString| w.tag_add(&value));
    act!(on_tag_set, |w, i: i32, value: slint::SharedString| {
        w.tag_set(usize::try_from(i).unwrap_or(0), &value);
    });
    act!(on_tag_remove, |w, i: i32| {
        w.tag_remove(usize::try_from(i).unwrap_or(0));
    });
    // the two modal writes change the text UNDER an open editor, so they
    // drop the raw guard for their own sync: without it the editor keeps
    // showing the old bytes and the next keystroke pushes them back over
    // the write. Only on a write — a refusal changed nothing.
    macro_rules! wrote {
        ($setter:ident, |$w:ident| $body:expr) => {{
            let m = model.clone();
            let la = last.clone();
            let weak = ui.as_weak();
            g.$setter(move || {
                let Some(ui) = weak.upgrade() else { return };
                let wrote = {
                    let mut $w = m.borrow_mut();
                    $body
                };
                if wrote {
                    la.borrow_mut().raw_for = None;
                }
                sync_after(&ui, &m, &la);
            });
        }};
    }
    wrote!(on_tag_commit, |w| w.tag_commit());
    act!(on_link_open, |w, target: slint::SharedString| w.link_open(&target));
    act!(on_link_close, |w| w.link_close());
    act!(on_relation_vocab, |w, keys: ModelRc<slint::SharedString>| {
        w.set_relation_vocab(keys.iter().map(|k| k.to_string()).collect());
    });
    act!(on_set_link_name, |w, v: slint::SharedString| w.set_link_name(&v));
    act!(on_set_link_target, |w, v: slint::SharedString| w.set_link_target(&v));
    act!(on_set_link_filter, |w, v: slint::SharedString| w.set_link_filter(&v));
    act!(on_set_link_custom, |w, v: slint::SharedString| w.set_link_custom(&v));
    act!(on_set_link_header, |w, on: bool| w.set_link_header(on));
    act!(on_set_link_qualifiers, |w, v: slint::SharedString| w
        .set_link_qualifiers(&v));
    // NOT through `act!`: the caret fires on every keystroke, arrow key
    // and click, and a whole-face sync per caret move would rebuild the
    // nav model, the blocks and the patch twice per keystroke. Nothing on
    // the face reads it - the next commit does.
    {
        let m = model.clone();
        g.on_set_cursor(move |at| {
            m.borrow_mut().set_cursor(usize::try_from(at).unwrap_or(0));
        });
    }
    act!(on_link_add_custom, |w| {
        w.link_add_custom();
    });
    act!(on_link_toggle, |w, key: slint::SharedString| w.link_toggle(&key));
    wrote!(on_link_commit, |w| w.link_commit());
    act!(on_open_link, |w, target: slint::SharedString| {
        // a dead link is a no-op — the preview stays put
        let _ = w.open_link(&target);
    });
    // workspace switch: the wiki model is per republic — reset, then load
    // the stored draft; the base follows over the normal bridge
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_workspace_changed(move || {
            let Some(ui) = weak.upgrade() else { return };
            *m.borrow_mut() = wiki::Wiki::empty();
            la.borrow_mut().raw_for = None;
            sync_after(&ui, &m, &la);
            ui.invoke_wiki_draft_load();
        });
    }
    // the stored draft arrived: restore it, then rebase on the current
    // bridge properties (a base that moved while closed reconciles like a
    // live move); seed the auto-save guard so the load does not re-save
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_draft_loaded(move |draft| {
            let Some(ui) = weak.upgrade() else { return };
            {
                let mut w = m.borrow_mut();
                let _ = w.restore_draft(&draft);
                let gs = ui.global::<WikiState>();
                let base: Vec<(String, Option<String>)> = gs
                    .get_base_docs()
                    .iter()
                    .map(|d| {
                        // `loaded` tells an empty document from one whose
                        // bytes have not been fetched (§4.10)
                        let content = d.loaded.then(|| d.content.to_string());
                        (d.path.to_string(), content)
                    })
                    .collect();
                let rev = u64::try_from(gs.get_base_rev()).unwrap_or(0);
                w.set_base(&base, rev);
                w.touch();
                let mut f = la.borrow_mut();
                f.draft = w.to_draft();
                f.draft_at = std::time::Instant::now();
                f.draft_gen = w.generation();
            }
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
        });
    }

    // rescue: a retired patch's changeset returns as local drafts —
    // best-effort per file, honest toast, and the wiki opens so the
    // rescued work is on screen
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        ui.on_wiki_rescue(move |patch| {
            let Some(ui) = weak.upgrade() else { return };
            let (applied, skipped) = m.borrow_mut().rescue_patch(&patch);
            sync_after(&ui, &m, &la);
            let word = ui.global::<Strings>().get_mem_toast_rescued();
            ui.invoke_show_toast(format!("⤵ {word} {applied}/{}", applied + skipped).into());
            ui.invoke_select_view("memory".into(), "brain".into());
        });
    }

    // one document's ratified bytes arrived (the lazy base's other half)
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_content_arrived(move |path, content| {
            let Some(ui) = weak.upgrade() else { return };
            // the flag is TAKEN here, before the vote runs: a second
            // arrival must not propose the same patch again
            let fire = {
                let mut w = m.borrow_mut();
                w.load_base(&path, &content);
                let fire = w.vote_pending() && w.unloaded_changes().is_empty();
                if fire {
                    w.clear_vote();
                }
                fire
            };
            sync_after(&ui, &m, &la);
            if fire {
                ui.global::<WikiState>().invoke_cs_vote();
            }
        });
    }

    // the bytes could not be read at all: a queued vote must be released
    // with a reason rather than wait forever
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_content_failed(move |path| {
            let Some(ui) = weak.upgrade() else { return };
            // the attempt is over: the path is askable again, and the next
            // sync (a member action) retries it - re-asking from HERE
            // would be a fetch loop nobody started (F5)
            m.borrow_mut().content_failed(&path);
            if !m.borrow().vote_pending() {
                return;
            }
            m.borrow_mut().clear_vote();
            sync_after(&ui, &m, &la);
            let msg = ui.global::<Strings>().get_mem_toast_content_failed();
            ui.invoke_show_toast_error(msg);
        });
    }

    // the surfaces mirror just wrote the folded base to the bridge
    // properties — rebase the model on it (local work is kept). Hand-wired
    // (not act!): the handler must read the WikiState global through the
    // UPGRADED window, which the macro's hygiene cannot name.
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_base_arrived(move || {
            let Some(ui) = weak.upgrade() else { return };
            {
                let gs = ui.global::<WikiState>();
                let base: Vec<(String, Option<String>)> = gs
                    .get_base_docs()
                    .iter()
                    .map(|d| {
                        // `loaded` tells an empty document from one whose
                        // bytes have not been fetched (§4.10)
                        let content = d.loaded.then(|| d.content.to_string());
                        (d.path.to_string(), content)
                    })
                    .collect();
                let rev = u64::try_from(gs.get_base_rev()).unwrap_or(0);
                m.borrow_mut().set_base(&base, rev);
            }
            sync_after(&ui, &m, &la);
        });
    }

    // the folder verbs — all deferred (slint#6426 class: each fires from
    // the folder row's own menu/input/drag and restructures that row)
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_new_file_in(move |folder| {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                m.borrow_mut().new_file_in(&folder);
                sync_after(&ui, &m, &la);
            });
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_folder_rename_start(move |folder| {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                m.borrow_mut().rename_folder_start(&folder);
                sync_after(&ui, &m, &la);
            });
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_folder_rename_commit(move |old, name| {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                {
                    let mut w = m.borrow_mut();
                    // Enter races the teardown after an Escape cancel —
                    // only act while the model still renames this folder
                    if w.renaming_folder() == Some(old.as_str()) {
                        if let Err(e) = w.rename_folder_commit(&old, &name) {
                            ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
                        }
                    }
                }
                sync_after(&ui, &m, &la);
            });
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_folder_delete(move |folder| {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                m.borrow_mut().delete_folder(&folder);
                sync_after(&ui, &m, &la);
            });
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_folder_drop(move |folder, row| {
            // target resolved synchronously, mutation deferred — see nav-drop
            let row = usize::try_from(row).unwrap_or(usize::MAX);
            let target = m.borrow().drop_target(row);
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                {
                    let mut w = m.borrow_mut();
                    if let Err(e) = w.move_folder_under(&folder, target.as_deref()) {
                        ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
                    }
                }
                sync_after(&ui, &m, &la);
            });
        });
    }

    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_move_root(move |id| {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                {
                    let mut w = m.borrow_mut();
                    if let Err(e) = w.move_to(wiki_doc_id(id), None) {
                        ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
                    }
                }
                sync_after(&ui, &m, &la);
            });
        });
    }

    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_new_folder_in(move |parent| {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                {
                    let mut w = m.borrow_mut();
                    if let Err(e) = w.new_folder_in(&parent) {
                        ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
                    }
                }
                sync_after(&ui, &m, &la);
            });
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_folder_move_root(move |folder| {
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                {
                    let mut w = m.borrow_mut();
                    if let Err(e) = w.move_folder_to_root(&folder) {
                        ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
                    }
                }
                sync_after(&ui, &m, &la);
            });
        });
    }

    // rename commit + drag drop carry a refusal the user must see
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_rename_commit(move |id, name| {
            // deferred (slint#6426 class): Enter fires from the rename
            // row's own FocusScope, and the commit swaps that row variant
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                {
                    let mut w = m.borrow_mut();
                    let id = wiki_doc_id(id);
                    // Enter-commit races the row teardown after an Escape
                    // cancel — only act while the model still renames this id
                    if w.renaming() == Some(id) {
                        if let Err(e) = w.rename_commit(id, &name) {
                            ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
                        }
                    }
                }
                sync_after(&ui, &m, &la);
            });
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_drop(move |id, row| {
            // the TARGET resolves NOW — the row index addresses what the
            // user saw; a model re-sync inside the deferral gap must not
            // re-point it. Only the MUTATION defers (slint#6426: tearing
            // rows down inside their own pointer callback panics the
            // interpreter — live crash 2026-08-15).
            let row = usize::try_from(row).unwrap_or(usize::MAX);
            let target = m.borrow().drop_target(row);
            let m = m.clone();
            let la = la.clone();
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                {
                    let mut w = m.borrow_mut();
                    if let Err(e) = w.move_to(wiki_doc_id(id), target.as_deref()) {
                        ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
                    }
                }
                sync_after(&ui, &m, &la);
            });
        });
    }

    // the changeset verbs rewrite doc content UNDER an open editor, so
    // each drops the raw-guard before syncing (a kept guard would leave
    // the TextInput on the pre-revert bytes and the next keystroke would
    // faithfully re-record them)
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_cs_undo(move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Err(e) = m.borrow_mut().undo() {
                ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
            }
            la.borrow_mut().raw_for = None;
            sync_after(&ui, &m, &la);
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_cs_revert(move || {
            let Some(ui) = weak.upgrade() else { return };
            m.borrow_mut().revert_all();
            la.borrow_mut().raw_for = None;
            sync_after(&ui, &m, &la);
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_nav_revert(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            if let Err(e) = m.borrow_mut().revert_doc(wiki_doc_id(id)) {
                ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
            }
            la.borrow_mut().raw_for = None;
            sync_after(&ui, &m, &la);
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_revert_active(move || {
            let Some(ui) = weak.upgrade() else { return };
            let active = m.borrow().active_id();
            if let Some(id) = active {
                if let Err(e) = m.borrow_mut().revert_doc(id) {
                    ui.invoke_show_toast_error(localize_wiki_err(ui.get_lang_index(), &e).into());
                }
            }
            la.borrow_mut().raw_for = None;
            sync_after(&ui, &m, &la);
        });
    }
    // copy-link: markdown link markup for the file, ready to paste into
    // the editor (goes through the window's one clipboard route)
    {
        let m = model.clone();
        let weak = ui.as_weak();
        g.on_copy_link(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let markup = m.borrow().link_markup(wiki_doc_id(id));
            if let Some(markup) = markup {
                ui.invoke_copy_text(markup.into());
                let msg = ui.global::<Strings>().get_mem_toast_link_md();
                ui.invoke_show_toast(msg);
            }
        });
    }

    (model, last)
}

/// Push one wiki patch into the `PatchView` global: the file navigator
/// (markers included) and the `sel`ected file's rendered diff rows.
/// `for_id` stamps which proposal the parse belongs to (0 clears).
pub(crate) fn patch_view_sync(ui: &AppWindow, patch: &str, sel: usize, for_id: i32) {
    let g = ui.global::<PatchView>();
    g.set_for_id(for_id);
    let files = patchview::parse_patch(patch);
    let sel = sel.min(files.len().saturating_sub(1));
    match files.get(sel) {
        Some(f) => {
            g.set_sel_label(f.display_path().into());
            // the header names the whole move (from → to); the nav rows
            // keep the short marker
            g.set_sel_marker(f.header_marker().into());
            g.set_sel_status(i32::from(f.status()));
        }
        None => {
            g.set_sel_label("".into());
            g.set_sel_marker("".into());
            g.set_sel_status(0);
        }
    }
    let nav: Vec<PatchNavRow> = files
        .iter()
        .enumerate()
        .map(|(i, f)| PatchNavRow {
            label: f.display_path().into(),
            marker: f.marker().into(),
            status: i32::from(f.status()),
            selected: i == sel,
        })
        .collect();
    let rows: Vec<DiffRow> = files
        .get(sel)
        .map(|f| {
            patchview::file_rows(f)
                .into_iter()
                .map(|r| DiffRow {
                    segs: ModelRc::new(VecModel::from(
                        r.segs
                            .into_iter()
                            .map(|s| DiffSeg {
                                text: s.text.into(),
                                tone: match s.tone {
                                    patchview::SegTone::Plain => 0,
                                    patchview::SegTone::Added => 1,
                                    patchview::SegTone::Removed => 2,
                                    patchview::SegTone::Meta => 3,
                                },
                            })
                            .collect::<Vec<_>>(),
                    )),
                })
                .collect()
        })
        .unwrap_or_default();
    g.set_nav_rows(ModelRc::new(VecModel::from(nav)));
    g.set_rows(ModelRc::new(VecModel::from(rows)));
}

/// The patch text of proposal `id`, wherever its card lives right now:
/// the selected decision, or a surface's pending list.
fn patch_text_for(ui: &AppWindow, id: i32) -> Option<String> {
    let dec = ui.get_selected_decision();
    if dec.id == id && dec.patch_op {
        return Some(dec.proposed.to_string());
    }
    ui.get_surfaces().iter().find_map(|s| {
        s.pending
            .iter()
            .find(|p| p.id == id && p.patch_op)
            .map(|p| p.proposed.to_string())
    })
}

/// Wire the diff viewer's file navigator: a click re-fills the details
/// pane for the proposal the global currently holds (`for-id`).
pub(crate) fn wire_patch_view(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.global::<PatchView>().on_select(move |idx| {
        let Some(ui) = weak.upgrade() else { return };
        let id = ui.global::<PatchView>().get_for_id();
        if let Some(patch) = patch_text_for(&ui, id) {
            patch_view_sync(&ui, &patch, usize::try_from(idx).unwrap_or(0), id);
        }
    });
}

/// The wiki export's outcome as toast copy, or `None` while it is idle or
/// still running. Pure on purpose: the caller edge-triggers it, so a
/// re-pushed session never re-toasts the same result.
pub(crate) fn wiki_export_toast(lang: i32, ex: &molt_core::ExportState) -> Option<(String, bool)> {
    if ex.running || ex.result.is_empty() {
        return None;
    }
    let l = if lang == 1 { Lexicon::de() } else { Lexicon::en() };
    if let Some(reason) = ex.result.strip_prefix("error: ") {
        return Some((format!("⚠ {} {reason}", l.mem_ex_failed), true));
    }
    if ex.result == "ok" {
        let unit = if ex.files == 1 {
            l.mem_ex_file
        } else {
            l.mem_ex_files
        };
        return Some((format!("{} {} {unit}", l.mem_ex_done, ex.files), false));
    }
    None
}

/// The export dialog's two doors: the native folder picker, and the command
/// itself. `WikiExport` answers `Ack` — the REAL outcome lands
/// asynchronously in `SessionView::wiki_export` (toasted from the mirror),
/// so only an immediate refusal (no target, empty wiki, a proof without a
/// chain) toasts from here.
pub(crate) fn wire_wiki_export(ui: &AppWindow, cx: &Ctx) {
    {
        let cx = cx.clone();
        ui.on_mem_export_pick(move || {
            let weak = cx.weak.clone();
            // only the property read runs on the UI thread; the stat in
            // browse_start_dir moves to a blocking task
            let draft = weak
                .upgrade()
                .map(|ui| ui.get_mem_export_dir().to_string())
                .unwrap_or_default();
            cx.rt.spawn(async move {
                let start_dir = tokio::task::spawn_blocking(move || browse_start_dir(&draft))
                    .await
                    .ok()
                    .flatten();
                let mut picker = rfd::AsyncFileDialog::new();
                if let Some(dir) = start_dir {
                    picker = picker.set_directory(dir);
                }
                let Some(folder) = picker.pick_folder().await else {
                    return; // cancelled
                };
                let path = folder.path().display().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_mem_export_dir(path.into());
                    }
                });
            });
        });
    }
    {
        let cx = cx.clone();
        ui.on_wiki_export(move |dest, proof| {
            cx.issue(
                Command::WikiExport {
                    dest: dest.to_string(),
                    proof,
                },
            );
        });
    }
}

/// Wire the changeset panel's "start vote" — the one wiki verb that talks
/// to the engine. The stack is reprocessed into the NET patch; a net-empty
/// changeset just clears (every action cancelled out). Otherwise the patch
/// rides a REAL gated proposal on the Memory surface — the same threshold
/// governance every surface runs — and the working copy resets to base:
/// the proposal carries the changes now.
pub(crate) fn wire_wiki_vote(
    ui: &AppWindow,
    cx: &Ctx,
    model: &Rc<RefCell<wiki::Wiki>>,
    last: &Rc<RefCell<FaceState>>,
) {
    let m = model.clone();
    let la = last.clone();
    let weak = ui.as_weak();
    let cx = cx.clone();
    ui.global::<WikiState>().on_cs_vote(move || {
        let Some(ui) = weak.upgrade() else { return };
        // this click decides: whatever was queued is superseded by it
        m.borrow_mut().clear_vote();
        let Some(patch) = m.borrow().build_patch() else {
            // bytes still on their way: QUEUE the vote and ask again. It
            // used to say "try again in a moment" over a request nobody
            // had made - a refusal the member could hit forever.
            if !m.borrow().unloaded_changes().is_empty() {
                {
                    let mut w = m.borrow_mut();
                    w.queue_vote();
                    // the click distrusts every fetch in progress: a
                    // request dropped somewhere would keep the queued vote
                    // waiting forever
                    w.requeue_content();
                }
                sync_after(&ui, &m, &la);
                let msg = ui.global::<Strings>().get_mem_toast_vote_queued();
                ui.invoke_show_toast(msg);
                return;
            }
            // an EMPTY folder is not expressible in a git patch, and it is
            // not noise either: reverting here threw the member's folder
            // away and said "changes cancel out", which was true of the
            // patch and false of their work
            let folders = m.borrow().empty_added_folders();
            if !folders.is_empty() {
                let msg = ui.global::<Strings>().get_mem_toast_folder_only();
                ui.invoke_show_toast_error(msg);
                return;
            }
            m.borrow_mut().revert_all();
            la.borrow_mut().raw_for = None;
            sync_after(&ui, &m, &la);
            let msg = ui.global::<Strings>().get_mem_toast_net_empty();
            ui.invoke_show_toast(msg);
            return;
        };
        // language-neutral summary for the proposal title, "+2 -1 →1 ~34"
        let summary = m.borrow().changeset_counts().summary();
        let payload = serde_json::json!({
            "op": "wiki_patch",
            "summary": summary,
            // display-only staleness hint (shared_memory_real.md §9.1):
            // the card warns when the base moved past this — the fold's
            // verdict NEVER reads it
            "base_rev": m.borrow().base_rev,
            "value": patch,
        });
        // the completion must not carry the UI-thread Rc model into the
        // task — clearing the changeset goes through the same WikiState
        // door the panel button uses, back on the UI thread
        let weak2 = ui.as_weak();
        let wh = cx.wallet.clone();
        cx.rt.spawn(async move {
            let outcome = wh
                .execute(Command::Propose {
                    surface: Surface::Memory,
                    payload,
                })
                .await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak2.upgrade() else { return };
                match outcome {
                    Ok(reply) => {
                        ui.global::<WikiState>().invoke_cs_revert();
                        // the patch is fine either way - the fold never reads
                        // a header - but a voter should see it before signing
                        let bad_header = matches!(
                            &reply,
                            molt_core::Reply::Proposed { warnings, .. } if !warnings.is_empty()
                        );
                        let s = ui.global::<Strings>();
                        if bad_header {
                            let msg = s.get_toast_proposed_bad_header();
                            ui.invoke_show_toast_error(msg);
                        } else {
                            let msg = s.get_toast_proposed();
                            ui.invoke_show_toast(msg);
                        }
                    }
                    Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
                }
            });
        });
    });
}

/// The engine-backed wiki READS (`knowledge_base_scale.md` §4.10): the
/// open document's in-edges and the navigator's full-text search. Neither
/// touches the local model - they only fill their own face, so a stale
/// reply can never move a document under the user's hands.
pub(crate) fn wire_wiki_index(ui: &AppWindow, ctx: &Ctx) {
    let g = ui.global::<WikiState>();
    {
        let cx = ctx.clone();
        // the base moved: page the METADATA in. The tree itself never
        // rides a read again (§4.10) - content follows per document.
        g.on_base_changed(move || {
            let wh = cx.wallet.clone();
            let weak = cx.weak.clone();
            cx.rt.spawn(async move {
                let mut paths: Vec<String> = Vec::new();
                let mut cursor: Option<String> = None;
                loop {
                    let reply = wh
                        .execute(Command::WikiList {
                            prefix: None,
                            cursor: cursor.clone(),
                            limit: 500,
                        })
                        .await;
                    let Ok(Reply::WikiList {
                        docs, next_cursor, ..
                    }) = reply
                    else {
                        break;
                    };
                    paths.extend(docs.into_iter().map(|d| d.path));
                    match next_cursor {
                        Some(c) => cursor = Some(c),
                        None => break,
                    }
                }
                // …and what this republic's headers already say: the
                // tags the + Tag modal offers, and the keys it uses as
                // RELATIONS - the ontology is content, not a schema
                let (vocab, relations): (Vec<String>, Vec<String>) =
                    match wh.execute(Command::WikiProps).await {
                        Ok(Reply::WikiProps { keys, .. }) => {
                            let tags = keys
                                .iter()
                                .find(|k| k.key == wiki::TAG_KEY)
                                .map(|k| {
                                    // a hint, not a catalogue: the chips sit
                                    // above the inputs and must not push OK
                                    // off the modal
                                    k.values.iter().take(12).map(|v| v.value.clone()).collect()
                                })
                                .unwrap_or_default();
                            // a key is a RELATION where its values are links
                            let rels = keys
                                .iter()
                                .filter(|k| k.key != wiki::TAG_KEY)
                                .filter(|k| {
                                    k.values
                                        .iter()
                                        .any(|v| molt_engine::link_target(&v.value).is_some())
                                })
                                .take(24)
                                .map(|k| k.key.clone())
                                .collect();
                            (tags, rels)
                        }
                        _ => (Vec::new(), Vec::new()),
                    };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let g = ui.global::<WikiState>();
                    // metadata only: `set_base` keeps the bytes this node
                    // already holds while the revision stands still, and
                    // drops them when it moved
                    let docs: Vec<WikiBase> = paths
                        .into_iter()
                        .map(|path| WikiBase {
                            path: path.into(),
                            content: String::new().into(),
                            loaded: false,
                        })
                        .collect();
                    sync_model(&g.get_base_docs(), docs, PartialEq::eq, |m| {
                        g.set_base_docs(m);
                    });
                    let vocab: Vec<slint::SharedString> =
                        vocab.into_iter().map(Into::into).collect();
                    sync_model(&g.get_tag_vocab(), vocab, PartialEq::eq, |m| {
                        g.set_tag_vocab(m);
                    });
                    g.invoke_relation_vocab(ModelRc::new(VecModel::from(
                        relations
                            .into_iter()
                            .map(slint::SharedString::from)
                            .collect::<Vec<_>>(),
                    )));
                    g.invoke_base_arrived();
                });
            });
        });
    }
    {
        let cx = ctx.clone();
        g.on_content_wanted(move |path| {
            let wh = cx.wallet.clone();
            let weak = cx.weak.clone();
            let wanted = path.to_string();
            cx.rt.spawn(async move {
                // BOUNDED, then loud. Dropping the failure silently left a
                // queued vote waiting for bytes nobody was fetching any
                // more; a retry loop instead of a bound would spin. Three
                // tries cover the transient refusals (the index still
                // building, the base still arriving), and the next sync
                // asks again anyway - every model mutation syncs, and the
                // Vote click re-requests.
                let mut content = None;
                let mut why = String::new();
                for wait in [1u64, 3, 0] {
                    match wh
                        .execute(Command::WikiGet {
                            path: wanted.clone(),
                        })
                        .await
                    {
                        Ok(Reply::WikiDocument { content: c, .. }) => {
                            content = Some(c);
                            break;
                        }
                        other => {
                            why = format!("{other:?}");
                            if wait == 0 {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        }
                    }
                }
                if content.is_none() {
                    tracing::warn!(path = %wanted, error = %why, "wiki content unreadable");
                }
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let g = ui.global::<WikiState>();
                    match content {
                        Some(c) => g.invoke_content_arrived(wanted.into(), c.into()),
                        // a path the ratified base no longer carries is a
                        // base that MOVED under us: `base_arrived` rebases
                        // and re-asks under the new path
                        None => g.invoke_content_failed(wanted.into()),
                    }
                });
            });
        });
    }
    {
        // the page's file references (§3.2): ONE resolve per distinct hex,
        // and the byte read the answer asks for
        let cx = ctx.clone();
        g.on_file_refs_wanted(move |hexes| {
            for hex in hexes.iter().map(|h| h.to_string()) {
                spawn_ref_resolve(&cx, hex);
            }
        });
    }
    {
        let cx = ctx.clone();
        g.on_file_fetch(move |dest| {
            if let Some(id) = file_ref_share(&dest) {
                cx.issue(Command::DownloadFile { id, dest: None });
            }
        });
    }
    {
        let cx = ctx.clone();
        g.on_file_retry(move |dest| {
            if let Some(ui) = cx.weak.upgrade() {
                forget_file_ref(&ui, &dest);
            }
        });
    }
    {
        // §3.5: the uploads table, pre-filtered to this ONE file
        let cx = ctx.clone();
        g.on_jump_upload(move |dest| {
            let (needle, view) = file_ref_jump(&dest);
            if let Ok(mut st) = cx.chat_ui.lock() {
                st.set_uploads_filter(needle);
            }
            cx.issue(Command::SelectView {
                surface: Surface::Files,
                view: view.to_string(),
            });
            cx.refresh_surfaces();
        });
    }
    {
        let cx = ctx.clone();
        g.on_backlinks_wanted(move |path| {
            let wh = cx.wallet.clone();
            let weak = cx.weak.clone();
            let wanted = path.to_string();
            cx.rt.spawn(async move {
                let edges = match wh
                    .execute(Command::WikiLinks {
                        path: wanted.clone(),
                        direction: Some("in".to_string()),
                        predicate: None,
                        limit: 0,
                        cursor: 0,
                    })
                    .await
                {
                    Ok(Reply::WikiLinks { edges, .. }) => edges,
                    // a document the approved base does not carry (a local
                    // draft) has no in-edges — an honest empty list
                    _ => Vec::new(),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let g = ui.global::<WikiState>();
                    // the reply may outlive the tab that asked for it
                    if g.get_doc_path().as_str() != wanted {
                        return;
                    }
                    let mut rows: Vec<slint::SharedString> =
                        edges.into_iter().map(|e| e.path.into()).collect();
                    rows.sort();
                    rows.dedup();
                    sync_model(&g.get_backlinks(), rows, PartialEq::eq, |m| {
                        g.set_backlinks(m);
                    });
                });
            });
        });
    }
    {
        let cx = ctx.clone();
        g.on_search(move |query| {
            let text = query.trim().to_string();
            let weak = cx.weak.clone();
            if text.is_empty() {
                if let Some(ui) = weak.upgrade() {
                    let g = ui.global::<WikiState>();
                    g.set_search_ran(false);
                    sync_model(&g.get_search_hits(), Vec::new(), PartialEq::eq, |m| {
                        g.set_search_hits(m);
                    });
                }
                return;
            }
            let wh = cx.wallet.clone();
            let asked = text.clone();
            cx.rt.spawn(async move {
                let outcome = wh
                    .execute(Command::WikiSearch {
                        query: text,
                        tags: Vec::new(),
                        kind: None,
                        folder: None,
                        props: Vec::new(),
                        limit: 50,
                        cursor: 0,
                    })
                    .await;
                // "the index is still building" is NOT "nothing matched" -
                // rendering the second for the first is the lie §4.6 warns
                // about, and the field would say "no hit" about a wiki it
                // has not read yet
                let building = matches!(
                    outcome,
                    Err(molt_core::MoltError::IndexBuilding { .. })
                );
                let hits = match outcome {
                    Ok(Reply::WikiSearch { hits, .. }) => hits,
                    _ => Vec::new(),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let g = ui.global::<WikiState>();
                    // typing outruns the index: a reply that belongs to an
                    // earlier needle must not describe the one in the field
                    if g.get_search_query().trim() != asked {
                        return;
                    }
                    let rows: Vec<WikiHitRow> = hits
                        .into_iter()
                        .map(|h| WikiHitRow {
                            path: h.path.into(),
                            title: h.title.unwrap_or_default().into(),
                            snippet: h.snippet.into(),
                        })
                        .collect();
                    sync_model(&g.get_search_hits(), rows, PartialEq::eq, |m| {
                        g.set_search_hits(m);
                    });
                    g.set_search_building(building);
                    g.set_search_ran(!building);
                });
            });
        });
    }
}

/// The debounced wiki-draft persist (WP-D) and its load on workspace
/// entry: the two draft doors the wiki model reaches the engine through.
/// Resolve ONE reference off the actor, then hand the answer to the UI
/// thread; a picture whose bytes are here reads them next.
fn spawn_ref_resolve(cx: &Ctx, hex: String) {
    let cx2 = cx.clone();
    let wh = cx.wallet.clone();
    let weak = cx.weak.clone();
    cx.rt.spawn(async move {
        let answer = match wh
            .execute(Command::ResolveUpload {
                checksum: hex.clone(),
            })
            .await
        {
            Ok(Reply::UploadResolved {
                upload,
                ambiguous,
                temporary,
                local,
            }) => Some(RefAnswer {
                upload,
                ambiguous,
                temporary,
                local,
            }),
            _ => None,
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(checksum) = file_resolved(&ui, &hex, answer) {
                spawn_upload_read(&cx2, hex, checksum);
            }
        });
    });
}

/// Read a referenced picture's verified bytes and decode them on a
/// worker - a 12 MB photo must never touch the UI thread (§3.4).
fn spawn_upload_read(cx: &Ctx, hex: String, checksum: String) {
    let wh = cx.wallet.clone();
    let weak = cx.weak.clone();
    cx.rt.spawn(async move {
        let bytes = match wh
            .execute(Command::ReadUploadBytes {
                checksum,
                cap: IMAGE_READ_CAP,
            })
            .await
        {
            Ok(Reply::UploadBytes { bytes }) => Some(bytes),
            _ => None,
        };
        let decoded = match bytes {
            Some(b) => tokio::task::spawn_blocking(move || crate::images::decode_wiki_image(&b))
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            image_decoded(&ui, &hex, decoded);
        });
    });
}

pub(crate) fn wire_wiki_draft(ui: &AppWindow, ctx: &Ctx) {
    {
        let cx = ctx.clone();
        // the debounced wiki-draft persist (WP-D) — an opaque blob hop
        ui.on_wiki_draft_save(move |draft| {
            cx.issue(
                Command::WikiDraftSave {
                    draft: draft.to_string(),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        // workspace entry: fetch the stored draft, hand it to the wiki
        // model over the WikiState bridge (the completion is Send-bound)
        ui.on_wiki_draft_load(move || {
            let w = cx.wallet.clone();
            let weak2 = cx.weak.clone();
            cx.rt.spawn(async move {
                let draft = match w.execute(Command::WikiDraftLoad).await {
                    Ok(Reply::WikiDraft { draft }) => draft,
                    _ => String::new(),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak2.upgrade() {
                        ui.global::<WikiState>().invoke_draft_loaded(draft.into());
                    }
                });
            });
        });
    }
}
