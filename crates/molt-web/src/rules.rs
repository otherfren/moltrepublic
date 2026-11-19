// SPDX-License-Identifier: GPL-3.0-or-later
//! What the browser build decides without a browser: plain values in,
//! plain values out. Compiled and tested on every target.

/// The connection is up and the server's build is this bundle's: the
/// first thing the page hears.
pub const READY: &str = "ready";
/// The server is another build than this bundle (`docs/web_gui.md` §5.1).
pub const VERSION: &str = "version";
/// The server refused the key (close 4401): at the hello, or after a
/// rotation.
pub const REFUSED: &str = "refused";
/// One side broke the wire (close 4400, or a frame this client cannot
/// read).
pub const PROTOCOL: &str = "protocol";
/// The connection ended for any other reason.
pub const CLOSED: &str = "closed";

/// The server dropped events and the window reads everything again: a
/// passing note, the connection stays.
pub const LAG: &str = "lag";

/// The web door's close code for a key it does not accept. This file
/// builds natively with no dependency, so the value is repeated here and
/// a test holds it to `molt_core::wire`'s.
const CLOSE_UNAUTHORIZED: u16 = 4401;
/// The web door's close code for a broken frame; pinned the same way.
const CLOSE_PROTOCOL: u16 = 4400;

/// Why a connection ended, as the page is told: the socket's close code
/// if it had one, and whether this client found the server's frames
/// broken.
#[must_use]
pub fn why_ended(close_code: Option<u16>, broken: bool) -> &'static str {
    match close_code {
        Some(CLOSE_UNAUTHORIZED) => REFUSED,
        Some(CLOSE_PROTOCOL) => PROTOCOL,
        _ if broken => PROTOCOL,
        _ => CLOSED,
    }
}

/// The door's socket for a page served from `host` (with its port) over
/// `protocol` (`location.protocol`, colon included). `None`: a page that
/// was not served over HTTP has no door.
#[must_use]
pub fn socket_url(protocol: &str, host: &str) -> Option<String> {
    let scheme = match protocol {
        "http:" => "ws",
        "https:" => "wss",
        _ => return None,
    };
    (!host.is_empty()).then(|| format!("{scheme}://{host}/ws"))
}

/// The window's language index (0 = English, 1 = German) for a browser
/// language tag, until the session names the node's own.
#[must_use]
pub fn lang_of(tag: &str) -> i32 {
    let primary = tag.split(['-', '_']).next().unwrap_or_default();
    i32::from(primary.eq_ignore_ascii_case("de"))
}

/// The window's size for a viewport of `width` x `height` CSS pixels
/// (`innerWidth`, `innerHeight`), or `None` for a viewport that has no
/// size to follow (a tab in the background may report zero).
#[must_use]
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)] // a viewport is far inside f32's exact range
pub fn window_size(width: f64, height: f64) -> Option<(f32, f32)> {
    let usable = |side: f64| side.is_finite() && (1.0..=65_535.0).contains(&side);
    (usable(width) && usable(height)).then_some((width as f32, height as f32))
}

/// What a picked file is called towards the window: the pick's number,
/// then the file's own name after the last `/` (the platform seam's rule;
/// a browser has no directory to show).
#[must_use]
pub fn pick_name(pick: u64, file_name: &str) -> String {
    let name = file_name.rsplit(['/', '\\']).next().unwrap_or_default();
    format!("{pick}/{name}")
}

/// The pick number inside a [`pick_name`].
#[must_use]
pub fn pick_id(named: &str) -> Option<u64> {
    named.split_once('/')?.0.parse().ok()
}

/// Whether a file of `size` bytes (a browser reports a float) is over
/// `ceiling`; a size that is no number is.
#[must_use]
#[allow(clippy::as_conversions)] // u64 to f64 has no lossless form; the ceiling is far below 2^53
pub fn over_ceiling(size: f64, ceiling: u64) -> bool {
    size.is_nan() || size > ceiling as f64
}

/// A minted key as text: lowercase hex, the shape `moltd` mints.
#[must_use]
pub fn token_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// How many random bytes a minted key holds (as `moltd`'s).
pub const TOKEN_BYTES: usize = 24;

#[cfg(test)]
mod tests {
    use super::*;

    /// The codes read here are the ones the door closes with.
    #[test]
    fn the_close_codes_are_the_wire_s() {
        assert_eq!(CLOSE_UNAUTHORIZED, molt_core::wire::CLOSE_UNAUTHORIZED);
        assert_eq!(CLOSE_PROTOCOL, molt_core::wire::CLOSE_PROTOCOL);
    }

    #[test]
    fn a_refused_key_is_told_from_a_broken_wire_and_a_plain_close() {
        assert_eq!(why_ended(Some(CLOSE_UNAUTHORIZED), false), REFUSED);
        assert_eq!(why_ended(Some(CLOSE_UNAUTHORIZED), true), REFUSED, "the server's word first");
        assert_eq!(why_ended(Some(CLOSE_PROTOCOL), false), PROTOCOL);
        assert_eq!(why_ended(None, true), PROTOCOL);
        assert_eq!(why_ended(Some(1006), true), PROTOCOL);
        assert_eq!(why_ended(Some(1000), false), CLOSED);
        assert_eq!(why_ended(Some(1006), false), CLOSED);
        assert_eq!(why_ended(None, false), CLOSED);
    }

    #[test]
    fn the_socket_is_on_the_page_s_own_host() {
        assert_eq!(socket_url("http:", "127.0.0.1:8765").as_deref(), Some("ws://127.0.0.1:8765/ws"));
        assert_eq!(socket_url("http:", "localhost").as_deref(), Some("ws://localhost/ws"));
        assert_eq!(socket_url("https:", "[::1]:9").as_deref(), Some("wss://[::1]:9/ws"));
        assert_eq!(socket_url("file:", ""), None);
        assert_eq!(socket_url("http:", ""), None);
    }

    #[test]
    fn a_german_browser_starts_in_german() {
        assert_eq!(lang_of("de"), 1);
        assert_eq!(lang_of("de-AT"), 1);
        assert_eq!(lang_of("DE_ch"), 1);
        assert_eq!(lang_of("en-US"), 0);
        assert_eq!(lang_of("den"), 0);
        assert_eq!(lang_of(""), 0);
    }

    #[test]
    fn the_window_follows_a_viewport_that_has_a_size() {
        assert_eq!(window_size(1280.0, 720.5), Some((1280.0, 720.5)));
        assert_eq!(window_size(0.0, 720.0), None, "a hidden tab");
        assert_eq!(window_size(1280.0, f64::NAN), None);
        assert_eq!(window_size(f64::INFINITY, 720.0), None);
        assert_eq!(window_size(-5.0, 720.0), None);
        assert_eq!(window_size(1e9, 720.0), None);
    }

    /// The words are distinct: the page switches on them.
    #[test]
    fn every_word_the_page_hears_is_its_own() {
        let words = [READY, VERSION, REFUSED, PROTOCOL, CLOSED, LAG];
        let distinct: std::collections::BTreeSet<&str> = words.into_iter().collect();
        assert_eq!(distinct.len(), words.len());
    }

    #[test]
    fn a_pick_names_its_file_after_the_last_slash() {
        let named = pick_name(7, "bericht.bin");
        assert_eq!(named, "7/bericht.bin");
        assert_eq!(pick_id(&named), Some(7));
        // a name may not smuggle a directory in front of itself
        assert_eq!(pick_name(8, "a/b\\c.txt"), "8/c.txt");
        assert_eq!(pick_id("x/c.txt"), None);
        assert_eq!(pick_id("c.txt"), None);
        assert_eq!(pick_id("/c.txt"), None);
    }

    #[test]
    fn a_file_is_measured_before_it_is_read() {
        let ceiling = 64 * 1024 * 1024;
        assert!(!over_ceiling(0.0, ceiling));
        assert!(!over_ceiling(67_108_864.0, ceiling));
        assert!(over_ceiling(67_108_865.0, ceiling));
        assert!(over_ceiling(f64::NAN, ceiling));
        assert!(over_ceiling(f64::INFINITY, ceiling));
    }

    /// A key minted in the browser reads like one `moltd` minted.
    #[test]
    fn a_minted_key_has_the_node_s_shape() {
        let node = molt_config::random_token().expect("a token");
        let here = token_hex(&[0xab; TOKEN_BYTES]);
        assert_eq!(here.len(), node.len());
        assert!(here.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "{here}");
        assert!(node.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(token_hex(&[0x00, 0x0f, 0xf0]), "000ff0");
    }
}
