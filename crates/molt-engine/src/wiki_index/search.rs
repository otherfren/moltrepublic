// SPDX-License-Identifier: GPL-3.0-or-later

//! Full-text search over the folded base
//! (`docs/memory/knowledge_base_scale.md` §4.6).
//!
//! A RAM index, rebuilt from the fold and never written to disk: like the
//! link graph it is a pure derivation, so losing it costs a rebuild and
//! nothing else. The at-rest posture is what keeps it in RAM - an index of
//! the wiki IS the wiki, so it must not lie beside the sealed workspace.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    Facet, FacetOptions, Field, IndexRecordOption, Schema, Value as _, STORED, STRING, TEXT,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

use super::front_matter;

/// The longest property value that becomes a facet.
const MAX_FACET_VALUE: usize = 64;

/// One hit.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Hit {
    /// The document's path.
    pub(crate) path: String,
    /// Its first heading, when it has one.
    pub(crate) title: Option<String>,
    /// tantivy's score.
    pub(crate) score: f32,
    /// A fragment of the body around the match.
    pub(crate) snippet: String,
}

/// What a query narrows on beside its text.
#[derive(Debug, Clone, Default)]
pub(crate) struct Filters {
    /// Every tag must be present.
    pub(crate) tags: Vec<String>,
    /// The header's `type`.
    pub(crate) kind: Option<String>,
    /// A folder prefix.
    pub(crate) folder: Option<String>,
}

struct Fields {
    path: Field,
    title: Field,
    body: Field,
    folder: Field,
    facet: Field,
}

/// The index and everything needed to query it.
pub(crate) struct WikiSearch {
    index: Index,
    writer: IndexWriter<TantivyDocument>,
    reader: IndexReader,
    f: Fields,
}

impl WikiSearch {
    /// A fresh empty index.
    pub(crate) fn new() -> tantivy::Result<Self> {
        let mut sb = Schema::builder();
        let path = sb.add_text_field("path", STRING | STORED);
        let title = sb.add_text_field("title", TEXT | STORED);
        let body = sb.add_text_field("body", TEXT | STORED);
        let folder = sb.add_text_field("folder", STRING);
        let facet = sb.add_facet_field("facet", FacetOptions::default());
        let index = Index::create_in_ram(sb.build());
        // one worker: the index is small and the actor is not waiting on
        // throughput, it is waiting on latency
        let writer = index.writer_with_num_threads(1, 15_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(WikiSearch {
            index,
            writer,
            reader,
            f: Fields {
                path,
                title,
                body,
                folder,
                facet,
            },
        })
    }

    /// The whole tree, from scratch.
    pub(crate) fn build(tree: &BTreeMap<String, String>) -> tantivy::Result<Self> {
        let mut s = WikiSearch::new()?;
        for (path, content) in tree {
            s.write(path, content);
        }
        s.commit()?;
        Ok(s)
    }

    /// Re-index the touched documents (a missing one is a deletion).
    pub(crate) fn update(
        &mut self,
        tree: &BTreeMap<String, String>,
        touched: &BTreeSet<String>,
    ) -> tantivy::Result<()> {
        for path in touched {
            self.writer
                .delete_term(Term::from_field_text(self.f.path, path));
            if let Some(content) = tree.get(path) {
                self.write(path, content);
            }
        }
        self.commit()
    }

    /// One document into the writer. A delete for the same path must have
    /// been issued first: tantivy has no update, only delete + add.
    fn write(&mut self, path: &str, content: &str) {
        let (props, _) = front_matter::properties(content);
        let props = props.unwrap_or_default();
        let (_, body) = front_matter::split(content);
        let mut d = doc!(
            self.f.path => path,
            self.f.title => front_matter::first_heading(content).unwrap_or_default(),
            self.f.body => body,
            self.f.folder => folder_of(path),
        );
        for facet in facets_of(&props) {
            d.add_facet(self.f.facet, facet);
        }
        // the writer only fails when its worker died, and then the next
        // commit says so - a lost document is a stale hit, never a wrong
        // answer about the republic
        let _ = self.writer.add_document(d);
    }

    fn commit(&mut self) -> tantivy::Result<()> {
        self.writer.commit()?;
        // ReloadPolicy::Manual: without this the searcher never sees it
        self.reader.reload()
    }

    /// Run one query. `cursor` is an offset into the ranked hits.
    pub(crate) fn search(
        &self,
        text: &str,
        filters: &Filters,
        limit: usize,
        cursor: usize,
    ) -> tantivy::Result<(Vec<Hit>, bool)> {
        let searcher = self.reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let text = text.trim();
        if !text.is_empty() {
            let parser = QueryParser::for_index(&self.index, vec![self.f.title, self.f.body]);
            clauses.push((Occur::Must, parser.parse_query(text)?));
        }
        for tag in &filters.tags {
            clauses.push((Occur::Must, facet_clause(self.f.facet, &format!("/tag/{}", facet_seg(tag)))));
        }
        if let Some(kind) = &filters.kind {
            clauses.push((Occur::Must, facet_clause(self.f.facet, &format!("/type/{}", facet_seg(kind)))));
        }
        if let Some(folder) = &filters.folder {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.f.folder, folder),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if clauses.is_empty() {
            return Ok((Vec::new(), false));
        }
        let query: Box<dyn Query> = Box::new(BooleanQuery::new(clauses));
        // one past the page, so "is there more" needs no second search
        let found = searcher.search(
            &*query,
            &TopDocs::with_limit(limit + 1).and_offset(cursor).order_by_score(),
        )?;
        let more = found.len() > limit;
        let mut snippets = SnippetGenerator::create(&searcher, &*query, self.f.body).ok();
        if let Some(g) = snippets.as_mut() {
            g.set_max_num_chars(160);
        }
        let mut hits = Vec::new();
        for (score, addr) in found.into_iter().take(limit) {
            let d: TantivyDocument = searcher.doc(addr)?;
            let text_of = |field: Field| {
                d.get_first(field)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            let snippet = snippets
                .as_ref()
                .map(|g| g.snippet_from_doc(&d).fragment().to_string())
                .unwrap_or_default();
            hits.push(Hit {
                path: text_of(self.f.path).unwrap_or_default(),
                title: text_of(self.f.title).filter(|t| !t.is_empty()),
                score,
                snippet,
            });
        }
        Ok((hits, more))
    }
}

/// A facet segment cannot carry the separator.
fn facet_seg(s: &str) -> String {
    s.replace('/', "-")
}

fn facet_clause(field: Field, path: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_facet(field, &Facet::from(path)),
        IndexRecordOption::Basic,
    ))
}

/// The document's folder, "" at the root.
fn folder_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(at) => &path[..at],
        None => "",
    }
}

/// `/tag/<t>`, `/type/<t>` and `/prop/<key>/<value>` for short string
/// properties - the facets a filter narrows on.
fn facets_of(props: &serde_json::Map<String, Value>) -> Vec<Facet> {
    let mut out = Vec::new();
    let mut push = |path: String| out.push(Facet::from(path.as_str()));
    for (key, value) in props {
        match (key.as_str(), value) {
            ("tags", Value::Array(items)) => {
                for t in items.iter().filter_map(Value::as_str) {
                    push(format!("/tag/{}", facet_seg(t)));
                }
            }
            ("tags", Value::String(t)) => push(format!("/tag/{}", facet_seg(t))),
            ("type", Value::String(t)) => push(format!("/type/{}", facet_seg(t))),
            (_, Value::String(v)) if v.len() <= MAX_FACET_VALUE => {
                push(format!("/prop/{}/{}", facet_seg(key), facet_seg(v)));
            }
            _ => {}
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

    fn paths(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.path.as_str()).collect()
    }

    #[test]
    fn it_finds_by_text_and_narrows_by_facet_and_folder() {
        let t = tree(&[
            (
                "people/anna.md",
                "---\ntype: person\ntags: [gruender, berlin]\n---\n# Anna\nAnna baut die Zentrale.\n",
            ),
            (
                "people/bob.md",
                "---\ntype: person\ntags: [berlin]\n---\n# Bob\nBob baut nichts.\n",
            ),
            ("orte/berlin.md", "---\ntype: place\n---\n# Berlin\nEine Zentrale.\n"),
        ]);
        let s = WikiSearch::build(&t).expect("index");

        let (hits, more) = s.search("Zentrale", &Filters::default(), 10, 0).expect("search");
        assert_eq!(hits.len(), 2, "two documents carry the word");
        assert!(!more);
        assert!(!hits[0].snippet.is_empty(), "a hit shows its fragment");

        let (hits, _) = s
            .search(
                "Zentrale",
                &Filters {
                    kind: Some("person".to_string()),
                    ..Filters::default()
                },
                10,
                0,
            )
            .expect("search");
        assert_eq!(paths(&hits), vec!["people/anna.md"]);

        let (hits, _) = s
            .search(
                "baut",
                &Filters {
                    tags: vec!["gruender".to_string()],
                    ..Filters::default()
                },
                10,
                0,
            )
            .expect("search");
        assert_eq!(paths(&hits), vec!["people/anna.md"], "the tag narrows");

        let (hits, _) = s
            .search(
                "baut",
                &Filters {
                    folder: Some("people".to_string()),
                    ..Filters::default()
                },
                10,
                0,
            )
            .expect("search");
        assert_eq!(hits.len(), 2, "the folder narrows to its own documents");
    }

    /// An edit must not leave the old text findable, and a deletion must
    /// not leave a hit at all - tantivy has no update, only delete + add.
    #[test]
    fn an_edit_replaces_the_document_and_a_deletion_removes_it() {
        let mut t = tree(&[("a.md", "# A\nvorher\n"), ("b.md", "# B\nnachbar\n")]);
        let mut s = WikiSearch::build(&t).expect("index");
        assert_eq!(s.search("vorher", &Filters::default(), 10, 0).expect("q").0.len(), 1);

        t.insert("a.md".to_string(), "# A\nnachher\n".to_string());
        s.update(&t, &["a.md".to_string()].into_iter().collect())
            .expect("update");
        assert!(
            s.search("vorher", &Filters::default(), 10, 0).expect("q").0.is_empty(),
            "the old text is gone, not duplicated"
        );
        assert_eq!(s.search("nachher", &Filters::default(), 10, 0).expect("q").0.len(), 1);

        t.remove("a.md");
        s.update(&t, &["a.md".to_string()].into_iter().collect())
            .expect("update");
        assert!(s.search("nachher", &Filters::default(), 10, 0).expect("q").0.is_empty());
        assert_eq!(s.search("nachbar", &Filters::default(), 10, 0).expect("q").0.len(), 1);
    }

    /// Paging: the caller learns there is more without a second search.
    #[test]
    fn paging_reports_whether_a_next_page_exists() {
        let docs: Vec<(String, String)> = (0..5)
            .map(|i| (format!("d{i}.md"), format!("# D{i}\ngemeinsam\n")))
            .collect();
        let t: BTreeMap<String, String> = docs.into_iter().collect();
        let s = WikiSearch::build(&t).expect("index");
        let (hits, more) = s.search("gemeinsam", &Filters::default(), 2, 0).expect("q");
        assert_eq!(hits.len(), 2);
        assert!(more);
        let (hits, more) = s.search("gemeinsam", &Filters::default(), 2, 4).expect("q");
        assert_eq!(hits.len(), 1);
        assert!(!more, "the last page says so");
    }

    /// An empty query with no filter is not "everything".
    #[test]
    fn an_empty_query_finds_nothing() {
        let s = WikiSearch::build(&tree(&[("a.md", "# A\ntext\n")])).expect("index");
        assert!(s.search("  ", &Filters::default(), 10, 0).expect("q").0.is_empty());
    }
}
