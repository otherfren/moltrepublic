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

#[test]
fn raster_dimension_limits_include_8192_but_reject_8193_on_either_axis() {
    for (w, h) in [(8192, 1), (1, 8192)] {
        let bytes = noisy_png(w, h);
        let preview = image_from_bytes(&bytes).expect("boundary raster decodes");
        assert_eq!((preview.size().width, preview.size().height), (w, h));
        assert!(crate::images::decode_wiki_image(&bytes).is_some());
        assert!(fit_member_image(&bytes, usize::MAX).is_ok());
    }
    for (w, h) in [(8193, 1), (1, 8193)] {
        let bytes = noisy_png(w, h);
        assert!(image_from_bytes(&bytes).is_none());
        assert!(crate::images::decode_wiki_image(&bytes).is_none());
        assert!(matches!(
            fit_member_image(&bytes, usize::MAX),
            Err(ImageFitError::Undecodable)
        ));
    }
}

#[test]
fn svg_fallback_is_only_for_previews_and_broken_rasters_stay_rejected() {
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#f00"/></svg>"##;
    assert!(image_from_bytes(svg).is_some());
    for bytes in [
        svg.as_slice(),
        b"",
        b"not an image",
        b"\x89PNG\r\n\x1a\n",
        b"GIF89a",
        b"BM",
    ] {
        assert!(crate::images::decode_wiki_image(bytes).is_none());
        assert!(matches!(
            fit_member_image(bytes, usize::MAX),
            Err(ImageFitError::Undecodable)
        ));
        if bytes != svg {
            assert!(image_from_bytes(bytes).is_none());
        }
    }
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

#[test]
fn cropping_and_resizing_preserve_exact_png_and_jpeg_output() {
    for edge in [160, 600] {
        let wide = noisy_png(edge * 2, edge);
        let square = image::load_from_memory(&wide)
            .expect("source decodes")
            .crop_imm(edge / 2, 0, edge, edge);
        let expected = if edge > 512 {
            square.resize(512, 512, image::imageops::FilterType::Lanczos3)
        } else {
            square
        };
        let mut png = std::io::Cursor::new(Vec::new());
        expected
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("png");
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(expected.to_rgb8())
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .expect("jpeg");
        let (png, jpeg) = (png.into_inner(), jpeg.into_inner());
        assert!(
            jpeg.len() < png.len(),
            "fixture must exercise jpeg fallback"
        );
        for (expected, ext) in [(png, "png"), (jpeg, "jpg")] {
            let fitted = fit_member_image(&wide, expected.len()).expect("crop fits budget");
            assert_eq!(fitted.ext, ext);
            assert!(fitted.bytes == expected, "{edge}px {ext} bytes changed");
        }
    }
}

#[test]
fn transparent_crop_preserves_alpha_and_never_falls_back_to_jpeg() {
    let rgba = image::RgbaImage::from_fn(40, 20, |x, y| {
        image::Rgba([x as u8, y as u8, 127, (x * 5) as u8])
    });
    let source = image::DynamicImage::ImageRgba8(rgba);
    let mut input = std::io::Cursor::new(Vec::new());
    source
        .write_to(&mut input, image::ImageFormat::Png)
        .expect("input");
    let mut expected = std::io::Cursor::new(Vec::new());
    source
        .crop_imm(10, 0, 20, 20)
        .write_to(&mut expected, image::ImageFormat::Png)
        .expect("crop");
    let expected = expected.into_inner();
    let fitted = fit_member_image(input.get_ref(), expected.len()).expect("png fits");
    assert_eq!(fitted.ext, "png");
    assert!(fitted.bytes == expected, "transparent crop bytes changed");
    assert!(matches!(
        fit_member_image(input.get_ref(), fitted.bytes.len() - 1),
        Err(ImageFitError::TooLarge)
    ));
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
/// (`avatar-<stem>.<ext>`, `logo.<ext>`), so a name-only cache key would
/// keep showing the old picture until the app restarts. The key moves
/// with the applied picture changes instead.
#[test]
fn a_replaced_picture_moves_its_key_although_its_name_is_unchanged() {
    use crate::bytes::{picture_key, pictures_version};
    let set = |bytes: &str| serde_json::json!({ "op": "set_image", "value": "logo.png", "bytes_b64": bytes });
    let first = pictures_version(&[set("AAAA")]);
    assert_eq!(first, pictures_version(&[set("AAAA")]), "an unchanged log keys the same");
    let replaced = pictures_version(&[set("AAAA"), set("BBBB")]);
    assert_ne!(first, replaced, "a replaced picture must not show the old decode");
    assert_ne!(picture_key("/w/logo.png", first), picture_key("/w/logo.png", replaced));
    assert!(picture_key("/w/logo.png", first).starts_with("/w/logo.png"), "the key still names the picture");

    // a member's picture counts, another change of the log does not
    let face = serde_json::json!({ "op": "set_member_image", "member": "ana", "value": "a.png", "bytes_b64": "CCCC" });
    assert_ne!(first, pictures_version(&[set("AAAA"), face]));
    let other = serde_json::json!({ "op": "set_name", "value": "Chess Club" });
    assert_eq!(first, pictures_version(&[set("AAAA"), other]), "pictures are not read again for a rename");
    let removed = serde_json::json!({ "op": "remove_image" });
    assert_ne!(first, pictures_version(&[set("AAAA"), removed]));

    assert_eq!(picture_key("", first), "", "no picture, no key");
}

/// What a picker answered names the file after its last separator, of
/// either kind; an export target without one is a bare name.
#[test]
fn a_picked_file_is_named_after_its_last_separator() {
    use crate::bytes::{file_name, is_bare_name};
    assert_eq!(file_name("/home/u/face.png"), "face.png");
    assert_eq!(file_name("C:\\pics\\face.png"), "face.png");
    assert_eq!(file_name("face.png"), "face.png");
    assert!(is_bare_name("club.molt.enc"));
    // the engine's own reading: no separator, so its download folder
    assert!(is_bare_name("~club.molt.enc"));
    for path in ["", "~/club.molt.enc", "/srv/club.molt.enc", "backups/club.molt.enc", "C:\\club.molt.enc"] {
        assert!(!is_bare_name(path), "{path:?}");
    }
}

/// A republic that is not stored names pictures the engine refuses to
/// open; every surfaces pass asking again would be a round trip per
/// picture per event. A refusal is remembered until the pictures change;
/// a read that failed otherwise (a dropped connection) is asked again.
#[test]
fn a_refused_picture_is_asked_for_once_per_state_of_the_pictures() {
    use crate::bytes::{hold_with, picture_key};
    use molt_core::MoltError;
    let rt = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
    let chat_ui = std::sync::Arc::new(std::sync::Mutex::new(crate::surfaces::ChatUiState::default()));
    let asked = std::cell::RefCell::new(Vec::<String>::new());
    let read = |image: String| {
        asked.borrow_mut().push(image.clone());
        async move {
            match image.as_str() {
                "refused.png" => Err(MoltError::BadPayload("unknown image".into())),
                "lost.png" => Err(MoltError::Engine("connection closed".into())),
                _ => Ok(vec![1, 2, 3]),
            }
        }
    };
    let wanted = |version: u64| -> Vec<(String, String)> {
        ["refused.png", "lost.png", "a.png"].iter().map(|i| (picture_key(i, version), (*i).to_string())).collect()
    };
    let count = |image: &str| asked.borrow().iter().filter(|a| *a == image).count();

    let held = rt.block_on(hold_with(&chat_ui, &wanted(1), read));
    assert_eq!(held.len(), 1, "only the picture that read is held");
    assert!(held.contains_key(&picture_key("a.png", 1)));
    rt.block_on(hold_with(&chat_ui, &wanted(1), read));
    assert_eq!(count("a.png"), 1);
    assert_eq!(count("refused.png"), 1, "the refusal is remembered");
    assert_eq!(count("lost.png"), 2, "a read that failed is asked again");

    rt.block_on(hold_with(&chat_ui, &wanted(2), read));
    assert_eq!(count("refused.png"), 2, "the pictures changed: asked again");
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

/// An older or minimal proposal may carry bytes without a display name:
/// the card and the save dialog still get one.
#[test]
fn an_unnamed_picture_card_gets_a_fallback_name() {
    let mut v = profile_view("set_image", "");
    v.payload = serde_json::json!({ "op": "set_image", "bytes_b64": "QUJD" });
    assert_eq!(proposal_row(0, &v).proposed, "image");
    v.proposed = "crest.png".to_string();
    assert_eq!(proposal_row(0, &v).proposed, "crest.png");
    // a removal carries no bytes and stays unnamed
    v.proposed = String::new();
    v.payload = serde_json::json!({ "op": "remove_image" });
    assert_eq!(proposal_row(0, &v).proposed, "");
}
