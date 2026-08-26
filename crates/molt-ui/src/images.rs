// SPDX-License-Identifier: GPL-3.0-or-later
//! Pictures: decoding proposal payloads and materialized files by CONTENT
//! (never by a peer-supplied extension), fitting a picked member picture
//! into what a republic carries, and the per-window avatar/logo caches.

use std::collections::HashMap;

/// Decode a pending `set_image` proposal's payload (base64 of the raw image
/// file) into a renderable [`slint::Image`]. The bytes rode the proposal
/// gossip (sign-what-you-see), so this runs locally on every member's
/// device — no transfer, no proposer needed. `None` on any decode failure.
pub(crate) fn proposal_image_from_b64(img_b64: &str) -> Option<slint::Image> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(img_b64)
        .ok()?;
    image_from_bytes(&bytes)
}

/// Decode raw image-file bytes into a [`slint::Image`], keyed on the
/// CONTENT — a payload carries no file name, so an extension-keyed loader
/// (`Image::load_from_path`, `image::open`) can never work here. Raster
/// formats are sniffed and decoded in memory (exactly the picker's set:
/// png/jpeg/webp/gif/bmp — pure-Rust decoders); an unsniffable payload
/// gets one try as SVG source. Untrusted peer input: decode dimensions are
/// capped so a tiny compressed bomb cannot balloon in memory.
pub(crate) fn image_from_bytes(bytes: &[u8]) -> Option<slint::Image> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    if reader.format().is_some() {
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        reader.limits(limits);
        let rgba = reader.decode().ok()?.into_rgba8();
        let (w, h) = rgba.dimensions();
        let buf =
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
        return Some(slint::Image::from_rgba8(buf));
    }
    // not a known raster signature — the one picker format without a magic
    // number is SVG (plain text): let the vector loader have a try
    slint::Image::load_from_svg_data(bytes).ok()
}

/// Why a picked picture cannot become a member-picture proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageFitError {
    /// The file is not a picture this build can read.
    Undecodable,
    /// Even the smallest allowed avatar stays over the republic's budget.
    TooLarge,
}

/// A picture ready to ride a `set_member_image` proposal.
pub(crate) struct FittedImage {
    /// The bytes that travel (base64-encoded by the caller).
    pub(crate) bytes: Vec<u8>,
    /// What those bytes REALLY are. The engine derives the avatar file's
    /// name from the proposal's display value (`proposals.rs::logo_ext`),
    /// so a re-encode must rename with it - "holiday.gif" carrying JPEG
    /// bytes would materialize a file whose extension lies.
    pub(crate) ext: &'static str,
}

/// Where the downscale starts, and where it stops. 512px is a generous
/// avatar on any display; below 128px a picture is no longer one, so the
/// honest answer there is a refusal, not a thumbnail.
const AVATAR_EDGE_START: u32 = 512;
const AVATAR_EDGE_MIN: u32 = 128;

/// Fit a picked picture into what a `set_member_image` may carry: SQUARE
/// (the engine refuses any other shape - `proposals.rs::member_image_ok`)
/// and within `budget`, the republic's own transport headroom as the
/// engine serves it (`StatusView::image_budget`).
///
/// A picture that already satisfies both travels untouched - re-encoding
/// what already fits only costs quality. Otherwise: centre-crop to the
/// shorter edge, then step the edge down until the encoded bytes fit.
/// PNG is preferred (lossless, and the only choice with real
/// transparency); a photo that PNG cannot squeeze goes JPEG.
pub(crate) fn fit_member_image(bytes: &[u8], budget: usize) -> Result<FittedImage, ImageFitError> {
    let format = image::guess_format(bytes).ok();
    let source = decode_capped(bytes).ok_or(ImageFitError::Undecodable)?;
    let (w, h) = (source.width(), source.height());
    if w == 0 || h == 0 {
        return Err(ImageFitError::Undecodable);
    }
    if let Some(ext) = format.and_then(|f| f.extensions_str().first().copied()) {
        // …but only up to the start edge: a highly compressible 6000²
        // picture would fit the budget and then cost every member's
        // window 144 MB of decoded avatar
        if w == h && w <= AVATAR_EDGE_START && bytes.len() <= budget {
            return Ok(FittedImage {
                bytes: bytes.to_vec(),
                ext,
            });
        }
    }
    let edge = w.min(h);
    let square = source.crop_imm((w - edge) / 2, (h - edge) / 2, edge, edge);
    // transparency survives only in PNG - flattening it onto JPEG's black
    // is a visible corruption, not a smaller picture. Read once: a resize
    // does not invent or lose an alpha channel
    let transparent =
        square.color().has_alpha() && square.to_rgba8().pixels().any(|p| p.0[3] != u8::MAX);
    let mut target = edge.min(AVATAR_EDGE_START);
    loop {
        let scaled = if target < edge {
            square.resize(target, target, image::imageops::FilterType::Lanczos3)
        } else {
            square.clone()
        };
        for fmt in [image::ImageFormat::Png, image::ImageFormat::Jpeg] {
            if fmt == image::ImageFormat::Jpeg && transparent {
                continue;
            }
            let encoded = match fmt {
                image::ImageFormat::Jpeg => encode(
                    &image::DynamicImage::ImageRgb8(scaled.to_rgb8()),
                    fmt,
                ),
                _ => encode(&scaled, fmt),
            };
            if let Some(out) = encoded.filter(|out| out.len() <= budget) {
                return Ok(FittedImage {
                    bytes: out,
                    ext: if fmt == image::ImageFormat::Jpeg {
                        "jpg"
                    } else {
                        "png"
                    },
                });
            }
        }
        if target <= AVATAR_EDGE_MIN {
            return Err(ImageFitError::TooLarge);
        }
        target = (target * 3 / 4).max(AVATAR_EDGE_MIN);
    }
}

/// Decode with the same 8192² ceiling [`image_from_bytes`] enforces - a
/// picked file is as untrusted as a proposed one (a tiny compressed bomb
/// balloons in memory either way).
fn decode_capped(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    reader.decode().ok()
}

/// Encode into memory. `write_to` needs `Write + Seek`, which a bare `Vec`
/// is not.
fn encode(img: &image::DynamicImage, fmt: image::ImageFormat) -> Option<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, fmt).ok()?;
    Some(out.into_inner())
}

/// The [`AvatarCache`] key for a materialized avatar file: its path plus
/// what identifies THIS version of it.
///
/// A seat that replaces its picture keeps the same file name
/// (`avatar-<stem>.<ext>`), so a path-only key would keep serving the old
/// decode forever. Runs a stat, so it belongs in the off-UI-thread gather
/// pass, never in the row mapping.
pub(crate) fn avatar_cache_key(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return path.to_string();
    };
    let stamp = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("{path}|{}|{stamp}", meta.len())
}

/// Whether the republic's picture must be decoded again, and under which
/// key. The engine materializes it as `logo.<ext>`, so REPLACING the picture
/// with the same format leaves the path byte-identical - a reload guarded on
/// the path string alone then shows the old picture until the app restarts.
/// The member avatars carry the same trap and are keyed by content for
/// exactly this reason.
pub(crate) fn logo_needs_reload(shown_key: &str, image_ref: &str) -> Option<String> {
    let key = avatar_cache_key(image_ref);
    (key != shown_key).then_some(key)
}

/// The decoded member avatars, keyed by [`avatar_cache_key`].
///
/// [`sync_rows`] rewrites EVERY row on EVERY mirror push, so a decode
/// inside the row mapping would re-decode the whole roster on every engine
/// event. This remembers the answer - the miss included, so a picture
/// whose file is not on this device does not re-read per push either.
#[derive(Default)]
pub(crate) struct AvatarCache {
    by_path: HashMap<String, Option<slint::Image>>,
}

impl AvatarCache {
    /// The image for `path`, loading it at most once.
    pub(crate) fn get(
        &mut self,
        path: &str,
        load: impl FnOnce(&str) -> Option<slint::Image>,
    ) -> Option<slint::Image> {
        if let Some(hit) = self.by_path.get(path) {
            return hit.clone();
        }
        let loaded = load(path);
        self.by_path.insert(path.to_string(), loaded.clone());
        loaded
    }

    /// Forget every path the roster no longer references - a replaced or
    /// removed picture, or another workspace's members.
    pub(crate) fn retain_live(&mut self, live: &std::collections::HashSet<&str>) {
        self.by_path.retain(|p, _| live.contains(p.as_str()));
    }
}

thread_local! {
    /// The window's one avatar cache. `apply_surfaces` runs on the UI
    /// thread only, so it needs no lock.
    /// What the window's republic picture was decoded from.
    pub(crate) static LOGO_KEY: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };

    pub(crate) static AVATARS: std::cell::RefCell<AvatarCache> =
        std::cell::RefCell::new(AvatarCache::default());
}
