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
use std::ops::Range;

use pulldown_cmark::{Event, Parser, Tag};
use serde_json::Value;

use super::front_matter;

/// The name lookup behind resolution: a document's basename and its stem,
/// and every alias it declares.
struct NameIndex<'a> {
    docs: &'a BTreeMap<String, DocMeta>,
    by_name: BTreeMap<&'a str, Vec<&'a str>>,
    by_alias: BTreeMap<&'a str, Vec<&'a str>>,
    by_title: BTreeMap<&'a str, Vec<&'a str>>,
}

impl<'a> NameIndex<'a> {
    fn build(docs: &'a BTreeMap<String, DocMeta>) -> Self {
        let mut by_name: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
        for path in docs.keys() {
            let base = path.rsplit('/').next().unwrap_or(path);
            by_name.entry(base).or_default().push(path);
            if let Some(stem) = base.strip_suffix(".md") {
                by_name.entry(stem).or_default().push(path);
            }
        }
        let mut by_alias: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
        let mut by_title: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
        for (path, meta) in docs {
            for alias in &meta.aliases {
                by_alias.entry(alias.as_str()).or_default().push(path);
            }
            if let Some(t) = &meta.header_title {
                by_title.entry(t.as_str()).or_default().push(path);
            }
        }
        NameIndex {
            docs,
            by_name,
            by_alias,
            by_title,
        }
    }

    /// Exact path, then unique basename or stem, then unique alias, then
    /// unique header title. Case EXACT, like the path resolution it extends.
    fn bind(&self, name: &str) -> Option<String> {
        if self.docs.contains_key(name) {
            return Some(name.to_string());
        }
        let unique = |index: &BTreeMap<&str, Vec<&str>>| -> Option<String> {
            match index.get(name) {
                Some(hits) if hits.len() == 1 => hits.first().map(|p| (*p).to_string()),
                _ => None,
            }
        };
        unique(&self.by_name)
            .or_else(|| unique(&self.by_alias))
            .or_else(|| unique(&self.by_title))
    }

    /// Every document the name could mean, strongest route first and the
    /// bound one at the head. A `case` entry is a case-insensitive match
    /// the case-exact rule does NOT bind - the "did you mean", so an agent
    /// can pick the real spelling instead of writing a dangling link.
    fn candidates(&self, name: &str) -> Vec<(String, &'static str)> {
        let mut how: BTreeMap<&str, &'static str> = BTreeMap::new();
        let mut note = |path: &'a str, via: &'static str| {
            how.entry(path).or_insert(via);
        };
        if let Some((path, _)) = self.docs.get_key_value(name) {
            note(path, "path");
        }
        for path in self.by_name.get(name).into_iter().flatten() {
            note(path, "basename");
        }
        for path in self.by_alias.get(name).into_iter().flatten() {
            note(path, "alias");
        }
        for path in self.by_title.get(name).into_iter().flatten() {
            note(path, "title");
        }
        let folded = name.to_lowercase();
        for path in self.docs.keys() {
            if path.to_lowercase() == folded {
                note(path, "case");
            }
        }
        for (key, paths) in self.by_name.iter().chain(self.by_alias.iter()).chain(self.by_title.iter()) {
            if key.to_lowercase() != folded {
                continue;
            }
            for path in paths {
                note(path, "case");
            }
        }
        let bound = self.bind(name);
        let mut out: Vec<(String, &'static str)> = how
            .into_iter()
            .map(|(path, via)| (path.to_string(), via))
            .collect();
        // the binding first, the rest path-sorted: a deterministic order
        out.sort_by_key(|(path, _)| (Some(path) != bound.as_ref(), path.clone()));
        out
    }
}

/// One `type` value with the header keys its pages carry (E3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeProps {
    /// The `type` value.
    pub(crate) kind: String,
    /// How many pages carry it.
    pub(crate) pages: u64,
    /// Its keys with the page count each, most-carried first.
    pub(crate) keys: Vec<(String, u64)>,
}

/// One predicate asserted between a type pair its own wiki rarely uses
/// (E3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectionOutlier {
    /// The predicate.
    pub(crate) predicate: String,
    /// The `type` of the pages asserting it.
    pub(crate) subject_type: String,
    /// The `type` of the pages it points at.
    pub(crate) object_type: String,
    /// How many edges run that way.
    pub(crate) count: u64,
    /// The pair the predicate usually runs between.
    pub(crate) usual: (String, String),
    /// How many edges run the usual way.
    pub(crate) usual_count: u64,
    /// The asserting pages, path-sorted.
    pub(crate) from: Vec<String>,
}

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
    /// The display title: the header's `title`, else the first heading.
    pub(crate) title: Option<String>,
    /// The header's `title` alone - a resolution key like an alias (F13:
    /// `[[Unitree Robotics]]` dangled although a page carried exactly that
    /// title). A heading never binds: "Quellen" is on every page.
    pub(crate) header_title: Option<String>,
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
        self.docs.insert(path.to_string(), doc_meta(content));
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

    /// Which documents each name denotes - by path, basename, stem,
    /// alias or header title - over the tree a `wiki_edit` WOULD leave
    /// (E1): `after` replaces (or adds) those documents, `gone` drops
    /// them. The case-fold "did you mean" route is left out: a collision
    /// is what the resolver would really confuse. Computed off the base
    /// alone, the check refuses an alias this very call moves.
    pub(crate) fn name_owners_after(
        &self,
        names: &BTreeSet<String>,
        after: &BTreeMap<String, String>,
        gone: &BTreeSet<String>,
    ) -> BTreeMap<String, Vec<String>> {
        let mut docs = self.docs.clone();
        for path in gone {
            docs.remove(path);
        }
        for (path, content) in after {
            docs.insert(path.clone(), doc_meta(content));
        }
        owners_in(&docs, names)
    }

    /// What a `[[name]]` binds to, and everything else it could mean -
    /// THE lookup, so `wiki_resolve` cannot drift from what [`Self::resolve`]
    /// actually binds.
    pub(crate) fn resolve_name(&self, name: &str) -> (Option<String>, Vec<(String, &'static str)>) {
        let index = NameIndex::build(&self.docs);
        (index.bind(name), index.candidates(name))
    }

    /// Resolve every raw edge against the current document set: exact path,
    /// then unique basename, then unique alias, else dangling.
    fn resolve(&mut self) {
        let index = NameIndex::build(&self.docs);
        let mut out: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
        let mut inn: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
        let mut dangling: BTreeMap<String, Vec<(String, Edge)>> = BTreeMap::new();
        for (src, edges) in &self.raw {
            for raw in edges {
                let hit = index.bind(&raw.target);
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

    /// Documents reachable from `path`, nearest first, plus whether the cap
    /// cut the walk short. Breadth first, so the edge REPORTED for a
    /// document is the one on a shortest route to it.
    pub(crate) fn neighbors(&self, path: &str, walk: &Walk<'_>) -> (Vec<Reached>, bool) {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        seen.insert(path);
        // each entry carries the route to it, so a hit can say what it came
        // through without a second walk
        let mut frontier: Vec<(&str, Vec<&str>)> = vec![(path, Vec::new())];
        let mut found: Vec<Reached> = Vec::new();
        let mut hop = 0u32;
        while !frontier.is_empty() {
            hop += 1;
            if !walk.transitive && hop > walk.depth {
                break;
            }
            let mut next: Vec<(&str, Vec<&str>)> = Vec::new();
            for (node, route) in &frontier {
                let sides = [
                    ("out", walk.out.then(|| self.out.get(*node)).flatten()),
                    ("in", walk.inn.then(|| self.inn.get(*node)).flatten()),
                ];
                for (side, edges) in sides {
                    for e in edges.into_iter().flatten() {
                        // matched on the PREDICATE, never on `header`: a
                        // typed link in the prose carries one too
                        if walk
                            .predicate
                            .is_some_and(|p| e.predicate.as_deref() != Some(p))
                        {
                            continue;
                        }
                        if seen.contains(e.to.as_str()) {
                            continue;
                        }
                        // `seen` borrows self.docs' keys, so insert the OWNED
                        // key the graph already holds
                        let Some((owned, _)) = self.docs.get_key_value(&e.to) else {
                            continue;
                        };
                        seen.insert(owned.as_str());
                        // `via` names what lies BETWEEN start and the hit,
                        // so the first hop has none
                        let via: Vec<&str> = if hop == 1 {
                            Vec::new()
                        } else {
                            let mut v = route.clone();
                            v.push(*node);
                            v
                        };
                        found.push(Reached {
                            path: owned.clone(),
                            distance: hop,
                            predicate: e.predicate.clone(),
                            direction: side,
                            via: via.iter().map(|p| (*p).to_string()).collect(),
                        });
                        next.push((owned.as_str(), via));
                        // one past the cap, so "was it cut" is a fact and
                        // not a guess about a full last page
                        if found.len() > walk.cap {
                            found.truncate(walk.cap);
                            return (found, true);
                        }
                    }
                }
            }
            frontier = next;
        }
        (found, false)
    }

    /// **Hygiene, part 1** (§4.11): the names this republic REFERENCES and
    /// does not carry, each with the documents that reference them - the
    /// "what should I write next" signal, computed on every resolution
    /// pass and until now exposed nowhere.
    pub(crate) fn dangling_targets(&self) -> Vec<(String, Vec<String>)> {
        self.dangling
            .iter()
            .map(|(name, refs)| {
                let mut from: Vec<String> = refs.iter().map(|(src, _)| src.clone()).collect();
                from.sort_unstable();
                from.dedup();
                (name.clone(), from)
            })
            .collect()
    }

    /// **Hygiene, part 2**: documents nothing points at. `inn` is filled
    /// only for RESOLVED edges and never holds an empty list, so absence
    /// from it IS the orphan test.
    pub(crate) fn orphans(&self) -> Vec<String> {
        self.docs
            .keys()
            .filter(|p| !self.inn.contains_key(*p))
            .cloned()
            .collect()
    }

    /// **Hygiene, part 4** (E3): per `type` value, how many pages carry
    /// it and how many of them carry each header key. `key_drift` sees
    /// two SPELLINGS of one key; this sees a page of the same type
    /// carrying an entirely different key from its peers.
    pub(crate) fn props_by_type(&self) -> Vec<TypeProps> {
        let mut per: BTreeMap<&str, (u64, BTreeMap<&str, u64>)> = BTreeMap::new();
        for (path, meta) in &self.docs {
            let Some(kind) = meta.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) else {
                continue;
            };
            let slot = per.entry(kind).or_default();
            slot.0 = slot.0.saturating_add(1);
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            for (key, _) in self.props.get(path).into_iter().flatten() {
                if seen.insert(key.as_str()) {
                    let n: &mut u64 = slot.1.entry(key.as_str()).or_default();
                    *n = n.saturating_add(1);
                }
            }
        }
        let mut out: Vec<TypeProps> = per
            .into_iter()
            .map(|(kind, (pages, keys))| {
                let mut keys: Vec<(String, u64)> =
                    keys.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
                keys.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                TypeProps {
                    kind: kind.to_string(),
                    pages,
                    keys,
                }
            })
            .collect();
        out.sort_by(|a, b| b.pages.cmp(&a.pages).then_with(|| a.kind.cmp(&b.kind)));
        out
    }

    /// **Hygiene, part 5** (E3): a predicate whose (subject type, object
    /// type) pair is RARE beside the pair the same predicate usually
    /// runs between - the reversed-relation signal (13 `authored_by`
    /// edges pointing person → work while the norm is work → person).
    ///
    /// HEURISTIC, and deliberately a blunt one: the wiki's own
    /// distribution is the only norm there is, so a pair is flagged only
    /// when a dominant pair dwarfs it.
    pub(crate) fn direction_outliers(&self) -> Vec<DirectionOutlier> {
        /// Below this many typed edges a predicate has no distribution.
        const MIN_EDGES: u64 = 5;
        /// The dominant pair must outnumber the flagged one this far.
        const FACTOR: u64 = 4;
        let mut per: BTreeMap<&str, BTreeMap<(&str, &str), BTreeSet<&str>>> = BTreeMap::new();
        for (src, edges) in &self.out {
            let Some(from) = self.docs.get(src).and_then(|m| m.kind.as_deref()) else {
                continue;
            };
            for edge in edges {
                let Some(pred) = edge.predicate.as_deref() else {
                    continue;
                };
                let Some(to) = self.docs.get(&edge.to).and_then(|m| m.kind.as_deref()) else {
                    continue;
                };
                per.entry(pred)
                    .or_default()
                    .entry((from, to))
                    .or_default()
                    .insert(src.as_str());
            }
        }
        let mut out = Vec::new();
        for (pred, pairs) in per {
            let count_of = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
            let total: u64 = pairs.values().map(|v| count_of(v.len())).sum();
            if total < MIN_EDGES || pairs.len() < 2 {
                continue;
            }
            let Some((usual, usual_pages)) = pairs.iter().max_by_key(|(_, v)| v.len()) else {
                continue;
            };
            let usual_count = count_of(usual_pages.len());
            for (pair, pages) in &pairs {
                let count = count_of(pages.len());
                if pair == usual || count.saturating_mul(FACTOR) > usual_count {
                    continue;
                }
                out.push(DirectionOutlier {
                    predicate: pred.to_string(),
                    subject_type: pair.0.to_string(),
                    object_type: pair.1.to_string(),
                    count,
                    usual: (usual.0.to_string(), usual.1.to_string()),
                    usual_count,
                    from: pages.iter().map(|p| (*p).to_string()).collect(),
                });
            }
        }
        out.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.predicate.cmp(&b.predicate))
        });
        out
    }

    /// **Hygiene, part 3**: keys that differ only in case or separator.
    /// `status` and `Status` are two fields to every property query, and
    /// nothing else in the tool set says so. Read off the INVENTORY, not
    /// off header parsing, so an inline predicate counts like a header key.
    pub(crate) fn key_drift(&self) -> Vec<Vec<String>> {
        let mut folded: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for key in self.inventory.keys() {
            let fold: String = key
                .chars()
                .filter(|c| !matches!(c, '-' | '_' | ' '))
                .flat_map(char::to_lowercase)
                .collect();
            folded.entry(fold).or_default().push(key.clone());
        }
        folded.into_values().filter(|g| g.len() > 1).collect()
    }
}

/// What a neighbour walk follows.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Walk<'a> {
    /// How many hops; ignored when `transitive`.
    pub(crate) depth: u32,
    /// Follow out-edges.
    pub(crate) out: bool,
    /// Follow in-edges.
    pub(crate) inn: bool,
    /// Only edges under this predicate.
    pub(crate) predicate: Option<&'a str>,
    /// Walk that ONE predicate to a fixpoint instead of `depth` hops.
    pub(crate) transitive: bool,
    /// At most this many documents.
    pub(crate) cap: usize,
}

/// One document a walk reached, with the edge that reached it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reached {
    /// The document's path.
    pub(crate) path: String,
    /// How many hops from the start.
    pub(crate) distance: u32,
    /// The predicate of that edge, if it carries one.
    pub(crate) predicate: Option<String>,
    /// `"out"` or `"in"`, as seen from the document it was reached FROM.
    pub(crate) direction: &'static str,
    /// The documents BETWEEN the start and this one, in walk order.
    pub(crate) via: Vec<String>,
}

/// Every scalar a header value carries, as the human reads it - one for a
/// scalar, N for a list, the members of a qualified relation. The SEARCH
/// renders values with it too, so the vocabulary `wiki_props` reports is
/// exactly the one a `props` filter narrows on.
pub(crate) fn scalar_strings(value: &Value) -> Vec<String> {
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
pub(crate) fn string_list(v: Option<&Value>) -> Vec<String> {
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

/// What the name index needs about one document. A free function so a
/// WORKING COPY can be measured with the same rule the index uses (E1).
pub(crate) fn doc_meta(content: &str) -> DocMeta {
    let (props, _) = front_matter::properties(content);
    let props = props.unwrap_or_default();
    DocMeta {
        title: front_matter::title(content),
        header_title: props
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        kind: props.get("type").and_then(Value::as_str).map(str::to_string),
        aliases: string_list(props.get("aliases")),
    }
}

/// Which documents each name already denotes, over a given document set.
fn owners_in(
    docs: &BTreeMap<String, DocMeta>,
    names: &BTreeSet<String>,
) -> BTreeMap<String, Vec<String>> {
    let index = NameIndex::build(docs);
    names
        .iter()
        .map(|name| {
            let mut owners: Vec<String> = index
                .candidates(name)
                .into_iter()
                .filter(|(_, via)| *via != "case")
                .map(|(path, _)| path)
                .collect();
            owners.sort();
            owners.dedup();
            (name.clone(), owners)
        })
        .collect()
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

    /// F13: the header's `title` binds like an alias - a unique one, the
    /// exact spelling; a heading never does, and an ambiguous title
    /// resolves to nothing. `wiki_resolve` names the route `title`.
    #[test]
    fn a_header_title_binds_like_an_alias() {
        let g = WikiGraph::build(&tree(&[
            (
                "src.md",
                "by title [[Unitree Robotics]], by heading [[Modelle]], twins [[Roomba]]\n",
            ),
            ("hersteller/unitree.md", "---\ntitle: Unitree Robotics\n---\n## Modelle\n"),
            ("hersteller/tesla.md", "---\ntitle: Tesla\n---\n## Modelle\n"),
            ("a.md", "---\ntitle: Roomba\n---\n"),
            ("b.md", "---\ntitle: Roomba\n---\n"),
        ]));
        let out = targets(g.out.get("src.md"));
        assert_eq!(out, vec!["hersteller/unitree.md".to_string()], "the unique title binds, nothing else");
        assert!(g.dangling.contains_key("Modelle"), "a heading is prose, not a name");
        assert!(g.dangling.contains_key("Roomba"), "an ambiguous title resolves to nothing");
        let (exact, candidates) = g.resolve_name("Unitree Robotics");
        assert_eq!(exact.as_deref(), Some("hersteller/unitree.md"));
        assert_eq!(candidates[0].1, "title");
    }

    /// G15: a markdown link to a URL that ends in `.md` cites a source; only
    /// a path is a wiki link.
    #[test]
    fn a_url_ending_in_md_is_a_source_not_a_link() {
        let g = WikiGraph::build(&tree(&[
            (
                "n.md",
                "see [NIP-01](https://github.com/nostr-protocol/nips/blob/master/01.md) and [local](a.md)\n",
            ),
            ("a.md", "# A\n"),
        ]));
        assert_eq!(targets(g.out.get("n.md")), vec!["a.md".to_string()]);
        assert!(g.dangling.is_empty(), "the URL dangles nowhere: {:?}", g.dangling);
    }

    /// F14: inside a table cell the display separator is written `\|`;
    /// the backslash is the escape, never the target's last character.
    #[test]
    fn a_table_cell_escape_is_the_display_separator() {
        let p = link_parts("roboter/humanoide/1x-neo.md\\|NEO");
        assert_eq!((p.name, p.display, p.predicate), ("roboter/humanoide/1x-neo.md", Some("NEO"), None));
        let p = link_parts("made_by::hersteller/1x.md\\|1X");
        assert_eq!((p.predicate, p.name, p.display), (Some("made_by"), "hersteller/1x.md", Some("1X")));
        let plain = link_parts("a.md|A");
        assert_eq!((plain.name, plain.display), ("a.md", Some("A")), "the bare form is unchanged");
        let g = WikiGraph::build(&tree(&[
            ("t.md", "| [[n.md\\|NEO]] | 2025 |\n"),
            ("n.md", "# N\n"),
        ]));
        assert_eq!(targets(g.out.get("t.md")), vec!["n.md".to_string()]);
        assert!(g.dangling.is_empty());
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

    /// Both directions, `depth` hops, cap 500 - the walk `wiki_neighbors`
    /// runs when the caller narrows nothing.
    fn walk(depth: u32) -> Walk<'static> {
        Walk {
            depth,
            out: true,
            inn: true,
            predicate: None,
            transitive: false,
            cap: 500,
        }
    }

    fn hops(found: &[Reached]) -> Vec<(&str, u32)> {
        found
            .iter()
            .map(|r| (r.path.as_str(), r.distance))
            .collect()
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
        assert_eq!(hops(&g.neighbors("a.md", &walk(1)).0), vec![("b.md", 1)]);
        assert_eq!(
            hops(&g.neighbors("a.md", &walk(2)).0),
            vec![("b.md", 1), ("c.md", 2)]
        );
        // …from the far end the walk runs backwards just the same
        assert_eq!(
            hops(&g.neighbors("c.md", &walk(2)).0),
            vec![("b.md", 1), ("a.md", 2)]
        );
        assert!(g.neighbors("far.md", &walk(2)).0.is_empty());
    }

    /// §7 step 2: a hit says HOW it was reached - under which predicate,
    /// which way that edge points, and what lies between.
    #[test]
    fn a_neighbour_carries_the_edge_that_reached_it() {
        let g = WikiGraph::build(&tree(&[
            ("a.md", "---\npart_of: \"[[b]]\"\n---\n"),
            ("b.md", "---\npart_of: \"[[c]]\"\n---\n"),
            ("c.md", "# C\n"),
        ]));
        let (found, capped) = g.neighbors("c.md", &walk(2));
        assert!(!capped);
        assert_eq!(hops(&found), vec![("b.md", 1), ("a.md", 2)]);
        assert_eq!(found[0].predicate.as_deref(), Some("part_of"));
        assert_eq!(
            found[0].direction, "in",
            "c is reached FROM b, so that edge points at it"
        );
        assert!(found[0].via.is_empty(), "the first hop has nothing between");
        assert_eq!(
            found[1].via,
            vec!["b.md".to_string()],
            "the route names what lies between"
        );
    }

    /// The predicate filter follows ONE relation, and `transitive` closes
    /// it instead of stopping at the depth bound. A cycle terminates on
    /// the `seen` set the walk already keeps.
    #[test]
    fn a_predicate_walk_closes_transitively_and_a_cycle_terminates() {
        let g = WikiGraph::build(&tree(&[
            ("a.md", "---\npart_of: \"[[b]]\"\nknows: \"[[far]]\"\n---\n"),
            ("b.md", "---\npart_of: \"[[c]]\"\n---\n"),
            ("c.md", "---\npart_of: \"[[d]]\"\n---\n"),
            ("d.md", "# D\n"),
            ("far.md", "# Far\n"),
        ]));
        let bounded = Walk {
            predicate: Some("part_of"),
            ..walk(2)
        };
        assert_eq!(
            hops(&g.neighbors("a.md", &bounded).0),
            vec![("b.md", 1), ("c.md", 2)],
            "the depth bound holds, and `knows` is not this walk"
        );
        let closed = Walk {
            transitive: true,
            ..bounded
        };
        assert_eq!(
            hops(&g.neighbors("a.md", &closed).0),
            vec![("b.md", 1), ("c.md", 2), ("d.md", 3)],
            "…and the closure runs past it"
        );
        // one direction is a different question: what belongs to d, rather
        // than what d is part of
        let inward = Walk {
            out: false,
            ..closed
        };
        assert_eq!(
            hops(&g.neighbors("d.md", &inward).0),
            vec![("c.md", 1), ("b.md", 2), ("a.md", 3)]
        );
        assert!(g.neighbors("a.md", &Walk { inn: false, ..inward }).0.is_empty());

        let cycle = WikiGraph::build(&tree(&[
            ("a.md", "---\npart_of: \"[[b]]\"\n---\n"),
            ("b.md", "---\npart_of: \"[[c]]\"\n---\n"),
            ("c.md", "---\npart_of: \"[[a]]\"\n---\n"),
        ]));
        assert_eq!(
            hops(&cycle.neighbors("a.md", &Walk { inn: false, ..closed }).0),
            vec![("b.md", 1), ("c.md", 2)],
            "a cycle terminates and the start is not its own neighbour"
        );
    }

    /// The cap bounds the walk, and the caller is TOLD when it was hit -
    /// a short answer and a cut answer are different claims.
    #[test]
    fn the_cap_bounds_the_walk_and_says_so() {
        let g = WikiGraph::build(&tree(&[
            ("hub.md", "---\nsee: [\"[[a]]\", \"[[b]]\", \"[[c]]\"]\n---\n"),
            ("a.md", "# A\n"),
            ("b.md", "# B\n"),
            ("c.md", "# C\n"),
        ]));
        let (found, capped) = g.neighbors("hub.md", &Walk { cap: 2, ..walk(1) });
        assert_eq!(found.len(), 2);
        assert!(capped, "the walk was cut short and says so");
        let (found, capped) = g.neighbors("hub.md", &Walk { cap: 3, ..walk(1) });
        assert_eq!(found.len(), 3);
        assert!(!capped, "exactly the cap with nothing left is not a cut");
    }

    /// **Hygiene** (§4.11): what this republic references and does not
    /// have, and what nothing points at. Both read off the SAME resolution
    /// pass `wiki_links` answers from, so neither can contradict it.
    #[test]
    fn the_hygiene_lists_name_the_missing_and_the_unreferenced() {
        let g = WikiGraph::build(&tree(&[
            ("a.md", "---\nsee: \"[[b]]\"\nnope: \"[[ghost]]\"\n---\n"),
            ("b.md", "# B\n"),
            ("lonely.md", "also [[ghost]]\n"),
        ]));
        assert_eq!(
            g.dangling_targets(),
            vec![(
                "ghost".to_string(),
                vec!["a.md".to_string(), "lonely.md".to_string()]
            )]
        );
        assert_eq!(
            g.orphans(),
            vec!["a.md".to_string(), "lonely.md".to_string()],
            "b.md is pointed at, the other two are not"
        );
    }

    /// `status` and `Status` are two fields to every property query, and a
    /// separator does the same thing - the drift hint is what says so.
    /// Read off the INVENTORY, so an inline predicate counts like a header
    /// key.
    #[test]
    fn key_drift_groups_keys_that_differ_only_in_case_or_separator() {
        let g = WikiGraph::build(&tree(&[
            ("a.md", "---\nstatus: draft\nworks_at: Acme\n---\n"),
            ("b.md", "---\nStatus: final\nworks-at: Beta\n---\n"),
            ("c.md", "---\nalone: 1\n---\n"),
        ]));
        assert_eq!(
            g.key_drift(),
            vec![
                vec!["Status".to_string(), "status".to_string()],
                vec!["works-at".to_string(), "works_at".to_string()],
            ],
            "a key nobody else spells differently is not drift"
        );
    }

    /// **E2**: a rename carries its in-links. Only what NAMES THE PATH
    /// moves - a `[[Name]]` binds by title or alias and survives the move
    /// untouched, a URL that happens to end in `.md` is a source, and an
    /// example in a code span is not a claim about the graph.
    #[test]
    fn a_rename_rewrites_only_the_links_that_name_the_path() {
        let doc = "See [Lexicon](standards/lexicon.md), [[standards/lexicon.md|the one]],\n\
                   [[Lexicon]] and <https://x.test/standards/lexicon.md>.\n\
                   A `[x](standards/lexicon.md)` span stays.\n";
        let out = rewrite_link_target(doc, "standards/lexicon.md", "standards/atproto-lexicon.md")
            .expect("three of them name the path");
        assert!(out.contains("[Lexicon](standards/atproto-lexicon.md)"));
        assert!(out.contains("[[standards/atproto-lexicon.md|the one]]"));
        assert!(out.contains("[[Lexicon]]"), "a name link is not a path link");
        assert!(
            out.contains("`[x](standards/lexicon.md)`"),
            "a code span is no claim: {out}"
        );
        assert_eq!(
            rewrite_link_target("nothing here\n", "standards/lexicon.md", "x.md"),
            None
        );
    }

    /// **E3**: the per-type key histogram, and a predicate whose direction
    /// the wiki itself contradicts.
    #[test]
    fn the_type_histogram_and_the_direction_check_read_the_distribution() {
        let mut docs: Vec<(String, String)> = vec![(
            "menschen/anna.md".to_string(),
            "---\ntype: person\nnotable_year: 1970\n---\n# Anna\n".to_string(),
        )];
        for n in 0..8 {
            docs.push((
                format!("werke/w{n}.md"),
                "---\ntype: work\n---\nby [[authored_by::menschen/anna.md]]\n".to_string(),
            ));
        }
        docs.push((
            "menschen/odd.md".to_string(),
            "---\ntype: person\nyear: 1970\n---\nwrote [[authored_by::werke/w0.md]]\n".to_string(),
        ));
        let pairs: Vec<(&str, &str)> = docs
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let g = WikiGraph::build(&tree(&pairs));

        let by_type = g.props_by_type();
        let person = by_type
            .iter()
            .find(|t| t.kind == "person")
            .expect("the person group");
        assert_eq!(person.pages, 2);
        assert!(person.keys.contains(&("notable_year".to_string(), 1)), "{person:?}");
        assert!(
            person.keys.contains(&("year".to_string(), 1)),
            "the wrong key shows: {person:?}"
        );

        let odd = g.direction_outliers();
        assert_eq!(odd.len(), 1, "{odd:?}");
        assert_eq!(odd[0].predicate, "authored_by");
        assert_eq!(
            (odd[0].subject_type.as_str(), odd[0].object_type.as_str()),
            ("person", "work")
        );
        assert_eq!(odd[0].count, 1, "one edge runs the other way");
        assert_eq!(odd[0].usual_count, 8, "against eight that run the usual way");
    }
}
