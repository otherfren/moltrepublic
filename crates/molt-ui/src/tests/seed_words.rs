// SPDX-License-Identifier: GPL-3.0-or-later
//! The rejoin phrase area's word chips.

use crate::actions::seed_word_rows;

#[test]
fn each_typed_word_is_numbered_and_toned_four_to_a_row() {
    let rows = seed_word_rows("abandon ability Able xyz above absent abs");
    let flat: Vec<(i32, &str, i32)> = rows
        .iter()
        .flatten()
        .map(|(n, w, t)| (*n, w.as_str(), *t))
        .collect();
    assert_eq!(
        flat,
        vec![
            (1, "abandon", 1),
            (2, "ability", 1),
            (3, "Able", 2),
            (4, "xyz", 2),
            (5, "above", 1),
            (6, "absent", 1),
            (7, "abs", 0),
        ]
    );
    assert_eq!(rows.iter().map(Vec::len).collect::<Vec<_>>(), vec![4, 3]);
}

#[test]
fn a_finished_word_is_judged_even_when_it_starts_a_list_word() {
    // the trailing space ends the word: "abs" itself is not a list word
    assert_eq!(seed_word_rows("abs "), vec![vec![(1, "abs".to_string(), 2)]]);
    assert!(seed_word_rows("  ").is_empty());
}
