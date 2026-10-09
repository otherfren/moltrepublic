// SPDX-License-Identifier: GPL-3.0-or-later
//! The wiki's references to the republic's PERSISTED files
//! (`docs_archive/ui/wiki_files_and_images.md` §3.1): `![alt](upload:<hex>)`
//! renders inline, `[text](upload:<hex>)` as a link. A file is named by
//! its content - the sha256 in hex, full or a prefix of at least
//! [`PREFIX_MIN`] digits - never by a path or a message id. The link
//! graph ignores the scheme by construction (no `.md`). The same walk
//! reads `[text](quest:<hex>)`, a link to a kanban task by its id or a
//! prefix of at least [`QUEST_PREFIX_MIN`] digits
//! (`docs_archive/kanban/kanban_workflows.md` §9 S5).

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// The destination scheme.
pub const SCHEME: &str = "upload:";
/// The shortest prefix that names a file (resolved like a git short hash).
pub const PREFIX_MIN: usize = 12;
/// A full sha256 in hex.
pub const HEX_FULL: usize = 64;

/// One `upload:` reference in a page, in document order. Every occurrence
/// is its own entry (the pane renders each); dedupe by `hex` for a lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// The hex after the scheme, trimmed and lowercased - exactly as
    /// written otherwise, so a malformed one is still reported
    /// ([`FileRef::valid`] tells).
    pub hex: String,
    /// The image's alt text, or the link's text.
    pub alt: String,
    /// `![…]` (renders inline) rather than `[…]` (a link).
    pub image: bool,
    /// The byte range of the whole markup in the source.
    pub span: std::ops::Range<usize>,
}

impl FileRef {
    /// The hex has the form a resolver accepts.
    #[must_use]
    pub fn valid(&self) -> bool {
        valid_hex(&self.hex)
    }
}

/// Whether `hex` is a lowercase hex prefix of [`PREFIX_MIN`]..=[`HEX_FULL`]
/// digits - the ONE form check the parser, the engine and the picker
/// share.
#[must_use]
pub fn valid_hex(hex: &str) -> bool {
    (PREFIX_MIN..=HEX_FULL).contains(&hex.len())
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The hex an `upload:` destination names, normalized; `None` for any
/// other destination.
#[must_use]
pub fn checksum_of(dest: &str) -> Option<String> {
    let rest = dest.trim().strip_prefix(SCHEME)?;
    Some(rest.trim().to_ascii_lowercase())
}

/// The task link scheme.
pub const QUEST_SCHEME: &str = "quest:";
/// The shortest prefix that names a task: its display form `#` + 8 hex.
pub const QUEST_PREFIX_MIN: usize = 8;
/// A full task id in hex.
pub const QUEST_HEX_FULL: usize = 32;

/// Whether `hex` is a lowercase hex task id or prefix of
/// [`QUEST_PREFIX_MIN`]..=[`QUEST_HEX_FULL`] digits.
#[must_use]
pub fn valid_quest_hex(hex: &str) -> bool {
    (QUEST_PREFIX_MIN..=QUEST_HEX_FULL).contains(&hex.len())
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The hex a `quest:` destination names, normalized; `None` for any other
/// destination.
#[must_use]
pub fn quest_id_of(dest: &str) -> Option<String> {
    let rest = dest.trim().strip_prefix(QUEST_SCHEME)?;
    Some(rest.trim().to_ascii_lowercase())
}

/// One `quest:` link in a page, in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestRef {
    /// The hex after the scheme, trimmed and lowercased.
    pub hex: String,
    /// The link's text.
    pub text: String,
    /// The byte range of the whole markup in the source.
    pub span: std::ops::Range<usize>,
}

impl QuestRef {
    /// The hex has the form a task id or its prefix takes.
    #[must_use]
    pub fn valid(&self) -> bool {
        valid_quest_hex(&self.hex)
    }
}

/// Every `quest:` link of `markdown`, in order. A picture names no task.
#[must_use]
pub fn quest_refs(markdown: &str) -> Vec<QuestRef> {
    refs(markdown, quest_id_of, false)
        .into_iter()
        .map(|r| QuestRef { hex: r.hex, text: r.alt, span: r.span })
        .collect()
}

/// Every `upload:` reference of `markdown`, in order. Code spans and
/// fences never yield one - pulldown-cmark emits no link or image inside
/// them, so an example stays an example.
#[must_use]
pub fn file_refs(markdown: &str) -> Vec<FileRef> {
    refs(markdown, checksum_of, true)
}

/// THE walk: the links (and with `images` the pictures) whose destination
/// `pick` accepts.
fn refs(markdown: &str, pick: fn(&str) -> Option<String>, images: bool) -> Vec<FileRef> {
    let mut out = Vec::new();
    // the reference being collected: (hex, image, span) + its text so far
    let mut open: Option<(String, bool, std::ops::Range<usize>, String)> = None;
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) if open.is_none() => {
                if let Some(hex) = pick(&dest_url).filter(|_| images) {
                    open = Some((hex, true, range, String::new()));
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) if open.is_none() => {
                if let Some(hex) = pick(&dest_url) {
                    open = Some((hex, false, range, String::new()));
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(o) = open.as_mut() {
                    o.3.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(o) = open.as_mut() {
                    o.3.push(' ');
                }
            }
            Event::End(TagEnd::Image) | Event::End(TagEnd::Link) => {
                if let Some((hex, image, span, alt)) = open.take() {
                    out.push(FileRef {
                        hex,
                        alt: alt.trim().to_string(),
                        image,
                        span,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "3f9a2c1b7e04d5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0";

    #[test]
    fn an_image_and_a_link_are_told_apart_and_carry_their_text() {
        let md = format!("Intro\n\n![Netzdiagramm](upload:3f9a2c1b7e04)\n\nSiehe [Bericht Q3 (PDF)](upload:{FULL}).\n");
        let refs = file_refs(&md);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].hex, "3f9a2c1b7e04");
        assert_eq!(refs[0].alt, "Netzdiagramm");
        assert!(refs[0].image);
        assert!(refs[0].valid());
        assert_eq!(&md[refs[0].span.clone()], "![Netzdiagramm](upload:3f9a2c1b7e04)");
        assert_eq!(refs[1].hex, FULL);
        assert_eq!(refs[1].alt, "Bericht Q3 (PDF)");
        assert!(!refs[1].image);
        assert!(refs[1].valid());
        assert_eq!(&md[refs[1].span.clone()], &format!("[Bericht Q3 (PDF)](upload:{FULL})"));
    }

    #[test]
    fn the_hex_is_normalized_and_its_form_is_judged_not_dropped() {
        let refs = file_refs("![a](upload:3F9A2C1B7E04) [b](upload:abc) [c](upload: 3f9a2c1b7e04 )");
        // a destination with spaces is no markdown link at all - markdown
        // decides the shape, the grammar only the hex
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].hex, "3f9a2c1b7e04");
        assert!(refs[0].valid(), "uppercase folds to the canonical form");
        assert_eq!(refs[1].hex, "abc");
        assert!(!refs[1].valid(), "too short - reported, so the pane can say so");
        assert!(!valid_hex("3f9a2c1b7e0g"), "not hex");
        assert!(!valid_hex(&format!("{FULL}0")), "longer than a sha256");
        assert!(valid_hex(FULL));
    }

    #[test]
    fn code_spans_and_fences_yield_no_reference() {
        let md = "Use `![x](upload:3f9a2c1b7e04)` like so:\n\n```\n[y](upload:3f9a2c1b7e04)\n```\n\n    [z](upload:3f9a2c1b7e04)\n\nreal: [w](upload:3f9a2c1b7e04)\n";
        let refs = file_refs(md);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].alt, "w");
    }

    #[test]
    fn other_destinations_are_not_references() {
        let md = "[page](other.md) [[Name]] ![pic](https://x.test/a.png) [u](uploads:3f9a2c1b7e04) [v](Upload:3f9a2c1b7e04)";
        assert!(file_refs(md).is_empty(), "the scheme is exact and lowercase");
    }

    #[test]
    fn every_occurrence_is_its_own_entry_in_document_order() {
        let md = "[a](upload:3f9a2c1b7e04)\n\n![b](upload:3f9a2c1b7e04)\n\n- [c](upload:aaaaaaaaaaaa)\n";
        let hexes: Vec<String> = file_refs(md).into_iter().map(|r| r.hex).collect();
        assert_eq!(hexes, ["3f9a2c1b7e04", "3f9a2c1b7e04", "aaaaaaaaaaaa"]);
    }

    #[test]
    fn an_image_inside_a_link_keeps_the_outer_reference_only() {
        // a linked image: the outer link is the reference collected; the
        // inner image's text rides along as the alt
        let refs = file_refs("[![alt](upload:aaaaaaaaaaaa)](upload:bbbbbbbbbbbb)");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].hex, "bbbbbbbbbbbb");
        assert!(!refs[0].image);
        assert_eq!(refs[0].alt, "alt");
    }

    #[test]
    fn a_quest_link_names_a_task_by_id_or_prefix() {
        let md = "See [the drill](quest:0B6D42F7) and [all](quest:0b6d42f70000000000000000000000aa).\n\n[f](upload:3f9a2c1b7e04) [p](a.md)";
        let refs = quest_refs(md);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].hex, "0b6d42f7");
        assert_eq!(refs[0].text, "the drill");
        assert!(refs[0].valid());
        assert_eq!(&md[refs[0].span.clone()], "[the drill](quest:0B6D42F7)");
        assert_eq!(refs[1].hex, "0b6d42f70000000000000000000000aa");
        assert!(refs[1].valid());
        assert_eq!(file_refs(md).len(), 1, "a quest link is no file reference");
    }

    #[test]
    fn a_quest_reference_is_a_link_outside_code_with_a_hex_of_8_to_32() {
        let md = "`[a](quest:0b6d42f7)`\n\n```\n[b](quest:0b6d42f7)\n```\n\n![c](quest:0b6d42f7) [d](quest:0b6d42f) [e](quest:0b6d42f7x)";
        let refs = quest_refs(md);
        let hexes: Vec<(&str, bool)> = refs.iter().map(|r| (r.hex.as_str(), r.valid())).collect();
        assert_eq!(hexes, [("0b6d42f", false), ("0b6d42f7x", false)], "code and pictures name no task; a bad hex is reported");
        assert!(valid_quest_hex(&"a".repeat(32)));
        assert!(!valid_quest_hex(&"a".repeat(33)));
        assert_eq!(quest_id_of(" quest:ABCDEF01 "), Some("abcdef01".to_string()));
        assert_eq!(quest_id_of("quests:abcdef01"), None);
        assert_eq!(quest_id_of("upload:abcdef012345"), None);
    }

    #[test]
    fn checksum_of_names_only_the_scheme() {
        assert_eq!(checksum_of("upload:ABCDEF012345"), Some("abcdef012345".to_string()));
        assert_eq!(checksum_of("other.md"), None);
        assert_eq!(checksum_of("upload:"), Some(String::new()));
    }
}
