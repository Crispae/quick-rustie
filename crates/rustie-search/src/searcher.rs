//! [`Searcher`]: RustIE pattern → Quickwit prefilter → in-memory exact evaluation.
//!
//! Quickwit is the storage and candidate engine (splits on S3, its caches, its metastore); the
//! exact RustIE semantics (sequences, gaps, captures, graph traversal) run here on the stored
//! tokens and dependency graph of each candidate. Quickwit has no plug-in point for custom
//! scorers, so matching cannot run inside its leaf search; the prefilter is what keeps the
//! number of candidates fetched small.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quickwit_config::SearcherConfig;
use quickwit_proto::metastore::MetastoreServiceClient;
use quickwit_proto::search::{CountHits, PartialHit, SearchRequest};
use quickwit_search::{
    ClusterClient, SearchJobPlacer, SearchServiceClient, SearchServiceImpl, SearcherContext,
    SearcherPool, root_search,
};
use quickwit_storage::StorageResolver;
use rustie_compiler::{CompiledQuery, QueryCompiler};
use rustie_indexer::{IndexSummary, IndexerOptions, MinioConfig, index_summary, open_metastore};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::cursor;
use crate::error::{Result, SearchError};
use crate::eval::{Evaluator, StoredSentence};
use crate::model::{QueryKind, SearchQuery, SearchResults, Timing};
use crate::prefilter;

/// Where the index lives and the resource limits applied to every query.
#[derive(Debug, Clone)]
pub struct SearcherOptions {
    pub index_id: String,
    pub metastore_uri: String,
    /// Candidate sentences fetched from Quickwit per round trip.
    pub page_size: usize,
    /// Hard cap on `limit` (default 1 000).
    pub max_limit: usize,
    /// Hard cap on candidates examined per query (default 200 000).
    pub max_candidates: usize,
    /// Wall-clock budget per query.
    pub timeout: Duration,
}

impl SearcherOptions {
    pub fn for_bucket(bucket: &str) -> Self {
        let indexer = IndexerOptions::for_bucket(bucket);
        Self {
            index_id: indexer.index_id,
            metastore_uri: indexer.metastore_uri,
            page_size: 200,
            max_limit: 1_000,
            max_candidates: 200_000,
            timeout: Duration::from_secs(30),
        }
    }
}

const DEFAULT_LIMIT: usize = 20;
const MAX_QUERY_CHARS: usize = 10_000;
/// Candidates fetched by the first round trip, as a multiple of the requested `limit`.
/// Later round trips double up to `page_size`: selective prefilters stay cheap, unselective
/// ones stop paying a round trip per handful of candidates.
const FIRST_PAGE_FACTOR: usize = 4;
const MIN_FIRST_PAGE: usize = 32;

/// Read-only handle on the index; cheap to share behind an `Arc`.
pub struct Searcher {
    options: SearcherOptions,
    minio: MinioConfig,
    compiler: QueryCompiler,
    /// Quickwit's searcher caches (split footers, fast fields, partial results). Built once
    /// and kept for the life of the process, including across metastore refreshes: this is
    /// the counterpart of RustIE's hotcache. (`single_node_search` builds a fresh one per
    /// call, so every query would start cold.)
    context: Arc<SearcherContext>,
    stack: RwLock<Stack>,
}

/// Everything derived from one metastore view.
struct Stack {
    metastore: MetastoreServiceClient,
    cluster_client: ClusterClient,
}

impl Stack {
    fn new(
        context: &Arc<SearcherContext>,
        metastore: MetastoreServiceClient,
        storage_resolver: StorageResolver,
    ) -> Self {
        // A one-node "cluster" whose only searcher is this process.
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 7280);
        let pool = SearcherPool::default();
        let cluster_client = ClusterClient::new(SearchJobPlacer::new(pool.clone()));
        let service = Arc::new(SearchServiceImpl::new(
            metastore.clone(),
            storage_resolver,
            cluster_client.clone(),
            Arc::clone(context),
        ));
        pool.insert(addr, SearchServiceClient::from_service(service, addr));
        Self {
            metastore,
            cluster_client,
        }
    }
}

impl Searcher {
    pub async fn connect(minio: MinioConfig, options: SearcherOptions) -> Result<Self> {
        if options.page_size == 0 || options.max_limit == 0 || options.max_candidates == 0 {
            return Err(SearchError::InvalidQuery(
                "page_size, max_limit and max_candidates must be > 0".into(),
            ));
        }
        let context = Arc::new(SearcherContext::new_without_invoker(
            SearcherConfig::default(),
            None,
        ));
        let (storage_resolver, metastore) = open_metastore(&minio, &options.metastore_uri).await?;
        Ok(Self {
            options,
            minio,
            compiler: QueryCompiler::new(),
            stack: RwLock::new(Stack::new(&context, metastore, storage_resolver)),
            context,
        })
    }

    pub fn options(&self) -> &SearcherOptions {
        &self.options
    }

    /// Re-open the metastore so splits published since the last (re)load become visible.
    /// The file-backed metastore does not poll on its own. Caches are kept.
    pub async fn refresh(&self) -> Result<()> {
        let (storage_resolver, metastore) =
            open_metastore(&self.minio, &self.options.metastore_uri).await?;
        *self.stack.write().await = Stack::new(&self.context, metastore, storage_resolver);
        Ok(())
    }

    pub async fn summary(&self) -> Result<IndexSummary> {
        let metastore = self.stack.read().await.metastore.clone();
        Ok(index_summary(&metastore, &self.options.index_id).await?)
    }

    /// Run `query`, bounded by the configured timeout.
    pub async fn search(&self, query: SearchQuery) -> Result<SearchResults> {
        let timeout = self.options.timeout;
        tokio::time::timeout(timeout, self.search_inner(query))
            .await
            .map_err(|_| SearchError::Timeout(timeout))?
    }

    async fn search_inner(&self, query: SearchQuery) -> Result<SearchResults> {
        let started = Instant::now();
        if query.query.trim().is_empty() || query.query.chars().count() > MAX_QUERY_CHARS {
            return Err(SearchError::InvalidQuery(format!(
                "query must be 1..={MAX_QUERY_CHARS} characters"
            )));
        }
        let limit = query
            .limit
            .unwrap_or(DEFAULT_LIMIT)
            .min(self.options.max_limit);
        if limit == 0 {
            return Err(SearchError::InvalidQuery("limit must be > 0".into()));
        }
        let max_candidates = query
            .max_candidates
            .unwrap_or(self.options.max_candidates)
            .min(self.options.max_candidates);
        let mut after: Option<PartialHit> = query
            .cursor
            .as_deref()
            .map(|token| cursor::decode(&query.query, token))
            .transpose()?;

        let compiled = self.compiler.compile(&query.query)?;
        let evaluator = Evaluator::new(&compiled)?;
        let kind = match compiled {
            CompiledQuery::Surface(_) => QueryKind::Surface,
            CompiledQuery::Graph(_) => QueryKind::Graph,
        };
        let prefilter = prefilter::plan(compiled.candidate());
        let query_ast = serde_json::to_string(&prefilter.ast)
            .map_err(|err| SearchError::Backend(err.to_string()))?;

        let (metastore, cluster_client) = {
            let stack = self.stack.read().await;
            (stack.metastore.clone(), stack.cluster_client.clone())
        };

        let mut hits = Vec::new();
        let mut scanned = 0usize;
        let mut candidates_total = None;
        let mut exhausted = false;
        let mut next_after: Option<PartialHit> = None;
        let mut page = (limit * FIRST_PAGE_FACTOR).clamp(MIN_FIRST_PAGE, self.options.page_size);
        let mut first_round_trip = true;
        let mut timing = Timing::default();

        while scanned < max_candidates {
            let want = page.min(max_candidates - scanned);
            let request = SearchRequest {
                index_id_patterns: vec![self.options.index_id.clone()],
                query_ast: query_ast.clone(),
                max_hits: want as u64,
                search_after: after.clone(),
                // Only exact-counting when asked: it forces a full pass of the prefilter.
                count_hits: if query.count && first_round_trip {
                    CountHits::CountAll
                } else {
                    CountHits::Underestimate
                } as i32,
                ..Default::default()
            };
            let backend_started = Instant::now();
            let response = root_search(&self.context, request, &metastore, &cluster_client)
                .await
                .map_err(|err| SearchError::Backend(err.to_string()))?;
            timing.backend_us += backend_started.elapsed().as_micros() as u64;
            if !response.errors.is_empty() || !response.failed_splits.is_empty() {
                return Err(SearchError::Backend(format!(
                    "{} split(s) failed: {}",
                    response.failed_splits.len(),
                    response.errors.join("; ")
                )));
            }
            if query.count && first_round_trip {
                candidates_total = Some(response.num_hits);
            }
            first_round_trip = false;

            let num_returned = response.hits.len();
            let match_started = Instant::now();
            let mut page_full = false;
            for hit in &response.hits {
                scanned += 1;
                next_after = hit.partial_hit.clone();
                let Some(sentence) = StoredSentence::from_hit_json(&hit.json) else {
                    warn!("skipping hit with unexpected stored shape");
                    continue;
                };
                if let Some(sentence_hit) = evaluator.evaluate(&sentence) {
                    hits.push(sentence_hit);
                    if hits.len() == limit {
                        page_full = true;
                        break;
                    }
                }
            }
            timing.match_us += match_started.elapsed().as_micros() as u64;
            debug!(scanned, matched = hits.len(), "round trip evaluated");
            if page_full {
                break;
            }
            if num_returned < want {
                exhausted = true;
                break;
            }
            after = next_after.clone();
            page = (page * 2).min(self.options.page_size);
        }

        let truncated = !exhausted && hits.len() < limit;
        let next_cursor = match (&next_after, exhausted) {
            (Some(partial), false) => Some(cursor::encode(&query.query, partial)),
            _ => None,
        };
        Ok(SearchResults {
            query: query.query,
            kind,
            candidate_query: serde_json::to_value(&prefilter.ast).unwrap_or_default(),
            hits,
            candidates_total,
            candidates_scanned: scanned,
            exhausted,
            truncated,
            next_cursor,
            prefilter_relaxed_clauses: prefilter.relaxed_clauses,
            took_ms: started.elapsed().as_millis() as u64,
            timing,
        })
    }
}
