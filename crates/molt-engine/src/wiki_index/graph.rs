// SPDX-License-Identifier: GPL-3.0-or-later

//! The link graph (`docs_archive/memory/knowledge_base_scale.md` §4.5): who points
//! at whom, and under which predicate.
//!
//! A pure function of the folded tree, so every node computes the same
//! graph. The incremental update RE-PARSES only the touched documents and
//! then runs the SAME resolution pass a full build runs — which is what
//! makes an updated graph identical to a fresh one by construction rather
//! than by careful edge surgery.

use std::collections::{BTreeMap, BTreeSet};

use pulldown_cmark::{Event, Parser, Tag};
use serde_json::Value;

use super::front_matter;

/// One resolved edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edge {
    /// The other end: the target for an out-edge, the source for an in-edge.
    pub(crate) to: String,
    /// The predicate: the header key that carried it, or the `pred::` an
    /// inline body link declared. `None` for a plain link.
    pub(crate) predicate: Option<String>,
    /// Did it come from the header? An inline predicate does NOT set this -
    /// the flag answers "from the front matter?", not "typed?".
    pub(crate) header: bool,
}

/// An edge before resolution: the target exactly as the document wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawEdge {
    target: String,
    predicate: Option<String>,
    header: bool,
}

/// What a listing needs about one document, without its content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DocMeta {
    /// The first heading.
    pub(crate) title: Option<String>,
    /// The header's `type`.
    pub(crate) kind: Option<String>,
    /// The header's `aliases`.
    pub(crate) aliases: Vec<String>,
}

/// The whole graph.
#[derive(Debug, Default)]
pub(crate) struct WikiGraph {
    /// Per document: its metadata.
    pub(crate) docs: BTreeMap<String, DocMeta>,
    /// Per document: its edges as written (the parse result).
    raw: BTreeMap<String, Vec<RawEdge>>,
    /// Per document: its header's scalar `(key, value)` pairs, kept so the
    /// inventory can be rebuilt without re-parsing the tree.
    props: BTreeMap<String, Vec<(String, String)>>,
    /// The republic's ONTOLOGY as it actually is: every header key in use,
    /// each with its values and how often each occurs. Derived, so it
    /// cannot rot - and a VIEW, never a registry: nothing here governs
    /// what a document may say (decision 4, the ontology is content).
    pub(crate) inventory: BTreeMap<String, BTreeMap<String, u32>>,
    /// Resolved out-edges.
    pub(crate) out: BTreeMap<String, Vec<Edge>>,
    /// Resolved in-edges, keyed by the TARGET.
    pub(crate) inn: BTreeMap<String, Vec<Edge>>,
    /// Edges naming a document that does not exist, keyed by that name.
    pub(crate) dangling: BTreeMap<String, Vec<(String, Edge)>>,
}

impl WikiGraph {
    /// Parse every document, then resolve.
    pub(crate) fn build(tree: &BTreeMap<String, String>) -> Self {
        let mut g = WikiGraph::default();
        for (path, content) in tree {
            g.reparse(path, Some(content));
        }
        g.resolve();
        g
    }

    /// Re-parse the touched documents (a missing one is a deletion), then
    /// resolve everything again.
    pub(crate) fn update(&mut self, tree: &BTreeMap<String, String>, touched: &BTreeSet<String>) {
        for path in touched {
            self.reparse(path, tree.get(path));
        }
        self.resolve();
    }

    /// One document's metadata and raw edges; `None` deletes it.
    fn reparse(&mut self, path: &str, content: Option<&String>) {
        let Some(content) = content else {
            self.docs.remove(path);
            self.raw.remove(path);
            self.props.remove(path);
            return;
        };
        let (props, _) = front_matter::properties(content);
        let props = props.unwrap_or_default();
        let aliases = string_list(props.get("aliases"));
        self.docs.insert(
            path.to_string(),
            DocMeta {
                title: front_matter::first_heading(content),
                kind: props
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                aliases,
            },
        );
        let mut edges: Vec<RawEdge> = Vec::new();
        // the header's typed relations: the KEY is the predicate
        for (key, value) in &props {
            collect_typed(key, value, &mut edges);
        }
        // …and the body's links, each with the predicate it declared
        let (_, body) = front_matter::split(content);
        let inline = body_links(body);
        for link in &inline {
            edges.push(RawEdge {
                target: link.target.clone(),
                predicate: link.predicate.clone(),
                header: false,
            });
        }
        self.raw.insert(path.to_string(), edges);
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (key, value) in &props {
            for shown in scalar_strings(value) {
                pairs.push((key.clone(), shown));
            }
        }
        // An inline predicate is the same claim as the header key, so it
        // lands in the SAME bucket rather than beside it: `wiki_props` is
        // the one call that answers "what relations does this republic
        // use", and two buckets per predicate would split that answer.
        // Once per document - two spellings of one claim are one claim.
        for link in inline {
            let Some(pred) = link.predicate else { continue };
            let pair = (pred, link.target);
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
        self.props.insert(path.to_string(), pairs);
    }

    /// Resolve every raw edge against the current document set: exact path,
    /// then unique basename, then unique alias, else dangling.
    fn resolve(&mut self) {
        let mut by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for path in self.docs.keys() {
            let base = path.rsplit('/').next().unwrap_or(path);
            by_name.entry(base).or_default().push(path);
            if let Some(stem) = base.strip_suffix(".md") {
                by_name.entry(stem).or_default().push(path);
            }
        }
        let mut by_alias: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (path, meta) in &self.docs {
            for alias in &meta.aliases {
                by_alias.entry(alias.as_str()).or_default().push(path);
            }
        }
        let unique = |index: &BTreeMap<&str, Vec<&str>>, key: &str| -> Option<String> {
            match index.get(key) {
                Some(hits) if hits.len() == 1 => hits.first().map(|p| (*p).to_string()),
                _ => None,
            }
        };

        let mut out: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
        let mut inn: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
        let mut dangling: BTreeMap<String, Vec<(String, Edge)>> = BTreeMap::new();
        for (src, edges) in &self.raw {
            for raw in edges {
                let hit = if self.docs.contains_key(&raw.target) {
                    Some(raw.target.clone())
                } else {
                    unique(&by_name, &raw.target).or_else(|| unique(&by_alias, &raw.target))
                };
                let edge = |to: String| Edge {
                    to,
                    predicate: raw.predicate.clone(),
                    header: raw.header,
                };
                match hit {
                    Some(target) => {
                        out.entry(src.clone()).or_default().push(edge(target.clone()));
                        inn.entry(target).or_default().push(edge(src.clone()));
                    }
                    None => dangling
                        .entry(raw.target.clone())
                        .or_default()
                        .push((src.clone(), edge(raw.target.clone()))),
                }
            }
        }
        self.out = out;
        self.inn = inn;
        self.dangling = dangling;
        let mut inventory: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
        for pairs in self.props.values() {
            for (key, value) in pairs {
                *inventory
                    .entry(key.clone())
                    .or_default()
                    .entry(value.clone())
                    .or_default() += 1;
            }
        }
        self.inventory = inventory;
    }

    /// Documents within `depth` hops of `path`, nearest first, capped.
    pub(crate) fn neighbors(&self, path: &str, depth: u32, cap: usize) -> Vec<(String, u32)> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        seen.insert(path);
        let mut frontier: Vec<String> = vec![path.to_string()];
        let mut found: Vec<(String, u32)> = Vec::new();
        for hop in 1..=depth {
            let mut next: Vec<String> = Vec::new();
            for node in &frontier {
                let both = self
                    .out
                    .get(node)
                    .into_iter()
                    .flatten()
                    .chain(self.inn.get(node).into_iter().flatten());
                for e in both {
                    if seen.contains(e.to.as_str()) {
                        continue;
                    }
                    // `seen` borrows self.docs' keys, so insert the OWNED
                    // key the graph already holds
                    let Some((owned, _)) = self.docs.get_key_value(&e.to) else {
                        continue;
                    };
                    seen.insert(owned.as_str());
                    found.push((owned.clone(), hop));
                    next.push(owned.clone());
                    if found.len() >= cap {
                        return found;
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        found
    }
}

/// Every scalar a header value carries, as the human reads it - one for a
/// scalar, N for a list, the members of a qualified relation.
fn scalar_strings(value: &Value) -> Vec<String> {
    let one = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(
                front_matter::link_target(s)
                    .unwrap_or(s)
                    .to_string(),
            ),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    };
    match value {
        Value::Array(items) => items
            .iter()
            .flat_map(|i| match i {
                Value::Object(map) => map.values().filter_map(one).collect::<Vec<_>>(),
                other => one(other).into_iter().collect(),
            })
            .collect(),
        Value::Object(map) => map.values().filter_map(one).collect(),
        other => one(other).into_iter().collect(),
    }
}

/// A header value's strings, one or many.
fn string_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Every link a header value carries, under `key` as its predicate. A
/// mapping's link-valued members are edges too, so `works_at: { to: … }`
/// binds under `works_at`, not under `to`.
fn collect_typed(key: &str, value: &Value, out: &mut Vec<RawEdge>) {
    let mut push = |s: &str| {
        if let Some(target) = front_matter::link_target(s) {
            out.push(RawEdge {
                target: target.to_string(),
                predicate: Some(key.to_string()),
                header: true,
            });
        }
    };
    match value {
        Value::String(s) => push(s),
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(s) => push(s),
                    Value::Object(map) => {
                        for v in map.values() {
                            if let Value::String(s) = v {
                                push(s);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                if let Value::String(s) = v {
                    push(s);
                }
            }
        }
        _ => {}
    }
}

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
    let (left, display) = match inner.split_once('|') {
        Some((l, d)) => (l.trim(), Some(d.trim()).filter(|d| !d.is_empty())),
        None => (inner.trim(), None),
    };
    match left.split_once("::") {
        Some((pred, name)) if front_matter::key_ok(pred.trim()) && !name.trim().is_empty() => {
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
    let mut out: Vec<BodyLink> = Vec::new();
    let mut code: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0u32;
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                let link = BodyLink {
                    target: dest_url.to_string(),
                    predicate: None,
                };
                if link.target.ends_with(".md") && !out.contains(&link) {
                    out.push(link);
                }
            }
            Event::Start(Tag::CodeBlock(_)) => depth += 1,
            Event::End(pulldown_cmark::TagEnd::CodeBlock) => depth = depth.saturating_sub(1),
            Event::Code(_) => code.push((range.start, range.end)),
            Event::Text(_) if depth > 0 => code.push((range.start, range.end)),
            _ => {}
        }
    }
    let masked = |at: usize| code.iter().any(|(s, e)| at >= *s && at < *e);
    let bytes = markdown.as_bytes();
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' && !masked(i) {
            if let Some(end) = markdown[i + 2..].find("]]") {
                let parts = link_parts(&markdown[i + 2..i + 2 + end]);
                let link = BodyLink {
                    target: parts.name.to_string(),
                    predicate: parts.predicate.map(str::to_string),
                };
                if !link.target.is_empty() && !link.target.contains('\n') && !out.contains(&link) {
                    out.push(link);
                }
                i += end + 4;
                continue;
            }
        }
        i += 1;
    }
    out
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

    fn tree(docs: &[(&str, &str)]) -> BTreeMap<String, String> {
        docs.iter()
            .map(|(p, c)| ((*p).to_string(), (*c).to_string()))
            .collect()
    }

    fn targets(edges: Option<&Vec<Edge>>) -> Vec<String> {
        edges.into_iter().flatten().map(|e| e.to.clone()).collect()
    }

    /// §4.5 resolution order: exact path, then unique basename, then
    /// unique alias - and an AMBIGUOUS name resolves to nothing rather
    /// than to a guess.
    #[test]
    fn a_link_resolves_by_path_then_basename_then_alias() {
        let g = WikiGraph::build(&tree(&[
            (
                "src.md",
                "---\nexact: \"people/anna.md\"\nbase: \"[[bob]]\"\nalias: \"[[Die Chefin]]\"\nnope: \"[[twin]]\"\n---\n",
            ),
            ("people/anna.md", "# Anna\n"),
            ("people/bob.md", "# Bob\n"),
            ("boss.md", "---\naliases: [Die Chefin]\n---\n# Boss\n"),
            ("a/twin.md", "# One\n"),
            ("b/twin.md", "# Two\n"),
        ]));
        let out = targets(g.out.get("src.md"));
        assert!(out.contains(&"people/anna.md".to_string()), "exact path");
        assert!(out.contains(&"people/bob.md".to_string()), "unique basename");
        assert!(out.contains(&"boss.md".to_string()), "unique alias");
        assert!(
            !out.iter().any(|t| t.ends_with("twin.md")),
            "an ambiguous name resolves to nothing"
        );
        assert_eq!(
            g.dangling.get("twin").map(Vec::len),
            Some(1),
            "…and stays dangling instead"
        );
    }

    /// The header key IS the predicate, including inside a qualified
    /// relation - `works_at: { to: … }` binds under `works_at`, never
    /// under `to`. A PLAIN body link carries none. Name resolution is
    /// CASE-EXACT, like the path resolution it extends.
    #[test]
    fn the_predicate_is_the_header_key() {
        let g = WikiGraph::build(&tree(&[
            (
                "p.md",
                "---\nworks_at:\n  to: \"[[Acme]]\"\n  since: 2019\nknows: [\"[[Bea]]\"]\nmiscased: \"[[acme]]\"\n---\nsee [Bea](Bea.md)\n",
            ),
            ("Acme.md", "# Acme\n"),
            ("Bea.md", "# Bea\n"),
        ]));
        assert_eq!(
            g.dangling.get("acme").map(Vec::len),
            Some(1),
            "resolution is case-exact"
        );
        let mut seen: Vec<(String, Option<String>, bool)> = g
            .out
            .get("p.md")
            .into_iter()
            .flatten()
            .map(|e| (e.to.clone(), e.predicate.clone(), e.header))
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("Acme.md".to_string(), Some("works_at".to_string()), true),
                ("Bea.md".to_string(), None, false),
                ("Bea.md".to_string(), Some("knows".to_string()), true),
            ]
        );
        // …and the in-edges see the same edges from the other side
        assert_eq!(targets(g.inn.get("Bea.md")), vec!["p.md", "p.md"]);
    }

    /// The property the incremental path exists for: after ANY change -
    /// edit, add, delete, rename - the updated graph equals a fresh build.
    #[test]
    fn an_updated_graph_equals_a_fresh_one() {
        let mut docs = vec![
            ("a.md", "---\nsee: \"[[b]]\"\n---\n# A\n"),
            ("b.md", "# B\n"),
            ("c.md", "links to [a](a.md)\n"),
        ];
        let mut t = tree(&docs);
        let mut g = WikiGraph::build(&t);

        let steps: Vec<(&str, Option<&str>)> = vec![
            // an edit that adds an edge
            ("b.md", Some("---\nback: \"[[a]]\"\n---\n# B\n")),
            // a new document that RESOLVES a dangling edge
            ("d.md", Some("---\nsee: \"[[ghost]]\"\n---\n")),
            ("ghost.md", Some("# Ghost\n")),
            // a deletion that turns an in-edge dangling
            ("a.md", None),
            // …and a fresh document under a name that was ambiguous
            ("e.md", Some("---\nsee: \"[[b]]\"\n---\n")),
        ];
        for (path, content) in steps {
            match content {
                Some(c) => {
                    t.insert(path.to_string(), c.to_string());
                }
                None => {
                    t.remove(path);
                }
            }
            let touched: BTreeSet<String> = [path.to_string()].into_iter().collect();
            g.update(&t, &touched);
            let fresh = WikiGraph::build(&t);
            assert_eq!(g.docs, fresh.docs, "docs drifted after {path}");
            assert_eq!(g.out, fresh.out, "out-edges drifted after {path}");
            assert_eq!(g.inn, fresh.inn, "in-edges drifted after {path}");
            assert_eq!(
                g.dangling, fresh.dangling,
                "dangling drifted after {path}"
            );
        }
        docs.clear();
    }

    /// A link inside a code fence is an example, not a claim.
    #[test]
    fn code_blocks_do_not_carry_links() {
        let links =
            body_link_targets("real [[Anna]]\n\n```\nnot [[Bob]]\n```\n\nand `[[Carl]]` inline\n");
        assert_eq!(links, vec!["Anna".to_string()]);
    }

    /// **The plan's §6 keystone**: a relation written in the SENTENCE is
    /// the same edge as the same relation written in the header - one set,
    /// not two, and only the `header` flag tells them apart. A predicate
    /// inside a code span is an example, not a claim; a `pred::` that is
    /// not a valid header key is no predicate at all (the whole string
    /// stays the name), and a SECOND `::` belongs to the name.
    #[test]
    fn an_inline_predicate_is_the_same_edge_as_the_header_key() {
        let g = WikiGraph::build(&tree(&[
            ("anna.md", "# Anna\n\nShe [[works_at::Acme]] since 2019.\n"),
            ("bob.md", "---\nworks_at: \"[[Acme]]\"\n---\n# Bob\n"),
            ("carl.md", "# Carl\n\n`[[works_at::Acme]]` is how it is written.\n"),
            ("dora.md", "# Dora\n\nShe [[1bad::Acme]] and [[a::b::c]].\n"),
            ("Acme.md", "# Acme\n"),
        ]));
        let edges = |path: &str| -> Vec<(String, Option<String>, bool)> {
            let mut e: Vec<(String, Option<String>, bool)> = g
                .out
                .get(path)
                .into_iter()
                .flatten()
                .map(|e| (e.to.clone(), e.predicate.clone(), e.header))
                .collect();
            e.sort();
            e
        };
        assert_eq!(
            edges("anna.md"),
            vec![("Acme.md".to_string(), Some("works_at".to_string()), false)],
            "the sentence carries the predicate, and it is not a header edge"
        );
        assert_eq!(
            edges("bob.md"),
            vec![("Acme.md".to_string(), Some("works_at".to_string()), true)],
            "…the header writes the same edge"
        );
        assert!(
            edges("carl.md").is_empty()
                && !g
                    .dangling
                    .values()
                    .flatten()
                    .any(|(src, _)| src == "carl.md"),
            "a predicate in a code span is not a claim about the graph"
        );
        assert!(
            g.dangling.contains_key("1bad::Acme"),
            "a predicate that is not a header key leaves an ordinary link"
        );
        assert!(
            g.dangling.contains_key("b::c"),
            "only the FIRST :: splits; the rest is the name"
        );
        // …and the target sees both claims from the other side
        assert_eq!(targets(g.inn.get("Acme.md")), vec!["anna.md", "bob.md"]);
    }

    /// The inventory (`wiki_props`) is the ontology as it IS, so it must
    /// see the form the republic writes: both spellings land in ONE bucket
    /// and their counts add. Per document a claim counts once, however
    /// often it is written.
    #[test]
    fn the_inventory_counts_inline_predicates_with_the_header_ones() {
        let g = WikiGraph::build(&tree(&[
            ("anna.md", "# Anna\n\n[[works_at::Acme]]\n"),
            ("bob.md", "---\nworks_at: \"[[Acme]]\"\n---\n# Bob\n"),
            (
                "cora.md",
                "---\nworks_at: \"[[Acme]]\"\n---\n# Cora\n\nand again [[works_at::Acme]]\n",
            ),
            ("Acme.md", "# Acme\n"),
        ]));
        assert_eq!(
            g.inventory.get("works_at").and_then(|v| v.get("Acme")),
            Some(&3),
            "three documents, one bucket - and cora's two spellings are one claim"
        );
    }

    /// Two hops, both directions, and the start is never its own neighbour.
    #[test]
    fn neighbors_walk_both_directions() {
        let g = WikiGraph::build(&tree(&[
            ("a.md", "---\nsee: \"[[b]]\"\n---\n"),
            ("b.md", "---\nsee: \"[[c]]\"\n---\n"),
            ("c.md", "# C\n"),
            ("far.md", "# Far\n"),
        ]));
        assert_eq!(
            g.neighbors("a.md", 1, 500),
            vec![("b.md".to_string(), 1)]
        );
        assert_eq!(
            g.neighbors("a.md", 2, 500),
            vec![("b.md".to_string(), 1), ("c.md".to_string(), 2)]
        );
        // …from the far end the walk runs backwards just the same
        assert_eq!(
            g.neighbors("c.md", 2, 500),
            vec![("b.md".to_string(), 1), ("a.md".to_string(), 2)]
        );
        assert!(g.neighbors("far.md", 2, 500).is_empty());
    }
}
