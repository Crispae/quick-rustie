//! Candidate documents of one segment: the compiler's [`CandidateFilter`] (a superset of the
//! matching sentences) evaluated on postings.

use std::sync::{Arc, LazyLock};

use rustie_compiler::matching::node_test::anchor;
use rustie_compiler::{CandidateFilter, LeafTest};
use tantivy::postings::{Postings, SegmentPostings};
use tantivy::schema::{Field, IndexRecordOption, Schema};
use tantivy::{DocId, DocSet, InvertedIndexReader, SegmentReader, TERMINATED};

use crate::expand::{Resolved, SegmentTerms};

/// A set of document ids of one segment, as a bitmap: the union of an `Or`, or of a common
/// leaf's postings when that is cheaper to probe than seeking.
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

    fn insert(&mut self, doc: DocId) {
        self.words[doc as usize / 64] |= 1 << (doc % 64);
    }

    fn contains(&self, doc: DocId) -> bool {
        self.words
            .get(doc as usize / 64)
            .is_some_and(|word| word & (1 << (doc % 64)) != 0)
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

/// What [`candidate_docs`] admits: every document, or an ascending list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidates {
    max_doc: u32,
    /// `None` = every document of the segment (nothing constrains it).
    docs: Option<Vec<DocId>>,
}

impl Candidates {
    pub(crate) fn docs(self) -> Vec<DocId> {
        self.docs.unwrap_or_else(|| (0..self.max_doc).collect())
    }

    fn none(max_doc: u32) -> Self {
        Self {
            max_doc,
            docs: Some(Vec::new()),
        }
    }
}

/// Documents of `reader` that `filter` admits. Warms (and reads) the postings it needs, through
/// `terms` so a `(field, test)` pair resolved here is reused by `refine_same_token` and the
/// external-leaf resolution in `query.rs`, instead of walking the dictionary again.
///
/// A conjunction (`And`, `Phrase`, a document-level `SameToken`) is planned by selectivity: all
/// its leaves are resolved concurrently (dictionary only), then intersected rarest first, each
/// further leaf probed only on the surviving documents, stopping as soon as nothing survives —
/// at which point the remaining leaves' postings are never read.
pub(crate) fn candidate_docs<'a>(
    filter: &'a CandidateFilter,
    terms: &'a SegmentTerms<'a>,
    reader: &'a SegmentReader,
    schema: &'a Schema,
) -> futures::future::BoxFuture<'a, anyhow::Result<Candidates>> {
    Box::pin(async move {
        let max_doc = reader.max_doc();
        match filter {
            CandidateFilter::All => Ok(Candidates {
                max_doc,
                docs: None,
            }),
            CandidateFilter::Or(parts) => {
                let mut acc = DocBits::empty(max_doc);
                for part in parts {
                    match candidate_docs(part, terms, reader, schema).await?.docs {
                        None => {
                            return Ok(Candidates {
                                max_doc,
                                docs: None,
                            });
                        }
                        Some(docs) => docs.into_iter().for_each(|doc| acc.insert(doc)),
                    }
                }
                Ok(Candidates {
                    max_doc,
                    docs: Some(acc.docs()),
                })
            }
            conjunction => {
                let mut leaves = Vec::new();
                let mut nested = Vec::new();
                flatten_conjunction(conjunction, &mut leaves, &mut nested)?;
                intersect(&leaves, &nested, terms, reader, schema).await
            }
        }
    })
}

/// `filter`'s conjuncts: the token tests every admitted document contains (`leaves`), and the
/// sub-filters it must also satisfy that are not single tests (`nested`, e.g. an `Or`).
fn flatten_conjunction<'f>(
    filter: &'f CandidateFilter,
    leaves: &mut Vec<(&'f str, LeafTest)>,
    nested: &mut Vec<&'f CandidateFilter>,
) -> anyhow::Result<()> {
    match filter {
        CandidateFilter::All => {}
        CandidateFilter::Term { .. } | CandidateFilter::Regex { .. } => {
            leaves.push(field_and_test(filter)?)
        }
        // Adjacency is verified by the exact matcher; here every term must be present.
        CandidateFilter::Phrase { field, terms } => leaves.extend(
            terms
                .iter()
                .map(|value| (field.as_str(), LeafTest::Exact(value.clone()))),
        ),
        // Document-level: every child must occur somewhere in the sentence, same as `And`.
        // That every child holds on the *same* token is checked later, over postings
        // positions, by `refine_same_token` — a further (sound) narrowing of this same set.
        CandidateFilter::And(parts) | CandidateFilter::SameToken(parts) => {
            for part in parts {
                flatten_conjunction(part, leaves, nested)?;
            }
        }
        CandidateFilter::Or(_) => nested.push(filter),
    }
    Ok(())
}

/// A leaf's field and resolution, or `None` when the split cannot hold it (field absent or not
/// indexed): then nothing matches.
async fn resolve_leaf(
    field_name: &str,
    test: &LeafTest,
    terms: &SegmentTerms<'_>,
    schema: &Schema,
) -> anyhow::Result<Option<(Field, Arc<Resolved>)>> {
    let Ok(field) = schema.get_field(field_name) else {
        return Ok(None);
    };
    if !schema.get_field_entry(field).is_indexed() {
        return Ok(None);
    }
    Ok(Some((field, terms.resolve(field, test).await?)))
}

async fn intersect(
    leaves: &[(&str, LeafTest)],
    nested: &[&CandidateFilter],
    terms: &SegmentTerms<'_>,
    reader: &SegmentReader,
    schema: &Schema,
) -> anyhow::Result<Candidates> {
    let max_doc = reader.max_doc();
    // Dictionary lookups only, all at once: on object storage each is a round trip.
    let resolved = futures::future::try_join_all(
        leaves
            .iter()
            .map(|(field, test)| resolve_leaf(field, test, terms, schema)),
    )
    .await?;
    let Some(mut resolved) = resolved.into_iter().collect::<Option<Vec<_>>>() else {
        return Ok(Candidates::none(max_doc));
    };
    resolved.sort_by_key(|(_, r)| r.doc_freq());
    if resolved.first().is_some_and(|(_, r)| r.doc_freq() == 0) {
        return Ok(Candidates::none(max_doc));
    }

    let mut acc: Option<Vec<DocId>> = None;
    for (field, leaf) in &resolved {
        if acc.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Candidates::none(max_doc));
        }
        terms.warm_postings(*field, leaf).await?;
        let inverted_index = reader.inverted_index(*field)?;
        match &mut acc {
            None => acc = Some(leaf_docs(&inverted_index, leaf)?),
            Some(docs) => retain_in_leaf(docs, &inverted_index, leaf, max_doc)?,
        }
    }
    for part in nested {
        if acc.as_ref().is_some_and(Vec::is_empty) {
            break;
        }
        let Some(part_docs) = candidate_docs(part, terms, reader, schema).await?.docs else {
            continue;
        };
        match &mut acc {
            None => acc = Some(part_docs),
            Some(docs) => intersect_sorted(docs, &part_docs),
        }
    }
    Ok(Candidates { max_doc, docs: acc })
}

/// Every document holding one of `leaf`'s terms, ascending (postings must be warm).
fn leaf_docs(inverted_index: &InvertedIndexReader, leaf: &Resolved) -> anyhow::Result<Vec<DocId>> {
    let mut out = Vec::with_capacity(leaf.doc_freq() as usize);
    for term in leaf.terms() {
        if let Some(mut postings) = inverted_index.read_postings(term, IndexRecordOption::Basic)? {
            while postings.doc() != TERMINATED {
                out.push(postings.doc());
                postings.advance();
            }
        }
    }
    if leaf.len() > 1 {
        out.sort_unstable();
        out.dedup();
    }
    Ok(out)
}

/// A seek into postings decodes (at least) the 128-doc block it lands in, so it only beats
/// decoding the whole list when candidates are sparse enough to skip blocks: on average more
/// than a block's worth of postings between consecutive candidates.
const SEEK_COST_IN_POSTINGS: u64 = 128;

/// Keeps the documents of `docs` holding one of `leaf`'s terms (postings must be warm). Seeks
/// each term's postings to each candidate when candidates are sparse against the leaf (see
/// [`SEEK_COST_IN_POSTINGS`]), otherwise decodes the leaf into a bitmap and probes that.
fn retain_in_leaf(
    docs: &mut Vec<DocId>,
    inverted_index: &InvertedIndexReader,
    leaf: &Resolved,
    max_doc: u32,
) -> anyhow::Result<()> {
    let seek_cost = (docs.len() as u64)
        .saturating_mul(leaf.len() as u64)
        .saturating_mul(SEEK_COST_IN_POSTINGS);
    if seek_cost <= leaf.doc_freq() {
        let mut postings = Vec::with_capacity(leaf.len());
        for term in leaf.terms() {
            if let Some(p) = inverted_index.read_postings(term, IndexRecordOption::Basic)? {
                postings.push(p);
            }
        }
        docs.retain(|&doc| postings.iter_mut().any(|p| seek_to(p, doc)));
    } else {
        let mut bits = DocBits::empty(max_doc);
        for term in leaf.terms() {
            if let Some(mut p) = inverted_index.read_postings(term, IndexRecordOption::Basic)? {
                while p.doc() != TERMINATED {
                    bits.insert(p.doc());
                    p.advance();
                }
            }
        }
        docs.retain(|&doc| bits.contains(doc));
    }
    Ok(())
}

/// Moves `postings` forward to `doc` (targets must ascend), and whether it holds `doc`.
fn seek_to(postings: &mut SegmentPostings, doc: DocId) -> bool {
    if postings.doc() < doc {
        postings.seek(doc);
    }
    postings.doc() == doc
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

/// One child of a `SameToken` node, positions warmed and postings opened: every expanded term's
/// postings. Built only after the cost guard passes (see [`refine_same_token_with`]), so a
/// skipped node never warms a position it won't use.
struct SameTokenChild {
    postings: Vec<SegmentPostings>,
}

/// A `SameToken` child resolved but not yet warmed for positions: its field and its dictionary
/// resolution (postings warm, `doc_freq` available with no further I/O).
struct PreparedChild {
    field: Field,
    resolved: Arc<Resolved>,
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

/// Resolves `child`'s terms (postings warm, no positions yet), or `None` when this split can't
/// answer a same-token check for it: the field is absent (an old split, or one without this field
/// at all) or was indexed without positions (`record: basic`, the pre-`rustie_edges` edge-field
/// mapping).
async fn prepare_same_token_child(
    filter: &CandidateFilter,
    terms: &SegmentTerms<'_>,
    schema: &Schema,
) -> anyhow::Result<Option<PreparedChild>> {
    let (field_name, test) = field_and_test(filter)?;
    let Ok(field) = schema.get_field(field_name) else {
        return Ok(None);
    };
    let entry = schema.get_field_entry(field);
    if !entry.is_indexed()
        || !entry
            .field_type()
            .get_index_record_option()
            .is_some_and(|o| o.has_positions())
    {
        return Ok(None);
    }
    let resolved = terms.resolve(field, &test).await?;
    Ok(Some(PreparedChild { field, resolved }))
}

/// `child`'s token positions in `doc` (the union over all its expanded terms), ascending and
/// deduplicated, into `out`. Its postings must already be at or before `doc`.
fn child_positions_for_doc(
    child: &mut SameTokenChild,
    doc: DocId,
    term_positions: &mut Vec<u32>,
    out: &mut Vec<u32>,
) {
    out.clear();
    for postings in &mut child.postings {
        if seek_to(postings, doc) {
            postings.positions(term_positions);
            out.extend_from_slice(term_positions);
        }
    }
    // One term's positions are already ascending and distinct.
    if child.postings.len() > 1 {
        out.sort_unstable();
        out.dedup();
    }
}

/// `acc` becomes `acc ∩ other` in place, both sorted and deduplicated.
fn intersect_sorted(acc: &mut Vec<u32>, other: &[u32]) {
    let (mut i, mut j, mut kept) = (0, 0, 0);
    while i < acc.len() && j < other.len() {
        match acc[i].cmp(&other[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                acc[kept] = acc[i];
                kept += 1;
                i += 1;
                j += 1;
            }
        }
    }
    acc.truncate(kept);
}

/// Reused across the documents of one refinement pass.
#[derive(Default)]
struct PositionScratch {
    term_positions: Vec<u32>,
    child: Vec<u32>,
    acc: Vec<u32>,
}

/// Whether some one token of `doc` satisfies every child of `children` (rarest first: the first
/// child whose positions don't meet the others' fails the node without decoding the rest).
fn doc_has_same_token(
    children: &mut [SameTokenChild],
    doc: DocId,
    scratch: &mut PositionScratch,
) -> bool {
    for (i, child) in children.iter_mut().enumerate() {
        child_positions_for_doc(child, doc, &mut scratch.term_positions, &mut scratch.child);
        if i == 0 {
            std::mem::swap(&mut scratch.acc, &mut scratch.child);
        } else {
            intersect_sorted(&mut scratch.acc, &scratch.child);
        }
        if scratch.acc.is_empty() {
            return false;
        }
    }
    true
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
    terms: &SegmentTerms<'_>,
    reader: &SegmentReader,
    schema: &Schema,
    docs: Vec<DocId>,
) -> anyhow::Result<Vec<DocId>> {
    refine_same_token_with(filter, terms, reader, schema, docs, refine_max_df_ratio()).await
}

/// [`refine_same_token`] with an explicit cost-guard threshold (tests pass their own, instead of
/// racing on the process-wide `RUSTIE_REFINE_MAX_DF_RATIO`).
async fn refine_same_token_with(
    filter: &CandidateFilter,
    terms: &SegmentTerms<'_>,
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
        // Phase 1: resolve every child (dictionary walk + postings warm only, no positions).
        let mut prepared = Vec::with_capacity(children.len());
        let mut skip = false;
        for child in children {
            match prepare_same_token_child(child, terms, schema).await? {
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
        // Phase 2: the cost guard, from each child's already-local `doc_freq` — before warming
        // any position.
        prepared.sort_by_key(|c| c.resolved.doc_freq());
        let rarest_ratio = prepared
            .first()
            .map_or(0.0, |c| c.resolved.doc_freq() as f64 / max_doc.max(1.0));
        if rarest_ratio > max_df_ratio {
            tracing::debug!(
                rarest_ratio,
                threshold = max_df_ratio,
                "rustie same-token refine: node skipped (rarest child too common)"
            );
            continue;
        }
        // Phase 3: now that the node is worth it, warm positions and open postings.
        let mut same_token_children = Vec::with_capacity(prepared.len());
        for child in &prepared {
            terms.warm_positions(child.field, &child.resolved).await?;
            let inverted_index = reader.inverted_index(child.field)?;
            let mut postings = Vec::with_capacity(child.resolved.len());
            for term in child.resolved.terms() {
                if let Some(p) =
                    inverted_index.read_postings(term, IndexRecordOption::WithFreqsAndPositions)?
                {
                    postings.push(p);
                }
            }
            same_token_children.push(SameTokenChild { postings });
        }
        let mut scratch = PositionScratch::default();
        docs.retain(|&doc| doc_has_same_token(&mut same_token_children, doc, &mut scratch));
    }
    Ok(docs)
}

#[cfg(test)]
#[path = "../test/unit/candidates.rs"]
mod tests;
