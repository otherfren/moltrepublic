// SPDX-License-Identifier: GPL-3.0-or-later

//! The document header (`docs/memory/knowledge_base_scale.md` §4.4): the
//! block's boundaries here, its YAML subset alongside.
//!
//! The FOLD never reads a header — a malformed one costs the document its
//! properties, never the patch that wrote it.

/// A header past this is not a header (§4.4).
pub(crate) const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Split a document into its front-matter block and its body. The block
/// exists only when the FIRST line is exactly `---` and a later line is
/// exactly `---` or `...`; anything else means the whole document is body.
pub(crate) fn split(doc: &str) -> (Option<&str>, &str) {
    let Some(rest) = doc
        .strip_prefix("---\n")
        .or_else(|| doc.strip_prefix("---\r\n"))
    else {
        return (None, doc);
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let fence = line.trim_end_matches(['\n', '\r']);
        if fence == "---" || fence == "..." {
            if offset > MAX_HEADER_BYTES {
                return (None, doc);
            }
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, doc)
}

/// The document's first ATX heading, header skipped. Display metadata.
pub(crate) fn first_heading(doc: &str) -> Option<String> {
    let (_, body) = split(doc);
    body.lines()
        .find(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_needs_the_first_line_and_a_closing_fence() {
        assert_eq!(split("---\na: 1\n---\nbody\n"), (Some("a: 1\n"), "body\n"));
        assert_eq!(split("---\na: 1\n...\nbody\n"), (Some("a: 1\n"), "body\n"));
        // no opening fence, an unclosed one, and a fence that is not first
        assert_eq!(split("body\n"), (None, "body\n"));
        assert_eq!(split("---\na: 1\n"), (None, "---\na: 1\n"));
        assert_eq!(split("x\n---\na: 1\n---\n"), (None, "x\n---\na: 1\n---\n"));
        assert_eq!(split("----\na: 1\n---\n"), (None, "----\na: 1\n---\n"));
        // an empty header is still a header
        assert_eq!(split("---\n---\nbody"), (Some(""), "body"));
    }

    #[test]
    fn an_oversized_header_is_no_header() {
        let doc = format!("---\n{}\n---\nbody\n", "k: v\n".repeat(MAX_HEADER_BYTES / 4));
        assert_eq!(split(&doc).0, None);
    }

    #[test]
    fn the_title_is_the_first_heading_below_the_header() {
        assert_eq!(
            first_heading("---\ntype: person\n---\n# Anna\n\ntext\n"),
            Some("Anna".to_string())
        );
        assert_eq!(first_heading("text only\n"), None);
        assert_eq!(first_heading("## Deep\n"), Some("Deep".to_string()));
        assert_eq!(first_heading("#\n"), None);
    }
}
