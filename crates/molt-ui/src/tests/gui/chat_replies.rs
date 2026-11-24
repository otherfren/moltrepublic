// SPDX-License-Identifier: GPL-3.0-or-later
//! Issue #14: a chain of replies indents one level per reply, capped at
//! three, and never steps back left.

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

/// The chat pane holding a chain of seven replies, `width` wide at app
/// font `font`; each card's `(x, width)`, depth 0 first.
fn chain_cards(width: u32, font: f32) -> Vec<(f32, f32)> {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(width, 2000));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("chat".into());
    ui.set_selected_view("today".into());
    ui.set_selected_channel("group".into());
    ui.global::<Theme>().set_fs_app(font);
    let chain = super::super::chat_log::reply_chain(7);
    let rows: Vec<LogLine> = chain
        .iter()
        .enumerate()
        .map(|(i, l)| LogLine {
            id: l.id.as_str().into(),
            lead: l.lead.as_str().into(),
            text: l.text.as_str().into(),
            first: true,
            quote: if i == 0 { -1 } else { i32::try_from(i - 1).expect("small") },
            quote_label: l.quote_label.as_str().into(),
            quote_indent: l.quote_indent,
            patch_id: -1,
            ..LogLine::default()
        })
        .collect();
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "chat".into(),
        applied_count: 7,
        log: ModelRc::new(VecModel::from(rows)),
        ..SurfaceTab::default()
    }])));
    apply_strings(&ui, 0);
    let _shown = show_headless(&ui);
    rail_settled();
    Handle::find_by_element_id(&ui, "ChatRow::card")
        .filter(|e| e.size().width > 0.0)
        .map(|e| (e.absolute_position().x, e.size().width))
        .collect()
}

#[test]
fn reply_offsets_grow_to_the_cap_and_stay() {
    i_slint_backend_testing::init_no_event_loop();
    for width in [900, 1400] {
        for (font, scale) in [(14.0_f32, 1.0_f32), (21.0, 1.5)] {
            let cards = chain_cards(width, font);
            assert_eq!(cards.len(), 7, "{width}px x{scale}: every reply renders");
            let x0 = cards[0].0;
            let offsets: Vec<f32> = cards.iter().map(|(x, _)| x - x0).collect();
            eprintln!("width {width} scale {scale}: offsets {offsets:?} widths {:?}", cards.iter().map(|c| c.1).collect::<Vec<_>>());
            for d in 1..7 {
                assert!(offsets[d] >= offsets[d - 1] - 0.5, "{width}px x{scale}: depth {d} steps back left: {offsets:?}");
            }
            for d in 1..=3 {
                assert!((offsets[d] - d as f32 * 22.0 * scale).abs() < 0.5, "{width}px x{scale}: depth {d}: {offsets:?}");
            }
            for d in 4..7 {
                assert!((offsets[d] - offsets[3]).abs() < 0.5, "{width}px x{scale}: depth {d} past the cap: {offsets:?}");
                assert!((cards[d].1 - cards[3].1).abs() < 0.5, "{width}px x{scale}: the text column stays as wide");
            }
            assert!(cards.iter().all(|c| c.0 >= x0 && c.1 > 0.0));
        }
    }
}
