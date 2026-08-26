// SPDX-License-Identifier: GPL-3.0-or-later
//! In-place `VecModel` mirroring: the repeater keeps its element instances
//! (focus, scroll, selection, double-click counters) only while the
//! `ModelRc` stays the same object, so every mirror push PATCHES rows and
//! never swaps the model wholesale.

use slint::{Model, ModelRc, VecModel};

use crate::WikiBlock;

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

/// [`sync_model`] for row types without an equality: every row is
/// rewritten (the repeater still keeps its elements - only the data moves).
pub(crate) fn sync_rows<T: Clone + 'static>(
    current: &ModelRc<T>,
    items: Vec<T>,
    set: impl FnOnce(ModelRc<T>),
) {
    sync_model(current, items, |_, _| false, set);
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

/// The `eq` of the preview blocks: `WikiBlock.spans` is a nested model
/// whose derived equality is POINTER identity, and every sync builds fresh
/// span models - the derived compare would therefore rewrite (and
/// re-create) every block row on every sync. Compare spans by content.
pub(crate) fn wiki_block_eq(a: &WikiBlock, b: &WikiBlock) -> bool {
    a.kind == b.kind
        && a.status == b.status
        && a.text == b.text
        && a.spans.row_count() == b.spans.row_count()
        && (0..a.spans.row_count()).all(|i| a.spans.row_data(i) == b.spans.row_data(i))
}
