// SPDX-License-Identifier: GPL-3.0-or-later

//! The wiki's body links (`docs_archive/memory/knowledge_base_scale.md` §4.5):
//! markdown destinations ending in `.md` plus the readable `[[Name]]` /
//! `[[pred::Name]]` form, code masked out.
//!
//! Pure, and in core so the engine's link graph and every GUI build (the
//! browser one included, `docs/web_gui.md` §4) read ONE parser.

use std::ops::Range;

use pulldown_cmark::{Event, Parser, Tag};

use crate::wiki_front_matter;

/// One link a body wrote, before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyLink {
    /// The target exactly as written: a `.md` destination, or the name
    /// inside a `[[…]]`.
    pub target: String,
    /// The `pred::` the link declared inline; `None` for a plain link.
    pub predicate: Option<String>,
}

/// The parts of a `[[…]]`.
pub struct LinkParts<'a> {
    /// The inline predicate, if the left half is a valid header key.
    pub predicate: Option<&'a str>,
    /// The link target.
    pub name: &'a str,
    /// The display half, if it carries text.
    pub display: Option<&'a str>,
}

/// Split the inside of a `[[…]]` — the ONE rule the index and the GUI
/// share. `|display` comes off first, then the FIRST `::`: its left half
/// is the predicate only if it is a valid header key (§4.4), otherwise
/// the whole left part is the name and every `::` in it stays there.
///
/// The consequence, deliberately: `[[std::vector]]` in prose IS a typed
/// link (`std` → `vector`). In a code span it is masked and therefore no
/// link at all, which is where an example belongs.
pub fn link_parts(inner: &str) -> LinkParts<'_> {
    // `\|` is how a link with display text survives a table cell (a bare
    // `|` would split the cell) - the escape IS the separator, never part
    // of the target (F14: 49 of 107 dangling names ended in a backslash)
    let split = inner
        .split_once("\\|")
        .or_else(|| inner.split_once('|'));
    let (left, display) = match split {
        Some((l, d)) => (l.trim(), Some(d.trim()).filter(|d| !d.is_empty())),
        None => (inner.trim(), None),
    };
    match left.split_once("::") {
        Some((pred, name)) if wiki_front_matter::key_ok(pred.trim()) && !name.trim().is_empty() => {
            LinkParts {
                predicate: Some(pred.trim()),
                name: name.trim(),
                display,
            }
        }
        _ => LinkParts {
            predicate: None,
            name: left,
            display,
        },
    }
}

/// The body's links: markdown destinations ending in `.md`, plus the
/// readable `[[Name]]` / `[[pred::Name]]` form. Code spans and code
/// blocks are masked out — a link in a fenced example is not a claim
/// about the graph. Deduped by the whole link, so two predicates onto one
/// target stay two claims.
pub fn body_links(markdown: &str) -> Vec<BodyLink> {
    let (dests, code) = scan(markdown);
    let mut out: Vec<BodyLink> = Vec::new();
    for (_, target) in dests {
        let link = BodyLink {
            target,
            predicate: None,
        };
        // a URL that happens to end in `.md` (a NIP on GitHub) is a
        // source, not a wiki path (G15, round 2)
        if link.target.ends_with(".md") && !link.target.contains("://") && !out.contains(&link) {
            out.push(link);
        }
    }
    for span in bracket_spans(markdown, &code) {
        let parts = link_parts(&markdown[span]);
        let link = BodyLink {
            target: parts.name.to_string(),
            predicate: parts.predicate.map(str::to_string),
        };
        if !link.target.is_empty() && !link.target.contains('\n') && !out.contains(&link) {
            out.push(link);
        }
    }
    out
}

/// A markdown link's destination with the span of its whole `[text](dest)`.
type Dest = (Range<usize>, String);

/// ONE parse: every markdown link destination with its span, and the code
/// spans a `[[…]]` inside must be masked by.
fn scan(markdown: &str) -> (Vec<Dest>, Vec<(usize, usize)>) {
    let mut dests: Vec<Dest> = Vec::new();
    let mut code: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0u32;
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                dests.push((range, dest_url.to_string()));
            }
            Event::Start(Tag::CodeBlock(_)) => depth += 1,
            Event::End(pulldown_cmark::TagEnd::CodeBlock) => depth = depth.saturating_sub(1),
            Event::Code(_) => code.push((range.start, range.end)),
            Event::Text(_) if depth > 0 => code.push((range.start, range.end)),
            _ => {}
        }
    }
    (dests, code)
}

/// The INSIDE of every `[[…]]` the body writes, code masked out.
fn bracket_spans(markdown: &str, code: &[(usize, usize)]) -> Vec<Range<usize>> {
    let masked = |at: usize| code.iter().any(|(s, e)| at >= *s && at < *e);
    let bytes = markdown.as_bytes();
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' && !masked(i) {
            if let Some(end) = markdown[i + 2..].find("]]") {
                out.push(i + 2..i + 2 + end);
                i += end + 4;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// **E2**: every link that names the path `from`, rewritten to `to` -
/// markdown destinations and the `[[path]]` form. A `[[Name]]` binds by
/// title or alias and names no path, so it is left alone. `None` = the
/// document carries none.
pub fn rewrite_link_target(markdown: &str, from: &str, to: &str) -> Option<String> {
    if !markdown.contains(from) {
        return None;
    }
    let (dests, code) = scan(markdown);
    let mut spans: Vec<Range<usize>> = Vec::new();
    for (span, dest) in dests {
        if dest != from {
            continue;
        }
        // the destination sits AFTER the link text, so the last hit in
        // the span is it (a reference link carries none and is skipped)
        if let Some(at) = markdown[span.clone()].rfind(from) {
            spans.push(span.start + at..span.start + at + from.len());
        }
    }
    for span in bracket_spans(markdown, &code) {
        let inner = &markdown[span.clone()];
        if link_parts(inner).name != from {
            continue;
        }
        // the predicate half carries no slash, so the first hit is the name
        if let Some(at) = inner.find(from) {
            spans.push(span.start + at..span.start + at + from.len());
        }
    }
    if spans.is_empty() {
        return None;
    }
    spans.sort_by_key(|s| s.start);
    spans.dedup();
    let mut out = String::with_capacity(markdown.len());
    let mut at = 0usize;
    for span in spans {
        if span.start < at {
            continue; // overlapping hits: the first one wins
        }
        out.push_str(&markdown[at..span.start]);
        out.push_str(to);
        at = span.end;
    }
    out.push_str(&markdown[at..]);
    Some(out)
}

/// Just the targets, deduped in order of first appearance: what a
/// navigator needs, which is where a link GOES and not what it asserts.
pub fn body_link_targets(markdown: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for link in body_links(markdown) {
        if !out.contains(&link.target) {
            out.push(link.target);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `|display` comes off first, then the first `::` with a valid key.
    #[test]
    fn a_link_splits_into_predicate_name_and_display() {
        let p = link_parts("made_by::a.md\\|A");
        assert_eq!((p.predicate, p.name, p.display), (Some("made_by"), "a.md", Some("A")));
        let plain = link_parts("a.md|A");
        assert_eq!((plain.predicate, plain.name, plain.display), (None, "a.md", Some("A")));
        let bare = link_parts(" One ");
        assert_eq!((bare.predicate, bare.name, bare.display), (None, "One", None));
    }

    /// Code is masked, a URL is a source, and two predicates stay two claims.
    #[test]
    fn body_links_read_prose_only() {
        let doc = "see [a](a.md), [n](https://x.example/n.md) and [[is::One]] [[One]]\n\n```\n[[Two]]\n```\n\n`[[Three]]`\n";
        assert_eq!(
            body_links(doc),
            vec![
                BodyLink { target: "a.md".into(), predicate: None },
                BodyLink { target: "One".into(), predicate: Some("is".into()) },
                BodyLink { target: "One".into(), predicate: None },
            ]
        );
        assert_eq!(body_link_targets(doc), vec!["a.md".to_string(), "One".to_string()]);
    }

    /// A rename rewrites the links that name the path, and nothing else.
    #[test]
    fn a_rename_rewrites_only_the_named_path() {
        let doc = "[a](a.md) [[is::a.md|A]] [[a]]\n";
        assert_eq!(rewrite_link_target(doc, "a.md", "b.md").as_deref(), Some("[a](b.md) [[is::b.md|A]] [[a]]\n"));
        assert_eq!(rewrite_link_target("none\n", "a.md", "b.md"), None);
    }
}
