//! The `rustie` query extension: a RustIE pattern evaluated inside each split.
//!
//! Per split, the extension warmup (asynchronous, after Quickwit's own warmup):
//! 1. evaluates the compiler's candidate filter on postings;
//! 2. for a graph pattern, opens the split's GPH2 file and binds the plan to its dictionaries;
//! 3. expands every token test answered by postings to its terms, warming their positions;
//! 4. fetches the sentence lengths (token patterns) or the graph blocks holding the candidates
//!    (graph patterns).
//!
//! The scorer (synchronous, on that local data) then walks the candidates and yields only the
//! documents the pattern matches exactly. When the postings already decide the match (one token
//! test, or alternatives of them), steps 3 and 4 are skipped and the candidates are the answer.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use lru::LruCache;
use quickwit_extensions::{ExtensionQueryBuild, ExtensionWarmup, QueryExtension};
use quickwit_query::query_ast::{CacheNode, ExtensionQuery, QueryAst};
use rustie_compiler::{
    BoundPlan, BoundSurface, CandidateFilter, CompiledQuery, EvalScratch, LeafSource,
    QueryCompiler, TokenSet,
};
use rustie_graph_store::reader::Gph2Block;
use rustie_graph_store::{BLOCK_DOCS, SentenceScratch};
use serde_json::Value as JsonValue;
use tracing::debug;
use tantivy::columnar::Column;
use tantivy::directory::Directory;
use tantivy::index::SegmentId;
use tantivy::postings::{Postings, SegmentPostings};
use tantivy::query::{EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::schema::{IndexRecordOption, Schema};
use tantivy::{DocId, DocSet, Score, Searcher, SegmentReader, TERMINATED, Term};

use crate::blocks::SplitGraph;
use crate::candidates::candidate_docs;
use crate::expand::expand_terms;

/// Field holding each sentence's token count (a fast field in the IE mapping).
const SENTENCE_LENGTH_FIELD: &str = "sentence_length";

/// The payload of a `rustie` extension query.
#[derive(Debug, Clone)]
pub struct PatternPayload {
    pub pattern: String,
}

impl PatternPayload {
    pub fn to_json(&self) -> JsonValue {
        serde_json::json!({ "pattern": self.pattern })
    }

    fn from_json(payload: &JsonValue) -> Result<Self, String> {
        let pattern = payload
            .get("pattern")
            .and_then(JsonValue::as_str)
            .ok_or("payload must be {\"pattern\": <string>}")?;
        Ok(Self {
            pattern: pattern.to_string(),
        })
    }
}

/// The `rustie` extension query for `pattern`, wrapped in Quickwit's predicate cache
/// (`quickwit_query::query_ast::CacheNode`). Every split's leaf search consults
/// `SearcherContext::predicate_cache` keyed on `(split_id, json(inner_ast))`: a hit replays a
/// previously-computed, fully-verified match set (`RustieScorer`'s `DocSet` already yields exact
/// matches, post graph verification, not just a postings candidate set) without opening GPH2,
/// evaluating candidates, or running graph scoring again; a miss runs the extension normally and
/// fills the cache.
///
/// Applied unconditionally, matching RustIE's other always-on caches (the compiled-query LRU,
/// the GPH2 trailer/block LRUs). Tradeoff: on a miss, Quickwit's `CacheFillerWeight` drains the
/// inner scorer to completion before returning anything to the collector, so the first run of a
/// new pattern loses whatever early-stop a doc-id-ordered page would normally get — only repeats
/// of an identical pattern against an unchanged split become nearly free.
pub fn cache_wrapped_query_ast(pattern: &str) -> QueryAst {
    QueryAst::Cache(CacheNode::new(QueryAst::Extension(ExtensionQuery {
        kind: crate::QUERY_KIND.to_string(),
        payload: PatternPayload {
            pattern: pattern.to_string(),
        }
        .to_json(),
    })))
}

/// A compiled pattern, shared by every split a query touches.
struct Compiled {
    compiled: CompiledQuery,
    /// The candidate filter is the answer (see `SurfacePlan::exact`): no per-sentence check.
    exact: bool,
    /// Bound once: token patterns do not depend on the split.
    surface: Option<BoundSurface>,
}

impl Compiled {
    fn candidate(&self) -> &CandidateFilter {
        self.compiled.candidate()
    }
}

/// Registered under [`crate::QUERY_KIND`].
pub struct RustieQueryExtension {
    compiled: Mutex<LruCache<String, Arc<Compiled>>>,
}

impl Default for RustieQueryExtension {
    fn default() -> Self {
        Self {
            compiled: Mutex::new(LruCache::new(NonZeroUsize::new(1024).unwrap())),
        }
    }
}

impl RustieQueryExtension {
    fn compile(&self, pattern: &str) -> Result<Arc<Compiled>, String> {
        if let Some(hit) = self.compiled.lock().expect("poisoned").get(pattern) {
            return Ok(hit.clone());
        }
        let compiled = QueryCompiler::new()
            .compile(pattern)
            .map_err(|err| err.to_string())?;
        let surface = match &compiled {
            CompiledQuery::Surface(plan) => Some(BoundSurface::bind(&plan.pattern)?),
            CompiledQuery::Graph(_) => None,
        };
        let exact = matches!(&compiled, CompiledQuery::Surface(plan) if plan.exact);
        let compiled = Arc::new(Compiled {
            compiled,
            exact,
            surface,
        });
        self.compiled
            .lock()
            .expect("poisoned")
            .put(pattern.to_string(), compiled.clone());
        Ok(compiled)
    }
}

impl QueryExtension for RustieQueryExtension {
    fn kind(&self) -> &str {
        crate::QUERY_KIND
    }

    fn build(&self, payload: &JsonValue, schema: &Schema) -> Result<ExtensionQueryBuild, String> {
        let payload = PatternPayload::from_json(payload)?;
        let compiled = self.compile(&payload.pattern)?;
        let required_terms = required_terms(compiled.candidate(), schema);
        let state = Arc::new(SplitState::default());
        Ok(ExtensionQueryBuild {
            query: Box::new(RustieQuery {
                compiled: compiled.clone(),
                state: state.clone(),
            }),
            warmup: Some(Arc::new(RustieWarmup {
                compiled,
                state,
                schema: schema.clone(),
            })),
            required_terms,
        })
    }
}

/// Terms every match contains: the top-level conjuncts of the candidate filter that are single
/// terms. Quickwit skips a split outright when one of them is absent.
fn required_terms(filter: &CandidateFilter, schema: &Schema) -> Vec<Term> {
    let conjuncts: Vec<&CandidateFilter> = match filter {
        CandidateFilter::And(parts) => parts.iter().collect(),
        other => vec![other],
    };
    conjuncts
        .into_iter()
        .filter_map(|part| match part {
            CandidateFilter::Term { field, value } => {
                let field = schema.get_field(field).ok()?;
                schema
                    .get_field_entry(field)
                    .is_indexed()
                    .then(|| Term::from_field_text(field, value))
            }
            _ => None,
        })
        .collect()
}

/// What the warmup prepared for one segment.
struct SegmentPlan {
    candidates: Vec<DocId>,
    /// Terms answering each external leaf of the bound pattern (same index).
    leaf_terms: Vec<Vec<Term>>,
    graph: Option<GraphSegment>,
}

struct GraphSegment {
    plan: BoundPlan,
    blocks: HashMap<u32, Arc<Gph2Block>>,
}

/// Shared between one split's query and its warmup.
#[derive(Default)]
struct SplitState {
    segments: Mutex<HashMap<SegmentId, Arc<SegmentPlan>>>,
}

struct RustieWarmup {
    compiled: Arc<Compiled>,
    state: Arc<SplitState>,
    schema: Schema,
}

#[async_trait]
impl ExtensionWarmup for RustieWarmup {
    async fn warm(
        &self,
        searcher: &Searcher,
        split_directory: &dyn Directory,
    ) -> anyhow::Result<()> {
        let graph = match &self.compiled.compiled {
            CompiledQuery::Graph(_) => SplitGraph::open(split_directory).await?,
            CompiledQuery::Surface(_) => None,
        };
        for reader in searcher.segment_readers() {
            let plan = self.prepare_segment(reader, graph.as_ref()).await?;
            self.state
                .segments
                .lock()
                .expect("poisoned")
                .insert(reader.segment_id(), Arc::new(plan));
        }
        Ok(())
    }
}

impl RustieWarmup {
    async fn prepare_segment(
        &self,
        reader: &SegmentReader,
        graph: Option<&SplitGraph>,
    ) -> anyhow::Result<SegmentPlan> {
        let started = Instant::now();
        let candidates = candidate_docs(self.compiled.candidate(), reader, &self.schema)
            .await?
            .docs();
        let candidates_took = started.elapsed();
        if self.compiled.exact {
            // Neither positions nor sentence lengths are read.
            return Ok(SegmentPlan {
                candidates,
                leaf_terms: Vec::new(),
                graph: None,
            });
        }
        let (leaves, graph_segment) = match (&self.compiled.compiled, graph) {
            (CompiledQuery::Surface(_), _) => (
                self.compiled
                    .surface
                    .as_ref()
                    .expect("bound at compile time")
                    .external_leaves()
                    .to_vec(),
                None,
            ),
            // A split written before graphs were stored: no sentence can be traversed.
            (CompiledQuery::Graph(_), None) => {
                return Ok(SegmentPlan {
                    candidates: Vec::new(),
                    leaf_terms: Vec::new(),
                    graph: None,
                });
            }
            (CompiledQuery::Graph(compiled), Some(graph)) => {
                let plan = BoundPlan::bind(&compiled.plan, &graph.trailer);
                (plan.external_leaves().to_vec(), Some(plan))
            }
        };
        if candidates.is_empty() {
            return Ok(SegmentPlan {
                candidates,
                leaf_terms: Vec::new(),
                graph: None,
            });
        }

        let mut leaf_terms = Vec::with_capacity(leaves.len());
        for leaf in &leaves {
            let terms = match self.schema.get_field(&leaf.field) {
                Ok(field) if self.schema.get_field_entry(field).is_indexed() => {
                    let inverted_index = reader.inverted_index(field)?;
                    expand_terms(&inverted_index, field, &leaf.test, true).await?
                }
                _ => Vec::new(),
            };
            leaf_terms.push(terms);
        }

        let leaves_took = started.elapsed() - candidates_took;
        let graph = match (graph_segment, graph) {
            (Some(plan), Some(graph)) => Some(GraphSegment {
                plan,
                blocks: graph.blocks_for_docs(&candidates).await?,
            }),
            _ => {
                // Token patterns need each sentence's length.
                warm_fast_field(reader, SENTENCE_LENGTH_FIELD).await?;
                None
            }
        };
        debug!(
            segment = %reader.segment_id().short_uuid_string(),
            candidates = candidates.len(),
            candidates_ms = candidates_took.as_millis() as u64,
            leaves_ms = leaves_took.as_millis() as u64,
            graph_ms = (started.elapsed() - candidates_took - leaves_took).as_millis() as u64,
            "rustie segment warmed"
        );
        Ok(SegmentPlan {
            candidates,
            leaf_terms,
            graph,
        })
    }
}

async fn warm_fast_field(reader: &SegmentReader, name: &str) -> anyhow::Result<()> {
    let columns = reader
        .fast_fields()
        .list_dynamic_column_handles(name)
        .await?;
    anyhow::ensure!(!columns.is_empty(), "split has no `{name}` fast field");
    for column in columns {
        column.file_slice().read_bytes_async().await?;
    }
    Ok(())
}

#[derive(Clone)]
struct RustieQuery {
    compiled: Arc<Compiled>,
    state: Arc<SplitState>,
}

impl fmt::Debug for RustieQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RustieQuery").finish_non_exhaustive()
    }
}

impl Query for RustieQuery {
    fn weight(&self, _: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(self.clone()))
    }
}

impl Weight for RustieQuery {
    fn scorer(&self, reader: &SegmentReader, _boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let plan = self
            .state
            .segments
            .lock()
            .expect("poisoned")
            .get(&reader.segment_id())
            .cloned()
            .ok_or_else(|| {
                tantivy::TantivyError::InternalError(
                    "rustie query executed without its warmup".to_string(),
                )
            })?;
        Ok(Box::new(RustieScorer::new(
            self.compiled.clone(),
            plan,
            reader,
        )?))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if scorer.seek(doc) == doc {
            Ok(Explanation::new("rustie pattern match", 1.0))
        } else {
            Err(tantivy::TantivyError::InvalidArgument(format!(
                "document {doc} does not match"
            )))
        }
    }
}

/// Positions of each external leaf's terms for the current document.
struct PostingsLeafSource<'a> {
    postings: &'a mut [Vec<SegmentPostings>],
    doc: DocId,
    positions: &'a mut Vec<u32>,
}

impl LeafSource for PostingsLeafSource<'_> {
    fn fill(&mut self, leaf: usize, out: &mut TokenSet) {
        let n = out.len();
        for postings in &mut self.postings[leaf] {
            if postings.doc() < self.doc {
                postings.seek(self.doc);
            }
            if postings.doc() == self.doc {
                self.positions.clear();
                postings.positions(self.positions);
                for &pos in self.positions.iter() {
                    if (pos as usize) < n {
                        out.set(pos as usize);
                    }
                }
            }
        }
    }
}

struct RustieScorer {
    compiled: Arc<Compiled>,
    plan: Arc<SegmentPlan>,
    postings: Vec<Vec<SegmentPostings>>,
    lengths: Option<Column<u64>>,
    cursor: usize,
    doc: DocId,
    positions: Vec<u32>,
    eval: EvalScratch,
    sentence: SentenceScratch,
}

impl RustieScorer {
    fn new(
        compiled: Arc<Compiled>,
        plan: Arc<SegmentPlan>,
        reader: &SegmentReader,
    ) -> tantivy::Result<Self> {
        let mut postings = Vec::with_capacity(plan.leaf_terms.len());
        for terms in &plan.leaf_terms {
            let mut leaf_postings = Vec::with_capacity(terms.len());
            for term in terms {
                let inverted_index = reader.inverted_index(term.field())?;
                if let Some(p) =
                    inverted_index.read_postings(term, IndexRecordOption::WithFreqsAndPositions)?
                {
                    leaf_postings.push(p);
                }
            }
            postings.push(leaf_postings);
        }
        let lengths = match plan.graph {
            None if !compiled.exact && !plan.candidates.is_empty() => {
                Some(reader.fast_fields().u64(SENTENCE_LENGTH_FIELD)?)
            }
            _ => None,
        };
        let mut scorer = Self {
            compiled,
            plan,
            postings,
            lengths,
            cursor: 0,
            doc: TERMINATED,
            positions: Vec::new(),
            eval: EvalScratch::default(),
            sentence: SentenceScratch::default(),
        };
        scorer.doc = scorer.next_match();
        Ok(scorer)
    }

    /// The next matching candidate from the cursor on, or `TERMINATED`.
    fn next_match(&mut self) -> DocId {
        while let Some(&doc) = self.plan.candidates.get(self.cursor) {
            self.cursor += 1;
            if self.compiled.exact || self.matches(doc) {
                return doc;
            }
        }
        TERMINATED
    }

    fn matches(&mut self, doc: DocId) -> bool {
        let mut source = PostingsLeafSource {
            postings: &mut self.postings,
            doc,
            positions: &mut self.positions,
        };
        match &self.plan.graph {
            Some(graph) => {
                let block = doc / BLOCK_DOCS as u32;
                let Some(block) = graph.blocks.get(&block) else {
                    return false;
                };
                let local = (doc % BLOCK_DOCS as u32) as usize;
                let Ok(view) = block.sentence(local, &mut self.sentence) else {
                    return false;
                };
                // Existence is all a hit needs.
                !graph
                    .plan
                    .evaluate(&view, &mut source, &mut self.eval, 1)
                    .is_empty()
            }
            None => {
                let n = self
                    .lengths
                    .as_ref()
                    .and_then(|column| column.first(doc))
                    .unwrap_or(0) as usize;
                let surface = self.compiled.surface.as_ref().expect("token pattern");
                !surface.spans(n, &mut source, &mut self.eval).is_empty()
            }
        }
    }
}

impl DocSet for RustieScorer {
    fn advance(&mut self) -> DocId {
        self.doc = self.next_match();
        self.doc
    }

    fn doc(&self) -> DocId {
        self.doc
    }

    fn size_hint(&self) -> u32 {
        (self.plan.candidates.len() - self.cursor.min(self.plan.candidates.len())) as u32
    }
}

impl Scorer for RustieScorer {
    fn score(&mut self) -> Score {
        1.0
    }
}
