// SPDX-License-Identifier: GPL-3.0-or-later
//! The Shared-Memory wiki's bridge between its Rust state machine
//! ([`crate::wiki`]) and the `WikiState` / `PatchView` globals: the face
//! sync, the callback wiring, the diff viewer and the export dialog.

use std::cell::RefCell;
use std::rc::Rc;

use molt_core::{Command, Reply, Surface};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::i18n::{error_toast, localize_wiki_err, Lexicon};
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

thread_local! {
    /// The wiki auto-save guard: the last draft handed to the engine and
    /// when — `sync_wiki` saves only what CHANGED, at most every 2 s (a
    /// hard kill loses at most that window; WP-D).
    static DRAFT_SAVE_GUARD: std::cell::RefCell<(String, std::time::Instant)> =
        std::cell::RefCell::new((String::new(), std::time::Instant::now()));
}

/// Push the wiki model into the `WikiState` global — the whole face, after
/// every mutation (the models are small, and rows patch in place). EXCEPT
/// the editor buffer: `raw` is rewritten only when the active doc or the
/// edit mode changes (`last`), never on the keystroke echo — a mid-typing
/// rewrite fights the caret.
fn sync_wiki(ui: &AppWindow, w: &wiki::Wiki, last: &mut Option<(wiki::DocId, bool)>) {
    // the debounced draft persist rides every sync (every model mutation
    // lands here) — cheap: serialize, compare, at most one engine hop/2 s
    let draft = w.to_draft();
    let due = DRAFT_SAVE_GUARD.with(|g| {
        let mut g = g.borrow_mut();
        if g.0 != draft && g.1.elapsed() >= std::time::Duration::from_secs(2) {
            g.0 = draft.clone();
            g.1 = std::time::Instant::now();
            true
        } else {
            false
        }
    });
    if due {
        ui.invoke_wiki_draft_save(draft.into());
    }
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
    s.set_can_reveal(w.active_id().is_some() && w.active_id() != w.marked());
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
    s.set_can_write_header(w.can_write_header());
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
    // the details modal shows the WHOLE change; the panel list is capped
    s.set_cs_patch(w.build_patch().unwrap_or_default().into());
    if let Some(doc) = w.active() {
        let id = doc.id;
        let path_changed = s.get_doc_path().as_str() != doc.path;
        s.set_doc_open(true);
        s.set_doc_path(doc.path.clone().into());
        s.set_doc_meta(format!("{} · {} · {}", doc.author, doc.ver, doc.when).into());
        s.set_doc_status(wiki_status_code(doc.status()));
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
                        })
                        .collect::<Vec<_>>(),
                )),
            })
            .collect();
        sync_model(&s.get_blocks(), blocks, wiki_block_eq, |m| s.set_blocks(m));
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
        let (tags, props): (Vec<WikiProp>, Vec<WikiProp>) =
            rows.into_iter().partition(|r| r.hue >= 0);
        sync_model(&s.get_props(), props, PartialEq::eq, |m| s.set_props(m));
        sync_model(&s.get_tag_pills(), tags, PartialEq::eq, |m| s.set_tag_pills(m));
        s.set_can_add_tags(w.can_add_tags());
        // …and the BYTES of the document being looked at, if this node
        // does not hold them yet (§4.10). Idempotent: the reply fills the
        // model and the next sync asks for nothing.
        if let Some(want) = w.wants_content() {
            s.invoke_content_wanted(want.into());
        }
        // the in-edges come from the engine, so they are fetched only when
        // the open document actually changed
        if path_changed {
            s.set_backlinks(ModelRc::new(VecModel::from(Vec::<slint::SharedString>::new())));
            s.invoke_backlinks_wanted(doc.path.clone().into());
        }
        if *last != Some((id, w.editing)) {
            s.set_raw(doc.raw.clone().into());
            *last = Some((id, w.editing));
        }
    } else {
        s.set_doc_open(false);
        s.set_doc_path("".into());
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
        *last = None;
    }
}

/// Wire the Multisig-Wiki's Rust state machine ([`wiki`]) to the
/// `WikiState` global: every action callback mutates the model, then
/// re-syncs the whole face. UI-local by design — of the wiki verbs only
/// the changeset VOTE talks to the engine, and that one is wired
/// separately ([`wire_wiki_vote`]) where the handles live. Returns the
/// shared model + raw-guard for that wiring.
#[allow(clippy::type_complexity)]
pub(crate) fn wire_wiki(
    ui: &AppWindow,
) -> (
    Rc<RefCell<wiki::Wiki>>,
    Rc<RefCell<Option<(wiki::DocId, bool)>>>,
) {
    // the REAL base arrives from the engine read (set_base) — until then
    // the honest empty state shows; the sample stays a test fixture
    let model = Rc::new(RefCell::new(wiki::Wiki::empty()));
    let last: Rc<RefCell<Option<(wiki::DocId, bool)>>> = Rc::new(RefCell::new(None));
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
    act!(on_reveal, |w| w.reveal());
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
    act!(on_tag_commit, |w| {
        w.tag_commit();
    });
    act!(on_link_open, |w, target: slint::SharedString| w.link_open(&target));
    act!(on_link_close, |w| w.link_close());
    act!(on_set_link_name, |w, v: slint::SharedString| w.set_link_name(&v));
    act!(on_set_link_target, |w, v: slint::SharedString| w.set_link_target(&v));
    act!(on_set_link_filter, |w, v: slint::SharedString| w.set_link_filter(&v));
    act!(on_set_link_custom, |w, v: slint::SharedString| w.set_link_custom(&v));
    act!(on_link_add_custom, |w| {
        w.link_add_custom();
    });
    act!(on_link_toggle, |w, key: slint::SharedString| w.link_toggle(&key));
    act!(on_link_commit, |w| {
        w.link_commit();
    });
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
            *la.borrow_mut() = None;
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                DRAFT_SAVE_GUARD.with(|g| *g.borrow_mut() = (w.to_draft(), std::time::Instant::now()));
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
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
            m.borrow_mut().load_base(&path, &content);
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
                sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
            *la.borrow_mut() = None;
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
        });
    }
    {
        let m = model.clone();
        let la = last.clone();
        let weak = ui.as_weak();
        g.on_cs_revert(move || {
            let Some(ui) = weak.upgrade() else { return };
            m.borrow_mut().revert_all();
            *la.borrow_mut() = None;
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
            *la.borrow_mut() = None;
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
            *la.borrow_mut() = None;
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
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
    last: &Rc<RefCell<Option<(wiki::DocId, bool)>>>,
) {
    let m = model.clone();
    let la = last.clone();
    let weak = ui.as_weak();
    let cx = cx.clone();
    ui.global::<WikiState>().on_cs_vote(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(patch) = m.borrow().build_patch() else {
            // an EMPTY folder is not expressible in a git patch, and it is
            // not noise either: reverting here threw the member's folder
            // away and said "changes cancel out", which was true of the
            // patch and false of their work
            // bytes still on their way: the request is already in flight
            // (sync_wiki asks for them), so this is "not yet", not "no"
            if !m.borrow().unloaded_changes().is_empty() {
                let msg = ui.global::<Strings>().get_mem_toast_loading();
                ui.invoke_show_toast(msg);
                return;
            }
            let folders = m.borrow().empty_added_folders();
            if !folders.is_empty() {
                let msg = ui.global::<Strings>().get_mem_toast_folder_only();
                ui.invoke_show_toast_error(msg);
                return;
            }
            m.borrow_mut().revert_all();
            *la.borrow_mut() = None;
            sync_wiki(&ui, &m.borrow(), &mut la.borrow_mut());
            let msg = ui.global::<Strings>().get_mem_toast_net_empty();
            ui.invoke_show_toast(msg);
            return;
        };
        // language-neutral summary for the proposal title, "+2 -1 →1 ~34"
        let c = m.borrow().changeset_counts();
        let mut summary = String::new();
        for (n, sign) in [
            (c.added, '+'),
            (c.deleted, '-'),
            (c.moved, '→'),
            (c.lines, '~'),
        ] {
            if n > 0 {
                if !summary.is_empty() {
                    summary.push(' ');
                }
                summary.push(sign);
                summary.push_str(&n.to_string());
            }
        }
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
                // …and the tags this republic already uses, which is
                // what the + Tag modal offers instead of a schema
                let vocab: Vec<String> = match wh.execute(Command::WikiProps).await {
                    Ok(Reply::WikiProps { keys, .. }) => keys
                        .into_iter()
                        .find(|k| k.key == wiki::TAG_KEY)
                        .map(|k| {
                            // a hint, not a catalogue: the chips sit above
                            // the inputs and must not push OK off the modal
                            k.values.into_iter().take(12).map(|v| v.value).collect()
                        })
                        .unwrap_or_default(),
                    _ => Vec::new(),
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
                let Ok(Reply::WikiDocument { content, .. }) = wh
                    .execute(Command::WikiGet {
                        path: wanted.clone(),
                    })
                    .await
                else {
                    return; // an unknown path is a base that moved under us
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<WikiState>()
                            .invoke_content_arrived(wanted.into(), content.into());
                    }
                });
            });
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
