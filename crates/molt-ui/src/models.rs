// SPDX-License-Identifier: GPL-3.0-or-later
//! In-place `VecModel` mirroring: the repeater keeps its element instances
//! (focus, scroll, selection, double-click counters) only while the
//! `ModelRc` stays the same object, so every mirror push PATCHES rows and
//! never swaps the model wholesale.

use slint::{Model, ModelRc, VecModel};

use crate::{LogLine, ProposalRow, WikiBlock, WorkspaceItem};

/// Update a `VecModel`-backed property IN PLACE: shrink, patch the rows
/// `eq` does not accept as unchanged, grow. Wholesale `ModelRc`
/// replacement re-creates every row element, which silently breaks
/// anything stateful inside them - the chat compose box once lost its
/// focus mid-typing that way, and the double-click detector on a wiki nav
/// row died the same death (the first click of the pair marks, the sync
/// then destroyed the TouchArea that was counting). `set` runs only on the
/// first push, while the property still holds its compile-time default
/// model (not a `VecModel`).
pub(crate) fn sync_model<T: Clone + 'static>(
    current: &ModelRc<T>,
    items: Vec<T>,
    eq: impl Fn(&T, &T) -> bool,
    set: impl FnOnce(ModelRc<T>),
) {
    let Some(m) = current.as_any().downcast_ref::<VecModel<T>>() else {
        set(ModelRc::new(VecModel::from(items)));
        return;
    };
    while m.row_count() > items.len() {
        m.remove(m.row_count() - 1);
    }
    for (i, item) in items.into_iter().enumerate() {
        if i < m.row_count() {
            if !m.row_data(i).as_ref().is_some_and(|old| eq(old, &item)) {
                m.set_row_data(i, item);
            }
        } else {
            m.push(item);
        }
    }
}

/// [`sync_model`] with the row type's OWN equality: a row the engine
/// re-derived byte-identically is not written, so an engine event that
/// changed nothing dirties nothing. Row types carrying a nested model
/// need a field-wise `eq` instead (see [`log_line_eq`]) - the derived
/// `PartialEq` compares a `ModelRc` by pointer.
pub(crate) fn sync_rows<T: Clone + PartialEq + 'static>(
    current: &ModelRc<T>,
    items: Vec<T>,
    set: impl FnOnce(ModelRc<T>),
) {
    sync_model(current, items, PartialEq::eq, set);
}

/// Patch a row's NESTED model in place and hand back that same instance.
/// A fresh `ModelRc` per push rebuilds the whole nested repeater and
/// (via the pointer compare) marks the parent row changed too - which is
/// how one chat message came to repaint every surface.
pub(crate) fn patch_nested<T: Clone + 'static>(
    old: Option<&ModelRc<T>>,
    items: Vec<T>,
    eq: impl Fn(&T, &T) -> bool,
) -> ModelRc<T> {
    let Some(cur) = old else {
        return ModelRc::new(VecModel::from(items));
    };
    let mut fresh = None;
    sync_model(cur, items, eq, |m| fresh = Some(m));
    fresh.unwrap_or_else(|| cur.clone())
}

/// Rebuild a `[string]` mirror in place.
pub(crate) fn sync_strings(
    current: &ModelRc<slint::SharedString>,
    items: &[String],
    set: impl FnOnce(ModelRc<slint::SharedString>),
) {
    sync_rows(
        current,
        items.iter().map(|l| l.as_str().into()).collect(),
        set,
    );
}

/// Two models hold the same rows.
pub(crate) fn models_eq<T: Clone + PartialEq + 'static>(a: &ModelRc<T>, b: &ModelRc<T>) -> bool {
    a.row_count() == b.row_count() && a.iter().eq(b.iter())
}

/// The `eq` of a row type carrying nested models: compare those by
/// CONTENT and let the derive cover every other field. Written this way
/// rather than field by field because the struct is generated from
/// `.slint` - a field added there must not silently drop out of the
/// comparison and freeze a stale row on screen.
pub(crate) fn wiki_block_eq(a: &WikiBlock, b: &WikiBlock) -> bool {
    models_eq(&a.spans, &b.spans)
        && *b == WikiBlock { spans: b.spans.clone(), ..a.clone() }
}

/// [`wiki_block_eq`] for a chat/log row (`reactions`, `receipts`).
pub(crate) fn log_line_eq(a: &LogLine, b: &LogLine) -> bool {
    models_eq(&a.reactions, &b.reactions)
        && models_eq(&a.receipts, &b.receipts)
        && *b
            == LogLine {
                reactions: b.reactions.clone(),
                receipts: b.receipts.clone(),
                ..a.clone()
            }
}

/// [`wiki_block_eq`] for a vote card (`relay_changes`, `votes`).
pub(crate) fn proposal_row_eq(a: &ProposalRow, b: &ProposalRow) -> bool {
    models_eq(&a.relay_changes, &b.relay_changes)
        && models_eq(&a.votes, &b.votes)
        && *b
            == ProposalRow {
                relay_changes: b.relay_changes.clone(),
                votes: b.votes.clone(),
                ..a.clone()
            }
}

/// [`wiki_block_eq`] for a workspace card (`members`).
pub(crate) fn workspace_item_eq(a: &WorkspaceItem, b: &WorkspaceItem) -> bool {
    models_eq(&a.members, &b.members)
        && *b == WorkspaceItem { members: b.members.clone(), ..a.clone() }
}
