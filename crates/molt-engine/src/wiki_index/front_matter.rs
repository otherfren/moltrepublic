// SPDX-License-Identifier: GPL-3.0-or-later

//! The document header (`docs_archive/memory/knowledge_base_scale.md` §4.4): the
//! block's boundaries here, its YAML subset alongside.
//!
//! The FOLD never reads a header — a malformed one costs the document its
//! properties, never the patch that wrote it.

/// A header past this is not a header (§4.4).
pub(crate) const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Split a document into its front-matter block and its body. The block
/// exists only when the FIRST line is exactly `---` and a later line is
/// exactly `---` or `...`; anything else means the whole document is body.
pub fn split(doc: &str) -> (Option<&str>, &str) {
    match header_block(doc) {
        Ok(Some((header, body))) => (Some(header), body),
        _ => (None, doc),
    }
}

/// The block, told apart from the two ways there is none: `Ok(None)` = no
/// block at all (no opening line, or no closing fence), `Err` = a block
/// that IS there and only its size disqualified it. The caller can then
/// say which, instead of rendering 64 KiB of YAML as prose in silence.
fn header_block(doc: &str) -> Result<Option<(&str, &str)>, ()> {
    let Some(rest) = doc
        .strip_prefix("---\n")
        .or_else(|| doc.strip_prefix("---\r\n"))
    else {
        return Ok(None);
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let fence = line.trim_end_matches(['\n', '\r']);
        if fence == "---" || fence == "..." {
            if offset > MAX_HEADER_BYTES {
                return Err(());
            }
            return Ok(Some((&rest[..offset], &rest[offset + line.len()..])));
        }
        offset += line.len();
    }
    Ok(None)
}

/// The document's first ATX heading, header skipped. Display metadata.
pub fn first_heading(doc: &str) -> Option<String> {
    let (_, body) = split(doc);
    body.lines()
        .find(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .filter(|t| !t.is_empty())
}


// ---------------------------------------------------------------------------
// The YAML subset (§4.4)
// ---------------------------------------------------------------------------

use serde_json::Value;
use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::TScalarStyle;

/// The longest key the subset accepts.
const MAX_KEY: usize = 64;

/// A key of the subset: `[A-Za-z_][A-Za-z0-9_-]{0,63}`.
fn key_ok(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= MAX_KEY
        && k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A PLAIN scalar of at most 18 digits is an Integer; everything else is a
/// String. There is no `no` → false in this subset, deliberately: implicit
/// typing is the surprise the event parser exists to avoid.
fn scalar_value(text: &str, style: TScalarStyle) -> Value {
    if style == TScalarStyle::Plain {
        let body = text.strip_prefix('-').unwrap_or(text);
        if !body.is_empty()
            && body.len() <= 18
            && body.bytes().all(|b| b.is_ascii_digit())
        {
            if let Ok(n) = text.parse::<i64>() {
                return Value::from(n);
            }
        }
    }
    Value::String(text.to_string())
}

/// A string value that IS a link: `[[Name]]`, `[[Name|display]]` (the
/// display half stripped), or a path ending in `.md`.
pub fn link_target(s: &str) -> Option<&str> {
    if let Some(inner) = s.strip_prefix("[[").and_then(|r| r.strip_suffix("]]")) {
        let name = inner.split('|').next().unwrap_or(inner).trim();
        return (!name.is_empty()).then_some(name);
    }
    s.ends_with(".md").then_some(s)
}

type Events<'a> = Parser<std::str::Chars<'a>>;

fn next_event(p: &mut Events<'_>) -> Result<Event, String> {
    p.next_token().map(|(e, _)| e).map_err(|e| e.to_string())
}

/// The properties of ONE header. Every rejection names the rule it broke:
/// the author sees it on the proposal card, and the fold never reads a
/// header at all, so a malformed one costs properties, never the patch.
pub(crate) fn parse(header: &str) -> Result<serde_json::Map<String, Value>, String> {
    let mut p = Parser::new_from_str(header);
    let mut out = serde_json::Map::new();
    if !matches!(next_event(&mut p)?, Event::StreamStart) {
        return Err("not a YAML stream".to_string());
    }
    match next_event(&mut p)? {
        // an empty header is a header with no properties
        Event::StreamEnd => return Ok(out),
        Event::DocumentStart => {}
        _ => return Err("the header is one mapping".to_string()),
    }
    match next_event(&mut p)? {
        Event::MappingStart(0, None) => {}
        Event::MappingStart(..) => {
            return Err("anchors and tags are outside the subset".to_string())
        }
        _ => return Err("the header is one mapping".to_string()),
    }
    loop {
        let key = match next_event(&mut p)? {
            Event::MappingEnd => break,
            Event::Scalar(text, TScalarStyle::Plain, 0, None) => text,
            Event::Scalar(..) => {
                return Err("keys are plain scalars without anchor or tag".to_string())
            }
            _ => return Err("keys are plain scalars".to_string()),
        };
        if !key_ok(&key) {
            return Err(format!("`{key}` is not a key of the subset"));
        }
        if out.contains_key(&key) {
            return Err(format!("`{key}` appears twice"));
        }
        let value = parse_value(&mut p)?;
        out.insert(key, value);
    }
    if !matches!(next_event(&mut p)?, Event::DocumentEnd) {
        return Err("the header is one mapping".to_string());
    }
    match next_event(&mut p)? {
        Event::StreamEnd => Ok(out),
        _ => Err("a header holds ONE document".to_string()),
    }
}

/// One level of structure below a key, never deeper.
fn parse_value(p: &mut Events<'_>) -> Result<Value, String> {
    match next_event(p)? {
        Event::Scalar(text, style, 0, None) => Ok(scalar_value(&text, style)),
        Event::Scalar(..) => Err("anchors and tags are outside the subset".to_string()),
        Event::Alias(_) => Err("aliases are outside the subset".to_string()),
        Event::SequenceStart(0, None) => {
            let mut items = Vec::new();
            loop {
                match next_event(p)? {
                    Event::SequenceEnd => break,
                    Event::Scalar(text, style, 0, None) => items.push(scalar_value(&text, style)),
                    Event::MappingStart(0, None) => {
                        items.push(Value::Object(parse_flat_map(p)?));
                    }
                    Event::Alias(_) => {
                        return Err("aliases are outside the subset".to_string())
                    }
                    _ => {
                        return Err("a sequence holds scalars or flat mappings".to_string())
                    }
                }
            }
            Ok(Value::Array(items))
        }
        Event::MappingStart(0, None) => Ok(Value::Object(parse_flat_map(p)?)),
        Event::SequenceStart(..) | Event::MappingStart(..) => {
            Err("anchors and tags are outside the subset".to_string())
        }
        _ => Err("value outside the subset".to_string()),
    }
}

/// A mapping of scalars - the qualified relation's shape.
fn parse_flat_map(p: &mut Events<'_>) -> Result<serde_json::Map<String, Value>, String> {
    let mut m = serde_json::Map::new();
    loop {
        let key = match next_event(p)? {
            Event::MappingEnd => break,
            Event::Scalar(text, TScalarStyle::Plain, 0, None) => text,
            _ => return Err("keys are plain scalars".to_string()),
        };
        if !key_ok(&key) {
            return Err(format!("`{key}` is not a key of the subset"));
        }
        if m.contains_key(&key) {
            return Err(format!("`{key}` appears twice"));
        }
        match next_event(p)? {
            Event::Scalar(text, style, 0, None) => {
                m.insert(key, scalar_value(&text, style));
            }
            _ => return Err("one level of structure below a key, never deeper".to_string()),
        }
    }
    Ok(m)
}

/// A document's properties, and the reason there are none when a header
/// stands outside the subset.
pub fn properties(doc: &str) -> (Option<serde_json::Map<String, Value>>, Option<String>) {
    let header = match header_block(doc) {
        Ok(Some((header, _))) => header,
        Ok(None) => return (None, None),
        Err(()) => {
            return (
                None,
                Some(format!("the header is longer than {MAX_HEADER_BYTES} bytes")),
            )
        }
    };
    match parse(header) {
        Ok(map) => (Some(map), None),
        Err(e) => (None, Some(e)),
    }
}

/// The header's `type`, the conventional kind of a page.
pub(crate) fn kind_of(doc: &str) -> Option<String> {
    properties(doc)
        .0?
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
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
    fn an_oversized_header_is_no_header_and_says_so() {
        let doc = format!("---\n{}\n---\nbody\n", "k: v\n".repeat(MAX_HEADER_BYTES / 4));
        assert_eq!(split(&doc).0, None);
        // …and the author hears WHY, which a missing fence does not claim
        assert!(properties(&doc).1.is_some_and(|e| e.contains("longer than")));
        assert_eq!(properties("---\na: 1\n").1, None, "an unclosed block is no block");
    }

    /// The worked example of §4.4 parses to exactly the values it shows -
    /// strings stay strings, a plain run of digits is an Integer, and one
    /// level of structure below a key is allowed.
    #[test]
    fn the_subset_accepts_the_worked_example() {
        let doc = "---\ntype: person\naliases: [P. Müller, Müller]\ntags: [gruender, berlin]\nworks_at: \"[[Acme GmbH]]\"\nknows: [\"[[Anna Schmidt]]\", \"[[Bob Meier]]\"]\nborn: 1975\n---\n# P\n";
        let (props, err) = properties(doc);
        assert_eq!(err, None);
        let props = props.expect("the header parses");
        assert_eq!(props["type"], serde_json::json!("person"));
        assert_eq!(props["born"], serde_json::json!(1975));
        assert_eq!(props["aliases"], serde_json::json!(["P. Müller", "Müller"]));
        assert_eq!(props["works_at"], serde_json::json!("[[Acme GmbH]]"));
        assert_eq!(kind_of(doc).as_deref(), Some("person"));
    }

    /// A qualified relation: a mapping of scalars under the predicate.
    #[test]
    fn a_qualified_relation_is_a_flat_mapping() {
        let props = parse("works_at:\n  to: \"[[Acme GmbH]]\"\n  since: 2019\n  role: CTO\n")
            .expect("parses");
        assert_eq!(
            props["works_at"],
            serde_json::json!({ "to": "[[Acme GmbH]]", "since": 2019, "role": "CTO" })
        );
    }

    /// No implicit typing: the `no` → false class cannot happen, and a
    /// QUOTED run of digits stays a string.
    #[test]
    fn only_a_plain_digit_run_is_an_integer() {
        let props = parse("a: no\nb: yes\nc: null\nd: 1.5\ne: \"1975\"\nf: 1975\ng: -3\nh: 12345678901234567890\n")
            .expect("parses");
        for k in ["a", "b", "c", "d", "e", "h"] {
            assert!(props[k].is_string(), "{k} must stay a string: {:?}", props[k]);
        }
        assert_eq!(props["f"], serde_json::json!(1975));
        assert_eq!(props["g"], serde_json::json!(-3));
    }

    /// One rule per line: everything the subset refuses, refused.
    #[test]
    fn the_subset_refuses_what_it_says_it_refuses() {
        for (yaml, why) in [
            ("- one\n- two\n", "top level must be a mapping"),
            ("just a scalar\n", "top level must be a mapping"),
            ("a: &anc x\nb: *anc\n", "anchors and aliases"),
            ("a: !!int 7\n", "tags"),
            ("1bad: x\n", "a key may not start with a digit"),
            ("with space: x\n", "a key may not hold a space"),
            ("dup: 1\ndup: 2\n", "a key may not repeat"),
            ("a:\n  b:\n    c: 1\n", "never deeper than one level"),
            ("a: [[1, 2], [3]]\n", "no nested sequences"),
            ("a: [{b: {c: 1}}]\n", "no mapping inside a mapping"),
            ("a: 1\n---\nb: 2\n", "one document only"),
        ] {
            assert!(parse(yaml).is_err(), "{why}: {yaml:?} was accepted");
        }
        // …and a document whose header is outside the subset simply has no
        // properties, plus the reason
        let (props, err) = properties("---\n- one\n---\nbody\n");
        assert_eq!(props, None);
        assert!(err.is_some(), "the author is told why");
    }

    /// A key whose value is a link is a typed relation; the display half
    /// of a wiki link is stripped.
    #[test]
    fn a_link_valued_string_is_recognised_either_way() {
        assert_eq!(link_target("[[Anna Schmidt]]"), Some("Anna Schmidt"));
        assert_eq!(link_target("[[Anna Schmidt|Anna]]"), Some("Anna Schmidt"));
        assert_eq!(link_target("people/anna.md"), Some("people/anna.md"));
        assert_eq!(link_target("Anna Schmidt"), None);
        assert_eq!(link_target("[[]]"), None);
        assert_eq!(link_target("[[|x]]"), None);
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
