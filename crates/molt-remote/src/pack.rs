// SPDX-License-Identifier: GPL-3.0-or-later
//! The door's encoders (`docs/web_gui.md` §5.6): the bundle's two content
//! codings, made once at start, and the deflate of a packed frame. The
//! codecs are `flate2` (on `miniz_oxide`) and `brotli`, both pure Rust;
//! the levels are the measured ones.

use std::io::Write;

use flate2::Compression;

/// gzip level for a bundle file: 9 is 0.2 % smaller for twice the time.
const GZIP_LEVEL: u32 = 6;
/// brotli quality for a bundle file: the knee of the curve (10 and 11
/// cost half a minute and more of start for the module).
const BROTLI_QUALITY: u32 = 5;
/// brotli window, log2: 4 MiB. A larger one doubles the encoder's memory
/// for nothing measurable.
const BROTLI_WINDOW: u32 = 22;
/// deflate level for a packed frame: the door cannot tell a tunnel from a
/// local browser, and 6 costs three times the CPU for 9 % fewer bytes.
const FRAME_LEVEL: u32 = 3;

/// `bytes` as a gzip member.
pub(crate) fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), Compression::new(GZIP_LEVEL));
    encoder.write_all(bytes)?;
    encoder.finish()
}

/// `bytes` as a brotli stream.
pub(crate) fn brotli(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = brotli::CompressorWriter::new(&mut out, 1 << 16, BROTLI_QUALITY, BROTLI_WINDOW);
    encoder.write_all(bytes)?;
    encoder.flush()?;
    // the stream is closed when the writer goes
    drop(encoder);
    Ok(out)
}

/// `text` as a raw deflate stream: what a packed frame carries.
pub(crate) fn deflate(text: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), Compression::new(FRAME_LEVEL));
    encoder.write_all(text)?;
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sample() -> Vec<u8> {
        "one two three two one ".repeat(4000).into_bytes()
    }

    #[test]
    fn every_coding_round_trips_and_shrinks() {
        let plain = sample();
        let mut back = Vec::new();
        let gz = gzip(&plain).expect("gzip");
        flate2::read::GzDecoder::new(gz.as_slice()).read_to_end(&mut back).expect("gunzip");
        assert_eq!(back, plain);
        assert!(gz.len() * 10 < plain.len());

        let br = brotli(&plain).expect("brotli");
        back.clear();
        ::brotli::Decompressor::new(br.as_slice(), 4096).read_to_end(&mut back).expect("unbrotli");
        assert_eq!(back, plain);
        assert!(br.len() * 10 < plain.len());

        let raw = deflate(&plain).expect("deflate");
        back.clear();
        flate2::read::DeflateDecoder::new(raw.as_slice()).read_to_end(&mut back).expect("inflate");
        assert_eq!(back, plain);
        assert!(raw.len() * 10 < plain.len());
    }

    #[test]
    fn nothing_encodes_to_something_decodable() {
        let mut back = Vec::new();
        ::brotli::Decompressor::new(brotli(b"").expect("brotli").as_slice(), 4096).read_to_end(&mut back).expect("empty");
        flate2::read::GzDecoder::new(gzip(b"").expect("gzip").as_slice()).read_to_end(&mut back).expect("empty");
        assert!(back.is_empty());
    }
}
