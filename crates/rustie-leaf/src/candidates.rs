//! Candidate documents of one segment: the compiler's [`CandidateFilter`] (a superset of the
//! matching sentences) evaluated on postings.

use std::sync::LazyLock;

use rustie_compiler::matching::node_test::anchor;
use rustie_compiler::{CandidateFilter, LeafTest};
use tantivy::postings::{Postings, SegmentPostings};
use tantivy::schema::{IndexRecordOption, Schema};
use tantivy::{DocId, DocSet, SegmentReader, TERMINATED};

use crate::expand::expand_terms;

/// A set of document ids of one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocBits {
    words: Vec<u64>,
    max_doc: u32,
}

impl DocBits {
    fn empty(max_doc: u32) -> Self {
        Self {
            words: vec![0; (max_doc as usize).div_ceil(64)],
            max_doc,
        }
    }

    fn full(max_doc: u32) -> Self {
        let mut bits = Self::empty(max_doc);
        for doc in 0..max_doc {
            bits.insert(doc);
        }
        bits
    }

    fn insert(&mut self, doc: DocId) {
        self.words[doc as usize / 64] |= 1 << (doc % 64);
    }

    fn and(&mut self, other: &Self) {
        self.words
            .iter_mut()
            .zip(&other.words)
            .for_each(|(a, b)| *a &= b);
    }

    fn or(&mut self, other: &Self) {
        self.words
            .iter_mut()
            .zip(&other.words)
            .for_each(|(a, b)| *a |= b);
    }

    pub(crate) fn docs(&self) -> Vec<DocId> {
        let mut out = Vec::new();
        for (w, &word) in self.words.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let bit = bits.trailing_zeros();
                out.push(w as u32 * 64 + bit);
                bits &= bits - 1;
            }
        }
        out.retain(|&doc| doc < self.max_doc);
        out
    }
}

/// Documents of `reader` that `filter` admits. Warms (and reads) the postings it needs.
pub(crate) fn candidate_docs<'a>(
    filter: &'a CandidateFilter,
    reader: &'a SegmentReader,
    schema: &'a Schema,
) -> futures::future::BoxFuture<'a, anyhow::Result<DocBits>> {
    Box::pin(async move {
        let max_doc = reader.max_doc();
        Ok(match filter {
            CandidateFilter::All => DocBits::full(max_doc),
            CandidateFilter::Term { field, value } => {
                docs_of_test(field, &LeafTest::Exact(value.clone()), reader, schema).await?
            }
            CandidateFilter::Regex { field, pattern } => {
                let regex = regex::Regex::new(pattern)
                    .map_err(|err| anyhow::anyhow!("invalid regex `{pattern}`: {err}"))?;
                docs_of_test(field, &LeafTest::Regex(anchor(&regex)), reader, schema).await?
            }
            // Adjacency is verified by the exact matcher; here every term must be present.
            CandidateFilter::Phrase { field, terms } => {
                let mut acc = DocBits::full(max_doc);
                for value in terms {
                    let docs = docs_of_test(field, &LeafTest::Exact(value.clone()), reader, schema)
                        .await?;
                    acc.and(&docs);
                }
                acc
            }
            CandidateFilter::And(parts) => {
                let mut acc = DocBits::full(max_doc);
                for part in parts {
                    acc.and(&candidate_docs(part, reader, schema).await?);
                }
                acc
            }
            CandidateFilter::Or(parts) => {
                let mut acc = DocBits::empty(max_doc);
                for part in parts {
                    acc.or(&candidate_docs(part, reader, schema).await?);
                }
                acc
            }
            // Document-level: every child must occur somewhere in the sentence, same as `And`.
            // That every child holds on the *same* token is checked later, over postings
            // positions, by `refine_same_token` — a further (sound) narrowing of this same set.
            CandidateFilter::SameToken(parts) => {
                let mut acc = DocBits::full(max_doc);
                for part in parts {
                    acc.and(&candidate_docs(part, reader, schema).await?);
                }
                acc
            }
        })
    })
}

/// Above this doc-frequency ratio, a `SameToken` node's rarest child is considered too common
/// to be worth refining, and the node is skipped (the doc-level filter, which already required
/// it, stays as-is). Default 1.0 = never skip: on PubMed (5.1M sentences, 10 graph queries) it
/// read the fewest GPH2 blocks and had the lowest total latency. Lower ratios skipped nodes
/// like `{tag:NN, outgoing:nsubj}`, whose children are each common but rarely on one token, so
/// the rarest child's frequency is a poor predictor of what refinement saves.
fn refine_max_df_ratio() -> f64 {
    static RATIO: LazyLock<f64> = LazyLock::new(|| {
        std::env::var("RUSTIE_REFINE_MAX_DF_RATIO")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0)
    });
    *RATIO
}

/// One child of a `SameToken` node: every expanded term's postings (opened with positions), and
/// their summed doc frequency (an upper bound on how many documents this child alone admits,
/// used both for the cost guard and to check the rarest child first).
struct SameTokenChild {
    postings: Vec<SegmentPostings>,
    doc_freq: u64,
}

/// `filter`'s `Term`/`Regex` translated to a field + [`LeafTest`], the same conversion
/// `candidate_docs` applies inline for its own `Term`/`Regex` arms.
fn field_and_test(filter: &CandidateFilter) -> anyhow::Result<(&str, LeafTest)> {
    match filter {
        CandidateFilter::Term { field, value } => Ok((field, LeafTest::Exact(value.clone()))),
        CandidateFilter::Regex { field, pattern } => {
            let regex = regex::Regex::new(pattern)
                .map_err(|err| anyhow::anyhow!("invalid regex `{pattern}`: {err}"))?;
            Ok((field, LeafTest::Regex(anchor(&regex))))
        }
        other => anyhow::bail!("SameToken child must be Term or Regex, got {other:?}"),
    }
}

/// Opens `child`'s postings with positions, or `None` when this split can't answer a same-token
/// check for it: the field is absent (an old split, or one without this field at all) or was
/// indexed without positions (`record: basic`, the pre-`rustie_edges` edge-field mapping).
async fn prepare_same_token_child(
    filter: &CandidateFilter,
    reader: &SegmentReader,
    schema: &Schema,
) -> anyhow::Result<Option<SameTokenChild>> {
    let (field_name, test) = field_and_test(filter)?;
    let Ok(field) = schema.get_field(field_name) else {
        return Ok(None);
    };
    let entry = schema.get_field_entry(field);
    if !entry.is_indexed() || !entry.field_type().get_index_record_option().is_some_and(|o| o.has_positions()) {
        return Ok(None);
    }
    let inverted_index = reader.inverted_index(field)?;
    let terms = expand_terms(&inverted_index, field, &test, true).await?;
    let mut postings = Vec::with_capacity(terms.len());
    let mut doc_freq: u64 = 0;
    for term in &terms {
        if let Some(p) = inverted_index.read_postings(term, IndexRecordOption::WithFreqsAndPositions)? {
            doc_freq += u64::from(p.doc_freq());
            postings.push(p);
        }
    }
    Ok(Some(SameTokenChild { postings, doc_freq }))
}

/// This child's token positions in `doc` (the union over all its expanded terms), or `false` if
/// it has none there. `buf` is cleared and reused across docs to avoid reallocating.
fn child_positions_for_doc(child: &mut SameTokenChild, doc: DocId, buf: &mut Vec<u32>) -> bool {
    buf.clear();
    let mut term_positions = Vec::new();
    for postings in &mut child.postings {
        if postings.doc() < doc {
            postings.seek(doc);
        }
        if postings.doc() == doc {
            postings.positions(&mut term_positions);
            buf.extend_from_slice(&term_positions);
        }
    }
    if buf.is_empty() {
        return false;
    }
    buf.sort_unstable();
    buf.dedup();
    true
}

/// `acc` becomes `acc ∩ other`, both sorted and deduplicated.
fn intersect_sorted(acc: &mut Vec<u32>, other: &[u32]) {
    let mut i = 0;
    let mut j = 0;
    let mut out = Vec::with_capacity(acc.len().min(other.len()));
    while i < acc.len() && j < other.len() {
        match acc[i].cmp(&other[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(acc[i]);
                i += 1;
                j += 1;
            }
        }
    }
    *acc = out;
}

/// Whether some one token of `doc` satisfies every child of `children` (rarest-first: the first
/// child with nothing at `doc` fails the whole node without touching the rest).
fn doc_has_same_token(children: &mut [SameTokenChild], doc: DocId, buf: &mut Vec<u32>, acc: &mut Vec<u32>) -> bool {
    for (i, child) in children.iter_mut().enumerate() {
        if !child_positions_for_doc(child, doc, buf) {
            return false;
        }
        if i == 0 {
            acc.clone_from(buf);
        } else {
            intersect_sorted(acc, buf);
            if acc.is_empty() {
                return false;
            }
        }
    }
    !acc.is_empty()
}

/// The `SameToken` nodes to refine on: `filter` itself, or its top-level `And` conjuncts. A
/// `SameToken` nested under `Or` is left alone — some other branch of the `Or` might hold
/// instead, so narrowing on this one would be unsound.
fn top_level_same_tokens(filter: &CandidateFilter) -> Vec<&Vec<CandidateFilter>> {
    match filter {
        CandidateFilter::SameToken(children) => vec![children],
        CandidateFilter::And(parts) => parts
            .iter()
            .filter_map(|p| match p {
                CandidateFilter::SameToken(children) => Some(children),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Narrows `docs` (already a document-level superset from [`candidate_docs`]) to the documents
/// where one *token* satisfies every child of some top-level `SameToken` node, checked directly
/// on postings positions — before any GPH2 block is read. Still a superset: `And`/`Or` conjuncts
/// other than a `SameToken`, and anything nested under `Or`, are left to the exact matcher.
pub(crate) async fn refine_same_token(
    filter: &CandidateFilter,
    reader: &SegmentReader,
    schema: &Schema,
    docs: Vec<DocId>,
) -> anyhow::Result<Vec<DocId>> {
    refine_same_token_with(filter, reader, schema, docs, refine_max_df_ratio()).await
}

/// [`refine_same_token`] with an explicit cost-guard threshold (tests pass their own, instead of
/// racing on the process-wide `RUSTIE_REFINE_MAX_DF_RATIO`).
async fn refine_same_token_with(
    filter: &CandidateFilter,
    reader: &SegmentReader,
    schema: &Schema,
    mut docs: Vec<DocId>,
    max_df_ratio: f64,
) -> anyhow::Result<Vec<DocId>> {
    let nodes = top_level_same_tokens(filter);
    let max_doc = reader.max_doc() as f64;
    for children in nodes {
        if docs.is_empty() {
            break;
        }
        let mut prepared = Vec::with_capacity(children.len());
        let mut skip = false;
        for child in children {
            match prepare_same_token_child(child, reader, schema).await? {
                Some(c) => prepared.push(c),
                None => {
                    skip = true;
                    break;
                }
            }
        }
        if skip {
            continue;
        }
        prepared.sort_by_key(|c| c.doc_freq);
        let rarest_ratio = prepared.first().map_or(0.0, |c| c.doc_freq as f64 / max_doc.max(1.0));
        if rarest_ratio > max_df_ratio {
            tracing::debug!(
                rarest_ratio,
                threshold = max_df_ratio,
                "rustie same-token refine: node skipped (rarest child too common)"
            );
            continue;
        }
        let mut buf = Vec::new();
        let mut acc = Vec::new();
        docs.retain(|&doc| doc_has_same_token(&mut prepared, doc, &mut buf, &mut acc));
    }
    Ok(docs)
}

async fn docs_of_test(
    field_name: &str,
    test: &LeafTest,
    reader: &SegmentReader,
    schema: &Schema,
) -> anyhow::Result<DocBits> {
    let mut bits = DocBits::empty(reader.max_doc());
    // A field the split does not have holds no term: nothing matches.
    let Ok(field) = schema.get_field(field_name) else {
        return Ok(bits);
    };
    if !schema.get_field_entry(field).is_indexed() {
        return Ok(bits);
    }
    let inverted_index = reader.inverted_index(field)?;
    for term in expand_terms(&inverted_index, field, test, false).await? {
        if let Some(mut postings) = inverted_index.read_postings(&term, IndexRecordOption::Basic)? {
            while postings.doc() != TERMINATED {
                bits.insert(postings.doc());
                postings.advance();
            }
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_operations() {
        let mut a = DocBits::empty(130);
        for d in [0, 64, 129] {
            a.insert(d);
        }
        let mut b = DocBits::empty(130);
        for d in [64, 100] {
            b.insert(d);
        }
        let mut and = a.clone();
        and.and(&b);
        assert_eq!(and.docs(), [64]);
        let mut or = a.clone();
        or.or(&b);
        assert_eq!(or.docs(), [0, 64, 100, 129]);
        assert_eq!(DocBits::full(3).docs(), [0, 1, 2]);
    }

    #[test]
    fn intersect_sorted_keeps_common_positions() {
        let mut acc = vec![0, 2, 5, 9];
        intersect_sorted(&mut acc, &[1, 2, 9, 12]);
        assert_eq!(acc, [2, 9]);
        intersect_sorted(&mut acc, &[3]);
        assert!(acc.is_empty());
    }

    use tantivy::schema::{TextFieldIndexing, TextOptions};
    use tantivy::{Index, IndexReader, doc};

    /// A one-segment in-RAM index with the `rustie_tokens` / `rustie_edges` tokenizers:
    /// `tag` and `incoming_edges` indexed with positions, `basic_edges` indexed like the
    /// pre-`rustie_edges` edge fields (`record: basic`, no positions).
    fn slot_index(docs: &[(&str, &str)]) -> (Index, IndexReader) {
        let positional = |tokenizer: &str, option| {
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(tokenizer)
                    .set_index_option(option),
            )
        };
        let mut builder = Schema::builder();
        let tag = builder.add_text_field(
            "tag",
            positional("rustie_tokens", IndexRecordOption::WithFreqsAndPositions),
        );
        let incoming = builder.add_text_field(
            "incoming_edges",
            positional("rustie_edges", IndexRecordOption::WithFreqsAndPositions),
        );
        let basic = builder.add_text_field(
            "basic_edges",
            positional("rustie_edges", IndexRecordOption::Basic),
        );
        let index = Index::create_in_ram(builder.build());
        index
            .tokenizers()
            .register("rustie_tokens", crate::tokenizer::slot_text_analyzer(false));
        index
            .tokenizers()
            .register("rustie_edges", crate::tokenizer::slot_text_analyzer(true));
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for (tags, edges) in docs {
            writer
                .add_document(doc!(tag => *tags, incoming => *edges, basic => *edges))
                .unwrap();
        }
        writer.commit().unwrap();
        let reader = index.reader().unwrap();
        (index, reader)
    }

    /// doc 0: `NNS` + `nsubj` on token 1 (same token). doc 1: `NN`/`NNS` on tokens 0-1, `nsubj`
    /// only on token 2 (`DT`). doc 2: `NN` + `nsubj` on token 0.
    const DOCS: [(&str, &str); 3] = [
        ("NN|NNS|DT", "|nsubj|"),
        ("NNS|NN|DT", "||nsubj"),
        ("NN|DT", "nsubj|"),
    ];

    async fn refine(filter: &CandidateFilter, ratio: f64) -> Vec<DocId> {
        let (index, reader) = slot_index(&DOCS);
        let searcher = reader.searcher();
        refine_same_token_with(filter, searcher.segment_reader(0), &index.schema(), vec![0, 1, 2], ratio)
            .await
            .unwrap()
    }

    fn tag_nn_with_nsubj(edge_field: &str) -> CandidateFilter {
        CandidateFilter::SameToken(vec![
            CandidateFilter::regex("tag", "NN.*"),
            CandidateFilter::term(edge_field, "nsubj"),
        ])
    }

    #[tokio::test]
    async fn regex_child_is_a_union_of_its_terms() {
        // `tag=/NN.*/` expands to `NN` and `NNS`. Doc 0's matching token is `NNS` (never `NN`):
        // the child's positions must be NN ∪ NNS, then intersected with `nsubj`'s. Intersecting
        // term by term (NN ∩ NNS ∩ nsubj) would be empty on every doc and drop 0 and 2.
        assert_eq!(refine(&tag_nn_with_nsubj("incoming_edges"), 1.0).await, [0, 2]);
    }

    #[tokio::test]
    async fn only_top_level_same_tokens_narrow() {
        let node = tag_nn_with_nsubj("incoming_edges");
        let conjunct = CandidateFilter::And(vec![node.clone(), CandidateFilter::term("tag", "DT")]);
        assert_eq!(refine(&conjunct, 1.0).await, [0, 2]);
        // Under an `Or`, another branch may hold instead: left alone.
        let disjunct = CandidateFilter::Or(vec![node, CandidateFilter::term("tag", "DT")]);
        assert_eq!(refine(&disjunct, 1.0).await, [0, 1, 2]);
    }

    #[tokio::test]
    async fn field_without_positions_skips_the_node() {
        // An old split's edge field (`record: basic`) cannot answer a same-token check.
        assert_eq!(refine(&tag_nn_with_nsubj("basic_edges"), 1.0).await, [0, 1, 2]);
    }

    #[tokio::test]
    async fn cost_guard_skips_a_node_whose_rarest_child_is_too_common() {
        // With the threshold at 0 no node is worth refining: candidates are left unchanged.
        assert_eq!(refine(&tag_nn_with_nsubj("incoming_edges"), 0.0).await, [0, 1, 2]);
    }
}
