// SPDX-License-Identifier: GPL-3.0-or-later
//! The republic's picture: its display shape, the status header's layout
//! and the sidebar icon (`beta_feedback_2026-11-11.md` WP6 card, WP7).

use super::*;

type Handle = i_slint_backend_testing::ElementHandle;

/// `(x, y, width, height)`.
type Rect = (f32, f32, f32, f32);

fn picture(w: u32, h: u32) -> slint::Image {
    slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h))
}

fn rect(e: &Handle) -> Rect {
    (e.absolute_position().x, e.absolute_position().y, e.size().width, e.size().height)
}

fn by_id(ui: &AppWindow, id: &str) -> Option<Rect> {
    Handle::find_by_element_id(ui, id).map(|e| rect(&e)).find(|r| r.2 > 0.0)
}

fn centre_x(r: Rect) -> f32 {
    r.0 + r.2 / 2.0
}

fn centre_y(r: Rect) -> f32 {
    r.1 + r.3 / 2.0
}

/// Organization -> Status with `title` and an optional `w`x`h` logo.
fn status_window(title: &str, logo: Option<(u32, u32)>) -> (AppWindow, Shown) {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("organization".into());
    ui.set_selected_view("status".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "organization".into(),
        name: "Organization".into(),
        ..SurfaceTab::default()
    }])));
    ui.set_active_workspace(title.into());
    if let Some((w, h)) = logo {
        ui.set_org_img(picture(w, h));
        ui.set_org_img_set(true);
    }
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    (ui, shown)
}

/// The header's `(banner, logo, title)`.
fn header(ui: &AppWindow) -> (Rect, Rect, Rect) {
    let banner = Handle::find_by_element_type_name(ui, "OrgBanner")
        .map(|e| rect(&e))
        .find(|r| r.2 > 0.0)
        .expect("the header renders");
    (
        banner,
        by_id(ui, "OrgBanner::ob-logo").expect("the logo box renders"),
        by_id(ui, "OrgBanner::ob-title").expect("the title renders"),
    )
}

const LONG: &str = "The Most Serene and Exceedingly Long Named Republic of the United \
     Allotment Gardeners, Beekeepers and Night Shift Chess Players of the Lower Valley";

fn assert_beside(what: &str, (banner, logo, title): (Rect, Rect, Rect)) {
    assert!((logo.2 - logo.3).abs() < 1.0, "{what}: the logo box is square: {logo:?}");
    assert!(logo.0 + logo.2 <= title.0, "{what}: logo left of the title: {logo:?} {title:?}");
    assert!((centre_y(logo) - centre_y(title)).abs() < 1.0, "{what}: one row");
    let pair = f32::midpoint(logo.0, title.0 + title.2);
    assert!((pair - centre_x(banner)).abs() < 1.5, "{what}: the pair is centred: {pair} in {banner:?}");
}

fn assert_stacked(what: &str, (banner, logo, title): (Rect, Rect, Rect)) {
    assert!(title.1 + title.3 <= logo.1, "{what}: the title is above the logo: {title:?} {logo:?}");
    assert!((centre_x(title) - centre_x(banner)).abs() < 1.5, "{what}: the title is centred");
    assert!((centre_x(logo) - centre_x(banner)).abs() < 1.5, "{what}: the logo is centred");
    assert!(logo.1 + logo.3 <= banner.1 + banner.3, "{what}: the logo stays inside the panel");
}

fn assert_banner_box(what: &str, logo: Rect) {
    assert!((logo.2 - logo.3 * 3.0).abs() < 1.0, "{what}: a 3:1 box: {logo:?}");
}

#[test]
fn a_square_logo_sits_left_of_a_short_title() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window("Chess Club", Some((200, 200)));
    assert_beside("square", header(&ui));
}

#[test]
fn a_square_logo_goes_below_a_long_title() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window(LONG, Some((200, 200)));
    let h = header(&ui);
    assert_stacked("long title", h);
    assert!((h.1 .2 - h.1 .3).abs() < 1.0, "still a square box: {:?}", h.1);
}

#[test]
fn a_wide_logo_is_a_banner_below_the_title() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window("Chess Club", Some((900, 150)));
    let h = header(&ui);
    assert_stacked("6:1", h);
    assert_banner_box("6:1", h.1);
}

#[test]
fn a_two_to_one_logo_is_a_banner_too() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window("Chess Club", Some((400, 200)));
    let h = header(&ui);
    assert_stacked("2:1", h);
    assert_banner_box("2:1", h.1);
}

#[test]
fn a_portrait_logo_counts_as_square() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window("Chess Club", Some((100, 300)));
    assert_beside("portrait", header(&ui));
}

#[test]
fn the_square_class_ends_at_five_to_four() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window("Chess Club", Some((125, 100)));
    assert_beside("1.25", header(&ui));
    ui.set_org_img(picture(126, 100));
    assert_banner_box("1.26", header(&ui).1);
    ui.set_org_img(picture(80, 100));
    assert_beside("0.8", header(&ui));
}

#[test]
fn the_header_without_a_logo_keeps_the_mark_beside_the_title() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window("Chess Club", None);
    assert_beside("no logo", header(&ui));
}

#[test]
fn the_sidebar_shows_a_square_logo_and_the_generic_icon_otherwise() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window("Chess Club", Some((200, 200)));
    let logo = by_id(&ui, "SurfaceRow::sr-logo").expect("a square logo is the sidebar icon");
    assert!((logo.2 - logo.3).abs() < 1.0, "undistorted: {logo:?}");
    assert!(by_id(&ui, "SurfaceRow::sr-icon").is_none(), "the logo replaces the generic icon");

    ui.set_org_img(picture(900, 150));
    assert!(by_id(&ui, "SurfaceRow::sr-logo").is_none(), "a banner is no sidebar icon");
    assert!(by_id(&ui, "SurfaceRow::sr-icon").is_some());

    ui.set_org_img_set(false);
    assert!(by_id(&ui, "SurfaceRow::sr-logo").is_none());
    assert!(by_id(&ui, "SurfaceRow::sr-icon").is_some());
}

#[test]
fn the_logo_modal_shows_the_current_picture_in_its_shape() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = status_window("Chess Club", Some((900, 150)));
    ui.set_org_logo_modal_open(true);
    assert_banner_box("modal", by_id(&ui, "AppWindow::ol-logo").expect("the modal preview renders"));
    ui.set_org_img(picture(100, 300));
    let square = by_id(&ui, "AppWindow::ol-logo").expect("the modal preview renders");
    assert!((square.2 - square.3).abs() < 1.0, "{square:?}");
}

#[test]
fn a_dropped_proposal_toasts_once() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let sv = SessionView {
        notice: "proposal-dropped:petra:too-large".to_string(),
        ..SessionView::default()
    };
    apply_session(&ui, &sv, true, &chat_ui);
    assert_eq!(
        ui.get_toast_text().as_str(),
        "Proposal from petra dropped - too large for the relays."
    );
    ui.invoke_show_toast("something else".into());
    apply_session(&ui, &sv, true, &chat_ui);
    assert_eq!(ui.get_toast_text().as_str(), "something else", "an unchanged notice re-toasted");
}

/// Organization -> Pending with one picture card.
fn card_window(name: &str, img_b64: &str) -> (AppWindow, Shown) {
    let ui = AppWindow::new().expect("headless window");
    ui.window().set_size(slint::PhysicalSize::new(1200, 800));
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("organization".into());
    ui.set_selected_view("pending".into());
    ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
        key: "organization".into(),
        pending_count: 1,
        pending: ModelRc::new(VecModel::from(vec![ProposalRow {
            id: 4,
            text: "Logo".into(),
            approvals: 1,
            threshold: 2,
            image_op: true,
            proposed: name.into(),
            img_b64: img_b64.into(),
            ..ProposalRow::default()
        }])),
        ..SurfaceTab::default()
    }])));
    apply_strings(&ui, 0);
    let shown = show_headless(&ui);
    (ui, shown)
}

#[test]
fn the_card_offers_preview_and_save_whenever_the_bytes_are_there() {
    i_slint_backend_testing::init_no_event_loop();
    for name in ["", "crest.png", "no extension"] {
        let (ui, _shown) = card_window(name, "aGVsbG8=");
        assert!(
            by_id(&ui, "ProposalCard::pi-line").is_some(),
            "name {name:?}: preview and save depend on the bytes"
        );
    }
    let (ui, _shown) = card_window("crest.png", "");
    assert!(by_id(&ui, "ProposalCard::pi-line").is_none(), "no bytes, nothing to save");
}

#[test]
fn the_card_previews_the_proposed_picture_in_its_shape() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _shown) = card_window("crest.png", "aGVsbG8=");
    ui.set_img_inline_src(picture(900, 150));
    ui.set_img_inline_id(4);
    assert_banner_box("card", by_id(&ui, "ProposalCard::pi-view").expect("the inline preview renders"));
    ui.set_img_inline_src(picture(100, 300));
    let square = by_id(&ui, "ProposalCard::pi-view").expect("the inline preview renders");
    assert!((square.2 - square.3).abs() < 1.0, "{square:?}");
}
