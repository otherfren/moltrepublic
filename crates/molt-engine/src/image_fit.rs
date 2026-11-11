// SPDX-License-Identifier: GPL-3.0-or-later
//! Fit a proposed picture into the transport budget: fewer pixels, same
//! format (`docs/reviews/beta_feedback_2026-11-11.md` L4, L9).

use image::ImageFormat;
use molt_core::MoltError;

/// The same 8192² ceiling as the propose sniff.
const MAX_DIM: u32 = 8192;
/// Below this longest edge a logo is no longer one: refuse instead.
const MIN_EDGE: u32 = 48;
/// The largest picture file the node reads for a `path` proposal.
pub(crate) const SOURCE_MAX: u64 = 32 * 1024 * 1024;

fn undecodable() -> MoltError {
    MoltError::BadPayload("the image cannot be decoded (png/jpeg/webp/gif/bmp)".into())
}

fn too_large(len: usize, budget: usize) -> MoltError {
    MoltError::BadPayload(format!(
        "the image is {} KiB - this republic's relays carry {} KiB",
        len / 1024,
        budget / 1024
    ))
}

/// `bytes` when they fit `budget`, else the same picture in the same format
/// with fewer pixels. An animation is refused: its frames cannot be kept.
pub(crate) fn fit_image(bytes: &[u8], budget: usize) -> Result<Vec<u8>, MoltError> {
    if bytes.len() <= budget {
        return Ok(bytes.to_vec());
    }
    let format = image::guess_format(bytes).map_err(|_| undecodable())?;
    if animated(bytes, format) {
        return Err(too_large(bytes.len(), budget));
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes));
    reader.set_format(format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    reader.limits(limits);
    let source = reader.decode().map_err(|_| undecodable())?;
    let (w, h) = (u64::from(source.width()), u64::from(source.height()));
    if w == 0 || h == 0 {
        return Err(undecodable());
    }
    // encoded size follows the pixel count, so the edge follows its root
    let (len, cap) = (u64::try_from(bytes.len()).unwrap_or(u64::MAX), u64::try_from(budget).unwrap_or(0));
    let ratio = cap.saturating_mul(1_000_000) / len;
    let mut per_mille = (1..=900u64).rev().find(|r| r * r <= ratio).unwrap_or(1);
    loop {
        let tw = u32::try_from(w * per_mille / 1000).unwrap_or(0).max(1);
        let th = u32::try_from(h * per_mille / 1000).unwrap_or(0).max(1);
        if tw.max(th) < MIN_EDGE {
            return Err(too_large(bytes.len(), budget));
        }
        let scaled = source.resize_exact(tw, th, image::imageops::FilterType::Lanczos3);
        let out = encode(&scaled, format).ok_or_else(|| too_large(bytes.len(), budget))?;
        if out.len() <= budget {
            return Ok(out);
        }
        per_mille = per_mille * 4 / 5;
    }
}

/// More than one frame? A resize would keep only the first.
fn animated(bytes: &[u8], format: ImageFormat) -> bool {
    use image::AnimationDecoder as _;
    let cur = std::io::Cursor::new(bytes);
    match format {
        ImageFormat::Gif => image::codecs::gif::GifDecoder::new(cur)
            .is_ok_and(|d| d.into_frames().take(2).count() > 1),
        ImageFormat::WebP => {
            image::codecs::webp::WebPDecoder::new(cur).is_ok_and(|d| d.has_animation())
        }
        ImageFormat::Png => image::codecs::png::PngDecoder::new(cur)
            .is_ok_and(|d| d.is_apng().unwrap_or(false)),
        _ => false,
    }
}

/// Re-encode in `format`; `None` when this build has no encoder for it.
fn encode(img: &image::DynamicImage, format: ImageFormat) -> Option<Vec<u8>> {
    // every encoder here takes 8-bit; only PNG also keeps 16-bit and grey
    let img = match format {
        ImageFormat::Png => img.clone(),
        ImageFormat::Jpeg => image::DynamicImage::ImageRgb8(img.to_rgb8()),
        _ if img.color().has_alpha() => image::DynamicImage::ImageRgba8(img.to_rgba8()),
        _ => image::DynamicImage::ImageRgb8(img.to_rgb8()),
    };
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, format).ok()?;
    Some(out.into_inner())
}

/// The extension a picture's own bytes name ("png" when unknown).
pub(crate) fn sniffed_ext(bytes: &[u8]) -> &'static str {
    image::guess_format(bytes)
        .ok()
        .and_then(|f| f.extensions_str().first().copied())
        .unwrap_or("png")
}

/// Read a picture file for a `path` proposal, capped at [`SOURCE_MAX`].
pub(crate) fn read_source(path: &std::path::Path) -> Result<Vec<u8>, MoltError> {
    let unreadable = || MoltError::BadPayload(format!("cannot read {}", path.display()));
    let meta = std::fs::metadata(path).map_err(|_| unreadable())?;
    if meta.len() > SOURCE_MAX {
        return Err(MoltError::BadPayload(format!(
            "the file is {} MiB - the limit is {} MiB",
            meta.len() / (1024 * 1024),
            SOURCE_MAX / (1024 * 1024)
        )));
    }
    std::fs::read(path).map_err(|_| unreadable())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A `w`x`h` picture of non-repeating pixels, so no encoder squeezes it.
    pub(crate) fn noisy(w: u32, h: u32, format: ImageFormat) -> Vec<u8> {
        let mut seed = 0x9e37_79b9_u32;
        let img = image::RgbImage::from_fn(w, h, |_, _| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let [r, g, b, _] = seed.to_le_bytes();
            image::Rgb([r, g, b])
        });
        encode(&image::DynamicImage::ImageRgb8(img), format).expect("encodes")
    }

    #[test]
    fn an_oversized_picture_lands_under_the_cap_in_its_own_format() {
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::WebP,
            ImageFormat::Gif,
            ImageFormat::Bmp,
        ] {
            let source = noisy(600, 300, format);
            let budget = 40 * 1024;
            assert!(source.len() > budget, "{format:?}: the fixture must be oversized");
            let out = fit_image(&source, budget).unwrap_or_else(|e| panic!("{format:?}: {e:?}"));
            assert!(out.len() <= budget, "{format:?}: {} over {budget}", out.len());
            assert_eq!(image::guess_format(&out).expect("sniffs"), format);
            let img = image::load_from_memory(&out).expect("decodes");
            assert!(img.width() < 600 && img.width() >= MIN_EDGE, "{format:?}: {}", img.width());
            // 2:1 stays 2:1 (rounding aside): pixels shrink, nothing is cropped
            assert!(img.width().abs_diff(img.height() * 2) <= 2, "{format:?}");
        }
    }

    #[test]
    fn a_fitting_picture_travels_untouched() {
        let source = noisy(40, 40, ImageFormat::Png);
        assert_eq!(fit_image(&source, source.len()).expect("fits"), source);
    }

    #[test]
    fn a_picture_no_downscale_can_fit_is_refused_with_the_size_message() {
        let source = noisy(600, 300, ImageFormat::Bmp);
        let MoltError::BadPayload(msg) = fit_image(&source, 512).expect_err("512 B holds no logo")
        else {
            panic!("expected BadPayload");
        };
        assert!(msg.contains("KiB"), "{msg}");
    }

    #[test]
    fn an_animation_is_refused_not_flattened() {
        let frame = || image::Frame::new(image::RgbaImage::from_fn(64, 64, |x, y| {
            let v = u8::try_from((x * 7 + y * 13) % 251).expect("below 256");
            image::Rgba([v, v ^ 0x5a, v.wrapping_mul(3), 255])
        }));
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif)
            .encode_frames([frame(), frame()])
            .expect("encodes");
        fit_image(&gif, gif.len() / 2).expect_err("two frames cannot be kept");
    }

    #[test]
    fn garbage_is_refused_as_undecodable() {
        let MoltError::BadPayload(msg) = fit_image(&[7u8; 4096], 1024).expect_err("no picture")
        else {
            panic!("expected BadPayload");
        };
        assert!(msg.contains("cannot be decoded"), "{msg}");
    }
}
