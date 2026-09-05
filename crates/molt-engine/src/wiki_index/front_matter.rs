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
pub fn key_ok(k: &str) -> bool {
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


// ---------------------------------------------------------------------------
// The header EMITTER (§4.4)
//
// ONE emitter for both writers: the GUI's relation modal and the engine's
// `wiki_edit` write the same header shape, and the PARSER above is the
// arbiter for both - what it does not read back is never written.
// ---------------------------------------------------------------------------

/// A YAML double-quoted scalar - the one form no member input can break
/// out of.
pub fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The byte range of the header BODY inside `raw`. The ENGINE decides
/// whether there is a header at all (size rule included); this only
/// locates the block it named, and gives up when the two disagree.
pub fn header_body_span(raw: &str) -> Option<(usize, usize)> {
    let header = split(raw).0?;
    let rest = raw.strip_prefix("---\n").or_else(|| raw.strip_prefix("---\r\n"))?;
    let start = raw.len() - rest.len();
    let mut off = start;
    for line in rest.split_inclusive('\n') {
        let fence = line.trim_end_matches(['\n', '\r']);
        if fence == "---" || fence == "..." {
            return (raw.get(start..off) == Some(header)).then_some((start, off));
        }
        off += line.len();
    }
    None
}

/// A header value as the list it stands for: a scalar is a list of one,
/// an absent key the empty list.
pub fn value_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        None => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
    }
}

/// The header re-emitted from what the parser says it holds, with
/// `display` added under `key`. The fallback, never the first choice: it
/// loses comments and formatting, which is why the line-wise edit runs
/// first.
pub fn canonical_header(
    before: &serde_json::Map<String, Value>,
    key: &str,
    display: &str,
) -> String {
    let mut out = String::new();
    for (k, v) in before {
        if k == key {
            let mut items = value_list(Some(v));
            items.push(Value::String(display.to_string()));
            out.push_str(&emit_key(k, &items));
        } else {
            out.push_str(&emit_key(k, &value_list(Some(v))));
        }
    }
    if !before.contains_key(key) {
        out.push_str(&emit_key(key, &[Value::String(display.to_string())]));
    }
    out
}

/// One `key: value` (or block list) of the canonical emitter.
pub fn emit_key(key: &str, items: &[Value]) -> String {
    match items {
        [] => format!("{key}: {}\n", yaml_quote("")),
        [one] => format!("{key}: {}\n", emit_scalar(one)),
        many => {
            let mut out = format!("{key}:\n");
            for item in many {
                out.push_str(&format!("  - {}\n", emit_scalar(item)));
            }
            out
        }
    }
}

/// One value of the canonical emitter. A flat mapping is the qualified
/// relation's shape and stays one, in flow form.
fn emit_scalar(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => yaml_quote(s),
        Value::Object(map) => {
            let inner = map
                .iter()
                .map(|(k, v)| format!("{k}: {}", emit_scalar(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        other => yaml_quote(&other.to_string()),
    }
}

/// The header body with `literal` added under `key`, edited line-wise.
/// Best-effort: [`with_relation`] verifies the result and falls back to
/// the canonical emitter when this misreads the shape.
pub fn header_lines(header: &str, key: &str, literal: &str) -> String {
    let prefix = format!("{key}:");
    let lines: Vec<&str> = header.split_inclusive('\n').collect();
    let Some(at) = lines.iter().position(|l| l.starts_with(&prefix)) else {
        let mut out = header.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("{key}: {literal}\n"));
        return out;
    };
    let rest = lines[at]
        .get(prefix.len()..)
        .unwrap_or_default()
        .trim_end_matches(['\n', '\r'])
        .trim();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_string()).collect();
    if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        let inner = inner.trim();
        out[at] = if inner.is_empty() {
            format!("{key}: [{literal}]\n")
        } else {
            format!("{key}: [{inner}, {literal}]\n")
        };
    } else if rest.is_empty() {
        // `key:` alone - a block sequence may follow, at ANY indent
        let mut last = None;
        let mut indent = "  ".to_string();
        for (i, l) in lines.iter().enumerate().skip(at + 1) {
            let t = l.trim_end_matches(['\n', '\r']);
            let body = t.trim_start();
            if body.starts_with("- ") {
                if last.is_none() {
                    indent = t.get(..t.len() - body.len()).unwrap_or("").to_string();
                }
                last = Some(i);
            } else if body.is_empty() {
                continue;
            } else {
                break;
            }
        }
        match last {
            Some(i) => out.insert(i + 1, format!("{indent}- {literal}\n")),
            None => out[at] = format!("{key}: {literal}\n"),
        }
    } else {
        out[at] = format!("{key}:\n  - {rest}\n  - {literal}\n");
    }
    out.concat()
}

/// One `key: value` for an arbitrary value of the subset. An ARRAY stays
/// an array even at length one - [`emit_key`] deliberately flattens that,
/// because its caller is growing a relation list rather than restating a
/// value.
pub fn emit_value(key: &str, value: &Value) -> String {
    match value {
        Value::Array(items) if items.is_empty() => format!("{key}: []\n"),
        Value::Array(items) => {
            let mut out = format!("{key}:\n");
            for item in items {
                out.push_str(&format!("  - {}\n", emit_scalar(item)));
            }
            out
        }
        other => format!("{key}: {}\n", emit_scalar(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emitter's contract with the parser above: what it writes, the
    /// parser reads back UNCHANGED - every shape of the subset, a
    /// one-element list included. The structured write path verifies each
    /// header this way before it proposes one, so a drift here is a
    /// refused edit rather than a broken document.
    #[test]
    fn the_emitter_round_trips_every_shape_of_the_subset() {
        let props: serde_json::Map<String, Value> = serde_json::from_str(
            r#"{
                "type": "person",
                "born": 1975,
                "negative": -3,
                "tags": ["berlin"],
                "aliases": ["P. Müller", "Müller", "a: b"],
                "works_at": {"to": "[[Acme GmbH]]", "since": 2019, "role": "CTO"},
                "seen": [{"at": "berlin", "year": 2020}],
                "note": "yes: no # [[x]]",
                "nothing": []
            }"#,
        )
        .expect("the fixture is JSON");
        let mut header = String::new();
        for (k, v) in &props {
            header.push_str(&emit_value(k, v));
        }
        let doc = format!("---\n{header}---\n# x\n");
        assert_eq!(properties(&doc).0.as_ref(), Some(&props), "emitted:\n{header}");
    }


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
