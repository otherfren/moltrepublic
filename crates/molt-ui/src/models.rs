// SPDX-License-Identifier: GPL-3.0-or-later
//! In-place `VecModel` mirroring: the repeater keeps its element instances
//! (focus, scroll, selection, double-click counters) only while the
//! `ModelRc` stays the same object, so every mirror push PATCHES rows and
//! never swaps the model wholesale.

use slint::{Model, ModelRc, VecModel};

use crate::WikiBlock;

/// Update a model's rows IN PLACE instead of replacing the ModelRc: the
/// repeater keeps its element instances, and with them focus, scroll and
/// selection state (replacing the model recreates everything — that is how
/// the chat compose box once lost its focus mid-typing).
pub(crate) fn sync_rows<T: Clone + 'static>(
    current: &ModelRc<T>,
    items: Vec<T>,
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
            m.set_row_data(i, item);
        } else {
            m.push(item);
        }
    }
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

/// Update a `VecModel`-backed property IN PLACE: shrink, patch changed
/// rows, grow. Wholesale `ModelRc` replacement re-creates every row
/// element, which silently breaks anything stateful inside them — the
/// double-click detector on a nav row died exactly that way (the first
/// click of the pair marks, the sync then destroyed the TouchArea that
/// was counting). Falls back to a fresh `VecModel` only on the first push
/// (the property still holds its compile-time default model).
pub(crate) fn sync_vec_model<T: Clone + PartialEq + 'static>(rc: &ModelRc<T>, new: Vec<T>) -> Option<ModelRc<T>> {
    let Some(vm) = rc.as_any().downcast_ref::<VecModel<T>>() else {
        return Some(ModelRc::new(VecModel::from(new)));
    };
    while vm.row_count() > new.len() {
        vm.remove(vm.row_count() - 1);
    }
    for (i, row) in new.into_iter().enumerate() {
        if i < vm.row_count() {
            if vm.row_data(i).as_ref() != Some(&row) {
                vm.set_row_data(i, row);
            }
        } else {
            vm.push(row);
        }
    }
    None
}

/// `sync_vec_model` for the preview blocks: `WikiBlock.spans` is a nested
/// model whose derived equality is POINTER identity, and every sync builds
/// fresh span models — the generic compare would therefore rewrite (and
/// re-create) every block row on every sync. Compare spans by content.
fn wiki_block_eq(a: &WikiBlock, b: &WikiBlock) -> bool {
    a.kind == b.kind
        && a.status == b.status
        && a.text == b.text
        && a.spans.row_count() == b.spans.row_count()
        && (0..a.spans.row_count()).all(|i| a.spans.row_data(i) == b.spans.row_data(i))
}

pub(crate) fn sync_wiki_blocks(rc: &ModelRc<WikiBlock>, new: Vec<WikiBlock>) -> Option<ModelRc<WikiBlock>> {
    let Some(vm) = rc.as_any().downcast_ref::<VecModel<WikiBlock>>() else {
        return Some(ModelRc::new(VecModel::from(new)));
    };
    while vm.row_count() > new.len() {
        vm.remove(vm.row_count() - 1);
    }
    for (i, row) in new.into_iter().enumerate() {
        if i < vm.row_count() {
            let same = vm
                .row_data(i)
                .as_ref()
                .is_some_and(|old| wiki_block_eq(old, &row));
            if !same {
                vm.set_row_data(i, row);
            }
        } else {
            vm.push(row);
        }
    }
    None
}
