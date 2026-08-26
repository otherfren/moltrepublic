// SPDX-License-Identifier: GPL-3.0-or-later
//! Pictures: content-keyed decoding, the member-picture fit, the caches.

use super::channels::view_of;
use super::*;

/// The pending-card image preview decodes the payload bytes that rode
/// the `set_image` proposal — for EVERY format the propose-side picker
/// offers (png, jpg, jpeg, webp, gif, svg, bmp). The decode must key on
/// the CONTENT, never on a file extension: the payload is raw bytes, no
/// name travels with it. (This pins the bug where the bytes were staged
/// as a `.img` temp file and `slint::Image::load_from_path` — which
/// trusts extensions — failed for every proposal, so "Click to view the
/// proposed image" only ever produced the failure toast.)
#[test]
fn a_proposed_image_decodes_from_the_payload_for_every_picker_format() {
    // real minimal files, one per picker format (2x2 red, PIL-generated)
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
    let gif = "R0lGODdhAgACAIEAAMgeHgAAAAAAAAAAACwAAAAAAgACAAAIBgABCAQQEAA7";
    let bmp = "Qk1GAAAAAAAAADYAAAAoAAAAAgAAAAIAAAABABgAAAAAABAAAADEDgAAxA4AAAAAAAAAAAAAHh7IHh7IAAAeHsgeHsgAAA==";
    let webp = "UklGRjoAAABXRUJQVlA4IC4AAACwAQCdASoCAAIAAUAmJaACdLoABDAAAP7x3I/4DdfFtMv/vYL/3YL/3YL/WwAA";
    let jpeg = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAACAAIDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwDkKKKK8U/TD//Z";
    for (fmt, b64) in [
        ("png", png),
        ("gif", gif),
        ("bmp", bmp),
        ("webp", webp),
        ("jpeg", jpeg),
    ] {
        let img = proposal_image_from_b64(b64);
        assert!(img.is_some(), "the {fmt} payload must decode");
        let img = img.expect("checked above");
        assert_eq!(img.size().width, 2, "{fmt} decodes to the real picture");
        assert_eq!(img.size().height, 2, "{fmt} decodes to the real picture");
    }
    // svg travels as its source text
    use base64::Engine as _;
    let svg = base64::engine::general_purpose::STANDARD.encode(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#f00"/></svg>"##,
    );
    assert!(
        proposal_image_from_b64(&svg).is_some(),
        "an svg payload must decode"
    );
}

/// Undecodable payloads answer `None` — the caller shows the honest
/// "could not be decoded" toast, never a broken image.
#[test]
fn an_undecodable_image_payload_is_none_not_a_panic() {
    assert!(proposal_image_from_b64("").is_none(), "empty payload");
    assert!(
        proposal_image_from_b64("not base64 at all!").is_none(),
        "not base64"
    );
    use base64::Engine as _;
    let garbage = base64::engine::general_purpose::STANDARD.encode([0x00u8; 64]);
    assert!(
        proposal_image_from_b64(&garbage).is_none(),
        "valid base64, but not an image"
    );
}

// ---------------------------------------------------------------
// Member profiles (`member_profiles_plan.md` §5): the picture a seat
// proposes for itself is fitted HERE - square and inside this
// republic's served budget - before the engine ever sees it.
// ---------------------------------------------------------------

/// A `w x h` picture with incompressible content: a flat colour would
/// fit any budget at any edge and prove nothing about the downscale.
pub(super) fn noisy_png(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    let mut seed: u32 = 0x1234_5678;
    for p in img.pixels_mut() {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *p = image::Rgb([(seed >> 16) as u8, (seed >> 8) as u8, seed as u8]);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

/// The engine refuses a non-square member picture (every frontend
/// renders it in a square box), so the fit crops from the CENTRE -
/// a top-left crop would behead every portrait.
#[test]
fn a_wide_picture_is_center_cropped_to_a_square() {
    use image::GenericImageView as _;
    let wide = noisy_png(40, 20);
    let fitted = fit_member_image(&wide, 1 << 20).expect("a small picture fits");
    let out = image::load_from_memory(&fitted.bytes).expect("the fit stays a picture");
    assert_eq!(
        out.width(),
        out.height(),
        "the engine refuses a non-square picture"
    );
    assert_eq!(out.width(), 20, "the square is the shorter edge");
    let src = image::load_from_memory(&wide).expect("source decodes");
    assert_eq!(
        out.get_pixel(0, 0),
        src.get_pixel(10, 0),
        "the crop starts at the middle, not at the left edge"
    );
}

/// The served budget is the promise the engine keeps; a picture over
/// it is stepped down until it fits, not sent to be refused.
#[test]
fn an_oversized_picture_lands_inside_the_budget() {
    let big = noisy_png(1024, 1024);
    let budget = 40 * 1024;
    assert!(big.len() > budget, "the fixture must actually be oversized");
    let fitted = fit_member_image(&big, budget).expect("a downscale fits it");
    assert!(
        fitted.bytes.len() <= budget,
        "{} bytes over a {budget} byte budget",
        fitted.bytes.len()
    );
    image::load_from_memory(&fitted.bytes).expect("the fit stays a picture");
}

/// A picture that is already square and already small travels as the
/// bytes the user picked - a re-encode would only lose quality.
#[test]
fn a_picture_that_already_fits_is_proposed_untouched() {
    let small = noisy_png(64, 64);
    let fitted = fit_member_image(&small, 1 << 20).expect("it fits");
    assert_eq!(fitted.bytes, small, "no re-encode when none is needed");
    assert_eq!(fitted.ext, "png", "the name must not lie about the format");
}

/// Below the floor the honest answer is a refusal: a 128px avatar that
/// still does not fit means the republic has no room for a picture.
#[test]
fn a_budget_below_the_floor_is_refused_honestly() {
    let big = noisy_png(1024, 1024);
    assert!(
        matches!(fit_member_image(&big, 400), Err(ImageFitError::TooLarge)),
        "an unreachable budget must refuse, never ship a 1px avatar"
    );
}

/// Undecodable bytes are caught by the frontend's real decoder, the
/// same pre-check `on_org_propose` runs for the logo.
#[test]
fn undecodable_bytes_never_reach_the_proposal() {
    assert!(matches!(
        fit_member_image(b"not an image at all", 1 << 20),
        Err(ImageFitError::Undecodable)
    ));
}

/// A seat that REPLACES its picture keeps the same file name
/// (`avatar-<stem>.<ext>`), so a path-only cache key would keep
/// showing the old face until the app restarts. The key carries the
/// file's identity, not just its name.
/// The republic's picture must survive a REPLACEMENT: same file name,
/// new content. A path compare says "unchanged" and the window keeps the
/// old logo until a restart - the bug this rule replaced.
#[test]
fn a_replaced_logo_forces_a_reload_although_its_path_is_unchanged() {
    let tmp = tempfile::tempdir().expect("tmp");
    let logo = tmp.path().join("logo.png");
    let path = logo.display().to_string();
    std::fs::write(&logo, noisy_png(8, 8)).expect("write the first logo");

    let first = super::logo_needs_reload("", &path).expect("a first picture always loads");
    assert_eq!(
        super::logo_needs_reload(&first, &path),
        None,
        "an unchanged picture must not be decoded again on every push"
    );

    std::fs::write(&logo, noisy_png(16, 16)).expect("replace the logo");
    let second = super::logo_needs_reload(&first, &path)
        .expect("a replaced picture must reload behind its unchanged path");
    assert_ne!(first, second, "the key moves with the content");

    assert_eq!(
        super::logo_needs_reload("", ""),
        None,
        "a republic without a picture never reloads one"
    );
}

#[test]
fn the_avatar_cache_key_moves_when_the_file_content_does() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("avatar-walter.png");
    std::fs::write(&path, noisy_png(8, 8)).expect("write");
    let p = path.display().to_string();
    let first = avatar_cache_key(&p);
    assert!(first.starts_with(&p), "the key still names the file: {first}");
    assert_eq!(first, avatar_cache_key(&p), "an untouched file keys the same");
    // the same NAME, a different picture
    std::fs::write(&path, noisy_png(16, 16)).expect("rewrite");
    assert_ne!(
        first,
        avatar_cache_key(&p),
        "a replaced picture must invalidate the cached decode"
    );
    assert_eq!(avatar_cache_key(""), "", "no picture, no key");
}

/// `sync_rows` rewrites EVERY row on EVERY mirror push, so a decode
/// inside the row mapping would re-decode the whole roster per tick.
#[test]
fn an_avatar_decodes_once_per_path_and_forgets_the_gone_ones() {
    let mut cache = AvatarCache::default();
    let loads = std::cell::Cell::new(0);
    let load = |_p: &str| {
        loads.set(loads.get() + 1);
        Some(slint::Image::default())
    };
    assert!(cache.get("/w/avatar-a.png", load).is_some());
    assert!(cache.get("/w/avatar-a.png", load).is_some());
    assert_eq!(loads.get(), 1, "one decode per path, not per push");
    // a miss is remembered too - a picture whose file is not on this
    // device must not re-stat on every tick either
    let missing = |_p: &str| {
        loads.set(loads.get() + 1);
        None
    };
    assert!(cache.get("/w/gone.png", missing).is_none());
    assert!(cache.get("/w/gone.png", missing).is_none());
    assert_eq!(loads.get(), 2, "the miss is cached like the hit");
    let live: std::collections::HashSet<&str> = ["/w/avatar-a.png"].into_iter().collect();
    cache.retain_live(&live);
    assert!(cache.get("/w/gone.png", missing).is_none());
    assert_eq!(loads.get(), 3, "a dropped path decodes again");
}

/// One `ProposalView` carrying a member-profile payload.
fn profile_view(op: &str, member: &str) -> ProposalView {
    let mut v = view_of(1, "", ProposalState::Proposed);
    v.surface = Surface::Organization;
    v.payload = serde_json::json!({ "op": op, "member": member });
    v
}

/// A member picture rides the same inline-preview and save path the
/// org logo has - the bytes are in the payload either way.
#[test]
fn a_member_picture_proposal_offers_the_preview() {
    for op in ["set_member_image", "remove_member_image"] {
        assert!(
            proposal_row(0, &profile_view(op, "walter")).image_op,
            "{op} must render as a picture change"
        );
    }
    assert!(
        !proposal_row(0, &profile_view("set_member_desc", "walter")).image_op,
        "a description carries no picture"
    );
    let mut v = profile_view("set_member_image", "walter");
    v.payload["bytes_b64"] = serde_json::json!("QUJD");
    assert_eq!(
        proposal_row(0, &v).img_b64,
        "QUJD",
        "the bytes reach the preview"
    );
}
